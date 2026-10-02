//! neovide-editor: the embeddable Neovide/Skia/GTK4 editor surface, extracted from the
//! validated poc/neovide_embed_live probe. See docs/superpowers/specs/2026-09-06-shell-scaffolding-design.md.
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
pub mod fd_watch;
mod frame_clock;
mod gl_interop;
mod keyboard;
mod mouse;
mod nvim_child;
mod nvim_rpc;
mod stdin;
mod tick_driver;

/// Unit tests that acquire the default main context take this first: two at once would make the
/// second `acquire` fail.
#[cfg(test)]
pub(crate) static DEFAULT_CONTEXT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub use editor_area::PresentationCounts;
pub use nvim_rpc::{CallWatch, NvimMode};

/// What [`NeovideEditorPane::exec_lua`] calls with nvim's answer, on the call's own thread: the
/// value, or the error text (nvim's, or why the connection could not answer). It must not touch
/// anything that belongs to the GTK thread.
pub type LuaReply = Box<dyn FnOnce(Result<rmpv::Value, String>) + Send + 'static>;
pub use stdin::{detach_stdin_from_nvim, ForwardedStdin};

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    EventControllerMotion, EventControllerScroll, EventControllerScrollFlags, GLArea, GestureClick, IMMulticontext,
};

use skia_safe::gpu::direct_contexts;
use skia_safe::{BlendMode, Color4f, Paint, SamplingOptions};

use neovide::live_harness::{LiveHarness, LiveHarnessOptions};
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

/// Log a frame-pacing line every N frames instead of spamming stdout every frame.
const LOG_EVERY_N_FRAMES: u64 = 60;

/// How many service runs (tick callback or fd watch) between `[tick]` summary log lines (see `TickStats`).
const TICK_LOG_EVERY_N_TICKS: u64 = 300;

/// The clear colour painted across the whole framebuffer before each `LiveHarness::render_frame`
/// call (`canvas.clear(OUTSIDE_COLOR)` in the render callback below) -- a cheap defensive clear,
/// not a colour a host can override any more (see the removal note below).
///
/// **2026-09-07:** this used to be a loud, deliberately-wrong magenta (`(0.55, 0.15, 0.55)`) so a
/// viewport-clear regression bleeding past `CONTENT_MARGIN` would be visible at a glance during
/// P0-P5's validation. That validation is done (P5: real screenshots at 6 scale factors, numeric
/// geometry cross-checks) and this pane is now embedded in the real `shell` product, where a
/// bright magenta strip around every real window reads as a bug rather than a debug aid -- a real
/// user asked "what is that purple border, can it go away" the first time they saw it. Now a
/// neutral near-black that blends with typical dark UI chrome.
///
/// **2026-09-16, established with a positive control, not just argued:** `CONTENT_MARGIN` is 0.0,
/// so there is no inset band left for this to paint, and **no pixel on screen shows it at all.**
/// `compute_content_region` returns the whole framebuffer at margin 0, and every arm of the render
/// callback repaints that entire rect on top of this clear:
///
/// - `Ready` hands `content_region` to the fork's `Renderer::draw_frame`, which `clip_rect`s to it
///   and *then* does `root_canvas.clear(default_background)` -- an `SkBlendMode::kSrc` fill of the
///   whole clip with nvim's own `Normal` background (`src/renderer/mod.rs:272-283` at the pinned
///   rev, fork commit `09b304a`, "only clear the renderer's own clipped viewport").
/// - `NotStarted`/`Starting`/`Failed` fill the same rect with `STARTING_COLOR`/`FAILED_COLOR`,
///   both opaque.
///
/// A GUI pass proved this with a positive control rather than assuming it from the code alone: a
/// magenta probe at margin 0 produced 0 pixels anywhere, including across 220 frames of a live
/// divider drag, while the identical probe at `CONTENT_MARGIN = 40` produced exactly the 40px
/// frame's own pixel count. A host-settable clear colour (`set_clear_color`) existed for one
/// release so a host could track nvim's own background in case this ever became visible again; it
/// was removed once that control confirmed the colour reaches no pixel today, and the sub-cell
/// remainder a floored `grid_size_for_content_region` leaves at the right and bottom edge lies
/// *inside* `content_region` and has always been painted by the renderer in nvim's own background,
/// not this colour. Removing the margin is what fixed the near-black frame on its own, and the
/// value also matches nothing any more: `#1a1b1f` was the background of the placeholder palette
/// `shell`'s theme pipeline replaced. If `CONTENT_MARGIN` ever becomes non-zero again, this
/// constant -- and a reason to bring back a host-settable override -- becomes reachable once more.
///
/// **2026-09-19: reachable again, exactly as the paragraph above predicted, and now only the
/// STARTING value.** `snap_region_to_grid` moves the grid's sub-cell vertical remainder to the top
/// edge so the bottom of the pane sits flush against the host's status bar, which means
/// `content_region` no longer covers the whole framebuffer and these pixels are on screen -- a thin
/// band under the top bar. A fixed near-black there would reinstate the near-black frame the margin
/// removal fixed, so [`NeovideEditorPane::set_clear_color`] is back, and `shell` drives it from the
/// same `tokens.bg` its theme pipeline already pushes at every colorscheme change. This value is
/// what the band shows for the ~190ms before the first theme payload arrives, and until a host sets
/// it at all.
const OUTSIDE_COLOR: Color4f = Color4f::new(0.102, 0.106, 0.122, 1.0);
/// Painted across the whole `content_region` while `LiveHarness` is being constructed (the one
/// real, observed blocking call in this crate) -- distinct from both `OUTSIDE_COLOR` and anything
/// the real renderer would draw, so it's obvious on screen which phase is showing.
const STARTING_COLOR: Color4f = Color4f::new(0.12, 0.12, 0.16, 1.0);
/// Painted across `content_region` if `LiveHarness::with_options` itself returned `Err` (e.g. no
/// `nvim` on `$PATH`) -- distinct from every other state color here.
const FAILED_COLOR: Color4f = Color4f::new(0.5, 0.05, 0.05, 1.0);

/// Lifecycle of the `LiveHarness` this pane drives, kept explicit (rather than a bare
/// `Option<LiveHarness>`) so the render callback can paint a "starting nvim..." placeholder frame
/// *before* the one real, observed blocking call in this crate (`LiveHarness::with_options`) runs.
/// `NotStarted` -> one placeholder frame painted+presented -> `Starting` -> next render callback
/// performs the blocking construction -> `Ready`/`Failed`.
pub enum LiveState {
    NotStarted,
    Starting,
    Ready(Box<LiveSession>),
    Failed(String),
    /// nvim quit and the host let this pane go ([`NeovideEditorPane::release_exited`]): the harness
    /// is shut down and dropped, so nothing -- a focus report, a resize, a key -- can reach its
    /// closed command channel (`send_ui` panics on one). Never left: a second `nvim --embed` cannot
    /// be started in this process, because `LiveHarness` builds a winit `EventLoop` and winit allows
    /// one per process (`EventLoopError::RecreationAttempt`).
    Exited,
}

/// `(fb_w, fb_h, cell_w bits, cell_h bits, cols, rows)`: what `LiveSession::log_layout_if_changed`
/// compares to decide whether the layout changed. Named for `clippy::type_complexity`.
type LayoutLogKey = (i32, i32, u32, u32, u32, u32);

