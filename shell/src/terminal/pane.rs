//! The bottom terminal's widget: a `GtkGLArea` that paints the session's latest frame
//! and handles keyboard, click, focus and resize events. The pane keeps the latest `PaintList`
//! and the GL surface. Bytes, parsing, encoding and rendering belong to the session thread.
//!
//! `start` requests a shell; the session starts at the first allocation with area. The pane
//! keeps that allocation in [`State::allocation`] and builds metrics at that size. The first
//! `resize` can arrive before metrics exist, and GTK sends no second resize for the same
//! allocation. Using a 1x1 placeholder would leave the first shell nearly blank until a resize.
//!
//! The session's waker sends on a channel awaited by `glib::spawn_future_local` on the main
//! loop. A 33 ms poll would add up to 33 ms to each echo. An idle terminal needs no wake-ups.
//! An unmapped pane tells the session it is hidden, so hidden output produces no renders.
//!
//! If `$SHELL` cannot start, the pane tries the passwd entry's shell and then `/bin/sh`.
//! If all fail, it shows a failure frame and waits for Enter instead of retrying on resize.
//!
//! `ime.rs` attaches a `GtkIMMulticontext` to the key controller. Commits go through [`submit`],
//! and unfinished composition is drawn at `Update::cursor`. The input method's caret is updated
//! on preedit changes, focus gain and cursor movement while focused. GTK can focus the input
//! method out before the pane receives its focus notification, and that can emit a commit.
//! [`connect_ime`] checks GTK's current focus so composition interrupted by focus loss is dropped.
//! [`TerminalPane::start`] sets the client widget, so an unused terminal creates no input client.
//! No `RefCell` borrow spans `reset`, `set_cursor_location` or `set_client_widget`: input-method
//! calls can emit `commit` or `preedit-changed` synchronously, and those handlers borrow state.
//!
//! Typed keys, prefix literals, clipboard pastes and input-method commits all use [`submit`].
//! The clipboard read is asynchronous; `input_queue` holds later typing behind it so Enter
//! cannot overtake the text it runs.
//!
//! A middle click pastes the primary selection through `paste_primary` and the same input queue.
//! `connect_pointer` maps drag motion and double/triple clicks to `SelectCommand`s using
//! `pointer::cell_at` and `kind_for`. On release, `pump` copies the finished selection to both
//! the clipboard and primary selection. `copy_selection` copies `State::last_selection` again
//! for `Ctrl+Shift+C` without another session round trip.
//!
//! OSC 52 copies go to GTK's clipboard (`c`) or primary selection (`p`/`s`), even while hidden.
//! `Term` refuses clipboard reads. A bell briefly tints the pane through `bell.rs`; a bell rung
//! while hidden does not flash when the pane returns. Program titles have no pane header to use.
//!
//! `TerminalMetrics` is built lazily by [`ensure_session`], so an unused terminal never touches
//! a font manager. If no usable monospace font exists, the pane shows its failure frame instead
//! of panicking the process.

use std::cell::RefCell;
use std::panic;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::gdk::{Clipboard, ModifierType, Rectangle};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    EventControllerFocus, EventControllerKey, EventControllerScroll, EventControllerScrollFlags, GLArea, GestureClick,
    GestureDrag, IMMulticontext, InputPurpose,
};

use eitri_terminal::mouse::{Button as MouseButton, MouseKind, MouseModes, MouseMods, WheelDir};
use eitri_terminal::pty::{fallback_shell, passwd_shell};
use eitri_terminal::{
    layout_preedit, CursorCell, PreeditLayout, PtySize, Screen, ScrollRequest, SelectCommand, SessionCommand,
    SessionConfig, SpawnSpec, TerminalColors, TerminalMetrics, TerminalSession,
};
use terminal_input::NormalizedInput;
use terminal_render::PaintList;

use super::bell::{self, BellFlash};
use super::gl::SkiaState;
use super::ime::{im_cursor_rect, Commit, FocusChange, GtkFocus, ImeGate};
use super::input_queue::{InputQueue, PaneInput, PasteTicket};
use super::keys::{self, ClipboardChord, CopyModeKey, DeliveredKeys, RawKey, RepeatTracker};
use super::pointer;
use crate::wheel_zoom::WheelZoom;

/// GDK's own numbering (X11/Wayland convention, not named constants in `gtk4-rs`): button 1 is the
/// primary (left) button, button 2 the middle.
const BUTTON_PRIMARY: u32 = 1;
const BUTTON_MIDDLE: u32 = 2;

/// Phase 1's face: the frozen pane's own default. Phase 4 takes nvim's `guifont`.
const FONT_FAMILY: &str = "monospace";
const FONT_SIZE_PT: f32 = 13.0;

/// How long `Ctrl+Shift+V` waits for the clipboard before typing goes on without the paste
/// (`input_queue`'s module doc). A read from this process takes microseconds and one through a
/// Wayland pipe milliseconds; two seconds is a clipboard owner that is not answering.
pub(crate) const PASTE_TIMEOUT: Duration = Duration::from_secs(2);

struct State {
    skia: Option<SkiaState>,
    /// `None` until [`ensure_session`] is about to spawn a shell. Delaying metrics construction
    /// avoids a font manager and fontconfig match for windows whose terminal is never shown.
    metrics: Option<TerminalMetrics>,
    /// The last allocation with area the GLArea was given; `None` until the first. What the
    /// metrics are built at when a shell is first wanted (GUI pass 2026-09-23, defect 1).
    allocation: Option<Allocation>,
    /// A shell is wanted (`start` was called). Spawned once there is an `allocation`.
    wanted: bool,
    session: Option<TerminalSession>,
    /// The shell exited, OR the metrics/spawn failed before one ever started: the last frame (or
    /// the failure message) stays, and Enter restarts it.
    exited: bool,
    frame: Option<PaintList>,
    /// Where the cursor is on `frame`, as the session reported it with that frame: where a
    /// composition is drawn (phase 2). `None` is "unknown" -- before the first frame, with a frame
    /// that carried none, and after a restart -- and nothing is drawn at a stale cell then.
    cursor: Option<CursorCell>,
    /// The input method's composition and what of it may reach the shell (phase 2).
    ime: ImeGate,
    repeats: RepeatTracker,
    /// Keyvals whose presses this pane forwarded, used to forward only their matching releases.
    delivered: DeliveredKeys,
    /// What is bound for the shell and waits behind a paste still being read (phase 2).
    queue: InputQueue,
    /// The bell's flash (phase 2).
    bell: BellFlash,
    /// The last selection a drag finished: kept host-side so `Ctrl+Shift+C`
    /// can copy it again without a round trip through the session (`copy_selection`'s own doc).
    /// Cleared on a restart, the same as `cursor`.
    last_selection: Option<String>,
    /// The live mouse-mode summary, as the session last reported it (bottom-terminal phase 3c):
    /// what [`pointer::route`]/[`pointer::wheel_route`] need to decide, *before* sending, whether a
    /// gesture is the program's or this pane's own. Reset to [`MouseModes::default`] (report off) on
    /// a restart, the same as `cursor`.
    mouse: MouseModes,
    /// Which button (if any) a drag or motion is holding, for the live mouse mode's own report
    ///. Reset on a restart: a button held across a restarted shell reports nothing new.
    reports: pointer::ReportTracker,
    /// `prefix [`/`prefix PageUp` opened copy mode: `connect_keyboard` reads
    /// this ahead of `scroll_chord`, and the input method's commit and preedit handlers read it too
    /// ([`Self::route_commit`], [`Self::take_preedit`]) -- that is where every printable key
    /// arrives -- so a copy-mode key never reaches the shell. Reset on exit or
    /// restart (`forget_session`), the same as `cursor`/`last_selection` -- a shell that is gone or
    /// about to be replaced holds nothing left to scroll back through.
    copy_mode: bool,
    /// The last widget-relative pointer position: a wheel event carries no position of its
    /// own, so a report needs the last one the motion controller (or drag gesture) saw.
    last_pointer: (f64, f64),
    colors: TerminalColors,
    focused: bool,
    cwd: PathBuf,
    /// Told when the shell ends in a way that closes the module (`super::closes_on_exit`); set by
    /// `main.rs` ([`TerminalPane::on_shell_exit`]). Called with no borrow of this state held.
    on_shell_exit: Option<Rc<dyn Fn() -> bool>>,
}

/// What becomes of an input-method commit ([`State::route_commit`]).
#[derive(Debug, PartialEq)]
enum CommitRoute {
    /// To the shell, through the pane's one door ([`submit`]).
    Shell(NormalizedInput),
    /// Copy mode is open: the commit is a copy-mode key or nothing, and never the shell's.
    CopyMode(CopyModeKey),
    /// Nothing at all; the gate's reason, for the log.
    Dropped(&'static str),
}

impl State {
    /// Constructs the state used by `TerminalPane::new`. GTK fields start as `None`, so tests can
    /// check the initial state, including absent metrics, without a display or `gtk4::init`.
    fn new(cwd: PathBuf) -> Self {
        State {
            skia: None,
            metrics: None,
            allocation: None,
            wanted: false,
            session: None,
            exited: false,
            frame: None,
            cursor: None,
            ime: ImeGate::new(),
            repeats: RepeatTracker::new(),
            delivered: DeliveredKeys::new(),
            queue: InputQueue::new(),
            bell: BellFlash::new(),
            last_selection: None,
            mouse: MouseModes::default(),
            reports: pointer::ReportTracker::default(),
            copy_mode: false,
            last_pointer: (0.0, 0.0),
            colors: TerminalColors::default(),
            focused: false,
            cwd,
            on_shell_exit: None,
        }
    }

    /// `connect_resize`'s bookkeeping, GTK-free so it is tested without a display: keep the
    /// allocation, and size the metrics to it if there are any yet. Returns the grid the PTY must
    /// be told about, when it changed.
    fn allocate(&mut self, allocation: Allocation) -> Option<PtySize> {
        self.allocation = Some(allocation);
        // `None` until a shell has actually been wanted (`spawn_size` builds it): there is nothing
        // to resize yet, and no PTY to tell. The allocation kept above is what it will be built at.
        let metrics = self.metrics.as_mut()?;
        let changed = if (metrics.scale() - allocation.scale).abs() > f32::EPSILON {
            *metrics = metrics.rescale(allocation.width, allocation.height, allocation.scale);
            true
        } else {
            metrics.resize(allocation.width, allocation.height)
        };
        changed.then(|| PtySize::from_metrics(metrics))
    }

    /// The composition laid out over the latest frame, at the cursor the session reported with it.
    /// `None` with no composition, or while the cursor is unknown.
    fn preedit_layout(&self) -> Option<PreeditLayout> {
        let preedit = self.ime.preedit()?;
        let frame = self.frame.as_ref()?;
        layout_preedit(
            &preedit.text,
            preedit.caret,
            self.cursor?,
            frame.cols,
            frame.rows,
            &self.colors,
        )
    }

