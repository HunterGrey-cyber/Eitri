//! What a chat that is not on screen owes the user: how
//! many permission cards it is holding, and whether a turn finished while nobody was looking. The
//! top bar's tray reads it as `agent ⚑N` (`layout::tray::chip_label`); a card arriving while the chat
//! is away also raises a toast, or with `modules.chat.on_permission = reveal` shows the chat again.
//!
//! **Counted from what the panel was handed, never from the projection alone.** On the sidecar
//! path a request the permission policy answered itself stays in `pending_permissions` until the
//! provider's `PermissionResolved` arrives on a later pump (`AgentBackend::answer_what_needs_no_human`'s
//! own doc). Counting the projection would light `⚑1` for a moment on every `Read`. So the tracker
//! counts the `PermissionRequested` events that reached the UI -- exactly the cards the panel draws
//! -- and drops each one the projection no longer holds, which is how an answer given from the panel
//! (`respond_permission`, on legacy never a delivered event) stops counting.
//!
//! **Resync excludes requests the host already answered.**
//! [`AttentionTracker::resync`] counts exactly the set it is handed. `TabSet::pump` passes the
//! projection's pending ids minus `SessionState::host_answered`, the ids Eitri already answered
//! in bypass or under the classifier. Filtering belongs in the caller so a request nobody needs
//! a card for never counts here either.
//!
//! **A session that ends owes nothing** ([`AttentionTracker::session_ended`]): its cards can no
//! longer be answered, so they stop counting even though the panel still draws them -- the count is
//! of cards still answerable. Ended off screen, it also sets `unread`: without that, a hidden chat
//! whose session died with a card waiting would go from `agent ⚑1` back to a bare `agent`, its only
//! sign gone. `shell` calls it at every place it takes a session away,
//! not only on the event that says so: a fatal command, or a session that never opened.
//!
//! **Only the end of a turn the panel ran is news.** A `TurnCompleted` naming another turn than the
//! one the tracker saw start (Claude Code ran a turn of its own that the panel could not show), or
//! arriving when the tracker saw the last turn end, sets nothing; nor does one named
//! [`agent::NO_OPEN_TURN_ID`], which a backend gives only a completion that came with no turn of its
//! own open, whatever the tracker saw. When the tracker cannot tell (a new tracker, a new session in
//! the tab, a resync that dropped events) any other completion counts, as it always did: a stray
//! mark is a small cost, a missed finished turn a larger one. A turn Claude Code started by itself
//! that drew nothing and ended without a result is no news either: the panel drops it, and an unread
//! mark would point at nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

use agent::AgentDomainEvent;

/// One clock for every tracker in the process, so the oldest card across session tabs is
/// comparable (`prefix a` goes to the oldest). Relaxed: only the order of stamps from one
/// thread (the GTK main loop) is ever compared.
static ARRIVAL_CLOCK: AtomicU64 = AtomicU64::new(1);

fn next_stamp() -> u64 {
    ARRIVAL_CLOCK.fetch_add(1, Ordering::Relaxed)
}

/// Whether the panel shows `event` as something in the conversation: what makes a turn Claude Code
/// started worth the unread mark however it ends.
fn draws_a_row(event: &AgentDomainEvent) -> bool {
    match event {
        AgentDomainEvent::ContentDelta { text, .. } => !text.is_empty(),
        // A retry, a compaction under way or a notification shows for a moment and leaves no row.
        AgentDomainEvent::SessionNotice { kind, .. } => !kind.is_transient(),
        AgentDomainEvent::UserPromptSubmitted { .. }
        | AgentDomainEvent::UserMessageDelivered { .. }
        | AgentDomainEvent::ToolCallStarted { .. }
        | AgentDomainEvent::PermissionRequested { .. }
        | AgentDomainEvent::QuestionAsked { .. }
        | AgentDomainEvent::SubagentFrame { .. } => true,
        _ => false,
    }
}

