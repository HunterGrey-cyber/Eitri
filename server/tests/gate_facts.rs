#![allow(dead_code)]

#[path = "../src/audit.rs"]
mod audit;
#[path = "../src/daemon.rs"]
mod daemon;
#[path = "../src/peer.rs"]
mod peer;
#[path = "../src/run_dir.rs"]
mod run_dir;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use agent::permission_policy::{gate_fact, request_digest, REASON_EITRI_OWN_PLACES};
use agent::project_boundary::NotABoundary;
use claude_runtime_protocol::v1::{
    session_event::Event, PermissionOrigin, PermissionRequested, PermissionResolved, SessionClosed, SessionEvent,
    TextDelta, TurnCompleted,
};
use daemon::gate::{self as gate, Asked, Engine, StartedEnv};
use daemon::serve::fake_sidecar::{End, FakeSidecar, Reply};
use daemon::serve::{GateFacts, Serving};
use daemon::sidecars::{Record, Sidecars};
use eitri_server_proto::gate_fact_event::Event as FactEvent;
use eitri_server_proto::gate_server::Gate;
use eitri_server_proto::*;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tonic::Request;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REVISION: &str = env!("EITRI_BUILD_COMMIT");
fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

const SESSION: &str = "session";
const INSIDE: &str = "the path resolves inside the project root";
const OUTSIDE: &str = "the path resolves outside the project root";
const LINK_LEAVES: &str =
    "a link on the way sits outside the project root or leads out of it, even if the path comes back in";

