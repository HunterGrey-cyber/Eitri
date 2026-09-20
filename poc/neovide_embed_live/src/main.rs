//! P2 feasibility probe (see docs/neovibe_feasibility_validation.md §5):
//!
//!     GtkApplicationWindow
//!     └── GtkGLArea (focusable, receives keyboard input)
//!         └── Skia Surface
//!             └── neovide::live_harness::LiveHarness
//!                 (patched Neovide fork, neovibe-integration branch)
//!                 └── real `nvim --embed` child process
//!
//! Direct sibling of `poc/neovide_embed` (P1): same GTK4 + `GtkGLArea` + Skia-wrapping-the-FBO
//! plumbing (`SkiaState`, `resolve_gl_proc`, `make_gl_interface`, `current_bound_framebuffer`,
//! `compute_content_region`'s resizable-viewport handling, the frame-pacing log) copied over
//! essentially unchanged -- see that crate's own doc comments for the full rationale on the
//! GL/Skia interop, which is not repeated here. The only architectural addition is real input:
//! this crate wires `GtkEventControllerKey` into `LiveHarness::send_text_input`, so a real
//! `nvim --embed` connection receives real keystrokes typed into the GTK window.
//!
//! ## What's new vs. P1
//!
//! - `neovide::demo_harness::DemoHarness` (fabricated content, no nvim) is replaced by
//!   `neovide::live_harness::LiveHarness` (real `nvim --embed` connection) per the P2 surgery
//!   phase's report.
//! - `GtkEventControllerKey` is attached to the `GLArea` (which is made focusable and grabs focus
//!   on startup) and forwards basic printable-character input --
//!   `gdk::Key::to_unicode()` plus a handful of named keys (Escape/Return/BackSpace/Tab) common
//!   enough that a human can actually drive nvim (enter insert mode, type, get back out, correct
//!   typos) -- through to `LiveHarness::send_text_input`. This is deliberately *not* full key-
//!   code/modifier translation fidelity (that is a later, dedicated input-system phase): any
//!   non-Shift modifier (Ctrl/Alt/Super) held down is treated as "not plain text" and ignored
//!   rather than forwarded, to avoid silently sending e.g. bare `c` for Ctrl+C.
//! - `LiveHarness::with_options` performs a real, synchronous, *blocking* call into
//!   `NeovimRuntime::launch` (spawns the child process and waits for the msgpack-rpc session to
//!   be established) -- unlike `DemoHarness::new`, which does no I/O at all. Per this phase's own
//!   task background (citing `poc/pump_events_spike/FINDINGS.md`'s "nothing on the shared GLib
//!   thread may block" constraint), that call is *not* hidden inside the same `render()` callback
//!   that first needs it: a "starting nvim..." placeholder frame is painted and presented first,
//!   and the actual (blocking) construction happens on the *next* callback, so the freeze -- which
//!   is real and was observed, see this crate's own `MANUAL_VERIFICATION.md` -- at least happens
//!   after the user sees an explanatory frame rather than a silently-frozen blank/garbage window.
//!   `LiveHarness::render_frame`'s own internal per-frame pump is separately confirmed
//!   non-blocking (`NON_BLOCKING = Duration::ZERO` in the harness's own source) and needs no such
//!   mitigation.
//! - On window close, `LiveHarness::shutdown()` is called and its return value logged, so a real
//!   `nvim --embed` child is never left orphaned behind this probe (see the `connect_close_request`
//!   handler below, and this crate's `MANUAL_VERIFICATION.md` for the `pgrep` diff used to confirm
//!   it empirically).
//! - **Fixed 2026-09-06**: the tick callback now also closes the window itself
//!   (`window.close()`) when `LiveHarness::has_neovim_exited()` comes back true *without* that
//!   having gone through `connect_close_request` first -- i.e. nvim quit on its own (`:qa!` typed
//!   inside it), not via a user closing this window. Before this fix, that case left a dead-
//!   looking-alive window on screen forever (last frame still rendering, all input silently
//!   swallowed with nowhere to route it) instead of exiting like stock Neovide does when its own
//!   nvim quits. See the tick callback's own comment for the mechanism and this crate's
//!   `MANUAL_VERIFICATION.md` for the bug's original discovery and this fix's verification.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::gdk::{Key, ModifierType, ScrollUnit};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, EventControllerKey, EventControllerMotion, EventControllerScroll,
    EventControllerScrollFlags, GLArea, GestureClick, IMMulticontext,
};

use skia_safe::gpu::gl::{Format as GlFormat, FramebufferInfo, Interface as GlInterface};
use skia_safe::gpu::{backend_render_targets, direct_contexts, surfaces, DirectContext, SurfaceOrigin};
use skia_safe::{Canvas, Color4f, Paint, PaintStyle, Rect, Surface};

use neovide::live_harness::{LiveHarness, LiveHarnessOptions};
use neovide::units::{GridScale, GridSize, PixelRect, PixelSize};

const APP_ID: &str = "cn.huntergrey.neovibe.neovide_embed_live";

/// Log a frame-pacing line every N frames instead of spamming stdout every frame.
const LOG_EVERY_N_FRAMES: u64 = 60;

/// How many `add_tick_callback` invocations between `[tick]` summary log lines (see `TickStats`).
/// The tick callback fires once per display frame (~144-165Hz on the dev machine this was
/// measured on per `poc/p11_measurements/PHASE_REPORT.md`), so 300 ticks is roughly a 2s window --
/// frequent enough to see idle-vs-active behavior change within a couple of seconds in the log,
/// without spamming stdout at display refresh rate.
const TICK_LOG_EVERY_N_TICKS: u64 = 300;

/// Inset (device pixels) between the GtkGLArea's own framebuffer edge and the rect handed to
/// `LiveHarness::render_frame` as `content_region`. Same rationale/value as P1's
/// `neovide_embed::CONTENT_MARGIN` -- a non-zero margin makes a viewport-containment bleed (or a
/// misplaced region) immediately visible instead of silently passing.
const CONTENT_MARGIN: f32 = 40.0;

/// Painted by us into the full framebuffer before each `LiveHarness::render_frame` call. Same
/// role as P1's `OUTSIDE_COLOR`: a color the renderer would never itself produce, so a viewport-
/// clear regression bleeding past `CONTENT_MARGIN` is visible at a glance.
const OUTSIDE_COLOR: Color4f = Color4f::new(0.55, 0.15, 0.55, 1.0);
const BORDER_COLOR: Color4f = Color4f::new(0.95, 0.85, 0.25, 1.0);
/// Painted across the whole `content_region` while `LiveHarness` is being constructed (the one
/// real, observed blocking call in this crate -- see this file's module doc) -- distinct from
/// both `OUTSIDE_COLOR` and anything the real renderer would draw, so it's obvious on screen
/// which phase is showing.
const STARTING_COLOR: Color4f = Color4f::new(0.12, 0.12, 0.16, 1.0);
/// Painted across `content_region` if `LiveHarness::with_options` itself returned `Err` (e.g. no
/// `nvim` on `$PATH`) -- distinct from every other state color here.
const FAILED_COLOR: Color4f = Color4f::new(0.5, 0.05, 0.05, 1.0);

