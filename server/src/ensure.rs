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

pub async fn ensure() -> io::Result<()> {
    let paths = Paths::from_env(crate::VERSION)?;
    paths.prepare()?;
    crate::gc::touch_install(&paths)?;
    crate::gc::collect(&paths, std::time::SystemTime::now())?;
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

fn spawn_daemon() -> io::Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR");
    let mut command = if use_systemd_scope(runtime.as_deref().map(Path::new)) {
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

pub fn use_systemd_scope(runtime: Option<&Path>) -> bool {
    runtime.is_some_and(|dir| dir.is_absolute() && dir.join("systemd/private").exists())
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
                if let Some(version) = entry.file_name().to_str() {
                    if let Ok(paths) = Paths::new(version, &data_home, &state_home, &std::env::temp_dir()) {
                        candidates.push(paths);
                    }
                }
            }
        }
    }
    candidates.sort_by(|left, right| (&left.version, &left.run).cmp(&(&right.version, &right.run)));
    for paths in candidates {
        let Ok(info) = paths.info() else { continue };
        if !crate::run_dir::pid_alive(info.pid) {
            continue;
        }
        let response = hello(&paths).await;
        if let Some(value) = listed_info(&info, response) {
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
    let info = paths.info()?;
    // Pin the process identity before resolving /proc: a recycled pid must never get the signal.
    // SAFETY: pidfd_open takes a validated positive pid and zero flags.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, info.pid, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful pidfd_open returns a new descriptor owned here.
    let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as i32) };
    let exe = std::fs::read_link(format!("/proc/{}/exe", info.pid))?;
    let bin = paths.install(&info.rev).join("bin").canonicalize()?;
    if !exe.is_absolute() || !exe.starts_with(&bin) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "daemon executable is outside that version's installed bin directory; refusing to signal",
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
    fn systemd_scope_requires_the_user_manager_socket() {
        assert!(!super::use_systemd_scope(None));
        assert!(!super::use_systemd_scope(Some(std::path::Path::new("relative"))));
        let root = std::env::temp_dir().join(format!("scope-{}", std::process::id()));
        agent::private_fs::create_private_dir_all(&root.join("systemd"), &root).unwrap();
        assert!(!super::use_systemd_scope(Some(&root)));
        agent::private_fs::write_private(&root.join("systemd/private"), b"").unwrap();
        assert!(super::use_systemd_scope(Some(&root)));
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
