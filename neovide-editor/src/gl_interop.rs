//! GL/Skia interop and content-region math for the editor surface.
//!
//! Extracted from poc/neovide_embed_live as the foundational GL wrapping and coordinate
//! system implementation that all P0–P11 validation phases depend on.

use gtk4::prelude::*;
use gtk4::GLArea;

use skia_safe::gpu::gl::{Format as GlFormat, FramebufferInfo, Interface as GlInterface};
use skia_safe::gpu::{backend_render_targets, surfaces, DirectContext, SurfaceOrigin};
use skia_safe::{Canvas, Color4f, Paint, Rect, Surface};

use neovide::live_harness::LiveHarness;
use neovide::units::{GridScale, GridSize, PixelRect, PixelSize};

/// Inset (device pixels) between the GtkGLArea's own framebuffer edge and the content region.
pub(crate) const CONTENT_MARGIN: f32 = 40.0;

/// GL-context-bound Skia state. Identical in spirit/implementation to
/// `neovide_embed::SkiaState` -- see that crate's own doc comment for the full rationale; nothing
/// about Skia/GL wrapping changes for a live nvim connection vs. the demo harness.
pub(crate) struct SkiaState {
    pub(crate) gr_context: DirectContext,
    pub(crate) surface: Option<Surface>,
    pub(crate) fb_width: i32,
    pub(crate) fb_height: i32,
}

impl SkiaState {
    pub(crate) fn ensure_surface(&mut self) {
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
pub(crate) fn current_bound_framebuffer() -> i32 {
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
pub(crate) fn make_gl_interface() -> GlInterface {
    unsafe {
        let lib = libloading::os::unix::Library::this();
        GlInterface::new_load_with(move |name: &str| resolve_gl_proc(&lib, name))
            .expect("failed to assemble Skia GL interface via process-wide symbol lookup")
    }
}

/// Carried over verbatim from `neovide_embed::resolve_gl_proc`.
unsafe fn resolve_gl_proc(
    lib: &libloading::os::unix::Library,
    name: &str,
) -> *const std::ffi::c_void {
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
pub(crate) fn compute_content_region(fb_width: i32, fb_height: i32) -> PixelRect<f32> {
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
pub(crate) fn grid_size_for_content_region(harness: &LiveHarness, content_region: &PixelRect<f32>) -> GridSize<u32> {
    let pixel_size = PixelSize::new(
        content_region.max.x - content_region.min.x,
        content_region.max.y - content_region.min.y,
    );
    let grid_size = pixel_size / harness.grid_scale();
    GridSize::new(grid_size.width.floor().max(1.0) as u32, grid_size.height.floor().max(1.0) as u32)
}

/// Converts a widget-local *logical*-pixel position -- exactly what GTK4's `GestureClick`/
/// `EventControllerMotion`/`EventControllerScroll` all report their `x`/`y` in -- into the (col,
/// row) grid cell it lands on, reusing the exact content_region/grid-scale relationship
/// `grid_size_for_content_region` already uses for the P2 resize fix rather than reimplementing
/// it: `logical * scale_factor` first converts into the same device-pixel space `content_region`
/// itself is expressed in (mirroring every other device-pixel user in this file, e.g. the initial
/// `SkiaState` construction's own `widget.width() * widget.scale_factor()`), then
/// `(pixel - content_region.min) / grid_scale`, floored and clamped to `[0, grid_size - 1]` --
/// precisely `MouseManager::get_relative_position_at`'s own formula
/// (`~/src/neovide/src/window/mouse_manager.rs` in the sibling fork checkout).
/// Callers pass `harness.grid_scale()`/`harness.get_grid_size()`
/// straight through (rather than this function taking `&LiveHarness` itself) so the coordinate
/// math stays a pure function of plain values -- independently unit-testable below, and clearly
/// separated from *which* harness state a caller chooses to clamp against. Every call site in
/// this crate clamps against `get_grid_size()` (the grid size nvim's own redraw traffic last
/// actually confirmed), not merely the size last *requested* via `resize_grid`, matching how the
/// reference clamps against a window's own reported `grid_size` rather than a pending resize
/// target.
pub(crate) fn pixel_to_grid_pos(
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

    (grid_x.min(grid_size.width.max(1) - 1), grid_y.min(grid_size.height.max(1) - 1))
}

/// `content_region` computed from `gl_area`'s own *current* framebuffer size -- the same
/// `widget.width() * widget.scale_factor()` / `widget.height() * widget.scale_factor()`
/// device-pixel conversion the render callback's initial `SkiaState` construction and the resize
/// handler both already use, so a mouse handler reading it between two paints still sees the same
/// rect the next frame will actually draw into (as opposed to reaching into `SkiaState`'s own
/// cached `fb_width`/`fb_height`, which is only ever updated from inside `connect_resize`/
/// `connect_render` and would otherwise need its own borrow here for no benefit).
pub(crate) fn current_content_region(gl_area: &GLArea) -> PixelRect<f32> {
    let scale_factor = gl_area.scale_factor();
    compute_content_region(gl_area.width() * scale_factor, gl_area.height() * scale_factor)
}

pub(crate) fn fill_content_region(canvas: &Canvas, content_region: &PixelRect<f32>, color: Color4f) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_to_grid_pos_accounts_for_content_region_offset_and_scale_factor() {
        let content_region = PixelRect::from_min_max((40.0, 40.0), (940.0, 640.0));
        let grid_scale = GridScale::new(PixelSize::new(9.0, 18.0));
        let grid_size = GridSize::new(100, 33);

        // Right at content_region's own top-left corner -> grid cell (0, 0), not wherever (0, 0)
        // of the raw framebuffer would map to -- this is the whole point of subtracting
        // content_region.min before dividing by grid_scale.
        assert_eq!(pixel_to_grid_pos(40.0, 40.0, 1, &content_region, grid_scale, grid_size), (0, 0));

        // One cell right/down of that (scale_factor=1, so logical == device pixels here).
        assert_eq!(pixel_to_grid_pos(49.0, 58.0, 1, &content_region, grid_scale, grid_size), (1, 1));

        // scale_factor=2 (HiDPI): logical (20, 20) is device pixel (40, 40) -- same as the first
        // case above once converted, so still grid cell (0, 0).
        assert_eq!(pixel_to_grid_pos(20.0, 20.0, 2, &content_region, grid_scale, grid_size), (0, 0));

        // Anything left of/above content_region clamps to 0 rather than underflowing.
        assert_eq!(pixel_to_grid_pos(0.0, 0.0, 1, &content_region, grid_scale, grid_size), (0, 0));

        // Anything past the grid's own reported size clamps to grid_size - 1, matching
        // `MouseManager::get_relative_position_at`'s own clamp.
        assert_eq!(
            pixel_to_grid_pos(9000.0, 9000.0, 1, &content_region, grid_scale, grid_size),
            (99, 32)
        );
    }
}
