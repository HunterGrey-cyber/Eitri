//! The window's trust gate: whether a session about to start may load the project's own
//! configuration for its agent (Claude's project and local setting tiers, Codex's project layers, a
//! registered agent's own files), and the question that decides it.
//!
//! One gate per window, over the window's one project root, with each agent's configuration kept
//! apart: every check, answer, record and memory of an answer belongs to one discovery profile, so a
//! `y` for one agent never lets another load anything, and two tabs of two agents get two questions.
//!
//! Every disk operation of the trust check -- discovering and fingerprinting the configuration,
//! reading Eitri's record of it, writing that record and removing it -- runs on the gate's own worker
//! thread, in the order it was asked for, so a slow or hostile state directory (a FIFO, a network
//! home) can never freeze the window.
//! The GTK thread only queues jobs and drains their results from the panel's tick ([`TrustGate::poll`]
//! never blocks), and everything it reads ([`TrustGate::row`], [`TrustGate::route`]) is the latest
//! result already in memory.
//!
//! An answer is bound to what its prompt showed: the gate keeps, per tab, the discovery it built the
//! prompt from, and accepts a `y` or `n` only when the page echoes that prompt's fingerprint and
//! digest and the latest check still has that fingerprint. A `y` is taken only after the worker found
//! the disk unchanged since the prompt, and one that needs a record writes the discovery that was
//! shown: neither a check started for another tab nor an edit made while the prompt was being read
//! can put a configuration nobody saw under the user's `y`.
//!
//! A configuration that could not be hashed entirely (a file too large or unreadable, a FIFO, a link
//! where the CLI reads a file, a walk over its budget) can change while its fingerprint stays the
//! same, so an answer about it covers one start: it is never recorded and the window does not
//! remember it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use agent::setting_sources::ProjectTrust;
use eitri_core::agent_bridge::{
    TrustAction, TrustDiffView, TrustItemView, TrustPromptState, TrustPromptView, TrustRememberView,
};
use eitri_core::project_trust::{
    civil_date, Discovery, Finding, Fingerprint, Profile, ProfileEnv, Remember, TrustState, TrustStore, UserCodexConfig,
};
use eitri_core::tabs::TabId;

/// Said once per window, after the first `n`.
pub(crate) const NOT_LOADED: &str = "this project's own Claude configuration is not loaded";
/// An answer whose echo is not what its prompt showed, or whose fingerprint is no longer the latest.
pub(crate) const OUT_OF_DATE: &str = "that prompt was out of date; asking again with what is on disk now";
/// The worker found the configuration changed between the prompt and the record.
pub(crate) const CHANGED_WHILE_READING: &str = "the configuration changed while you were reading it; asking again";
/// A `y` whose record was overtaken by a later `:untrust`.
pub(crate) const WITHDRAWN: &str = "trust was withdrawn by :untrust; asking again";
/// An answer for a prompt that is not the one on screen.
pub(crate) const NOT_CURRENT: &str = "that prompt is no longer current";
/// `Escape` on a resume or a restore.
pub(crate) const PUT_OFF: &str = "not started: the trust question was put off";
/// `Escape` on a first message, which goes back to the composer.
pub(crate) const NOT_STARTED: &str = "not started";
/// `:trust`'s prompt was overtaken by a newer check.
pub(crate) const COMMAND_OUT_OF_DATE: &str =
    "that prompt was out of date; type :trust again to see what is on disk now";
/// `:trust` answered `n`, put off, or overtaken.
pub(crate) const NOT_TRUSTED: &str = "not trusted";
pub(crate) const TRUSTED_FROM_NOW: &str =
    "trusted: sessions started from now on load this project's Claude configuration; running sessions are unchanged";
pub(crate) const NOTHING_HERE: &str = "nothing to trust here";
pub(crate) const UNTRUSTED_FROM_NOW: &str = "not trusted: sessions started from now on leave this project's Claude \
                                             configuration out; running sessions are unchanged";
pub(crate) const WAS_NOT_TRUSTED: &str = "was not trusted";

/// [`NOT_LOADED`] for any agent.
pub(crate) fn not_loaded(profile: &Profile) -> String {
    match profile {
        Profile::Claude => NOT_LOADED.to_string(),
        other => format!("this project's own {} configuration is not loaded", other.label()),
    }
}

/// [`TRUSTED_FROM_NOW`] for any agent.
pub(crate) fn trusted_from_now(profile: &Profile) -> String {
    match profile {
        Profile::Claude => TRUSTED_FROM_NOW.to_string(),
        other => format!(
            "trusted: {} sessions started from now on load this project's {} configuration; running sessions \
             are unchanged",
            other.label(),
            other.label()
        ),
    }
}

/// [`UNTRUSTED_FROM_NOW`] for any agent.
pub(crate) fn untrusted_from_now(profile: &Profile) -> String {
    match profile {
        Profile::Claude => UNTRUSTED_FROM_NOW.to_string(),
        other => format!(
            "not trusted: {} sessions started from now on leave this project's {} configuration out; running \
             sessions are unchanged",
            other.label(),
            other.label()
        ),
    }
}

/// An `n` for an agent that cannot be started without loading the project's own configuration.
pub(crate) fn refused_without_yes(profile: &Profile) -> String {
    format!(
        "{} would load this project's own configuration, which you have not trusted; Eitri cannot start it \
         without loading that",
        profile.label()
    )
}

pub(crate) fn window_only_notice(why: &str) -> String {
    format!("trusted for this window only: {why}")
}

pub(crate) fn session_only_notice(discovery: &Discovery) -> String {
    format!(
        "trusted for this session only: {} cannot be checked",
        uncheckable_names(discovery)
    )
}

pub(crate) fn record_failed_notice(why: &str) -> String {
    format!("trust could not be recorded ({why}); asking again for this window only")
}

fn cannot_remember_notice(discovery: &Discovery) -> String {
    format!(
        "cannot be remembered: {} cannot be checked; each session start asks",
        uncheckable_names(discovery)
    )
}

/// The files nobody could check, by name; the budget alone when it was the only gap.
fn uncheckable_names(discovery: &Discovery) -> String {
    let names: Vec<String> = discovery
        .uncheckable()
        .into_iter()
        .map(|(path, _)| path.display().to_string())
        .collect();
    if names.is_empty() {
        "part of the configuration".to_string()
    } else {
        names.join(", ")
    }
}

/// Where a check sent a waiting tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    /// Start now, with these tiers.
    Start(ProjectTrust),
    /// Ask the question first.
    Ask,
}

/// What the gate tells the panel, drained by [`TrustGate::poll`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateEvent {
    /// A waiting tab's check came back.
    Route(TabId, Route),
    /// A newer check no longer has the fingerprint this tab's prompt showed: the prompt is gone and
    /// the tab is asked again.
    Reask(TabId),
    /// This tab's `y` is taken: its record is written, or, for a `y` that cannot outlive this window
    /// or this start, the disk was found unchanged since the prompt. `notice`, when there is one, is
    /// said in the panel once the session starts.
    Recorded { tab: TabId, notice: Option<String> },
    /// Nothing was written for this tab's `y` (the disk changed since the prompt, the write failed,
    /// or a later `:untrust` overtook it); the tab is asked again.
    RecordRefused { tab: TabId, notice: String },
    /// `:trust` found something to trust: the panel shows its prompt in `tab`, the tab whose `:` line
    /// asked, if that tab is still on screen.
    CommandAsk { request_id: String, tab: TabId },
    /// `:trust` or `:untrust` is answered. `notice`, when there is one, is said in the panel.
    CommandDone {
        request_id: String,
        ok: bool,
        notice: Option<String>,
    },
}

/// What [`TrustGate::answer`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AnswerOutcome {
    /// Start the tab's session now, with these tiers, saying `notice` if there is one. Only an `n`
    /// starts at once: it loads nothing, so what is on disk now cannot matter to it.
    Start(ProjectTrust, Option<String>),
    /// A `y`: the worker looks at the disk again (and writes the record, when one can be kept); the
    /// session starts on [`GateEvent::Recorded`].
    Recording,
    /// Not taken; the tab is asked again with what is on disk now.
    Refused(String),
    /// An `n` for an agent that has no way to start without the project's own configuration: the
    /// start is dropped, saying why.
    NotStarted(String),
}

/// What [`TrustGate::answer_command`] decided about `:trust`'s own prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommandAnswer {
    /// `:trust` is answered now.
    Done {
        request_id: String,
        ok: bool,
        notice: Option<String>,
    },
    /// The worker reads the disk again (and writes the record, when one can be kept); `:trust` is
    /// answered by a later [`GateEvent::CommandDone`].
    Recording,
}

/// What a tab's prompt showed: an answer must name exactly this.
struct Shown {
    nonce: u64,
    fingerprint: Fingerprint,
    findings_digest: String,
    discovery: Discovery,
}

/// This window's own answer, kept against the fingerprint it was given for.
#[derive(Debug, Clone)]
struct Answered {
    fingerprint: Fingerprint,
    trust: ProjectTrust,
    /// Why a `y` could not be recorded, for a window-only trust.
    window_only: Option<String>,
}

/// `:trust`'s prompt, on screen in `tab`.
struct CommandShown {
    request_id: String,
    tab: TabId,
    shown: Shown,
}

/// Where a record's result goes.
#[derive(Debug, Clone)]
enum Purpose {
    ForTab(TabId),
    Command(String),
}

enum Job {
    Check {
        generation: u64,
        profile: Profile,
    },
    Record {
        purpose: Purpose,
        shown: Box<Discovery>,
        epoch: u64,
        now: u64,
    },
    /// A `y` that writes nothing: the disk is read again and must still be what was shown.
    Confirm {
        purpose: Purpose,
        shown: Box<Discovery>,
        epoch: u64,
        kept: Kept,
    },
    CommandCheck {
        request_id: String,
        tab: TabId,
        profile: Profile,
    },
    Forget {
        request_id: String,
        had_window_trust: bool,
        profile: Profile,
    },
}

enum Done {
    Checked {
        generation: u64,
        state: TrustState,
    },
    Recorded {
        purpose: Purpose,
        epoch: u64,
        state: TrustState,
    },
    Confirmed {
        purpose: Purpose,
        epoch: u64,
        kept: Kept,
        state: TrustState,
    },
    Moved {
        purpose: Purpose,
        state: TrustState,
    },
    RecordFailed {
        purpose: Purpose,
        profile: ProfileKey,
        reason: String,
    },
    CommandChecked {
        request_id: String,
        tab: TabId,
        state: TrustState,
    },
    Forgot {
        request_id: String,
        ok: bool,
        notice: String,
        state: TrustState,
    },
}

/// How long a `y` that writes no record lasts, and what the panel says once it is taken.
#[derive(Debug, Clone)]
struct Kept {
    /// `Some(why)`: the window remembers the answer, the record having been impossible for `why`.
    /// `None`: it covers this one start.
    window_only: Option<String>,
    notice: String,
}

/// Nonces of `:trust`'s own prompts start here, far from the tab set's, so the two can never be
/// mistaken for each other.
const COMMAND_NONCE_BASE: u64 = 1 << 48;

/// A profile's name as the gate's maps key it ([`Profile::key`]).
type ProfileKey = String;

pub(crate) struct TrustGate {
    #[cfg_attr(not(test), allow(dead_code))]
    root: PathBuf,
    jobs: Option<mpsc::Sender<Job>>,
    done: mpsc::Receiver<Done>,
    /// The last check queued, of any profile.
    generation: u64,
    /// Moved by `:untrust` of a profile: a record of it queued before is not trust any more when it
    /// lands.
    epochs: HashMap<ProfileKey, u64>,
    /// Per profile, the newest result of any check of it, in the order they were queued.
    latest: HashMap<ProfileKey, TrustState>,
    /// What each tab's question showed; the discovery it holds names the question's profile.
    shown: HashMap<TabId, Shown>,
    /// This window's own answer per profile.
    answered: HashMap<ProfileKey, Answered>,
    /// Per profile, why its last record could not be written: its later prompts say this window
    /// only. Each profile has a record file of its own, so one that cannot be written says nothing
    /// about another's.
    record_failure: HashMap<ProfileKey, String>,
    /// The profiles whose "not loaded" notice has been said.
    said_not_loaded: std::collections::HashSet<ProfileKey>,
    /// Tabs waiting for a check, each with the profile and the generation of the check that answers
    /// it.
    waiting: Vec<(TabId, ProfileKey, u64)>,
    /// `:trust` asked for a question: its request, the tab whose `:` line asked, and the profile.
    command_pending: Option<(String, TabId, ProfileKey)>,
    command_shown: Option<CommandShown>,
    next_command_nonce: u64,
    /// Results made here rather than by the worker, delivered by the next `poll`.
    owed: Vec<GateEvent>,
}

impl TrustGate {
    /// The gate for `root` (canonical), starting the window's worker, which alone owns `store`.
    /// Touches no disk here.
    pub(crate) fn new(store: TrustStore, root: PathBuf, home: Option<PathBuf>) -> TrustGate {
        TrustGate::with_wait(store, root, home, Box::new(|| {}))
    }

    /// A gate whose worker waits on `hold` before each job, so a test decides how slow the disk is.
    #[cfg(test)]
    pub(crate) fn new_held(store: TrustStore, root: PathBuf, home: Option<PathBuf>, hold: WorkerHold) -> TrustGate {
        TrustGate::with_wait(store, root, home, Box::new(move || hold.wait()))
    }

    /// The worker is gone from here on, as when its thread has died: what it would answer is made by
    /// [`TrustGate::queue`], with every earlier check's result still held.
    #[cfg(test)]
    pub(crate) fn drop_worker(&mut self) {
        self.jobs = None;
    }

