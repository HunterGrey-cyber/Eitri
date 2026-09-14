//! The built-in bottom-slot terminal: a real, live `verdandi.terminal.runtime.v1` session shared
//! between two views -- the native raw-terminal pane (`terminal_pane::TerminalPane`) and the
//! semantic Claude-conversation pane (`semantic_pane::SemanticPane`) -- toggled with `Ctrl+Shift+S`.
//!
//! # One authoritative view switch
//!
//! [`TerminalBottomSlot::set_view`]/[`TerminalBottomSlot::toggle_view`] are the ONE place this
//! repository decides which view is visible. `main.rs`'s `Ctrl+Shift+S` `gio::SimpleAction` calls
//! `toggle_view()`; nothing else may call `gtk4::Stack::set_visible_child_name` on this slot's
//! `Stack` directly. This is what lets a test/diagnostic harness switch views deterministically
//! -- `terminal.set_view(TerminalView::Semantic)` -- without synthesizing a keyboard chord at
//! all, and it means there is exactly one implementation of "which view is showing" to get right
//! (see `TerminalView`'s own doc comment for the pure decision logic this delegates to).
//!
//! # One session, one PTY, two independent watchers
//!
//! `TerminalSession::open` is called exactly ONCE, here, and nowhere else in this repository. Both
//! views attach to that same session via their own independent `TerminalSession::watch` call (the
//! mechanism `terminal-session`'s own doc comment describes: "any number of independent callers
//! watch its raw output stream") -- never a second `open`. See
//! `docs/superpowers/specs/2026-09-12-semantic-pane-integration-v0-design.md` for the full design
//! this module implements.
//!
//! # Why this crate owns the `Term`s and not Verdandi
//!
//! Two independent client-side terminal reconstructions is the design, not a bug: `raw_state`'s
//! `LiveTerminalDriver` (in `terminal-pane`) and `semantic_state`'s `SemanticDriver` (in
//! `semantic-pane`, wrapping Verdandi's own `terminal-extractor`) each own their own
//! `alacritty_terminal::Term`, fed by the identical byte stream from the identical PTY. Neither of
//! them opens a PTY, spawns a child, or writes to the session except via `TerminalSession` -- the
//! one-child/one-PTY/one-authoritative-terminal invariant is about the PTY and its child, not about
//! how many client-side reconstructions read its output.
//!
//! # Resize has no wire path to a second watcher, so it isn't given one
//!
//! `TerminalStreamEvent` (the wire type both watchers observe) has no "resized" variant -- only
//! `output`/`exited`. So geometry is single-sourced from `raw_pane`'s own `connect_resize` callback
//! (GTK's real, already-reliable report of this pane's cell grid) and fed directly, in-process, to
//! three places from that one call site: the real `TerminalSession::resize`, `raw_state`'s
//! `LiveTerminalDriver::feed_resize`, and `semantic_state`'s `SemanticDriver::feed_resize`. Neither
//! child ever derives geometry independently. Whether a `gtk4::Stack`'s hidden child still receives
//! `connect_resize` while it isn't the visible one is exactly the kind of thing a sandbox pass must
//! confirm -- if it turns out not to, move this same call site to a resize observation on `stack`
//! itself instead (both children are always given the same allocation either way).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use gtk4::glib;
use gtk4::prelude::*;

use semantic_pane::{SemanticDriver, SemanticPane, SemanticSessionProjection, SemanticStreamStatus};
use terminal_pane::{LiveTerminalDriver, TerminalPane};
use terminal_session::{ReplayStart, TerminalOutputEvent, TerminalSession, TerminalSessionError};

/// Matches `agent_panel.rs`'s own `PUMP_POLL_INTERVAL_MS` -- there is no reason for this pane's
/// live-update cadence to differ from the one already tuned for the agent panel's WebView bridge.
const PUMP_POLL_INTERVAL_MS: u64 = 33;

