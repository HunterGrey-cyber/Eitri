//! A project's two sessions as the sync record names them, and what the actor asks the server's helper.
//!
//! Only the holder of a project's sync lock creates, pauses, resumes, re-creates or terminates its sessions; `eitri
//! host stop` pauses every session on that host and `eitri host uninstall` terminates them.

use std::path::Path;

use agent::remote_client as wire;

use super::engine::{project_labels, session_name, Role, SessionSpec, SessionStatus, SyncEngine, SyncState};
use super::{SyncError, SyncKey};
use crate::remote_project::{RemoteConnection, RemoteProject, RemoteProjects, SyncRecord};

/// A mode keeper's line from the server's Hello, for one root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModeLine {
    pub root: String,
    pub text: String,
    /// 0 on lines that count nothing.
    pub sequence: u64,
    pub epoch: u64,
}

/// What the actor asks the server's helper about the project. Every call crosses the network: a worker's.
pub trait SyncHelper: Send + Sync {
    fn read_file(&self, request: wire::ReadFileRequest) -> Result<Vec<wire::ReadFileChunk>, String>;
    fn replace_file_if(&self, request: wire::ReplaceFileIfRequest) -> Result<wire::ReplaceFileIfResponse, String>;
    fn remove_file_if(&self, request: wire::RemoveFileIfRequest) -> Result<wire::RemoveFileIfResponse, String>;
    fn sync_ignores(&self, root: &Path) -> Result<wire::SyncIgnoresResponse, String>;
    fn git_config(&self, root: &Path) -> Result<wire::GitConfigResponse, String>;
    fn list_tree(&self, root: &Path, patterns: &[String], budget: u64) -> Result<Vec<wire::ListTreeEntry>, String>;
    fn path_sizes(
        &self,
        root: &Path,
        rels: &[String],
        whole_root: bool,
        patterns: &[String],
        budget: u64,
    ) -> Result<wire::PathSizesResponse, String>;
    fn project_activity(&self, root: &Path) -> Result<wire::ProjectActivityResponse, String>;
    fn watch_modes(
        &self,
        root: &Path,
        patterns: &[String],
        on: bool,
        staging_name: &str,
    ) -> Result<wire::WatchModesResponse, String>;
    fn mode_lines(&self) -> Result<Vec<ModeLine>, String>;
    /// The host key the connection under this helper shows now, when ssh accepted one other than the key the project
    /// was recorded with (a window's link that came back to another server). `None` while it shows the recorded key,
    /// when none can be read, or with no ssh master. Blocking: it reads ssh's record of the master's key.
    fn changed_host_key(&self) -> Result<Option<String>, String>;
}

fn root_text(root: &Path) -> Result<String, String> {
    root.to_str().map(str::to_owned).ok_or_else(|| {
        format!(
            "{} is not UTF-8, which the server's helper cannot be sent",
            root.display()
        )
    })
}

impl SyncHelper for RemoteConnection {
    fn read_file(&self, request: wire::ReadFileRequest) -> Result<Vec<wire::ReadFileChunk>, String> {
        RemoteConnection::read_file(self, request)
    }
    fn replace_file_if(&self, request: wire::ReplaceFileIfRequest) -> Result<wire::ReplaceFileIfResponse, String> {
        RemoteConnection::replace_file_if(self, request)
    }
    fn remove_file_if(&self, request: wire::RemoveFileIfRequest) -> Result<wire::RemoveFileIfResponse, String> {
        RemoteConnection::remove_file_if(self, request)
    }

    fn sync_ignores(&self, root: &Path) -> Result<wire::SyncIgnoresResponse, String> {
        RemoteConnection::sync_ignores(self, wire::SyncIgnoresRequest { root: root_text(root)? })
    }

    fn git_config(&self, root: &Path) -> Result<wire::GitConfigResponse, String> {
        RemoteConnection::git_config(self, wire::GitConfigRequest { root: root_text(root)? })
    }

