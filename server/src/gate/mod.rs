//! Path facts for a session's gate requests, computed on this server's disk.
//!
//! For each permission request a session's gate is waiting on, the helper runs the disk half of Eitri's permission
//! classifier (`agent::permission_policy::gate_fact`) here, where the files are, and holds the answer in memory beside
//! the session's Pending requests, never in them: a fact writes no journal frame, changes no pending request or
//! watermark, answers nothing and is sent nowhere but to a laptop that asks. A restarted daemon has none and computes
//! them again for every request still pending when it adopts the session. A laptop asks for a fresh fact, computed
//! at that moment, before any answer it gives without the person; that one, not the first, is what such an answer
//! rests on.
//!
//! Every fact is computed off the runtime, on the blocking pool, within [`GATE_FACT_TIMEOUT`]: a stalled mount can
//! block a single system call for longer than any work budget, so past the deadline the fact is a refusal. At most
//! [`WORKERS`] computations run at once, counted until their thread really returns, so stalled ones cannot take the
//! threads the journal needs.

pub mod surroundings;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use agent::permission_policy::{gate_fact, is_gate_request, request_digest, request_input, Classification, GateFact};
use claude_runtime_protocol::v1::{session_event::Event, PermissionRequested, SessionEvent};
use eitri_server::journal::Pending;
use eitri_server_proto::{
    GateFactMsg, GateRequestFact, GateRootFact, GateVerdict, GATE_FACTS_HELD_MAX, GATE_FACT_TIMEOUT,
};
use prost::Message;
use serde_json::Value;

pub use surroundings::{Scene, StartedEnv};

/// The most fact computations that may hold a blocking thread at once, across the daemon.
pub const WORKERS: usize = 8;

/// Why a request has no fact: its computation did not finish in time.
pub const REFUSAL_TIMED_OUT: &str = "the server's path check did not finish within its time limit";
/// Why a request has no fact: its computation failed.
pub const REFUSAL_FAILED: &str = "the server's path check failed";
/// Why a request has no fact: more requests are pending than the helper computes facts for.
pub const REFUSAL_TOO_MANY: &str = "more requests are pending than the server checks";
/// Why a request has no fact: the helper does not know the environment this session's agent was started in.
pub const REFUSAL_UNKNOWN_START: &str = "the server does not know the environment this agent was started in";
/// Why a fresh check is refused: the request is answered, or its turn or session ended.
pub const REFUSAL_NOT_PENDING: &str = "no longer pending";
/// Why a fresh check is refused: the digest names another request than the one pending under that id.
pub const REFUSAL_DIGEST: &str = "the request digest does not match the pending request";
/// Why a fresh check is refused: the request is the CLI's own prompt, which the gate does not judge.
pub const REFUSAL_NOT_A_GATE_REQUEST: &str = "this request is the CLI's own prompt, which has no path fact";
/// Why a fresh check is refused: the pending request's bytes could not be read as a permission request.
pub const REFUSAL_UNREADABLE: &str = "the pending request could not be read";
/// Why a fresh check has no first fact: none was held for the request.
pub const REFUSAL_NO_FIRST: &str = "the server holds no fact for this request";

/// The disk half as the helper runs it; tests put a slow or counting one in its place.
pub type Classify = Arc<dyn Fn(&str, &Value, &Scene) -> GateFact + Send + Sync>;

/// How the daemon computes facts: the classifier, how long one may take, how many may run at once, and how many
/// computed facts one session holds.
pub struct Engine {
    classify: Classify,
    timeout: Duration,
    held_max: usize,
    workers: Arc<tokio::sync::Semaphore>,
    /// The helper's state directory, whose audit log records that every worker was busy.
    audit: OnceLock<PathBuf>,
    /// Every worker was busy the last time a computation waited for one; recorded once until one is free again.
    saturated: AtomicBool,
}

impl Engine {
    /// The disk half of Eitri's classifier, within [`GATE_FACT_TIMEOUT`].
    pub fn standard() -> Arc<Engine> {
        Engine::new(
            Arc::new(|tool: &str, input: &Value, scene: &Scene| {
                gate_fact(tool, input, &scene.root, &scene.around, &scene.places)
            }),
            GATE_FACT_TIMEOUT,
        )
    }

