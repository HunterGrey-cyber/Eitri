//! A remote tab's chooser, resume and rebuild from the server's session transcripts, against a scripted in-process
//! `Transcripts` service: no ssh, no display, no real `claude`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use agent::remote_client::terminal_transport::{tokio_stream, tonic};
use agent::remote_client::{
    self as wire, HelperClient, ListTranscriptsResponse, ReadDenySettingsEvent, ReadTranscriptRequest, TranscriptChunk,
    TranscriptEntry, TranscriptScope, TranscriptStableResponse,
};
use agent::{ConversationError, GateRoot};
use eitri_core::agent_backend::{AgentId, BackendGreeting, BackendKind, Titles};
use eitri_core::remote_project::resume::{
    rebuilt_note, rebuilt_note_if_read_from_the_transcript, spawn_listing, ServerSessions, SessionListing,
};
use eitri_core::remote_project::{RemoteConnection, RemoteProject};

type Items<T> = std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<T, tonic::Status>> + Send>>;

const SESSION: &str = "0b7a7d3e-5f0c-4c52-9a0e-3a1f0c6d2b11";
const OTHER: &str = "5c1f4a2e-8d3b-4e07-9b6a-2f7d1c0e9a44";
const CONFIG: &str = "/home/example/.claude";
const ROOT: &str = "/srv/project";

/// What the scripted helper answers, and what it was asked.
#[derive(Default)]
struct Script {
    list: ListTranscriptsResponse,
    tail: Vec<TranscriptChunk>,
    stable: TranscriptStableResponse,
    probes: AtomicUsize,
    tails: AtomicUsize,
}

struct Scripted(Arc<Script>);

fn stream<T: Clone + Send + 'static>(items: &[T]) -> tonic::Response<Items<T>> {
    let items: Vec<Result<T, tonic::Status>> = items.iter().cloned().map(Ok).collect();
    tonic::Response::new(Box::pin(tokio_stream::iter(items)))
}

#[tonic::async_trait]
impl wire::terminal_transport::wire::transcripts_server::Transcripts for Scripted {
    async fn list_transcripts(
        &self,
        _: tonic::Request<TranscriptScope>,
    ) -> Result<tonic::Response<ListTranscriptsResponse>, tonic::Status> {
        Ok(tonic::Response::new(self.0.list.clone()))
    }

    type ReadTranscriptTailStream = Items<TranscriptChunk>;
    async fn read_transcript_tail(
        &self,
        _: tonic::Request<ReadTranscriptRequest>,
    ) -> Result<tonic::Response<Self::ReadTranscriptTailStream>, tonic::Status> {
        self.0.tails.fetch_add(1, Ordering::SeqCst);
        Ok(stream(&self.0.tail))
    }

    async fn transcript_stable(
        &self,
        _: tonic::Request<ReadTranscriptRequest>,
    ) -> Result<tonic::Response<TranscriptStableResponse>, tonic::Status> {
        self.0.probes.fetch_add(1, Ordering::SeqCst);
        Ok(tonic::Response::new(self.0.stable.clone()))
    }

    type ReadDenySettingsStream = Items<ReadDenySettingsEvent>;
    async fn read_deny_settings(
        &self,
        _: tonic::Request<TranscriptScope>,
    ) -> Result<tonic::Response<Self::ReadDenySettingsStream>, tonic::Status> {
        Ok(stream(&[]))
    }
}

fn connection_with(capabilities: &[&str], script: Arc<Script>) -> RemoteConnection {
    let client =
        HelperClient::serving_transcripts_for_test("example-host", "SHA256:example", capabilities, Scripted(script));
    let project = RemoteProject {
        version: 1,
        host: "example-host".into(),
        host_key_sha256: "SHA256:example".into(),
        root: ROOT.into(),
        helper_version: "0.4.0".into(),
        sync: None,
        created_at: 1,
        queued_stops: Vec::new(),
    };
    RemoteConnection::with_client(client, project, String::new(), String::new())
}

fn served(script: Script) -> (RemoteConnection, Arc<Script>) {
    let script = Arc::new(script);
    (connection_with(&[wire::CAP_TRANSCRIPTS], script.clone()), script)
}

fn entry(id: &str, title: &str, modified: u64) -> TranscriptEntry {
    TranscriptEntry {
        provider_session_id: id.into(),
        len: 100,
        modified_unix_ms: modified,
        ai_title: title.into(),
    }
}

fn list_of(entries: Vec<TranscriptEntry>) -> ListTranscriptsResponse {
    ListTranscriptsResponse {
        entries,
        config_dir: CONFIG.into(),
        root: ROOT.into(),
        ..Default::default()
    }
}