    /// The cell the input method's candidate window belongs at: the composition's caret while there
    /// is one, else the cursor. `None` while the cursor is unknown.
    fn ime_caret(&self) -> Option<(u16, u16)> {
        if let Some(layout) = self.preedit_layout() {
            return Some((layout.caret_row, layout.caret_col));
        }
        let cursor = self.cursor?;
        Some((cursor.row, cursor.col))
    }

    /// [`Self::ime_caret`] as the rectangle `set_cursor_location` wants. `None` also before the
    /// metrics exist.
    fn ime_rect(&self, scale_factor: i32) -> Option<Rectangle> {
        let (row, col) = self.ime_caret()?;
        let metrics = self.metrics.as_ref()?;
        Some(im_cursor_rect(metrics.cell_rect(row, col, 1), scale_factor))
    }

    /// Stores the cursor reported with a frame. A frame without a cursor, such as one from a
    /// contained panic, clears the old cell. Returns `true` when the cursor moved while focused
    /// and the input method must be pointed again. fcitx5 places its first candidate window
    /// before `preedit-*`, so the location must be set on gaining focus and on cursor movement.
    fn take_cursor(&mut self, cursor: Option<CursorCell>) -> bool {
        let cell = |cursor: Option<CursorCell>| cursor.map(|c| (c.row, c.col));
        let moved = cell(cursor) != cell(self.cursor);
        self.cursor = cursor;
        moved && cursor.is_some() && self.focused
    }

    /// A bell the session reported. It flashes only a pane that is on screen: a bell rung while the
    /// terminal was hidden is still pending when it is shown again, and flashing it then would be a
    /// flash for something long over ([`connect_visibility`] takes it with `on_screen: false`).
    fn bell(&mut self, on_screen: bool, now: Instant) -> Option<Instant> {
        if on_screen {
            self.bell.ring(now)
        } else {
            None
        }
    }

    /// Straight to the running shell; dropped when there is none (not started, or exited). The one
    /// place that turns a [`PaneInput`] into the matching [`SessionCommand`] (bottom-terminal phase
    /// 3c): a key is `Input`, a mouse gesture `Mouse`, a focus change `FocusReport` -- never the
    /// direct `SessionCommand::Focus` a report must not ride (`set_focused`'s own doc).
    fn send_input(&self, input: PaneInput) {
        if let (Some(session), false) = (&self.session, self.exited) {
            let command = match input {
                PaneInput::Key(input) => SessionCommand::Input(input),
                PaneInput::Mouse(m) => SessionCommand::Mouse(m),
                PaneInput::Focus(focused) => SessionCommand::FocusReport(focused),
            };
            session.send(command);
        }
    }

    /// One command to the running shell's session, dropped when there is none. Callers write
    /// `state.borrow().send_command(..)`, so no `if let` scrutinee has to hold the pane's `RefCell`
    /// borrow across the send.
    fn send_command(&self, command: SessionCommand) {
        if let Some(session) = &self.session {
            session.send(command);
        }
    }

    /// Ends one paste and sends what it was holding back. `false`: the paste had already been given
    /// up on (the timeout, or a restart), and its text is not sent.
    fn finish_paste(&mut self, ticket: PasteTicket, input: Option<NormalizedInput>) -> bool {
        match self.queue.finish(ticket, input) {
            Some(ready) => {
                for input in ready {
                    self.send_input(input);
                }
                true
            }
            None => false,
        }
    }

    /// Enter after the shell exited: the old session goes, and so does anything still waiting to
    /// reach it behind a paste, and where its cursor was (the new shell's first frame says).
    fn forget_session(&mut self) {
        self.session = None;
        self.exited = false;
        self.queue.clear();
        self.cursor = None;
        self.last_selection = None;
        self.mouse = MouseModes::default();
        self.reports = pointer::ReportTracker::default();
        // Leave copy mode on exit or restart: the old session has no remaining scrollback to use.
        self.copy_mode = false;
    }

    /// `prefix [`/`prefix PageUp`, once `main.rs` has already established this
    /// pane holds the keys (`super::copy_mode_entry`): does nothing without a running shell -- a
    /// prompt or a message frame with none holds no copy to scroll back through. `up` also asks for
    /// one page up at once, the one difference between `prefix [` and `prefix PageUp`.
    fn enter_copy_mode(&mut self, up: bool) {
        if self.exited {
            return;
        }
        let Some(session) = self.session.as_ref() else { return };
        session.send(SessionCommand::CopyMode(true));
        if up {
            session.send(SessionCommand::Scroll(ScrollRequest::Pages(1)));
        }
        self.copy_mode = true;
    }

    /// One copy-mode key, from either door it can arrive by: `key-pressed` (the arrows, the page keys,
    /// `Ctrl+u`/`Ctrl+d`, `Esc`) or an input-method commit (every printable key, [`Self::route_commit`]).
    /// Nothing here reaches the shell: a scroll goes to the view, a leave snaps it back to the bottom.
    fn apply_copy_mode_key(&mut self, key: CopyModeKey) {
        match key {
            CopyModeKey::Scroll(req) => {
                if let Some(session) = &self.session {
                    session.send(SessionCommand::Scroll(req));
                }
            }
            CopyModeKey::Leave => {
                if let Some(session) = &self.session {
                    session.send(SessionCommand::CopyMode(false));
                }
                self.copy_mode = false;
            }
            CopyModeKey::Swallow => {}
        }
    }

    /// Where an input-method commit goes. The gate decides first, as ever (`ImeGate::commit`: the
    /// keys, a reset of the pane's own, an empty commit). What it would send goes to the shell --
    /// **except in copy mode**, where it is a copy-mode key or nothing (`keys::copy_mode_commit`):
    /// with an input method attached a printable key arrives only as a commit (`ime.rs`), so without
    /// this `k`, `j`, `g`, `G`, `q` and every other letter would be typed into the shell underneath
    ///.
    fn route_commit(&mut self, text: &str, gtk: GtkFocus) -> CommitRoute {
        match self.ime.commit(text, gtk) {
            Commit::Dropped(why) => CommitRoute::Dropped(why),
            Commit::Send(_) if self.copy_mode => CommitRoute::CopyMode(keys::copy_mode_commit(text)),
            Commit::Send(input) => CommitRoute::Shell(input),
        }
    }

    /// A composition started, changed or ended ([`preedit_changed`]'s half that needs no display).
    /// `true`: it must be thrown away -- copy mode has no composition to draw and nothing to commit it
    /// to, so a composition that starts there (rime on, and a letter pressed) is never kept, and the
    /// caller resets the input method. The letter that started it does nothing: with rime composing,
    /// the key was the input method's, as it would be in any terminal under tmux's copy mode.
    fn take_preedit(&mut self, text: &str, caret: i32) -> bool {
        if self.copy_mode {
            self.ime.preedit_changed("", 0);
            return !text.is_empty();
        }
        self.ime.preedit_changed(text, caret);
        false
    }

    /// What [`pointer::route`]/[`pointer::wheel_route`] see: the live mode while a shell is running,
    /// or [`MouseModes::default`] (report off) once it has exited or never started -- a program that
    /// died in mouse mode (`kill -9 htop`) must not leave its final screen unselectable.
    fn routing_modes(&self) -> MouseModes {
        if self.exited || self.session.is_none() {
            MouseModes::default()
        } else {
            self.mouse
        }
    }

    /// [`TerminalPane::close`]'s half that needs no display: no shell wanted, none kept, nothing of
    /// the last one left on screen. The metrics and the allocation stay: they are the pane's, not
    /// the shell's, and the next start spawns at them.
    fn close(&mut self) {
        self.wanted = false;
        self.forget_session();
        self.frame = None;
    }

    /// [`ensure_session`]'s gate, and the size it spawns at. `None`: nothing to start (no shell
    /// wanted, no allocation yet, one already running, or an exited one waiting for Enter).
    /// `Some(Err)`: the font failed. Otherwise the metrics exist -- built by `build` at the kept
    /// allocation the first time -- and the size is theirs.
    fn spawn_size(
        &mut self,
        build: impl FnOnce(Allocation) -> Result<TerminalMetrics, String>,
    ) -> Option<Result<PtySize, String>> {
        if !self.wanted || self.session.is_some() || self.exited {
            return None;
        }
        let allocation = self.allocation?;
        let metrics = match self.metrics.take() {
            Some(metrics) => metrics,
            None => match build(allocation) {
                Ok(metrics) => metrics,
                Err(reason) => return Some(Err(reason)),
            },
        };
        let size = PtySize::from_metrics(&metrics);
        self.metrics = Some(metrics);
        Some(Ok(size))
    }
}

/// One allocation of the GLArea, as its `resize` signal gives it: DEVICE pixels, and the scale
/// they are at -- what [`TerminalMetrics`] is built and resized with.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Allocation {
    width: f32,
    height: f32,
    scale: f32,
}

/// Cheap to clone: a handle on one widget and its state.
#[derive(Clone)]
pub(crate) struct TerminalPane {
    area: GLArea,
    state: Rc<RefCell<State>>,
    /// The input method (phase 2): `area`'s key controller runs it, and its client widget is `area`
    /// from the first [`Self::start`].
    im: IMMulticontext,
}

impl TerminalPane {
    /// The pane for a terminal whose shell will run in `cwd` (the resolved project root).
    pub(crate) fn new(cwd: PathBuf) -> Self {
        let area = GLArea::builder()
            .hexpand(true)
            .vexpand(true)
            // Both must match what `SkiaState::ensure_surface` tells Skia: stencil 8, no MSAA.
            .has_stencil_buffer(true)
            .auto_render(false)
            .focusable(true)
            .can_focus(true)
            .build();
        area.add_css_class("terminal");
        let state = Rc::new(RefCell::new(State::new(cwd)));
        // Its client widget is set at the first `start`, not here: that is what creates the input
        // method's own client (for fcitx5-gtk, a D-Bus client and an input window), and a window
        // whose terminal is never shown must not pay for one. Neither this nor the purpose does.
        let im = IMMulticontext::new();
        // What VTE tells the input method too: a terminal, where any character and the control
        // codes are input. (fcitx5-gtk has no terminal case and treats it as free form.)
        im.set_input_purpose(InputPurpose::Terminal);
        connect_resize(&area, &state, &im);
        connect_scale_change(&area);
        connect_render(&area, &state);
        connect_keyboard(&area, &state, &im);
        connect_ime(&area, &state, &im);
        connect_pointer(&area, &state, &im);
        connect_scroll(&area, &state);
        connect_visibility(&area, &state, &im);
        TerminalPane { area, state, im }
    }

    /// The module host that `main.rs` adds to the grid. Keys reach it through `focus_module`,
    /// which refuses hidden modules, rather than through the pane's own focus call.
    pub(crate) fn widget(&self) -> &GLArea {
        &self.area
    }

    /// A shell is wanted: spawn it now if the pane has been laid out, else at its first allocation.
    /// Also where the input method gets its client widget, once (GTK ignores the same widget again;
    /// set late, `GtkIMMulticontext` rebuilds its delegate with it, focus included).
    pub(crate) fn start(&self) {
        self.im.set_client_widget(Some(&self.area));
        self.state.borrow_mut().wanted = true;
        ensure_session(&self.state, &self.area, &self.im);
    }

