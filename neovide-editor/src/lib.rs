//! neovide-editor: the embeddable Neovide/Skia/GTK4 editor surface, extracted from the
//! validated poc/neovide_embed_live probe, so the shell embeds the path that probe proved.
//!
//! `NeovideEditorPane` is this crate's public surface: a `GtkGLArea`-backed pane that owns a
//! `LiveHarness` (real `nvim --embed` connection) and wires resize/IME/keyboard/mouse/tick/render
//! callbacks onto it -- but, unlike the `poc/neovide_embed_live` probe it's extracted from, it
//! never constructs its own `Application`/`ApplicationWindow`. An embeddable editor surface must
//! not assume it owns the top-level window: creating the `Application`/`Window`, calling
//! `.present()`, and deciding what a window-close request should do are all left to the host
//! (e.g. the shell binary that embeds this pane). The one place the reference probe *did* reach
//! for its own `window` -- the tick callback calling `window.close()` directly when
//! `LiveHarness::has_neovim_exited()` comes back true on its own (nvim quit itself, e.g. via
//! `:qa!` typed inside it, not via the host closing anything) -- is replaced here by invoking a
//! caller-registered callback (`on_exited_unrequested`), so the host decides what "nvim exited
//! unrequested" should actually do to its own window. Similarly, the reference's
//! `connect_close_request` handler (which calls `LiveHarness::shutdown()` and logs the result)
//! becomes the public `shutdown()` method, for the host's own close-request handler to call.

mod dmabuf_target;
mod editor_area;
#[doc(hidden)]
pub mod exit_wake;
#[doc(hidden)]
pub mod fd_watch;
#[doc(hidden)]
pub mod subsurface_testing {
    pub use crate::subsurface::{
        BufferKey, ConnectionProbe, DmabufFeedback, FrameData, Globals, PaneId, PresentEvent, PresentSink,
        PresenterThread, TestCommand,
    };

    /// Whether this display provides a foreign Wayland connection, for display-isolated tests.
    pub fn connection_for(display: &gtk4::gdk::Display) -> Option<wayland_client::Connection> {
        crate::subsurface::connection_for(display)
    }
}
mod frame_clock;
mod gl_interop;
mod keyboard;
mod live_watch;
mod mouse;
mod nvim_child;
mod nvim_rpc;
mod stdin;
mod subsurface;
#[cfg(test)]
mod test_nvim;
mod tick_driver;

/// Unit tests that acquire the default main context take this first: two at once would make the
/// second `acquire` fail.
#[cfg(test)]
pub(crate) static DEFAULT_CONTEXT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub use editor_area::PresentationCounts;
pub use nvim_rpc::{CallWatch, NvimMode};
pub use subsurface::Presentation;

/// The priority of the one watch on the editors' shared event-loop fd, `None` while no editor pane
/// on this thread has a live harness. Diagnostic: the multi-pane tests read it.
pub fn event_loop_watch_priority() -> Option<std::ffi::c_int> {
    live_watch::watch_priority()
}

/// What [`NeovideEditorPane::exec_lua`] calls with nvim's answer, on the session's call thread: the
/// value, or the error text (nvim's, or why the connection could not answer). It must not touch
/// anything that belongs to the GTK thread, and must return at once: every other call of the
/// session waits while it runs.
pub type LuaReply = Box<dyn FnOnce(Result<rmpv::Value, String>) + Send + 'static>;
/// Why an attached session ended on nvim's own initiative ([`PaneEnded::SessionLeft`]).
pub use neovide::live_harness::Left;
pub use stdin::{detach_stdin_from_nvim, ForwardedStdin};

use std::cell::{Cell, RefCell};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    EventControllerMotion, EventControllerScroll, EventControllerScrollFlags, GLArea, GestureClick, IMMulticontext,
};

use skia_safe::gpu::direct_contexts;
use skia_safe::{BlendMode, Color4f, Paint, SamplingOptions};

use neovide::live_harness::{Followed, LiveEvent, LiveHarness, LiveHarnessOptions};
use neovide::units::{GridScale, GridSize};

use frame_clock::AnimationClock;
use gl_interop::{
    compute_content_region, fill_content_region, grid_layout, grid_size_for_content_region, make_gl_interface,
    same_grid_scale, GridLayout, SkiaState,
};
use mouse::DragState;
use tick_driver::{
    service_pending_after_render, tick_still_wanted, PendingInputs, Service, ServiceSource, TickDriver, TickInputs,
    WantsFrame,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameReason {
    Batch,
    Animation,
    Focus,
    Theme,
    Map,
    Scale,
    Restart,
}

fn subsurface_on(widget: &GLArea) -> Option<Rc<subsurface::SubsurfacePane>> {
    widget.downcast_ref::<editor_area::EditorGlArea>()?.subsurface()
}

fn request_frame_on(widget: &GLArea, reason: FrameReason) {
    if reason == FrameReason::Restart {
        if let Some(log) = widget
            .downcast_ref::<editor_area::EditorGlArea>()
            .and_then(|area| area.present_log())
        {
            log.missing();
        }
    }
    if let Some(pane) = subsurface_on(widget) {
        pane.request(reason);
    } else {
        widget.queue_render();
    }
}

fn set_clear_color_and_request(clear_color: &Cell<Color4f>, color: (u8, u8, u8), request: impl FnOnce(FrameReason)) {
    clear_color.set(rgb_color(color));
    request(FrameReason::Theme);
}

fn set_focused_and_request(
    focused: &Cell<Option<bool>>,
    live_state: &RefCell<LiveState>,
    value: bool,
    request: impl FnOnce(FrameReason),
) {
    focused.set(Some(value));
    if let LiveState::Ready(session) = &mut *live_state.borrow_mut() {
        session.harness.set_focused(value);
        session.wants_frame.set(true);
    }
    request(FrameReason::Focus);
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum RenderTarget {
    GtkFramebuffer {
        os_scale: f64,
    },
    OwnTexture {
        os_scale: f64,
    },
    Subsurface {
        framebuffer: u32,
        buffer: (i32, i32),
        content: (i32, i32),
        os_scale: f64,
    },
}

type RenderFn = Rc<dyn Fn(&GLArea, RenderTarget) -> glib::Propagation>;

fn current_target(widget: &GLArea) -> RenderTarget {
    let os_scale = f64::from(widget.scale_factor().max(1));
    if widget
        .downcast_ref::<editor_area::EditorGlArea>()
        .is_some_and(editor_area::EditorGlArea::draws_into_own_buffer)
    {
        RenderTarget::OwnTexture { os_scale }
    } else {
        RenderTarget::GtkFramebuffer { os_scale }
    }
}

struct PaneShared {
    skia_state: Rc<RefCell<Option<SkiaState>>>,
    live_state: Rc<RefCell<LiveState>>,
    fb_size: Rc<Cell<(i32, i32)>>,
    device_scale: Rc<Cell<f64>>,
}

impl PaneShared {
    fn device_scale(&self) -> f64 {
        self.device_scale.get()
    }
}

fn framebuffer_resized(state: &PaneShared, width: i32, height: i32, os_scale: f64) {
    let PaneShared {
        skia_state,
        live_state,
        fb_size,
        device_scale,
    } = state;
    device_scale.set(os_scale.max(1.0));
    // Keep the framebuffer and its scale together, including before nvim starts.
    fb_size.set((width, height));
    let mut live = live_state.borrow_mut();
    let (forced_next_frame, resized_grid) = if let LiveState::Ready(session) = &mut *live {
        session.wants_frame.set(true);
        // The cell size must match the new framebuffer before computing its grid.
        sync_os_scale(&mut session.harness, state.device_scale());
        let new_grid_size = grid_layout(width, height, session.harness.grid_scale(), |r| {
            grid_size_for_content_region(&session.harness, r)
        })
        .grid;
        let resized_grid = if new_grid_size != session.last_grid_size.get() {
            session.harness.resize_grid(new_grid_size);
            session.last_grid_size.set(new_grid_size);
            Some(new_grid_size)
        } else {
            None
        };
        (true, resized_grid)
    } else {
        (false, None)
    };
    drop(live);
    println!(
        "[resize] fb={}x{}px scale_factor={} logical={}x{} forced_next_frame={} resized_grid={:?}",
        width,
        height,
        state.device_scale(),
        f64::from(width) / state.device_scale(),
        f64::from(height) / state.device_scale(),
        forced_next_frame,
        resized_grid.map(|g| (g.width, g.height)),
    );
    if let Some(state) = skia_state.borrow_mut().as_mut() {
        state.fb_width = width;
        state.fb_height = height;
        // A resize invalidates the surface even if GL reused its framebuffer name.
        state.gtk_surface = None;
    }
}

/// Log a frame-pacing line every N frames instead of spamming stdout every frame.
const LOG_EVERY_N_FRAMES: u64 = 60;

/// How many service runs (tick callback or fd watch) between `[tick]` summary log lines (see `TickStats`).
const TICK_LOG_EVERY_N_TICKS: u64 = 300;

/// Initial framebuffer clear colour before the host supplies a theme colour.
///
/// The render callback clears the whole framebuffer before `LiveHarness::render_frame`.
/// Within `content_region`, `Ready` clips `Renderer::draw_frame` and clears to nvim's `Normal`
/// background with an `SkBlendMode::kSrc` fill. The other states paint opaque `STARTING_COLOR`
/// or `FAILED_COLOR` fills.
///
/// Even with `CONTENT_MARGIN` at 0, `snap_region_to_grid` leaves the vertical sub-cell remainder
/// above the grid so its bottom is flush with the host's status bar. That band lies outside
/// `content_region` and shows the framebuffer clear colour. The host uses
/// [`NeovideEditorPane::set_clear_color`] to match `tokens.bg` at each colorscheme change.
/// This near-black value shows until the host supplies its first colour.
const OUTSIDE_COLOR: Color4f = Color4f::new(0.102, 0.106, 0.122, 1.0);
/// Painted across the whole `content_region` while `LiveHarness` is being constructed (the one
/// real, observed blocking call in this crate) -- distinct from both `OUTSIDE_COLOR` and anything
/// the real renderer would draw, so it's obvious on screen which phase is showing.
const STARTING_COLOR: Color4f = Color4f::new(0.12, 0.12, 0.16, 1.0);

/// Converts RGB bytes to an opaque colour for drawing.
fn rgb_color((r, g, b): (u8, u8, u8)) -> Color4f {
    Color4f::new(f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0, 1.0)
}

/// Uses the host's starting colour when supplied, otherwise the pane's own placeholder colour.
fn placeholder_color(starting: Option<Color4f>) -> Color4f {
    starting.unwrap_or(STARTING_COLOR)
}

/// The colour the subsurface path paints where the editor's hole is closed: the frame's clear colour once nvim is
/// ready, and before that the same placeholder the GTK path shows.
fn hole_cover_color(nvim_ready: bool, clear: Color4f, starting: Option<(u8, u8, u8)>) -> Color4f {
    if nvim_ready {
        clear
    } else {
        placeholder_color(starting.map(rgb_color))
    }
}

/// Covers incomplete nvim frames with the host's starting colour until the UI is ready.
fn pre_ready_cover(starting: Option<Color4f>, nvim_ready: bool) -> Option<Color4f> {
    if nvim_ready {
        None
    } else {
        starting
    }
}
/// Painted across `content_region` if `LiveHarness::with_options` itself returned `Err` (e.g. no
/// `nvim` on `$PATH`) -- distinct from every other state color here.
const FAILED_COLOR: Color4f = Color4f::new(0.5, 0.05, 0.05, 1.0);

/// Lifecycle of the `LiveHarness` this pane drives, kept explicit (rather than a bare
/// `Option<LiveHarness>`) so the render callback can paint a "starting nvim..." placeholder frame
/// *before* the one real, observed blocking call in this crate (`LiveHarness::with_options`) runs.
/// `NotStarted` -> one placeholder frame painted+presented -> `Starting` -> next render callback
/// performs the blocking construction -> `Ready`/`Failed`. A restart
/// ([`NeovideEditorPane::restart`]) takes an `Exited` or `Failed` pane back to `NotStarted`, and the
/// same path runs again on the same widget.
pub enum LiveState {
    NotStarted,
    Starting,
    Ready(Box<LiveSession>),
    Failed(String),
    /// nvim quit and the host let this pane go ([`NeovideEditorPane::release_exited`]), or the host
    /// shut the pane down before it launched its nvim ([`NeovideEditorPane::shutdown`]): the harness
    /// is shut down and dropped (or was never built), so nothing -- a focus report, a resize, a
    /// key -- can reach its closed command channel (`send_ui` panics on one). The process can start another
    /// `nvim --embed`: harnesses share one event loop per thread, so a new one joins it rather than
    /// building a second winit `EventLoop`. [`NeovideEditorPane::restart`] starts one in this pane.
    Exited,
}

impl LiveState {
    /// The variant's name, for a log line or a refusal that says which state it found.
    pub fn name(&self) -> &'static str {
        match self {
            LiveState::NotStarted => "NotStarted",
            LiveState::Starting => "Starting",
            LiveState::Ready(_) => "Ready",
            LiveState::Failed(_) => "Failed",
            LiveState::Exited => "Exited",
        }
    }
}

/// Why [`NeovideEditorPane::restart`] did nothing: the pane was neither [`LiveState::Exited`] nor
/// [`LiveState::Failed`], or it is attached to an nvim it did not start. `state` is the name of the state it was in
/// ([`LiveState::name`]), or [`RestartRefused::ATTACHED`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartRefused {
    pub state: &'static str,
}

impl RestartRefused {
    /// The `state` of a refusal by an attached pane ([`NeovideEditorPane::is_attached`]), whatever its
    /// [`LiveState`]: it never starts an nvim of its own in place of the one it was attached to.
    pub const ATTACHED: &'static str = "Attached";
}

impl std::fmt::Display for RestartRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.state == Self::ATTACHED {
            return f.write_str(
                "the editor pane is attached to an nvim it did not start, and never starts one of its own in its place",
            );
        }
        write!(
            f,
            "the editor pane is {}, and only a pane whose nvim ended and was released, or whose launch failed, can restart",
            self.state
        )
    }
}

impl std::error::Error for RestartRefused {}

/// The refusal [`NeovideEditorPane::restart`] returns from `state`, or `None` when it may restart:
/// only from `Exited` or `Failed`, the two states that hold no harness (a failed launch never made
/// one). Every other state still has, or is about to have, a harness of its own, and a second one
/// started over it would leave the first running with nothing to end it.
fn restart_refusal(state: &LiveState) -> Option<RestartRefused> {
    match state {
        LiveState::Exited | LiveState::Failed(_) => None,
        other => Some(RestartRefused { state: other.name() }),
    }
}

/// `(fb_w, fb_h, cell_w bits, cell_h bits, cols, rows)`: what `LiveSession::log_layout_if_changed`
/// compares to decide whether the layout changed. Named for `clippy::type_complexity`.
type LayoutLogKey = (i32, i32, u32, u32, u32, u32);

pub struct LiveSession {
    pub(crate) harness: LiveHarness,
    /// This pane's registration with the event-loop watch (`live_watch`), held for as long as the
    /// harness: the registry keeps only a weak reference.
    live: Rc<live_watch::PaneLive>,
    start: Instant,
    last_frame: Instant,
    animation_clock: AnimationClock,
    frame_count: u64,
    logged_ready: bool,
    /// `render_frame`'s own returned `animating` value, set after every render callback
    /// invocation below. Starts `true` so the tick driver keeps running through
    /// the first few Ready-state frames, until a real `render_frame` call has actually reported a
    /// real value -- erring toward "render" rather than "skip" whenever this value hasn't been
    /// established yet.
    last_animating: Cell<bool>,
    /// `harness.redraw_batches_seen()` as of the last time either the render callback or the
    /// service run looked at it. The service (run by the tick while a frame is wanted, and by the
    /// event-loop fd watch the moment a batch arrives) calls `LiveHarness::pump`, so a change here
    /// is seen without waiting for a tick and even on runs that don't render -- this is the "did
    /// nvim actually send anything new" half of the fix, independent of `last_animating`.
    last_seen_batches: Cell<u64>,
    /// Set by the resize handler and the other input handlers that decide a frame is owed (a
    /// resize needs its own frame at the new size even if nvim sent nothing new). A plain key press
    /// no longer sets it: the frame that shows a key's effect follows nvim's redraw, which the fd
    /// watch sees. Setting it also makes sure the tick driver runs (`WantsFrame::set`).
    /// Read-and-cleared by each service run.
    pub(crate) wants_frame: WantsFrame,
    /// The grid (cols, rows) size nvim was last asked to resize to via
    /// `LiveHarness::resize_grid`. Set once at construction (to whatever the initial
    /// `content_region` computed to) and re-checked on every `connect_resize` callback so a real
    /// RPC is only sent when the *grid-cell* size actually changes, not on every pixel-level
    /// resize event mid-drag.
    last_grid_size: Cell<GridSize<u32>>,
    /// Set once the tick callback has invoked the registered `on_exited_unrequested` callback (if
    /// any) because `LiveHarness::has_neovim_exited()` came back true on its own (i.e. nvim quit
    /// itself, e.g. via `:qa!` typed inside it -- not via the host calling `shutdown()`). This is
    /// the "dead-looking-alive window" fix, adapted for a pane that doesn't own a `Window`:
    /// `has_neovim_exited()` is monotonic (never goes back to `false`), so without this guard the
    /// tick callback would fire the callback again on every subsequent tick until the host's own
    /// teardown actually completes -- harmless in principle but noisy (a duplicate
    /// `[live] nvim exited...` log line and a duplicate callback invocation every tick in
    /// between). Read-and-set via `Cell::replace` so the check-and-flag is a single atomic step
    /// from this single-threaded GTK main-loop caller's point of view.
    ///
    /// Also set (via a plain `Cell::set(true)`, not `replace`) by `NeovideEditorPane::shutdown()`
    /// itself, for the opposite direction: a *host*-initiated shutdown must trip this same guard
    /// so a later tick doesn't independently notice `has_neovim_exited() == true` once nvim
    /// actually exits and fire `on_exited_unrequested` a second time for a shutdown the host
    /// already knows about.
    close_requested: Cell<bool>,
    /// Set on a `GestureClick` press, cleared on its matching release -- see `mouse::DragState`'s
    /// own doc for what this tracks and why.
    pub(crate) active_drag: Cell<Option<DragState>>,
    /// Running fractional (x, y) grid-line scroll accumulator: only the *change* in `floor()`
    /// between one scroll event and the next determines how many whole-grid-line `Scroll` RPCs to
    /// send (see `mouse::handle_mouse_scroll`), so this is never reset back to zero -- only ever
    /// added to, for the life of the session.
    pub(crate) scroll_position: Cell<(f32, f32)>,
    /// Widget-local logical-pixel pointer position last reported by `EventControllerMotion`'s
    /// `motion`/`enter` signals. `GtkEventControllerScroll`'s own `scroll` signal carries no
    /// position at all, so a scroll event's target grid cell has to come from here -- the last
    /// place the pointer was actually seen.
    pub(crate) last_pointer_pos: Cell<(f64, f64)>,
    /// The pane's one framebuffer size (device pixels), as GTK last reported it through
    /// `GLArea::resize` -- shared with the render callback, the resize handler and the tick, so
    /// the mouse handlers hit-test against exactly the size the frame was drawn at
    /// (`gl_interop::current_content_region`); see `gl_interop::GridLayout`.
    pub(crate) fb_size: Rc<Cell<(i32, i32)>>,
    device_scale: Rc<Cell<f64>>,
    /// `(fb_w, fb_h, cell_w bits, cell_h bits, cols, rows)` as the `[layout]` line last printed
    /// it, so the line is printed when the layout changes and never per frame. Bits, not f32, so
    /// a NaN cell before nvim reports a font does not count as a change every frame.
    last_logged_layout: Cell<Option<LayoutLogKey>>,
    /// The `nvim --embed` process itself (`nvim_child`'s module doc): what a shutdown that timed out
    /// ends, and what [`NeovideEditorPane::end_nvim`] ends. Holds what the fork started from the
    /// launch on, and nvim's own `getpid()` answer once it comes back ([`ask_nvim_for_its_pid`]);
    /// shared with the thread that asks.
    ///
    /// `None` for a session attached to an nvim this pane did not start ([`AttachSpec`]): no process
    /// was looked for at its launch and nvim is never asked for its pid, so nothing here can watch,
    /// signal or end it. Every ending of such a session is a hang-up of this UI's channel, after
    /// which nvim goes on.
    nvim_process: Option<Arc<nvim_child::NvimProcess>>,
    /// Set by every hang-up this pane makes of an attached session (its shutdown, its drop, a
    /// [`HangUp`] handle) before the channel is closed, so the end that follows is told from one
    /// nvim made ([`PaneEnded`]).
    detached: Arc<AtomicBool>,
    /// The last [`NeovideEditorPane::exec_lua_watched`] request and its watch (`nvim_rpc`).
    watched: Option<nvim_rpc::WatchedCall>,
    /// Every request this session makes for a caller, written to nvim in the order they were made
    /// (`nvim_rpc`'s module doc); started by the first one.
    calls: std::cell::OnceCell<nvim_rpc::CallLine>,
    /// When the ending started, and on which schedule ([`NeovideEditorPane::end_nvim`]), so a
    /// second ending -- the window's shutdown after it -- keeps to the first one's schedule and
    /// starts no second watch.
    ending: Cell<Option<(Instant, nvim_child::Schedule)>>,
    /// Whether [`NeovideEditorPane::on_nvim_unreachable`]'s callback has fired for this session.
    unreachable_said: Cell<bool>,
    /// When the fork first reported nvim's exit while nvim's own process still ran ([`LiveSession::wake_on_nvim_exit`]).
    outlived_at: Cell<Option<Instant>>,
    /// What runs the service once nvim's own process is gone, and once [`UNREACHABLE_GRACE`] has passed; armed when the
    /// fork reported the exit with the process still running, dropped before the session, and the pidfd watched, go.
    exit_wake: RefCell<Option<exit_wake::Wake>>,
    grace_wake: RefCell<Option<exit_wake::Wake>>,
    /// Which session this is ([`NeovideEditorPane::session_serial`]), never reused in the process.
    serial: u64,
    /// The pane's own nvim a followed `:connect` left, and its address: it runs on with no UI, unless
    /// a banged `:connect` stopped it. Held only to say whether it still runs
    /// ([`NeovideEditorPane::left_running`]); nothing signals or ends it.
    /// Whether this session's nvim is the server a followed `:restart` started: its parent was the nvim before
    /// it, so a launcher has nothing to do with how it outlives the fork's report.
    restarted: bool,
    left: Option<(Arc<nvim_child::NvimProcess>, Option<String>)>,
}

/// How long nvim's process may outlive the fork's report of its exit before the host is told nvim is out of reach. A
/// quit closes nvim's pipe a few milliseconds before its process is gone, which is no reason to say so; a launcher that
/// exits leaving nvim running is.
const UNREACHABLE_GRACE: Duration = Duration::from_millis(500);

/// The serial the next session takes. Process-wide, so the render callback that builds a session
/// needs no counter handed to it, and no two sessions of any pane share a number.
static NEXT_SESSION_SERIAL: AtomicU64 = AtomicU64::new(1);

impl Drop for LiveSession {
    fn drop(&mut self) {
        // Before the pidfd one may watch can close (`fd_watch`'s contract): the fields, `nvim_process` among them, drop
        // after this body.
        drop(self.exit_wake.borrow_mut().take());
        drop(self.grace_wake.borrow_mut().take());
        // The pane leaves the event-loop watch with its harness; the last pane out removes it.
        live_watch::unregister(Rc::as_ptr(&self.live));
        self.wants_frame.session_ended();
        // The harness drops after this body, and a harness not already shut down shuts itself down
        // then, pumping the shared loop: the other panes are swept once that is over (the sweep
        // is queued now and runs from the main loop, after the drop).
        live_watch::after_outside_pump(None);
    }
}

impl LiveSession {
    fn device_scale(&self) -> f64 {
        self.device_scale.get()
    }

    /// The fork reported nvim's exit and nvim's own process still runs. Nothing else runs the service again: the tick
    /// runs only while a frame is wanted, and a dead nvim's pipe wakes no event-loop watch. So two wakes do, each by
    /// queueing one sweep of the event-loop watch (`live_watch::after_outside_pump`), which services every pane as the
    /// shared fd would, in one pass, after the wake's own source is gone (the run may end this session): the process's
    /// exit (a watch on its pidfd, or a poll every [`exit_wake::EXIT_POLL`] without one), and the end of
    /// [`UNREACHABLE_GRACE`], after which a process still running is said to be out of reach. Armed once a session.
    fn wake_on_nvim_exit(&self) {
        // An attached session holds no process, so there is nothing to watch.
        let Some(process) = self.nvim_process.clone() else {
            return;
        };
        if self.outlived_at.get().is_some() {
            return;
        }
        self.outlived_at.set(Some(Instant::now()));
        let sweep = || live_watch::after_outside_pump(None);
        *self.exit_wake.borrow_mut() = Some(exit_wake::Wake::on_exit(
            process.exit_fd(),
            move || process.alive_within(Duration::ZERO) == Some(true),
            sweep,
        ));
        *self.grace_wake.borrow_mut() = Some(exit_wake::Wake::after(UNREACHABLE_GRACE, sweep));
    }

    /// Whether [`UNREACHABLE_GRACE`] has passed since the fork reported an exit nvim's process outlived.
    fn grace_over(&self) -> bool {
        self.outlived_at
            .get()
            .is_some_and(|at| at.elapsed() >= UNREACHABLE_GRACE)
    }