    fn list_tree(&self, root: &Path, patterns: &[String], budget: u64) -> Result<Vec<wire::ListTreeEntry>, String> {
        let request = wire::ListTreeRequest {
            root: root_text(root)?,
            patterns: patterns.to_vec(),
            budget,
        };
        RemoteConnection::list_tree(self, request)
    }

    fn path_sizes(
        &self,
        root: &Path,
        rels: &[String],
        whole_root: bool,
        patterns: &[String],
        budget: u64,
    ) -> Result<wire::PathSizesResponse, String> {
        RemoteConnection::path_sizes(
            self,
            wire::PathSizesRequest {
                root: root_text(root)?,
                rels: rels.to_vec(),
                whole_root,
                patterns: patterns.to_vec(),
                budget,
            },
        )
    }

    fn project_activity(&self, root: &Path) -> Result<wire::ProjectActivityResponse, String> {
        RemoteConnection::project_activity(self, wire::ProjectActivityRequest { root: root_text(root)? })
    }

    fn watch_modes(
        &self,
        root: &Path,
        patterns: &[String],
        on: bool,
        staging_name: &str,
    ) -> Result<wire::WatchModesResponse, String> {
        RemoteConnection::watch_modes(
            self,
            wire::WatchModesRequest {
                root: root_text(root)?,
                patterns: patterns.to_vec(),
                on,
                staging_name: staging_name.to_owned(),
            },
        )
    }

    fn mode_lines(&self) -> Result<Vec<ModeLine>, String> {
        RemoteConnection::mode_lines(self)
    }

    fn changed_host_key(&self) -> Result<Option<String>, String> {
        RemoteConnection::changed_host_key(self)
    }
}

/// The staging directory name the mode keeper is told about: Mutagen names a session's neighbouring staging directory
/// after the session's identifier (`.mutagen-temporary-staging-sync_<id>-beta`), which differs for each instance of
/// a session. The keeper keeps that directory ready between cycles; with no identifier known, none is named and the
/// keeper pairs any `.mutagen-temporary-staging-*` in the root's parent without making one.
pub fn staging_name(identifier: &str) -> String {
    if identifier.is_empty() {
        return String::new();
    }
    format!(".mutagen-temporary-staging-{identifier}-beta")
}

/// The tree session's spec: `.git`, the effective patterns in their order (repetitions kept: Mutagen's last match
/// decides), then `.git` again, so no exception the server wrote can bring a `.git` entry of any kind back.
pub fn tree_spec(key: &SyncKey, root: &Path, patterns: &[String]) -> SessionSpec {
    let mut spec = SessionSpec::tree(key.clone(), root.to_owned(), root.to_owned());
    spec.ignores.extend(patterns.iter().cloned());
    spec.ignores
        .extend(super::git_replica::TREE_VCS_IGNORES.iter().map(|p| (*p).to_owned()));
    spec
}

pub fn git_spec(key: &SyncKey, root: &Path) -> SessionSpec {
    SessionSpec::git(key.clone(), root.to_owned(), root.to_owned())
}

/// The status of the session named `name` among those labelled for this root, or `None` when Mutagen does not know
/// it. A listing that fails is an error, never "not known".
pub fn find(engine: &dyn SyncEngine, root: &Path, name: &str) -> Result<Option<SessionStatus>, SyncError> {
    let labels = root_labels(root);
    Ok(engine.status(&labels)?.into_iter().find(|status| status.name == name))
}

/// Mutagen's labels for every Eitri session of this laptop root, whatever its key.
pub fn root_labels(root: &Path) -> super::engine::Labels {
    let mut labels = project_labels(&SyncKey::new("", root), Role::Tree, root);
    labels.retain(|key, _| key == "eitri-root");
    labels
}

