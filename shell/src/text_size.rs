//! Zoom both panes together, and each one alone. Spec:
//! docs/superpowers/specs/2026-09-22-zoom-together-design.md, corrected 2026-09-23 (see §4), then
//! corrected again 2026-09-23 (the pending model dropped time altogether).
//!
//! Two numbers and a base (§2): **S** is `g:neovide_scale_factor`, nvim's own variable and the
//! source of truth every writer (our keys, the owner's mappings, a typed `:let`) goes through and
//! everyone follows. **R** is the panel's factor relative to S, held here so it survives a panel
//! reload, 1.0 at startup, never persisted across restarts. **base** is `agent.font_size` (or the
//! shipped default). `panel px = base × clamp(S × R, ZOOM_MIN, ZOOM_MAX)`; call `P = S × R` the
//! panel's effective scale.
//!
//! [`TextSize`] is the pure model -- one function per row of spec §3's two tables, each returning
//! the new state and, where the row writes S, the value to hand `set_scale_factor_setting` (if
//! any -- see §4). [`TextSizeController`] is the GTK wiring around it, mirroring
//! `window_mode::WindowModes`: the three app-level "both panes" accelerators, and following nvim's
//! report of S back (§4's asynchrony/echo rule) so a colorscheme recompute squeezed between a
//! write and its echo cannot see the wrong value.
//!
//! ## §4, corrected (2026-09-23): the blocking defect, and why `pending` was timestamped
//!
//! The first version of this model set `pending: Option<f32>` on every write and cleared it only
//! when nvim's `on_scale_factor_setting` callback echoed the SAME value back. That callback,
//! though, only fires when nvim's own per-tick watch sees the variable actually CHANGE
//! (`neovide-editor`'s `ScaleWatch::observe`, by construction -- and rightly so, that is what makes
//! an unrelated colorscheme recompute cheap). So a write that assigned the value nvim ALREADY held
//! produced no change, produced no callback, and therefore never cleared `pending` -- it stuck
//! forever. Reproduced exactly as found: `TextSize::new(14.0, 1.0).both_reset()` at S already 1.0
//! sets `pending = Some(1.0)` and it never echoes; a later external `:let g:neovide_scale_factor =
//! 2.0` updates `reported_s` to 2.0 but `current_s` kept preferring the stale `pending` (1.0) over
//! it, so the panel stayed rendered at 14px instead of following the editor to 28px; and the next
//! `Ctrl+=` then stepped off that stale 1.0 to 1.1 -- the editor **shrank** from 2.0 rather than
//! growing from it, with no user action that should have shrunk anything at all.
//!
//! This first-round fix gave `pending` its own `since: Instant` and a `PENDING_TTL = 1s`: past
//! that, `current_s` fell back to `reported_s` regardless, and `shell` scheduled a one-shot `glib`
//! flush at that same delay so the panel would correct itself even with no echo at all.
//!
//! ## §4, corrected again (2026-09-23, later): the TTL was itself wrong, and there is no fix to a
//! ## deadline -- only to not having one
//!
//! An adversarial recheck found the TTL model wrong whenever nvim's echo simply arrives later than
//! one second -- not a rare fault, just a slow tick (a blocking command, a busy compositor, a
//! loaded machine). Worst case: two editor-only zoom presses in a row, the second one made after
//! the first's TTL had already expired with no echo yet. `current_s` fell back to `reported_s`
//! (the STALE value from before either press), so the second press's `R' = R·S/S'` compensation
//! was computed from the wrong `S` -- and the resulting wrong `R` never self-corrects, because once
//! nvim's real echoes for both presses eventually arrive, `reported_s` catches up but `R` does not
//! move again. The panel was left at the wrong size **permanently**, not just for the length of the
//! gap. Every other TTL-adjacent case (a flash back to the old size for the gap between expiry and
//! a late echo; a second `both_larger` after expiry repeating the same value instead of stepping
//! further; an external report landing inside the one-second window some other write is still
//! occupying) was real too, all for the same underlying reason: a timeout is a guess at how long
//! nvim will take, and any guess is wrong on some machine on some day.
//!
//! The fix removes the guess instead of tuning it. Nothing in the model measures time any more --
//! no `Instant`, no `PENDING_TTL`, no `glib` timeout. `pending` is cleared in exactly three ways,
//! none of them a clock: nvim's echo of it; a report of a value that is not in the current `burst`
//! at all (an external write, treated as authoritative); or a write of a value equal to
//! `reported_s` (rule 2 of the write rules below). An echo of an OLDER write still in `burst`
//! clears nothing -- it only moves `reported_s`. A write nvim never answers at all -- the one case a
//! TTL existed to rescue -- has one recovery in this design, and it is the one the owner already has
//! a key for: `Ctrl+0`/`app.text-reset` writes `1.0`, and when nvim's last real value is `1.0` that
//! write takes rule 2 and clears the stuck `pending`. See `nvim_reported_s`'s and `write`'s own doc
//! for the rules, and this module's `reset_heals_a_pending_stuck_on_a_write_nvim_never_echoed`
//! test for the exact scenario.
//!
//! **Two races this model does not survive, both recorded by the second recheck (2026-09-23) and
//! neither reachable by hand.** (1) Two *different* keys inside one frame, the second reversing the
//! first, with nvim applying and a tick reporting between them in one particular order (0.7% of all
//! three-key interleavings, never under autorepeat or a busy-nvim catch-up): `R` and the step count
//! stay wrong until a reset. (2) Another program writing `g:neovide_scale_factor` back to its
//! last-reported value within one tick of our write: the tick sees no change, so `pending` stays
//! stuck until the next key. Both need two writers or two chords inside ~16ms.
//!
//! ### The corrected model (no time anywhere)
//!
//! - `reported_s`: the last value nvim actually reported -- authoritative for the editor. Starts at
//!   the pane's `scale_factor_setting()` read at startup, not an assumed 1.0.
//! - `pending: Option<f32>`: the last value this window wrote that nvim has not yet echoed back.
//! - `burst: Vec<f32>`, capped at [`BURST_CAP`] (oldest dropped): every value this window has
//!   written since `pending` was last `None`. Lets a report recognise an echo of an EARLIER write in
//!   a fast sequence (not just the latest one) as still "ours", rather than mistaking it for an
//!   external change and clearing a pending write that is still genuinely outstanding.
//! - `current_s() = pending.unwrap_or(reported_s)` (substituted with `1.0` when not finite and
//!   positive, §2) -- both what the panel displays against and what every step computes its next
//!   value from. There is no deadline on how long a pending value is trusted; it is trusted until a
//!   report says otherwise, however long that takes.
//! - **A write of `v`** (already rounded to two decimals, as every write here is):
//!   1. If `v == current_s()`, nothing is written and nothing changes at all -- nvim's watcher would
//!      see no change either, so there is nothing to wait for (the same "never write an equal
//!      value" rule `window_mode::setting_to_write` already uses).
//!   2. Else if `v == reported_s`, the write **is** still sent (nvim's live value may currently be
//!      the outstanding `pending`, which really does need correcting) -- but `pending` and `burst`
//!      are cleared regardless of whatever echo nvim does or does not send back for it. Once
//!      `reported_s` itself becomes `v` (whether nvim echoes this exact write or simply reports its
//!      own eventual settling), the state is already correct; nothing needs tracking.
//!   3. Otherwise, send it, `pending = Some(v)`, and push `v` onto `burst`.
//! - **A report of `r`:** `reported_s = r`, unconditionally.
//!   - If `pending == Some(r)`, it is our own write's echo: `pending = None`, `burst` cleared.
//!   - Else if `burst` contains `r`, it is an echo of an OLDER write of ours still inside the
//!     current run: `pending` (and `burst`) are left alone -- an intermediate echo must not be
//!     mistaken for confirmation that nothing is outstanding.
//!   - Else, it is an external change (the owner's own mapping, a typed `:let`) or simply nothing
//!     we recognise: `pending = None`, `burst` cleared. `r` (R) is never touched by a report --
//!     "together" is the default for every writer, not just this window's own keys.
//! - **Editor-only steps** (`editor_larger`/`-smaller`/`-reset`) compute `R' = R · current_s() / S'`
//!   from `current_s()` at the moment of the step, exactly as before -- there is simply no longer a
//!   TTL that can make that read stale.
//! - **Panel-only steps** (`panel_larger`/`-smaller`) never write S at all: `P' = clamp(current_s() ×
//!   R ± 0.1)`, `R' = P'/current_s()`.
//! - **Reset** (`app.text-reset`, `Ctrl+a 0` on the editor) sets `R = 1` and writes `1.0` through
//!   the same `write` rules above -- which is what makes it a heal, not just a jump to a known
//!   value: if nvim's `reported_s` is already `1.0` (a pending write nvim never answered is stuck on
//!   some OTHER value), rule 2 fires and the stuck `pending` is cleared unconditionally.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;
use neovibe_core::layout::{ModuleId, ModuleKind};
use neovide_editor::NeovideEditorPane;

