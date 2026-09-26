//! The bottom terminal's widget: a `GtkGLArea` that paints its session's latest frame, and the
//! keyboard, click, focus and resize wiring around it.
//!
//! From the frozen `terminal-pane/src/pane.rs` (`freeze/terminal-stack` @ `1e715ab`), which had no
//! session at all; what changed is what the frozen pane's callbacks are now connected to. The pane
//! owns no terminal semantics: bytes, parsing, encoding and rendering are `neovibe-terminal`'s, on
//! the session thread. What it keeps is the latest `PaintList` and the GL surface to paint it on.
//!
//! **The shell starts lazily** (spec §4.5): `start` only says a shell is wanted, and the session is
//! spawned at the first allocation with area, so it never starts at 1x1 and redraws its prompt.
//! That allocation is **kept** ([`State::allocation`]) and the lazily built metrics are built AT it
//! (GUI pass 2026-09-23, defect 1): the first `resize` arrives before any metrics exist, so it has
//! nothing to resize, and GTK sends no second one for the same allocation. Before this, the metrics
//! were built at a 1x1 placeholder and the first `Ctrl+a t` spawned the shell on a 1x1 PTY, blank
//! but for one cursor cell, until something happened to resize the pane.
//!
//! **Woken, never polled.** The session calls a waker from its own thread; the waker sends on a
//! channel whose receiver a `glib::spawn_future_local` future awaits on the main loop. No timer:
//! the agent panel's 33ms pump would add up to 33ms to every echo, and an idle terminal costs no
//! wake-ups at all (tested in `neovibe-terminal`'s `idle_renders_nothing`). Hidden (unmapped), the
//! pane tells the session so, and a program printing away in a hidden terminal renders nothing.
//!
//! **A shell that will not start** (a stale `$SHELL`) falls back to the passwd entry's shell and
//! then `/bin/sh`; if none starts, the pane says so in its own frame and waits for Enter, rather
//! than trying again on every resize.
//!
//! **Chinese input (bottom-terminal phase 2)** is `ime.rs`'s: a `GtkIMMulticontext` on this widget's
//! key controller, its commits through [`submit`], its unfinished composition drawn over the frame
//! at the cursor the session reports (`Update::cursor`), and the input method pointed at its caret
//! on every `preedit-*`, when the terminal gains the keys, and when a frame moves the cursor while
//! it has them. **GTK's key controller owns the input method's focus** (`set_im_context`) and
//! focuses it out from inside a focus change, before `pane_focus` reports anything -- which is when
//! fcitx5 commits whatever was being composed. So the commit handler asks GTK whether the terminal
//! still has the keys ([`connect_ime`]), and a composition cut off by the keys leaving is dropped,
//! never typed (`ime.rs`'s module doc has the sources). Its client widget is set by
//! [`TerminalPane::start`], so a window whose terminal is never shown creates no input-method client.
//! **No `RefCell` borrow is held across a call into the input method** (`reset`,
//! `set_cursor_location`, `set_client_widget`): `reset` can emit `commit` or `preedit-changed` before
//! it returns, and those handlers borrow the state.
//!
//! **Everything bound for the shell goes through one door, [`submit`]** (bottom-terminal phase 2):
//! typed keys, the prefix's literals, `Ctrl+Shift+V`'s text and input-method commits. A paste reads
//! the clipboard asynchronously, and what is typed while it is read waits behind it (`input_queue`),
//! so an Enter can never overtake the text it was meant to run. `Ctrl+Shift+C` is still held back:
//! nothing can be selected until phase 3.
//!
//! **A program's copy lands on the desktop, and its bell flashes the pane** (bottom-terminal phase
//! 2). An OSC 52 copy goes to GTK's clipboard (`c`) or primary selection (`p`/`s`), hidden or not; a
//! read is still refused, by `Term` itself. A bell tints the pane for a moment (`bell.rs`), and a
//! bell rung while the terminal was hidden is not flashed when it comes back. The title a program
//! sets has nowhere to go: the spec sends it to the pane's header "when one exists", and none does.
//!
//! **The font metrics are lazy too, and never panic** (review 2026-09-23, M8/task-8 minor). A
//! window whose terminal is never shown never touches a font manager: `TerminalMetrics` is built
//! by [`ensure_session`], the same gate that lazily spawns the shell, not at [`TerminalPane::new`].
//! A machine with no usable monospace font shows the pane's own failure frame -- the same one a
//! shell that would not start shows -- rather than panicking the whole process.

