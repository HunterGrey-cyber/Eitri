//! A chat's state as it moves from one window to the other.
//!
//! Two records cross: [`ChatMoveCore`], what the destination folds into a new projection, and
//! [`ConversationHandoff`], the conversation's own state beyond its projection. Neither carries a
//! live declaration -- a status, an active turn, a waiting card, an open message or thought. Those
//! come out of the folds in the destination exactly as they came out of them in the source:
//!
//! 1. the session's lifecycle events (the last `SessionOpened` and every resume verdict, end or
//!    loss folded after it, up to the export), into an empty projection, where they create no
//!    record and set the status;
//! 2. one [`AgentDomainEvent::MoveSeeded`] carrying [`MoveSeed`]: every settled record with its own
//!    `seq`, and the revision the export was taken at, which the projection takes over;
//! 3. the running turn's events, as the source folded them, which therefore get the source's own
//!    sequence numbers.
//!
//! So an item a view names by its `seq` is the same item in either window, and the revision the
//! destination reports is the one the source would have.

use crate::ingestion::{IngestStats, ResumeOutcomeRecord};
use crate::lease::SessionLease;
use crate::projection::{
    AccountLimitsRecord, AgentDomainEvent, AgentSessionProjection, CallIndex, GateInput, HistoryNotice, InputChange,
    NoticeRecord, QuestionRecord, SlashCommand, SteerOutcome, ThinkingBlock, ToolCallDenial, ToolCallRecord,
    TranscriptMessage, TurnEndingKind, TurnEndingRecord, TurnStartRecord, UngatedCliModeRecord, UsageInfo,
    UserPromptRecord,
};
use crate::provider::{AgentProvider, ProviderCapabilities, ProviderInfo};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

/// Every settled record of a projection with its own sequence number, and the revision it was taken
/// at. Built by [`AgentSessionProjection::export_settled`] and taken by a projection only through
/// [`AgentDomainEvent::MoveSeeded`]; its fields are this crate's, so nothing else assembles one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MoveSeed {
    pub(crate) session_id: Option<String>,
    pub(crate) provider_session_id: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) user_prompts: Vec<UserPromptRecord>,
    pub(crate) transcript: Vec<TranscriptMessage>,
    pub(crate) tool_calls: Vec<ToolCallRecord>,
    pub(crate) turn_endings: Vec<TurnEndingRecord>,
    #[serde(default)]
    pub(crate) turn_starts: Vec<TurnStartRecord>,
    #[serde(default)]
    pub(crate) notices: Vec<NoticeRecord>,
    #[serde(default)]
    pub(crate) questions: Vec<QuestionRecord>,
    pub(crate) last_turn_ending: Option<TurnEndingKind>,
    pub(crate) usage: Option<UsageInfo>,
    pub(crate) thinking: Vec<ThinkingBlock>,
    pub(crate) history: Option<HistoryNotice>,
    pub(crate) ungated_cli_mode: Option<UngatedCliModeRecord>,
    pub(crate) cli_mode_reported: Option<String>,
    pub(crate) policy_mismatch: Option<(String, String)>,
    pub(crate) account_limits: Option<AccountLimitsRecord>,
    #[serde(default)]
    pub(crate) background_tasks: crate::TaskRoster,
    pub(crate) background_calls: u32,
    pub(crate) available_commands: Option<Vec<SlashCommand>>,
    pub(crate) background_at_last_turn_end: u32,
    pub(crate) denials_before_their_call: VecDeque<(String, ToolCallDenial)>,
    pub(crate) gate_inputs: VecDeque<(String, GateInput)>,
    pub(crate) changed_inputs_before_their_call: VecDeque<(String, InputChange)>,
    pub(crate) call_index: CallIndex,
    pub(crate) steer_outcomes: VecDeque<(String, SteerOutcome)>,
    /// Whether Claude Code was working where the panel cannot show it. Settled for the move: no fold
    /// after the seed sets it again until the next such notice arrives, which may be long after.
    #[serde(default)]
    pub(crate) cli_unseen_busy: bool,
    /// The model the CLI fell back to, and the latest rate-limit warning: reports that stay on the
    /// band after their turn ends until a later report replaces them, so they are settled for the move.
    #[serde(default)]
    pub(crate) fallback_model: Option<String>,
    #[serde(default)]
    pub(crate) rate_limit_warning: Option<NoticeRecord>,
    /// The exporting projection's `last_revision`: every record's `seq` is below it, and the adopting
    /// projection's revision equals it once the seed is folded.
    pub through_revision: u64,
}