    /// A gate with no worker: the test reads the jobs it queues and hands it the results itself.
    #[cfg(test)]
    fn detached(root: PathBuf) -> (TrustGate, mpsc::Receiver<Job>, mpsc::Sender<Done>) {
        let (jobs_tx, jobs_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let gate = TrustGate {
            root,
            jobs: Some(jobs_tx),
            done: done_rx,
            generation: 0,
            epochs: HashMap::new(),
            latest: HashMap::new(),
            shown: HashMap::new(),
            answered: HashMap::new(),
            record_failure: HashMap::new(),
            said_not_loaded: Default::default(),
            waiting: Vec::new(),
            command_pending: None,
            command_shown: None,
            next_command_nonce: COMMAND_NONCE_BASE,
            owed: Vec::new(),
        };
        (gate, jobs_rx, done_tx)
    }

    /// A gate whose worker never started, as when the thread cannot be spawned: every check sends
    /// its tabs on without the project's tiers, and every record or command fails. Touches no disk.
    #[cfg(test)]
    pub(crate) fn without_worker(root: PathBuf) -> TrustGate {
        let (mut gate, _jobs, _done) = TrustGate::detached(root);
        gate.jobs = None;
        gate
    }

    fn with_wait(
        store: TrustStore,
        root: PathBuf,
        home: Option<PathBuf>,
        before_each: Box<dyn Fn() + Send>,
    ) -> TrustGate {
        let (jobs_tx, jobs_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker_root = root.clone();
        // Read here, where no disk is touched: the worker resolves it.
        let codex_home = std::env::var_os("CODEX_HOME").map(PathBuf::from);
        let started = std::thread::Builder::new()
            .name("eitri-trust".into())
            .spawn(move || run_worker(store, worker_root, home, codex_home, jobs_rx, done_tx, before_each));
        let jobs = match started {
            Ok(_) => Some(jobs_tx),
            Err(e) => {
                eprintln!("[trust] the trust check could not start ({e}); sessions start without the project's own configuration");
                None
            }
        };
        TrustGate {
            root,
            jobs,
            done: done_rx,
            generation: 0,
            epochs: HashMap::new(),
            latest: HashMap::new(),
            shown: HashMap::new(),
            answered: HashMap::new(),
            record_failure: HashMap::new(),
            said_not_loaded: Default::default(),
            waiting: Vec::new(),
            command_pending: None,
            command_shown: None,
            next_command_nonce: COMMAND_NONCE_BASE,
            owed: Vec::new(),
        }
    }

    /// The root this gate is about.
    #[cfg(test)]
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Hands `job` to the worker. With no worker, what it would have answered is made here, failing
    /// closed: a check routes its tabs to start without the project's tiers, and a record or a
    /// command fails.
    fn queue(&mut self, job: Job) {
        let job = match self.jobs.as_ref() {
            Some(jobs) => match jobs.send(job) {
                Ok(()) => return,
                Err(mpsc::SendError(job)) => job,
            },
            None => job,
        };
        const NO_WORKER: &str = "the trust check is not running";
        match job {
            Job::Check { generation, profile } => {
                let key = profile.key();
                let (resolved, kept): (Vec<_>, Vec<_>) = self
                    .waiting
                    .drain(..)
                    .partition(|(_, p, g)| *p == key && *g <= generation);
                self.waiting = kept;
                // Started untrusted. An agent that needs a `y` is refused where external starts begin,
                // which takes no untrusted start of one, whatever discovery an earlier check left.
                for (tab, _, _) in resolved {
                    self.owed
                        .push(GateEvent::Route(tab, Route::Start(ProjectTrust::Untrusted)));
                }
            }
            Job::Record { purpose, .. } | Job::Confirm { purpose, .. } => match purpose {
                Purpose::ForTab(tab) => self.owed.push(GateEvent::RecordRefused {
                    tab,
                    notice: record_failed_notice(NO_WORKER),
                }),
                Purpose::Command(request_id) => self.owed.push(GateEvent::CommandDone {
                    request_id,
                    ok: false,
                    notice: Some(NO_WORKER.to_string()),
                }),
            },
            Job::CommandCheck { request_id, .. } | Job::Forget { request_id, .. } => {
                self.owed.push(GateEvent::CommandDone {
                    request_id,
                    ok: false,
                    notice: Some(NO_WORKER.to_string()),
                })
            }
        }
    }

    /// Queues a fresh check of the root for `profile`; returns its generation.
    pub(crate) fn begin_check(&mut self, profile: &Profile) -> u64 {
        self.generation += 1;
        let generation = self.generation;
        self.queue(Job::Check {
            generation,
            profile: profile.clone(),
        });
        generation
    }

    /// `tab` waits for the check of `profile` that [`TrustGate::begin_check`] queues next (callers
    /// always call it right after); an older result, or one of another profile, does not resolve it.
    pub(crate) fn wait_for(&mut self, tab: TabId, profile: &Profile) {
        let generation = self.generation + 1;
        self.waiting.retain(|(t, _, _)| *t != tab);
        self.waiting.push((tab, profile.key(), generation));
    }

    pub(crate) fn is_waiting(&self, tab: TabId) -> bool {
        self.waiting.iter().any(|(t, _, _)| *t == tab)
    }

    /// Whether `tab`'s prompt on screen is the one under `nonce`.
    pub(crate) fn is_shown(&self, tab: TabId, nonce: u64) -> bool {
        self.shown.get(&tab).is_some_and(|s| s.nonce == nonce)
    }

    /// Whatever the worker finished, as events. Never blocks.
    ///
    /// Several results can arrive between two polls, and a later one may find the configuration
    /// changed after an earlier one let a tab through. So nothing that would start a session is
    /// decided from an earlier result in the batch: a waiting tab's route is worked out from the
    /// newest result once the whole batch is in, and a taken `y` whose fingerprint the newest result
    /// no longer has is refused and asked again.
    pub(crate) fn poll(&mut self) -> Vec<GateEvent> {
        let mut events = std::mem::take(&mut self.owed);
        // Positions in `events` settled only once the batch is drained, each with its profile.
        let mut routes: Vec<(usize, ProfileKey)> = Vec::new();
        let mut taken: Vec<(usize, ProfileKey, Fingerprint)> = Vec::new();
        while let Ok(done) = self.done.try_recv() {
            match done {
                Done::Checked { generation, state } => {
                    let key = state_profile(&state).key();
                    self.take_latest(state, &mut events);
                    let (resolved, kept): (Vec<_>, Vec<_>) = self
                        .waiting
                        .drain(..)
                        .partition(|(_, p, g)| *p == key && *g <= generation);
                    self.waiting = kept;
                    for (tab, _, _) in resolved {
                        routes.push((events.len(), key.clone()));
                        events.push(GateEvent::Route(tab, Route::Ask));
                    }
                }
                Done::Recorded { purpose, epoch, state } => {
                    let profile = state_profile(&state).clone();
                    let key = profile.key();
                    if epoch == self.epoch_of(&key) {
                        let fingerprint = discovery_of(&state).map(|d| d.fingerprint);
                        if let Some(fingerprint) = fingerprint {
                            self.answered.insert(
                                key.clone(),
                                Answered {
                                    fingerprint,
                                    trust: ProjectTrust::Trusted,
                                    window_only: None,
                                },
                            );
                        }
                        self.record_failure.remove(&key);
                        self.take_latest(state, &mut events);
                        if let (Purpose::ForTab(_), Some(fingerprint)) = (&purpose, fingerprint) {
                            taken.push((events.len(), key, fingerprint));
                        }
                        events.push(match purpose {
                            Purpose::ForTab(tab) => GateEvent::Recorded { tab, notice: None },
                            Purpose::Command(request_id) => GateEvent::CommandDone {
                                request_id,
                                ok: true,
                                notice: Some(trusted_from_now(&profile)),
                            },
                        });
                    } else {
                        // A `:untrust` came after this `y` and its forget runs next: the later
                        // command wins, so nothing here is trust.
                        events.push(refused(purpose, WITHDRAWN.to_string()));
                    }
                }
                Done::Confirmed {
                    purpose,
                    epoch,
                    kept,
                    state,
                } => {
                    let key = state_profile(&state).key();
                    if epoch != self.epoch_of(&key) {
                        events.push(refused(purpose, WITHDRAWN.to_string()));
                        continue;
                    }
                    let fingerprint = discovery_of(&state).map(|d| d.fingerprint);
                    if let (Some(why), Some(fingerprint)) = (kept.window_only.clone(), fingerprint) {
                        self.answered.insert(
                            key.clone(),
                            Answered {
                                fingerprint,
                                trust: ProjectTrust::Trusted,
                                window_only: Some(why),
                            },
                        );
                    }
                    self.take_latest(state, &mut events);
                    if let (Purpose::ForTab(_), Some(fingerprint)) = (&purpose, fingerprint) {
                        taken.push((events.len(), key, fingerprint));
                    }
                    events.push(match purpose {
                        Purpose::ForTab(tab) => GateEvent::Recorded {
                            tab,
                            notice: Some(kept.notice),
                        },
                        Purpose::Command(request_id) => GateEvent::CommandDone {
                            request_id,
                            ok: true,
                            notice: Some(kept.notice),
                        },
                    });
                }
                Done::Moved { purpose, state } => {
                    self.take_latest(state, &mut events);
                    events.push(refused(purpose, CHANGED_WHILE_READING.to_string()));
                }
                Done::RecordFailed {
                    purpose,
                    profile,
                    reason,
                } => {
                    let notice = match &purpose {
                        Purpose::ForTab(_) => record_failed_notice(&reason),
                        Purpose::Command(_) => format!("trust could not be recorded ({reason})"),
                    };
                    self.record_failure.insert(profile, reason);
                    events.push(refused(purpose, notice));
                }
                Done::CommandChecked { request_id, tab, state } => {
                    let key = state_profile(&state).key();
                    self.take_latest(state, &mut events);
                    self.drop_command_prompt_into(&mut events);
                    events.push(self.command_check_result(request_id, tab, key));
                }
                Done::Forgot {
                    request_id,
                    ok,
                    notice,
                    state,
                } => {
                    self.take_latest(state, &mut events);
                    events.push(GateEvent::CommandDone {
                        request_id,
                        ok,
                        notice: Some(notice),
                    });
                }
            }
        }
        for (at, key) in routes {
            let route = self.latest.get(&key).map_or(Route::Ask, |state| self.route(state));
            if let GateEvent::Route(_, r) = &mut events[at] {
                *r = route;
            }
        }
        for (at, key, fingerprint) in taken {
            let newest = self.latest.get(&key).and_then(discovery_of).map(|d| d.fingerprint);
            if newest != Some(fingerprint) {
                if let GateEvent::Recorded { tab, .. } = events[at] {
                    events[at] = GateEvent::RecordRefused {
                        tab,
                        notice: CHANGED_WHILE_READING.to_string(),
                    };
                }
            }
        }
        events
    }

    /// The `:untrust` count of a profile.
    fn epoch_of(&self, key: &str) -> u64 {
        self.epochs.get(key).copied().unwrap_or(0)
    }

    /// A newer result for its profile: every prompt of that profile that no longer shows its
    /// fingerprint goes, and is asked again. Other profiles' prompts are about other files.
    fn take_latest(&mut self, state: TrustState, events: &mut Vec<GateEvent>) {
        let key = state_profile(&state).key();
        let fingerprint = discovery_of(&state).map(|d| d.fingerprint);
        let mut stale: Vec<TabId> = self
            .shown
            .iter()
            .filter(|(_, shown)| shown.discovery.profile.key() == key && Some(shown.fingerprint) != fingerprint)
            .map(|(tab, _)| *tab)
            .collect();
        stale.sort_by_key(|tab| tab.0);
        for tab in stale {
            self.shown.remove(&tab);
            events.push(GateEvent::Reask(tab));
        }
        if self
            .command_shown
            .as_ref()
            .is_some_and(|c| c.shown.discovery.profile.key() == key && Some(c.shown.fingerprint) != fingerprint)
        {
            let command = self.command_shown.take().expect("checked above");
            events.push(GateEvent::CommandDone {
                request_id: command.request_id,
                ok: false,
                notice: Some(COMMAND_OUT_OF_DATE.to_string()),
            });
        }
        self.latest.insert(key, state);
    }

    fn command_check_result(&mut self, request_id: String, tab: TabId, key: ProfileKey) -> GateEvent {
        let done = |ok: bool, notice: String| GateEvent::CommandDone {
            request_id: request_id.clone(),
            ok,
            notice: Some(notice),
        };
        match self.latest.get(&key) {
            None | Some(TrustState::NothingToTrust(_)) => done(true, NOTHING_HERE.to_string()),
            Some(TrustState::Trusted { since_unix, .. }) => {
                done(true, format!("already trusted since {}", civil_date(*since_unix)))
            }
            Some(TrustState::Untrusted { discovery, .. } | TrustState::Changed { discovery, .. })
                if !discovery.fully_hashed() =>
            {
                done(false, cannot_remember_notice(discovery))
            }
            Some(_) => {
                self.command_pending = Some((request_id.clone(), tab, key));
                GateEvent::CommandAsk { request_id, tab }
            }
        }
    }

    /// Where a start goes, given a check's result and this window's answers for its profile. Pure.
    pub(crate) fn route(&self, state: &TrustState) -> Route {
        let profile = state_profile(state);
        match state {
            // Nothing exists to load, so the untrusted tiers lose nothing, and they keep out a
            // `.claude/` that appears between this check and the CLI's own read. An agent that needs
            // a `y` never gets here: the store asks about its empty discovery.
            TrustState::NothingToTrust(_) if !profile.requires_explicit_yes() => Route::Start(ProjectTrust::Untrusted),
            TrustState::NothingToTrust(_) => Route::Ask,
            TrustState::Trusted { .. } => Route::Start(ProjectTrust::Trusted),
            TrustState::Untrusted { discovery, .. } | TrustState::Changed { discovery, .. } => {
                match self.answered.get(&profile.key()) {
                    Some(answered)
                        if answered.fingerprint == discovery.fingerprint
                            && discovery.fully_hashed()
                            && (answered.trust == ProjectTrust::Trusted || !profile.requires_explicit_yes()) =>
                    {
                        Route::Start(answered.trust)
                    }
                    _ => Route::Ask,
                }
            }
        }
    }

    /// The route for a start of `profile` right now, from its latest check; `None` before any check
    /// of it came back.
    pub(crate) fn current_route(&self, profile: &Profile) -> Option<Route> {
        self.latest.get(&profile.key()).map(|state| self.route(state))
    }

    /// The newest discovery of `profile`: what a start routed by [`TrustGate::current_route`], or let
    /// through by an answer, is bound to. A `y` is taken only while the newest check still has the
    /// fingerprint its prompt showed, and an `n` only while the page echoes it, so this is the
    /// configuration the answer was about.
    pub(crate) fn bound_discovery(&self, profile: &Profile) -> Option<&Discovery> {
        self.latest.get(&profile.key()).and_then(discovery_of)
    }

    /// The prompt for `tab` under `nonce`, from the latest check of `profile` (the tab's agent's),
    /// kept as what this tab was shown. `None` when that check is nothing to ask about.
    pub(crate) fn show(&mut self, tab: TabId, nonce: u64, profile: &Profile) -> Option<TrustPromptView> {
        let (view, shown) = self.build_prompt(tab, nonce, &profile.key())?;
        self.shown.insert(tab, shown);
        Some(view)
    }

    fn build_prompt(&self, tab: TabId, nonce: u64, key: &str) -> Option<(TrustPromptView, Shown)> {
        let (discovery, state, diff, remember) = match self.latest.get(key)? {
            TrustState::Untrusted { discovery, remember } => (discovery, TrustPromptState::Untrusted, None, remember),
            TrustState::Changed {
                discovery,
                diff,
                remember,
            } => (discovery, TrustPromptState::Changed, Some(diff), remember),
            _ => return None,
        };
        let (remember, remember_note) = match remember {
            Remember::SessionOnly(note) => (
                TrustRememberView::Session,
                Some(format!("{note}; y trusts this start only; the next one asks again")),
            ),
            Remember::WindowOnly(why) => (TrustRememberView::Window, Some(why.clone())),
            Remember::Yes => match self.record_failure.get(key) {
                Some(why) => (
                    TrustRememberView::Window,
                    Some(format!("trust could not be recorded ({why})")),
                ),
                None => (TrustRememberView::Yes, None),
            },
        };
        let lossy = |paths: &[PathBuf]| -> Vec<String> { paths.iter().map(|p| p.display().to_string()).collect() };
        let view = TrustPromptView {
            tab,
            nonce,
            agent: discovery.profile.label(),
            root: discovery.root.display().to_string(),
            top: discovery.top.display().to_string(),
            fingerprint: discovery.fingerprint.hex(),
            state,
            remember,
            remember_note,
            changed: diff.map(|diff| TrustDiffView {
                added: lossy(&diff.added),
                removed: lossy(&diff.removed),
                changed: lossy(&diff.changed),
            }),
            items: discovery.findings.iter().flat_map(items_of).collect(),
        };
        let shown = Shown {
            nonce,
            fingerprint: discovery.fingerprint,
            findings_digest: view.findings_digest(),
            discovery: discovery.clone(),
        };
        Some((view, shown))
    }

    /// Whether an answer can outlive this start, for the latest check of a profile.
    fn remember_now(&self, key: &str) -> Remember {
        match self.latest.get(key) {
            Some(TrustState::Untrusted { remember, .. } | TrustState::Changed { remember, .. }) => match remember {
                Remember::Yes => match self.record_failure.get(key) {
                    Some(why) => Remember::WindowOnly(format!("trust could not be recorded ({why})")),
                    None => Remember::Yes,
                },
                other => other.clone(),
            },
            _ => Remember::Yes,
        }
    }

    /// The checks every answer makes before anything is taken: the prompt is the one this tab was
    /// shown under `nonce`, the page echoes exactly what it showed, and the latest check still has
    /// its fingerprint.
    fn check_echo(
        shown: Option<&Shown>,
        nonce: u64,
        latest: Option<&TrustState>,
        fingerprint: &str,
        digest: &str,
    ) -> Result<(), &'static str> {
        let Some(shown) = shown.filter(|s| s.nonce == nonce) else {
            return Err(NOT_CURRENT);
        };
        let latest = latest.and_then(discovery_of).map(|d| d.fingerprint);
        if fingerprint != shown.fingerprint.hex()
            || digest != shown.findings_digest
            || latest != Some(shown.fingerprint)
        {
            return Err(OUT_OF_DATE);
        }
        Ok(())
    }

    /// `y` (`trust`) or `n` to `tab`'s prompt. Touches no disk; a `y` queues the worker's second look
    /// at it, and the record when one is kept.
    pub(crate) fn answer(
        &mut self,
        tab: TabId,
        nonce: u64,
        fingerprint: &str,
        findings_digest: &str,
        trust: bool,
        now_unix: u64,
    ) -> AnswerOutcome {
        let shown_key = self.shown.get(&tab).map(|s| s.discovery.profile.key());
        if let Err(why) = Self::check_echo(
            self.shown.get(&tab),
            nonce,
            shown_key.as_deref().and_then(|key| self.latest.get(key)),
            fingerprint,
            findings_digest,
        ) {
            // A prompt answered with something other than what it showed is spent: the caller asks
            // again with what is on disk now. An answer to another prompt leaves this one alone.
            if why == OUT_OF_DATE {
                self.shown.remove(&tab);
            }
            return AnswerOutcome::Refused(why.to_string());
        }
        let shown = self.shown.remove(&tab).expect("checked above");
        let profile = shown.discovery.profile.clone();
        let key = profile.key();
        let remember = self.remember_now(&key);
        if !trust {
            // An `n` for an agent that cannot leave the project's configuration out starts nothing,
            // and is not remembered: the next start asks again.
            if profile.requires_explicit_yes() {
                return AnswerOutcome::NotStarted(refused_without_yes(&profile));
            }
            if shown.discovery.fully_hashed() {
                self.answered.insert(
                    key.clone(),
                    Answered {
                        fingerprint: shown.fingerprint,
                        trust: ProjectTrust::Untrusted,
                        window_only: None,
                    },
                );
            }
            let notice = self.said_not_loaded.insert(key).then(|| not_loaded(&profile));
            return AnswerOutcome::Start(ProjectTrust::Untrusted, notice);
        }
        // Every `y` waits for the worker to read the disk again: the prompt may have been on screen
        // for as long as it took to read it, and a configuration changed meanwhile must be asked
        // about rather than loaded unseen, whether or not a record is written.
        let epoch = self.epoch_of(&key);
        match remember {
            Remember::SessionOnly(_) => {
                let notice = session_only_notice(&shown.discovery);
                self.queue(Job::Confirm {
                    purpose: Purpose::ForTab(tab),
                    shown: Box::new(shown.discovery),
                    epoch,
                    kept: Kept {
                        window_only: None,
                        notice,
                    },
                });
                AnswerOutcome::Recording
            }
            Remember::WindowOnly(why) => {
                self.queue(Job::Confirm {
                    purpose: Purpose::ForTab(tab),
                    shown: Box::new(shown.discovery),
                    epoch,
                    kept: Kept {
                        notice: window_only_notice(&why),
                        window_only: Some(why),
                    },
                });
                AnswerOutcome::Recording
            }
            Remember::Yes => {
                self.queue(Job::Record {
                    purpose: Purpose::ForTab(tab),
                    shown: Box::new(shown.discovery),
                    epoch,
                    now: now_unix,
                });
                AnswerOutcome::Recording
            }
        }
    }

    /// `:trust` (`tab` is the tab whose `:` line asked) or `:untrust`, about `profile` (that tab's
    /// agent's). Only queues: the worker runs a fresh check and the record's removal, and the answer
    /// is a [`GateEvent::CommandDone`].
    pub(crate) fn command(&mut self, request_id: String, action: TrustAction, tab: TabId, profile: &Profile) {
        match action {
            TrustAction::Trust => self.queue(Job::CommandCheck {
                request_id,
                tab,
                profile: profile.clone(),
            }),
            TrustAction::Untrust => {
                // Before the forget is queued: from here on no start reuses this window's answer, and
                // a record still queued for an earlier `y` is not trust when it lands.
                let key = profile.key();
                let had_window_trust = self
                    .answered
                    .remove(&key)
                    .is_some_and(|answered| answered.trust == ProjectTrust::Trusted);
                *self.epochs.entry(key).or_insert(0) += 1;
                let mut dropped = Vec::new();
                self.drop_command_prompt_into(&mut dropped);
                self.owed.extend(dropped);
                self.queue(Job::Forget {
                    request_id,
                    had_window_trust,
                    profile: profile.clone(),
                });
            }
        }
    }

    /// `:trust`'s prompt for `tab`, if `:trust` asked for one there and it can still be asked.
    pub(crate) fn show_command(&mut self, tab: TabId, request_id: &str) -> Option<TrustPromptView> {
        let key = match self.command_pending.take() {
            Some((pending, pending_tab, key)) if pending == request_id && pending_tab == tab => key,
            other => {
                self.command_pending = other;
                return None;
            }
        };
        let nonce = self.next_command_nonce;
        self.next_command_nonce += 1;
        let (view, shown) = self.build_prompt(tab, nonce, &key)?;
        self.command_shown = Some(CommandShown {
            request_id: request_id.to_string(),
            tab,
            shown,
        });
        Some(view)
    }

    /// Whether `nonce` names `:trust`'s prompt on screen in `tab`.
    pub(crate) fn is_command_prompt(&self, tab: TabId, nonce: u64) -> bool {
        self.command_shown
            .as_ref()
            .is_some_and(|c| c.tab == tab && c.shown.nonce == nonce)
    }

    /// `y` or `n` to `:trust`'s prompt, with the same echo checks as a tab's. An answer that is not
    /// taken ends the command; `:trust` can be typed again.
    pub(crate) fn answer_command(
        &mut self,
        tab: TabId,
        nonce: u64,
        fingerprint: &str,
        findings_digest: &str,
        trust: bool,
        now_unix: u64,
    ) -> Option<CommandAnswer> {
        let command = self.command_shown.take_if(|c| c.tab == tab && c.shown.nonce == nonce)?;
        let request_id = command.request_id;
        let profile = command.shown.discovery.profile.clone();
        let key = profile.key();
        let done = |ok: bool, notice: Option<String>| CommandAnswer::Done {
            request_id: request_id.clone(),
            ok,
            notice,
        };
        if Self::check_echo(
            Some(&command.shown),
            nonce,
            self.latest.get(&key),
            fingerprint,
            findings_digest,
        )
        .is_err()
        {
            return Some(done(false, Some(COMMAND_OUT_OF_DATE.to_string())));
        }
        if !trust {
            return Some(done(false, None));
        }
        Some(match self.remember_now(&key) {
            Remember::SessionOnly(_) => done(false, Some(cannot_remember_notice(&command.shown.discovery))),
            Remember::WindowOnly(why) => {
                // Nothing is written, but the disk is read again first, as for a tab's `y`.
                let epoch = self.epoch_of(&key);
                self.queue(Job::Confirm {
                    purpose: Purpose::Command(request_id.clone()),
                    shown: Box::new(command.shown.discovery),
                    epoch,
                    kept: Kept {
                        notice: format!("{} (this window only: {why})", trusted_from_now(&profile)),
                        window_only: Some(why),
                    },
                });
                CommandAnswer::Recording
            }
            Remember::Yes => {
                let epoch = self.epoch_of(&key);
                self.queue(Job::Record {
                    purpose: Purpose::Command(request_id.clone()),
                    shown: Box::new(command.shown.discovery),
                    epoch,
                    now: now_unix,
                });
                CommandAnswer::Recording
            }
        })
    }

    /// Takes `:trust`'s prompt (or its pending question) off, answering the command `not trusted`
    /// on the next poll: the user moved away from it.
    pub(crate) fn drop_command_prompt(&mut self) {
        let mut dropped = Vec::new();
        self.drop_command_prompt_into(&mut dropped);
        self.owed.extend(dropped);
    }

    fn drop_command_prompt_into(&mut self, events: &mut Vec<GateEvent>) {
        let request_ids = self
            .command_shown
            .take()
            .map(|c| c.request_id)
            .into_iter()
            .chain(self.command_pending.take().map(|(request_id, _, _)| request_id));
        for request_id in request_ids {
            events.push(GateEvent::CommandDone {
                request_id,
                ok: false,
                notice: None,
            });
        }
    }

    /// The `:trust` prompt's request, if `nonce` names it in `tab`, taken off for an `Escape`.
    pub(crate) fn cancel_command(&mut self, tab: TabId, nonce: u64) -> Option<String> {
        self.command_shown
            .take_if(|c| c.tab == tab && c.shown.nonce == nonce)
            .map(|c| c.request_id)
    }

    /// `tab`'s prompt is no longer on screen (it was routed away from, or the page reloaded).
    pub(crate) fn hide(&mut self, tab: TabId) {
        self.shown.remove(&tab);
    }

    /// A closed tab: its prompt and its wait go.
    pub(crate) fn forget_tab(&mut self, tab: TabId) {
        self.shown.remove(&tab);
        self.waiting.retain(|(t, _, _)| *t != tab);
        if self.command_shown.as_ref().is_some_and(|c| c.tab == tab)
            || self.command_pending.as_ref().is_some_and(|(_, t, _)| *t == tab)
        {
            self.drop_command_prompt();
        }
    }

    /// The `trust` row of `prefix i`, for the latest check of `profile` (the tab's agent's).
    pub(crate) fn row(&self, profile: &Profile) -> String {
        let key = profile.key();
        let Some(state) = self.latest.get(&key) else {
            return "not checked yet".to_string();
        };
        match state {
            TrustState::NothingToTrust(_) => "nothing to trust".to_string(),
            TrustState::Trusted { since_unix, .. } => format!("trusted since {}", civil_date(*since_unix)),
            TrustState::Untrusted { discovery, .. } | TrustState::Changed { discovery, .. } => {
                let answered = self
                    .answered
                    .get(&key)
                    .filter(|a| a.fingerprint == discovery.fingerprint && discovery.fully_hashed());
                match (answered, state) {
                    (
                        Some(Answered {
                            trust: ProjectTrust::Trusted,
                            window_only,
                            ..
                        }),
                        _,
                    ) => format!(
                        "trusted for this window only ({})",
                        window_only.as_deref().unwrap_or("no record")
                    ),
                    (Some(_), _) => "not trusted (answered n in this window)".to_string(),
                    (None, TrustState::Changed { .. }) => "changed since trusted".to_string(),
                    (None, _) => "not trusted".to_string(),
                }
            }
        }
    }

    /// What a session of `profile` started now would be given without asking; untrusted when it
    /// would ask.
    pub(crate) fn next_session_trust(&self, profile: &Profile) -> ProjectTrust {
        match self.current_route(profile) {
            Some(Route::Start(trust)) => trust,
            Some(Route::Ask) | None => ProjectTrust::Untrusted,
        }
    }
}

fn refused(purpose: Purpose, notice: String) -> GateEvent {
    match purpose {
        Purpose::ForTab(tab) => GateEvent::RecordRefused { tab, notice },
        Purpose::Command(request_id) => GateEvent::CommandDone {
            request_id,
            ok: false,
            notice: Some(notice),
        },
    }
}

/// The profile a check's result is about.
fn state_profile(state: &TrustState) -> &Profile {
    match state {
        TrustState::NothingToTrust(discovery)
        | TrustState::Trusted { discovery, .. }
        | TrustState::Untrusted { discovery, .. }
        | TrustState::Changed { discovery, .. } => &discovery.profile,
    }
}

fn discovery_of(state: &TrustState) -> Option<&Discovery> {
    match state {
        TrustState::NothingToTrust(discovery)
        | TrustState::Trusted { discovery, .. }
        | TrustState::Untrusted { discovery, .. }
        | TrustState::Changed { discovery, .. } => Some(discovery),
    }
}

/// One finding as the prompt shows it: one row, or for a rule file whose calls are all it holds one
/// row per call (a file with none still gets its row).
fn items_of(finding: &Finding) -> Vec<TrustItemView> {
    match finding {
        Finding::RuleFile {
            path,
            calls,
            text: None,
        } if !calls.is_empty() => calls
            .iter()
            .map(|call| TrustItemView {
                what: "rule",
                file: path.display().to_string(),
                label: "exec-policy rule".to_string(),
                value: call.clone(),
                outside: false,
            })
            .collect(),
        other => vec![item_of(other)],
    }
}

/// One finding as one row of the prompt.
fn item_of(finding: &Finding) -> TrustItemView {
    let lossy = |p: &Path| p.display().to_string();
    let item = |what: &'static str, file: &Path, label: String, value: String| TrustItemView {
        what,
        file: lossy(file),
        label,
        value,
        outside: false,
    };
    match finding {
        Finding::Hook {
            file,
            event,
            matcher,
            command,
        } => {
            let label = match matcher {
                Some(matcher) => format!("{event} ({matcher})"),
                None => event.clone(),
            };
            item("hook", file, label, command.clone())
        }
        Finding::McpServer {
            file,
            name,
            command_line,
            approval_mode,
        } => {
            let label = match approval_mode {
                Some(mode) => format!("{name} (tools approved: {mode})"),
                None => name.clone(),
            };
            item("mcp_server", file, label, command_line.clone())
        }
        Finding::EnvKey { file, key } => item("env_key", file, "env".to_string(), key.clone()),
        Finding::ApiKeyHelper { file, command } => {
            item("api_key_helper", file, "apiKeyHelper".to_string(), command.clone())
        }
        Finding::Allow { file, rule } => item("allow", file, "permissions.allow".to_string(), rule.clone()),
        Finding::AdditionalDirectory { file, path } => item(
            "additional_directory",
            file,
            "additionalDirectories".to_string(),
            path.clone(),
        ),
        Finding::OtherSetting { file, key, value } => item("other_setting", file, key.clone(), value.clone()),
        Finding::Unparsed { file, reason } => item("unparsed", file, "not parsed".to_string(), reason.clone()),
        Finding::Memory { path } => item("memory", path, "memory file".to_string(), "present".to_string()),
        // A file that is more than its rule calls is shown whole: what it allows may be spelt in no
        // call a scan can pick out.
        Finding::RuleFile {
            path, text: Some(text), ..
        } => item("rule", path, "exec-policy rules, whole file".to_string(), text.clone()),
        Finding::RuleFile { path, .. } => item(
            "rule",
            path,
            "exec-policy rules".to_string(),
            "only comments".to_string(),
        ),
        Finding::Skill { path, name } => item("skill", path, "skill".to_string(), name.clone()),
        Finding::PluginListing { path } => item(
            "plugin_listing",
            path,
            "plugin listing".to_string(),
            "present".to_string(),
        ),
        Finding::Overridden { file, key, value } => item("overridden", file, key.clone(), value.clone()),
        Finding::Symlink { path, target, outside } => TrustItemView {
            outside: *outside,
            ..item("symlink", path, lossy(path), lossy(target))
        },
        Finding::OtherFiles { dir, count } => item("other_files", dir, "other files".to_string(), count.to_string()),
        Finding::NotRead { path, reason } => item("not_read", path, "not read".to_string(), reason.clone()),
        Finding::Unreadable { path, reason } => {
            item("unreadable", path, "cannot be checked".to_string(), reason.clone())
        }
        Finding::OverBudget { reason } => item("over_budget", Path::new(""), "over budget".to_string(), reason.clone()),
        Finding::GitUnverified { path, reason, target } => {
            let value = match target {
                Some(target) => format!("{reason} -> {}", target.display()),
                None => reason.clone(),
            };
            item("git_unverified", path, "git".to_string(), value)
        }
    }
}

