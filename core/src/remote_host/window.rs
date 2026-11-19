//! A remote project's window reaching its server: the connection, the host key it shows, the helper this Eitri
//! needs (installed when an upgrade left it missing on a host the person already consented to) and the connection to
//! that helper. Every step crosses the network or the disk and runs on a worker; what the window shows comes back as
//! band lines.

use std::cell::Cell;
use std::path::Path;
use std::sync::Arc;

use agent::ssh::{ChannelLauncher, ControlMaster, SshLauncher};

use super::install::{self, InstallError, InstallPlan};
use crate::remote_project::{
    host_open_hint, reconcile_host_key, verified_host_key, RemoteConnection, RemoteHost, RemoteProject, RemoteProjects,
};

/// The band while the connection starts.
pub fn connecting_text(host: &str) -> String {
    format!("connecting to {host}…")
}

fn incompatible_helper_text(version: &str, host: &str) -> String {
    format!("Eitri server {version} speaks another protocol; eitri host stop {host} stops all servers and their agents there; then open this window again")
}

/// The band once the window's remote tabs can start.
pub fn ready_text(host: &str) -> String {
    format!("{host} ready")
}

/// The band while a helper this window needs is uploaded.
pub fn installing_text(version: &str, host: &str, percent: u64) -> String {
    format!("installing Eitri server {version} on {host}… {percent} %")
}

/// What the window's connection came to.
pub struct Opened {
    pub connection: RemoteConnection,
    /// The record as it is now: a changed host key is the record's from here on.
    pub project: RemoteProject,
    /// The sentence saying the host key changed, when it did.
    pub key_changed: Option<String>,
    /// The ssh connection under it, for the checks after a resume or a network change.
    pub master: ControlMaster,
}

/// Whether `host` records `plan`'s helper and sidecar as installed, with the hashes this build has, in the directories
/// the probe found on the server now (a changed `XDG_DATA_HOME` there is another place). Whether the files are still
/// there is the start's to find out ([`start_server`]).
pub fn installed(host: &RemoteHost, plan: &InstallPlan) -> bool {
    host.data_dir == plan.data_dir()
        && host.state_dir == plan.state_dir()
        && host
            .helpers
            .get(plan.version())
            .is_some_and(|hash| plan.manifest().lines.first().is_some_and(|(helper, _)| helper == hash))
        && host
            .sidecars
            .get(&install::sidecar_record_key(plan.version(), plan.revision()))
            .is_some_and(|hash| hash == plan.sidecar_sha256())
}

/// Brings `plan`'s helper up on a host the person consented to, under `launcher`: installed first when this machine
/// has not recorded it there (after an Eitri upgrade), with no question, the band saying how far the upload is and
/// then that it is installed; then started. A recorded installation whose files the start does not find as recorded
/// (removed from another machine, a server reinstalled with its host keys kept) is installed again the same way and
/// started once more. A host never consented to is refused with what to run in a terminal. Then `connect` opens what
/// the window uses, and the band says the host is ready. Blocking.
pub fn start_server<T>(
    launcher: &dyn ChannelLauncher,
    store: &RemoteProjects,
    project: &RemoteProject,
    record: &RemoteHost,
    plan: &mut InstallPlan,
    band: &dyn Fn(String),
    connect: impl FnOnce(&InstallPlan) -> Result<T, String>,
) -> Result<T, String> {
    let host = &project.host;
    if !record.consented {
        return Err(format!(
            "Eitri server on {host} was not installed with your consent; {}",
            host_open_hint(host, &project.root)
        ));
    }
    plan.use_host_record(record).map_err(|e| e.to_string())?;
    let install_now = |plan: &InstallPlan| -> Result<(), String> {
        let version = plan.version().to_owned();
        let shown = Cell::new(None);
        let progress = |done: u64, total: u64| {
            let percent = (done.saturating_mul(100)).checked_div(total).unwrap_or(100).min(100);
            if shown.get() != Some(percent) {
                shown.set(Some(percent));
                band(installing_text(&version, host, percent));
            }
        };
        install::install_with(launcher, plan, &progress).map_err(|e| format!("installing on {host}: {e}"))?;
        plan.record_installed(store).map_err(|e| e.to_string())?;
        band(plan.installed_text(host));
        Ok(())
    };
    let recorded = installed(record, plan);
    if !recorded {
        install_now(plan)?;
    }
    match install::ensure_started_with(launcher, plan) {
        Ok(()) => {}
        // This build's own files, at its own hashes: the same install an upgrade makes, with no question.
        Err(InstallError::Reinstall(why)) if recorded => {
            eprintln!("[remote] {host}: the recorded installation did not start ({why}); installing it again");
            install_now(plan)?;
            install::ensure_started_with(launcher, plan)
                .map_err(|e| format!("starting Eitri server on {host}: {e}"))?;
        }
        Err(e) => return Err(format!("starting Eitri server on {host}: {e}")),
    }
    let opened = connect(plan)?;
    band(ready_text(host));
    Ok(opened)
}

