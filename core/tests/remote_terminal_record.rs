//! Terminal ownership with temporary state and a fake control link: no SSH, sockets or display.
use std::collections::BTreeMap;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use agent::remote_client::terminal_transport::{tonic, wire};
use eitri_core::remote_project::terminal_link::TerminalTarget;
use eitri_core::remote_project::terminals::{terminal_lines, TerminalControl, TerminalEntry, TerminalState};
use eitri_core::remote_project::RemoteProjects;

const KEY: &str = "SHA256:example";
const ROOT: &str = "/srv/project";

struct Fixture {
    state: PathBuf,
    projects: RemoteProjects,
}
impl Fixture {
    fn new() -> Self {
        let state = agent::state_dirs::test_workspace_dir("terminal-record");
        let projects = RemoteProjects::new(Some(state.as_os_str()), None);
        projects.save(&project(KEY, ROOT)).unwrap();
        Self { state, projects }
    }
    fn opening(&self, slot: u8, module: &str) -> TerminalEntry {
        self.projects
            .terminals()
            .begin_open(KEY, Path::new(ROOT), slot, module, "0.3.0-example", 42)
            .unwrap()
    }
    fn open(&self, slot: u8, module: &str) -> TerminalEntry {
        let entry = self.opening(slot, module);
        self.projects
            .terminals()
            .opened(
                KEY,
                Path::new(ROOT),
                &entry.command_id,
                &format!("term-{module}"),
                "daemon",
                "boot",
            )
            .unwrap();
        self.projects
            .terminals()
            .record(KEY, Path::new(ROOT))
            .unwrap()
            .entries
            .into_iter()
            .find(|e| e.command_id == entry.command_id)
            .unwrap()
    }
    fn entries(&self) -> Vec<TerminalEntry> {
        self.projects.terminals().record(KEY, Path::new(ROOT)).unwrap().entries
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.state);
    }
}

#[derive(Default)]
struct Link {
    opened: BTreeMap<String, String>,
    stopped: Vec<String>,
    fail: Option<String>,
}
impl Link {
    fn open(&mut self, target: TerminalTarget) -> String {
        let TerminalTarget::Open { command_id, .. } = target else {
            panic!("an open")
        };
        let id = format!("terminal-{}", self.opened.len());
        self.opened.entry(command_id).or_insert(id).clone()
    }
}
impl TerminalControl for Link {
    fn stop(&mut self, _: &Path, entry: &TerminalEntry) -> io::Result<Option<String>> {
        if self.fail.as_deref() == Some(&entry.command_id) {
            return Err(io::ErrorKind::NotConnected.into());
        }
        if let Some(id) = entry
            .terminal_id
            .as_ref()
            .or_else(|| self.opened.get(&entry.command_id))
        {
            self.stopped.push(id.clone());
        }
        Ok(None)
    }
}

#[test]
fn the_intent_is_written_before_the_open_is_sent() {
    let f = Fixture::new();
    let intent = f.opening(1, "terminal-1");
    let disk = RemoteProjects::new(Some(f.state.as_os_str()), None);
    assert_eq!(
        disk.terminals().record(KEY, Path::new(ROOT)).unwrap().entries,
        [intent.clone()]
    );
    assert_eq!(intent.state, TerminalState::Opening);
    let mut link = Link::default();
    let id = link.open(intent.target(Path::new(ROOT)).unwrap());
    f.projects
        .terminals()
        .opened(KEY, Path::new(ROOT), &intent.command_id, &id, "daemon", "boot")
        .unwrap();
    assert_eq!(f.entries()[0].state, TerminalState::Open);
    assert_eq!(f.entries()[0].terminal_id.as_deref(), Some(id.as_str()));
}