/// A private scratch directory, canonical, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Scratch {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = PathBuf::from(format!(
            "/tmp/gf-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        agent::private_fs::create_private_dir_all(&path, &path).unwrap();
        Scratch(path.canonicalize().unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A project root that is a boundary, a home beside it and a file outside it.
struct Project {
    scratch: Scratch,
    root: PathBuf,
    home: PathBuf,
    secret: PathBuf,
}

impl Project {
    fn new() -> Project {
        let scratch = Scratch::new("p");
        let root = scratch.0.join("project");
        let home = scratch.0.join("home");
        for dir in [&root, &home, &scratch.0.join("outside")] {
            agent::private_fs::create_private_dir_all(dir, &scratch.0).unwrap();
        }
        std::fs::write(root.join("inside.txt"), "inside\n").unwrap();
        std::fs::write(root.join("other.txt"), "other\n").unwrap();
        let secret = scratch.0.join("outside/secret");
        std::fs::write(&secret, "private\n").unwrap();
        std::os::unix::fs::symlink("inside.txt", root.join("notes.txt")).unwrap();
        Project {
            scratch,
            root,
            home,
            secret,
        }
    }

    fn started(&self) -> StartedEnv {
        StartedEnv {
            git_location_from_environment: false,
            places: BTreeMap::from([("HOME".to_owned(), self.home.as_os_str().as_encoded_bytes().to_vec())]),
        }
    }

    fn read(&self, name: &str) -> String {
        json!({ "file_path": self.root.join(name) }).to_string()
    }
}

fn event(sequence: u64, event: Event) -> SessionEvent {
    SessionEvent {
        session_id: SESSION.into(),
        sequence,
        event: Some(event),
        ..Default::default()
    }
}

fn asked_with(sequence: u64, id: &str, tool: &str, input_json: &str, origin: PermissionOrigin) -> SessionEvent {
    event(
        sequence,
        Event::PermissionRequested(PermissionRequested {
            permission_id: id.into(),
            tool_name: tool.into(),
            input_json: input_json.into(),
            origin: origin as i32,
            ..Default::default()
        }),
    )
}

fn asked(sequence: u64, id: &str, tool: &str, input_json: &str) -> SessionEvent {
    asked_with(sequence, id, tool, input_json, PermissionOrigin::Hook)
}

fn resolved(sequence: u64, id: &str) -> SessionEvent {
    event(
        sequence,
        Event::PermissionResolved(PermissionResolved {
            permission_id: id.into(),
            ..Default::default()
        }),
    )
}

fn text(sequence: u64) -> SessionEvent {
    event(sequence, Event::TextDelta(TextDelta::default()))
}

/// A served session: the fake sidecar, registered as a running sidecar of this daemon at the project's root.
struct Served {
    project: Project,
    sidecar: FakeSidecar,
    serving: Arc<Serving>,
    gate: GateFacts,
}

fn record(sidecar: &FakeSidecar, project: &Project) -> Record {
    let pid = sidecar.info.pid;
    Record {
        pid,
        start_time: daemon::sidecars::start_time(pid).unwrap(),
        socket: agent::socket_path::in_dir(&sidecar.paths.run, &format!("sc-{}.sock", sidecar.info.sidecar_id))
            .unwrap(),
        sidecar_id: sidecar.info.sidecar_id.clone(),
        root: project.root.to_str().unwrap().to_owned(),
        account: String::new(),
        command_id: "start".into(),
        started_unix_ms: 0,
        ready: true,
        failure: None,
        exited_unix_ms: None,
        started: Some(project.started()),
    }
}

fn serving_for(sidecar: &FakeSidecar, project: &Project, engine: Arc<Engine>) -> Arc<Serving> {
    serving_with(sidecar, record(sidecar, project), engine)
}

fn serving_with(sidecar: &FakeSidecar, record: Record, engine: Arc<Engine>) -> Arc<Serving> {
    let sidecars = Arc::new(Sidecars::new(sidecar.paths.clone()).unwrap());
    sidecars.test_running(record);
    Serving::with_gate(sidecar.paths.clone(), sidecars, engine)
}

async fn served(project: Project, script: Vec<Reply>, engine: Arc<Engine>) -> Served {
    served_with(project, script, engine, |_| {}).await
}

/// A served session whose sidecar record `edit` changes first.
async fn served_with(
    project: Project,
    script: Vec<Reply>,
    engine: Arc<Engine>,
    edit: impl FnOnce(&mut Record),
) -> Served {
    let sidecar = FakeSidecar::start(script).await;
    let mut record = record(&sidecar, &project);
    edit(&mut record);
    let serving = serving_with(&sidecar, record, engine);
    Served {
        gate: GateFacts(serving.clone()),
        project,
        sidecar,
        serving,
    }
}

type Facts = <GateFacts as Gate>::WatchFactsStream;

impl Served {
    fn id(&self) -> String {
        self.sidecar.info.sidecar_id.clone()
    }

    async fn watch(&self) -> Facts {
        self.gate
            .watch_facts(Request::new(WatchFactsRequest {
                sidecar_id: self.id(),
                session_id: SESSION.into(),
            }))
            .await
            .unwrap()
            .into_inner()
    }

    async fn check(&self, id: &str, digest: Vec<u8>) -> CheckFactResponse {
        self.gate
            .check_fact(Request::new(CheckFactRequest {
                sidecar_id: self.id(),
                session_id: SESSION.into(),
                permission_id: id.into(),
                request_digest: digest,
            }))
            .await
            .unwrap()
            .into_inner()
    }

    fn digest(&self, id: &str, tool: &str, input_json: &str) -> Vec<u8> {
        request_digest(SESSION, id, tool, input_json.as_bytes(), &self.project.root).to_vec()
    }

    /// The session's journal head and pending count, once the helper follows it.
    async fn info(&self) -> Option<SessionInfo> {
        self.serving.sessions(&self.id()).await.into_iter().next()
    }

    async fn until_head(&self, head: u64) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while self.info().await.is_none_or(|info| info.journal_head < head) {
            assert!(tokio::time::Instant::now() < deadline, "hang guard");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// Waits for `done`; the deadline only guards against a hang.
async fn until(mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(tokio::time::Instant::now() < deadline, "hang guard");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The next event of a stream; the deadline only guards against a hang.
async fn next(facts: &mut Facts) -> GateFactEvent {
    tokio::time::timeout(Duration::from_secs(30), facts.next())
        .await
        .expect("hang guard")
        .expect("the stream ended")
        .unwrap()
}

async fn root_of(facts: &mut Facts) -> GateRootFact {
    match next(facts).await.event {
        Some(FactEvent::Root(root)) => root,
        other => panic!("the first event must be the root's: {other:?}"),
    }
}

async fn request_of(facts: &mut Facts) -> GateRequestFact {
    match next(facts).await.event {
        Some(FactEvent::Request(fact)) => fact,
        other => panic!("expected a request's fact: {other:?}"),
    }
}

/// Nothing arrives on the stream for a while (it may end).
async fn quiet(facts: &mut Facts) {
    if let Ok(Some(item)) = tokio::time::timeout(Duration::from_millis(300), facts.next()).await {
        panic!("nothing more was expected: {item:?}");
    }
}

fn classification(fact: &GateRequestFact) -> GateVerdict {
    fact.fact
        .as_ref()
        .expect("a fact")
        .classification
        .clone()
        .expect("a classification")
}

/// A classifier that waits until it is opened, counting its calls; the real disk half once opened.
#[derive(Clone, Default)]
struct Held {
    open: Arc<(Mutex<bool>, Condvar)>,
    calls: Arc<AtomicUsize>,
}

impl Held {
    fn opened() -> Held {
        let held = Held::default();
        held.release();
        held
    }

    fn engine(&self, timeout: Duration) -> Arc<Engine> {
        Engine::new(self.classify(), timeout)
    }

    fn classify(&self) -> gate::Classify {
        let held = self.clone();
        Arc::new(move |tool: &str, input: &Value, scene: &gate::Scene| {
            held.calls.fetch_add(1, Ordering::SeqCst);
            let (open, opened) = &*held.open;
            let _open = opened.wait_while(open.lock().unwrap(), |open| !*open).unwrap();
            gate_fact(tool, input, &scene.root, &scene.around, &scene.places)
        })
    }

    fn release(&self) {
        *self.open.0.lock().unwrap() = true;
        self.open.1.notify_all();
    }
}

/// A classifier that holds each call whose `file_path` names a file still closed, counting the calls that began and
/// returned by file name; the real disk half once the file is opened.
#[derive(Clone, Default)]
struct Gates {
    closed: Arc<(Mutex<HashSet<String>>, Condvar)>,
    began: Arc<Mutex<Vec<String>>>,
    returned: Arc<Mutex<Vec<String>>>,
}

impl Gates {
    fn closing(names: &[&str]) -> Gates {
        let gates = Gates::default();
        gates
            .closed
            .0
            .lock()
            .unwrap()
            .extend(names.iter().map(|name| (*name).to_owned()));
        gates
    }

    fn engine(&self) -> Arc<Engine> {
        let gates = self.clone();
        Engine::new(
            Arc::new(move |tool: &str, input: &Value, scene: &gate::Scene| {
                let name = Path::new(input["file_path"].as_str().unwrap_or_default())
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                gates.began.lock().unwrap().push(name.clone());
                let (closed, opened) = &*gates.closed;
                drop(
                    opened
                        .wait_while(closed.lock().unwrap(), |closed| closed.contains(&name))
                        .unwrap(),
                );
                let fact = gate_fact(tool, input, &scene.root, &scene.around, &scene.places);
                gates.returned.lock().unwrap().push(name);
                fact
            }),
            Duration::from_secs(30),
        )
    }

    fn open(&self, name: &str) {
        self.closed.0.lock().unwrap().remove(name);
        self.closed.1.notify_all();
    }

    fn began(&self, name: &str) -> usize {
        self.began.lock().unwrap().iter().filter(|began| *began == name).count()
    }

    fn returned(&self, name: &str) -> usize {
        self.returned
            .lock()
            .unwrap()
            .iter()
            .filter(|returned| *returned == name)
            .count()
    }
}

fn closed_event(sequence: u64) -> SessionEvent {
    event(sequence, Event::SessionClosed(SessionClosed::default()))
}

fn turn_end(sequence: u64) -> SessionEvent {
    event(sequence, Event::TurnCompleted(TurnCompleted::default()))
}

/// The stream ends; the deadline only guards against a hang.
async fn ends(facts: &mut Facts) {
    let next = tokio::time::timeout(Duration::from_secs(30), facts.next())
        .await
        .expect("the stream did not end");
    assert!(next.is_none(), "the stream was to end: {next:?}");
}

#[tokio::test]
async fn a_gate_request_gets_a_fact_and_a_provider_prompt_does_not() {
    let project = Project::new();
    let input = project.read("inside.txt");
    let script = vec![Reply::Events(
        vec![
            asked(1, "p1", "Read", &input),
            asked_with(2, "p2", "Read", &input, PermissionOrigin::ProviderPrompt),
            event(
                3,
                Event::PermissionRequested(PermissionRequested {
                    permission_id: "p3".into(),
                    tool_name: "Read".into(),
                    input_json: input.clone(),
                    origin: 99,
                    ..Default::default()
                }),
            ),
            text(4),
        ],
        End::Hold,
    )];
    let s = served(project, script, Engine::standard()).await;
    let mut facts = s.watch().await;
    assert!(root_of(&mut facts).await.is_boundary);
    let fact = request_of(&mut facts).await;
    assert_eq!((fact.permission_id.as_str(), fact.sequence), ("p1", 1));
    assert_eq!(
        classification(&fact),
        GateVerdict {
            passes: true,
            why: INSIDE.into()
        }
    );
    let held = fact.fact.unwrap();
    assert!(held.root_is_boundary);
    assert_eq!(held.places_card, None);
    assert_eq!(held.request_digest, s.digest("p1", "Read", &input));
    s.until_head(4).await;
    assert_eq!(s.info().await.unwrap().pending, 3, "every request is pending");
    quiet(&mut facts).await;
    // The CLI's own prompt, and an origin this helper does not know, which the laptop takes for one.
    for id in ["p2", "p3"] {
        let refused = s.check(id, s.digest(id, "Read", &input)).await;
        assert_eq!(refused.refusal, gate::REFUSAL_NOT_A_GATE_REQUEST, "{id}");
        assert_eq!((refused.first, refused.fresh), (None, None));
    }
}

#[tokio::test]
async fn watch_sends_pending_facts_then_new_ones() {
    let project = Project::new();
    let (one, two, three) = (
        project.read("inside.txt"),
        project.read("other.txt"),
        json!({ "file_path": project.secret }).to_string(),
    );
    let later = Arc::new(tokio::sync::Notify::new());
    let script = vec![
        Reply::Events(
            vec![asked(1, "p1", "Read", &one), asked(2, "p2", "Read", &two)],
            End::Clean,
        ),
        Reply::Later(later.clone(), vec![asked(3, "p3", "Read", &three)]),
    ];
    let s = served(project, script, Engine::standard()).await;
    let mut first = s.watch().await;
    root_of(&mut first).await;
    let mut seen = vec![request_of(&mut first).await, request_of(&mut first).await];
    seen.sort_by_key(|fact| fact.sequence);
    assert_eq!(
        seen.iter().map(|fact| fact.permission_id.as_str()).collect::<Vec<_>>(),
        ["p1", "p2"]
    );
    drop(first);
    // A stream opened later gets the root's fact, then the facts of the requests pending now, by sequence.
    let mut facts = s.watch().await;
    assert!(root_of(&mut facts).await.is_boundary);
    assert_eq!(request_of(&mut facts).await, seen[0]);
    assert_eq!(request_of(&mut facts).await, seen[1]);
    quiet(&mut facts).await;
    later.notify_one();
    let new = request_of(&mut facts).await;
    assert_eq!((new.permission_id.as_str(), new.sequence), ("p3", 3));
    assert_eq!(
        classification(&new),
        GateVerdict {
            passes: false,
            why: OUTSIDE.into()
        }
    );
}

#[tokio::test]
async fn the_fresh_fact_follows_the_disk_now() {
    let project = Project::new();
    let input = project.read("notes.txt");
    let script = vec![Reply::Events(vec![asked(1, "p1", "Read", &input)], End::Hold)];
    let s = served(project, script, Engine::standard()).await;
    let mut facts = s.watch().await;
    root_of(&mut facts).await;
    assert!(classification(&request_of(&mut facts).await).passes);
    // A background job points the link elsewhere after the request was made.
    let link = s.project.root.join("notes.txt");
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&s.project.secret, &link).unwrap();
    let checked = s.check("p1", s.digest("p1", "Read", &input)).await;
    assert_eq!(checked.refusal, "");
    let first = checked.first.unwrap().classification.unwrap();
    let fresh = checked.fresh.unwrap();
    assert!(first.passes, "the first fact was computed before the swap");
    let now = fresh.classification.unwrap();
    assert!(!now.passes, "the fresh fact reads the link as it is now: {now:?}");
    assert_eq!(
        agent::permission_policy::known_reason(&now.why),
        Some(LINK_LEAVES),
        "{now:?}"
    );
    assert_eq!(fresh.request_digest, s.digest("p1", "Read", &input));
}

#[tokio::test]
async fn check_fact_refuses_a_digest_for_another_request() {
    let project = Project::new();
    let (input, other) = (project.read("inside.txt"), project.read("other.txt"));
    let script = vec![Reply::Events(vec![asked(1, "p1", "Read", &input)], End::Hold)];
    let s = served(project, script, Engine::standard()).await;
    let mut facts = s.watch().await;
    root_of(&mut facts).await;
    request_of(&mut facts).await;
    for digest in [
        s.digest("p1", "Read", &other),
        s.digest("p2", "Read", &input),
        s.digest("p1", "Grep", &input),
        request_digest("another", "p1", "Read", input.as_bytes(), &s.project.root).to_vec(),
        request_digest(SESSION, "p1", "Read", input.as_bytes(), Path::new("/srv/project")).to_vec(),
        Vec::new(),
    ] {
        let refused = s.check("p1", digest).await;
        assert_eq!(refused.refusal, gate::REFUSAL_DIGEST);
        assert_eq!((refused.first, refused.fresh), (None, None));
    }
    let checked = s.check("p1", s.digest("p1", "Read", &input)).await;
    assert_eq!(checked.refusal, "");
    assert!(checked.first.is_some() && checked.fresh.is_some());
    assert_eq!(
        s.check("p9", s.digest("p9", "Read", &input)).await.refusal,
        gate::REFUSAL_NOT_PENDING
    );
}

#[tokio::test]
async fn a_resolved_request_loses_its_fact() {
    let project = Project::new();
    let (one, two) = (project.read("inside.txt"), project.read("other.txt"));
    let later = Arc::new(tokio::sync::Notify::new());
    let script = vec![
        Reply::Events(
            vec![asked(1, "p1", "Read", &one), asked(2, "p2", "Read", &two)],
            End::Clean,
        ),
        Reply::Later(later.clone(), vec![resolved(3, "p1")]),
    ];
    let s = served(project, script, Engine::standard()).await;
    let mut facts = s.watch().await;
    root_of(&mut facts).await;
    request_of(&mut facts).await;
    request_of(&mut facts).await;
    later.notify_one();
    s.until_head(3).await;
    assert_eq!(
        s.check("p1", s.digest("p1", "Read", &one)).await.refusal,
        gate::REFUSAL_NOT_PENDING
    );
    let mut after = s.watch().await;
    root_of(&mut after).await;
    let left = request_of(&mut after).await;
    assert_eq!(left.permission_id, "p2");
    quiet(&mut after).await;
}

#[tokio::test]
async fn a_close_drops_every_fact_and_ends_the_stream() {
    let project = Project::new();
    let input = project.read("inside.txt");
    let later = Arc::new(tokio::sync::Notify::new());
    let script = vec![
        Reply::Events(vec![asked(1, "p1", "Read", &input)], End::Clean),
        Reply::Later(
            later.clone(),
            vec![
                event(2, Event::TurnCompleted(TurnCompleted::default())),
                asked(3, "p2", "Read", &input),
                event(4, Event::SessionClosed(SessionClosed::default())),
            ],
        ),
    ];
    let s = served(project, script, Engine::standard()).await;
    let mut facts = s.watch().await;
    root_of(&mut facts).await;
    assert_eq!(request_of(&mut facts).await.permission_id, "p1");
    later.notify_one();
    // The close ends the stream; p2's fact, if it was sent before the close, is the only other event.
    let rest: Vec<_> = tokio::time::timeout(Duration::from_secs(30), facts.collect::<Vec<_>>())
        .await
        .expect("a closed session ends the stream");
    for item in rest {
        match item.unwrap().event {
            Some(FactEvent::Request(fact)) => assert_eq!(fact.permission_id, "p2"),
            other => panic!("unexpected {other:?}"),
        }
    }
    for id in ["p1", "p2"] {
        assert_eq!(
            s.check(id, s.digest(id, "Read", &input)).await.refusal,
            gate::REFUSAL_NOT_PENDING
        );
    }
    let mut closed = s.watch().await;
    root_of(&mut closed).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(30), closed.next())
            .await
            .expect("hang guard")
            .is_none(),
        "a closed session's stream holds no fact"
    );
}

/// A watch that ends because the sidecar no longer has the session ends the fact streams too.
#[tokio::test]
async fn a_session_the_sidecar_dropped_ends_its_stream() {
    let project = Project::new();
    let input = project.read("inside.txt");
    let later = Arc::new(tokio::sync::Notify::new());
    let script = vec![Reply::LaterThen(
        later.clone(),
        vec![asked(1, "p1", "Read", &input)],
        End::Fail(tonic::Status::not_found("this session is gone")),
    )];
    let s = served(project, script, Engine::standard()).await;
    let mut facts = s.watch().await;
    root_of(&mut facts).await;
    later.notify_one();
    let rest: Vec<_> = tokio::time::timeout(Duration::from_secs(30), facts.collect::<Vec<_>>())
        .await
        .expect("a session the sidecar dropped ends the stream");
    for item in rest {
        match item.unwrap().event {
            Some(FactEvent::Request(fact)) => assert_eq!(fact.permission_id, "p1"),
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[tokio::test]
async fn adoption_recomputes_facts_for_pending_requests() {
    let project = Project::new();
    let input = project.read("inside.txt");
    let sidecar = FakeSidecar::start(vec![Reply::Events(vec![asked(1, "p1", "Read", &input)], End::Hold)]).await;
    let before = Held::opened();
    let serving = serving_for(&sidecar, &project, before.engine(GATE_FACT_TIMEOUT));
    let gate = GateFacts(serving.clone());
    let request = || {
        Request::new(WatchFactsRequest {
            sidecar_id: sidecar.info.sidecar_id.clone(),
            session_id: SESSION.into(),
        })
    };
    let mut facts = gate.watch_facts(request()).await.unwrap().into_inner();
    root_of(&mut facts).await;
    let first = request_of(&mut facts).await;
    assert_eq!(before.calls.load(Ordering::SeqCst), 1);
    drop(facts);
    drop(gate);
    drop(serving);
    // A restarted daemon has no facts: adopting the session computes the pending request's again.
    let after = Held::opened();
    let restarted = serving_for(&sidecar, &project, after.engine(GATE_FACT_TIMEOUT));
    restarted.adopt().await.unwrap();
    let gate = GateFacts(restarted.clone());
    let mut facts = gate.watch_facts(request()).await.unwrap().into_inner();
    root_of(&mut facts).await;
    let again = request_of(&mut facts).await;
    assert_eq!(after.calls.load(Ordering::SeqCst), 1, "adoption computed the fact");
    assert_eq!((again.permission_id.as_str(), again.sequence), ("p1", 1));
    assert_eq!(classification(&again), classification(&first));
    let info = restarted.sessions(&sidecar.info.sidecar_id).await;
    assert_eq!((info[0].journal_head, info[0].pending), (1, 1));
}

#[tokio::test]
async fn a_stalled_classification_becomes_a_refusal_fact() {
    // A computation that has not returned holds nothing up: with a classifier that waits until it is let go, and no
    // time limit to speak of, the watch still takes the events after the request.
    let project = Project::new();
    let input = project.read("inside.txt");
    let script = vec![Reply::Events(vec![asked(1, "p1", "Read", &input), text(2)], End::Hold)];
    let waiting = Held::default();
    let s = served(project, script, waiting.engine(Duration::from_secs(3600))).await;
    let mut facts = s.watch().await;
    root_of(&mut facts).await;
    s.until_head(2).await;
    until(|| waiting.calls.load(Ordering::SeqCst) == 1).await;
    waiting.release();
    assert!(classification(&request_of(&mut facts).await).passes);

    // Past the time limit the fact is a refusal, and so is a fresh one.
    let project = Project::new();
    let input = project.read("inside.txt");
    let script = vec![Reply::Events(vec![asked(1, "p1", "Read", &input)], End::Hold)];
    let stalled = Held::default();
    let s = served(project, script, stalled.engine(Duration::from_millis(200))).await;
    let mut facts = s.watch().await;
    root_of(&mut facts).await;
    let fact = request_of(&mut facts).await;
    assert_eq!((fact.permission_id.as_str(), fact.fact), ("p1", None));
    assert_eq!(fact.refusal, gate::REFUSAL_TIMED_OUT);
    let checked = s.check("p1", s.digest("p1", "Read", &input)).await;
    assert_eq!(checked.refusal, gate::REFUSAL_TIMED_OUT);
    assert_eq!((checked.first, checked.fresh), (None, None));
    stalled.release();
}

#[tokio::test]
async fn the_helpers_state_directory_is_a_protected_place() {
    let project = Project::new();
    let homes = Scratch::new("h");
    let paths = run_dir::Paths::new("t", &homes.0.join("d"), &homes.0.join("s"), &homes.0).unwrap();
    paths.prepare().unwrap();
    let scene = Arc::new(gate::surroundings::scene(&project.root, &project.started(), &paths));
    let engine = Engine::standard();
    let fact = |tool: &'static str, input: Value| {
        let engine = engine.clone();
        let scene = scene.clone();
        async move {
            let asked = Asked {
                permission_id: "p".into(),
                sequence: 1,
                tool_name: tool.into(),
                input_json: input.to_string(),
            };
            engine.fact(&asked, SESSION, &scene).await.unwrap()
        }
    };
    for target in [
        paths.state.join("pending/x.bin"),
        paths.state.join("journal/x/seg"),
        paths.run.join("sc-x.sock"),
        paths.data.join("installation/bin/eitri-server"),
    ] {
        let written = fact("Write", json!({ "file_path": target, "content": "" })).await;
        assert_eq!(
            written.places_card,
            Some(GateVerdict {
                passes: false,
                why: REASON_EITRI_OWN_PLACES.into()
            }),
            "{}",
            target.display()
        );
        assert!(!written.classification.unwrap().passes);
        let touched = fact("Bash", json!({ "command": format!("touch {}", target.display()) })).await;
        assert_eq!(touched.places_card.unwrap().why, REASON_EITRI_OWN_PLACES);
    }
    let inside = fact(
        "Write",
        json!({ "file_path": project.root.join("new.txt"), "content": "" }),
    )
    .await;
    assert_eq!(inside.places_card, None);
    assert!(inside.classification.unwrap().passes);
}

/// Every file under `dir`, by path, with its bytes.
fn tree(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.insert(path.clone(), std::fs::read(&path).unwrap());
            }
        }
    }
    files
}