/// GL-context-bound Skia state. Identical in spirit/implementation to
/// `neovide_embed::SkiaState` -- see that crate's own doc comment for the full rationale; nothing
/// about Skia/GL wrapping changes for a live nvim connection vs. the demo harness.
struct SkiaState {
    gr_context: DirectContext,
    surface: Option<Surface>,
    fb_width: i32,
    fb_height: i32,
}

impl SkiaState {
    fn ensure_surface(&mut self) {
        if self.surface.is_some() {
            return;
        }
        if self.fb_width <= 0 || self.fb_height <= 0 {
            return;
        }

        let fboid = current_bound_framebuffer();
        let fb_info = FramebufferInfo {
            fboid: fboid as u32,
            format: GlFormat::RGBA8.into(),
            ..Default::default()
        };

        let render_target = backend_render_targets::make_gl(
            (self.fb_width, self.fb_height),
            0, // sample_count: no explicit MSAA, GtkGLArea isn't configured for it
            8, // stencil_bits: GLArea is built with has_stencil_buffer(true)
            fb_info,
        );

        let surface = surfaces::wrap_backend_render_target(
            &mut self.gr_context,
            &render_target,
            SurfaceOrigin::BottomLeft,
            skia_safe::ColorType::RGBA8888,
            None,
            None,
        )
        .expect("failed to wrap GtkGLArea framebuffer as a Skia Surface");

        self.surface = Some(surface);
    }
}

/// Carried over verbatim from `neovide_embed::current_bound_framebuffer` (itself carried over from
/// `gl_skia_test`) -- see that crate's doc comment for the full nm -D-verified explanation of the
/// local libepoxy quirk this works around.
fn current_bound_framebuffer() -> i32 {
    const GL_FRAMEBUFFER_BINDING: u32 = 0x8CA6;
    unsafe {
        let lib = libloading::os::unix::Library::this();
        let proc_addr = resolve_gl_proc(&lib, "glGetIntegerv");
        if proc_addr.is_null() {
            return 0;
        }
        let get_integerv: unsafe extern "C" fn(u32, *mut i32) = std::mem::transmute(proc_addr);
        let mut fbo: i32 = 0;
        get_integerv(GL_FRAMEBUFFER_BINDING, &mut fbo as *mut i32);
        fbo
    }
}

/// Carried over verbatim from `neovide_embed::make_gl_interface`.
fn make_gl_interface() -> GlInterface {
    unsafe {
        let lib = libloading::os::unix::Library::this();
        GlInterface::new_load_with(move |name: &str| resolve_gl_proc(&lib, name))
            .expect("failed to assemble Skia GL interface via process-wide symbol lookup")
    }
}

/// Carried over verbatim from `neovide_embed::resolve_gl_proc`.
unsafe fn resolve_gl_proc(lib: &libloading::os::unix::Library, name: &str) -> *const std::ffi::c_void {
    unsafe {
        if let Ok(epoxy_name) = std::ffi::CString::new(format!("epoxy_{name}")) {
            if let Ok(sym) = lib.get::<*const std::ffi::c_void>(epoxy_name.as_bytes_with_nul()) {
                let slot: *const *const std::ffi::c_void = *sym as *const *const std::ffi::c_void;
                if !slot.is_null() {
                    let fn_ptr = *slot;
                    if !fn_ptr.is_null() {
                        return fn_ptr;
                    }
                }
            }
        }
        if let Ok(cname) = std::ffi::CString::new(name) {
            if let Ok(sym) = lib.get::<*const std::ffi::c_void>(cname.as_bytes_with_nul()) {
                return *sym;
            }
        }
        std::ptr::null()
    }
}

/// The pixel rect (within the GLArea's own framebuffer) handed to `LiveHarness::render_frame` as
/// `content_region`, inset from the framebuffer edges by `CONTENT_MARGIN` on every side (falling
/// back to the full framebuffer if it's too small for that margin to make sense). Identical to
/// `neovide_embed::compute_content_region`.
fn compute_content_region(fb_width: i32, fb_height: i32) -> PixelRect<f32> {
    let (w, h) = (fb_width as f32, fb_height as f32);
    if w > CONTENT_MARGIN * 2.0 + 20.0 && h > CONTENT_MARGIN * 2.0 + 20.0 {
        PixelRect::from_min_max(
            (CONTENT_MARGIN, CONTENT_MARGIN),
            (w - CONTENT_MARGIN, h - CONTENT_MARGIN),
        )
    } else {
        PixelRect::from_min_max((0.0, 0.0), (w.max(1.0), h.max(1.0)))
    }
}

/// The (cols, rows) grid size that fits inside `content_region`, using `harness`'s own current
/// font-derived `grid_scale` (`LiveHarness::grid_scale`) -- floored, not rounded, so nvim's grid
/// never claims more space than the host actually has, the same convention neovide's own
/// `WinitWindowWrapper::get_grid_size_from_window` uses when deriving a grid size from a real OS
/// window's pixel content area. `LiveHarness::resize_grid` clamps again internally
/// (`settings::clamped_grid_size`), but flooring here first keeps this value an honest reflection
/// of what was actually computed from `content_region`, for the caller's own dedup/log purposes.
fn grid_size_for_content_region(harness: &LiveHarness, content_region: &PixelRect<f32>) -> GridSize<u32> {
    let pixel_size = PixelSize::new(
        content_region.max.x - content_region.min.x,
        content_region.max.y - content_region.min.y,
    );
    let grid_size = pixel_size / harness.grid_scale();
    GridSize::new(
        grid_size.width.floor().max(1.0) as u32,
        grid_size.height.floor().max(1.0) as u32,
    )
}

/// nvim's own button-text notation for a GDK button number, mirroring the reference
/// `neovide::window::mouse_manager::mouse_button_to_button_text`'s winit-`MouseButton`-keyed
/// equivalent. GDK numbers buttons in the X11 convention: 1=left/primary, 2=middle, 3=right/
/// secondary, 8=back, 9=forward. Anything else (e.g. an extra side button some mice report
/// differently) is not forwarded -- same "silently drop, don't guess" stance the reference takes
/// for a `MouseButton` variant its own match doesn't cover.
fn gdk_button_to_button_text(button: u32) -> Option<&'static str> {
    match button {
        1 => Some("left"),
        2 => Some("middle"),
        3 => Some("right"),
        8 => Some("x1"),
        9 => Some("x2"),
        _ => None,
    }
}

/// nvim's own modifier-prefix notation (`"S-"`/`"C-"`/`"M-"`/`"D-"`, concatenated in that order)
/// for a GDK modifier snapshot -- mirrors
/// `neovide::window::keyboard_manager::KeyboardManager::format_modifier_string` called with
/// `is_special = true` the way every mouse RPC in the reference `MouseManager` calls it (mouse
/// events, like special keys, always include Shift when held rather than only when combined with
/// Ctrl+ASCII -- see that method's own doc for why). Unlike the reference, which reads a
/// `winit::keyboard::ModifiersState` this crate never accumulates (see the keyboard controller's
/// own module-doc caveat on modifier fidelity), this reads GTK's own live `ModifierType` for
/// whichever event is currently being handled
/// (`EventControllerExt::current_event_state`) -- sufficient for a mouse command, which is always
/// built and sent synchronously from inside the one GTK signal callback that observed it.
fn format_modifier_string(state: ModifierType) -> String {
    let mut modifiers = String::new();
    if state.contains(ModifierType::SHIFT_MASK) {
        modifiers.push_str("S-");
    }
    if state.contains(ModifierType::CONTROL_MASK) {
        modifiers.push_str("C-");
    }
    if state.contains(ModifierType::ALT_MASK) {
        modifiers.push_str("M-");
    }
    if state.contains(ModifierType::SUPER_MASK) {
        modifiers.push_str("D-");
    }
    modifiers
}

