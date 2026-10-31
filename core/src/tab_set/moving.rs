//! A tab leaving this window's tab set for the other window's, and the wait every turn start makes for
//! this window's presence.
//!
//! **A move.** [`TabSet::freeze_for_move`] stops the tab: it takes no input and answers no card, its
//! conversation stops folding, and the pump runs for it one last time, so whatever was folded and not yet
//! delivered is answered (bypass, a saved rule, auto) and noted here, where the record is taken. The
//! record ([`TabHandoff`]) is built by naming every field of the tab, so a field added later does not
//! compile until it is placed in the record or named as staying behind. The other window takes it up with
//! [`TabSet::adopt_moved`], a path of its own that is neither a `Resync` nor a backend install, so the
//! approvals, answered sets and note candidates it carries survive the first tick and the first delivery.
//! Once the move is done the tab leaves through [`TabSet::hand_over`], which ends nothing: no end
//! snapshot, no retiring count, only the presence file going. A move that does not happen ends with
//! [`TabSet::unfreeze`].
//!
//! **Turn starts.** A turn of Eitri's is in the running set from just before its prompt goes out
//! (`starting`) until its `TurnStarted` is folded, so neither this window nor its linked partner reverts
//! while the turn may be starting. In a linked window the prompt goes out only once the presence thread
//! has confirmed the turn's file, or the partner has acknowledged counting this window as another window
//! ([`SendGate`]); alone, it goes out at once, as it always did.

use super::*;
use crate::agent_backend::{BackendHandoff, HumanApprovals};
use crate::turn_review::{Decision, Held, Partner, PresenceFallback, ReviewDraft, ReviewHandoff};
use std::time::{Duration, Instant};

/// How long a move waits for the running turn's session id, which its review is filed under, before
/// the move is refused.
const SESSION_ID_WAIT: Duration = Duration::from_secs(2);

/// A tab's record as it crosses to the other window: every field of the tab and of its session state,
/// placed here or named where the record is built as staying behind. Not moved: the tab's id and number
/// (the other window allocates its own), its view (the other window says which of its views shows it),
/// its backend (the provider is adopted, the conversation rebuilt from its own records) and its review
/// watchers (handles into this window's review).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TabHandoff {
    name: Option<String>,
    mode: SessionModeChoice,
    agent: AgentId,
    state: MovedState,
    attention: MovedAttention,
    failed_after_start: bool,
    failed_resume: Option<String>,
    last_handoff: Option<MovedResumeCommand>,
    editing_draft: Option<u64>,
    review_draft: ReviewDraft,
    notes: CallNotes,
    title: Option<String>,
    title_from_record: bool,
    project_tiers: Option<MovedTiers>,
    stale: bool,
    reported_start_failure: bool,
    queue: Vec<Queued>,
    /// A live transfer preserves keyboard admission; ordinary queue deserialization does not.
    #[serde(default)]
    queue_typed_by_person: Vec<bool>,
    #[serde(default)]
    opening_prompt: Option<OpeningPrompt>,
    queue_error: Option<String>,
    queue_note: Option<String>,
    draft: String,
    question_draft: BTreeMap<String, serde_json::Value>,
    editing_question: Option<(String, u32)>,
    /// Each waiting card's offered rule, in the rules file's own syntax: read back through the rule
    /// parser, so the record cannot carry a rule the parser would refuse.
    rule_offers: BTreeMap<String, String>,
    was_running: bool,
    note_ledger: NoteLedger,
    creates_file_unsent: BTreeSet<String>,
    rule_offers_seen: Vec<String>,
    turn_clock: Option<(String, u64)>,
    /// Crosses with `turn_clock`: the destination would otherwise report the turn in progress as begun.
    #[serde(default)]
    last_began_turn: Option<String>,
    turn_times_unsent: usize,
    host_answered: BTreeSet<String>,
    human_allowed: HumanApprovals,
    user_answered: BTreeSet<String>,
    /// The session's turn review, for the window the tab lands in to go on reviewing it.
    review: Option<ReviewHandoff>,
    #[cfg(any(test, feature = "test-support"))]
    #[serde(default)]
    gate_log: Vec<crate::agent_backend::GateAnswer>,
}

impl TabHandoff {
    /// The agent the tab runs.
    pub fn agent(&self) -> &AgentId {
        &self.agent
    }

    /// The tab's permission mode, which moves with it.
    pub fn mode(&self) -> SessionModeChoice {
        self.mode
    }

    /// Whether the tab held a session: it is taken up with its adopted backend
    /// ([`TabSet::adopt_moved`]), else on its own ([`TabSet::adopt_moved_unstarted`]).
    pub fn has_session(&self) -> bool {
        matches!(self.state, MovedState::Live)
    }

    /// The session's turn review the record carries.
    pub fn review(&self) -> Option<&ReviewHandoff> {
        self.review.as_ref()
    }
}

/// What the tab held instead of a session, or that it held one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum MovedState {
    Empty,
    /// A start put off for the trust question: the window it lands in asks again, in its own page.
    AwaitingTrust(MovedStart),
    Failed {
        reason: String,
    },
    Live,
}

/// A start put off for the trust question, as it crosses. The command it answered was the other page's;
/// the window it lands in owes nobody a result for it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum MovedStart {
    FirstSend(FirstTurn),
    Resume {
        provider_session_id: String,
        resumed_title: Option<String>,
        resumed_name: Option<String>,
    },
}

/// The cards the tab's tray chip counts, oldest first, and whether it was unread. The arrival stamps are
/// this process's own clock, so the window it lands in stamps the cards again in the same order.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct MovedAttention {
    cards: Vec<String>,
    questions: Vec<String>,
    unread: bool,
}

/// The terminal handoff's command, as the session id and directory it is built from, so the window it
/// lands in builds it through the one function that decides what that command is.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct MovedResumeCommand {
    cwd: String,
    provider_session_id: String,
}

/// Which of the project's own configuration tiers the tab's session was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum MovedTiers {
    Trusted,
    Untrusted,
    NothingFound,
}

impl From<agent::setting_sources::ProjectTrust> for MovedTiers {
    fn from(trust: agent::setting_sources::ProjectTrust) -> Self {
        match trust {
            agent::setting_sources::ProjectTrust::Trusted => MovedTiers::Trusted,
            agent::setting_sources::ProjectTrust::Untrusted => MovedTiers::Untrusted,
            agent::setting_sources::ProjectTrust::NothingFound => MovedTiers::NothingFound,
        }
    }
}

impl From<MovedTiers> for agent::setting_sources::ProjectTrust {
    fn from(tiers: MovedTiers) -> Self {
        match tiers {
            MovedTiers::Trusted => agent::setting_sources::ProjectTrust::Trusted,
            MovedTiers::Untrusted => agent::setting_sources::ProjectTrust::Untrusted,
            MovedTiers::NothingFound => agent::setting_sources::ProjectTrust::NothingFound,
        }
    }
}

/// A tab frozen for a move: its record, and for a tab with a session what its conversation is rebuilt
/// from. The tab stays in this window, frozen, until [`TabSet::hand_over`] or [`TabSet::unfreeze`].
///
/// For a Codex or ACP tab, the answers the last pump sent are taken by the agent once the provider's own
/// freeze has written out what its link had queued; when that freeze is refused, the caller unfreezes
/// the tab here too.
#[derive(Debug, Clone)]
pub struct FrozenTab {
    pub tab: TabHandoff,
    pub chat: Option<FrozenChat>,
}