#[test]
fn a_crash_between_open_and_answer_finds_the_same_shell() {
    let f = Fixture::new();
    let intent = f.opening(1, "terminal-1");
    let mut link = Link::default();
    let id = link.open(intent.target(Path::new(ROOT)).unwrap());
    let reopened = RemoteProjects::new(Some(f.state.as_os_str()), None);
    let retry = reopened
        .terminals()
        .begin_open(KEY, Path::new(ROOT), 1, "terminal-1", "0.3.0-new", 100)
        .unwrap();
    assert_eq!(retry, intent);
    assert_eq!(link.open(retry.target(Path::new(ROOT)).unwrap()), id);
    assert_eq!(link.opened.len(), 1);
}

#[test]
fn a_reopened_window_reattaches_its_own_slot_only() {
    let f = Fixture::new();
    let first = f.open(1, "terminal-1");
    f.open(2, "terminal-1");
    let restored = f
        .projects
        .terminals()
        .restored(KEY, Path::new(ROOT), 1, &["terminal-1".into()])
        .unwrap();
    assert_eq!(restored, [first.clone()]);
    assert!(
        matches!(restored[0].target(Path::new(ROOT)).unwrap(), TerminalTarget::Attach { terminal_id, daemon_instance, boot_id }
        if terminal_id == first.terminal_id.unwrap() && daemon_instance == "daemon" && boot_id == "boot")
    );
}

#[test]
fn unclaimed_entries_are_offered_not_dropped() {
    let f = Fixture::new();
    f.open(1, "terminal-1");
    let orphan = f.open(2, "terminal-2");
    let orphans = f
        .projects
        .terminals()
        .orphans(KEY, Path::new(ROOT), &[(1, "terminal-1".into())])
        .unwrap();
    assert_eq!(orphans, [orphan.clone()]);
    assert_eq!(f.entries().len(), 2);
    let adopted = f
        .projects
        .terminals()
        .claim(KEY, Path::new(ROOT), &orphan.command_id, 1, "terminal-3")
        .unwrap();
    assert_eq!(adopted.terminal_id, orphan.terminal_id);
    assert!(matches!(
        adopted.target(Path::new(ROOT)).unwrap(),
        TerminalTarget::Attach { .. }
    ));
    let mut link = Link::default();
    f.projects
        .terminals()
        .close(KEY, Path::new(ROOT), &adopted.command_id, true, Some(&mut link))
        .unwrap();
    assert_eq!(link.stopped, [orphan.terminal_id.unwrap()]);
    assert_eq!(f.entries().len(), 1);
}

#[test]
fn close_by_hand_while_connected_stops_and_forgets() {
    let f = Fixture::new();
    let entry = f.open(1, "terminal-1");
    let mut link = Link::default();
    f.projects
        .terminals()
        .close(KEY, Path::new(ROOT), &entry.command_id, true, Some(&mut link))
        .unwrap();
    assert_eq!(link.stopped, [entry.terminal_id.unwrap()]);
    assert!(f.entries().is_empty());
}

#[test]
fn close_by_hand_while_disconnected_queues_and_the_next_connection_stops() {
    let f = Fixture::new();
    let entry = f.open(1, "terminal-1");
    f.projects
        .terminals()
        .close(KEY, Path::new(ROOT), &entry.command_id, true, None)
        .unwrap();
    assert_eq!(f.entries()[0].state, TerminalState::StopQueued);
    assert!(f.entries()[0].target(Path::new(ROOT)).is_err());
    let mut link = Link::default();
    let reopened = RemoteProjects::new(Some(f.state.as_os_str()), None);
    assert!(reopened
        .terminals()
        .send_queued(KEY, Path::new(ROOT), &mut link)
        .unwrap()
        .is_empty());
    assert_eq!(link.stopped, [entry.terminal_id.unwrap()]);
    assert!(f.entries().is_empty());
}

#[test]
fn a_failed_stop_is_retained_and_does_not_hold_the_rest() {
    let f = Fixture::new();
    let first = f.open(1, "terminal-1");
    let second = f.opening(2, "terminal-2");
    for entry in [&first, &second] {
        f.projects
            .terminals()
            .queue_stop(KEY, Path::new(ROOT), &entry.command_id)
            .unwrap();
    }
    let mut link = Link {
        fail: Some(first.command_id.clone()),
        ..Link::default()
    };
    assert_eq!(
        f.projects
            .terminals()
            .send_queued(KEY, Path::new(ROOT), &mut link)
            .unwrap()
            .len(),
        1
    );
    assert!(link.opened.is_empty(), "a stop must never start a shell");
    assert_eq!(f.entries().len(), 1);
    assert_eq!(f.entries()[0].command_id, first.command_id);
}