/// Converts a widget-local *logical*-pixel position -- exactly what GTK4's `GestureClick`/
/// `EventControllerMotion`/`EventControllerScroll` all report their `x`/`y` in -- into the (col,
/// row) grid cell it lands on, reusing the exact content_region/grid-scale relationship
/// `grid_size_for_content_region` already uses for the P2 resize fix rather than reimplementing
/// it: `logical * scale_factor` first converts into the same device-pixel space `content_region`
/// itself is expressed in (mirroring every other device-pixel user in this file, e.g. the initial
/// `SkiaState` construction's own `widget.width() * widget.scale_factor()`), then
/// `(pixel - content_region.min) / grid_scale`, floored and clamped to `[0, grid_size - 1]` --
/// precisely `MouseManager::get_relative_position_at`'s own formula (see this crate's own task
/// background for that reference). Callers pass `harness.grid_scale()`/`harness.get_grid_size()`
/// straight through (rather than this function taking `&LiveHarness` itself) so the coordinate
/// math stays a pure function of plain values -- independently unit-testable below, and clearly
/// separated from *which* harness state a caller chooses to clamp against. Every call site in
/// this crate clamps against `get_grid_size()` (the grid size nvim's own redraw traffic last
/// actually confirmed), not merely the size last *requested* via `resize_grid`, matching how the
/// reference clamps against a window's own reported `grid_size` rather than a pending resize
/// target.
fn pixel_to_grid_pos(
    logical_x: f64,
    logical_y: f64,
    scale_factor: i32,
    content_region: &PixelRect<f32>,
    grid_scale: GridScale,
    grid_size: GridSize<u32>,
) -> (u32, u32) {
    let scale_factor = scale_factor as f32;
    let pixel_x = logical_x as f32 * scale_factor - content_region.min.x;
    let pixel_y = logical_y as f32 * scale_factor - content_region.min.y;

    let grid_x = (pixel_x / grid_scale.width()).floor().max(0.0) as u32;
    let grid_y = (pixel_y / grid_scale.height()).floor().max(0.0) as u32;

    (
        grid_x.min(grid_size.width.max(1) - 1),
        grid_y.min(grid_size.height.max(1) - 1),
    )
}

/// `content_region` computed from `gl_area`'s own *current* framebuffer size -- the same
/// `widget.width() * widget.scale_factor()` / `widget.height() * widget.scale_factor()`
/// device-pixel conversion the render callback's initial `SkiaState` construction and the resize
/// handler both already use, so a mouse handler reading it between two paints still sees the same
/// rect the next frame will actually draw into (as opposed to reaching into `SkiaState`'s own
/// cached `fb_width`/`fb_height`, which is only ever updated from inside `connect_resize`/
/// `connect_render` and would otherwise need its own borrow here for no benefit).
fn current_content_region(gl_area: &GLArea) -> PixelRect<f32> {
    let scale_factor = gl_area.scale_factor();
    compute_content_region(gl_area.width() * scale_factor, gl_area.height() * scale_factor)
}

fn fill_content_region(canvas: &Canvas, content_region: &PixelRect<f32>, color: Color4f) {
    let mut paint = Paint::default();
    paint.set_color4f(color, None);
    canvas.draw_rect(
        Rect::from_ltrb(
            content_region.min.x,
            content_region.min.y,
            content_region.max.x,
            content_region.max.y,
        ),
        &paint,
    );
}

/// Lifecycle of the `LiveHarness` this window drives, kept explicit (rather than a bare
/// `Option<LiveHarness>`) so the render callback can paint a "starting nvim..." placeholder frame
/// *before* the one real, observed blocking call in this crate (`LiveHarness::with_options`) runs
/// -- see this file's module doc for why that ordering matters. `NotStarted` -> one placeholder
/// frame painted+presented -> `Starting` -> next render callback performs the blocking
/// construction -> `Ready`/`Failed`.
enum LiveState {
    NotStarted,
    Starting,
    Ready(Box<LiveSession>),
    Failed(String),
}

/// Mouse button + last grid cell an ongoing drag was last sent at -- tracked by `LiveSession::
/// active_drag` while a button is held, mirroring `MouseManager::drag_details`. Its presence is
/// what tells the `EventControllerMotion` handler below whether a `Drag` RPC should be sent at
/// all (only while some button is down); its `last_grid_pos` is what lets that handler dedupe on
/// the grid cell actually changing, the same `has_moved` check
/// `MouseManager::handle_pointer_motion` does internally -- see `LiveHarness::send_mouse_drag`'s
/// own doc for why that dedup has to live in the caller here rather than the harness.
#[derive(Clone, Copy)]
struct DragState {
    button: &'static str,
    last_grid_pos: (u32, u32),
}