    fn new(
        harness: LiveHarness,
        grid_size: GridSize<u32>,
        fb_size: Rc<Cell<(i32, i32)>>,
        device_scale: Rc<Cell<f64>>,
        nvim_process: Option<Arc<nvim_child::NvimProcess>>,
        driver: &Rc<TickDriver>,
    ) -> Self {
        // A ready harness: the event-loop watch wakes this pane from now on.
        let live = live_watch::PaneLive::new(harness.event_loop_fd(), driver);
        live_watch::register(Rc::downgrade(&live));
        let now = Instant::now();
        Self {
            harness,
            live,
            start: now,
            last_frame: now,
            animation_clock: AnimationClock::default(),
            frame_count: 0,
            logged_ready: false,
            last_animating: Cell::new(true),
            last_seen_batches: Cell::new(0),
            wants_frame: WantsFrame::new(driver),
            last_grid_size: Cell::new(grid_size),
            close_requested: Cell::new(false),
            active_drag: Cell::new(None),
            scroll_position: Cell::new((0.0, 0.0)),
            last_pointer_pos: Cell::new((0.0, 0.0)),
            fb_size,
            device_scale,
            last_logged_layout: Cell::new(None),
            nvim_process,
            detached: Arc::new(AtomicBool::new(false)),
            watched: None,
            calls: std::cell::OnceCell::new(),
            ending: Cell::new(None),
            unreachable_said: Cell::new(false),
            outlived_at: Cell::new(None),
            exit_wake: RefCell::new(None),
            grace_wake: RefCell::new(None),
            serial: NEXT_SESSION_SERIAL.fetch_add(1, Ordering::Relaxed),
            restarted: false,
            left: None,
        }
    }

    /// The pane's side of a `:restart` or `:connect` the harness followed: the session goes on, on the
    /// new server, as a new session (its own serial; nothing of its end said yet; no quit or ending of
    /// the old nvim carried over). A followed `:restart` reached a server the pane's own nvim started,
    /// which is the pane's from now on: its pid is asked as at a launch, and held only if it is an
    /// `--embed` nvim. A followed `:connect` reached a server the pane did not start: from now on the
    /// session holds no process, so nothing asks its pid, signals it or ends it, and the pane's
    /// attached rules hold.
    fn follow(&mut self, how: Followed, learned: &LearnedNvim) -> PaneFollowed {
        self.serial = NEXT_SESSION_SERIAL.fetch_add(1, Ordering::Relaxed);
        self.close_requested.set(false);
        self.unreachable_said.set(false);
        self.outlived_at.set(None);
        *self.exit_wake.borrow_mut() = None;
        *self.grace_wake.borrow_mut() = None;
        self.watched = None;
        self.ending.set(None);
        self.detached = Arc::new(AtomicBool::new(false));
        self.wants_frame.set(true);
        learned.clear();
        match how {
            Followed::Restart => {
                let process = Arc::new(nvim_child::NvimProcess::new(None));
                println!("[live] the editor follows nvim's :restart to the server it started; asking it for its pid");
                ask_nvim_for_its_pid(&self.harness, &process, Some(learned.clone()));
                self.nvim_process = Some(process);
                self.restarted = true;
                PaneFollowed::Restart
            }
            Followed::Connect { left } => {
                println!(
                    "[live] the editor follows nvim's :connect to a server it did not start, attached from now on; \
                     the nvim it left ({}) is not signalled or ended",
                    left.as_deref().unwrap_or("no address")
                );
                self.left = self.nvim_process.take().map(|process| (process, left.clone()));
                PaneFollowed::Connect { left }
            }
        }
    }

    /// This session's call line, its thread started by the first call.
    fn call_line(&self) -> std::io::Result<&nvim_rpc::CallLine> {
        if let Some(line) = self.calls.get() {
            return Ok(line);
        }
        let line = nvim_rpc::CallLine::start("calls")?;
        Ok(self.calls.get_or_init(|| line))
    }

    /// The fork reports nvim exited, and nvim's own process is still alive. The fork's report
    /// follows the process IT started: a `nvim` launcher that does not `exec` exits -- or crashes
    /// -- and 500 ms later the fork says nvim exited (`bridge::run` waits that long for the IO
    /// stream, then reports regardless), while nvim runs on, re-parented, perhaps still writing a
    /// file. The fork takes no keys for it from then on. This is not an exit, and nothing
    /// that follows an exit -- the window's close, the editor's retirement -- happens until the
    /// process is gone ([`exit_events`]). `false` when no process is known to be nvim: then the
    /// fork's report is all there is.
    fn exit_pending(&self) -> bool {
        self.harness.has_neovim_exited() && self.nvim_alive() == Some(true)
    }

    /// What the pidfd on nvim's process says, now; `None` when no process is known to be nvim, as for
    /// an attached session, which holds none.
    fn nvim_alive(&self) -> Option<bool> {
        self.nvim_process
            .as_ref()
            .and_then(|process| process.alive_within(Duration::ZERO))
    }

    /// Whether this session is attached to an nvim the pane did not start.
    fn is_attached(&self) -> bool {
        self.nvim_process.is_none()
    }

    /// Closes an attached session's channel, marking the end as this pane's own first. nvim drops
    /// the UI and goes on.
    fn detach(&mut self) {
        self.detached.store(true, Ordering::SeqCst);
        self.harness.hang_up();
    }

    /// How the session ended, once the fork reports it ended (`None` before): an owned session's
    /// end is its nvim's exit; an attached one's is the `:connect` or `:restart` the fork reported,
    /// else this pane's hang-up, else nvim's own doing ([`pane_ended`]). Reads the fork's events, so the caller
    /// keeps the answer ([`NeovideEditorPane::ended`]).
    fn how_it_ended(&self) -> Option<PaneEnded> {
        if !self.harness.has_neovim_exited() {
            return None;
        }
        let mut left = None;
        while let Some(event) = self.harness.take_event() {
            match event {
                LiveEvent::SessionLeft(why) => left = Some(why),
            }
        }
        Some(pane_ended(
            self.is_attached(),
            self.detached.load(Ordering::SeqCst),
            left,
        ))
    }

    /// The schedule an ending starting now takes (`nvim_child::schedule`): nvim may be in a prompt
    /// when its `:confirm qall` request -- the only prompt this pane knows of -- has not come back.
    /// Without a process known to be nvim there is nothing to signal, and the hang-up comes first.
    fn ending_schedule(&self) -> nvim_child::Schedule {
        let quit_outstanding = self.watched.as_ref().is_some_and(|call| !call.watch().done);
        let nvim_known = self
            .nvim_process
            .as_ref()
            .is_some_and(|process| process.nvim_pid().is_some());
        nvim_child::schedule(quit_outstanding && nvim_known)
    }

    /// Prints `[layout] fb=WxH cell=W×H grid=CxR band_top=Npx` when the layout differs from the
    /// last one printed. The GUI pass reads it: `band_top` under one cell means the grid ends
    /// flush at the bottom with the remainder above it; a blank *row* at the bottom with a small
    /// `band_top` is nvim's own cmdline (`cmdheight`), not this crate's geometry.
    fn log_layout_if_changed(&self, fb_w: i32, fb_h: i32, grid_scale: GridScale, layout: &GridLayout) {
        let key = (
            fb_w,
            fb_h,
            grid_scale.width().to_bits(),
            grid_scale.height().to_bits(),
            layout.grid.width,
            layout.grid.height,
        );
        if self.last_logged_layout.replace(Some(key)) != Some(key) {
            println!(
                "[layout] fb={fb_w}x{fb_h} cell={:.2}×{:.2} grid={}x{} band_top={:.1}px",
                grid_scale.width(),
                grid_scale.height(),
                layout.grid.width,
                layout.grid.height,
                layout.band_top_px,
            );
        }
    }

    /// Advance real elapsed time and return (dt, instantaneous_fps).
    fn tick(&mut self) -> (f32, f32) {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32();
        self.last_frame = now;
        self.frame_count += 1;
        let fps = if dt > 0.0 { 1.0 / dt } else { 0.0 };
        (dt, fps)
    }
}

/// Counts issued and skipped `queue_render()` calls over windows of `TICK_LOG_EVERY_N_TICKS`
/// service runs. `ServiceSource` distinguishes tick callbacks, registered while a frame is wanted,
/// from event-loop fd wake-ups while idle. Their counts remain separate. An idle pane should
/// mostly skip rendering; typing, scrolling and animation should mostly issue renders.
struct TickStats {
    /// Service runs in this window, from either driver; the window closes at
    /// `TICK_LOG_EVERY_N_TICKS`.
    runs: Cell<u64>,
    tick_runs: Cell<u64>,
    fd_runs: Cell<u64>,
    issued: Cell<u64>,
    skipped: Cell<u64>,
    tick_runs_total: Cell<u64>,
    fd_runs_total: Cell<u64>,
    issued_total: Cell<u64>,
    skipped_total: Cell<u64>,
}

impl TickStats {
    fn new() -> Self {
        Self {
            runs: Cell::new(0),
            tick_runs: Cell::new(0),
            fd_runs: Cell::new(0),
            issued: Cell::new(0),
            skipped: Cell::new(0),
            tick_runs_total: Cell::new(0),
            fd_runs_total: Cell::new(0),
            issued_total: Cell::new(0),
            skipped_total: Cell::new(0),
        }
    }

    /// Record one service run's outcome and which driver ran it; every `TICK_LOG_EVERY_N_TICKS`
    /// runs, print a `[tick]` summary of the just-finished window and reset the windowed counters
    /// (the `_total` counters keep accumulating for the life of the process).
    fn record(&self, source: ServiceSource, issued_this_run: bool) {
        self.runs.set(self.runs.get() + 1);
        let (window, total) = match source {
            ServiceSource::Tick => (&self.tick_runs, &self.tick_runs_total),
            ServiceSource::FdWatch => (&self.fd_runs, &self.fd_runs_total),
        };
        window.set(window.get() + 1);
        total.set(total.get() + 1);
        if issued_this_run {
            self.issued.set(self.issued.get() + 1);
            self.issued_total.set(self.issued_total.get() + 1);
        } else {
            self.skipped.set(self.skipped.get() + 1);
            self.skipped_total.set(self.skipped_total.get() + 1);
        }

        if self.runs.get() >= TICK_LOG_EVERY_N_TICKS {
            let runs = self.runs.get();
            let issued = self.issued.get();
            let skipped = self.skipped.get();
            println!(
                "[tick] last {runs} service runs (tick={} fd_watch={}): issued={issued} skipped={skipped} \
                 skip_ratio={:.1}% (cumulative tick={} fd_watch={} issued={} skipped={})",
                self.tick_runs.get(),
                self.fd_runs.get(),
                (skipped as f64 / runs as f64) * 100.0,
                self.tick_runs_total.get(),
                self.fd_runs_total.get(),
                self.issued_total.get(),
                self.skipped_total.get(),
            );
            self.runs.set(0);
            self.tick_runs.set(0);
            self.fd_runs.set(0);
            self.issued.set(0);
            self.skipped.set(0);
        }
    }
}

/// Slot for the callback `on_exited_unrequested` registers, shared with the tick callback set up
/// in `NeovideEditorPane::new()`. Factored into a named alias purely to satisfy
/// `clippy::type_complexity` -- no behavior difference from writing the nested type out inline.
type ExitedCallbackSlot = Rc<RefCell<Option<Box<dyn Fn()>>>>;
/// See [`NeovideEditorPane::on_nvim_unreachable`].
type UnreachableCallbackSlot = Rc<RefCell<Option<Box<dyn Fn()>>>>;
type FullscreenCallbackSlot = Rc<RefCell<Option<Box<dyn Fn(bool)>>>>;
type ScaleFactorCallbackSlot = Rc<RefCell<Option<Box<dyn Fn(f32)>>>>;
type CellSizeCallbackSlot = Rc<RefCell<Option<Box<dyn Fn(f64, f64)>>>>;
/// See [`NeovideEditorPane::on_start_failed`].
type StartFailedCallbackSlot = Rc<RefCell<Option<Box<dyn Fn(&str)>>>>;
/// See [`NeovideEditorPane::on_session_ready`].
type SessionReadyCallbackSlot = Rc<RefCell<Option<Box<dyn Fn()>>>>;
/// See [`NeovideEditorPane::on_followed`].
type FollowedCallbackSlot = Rc<RefCell<Option<Box<dyn Fn(PaneFollowed)>>>>;

/// The embeddable Neovide/Skia editor surface. Wraps a single `GtkGLArea` driving a real
/// `nvim --embed` connection (`neovide::live_harness::LiveHarness`), with resize/IME/keyboard/
/// mouse/tick/render callbacks all wired up by `new()`. Deliberately does not own an
/// `Application`/`ApplicationWindow` -- see this module's own doc for why -- so the host must
/// call `.widget()` to place this pane's `GLArea` into its own window, `.grab_focus()` once that
/// window is shown, and wire its own window's `connect_close_request` to `.shutdown()`.
pub struct NeovideEditorPane {
    widget: GLArea,
    shared: Rc<PaneShared>,
    hole_changed_callbacks: Rc<RefCell<Vec<Rc<dyn Fn()>>>>,
    presentation: Rc<Cell<Option<Presentation>>>,
    /// Kept alive here (rather than only living inside the closures set up in `new()`) so
    /// `grab_focus()` can call `im_context.focus_in()` after the host actually shows/focuses this
    /// pane's widget -- mirroring the reference probe's own `gl_area.grab_focus()` +
    /// `im_context.focus_in()` pairing, which only makes sense once the widget is realized, not
    /// at construction time.
    im_context: IMMulticontext,
    live_state: Rc<RefCell<LiveState>>,
    exited_callback: ExitedCallbackSlot,
    unreachable_callback: UnreachableCallbackSlot,
    /// Shared with the render callback. See [`NeovideEditorPane::set_clear_color`].
    clear_color: Rc<Cell<Color4f>>,
    /// The focus state the host last reported, shared with the render callback so a report that
    /// arrives before nvim has started is applied the moment the harness exists. See
    /// [`NeovideEditorPane::set_focused`].
    focused: Rc<Cell<Option<bool>>>,
    /// See [`NeovideEditorPane::on_fullscreen_setting`].
    fullscreen_callback: FullscreenCallbackSlot,
    /// A [`NeovideEditorPane::set_fullscreen_setting`] made before nvim existed, handed over the
    /// moment it does -- the same shape as `focused`. Only the last one matters.
    pending_fullscreen: Rc<Cell<Option<bool>>>,
    /// See [`NeovideEditorPane::on_scale_factor_setting`].
    scale_factor_callback: ScaleFactorCallbackSlot,
    /// See [`ScaleWatch`]'s own doc for why the per-tick change watch and the pre-`Ready` pending
    /// write live in one unit.
    scale_watch: Rc<ScaleWatch>,
    /// See [`NeovideEditorPane::connect_cell_size_changed`].
    cell_size_callback: CellSizeCallbackSlot,
    /// See [`NeovideEditorPane::grid_top_share`]. Written by the render callback.
    grid_top_share: Rc<Cell<Option<f64>>>,
    /// See [`NeovideEditorPane::on_start_failed`].
    start_failed_callback: StartFailedCallbackSlot,
    /// See [`NeovideEditorPane::on_session_ready`].
    session_ready_callback: SessionReadyCallbackSlot,
    /// See [`NeovideEditorPane::connect_key_activity`].
    key_activity: keyboard::KeyActivity,
    /// The tick callback's registration and the event-loop fd watch (`tick_driver`).
    tick_driver: Rc<TickDriver>,
    /// What the next nvim is started with, read by the render callback when it builds the harness:
    /// the construction's options, until [`NeovideEditorPane::restart`] replaces them.
    spawn_options: Rc<RefCell<NeovideEditorPaneOptions>>,
    /// How the last session ended ([`NeovideEditorPane::ended`]): written once per session, when the
    /// service reports its end or a shutdown or release sees it; cleared when a new session starts.
    ended: Rc<RefCell<Option<PaneEnded>>>,
    /// See [`NeovideEditorPane::on_followed`].
    followed_callback: FollowedCallbackSlot,
    /// See [`NeovideEditorPane::learned_nvim`].
    learned: LearnedNvim,
    /// A followed `:connect` made this pane an attached one: it stays so whatever its state, so it never
    /// starts an nvim of its own again.
    followed_attach: Rc<Cell<bool>>,
}

/// The oldest nvim the editor runs, as `(major, minor, patch)`: the window's own floor
/// (`eitri_core::nvim_bin::MINIMUM_VERSION`, held equal by a `shell` test). Neovide itself requires
/// 0.12; the pane hands this to `LiveHarnessOptions::minimum_nvim_version`, so an 0.11 nvim gets
/// everything Neovide does not need 0.12 for, and the 0.12-only parts (`:restart`, `:connect`, the
/// mouse grid detection and the newer IME calls) stay off by nvim's own version.
pub const NVIM_FLOOR: (u64, u64, u64) = (0, 11, 0);

/// Construction-time options for [`NeovideEditorPane::with_options`], and a restart's for
/// [`NeovideEditorPane::restart`]. Except for the pane's `starting_color`, each field is forwarded to
/// the `LiveHarnessOptions` field of the same name, so this crate stays a pass-through rather than
/// growing a `with_this_and_that` constructor per combination a host happens to need.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct NeovideEditorPaneOptions {
    /// The colour the pane paints before nvim's UI is ready; `None` keeps the crate's own colour.
    pub starting_color: Option<(u8, u8, u8)>,
    /// Launch nvim with `--clean` (no user plugins/config), for deterministic verification runs.
    pub clean: bool,
    /// Extra `(name, value)` environment variables set on the spawned `nvim --embed` child process
    /// **only** -- forwarded verbatim to `LiveHarnessOptions::child_env`, which reaches the child
    /// through `Command::env` at spawn time. This pane's own process environment is never touched,
    /// so nothing else the host spawns can observe the injection.
    ///
    /// This crate takes no view on *what* a host injects or why; it only owns the plumbing. The
    /// one real caller today is `shell`, which uses it to make the embedded nvim believe it is
    /// running inside tmux so `vim-tmux-navigator` forwards boundary-crossing `Ctrl-h`/`Ctrl-l`
    /// out to the host (see `shell/src/pane_switch.rs`). Values are applied on top of the
    /// inherited environment, so a host prepending to `PATH` must compose the whole value itself.
    pub child_env: Vec<(String, String)>,
    /// Working directory for the spawned `nvim --embed` child. `None` inherits this process's own
    /// cwd, which is exactly what `None` means to `LiveHarnessOptions::cwd`.
    ///
    /// A host that resolves a project root for its other panes must set this too: an editor left
    /// on the process cwd while the agent panel and terminal were pointed somewhere else is a
    /// window whose three panes disagree about which project is open, which is worse than any of
    /// them being "wrong" consistently.
    pub cwd: Option<std::path::PathBuf>,
    /// Extra arguments for the `nvim` binary itself, forwarded to
    /// `LiveHarnessOptions::extra_nvim_args` after `--clean` (when `clean` is set).
    ///
    /// Like `child_env`, this crate takes no view on what a host passes. `shell` uses it for one
    /// `--cmd` that loads its theme feed (see `shell/src/theme/feed.rs`); `--cmd` runs before the
    /// user's own config, so what it installs is autocommands, not settings.
    pub extra_nvim_args: Vec<String>,
    /// What nvim opens at start, as the arguments `nvim` takes after its options: an optional
    /// `+N`/`+`/`+call cursor(L,C)` and then absolute file paths. Empty (the default) opens nothing
    /// and leaves the argument list exactly as it was.
    ///
    /// They are real nvim arguments, so `argc()`, `:args`, `VimEnter` and the cursor command behave as
    /// for `nvim file`. This crate does not validate or quote them; the host builds them from paths
    /// that start with `/`, so none can read as an option or a command.
    pub open: Vec<String>,
    /// Join an nvim that is already running, as one more UI, instead of starting one
    /// (`LiveHarnessOptions::server`). `None` (the default) starts the pane's own `nvim --embed`.
    ///
    /// The nvim is not this pane's: no process is looked for at the launch and nvim is never asked
    /// for its pid, so nothing can signal it; [`end_nvim`](NeovideEditorPane::end_nvim),
    /// [`exec_lua_watched`](NeovideEditorPane::exec_lua_watched) (the quit's carrier) and
    /// [`restart`](NeovideEditorPane::restart) are refused; every way the pane lets go of it -- its
    /// shutdown, its drop, a [`HangUp`] handle -- closes this UI's channel and nvim goes on. The
    /// fork puts no clipboard provider into it, and `clean`, `child_env`, `cwd`,
    /// `extra_nvim_args` and `open` are not used. The launch still blocks this thread, for at most
    /// the fork's bound on an attach's setup (4 s).
    pub attach: Option<AttachSpec>,
    /// Decides, before [`exec_lua`](NeovideEditorPane::exec_lua) sends anything, whether a chunk
    /// may reach nvim and with which arguments. Set at construction (or a restart), so it holds from
    /// the first call. An attached pane with none answers every `exec_lua` with a refusal.
    pub chunk_check: Option<ChunkCheck>,
}

/// The nvim an attached pane joins ([`NeovideEditorPaneOptions::attach`]): its `--listen` address,
/// a unix socket that must be held by this process's own uid (the fork checks the peer before any
/// request).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachSpec {
    pub server: String,
}

/// A [`ChunkCheck`]'s refusal: `reason` is what the caller's reply is answered with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRefused {
    pub reason: String,
}

impl std::fmt::Display for ChunkRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for ChunkRefused {}

/// A check's signature: the chunk and the caller's arguments in; the arguments to send, or a
/// refusal, out.
type ChunkCheckFn = dyn Fn(&'static str, Vec<rmpv::Value>) -> Result<Vec<rmpv::Value>, ChunkRefused>;

/// The host's rule for what [`NeovideEditorPane::exec_lua`] may send
/// ([`NeovideEditorPaneOptions::chunk_check`]): called on this thread with the chunk and its
/// arguments, before anything is written to nvim. `Ok` carries the arguments to send (the caller's,
/// or the caller's with more appended); `Err` is answered to the caller at once and nothing is sent.
/// Shared rather than boxed, so the options that hold it can be cloned for a restart and compared
/// (two checks are equal when they are the same one).
#[derive(Clone)]
pub struct ChunkCheck(Rc<ChunkCheckFn>);

impl ChunkCheck {
    pub fn new(
        check: impl Fn(&'static str, Vec<rmpv::Value>) -> Result<Vec<rmpv::Value>, ChunkRefused> + 'static,
    ) -> Self {
        ChunkCheck(Rc::new(check))
    }

    fn run(&self, code: &'static str, args: Vec<rmpv::Value>) -> Result<Vec<rmpv::Value>, ChunkRefused> {
        (self.0)(code, args)
    }
}

impl std::fmt::Debug for ChunkCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChunkCheck(..)")
    }
}

impl PartialEq for ChunkCheck {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

/// How a pane's session ended ([`NeovideEditorPane::ended`]), read by the host in its exit callback
/// ([`NeovideEditorPane::on_exited_unrequested`]) or after its own shutdown. There is no exit status:
/// the fork does not keep one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneEnded {
    /// nvim ended the session. For an nvim the pane started, it exited. For an attached one, nvim
    /// either exited (`:qa`) or closed this UI's channel itself (the fork's own quit in attach mode)
    /// and goes on; the pane cannot tell which, and a host with a connection of its own to that nvim
    /// can.
    Exited,
    /// This pane let go of an attached nvim: its shutdown, its drop or a [`HangUp`] handle closed
    /// the channel. nvim goes on.
    Detached,
    /// `:connect` or `:restart` typed in an attached nvim moved it away from this UI; the fork does
    /// not follow ([`Left`]).
    SessionLeft(Left),
}

/// A `:restart` or `:connect` typed in the pane's own nvim, which the pane followed
/// ([`NeovideEditorPane::on_followed`]) instead of ending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneFollowed {
    /// The pane shows the server `:restart` started: still the pane's own nvim, a new session of it.
    Restart,
    /// The pane shows the server `:connect` moved it to, which it did not start: it is attached to it
    /// from now on ([`NeovideEditorPane::is_attached`]). `left` is the address of the pane's own nvim it
    /// left, which runs on with no UI unless a banged `:connect` stopped it.
    Connect { left: Option<String> },
}

/// The pid and start time (`/proc/<pid>/stat`'s 22nd field) of the server a followed `:restart`
/// started, once it answered for itself ([`NeovideEditorPane::learned_nvim`]); none before, and none
/// for any other session. A clone reads the same slot from any thread.
#[derive(Debug, Clone, Default)]
pub struct LearnedNvim(Arc<std::sync::Mutex<Option<(u32, u64)>>>);