/// What the destination folds, in this order: `lifecycle`, then `seed`, then `running_turn`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMoveCore {
    /// The last `SessionOpened` folded before the export and every resume verdict, unavailability or
    /// close folded after it, in order; while no `SessionOpened` had been folded, every verdict,
    /// unavailability or close folded so far. Empty for a session that had neither opened nor ended.
    pub lifecycle: Vec<AgentDomainEvent>,
    pub seed: MoveSeed,
    /// The running turn's events as the source folded them, its locally folded prompt included;
    /// empty for an idle chat, whose seed was taken at the freeze.
    pub running_turn: Vec<AgentDomainEvent>,
    /// The provider's own sequence number of the last event it had taken in when the source stopped
    /// folding (`AgentProvider::pumped_through`), every event up to it folded: the destination's
    /// provider reads on from the next one. 0 when the provider numbers nothing or had reported no
    /// position yet.
    pub folded_through: u64,
}

/// Why a move's records were not taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedRefused {
    /// The lifecycle list holds a `SessionOpened` anywhere but first, or something other than the
    /// session's opening, verdicts, unavailability and close.
    LifecycleOutOfOrder,
    /// A seed among the lifecycle or running-turn events: only the move places one, once.
    SeedOutOfPlace,
    /// The projection already took a seed.
    AlreadySeeded,
    /// The projection already holds records, which the seed's would sit beside.
    HoldsRecords,
    /// The seed names no revision, or one behind the projection's own.
    RevisionBehind,
    /// A record's `seq` is not below the seed's revision.
    SeqPastRevision,
}

impl std::fmt::Display for SeedRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let what = match self {
            SeedRefused::LifecycleOutOfOrder => {
                "the session's lifecycle events are not in the order they are folded in"
            }
            SeedRefused::SeedOutOfPlace => "a seed is among the events to fold",
            SeedRefused::AlreadySeeded => "this conversation was already seeded",
            SeedRefused::HoldsRecords => "this conversation already holds records",
            SeedRefused::RevisionBehind => "the seed's revision is behind this conversation's",
            SeedRefused::SeqPastRevision => "a record of the seed is numbered past the seed's revision",
        };
        f.write_str(what)
    }
}

impl std::error::Error for SeedRefused {}

/// A rename noted and not yet written, as the conversation keeps it until its record exists. Its own
/// type because `Option<Option<String>>` reads back from JSON as `None` when it was `Some(None)`, which
/// would turn "clear the name" into "nothing to do".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingName {
    pub name: Option<String>,
}

/// The conversation's own state beyond its projection, as the source held it at the freeze. Built by
/// destructuring the conversation and its ingest without a rest pattern, so a field added to either
/// has to be placed here or named as not moved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationHandoff {
    pub conversation_id: String,
    pub canonical_cwd: PathBuf,
    pub capabilities: ProviderCapabilities,
    pub info: ProviderInfo,
    pub session_id: Option<String>,
    pub title_noted: bool,
    /// The name the lease and the record are keyed by (`claude`, or the external provider's).
    pub provider_name: String,
    pub provider_advertises_resume: bool,
    pub provider_session_id: Option<String>,
    pub resume_outcome: Option<ResumeOutcomeRecord>,
    pub stats: IngestStats,
    pub pending_title: Option<String>,
    pub pending_name: Option<PendingName>,
    /// Whether the session's record is on disk: wrong, a title would be lost or the record written
    /// twice.
    pub record_written: bool,
    /// Whether something the history stores (a turn's or the session's end, a question's ask, its
    /// call's start or its settle) was folded since the history was last written: lost, it would never
    /// be written.
    pub history_dirty: bool,
}

/// Why a conversation did not move.
#[derive(Debug)]
pub enum HandoffRefused {
    /// Its close failed, or it was shut down: its session is not one to hand on.
    Closing,
    /// Idle, yet a card is waiting or a reply is open, which no fold in the destination would bring
    /// back.
    Unsettled,
    /// Folded events the source had not delivered when the record was to be taken.
    Undelivered,
    /// Its ingest was folding a batch, or writing after one, when the freeze was asked: nothing was
    /// changed, and the same freeze asked again in a moment goes through.
    Folding,
    /// The source's last pump could not finish (its answers were not taken in time).
    LastPump(String),
    /// The destination could not rebuild the projection.
    Seed(SeedRefused),
    /// The provider or the lease handed to the destination is not the conversation's.
    Mismatch(&'static str),
}

impl std::fmt::Display for HandoffRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandoffRefused::Closing => f.write_str("the conversation is closing"),
            HandoffRefused::Unsettled => f.write_str("a card or a reply of this conversation is still open"),
            HandoffRefused::Undelivered => f.write_str("the conversation has events its page has not been given"),
            HandoffRefused::Folding => f.write_str("the conversation is folding its events; ask again"),
            HandoffRefused::LastPump(why) => write!(f, "the last answers were not taken: {why}"),
            HandoffRefused::Seed(refused) => write!(f, "the conversation could not be rebuilt: {refused}"),
            HandoffRefused::Mismatch(what) => write!(f, "{what} is not this conversation's"),
        }
    }
}

impl std::error::Error for HandoffRefused {}

/// What the destination drives the adopted conversation with: the provider (adopted by its own
/// handoff), its external side for Codex and ACP, and the session lease rebuilt from the descriptor
/// that crossed with the move.
pub struct MovedProvider {
    pub agent: Arc<dyn AgentProvider + Send + Sync>,
    pub external: Option<Arc<dyn crate::external::ExternalProvider + Send + Sync>>,
    pub lease: Option<SessionLease>,
}

