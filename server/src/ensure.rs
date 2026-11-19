use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use agent::private_fs;
use eitri_server_proto::{helper_client::HelperClient, HelloRequest, HelloResponse, PROTOCOL_MAJOR, PROTOCOL_MINOR};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tonic::transport::Endpoint;
use tower::service_fn;

use crate::run_dir::Paths;

pub const START_TIMEOUT: Duration = Duration::from_secs(15);

pub async fn hello(paths: &Paths) -> io::Result<HelloResponse> {
    let socket = paths.socket()?;
    let request = async {
        let channel = Endpoint::from_static("http://[::]:50051")
            .connect_with_connector(service_fn(move |_| {
                let socket = socket.clone();
                async move {
                    let stream = UnixStream::connect(socket).await?;
                    crate::peer::check(&stream)?;
                    Ok::<_, io::Error>(TokioIo::new(stream))
                }
            }))
            .await
            .map_err(io::Error::other)?;
        HelperClient::new(channel)
            .hello(HelloRequest {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                client_version: crate::VERSION.to_owned(),
            })
            .await
            .map(tonic::Response::into_inner)
            .map_err(io::Error::other)
    };
    tokio::time::timeout(Duration::from_millis(500), request)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "daemon Hello timed out"))?
}

/// Starts this installation's own daemon unless it answers already: a daemon of another installation of the same
/// version is another daemon, with a run directory of its own.
pub async fn ensure() -> io::Result<()> {
    let paths = Paths::own_from_env()?;
    paths.prepare()?;
    refresh_install(&paths);
    let deadline = Instant::now() + START_TIMEOUT;
    if hello(&paths).await.is_ok() {
        return Ok(());
    }
    let lock = private_fs::PrivateLock::open(&paths.metadata_run().join("lock"))?;
    while !lock.try_lock()? {
        if Instant::now() >= deadline {
            return hello(&paths)
                .await
                .map(|_| ())
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "daemon startup lock timed out"));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut child: Option<std::process::Child> = None;
    loop {
        if hello(&paths).await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return hello(&paths)
                .await
                .map(|_| ())
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "daemon did not answer within 15 seconds"));
        }
        if child.is_none() {
            match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), remove_stale_socket(&paths)).await {
                Err(_) => continue,
                Ok(result) => match result {
                    Ok(()) => child = Some(spawn_daemon()?),
                    // A previous daemon may still be starting or relinquishing its lifetime lock.
                    Err(error) if error.kind() == io::ErrorKind::AddrInUse => {}
                    Err(error) => return Err(error),
                },
            }
        }
        if let Some(child) = &mut child {
            let _ = child.try_wait()?;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn refresh_install(paths: &Paths) {
    for (kind, result) in [
        ("install-touch", crate::gc::touch_install(paths)),
        ("install-gc", crate::gc::collect(paths, std::time::SystemTime::now())),
    ] {
        if let Err(error) = result {
            eprintln!("eitri-server: {kind}: {error}");
            let _ = crate::audit::record(&paths.state, kind, &error.to_string());
        }
    }
}

fn spawn_daemon() -> io::Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR");
    let lingering = account_name().is_some_and(|name| lingers(Path::new(LINGER_DIR), &name));
    let mut command = if use_systemd_scope(runtime.as_deref().map(Path::new), lingering) {
        let mut command = Command::new("systemd-run");
        command.args(["--user", "--scope", "--quiet"]).arg(&exe);
        command
    } else {
        Command::new(exe)
    };
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe and this closure touches no shared Rust state.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()
}