/// A pane measured before GTK has laid it out reports 1x1 (see `terminal_pane`'s own placeholder
/// test) -- too small to be a real starting grid for a session that opens before the window is
/// shown. `OpenTerminal` gets this floor instead; the first real `connect_resize` callback, which
/// fires once GTK actually allocates the pane, immediately corrects it via the same single resize
/// call site every later resize uses.
const BOOTSTRAP_COLS: u16 = 80;
const BOOTSTRAP_ROWS: u16 = 24;

fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

/// Which of the two views is currently showing. Pure, GTK-free decision logic -- unit-testable
/// with no display, unlike the `gtk4::Stack` it ultimately drives -- kept separate from
/// [`TerminalBottomSlot::set_view`] for exactly that reason (this project's established split,
/// e.g. `semantic-pane::layout` vs `semantic-pane::widget`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalView {
    Raw,
    Semantic,
}

impl TerminalView {
    const RAW_NAME: &'static str = "raw";
    const SEMANTIC_NAME: &'static str = "semantic";

    fn stack_child_name(self) -> &'static str {
        match self {
            TerminalView::Raw => Self::RAW_NAME,
            TerminalView::Semantic => Self::SEMANTIC_NAME,
        }
    }

    /// `gtk4::Stack::visible_child_name()`'s return type, mapped back to a `TerminalView`.
    /// Anything other than the literal `"semantic"` name reads as `Raw` -- including `None`
    /// (an unrealized `Stack`, before its first `set_visible_child_name` call) -- because Raw is
    /// this slot's documented default and a decision function should not assume its caller
    /// already set it correctly.
    fn from_stack_child_name(name: Option<&str>) -> Self {
        match name {
            Some(Self::SEMANTIC_NAME) => TerminalView::Semantic,
            _ => TerminalView::Raw,
        }
    }

    fn toggled(self) -> Self {
        match self {
            TerminalView::Raw => TerminalView::Semantic,
            TerminalView::Semantic => TerminalView::Raw,
        }
    }
}

/// Owns the raw view's live-driving state: `terminal-pane`'s `LiveTerminalDriver` plus its own
/// watcher. Never touches `semantic_state` and vice versa -- a bug or disconnect on one side must
/// never reach the other (design spec §4/§20).
struct RawState {
    driver: LiveTerminalDriver,
    watcher: terminal_session::TerminalOutputWatcher,
}

/// Owns the semantic view's live-driving state. `last_rendered_revision` is what makes the pump
/// loop's repaint revision-gated rather than unconditional, mirroring `agent_bridge.rs`'s existing
/// pattern (design spec §4/§21).
struct SemanticState {
    driver: SemanticDriver,
    projection: SemanticSessionProjection,
    watcher: terminal_session::TerminalOutputWatcher,
    last_rendered_revision: u64,
    /// `SemanticDriver` does not expose its own grid size back (it is never asked to render), so
    /// this is tracked alongside it -- kept in sync by the one shared resize call site -- purely so
    /// a later desync-recovery rebuild knows what geometry to reconstruct `SemanticDriver::new`
    /// with, instead of falling back to the bootstrap default and silently losing the real size
    /// until the next actual resize.
    cols: u16,
    rows: u16,
}

/// The bottom-slot handle `main.rs` holds: a `gtk4::Stack` of the two views over one shared
/// `TerminalSession`, plus what focus-routing and window-close need.
pub struct TerminalBottomSlot {
    stack: gtk4::Stack,
    raw_pane: Rc<TerminalPane>,
    semantic_pane: Rc<SemanticPane>,
    /// `None` when `TerminalSession::open` itself failed (no sidecar running) -- the slot still
    /// renders (a static explanatory placeholder in the raw pane, matching this project's existing
    /// convention of a real, live, focusable pane that simply has nothing behind it yet), it just
    /// has nothing to close on window shutdown.
    session: Option<Rc<TerminalSession>>,
    /// The same `SemanticState` the live pump timer mutates every tick -- shared, not copied, so
    /// [`TerminalBottomSlot::semantic_blocks`] always reads the current projection, never a stale
    /// snapshot taken at construction time. `None` exactly when `session` is `None`.
    semantic_state: Option<Rc<RefCell<SemanticState>>>,
}

