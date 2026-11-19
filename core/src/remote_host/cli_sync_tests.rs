//! The sync verbs against the actor's fakes: what each writes into the record, and what the person reads.

use super::*;
use crate::remote_host::cli::{Agent, Contact, Reach, Terminal};
use crate::remote_host::install::InstallPlan;
use crate::remote_project::PauseRecord;
use crate::remote_sync::actor::tests::{wait, Lab};
use crate::remote_sync::actor::PAUSE_RECREATE;
use crate::remote_sync::engine::{Conflict, EntryKind, SyncEngine};
use crate::remote_sync::sessions::SyncHelper;
use agent::ssh::{ChannelLauncher, ConnectError};
use std::collections::VecDeque;

struct FakeReach {
    engine: Arc<dyn SyncEngine>,
    helper: Arc<dyn SyncHelper>,
}

impl Reach for FakeReach {
    fn connect(&mut self, _host: &str) -> Result<String, ConnectError> {
        unreachable!("the sync verbs never connect for themselves")
    }
    fn first_contact(&mut self, _host: &str) -> Result<Contact, String> {
        unreachable!()
    }
    fn launcher(&self, _helper: &Path) -> Arc<dyn ChannelLauncher> {
        unreachable!()
    }
    fn plan(&mut self) -> Result<InstallPlan, String> {
        unreachable!()
    }
    fn agents(&mut self, _host: &str, _key: &str, _helper: &Path) -> Result<Vec<Agent>, String> {
        unreachable!()
    }
    fn sync_engine(&mut self) -> Result<Arc<dyn SyncEngine>, crate::remote_sync::SyncError> {
        Ok(self.engine.clone())
    }
    fn sync_helper(&mut self, _project: &RemoteProject) -> Result<Arc<dyn SyncHelper>, String> {
        Ok(self.helper.clone())
    }
}

#[derive(Default)]
struct Term {
    said: Vec<String>,
    answers: VecDeque<bool>,
    asked: Vec<String>,
    cancelled: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl Terminal for Term {
    fn cancelled(&self) -> bool {
        self.cancelled
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
    }
    fn say(&mut self, line: &str) {
        self.said.push(line.to_owned());
    }
    fn ask(&mut self, question: &str) -> bool {
        self.asked.push(question.to_owned());
        self.answers.pop_front().unwrap_or(false)
    }
}

fn run(lab: &Lab, term: &mut Term, cwd: &Path, args: &[&str]) -> i32 {
    run_io(lab, term, cwd, args, b"", &mut vec![])
}

fn run_io(lab: &Lab, term: &mut Term, cwd: &Path, args: &[&str], input: &[u8], output: &mut Vec<u8>) -> i32 {
    let mut reach = FakeReach {
        engine: lab.engine.clone(),
        helper: lab.helper.clone(),
    };
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let enabled = || false;
    let input: Box<dyn std::io::BufRead + Send> = Box::new(std::io::Cursor::new(input.to_vec()));
    run_sync_in(
        &args,
        &mut Context {
            store: &lab.store,
            reach: &mut reach,
            term,
            remote_dev_enabled: &enabled,
            now: 0,
        },
        cwd,
        input,
        output,
    )
}

fn pause(reason: &str, path: Option<&str>) -> PauseRecord {
    PauseRecord {
        reason: reason.into(),
        detail: format!("paused for {reason}"),
        since_unix_ms: 1,
        path: path.map(str::to_owned),
    }
}

#[test]
fn verbs_find_the_project_from_the_path_and_refuse_outside_any() {
    let lab = Lab::new();
    let mut term = Term::default();
    assert_eq!(run(&lab, &mut term, Path::new("/"), &["status"]), 1);
    assert!(term.said.last().unwrap().contains("is not inside a remote project"));
    let mut term = Term::default();
    // With remote development off, and no window following it, status still answers.
    assert_eq!(run(&lab, &mut term, &lab.root.join("src"), &["status"]), 0);
    assert!(
        term.said[0].starts_with("sync with example-host for "),
        "{:?}",
        term.said
    );
    assert!(
        term.said
            .iter()
            .any(|l| l.contains("its sync is paused until one does")),
        "{:?}",
        term.said
    );
    let mut term = Term::default();
    let mut output = Vec::new();
    assert_eq!(
        run_io(
            &lab,
            &mut term,
            Path::new("/"),
            &["status", "--json", lab.root.to_str().unwrap()],
            b"",
            &mut output
        ),
        0
    );
    let json: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["host"], "example-host");
}