    /// Whether the pane holds the keys: a solid cursor or a hollow one (`pane_focus`'s rule).
    ///
    /// **This is not what focuses the input method in or out: GTK's key controller did, before
    /// `pane_focus` could report** (`ime.rs`'s module doc), and a composition fcitx5 committed on the
    /// way out was dropped by the commit handler asking GTK, not by this. What is left here: gaining
    /// the keys points the input method at the cursor before the first key; losing them resets it,
    /// which discards what survives a focus-out in the client (a dead-key sequence in GTK's simple
    /// context or fcitx5-gtk's xkb compose state, `fcitximcontext.cpp:1113-1130`; the gate is
    /// already closed, so nothing that reset commits passes), and takes the composition off the
    /// screen.
    ///
    /// `SessionCommand::Focus` goes straight to the session, beside [`submit`], because it changes
    /// only the cursor's shape: no byte reaches the program. **Phase 3's focus reports (DECSET 1004)
    /// do not ride it that way:** a report is input, and one sent here would overtake keys still
    /// waiting behind a paste -- so this method also
    /// submits a [`PaneInput::Focus`] through the door, once the borrow above is released.
    pub(crate) fn set_focused(&self, focused: bool) {
        let (was, change) = {
            let mut state = self.state.borrow_mut();
            let was = state.focused;
            state.focused = focused;
            if let Some(session) = &state.session {
                session.send(SessionCommand::Focus(focused));
            }
            (was, state.ime.set_focused(focused))
        };
        // Submit the focus report after dropping the borrow above: `submit` mutably borrows `state`,
        // and a nested borrow would panic the GTK thread. `pane_focus.rs` already suppresses duplicate
        // reports, so `was != focused` is an additional guard.
        if was != focused {
            submit(&self.state, PaneInput::Focus(focused));
        }
        match change {
            FocusChange::In => point_input_method(&self.im, &self.state, &self.area),
            FocusChange::Out => {
                self.im.reset();
                self.area.queue_render();
            }
            FocusChange::Unchanged => {}
        }
    }

    pub(crate) fn set_colors(&self, colors: TerminalColors) {
        let mut state = self.state.borrow_mut();
        state.colors = colors;
        if let Some(session) = &state.session {
            session.send(SessionCommand::SetColors(colors));
        }
        drop(state);
        self.area.queue_render();
    }

    /// Hands the running shell input that did not come from this widget's own keyboard -- the
    /// prefix's literal `Ctrl+a`/`Ctrl+l`. Dropped when no shell is running; behind a paste still
    /// being read, it waits for it ([`submit`]).
    pub(crate) fn send(&self, input: NormalizedInput) {
        discard_composition(&self.state, &self.im, &self.area);
        submit(&self.state, PaneInput::Key(input));
    }

    /// `prefix [`/`prefix PageUp`: `main.rs`'s own prefix arm calls this only
    /// once `super::copy_mode_entry` has already said this pane holds the keys.
    ///
    /// A composition in progress is thrown away first, as a paste or a `Ctrl+a` literal throws it
    /// away: copy mode has nowhere to commit it, and it must not reach the shell once copy mode ends.
    pub(crate) fn enter_copy_mode(&self, up: bool) {
        discard_composition(&self.state, &self.im, &self.area);
        self.state.borrow_mut().enter_copy_mode(up);
    }

    /// `f` runs when the shell ends by itself in a way that closes the module (`super::closes_on_exit`),
    /// after the notice has been published and with this pane's state released. It takes the module
    /// off screen and says whether it did; if it did, the pane is [`Self::close`]d. If it could not
    /// -- the terminal is the last module on screen (`super::OnExit::CloseWindow`), so `f` closes
    /// the window instead of the module -- this pane is left alone: the notice stays and Enter
    /// restarts, until the window really goes.
    pub(crate) fn on_shell_exit(&self, f: impl Fn() -> bool + 'static) {
        self.state.borrow_mut().on_shell_exit = Some(Rc::new(f));
    }

    /// `prefix x` (or the shell's own end): the shell is hung up if it still runs, and the pane
    /// forgets it -- its screen, its cursor, anything queued behind a paste (and the input method is reset) -- so the
    /// next [`Self::start`] is a fresh shell on a blank pane, as the first one was. Does not wait:
    /// `TerminalSession`'s own `Drop` hangs up and reaps off the thread.
    pub(crate) fn close(&self) {
        self.state.borrow_mut().close();
        // No borrow held: `reset` can emit `commit`/`preedit-changed`, whose handlers borrow the state.
        self.im.reset();
        self.area.queue_render();
    }

    /// Window close: hang the shell up. Does not wait -- `TerminalSession`'s own `Drop`.
    pub(crate) fn shutdown(&self) {
        let mut state = self.state.borrow_mut();
        state.wanted = false;
        state.session = None;
    }
}

/// What the pane shows a failure message at when it has no metrics to size a screen from -- a
/// font failure, before any metrics have ever been built.
const NO_METRICS_SIZE: PtySize = PtySize {
    cols: 80,
    rows: 24,
    cell_width_px: 0,
    cell_height_px: 0,
};

/// Builds font metrics at `allocation` and catches font-manager panics. If no usable monospace
/// font exists, the pane can show a failure message instead of crashing the window.
///
/// Use the actual allocation rather than a 1x1 placeholder. With lazy metrics, the first resize
/// arrives before they exist, and GTK sends no second resize until the pane changes size.
/// Building at a placeholder would therefore spawn the shell with the wrong PTY dimensions.
fn build_metrics(allocation: Allocation) -> Result<TerminalMetrics, String> {
    let Allocation { width, height, scale } = allocation;
    panic::catch_unwind(|| TerminalMetrics::with_font(FONT_FAMILY, FONT_SIZE_PT, width, height, scale)).map_err(
        |payload| {
            payload
                .downcast_ref::<&str>()
                .map(ToString::to_string)
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "the font manager panicked".to_string())
        },
    )
}

/// Spawns the session if one is wanted, the pane has real geometry, and none is running. Also
/// where the terminal's font metrics are built, the first time they are needed -- never at
/// construction, so a window whose terminal is never shown never touches a font manager.
fn ensure_session(state: &Rc<RefCell<State>>, area: &GLArea, im: &IMMulticontext) {
    let (cwd, size, colors, focused) = {
        let mut s = state.borrow_mut();
        let size = match s.spawn_size(build_metrics) {
            None => return,
            Some(Ok(size)) => size,
            Some(Err(reason)) => {
                eprintln!("[terminal] could not build font metrics: {reason}");
                s.exited = true;
                s.frame = Some(message_frame(
                    NO_METRICS_SIZE,
                    s.colors,
                    &format!("[no terminal font available: {reason} \u{2014} Enter tries again]"),
                ));
                drop(s);
                area.queue_render();
                return;
            }
        };
        (s.cwd.clone(), size, s.colors, s.focused)
    };
    let (wake, mut woken) = futures_channel::mpsc::unbounded::<()>();
    let mut spec = SpawnSpec::user_shell(cwd.clone());
    let mut tried = Vec::new();
    let started = loop {
        let program = spec.program.clone();
        let wake = wake.clone();
        let config = SessionConfig {
            spawn: spec,
            size,
            colors,
            focused,
            tap: None,
        };
        match TerminalSession::spawn(config, move || {
            let _ = wake.unbounded_send(());
        }) {
            Ok(session) => {
                println!("[terminal] started {} (pid {})", program.display(), session.pid());
                break Ok(session);
            }
            Err(err) => {
                eprintln!("[terminal] could not start {}: {err}", program.display());
                tried.push(program);
                // Looked up only now, on this rare path: a passwd lookup can go through NSS, and
                // this is the GTK thread.
                match fallback_shell(&tried, passwd_shell()) {
                    Some(next) => spec = SpawnSpec::shell(next, cwd.clone()),
                    None => break Err(err),
                }
            }
        }
    };
    // Only the running session's waker may keep the channel open: its end is the future's end.
    drop(wake);
    let session = match started {
        Ok(session) => session,
        Err(err) => {
            let mut s = state.borrow_mut();
            s.exited = true;
            s.frame = Some(message_frame(
                size,
                colors,
                &format!("[no shell would start: {err} \u{2014} Enter tries again]"),
            ));
            drop(s);
            area.queue_render();
            return;
        }
    };
    state.borrow_mut().session = Some(session);
    let state = Rc::downgrade(state);
    let area = area.downgrade();
    let im = im.downgrade();
    // Ends when the session's thread drops the waker, i.e. when the session is over.
    glib::spawn_future_local(async move {
        while woken.recv().await.is_ok() {
            let (Some(state), Some(area), Some(im)) = (state.upgrade(), area.upgrade(), im.upgrade()) else {
                break;
            };
            let on_screen = area.is_mapped();
            pump(&state, &area, &im, on_screen);
        }
    });
}

/// A frame with one dim line on it, painted like any other: what the pane shows when it has no
/// shell to show.
fn message_frame(size: PtySize, colors: TerminalColors, message: &str) -> PaintList {
    let mut screen = Screen::new(size, colors);
    screen.feed(format!("\x1b[0;2m{message}\x1b[0m").as_bytes());
    screen.render(false)
}

/// Mapped and unmapped: hidden by `Ctrl+a t` or by a zoom of another pane. The session still reads
/// and answers while hidden; it only stops rendering (`SessionCommand::Visible`).
///
/// On the way back, what the session published while hidden is taken first, with bells unflashed
/// (phase 2): a hidden session folds a bell into its pending update without waking this pane, so
/// that bell would otherwise come out with the first frame after the show.
fn connect_visibility(area: &GLArea, state: &Rc<RefCell<State>>, im: &IMMulticontext) {
    {
        let state = state.clone();
        let im = im.clone();
        area.connect_map(move |area| {
            pump(&state, area, &im, false);
            state.borrow().send_command(SessionCommand::Visible(true));
        });
    }
    let state = state.clone();
    area.connect_unmap(move |_| {
        state.borrow().send_command(SessionCommand::Visible(false));
    });
}