impl TerminalBottomSlot {
    pub fn widget(&self) -> &gtk4::Widget {
        self.stack.upcast_ref()
    }

    /// Delegates to whichever child is currently visible -- callers (`main.rs`'s Ctrl+j/Ctrl+k
    /// handlers) don't need to know or care which view is showing.
    pub fn grab_focus(&self) {
        match self.current_view() {
            TerminalView::Semantic => self.semantic_pane.grab_focus(),
            TerminalView::Raw => self.raw_pane.grab_focus(),
        }
    }

    /// Which view is currently visible. Side-effect-free.
    pub fn current_view(&self) -> TerminalView {
        TerminalView::from_stack_child_name(self.stack.visible_child_name().as_deref())
    }

    /// THE one authoritative view switch (see this module's own doc comment). Touches only which
    /// `Stack` child is visible -- never the `TerminalSession`, the `WatchHub`, either watcher, or
    /// either driver, so switching views is side-effect-free with respect to the live session by
    /// construction, not by convention that could drift.
    pub fn set_view(&self, view: TerminalView) {
        self.stack.set_visible_child_name(view.stack_child_name());
        println!("[terminal] view -> {view:?}");
    }

    pub fn toggle_view(&self) {
        self.set_view(self.current_view().toggled());
    }

    /// Writes raw bytes to the live session's PTY, if one is open (`None` when
    /// `TerminalSession::open` itself failed -- see this struct's own `session` field doc). Real
    /// keyboard input reaches the same `TerminalSession::write` through the raw pane's own
    /// `connect_input` -> `terminal_input::encode` path in `wire_live_session`; this is the same
    /// underlying call exposed directly, for a caller that already has bytes (a paste, or a real
    /// two-turn conversation driven by a diagnostic harness) and has no `NormalizedInput` to
    /// fabricate. Writing bytes never depends on which view (`current_view()`) is visible -- the
    /// session is shared, not owned by either pane.
    pub fn write_raw(&self, bytes: &[u8]) -> Option<Result<(), TerminalSessionError>> {
        self.session.as_ref().map(|session| session.write(bytes))
    }

    /// A snapshot of the semantic view's current blocks -- structural (not merely visual)
    /// verification: `PresentationBlock::kind`/`confidence`/`as_text()` let a caller (a
    /// diagnostic harness, this crate's own tests) check what the projection actually contains,
    /// including its `BlockKind` and streaming identity (the same block id persisting across an
    /// update, rather than a duplicate appearing), without reading pixels off the `SemanticPane`
    /// widget. `None` exactly when no live session was ever opened. This reads the SAME
    /// `SemanticSessionProjection` the pump timer resyncs every tick and `SemanticPane::
    /// set_projection` renders, not a copy that could drift from what's on screen.
    pub fn semantic_blocks(&self) -> Option<Vec<terminal_semantic::PresentationBlock>> {
        self.semantic_state.as_ref().map(|state| state.borrow().projection.blocks.clone())
    }

    /// Closes the live session, if one was ever opened. Called from `main.rs`'s
    /// `connect_close_request`, the same place `agent_panel_handle.shutdown()` already runs --
    /// window close must not leave an orphaned sidecar-hosted child behind any more than it may
    /// leave an orphaned `claude` process behind.
    pub fn close(&self) {
        if let Some(session) = &self.session {
            if let Err(err) = session.close() {
                eprintln!("[terminal] session close failed: {err}");
            }
        }
    }
}

