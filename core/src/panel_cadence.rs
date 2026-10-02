//! Pacing the agent panel's stream while the user types in the editor (owner decision #37, as
//! revised 2026-09-29 22:36: "我们对agent panel延时要求不高，但最好流畅").
//!
//! **Why.** GDK's frame clock is one clock for the whole toplevel: a WebKit frame the panel causes
//! delays the editor's next frame by up to one refresh, so a key pressed within ~8 ms of a panel
//! frame lands two refreshes late (`the private review notes`).
//! Eitri has no API to start a cycle early; the lever is to cause fewer, better-spaced panel
//! cycles while the user types.
//!
//! **What.** While the editor holds the keys AND a key was pressed in it within [`TYPING_WINDOW`],
//! [`PanelCadence`] holds the active tab's stream envelopes ([`EnvelopeClass::Stream`]) and releases
//! them at a fixed, even cadence (default [`DEFAULT_CADENCE_HZ`] = 5/s = every 200 ms), each release
//! carrying everything that arrived since the previous one, whole and in order. Outside that window
//! the stream goes at today's full rate. **No hold-then-burst**: the slot grid advances by one
//! period per release whether or not keys keep coming, so an auto-repeating key cannot starve the
//! panel and no release is larger than one period's arrivals.
//!
//! **What is never delayed** ([`EnvelopeClass::Immediate`]): anything but a batch of plain stream
//! content -- permission cards and their resolutions, turn start/end, session end, a snapshot
//! (resync), notes, `tabs`, `hello`, replies, tab switches -- and everything while the panel (not
//! the editor) holds the keys. Sending one first hands over whatever is pending
//! ([`Route::FlushPendingThenSend`]) so the wire order is the order things happened in. When in
//! doubt an envelope is `Immediate`.
//!
//! This module is pure (no clock of its own: every method takes an `Instant`, no GTK), so the
//! rules are testable without a display; `shell/src/panel_pacer.rs` is the thin half that owns the
//! WebView.

use std::collections::VecDeque;
use std::ops::RangeInclusive;
use std::time::{Duration, Instant};

use agent::AgentDomainEvent;

/// The config key (`init.lua`: `eitri.config.set("agent.typing_cadence_hz", "5")`).
pub const CADENCE_KEY: &str = "agent.typing_cadence_hz";

/// Pushes per second while typing when `init.lua` says nothing. 5 by the owner's selection rule
/// (decision #37, revised: the lowest late-press rate whose stream still looks even), measured on
/// the laptop against 8 and 12 (dated record, 2026-09-29 (panel stream cadence)); whether it looks
/// smooth to a person is still owed (`shell/MANUAL_VERIFICATION.md`, "Panel stream cadence").
pub const DEFAULT_CADENCE_HZ: u32 = 5;

/// What the config key accepts: a whole number of pushes per second in this range, or `"off"`.
pub const CADENCE_HZ_RANGE: RangeInclusive<u32> = 1..=60;

/// How long after an editor key the user counts as typing.
pub const TYPING_WINDOW: Duration = Duration::from_millis(500);

/// The shortest interval between two repaints the page drives by itself: its turn meter
/// (`agent-ui/web/src/index.css`, `turn-meter` over 1200 ms in 4 steps) holds each state for
/// 300 ms. A cadence whose period is at least this needs no help from the page; a slower one asks
/// the page to slow its meter down to the cadence (`editor_typing`,
/// `agent-ui/web/src/typingCadence.ts`). `indexCss.test.ts` holds this equal to the stylesheet's.
pub const SELF_DRIVEN_STEP_MS: u32 = 300;

/// The value of [`CADENCE_KEY`] as `init.lua` left it: `None` for unset (the default), else the
/// text. `Ok(None)` is `"off"` (every envelope goes at full rate, always); `Ok(Some(hz))` is the
/// cadence. Anything else is an error naming the key -- the repo's config rule, as
/// `agent.font_size` follows.
pub fn parse_config(value: Option<&str>) -> Result<Option<u32>, String> {
    let Some(raw) = value else {
        return Ok(Some(DEFAULT_CADENCE_HZ));
    };
    let text = raw.trim();
    if text == "off" {
        return Ok(None);
    }
    match text.parse::<u32>() {
        Ok(hz) if CADENCE_HZ_RANGE.contains(&hz) => Ok(Some(hz)),
        _ => Err(format!(
            "eitri.config.set(\"{CADENCE_KEY}\", {raw:?}): must be a whole number from {} to {} \
             (panel updates per second while typing in the editor) or \"off\"",
            CADENCE_HZ_RANGE.start(),
            CADENCE_HZ_RANGE.end(),
        )),
    }
}

