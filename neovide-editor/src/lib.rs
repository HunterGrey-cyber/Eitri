//! An embeddable Neovide/Skia/GTK4 editor surface.
//!
//! `NeovideEditorPane` owns a `LiveHarness` connection to `nvim --embed` and attaches resize,
//! IME, keyboard, mouse, tick and render callbacks to a `GtkGLArea`. The host creates the
//! `Application` and `ApplicationWindow`, presents the window and handles close requests.
//!
//! When `LiveHarness::has_neovim_exited()` reports that nvim quit on its own, the pane invokes
//! `on_exited_unrequested`. The host chooses how that affects its window. The host's close-request
//! handler can call the pane's public `shutdown()` method.

mod frame_clock;
mod gl_interop;
mod keyboard;
mod mouse;
mod nvim_child;
mod nvim_rpc;
mod stdin;

pub use nvim_rpc::{CallWatch, NvimMode};
pub use stdin::{detach_stdin_from_nvim, ForwardedStdin};

use std::cell::{Cell, RefCell};
use std::rc::Rc;
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

/// Log a frame-pacing line every N frames instead of spamming stdout every frame.
const LOG_EVERY_N_FRAMES: u64 = 60;

/// How many `add_tick_callback` invocations between `[tick]` summary log lines (see `TickStats`).
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
    /// invocation below. Starts `true` so the tick callback keeps rendering continuously through
    /// the first few Ready-state frames, until a real `render_frame` call has actually reported a
    /// real value -- erring toward "render" rather than "skip" whenever this value hasn't been
    /// established yet.
    last_animating: Cell<bool>,
    /// `harness.redraw_batches_seen()` as of the last time either the render callback or the tick
    /// callback looked at it. The tick callback calls `LiveHarness::pump` every tick specifically
    /// so a change here is visible at full display-refresh-rate latency even on ticks that don't
    /// render -- this is the "did nvim actually send anything new" half of the fix, independent
    /// of `last_animating`.
    last_seen_batches: Cell<u64>,
    /// Set by the resize handler and the keyboard input handler: both are real external events
    /// that deserve a guaranteed next frame regardless of what `last_animating`/
    /// `last_seen_batches` currently say (a resize needs its own frame at the new size even if
    /// nvim sent nothing new; a keypress deserves a same-tick-latency render rather than waiting
    /// on nvim's async redraw round-trip to eventually move `last_seen_batches`). Read-and-cleared
    /// by the tick callback every tick.
    pub(crate) wants_frame: Cell<bool>,
    /// The grid (cols, rows) size nvim was last asked to resize to via
    /// `LiveHarness::resize_grid`. Set once at construction (to whatever the initial
    /// `content_region` computed to) and re-checked on every `connect_resize` callback so a real
    /// RPC is only sent when the *grid-cell* size actually changes, not on every pixel-level
    /// resize event mid-drag.
    last_grid_size: Cell<GridSize<u32>>,
    /// Set after the tick reports an unrequested nvim exit through `on_exited_unrequested`.
    /// `has_neovim_exited()` stays true, so this guard prevents repeated callbacks and log messages
    /// while the host finishes teardown. `Cell::replace` checks and sets it on the GTK main thread.
    ///
    /// `NeovideEditorPane::shutdown()` also sets it so a host-initiated shutdown is not reported
    /// again as an unrequested exit when nvim stops.
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
    /// The framebuffer size in device pixels last reported by `GLArea::resize`.
    /// Shared by rendering, resizing and the tick so mouse hit testing uses the size actually drawn
    /// through `gl_interop::current_content_region` and `gl_interop::GridLayout`.
    pub(crate) fb_size: Rc<Cell<(i32, i32)>>,
    /// `(fb_w, fb_h, cell_w bits, cell_h bits, cols, rows)` as the `[layout]` line last printed
    /// it, so the line is printed when the layout changes and never per frame. Bits, not f32, so
    /// a NaN cell before nvim reports a font does not count as a change every frame.
    last_logged_layout: Cell<Option<LayoutLogKey>>,
    /// The `nvim --embed` process itself (`nvim_child`'s module doc): what a shutdown that timed out
    /// kills, and what [`NeovideEditorPane::kill_nvim`] kills. Holds what the fork started from the
    /// launch on, and nvim's own `getpid()` answer once it comes back ([`ask_nvim_for_its_pid`]);
    /// shared with the thread that asks.
    nvim_process: Arc<nvim_child::NvimProcess>,
    /// The last [`NeovideEditorPane::exec_lua_watched`] request and its watch (`nvim_rpc`).
    watched: Option<nvim_rpc::WatchedCall>,
}