struct LiveSession {
    harness: LiveHarness,
    start: Instant,
    last_frame: Instant,
    frame_count: u64,
    logged_ready: bool,
    /// `render_frame`'s own returned `animating` value, set after every render callback
    /// invocation below. This is the P11-report-identified signal the tick callback was
    /// previously ignoring -- see `poc/p11_measurements/PHASE_REPORT.md`'s "headline finding".
    /// Starts `true` so the tick callback keeps rendering continuously through the first few
    /// Ready-state frames, until a real `render_frame` call has actually reported a real value --
    /// erring toward "render" rather than "skip" whenever this value hasn't been established yet.
    last_animating: Cell<bool>,
    /// `harness.redraw_batches_seen()` as of the last time either the render callback or the tick
    /// callback looked at it. The tick callback calls `LiveHarness::pump` every tick specifically
    /// so a change here is visible at full display-refresh-rate latency even on ticks that don't
    /// render -- this is the "did nvim actually send anything new" half of the fix, independent of
    /// `last_animating`.
    last_seen_batches: Cell<u64>,
    /// Set by the resize handler and the keyboard input handler: both are real external events
    /// that deserve a guaranteed next frame regardless of what `last_animating`/
    /// `last_seen_batches` currently say (a resize needs its own frame at the new size even if
    /// nvim sent nothing new; a keypress deserves a same-tick-latency render rather than waiting
    /// on nvim's async redraw round-trip to eventually move `last_seen_batches`). Read-and-cleared
    /// by the tick callback every tick.
    wants_frame: Cell<bool>,
    /// The grid (cols, rows) size nvim was last asked to resize to via
    /// `LiveHarness::resize_grid` -- the P2 frozen-scroll-bug fix. Set once at construction (to
    /// whatever the initial `content_region` computed to) and re-checked on every `connect_resize`
    /// callback so a real RPC is only sent when the *grid-cell* size actually changes, not on
    /// every pixel-level resize event mid-drag (see `LiveHarness::resize_grid`'s own doc on why
    /// that dedup matters).
    last_grid_size: Cell<GridSize<u32>>,
    /// Set once the tick callback has asked `window.close()` to run because
    /// `LiveHarness::has_neovim_exited()` came back true on its own (i.e. nvim quit itself, e.g.
    /// via `:qa!` typed inside it -- not via our own `connect_close_request` -> `shutdown()`
    /// path). This is the P2 "dead-looking-alive window" bugfix: `has_neovim_exited()` is
    /// monotonic (never goes back to `false`), so without this guard the tick callback would call
    /// `window.close()` again on every subsequent tick until the window actually finishes tearing
    /// down -- harmless in principle (`GtkWindow::close()` on an already-closing window is not
    /// expected to misbehave) but noisy (a duplicate `[live] nvim exited...` log line and a
    /// duplicate `connect_close_request` -> `shutdown()` call every tick in between). Read-and-set
    /// via `Cell::replace` so the check-and-flag is a single atomic step from this single-threaded
    /// GTK main-loop caller's point of view.
    close_requested: Cell<bool>,
    /// Set on a `GestureClick` press, cleared on its matching release -- see `DragState`'s own
    /// doc for what this tracks and why.
    active_drag: Cell<Option<DragState>>,
    /// Running fractional (x, y) grid-line scroll accumulator, mirroring `MouseManager::
    /// scroll_position` exactly: only the *change* in `floor()` between one scroll event and the
    /// next determines how many whole-grid-line `Scroll` RPCs to send (see the
    /// `EventControllerScroll` handler below), so this is never reset back to zero -- only ever
    /// added to, for the life of the session.
    scroll_position: Cell<(f32, f32)>,
    /// Widget-local logical-pixel pointer position last reported by `EventControllerMotion`'s
    /// `motion`/`enter` signals, mirroring `MouseManager::window_position`. `GtkEventControllerScroll`'s
    /// own `scroll` signal (like winit's `WindowEvent::MouseWheel`) carries no position at all, so
    /// a scroll event's target grid cell has to come from here -- the last place the pointer was
    /// actually seen -- exactly like the reference's own `get_window_details_under_mouse` reads
    /// `self.window_position` rather than anything carried by the wheel event itself.
    last_pointer_pos: Cell<(f64, f64)>,
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