/// What the agent's tray chip shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Attention {
    /// Permission cards, structured questions and the tool broker's cards waiting for the user.
    pub pending: usize,
    /// A turn completed, or the session ended, while the chat was not on screen, and it has not
    /// been on screen since.
    pub unread: bool,
    /// Every card the panel has been handed since the tracker was made; it only grows, across a
    /// new session in the same tab too ([`AttentionTracker::restart`]). What [`react`] compares: a
    /// card that arrives in the pump that answers another leaves `pending` where it was, and is
    /// still new.
    pub arrived: u64,
}

/// The agent panel's running count. See the module doc for why it is fed deliveries.
#[derive(Debug, Default)]
pub struct AttentionTracker {
    cards: BTreeMap<String, u64>,
    questions: BTreeMap<String, u64>,
    /// The tool broker's cards, kept apart from `cards` so nothing that approves a tab's waiting cards can name one.
    editor_cards: BTreeMap<String, u64>,
    unread: bool,
    arrived: u64,
    turn: TurnSeen,
    /// The running turn is one Claude Code started by itself, and nothing of it reached the panel yet.
    empty_cli_turn: bool,
}

/// Which turn the tracker saw running, as far as the deliveries it was handed say.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
enum TurnSeen {
    /// Nothing it saw says: a new tracker or session, events dropped by a resync, or no turn start
    /// ever delivered.
    #[default]
    Unknown,
    Running(String),
    /// The turn it saw start has ended, and no other has started since.
    Idle,
}

impl AttentionTracker {
    /// A new session in the same tab (installed, or `r` reset): no card and nothing unread, but
    /// `arrived` carries on. A fresh tracker would restart it at 0, and a window's total
    /// (`tabs::sum_attention`) that dropped in the same tick a card arrived in another tab would
    /// not rise, so [`react`] would miss that card.
    pub fn restart(&mut self) {
        self.cards.clear();
        self.questions.clear();
        self.editor_cards.clear();
        self.unread = false;
        self.turn = TurnSeen::Unknown;
        self.empty_cli_turn = false;
    }

    /// One pump's delivery, as the panel got it (after the policy answered what needs no human).
    /// `on_screen`: whether the chat is on screen now.
    pub fn observe(&mut self, delivered: &[AgentDomainEvent], on_screen: bool) {
        for event in delivered {
            match event {
                AgentDomainEvent::PermissionRequested { permission_id, .. } => {
                    self.empty_cli_turn = false;
                    if !self.cards.contains_key(permission_id) {
                        self.cards.insert(permission_id.clone(), next_stamp());
                        self.arrived += 1;
                    }
                }
                AgentDomainEvent::PermissionResolved { permission_id, .. } => {
                    self.cards.remove(permission_id);
                }
                AgentDomainEvent::TurnStarted { turn_id } => {
                    self.turn = TurnSeen::Running(turn_id.clone());
                    self.empty_cli_turn = false;
                }
                AgentDomainEvent::TurnAttributed {
                    turn_id,
                    origin: agent::TurnOrigin::Cli,
                    ..
                } if self.turn == TurnSeen::Running(turn_id.clone()) => self.empty_cli_turn = true,
                // Ended no turn of the panel's, whichever the tracker saw running.
                AgentDomainEvent::TurnCompleted { turn_id, .. } if turn_id == agent::NO_OPEN_TURN_ID => {}
                AgentDomainEvent::TurnCompleted { turn_id, detail, .. } => {
                    let counts = match &self.turn {
                        TurnSeen::Running(running) if running == turn_id => {
                            self.turn = TurnSeen::Idle;
                            let empty = std::mem::take(&mut self.empty_cli_turn);
                            !(empty && detail.reason.as_deref() == Some(agent::NO_RESULT_REASON))
                        }
                        TurnSeen::Running(_) | TurnSeen::Idle => false,
                        // Stays unknown: a backend whose turn starts are never delivered (the legacy
                        // one) must keep counting every completion.
                        TurnSeen::Unknown => true,
                    };
                    if counts && !on_screen {
                        self.unread = true;
                    }
                }
                AgentDomainEvent::SessionUnavailable { .. } | AgentDomainEvent::SessionClosed { .. } => {
                    self.session_ended(on_screen);
                }
                event if draws_a_row(event) => self.empty_cli_turn = false,
                _ => {}
            }
        }
    }