impl LearnedNvim {
    pub fn get(&self) -> Option<(u32, u64)> {
        *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn set(&self, pid: u32) {
        let held = proc_start_time(pid).map(|start| (pid, start));
        *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = held;
    }

    fn clear(&self) {
        *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

/// `/proc/<pid>/stat`'s 22nd field, the start time that tells a process from a later one given its pid.
fn proc_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// What the pane runs, over the fork's own channel, to let go of a server a followed `:connect` reached:
/// the fork's cleanup for that channel, its id the argument, which puts back what the fork put in (no glue
/// of Eitri's is there to run it). Lua cannot name the channel a request came on (its own API calls are
/// internal ones), so the pane asks nvim for it first, over the same connection.
const FOLLOWED_CLEANUP_LUA: &str = "local record = rawget(_G, '__neovide_attach') \
    if type(record) ~= 'table' or type(record.cleanup) ~= 'function' then return false end \
    return record.cleanup(...) == true";

/// How long [`NeovideEditorPane::detach_followed`] waits for that cleanup before it hangs up anyway: a
/// server waiting for a key answers later, and its record stays.
const FOLLOWED_CLEANUP_WITHIN: Duration = Duration::from_millis(300);

/// Closes an attached pane's channel from any thread ([`NeovideEditorPane::hang_up_handle`]): nvim
/// drops the UI and goes on, and the pane reports the end on its next service run:
/// [`PaneEnded::Detached`], unless nvim had already left on its own. Idempotent, and harmless once the session has ended. The only thing it can do to
/// nvim is close this one connection.
#[derive(Clone)]
pub struct HangUp {
    connection: neovide::bridge::session::HangUp,
    detached: Arc<AtomicBool>,
}

impl HangUp {
    pub fn hang_up(&self) {
        self.detached.store(true, Ordering::SeqCst);
        self.connection.hang_up();
    }
}

impl std::fmt::Debug for HangUp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HangUp(..)")
    }
}

/// The `--cmd` that hides the cursor while the pane does not have keyboard focus.
///
/// `g:neovide_cursor_unfocused_outline_width` defaults to a hollow block of width 1/8 em.
/// Here, a width `<= 0` suppresses every unfocused cursor shape. Drawing a zero-width outline can
/// otherwise leave an anti-aliased sliver at fractional cell sizes: 33 pixels at scale 1.5.
///
/// `--cmd` runs before user configuration, so `init.lua` can restore the hollow block with
/// `vim.g.neovide_cursor_unfocused_outline_width = 0.125`. The initial value is read by
/// `settings.read_initial_values` before `ui_attach`. Configuration at `ui_attach` and later
/// runtime changes reach the renderer through the `WatchGlobal` watcher and `setting_changed`.
pub const HIDE_UNFOCUSED_CURSOR_CMD: &str = "let g:neovide_cursor_unfocused_outline_width = 0";

/// The whole argument list this pane hands to `LiveHarnessOptions::extra_nvim_args`: `--clean`
/// if asked for, then this pane's own default (`HIDE_UNFOCUSED_CURSOR_CMD`), then the host's
/// extras -- so a host's `--cmd` can override the default too. When there is something to open, the
/// list ends with `--embed` and then those arguments.
///
/// `--embed` is spelled here, only when `open` is not empty, because nvim loads a positional file
/// only after it has seen `--embed`: a file before it lands in the argument list and is never read.
/// The fork appends `--embed` to the end of whatever it is given, and only when it is absent, so
/// arguments placed after our own `--embed` stay after it. With nothing to open the fork's own
/// append is left to do its work and the list is what it always was.
fn nvim_args(clean: bool, extra: &[String], open: &[String]) -> Vec<String> {
    let mut args = Vec::with_capacity(extra.len() + open.len() + 4);
    if clean {
        args.push("--clean".to_string());
    }
    args.push("--cmd".to_string());
    args.push(HIDE_UNFOCUSED_CURSOR_CMD.to_string());
    args.extend(extra.iter().cloned());
    if !open.is_empty() {
        args.push("--embed".to_string());
        args.extend(open.iter().cloned());
    }
    args
}

/// How long after this pane's `GLArea` is mapped it asks for one more frame. 50ms is the measured
/// fix: 50 re-shows of 50 right. A render asked for in the same main-loop
/// turn as the show coalesces into the first frame and fixed only 6 of 8; a frame-clock "next tick"
/// is plausible and **unmeasured**, and needs the same 50-cycle check before it replaces this.
const REMAP_RENDER_DELAY: Duration = Duration::from_millis(50);

/// One delayed render per map. A map that arrives while an earlier map's render is still pending
/// replaces it, so a burst of hide/show cycles asks for one render, not one per cycle.
///
/// Why a map needs a render at all: a host that hides this pane with `set_child_visible(false)`
/// (the module grid) and shows it again over the same rectangle gives the `GLArea` no `resize`,
/// so nothing sets `wants_frame`, and the single frame drawn on map is the only frame. If nvim
/// redrew while the pane was hidden -- the fd watch keeps servicing (draining) nvim --
/// that frame shows the cursor and nothing else, until nvim's next redraw (measured: 39 re-shows of 50,
/// and 0 of 8 when only the cursor had moved). `GtkPaned` never showed it: its `set_visible(true)`
/// forces a resize, and ~14 renders follow. Which half draws the window-less frame, Neovide's
/// renderer or GTK's texture handling after a re-map, was not isolated.
struct RemapKick {
    pending: Rc<RefCell<Option<glib::SourceId>>>,
}

impl RemapKick {
    fn new() -> Self {
        RemapKick {
            pending: Rc::new(RefCell::new(None)),
        }
    }

    fn schedule(&self, delay: Duration, render: impl FnOnce() + 'static) {
        if let Some(earlier) = self.pending.borrow_mut().take() {
            earlier.remove();
        }
        let pending = self.pending.clone();
        let id = glib::timeout_add_local_once(delay, move || {
            // This source is finished once it fires; taking its id here is what stops a later map
            // from `remove`-ing a source glib no longer has.
            pending.borrow_mut().take();
            render();
        });
        *self.pending.borrow_mut() = Some(id);
    }
}

/// Whether the tick callback's `Ready` arm should queue another render this frame, given each
/// independent reason the render loop already tracks. Split out of the tick closure -- which
/// borrows `GLArea`/`LiveSession` types no unit test can construct -- for the same reason the fork
/// splits `focus_changed`/`scale_factor_changed` out of `LiveHarness`: it is the one decision here
/// worth testing on its own.
///
/// `scale_factor_changed_now` exists because a scale-factor write produces none of the other three
/// signals: it does not touch `redraw_batches_seen` (nvim itself sent nothing new -- the value is
/// applied inside `LiveHarness::render_frame`, not delivered as a redraw batch), it is not a resize
/// (the window doesn't move, so `connect_resize` never fires), and no keypress or resize handler
/// sets `wants_frame` for it either. Dropping this term is exactly the failure the zoom-together
/// design measured: a `:let g:neovide_scale_factor` (or `set_scale_factor_setting`) with nothing
/// else in flight would apply inside the harness and then sit on screen unpainted until some
/// unrelated activity eventually asked for a frame.
fn tick_should_render(
    last_animating: bool,
    new_content: bool,
    wants_frame: bool,
    scale_factor_changed_now: bool,
) -> bool {
    last_animating || new_content || wants_frame || scale_factor_changed_now
}

/// Whether `connect_cell_size_changed` should fire this frame: `None` (nvim has never
/// reported a cell yet) always counts as a change, same as `ScaleWatch`'s own first-observe rule;
/// otherwise a plain inequality. Split out of the render callback -- which borrows `GLArea`/
/// `LiveSession` types no unit test can construct -- for the same reason `tick_should_render` is.
fn cell_size_changed(last: Option<(f64, f64)>, now: (f64, f64)) -> bool {
    last != Some(now)
}

/// [`NVIM_FLOOR`] as the pane's messages spell it -- restated because the fork's own too-old error
/// names it as bare numbers with no context of what *this project* needs, and the pane's own message
/// should say so plainly.
const NVIM_VERSION_FLOOR: &str = "0.11";

/// Where to get a newer nvim -- the same URL the fork's own too-old error already links
/// (`bridge/mod.rs:197`), restated here so the missing-nvim case (whose error text never names a
/// URL at all) gets one too.
const NVIM_INSTALL_URL: &str = "https://github.com/neovim/neovim/wiki/Installing-Neovim";

/// The message [`NeovideEditorPane::on_start_failed`] hands the host and [`LiveState::Failed`]
/// stores, built from the raw error `LiveHarness::with_options` returned (`format!("{err:#}")`, the
/// fork's own `anyhow` chain -- still logged to stdout verbatim by the caller, unchanged). Two shapes
/// the pinned fork's `bridge::create_neovim_session` produces:
///
/// - **too old**: `"Neovide requires nvim version {major}.{minor}.{patch} or higher, but {found} was
///   detected. Download the latest version here <url>"` -- [`detected_nvim_version`] pulls `{found}`
///   back out, since the raw text names the *fork's* floor as bare numbers with no mention of what
///   Eitri itself needs.
/// - **missing, or any other launch failure** (`nvim` absent from `PATH`, a spawn error, a socket
///   that never opens): `.context("Could not locate or start neovim process")`'s own chain, which
///   never names a version or a place to get one at all.
///
/// Split out for the same reason `tick_should_render` is: no unit test here can construct a real
/// `LiveHarness::with_options` error, so the raw text is the fixture instead.
fn build_start_failure_message(raw_error: &str) -> String {
    match detected_nvim_version(raw_error) {
        Some(found) => format!(
            "nvim {found} is older than Eitri's floor ({NVIM_VERSION_FLOOR} or newer). Get a newer \
             nvim: {NVIM_INSTALL_URL}"
        ),
        None => format!(
            "nvim could not be started (Eitri needs {NVIM_VERSION_FLOOR} or newer): {raw_error}. \
             Install it: {NVIM_INSTALL_URL}"
        ),
    }
}

/// A construction panic can come from the window system before nvim is started, so it carries
/// the panic's detail without suggesting an nvim version problem.
fn build_start_panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    let detail = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic");
    format!("the editor could not start: {detail}")
}

/// What the window and the environment say about the display, read where the editor failed to start.
#[derive(Debug, Default, Clone)]
struct DisplayFacts {
    /// Whether GTK put the window on Wayland (else X11 or anything else).
    window_on_wayland: bool,
    wayland_display: Option<String>,
    x_display: Option<String>,
    gdk_backend: Option<String>,
}

impl DisplayFacts {
    fn read(window_display_type: &str) -> Self {
        let var = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
        DisplayFacts {
            window_on_wayland: window_display_type == "GdkWaylandDisplay",
            wayland_display: var("WAYLAND_DISPLAY"),
            x_display: var("DISPLAY"),
            gdk_backend: var("GDK_BACKEND"),
        }
    }
}

/// Plain words for a start failure that is the window system's: the editor's event loop could not
/// connect to the display it picked. The window may already be open on another display server, so the
/// text says which one each side is on and what to do. `None` for every other failure. The raw error
/// stays below it in the page.
fn display_failure_hint(detail: &str, facts: &DisplayFacts) -> Option<String> {
    let lower = detail.to_ascii_lowercase();
    if !(lower.contains("winit event loop")
        || lower.contains("nocompositor")
        || lower.contains("neither wayland_display"))
    {
        return None;
    }
    let window = if facts.window_on_wayland { "Wayland" } else { "X11" };
    let mut text = String::from("The editor could not connect to a display server. ");
    if lower.contains("neither wayland_display") {
        text.push_str(
            "Neither WAYLAND_DISPLAY nor DISPLAY is set here. Start Eitri from a shell inside your desktop session, \
             or set WAYLAND_DISPLAY to your compositor's socket (see `ls $XDG_RUNTIME_DIR/wayland-*`).",
        );
    } else if lower.contains("wayland") {
        let named = facts.wayland_display.as_deref().unwrap_or("no name");
        text.push_str(&format!(
            "It tried the Wayland display \"{named}\" and nothing answered there, while the window is on {window}. \
             A closed desktop session leaves such a socket behind, and a tmux server that is older than your \
             session keeps the old name. Run `ls $XDG_RUNTIME_DIR/wayland-*` to find the live socket, then restart \
             Eitri with WAYLAND_DISPLAY set to it (inside tmux: `tmux setenv -g WAYLAND_DISPLAY <name>`, or restart \
             the tmux server)."
        ));
        if let (false, Some(backend)) = (facts.window_on_wayland, facts.gdk_backend.as_deref()) {
            text.push_str(&format!(
                " GDK_BACKEND={backend} is set, which puts the window on {window} while the editor follows \
                 WAYLAND_DISPLAY; unset one of them."
            ));
        }
    } else {
        let named = facts.x_display.as_deref().unwrap_or("no name");
        text.push_str(&format!(
            "It tried the X11 display \"{named}\" and could not open it, while the window is on {window}. \
             Check that DISPLAY names a running X server (in tmux, a server older than your session keeps the \
             old value), then restart Eitri."
        ));
    }
    Some(text)
}

/// The failure message with [`display_failure_hint`] in front of it when the display is the cause.
fn with_display_hint(message: String, facts: &DisplayFacts) -> String {
    match display_failure_hint(&message, facts) {
        Some(hint) => format!("{hint}\n\n{message}"),
        None => message,
    }
}

/// Pulls the detected version out of the fork's own too-old sentence ("... but {found} was detected.
/// ..."), or `None` for any other shape -- a missing/unspawnable nvim never mentions a detected
/// version at all.
fn detected_nvim_version(raw_error: &str) -> Option<&str> {
    let after_but = raw_error.split_once("but ")?.1;
    let (version, _) = after_but.split_once(" was detected")?;
    // The fork's {found} is `:version`'s first line, "NVIM v0.9.5" (the GUI pass, F1): the number.
    let version = version.strip_prefix("NVIM ").unwrap_or(version);
    Some(version.strip_prefix('v').unwrap_or(version))
}

/// What the tick reports about nvim's end: the fork's own report is `fork_exited`, and
/// `nvim_alive` is what the pidfd on nvim's process says (`None`: no process is known to be nvim).
/// An exit is reported only once nvim's process is gone -- or, with none known, on the fork's word
/// -- so what a host does on it (close the window, retire the editor) never runs while an nvim we
/// started may still be writing, and it is reported once however often the tick asks (`said`, the
/// pane's latches). While the fork says exited and the process lives, the host is told once that
/// nvim is out of reach, and only once `grace_over` ([`UNREACHABLE_GRACE`]): a quit's process goes a moment after its pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ExitEvents {
    unreachable: bool,
    exited: bool,
}

fn exit_events(
    fork_exited: bool,
    nvim_alive: Option<bool>,
    grace_over: bool,
    unreachable_said: bool,
    exit_said: bool,
) -> ExitEvents {
    if !fork_exited {
        return ExitEvents::default();
    }
    match nvim_alive {
        Some(true) => ExitEvents {
            unreachable: grace_over && !unreachable_said,
            exited: false,
        },
        _ => ExitEvents {
            unreachable: false,
            exited: !exit_said,
        },
    }
}

/// What a session's end means ([`PaneEnded`]), from whether it was attached, whether this pane hung up on it, and
/// the `:connect` or `:restart` the fork reported for it.
fn pane_ended(attached: bool, detached: bool, left: Option<Left>) -> PaneEnded {
    if !attached {
        PaneEnded::Exited
    } else if let Some(why) = left {
        // The fork reports a `:connect` or `:restart` only when nvim ended the session before this
        // side hung up, so a hang-up made after it (a shutdown or a handle, before the pane's next
        // service run read the end) does not hide where nvim went.
        PaneEnded::SessionLeft(why)
    } else if detached {
        PaneEnded::Detached
    } else {
        PaneEnded::Exited
    }
}

/// Keeps how `session` ended in `slot`, once: the first answer stands for the session (a later
/// read of the fork's events would find them taken). `None` while it has not ended.
fn record_end(session: &LiveSession, slot: &RefCell<Option<PaneEnded>>) -> Option<PaneEnded> {
    let known = slot.borrow().clone();
    if known.is_some() {
        return known;
    }
    let how = session.how_it_ended()?;
    *slot.borrow_mut() = Some(how.clone());
    Some(how)
}

/// The line the service logs when it tells the host nvim is out of reach: its process outlived the fork's report of
/// its exit by [`UNREACHABLE_GRACE`]. Only nvim run behind a launcher is said to have been left by one.
/// How the nvim a session holds came to run, for the line that says it outlived the fork's report of its exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NvimStart {
    /// Started by the fork, as the pane's own child.
    Direct,
    /// Behind a launcher that exited and left it running.
    Launcher,
    /// The server a followed `:restart` started: its parent was the nvim before it.
    Restart,
}

fn unreachable_log_line(start: NvimStart) -> String {
    let why = match start {
        NvimStart::Direct => "",
        NvimStart::Launcher => " (its launcher exited without it)",
        NvimStart::Restart => " (the server nvim's :restart started)",
    };
    format!(
        "[live] the fork reports nvim exited, and nvim's own process is still alive {} ms later{why}; its exit is \
         reported once it is gone",
        UNREACHABLE_GRACE.as_millis()
    )
}

/// Whether nvim's exit follows a quit this pane asked of it: the last watched request
/// ([`NeovideEditorPane::exec_lua_watched`], used for the editor's `:confirm qall`) was never answered, so
/// either it is still out or it ended because nvim went away. A request nvim did answer was a quit it
/// declined, and an exit after that comes from nothing this pane asked.
fn exit_follows_quit(watch: Option<&CallWatch>) -> bool {
    watch.is_some_and(|watch| !watch.returned)
}

/// The line the tick logs when it passes an exit to the host: a quiet one for the exit a quit asked for, and
/// the one that says nvim left by itself for any other.
fn exit_log_line(after_quit: bool) -> &'static str {
    if after_quit {
        "[live] nvim has exited after the quit it was asked for -- invoking the registered \
         on_exited_unrequested callback (if any)"
    } else {
        "[live] nvim exited on its own (not via a caller-initiated shutdown) -- invoking the registered \
         on_exited_unrequested callback (if any) instead of closing a window this pane doesn't own"
    }
}

/// What a watched request's failure is worth saying when it happens. A connection that ended under the request
/// (nvim went away: the reader or writer of the pipe failed, or the request's channel closed) is the exit the
/// request asked for or an exit the tick reports in its own line, and says nothing. Every other failure -- nvim
/// answering with an error or a value of the wrong type, a message that did not decode, a buffer that would not
/// encode -- is something only this line says.
fn watched_failure_note(err: &nvim_rs::error::CallError) -> Option<String> {
    use nvim_rs::error::{CallError, DecodeError, EncodeError};
    let connection_ended = match err {
        CallError::InternalReceiveError(..) => true,
        CallError::DecodeError(decode, _) => matches!(decode.as_ref(), DecodeError::ReaderError(_)),
        CallError::SendError(encode, _) => matches!(encode, EncodeError::WriterError(_)),
        CallError::NeovimError(..) | CallError::WrongValueType(..) => false,
    };
    (!connection_ended).then(|| err.to_string())
}

/// How long [`NeovideEditorPane::settled_watched_call`] waits for a request's worker to publish
/// how it ended. At nvim's exit nvim-rs's IO loop has already resolved the request, so this is only
/// ever the worker thread's own scheduling; the bound is a backstop.
const SETTLE_WAIT: Duration = Duration::from_millis(500);

/// A job for a thread of its own.
type Job = Box<dyn FnOnce() + Send>;

/// Starts `job` on a named thread (`std::thread::Builder`).
fn spawn_named(name: &'static str) -> impl FnOnce(Job) -> std::io::Result<()> {
    move |job| std::thread::Builder::new().name(name.into()).spawn(job).map(|_| ())
}

/// Runs `job` on a thread `spawn` starts, or -- when none can be started (`EAGAIN` under a process
/// limit) -- here, inline, so what it does is never simply dropped (a dropped ending of nvim left
/// every later close waiting for it forever). `true` when it went to a
/// thread. A failed `Builder::spawn` consumes its closure, so the job travels in a slot both sides
/// can take from.
fn run_detached(job: Job, spawn: impl FnOnce(Job) -> std::io::Result<()>) -> bool {
    let slot = Arc::new(std::sync::Mutex::new(Some(job)));
    let theirs = slot.clone();
    let spawned = spawn(Box::new(move || {
        let job = theirs.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some(job) = job {
            job();
        }
    }));
    match spawned {
        Ok(()) => true,
        Err(err) => {
            eprintln!("[live] could not start a thread ({err}); running its job here instead");
            let job = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
            if let Some(job) = job {
                job();
            }
            false
        }
    }
}

/// Asks nvim, over the fork's own connection and on a thread of its own, for its pid (`getpid()`),
/// and holds the answer in `process` -- the process that answers on nvim's pipe is nvim, whatever
/// launched it (a `nvim` wrapper that does not `exec`
/// is the fork's direct child, and a kill of that left the hung nvim running). A thread, not the
/// GTK thread: nvim answers requests only once its loop runs, which is after the user's whole
/// config has been sourced -- and never, for an nvim that hung in it (then `process` keeps only
/// what the fork started, `nvim_child`'s source 2). The request is never abandoned: nvim-rs ends
/// the whole connection when a response arrives for a dropped request (`nvim_rpc`'s module doc),
/// and nvim exiting resolves it with an error.
fn ask_nvim_for_its_pid(harness: &LiveHarness, process: &Arc<nvim_child::NvimProcess>, learned: Option<LearnedNvim>) {
    let Some(nvim) = harness.neovim_handler().clone_current_neovim() else {
        println!("[live] no nvim connection to ask for its pid");
        return;
    };
    let process = process.clone();
    let spawned = std::thread::Builder::new()
        .name("nvim-rpc getpid".into())
        .spawn(move || {
            let answer = nvim_rpc::block_on(nvim.call_function("getpid", vec![]));
            match answer
                .ok()
                .and_then(|pid| pid.as_u64())
                .and_then(|pid| u32::try_from(pid).ok())
            {
                Some(pid) if process.learn(pid) => {
                    if let Some(learned) = &learned {
                        learned.set(pid);
                    }
                    println!("[live] nvim is pid {pid} (its own answer)")
                }
                Some(pid) => println!("[live] nvim answered pid {pid}, which is not a live nvim --embed; not held"),
                None => {
                    println!("[live] nvim did not answer getpid(); a forced kill falls back to what the fork started");
                }
            }
        });
    if let Err(err) = spawned {
        eprintln!("[live] could not start the thread that asks nvim for its pid ({err})");
    }
}

/// Whether the tick callback should ask for one more frame while [`LiveState::Failed`]: exactly
/// once -- the frame that actually paints [`FAILED_COLOR`] and the message. A Failed pane renders once
/// and stops, replacing the old unconditional `true` (`queue_render()` at display
/// refresh rate for the rest of the window's life, since `Failed` never leaves itself). Split out for
/// the same reason `tick_should_render` is.
fn failed_tick_should_render(already_painted: bool) -> bool {
    !already_painted
}

/// Whether the tick callback asks for a frame for a pane with no live session -- every state but
/// [`LiveState::Ready`], which the tick decides itself ([`tick_should_render`]). Split out so the
/// `Failed` arm is a test's (`Failed(_) => true` in the tick used to leave the whole suite green, since
/// only [`failed_tick_should_render`] was tested).
fn sessionless_tick_should_render(state: &LiveState, failed_painted: bool) -> bool {
    match state {
        // The placeholder-frame dance and the one blocking `LiveHarness::with_options` call both
        // happen *inside* the render callback and only run when a render is actually requested --
        // keep rendering here so that state machine can advance.
        LiveState::NotStarted | LiveState::Starting => true,
        // Failed is a terminal state that paints the same fill and message every time, so
        // unlike NotStarted/Starting it does NOT keep asking for frames forever -- only until the
        // render callback has actually painted it once (the `Failed` arm there sets
        // `failed_painted`). Asking every tick would render at display refresh rate for the rest of
        // the window's life.
        LiveState::Failed(_) => failed_tick_should_render(failed_painted),
        // Nothing will ever change: no frame is needed.
        LiveState::Exited => false,
        // Not sessionless: never asked here (the tick's own `Ready` arm decides).
        LiveState::Ready(_) => true,
    }
}

/// The device scale to apply to the harness, or `None` when it already agrees by bits.
/// The framebuffer's producer supplies this scale; values below one count as one.
fn os_scale_to_apply(current: f64, scale: f64) -> Option<f64> {
    let target = scale.max(1.0);
    if target.to_bits() == current.to_bits() {
        None
    } else {
        Some(target)
    }
}

/// Applies the framebuffer's device scale before any grid is computed from the harness's cells.
/// The resize and render paths call this with the scale of the framebuffer they received;
/// scale notification only asks for a frame, because it does not yet have that framebuffer.
/// Returns whether the harness applied a change and logs only that change.
fn sync_os_scale(harness: &mut LiveHarness, scale: f64) -> bool {
    let Some(new_scale) = os_scale_to_apply(harness.os_scale_factor(), scale) else {
        return false;
    };
    let old_scale = harness.os_scale_factor();
    if !harness.set_os_scale_factor(new_scale) {
        return false;
    }
    let cell = harness.grid_scale();
    println!(
        "[hidpi] os scale {old_scale} -> {new_scale}; cell now {:.2}×{:.2}px",
        cell.width(),
        cell.height()
    );
    true
}

/// One tick's [`ScaleWatch::observe`] result: whether `g:neovide_scale_factor` moved since the
/// last tick, and its current value either way.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ScaleObservation {
    pub(crate) changed: bool,
    pub(crate) value: f32,
}

/// The per-tick "did `g:neovide_scale_factor` change" watch, plus a pre-`Ready`
/// [`NeovideEditorPane::set_scale_factor_setting`] write waiting for a harness to exist. Bundled
/// into one unit, rather than the two independent `Cell`s (`last_scale_factor_setting` on
/// `LiveSession`, `pending_scale_factor` on the pane) an earlier revision of this file kept, so the
/// tick closure and the `Ready`-construction arm each call exactly one method and cannot
/// independently drop a piece of what the zoom-together design's L2 layer needs: the callback
/// firing, [`tick_should_render`] seeing the change, and a pre-`Ready` write reaching nvim once it
/// exists. A GTK call site that inlined this logic itself had all three go missing at once under a
/// mutation testing -- a literal `false` in place of the real signal, the callback
/// invocation dropped, the pending flush at `Ready` dropped -- each of which stayed green because
/// nothing in this crate called `observe`/`take_pending_for_ready` by name for a test to hold onto.
#[derive(Debug)]
pub(crate) struct ScaleWatch {
    last: Cell<f32>,
    pending: Cell<Option<f32>>,
}

impl ScaleWatch {
    /// `initial` is what a tick compares the FIRST real report against -- Neovide's own default
    /// (`1.0`), so a value `init.lua` set is reported as a change on the first tick, matching
    /// `last_fullscreen_setting`'s own starting value.
    pub(crate) fn new(initial: f32) -> Self {
        ScaleWatch {
            last: Cell::new(initial),
            pending: Cell::new(None),
        }
    }