#[test]
fn json_inspection_escapes_terminal_controls_and_round_trips() {
    use crate::agent_bridge::is_control_like;
    use crate::remote_sync::status_schema::{ConflictEntry, ProblemEntry};
    let (lab, _lock) = conflict_fixture(Lab::new());
    let dir = actor::project_dir(&lab.state, &lab.project());
    let mut status = status_file::read_status_file(&dir).unwrap();
    let name = "a\u{9b}2Jb\u{9d}0;t\u{7}c\u{202e}d\u{2028}e\u{7f}f\ng";
    status.conflicts.push(ConflictEntry {
        path: name.into(),
        laptop_kind: "file".into(),
        server_kind: "file".into(),
    });
    status.problems.push(ProblemEntry {
        path: name.into(),
        side: "server".into(),
        text: "\u{9b}2J".into(),
    });
    status_file::write_status(&dir, &status).unwrap();
    for (verb, pointer) in [("status", "/conflicts"), ("conflicts", "")] {
        let mut output = Vec::new();
        let mut term = Term::default();
        assert_eq!(
            run_io(&lab, &mut term, &lab.root, &[verb, "--json"], b"", &mut output),
            0
        );
        let text = String::from_utf8(output).unwrap();
        let body = text.strip_suffix('\n').unwrap();
        assert!(!body.chars().any(is_control_like), "{verb}: {body:?}");
        let json: serde_json::Value = serde_json::from_str(body).unwrap();
        let listed = json.pointer(pointer).unwrap().as_array().unwrap();
        assert!(listed.iter().any(|c| c["path"] == name), "{verb}: {json}");
    }
    let line = event_json(&SyncEvent::Line(name.into()));
    assert!(!line.chars().any(is_control_like), "{line:?}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&line).unwrap()["text"], name);
}

#[test]
fn human_status_names_problem_actions() {
    let (lab, _lock) = conflict_fixture(Lab::new());
    let dir = actor::project_dir(&lab.state, &lab.project());
    let mut status = status_file::read_status_file(&dir).unwrap();
    status.problems.push(crate::remote_sync::status_schema::ProblemEntry {
        path: "entry".into(),
        side: "server".into(),
        text: "permission denied".into(),
    });
    status.problems.push(crate::remote_sync::status_schema::ProblemEntry {
        path: "unreadable".into(),
        side: "laptop".into(),
        text: "permission denied".into(),
    });
    status_file::write_status(&dir, &status).unwrap();
    let mut term = Term::default();
    assert_eq!(run(&lab, &mut term, &lab.root, &["status"]), 0);
    assert!(term.said.iter().any(|s| s.contains("not synced: entry")
        && s.contains("fix the entry on the server")
        && s.contains("eitri host sync exclude <path>")));
    // The server never received a laptop-side entry: neither the server nor its git can help with it.
    let laptop = term.said.iter().find(|s| s.contains("not synced: unreadable")).unwrap();
    assert!(laptop.contains("fix the entry on this laptop"), "{laptop}");
    assert!(laptop.contains("eitri host sync exclude <path>"), "{laptop}");
    assert!(
        !laptop.contains("on the server") && !laptop.contains("terminal on"),
        "{laptop}"
    );
}

#[test]
fn exclude_include_and_their_refusals() {
    let lab = Lab::new();
    let mut term = Term::default();
    assert_eq!(run(&lab, &mut term, &lab.root, &["exclude", "assets/video/"]), 0);
    assert_eq!(lab.record().excludes_added, vec!["assets/video".to_owned()]);
    assert!(term.said[0].contains("stays on example-host"));
    assert_eq!(run(&lab, &mut term, &lab.root, &["include", "assets/video"]), 0);
    assert!(lab.record().excludes_added.is_empty());
    assert!(lab.record().excludes_removed.is_empty());
    assert_eq!(run(&lab, &mut term, &lab.root, &["include", "notes"]), 0);
    assert_eq!(lab.record().excludes_removed, vec!["notes".to_owned()]);
    for bad in [".git/config", ".", "a/../.."] {
        let mut term = Term::default();
        assert_eq!(run(&lab, &mut term, &lab.root, &["exclude", bad]), 1, "{bad}");
    }
}

#[test]
fn allow_takes_the_tripped_path_or_the_batch() {
    let lab = Lab::new();
    let mut term = Term::default();
    assert_eq!(run(&lab, &mut term, &lab.root, &["allow"]), 1);
    lab.update_record(|r| r.paused = Some(pause(PAUSE_TRIPWIRE, Some("assets"))));
    assert_eq!(run(&lab, &mut term, &lab.root, &["allow"]), 0);
    let record = lab.record();
    assert_eq!(
        (record.tripwire_allowed, record.paused),
        (vec!["assets".to_owned()], None)
    );
    lab.update_record(|r| r.paused = Some(pause(PAUSE_TRIPWIRE, None)));
    assert_eq!(run(&lab, &mut term, &lab.root, &["allow"]), 0);
    assert!(lab.record().tripwire_allow_batch);
    assert_eq!(run(&lab, &mut term, &lab.root.join("data"), &["allow", "big.bin"]), 0);
    assert!(lab.record().tripwire_allowed.contains(&"data/big.bin".to_owned()));
}

#[test]
fn resume_asks_and_accepts_a_changed_host_key() {
    let lab = Lab::new();
    lab.update_record(|r| {
        r.host_key_sha256 = "SHA256:old".into();
        r.paused = Some(pause(PAUSE_HOST_KEY, None));
    });
    let mut term = Term::default();
    assert_eq!(run(&lab, &mut term, &lab.root, &["resume"]), 0);
    assert!(term.asked[0].contains("Its sessions are re-created"));
    assert!(lab.record().paused.is_some(), "no answer changes nothing");
    let mut term = Term {
        answers: VecDeque::from([true]),
        ..Default::default()
    };
    assert_eq!(run(&lab, &mut term, &lab.root, &["resume"]), 0);
    let record = lab.record();
    assert_eq!(record.paused, None);
    assert_eq!(record.accepted_host_key.as_deref(), Some("SHA256:example"));
    lab.update_record(|r| r.paused = Some(pause(PAUSE_RECREATE, None)));
    let mut term = Term {
        answers: VecDeque::from([true]),
        ..Default::default()
    };
    assert_eq!(run(&lab, &mut term, &lab.root, &["resume"]), 0);
    assert_eq!(lab.record().paused, None);
}

/// A host-key pause whose record already has the project's key (an open window saw another key; no window has
/// settled it yet) names no key it cannot know: the plain question, and no key is accepted.
#[test]
fn resume_of_a_key_pause_this_machine_has_not_settled_names_no_key() {
    let lab = Lab::new();
    lab.update_record(|r| r.paused = Some(pause(PAUSE_HOST_KEY, None)));
    let mut term = Term {
        answers: VecDeque::from([true]),
        ..Default::default()
    };
    assert_eq!(run(&lab, &mut term, &lab.root, &["resume"]), 0);
    assert!(!term.asked[0].contains("now shows"), "{}", term.asked[0]);
    assert!(!term.asked[0].contains("re-created"), "{}", term.asked[0]);
    let record = lab.record();
    assert_eq!((record.paused, record.accepted_host_key), (None, None));
}

#[test]
fn flush_runs_the_engine_flush_of_the_tree_session() {
    let lab = Lab::new();
    let mut term = Term::default();
    assert_eq!(run(&lab, &mut term, &lab.root, &["flush"]), 1, "no session yet");
    lab.update_record(|r| r.tree_session = Some(lab.tree()));
    let mut term = Term::default();
    assert_eq!(run(&lab, &mut term, &lab.root, &["flush"]), 0);
    assert_eq!(lab.engine.calls("flush"), vec![format!("flush {}", lab.tree())]);
}

#[test]
fn watch_json_streams_status_lines() {
    let lab = Lab::new();
    let config = lab.config(HolderKind::Watch);
    let (read_end, mut write_end) = std::io::pipe().unwrap();
    let input: Box<dyn std::io::BufRead + Send> = Box::new(std::io::BufReader::new(read_end));
    let output = Arc::new(Mutex::new(Vec::<u8>::new()));
    struct Shared(Arc<Mutex<Vec<u8>>>);
    impl Write for Shared {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut sink = Shared(output.clone());
    let project = lab.project();
    let engine: Arc<dyn SyncEngine> = lab.engine.clone();
    let helper: Arc<dyn SyncHelper> = lab.helper.clone();
    std::thread::scope(|scope| {
        let follower = scope.spawn(|| follow(project, engine, helper, config, input, &mut sink));
        wait(|| String::from_utf8_lossy(&output.lock().unwrap()).contains("\"type\":\"status\""));
        // The editor's report arrives on the input.
        use std::io::Write as _;
        let report = serde_json::json!({"unsaved": [lab.root.join("a.rs")], "files": []});
        writeln!(write_end, "{report}").unwrap();
        drop(write_end);
        follower.join().unwrap().unwrap();
    });
    let text = String::from_utf8(output.lock().unwrap().clone()).unwrap();
    for line in text.lines() {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(value["type"] == "status" || value["type"] == "line", "{line}");
    }
    // Input ended: the watch let the lock go, pausing the sessions.
    assert!(!lab.engine.calls("pause").is_empty());
}

#[test]
fn report_lines_parse_and_cannot_tell_is_null() {
    assert_eq!(parse_report("{\"unsaved\": null}"), Some(None));
    let parsed = parse_report("{\"unsaved\": [\"/r/a\"], \"files\": [[\"/r/a\", 5]]}")
        .unwrap()
        .unwrap();
    assert_eq!(parsed.unsaved, vec![PathBuf::from("/r/a")]);
    assert_eq!(parsed.generation(), 5);
    assert_eq!(parse_report("not json"), None);
    let line: serde_json::Value = serde_json::from_str(&event_json(&SyncEvent::Line("sync paused: x".into()))).unwrap();
    assert_eq!(line, serde_json::json!({"type": "line", "text": "sync paused: x"}));
}

#[test]
fn changing_sync_settings_never_sets_up_a_project() {
    let lab = Lab::new();
    lab.store.update(&lab.root, |p| p.sync = None).unwrap();
    for args in [&["exclude", "file"][..], &["include", "file"], &["allow"], &["resume"]] {
        let mut term = Term::default();
        assert_eq!(run(&lab, &mut term, &lab.root, args), 1);
        assert!(term.said.last().unwrap().contains(&lab.project().sync_not_set_up()));
        assert!(lab.project().sync.is_none());
    }
}

#[test]
fn watch_refuses_with_remote_development_off() {
    let lab = Lab::new();
    let mut term = Term::default();
    assert_eq!(run(&lab, &mut term, &lab.root, &["watch", "--json"]), 1);
    assert!(
        term.said.last().unwrap().contains("remote_dev.enabled"),
        "{:?}",
        term.said
    );
    assert!(lab.engine.calls("").is_empty(), "nothing was started");
}

fn conflicted() -> (Lab, agent::private_fs::PrivateLock) {
    conflict_fixture(Lab::new())
}

fn conflict_fixture(lab: Lab) -> (Lab, agent::private_fs::PrivateLock) {
    use crate::remote_sync::SyncKey;
    let project = lab.project();
    let key = SyncKey::new(&project.host_key_sha256, &lab.root);
    let patterns = vec!["/target/".to_string()];
    lab.engine
        .create(&sessions::tree_spec(&key, &lab.root, &patterns))
        .unwrap();
    lab.update_record(|r| {
        r.tree_session = Some(lab.tree());
        r.ignore_digest = ignores::Translated::new(patterns.clone(), vec![]).digest;
    });
    let dir = actor::project_dir(&lab.state, &project);
    status_file::prepare_dir(&lab.state, &dir).unwrap();
    let locks = actor::lock_dir(&lab.state, &project.root);
    status_file::prepare_dir(&lab.state, &locks).unwrap();
    let lock = agent::private_fs::PrivateLock::open(&locks.join("lock")).unwrap();
    assert!(lock.try_lock().unwrap());
    recreate::write_created_patterns(&dir, &patterns).unwrap();
    record_conflict(&lab, EntryKind::File, EntryKind::File);
    std::fs::write(lab.root.join("file"), b"laptop\0\xff").unwrap();
    lab.helper
        .files
        .lock()
        .unwrap()
        .insert("file".into(), b"server\0\xff".to_vec());
    (lab, lock)
}

fn record_conflict(lab: &Lab, laptop: EntryKind, server: EntryKind) {
    let project = lab.project();
    let dir = actor::project_dir(&lab.state, &project);
    let mut tree = lab.watching(&lab.tree(), 1);
    tree.conflicts.push(Conflict {
        path: "file".into(),
        laptop,
        server,
    });
    lab.engine.push(tree.clone());
    status_file::write_status(
        &dir,
        &status_file::compose(&project.host, &lab.record(), Some(&tree), None, None, 1),
    )
    .unwrap();
}

fn choices(lab: &Lab) -> serde_json::Value {
    let mut out = Vec::new();
    assert_eq!(
        run_io(
            lab,
            &mut Term::default(),
            &lab.root,
            &["status", "--json", "--details", "."],
            b"",
            &mut out
        ),
        0
    );
    let details: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let choices = &details["choices"]["file"];
    assert!(choices.is_object(), "{details}");
    choices.clone()
}

fn saved(lab: &Lab) -> Vec<Vec<u8>> {
    let key = crate::remote_sync::SyncKey::new(&lab.project().host_key_sha256, &lab.root);
    std::fs::read_dir(lab.state.join("eitri/remote/conflict-backups").join(key.as_str()))
        .unwrap()
        .map(|entry| std::fs::read(entry.unwrap().path().join("file")).unwrap())
        .collect()
}

#[test]
fn keep_server_backs_up_then_removes_the_laptop_file() {
    let (lab, _holder) = conflicted();
    let mut term = Term::default();
    let mut bytes = vec![];
    assert_eq!(
        run_io(&lab, &mut term, &lab.root, &["show-server", "file"], b"", &mut bytes),
        0
    );
    assert_eq!(bytes, b"server\0\xff");
    assert_eq!(
        run(&lab, &mut term, &lab.root, &["resolve", "file", "--keep", "server"]),
        0,
        "{:?}",
        term.said
    );
    assert!(!lab.root.join("file").exists());
    assert_eq!(saved(&lab), [b"laptop\0\xff"]);
}

#[test]
fn keep_laptop_backs_up_the_server_version_first() {
    let (lab, _holder) = conflicted();
    assert_eq!(
        run(
            &lab,
            &mut Term::default(),
            &lab.root,
            &["resolve", "file", "--keep", "laptop"]
        ),
        0
    );
    assert_eq!(saved(&lab), [b"server\0\xff"]);
    assert!(!lab.helper.files.lock().unwrap().contains_key("file"));
}

#[test]
fn keep_laptop_refuses_when_the_server_file_changed() {
    let (lab, _holder) = conflicted();
    lab.helper
        .refuse_compare
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let mut term = Term::default();
    assert_eq!(
        run(&lab, &mut term, &lab.root, &["resolve", "file", "--keep", "laptop"]),
        1
    );
    assert!(term.said[0].contains("file changed on example-host since you looked; look again"));
    assert_eq!(saved(&lab), [b"server\0\xff"]);
    assert!(lab.helper.files.lock().unwrap().contains_key("file"));
}

#[test]
fn merge_write_preserves_a_laptop_file_changed_since_opening_the_diff() {
    let (lab, _holder) = conflicted();
    let mut term = Term::default();
    let args = [
        "merge-write",
        "file",
        "--server-sha256",
        &actions::hash(b"server\0\xff"),
        "--laptop-sha256",
        &actions::hash(b"laptop\0\xff"),
    ];
    std::fs::write(lab.root.join("file"), b"new laptop edit").unwrap();
    assert_eq!(run_io(&lab, &mut term, &lab.root, &args, b"merged", &mut vec![]), 1);
    assert!(term.said[0].contains("changed on the laptop since you looked"));
    assert_eq!(std::fs::read(lab.root.join("file")).unwrap(), b"new laptop edit");
    assert_eq!(lab.helper.files.lock().unwrap()["file"], b"server\0\xff");
}

#[test]
fn merge_write_refuses_a_stale_server_hash_and_keeps_the_laptop_write() {
    let (lab, _holder) = conflicted();
    let mut term = Term::default();
    let args = ["merge-write", "file", "--server-sha256", &actions::hash(b"earlier")];
    assert_eq!(run_io(&lab, &mut term, &lab.root, &args, b"merged", &mut vec![]), 1);
    assert!(term.said[0].contains("changed on example-host"), "{:?}", term.said);
    assert_eq!(std::fs::read(lab.root.join("file")).unwrap(), b"merged");
    assert_eq!(lab.helper.files.lock().unwrap()["file"], b"server\0\xff");
    assert!(saved(&lab).contains(&b"laptop\0\xff".to_vec()));
    let args = [
        "merge-write",
        "file",
        "--server-sha256",
        &actions::hash(b"server\0\xff"),
    ];
    assert_eq!(run_io(&lab, &mut term, &lab.root, &args, b"merged", &mut vec![]), 0);
    assert_eq!(lab.helper.files.lock().unwrap()["file"], b"merged");
    assert!(saved(&lab).contains(&b"server\0\xff".to_vec()));
}

#[test]
fn actions_refuse_during_a_turn_unknown_activity_and_while_paused() {
    use agent::remote_client::ProjectActivityResponse as Activity;
    let (lab, _holder) = conflicted();
    let mut term = Term::default();
    let args = ["resolve", "file", "--keep", "server"];
    for activity in [
        Activity {
            unknown: true,
            ..Default::default()
        },
        Activity {
            turns_running: 1,
            ..Default::default()
        },
        Activity {
            tasks_running: 1,
            ..Default::default()
        },
    ] {
        lab.helper.activity(activity);
        assert_eq!(run(&lab, &mut term, &lab.root, &args), 1);
        assert!(term.said.last().unwrap().contains("resolve after the turn"));
    }
    lab.helper.activity(Activity::default());
    lab.update_record(|r| r.paused = Some(pause(PAUSE_TRIPWIRE, None)));
    assert_eq!(run(&lab, &mut term, &lab.root, &args), 1);
    assert!(term.said.last().unwrap().contains("paused"));
    lab.update_record(|r| {
        r.paused = None;
        r.excludes_added.push("assets".into());
    });
    assert_eq!(run(&lab, &mut term, &lab.root, &args), 1);
    assert!(term.said.last().unwrap().contains("re-creation is pending"));
    assert_eq!(std::fs::read(lab.root.join("file")).unwrap(), b"laptop\0\xff");
}

#[test]
fn directory_symlink_and_large_conflicts_offer_no_action() {
    let (lab, _holder) = conflicted();
    let path = lab.root.join("file");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let refused = |kind: &str| {
        let choice = choices(&lab);
        assert!(choice["reason"].as_str().unwrap().contains(kind), "{choice}");
        assert_eq!(choice.as_object().unwrap().len(), 1);
    };
    lab.helper.list_file("file", 1);
    record_conflict(&lab, EntryKind::Directory, EntryKind::File);
    refused("directory");
    let args = ["resolve", "file", "--keep", "server"];
    let mut term = Term::default();
    assert_eq!(run(&lab, &mut term, &lab.root, &args), 1);
    std::fs::remove_dir(&path).unwrap();
    std::os::unix::fs::symlink("/srv/project", &path).unwrap();
    record_conflict(&lab, EntryKind::Symlink, EntryKind::File);
    refused("symlink");
    assert_eq!(run(&lab, &mut term, &lab.root, &args), 1);
    std::fs::remove_file(&path).unwrap();
    record_conflict(&lab, EntryKind::File, EntryKind::File);
    std::fs::File::create(&path)
        .unwrap()
        .set_len(actions::FILE_LIMIT + 1)
        .unwrap();
    assert_eq!(run(&lab, &mut term, &lab.root, &args), 1);
    assert!(term
        .said
        .iter()
        .all(|s| s.contains("settle it in the terminal on example-host") && s.contains("Eitri does not act on")));
    std::fs::write(&path, "small").unwrap();
    lab.helper.list_file("file", actions::FILE_LIMIT + 1);
    refused("over 64 MiB");
    lab.helper.list_file("file", actions::MERGE_LIMIT + 1);
    assert_eq!(choices(&lab)["merge"], false);
    lab.helper.list_file("file", actions::MERGE_LIMIT);
    std::fs::File::create(&path)
        .unwrap()
        .set_len(actions::MERGE_LIMIT)
        .unwrap();
    assert_eq!(choices(&lab)["merge"], true);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(actions::MERGE_LIMIT + 1)
        .unwrap();
    assert_eq!(choices(&lab)["merge"], false);
}

#[test]
fn resolve_on_a_path_not_in_conflict_refuses() {
    let (lab, _holder) = conflicted();
    let mut term = Term::default();
    assert_eq!(
        run(&lab, &mut term, &lab.root, &["resolve", "other", "--keep", "server"]),
        1
    );
    assert!(term.said[0].contains("other is not in conflict"));
    lab.engine.push(lab.watching(&lab.tree(), 2));
    assert_eq!(
        run(&lab, &mut term, &lab.root, &["resolve", "file", "--keep", "server"]),
        1
    );
    assert!(lab.root.join("file").exists());
}

#[test]
fn dismiss_clears_copied_back() {
    let (lab, _holder) = conflicted();
    lab.update_record(|r| {
        r.copied_back = vec!["a".into(), "b".into()];
        r.copied_back_more = 2;
    });
    let mut term = Term::default();
    assert_eq!(run(&lab, &mut term, &lab.root, &["dismiss", "a"]), 0);
    assert_eq!(lab.record().copied_back, ["b"]);
    assert_eq!(run(&lab, &mut term, &lab.root, &["dismiss", "--all"]), 0);
    assert!(lab.record().copied_back.is_empty());
    assert_eq!(lab.record().copied_back_more, 0);
}

#[test]
fn cleared_conflict_after_writing_the_server_bytes_is_a_successful_merge() {
    let (lab, _holder) = conflicted();
    let bytes = b"server\0\xff";
    lab.engine.clear_when_equal(lab.root.join("file"), bytes.to_vec());
    let args = ["merge-write", "file", "--server-sha256", &actions::hash(bytes)];
    let mut out = vec![];
    let mut term = Term::default();
    assert_eq!(
        run_io(&lab, &mut term, &lab.root, &args, bytes, &mut out),
        0,
        "{:?}",
        term.said
    );
    assert_eq!(std::fs::read(lab.root.join("file")).unwrap(), bytes);
    let paths = String::from_utf8(out).unwrap();
    for (side, data) in [("laptop", b"laptop\0\xff".as_slice()), ("server", bytes)] {
        let path = paths
            .lines()
            .find(|line| line.contains(&format!("-{side}/file")))
            .unwrap()
            .strip_prefix("Backup: ")
            .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), data);
    }
}

