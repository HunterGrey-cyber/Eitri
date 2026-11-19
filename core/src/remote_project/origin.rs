//! Which project a window opens: decided once at launch, before any toolkit starts, from the command line and the
//! remote project records. The decision is also the window's identity: a window of a remote project and a window of
//! its copy on this machine have the same root and are still two projects, so whatever finds a window by its project
//! (the open socket, the saved tabs) keys it by origin as well.

use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::{RemoteProject, RemoteProjects, TabOrigin};

/// How a window opens its root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowOrigin {
    /// A project on this machine.
    Local,
    /// `--local` on a remote project's copy: a local project whose agent tabs refuse to start, since they would act
    /// on the copy rather than on the server.
    LocalCopy { host: String },
    /// A project on a server: the record `eitri host open` wrote.
    Remote(RemoteProject),
}

impl WindowOrigin {
    /// The origin the window's tabs start with.
    pub fn tab_origin(&self) -> TabOrigin {
        match self {
            WindowOrigin::Local | WindowOrigin::LocalCopy { .. } => TabOrigin::Local,
            WindowOrigin::Remote(project) => TabOrigin::Remote {
                host: project.host.clone(),
                host_key_sha256: project.host_key_sha256.clone(),
                root: project.root.clone(),
            },
        }
    }

    /// The recorded host when the window opens a remote project's copy.
    pub fn local_copy_host(&self) -> Option<&str> {
        match self {
            WindowOrigin::LocalCopy { host } => Some(host),
            _ => None,
        }
    }

    pub fn remote(&self) -> Option<&RemoteProject> {
        match self {
            WindowOrigin::Remote(project) => Some(project),
            _ => None,
        }
    }

    /// The name of the subdirectory that keeps this origin's windows apart from local ones wherever a window is
    /// found by its root (`None` for a local project and its copy, which keep today's places).
    pub fn key(&self) -> Option<String> {
        self.remote().map(|project| origin_key(&project.host_key_sha256))
    }
}

/// The directory a window of `origin` holds its open socket in, and a launch looks for one: the shared control
/// directory for a local project and its copy, a subdirectory named by the server's key for a remote one, so a file
/// or a raise for one never reaches the other's window.
pub fn control_dir(origin: &WindowOrigin) -> PathBuf {
    let base = crate::panel_control::control_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), &std::env::temp_dir());
    match origin.key() {
        Some(key) => base.join(key),
        None => base,
    }
}

/// `ssh-<16 hex>` of a server's host key: the subdirectory name for a remote origin's windows. Keyed by the key ssh
/// verified, never by the host as typed, so two aliases of one server are one origin.
pub fn origin_key(host_key_sha256: &str) -> String {
    let digest = Sha256::digest(format!("eitri-window-origin\0{host_key_sha256}").as_bytes());
    format!("ssh-{}", &format!("{digest:x}")[..16])
}

/// [`origin_key`] of a tab origin, `None` for a local one.
pub fn tab_origin_key(origin: &TabOrigin) -> Option<String> {
    match origin {
        TabOrigin::Local => None,
        TabOrigin::Remote { host_key_sha256, .. } => Some(origin_key(host_key_sha256)),
    }
}

/// What the person runs to record a project on a host before a window can open it.
pub fn host_open_hint(host: &str, root: &Path) -> String {
    format!("run `eitri host open {host}:{}` in a terminal first", root.display())
}

/// The window's origin for `root`: `ssh` is `--ssh <host>`, `local` is `--local`. A root with a record opens as remote
/// unless `--local` asks for its copy; `--ssh` needs a record of that root on that host. Reads the record: a worker's
/// or the launch's, before any toolkit starts.
pub fn decide_origin(
    store: &RemoteProjects,
    root: &Path,
    ssh: Option<&str>,
    local: bool,
) -> Result<WindowOrigin, String> {
    let found = match store.find(root) {
        Ok(found) => found,
        // No state home, or a root no record can have: nothing was recorded for it.
        Err(e) if ssh.is_none() && matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::InvalidInput) => None,
        Err(e) => {
            return Err(format!(
                "the remote project record of {} cannot be read ({e}); open its copy here with eitri --local {}",
                root.display(),
                root.display()
            ))
        }
    };
    if local {
        return Ok(match found {
            Some(project) => WindowOrigin::LocalCopy { host: project.host },
            None => WindowOrigin::Local,
        });
    }
    match (ssh, found) {
        (None, None) => Ok(WindowOrigin::Local),
        (None, Some(project)) => Ok(WindowOrigin::Remote(project)),
        (Some(host), None) => Err(format!(
            "{} has never been opened on {host}; {}",
            root.display(),
            host_open_hint(host, root)
        )),
        (Some(host), Some(project)) if project.host != host => Err(format!(
            "{} is recorded as a project on {}, not on {host}; {}",
            root.display(),
            project.host,
            host_open_hint(host, root)
        )),
        (Some(_), Some(project)) => Ok(WindowOrigin::Remote(project)),
    }
}

