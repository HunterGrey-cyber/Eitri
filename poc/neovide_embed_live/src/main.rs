//! GTK4 probe embedding `LiveHarness` in a focusable `GtkGLArea` backed by a Skia surface.
//! `LiveHarness` connects to a real `nvim --embed` child. `GtkEventControllerKey` forwards
//! printable characters and Escape, Return, BackSpace, and Tab through `send_text_input`.
//! Ctrl, Alt, and Super combinations are ignored to avoid sending bare characters for shortcuts.
//! `LiveHarness::with_options` blocks while launching nvim and establishing the RPC session.
//! A starting placeholder is painted and presented before construction on the next callback,
//! so the user sees an explanation while the GTK main loop is blocked. The per-frame pump
//! inside `render_frame` uses `Duration::ZERO` and does not need that mitigation.
//! The close-request handler calls `LiveHarness::shutdown()` and logs the result to clean up
//! the embedded child when the window closes.
//! The tick callback also closes the window when `has_neovim_exited()` becomes true,
//! so quitting nvim directly does not leave an unresponsive window displaying the last frame.

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

/// Number of `add_tick_callback` invocations between `[tick]` summary log lines.
/// Ticks follow the display refresh rate, so 300 ticks gives a readable summary interval
/// without logging on every frame.
const TICK_LOG_EVERY_N_TICKS: u64 = 300;

/// Inset in device pixels between the framebuffer edge and the `content_region` passed to
/// `LiveHarness::render_frame`. The margin makes drawing outside the viewport or using a
/// misplaced region visible.
const CONTENT_MARGIN: f32 = 40.0;

/// Color painted across the framebuffer before each `LiveHarness::render_frame` call.
/// The renderer does not produce this color, so clearing beyond `CONTENT_MARGIN` is visible.
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

/// Converts widget-local logical pixels to a grid cell using `content_region` and `grid_scale`.
/// Multiply by `scale_factor` to get device pixels, subtract `content_region.min`, divide by
/// `grid_scale`, then floor and clamp to `[0, grid_size - 1]`.
/// Plain value arguments keep the coordinate math independent of harness state and easy to test.
/// Callers use `harness.grid_scale()` and `harness.get_grid_size()` so clamping follows the grid
/// confirmed by nvim's redraw traffic, rather than a pending resize request.
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
    /// `render_frame`'s returned `animating` value, updated after every render callback.
    /// Starts `true` so the tick callback keeps rendering through the first Ready-state frames
    /// until `render_frame` reports whether another frame is needed.
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
    /// Grid size in columns and rows last requested through `LiveHarness::resize_grid`.
    /// Initialized at construction and checked on every `connect_resize` callback so a resize RPC
    /// is sent only when the cell dimensions change, avoiding redundant calls during pixel-level drags.
    last_grid_size: Cell<GridSize<u32>>,
    /// Set when the tick callback requests `window.close()` after nvim exits on its own.
    /// `has_neovim_exited()` stays true, so this guard prevents repeated close requests, logs,
    /// and shutdown calls while the window is tearing down. `Cell::replace` checks and sets
    /// the flag in one operation on the GTK main loop.
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

/// Counts issued and skipped `queue_render()` calls over windows of
/// `TICK_LOG_EVERY_N_TICKS` tick callbacks. Idle windows should mostly skip rendering;
/// typing, scrolling and animation should cause more frames to be issued.
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