    /// The session is gone -- said by an event, or taken by the panel (a fatal command, a session
    /// that never opened): no card is answerable any more, and ended off screen it is unread (the
    /// module doc).
    pub fn session_ended(&mut self, on_screen: bool) {
        self.cards.clear();
        self.questions.clear();
        self.editor_cards.clear();
        if !on_screen {
            self.unread = true;
        }
    }

    /// A resync: the panel rebuilds every card from the projection, so the count does too. A card
    /// it holds that the tracker did not is one more arrival.
    pub fn resync(&mut self, pending: impl IntoIterator<Item = String>) {
        // The dropped events may have started or ended turns.
        self.turn = TurnSeen::Unknown;
        let ids: BTreeSet<String> = pending.into_iter().collect();
        let mut cards = BTreeMap::new();
        for id in ids {
            let stamp = match self.cards.get(&id) {
                Some(stamp) => *stamp,
                None => {
                    self.arrived += 1;
                    next_stamp()
                }
            };
            cards.insert(id, stamp);
        }
        self.cards = cards;
    }

    pub fn question_ids(&self) -> Vec<String> {
        self.questions.keys().cloned().collect()
    }

    /// Questions need attention without becoming cards that bypass may approve.
    pub fn sync_questions(&mut self, waiting: impl IntoIterator<Item = String>) {
        let ids: BTreeSet<String> = waiting.into_iter().collect();
        self.questions.retain(|id, _| ids.contains(id));
        for id in ids {
            self.questions.entry(id).or_insert_with(|| {
                self.arrived += 1;
                next_stamp()
            });
        }
    }

    /// The tool broker's cards need attention the same way, and for the same reason are not in `card_ids`: only a
    /// person's answer on a card's own id settles one.
    pub fn sync_editor_cards(&mut self, waiting: impl IntoIterator<Item = String>) {
        let ids: BTreeSet<String> = waiting.into_iter().collect();
        self.editor_cards.retain(|id, _| ids.contains(id));
        for id in ids {
            self.editor_cards.entry(id).or_insert_with(|| {
                self.arrived += 1;
                next_stamp()
            });
        }
    }

    /// Forgets every card the projection no longer holds.
    pub fn retain_pending(&mut self, still_pending: impl Fn(&str) -> bool) {
        self.cards.retain(|id, _| still_pending(id));
    }

    /// The ids this tracker still holds a card for, oldest arrival first (`TabSet::waiting_cards`:
    /// a bypass entry approves exactly these). `cards` is keyed by id, not
    /// by arrival order, so this sorts by the stamp `observe`/`resync` assigned rather than walking
    /// the map's own (alphabetical) key order.
    pub fn card_ids(&self) -> Vec<String> {
        let mut ids: Vec<(&String, &u64)> = self.cards.iter().collect();
        ids.sort_by_key(|(_, stamp)| **stamp);
        ids.into_iter().map(|(id, _)| id.clone()).collect()
    }

    /// The chat is on screen: what finished while it was away has been seen.
    pub fn seen(&mut self) {
        self.unread = false;
    }

    /// When the oldest card still pending arrived, on the process-wide clock; `None` with no card.
    pub fn oldest_pending_stamp(&self) -> Option<u64> {
        self.stamps().min().copied()
    }

    /// When the newest card still pending arrived.
    pub fn newest_pending_stamp(&self) -> Option<u64> {
        self.stamps().max().copied()
    }

    fn stamps(&self) -> impl Iterator<Item = &u64> {
        self.cards
            .values()
            .chain(self.questions.values())
            .chain(self.editor_cards.values())
    }

    pub fn attention(&self) -> Attention {
        Attention {
            pending: self.cards.len() + self.questions.len() + self.editor_cards.len(),
            unread: self.unread,
            arrived: self.arrived,
        }
    }
}