/// Terminates every Eitri session of this root whose key is not `key` (the root was opened against another host or
/// under another host key). Returns the names terminated.
pub fn terminate_other_keys(engine: &dyn SyncEngine, key: &SyncKey, root: &Path) -> Result<Vec<String>, SyncError> {
    let ours = [session_name(key, Role::Tree), session_name(key, Role::Git)];
    let mut terminated = Vec::new();
    for status in engine.status(&root_labels(root))? {
        if !ours.contains(&status.name) {
            engine.terminate(&status.name)?;
            terminated.push(status.name);
        }
    }
    Ok(terminated)
}

/// Whether a session counts as running, for resuming it.
pub fn is_paused(status: &SessionStatus) -> bool {
    status.state == SyncState::Paused
}

/// Every recorded session of the projects under `host_key`.
fn key_sessions(store: &RemoteProjects, host_key: &str) -> (Vec<(RemoteProject, SyncRecord)>, Vec<String>) {
    let (projects, mut problems) = match store.list() {
        Ok(listed) => listed,
        Err(error) => return (vec![], vec![error.to_string()]),
    };
    let mut found = Vec::new();
    for project in projects
        .into_iter()
        .filter(|project| project.host_key_sha256 == host_key)
    {
        match project.sync_record() {
            Ok(Some(record)) => found.push((project, record)),
            Ok(None) => {}
            Err(why) => problems.push(why),
        }
    }
    (found, problems)
}

/// After the window recorded a changed host key: a sync record whose sessions run under the old key gets the host-key
/// pause (unless the person already accepted this key), so nothing syncs until they resume. Returns the line to show.
pub fn note_key_change(store: &RemoteProjects, project: &RemoteProject) -> Option<String> {
    let record = project.sync_record().ok().flatten()?;
    if record.host_key_sha256 == project.host_key_sha256
        || record.accepted_host_key.as_deref() == Some(project.host_key_sha256.as_str())
        || record.paused.is_some()
    {
        return None;
    }
    let text = super::actor::host_key_text(
        &project.host,
        &record.host_key_sha256,
        &project.host_key_sha256,
        &project.root,
    );
    let saved = store.update_for_host(&project.root, &project.host, |project| {
        if let Ok(Some(mut record)) = project.sync_record() {
            record.paused = Some(crate::remote_project::PauseRecord {
                reason: super::actor::PAUSE_HOST_KEY.into(),
                detail: text.clone(),
                since_unix_ms: super::status_file::now_unix_ms(),
                path: None,
            });
            project.set_sync_record(&record);
        }
    });
    Some(match saved {
        Ok(_) => text,
        Err(error) => format!("{text} (the pause was not recorded: {error})"),
    })
}

/// `eitri host stop <host>`: pauses every session of `host`'s projects. Returns the lines to show.
pub fn pause_host(
    engine: &dyn SyncEngine,
    store: &RemoteProjects,
    host_key: &str,
    command: &str,
    host: &str,
) -> Vec<String> {
    let (found, mut lines) = key_sessions(store, host_key);
    for (project, record) in found {
        // Recorded, so a window holding the project keeps it paused and `eitri host sync resume` lifts it.
        if record.paused.is_none() {
            let text = format!(
                "sync paused by eitri host {command} {host}; run eitri host sync resume in {} to sync again",
                project.root.display()
            );
            let saved = store.update_checked(&project.root, |project| {
                if project.host_key_sha256 != host_key {
                    return Err(std::io::Error::other("the project now points at another server"));
                }
                if let Ok(Some(mut record)) = project.sync_record() {
                    record.paused = Some(crate::remote_project::PauseRecord {
                        reason: super::actor::PAUSE_STOPPED.into(),
                        detail: text.clone(),
                        since_unix_ms: super::status_file::now_unix_ms(),
                        path: None,
                    });
                    project.set_sync_record(&record);
                }
                Ok(())
            });
            if let Err(error) = saved {
                lines.push(format!(
                    "the pause of {} was not recorded: {error}",
                    project.root.display()
                ));
            }
        }
        for name in [&record.tree_session, &record.git_session].into_iter().flatten() {
            if let Err(error) = engine.pause(name) {
                if !error.to_string().contains("no such session") {
                    lines.push(format!("sync for {} was not paused: {error}", project.root.display()));
                }
            }
        }
    }
    lines
}