/// Where Codex keeps the user's own configuration, as far as Eitri can tell without asking it.
#[derive(Debug, PartialEq, Eq)]
struct CodexHomeGuess {
    /// A project's `.codex/` that is this directory is the user's own and is not read. `None` skips
    /// nothing.
    skip: Option<PathBuf>,
    /// Where the alternate names of `AGENTS.md` are read from.
    config: Option<PathBuf>,
}

/// Codex's home from `CODEX_HOME` (`var`), as Codex picks it: unset or empty means `~/.codex`, and
/// any other value is used as given, a relative one resolved against Codex's own working directory.
/// That directory is the project root when Eitri starts it, but nothing here can be sure of it, so
/// a relative value skips no `.codex/` (each is asked about) and only the alternate names are taken
/// from the root's join, which can add files to ask about but never remove one (its MCP servers are
/// not taken as the user's, [`codex_profile_env`]). Touches no disk.
fn codex_home_guess(var: Option<&std::ffi::OsStr>, home: Option<&Path>, root: &Path) -> CodexHomeGuess {
    match var.filter(|v| !v.is_empty()).map(Path::new) {
        None => {
            let default = home.map(|h| h.join(".codex"));
            CodexHomeGuess {
                skip: default.clone(),
                config: default,
            }
        }
        Some(dir) if dir.is_absolute() => CodexHomeGuess {
            skip: Some(dir.to_path_buf()),
            config: Some(dir.to_path_buf()),
        },
        Some(relative) => CodexHomeGuess {
            skip: None,
            config: Some(root.join(relative)),
        },
    }
}