#[test]
fn merge_and_keep_accept_a_missing_side() {
    for missing_server in [true, false] {
        for merge in [true, false] {
            let (lab, _holder) = conflicted();
            let file = lab.root.join("file");
            let (server_sha, laptop_sha, keep) = if missing_server {
                lab.helper.files.lock().unwrap().clear();
                record_conflict(&lab, EntryKind::File, EntryKind::Missing);
                (String::new(), actions::hash(b"laptop\0\xff"), "server")
            } else {
                std::fs::remove_file(&file).unwrap();
                record_conflict(&lab, EntryKind::Missing, EntryKind::File);
                (actions::hash(b"server\0\xff"), String::new(), "laptop")
            };
            let mut term = Term::default();
            if merge {
                let args = [
                    "merge-write",
                    "file",
                    "--server-sha256",
                    &server_sha,
                    "--laptop-sha256",
                    &laptop_sha,
                ];
                assert_eq!(
                    run_io(&lab, &mut term, &lab.root, &args, b"merged", &mut vec![]),
                    0,
                    "{:?}",
                    term.said
                );
                assert_eq!(std::fs::read(file).unwrap(), b"merged");
                assert_eq!(lab.helper.files.lock().unwrap()["file"], b"merged");
            } else {
                assert_eq!(run(&lab, &mut term, &lab.root, &["resolve", "file", "--keep", keep]), 0);
                assert!(!file.exists() && !lab.helper.files.lock().unwrap().contains_key("file"));
            }
            assert_eq!(saved(&lab).len(), 1);
        }
    }
}