/// Takes what the session published: the frame and where its cursor is, how the shell ended, a copy
/// for the desktop, a bell. `on_screen: false` takes a bell without flashing it. After the borrow,
/// because these are calls into GTK: the input method is pointed again when the cursor moved under
/// the keys, and the copies go onto the desktop. The flash-end timer is armed for
/// [`bell::repaint_delay`], not the raw remainder, so it never fires before the flash is actually
/// over. **Known limit:** the clipboard calls, the input method and the timer's own firing are GTK's
/// and are not tested without a display; the decisions are (`State::take_cursor`, `State::bell`,
/// `BellFlash`, `bell::repaint_delay`, and the engine's OSC 52 tests).
fn pump(state: &RefCell<State>, area: &GLArea, im: &IMMulticontext, on_screen: bool) {
    let Some(update) = state.borrow().session.as_ref().map(TerminalSession::take_update) else {
        return;
    };
    let (repoint, flash_until, closed_by_exit) = {
        let mut s = state.borrow_mut();
        let mut repoint = false;
        let mut closed_by_exit = None;
        if let Some(frame) = update.frame {
            s.frame = Some(frame);
            repoint = s.take_cursor(update.cursor);
            area.queue_render();
        }
        // Independent of `update.frame`: a synchronized update
        // can hold every frame back for up to 150ms while the mode changes underneath it, and
        // `pointer::route`/`wheel_route` must see the live mode the moment it changes, not only
        // when the next frame happens to render.
        if let Some(m) = update.mouse {
            s.mouse = m;
        }
        if let Some(exit) = update.exited {
            s.exited = true;
            println!("[terminal] {}", exit.notice());
            if super::closes_on_exit(&exit) {
                closed_by_exit = s.on_shell_exit.clone();
            }
        }
        let flash_until = if update.events.bell {
            s.bell(on_screen, Instant::now())
        } else {
            None
        };
        (repoint, flash_until, closed_by_exit)
    };
    if repoint {
        point_input_method(im, state, area);
    }
    // A copy lands whether the pane is shown or not; a read of either clipboard never gets this far
    // (`Term` refuses it, `Osc52::OnlyCopy`).
    if let Some(text) = update.events.clipboard {
        area.clipboard().set_text(&text);
        println!(
            "[terminal] a program copied {} characters to the clipboard (OSC 52)",
            text.chars().count()
        );
    }
    if let Some(text) = update.events.primary {
        area.primary_clipboard().set_text(&text);
        println!(
            "[terminal] a program copied {} characters to the primary selection (OSC 52)",
            text.chars().count()
        );
    }
    // A finished selection is copied to both the clipboard and primary selection on release.
    // Keep it in `State::last_selection` so `Ctrl+Shift+C` can copy it again without a session
    // round trip. The OSC 52 copies above do not update this cached selection.
    if let Some(text) = update.events.selection {
        area.clipboard().set_text(&text);
        area.primary_clipboard().set_text(&text);
        println!(
            "[terminal] a selection finished: copied {} characters to the clipboard and the primary selection",
            text.chars().count()
        );
        state.borrow_mut().last_selection = Some(text);
    }
    if let Some(until) = flash_until {
        area.queue_render();
        let area = area.downgrade();
        // `bell::repaint_delay`, not the raw `until - now`: glib truncates a `Duration` to whole
        // milliseconds, so the raw remainder fires up to 1 ms before `until` while the flash is
        // still active, and nothing else would then repaint the pane to clear it.
        glib::timeout_add_local_once(bell::repaint_delay(until, Instant::now()), move || {
            if let Some(area) = area.upgrade() {
                area.queue_render();
            }
        });
    }
    // `update.events.title` has nowhere to go: the spec gives it to the pane's header "when one
    // exists", and the terminal has no header yet.

    // Last, with nothing borrowed: the hook kills the module, unmapping this pane, whose handler
    // borrows the state. Then the pane forgets the shell, as `TerminalPane::close` does.
    if let Some(closed_by_exit) = closed_by_exit {
        if closed_by_exit() {
            state.borrow_mut().close();
            im.reset();
            area.queue_render();
        }
    }
}

/// Enter after the shell exited: a fresh session on a fresh PTY, in the same place.
fn restart(state: &Rc<RefCell<State>>, area: &GLArea, im: &IMMulticontext) {
    state.borrow_mut().forget_session();
    ensure_session(state, area, im);
}

fn connect_resize(area: &GLArea, state: &Rc<RefCell<State>>, im: &IMMulticontext) {
    let state = state.clone();
    let im = im.clone();
    area.connect_resize(move |area, width, height| {
        // A hidden or unallocated pane can report no area. Do not send that size to the PTY:
        // the terminal keeps its last size while hidden.
        if width <= 0 || height <= 0 {
            return;
        }
        let allocation = Allocation {
            width: width as f32,
            height: height as f32,
            scale: area.scale_factor() as f32,
        };
        let resized = {
            let mut s = state.borrow_mut();
            if let Some(skia) = s.skia.as_mut() {
                skia.resize(width, height);
            }
            s.allocate(allocation)
        };
        area.queue_render();
        if let Some(size) = resized {
            state.borrow().send_command(SessionCommand::Resize(size));
        }
        ensure_session(&state, area, &im);
    });
}

/// Queue a snapshot on scale changes so an idle pane can receive `resize`.
/// `connect_resize` rescales through `State::allocate` and `TerminalMetrics::rescale`; a scale
/// change alone leaves the logical size and grid dimensions unchanged. GTK emits the resize
/// only on the next `GLArea` snapshot. Without `queue_render`, the old framebuffer stays
/// upscaled and blurry until output causes a redraw.
fn connect_scale_change(area: &GLArea) {
    area.connect_scale_factor_notify(gtk4::prelude::GLAreaExt::queue_render);
}

fn connect_render(area: &GLArea, state: &Rc<RefCell<State>>) {
    let state = state.clone();
    area.connect_render(move |area, _ctx| {
        let mut s = state.borrow_mut();
        if s.skia.is_none() {
            s.skia = SkiaState::new();
            let (width, height) = (area.width() * area.scale_factor(), area.height() * area.scale_factor());
            if let Some(skia) = s.skia.as_mut() {
                skia.resize(width, height);
            }
        }
        // Laid out before the fields are borrowed apart: it reads the frame, the cursor and the
        // colours together.
        let preedit = s.preedit_layout();
        let flashing = s.bell.active(Instant::now());
        let State {
            skia,
            metrics,
            frame,
            colors,
            ..
        } = &mut *s;
        let Some(skia) = skia.as_mut() else {
            return glib::Propagation::Stop;
        };
        // MUST be first: see `SkiaState::invalidate_cached_gl_state`.
        skia.invalidate_cached_gl_state();
        skia.ensure_surface();
        let Some(surface) = skia.surface.as_mut() else {
            return glib::Propagation::Stop;
        };
        match frame.as_ref().zip(metrics.as_ref()) {
            Some((list, metrics)) => {
                eitri_terminal::paint(surface.canvas(), list, metrics);
                // After the frame, over it: the input method's composition at the cursor (phase 2).
                if let Some(preedit) = &preedit {
                    eitri_terminal::paint_ops(surface.canvas(), &preedit.ops, metrics);
                }
                // Over everything, for a moment: the bell (phase 2).
                if flashing {
                    bell::tint(surface.canvas(), colors.foreground);
                }
            }
            // No frame yet (or no metrics: the terminal has never been started), the theme's
            // background, never GTK's default or black.
            None => {
                let bg = colors.background;
                surface.canvas().clear(skia_safe::Color::from_rgb(bg.r, bg.g, bg.b));
            }
        }
        skia.gr_context.flush_and_submit();
        glib::Propagation::Stop
    });
}

/// Clicking the pane focuses it (GTK4 does not do this for a plain widget; the frozen pane's own
/// doc records the editor losing click-focus the day a second focusable pane appeared), the primary
/// button also drags a selection out of it, and the middle
/// button pastes the primary selection -- foot's and alacritty's own convention. **Since phase 3c
///, any of that is instead a mouse report when the program's own mode asks for one and
/// `Shift` is not held**: what changed
/// is which of two already-connected controllers acts, never a new one for reporting.
///
/// **Two controllers on the primary button, not one**, because they answer different questions:
/// `GestureClick` is the only one of the two GTK tells the click COUNT (`n_press`), which
/// `pointer::kind_for` needs and `GestureDrag` has no notion of; `GestureDrag` is the only one GTK
/// tells the drag's motion and release. Both attached to the same widget and button see the same
/// press, since neither claims the event sequence away from the other (`set_state` is never
/// called): the click gesture starts the selection at `pressed`, and the drag gesture extends and
/// finishes it as the button moves and lifts. A plain click with no motion still fires
/// `drag-end` at zero offset -- `Finish` on a selection that never left its single starting cell,
/// which for `Simple` is empty (`Term::selection_to_string` reports `None`, same as never having
/// selected anything) and for `Word`/`Line` is exactly the click's own word or line, no drag
/// required (`select.rs`'s own tests already prove `Start` + `Finish` alone is enough for those two).
/// **Known limit:** whether GTK really dispatches to both controllers rather than one claiming the
/// sequence away from the other is GTK's own runtime behaviour and is not tested without a display
/// (the same limit `paste_clipboard`'s own doc already names for the clipboard read and its timer).
///
/// **The drag gesture is widened to button 0** (every button, not only the primary): a `GestureClick`
/// stops delivering `released` once the pointer crosses the drag threshold, so it cannot be a
/// press/release pair's own source once a real drag is possible -- `GestureDrag`'s `drag-begin` and
/// `drag-end` always fire, at zero offset for a plain click. Its own selection code (button 1,
/// `Shift` not held) is unchanged; every other case (a report, or a local button 2/3 with nothing
/// defined for it) is new here and does nothing to the selection path.
fn connect_pointer(area: &GLArea, state: &Rc<RefCell<State>>, im: &IMMulticontext) {
    let focus_click = GestureClick::new();
    focus_click.set_button(0);
    {
        let target = area.clone();
        focus_click.connect_pressed(move |_, _, _, _| {
            if !target.has_focus() {
                target.grab_focus();
            }
        });
    }
    area.add_controller(focus_click);

    let select_click = GestureClick::new();
    select_click.set_button(BUTTON_PRIMARY);
    {
        let state = state.clone();
        select_click.connect_pressed(move |gesture, n_press, x, y| {
            let modes = state.borrow().routing_modes();
            if pointer::route(shift_held(gesture), modes) == pointer::PointerRoute::Program {
                return; // the widened drag gesture below reports this press instead.
            }
            send_select(&state, SelectStage::Start(n_press), x, y);
        });
    }
    area.add_controller(select_click);

    let drag = GestureDrag::new();
    drag.set_button(0);
    {
        let state = state.clone();
        drag.connect_drag_begin(move |gesture, x, y| {
            let modes = state.borrow().routing_modes();
            let shift = shift_held(gesture);
            if pointer::route(shift, modes) == pointer::PointerRoute::Program {
                if let Some(button) = mouse_button(gesture.current_button()) {
                    report_press(&state, button, gesture.current_event_state(), x, y);
                }
            }
            // A local button 1 press is `select_click`'s own `pressed`, above; a local button 2/3
            // press has nothing defined for it here (button 2's paste is `middle_click`'s own).
        });
    }
    {
        let state = state.clone();
        drag.connect_drag_update(move |gesture, offset_x, offset_y| {
            let Some((start_x, start_y)) = gesture.start_point() else {
                return;
            };
            let (x, y) = (start_x + offset_x, start_y + offset_y);
            let modes = state.borrow().routing_modes();
            let shift = shift_held(gesture);
            if pointer::route(shift, modes) == pointer::PointerRoute::Program {
                report_motion(&state, gesture.current_event_state(), x, y);
                return;
            }
            if gesture.current_button() == BUTTON_PRIMARY {
                send_select(&state, SelectStage::Extend, x, y);
            }
        });
    }
    {
        let state = state.clone();
        drag.connect_drag_end(move |gesture, offset_x, offset_y| {
            let Some((start_x, start_y)) = gesture.start_point() else {
                return;
            };
            let (x, y) = (start_x + offset_x, start_y + offset_y);
            // Use `reports.is_held()` to remember whether the press was reported. `route()` uses the
            // current Shift state, so a release with Shift newly held can route locally even though
            // the program received the press. Skipping that release would strand `ReportTracker::held`
            // and make the next ordinary motion report a phantom drag. Always call `report_release`;
            // it is a no-op when no button is held, as `connect_cancel` also relies on.
            let was_held = state.borrow().reports.is_held();
            report_release(&state, gesture.current_event_state(), x, y);
            if was_held {
                return;
            }
            if gesture.current_button() == BUTTON_PRIMARY {
                send_select(&state, SelectStage::Extend, x, y);
                state
                    .borrow()
                    .send_command(SessionCommand::Select(SelectCommand::Finish));
            }
        });
    }
    {
        let state = state.clone();
        // A sequence cancelled (another widget claimed it, a touch was cancelled) fires no
        // `drag-end`. A button `report_press` reported and never released would leave the program
        // thinking it is still held down; `report_release` itself is a no-op if nothing is held
        // (Local route, or nothing was ever pressed), so this is safe to call unconditionally.
        drag.connect_cancel(move |_, _| {
            let (x, y) = state.borrow().last_pointer;
            report_release(&state, ModifierType::empty(), x, y);
        });
    }
    area.add_controller(drag);

    let middle_click = GestureClick::new();
    middle_click.set_button(BUTTON_MIDDLE);
    {
        let state = state.clone();
        let area = area.clone();
        let im = im.clone();
        middle_click.connect_pressed(move |gesture, _, _, _| {
            let modes = state.borrow().routing_modes();
            if pointer::route(shift_held(gesture), modes) == pointer::PointerRoute::Program {
                return; // the widened drag gesture above reports this press instead.
            }
            paste_primary(&state, &area, &im);
        });
    }
    area.add_controller(middle_click);

    connect_pointer_motion(area, state);
}

