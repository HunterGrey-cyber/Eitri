//! Size every module together, and each one alone.
//!
//! **A** is the app size, and every module's size is A times that module's own offset. The editor's is **E**, and
//! `S = clamp(A × E)` is `g:neovide_scale_factor`, nvim's own variable: the one value any writer (Eitri's keys, the
//! user's mappings, a typed `:let`) goes through. The chat's offset is **R**, held here so it survives a chat reload.
//! `chat px = base × clamp(A × R, ZOOM_MIN, ZOOM_MAX)`, where **base** is `agent.font_size` (or the shipped default);
//! call `P = A × R` the chat's effective scale. Offsets start at 1.0 and are never persisted across restarts. A
//! non-finite or non-positive S counts as 1.0 for that arithmetic, and every value written is clamped to the zoom
//! range and rounded to two decimals.
//!
//! A write of S by anyone else moves A, offsets kept, so every writer moves every module. A pane reset clears that
//! pane's offset, so it always rejoins the others. `Ctrl+0` clears them all and returns A to [`TextSize::configured`],
//! what the user's config left in S at nvim's VimEnter; 1.0 unless the config set it.
//!
//! [`TextSize`] is the pure model: one function per key's row (every module, the editor alone, the
//! chat alone, the terminals alone, a page alone), each returning the new state and, where the row
//! writes S, the value to hand the editor (if any). It holds no toolkit type and no clock. The GTK wiring around it --
//! the three app-level accelerators, nvim's echo of S, the chat's live px and the page
//! views -- is `shell/src/text_size.rs`'s `TextSizeController`; [`route_focused_module`] decides
//! which of them a pane step moves.
//!
//! ## Why the pending model has no clock
//!
//! nvim reports S back only when its per-tick watch sees the variable actually change (the
//! editor's scale watch, by construction: that is what keeps an unrelated colorscheme recompute
//! cheap). So a write of the value nvim already holds produces no report at all, and a write's
//! echo can arrive arbitrarily late (a blocking command, a busy compositor, a loaded machine).
//!
//! A pending write that is cleared only by its own echo sticks forever after a write of an equal
//! value: a later external `:let g:neovide_scale_factor = 2.0` then moves `reported_s`, but the
//! stale pending would keep the chat at the old size, and the next `Ctrl+=` would step off the
//! stale value -- shrinking the editor from 2.0 rather than growing it. A deadline on the pending
//! value does not rescue that either: whenever an echo arrives later than the deadline, the next
//! editor-only step computes its `E' = S'/A` from the stale report, and the wrong offset
//! never corrects itself once the real echoes catch up -- the editor stays out of step for good.
//! Any timeout is a guess at how long nvim takes, and every guess is wrong on some machine on
//! some day.
//!
//! So nothing here measures time. `pending` is cleared in exactly three ways, none of them a
//! clock: nvim's echo of it; a report of a value that is not in the current `burst` at all (an
//! external write, treated as authoritative); or a write of a value equal to `reported_s` (rule 2
//! of the write rules below). An echo of an OLDER write still in `burst` clears nothing -- it only
//! moves `reported_s`. A write nvim never answers at all has one recovery, and it already has a
//! key: `Ctrl+0`/`app.text-reset` writes the configured size, and when that is nvim's last real value the
//! write takes rule 2 and clears the stuck `pending`. See `nvim_reported_s`'s and `write`'s own
//! docs for the rules, and `reset_heals_a_pending_stuck_on_a_write_nvim_never_echoed` for the
//! exact scenario.
//!
//! **Two races this model does not survive, neither reachable by hand.** (1) Two *different* keys
//! inside one frame, the second reversing the first, with nvim applying and a tick reporting
//! between them in one particular order (0.7% of all three-key interleavings, never under
//! autorepeat or a busy-nvim catch-up): `A` and the step count stay wrong until a reset.
//! (2) Another program writing `g:neovide_scale_factor` back to its last-reported value within one
//! tick of our write: the tick sees no change, so `pending` stays stuck until the next key. Both
//! need two writers or two chords inside ~16ms.
//!
//! ### The model
//!
//! - `reported_s`: the last value nvim actually reported -- authoritative for the editor. Starts at
//!   the editor's `scale_factor_setting()` read at startup, not an assumed 1.0.
//! - `pending: Option<f32>`: the last value this window wrote that nvim has not yet echoed back.
//! - `burst: Vec<f32>`, capped at [`BURST_CAP`] (oldest dropped): every value this window has
//!   written since `pending` was last `None`. Lets a report recognise an echo of an EARLIER write in
//!   a fast sequence (not just the latest one) as still "ours", rather than mistaking it for an
//!   external change and clearing a pending write that is still genuinely outstanding.
//! - `current_s() = pending.unwrap_or(reported_s)` (substituted with `1.0` when not finite and
//!   positive) -- what an editor-only step computes its next value from. A pending value is trusted until a
//!   report says otherwise, however long that takes.
//! - `app`: A. Moved by an every-module step at the moment of the step, so the chat, the terminals and the pages
//!   never wait for nvim, and by a report only when the report is an external change.
//! - **A write of `v`** (already rounded to two decimals, as every write here is):
//!   1. If `v` equals the raw current value (`pending`, else `reported_s`), nothing is written and
//!      nothing changes at all -- nvim's watcher would see no change either, so there is nothing
//!      to wait for (the same "never write an equal value" rule the window modes use).
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
//!   - Else, it is an external change (the user's own mapping, a typed `:let`) or simply nothing
//!     we recognise: `pending = None`, `burst` cleared, and `A = r/E`. No offset is touched by a report --
//!     "together" is the default for every writer, not just this window's own keys.
//! - **Every-module steps** (`both_larger`/`-smaller`) move A and write `clamp(A' × E)`. The editor can sit at an end
//!   of the range while A goes on moving, and no offset is lost to the clamp.
//! - **Editor-only steps** (`editor_larger`/`-smaller`) step from `current_s()` at the moment of the step and set
//!   `E' = S'/A`, so A, and with it every other module, does not move. `editor_reset` writes A and sets `E = 1`.
//! - **Chat-only steps** (`panel_larger`/`-smaller`) never write S at all: `P' = clamp(A × R ± 0.1)`, `R' = P'/A`.
//! - **Reset** (`app.text-reset`) clears every offset, sets A to the configured size and writes it through
//!   the same `write` rules above -- which is what makes it a heal, not just a jump to a known
//!   value: if nvim's `reported_s` is already that value (a pending write nvim never answered is stuck on
//!   some OTHER value), rule 2 fires and the stuck `pending` is cleared unconditionally.
//! - **A new base** (`with_base`, the editor's font changed while the chat follows it) keeps A, the offsets
//!   and any outstanding write, so a size in effect stays in effect at the new base.
//!
//! ## Pages
//!
//! A Lua panel's page, including a plugin's page,
//! follows the app size the way the chat does, with an offset of its own:
//! `page zoom = clamp(A × R_page, ZOOM_MIN, ZOOM_MAX)`, handed to WebKit as the view's zoom level
//! (whole-page, so the page's layout scales with its text). `R_page` is 1.0 when the page is first
//! shown, kept per module for the session, never persisted. Every rule above that touches R
//! touches each `R_page` the same way: an every-module step leaves it (the page follows A), `Ctrl+0`
//! sets it to 1.0, an editor-only step leaves it, and the prefix keys with the page holding them step or reset
//! that page's `R_page` alone.
//!
//! ## Terminals
//!
//! Every terminal of the window shares one offset `R_t`, like the chat's R:
//! `terminal zoom = clamp(A × R_t, ZOOM_MIN, ZOOM_MAX)`, relative to its un-zoomed font. `R_t` starts
//! at 1.0 and is never persisted. An every-module step leaves it so terminals follow A; `Ctrl+0`
//! sets it to 1.0. An editor-only step leaves it. A terminal step or reset changes only `R_t`, without writing S.
//! Reports leave it untouched.
//!
//! ## No editor
//!
//! A window with no editor (a linked second window, a module window, a companion window) has no S. A is its own, it
//! starts at 1.0 and nothing is written ([`TextSize::apply_app_step`]).

