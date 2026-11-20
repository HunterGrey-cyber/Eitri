//! The helper's `Transcripts` service: the account's transcripts and settings files on the server, read-only.
#![allow(dead_code)]

#[path = "../src/audit.rs"]
mod audit;
#[path = "../src/daemon.rs"]
mod daemon;
#[path = "../src/peer.rs"]
mod peer;
#[path = "../src/run_dir.rs"]
mod run_dir;

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use agent::read_deny::{files_to_read, FileRead, ReadRules, SettingsFile};
use agent::transcript::sanitize_cwd_for_claude_projects;
use daemon::login_env::Environment;
use daemon::serve::transcripts::TranscriptsService;
use daemon::transcripts;
use eitri_server_proto::read_deny_settings_event::Event;
use eitri_server_proto::transcripts_server::Transcripts;
use eitri_server_proto::*;
use tokio_stream::StreamExt;
use tonic::Request;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REVISION: &str = env!("EITRI_BUILD_COMMIT");
fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

const OLD: &str = "11111111-1111-4111-8111-111111111111";
const NEW: &str = "22222222-2222-4222-8222-222222222222";
const ROOT: &str = "/srv/project";

/// A login home with an account's config directory in it, and a project, all under the test's own directory.
struct Scratch {
    base: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "transcripts-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("home/.claude-example")).unwrap();
        std::fs::create_dir_all(base.join("project")).unwrap();
        Self {
            base: std::fs::canonicalize(&base).unwrap(),
        }
    }

    fn home(&self) -> PathBuf {
        self.base.join("home")
    }

    /// The config directory the tests name.
    fn config(&self) -> PathBuf {
        self.home().join(".claude-example")
    }

    fn project(&self) -> PathBuf {
        self.base.join("project")
    }

    /// A project reached through a link on the way, as `/home -> /var/home` does: `(spelled, real)`. The real path's
    /// parents include `<base>/var`; the spelled path's do not.
    fn linked_project(&self) -> (PathBuf, PathBuf) {
        let real = self.base.join("var/home/user/project");
        std::fs::create_dir_all(&real).unwrap();
        symlink("var/home", self.base.join("h")).unwrap();
        (self.base.join("h/user/project"), real)
    }

    fn env(&self) -> Environment {
        Environment::from([("HOME".into(), self.home().into_os_string())])
    }

    fn scope(&self, root: &Path) -> TranscriptScope {
        TranscriptScope {
            root: root.to_str().unwrap().into(),
            account: String::new(),
            config_dir: self.config().to_str().unwrap().into(),
        }
    }

    /// The folder the CLI keeps `root`'s transcripts in.
    fn sessions(&self, root: &Path) -> PathBuf {
        let dir = self
            .config()
            .join("projects")
            .join(sanitize_cwd_for_claude_projects(root.to_str().unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn service(&self) -> TranscriptsService {
        TranscriptsService::with_environment(self.env())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn set_mtime(path: &Path, secs: u64) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
        .unwrap();
}

fn title_line(title: &str) -> String {
    format!("{{\"type\":\"ai-title\",\"aiTitle\":\"{title}\"}}\n")
}

async fn all<T>(mut stream: impl tokio_stream::Stream<Item = Result<T, tonic::Status>> + Unpin) -> Vec<T> {
    let mut seen = Vec::new();
    while let Some(item) = stream.next().await {
        seen.push(item.unwrap());
    }
    seen
}

fn request(scratch: &Scratch, root: &Path, id: &str) -> ReadTranscriptRequest {
    ReadTranscriptRequest {
        scope: Some(scratch.scope(root)),
        provider_session_id: id.into(),
    }
}

// ---- the bounds the laptop and the helper share with `agent` --------------------------------------

#[test]
fn the_wire_bounds_are_the_local_readers_own() {
    assert_eq!(SETTINGS_FILE_MAX, agent::read_deny::FILE_MAX);
    assert_eq!(TRANSCRIPT_TAIL_MAX, agent::history::HISTORY_SCAN_MAX_BYTES);
}

// ---- the listing ----------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_names_only_session_files_newest_first_with_titles() {
    let scratch = Scratch::new("list");
    let root = scratch.project();
    let dir = scratch.sessions(&root);
    std::fs::write(
        dir.join(format!("{OLD}.jsonl")),
        format!("{{\"type\":\"user\"}}\n{}", title_line("Fix the bug")),
    )
    .unwrap();
    std::fs::write(dir.join(format!("{NEW}.jsonl")), "{\"type\":\"user\"}\n").unwrap();
    set_mtime(&dir.join(format!("{OLD}.jsonl")), 100);
    set_mtime(&dir.join(format!("{NEW}.jsonl")), 200);
    for skipped in ["agent-a1b2c3d.jsonl", "not-a-uuid.jsonl", "notes.txt"] {
        std::fs::write(dir.join(skipped), "x\n").unwrap();
    }
    // An upper-case name is a UUID the laptop would not take for a session, so it is not offered.
    std::fs::write(dir.join("33333333-3333-4333-8333-33333333333A.jsonl"), "x\n").unwrap();
    // A project with another root has its own folder.
    let other = scratch.base.join("other");
    scratch.sessions(&other);

    let service = scratch.service();
    let answer = service
        .list_transcripts(Request::new(scratch.scope(&root)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(answer.refusal, "");
    assert!(!answer.truncated);
    assert_eq!(answer.config_dir, scratch.config().to_str().unwrap());
    assert_eq!(
        (answer.root.as_str(), answer.root_gone),
        (root.to_str().unwrap(), false)
    );
    assert_eq!(
        answer.entries,
        vec![
            TranscriptEntry {
                provider_session_id: NEW.into(),
                len: 16,
                modified_unix_ms: 200_000,
                ai_title: String::new(),
            },
            TranscriptEntry {
                provider_session_id: OLD.into(),
                len: 16 + title_line("Fix the bug").len() as u64,
                modified_unix_ms: 100_000,
                ai_title: "Fix the bug".into(),
            },
        ]
    );

    let none = service
        .list_transcripts(Request::new(scratch.scope(&other)))
        .await
        .unwrap()
        .into_inner();
    assert!(none.entries.is_empty() && none.refusal.is_empty());
    let never = scratch.base.join("never");
    let unknown = service
        .list_transcripts(Request::new(scratch.scope(&never)))
        .await
        .unwrap()
        .into_inner();
    assert!(unknown.entries.is_empty() && unknown.refusal.is_empty());
    // A folder that is gone is named through the parents that are there, and the answer says so.
    assert_eq!(
        (unknown.root.as_str(), unknown.root_gone),
        (never.to_str().unwrap(), true)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_past_its_bound_says_so() {
    let scratch = Scratch::new("list-bound");
    let root = scratch.project();
    let dir = scratch.sessions(&root);
    for number in 0..=TRANSCRIPTS_LIST_MAX {
        let name = format!("{number:08x}-0000-4000-8000-000000000000.jsonl");
        std::fs::write(dir.join(&name), "x\n").unwrap();
        set_mtime(&dir.join(&name), 1000 + number as u64);
    }
    let answer = scratch
        .service()
        .list_transcripts(Request::new(scratch.scope(&root)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(answer.entries.len(), TRANSCRIPTS_LIST_MAX);
    assert!(answer.truncated);
    // The newest 200 are the ones kept.
    assert!(answer.entries[0]
        .provider_session_id
        .starts_with(&format!("{:08x}", TRANSCRIPTS_LIST_MAX)));
    assert!(answer
        .entries
        .iter()
        .all(|entry| !entry.provider_session_id.starts_with("00000000")));
}

// ---- the account's directory ----------------------------------------------------------------------

#[test]
fn a_given_config_dir_must_be_the_accounts_own_directory() {
    let scratch = Scratch::new("config");
    let env = scratch.env();
    let scope = |config_dir: &str| TranscriptScope {
        root: ROOT.into(),
        account: String::new(),
        config_dir: config_dir.into(),
    };
    let scope_in = |config_dir: &str, root: PathBuf| TranscriptScope {
        root: root.to_str().unwrap().into(),
        account: String::new(),
        config_dir: config_dir.into(),
    };
    let own = scratch.config();
    assert_eq!(
        transcripts::config_dir(&scope(own.to_str().unwrap()), &env).unwrap(),
        own
    );
    std::fs::write(scratch.base.join("file"), "x").unwrap();
    let mut refused = vec![
        "relative/dir".to_owned(),
        format!("{}/../home/.claude-example", scratch.base.display()),
        scratch.base.join("missing").display().to_string(),
        scratch.base.join("file").display().to_string(),
        format!("/{}", "a/".repeat(3000)),
        format!("{}\n", own.display()),
    ];
    // Another account's directory is refused unless this one runs as root.
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        refused.push("/usr".to_owned());
    }
    for config_dir in refused {
        let error = transcripts::config_dir(&scope(&config_dir), &env).unwrap_err();
        assert!(!error.is_empty(), "{config_dir:?}");
    }
    // A trailing, doubled or `.` component names the directory the CLI reads, and the answer names the normal form.
    for spelled in [
        format!("{}/", own.display()),
        format!("{}//.claude-example", scratch.home().display()),
        format!("{}/./.claude-example", scratch.home().display()),
        format!("{}//./.claude-example//", scratch.home().display()),
    ] {
        assert_eq!(
            transcripts::config_dir(&scope(&spelled), &env).unwrap(),
            own,
            "{spelled:?}"
        );
        let listed = transcripts::list(&scope_in(&spelled, scratch.project()), &env);
        assert_eq!(listed.refusal, "", "{spelled:?}");
        assert_eq!(listed.config_dir, own.to_str().unwrap());
    }
    // A spelling that is still wrong after that stays refused: not a directory, not one the account owns.
    let dotted = transcripts::config_dir(&scope(&format!("{}/./file/", scratch.base.display())), &env);
    assert!(dotted.is_err());
    let listed = transcripts::list(&scope_in("relative", scratch.project()), &env);
    assert!(listed.refusal.contains("absolute") && listed.entries.is_empty());
}

#[test]
fn the_directory_follows_the_given_one_or_the_account_then_the_login_environment() {
    let scratch = Scratch::new("order");
    let mut env = scratch.env();
    let scope = |account: &str, config_dir: &str| TranscriptScope {
        root: ROOT.into(),
        account: account.into(),
        config_dir: config_dir.into(),
    };
    // Nothing named, nothing in the environment: the login home's own directory, there or not.
    assert_eq!(
        transcripts::config_dir(&scope("", ""), &env).unwrap(),
        scratch.home().join(".claude")
    );
    // The login environment's variable.
    let variable = scratch.base.join("variable-dir");
    env.insert("CLAUDE_CONFIG_DIR".into(), variable.clone().into_os_string());
    assert_eq!(transcripts::config_dir(&scope("", ""), &env).unwrap(), variable);
    env.insert("CLAUDE_CONFIG_DIR".into(), OsString::from("relative"));
    assert!(transcripts::config_dir(&scope("", ""), &env).is_err());
    env.insert("CLAUDE_CONFIG_DIR".into(), variable.clone().into_os_string());
    // An account, resolved on the login home, beats the variable.
    assert_eq!(
        transcripts::config_dir(&scope("example", ""), &env).unwrap(),
        scratch.config()
    );
    for bad in ["missing", "../escape", " example", "with space"] {
        assert!(transcripts::config_dir(&scope(bad, ""), &env).is_err(), "{bad:?}");
    }
    // A directory the tab names and an account that names another are a refusal that names both.
    let own = scratch.home().join(".claude-other");
    std::fs::create_dir(&own).unwrap();
    let refused = transcripts::config_dir(&scope("example", own.to_str().unwrap()), &env).unwrap_err();
    assert!(
        refused.contains(own.to_str().unwrap())
            && refused.contains(scratch.config().to_str().unwrap())
            && refused.contains("example"),
        "{refused}"
    );
    // The same directory by two spellings is not a mismatch.
    symlink(scratch.home(), scratch.base.join("home-link")).unwrap();
    let respelled = scratch.base.join("home-link/.claude-example");
    assert_eq!(
        transcripts::config_dir(&scope("example", respelled.to_str().unwrap()), &env).unwrap(),
        respelled
    );
    assert_eq!(
        transcripts::config_dir(&scope("example", scratch.config().to_str().unwrap()), &env).unwrap(),
        scratch.config()
    );
    // An account that does not resolve is a refusal even when a directory is given.
    assert!(transcripts::config_dir(&scope("missing", scratch.config().to_str().unwrap()), &env).is_err());
    // A directory the tab names alone is taken without an account.
    assert_eq!(
        transcripts::config_dir(&scope("", own.to_str().unwrap()), &env).unwrap(),
        own
    );
    // No home in the login environment and nothing else to go on.
    assert!(transcripts::config_dir(&scope("", ""), &Environment::new()).is_err());
}

// ---- a transcript's tail --------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tail_of_a_large_transcript_starts_mid_file() {
    let scratch = Scratch::new("tail");
    let root = scratch.project();
    let dir = scratch.sessions(&root);
    let path = dir.join(format!("{NEW}.jsonl"));
    // A sparse file a little past the bound, with known bytes at its end.
    let extra = 3000u64;
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = std::fs::File::create(&path).unwrap();
        file.set_len(TRANSCRIPT_TAIL_MAX + extra).unwrap();
        let end = b"{\"type\":\"last\"}\nabcd\n";
        file.seek(SeekFrom::Start(TRANSCRIPT_TAIL_MAX + extra - end.len() as u64))
            .unwrap();
        file.write_all(end).unwrap();
    }
    let service = scratch.service();
    let chunks = all(service
        .read_transcript_tail(Request::new(request(&scratch, &root, NEW)))
        .await
        .unwrap()
        .into_inner())
    .await;
    assert_eq!(chunks.len(), TRANSCRIPT_TAIL_MAX as usize / TRANSCRIPT_CHUNK);
    assert!(chunks
        .iter()
        .all(|chunk| chunk.refusal.is_empty() && chunk.started_mid_file));
    assert_eq!(chunks.iter().filter(|chunk| chunk.last).count(), 1);
    assert!(chunks.last().unwrap().last);
    assert_eq!(chunks[0].config_dir, scratch.config().to_str().unwrap());
    assert_eq!(
        (chunks[0].root.as_str(), chunks[0].root_gone),
        (root.to_str().unwrap(), false)
    );
    assert!(chunks[1..]
        .iter()
        .all(|chunk| chunk.config_dir.is_empty() && chunk.root.is_empty()));
    assert!(chunks.iter().all(|chunk| chunk.data.len() <= TRANSCRIPT_CHUNK));
    let sent: Vec<u8> = chunks.into_iter().flat_map(|chunk| chunk.data).collect();
    assert_eq!(sent.len() as u64, TRANSCRIPT_TAIL_MAX);
    let whole = std::fs::read(&path).unwrap();
    assert!(
        sent[..] == whole[extra as usize..],
        "the bytes sent are the file's last 32 MiB"
    );
    assert!(sent.ends_with(b"{\"type\":\"last\"}\nabcd\n"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_small_or_empty_transcript_is_one_chunk_from_its_start() {
    let scratch = Scratch::new("tail-small");
    let root = scratch.project();
    let dir = scratch.sessions(&root);
    std::fs::write(dir.join(format!("{OLD}.jsonl")), "one\ntwo\n").unwrap();
    std::fs::write(dir.join(format!("{NEW}.jsonl")), "").unwrap();
    let service = scratch.service();
    for (id, bytes) in [(OLD, &b"one\ntwo\n"[..]), (NEW, &b""[..])] {
        let chunks = all(service
            .read_transcript_tail(Request::new(request(&scratch, &root, id)))
            .await
            .unwrap()
            .into_inner())
        .await;
        assert_eq!(chunks.len(), 1, "{id}");
        assert!(chunks[0].last && !chunks[0].started_mid_file && chunks[0].refusal.is_empty());
        assert_eq!(chunks[0].data, bytes);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_link_in_place_of_a_transcript_refuses() {
    let scratch = Scratch::new("link");
    let root = scratch.project();
    let dir = scratch.sessions(&root);
    let secret = scratch.base.join("secret.txt");
    std::fs::write(&secret, "private\n").unwrap();
    symlink(&secret, dir.join(format!("{NEW}.jsonl"))).unwrap();
    std::fs::create_dir(dir.join(format!("{OLD}.jsonl"))).unwrap();
    let service = scratch.service();
    for id in [NEW, OLD] {
        let chunks = all(service
            .read_transcript_tail(Request::new(request(&scratch, &root, id)))
            .await
            .unwrap()
            .into_inner())
        .await;
        assert_eq!(chunks.len(), 1, "{id}");
        assert!(
            !chunks[0].refusal.is_empty() && chunks[0].data.is_empty() && !chunks[0].last,
            "{id}"
        );
        let stable = service
            .transcript_stable(Request::new(request(&scratch, &root, id)))
            .await
            .unwrap()
            .into_inner();
        assert!(!stable.refusal.is_empty() && !stable.stable, "{id}");
    }
    // The listing does not offer either.
    let listed = service
        .list_transcripts(Request::new(scratch.scope(&root)))
        .await
        .unwrap()
        .into_inner();
    assert!(listed.entries.is_empty());
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "private\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_that_is_not_a_session_of_this_root_refuses() {
    let scratch = Scratch::new("refuse");
    let root = scratch.project();
    scratch.sessions(&root);
    let service = scratch.service();
    let mut bad = vec![
        request(&scratch, &root, "../../settings"),
        request(&scratch, &root, "22222222-2222-4222-8222-22222222222A"),
        request(&scratch, &root, ""),
        request(&scratch, &root, NEW),
    ];
    bad[3].scope.as_mut().unwrap().root = "relative".into();
    bad.push(ReadTranscriptRequest {
        scope: None,
        provider_session_id: NEW.into(),
    });
    for one in bad {
        let chunks = all(service
            .read_transcript_tail(Request::new(one.clone()))
            .await
            .unwrap()
            .into_inner())
        .await;
        assert!(chunks.len() == 1 && !chunks[0].refusal.is_empty(), "{one:?}");
        let stable = service.transcript_stable(Request::new(one)).await.unwrap().into_inner();
        assert!(!stable.refusal.is_empty());
    }
    // A session that is not there is not a refusal of the read: the stream says so.
    let chunks = all(service
        .read_transcript_tail(Request::new(request(&scratch, &root, NEW)))
        .await
        .unwrap()
        .into_inner())
    .await;
    assert!(chunks[0].refusal.contains("no such transcript"));
}

// ---- stability -------------------------------------------------------------------------------------

#[test]
fn stable_and_moving_and_missing() {
    let scratch = Scratch::new("stable");
    let path = scratch.base.join("t.jsonl");
    assert_eq!(
        transcripts::stable(&path, Duration::from_millis(10)).unwrap(),
        transcripts::Stability::Missing
    );
    std::fs::write(&path, "x\n").unwrap();
    set_mtime(&path, 500);
    assert_eq!(
        transcripts::stable(&path, transcripts::STABLE_PROBE).unwrap(),
        transcripts::Stability::Stable
    );
    // Another process keeps writing: its modification time moves between the probe's two reads.
    let moving = path.clone();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopped = stop.clone();
    let writer = std::thread::spawn(move || {
        let mut secs = 1000;
        while !stopped.load(Ordering::Relaxed) {
            set_mtime(&moving, secs);
            secs += 1;
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    let found = transcripts::stable(&path, transcripts::STABLE_PROBE);
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    assert_eq!(found.unwrap(), transcripts::Stability::Moving);
    // A directory or a link is not a transcript.
    let directory = scratch.base.join("d.jsonl");
    std::fs::create_dir(&directory).unwrap();
    assert!(transcripts::stable(&directory, Duration::from_millis(10)).is_err());
    let link = scratch.base.join("l.jsonl");
    symlink(&path, &link).unwrap();
    assert!(transcripts::stable(&link, Duration::from_millis(10)).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_stability_probe_answers_through_the_service() {
    let scratch = Scratch::new("stable-service");
    let root = scratch.project();
    let dir = scratch.sessions(&root);
    std::fs::write(dir.join(format!("{NEW}.jsonl")), "x\n").unwrap();
    set_mtime(&dir.join(format!("{NEW}.jsonl")), 500);
    let service = scratch.service();
    let stable = service
        .transcript_stable(Request::new(request(&scratch, &root, NEW)))
        .await
        .unwrap()
        .into_inner();
    assert!(stable.stable && !stable.missing && stable.refusal.is_empty());
    assert_eq!(stable.config_dir, scratch.config().to_str().unwrap());
    assert_eq!(
        (stable.root.as_str(), stable.root_gone),
        (root.to_str().unwrap(), false)
    );
    let missing = service
        .transcript_stable(Request::new(request(&scratch, &root, OLD)))
        .await
        .unwrap()
        .into_inner();
    assert!(!missing.stable && missing.missing && missing.refusal.is_empty());
}

// ---- the settings files ---------------------------------------------------------------------------

async fn settings_events(service: &TranscriptsService, scope: TranscriptScope) -> Vec<ReadDenySettingsEvent> {
    all(service
        .read_deny_settings(Request::new(scope))
        .await
        .unwrap()
        .into_inner())
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_reads_follow_files_to_read_on_the_server() {
    let scratch = Scratch::new("settings");
    let root = scratch.project();
    std::fs::create_dir_all(root.join(".claude")).unwrap();
    std::fs::write(
        root.join(".claude/settings.local.json"),
        r#"{"permissions":{"deny":["Read(./a.env)"]}}"#,
    )
    .unwrap();
    std::fs::write(
        scratch.config().join("settings.json"),
        r#"{"permissions":{"deny":["Read(~/.ssh/**)"]}}"#,
    )
    .unwrap();
    // A file that is there and cannot be read as a file.
    std::fs::create_dir(root.join(".claude/settings.json")).unwrap();
    let scope = scratch.scope(&root);
    let events = settings_events(&scratch.service(), scope.clone()).await;
    let Some(Event::Scope(head)) = events[0].event.clone() else {
        panic!("the first event is the scope: {:?}", events[0]);
    };
    assert_eq!(head.home, scratch.home().to_str().unwrap());
    assert_eq!(head.config_dir, scratch.config().to_str().unwrap());
    assert_eq!((head.root.as_str(), head.root_gone), (root.to_str().unwrap(), false));
    let files: Vec<SettingsRead> = events[1..]
        .iter()
        .map(|event| match event.event.clone() {
            Some(Event::File(file)) => file,
            other => panic!("expected a file: {other:?}"),
        })
        .collect();
    assert_eq!(head.files as usize, files.len());
    let expected = files_to_read(&root, Some(&scratch.config()));
    assert_eq!(
        files
            .iter()
            .map(|file| (file.path.as_str(), file.strict))
            .collect::<Vec<_>>(),
        expected
            .iter()
            .map(|file| (file.path.to_str().unwrap(), file.strict))
            .collect::<Vec<_>>()
    );
    let by_path = |name: &str| files.iter().find(|file| file.path.ends_with(name)).unwrap();
    assert!(by_path("project/.claude/settings.local.json")
        .bytes
        .starts_with(b"{\"permissions\""));
    assert!(by_path(".claude-example/settings.json")
        .bytes
        .starts_with(b"{\"permissions\""));
    assert!(by_path(".claude-example/remote-settings.json").missing);
    let unreadable = by_path("project/.claude/settings.json");
    assert!(!unreadable.failed.is_empty() && unreadable.bytes.is_empty() && !unreadable.missing);
    // Built from what was sent, the rules are the ones the server's own files give; the unreadable file closes the
    // reader there as it does here.
    let reads: Vec<(SettingsFile, FileRead)> = files
        .iter()
        .map(|file| {
            let read = if file.missing {
                FileRead::Missing
            } else if !file.failed.is_empty() {
                FileRead::Failed(file.failed.clone())
            } else {
                FileRead::Bytes(file.bytes.clone())
            };
            (
                SettingsFile {
                    path: PathBuf::from(&file.path),
                    strict: file.strict,
                },
                read,
            )
        })
        .collect();
    let home = PathBuf::from(&head.home);
    let sent = ReadRules::from_reads(&root, Some(&home), true, reads);
    let local = ReadRules::load(&root, Some(&scratch.config()), Some(&home));
    assert_eq!(sent, local);
    assert!(sent.closed().is_some());
    // The server's `~` is the login home.
    assert!(sent.denies(&root, &scratch.home().join(".ssh/id_ed25519")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_project_with_too_many_settings_files_is_refused_not_cut() {
    let scratch = Scratch::new("settings-deep");
    let deep = format!("/{}", vec!["d"; 40].join("/"));
    let events = settings_events(&scratch.service(), scratch.scope(Path::new(&deep))).await;
    assert_eq!(events.len(), 1);
    let Some(Event::Refusal(why)) = events[0].event.clone() else {
        panic!("a refusal, not a list: {:?}", events[0]);
    };
    assert!(why.contains(&SETTINGS_FILES_MAX.to_string()), "{why}");
    // A root that is not one, and an account that is not there, are refusals too.
    for scope in [
        TranscriptScope {
            root: "relative".into(),
            ..scratch.scope(&scratch.project())
        },
        TranscriptScope {
            config_dir: String::new(),
            account: "missing".into(),
            ..scratch.scope(&scratch.project())
        },
    ] {
        let events = settings_events(&scratch.service(), scope).await;
        assert!(matches!(
            events[..],
            [ReadDenySettingsEvent {
                event: Some(Event::Refusal(_))
            }]
        ));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settings_file_past_its_bound_is_sent_as_failed_not_cut() {
    let scratch = Scratch::new("settings-big");
    let root = scratch.project();
    std::fs::create_dir_all(root.join(".claude")).unwrap();
    let mut big = br#"{"permissions":{"deny":[]}}"#.to_vec();
    big.resize(SETTINGS_FILE_MAX as usize + 1, b' ');
    std::fs::write(root.join(".claude/settings.json"), &big).unwrap();
    let events = settings_events(&scratch.service(), scratch.scope(&root)).await;
    let failed: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            Some(Event::File(file)) if file.path.ends_with("project/.claude/settings.json") => Some(file.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(failed.len(), 1);
    assert!(failed[0].bytes.is_empty() && failed[0].failed.contains("4 MiB"));
}

// ---- nothing under the config directory changes ---------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_under_the_config_directory_is_written() {
    let scratch = Scratch::new("readonly");
    let root = scratch.project();
    let dir = scratch.sessions(&root);
    std::fs::write(dir.join(format!("{NEW}.jsonl")), format!("a\n{}", title_line("t"))).unwrap();
    std::fs::write(scratch.config().join("settings.json"), "{}").unwrap();
    let snapshot = |base: &Path| {
        let mut seen = Vec::new();
        let mut pending = vec![base.to_path_buf()];
        while let Some(path) = pending.pop() {
            let meta = std::fs::symlink_metadata(&path).unwrap();
            seen.push((path.clone(), meta.len(), meta.modified().unwrap()));
            if meta.is_dir() {
                pending.extend(std::fs::read_dir(&path).unwrap().map(|entry| entry.unwrap().path()));
            }
        }
        seen.sort();
        seen
    };
    let before = snapshot(&scratch.config());
    let service = scratch.service();
    let _ = service
        .list_transcripts(Request::new(scratch.scope(&root)))
        .await
        .unwrap();
    let _ = all(service
        .read_transcript_tail(Request::new(request(&scratch, &root, NEW)))
        .await
        .unwrap()
        .into_inner())
    .await;
    let _ = service
        .transcript_stable(Request::new(request(&scratch, &root, NEW)))
        .await
        .unwrap();
    let _ = settings_events(&service, scratch.scope(&root)).await;
    // A root that is gone and one reached through a link read the same way: no folder is made for either.
    let gone_path = scratch.base.join("never");
    let gone = gone_path.as_path();
    let _ = service
        .list_transcripts(Request::new(scratch.scope(gone)))
        .await
        .unwrap();
    let _ = all(service
        .read_transcript_tail(Request::new(request(&scratch, gone, NEW)))
        .await
        .unwrap()
        .into_inner())
    .await;
    let _ = settings_events(&service, scratch.scope(gone)).await;
    let (spelled, _) = scratch.linked_project();
    let _ = service
        .list_transcripts(Request::new(scratch.scope(&spelled)))
        .await
        .unwrap();
    assert_eq!(snapshot(&scratch.config()), before);
}

// ---- the root, resolved as a sidecar start records it -----------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_root_reached_through_a_link_is_the_real_folder_for_every_call() {
    let scratch = Scratch::new("linked");
    let (spelled, real) = scratch.linked_project();
    // The CLI records a session under the folder the kernel names; the link's own key holds a decoy.
    let dir = scratch.sessions(&real);
    std::fs::write(dir.join(format!("{NEW}.jsonl")), title_line("real")).unwrap();
    let decoy = scratch.sessions(&spelled);
    std::fs::write(decoy.join(format!("{OLD}.jsonl")), title_line("decoy")).unwrap();
    // A deny rule in a folder above the real path only: the spelled path's own parents do not hold it.
    std::fs::create_dir_all(scratch.base.join("var/.claude")).unwrap();
    std::fs::write(
        scratch.base.join("var/.claude/settings.json"),
        r#"{"permissions":{"deny":["Read(./secret/**)"]}}"#,
    )
    .unwrap();

    let service = scratch.service();
    let real_text = real.to_str().unwrap();
    let listed = service
        .list_transcripts(Request::new(scratch.scope(&spelled)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.refusal, "");
    assert_eq!((listed.root.as_str(), listed.root_gone), (real_text, false));
    assert_eq!(
        listed
            .entries
            .iter()
            .map(|entry| (entry.provider_session_id.as_str(), entry.ai_title.as_str()))
            .collect::<Vec<_>>(),
        vec![(NEW, "real")]
    );

    let chunks = all(service
        .read_transcript_tail(Request::new(request(&scratch, &spelled, NEW)))
        .await
        .unwrap()
        .into_inner())
    .await;
    assert!(chunks[0].refusal.is_empty() && chunks[0].data.starts_with(b"{\"type\":\"ai-title\""));
    assert_eq!(chunks[0].root, real_text);
    let stable = service
        .transcript_stable(Request::new(request(&scratch, &spelled, NEW)))
        .await
        .unwrap()
        .into_inner();
    assert!(stable.stable && stable.root == real_text);
    // The decoy's session is not this project's.
    let other = service
        .transcript_stable(Request::new(request(&scratch, &spelled, OLD)))
        .await
        .unwrap()
        .into_inner();
    assert!(other.missing);

    let events = settings_events(&service, scratch.scope(&spelled)).await;
    let Some(Event::Scope(head)) = events[0].event.clone() else {
        panic!("the first event is the scope: {:?}", events[0]);
    };
    assert_eq!((head.root.as_str(), head.root_gone), (real_text, false));
    let paths: Vec<String> = events[1..]
        .iter()
        .filter_map(|event| match &event.event {
            Some(Event::File(file)) => Some(file.path.clone()),
            _ => None,
        })
        .collect();
    let above = scratch.base.join("var/.claude/settings.json");
    let bytes = events[1..]
        .iter()
        .find_map(|event| match &event.event {
            Some(Event::File(file)) if file.path == above.to_str().unwrap() => Some(file.bytes.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the real parents' settings are read: {paths:?}"));
    assert!(bytes.starts_with(b"{\"permissions\""));
    let through_link = format!("{}/h/", scratch.base.display());
    assert!(
        paths.iter().all(|path| !path.contains(&through_link)),
        "no path goes through the link: {paths:?}"
    );
}

#[test]
fn a_root_is_resolved_as_a_sidecar_start_resolves_it() {
    let scratch = Scratch::new("root");
    let (spelled, real) = scratch.linked_project();
    let found = transcripts::check_root(spelled.to_str().unwrap()).unwrap();
    assert_eq!((found.text.as_str(), found.gone), (real.to_str().unwrap(), false));
    // A folder that is not there is as spelled under the parents that are; so is one under a missing parent.
    for gone in [scratch.base.join("project-gone"), scratch.base.join("a/b/c")] {
        let found = transcripts::check_root(gone.to_str().unwrap()).unwrap();
        assert_eq!((found.text.as_str(), found.gone), (gone.to_str().unwrap(), true));
    }
    // The folder's own parents are the ones the answer names: a missing name under a link is under the real folder,
    // and so is one under the root of the file system.
    let through = scratch.base.join("h/user/never");
    let found = transcripts::check_root(through.to_str().unwrap()).unwrap();
    assert_eq!(
        (found.text.as_str(), found.gone),
        (real.parent().unwrap().join("never").to_str().unwrap(), true)
    );
    let top = transcripts::check_root("/eitri-test-root-never-exists-0").unwrap();
    assert_eq!((top.text.as_str(), top.gone), ("/eitri-test-root-never-exists-0", true));
    // A link that leads nowhere is a folder that is not there, named by the link's own name.
    symlink("nowhere", scratch.base.join("dangling")).unwrap();
    symlink("nowhere/deeper", scratch.base.join("dangling-deep")).unwrap();
    for name in ["dangling", "dangling-deep"] {
        let root = scratch.base.join(name);
        let found = transcripts::check_root(root.to_str().unwrap()).unwrap();
        assert_eq!((found.text.as_str(), found.gone), (root.to_str().unwrap(), true));
    }
    let under = scratch.base.join("h/user/dangling");
    symlink("nowhere", real.parent().unwrap().join("dangling")).unwrap();
    let found = transcripts::check_root(under.to_str().unwrap()).unwrap();
    assert_eq!(
        (found.text.as_str(), found.gone),
        (real.parent().unwrap().join("dangling").to_str().unwrap(), true)
    );
    std::fs::write(scratch.base.join("file"), "x").unwrap();
    let mut refused = vec![
        "".to_owned(),
        "relative/dir".to_owned(),
        format!("{}//never", scratch.base.display()),
        format!("{}/never/", scratch.base.display()),
        format!("{}/./never", scratch.base.display()),
        format!("{}/../never", scratch.base.display()),
        format!("{}/pro\nject", scratch.base.display()),
        format!("{}/pro\0ject", scratch.base.display()),
        format!("/{}", "a".repeat(4096)),
        // A file, or a path through one, is not a folder that is gone.
        scratch.base.join("file").display().to_string(),
        scratch.base.join("file/inside").display().to_string(),
    ];
    // Another account's folder is refused unless this one runs as root.
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        refused.push("/usr".to_owned());
    }
    for root in refused {
        assert!(transcripts::check_root(&root).is_err(), "{root:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gone_root_through_a_link_has_the_real_parents_for_every_call() {
    let scratch = Scratch::new("gone-linked");
    let (_, real) = scratch.linked_project();
    // The project's folder is gone, its parent is reached through a link: the CLI recorded its sessions under the
    // real parent's path, and the real folders above it hold the settings that apply.
    let spelled = scratch.base.join("h/user/removed");
    let named = real.parent().unwrap().join("removed");
    let dir = scratch.sessions(&named);
    std::fs::write(dir.join(format!("{NEW}.jsonl")), title_line("kept")).unwrap();
    std::fs::create_dir_all(scratch.base.join("var/.claude")).unwrap();
    std::fs::write(
        scratch.base.join("var/.claude/settings.json"),
        r#"{"permissions":{"deny":["Read(./secret/**)"]}}"#,
    )
    .unwrap();

    let service = scratch.service();
    let listed = service
        .list_transcripts(Request::new(scratch.scope(&spelled)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.refusal, "");
    assert_eq!(
        (listed.root.as_str(), listed.root_gone),
        (named.to_str().unwrap(), true)
    );
    assert_eq!(listed.entries.len(), 1);
    assert_eq!(listed.entries[0].ai_title, "kept");
    let chunks = all(service
        .read_transcript_tail(Request::new(request(&scratch, &spelled, NEW)))
        .await
        .unwrap()
        .into_inner())
    .await;
    assert_eq!(
        (chunks[0].root.as_str(), chunks[0].root_gone),
        (named.to_str().unwrap(), true)
    );
    let events = settings_events(&service, scratch.scope(&spelled)).await;
    let Some(Event::Scope(head)) = events[0].event.clone() else {
        panic!("the first event is the scope: {:?}", events[0]);
    };
    assert_eq!((head.root.as_str(), head.root_gone), (named.to_str().unwrap(), true));
    let above = scratch.base.join("var/.claude/settings.json");
    let paths: Vec<String> = events[1..]
        .iter()
        .filter_map(|event| match &event.event {
            Some(Event::File(file)) => Some(file.path.clone()),
            _ => None,
        })
        .collect();
    assert!(
        paths.iter().any(|path| path == above.to_str().unwrap()),
        "the real parents' settings are read: {paths:?}"
    );
    let through_link = format!("{}/h/", scratch.base.display());
    assert!(paths.iter().all(|path| !path.contains(&through_link)), "{paths:?}");
}

#[test]
fn a_held_root_is_the_folder_that_was_checked() {
    let scratch = Scratch::new("held");
    let project = scratch.project();
    let held = transcripts::check_root(project.to_str().unwrap()).unwrap();
    assert!(held.is_held());
    // A folder renamed away and another made in its place is not the folder that was checked.
    std::fs::rename(&project, scratch.base.join("moved")).unwrap();
    assert!(!held.is_held());
    std::fs::create_dir(&project).unwrap();
    assert!(!held.is_held());
    assert!(transcripts::check_root(project.to_str().unwrap()).unwrap().is_held());

    // A folder that is gone is held by the parent that is there.
    let gone = project.join("removed");
    let held = transcripts::check_root(gone.to_str().unwrap()).unwrap();
    assert!(held.gone && held.is_held());
    std::fs::rename(&project, scratch.base.join("moved-again")).unwrap();
    std::fs::create_dir(&project).unwrap();
    assert!(!held.is_held());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_root_at_the_byte_bound_is_answered_within_the_path_bound() {
    let scratch = Scratch::new("long-root");
    // Fifteen 255-byte names and a shorter one: a gone root of 4090 bytes, as long as a path can be statted.
    let mut root = String::new();
    for _ in 0..15 {
        root.push('/');
        root.push_str(&"a".repeat(255));
    }
    root.push('/');
    root.push_str(&"b".repeat(4090 - root.len()));
    assert_eq!(root.len(), 4090);
    let events = settings_events(&scratch.service(), scratch.scope(Path::new(&root))).await;
    let Some(Event::Scope(head)) = events[0].event.clone() else {
        panic!("the first event is the scope: {:?}", events[0]);
    };
    assert_eq!((head.root.len(), head.root_gone), (4090, true));
    assert!(events.len() > 1);
    for event in &events[1..] {
        let Some(Event::File(file)) = &event.event else {
            panic!("a file: {event:?}");
        };
        assert!(file.path.len() <= TRANSCRIPT_PATH_MAX, "{}", file.path.len());
    }
    let longer = format!("/{}", "a".repeat(4096));
    let events = settings_events(&scratch.service(), scratch.scope(Path::new(&longer))).await;
    assert!(matches!(
        events[..],
        [ReadDenySettingsEvent {
            event: Some(Event::Refusal(_))
        }]
    ));
}

#[test]
fn a_path_past_the_bound_is_not_sent() {
    let long = PathBuf::from(format!("/{}", "a".repeat(TRANSCRIPT_PATH_MAX)));
    assert!(transcripts::checked_path(&long).unwrap_err().contains("exceeds"));
    let at = PathBuf::from(format!("/{}", "a".repeat(TRANSCRIPT_PATH_MAX - 1)));
    assert_eq!(transcripts::checked_path(&at).unwrap().len(), TRANSCRIPT_PATH_MAX);
    use std::os::unix::ffi::OsStrExt;
    assert!(
        transcripts::checked_path(Path::new(std::ffi::OsStr::from_bytes(b"/bad\xffname")))
            .unwrap_err()
            .contains("UTF-8")
    );
}

#[test]
fn the_login_environments_directories_are_read_leniently_and_the_laptops_strictly() {
    let scratch = Scratch::new("env");
    let mut env = scratch.env();
    let scope = TranscriptScope {
        root: ROOT.into(),
        ..Default::default()
    };
    let variable = scratch.base.join("variable-dir");
    // A trailing, doubled or `.` component names the directory the CLI reads.
    for spelled in [
        format!("{}/", variable.display()),
        format!("{}//", variable.display()),
        format!("{}/./", variable.display()),
        format!("{}/./variable-dir", scratch.base.display()),
    ] {
        env.insert("CLAUDE_CONFIG_DIR".into(), OsString::from(&spelled));
        assert_eq!(transcripts::config_dir(&scope, &env).unwrap(), variable, "{spelled:?}");
    }
    for refused in [
        "relative".to_owned(),
        format!("{}/../x", scratch.base.display()),
        format!("{}\n", variable.display()),
        format!("/{}", "a".repeat(4096)),
    ] {
        env.insert("CLAUDE_CONFIG_DIR".into(), OsString::from(&refused));
        assert!(transcripts::config_dir(&scope, &env).is_err(), "{refused:?}");
    }
    env.remove(OsStr::new("CLAUDE_CONFIG_DIR"));
    // The home is normalised the same way, and what an answer names is the normal form.
    env.insert("HOME".into(), OsString::from(format!("{}//", scratch.home().display())));
    assert_eq!(transcripts::home_of(&env).unwrap(), scratch.home());
    assert_eq!(
        transcripts::config_dir(&scope, &env).unwrap(),
        scratch.home().join(".claude")
    );
    let account = TranscriptScope {
        account: "example".into(),
        ..scope.clone()
    };
    assert_eq!(transcripts::config_dir(&account, &env).unwrap(), scratch.config());
    env.insert("HOME".into(), OsString::from(format!("/{}", "a".repeat(4096))));
    assert!(transcripts::home_of(&env).is_err());
}

// ---- the listing's bound ---------------------------------------------------------------------------

fn session_name(number: usize) -> String {
    format!("{number:08x}-0000-4000-8000-000000000000")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn names_the_laptop_would_not_take_do_not_count_against_the_listing_bound() {
    let scratch = Scratch::new("list-names");
    let root = scratch.project();
    let dir = scratch.sessions(&root);
    // Exactly the bound of sessions, and more files that are not sessions, all newer: the sessions are all listed and
    // the listing is not marked cut.
    for number in 0..TRANSCRIPTS_LIST_MAX {
        let name = format!("{}.jsonl", session_name(number));
        std::fs::write(dir.join(&name), "x\n").unwrap();
        set_mtime(&dir.join(&name), 1000 + number as u64);
    }
    for number in 0..TRANSCRIPTS_LIST_MAX + 50 {
        let upper = format!("{number:08x}-0000-4000-8000-00000000000A.jsonl");
        let braced = format!("{{{}}}.jsonl", session_name(number));
        let agent = format!("agent-{number:07x}.jsonl");
        for bad in [upper, braced, agent] {
            std::fs::write(dir.join(&bad), "x\n").unwrap();
            set_mtime(&dir.join(&bad), 5000);
        }
    }
    let answer = scratch
        .service()
        .list_transcripts(Request::new(scratch.scope(&root)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(answer.entries.len(), TRANSCRIPTS_LIST_MAX);
    assert!(!answer.truncated, "no session was left out");
    assert!(answer
        .entries
        .iter()
        .all(|entry| transcripts::is_session_id(&entry.provider_session_id)));

    // One more session makes it exact the other way.
    let name = format!("{}.jsonl", session_name(TRANSCRIPTS_LIST_MAX));
    std::fs::write(dir.join(&name), "x\n").unwrap();
    set_mtime(&dir.join(&name), 1);
    let answer = scratch
        .service()
        .list_transcripts(Request::new(scratch.scope(&root)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(answer.entries.len(), TRANSCRIPTS_LIST_MAX);
    assert!(answer.truncated);
}

// ---- the title -------------------------------------------------------------------------------------

#[test]
fn a_title_and_a_refusal_are_cut_on_a_character_boundary() {
    // Three bytes a character: a cut at 1024 would fall inside one.
    let long = "\u{20ac}".repeat(2000);
    let refusal = transcripts::bounded(long.clone());
    assert_eq!(refusal.len(), 1023);
    assert!(long.starts_with(&refusal));
    assert_eq!(transcripts::bounded("short"), "short");
    assert_eq!(transcripts::cut_to("a\u{20ac}".into(), 2), "a");
    assert_eq!(transcripts::cut_to("abc".into(), 3), "abc");
    assert_eq!(transcripts::cut_to("abc".into(), 0), "");
    // A title is what the local reader's normaliser makes of the CLI's newest (80 characters at most), far under the
    // bound the answer keeps as its own.
    let scratch = Scratch::new("title");
    let path = scratch.base.join("t.jsonl");
    std::fs::write(&path, title_line(&"x".repeat(500))).unwrap();
    assert_eq!(transcripts::title_of(&path).chars().count(), 80);
}

#[test]
fn a_title_is_not_read_through_a_link_or_from_a_pipe() {
    let scratch = Scratch::new("title-open");
    let real = scratch.base.join("real.jsonl");
    std::fs::write(&real, title_line("a title")).unwrap();
    assert_eq!(transcripts::title_of(&real), "a title");
    let link = scratch.base.join("link.jsonl");
    symlink(&real, &link).unwrap();
    assert_eq!(transcripts::title_of(&link), "");
    assert_eq!(transcripts::title_of(&scratch.base), "");
    assert_eq!(transcripts::title_of(&scratch.base.join("missing.jsonl")), "");

    // A pipe in a transcript's place must not hold the thread: the open would wait for a writer.
    let fifo = scratch.base.join("fifo.jsonl");
    let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: `name` is a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let (send, receive) = std::sync::mpsc::channel();
    let probed = fifo.clone();
    std::thread::spawn(move || {
        let _ = send.send(transcripts::title_of(&probed));
    });
    let answered = receive.recv_timeout(Duration::from_secs(10));
    if answered.is_err() {
        // Release the stuck open so the test process can end, then fail.
        let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
    }
    assert_eq!(answered.expect("a pipe held the title read"), "");
}

// ---- refusals and the deadline ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_login_environment_that_is_unavailable_is_a_refusal_within_the_bound() {
    let scratch = Scratch::new("unavailable");
    let root = scratch.project();
    let service = TranscriptsService::without_environment(&"\u{20ac}".repeat(2000));
    let listed = service
        .list_transcripts(Request::new(scratch.scope(&root)))
        .await
        .unwrap()
        .into_inner();
    assert!(listed.refusal.starts_with("the login environment is unavailable"));
    assert!(
        listed.refusal.len() <= TRANSCRIPT_REFUSAL_MAX,
        "{}",
        listed.refusal.len()
    );
    let stable = service
        .transcript_stable(Request::new(request(&scratch, &root, NEW)))
        .await
        .unwrap()
        .into_inner();
    assert!(stable.refusal.starts_with("the login environment is unavailable"));
    assert!(stable.refusal.len() <= TRANSCRIPT_REFUSAL_MAX);
    let chunks = all(service
        .read_transcript_tail(Request::new(request(&scratch, &root, NEW)))
        .await
        .unwrap()
        .into_inner())
    .await;
    assert!(chunks.len() == 1 && chunks[0].refusal.len() <= TRANSCRIPT_REFUSAL_MAX && !chunks[0].refusal.is_empty());
    let events = settings_events(&service, scratch.scope(&root)).await;
    assert!(matches!(
        &events[..],
        [ReadDenySettingsEvent { event: Some(Event::Refusal(why)) }]
            if !why.is_empty() && why.len() <= TRANSCRIPT_REFUSAL_MAX
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_that_runs_past_its_deadline_ends_in_a_refusal() {
    let scratch = Scratch::new("deadline");
    let root = scratch.project();
    let dir = scratch.sessions(&root);
    // Two chunks, so one has to be sent before the last.
    let path = dir.join(format!("{NEW}.jsonl"));
    std::fs::File::create(&path)
        .unwrap()
        .set_len(2 * TRANSCRIPT_CHUNK as u64)
        .unwrap();
    let service = scratch.service().with_deadline(Duration::ZERO);
    let chunks = all(service
        .read_transcript_tail(Request::new(request(&scratch, &root, NEW)))
        .await
        .unwrap()
        .into_inner())
    .await;
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].refusal.contains("longer than 0 s") && !chunks[0].last && chunks[0].data.is_empty());
    let events = settings_events(&service, scratch.scope(&root)).await;
    assert!(
        matches!(
            &events[..],
            [ReadDenySettingsEvent { event: Some(Event::Refusal(why)) }] if why.contains("longer than 0 s")
        ),
        "{events:?}"
    );
}

// ---- the settings files' total bound ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_files_past_their_total_are_sent_as_failed_not_cut() {
    let scratch = Scratch::new("settings-total");
    let root = scratch.project();
    std::fs::create_dir_all(root.join(".claude")).unwrap();
    // Each file is within its own bound; five of them are 20 MiB, past the 16 MiB the answer carries.
    let mut file = br#"{"permissions":{"deny":[]}}"#.to_vec();
    file.resize(SETTINGS_FILE_MAX as usize, b' ');
    let each = [
        scratch.config().join("settings.json"),
        scratch.config().join("settings.local.json"),
        scratch.config().join("remote-settings.json"),
        root.join(".claude/settings.json"),
        root.join(".claude/settings.local.json"),
    ];
    for path in &each {
        std::fs::write(path, &file).unwrap();
    }
    let events = settings_events(&scratch.service(), scratch.scope(&root)).await;
    let files: Vec<SettingsRead> = events[1..]
        .iter()
        .map(|event| match event.event.clone() {
            Some(Event::File(file)) => file,
            other => panic!("expected a file: {other:?}"),
        })
        .collect();
    let sent: Vec<&SettingsRead> = files.iter().filter(|read| !read.bytes.is_empty()).collect();
    assert_eq!(
        sent.len(),
        (SETTINGS_TOTAL_MAX / SETTINGS_FILE_MAX) as usize,
        "four fit"
    );
    let failed: Vec<&SettingsRead> = files.iter().filter(|read| read.failed.contains("together")).collect();
    assert_eq!(failed.len(), 1, "the fifth does not");
    assert!(failed[0].bytes.is_empty() && failed[0].failed.contains("16 MiB"));
    let Some(Event::Scope(head)) = events[0].event.clone() else {
        panic!("the first event is the scope");
    };
    assert_eq!(head.files as usize, files.len(), "the list is whole");
}