use crate::agent_panel::AgentPanelHandle;

pub(crate) const ZOOM_STEP: f32 = 0.1;
pub(crate) const ZOOM_MIN: f32 = 0.5;
pub(crate) const ZOOM_MAX: f32 = 3.0;

/// How many of this window's own not-yet-echoed writes are remembered at once (§4), so an
/// intermediate echo of an EARLIER one -- not necessarily the latest -- can still be recognised as
/// ours rather than mistaken for an external change. Far more than any realistic run of key
/// presses between ticks; oldest drops first if it is ever exceeded.
const BURST_CAP: usize = 64;

/// Every value this module writes is rounded to two decimals so repeated steps do not accumulate
/// float noise (§2) -- ten `both_larger` steps from 1.0 land on exactly 2.0 because each step
/// starts from the previous step's already-rounded result, not because `+=0.1` is somehow exact.
fn round2(x: f32) -> f32 {
    (x * 100.0).round() / 100.0
}

/// A value this module is about to write to `g:neovide_scale_factor`: clamped to the zoom range
/// and rounded (§2). Reports arriving FROM nvim are never passed through this -- they are
/// "respected as-is for the editor" (§2), clamped only where they feed the panel's own arithmetic
/// (see [`TextSize::current_s`]).
fn clamp_round_s(target: f32) -> f32 {
    round2(target.clamp(ZOOM_MIN, ZOOM_MAX))
}

/// Whether `a` and `b` are the same value once both go through the same two-decimal rounding every
/// write already does -- tolerating whatever float noise nvim's own handling might introduce well
/// under that granularity (and the f32(shell) -> f64(nvim) -> f32(back) round trip is in fact
/// lossless regardless -- see `the_echo_comparison_survives_the_f32_f64_f32_round_trip`).
fn is_our_echo(a: f32, b: f32) -> bool {
    round2(a) == round2(b)
}

/// Pushes `v` onto `burst`, dropping the oldest entry first if it is already at [`BURST_CAP`].
fn push_burst(burst: &mut Vec<f32>, v: f32) {
    if burst.len() >= BURST_CAP {
        burst.remove(0);
    }
    burst.push(v);
}

/// One row of the `Ctrl+a =`/`-`/`0` table (spec §3) -- what a plain key after the prefix means,
/// before `main.rs` decides which pane it applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextStep {
    Larger,
    Smaller,
    Reset,
}

impl From<neovibe_core::keymap::TextChange> for TextStep {
    fn from(change: neovibe_core::keymap::TextChange) -> Self {
        match change {
            neovibe_core::keymap::TextChange::Larger => TextStep::Larger,
            neovibe_core::keymap::TextChange::Smaller => TextStep::Smaller,
            neovibe_core::keymap::TextChange::Reset => TextStep::Reset,
        }
    }
}

/// S, R, a pending write and the burst it belongs to, and the panel's base size. See this module's
/// own doc for what each means; every operation here is a pure function of the state alone -- there
/// is no notion of time anywhere in this type (§4, corrected again 2026-09-23).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TextSize {
    /// `g:neovide_scale_factor` as nvim last actually reported it -- authoritative for the editor.
    /// Starts at the pane's `scale_factor_setting()` read at startup (§4), never this module's own
    /// 1.0 assumption, so a value the owner's `init.lua` set is respected from the first frame.
    reported_s: f32,
    /// The panel's factor relative to `reported_s` (or the outstanding pending, if any -- see
    /// [`current_s`](Self::current_s)). 1.0 keeps the panel in step with the editor, which is the
    /// default for every writer of S except the panel-focused prefix keys.
    r: f32,
    /// A write this window made that nvim has not yet echoed back. See this module's own §4 doc
    /// for exactly when this is set, cleared, or left alone.
    pending: Option<f32>,
    /// Every value written since `pending` was last `None`, capped at [`BURST_CAP`] -- see this
    /// module's own §4 doc.
    burst: Vec<f32>,
    /// `agent.font_size`, or the shipped default -- the panel's un-zoomed size.
    base: f32,
}

impl TextSize {
    /// The starting state for a window: R untouched (1.0), no write outstanding, S seeded from
    /// `reported_s` -- the pane's own `scale_factor_setting()` at startup, or `1.0` with no editor
    /// (unreachable from `main.rs` since the modules design's P1; §4: "starting at the pane's
    /// `scale_factor_setting()` at startup").
    pub(crate) fn new(base: f32, reported_s: f32) -> Self {
        TextSize {
            reported_s,
            r: 1.0,
            pending: None,
            burst: Vec::new(),
            base,
        }
    }

    /// The value every operation below steps from: the outstanding pending write, or `reported_s`
    /// if none -- substituted with 1.0 when that is not finite and positive (§2: "A non-finite or
    /// non-positive S is treated as 1.0 for the panel's arithmetic"). An operation stepping off a
    /// garbage report would otherwise propagate it (`NaN + 0.1` is still `NaN`) rather than
    /// recovering from it the moment the owner next touches a text-size key. No deadline gates
    /// `pending` here any more (§4, corrected again 2026-09-23): it is trusted until a report says
    /// otherwise, however long that takes.
    fn current_s(&self) -> f32 {
        let raw = self.pending.unwrap_or(self.reported_s);
        if raw.is_finite() && raw > 0.0 {
            raw
        } else {
            1.0
        }
    }

    /// `P = S × R`, clamped (§2) -- the panel's effective scale before it is multiplied by `base`.
    pub(crate) fn panel_scale(&self) -> f32 {
        (self.current_s() * self.r).clamp(ZOOM_MIN, ZOOM_MAX)
    }

    /// `base × panel_scale()` -- what `shell` hands the agent panel as `font_size_px`.
    pub(crate) fn panel_px(&self) -> f32 {
        self.base * self.panel_scale()
    }

    // --- Both panes (app-level accelerators; spec §3's first table). R is never touched here. ---

    /// `app.text-larger`: `S' = clamp(S + 0.1)`; R unchanged.
    pub(crate) fn both_larger(self) -> (Self, Option<f32>) {
        let s = clamp_round_s(self.current_s() + ZOOM_STEP);
        let r = self.r;
        self.write_with_r(s, r)
    }

    /// `app.text-smaller`: `S' = clamp(S - 0.1)`; R unchanged.
    pub(crate) fn both_smaller(self) -> (Self, Option<f32>) {
        let s = clamp_round_s(self.current_s() - ZOOM_STEP);
        let r = self.r;
        self.write_with_r(s, r)
    }

    /// `app.text-reset`: `S' = 1.0` and `R' = 1.0` -- also the universal heal for a `pending` stuck
    /// on a write nvim never answered (§4's "corrected again" section): if `reported_s` is already
    /// `1.0`, `write`'s rule 2 fires regardless of what `pending` currently holds.
    pub(crate) fn both_reset(self) -> (Self, Option<f32>) {
        self.write_with_r(clamp_round_s(1.0), 1.0)
    }

    pub(crate) fn apply_both_step(self, step: TextStep) -> (Self, Option<f32>) {
        match step {
            TextStep::Larger => self.both_larger(),
            TextStep::Smaller => self.both_smaller(),
            TextStep::Reset => self.both_reset(),
        }
    }