async fn remove_stale_socket(paths: &Paths) -> io::Result<()> {
    let live = private_fs::PrivateLock::open(&paths.metadata_run().join("live.lock"))?;
    if !live.try_lock()? {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "a live daemon owns the socket; refusing to reclaim it",
        ));
    }
    let socket = paths.socket()?;
    match UnixStream::connect(&socket).await {
        Ok(stream) => {
            crate::peer::check(&stream)?;
            Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "daemon socket accepts connections but does not answer Hello",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
            let meta = socket.symlink_metadata()?;
            if !meta.file_type().is_socket() || meta.uid() != private_fs::euid() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "stale socket is not this user's socket",
                ));
            }
            std::fs::remove_file(socket)
        }
        Err(error) => Err(error),
    }
}

/// The account's passwd name, for the linger check. The lookup runs on a thread of its own and gets a second: a
/// directory service that does not answer (NSS can outlive any deadline) counts as not lingering, the placement that
/// is safe while KillUserProcesses is off, rather than holding the start lock.
fn account_name() -> Option<std::ffi::OsString> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("eitri-passwd".into())
        .spawn(move || {
            let name = crate::daemon::login_env::minimal_environment()
                .ok()
                .and_then(|account| account.get(std::ffi::OsStr::new("USER")).cloned());
            let _ = tx.send(name);
        })
        .ok()?;
    rx.recv_timeout(Duration::from_secs(1)).ok().flatten()
}

/// Where logind keeps one empty file per lingering account (`loginctl enable-linger` creates it).
const LINGER_DIR: &str = "/var/lib/systemd/linger";

fn lingers(dir: &Path, name: &std::ffi::OsStr) -> bool {
    let plain = !name.is_empty() && name != "." && name != ".." && !name.as_encoded_bytes().contains(&b'/');
    plain && dir.join(name).exists()
}

/// The daemon moves into a scope of the user manager only while the account lingers. Without lingering, logind stops
/// the user manager about ten seconds after no process is left in any of the account's sessions, and everything in
/// the manager's scopes (the daemon, its sidecars and their agents) is killed with it. Left in the ssh session's scope
/// instead, the detached daemon keeps that session "closing" rather than gone, so the account's processes live on as
/// long as logind's KillUserProcesses is off (its default). With lingering on, the user manager outlives every
/// session, and its scope also survives a KillUserProcesses=yes server.
pub fn use_systemd_scope(runtime: Option<&Path>, lingering: bool) -> bool {
    lingering && runtime.is_some_and(|dir| dir.is_absolute() && dir.join("systemd/private").exists())
}

pub async fn connect(paths: &Paths) -> io::Result<()> {
    let mut socket = UnixStream::connect(paths.socket()?).await?;
    crate::peer::check(&socket)?;
    let fd = socket.as_raw_fd();
    let (mut read, mut write) = socket.split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let result = tokio::select! {
        result = tokio::io::copy(&mut stdin, &mut write) => result,
        result = tokio::io::copy(&mut read, &mut stdout) => result,
    };
    // A half-close leaves the other pump waiting forever when a link disappears.
    // SAFETY: fd still names the live stream borrowed by both halves above.
    unsafe { libc::shutdown(fd, libc::SHUT_RDWR) };
    result.map(|_| ())
}

pub async fn list_daemons() -> io::Result<()> {
    let (data_home, state_home) = crate::run_dir::environment_roots()?;
    let state = state_home.join("eitri-server");
    let mut candidates = Vec::new();
    let root = state.join("run");
    if private_fs::is_private_dir(&root) {
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            if private_fs::is_private_dir(&entry.path()) {
                if let Some(key) = entry.file_name().to_str() {
                    if let Ok(paths) = Paths::new(key, &data_home, &state_home, &std::env::temp_dir()) {
                        candidates.push(paths);
                    }
                }
            }
        }
    }
    candidates.sort_by(|left, right| (&left.key, &left.run).cmp(&(&right.key, &right.run)));
    for paths in candidates {
        let Ok(published) = paths.published() else { continue };
        if !crate::run_dir::pid_alive(published.info.pid) {
            continue;
        }
        let response = hello(&paths).await;
        if let Some(mut value) = listed_info(&published.info, response) {
            // What `stop-daemon` and `connect --daemon` take to reach this daemon.
            value["run"] = paths.key.clone().into();
            println!("{}", value);
        }
    }
    Ok(())
}

