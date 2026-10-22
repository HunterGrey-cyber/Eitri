//! Zoom both panes together or adjust each pane independently.
//!
//! `S` is nvim's `g:neovide_scale_factor`, shared by application keys, user mappings,
//! and typed `:let` commands. `R` is the panel's factor relative to `S`; it starts at
//! 1.0, survives panel reloads, and is not persisted across restarts. The base size is
//! `agent.font_size` or its default. The panel size is
//! `base * clamp(S * R, ZOOM_MIN, ZOOM_MAX)`.
//!
//! [`TextSize`] is the pure state model. Operations return the new state and any value
//! to send to `set_scale_factor_setting`. [`TextSizeController`] connects it to GTK
//! actions and nvim's scale reports. Pending writes let the panel use the intended
//! scale before nvim echoes it, including during a colorscheme recompute.
//!
//! nvim's scale callback fires only when its per-tick watcher observes a change.
//! Writing a value it already holds may produce no echo. Waiting for that echo alone
//! can leave `pending` stuck and make subsequent steps use a stale scale.
//! A timeout cannot safely resolve this: a slow nvim tick can outlast any deadline.
//! Falling back to `reported_s` before an echo arrives makes editor-only compensation
//! use the wrong scale, and later echoes do not repair the resulting relative factor.
//! The model therefore tracks outstanding writes without measuring elapsed time.
//!
//! `reported_s` starts from the pane's `scale_factor_setting()` and always takes the
//! latest reported value. `pending` holds the latest unconfirmed write.
//! `burst`, capped at [`BURST_CAP`] with the oldest entry dropped, holds values written
//! since `pending` was last cleared. It distinguishes echoes of earlier writes from
//! external changes. `current_s()` uses `pending` when present, otherwise `reported_s`,
//! with a fallback of 1.0 for a value that is not finite and positive.
//!
//! Writes are rounded to two decimals. Writing `current_s()` changes nothing and sends
//! nothing. Writing `reported_s` sends the value but clears `pending` and `burst`:
//! nvim may still need the write, but an unchanged value may never be echoed.
//! Any other write sets `pending` and appends the value to `burst`.
//!
//! Every report updates `reported_s`. An echo matching `pending` clears `pending` and
//! `burst`. An echo matching an older value in `burst` leaves both intact because a
//! newer write is still outstanding. Any unrecognised report is authoritative and
//! clears both. Reports leave the relative panel factor unchanged, so external scale
//! changes move both panes together.
//!
//! Editor-only steps compensate with `R' = R * current_s() / S'` to keep the panel size.
//! Panel-only steps never write `S`: they step the clamped effective panel scale by
//! 0.1 and divide by `current_s()` to obtain the new relative factor.
//! Reset sets `R = 1` and writes 1.0 through the same write logic. If `reported_s` is
//! already 1.0, this clears a pending write that nvim never echoed.
//!
//! Two interleavings can still leave stale state. Reversing zoom operations within one
//! frame, with a nvim tick between them, can leave `R` and the step count wrong until
//! reset. An external writer restoring the last reported value within the same tick
//! as an application write can hide the change from the watcher and leave `pending`
//! stuck until the next key.
//!
//! Lua panel pages follow `S` with their own factor:
//! `page zoom = clamp(S * R_page, ZOOM_MIN, ZOOM_MAX)`. WebKit applies this to the whole
//! page, including layout. Each module's `R_page` starts at 1.0 when first shown,
//! lasts for the session, and is not persisted. Both-panes steps leave it unchanged;
//! both-panes reset sets it to 1.0; editor-only steps compensate it to preserve page
//! size. Prefix keys targeting a page adjust or reset only that page's factor.
//! Pages use the same `current_s()` and pending-write handling as the panel.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use eitri_core::layout::{ModuleId, ModuleKind};
use gtk4::glib;
use gtk4::prelude::*;
use neovide_editor::NeovideEditorPane;
use webkit6::prelude::*;

use crate::agent_panel::AgentPanelHandle;

pub(crate) const ZOOM_STEP: f32 = 0.1;
pub(crate) const ZOOM_MIN: f32 = 0.5;
pub(crate) const ZOOM_MAX: f32 = 3.0;