    pub fn new(classify: Classify, timeout: Duration) -> Arc<Engine> {
        Engine::holding(classify, timeout, GATE_FACTS_HELD_MAX)
    }

    /// An engine whose sessions each hold computed facts for at most `held_max` requests.
    pub fn holding(classify: Classify, timeout: Duration, held_max: usize) -> Arc<Engine> {
        Arc::new(Engine {
            classify,
            timeout,
            held_max,
            workers: Arc::new(tokio::sync::Semaphore::new(WORKERS)),
            audit: OnceLock::new(),
            saturated: AtomicBool::new(false),
        })
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Records in the audit log under `state` when every worker is busy.
    pub fn audit_into(&self, state: &Path) {
        let _ = self.audit.set(state.to_path_buf());
    }

    /// Runs `work` on the blocking pool under a worker permit, within the timeout from now. The permit goes with the
    /// thread, so one that stalls past the deadline still counts until it returns.
    async fn bounded<T: Send + 'static>(&self, work: impl FnOnce() -> T + Send + 'static) -> Result<T, &'static str> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        let permit = match tokio::time::timeout_at(deadline, self.workers.clone().acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return Err(REFUSAL_FAILED),
            Err(_) => {
                self.all_busy();
                return Err(REFUSAL_TIMED_OUT);
            }
        };
        self.saturated.store(false, Ordering::Relaxed);
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work()
        });
        match tokio::time::timeout_at(deadline, task).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(REFUSAL_FAILED),
            Err(_) => Err(REFUSAL_TIMED_OUT),
        }
    }

    /// Every worker stayed busy for a whole timeout, so facts are refusals until one returns: computations stalled
    /// on the disk hold them all. Audited once until a worker is free again.
    fn all_busy(&self) {
        if self.saturated.swap(true, Ordering::Relaxed) {
            return;
        }
        if let Some(state) = self.audit.get().cloned() {
            let fields = format!("workers={WORKERS} timeout_ms={}", self.timeout.as_millis());
            tokio::task::spawn_blocking(move || crate::audit::record(&state, "gate-facts-workers-busy", &fields));
        }
    }

    /// The fact for `asked` in `scene`, computed now, or why there is none.
    pub async fn fact(&self, asked: &Asked, session_id: &str, scene: &Arc<Scene>) -> Result<GateFactMsg, &'static str> {
        let (classify, scene, asked, session_id) = (
            self.classify.clone(),
            scene.clone(),
            asked.clone(),
            session_id.to_owned(),
        );
        // The digest hashes the whole input, so it is taken off the runtime with the classification.
        let (fact, digest) = self
            .bounded(move || {
                let digest = asked.digest(&session_id, &scene.root);
                let input = request_input(&asked.input_json, |_| {});
                (classify(&asked.tool_name, &input, &scene), digest)
            })
            .await?;
        Ok(message(&fact, digest))
    }

    /// Whether `scene`'s root is a project boundary here, and when not, why. A check that does not finish in time,
    /// or a scene that is not known, says no boundary with no reason named.
    pub async fn root(&self, scene: Result<Arc<Scene>, &'static str>) -> GateRootFact {
        let Ok(scene) = scene else {
            return GateRootFact::default();
        };
        let checked = self
            .bounded(move || {
                agent::project_boundary::refusal(&scene.root, scene.around.home.as_deref(), &scene.around.identity)
            })
            .await;
        match checked {
            Ok(None) => GateRootFact {
                is_boundary: true,
                not_a_boundary: String::new(),
            },
            Ok(Some(refused)) => GateRootFact {
                is_boundary: false,
                not_a_boundary: refused.name().to_owned(),
            },
            Err(_) => GateRootFact::default(),
        }
    }
}

fn verdict(classification: &Classification) -> GateVerdict {
    GateVerdict {
        passes: !classification.needs_a_human(),
        why: classification.reason.to_owned(),
    }
}

/// The wire form of a fact about the request `digest` names.
pub fn message(fact: &GateFact, digest: [u8; 32]) -> GateFactMsg {
    GateFactMsg {
        root_is_boundary: fact.root_is_boundary,
        places_card: fact.places_card.as_ref().map(verdict),
        classification: Some(verdict(&fact.classification)),
        request_digest: digest.to_vec(),
        computed_unix_ms: crate::run_dir::unix_ms(),
    }
}