    /// Called once per tick with nvim's current report. Bit equality is deliberate: the value comes
    /// from nvim verbatim, so an unchanged variable compares equal bit for bit -- including a NaN,
    /// which `!=` would report as a change on every tick (a forced render and a panel theme re-send
    /// 60 times a second after `:let g:neovide_scale_factor = 0/0`).
    pub(crate) fn observe(&self, current: f32) -> ScaleObservation {
        let changed = self.last.replace(current).to_bits() != current.to_bits();
        ScaleObservation {
            changed,
            value: current,
        }
    }

    /// Whether [`observe`](Self::observe) would report `current` as a change, without recording it:
    /// the render callback asks this after its own pump, to see a change only the tick reports.
    pub(crate) fn would_change(&self, current: f32) -> bool {
        self.last.get().to_bits() != current.to_bits()
    }

    /// A host write made before nvim existed (buffered because there was no harness yet). Only the
    /// last one matters, mirroring `pending_fullscreen`.
    pub(crate) fn buffer_pending(&self, value: f32) {
        self.pending.set(Some(value));
    }

    /// Takes the buffered pre-`Ready` write, if any -- call once, right after the harness is
    /// constructed and before its first tick, and hand the result straight to
    /// `LiveHarness::set_scale_factor_setting`. `None` every other time.
    pub(crate) fn take_pending_for_ready(&self) -> Option<f32> {
        self.pending.take()
    }
}

impl NeovideEditorPane {
    /// The colour painted where the editor's own grid does not reach -- today exactly one place:
    /// the band at the TOP that `gl_interop::snap_region_to_grid` creates by moving the sub-cell
    /// remainder off the bottom edge.
    ///
    /// **A host that does not call this gets `OUTSIDE_COLOR`, and on a light colorscheme that is
    /// visibly wrong** -- a near-black line under the top bar. This is not an optional refinement;
    /// it is the other half of the snap. `shell` calls it from the same theme listener that already
    /// drives its GTK CSS and the agent panel, so the band is nvim's own `Normal` background and
    /// therefore indistinguishable from the first text row below it.
    ///
    /// Takes straight RGB bytes rather than a `Color4f` so a host does not need `skia_safe` in its
    /// own dependency graph to call it; alpha is always opaque, since a translucent band would let
    /// through whatever GTK last painted there.
    pub fn set_clear_color(&self, color: (u8, u8, u8)) {
        set_clear_color_and_request(&self.clear_color, color, |reason| self.request_frame(reason));
    }

    /// Builds the `GtkGLArea` and wires every resize/IME/keyboard/mouse/tick/render callback onto
    /// it and the shared `Rc<RefCell<LiveState>>` -- everything `poc/neovide_embed_live::build_ui`
    /// did except constructing an `Application`/`ApplicationWindow` and calling
    /// `.present()`/`.grab_focus()` (the host does those, via `.widget()`/`.grab_focus()` below).
    /// `clean` is forwarded to `LiveHarnessOptions::extra_nvim_args` as `--clean`, for
    /// deterministic manual-verification runs; everything else takes its default. Use
    /// [`NeovideEditorPane::with_options`] to set anything more.
    pub fn new(clean: bool) -> Self {
        Self::with_options(NeovideEditorPaneOptions {
            clean,
            ..Default::default()
        })
    }

    /// Same as [`NeovideEditorPane::new`], with every construction-time knob this pane forwards to
    /// `LiveHarnessOptions` spelled out. See [`NeovideEditorPaneOptions`] for what each one means.
    pub fn with_options(options: NeovideEditorPaneOptions) -> Self {
        let spawn_options = Rc::new(RefCell::new(options));
        // A `GtkGLArea` whose snapshot presents through the pane's own tiled buffers
        // (`editor_area`); every handler below is connected to it as a plain `GLArea`.
        let gl_area: GLArea = glib::Object::builder::<editor_area::EditorGlArea>()
            .property("hexpand", true)
            .property("vexpand", true)
            .property("has-stencil-buffer", true)
            .property("auto-render", true)
            .property("focusable", true)
            .property("can-focus", true)
            .build()
            .upcast();

        let skia_state: Rc<RefCell<Option<SkiaState>>> = Rc::new(RefCell::new(None));
        let live_state: Rc<RefCell<LiveState>> = Rc::new(RefCell::new(LiveState::NotStarted));
        if let Some(log) = gl_area
            .downcast_ref::<editor_area::EditorGlArea>()
            .and_then(|area| area.present_log())
        {
            let keys = gtk4::EventControllerKey::new();
            keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
            let live = Rc::downgrade(&live_state);
            keys.connect_key_pressed(move |_, keyval, keycode, _| {
                use glib::translate::IntoGlib;
                let epoch = live.upgrade().and_then(|live| {
                    let live = live.try_borrow().ok()?;
                    match &*live {
                        LiveState::Ready(session) => Some(session.harness.redraw_batches_seen()),
                        _ => None,
                    }
                });
                log.key(epoch, keyval.into_glib(), keycode);
                glib::Propagation::Proceed
            });
            gl_area.add_controller(keys);
        }
        let tick_driver: Rc<TickDriver> = Rc::new(TickDriver::default());
        let clear_color = Rc::new(Cell::new(OUTSIDE_COLOR));
        let presentation = Rc::new(Cell::new(None));
        let hole_changed_callbacks: Rc<RefCell<Vec<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(Vec::new()));
        if let Some(area) = gl_area.downcast_ref::<editor_area::EditorGlArea>() {
            let callbacks = hole_changed_callbacks.clone();
            area.set_hole_changed(Rc::new(move || {
                let callbacks = callbacks.borrow().clone();
                for callback in callbacks {
                    callback();
                }
            }));
            let clear = clear_color.clone();
            let options = spawn_options.clone();
            let live = live_state.clone();
            area.set_cover(Rc::new(move || {
                let ready = live
                    .try_borrow()
                    .is_ok_and(|live| matches!(&*live, LiveState::Ready(session) if session.harness.is_ready()));
                hole_cover_color(ready, clear.get(), options.borrow().starting_color)
            }));
        }
        let focused: Rc<Cell<Option<bool>>> = Rc::new(Cell::new(None));
        let exited_callback: ExitedCallbackSlot = Rc::new(RefCell::new(None));
        let ended: Rc<RefCell<Option<PaneEnded>>> = Rc::new(RefCell::new(None));
        let unreachable_callback: UnreachableCallbackSlot = Rc::new(RefCell::new(None));
        let fullscreen_callback: FullscreenCallbackSlot = Rc::new(RefCell::new(None));
        let pending_fullscreen: Rc<Cell<Option<bool>>> = Rc::new(Cell::new(None));
        let scale_factor_callback: ScaleFactorCallbackSlot = Rc::new(RefCell::new(None));
        let scale_watch: Rc<ScaleWatch> = Rc::new(ScaleWatch::new(1.0));
        // `LiveHarness::fullscreen_setting()` as the service last read it, so the host's
        // `on_fullscreen_setting` callback fires on a change and not on every run. Starts at
        // Neovide's own default (`false`), so a value `init.lua` set is reported on the first run.
        // The pane's, not a session's, as `scale_watch` is: a restarted nvim's value is compared
        // with the one the host was last told, so a host that followed the ended nvim into
        // fullscreen hears the new nvim's `false`.
        let last_fullscreen_setting: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let cell_size_callback: CellSizeCallbackSlot = Rc::new(RefCell::new(None));
        let grid_top_share: Rc<Cell<Option<f64>>> = Rc::new(Cell::new(None));
        let start_failed_callback: StartFailedCallbackSlot = Rc::new(RefCell::new(None));
        let session_ready_callback: SessionReadyCallbackSlot = Rc::new(RefCell::new(None));
        let followed_callback: FollowedCallbackSlot = Rc::new(RefCell::new(None));
        let learned = LearnedNvim::default();
        let followed_attach: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        // Set the first time the `Failed` arm below actually paints `FAILED_COLOR` and the message
        // -- read by the tick callback ([`failed_tick_should_render`]) to stop asking for more
        // frames once that one paint has happened. `Failed` leaves itself only through a restart,
        // and the `NotStarted` arm clears this then, so a launch that fails again paints again.
        let failed_painted: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        // Only the render closure below ever reads this -- not stored on `Self`, unlike
        // `cell_size_callback`, which a host registers *after* construction.
        let last_cell_size: Rc<Cell<Option<(f64, f64)>>> = Rc::new(Cell::new(None));
        // The one framebuffer size (device pixels) the grid, the snap, the tick's resync and hit
        // testing all derive from -- GTK's own, as `GLArea::resize` reports it, so none of them can
        // drift from the others; see `gl_interop::GridLayout`.
        let fb_size: Rc<Cell<(i32, i32)>> = Rc::new(Cell::new((0, 0)));
        let shared = Rc::new(PaneShared {
            skia_state: skia_state.clone(),
            live_state: live_state.clone(),
            fb_size: fb_size.clone(),
            device_scale: Rc::new(Cell::new(f64::from(gl_area.scale_factor().max(1)))),
        });

        // GtkWidget::unrealize is RUN_LAST: this normal handler runs before GtkGLArea deletes
        // its framebuffer and clears its GL context. Keep the nvim session across re-realization,
        // but let the next render create fresh Skia state for the new context.
        {
            let skia_state = skia_state.clone();
            gl_area.connect_unrealize(move |widget| {
                if let Some(area) = widget.downcast_ref::<editor_area::EditorGlArea>() {
                    let pane = area.subsurface();
                    area.set_subsurface(None);
                    if let Some(pane) = pane {
                        pane.stop();
                    }
                }
                let Some(state) = skia_state.borrow_mut().take() else {
                    return;
                };
                widget.make_current();
                let context_is_current = widget.error().is_none()
                    && widget
                        .context()
                        .is_some_and(|context| gtk4::gdk::GLContext::current().as_ref() == Some(&context));
                state.release(context_is_current);
            });
        }

        // --- resize: GtkGLArea's FBO can be resized/recreated under us, so drop the cached
        // Surface and let the next render() rebuild it against the new framebuffer dimensions
        // (device pixels, not logical widget units). Also the frozen-scroll fix:
        // recompute the grid size that actually fits the new content_region and, if it differs
        // (in grid *cells*, not raw pixels) from what nvim was last told, call
        // `LiveHarness::resize_grid` so nvim's own viewport tracks the real host size.
        {
            let shared = shared.clone();
            gl_area.connect_resize(move |widget, width, height| {
                if subsurface_on(widget).is_some_and(|pane| pane.owns_snapshot()) {
                    return;
                }
                framebuffer_resized(&shared, width, height, f64::from(widget.scale_factor().max(1)));
            });
        }

        // --- map: one more frame ~50ms after this pane is back on screen. See `RemapKick` for why
        // a host that hides panes by `set_child_visible` needs it, and for what was measured.
        {
            let live_state = live_state.clone();
            let kick = RemapKick::new();
            gl_area.connect_map(move |widget| {
                // A pane hidden mid-animation must not simulate the entire hidden interval on
                // its first visible frame. A new monitor may also supply a different clock.
                if let LiveState::Ready(session) = &mut *live_state.borrow_mut() {
                    session.animation_clock.reset();
                }
                if let Some(pane) = subsurface_on(widget).filter(|pane| pane.is_active()) {
                    pane.map();
                    return;
                }
                let widget = widget.downgrade();
                let live_state = live_state.clone();
                kick.schedule(REMAP_RENDER_DELAY, move || {
                    let Some(widget) = widget.upgrade() else {
                        return;
                    };
                    if let Ok(live) = live_state.try_borrow() {
                        if let LiveState::Ready(session) = &*live {
                            session.wants_frame.set(true);
                        }
                    }
                    request_frame_on(&widget, FrameReason::Map);
                });
            });
        }

        gl_area.connect_unmap(|widget| {
            if let Some(pane) = subsurface_on(widget) {
                pane.unmap();
            }
        });

        // --- scale-factor notify: `GtkGLArea` reallocates its own buffers and fires
        // `resize` (with the new framebuffer) only on its NEXT snapshot after this notify fires --
        // and this pane only ever renders on change -- so without asking for a frame here the
        // old frame stays upscaled indefinitely (measured: `s1-after-scale2.png`, `s1.log` has the
        // notify line and no `[resize]` for 4s). This handler does exactly that and nothing more: it
        // never touches the harness or `fb_size` itself. The actual scale sync (`sync_os_scale`) runs
        // in `connect_resize` and the render callback below, where GTK has handed over the matching
        // new framebuffer in the very same snapshot -- never here, at notify time, which is why the
        // notify handler stays this small.
        {
            let live_state = live_state.clone();
            gl_area.connect_scale_factor_notify(move |widget| {
                // `try_borrow`, not `borrow`: a GTK signal can fire while a render or tick closure
                // already holds `live_state` borrowed (a `run_tab_verb` panic in the panel once came
                // from exactly that).
                if let Ok(live) = live_state.try_borrow() {
                    if let LiveState::Ready(session) = &*live {
                        session.wants_frame.set(true);
                    }
                }
                request_frame_on(widget, FrameReason::Scale);
            });
        }

        // --- IME + keyboard input: see `keyboard::attach_keyboard_input`'s own doc for exactly
        // what this wires onto `gl_area` (GtkIMMulticontext composition + GtkEventControllerKey
        // plain-text input). The constructed `IMMulticontext` is handed back so it can be stored
        // in `Self::im_context` below for `grab_focus()` to use later.
        let key_activity = keyboard::KeyActivity::default();
        let im_context = keyboard::attach_keyboard_input(&gl_area, &live_state, &key_activity);

        // --- mouse input: three GTK4 controllers on the GLArea for click/drag-selection/
        // scroll-wheel. All three share the exact content_region/grid-scale coordinate math
        // `gl_interop::current_content_region`/`gl_interop::pixel_to_grid_pos` already provide,
        // and all three unconditionally target `LiveHarness`'s single base grid.
        {
            let click = GestureClick::new();
            click.set_button(0);
            {
                let live_state = live_state.clone();
                let gl_area = gl_area.clone();
                click.connect_pressed(move |gesture, _n_press, x, y| {
                    mouse::handle_mouse_button(&live_state, &gl_area, gesture, x, y, true);
                });
            }
            {
                let live_state = live_state.clone();
                let gl_area = gl_area.clone();
                click.connect_released(move |gesture, _n_press, x, y| {
                    mouse::handle_mouse_button(&live_state, &gl_area, gesture, x, y, false);
                });
            }
            gl_area.add_controller(click);
        }

        {
            let motion = EventControllerMotion::new();
            {
                let live_state = live_state.clone();
                motion.connect_motion(move |controller, x, y| {
                    mouse::handle_mouse_motion(&live_state, controller, x, y);
                });
            }
            {
                let live_state = live_state.clone();
                motion.connect_enter(move |controller, x, y| {
                    mouse::handle_mouse_motion(&live_state, controller, x, y);
                });
            }
            gl_area.add_controller(motion);
        }

        {
            let scroll = EventControllerScroll::new(EventControllerScrollFlags::BOTH_AXES);
            let live_state = live_state.clone();
            scroll
                .connect_scroll(move |controller, dx, dy| mouse::handle_mouse_scroll(&live_state, controller, dx, dy));
            gl_area.add_controller(scroll);
        }