use std::cell::RefCell;
use std::panic;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::gdk::{ModifierType, Rectangle};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{EventControllerFocus, EventControllerKey, GLArea, GestureClick, IMMulticontext, InputPurpose};

use neovibe_terminal::pty::{fallback_shell, passwd_shell};
use neovibe_terminal::{
    layout_preedit, CursorCell, PreeditLayout, PtySize, Screen, SessionCommand, SessionConfig, SpawnSpec,
    TerminalColors, TerminalMetrics, TerminalSession,
};
use terminal_input::NormalizedInput;
use terminal_render::PaintList;

use super::bell::{self, BellFlash};
use super::gl::SkiaState;
use super::ime::{im_cursor_rect, Commit, FocusChange, GtkFocus, ImeGate};
use super::input_queue::{InputQueue, PasteTicket};
use super::keys::{self, ClipboardChord, DeliveredKeys, RawKey, RepeatTracker};

/// Phase 1's face: the frozen pane's own default. Phase 4 takes nvim's `guifont` (spec §2.7).
const FONT_FAMILY: &str = "monospace";
const FONT_SIZE_PT: f32 = 13.0;

/// How long `Ctrl+Shift+V` waits for the clipboard before typing goes on without the paste
/// (`input_queue`'s module doc). A read from this process takes microseconds and one through a
/// Wayland pipe milliseconds; two seconds is a clipboard owner that is not answering.
pub(crate) const PASTE_TIMEOUT: Duration = Duration::from_secs(2);

struct State {
    skia: Option<SkiaState>,
    /// `None` until [`ensure_session`] builds it, the first time a shell is actually about to
    /// spawn. Never built at construction (M8/task-8 minor): a window whose terminal is never
    /// shown must not pay for a font manager and a fontconfig match at every launch.
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
    /// Keyvals whose press this pane forwarded, so it knows which releases are its to forward too
    /// (review 2026-09-23, M2).
    delivered: DeliveredKeys,
    /// What is bound for the shell and waits behind a paste still being read (phase 2).
    queue: InputQueue,
    /// The bell's flash (phase 2).
    bell: BellFlash,
    colors: TerminalColors,
    focused: bool,
    cwd: PathBuf,
    /// Told when the shell ends in a way that closes the module (`super::closes_on_exit`); set by
    /// `main.rs` ([`TerminalPane::on_shell_exit`]). Called with no borrow of this state held.
    on_shell_exit: Option<Rc<dyn Fn() -> bool>>,
}

impl State {
    /// The construction seam `TerminalPane::new` uses. Deliberately GTK-free (no field here holds
    /// a `gtk4` type instantiated by anything but `None`), so a test can assert what a pane's state
    /// looks like the instant it exists -- in particular, that it holds no metrics -- with no
    /// display and no `gtk4::init` (review 2026-09-23, M8/task-8 minor).
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

    /// A new frame's cursor, as the session reported it with the frame. A frame without one (a
    /// contained panic's) makes it unknown rather than leaving the old cell in place. `true` when the
    /// input method must be pointed again: the cursor moved while the terminal holds the keys. fcitx5
    /// places the first candidate window of a composition before any `preedit-*` has told it where
    /// (review S3), so it has to have been told already -- by this, and on gaining the keys.
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