/// `init.lua`'s `eitri.config.set("modules.chat.on_permission", ...)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatOnPermission {
    /// The default: the chip and a toast. Nothing reveals itself.
    Badge,
    /// The chat reappears in its remembered place; the keys do not move.
    Reveal,
}

impl ChatOnPermission {
    /// The config key.
    pub const KEY: &'static str = "modules.chat.on_permission";

    /// The value as `init.lua` set it, or the default when it did not. Anything else is a startup
    /// failure naming the key -- the discipline `agent.font_size` follows -- and this is its message.
    pub fn parse(value: Option<&str>) -> Result<ChatOnPermission, String> {
        match value {
            None | Some("badge") => Ok(ChatOnPermission::Badge),
            Some("reveal") => Ok(ChatOnPermission::Reveal),
            Some(other) => Err(format!(
                "eitri.config.set(\"{}\", {other:?}): must be \"badge\" or \"reveal\"",
                Self::KEY
            )),
        }
    }
}

/// Where the chat is, as far as a new card is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPlace {
    OnScreen,
    /// On screen with a float over part of it: cards may be hidden, so they get a chip and answers are refused.
    Covered,
    /// Shown on an inactive tab, with no zoom keeping it away.
    TabbedAway,
    /// Hidden, with no zoom on: `reveal` can put it back.
    Hidden,
    /// Hidden while another module is zoomed: showing it would not put it on screen, so `reveal`
    /// falls back to the badge.
    HiddenUnderZoom,
    /// Shown, but another module is zoomed.
    ZoomedAway,
}

/// What a change of attention asks the window to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reaction {
    /// Raise the toast, beside the top-bar chip `agent ⚑N`.
    pub toast: bool,
    /// Show the chat where it was hidden from, without moving the keys.
    pub reveal: bool,
}