/// The maximum number of unconfirmed writes remembered so intermediate echoes can be recognised
/// as local writes rather than external changes. If the burst exceeds this cap, the oldest
/// value is dropped first.
const BURST_CAP: usize = 64;

/// Rounds outgoing values to two decimals so repeated steps do not accumulate float noise.
/// Ten `both_larger` steps from 1.0 reach 2.0 because each starts from the rounded previous
/// result, rather than relying on `+=0.1` being exact.
fn round2(x: f32) -> f32 {
    (x * 100.0).round() / 100.0
}

/// Clamps an outgoing `g:neovide_scale_factor` value to the zoom range and rounds it.
/// Reports from nvim bypass this function and remain unchanged for the editor. Only the
/// panel's arithmetic sanitizes or clamps them; see [`TextSize::current_s`].
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

/// One row of the `Ctrl+a =`/`-`/`0` table -- what a plain key after the prefix means,
/// before `main.rs` decides which pane it applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextStep {
    Larger,
    Smaller,
    Reset,
}

impl From<eitri_core::keymap::TextChange> for TextStep {
    fn from(change: eitri_core::keymap::TextChange) -> Self {
        match change {
            eitri_core::keymap::TextChange::Larger => TextStep::Larger,
            eitri_core::keymap::TextChange::Smaller => TextStep::Smaller,
            eitri_core::keymap::TextChange::Reset => TextStep::Reset,
        }
    }
}

/// S, R, a pending write, its burst and the panel's base size. Operations depend only on
/// this state and do not measure time.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TextSize {
    /// Nvim's last reported `g:neovide_scale_factor`, authoritative for the editor. Starts from
    /// the pane's `scale_factor_setting()` so values set in `init.lua` apply from the first frame.
    reported_s: f32,
    /// The panel's factor relative to `reported_s` (or the outstanding pending, if any -- see
    /// [`current_s`](Self::current_s)). 1.0 keeps the panel in step with the editor, which is the
    /// default for every writer of S except the panel-focused prefix keys.
    r: f32,
    /// The latest write awaiting nvim's echo. The report and write handlers determine when it clears.
    pending: Option<f32>,
    /// Values written since `pending` was last `None`, capped at [`BURST_CAP`], used to recognise
    /// intermediate echoes.
    burst: Vec<f32>,
    /// `agent.font_size`, or the shipped default -- the panel's un-zoomed size.
    base: f32,
    /// Each page shown this session and its factor relative to S (this module's "Pages" section). A
    /// page not in the map reads 1.0.
    pages: BTreeMap<ModuleId, f32>,
}

impl TextSize {
    /// Initial window state: R is 1.0 and no write is outstanding. S starts from `reported_s`,
    /// the pane's startup `scale_factor_setting()`, or 1.0 if no editor is present.
    pub(crate) fn new(base: f32, reported_s: f32) -> Self {
        TextSize {
            reported_s,
            r: 1.0,
            pending: None,
            burst: Vec::new(),
            base,
            pages: BTreeMap::new(),
        }
    }

    /// The scale used by each operation: `pending` if present, otherwise `reported_s`.
    /// Substitutes 1.0 for non-finite or non-positive values so invalid reports do not propagate
    /// through arithmetic such as `NaN + 0.1`. A pending value has no expiry; it remains in use
    /// until a report or write resolves it.
    fn current_s(&self) -> f32 {
        let raw = self.pending.unwrap_or(self.reported_s);
        if raw.is_finite() && raw > 0.0 {
            raw
        } else {
            1.0
        }
    }

    /// A new un-zoomed size for the panel (the editor's font changed while the panel follows it): S, R
    /// and any outstanding write are untouched, so a zoom in effect stays in effect at the new size.
    pub(crate) fn with_base(mut self, base: f32) -> Self {
        self.base = base;
        self
    }

    /// The panel's effective scale, `P = S × R`, clamped before multiplication by `base`.
    pub(crate) fn panel_scale(&self) -> f32 {
        (self.current_s() * self.r).clamp(ZOOM_MIN, ZOOM_MAX)
    }

