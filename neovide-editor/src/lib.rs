//! An embeddable Neovide/Skia/GTK4 editor surface.
//!
//! `NeovideEditorPane` owns a `LiveHarness` connection to `nvim --embed` and attaches resize,
//! IME, keyboard, mouse, tick and render callbacks to a `GtkGLArea`. The host creates the
//! `Application` and `ApplicationWindow`, presents the window and handles close requests.
//!
//! When `LiveHarness::has_neovim_exited()` reports that nvim quit on its own, the pane invokes
//! `on_exited_unrequested`. The host chooses how that affects its window. The host's close-request
//! handler can call the pane's public `shutdown()` method.

mod gl_interop;
mod keyboard;
mod mouse;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    EventControllerMotion, EventControllerScroll, EventControllerScrollFlags, GLArea,
    GestureClick, IMMulticontext,
};

use skia_safe::gpu::direct_contexts;
use skia_safe::Color4f;

use neovide::live_harness::{LiveHarness, LiveHarnessOptions};
use neovide::units::GridSize;

use gl_interop::{
    compute_content_region, fill_content_region, grid_size_for_content_region, make_gl_interface,
    SkiaState,
};
use mouse::DragState;

/// Log a frame-pacing line every N frames instead of spamming stdout every frame.
const LOG_EVERY_N_FRAMES: u64 = 60;

/// How many `add_tick_callback` invocations between `[tick]` summary log lines (see `TickStats`).
const TICK_LOG_EVERY_N_TICKS: u64 = 300;

/// Defensive clear colour for the whole framebuffer before `LiveHarness::render_frame`.
///
/// With `CONTENT_MARGIN` at 0, `compute_content_region` covers the whole framebuffer and every
/// render state paints over this clear. `Ready` clips `Renderer::draw_frame` to `content_region`
/// and clears it to nvim's `Normal` background with an `SkBlendMode::kSrc` fill. The other states
/// use the opaque `STARTING_COLOR` or `FAILED_COLOR`.
///
/// The sub-cell remainder at the right and bottom lies inside `content_region` and is also
/// painted by the renderer. This near-black colour becomes visible if a non-zero margin leaves
/// pixels outside the content region. There is no host override in this version.
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
}

pub struct LiveSession {
    pub(crate) harness: LiveHarness,
    start: Instant,
    last_frame: Instant,
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
}