/// The one line that refuses remote development while `remote_dev.enabled` is off.
pub fn remote_dev_off(what: &str) -> String {
    format!(
        "{what}: remote development is off; turn it on with eitri.config.set(\"{}\", true) in init.lua",
        crate::lua::config::REMOTE_DEV_ENABLED_KEY
    )
}

/// `Err` with [`remote_dev_off`] when `enabled` is false and `origin` opens a project on a server. A local project and
/// a remote project's copy open whatever the switch says.
pub fn check_remote_dev(enabled: bool, origin: &WindowOrigin) -> Result<(), String> {
    match origin {
        WindowOrigin::Remote(project) if !enabled => Err(format!(
            "{}; or open its copy here with eitri --local {}",
            remote_dev_off(&format!("{} is a project on {}", project.root.display(), project.host)),
            project.root.display()
        )),
        _ => Ok(()),
    }
}

/// The sentence a changed host key is shown with.
pub fn host_key_changed(host: &str, old: &str, new: &str) -> String {
    format!("{host}'s host key changed from {old} to {new}; ssh accepted it, so Eitri treats it as a new server")
}

/// Compares the key the connection verified with `project`'s. A different key is a new server to Eitri as far as
/// trust goes: the record takes the new key, and a remote project's trust answers are kept under its host key and
/// root, so its first tab asks again. What the record holds for agents still running there goes with it: the queued
/// stops stay queued (sent once the new key has an installation record, which a window needs before it connects, and
/// a stop for an agent that server does not know does nothing), and the windows' saved tabs move to the new key's
/// directory ([`carry_tabs_to_key`]). The sentence saying so comes back to be shown. Disk work: a worker's.
pub fn reconcile_host_key(
    store: &RemoteProjects,
    project: &RemoteProject,
    verified: &str,
) -> io::Result<(RemoteProject, Option<String>)> {
    let updated = store.update_for_host(&project.root, &project.host, |record| {
        record.host_key_sha256 = verified.to_owned()
    })?;
    if project.host_key_sha256 == verified {
        return Ok((updated, None));
    }
    let tabs = carry_tabs_to_key(store, &project.root, &project.host_key_sha256, verified);
    let notice = format!(
        "{}{}",
        host_key_changed(&project.host, &project.host_key_sha256, verified),
        carried_text(tabs, updated.queued_stops.len())
    );
    Ok((updated, Some(notice)))
}

/// The saved tabs of `root`'s windows under the host key `old` move to `new` (`saved_tabs::carry_to_new_host_key`):
/// how many remote tabs moved. One that cannot move stays where it was, and the log says why. Disk work.
pub fn carry_tabs_to_key(store: &RemoteProjects, root: &Path, old: &str, new: &str) -> usize {
    let Some(base) = store.saved_tabs_dir() else {
        return 0;
    };
    crate::saved_tabs::carry_to_new_host_key(&base, root, old, new).unwrap_or_else(|e| {
        eprintln!(
            "[remote] the saved tabs of {} stay under the old host key: {e}",
            root.display()
        );
        0
    })
}