#[tokio::test]
async fn facts_write_no_journal_frame_and_leave_pending_unchanged() {
    let project = Project::new();
    let (one, two) = (project.read("inside.txt"), project.read("notes.txt"));
    let script = vec![Reply::Events(
        vec![asked(1, "p1", "Read", &one), asked(2, "p2", "Read", &two), text(3)],
        End::Hold,
    )];
    let held = Held::default();
    let s = served(project, script, held.engine(Duration::from_secs(30))).await;
    let mut facts = s.watch().await;
    root_of(&mut facts).await;
    s.until_head(3).await;
    let state = s.sidecar.paths.state.clone();
    let read_state = || (tree(&state.join("journal")), tree(&state.join("pending")));
    let pending = |serving: Arc<Serving>, id: String| async move { serving.pending(&id, SESSION).await.unwrap() };
    // Taken while both facts are still being computed.
    let before = read_state();
    let pending_before = pending(s.serving.clone(), s.id()).await;
    let info_before = s.info().await.unwrap();
    until(|| held.calls.load(Ordering::SeqCst) == 2).await;
    held.release();
    request_of(&mut facts).await;
    request_of(&mut facts).await;
    for (id, input) in [("p1", &one), ("p2", &two)] {
        let checked = s.check(id, s.digest(id, "Read", input)).await;
        assert_eq!(checked.refusal, "");
    }
    assert_eq!(
        held.calls.load(Ordering::SeqCst),
        4,
        "two first facts and two fresh ones"
    );
    assert_eq!(read_state(), before, "a fact changed the journal or the pending file");
    assert_eq!(pending(s.serving.clone(), s.id()).await, pending_before);
    assert_eq!(s.info().await.unwrap(), info_before);
}