#[test]
fn a_signal_close_or_crash_leaves_the_record() {
    let f = Fixture::new();
    let entry = f.open(1, "terminal-1");
    let mut link = Link::default();
    f.projects
        .terminals()
        .close(KEY, Path::new(ROOT), &entry.command_id, false, Some(&mut link))
        .unwrap();
    assert!(link.stopped.is_empty());
    assert_eq!(f.entries(), [entry]);
}

#[test]
fn gone_is_shown_once_then_forgotten() {
    let f = Fixture::new();
    let entry = f.open(1, "terminal-1");
    let mut shown = 0;
    for _ in 0..2 {
        f.projects
            .terminals()
            .show_gone(KEY, Path::new(ROOT), &entry.command_id, || shown += 1)
            .unwrap();
    }
    assert_eq!(shown, 1);
    assert!(f.entries().is_empty());
}

#[test]
fn a_key_change_moves_the_record() {
    for host in ["example-host", "user@example-host:2222"] {
        let f = Fixture::new();
        let mut project = project(KEY, ROOT);
        project.host = host.into();
        f.projects.save(&project).unwrap();
        let entry = f.open(1, "terminal-1");
        let records = f.projects.terminals();
        records.queue_stop(KEY, Path::new(ROOT), &entry.command_id).unwrap();
        let before = f.entries();
        assert_eq!(
            records.record(KEY, Path::new(ROOT)).unwrap().host.as_deref(),
            Some(host)
        );
        assert!(records.reconcile_key("SHA256:new", Path::new(ROOT)).unwrap().is_empty());
        assert_eq!(records.record("SHA256:new", Path::new(ROOT)).unwrap().entries, before);
        assert!(!records.path(KEY, Path::new(ROOT)).unwrap().exists());
        assert!(records.reconcile_key("SHA256:new", Path::new(ROOT)).unwrap().is_empty());
        assert_eq!(records.record("SHA256:new", Path::new(ROOT)).unwrap().entries.len(), 1);
    }
}

#[test]
fn a_later_builds_fields_are_kept() {
    let f = Fixture::new();
    let entry = f.opening(1, "terminal-1");
    let path = f.projects.terminals().path(KEY, Path::new(ROOT)).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["future"] = serde_json::json!({"answer": 42});
    value["entries"][0]["later"] = serde_json::json!([1, 2, 3]);
    agent::private_fs::write_private(&path, &serde_json::to_vec(&value).unwrap()).unwrap();
    f.projects
        .terminals()
        .opened(KEY, Path::new(ROOT), &entry.command_id, "terminal", "daemon", "boot")
        .unwrap();
    let updated: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(updated["future"], value["future"]);
    assert_eq!(updated["entries"][0]["later"], value["entries"][0]["later"]);
    for field in ["terminal_id", "boot_id", "command_id", "module", "state", "opened_at"] {
        let mut missing = updated.clone();
        missing["entries"][0].as_object_mut().unwrap().remove(field);
        agent::private_fs::write_private(&path, &serde_json::to_vec(&missing).unwrap()).unwrap();
        assert!(
            f.projects.terminals().record(KEY, Path::new(ROOT)).is_err(),
            "missing {field}"
        );
    }
}