    /// `base × panel_scale()` -- what `shell` hands the agent panel as `font_size_px`.
    pub(crate) fn panel_px(&self) -> f32 {
        self.base * self.panel_scale()
    }

    // --- Both panes. R is never touched here. ---

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

    /// `app.text-reset` sets S, R and every `R_page` to 1.0. If `reported_s` is already 1.0,
    /// the write clears `pending` even when nvim never answered the outstanding write.
    pub(crate) fn both_reset(mut self) -> (Self, Option<f32>) {
        for r in self.pages.values_mut() {
            *r = 1.0;
        }
        self.write_with_r(clamp_round_s(1.0), 1.0)
    }

    pub(crate) fn apply_both_step(self, step: TextStep) -> (Self, Option<f32>) {
        match step {
            TextStep::Larger => self.both_larger(),
            TextStep::Smaller => self.both_smaller(),
            TextStep::Reset => self.both_reset(),
        }
    }

    // --- One pane, editor focused.
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

    // --- One pane, panel focused.
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

    // --- One pane, a page holding the keys: the panel's three rows, on that page's own factor.
    // None of these write S, and neither the panel nor any other page moves. ---

    /// The page is shown: from now on an editor-only step keeps it in place. A page already known
    /// keeps its factor (a page hidden and shown again, or rebuilt after a kill).
    pub(crate) fn page_shown(mut self, page: &ModuleId) -> Self {
        self.pages.entry(page.clone()).or_insert(1.0);
        self
    }

    /// `R_page`: 1.0 for a page never stepped.
    fn page_r(&self, page: &ModuleId) -> f32 {
        self.pages.get(page).copied().unwrap_or(1.0)
    }

    /// `clamp(S × R_page)` -- the zoom level the page's view is given.
    pub(crate) fn page_scale(&self, page: &ModuleId) -> f32 {
        (self.current_s() * self.page_r(page)).clamp(ZOOM_MIN, ZOOM_MAX)
    }

    /// `P_page' = clamp(P_page + 0.1)`; `R_page' = P_page'/S`.
    pub(crate) fn page_larger(self, page: &ModuleId) -> Self {
        let target = self.page_scale(page) + ZOOM_STEP;
        self.set_page_scale(page, target)
    }

    /// `P_page' = clamp(P_page - 0.1)`; `R_page' = P_page'/S`.
    pub(crate) fn page_smaller(self, page: &ModuleId) -> Self {
        let target = self.page_scale(page) - ZOOM_STEP;
        self.set_page_scale(page, target)
    }

    /// `R_page' = 1` -- the page back in step with the editor.
    pub(crate) fn page_reset(mut self, page: &ModuleId) -> Self {
        self.pages.insert(page.clone(), 1.0);
        self
    }

    pub(crate) fn apply_page_step(self, page: &ModuleId, step: TextStep) -> Self {
        match step {
            TextStep::Larger => self.page_larger(page),
            TextStep::Smaller => self.page_smaller(page),
            TextStep::Reset => self.page_reset(page),
        }
    }

    /// `R_page' = clamp(target)/S`, clamping the target itself for the same reason as
    /// [`set_panel_scale`](Self::set_panel_scale): the raw factor stays bounded under repeated
    /// presses at an end of the range.
    fn set_page_scale(mut self, page: &ModuleId, target: f32) -> Self {
        let r = target.clamp(ZOOM_MIN, ZOOM_MAX) / self.current_s();
        self.pages.insert(page.clone(), r);
        self
    }

    // --- What nvim reports. ---

    /// nvim reported a new `g:neovide_scale_factor` -- our own write echoing back, an echo of an
    /// OLDER write of ours still inside the current burst, or a real external change (the user's
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