#[test]
fn canonical_cwd_and_recorded_root_reach_the_same_conflict_without_following_children() {
    let mut lab = Lab::new();
    let mut project = lab.project();
    let real = lab.root.join("real/project");
    std::fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(real.parent().unwrap(), lab.root.join("alias")).unwrap();
    lab.root = lab.root.join("alias/project");
    project.root = lab.root.clone();
    lab.store.save(&project).unwrap();
    let (lab, _holder) = conflict_fixture(lab);
    lab.helper.list_file("file", 1);
    assert_eq!(choices(&lab)["merge"], true);
    for cwd in [&real, &lab.root] {
        assert_eq!(run(&lab, &mut Term::default(), cwd, &["show-server", "file"]), 0);
    }
    std::fs::remove_file(real.join("file")).unwrap();
    std::os::unix::fs::symlink("elsewhere", real.join("file")).unwrap();
    assert_eq!(
        run(
            &lab,
            &mut Term::default(),
            &real,
            &["resolve", "file", "--keep", "server"]
        ),
        1
    );
}

#[test]
fn resolve_against_a_live_actor_prepares_the_backup_before_waiting_for_the_action_lock() {
    let lab = Lab::new();
    let (_actor, _events) = lab.synced();
    let mut tree = lab.watching(&lab.tree(), 2);
    tree.conflicts.push(Conflict {
        path: "file".into(),
        laptop: EntryKind::File,
        server: EntryKind::File,
    });
    lab.engine.push(tree);
    let dir = actor::project_dir(&lab.state, &lab.project());
    wait(|| status_file::read_status_file(&dir).is_ok_and(|s| !s.conflicts.is_empty()));
    std::fs::write(lab.root.join("file"), b"local").unwrap();
    lab.helper
        .files
        .lock()
        .unwrap()
        .insert("file".into(), b"server".to_vec());
    let lock = actions::action_lock(&actor::lock_dir(&lab.state, &lab.root)).unwrap();
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            run(
                &lab,
                &mut Term::default(),
                &lab.root,
                &["resolve", "file", "--keep", "server"],
            )
        });
        let backups = lab
            .state
            .join("eitri/remote/conflict-backups")
            .join(actor::sync_key(&lab.project()).as_str());
        wait(|| {
            std::fs::read_dir(&backups).is_ok_and(|entries| {
                entries
                    .flatten()
                    .any(|v| std::fs::read(v.path().join("file")).ok().as_deref() == Some(b"local"))
            })
        });
        assert!(lab.root.join("file").exists());
        drop(lock);
        assert_eq!(worker.join().unwrap(), 0);
    });
    assert!(!lab.root.join("file").exists());
}