/// Builds the terminal bottom slot: opens one live session (or degrades to a static placeholder if
/// none is available), builds both views, and wires resize, input, and the live-update pump. The
/// `Ctrl+Shift+S` view toggle is wired by the caller (`main.rs`, as a `gio::SimpleAction` calling
/// `TerminalBottomSlot::toggle_view`) rather than here -- see this module's own doc comment.
pub fn build_terminal_panel(cwd: String) -> (Rc<TerminalBottomSlot>, gtk4::Widget) {
    let raw_pane = Rc::new(TerminalPane::new());
    let semantic_pane = Rc::new(SemanticPane::new());

    let stack = gtk4::Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    stack.add_named(raw_pane.widget(), Some("raw"));
    stack.add_named(semantic_pane.widget(), Some("semantic"));
    stack.set_visible_child_name(TerminalView::Raw.stack_child_name());

    // A pane measured before GTK has laid it out reports 1x1, never 0x0 (confirmed via diagnostic
    // logging while investigating a real rendering issue, see MANUAL_VERIFICATION.md -- `cols == 0`
    // never actually fires; the real pre-layout value silently slipped through this check and was
    // used as the real session's initial geometry until the first live resize corrected it).
    let (cols, rows) = raw_pane.grid_size();
    let (cols, rows) = if cols <= 1 || rows <= 1 { (BOOTSTRAP_COLS, BOOTSTRAP_ROWS) } else { (cols, rows) };

    let (session, semantic_state) = match TerminalSession::open(cwd, cols, rows) {
        Ok(session) => {
            let session = Rc::new(session);
            let semantic_state = wire_live_session(session.clone(), &raw_pane, &semantic_pane, cols, rows);
            (Some(session), semantic_state)
        }
        Err(err) => {
            eprintln!(
                "[terminal] failed to open a live terminal session: {err} -- \
                 the bottom slot renders a static placeholder with no live session behind it"
            );
            raw_pane.set_paint_list(no_session_placeholder(cols, rows, &err.to_string()));
            (None, None)
        }
    };

    let slot = Rc::new(TerminalBottomSlot { stack: stack.clone(), raw_pane, semantic_pane, session, semantic_state });
    let widget: gtk4::Widget = stack.upcast();
    (slot, widget)
}