    /// Advance real elapsed time and return (dt, instantaneous_fps), matching
    /// `neovide_embed::DemoState::tick`'s own semantics exactly.
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
/// frame -- this is the fix's own before/after evidence: an idle window should show `skipped`
/// dominating, while a typing/scrolling/animating window should show `issued` dominating. See
/// `poc/p11_measurements/PHASE_REPORT.md` for the bug this directly addresses.
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

/// Shared body for `GestureClick`'s `pressed`/`released` handlers -- see the controller wiring in
/// `build_ui` for how each is attached. `pressed` selects which one this call is for.
///
/// On press: forwards a `MouseButton{action: "press"}` via `LiveHarness::send_mouse_button`, then
/// arms `session.active_drag` so the motion handler below starts sending `Drag` RPCs while this
/// button stays held (mirroring `MouseManager::send_nvim_mouse_button`'s own `self.drag_details =
/// Some(..)` on press). On release: forwards `MouseButton{action: "release"}` at whichever grid
/// cell the drag was last actually at (`active_drag`'s own `last_grid_pos`, if a drag happened) --
/// matching the reference's own `if !down && self.has_moved { self.grid_position } else {
/// self.get_relative_position(..) }` choice of position for a release after a drag -- then
/// disarms `active_drag` regardless.
///
/// `gesture.current_button()` (not a `button` signal argument -- neither `pressed` nor `released`
/// carries one) is nvim's own button-text notation source; a button this crate doesn't recognize
/// (`gdk_button_to_button_text` returning `None`) is silently ignored, same as the reference.
/// `x`/`y` are whatever the firing signal itself handed the caller (both `pressed` and `released`
/// report the pointer position at that instant).
fn handle_mouse_button(
    live_state: &Rc<RefCell<LiveState>>,
    gl_area: &GLArea,
    gesture: &GestureClick,
    x: f64,
    y: f64,
    pressed: bool,
) {
    let Some(button) = gdk_button_to_button_text(gesture.current_button()) else {
        return;
    };
    let modifier_string = format_modifier_string(gesture.current_event_state());

    let mut live = live_state.borrow_mut();
    let LiveState::Ready(session) = &mut *live else {
        return;
    };
    if session.harness.has_neovim_exited() {
        return;
    }

    let content_region = current_content_region(gl_area);
    let position_from_event = pixel_to_grid_pos(
        x,
        y,
        gl_area.scale_factor(),
        &content_region,
        session.harness.grid_scale(),
        session.harness.get_grid_size(),
    );
    let grid_pos = if !pressed {
        // A release after a drag: send it at the drag's own last-known cell, not wherever the
        // pointer happens to sit right now relative to a freshly recomputed content_region --
        // mirrors the reference's `self.has_moved -> self.grid_position` branch exactly.
        match session.active_drag.get() {
            Some(drag) if drag.button == button => drag.last_grid_pos,
            _ => position_from_event,
        }
    } else {
        position_from_event
    };

    session
        .harness
        .send_mouse_button(button, pressed, grid_pos, &modifier_string);
    session.active_drag.set(if pressed {
        Some(DragState {
            button,
            last_grid_pos: grid_pos,
        })
    } else {
        None
    });
    session.wants_frame.set(true);
}

/// `EventControllerMotion`'s `motion` handler -- sends a `Drag` RPC via
/// `LiveHarness::send_mouse_drag` only while `session.active_drag` is armed (some button held,
/// set by `handle_mouse_button` above) and only when the computed grid cell actually differs from
/// `active_drag`'s own `last_grid_pos`, mirroring `MouseManager::handle_pointer_motion`'s combined
/// `drag_details.is_some()` + `has_moved` gate. A plain hover-move with no button held is not
/// forwarded at all -- the reference only does that when `WindowSettings::mouse_move_event` is
/// explicitly enabled (default off), which this crate has no equivalent setting for.
fn handle_mouse_motion(
    live_state: &Rc<RefCell<LiveState>>,
    gl_area: &GLArea,
    controller: &EventControllerMotion,
    x: f64,
    y: f64,
) {
    let mut live = live_state.borrow_mut();
    let LiveState::Ready(session) = &mut *live else {
        return;
    };
    // Recorded on every motion/enter regardless of drag state -- this is the only source of
    // pointer position a subsequent scroll event (which carries none of its own) has. See
    // `LiveSession::last_pointer_pos`'s own doc.
    session.last_pointer_pos.set((x, y));

    let Some(drag) = session.active_drag.get() else {
        return;
    };
    if session.harness.has_neovim_exited() {
        return;
    }

    let content_region = current_content_region(gl_area);
    let grid_pos = pixel_to_grid_pos(
        x,
        y,
        gl_area.scale_factor(),
        &content_region,
        session.harness.grid_scale(),
        session.harness.get_grid_size(),
    );
    if grid_pos == drag.last_grid_pos {
        return;
    }

    let modifier_string = format_modifier_string(controller.current_event_state());
    session.harness.send_mouse_drag(drag.button, grid_pos, &modifier_string);
    session.active_drag.set(Some(DragState {
        button: drag.button,
        last_grid_pos: grid_pos,
    }));
    session.wants_frame.set(true);
}

/// `EventControllerScroll`'s `scroll` handler -- mirrors `MouseManager::handle_line_scroll`/
/// `handle_pixel_scroll` combined: `controller.unit()` (`gdk::ScrollUnit`, always available here
/// since this crate is built with gtk4's `"v4_18"` feature, which pulls in the `"v4_8"` this
/// getter needs) tells us whether `dx`/`dy` are already in wheel-notch units (`Wheel` -- the same
/// semantic as winit's `MouseScrollDelta::LineDelta`, no conversion needed) or raw device pixels
/// (`Surface` -- winit's `PixelDelta` equivalent, divided by `grid_scale` first, exactly like
/// `handle_pixel_scroll` does). Either way the result is accumulated into
/// `session.scroll_position` and only the *change* in `floor()` since the last event decides how
/// many whole-line `Scroll` RPCs to send, in a loop, exactly like the reference.
///
/// GDK's own sign convention for `dy`/`dx` (matching `GtkScrolledWindow`'s adjustment-value
/// convention: a positive delta increases the adjustment, scrolling the view down/right) is
/// mapped to nvim's `"down"`/`"right"` here. **Verified end-to-end in the sandbox (2026-09-06)**:
/// a real `zwlr_virtual_pointer_v1.axis(..., VerticalScroll, +value)` + `axis_source(Wheel)` +
/// `frame()` sequence (see `poc/tools/wlr_vptr_drag/src/scroll.rs`, added this pass) reliably
/// scrolled the real viewport further into the buffer (revealing later lines) and a matching
/// negative value scrolled back to the original position -- both directions round-tripped
/// correctly, confirming this mapping is at least internally consistent and produces the intended
/// effect. **Still open**: whether a real physical wheel notch rotated in a specific physical
/// direction reports positive or negative to GDK on the *real* desktop (as opposed to a value this
/// phase's own test tool chose) is unverified -- if a human's first real scroll comes out
/// inverted, the fix is a one-line swap of the `Greater`/`Less` arms below, not a coordinate-math
/// bug. See this crate's own `MANUAL_VERIFICATION.md` for the full verification writeup, including
/// a real, distinct finding: `wlrctl pointer scroll` (unlike this pass's own `wlr-vptr-scroll`)
/// sends a bare `axis`+`frame` with no `axis_source` at all, which this sandbox's GDK/Wayland
/// backend silently drops entirely (`handle_mouse_scroll` never even gets called) -- a real,
/// specific `wlrctl` tooling gap, not a bug in this handler.
fn handle_mouse_scroll(
    live_state: &Rc<RefCell<LiveState>>,
    gl_area: &GLArea,
    controller: &EventControllerScroll,
    dx: f64,
    dy: f64,
) -> glib::Propagation {
    let mut live = live_state.borrow_mut();
    let LiveState::Ready(session) = &mut *live else {
        return glib::Propagation::Proceed;
    };
    if session.harness.has_neovim_exited() {
        return glib::Propagation::Proceed;
    }

    // NOTE (found during P3 verification, 2026-09-06): in the sandbox, a *synthetic* wheel-sourced
    // scroll delivered via wlr-virtual-pointer (axis_source=Wheel) still arrives here classified as
    // `ScrollUnit::Surface`, not `Wheel` -- confirmed via temporary instrumentation, not assumed.
    // That makes a single simulated "notch" (raw value ~10, libinput's own one-click convention)
    // divide down to under half a grid line, so it takes several notches to produce one real
    // `Scroll` RPC -- end-to-end scrolling still works (verified: the real viewport moves, in the
    // correct direction, round-trips cleanly), just requires more simulated notches than a real
    // wheel might need. This divide-by-`grid_scale` branch is the textbook-correct handling for a
    // genuine `Surface` (pixel-space, e.g. touchpad) delta per GTK4's own documented contract, and
    // mirrors the reference `handle_pixel_scroll` exactly -- left as-is rather than "fixed" against
    // a single sandbox observation, since it's unconfirmed whether a *real* hardware wheel on a
    // *real* desktop reports `Wheel` correctly here (this may well be specific to how wlroots'
    // virtual-pointer protocol forwards axis_source, not a real-hardware behavior) -- see
    // MANUAL_VERIFICATION.md for the full writeup and why this is a "needs a human with a real
    // wheel" open item, not a bug fixed or left broken by guesswork.
    let (mut amount_x, mut amount_y) = (dx as f32, dy as f32);
    if controller.unit() == ScrollUnit::Surface {
        let grid_scale = session.harness.grid_scale();
        amount_x /= grid_scale.width();
        amount_y /= grid_scale.height();
    }

    let content_region = current_content_region(gl_area);
    // `EventControllerScroll` (unlike `GestureClick`/`EventControllerMotion`) never reports the
    // pointer's own x/y at all -- only deltas -- so the grid cell a scroll targets comes from
    // `last_pointer_pos`, the most recent position `EventControllerMotion` observed. See
    // `LiveSession::last_pointer_pos`'s own doc for why that mirrors the reference exactly rather
    // than being a workaround.
    let (px, py) = session.last_pointer_pos.get();
    let grid_pos = pixel_to_grid_pos(
        px,
        py,
        gl_area.scale_factor(),
        &content_region,
        session.harness.grid_scale(),
        session.harness.get_grid_size(),
    );

    let (prev_x, prev_y) = session.scroll_position.get();
    let (new_x, new_y) = (prev_x + amount_x, prev_y + amount_y);
    session.scroll_position.set((new_x, new_y));

    let modifier_string = format_modifier_string(controller.current_event_state());

    let (prev_floor_y, new_floor_y) = (prev_y.floor() as i64, new_y.floor() as i64);
    let vertical_direction = match new_floor_y.cmp(&prev_floor_y) {
        std::cmp::Ordering::Greater => Some("down"),
        std::cmp::Ordering::Less => Some("up"),
        std::cmp::Ordering::Equal => None,
    };
    if let Some(direction) = vertical_direction {
        for _ in 0..(new_floor_y - prev_floor_y).abs() {
            session.harness.send_mouse_scroll(direction, grid_pos, &modifier_string);
        }
    }

    let (prev_floor_x, new_floor_x) = (prev_x.floor() as i64, new_x.floor() as i64);
    let horizontal_direction = match new_floor_x.cmp(&prev_floor_x) {
        std::cmp::Ordering::Greater => Some("right"),
        std::cmp::Ordering::Less => Some("left"),
        std::cmp::Ordering::Equal => None,
    };
    if let Some(direction) = horizontal_direction {
        for _ in 0..(new_floor_x - prev_floor_x).abs() {
            session.harness.send_mouse_scroll(direction, grid_pos, &modifier_string);
        }
    }

    if vertical_direction.is_some() || horizontal_direction.is_some() {
        session.wants_frame.set(true);
    }

    glib::Propagation::Stop
}

fn main() -> glib::ExitCode {
    // Passthrough convenience for manual verification runs, mirroring the fork's own
    // `examples/live_harness_offscreen.rs` choice to opt into `--clean` for determinism -- *not*
    // a change to `LiveHarnessOptions`'s own default, which stays "a real embedding host's actual
    // nvim config" per the surgery report. Pass `--clean` on this binary's own command line
    // (before GTK/glib get a chance to see it) to launch nvim with `--clean` instead.
    let want_clean = std::env::args().any(|arg| arg == "--clean");

    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| build_ui(app, want_clean));
    app.run_with_args::<&str>(&[])
}