fn listed_info(
    info: &eitri_server_proto::probe::DaemonInfo,
    response: io::Result<HelloResponse>,
) -> Option<serde_json::Value> {
    let incompatible = info.protocol_major != PROTOCOL_MAJOR
        && response
            .as_ref()
            .err()
            .and_then(io::Error::get_ref)
            .and_then(|e| e.downcast_ref::<tonic::Status>())
            .is_some_and(|s| s.code() == tonic::Code::FailedPrecondition);
    if response.is_ok() || incompatible {
        let mut value = serde_json::to_value(info).ok()?;
        value["incompatible_protocol"] = incompatible.into();
        Some(value)
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
pub fn stop_daemon(paths: &Paths) -> io::Result<()> {
    use std::os::fd::FromRawFd;
    let published = paths.published()?;
    let info = &published.info;
    // Pin the process identity before resolving /proc: a recycled pid must never get the signal.
    // SAFETY: pidfd_open takes a validated positive pid and zero flags.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, info.pid, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful pidfd_open returns a new descriptor owned here.
    let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as i32) };
    let exe = std::fs::read_link(format!("/proc/{}/exe", info.pid))?;
    // The installation the daemon recorded as its own, under the data directory, of its own version and revision:
    // the helper running this stop may be another installation's.
    let installed = crate::run_dir::installation_of(&exe, &paths.data).is_some_and(|(install, version, revision)| {
        published
            .installation
            .as_deref()
            .is_none_or(|name| install.file_name() == Some(std::ffi::OsStr::new(name)))
            && version == info.version
            && revision == crate::run_dir::rev7(&info.rev)
    });
    if !installed {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "daemon executable does not match its recorded installation identity; refusing to signal",
        ));
    }
    // SAFETY: the pidfd pins the checked process; null siginfo and zero flags request plain SIGTERM.
    if unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            libc::SIGTERM,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn stop_daemon(_: &Paths) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "daemon executable verification requires Linux /proc",
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn installation_maintenance_survives_audit_and_collection_errors() {
        let root = std::env::temp_dir().join(format!("maintenance-{}", std::process::id()));
        let install = root.join("data/0.3.0-abcdef0");
        agent::private_fs::create_private_dir_all(&install, &root).unwrap();
        let paths = crate::run_dir::Paths {
            version: "0.3.0".into(),
            key: "test".into(),
            data: root.join("data"),
            state: root.join("state"),
            run: root.join("run"),
            install: Some(install.clone()),
            installed: true,
        };
        paths.prepare().unwrap();
        agent::private_fs::create_private_dir_all(&paths.state.join("audit.log"), &paths.state).unwrap();
        crate::gc::touch_install(&paths).unwrap();
        assert!(install.join("last-used").is_file());
        std::fs::remove_file(paths.data.join("gc.lock")).unwrap();
        std::fs::create_dir(paths.data.join("gc.lock")).unwrap();
        super::refresh_install(&paths);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn systemd_scope_requires_the_user_manager_socket() {
        assert!(!super::use_systemd_scope(None, true));
        assert!(!super::use_systemd_scope(Some(std::path::Path::new("relative")), true));
        let root = std::env::temp_dir().join(format!("scope-{}", std::process::id()));
        agent::private_fs::create_private_dir_all(&root.join("systemd"), &root).unwrap();
        assert!(!super::use_systemd_scope(Some(&root), true));
        agent::private_fs::write_private(&root.join("systemd/private"), b"").unwrap();
        assert!(super::use_systemd_scope(Some(&root), true));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_user_that_does_not_linger_keeps_the_daemon_in_its_session() {
        let root = std::env::temp_dir().join(format!("linger-{}", std::process::id()));
        agent::private_fs::create_private_dir_all(&root.join("runtime/systemd"), &root).unwrap();
        agent::private_fs::write_private(&root.join("runtime/systemd/private"), b"").unwrap();
        let linger = root.join("linger");
        agent::private_fs::create_private_dir_all(&linger, &root).unwrap();
        let name = std::ffi::OsStr::new("someone");
        let runtime = root.join("runtime");
        assert!(!super::lingers(&linger, name));
        assert!(!super::use_systemd_scope(Some(&runtime), super::lingers(&linger, name)));
        agent::private_fs::write_private(&linger.join("someone"), b"").unwrap();
        assert!(super::lingers(&linger, name));
        assert!(super::use_systemd_scope(Some(&runtime), super::lingers(&linger, name)));
        for odd in ["", ".", "..", "a/b"] {
            assert!(!super::lingers(&linger, std::ffi::OsStr::new(odd)), "{odd:?}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn incompatible_live_daemons_keep_their_identity_and_a_flag() {
        let info = eitri_server_proto::probe::DaemonInfo {
            version: "older".into(),
            rev: "abcdef0".into(),
            pid: 42,
            protocol_major: super::PROTOCOL_MAJOR + 1,
            protocol_minor: 0,
            started_unix_ms: 1,
        };
        let value = super::listed_info(
            &info,
            Err(std::io::Error::other(tonic::Status::failed_precondition(
                "major mismatch",
            ))),
        )
        .unwrap();
        assert_eq!(value["pid"], 42);
        assert_eq!(value["incompatible_protocol"], true);
        assert!(super::listed_info(&info, Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))).is_none());
    }
}

/// Removal must preserve every surviving agent's adoption records, even without a live daemon.
pub(crate) fn check_sidecars_stopped(run: &Path) -> io::Result<()> {
    let runs = sidecar_runs(run, false)?;
    if runs.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other("recorded sidecars may still run"))
    }
}