impl LiveSession {
    fn new(
        harness: LiveHarness,
        grid_size: GridSize<u32>,
        fb_size: Rc<Cell<(i32, i32)>>,
        nvim_process: Arc<nvim_child::NvimProcess>,
    ) -> Self {
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
            wants_frame: Cell::new(false),
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
        }
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
/// tick-callback invocations. An idle pane should mostly skip rendering; typing, scrolling and
/// animation should mostly issue renders.
struct TickStats {
    ticks: Cell<u64>,
    issued: Cell<u64>,
    skipped: Cell<u64>,
    issued_total: Cell<u64>,
    skipped_total: Cell<u64>,
}

impl TickStats {
    fn new() -> Self {
        Self {
            ticks: Cell::new(0),
            issued: Cell::new(0),
            skipped: Cell::new(0),
            issued_total: Cell::new(0),
            skipped_total: Cell::new(0),
        }
    }

    /// Record one tick's outcome; every `TICK_LOG_EVERY_N_TICKS` ticks, print a `[tick]` summary
    /// of the just-finished window and reset the windowed counters (the `_total` counters keep
    /// accumulating for the life of the process).
    fn record(&self, issued_this_tick: bool) {
        self.ticks.set(self.ticks.get() + 1);
        if issued_this_tick {
            self.issued.set(self.issued.get() + 1);
            self.issued_total.set(self.issued_total.get() + 1);
        } else {
            self.skipped.set(self.skipped.get() + 1);
            self.skipped_total.set(self.skipped_total.get() + 1);
        }

        if self.ticks.get() >= TICK_LOG_EVERY_N_TICKS {
            let ticks = self.ticks.get();
            let issued = self.issued.get();
            let skipped = self.skipped.get();
            println!(
                "[tick] last {ticks} ticks: issued={issued} skipped={skipped} \
                 skip_ratio={:.1}% (cumulative issued={} skipped={})",
                (skipped as f64 / ticks as f64) * 100.0,
                self.issued_total.get(),
                self.skipped_total.get(),
            );
            self.ticks.set(0);
            self.issued.set(0);
            self.skipped.set(0);
        }
    }
}

/// Slot for the callback `on_exited_unrequested` registers, shared with the tick callback set up
/// in `NeovideEditorPane::new()`. Factored into a named alias purely to satisfy
/// `clippy::type_complexity` -- no behavior difference from writing the nested type out inline.
type ExitedCallbackSlot = Rc<RefCell<Option<Box<dyn Fn()>>>>;
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

/// Delay after mapping `GLArea` before requesting another frame. A request in the same main-loop
/// turn can coalesce into the first frame. The 50 ms delay restored content in 50 of 50 re-shows;
/// a same-turn request worked in only 6 of 8. A frame-clock tick has not been measured here.
const REMAP_RENDER_DELAY: Duration = Duration::from_millis(50);

/// Schedules one delayed render per map. A new map replaces any pending render, so a burst of
/// hide/show cycles requests only one frame.
///
/// Showing the pane over the same rectangle after `set_child_visible(false)` need not emit
/// `resize` or set `wants_frame`. If nvim redrew while hidden, its traffic is still drained,
/// but the first mapped frame can show only the cursor until another redraw arrives.
/// `GtkPaned::set_visible(true)` forces a resize and further renders, masking this case.
/// It is unclear whether the incomplete frame comes from the renderer or GTK's remapped texture.
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

/// Whether `connect_cell_size_changed` should fire this frame. `None` means nvim has not yet
/// reported a cell size and always counts as a change; otherwise compare the sizes directly.
/// Kept separate from the render callback so this can be tested without constructing `GLArea`
/// or `LiveSession`.
fn cell_size_changed(last: Option<(f64, f64)>, now: (f64, f64)) -> bool {
    last != Some(now)
}

/// The nvim version floor enforced by `NEOVIM_REQUIRED_VERSION` in `bridge/mod.rs`.
/// Repeated here so the pane's launch-failure message can state the required version directly.
const NVIM_VERSION_FLOOR: &str = "0.10";

/// Where to get a newer nvim -- the same URL the fork's own too-old error already links
/// (`bridge/mod.rs:197`), restated here so the missing-nvim case (whose error text never names a
/// URL at all) gets one too.
const NVIM_INSTALL_URL: &str = "https://github.com/neovim/neovim/wiki/Installing-Neovim";

/// Builds the launch-failure message stored in [`LiveState::Failed`] and passed to
/// [`NeovideEditorPane::on_start_failed`] from `LiveHarness::with_options`'s raw error chain.
/// The caller also logs the raw chain to stdout.
///
/// For a too-old nvim, [`detected_nvim_version`] extracts the detected version from the harness
/// error. For a missing executable or another launch failure, keep the raw error detail.
/// Both messages include the required version and an installation link.
/// Kept separate so tests can use raw error strings without constructing a live harness.
fn build_start_failure_message(raw_error: &str) -> String {
    match detected_nvim_version(raw_error) {
        Some(found) => format!(
            "nvim {found} is older than neovibe's floor ({NVIM_VERSION_FLOOR} or newer). Get a newer \
             nvim: {NVIM_INSTALL_URL}"
        ),
        None => format!(
            "nvim could not be started (neovibe needs {NVIM_VERSION_FLOOR} or newer): {raw_error}. \
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
    Some(version)
}

/// Asks nvim for `getpid()` over the harness connection and stores the answer in `process`.
/// The answering process is nvim itself; a launcher that does not `exec` can be the direct child,
/// and killing that launcher can leave nvim running.
///
/// Use a worker thread because nvim answers only after sourcing the user's configuration,
/// which may hang. Until it answers, retain the process identified at launch.
/// Never abandon the request: nvim-rs closes the connection if a response arrives for a dropped
/// request. Nvim exiting resolves the request with an error.
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

/// Whether the tick should request a frame for [`LiveState::Failed`]. Render once to paint
/// [`FAILED_COLOR`] and show the failure message, then stop requesting frames.
/// The state is terminal, so unconditional requests would redraw it at every display refresh.
/// Kept separate from the GTK callback for testing.
fn failed_tick_should_render(already_painted: bool) -> bool {
    !already_painted
}

/// Whether to request a frame without a live session. The tick handles [`LiveState::Ready`]
/// separately through [`tick_should_render`]. Keeping this dispatch testable checks the actual
/// `Failed` arm as well as [`failed_tick_should_render`].
fn sessionless_tick_should_render(state: &LiveState, failed_painted: bool) -> bool {
    match state {
        // The placeholder-frame dance and the one blocking `LiveHarness::with_options` call both
        // happen *inside* the render callback and only run when a render is actually requested --
        // keep rendering here so that state machine can advance.
        LiveState::NotStarted | LiveState::Starting => true,
        // `Failed` is terminal and paints the same fill and message each time.
        // Request frames only until the render callback sets `failed_painted`, then leave it idle.
        LiveState::Failed(_) => failed_tick_should_render(failed_painted),
        // Nothing will ever change: no frame is needed.
        LiveState::Exited => false,
        // Not sessionless: never asked here (the tick's own `Ready` arm decides).
        LiveState::Ready(_) => true,
    }
}

/// The OS scale to pass to `LiveHarness::set_os_scale_factor`, or `None` if `current` already
/// matches GTK's `Widget::scale_factor()`. Kept pure for tests without a live harness.
///
/// Use GTK's integer factor, matching `pixel_to_grid_pos`, `cell_size` and the tick's widget-size
/// calculation. Clamp values below 1 to 1 for widgets not yet realized on a surface.
/// Compare bits as `LiveHarness::set_os_scale_factor` does, so both agree on whether to apply it.
fn os_scale_to_apply(current: f64, widget_scale_factor: i32) -> Option<f64> {
    let target = f64::from(widget_scale_factor.max(1));
    if target.to_bits() == current.to_bits() {
        None
    } else {
        Some(target)
    }
}

/// Applies GTK's `Widget::scale_factor()` when it differs from the harness's OS scale
/// ([`os_scale_to_apply`]). Runs in the `Ready` paths of `connect_resize` and the render callback,
/// before computing a grid from `harness.grid_scale()`.
///
/// `set_os_scale_factor` changes cell size synchronously. Applying it with the matching new
/// framebuffer keeps grid calculations from mixing dimensions from different scales.
/// The scale-factor notification only requests a frame; GTK supplies the resized framebuffer
/// on its next snapshot, and scale synchronization happens then.
///
/// Returns `true` only when the harness applies a change. Logs one `[hidpi]` line for that change;
/// an unchanged scale costs one bit comparison and produces no log output.
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
        let gl_area = GLArea::builder()
            .hexpand(true)
            .vexpand(true)
            .has_stencil_buffer(true)
            .auto_render(true)
            .focusable(true)
            .can_focus(true)
            .build();

        let skia_state: Rc<RefCell<Option<SkiaState>>> = Rc::new(RefCell::new(None));
        let live_state: Rc<RefCell<LiveState>> = Rc::new(RefCell::new(LiveState::NotStarted));
        let clear_color = Rc::new(Cell::new(OUTSIDE_COLOR));
        let focused: Rc<Cell<Option<bool>>> = Rc::new(Cell::new(None));
        let exited_callback: ExitedCallbackSlot = Rc::new(RefCell::new(None));
        let fullscreen_callback: FullscreenCallbackSlot = Rc::new(RefCell::new(None));
        let pending_fullscreen: Rc<Cell<Option<bool>>> = Rc::new(Cell::new(None));
        let scale_factor_callback: ScaleFactorCallbackSlot = Rc::new(RefCell::new(None));
        let scale_watch: Rc<ScaleWatch> = Rc::new(ScaleWatch::new(1.0));
        let cell_size_callback: CellSizeCallbackSlot = Rc::new(RefCell::new(None));
        let start_failed_callback: StartFailedCallbackSlot = Rc::new(RefCell::new(None));
        // Set when the `Failed` arm first paints `FAILED_COLOR` and the message.
        // The tick reads it through `failed_tick_should_render` to stop requesting frames afterward.
        // `Failed` is terminal, so this is never reset.
        let failed_painted: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        // Only the render closure below ever reads this -- not stored on `Self`, unlike
        // `cell_size_callback`, which a host registers *after* construction.
        let last_cell_size: Rc<Cell<Option<(f64, f64)>>> = Rc::new(Cell::new(None));
        // Use the framebuffer size reported by `GLArea::resize`, in device pixels, for the grid,
        // snapping, tick resync and hit testing. `gl_interop::GridLayout` keeps them consistent.
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

        // GtkGLArea can resize or recreate its FBO. Drop the cached Surface so the next render rebuilds
        // it with the new framebuffer dimensions in device pixels.
        // Recompute the grid that fits `content_region` and call `LiveHarness::resize_grid` only if the
        // cell count changed, keeping nvim's viewport aligned with the host.
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
                    // Sync scale before reading `grid_scale()` so the new cell size and the framebuffer supplied
                    // by this `resize` signal are used together.
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

        // GtkGLArea reallocates its buffers and emits `resize` on the next snapshot after a scale
        // notification. Request a frame here, or the old image can remain upscaled indefinitely.
        // Do not change the harness scale or `fb_size` here. `sync_os_scale` runs in `connect_resize`
        // and the render callback once GTK supplies the matching new framebuffer.
        {
            let live_state = live_state.clone();
            gl_area.connect_scale_factor_notify(move |widget| {
                // Use `try_borrow`: a GTK signal can run while a render or tick closure already borrows
                // `live_state`. Skipping that borrow avoids a reentrant `RefCell` panic.
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
        let im_context = keyboard::attach_keyboard_input(&gl_area, &live_state);

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
                state.ensure_gtk_surface();
                state.ensure_render_surface();

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
                // Set in the `Starting` arm's error case and fired after dropping `live`, allowing reentry.
                // This fires at most once per pane because `Starting` transitions to `Failed` only once.
                let mut start_failed_message: Option<String> = None;

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
                                // `LiveHarnessOptions::grid_size` only sets the grid at `nvim_ui_attach` time.
                                // Leaving it `None` uses `DEFAULT_GRID_SIZE` (100x50), independent of this pane's content region.
                                // Resize immediately to the grid that fits the actual `content_region`.
                                // Use the shared `grid_layout` geometry. Snapping preserves the number of rows that fit,
                                // so it gives the same grid size as the unsnapped region.
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
                                )));
                            }
                            Err(err) => {
                                let elapsed = t0.elapsed();
                                let raw = format!("{err:#}");
                                println!("[live] LiveHarness::with_options failed after {elapsed:?}: {raw}");
                                // Stdout gets the raw error chain. The pane stores and passes the host a message describing
                                // the failure, the required version and where to get a newer nvim.
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
                        // Sync here too for re-realized widgets or snapshots that do not emit `resize`.
                        // Together with `connect_resize`, this keeps cell size and framebuffer scale consistent.
                        // Set `wants_frame` when a scale change is applied here because this path has no resize handler
                        // to request the next frame.
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
                            // `render_frame` pumps nvim and applies `g:neovide_scale_factor` and `guifont` changes before
                            // reading `grid_scale`. This frame was snapped for the old cell height, so the new rows
                            // can leave a band at the bottom. Request another frame to snap with the new scale;
                            // the tick's resync corrects the row count. An idle pane has no other render trigger.
                            // The next frame sees the scale already applied, so this does not loop.
                            session.wants_frame.set(true);
                        }
                        // The host uses the editor's cell height for `--nv-editor-row` to bound the status band.
                        // Compute it with the same `grid_scale` and `scale_factor` as `cell_size()`.
                        // Report only changes; `cell_size_changed` treats the first value as a change too.
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
                        // This marks the paint awaited by `sessionless_tick_should_render`.
                        // Once set, the tick stops requesting frames.
                        failed_painted_for_render.set(true);
                    }
                    // Only the clear above: a host that released the pane does not show it.
                    LiveState::Exited => {}
                }
                drop(live);

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

        // Drive redraws from the display's frame clock so pacing follows vsync timing.
        // Call `queue_render()` only when another frame is needed.
        {
            let live_state = live_state.clone();
            let gl_area_for_tick = gl_area.clone();
            // Needed for the nvim-exited-on-its-own case just below: invoking the registered
            // `exited_callback` may, on the host side, lead straight back into `shutdown()` (e.g.
            // if the callback closes the host's own window, whose `connect_close_request` handler
            // calls `pane.shutdown()`), which itself needs `live_state.borrow_mut()` -- so this
            // closure must never invoke the callback while still holding `live` borrowed, or
            // that reentrant borrow would hit a `RefCell` `BorrowMutError` panic. See
            // `should_fire_exited_callback` below for how that's kept safe.
            let exited_callback_for_tick = exited_callback.clone();
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
            // Read only here; the render callback's `Failed` arm is the sole writer.
            // See `failed_tick_should_render`.
            let failed_painted_for_tick = failed_painted.clone();
            gl_area.add_tick_callback(move |widget, _clock| {
                let mut live = live_state.borrow_mut();
                // Set from inside the `Ready` arm below, then acted on only *after* `live` is
                // dropped -- see the comment on `exited_callback_for_tick` above for why the
                // ordering matters.
                let mut should_fire_exited_callback = false;
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

                        // Resync the grid if its size no longer matches the framebuffer. A window can reach its final
                        // size while `LiveHarness::with_options` blocks the main loop, without another `resize` after
                        // the initial snapshot. That leaves a stale grid and an undrawn band at its edge.
                        // `draw_frame` clears the current content region to nvim's background, so the band has that color.
                        // Recompute the fitting grid each tick and call `resize_grid` only when `last_grid_size` differs.
                        //
                        // Use `fb_size` from `GLArea::resize`, matching the framebuffer actually drawn.
                        // In GTK 4.22.5, buffers and the resize signal use widget dimensions times the scale factor.
                        // They can lag the allocation while hidden or before a snapshot; the next snapshot updates both.
                        // Log a note if the dimensions differ on a drawn widget, since its buffer sizing may differ.
                        //
                        // During a scale change, `notify::scale-factor` updates the widget factor before the next
                        // snapshot supplies a new framebuffer and synchronizes the harness. This gap can last longer
                        // while hidden. Suppress the note during that expected mismatch by checking `os_scale_factor()`.
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

                        // `shutdown()` handles host-initiated closure. Check here after `pump()` for nvim exiting on
                        // its own, such as through `:qa!`, so the host can react on the same tick.
                        // `close_requested.replace(true)` makes the notification one-shot, even though the harness
                        // continues to report the exit on later ticks.
                        if session.harness.has_neovim_exited() && !session.close_requested.replace(true) {
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
                    gl_area_for_tick.queue_render();
                }
                tick_stats.record(issued);

                glib::ControlFlow::Continue
            });
        }

        Self {
            widget: gl_area,
            im_context,
            live_state,
            exited_callback,
            clear_color,
            focused,
            fullscreen_callback,
            pending_fullscreen,
            scale_factor_callback,
            scale_watch,
            cell_size_callback,
            start_failed_callback,
        }
    }

    /// The `GtkGLArea` this pane renders into. The host places this into its own window (e.g. as
    /// an `ApplicationWindow`'s child) -- this pane never does so itself.
    pub fn widget(&self) -> &gtk4::GLArea {
        &self.widget
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
    /// real window and follows the variable. Read from the tick callback. **Corrected 2026-09-23:**
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
    /// own alongside the editor. Read from the tick callback -- see
    /// [`on_fullscreen_setting`](Self::on_fullscreen_setting)'s own doc (corrected 2026-09-23) for
    /// what is and is not known about whether hiding this pane's widget pauses that tick: most
    /// likely it does not, per GTK's own source, but that has never been confirmed on a screen.
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
            session.harness.send_text_input(keys);
            session.wants_frame.set(true);
        } else {
            println!("[live] send_keys({keys:?}) before nvim is ready -- dropped");
        }
        self.widget.queue_render();
    }

    /// Whether nvim is up, so a host can refuse a request `send_keys` would drop (phase 3: the
    /// scratch round trips answer "the editor is not ready yet" instead of doing nothing).
    pub fn is_ready(&self) -> bool {
        matches!(&*self.live_state.borrow(), LiveState::Ready(_))
    }

    /// Whether nvim is up and has not exited, as of the last tick's pump. Unlike
    /// [`is_ready`](Self::is_ready), which stays `true` from an unrequested exit until the host
    /// [`release_exited`](Self::release_exited) the pane: a host deciding whether nvim can still be
    /// asked something (a window close's `:confirm qall`) asks this.
    pub fn is_running(&self) -> bool {
        matches!(&*self.live_state.borrow(), LiveState::Ready(session) if !session.harness.has_neovim_exited())
    }

    /// Whether nvim ran and has exited: seen exited and not yet released, or released
    /// ([`LiveState::Exited`]). `false` for an nvim that never started.
    pub fn nvim_exited(&self) -> bool {
        match &*self.live_state.borrow() {
            LiveState::Ready(session) => session.harness.has_neovim_exited(),
            LiveState::Exited => true,
            LiveState::NotStarted | LiveState::Starting | LiveState::Failed(_) => false,
        }
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
                session.wants_frame.set(true);
                true
            }
            Err(err) => {
                eprintln!("[live] exec_lua_watched: could not start its thread ({err}) -- nothing sent");
                false
            }
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

    /// Kills the nvim process itself (SIGKILL) -- nvim, never a launcher that started it
    /// (`nvim_child`'s module doc) -- for a host whose nvim stopped answering. Its swap files stay,
    /// so its unsaved changes can be recovered from them; its exit then reaches the host as any exit
    /// does ([`on_exited_unrequested`](Self::on_exited_unrequested)). `true` when nvim was killed or
    /// was already gone (its exit is on its way); `false`, doing nothing, when nvim is not running
    /// or no process is known to be nvim.
    pub fn kill_nvim(&self) -> bool {
        match &*self.live_state.borrow() {
            LiveState::Ready(session) if !session.harness.has_neovim_exited() => {
                let killed = session.nvim_process.kill();
                println!(
                    "[live] kill_nvim: {}",
                    match killed {
                        nvim_child::Killed::Sent(pid) => format!("SIGKILL sent to nvim (pid {pid})"),
                        nvim_child::Killed::AlreadyGone(pid) => format!("nvim (pid {pid}) was already gone"),
                        nvim_child::Killed::Unknown => "no process is known to be nvim; nothing was killed".to_string(),
                    }
                );
                killed != nvim_child::Killed::Unknown
            }
            _ => false,
        }
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

    /// Registers `callback` for changes to the logical pixel `(width, height)` returned by
    /// [`cell_size`](Self::cell_size), including the first observed value. A host can use these
    /// dimensions to match a panel's status band to the editor's row height.
    /// The render callback invokes it after `render_frame` and after dropping `live`, so a host
    /// handler can call back into the pane. Replaces any earlier callback.
    pub fn connect_cell_size_changed(&self, callback: impl Fn(f64, f64) + 'static) {
        *self.cell_size_callback.borrow_mut() = Some(Box::new(callback));
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

    /// Registers a callback for a `LiveHarness::with_options` failure, such as a missing or too-old
    /// nvim. Called at most once with the message from [`build_start_failure_message`], so the host
    /// can show a reason even when stdout is unavailable.
    /// Fires after the render callback drops `live`, allowing the host to call pane methods safely.
    /// Replaces any previously registered callback.
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
        session.close_requested.set(true);
        session.harness.shutdown();
        *live = LiveState::Exited;
        println!("[live] nvim exited and the host released the pane; it stays empty");
        true
    }

    /// Cleanly shuts down the live `nvim --embed` connection, if one was ever started. Mirrors
    /// the reference probe's `connect_close_request` handler body exactly: the host wires this to
    /// its own window's `connect_close_request`. Returns `LiveHarness::shutdown()`'s own return
    /// value (`true` if a real `NeovimExited` was observed, `false` if it timed out waiting for
    /// one) -- or `true` if `LiveState` never reached `Ready` at all, since there is no live
    /// connection to shut down in that case.
    ///
    /// **This runs nvim's `:qa!`** (the fork forces `confirm_quit` off): unsaved buffers are
    /// discarded and their swap files deleted. A host that may still have unsaved work asks nvim
    /// first (`shell` sends `:confirm qall` and calls this only once nvim has exited). A timeout
    /// no longer leaves nvim orphaned: the child is killed by its pid (`nvim_child`).
    pub fn shutdown(&self) -> bool {
        let mut live = self.live_state.borrow_mut();
        if let LiveState::Ready(session) = &mut *live {
            // Set before calling `LiveHarness::shutdown()` below so the tick callback's own
            // `close_requested.replace(true)` guard (see `LiveSession::close_requested`'s doc) is
            // already tripped by the time nvim actually exits -- without this, a host-initiated
            // shutdown wouldn't stop a later tick from independently noticing
            // `has_neovim_exited() == true` and firing `on_exited_unrequested` a second time, for
            // a shutdown the host itself already initiated.
            session.close_requested.set(true);
            println!("[live] shutdown() called: calling LiveHarness::shutdown()...");
            let exited_cleanly = session.harness.shutdown();
            println!(
                "[live] LiveHarness::shutdown() returned {exited_cleanly} \
                 ({})",
                if exited_cleanly {
                    "real NeovimExited observed"
                } else {
                    "timed out waiting for NeovimExited -- killing the nvim child by its pid"
                }
            );
            // Codex p1 #2: the fork leaves a child that never answered running, orphaned. Killed
            // here by the pid this pane holds (`nvim_child`), after the fork's own wait.
            nvim_child::after_shutdown(exited_cleanly, &session.nvim_process);
            exited_cleanly
        } else {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_start_failure_message, cell_size_changed, failed_tick_should_render, nvim_args, os_scale_to_apply,
        sessionless_tick_should_render, tick_should_render, LiveState, RemapKick, ScaleWatch, NVIM_INSTALL_URL,
        NVIM_VERSION_FLOOR, REMAP_RENDER_DELAY,
    };
    use gtk4::glib;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    /// A map requests one delayed render, never in the same main-loop turn; a burst of maps requests
    /// only one. This test covers scheduling without a display. Real `connect_map` wiring and restored
    /// content after editing while hidden require a display to check.
    #[test]
    fn a_map_asks_for_one_render_after_the_delay_and_never_in_the_same_turn() {
        assert_eq!(
            REMAP_RENDER_DELAY,
            Duration::from_millis(50),
            "S3's measured fix; another delay, or a frame-clock tick, needs the same 50-cycle check first"
        );
        let context = glib::MainContext::default();
        let _owner = context
            .acquire()
            .expect("no other test in this crate runs the default main context");
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

    /// A scale-factor change must queue a render even without animation, new nvim content or a
    /// pending resize or keypress. With all other inputs false, only `scale_factor_changed_now`
    /// can make `tick_should_render` true.
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

    // Cell-size notifications fire on the first value and on changes only.

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

    // `os_scale_to_apply` tests the integer scale and skips an already matching harness scale.
    // It is the pure part of `sync_os_scale`, which needs a live `&mut LiveHarness`.

    /// GTK's `scale_factor()` supplies the target; an already matching harness needs no update.
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

    // `build_start_failure_message` includes the failure, required version and installation link.
    // Use the two raw error shapes from `bridge/mod.rs` as fixtures because a unit test here cannot
    // construct a real `LiveHarness::with_options` error.

    /// The too-old shape: the fork's own sentence names the found version; the built message must
    /// name it too, plus neovibe's own floor and where to get a newer nvim.
    #[test]
    fn a_too_old_nvim_names_the_version_that_was_found() {
        let raw = "Neovide requires nvim version 0.10.0 or higher, but 0.9.5 was detected. Download \
                    the latest version here https://github.com/neovim/neovim/wiki/Installing-Neovim";
        let message = build_start_failure_message(raw);
        assert!(message.contains("0.9.5"), "message: {message}");
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

    // `failed_tick_should_render` requests one frame to show the failure, then reports idle.
    // The decision is separate so it can be tested without a real GLArea tick callback.

    #[test]
    fn the_tick_still_renders_once_more_right_after_a_start_failure() {
        assert!(failed_tick_should_render(false));
    }

    #[test]
    fn the_tick_reports_idle_once_the_failed_frame_has_been_painted() {
        assert!(!failed_tick_should_render(true));
    }

    /// Check the dispatch used by the tick for states without a live session.
    /// After a start failure, it requests the frame that shows the message and then reports idle.
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