    /// Straight to the running shell; dropped when there is none (not started, or exited).
    fn send_input(&self, input: NormalizedInput) {
        if let (Some(session), false) = (&self.session, self.exited) {
            session.send(SessionCommand::Input(input));
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
        connect_render(&area, &state);
        connect_keyboard(&area, &state, &im);
        connect_ime(&area, &state, &im);
        connect_click_to_focus(&area);
        connect_visibility(&area, &state, &im);
        TerminalPane { area, state, im }
    }

    /// The module host `main.rs` adds to the grid. The keys reach it through `main.rs`'s
    /// `focus_module`, as every module's do since the modules design's P1 re-homed the terminal
    /// (which refuses a hidden module): the pane's own `grab_focus` went with that.
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
    /// must not ride it that way:** a report is input, and one sent here would overtake keys still
    /// waiting behind a paste (whole-branch review 2026-09-24, window minor 6).
    pub(crate) fn set_focused(&self, focused: bool) {
        let change = {
            let mut state = self.state.borrow_mut();
            state.focused = focused;
            if let Some(session) = &state.session {
                session.send(SessionCommand::Focus(focused));
            }
            state.ime.set_focused(focused)
        };
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
        submit(&self.state, input);
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

/// Builds the pane's font metrics at `allocation`, containing a font-manager panic rather than
/// letting it take the whole process down (review 2026-09-23, M8/task-8 minor): a machine with no
/// usable monospace font must fail with a message in the pane, like a shell that would not start,
/// not crash the window.
///
/// **At the allocation, never at a placeholder** (GUI pass 2026-09-23, defect 1). The first version
/// built these at 1x1, "exactly as `TerminalPane::new` used to build it eagerly", trusting the next
/// resize to correct cols/rows. Eagerly, that was true: the first resize found metrics and sized
/// them before the spawn. Lazily it is not: the first resize is the one that finds none, the spawn
/// follows it at 1x1, and there is no next resize until the pane's size changes.
fn build_metrics(allocation: Allocation) -> Result<TerminalMetrics, String> {
    let Allocation { width, height, scale } = allocation;
    panic::catch_unwind(|| TerminalMetrics::with_font(FONT_FAMILY, FONT_SIZE_PT, width, height, scale)).map_err(
        |payload| {
            payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
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
            if let Some(session) = &state.borrow().session {
                session.send(SessionCommand::Visible(true));
            }
        });
    }
    let state = state.clone();
    area.connect_unmap(move |_| {
        if let Some(session) = &state.borrow().session {
            session.send(SessionCommand::Visible(false));
        }
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
    let Some(update) = state.borrow().session.as_ref().map(|s| s.take_update()) else {
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
    if let Some(until) = flash_until {
        area.queue_render();
        let area = area.downgrade();
        // `bell::repaint_delay`, not the raw `until - now`: glib truncates a `Duration` to whole
        // milliseconds, so the raw remainder fires up to 1 ms before `until` while the flash is
        // still active, and nothing else would then repaint the pane to clear it (review 2026-09-24).
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
        // A hidden or not-yet-laid-out pane can report no area. That must never reach the PTY: a
        // terminal keeps its last size while hidden (modules design §3.2).
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
        if let (Some(size), Some(session)) = (resized, state.borrow().session.as_ref()) {
            session.send(SessionCommand::Resize(size));
        }
        ensure_session(&state, area, &im);
    });
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
                neovibe_terminal::paint(surface.canvas(), list, metrics);
                // After the frame, over it: the input method's composition at the cursor (phase 2).
                if let Some(preedit) = &preedit {
                    neovibe_terminal::paint_ops(surface.canvas(), &preedit.ops, metrics);
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

/// Clicking the pane focuses it. GTK4 does not do this for a plain widget; the frozen pane's own
/// doc records the editor losing click-focus the day a second focusable pane appeared.
fn connect_click_to_focus(area: &GLArea) {
    let click = GestureClick::new();
    click.set_button(0);
    let target = area.clone();
    click.connect_pressed(move |_, _, _, _| {
        if !target.has_focus() {
            target.grab_focus();
        }
    });
    area.add_controller(click);
}

fn connect_keyboard(area: &GLArea, state: &Rc<RefCell<State>>, im: &IMMulticontext) {
    let keys = EventControllerKey::new();
    // GTK runs the input method on every key before `key-pressed`/`key-released` below, and a key it
    // consumes never reaches them (phase 2; the editor's own wiring, `neovide-editor/src/keyboard.rs`).
    // neovibe's own chords never get this far: they are taken in the capture phase.
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
            match keys::clipboard_chord(keyval, modifier) {
                Some(ClipboardChord::Copy) => {
                    println!("[terminal] Ctrl+Shift+C: nothing can be selected yet (phase 3); held back");
                    return glib::Propagation::Stop;
                }
                Some(ClipboardChord::Paste) => {
                    paste_clipboard(&state, &area, &im);
                    return glib::Propagation::Stop;
                }
                None => {}
            }
            // Recorded before delivery: a release is forwarded only for a keyval whose press
            // reached here (review 2026-09-23, M2), never re-derived from the release's own
            // modifier state, which can have changed (Shift let go before a clipboard chord's
            // letter, for one).
            state.borrow_mut().delivered.mark(keyval);
            deliver(&state, raw_key(&area, keyval, keycode, modifier, true, repeat));
            // Claimed unconditionally: every key that reaches the terminal is the child's. The keys
            // neovibe keeps were taken before this controller ever saw them (spec §2.5).
            glib::Propagation::Stop
        });
    }
    {
        let state = state.clone();
        keys.connect_key_released(move |controller, keyval, keycode, modifier| {
            state.borrow_mut().repeats.release(keyval);
            // A release reaches this pane for a keyval whose press never did -- the key that moved
            // focus in, the real key behind a `Ctrl+a Ctrl+a`/`Ctrl+a Ctrl+l` literal, a HINT label.
            // Forward only what was actually delivered (review 2026-09-23, M2).
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
            let verdict = state.borrow_mut().ime.commit(text, gtk);
            match verdict {
                Commit::Send(input) => submit(&state, input),
                Commit::Dropped(why) => println!(
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
fn preedit_changed(im: &IMMulticontext, state: &RefCell<State>, area: &GLArea) {
    let (text, _attributes, caret) = im.preedit_string();
    state.borrow_mut().ime.preedit_changed(&text, caret);
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
    submit(state, input);
}

/// The one door to the shell: straight through, or behind a paste still being read.
fn submit(state: &RefCell<State>, input: NormalizedInput) {
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
    discard_composition(state, im, area);
    let ticket = state.borrow_mut().queue.begin_paste();
    let read = area.clipboard().read_text_future();
    {
        let state = Rc::downgrade(state);
        glib::spawn_future_local(async move {
            let text = match read.await {
                Ok(Some(text)) => text.to_string(),
                Ok(None) => {
                    println!("[terminal] Ctrl+Shift+V: the clipboard holds no text");
                    String::new()
                }
                Err(err) => {
                    eprintln!("[terminal] Ctrl+Shift+V: could not read the clipboard: {err}");
                    String::new()
                }
            };
            let Some(state) = state.upgrade() else { return };
            let input = keys::paste(&text);
            let had_text = input.is_some();
            if !state.borrow_mut().finish_paste(ticket, input) && had_text {
                eprintln!("[terminal] Ctrl+Shift+V: the clipboard answered too late; that paste is dropped");
            }
        });
    }
    let state = Rc::downgrade(state);
    glib::timeout_add_local_once(PASTE_TIMEOUT, move || {
        let Some(state) = state.upgrade() else { return };
        if state.borrow_mut().finish_paste(ticket, None) {
            eprintln!("[terminal] Ctrl+Shift+V: no answer from the clipboard in {PASTE_TIMEOUT:?}; typing goes on");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review 2026-09-23, M8/task-8 minor: an unused, hidden terminal must not build metrics -- a
    /// Skia `FontMgr`, a fontconfig match, and (before this fix) an `.expect("no usable font")`
    /// panic path -- at every launch, only because a window exists. `State::new` is the
    /// construction seam `TerminalPane::new` uses before any `gtk4::GLArea` is even built, so this
    /// needs no display and no `gtk4::init` -- consistent with every other test in this module,
    /// none of which touches real GTK.
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

    /// The pane's own allocation in the GUI pass that found defect 1: 1280x234 device pixels at
    /// scale 1, which `d6d97f9` (metrics built eagerly) spawned at 13x160.
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
        assert_eq!(state.queue.submit(typed.clone()), None, "it waits behind the paste");
        state.exited = true;
        state.forget_session();
        assert!(!state.exited);
        assert!(
            !state.finish_paste(ticket, None),
            "that paste is gone with the old shell"
        );
        assert_eq!(state.queue.submit(typed.clone()), Some(typed), "nothing waits any more");
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

    /// Review S3: fcitx5 shows the first candidate window of a composition before any `preedit-*`
    /// has told it where, so it must already have been pointed at the cursor. A frame that moves
    /// the cursor while the terminal holds the keys says so; one that does not move it, or moves it
    /// while the keys are elsewhere, does not (and a visibility toggle alone is not a move).
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

    /// Review N2: a frame that carries no cursor (a contained panic's) and a restart both make the
    /// cursor unknown, and a composition is then drawn nowhere rather than at the old cell.
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

    /// Bottom-terminal phase 2: a bell rung while the terminal was hidden comes out with the first
    /// update after it is shown again, and must not flash then; one rung on screen flashes.
    ///
    /// This only reaches `State::bell`'s `on_screen` branch (review 2026-09-24, minor): it does not
    /// drive `connect_visibility`'s map handler, which is what actually calls `pump(.., false)`
    /// before sending `Visible(true)`, nor the session's own folding of a hidden bell
    /// (`session.rs:571-576`) into one `Update` never woken for. That whole path needs a display and
    /// is Task 6 item 8's GUI-pass job; this test is the pure half only.
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
}