#[test]
fn the_record_is_private_and_durable() {
    let f = Fixture::new();
    let entry = f.opening(1, "terminal-1");
    let path = f.projects.terminals().path(KEY, Path::new(ROOT)).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    for dir in [path.parent().unwrap(), path.parent().unwrap().parent().unwrap()] {
        assert_eq!(std::fs::metadata(dir).unwrap().permissions().mode() & 0o777, 0o700);
    }
    let old = std::fs::read(&path).unwrap();
    let temporary = eitri_core::state_paths::temporary(&path);
    agent::private_fs::write_private(&temporary, b"interrupted record").unwrap();
    assert_eq!(f.entries(), [entry.clone()]);
    // Refuse replacement before rename by occupying its temporary path with a directory.
    std::fs::remove_file(&temporary).unwrap();
    std::fs::create_dir(&temporary).unwrap();
    assert!(f
        .projects
        .terminals()
        .queue_stop(KEY, Path::new(ROOT), &entry.command_id)
        .is_err());
    assert_eq!(std::fs::read(&path).unwrap(), old);
}

#[test]
fn host_list_prints_terminals_escaped() {
    let response = wire::ListTerminalsResponse {
        terminals: vec![
            wire::TerminalInfo {
                terminal_id: "0123456789abcdef".into(),
                cwd: "/srv/project\u{1b}[31m".into(),
                foreground_job: Some(true),
                foreground_name: "job\u{202e}name".into(),
                detached_since_unix_ms: 42,
                ..Default::default()
            },
            wire::TerminalInfo {
                terminal_id: "exited".into(),
                start_dir: ROOT.into(),
                attached: true,
                exit: Some(wire::TerminalExited {
                    code: Some(7),
                    signal: None,
                    left: false,
                }),
                ..Default::default()
            },
        ],
        truncated: true,
    };
    let detached = terminal_lines(&response)[0]
        .split("no window since ")
        .nth(1)
        .unwrap()
        .split("  running")
        .next()
        .unwrap()
        .to_owned();
    assert!(
        detached.contains("1970-01-01") || detached.contains("1969-12-31"),
        "{detached}"
    );
    assert!(detached.contains(":"), "a readable local time: {detached}");
    let expected = format!(
        "  0123456789ab  {}  no window since {detached}  running {}",
        eitri_core::onboarding::display::display_text("/srv/project\u{1b}[31m"),
        eitri_core::onboarding::display::display_text("job\u{202e}name")
    );
    assert_eq!(
        terminal_lines(&response),
        [
            expected,
            format!("  exited  {ROOT}  attached  exited with status 7"),
            "  terminal list truncated".into()
        ]
    );
    assert!(!terminal_lines(&response).join("\n").contains('\u{1b}'));
    assert!(!terminal_lines(&response).join("\n").contains('\u{202e}'));
}

#[test]
fn host_stop_and_uninstall_clear_records() {
    let f = Fixture::new();
    f.open(1, "terminal-1");
    let records = f.projects.terminals();
    records
        .begin_open("SHA256:other", Path::new(ROOT), 1, "terminal-1", "0.3.0-example", 42)
        .unwrap();
    records.clear_host(KEY).unwrap();
    assert!(f.entries().is_empty());
    assert_eq!(
        records.record("SHA256:other", Path::new(ROOT)).unwrap().entries.len(),
        1
    );
    f.open(1, "terminal-1");
    records.clear_host(KEY).unwrap();
    assert!(f.entries().is_empty());
}

fn project(key: &str, root: &str) -> eitri_core::remote_project::RemoteProject {
    eitri_core::remote_project::RemoteProject {
        version: 1,
        host: "example-host".into(),
        host_key_sha256: key.into(),
        root: root.into(),
        helper_version: "0.3.0-example".into(),
        sync: None,
        created_at: 42,
        queued_stops: Vec::new(),
    }
}

#[test]
fn an_interrupted_key_move_is_retried_on_the_next_open() {
    use eitri_core::remote_project::{reconcile_host_key, terminals::reconcile_records};
    let f = Fixture::new();
    let entry = f.open(1, "terminal-1");
    let old = project(KEY, ROOT);
    f.projects.save(&old).unwrap();
    // The project update survived, but the process stopped before moving its terminal record.
    let (current, _) = reconcile_host_key(&f.projects, &old, "SHA256:new").unwrap();
    let (current, changed) = reconcile_host_key(&f.projects, &current, "SHA256:new").unwrap();
    assert!(changed.is_none());
    let notices = std::cell::RefCell::new(Vec::new());
    for _ in 0..2 {
        reconcile_records(&f.projects, &current, &|line| notices.borrow_mut().push(line));
    }
    assert!(notices.into_inner().is_empty());
    assert_eq!(
        f.projects
            .terminals()
            .record("SHA256:new", Path::new(ROOT))
            .unwrap()
            .entries,
        [entry]
    );
    assert!(!f.projects.terminals().path(KEY, Path::new(ROOT)).unwrap().exists());
}