/// How urgent one WebView-bound envelope is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeClass {
    /// Plain stream content for the active tab: may wait for the next slot while typing.
    Stream,
    /// Everything else: sent at once.
    Immediate,
}

/// What to do with one envelope, given the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Nothing is waiting and the envelope may go now.
    SendNow,
    /// Hold it: [`PanelCadence::push`] it and let [`PanelCadence::due`] release it at a slot.
    Pace,
    /// Something is waiting and this one may not overtake it: [`PanelCadence::drain_pending`]
    /// first, then send this.
    FlushPendingThenSend,
}

/// [`EnvelopeClass::Stream`] only for a batch made entirely of streamed content -- text and thinking
/// deltas, message boundaries, tool calls starting and completing -- and carrying no call notes.
/// Anything else (a permission event, a turn start or end, a prompt, a session end, ...) makes the
/// whole batch `Immediate`, and so does an empty one.
pub fn classify_events(events: &[AgentDomainEvent], has_notes: bool) -> EnvelopeClass {
    if has_notes || events.is_empty() || !events.iter().all(is_plain_stream_content) {
        EnvelopeClass::Immediate
    } else {
        EnvelopeClass::Stream
    }
}

/// Exhaustive on purpose: a new event kind must be placed here by whoever adds it, and "not sure"
/// is `false`.
fn is_plain_stream_content(event: &AgentDomainEvent) -> bool {
    match event {
        AgentDomainEvent::ContentDelta { .. }
        | AgentDomainEvent::AssistantMessageBoundary { .. }
        | AgentDomainEvent::ToolCallStarted { .. }
        | AgentDomainEvent::ToolCallCompleted { .. } => true,
        // A card, and what answers or cancels one: the user may be waiting to act on it.
        AgentDomainEvent::PermissionRequested { .. } | AgentDomainEvent::PermissionResolved { .. } => false,
        // A turn beginning or ending, what the user sent, and the session's own life: state changes
        // the panel's chrome, the tab marker and the queue's flush hang on.
        AgentDomainEvent::TurnStarted { .. }
        | AgentDomainEvent::UserPromptSubmitted { .. }
        | AgentDomainEvent::TurnCompleted { .. }
        | AgentDomainEvent::SessionOpened { .. }
        | AgentDomainEvent::SessionUnavailable { .. }
        | AgentDomainEvent::SessionClosed { .. }
        | AgentDomainEvent::ResumeOutcome { .. }
        | AgentDomainEvent::PermissionModeChanged { .. }
        | AgentDomainEvent::UngatedCliMode { .. }
        | AgentDomainEvent::CliPermissionMode { .. } => false,
        // The CLI refused a call: the row's note says why, and the user may want to act on it.
        AgentDomainEvent::PermissionDenied { .. } => false,
    }
}

/// The pacer. `T` is whatever the caller queues (the shell queues the serialized envelope plus a
/// trace flag).
#[derive(Debug)]
pub struct PanelCadence<T> {
    period: Option<Duration>,
    editor_has_keys: bool,
    last_editor_key: Option<Instant>,
    /// The next release time; `Some` from the first key of a window until `due` sees it end.
    slot: Option<Instant>,
    pending: VecDeque<T>,
}

impl<T> PanelCadence<T> {
    pub const TYPING_WINDOW: Duration = TYPING_WINDOW;

    /// `cadence_hz`: pushes per second while typing; `None` never paces.
    pub fn new(cadence_hz: Option<u32>) -> Self {
        Self {
            // Clamped as a second line of defence: `parse_config` is the place a bad number is
            // reported, and a period of zero would make `due` divide by it.
            period: cadence_hz
                .map(|hz| Duration::from_secs(1) / hz.clamp(*CADENCE_HZ_RANGE.start(), *CADENCE_HZ_RANGE.end())),
            editor_has_keys: false,
            last_editor_key: None,
            slot: None,
            pending: VecDeque::new(),
        }
    }

