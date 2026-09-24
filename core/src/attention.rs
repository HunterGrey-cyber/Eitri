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
//! **One gap is inherited, not fixed here:** [`AttentionTracker::resync`] takes the projection's
//! pending set, because a resync rebuilds the panel's cards from that same snapshot -- so a `Read`
//! the policy answered, still pending there, is counted until the next pump's `retain_pending`
//! drops it, and can raise a toast. It is the gap `AgentBackend::answer_what_needs_no_human`'s doc
//! records for the panel (a resync needs 256 queued events, never observed). The count matching
//! the cards drawn is the point; fix it there, in the snapshot, never by counting less here.
//!
//! **A session that ends owes nothing** ([`AttentionTracker::session_ended`]): its cards can no
//! longer be answered, so they stop counting even though the panel still draws them -- the count is
//! of cards still answerable. Ended off screen, it also sets `unread`: without that, a hidden chat
//! whose session died with a card waiting would go from `agent ⚑1` back to a bare `agent`, its only
//! sign gone. `shell` calls it at every place it takes a session away,
//! not only on the event that says so: a fatal command, or a session that never opened.

use std::collections::BTreeSet;

use agent::AgentDomainEvent;

/// What the agent's tray chip shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Attention {
    /// Permission cards waiting for the user.
    pub pending: usize,
    /// A turn completed, or the session ended, while the chat was not on screen, and it has not
    /// been on screen since.
    pub unread: bool,
    /// Every card the panel has been handed since the tracker was made (a new session makes a new
    /// one); it only grows. What [`react`] compares: a card that arrives in the pump that answers
    /// another leaves `pending` where it was, and is still new.
    pub arrived: u64,
}

/// The agent panel's running count. See the module doc for why it is fed deliveries.
#[derive(Debug, Default)]
pub struct AttentionTracker {
    cards: BTreeSet<String>,
    unread: bool,
    arrived: u64,
}

impl AttentionTracker {
    /// One pump's delivery, as the panel got it (after the policy answered what needs no human).
    /// `on_screen`: whether the chat is on screen now.
    pub fn observe(&mut self, delivered: &[AgentDomainEvent], on_screen: bool) {
        for event in delivered {
            match event {
                AgentDomainEvent::PermissionRequested { permission_id, .. } => {
                    if self.cards.insert(permission_id.clone()) {
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
        let cards: BTreeSet<String> = pending.into_iter().collect();
        self.arrived += cards.difference(&self.cards).count() as u64;
        self.cards = cards;
    }

    /// Forgets every card the projection no longer holds.
    pub fn retain_pending(&mut self, still_pending: impl Fn(&str) -> bool) {
        self.cards.retain(|id| still_pending(id));
    }

    /// The chat is on screen: what finished while it was away has been seen.
    pub fn seen(&mut self) {
        self.unread = false;
    }

    pub fn attention(&self) -> Attention {
        Attention {
            pending: self.cards.len(),
            unread: self.unread,
            arrived: self.arrived,
        }
    }
}

/// `init.lua`'s `neovibe.config.set("modules.chat.on_permission", ...)`.
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
                "neovibe.config.set(\"{}\", {other:?}): must be \"badge\" or \"reveal\"",
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
    /// falls back to the badge.
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

/// A new card off screen raises a toast, or reveals the chat with `Reveal` when nothing is
/// zoomed. Cards already on screen need no reaction. Detect arrivals through increases in
/// [`Attention::arrived`]: replacing one card can leave `pending` unchanged. Resetting a
/// tracker decreases `arrived` and must not react. Require a waiting card too, since cards
/// answered, cancelled, or ended within the same pump should produce neither an empty toast
/// nor an unnecessary reveal.
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
        // SessionUnavailable clears pending cards, just as SessionClosed does.
        t.observe(
            &[AgentDomainEvent::SessionUnavailable {
                reason: "the sidecar exited".into(),
            }],
            true,
        );
        assert_eq!(t.attention().pending, 0, "a session that died owes nothing either");
    }

    /// A hidden chat whose session ends keeps a sign: `agent ⚑1` becomes `agent •`, not a bare
    /// `agent` -- by the event, and by the panel taking the session
    /// itself.
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

    /// A turn completed off screen sets the unread bit.
    /// Returning to the chat clears it.
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
            "neovibe.config.set(\"modules.chat.on_permission\", \"popup\"): must be \"badge\" or \"reveal\""
        );
    }

    /// New cards off screen raise a toast by default. `reveal` shows the chat unless
    /// something is zoomed. Cards arriving in view and changes without a new arrival need no
    /// reaction.
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
        // The arrival count still increases, so the new card must trigger a notification.
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

    /// A card that arrives and is answered or ends with its session in the same pump still
    /// counts as an arrival. With no card waiting, it raises no toast and does not reveal the
    /// chat.
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
}