/// Whether `event` may stand in a lifecycle list at `position`: `SessionOpened` only first (each one
/// starts the list again), and otherwise only what can change the session's status, which a session
/// lost before it opened has with no `SessionOpened` before it.
fn lifecycle_fits(position: usize, event: &AgentDomainEvent) -> bool {
    match event {
        AgentDomainEvent::SessionOpened { .. } => position == 0,
        AgentDomainEvent::ResumeOutcome { .. }
        | AgentDomainEvent::SessionUnavailable { .. }
        | AgentDomainEvent::SessionClosed { .. } => true,
        _ => false,
    }
}

impl AgentSessionProjection {
    /// The settled records, with their `seq`s and this projection's revision. Taken where no live
    /// declaration is set (no turn, no card, no open reply or thought): the destination gets those
    /// only from the folds around the seed, so a live one set here would be dropped.
    ///
    /// Destructures `Self` without a rest pattern: a field added to the projection does not compile
    /// here until it is placed in the seed or named as live.
    pub fn export_settled(&self) -> MoveSeed {
        let AgentSessionProjection {
            session_id,
            provider_session_id,
            model,
            cwd,
            // Live: set by the lifecycle events folded before the seed.
            status: _,
            // Live: set by the running turn's events folded after it.
            active_turn_id: _,
            user_prompts,
            transcript,
            tool_calls,
            // Live: a waiting card is rebuilt by folding the request that made it.
            pending_permissions: _,
            turn_endings,
            turn_starts,
            notices,
            // Live: transient phases are rebuilt by the receiving session.
            api_retry: _,
            compacting: _,
            // Settled: they stay on the band after the turn that reported them, until a later report
            // replaces them.
            rate_limit_warning,
            fallback_model,
            // Live, as the phases above.
            notification: _,
            questions,
            last_turn_ending,
            // Not carried: it tells the window that started a turn whether it ended, and a tab moves
            // only once no turn of its own is starting.
            last_turn_end_seq: _,
            usage,
            // Live: whether the next delta continues a reply is the running turn's to say.
            assistant_message_open: _,
            thinking,
            // Live, as the open reply is.
            thinking_open: _,
            last_revision,
            history,
            ungated_cli_mode,
            cli_mode_reported,
            policy_mismatch,
            account_limits,
            background_tasks,
            background_calls,
            available_commands,
            background_at_last_turn_end,
            denials_before_their_call,
            gate_inputs,
            changed_inputs_before_their_call,
            call_index,
            steer_outcomes,
            // Not carried: the adopting projection marks its own seed when it takes this one.
            move_seeded: _,
            cli_unseen_busy,
            // Not carried, as `last_turn_end_seq`: they describe a turn being started or running, and a
            // tab moves only once no turn of its own is starting.
            prompt_before_turn: _,
            running_turn_prompt_is_slash: _,
            running_turn_said_text: _,
            opening_prompt: _,
            running_turn_cli: _,
        } = self;
        MoveSeed {
            session_id: session_id.clone(),
            provider_session_id: provider_session_id.clone(),
            model: model.clone(),
            cwd: cwd.clone(),
            user_prompts: user_prompts.clone(),
            transcript: transcript.clone(),
            tool_calls: tool_calls.clone(),
            turn_endings: turn_endings.clone(),
            turn_starts: turn_starts.clone(),
            notices: notices.clone(),
            questions: questions.clone(),
            last_turn_ending: *last_turn_ending,
            usage: usage.clone(),
            thinking: thinking.clone(),
            history: history.clone(),
            ungated_cli_mode: ungated_cli_mode.clone(),
            cli_mode_reported: cli_mode_reported.clone(),
            policy_mismatch: policy_mismatch.clone(),
            account_limits: account_limits.clone(),
            background_tasks: background_tasks.clone(),
            background_calls: *background_calls,
            available_commands: available_commands.clone(),
            background_at_last_turn_end: *background_at_last_turn_end,
            denials_before_their_call: denials_before_their_call.clone(),
            gate_inputs: gate_inputs.clone(),
            changed_inputs_before_their_call: changed_inputs_before_their_call.clone(),
            call_index: call_index.clone(),
            steer_outcomes: steer_outcomes.clone(),
            cli_unseen_busy: *cli_unseen_busy,
            fallback_model: fallback_model.clone(),
            rate_limit_warning: rate_limit_warning.clone(),
            through_revision: *last_revision,
        }
    }