        // --- render: build (lazily) and drive one LiveHarness frame every tick. See the
        // `LiveState` doc for why construction is deliberately split across two render callbacks
        // instead of happening inline here.
        let render: RenderFn = {
            let shared = shared.clone();
            let skia_state = skia_state.clone();
            let live_state = live_state.clone();
            // This closure is the only place `LiveHarness` is ever constructed, so it is the one
            // reader of the options a start (the first, or a restart) uses.
            let spawn_options = spawn_options.clone();
            let ended_for_render = ended.clone();
            let clear_color = clear_color.clone();
            let focused = focused.clone();
            let pending_fullscreen = pending_fullscreen.clone();
            let scale_watch_for_ready = scale_watch.clone();
            let scale_watch_for_render = scale_watch.clone();
            let last_fullscreen_for_render = last_fullscreen_setting.clone();
            let tick_driver_for_render = tick_driver.clone();
            let fb_size = fb_size.clone();
            let cell_size_callback_for_render = cell_size_callback.clone();
            let grid_top_share_for_render = grid_top_share.clone();
            let last_cell_size = last_cell_size.clone();
            let start_failed_callback_for_render = start_failed_callback.clone();
            let session_ready_callback_for_render = session_ready_callback.clone();
            let failed_painted_for_render = failed_painted.clone();
            Rc::new(move |widget: &GLArea, target: RenderTarget| {
                let separate = match target {
                    RenderTarget::Subsurface {
                        framebuffer,
                        buffer,
                        content,
                        os_scale,
                    } => {
                        if shared.fb_size.get() != content || shared.device_scale() != os_scale {
                            framebuffer_resized(&shared, content.0, content.1, os_scale);
                        }
                        Some((framebuffer, buffer))
                    }
                    RenderTarget::GtkFramebuffer { os_scale } | RenderTarget::OwnTexture { os_scale } => {
                        let content = (
                            (f64::from(widget.width()) * os_scale).round() as i32,
                            (f64::from(widget.height()) * os_scale).round() as i32,
                        );
                        if shared.fb_size.get() != content || shared.device_scale() != os_scale {
                            framebuffer_resized(&shared, content.0, content.1, os_scale);
                        }
                        None
                    }
                };
                let (os_scale, own_buffer) = match target {
                    RenderTarget::GtkFramebuffer { os_scale } => (os_scale, false),
                    RenderTarget::OwnTexture { os_scale } => (os_scale, true),
                    RenderTarget::Subsurface { os_scale, .. } => (os_scale, true),
                };
                shared.device_scale.set(os_scale.max(1.0));

                let mut state_slot = skia_state.borrow_mut();

                if state_slot.is_none() {
                    let interface = make_gl_interface();
                    let Some(gr_context) = direct_contexts::make_gl(interface, None) else {
                        return glib::Propagation::Proceed;
                    };
                    let width = (f64::from(widget.width()) * shared.device_scale()).round() as i32;
                    let height = (f64::from(widget.height()) * shared.device_scale()).round() as i32;
                    println!(
                        "[init] Skia DirectContext created; initial fb={}x{}px scale_factor={}",
                        width,
                        height,
                        shared.device_scale()
                    );
                    *state_slot = Some(SkiaState::new(gr_context, width, height));
                    // The same computation GTK 4.22.5 uses for its own buffers and `resize`
                    // signal (`gtkglarea.c` `gtk_gl_area_snapshot`), so this agrees with the
                    // `resize` that normally arrived first; it covers one that has not.
                    fb_size.set((width, height));
                }

                let Some(state) = state_slot.as_mut() else {
                    return glib::Propagation::Proceed;
                };
                // Must come before anything draws this frame: GTK shares this GL context with us
                // and rebinds textures behind Skia's back (on a resize in particular), which makes
                // every glyph paint as a solid block until Skia is told to distrust its state
                // cache. See `SkiaState::invalidate_cached_gl_state` for the full root-cause
                // record and the measurements behind doing this every frame rather than only on
                // resize frames.
                state.invalidate_cached_gl_state();
                // `EditorGlArea`'s own tiled buffer is bound (`editor_area`): Skia draws straight
                // into it, with no intermediate and no copy. Otherwise `GtkGLArea`'s.
                let mut separate_surface = if let Some((framebuffer, buffer)) = separate {
                    use skia_safe::gpu::{backend_render_targets, gl::FramebufferInfo, surfaces, SurfaceOrigin};
                    let target = backend_render_targets::make_gl(
                        buffer,
                        0,
                        8,
                        FramebufferInfo {
                            fboid: framebuffer,
                            format: skia_safe::gpu::gl::Format::RGBA8.into(),
                            ..Default::default()
                        },
                    );
                    // Top row first in the buffer, so the child needs no buffer transform: the viewport crops a buffer
                    // larger than the frame, and Hyprland 0.56 shows a flipped buffer as if it cropped before it
                    // transformed: the frame sat shifted up by the buffer's spare rows.
                    let surface = surfaces::wrap_backend_render_target(
                        &mut state.gr_context,
                        &target,
                        SurfaceOrigin::TopLeft,
                        skia_safe::ColorType::RGBA8888,
                        None,
                        None,
                    );
                    if surface.is_none() {
                        return glib::Propagation::Proceed;
                    }
                    state.render_surface = None;
                    surface
                } else {
                    state.ensure_gtk_surface(own_buffer);
                    state.ensure_render_surface(own_buffer);
                    None
                };

                let Some(gtk_surface) = separate_surface.as_mut().or(state.gtk_surface.as_mut()) else {
                    return glib::Propagation::Proceed;
                };

                let (fb_w, fb_h) = fb_size.get();
                let content_region = compute_content_region(fb_w, fb_h);
                // All content, including placeholders, uses the owned target when available.
                // Target creation failure uses the original direct path.
                let canvas = match state.render_surface.as_mut() {
                    Some(surface) => surface.canvas(),
                    None => gtk_surface.canvas(),
                };

                // Cheap defensive clear, predating this work. Paint the entire framebuffer
                // `OUTSIDE_COLOR` first, then hand only `content_region` to whatever's actually
                // drawing this frame. At CONTENT_MARGIN 0 those two are the same rect and *every*
                // arm below repaints all of it -- the renderer clip-clears `content_region` to
                // nvim's own background, the placeholder arms fill it opaquely -- so nothing this
                // clear writes survives the frame today; see `OUTSIDE_COLOR`'s own doc for the
                // positive-control evidence.
                canvas.clear(clear_color.get());

                // Set from inside the `Ready` arm below, then fired only *after* `live` is
                // dropped -- the same reentrancy rule the tick callback's `fullscreen_callback_for_tick`
                // follows (a host's handler can call straight back into this pane).
                let mut cell_size_changed_to: Option<(f64, f64)> = None;
                // Set from inside the `Starting` arm's `Err` case below, fired only *after*
                // `live` is dropped -- same reentrancy rule. Unlike `cell_size_changed_to` this fires
                // at most once per launch, since each launch goes `Starting` -> `Failed` at most once.
                let mut start_failed_message: Option<String> = None;
                // Set where the `Starting` arm's launch makes a session, fired only *after* `live` is
                // dropped -- same reentrancy rule. Once per session: the first start and each restart.
                let mut session_became_ready = false;
                // Set in the `Ready`, `Failed` and `Exited` arms, acted on after `live` is dropped:
                // whether the tick callback is still wanted once this frame is committed (F2/F3).
                let mut tick_wanted_after_render: Option<bool> = None;

                let starting = spawn_options.borrow().starting_color.map(rgb_color);
                let mut live = live_state.borrow_mut();
                match &mut *live {
                    LiveState::NotStarted => {
                        // Paint+present a placeholder frame *before* the blocking
                        // LiveHarness::with_options call below ever runs (that call happens on
                        // the *next* render callback, once this state transition has actually
                        // been presented to the compositor).
                        println!(
                            "[live] showing 'starting nvim...' placeholder; LiveHarness::with_options \
                             will run (and block this thread) on the next frame"
                        );
                        fill_content_region(canvas, &content_region, placeholder_color(starting));
                        // A restart of a failed pane starts here too: its failure, if it comes
                        // again, needs its own paint.
                        failed_painted_for_render.set(false);
                        *live = LiveState::Starting;
                    }
                    LiveState::Starting => {
                        fill_content_region(canvas, &content_region, placeholder_color(starting));

                        let os_scale_factor = shared.device_scale();
                        let spawn = spawn_options.borrow();
                        let (clean, cwd) = (spawn.clean, spawn.cwd.clone());
                        let attach = spawn.attach.clone();
                        let owned_options = || LiveHarnessOptions {
                            os_scale_factor,
                            extra_nvim_args: nvim_args(clean, &spawn.extra_nvim_args, &spawn.open),
                            // Cloned rather than moved because a restart may start from these
                            // options again; this arm runs once per start, so the clone does too.
                            child_env: spawn.child_env.clone(),
                            cwd: cwd.clone(),
                            // Off, unconditionally, for this embedding -- the opposite of the
                            // `LiveHarnessOptions` default, and the one place this crate takes a
                            // view rather than passing a host's choice through.
                            //
                            // Neovide's startup-message capture attaches with `ext_messages` on
                            // and, on the first flush, restores the built-in message UI by writing
                            // the *pre-attach* `cmdheight` back. That value is the stock `1`: nvim
                            // has not read the user's config when it is sampled. For a config that
                            // externalises the cmdline itself (noice.nvim, which LazyVim ships),
                            // the `1` lands on top of the `0` that config chose and nvim then
                            // reserves a row nothing ever paints -- measured on this pane as a
                            // one-cell band of nvim's own `Normal` background along the bottom,
                            // 44px at a 44px cell, sitting above the shell's status bar and never
                            // filling.
                            //
                            // Turning the capture off is not just a smaller evil here, it is the
                            // right shape: `ext_messages` is set only inside the same branch that
                            // reads the pre-attach `cmdheight`, so opting out leaves nvim's own
                            // message UI in place rather than externalising messages with nothing
                            // to restore them. Verified at the nvim protocol level against both a
                            // cmdline-externalising config and a plain one with no message handler
                            // at all: the plain config keeps a real, functional command line (still
                            // `cmdheight=1`, still painted -- an `:echomsg` reaches the built-in
                            // message grid either way), and the externalising config gets the row
                            // back. What is given up is narrow and was measured too: an error
                            // raised while loading the config is painted onto nvim's own message
                            // grid, where it can want a keypress to dismiss, instead of being held
                            // and replayed after the first frame.
                            startup_message_capture: false,
                            minimum_nvim_version: Some(NVIM_FLOOR),
                            // nvim's clipboard is the host's (`child_env`, `extra_nvim_args`), also on a
                            // server a followed `:restart` reaches: the fork puts its own provider into
                            // no session.
                            remote_clipboard: false,
                            ..Default::default()
                        };
                        let options = match &attach {
                            // The person's nvim, not one of ours: nothing to start, so none of the
                            // spawn's arguments, environment or directory apply, and nvim keeps its own
                            // clipboard provider.
                            Some(spec) => LiveHarnessOptions {
                                os_scale_factor,
                                startup_message_capture: false,
                                minimum_nvim_version: Some(NVIM_FLOOR),
                                server: Some(spec.server.clone()),
                                remote_clipboard: false,
                                ..Default::default()
                            },
                            None => owned_options(),
                        };
                        drop(spawn);
                        match &attach {
                            Some(spec) => println!(
                                "[live] attaching LiveHarness::with_options(os_scale_factor={os_scale_factor}) to the \
                                 running nvim at {} -- it blocks the GTK main loop until the attach's setup is \
                                 answered, at most the fork's 4 s bound",
                                spec.server
                            ),
                            None => println!(
                                "[live] constructing LiveHarness::with_options(os_scale_factor={os_scale_factor}, \
                                 clean={clean}, cwd={cwd:?}) -- this performs a real, synchronous nvim \
                                 launch and WILL block the GTK main loop until it returns"
                            ),
                        }
                        // Before the launch, so the one new `--embed` child after it is nvim. An attach
                        // starts no process, and an `--embed` child of this process found then would
                        // not be the nvim attached to: nothing is looked for.
                        let children_before = attach.is_none().then(nvim_child::direct_children);
                        let t0 = Instant::now();
                        // A construction panic must stop here: GTK's render trampoline cannot unwind.
                        match catch_unwind(AssertUnwindSafe(|| LiveHarness::with_options(options))) {
                            Ok(Ok(mut harness)) => {
                                let elapsed = t0.elapsed();
                                println!(
                                    "[live] LiveHarness::with_options returned after {elapsed:?} \
                                     (blocked the GTK main loop for that long)"
                                );
                                // An attached session holds no process: nothing here may watch, signal
                                // or end an nvim this pane did not start, so nothing asks which it is.
                                let nvim_process = children_before.map(|children_before| {
                                    let nvim_process = Arc::new(nvim_child::NvimProcess::new(
                                        nvim_child::NvimChild::find_new(&children_before),
                                    ));
                                    match nvim_process.launched_pid() {
                                        Some(pid) => println!(
                                            "[live] the fork started pid {pid} (nvim, or a launcher of \
                                             it); asking nvim for its own pid"
                                        ),
                                        None => println!(
                                            "[live] the process the fork started was not found; asking \
                                             nvim for its own pid"
                                        ),
                                    }
                                    ask_nvim_for_its_pid(&harness, &nvim_process, None);
                                    nvim_process
                                });
                                if nvim_process.is_none() {
                                    println!(
                                        "[live] attached as one more UI of an nvim this pane did not start: no \
                                         process of it is held, and letting go of it only closes this channel"
                                    );
                                }
                                *ended_for_render.borrow_mut() = None;
                                // The frozen-scroll fix: `LiveHarnessOptions::grid_size`
                                // (left `None` above, so `DEFAULT_GRID_SIZE` 100x50) only ever
                                // sets nvim's grid size at `nvim_ui_attach` time and has no
                                // relationship to this pane's actual `content_region` -- resize
                                // it immediately to what the real content_region fits.
                                // Through `grid_layout`, the one geometry every other caller uses;
                                // snapping never changes the row count
                                // (`snapping_never_changes_how_many_rows_fit`), so this is the same
                                // size the unsnapped region gave.
                                let grid_size = grid_layout(fb_w, fb_h, harness.grid_scale(), |r| {
                                    grid_size_for_content_region(&harness, r)
                                })
                                .grid;
                                println!(
                                    "[live] resizing nvim grid to {}x{} to match initial \
                                     content_region ({}x{}px) -- fixes the P2 frozen-scroll bug \
                                     (nvim's grid no longer stays stuck at LiveHarnessOptions' \
                                     launch-time default)",
                                    grid_size.width,
                                    grid_size.height,
                                    (content_region.max.x - content_region.min.x) as i32,
                                    (content_region.max.y - content_region.min.y) as i32,
                                );
                                harness.resize_grid(grid_size);
                                // A focus report can arrive before nvim exists -- the host tracks
                                // GTK focus from the first frame. Hand it over now, or the cursor
                                // would stay solid until the next focus change.
                                if let Some(state) = focused.get() {
                                    harness.set_focused(state);
                                }
                                // Same for a fullscreen write the host made before nvim existed.
                                if let Some(fullscreen) = pending_fullscreen.take() {
                                    harness.set_fullscreen_setting(fullscreen);
                                }
                                // Same again for a scale-factor write the host made before nvim
                                // existed (the zoom-together design's app accelerators/prefix keys
                                // can fire before this pane's first frame ever reaches `Ready`).
                                if let Some(scale_factor) = scale_watch_for_ready.take_pending_for_ready() {
                                    harness.set_scale_factor_setting(scale_factor);
                                }
                                *live = LiveState::Ready(Box::new(LiveSession::new(
                                    harness,
                                    grid_size,
                                    fb_size.clone(),
                                    shared.device_scale.clone(),
                                    nvim_process,
                                    &tick_driver_for_render,
                                )));
                                session_became_ready = true;
                            }
                            Ok(Err(err)) => {
                                let elapsed = t0.elapsed();
                                let chain = format!("{err:#}");
                                println!("[live] LiveHarness::with_options failed after {elapsed:?}: {chain}");
                                // The raw chain above is what stdout gets (unchanged); the
                                // pane's own state, and what a host is handed, is the built message
                                // -- what failed, the version floor, where to get a newer nvim.
                                let message = with_display_hint(
                                    build_start_failure_message(&chain),
                                    &DisplayFacts::read(widget.display().type_().name()),
                                );
                                *live = LiveState::Failed(message.clone());
                                start_failed_message = Some(message);
                            }
                            Err(payload) => {
                                let elapsed = t0.elapsed();
                                let message = with_display_hint(
                                    build_start_panic_message(payload.as_ref()),
                                    &DisplayFacts::read(widget.display().type_().name()),
                                );
                                println!("[live] LiveHarness::with_options failed after {elapsed:?}: {message}");
                                *live = LiveState::Failed(message.clone());
                                start_failed_message = Some(message);
                            }
                        }
                    }
                    LiveState::Ready(session) => {
                        // Keep diagnostics on wall time, but animate on GDK's stable frame time.
                        // After idle, `last_frame` can be seconds old: feeding that gap into a
                        // newly arrived cursor movement completes its spring in the first frame.
                        let (frame_dt, fps) = session.tick();
                        let (frame_time, refresh_interval) = widget
                            .frame_clock()
                            .map(|clock| {
                                let time = clock.frame_time();
                                (time, clock.refresh_info(time).0)
                            })
                            .unwrap_or_else(|| (glib::monotonic_time(), 16_667));
                        // Up to and including the first frame that shows nvim, everything is placed,
                        // not animated: nothing was on screen to move from (`AnimationClock::place`).
                        let dt = if session.logged_ready {
                            if let Some(pane) =
                                subsurface_on(widget).filter(|pane| pane.is_active() && separate.is_some())
                            {
                                session.animation_clock.advance_subsurface(
                                    glib::monotonic_time(),
                                    pane.period(),
                                    session.last_animating.get(),
                                )
                            } else {
                                session.animation_clock.advance(
                                    frame_time,
                                    refresh_interval,
                                    session.last_animating.get(),
                                )
                            }
                        } else {
                            session.animation_clock.place(if separate.is_some() {
                                glib::monotonic_time()
                            } else {
                                frame_time
                            })
                        };
                        // Covers a re-realized widget, or any GTK whose snapshot does not
                        // re-emit `resize` -- `connect_resize` above is the OTHER call site, and
                        // between the two nothing computes a grid from a cell size and a framebuffer
                        // that belong to different scales. Unlike `connect_resize`, nothing else on
                        // this path already sets `wants_frame` for a plain scale change, so fold the
                        // "did this frame actually apply one" result in here.
                        if sync_os_scale(&mut session.harness, shared.device_scale()) {
                            session.wants_frame.set(true);
                        }
                        // Snapped HERE rather than where `content_region` is computed above,
                        // because the two users want different rects. The grid must sit on whole
                        // cells (see `snap_region_to_grid`), while the placeholder arms below have
                        // no grid at all and should cover every pixel they can -- a "starting
                        // nvim..." screen with a band of clear colour along one edge would be a
                        // regression, not a fix.
                        let snapped_scale = session.harness.grid_scale();
                        let layout = grid_layout(fb_w, fb_h, snapped_scale, |r| {
                            grid_size_for_content_region(&session.harness, r)
                        });
                        session.log_layout_if_changed(fb_w, fb_h, snapped_scale, &layout);
                        grid_top_share_for_render.set(gl_interop::band_share(layout.band_top_px, fb_h));
                        let animating = session.harness.render_frame(canvas, Some(&layout.region), dt);
                        if let Some(log) = widget
                            .downcast_ref::<editor_area::EditorGlArea>()
                            .and_then(|area| area.present_log())
                        {
                            log.rendered(session.harness.redraw_batches_seen());
                        }
                        if let Some(c) = pre_ready_cover(starting, session.harness.is_ready()) {
                            fill_content_region(canvas, &content_region, c);
                        }
                        // `render_frame` pumped every pane's batches outside the watch.
                        live_watch::after_outside_pump(Some(Rc::as_ptr(&session.live)));
                        if !same_grid_scale(snapped_scale, session.harness.grid_scale()) {
                            // `render_frame` pumps nvim and applies `g:neovide_scale_factor` and `guifont` changes before
                            // reading `grid_scale`. This frame was snapped for the old cell height, so the new rows
                            // can leave a band at the bottom. Request another frame to snap with the new scale;
                            // the tick's resync corrects the row count. An idle pane has no other render trigger.
                            // The next frame sees the scale already applied, so this does not loop.
                            session.wants_frame.set(true);
                        }
                        // The editor's own cell height is what `shell` puts into
                        // `--nv-editor-row` so the agent panel's status band stops growing past it.
                        // Computed exactly as `cell_size()` does (same `grid_scale`/`scale_factor`
                        // pair), fired only when it actually differs from the last value this pane
                        // reported -- see `cell_size_changed`'s own doc for why `None` always
                        // counts as a change.
                        let current_scale = session.harness.grid_scale();
                        let factor = shared.device_scale();
                        let new_cell_size = (
                            f64::from(current_scale.width()) / factor,
                            f64::from(current_scale.height()) / factor,
                        );
                        if cell_size_changed(last_cell_size.get(), new_cell_size) {
                            last_cell_size.set(Some(new_cell_size));
                            cell_size_changed_to = Some(new_cell_size);
                        }
                        // Share this frame's "do we still need more frames" signals with the tick
                        // callback.
                        session.last_animating.set(animating);
                        if let Some(pane) = subsurface_on(widget).filter(|pane| pane.is_active()) {
                            pane.animating(animating);
                        }
                        session.last_seen_batches.set(session.harness.redraw_batches_seen());
                        // F2: `render_frame` pumped, and an exit, a fullscreen change or a scale
                        // change it consumed is reported only by the tick's service. If this frame
                        // is not animating nothing else would run that service. F3: with none of
                        // those pending and nothing else wanted, the tick stops here, not one
                        // empty cycle later.
                        let exited = session.harness.has_neovim_exited();
                        let service_pending = service_pending_after_render(PendingInputs {
                            exited,
                            close_requested: session.close_requested.get(),
                            fullscreen_changed: session.harness.fullscreen_setting()
                                != last_fullscreen_for_render.get(),
                            scale_changed: scale_watch_for_render.would_change(session.harness.scale_factor_setting()),
                        });
                        tick_wanted_after_render = Some(tick_still_wanted(TickInputs {
                            animating,
                            wants_frame: session.wants_frame.get(),
                            exit_pending: exited && !session.close_requested.get(),
                            service_pending,
                        }));

                        if !session.logged_ready && session.harness.is_ready() {
                            session.logged_ready = true;
                            println!(
                                "[live] LiveHarness reports is_ready()=true after {:?} \
                                 ({} redraw batch(es) applied) -- first real nvim content should be \
                                 visible now",
                                session.start.elapsed(),
                                session.harness.redraw_batches_seen()
                            );
                        }

                        if session.frame_count.is_multiple_of(LOG_EVERY_N_FRAMES) {
                            let elapsed = session.start.elapsed().as_secs_f32();
                            println!(
                                "[frame {:>6}] t={:>7.2}s dt={:>6.2}ms instant_fps={:>6.1} avg_fps={:>6.1} \
                                 fb={}x{} region={}x{}@({},{}) animating={} ready={} batches={} \
                                 nvim_exited={}",
                                session.frame_count,
                                elapsed,
                                frame_dt * 1000.0,
                                fps,
                                session.frame_count as f32 / elapsed.max(0.0001),
                                fb_w,
                                fb_h,
                                (content_region.max.x - content_region.min.x) as i32,
                                (content_region.max.y - content_region.min.y) as i32,
                                content_region.min.x as i32,
                                content_region.min.y as i32,
                                animating,
                                session.harness.is_ready(),
                                session.harness.redraw_batches_seen(),
                                session.harness.has_neovim_exited(),
                            );
                        }
                    }
                    LiveState::Failed(_) => {
                        // The text is drawn by the host's own label, not this canvas.
                        fill_content_region(canvas, &content_region, FAILED_COLOR);
                        // This is the one paint `sessionless_tick_should_render` is waiting
                        // for -- once it is set, the tick callback stops asking for more frames.
                        failed_painted_for_render.set(true);
                        // F3 for a failed pane: this paint is the last one it will ever want, so
                        // the tick stops here rather than one empty cycle later.
                        tick_wanted_after_render = Some(failed_tick_should_render(true));
                    }
                    // Only the clear above: a host that released the pane does not show it, and
                    // nothing will ever change, so the tick stops here too.
                    LiveState::Exited => tick_wanted_after_render = Some(false),
                }
                drop(live);

                match tick_wanted_after_render {
                    Some(true) => tick_driver_for_render.kick(),
                    Some(false) => tick_driver_for_render.stop_after_render(),
                    None => {}
                }

                if let Some(message) = start_failed_message {
                    if let Some(cb) = start_failed_callback_for_render.borrow().as_ref() {
                        cb(&message);
                    }
                }

                if session_became_ready {
                    if let Some(cb) = session_ready_callback_for_render.borrow().as_ref() {
                        cb();
                    }
                }

                if let Some((width, height)) = cell_size_changed_to {
                    if let Some(cb) = cell_size_callback_for_render.borrow().as_ref() {
                        cb(width, height);
                    }
                }

                if let Some(render_surface) = state.render_surface.as_mut() {
                    // Same pixels, origin and extent: one Src copy into GTK's wrapped target.
                    // Surface::draw records into the same context; the existing single flush below
                    // submits both the offscreen rendering and this copy.
                    let mut paint = Paint::default();
                    paint.set_blend_mode(BlendMode::Src);
                    render_surface.draw(
                        gtk_surface.canvas(),
                        (0.0, 0.0),
                        SamplingOptions::default(),
                        Some(&paint),
                    );
                }
                if let Err(error) = state.gr_context.flush_and_submit() {
                    // The frame is lost; the next render draws a whole one again. Said once a process,
                    // since a context that fails once tends to fail every frame.
                    static SAID: std::sync::Once = std::sync::Once::new();
                    SAID.call_once(|| eprintln!("[editor] skia flush failed: {error:?}"));
                }

                glib::Propagation::Stop
            })
        };
        {
            let render = render.clone();
            gl_area.connect_render(move |widget, _| {
                let log = widget
                    .downcast_ref::<editor_area::EditorGlArea>()
                    .and_then(|area| area.present_log());
                if let Some(log) = &log {
                    log.begin_render();
                }
                let result = render(widget, current_target(widget));
                if result == glib::Propagation::Stop {
                    if let Some(log) = log {
                        log.commit(0);
                    }
                }
                result
            });
        }

        {
            let presentation = presentation.clone();
            let render = render.clone();
            gl_area.connect_realize(move |widget| {
                use subsurface::{decide, effective, Decision, Facts};
                let stored = presentation.get().unwrap_or(Presentation::DEFAULT);
                let setting = match effective(stored) {
                    Ok(setting) => setting,
                    Err(reason) => {
                        eprintln!("[editor] subsurface: {reason}; falls back to the GTK path");
                        return;
                    }
                };
                let wayland = widget.display().type_().name() == "GdkWaylandDisplay";
                if !wayland || setting == Presentation::Gtk {
                    return;
                }
                widget.make_current();
                let facts = Facts {
                    wayland,
                    gtk: (gtk4::major_version(), gtk4::minor_version(), gtk4::micro_version()),
                    egl_vendor: if widget.error().is_none() {
                        dmabuf_target::egl_vendor()
                    } else {
                        None
                    },
                };
                if decide(setting, &facts) == Decision::Gtk {
                    return;
                }
                if let Some(error) = widget.error() {
                    eprintln!(
                        "[editor] subsurface: the editor's GL context failed: {error}; falls back to the GTK path"
                    );
                    return;
                }
                let Some(thread) = subsurface::PresenterThread::for_display(&widget.display()) else {
                    eprintln!(
                        "[editor] subsurface: the presentation thread could not start; falls back to the GTK path"
                    );
                    return;
                };
                if let Some(area) = widget.downcast_ref::<editor_area::EditorGlArea>() {
                    subsurface::SubsurfacePane::start(area, thread, render.clone());
                }
            });
        }

        // --- drive redraws off the display's frame clock, tying frame pacing to actual
        // vsync-reported timing rather than a fixed timer -- but, so an idle pane costs nothing, only actually
        // calls `queue_render()` when something genuinely needs another frame.
        {
            let live_state = live_state.clone();
            // No strong `GLArea` here: the driver owns this service, and the area's own render
            // handler owns the driver, so a strong capture would be a cycle through the driver.
            // The area is always the `widget` argument (the tick's own, or the driver's weak ref).
            // Needed for the nvim-exited-on-its-own case just below: invoking the registered
            // `exited_callback` may, on the host side, lead straight back into `shutdown()` (e.g.
            // if the callback closes the host's own window, whose `connect_close_request` handler
            // calls `pane.shutdown()`), which itself needs `live_state.borrow_mut()` -- so this
            // closure must never invoke the callback while still holding `live` borrowed, or
            // that reentrant borrow would hit a `RefCell` `BorrowMutError` panic. See
            // `should_fire_exited_callback` below for how that's kept safe.
            let exited_callback_for_tick = exited_callback.clone();
            let ended_for_tick = ended.clone();
            let followed_callback_for_tick = followed_callback.clone();
            let session_ready_callback_for_tick = session_ready_callback.clone();
            let learned_for_tick = learned.clone();
            let followed_attach_for_tick = followed_attach.clone();
            let unreachable_callback_for_tick = unreachable_callback.clone();
            // Fired after `live` is dropped, for the same reentrancy reason as the exited callback:
            // the host's handler calls straight back into `set_fullscreen_setting`.
            let fullscreen_callback_for_tick = fullscreen_callback.clone();
            // Same reentrancy reason again: the host's handler (`shell`'s pending/echo tracking)
            // calls straight back into `set_scale_factor_setting`.
            let scale_factor_callback_for_tick = scale_factor_callback.clone();
            let scale_watch_for_tick = scale_watch.clone();
            let last_fullscreen_for_tick = last_fullscreen_setting.clone();
            let shared = shared.clone();
            let tick_stats = Rc::new(TickStats::new());
            let fb_size = fb_size.clone();
            // One-shot: `[layout] note:` is printed at most once per pane.
            let layout_note_logged = Cell::new(false);
            // Read (never written) here -- the render callback's `Failed` arm is the only
            // writer. See `failed_tick_should_render`.
            let failed_painted_for_tick = failed_painted.clone();
            let service: Service = Rc::new(move |widget: &GLArea, source: ServiceSource| {
                let mut live = live_state.borrow_mut();
                // Set from inside the `Ready` arm below, then acted on only *after* `live` is
                // dropped -- see the comment on `exited_callback_for_tick` above for why the
                // ordering matters.
                let mut should_fire_exited_callback = false;
                let mut exit_after_quit = false;
                // How an attached session ended, for the exit's log line.
                let mut attached_end: Option<PaneEnded> = None;
                let mut should_fire_unreachable_callback = false;
                let mut fullscreen_changed: Option<bool> = None;
                let mut scale_factor_changed: Option<f32> = None;
                // A `:restart` or `:connect` this pane followed in this run, told to the host after `live`
                // is dropped.
                let mut pane_followed: Option<PaneFollowed> = None;
                let issued = match &mut *live {
                    LiveState::Ready(session) => {
                        // Cheap, non-blocking drain of any nvim redraw traffic that arrived since
                        // the last tick. Inside a wake only the first pane's service pumps, so every
                        // pane is inspected after the one pump that applied its batches.
                        live_watch::pump_once_per_pass(|| session.harness.pump(Duration::ZERO));
                        // The pump applied every pane's batches; from a tick, the watch did not see
                        // them, so the other panes are swept (a wake services every pane itself).
                        if source == ServiceSource::Tick {
                            live_watch::after_outside_pump(Some(Rc::as_ptr(&session.live)));
                        }
                        // Whichever pane's pump applied the end of this pane's session, the new one is set
                        // up here, by this pane's own harness; the pane switches before anything is told.
                        session.harness.follow_if_due();
                        if let Some(how) = session.harness.take_followed() {
                            let how = session.follow(how, &learned_for_tick);
                            if matches!(how, PaneFollowed::Connect { .. }) {
                                followed_attach_for_tick.set(true);
                            }
                            *ended_for_tick.borrow_mut() = None;
                            pane_followed = Some(how);
                        }
                        let fullscreen = session.harness.fullscreen_setting();
                        if last_fullscreen_for_tick.replace(fullscreen) != fullscreen {
                            fullscreen_changed = Some(fullscreen);
                        }
                        // One call, one `ScaleObservation`, used below for both the callback and
                        // `tick_should_render` -- see `ScaleWatch`'s own doc for why this used to
                        // be an inline `Cell` comparison a mutation could pick apart.
                        //
                        // known limit: `ScaleWatch::observe` itself is unit-tested (see this
                        // file's `tests` module), but this call site -- reading
                        // `scale_observation.changed` into both `scale_factor_changed_now` and the
                        // `if` below rather than, say, one of them silently reading `false` -- runs
                        // only inside a real `add_tick_callback`, which needs a live `GtkGLArea` on
                        // a real display. No test here exercises this exact wiring; a GUI pass does.
                        let scale_observation = scale_watch_for_tick.observe(session.harness.scale_factor_setting());
                        let scale_factor_changed_now = scale_observation.changed;
                        if scale_observation.changed {
                            scale_factor_changed = Some(scale_observation.value);
                        }
                        let batches_now = session.harness.redraw_batches_seen();
                        let new_content = batches_now != session.last_seen_batches.get();
                        if new_content {
                            session.last_seen_batches.set(batches_now);
                        }

                        // Resize-resync safety net (found 2026-09-07 on real GNOME/Mutter
                        // hardware, via `shell`, not the headless-sway sandbox every earlier
                        // resize check ran in): `connect_resize` above is the only other place
                        // that calls `resize_grid`, and it only fires on a `GtkGLArea` `resize`
                        // signal. On this real desktop, a window shown already-maximized (or
                        // resized by the compositor while `LiveHarness::with_options`'s
                        // synchronous, main-loop-blocking nvim launch was in flight) can settle at
                        // its final size without ever emitting a *further* `resize` signal after
                        // the Ready-time snapshot the constructor already takes -- leaving nvim's
                        // grid permanently sized for a stale, smaller content_region than the
                        // widget's real one, visible as an undrawn band past the grid's real edge.
                        // (This comment said "leftover `OUTSIDE_COLOR`" until 2026-09-16; that was
                        // never right -- `draw_frame` clip-clears the *current* content_region to
                        // nvim's own background, so the band is that background, not the host's
                        // clear colour.) Re-deriving the grid size from the widget's *current*
                        // actual size on every tick (cheap: a handful of integer ops, no
                        // allocation) and only calling `resize_grid` -- which actually costs --
                        // when it disagrees with `last_grid_size` makes this self-correcting
                        // regardless of which exact GTK/Wayland/compositor timing produced the
                        // staleness, instead of chasing that one root cause.
                        //
                        // The size is GTK's own framebuffer (`fb_size`, set by
                        // `connect_resize`), no longer `widget.width() * scale_factor()`. GTK
                        // 4.22.5 derives its buffers and the `resize` signal from that same
                        // product (`gtkglarea.c`), so the two differ only while the widget has
                        // not been drawn at its latest allocation (hidden by
                        // `set_child_visible`, or between an allocation and its snapshot) --
                        // and then GTK's `resize` on the next snapshot resizes nvim for the size
                        // actually drawn. The `note:` line records it if they ever differ on a
                        // drawn widget, which would mean a GTK whose buffers follow some other
                        // size.
                        //
                        // Device scale advances with the framebuffer, so a scale notification
                        // alone leaves both at their previous values until resize or render.
                        let (width, height) = fb_size.get();
                        let widget_size = (
                            (f64::from(widget.width()) * shared.device_scale()).round() as i32,
                            (f64::from(widget.height()) * shared.device_scale()).round() as i32,
                        );
                        if !layout_note_logged.get()
                            && widget.is_drawable()
                            && width > 0
                            && height > 0
                            && widget_size.0 > 0
                            && widget_size.1 > 0
                            && widget_size != (width, height)
                            && session.harness.os_scale_factor() == shared.device_scale()
                        {
                            layout_note_logged.set(true);
                            println!(
                                "[layout] note: widget*scale ({}x{}) differs from GTK's framebuffer \
                                 ({width}x{height}); using GTK's",
                                widget_size.0, widget_size.1,
                            );
                        }
                        let new_grid_size = grid_layout(width, height, session.harness.grid_scale(), |r| {
                            grid_size_for_content_region(&session.harness, r)
                        })
                        .grid;
                        // A freshly re-realized widget can tick before its first allocation.
                        // A transient 0x0 is not a one-cell editor: resizing nvim then would
                        // disturb its viewport/cursor before the real allocation arrives.
                        let grid_resynced = if width > 0 && height > 0 && new_grid_size != session.last_grid_size.get()
                        {
                            session.harness.resize_grid(new_grid_size);
                            session.last_grid_size.set(new_grid_size);
                            println!(
                                "[tick] resize-resync: grid was stale for the widget's current \
                                 size (fb={width}x{height}px) -- corrected to {}x{}",
                                new_grid_size.width, new_grid_size.height,
                            );
                            true
                        } else {
                            false
                        };

                        // Read-and-clear: a resize or a keypress since the last tick each force
                        // exactly one more frame, on top of the ongoing-animation and new-content
                        // signals above.
                        let wants_frame = session.wants_frame.replace(false) || grid_resynced;

                        // The "dead-looking-alive pane" fix: `shutdown()` already calls
                        // `LiveHarness::shutdown()` when the host closes its own window, but
                        // nothing previously reacted when nvim exits *on its own* (e.g. `:qa!`
                        // typed inside it). `pump()` just above is what actually observes a real
                        // `UserEvent::NeovimExited` arriving asynchronously, so checking right
                        // here, on every service run (the tick, or the fd watch for a hidden or unrealized pane), catches it at the same latency the frame log already
                        // did. `close_requested.replace(true)` is the one-shot guard documented on
                        // `LiveSession::close_requested`; only the tick that flips it false->true
                        // actually fires the callback.
                        //
                        // Only once nvim's own process is gone (`exit_events`).
                        let fork_exited = session.harness.has_neovim_exited();
                        let nvim_alive = fork_exited.then(|| session.nvim_alive()).flatten();
                        if nvim_alive == Some(true) {
                            session.wake_on_nvim_exit();
                        }
                        let events = exit_events(
                            fork_exited,
                            nvim_alive,
                            session.grace_over(),
                            session.unreachable_said.get(),
                            session.close_requested.get(),
                        );
                        if events.unreachable {
                            session.unreachable_said.set(true);
                            should_fire_unreachable_callback = true;
                            println!(
                                "{}",
                                unreachable_log_line(if session.restarted {
                                    NvimStart::Restart
                                } else if session
                                    .nvim_process
                                    .as_ref()
                                    .is_some_and(|process| process.behind_a_launcher())
                                {
                                    NvimStart::Launcher
                                } else {
                                    NvimStart::Direct
                                })
                            );
                        }
                        if events.exited {
                            session.close_requested.set(true);
                            should_fire_exited_callback = true;
                            exit_after_quit =
                                exit_follows_quit(session.watched.as_ref().map(nvim_rpc::WatchedCall::watch).as_ref());
                            attached_end = record_end(session, &ended_for_tick).filter(|_| session.is_attached());
                        }

                        tick_should_render(
                            session.last_animating.get(),
                            new_content,
                            wants_frame,
                            scale_factor_changed_now,
                        )
                    }
                    other => sessionless_tick_should_render(other, failed_painted_for_tick.get()),
                };
                drop(live);

                if let Some(how) = pane_followed {
                    if let Some(cb) = followed_callback_for_tick.borrow().as_ref() {
                        cb(how.clone());
                    }
                    // A followed `:restart` is a new session of the pane's own nvim, which the host sets up
                    // as it does each launch's; a server `:connect` reached is not the host's to set up.
                    if how == PaneFollowed::Restart {
                        if let Some(cb) = session_ready_callback_for_tick.borrow().as_ref() {
                            cb();
                        }
                    }
                }

                if let Some(fullscreen) = fullscreen_changed {
                    println!("[live] g:neovide_fullscreen is now {fullscreen}");
                    if let Some(cb) = fullscreen_callback_for_tick.borrow().as_ref() {
                        cb(fullscreen);
                    }
                }

                if let Some(scale_factor) = scale_factor_changed {
                    println!("[live] g:neovide_scale_factor is now {scale_factor}");
                    if let Some(cb) = scale_factor_callback_for_tick.borrow().as_ref() {
                        cb(scale_factor);
                    }
                }

                if should_fire_unreachable_callback {
                    if let Some(cb) = unreachable_callback_for_tick.borrow().as_ref() {
                        cb();
                    }
                }

                if should_fire_exited_callback {
                    match &attached_end {
                        Some(how) => println!(
                            "[live] the attached nvim's session ended ({how:?}) -- invoking the registered \
                             on_exited_unrequested callback (if any)"
                        ),
                        None => println!("{}", exit_log_line(exit_after_quit)),
                    }
                    if let Some(cb) = exited_callback_for_tick.borrow().as_ref() {
                        cb();
                    }
                }

                if issued {
                    request_frame_on(
                        widget,
                        match source {
                            ServiceSource::Tick => FrameReason::Animation,
                            ServiceSource::FdWatch => FrameReason::Batch,
                        },
                    );
                }
                tick_stats.record(source, issued);

                // Whether another tick cycle is wanted, read after every callback above (one may
                // have asked for a frame). Pending service work is zero here: this run just
                // consumed it.
                match &*live_state.borrow() {
                    LiveState::Ready(session) => tick_still_wanted(TickInputs {
                        animating: session.last_animating.get(),
                        wants_frame: session.wants_frame.get(),
                        exit_pending: session.harness.has_neovim_exited() && !session.close_requested.get(),
                        service_pending: false,
                    }),
                    other => sessionless_tick_should_render(other, failed_painted_for_tick.get()),
                }
            });
            tick_driver.attach(&gl_area, service);
            tick_driver.kick();
        }