/// Wires resize/input/the live pump, and returns the semantic side's live `SemanticState` handle
/// so `build_terminal_panel` can store it on `TerminalBottomSlot` for `semantic_blocks()` --
/// `None` only when opening either watcher against the (already successfully opened) session
/// fails, which the raw pane's own placeholder-on-failure convention does not otherwise cover.
fn wire_live_session(
    session: Rc<TerminalSession>,
    raw_pane: &Rc<TerminalPane>,
    semantic_pane: &Rc<SemanticPane>,
    cols: u16,
    rows: u16,
) -> Option<Rc<RefCell<SemanticState>>> {
    let raw_watcher = match session.watch(ReplayStart::FromNow) {
        Ok(watcher) => watcher,
        Err(err) => {
            eprintln!("[terminal] failed to open the raw view's watcher: {err}");
            return None;
        }
    };
    let semantic_watcher = match session.watch(ReplayStart::RetainedHistory) {
        Ok(watcher) => watcher,
        Err(err) => {
            eprintln!("[terminal] failed to open the semantic view's watcher: {err}");
            return None;
        }
    };

    let raw_state = Rc::new(RefCell::new(RawState { driver: LiveTerminalDriver::new(cols, rows), watcher: raw_watcher }));
    let semantic_state = Rc::new(RefCell::new(SemanticState {
        driver: SemanticDriver::new(cols as usize, rows as usize),
        projection: SemanticSessionProjection::new(),
        watcher: semantic_watcher,
        last_rendered_revision: 0,
        cols,
        rows,
    }));

    // --- input: NormalizedInput -> bytes, encoded against the raw view's own live TermMode (the
    // only place in this repository that owns one) -- this is terminal-pane's own doc comment's
    // "one call short of the PTY" call, made.
    {
        let session_for_input = session.clone();
        let raw_state_for_input = raw_state.clone();
        raw_pane.connect_input(move |input| {
            let mode = raw_state_for_input.borrow().driver.term_mode();
            let bytes = terminal_input::encode(&input, mode);
            if let Err(err) = session_for_input.write(&bytes) {
                eprintln!("[terminal] write failed: {err}");
            }
        });
    }

    // --- resize: single geometry source (see this module's own doc comment for why), feeding the
    // real PTY resize and both client-side reconstructions from the one call site.
    {
        let session_for_resize = session.clone();
        let raw_pane_for_resize = raw_pane.clone();
        let raw_state_for_resize = raw_state.clone();
        let semantic_state_for_resize = semantic_state.clone();
        raw_pane.connect_resize(move |cols, rows| {
            if let Err(err) = session_for_resize.resize(cols, rows) {
                eprintln!("[terminal] resize failed: {err}");
            }
            let paint_list = raw_state_for_resize.borrow_mut().driver.feed_resize(cols, rows);
            raw_pane_for_resize.set_paint_list(paint_list);

            let mut semantic = semantic_state_for_resize.borrow_mut();
            semantic.cols = cols;
            semantic.rows = rows;
            let events = semantic.driver.feed_resize(cols as usize, rows as usize, now_ns());
            let current_blocks = semantic.driver.current_blocks().to_vec();
            semantic.projection.resync(&current_blocks, &events);
        });
    }

    // --- the live-update pump: drains both watchers every 33ms, matching agent_panel.rs's own
    // pump cadence. `pump_raw` runs unguarded -- the raw view is this slot's operational
    // fallback and a bug there is meant to be loud. `pump_semantic` runs through
    // `run_isolated`: a panic inside a `glib::timeout_add_local` closure would otherwise unwind
    // straight out through glib's C callback dispatch, which gtk-rs converts to a process abort
    // rather than risk UB -- taking the raw pane down along with the semantic one despite the two
    // being otherwise fully independent (design spec §4/§20's "never reaches the other" claim was
    // aspirational until this wrapping existed). A caught panic rebuilds semantic state fresh,
    // via the same `fresh_semantic_state` helper the Lagged/Disconnected path already uses, so
    // the semantic view can recover on its own rather than repeating the same panic forever.
    {
        let raw_pane_for_pump = raw_pane.clone();
        let semantic_pane_for_pump = semantic_pane.clone();
        let session_for_pump = session.clone();
        let raw_state_for_pump = raw_state.clone();
        let semantic_state_for_pump = semantic_state.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(PUMP_POLL_INTERVAL_MS), move || {
            pump_raw(&session_for_pump, &raw_state_for_pump, &raw_pane_for_pump);

            let (cols, rows) = {
                let s = semantic_state_for_pump.borrow();
                (s.cols, s.rows)
            };
            let session_for_semantic = session_for_pump.clone();
            let semantic_state_for_semantic = semantic_state_for_pump.clone();
            let semantic_pane_for_semantic = semantic_pane_for_pump.clone();
            let panicked = run_isolated(
                "semantic pump",
                std::panic::AssertUnwindSafe(move || {
                    pump_semantic(&session_for_semantic, &semantic_state_for_semantic, &semantic_pane_for_semantic);
                }),
            );
            if panicked {
                eprintln!("[terminal] semantic view recovering after an isolated panic -- the raw terminal was unaffected");
                match fresh_semantic_state(&session_for_pump, cols, rows) {
                    Ok(fresh) => *semantic_state_for_pump.borrow_mut() = fresh,
                    Err(err) => eprintln!("[terminal] could not reopen the semantic watcher after a panic: {err}"),
                }
            }

            glib::ControlFlow::Continue
        });
    }

    Some(semantic_state)
}