    /// Handles every outgoing S value. Callers first clamp and round `s` through [`clamp_round_s`].
    /// If `s` equals the raw pending value, or `reported_s` when nothing is pending, nothing is
    /// written and state is unchanged: nvim's watcher may not echo an unchanged value. Comparing
    /// the raw value instead of [`current_s`](Self::current_s)'s fallback lets reset repair an
    /// invalid scale.
    /// If `s` instead equals `reported_s`, send it to correct any pending value nvim has applied,
    /// but clear `pending` and `burst` without waiting for an echo. The state is already correct
    /// when nvim settles at that value.
    /// Otherwise send the write and remember it in `pending` and `burst`.
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
    /// Every shown page's factor is adjusted by the same ratio, so no page moves either.
    fn write_s_keeping_panel_px(mut self, target: f32) -> (Self, Option<f32>) {
        let old_s = self.current_s();
        let s = clamp_round_s(target);
        let r = self.r * old_s / s;
        for page_r in self.pages.values_mut() {
            *page_r = *page_r * old_s / s;
        }
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

/// The target for `Ctrl+a =`/`-`/`0`, selected by the module holding the keys. Centralizing this
/// choice keeps `main.rs` from duplicating the editor, panel and no-op routing at each call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TextSizeTarget {
    Editor,
    Panel,
    /// A Lua panel's page (a plugin's page, or one `eitri.panel.register` loads): its own factor.
    Page(ModuleId),
    Neither,
}

/// Routes `focused`, the module currently receiving keys, by its kind.
/// A Lua panel uses [`TextSizeTarget::Page`]. Focus on the top bar, canvas or
/// bottom terminal uses [`TextSizeTarget::Neither`], so text-size commands do nothing.
/// Routing by kind keeps a Lua panel whose id is `"editor"` from being routed as the editor.
pub(crate) fn route_focused_module(focused: Option<&ModuleId>) -> TextSizeTarget {
    let Some(id) = focused else {
        return TextSizeTarget::Neither;
    };
    match id.kind() {
        ModuleKind::Editor => TextSizeTarget::Editor,
        ModuleKind::Agent => TextSizeTarget::Panel,
        ModuleKind::LuaWebview => TextSizeTarget::Page(id.clone()),
        ModuleKind::Canvas | ModuleKind::Terminal => TextSizeTarget::Neither,
    }
}

/// Returns the live panel font size for the colorscheme listener's `panel_tokens` closure
/// in `main.rs`. A colorscheme change must rebuild the other tokens without resetting zoom,
/// so the closure reads the size from the zoom controls rather than a captured startup base.
pub(crate) fn live_panel_font_size_px(panel_px: &Cell<f32>) -> f32 {
    panel_px.get()
}

/// Every `app.text-*` action and its [`TextStep`] share one table, which
/// `TextSizeController::install` iterates so the action names and steps cannot drift apart.
/// The root keymap binds `Ctrl+=`, `Ctrl+-`, `Ctrl+0` and their keypad forms.
/// `<Control>plus` is excluded because it is `Ctrl+Shift+=` on a US layout,
/// and no root chord holds `Ctrl+Shift`.
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
    /// `None` when there is no nvim to receive `g:neovide_scale_factor` writes.
    /// In that case `apply_editor` logs and does nothing, while `apply_both` scales the
    /// panel alone so the remaining pane can still be resized. Every window has an editor,
    /// even when a Lua `main` panel hides it, so `main.rs` passes `Some`. The pane buffers
    /// writes made before nvim starts.
    editor: Option<Rc<NeovideEditorPane>>,
    panel: AgentPanelHandle,
    /// The panel's live px, shared with `main.rs`'s colorscheme listener (`panel_tokens`, via
    /// [`live_panel_font_size_px`]) so a colorscheme change re-derives every other token but never
    /// resets a zoom. This struct writes it; `main.rs` only ever reads it.
    panel_px: Rc<Cell<f32>>,
    /// Each page's view, by module, given its zoom level whenever the state moves. Weak: the grid
    /// owns the view, and a killed page's view may go.
    pages: RefCell<BTreeMap<ModuleId, glib::WeakRef<webkit6::WebView>>>,
}