/// Whether `Shift` is held on the event a controller is currently handling
/// (`EventControllerExt::current_event_state`, which every gesture below inherits).
fn shift_held(controller: &impl IsA<gtk4::EventController>) -> bool {
    controller.current_event_state().contains(ModifierType::SHIFT_MASK)
}

/// GDK's button numbering (1/2/3) to [`MouseButton`]; any other button (4/5, the wheel's own
/// "buttons" on some setups; a stylus button) is not reportable.
fn mouse_button(gdk_button: u32) -> Option<MouseButton> {
    match gdk_button {
        1 => Some(MouseButton::Left),
        2 => Some(MouseButton::Middle),
        3 => Some(MouseButton::Right),
        _ => None,
    }
}

fn mouse_mods(modifiers: ModifierType) -> MouseMods {
    MouseMods {
        shift: modifiers.contains(ModifierType::SHIFT_MASK),
        alt: modifiers.contains(ModifierType::ALT_MASK),
        ctrl: modifiers.contains(ModifierType::CONTROL_MASK),
    }
}

/// A widget-relative pixel turned into the absolute (line, column) cell a mouse report names --
/// [`selection_geometry`]'s own mapping, reused rather than reimplemented (`pointer.rs`'s
/// own module doc). `None` before a shell has ever spawned or rendered once.
fn report_cell(state: &State, x: f64, y: f64) -> Option<(i32, u16)> {
    let (cell_w, cell_h, cols, rows, top_line) = selection_geometry(state)?;
    let (line, col, _) = pointer::cell_at(x, y, cell_w, cell_h, cols, rows, top_line);
    Some((line, col))
}

/// A press at `(x, y)`: turned into a cell, fed to [`pointer::ReportTracker::press`], and sent
/// through the pane's one door. Dropped with no session/metrics/frame yet, or a scrollback cell
/// (`ReportTracker`'s own doc).
fn report_press(state: &Rc<RefCell<State>>, button: MouseButton, modifiers: ModifierType, x: f64, y: f64) {
    let mut s = state.borrow_mut();
    let Some(cell) = report_cell(&s, x, y) else { return };
    let Some(mut input) = s.reports.press(button, cell) else {
        return;
    };
    input.mods = mouse_mods(modifiers);
    drop(s);
    submit(state, PaneInput::Mouse(input));
}

/// A motion to `(x, y)`, deduped by cell (`ReportTracker`'s own doc): fed from both the drag
/// gesture's own update and the plain motion controller below, which is harmless (design decision,
/// spec Phase 3).
fn report_motion(state: &Rc<RefCell<State>>, modifiers: ModifierType, x: f64, y: f64) {
    let mut s = state.borrow_mut();
    let Some(cell) = report_cell(&s, x, y) else { return };
    let Some(mut input) = s.reports.motion(cell) else {
        return;
    };
    input.mods = mouse_mods(modifiers);
    drop(s);
    submit(state, PaneInput::Mouse(input));
}

/// A release at `(x, y)`: `None` from [`pointer::ReportTracker::release`] with no button held (a
/// gesture that started before the mode turned on) is silently dropped, same as a press/motion with
/// no geometry yet.
fn report_release(state: &Rc<RefCell<State>>, modifiers: ModifierType, x: f64, y: f64) {
    let mut s = state.borrow_mut();
    let Some(cell) = report_cell(&s, x, y) else { return };
    let Some(mut input) = s.reports.release(cell) else {
        return;
    };
    input.mods = mouse_mods(modifiers);
    drop(s);
    submit(state, PaneInput::Mouse(input));
}

/// Hover motion (no button held): `MOUSE_MOTION` (1003) reporting, and the pointer position wheel
/// events need but GDK does not carry on a scroll event (`connect_scroll`'s own doc). Its own report
/// is filtered by `encode_mouse` against the live mode exactly like the drag gesture's -- `wants`
/// does not accept `Motion(None)` under 1002 alone, only 1003 (`mouse.rs`'s own tests).
fn connect_pointer_motion(area: &GLArea, state: &Rc<RefCell<State>>) {
    let motion = gtk4::EventControllerMotion::new();
    {
        let state = state.clone();
        motion.connect_enter(move |controller, x, y| {
            state.borrow_mut().last_pointer = (x, y);
            let modes = state.borrow().routing_modes();
            if pointer::route(shift_held(controller), modes) == pointer::PointerRoute::Program {
                report_motion(&state, controller.current_event_state(), x, y);
            }
        });
    }
    {
        let state = state.clone();
        motion.connect_motion(move |controller, x, y| {
            state.borrow_mut().last_pointer = (x, y);
            let modes = state.borrow().routing_modes();
            if pointer::route(shift_held(controller), modes) == pointer::PointerRoute::Program {
                report_motion(&state, controller.current_event_state(), x, y);
            }
        });
    }
    area.add_controller(motion);
}

/// Which half of a `Start`/`Extend` pair [`send_select`] is sending: `Start` carries the click count
/// `pointer::kind_for` turns into a [`eitri_terminal::SelectKind`], `Extend` carries none.
enum SelectStage {
    Start(i32),
    Extend,
}

/// The geometry needed to turn a pointer position into a [`SelectCommand`]: cell size, grid
/// dimensions and the last frame's absolute top line (`PaintList::top_line`). The session thread
/// keeps `Screen::top_line`; the pane uses the frame to correlate a click with its displayed text.
/// Returns `None` until metrics exist, and uses top line zero when no frame exists.
///
/// Cell sizes are returned in widget-logical pixels, matching GTK gesture and motion coordinates.
/// Metrics use device pixels, so divide by [`TerminalMetrics::scale`]. [`im_cursor_rect`] makes
/// the reverse conversion. Without this division, scale 2 clicks land at about half their
/// intended row and column.
fn selection_geometry(state: &State) -> Option<(f64, f64, u16, u16, i32)> {
    let metrics = state.metrics.as_ref()?;
    let top_line = state.frame.as_ref().map_or(0, |f| f.top_line);
    let scale = f64::from(metrics.scale()).max(1.0);
    Some((
        f64::from(metrics.cell_width()) / scale,
        f64::from(metrics.cell_height()) / scale,
        metrics.cols(),
        metrics.rows(),
        top_line,
    ))
}

/// Turns one pointer position into a [`SelectCommand`] and sends it, dropped with no session or no
/// metrics yet (nothing to select against).
fn send_select(state: &Rc<RefCell<State>>, stage: SelectStage, x: f64, y: f64) {
    let s = state.borrow();
    let Some(session) = &s.session else { return };
    let Some((cell_w, cell_h, cols, rows, top_line)) = selection_geometry(&s) else {
        return;
    };
    let (line, col, right_half) = pointer::cell_at(x, y, cell_w, cell_h, cols, rows, top_line);
    let cmd = match stage {
        SelectStage::Start(n_press) => SelectCommand::Start {
            line,
            col,
            right_half,
            kind: pointer::kind_for(n_press),
        },
        SelectStage::Extend => SelectCommand::Extend { line, col, right_half },
    };
    session.send(SessionCommand::Select(cmd));
}

/// Copy the last finished selection to the clipboard for `Ctrl+Shift+C`.
/// `State::last_selection` holds the text received through `pump`'s `events.selection`, so no
/// additional `SelectCommand::Finish` or session round trip is needed. With no selection, only
/// a log line is emitted. The key is still swallowed so its encoding cannot reach the child.
fn copy_selection(state: &Rc<RefCell<State>>, area: &GLArea) {
    let Some(text) = state.borrow().last_selection.clone() else {
        println!("[terminal] Ctrl+Shift+C: nothing is selected");
        return;
    };
    area.clipboard().set_text(&text);
    println!(
        "[terminal] Ctrl+Shift+C: copied {} characters to the clipboard",
        text.chars().count()
    );
}

/// Grid lines scrolled per wheel notch. Reuse the terminal library's `LINES_PER_NOTCH`
/// so scrollback and alternate-screen arrow-key repeats use the same multiplier.
const LINES_PER_WHEEL_NOTCH: i32 = eitri_terminal::mouse::LINES_PER_NOTCH as i32;

/// One scroll event, in `unit`, folded into `wheel`'s carry-over state -- the same accumulator
/// `crate::wheel_zoom::WheelZoom` uses for Ctrl+wheel text size, reused rather than reimplemented so
/// a slow touchpad gesture and a reversal mid-scroll behave identically here (its own module doc and
/// tests already cover both). How many whole notches this event completed, positive up
/// (`WheelZoom`'s own "up is larger" convention, `dy` negative -- "up is into history" for
/// `scroll_request` below, "up" for a mouse-mode wheel report either way). **Called at most once
/// per scroll event**: `WheelZoom::scroll_in` is stateful, and calling it twice for one event (once
/// for a report, once for the scrollback) would double-count it.
fn wheel_notches(wheel: &mut WheelZoom, unit: gtk4::gdk::ScrollUnit, dy: f64) -> i32 {
    wheel.scroll_in(unit, dy)
}