/// A card that arrives while the chat is not on screen: a toast, or with `Reveal` and nothing
/// zoomed, the chat itself. A card that arrives while it is on screen is already in view. "Arrives"
/// is [`Attention::arrived`] growing, not `pending`: a card answered and a new one in the same pump
/// leave `pending` unchanged. A total that goes down is a decrease, never a reaction (a new session in
/// the same tab does not lower it: [`AttentionTracker::restart`] keeps `arrived`). And only a card
/// still waiting reacts: one that arrived and was answered, cancelled or ended with its session in the same pump
/// has gone by the time `shell` looks, so a toast would say "0 permission cards waiting" and a
/// reveal would show the chat for nothing.
pub fn react(policy: ChatOnPermission, before: Attention, after: Attention, place: AgentPlace) -> Reaction {
    if after.arrived <= before.arrived || after.pending == 0 || place == AgentPlace::OnScreen {
        return Reaction::default();
    }
    let reveal = policy == ChatOnPermission::Reveal
        && matches!(place, AgentPlace::Hidden | AgentPlace::TabbedAway | AgentPlace::Covered);
    Reaction { toast: !reveal, reveal }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::{PermissionOutcome, TurnOutcome};

    fn requested(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input: serde_json::json!({}),
            provider_prompt: None,
        }
    }
    fn resolved(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::PermissionResolved {
            permission_id: id.into(),
            outcome: PermissionOutcome::Allowed,
        }
    }
    fn completed() -> AgentDomainEvent {
        AgentDomainEvent::TurnCompleted {
            turn_id: "t".into(),
            outcome: TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        }
    }

    #[test]
    fn a_question_counts_for_attention() {
        let mut tracker = AttentionTracker::default();
        let before = tracker.attention();
        tracker.sync_questions(["q1".into()]);
        assert_eq!(tracker.attention().pending, 1);
        assert_eq!(
            crate::layout::tray::chip_label("agent", Some(tracker.attention())),
            "agent ⚑1"
        );
        assert!(tracker.card_ids().is_empty());
        assert!(react(ChatOnPermission::Badge, before, tracker.attention(), AgentPlace::Hidden).toast);
        tracker.resync(Vec::new());
        tracker.sync_questions(["q1".into()]);
        assert_eq!(tracker.attention().arrived, 1);
        tracker.sync_questions(Vec::new());
        assert_eq!(tracker.attention().pending, 0);
    }

    #[test]
    fn cards_count_what_the_panel_was_handed_and_stop_counting_when_resolved() {
        let mut t = AttentionTracker::default();
        t.observe(&[requested("p1"), requested("p2")], false);
        assert_eq!(t.attention().pending, 2);
        t.observe(&[resolved("p1")], false);
        assert_eq!(t.attention().pending, 1);
        // Answered from the panel on legacy: no delivery says so; the projection no longer holds it.
        t.retain_pending(|id| id != "p2");
        assert_eq!(t.attention().pending, 0);
        t.observe(&[requested("p3")], false);
        t.observe(
            &[AgentDomainEvent::SessionClosed {
                reason: "closed".into(),
            }],
            false,
        );
        assert_eq!(t.attention().pending, 0, "a session that ended owes nothing");
        t.resync(["p4".to_string()]);
        assert_eq!(t.attention().pending, 1);
        // Ending a session through the explicit session-end path also preserves the unread sign.
        t.observe(
            &[AgentDomainEvent::SessionUnavailable {
                reason: "the sidecar exited".into(),
            }],
            true,
        );
        assert_eq!(t.attention().pending, 0, "a session that died owes nothing either");
    }

    /// A hidden chat whose session ends keeps an unread sign: `agent ⚑1` becomes `agent •`, not
    /// bare `agent`. Both an end event and the panel taking a failed or unopened session preserve it.
    #[test]
    fn a_session_that_ends_off_screen_leaves_the_chat_unread() {
        let mut t = AttentionTracker::default();
        t.observe(&[requested("p1")], false);
        t.observe(
            &[AgentDomainEvent::SessionUnavailable {
                reason: "the sidecar exited".into(),
            }],
            false,
        );
        assert_eq!(
            t.attention(),
            Attention {
                pending: 0,
                unread: true,
                arrived: 1
            }
        );

        let mut t = AttentionTracker::default();
        t.observe(&[requested("p1")], false);
        t.session_ended(false);
        assert_eq!((t.attention().pending, t.attention().unread), (0, true));
        let mut t = AttentionTracker::default();
        t.observe(&[requested("p1")], true);
        t.session_ended(true);
        assert_eq!(
            (t.attention().pending, t.attention().unread),
            (0, false),
            "seen as it ended"
        );
    }

    /// `arrived` counts each card once, a resync's new ones included, and never goes down.
    #[test]
    fn every_card_handed_over_is_one_arrival() {
        let mut t = AttentionTracker::default();
        t.observe(&[requested("p1"), requested("p1")], false);
        assert_eq!(t.attention().arrived, 1, "the same card twice is one card");
        t.observe(&[resolved("p1"), requested("p2")], false);
        assert_eq!((t.attention().pending, t.attention().arrived), (1, 2));
        t.resync(["p2".to_string(), "p3".to_string()]);
        assert_eq!((t.attention().pending, t.attention().arrived), (2, 3));
        t.retain_pending(|_| false);
        assert_eq!((t.attention().pending, t.attention().arrived), (0, 3));
    }

    /// A turn completing while the module is off screen sets unread; returning clears it.
    #[test]
    fn a_turn_that_finishes_off_screen_is_unread_until_the_chat_is_seen() {
        let mut t = AttentionTracker::default();
        t.observe(&[completed()], true);
        assert!(!t.attention().unread, "seen as it finished");
        t.observe(&[completed()], false);
        assert!(t.attention().unread);
        t.seen();
        assert!(!t.attention().unread);
    }

    fn started(turn: &str) -> AgentDomainEvent {
        AgentDomainEvent::TurnStarted { turn_id: turn.into() }
    }

    fn ended(turn: &str) -> AgentDomainEvent {
        AgentDomainEvent::TurnCompleted {
            turn_id: turn.into(),
            outcome: TurnOutcome::Failed,
            result_text: "the answer".into(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        }
    }

    /// The end of a turn Claude Code ran where the panel could not show it is no news: not while the
    /// panel's own turn runs, and not after that turn ended. The panel's own turn still counts.
    #[test]
    fn a_completion_of_a_turn_the_panel_did_not_run_marks_nothing_unread() {
        let mut t = AttentionTracker::default();
        t.observe(&[started("t1"), ended("unknown-turn")], false);
        assert!(!t.attention().unread, "another turn's end while t1 runs");
        t.observe(&[ended("t1")], true);
        t.observe(&[ended("unknown-turn")], false);
        assert!(!t.attention().unread, "a turn's end with none running");
        t.observe(&[started("t2"), ended("t2")], false);
        assert!(t.attention().unread, "the panel's own turn ended off screen");
    }

    /// Without a turn start to compare with (a backend that never delivers one, or events a resync
    /// dropped), every completion still counts.
    #[test]
    fn without_a_turn_start_seen_every_completion_counts() {
        let mut t = AttentionTracker::default();
        t.observe(&[started("t1")], true);
        t.resync(Vec::<String>::new());
        t.observe(&[ended("t9")], false);
        assert!(t.attention().unread);
        t.seen();
        t.observe(&[ended("t10")], false);
        assert!(t.attention().unread, "still nothing to compare with");
    }

    /// A completion that came with no turn of the backend's own open is no news even when the tracker
    /// has nothing to compare with: a resumed idle tab, or a resync while another turn runs.
    #[test]
    fn a_completion_with_no_turn_open_never_counts() {
        let mut t = AttentionTracker::default();
        t.observe(&[ended(agent::NO_OPEN_TURN_ID)], false);
        assert!(!t.attention().unread, "a new tracker");
        t.observe(&[started("t1")], true);
        t.resync(Vec::<String>::new());
        t.observe(&[ended(agent::NO_OPEN_TURN_ID)], false);
        assert!(!t.attention().unread, "after a resync, while t1 runs");
        t.observe(&[ended("t1")], false);
        assert!(t.attention().unread, "the panel's own turn still counts");
    }

    /// A turn Claude Code started is the panel's like any other: its end off screen is news. One that drew
    /// nothing and ended without a result is not: the panel drops it, so there is nothing to read.
    #[test]
    fn a_cli_turn_ending_off_screen_is_unread_unless_it_drew_nothing_and_had_no_result() {
        let cli = |turn: &str| AgentDomainEvent::TurnAttributed {
            turn_id: turn.into(),
            origin: agent::TurnOrigin::Cli,
            cause: None,
        };
        let no_result = |turn: &str| AgentDomainEvent::TurnCompleted {
            turn_id: turn.into(),
            outcome: TurnOutcome::Failed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: agent::TurnEndDetail {
                reason: Some(agent::NO_RESULT_REASON.into()),
                ..Default::default()
            },
        };
        let text = |turn: &str| AgentDomainEvent::ContentDelta {
            turn_id: turn.into(),
            kind: agent::ContentKind::Text,
            text: "done".into(),
        };
        let mut t = AttentionTracker::default();
        t.observe(&[started("c1"), cli("c1"), text("c1"), ended("c1")], false);
        assert!(t.attention().unread, "a CLI turn that said something");
        t.seen();
        t.observe(&[started("c2"), cli("c2"), no_result("c2")], false);
        assert!(!t.attention().unread, "an empty CLI turn without a result is dropped");
        t.observe(&[started("c3"), cli("c3"), text("c3"), no_result("c3")], false);
        assert!(
            t.attention().unread,
            "one that said something still counts without a result"
        );
        let notice = |kind: agent::SessionNoticeKind| AgentDomainEvent::SessionNotice {
            kind,
            level: agent::NoticeLevel::Info,
            content: None,
            scalars: Default::default(),
            task_id: None,
            tool_use_id: None,
            task_status: None,
        };
        t.seen();
        t.observe(
            &[
                started("c5"),
                cli("c5"),
                notice(agent::SessionNoticeKind::ApiRetry),
                no_result("c5"),
            ],
            false,
        );
        assert!(
            !t.attention().unread,
            "a retry shown for a moment leaves nothing to read"
        );
        t.observe(
            &[
                started("c6"),
                cli("c6"),
                notice(agent::SessionNoticeKind::ModelFallback),
                no_result("c6"),
            ],
            false,
        );
        assert!(
            t.attention().unread,
            "a notice that stays as a row is something to read"
        );
        t.seen();
        t.observe(&[started("t4"), no_result("t4")], false);
        assert!(t.attention().unread, "a turn the panel sent counts however it ends");
    }

    #[test]
    fn the_config_value_is_badge_by_default_and_anything_else_fails_naming_the_key() {
        assert_eq!(ChatOnPermission::parse(None), Ok(ChatOnPermission::Badge));
        assert_eq!(ChatOnPermission::parse(Some("badge")), Ok(ChatOnPermission::Badge));
        assert_eq!(ChatOnPermission::parse(Some("reveal")), Ok(ChatOnPermission::Reveal));
        let err = ChatOnPermission::parse(Some("popup")).unwrap_err();
        assert_eq!(
            err,
            "eitri.config.set(\"modules.chat.on_permission\", \"popup\"): must be \"badge\" or \"reveal\""
        );
    }

    /// The chat stays quiet unless asked: nothing reveals itself by default; `reveal` does, unless something is
    /// zoomed; a card that arrives in view needs nothing; only a NEW card reacts.
    #[test]
    fn a_new_card_off_screen_raises_a_toast_or_with_reveal_the_chat() {
        let none = Attention::default();
        let one = Attention {
            pending: 1,
            unread: false,
            arrived: 1,
        };
        let toast = Reaction {
            toast: true,
            reveal: false,
        };
        let reveal = Reaction {
            toast: false,
            reveal: true,
        };
        use AgentPlace::*;
        use ChatOnPermission::*;
        assert_eq!(react(Badge, none, one, Hidden), toast);
        assert_eq!(react(Badge, none, one, ZoomedAway), toast);
        assert_eq!(react(Reveal, none, one, Hidden), reveal);
        assert_eq!(
            react(Reveal, none, one, HiddenUnderZoom),
            toast,
            "reveal falls back to the badge"
        );
        assert_eq!(react(Reveal, none, one, ZoomedAway), toast);
        assert_eq!(react(Badge, none, one, OnScreen), Reaction::default());
        assert_eq!(react(Reveal, one, one, Hidden), Reaction::default(), "no new card");
        let answered = Attention { pending: 0, ..one };
        assert_eq!(
            react(Reveal, one, answered, Hidden),
            Reaction::default(),
            "a card answered"
        );
        // One answered card and one new card in the same pump leave `pending` at 1.
        let replaced = Attention { arrived: 2, ..one };
        assert_eq!(
            react(Badge, one, replaced, Hidden),
            toast,
            "a new card, whatever the count"
        );
        assert_eq!(
            react(Badge, replaced, none, Hidden),
            Reaction::default(),
            "a new session's tracker starts again"
        );
    }

    /// A card arriving and disappearing in one pump counts as arrived but not waiting. Whether
    /// answered or ended with its session, it must not trigger a toast about zero waiting cards
    /// or reveal a chat with nothing to answer.
    #[test]
    fn a_card_gone_in_the_same_pump_raises_nothing() {
        let mut tracker = AttentionTracker::default();
        let before = tracker.attention();
        tracker.observe(&[requested("p1")], false);
        tracker.session_ended(false);
        let after = tracker.attention();
        assert_eq!(
            after,
            Attention {
                pending: 0,
                unread: true,
                arrived: 1
            }
        );
        for policy in [ChatOnPermission::Badge, ChatOnPermission::Reveal] {
            for place in [AgentPlace::Hidden, AgentPlace::HiddenUnderZoom, AgentPlace::ZoomedAway] {
                assert_eq!(
                    react(policy, before, after, place),
                    Reaction::default(),
                    "{policy:?} {place:?}"
                );
            }
        }
    }

    /// Bypass entry approves delivered cards in arrival order, not alphabetical order:
    /// a plain `BTreeMap::keys()` would put `"a"` before `"b"` regardless of arrival.
    #[test]
    fn card_ids_are_oldest_first_not_alphabetical() {
        let mut t = AttentionTracker::default();
        t.observe(&[requested("b")], false);
        t.observe(&[requested("a")], false);
        assert_eq!(t.card_ids(), vec!["b".to_string(), "a".to_string()]);
    }

    /// Every tab's tracker shares one clock so the oldest card is comparable across tabs.
    #[test]
    fn cards_carry_one_arrival_clock_across_trackers() {
        let mut first = AttentionTracker::default();
        let mut second = AttentionTracker::default();
        assert_eq!(first.oldest_pending_stamp(), None);
        second.observe(&[requested("b-1")], true);
        first.observe(&[requested("a-1")], true);
        second.observe(&[requested("b-2")], true);
        let (a, b_old, b_new) = (
            first.oldest_pending_stamp().unwrap(),
            second.oldest_pending_stamp().unwrap(),
            second.newest_pending_stamp().unwrap(),
        );
        assert!(b_old < a && a < b_new, "{b_old} < {a} < {b_new}");
        // A card seen again keeps its stamp; a resync keeps the stamps of cards it already held.
        second.observe(&[requested("b-1")], true);
        assert_eq!(second.oldest_pending_stamp(), Some(b_old));
        second.resync(["b-1".to_string(), "b-2".to_string()]);
        assert_eq!(second.oldest_pending_stamp(), Some(b_old));
        second.observe(&[resolved("b-1")], true);
        assert_eq!(second.oldest_pending_stamp(), Some(b_new));
        second.session_ended(true);
        assert_eq!(second.oldest_pending_stamp(), None);
    }

    #[test]
    fn a_covered_chat_is_revealed_under_reveal_and_toasted_under_badge() {
        let before = Attention::default();
        let after = Attention {
            pending: 1,
            unread: false,
            arrived: 1,
        };
        assert_eq!(
            react(ChatOnPermission::Reveal, before, after, AgentPlace::Covered),
            Reaction {
                toast: false,
                reveal: true
            }
        );
        assert_eq!(
            react(ChatOnPermission::Badge, before, after, AgentPlace::Covered),
            Reaction {
                toast: true,
                reveal: false
            }
        );
    }

    #[test]
    fn a_broker_card_is_counted_and_stamped_and_is_never_a_card_id() {
        let mut tracker = AttentionTracker::default();
        tracker.sync_editor_cards(["ec-1".to_owned()]);
        let attention = tracker.attention();
        assert_eq!((attention.pending, attention.arrived), (1, 1));
        assert!(tracker.card_ids().is_empty());
        assert!(tracker.oldest_pending_stamp().is_some());
        assert_eq!(tracker.oldest_pending_stamp(), tracker.newest_pending_stamp());

        tracker.sync_editor_cards(["ec-1".to_owned()]);
        assert_eq!(tracker.attention().arrived, 1, "the same list again is no new arrival");
        tracker.resync(Vec::new());
        tracker.retain_pending(|_| false);
        assert_eq!(tracker.attention().pending, 1, "a rebuilt card list says nothing of it");

        tracker.sync_editor_cards(Vec::new());
        assert_eq!(tracker.attention().pending, 0);
        assert_eq!(tracker.attention().arrived, 1);
    }

    #[test]
    fn a_session_that_ends_or_starts_over_owes_no_broker_card() {
        let mut tracker = AttentionTracker::default();
        tracker.sync_editor_cards(["ec-1".to_owned()]);
        tracker.session_ended(true);
        assert_eq!(tracker.attention().pending, 0);

        tracker.sync_editor_cards(["ec-2".to_owned()]);
        tracker.restart();
        assert_eq!(tracker.attention().pending, 0);
        assert_eq!(tracker.attention().arrived, 2, "what arrived stays counted");
    }
}