use crate::layout::{ModuleId, ModuleKind};
use std::collections::BTreeMap;

pub const ZOOM_STEP: f32 = 0.1;
pub const ZOOM_MIN: f32 = 0.5;
pub const ZOOM_MAX: f32 = 3.0;

/// How many of this window's own not-yet-echoed writes are remembered at once, so an
/// intermediate echo of an EARLIER one -- not necessarily the latest -- can still be recognised as
/// ours rather than mistaken for an external change. Far more than any realistic run of key
/// presses between ticks; oldest drops first if it is ever exceeded.
pub const BURST_CAP: usize = 64;

/// Every value this module writes is rounded to two decimals so repeated steps do not accumulate
/// float noise -- ten `both_larger` steps from 1.0 land on exactly 2.0 because each step
/// starts from the previous step's already-rounded result, not because `+=0.1` is somehow exact.
fn round2(x: f32) -> f32 {
    (x * 100.0).round() / 100.0
}

/// A value this module is about to write to `g:neovide_scale_factor`: clamped to the zoom range
/// and rounded. Reports arriving FROM nvim are never passed through this -- the editor shows
/// whatever nvim holds, so a report is clamped only where it feeds the chat's own arithmetic
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

/// `s`, or 1.0 when it is not finite and positive: the app size stays a number the arithmetic can use.
fn usable(s: f32) -> f32 {
    if s.is_finite() && s > 0.0 {
        s
    } else {
        1.0
    }
}

/// Pushes `v` onto `burst`, dropping the oldest entry first if it is already at [`BURST_CAP`].
fn push_burst(burst: &mut Vec<f32>, v: f32) {
    if burst.len() >= BURST_CAP {
        burst.remove(0);
    }
    burst.push(v);
}

/// What a text size key means, before [`route_focused_module`] decides which module it applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextStep {
    Larger,
    Smaller,
    Reset,
}

impl From<crate::keymap::TextChange> for TextStep {
    fn from(change: crate::keymap::TextChange) -> Self {
        match change {
            crate::keymap::TextChange::Larger => TextStep::Larger,
            crate::keymap::TextChange::Smaller => TextStep::Smaller,
            crate::keymap::TextChange::Reset => TextStep::Reset,
        }
    }
}

/// S, the app size, the offsets, a pending write and the burst it belongs to, and the panel's base size. See this
/// module's own doc for what each means; every operation here is a pure function of the state alone -- there
/// is no notion of time anywhere in this type, because any timeout is a guess at nvim's speed.
#[derive(Debug, Clone, PartialEq)]
pub struct TextSize {
    /// `g:neovide_scale_factor` as nvim last actually reported it -- authoritative for the editor.
    /// Starts at the editor's `scale_factor_setting()` read at startup, never this module's own
    /// 1.0 assumption, so a value the user's `init.lua` set is respected from the first frame.
    reported_s: f32,
    /// A: the size every module follows.
    app: f32,
    /// E: the editor's offset. `S = clamp(app × editor)`.
    editor: f32,
    /// Where `Ctrl+0` returns A to.
    configured: f32,
    /// The panel's offset relative to A. 1.0 keeps the panel in step, which is the default for every writer of S.
    r: f32,
    /// Every terminal's offset relative to A, kept for the window even before a terminal is shown.
    terminal_r: f32,
    /// A write this window made that nvim has not yet echoed back. See this module's own doc
    /// ("The model") for exactly when this is set, cleared, or left alone.
    pending: Option<f32>,
    /// Every value written since `pending` was last `None`, capped at [`BURST_CAP`] -- see this
    /// module's own doc.
    burst: Vec<f32>,
    /// `agent.font_size`, or the shipped default -- the panel's un-zoomed size.
    base: f32,
    /// Each page shown this session and its offset relative to A (this module's "Pages" section). A
    /// page not in the map reads 1.0.
    pages: BTreeMap<ModuleId, f32>,
}

impl TextSize {
    /// The starting state for a window: every offset 1.0, no write outstanding, S seeded from
    /// `reported_s` -- the editor's own `scale_factor_setting()` at startup, or `1.0` with no editor.
    pub fn new(base: f32, reported_s: f32) -> Self {
        let app = usable(reported_s);
        TextSize {
            reported_s,
            app,
            editor: 1.0,
            configured: app,
            r: 1.0,
            terminal_r: 1.0,
            pending: None,
            burst: Vec::new(),
            base,
            pages: BTreeMap::new(),
        }
    }

    /// The value an editor-only step moves from: the outstanding pending write, or `reported_s`
    /// if none -- substituted with 1.0 when that is not finite and positive. An operation stepping off a
    /// garbage report would otherwise propagate it (`NaN + 0.1` is still `NaN`) rather than
    /// recovering from it the moment the user next touches a text-size key. No deadline gates
    /// `pending`: it is trusted until a report says otherwise, however long that takes (a deadline
    /// shorter than a slow echo would make the next step set E against a stale S).
    fn current_s(&self) -> f32 {
        usable(self.pending.unwrap_or(self.reported_s))
    }

    /// A new un-zoomed size for the panel (the editor's font changed while the panel follows it): A, the offsets
    /// and any outstanding write are untouched, so a size in effect stays in effect at the new base.
    pub fn with_base(mut self, base: f32) -> Self {
        self.base = base;
        self
    }