#[test]
fn a_sidecars_start_environment_keeps_its_git_flag_and_place_variables() {
    let mut command = Command::new("/bin/true");
    command
        .env_clear()
        .envs([
            ("HOME", "/srv/home"),
            ("XDG_DATA_HOME", "/srv/home/data"),
            ("XDG_STATE_HOME", "/srv/home/state"),
            ("PATH", "/usr/bin"),
        ])
        .env_remove("XDG_STATE_HOME");
    let started = StartedEnv::of(&command).unwrap();
    assert!(!started.git_location_from_environment);
    assert_eq!(
        started.places,
        BTreeMap::from([
            ("HOME".to_owned(), b"/srv/home".to_vec()),
            ("XDG_DATA_HOME".to_owned(), b"/srv/home/data".to_vec()),
        ])
    );
    command.env("GIT_DIR", "");
    assert!(StartedEnv::of(&command).unwrap().git_location_from_environment);
    // A record written before the helper kept it reads with none, and a kept one survives the round trip.
    let older: Record = serde_json::from_str(r#"{"pid":1,"start_time":2,"socket":"/x"}"#).unwrap();
    assert_eq!(older.started, None);
    let mut kept = older.clone();
    kept.started = Some(started.clone());
    let back: Record = serde_json::from_slice(&serde_json::to_vec(&kept).unwrap()).unwrap();
    assert_eq!(back.started, Some(started));
    // Values are kept as text, or as hex when they are not UTF-8, and read back as the same bytes.
    let odd = StartedEnv {
        git_location_from_environment: true,
        places: BTreeMap::from([
            ("HOME".to_owned(), b"/srv/home".to_vec()),
            ("XDG_DATA_DIRS".to_owned(), b"/srv/\xff:/usr/share".to_vec()),
        ]),
    };
    let json = serde_json::to_value(&odd).unwrap();
    assert_eq!(
        json["places"],
        json!({ "HOME": "/srv/home", "XDG_DATA_DIRS": { "hex": "2f7372762fff3a2f7573722f7368617265" } })
    );
    assert_eq!(serde_json::from_value::<StartedEnv>(json).unwrap(), odd);
    // A kept environment that does not read (another form, a bad value) leaves the record readable, with none.
    for started in [
        json!({ "git_location_from_environment": false, "places": { "HOME": [47, 115] } }),
        json!({ "git_location_from_environment": false, "places": { "HOME": { "hex": "+f" } } }),
        json!("not an environment"),
    ] {
        let record: Record =
            serde_json::from_value(json!({ "pid": 1, "start_time": 2, "socket": "/x", "started": started })).unwrap();
        assert_eq!((record.pid, record.started), (1, None));
    }
}

#[test]
fn every_reason_and_refusal_fits_the_wire_bound() {
    for text in agent::permission_policy::ALL_REASONS.iter().chain(&[
        gate::REFUSAL_TIMED_OUT,
        gate::REFUSAL_FAILED,
        gate::REFUSAL_TOO_MANY,
        gate::REFUSAL_UNKNOWN_START,
        gate::REFUSAL_NOT_PENDING,
        gate::REFUSAL_DIGEST,
        gate::REFUSAL_NOT_A_GATE_REQUEST,
        gate::REFUSAL_UNREADABLE,
        gate::REFUSAL_NO_FIRST,
    ]) {
        assert!(text.len() <= GATE_REASON_MAX, "{text}");
    }
}

/// A daemon of this build, run by the test below in a process of its own.
#[test]
fn fixture_gate_daemon_entry() {
    if std::env::var_os("EITRI_TEST_GATE_DAEMON").is_none() {
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let paths = run_dir::Paths::from_env(VERSION).unwrap();
        daemon::run_with_environment(
            paths,
            Duration::from_secs(600),
            daemon::OwnedSidecars::default(),
            async { Err(std::io::Error::other("no login environment in this test")) },
        )
        .await
        .unwrap();
    });
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn the_daemon_serves_gate_and_hello_lists_gate_facts() {
    let homes = Scratch::new("d");
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "fixture_gate_daemon_entry", "--nocapture"])
        .env_clear()
        .env("EITRI_TEST_GATE_DAEMON", "1")
        .env("HOME", &homes.0)
        .env("XDG_DATA_HOME", homes.0.join("d"))
        .env("XDG_STATE_HOME", homes.0.join("s"))
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut daemon = Daemon(child);
    let socket = run_dir::Paths::new(VERSION, &homes.0.join("d"), &homes.0.join("s"), &std::env::temp_dir())
        .unwrap()
        .socket()
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let channel = loop {
        assert!(daemon.0.try_wait().unwrap().is_none(), "the daemon exited");
        if let Ok(channel) = channel_to(socket.clone()).await {
            break channel;
        }
        assert!(tokio::time::Instant::now() < deadline, "the daemon did not start");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let hello = loop {
        match helper_client::HelperClient::new(channel.clone())
            .hello(HelloRequest {
                protocol_major: PROTOCOL_MAJOR,
                ..Default::default()
            })
            .await
        {
            Ok(hello) => break hello.into_inner(),
            Err(_) if tokio::time::Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(10)).await,
            Err(status) => panic!("{status}"),
        }
    };
    assert!(hello.capabilities.iter().any(|cap| cap == CAP_GATE_FACTS));
    let refused = gate_client::GateClient::new(channel)
        .check_fact(CheckFactRequest {
            sidecar_id: "0123456789abcdef".into(),
            session_id: SESSION.into(),
            permission_id: "p".into(),
            request_digest: vec![0; 32],
        })
        .await
        .unwrap_err();
    assert!(
        matches!(refused.code(), tonic::Code::NotFound | tonic::Code::Unavailable),
        "the daemon serves Gate: {refused:?}"
    );
}