    // --- One pane, editor focused (`Ctrl+a =`/`-`/`0`; spec §3's second table, right column).
    // Each writes a fresh S and adjusts R so the PANEL's px does not move. ---

    /// `S' = clamp(S + 0.1)`; `R' = R·S/S'` (panel px unchanged).
    pub(crate) fn editor_larger(self) -> (Self, Option<f32>) {
        let target = self.current_s() + ZOOM_STEP;
        self.write_s_keeping_panel_px(target)
    }

    /// `S' = clamp(S - 0.1)`; `R' = R·S/S'` (panel px unchanged).
    pub(crate) fn editor_smaller(self) -> (Self, Option<f32>) {
        let target = self.current_s() - ZOOM_STEP;
        self.write_s_keeping_panel_px(target)
    }

    /// `S' = 1.0`; `R' = R·S/S'` (panel px unchanged).
    pub(crate) fn editor_reset(self) -> (Self, Option<f32>) {
        self.write_s_keeping_panel_px(1.0)
    }

    pub(crate) fn apply_editor_step(self, step: TextStep) -> (Self, Option<f32>) {
        match step {
            TextStep::Larger => self.editor_larger(),
            TextStep::Smaller => self.editor_smaller(),
            TextStep::Reset => self.editor_reset(),
        }
    }

    // --- One pane, panel focused (`Ctrl+a =`/`-`/`0`; spec §3's second table, middle column).
    // None of these write S at all -- only R moves. ---

    /// `P' = clamp(P + 0.1)`; `R' = P'/S`.
    pub(crate) fn panel_larger(self) -> Self {
        let target = self.panel_scale() + ZOOM_STEP;
        self.set_panel_scale(target)
    }

    /// `P' = clamp(P - 0.1)`; `R' = P'/S`.
    pub(crate) fn panel_smaller(self) -> Self {
        let target = self.panel_scale() - ZOOM_STEP;
        self.set_panel_scale(target)
    }

    /// `R' = 1` -- the panel back in step with the editor.
    pub(crate) fn panel_reset(self) -> Self {
        TextSize { r: 1.0, ..self }
    }

    pub(crate) fn apply_panel_step(self, step: TextStep) -> Self {
        match step {
            TextStep::Larger => self.panel_larger(),
            TextStep::Smaller => self.panel_smaller(),
            TextStep::Reset => self.panel_reset(),
        }
    }

    // --- What nvim reports (§4). ---

    /// nvim reported a new `g:neovide_scale_factor` -- our own write echoing back, an echo of an
    /// OLDER write of ours still inside the current burst, or a real external change (the owner's
    /// own mapping, a typed `:let`). `reported_s` follows the report unconditionally -- nvim's
    /// report is authoritative for the editor regardless of what this window happens to be waiting
    /// for. `r` is untouched in every case -- for our own echo it was already set correctly at
    /// write time, and for an external change "together" is the default for every writer, not just
    /// this window's own keys.
    pub(crate) fn nvim_reported_s(self, reported: f32) -> Self {
        match self.pending {
            Some(p) if is_our_echo(p, reported) => TextSize {
                reported_s: reported,
                pending: None,
                burst: Vec::new(),
                ..self
            },
            Some(_) if self.burst.iter().any(|b| is_our_echo(*b, reported)) => TextSize {
                reported_s: reported,
                ..self
            },
            _ => TextSize {
                reported_s: reported,
                pending: None,
                burst: Vec::new(),
                ..self
            },
        }
    }

    /// The shared decision behind every value this window writes to S (§4): `s` must already be
    /// rounded/clamped (every call site here routes through [`clamp_round_s`] first).
    ///
    /// 1. If `s` already equals the outstanding `pending`, or `reported_s` when there is none
    ///    (the raw value, not [`current_s`](Self::current_s)'s 1.0 substitute), nothing is written
    ///    and nothing changes -- nvim's watcher would see no change either, so nothing would ever echo it (the
    ///    same "never write an equal value" rule `window_mode::setting_to_write` already uses).
    /// 2. Else if `s` equals `reported_s`, the write is still sent (nvim's live value may currently
    ///    be the outstanding `pending`, which really does need correcting) -- but `pending`/`burst`
    ///    are cleared regardless of what nvim does or does not echo back for it, because the moment
    ///    `reported_s` itself becomes `s` the state is already right.
    /// 3. Otherwise a write is genuinely in flight: send it, and remember it (`pending`, `burst`).
    fn write(self, s: f32) -> (Self, Option<f32>) {
        // Rule 1 compares with the RAW value, not `current_s()`: that substitutes 1.0 for a
        // non-finite or non-positive S, so `Ctrl+0` after `:let g:neovide_scale_factor = 0` would
        // compare 1.0 with 1.0, write nothing, and leave the editor at the bad scale.
        let raw = self.pending.unwrap_or(self.reported_s);
        if is_our_echo(s, raw) {
            return (self, None);
        }
        if is_our_echo(s, self.reported_s) {
            return (
                TextSize {
                    pending: None,
                    burst: Vec::new(),
                    ..self
                },
                Some(s),
            );
        }
        let mut burst = self.burst;
        push_burst(&mut burst, s);
        (
            TextSize {
                pending: Some(s),
                burst,
                ..self
            },
            Some(s),
        )
    }

    /// [`write`](Self::write), also setting `r` to a value the caller already computed (used by
    /// the "both panes" operations, which never adjust R, and by `write_s_keeping_panel_px`, which
    /// does).
    fn write_with_r(self, s: f32, r: f32) -> (Self, Option<f32>) {
        TextSize { r, ..self }.write(s)
    }

    /// Writes a fresh S, adjusting R by the ratio the write moved S so `panel_scale()` (and so
    /// `panel_px()`) comes out unchanged -- see `editor_larger`/`editor_smaller`/`editor_reset`.
    fn write_s_keeping_panel_px(self, target: f32) -> (Self, Option<f32>) {
        let old_s = self.current_s();
        let s = clamp_round_s(target);
        let r = self.r * old_s / s;
        self.write_with_r(s, r)
    }

    /// `R' = clamp(target)/S`, the shared step behind `panel_larger`/`panel_smaller`. Clamps
    /// `target` itself (not just what `panel_scale()` later reports) so `r` cannot run away
    /// unboundedly under repeated presses even though `panel_scale()` would mask that by clamping
    /// its own output regardless -- see `panel_larger_clamps_the_raw_r_too_not_just_panel_scales_own_clamp`.
    fn set_panel_scale(self, target: f32) -> Self {
        let r = target.clamp(ZOOM_MIN, ZOOM_MAX) / self.current_s();
        TextSize { r, ..self }
    }
}

/// One pane, decided by which module holds the keys (`Ctrl+a =`/`-`/`0`) -- pure, so `main.rs`'s own
/// prefix routing has exactly one place to get right rather than re-deriving "the editor, the panel,
/// anything else a no-op" inline at the call site, where a review's mutation testing found a swap of
/// the two arms went unnoticed (item 3g). Until the modules design's P1 that rule was written in pane
/// indices: pane 0 the editor, pane 1 the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextSizeTarget {
    Editor,
    Panel,
    Neither,
}

/// `focused`: [`crate::pane_focus::focused_module`]'s own answer -- the module that HOLDS THE KEYS
/// right now, not the remembered owner. Any other focus (the top bar, a Lua panel, the bottom
/// terminal) is [`TextSizeTarget::Neither`] (spec §3: "with any other focus these are no-ops"). The
/// terminal's face is fixed until its phase 4 takes nvim's `guifont` and joins `Ctrl+=`
/// (bottom-terminal spec §2.7), as on `main`, where its pane index 2 was `Neither` too.
///
/// Keyed by the module's kind since the modules design's P1. It used to take a pane index plus
/// `editor_is_main`/`side_is_agent` to say whether slot 0/1 really held the editor/panel; a
/// `ModuleId` says what it is, so a Lua panel -- even one whose own id is `"editor"` -- can never be
/// routed as the editor.
pub(crate) fn route_focused_module(focused: Option<&ModuleId>) -> TextSizeTarget {
    match focused.map(ModuleId::kind) {
        Some(ModuleKind::Editor) => TextSizeTarget::Editor,
        Some(ModuleKind::Agent) => TextSizeTarget::Panel,
        _ => TextSizeTarget::Neither,
    }
}