        Self {
            widget: gl_area,
            shared,
            hole_changed_callbacks,
            presentation,
            im_context,
            live_state,
            exited_callback,
            unreachable_callback,
            clear_color,
            focused,
            fullscreen_callback,
            pending_fullscreen,
            scale_factor_callback,
            scale_watch,
            cell_size_callback,
            grid_top_share,
            start_failed_callback,
            session_ready_callback,
            key_activity,
            tick_driver,
            spawn_options,
            ended,
            followed_callback,
            learned,
            followed_attach,
        }
    }

    pub(crate) fn request_frame(&self, reason: FrameReason) {
        request_frame_on(&self.widget, reason);
    }

    /// Stores this pane's preference across nvim restarts. A realized area keeps its current
    /// presentation until it is realized again.
    pub fn set_presentation(&self, value: Presentation) {
        self.presentation.set(Some(value));
        if self.widget.is_realized() {
            eprintln!("[editor] subsurface: presentation set after realize; applies at the next realize");
        }
    }

    /// The `GtkGLArea` this pane renders into. The host places this into its own window (e.g. as
    /// an `ApplicationWindow`'s child) -- this pane never does so itself.
    pub fn widget(&self) -> &gtk4::GLArea {
        &self.widget
    }

    /// Whether the last frame was drawn into the pane's own tiled dmabuf buffers and handed to GTK
    /// as a texture (`editor_area`), rather than into `GtkGLArea`'s texture through a Skia
    /// intermediate. Diagnostic: the real-framebuffer regression prints which path it ran on.
    pub fn draws_into_own_buffers(&self) -> bool {
        self.widget
            .downcast_ref::<editor_area::EditorGlArea>()
            .is_some_and(editor_area::EditorGlArea::draws_into_own_buffer)
    }

    /// How many frames went through each presentation path, and how often the own-buffer path
    /// failed, since this pane was built -- across every realize, never reset. Diagnostic: the
    /// real-framebuffer regression fails a run that expects the own buffers on any fallback.
    pub fn presentation_counts(&self) -> PresentationCounts {
        self.widget
            .downcast_ref::<editor_area::EditorGlArea>()
            .map(editor_area::EditorGlArea::presentation_counts)
            .unwrap_or_default()
    }

    /// The rectangle exposed for separate editor presentation, absent on the GTK path.
    pub fn presentation_hole(&self) -> Option<gtk4::graphene::Rect> {
        subsurface_on(&self.widget)?.hole_rect()
    }

    /// Registers a callback for changes to the editor's presentation hole.
    pub fn connect_hole_changed(&self, callback: impl Fn() + 'static) {
        self.hole_changed_callbacks.borrow_mut().push(Rc::new(callback));
    }

    /// Tells the editor whether it has keyboard focus through `LiveHarness::set_focused`.
    /// This updates the cursor and sends `nvim_ui_set_focus` so nvim can fire `FocusGained` and
    /// `FocusLost`. `HIDE_UNFOCUSED_CURSOR_CMD` hides the unfocused cursor unless the user restores
    /// the outline width.
    ///
    /// This is separate from [`grab_focus`](Self::grab_focus). The host defines focus; `shell`
    /// requires both this pane to hold the window's focus widget and the window to be active.
    /// Calls before nvim starts are saved, and the harness drops repeated focus notifications.
    pub fn set_focused(&self, focused: bool) {
        set_focused_and_request(&self.focused, &self.live_state, focused, |reason| {
            self.request_frame(reason)
        });
    }

    pub fn grab_focus(&self) {
        self.widget.grab_focus();
        self.im_context.focus_in();
    }

    /// Registers `callback`, called with the new value each time `g:neovide_fullscreen` changes in
    /// nvim -- a `:let`, a mapping, or a value `init.lua` set (reported on the first tick after
    /// nvim is ready). This pane owns no window, so it acts on nothing itself: the host owns the
    /// real window and follows the variable. Read by the service run (the tick callback, or the fd watch). **Corrected 2026-09-23:**
    /// this used to claim that while this pane's widget is hidden (`set_visible(false)`, e.g. a
    /// pane zoom) a change waits until it is shown again. An adversarial recheck found that
    /// unverified and, per GTK 4.22.5's own source, most likely false: `gtkwidget.c` disconnects a
    /// widget's tick callback only in `gtk_widget_real_unrealize`, and `gtk_widget_on_frame_clock_update`
    /// (what actually invokes it) has no mapped/visible check at all -- so a merely-hidden
    /// (unmapped, not unrealized) widget most likely keeps ticking. Not confirmed on a screen
    /// either way; do not rely on either behaviour without checking. Replaces any earlier callback.
    pub fn on_fullscreen_setting(&self, callback: impl Fn(bool) + 'static) {
        *self.fullscreen_callback.borrow_mut() = Some(Box::new(callback));
    }

    /// Writes `g:neovide_fullscreen` in nvim, for a host whose window changed fullscreen state for
    /// a reason nvim did not see (its own key, the compositor), so the variable keeps matching the
    /// window. The write goes through nvim's own watcher like a `:let`, so the host's
    /// [`on_fullscreen_setting`](Self::on_fullscreen_setting) callback sees it come back; a host
    /// that already matches the value it is told does nothing and so cannot ping-pong. Before nvim
    /// is ready the value is held and written the moment it is.
    pub fn set_fullscreen_setting(&self, fullscreen: bool) {
        if let LiveState::Ready(session) = &*self.live_state.borrow() {
            session.harness.set_fullscreen_setting(fullscreen);
        } else {
            self.pending_fullscreen.set(Some(fullscreen));
        }
    }

    /// `LiveHarness::scale_factor_setting()` (`g:neovide_scale_factor`) right now, or `1.0` before
    /// nvim exists -- Neovide's own default, and this pane's own "not zoomed" answer for a host
    /// that asks before a live session exists at all. Unlike fullscreen, this one is also a
    /// getter and not just a callback: the zoom-together design's `shell` side needs a synchronous
    /// read to reconcile its own pending write against nvim's report (see that design's
    /// "asynchrony and echo" section) without waiting for the next tick's callback.
    pub fn scale_factor_setting(&self) -> f32 {
        if let LiveState::Ready(session) = &*self.live_state.borrow() {
            session.harness.scale_factor_setting()
        } else {
            1.0
        }
    }

    /// Registers `callback`, called with the new value each time `g:neovide_scale_factor` changes
    /// in nvim -- a `:let`, a mapping, or a value `init.lua` set (reported on the first tick after
    /// nvim is ready). This pane owns no window and applies the scale to its own renderer already
    /// (inside `LiveHarness::render_frame`); a host follows the callback to scale anything of its
    /// own alongside the editor. Read by the service run (tick callback or fd watch) -- see
    /// [`on_fullscreen_setting`](Self::on_fullscreen_setting)'s own doc (corrected 2026-09-23) for
    /// what is and is not known about whether hiding this pane's widget pauses the tick (since the
    /// typing-latency fix the fd watch services a hidden or unrealized pane regardless, and the tick
    /// exists only while a frame is wanted).
    /// `shell`'s zoom-together pending model (`shell/src/text_size.rs`) no longer has any
    /// time-based fallback for a paused tick to defeat -- a write it makes stays pending, however
    /// long nvim takes to answer, rather than assuming a fixed delay and going stale against it.
    /// Replaces any earlier callback.
    pub fn on_scale_factor_setting(&self, callback: impl Fn(f32) + 'static) {
        *self.scale_factor_callback.borrow_mut() = Some(Box::new(callback));
    }

    /// Writes `g:neovide_scale_factor` in nvim, for a host whose text-size key nvim never sees
    /// (the zoom-together design's app-level accelerators and `Ctrl+a` prefix). The write goes
    /// through nvim's own watcher like a `:let`, so the host's
    /// [`on_scale_factor_setting`](Self::on_scale_factor_setting) callback sees it come back; a
    /// host that already matches the value it is told does nothing and so cannot ping-pong.
    /// Before nvim is ready the value is held and written the moment it is -- mirrors
    /// [`set_fullscreen_setting`](Self::set_fullscreen_setting) exactly.
    pub fn set_scale_factor_setting(&self, scale_factor: f32) {
        if let LiveState::Ready(session) = &*self.live_state.borrow() {
            session.harness.set_scale_factor_setting(scale_factor);
        } else {
            self.scale_watch.buffer_pending(scale_factor);
        }
    }

    /// Sends `keys`, in nvim's own key notation (`"<C-a>"`), exactly as a typed key would arrive:
    /// through `nvim_input`. For a host that took a key before this pane saw it and now hands it
    /// on -- `shell`'s `Ctrl+a Ctrl+a`. Dropped, with a log line, before nvim is ready.
    pub fn send_keys(&self, keys: &str) {
        if let LiveState::Ready(session) = &mut *self.live_state.borrow_mut() {
            // Forwarded to nvim only; its reply requests the frame.
            session.harness.send_text_input(keys);
        } else {
            println!("[live] send_keys: {} bytes before nvim is ready -- dropped", keys.len());
        }
    }

    /// Whether nvim is up, so a host can refuse a request `send_keys` would drop (phase 3: the
    /// scratch round trips answer "the editor is not ready yet" instead of doing nothing).
    pub fn is_ready(&self) -> bool {
        matches!(&*self.live_state.borrow(), LiveState::Ready(_))
    }

    /// Whether the pane is on its way to an nvim it has not launched yet: built, or restarted, and
    /// waiting for the frames that paint its placeholder and then launch it.
    pub fn is_starting(&self) -> bool {
        matches!(&*self.live_state.borrow(), LiveState::NotStarted | LiveState::Starting)
    }

    /// Whether nvim is up and has not exited, as of the last service run's pump (tick or fd watch). Unlike
    /// [`is_ready`](Self::is_ready), which stays `true` from an unrequested exit until the host
    /// [`release_exited`](Self::release_exited) the pane: a host deciding whether nvim can still be
    /// asked something (a window close's `:confirm qall`) asks this.
    pub fn is_running(&self) -> bool {
        matches!(&*self.live_state.borrow(), LiveState::Ready(session) if !session.harness.has_neovim_exited())
    }

    /// Whether nvim ran and has exited: seen exited and not yet released, or released
    /// ([`LiveState::Exited`]). `false` for an nvim that never started, and for one
    /// the fork reports exited while nvim's own process is still alive
    /// ([`nvim_exit_pending`](Self::nvim_exit_pending)).
    pub fn nvim_exited(&self) -> bool {
        match &*self.live_state.borrow() {
            LiveState::Ready(session) => session.harness.has_neovim_exited() && !session.exit_pending(),
            LiveState::Exited => true,
            LiveState::NotStarted | LiveState::Starting | LiveState::Failed(_) => false,
        }
    }

    /// The fork reports nvim exited, yet nvim's own process lives -- its launcher exited without it
    /// (`LiveSession::exit_pending`'s doc). Neither running (the fork takes no keys and no request
    /// for it) nor exited: its exit is reported once the process is gone, and a host that wants it
    /// gone sooner [`end_nvim`](Self::end_nvim)s it.
    pub fn nvim_exit_pending(&self) -> bool {
        matches!(&*self.live_state.borrow(), LiveState::Ready(session) if session.exit_pending())
    }

    /// Has nvim run `code` through `nvim_exec_lua` -- an RPC request, never typed keys, so a key nvim
    /// is waiting for (after `f`, inside `getchar()`) cannot swallow it (`nvim_rpc`'s module doc) --
    /// handed to the session's call line, returning at once: it is written after every
    /// [`exec_lua`](Self::exec_lua) call made before it, without waiting for their answers. Until the
    /// request comes back, a thread of its own asks nvim's fast `nvim_get_mode` every 250 ms, so a
    /// host can tell an nvim showing a dialog from one whose loop is stuck
    /// ([`watched_call`](Self::watched_call)). Replaces the previous watch (whose request still runs
    /// to its end). `false`, sending nothing, when nvim is not running, and on an attached pane
    /// ([`is_attached`](Self::is_attached)): this is how a window asks nvim to quit, and an nvim the
    /// pane did not start is never asked.
    pub fn exec_lua_watched(&self, code: &str) -> bool {
        let mut live = self.live_state.borrow_mut();
        let LiveState::Ready(session) = &mut *live else {
            return false;
        };
        if session.is_attached() {
            println!(
                "[live] exec_lua_watched refused: the pane is attached to an nvim it did not start -- nothing sent"
            );
            return false;
        }
        if session.harness.has_neovim_exited() {
            return false;
        }
        let Some(nvim) = session.harness.neovim_handler().clone_current_neovim() else {
            println!("[live] exec_lua_watched: no nvim connection -- nothing sent");
            return false;
        };
        let call = {
            let nvim = nvim.clone();
            let code = code.to_owned();
            async move {
                match nvim.exec_lua(&code, vec![]).await {
                    Ok(_) => {
                        println!("[live] exec_lua_watched: the Lua returned");
                        true
                    }
                    Err(err) => {
                        if let Some(note) = watched_failure_note(&err) {
                            println!("[live] exec_lua_watched: {note}");
                        }
                        false
                    }
                }
            }
        };
        let probe = move || {
            let nvim = nvim.clone();
            async move {
                let pairs = nvim.get_mode().await.ok()?;
                let field = |name: &str| pairs.iter().find(|(k, _)| k.as_str() == Some(name)).map(|(_, v)| v);
                Some((
                    field("mode").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                    field("blocking").and_then(rmpv::Value::as_bool).unwrap_or(false),
                ))
            }
        };
        let started = session
            .call_line()
            .and_then(|line| nvim_rpc::WatchedCall::start(line, "exec_lua", call, probe));
        match started {
            Ok(watched) => {
                session.watched = Some(watched);
                // Buys one service run and one frame (the tick starts); nothing in `service` reads
                // `session.watched`, so this is not what keeps the watch alive.
                session.wants_frame.set(true);
                true
            }
            Err(err) => {
                eprintln!("[live] exec_lua_watched: could not start its thread ({err}) -- nothing sent");
                false
            }
        }
    }

    /// `nvim_exec_lua(code, args)` handed to the session's call line: written to nvim after every
    /// call this pane made before it (this and [`exec_lua_watched`](Self::exec_lua_watched)), and
    /// without waiting for their answers; nvim runs them in that order. `reply` runs on the line's
    /// thread once nvim answered (or the connection ended). `false`, `reply` dropped uncalled and
    /// nothing sent, when nvim is not running or the line's thread could not start.
    ///
    /// nvim may take long to answer: it queues a request while it waits for a key, and answers
    /// after the key. A call nobody waits for any more stays on the line until then, costing no
    /// thread of its own, and every later call is still written behind it. Unlike
    /// [`exec_lua_watched`](Self::exec_lua_watched) this keeps no record in the session and starts no
    /// watch: the quit and its watch stay the window's own.
    ///
    /// With a [`ChunkCheck`] ([`NeovideEditorPaneOptions::chunk_check`]) the chunk goes only where
    /// the check lets it, with the arguments the check returns; a refusal is answered at once,
    /// `reply` called here with its reason, and nothing is sent. An attached pane with no check
    /// answers every call that way. `true` whenever `reply` was or will be called.
    pub fn exec_lua(&self, code: &'static str, args: Vec<rmpv::Value>, reply: LuaReply) -> bool {
        let attached = match &*self.live_state.borrow() {
            LiveState::Ready(session) if !session.harness.has_neovim_exited() => session.is_attached(),
            _ => return false,
        };
        // No borrow is held while the host's check runs.
        let check = self.spawn_options.borrow().chunk_check.clone();
        let checked = match check {
            Some(check) => check.run(code, args),
            None if attached => Err(ChunkRefused {
                reason: "the editor is attached to an nvim it did not start, and no chunk check was given".to_owned(),
            }),
            None => Ok(args),
        };
        let args = match checked {
            Ok(args) => args,
            Err(refused) => {
                println!(
                    "[live] exec_lua: a chunk of {} bytes was refused before anything was sent ({refused})",
                    code.len()
                );
                reply(Err(refused.reason));
                return true;
            }
        };
        let live = self.live_state.borrow();
        let LiveState::Ready(session) = &*live else {
            return false;
        };
        if session.harness.has_neovim_exited() {
            return false;
        }
        let Some(nvim) = session.harness.neovim_handler().clone_current_neovim() else {
            return false;
        };
        let line = match session.call_line() {
            Ok(line) => line,
            Err(err) => {
                eprintln!("[live] exec_lua: could not start the call line's thread ({err}) -- nothing sent");
                return false;
            }
        };
        let call = async move { nvim.exec_lua(code, args).await.map_err(|e| e.to_string()) };
        if line.submit(call, reply) {
            true
        } else {
            eprintln!("[live] exec_lua: the call line's thread is gone -- nothing sent");
            false
        }
    }

    /// Which nvim session this pane runs: a new number each time a session becomes Ready, `None`
    /// while none is or once nvim has exited. An answer is about the session it was asked of.
    pub fn session_serial(&self) -> Option<u64> {
        match &*self.live_state.borrow() {
            LiveState::Ready(session) if !session.harness.has_neovim_exited() => Some(session.serial),
            _ => None,
        }
    }

    /// The last [`exec_lua_watched`](Self::exec_lua_watched) request as it stands: sent when, back
    /// yet, and nvim's last `nvim_get_mode` answer since. `None` before one, or once nvim is released.
    pub fn watched_call(&self) -> Option<CallWatch> {
        match &*self.live_state.borrow() {
            LiveState::Ready(session) => session.watched.as_ref().map(nvim_rpc::WatchedCall::watch),
            _ => None,
        }
    }

    /// [`watched_call`](Self::watched_call) once its worker has published how the request ended,
    /// waited for up to [`SETTLE_WAIT`]: nvim-rs hands the answer to the
    /// worker through a oneshot, and nvim's exit reaches the host without waiting for it. For a
    /// host deciding, at nvim's exit, whether nvim had answered its quit.
    pub fn settled_watched_call(&self) -> Option<CallWatch> {
        match &*self.live_state.borrow() {
            LiveState::Ready(session) => session.watched.as_ref().map(|call| call.settled(SETTLE_WAIT)),
            _ => None,
        }
    }

    /// Ends nvim without `:qa!`, which would discard unsaved buffers and their swap files: closes
    /// its stdin (the fork's
    /// `LiveHarness::hang_up`), on which nvim exits keeping its swap files, and escalates on a thread
    /// by the pid this pane holds (`nvim_child::end`: SIGTERM, which nvim answers the same way and
    /// which ends a dialog EOF does not; SIGKILL after 5 s) -- never the launcher that started it
    /// (`nvim_child`'s module doc). The order is `nvim_child::schedule`'s: stdin closed now and
    /// SIGTERM from 1 s; or, while this pane's `:confirm qall` has not come back and nvim may be in
    /// its prompt, SIGTERM now and stdin closed at 1 s, by a timer on this thread, since the
    /// harness lives here. Its exit then reaches the host as any exit does,
    /// once the process is gone ([`on_exited_unrequested`](Self::on_exited_unrequested)).
    /// Idempotent: a second call starts no second watch. `false`, doing nothing, when there is no
    /// session, and on an attached pane ([`is_attached`](Self::is_attached)), whose nvim is not this
    /// pane's to end: it only lets go of it ([`hang_up_handle`](Self::hang_up_handle),
    /// [`shutdown`](Self::shutdown)).
    pub fn end_nvim(&self) -> bool {
        if self.is_attached() {
            println!("[live] end_nvim refused: the pane is attached to an nvim it did not start -- nothing ended");
            return false;
        }
        let LiveState::Ready(session) = &mut *self.live_state.borrow_mut() else {
            return false;
        };
        let Some(process) = session.nvim_process.clone() else {
            return false;
        };
        if session.ending.get().is_some() {
            return true;
        }
        let at = Instant::now();
        let schedule = session.ending_schedule();
        session.ending.set(Some((at, schedule)));
        let watched = process.nvim_pid();
        if schedule.hang_up_after.is_zero() {
            session.harness.hang_up();
            println!("[live] end_nvim: nvim's stdin is closed; watching pid {watched:?} until it has gone");
        } else {
            let live = Rc::downgrade(&self.live_state);
            // The timer belongs to the session it ends. nvim may answer the SIGTERM well inside the
            // delay, and a host may release the pane and restart it before the timer fires: the
            // session it finds then is a new nvim, whose stdin it must leave open.
            let ending = session.serial;
            glib::timeout_add_local_once(schedule.hang_up_after, move || {
                let Some(live) = live.upgrade() else { return };
                let mut live = live.borrow_mut();
                match &mut *live {
                    LiveState::Ready(session) if session.serial == ending => {
                        // Idempotent, and harmless once nvim has gone.
                        session.harness.hang_up();
                        println!("[live] end_nvim: nvim's stdin is closed");
                    }
                    _ => println!("[live] end_nvim: the session it was to end has gone; nothing to close"),
                }
            });
            println!(
                "[live] end_nvim: nvim may be in its prompt: SIGTERM first, stdin closed in {:?}; watching pid \
                 {watched:?} until it has gone",
                schedule.hang_up_after
            );
        }
        run_detached(
            Box::new(move || {
                // The hang-up is this thread's timer's, above: the harness is not `Send`.
                let ended = nvim_child::end(&process, at, schedule, &mut || {});
                println!("[live] end_nvim: {ended:?}");
            }),
            spawn_named("nvim end"),
        );
        true
    }

    /// Whether this pane is attached to an nvim it did not start ([`NeovideEditorPaneOptions::attach`]):
    /// its session's, once it has one, else what its options ask for. Such a pane refuses
    /// [`end_nvim`](Self::end_nvim), [`exec_lua_watched`](Self::exec_lua_watched) and
    /// [`restart`](Self::restart), and lets go of nvim only by closing its channel.
    pub fn is_attached(&self) -> bool {
        if self.followed_attach.get() {
            return true;
        }
        match &*self.live_state.borrow() {
            LiveState::Ready(session) => session.is_attached(),
            _ => self.spawn_options.borrow().attach.is_some(),
        }
    }

    /// A handle that closes an attached session's channel from any thread ([`HangUp`]), for a host
    /// that lets go of nvim off this thread once its own teardown in nvim is done. `None` without a
    /// running attached session: an nvim this pane started is ended through
    /// [`end_nvim`](Self::end_nvim) and [`shutdown`](Self::shutdown), never by a bare hang-up.
    pub fn hang_up_handle(&self) -> Option<HangUp> {
        match &*self.live_state.borrow() {
            LiveState::Ready(session) if session.is_attached() => {
                session.harness.hang_up_handle().map(|connection| HangUp {
                    connection,
                    detached: session.detached.clone(),
                })
            }
            _ => None,
        }
    }

    /// How the last session ended ([`PaneEnded`]), from the moment its end is reported
    /// ([`on_exited_unrequested`](Self::on_exited_unrequested)'s callback can read it) or the host's
    /// [`shutdown`](Self::shutdown) returned, until a new session starts. `None` while the session
    /// runs, or before there was one.
    pub fn ended(&self) -> Option<PaneEnded> {
        let known = self.ended.borrow().clone();
        if known.is_some() {
            return known;
        }
        match &*self.live_state.borrow() {
            LiveState::Ready(session) => record_end(session, &self.ended),
            _ => None,
        }
    }

    /// One grid cell's size in logical pixels (the unit GTK sizes and positions widgets in), or
    /// `None` before nvim is ready. For a host that moves a divider by whole cells.
    pub fn cell_size(&self) -> Option<(f64, f64)> {
        let LiveState::Ready(session) = &*self.live_state.borrow() else {
            return None;
        };
        let scale = session.harness.grid_scale();
        let factor = self.shared.device_scale();
        Some((f64::from(scale.width()) / factor, f64::from(scale.height()) / factor))
    }

    /// The share of the editor's height above its grid's first row, as last drawn: the part of a cell
    /// the height does not fill, which the snap leaves at the top (`gl_interop::snap_region_to_grid`).
    /// A host that maps nvim's screen rows to heights beside the editor (a plugin page placing the
    /// line nvim's cursor is on) takes it off. `None` before the first frame with a grid.
    pub fn grid_top_share(&self) -> Option<f64> {
        self.grid_top_share.get()
    }

    /// Registers `callback`, called with `(width, height)` in logical px -- the same numbers
    /// [`cell_size`](Self::cell_size) returns -- every time the render callback observes them
    /// differ from the last value this pane reported, first time included, so a host can size a
    /// neighbour's bottom row (the agent panel's input line) to the editor's own status line beside
    /// it. Fired from
    /// the render callback, after `render_frame`, once `live` is dropped -- the same reentrancy
    /// rule `on_fullscreen_setting`'s callback follows, since a host's handler (`shell` writing
    /// `--nv-editor-row` into the panel's theme tokens) can call straight back into this pane.
    /// Replaces any earlier callback.
    pub fn connect_cell_size_changed(&self, callback: impl Fn(f64, f64) + 'static) {
        *self.cell_size_callback.borrow_mut() = Some(Box::new(callback));
    }

    /// Registers `callback`, called each time a key the user pressed in this pane (or an input
    /// method's commit) has been handed to nvim -- a notification and nothing else: what reaches
    /// nvim is unchanged, and a key nvim could not take (not started, exited) is not reported. For
    /// a host that wants to know the user is typing (`shell` quiets the agent panel's stream
    /// while they do). Not called for [`send_keys`](Self::send_keys), a host handing a key on or
    /// driving a scratch round trip, nor for the mouse. Called from the key handler with no borrow
    /// of this pane held, so it may call back in; keep it short, it runs in the press's path.
    /// Replaces any earlier callback.
    pub fn connect_key_activity(&self, callback: impl Fn() + 'static) {
        self.key_activity.set(callback);
    }

    /// Registers a callback fired (at most once) when the tick callback observes that nvim
    /// exited on its own -- i.e. `LiveHarness::has_neovim_exited()` became true without the host
    /// ever having called `shutdown()` first (e.g. `:qa!` typed inside nvim). The reference probe
    /// handled this by calling `window.close()` directly; since this pane doesn't own a `Window`,
    /// the host registers whatever it wants done instead (closing its own window, showing a
    /// dialog, etc).
    pub fn on_exited_unrequested(&self, callback: impl Fn() + 'static) {
        *self.exited_callback.borrow_mut() = Some(Box::new(callback));
    }

    /// Registers a callback fired (at most once per session) when the fork reports nvim exited while
    /// nvim's own process is still alive -- a `nvim` launcher that does not `exec` exited without it
    /// ([`nvim_exit_pending`](Self::nvim_exit_pending)). nvim is then out of the editor's reach (the
    /// fork takes no keys for it), and its exit is reported only once its process is gone, so a
    /// host can say so. Fired from the tick, after `live` is dropped. Replaces any earlier callback.
    pub fn on_nvim_unreachable(&self, callback: impl Fn() + 'static) {
        *self.unreachable_callback.borrow_mut() = Some(Box::new(callback));
    }

    /// Registers `callback`, called once for each launch that fails, with the human-readable reason
    /// ([`build_start_failure_message`] or [`build_start_panic_message`]) if `LiveHarness::with_options`
    /// returns `Err` or panics. Errors include nvim missing from `PATH` or older than this fork's floor;
    /// panics can also come from the window system before nvim starts. Fired from the render callback,
    /// after `live` is dropped -- the same reentrancy rule [`NeovideEditorPane::on_exited_unrequested`]'s own
    /// callback follows, so a host's handler (`shell` drawing a label over this pane and moving focus
    /// elsewhere) is free to call back into this pane's own methods. Replaces any earlier callback.
    pub fn on_start_failed(&self, callback: impl Fn(&str) + 'static) {
        *self.start_failed_callback.borrow_mut() = Some(Box::new(callback));
    }

    /// Registers `callback`, called once for each nvim this pane launches, the moment its session
    /// is made ([`is_ready`](Self::is_ready) answers `true` from then on): at the first start and at
    /// every [`restart`](Self::restart), so a host can install again into a new nvim what it
    /// installed into the one before. Not called for a launch that failed
    /// ([`on_start_failed`](Self::on_start_failed)). Fired from the render callback, after `live` is
    /// dropped -- the same reentrancy rule `on_start_failed`'s callback follows. Replaces any
    /// earlier callback.
    pub fn on_session_ready(&self, callback: impl Fn() + 'static) {
        *self.session_ready_callback.borrow_mut() = Some(Box::new(callback));
    }

    /// Registers `callback`, called each time the pane followed a `:restart` or `:connect` typed in its
    /// own nvim ([`PaneFollowed`]) instead of ending: after the pane's own switch, so it already answers
    /// for the new server ([`is_attached`](Self::is_attached) is `true` after a `:connect`). A followed
    /// `:restart` is also reported to [`on_session_ready`](Self::on_session_ready)'s callback, after this
    /// one. Fired from the service, after `live` is dropped. Replaces any earlier callback.
    pub fn on_followed(&self, callback: impl Fn(PaneFollowed) + 'static) {
        *self.followed_callback.borrow_mut() = Some(Box::new(callback));
    }

    /// The server a followed `:restart` started, as it answered for itself; a clone the host may read
    /// from any thread (the window's clipboard admits it: it is no child of the window once the nvim
    /// that started it has exited).
    pub fn learned_nvim(&self) -> LearnedNvim {
        self.learned.clone()
    }

    /// Replaces the [`ChunkCheck`] that decides what [`exec_lua`](Self::exec_lua) sends
    /// ([`NeovideEditorPaneOptions::chunk_check`]); for a host whose editor became attached through a
    /// followed `:connect`, which refuses every chunk until it has one.
    pub fn set_chunk_check(&self, check: Option<ChunkCheck>) {
        self.spawn_options.borrow_mut().chunk_check = check;
    }

    /// The address of the pane's own nvim a followed `:connect` left, while that nvim still runs; `None`
    /// once it has exited (a banged `:connect`), and for every other session.
    pub fn left_running(&self) -> Option<String> {
        match &*self.live_state.borrow() {
            LiveState::Ready(session) => session
                .left
                .as_ref()
                .filter(|(process, _)| process.alive_within(Duration::ZERO) == Some(true))
                .and_then(|(_, address)| address.clone()),
            _ => None,
        }
    }

    /// Lets go of the server a followed `:connect` reached, on a thread of its own: the fork's cleanup
    /// over the fork's channel (what the fork put into that nvim is taken out), waited for at most
    /// [`FOLLOWED_CLEANUP_WITHIN`], then this UI's hang-up; `done` gets whether the cleanup said it
    /// acted. Nothing else is sent: the server goes on as it was. `false`, doing nothing, for any other
    /// session.
    pub fn detach_followed(&self, done: impl FnOnce(bool) + Send + 'static) -> bool {
        let live = self.live_state.borrow();
        let LiveState::Ready(session) = &*live else {
            return false;
        };
        if !self.followed_attach.get() || !session.is_attached() || session.harness.has_neovim_exited() {
            return false;
        }
        let (Some(nvim), Some(connection)) = (
            session.harness.neovim_handler().clone_current_neovim(),
            session.harness.hang_up_handle(),
        ) else {
            return false;
        };
        let hang_up = HangUp {
            connection,
            detached: session.detached.clone(),
        };
        run_detached(
            Box::new(move || {
                let (tx, rx) = std::sync::mpsc::channel();
                let asked = std::thread::Builder::new()
                    .name("nvim-rpc cleanup".into())
                    .spawn(move || {
                        let answer = nvim_rpc::block_on(async {
                            // `nvim_get_api_info`'s first item is the channel the request came on: the fork's.
                            let info = nvim.get_api_info().await.ok()?;
                            let chan = info.first().and_then(rmpv::Value::as_u64)?;
                            nvim.exec_lua(FOLLOWED_CLEANUP_LUA, vec![chan.into()]).await.ok()
                        });
                        let _ = tx.send(answer);
                    });
                let cleaned = asked.is_ok()
                    && matches!(
                        rx.recv_timeout(FOLLOWED_CLEANUP_WITHIN),
                        Ok(Some(rmpv::Value::Boolean(true)))
                    );
                println!(
                    "[live] letting go of the server a :connect reached: the fork's cleanup {}",
                    if cleaned { "acted" } else { "did not act in time" }
                );
                hang_up.hang_up();
                done(cleaned);
            }),
            spawn_named("followed detach"),
        );
        true
    }

    /// Asks nvim's `nvim_get_mode`, which nvim answers even while it waits for a key, on a thread of its
    /// own; `reply` gets the answer, or `None` when none came within `within` or nvim is not running.
    /// `false`, with `reply` dropped uncalled, when there is no running session.
    pub fn ask_mode(&self, within: Duration, reply: impl FnOnce(Option<NvimMode>) + Send + 'static) -> bool {
        let nvim = match &*self.live_state.borrow() {
            LiveState::Ready(session) if !session.harness.has_neovim_exited() => {
                session.harness.neovim_handler().clone_current_neovim()
            }
            _ => None,
        };
        let Some(nvim) = nvim else {
            return false;
        };
        run_detached(
            Box::new(move || {
                let (tx, rx) = std::sync::mpsc::channel();
                let asked = std::thread::Builder::new().name("nvim-rpc mode".into()).spawn(move || {
                    let _ = tx.send(nvim_rpc::block_on(nvim.get_mode()));
                });
                let mode = asked
                    .ok()
                    .and_then(|_| rx.recv_timeout(within).ok())
                    .and_then(Result::ok)
                    .map(|pairs| {
                        let field = |name: &str| pairs.iter().find(|(k, _)| k.as_str() == Some(name)).map(|(_, v)| v);
                        NvimMode {
                            at: Instant::now(),
                            mode: field("mode").and_then(|v| v.as_str()).unwrap_or_default().to_owned(),
                            blocking: field("blocking").and_then(rmpv::Value::as_bool).unwrap_or(false),
                        }
                    });
                reply(mode);
            }),
            spawn_named("nvim mode"),
        );
        true
    }

    /// After nvim quit on its own ([`NeovideEditorPane::on_exited_unrequested`]), for a host that
    /// keeps its window open without the editor (`shell`'s `prefix x`): shuts the harness down --
    /// nvim is already gone, so this does not wait for it -- drops it, and leaves the pane
    /// [`LiveState::Exited`], where every method is a no-op and nothing is sent to the closed
    /// connection, until a [`restart`](Self::restart). `false`, changing nothing, if nvim has not
    /// exited or never started.
    pub fn release_exited(&self) -> bool {
        let mut live = self.live_state.borrow_mut();
        let LiveState::Ready(session) = &mut *live else {
            return false;
        };
        if !session.harness.has_neovim_exited() {
            return false;
        }
        // Released, the pane forgets the pidfd on nvim's process: not while that process lives.
        if session.exit_pending() {
            println!("[live] release refused: nvim's process is still alive although the fork reported it exited");
            return false;
        }
        session.close_requested.set(true);
        record_end(session, &self.ended);
        // The shared event loop's fd outlives every harness, so leaving the watch is not what keeps
        // it off a closed fd: the pane leaves because nothing of it is left to wake, and the last
        // pane out removes the watch.
        live_watch::unregister(Rc::as_ptr(&session.live));
        self.tick_driver.session_ended();
        session.harness.shutdown();
        // Its shutdown pumped the shared loop, perhaps applying another pane's batches.
        live_watch::after_outside_pump(None);
        *live = LiveState::Exited;
        println!("[live] nvim exited and the host released the pane; it stays empty until a restart");
        true
    }

    /// Starts a new nvim in a pane whose nvim ended and was released ([`release_exited`]), or whose
    /// launch failed ([`on_start_failed`]), with `options` in place of the ones the pane was built
    /// with (they are kept for any later restart). The pane goes back to [`LiveState::NotStarted`] on the widget it has -- no new
    /// `GtkGLArea`, nothing reparented or unrealized -- and the next render paints the placeholder
    /// and then launches nvim, exactly as at first start, so the launch blocks this thread as that
    /// one does. The callbacks the host registered stay registered and report on the new nvim.
    /// Returns at once; a pane that is not shown starts once it is drawn.
    ///
    /// Refused, changing nothing, unless the pane is [`LiveState::Exited`] or [`LiveState::Failed`]:
    /// the error names the state it found. Refused in every state by an attached pane
    /// ([`is_attached`](Self::is_attached); [`RestartRefused::ATTACHED`]): it never starts an nvim of
    /// its own where the person's was.
    ///
    /// [`release_exited`]: Self::release_exited
    /// [`on_start_failed`]: Self::on_start_failed
    pub fn restart(&self, options: NeovideEditorPaneOptions) -> Result<(), RestartRefused> {
        if self.is_attached() {
            println!("[live] restart refused: the pane is attached to an nvim it did not start");
            return Err(RestartRefused {
                state: RestartRefused::ATTACHED,
            });
        }
        {
            let mut live = self.live_state.borrow_mut();
            if let Some(refused) = restart_refusal(&live) {
                println!("[live] restart refused: the pane is {}", refused.state);
                return Err(refused);
            }
            *self.spawn_options.borrow_mut() = options;
            *live = LiveState::NotStarted;
        }
        println!("[live] restart: the pane starts a new nvim on its own widget");
        // The tick stopped when the pane was released; the service asks for frames again while
        // the pane is not started, and the render advances the start one frame at a time.
        self.tick_driver.kick();
        self.request_frame(FrameReason::Restart);
        Ok(())
    }

    /// Shuts down the live `nvim --embed` connection, if one was ever started: the host wires this
    /// to its own window's `connect_close_request`. **Never `:qa!`** (it once ran
    /// the fork's quit, which is `:qa!`: unsaved buffers discarded, swap files deleted). Closes
    /// nvim's stdin and ends it as [`end_nvim`](Self::end_nvim) does, in the same order -- here on
    /// this thread, on the first ending's schedule if one was already under way -- so it returns
    /// once nvim's process is gone, at most the schedule's 5 s later; then tears the fork's harness down
    /// (`LiveHarness::shutdown`, which closes stdin too and sends nothing). A host asks nvim first
    /// (`shell` sends `:confirm qall` and calls this once nvim has exited), so it normally returns at
    /// once. Returns whether the fork saw nvim exit -- `true` if `LiveState` never reached `Ready`.
    ///
    /// A pane still to launch its nvim (built or restarted, `NotStarted` or `Starting`) is left
    /// [`LiveState::Exited`] instead, so no later frame launches one: a window closed while its
    /// editor restarts leaves no nvim behind it.
    ///
    /// An attached pane ([`is_attached`](Self::is_attached)) only closes its channel: nvim drops this
    /// UI and goes on, the fork's shutdown returns at the channel's end, and nothing is signalled.
    /// Whatever the host installed in that nvim is the host's to take out before this.
    pub fn shutdown(&self) -> bool {
        let mut live = self.live_state.borrow_mut();
        if matches!(&*live, LiveState::NotStarted | LiveState::Starting) {
            *live = LiveState::Exited;
            println!("[live] shutdown(): nvim had not been launched yet; the pane will not launch it");
            return true;
        }
        if let LiveState::Ready(session) = &mut *live {
            // Set before anything below so the tick callback's own guard (see
            // `LiveSession::close_requested`'s doc) is already tripped when nvim actually exits: a
            // shutdown the host itself initiated must not also reach it as an unrequested exit.
            session.close_requested.set(true);
            // A harness being torn down has nothing left for the watch to wake.
            live_watch::unregister(Rc::as_ptr(&session.live));
            self.tick_driver.session_ended();
            match session.nvim_process.clone() {
                Some(process) => {
                    let (at, schedule) = session
                        .ending
                        .get()
                        .unwrap_or_else(|| (Instant::now(), session.ending_schedule()));
                    session.ending.set(Some((at, schedule)));
                    // On this thread, blocking: `end` closes stdin itself when the schedule says, since an
                    // earlier `end_nvim`'s timer cannot run until this returns.
                    let harness = &mut session.harness;
                    let ended = nvim_child::end(&process, at, schedule, &mut || harness.hang_up());
                    println!("[live] shutdown(): nvim's stdin is closed ({ended:?}); tearing the harness down");
                }
                None => {
                    session.detach();
                    println!(
                        "[live] shutdown(): the channel to the attached nvim is closed; nvim goes on; tearing the \
                         harness down"
                    );
                }
            }
            let exited_cleanly = session.harness.shutdown();
            // Its shutdown pumped the shared loop, perhaps applying another pane's batches.
            live_watch::after_outside_pump(None);
            record_end(session, &self.ended);
            println!("[live] LiveHarness::shutdown() returned {exited_cleanly}");
            exited_cleanly
        } else {
            true
        }
    }
}