/// Convert whole `notches` from [`wheel_notches`] to a scrollback request. Zero returns `None`.
/// Multiply the positive-is-up count by `LINES_PER_WHEEL_NOTCH` without changing its sign,
/// so positive values scroll into history.
fn scroll_request(notches: i32) -> Option<ScrollRequest> {
    (notches != 0).then_some(ScrollRequest::Lines(notches * LINES_PER_WHEEL_NOTCH))
}

/// Handle vertical wheel scrolling over the pane. `KINETIC` is disabled so scrolling stops
/// with the gesture. `crate::wheel_zoom::install` claims Ctrl+wheel during capture on the module
/// host to change text size before this controller receives it.
///
/// `pointer::wheel_route` chooses program mouse reports, alternate-screen arrow keys or
/// scrollback. Reports and arrows both use `SessionCommand::Mouse(Wheel)` because the session
/// thread needs the live `APP_CURSOR` bit to choose the bytes. Scrollback uses
/// `SessionCommand::Scroll` directly. Send one mouse report per notch: programs may already
/// multiply each wheel event, so sending a report per scrollback line would multiply twice.
fn connect_scroll(area: &GLArea, state: &Rc<RefCell<State>>) {
    let controller = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
    let wheel = RefCell::new(WheelZoom::default());
    let state = state.clone();
    controller.connect_scroll(move |controller, _dx, dy| {
        let notches = wheel_notches(&mut wheel.borrow_mut(), controller.unit(), dy);
        if notches != 0 {
            let modes = state.borrow().routing_modes();
            match pointer::wheel_route(shift_held(controller), modes) {
                pointer::WheelRoute::Scrollback => {
                    if let Some(req) = scroll_request(notches) {
                        state.borrow().send_command(SessionCommand::Scroll(req));
                    }
                }
                pointer::WheelRoute::Report | pointer::WheelRoute::Arrows => {
                    let dir = if notches > 0 { WheelDir::Up } else { WheelDir::Down };
                    let (x, y) = state.borrow().last_pointer;
                    let modifiers = controller.current_event_state();
                    for _ in 0..notches.unsigned_abs() {
                        report_wheel(&state, dir, modifiers, x, y);
                    }
                }
            }
        }
        glib::Propagation::Stop
    });
    area.add_controller(controller);
}

/// A wheel notch: unlike a press/release/motion, a wheel report needs no [`pointer::ReportTracker`]
/// state (it is never deduped and carries no held button) -- just the current cell, dropped the
/// same way a scrollback cell is (`pointer::mouse_input`'s own doc).
fn report_wheel(state: &Rc<RefCell<State>>, dir: WheelDir, modifiers: ModifierType, x: f64, y: f64) {
    let s = state.borrow();
    let Some(cell) = report_cell(&s, x, y) else { return };
    drop(s);
    let Some(mut input) = pointer::mouse_input(MouseKind::Wheel(dir), cell) else {
        return;
    };
    input.mods = mouse_mods(modifiers);
    submit(state, PaneInput::Mouse(input));
}

fn connect_keyboard(area: &GLArea, state: &Rc<RefCell<State>>, im: &IMMulticontext) {
    let keys = EventControllerKey::new();
    // GTK runs the input method on every key before `key-pressed`/`key-released` below, and a key it
    // consumes never reaches them (phase 2; the editor's own wiring, `neovide-editor/src/keyboard.rs`).
    // Eitri's own chords never get this far: they are taken in the capture phase.
    keys.set_im_context(Some(im));
    {
        let state = state.clone();
        let im = im.clone();
        keys.connect_key_pressed(move |controller, keyval, keycode, modifier| {
            let Some(area) = controller.widget().and_downcast::<GLArea>() else {
                return glib::Propagation::Stop;
            };
            let repeat = state.borrow_mut().repeats.press(keyval);
            if state.borrow().exited {
                if keys::restarts(keyval, modifier) {
                    restart(&state, &area, &im);
                }
                return glib::Propagation::Stop;
            }
            // Copy mode, ahead of `scroll_chord`: a copy-mode key never reaches
            // the shell, whether or not it is also one of `scroll_chord`'s own bindings.
            // Only the keys the input method passes on get here (the arrows, the page keys, `Esc`,
            // the `Ctrl` chords); every printable one arrives as a commit (`route_commit`).
            if state.borrow().copy_mode {
                let key = keys::copy_mode_key(keyval, modifier);
                state.borrow_mut().apply_copy_mode_key(key);
                return glib::Propagation::Stop;
            }
            if let Some(req) = keys::scroll_chord(keyval, modifier) {
                state.borrow().send_command(SessionCommand::Scroll(req));
                // Never forwarded to the child: `Shift+PageUp`/`Shift+PageDown`/`Shift+Home`/
                // `Shift+End` are this pane's own scrollback chords (bottom-terminal phase 3a).
                return glib::Propagation::Stop;
            }
            match keys::clipboard_chord(keyval, modifier) {
                Some(ClipboardChord::Copy) => {
                    copy_selection(&state, &area);
                    return glib::Propagation::Stop;
                }
                Some(ClipboardChord::Paste) => {
                    paste_clipboard(&state, &area, &im);
                    return glib::Propagation::Stop;
                }
                None => {}
            }
            // Remember delivery before forwarding the press. Forward a release only if its press reached
            // this pane. Do not infer delivery from release modifiers: Shift may have been released
            // before the letter in a clipboard chord.
            state.borrow_mut().delivered.mark(keyval);
            deliver(&state, raw_key(&area, keyval, keycode, modifier, true, repeat));
            // Claimed unconditionally: every key that reaches the terminal is the child's. The keys
            // Eitri keeps were taken before this controller ever saw them.
            glib::Propagation::Stop
        });
    }
    {
        let state = state.clone();
        keys.connect_key_released(move |controller, keyval, keycode, modifier| {
            state.borrow_mut().repeats.release(keyval);
            // A release can arrive without its press, such as the key that moved focus here, a prefix
            // literal's physical key, or a HINT label. Forward only releases for delivered presses.
            let was_delivered = state.borrow_mut().delivered.take(keyval);
            if !was_delivered {
                return;
            }
            let Some(area) = controller.widget().and_downcast::<GLArea>() else {
                return;
            };
            if !state.borrow().exited {
                deliver(&state, raw_key(&area, keyval, keycode, modifier, false, false));
            }
        });
    }
    area.add_controller(keys);

    // Focus-out must clear the held key: its release goes to whichever widget gets focus next.
    let focus = EventControllerFocus::new();
    {
        let state = state.clone();
        focus.connect_leave(move |_| {
            let mut s = state.borrow_mut();
            s.repeats.reset();
            s.delivered.reset();
        });
    }
    area.add_controller(focus);
}

/// Throws away a composition in progress rather than letting it commit: a paste or a `Ctrl+a`
/// literal is about to reach the shell, and half-typed pinyin must neither follow it nor precede it.
/// What the input method commits out of this reset is dropped (`ImeGate::begin_reset`). Nothing to
/// do, and no call into the input method, when nothing is being composed.
fn discard_composition(state: &RefCell<State>, im: &IMMulticontext, area: &GLArea) {
    {
        let mut s = state.borrow_mut();
        if !s.ime.composing() {
            return;
        }
        s.ime.begin_reset();
    }
    println!("[terminal] a composition in progress was discarded");
    im.reset();
    state.borrow_mut().ime.end_reset();
    area.queue_render();
}

/// The input method's news (`ime.rs`): a commit goes through the gate to [`submit`]; a composition
/// that starts, changes or ends is kept, redrawn, and the input method told where its caret is.
/// **Known limit:** these handlers and the GTK calls they make are not tested without a display;
/// what they decide with is (`ImeGate`, `State::preedit_layout`, `State::ime_caret`,
/// `State::ime_rect`, `State::take_cursor`, `im_cursor_rect`).
fn connect_ime(area: &GLArea, state: &Rc<RefCell<State>>, im: &IMMulticontext) {
    {
        let state = state.clone();
        let area = area.downgrade();
        im.connect_commit(move |_, text| {
            // GTK focuses the input method out from inside a focus change, before `pane_focus`
            // reports it, and fcitx5-gtk commits its composition right there (`ime.rs`'s module
            // doc). GTK's own state is already updated by then, so ask it, not only the gate. Keep
            // BOTH halves (`GtkFocus`'s doc): a focus move clears `has_focus` before that commit,
            // but alt-tab clears only `is_active` -- `has_focus` alone would type `ni hao` there.
            let gtk = area.upgrade().map_or(GtkFocus::default(), |area| GtkFocus {
                widget_has_focus: area.has_focus(),
                window_is_active: area
                    .root()
                    .and_downcast::<gtk4::Window>()
                    .is_some_and(|window| window.is_active()),
            });
            let route = state.borrow_mut().route_commit(text, gtk);
            match route {
                CommitRoute::Shell(input) => submit(&state, PaneInput::Key(input)),
                CommitRoute::CopyMode(key) => state.borrow_mut().apply_copy_mode_key(key),
                CommitRoute::Dropped(why) => println!(
                    "[terminal] an input-method commit ({} characters) was dropped: {why}",
                    text.chars().count()
                ),
            }
            if let Some(area) = area.upgrade() {
                area.queue_render();
            }
        });
    }
    let on_preedit = {
        let state = state.clone();
        let area = area.downgrade();
        move |im: &IMMulticontext| {
            if let Some(area) = area.upgrade() {
                preedit_changed(im, &state, &area);
            }
        }
    };
    im.connect_preedit_start(on_preedit.clone());
    im.connect_preedit_changed(on_preedit.clone());
    im.connect_preedit_end(on_preedit);
}

/// A composition started, changed or ended: keep it, redraw it, and point the input method at its
/// caret. On all three, as the editor does: `start` so the candidate window is in place, `changed`
/// so it follows the composition, `end` so the next one never starts against a stale place. The
/// preedit's pango attributes (rime's `HighLight` on the segment being converted) are not drawn.
///
/// In copy mode a composition is never kept ([`State::take_preedit`]): it is reset, from an idle
/// callback rather than from inside this signal -- fcitx5-gtk emits `preedit-start` from inside its
/// own preedit update and would re-enter it (`fcitximcontext.cpp`'s
/// `_fcitx_im_context_update_formatted_preedit_cb`). Whatever that reset commits is dropped by the
/// gate (`ImeGate::begin_reset`), and a commit before it is a copy-mode key or nothing.
fn preedit_changed(im: &IMMulticontext, state: &Rc<RefCell<State>>, area: &GLArea) {
    let (text, _attributes, caret) = im.preedit_string();
    let discard = state.borrow_mut().take_preedit(&text, caret);
    if discard {
        let im = im.clone();
        let state = state.clone();
        glib::idle_add_local_once(move || {
            state.borrow_mut().ime.begin_reset();
            im.reset();
            state.borrow_mut().ime.end_reset();
        });
    }
    point_input_method(im, state, area);
    area.queue_render();
}