/// What the colorscheme listener's own `panel_tokens` closure (`main.rs`) must read for the
/// panel's font size: the LIVE value the zoom controls have pushed, never the startup base
/// captured once at launch -- a colorscheme change must re-derive every OTHER token but never
/// reset a zoom. A pure function so that closure has nothing left to get wrong beyond calling it
/// (item 3g: an earlier revision of `main.rs` read the captured startup base here instead).
pub(crate) fn live_panel_font_size_px(panel_px: &Cell<f32>) -> f32 {
    panel_px.get()
}

/// Every `app.text-*` action this module registers: its GTK action name and which [`TextStep`] it
/// performs -- one table `TextSizeController::install` iterates, so a name and a step cannot drift
/// apart (item 3g). The accelerators are `neovibe_core::keymap::root`'s (keymap spec §2.2): `Ctrl+=`,
/// `Ctrl+-`, `Ctrl+0` and their keypad forms. `<Control>plus` is gone -- on a US layout it is
/// `Ctrl+Shift+=`, and no root chord holds `Ctrl+Shift`.
pub(crate) const TEXT_SIZE_ACTIONS: &[(&str, TextStep)] = &[
    ("text-larger", TextStep::Larger),
    ("text-smaller", TextStep::Smaller),
    ("text-reset", TextStep::Reset),
];

/// Owns the live [`TextSize`] for one window: the three `app.text-*` accelerators (registered
/// exactly like `window_mode::WindowModes` registers F11) and following `g:neovide_scale_factor`
/// back from nvim. `main.rs`'s `Ctrl+a` prefix routing calls [`apply_editor`](Self::apply_editor)/
/// [`apply_panel`](Self::apply_panel) directly rather than going through an action, since those
/// two are one-pane operations the prefix has already resolved a target for (via
/// [`route_focused_module`]).
pub(crate) struct TextSizeController {
    /// A `RefCell`, not a `Cell` (2026-09-23): `TextSize` holds a `Vec` (`burst`) and so is no
    /// longer `Copy`.
    state: RefCell<TextSize>,
    /// `None` when there is no nvim to write `g:neovide_scale_factor` to: `apply_editor` logs and
    /// does nothing; `apply_both` degrades to scaling the panel alone rather than doing nothing at
    /// all (spec §2's note (a), 2026-09-23 -- "together" means "every pane that exists", and with no
    /// editor that is just the panel). That was a Lua plugin in the main slot until the modules
    /// design's P1; every window has the editor since (a Lua `main` panel hides it), so `main.rs`
    /// passes `Some`, and a write before nvim has started is buffered by the pane.
    editor: Option<Rc<NeovideEditorPane>>,
    panel: AgentPanelHandle,
    /// The panel's live px, shared with `main.rs`'s colorscheme listener (`panel_tokens`, via
    /// [`live_panel_font_size_px`]) so a colorscheme change re-derives every other token but never
    /// resets a zoom. This struct writes it; `main.rs` only ever reads it.
    panel_px: Rc<Cell<f32>>,
}

impl TextSizeController {
    pub(crate) fn install(
        app: &gtk4::Application,
        editor: Option<Rc<NeovideEditorPane>>,
        panel: AgentPanelHandle,
        base: f32,
        panel_px: Rc<Cell<f32>>,
    ) -> Rc<Self> {
        // §4: seed `reported_s` from the pane's own live value (its `1.0` default before nvim
        // exists is exactly `NeovideEditorPane::scale_factor_setting`'s own documented fallback),
        // not this module's own separate assumption -- a value the owner's `init.lua` already set
        // before this controller was constructed must not be forgotten.
        let reported_s = editor.as_ref().map(|e| e.scale_factor_setting()).unwrap_or(1.0);
        let this = Rc::new(TextSizeController {
            state: RefCell::new(TextSize::new(base, reported_s)),
            editor,
            panel,
            panel_px,
        });
        this.push_panel_px();

        // App-level, so GTK takes them before nvim or the panel sees the key (spec §3; see
        // `TEXT_SIZE_ACTIONS`'s own doc for why each accelerator is bound).
        for (name, step) in TEXT_SIZE_ACTIONS.iter().copied() {
            let action = gtk4::gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(&this);
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    this.apply_both(step);
                }
            });
            app.add_action(&action);
            app.set_accels_for_action(&format!("app.{name}"), neovibe_core::keymap::root::accels(name));
        }

        if let Some(editor) = &this.editor {
            let weak = Rc::downgrade(&this);
            // known limit (item 3g, 2026-09-23): `nvim_reported_s` itself is unit-tested
            // extensively above, but this closure -- calling it and re-pushing the panel px -- is
            // GTK wiring reached only from `neovide-editor`'s tick callback, with no headless
            // harness. A future edit dropping the state-update line, or the `push_panel_px()`
            // call, compiles and passes every test here; only a GUI pass (change S externally,
            // e.g. `:let`, and watch the panel follow) would catch it.
            editor.on_scale_factor_setting(move |value| {
                let Some(this) = weak.upgrade() else { return };
                let next = this.state.borrow().clone().nvim_reported_s(value);
                *this.state.borrow_mut() = next;
                this.push_panel_px();
            });
        }

        this
    }

    /// `app.text-larger`/`-smaller`/`-reset`: both panes move together, or -- with no editor, which
    /// `main.rs` never passes since the modules design's P1 -- the panel alone (spec §2's note (a),
    /// 2026-09-23).
    pub(crate) fn apply_both(self: &Rc<Self>, step: TextStep) {
        let Some(editor) = &self.editor else {
            eprintln!("[text-size] {step:?} (both panes): no editor pane, scaling the panel alone");
            let next = self.state.borrow().clone().apply_panel_step(step);
            *self.state.borrow_mut() = next;
            self.push_panel_px();
            let state = self.state.borrow();
            eprintln!(
                "[text-size] panel-alone {:.2} ({:.1}px)",
                state.panel_scale(),
                state.panel_px()
            );
            return;
        };
        let (next, write) = self.state.borrow().clone().apply_both_step(step);
        *self.state.borrow_mut() = next;
        if let Some(write) = write {
            editor.set_scale_factor_setting(write);
        }
        self.push_panel_px();
        let state = self.state.borrow();
        eprintln!(
            "[text-size] both {} panel {:.2} ({:.1}px)",
            describe_write(write),
            state.panel_scale(),
            state.panel_px()
        );
    }

    /// The keymap's `text.larger`/`text.smaller`/`text.reset` (not bound by default; `Ctrl`+wheel
    /// over a pane is the default per-pane route, Task 9) with the editor focused.
    pub(crate) fn apply_editor(self: &Rc<Self>, step: TextStep) {
        let Some(editor) = &self.editor else {
            eprintln!("[text-size] {step:?} (editor): no editor pane, ignoring");
            return;
        };
        let (next, write) = self.state.borrow().clone().apply_editor_step(step);
        *self.state.borrow_mut() = next;
        if let Some(write) = write {
            editor.set_scale_factor_setting(write);
        }
        self.push_panel_px();
        let state = self.state.borrow();
        eprintln!(
            "[text-size] editor {} panel {:.2} ({:.1}px)",
            describe_write(write),
            state.panel_scale(),
            state.panel_px()
        );
    }

    /// The keymap's `text.larger`/`text.smaller`/`text.reset` with the panel focused. Never touches
    /// nvim: only R moves.
    pub(crate) fn apply_panel(&self, step: TextStep) {
        let next = self.state.borrow().clone().apply_panel_step(step);
        *self.state.borrow_mut() = next;
        self.push_panel_px();
        let state = self.state.borrow();
        eprintln!(
            "[text-size] panel {:.2} ({:.1}px)",
            state.panel_scale(),
            state.panel_px()
        );
    }

    fn push_panel_px(&self) {
        let px = self.state.borrow().panel_px();
        self.panel_px.set(px);
        self.panel.set_panel_font_size_px(px);
    }
}