#[test]
fn key_reconciliation_keeps_conflicts_and_moves_the_rest() {
    let f = Fixture::new();
    let conflict = f.open(1, "terminal-1");
    let moved = f.open(2, "terminal-2");
    let records = f.projects.terminals();
    let destination = records
        .begin_open("SHA256:new", Path::new(ROOT), 1, "terminal-1", "0.3.0-example", 43)
        .unwrap();
    let other = records
        .begin_open(
            "SHA256:other",
            Path::new("/srv/other"),
            1,
            "terminal",
            "0.3.0-example",
            43,
        )
        .unwrap();
    for _ in 0..2 {
        let notices = records.reconcile_key("SHA256:new", Path::new(ROOT)).unwrap();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains(&conflict.command_id));
        assert_eq!(f.entries(), [conflict.clone()]);
        assert_eq!(
            records.record("SHA256:new", Path::new(ROOT)).unwrap().entries,
            [destination.clone(), moved.clone()]
        );
        assert_eq!(
            records.record("SHA256:other", Path::new("/srv/other")).unwrap().entries,
            [other.clone()]
        );
    }
}

#[test]
fn a_failed_move_and_a_duplicate_after_a_crash_are_recoverable() {
    let f = Fixture::new();
    let entry = f.open(1, "terminal-1");
    let records = f.projects.terminals();
    let path = records.path("SHA256:new", Path::new(ROOT)).unwrap();
    let temporary = eitri_core::state_paths::temporary(&path);
    std::fs::create_dir(&temporary).unwrap();
    assert!(records.reconcile_key("SHA256:new", Path::new(ROOT)).is_err());
    assert_eq!(f.entries(), [entry.clone()]);
    std::fs::remove_dir(temporary).unwrap();
    // The destination was synced, but the source was not yet removed.
    let mut record = records.record(KEY, Path::new(ROOT)).unwrap();
    record.host_key = "SHA256:new".into();
    agent::private_fs::write_private(&path, &serde_json::to_vec(&record).unwrap()).unwrap();
    assert!(records.reconcile_key("SHA256:new", Path::new(ROOT)).unwrap().is_empty());
    assert_eq!(records.record("SHA256:new", Path::new(ROOT)).unwrap().entries, [entry]);
    assert!(!records.path(KEY, Path::new(ROOT)).unwrap().exists());
}

#[test]
fn a_damaged_terminal_record_does_not_deny_window_reconciliation() {
    let f = Fixture::new();
    f.open(1, "terminal-1");
    let records = f.projects.terminals();
    let path = records.path(KEY, Path::new(ROOT)).unwrap();
    agent::private_fs::write_private(&path, b"damaged").unwrap();
    let notices = std::cell::RefCell::new(Vec::new());
    // This is the unconditional, infallible hook called immediately after host-key reconciliation.
    eitri_core::remote_project::terminals::reconcile_records(&f.projects, &project(KEY, ROOT), &|line| {
        notices.borrow_mut().push(line);
    });
    assert!(notices.borrow().iter().any(|line| line.contains("terminal record")));
    assert_eq!(std::fs::read(path).unwrap(), b"damaged");
}