#[test]
fn resolve_refuses_after_the_action_lock_retry_budget() {
    let (lab, _holder) = conflicted();
    let locks = actor::lock_dir(&lab.state, &lab.root);
    let _busy = actions::action_lock(&locks).unwrap();
    let mut term = Term::default();
    assert_eq!(
        run(&lab, &mut term, &lab.root, &["resolve", "file", "--keep", "server"]),
        1
    );
    assert!(term.said.last().unwrap().ends_with("sync is updating; look again"));
    assert_eq!(std::fs::read(lab.root.join("file")).unwrap(), b"laptop\0\xff");
}

/// A directory below the root swapped for a link: no action follows it, and what it leads to is untouched.
#[test]
fn a_linked_directory_below_the_root_is_never_followed() {
    let (lab, _holder) = conflicted();
    let elsewhere = lab.state.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("file"), b"not the project's").unwrap();
    std::os::unix::fs::symlink(&elsewhere, lab.root.join("sub")).unwrap();
    let project = lab.project();
    let mut tree = lab.watching(&lab.tree(), 1);
    tree.conflicts.push(Conflict {
        path: "sub/file".into(),
        laptop: EntryKind::File,
        server: EntryKind::File,
    });
    lab.engine.push(tree.clone());
    status_file::write_status(
        &actor::project_dir(&lab.state, &project),
        &status_file::compose(&project.host, &lab.record(), Some(&tree), None, None, 1),
    )
    .unwrap();
    for keep in ["server", "laptop"] {
        let mut term = Term::default();
        assert_eq!(
            run(&lab, &mut term, &lab.root, &["resolve", "sub/file", "--keep", keep]),
            1,
            "{:?}",
            term.said
        );
    }
    assert_eq!(std::fs::read(elsewhere.join("file")).unwrap(), b"not the project's");
}