/// `eitri host uninstall`: terminates every session of the projects recorded under the server's verified host key
/// (whatever alias they were opened with) and forgets them in the records, leaving the mirrors on disk. Returns the
/// lines to show.
pub fn terminate_host(engine: &dyn SyncEngine, store: &RemoteProjects, host_key: &str) -> Vec<String> {
    let (found, mut lines) = key_sessions(store, host_key);
    for (project, record) in found {
        let mut failed = false;
        for name in [&record.tree_session, &record.git_session].into_iter().flatten() {
            if let Err(error) = engine.terminate(name) {
                failed = true;
                lines.push(format!("sync for {} was not stopped: {error}", project.root.display()));
            }
        }
        if !failed {
            let forgotten = store.update_for_host(&project.root, &project.host, |project| {
                if let Ok(Some(mut record)) = project.sync_record() {
                    record.tree_session = None;
                    record.git_session = None;
                    project.set_sync_record(&record);
                }
            });
            if let Err(error) = forgotten {
                lines.push(format!(
                    "the sync record of {} still names its sessions: {error}",
                    project.root.display()
                ));
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_changed_key_records_the_pause_once() {
        let state = agent::state_dirs::test_workspace_dir("sync-key-change");
        let store = RemoteProjects::new(Some(state.as_os_str()), None);
        let mut project = RemoteProject {
            version: 1,
            host: "example-host".into(),
            host_key_sha256: "SHA256:new".into(),
            root: "/srv/project".into(),
            helper_version: "v".into(),
            sync: None,
            created_at: 0,
            queued_stops: vec![],
        };
        store.save(&project).unwrap();
        assert_eq!(
            note_key_change(&store, &project),
            None,
            "no sync record, nothing to pause"
        );
        project.set_sync_record(&SyncRecord {
            engine: "mutagen".into(),
            host_key_sha256: "SHA256:old".into(),
            ..Default::default()
        });
        store.save(&project).unwrap();
        let line = note_key_change(&store, &project).unwrap();
        assert!(
            line.contains("changed its host key from SHA256:old to SHA256:new"),
            "{line}"
        );
        let saved = store.find(Path::new("/srv/project")).unwrap().unwrap();
        let pause = saved.sync_record().unwrap().unwrap().paused.unwrap();
        assert_eq!(pause.reason, super::super::actor::PAUSE_HOST_KEY);
        assert_eq!(note_key_change(&store, &saved), None, "already paused");
    }

    #[test]
    fn tree_spec_keeps_the_patterns_in_order_and_dot_git_last() {
        let key = SyncKey::new("SHA256:k", Path::new("/srv/project"));
        let rules: Vec<String> = ["/private", "!/private", "/private", "!.git"]
            .map(String::from)
            .to_vec();
        let spec = tree_spec(&key, Path::new("/srv/project"), &rules);
        assert_eq!(
            spec.ignores,
            [".git", "/private", "!/private", "/private", "!.git", ".git"]
                .map(String::from)
                .to_vec()
        );
        let matcher = crate::remote_sync::ignores::Matcher::new(&spec.ignores);
        assert!(matcher.ignored("a/.git", false));
        assert!(matcher.ignored("private", false));
        assert_eq!(spec.name, format!("eitri-{key}-tree"));
        assert_eq!(staging_name(""), "");
        assert_eq!(staging_name("sync_abc"), ".mutagen-temporary-staging-sync_abc-beta");
        let labels = root_labels(Path::new("/srv/project"));
        assert_eq!(labels.len(), 1);
        assert_eq!(labels["eitri-root"], spec.labels["eitri-root"]);
    }
}