/// What a Codex discovery is given from the configuration read in `user`, with `codex_home` the
/// `.codex/` known to be the user's own (`None` when it is not known). The servers `user` lists are
/// passed on only when that home is known: a relative `CODEX_HOME` may name a directory inside the
/// project, and taking the servers of a file the project wrote as the user's would let the project
/// exempt its own server from the check an untrusted session makes. The alternate names are passed
/// on either way, since they can only add files to ask about. Touches no disk.
fn codex_profile_env(codex_home: Option<PathBuf>, user: UserCodexConfig) -> ProfileEnv {
    let user_mcp_servers = if codex_home.is_some() { user.mcp_servers } else { None };
    ProfileEnv {
        fallback_names: user.fallback_names,
        user_mcp_servers,
        codex_home,
    }
}

/// The user's own Codex `config.toml` in `dir`, read the way Codex reads it: a link at the file is
/// followed, since a user who keeps it in a dotfiles repository links it into place (read without
/// following, such a file would hide every server the user has). The file it names is still read as
/// a regular file of at most 4 MiB without blocking. Only the alternate names of `AGENTS.md` and the
/// names of MCP servers are kept. Runs on the worker only.
fn read_codex_user_config(dir: &Path) -> UserCodexConfig {
    let file = dir.join("config.toml");
    let resolved = std::fs::canonicalize(&file).unwrap_or(file);
    eitri_core::project_trust::read_user_codex_config(&resolved)
}

/// The worker: the only code here that touches the disk. It owns the store, runs each job in the
/// order queued, and ends when the gate (and so the sender) is dropped.
fn run_worker(
    store: TrustStore,
    root: PathBuf,
    home: Option<PathBuf>,
    codex_home: Option<PathBuf>,
    jobs: mpsc::Receiver<Job>,
    done: mpsc::Sender<Done>,
    before_each: Box<dyn Fn() + Send>,
) {
    // Discovery compares the root's ancestors with the home by path, so a home spelt through a link
    // must be resolved once, here, where touching the disk is allowed.
    let home = home
        .filter(|h| h.is_absolute())
        .map(|h| std::fs::canonicalize(&h).unwrap_or(h));
    let home = home.as_deref();
    let canonical = |h: PathBuf| std::fs::canonicalize(&h).unwrap_or(h);
    let guess = codex_home_guess(codex_home.as_deref().map(Path::as_os_str), home, &root);
    let codex_home = guess.skip.map(canonical);
    let codex_config_dir = guess.config.map(canonical);
    // Read again for every Codex job: the user may change their configuration while Eitri runs.
    let env_for = |profile: &Profile| match profile {
        Profile::Codex => {
            let user = codex_config_dir.as_deref().map_or_else(
                || UserCodexConfig {
                    fallback_names: Vec::new(),
                    mcp_servers: None,
                },
                read_codex_user_config,
            );
            codex_profile_env(codex_home.clone(), user)
        }
        Profile::Claude | Profile::Acp { .. } => ProfileEnv::default(),
    };
    let check = |store: &TrustStore, profile: &Profile| {
        eitri_core::project_trust::for_root(store, &root, home, profile, &env_for(profile))
    };
    let rediscover = |profile: &Profile| {
        eitri_core::project_trust::discover(&root, home, &Default::default(), profile, &env_for(profile))
    };
    while let Ok(job) = jobs.recv() {
        before_each();
        let result = match job {
            Job::Check { generation, profile } => Done::Checked {
                generation,
                state: check(&store, &profile),
            },
            Job::Record {
                purpose,
                shown,
                epoch,
                now,
            } => {
                // What was shown is what is written, and only if the disk still says the same: a
                // change since the prompt is asked about again rather than trusted unseen.
                let fresh = rediscover(&shown.profile);
                if fresh.fingerprint != shown.fingerprint {
                    Done::Moved {
                        purpose,
                        state: store.state(fresh),
                    }
                } else {
                    match store.record(&shown, now) {
                        Ok(()) => Done::Recorded {
                            purpose,
                            epoch,
                            state: TrustState::Trusted {
                                since_unix: now,
                                discovery: fresh,
                            },
                        },
                        Err(e) => Done::RecordFailed {
                            purpose,
                            profile: shown.profile.key(),
                            reason: e.to_string(),
                        },
                    }
                }
            }
            Job::Confirm {
                purpose,
                shown,
                epoch,
                kept,
            } => {
                let fresh = rediscover(&shown.profile);
                if fresh.fingerprint != shown.fingerprint {
                    Done::Moved {
                        purpose,
                        state: store.state(fresh),
                    }
                } else {
                    Done::Confirmed {
                        purpose,
                        epoch,
                        kept,
                        state: store.state(fresh),
                    }
                }
            }
            Job::CommandCheck {
                request_id,
                tab,
                profile,
            } => Done::CommandChecked {
                request_id,
                tab,
                state: check(&store, &profile),
            },
            Job::Forget {
                request_id,
                had_window_trust,
                profile,
            } => {
                let (ok, notice) = match store.forget(&root, &profile) {
                    Ok(existed) if existed || had_window_trust => (true, untrusted_from_now(&profile)),
                    Ok(_) => (true, WAS_NOT_TRUSTED.to_string()),
                    Err(e) => (false, e.to_string()),
                };
                Done::Forgot {
                    request_id,
                    ok,
                    notice,
                    state: check(&store, &profile),
                }
            }
        };
        if done.send(result).is_err() {
            return;
        }
    }
}

/// A gate on the worker, for tests: closed, every job waits until it is opened (or ten seconds
/// pass, so a test that never opens it does not leave a thread blocked for good).
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct WorkerHold(std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>);

#[cfg(test)]
impl WorkerHold {
    pub(crate) fn closed() -> WorkerHold {
        let hold = WorkerHold::default();
        hold.close();
        hold
    }

    pub(crate) fn close(&self) {
        *self.0 .0.lock().unwrap() = true;
    }

    pub(crate) fn open(&self) {
        *self.0 .0.lock().unwrap() = false;
        self.0 .1.notify_all();
    }

    fn wait(&self) {
        let (closed, changed) = &*self.0;
        let guard = closed.lock().unwrap();
        let _ = changed
            .wait_timeout_while(guard, std::time::Duration::from_secs(10), |closed| *closed)
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::project_trust::{Entry, EntryKind};
    use eitri_core::source_scan as scan;
    use std::time::{Duration, Instant};

    /// A scratch directory holding a fake home with the project below it and a fake state home.
    /// Every file a test inspects is made here; nothing points at a real project or the real home.
    struct Fx {
        outer: PathBuf,
        home: PathBuf,
        state: PathBuf,
        root: PathBuf,
    }

    impl Fx {
        fn new(label: &str) -> Fx {
            let outer = std::env::temp_dir().join(format!("eitri-trust-gate-{label}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&outer).unwrap();
            let outer = outer.canonicalize().unwrap();
            let home = outer.join("home");
            let state = outer.join("state");
            // Strictly below the home: the home itself is the user tier, which reads differently.
            let root = home.join("p");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::create_dir_all(&state).unwrap();
            Fx {
                outer,
                home,
                state,
                root,
            }
        }

        fn store(&self) -> TrustStore {
            TrustStore::new(Some(self.state.as_os_str()), Some(self.home.as_os_str()))
        }

        fn gate(&self) -> TrustGate {
            TrustGate::new(self.store(), self.root.clone(), Some(self.home.clone()))
        }

        fn held(&self, hold: &WorkerHold) -> TrustGate {
            TrustGate::new_held(self.store(), self.root.clone(), Some(self.home.clone()), hold.clone())
        }

        fn record_file(&self) -> PathBuf {
            self.state
                .join("eitri/trust")
                .join(format!("{}.json", &agent::conversation_id_for_cwd(&self.root)[..16]))
        }

        fn write(&self, relative: &str, bytes: &[u8]) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }

        /// Records the configuration as it is now, as another window's `y` would have.
        fn trust_now(&self, at: u64) {
            let found = eitri_core::project_trust::discover(
                &self.root,
                Some(&self.home),
                &Default::default(),
                &Profile::Claude,
                &ProfileEnv::default(),
            );
            self.store().record(&found, at).unwrap();
        }
    }

    impl Drop for Fx {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.outer);
        }
    }