#[test]
fn exclude_and_include_refuse_control_characters() {
    for rel in ["bad\nname", "bad\tname", "bad\u{1b}name"] {
        assert!(checked_rel(rel).is_err(), "{rel:?}");
    }
}

#[test]
fn watching_from_a_terminal_keeps_its_shared_input_blocking() {
    use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: openpty initializes two descriptors for this test's private terminal.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    let (_master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let _output = WatchOutput::new(slave.as_fd(), &|| false).unwrap();
    assert_eq!(
        unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
        0
    );
}

#[test]
fn watching_a_terminal_that_cannot_be_reopened_still_writes_blocking() {
    use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: openpty initializes two descriptors for this test's private terminal.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // SAFETY: geteuid takes no arguments.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root opens a terminal whatever its mode, so the reopen cannot be made to fail this way");
        return;
    }
    // As after `su` or inside a container: the terminal is open here but its name cannot be opened again.
    // SAFETY: fchmod on this test's own terminal.
    assert_eq!(unsafe { libc::fchmod(slave.as_raw_fd(), 0) }, 0);
    let mut output = WatchOutput::new(slave.as_fd(), &|| false).unwrap();
    assert!(output.own_flags.is_none(), "the reopen was expected to fail");
    assert_eq!(
        unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
        0
    );
    output.write_all(b"line\n").unwrap();
    let mut read = [0u8; 16];
    let mut master = std::fs::File::from(master);
    let n = std::io::Read::read(&mut master, &mut read).unwrap();
    assert!(read[..n].starts_with(b"line"), "{:?}", &read[..n]);
}