/// Handles `EventControllerScroll` deltas using the same accumulation as
/// `MouseManager::handle_line_scroll` and `handle_pixel_scroll`. `controller.unit()`
/// distinguishes wheel-notch units (`Wheel`) from device pixels (`Surface`). Surface
/// deltas are divided by `grid_scale` before being added to `session.scroll_position`.
/// Changes in the floored position determine how many whole-line `Scroll` RPCs to send.
///
/// Positive GDK deltas map to nvim's `"down"` and `"right"`. Synthetic virtual-pointer
/// scrolls with `axis_source(Wheel)` moved the viewport in both directions and back.
/// Physical wheel direction remains unverified; an inverted direction would require
/// swapping the `Greater` and `Less` arms below.
///
/// In the sandbox, `wlrctl pointer scroll` sends `axis` and `frame` without `axis_source`.
/// The GDK/Wayland backend drops those events before this handler is called.
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

    // In the sandbox, synthetic virtual-pointer scrolls with `axis_source=Wheel` arrive as
    // `ScrollUnit::Surface`. A simulated notch of about 10 pixels becomes less than half a
    // grid line, so several notches may be needed for one `Scroll` RPC. Scrolling still moves
    // the viewport in both directions. Surface deltas use pixel units and must be divided
    // by `grid_scale`, matching `handle_pixel_scroll`. Physical wheel classification remains
    // unverified, so the synthetic result alone does not justify changing this conversion.
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

    // GtkGLArea may resize or recreate its FBO, so drop the cached Surface and rebuild it on
    // the next render with the new framebuffer dimensions in device pixels. This also supplies
    // an updated `content_region` to `LiveHarness::render_frame`.
    {
        let skia_state = skia_state.clone();
        let live_state = live_state.clone();
        gl_area.connect_resize(move |widget, width, height| {
            // Request a frame at the new size even if nvim has sent no new content.
            // Recompute the grid dimensions that fit `content_region` and call `LiveHarness::resize_grid`
            // only when they differ in cells from the last requested size, so nvim's viewport tracks
            // the available space instead of remaining at its launch size.
            // `borrow_mut()` is required because `resize_grid` takes `&mut LiveHarness`.
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

    // Attach `GtkIMMulticontext` with `set_im_context` so GTK filters key events before
    // emitting `key-pressed`. Keys consumed during composition never reach the plain-text
    // handler, preventing double forwarding. Composed text arrives through `connect_commit`
    // and uses the same `send_text_input` path as plain keystrokes, as UTF-8 text.
    // `set_client_widget` and `focus_in()`, paired with `grab_focus()` below, give fcitx5 an
    // anchor for its candidate window. This crate draws no preedit indicator of its own.
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

    // Three GTK4 controllers handle clicks, drag selection, and scrolling on the GLArea.
    // They share `pixel_to_grid_pos` and `current_content_region` for coordinate conversion and
    // target `LiveHarness`'s single base grid; this crate has no per-window or split hit testing.
    // `format_modifier_string` reads GTK's live `ModifierType` for each event.
    // `click.set_button(0)` accepts any button. `handle_mouse_button` reads `current_button()`
    // because the pressed and released signals do not pass a button argument.
    // Double and triple clicks have no separate handling here.
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

            // Paint the entire framebuffer a color the renderer never produces, then draw only inside
            // the inset `content_region`. This makes drawing outside the viewport visible.
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
                            // `LiveHarnessOptions::grid_size` only sets the grid at `nvim_ui_attach` time; leaving it
                            // `None` uses the 100x50 default regardless of this window's `content_region`.
                            // Resize immediately to the grid that fits the content region so the first frame has the
                            // correct viewport, even if the user never triggers a later `connect_resize` event.
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

    // Drive redraws from the display's frame clock to pace frames with vsync.
    // Call `queue_render()` only when another frame is needed. Requesting a render on every
    // tick keeps the shell's CPU and GPU busy even when nvim itself is idle.
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

                    // `pump()` observes asynchronous `UserEvent::NeovimExited` messages. Check for exit every
                    // tick so quitting nvim directly closes the window instead of leaving its last frame visible.
                    // `close_requested.replace(true)` permits only the first close request, avoiding repeated
                    // shutdown calls while the window is closing.
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

/// Tests for the pure mouse-input helpers, without a GTK/GLib runtime.
/// These check coordinate math, clamping and modifier formatting. They do not exercise
/// GTK signals or end-to-end click, drag and scroll delivery in a Wayland session.
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