    /// Where `Ctrl+0` returns the app size to, from nvim's VimEnter. Nothing moves until it is used.
    pub fn with_configured(mut self, scale: f32) -> Self {
        self.configured = usable(scale);
        self
    }

    /// `P = A × R`, clamped -- the chat's effective scale before it is multiplied by `base`.
    pub fn panel_scale(&self) -> f32 {
        (self.app * self.r).clamp(ZOOM_MIN, ZOOM_MAX)
    }

    /// `base × panel_scale()` -- what the host hands the chat as its font size in px.
    pub fn panel_px(&self) -> f32 {
        self.base * self.panel_scale()
    }

    /// `clamp(A × R_t)` -- every terminal's zoom relative to its un-zoomed font.
    pub fn terminal_scale(&self) -> f32 {
        (self.app * self.terminal_r).clamp(ZOOM_MIN, ZOOM_MAX)
    }

    // --- Every module (the app-level accelerators, `text.all.*`). No offset is touched by a step. ---

    /// `app.text-larger`: `A' = clamp(A + 0.1)`, `S' = clamp(A' × E)`.
    pub fn both_larger(self) -> (Self, Option<f32>) {
        let app = clamp_round_s(self.app + ZOOM_STEP);
        self.write_app(app)
    }

    /// `app.text-smaller`: `A' = clamp(A - 0.1)`, `S' = clamp(A' × E)`.
    pub fn both_smaller(self) -> (Self, Option<f32>) {
        let app = clamp_round_s(self.app - ZOOM_STEP);
        self.write_app(app)
    }

    /// `app.text-reset`: A back to the configured size and every offset 1.0 -- also the universal heal for a
    /// `pending` stuck on a write nvim never answered (see this module's own doc): if `reported_s` is already that
    /// value, `write`'s rule 2 fires regardless of what `pending` currently holds.
    pub fn both_reset(mut self) -> (Self, Option<f32>) {
        self.r = 1.0;
        self.terminal_r = 1.0;
        self.editor = 1.0;
        for r in self.pages.values_mut() {
            *r = 1.0;
        }
        let app = clamp_round_s(self.configured);
        self.write_app(app)
    }

    pub fn apply_both_step(self, step: TextStep) -> (Self, Option<f32>) {
        match step {
            TextStep::Larger => self.both_larger(),
            TextStep::Smaller => self.both_smaller(),
            TextStep::Reset => self.both_reset(),
        }
    }

    /// The same step in a window with no editor, where nothing holds S: only A moves, and no write stays outstanding.
    pub fn apply_app_step(self, step: TextStep) -> Self {
        let (state, _) = self.apply_both_step(step);
        TextSize {
            pending: None,
            burst: Vec::new(),
            ..state
        }
    }

    // --- One pane, editor focused (`prefix C-=`/`C--`/`C-0`, or the wheel). Each writes a fresh S; only E moves with
    // it, so no other module does. ---

    /// `S' = clamp(S + 0.1)`; `E' = S'/A`.
    pub fn editor_larger(self) -> (Self, Option<f32>) {
        let target = self.current_s() + ZOOM_STEP;
        self.write_s_keeping_app(target)
    }

    /// `S' = clamp(S - 0.1)`; `E' = S'/A`.
    pub fn editor_smaller(self) -> (Self, Option<f32>) {
        let target = self.current_s() - ZOOM_STEP;
        self.write_s_keeping_app(target)
    }

    /// `E' = 1`, `S' = A`: the editor back in step with every other module.
    pub fn editor_reset(mut self) -> (Self, Option<f32>) {
        self.editor = 1.0;
        let s = clamp_round_s(self.app);
        self.write(s)
    }

    pub fn apply_editor_step(self, step: TextStep) -> (Self, Option<f32>) {
        match step {
            TextStep::Larger => self.editor_larger(),
            TextStep::Smaller => self.editor_smaller(),
            TextStep::Reset => self.editor_reset(),
        }
    }

    // --- One pane, chat focused (`prefix C-=`/`C--`/`C-0`, or the wheel).
    // None of these write S at all -- only R moves. ---

    /// `P' = clamp(P + 0.1)`; `R' = P'/A`.
    pub fn panel_larger(self) -> Self {
        let target = self.panel_scale() + ZOOM_STEP;
        self.set_panel_scale(target)
    }

    /// `P' = clamp(P - 0.1)`; `R' = P'/A`.
    pub fn panel_smaller(self) -> Self {
        let target = self.panel_scale() - ZOOM_STEP;
        self.set_panel_scale(target)
    }

    /// `R' = 1` -- the panel back in step with the app size.
    pub fn panel_reset(self) -> Self {
        TextSize { r: 1.0, ..self }
    }

    pub fn apply_panel_step(self, step: TextStep) -> Self {
        match step {
            TextStep::Larger => self.panel_larger(),
            TextStep::Smaller => self.panel_smaller(),
            TextStep::Reset => self.panel_reset(),
        }
    }

    // --- One pane, a terminal holding the keys: the panel's three rows, on the shared factor.
    // None of these write S, and neither the panel nor any page moves. ---

    /// `P_t' = clamp(P_t + 0.1)`; `R_t' = P_t'/A`.
    pub fn terminal_larger(self) -> Self {
        let target = self.terminal_scale() + ZOOM_STEP;
        self.set_terminal_scale(target)
    }

    /// `P_t' = clamp(P_t - 0.1)`; `R_t' = P_t'/A`.
    pub fn terminal_smaller(self) -> Self {
        let target = self.terminal_scale() - ZOOM_STEP;
        self.set_terminal_scale(target)
    }

    /// `R_t' = 1` -- every terminal back in step with the app size.
    pub fn terminal_reset(self) -> Self {
        TextSize {
            terminal_r: 1.0,
            ..self
        }
    }

    pub fn apply_terminal_step(self, step: TextStep) -> Self {
        match step {
            TextStep::Larger => self.terminal_larger(),
            TextStep::Smaller => self.terminal_smaller(),
            TextStep::Reset => self.terminal_reset(),
        }
    }

    /// `R_t' = clamp(target)/A`, clamping the target itself so repeated steps at an end of the
    /// range keep the raw offset bounded, as [`set_panel_scale`](Self::set_panel_scale) does.
    fn set_terminal_scale(self, target: f32) -> Self {
        let terminal_r = target.clamp(ZOOM_MIN, ZOOM_MAX) / self.app;
        TextSize { terminal_r, ..self }
    }

    // --- One pane, a page holding the keys: the panel's three rows, on that page's own offset.
    // None of these write S, and neither the panel nor any other page moves. ---