/// Read-only inventory; no ensure, installation touch or garbage collection is performed here.
pub(crate) fn live_sidecar_runs(run: &Path) -> io::Result<()> {
    for value in sidecar_runs(run, true)? {
        println!("{value}");
    }
    Ok(())
}

fn sidecar_runs(run: &Path, recovery: bool) -> io::Result<Vec<serde_json::Value>> {
    use std::os::unix::fs::OpenOptionsExt;
    let entries = match std::fs::read_dir(run) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut refusing = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let key = entry.file_name().to_string_lossy().into_owned();
        let records = crate::daemon::sidecars::refusing_records_in(&entry.path())?;
        if records.is_empty() {
            continue;
        }
        for record in &records {
            eprintln!(
                "run {key} sidecar {} pid {} {}{}",
                record.id,
                record
                    .pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "unknown".into()),
                record.status,
                record
                    .error
                    .as_ref()
                    .map(|error| format!(": {}", error.replace(['\r', '\n'], " ")))
                    .unwrap_or_default()
            );
        }
        if recovery {
            if !records.iter().any(|record| record.status == "alive") {
                continue;
            }
            // A non-answering daemon can still hold its lifetime lock while starting or stopping.
            match std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(entry.path().join("live.lock"))
            {
                Ok(lock) => {
                    // SAFETY: flock operates on the live descriptor and never waits with LOCK_NB.
                    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() == io::ErrorKind::WouldBlock {
                            continue;
                        }
                        return Err(error);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        let published = private_fs::read_private_nofollow_capped(&entry.path().join("info"), 16 * 1024)
            .ok()
            .and_then(|text| serde_json::from_str::<crate::run_dir::Published>(&text).ok())
            .filter(|published| match &published.installation {
                Some(installation) => crate::run_dir::run_key_of(installation) == key,
                None => published.info.version == key,
            });
        refusing.push(serde_json::json!({
            "run": key,
            "daemon": published.as_ref().map(|published| &published.info),
            "installation": published.as_ref().and_then(|published| published.installation.as_ref()),
        }));
    }
    Ok(refusing)
}