/// What a frozen tab's conversation is rebuilt from in the other window.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FrozenChat {
    pub core: agent::ChatMoveCore,
    pub conversation: agent::ConversationHandoff,
    pub backend: BackendHandoff,
}

/// Why a tab did not freeze for a move. The tab is left as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveRefusedWhy {
    NoTab,
    AlreadyMoving,
    /// Its session is still being started, or a prompt of it is about to go out.
    Starting,
    /// It is being handed to a terminal.
    TerminalHandoff,
    /// A message sent into its running turn has not been read by the agent yet.
    Steered,
    /// Its trust answer is being recorded.
    TrustRecording,
    /// It waits to bring back the last window's tabs.
    Restoring,
    /// The legacy backend's session cannot be handed to another process.
    Legacy,
    /// Its running turn does not know its session's id yet: ask again shortly.
    WaitingForSessionId,
    /// Its running turn still did not know its session's id after the wait.
    NoSessionId,
    /// Its conversation was folding a batch of events, or writing after one: ask again shortly. The
    /// freeze never waits for it, since the window's own thread asks.
    Folding,
    /// Its conversation could not be frozen or its records taken.
    Conversation(String),
}

impl MoveRefusedWhy {
    /// Whether the same move may be asked for again in a moment and go through.
    pub fn is_retryable(&self) -> bool {
        matches!(self, MoveRefusedWhy::WaitingForSessionId | MoveRefusedWhy::Folding)
    }
}