/// A pane dropped without a shutdown lets go of an attached nvim at once: its channel is closed
/// here rather than whenever the widget's handlers release the session, and a pane still to attach
/// (`NotStarted` or `Starting`) is left [`LiveState::Exited`], so the frames its widget still draws
/// never attach with no pane left to let go. Nothing else changes: an nvim the pane started is ended
/// as it always was, by its harness's own drop.
impl Drop for NeovideEditorPane {
    fn drop(&mut self) {
        let Ok(mut live) = self.live_state.try_borrow_mut() else {
            return;
        };
        match &mut *live {
            LiveState::Ready(session) => {
                if session.is_attached() && !session.harness.has_neovim_exited() {
                    session.detach();
                    println!("[live] the pane was dropped: the channel to the attached nvim is closed; nvim goes on");
                }
            }
            LiveState::NotStarted | LiveState::Starting if self.spawn_options.borrow().attach.is_some() => {
                *live = LiveState::Exited;
                println!("[live] the pane was dropped before it attached; it will not attach");
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod end_tests {
    use super::{pane_ended, Left, PaneEnded};

    #[test]
    fn an_owned_session_always_ends_as_an_exit() {
        assert_eq!(pane_ended(false, false, None), PaneEnded::Exited);
        assert_eq!(pane_ended(false, true, Some(Left::Connect)), PaneEnded::Exited);
    }

    #[test]
    fn an_attached_session_the_pane_let_go_of_ends_detached() {
        assert_eq!(pane_ended(true, true, None), PaneEnded::Detached);
    }

    #[test]
    fn an_attached_session_nvim_ended_with_nothing_reported_ends_as_an_exit() {
        assert_eq!(pane_ended(true, false, None), PaneEnded::Exited);
    }

    #[test]
    fn a_connect_or_restart_nvim_reported_is_kept() {
        assert_eq!(
            pane_ended(true, false, Some(Left::Connect)),
            PaneEnded::SessionLeft(Left::Connect)
        );
        let restart = Left::Restart {
            addr: Some("/run/o.sock".into()),
        };
        assert_eq!(
            pane_ended(true, false, Some(restart.clone())),
            PaneEnded::SessionLeft(restart)
        );
    }

    /// The fork reports a `:connect` or `:restart` only for an end nvim made before this side hung up, so a hang-up
    /// that comes after it (the host's shutdown or a worker's handle, before the pane's next service run) keeps it.
    #[test]
    fn a_hang_up_after_nvim_left_keeps_where_it_went() {
        assert_eq!(
            pane_ended(true, true, Some(Left::Connect)),
            PaneEnded::SessionLeft(Left::Connect)
        );
        let restart = Left::Restart {
            addr: Some("/run/o.sock".into()),
        };
        assert_eq!(
            pane_ended(true, true, Some(restart.clone())),
            PaneEnded::SessionLeft(restart)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{rgb_color, set_clear_color_and_request, set_focused_and_request, FrameReason, OUTSIDE_COLOR};
    use std::cell::RefCell;
    #[test]
    fn set_clear_color_requests_a_frame_through_the_funnel() {
        let clear_color = Cell::new(OUTSIDE_COLOR);
        let requests = Cell::new(0);
        set_clear_color_and_request(&clear_color, (50, 100, 150), |reason| {
            assert_eq!(reason, FrameReason::Theme);
            assert_eq!(clear_color.get(), rgb_color((50, 100, 150)));
            requests.set(requests.get() + 1);
        });
        assert_eq!(requests.get(), 1);
    }

    #[test]
    fn set_focused_requests_a_frame_through_the_funnel() {
        let focused = Cell::new(None);
        let live = RefCell::new(LiveState::NotStarted);
        let requests = Cell::new(0);
        set_focused_and_request(&focused, &live, true, |reason| {
            assert_eq!(reason, FrameReason::Focus);
            assert_eq!(focused.get(), Some(true));
            assert!(live.try_borrow_mut().is_ok());
            requests.set(requests.get() + 1);
        });
        assert_eq!(requests.get(), 1);
    }

    #[test]
    fn the_placeholder_is_the_starting_colour_when_given() {
        let c = skia_safe::Color4f::new(0.2, 0.4, 0.6, 1.0);
        assert_eq!(super::placeholder_color(Some(c)), c);
    }

    #[test]
    fn without_one_the_placeholder_is_starting_color() {
        assert_eq!(super::placeholder_color(None), super::STARTING_COLOR);
    }

    #[test]
    fn the_cover_is_the_starting_colour_only_before_nvim_is_ready() {
        let c = skia_safe::Color4f::new(0.2, 0.4, 0.6, 1.0);
        assert_eq!(super::pre_ready_cover(Some(c), false), Some(c));
        assert_eq!(super::pre_ready_cover(Some(c), true), None);
        assert_eq!(super::pre_ready_cover(None, false), None);
    }

    #[test]
    fn the_hole_cover_is_the_placeholder_until_nvim_is_ready_then_the_clear_colour() {
        let clear = skia_safe::Color4f::new(0.9, 0.8, 0.7, 1.0);
        let starting = (0x33, 0x66, 0x99);
        assert_eq!(
            super::hole_cover_color(false, clear, Some(starting)),
            super::rgb_color(starting)
        );
        assert_eq!(super::hole_cover_color(false, clear, None), super::STARTING_COLOR);
        assert_eq!(super::hole_cover_color(true, clear, Some(starting)), clear);
    }

    #[test]
    fn rgb_color_maps_bytes_to_unit_floats() {
        let c = super::rgb_color((0xfa, 0xf4, 0xed));
        assert_eq!(c.r, 250.0 / 255.0);
        assert_eq!(c.g, 244.0 / 255.0);
        assert_eq!(c.b, 237.0 / 255.0);
        assert_eq!(c.a, 1.0);
    }

    use super::{
        build_start_failure_message, build_start_panic_message, cell_size_changed, display_failure_hint, exit_events,
        exit_follows_quit, exit_log_line, failed_tick_should_render, nvim_args, os_scale_to_apply, restart_refusal,
        run_detached, sessionless_tick_should_render, spawn_named, tick_should_render, unreachable_log_line,
        watched_failure_note, with_display_hint, CallWatch, ChunkCheck, ChunkRefused, DisplayFacts, ExitEvents,
        LiveState, NeovideEditorPaneOptions, NvimStart, RemapKick, RestartRefused, ScaleWatch, NVIM_FLOOR,
        NVIM_INSTALL_URL, NVIM_VERSION_FLOOR, REMAP_RENDER_DELAY,
    };
    use gtk4::glib;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    /// A display-free check: a map asks for exactly one
    /// render, never in the same main-loop turn, and a burst of maps asks for one. What it cannot
    /// hold is the wiring (`connect_map` on a real `GLArea`) and the pixels; the GUI checklist's
    /// "edit while hidden, then show" item does.
    #[test]
    fn a_map_asks_for_one_render_after_the_delay_and_never_in_the_same_turn() {
        assert_eq!(
            REMAP_RENDER_DELAY,
            Duration::from_millis(50),
            "the measured fix (50 re-shows of 50 right); another delay, or a frame-clock tick, needs the same \
             50-cycle check first"
        );
        let _serial = crate::DEFAULT_CONTEXT_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let context = glib::MainContext::default();
        let _owner = context
            .acquire()
            .expect("tests that run the default main context take DEFAULT_CONTEXT_TEST_LOCK first");
        // Longer than the product's 50ms on purpose: "nothing in the same turn" is checked by running
        // the loop once without blocking, and a test thread descheduled for longer than the delay
        // would see the render fire there and fail for a reason that is not the code's.
        let delay = Duration::from_millis(300);
        let renders = Rc::new(Cell::new(0));
        let kick = RemapKick::new();
        let start = Instant::now();
        for _ in 0..3 {
            let renders = renders.clone();
            kick.schedule(delay, move || renders.set(renders.get() + 1));
        }
        while context.iteration(false) {}
        assert_eq!(
            renders.get(),
            0,
            "nothing in the turn the show happens in: it would coalesce into the first frame (6 of 8 right)"
        );
        while renders.get() == 0 && start.elapsed() < Duration::from_secs(2) {
            context.iteration(true);
        }
        assert_eq!(renders.get(), 1, "three maps in a row ask for one render");
        assert!(start.elapsed() >= delay);
        let after = Instant::now();
        while after.elapsed() < Duration::from_millis(150) {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(renders.get(), 1, "and nothing after it");
    }

    #[test]
    fn clean_comes_first_then_the_cursor_default_then_the_hosts_extras() {
        let hide = super::HIDE_UNFOCUSED_CURSOR_CMD;
        let extra = vec!["--cmd".to_string(), "lua print(1)".to_string()];
        assert_eq!(
            nvim_args(true, &extra, &[]),
            vec!["--clean", "--cmd", hide, "--cmd", "lua print(1)"]
        );
        assert_eq!(
            nvim_args(false, &extra, &[]),
            vec!["--cmd", hide, "--cmd", "lua print(1)"]
        );
        assert_eq!(nvim_args(true, &[], &[]), vec!["--clean", "--cmd", hide]);
        assert_eq!(nvim_args(false, &[], &[]), vec!["--cmd", hide]);
    }

    #[test]
    fn targets_follow_an_embed_the_pane_spells_and_nothing_else_changes() {
        let hide = super::HIDE_UNFOCUSED_CURSOR_CMD;
        let extra = vec!["--cmd".to_string(), "lua print(1)".to_string()];
        let open = vec!["+42".to_string(), "/a b".to_string()];
        assert_eq!(
            nvim_args(true, &extra, &open),
            vec![
                "--clean",
                "--cmd",
                hide,
                "--cmd",
                "lua print(1)",
                "--embed",
                "+42",
                "/a b"
            ]
        );
        assert_eq!(
            nvim_args(false, &[], &open),
            vec!["--cmd", hide, "--embed", "+42", "/a b"]
        );
        assert_eq!(
            nvim_args(true, &[], &["/f".to_string()]),
            vec!["--clean", "--cmd", hide, "--embed", "/f"]
        );
    }

    /// A scale-factor change alone, with no animation, content, resize or keypress, must queue
    /// a render. With every other input false, only the scale term makes `tick_should_render`
    /// true. Removing `|| scale_factor_changed_now` was checked to make this assertion fail.
    #[test]
    fn a_scale_factor_change_alone_still_queues_a_render() {
        assert!(tick_should_render(false, false, false, true));
        assert!(!tick_should_render(false, false, false, false));
    }

    /// The other three reasons to render still work on their own, unaffected by the new term --
    /// this is what would catch an `&&` typo'd in place of the intended `||`.
    #[test]
    fn each_existing_render_reason_still_works_alone() {
        assert!(tick_should_render(true, false, false, false));
        assert!(tick_should_render(false, true, false, false));
        assert!(tick_should_render(false, false, true, false));
    }

    // --- cell_size_changed fires only when the cell size changes. ---

    #[test]
    fn the_first_report_is_always_a_change() {
        assert!(cell_size_changed(None, (8.0, 17.0)));
    }

    #[test]
    fn the_same_size_again_is_not_a_change() {
        assert!(!cell_size_changed(Some((8.0, 17.0)), (8.0, 17.0)));
    }

    #[test]
    fn a_different_size_is_a_change() {
        assert!(cell_size_changed(Some((8.0, 17.0)), (8.0, 18.0)));
        assert!(cell_size_changed(Some((8.0, 17.0)), (9.0, 17.0)));
    }

    // --- ScaleWatch: the unit the tick and the Ready-construction arm each call exactly once. ---

    #[test]
    fn observe_reports_unchanged_then_changed_then_unchanged_again() {
        let watch = ScaleWatch::new(1.0);
        let first = watch.observe(1.0);
        assert!(!first.changed);
        assert_eq!(first.value, 1.0);

        let second = watch.observe(1.5);
        assert!(second.changed);
        assert_eq!(second.value, 1.5);

        let third = watch.observe(1.5);
        assert!(!third.changed, "the same value twice in a row is not a change");
    }

    /// A NaN held by nvim is one change, not one per tick: `!=` says NaN differs from itself.
    #[test]
    fn a_nan_scale_is_reported_once_not_on_every_tick() {
        let watch = ScaleWatch::new(1.0);
        assert!(watch.observe(f32::NAN).changed);
        assert!(
            !watch.observe(f32::NAN).changed,
            "the same NaN on the next tick is not a change"
        );
        assert!(watch.observe(1.0).changed);
    }

    /// `init.lua`'s own startup value must register as a change on the very first tick, the same
    /// way `last_fullscreen_setting`'s starting value does -- this is what `ScaleWatch::new`'s
    /// `initial` parameter is for.
    #[test]
    fn a_startup_value_different_from_the_initial_default_is_a_change_on_the_first_observe() {
        let watch = ScaleWatch::new(1.0);
        let first = watch.observe(1.4);
        assert!(first.changed);
        assert_eq!(first.value, 1.4);
    }

    #[test]
    fn no_pending_write_is_taken_when_none_was_buffered() {
        let watch = ScaleWatch::new(1.0);
        assert_eq!(watch.take_pending_for_ready(), None);
    }

    #[test]
    fn a_buffered_write_is_taken_exactly_once() {
        let watch = ScaleWatch::new(1.0);
        watch.buffer_pending(1.7);
        assert_eq!(watch.take_pending_for_ready(), Some(1.7));
        assert_eq!(
            watch.take_pending_for_ready(),
            None,
            "a second take must not replay the same write"
        );
    }

    /// Only the last buffered write matters, mirroring `pending_fullscreen`.
    #[test]
    fn buffering_twice_before_a_take_keeps_only_the_last_value() {
        let watch = ScaleWatch::new(1.0);
        watch.buffer_pending(1.2);
        watch.buffer_pending(1.8);
        assert_eq!(watch.take_pending_for_ready(), Some(1.8));
    }

    /// `observe` and the pending write are independent: taking the pending write must not disturb
    /// what the next `observe` compares against.
    #[test]
    fn observe_and_the_pending_write_do_not_interfere() {
        let watch = ScaleWatch::new(1.0);
        watch.buffer_pending(1.6);
        let observed = watch.observe(1.0);
        assert!(!observed.changed, "the pending write has not been applied to nvim yet");
        assert_eq!(watch.take_pending_for_ready(), Some(1.6));
    }

    // --- os_scale_to_apply follows the supplied scale only when it differs. This is the pure
    // half of `sync_os_scale`, whose real `LiveHarness` cannot be constructed in a unit test. ---

    /// Integer scales supplied by GTK apply only when the harness differs.
    #[test]
    fn os_scale_to_apply_follows_gtks_integer_factor() {
        assert_eq!(os_scale_to_apply(1.0, 2.0), Some(2.0));
        assert_eq!(os_scale_to_apply(2.0, 2.0), None);
        assert_eq!(os_scale_to_apply(2.0, 1.0), Some(1.0));
        // A widget not yet on a surface reports 0 (and never negative in practice): treat as 1, the
        // same `.max(1)` every other scale read in this crate uses.
        assert_eq!(os_scale_to_apply(1.0, 0.0), None);
        assert_eq!(os_scale_to_apply(f64::NAN, 1.0), Some(1.0));
    }

    #[test]
    fn framebuffer_resize_keeps_its_fractional_scale_before_nvim_starts() {
        let state = super::PaneShared {
            skia_state: Rc::new(RefCell::new(None)),
            live_state: Rc::new(RefCell::new(LiveState::NotStarted)),
            fb_size: Rc::new(Cell::new((0, 0))),
            device_scale: Rc::new(Cell::new(1.0)),
        };
        super::framebuffer_resized(&state, 1200, 900, 1.5);
        assert_eq!(state.device_scale(), 1.5);
        assert_eq!(state.fb_size.get(), (1200, 900));
    }

    #[test]
    fn os_scale_to_apply_keeps_a_fractional_scale() {
        assert_eq!(os_scale_to_apply(1.5, 1.5), None);
        assert_eq!(os_scale_to_apply(2.0, 1.5), Some(1.5));
        assert_eq!(os_scale_to_apply(1.0, 0.0), None);
    }

    // --- build_start_failure_message names what failed, the version floor and where to get nvim.
    // It uses the raw `anyhow` chain from `LiveHarness::with_options`. A real harness cannot be
    // constructed here, so fixtures use the two error shapes the pinned fork's bridge produces. ---

    #[test]
    fn the_floor_the_messages_name_is_the_one_the_harness_gets() {
        assert_eq!(format!("{}.{}", NVIM_FLOOR.0, NVIM_FLOOR.1), NVIM_VERSION_FLOOR);
    }

    /// The too-old shape: the fork's own sentence names the found version; the built message must
    /// name it too, plus Eitri's own floor and where to get a newer nvim.
    #[test]
    fn a_too_old_nvim_names_the_version_that_was_found() {
        // The real shape: the fork puts `:version`'s whole first line, "NVIM v0.9.5", into {found}
        // (measured in the GUI on 2026-09-27: the message read "nvim NVIM v0.9.5 is older").
        let raw = "Neovide requires nvim version 0.11.0 or higher, but NVIM v0.10.4 was detected. Download \
                    the latest version here https://github.com/neovim/neovim/wiki/Installing-Neovim";
        let message = build_start_failure_message(raw);
        assert!(message.starts_with("nvim 0.10.4 is older"), "message: {message}");
        assert!(!message.contains("NVIM"), "message: {message}");
        assert!(message.contains(NVIM_VERSION_FLOOR), "message: {message}");
        assert!(message.contains(NVIM_INSTALL_URL), "message: {message}");
    }

    /// The missing shape: no version is named at all (this is what `.context("Could not locate or
    /// start neovim process")`'s own chain looks like), so nothing to extract -- the message still
    /// names the floor and where to get a newer nvim, and keeps the raw detail for anyone who wants
    /// it.
    #[test]
    fn a_missing_nvim_still_names_the_floor_and_where_to_get_one() {
        let raw = "Could not locate or start neovim process: No such file or directory (os error 2)";
        let message = build_start_failure_message(raw);
        assert!(message.contains(NVIM_VERSION_FLOOR), "message: {message}");
        assert!(message.contains(NVIM_INSTALL_URL), "message: {message}");
        assert!(message.contains(raw), "message: {message}");
    }

    #[test]
    fn a_start_panic_keeps_its_detail_without_nvim_version_advice() {
        let cases: [(Box<dyn std::any::Any + Send>, &str); 3] = [
            (
                Box::new("event loop unavailable"),
                "the editor could not start: event loop unavailable",
            ),
            (
                Box::new(String::from("no display")),
                "the editor could not start: no display",
            ),
            (Box::new(42_u32), "the editor could not start: unknown panic"),
        ];
        for (payload, expected) in cases {
            assert_eq!(build_start_panic_message(payload.as_ref()), expected);
        }
    }

    // --- the red page names the display problem in plain words, with the raw error under it.
    const NO_COMPOSITOR: &str =
        "the editor could not start: Failed to create winit event loop: Os(OsError { line: 89, \
         file: \"winit-0.30.13/src/platform_impl/linux/wayland/event_loop/mod.rs\", error: \
         WaylandError(Connection(NoCompositor)) })";

    #[test]
    fn a_dead_wayland_display_is_explained_with_both_sides_named() {
        let facts = DisplayFacts {
            window_on_wayland: false,
            wayland_display: Some("wayland-0".into()),
            x_display: Some(":0".into()),
            gdk_backend: None,
        };
        let page = with_display_hint(NO_COMPOSITOR.to_string(), &facts);
        let (hint, raw) = page.split_once("\n\n").unwrap();
        assert!(hint.contains("could not connect to a display server"), "{hint}");
        assert!(hint.contains("Wayland display \"wayland-0\""), "{hint}");
        assert!(hint.contains("the window is on X11"), "{hint}");
        assert!(hint.contains("tmux"), "{hint}");
        assert!(hint.contains("wayland-*"), "{hint}");
        assert!(!hint.contains("GDK_BACKEND"), "{hint}");
        assert_eq!(raw, NO_COMPOSITOR, "the raw error stays under the explanation");
    }

    #[test]
    fn a_window_forced_onto_x11_while_the_editor_follows_wayland_says_so() {
        let facts = DisplayFacts {
            window_on_wayland: false,
            wayland_display: Some("wayland-1".into()),
            x_display: Some(":0".into()),
            gdk_backend: Some("x11".into()),
        };
        let hint = display_failure_hint(NO_COMPOSITOR, &facts).unwrap();
        assert!(hint.contains("GDK_BACKEND=x11"), "{hint}");
        let on_wayland = DisplayFacts {
            window_on_wayland: true,
            ..facts
        };
        let hint = display_failure_hint(NO_COMPOSITOR, &on_wayland).unwrap();
        assert!(hint.contains("the window is on Wayland"), "{hint}");
        assert!(!hint.contains("GDK_BACKEND"), "{hint}");
    }

    #[test]
    fn an_x11_failure_and_a_missing_display_get_their_own_words() {
        let facts = DisplayFacts {
            x_display: Some(":7".into()),
            ..Default::default()
        };
        let x11 = "the editor could not start: Failed to create winit event loop: X11 error: could not open display";
        let hint = display_failure_hint(x11, &facts).unwrap();
        assert!(hint.contains("X11 display \":7\""), "{hint}");
        let none = "the editor could not start: neither WAYLAND_DISPLAY nor WAYLAND_SOCKET nor DISPLAY is set";
        let hint = display_failure_hint(none, &DisplayFacts::default()).unwrap();
        assert!(hint.contains("Neither WAYLAND_DISPLAY nor DISPLAY is set"), "{hint}");
    }

    #[test]
    fn other_start_failures_are_left_as_they_were() {
        let facts = DisplayFacts::default();
        for message in [
            "the editor could not start: unknown panic",
            "nvim could not be started (Eitri needs 0.11 or newer): No such file or directory",
        ] {
            assert_eq!(with_display_hint(message.to_string(), &facts), message);
        }
    }

    // --- failed_tick_should_render renders the failure once and stops. The pure decision for
    // LiveState::Failed is split out because a unit test cannot construct a real GLArea tick. ---

    #[test]
    fn the_tick_still_renders_once_more_right_after_a_start_failure() {
        assert!(failed_tick_should_render(false));
    }

    #[test]
    fn the_tick_reports_idle_once_the_failed_frame_has_been_painted() {
        assert!(!failed_tick_should_render(true));
    }

    /// An exit is reported only once nvim's own
    /// process is gone, exactly once however often the tick asks; while the fork says exited and
    /// the process lives, the host is told once that nvim is out of reach; with no process known,
    /// the fork's report is all there is.
    #[test]
    fn an_exit_is_reported_once_nvims_process_is_gone_and_only_once() {
        let nothing = ExitEvents::default();
        assert_eq!(exit_events(false, None, false, false, false), nothing, "running");
        assert_eq!(exit_events(false, Some(true), false, false, false), nothing);
        assert_eq!(
            exit_events(true, Some(true), false, false, false),
            nothing,
            "a process going a moment after its pipe is said nothing of"
        );
        let unreachable = ExitEvents {
            unreachable: true,
            exited: false,
        };
        assert_eq!(
            exit_events(true, Some(true), true, false, false),
            unreachable,
            "still running after the grace: the launcher's exit"
        );
        assert_eq!(exit_events(true, Some(true), true, true, false), nothing, "said once");
        let exited = ExitEvents {
            unreachable: false,
            exited: true,
        };
        assert_eq!(
            exit_events(true, Some(false), false, false, false),
            exited,
            "gone within the grace: an exit, with nothing said before it"
        );
        assert_eq!(
            exit_events(true, Some(false), true, true, false),
            exited,
            "then its own exit"
        );
        assert_eq!(
            exit_events(true, Some(false), true, true, true),
            nothing,
            "reported once"
        );
        assert_eq!(
            exit_events(true, None, false, false, false),
            exited,
            "nothing known: the fork's word"
        );
    }

    #[test]
    fn only_nvim_behind_a_launcher_is_said_to_have_been_left_by_it() {
        assert!(unreachable_log_line(NvimStart::Launcher).contains("(its launcher exited without it)"));
        let direct = unreachable_log_line(NvimStart::Direct);
        assert!(!direct.contains("launcher"), "{direct}");
        let restarted = unreachable_log_line(NvimStart::Restart);
        assert!(
            restarted.contains("(the server nvim's :restart started)"),
            "{restarted}"
        );
        assert!(
            !restarted.contains("launcher"),
            "a restarted server was left by no launcher: {restarted}"
        );
        assert!(direct.contains("500 ms later"), "{direct}");
    }

    #[test]
    fn an_exit_after_an_unanswered_quit_is_the_quit_s_and_is_logged_quietly() {
        use std::time::Instant;
        let watch = |done, returned| CallWatch {
            sent_at: Instant::now(),
            done,
            returned,
            last_answer: None,
        };
        assert!(!exit_follows_quit(None), "no quit was asked: nvim left by itself");
        assert!(exit_follows_quit(Some(&watch(false, false))), "the quit is still out");
        assert!(
            exit_follows_quit(Some(&watch(true, false))),
            "the quit ended with nvim going away"
        );
        assert!(
            !exit_follows_quit(Some(&watch(true, true))),
            "nvim answered, so it declined the quit; a later exit is its own"
        );
        let asked = exit_log_line(true);
        let alone = exit_log_line(false);
        assert!(!asked.contains("on its own"), "{asked}");
        assert!(alone.contains("nvim exited on its own"), "{alone}");
    }

    #[test]
    fn a_watched_request_that_lost_its_connection_says_nothing_but_an_nvim_error_does() {
        use nvim_rs::error::CallError;
        let eof = || std::io::Error::from(std::io::ErrorKind::UnexpectedEof);
        let closed = [
            CallError::DecodeError(
                std::sync::Arc::new(nvim_rs::error::DecodeError::ReaderError(eof())),
                "nvim_exec_lua".into(),
            ),
            CallError::SendError(nvim_rs::error::EncodeError::WriterError(eof()), "nvim_exec_lua".into()),
        ];
        for lost in &closed {
            assert_eq!(watched_failure_note(lost), None, "{lost}");
        }
        // A message that would not decode is not a connection that ended.
        let garbled = CallError::DecodeError(
            std::sync::Arc::new(nvim_rs::error::DecodeError::InvalidMessage(
                nvim_rs::error::InvalidMessage::UnknownMessageType(9),
            )),
            "nvim_exec_lua".into(),
        );
        let note = watched_failure_note(&garbled).expect("a message that did not decode is said");
        assert!(note.contains("nvim_exec_lua"), "{note}");
        let refused = CallError::NeovimError(Some(1), "E5108: Lua error".into());
        let note = watched_failure_note(&refused).expect("nvim's own error is said");
        assert!(note.contains("E5108"), "{note}");
    }

    /// A thread that cannot be started (`EAGAIN` under a process limit,
    /// reproduced under that limit) must not leave nvim's ending unscheduled -- the pane had marked
    /// it hung up, so every later close returned early and the window waited forever. The job then
    /// runs here, inline.
    #[test]
    fn a_job_whose_thread_cannot_start_runs_inline() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let ran = Arc::new(AtomicBool::new(false));
        let job = {
            let ran = ran.clone();
            Box::new(move || ran.store(true, Ordering::SeqCst))
        };
        let detached = run_detached(job, |_job| Err(std::io::Error::from_raw_os_error(libc::EAGAIN)));
        assert!(!detached);
        assert!(ran.load(Ordering::SeqCst), "the job ran inline");

        let ran = Arc::new(AtomicBool::new(false));
        let job = {
            let ran = ran.clone();
            Box::new(move || ran.store(true, Ordering::SeqCst))
        };
        assert!(run_detached(job, spawn_named("test job")));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ran.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "the thread ran it");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The tick's own decision for every state without a live session, so
    /// the `Failed` arm is pinned where the tick reads it -- after a start failure the render loop
    /// asks for the one frame that paints the message, then reports idle.
    #[test]
    fn after_a_start_failure_the_render_loop_goes_idle() {
        let failed = LiveState::Failed("nvim could not start".into());
        assert!(
            sessionless_tick_should_render(&failed, false),
            "the frame that paints it"
        );
        assert!(!sessionless_tick_should_render(&failed, true), "then idle");
        assert!(
            sessionless_tick_should_render(&LiveState::NotStarted, true),
            "starting still advances"
        );
        assert!(sessionless_tick_should_render(&LiveState::Starting, true));
        assert!(
            !sessionless_tick_should_render(&LiveState::Exited, false),
            "a released pane never draws"
        );
    }

    /// Only a released pane or one whose launch failed may start a new nvim: any other state has,
    /// or is about to have, a harness, and the refusal says which state it found.
    #[test]
    fn only_an_exited_or_failed_pane_may_restart() {
        assert_eq!(restart_refusal(&LiveState::Exited), None);
        assert_eq!(
            restart_refusal(&LiveState::Failed("nvim could not start".into())),
            None,
            "a failed launch made no harness, so it can be tried again"
        );
        for (state, name) in [(LiveState::NotStarted, "NotStarted"), (LiveState::Starting, "Starting")] {
            assert_eq!(restart_refusal(&state), Some(RestartRefused { state: name }));
        }
        let refused = RestartRefused { state: "Ready" };
        assert!(
            refused.to_string().contains("Ready"),
            "the message names the state: {refused}"
        );
    }

    #[test]
    fn an_attached_panes_refusal_says_it_is_attached() {
        let refused = RestartRefused {
            state: RestartRefused::ATTACHED,
        };
        let text = refused.to_string();
        assert!(text.contains("attached"), "the message names the attach: {text}");
        assert!(
            !text.contains("released"),
            "an attached pane has no state it may restart from: {text}"
        );
    }

    #[test]
    fn a_chunk_check_is_equal_only_to_itself_and_stays_so_in_a_copy_of_the_options() {
        let refuse = |_: &'static str, _: Vec<rmpv::Value>| {
            Err(ChunkRefused {
                reason: "refused".to_owned(),
            })
        };
        let check = ChunkCheck::new(refuse);
        let options = NeovideEditorPaneOptions {
            chunk_check: Some(check.clone()),
            ..Default::default()
        };
        assert_eq!(options.clone(), options, "a copy for a restart keeps the same check");
        assert_ne!(
            check,
            ChunkCheck::new(refuse),
            "another check, even of the same code, is another one"
        );
        assert_eq!(
            check.run("return 1", vec![]),
            Err(ChunkRefused {
                reason: "refused".to_owned()
            })
        );
        let append = ChunkCheck::new(|_, mut args| {
            args.push(rmpv::Value::from(7));
            Ok(args)
        });
        assert_eq!(
            append.run("return ...", vec![rmpv::Value::from("a")]),
            Ok(vec![rmpv::Value::from("a"), rmpv::Value::from(7)])
        );
    }
}