pub struct LiveSession {
    pub(crate) harness: LiveHarness,
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
    /// the P2 "dead-looking-alive window" bugfix, adapted for a pane that doesn't own a `Window`:
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
    /// `LiveHarness::fullscreen_setting()` as the tick callback last read it, so the host's
    /// `on_fullscreen_setting` callback fires on a change and not on every tick. Starts at
    /// Neovide's own default (`false`), so a value `init.lua` set is reported on the first tick.
    last_fullscreen_setting: Cell<bool>,
    /// The pane's one framebuffer size (device pixels), as GTK last reported it through
    /// `GLArea::resize` -- shared with the render callback, the resize handler and the tick, so
    /// the mouse handlers hit-test against exactly the size the frame was drawn at
    /// (`gl_interop::current_content_region`). Wave 4; see `gl_interop::GridLayout`.
    pub(crate) fb_size: Rc<Cell<(i32, i32)>>,
    /// `(fb_w, fb_h, cell_w bits, cell_h bits, cols, rows)` as the `[layout]` line last printed
    /// it, so the line is printed when the layout changes and never per frame. Bits, not f32, so
    /// a NaN cell before nvim reports a font does not count as a change every frame.
    last_logged_layout: Cell<Option<LayoutLogKey>>,
    /// The `nvim --embed` process itself (`nvim_child`'s module doc): what a shutdown that timed out
    /// ends, and what [`NeovideEditorPane::end_nvim`] ends. Holds what the fork started from the
    /// launch on, and nvim's own `getpid()` answer once it comes back ([`ask_nvim_for_its_pid`]);
    /// shared with the thread that asks.
    nvim_process: Arc<nvim_child::NvimProcess>,
    /// The last [`NeovideEditorPane::exec_lua_watched`] request and its watch (`nvim_rpc`).
    watched: Option<nvim_rpc::WatchedCall>,
    /// When the ending started, and on which schedule ([`NeovideEditorPane::end_nvim`]), so a
    /// second ending -- the window's shutdown after it -- keeps to the first one's schedule and
    /// starts no second watch.
    ending: Cell<Option<(Instant, nvim_child::Schedule)>>,
    /// Whether [`NeovideEditorPane::on_nvim_unreachable`]'s callback has fired for this session.
    unreachable_said: Cell<bool>,
    /// Which session this is ([`NeovideEditorPane::session_serial`]), never reused in the process.
    serial: u64,
}

/// The serial the next session takes. Process-wide, so the render callback that builds a session
/// needs no counter handed to it, and no two sessions of any pane share a number.
static NEXT_SESSION_SERIAL: AtomicU64 = AtomicU64::new(1);

impl Drop for LiveSession {
    fn drop(&mut self) {
        // Before the harness's fields drop: a watch on a closed (or reused) fd number is a bug.
        self.wants_frame.release_watch();
    }
}