    /// The page is shown. A page already known keeps its offset (a page hidden and shown again, or rebuilt after a
    /// kill).
    pub fn page_shown(mut self, page: &ModuleId) -> Self {
        self.pages.entry(page.clone()).or_insert(1.0);
        self
    }

    /// `R_page`: 1.0 for a page never stepped.
    fn page_r(&self, page: &ModuleId) -> f32 {
        self.pages.get(page).copied().unwrap_or(1.0)
    }

    /// `clamp(A × R_page)` -- the zoom level the page's view is given.
    pub fn page_scale(&self, page: &ModuleId) -> f32 {
        (self.app * self.page_r(page)).clamp(ZOOM_MIN, ZOOM_MAX)
    }

    /// `P_page' = clamp(P_page + 0.1)`; `R_page' = P_page'/A`.
    pub fn page_larger(self, page: &ModuleId) -> Self {
        let target = self.page_scale(page) + ZOOM_STEP;
        self.set_page_scale(page, target)
    }

    /// `P_page' = clamp(P_page - 0.1)`; `R_page' = P_page'/A`.
    pub fn page_smaller(self, page: &ModuleId) -> Self {
        let target = self.page_scale(page) - ZOOM_STEP;
        self.set_page_scale(page, target)
    }

    /// `R_page' = 1` -- the page back in step with the app size.
    pub fn page_reset(mut self, page: &ModuleId) -> Self {
        self.pages.insert(page.clone(), 1.0);
        self
    }

    pub fn apply_page_step(self, page: &ModuleId, step: TextStep) -> Self {
        match step {
            TextStep::Larger => self.page_larger(page),
            TextStep::Smaller => self.page_smaller(page),
            TextStep::Reset => self.page_reset(page),
        }
    }

    /// `R_page' = clamp(target)/A`, clamping the target itself for the same reason as
    /// [`set_panel_scale`](Self::set_panel_scale): the raw offset stays bounded under repeated
    /// presses at an end of the range.
    fn set_page_scale(mut self, page: &ModuleId, target: f32) -> Self {
        let r = target.clamp(ZOOM_MIN, ZOOM_MAX) / self.app;
        self.pages.insert(page.clone(), r);
        self
    }

    // --- What nvim reports. ---

    /// nvim reported a new `g:neovide_scale_factor` -- our own write echoing back, an echo of an
    /// OLDER write of ours still inside the current burst, or a real external change (the user's
    /// own mapping, a typed `:let`). `reported_s` follows the report unconditionally -- nvim's
    /// report is authoritative for the editor regardless of what this window happens to be waiting
    /// for. An external change moves A and leaves every offset, because "together" is the default for every writer,
    /// not just this window's own keys.
    pub fn nvim_reported_s(self, reported: f32) -> Self {
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
                app: usable(reported) / self.editor,
                pending: None,
                burst: Vec::new(),
                ..self
            },
        }
    }

    /// The shared decision behind every value this window writes to S: `s` must already be
    /// rounded/clamped (every call site here routes through [`clamp_round_s`] first).
    ///
    /// 1. If `s` already equals the outstanding `pending`, or `reported_s` when there is none
    ///    (the raw value, not [`current_s`](Self::current_s)'s 1.0 substitute), nothing is written
    ///    and nothing changes -- nvim's watcher would see no change either, so nothing would ever echo it (the
    ///    same "never write an equal value" rule the shell's window modes already use).
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

    /// A new app size, already clamped and rounded, and the S it makes with the editor's offset.
    fn write_app(self, app: f32) -> (Self, Option<f32>) {
        let s = clamp_round_s(app * self.editor);
        TextSize { app, ..self }.write(s)
    }

    /// A fresh S for the editor alone: `E' = S'/A`, so A and every other module stay where they are.
    fn write_s_keeping_app(self, target: f32) -> (Self, Option<f32>) {
        let s = clamp_round_s(target);
        let editor = s / self.app;
        TextSize { editor, ..self }.write(s)
    }

    /// `R' = clamp(target)/A`, the shared step behind `panel_larger`/`panel_smaller`. Clamps
    /// `target` itself (not just what `panel_scale()` later reports) so `r` cannot run away
    /// unboundedly under repeated presses even though `panel_scale()` would mask that by clamping
    /// its own output regardless -- see `panel_larger_clamps_the_raw_r_too_not_just_panel_scales_own_clamp`.
    fn set_panel_scale(self, target: f32) -> Self {
        let r = target.clamp(ZOOM_MIN, ZOOM_MAX) / self.app;
        TextSize { r, ..self }
    }
}

/// One pane, decided by which module holds the keys (`prefix C-=`/`C--`/`C-0`) -- pure, so the prefix
/// routing has exactly one place to get right rather than re-deriving "the editor, the chat, the
/// terminals, a page, anything else a no-op" inline at the call site, where a swap of two arms would go unnoticed by
/// any test of the model alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextSizeTarget {
    Editor,
    Panel,
    Terminal,
    /// A Lua panel's page (a plugin's page, or one `eitri.panel.register` loads): its own factor.
    Page(ModuleId),
    Neither,
}

/// `focused`: the host's own answer (`GtkHost::focused_module`) -- the module that HOLDS THE KEYS
/// right now, not the remembered last holder. A Lua panel (a plugin page) is [`TextSizeTarget::Page`].
/// Every terminal instance is [`TextSizeTarget::Terminal`], sharing one factor for the window.
/// Any other focus (the top bar, the canvas) is [`TextSizeTarget::Neither`]: these keys do nothing.
///
/// Keyed by the module's kind, not by a slot: a `ModuleId` says what it is, so a Lua panel -- even
/// one whose own id is `"editor"` -- can never be routed as the editor.
pub fn route_focused_module(focused: Option<&ModuleId>) -> TextSizeTarget {
    let Some(id) = focused else {
        return TextSizeTarget::Neither;
    };
    match id.kind() {
        ModuleKind::Editor => TextSizeTarget::Editor,
        ModuleKind::Agent => TextSizeTarget::Panel,
        ModuleKind::LuaWebview => TextSizeTarget::Page(id.clone()),
        ModuleKind::Terminal => TextSizeTarget::Terminal,
        ModuleKind::Canvas => TextSizeTarget::Neither,
    }
}

/// Every `app.text-*` action the shell registers: its GTK action name and which [`TextStep`] it
/// performs -- one table the shell's `TextSizeController::install` iterates, so a name and a step
/// cannot drift apart. The accelerators are `crate::keymap::root`'s: `Ctrl+=`,
/// `Ctrl+-`, `Ctrl+0` and their keypad forms. `Ctrl++` is no accelerator: `<Control>plus` also fires on a US
/// layout's `Ctrl+Shift+=`, and no root chord holds `Ctrl+Shift`.
pub const TEXT_SIZE_ACTIONS: &[(&str, TextStep)] = &[
    ("text-larger", TextStep::Larger),
    ("text-smaller", TextStep::Smaller),
    ("text-reset", TextStep::Reset),
];