/// Runs `f`, catching (and logging) any panic instead of letting it propagate. Returns whether
/// `f` panicked, so the caller can decide what recovery (if any) to run. Pure and GTK-free --
/// see `tests::a_panic_inside_run_isolated_is_caught_and_reported_not_propagated` for the direct
/// proof this actually isolates, with no live session or display required.
fn run_isolated<F: FnOnce() + std::panic::UnwindSafe>(context: &str, f: F) -> bool {
    match std::panic::catch_unwind(f) {
        Ok(()) => false,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".to_string());
            eprintln!("[terminal] {context} panicked and was isolated: {message}");
            true
        }
    }
}

/// A brand-new `SemanticState` against a freshly reopened `RetainedHistory` watch on the same
/// session -- shared by the Lagged/Disconnected recovery path inside `pump_semantic` and the
/// panic-recovery path in the pump timer above, so there is exactly one "start the semantic view
/// over" implementation. Never opens a second `TerminalSession` or a second real upstream watch
/// (`session.watch` always subscribes to the one `WatchHub` this slot's session already owns).
fn fresh_semantic_state(
    session: &Rc<TerminalSession>,
    cols: u16,
    rows: u16,
) -> Result<SemanticState, terminal_session::TerminalSessionError> {
    let watcher = session.watch(ReplayStart::RetainedHistory)?;
    Ok(SemanticState {
        driver: SemanticDriver::new(cols as usize, rows as usize),
        projection: SemanticSessionProjection::new(),
        watcher,
        last_rendered_revision: 0,
        cols,
        rows,
    })
}

/// Drains the raw watcher this tick. A `Disconnected` (the realistic failure mode -- `Lagged`
/// should not occur at all for a `ReplayStart::FromNow` watcher, which never fails on replay
/// grounds) rebuilds a fresh driver against a fresh `RetainedHistory` watcher rather than trying to
/// patch a `Term` that may have missed bytes -- the same reasoning `SemanticStreamStatus::Desynced`
/// recovery uses on the semantic side (design spec §4), applied here for the identical reason: a
/// dropped chunk permanently desyncs whichever `Term` missed it.
fn pump_raw(session: &Rc<TerminalSession>, state: &Rc<RefCell<RawState>>, pane: &Rc<TerminalPane>) {
    loop {
        let event = state.borrow_mut().watcher.try_recv();
        let Some(event) = event else { break };
        match event {
            TerminalOutputEvent::Chunk { data, .. } => {
                if let Some(paint_list) = state.borrow_mut().driver.feed_output(&data) {
                    pane.set_paint_list(paint_list);
                }
            }
            TerminalOutputEvent::Exited { code, signal } => {
                let (cols, rows) = state.borrow().driver.grid_size();
                pane.set_paint_list(session_ended_placeholder(cols, rows, code, signal));
                break;
            }
            TerminalOutputEvent::Lagged | TerminalOutputEvent::Disconnected => {
                eprintln!("[terminal] raw watcher {event:?} -- rebuilding from retained history");
                let (cols, rows) = state.borrow().driver.grid_size();
                match session.watch(ReplayStart::RetainedHistory) {
                    Ok(watcher) => {
                        *state.borrow_mut() = RawState { driver: LiveTerminalDriver::new(cols, rows), watcher };
                    }
                    Err(err) => eprintln!("[terminal] could not reopen the raw watcher: {err}"),
                }
                break;
            }
        }
    }
}