/// What a changed host key's sentence adds about the agents the record follows: nothing when there are none.
pub fn carried_text(tabs: usize, stops: usize) -> String {
    match (tabs, stops) {
        (0, 0) => String::new(),
        _ => format!("; this project's {tabs} saved remote tab(s) and {stops} queued agent stop(s) go with it"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn store() -> RemoteProjects {
        let state = agent::state_dirs::test_workspace_dir("remote-origin");
        RemoteProjects::new(Some(state.as_os_str()), None)
    }

    fn record() -> RemoteProject {
        RemoteProject {
            version: 1,
            host: "example-host".into(),
            host_key_sha256: "SHA256:old".into(),
            root: "/srv/project".into(),
            helper_version: "0.3.0".into(),
            sync: None,
            created_at: 1,
            queued_stops: vec!["sc-1".into()],
        }
    }

    #[test]
    fn a_remote_origin_keeps_its_sockets_in_a_subdirectory_named_by_its_key() {
        let base =
            crate::panel_control::control_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), &std::env::temp_dir());
        assert_eq!(control_dir(&WindowOrigin::Local), base);
        let copy = WindowOrigin::LocalCopy {
            host: "example-host".into(),
        };
        assert_eq!(control_dir(&copy), base);
        let remote = WindowOrigin::Remote(record());
        assert_eq!(control_dir(&remote), base.join(origin_key("SHA256:old")));
        assert_ne!(control_dir(&remote), base);
    }

    /// `--ssh` needs a record of that root on that host and says what to run when there is none; a recorded root
    /// opens as remote by itself; `--local` opens its copy.
    #[test]
    fn open_without_a_record_says_to_run_host_open() {
        let store = store();
        let root = PathBuf::from("/srv/project");
        let refused = decide_origin(&store, &root, Some("example-host"), false).unwrap_err();
        assert!(
            refused.contains("run `eitri host open example-host:/srv/project` in a terminal first"),
            "{refused}"
        );
        assert_eq!(decide_origin(&store, &root, None, false), Ok(WindowOrigin::Local));
        assert_eq!(decide_origin(&store, &root, None, true), Ok(WindowOrigin::Local));
        store.save(&record()).unwrap();
        assert_eq!(
            decide_origin(&store, &root, Some("example-host"), false),
            Ok(WindowOrigin::Remote(record()))
        );
        assert_eq!(
            decide_origin(&store, &root, None, false),
            Ok(WindowOrigin::Remote(record()))
        );
        let copy = decide_origin(&store, &root, None, true).unwrap();
        assert_eq!(copy.local_copy_host(), Some("example-host"));
        assert_eq!(copy.tab_origin(), TabOrigin::Local);
        assert_eq!(copy.key(), None);
        let elsewhere = decide_origin(&store, &root, Some("other-host"), false).unwrap_err();
        assert!(
            elsewhere.contains("example-host") && elsewhere.contains("eitri host open other-host:"),
            "{elsewhere}"
        );
        assert_eq!(
            decide_origin(&store, Path::new("/srv/other"), None, false),
            Ok(WindowOrigin::Local)
        );
    }

    /// The switch gates only a project on a server, and the refusal names the key.
    #[test]
    fn remote_disabled_refuses_with_the_key_name() {
        let refused = check_remote_dev(false, &WindowOrigin::Remote(record())).unwrap_err();
        assert!(refused.contains("remote_dev.enabled"), "{refused}");
        assert!(refused.contains("eitri --local /srv/project"), "{refused}");
        assert!(!refused.contains("remote.enabled\""), "{refused}");
        assert!(check_remote_dev(true, &WindowOrigin::Remote(record())).is_ok());
        assert!(check_remote_dev(false, &WindowOrigin::Local).is_ok());
        let copy = WindowOrigin::LocalCopy {
            host: "example-host".into(),
        };
        assert!(check_remote_dev(false, &copy).is_ok());
        assert!(remote_dev_off("eitri host open").contains("remote_dev.enabled"));
    }

    /// A key other than the recorded one is shown in the sentence and becomes the record's; the same key changes
    /// nothing.
    #[test]
    fn a_window_cannot_reconcile_or_update_a_replacement_hosts_record() {
        let store = store();
        let launched = record();
        for host in ["other-example-host", "user@example-host", "ssh://example-host:2222"] {
            let mut replacement = launched.clone();
            replacement.host = host.into();
            for verified in ["SHA256:old", "SHA256:new"] {
                store.save(&replacement).unwrap();
                let why = reconcile_host_key(&store, &launched, verified).unwrap_err().to_string();
                assert!(why.contains("another server") && why.contains("reopen"));
                assert!(store
                    .update_for_host(&launched.root, &launched.host, |_| panic!(
                        "a replacement host's record must not be updated"
                    ))
                    .is_err());
                assert_eq!(store.find(&launched.root).unwrap(), Some(replacement.clone()));
            }
        }
    }

    #[test]
    fn changed_host_key_is_shown_and_recorded() {
        let store = store();
        store.save(&record()).unwrap();
        let (same, notice) = reconcile_host_key(&store, &record(), "SHA256:old").unwrap();
        assert_eq!((same, notice), (record(), None));
        let (updated, notice) = reconcile_host_key(&store, &record(), "SHA256:new").unwrap();
        assert_eq!(
            notice.as_deref(),
            Some(
                "example-host's host key changed from SHA256:old to SHA256:new; ssh accepted it, so Eitri treats \
                 it as a new server; this project's 0 saved remote tab(s) and 1 queued agent stop(s) go with it"
            )
        );
        assert_eq!(updated.host_key_sha256, "SHA256:new");
        assert_eq!(store.find(Path::new("/srv/project")).unwrap(), Some(updated));
    }

    /// The agents a changed key's record still answers for go with it: its queued stops stay queued and the windows'
    /// saved remote tabs move to the new key's directory, taking the new key, and the sentence counts both.
    #[test]
    fn a_changed_host_key_keeps_queued_stops_and_saved_tabs() {
        use crate::saved_tabs::{load_as, save_as, Loaded, SavedRemoteTab, SavedTabs};
        use crate::window_link::pair::Role;
        let state = agent::state_dirs::test_workspace_dir("remote-origin-rekey");
        let store = RemoteProjects::new(Some(state.as_os_str()), None);
        let mut old = record();
        old.root = "/srv/rekeyed".into();
        store.save(&old).unwrap();
        let base = store.saved_tabs_dir().unwrap();
        let tab = SavedRemoteTab {
            window: None,
            helper: agent::HelperHandoff {
                daemon: "".into(),
                declared_client_tools: Default::default(),
                host: "example-host".into(),
                host_key_sha256: "SHA256:old".into(),
                sidecar_id: "sc-7".into(),
                session_id: "s-7".into(),
                last_folded_sequence: 0,
            },
            provider_session_id: None,
            name: None,
            mode: crate::agent_bridge::SessionModeChoice::Auto,
        };
        save_as(
            &base.join(origin_key("SHA256:old")),
            &old.root,
            Role::Main,
            &SavedTabs::default().with_remote(vec![tab]),
        )
        .unwrap();
        let (updated, notice) = reconcile_host_key(&store, &old, "SHA256:new").unwrap();
        assert_eq!(
            updated.queued_stops,
            ["sc-1"],
            "the stop waits for the new key's connection"
        );
        assert!(notice
            .unwrap()
            .ends_with("; this project's 1 saved remote tab(s) and 1 queued agent stop(s) go with it"));
        let Loaded::Saved(moved) = load_as(&base.join(origin_key("SHA256:new")), &old.root, Role::Main) else {
            panic!("the saved tabs moved to the new key's directory");
        };
        assert_eq!(moved.remote[0].helper.sidecar_id, "sc-7");
        assert_eq!(moved.remote[0].helper.host_key_sha256, "SHA256:new");
        assert_eq!(
            load_as(&base.join(origin_key("SHA256:old")), &old.root, Role::Main),
            Loaded::Missing
        );
    }

    /// One server is one origin whatever it is called; another server or a local project is another.
    #[test]
    fn the_origin_key_follows_the_host_key() {
        let one = WindowOrigin::Remote(record());
        let mut aliased = record();
        aliased.host = "alias".into();
        assert_eq!(one.key(), WindowOrigin::Remote(aliased).key());
        let mut other = record();
        other.host_key_sha256 = "SHA256:new".into();
        assert_ne!(one.key(), WindowOrigin::Remote(other).key());
        let key = one.key().unwrap();
        assert!(key.starts_with("ssh-") && key.len() == 20, "{key}");
        assert_eq!(tab_origin_key(&one.tab_origin()), Some(key));
        assert_eq!(tab_origin_key(&TabOrigin::Local), None);
        assert_eq!(WindowOrigin::Local.key(), None);
    }
}