/// Tells the input method where its candidate window belongs ([`State::ime_rect`]); nothing while
/// that is unknown. fcitx5-gtk skips a rectangle it already has (`fcitximcontext.cpp:827-832`), so
/// calling this more often than needed costs nothing there.
fn point_input_method(im: &IMMulticontext, state: &RefCell<State>, area: &GLArea) {
    let rect = state.borrow().ime_rect(area.scale_factor());
    if let Some(rect) = rect {
        im.set_cursor_location(&rect);
    }
}

fn raw_key(
    area: &GLArea,
    keyval: gtk4::gdk::Key,
    keycode: u32,
    modifier: ModifierType,
    pressed: bool,
    repeat: bool,
) -> RawKey {
    // Re-translating the hardware keycode with no modifiers recovers the base key (`1` under `!`),
    // and the same call says whether the layout itself consumed Control. See `keys`.
    let display = area.display();
    let unmodified_keyval = display
        .translate_key(keycode, ModifierType::empty(), 0)
        .map(|(key, _, _, _)| key);
    let consumed = display
        .translate_key(keycode, modifier, 0)
        .map(|(_, _, _, consumed)| consumed)
        .unwrap_or_else(ModifierType::empty);
    RawKey {
        keyval,
        unmodified_keyval,
        state: modifier,
        consumed,
        pressed,
        repeat,
    }
}

fn deliver(state: &RefCell<State>, raw: RawKey) {
    let Some(input) = keys::normalize_key(raw) else { return };
    submit(state, PaneInput::Key(input));
}

/// The one door to the shell: straight through, or behind a paste still being read.
fn submit(state: &RefCell<State>, input: PaneInput) {
    let mut s = state.borrow_mut();
    if let Some(input) = s.queue.submit(input) {
        s.send_input(input);
    }
}

/// `Ctrl+Shift+V`: reads the clipboard's text and pastes it. Its place in line is taken now, so
/// what is typed while the read is in flight follows the paste; [`PASTE_TIMEOUT`] gives up on a
/// clipboard that does not answer. **Known limit:** the read and the timer are GTK's and are not
/// tested without a display; what they call (`State::finish_paste`, `InputQueue`) is.
fn paste_clipboard(state: &Rc<RefCell<State>>, area: &GLArea, im: &IMMulticontext) {
    paste_from(state, area, im, area.clipboard(), "Ctrl+Shift+V");
}

/// Paste the PRIMARY selection on a middle click through the same path as `Ctrl+Shift+V`.
/// Reuse `InputQueue` and [`PASTE_TIMEOUT`] so composition, asynchronous paste reads and later
/// typing stay ordered without a second implementation for this gesture.
fn paste_primary(state: &Rc<RefCell<State>>, area: &GLArea, im: &IMMulticontext) {
    paste_from(state, area, im, area.primary_clipboard(), "middle click");
}