fn build_ui(app: &Application, want_clean: bool) {
    let gl_area = GLArea::builder()
        .hexpand(true)
        .vexpand(true)
        .has_stencil_buffer(true)
        .auto_render(true)
        .focusable(true)
        .can_focus(true)
        .build();

    let window = ApplicationWindow::builder()
        .application(app)
        .title("neovibe P2: real nvim --embed (LiveHarness) in GtkGLArea")
        .default_width(1000)
        .default_height(700)
        .child(&gl_area)
        .build();

    let skia_state: Rc<RefCell<Option<SkiaState>>> = Rc::new(RefCell::new(None));
    let live_state: Rc<RefCell<LiveState>> = Rc::new(RefCell::new(LiveState::NotStarted));

    // --- resize: same rationale as neovide_embed's own resize handler -- GtkGLArea's FBO can be
    // resized/recreated under us, so drop the cached Surface and let the next render() rebuild it
    // against the new framebuffer dimensions (device pixels, not logical widget units). This is
    // also what feeds a fresh `content_region` into LiveHarness::render_frame on the very next
    // call -- P1's viewport-containment point, now proven against real nvim redraw traffic.
    {
        let skia_state = skia_state.clone();
        let live_state = live_state.clone();
        gl_area.connect_resize(move |widget, width, height| {
            // A resize is a real event that deserves a guaranteed next frame at the new size --
            // flag it for the tick callback regardless of whether nvim itself has anything new to
            // say (see `LiveSession::wants_frame`'s own doc). Also the P2 frozen-scroll-bug fix:
            // recompute the grid size that actually fits the new content_region and, if it
            // differs (in grid *cells*, not raw pixels) from what nvim was last told, call
            // `LiveHarness::resize_grid` so nvim's own viewport tracks the real host size instead
            // of staying stuck at whatever grid size was in effect at launch -- see
            // `LiveHarness::resize_grid`'s own doc for why that staleness is exactly what froze
            // scrolling. `borrow_mut()` (not the plain `borrow()` this handler used before) is
            // required now: `resize_grid` takes `&mut LiveHarness`, unlike the `Cell`-based
            // `wants_frame` flag alone.
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
                    // GrContext not created yet; render() will pick up the GLArea's current size
                    // directly.
                }
            }
        });
    }

    // --- IME: a GtkIMMulticontext attached to the key controller via `set_im_context`, which
    // makes GTK itself run `gtk_im_context_filter_keypress` on every key event before ever
    // emitting ::key-pressed -- a key an input method consumes (composition in progress) never
    // reaches the plain-text handler below at all, so there is no double-forwarding to guard
    // against here. Composed text arrives separately via `connect_commit`, as one or more whole
    // characters (e.g. a full pinyin syllable's chosen candidate), and is forwarded to nvim
    // through the exact same `send_text_input` path plain keystrokes already use -- nvim sees an
    // IME commit and a literal keystroke identically, both as UTF-8 text. `set_client_widget` +
    // `focus_in()` (paired with `grab_focus()` below) is what lets fcitx5 know where to anchor its
    // own candidate window; this crate draws no on-screen preedit indicator of its own (see
    // MANUAL_VERIFICATION.md for what that leaves unverified).
    let im_context = IMMulticontext::new();
    im_context.set_client_widget(Some(&gl_area));
    {
        let live_state = live_state.clone();
        im_context.connect_commit(move |_ctx, text| {
            let mut live = live_state.borrow_mut();
            if let LiveState::Ready(session) = &mut *live {
                if !session.harness.has_neovim_exited() {
                    session.harness.send_text_input(text);
                    session.wants_frame.set(true);
                }
            }
        });
    }
    im_context.connect_preedit_start(|_ctx| println!("[ime] preedit-start"));
    im_context.connect_preedit_end(|_ctx| println!("[ime] preedit-end"));
    im_context.connect_preedit_changed(|ctx| {
        let (text, _attrs, _cursor_pos) = ctx.preedit_string();
        println!("[ime] preedit-changed: {text:?}");
    });

    // --- keyboard input: GtkEventControllerKey attached directly to the GLArea (made focusable
    // above, grab_focus()'d below once the window is shown). Deliberately not full key-code/
    // modifier translation fidelity -- see this file's module doc -- just enough plain-text input
    // to drive a real nvim buffer: printable characters via `Key::to_unicode()`, plus a handful of
    // named keys common enough to actually use nvim with (Escape to leave insert mode, Return,
    // BackSpace, Tab). Any held Ctrl/Alt/Super is treated as "not plain text" and ignored, rather
    // than silently forwarding e.g. bare `c` for what the user meant as Ctrl+C. IME composition
    // input never reaches this closure at all -- see the `im_context` wiring just above.
    {
        let live_state = live_state.clone();
        let key_controller = EventControllerKey::new();
        key_controller.set_im_context(Some(&im_context));
        key_controller.connect_key_pressed(move |_controller, key, _keycode, state| {
            if state.intersects(ModifierType::CONTROL_MASK | ModifierType::ALT_MASK | ModifierType::SUPER_MASK) {
                return glib::Propagation::Proceed;
            }

            let text: Option<&str> = match key {
                Key::Escape => Some("<Esc>"),
                Key::Return | Key::KP_Enter => Some("<CR>"),
                Key::BackSpace => Some("<BS>"),
                Key::Tab => Some("<Tab>"),
                _ => None,
            };

            let owned_char;
            let text = if let Some(text) = text {
                Some(text)
            } else if let Some(ch) = key.to_unicode() {
                if ch.is_control() {
                    None
                } else {
                    owned_char = ch.to_string();
                    Some(owned_char.as_str())
                }
            } else {
                None
            };

            let Some(text) = text else {
                return glib::Propagation::Proceed;
            };

            let mut live = live_state.borrow_mut();
            if let LiveState::Ready(session) = &mut *live {
                if !session.harness.has_neovim_exited() {
                    session.harness.send_text_input(text);
                    // A keypress deserves a same-tick-latency render rather than waiting on
                    // nvim's async redraw round-trip to eventually move `last_seen_batches`.
                    session.wants_frame.set(true);
                }
            }
            // Handled either way (even pre-Ready/post-exit) -- there is nothing else on this
            // single-widget window that should react to a keypress instead.
            glib::Propagation::Stop
        });
        gl_area.add_controller(key_controller);
    }

    // --- mouse input: three GTK4 controllers on the GLArea for click/drag-selection/scroll-wheel
    // -- the P3 addition this crate previously had none of at all (see this file's module doc).
    // All three share the exact content_region/grid-scale coordinate math this crate already uses
    // for the P2 resize fix (`pixel_to_grid_pos`/`current_content_region` above) rather than
    // reimplementing it, and all three unconditionally target `LiveHarness`'s single base grid --
    // see `LiveHarness::send_mouse_button`'s own doc for why (no per-window/split hit-testing
    // exists in this crate). Modifier handling reads GTK's own live `ModifierType` per event
    // (`format_modifier_string`) rather than a persistent tracker, the same reduced-fidelity
    // stance the keyboard controller above already takes and documents.
    //
    // GestureClick: click.set_button(0) means "any button" (GestureSingle's own convention) so
    // one gesture handles left/right/middle/back/forward uniformly; `handle_mouse_button` reads
    // which one via `gesture.current_button()` since neither `pressed` nor `released` carries a
    // button argument. Double/triple-click is deliberately not hand-rolled here (or in the
    // reference) -- see `handle_mouse_button`'s own doc.
    {
        let click = GestureClick::new();
        click.set_button(0);
        {
            let live_state = live_state.clone();
            let gl_area = gl_area.clone();
            click.connect_pressed(move |gesture, _n_press, x, y| {
                handle_mouse_button(&live_state, &gl_area, gesture, x, y, true);
            });
        }
        {
            let live_state = live_state.clone();
            let gl_area = gl_area.clone();
            click.connect_released(move |gesture, _n_press, x, y| {
                handle_mouse_button(&live_state, &gl_area, gesture, x, y, false);
            });
        }
        gl_area.add_controller(click);
    }

    // EventControllerMotion: tracks `last_pointer_pos` (for the scroll handler, which gets no
    // position of its own) on every motion/enter, and emits `Drag` RPCs while a button is held
    // (armed by the GestureClick handler above) -- see `handle_mouse_motion`'s own doc.
    {
        let motion = EventControllerMotion::new();
        {
            let live_state = live_state.clone();
            let gl_area = gl_area.clone();
            motion.connect_motion(move |controller, x, y| {
                handle_mouse_motion(&live_state, &gl_area, controller, x, y);
            });
        }
        {
            let live_state = live_state.clone();
            let gl_area = gl_area.clone();
            motion.connect_enter(move |controller, x, y| {
                handle_mouse_motion(&live_state, &gl_area, controller, x, y);
            });
        }
        gl_area.add_controller(motion);
    }

    // EventControllerScroll: BOTH_AXES with neither DISCRETE nor KINETIC set gets GDK's own
    // smooth-scroll deltas, tagged with a `gdk::ScrollUnit` (`unit()`, needs the "v4_8" feature
    // this crate's "v4_18" already pulls in) telling us whether they're wheel-notch units or raw
    // surface pixels -- see `handle_mouse_scroll`'s own doc for how each is handled.
    {
        let scroll = EventControllerScroll::new(EventControllerScrollFlags::BOTH_AXES);
        let live_state = live_state.clone();
        let gl_area_for_scroll = gl_area.clone();
        scroll.connect_scroll(move |controller, dx, dy| {
            handle_mouse_scroll(&live_state, &gl_area_for_scroll, controller, dx, dy)
        });
        gl_area.add_controller(scroll);
    }

    // --- render: build (lazily) and drive one LiveHarness frame every tick. See the `LiveState`
    // doc for why construction is deliberately split across two render callbacks instead of
    // happening inline here.
    {
        let skia_state = skia_state.clone();
        let live_state = live_state.clone();
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
                *state_slot = Some(SkiaState {
                    gr_context,
                    surface: None,
                    fb_width: width,
                    fb_height: height,
                });
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
            // frame -- same viewport-containment check as P1, now against real nvim traffic.
            canvas.clear(OUTSIDE_COLOR);

            let mut live = live_state.borrow_mut();
            match &mut *live {
                LiveState::NotStarted => {
                    // Paint+present a placeholder frame *before* the blocking
                    // LiveHarness::with_options call below ever runs (that call happens on the
                    // *next* render callback, once this state transition has actually been
                    // presented to the compositor) -- see this file's module doc.
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
                        extra_nvim_args: if want_clean {
                            vec!["--clean".to_string()]
                        } else {
                            Vec::new()
                        },
                        ..Default::default()
                    };
                    println!(
                        "[live] constructing LiveHarness::with_options(os_scale_factor={os_scale_factor}, \
                         clean={want_clean}) -- this performs a real, synchronous nvim launch and \
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
                            // (left `None` above, so `DEFAULT_GRID_SIZE` 100x50) only ever sets
                            // nvim's grid size at `nvim_ui_attach` time and has no relationship to
                            // this window's actual `content_region` -- resize it immediately to
                            // what the real content_region fits, so nvim's own viewport tracks the
                            // host's real size from the very first frame instead of only being
                            // caught up by the next `connect_resize` event (which may never come,
                            // e.g. if the user never resizes the window at all).
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
                            println!("[live] LiveHarness::with_options failed after {elapsed:?}: {message}");
                            *live = LiveState::Failed(message);
                        }
                    }
                }
                LiveState::Ready(session) => {
                    let (dt, fps) = session.tick();
                    let animating = session.harness.render_frame(canvas, Some(&content_region), dt);
                    // Share this frame's "do we still need more frames" signals with the tick
                    // callback -- the fix this crate exists to validate (see PHASE_REPORT.md).
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

            // Border traces exactly where `content_region` is, so a bleed (or a misplaced region)
            // is visible at a glance, in every LiveState -- same role as in neovide_embed.
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

    // --- drive redraws off the display's frame clock, same tick-callback plumbing as
    // neovide_embed/gl_skia_test (ties frame pacing to actual vsync-reported timing rather than a
    // fixed timer) -- but, per the P11 fix, only actually calls `queue_render()` when something
    // genuinely needs another frame. Previously this unconditionally requested a render on every
    // single tick (~144-165Hz on the P11 dev machine) forever, which is the entire root cause of
    // that phase's idle CPU/GPU finding (see `poc/p11_measurements/PHASE_REPORT.md`) -- `nvim`
    // itself was already idling at 0.0% CPU; only this host-shell tick loop was hot.
    {
        let live_state = live_state.clone();
        let gl_area_for_tick = gl_area.clone();
        // Needed for the nvim-exited-on-its-own fix just below: `window.close()` re-enters
        // `connect_close_request` (see that handler further down), which itself needs
        // `live_state.borrow_mut()` -- so this closure must never call it while still holding
        // `live` borrowed, or GTK's synchronous close-request dispatch would hit a `RefCell`
        // `BorrowMutError` panic against our own still-live borrow. See `should_close_window`
        // below for how that's kept safe.
        let window_for_tick = window.clone();
        let tick_stats = Rc::new(TickStats::new());
        gl_area.add_tick_callback(move |_widget, _clock| {
            let mut live = live_state.borrow_mut();
            // Set from inside the `Ready` arm below, then acted on only *after* `live` is
            // dropped -- see the comment on `window_for_tick` above for why the ordering matters.
            let mut should_close_window = false;
            let issued = match &mut *live {
                LiveState::Ready(session) => {
                    // Cheap, non-blocking drain of any nvim redraw traffic that arrived since the
                    // last tick. This does not touch the GL context or the Skia surface (that's
                    // `render_frame`'s job, called only from the render callback) -- it's safe,
                    // and per `LiveHarness::pump`'s own doc harmless, to call every display-frame
                    // tick regardless of whether this tick ends up rendering. Without this, a
                    // redraw batch nvim sent while we were otherwise idle would sit unapplied
                    // until something else happened to trigger a render.
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

                    // The P2 "dead-looking-alive window" bugfix: `connect_close_request` below
                    // already calls `LiveHarness::shutdown()` when a *user* closes this window,
                    // but nothing previously reacted when nvim exits *on its own* (e.g. `:qa!`
                    // typed inside it) -- `has_neovim_exited()` was read every render-loop frame
                    // purely for the `[frame ...]` log line, never acted on. `pump()` just above
                    // is what actually observes a real `UserEvent::NeovimExited` arriving
                    // asynchronously (see `LiveHarness::user_event`'s handling of that variant),
                    // so checking right here, every tick, catches it at the same latency the
                    // frame log already did -- just now actually doing something about it.
                    // `close_requested.replace(true)` is the one-shot guard documented on
                    // `LiveSession::close_requested`; only the tick that flips it false->true
                    // actually asks the window to close.
                    if session.harness.has_neovim_exited() && !session.close_requested.replace(true) {
                        should_close_window = true;
                    }

                    session.last_animating.get() || new_content || wants_frame
                }
                // NotStarted/Starting: the placeholder-frame dance and the one blocking
                // `LiveHarness::with_options` call both happen *inside* the render callback and
                // only run when a render is actually requested -- keep rendering continuously
                // here so that state machine can advance (see `LiveState`'s own doc). Failed: a
                // rare terminal state; keep rendering rather than risk the one remaining
                // Starting->Failed state-transition frame never actually getting painted (that
                // transition sets the enum variant but doesn't itself paint FAILED_COLOR -- the
                // *next* render call does, in the `Failed` match arm).
                LiveState::NotStarted | LiveState::Starting | LiveState::Failed(_) => true,
            };
            drop(live);

            if should_close_window {
                println!(
                    "[live] nvim exited on its own (not via a user-initiated window close) -- \
                     calling window.close() so the normal connect_close_request -> \
                     LiveHarness::shutdown() path still runs, instead of leaving a dead frame on \
                     screen forever"
                );
                window_for_tick.close();
            }

            if issued {
                gl_area_for_tick.queue_render();
            }
            tick_stats.record(issued);

            glib::ControlFlow::Continue
        });
    }

    // --- shutdown: on window close, ask LiveHarness to cleanly quit its real nvim connection and
    // log whether a real NeovimExited was actually observed (LiveHarness::shutdown's own return
    // value) before letting the window actually close. This blocks for up to 5s in the worst case
    // (LiveHarness::shutdown's own documented wait) -- acceptable on the way out, unlike the
    // startup block this file's module doc discusses, since the app is exiting either way. Only
    // matters when LiveState::Ready was ever reached; NotStarted/Starting/Failed have no live
    // nvim connection to shut down.
    {
        let live_state = live_state.clone();
        window.connect_close_request(move |_window| {
            let mut live = live_state.borrow_mut();
            if let LiveState::Ready(session) = &mut *live {
                println!("[live] window closing: calling LiveHarness::shutdown()...");
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
            }
            glib::Propagation::Proceed
        });
    }

    window.present();
    gl_area.grab_focus();
    im_context.focus_in();
    println!(
        "neovibe P2 probe running (LiveHarness/real nvim --embed in GtkGLArea). \
         initial window scale_factor={} clean={}",
        window.scale_factor(),
        want_clean
    );
}