    /// A projection rebuilt from a move's records by folding alone: the lifecycle events, the seed,
    /// the running turn's events. Refused when the lists are not what a source exports or the seed
    /// does not fit.
    pub fn adopt(core: &ChatMoveCore) -> Result<AgentSessionProjection, SeedRefused> {
        if !core
            .lifecycle
            .iter()
            .enumerate()
            .all(|(at, event)| lifecycle_fits(at, event))
        {
            return Err(SeedRefused::LifecycleOutOfOrder);
        }
        if core
            .running_turn
            .iter()
            .any(|event| matches!(event, AgentDomainEvent::MoveSeeded(_)))
        {
            return Err(SeedRefused::SeedOutOfPlace);
        }
        let mut projection = AgentSessionProjection::default();
        for event in &core.lifecycle {
            projection.apply(event);
        }
        if core.seed.through_revision == 0 {
            // Exported before anything was folded: a projection that has folded nothing holds nothing,
            // so there is nothing to place, and the running turn numbers from zero as it did there.
            if !core.lifecycle.is_empty() || core.seed != AgentSessionProjection::default().export_settled() {
                return Err(SeedRefused::RevisionBehind);
            }
        } else {
            projection.seed_fits(&core.seed)?;
            projection.apply(&AgentDomainEvent::MoveSeeded(Box::new(core.seed.clone())));
        }
        for event in &core.running_turn {
            projection.apply(event);
        }
        Ok(projection)
    }

    /// Whether this projection would take `seed`.
    pub(crate) fn seed_fits(&self, seed: &MoveSeed) -> Result<(), SeedRefused> {
        if self.move_seeded {
            return Err(SeedRefused::AlreadySeeded);
        }
        if !self.holds_no_record() {
            return Err(SeedRefused::HoldsRecords);
        }
        let through = seed.through_revision;
        if through == 0 || through < self.last_revision {
            return Err(SeedRefused::RevisionBehind);
        }
        let seqs = seed
            .user_prompts
            .iter()
            .map(|r| r.seq)
            .chain(seed.transcript.iter().map(|r| r.seq))
            .chain(seed.tool_calls.iter().map(|r| r.seq))
            .chain(seed.turn_endings.iter().map(|r| r.seq))
            .chain(seed.turn_starts.iter().map(|r| r.seq))
            .chain(seed.notices.iter().map(|r| r.seq))
            .chain(seed.questions.iter().map(|r| r.seq))
            .chain(seed.thinking.iter().map(|r| r.seq))
            .chain(seed.rate_limit_warning.iter().map(|r| r.seq));
        if seqs.into_iter().any(|seq| seq >= through) {
            return Err(SeedRefused::SeqPastRevision);
        }
        Ok(())
    }

    /// No item of any collection and no restored history: what a projection that has folded only
    /// lifecycle events holds.
    fn holds_no_record(&self) -> bool {
        self.user_prompts.is_empty()
            && self.transcript.is_empty()
            && self.tool_calls.is_empty()
            && self.pending_permissions.is_empty()
            && self.turn_endings.is_empty()
            && self.turn_starts.is_empty()
            && self.notices.is_empty()
            && self.background_tasks.rows.is_empty()
            && self.questions.is_empty()
            && self.thinking.is_empty()
            && self.history.is_none()
    }

    /// No live declaration is set: no turn runs, no card waits, no reply or thought is open. What
    /// [`Self::export_settled`] needs to lose nothing.
    pub(crate) fn is_settled(&self) -> bool {
        self.active_turn_id.is_none()
            && self.pending_permissions.is_empty()
            && !self.assistant_message_open
            && !self.thinking_open
    }