    /// The gap between two slots, `None` when pacing is off.
    pub fn period(&self) -> Option<Duration> {
        self.period
    }

    /// The period in milliseconds when the page has to be told the user is typing: its own repaints
    /// (the turn meter's step, [`SELF_DRIVEN_STEP_MS`]) are then faster than this cadence and would
    /// add frame-clock cycles the pacing removes elsewhere. `None` when they are already no faster
    /// (every cadence of 4/s or more) or pacing is off -- then telling the page would only cost the two
    /// repaints the message itself causes per typing burst.
    pub fn page_needs_typing_state(&self) -> Option<u32> {
        let ms = u32::try_from(self.period?.as_millis()).ok()?;
        (ms > SELF_DRIVEN_STEP_MS).then_some(ms)
    }

    /// A key was pressed (and sent to nvim) in the editor.
    pub fn note_editor_key(&mut self, now: Instant) {
        let Some(period) = self.period else { return };
        // The editor's key handler only runs when the editor has GTK focus, so this is a second
        // opinion from `pane_focus`; where the two disagree the keys are not the editor's.
        if !self.editor_has_keys {
            return;
        }
        let was_typing = self.typing(now);
        self.last_editor_key = Some(now);
        if !was_typing {
            // A new window. The grid starts one period after its first key; if the window that
            // ended before it left envelopes behind (no poll saw it end), the next poll releases
            // them at once rather than making them wait for this window's first slot.
            self.slot = Some(if self.pending.is_empty() { now + period } else { now });
        } else if self.slot.is_none() {
            self.slot = Some(now + period);
        }
    }

    /// Whether the editor pane holds the window's keys (`pane_focus`: it holds focus AND the
    /// window is active). Losing them ends the window at once.
    pub fn set_editor_has_keys(&mut self, has: bool) {
        self.editor_has_keys = has;
    }

    /// Whether the user is typing in the editor at `now`: pacing is on, the editor holds the keys
    /// and a key was pressed there less than [`TYPING_WINDOW`] ago.
    pub fn typing(&self, now: Instant) -> bool {
        self.period.is_some()
            && self.editor_has_keys
            && self
                .last_editor_key
                .is_some_and(|key| now.saturating_duration_since(key) < TYPING_WINDOW)
    }

    /// What to do with an envelope of `class` at `now`.
    pub fn route(&self, class: EnvelopeClass, now: Instant) -> Route {
        match class {
            EnvelopeClass::Stream if self.typing(now) => Route::Pace,
            _ if self.pending.is_empty() => Route::SendNow,
            _ => Route::FlushPendingThenSend,
        }
    }

    /// Holds an envelope [`Route::Pace`] asked to hold.
    pub fn push(&mut self, item: T) {
        self.pending.push_back(item);
    }

    /// Everything waiting, oldest first, removed. For [`Route::FlushPendingThenSend`], a reload and
    /// shutdown.
    pub fn drain_pending(&mut self) -> Vec<T> {
        self.pending.drain(..).collect()
    }