impl TextSizeController {
    pub(crate) fn install(
        app: &gtk4::Application,
        editor: Option<Rc<NeovideEditorPane>>,
        panel: AgentPanelHandle,
        base: f32,
        panel_px: Rc<Cell<f32>>,
    ) -> Rc<Self> {
        // Seed `reported_s` from the pane's live value so a scale already set by `init.lua`
        // is preserved. Before nvim exists, `NeovideEditorPane::scale_factor_setting`
        // falls back to `1.0`, which is also used when there is no editor pane.
        let reported_s = editor.as_ref().map(|e| e.scale_factor_setting()).unwrap_or(1.0);
        let this = Rc::new(TextSizeController {
            state: RefCell::new(TextSize::new(base, reported_s)),
            editor,
            panel,
            panel_px,
            pages: RefCell::default(),
        });
        this.push_panel_px();

        // Register app-level actions so GTK handles the key before nvim or the panel.
        // `TEXT_SIZE_ACTIONS` documents why each accelerator is bound.
        for (name, step) in TEXT_SIZE_ACTIONS.iter().copied() {
            let action = gtk4::gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(&this);
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    this.apply_both(step);
                }
            });
            app.add_action(&action);
            app.set_accels_for_action(&format!("app.{name}"), eitri_core::keymap::root::accels(name));
        }

        if let Some(editor) = &this.editor {
            let weak = Rc::downgrade(&this);
            // `nvim_reported_s` has unit tests, but this GTK closure is reached only from the
            // `neovide-editor` tick callback and has no headless harness. Removing the state
            // update or `push_panel_px()` would still pass the tests here. Check this wiring
            // in the GUI by changing S externally, for example with `:let`, and watching
            // the panel follow.
            editor.on_scale_factor_setting(move |value| {
                let Some(this) = weak.upgrade() else { return };
                let next = this.state.borrow().clone().nvim_reported_s(value);
                *this.state.borrow_mut() = next;
                this.push_panel_px();
            });
        }

        this
    }

    /// `app.text-larger`/`-smaller`/`-reset` scale both panes together.
    /// Without an editor, they scale the panel alone.
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

    /// Handles `text.larger`/`text.smaller`/`text.reset` with the editor focused.
    /// These actions are unbound by default; `Ctrl`+wheel over a pane adjusts that pane.
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

    /// The keymap's `text.larger`/`text.smaller`/`text.reset` (and `Ctrl`+wheel) with the page `page`
    /// holding the keys. Never touches nvim: only that page's factor moves. A Lua panel with no view
    /// here (WebKit could not start, or a plugin page not built yet) takes the key and does nothing.
    pub(crate) fn apply_page(&self, page: &ModuleId, step: TextStep) {
        if !self.pages.borrow().contains_key(page) {
            eprintln!("[text-size] {step:?} ({page}): no page view, ignoring");
            return;
        }
        let next = self.state.borrow().clone().apply_page_step(page, step);
        *self.state.borrow_mut() = next;
        self.push_panel_px();
        eprintln!("[text-size] page {page} {:.2}", self.state.borrow().page_scale(page));
    }

    /// The page `page` now has a view: it follows the editor from here on, at 1.0 relative to it the
    /// first time (a view built again for the same module keeps the factor it had). The level is set
    /// again at every committed load, though WebKit keeps it across loads (see
    /// [`crate::plugin_page::set_zoom`]).
    pub(crate) fn attach_page(self: &Rc<Self>, page: ModuleId, view: &webkit6::WebView) {
        let known = self
            .pages
            .borrow()
            .get(&page)
            .and_then(glib::WeakRef::upgrade)
            .is_some_and(|known| known == *view);
        if known {
            return;
        }
        let next = self.state.borrow().clone().page_shown(&page);
        *self.state.borrow_mut() = next;
        self.pages.borrow_mut().insert(page.clone(), view.downgrade());
        {
            let weak = Rc::downgrade(self);
            let page = page.clone();
            view.connect_load_changed(move |view, event| {
                if event != webkit6::LoadEvent::Committed {
                    return;
                }
                let Some(this) = weak.upgrade() else { return };
                let level = this.state.borrow().page_scale(&page);
                crate::plugin_page::set_zoom(view, level);
            });
        }
        let level = self.state.borrow().page_scale(&page);
        crate::plugin_page::set_zoom(view, level);
        eprintln!("[text-size] page {page} shown at {level:.2}");
    }

    /// The panel's un-zoomed size moved (it follows the editor's `guifont`, or the desktop's text scaling
    /// moved the conversion): the live size is re-derived from it with the zoom kept.
    pub(crate) fn set_base(&self, base: f32) {
        let next = self.state.borrow().clone().with_base(base);
        *self.state.borrow_mut() = next;
        self.push_panel_px();
        eprintln!(
            "[text-size] base {base:.2}px (the editor's font), panel {:.1}px",
            self.state.borrow().panel_px()
        );
    }

    /// The state moved: the panel's px and every page's zoom level follow it.
    fn push_panel_px(&self) {
        let px = self.state.borrow().panel_px();
        self.panel_px.set(px);
        self.panel.set_panel_font_size_px(px);
        self.push_pages();
    }

    fn push_pages(&self) {
        let state = self.state.borrow();
        for (page, view) in self.pages.borrow().iter() {
            if let Some(view) = view.upgrade() {
                crate::plugin_page::set_zoom(&view, state.page_scale(page));
            }
        }
    }
}