fn stable_answer(stable: bool) -> TranscriptStableResponse {
    TranscriptStableResponse {
        stable,
        config_dir: CONFIG.into(),
        root: ROOT.into(),
        ..Default::default()
    }
}

fn tail_of(data: &[u8], started_mid_file: bool) -> Vec<TranscriptChunk> {
    vec![TranscriptChunk {
        data: data.to_vec(),
        started_mid_file,
        last: true,
        config_dir: CONFIG.into(),
        root: ROOT.into(),
        ..Default::default()
    }]
}

fn prompt_line(text: &str) -> String {
    format!(
        r#"{{"type":"user","uuid":"u-1","version":"2.1.272","promptSource":"typed","message":{{"role":"user","content":{}}}}}"#,
        serde_json::to_string(text).unwrap()
    )
}

fn reply_line(text: &str) -> String {
    format!(
        r#"{{"type":"assistant","uuid":"a-1","version":"2.1.272","message":{{"role":"assistant","content":[{{"type":"text","text":{}}}]}}}}"#,
        serde_json::to_string(text).unwrap()
    )
}

fn scope(connection: &RemoteConnection) -> eitri_core::remote_project::transcripts::TabScope {
    connection.tab_scope(ROOT, None, None).expect("a named root")
}

#[test]
fn a_remote_tab_lists_the_servers_sessions_with_titles() {
    let (connection, _) = served(Script {
        list: list_of(vec![
            entry(SESSION, "Fix the build", 2_000),
            entry(OTHER, "Reverse \u{202e}gnp.exe\nsecond line \u{1b}[31m", 1_000),
        ]),
        ..Default::default()
    });
    assert!(connection.offers_transcripts());
    let listed = connection.list_server_sessions(&scope(&connection)).unwrap();
    assert_eq!(
        listed
            .sessions
            .iter()
            .map(|s| s.provider_session_id.as_str())
            .collect::<Vec<_>>(),
        [SESSION, OTHER],
        "the server's order, newest first"
    );
    assert_eq!(listed.sessions[0].title.as_deref(), Some("Fix the build"));
    assert_eq!(listed.sessions[0].updated_at, "2000");

    // The chooser's greeting for a remote root offers them; a local root and another agent's tab never do.
    let offered = |agent: &AgentId, root: GateRoot<'_>| {
        BackendGreeting::for_agent_at_root(
            BackendKind::Sidecar,
            agent,
            PathBuf::from(ROOT),
            Titles::ReadNow,
            root,
            listed.sessions.clone(),
        )
        .resumable
        .len()
    };
    assert_eq!(offered(&AgentId::Claude, GateRoot::RemoteWithoutFacts), 2);
    assert_eq!(offered(&AgentId::Codex, GateRoot::RemoteWithoutFacts), 0);
}

/// A title is the server's text: shown with its hidden characters written out, on one line, never as it came.
#[test]
fn titles_are_escaped_when_shown() {
    let (connection, _) = served(Script {
        list: list_of(vec![entry(OTHER, "Reverse \u{202e}gnp.exe\nsecond \u{1b}[31m line", 1)]),
        ..Default::default()
    });
    let listed = connection.list_server_sessions(&scope(&connection)).unwrap();
    let title = listed.sessions[0].title.clone().unwrap();
    assert!(title.contains("\\u{202e}"), "{title}");
    assert!(!title.chars().any(|c| c.is_control() || c == '\u{202e}'), "{title:?}");
    assert!(!title.contains("second"), "one line only: {title}");
}

#[test]
fn a_resume_waits_for_the_servers_stability_probe_and_refuses_a_moving_transcript() {
    let (connection, script) = served(Script {
        stable: stable_answer(false),
        tail: tail_of(prompt_line("hello").as_bytes(), false),
        ..Default::default()
    });
    let refused = connection.resume_seed(&scope(&connection), SESSION).unwrap_err();
    assert!(
        matches!(&refused, ConversationError::TranscriptUnstable { provider_session_id } if provider_session_id == SESSION),
        "{refused:?}"
    );
    assert_eq!(script.probes.load(Ordering::SeqCst), 1, "the probe ran first");
    assert_eq!(
        script.tails.load(Ordering::SeqCst),
        0,
        "a moving transcript is not read"
    );

    // A probe the server refused is no evidence of a writer: the resume goes on, as it does when it cannot probe here.
    let (connection, script) = served(Script {
        stable: TranscriptStableResponse {
            refusal: "the probe was refused".into(),
            ..Default::default()
        },
        tail: tail_of(format!("{}\n", prompt_line("hello")).as_bytes(), false),
        ..Default::default()
    });
    assert!(connection.resume_seed(&scope(&connection), SESSION).unwrap().is_some());
    assert_eq!(script.tails.load(Ordering::SeqCst), 1);
}