    /// The seed's arm of `apply`: every record and settled value in place, and the revision one short
    /// of the seed's, which `apply`'s own bump completes. Destructures the seed without a rest
    /// pattern, so a field added to it has to be placed here too.
    pub(crate) fn place_seed(&mut self, seed: &MoveSeed) {
        let MoveSeed {
            session_id,
            provider_session_id,
            model,
            cwd,
            user_prompts,
            transcript,
            tool_calls,
            turn_endings,
            turn_starts,
            notices,
            questions,
            last_turn_ending,
            usage,
            thinking,
            history,
            ungated_cli_mode,
            cli_mode_reported,
            policy_mismatch,
            account_limits,
            background_tasks,
            background_calls,
            available_commands,
            background_at_last_turn_end,
            denials_before_their_call,
            gate_inputs,
            changed_inputs_before_their_call,
            call_index,
            steer_outcomes,
            cli_unseen_busy,
            fallback_model,
            rate_limit_warning,
            through_revision,
        } = seed;
        self.session_id = session_id.clone();
        self.provider_session_id = provider_session_id.clone();
        self.model = model.clone();
        self.cwd = cwd.clone();
        self.user_prompts = user_prompts.clone();
        self.transcript = transcript.clone();
        self.tool_calls = tool_calls.clone();
        self.turn_endings = turn_endings.clone();
        self.turn_starts = turn_starts.clone();
        self.notices = notices.clone();
        self.questions = questions.clone();
        self.last_turn_ending = *last_turn_ending;
        self.usage = usage.clone();
        self.thinking = thinking.clone();
        self.history = history.clone();
        self.ungated_cli_mode = ungated_cli_mode.clone();
        self.cli_mode_reported = cli_mode_reported.clone();
        self.policy_mismatch = policy_mismatch.clone();
        self.account_limits = account_limits.clone();
        self.background_tasks = background_tasks.clone();
        self.background_calls = *background_calls;
        self.available_commands = available_commands.clone();
        self.background_at_last_turn_end = *background_at_last_turn_end;
        self.denials_before_their_call = denials_before_their_call.clone();
        self.gate_inputs = gate_inputs.clone();
        self.changed_inputs_before_their_call = changed_inputs_before_their_call.clone();
        self.call_index = call_index.clone();
        self.steer_outcomes = steer_outcomes.clone();
        self.cli_unseen_busy = *cli_unseen_busy;
        self.fallback_model = fallback_model.clone();
        self.rate_limit_warning = rate_limit_warning.clone();
        // `seed_fits` refused a zero revision, so this does not wrap.
        self.last_revision = through_revision - 1;
        self.move_seeded = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::{
        ContentKind, PermissionOutcome, ProjectionStatus, ResumeStatus, TurnEndDetail, TurnOutcome,
    };
    use serde_json::json;

    fn opened() -> AgentDomainEvent {
        AgentDomainEvent::SessionOpened {
            session_id: "s-1".into(),
            provider_session_id: "p-1".into(),
            model: "m".into(),
            cwd: "/w".into(),
        }
    }

    fn fold(projection: &mut AgentSessionProjection, events: &[AgentDomainEvent]) {
        for event in events {
            projection.apply(event);
        }
    }

    /// Two finished turns' worth of every kind of settled record and value: prompts, a reply, a
    /// thought, a tool call with its result, an interrupted turn's ending, usage, the account's
    /// limits, the CLI's mode, the slash commands, a refusal held for a call that never came, a
    /// remembered gate input, a steer outcome.
    fn settled_history() -> Vec<AgentDomainEvent> {
        vec![
            AgentDomainEvent::UserPromptSubmitted { text: "first".into() },
            AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
            AgentDomainEvent::CliPermissionMode {
                reported: "auto".into(),
            },
            AgentDomainEvent::ContentDelta {
                turn_id: "t1".into(),
                kind: ContentKind::Thinking,
                text: "hmm".into(),
            },
            AgentDomainEvent::ContentDelta {
                turn_id: "t1".into(),
                kind: ContentKind::Text,
                text: "a reply".into(),
            },
            AgentDomainEvent::ToolCallStarted {
                turn_id: "t1".into(),
                tool_use_id: "call-1".into(),
                name: "Bash".into(),
                input: json!({ "command": "ls" }),
            },
            AgentDomainEvent::PermissionRequested {
                permission_id: "perm-1".into(),
                tool_use_id: Some("call-1".into()),
                tool_name: "Bash".into(),
                input: json!({ "command": "ls" }),
                provider_prompt: None,
            },
            AgentDomainEvent::PermissionResolved {
                permission_id: "perm-1".into(),
                outcome: PermissionOutcome::Allowed,
            },
            AgentDomainEvent::ToolCallCompleted {
                turn_id: "t1".into(),
                tool_use_id: "call-1".into(),
                content: json!("a b"),
                is_error: false,
            },
            AgentDomainEvent::UserMessageDropped {
                client_message_id: "c-1".into(),
                reason: "too late".into(),
            },
            AgentDomainEvent::BackgroundCalls { running: 1 },
            AgentDomainEvent::TurnCompleted {
                turn_id: "t1".into(),
                outcome: TurnOutcome::Interrupted,
                result_text: String::new(),
                stop_reason: None,
                // Neither value has a short decimal form: the seed has to read back to the same bits.
                usage: Some(UsageInfo {
                    total_cost_usd: 0.1 + 0.2,
                    ..UsageInfo::default()
                }),
                detail: TurnEndDetail::default(),
            },
            AgentDomainEvent::AccountLimits {
                used_percent: 100.0 / 3.0,
                window_minutes: Some(300),
                resets_at: Some(1_800_000_000),
                plan: Some("max".into()),
            },
            AgentDomainEvent::CommandsAvailable {
                commands: vec![SlashCommand {
                    name: "review".into(),
                    description: "review".into(),
                    hint: None,
                }],
            },
            AgentDomainEvent::PermissionDenied {
                tool_use_id: Some("call-never".into()),
                tool_name: "Bash".into(),
                reason_type: Some("classifier".into()),
                reason: Some("no".into()),
            },
            AgentDomainEvent::ToolInputChanged {
                tool_use_id: "call-never".into(),
                change: InputChange {
                    fields: serde_json::Map::from_iter([("command".to_string(), json!("rm"))]),
                    removed: Vec::new(),
                    clipped: false,
                },
            },
            AgentDomainEvent::PermissionRequested {
                permission_id: "perm-late".into(),
                tool_use_id: Some("call-late".into()),
                tool_name: "Read".into(),
                input: json!({ "file_path": "b.rs" }),
                provider_prompt: None,
            },
            AgentDomainEvent::PermissionResolved {
                permission_id: "perm-late".into(),
                outcome: PermissionOutcome::Denied,
            },
            AgentDomainEvent::UngatedCliMode {
                reported: "bypassPermissions".into(),
                detail: "seen".into(),
            },
            AgentDomainEvent::PolicyMismatch {
                requested: "default".into(),
                reported: "plan".into(),
            },
        ]
    }

    /// A projection that folded `lifecycle` and then the settled history.
    fn source_with(lifecycle: &[AgentDomainEvent]) -> AgentSessionProjection {
        let mut source = AgentSessionProjection::default();
        source.history = Some(HistoryNotice {
            source: crate::projection::HistorySource::EitriCopy,
            restored_items: 0,
            omitted_items: Some(0),
            upto_seq: 0,
            source_path: "/state/history.json".into(),
            attempted_transcript_path: None,
            fallback_reason: Some("no transcript".into()),
            writer_version: None,
        });
        fold(&mut source, lifecycle);
        fold(&mut source, &settled_history());
        source
    }

    fn core(lifecycle: Vec<AgentDomainEvent>, seed: MoveSeed, running_turn: Vec<AgentDomainEvent>) -> ChatMoveCore {
        ChatMoveCore {
            lifecycle,
            seed,
            running_turn,
            folded_through: 0,
        }
    }

    #[test]
    fn a_seed_round_trips_with_every_sequence_number_and_the_last_revision() {
        let source = source_with(&[opened()]);
        assert!(source.is_settled(), "the history ends between turns");
        assert!(!source.denials_before_their_call.is_empty() && !source.gate_inputs.is_empty());
        assert!(!source.changed_inputs_before_their_call.is_empty() && source.ungated_cli_mode.is_some());
        assert!(!source.steer_outcomes.is_empty() && source.call_index != CallIndex::default());
        assert!(source.usage.is_some() && source.policy_mismatch.is_some() && source.history.is_some());

        let seed = source.export_settled();
        let wire = serde_json::to_string(&seed).unwrap();
        let back: MoveSeed = serde_json::from_str(&wire).unwrap();
        assert_eq!(
            serde_json::to_string(&back).unwrap(),
            wire,
            "the seed reads back as written"
        );
        assert_eq!(back, seed);

        let adopted = AgentSessionProjection::adopt(&core(vec![opened()], back, Vec::new())).unwrap();
        assert_eq!(
            serde_json::to_value(&adopted).unwrap(),
            serde_json::to_value(&source).unwrap(),
            "every record with its own seq, and the revision"
        );
        assert_eq!(adopted.last_revision, source.last_revision);
        assert_eq!(
            adopted.export_settled(),
            source.export_settled(),
            "the values a snapshot leaves out are carried too"
        );
    }

    #[test]
    fn a_background_roster_moves_with_its_count_and_pending_outcomes() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../agent-ui/web/src/fixtures/background-tasks.json")).unwrap();
        for case in cases["cases"].as_array().unwrap() {
            let mut source = AgentSessionProjection::default();
            let events: Vec<AgentDomainEvent> = serde_json::from_value(case["domain_events"].clone()).unwrap();
            for event in &events {
                source.apply_with_derived(event);
            }
            let seed = source.export_settled();
            let encoded = serde_json::to_vec(&seed).unwrap();
            let seed = serde_json::from_slice(&encoded).unwrap();
            let mut destination = AgentSessionProjection::default();
            destination.apply(&AgentDomainEvent::MoveSeeded(Box::new(seed)));
            assert_eq!(
                source.background_tasks, destination.background_tasks,
                "{}",
                case["name"]
            );
            assert_eq!(
                source.background_calls, destination.background_calls,
                "{}",
                case["name"]
            );
        }
    }