#[test]
fn stopping_an_unanswered_open_closes_only_its_inventory_match() {
    let f = Fixture::new();
    let entry = f.opening(1, "terminal-1");
    let mut link = Link::default();
    let id = link.open(entry.target(Path::new(ROOT)).unwrap());
    let other = link.open(TerminalTarget::Open {
        command_id: "other".into(),
        cwd: ROOT.into(),
    });
    f.projects
        .terminals()
        .close(KEY, Path::new(ROOT), &entry.command_id, true, Some(&mut link))
        .unwrap();
    assert_eq!(link.stopped, [id]);
    assert_eq!(link.opened.len(), 2);
    assert!(link.opened.values().any(|id| id == &other));
    assert!(f.entries().is_empty());
}

#[test]
fn stopping_an_unanswered_open_with_no_inventory_match_forgets_without_opening() {
    let f = Fixture::new();
    let entry = f.opening(1, "terminal-1");
    let mut link = Link::default();
    f.projects
        .terminals()
        .close(KEY, Path::new(ROOT), &entry.command_id, true, Some(&mut link))
        .unwrap();
    assert!(link.opened.is_empty());
    assert!(link.stopped.is_empty());
    assert!(f.entries().is_empty());
}

#[test]
fn unchanged_updates_do_not_replace_or_create_records() {
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new();
    let records = f.projects.terminals();
    let path = records.path(KEY, Path::new(ROOT)).unwrap();
    records.forget(KEY, Path::new(ROOT), "absent").unwrap();
    assert!(!path.exists());
    let entry = f.opening(1, "terminal-1");
    let inode = std::fs::metadata(&path).unwrap().ino();
    records
        .begin_open(KEY, Path::new(ROOT), 1, "terminal-1", "0.3.0-example", 42)
        .unwrap();
    records.forget(KEY, Path::new(ROOT), "absent").unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    records.queue_stop(KEY, Path::new(ROOT), &entry.command_id).unwrap();
    let inode = std::fs::metadata(&path).unwrap().ino();
    records.queue_stop(KEY, Path::new(ROOT), &entry.command_id).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    records.reconcile_key("SHA256:new", Path::new(ROOT)).unwrap();
    records.forget(KEY, Path::new(ROOT), &entry.command_id).unwrap();
    assert!(!path.exists(), "an old window must not recreate the moved record");
}

#[test]
fn begin_open_refuses_more_than_256_entries_but_allows_an_existing_intent() {
    let f = Fixture::new();
    let intent = f.opening(1, "terminal-0");
    let records = f.projects.terminals();
    let mut record = records.record(KEY, Path::new(ROOT)).unwrap();
    for i in 1..256 {
        let mut entry = intent.clone();
        entry.module = format!("terminal-{i}");
        entry.command_id = format!("command-{i}");
        record.entries.push(entry);
    }
    let path = records.path(KEY, Path::new(ROOT)).unwrap();
    agent::private_fs::write_private(&path, &serde_json::to_vec(&record).unwrap()).unwrap();
    assert_eq!(
        records
            .begin_open(KEY, Path::new(ROOT), 1, "terminal-0", "0.3.0-example", 42)
            .unwrap(),
        intent
    );
    let error = records
        .begin_open(KEY, Path::new(ROOT), 1, "one-more", "0.3.0-example", 42)
        .unwrap_err();
    assert!(error.to_string().contains("256"));
    assert_eq!(f.entries(), record.entries);
}

#[test]
fn a_damaged_record_does_not_prevent_clearing_other_records() {
    let f = Fixture::new();
    f.open(1, "terminal-1");
    let records = f.projects.terminals();
    records
        .begin_open(KEY, Path::new("/srv/other"), 1, "terminal", "0.3.0-example", 42)
        .unwrap();
    records
        .begin_open("SHA256:other", Path::new(ROOT), 1, "terminal", "0.3.0-example", 42)
        .unwrap();
    let damaged = records.path(KEY, Path::new(ROOT)).unwrap();
    agent::private_fs::write_private(&damaged, b"damaged").unwrap();
    let notices = records.clear_host(KEY).unwrap();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains(damaged.file_name().unwrap().to_str().unwrap()));
    assert!(damaged.exists());
    assert!(!records.path(KEY, Path::new("/srv/other")).unwrap().exists());
    assert_eq!(
        records.record("SHA256:other", Path::new(ROOT)).unwrap().entries.len(),
        1
    );
}