#[test]
fn a_resume_seeds_history_from_the_servers_tail() {
    let transcript = format!("{}\n{}\n", prompt_line("first question"), reply_line("the answer"));
    let (connection, _) = served(Script {
        stable: stable_answer(true),
        tail: tail_of(transcript.as_bytes(), false),
        ..Default::default()
    });
    let seed = connection
        .resume_seed(&scope(&connection), SESSION)
        .unwrap()
        .expect("the helper offers transcripts");
    assert_eq!(seed.user_prompts.len(), 1);
    assert_eq!(seed.user_prompts[0].text, "first question");
    assert_eq!(seed.transcript.len(), 1);
    assert_eq!(seed.transcript[0].text, "the answer");
    assert!(!seed.assistant_message_open && seed.active_turn_id.is_none());
    let notice = seed
        .history
        .as_ref()
        .expect("a restored history says where it came from");
    assert_eq!(notice.source, agent::HistorySource::ClaudeTranscript);
    assert_eq!(
        notice.source_path,
        format!("example-host:{CONFIG}/projects/-srv-project/{SESSION}.jsonl")
    );
    assert_eq!(notice.omitted_items, Some(0));
}

#[test]
fn a_huge_server_transcript_seeds_from_its_tail_with_the_note() {
    // The server sent the last bytes of a longer file: the first line is cut in the middle and dropped, the rest is
    // read, and the notice says records before it were left out without counting them.
    let mut tail = String::from("ation\":\"cut off\"}\n");
    for n in 0..20_000 {
        tail.push_str(&reply_line(&format!("reply {n} {}", "x".repeat(60))));
        tail.push('\n');
    }
    assert!(tail.len() > 1 << 20);
    let (connection, _) = served(Script {
        stable: stable_answer(true),
        tail: tail_of(tail.as_bytes(), true),
        ..Default::default()
    });
    let seed = connection.resume_seed(&scope(&connection), SESSION).unwrap().unwrap();
    assert!(!seed.transcript.is_empty(), "the tail's replies were read");
    let notice = seed.history.as_ref().expect("the tail has a notice");
    assert_eq!(notice.source, agent::HistorySource::ClaudeTranscript);
    assert_eq!(
        notice.omitted_items, None,
        "earlier records certainly left out, not counted"
    );
    assert!(
        notice.source_path.starts_with("example-host:"),
        "{}",
        notice.source_path
    );
}

#[test]
fn an_evicted_reattach_rebuilds_with_the_note() {
    // The rebuild reads the same transcript a resume does and names the server in its note, and only a history that
    // really was read from the CLI's transcript is called rebuilt from it; Eitri's own journal copy is not needed.
    let transcript = format!("{}\n{}\n", prompt_line("question"), reply_line("answer"));
    let (connection, _) = served(Script {
        stable: stable_answer(true),
        tail: tail_of(transcript.as_bytes(), false),
        ..Default::default()
    });
    let seed = connection.resume_seed(&scope(&connection), SESSION).unwrap().unwrap();
    assert_eq!(seed.user_prompts.len(), 1);
    assert_eq!(seed.transcript.len(), 1);
    assert_eq!(
        seed.history.as_ref().unwrap().source,
        agent::HistorySource::ClaudeTranscript
    );
    assert_eq!(
        rebuilt_note("example-host"),
        "rebuilt from the CLI's transcript on example-host"
    );
    assert_eq!(
        rebuilt_note("ex\u{202e}host"),
        "rebuilt from the CLI's transcript on ex\\u{202e}host",
        "the host's name is shown escaped"
    );
    assert_eq!(
        rebuilt_note_if_read_from_the_transcript("example-host", seed.history.as_ref()),
        Some("rebuilt from the CLI's transcript on example-host".to_string())
    );

    // A transcript the server could not give leaves a history that was not read from it: nothing was rebuilt from
    // the CLI's transcript, so the tab does not say so.
    let (missing, _) = served(Script {
        stable: stable_answer(true),
        tail: tail_of(b"", false),
        ..Default::default()
    });
    let seed = missing.resume_seed(&scope(&missing), SESSION).unwrap().unwrap();
    assert_eq!(
        rebuilt_note_if_read_from_the_transcript("example-host", seed.history.as_ref()),
        None
    );
    assert_eq!(rebuilt_note_if_read_from_the_transcript("example-host", None), None);
}

