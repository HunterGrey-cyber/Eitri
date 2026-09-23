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
//! `TerminalMetrics` is built lazily by [`ensure_session`], so an unused terminal never touches
//! a font manager. If no usable monospace font exists, the pane shows its failure frame instead
//! of panicking the process.

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

/// Phase 1's face: the frozen pane's own default. Phase 4 takes nvim's `guifont`.
const FONT_FAMILY: &str = "monospace";
const FONT_SIZE_PT: f32 = 13.0;

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
    repeats: RepeatTracker,
    /// Keyvals whose presses this pane forwarded, used to forward only their matching releases.
    delivered: DeliveredKeys,
    colors: TerminalColors,
    focused: bool,
    cwd: PathBuf,
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

    pub(crate) fn widget(&self) -> &GLArea {
        &self.area
    }

    pub(crate) fn grab_focus(&self) -> bool {
        self.area.grab_focus()
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
            // Remember delivery before forwarding the press. Forward a release only if its press reached
            // this pane. Do not infer delivery from release modifiers: Shift may have been released
            // before the letter in a clipboard chord.
            state.borrow_mut().delivered.mark(keyval);
            deliver(&state, raw_key(&area, keyval, keycode, modifier, true, repeat));
            // Claimed unconditionally: every key that reaches the terminal is the child's. The keys
            // neovibe keeps were taken before this controller ever saw them.
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