#[test]
fn watching_into_a_pipe_leaves_the_shared_output_blocking() {
    use std::os::fd::{AsFd, AsRawFd};
    let (mut read, write) = std::io::pipe().unwrap();
    let mut output = WatchOutput::new(write.as_fd(), &|| false).unwrap();
    // The pipe's open description is shared with this process's stdout and with other writers of a pipeline.
    assert_eq!(
        unsafe { libc::fcntl(write.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
        0
    );
    // A write larger than the pipe's atomic size still arrives whole.
    let line = vec![b'x'; 3 * libc::PIPE_BUF + 7];
    std::thread::scope(|scope| {
        let reader = scope.spawn(move || {
            let mut got = vec![0u8; line.len()];
            std::io::Read::read_exact(&mut read, &mut got).unwrap();
            got
        });
        output.write_all(&vec![b'x'; 3 * libc::PIPE_BUF + 7]).unwrap();
        assert_eq!(reader.join().unwrap(), vec![b'x'; 3 * libc::PIPE_BUF + 7]);
    });
}

#[test]
fn cancelled_watch_releases_both_sessions_with_stdin_still_open() {
    let lab = Lab::new();
    let (read, write) = std::io::pipe().unwrap();
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (output_read, mut output_write) = std::io::pipe().unwrap();
    use std::os::fd::AsFd;
    use std::os::fd::AsRawFd;
    let fd = output_write.as_raw_fd();
    // Fill this pipe without a reader, then restore blocking writes before the watch starts.
    unsafe { assert_eq!(libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK), 0) };
    let buf = [0u8; 4096];
    while output_write.write(&buf).is_ok() {}
    while output_write.write(&[0]).is_ok() {}
    unsafe { assert_eq!(libc::fcntl(fd, libc::F_SETFL, 0), 0) };
    let (writing, blocked) = mpsc::channel();
    let (done, result) = mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut reach = FakeReach {
                engine: lab.engine.clone(),
                helper: lab.helper.clone(),
            };
            let mut term = Term {
                cancelled: Some(cancelled.clone()),
                ..Default::default()
            };
            let mut cx = Context {
                store: &lab.store,
                reach: &mut reach,
                term: &mut term,
                remote_dev_enabled: &|| true,
                now: 0,
            };
            let check_cancel = || {
                let _ = writing.send(());
                cancelled.load(std::sync::atomic::Ordering::Acquire)
            };
            let mut writer = WatchOutput::new(output_write.as_fd(), &check_cancel).unwrap();
            done.send(watch(
                &mut cx,
                &lab.root,
                None,
                Box::new(std::io::BufReader::new(read)),
                &mut writer,
            ))
            .unwrap();
        });
        wait(|| lab.engine.status_of(&lab.git()).is_some());
        // The second check observes a write that already found the pipe full.
        for _ in 0..2 {
            blocked.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        cancelled.store(true, std::sync::atomic::Ordering::Release);
        let outcome = result.recv_timeout(Duration::from_secs(10));
        drop(output_read);
        drop(write);
        outcome
            .expect("cancellation must wake the watch with stdin open")
            .unwrap();
    });
    for name in [lab.tree(), lab.git()] {
        assert_eq!(
            lab.engine.status_of(&name).unwrap().state,
            crate::remote_sync::engine::SyncState::Paused
        );
    }
    let lock = agent::private_fs::PrivateLock::open(&actor::lock_dir(&lab.state, &lab.root).join("lock")).unwrap();
    assert!(lock.try_lock().unwrap());
}