    /// A fallback to another model and a rate-limit warning outlive the turn that reported them: the
    /// band keeps showing both until a later report replaces them, so the window a tab moves to shows
    /// them too, and a second move carries them on.
    #[test]
    fn a_reported_fallback_model_and_rate_limit_warning_move_with_the_chat() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../agent-ui/web/src/fixtures/session-notices.json")).unwrap();
        let mut notices = Vec::new();
        for name in ["fallback", "rate_allowed_warning"] {
            let case = cases["cases"]
                .as_array()
                .unwrap()
                .iter()
                .find(|case| case["name"] == name)
                .unwrap();
            let events: Vec<AgentDomainEvent> = serde_json::from_value(case["domain_events"].clone()).unwrap();
            notices.extend(events);
        }
        let mut source = source_with(&[opened()]);
        fold(&mut source, &notices);
        assert!(source.is_settled());
        assert_eq!(source.fallback_model.as_deref(), Some("small"));
        assert!(source.rate_limit_warning.is_some());

        let wire = serde_json::to_string(&source.export_settled()).unwrap();
        let seed: MoveSeed = serde_json::from_str(&wire).unwrap();
        let adopted = AgentSessionProjection::adopt(&core(vec![opened()], seed, Vec::new())).unwrap();
        assert_eq!(adopted.fallback_model.as_deref(), Some("small"));
        assert_eq!(adopted.rate_limit_warning, source.rate_limit_warning);
        let (mut shown, mut destination) = (serde_json::json!({}), serde_json::json!({}));
        source.snapshot_modules(&mut shown);
        adopted.snapshot_modules(&mut destination);
        assert_eq!(shown["fallbackModel"], destination["fallbackModel"]);
        assert_eq!(shown["rateLimitWarning"], destination["rateLimitWarning"]);
        assert_eq!(adopted.export_settled(), source.export_settled());