impl LiveSession {
    fn new(harness: LiveHarness, grid_size: GridSize<u32>) -> Self {
        let now = Instant::now();
        Self {
            harness,
            start: now,
            last_frame: now,
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

/// The whole argument list this pane hands to `LiveHarnessOptions::extra_nvim_args`.
fn nvim_args(clean: bool, extra: &[String]) -> Vec<String> {
    let mut args = Vec::with_capacity(extra.len() + 1);
    if clean {
        args.push("--clean".to_string());
    }
    args.extend(extra.iter().cloned());
    args
}

impl NeovideEditorPane {
    /// Builds the `GtkGLArea` and wires every resize/IME/keyboard/mouse/tick/render callback onto
    /// it and the shared `Rc<RefCell<LiveState>>` -- everything `poc/neovide_embed_live::build_ui`
    /// did except constructing an `Application`/`ApplicationWindow` and calling
    /// `.present()`/`.grab_focus()` (the host does those, via `.widget()`/`.grab_focus()` below).
    /// `clean` is forwarded to `LiveHarnessOptions::extra_nvim_args` as `--clean`, for
    /// deterministic manual-verification runs; everything else takes its default. Use
    /// [`NeovideEditorPane::with_options`] to set anything more.
    pub fn new(clean: bool) -> Self {
        Self::with_options(NeovideEditorPaneOptions { clean, ..Default::default() })
    }

    /// Same as [`NeovideEditorPane::new`], with every construction-time knob this pane forwards to
    /// `LiveHarnessOptions` spelled out. See [`NeovideEditorPaneOptions`] for what each one means.
    pub fn with_options(options: NeovideEditorPaneOptions) -> Self {
        let NeovideEditorPaneOptions { clean, child_env, cwd, extra_nvim_args } = options;
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
        let exited_callback: ExitedCallbackSlot = Rc::new(RefCell::new(None));

        // GtkGLArea can resize or recreate its FBO. Drop the cached Surface so the next render rebuilds
        // it with the new framebuffer dimensions in device pixels.
        // Recompute the grid that fits `content_region` and call `LiveHarness::resize_grid` only if the
        // cell count changed, keeping nvim's viewport aligned with the host.
        {
            let skia_state = skia_state.clone();
            let live_state = live_state.clone();
            gl_area.connect_resize(move |widget, width, height| {
                let mut live = live_state.borrow_mut();
                let (forced_next_frame, resized_grid) = if let LiveState::Ready(session) = &mut *live {
                    session.wants_frame.set(true);
                    let content_region = compute_content_region(width, height);
                    let new_grid_size = grid_size_for_content_region(&session.harness, &content_region);
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
                        state.surface = None; // force rebuild on next render
                    }
                    None => {
                        // GrContext not created yet; render() will pick up the GLArea's current
                        // size directly.
                    }
                }
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
            gl_area.connect_render(move |widget, _gl_ctx| {
                let mut state_slot = skia_state.borrow_mut();

                if state_slot.is_none() {
                    let interface = make_gl_interface();
                    let gr_context = direct_contexts::make_gl(interface, None)
                        .expect("failed to create Skia GL DirectContext");
                    let width = widget.width() * widget.scale_factor();
                    let height = widget.height() * widget.scale_factor();
                    println!(
                        "[init] Skia DirectContext created; initial fb={}x{}px scale_factor={}",
                        width,
                        height,
                        widget.scale_factor()
                    );
                    *state_slot = Some(SkiaState { gr_context, surface: None, fb_width: width, fb_height: height });
                }

                let state = state_slot.as_mut().unwrap();
                // Must come before anything draws this frame: GTK shares this GL context with us
                // and rebinds textures behind Skia's back (on a resize in particular), which makes
                // every glyph paint as a solid block until Skia is told to distrust its state
                // cache. See `SkiaState::invalidate_cached_gl_state` for the full root-cause
                // record and the measurements behind doing this every frame rather than only on
                // resize frames.
                state.invalidate_cached_gl_state();
                state.ensure_surface();

                let Some(surface) = state.surface.as_mut() else {
                    return glib::Propagation::Stop;
                };

                let (fb_w, fb_h) = (state.fb_width, state.fb_height);
                let content_region = compute_content_region(fb_w, fb_h);
                let canvas = surface.canvas();

                // Cheap defensive clear, predating this work. Paint the entire framebuffer
                // `OUTSIDE_COLOR` first, then hand only `content_region` to whatever's actually
                // drawing this frame. At CONTENT_MARGIN 0 those two are the same rect and *every*
                // arm below repaints all of it -- the renderer clip-clears `content_region` to
                // nvim's own background, the placeholder arms fill it opaquely -- so nothing this
                // clear writes survives the frame today; see `OUTSIDE_COLOR`'s own doc for the
                // positive-control evidence.
                canvas.clear(OUTSIDE_COLOR);

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
                            ..Default::default()
                        };
                        println!(
                            "[live] constructing LiveHarness::with_options(os_scale_factor={os_scale_factor}, \
                             clean={clean}, cwd={cwd:?}) -- this performs a real, synchronous nvim \
                             launch and WILL block the GTK main loop until it returns"
                        );
                        let t0 = Instant::now();
                        match LiveHarness::with_options(options) {
                            Ok(mut harness) => {
                                let elapsed = t0.elapsed();
                                println!(
                                    "[live] LiveHarness::with_options returned after {elapsed:?} \
                                     (blocked the GTK main loop for that long)"
                                );
                                // `LiveHarnessOptions::grid_size` only sets the grid at `nvim_ui_attach` time.
                                // Leaving it `None` uses `DEFAULT_GRID_SIZE` (100x50), independent of this pane's content region.
                                // Resize immediately to the grid that fits the actual `content_region`.
                                let grid_size = grid_size_for_content_region(&harness, &content_region);
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
                                *live = LiveState::Ready(Box::new(LiveSession::new(harness, grid_size)));
                            }
                            Err(err) => {
                                let elapsed = t0.elapsed();
                                let message = format!("{err:#}");
                                println!(
                                    "[live] LiveHarness::with_options failed after {elapsed:?}: {message}"
                                );
                                *live = LiveState::Failed(message);
                            }
                        }
                    }
                    LiveState::Ready(session) => {
                        let (dt, fps) = session.tick();
                        let animating =
                            session.harness.render_frame(canvas, Some(&content_region), dt);
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
                                dt * 1000.0,
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
                    LiveState::Failed(message) => {
                        fill_content_region(canvas, &content_region, FAILED_COLOR);
                        let _ = message; // already logged once when the transition happened
                    }
                }
                drop(live);

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
            let tick_stats = Rc::new(TickStats::new());
            gl_area.add_tick_callback(move |widget, _clock| {
                let mut live = live_state.borrow_mut();
                // Set from inside the `Ready` arm below, then acted on only *after* `live` is
                // dropped -- see the comment on `exited_callback_for_tick` above for why the
                // ordering matters.
                let mut should_fire_exited_callback = false;
                let issued = match &mut *live {
                    LiveState::Ready(session) => {
                        // Cheap, non-blocking drain of any nvim redraw traffic that arrived since
                        // the last tick.
                        session.harness.pump(Duration::ZERO);
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
                        let width = widget.width() * widget.scale_factor();
                        let height = widget.height() * widget.scale_factor();
                        let content_region = compute_content_region(width, height);
                        let new_grid_size = grid_size_for_content_region(&session.harness, &content_region);
                        let grid_resynced = if new_grid_size != session.last_grid_size.get() {
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
                        if session.harness.has_neovim_exited() && !session.close_requested.replace(true)
                        {
                            should_fire_exited_callback = true;
                        }

                        session.last_animating.get() || new_content || wants_frame
                    }
                    // NotStarted/Starting: the placeholder-frame dance and the one blocking
                    // `LiveHarness::with_options` call both happen *inside* the render callback
                    // and only run when a render is actually requested -- keep rendering
                    // continuously here so that state machine can advance. Failed: a rare
                    // terminal state; keep rendering rather than risk the one remaining
                    // Starting->Failed state-transition frame never actually getting painted.
                    LiveState::NotStarted | LiveState::Starting | LiveState::Failed(_) => true,
                };
                drop(live);

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

        Self { widget: gl_area, im_context, live_state, exited_callback }
    }

    /// The `GtkGLArea` this pane renders into. The host places this into its own window (e.g. as
    /// an `ApplicationWindow`'s child) -- this pane never does so itself.
    pub fn widget(&self) -> &gtk4::GLArea {
        &self.widget
    }

    /// Grabs keyboard focus for this pane's `GLArea` and tells the IME it's now the focused
    /// widget -- call this once the host's window is actually shown (mirroring the reference
    /// probe's own `window.present(); gl_area.grab_focus(); im_context.focus_in();` sequence,
    /// minus the `.present()`, which is the host's own window's job).
    pub fn grab_focus(&self) {
        self.widget.grab_focus();
        self.im_context.focus_in();
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

    /// Cleanly shuts down the live `nvim --embed` connection, if one was ever started. Mirrors
    /// the reference probe's `connect_close_request` handler body exactly: the host wires this to
    /// its own window's `connect_close_request`. Returns `LiveHarness::shutdown()`'s own return
    /// value (`true` if a real `NeovimExited` was observed, `false` if it timed out waiting for
    /// one and the nvim child may be orphaned) -- or `true` if `LiveState` never reached `Ready`
    /// at all, since there is no live connection to shut down in that case.
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
                    "timed out waiting for NeovimExited -- nvim child may be orphaned, see \
                     LiveHarness::shutdown's own doc"
                }
            );
            exited_cleanly
        } else {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::nvim_args;

    #[test]
    fn clean_comes_first_and_extra_arguments_follow_in_order() {
        let extra = vec!["--cmd".to_string(), "lua print(1)".to_string()];
        assert_eq!(nvim_args(true, &extra), vec!["--clean", "--cmd", "lua print(1)"]);
        assert_eq!(nvim_args(false, &extra), vec!["--cmd", "lua print(1)"]);
        assert_eq!(nvim_args(true, &[]), vec!["--clean"]);
        assert!(nvim_args(false, &[]).is_empty());
    }
}
