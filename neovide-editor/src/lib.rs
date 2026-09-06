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
use skia_safe::{Color4f, Paint, PaintStyle, Rect};

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

/// Painted by us into the full framebuffer before each `LiveHarness::render_frame` call. A color
/// the renderer would never itself produce, so a viewport-clear regression bleeding past
/// `gl_interop::CONTENT_MARGIN` is visible at a glance.
const OUTSIDE_COLOR: Color4f = Color4f::new(0.55, 0.15, 0.55, 1.0);
const BORDER_COLOR: Color4f = Color4f::new(0.95, 0.85, 0.25, 1.0);
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

/// Counts, over rolling windows of `TICK_LOG_EVERY_N_TICKS` tick-callback invocations, how many
/// ticks actually issued a `queue_render()` vs. how many skipped it because nothing needed another
/// frame -- an idle pane should show `skipped` dominating, while a typing/scrolling/animating pane
/// should show `issued` dominating. See `poc/p11_measurements/PHASE_REPORT.md` for the bug this
/// directly addresses.
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

impl NeovideEditorPane {
    /// Builds the `GtkGLArea` and wires every resize/IME/keyboard/mouse/tick/render callback onto
    /// it and the shared `Rc<RefCell<LiveState>>` -- everything `poc/neovide_embed_live::build_ui`
    /// did except constructing an `Application`/`ApplicationWindow` and calling
    /// `.present()`/`.grab_focus()` (the host does those, via `.widget()`/`.grab_focus()` below).
    /// `clean` is forwarded to `LiveHarnessOptions::extra_nvim_args` as `--clean`, for
    /// deterministic manual-verification runs.
    pub fn new(clean: bool) -> Self {
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

        // --- resize: GtkGLArea's FBO can be resized/recreated under us, so drop the cached
        // Surface and let the next render() rebuild it against the new framebuffer dimensions
        // (device pixels, not logical widget units). Also the P2 frozen-scroll-bug fix:
        // recompute the grid size that actually fits the new content_region and, if it differs
        // (in grid *cells*, not raw pixels) from what nvim was last told, call
        // `LiveHarness::resize_grid` so nvim's own viewport tracks the real host size.
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
                state.ensure_surface();

                let Some(surface) = state.surface.as_mut() else {
                    return glib::Propagation::Stop;
                };

                let (fb_w, fb_h) = (state.fb_width, state.fb_height);
                let content_region = compute_content_region(fb_w, fb_h);
                let canvas = surface.canvas();

                // Paint the *entire* framebuffer a color the renderer would never itself produce,
                // then hand only the inset `content_region` to whatever's actually drawing this
                // frame -- viewport-containment check.
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
                            extra_nvim_args: if clean {
                                vec!["--clean".to_string()]
                            } else {
                                Vec::new()
                            },
                            ..Default::default()
                        };
                        println!(
                            "[live] constructing LiveHarness::with_options(os_scale_factor={os_scale_factor}, \
                             clean={clean}) -- this performs a real, synchronous nvim launch and \
                             WILL block the GTK main loop until it returns"
                        );
                        let t0 = Instant::now();
                        match LiveHarness::with_options(options) {
                            Ok(mut harness) => {
                                let elapsed = t0.elapsed();
                                println!(
                                    "[live] LiveHarness::with_options returned after {elapsed:?} \
                                     (blocked the GTK main loop for that long)"
                                );
                                // The P2 frozen-scroll-bug fix: `LiveHarnessOptions::grid_size`
                                // (left `None` above, so `DEFAULT_GRID_SIZE` 100x50) only ever
                                // sets nvim's grid size at `nvim_ui_attach` time and has no
                                // relationship to this pane's actual `content_region` -- resize
                                // it immediately to what the real content_region fits.
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

                // Border traces exactly where `content_region` is, so a bleed (or a misplaced
                // region) is visible at a glance, in every LiveState.
                let mut border_paint = Paint::default();
                border_paint.set_anti_alias(true);
                border_paint.set_style(PaintStyle::Stroke);
                border_paint.set_stroke_width(2.0);
                border_paint.set_color4f(BORDER_COLOR, None);
                canvas.draw_rect(
                    Rect::from_ltrb(
                        content_region.min.x,
                        content_region.min.y,
                        content_region.max.x,
                        content_region.max.y,
                    ),
                    &border_paint,
                );

                state.gr_context.flush_and_submit();

                glib::Propagation::Stop
            });
        }

        // --- drive redraws off the display's frame clock, tying frame pacing to actual
        // vsync-reported timing rather than a fixed timer -- but, per the P11 fix, only actually
        // calls `queue_render()` when something genuinely needs another frame.
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
            gl_area.add_tick_callback(move |_widget, _clock| {
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

                        // Read-and-clear: a resize or a keypress since the last tick each force
                        // exactly one more frame, on top of the ongoing-animation and new-content
                        // signals above.
                        let wants_frame = session.wants_frame.replace(false);

                        // The P2 "dead-looking-alive pane" bugfix: `shutdown()` already calls
                        // `LiveHarness::shutdown()` when the host closes its own window, but
                        // nothing previously reacted when nvim exits *on its own* (e.g. `:qa!`
                        // typed inside it). `pump()` just above is what actually observes a real
                        // `UserEvent::NeovimExited` arriving asynchronously, so checking right
                        // here, every tick, catches it at the same latency the frame log already
                        // did. `close_requested.replace(true)` is the one-shot guard documented on
                        // `LiveSession::close_requested`; only the tick that flips it false->true
                        // actually fires the callback.
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