/// Same shape as `pump_raw`, for the semantic side. Repaint is revision-gated (`resync` bumps
/// `last_revision` once per event; only a real change triggers `set_projection`), matching
/// `agent_bridge.rs`'s existing pattern rather than repainting unconditionally every tick.
fn pump_semantic(session: &Rc<TerminalSession>, state: &Rc<RefCell<SemanticState>>, pane: &Rc<SemanticPane>) {
    let mut all_events = Vec::new();
    let mut terminal_event = None;
    loop {
        let event = state.borrow_mut().watcher.try_recv();
        let Some(event) = event else { break };
        match event {
            TerminalOutputEvent::Chunk { data, .. } => {
                let events = state.borrow_mut().driver.feed_output(&data, now_ns());
                all_events.extend(events);
            }
            other => {
                terminal_event = Some(other);
                break;
            }
        }
    }

    if !all_events.is_empty() {
        let mut s = state.borrow_mut();
        let current_blocks = s.driver.current_blocks().to_vec();
        s.projection.resync(&current_blocks, &all_events);
    }

    match terminal_event {
        Some(TerminalOutputEvent::Exited { .. }) => {
            let mut s = state.borrow_mut();
            let events = s.driver.finish(now_ns());
            let current_blocks = s.driver.current_blocks().to_vec();
            s.projection.resync(&current_blocks, &events);
            s.projection.mark_stream_status(SemanticStreamStatus::Disconnected { reason: "the terminal session exited".into() });
        }
        Some(TerminalOutputEvent::Lagged) | Some(TerminalOutputEvent::Disconnected) => {
            eprintln!("[terminal] semantic watcher desynced -- rebuilding from retained history");
            let (cols, rows) = {
                let s = state.borrow();
                (s.cols, s.rows)
            };
            match fresh_semantic_state(session, cols, rows) {
                Ok(fresh) => *state.borrow_mut() = fresh,
                Err(err) => eprintln!("[terminal] could not reopen the semantic watcher: {err}"),
            }
        }
        _ => {}
    }

    let mut s = state.borrow_mut();
    if s.projection.last_revision != s.last_rendered_revision {
        s.last_rendered_revision = s.projection.last_revision;
        pane.set_projection(&s.projection);
    }
}

/// The "no live session" frame -- shown only when `TerminalSession::open` itself failed (no
/// sidecar running at all). Distinct from `session_ended_placeholder`: this one means "never
/// connected", that one means "connected, then the session ended".
fn no_session_placeholder(cols: u16, rows: u16, reason: &str) -> terminal_render::PaintList {
    banner_placeholder(cols, rows, "neovibe terminal", &format!("no live session: {reason}"))
}

fn session_ended_placeholder(cols: u16, rows: u16, code: i32, signal: Option<i32>) -> terminal_render::PaintList {
    let detail = match signal {
        Some(sig) => format!("session ended (exit code {code}, signal {sig})"),
        None => format!("session ended (exit code {code})"),
    };
    banner_placeholder(cols, rows, "neovibe terminal", &detail)
}

