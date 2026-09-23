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
//! **The font metrics are lazy too, and never panic** (review 2026-09-23, M8/task-8 minor). A
//! window whose terminal is never shown never touches a font manager: `TerminalMetrics` is built
//! by [`ensure_session`], the same gate that lazily spawns the shell, not at [`TerminalPane::new`].
//! A machine with no usable monospace font shows the pane's own failure frame -- the same one a
//! shell that would not start shows -- rather than panicking the whole process.

use std::cell::RefCell;
use std::panic;
use std::path::PathBuf;
use std::rc::Rc;

use gtk4::gdk::ModifierType;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{EventControllerFocus, EventControllerKey, GLArea, GestureClick};

use neovibe_terminal::pty::{fallback_shell, passwd_shell};
use neovibe_terminal::{
    PtySize, Screen, SessionCommand, SessionConfig, SpawnSpec, TerminalColors, TerminalMetrics, TerminalSession,
};
use terminal_input::NormalizedInput;
use terminal_render::PaintList;

use super::gl::SkiaState;
use super::keys::{self, DeliveredKeys, RawKey, RepeatTracker};

/// Phase 1's face: the frozen pane's own default. Phase 4 takes nvim's `guifont` (spec §2.7).
const FONT_FAMILY: &str = "monospace";
const FONT_SIZE_PT: f32 = 13.0;

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
    repeats: RepeatTracker,
    /// Keyvals whose press this pane forwarded, so it knows which releases are its to forward too
    /// (review 2026-09-23, M2).
    delivered: DeliveredKeys,
    colors: TerminalColors,
    focused: bool,
    cwd: PathBuf,
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
            repeats: RepeatTracker::new(),
            delivered: DeliveredKeys::new(),
            colors: TerminalColors::default(),
            focused: false,
            cwd,
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
        connect_resize(&area, &state);
        connect_render(&area, &state);
        connect_keyboard(&area, &state);
        connect_click_to_focus(&area);
        connect_visibility(&area, &state);
        TerminalPane { area, state }
    }

    /// The module host `main.rs` adds to the grid. The keys reach it through `main.rs`'s
    /// `focus_module`, as every module's do since the modules design's P1 re-homed the terminal
    /// (which refuses a hidden module): the pane's own `grab_focus` went with that.
    pub(crate) fn widget(&self) -> &GLArea {
        &self.area
    }

    /// A shell is wanted: spawn it now if the pane has been laid out, else at its first allocation.
    pub(crate) fn start(&self) {
        self.state.borrow_mut().wanted = true;
        ensure_session(&self.state, &self.area);
    }

    /// Whether the pane holds the keys: a solid cursor or a hollow one (`pane_focus`'s rule).
    pub(crate) fn set_focused(&self, focused: bool) {
        let mut state = self.state.borrow_mut();
        state.focused = focused;
        if let Some(session) = &state.session {
            session.send(SessionCommand::Focus(focused));
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
    /// prefix's literal `Ctrl+a`/`Ctrl+l`. Dropped when no shell is running.
    pub(crate) fn send(&self, input: NormalizedInput) {
        let state = self.state.borrow();
        if let (Some(session), false) = (&state.session, state.exited) {
            session.send(SessionCommand::Input(input));
        }
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
fn ensure_session(state: &Rc<RefCell<State>>, area: &GLArea) {
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
    // Ends when the session's thread drops the waker, i.e. when the session is over.
    glib::spawn_future_local(async move {
        while woken.recv().await.is_ok() {
            let (Some(state), Some(area)) = (state.upgrade(), area.upgrade()) else {
                break;
            };
            pump(&state, &area);
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
fn connect_visibility(area: &GLArea, state: &Rc<RefCell<State>>) {
    for visible in [true, false] {
        let state = state.clone();
        let tell = move |_: &GLArea| {
            if let Some(session) = &state.borrow().session {
                session.send(SessionCommand::Visible(visible));
            }
        };
        if visible {
            area.connect_map(tell);
        } else {
            area.connect_unmap(tell);
        }
    }
}

/// Takes what the session published and schedules a repaint if there is a new frame.
fn pump(state: &RefCell<State>, area: &GLArea) {
    let Some(update) = state.borrow().session.as_ref().map(|s| s.take_update()) else {
        return;
    };
    let mut s = state.borrow_mut();
    if let Some(frame) = update.frame {
        s.frame = Some(frame);
        area.queue_render();
    }
    if let Some(exit) = update.exited {
        s.exited = true;
        println!("[terminal] {}", exit.notice());
    }
    // Phase 2 puts these on the GTK clipboard, the pane's header and a bell; phase 1 says so.
    if update.events.clipboard.is_some() {
        println!("[terminal] a program copied to the clipboard (OSC 52); not wired until phase 2");
    }
}

/// Enter after the shell exited: a fresh session on a fresh PTY, in the same place.
fn restart(state: &Rc<RefCell<State>>, area: &GLArea) {
    {
        let mut s = state.borrow_mut();
        s.session = None;
        s.exited = false;
    }
    ensure_session(state, area);
}

fn connect_resize(area: &GLArea, state: &Rc<RefCell<State>>) {
    let state = state.clone();
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
        ensure_session(&state, area);
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
            Some((list, metrics)) => neovibe_terminal::paint(surface.canvas(), list, metrics),
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

fn connect_keyboard(area: &GLArea, state: &Rc<RefCell<State>>) {
    let keys = EventControllerKey::new();
    {
        let state = state.clone();
        keys.connect_key_pressed(move |controller, keyval, keycode, modifier| {
            let Some(area) = controller.widget().and_downcast::<GLArea>() else {
                return glib::Propagation::Stop;
            };
            let repeat = state.borrow_mut().repeats.press(keyval);
            if state.borrow().exited {
                if keys::restarts(keyval, modifier) {
                    restart(&state, &area);
                }
                return glib::Propagation::Stop;
            }
            if keys::clipboard_chord(keyval, modifier) {
                println!("[terminal] Ctrl+Shift+C/V: copy and paste arrive in phase 2; held back meanwhile");
                return glib::Propagation::Stop;
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
    if let Some(session) = state.borrow().session.as_ref() {
        session.send(SessionCommand::Input(input));
    }
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