#[test]
fn window_terminal_hooks_are_unconditional_and_do_not_propagate_errors() {
    let source = eitri_core::source_scan::read_required("core/src/remote_host/window.rs");
    let (_, body) = eitri_core::source_scan::function_in(&[source], "open_window");
    let code: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    let after_reconciliation = code
        .split("reconcile_host_key(")
        .nth(1)
        .unwrap()
        .split_once("?;")
        .unwrap()
        .1;
    assert!(
        after_reconciliation.starts_with("crate::remote_project::terminals::reconcile_records(store,&project,band);")
    );
    assert_eq!(code.matches("terminals::reconcile_records(").count(), 1);
    assert!(code.contains("terminals::send_queued_stops(store,&connection,band);"));
    // The terminals' hooks run before the sync's at both points: records follow the key before a changed key pauses
    // the sync, and queued stops go out before the sync actor starts on the connection.
    let position = |needle: &str| {
        code.find(needle)
            .unwrap_or_else(|| panic!("{needle} missing from open_window"))
    };
    assert!(position("terminals::reconcile_records(") < position("sessions::note_key_change("));
    assert!(position("terminals::send_queued_stops(") < position("sync(&connection,&project);"));
    assert!(position("sync(&connection,&project);") < position("Ok(Opened{"));
}