/// Lightweight sanity checks for this crate's own pure mouse-input helper functions -- deliberately
/// *not* a substitute for real end-to-end verification (real GTK signals firing from a real click/
/// drag/scroll on a real Wayland session, ultimately needing either a human or a resolved
/// synthetic-input story neither of which this phase attempts -- see this crate's own
/// `MANUAL_VERIFICATION.md`). These just pin down the coordinate math and string-formatting logic
/// against no GTK/GLib runtime at all, so a future edit can't silently invert a clamp or drop a
/// modifier bit without a test noticing.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gdk_button_mapping_matches_x11_convention() {
        assert_eq!(gdk_button_to_button_text(1), Some("left"));
        assert_eq!(gdk_button_to_button_text(2), Some("middle"));
        assert_eq!(gdk_button_to_button_text(3), Some("right"));
        assert_eq!(gdk_button_to_button_text(8), Some("x1"));
        assert_eq!(gdk_button_to_button_text(9), Some("x2"));
        // A button code this crate doesn't recognize is silently dropped, not guessed at.
        assert_eq!(gdk_button_to_button_text(4), None);
        assert_eq!(gdk_button_to_button_text(0), None);
    }

    #[test]
    fn modifier_string_orders_shift_control_alt_super() {
        assert_eq!(format_modifier_string(ModifierType::empty()), "");
        assert_eq!(format_modifier_string(ModifierType::CONTROL_MASK), "C-");
        assert_eq!(
            format_modifier_string(ModifierType::SHIFT_MASK | ModifierType::CONTROL_MASK),
            "S-C-"
        );
        assert_eq!(
            format_modifier_string(
                ModifierType::SHIFT_MASK
                    | ModifierType::CONTROL_MASK
                    | ModifierType::ALT_MASK
                    | ModifierType::SUPER_MASK
            ),
            "S-C-M-D-"
        );
    }

    #[test]
    fn pixel_to_grid_pos_accounts_for_content_region_offset_and_scale_factor() {
        let content_region = PixelRect::from_min_max((40.0, 40.0), (940.0, 640.0));
        let grid_scale = GridScale::new(PixelSize::new(9.0, 18.0));
        let grid_size = GridSize::new(100, 33);

        // Right at content_region's own top-left corner -> grid cell (0, 0), not wherever (0, 0)
        // of the raw framebuffer would map to -- this is the whole point of subtracting
        // content_region.min before dividing by grid_scale.
        assert_eq!(
            pixel_to_grid_pos(40.0, 40.0, 1, &content_region, grid_scale, grid_size),
            (0, 0)
        );

        // One cell right/down of that (scale_factor=1, so logical == device pixels here).
        assert_eq!(
            pixel_to_grid_pos(49.0, 58.0, 1, &content_region, grid_scale, grid_size),
            (1, 1)
        );

        // scale_factor=2 (HiDPI): logical (20, 20) is device pixel (40, 40) -- same as the first
        // case above once converted, so still grid cell (0, 0).
        assert_eq!(
            pixel_to_grid_pos(20.0, 20.0, 2, &content_region, grid_scale, grid_size),
            (0, 0)
        );

        // Anything left of/above content_region clamps to 0 rather than underflowing.
        assert_eq!(
            pixel_to_grid_pos(0.0, 0.0, 1, &content_region, grid_scale, grid_size),
            (0, 0)
        );

        // Anything past the grid's own reported size clamps to grid_size - 1, matching
        // `MouseManager::get_relative_position_at`'s own clamp.
        assert_eq!(
            pixel_to_grid_pos(9000.0, 9000.0, 1, &content_region, grid_scale, grid_size),
            (99, 32)
        );
    }
}
