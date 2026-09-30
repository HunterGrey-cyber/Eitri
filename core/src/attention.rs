//! What a chat that is not on screen owes the user (modules spec §3.3, the owner's decision b): how
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
//! **That gap is closed as of R07/S2 (2026-09-27, Task 2), not by counting less here.**
//! [`AttentionTracker::resync`] still takes whatever set the caller hands it, but `TabSet::pump`
//! now passes the projection's pending ids MINUS `Tab::host_answered` (D9) -- the ids Eitri
//! itself already answered, in bypass or under the classifier -- so a request nobody needs to see
//! a card for is never counted here either. The fix stays in the caller, not in `resync` itself:
//! this tracker still counts exactly the set it is handed.
//!
//! **A session that ends owes nothing** ([`AttentionTracker::session_ended`]): its cards can no
//! longer be answered, so they stop counting even though the panel still draws them -- the count is
//! of cards still answerable. Ended off screen, it also sets `unread`: without that, a hidden chat
//! whose session died with a card waiting would go from `agent ⚑1` back to a bare `agent`, its only
//! sign gone (Task 6's review, minor 2). `shell` calls it at every place it takes a session away,
//! not only on the event that says so: a fatal command, or a session that never opened.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

use agent::AgentDomainEvent;

/// One clock for every tracker in the process, so the oldest card across session tabs is
/// comparable (session tabs spec §3.4, `prefix a`). Relaxed: only the order of stamps from one
/// thread (the GTK main loop) is ever compared.
static ARRIVAL_CLOCK: AtomicU64 = AtomicU64::new(1);

fn next_stamp() -> u64 {
    ARRIVAL_CLOCK.fetch_add(1, Ordering::Relaxed)
}

/// What the agent's tray chip shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Attention {
    /// Permission cards waiting for the user.
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
    unread: bool,
    arrived: u64,
}

impl AttentionTracker {
    /// A new session in the same tab (installed, or `r` reset): no card and nothing unread, but
    /// `arrived` carries on. A fresh tracker would restart it at 0, and a window's total
    /// (`tabs::sum_attention`) that dropped in the same tick a card arrived in another tab would
    /// not rise, so [`react`] would miss that card (the session-tabs whole-branch review).
    pub fn restart(&mut self) {
        self.cards.clear();
        self.unread = false;
    }

    /// One pump's delivery, as the panel got it (after the policy answered what needs no human).
    /// `on_screen`: whether the chat is on screen now.
    pub fn observe(&mut self, delivered: &[AgentDomainEvent], on_screen: bool) {
        for event in delivered {
            match event {
                AgentDomainEvent::PermissionRequested { permission_id, .. } => {
                    if !self.cards.contains_key(permission_id) {
                        self.cards.insert(permission_id.clone(), next_stamp());
                        self.arrived += 1;
                    }
                }
                AgentDomainEvent::PermissionResolved { permission_id, .. } => {
                    self.cards.remove(permission_id);
                }
                AgentDomainEvent::TurnCompleted { .. } if !on_screen => self.unread = true,
                AgentDomainEvent::SessionUnavailable { .. } | AgentDomainEvent::SessionClosed { .. } => {
                    self.session_ended(on_screen)
                }
                _ => {}
            }
        }
    }

    /// The session is gone -- said by an event, or taken by the panel (a fatal command, a session
    /// that never opened): no card is answerable any more, and ended off screen it is unread (the
    /// module doc).
    pub fn session_ended(&mut self, on_screen: bool) {
        self.cards.clear();
        if !on_screen {
            self.unread = true;
        }
    }

    /// A resync: the panel rebuilds every card from the projection, so the count does too. A card
    /// it holds that the tracker did not is one more arrival.
    pub fn resync(&mut self, pending: impl IntoIterator<Item = String>) {
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

    /// Forgets every card the projection no longer holds.
    pub fn retain_pending(&mut self, still_pending: impl Fn(&str) -> bool) {
        self.cards.retain(|id, _| still_pending(id));
    }

    /// The ids this tracker still holds a card for, oldest arrival first (R06/S2, Task 2's
    /// `TabSet::waiting_cards`: a bypass entry approves exactly these). `cards` is keyed by id, not
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
        self.cards.values().min().copied()
    }