    fn hook(command: &str) -> Vec<u8> {
        format!(r#"{{"hooks":{{"SessionStart":[{{"hooks":[{{"type":"command","command":"{command}"}}]}}]}}}}"#)
            .into_bytes()
    }

    fn padded_hook(command: &str, len: usize) -> Vec<u8> {
        let mut body = hook(command);
        body.pop();
        body.resize(len - 1, b' ');
        body.push(b'}');
        body
    }

    const SETTINGS: &str = ".claude/settings.json";
    const A: TabId = TabId(1);
    const CLAUDE: Profile = Profile::Claude;
    const B: TabId = TabId(2);

    /// Polls every 5 ms until `done` holds for everything seen so far, for at most two seconds.
    fn drain(gate: &mut TrustGate, done: impl Fn(&[GateEvent]) -> bool) -> Vec<GateEvent> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut seen = Vec::new();
        loop {
            seen.extend(gate.poll());
            if done(&seen) {
                return seen;
            }
            assert!(Instant::now() < deadline, "gave up waiting; saw {seen:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A start for `tab`, as the panel defers one: waits for a fresh check and returns its route.
    fn start(gate: &mut TrustGate, tab: TabId) -> Route {
        gate.wait_for(tab, &CLAUDE);
        gate.begin_check(&CLAUDE);
        let seen = drain(gate, |seen| {
            seen.iter().any(|e| matches!(e, GateEvent::Route(t, _) if *t == tab))
        });
        seen.into_iter()
            .find_map(|e| match e {
                GateEvent::Route(t, route) if t == tab => Some(route),
                _ => None,
            })
            .unwrap()
    }

    fn answer(gate: &mut TrustGate, view: &TrustPromptView, trust: bool) -> AnswerOutcome {
        gate.answer(
            view.tab,
            view.nonce,
            &view.fingerprint,
            &view.findings_digest(),
            trust,
            1_790_000_000,
        )
    }

    fn command_done(seen: &[GateEvent], request: &str) -> Option<(bool, Option<String>)> {
        seen.iter().find_map(|e| match e {
            GateEvent::CommandDone { request_id, ok, notice } if request_id == request => Some((*ok, notice.clone())),
            _ => None,
        })
    }

    fn command(gate: &mut TrustGate, request: &str, action: TrustAction) -> Vec<GateEvent> {
        gate.command(request.to_string(), action, A, &CLAUDE);
        drain(gate, |seen| {
            command_done(seen, request).is_some()
                || seen
                    .iter()
                    .any(|e| matches!(e, GateEvent::CommandAsk { request_id, .. } if request_id == request))
        })
    }

    #[test]
    fn nothing_to_trust_starts_silently_without_the_tiers() {
        let fx = Fx::new("nothing");
        fx.write("src/main.rs", b"fn main() {}\n");
        let mut gate = fx.gate();
        assert_eq!(gate.row(&CLAUDE), "not checked yet");
        assert_eq!(start(&mut gate, A), Route::Start(ProjectTrust::Untrusted));
        assert!(gate.show(A, 1, &CLAUDE).is_none(), "nothing to ask about");
        assert_eq!(gate.row(&CLAUDE), "nothing to trust");
        assert_eq!(gate.next_session_trust(&CLAUDE), ProjectTrust::Untrusted);
    }

    #[test]
    fn trusted_unchanged_starts_with_the_tiers_without_asking() {
        let fx = Fx::new("trusted");
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        fx.trust_now(1_790_000_000);
        let mut gate = fx.gate();
        assert_eq!(start(&mut gate, A), Route::Start(ProjectTrust::Trusted));
        assert!(gate.show(A, 1, &CLAUDE).is_none());
        assert_eq!(gate.row(&CLAUDE), "trusted since 2026-09-21");
        assert_eq!(gate.next_session_trust(&CLAUDE), ProjectTrust::Trusted);
    }

    #[test]
    fn untrusted_and_changed_ask() {
        let fx = Fx::new("ask");
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        let mut gate = fx.gate();
        assert_eq!(start(&mut gate, A), Route::Ask);
        let view = gate.show(A, 1, &CLAUDE).unwrap();
        assert_eq!(view.state, TrustPromptState::Untrusted);
        assert_eq!(view.remember, TrustRememberView::Yes);
        assert!(view.items.iter().any(|i| i.what == "hook" && i.value == "touch /tmp/a"));
        assert_eq!(gate.next_session_trust(&CLAUDE), ProjectTrust::Untrusted);

        fx.trust_now(5);
        fx.write(SETTINGS, &hook("touch /tmp/b"));
        assert_eq!(start(&mut gate, B), Route::Ask);
        let view = gate.show(B, 2, &CLAUDE).unwrap();
        assert_eq!(view.state, TrustPromptState::Changed);
        assert_eq!(view.changed.unwrap().changed, vec![SETTINGS.to_string()]);
        assert_eq!(gate.row(&CLAUDE), "changed since trusted");
    }

    #[test]
    fn an_answer_covers_the_window_until_the_fingerprint_changes() {
        let fx = Fx::new("window");
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        let mut gate = fx.gate();
        assert_eq!(start(&mut gate, A), Route::Ask);
        let view = gate.show(A, 1, &CLAUDE).unwrap();
        assert_eq!(
            answer(&mut gate, &view, false),
            AnswerOutcome::Start(ProjectTrust::Untrusted, Some(NOT_LOADED.to_string()))
        );
        // Another tab of this window starts without the question while nothing changed.
        assert_eq!(start(&mut gate, B), Route::Start(ProjectTrust::Untrusted));
        assert_eq!(gate.next_session_trust(&CLAUDE), ProjectTrust::Untrusted);
        fx.write(SETTINGS, &hook("touch /tmp/b"));
        assert_eq!(start(&mut gate, B), Route::Ask, "a changed fingerprint asks again");
        assert!(!fx.record_file().exists(), "an n writes nothing");
    }

    #[test]
    fn y_without_a_state_home_is_window_only_and_says_so() {
        let fx = Fx::new("nostate");
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        let mut gate = TrustGate::new(TrustStore::at(None), fx.root.clone(), Some(fx.home.clone()));
        assert_eq!(start(&mut gate, A), Route::Ask);
        let view = gate.show(A, 1, &CLAUDE).unwrap();
        assert_eq!(view.remember, TrustRememberView::Window);
        let why = "no usable state directory (XDG_STATE_HOME/HOME)";
        assert_eq!(view.remember_note.as_deref(), Some(why));
        assert_eq!(answer(&mut gate, &view, true), AnswerOutcome::Recording);
        drain(&mut gate, |seen| {
            seen.contains(&GateEvent::Recorded {
                tab: A,
                notice: Some(window_only_notice(why)),
            })
        });
        assert_eq!(
            window_only_notice(why),
            "trusted for this window only: no usable state directory (XDG_STATE_HOME/HOME)"
        );
        assert_eq!(start(&mut gate, B), Route::Start(ProjectTrust::Trusted));
        assert_eq!(gate.row(&CLAUDE), format!("trusted for this window only ({why})"));
        assert_eq!(gate.next_session_trust(&CLAUDE), ProjectTrust::Trusted);
    }

    #[test]
    fn n_says_not_loaded_once() {
        let fx = Fx::new("once");
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        let mut gate = fx.gate();
        start(&mut gate, A);
        let view = gate.show(A, 1, &CLAUDE).unwrap();
        assert_eq!(
            answer(&mut gate, &view, false),
            AnswerOutcome::Start(ProjectTrust::Untrusted, Some(NOT_LOADED.to_string()))
        );
        fx.write(SETTINGS, &hook("touch /tmp/b"));
        assert_eq!(start(&mut gate, B), Route::Ask);
        let view = gate.show(B, 2, &CLAUDE).unwrap();
        assert_eq!(
            answer(&mut gate, &view, false),
            AnswerOutcome::Start(ProjectTrust::Untrusted, None),
            "said once per window"
        );
    }

    #[test]
    fn the_command_notices() {
        // Nothing on the path.
        let fx = Fx::new("cmd-nothing");
        let mut gate = fx.gate();
        let seen = command(&mut gate, "r1", TrustAction::Trust);
        assert_eq!(command_done(&seen, "r1"), Some((true, Some(NOTHING_HERE.to_string()))));
        let seen = command(&mut gate, "r2", TrustAction::Untrust);
        assert_eq!(
            command_done(&seen, "r2"),
            Some((true, Some(WAS_NOT_TRUSTED.to_string())))
        );

        // Something to trust: the prompt, then the record, then the two `:untrust` texts.
        let fx = Fx::new("cmd-record");
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        let mut gate = fx.gate();
        let seen = command(&mut gate, "r3", TrustAction::Trust);
        assert!(seen.contains(&GateEvent::CommandAsk {
            request_id: "r3".into(),
            tab: A
        }));
        let view = gate.show_command(A, "r3").unwrap();
        let answered = gate.answer_command(
            A,
            view.nonce,
            &view.fingerprint,
            &view.findings_digest(),
            true,
            1_790_000_000,
        );
        assert_eq!(answered, Some(CommandAnswer::Recording));
        let seen = drain(&mut gate, |seen| command_done(seen, "r3").is_some());
        assert_eq!(
            command_done(&seen, "r3"),
            Some((true, Some(TRUSTED_FROM_NOW.to_string())))
        );
        assert!(fx.record_file().is_file());
        let seen = command(&mut gate, "r4", TrustAction::Trust);
        assert_eq!(
            command_done(&seen, "r4"),
            Some((true, Some("already trusted since 2026-09-21".to_string())))
        );
        let seen = command(&mut gate, "r5", TrustAction::Untrust);
        assert_eq!(
            command_done(&seen, "r5"),
            Some((true, Some(UNTRUSTED_FROM_NOW.to_string())))
        );
        assert!(!fx.record_file().exists());
        let seen = command(&mut gate, "r6", TrustAction::Untrust);
        assert_eq!(
            command_done(&seen, "r6"),
            Some((true, Some(WAS_NOT_TRUSTED.to_string())))
        );

        // No record possible: this window only, and `:untrust` drops that too.
        let fx = Fx::new("cmd-window");
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        let mut gate = TrustGate::new(TrustStore::at(None), fx.root.clone(), Some(fx.home.clone()));
        command(&mut gate, "r7", TrustAction::Trust);
        let view = gate.show_command(A, "r7").unwrap();
        let answered = gate.answer_command(A, view.nonce, &view.fingerprint, &view.findings_digest(), true, 1);
        assert_eq!(answered, Some(CommandAnswer::Recording));
        let seen = drain(&mut gate, |seen| command_done(seen, "r7").is_some());
        assert_eq!(
            command_done(&seen, "r7"),
            Some((
                true,
                Some(format!(
                    "{TRUSTED_FROM_NOW} (this window only: no usable state directory (XDG_STATE_HOME/HOME))"
                ))
            ))
        );
        assert_eq!(start(&mut gate, B), Route::Start(ProjectTrust::Trusted));
        let seen = command(&mut gate, "r8", TrustAction::Untrust);
        assert_eq!(
            command_done(&seen, "r8"),
            Some((true, Some(UNTRUSTED_FROM_NOW.to_string())))
        );
        assert_eq!(start(&mut gate, B), Route::Ask);

        // Something that cannot be checked: nothing to remember, and nothing asked.
        let fx = Fx::new("cmd-session");
        fx.write(SETTINGS, &padded_hook("touch /tmp/a", 5 << 20));
        let mut gate = fx.gate();
        let seen = command(&mut gate, "r9", TrustAction::Trust);
        assert_eq!(
            command_done(&seen, "r9"),
            Some((
                false,
                Some(
                    "cannot be remembered: .claude/settings.json cannot be checked; each session start asks"
                        .to_string()
                )
            ))
        );
        assert!(!fx.record_file().exists());
    }

    #[test]
    fn a_trust_command_shows_the_prompt_and_records_only_after_y() {
        let fx = Fx::new("cmd-prompt");
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        let mut gate = fx.gate();
        command(&mut gate, "r1", TrustAction::Trust);
        assert!(!fx.record_file().exists(), "nothing is recorded unseen");
        assert!(
            gate.show_command(B, "r1").is_none(),
            "only in the tab whose : line asked"
        );
        let view = gate.show_command(A, "r1").unwrap();
        assert!(gate.is_command_prompt(A, view.nonce));
        assert_eq!(
            gate.answer_command(A, view.nonce, &view.fingerprint, &view.findings_digest(), false, 1),
            Some(CommandAnswer::Done {
                request_id: "r1".into(),
                ok: false,
                notice: None
            })
        );
        std::thread::sleep(Duration::from_millis(50));
        assert!(gate.poll().is_empty());
        assert!(!fx.record_file().exists());

        command(&mut gate, "r2", TrustAction::Trust);
        let view = gate.show_command(A, "r2").unwrap();
        // An echo of something else is not an answer to this prompt.
        assert!(matches!(
            gate.answer_command(A, view.nonce, &"0".repeat(64), &view.findings_digest(), true, 1),
            Some(CommandAnswer::Done { ok: false, .. })
        ));
        assert!(!fx.record_file().exists());

        command(&mut gate, "r3", TrustAction::Trust);
        let view = gate.show_command(A, "r3").unwrap();
        // Moving away takes the prompt off and answers the command.
        gate.drop_command_prompt();
        assert_eq!(command_done(&gate.poll(), "r3"), Some((false, None)));
        assert!(gate
            .answer_command(A, view.nonce, &view.fingerprint, &view.findings_digest(), true, 1)
            .is_none());

        command(&mut gate, "r4", TrustAction::Trust);
        let view = gate.show_command(A, "r4").unwrap();
        assert_eq!(
            gate.answer_command(A, view.nonce, &view.fingerprint, &view.findings_digest(), true, 1),
            Some(CommandAnswer::Recording)
        );
        drain(&mut gate, |seen| command_done(seen, "r4").is_some());
        assert!(fx.record_file().is_file());
    }

    #[test]
    fn untrust_drops_the_record_and_the_window_answer() {
        let fx = Fx::new("untrust");
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        let mut gate = fx.gate();
        start(&mut gate, A);
        let view = gate.show(A, 1, &CLAUDE).unwrap();
        assert_eq!(answer(&mut gate, &view, true), AnswerOutcome::Recording);
        drain(&mut gate, |seen| {
            seen.contains(&GateEvent::Recorded { tab: A, notice: None })
        });
        assert!(fx.record_file().is_file());
        assert_eq!(start(&mut gate, B), Route::Start(ProjectTrust::Trusted));
        let seen = command(&mut gate, "u", TrustAction::Untrust);
        assert_eq!(
            command_done(&seen, "u"),
            Some((true, Some(UNTRUSTED_FROM_NOW.to_string())))
        );
        assert!(!fx.record_file().exists());
        assert_eq!(gate.row(&CLAUDE), "not trusted");
        assert_eq!(start(&mut gate, B), Route::Ask);
    }

    #[test]
    fn untrust_after_a_queued_record_leaves_no_trust() {
        let fx = Fx::new("untrust-queued");
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        let hold = WorkerHold::default();
        let mut gate = fx.held(&hold);
        start(&mut gate, A);
        let view = gate.show(A, 1, &CLAUDE).unwrap();
        hold.close();
        assert_eq!(answer(&mut gate, &view, true), AnswerOutcome::Recording);
        gate.command("u".into(), TrustAction::Untrust, A, &CLAUDE);
        hold.open();
        let seen = drain(&mut gate, |seen| command_done(seen, "u").is_some());
        assert!(
            seen.contains(&GateEvent::RecordRefused {
                tab: A,
                notice: WITHDRAWN.to_string()
            }),
            "{seen:?}"
        );
        assert!(!seen.contains(&GateEvent::Recorded { tab: A, notice: None }));
        assert!(!fx.record_file().exists(), "the forget ran after the record");
        assert_eq!(start(&mut gate, B), Route::Ask, "no window answer survives");
    }

    #[test]
    fn the_row_for_each_state() {
        let fx = Fx::new("row");
        let mut gate = fx.gate();
        assert_eq!(gate.row(&CLAUDE), "not checked yet");
        start(&mut gate, A);
        assert_eq!(gate.row(&CLAUDE), "nothing to trust");
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        start(&mut gate, A);
        assert_eq!(gate.row(&CLAUDE), "not trusted");
        let view = gate.show(A, 1, &CLAUDE).unwrap();
        answer(&mut gate, &view, false);
        assert_eq!(gate.row(&CLAUDE), "not trusted (answered n in this window)");
        fx.trust_now(1_790_000_000);
        start(&mut gate, A);
        assert_eq!(gate.row(&CLAUDE), "trusted since 2026-09-21");
        fx.write(SETTINGS, &hook("touch /tmp/b"));
        start(&mut gate, A);
        assert_eq!(gate.row(&CLAUDE), "changed since trusted");

        let mut window = TrustGate::new(TrustStore::at(None), fx.root.clone(), Some(fx.home.clone()));
        start(&mut window, A);
        let view = window.show(A, 1, &CLAUDE).unwrap();
        answer(&mut window, &view, true);
        drain(&mut window, |seen| {
            seen.iter().any(|e| matches!(e, GateEvent::Recorded { tab: A, .. }))
        });
        assert_eq!(
            window.row(&CLAUDE),
            "trusted for this window only (no usable state directory (XDG_STATE_HOME/HOME))"
        );
    }

    #[test]
    fn a_stale_check_result_does_not_resolve_a_newer_wait() {
        let fx = Fx::new("stale");
        let state = eitri_core::project_trust::for_root(
            &fx.store(),
            &fx.root,
            Some(&fx.home),
            &Profile::Claude,
            &ProfileEnv::default(),
        );
        let (mut gate, jobs, done) = TrustGate::detached(fx.root.clone());
        let first = gate.begin_check(&CLAUDE);
        gate.wait_for(A, &CLAUDE);
        let second = gate.begin_check(&CLAUDE);
        assert!(second > first);
        assert!(matches!(jobs.try_recv(), Ok(Job::Check { generation, .. }) if generation == first));
        assert!(matches!(jobs.try_recv(), Ok(Job::Check { generation, .. }) if generation == second));
        done.send(Done::Checked {
            generation: first,
            state: state.clone(),
        })
        .unwrap();
        assert!(gate.poll().is_empty(), "a check queued before the wait answers nothing");
        assert!(gate.is_waiting(A));
        done.send(Done::Checked {
            generation: second,
            state,
        })
        .unwrap();
        assert_eq!(
            gate.poll(),
            vec![GateEvent::Route(A, Route::Start(ProjectTrust::Untrusted))]
        );
        assert!(!gate.is_waiting(A));
    }

    #[test]
    fn an_answer_echoing_another_fingerprint_or_digest_is_refused() {
        let fx = Fx::new("echo");
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        let state = eitri_core::project_trust::for_root(
            &fx.store(),
            &fx.root,
            Some(&fx.home),
            &Profile::Claude,
            &ProfileEnv::default(),
        );
        let (mut gate, jobs, done) = TrustGate::detached(fx.root.clone());
        gate.wait_for(A, &CLAUDE);
        gate.begin_check(&CLAUDE);
        done.send(Done::Checked { generation: 1, state }).unwrap();
        assert_eq!(gate.poll(), vec![GateEvent::Route(A, Route::Ask)]);
        let _ = jobs.try_recv();
        let view = gate.show(A, 7, &CLAUDE).unwrap();
        let digest = view.findings_digest();
        let other = "0".repeat(64);
        for (nonce, fingerprint, digest, why) in [
            (7, other.as_str(), digest.as_str(), OUT_OF_DATE),
            (7, view.fingerprint.as_str(), other.as_str(), OUT_OF_DATE),
            (8, view.fingerprint.as_str(), digest.as_str(), NOT_CURRENT),
        ] {
            // Shown again each time: an answer that echoes something else spends the prompt.
            let view = gate.show(A, 7, &CLAUDE).unwrap();
            assert_eq!(view.findings_digest(), digest_of(&gate, A));
            assert_eq!(
                gate.answer(A, nonce, fingerprint, digest, true, 1),
                AnswerOutcome::Refused(why.to_string())
            );
            assert!(jobs.try_recv().is_err(), "nothing was queued");
        }
        let view = gate.show(A, 9, &CLAUDE).unwrap();
        assert_eq!(answer(&mut gate, &view, true), AnswerOutcome::Recording);
        assert!(matches!(jobs.try_recv(), Ok(Job::Record { .. })));
    }

    fn digest_of(gate: &TrustGate, tab: TabId) -> String {
        gate.shown.get(&tab).unwrap().findings_digest.clone()
    }

    #[test]
    fn a_newer_check_from_another_tab_refuses_the_older_prompt() {
        let fx = Fx::new("newer");
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        let mut gate = fx.gate();
        assert_eq!(start(&mut gate, A), Route::Ask);
        let first = gate.show(A, 1, &CLAUDE).unwrap();
        fx.write(SETTINGS, &hook("touch /tmp/b"));
        gate.wait_for(B, &CLAUDE);
        gate.begin_check(&CLAUDE);
        let seen = drain(&mut gate, |seen| {
            seen.iter().any(|e| matches!(e, GateEvent::Route(t, _) if *t == B))
        });
        assert!(seen.contains(&GateEvent::Reask(A)), "{seen:?}");
        assert!(seen.contains(&GateEvent::Route(B, Route::Ask)));
        assert!(matches!(answer(&mut gate, &first, true), AnswerOutcome::Refused(_)));
        std::thread::sleep(Duration::from_millis(100));
        assert!(gate.poll().is_empty(), "no record was queued");
        assert!(!fx.record_file().exists());
        let second = gate.show(A, 2, &CLAUDE).unwrap();
        assert_ne!(second.fingerprint, first.fingerprint);
    }

    #[test]
    fn a_record_whose_disk_changed_since_the_prompt_writes_nothing() {
        let fx = Fx::new("moved");
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        let hold = WorkerHold::default();
        let mut gate = fx.held(&hold);
        start(&mut gate, A);
        let view = gate.show(A, 1, &CLAUDE).unwrap();
        hold.close();
        assert_eq!(answer(&mut gate, &view, true), AnswerOutcome::Recording);
        fx.write(SETTINGS, &hook("touch /tmp/b"));
        hold.open();
        let seen = drain(&mut gate, |seen| {
            seen.iter().any(|e| matches!(e, GateEvent::RecordRefused { .. }))
        });
        assert!(seen.contains(&GateEvent::RecordRefused {
            tab: A,
            notice: CHANGED_WHILE_READING.to_string()
        }));
        assert!(!fx.record_file().exists());
        let again = gate.show(A, 2, &CLAUDE).unwrap();
        assert_ne!(again.fingerprint, view.fingerprint, "asked about what is there now");
    }

    #[test]
    fn a_five_mib_settings_file_is_trusted_for_one_start_only() {
        let fx = Fx::new("five");
        let body = padded_hook("touch /tmp/a", 5 << 20);
        fx.write(SETTINGS, &body);
        let mut gate = fx.gate();
        assert_eq!(start(&mut gate, A), Route::Ask);
        let view = gate.show(A, 1, &CLAUDE).unwrap();
        assert_eq!(view.remember, TrustRememberView::Session);
        assert!(view
            .remember_note
            .as_deref()
            .unwrap()
            .ends_with("y trusts this start only; the next one asks again"));
        assert!(
            view.items
                .iter()
                .any(|i| i.what == "unreadable" && i.file == SETTINGS && i.label == "cannot be checked"),
            "{:?}",
            view.items
        );
        assert_eq!(answer(&mut gate, &view, true), AnswerOutcome::Recording);
        drain(&mut gate, |seen| {
            seen.contains(&GateEvent::Recorded {
                tab: A,
                notice: Some("trusted for this session only: .claude/settings.json cannot be checked".to_string()),
            })
        });
        assert!(!fx.record_file().exists());
        assert_eq!(start(&mut gate, B), Route::Ask, "the next start asks again");
        assert_eq!(gate.next_session_trust(&CLAUDE), ProjectTrust::Untrusted);
        // Changed in place at the same size: still asked, still never recorded.
        let changed = padded_hook("touch /tmp/b", 5 << 20);
        assert_eq!(changed.len(), body.len());
        fx.write(SETTINGS, &changed);
        assert_eq!(start(&mut gate, B), Route::Ask);
        let view = gate.show(B, 2, &CLAUDE).unwrap();
        assert!(matches!(
            answer(&mut gate, &view, false),
            AnswerOutcome::Start(ProjectTrust::Untrusted, _)
        ));
        assert_eq!(start(&mut gate, A), Route::Ask, "nor is an n remembered");
        assert!(!fx.record_file().exists());
    }

    /// A settings file that is a link: the CLI follows it and the fingerprint cannot see its target
    /// change, so it counts as unchecked: a `y` covers one start, nothing is recorded and the window
    /// remembers neither answer.
    #[test]
    fn a_linked_settings_file_is_trusted_for_one_start_only() {
        let fx = Fx::new("linked");
        let target = fx.outer.join("elsewhere.json");
        std::fs::write(&target, hook("touch /tmp/a")).unwrap();
        std::fs::create_dir_all(fx.root.join(".claude")).unwrap();
        std::os::unix::fs::symlink(&target, fx.root.join(SETTINGS)).unwrap();
        let mut gate = fx.gate();
        assert_eq!(start(&mut gate, A), Route::Ask);
        let view = gate.show(A, 1, &CLAUDE).unwrap();
        assert_eq!(view.remember, TrustRememberView::Session);
        assert_eq!(answer(&mut gate, &view, true), AnswerOutcome::Recording);
        drain(&mut gate, |seen| {
            seen.iter().any(|e| {
                matches!(e, GateEvent::Recorded { tab: A, notice: Some(notice) }
                    if notice.starts_with("trusted for this session only: "))
            })
        });
        assert_eq!(start(&mut gate, B), Route::Ask, "the next start asks again");
        let view = gate.show(B, 2, &CLAUDE).unwrap();
        assert!(matches!(
            answer(&mut gate, &view, false),
            AnswerOutcome::Start(ProjectTrust::Untrusted, _)
        ));
        assert_eq!(start(&mut gate, A), Route::Ask, "nor is an n remembered");
        assert!(!fx.record_file().exists());
    }

    #[test]
    fn a_record_does_not_block_the_answer() {
        let fx = Fx::new("noblock");
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        let hold = WorkerHold::default();
        let mut gate = fx.held(&hold);
        start(&mut gate, A);
        let view = gate.show(A, 1, &CLAUDE).unwrap();
        hold.close();
        let began = Instant::now();
        assert_eq!(answer(&mut gate, &view, true), AnswerOutcome::Recording);
        assert!(began.elapsed() < Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(50));
        assert!(gate.poll().is_empty());
        assert!(!fx.record_file().exists());
        hold.open();
        drain(&mut gate, |seen| {
            seen.contains(&GateEvent::Recorded { tab: A, notice: None })
        });
        assert!(fx.record_file().is_file());
    }

    #[test]
    fn a_failing_state_dir_reports_and_starts_nothing() {
        let fx = Fx::new("statefile");
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        std::fs::create_dir_all(fx.state.join("eitri")).unwrap();
        std::fs::write(fx.state.join("eitri/trust"), b"i am a file\n").unwrap();
        let mut gate = fx.gate();
        start(&mut gate, A);
        let view = gate.show(A, 1, &CLAUDE).unwrap();
        assert_eq!(view.remember, TrustRememberView::Yes);
        let began = Instant::now();
        assert_eq!(answer(&mut gate, &view, true), AnswerOutcome::Recording);
        assert!(began.elapsed() < Duration::from_millis(50));
        let seen = drain(&mut gate, |seen| {
            seen.iter().any(|e| matches!(e, GateEvent::RecordRefused { .. }))
        });
        let notice = seen
            .iter()
            .find_map(|e| match e {
                GateEvent::RecordRefused { tab, notice } if *tab == A => Some(notice.clone()),
                _ => None,
            })
            .unwrap();
        assert!(notice.starts_with("trust could not be recorded ("), "{notice}");
        assert!(notice.ends_with("); asking again for this window only"), "{notice}");
        assert!(!seen.contains(&GateEvent::Recorded { tab: A, notice: None }));
        let again = gate.show(A, 2, &CLAUDE).unwrap();
        assert_eq!(again.remember, TrustRememberView::Window);
        assert!(again
            .remember_note
            .as_deref()
            .unwrap()
            .starts_with("trust could not be recorded ("));
        // Its `y` now trusts the window without trying the record again, once the disk is found
        // unchanged.
        assert_eq!(answer(&mut gate, &again, true), AnswerOutcome::Recording);
        drain(&mut gate, |seen| {
            seen.iter().any(|e| {
                matches!(
                    e,
                    GateEvent::Recorded {
                        tab: A,
                        notice: Some(_)
                    }
                )
            })
        });
        assert!(!fx.record_file().exists());
        assert_eq!(start(&mut gate, B), Route::Start(ProjectTrust::Trusted));
    }

    /// A `y` that cannot be recorded still waits for the disk to be read again: a configuration
    /// edited while the prompt was on screen is asked about, never loaded unseen, and the window
    /// remembers nothing.
    #[test]
    fn a_window_only_y_whose_disk_changed_since_the_prompt_starts_nothing() {
        let fx = Fx::new("window-moved");
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        let hold = WorkerHold::default();
        let mut gate = TrustGate::new_held(
            TrustStore::at(None),
            fx.root.clone(),
            Some(fx.home.clone()),
            hold.clone(),
        );
        assert_eq!(start(&mut gate, A), Route::Ask);
        let view = gate.show(A, 1, &CLAUDE).unwrap();
        assert_eq!(view.remember, TrustRememberView::Window);
        hold.close();
        assert_eq!(answer(&mut gate, &view, true), AnswerOutcome::Recording);
        fx.write(SETTINGS, &hook("touch /tmp/evil"));
        hold.open();
        let seen = drain(&mut gate, |seen| {
            seen.iter().any(|e| matches!(e, GateEvent::RecordRefused { .. }))
        });
        assert!(seen.contains(&GateEvent::RecordRefused {
            tab: A,
            notice: CHANGED_WHILE_READING.to_string()
        }));
        assert!(
            !seen.iter().any(|e| matches!(e, GateEvent::Recorded { .. })),
            "{seen:?}"
        );
        assert_eq!(start(&mut gate, B), Route::Ask, "the window remembers nothing");
        let again = gate.show(B, 2, &CLAUDE).unwrap();
        assert_ne!(again.fingerprint, view.fingerprint, "asked about what is there now");
        assert!(again.items.iter().any(|i| i.value == "touch /tmp/evil"));
    }

    /// The same for a `y` that covers one start, and for `:trust` in a window that cannot keep a
    /// record.
    #[test]
    fn a_session_only_or_command_y_rereads_the_disk_first() {
        let fx = Fx::new("session-moved");
        fx.write(SETTINGS, &padded_hook("touch /tmp/a", 5 << 20));
        fx.write(".mcp.json", br#"{"mcpServers":{}}"#);
        let hold = WorkerHold::default();
        let mut gate = fx.held(&hold);
        assert_eq!(start(&mut gate, A), Route::Ask);
        let view = gate.show(A, 1, &CLAUDE).unwrap();
        assert_eq!(view.remember, TrustRememberView::Session);
        hold.close();
        assert_eq!(answer(&mut gate, &view, true), AnswerOutcome::Recording);
        fx.write(".mcp.json", br#"{"mcpServers":{"m":{"command":"sh"}}}"#);
        hold.open();
        let seen = drain(&mut gate, |seen| {
            seen.iter().any(|e| matches!(e, GateEvent::RecordRefused { .. }))
        });
        assert!(
            !seen.iter().any(|e| matches!(e, GateEvent::Recorded { .. })),
            "{seen:?}"
        );

        let fx = Fx::new("command-moved");
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        let mut gate = TrustGate::new_held(
            TrustStore::at(None),
            fx.root.clone(),
            Some(fx.home.clone()),
            hold.clone(),
        );
        command(&mut gate, "c1", TrustAction::Trust);
        let view = gate.show_command(A, "c1").unwrap();
        hold.close();
        assert_eq!(
            gate.answer_command(A, view.nonce, &view.fingerprint, &view.findings_digest(), true, 1),
            Some(CommandAnswer::Recording)
        );
        fx.write(SETTINGS, &hook("touch /tmp/evil"));
        hold.open();
        let seen = drain(&mut gate, |seen| command_done(seen, "c1").is_some());
        assert_eq!(
            command_done(&seen, "c1"),
            Some((false, Some(CHANGED_WHILE_READING.to_string())))
        );
        assert_eq!(start(&mut gate, B), Route::Ask, "the window remembers nothing");
    }

    /// Two checks that land between two polls: the earlier found the recorded configuration, the
    /// later found it changed. The tab the earlier one answered is routed by the later, so it is
    /// asked rather than started with the project's tiers.
    #[test]
    fn a_trusted_route_is_overtaken_by_a_newer_check_in_the_same_batch() {
        let fx = Fx::new("batch-route");
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        fx.trust_now(1_790_000_000);
        let trusted = eitri_core::project_trust::for_root(
            &fx.store(),
            &fx.root,
            Some(&fx.home),
            &Profile::Claude,
            &ProfileEnv::default(),
        );
        assert!(matches!(trusted, TrustState::Trusted { .. }));
        fx.write(SETTINGS, &hook("touch /tmp/evil"));
        let changed = eitri_core::project_trust::for_root(
            &fx.store(),
            &fx.root,
            Some(&fx.home),
            &Profile::Claude,
            &ProfileEnv::default(),
        );
        assert!(matches!(changed, TrustState::Changed { .. }));

        let (mut gate, _jobs, done) = TrustGate::detached(fx.root.clone());
        gate.wait_for(A, &CLAUDE);
        let first = gate.begin_check(&CLAUDE);
        gate.wait_for(B, &CLAUDE);
        let second = gate.begin_check(&CLAUDE);
        done.send(Done::Checked {
            generation: first,
            state: trusted.clone(),
        })
        .unwrap();
        done.send(Done::Checked {
            generation: second,
            state: changed,
        })
        .unwrap();
        let seen = gate.poll();
        assert!(seen.contains(&GateEvent::Route(A, Route::Ask)), "{seen:?}");
        assert!(seen.contains(&GateEvent::Route(B, Route::Ask)), "{seen:?}");
        assert!(
            !seen.contains(&GateEvent::Route(A, Route::Start(ProjectTrust::Trusted))),
            "{seen:?}"
        );

        // The other way round the newest result is the trusted one, and both start with it.
        let (mut gate, _jobs, done) = TrustGate::detached(fx.root.clone());
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        let untrusted_first = eitri_core::project_trust::for_root(
            &TrustStore::at(None),
            &fx.root,
            Some(&fx.home),
            &Profile::Claude,
            &ProfileEnv::default(),
        );
        gate.wait_for(A, &CLAUDE);
        let first = gate.begin_check(&CLAUDE);
        gate.wait_for(B, &CLAUDE);
        let second = gate.begin_check(&CLAUDE);
        done.send(Done::Checked {
            generation: first,
            state: untrusted_first,
        })
        .unwrap();
        done.send(Done::Checked {
            generation: second,
            state: trusted,
        })
        .unwrap();
        assert_eq!(
            gate.poll(),
            vec![
                GateEvent::Route(A, Route::Start(ProjectTrust::Trusted)),
                GateEvent::Route(B, Route::Start(ProjectTrust::Trusted)),
            ]
        );
    }

    /// A record written for a tab's `y`, then, in the same batch, a check that finds the
    /// configuration changed since: the tab is asked again rather than started with the tiers.
    #[test]
    fn a_record_overtaken_by_a_newer_check_in_the_same_batch_is_refused() {
        let fx = Fx::new("batch-record");
        fx.write(SETTINGS, &hook("touch /tmp/a"));
        let untrusted = eitri_core::project_trust::for_root(
            &fx.store(),
            &fx.root,
            Some(&fx.home),
            &Profile::Claude,
            &ProfileEnv::default(),
        );
        let (mut gate, jobs, done) = TrustGate::detached(fx.root.clone());
        gate.wait_for(A, &CLAUDE);
        let first = gate.begin_check(&CLAUDE);
        done.send(Done::Checked {
            generation: first,
            state: untrusted,
        })
        .unwrap();
        assert_eq!(gate.poll(), vec![GateEvent::Route(A, Route::Ask)]);
        let view = gate.show(A, 1, &CLAUDE).unwrap();
        assert_eq!(answer(&mut gate, &view, true), AnswerOutcome::Recording);
        let _ = jobs.try_recv();
        let shown = eitri_core::project_trust::discover(
            &fx.root,
            Some(&fx.home),
            &Default::default(),
            &Profile::Claude,
            &ProfileEnv::default(),
        );
        fx.store().record(&shown, 1).unwrap();
        fx.write(SETTINGS, &hook("touch /tmp/evil"));
        let changed = eitri_core::project_trust::for_root(
            &fx.store(),
            &fx.root,
            Some(&fx.home),
            &Profile::Claude,
            &ProfileEnv::default(),
        );
        gate.wait_for(B, &CLAUDE);
        let second = gate.begin_check(&CLAUDE);
        done.send(Done::Recorded {
            purpose: Purpose::ForTab(A),
            epoch: 0,
            state: TrustState::Trusted {
                since_unix: 1,
                discovery: shown,
            },
        })
        .unwrap();
        done.send(Done::Checked {
            generation: second,
            state: changed,
        })
        .unwrap();
        let seen = gate.poll();
        assert!(
            seen.contains(&GateEvent::RecordRefused {
                tab: A,
                notice: CHANGED_WHILE_READING.to_string()
            }),
            "{seen:?}"
        );
        assert!(
            !seen.iter().any(|e| matches!(e, GateEvent::Recorded { .. })),
            "{seen:?}"
        );
        assert!(seen.contains(&GateEvent::Route(B, Route::Ask)), "{seen:?}");
    }

    #[test]
    fn commands_only_queue() {
        let fx = Fx::new("cmdqueue");
        let hold = WorkerHold::closed();
        let mut gate = fx.held(&hold);
        let began = Instant::now();
        gate.command("t".into(), TrustAction::Trust, A, &CLAUDE);
        gate.command("u".into(), TrustAction::Untrust, A, &CLAUDE);
        assert!(began.elapsed() < Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(50));
        assert!(gate.poll().is_empty());
        hold.open();
        let seen = drain(&mut gate, |seen| {
            command_done(seen, "t").is_some() && command_done(seen, "u").is_some()
        });
        assert_eq!(command_done(&seen, "t"), Some((true, Some(NOTHING_HERE.to_string()))));
        assert_eq!(
            command_done(&seen, "u"),
            Some((true, Some(WAS_NOT_TRUSTED.to_string())))
        );
    }

    #[test]
    fn the_view_names_every_finding() {
        let p = PathBuf::from;
        let findings = vec![
            Finding::Hook {
                file: p(SETTINGS),
                event: "PreToolUse".into(),
                matcher: Some("Bash".into()),
                command: "run".into(),
            },
            Finding::Hook {
                file: p(SETTINGS),
                event: "SessionStart".into(),
                matcher: None,
                command: "go".into(),
            },
            Finding::McpServer {
                file: p(".mcp.json"),
                name: "m".into(),
                command_line: "sh -c x".into(),
                approval_mode: None,
            },
            Finding::McpServer {
                file: p(".codex/config.toml"),
                name: "c".into(),
                command_line: "srv".into(),
                approval_mode: Some("approve".into()),
            },
            Finding::EnvKey {
                file: p(SETTINGS),
                key: "K".into(),
            },
            Finding::ApiKeyHelper {
                file: p(SETTINGS),
                command: "helper".into(),
            },
            Finding::Allow {
                file: p(SETTINGS),
                rule: "Bash(ls)".into(),
            },
            Finding::AdditionalDirectory {
                file: p(SETTINGS),
                path: "/x".into(),
            },
            Finding::OtherSetting {
                file: p(SETTINGS),
                key: "model".into(),
                value: "\"m\"".into(),
            },
            Finding::Unparsed {
                file: p(".mcp.json"),
                reason: "bad".into(),
            },
            Finding::Memory { path: p("CLAUDE.md") },
            Finding::RuleFile {
                path: p(".codex/rules/a.rules"),
                calls: vec![
                    "prefix_rule(pattern=[\"ls\"])".into(),
                    "prefix_rule(pattern=[\"pwd\"])".into(),
                ],
                text: None,
            },
            Finding::RuleFile {
                path: p(".codex/rules/b.rules"),
                calls: vec![],
                text: None,
            },
            Finding::RuleFile {
                path: p(".codex/rules/c.rules"),
                calls: vec![],
                text: Some("r = prefix_rule\nr(pattern=[\"rm\"], decision=\"allow\")\n".into()),
            },
            Finding::Skill {
                path: p(".agents/skills/s"),
                name: "s".into(),
            },
            Finding::PluginListing {
                path: p(".claude-plugin/marketplace.json"),
            },
            Finding::Overridden {
                file: p(".codex/config.toml"),
                key: "sandbox_mode".into(),
                value: "\"danger-full-access\"".into(),
            },
            Finding::Symlink {
                path: p(".claude/hooks/run"),
                target: p("/opt/x/run"),
                outside: true,
            },
            Finding::OtherFiles {
                dir: p(".claude/agents"),
                count: 3,
            },
            Finding::NotRead {
                path: p(".claude/worktrees"),
                reason: "kept apart".into(),
            },
            Finding::Unreadable {
                path: p(".claude/big"),
                reason: "too large".into(),
            },
            Finding::OverBudget {
                reason: "too many".into(),
            },
            Finding::GitUnverified {
                path: p(".git"),
                reason: "a link".into(),
                target: Some(p("/elsewhere")),
            },
        ];
        let discovery = Discovery {
            profile: Profile::Claude,
            root: p("/home/u/p"),
            top: p("/home/u/p"),
            walked: vec![p("/home/u/p")],
            entries: vec![Entry {
                path: p(SETTINGS),
                kind: EntryKind::File {
                    sha256: [1; 32],
                    len: 1,
                },
            }],
            findings,
            fingerprint: Fingerprint([2; 32]),
            over_budget: None,
            user_mcp_servers: None,
        };
        let (mut gate, _jobs, done) = TrustGate::detached(p("/home/u/p"));
        done.send(Done::Checked {
            generation: 0,
            state: TrustState::Untrusted {
                discovery,
                remember: Remember::Yes,
            },
        })
        .unwrap();
        gate.poll();
        let view = gate.show(A, 1, &CLAUDE).unwrap();
        let rows: Vec<(&str, &str, &str, &str, bool)> = view
            .items
            .iter()
            .map(|i| (i.what, i.file.as_str(), i.label.as_str(), i.value.as_str(), i.outside))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("hook", SETTINGS, "PreToolUse (Bash)", "run", false),
                ("hook", SETTINGS, "SessionStart", "go", false),
                ("mcp_server", ".mcp.json", "m", "sh -c x", false),
                (
                    "mcp_server",
                    ".codex/config.toml",
                    "c (tools approved: approve)",
                    "srv",
                    false,
                ),
                ("env_key", SETTINGS, "env", "K", false),
                ("api_key_helper", SETTINGS, "apiKeyHelper", "helper", false),
                ("allow", SETTINGS, "permissions.allow", "Bash(ls)", false),
                ("additional_directory", SETTINGS, "additionalDirectories", "/x", false),
                ("other_setting", SETTINGS, "model", "\"m\"", false),
                ("unparsed", ".mcp.json", "not parsed", "bad", false),
                ("memory", "CLAUDE.md", "memory file", "present", false),
                (
                    "rule",
                    ".codex/rules/a.rules",
                    "exec-policy rule",
                    "prefix_rule(pattern=[\"ls\"])",
                    false,
                ),
                (
                    "rule",
                    ".codex/rules/a.rules",
                    "exec-policy rule",
                    "prefix_rule(pattern=[\"pwd\"])",
                    false,
                ),
                (
                    "rule",
                    ".codex/rules/b.rules",
                    "exec-policy rules",
                    "only comments",
                    false
                ),
                (
                    "rule",
                    ".codex/rules/c.rules",
                    "exec-policy rules, whole file",
                    "r = prefix_rule\nr(pattern=[\"rm\"], decision=\"allow\")\n",
                    false,
                ),
                ("skill", ".agents/skills/s", "skill", "s", false),
                (
                    "plugin_listing",
                    ".claude-plugin/marketplace.json",
                    "plugin listing",
                    "present",
                    false,
                ),
                (
                    "overridden",
                    ".codex/config.toml",
                    "sandbox_mode",
                    "\"danger-full-access\"",
                    false,
                ),
                ("symlink", ".claude/hooks/run", ".claude/hooks/run", "/opt/x/run", true),
                ("other_files", ".claude/agents", "other files", "3", false),
                ("not_read", ".claude/worktrees", "not read", "kept apart", false),
                ("unreadable", ".claude/big", "cannot be checked", "too large", false),
                ("over_budget", "", "over budget", "too many", false),
                ("git_unverified", ".git", "git", "a link -> /elsewhere", false),
            ]
        );
        assert_eq!(view.root, "/home/u/p");
        assert_eq!(view.fingerprint, Fingerprint([2; 32]).hex());
    }

    fn own_source() -> String {
        scan::code_only(
            &std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/trust_gate.rs")).unwrap(),
        )
    }

    /// The disk is touched by the worker only: outside `run_worker`, the helper it reads the user's
    /// Codex configuration with (and these tests) no discovery, no record read, write or removal, and
    /// no file system call at all.
    #[test]
    fn only_the_worker_touches_the_disk() {
        let code = own_source();
        let worker = scan::functions(&code, "run_worker");
        assert_eq!(worker.len(), 1, "run_worker not found");
        for anchor in [
            "for_root",
            "discover",
            ".state(",
            ".record(",
            ".forget(",
            "std::fs",
            "canonicalize",
        ] {
            assert!(
                worker[0].contains(anchor),
                "the scan would not see `{anchor}` in run_worker"
            );
        }
        // The one helper the worker reads the user's Codex configuration with; called from nowhere
        // else (checked below, since it is cut out with the worker).
        let helper = scan::functions(&code, "read_codex_user_config");
        assert_eq!(helper.len(), 1, "read_codex_user_config not found");
        assert!(
            worker[0].contains("read_codex_user_config"),
            "the worker reads the user's config"
        );
        let tests = scan::modules(&code, "tests");
        assert_eq!(tests.len(), 1, "the tests module not found");
        let mut cut: Vec<&str> = worker;
        cut.extend(helper);
        cut.extend(tests);
        let rest = scan::without(&code, &cut);
        assert_eq!(
            scan::word_ends(&rest, "read_codex_user_config").len(),
            1,
            "read_codex_user_config is named outside the worker (beyond its own definition)"
        );
        for word in ["for_root", "discover", "canonicalize"] {
            assert!(scan::word_ends(&rest, word).is_empty(), "`{word}` outside the worker");
        }
        for needle in [".state(", ".record(", ".forget(", "std::fs", "fs::"] {
            assert!(!rest.contains(needle), "`{needle}` outside the worker");
        }
    }

    /// Terminal `claude`'s own trust flag lives in the account's config file; neither the gate nor
    /// the panel that wires it ever names that file.
    #[test]
    fn the_gate_never_names_the_accounts_config_file() {
        const SCANNED: &[&str] = &["shell/src", "panel/src"];
        let read = scan::rust_sources(SCANNED);
        for needle in ["struct TrustGate", "struct AgentPanelState"] {
            let source = scan::file_with(&read, needle);
            assert!(
                !source.text.contains(concat!(".claude", ".json")),
                "{} names it",
                source.path
            );
        }
    }

    // ---- one gate, several agents ---------------------------------------------------------------

    const CODEX: Profile = Profile::Codex;

    fn gem() -> Profile {
        Profile::Acp {
            name: "gem".into(),
            files: vec!["GEM.md".into()],
        }
    }

    /// A start of `profile` for `tab`: waits for a fresh check of it and returns its route.
    fn start_as(gate: &mut TrustGate, tab: TabId, profile: &Profile) -> Route {
        gate.wait_for(tab, profile);
        gate.begin_check(profile);
        let seen = drain(gate, |seen| {
            seen.iter().any(|e| matches!(e, GateEvent::Route(t, _) if *t == tab))
        });
        seen.into_iter()
            .find_map(|e| match e {
                GateEvent::Route(t, route) if t == tab => Some(route),
                _ => None,
            })
            .unwrap()
    }

    fn item_files(view: &TrustPromptView) -> Vec<String> {
        view.items.iter().map(|item| item.file.clone()).collect()
    }

    /// A Claude tab and a Codex tab wait at once: two checks, each with its own tab's profile, two
    /// questions about two different sets of files under two nonces, and a check of one agent never
    /// takes the other's question off.
    #[test]
    fn two_waiting_tabs_of_two_agents_get_two_questions() {
        let fx = Fx::new("two-agents");
        fx.write(SETTINGS, &hook("touch /tmp/claude"));
        fx.write("AGENTS.md", b"codex instructions\n");
        let mut gate = fx.gate();
        gate.wait_for(A, &CLAUDE);
        gate.begin_check(&CLAUDE);
        gate.wait_for(B, &CODEX);
        gate.begin_check(&CODEX);
        let seen = drain(&mut gate, |seen| {
            seen.iter().filter(|e| matches!(e, GateEvent::Route(..))).count() == 2
        });
        assert!(seen.contains(&GateEvent::Route(A, Route::Ask)), "{seen:?}");
        assert!(seen.contains(&GateEvent::Route(B, Route::Ask)), "{seen:?}");

        let claude = gate.show(A, 1, &CLAUDE).unwrap();
        // A newer Codex check does not touch the Claude question.
        gate.wait_for(B, &CODEX);
        gate.begin_check(&CODEX);
        let seen = drain(&mut gate, |seen| {
            seen.iter().any(|e| matches!(e, GateEvent::Route(t, _) if *t == B))
        });
        assert!(!seen.contains(&GateEvent::Reask(A)), "{seen:?}");
        let codex = gate.show(B, 2, &CODEX).unwrap();
        assert_ne!(claude.fingerprint, codex.fingerprint);
        assert_ne!(claude.nonce, codex.nonce);
        assert_eq!(item_files(&claude), [SETTINGS]);
        assert_eq!(item_files(&codex), ["AGENTS.md"]);

        // Each answer is about its own agent, and each agent's first `n` says so once.
        assert_eq!(
            answer(&mut gate, &claude, false),
            AnswerOutcome::Start(ProjectTrust::Untrusted, Some(NOT_LOADED.to_string()))
        );
        assert_eq!(
            answer(&mut gate, &codex, false),
            AnswerOutcome::Start(
                ProjectTrust::Untrusted,
                Some("this project's own Codex configuration is not loaded".to_string())
            )
        );
        assert_eq!(gate.row(&CLAUDE), "not trusted (answered n in this window)");
        assert_eq!(gate.row(&CODEX), "not trusted (answered n in this window)");
    }

    /// A `y` in a Codex tab writes Codex's record and remembers Codex's answer; a Claude start in the
    /// same window is still asked, and Claude's record is never written.
    #[test]
    fn a_codex_yes_does_not_trust_claude() {
        let fx = Fx::new("codex-yes");
        fx.write(SETTINGS, &hook("touch /tmp/claude"));
        fx.write(".codex/config.toml", b"[mcp_servers.srv]\ncommand = \"srv\"\n");
        let mut gate = fx.gate();
        assert_eq!(start_as(&mut gate, B, &CODEX), Route::Ask);
        let view = gate.show(B, 1, &CODEX).unwrap();
        assert_eq!(answer(&mut gate, &view, true), AnswerOutcome::Recording);
        let seen = drain(&mut gate, |seen| {
            seen.iter().any(|e| matches!(e, GateEvent::Recorded { .. }))
        });
        assert!(seen.contains(&GateEvent::Recorded { tab: B, notice: None }), "{seen:?}");
        let codex_record = PathBuf::from(format!("{}.codex", fx.record_file().display()));
        assert!(codex_record.is_file());
        assert!(
            !fx.record_file().exists(),
            "Claude's record is not written by a Codex y"
        );

        assert_eq!(start_as(&mut gate, A, &CLAUDE), Route::Ask);
        assert_eq!(gate.row(&CLAUDE), "not trusted");
        assert_eq!(gate.next_session_trust(&CLAUDE), ProjectTrust::Untrusted);
        assert!(gate.row(&CODEX).starts_with("trusted since "), "{}", gate.row(&CODEX));
        assert_eq!(gate.next_session_trust(&CODEX), ProjectTrust::Trusted);
        // Another Codex start in this window is let through by the record.
        assert_eq!(start_as(&mut gate, B, &CODEX), Route::Start(ProjectTrust::Trusted));

        // `:untrust` in the Claude tab leaves Codex's record alone.
        gate.command("u".into(), TrustAction::Untrust, A, &CLAUDE);
        drain(&mut gate, |seen| command_done(seen, "u").is_some());
        assert!(codex_record.is_file());
        assert_eq!(start_as(&mut gate, B, &CODEX), Route::Start(ProjectTrust::Trusted));
    }

    /// Claude's record cannot be written (its path is a directory) while Codex's can: the failure makes
    /// Claude's later questions window-only, and leaves Codex's question, and its record, as they were.
    #[test]
    fn a_record_failure_of_one_agent_leaves_the_other_s_record_alone() {
        let fx = Fx::new("record-failure-per-agent");
        fx.write(SETTINGS, &hook("touch /tmp/claude"));
        fx.write("AGENTS.md", b"codex instructions\n");
        std::fs::create_dir_all(fx.record_file().join("in-the-way")).unwrap();
        let mut gate = fx.gate();
        assert_eq!(start_as(&mut gate, A, &CLAUDE), Route::Ask);
        let claude = gate.show(A, 1, &CLAUDE).unwrap();
        assert_eq!(claude.agent, "Claude");
        assert_eq!(answer(&mut gate, &claude, true), AnswerOutcome::Recording);
        drain(&mut gate, |seen| {
            seen.iter()
                .any(|e| matches!(e, GateEvent::RecordRefused { tab, .. } if *tab == A))
        });
        assert_eq!(gate.show(A, 2, &CLAUDE).unwrap().remember, TrustRememberView::Window);

        assert_eq!(start_as(&mut gate, B, &CODEX), Route::Ask);
        let codex = gate.show(B, 3, &CODEX).unwrap();
        assert_eq!(codex.agent, "Codex");
        assert_eq!(codex.remember, TrustRememberView::Yes);
        assert_eq!(codex.remember_note, None);
        assert_eq!(answer(&mut gate, &codex, true), AnswerOutcome::Recording);
        let seen = drain(&mut gate, |seen| {
            seen.iter()
                .any(|e| matches!(e, GateEvent::Recorded { tab, .. } if *tab == B))
        });
        assert!(seen.contains(&GateEvent::Recorded { tab: B, notice: None }), "{seen:?}");
        assert!(PathBuf::from(format!("{}.codex", fx.record_file().display())).is_file());
        // Codex's record written does not clear Claude's failure.
        assert_eq!(gate.show(A, 4, &CLAUDE).unwrap().remember, TrustRememberView::Window);
    }

    /// The user's Codex `config.toml` is read through a link the user made, as Codex reads it, and a
    /// link to something other than a regular file still leaves the servers unknown.
    #[test]
    fn the_user_codex_config_is_read_through_a_link() {
        let fx = Fx::new("codex-user-link");
        let dotfiles = fx.outer.join("dotfiles/codex.toml");
        std::fs::create_dir_all(dotfiles.parent().unwrap()).unwrap();
        std::fs::write(&dotfiles, b"[mcp_servers.context7]\ncommand = \"npx\"\n").unwrap();
        let codex_home = fx.home.join(".codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        std::os::unix::fs::symlink(&dotfiles, codex_home.join("config.toml")).unwrap();
        assert_eq!(
            read_codex_user_config(&codex_home).mcp_servers,
            Some(vec!["context7".to_string()])
        );

        let to_dir = fx.outer.join("to-dir");
        std::fs::create_dir_all(&to_dir).unwrap();
        std::os::unix::fs::symlink(fx.outer.join("dotfiles"), to_dir.join("config.toml")).unwrap();
        assert_eq!(read_codex_user_config(&to_dir).mcp_servers, None);
        assert_eq!(
            read_codex_user_config(&fx.outer.join("absent")).mcp_servers,
            Some(Vec::new())
        );
    }

    /// `CODEX_HOME` as Codex reads it: unset or empty is `~/.codex`, an absolute one is used as is,
    /// and a relative one, which Codex resolves against its own working directory, skips no
    /// project `.codex/` and reads the alternate names from below the root.
    #[test]
    fn a_relative_codex_home_skips_nothing() {
        let home = Path::new("/h");
        let root = Path::new("/h/p");
        let both = |dir: &str| CodexHomeGuess {
            skip: Some(PathBuf::from(dir)),
            config: Some(PathBuf::from(dir)),
        };
        assert_eq!(codex_home_guess(None, Some(home), root), both("/h/.codex"));
        assert_eq!(
            codex_home_guess(Some(std::ffi::OsStr::new("")), Some(home), root),
            both("/h/.codex")
        );
        assert_eq!(
            codex_home_guess(Some(std::ffi::OsStr::new("/elsewhere/codex")), Some(home), root),
            both("/elsewhere/codex")
        );
        assert_eq!(
            codex_home_guess(Some(std::ffi::OsStr::new("../.codex")), Some(home), root),
            CodexHomeGuess {
                skip: None,
                config: Some(PathBuf::from("/h/p/../.codex")),
            }
        );
        assert_eq!(
            codex_home_guess(None, None, root),
            CodexHomeGuess {
                skip: None,
                config: None
            }
        );
    }

    /// A relative `CODEX_HOME` names a directory the project may hold: its `config.toml` is not known
    /// to be the user's own, so the servers it lists are not taken as the user's, and an untrusted
    /// start still watches for every server the project gives. A home that is the user's own still
    /// leaves out the servers it starts itself.
    #[test]
    fn a_relative_codex_home_does_not_whitelist_project_servers() {
        let fx = Fx::new("codex-relative-home");
        fx.write(".codex/config.toml", b"[mcp_servers.evil]\ncommand = \"evil\"\n");
        let plan_for = |var: Option<&str>| {
            let guess = codex_home_guess(var.map(std::ffi::OsStr::new), Some(&fx.home), &fx.root);
            let user = read_codex_user_config(guess.config.as_deref().unwrap());
            let env = codex_profile_env(guess.skip, user);
            let found =
                eitri_core::project_trust::discover(&fx.root, Some(&fx.home), &Default::default(), &CODEX, &env);
            eitri_core::project_trust::trust_plan(&found, agent::DirTrust::Untrusted, &fx.root)
                .unwrap()
                .project_only
        };
        assert_eq!(plan_for(Some(".codex")), vec!["evil".to_string()]);
        assert_eq!(plan_for(Some("./.codex")), vec!["evil".to_string()]);

        let own = fx.home.join(".codex");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(own.join("config.toml"), b"[mcp_servers.evil]\ncommand = \"mine\"\n").unwrap();
        fx.write(
            ".codex/config.toml",
            b"[mcp_servers.evil]\ncommand = \"evil\"\n[mcp_servers.other]\ncommand = \"o\"\n",
        );
        assert_eq!(plan_for(None), vec!["other".to_string()]);
    }

    /// A registered agent cannot leave the project's configuration out, so its `n` starts nothing and
    /// is not remembered, and a project with none of its files is still asked about: a `y` records the
    /// empty configuration, and a file that appears later asks again.
    #[test]
    fn acp_profile_n_refuses_and_empty_asks() {
        let fx = Fx::new("acp-explicit");
        let acp = gem();
        let mut gate = fx.gate();
        // Claude in the same empty root starts at once, without the project's tiers.
        assert_eq!(start_as(&mut gate, A, &CLAUDE), Route::Start(ProjectTrust::Untrusted));
        assert_eq!(start_as(&mut gate, B, &acp), Route::Ask);
        let view = gate.show(B, 1, &acp).expect("an empty project is still asked about");
        assert!(view.items.is_empty());
        assert_eq!(
            answer(&mut gate, &view, false),
            AnswerOutcome::NotStarted(refused_without_yes(&acp))
        );
        assert!(refused_without_yes(&acp).starts_with("gem would load this project's own configuration"));
        // Not remembered: the next start asks again.
        assert_eq!(start_as(&mut gate, B, &acp), Route::Ask);
        let view = gate.show(B, 2, &acp).unwrap();
        assert_eq!(answer(&mut gate, &view, true), AnswerOutcome::Recording);
        let seen = drain(&mut gate, |seen| {
            seen.iter().any(|e| matches!(e, GateEvent::Recorded { .. }))
        });
        assert!(seen.contains(&GateEvent::Recorded { tab: B, notice: None }), "{seen:?}");
        assert!(PathBuf::from(format!("{}.acp-gem", fx.record_file().display())).is_file());
        assert_eq!(start_as(&mut gate, B, &acp), Route::Start(ProjectTrust::Trusted));
        fx.write("GEM.md", b"appeared later\n");
        assert_eq!(start_as(&mut gate, B, &acp), Route::Ask);
    }
}