/// A gate request as the helper holds it: the event's own fields, untouched.
#[derive(Clone, Debug)]
pub struct Asked {
    pub permission_id: String,
    pub sequence: u64,
    pub tool_name: String,
    pub input_json: String,
}

impl Asked {
    /// The request at `sequence`, or `None` for one the gate did not raise: the CLI's own prompt, or an origin this
    /// helper does not know, which the laptop treats as the CLI's prompt and never judges by path.
    pub fn of(sequence: u64, request: &PermissionRequested) -> Option<Asked> {
        is_gate_request(request.origin).then(|| Asked {
            permission_id: request.permission_id.clone(),
            sequence,
            tool_name: request.tool_name.clone(),
            input_json: request.input_json.clone(),
        })
    }

    /// What a fact about this request names it by, in `session_id` at `root`.
    pub fn digest(&self, session_id: &str, root: &Path) -> [u8; 32] {
        request_digest(
            session_id,
            &self.permission_id,
            &self.tool_name,
            self.input_json.as_bytes(),
            root,
        )
    }
}

/// A computation to run for one request, made where the request was first held.
pub struct Job {
    asked: Asked,
    engine: Arc<Engine>,
    scene: Arc<Scene>,
}

#[derive(Clone)]
enum Slot {
    Computing {
        sequence: u64,
    },
    /// `generation` orders the done facts by when they were done, so a stream sends each once.
    Done {
        fact: GateRequestFact,
        generation: u64,
    },
}

impl Slot {
    /// Whether the slot counts against the engine's `held_max`: a computation, or a fact computed. A refusal fact
    /// holds nothing but its words.
    fn holds(&self) -> bool {
        !matches!(self, Slot::Done { fact, .. } if fact.fact.is_none())
    }
}

#[derive(Default)]
struct Held {
    slots: HashMap<String, Slot>,
    /// The id of each done fact, by its generation.
    done: BTreeMap<u64, String>,
    /// The last generation given.
    generation: u64,
    /// How many slots hold a computation or a computed fact.
    holding: usize,
    closed: bool,
}

impl Held {
    fn insert(&mut self, id: String, slot: Slot) {
        self.holding += usize::from(slot.holds());
        if let Some(old) = self.slots.insert(id, slot) {
            self.forget(&old);
        }
    }

    fn remove(&mut self, id: &str) {
        if let Some(old) = self.slots.remove(id) {
            self.forget(&old);
        }
    }

    fn forget(&mut self, old: &Slot) {
        self.holding -= usize::from(old.holds());
        if let Slot::Done { generation, .. } = old {
            self.done.remove(generation);
        }
    }

    fn finish(&mut self, id: String, fact: GateRequestFact) {
        self.generation += 1;
        self.done.insert(self.generation, id.clone());
        let generation = self.generation;
        self.insert(id, Slot::Done { fact, generation });
    }

    fn clear(&mut self) {
        self.slots.clear();
        self.done.clear();
        self.holding = 0;
    }

    /// The fact done at `generation`, if it is still held.
    fn done_at(&self, generation: u64) -> Option<GateRequestFact> {
        match self.slots.get(self.done.get(&generation)?) {
            Some(Slot::Done { fact, generation: at }) if *at == generation => Some(fact.clone()),
            _ => None,
        }
    }
}

/// How the session computes facts, set once it is observed with a running sidecar.
#[derive(Clone)]
struct Ready {
    engine: Arc<Engine>,
    /// The session's scene, or why its requests get a refusal fact instead.
    scene: Result<Arc<Scene>, &'static str>,
}

/// One session's facts, in memory. Changed by the watch loop (under the session lock, so they follow Pending) and by
/// the computations it spawns; read by the laptop's calls. Its locks are taken under the session lock, so a panic
/// while one was held does not stop the watch: what they guard is replaced whole or not at all.
pub struct HeldFacts {
    session_id: String,
    ready: Mutex<Option<Ready>>,
    held: Mutex<Held>,
    changed: tokio::sync::watch::Sender<u64>,
}