/// Formats a write for the `[text-size]` log lines -- `None` (a true no-op, the write's rule 1) reads
/// as "(unchanged)" rather than omitting the write entirely, so a log reader can tell "nothing
/// changed" apart from a missing line.
pub fn describe_write(write: Option<f32>) -> String {
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
        use crate::keymap::TextChange;
        assert_eq!(TextStep::from(TextChange::Larger), TextStep::Larger);
        assert_eq!(TextStep::from(TextChange::Smaller), TextStep::Smaller);
        assert_eq!(TextStep::from(TextChange::Reset), TextStep::Reset);
    }

    fn ts(s: f32, r: f32, base: f32) -> TextSize {
        TextSize {
            reported_s: s,
            app: s,
            editor: 1.0,
            configured: 1.0,
            r,
            terminal_r: 1.0,
            pending: None,
            burst: Vec::new(),
            base,
            pages: BTreeMap::new(),
        }
    }

    // --- Both panes together. ---

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

    // --- One pane, chat focused. Never writes S. ---

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

    /// `R' = P'/S` -- at a non-unit S, dividing by 1.0 instead of the real S (a mutation a test at
    /// S = 1.0 alone cannot catch) would give a visibly different, wrong `r`.
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

    /// The clamp's `set_panel_scale` half: repeatedly zooming the panel must not let the raw `r`
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

    // --- One pane, editor focused. Writes S; R compensates. ---

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

    /// The editor's offset cleared, with the rest where it was: from `Ctrl+=` three times, then the editor two steps
    /// past the others, a reset puts the editor at the app size, not at 1.0.
    #[test]
    fn editor_reset_rejoins_the_app_size_and_moves_nothing_else() {
        let mut state = TextSize::new(14.0, 1.0);
        for _ in 0..3 {
            let (next, write) = state.both_larger();
            state = next.nvim_reported_s(write.unwrap());
        }
        for _ in 0..2 {
            let (next, write) = state.editor_larger();
            state = next.nvim_reported_s(write.unwrap());
        }
        assert_eq!(state.reported_s, 1.5);
        let px = state.panel_px();
        let (next, write) = state.editor_reset();
        assert_eq!(write, Some(1.3));
        assert_eq!(next.editor, 1.0);
        assert!(close(next.panel_px(), px));
        assert!(close(next.panel_scale(), 1.3));
    }

    /// In step already, a reset has nothing to write: three `Ctrl+=` leave the editor at 1.3 with everyone else.
    #[test]
    fn editor_reset_in_step_writes_nothing() {
        let (next, write) = ts(1.3, 1.0, 14.0).editor_reset();
        assert_eq!(write, None);
        assert_eq!(next.current_s(), 1.3);
    }

    // --- The app size and the offsets. ---

    /// Every module steps from the app size, and each keeps its offset, the editor's among them.
    #[test]
    fn an_every_module_step_keeps_every_offset() {
        let state = TextSize::new(14.0, 1.0).page_shown(&page("a"));
        let (state, _) = state.editor_larger();
        let state = state
            .nvim_reported_s(1.1)
            .panel_larger()
            .terminal_larger()
            .page_larger(&page("a"));
        let (state, write) = state.both_larger();
        assert_eq!(write, Some(1.21), "S = A × E");
        assert!(close(state.app, 1.1));
        assert!(close(state.panel_scale(), 1.1 * 1.1));
        assert!(close(state.terminal_scale(), 1.1 * 1.1));
        assert!(close(state.page_scale(&page("a")), 1.1 * 1.1));
        let state = state.nvim_reported_s(1.21);
        assert!(close(state.panel_scale(), 1.1 * 1.1), "the echo moves nothing");
    }

    /// Every writer moves every module: a `:let` of S moves the app size, and the editor keeps its offset.
    #[test]
    fn an_external_write_moves_the_app_size_through_the_editors_offset() {
        let (state, write) = ts(1.0, 1.0, 14.0).editor_larger();
        let state = state.nvim_reported_s(write.unwrap()).nvim_reported_s(2.2);
        assert!(close(state.app, 2.0));
        assert!(close(state.editor, 1.1));
        assert!(close(state.panel_scale(), 2.0));
        let (reset, write) = state.editor_reset();
        assert_eq!(write, Some(2.0), "a pane reset rejoins wherever the app size went");
        assert!(close(reset.panel_scale(), 2.0));
    }

    /// The editor stops at the top of the range and the others go on; on the way back it keeps its offset, where a
    /// model reading A off S would have lost it to the clamp.
    #[test]
    fn the_clamp_on_s_costs_the_app_size_nothing() {
        let (state, write) = ts(2.5, 1.0, 14.0).editor_larger();
        let mut state = state.nvim_reported_s(write.unwrap());
        for _ in 0..5 {
            let (next, write) = state.editor_larger();
            state = write.map_or(next.clone(), |s| next.nvim_reported_s(s));
        }
        assert_eq!(state.reported_s, 3.0);
        let editor = state.editor;
        assert!(close(editor, 1.2));
        let (state, write) = state.both_larger();
        assert_eq!(write, None, "S stays at the top");
        assert!(close(state.panel_scale(), 2.6), "the app size went on");
        let (state, write) = state.both_smaller();
        assert_eq!(write, None);
        assert!(close(state.panel_scale(), 2.5));
        let (state, write) = state.both_smaller();
        assert_eq!(write, Some(2.88));
        assert_eq!(state.editor, editor, "still the offset it had");
    }

    /// A write that rounds to the value last reported is still another writer's, with one of ours outstanding: S is
    /// what nvim holds now, and the app size follows it rather than the write that lost.
    #[test]
    fn an_external_write_near_the_last_report_still_moves_the_app_size() {
        let mut state = TextSize::new(14.0, 1.0);
        for _ in 0..10 {
            state = state.both_larger().0;
        }
        assert_eq!(state.pending, Some(2.0));
        let state = state.nvim_reported_s(1.004);
        assert_eq!(state.pending, None);
        assert!(close(state.panel_scale(), 1.004));
    }

    /// A pair of every-module steps inside one tick, with the echoes arriving after rule 2 cleared the write: each
    /// reads as external on the way, and the end state is where the pair started, offset kept.
    #[test]
    fn a_net_zero_pair_with_an_offset_ends_where_it_started() {
        let (state, write) = ts(1.0, 1.0, 14.0).editor_larger();
        let (state, _) = state.nvim_reported_s(write.unwrap()).editor_larger();
        let state = state.nvim_reported_s(1.2);
        let editor = state.editor;
        let (state, first) = state.both_larger();
        let (state, second) = state.both_smaller();
        assert_eq!(first, Some(1.32));
        assert_eq!(second, Some(1.2), "rule 2");
        assert_eq!(state.pending, None);
        let state = state.nvim_reported_s(1.32).nvim_reported_s(1.2);
        assert!(close(state.app, 1.0));
        assert_eq!(state.editor, editor);
    }

    /// `Ctrl+0` returns to what the config said, every offset cleared.
    #[test]
    fn reset_returns_to_the_configured_size() {
        let state = TextSize::new(14.0, 1.0).with_configured(1.25).nvim_reported_s(1.25);
        let (state, write) = state.both_larger();
        let (state, _) = state.nvim_reported_s(write.unwrap()).editor_larger();
        let state = state.nvim_reported_s(1.45).panel_larger().terminal_smaller();
        let (state, write) = state.both_reset();
        assert_eq!(write, Some(1.25));
        assert_eq!(state.editor, 1.0);
        assert_eq!(state.panel_scale(), 1.25);
        assert_eq!(state.terminal_scale(), 1.25);
    }

    /// The configured size moves nothing by itself, and a bad value is no size.
    #[test]
    fn the_configured_size_is_used_only_by_a_reset() {
        let state = TextSize::new(14.0, 1.1);
        let next = state.clone().with_configured(2.0);
        assert_eq!(next.panel_scale(), state.panel_scale());
        assert_eq!(next.reported_s, state.reported_s);
        for bad in [f32::NAN, 0.0, -1.0, f32::INFINITY] {
            assert_eq!(state.clone().with_configured(bad).configured, 1.0, "{bad}");
        }
    }

    /// With no editor nothing holds S: the app size moves, every offset is kept, pages follow, no write is left
    /// outstanding, and a reset clears the offsets.
    #[test]
    fn with_no_editor_an_every_module_step_writes_nothing_and_moves_everything() {
        let state = TextSize::new(14.0, 1.0).page_shown(&page("a")).page_larger(&page("a"));
        let state = state.apply_app_step(TextStep::Larger);
        assert_eq!(state.pending, None);
        assert!(close(state.panel_scale(), 1.1));
        assert!(close(state.terminal_scale(), 1.1));
        assert!(close(state.page_scale(&page("a")), 1.21));
        let state = state.apply_app_step(TextStep::Larger).apply_app_step(TextStep::Reset);
        assert_eq!(state.panel_scale(), 1.0);
        assert_eq!(state.page_scale(&page("a")), 1.0);
        assert_eq!(state.pending, None);
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

    // --- The clamp's `panel_scale`/`panel_px` half: an external report still clamps the chat. ---

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

    // --- What nvim reports, and the model with no clock. ---

    #[test]
    fn an_external_let_updates_s_and_leaves_r_unchanged() {
        // Nobody here wrote anything (`pending` is `None`): a real `:let` or the user's own
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

    /// The echo the pending slot exists for: an editor-only step writes S optimistically and sets
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

    // --- A write nvim never echoes, a stuck pending, and echoes arriving later than any deadline
    // would allow -- each test below is one of those scenarios. ---

    /// The write's rule 1: a write whose value already equals `current_s()` is a true no-op -- nothing is
    /// written to nvim at all, because nvim's watcher would see no change and so would never echo
    /// it (the same "never write an equal value" rule the shell's window modes use).
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
    /// it SHOWS (`current_s() × R`, with the pending S), not from the stale `reported_s`. Without
    /// this test, `set_panel_scale` dividing by `reported_s` instead would pass every other one.
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

    /// An echo of an older write in the burst still moves `reported_s` (leaving it behind would
    /// pass every other test).
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

    /// s7: two editor-only presses in a row with NO echo for
    /// either yet. With no deadline to go stale against, the second press must step from the real
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

    /// s6: a single editor-only press with an echo that a model with a deadline would see
    /// arrive after it. There is no deadline, so however long nvim takes, the panel
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

    /// s4/s5: both-panes larger, pressed twice with no echo for either yet. A model with a
    /// deadline would flash back to the pre-write size once the deadline passed (s4) and repeat the
    /// same value on a second press after that (s5, "writes 1.1 again, not 1.2"). With no deadline,
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

        // No report has arrived for either write yet. An external `:let` (or the user's own
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
    /// write, or just an unusually slow nvim) is the one case a deadline would exist to rescue. The
    /// user already has a key for it: `Ctrl+0` writes `1.0`, and since `reported_s` really is
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

    // --- Helpers: the action table and the prefix's routing. ---

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
                !crate::keymap::root::chords_of(name).is_empty(),
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
    fn route_focused_module_is_neither_off_the_top_bar_or_on_the_canvas() {
        assert_eq!(route_focused_module(None), TextSizeTarget::Neither);
        assert_eq!(
            route_focused_module(Some(&ModuleId::parse("canvas").unwrap())),
            TextSizeTarget::Neither
        );
    }

    #[test]
    fn route_focused_module_sends_every_terminal_to_the_terminal() {
        for module in [ModuleId::terminal(), ModuleId::parse("terminal#2").unwrap()] {
            assert_eq!(route_focused_module(Some(&module)), TextSizeTarget::Terminal);
        }
    }

    /// A plugin page (a Lua panel module) holding the keys is its own target, named by its module,
    /// so the prefix steps that page and no other.
    #[test]
    fn route_focused_module_sends_a_lua_panel_to_its_page() {
        assert_eq!(
            route_focused_module(Some(&ModuleId::lua("demo"))),
            TextSizeTarget::Page(ModuleId::lua("demo"))
        );
        // A Lua panel that named itself "editor" is still a Lua panel, never the editor.
        assert_eq!(
            route_focused_module(Some(&ModuleId::lua("editor"))),
            TextSizeTarget::Page(ModuleId::lua("editor"))
        );
    }

    // --- Terminals: one factor for the window. ---

    #[test]
    fn both_steps_move_the_terminal_with_s_and_reset_its_factor() {
        let state = TextSize::new(14.0, 1.5).with_configured(1.0).terminal_larger();
        let factor = state.terminal_r;
        for (step, s) in [(TextStep::Larger, 1.6), (TextStep::Smaller, 1.4)] {
            let (next, write) = state.clone().apply_both_step(step);
            assert_eq!(write, Some(s));
            assert_eq!(next.terminal_r, factor);
            assert!(close(next.terminal_scale(), s * factor));
            let echoed = next.nvim_reported_s(s);
            assert_eq!(echoed.terminal_r, factor);
            assert!(close(echoed.terminal_scale(), s * factor));
        }
        let (reset, write) = state.both_reset();
        assert_eq!(write, Some(1.0));
        assert_eq!(reset.terminal_r, 1.0);
        assert_eq!(reset.terminal_scale(), 1.0);
        let (reset, write) = TextSize::new(14.0, 1.0).terminal_larger().both_reset();
        assert_eq!(write, None);
        assert_eq!(reset.terminal_r, 1.0);
        assert_eq!(reset.terminal_scale(), 1.0);
    }

    #[test]
    fn editor_steps_keep_the_terminal_in_place_before_and_after_the_echo() {
        for s in [ZOOM_MIN, 1.0, 1.5, ZOOM_MAX] {
            let state = TextSize::new(14.0, s).terminal_larger();
            let level = state.terminal_scale();
            for step in [TextStep::Larger, TextStep::Smaller, TextStep::Reset] {
                let (next, write) = state.clone().apply_editor_step(step);
                assert!(close(next.terminal_scale(), level), "{s} {step:?}");
                if let Some(s) = write {
                    assert!(close(next.nvim_reported_s(s).terminal_scale(), level));
                }
            }
        }
    }

    #[test]
    fn terminal_steps_change_only_the_terminal_factor() {
        let (state, _) = TextSize::new(14.0, 1.4)
            .panel_larger()
            .page_shown(&page("a"))
            .page_larger(&page("a"))
            .both_larger();
        let state = state.terminal_larger();
        for (step, level) in [
            (TextStep::Larger, 1.7),
            (TextStep::Smaller, 1.5),
            (TextStep::Reset, 1.5),
        ] {
            let next = state.clone().apply_terminal_step(step);
            assert!(close(next.terminal_scale(), level));
            assert!(close(next.terminal_r, level / 1.5));
            let mut expected = state.clone();
            expected.terminal_r = next.terminal_r;
            assert_eq!(next, expected, "{step:?} leaves S, R, pages and pending writes alone");
        }
        let reset = state.terminal_reset();
        assert_eq!(reset.terminal_r, 1.0);
        assert!(close(reset.terminal_scale(), 1.5));
    }

    #[test]
    fn terminal_steps_clamp_the_raw_factor_at_both_ends() {
        for s in [0.25, 1.0, 2.0, 4.0] {
            let mut state = TextSize::new(14.0, s);
            for _ in 0..80 {
                state = state.terminal_larger();
            }
            assert_eq!(state.terminal_scale(), ZOOM_MAX);
            assert!(close(state.terminal_r, ZOOM_MAX / s));
            state = state.terminal_smaller();
            assert!(close(state.terminal_scale(), ZOOM_MAX - ZOOM_STEP));
            for _ in 0..80 {
                state = state.terminal_smaller();
            }
            assert_eq!(state.terminal_scale(), ZOOM_MIN);
            assert!(close(state.terminal_r, ZOOM_MIN / s));
            assert!(close(state.terminal_larger().terminal_scale(), ZOOM_MIN + ZOOM_STEP));
        }
    }

    #[test]
    fn a_bad_s_reads_as_one_for_the_terminal() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -1.0] {
            let state = TextSize::new(14.0, bad);
            assert_eq!(state.terminal_scale(), 1.0);
            assert!(close(state.clone().terminal_larger().terminal_scale(), 1.1));
            assert!(close(state.clone().terminal_smaller().terminal_scale(), 0.9));
            assert_eq!(state.terminal_reset().terminal_scale(), 1.0);
        }
    }

    #[test]
    fn reports_leave_the_terminal_factor_and_follow_the_pending_write() {
        let state = TextSize::new(14.0, 1.0).terminal_larger();
        let factor = state.terminal_r;
        let (state, _) = state.both_larger();
        let (state, _) = state.both_larger();
        assert!(close(state.terminal_scale(), 1.2 * factor));
        let state = state.nvim_reported_s(1.1);
        assert_eq!(state.pending, Some(1.2));
        assert_eq!(state.terminal_r, factor);
        assert!(close(state.terminal_scale(), 1.2 * factor));
        let state = state.nvim_reported_s(1.2);
        assert_eq!(state.pending, None);
        assert_eq!(state.terminal_r, factor);
        assert!(close(state.terminal_scale(), 1.2 * factor));
        let state = state.nvim_reported_s(2.0);
        assert_eq!(state.terminal_r, factor);
        assert!(close(state.terminal_scale(), 2.0 * factor));
        assert!(close(state.clone().with_base(12.0).terminal_scale(), 2.0 * factor));
        assert_eq!(state.nvim_reported_s(4.0).terminal_scale(), ZOOM_MAX);
    }

    // --- Pages: a plugin page follows S with a factor of its own. ---

    fn page(name: &str) -> ModuleId {
        ModuleId::lua(name)
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn a_page_first_shown_is_in_step_with_the_editor() {
        let state = ts(1.3, 0.7, 14.0).page_shown(&page("md"));
        assert_eq!(state.page_scale(&page("md")), 1.3);
        assert_eq!(state.page_scale(&page("never-shown")), 1.3);
    }

    /// `Ctrl+=`/`Ctrl+-` change S and every page follows; no page's factor moves.
    #[test]
    fn both_panes_steps_move_every_page_with_s() {
        let state = ts(1.0, 1.0, 14.0).page_shown(&page("a")).page_larger(&page("a"));
        let state = state.page_shown(&page("b"));
        assert!(close(state.page_scale(&page("a")), 1.1));
        let (state, write) = state.both_larger();
        assert_eq!(write, Some(1.1));
        assert!(close(state.page_scale(&page("a")), 1.1 * 1.1));
        assert!(close(state.page_scale(&page("b")), 1.1));
        let (state, write) = state.both_smaller();
        assert_eq!(write, Some(1.0));
        assert!(close(state.page_scale(&page("a")), 1.1));
        assert!(close(state.page_scale(&page("b")), 1.0));
    }

    /// `Ctrl+0` puts S and every page back at 1.0, as it does the panel.
    #[test]
    fn both_reset_puts_every_page_back_in_step() {
        let state = ts(1.6, 1.0, 14.0)
            .page_shown(&page("a"))
            .page_larger(&page("a"))
            .page_shown(&page("b"))
            .page_smaller(&page("b"));
        let (state, write) = state.both_reset();
        assert_eq!(write, Some(1.0));
        assert_eq!(state.page_scale(&page("a")), 1.0);
        assert_eq!(state.page_scale(&page("b")), 1.0);
        assert_eq!(state.r, 1.0);
    }

    /// The prefix with a page holding the keys: that page alone, the panel's step, nothing written.
    #[test]
    fn a_page_step_moves_that_page_alone_and_writes_nothing() {
        let state = ts(1.5, 1.2, 14.0).page_shown(&page("a")).page_shown(&page("b"));
        let panel_px = state.panel_px();
        let next = state.clone().page_larger(&page("a"));
        assert!(close(next.page_scale(&page("a")), 1.6), "P_page + 0.1 from S = 1.5");
        assert!(
            close(next.pages[&page("a")], 1.6 / 1.5),
            "R_page = P_page / S, not P_page"
        );
        assert_eq!(next.page_scale(&page("b")), 1.5, "another page does not move");
        assert_eq!(next.panel_px(), panel_px, "the panel does not move");
        assert_eq!(next.reported_s, 1.5);
        assert_eq!(next.pending, None, "a page step never writes S");
        let next = next.page_smaller(&page("a")).page_smaller(&page("a"));
        assert!(close(next.page_scale(&page("a")), 1.4));
    }

    #[test]
    fn a_page_reset_puts_that_page_back_in_step_and_leaves_s() {
        let state = ts(1.4, 1.0, 14.0)
            .page_shown(&page("a"))
            .page_larger(&page("a"))
            .page_shown(&page("b"))
            .page_larger(&page("b"));
        let next = state.apply_page_step(&page("a"), TextStep::Reset);
        assert_eq!(next.page_scale(&page("a")), 1.4);
        assert!(
            close(next.page_scale(&page("b")), 1.5),
            "only the page with the keys resets"
        );
        assert_eq!(next.reported_s, 1.4);
        assert_eq!(next.pending, None);
    }

    #[test]
    fn apply_page_step_maps_each_step_to_its_row() {
        let state = ts(1.0, 1.0, 14.0).page_shown(&page("a"));
        assert!(close(
            state
                .clone()
                .apply_page_step(&page("a"), TextStep::Larger)
                .page_scale(&page("a")),
            1.1
        ));
        assert!(close(
            state
                .clone()
                .apply_page_step(&page("a"), TextStep::Smaller)
                .page_scale(&page("a")),
            0.9
        ));
    }

    /// Repeated presses at the top of the range keep the raw factor bounded, as the panel's do.
    #[test]
    fn a_page_step_clamps_the_raw_factor_too() {
        let mut state = ts(1.0, 1.0, 14.0).page_shown(&page("a"));
        for _ in 0..40 {
            state = state.page_larger(&page("a"));
        }
        assert_eq!(state.page_scale(&page("a")), ZOOM_MAX);
        assert!(state.pages[&page("a")] <= ZOOM_MAX + 1e-6);
        for _ in 0..80 {
            state = state.page_smaller(&page("a"));
        }
        assert_eq!(state.page_scale(&page("a")), ZOOM_MIN);
    }

    /// An editor-only step keeps every shown page where it was, as it keeps the panel; a page shown
    /// only afterwards starts at the app size, not at the editor's.
    #[test]
    fn an_editor_only_step_leaves_every_shown_page_in_place() {
        let state = ts(1.0, 1.0, 14.0)
            .page_shown(&page("a"))
            .page_larger(&page("a"))
            .page_shown(&page("b"));
        let (a, b) = (state.page_scale(&page("a")), state.page_scale(&page("b")));
        for op in [
            TextSize::editor_larger as fn(TextSize) -> (TextSize, Option<f32>),
            TextSize::editor_smaller,
        ] {
            let (next, write) = op(state.clone());
            assert!(write.is_some());
            assert!(close(next.page_scale(&page("a")), a));
            assert!(close(next.page_scale(&page("b")), b));
            let next = next.nvim_reported_s(write.unwrap());
            assert!(close(next.page_scale(&page("a")), a), "the echo does not move it");
        }
        let (moved, _) = ts(2.0, 1.0, 14.0).page_shown(&page("a")).editor_reset();
        assert!(close(moved.page_scale(&page("a")), 2.0));
        let (later, write) = state.editor_larger();
        assert_eq!(write, Some(1.1));
        assert!(close(later.page_shown(&page("c")).page_scale(&page("c")), 1.0));
    }

    /// A page shown again (hidden and shown, or its view rebuilt after a kill) keeps its factor.
    #[test]
    fn a_page_shown_again_keeps_its_factor() {
        let state = ts(1.0, 1.0, 14.0).page_shown(&page("a")).page_larger(&page("a"));
        let again = state.page_shown(&page("a"));
        assert!(close(again.page_scale(&page("a")), 1.1));
    }

    /// A page reads S as the panel does: a pending write moves it at once, and the page step made
    /// before the echo steps from what the page shows, so the echo moves nothing.
    #[test]
    fn a_page_follows_a_pending_write_and_steps_from_what_it_shows() {
        let state = TextSize::new(14.0, 1.0).page_shown(&page("a"));
        let (state, write) = state.both_larger();
        assert_eq!(write, Some(1.1));
        assert!(
            close(state.page_scale(&page("a")), 1.1),
            "the pending write, before any echo"
        );
        let state = state.page_larger(&page("a"));
        assert!(close(state.page_scale(&page("a")), 1.2));
        let state = state.nvim_reported_s(1.1);
        assert_eq!(state.pending, None);
        assert!(close(state.page_scale(&page("a")), 1.2), "the echo does not move it");
    }

    /// An external `:let` moves every page with it, its factor kept, and the stuck-pending heal
    /// (`Ctrl+0` at a `reported_s` of 1.0) puts pages back too.
    #[test]
    fn an_external_report_moves_pages_and_reset_heals_them() {
        let state = TextSize::new(14.0, 1.0).page_shown(&page("a")).page_larger(&page("a"));
        let state = state.nvim_reported_s(2.0);
        assert!(close(state.page_scale(&page("a")), 2.2));
        let state = TextSize::new(14.0, 1.0).page_shown(&page("a")).page_larger(&page("a"));
        let (stuck, _) = state.both_larger();
        let (healed, write) = stuck.both_reset();
        assert_eq!(write, Some(1.0));
        assert_eq!(healed.pending, None);
        assert_eq!(healed.page_scale(&page("a")), 1.0);
    }

    /// A report the panel cannot use (not finite, not positive) leaves pages at their factor on 1.0.
    #[test]
    fn a_bad_report_reads_as_one_for_pages() {
        for bad in [f32::NAN, 0.0, -1.0] {
            let state = TextSize::new(14.0, 1.0).page_shown(&page("a")).nvim_reported_s(bad);
            assert_eq!(state.page_scale(&page("a")), 1.0, "{bad}");
        }
    }

    #[test]
    fn a_new_base_keeps_the_zoom_in_effect() {
        let (zoomed, _) = TextSize::new(14.0, 1.0).both_larger();
        let zoomed = zoomed.panel_larger();
        let scale = zoomed.panel_scale();
        let moved = zoomed.clone().with_base(12.0);
        assert_eq!(moved.panel_scale(), scale);
        assert!((moved.panel_px() - 12.0 * scale).abs() < 1e-4);
    }
}