/// Opens the window's connection to `project`'s server over ssh: the master (through the person's own ssh config,
/// `ssh_dir` holding its socket and the record of the key it verified), the key it verified compared with the
/// record's, the helper this build needs on the host, and the connection to it. `exe` is this Eitri, which ssh runs
/// to record the key. Blocking: a worker's.
/// `sync` runs once the connection is open, on this worker: the window starts its sync actor there.
pub fn open_window(
    project: &RemoteProject,
    ssh_dir: &Path,
    exe: &Path,
    store: &RemoteProjects,
    band: &dyn Fn(String),
    sync: &dyn Fn(&RemoteConnection, &RemoteProject),
) -> Result<Opened, String> {
    let host = project.host.clone();
    band(connecting_text(&host));
    let master = ControlMaster::ensure(&host, ssh_dir, exe).map_err(|e| format!("connecting to {host}: {e}"))?;
    let verified = verified_host_key(&master)?;
    let (project, key_changed) = reconcile_host_key(store, project, &verified).map_err(|e| e.to_string())?;
    if let Some(notice) = &key_changed {
        band(notice.clone());
        // Sync waits for the person: its sessions run under the old key until they resume it.
        if let Some(line) = crate::remote_sync::sessions::note_key_change(store, &project) {
            band(line);
        }
    }
    let record = store.host(&verified).map_err(|e| e.to_string())?.ok_or_else(|| {
        format!(
            "Eitri server was never installed on {host} under host key {verified}; {}",
            host_open_hint(&host, &project.root)
        )
    })?;
    let probe = super::probe_step::probe(&master).map_err(|e| format!("probing {host}: {e}"))?;
    let sidecar = agent::sidecar_for_upload()?;
    let mut plan = install::plan_connection(&probe, &sidecar.path).map_err(|e| e.to_string())?;
    for warning in plan.warnings() {
        band(warning.clone());
    }
    let launcher: Arc<dyn ChannelLauncher> = Arc::new(SshLauncher {
        master: master.clone(),
        helper: plan.install_path().join("bin/eitri-server"),
    });
    let connection = start_server(launcher.as_ref(), store, &project, &record, &mut plan, band, |plan| {
        if project.helper_version != plan.version() {
            let version = plan.version().to_owned();
            // The record only says which installation the window last started; failing to say so changes nothing.
            if let Err(e) =
                store.update_for_host(&project.root, &project.host, |record| record.helper_version = version)
            {
                eprintln!("[remote] {host}: the project record keeps its older helper version ({e})");
            }
        }
        let running = super::cli::daemons(launcher.as_ref(), &project.host_key_sha256)?;
        let daemons: Vec<_> = running.iter().map(|d| d.daemon.clone()).collect();
        let prefix = format!("{}-", env!("CARGO_PKG_VERSION"));
        let revision = plan
            .version()
            .strip_prefix(&prefix)
            .and_then(|s| s.get(..7))
            .ok_or_else(|| "helper installation does not match this Eitri version".to_owned())?;
        for (_, choice) in super::skew::decide_attach(
            agent::ssh::PROTOCOL_MAJOR,
            env!("CARGO_PKG_VERSION"),
            revision,
            &daemons,
        ) {
            match choice {
                super::skew::Attach::Newer { .. } => {
                    return Err("The server runs a newer Eitri helper; this window attaches nothing.".into())
                }
                super::skew::Attach::OlderIncompatible { daemon } => {
                    return Err(incompatible_helper_text(&daemon.version, &project.host))
                }
                _ => {}
            }
        }
        RemoteConnection::open_over_ssh(&master, store, &project, plan.version(), plan.revision())
    })?;
    sync(&connection, &project);
    Ok(Opened {
        connection,
        project,
        key_changed,
        master,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use crate::test_scratch_dir::ScratchDir;
    #[cfg(target_os = "linux")]
    use std::cell::RefCell;

    #[test]
    fn incompatible_helper_hint_names_the_existing_command_and_its_scope() {
        let hint = incompatible_helper_text("0.2.0", "example-host");
        assert!(hint.contains("eitri host stop example-host"));
        assert!(hint.contains("all servers and their agents"));
    }

    #[cfg(target_os = "linux")]
    fn project() -> RemoteProject {
        RemoteProject {
            version: 1,
            host: "example-host".into(),
            host_key_sha256: "SHA256:example".into(),
            root: "/srv/project".into(),
            helper_version: "0.2.0-old".into(),
            sync: None,
            created_at: 1,
            queued_stops: Vec::new(),
        }
    }

    /// A host the person consented to that has only an older helper gets this build's with no question: the band
    /// counts the upload up in order, says it is installed, and only then that the host is ready; the record keeps
    /// the older installation beside the new one.
    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn window_installs_a_newer_helper_with_progress() {
        let root = ScratchDir::new("remote", "window-upgrade");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let (mut plan, launcher) = install::test_plan(&root, "new");
        let mut record = install::test_host_record(&plan);
        record.helpers = [("0.2.0-old".to_string(), "a".repeat(64))].into();
        record.sidecars.clear();
        store.save_host(&record).unwrap();
        let lines = RefCell::new(Vec::new());
        let band = |line: String| lines.borrow_mut().push(line);
        let version = plan.version().to_owned();
        let connected = start_server(&launcher, &store, &project(), &record, &mut plan, &band, |plan| {
            assert!(
                plan.install_path().join("started").exists(),
                "started before the connection"
            );
            Ok(plan.version().to_owned())
        })
        .unwrap();
        assert_eq!(connected, version);
        let lines = lines.into_inner();
        assert_eq!(
            lines.first().map(String::as_str),
            Some(installing_text(&version, "example-host", 0).as_str())
        );
        let percents: Vec<u64> = lines
            .iter()
            .filter_map(|line| line.rsplit_once("… ")?.1.strip_suffix(" %")?.parse().ok())
            .collect();
        assert!(percents.windows(2).all(|pair| pair[0] < pair[1]), "{lines:?}");
        assert_eq!(percents.last(), Some(&100));
        assert_eq!(
            &lines[lines.len() - 2..],
            [
                format!("installed Eitri server {version} on example-host"),
                ready_text("example-host")
            ]
        );
        assert!(
            lines.iter().all(|line| !line.contains("[y/N]")),
            "no question: {lines:?}"
        );
        let saved = store.host("SHA256:example").unwrap().unwrap();
        assert!(saved.helpers.contains_key("0.2.0-old") && saved.helpers.contains_key(&version));
        assert!(installed(&saved, &plan));
    }

    /// The record says this build is installed, but the server has none of it (removed from another machine, or a
    /// server reinstalled with its host keys kept): the start finds out, and this build's files go up again with no
    /// question, the band saying so, before the host is ready.
    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_recorded_installation_missing_on_the_server_is_installed_again() {
        let root = ScratchDir::new("remote", "window-missing");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let (mut plan, launcher) = install::test_plan(&root, "gone");
        let record = install::test_host_record(&plan);
        store.save_host(&record).unwrap();
        assert!(installed(&record, &plan) && !plan.install_path().exists());
        let lines = RefCell::new(Vec::new());
        let band = |line: String| lines.borrow_mut().push(line);
        start_server(&launcher, &store, &project(), &record, &mut plan, &band, |_| Ok(())).unwrap();
        assert!(plan.install_path().join("started").exists());
        let lines = lines.into_inner();
        assert!(lines.contains(&plan.installed_text("example-host")), "{lines:?}");
        assert_eq!(lines.last(), Some(&ready_text("example-host")));
    }

    /// The server's data directory moved (another `XDG_DATA_HOME` there): the record's installation is somewhere else,
    /// so this build is installed where the probe found the directory, and the record follows.
    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_moved_data_directory_is_installed_into() {
        let root = ScratchDir::new("remote", "window-moved");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let (mut plan, launcher) = install::test_plan(&root, "moved");
        let mut record = install::test_host_record(&plan);
        record.data_dir = root.join("old-data/eitri-server");
        store.save_host(&record).unwrap();
        assert!(!installed(&record, &plan));
        let band = |_: String| {};
        start_server(&launcher, &store, &project(), &record, &mut plan, &band, |_| Ok(())).unwrap();
        assert!(plan.install_path().join("started").exists());
        let saved = store.host("SHA256:example").unwrap().unwrap();
        assert_eq!(saved.data_dir, plan.data_dir());
        assert!(installed(&saved, &plan));
    }

    /// An installed helper is only started; a host never consented to installs and starts nothing.
    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_installed_helper_is_started_and_an_unconsented_host_refused() {
        let root = ScratchDir::new("remote", "window-start");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let (mut plan, launcher) = install::test_plan(&root, "same");
        let mut record = install::test_host_record(&plan);
        record.consented = false;
        let lines = RefCell::new(Vec::new());
        let band = |line: String| lines.borrow_mut().push(line);
        let refused = start_server(&launcher, &store, &project(), &record, &mut plan, &band, |_| Ok(())).unwrap_err();
        assert!(
            refused.contains("eitri host open example-host:/srv/project"),
            "{refused}"
        );
        assert!(!plan.install_path().exists());
        record.consented = true;
        install::install_with(
            &launcher,
            &{
                let mut authorized = plan.clone();
                authorized.use_host_record(&record).unwrap();
                authorized
            },
            &|_, _| {},
        )
        .unwrap();
        start_server(&launcher, &store, &project(), &record, &mut plan, &band, |_| Ok(())).unwrap();
        assert_eq!(lines.into_inner(), [ready_text("example-host")]);
        assert!(plan.install_path().join("started").exists());
    }
}