/// What [`paste_clipboard`] and [`paste_primary`] share: read `clipboard`'s text asynchronously and
/// paste it, `label` naming the gesture in every log line so the two are told apart.
fn paste_from(
    state: &Rc<RefCell<State>>,
    area: &GLArea,
    im: &IMMulticontext,
    clipboard: Clipboard,
    label: &'static str,
) {
    discard_composition(state, im, area);
    let ticket = state.borrow_mut().queue.begin_paste();
    let read = clipboard.read_text_future();
    {
        let state = Rc::downgrade(state);
        glib::spawn_future_local(async move {
            let text = match read.await {
                Ok(Some(text)) => text.to_string(),
                Ok(None) => {
                    println!("[terminal] {label}: the clipboard holds no text");
                    String::new()
                }
                Err(err) => {
                    eprintln!("[terminal] {label}: could not read the clipboard: {err}");
                    String::new()
                }
            };
            let Some(state) = state.upgrade() else { return };
            let input = keys::paste(&text);
            let had_text = input.is_some();
            if !state.borrow_mut().finish_paste(ticket, input) && had_text {
                eprintln!("[terminal] {label}: the clipboard answered too late; that paste is dropped");
            }
        });
    }
    let state = Rc::downgrade(state);
    glib::timeout_add_local_once(PASTE_TIMEOUT, move || {
        let Some(state) = state.upgrade() else { return };
        if state.borrow_mut().finish_paste(ticket, None) {
            eprintln!("[terminal] {label}: no answer from the clipboard in {PASTE_TIMEOUT:?}; typing goes on");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk4::gdk::ScrollUnit;

    /// One wheel notch scrolls 3 lines UP (into history): `WheelZoom`'s own "up is larger"
    /// convention (`dy` negative) carries straight through to "up is into history" with no sign
    /// flip in `scroll_request` itself.
    #[test]
    fn one_wheel_notch_is_three_lines_up() {
        let mut wheel = WheelZoom::default();
        let notches = wheel_notches(&mut wheel, ScrollUnit::Wheel, -1.0);
        assert_eq!(scroll_request(notches), Some(ScrollRequest::Lines(3)));
    }

    /// Small touchpad `dy` values accumulate to one whole wheel notch before producing a request,
    /// rather than producing a scroll step for every event.
    #[test]
    fn ten_surface_pixel_events_worth_one_notch_make_one_request() {
        let mut wheel = WheelZoom::default();
        let mut requests = Vec::new();
        for _ in 0..10 {
            requests.extend(scroll_request(wheel_notches(&mut wheel, ScrollUnit::Surface, -1.5)));
        }
        assert_eq!(requests, vec![ScrollRequest::Lines(3)], "one notch (15px), not ten");
    }

    /// `WheelZoom`'s own semantics, reused rather than reimplemented: a reversal mid-gesture drops
    /// the fractional remainder instead of netting it against the new direction.
    #[test]
    fn a_reversal_drops_the_remainder_instead_of_netting_it() {
        let mut wheel = WheelZoom::default();
        assert_eq!(
            scroll_request(wheel_notches(&mut wheel, ScrollUnit::Wheel, -0.75)),
            None
        );
        assert_eq!(
            scroll_request(wheel_notches(&mut wheel, ScrollUnit::Wheel, 1.0)),
            Some(ScrollRequest::Lines(-3)),
            "the -0.75 is dropped, not netted into this one"
        );
    }

    /// An unused terminal must not create a Skia `FontMgr` or perform a fontconfig match at launch.
    /// `State::new`, used by `TerminalPane::new`, leaves metrics absent before any `gtk4::GLArea`
    /// is built. This construction check needs no display or `gtk4::init`.
    #[test]
    fn a_freshly_constructed_pane_builds_no_metrics() {
        let state = State::new(PathBuf::from("/tmp"));
        assert!(
            state.metrics.is_none(),
            "metrics must wait for ensure_session's first spawn attempt, not be built at construction"
        );
    }

    /// `prefix x` or the shell's own end (2026-09-26): after a close nothing spawns until a start
    /// wants a shell again, and then a fresh one does -- even after an exit, which on its own waits
    /// for Enter -- on a blank pane, at the allocation the pane already had.
    #[test]
    fn a_closed_pane_forgets_its_shell_and_the_next_start_spawns_a_fresh_one() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.wanted = true;
        state.allocate(FIRST_SHOW);
        let first = state.spawn_size(build_metrics).unwrap().unwrap();
        // The shell exited: its notice is on screen and Enter would restart it.
        state.exited = true;
        state.frame = Some(message_frame(first, state.colors, "[process exited 0]"));
        state.cursor = Some(CursorCell {
            row: 1,
            col: 0,
            visible: true,
        });

        state.close();
        assert!(!state.wanted && !state.exited && state.session.is_none());
        assert!(
            state.frame.is_none() && state.cursor.is_none(),
            "nothing of the last shell stays"
        );
        assert_eq!(state.spawn_size(build_metrics), None, "closed: nothing spawns");

        state.wanted = true;
        assert_eq!(
            state.spawn_size(build_metrics).unwrap().unwrap(),
            first,
            "a fresh shell, at the allocation the pane already had"
        );
    }

    /// First-show test allocation: 1280x234 device pixels at scale 1.
    const FIRST_SHOW: Allocation = Allocation {
        width: 1280.0,
        height: 234.0,
        scale: 1.0,
    };

    /// GUI pass 2026-09-23, defect 1: the first `Ctrl+a t` spawned the shell on a 1x1 PTY. Driven
    /// in the order GTK drives it -- `start` before the pane has any allocation, then the GLArea's
    /// first `resize`, which finds no metrics to resize, then `ensure_session` -- with the real
    /// `build_metrics`, so it fails against the placeholder build this replaced (checked: `1x1`).
    #[test]
    fn a_lazily_started_shell_spawns_at_the_panes_allocation_never_1x1() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.wanted = true;
        assert_eq!(
            state.spawn_size(build_metrics),
            None,
            "no allocation yet: nothing to spawn at"
        );
        assert_eq!(state.allocate(FIRST_SHOW), None, "no metrics yet, and no PTY to tell");

        let size = state
            .spawn_size(build_metrics)
            .expect("wanted and allocated: a shell is due")
            .expect("this machine has a monospace font");
        let expected = TerminalMetrics::with_font(FONT_FAMILY, FONT_SIZE_PT, 1280.0, 234.0, 1.0);
        assert_eq!(
            (size.cols, size.rows),
            (expected.cols(), expected.rows()),
            "the first PTY size is the allocation's grid"
        );
        assert!(size.cols > 1 && size.rows > 1, "never 1x1: {size:?}");
    }

    /// The scale is part of the allocation kept for the lazy build: metrics built at scale 1 on a
    /// scale-2 output would carry half-size cells into the first spawn.
    #[test]
    fn the_lazy_metrics_are_built_at_the_allocations_scale() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.wanted = true;
        let doubled = Allocation {
            width: 2560.0,
            height: 468.0,
            scale: 2.0,
        };
        state.allocate(doubled);
        let size = state.spawn_size(build_metrics).unwrap().unwrap();
        let metrics = state.metrics.as_ref().expect("built by spawn_size");
        assert_eq!(metrics.scale(), 2.0);
        let expected = TerminalMetrics::with_font(FONT_FAMILY, FONT_SIZE_PT, 2560.0, 468.0, 2.0);
        assert_eq!((size.cols, size.rows), (expected.cols(), expected.rows()));
    }

    /// Pointer positions use widget-logical pixels while metrics use device pixels. At scale 2,
    /// a logical point in the middle of cell (row 3, col 10) must map to that cell. Dividing by
    /// the device cell size without converting units would halve both coordinates.
    #[test]
    fn a_pointer_at_scale_2_lands_on_the_cell_it_is_over() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.wanted = true;
        state.allocate(Allocation {
            width: 2560.0,
            height: 936.0,
            scale: 2.0,
        });
        state.spawn_size(build_metrics).unwrap().unwrap();
        let metrics = state.metrics.as_ref().expect("built by spawn_size");
        assert_eq!(metrics.scale(), 2.0);
        assert!(
            metrics.rows() > 3 && metrics.cols() > 10,
            "{}x{}",
            metrics.cols(),
            metrics.rows()
        );
        let logical_w = f64::from(metrics.cell_width()) / 2.0;
        let logical_h = f64::from(metrics.cell_height()) / 2.0;
        let (x, y) = (logical_w * 10.25, logical_h * 3.5);
        assert_eq!(report_cell(&state, x, y), Some((3, 10)));
        let (cw, ch, cols, rows, top) = selection_geometry(&state).unwrap();
        assert_eq!(pointer::cell_at(x, y, cw, ch, cols, rows, top), (3, 10, false));
    }

    /// Once the metrics exist, a later allocation resizes them and is reported to the PTY -- and
    /// the latest allocation, not the first, is what a lazy build would use.
    #[test]
    fn an_allocation_after_the_metrics_exist_is_a_resize() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.wanted = true;
        state.allocate(FIRST_SHOW);
        let first = state.spawn_size(build_metrics).unwrap().unwrap();
        let taller = Allocation {
            height: FIRST_SHOW.height * 2.0,
            ..FIRST_SHOW
        };
        let resized = state.allocate(taller).expect("twice the height is more rows");
        assert!(resized.rows > first.rows, "{first:?} -> {resized:?}");
        assert_eq!(resized.cols, first.cols);
        assert_eq!(
            state.allocate(taller),
            None,
            "the same allocation again is not a change"
        );
    }

    /// Bottom-terminal phase 2: Enter after an exit starts a new shell, and what was typed for the
    /// old one while a paste was being read must not reach the new one when that paste ends.
    #[test]
    fn restarting_forgets_input_that_waited_behind_a_paste() {
        let mut state = State::new(PathBuf::from("/tmp"));
        let ticket = state.queue.begin_paste();
        let typed = NormalizedInput::Paste {
            text: "for the old shell".to_string(),
            bracketed: false,
        };
        assert_eq!(
            state.queue.submit(typed.clone().into()),
            None,
            "it waits behind the paste"
        );
        state.exited = true;
        state.forget_session();
        assert!(!state.exited);
        assert!(
            !state.finish_paste(ticket, None),
            "that paste is gone with the old shell"
        );
        assert_eq!(
            state.queue.submit(typed.clone().into()),
            Some(typed.into()),
            "nothing waits any more"
        );
    }

    /// Bottom-terminal phase 3c: a restart forgets the live mouse mode too -- a program that turned
    /// mouse reporting on must not have its old mode outlive it into a fresh shell.
    #[test]
    fn restarting_forgets_the_mouse_mode() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.mouse = MouseModes {
            report: true,
            alt_screen: true,
            alternate_scroll: true,
        };
        state.forget_session();
        assert_eq!(state.mouse, MouseModes::default());
    }

    /// A program that turned mouse reporting on and then died (`kill -9 htop`) must not leave its
    /// final screen unselectable: once the pane knows the shell has exited, routing sees the mode
    /// off, whatever the session last reported.
    #[test]
    fn routing_modes_ignores_a_stale_report_mode_once_exited() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.mouse = MouseModes {
            report: true,
            alt_screen: false,
            alternate_scroll: true,
        };
        assert!(!state.routing_modes().report, "no session yet: routing ignores it");
        state.exited = true;
        assert!(!state.routing_modes().report, "exited: routing ignores it too");
    }

    /// Bottom-terminal phase 2: a composition is drawn at the cursor the session reported with the
    /// latest frame, the candidate window follows its caret, and without one the input method is
    /// pointed at the cursor itself. Losing the keys takes the composition off the screen.
    #[test]
    fn a_composition_is_drawn_at_the_reported_cursor_and_the_candidates_follow_its_caret() {
        use terminal_render::PaintOp;
        let mut state = State::new(PathBuf::from("/tmp"));
        assert_eq!(state.ime_caret(), None, "no frame yet: nowhere to point");
        let mut screen = Screen::new(
            PtySize {
                cols: 20,
                rows: 5,
                cell_width_px: 9,
                cell_height_px: 18,
            },
            TerminalColors::default(),
        );
        screen.feed(b"$ ");
        state.frame = Some(screen.render(true));
        state.cursor = Some(screen.cursor_cell());
        assert_eq!(state.preedit_layout(), None);
        assert_eq!(state.ime_caret(), Some((0, 2)), "no composition: the cursor");
        state.ime.set_focused(true);
        state.ime.preedit_changed("ni", 2);
        let layout = state.preedit_layout().expect("a composition over a frame");
        assert!(matches!(
            layout.ops.first(),
            Some(PaintOp::FillCells {
                row: 0,
                col: 2,
                cols: 2,
                ..
            })
        ));
        assert_eq!(state.ime_caret(), Some((0, 4)), "the composition's caret");
        state.ime.set_focused(false);
        assert_eq!(state.preedit_layout(), None, "the keys left: nothing is drawn");
    }

    /// fcitx5 places the first candidate window before `preedit-*`, so it must already know the
    /// cursor location. A frame requests a new location only if the cell moved while the terminal
    /// held the keys. Changing cursor visibility alone is not movement.
    #[test]
    fn a_frame_repoints_the_input_method_only_when_the_cursor_moved_under_the_keys() {
        let cell = |row, col, visible| CursorCell { row, col, visible };
        let mut state = State::new(PathBuf::from("/tmp"));
        assert!(
            !state.take_cursor(Some(cell(0, 2, true))),
            "moved, but the keys are elsewhere"
        );
        state.focused = true;
        assert!(!state.take_cursor(Some(cell(0, 2, true))), "the same cell");
        assert!(
            !state.take_cursor(Some(cell(0, 2, false))),
            "hidden in place is not a move"
        );
        assert!(state.take_cursor(Some(cell(1, 0, true))), "moved under the keys");
        assert_eq!(state.cursor, Some(cell(1, 0, true)));
    }

    /// A frame without a cursor and a restart both clear the cursor, so composition is not drawn
    /// at a stale cell.
    #[test]
    fn an_unknown_cursor_draws_no_composition_and_points_nowhere() {
        let mut state = State::new(PathBuf::from("/tmp"));
        let mut screen = Screen::new(
            PtySize {
                cols: 20,
                rows: 5,
                cell_width_px: 9,
                cell_height_px: 18,
            },
            TerminalColors::default(),
        );
        screen.feed(b"$ ");
        state.frame = Some(screen.render(true));
        state.focused = true;
        state.ime.set_focused(true);
        state.take_cursor(Some(screen.cursor_cell()));
        state.ime.preedit_changed("ni", 0);
        assert!(state.preedit_layout().is_some());
        assert!(!state.take_cursor(None), "nothing to point at");
        assert_eq!(state.preedit_layout(), None, "no composition at a stale cell");
        assert_eq!(state.ime_caret(), None);
        state.take_cursor(Some(screen.cursor_cell()));
        state.forget_session();
        assert_eq!(state.cursor, None, "a restart forgets where the old shell's cursor was");
        assert_eq!(state.ime_caret(), None);
    }

    /// A bell received while hidden must not flash when the pane becomes visible; an on-screen
    /// bell flashes. This test covers only `State::bell`'s `on_screen` handling. The full path
    /// requires a display: the map handler drains updates with `pump(.., false)` before sending
    /// `Visible(true)`, and the session folds hidden bells into an update without waking the pane.
    #[test]
    fn a_bell_from_while_the_pane_was_hidden_does_not_flash() {
        let mut state = State::new(PathBuf::from("/tmp"));
        let now = Instant::now();
        assert_eq!(state.bell(false, now), None);
        assert!(!state.bell.active(now), "nothing shows");
        assert!(state.bell(true, now).is_some());
        assert!(state.bell.active(now));
    }

    /// The gate itself: nothing is spawned for a pane nobody asked for, and nothing twice.
    #[test]
    fn nothing_spawns_unless_wanted_and_the_font_failure_is_reported() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.allocate(FIRST_SHOW);
        assert_eq!(state.spawn_size(build_metrics), None, "not wanted");
        assert!(state.metrics.is_none(), "and no font manager touched for it");
        state.wanted = true;
        state.exited = true;
        assert_eq!(state.spawn_size(build_metrics), None, "an exited shell waits for Enter");
        state.exited = false;
        let failed = state.spawn_size(|_| Err("no fonts".to_string()));
        assert_eq!(failed, Some(Err("no fonts".to_string())));
        assert!(state.metrics.is_none(), "a failed build leaves nothing half-made");
    }

    const HAS_THE_KEYS: GtkFocus = GtkFocus {
        widget_has_focus: true,
        window_is_active: true,
    };

    /// With an input method attached, printable keys arrive as commits, so commits handle copy-mode
    /// letters. In copy mode they become copy-mode keys or are ignored; they never reach the shell.
    /// After `q` leaves copy mode, the next commit is typing again.
    #[test]
    fn in_copy_mode_a_commit_is_a_copy_mode_key_and_never_reaches_the_shell() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.focused = true;
        state.ime.set_focused(true);
        assert!(
            matches!(state.route_commit("k", HAS_THE_KEYS), CommitRoute::Shell(_)),
            "outside copy mode a letter is typing"
        );

        state.copy_mode = true;
        assert_eq!(
            state.route_commit("k", HAS_THE_KEYS),
            CommitRoute::CopyMode(CopyModeKey::Scroll(ScrollRequest::Lines(1)))
        );
        assert_eq!(
            state.route_commit("G", HAS_THE_KEYS),
            CommitRoute::CopyMode(CopyModeKey::Scroll(ScrollRequest::Bottom))
        );
        for swallowed in ["x", "ls", "\u{4f60}\u{597d}", "echo hi\n"] {
            assert_eq!(
                state.route_commit(swallowed, HAS_THE_KEYS),
                CommitRoute::CopyMode(CopyModeKey::Swallow),
                "{swallowed:?} reaches nothing in copy mode"
            );
        }
        assert!(
            matches!(state.route_commit("k", GtkFocus::default()), CommitRoute::Dropped(_)),
            "the gate still decides first: no keys, no key"
        );

        let leave = state.route_commit("q", HAS_THE_KEYS);
        assert_eq!(leave, CommitRoute::CopyMode(CopyModeKey::Leave));
        let CommitRoute::CopyMode(key) = leave else {
            unreachable!()
        };
        state.apply_copy_mode_key(key);
        assert!(!state.copy_mode, "q left copy mode");
        assert!(
            matches!(state.route_commit("k", HAS_THE_KEYS), CommitRoute::Shell(_)),
            "and the next letter is typing again"
        );
    }

    /// With rime on, a letter in copy mode starts a composition instead of committing. It is never
    /// kept or drawn, and the pane is told to reset the input method, so nothing it composes can be
    /// committed into the shell once copy mode ends. Outside copy mode a composition is kept as ever.
    #[test]
    fn a_composition_started_in_copy_mode_is_never_kept_and_is_thrown_away() {
        let mut state = State::new(PathBuf::from("/tmp"));
        state.ime.set_focused(true);
        state.copy_mode = true;
        assert!(state.take_preedit("k", 1), "a composition in copy mode is reset");
        assert!(!state.ime.composing(), "and never kept or drawn");
        assert!(!state.take_preedit("", 0), "its end asks for nothing more");

        state.copy_mode = false;
        assert!(!state.take_preedit("ni", 2));
        assert!(state.ime.composing(), "outside copy mode a composition is kept");
    }
}