impl LiveSession {
    fn new(
        harness: LiveHarness,
        grid_size: GridSize<u32>,
        fb_size: Rc<Cell<(i32, i32)>>,
        nvim_process: Arc<nvim_child::NvimProcess>,
        driver: &Rc<TickDriver>,
    ) -> Self {
        // A new harness: the driver's fd watch starts afresh on its fd.
        driver.session_started();
        let now = Instant::now();
        Self {
            harness,
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
            last_fullscreen_setting: Cell::new(false),
            fb_size,
            last_logged_layout: Cell::new(None),
            nvim_process,
            watched: None,
            ending: Cell::new(None),
            unreachable_said: Cell::new(false),
            serial: NEXT_SESSION_SERIAL.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// The fork reports nvim exited, and nvim's own process is still alive. The fork's report
    /// follows the process IT started: a `nvim` launcher that does not `exec` exits -- or crashes
    /// -- and 500 ms later the fork says nvim exited (`bridge::run` waits that long for the IO
    /// stream, then reports regardless), while nvim runs on, re-parented, perhaps still writing a
    /// file. The fork takes no keys for it from then on. Round 4: this is not an exit, and nothing
    /// that follows an exit -- the window's close, the editor's retirement -- happens until the
    /// process is gone ([`exit_events`]). `false` when no process is known to be nvim: then the
    /// fork's report is all there is.
    fn exit_pending(&self) -> bool {
        self.harness.has_neovim_exited() && self.nvim_process.alive_within(Duration::ZERO) == Some(true)
    }

    /// The schedule an ending starting now takes (`nvim_child::schedule`): nvim may be in a prompt
    /// when its `:confirm qall` request -- the only prompt this pane knows of -- has not come back.
    /// Without a process known to be nvim there is nothing to signal, and the hang-up comes first.
    fn ending_schedule(&self) -> nvim_child::Schedule {
        let quit_outstanding = self.watched.as_ref().is_some_and(|call| !call.watch().done);
        nvim_child::schedule(quit_outstanding && self.nvim_process.nvim_pid().is_some())
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

/// Counts, over rolling windows of `TICK_LOG_EVERY_N_TICKS` service runs, how many issued a
/// `queue_render()` vs. how many skipped it because nothing needed another frame. A service run
/// comes from one of two drivers (`ServiceSource`): the tick callback (registered only while a
/// frame is wanted) or the event-loop fd watch (one run per wake-up while idle, so it counts on
/// its own and is never mixed into the tick's numbers). An idle pane shows `skipped` dominating; a
/// typing/scrolling/animating pane shows `issued` dominating. See
/// `poc/p11_measurements/PHASE_REPORT.md` for the bug this directly addresses.
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

/// The embeddable Neovide/Skia editor surface. Wraps a single `GtkGLArea` driving a real
/// `nvim --embed` connection (`neovide::live_harness::LiveHarness`), with resize/IME/keyboard/
/// mouse/tick/render callbacks all wired up by `new()`. Deliberately does not own an
/// `Application`/`ApplicationWindow` -- see this module's own doc for why -- so the host must
/// call `.widget()` to place this pane's `GLArea` into its own window, `.grab_focus()` once that
/// window is shown, and wire its own window's `connect_close_request` to `.shutdown()`.
pub struct NeovideEditorPane {
    widget: GLArea,
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
    /// See [`NeovideEditorPane::on_start_failed`].
    start_failed_callback: StartFailedCallbackSlot,
    /// See [`NeovideEditorPane::connect_key_activity`].
    key_activity: keyboard::KeyActivity,
    /// The tick callback's registration and the event-loop fd watch (`tick_driver`).
    tick_driver: Rc<TickDriver>,
}

/// Construction-time options for [`NeovideEditorPane::with_options`]. Each field is forwarded to
/// the `LiveHarnessOptions` field of the same name, so this crate stays a pass-through rather than
/// growing a `with_this_and_that` constructor per combination a host happens to need.
#[derive(Default)]
pub struct NeovideEditorPaneOptions {
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
}

/// The `--cmd` that makes the editor draw **no cursor** while it does not have the keys.
///
/// `g:neovide_cursor_unfocused_outline_width` is Neovide's own setting (default 1/8 em, a hollow
/// block). The fork (`0862c35`) makes a width `<= 0` mean "draw nothing while unfocused, any
/// shape" -- stock Neovide at width 0 still leaves an anti-aliased sliver at fractional cell sizes
/// (measured: 33 pixels at scale 1.5). The owner asked for no cursor at all, 2026-09-19.
///
/// It is a `--cmd`, so it runs before the user's own config: a user who wants the hollow block
/// back sets the variable in their `init.lua` (e.g. `vim.g.neovide_cursor_unfocused_outline_width
/// = 0.125`) and wins. Neovide reads a `--cmd` value once, in `settings.read_initial_values`, BEFORE
/// `ui_attach`; `init.lua` runs AT `ui_attach`, so its value, like a runtime `:let`, reaches the
/// renderer by the other path -- the `WatchGlobal` dict watcher and `setting_changed`. Both paths are
/// proved on real frames by `tests/unfocused_cursor.rs`: a later `--cmd`, and a `:let` typed after
/// nvim is ready. A real `init.lua` itself was not loaded (the test runs `--clean`); the runtime
/// `:let` stands in for it because it takes the same watcher path.
pub const HIDE_UNFOCUSED_CURSOR_CMD: &str = "let g:neovide_cursor_unfocused_outline_width = 0";

/// The whole argument list this pane hands to `LiveHarnessOptions::extra_nvim_args`: `--clean`
/// if asked for, then this pane's own default (`HIDE_UNFOCUSED_CURSOR_CMD`), then the host's
/// extras -- so a host's `--cmd` can override the default too.
fn nvim_args(clean: bool, extra: &[String]) -> Vec<String> {
    let mut args = Vec::with_capacity(extra.len() + 3);
    if clean {
        args.push("--clean".to_string());
    }
    args.push("--cmd".to_string());
    args.push(HIDE_UNFOCUSED_CURSOR_CMD.to_string());
    args.extend(extra.iter().cloned());
    args
}

/// How long after this pane's `GLArea` is mapped it asks for one more frame (modules design §5,
/// S3). 50ms is the measured fix: 50 re-shows of 50 right. A render asked for in the same main-loop
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
/// that frame shows the cursor and nothing else, until nvim's next redraw (S3: 39 re-shows of 50,
/// and 0 of 8 when only the cursor had moved). `GtkPaned` never showed it: its `set_visible(true)`
/// forces a resize, and ~14 renders follow. Which half draws the window-less frame, Neovide's
/// renderer or GTK's texture handling after a re-map, was not isolated (modules spec §14).
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

/// Whether `connect_cell_size_changed` (wave 4, R5) should fire this frame: `None` (nvim has never
/// reported a cell yet) always counts as a change, same as `ScaleWatch`'s own first-observe rule;
/// otherwise a plain inequality. Split out of the render callback -- which borrows `GLArea`/
/// `LiveSession` types no unit test can construct -- for the same reason `tick_should_render` is.
fn cell_size_changed(last: Option<(f64, f64)>, now: (f64, f64)) -> bool {
    last != Some(now)
}

/// The nvim version floor this pane's pinned fork enforces (`NEOVIM_REQUIRED_VERSION` in
/// `bridge/mod.rs`) -- restated here because the fork's own too-old error names it as bare numbers
/// with no context of what *this project* needs, and R1-4 wants the pane's own message to say so
/// plainly rather than making the host go read the fork's source.
const NVIM_VERSION_FLOOR: &str = "0.10";

/// Where to get a newer nvim -- the same URL the fork's own too-old error already links
/// (`bridge/mod.rs:197`), restated here so the missing-nvim case (whose error text never names a
/// URL at all) gets one too.
const NVIM_INSTALL_URL: &str = "https://github.com/neovim/neovim/wiki/Installing-Neovim";

/// R1-4: the message [`NeovideEditorPane::on_start_failed`] hands the host and [`LiveState::Failed`]
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

/// What the tick reports about nvim's end (round 4): the fork's own report is `fork_exited`, and
/// `nvim_alive` is what the pidfd on nvim's process says (`None`: no process is known to be nvim).
/// An exit is reported only once nvim's process is gone -- or, with none known, on the fork's word
/// -- so what a host does on it (close the window, retire the editor) never runs while an nvim we
/// started may still be writing, and it is reported once however often the tick asks (`said`, the
/// pane's latches). While the fork says exited and the process lives, the host is told once that
/// nvim is out of reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ExitEvents {
    unreachable: bool,
    exited: bool,
}

fn exit_events(fork_exited: bool, nvim_alive: Option<bool>, unreachable_said: bool, exit_said: bool) -> ExitEvents {
    if !fork_exited {
        return ExitEvents::default();
    }
    match nvim_alive {
        Some(true) => ExitEvents {
            unreachable: !unreachable_said,
            exited: false,
        },
        _ => ExitEvents {
            unreachable: false,
            exited: !exit_said,
        },
    }
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
/// limit) -- here, inline, so what it does is never simply dropped (round 5, codex finding 2: the
/// ending of nvim was, and every later close then waited for it forever). `true` when it went to a
/// thread. A failed `Builder::spawn` consumes its closure, so the job travels in a slot both sides
/// can take from.
fn run_detached(job: Job, spawn: impl FnOnce(Job) -> std::io::Result<()>) -> bool {
    let slot = Arc::new(std::sync::Mutex::new(Some(job)));
    let theirs = slot.clone();
    let spawned = spawn(Box::new(move || {
        let job = theirs.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(job) = job {
            job();
        }
    }));
    match spawned {
        Ok(()) => true,
        Err(err) => {
            eprintln!("[live] could not start a thread ({err}); running its job here instead");
            let job = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(job) = job {
                job();
            }
            false
        }
    }
}

/// Asks nvim, over the fork's own connection and on a thread of its own, for its pid (`getpid()`),
/// and holds the answer in `process` -- the process that answers on nvim's pipe is nvim, whatever
/// launched it (codex finding C2, the Opus review's T6-2: a `nvim` wrapper that does not `exec`
/// is the fork's direct child, and a kill of that left the hung nvim running). A thread, not the
/// GTK thread: nvim answers requests only once its loop runs, which is after the user's whole
/// config has been sourced -- and never, for an nvim that hung in it (then `process` keeps only
/// what the fork started, `nvim_child`'s source 2). The request is never abandoned: nvim-rs ends
/// the whole connection when a response arrives for a dropped request (`nvim_rpc`'s module doc),
/// and nvim exiting resolves it with an error.
fn ask_nvim_for_its_pid(harness: &LiveHarness, process: &Arc<nvim_child::NvimProcess>) {
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
                Some(pid) if process.learn(pid) => println!("[live] nvim is pid {pid} (its own answer)"),
                Some(pid) => println!("[live] nvim answered pid {pid}, which is not a live nvim --embed; not held"),
                None => {
                    println!("[live] nvim did not answer getpid(); a forced kill falls back to what the fork started")
                }
            }
        });
    if let Err(err) = spawned {
        eprintln!("[live] could not start the thread that asks nvim for its pid ({err})");
    }
}

/// Whether the tick callback should ask for one more frame while [`LiveState::Failed`]: exactly
/// once -- the frame that actually paints [`FAILED_COLOR`] and the message. R1-4's "a Failed pane
/// should render once and stop", replacing the old unconditional `true` (`queue_render()` at display
/// refresh rate for the rest of the window's life, since `Failed` never leaves itself). Split out for
/// the same reason `tick_should_render` is.
fn failed_tick_should_render(already_painted: bool) -> bool {
    !already_painted
}

/// Whether the tick callback asks for a frame for a pane with no live session -- every state but
/// [`LiveState::Ready`], which the tick decides itself ([`tick_should_render`]). Split out so the
/// `Failed` arm is a test's (the Opus review's T7-2: `Failed(_) => true` in the tick left the whole
/// suite green, since only [`failed_tick_should_render`] was tested).
fn sessionless_tick_should_render(state: &LiveState, failed_painted: bool) -> bool {
    match state {
        // The placeholder-frame dance and the one blocking `LiveHarness::with_options` call both
        // happen *inside* the render callback and only run when a render is actually requested --
        // keep rendering here so that state machine can advance.
        LiveState::NotStarted | LiveState::Starting => true,
        // R1-4: Failed is a terminal state that paints the same fill and message every time, so
        // unlike NotStarted/Starting it does NOT keep asking for frames forever -- only until the
        // render callback has actually painted it once (the `Failed` arm there sets
        // `failed_painted`). Before R1-4 the pane asked for a frame every tick for the rest of the
        // window's life.
        LiveState::Failed(_) => failed_tick_should_render(failed_painted),
        // Nothing will ever change: no frame is needed.
        LiveState::Exited => false,
        // Not sessionless: never asked here (the tick's own `Ready` arm decides).
        LiveState::Ready(_) => true,
    }
}

/// The OS-scale value [`sync_os_scale`] should hand `LiveHarness::set_os_scale_factor`, given GTK's
/// raw `Widget::scale_factor()` right now -- or `None` if `current` (the harness's own
/// `os_scale_factor()`) already agrees with it (v1 P2, S1/S2). Pure and split out for the same
/// reason `tick_should_render`/`cell_size_changed` are: no unit test here can construct a real
/// `LiveHarness`.
///
/// `widget_scale_factor` is GTK's own rounded-up integer (S1: the same basis every hit-test in this
/// crate already uses -- `pixel_to_grid_pos`, `cell_size`, the tick's own `widget_size` computation
/// -- never `Surface::scale()`'s fraction). A widget not yet realized on a surface reports `0` (and
/// never negative in practice), so this treats anything below `1` as `1`, matching every other
/// `.max(1)` in this crate. Compared against `current` by bits, exactly like
/// `LiveHarness::set_os_scale_factor` compares internally, so this and that call always agree on
/// whether anything actually changed.
fn os_scale_to_apply(current: f64, widget_scale_factor: i32) -> Option<f64> {
    let target = f64::from(widget_scale_factor.max(1));
    if target.to_bits() == current.to_bits() {
        None
    } else {
        Some(target)
    }
}

/// Applies GTK's current `Widget::scale_factor()` to `harness` when it disagrees with the harness's
/// own record ([`os_scale_to_apply`]) -- S1/S2's whole implementation. Called from the two sites S2
/// requires, `connect_resize`'s `Ready` branch and the render callback's `Ready` arm, both
/// **before** any grid is computed from `harness.grid_scale()`: `set_os_scale_factor` moves the
/// renderer's cell size synchronously, so calling this before the grid read means a scale change and
/// the framebuffer resize that always accompanies it (GTK reallocates `GtkGLArea`'s buffers and
/// fires `resize` on its next snapshot) land in the very same GTK callback, and no tick ever computes
/// a grid from a cell size and a framebuffer that belong to different scales (see `gl_interop`'s
/// `snap_tests::a_scale_change_applied_with_its_framebuffer_resizes_nothing` for the invariant this
/// keeps, and its counter-example for the transient grid it prevents).
///
/// Returns exactly what `LiveHarness::set_os_scale_factor` returns: `true` only on a real, applied
/// change. Never called from `connect_scale_factor_notify` itself (S2) -- that handler only asks for
/// a frame; this runs once GTK actually hands over the matching new framebuffer. Prints one
/// `[hidpi]` line on an applied change, for a GUI pass to read; the common case (nothing changed
/// this frame) costs one bit comparison and prints nothing (P11).
fn sync_os_scale(harness: &mut LiveHarness, widget_scale_factor: i32) -> bool {
    let Some(new_scale) = os_scale_to_apply(harness.os_scale_factor(), widget_scale_factor) else {
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
/// review's mutation testing -- a literal `false` in place of the real signal, the callback
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
    pub fn set_clear_color(&self, (r, g, b): (u8, u8, u8)) {
        self.clear_color.set(Color4f::new(
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
            1.0,
        ));
        self.widget.queue_render();
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
        let NeovideEditorPaneOptions {
            clean,
            child_env,
            cwd,
            extra_nvim_args,
        } = options;
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
        let tick_driver: Rc<TickDriver> = Rc::new(TickDriver::default());
        let clear_color = Rc::new(Cell::new(OUTSIDE_COLOR));
        let focused: Rc<Cell<Option<bool>>> = Rc::new(Cell::new(None));
        let exited_callback: ExitedCallbackSlot = Rc::new(RefCell::new(None));
        let unreachable_callback: UnreachableCallbackSlot = Rc::new(RefCell::new(None));
        let fullscreen_callback: FullscreenCallbackSlot = Rc::new(RefCell::new(None));
        let pending_fullscreen: Rc<Cell<Option<bool>>> = Rc::new(Cell::new(None));
        let scale_factor_callback: ScaleFactorCallbackSlot = Rc::new(RefCell::new(None));
        let scale_watch: Rc<ScaleWatch> = Rc::new(ScaleWatch::new(1.0));
        let cell_size_callback: CellSizeCallbackSlot = Rc::new(RefCell::new(None));
        let start_failed_callback: StartFailedCallbackSlot = Rc::new(RefCell::new(None));
        // R1-4: set once, the first time the `Failed` arm below actually paints `FAILED_COLOR` and
        // the message -- read by the tick callback ([`failed_tick_should_render`]) to stop asking for
        // more frames once that one paint has happened. `Failed` never leaves itself, so this is
        // never reset.
        let failed_painted: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        // Only the render closure below ever reads this -- not stored on `Self`, unlike
        // `cell_size_callback`, which a host registers *after* construction.
        let last_cell_size: Rc<Cell<Option<(f64, f64)>>> = Rc::new(Cell::new(None));
        // The one framebuffer size (device pixels) the grid, the snap, the tick's resync and hit
        // testing all derive from -- GTK's own, as `GLArea::resize` reports it. Wave 4 (owner:
        // "neovim最下面的空白应该填在上面"); see `gl_interop::GridLayout`.
        let fb_size: Rc<Cell<(i32, i32)>> = Rc::new(Cell::new((0, 0)));

        // GtkWidget::unrealize is RUN_LAST: this normal handler runs before GtkGLArea deletes
        // its framebuffer and clears its GL context. Keep the nvim session across re-realization,
        // but let the next render create fresh Skia state for the new context.
        {
            let skia_state = skia_state.clone();
            gl_area.connect_unrealize(move |widget| {
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
        // (device pixels, not logical widget units). Also the P2 frozen-scroll-bug fix:
        // recompute the grid size that actually fits the new content_region and, if it differs
        // (in grid *cells*, not raw pixels) from what nvim was last told, call
        // `LiveHarness::resize_grid` so nvim's own viewport tracks the real host size.
        {
            let skia_state = skia_state.clone();
            let live_state = live_state.clone();
            let fb_size = fb_size.clone();
            gl_area.connect_resize(move |widget, width, height| {
                // Outside the `Ready` check, so a resize before nvim exists is kept too.
                fb_size.set((width, height));
                let mut live = live_state.borrow_mut();
                let (forced_next_frame, resized_grid) = if let LiveState::Ready(session) = &mut *live {
                    session.wants_frame.set(true);
                    // S2: before any grid is computed from `grid_scale()`, so a scale change and
                    // the framebuffer this very `resize` signal hands over always land together.
                    sync_os_scale(&mut session.harness, widget.scale_factor());
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
                    "[resize] fb={}x{}px scale_factor={} logical={}x{} forced_next_frame={} \
                     resized_grid={:?}",
                    width,
                    height,
                    widget.scale_factor(),
                    widget.width(),
                    widget.height(),
                    forced_next_frame,
                    resized_grid.map(|g| (g.width, g.height)),
                );
                let mut state = skia_state.borrow_mut();
                match state.as_mut() {
                    Some(state) => {
                        state.fb_width = width;
                        state.fb_height = height;
                        // Force rebuild against GTK's resized framebuffer.
                        state.gtk_surface = None;
                        // The render target is reallocated only if its pixel size changed. GTK can
                        // emit resize after an allocation whose dimensions stayed exactly the same.
                    }
                    None => {
                        // GrContext not created yet; render() will pick up the GLArea's current
                        // size directly.
                    }
                }
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
                    widget.queue_render();
                });
            });
        }

        // --- scale-factor notify (v1 P2, D1/S2): `GtkGLArea` reallocates its own buffers and fires
        // `resize` (with the new framebuffer) only on its NEXT snapshot after this notify fires --
        // and this pane only ever renders on change (P11) -- so without asking for a frame here the
        // old frame stays upscaled indefinitely (measured: `s1-after-scale2.png`, `s1.log` has the
        // notify line and no `[resize]` for 4s). This handler does exactly that and nothing more: it
        // never touches the harness or `fb_size` itself. The actual scale sync (`sync_os_scale`) runs
        // in `connect_resize` and the render callback below, where GTK has handed over the matching
        // new framebuffer in the very same snapshot -- never here, which is why the notify handler
        // stays this small (S2's "never at notify time").
        {
            let live_state = live_state.clone();
            gl_area.connect_scale_factor_notify(move |widget| {
                // `try_borrow`, not `borrow`: a GTK signal can fire while a render or tick closure
                // already holds `live_state` borrowed (the `run_tab_verb` panic of panel round 2 is
                // the precedent this guards against).
                if let Ok(live) = live_state.try_borrow() {
                    if let LiveState::Ready(session) = &*live {
                        session.wants_frame.set(true);
                    }
                }
                widget.queue_render();
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
                let gl_area = gl_area.clone();
                motion.connect_motion(move |controller, x, y| {
                    mouse::handle_mouse_motion(&live_state, &gl_area, controller, x, y);
                });
            }
            {
                let live_state = live_state.clone();
                let gl_area = gl_area.clone();
                motion.connect_enter(move |controller, x, y| {
                    mouse::handle_mouse_motion(&live_state, &gl_area, controller, x, y);
                });
            }
            gl_area.add_controller(motion);
        }

        {
            let scroll = EventControllerScroll::new(EventControllerScrollFlags::BOTH_AXES);
            let live_state = live_state.clone();
            let gl_area_for_scroll = gl_area.clone();
            scroll.connect_scroll(move |controller, dx, dy| {
                mouse::handle_mouse_scroll(&live_state, &gl_area_for_scroll, controller, dx, dy)
            });
            gl_area.add_controller(scroll);
        }

        // --- render: build (lazily) and drive one LiveHarness frame every tick. See the
        // `LiveState` doc for why construction is deliberately split across two render callbacks
        // instead of happening inline here.
        {
            let skia_state = skia_state.clone();
            let live_state = live_state.clone();
            // `child_env` and `cwd` are moved into this closure by the `move` below -- neither is
            // needed again in this function, and this closure is the only place `LiveHarness` is
            // ever constructed.
            let clear_color = clear_color.clone();
            let focused = focused.clone();
            let pending_fullscreen = pending_fullscreen.clone();
            let scale_watch_for_ready = scale_watch.clone();
            let scale_watch_for_render = scale_watch.clone();
            let tick_driver_for_render = tick_driver.clone();
            let fb_size = fb_size.clone();
            let cell_size_callback_for_render = cell_size_callback.clone();
            let last_cell_size = last_cell_size.clone();
            let start_failed_callback_for_render = start_failed_callback.clone();
            let failed_painted_for_render = failed_painted.clone();
            gl_area.connect_render(move |widget, _gl_ctx| {
                let mut state_slot = skia_state.borrow_mut();

                if state_slot.is_none() {
                    let interface = make_gl_interface();
                    let gr_context =
                        direct_contexts::make_gl(interface, None).expect("failed to create Skia GL DirectContext");
                    let width = widget.width() * widget.scale_factor();
                    let height = widget.height() * widget.scale_factor();
                    println!(
                        "[init] Skia DirectContext created; initial fb={}x{}px scale_factor={}",
                        width,
                        height,
                        widget.scale_factor()
                    );
                    *state_slot = Some(SkiaState::new(gr_context, width, height));
                    // The same computation GTK 4.22.5 uses for its own buffers and `resize`
                    // signal (`gtkglarea.c` `gtk_gl_area_snapshot`), so this agrees with the
                    // `resize` that normally arrived first; it covers one that has not.
                    fb_size.set((width, height));
                }

                let state = state_slot.as_mut().unwrap();
                // Must come before anything draws this frame: GTK shares this GL context with us
                // and rebinds textures behind Skia's back (on a resize in particular), which makes
                // every glyph paint as a solid block until Skia is told to distrust its state
                // cache. See `SkiaState::invalidate_cached_gl_state` for the full root-cause
                // record and the measurements behind doing this every frame rather than only on
                // resize frames.
                state.invalidate_cached_gl_state();
                // `EditorGlArea`'s own tiled buffer is bound (`editor_area`): Skia draws straight
                // into it, with no intermediate and no copy. Otherwise `GtkGLArea`'s.
                let own_buffer = widget
                    .downcast_ref::<editor_area::EditorGlArea>()
                    .is_some_and(|area| area.draws_into_own_buffer());
                state.ensure_gtk_surface(own_buffer);
                state.ensure_render_surface(own_buffer);

                let Some(gtk_surface) = state.gtk_surface.as_mut() else {
                    return glib::Propagation::Stop;
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
                // R1-4: set from inside the `Starting` arm's `Err` case below, fired only *after*
                // `live` is dropped -- same reentrancy rule. Unlike `cell_size_changed_to` this fires
                // at most once per pane, since `Starting` -> `Failed` happens exactly once.
                let mut start_failed_message: Option<String> = None;
                // Set in the `Ready`, `Failed` and `Exited` arms, acted on after `live` is dropped:
                // whether the tick callback is still wanted once this frame is committed (F2/F3).
                let mut tick_wanted_after_render: Option<bool> = None;

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
                        fill_content_region(canvas, &content_region, STARTING_COLOR);
                        *live = LiveState::Starting;
                    }
                    LiveState::Starting => {
                        fill_content_region(canvas, &content_region, STARTING_COLOR);

                        let os_scale_factor = widget.scale_factor() as f64;
                        let options = LiveHarnessOptions {
                            os_scale_factor,
                            extra_nvim_args: nvim_args(clean, &extra_nvim_args),
                            // Cloned rather than moved because this closure is `Fn` and runs on
                            // every frame -- but this arm is the one-shot construction pass, so
                            // the clone happens exactly once per pane.
                            child_env: child_env.clone(),
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
                            ..Default::default()
                        };
                        println!(
                            "[live] constructing LiveHarness::with_options(os_scale_factor={os_scale_factor}, \
                             clean={clean}, cwd={cwd:?}) -- this performs a real, synchronous nvim \
                             launch and WILL block the GTK main loop until it returns"
                        );
                        // Before the launch, so the one new `--embed` child after it is nvim.
                        let children_before = nvim_child::direct_children();
                        let t0 = Instant::now();
                        match LiveHarness::with_options(options) {
                            Ok(mut harness) => {
                                let elapsed = t0.elapsed();
                                println!(
                                    "[live] LiveHarness::with_options returned after {elapsed:?} \
                                     (blocked the GTK main loop for that long)"
                                );
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
                                ask_nvim_for_its_pid(&harness, &nvim_process);
                                // The P2 frozen-scroll-bug fix: `LiveHarnessOptions::grid_size`
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
                                    nvim_process,
                                    &tick_driver_for_render,
                                )));
                            }
                            Err(err) => {
                                let elapsed = t0.elapsed();
                                let raw = format!("{err:#}");
                                println!("[live] LiveHarness::with_options failed after {elapsed:?}: {raw}");
                                // R1-4: the raw chain above is what stdout gets (unchanged); the
                                // pane's own state, and what a host is handed, is the built message
                                // -- what failed, the version floor, where to get a newer nvim.
                                let message = build_start_failure_message(&raw);
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
                        let dt =
                            session
                                .animation_clock
                                .advance(frame_time, refresh_interval, session.last_animating.get());
                        // S2: covers a re-realized widget, or any GTK whose snapshot does not
                        // re-emit `resize` -- `connect_resize` above is the OTHER S2 call site, and
                        // between the two nothing computes a grid from a cell size and a framebuffer
                        // that belong to different scales. Unlike `connect_resize`, nothing else on
                        // this path already sets `wants_frame` for a plain scale change, so fold the
                        // "did this frame actually apply one" result in here.
                        if sync_os_scale(&mut session.harness, widget.scale_factor()) {
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
                        let animating = session.harness.render_frame(canvas, Some(&layout.region), dt);
                        if !same_grid_scale(snapped_scale, session.harness.grid_scale()) {
                            // `render_frame` pumps nvim and applies `g:neovide_scale_factor` (and
                            // any `guifont` change) BEFORE it reads `grid_scale` (fork `5997cef`,
                            // `live_harness.rs` `render_frame`), so this frame was snapped for the
                            // old cell: its remainder sits at the top for the old height and the
                            // new rows leave a band at the bottom. Nothing else asks for a frame
                            // once the pane is idle, so ask for one here; it re-snaps, and the
                            // tick's resync fixes the row count. The second frame sees the scale
                            // already applied, so this does not loop. eitri-only.
                            session.wants_frame.set(true);
                        }
                        // Wave 4, R5: the editor's own cell height is what `shell` puts into
                        // `--nv-editor-row` so the agent panel's status band stops growing past it.
                        // Computed exactly as `cell_size()` does (same `grid_scale`/`scale_factor`
                        // pair), fired only when it actually differs from the last value this pane
                        // reported -- see `cell_size_changed`'s own doc for why `None` always
                        // counts as a change.
                        let current_scale = session.harness.grid_scale();
                        let factor = f64::from(widget.scale_factor().max(1));
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
                                != session.last_fullscreen_setting.get(),
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
                        // R1-4: this is the one paint `sessionless_tick_should_render` is waiting
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
                state.gr_context.flush_and_submit();

                glib::Propagation::Stop
            });
        }

        // --- drive redraws off the display's frame clock, tying frame pacing to actual
        // vsync-reported timing rather than a fixed timer -- but, per the P11 fix, only actually
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
            let unreachable_callback_for_tick = unreachable_callback.clone();
            // Fired after `live` is dropped, for the same reentrancy reason as the exited callback:
            // the host's handler calls straight back into `set_fullscreen_setting`.
            let fullscreen_callback_for_tick = fullscreen_callback.clone();
            // Same reentrancy reason again: the host's handler (`shell`'s pending/echo tracking)
            // calls straight back into `set_scale_factor_setting`.
            let scale_factor_callback_for_tick = scale_factor_callback.clone();
            let scale_watch_for_tick = scale_watch.clone();
            let tick_stats = Rc::new(TickStats::new());
            let fb_size = fb_size.clone();
            // One-shot: `[layout] note:` is printed at most once per pane.
            let layout_note_logged = Cell::new(false);
            // R1-4: read (never written) here -- the render callback's `Failed` arm is the only
            // writer. See `failed_tick_should_render`.
            let failed_painted_for_tick = failed_painted.clone();
            let driver_for_service = Rc::downgrade(&tick_driver);
            let service: Service = Rc::new(move |widget: &GLArea, source: ServiceSource| {
                let mut live = live_state.borrow_mut();
                // Set from inside the `Ready` arm below, then acted on only *after* `live` is
                // dropped -- see the comment on `exited_callback_for_tick` above for why the
                // ordering matters.
                let mut should_fire_exited_callback = false;
                let mut should_fire_unreachable_callback = false;
                let mut fullscreen_changed: Option<bool> = None;
                let mut scale_factor_changed: Option<f32> = None;
                let issued = match &mut *live {
                    LiveState::Ready(session) => {
                        // Cheap, non-blocking drain of any nvim redraw traffic that arrived since
                        // the last tick.
                        session.harness.pump(Duration::ZERO);
                        let fullscreen = session.harness.fullscreen_setting();
                        if session.last_fullscreen_setting.replace(fullscreen) != fullscreen {
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
                        // Wave 4: the size is GTK's own framebuffer (`fb_size`, set by
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
                        // v1 P2: a live OS-scale change is exactly this kind of mismatch between GTK's
                        // `notify::scale-factor` -- which moves `widget.scale_factor()` while the
                        // harness is still at the old scale and `fb_size` still holds the old
                        // framebuffer -- and the next snapshot, whose `resize` sets `fb_size` and then
                        // syncs the harness in the same callback (so the harness never runs ahead of
                        // `fb_size`). The gap lasts one frame on a drawn widget and longer while the
                        // pane is hidden. Expected, not the anomaly this note exists for: the added
                        // `os_scale_factor()` comparison below skips the note while it is open.
                        let (width, height) = fb_size.get();
                        let widget_size = (
                            widget.width() * widget.scale_factor(),
                            widget.height() * widget.scale_factor(),
                        );
                        if !layout_note_logged.get()
                            && widget.is_drawable()
                            && width > 0
                            && height > 0
                            && widget_size.0 > 0
                            && widget_size.1 > 0
                            && widget_size != (width, height)
                            && session.harness.os_scale_factor() == f64::from(widget.scale_factor().max(1))
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

                        // The P2 "dead-looking-alive pane" bugfix: `shutdown()` already calls
                        // `LiveHarness::shutdown()` when the host closes its own window, but
                        // nothing previously reacted when nvim exits *on its own* (e.g. `:qa!`
                        // typed inside it). `pump()` just above is what actually observes a real
                        // `UserEvent::NeovimExited` arriving asynchronously, so checking right
                        // here, on every service run (the tick, or the fd watch for a hidden or unrealized pane), catches it at the same latency the frame log already
                        // did. `close_requested.replace(true)` is the one-shot guard documented on
                        // `LiveSession::close_requested`; only the tick that flips it false->true
                        // actually fires the callback.
                        //
                        // Round 4: only once nvim's own process is gone (`exit_events`).
                        let fork_exited = session.harness.has_neovim_exited();
                        let events = exit_events(
                            fork_exited,
                            fork_exited
                                .then(|| session.nvim_process.alive_within(Duration::ZERO))
                                .flatten(),
                            session.unreachable_said.get(),
                            session.close_requested.get(),
                        );
                        if events.unreachable {
                            session.unreachable_said.set(true);
                            should_fire_unreachable_callback = true;
                            println!(
                                "[live] the fork reports nvim exited, and nvim's own process is still alive \
                                 (its launcher exited without it); its exit is reported once it is gone"
                            );
                        }
                        if events.exited {
                            session.close_requested.set(true);
                            should_fire_exited_callback = true;
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
                    println!(
                        "[live] nvim exited on its own (not via a caller-initiated shutdown) -- \
                         invoking the registered on_exited_unrequested callback (if any) instead \
                         of closing a window this pane doesn't own"
                    );
                    if let Some(cb) = exited_callback_for_tick.borrow().as_ref() {
                        cb();
                    }
                }

                if issued {
                    widget.queue_render();
                }
                tick_stats.record(source, issued);

                // Whether another tick cycle is wanted, read after every callback above (one may
                // have asked for a frame), and the fd watch once a harness exists. Pending service
                // work is zero here: this run just consumed it.
                let (need, fd) = match &*live_state.borrow() {
                    LiveState::Ready(session) => (
                        tick_still_wanted(TickInputs {
                            animating: session.last_animating.get(),
                            wants_frame: session.wants_frame.get(),
                            exit_pending: session.harness.has_neovim_exited() && !session.close_requested.get(),
                            service_pending: false,
                        }),
                        Some(session.harness.event_loop_fd()),
                    ),
                    other => (
                        sessionless_tick_should_render(other, failed_painted_for_tick.get()),
                        None,
                    ),
                };
                if let (Some(fd), Some(driver)) = (fd, driver_for_service.upgrade()) {
                    driver.watch_fd(fd);
                }
                need
            });
            tick_driver.attach(&gl_area, service);
            tick_driver.kick();
        }

        Self {
            widget: gl_area,
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
            start_failed_callback,
            key_activity,
            tick_driver,
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
            .is_some_and(|area| area.draws_into_own_buffer())
    }

    /// How many frames went through each presentation path, and how often the own-buffer path
    /// failed, since this pane was built -- across every realize, never reset. Diagnostic: the
    /// real-framebuffer regression fails a run that expects the own buffers on any fallback.
    pub fn presentation_counts(&self) -> PresentationCounts {
        self.widget
            .downcast_ref::<editor_area::EditorGlArea>()
            .map(|area| area.presentation_counts())
            .unwrap_or_default()
    }

    /// Grabs keyboard focus for this pane's `GLArea` and tells the IME it's now the focused
    /// widget -- call this once the host's window is actually shown (mirroring the reference
    /// probe's own `window.present(); gl_area.grab_focus(); im_context.focus_in();` sequence,
    /// minus the `.present()`, which is the host's own window's job).
    /// Tells the editor whether it has the keyboard, so it can say so through its cursor: a solid
    /// block when it has focus, **no cursor at all** when it does not (`HIDE_UNFOCUSED_CURSOR_CMD`;
    /// a user who sets the width back gets upstream's hollow block). It forwards to
    /// `LiveHarness::set_focused` (fork `8f043a2`), which also tells nvim through
    /// `nvim_ui_set_focus`. Before this, nvim never heard about focus at all, and nothing fired
    /// `FocusGained` or `FocusLost`.
    ///
    /// This is a separate call from [`grab_focus`](Self::grab_focus), and the host decides what
    /// "focused" means. `shell` passes `true` only while this pane holds the window's focus widget
    /// AND the window is active, so alt-tabbing away hides the cursor too. It may be called before nvim has started, and on every focus event:
    /// repeats are dropped by the harness.
    pub fn set_focused(&self, focused: bool) {
        self.focused.set(Some(focused));
        if let LiveState::Ready(session) = &mut *self.live_state.borrow_mut() {
            session.harness.set_focused(focused);
            session.wants_frame.set(true);
        }
        self.widget.queue_render();
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
            println!("[live] send_keys({keys:?}) before nvim is ready -- dropped");
        }
    }

    /// Whether nvim is up, so a host can refuse a request `send_keys` would drop (phase 3: the
    /// scratch round trips answer "the editor is not ready yet" instead of doing nothing).
    pub fn is_ready(&self) -> bool {
        matches!(&*self.live_state.borrow(), LiveState::Ready(_))
    }

    /// Whether nvim is up and has not exited, as of the last service run's pump (tick or fd watch). Unlike
    /// [`is_ready`](Self::is_ready), which stays `true` from an unrequested exit until the host
    /// [`release_exited`](Self::release_exited) the pane: a host deciding whether nvim can still be
    /// asked something (a window close's `:confirm qall`) asks this.
    pub fn is_running(&self) -> bool {
        matches!(&*self.live_state.borrow(), LiveState::Ready(session) if !session.harness.has_neovim_exited())
    }

    /// Whether nvim ran and has exited: seen exited and not yet released, or released
    /// ([`LiveState::Exited`]). `false` for an nvim that never started, and -- round 3 -- for one
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
    /// made from a thread of its own, returning at once. Until the request comes back, a second thread
    /// asks nvim's fast `nvim_get_mode` every 250 ms, so a host can tell an nvim showing a dialog from
    /// one whose loop is stuck ([`watched_call`](Self::watched_call)). Replaces the previous watch
    /// (whose request still runs to its end). `false`, sending nothing, when nvim is not running.
    pub fn exec_lua_watched(&self, code: &str) -> bool {
        let mut live = self.live_state.borrow_mut();
        let LiveState::Ready(session) = &mut *live else {
            return false;
        };
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
            move || async move {
                match nvim.exec_lua(&code, vec![]).await {
                    Ok(_) => {
                        println!("[live] exec_lua_watched: the Lua returned");
                        true
                    }
                    Err(err) => {
                        println!("[live] exec_lua_watched: no return (nvim exited?): {err}");
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
                    field("blocking").and_then(|v| v.as_bool()).unwrap_or(false),
                ))
            }
        };
        match nvim_rpc::WatchedCall::start("exec_lua", call, probe) {
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

    /// `nvim_exec_lua(code, args)` on a thread of its own; `reply` runs on that thread once nvim
    /// answered (or the connection ended). `false`, `reply` dropped uncalled, when nvim is not
    /// running or the thread could not start.
    ///
    /// The thread lives until nvim answers, which may be long: nvim queues a request while it
    /// waits for a key, and answers after the key. So a caller keeps at most one call outstanding
    /// for each thing it asks. Unlike [`exec_lua_watched`](Self::exec_lua_watched) this keeps no
    /// record in the session and starts no watch: the quit and its watch stay the window's own.
    pub fn exec_lua(&self, code: &'static str, args: Vec<rmpv::Value>, reply: LuaReply) -> bool {
        let nvim = {
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
            nvim
        };
        let call = async move { nvim.exec_lua(code, args).await.map_err(|e| e.to_string()) };
        match nvim_rpc::spawn_answered("exec_lua", call, reply) {
            Ok(()) => true,
            Err(err) => {
                eprintln!("[live] exec_lua: could not start its thread ({err}) -- nothing sent");
                false
            }
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
    /// waited for up to [`SETTLE_WAIT`] (round 3, codex finding 2): nvim-rs hands the answer to the
    /// worker through a oneshot, and nvim's exit reaches the host without waiting for it. For a
    /// host deciding, at nvim's exit, whether nvim had answered its quit.
    pub fn settled_watched_call(&self) -> Option<CallWatch> {
        match &*self.live_state.borrow() {
            LiveState::Ready(session) => session.watched.as_ref().map(|call| call.settled(SETTLE_WAIT)),
            _ => None,
        }
    }

    /// Ends nvim without `:qa!` (the round-4 ruling): closes its stdin (the fork's
    /// `LiveHarness::hang_up`), on which nvim exits keeping its swap files, and escalates on a thread
    /// by the pid this pane holds (`nvim_child::end`: SIGTERM, which nvim answers the same way and
    /// which ends a dialog EOF does not; SIGKILL after 5 s) -- never the launcher that started it
    /// (`nvim_child`'s module doc). The order is `nvim_child::schedule`'s: stdin closed now and
    /// SIGTERM from 1 s; or, while this pane's `:confirm qall` has not come back and nvim may be in
    /// its prompt, SIGTERM now and stdin closed at 1 s, by a timer on this thread, since the
    /// harness lives here (the quit follow-up). Its exit then reaches the host as any exit does,
    /// once the process is gone ([`on_exited_unrequested`](Self::on_exited_unrequested)).
    /// Idempotent: a second call starts no second watch. `false`, doing nothing, when there is no
    /// session.
    pub fn end_nvim(&self) -> bool {
        let LiveState::Ready(session) = &mut *self.live_state.borrow_mut() else {
            return false;
        };
        if session.ending.get().is_some() {
            return true;
        }
        let at = Instant::now();
        let schedule = session.ending_schedule();
        session.ending.set(Some((at, schedule)));
        let watched = session.nvim_process.nvim_pid();
        if schedule.hang_up_after.is_zero() {
            session.harness.hang_up();
            println!("[live] end_nvim: nvim's stdin is closed; watching pid {watched:?} until it has gone");
        } else {
            let live = Rc::downgrade(&self.live_state);
            glib::timeout_add_local_once(schedule.hang_up_after, move || {
                let Some(live) = live.upgrade() else { return };
                let mut live = live.borrow_mut();
                if let LiveState::Ready(session) = &mut *live {
                    // Idempotent, and harmless once nvim has gone.
                    session.harness.hang_up();
                    println!("[live] end_nvim: nvim's stdin is closed");
                }
            });
            println!(
                "[live] end_nvim: nvim may be in its prompt: SIGTERM first, stdin closed in {:?}; watching pid \
                 {watched:?} until it has gone",
                schedule.hang_up_after
            );
        }
        let process = session.nvim_process.clone();
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

    /// One grid cell's size in logical pixels (the unit GTK sizes and positions widgets in), or
    /// `None` before nvim is ready. For a host that moves a divider by whole cells.
    pub fn cell_size(&self) -> Option<(f64, f64)> {
        let LiveState::Ready(session) = &*self.live_state.borrow() else {
            return None;
        };
        let scale = session.harness.grid_scale();
        let factor = f64::from(self.widget.scale_factor().max(1));
        Some((f64::from(scale.width()) / factor, f64::from(scale.height()) / factor))
    }

    /// Registers `callback`, called with `(width, height)` in logical px -- the same numbers
    /// [`cell_size`](Self::cell_size) returns -- every time the render callback observes them
    /// differ from the last value this pane reported, first time included (wave 4, R5: "agent
    /// pane最下面的input >> auto那一行太宽了，最好做到和旁边neovim底下的status一样宽"). Fired from
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

    /// R1-4: registers `callback`, called at most once, with the human-readable reason
    /// ([`build_start_failure_message`]) if `LiveHarness::with_options` ever returns `Err` -- nvim
    /// missing from `PATH`, or older than this fork's floor. Before this there was no way for a host
    /// to learn *why* the pane went [`LiveState::Failed`] at all; only a `[live] ... failed` line on
    /// stdout, which a launch from a menu never shows. Fired from the render callback, after `live`
    /// is dropped -- the same reentrancy rule [`NeovideEditorPane::on_exited_unrequested`]'s own
    /// callback follows, so a host's handler (`shell` drawing a label over this pane and moving focus
    /// elsewhere) is free to call back into this pane's own methods. Replaces any earlier callback.
    pub fn on_start_failed(&self, callback: impl Fn(&str) + 'static) {
        *self.start_failed_callback.borrow_mut() = Some(Box::new(callback));
    }

    /// After nvim quit on its own ([`NeovideEditorPane::on_exited_unrequested`]), for a host that
    /// keeps its window open without the editor (`shell`'s `prefix x`): shuts the harness down --
    /// nvim is already gone, so this does not wait for it -- drops it, and leaves the pane
    /// [`LiveState::Exited`], where every method is a no-op and nothing is sent to the closed
    /// connection. `false`, changing nothing, if nvim has not exited or never started.
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
        // The harness (and its event loop's fd) is dropped just below: no watch may outlive it.
        self.tick_driver.unwatch_fd();
        session.harness.shutdown();
        *live = LiveState::Exited;
        println!("[live] nvim exited and the host released the pane; it stays empty");
        true
    }

    /// Shuts down the live `nvim --embed` connection, if one was ever started: the host wires this
    /// to its own window's `connect_close_request`. **Never `:qa!`** (round 4; until then this ran
    /// the fork's quit, which is `:qa!`: unsaved buffers discarded, swap files deleted). Closes
    /// nvim's stdin and ends it as [`end_nvim`](Self::end_nvim) does, in the same order -- here on
    /// this thread, on the first ending's schedule if one was already under way -- so it returns
    /// once nvim's process is gone, at most the schedule's 5 s later; then tears the fork's harness down
    /// (`LiveHarness::shutdown`, which closes stdin too and sends nothing). A host asks nvim first
    /// (`shell` sends `:confirm qall` and calls this once nvim has exited), so it normally returns at
    /// once. Returns whether the fork saw nvim exit -- `true` if `LiveState` never reached `Ready`.
    pub fn shutdown(&self) -> bool {
        let mut live = self.live_state.borrow_mut();
        if let LiveState::Ready(session) = &mut *live {
            // Set before anything below so the tick callback's own guard (see
            // `LiveSession::close_requested`'s doc) is already tripped when nvim actually exits: a
            // shutdown the host itself initiated must not also reach it as an unrequested exit.
            session.close_requested.set(true);
            let (at, schedule) = session
                .ending
                .get()
                .unwrap_or_else(|| (Instant::now(), session.ending_schedule()));
            session.ending.set(Some((at, schedule)));
            // On this thread, blocking: `end` closes stdin itself when the schedule says, since an
            // earlier `end_nvim`'s timer cannot run until this returns.
            // No watch on the fd of a harness that is being torn down.
            self.tick_driver.unwatch_fd();
            let harness = &mut session.harness;
            let ended = nvim_child::end(&session.nvim_process, at, schedule, &mut || harness.hang_up());
            println!("[live] shutdown(): nvim's stdin is closed ({ended:?}); tearing the harness down");
            let exited_cleanly = session.harness.shutdown();
            println!("[live] LiveHarness::shutdown() returned {exited_cleanly}");
            exited_cleanly
        } else {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_start_failure_message, cell_size_changed, exit_events, failed_tick_should_render, nvim_args,
        os_scale_to_apply, run_detached, sessionless_tick_should_render, spawn_named, tick_should_render, ExitEvents,
        LiveState, RemapKick, ScaleWatch, NVIM_INSTALL_URL, NVIM_VERSION_FLOOR, REMAP_RENDER_DELAY,
    };
    use gtk4::glib;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    /// S3 change 1, the part of it a test can hold without a display: a map asks for exactly one
    /// render, never in the same main-loop turn, and a burst of maps asks for one. What it cannot
    /// hold is the wiring (`connect_map` on a real `GLArea`) and the pixels; the GUI checklist's
    /// "edit while hidden, then show" item does.
    #[test]
    fn a_map_asks_for_one_render_after_the_delay_and_never_in_the_same_turn() {
        assert_eq!(
            REMAP_RENDER_DELAY,
            Duration::from_millis(50),
            "S3's measured fix; another delay, or a frame-clock tick, needs the same 50-cycle check first"
        );
        let _serial = crate::DEFAULT_CONTEXT_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
            "nothing in the turn the show happens in: it would coalesce into the first frame (S3: 6/8)"
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
            nvim_args(true, &extra),
            vec!["--clean", "--cmd", hide, "--cmd", "lua print(1)"]
        );
        assert_eq!(nvim_args(false, &extra), vec!["--cmd", hide, "--cmd", "lua print(1)"]);
        assert_eq!(nvim_args(true, &[]), vec!["--clean", "--cmd", hide]);
        assert_eq!(nvim_args(false, &[]), vec!["--cmd", hide]);
    }

    /// A scale-factor change alone -- nothing animating, no new nvim content, no pending
    /// resize/keypress -- still has to queue a render. If `tick_should_render` ever drops its
    /// fourth term, this is the assertion that catches it: with everything else `false`, only the
    /// scale term can make the result `true`. Red-checked by temporarily deleting
    /// `|| scale_factor_changed_now` from `tick_should_render`'s body; see this file's own commit
    /// message and `.superpowers/zoom/L2-report.md` for the failing output that produced.
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

    // --- cell_size_changed: wave 4, R5's "fires on change only" rule. ---

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

    // --- os_scale_to_apply: v1 P2, S1's integer-scale rule and S2's "only when it actually
    // differs" gate -- the pure half of `sync_os_scale`, which a unit test cannot construct (it
    // takes a real `&mut LiveHarness`). ---

    /// GTK's `scale_factor()` is the source of truth (S1); a harness already there needs nothing.
    #[test]
    fn os_scale_to_apply_follows_gtks_integer_factor() {
        assert_eq!(os_scale_to_apply(1.0, 2), Some(2.0));
        assert_eq!(os_scale_to_apply(2.0, 2), None);
        assert_eq!(os_scale_to_apply(2.0, 1), Some(1.0));
        // A widget not yet on a surface reports 0 (and never negative in practice): treat as 1, the
        // same `.max(1)` every other scale read in this crate uses.
        assert_eq!(os_scale_to_apply(1.0, 0), None);
        assert_eq!(os_scale_to_apply(f64::NAN, 1), Some(1.0));
    }

    // --- build_start_failure_message: R1-4's "what failed, the version floor, where to get one",
    // built from the raw `anyhow` chain `LiveHarness::with_options` returns. No unit test here can
    // construct a real one, so the two raw shapes the pinned fork actually produces (bridge/mod.rs)
    // are the fixtures. ---

    /// The too-old shape: the fork's own sentence names the found version; the built message must
    /// name it too, plus Eitri's own floor and where to get a newer nvim.
    #[test]
    fn a_too_old_nvim_names_the_version_that_was_found() {
        // The real shape: the fork puts `:version`'s whole first line, "NVIM v0.9.5", into {found}
        // (the GUI pass of 2026-09-27, finding F1: the message read "nvim NVIM v0.9.5 is older").
        let raw = "Neovide requires nvim version 0.10.0 or higher, but NVIM v0.9.5 was detected. Download \
                    the latest version here https://github.com/neovim/neovim/wiki/Installing-Neovim";
        let message = build_start_failure_message(raw);
        assert!(message.starts_with("nvim 0.9.5 is older"), "message: {message}");
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

    // --- failed_tick_should_render: R1-4's "render once and stop" -- the tick callback's own
    // decision for LiveState::Failed, split out for the same reason `tick_should_render` is (no
    // unit test here can construct a real GLArea tick callback). ---

    #[test]
    fn the_tick_still_renders_once_more_right_after_a_start_failure() {
        assert!(failed_tick_should_render(false));
    }

    #[test]
    fn the_tick_reports_idle_once_the_failed_frame_has_been_painted() {
        assert!(!failed_tick_should_render(true));
    }

    /// Round 4 (and codex's round-3 findings (c) and (d)): an exit is reported only once nvim's own
    /// process is gone, exactly once however often the tick asks; while the fork says exited and
    /// the process lives, the host is told once that nvim is out of reach; with no process known,
    /// the fork's report is all there is.
    #[test]
    fn an_exit_is_reported_once_nvims_process_is_gone_and_only_once() {
        let nothing = ExitEvents::default();
        assert_eq!(exit_events(false, None, false, false), nothing, "running");
        assert_eq!(exit_events(false, Some(true), false, false), nothing);
        let unreachable = ExitEvents {
            unreachable: true,
            exited: false,
        };
        assert_eq!(
            exit_events(true, Some(true), false, false),
            unreachable,
            "the launcher's exit"
        );
        assert_eq!(exit_events(true, Some(true), true, false), nothing, "said once");
        let exited = ExitEvents {
            unreachable: false,
            exited: true,
        };
        assert_eq!(exit_events(true, Some(false), true, false), exited, "then its own exit");
        assert_eq!(exit_events(true, Some(false), true, true), nothing, "reported once");
        assert_eq!(
            exit_events(true, None, false, false),
            exited,
            "nothing known: the fork's word"
        );
    }

    /// Round 5, codex finding 2: a thread that cannot be started (`EAGAIN` under a process limit,
    /// reproduced by the reviewer) must not leave nvim's ending unscheduled -- the pane had marked
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

    /// The Opus review's T7-2: the tick's own decision for every state without a live session, so
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
}