fn set_record_host(f: &Fixture, key: &str, host: Option<&str>) {
    let path = f.projects.terminals().path(key, Path::new(ROOT)).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    if let Some(host) = host {
        value["host"] = host.into();
    } else {
        value.as_object_mut().unwrap().remove("host");
    }
    agent::private_fs::write_private(&path, &serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn forgetting_one_host_and_opening_another_leaves_its_terminals_untouched() {
    use eitri_core::remote_project::terminals::reconcile_records;
    for other_host in ["other-host", "user@example-host", "example-host:2222"] {
        let f = Fixture::new();
        f.open(1, "terminal");
        set_record_host(&f, KEY, Some("example-host"));
        let path = f.projects.terminals().path(KEY, Path::new(ROOT)).unwrap();
        let before = std::fs::read(&path).unwrap();
        f.projects.forget(Path::new(ROOT)).unwrap();
        let mut other = project("SHA256:other", ROOT);
        other.host = other_host.into();
        f.projects.save(&other).unwrap();
        let notices = std::cell::RefCell::new(Vec::new());
        reconcile_records(&f.projects, &other, &|line| notices.borrow_mut().push(line));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(notices.borrow().is_empty());
        assert!(!f
            .projects
            .terminals()
            .path("SHA256:other", Path::new(ROOT))
            .unwrap()
            .exists());
        // An unrelated host's malformed entries must not even be decoded or diagnosed.
        let mut value: serde_json::Value = serde_json::from_slice(&before).unwrap();
        value["entries"] = serde_json::json!([{"invalid": true}]);
        let bytes = serde_json::to_vec(&value).unwrap();
        agent::private_fs::write_private(&path, &bytes).unwrap();
        reconcile_records(&f.projects, &other, &|line| notices.borrow_mut().push(line));
        assert!(notices.borrow().is_empty(), "{:?}", notices.borrow());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn a_record_without_the_typed_host_is_never_moved() {
    let f = Fixture::new();
    f.open(1, "terminal");
    set_record_host(&f, KEY, None);
    let records = f.projects.terminals();
    let path = records.path(KEY, Path::new(ROOT)).unwrap();
    let before = std::fs::read(&path).unwrap();
    // Kept as it is, and said: it cannot be told from another server's record of the same path.
    let notices = records.reconcile_key("SHA256:new", Path::new(ROOT)).unwrap();
    assert!(notices.len() == 1 && notices[0].contains("no host name"), "{notices:?}");
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert!(!records.path("SHA256:new", Path::new(ROOT)).unwrap().exists());
}

#[test]
fn a_merge_over_256_entries_keeps_them_all_and_reports_the_count() {
    let f = Fixture::new();
    let intent = f.opening(1, "terminal-0");
    set_record_host(&f, KEY, Some("example-host"));
    let records = f.projects.terminals();
    records
        .begin_open("SHA256:new", Path::new(ROOT), 2, "destination", "0.3.0-example", 42)
        .unwrap();
    set_record_host(&f, "SHA256:new", Some("example-host"));
    let mut record = records.record(KEY, Path::new(ROOT)).unwrap();
    for i in 1..256 {
        let mut entry = intent.clone();
        entry.module = format!("terminal-{i}");
        entry.command_id = format!("command-{i}");
        record.entries.push(entry);
    }
    let path = records.path(KEY, Path::new(ROOT)).unwrap();
    agent::private_fs::write_private(&path, &serde_json::to_vec(&record).unwrap()).unwrap();
    let notices = records.reconcile_key("SHA256:new", Path::new(ROOT)).unwrap();
    assert!(
        notices.iter().any(|line| line.contains("257") && line.contains("256")),
        "{notices:?}"
    );
    assert_eq!(
        records.record("SHA256:new", Path::new(ROOT)).unwrap().entries.len(),
        257
    );
    assert!(!path.exists());
}

#[test]
fn data_loss_forgets_a_stop_with_a_notice() {
    struct Refused;
    impl TerminalControl for Refused {
        fn stop(&mut self, _: &Path, _: &TerminalEntry) -> io::Result<Option<String>> {
            Err(io::Error::other(tonic::Status::data_loss("invalid terminal id")))
        }
    }
    let f = Fixture::new();
    let entry = f.opening(1, "terminal");
    let records = f.projects.terminals();
    records.queue_stop(KEY, Path::new(ROOT), &entry.command_id).unwrap();
    let notices = records.send_queued(KEY, Path::new(ROOT), &mut Refused).unwrap();
    assert!(f.entries().is_empty());
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("forgotten"));
    assert!(notices[0].contains("invalid terminal id"));
}

#[test]
fn a_terminal_never_attached_has_no_detached_time() {
    let response = wire::ListTerminalsResponse {
        terminals: vec![wire::TerminalInfo {
            terminal_id: "a".repeat(32),
            start_dir: ROOT.into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(
        terminal_lines(&response),
        [format!("  aaaaaaaaaaaa  {ROOT}  no window yet")]
    );
}

#[test]
fn a_queued_stop_does_not_reserve_a_reused_module() {
    let f = Fixture::new();
    let old = f.open(1, "terminal");
    f.projects
        .terminals()
        .queue_stop(KEY, Path::new(ROOT), &old.command_id)
        .unwrap();
    let new = f.opening(1, "terminal");
    assert_ne!(old.command_id, new.command_id);
    let entries = f.entries();
    assert_eq!(entries.len(), 2, "both the stop and the new intent survive loading");
    assert_eq!(entries[0].state, TerminalState::StopQueued);
    assert_eq!(f.opening(1, "terminal"), new);
    let mut link = Link::default();
    f.projects
        .terminals()
        .close(KEY, Path::new(ROOT), &old.command_id, true, Some(&mut link))
        .unwrap();
    assert_eq!(f.entries(), [new]);
}

#[test]
fn a_queued_stop_does_not_refuse_a_claim_into_its_module() {
    let f = Fixture::new();
    let old = f.open(1, "terminal");
    f.projects
        .terminals()
        .queue_stop(KEY, Path::new(ROOT), &old.command_id)
        .unwrap();
    let orphan = f.open(2, "terminal#2");
    let claimed = f
        .projects
        .terminals()
        .claim(KEY, Path::new(ROOT), &orphan.command_id, 1, "terminal")
        .unwrap();
    assert_eq!(claimed.command_id, orphan.command_id);
    assert_eq!(f.entries().len(), 2);
    let mut link = Link::default();
    f.projects
        .terminals()
        .close(KEY, Path::new(ROOT), &old.command_id, true, Some(&mut link))
        .unwrap();
    assert_eq!(f.entries(), [claimed]);
}