        // A report after the move replaces what the move carried, as it would have in the source.
        let mut after = adopted;
        let cleared: AgentDomainEvent = serde_json::from_value(serde_json::json!({
            "type": "session_notice", "kind": "rate_limit", "level": "notice", "content": null,
            "scalars": {"rate_status": "allowed"}, "task_id": null, "task_status": null, "tool_use_id": null
        }))
        .unwrap();
        after.apply(&cleared);
        assert!(after.rate_limit_warning.is_none());
    }

    /// Claude Code working where the panel cannot show it, with no turn of the panel's running, is a
    /// settled chat that moves; the note goes with it, or the other window would close the tab and hand
    /// it to a terminal without asking until the next notice.
    #[test]
    fn work_out_of_sight_moves_with_the_chat() {
        let mut source = source_with(&[opened()]);
        source.apply(&AgentDomainEvent::CliWorkUnseen);
        assert!(source.is_settled() && source.cli_unseen_busy);
        let wire = serde_json::to_string(&source.export_settled()).unwrap();
        let seed: MoveSeed = serde_json::from_str(&wire).unwrap();
        let adopted = AgentSessionProjection::adopt(&core(vec![opened()], seed, Vec::new())).unwrap();
        assert!(adopted.cli_unseen_busy);
        assert_eq!(adopted.export_settled(), source.export_settled());
    }

    #[test]
    fn lifecycle_events_folded_before_the_seed_give_running() {
        let source = source_with(&[opened()]);
        let adopted =
            AgentSessionProjection::adopt(&core(vec![opened()], source.export_settled(), Vec::new())).unwrap();
        assert_eq!(adopted.status, ProjectionStatus::Running);

        // Without the lifecycle events nothing would say the session runs: the seed never sets a status.
        let mut bare = AgentSessionProjection::default();
        bare.apply(&AgentDomainEvent::MoveSeeded(Box::new(source.export_settled())));
        assert_eq!(bare.status, ProjectionStatus::Starting);
        assert_eq!(bare.last_revision, source.last_revision);
    }

    #[test]
    fn session_opened_then_a_rejected_resume_gives_unavailable() {
        let lifecycle = vec![
            opened(),
            AgentDomainEvent::ResumeOutcome {
                requested_provider_session_id: "p-0".into(),
                status: ResumeStatus::Rejected,
                attached_provider_session_id: None,
                forked: false,
                detail: None,
            },
        ];
        let source = source_with(&lifecycle);
        assert!(matches!(source.status, ProjectionStatus::Unavailable { .. }));
        let adopted = AgentSessionProjection::adopt(&core(lifecycle, source.export_settled(), Vec::new())).unwrap();
        assert_eq!(adopted.status, source.status);
        assert_eq!(
            serde_json::to_value(&adopted).unwrap(),
            serde_json::to_value(&source).unwrap()
        );
    }

    #[test]
    fn a_seed_is_refused_on_a_projection_holding_a_record_and_a_second_time() {
        let seed = source_with(&[opened()]).export_settled();

        let mut holding = AgentSessionProjection::default();
        holding.apply(&AgentDomainEvent::UserPromptSubmitted { text: "mine".into() });
        assert_eq!(holding.seed_fits(&seed), Err(SeedRefused::HoldsRecords));
        let before = holding.last_revision;
        holding.apply(&AgentDomainEvent::MoveSeeded(Box::new(seed.clone())));
        assert_eq!(
            holding.user_prompts.len(),
            1,
            "the seed's records were not placed beside the prompt"
        );
        assert_eq!(holding.last_revision, before + 1, "a refused seed is still one fold");
        assert!(!holding.move_seeded);

        let mut fresh = AgentSessionProjection::default();
        fresh.apply(&AgentDomainEvent::MoveSeeded(Box::new(seed.clone())));
        assert!(fresh.move_seeded);
        let placed = serde_json::to_value(&fresh).unwrap();
        // An empty seed would fit the record check; the mark is what refuses it.
        let mut empty = AgentSessionProjection::default();
        empty.apply(&opened());
        let mut once = AgentSessionProjection::default();
        once.apply(&AgentDomainEvent::MoveSeeded(Box::new(empty.export_settled())));
        assert_eq!(once.seed_fits(&empty.export_settled()), Err(SeedRefused::AlreadySeeded));
        fresh.apply(&AgentDomainEvent::MoveSeeded(Box::new(seed.clone())));
        let mut again = serde_json::to_value(&fresh).unwrap();
        again["last_revision"] = placed["last_revision"].clone();
        assert_eq!(again, placed, "a second seed placed nothing");

        // Nor does a seed travel among the events folded around it.
        let refused = AgentSessionProjection::adopt(&core(
            vec![opened()],
            seed.clone(),
            vec![AgentDomainEvent::MoveSeeded(Box::new(seed.clone()))],
        ));
        assert_eq!(refused.unwrap_err(), SeedRefused::SeedOutOfPlace);
        let refused = AgentSessionProjection::adopt(&core(
            vec![AgentDomainEvent::SessionClosed { reason: "x".into() }, opened()],
            seed,
            Vec::new(),
        ));
        assert_eq!(refused.unwrap_err(), SeedRefused::LifecycleOutOfOrder);
    }

    #[test]
    fn a_running_turns_records_get_the_sources_seqs_after_the_fold() {
        let mut source = source_with(&[opened()]);
        // Taken where the source takes it: at the running turn's start.
        let seed = source.export_settled();
        let running_turn = vec![
            AgentDomainEvent::UserPromptSubmitted { text: "second".into() },
            AgentDomainEvent::TurnStarted { turn_id: "t2".into() },
            // The SDK reports the session again at every turn's start.
            opened(),
            AgentDomainEvent::ContentDelta {
                turn_id: "t2".into(),
                kind: ContentKind::Text,
                text: "working".into(),
            },
            AgentDomainEvent::ToolCallStarted {
                turn_id: "t2".into(),
                tool_use_id: "call-2".into(),
                name: "Edit".into(),
                input: json!({ "file_path": "a.rs" }),
            },
            AgentDomainEvent::PermissionRequested {
                permission_id: "perm-2".into(),
                tool_use_id: Some("call-2".into()),
                tool_name: "Edit".into(),
                input: json!({ "file_path": "a.rs" }),
                provider_prompt: None,
            },
        ];
        fold(&mut source, &running_turn);

        let wire = serde_json::to_string(&core(vec![opened()], seed, running_turn)).unwrap();
        let carried: ChatMoveCore = serde_json::from_str(&wire).unwrap();
        let adopted = AgentSessionProjection::adopt(&carried).unwrap();
        assert_eq!(
            serde_json::to_value(&adopted).unwrap(),
            serde_json::to_value(&source).unwrap(),
            "the running turn's prompt, reply, call and card carry the source's seqs"
        );
        assert_eq!(adopted.last_revision, source.last_revision);
        assert_eq!(adopted.active_turn_id.as_deref(), Some("t2"));
        assert!(adopted.pending_permissions.contains_key("perm-2"));
        assert_eq!(adopted.assistant_message_open, source.assistant_message_open);
        assert_eq!(adopted.export_settled(), source.export_settled());
    }

    /// A move during the first turn, before the provider reported the session: the prompt was the
    /// first thing folded, so the seed names revision zero and the session opens inside the turn.
    #[test]
    fn a_first_turn_moved_before_the_session_opened_numbers_from_zero() {
        let mut source = AgentSessionProjection::default();
        let seed = source.export_settled();
        let running_turn = vec![
            AgentDomainEvent::UserPromptSubmitted { text: "hello".into() },
            opened(),
            AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
        ];
        fold(&mut source, &running_turn);
        let adopted = AgentSessionProjection::adopt(&core(Vec::new(), seed, running_turn)).unwrap();
        assert_eq!(
            serde_json::to_value(&adopted).unwrap(),
            serde_json::to_value(&source).unwrap()
        );
    }

    #[test]
    fn a_session_lost_before_it_opened_is_rebuilt_unavailable() {
        let lost = AgentDomainEvent::SessionUnavailable {
            reason: "the event stream failed".into(),
        };
        let mut source = AgentSessionProjection::default();
        fold(
            &mut source,
            &[
                AgentDomainEvent::UserPromptSubmitted { text: "hello".into() },
                lost.clone(),
            ],
        );
        assert!(source.is_settled());

        let adopted = AgentSessionProjection::adopt(&core(vec![lost], source.export_settled(), Vec::new())).unwrap();
        assert_eq!(
            adopted.status,
            ProjectionStatus::Unavailable {
                reason: "the event stream failed".into()
            }
        );
        assert_eq!(
            serde_json::to_value(&adopted).unwrap(),
            serde_json::to_value(&source).unwrap()
        );
    }

    #[test]
    fn every_finite_float_reads_back_to_the_same_bits() {
        // The seed's floats cross as JSON text; a parser that is only nearly right changes a value by
        // its last bit, and the rebuilt projection then differs from the source's.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut checked = 0;
        while checked < 20_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let value = f64::from_bits(state);
            if !value.is_finite() {
                continue;
            }
            let back: f64 = serde_json::from_str(&serde_json::to_string(&value).unwrap()).unwrap();
            assert_eq!(back.to_bits(), value.to_bits(), "{value:e} read back as {back:e}");
            checked += 1;
        }
    }
}