/// Formats a write for the `[text-size]` log lines above. `None` reads as
/// "(unchanged)" so a log reader can distinguish an unchanged value from a missing line.
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
        use eitri_core::keymap::TextChange;
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

    // --- One pane, panel focused. Never writes S. ---

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

    /// `R' = P'/S`: at a non-unit S, the new ratio must use the real S.
    /// At S=1.5, `Ctrl+a =` with the panel focused takes P from 1.5 to 1.6,
    /// so `r` must be `1.6/1.5`. Omitting `/ current_s` would leave `r` at 1.6
    /// and make P equal `1.6 * 1.5 = 2.4` instead of 1.6.
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

    /// At S=1, repeated panel zooms must keep the raw `r` bounded by `ZOOM_MAX`.
    /// `panel_scale()` clamps its output, which could hide an unbounded `r`.
    /// This test reads `r` directly to check `set_panel_scale`'s clamp on `target`.
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

    // --- `panel_scale` and `panel_px` still clamp the panel after an external report. ---

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

    // --- nvim reports, write echoes, and pending state. ---

    #[test]
    fn an_external_let_updates_s_and_leaves_r_unchanged() {
        // Nobody here wrote anything (`pending` is `None`): a real `:let` or a custom
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

    /// An editor-only step writes S optimistically and sets `pending`.
    /// A colorscheme recompute or other `panel_px()` read before the echo arrives
    /// must see the unchanged panel size. Using the new S with the old R would move it.
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

    /// A write equal to `current_s()` sends nothing to nvim: its watcher would see
    /// no change and would never echo the value. `window_mode::setting_to_write`
    /// also skips equal values.
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

    /// While an editor-only write is in flight, a panel-only step starts from the
    /// visible scale, `current_s() × R`, using the pending S. Dividing by the stale
    /// `reported_s` in `set_panel_scale` would produce the wrong panel size.
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

    /// An echo of an older write in the burst still updates `reported_s`.
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

        // No report has arrived for either write yet. An external `:let` (or a custom
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

    // --- Helpers for the action table and prefix routing. ---

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
                !eitri_core::keymap::root::accels(name).is_empty(),
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
        // The bottom terminal's text size is its own until its phase 4 (`main`: pane 2, Neither).
        assert_eq!(
            route_focused_module(Some(&ModuleId::terminal())),
            TextSizeTarget::Neither
        );
    }

    /// A plugin page (a Lua panel module) holding the keys is its own target, named by its module,
    /// so the prefix steps that page and no other.
    #[test]
    fn route_focused_module_sends_a_lua_panel_to_its_page() {
        assert_eq!(
            route_focused_module(Some(&ModuleId::lua("snotra"))),
            TextSizeTarget::Page(ModuleId::lua("snotra"))
        );
        // A Lua panel that named itself "editor" is still a Lua panel, never the editor.
        assert_eq!(
            route_focused_module(Some(&ModuleId::lua("editor"))),
            TextSizeTarget::Page(ModuleId::lua("editor"))
        );
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
    /// only afterwards starts in step with the new S.
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
        assert!(close(later.page_shown(&page("c")).page_scale(&page("c")), 1.1));
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