impl std::fmt::Display for MoveRefusedWhy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MoveRefusedWhy::NoTab => f.write_str("there is no such tab"),
            MoveRefusedWhy::AlreadyMoving => f.write_str("the tab is already moving"),
            MoveRefusedWhy::Starting => f.write_str("the session or its turn is still starting"),
            MoveRefusedWhy::TerminalHandoff => f.write_str("the tab is being handed to a terminal"),
            MoveRefusedWhy::Steered => f.write_str("a message sent into the turn has not been read yet"),
            MoveRefusedWhy::TrustRecording => f.write_str("the trust answer is still being recorded"),
            MoveRefusedWhy::Restoring => f.write_str("the tab waits to bring back the last window's tabs"),
            MoveRefusedWhy::Legacy => f.write_str("a session on the legacy backend cannot move"),
            MoveRefusedWhy::WaitingForSessionId | MoveRefusedWhy::NoSessionId => {
                f.write_str("the running turn does not know its session yet")
            }
            MoveRefusedWhy::Folding => f.write_str("the conversation is folding its events"),
            MoveRefusedWhy::Conversation(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for MoveRefusedWhy {}

/// What a held prompt carries until it may go out.
pub(super) enum TurnSend {
    /// The tab's queue, read when it goes, so what was queued meanwhile goes with it.
    Queue,
    /// A new session's first message, and the command its result is owed to.
    First { request_id: String, turn: FirstTurn },
    /// A message sent from `page`'s composer into an idle session, answered to that page.
    Prompt {
        request_id: String,
        page: ViewId,
        turn: FirstTurn,
    },
}

/// A prompt held until the presence says it may go out.
pub(super) struct PendingSend {
    gate: SendGate,
    what: TurnSend,
}

/// What happened to a turn of Eitri's that was let go: sent (whatever the backend answered), or held at
/// the head of the tab's queue because the presence could not mark it as running.
pub enum Sent {
    /// The tab's queue went out as one turn.
    Flushed { tab: TabId, flush: Flush },
    /// A new session's first message went out; `request_id` is the command its result is owed to.
    First {
        tab: TabId,
        request_id: String,
        typed: String,
        outcome: Result<(), BackendError>,
    },
    /// A message sent from `page`'s composer went out; its command's result goes to that page.
    Prompt {
        tab: TabId,
        page: ViewId,
        request_id: String,
        typed: String,
        outcome: Result<(), BackendError>,
    },
    /// Not sent: it waits at the head of the tab's queue with `note` beside it, for the user to send it
    /// again. `request_id` is the command its result is owed to, if any: answered to `page` for a
    /// composer's message, to the page that started the session for a first message (`page` `None`).
    NotSent {
        tab: TabId,
        note: String,
        request_id: Option<String>,
        page: Option<ViewId>,
    },
}

/// The wait of one prompt for this window's presence, made by [`TabSet::begin_turn_send`] once its tab
/// is in `starting`. Alone in an unlinked window it lets the prompt go at once; in a linked one, once the
/// presence thread has confirmed the turn's file, or, failing that, once the partner has acknowledged
/// counting this window as another window ([`PresenceFallback`]).
pub struct SendGate {
    tab: TabId,
    /// Whether this gate put its tab in `starting`. A tab already there started a turn of its own that
    /// is not folded yet, and a send that fails after it must leave that turn's start in place.
    entered: bool,
    wait: Option<GateWait>,
}

struct GateWait {
    fallback: PresenceFallback,
    /// The presence thread's answer, until it came.
    answer: Option<mpsc::Receiver<Held>>,
}

impl SendGate {
    pub fn tab(&self) -> TabId {
        self.tab
    }

    /// What the prompt does now.
    pub fn poll(&mut self, now: Instant) -> Decision {
        let Some(wait) = self.wait.as_mut() else {
            return Decision::SendPrompt;
        };
        if let Some(answer) = &wait.answer {
            let held = match answer.try_recv() {
                Ok(held) => Some(held),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(Held::No("the presence thread stopped".to_string())),
            };
            if let Some(held) = held {
                wait.answer = None;
                return wait.fallback.on_confirmed(held, now);
            }
        }
        wait.fallback.tick(now)
    }

    /// The partner acknowledged that it counts this window as another window.
    pub fn on_ack(&mut self, now: Instant) -> Decision {
        match self.wait.as_mut() {
            Some(wait) => wait.fallback.on_ack(now),
            None => Decision::SendPrompt,
        }
    }

    /// This window is closing its link: a prompt still waiting is not sent.
    pub fn on_closing(&mut self) -> Decision {
        match self.wait.as_mut() {
            Some(wait) => wait.fallback.on_closing(),
            None => Decision::SendPrompt,
        }
    }

    /// The link ended: the partner counts this window's own file again, or is gone.
    pub fn on_link_end(&mut self) -> Decision {
        match self.wait.as_mut() {
            Some(wait) => wait.fallback.on_link_end(),
            None => Decision::SendPrompt,
        }
    }
}

/// Builds a tab's record. Every field of the tab and of its session state is named: one added later does
/// not compile here until it is placed in the record or named as staying behind, with why.
fn handoff_of(tab: &Tab, review: Option<ReviewHandoff>) -> TabHandoff {
    let Tab {
        // The panel record carries following across windows.
        following: _,
        // Allocated by the window the tab lands in.
        id: _,
        number: _,
        name,
        mode,
        agent,
        // The window the tab lands in says which of its views shows it.
        view: _,
        // Read for what the tab held; a session is adopted from its own records.
        backend,
        session,
        attention,
        failed_after_start,
        failed_resume,
        // Refused by the freeze while set: a terminal handoff in flight does not move.
        pending_handoff: _,
        last_handoff,
        editing_draft,
        review_draft,
        notes,
        // Handles into this window's review; the other window's review makes its own.
        review_watchers: _,
        #[cfg(any(test, feature = "test-support"))]
        gate_log,
    } = tab;
    let SessionState {
        title,
        title_from_record,
        // Names a view of this window, whose page cannot post to the other one.
        left_view: _,
        project_tiers,
        stale,
        // Its marks are instants on this process's clock, which mean nothing in another process.
        turn_trace: _,
        reported_start_failure,
        queue,
        opening_prompt,
        queue_error,
        queue_note,
        draft,
        question_draft,
        editing_question,
        rule_offers,
        was_running,
        // Not carried: the projection's note of unseen work is live, noted again by the adopting window.
        was_unseen_busy: _,
        note_ledger,
        creates_file_unsent,
        rule_offers_seen,
        turn_clock,
        last_began_turn,
        turn_times_unsent,
        host_answered,
        human_allowed,
        user_answered,
        // A line for a page of this window. The landing window reads the same server report from the
        // moved projection and says it once on its own page. A provider's line was the source's to say: the
        // adopted provider never opens a session, so it has none.
        foreign_server_note: _,
        // What this window's broker was told. An external session's tool list crosses in the panel's record, with
        // the socket it belongs to.
        client_calls: _,
        editor_declared: _,
        // A card ends with its call when the tab leaves: the other window's broker never raised it.
        editor_cards: _,
    } = session;
    let state = match backend {
        TabBackend::NotStarted => MovedState::Empty,
        TabBackend::AwaitingTrust(waiting) => MovedState::AwaitingTrust(match &waiting.start {
            DeferredStart::FirstSend { first_turn, .. } => MovedStart::FirstSend(first_turn.clone()),
            DeferredStart::Resume {
                provider_session_id,
                resumed_title,
                resumed_name,
                ..
            } => MovedStart::Resume {
                provider_session_id: provider_session_id.clone(),
                resumed_title: resumed_title.clone(),
                resumed_name: resumed_name.clone(),
            },
            DeferredStart::Restore { .. } => unreachable!("a restore's start is refused by the freeze"),
        }),
        TabBackend::Failed { reason } => MovedState::Failed { reason: reason.clone() },
        TabBackend::Live(_) => MovedState::Live,
        TabBackend::Starting(_) => unreachable!("a start in flight is refused by the freeze"),
    };
    TabHandoff {
        name: name.clone(),
        mode: *mode,
        agent: agent.clone(),
        state,
        attention: MovedAttention {
            cards: attention.card_ids(),
            questions: attention.question_ids(),
            unread: attention.attention().unread,
        },
        failed_after_start: *failed_after_start,
        failed_resume: failed_resume.clone(),
        last_handoff: last_handoff.as_ref().map(|command| MovedResumeCommand {
            cwd: command.cwd().to_string(),
            provider_session_id: command.provider_session_id().to_string(),
        }),
        editing_draft: *editing_draft,
        review_draft: review_draft.clone(),
        notes: notes.clone(),
        title: title.clone(),
        title_from_record: *title_from_record,
        project_tiers: project_tiers.map(MovedTiers::from),
        stale: *stale,
        reported_start_failure: *reported_start_failure,
        queue: queue.clone(),
        queue_typed_by_person: queue.iter().map(|item| item.typed_by_person).collect(),
        opening_prompt: opening_prompt.clone(),
        queue_error: queue_error.clone(),
        queue_note: queue_note.clone(),
        draft: draft.clone(),
        question_draft: question_draft.clone(),
        editing_question: editing_question.clone(),
        rule_offers: rule_offers
            .iter()
            .map(|(id, rule)| (id.clone(), rule.to_rule_string()))
            .collect(),
        was_running: *was_running,
        note_ledger: note_ledger.clone(),
        creates_file_unsent: creates_file_unsent.clone(),
        rule_offers_seen: rule_offers_seen.clone(),
        turn_clock: turn_clock.clone(),
        last_began_turn: last_began_turn.clone(),
        turn_times_unsent: *turn_times_unsent,
        host_answered: host_answered.clone(),
        human_allowed: human_allowed.clone(),
        user_answered: user_answered.clone(),
        review,
        #[cfg(any(test, feature = "test-support"))]
        gate_log: gate_log.clone(),
    }
}

/// The tray chip's count rebuilt in this window: the same cards in the same order, stamped on this
/// process's clock, and unread if it was.
fn attention_from(moved: MovedAttention) -> AttentionTracker {
    let MovedAttention {
        cards,
        questions,
        unread,
    } = moved;
    let mut attention = AttentionTracker::default();
    if unread {
        attention.session_ended(false);
    }
    let mut held: Vec<String> = Vec::new();
    for card in cards {
        held.push(card);
        attention.resync(held.iter().cloned());
    }
    attention.sync_questions(questions);
    attention
}

/// Whether `event`, folded after a tab entered `starting`, ends its start: the turn started (from then on
/// it runs), it ended, or the session's state changed under it.
fn ends_a_start(event: &AgentDomainEvent) -> bool {
    matches!(
        event,
        AgentDomainEvent::TurnStarted { .. }
            | AgentDomainEvent::TurnCompleted { .. }
            | AgentDomainEvent::SessionOpened { .. }
            | AgentDomainEvent::SessionUnavailable { .. }
            | AgentDomainEvent::ResumeOutcome { .. }
            | AgentDomainEvent::SessionClosed { .. }
    )
}

/// What a move's last pump left for the page that still shows the tab, and which tab was the base
/// view's active one then.
pub(super) struct HeldPump {
    active: TabId,
    out: PumpOutput,
}

impl PumpOutput {
    pub(super) fn empty() -> PumpOutput {
        PumpOutput {
            active_payload: None,
            active_class: crate::panel_cadence::EnvelopeClass::Immediate,
            first_text: false,
            turn_ended: Vec::new(),
            offers_changed: Vec::new(),
            queue_changed: Vec::new(),
            tripped: Vec::new(),
            review_hints: Vec::new(),
            view_payloads: Vec::new(),
        }
    }

    /// `later` after this, as one output: its base payload follows this one's, to the same page.
    fn absorb(&mut self, later: PumpOutput, active: TabId) {
        let PumpOutput {
            active_payload,
            active_class,
            first_text,
            turn_ended,
            offers_changed,
            queue_changed,
            tripped,
            review_hints,
            view_payloads,
        } = later;
        if let Some(payload) = active_payload {
            if self.active_payload.is_some() {
                self.view_payloads.push(ShownPayload {
                    view: ViewId::BASE,
                    tab: active,
                    payload,
                    class: active_class,
                    first_text,
                });
            } else {
                self.active_payload = Some(payload);
                self.active_class = active_class;
                self.first_text |= first_text;
            }
        }
        self.turn_ended.extend(turn_ended);
        self.offers_changed.extend(offers_changed);
        self.queue_changed.extend(queue_changed);
        self.tripped.extend(tripped);
        self.review_hints.extend(review_hints);
        self.view_payloads.extend(view_payloads);
    }
}

impl TabSet {
    // ---- turn starts -------------------------------------------------------------------------

    /// Says this window is linked to `partner` (`None`: not linked, or no longer). Every guard of this
    /// window skips the partner's window file from the next check on, and from now on a prompt waits for
    /// its presence file. A link that ends lets every prompt that was waiting go: the partner counts this
    /// window again, or is gone. Returns what those prompts did.
    pub fn set_partner(&mut self, partner: Option<Partner>) -> Vec<Sent> {
        if let Some(guard) = self.presence_guard() {
            guard.set_partner(partner.clone());
        }
        self.partner_pids.clear();
        self.linked = partner.is_some();
        self.untracked_starts.clear();
        if self.linked {
            self.untracked_turns = self.untracked_running(crate::turn_review::monotonic_ns());
            return Vec::new();
        }
        self.untracked_turns.clear();
        self.decide_every(|gate, _| gate.on_link_end())
    }

    /// Names one more linked window, for a window with several links. A prompt waits for its presence file from
    /// now on, as under [`set_partner`](Self::set_partner). The same pid again replaces the partner: a relink.
    pub fn add_partner(&mut self, partner: Partner) {
        if self.partner_pids.is_empty() {
            self.untracked_turns = self.untracked_running(crate::turn_review::monotonic_ns());
            self.untracked_starts.clear();
        }
        self.partner_pids.insert(partner.pid);
        self.linked = true;
        if let Some(guard) = self.presence_guard() {
            guard.add_partner(partner);
        }
    }

    /// One link of a window with several ended, or, with `closing`, this window is about to close it. A prompt
    /// waiting for its presence goes on waiting while another partner may skip this window's file; once the last
    /// one is gone it goes as [`set_partner(None)`](Self::set_partner) lets it, or, with `closing`, stays as
    /// [`close_link`](Self::close_link) keeps it. Returns what those prompts did.
    pub fn remove_partner(&mut self, pid: u32, closing: bool) -> Vec<Sent> {
        if let Some(guard) = self.presence_guard() {
            guard.remove_partner(pid);
        }
        self.partner_pids.remove(&pid);
        if !self.partner_pids.is_empty() {
            return Vec::new();
        }
        self.linked = false;
        self.untracked_turns.clear();
        self.decide_every(|gate, _| if closing { gate.on_closing() } else { gate.on_link_end() })
    }

    /// The turns running in this window's tabs that have a session, each with its session and its start on the
    /// `CLOCK_MONOTONIC` clock (from the turn's wall-clock start, as long ago as that was), for a window with no
    /// turn review; none for a window that has one, whose review keeps its own.
    fn untracked_running(&self, now_ns: u64) -> BTreeMap<u64, (String, u64)> {
        if self.turn_review.is_some() {
            return BTreeMap::new();
        }
        let now_ms = super::wall_clock_ms();
        self.tabs
            .iter()
            .filter(|tab| tab.turn_running())
            .filter_map(|tab| {
                let session = tab.provider_session_id()?;
                let ago_ms = tab.turn_started_at_ms().map_or(0, |at| now_ms.saturating_sub(at));
                Some((
                    tab.id.0,
                    (session, now_ns.saturating_sub(ago_ms.saturating_mul(1_000_000))),
                ))
            })
            .collect()
    }

    /// The turns running here now, as `started` edges, for a linked window with no turn review: what its `hello`
    /// or `welcome` names. A window with turn review names them from its review instead.
    pub fn untracked_running_turn_edges(&self) -> Vec<crate::window_link::TurnEdge> {
        self.untracked_running(crate::turn_review::monotonic_ns())
            .into_values()
            .map(|(session, at_ns)| crate::window_link::TurnEdge {
                session,
                edge: crate::window_link::EdgeKind::Started,
                at_ns,
            })
            .collect()
    }

    /// For a linked window with no turn review: the turn starts and ends since the last call (an end at `now_ns`, a
    /// start at the turn's own start), so the partner's overlap notes still count this window's turns. A window with
    /// turn review announces them from its review, with the exact times; an unlinked one announces nothing.
    ///
    /// The running turns are read as they are now, and the ingestion thread folds between two calls, so a turn can
    /// start and end without any call seeing it run. Its start in a delivered batch is then all that is left of it,
    /// and it is announced as started and ended at `now_ns`: a window with turn review stamps a turn on delivery too.
    pub fn take_untracked_turn_edges(&mut self, now_ns: u64) -> Vec<crate::window_link::TurnEdge> {
        use crate::window_link::{EdgeKind, TurnEdge};
        let delivered = std::mem::take(&mut self.untracked_starts);
        if !self.linked || self.turn_review.is_some() {
            self.untracked_turns.clear();
            return Vec::new();
        }
        let running = self.untracked_running(now_ns);
        let mut edges = Vec::new();
        let told = std::mem::take(&mut self.untracked_turns);
        for (tab, (session, _)) in &told {
            if running.get(tab).is_none_or(|(now, _)| now != session) {
                edges.push(TurnEdge {
                    session: session.clone(),
                    edge: EdgeKind::Ended,
                    at_ns: now_ns,
                });
            }
        }
        for (tab, (session, started)) in &running {
            if told.get(tab).is_none_or(|(was, _)| was != session) {
                edges.push(TurnEdge {
                    session: session.clone(),
                    edge: EdgeKind::Started,
                    at_ns: *started,
                });
            }
        }
        for tab in delivered {
            if told.contains_key(&tab) || running.contains_key(&tab) {
                continue;
            }
            if let Some(session) = self.get(TabId(tab)).and_then(Tab::provider_session_id) {
                for edge in [EdgeKind::Started, EdgeKind::Ended] {
                    edges.push(TurnEdge {
                        session: session.clone(),
                        edge,
                        at_ns: now_ns,
                    });
                }
            }
        }
        self.untracked_turns = running
            .into_iter()
            .map(|(tab, (session, started))| {
                let started = told
                    .get(&tab)
                    .filter(|(was, _)| *was == session)
                    .map_or(started, |(_, at)| *at);
                (tab, (session, started))
            })
            .collect();
        edges
    }

    /// This window is closing its link: as [`set_partner(None)`](Self::set_partner), except that a prompt still
    /// waiting for its presence is not sent but held at the head of its tab's queue with a note. The partner learns
    /// of the close only once it reads it, and until then it may skip this window's file, which was not confirmed
    /// to mark that prompt's turn. Returns what those prompts did.
    pub fn close_link(&mut self) -> Vec<Sent> {
        if let Some(guard) = self.presence_guard() {
            guard.set_partner(None);
        }
        self.partner_pids.clear();
        self.linked = false;
        self.untracked_turns.clear();
        self.decide_every(|gate, _| gate.on_closing())
    }

    /// Whether this window is linked to another one.
    pub fn is_linked(&self) -> bool {
        self.linked
    }

    /// The partner acknowledged `presence{held: false}`: the prompts waiting for it go. Returns what they
    /// did.
    pub fn on_presence_ack(&mut self, now: Instant) -> Vec<Sent> {
        self.decide_every(move |gate, _| gate.on_ack(now))
    }

    /// Whether the partner is owed `presence{held: false}` (taken once): a prompt's file could not be
    /// confirmed, and the prompt waits for the partner's acknowledgement.
    pub fn take_presence_false_owed(&mut self) -> bool {
        std::mem::take(&mut self.presence_false_owed)
    }

    /// Asks the presence thread to confirm every file of the running set as it is now, for a caller that
    /// must know they are held before it goes on (a moved tab's turn, before the move is reported done).
    /// `None` when this window holds no presence. Never waits: the caller polls the answer.
    pub fn presence_confirmation(&self) -> Option<mpsc::Receiver<Held>> {
        self.presence
            .as_ref()
            .map(|holder| holder.sync_confirmed(&self.running_set()))
    }

    /// A turn of Eitri's is about to start in `tab`: the tab enters `starting`, so the presence counts it
    /// as running (this window's own reverts refuse as for a running turn), and the returned gate says
    /// when the prompt may go out. Every turn Eitri starts passes through here first.
    pub fn begin_turn_send(&mut self, tab: TabId) -> SendGate {
        let since = self
            .get(tab)
            .and_then(Tab::live)
            .map_or(0, |backend| backend.projection().last_revision);
        let entered = self.starting.insert(tab.0);
        // A start already under way keeps the revision it began at: only events after that one say
        // whether it is over.
        self.starting_since.entry(tab.0).or_insert(since);
        if !self.linked {
            self.sync_presence();
            return SendGate {
                tab,
                entered,
                wait: None,
            };
        }
        let answer = match &self.presence {
            Some(holder) => holder.sync_confirmed(&self.running_set()),
            None => {
                let (told, answer) = mpsc::channel();
                let _ = told.send(Held::No("this window holds no presence".to_string()));
                answer
            }
        };
        SendGate {
            tab,
            entered,
            wait: Some(GateWait {
                fallback: PresenceFallback::start(Instant::now()),
                answer: Some(answer),
            }),
        }
    }

    /// The first message of a session that just started in `tab`, owed to the command `request_id`:
    /// sent now when the presence allows it, else held until [`TabSet::poll_turn_sends`] lets it go.
    pub fn send_first_turn(&mut self, tab: TabId, request_id: &str, turn: FirstTurn) -> Option<Sent> {
        if let Some(message) = self.why_no_turn_send(tab) {
            return Some(Sent::First {
                tab,
                request_id: request_id.to_string(),
                typed: turn.typed,
                outcome: Err(BackendError { message, benign: true }),
            });
        }
        let gate = self.begin_turn_send(tab);
        self.send_when_allowed(
            gate,
            TurnSend::First {
                request_id: request_id.to_string(),
                turn,
            },
        )
    }

    /// A message typed in `page`'s composer for `tab`'s idle session (`turn`), whose command `request_id`
    /// the page waits on: sent now when the presence allows it, else held until
    /// [`TabSet::poll_turn_sends`] lets it go.
    ///
    /// Refused at once, with nothing sent and the tab's start left as it was, while the tab is frozen for
    /// a move or another of its messages is held for the presence.
    pub fn send_prompt(&mut self, tab: TabId, request_id: &str, page: ViewId, turn: FirstTurn) -> Option<Sent> {
        if let Some(message) = self.why_no_turn_send(tab) {
            return Some(Sent::Prompt {
                tab,
                page,
                request_id: request_id.to_string(),
                typed: turn.typed,
                outcome: Err(BackendError { message, benign: true }),
            });
        }
        // A card or question waits for an answer: keep the message in the queue until it settles,
        // because the session is stopped on the user.
        if self.card_waits(tab) {
            let outcome = self.hold_behind_card(tab, &turn);
            return Some(Sent::Prompt {
                tab,
                page,
                request_id: request_id.to_string(),
                typed: turn.typed,
                outcome,
            });
        }
        let gate = self.begin_turn_send(tab);
        self.send_when_allowed(
            gate,
            TurnSend::Prompt {
                request_id: request_id.to_string(),
                page,
                turn,
            },
        )
    }

    /// Keeps a composer's message at the end of `tab`'s queue, marked to go as a turn once no card waits
    /// (`TabSet::release_after_card`).
    pub(super) fn hold_behind_card(&mut self, tab: TabId, turn: &FirstTurn) -> Result<(), BackendError> {
        let outcome = self
            .queue_message(
                tab,
                &turn.typed,
                turn.wire.clone(),
                wall_clock_ms(),
                true,
                turn.origin(),
            )
            .map(|_| ())
            .map_err(|message| BackendError { message, benign: true });
        if outcome.is_ok() {
            if let Some(item) = self.get_mut(tab).and_then(|t| t.session.queue.last_mut()) {
                item.behind_card = true;
            }
        }
        outcome
    }

    /// Why no message may start a turn in `tab` now: it is frozen for a move (its record is taken, and a
    /// turn begun here would be missing from it), or a message of it already waits for the presence (a
    /// second one would take its place, and the first would be neither sent nor answered).
    fn why_no_turn_send(&self, tab: TabId) -> Option<String> {
        if self.moving.contains(&tab.0) {
            return Some(MOVING.to_string());
        }
        if self.pending_sends.contains_key(&tab.0) {
            return Some(SEND_HELD.to_string());
        }
        None
    }

    /// Lets go every held prompt whose presence now allows it, and says which could not be sent. Run by
    /// the window's tick.
    pub fn poll_turn_sends(&mut self, now: Instant) -> Vec<Sent> {
        self.poll_at(now)
    }

    /// Whether a prompt of `tab` is held for the presence.
    pub fn send_is_held(&self, tab: TabId) -> bool {
        self.pending_sends.contains_key(&tab.0)
    }

    /// The command a held prompt of `tab` owes its answer to: its request id, and for a composer's message
    /// the page it was typed in (`None` for a session's first message, whose start knows its page). `None`
    /// when nothing is held or what is held is the tab's queue, which no command waits on.
    pub fn held_send_request(&self, tab: TabId) -> Option<(&str, Option<ViewId>)> {
        match &self.pending_sends.get(&tab.0)?.what {
            TurnSend::Queue => None,
            TurnSend::First { request_id, .. } => Some((request_id, None)),
            TurnSend::Prompt { request_id, page, .. } => Some((request_id, Some(*page))),
        }
    }

    fn poll_at(&mut self, now: Instant) -> Vec<Sent> {
        self.decide_every(move |gate, _| gate.poll(now))
    }

    /// Asks every held prompt's gate `ask` and does what it says.
    fn decide_every(&mut self, mut ask: impl FnMut(&mut SendGate, Instant) -> Decision) -> Vec<Sent> {
        let now = Instant::now();
        let mut sent = Vec::new();
        for id in self.pending_sends.keys().copied().collect::<Vec<_>>() {
            let Some(mut pending) = self.pending_sends.remove(&id) else {
                continue;
            };
            let decision = ask(&mut pending.gate, now);
            sent.extend(self.decide(pending, decision));
        }
        sent
    }

    /// The prompt goes out if the gate allows it now, else it is held.
    pub(super) fn send_when_allowed(&mut self, mut gate: SendGate, what: TurnSend) -> Option<Sent> {
        let decision = gate.poll(Instant::now());
        self.decide(PendingSend { gate, what }, decision)
    }

    fn decide(&mut self, pending: PendingSend, decision: Decision) -> Option<Sent> {
        let PendingSend { gate, what } = pending;
        let tab = gate.tab();
        match decision {
            Decision::SendPrompt => self.release_turn_send(gate, what),
            Decision::Wait => {
                self.pending_sends.insert(tab.0, PendingSend { gate, what });
                None
            }
            Decision::SendPresenceFalse => {
                self.presence_false_owed = true;
                self.pending_sends.insert(tab.0, PendingSend { gate, what });
                None
            }
            Decision::NotSent(note) => {
                self.end_start_of(&gate);
                Some(self.not_sent(tab, what, note))
            }
        }
    }

    /// A prompt the presence could not mark as running: it waits at the head of the tab's queue with the
    /// note, and goes out again only when the user sends it.
    fn not_sent(&mut self, tab: TabId, what: TurnSend, note: String) -> Sent {
        let (request_id, page, turn) = match what {
            TurnSend::Queue => (None, None, None),
            TurnSend::First { request_id, turn } => (Some(request_id), None, Some(turn)),
            TurnSend::Prompt { request_id, page, turn } => (Some(request_id), Some(page), Some(turn)),
        };
        if let Some(t) = self.get_mut(tab) {
            if let Some(turn) = turn {
                let mut item = Queued::held(&turn.typed, turn.wire, wall_clock_ms());
                item.typed_by_person = turn.typed_by_person;
                t.session.queue.insert(0, item);
            }
            t.session.queue_error = Some(note.clone());
        }
        Sent::NotSent {
            tab,
            note,
            request_id,
            page,
        }
    }

    /// `tab`'s start is over without a turn of its own having started (its send failed, it was
    /// refused, or the tab can no longer send).
    pub(super) fn end_start(&mut self, tab: TabId) {
        let was = self.starting.remove(&tab.0);
        self.starting_since.remove(&tab.0);
        if was {
            self.sync_presence();
        }
    }

    /// [`TabSet::end_start`] for a send that did not go out or failed, when its own gate put the tab in
    /// `starting`: a start an earlier send began stays until its own turn says otherwise.
    pub(super) fn end_start_of(&mut self, gate: &SendGate) {
        if gate.entered {
            self.end_start(gate.tab());
        }
    }

    /// Leaves `starting` for `tab` once its delivery says the start is over (see [`ends_a_start`]) or its
    /// turn runs. A tab whose prompt is still held stays: its prompt has not gone out.
    pub(super) fn end_start_if_over(
        tab: TabId,
        backend: &AgentBackend,
        delivery: &RevisedDelivery,
        starting: &mut BTreeSet<u64>,
        starting_since: &mut BTreeMap<u64, u64>,
        held: &BTreeMap<u64, PendingSend>,
    ) {
        let Some(&since) = starting_since.get(&tab.0) else {
            return;
        };
        if held.contains_key(&tab.0) {
            return;
        }
        let over = match delivery {
            RevisedDelivery::Events(tagged) => tagged
                .iter()
                .any(|(revision, event)| *revision > since && ends_a_start(event)),
            // The events were dropped: what they would have said is read off the projection.
            RevisedDelivery::Resync => {
                let projection = backend.projection();
                projection.turn_endings.last().is_some_and(|ending| ending.seq >= since)
                    || projection.last_turn_end_seq.is_some_and(|seq| seq >= since)
                    || matches!(
                        projection.status,
                        agent::ProjectionStatus::Closed { .. } | agent::ProjectionStatus::Unavailable { .. }
                    )
            }
            RevisedDelivery::Nothing => false,
        };
        let started = backend.projection().active_turn_id.is_some();
        if over || started {
            starting.remove(&tab.0);
            starting_since.remove(&tab.0);
        }
    }

    /// Everything the set keeps about a move or a turn start of `tab`, forgotten: the tab has left.
    pub(super) fn forget_move_state(&mut self, tab: TabId) {
        self.moving.remove(&tab.0);
        self.starting.remove(&tab.0);
        self.starting_since.remove(&tab.0);
        self.pending_sends.remove(&tab.0);
        self.unbound_since.remove(&tab.0);
        self.flush_owed.remove(&tab.0);
    }

    // ---- a move ------------------------------------------------------------------------------

    /// Whether `tab` is frozen for a move.
    pub fn is_moving(&self, tab: TabId) -> bool {
        self.moving.contains(&tab.0)
    }

    /// Why `tab` would not freeze for a move now, read without changing anything: what
    /// [`TabSet::freeze_for_move`] refuses before it touches the tab. `None` does not promise the freeze:
    /// its conversation and its review can still refuse it.
    pub fn move_blocker(&self, tab: TabId) -> Option<MoveRefusedWhy> {
        let Some(at) = self.position(tab) else {
            return Some(MoveRefusedWhy::NoTab);
        };
        if self.moving.contains(&tab.0) {
            return Some(MoveRefusedWhy::AlreadyMoving);
        }
        let t = &self.tabs[at];
        // Ingestion can start the turn after the last pump checked `starting`. The final frozen
        // pump reconciles that flag; a prompt still held for presence must stay here regardless.
        if self.pending_sends.contains_key(&tab.0) || (self.starting.contains(&tab.0) && !t.turn_running()) {
            return Some(MoveRefusedWhy::Starting);
        }
        if t.pending_handoff.is_some() {
            return Some(MoveRefusedWhy::TerminalHandoff);
        }
        if t.session.queue.iter().any(|q| !q.is_held()) {
            return Some(MoveRefusedWhy::Steered);
        }
        match &t.backend {
            TabBackend::Starting(_) => Some(MoveRefusedWhy::Starting),
            TabBackend::AwaitingTrust(AwaitingTrust { recording: true, .. }) => Some(MoveRefusedWhy::TrustRecording),
            TabBackend::AwaitingTrust(AwaitingTrust {
                start: DeferredStart::Restore { .. },
                ..
            }) => Some(MoveRefusedWhy::Restoring),
            TabBackend::Live(AgentBackend::Legacy(_)) => Some(MoveRefusedWhy::Legacy),
            TabBackend::NotStarted
            | TabBackend::AwaitingTrust(_)
            | TabBackend::Failed { .. }
            | TabBackend::Live(AgentBackend::Conversation { .. }) => None,
        }
    }

    /// Freezes `tab` for a move to the other window and takes its record. From here on it takes no input
    /// (refused with [`MOVING`]) and answers no card, and nothing of it is delivered here. Its
    /// conversation stops folding, and the pump runs for it once more first, so what was folded and not
    /// yet delivered is answered and noted before the record is taken; what that pump has for the page
    /// still showing the tab comes with the next pump's output.
    ///
    /// Refused while its session or a prompt of it is starting, it is handed to a terminal, a message
    /// sent into its turn is unread, its trust answer is being recorded, it waits for a restore, it runs
    /// on the legacy backend, or its conversation is closing or cannot be frozen. A running turn that
    /// does not know its session's id yet is refused as [`MoveRefusedWhy::WaitingForSessionId`] for up to
    /// two seconds, then for good. A conversation folding a batch, or writing after one, is refused as
    /// [`MoveRefusedWhy::Folding`] with nothing changed: this never waits for its ingest thread.
    pub fn freeze_for_move(&mut self, tab: TabId) -> Result<FrozenTab, MoveRefusedWhy> {
        if let Some(why) = self.move_blocker(tab) {
            return Err(why);
        }
        let at = self.position(tab).ok_or(MoveRefusedWhy::NoTab)?;
        match &self.tabs[at].backend {
            TabBackend::NotStarted | TabBackend::AwaitingTrust(_) | TabBackend::Failed { .. } => {
                self.moving.insert(tab.0);
                return Ok(FrozenTab {
                    tab: handoff_of(&self.tabs[at], None),
                    chat: None,
                });
            }
            // Refused above: only a live conversation is left.
            TabBackend::Starting(_) | TabBackend::Live(_) => {}
        }
        let project_root = self
            .project_root
            .clone()
            .ok_or_else(|| MoveRefusedWhy::Conversation("this window has not pumped its tabs yet".to_string()))?;
        self.tabs[at]
            .live_mut()
            .expect("matched as live above")
            .freeze_folding()
            .map_err(|refused| match refused {
                agent::HandoffRefused::Folding => MoveRefusedWhy::Folding,
                refused => MoveRefusedWhy::Conversation(refused.to_string()),
            })?;
        self.last_pump(tab, &project_root);
        // The last pump can end the session (its CLI reported a mode the gate cannot hold): there is then
        // nothing to move, and the session is retired with that pump's output.
        let Some(at) = self.position(tab).filter(|at| self.tabs[*at].live().is_some()) else {
            return Err(MoveRefusedWhy::Conversation("the session ended".to_string()));
        };
        let session = self.tabs[at].provider_session_id();
        let (core, conversation, backend) = self.tabs[at]
            .live_mut()
            .expect("checked live above")
            .take_frozen()
            .map_err(|refused| MoveRefusedWhy::Conversation(refused.to_string()))?;
        let review = match self.turn_review.as_mut() {
            None => None,
            Some(review) => match review.hand_over_session(session.as_deref().unwrap_or_default(), tab.0) {
                Ok(handoff) => Some(handoff),
                Err(crate::turn_review::Unbound) => {
                    if let Some(backend) = self.tabs[at].live_mut() {
                        backend.unfreeze_folding();
                    }
                    let now = Instant::now();
                    let since = self.unbound_wait(tab, now);
                    return Err(if now.saturating_duration_since(since) >= SESSION_ID_WAIT {
                        self.unbound_since.remove(&tab.0);
                        MoveRefusedWhy::NoSessionId
                    } else {
                        MoveRefusedWhy::WaitingForSessionId
                    });
                }
            },
        };
        self.unbound_since.remove(&tab.0);
        self.moving.insert(tab.0);
        Ok(FrozenTab {
            tab: handoff_of(&self.tabs[at], review),
            chat: Some(FrozenChat {
                core,
                conversation,
                backend,
            }),
        })
    }

    /// When the current attempt to move `tab` began waiting for its session id. A caller that waits
    /// asks again well within [`SESSION_ID_WAIT`]; a refusal last seen longer ago than that belongs to
    /// an attempt that was given up, and this one waits afresh.
    pub(super) fn unbound_wait(&mut self, tab: TabId, now: Instant) -> Instant {
        let (first, last) = self.unbound_since.entry(tab.0).or_insert((now, now));
        if now.saturating_duration_since(*last) > SESSION_ID_WAIT {
            *first = now;
        }
        *last = now;
        *first
    }

    /// The ordinary pump, for one tab: what its frozen conversation folded and has not delivered is
    /// answered and noted now. Its output is held for the next pump to hand on.
    fn last_pump(&mut self, tab: TabId, project_root: &Path) {
        let Some(at) = self.position(tab) else { return };
        let active = self.active;
        let mut out = PumpOutput::empty();
        let review = self.turn_review.as_mut();
        let mut tick = PumpTick {
            project_root,
            rules: &self.rules,
            ui_wake: self.ui_wake.as_ref(),
            retiring: &mut self.retiring,
            starting: &mut self.starting,
            starting_since: &mut self.starting_since,
            held_sends: &self.pending_sends,
            untracked_starts: &mut self.untracked_starts,
            #[cfg(test)]
            after_drain: &mut self.after_drain,
            #[cfg(test)]
            redeliver_covered: self.redeliver_covered,
            #[cfg(test)]
            before_resync_snapshot: &mut self.before_resync_snapshot,
        };
        let t = &mut self.tabs[at];
        let is_active = t.id == active;
        let in_a_page = is_active || !t.view.is_base();
        let place = PumpPlace {
            is_active,
            in_a_page,
            on_screen: in_a_page,
        };
        Self::pump_tab(t, &mut tick, review, place, &mut out);
        self.sync_presence();
        match self.held_pump.as_mut() {
            Some(held) => held.out.absorb(out, active),
            None => self.held_pump = Some(HeldPump { active, out }),
        }
    }

    /// A move's held output, less what no longer has a place: a payload for a page that shows another
    /// tab now, and anything about a tab that has left. A tab whose payload is dropped is stale, so it
    /// gets a snapshot when it is shown again.
    pub(super) fn still_wanted(&mut self, held: HeldPump) -> PumpOutput {
        let HeldPump { active, mut out } = held;
        if out.active_payload.is_some() && active != self.active {
            out.active_payload = None;
            out.first_text = false;
            if let Some(t) = self.get_mut(active) {
                t.session.stale = true;
            }
        }
        let tabs = &self.tabs;
        let current = self.active;
        let shown_there = |view: ViewId, tab: TabId| {
            tabs.iter().any(|t| t.id == tab && t.view == view) && (!view.is_base() || tab == current)
        };
        let mut dropped = Vec::new();
        out.view_payloads.retain(|shown| {
            let keep = shown_there(shown.view, shown.tab);
            if !keep {
                dropped.push(shown.tab);
            }
            keep
        });
        let exists = |tab: &TabId| tabs.iter().any(|t| t.id == *tab);
        out.turn_ended.retain(exists);
        out.offers_changed.retain(exists);
        out.queue_changed.retain(exists);
        for tab in dropped {
            if let Some(t) = self.get_mut(tab) {
                t.session.stale = true;
            }
        }
        out
    }

    /// The move of `tab` did not happen: it takes input again, its conversation folds again from where
    /// its provider kept its events, and its review stays here. A turn that ended while it was frozen
    /// lets its queue go at the next pump.
    pub fn unfreeze(&mut self, tab: TabId) {
        if !self.moving.remove(&tab.0) {
            return;
        }
        let session = self.get(tab).and_then(Tab::provider_session_id);
        if let Some(backend) = self.get_mut(tab).and_then(Tab::live_mut) {
            backend.unfreeze_folding();
        }
        if let (Some(review), Some(session)) = (self.turn_review.as_mut(), session) {
            review.move_aborted(&session);
        }
        self.owe_flush_if_idle(tab);
    }

    /// A chat holding `session` is arriving with move `mv`: presence counts a turn under a tab id held
    /// for this move before the other window lets go. The hold stays until this move's drop after
    /// adoption, even when another arrival has the same session id. The answer says once the presence
    /// thread holds it; `None` when this window holds no presence. Holding the same move twice holds it once.
    pub fn hold_arrival(&mut self, session: &str, mv: u64) -> Option<std::sync::mpsc::Receiver<Held>> {
        if !self.arrivals.contains_key(&mv) {
            let id = TabId(self.next_id);
            self.next_id += 1;
            self.arrivals.insert(mv, (session.to_owned(), id));
        }
        let running = self.running_set();
        self.presence.as_ref().map(|holder| holder.sync_confirmed(&running))
    }

    /// Move `mv` was adopted or given up: only its arrival hold is let go.
    pub fn drop_arrival(&mut self, mv: u64) {
        if self.arrivals.remove(&mv).is_some() {
            self.sync_presence();
        }
    }

    /// The move of `tab` is done (the other window took it up): it leaves this set without being ended.
    /// Its review watchers are cancelled first; its review goes without an end snapshot and without a
    /// job, its running turn going on as the partner's; nothing counts it as retiring, and the presence
    /// lets its turn's file go. The base view keeps an active tab as when a tab closes. Also how a window
    /// takes out again a tab it adopted for a move that did not commit.
    ///
    /// The tab comes back with its backend, which the caller forgets without ending it: the session now
    /// belongs to the other window.
    #[must_use = "the handed-over tab's backend is the other window's now: forget it without ending it"]
    pub fn hand_over(&mut self, tab: TabId) -> Option<Tab> {
        let at = self.position(tab)?;
        let base_index = self.base_tabs().iter().position(|t| *t == tab);
        let before = self.active;
        let mut left = self.tabs.remove(at);
        left.cancel_review_watchers();
        if let Some(review) = self.turn_review.as_mut() {
            review.drop_moved(tab.0);
        }
        self.forget_move_state(tab);
        self.sync_presence();
        self.tab_left(&left, base_index, before);
        Some(left)
    }

    /// Takes up a tab another window froze for a move, with the backend adopted from its conversation's
    /// records, shown in `view` (an empty extra view, or the base view, where it becomes the active tab).
    /// Every field the record carries is kept as it came: this is neither a `Resync` nor a backend
    /// install, so the approvals, answered sets and note candidates survive, and its first delivery is
    /// the snapshot the page is given (noted as any snapshot is). Its review continues here, and the
    /// presence counts its running turn at once.
    pub fn adopt_moved(&mut self, handoff: TabHandoff, backend: AgentBackend, view: ViewId) -> TabId {
        if let Some(wake) = &self.ui_wake {
            backend.ensure_ui_wake(wake);
        }
        self.adopt(handoff, TabBackend::Live(backend), view)
    }

    /// Takes up a moved tab that held no session: empty, failed, or waiting for the trust question, which
    /// is asked again in this window's page. Refused for a record of a tab that held a session.
    pub fn adopt_moved_unstarted(&mut self, handoff: TabHandoff, view: ViewId) -> Result<TabId, String> {
        let backend = match &handoff.state {
            MovedState::Live => return Err("a tab with a session is taken up with its backend".to_string()),
            MovedState::Empty => TabBackend::NotStarted,
            MovedState::Failed { reason } => TabBackend::Failed { reason: reason.clone() },
            MovedState::AwaitingTrust(start) => {
                let request_id = format!("moved-{}", self.next_nonce);
                self.next_nonce += 1;
                let start = match start.clone() {
                    MovedStart::FirstSend(first_turn) => DeferredStart::FirstSend { request_id, first_turn },
                    MovedStart::Resume {
                        provider_session_id,
                        resumed_title,
                        resumed_name,
                    } => DeferredStart::Resume {
                        request_id,
                        provider_session_id,
                        resumed_title,
                        resumed_name,
                    },
                };
                TabBackend::AwaitingTrust(AwaitingTrust {
                    start,
                    asking: None,
                    recording: false,
                })
            }
        };
        Ok(self.adopt(handoff, backend, view))
    }

    fn adopt(&mut self, handoff: TabHandoff, backend: TabBackend, view: ViewId) -> TabId {
        let TabHandoff {
            name,
            mode,
            agent,
            // Read by the caller for what `backend` is.
            state: _,
            attention,
            failed_after_start,
            failed_resume,
            last_handoff,
            editing_draft,
            review_draft,
            notes,
            title,
            title_from_record,
            project_tiers,
            stale,
            reported_start_failure,
            mut queue,
            queue_typed_by_person,
            opening_prompt,
            queue_error,
            queue_note,
            draft,
            question_draft,
            editing_question,
            rule_offers,
            was_running,
            note_ledger,
            creates_file_unsent,
            rule_offers_seen,
            turn_clock,
            last_began_turn,
            turn_times_unsent,
            host_answered,
            human_allowed,
            user_answered,
            review,
            #[cfg(any(test, feature = "test-support"))]
            gate_log,
        } = handoff;
        for (item, typed) in queue.iter_mut().zip(queue_typed_by_person) {
            item.typed_by_person = typed;
        }
        // Take the first matching hold's unused tab id in move order, so its turn's file stays across adoption.
        // Every hold stays until its move's drop; another arrival may have the same session id.
        let held = match &backend {
            TabBackend::Live(live) => live.provider_session_id().and_then(|session| {
                self.arrivals
                    .values()
                    .find(|(held_session, id)| *held_session == session && self.get(*id).is_none())
                    .map(|(_, id)| *id)
            }),
            _ => None,
        };
        let id = match held {
            Some(id) => self.insert_tab_as(id, agent, false),
            None => self.insert_tab(agent, false),
        };
        let at = self.position(id).expect("just inserted");
        let t = &mut self.tabs[at];
        t.name = name;
        t.mode = mode;
        t.backend = backend;
        t.attention = attention_from(attention);
        t.failed_after_start = failed_after_start;
        t.failed_resume = failed_resume;
        t.last_handoff = last_handoff.and_then(|moved| {
            agent::handoff::ClaudeResumeCommand::for_session(&moved.cwd, &moved.provider_session_id).ok()
        });
        t.editing_draft = editing_draft;
        t.review_draft = review_draft;
        t.notes = notes;
        t.session = SessionState {
            title,
            title_from_record,
            // It came from the other window: no page of this one is about to post a draft for it.
            left_view: None,
            project_tiers: project_tiers.map(Into::into),
            stale,
            // The turn's trace stayed with the process whose clock it read.
            turn_trace: None,
            reported_start_failure,
            queue,
            opening_prompt,
            queue_error,
            queue_note,
            draft,
            question_draft,
            editing_question,
            rule_offers: rule_offers
                .into_iter()
                .filter_map(|(id, rule)| agent::PrefixRule::parse(&rule).map(|rule| (id, rule)))
                .collect(),
            was_running,
            was_unseen_busy: false,
            note_ledger,
            creates_file_unsent,
            rule_offers_seen,
            turn_clock,
            last_began_turn,
            turn_times_unsent,
            host_answered,
            human_allowed,
            user_answered,
            foreign_server_note: ForeignServerNote::default(),
            // This window's broker has heard of no call of the tab yet.
            client_calls: ClientCalls::default(),
            editor_declared: BTreeSet::new(),
            editor_cards: Vec::new(),
        };
        #[cfg(any(test, feature = "test-support"))]
        {
            t.gate_log = gate_log;
        }
        if !view.is_base() && self.tab_in(view).is_none() {
            self.tabs[at].view = view;
        } else {
            self.select(id);
        }
        if let (Some(turn_review), Some(review)) = (self.turn_review.as_mut(), review) {
            turn_review.adopt_session(review, id.0);
        }
        self.owe_flush_if_idle(id);
        self.sync_presence();
        debug_assert!(self.v1_holds());
        id
    }

    /// A tab whose turn is over and whose queue holds messages lets them go at the next pump.
    fn owe_flush_if_idle(&mut self, tab: TabId) {
        let owed = self
            .get(tab)
            .is_some_and(|t| t.live().is_some() && !t.turn_running() && !t.session.queue.is_empty());
        if owed {
            self.flush_owed.insert(tab.0);
        }
    }
}