#[test]
fn without_the_capability_resume_is_refused_as_before() {
    let script = Arc::new(Script::default());
    let connection = connection_with(&[], script.clone());
    assert!(!connection.offers_transcripts());
    // Nothing is asked, and nothing seeds the resume: the panel keeps its refusal for such a tab.
    assert!(connection.resume_seed(&scope(&connection), SESSION).unwrap().is_none());
    assert_eq!(
        script.probes.load(Ordering::SeqCst) + script.tails.load(Ordering::SeqCst),
        0
    );
    let why = connection.list_server_sessions(&scope(&connection)).unwrap_err();
    assert!(why.contains("does not offer transcripts"), "{why}");
    assert_eq!(
        eitri_core::tab_set::REMOTE_RESUME_UNAVAILABLE,
        "session resume is not available on remote tabs yet"
    );
}

#[test]
fn a_config_dir_mismatch_refuses_with_both_paths() {
    let transcript = format!("{}\n", prompt_line("question"));
    let (connection, script) = served(Script {
        list: list_of(vec![entry(SESSION, "t", 1)]),
        stable: stable_answer(true),
        tail: tail_of(transcript.as_bytes(), false),
        ..Default::default()
    });
    // The tab's own agent reported a directory (with a stray `//` and `.`, still the same one) the answer names.
    let same = connection
        .tab_scope(ROOT, None, Some("/home/example//./.claude/"))
        .unwrap();
    assert!(connection.list_server_sessions(&same).is_ok());

    let other = connection
        .tab_scope(ROOT, None, Some("/home/example/.claude-work\n"))
        .unwrap();
    let why = connection.list_server_sessions(&other).unwrap_err();
    assert!(
        why.contains(CONFIG) && why.contains("/home/example/.claude-work"),
        "{why}"
    );
    assert!(!why.contains('\n'), "the server's text is shown on one line: {why:?}");
    // A resume takes no history from the other directory and says why instead of starting an empty tab: it is
    // refused, both paths named, whether or not Eitri holds a copy of the session.
    let refused = connection.resume_seed(&other, SESSION).unwrap_err();
    let ConversationError::ResumeRejected { reason, .. } = &refused else {
        panic!("the resume is refused: {refused:?}");
    };
    assert!(
        reason.contains(CONFIG) && reason.contains("/home/example/.claude-work"),
        "{reason}"
    );
    assert!(!refused.is_benign());
    assert_eq!(script.tails.load(Ordering::SeqCst), 1, "the tail came and was refused");
    // The directory the tab's agent reported, spelled differently, is the same one: the resume goes on.
    assert!(connection.resume_seed(&same, SESSION).unwrap().is_some());
}

#[test]
fn a_tab_whose_folder_is_not_named_is_not_asked() {
    let (connection, script) = served(Script::default());
    let why = connection.tab_scope("", None, None).unwrap_err();
    assert!(why.contains("not been checked yet"), "{why}");
    assert_eq!(script.probes.load(Ordering::SeqCst), 0);
}

#[test]
fn the_chooser_fetch_runs_off_the_gtk_thread() {
    let caller = std::thread::current().id();
    let (release, hold) = mpsc::channel::<()>();
    let (ran_on_tx, ran_on_rx) = mpsc::channel();
    let mut listing = SessionListing::default();
    let now = Instant::now();
    let started = listing.ask(now, || {
        spawn_listing(move || {
            ran_on_tx.send(std::thread::current().id()).unwrap();
            hold.recv().unwrap();
            Ok(ServerSessions {
                sessions: Vec::new(),
                truncated: false,
            })
        })
    });
    // `ask` returned while the work is still held: the caller was not made to wait for it.
    assert!(started && listing.is_pending());
    assert!(!listing.collect(), "no answer yet");
    assert_ne!(ran_on_rx.recv_timeout(Duration::from_secs(5)).unwrap(), caller);

    // Another open meanwhile, and one inside ten seconds of the ask, start nothing.
    assert!(!listing.ask(now + Duration::from_secs(1), || panic!("an ask is under way")));
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !listing.collect() {
        assert!(Instant::now() < deadline, "the answer never arrived");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(matches!(listing.latest(), Some(Ok(_))));
    assert!(!listing.ask(now + Duration::from_secs(9), || panic!(
        "asked again inside ten seconds"
    )));
    assert!(listing.ask(now + Duration::from_secs(10), || spawn_listing(|| Err("later".into()))));
}