impl HeldFacts {
    /// The facts of the session `session_id`; one already closed holds none and its streams end at once.
    pub fn new(session_id: &str, closed: bool) -> HeldFacts {
        HeldFacts {
            session_id: session_id.to_owned(),
            ready: Mutex::new(None),
            held: Mutex::new(Held {
                closed,
                ..Held::default()
            }),
            changed: tokio::sync::watch::channel(0).0,
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    fn held(&self) -> MutexGuard<'_, Held> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn ready(&self) -> MutexGuard<'_, Option<Ready>> {
        self.ready.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes the session's facts computable with `engine` in `scene` (an error: the reason every request gets a
    /// refusal fact, such as a sidecar start environment the helper does not know).
    pub fn start(&self, engine: Arc<Engine>, scene: Result<Arc<Scene>, &'static str>) {
        *self.ready() = Some(Ready { engine, scene });
    }

    /// The engine and the scene, once the session's facts are computable.
    pub fn engine_and_scene(&self) -> Option<(Arc<Engine>, Result<Arc<Scene>, &'static str>)> {
        self.ready().clone().map(|ready| (ready.engine, ready.scene))
    }

    fn bump(&self) {
        self.changed
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    /// Follows `event`, whose effect Pending has just taken: a gate request Pending now holds gets a computation
    /// (returned, for the caller to spawn), an answer drops its request's fact, a turn's end or the session's close
    /// drops them all. Called under the session lock, so the facts held always belong to requests Pending holds.
    pub fn follow(&self, event: &SessionEvent, pending: &Pending) -> Option<Job> {
        match &event.event {
            Some(Event::PermissionRequested(request)) => {
                // A request Pending refused (its id already pending under another event) changes nothing here.
                if pending.held(&request.permission_id).map(|(sequence, _)| sequence) != Some(event.sequence) {
                    return None;
                }
                self.requested(event.sequence, request)
            }
            Some(Event::PermissionResolved(resolved)) => {
                self.held().remove(&resolved.permission_id);
                self.bump();
                None
            }
            Some(Event::TurnCompleted(_) | Event::SessionClosed(_)) => {
                self.held().clear();
                self.bump();
                None
            }
            _ => None,
        }
    }

    /// Drops every fact and ends the session's streams: it closed, or the helper no longer follows it.
    pub fn close(&self) {
        let mut held = self.held();
        held.clear();
        held.closed = true;
        drop(held);
        self.bump();
    }

    /// Computations for every gate request Pending holds, when the session is adopted: none survive a restart.
    pub fn adopt(&self, pending: &Pending) -> Vec<Job> {
        pending
            .list()
            .into_iter()
            .filter_map(|(sequence, bytes)| {
                let event = SessionEvent::decode(bytes.as_slice()).ok()?;
                match event.event {
                    Some(Event::PermissionRequested(request)) => self.requested(sequence, &request),
                    _ => None,
                }
            })
            .collect()
    }

    fn requested(&self, sequence: u64, request: &PermissionRequested) -> Option<Job> {
        let id = request.permission_id.clone();
        let mut held = self.held();
        let Some(asked) = Asked::of(sequence, request) else {
            // The CLI's own prompt has no fact. Pending holding this id at this sequence means the earlier request
            // under it was answered or cleared, which dropped its fact; this only guards facts and Pending drifting
            // apart.
            if held.slots.contains_key(&id) {
                held.remove(&id);
                drop(held);
                self.bump();
            }
            return None;
        };
        let others = held.holding - held.slots.get(&id).map_or(0, |slot| usize::from(slot.holds()));
        let job = match self.ready().clone() {
            Some(Ready {
                engine,
                scene: Ok(scene),
            }) if others < engine.held_max => Ok(Job { asked, engine, scene }),
            Some(Ready { scene: Ok(_), .. }) => Err(REFUSAL_TOO_MANY),
            Some(Ready { scene: Err(why), .. }) => Err(why),
            None => Err(REFUSAL_UNKNOWN_START),
        };
        let job = match job {
            Ok(job) => {
                held.insert(id, Slot::Computing { sequence });
                Some(job)
            }
            Err(why) => {
                let fact = refusal_fact(&id, sequence, why);
                held.finish(id, fact);
                None
            }
        };
        drop(held);
        self.bump();
        job
    }

    /// Runs `job` and keeps its fact, unless its request was answered, cleared or replaced meanwhile: the fact is
    /// kept only while the slot still waits for this computation (the same id at the same sequence).
    pub async fn run(self: Arc<Self>, job: Job) {
        let Job { asked, engine, scene } = job;
        let computed = engine.fact(&asked, &self.session_id, &scene).await;
        let fact = match computed {
            Ok(fact) => GateRequestFact {
                permission_id: asked.permission_id.clone(),
                sequence: asked.sequence,
                fact: Some(fact),
                refusal: String::new(),
            },
            Err(refusal) => refusal_fact(&asked.permission_id, asked.sequence, refusal),
        };
        let mut held = self.held();
        match held.slots.get(&asked.permission_id) {
            Some(Slot::Computing { sequence }) if *sequence == asked.sequence => held.finish(asked.permission_id, fact),
            _ => return,
        }
        drop(held);
        self.bump();
    }

    /// A receiver told of every change.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// The generations of the facts done now, by their requests' sequence, and the cursor after the latest.
    pub fn backlog(&self) -> (Vec<u64>, u64) {
        let held = self.held();
        let mut done: Vec<(u64, u64)> = held
            .slots
            .values()
            .filter_map(|slot| match slot {
                Slot::Done { fact, generation } => Some((fact.sequence, *generation)),
                Slot::Computing { .. } => None,
            })
            .collect();
        done.sort_unstable();
        (
            done.into_iter().map(|(_, generation)| generation).collect(),
            held.generation,
        )
    }

    /// The fact done at `generation`, if it is still held.
    pub fn done_at(&self, generation: u64) -> Option<GateRequestFact> {
        self.held().done_at(generation)
    }

    /// The first fact done after the cursor `after` that is still held, with its generation.
    pub fn next_after(&self, after: u64) -> Option<(u64, GateRequestFact)> {
        let held = self.held();
        let (generation, _) = held.done.range(after + 1..).next()?;
        held.done_at(*generation).map(|fact| (*generation, fact))
    }

    /// Whether the session has closed, or is no longer followed.
    pub fn is_closed(&self) -> bool {
        self.held().closed
    }

    /// The fact held for the request `permission_id` at `sequence`, waiting at most `within` while it is computed.
    pub async fn first(&self, permission_id: &str, sequence: u64, within: Duration) -> Result<GateFactMsg, String> {
        let mut changed = self.subscribe();
        let deadline = tokio::time::Instant::now() + within;
        loop {
            changed.borrow_and_update();
            let slot = self.held().slots.get(permission_id).cloned();
            match slot {
                Some(Slot::Done { fact, .. }) if fact.sequence == sequence => {
                    return fact.fact.ok_or(fact.refusal);
                }
                Some(Slot::Computing { sequence: computing }) if computing == sequence => {}
                _ => return Err(REFUSAL_NO_FIRST.to_owned()),
            }
            match tokio::time::timeout_at(deadline, changed.changed()).await {
                Ok(Ok(())) => {}
                _ => return Err(REFUSAL_TIMED_OUT.to_owned()),
            }
        }
    }
}

fn refusal_fact(permission_id: &str, sequence: u64, refusal: &str) -> GateRequestFact {
    GateRequestFact {
        permission_id: permission_id.to_owned(),
        sequence,
        fact: None,
        refusal: refusal.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_runtime_protocol::v1::PermissionOrigin;

    #[test]
    fn a_poisoned_lock_does_not_stop_the_facts() {
        let facts = Arc::new(HeldFacts::new("session", false));
        let poisoning = facts.clone();
        std::thread::spawn(move || {
            let _held = poisoning.held.lock().unwrap();
            let _ready = poisoning.ready.lock().unwrap();
            panic!("poisons both locks");
        })
        .join()
        .unwrap_err();
        assert!(facts.held.is_poisoned() && facts.ready.is_poisoned());
        assert_eq!(facts.engine_and_scene().map(|_| ()), None);
        facts.start(Engine::standard(), Err(REFUSAL_UNKNOWN_START));
        let request = PermissionRequested {
            permission_id: "p".into(),
            origin: PermissionOrigin::Hook as i32,
            ..Default::default()
        };
        assert!(facts.requested(1, &request).is_none());
        let (backlog, cursor) = facts.backlog();
        assert_eq!(backlog, [cursor]);
        assert_eq!(facts.done_at(cursor).unwrap().refusal, REFUSAL_UNKNOWN_START);
        facts.close();
        assert!(facts.is_closed());
        assert_eq!(facts.next_after(0).map(|(generation, _)| generation), None);
    }
}