fn banner_placeholder(cols: u16, rows: u16, title: &str, detail: &str) -> terminal_render::PaintList {
    use terminal_render::{CursorShape, GlyphStyle, PaintList, PaintOp, RgbColor, UnderlineKind};

    const BG: RgbColor = RgbColor::new(0x14, 0x16, 0x1c);
    const FG: RgbColor = RgbColor::new(0x9a, 0xa5, 0xb4);
    const ACCENT: RgbColor = RgbColor::new(0x8a, 0xbe, 0xb7);

    let lines: [(&str, RgbColor, bool); 2] = [(title, ACCENT, true), (detail, FG, false)];
    let mut ops = Vec::new();
    for row in 0..rows {
        ops.push(PaintOp::FillCells { row, col: 0, cols, color: BG });
    }
    for (index, (text, color, bold)) in lines.iter().enumerate() {
        let row = index as u16 + 1;
        if row >= rows {
            break;
        }
        for (col, ch) in (2u16..).zip(text.chars()) {
            if col >= cols {
                break;
            }
            ops.push(PaintOp::DrawText {
                row,
                col,
                cols: 1,
                text: ch.to_string(),
                color: *color,
                style: GlyphStyle { bold: *bold, italic: false, underline: UnderlineKind::None, underline_color: *color, strikeout: false },
            });
        }
    }
    if rows > 5 {
        ops.push(PaintOp::DrawCursor { row: 5, col: 2, cols: 1, shape: CursorShape::HollowBlock, color: FG, text_under: None, blinking: false });
    }
    PaintList { ops, cols, rows, surface_background: BG, top_line: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal_render::PaintOp;

    // --- TerminalView: pure, GTK-free, no display required (unlike the Stack it ultimately
    // drives) -- this is the exact decision logic `TerminalBottomSlot::set_view`/`toggle_view`/
    // `current_view` delegate to.

    #[test]
    fn raw_is_the_default_for_any_name_other_than_the_literal_semantic_string() {
        assert_eq!(TerminalView::from_stack_child_name(None), TerminalView::Raw);
        assert_eq!(TerminalView::from_stack_child_name(Some("raw")), TerminalView::Raw);
        assert_eq!(TerminalView::from_stack_child_name(Some("bogus")), TerminalView::Raw);
    }

    #[test]
    fn semantic_is_recognised_only_by_its_exact_stack_child_name() {
        assert_eq!(TerminalView::from_stack_child_name(Some("semantic")), TerminalView::Semantic);
    }

    #[test]
    fn stack_child_name_and_from_stack_child_name_round_trip() {
        for view in [TerminalView::Raw, TerminalView::Semantic] {
            assert_eq!(TerminalView::from_stack_child_name(Some(view.stack_child_name())), view);
        }
    }

    #[test]
    fn toggled_swaps_and_toggling_twice_returns_to_the_start() {
        assert_eq!(TerminalView::Raw.toggled(), TerminalView::Semantic);
        assert_eq!(TerminalView::Semantic.toggled(), TerminalView::Raw);
        assert_eq!(TerminalView::Raw.toggled().toggled(), TerminalView::Raw);
    }

    // --- run_isolated: the panic-isolation primitive `pump_semantic`'s wrapping in the live pump
    // depends on. Also pure and GTK-free -- proves the isolation actually catches a panic rather
    // than propagating it, with no live session required to exercise it.

    #[test]
    fn a_panic_inside_run_isolated_is_caught_and_reported_not_propagated() {
        let panicked = run_isolated(
            "test",
            std::panic::AssertUnwindSafe(|| panic!("synthetic failure for the isolation test")),
        );
        assert!(panicked, "run_isolated must report that the closure panicked");
        // Reaching this line at all is the real assertion: a caught panic did not propagate out
        // of `run_isolated` and abort this test process.
    }

    #[test]
    fn a_non_panicking_closure_reports_no_panic() {
        let panicked = run_isolated("test", std::panic::AssertUnwindSafe(|| {}));
        assert!(!panicked);
    }

    #[test]
    fn the_no_session_placeholder_is_layer_ordered() {
        assert!(no_session_placeholder(80, 24, "no sidecar listening").is_layer_ordered());
    }

    #[test]
    fn the_session_ended_placeholder_is_layer_ordered() {
        assert!(session_ended_placeholder(80, 24, 0, None).is_layer_ordered());
        assert!(session_ended_placeholder(80, 24, 1, Some(9)).is_layer_ordered());
    }

    #[test]
    fn a_one_cell_grid_still_produces_a_valid_frame() {
        let list = no_session_placeholder(1, 1, "x");
        assert!(list.is_layer_ordered());
        for op in &list.ops {
            let (row, col, cols) = match op {
                PaintOp::FillCells { row, col, cols, .. } => (*row, *col, *cols),
                PaintOp::DrawText { row, col, cols, .. } => (*row, *col, *cols),
                PaintOp::DrawCursor { row, col, cols, .. } => (*row, *col, *cols),
                PaintOp::DrawNotice { row, col, cols, .. } => (*row, *col, *cols),
            };
            assert!(row < list.rows, "row {row} outside {}", list.rows);
            assert!(col + cols <= list.cols, "col {col}+{cols} outside {}", list.cols);
        }
    }
}