    /// Throws what is waiting away (a document reload or a snapshot that already carries it).
    pub fn clear_pending(&mut self) {
        self.pending.clear();
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The tick's poll: what is released at `now`. Everything waiting, in order, when a slot has
    /// come or when the typing window has ended; nothing otherwise. Cheap and idempotent: call it
    /// every tick.
    pub fn due(&mut self, now: Instant) -> Vec<T> {
        let Some(period) = self.period else {
            return self.drain_pending();
        };
        if !self.typing(now) {
            self.slot = None;
            return self.drain_pending();
        }
        let slot = *self.slot.get_or_insert(now + period);
        if now < slot {
            return Vec::new();
        }
        // A slot has come. The next one is the first grid point after `now`: a poll that ran late
        // releases once and rejoins the grid, it does not owe the slots it missed.
        let missed = now.duration_since(slot).as_nanos() / period.as_nanos();
        let steps = u32::try_from(missed.saturating_add(1)).unwrap_or(u32::MAX);
        self.slot = Some(
            period
                .checked_mul(steps)
                .and_then(|advance| slot.checked_add(advance))
                .unwrap_or(now + period),
        );
        self.drain_pending()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::{ContentKind, PermissionOutcome, TurnOutcome};

    const MS: fn(u64) -> Duration = Duration::from_millis;

    fn cadence() -> PanelCadence<String> {
        PanelCadence::new(Some(8))
    }

    /// A cadence whose editor holds the keys, with an origin instant to add offsets to.
    fn typing_cadence() -> (PanelCadence<String>, Instant) {
        let mut c = cadence();
        c.set_editor_has_keys(true);
        (c, Instant::now())
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // ---- the config key ------------------------------------------------------------------

    #[test]
    fn config_unset_is_the_default_cadence() {
        assert_eq!(parse_config(None), Ok(Some(DEFAULT_CADENCE_HZ)));
        assert_eq!(DEFAULT_CADENCE_HZ, 5);
    }

    #[test]
    fn config_accepts_the_range_and_off() {
        assert_eq!(parse_config(Some("8")), Ok(Some(8)));
        assert_eq!(parse_config(Some("1")), Ok(Some(1)));
        assert_eq!(parse_config(Some("60")), Ok(Some(60)));
        assert_eq!(parse_config(Some(" 12 ")), Ok(Some(12)));
        assert_eq!(parse_config(Some("off")), Ok(None));
    }

    #[test]
    fn config_rejects_everything_else_naming_the_key() {
        for bad in ["0", "61", "fast", "", "8.5", "-3", "Off", "8hz", "100000000000"] {
            let err = parse_config(Some(bad)).expect_err(bad);
            assert!(err.contains(CADENCE_KEY), "{bad:?}: {err}");
            assert!(err.contains(&format!("{bad:?}")), "{bad:?} echoed: {err}");
        }
    }

    #[test]
    fn what_init_lua_sets_reaches_the_parser_as_the_store_holds_it() {
        use crate::lua::config::{install, ConfigStore};
        use std::cell::RefCell;
        use std::rc::Rc;
        let lua = mlua::Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();
        for (script, expected) in [
            (r#"eitri.config.set("agent.typing_cadence_hz", "5")"#, Ok(Some(5))),
            // A Lua number is stored as its text.
            (r#"eitri.config.set("agent.typing_cadence_hz", 12)"#, Ok(Some(12))),
            (r#"eitri.config.set("agent.typing_cadence_hz", "off")"#, Ok(None)),
        ] {
            lua.load(script).exec().unwrap();
            assert_eq!(parse_config(store.borrow().get(CADENCE_KEY)), expected, "{script}");
        }
        lua.load(r#"eitri.config.set("agent.typing_cadence_hz", 0)"#)
            .exec()
            .unwrap();
        assert!(parse_config(store.borrow().get(CADENCE_KEY)).is_err());
    }

    // ---- the typing window ---------------------------------------------------------------

    #[test]
    fn typing_needs_the_editor_to_hold_the_keys_and_a_recent_editor_key() {
        let (mut c, t0) = typing_cadence();
        assert!(!c.typing(t0), "no key yet");
        c.note_editor_key(t0);
        assert!(c.typing(t0));
        assert!(c.typing(t0 + MS(499)));
        assert!(!c.typing(t0 + MS(500)), "the window is strictly shorter than 500 ms");
        assert!(!c.typing(t0 + MS(5_000)));
        assert_eq!(PanelCadence::<String>::TYPING_WINDOW, MS(500));
    }

    #[test]
    fn a_key_while_the_editor_does_not_hold_the_keys_does_not_type() {
        let mut c = cadence();
        let t0 = Instant::now();
        c.note_editor_key(t0);
        assert!(!c.typing(t0), "the panel or the terminal holds the keys");
        c.set_editor_has_keys(true);
        assert!(
            !c.typing(t0),
            "a key pressed before the editor had the keys does not count"
        );
    }

    #[test]
    fn losing_the_keys_ends_the_window_at_once() {
        let (mut c, t0) = typing_cadence();
        c.note_editor_key(t0);
        c.set_editor_has_keys(false);
        assert!(!c.typing(t0 + MS(1)));
    }

    #[test]
    fn off_never_types() {
        let mut c: PanelCadence<String> = PanelCadence::new(None);
        c.set_editor_has_keys(true);
        let t0 = Instant::now();
        c.note_editor_key(t0);
        assert!(!c.typing(t0));
        assert_eq!(c.period(), None);
        assert_eq!(c.route(EnvelopeClass::Stream, t0), Route::SendNow);
        assert!(c.due(t0 + MS(10_000)).is_empty());
    }

    #[test]
    fn the_page_is_told_about_typing_only_when_its_own_motion_would_outpace_the_cadence() {
        let needs = |hz: Option<u32>| PanelCadence::<String>::new(hz).page_needs_typing_state();
        // The meter holds each state for 300 ms: a cadence of 4/s (250 ms) or faster already
        // covers it; 3/s (333 ms) and slower would be outpaced by it.
        assert_eq!(needs(Some(8)), None);
        assert_eq!(needs(Some(60)), None);
        assert_eq!(needs(Some(4)), None);
        assert_eq!(needs(Some(3)), Some(333));
        assert_eq!(needs(Some(2)), Some(500));
        assert_eq!(needs(Some(1)), Some(1000));
        assert_eq!(needs(None), None);
        assert_eq!(SELF_DRIVEN_STEP_MS, 300);
    }

    #[test]
    fn the_period_is_a_second_over_the_rate() {
        assert_eq!(PanelCadence::<String>::new(Some(8)).period(), Some(MS(125)));
        assert_eq!(PanelCadence::<String>::new(Some(1)).period(), Some(MS(1000)));
        assert_eq!(PanelCadence::<String>::new(Some(5)).period(), Some(MS(200)));
    }

    // ---- routing -------------------------------------------------------------------------

    #[test]
    fn a_stream_envelope_is_paced_only_while_typing() {
        let (mut c, t0) = typing_cadence();
        assert_eq!(c.route(EnvelopeClass::Stream, t0), Route::SendNow, "not typing yet");
        c.note_editor_key(t0);
        assert_eq!(c.route(EnvelopeClass::Stream, t0), Route::Pace);
        assert_eq!(c.route(EnvelopeClass::Stream, t0 + MS(499)), Route::Pace);
        assert_eq!(c.route(EnvelopeClass::Stream, t0 + MS(500)), Route::SendNow);
    }

    #[test]
    fn an_immediate_envelope_is_never_paced_and_never_overtakes_a_waiting_one() {
        let (mut c, t0) = typing_cadence();
        c.note_editor_key(t0);
        assert_eq!(c.route(EnvelopeClass::Immediate, t0), Route::SendNow);
        c.push("a".to_string());
        assert_eq!(
            c.route(EnvelopeClass::Immediate, t0 + MS(1)),
            Route::FlushPendingThenSend
        );
        assert_eq!(c.drain_pending(), strs(&["a"]));
        assert_eq!(c.route(EnvelopeClass::Immediate, t0 + MS(2)), Route::SendNow);
    }

    #[test]
    fn a_stream_envelope_after_the_window_ends_does_not_overtake_a_waiting_one() {
        let (mut c, t0) = typing_cadence();
        c.note_editor_key(t0);
        c.push("a".to_string());
        // Window over, but `due` has not been polled yet.
        assert_eq!(
            c.route(EnvelopeClass::Stream, t0 + MS(510)),
            Route::FlushPendingThenSend
        );
    }

    // ---- the even cadence ----------------------------------------------------------------

    #[test]
    fn everything_since_the_last_slot_goes_out_at_the_slot_in_order() {
        let (mut c, t0) = typing_cadence();
        c.note_editor_key(t0);
        c.push("a".to_string());
        c.push("b".to_string());
        assert!(c.due(t0 + MS(124)).is_empty(), "slot is one period after the first key");
        c.push("c".to_string());
        assert_eq!(c.due(t0 + MS(125)), strs(&["a", "b", "c"]));
        assert!(!c.has_pending());
    }

    #[test]
    fn slots_are_fixed_and_even_whatever_the_keys_do() {
        let (mut c, t0) = typing_cadence();
        c.note_editor_key(t0);
        c.push("a".to_string());
        assert_eq!(c.due(t0 + MS(125)), strs(&["a"]));
        // Keys at odd moments do not move the next slot (250 ms).
        c.note_editor_key(t0 + MS(130));
        c.push("b".to_string());
        c.note_editor_key(t0 + MS(200));
        assert!(c.due(t0 + MS(249)).is_empty());
        assert_eq!(c.due(t0 + MS(250)), strs(&["b"]));
        c.push("c".to_string());
        c.note_editor_key(t0 + MS(260));
        assert!(c.due(t0 + MS(374)).is_empty());
        assert_eq!(c.due(t0 + MS(375)), strs(&["c"]));
    }

    #[test]
    fn a_slot_with_nothing_waiting_still_advances_the_grid() {
        let (mut c, t0) = typing_cadence();
        c.note_editor_key(t0);
        assert!(c.due(t0 + MS(125)).is_empty());
        c.note_editor_key(t0 + MS(200));
        c.push("a".to_string());
        // Held for the next slot (250), not sent at once and not a new grid from the push.
        assert!(c.due(t0 + MS(240)).is_empty());
        assert_eq!(c.due(t0 + MS(250)), strs(&["a"]));
    }

    #[test]
    fn a_late_poll_releases_once_and_lands_back_on_the_grid() {
        let (mut c, t0) = typing_cadence();
        c.note_editor_key(t0);
        c.push("a".to_string());
        // The main loop stalled through slots at 125, 250, 375.
        c.note_editor_key(t0 + MS(400));
        assert_eq!(c.due(t0 + MS(410)), strs(&["a"]));
        c.push("b".to_string());
        // No catch-up burst: the next slot is the next grid point (500), not "now".
        assert!(c.due(t0 + MS(411)).is_empty());
        assert!(c.due(t0 + MS(499)).is_empty());
        assert_eq!(c.due(t0 + MS(500)), strs(&["b"]));
    }

    #[test]
    fn an_auto_repeating_key_never_starves_the_panel_and_never_bursts() {
        // A key every 30 ms for 3 s, a 33 ms poll, one envelope per poll: the panel must be
        // served every slot, each release holding about a period's worth.
        let (mut c, t0) = typing_cadence();
        let mut last_release = t0;
        let mut releases: Vec<(Duration, usize)> = Vec::new();
        let mut next_key = t0;
        let mut pushed = 0usize;
        let mut released = 0usize;
        for tick in 0..=90u64 {
            let now = t0 + MS(tick * 33);
            while next_key <= now {
                c.note_editor_key(next_key);
                next_key += MS(30);
            }
            let route = c.route(EnvelopeClass::Stream, now);
            assert_eq!(route, Route::Pace, "tick {tick}: typing the whole time");
            c.push(format!("e{tick}"));
            pushed += 1;
            let out = c.due(now);
            if !out.is_empty() {
                releases.push((now - last_release, out.len()));
                released += out.len();
                last_release = now;
            }
        }
        assert!(releases.len() >= 20, "{} releases in 3 s", releases.len());
        for (gap, n) in &releases {
            assert!(*gap <= MS(125 + 33), "gap {gap:?} between releases");
            assert!(
                *n <= 5,
                "{n} envelopes in one release (about 125/33 = 4 arrive per period)"
            );
        }
        assert_eq!(released + c.drain_pending().len(), pushed, "nothing lost");
    }

    // ---- leaving the window --------------------------------------------------------------

    #[test]
    fn the_end_of_the_window_releases_everything_at_once() {
        let (mut c, t0) = typing_cadence();
        c.note_editor_key(t0);
        c.push("a".to_string());
        c.push("b".to_string());
        c.note_editor_key(t0 + MS(100));
        assert!(c.due(t0 + MS(124)).is_empty());
        assert_eq!(c.due(t0 + MS(125)), strs(&["a", "b"]));
        for slot in [250, 375, 500] {
            assert!(c.due(t0 + MS(slot)).is_empty(), "nothing waits at {slot}");
        }
        // The last key was at 100, so the window ends at 600; the next slot would be 625. "c" must
        // not wait for it: the poll that sees the window over releases it.
        c.push("c".to_string());
        assert!(c.due(t0 + MS(511)).is_empty());
        assert!(
            c.due(t0 + MS(599)).is_empty(),
            "still inside the window, waiting for the slot"
        );
        assert_eq!(
            c.due(t0 + MS(600)),
            strs(&["c"]),
            "released by the window ending, not a slot"
        );
        assert_eq!(c.route(EnvelopeClass::Stream, t0 + MS(601)), Route::SendNow);
    }

    #[test]
    fn losing_the_keys_releases_what_waits_at_the_next_poll() {
        let (mut c, t0) = typing_cadence();
        c.note_editor_key(t0);
        c.push("a".to_string());
        c.set_editor_has_keys(false);
        assert_eq!(c.due(t0 + MS(10)), strs(&["a"]));
        assert!(c.due(t0 + MS(11)).is_empty());
    }

    #[test]
    fn a_new_window_after_idle_starts_a_fresh_grid_one_period_after_its_first_key() {
        let (mut c, t0) = typing_cadence();
        c.note_editor_key(t0);
        assert!(c.due(t0 + MS(600)).is_empty(), "window over, nothing waiting");
        let t1 = t0 + MS(10_000);
        c.note_editor_key(t1);
        c.push("a".to_string());
        assert!(c.due(t1 + MS(1)).is_empty(), "not released at once");
        assert!(c.due(t1 + MS(124)).is_empty());
        assert_eq!(c.due(t1 + MS(125)), strs(&["a"]));
    }

    #[test]
    fn what_an_ended_window_left_is_released_at_the_next_poll_after_a_new_key() {
        // The window ended and a key arrived before the tick polled `due`: the old window's
        // envelopes must not wait for the new window's first slot.
        let (mut c, t0) = typing_cadence();
        c.note_editor_key(t0);
        c.push("a".to_string());
        c.note_editor_key(t0 + MS(505));
        assert_eq!(c.due(t0 + MS(506)), strs(&["a"]));
    }

    #[test]
    fn clear_pending_drops_what_waits() {
        let (mut c, t0) = typing_cadence();
        c.note_editor_key(t0);
        c.push("a".to_string());
        c.clear_pending();
        assert!(!c.has_pending());
        assert!(c.due(t0 + MS(125)).is_empty());
    }

    // ---- classification ------------------------------------------------------------------

    fn delta() -> AgentDomainEvent {
        AgentDomainEvent::ContentDelta {
            turn_id: "t".into(),
            kind: ContentKind::Text,
            text: "hi".into(),
        }
    }

    #[test]
    fn plain_stream_content_is_stream() {
        let events = vec![
            delta(),
            AgentDomainEvent::ContentDelta {
                turn_id: "t".into(),
                kind: ContentKind::Thinking,
                text: "hm".into(),
            },
            AgentDomainEvent::AssistantMessageBoundary { turn_id: "t".into() },
            AgentDomainEvent::ToolCallStarted {
                turn_id: "t".into(),
                tool_use_id: "u".into(),
                name: "Read".into(),
                input: serde_json::json!({}),
            },
            AgentDomainEvent::ToolCallCompleted {
                turn_id: "t".into(),
                tool_use_id: "u".into(),
                content: serde_json::json!("ok"),
                is_error: false,
            },
        ];
        assert_eq!(classify_events(&events, false), EnvelopeClass::Stream);
        assert_eq!(classify_events(&events[..1], false), EnvelopeClass::Stream);
    }

    #[test]
    fn any_event_that_is_not_plain_content_makes_the_whole_batch_immediate() {
        let others = vec![
            AgentDomainEvent::PermissionRequested {
                permission_id: "p".into(),
                tool_use_id: None,
                tool_name: "Write".into(),
                input: serde_json::json!({}),
                provider_prompt: None,
            },
            AgentDomainEvent::PermissionResolved {
                permission_id: "p".into(),
                outcome: PermissionOutcome::Allowed,
            },
            AgentDomainEvent::TurnStarted { turn_id: "t".into() },
            AgentDomainEvent::UserPromptSubmitted { text: "x".into() },
            AgentDomainEvent::TurnCompleted {
                turn_id: "t".into(),
                outcome: TurnOutcome::Completed,
                result_text: String::new(),
                stop_reason: None,
                usage: None,
            },
            AgentDomainEvent::SessionUnavailable { reason: "x".into() },
            AgentDomainEvent::SessionClosed { reason: "x".into() },
            AgentDomainEvent::UngatedCliMode {
                reported: "bypassPermissions".into(),
                detail: "x".into(),
            },
        ];
        for other in others {
            let batch = vec![delta(), other.clone(), delta()];
            assert_eq!(classify_events(&batch, false), EnvelopeClass::Immediate, "{other:?}");
            assert_eq!(
                classify_events(std::slice::from_ref(&other), false),
                EnvelopeClass::Immediate,
                "{other:?}"
            );
        }
    }

    #[test]
    fn notes_and_empty_batches_are_immediate() {
        assert_eq!(classify_events(&[delta()], true), EnvelopeClass::Immediate);
        assert_eq!(classify_events(&[], false), EnvelopeClass::Immediate);
    }
}