async fn channel_to(socket: PathBuf) -> Result<tonic::transport::Channel, tonic::transport::Error> {
    tonic::transport::Endpoint::from_static("http://localhost")
        .connect_with_connector(tower::service_fn(move |_| {
            let socket = socket.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(socket).await?;
                peer::check(&stream)?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
}

#[tokio::test]
async fn a_turn_end_drops_every_fact_and_the_stream_goes_on() {
    let project = Project::new();
    let input = project.read("inside.txt");
    let later = Arc::new(tokio::sync::Notify::new());
    let script = vec![
        Reply::Events(vec![asked(1, "p1", "Read", &input)], End::Clean),
        Reply::Later(later.clone(), vec![turn_end(2)]),
    ];
    let s = served(project, script, Engine::standard()).await;
    let mut facts = s.watch().await;
    root_of(&mut facts).await;
    assert_eq!(request_of(&mut facts).await.permission_id, "p1");
    later.notify_one();
    s.until_head(2).await;
    assert_eq!(
        s.check("p1", s.digest("p1", "Read", &input)).await.refusal,
        gate::REFUSAL_NOT_PENDING
    );
    let mut after = s.watch().await;
    root_of(&mut after).await;
    // No fact is held, and the session is still open: both streams wait for the next one.
    for stream in [&mut facts, &mut after] {
        assert!(
            tokio::time::timeout(Duration::from_millis(300), stream.next())
                .await
                .is_err(),
            "a turn's end neither sends a fact nor ends the stream"
        );
    }
}

#[tokio::test]
async fn a_session_closed_before_it_was_observed_ends_its_stream() {
    let project = Project::new();
    let input = project.read("inside.txt");
    let sidecar = FakeSidecar::start(vec![Reply::Events(
        vec![asked(1, "p1", "Read", &input), closed_event(2)],
        End::Hold,
    )])
    .await;
    let serving = serving_for(&sidecar, &project, Engine::standard());
    let gate = GateFacts(serving.clone());
    let request = || {
        Request::new(WatchFactsRequest {
            sidecar_id: sidecar.info.sidecar_id.clone(),
            session_id: SESSION.into(),
        })
    };
    let mut facts = gate.watch_facts(request()).await.unwrap().into_inner();
    root_of(&mut facts).await;
    while let Some(item) = tokio::time::timeout(Duration::from_secs(30), facts.next())
        .await
        .expect("a closed session ends the stream")
    {
        assert!(matches!(item.unwrap().event, Some(FactEvent::Request(_))));
    }
    drop((facts, gate, serving));
    // A restarted daemon reads the session closed from its journal: no fact, and the stream ends after the root's.
    let restarted = serving_for(&sidecar, &project, Engine::standard());
    restarted.adopt().await.unwrap();
    let gate = GateFacts(restarted.clone());
    let mut facts = gate.watch_facts(request()).await.unwrap().into_inner();
    assert_eq!(root_of(&mut facts).await, GateRootFact::default());
    ends(&mut facts).await;
}

#[tokio::test]
async fn a_fact_whose_request_left_pending_while_it_was_computed_is_dropped() {
    // Answered, and its turn ended, while the fact was computed.
    for ending in [resolved(2, "p1"), turn_end(2)] {
        let project = Project::new();
        let input = project.read("inside.txt");
        let gates = Gates::closing(&["inside.txt"]);
        let later = Arc::new(tokio::sync::Notify::new());
        let script = vec![
            Reply::Events(vec![asked(1, "p1", "Read", &input)], End::Clean),
            Reply::Later(later.clone(), vec![ending]),
        ];
        let s = served(project, script, gates.engine()).await;
        let mut facts = s.watch().await;
        root_of(&mut facts).await;
        until(|| gates.began("inside.txt") == 1).await;
        later.notify_one();
        s.until_head(2).await;
        gates.open("inside.txt");
        until(|| gates.returned("inside.txt") == 1).await;
        quiet(&mut facts).await;
        let mut after = s.watch().await;
        root_of(&mut after).await;
        quiet(&mut after).await;
        assert_eq!(
            s.check("p1", s.digest("p1", "Read", &input)).await.refusal,
            gate::REFUSAL_NOT_PENDING
        );
    }
}

#[tokio::test]
async fn a_fact_for_an_earlier_request_under_the_same_id_is_never_sent() {
    let project = Project::new();
    let (one, two) = (project.read("inside.txt"), project.read("other.txt"));
    let gates = Gates::closing(&["inside.txt", "other.txt"]);
    let later = Arc::new(tokio::sync::Notify::new());
    let script = vec![
        Reply::Events(vec![asked(1, "p1", "Read", &one)], End::Clean),
        Reply::Later(later.clone(), vec![resolved(2, "p1"), asked(3, "p1", "Read", &two)]),
    ];
    let s = served(project, script, gates.engine()).await;
    let mut facts = s.watch().await;
    root_of(&mut facts).await;
    until(|| gates.began("inside.txt") == 1).await;
    later.notify_one();
    s.until_head(3).await;
    until(|| gates.began("other.txt") == 1).await;
    // The first request's computation returns while the second one's is still running: its fact is not kept.
    gates.open("inside.txt");
    until(|| gates.returned("inside.txt") == 1).await;
    quiet(&mut facts).await;
    gates.open("other.txt");
    let fact = request_of(&mut facts).await;
    assert_eq!((fact.permission_id.as_str(), fact.sequence), ("p1", 3));
    assert_eq!(fact.fact.unwrap().request_digest, s.digest("p1", "Read", &two));
    quiet(&mut facts).await;
    let checked = s.check("p1", s.digest("p1", "Read", &two)).await;
    assert_eq!(checked.refusal, "");
    assert_eq!(checked.first.unwrap().request_digest, s.digest("p1", "Read", &two));
    assert_eq!(
        s.check("p1", s.digest("p1", "Read", &one)).await.refusal,
        gate::REFUSAL_DIGEST
    );
}

#[tokio::test]
async fn a_request_pending_refused_gets_no_fact() {
    // Pending keeps the first request under an id while it is pending; a second one under the same id is not held,
    // and neither is a fact for it.
    let project = Project::new();
    let (one, two) = (project.read("inside.txt"), project.read("other.txt"));
    let script = vec![Reply::Events(
        vec![asked(1, "p1", "Read", &one), asked(2, "p1", "Read", &two), text(3)],
        End::Hold,
    )];
    let s = served(project, script, Engine::standard()).await;
    let mut facts = s.watch().await;
    root_of(&mut facts).await;
    let fact = request_of(&mut facts).await;
    assert_eq!((fact.permission_id.as_str(), fact.sequence), ("p1", 1));
    s.until_head(3).await;
    quiet(&mut facts).await;
    let checked = s.check("p1", s.digest("p1", "Read", &one)).await;
    assert_eq!(checked.refusal, "");
    assert_eq!(checked.first.unwrap().request_digest, s.digest("p1", "Read", &one));
    assert_eq!(
        s.check("p1", s.digest("p1", "Read", &two)).await.refusal,
        gate::REFUSAL_DIGEST
    );
}

#[tokio::test]
async fn check_fact_refuses_a_request_answered_while_it_computed() {
    let project = Project::new();
    let input = project.read("inside.txt");
    let gates = Gates::closing(&["inside.txt"]);
    let later = Arc::new(tokio::sync::Notify::new());
    let script = vec![
        Reply::Events(vec![asked(1, "p1", "Read", &input)], End::Clean),
        Reply::Later(later.clone(), vec![resolved(2, "p1")]),
    ];
    let s = served(project, script, gates.engine()).await;
    let mut facts = s.watch().await;
    root_of(&mut facts).await;
    until(|| gates.began("inside.txt") == 1).await;
    let (checked, ()) = tokio::join!(s.check("p1", s.digest("p1", "Read", &input)), async {
        // The fresh fact is being computed when the answer arrives.
        until(|| gates.began("inside.txt") == 2).await;
        later.notify_one();
        s.until_head(2).await;
        gates.open("inside.txt");
    });
    assert_eq!(checked.refusal, gate::REFUSAL_NOT_PENDING);
    assert_eq!((checked.first, checked.fresh), (None, None));
}

#[tokio::test]
async fn an_unknown_start_environment_gives_refusals() {
    let project = Project::new();
    let input = project.read("inside.txt");
    let script = vec![Reply::Events(vec![asked(1, "p1", "Read", &input)], End::Hold)];
    let s = served_with(project, script, Engine::standard(), |record| record.started = None).await;
    let mut facts = s.watch().await;
    assert_eq!(root_of(&mut facts).await, GateRootFact::default());
    let fact = request_of(&mut facts).await;
    assert_eq!((fact.fact, fact.refusal.as_str()), (None, gate::REFUSAL_UNKNOWN_START));
    let checked = s.check("p1", s.digest("p1", "Read", &input)).await;
    assert_eq!(checked.refusal, gate::REFUSAL_UNKNOWN_START);
    assert_eq!((checked.first, checked.fresh), (None, None));
}

#[tokio::test]
async fn a_root_that_is_no_boundary_is_named() {
    for (root, refusal) in [("home", NotABoundary::HomeOrAbove), ("/", NotABoundary::FilesystemRoot)] {
        let project = Project::new();
        let root = match root {
            "home" => project.home.clone(),
            other => PathBuf::from(other),
        };
        let input = json!({ "file_path": root.join("x") }).to_string();
        let script = vec![Reply::Events(vec![asked(1, "p1", "Read", &input)], End::Hold)];
        let named = root.to_str().unwrap().to_owned();
        let s = served_with(project, script, Engine::standard(), |record| record.root = named).await;
        let mut facts = s.watch().await;
        assert_eq!(
            root_of(&mut facts).await,
            GateRootFact {
                is_boundary: false,
                not_a_boundary: refusal.name().into(),
            }
        );
        let fact = request_of(&mut facts).await.fact.unwrap();
        assert!(!fact.root_is_boundary);
        assert!(!fact.classification.unwrap().passes);
    }
}

#[tokio::test]
async fn busy_workers_turn_facts_and_the_root_into_refusals_and_are_audited_once() {
    let project = Project::new();
    let inputs: Vec<String> = (0..=gate::WORKERS).map(|_| project.read("inside.txt")).collect();
    let script = vec![Reply::Events(
        inputs
            .iter()
            .enumerate()
            .map(|(at, input)| asked(at as u64 + 1, &format!("p{at}"), "Read", input))
            .collect(),
        End::Hold,
    )];
    let stalled = Held::default();
    let s = served(project, script, stalled.engine(Duration::from_millis(300))).await;
    let mut facts = s.watch().await;
    root_of(&mut facts).await;
    let mut refusals = Vec::new();
    for _ in 0..=gate::WORKERS {
        refusals.push(request_of(&mut facts).await.refusal);
    }
    assert_eq!(refusals, vec![gate::REFUSAL_TIMED_OUT; gate::WORKERS + 1]);
    assert_eq!(
        stalled.calls.load(Ordering::SeqCst),
        gate::WORKERS,
        "no more computations run at once than there are workers"
    );
    // The stalled computations still hold every worker, so the root's check cannot run either.
    let mut later = s.watch().await;
    assert_eq!(root_of(&mut later).await, GateRootFact::default());
    let audit = s.sidecar.paths.state.join("audit.log");
    let busy = || {
        std::fs::read_to_string(&audit)
            .unwrap_or_default()
            .matches(" gate-facts-workers-busy ")
            .count()
    };
    until(|| busy() >= 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(busy(), 1, "audited once while the workers stay busy");
    stalled.release();
}

#[tokio::test]
async fn a_session_holds_computed_facts_for_at_most_its_bound() {
    let project = Project::new();
    let input = project.read("inside.txt");
    let script = vec![Reply::Events(
        vec![
            asked(1, "p1", "Read", &input),
            asked(2, "p2", "Read", &input),
            asked(3, "p3", "Read", &input),
        ],
        End::Hold,
    )];
    let held = Held::default();
    let s = served(
        project,
        script,
        Engine::holding(held.classify(), Duration::from_secs(30), 2),
    )
    .await;
    let mut facts = s.watch().await;
    root_of(&mut facts).await;
    let refused = request_of(&mut facts).await;
    assert_eq!(
        (refused.permission_id.as_str(), refused.refusal.as_str()),
        ("p3", gate::REFUSAL_TOO_MANY)
    );
    held.release();
    let mut computed = vec![request_of(&mut facts).await, request_of(&mut facts).await];
    computed.sort_by_key(|fact| fact.sequence);
    assert_eq!(
        computed
            .iter()
            .map(|fact| fact.permission_id.as_str())
            .collect::<Vec<_>>(),
        ["p1", "p2"]
    );
    assert!(computed.iter().all(|fact| fact.fact.is_some()));
}

#[tokio::test]
async fn a_failed_computation_is_a_refusal_fact() {
    let project = Project::new();
    let input = project.read("inside.txt");
    let script = vec![Reply::Events(vec![asked(1, "p1", "Read", &input)], End::Hold)];
    let failing = Engine::new(
        Arc::new(|_: &str, _: &Value, _: &gate::Scene| panic!("the disk half failed")),
        GATE_FACT_TIMEOUT,
    );
    let s = served(project, script, failing).await;
    let mut facts = s.watch().await;
    root_of(&mut facts).await;
    let fact = request_of(&mut facts).await;
    assert_eq!((fact.fact, fact.refusal.as_str()), (None, gate::REFUSAL_FAILED));
    let checked = s.check("p1", s.digest("p1", "Read", &input)).await;
    assert_eq!(checked.refusal, gate::REFUSAL_FAILED);
    assert_eq!((checked.first, checked.fresh), (None, None));
}

/// A refusal fact holds nothing but its words: it takes no place under the bound on computed facts.
#[tokio::test]
async fn a_refusal_fact_takes_no_place_under_the_bound() {
    let project = Project::new();
    let (bad, good) = (project.read("bad.txt"), project.read("inside.txt"));
    let later = Arc::new(tokio::sync::Notify::new());
    let script = vec![
        Reply::Events(vec![asked(1, "p1", "Read", &bad)], End::Clean),
        Reply::Later(later.clone(), vec![asked(2, "p2", "Read", &good)]),
    ];
    let engine = Engine::holding(
        Arc::new(|tool: &str, input: &Value, scene: &gate::Scene| {
            if input["file_path"].as_str().unwrap_or_default().ends_with("bad.txt") {
                panic!("the disk half failed");
            }
            gate_fact(tool, input, &scene.root, &scene.around, &scene.places)
        }),
        Duration::from_secs(30),
        1,
    );
    let s = served(project, script, engine).await;
    let mut facts = s.watch().await;
    root_of(&mut facts).await;
    let refused = request_of(&mut facts).await;
    assert_eq!(
        (refused.permission_id.as_str(), refused.refusal.as_str()),
        ("p1", gate::REFUSAL_FAILED)
    );
    later.notify_one();
    let computed = request_of(&mut facts).await;
    assert_eq!(computed.permission_id, "p2");
    assert!(computed.fact.is_some(), "{computed:?}");
}

/// A fresh check asked while the first fact is still being computed waits for it and returns it.
#[tokio::test]
async fn check_fact_returns_a_first_fact_still_being_computed() {
    let project = Project::new();
    let input = project.read("inside.txt");
    let gates = Gates::closing(&["inside.txt"]);
    let script = vec![Reply::Events(vec![asked(1, "p1", "Read", &input)], End::Hold)];
    let s = served(project, script, gates.engine()).await;
    let mut facts = s.watch().await;
    root_of(&mut facts).await;
    until(|| gates.began("inside.txt") == 1).await;
    let (checked, ()) = tokio::join!(s.check("p1", s.digest("p1", "Read", &input)), async {
        until(|| gates.began("inside.txt") == 2).await;
        gates.open("inside.txt");
    });
    assert_eq!(checked.refusal, "");
    let digest = s.digest("p1", "Read", &input);
    assert_eq!(checked.first.unwrap().request_digest, digest);
    assert_eq!(checked.fresh.unwrap().request_digest, digest);
}

/// A run directory that falls back outside the state directory (a state home too long for a socket path) is one
/// of the helper's protected places too.
#[test]
fn a_run_directory_outside_the_state_directory_is_a_protected_place() {
    let scratch = Scratch::new("rp");
    let state_home = scratch.0.join("s".repeat(120));
    let tmp = scratch.0.join("t");
    agent::private_fs::create_private_dir_all(&tmp, &scratch.0).unwrap();
    let paths = run_dir::Paths::new("t", &scratch.0.join("d"), &state_home, &tmp).unwrap();
    assert!(!paths.run.starts_with(&paths.state), "{}", paths.run.display());
    let places = gate::surroundings::helper_places(&paths);
    assert!(places.contains(&paths.run));
    assert!(places.contains(&paths.state) && places.contains(&paths.data));
}