/// Formats a write for the `[text-size]` log lines above -- `None` (§4's true no-op, rule 1) reads
/// as "(unchanged)" rather than omitting the write entirely, so a log reader can tell "nothing
/// changed" apart from a missing line.
fn describe_write(write: Option<f32>) -> String {
    match write {
        Some(v) => format!("{v:.2}"),
        None => "(unchanged)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_keymap_text_change_is_the_same_step() {
        use neovibe_core::keymap::TextChange;
        assert_eq!(TextStep::from(TextChange::Larger), TextStep::Larger);
        assert_eq!(TextStep::from(TextChange::Smaller), TextStep::Smaller);
        assert_eq!(TextStep::from(TextChange::Reset), TextStep::Reset);
    }

    fn ts(s: f32, r: f32, base: f32) -> TextSize {
        TextSize {
            reported_s: s,
            r,
            pending: None,
            burst: Vec::new(),
            base,
        }
    }

    // --- §3's first table: both panes together. ---

    /// `app.text-larger` from the middle of the range: S steps by 0.1, R is untouched.
    #[test]
    fn both_larger_steps_s_and_leaves_r() {
        let (next, write) = ts(1.2, 0.8, 14.0).both_larger();
        assert_eq!(write, Some(1.3));
        assert_eq!(
            next.reported_s, 1.2,
            "reported_s only moves on a report, not on our own write"
        );
        assert_eq!(next.pending, Some(1.3));
        assert_eq!(next.r, 0.8);
    }

    #[test]
    fn both_smaller_steps_s_down_and_leaves_r() {
        let (next, write) = ts(1.2, 0.8, 14.0).both_smaller();
        assert_eq!(write, Some(1.1));
        assert_eq!(next.r, 0.8);
    }

    #[test]
    fn both_reset_sets_s_and_r_to_one() {
        let (next, write) = ts(2.4, 0.6, 14.0).both_reset();
        assert_eq!(write, Some(1.0));
        assert_eq!(next.pending, Some(1.0));
        assert_eq!(next.r, 1.0);
    }

    #[test]
    fn both_larger_clamps_at_the_top() {
        let (next, write) = ts(ZOOM_MAX, 1.0, 14.0).both_larger();
        assert_eq!(
            write, None,
            "already at the ceiling: clamped target equals current_s, a true no-op"
        );
        assert_eq!(next.current_s(), ZOOM_MAX);
    }

    #[test]
    fn both_smaller_clamps_at_the_bottom() {
        let (next, write) = ts(ZOOM_MIN, 1.0, 14.0).both_smaller();
        assert_eq!(write, None);
        assert_eq!(next.current_s(), ZOOM_MIN);
    }

    /// The rounding this module exists to guarantee: ten `Ctrl+=` steps from 1.0 land on exactly
    /// 2.0, not 1.9999999 or 2.0000002 -- each step rounds off the previous step's own result
    /// rather than accumulating `+= 0.1` error across all ten. Each step's write is echoed back
    /// immediately (as it would be in practice) so `current_s` steps from a real report each time.
    #[test]
    fn ten_larger_steps_from_one_land_on_exactly_two() {
        let mut state = TextSize::new(14.0, 1.0);
        for _ in 0..10 {
            let (next, write) = state.both_larger();
            state = next.nvim_reported_s(write.expect("every step in this run changes S"));
        }
        assert_eq!(state.reported_s, 2.0);
    }

    // --- §3's second table: one pane, panel focused. Never writes S. ---

    #[test]
    fn panel_larger_moves_r_and_leaves_s() {
        // S=1.0, R=1.0 -> P=1.0. panel_larger asks for P=1.1, so R'=1.1/1.0=1.1.
        let next = ts(1.0, 1.0, 14.0).panel_larger();
        assert_eq!(next.reported_s, 1.0);
        assert!((next.r - 1.1).abs() < 1e-6);
        assert!((next.panel_scale() - 1.1).abs() < 1e-6);
    }

    #[test]
    fn panel_smaller_moves_r_down() {
        let next = ts(1.0, 1.0, 14.0).panel_smaller();
        assert!((next.r - 0.9).abs() < 1e-6);
    }

    /// §3a: `R' = P'/S` -- at a non-unit S, dividing by 1.0 instead of the real S (the mutation a
    /// review found this file's tests could not catch) would give a visibly different, wrong `r`.
    /// S=1.5: `Ctrl+a =` with the panel focused takes P 1.5 -> 1.6, so `r` must be `1.6/1.5`, not
    /// `1.6` itself (which is what dropping the `/ current_s` division would produce -- that would
    /// leave P at `1.6 * 1.5 = 2.4`, not 1.6).
    #[test]
    fn panel_larger_at_a_nonunit_s_divides_by_the_real_s_not_by_one() {
        let next = ts(1.5, 1.0, 14.0).panel_larger();
        assert!((next.panel_scale() - 1.6).abs() < 1e-4, "P must land on 1.6");
        assert!(
            (next.r - (1.6 / 1.5)).abs() < 1e-4,
            "R must be P/S ({}), not raw P (1.6)",
            1.6 / 1.5
        );
        assert!(
            (next.r - 1.6).abs() > 1e-3,
            "dropping the /S division would leave R=1.6, which this must not equal"
        );
    }

    #[test]
    fn panel_larger_clamps_p_at_the_top() {
        // S=2.0, R=1.5 -> P=clamp(3.0)=3.0 already. Another larger step must not push R past
        // what keeps P at the ceiling.
        let next = ts(2.0, 1.5, 14.0).panel_larger();
        assert_eq!(next.panel_scale(), ZOOM_MAX);
    }

    /// §3b (the `set_panel_scale` half): repeatedly zooming the panel must not let the raw `r`
    /// run away past `ZOOM_MAX` even though `panel_scale()`'s OWN clamp would mask that from any
    /// test that only ever reads `panel_scale()` -- this reads `r` directly, which only
    /// `set_panel_scale`'s own internal clamp on `target` can keep bounded.
    #[test]
    fn panel_larger_clamps_the_raw_r_too_not_just_panel_scales_own_clamp() {
        let mut state = ts(1.0, 1.0, 14.0);
        for _ in 0..30 {
            state = state.panel_larger();
        }
        assert_eq!(state.panel_scale(), ZOOM_MAX);
        assert!(
            state.r <= ZOOM_MAX + 1e-6,
            "set_panel_scale's own clamp must cap r itself (got {}), not just panel_scale()'s output",
            state.r
        );
    }

    #[test]
    fn panel_reset_sets_r_to_one_and_leaves_s() {
        let next = ts(1.4, 0.3, 14.0).panel_reset();
        assert_eq!(next.r, 1.0);
        assert_eq!(next.reported_s, 1.4);
    }

    // --- §3's second table: one pane, editor focused. Writes S; R compensates. ---

    #[test]
    fn editor_larger_steps_s_and_keeps_panel_px() {
        let before = ts(1.0, 1.2, 14.0);
        let before_px = before.panel_px();
        let (next, write) = before.editor_larger();
        assert_eq!(write, Some(1.1));
        assert_eq!(next.pending, Some(1.1));
        assert!((next.panel_px() - before_px).abs() < 1e-3, "panel px must not move");
    }

    #[test]
    fn editor_smaller_steps_s_down_and_keeps_panel_px() {
        let before = ts(1.5, 0.7, 14.0);
        let before_px = before.panel_px();
        let (next, write) = before.editor_smaller();
        assert_eq!(write, Some(1.4));
        assert!((next.panel_px() - before_px).abs() < 1e-3);
    }

    #[test]
    fn editor_reset_sets_s_to_one_and_keeps_panel_px() {
        let before = ts(2.0, 0.5, 14.0);
        let before_px = before.panel_px();
        let (next, write) = before.editor_reset();
        assert_eq!(write, Some(1.0));
        assert_eq!(next.pending, Some(1.0));
        assert!((next.panel_px() - before_px).abs() < 1e-3);
    }

    /// An editor-only step must leave `panel_px` alone regardless of which of the three it is --
    /// the property `editor_larger`/`editor_smaller`/`editor_reset` each individually claim above,
    /// checked once more together so a future fourth editor operation is reminded to hold it too.
    #[test]
    fn every_editor_only_step_leaves_panel_px_unchanged() {
        let before = ts(1.3, 0.8, 14.0);
        let before_px = before.panel_px();
        for op in [
            TextSize::editor_larger as fn(TextSize) -> (TextSize, Option<f32>),
            TextSize::editor_smaller,
        ] {
            let (next, _) = op(before.clone());
            assert!((next.panel_px() - before_px).abs() < 1e-3);
        }
        let (next, _) = before.clone().editor_reset();
        assert!((next.panel_px() - before_px).abs() < 1e-3);
    }

    #[test]
    fn editor_larger_clamps_s_at_the_top() {
        let (next, write) = ts(ZOOM_MAX, 1.0, 14.0).editor_larger();
        // Already at the ceiling, clamped to the SAME value already in `reported_s`, with nothing
        // outstanding: a true no-op (rule 1) -- nvim's watcher would see no change either.
        assert_eq!(write, None);
        assert_eq!(next.pending, None);
    }

    // --- §3b (the `panel_scale`/`panel_px` half): an external report still clamps the panel. ---

    #[test]
    fn an_external_report_of_a_huge_s_still_clamps_the_panels_px() {
        // S=5, R=1 unclamped would be P=5, panel px = 14*5 = 70. panel_scale()'s own clamp caps P
        // at ZOOM_MAX (3.0), so px caps at 42, not 70.
        let next = TextSize::new(14.0, 1.0).nvim_reported_s(5.0);
        assert_eq!(next.panel_scale(), ZOOM_MAX);
        assert_eq!(next.panel_px(), 14.0 * ZOOM_MAX);
        assert_ne!(next.panel_px(), 70.0);
    }

    #[test]
    fn an_external_report_of_a_tiny_s_still_clamps_the_panels_px() {
        let next = TextSize::new(14.0, 1.0).nvim_reported_s(0.1);
        assert_eq!(next.panel_scale(), ZOOM_MIN);
        assert_eq!(next.panel_px(), 14.0 * ZOOM_MIN);
    }

    // --- §4: what nvim reports, and the corrected no-time model. ---

    #[test]
    fn an_external_let_updates_s_and_leaves_r_unchanged() {
        // Nobody here wrote anything (`pending` is `None`): a real `:let` or the owner's own
        // mapping. "Together" is the default for every writer, so R does not move.
        let next = ts(1.0, 1.3, 14.0).nvim_reported_s(2.0);
        assert_eq!(next.reported_s, 2.0);
        assert_eq!(next.r, 1.3);
    }

    #[test]
    fn our_own_write_is_recognised_as_an_echo_and_clears_pending() {
        let (written, write) = ts(1.0, 1.0, 14.0).both_larger();
        let write = write.expect("this write changes S");
        assert_eq!(written.pending, Some(write));
        let echoed = written.nvim_reported_s(write);
        assert_eq!(echoed.pending, None);
        assert_eq!(echoed.reported_s, write);
    }

    /// The echo the design's §4 exists for: an editor-only step writes S optimistically and sets
    /// `pending`; a colorscheme recompute (or anything else that reads `panel_px()`) squeezed in
    /// BEFORE the echo arrives must see the same, unchanged value -- not the new S multiplied by
    /// the old R, which is what a model with no `pending` slot would compute.
    #[test]
    fn a_recompute_before_the_echo_arrives_does_not_change_panel_px() {
        let before = ts(1.0, 1.2, 14.0);
        let before_px = before.panel_px();
        let (after_write, write) = before.clone().editor_larger();
        let write = write.expect("this write changes S");
        assert_ne!(write, before.reported_s, "the write really did move s");
        // The "colorscheme change" stand-in: read panel_px() again, more than once, before nvim's
        // watcher has reported anything back.
        assert!((after_write.panel_px() - before_px).abs() < 1e-3);
        assert!((after_write.panel_px() - before_px).abs() < 1e-3);
        // The echo now arrives. panel_px must still not have moved.
        let after_echo = after_write.nvim_reported_s(write);
        assert_eq!(after_echo.pending, None);
        assert!((after_echo.panel_px() - before_px).abs() < 1e-3);
    }

    #[test]
    fn a_nonfinite_or_nonpositive_s_falls_back_to_one_for_panel_arithmetic() {
        let baseline = ts(1.0, 1.0, 14.0).panel_px();
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -1.0] {
            let reported = ts(1.0, 1.0, 14.0).nvim_reported_s(bad);
            assert_eq!(
                reported.panel_px(),
                baseline,
                "{bad} must fall back to 1.0 for the panel's own arithmetic"
            );
        }
    }

    /// The float round-trip hazard: S travels shell(f32) -> nvim(f64) -> Neovide's settings(f32).
    /// That specific trip is lossless (f64 exactly represents any f32, and rounding back to f32
    /// recovers the same bits), verified here so the comparison below is not accidentally standing
    /// in for a mistake elsewhere -- but `is_our_echo` is written to tolerate the trip regardless,
    /// via the same two-decimal rounding every write already goes through, rather than relying on
    /// this losslessness holding forever.
    #[test]
    fn the_echo_comparison_survives_the_f32_f64_f32_round_trip() {
        for value in [1.1_f32, 0.7_f32, 1.3_f32] {
            let round_tripped = (value as f64) as f32;
            assert_eq!(
                value.to_bits(),
                round_tripped.to_bits(),
                "the f64 round trip is lossless"
            );
            assert!(is_our_echo(value, round_tripped));
        }
        // Noise well under the write granularity (e.g. nvim's own float formatting) is tolerated.
        assert!(is_our_echo(1.1_f32, 1.1004_f32));
        // A value that rounds to a genuinely different two decimals is not our echo.
        assert!(!is_our_echo(1.1_f32, 1.3_f32));
    }

    // --- The blocking defect (found 2026-09-23), its first (TTL) fix, and the adversarial recheck
    // that found the TTL itself wrong (also 2026-09-23) -- tests below are named after the
    // recheck's own scenario ids so they can be traced back to it directly. ---

    /// §4 rule 1: a write whose value already equals `current_s()` is a true no-op -- nothing is
    /// written to nvim at all, because nvim's watcher would see no change and so would never echo
    /// it (mirrors `window_mode::setting_to_write`'s own "never write an equal value" rule).
    #[test]
    fn a_write_equal_to_current_s_is_a_true_no_op_nothing_sent() {
        // reported_s is already 1.0, nothing pending: both_reset's target is 1.0.
        let (next, write) = TextSize::new(14.0, 1.0).both_reset();
        assert_eq!(write, None, "current_s() is already 1.0 -- nothing to send");
        assert_eq!(next.pending, None);
    }

    /// The exact scenario the blocking defect description reproduces: `both_reset()` at S already
    /// 1.0 (a true no-op -- rule 1), then an external `:let` to 2.0 (the panel must follow to 2.0,
    /// not stay stuck), then the next `Ctrl+=` must GROW from the real current 2.0 to 2.1, never
    /// shrink to 1.1 off a stale pending.
    #[test]
    fn the_reproduced_blocking_scenario_the_new_model_does_not_get_stuck() {
        let (after_reset, write) = TextSize::new(14.0, 1.0).both_reset();
        assert_eq!(write, None);
        assert_eq!(after_reset.pending, None);

        let after_external = after_reset.nvim_reported_s(2.0);
        assert_eq!(after_external.pending, None);
        assert_eq!(after_external.reported_s, 2.0);
        assert_eq!(
            after_external.panel_px(),
            14.0 * 2.0,
            "the panel must follow the external change to 2.0, not stay stuck at the old value"
        );

        let (after_plus, write2) = after_external.both_larger();
        assert_eq!(write2, Some(2.1));
        assert!(
            write2.unwrap() > 2.0,
            "must grow from the real current scale (2.0 -> 2.1), not shrink from a stale pending (which would give 1.1)"
        );
        let _ = after_plus;
    }

    /// A panel-only step while an editor-only write is still in flight: the panel steps from what
    /// it SHOWS (`current_s() × R`, with the pending S), not from the stale `reported_s`. Found by
    /// the second recheck's mutation M13 (`set_panel_scale` dividing by `reported_s`), which no
    /// test caught.
    #[test]
    fn a_panel_step_during_a_pending_editor_write_steps_from_what_the_panel_shows() {
        let (state, write) = TextSize::new(14.0, 1.0).editor_larger();
        assert_eq!(write, Some(1.1));
        assert!(
            (state.panel_px() - 14.0).abs() < 1e-3,
            "an editor-only step leaves the panel alone"
        );
        let state = state.panel_larger();
        assert!(
            (state.panel_px() - 15.4).abs() < 1e-3,
            "P + 0.1 from 1.0, got {}",
            state.panel_px()
        );
        let state = state.nvim_reported_s(1.1);
        assert!(
            (state.panel_px() - 15.4).abs() < 1e-3,
            "the echo does not move it, got {}",
            state.panel_px()
        );
    }

    /// An echo of an older write in the burst still moves `reported_s` (the second recheck's
    /// mutation M18 left it behind and every test stayed green).
    #[test]
    fn an_intermediate_echo_still_moves_reported_s() {
        let mut state = TextSize::new(14.0, 1.0);
        for _ in 0..3 {
            state = state.both_larger().0;
        }
        assert_eq!(state.pending, Some(1.3));
        let state = state.nvim_reported_s(1.1);
        assert_eq!(state.reported_s, 1.1);
        assert_eq!(
            state.pending,
            Some(1.3),
            "an older echo leaves the pending write outstanding"
        );
    }

    /// `Ctrl+0` after nvim was handed a scale the panel cannot use: the reset is written, not
    /// swallowed by rule 1 comparing 1.0 with `current_s()`'s own 1.0 substitute.
    #[test]
    fn reset_writes_one_after_a_bad_external_scale() {
        for bad in [0.0_f32, -1.0, f32::NAN] {
            let state = TextSize::new(14.0, 1.0).nvim_reported_s(bad);
            let (_, write) = state.both_reset();
            assert_eq!(write, Some(1.0), "reset after {bad} must write 1.0");
        }
    }

    /// s7 (the recheck's "should-fix" defect): two editor-only presses in a row with NO echo for
    /// either yet. With no TTL to go stale against, the second press must step from the real
    /// outstanding 1.1, writing 1.2 -- not fall back to a stale `reported_s` of 1.0 and repeat 1.1.
    /// Both presses must leave the panel exactly where it started, throughout and after both
    /// echoes eventually arrive.
    #[test]
    fn s7_a_second_editor_only_press_before_any_echo_steps_from_the_real_pending() {
        let start = TextSize::new(14.0, 1.0);
        let panel_start = start.panel_px();

        let (after_first, write1) = start.editor_larger();
        assert_eq!(write1, Some(1.1));
        assert_eq!(
            after_first.panel_px(),
            panel_start,
            "editor-only must never move the panel"
        );

        let (after_second, write2) = after_first.editor_larger();
        assert_eq!(
            write2,
            Some(1.2),
            "must step from the real outstanding 1.1, not a stale fallback to 1.0"
        );
        assert_eq!(
            after_second.panel_px(),
            panel_start,
            "still must not have moved the panel"
        );

        let after_echo_1 = after_second.nvim_reported_s(1.1);
        assert_eq!(
            after_echo_1.pending,
            Some(1.2),
            "an echo of the OLDER write must not clear the newer one"
        );
        assert_eq!(after_echo_1.panel_px(), panel_start);

        let after_echo_2 = after_echo_1.nvim_reported_s(1.2);
        assert_eq!(after_echo_2.pending, None);
        assert_eq!(
            after_echo_2.panel_px(),
            panel_start,
            "panel must still read 14px after both echoes"
        );
    }

    /// s6: a single editor-only press with an echo that (in the old TTL model) would have arrived
    /// after the deadline. There is no deadline any more, so however long nvim takes, the panel
    /// must not move before the echo, and must still be exactly where it started once the echo
    /// does arrive.
    #[test]
    fn s6_an_editor_only_step_with_a_very_late_echo_never_moves_the_panel() {
        let state = ts(1.0, 0.8, 14.0);
        let start_px = state.panel_px();

        let (after_write, write) = state.editor_larger();
        assert_eq!(write, Some(1.1));
        assert_eq!(
            after_write.panel_px(),
            start_px,
            "no time in this model to go stale against"
        );

        let after_echo = after_write.nvim_reported_s(1.1);
        assert_eq!(after_echo.pending, None);
        assert_eq!(after_echo.panel_px(), start_px);
    }

    /// s4/s5: both-panes larger, pressed twice with no echo for either yet. The old TTL model
    /// flashed back to the pre-write size once the deadline passed (s4) and repeated the same
    /// value on a second press after that (s5, "writes 1.1 again, not 1.2"). With no deadline,
    /// the panel must track the pending write for as long as it takes, and a second press must
    /// step from the real outstanding value.
    #[test]
    fn s4_s5_both_panes_with_no_echo_yet_never_flash_back_and_keep_stepping() {
        let state = TextSize::new(14.0, 1.0);
        let (after_first, write1) = state.both_larger();
        assert_eq!(write1, Some(1.1));
        assert_eq!(
            after_first.panel_px(),
            14.0 * 1.1,
            "s4: no TTL to flash back to -- the panel follows the pending write for as long as nvim takes"
        );

        let (after_second, write2) = after_first.both_larger();
        assert_eq!(write2, Some(1.2), "s5: must write 1.2, not repeat 1.1");
        assert_eq!(after_second.panel_px(), 14.0 * 1.2);
    }

    /// s2: a larger/smaller pair inside one tick nets back to the value nvim already holds
    /// (`reported_s`) before either write has echoed -- rule 2 fires on the second write (its
    /// value equals `reported_s`), clearing `pending`/`burst` even though nothing has echoed yet.
    /// An external report that then arrives is followed immediately, and the next press grows
    /// from the real external value rather than shrinking off a stale pending.
    #[test]
    fn s2_a_net_zero_pair_inside_one_tick_then_an_external_change_is_followed() {
        let state = TextSize::new(14.0, 1.0);
        let (after_larger, write1) = state.both_larger(); // 1.0 -> 1.1
        assert_eq!(write1, Some(1.1));
        let (after_smaller, write2) = after_larger.both_smaller(); // 1.1 -> 1.0 == reported_s
        assert_eq!(
            write2,
            Some(1.0),
            "still sent, even though it lands back on the value nvim already holds (rule 2)"
        );
        assert_eq!(
            after_smaller.pending, None,
            "landing back on reported_s clears pending (rule 2)"
        );

        // No report has arrived for either write yet. An external `:let` (or the owner's own
        // mapping) sets S to 2.0.
        let after_external = after_smaller.nvim_reported_s(2.0);
        assert_eq!(after_external.reported_s, 2.0);
        assert_eq!(after_external.pending, None);
        assert_eq!(
            after_external.panel_px(),
            14.0 * 2.0,
            "the panel must follow the external change immediately"
        );

        let (_, write3) = after_external.both_larger();
        assert_eq!(
            write3,
            Some(2.1),
            "must grow from the real 2.0, not shrink off a stale pending"
        );
    }

    /// s2b: an external report of a genuinely new value arrives before our own write's echo. It
    /// must be treated as authoritative and clear our own outstanding pending (rule: an
    /// unrecognised report always clears `pending`), so the next press steps from the real value.
    #[test]
    fn s2b_an_external_report_arriving_before_our_own_echo_clears_pending() {
        let state = TextSize::new(14.0, 1.0);
        let (after_write, write1) = state.both_larger(); // 1.0 -> 1.1
        assert_eq!(write1, Some(1.1));
        assert!(after_write.pending.is_some());

        // nvim reports the EXTERNAL 2.0 before it ever echoes our own 1.1.
        let after_external = after_write.nvim_reported_s(2.0);
        assert_eq!(
            after_external.pending, None,
            "an external report clears our own outstanding pending"
        );
        assert_eq!(after_external.reported_s, 2.0);

        let (_, write2) = after_external.both_larger();
        assert_eq!(write2, Some(2.1));
    }

    /// Intermediate echoes: three quick larger presses (1.1, 1.2, 1.3), all before any echo. Each
    /// press's value is remembered in `burst`, so an echo of an OLDER one (1.1, then 1.2) must not
    /// move the panel backwards or be mistaken for confirmation that nothing is outstanding --
    /// only the echo of the actual pending value (1.3) clears it.
    #[test]
    fn three_quick_larger_presses_the_panel_never_moves_backwards() {
        let state = TextSize::new(14.0, 1.0);
        let (s1, w1) = state.both_larger();
        assert_eq!(w1, Some(1.1));
        let (s2, w2) = s1.both_larger();
        assert_eq!(w2, Some(1.2));
        let (s3, w3) = s2.both_larger();
        assert_eq!(w3, Some(1.3));
        assert_eq!(s3.panel_px(), 14.0 * 1.3);

        let after_1_1 = s3.nvim_reported_s(1.1);
        assert_eq!(
            after_1_1.pending,
            Some(1.3),
            "an echo of the OLDEST write must not clear the real pending"
        );
        assert_eq!(after_1_1.panel_px(), 14.0 * 1.3, "must not move backwards");

        let after_1_2 = after_1_1.nvim_reported_s(1.2);
        assert_eq!(after_1_2.pending, Some(1.3));
        assert_eq!(after_1_2.panel_px(), 14.0 * 1.3);

        let after_1_3 = after_1_2.nvim_reported_s(1.3);
        assert_eq!(
            after_1_3.pending, None,
            "pending clears once the newest write is echoed"
        );
        assert_eq!(after_1_3.panel_px(), 14.0 * 1.3);
    }

    /// The same three presses, but nvim's own per-tick watch coalesces and the 1.2 report is
    /// simply never sent at all -- the 1.3 echo alone must still clear `pending`.
    #[test]
    fn three_quick_larger_presses_survive_a_skipped_intermediate_report() {
        let state = TextSize::new(14.0, 1.0);
        let (s1, _) = state.both_larger();
        let (s2, _) = s1.both_larger();
        let (s3, w3) = s2.both_larger();
        assert_eq!(w3, Some(1.3));

        let after_1_1 = s3.nvim_reported_s(1.1);
        assert_eq!(after_1_1.pending, Some(1.3));
        let after_1_3 = after_1_1.nvim_reported_s(1.3); // 1.2's report never arrives at all
        assert_eq!(after_1_3.pending, None);
        assert_eq!(after_1_3.panel_px(), 14.0 * 1.3);
    }

    /// Editor-only net-zero burst: a larger step then a smaller step within one tick, both before
    /// either has echoed. Each preserves panel px by construction (R compensates), so the panel
    /// never actually needs to move. nvim eventually reports both writes, in the order it received
    /// them, in separate ticks -- the end state must land exactly back where it started (an
    /// unrecognised intermediate report is allowed to read as "external" and briefly show a
    /// different value; only the END state is asserted here).
    #[test]
    fn editor_only_net_zero_burst_ends_where_it_started() {
        let state = ts(1.0, 1.4, 14.0);
        let start_px = state.panel_px();

        let (after_larger, w1) = state.editor_larger(); // 1.0 -> 1.1
        assert_eq!(w1, Some(1.1));
        assert_eq!(after_larger.panel_px(), start_px);

        let (after_smaller, w2) = after_larger.editor_smaller(); // 1.1 -> 1.0 == reported_s
        assert_eq!(w2, Some(1.0));
        assert_eq!(
            after_smaller.pending, None,
            "lands back on reported_s -- rule 2 clears pending"
        );
        assert_eq!(
            after_smaller.panel_px(),
            start_px,
            "R compensated both times: already back at the start"
        );

        let after_1_1 = after_smaller.nvim_reported_s(1.1);
        let after_1_0 = after_1_1.nvim_reported_s(1.0);
        assert_eq!(after_1_0.panel_px(), start_px, "end state must match the start");
    }

    /// The reset heal: a pending write nvim never answers at all (no report, ever -- a rejected
    /// write, or just an unusually slow nvim) is the one case a TTL used to exist to rescue. The
    /// owner already has a key for it: `Ctrl+0` writes `1.0`, and since `reported_s` really is
    /// `1.0` (nvim was never asked to change it), rule 2 fires and heals the stuck pending
    /// regardless of what it was stuck on.
    #[test]
    fn reset_heals_a_pending_stuck_on_a_write_nvim_never_echoed() {
        let state = TextSize::new(14.0, 1.0);
        let (after_write, write1) = state.both_larger();
        assert_eq!(write1, Some(1.1));
        assert_eq!(after_write.pending, Some(1.1));
        assert_eq!(
            after_write.reported_s, 1.0,
            "nvim's last real report is still 1.0 -- it never echoed"
        );

        let (after_reset, write2) = after_write.both_reset();
        assert_eq!(write2, Some(1.0));
        assert_eq!(after_reset.pending, None, "Ctrl+0 heals the stuck pending");
        assert_eq!(after_reset.r, 1.0);
        assert_eq!(after_reset.panel_px(), 14.0, "the panel is back at base");
    }

    // --- item 3f/3g helpers: the action table and the prefix's routing. ---

    #[test]
    fn text_size_actions_map_each_name_to_its_own_step_in_order() {
        assert_eq!(TEXT_SIZE_ACTIONS.len(), 3);
        assert_eq!(TEXT_SIZE_ACTIONS[0].0, "text-larger");
        assert_eq!(TEXT_SIZE_ACTIONS[0].1, TextStep::Larger);
        assert_eq!(TEXT_SIZE_ACTIONS[1].0, "text-smaller");
        assert_eq!(TEXT_SIZE_ACTIONS[1].1, TextStep::Smaller);
        assert_eq!(TEXT_SIZE_ACTIONS[2].0, "text-reset");
        assert_eq!(TEXT_SIZE_ACTIONS[2].1, TextStep::Reset);
        for (name, _) in TEXT_SIZE_ACTIONS.iter().copied() {
            assert!(
                !neovibe_core::keymap::root::accels(name).is_empty(),
                "{name} has no root accelerator"
            );
        }
    }

    #[test]
    fn route_focused_module_sends_the_editor_to_the_editor_and_the_agent_to_the_panel() {
        assert_eq!(route_focused_module(Some(&ModuleId::editor())), TextSizeTarget::Editor);
        assert_eq!(route_focused_module(Some(&ModuleId::agent())), TextSizeTarget::Panel);
    }

    #[test]
    fn route_focused_module_is_neither_off_the_top_bar_or_in_a_lua_panel() {
        assert_eq!(route_focused_module(None), TextSizeTarget::Neither);
        assert_eq!(
            route_focused_module(Some(&ModuleId::lua("notes"))),
            TextSizeTarget::Neither
        );
        // A Lua panel that named itself "editor" is still a Lua panel.
        assert_eq!(
            route_focused_module(Some(&ModuleId::lua("editor"))),
            TextSizeTarget::Neither
        );
        // The bottom terminal's text size is its own until its phase 4 (`main`: pane 2, Neither).
        assert_eq!(
            route_focused_module(Some(&ModuleId::terminal())),
            TextSizeTarget::Neither
        );
    }

    #[test]
    fn live_panel_font_size_px_reads_the_shared_cell_not_a_snapshot() {
        let cell = Cell::new(14.0);
        assert_eq!(live_panel_font_size_px(&cell), 14.0);
        cell.set(22.4);
        assert_eq!(
            live_panel_font_size_px(&cell),
            22.4,
            "must read the CURRENT value, not one captured earlier"
        );
    }
}