    /// When the newest card still pending arrived.
    pub fn newest_pending_stamp(&self) -> Option<u64> {
        self.cards.values().max().copied()
    }

    pub fn attention(&self) -> Attention {
        Attention {
            pending: self.cards.len(),
            unread: self.unread,
            arrived: self.arrived,
        }
    }
}

/// `init.lua`'s `eitri.config.set("modules.chat.on_permission", ...)` (spec §3.3, decision b).
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
    /// Hidden, with no zoom on: `reveal` can put it back.
    Hidden,
    /// Hidden while another module is zoomed: showing it would not put it on screen, so `reveal`
    /// falls back to the badge (spec §3.3).
    HiddenUnderZoom,
    /// Shown, but another module is zoomed.
    ZoomedAway,
}

/// What a change of attention asks the window to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reaction {
    /// Raise the toast (decision b: "a top-bar chip `agent ⚑N` and a toast").
    pub toast: bool,
    /// Show the chat where it was hidden from, without moving the keys.
    pub reveal: bool,
}

/// A card that arrives while the chat is not on screen: a toast, or with `Reveal` and nothing
/// zoomed, the chat itself. A card that arrives while it is on screen is already in view. "Arrives"
/// is [`Attention::arrived`] growing, not `pending`: a card answered and a new one in the same pump
/// leave `pending` unchanged (Task 6's review, minor 4). A tracker reset for a new session starts
/// `arrived` again at 0, which is a decrease, and never a reaction. And only a card still waiting
/// reacts: one that arrived and was answered, cancelled or ended with its session in the same pump
/// has gone by the time `shell` looks, so a toast would say "0 permission cards waiting" and a
/// reveal would show the chat for nothing (the final fix's re-review, N1).
pub fn react(policy: ChatOnPermission, before: Attention, after: Attention, place: AgentPlace) -> Reaction {
    if after.arrived <= before.arrived || after.pending == 0 || place == AgentPlace::OnScreen {
        return Reaction::default();
    }
    let reveal = policy == ChatOnPermission::Reveal && place == AgentPlace::Hidden;
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
        }
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
        // The other half of the session-end arm (the whole-branch review's M3 dropped it, failing
        // nothing).
        t.observe(
            &[AgentDomainEvent::SessionUnavailable {
                reason: "the sidecar exited".into(),
            }],
            true,
        );
        assert_eq!(t.attention().pending, 0, "a session that died owes nothing either");
    }

    /// A hidden chat whose session ends keeps a sign: `agent ⚑1` becomes `agent •`, not a bare
    /// `agent` (Task 6's review, minor 2) -- by the event, and by the panel taking the session
    /// itself (a fatal command, a session that never opened; the whole-branch review's finding 2).
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

    /// Spec §3.3: the "unread" bit is "set when a turn completes while the module is off screen",
    /// and cleared when it is back.
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

    /// Decision b and §3.3: nothing reveals itself by default; `reveal` does, unless something is
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
        // One answered and one new in the same pump: `pending` is 1 before and after (Task 6's
        // review, minor 4).
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

    /// A card that arrives and is gone within one pump -- asked, then its session ended or it was
    /// answered, in the same delivery -- counts as arrived but is not waiting: no toast saying "0
    /// permission cards waiting", and no chat revealed for nothing (the final fix's re-review, N1).
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

    /// R06/S2: a bypass entry approves the delivered cards in arrival order, not alphabetical order
    /// -- a plain `BTreeMap::keys()` would put `"a"` before `"b"` however they arrived.
    #[test]
    fn card_ids_are_oldest_first_not_alphabetical() {
        let mut t = AttentionTracker::default();
        t.observe(&[requested("b")], false);
        t.observe(&[requested("a")], false);
        assert_eq!(t.card_ids(), vec!["b".to_string(), "a".to_string()]);
    }

    /// Ruling 20: one clock across every tab's tracker, so "the oldest card" means the same thing
    /// in tab 1 and tab 3.
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
}
