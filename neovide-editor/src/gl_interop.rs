//! GL/Skia interop and content-region math for the editor surface.
//!
//! Extracted from poc/neovide_embed_live as the foundational GL wrapping and coordinate
//! system implementation that all P0–P11 validation phases depend on.

use skia_safe::gpu::gl::{Format as GlFormat, FramebufferInfo, Interface as GlInterface};
use skia_safe::gpu::{backend_render_targets, surfaces, Budgeted, DirectContext, SurfaceOrigin};
use skia_safe::{AlphaType, Canvas, Color4f, ColorType, ImageInfo, Paint, Rect, Surface};

use neovide::live_harness::LiveHarness;
use neovide::units::{GridScale, GridSize, PixelRect, PixelSize};

/// Inset (device pixels) between the GtkGLArea's own framebuffer edge and the content region.
///
/// **Zero since 2026-09-16, deliberately.** It was 40.0 as a P0-P5 validation aid inherited from
/// `poc/neovide_embed*`: a non-zero inset made a viewport-containment bleed -- the renderer
/// painting outside the rect it was handed -- obvious on screen. That validation is finished (P5:
/// real screenshots at six scale factors, numeric geometry cross-checks), and what the margin left
/// behind in the product was a hardcoded near-black frame around the editor, dark around a light
/// colorscheme. The editor now fills its pane.
///
/// Kept as a named constant rather than deleted or inlined: it is what this file's doc comments and
/// geometry tests name, and the frozen `poc/` crates (ADR §6) still carry their own copies.
pub(crate) const CONTENT_MARGIN: f32 = 0.0;

/// GL-context-bound Skia state. Complex drawing uses a Skia-owned GPU target, then one Src copy
/// presents it through GTK's framebuffer. GTK's exported buffer can have a linear layout that is
/// expensive for the glyph workload; keeping the render target separate lets Skia choose its own.
pub(crate) struct SkiaState {
    pub(crate) gr_context: DirectContext,
    pub(crate) gtk_surface: Option<Surface>,
    pub(crate) render_surface: Option<Surface>,
    render_surface_failed_size: Option<(i32, i32)>,
    pub(crate) fb_width: i32,
    pub(crate) fb_height: i32,
}

impl SkiaState {
    pub(crate) fn new(gr_context: DirectContext, fb_width: i32, fb_height: i32) -> Self {
        Self {
            gr_context,
            gtk_surface: None,
            render_surface: None,
            render_surface_failed_size: None,
            fb_width,
            fb_height,
        }
    }

    /// Tell Skia that the GL context's state was changed by someone else since the last frame, so
    /// it must re-emit its own state instead of trusting its cache. **Call this at the top of
    /// every render callback, before any drawing.**
    ///
    /// This is not a precaution -- it is the fix for a real, reproducible, long-standing rendering
    /// bug (found 2026-09-07 via `shell`, root-caused 2026-09-08). `GrDirectContext` assumes it is
    /// the only thing touching the underlying GL context and caches the GL state it has already
    /// set, most importantly which texture is bound to which texture unit. That assumption does
    /// not hold inside a `GtkGLArea`: GTK drives the very same GL context around our render
    /// callback, and on any resize `gtk_gl_area_allocate_buffers()` creates the new colour texture
    /// backing the area's framebuffer and leaves *it* bound to texture unit 0.
    ///
    /// Measured directly (`glGetIntegerv(GL_TEXTURE_BINDING_2D)` at the top of the render
    /// callback): on an ordinary frame the unit-0 binding on entry is the same texture Skia left
    /// bound on exit from the previous frame, but on a frame following a GTK buffer reallocation
    /// it is GTK's *new* texture instead -- while Skia's cache still believes its own glyph atlas
    /// is bound there. Skia therefore skips the `glBindTexture` it would otherwise emit, and every
    /// glyph-mask draw samples GTK's opaque RGBA framebuffer texture instead of the A8 glyph
    /// atlas. Full coverage everywhere inside each glyph's quad, so every character paints as a
    /// solid block the exact shape of its bounding box, with no letterform detail -- and it
    /// persists, because the idle-render gating added in P11 correctly stops issuing frames once
    /// nothing is animating, leaving that one bad frame on screen until real input arrives.
    ///
    /// Evidence for why this is unconditional rather than resize-only: resetting only on the frame
    /// that rebuilds the surface (i.e. only on the resize frame itself) was tested and did **not**
    /// fix it (0/4 trials clean), because nvim answers the resize asynchronously and the frames
    /// that repaint the newly-sized grid arrive later. Resetting on every frame does
    /// (5/5 clean with the full reset here, 4/4 with the narrower `reset_gl_texture_bindings()`).
    /// The cost measured over a scripted scroll workload is ~1-4us against a ~3ms render callback
    /// -- roughly 0.1% -- so the wider, safer reset is used: GTK also owns the framebuffer binding
    /// and can touch program/blend state, and those caches are equally stale for the same reason.
    pub(crate) fn invalidate_cached_gl_state(&mut self) {
        self.gr_context.reset(None);
    }

    pub(crate) fn ensure_gtk_surface(&mut self) {
        if self.gtk_surface.is_some() {
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

        self.gtk_surface = Some(surface);
    }

    /// Retain GTK's physical-pixel size and colour format. If Skia cannot create the target,
    /// draw into GTK directly; retry only at a different size or with a new GL context.
    /// Skia can defer GPU allocation until submission: a later device/OOM failure is not
    /// reported by this constructor and is not covered by this fallback.
    pub(crate) fn ensure_render_surface(&mut self) {
        if self.fb_width <= 0 || self.fb_height <= 0 {
            self.render_surface = None;
            return;
        }
        if self
            .render_surface
            .as_ref()
            .is_some_and(|surface| surface.width() == self.fb_width && surface.height() == self.fb_height)
        {
            return;
        }
        // Called from render with the GL context current, including when releasing a stale size.
        self.render_surface = None;
        let size = (self.fb_width, self.fb_height);
        if self.render_surface_failed_size == Some(size) {
            return;
        }
        let info = ImageInfo::new(size, ColorType::RGBA8888, AlphaType::Premul, None);
        self.render_surface = surfaces::render_target(
            &mut self.gr_context,
            Budgeted::Yes,
            &info,
            0usize,
            SurfaceOrigin::BottomLeft,
            None,
            false,
            false,
        );
        if self.render_surface.is_none() {
            self.render_surface_failed_size = Some(size);
            eprintln!(
                "[editor] could not create a {}x{} GPU render target; drawing directly into GTK",
                self.fb_width, self.fb_height
            );
        } else {
            self.render_surface_failed_size = None;
        }
    }

    /// The ordinary unrealize handler runs before GTK destroys its GL context. With that context
    /// current, release surfaces first and then Skia's caches. If the context is already lost,
    /// abandon FIRST so dropping GPU objects cannot issue GL calls against some other context.
    pub(crate) fn release(mut self, context_is_current: bool) {
        if !context_is_current {
            self.gr_context.abandon();
        }
        self.render_surface = None;
        self.gtk_surface = None;
        if context_is_current {
            self.gr_context.release_resources_and_abandon();
        }
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
///
/// At today's `CONTENT_MARGIN` of 0.0 both branches produce the same rect for any framebuffer
/// wider and taller than 20px, so the guard reads as dead -- it is not. The fallback's `.max(1.0)`
/// is what keeps a zero-sized framebuffer (a widget that has been allocated nothing yet) from
/// producing a degenerate rect, and the margin is a constant a host or a future validation pass can
/// put back.
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
    GridSize::new(
        grid_size.width.floor().max(1.0) as u32,
        grid_size.height.floor().max(1.0) as u32,
    )
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

    (
        grid_x.min(grid_size.width.max(1) - 1),
        grid_y.min(grid_size.height.max(1) - 1),
    )
}

/// The grid's region for hit testing, derived from the pane's one framebuffer size (`fb`, the
/// `NeovideEditorPane::fb_size` GTK last reported through `GLArea::resize`) through the same
/// [`grid_layout`] the render callback draws with, so a click and the frame it lands on can never
/// disagree about where row 0 starts (the 2026-09-19 lesson: "the snap has to reach
/// hit-testing"). Before wave 4 this re-derived the size as `widget.width() * scale_factor()`, a
/// second source that only agrees with GTK's while the widget has been drawn at its allocation.
pub(crate) fn current_content_region(fb: (i32, i32), grid_scale: GridScale) -> PixelRect<f32> {
    grid_layout(fb.0, fb.1, grid_scale, |_| GridSize::new(1, 1)).region
}

/// Everything the editor's geometry needs for one frame, derived from ONE framebuffer size:
/// the snapped region the grid is drawn into, the `(cols, rows)` nvim should be sized to, and how
/// many pixels of sub-cell remainder sit above the grid (`band_top_px`, logged so a GUI pass can
/// tell a sub-cell band from a whole blank row, which would be nvim's own cmdline instead).
///
/// Wave 4 (owner: "neovim最下面的空白应该填在上面"): the render callback, the resize handler, the
/// tick's resync and hit testing each used to derive this themselves, some from GTK's reported
/// framebuffer and some from `widget.width() * scale_factor()`; one function over one size means
/// they cannot drift apart. neovibe-only.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GridLayout {
    pub region: PixelRect<f32>,
    pub grid: GridSize<u32>,
    pub band_top_px: f32,
}

/// See [`GridLayout`]. `grid_for` does the division (in the product,
/// `|r| grid_size_for_content_region(&harness, r)`), so the grid is rounded exactly as the size
/// `resize_grid` is sent; the pure tests pass a plain floor division.
pub(crate) fn grid_layout(
    fb_width: i32,
    fb_height: i32,
    grid_scale: GridScale,
    grid_for: impl Fn(&PixelRect<f32>) -> GridSize<u32>,
) -> GridLayout {
    let unsnapped = compute_content_region(fb_width, fb_height);
    let region = snap_region_to_grid(&unsnapped, grid_scale);
    GridLayout {
        grid: grid_for(&region),
        band_top_px: region.min.y - unsnapped.min.y,
        region,
    }
}

/// Whether two grid scales are the same cell, compared by bits. `GridScale` has no `PartialEq`,
/// and a plain f32 `!=` would call a NaN cell (before nvim reports a font) "changed" on every
/// frame -- the render callback would then ask for another frame forever, the P11 idle-CPU
/// regression. By bits, NaN equals itself. neovibe-only.
pub(crate) fn same_grid_scale(a: GridScale, b: GridScale) -> bool {
    (a.width().to_bits(), a.height().to_bits()) == (b.width().to_bits(), b.height().to_bits())
}

/// `region` with the vertical sub-cell remainder moved from the BOTTOM edge to the TOP edge.
///
/// A pane is almost never an exact multiple of the font's cell height, and
/// `grid_size_for_content_region` floors, so `height mod cell_height` pixels are always left over.
/// They are painted -- `draw_frame` clip-clears the whole region to nvim's own `Normal` background
/// -- so this is not an undrawn band. It is a band of the *editor's* background in a place where
/// that colour is not what sits above it.
///
/// **Which edge it lands on is the entire difference, and it is not a matter of taste.** At the
/// bottom, the row above the remainder is nvim's statusline (a global `laststatus=3` lualine, in
/// the config this was reported against), whose background is deliberately a different colour; the
/// leftover then reads as a stripe of a third colour between the statusline and the shell's own
/// status bar. At the top, the row below it is ordinary buffer text on `Normal` -- the same colour
/// as the remainder -- so it is indistinguishable from padding under the top bar. Reported
/// 2026-09-19 as "最下面还是有空余" after the 44px dead-cmdline row (fork `20bee56`) was fixed and
/// only this 14px remainder was left.
///
/// **Vertical only, deliberately.** The horizontal remainder sits at the right edge, where its
/// neighbour is ordinary buffer background of the same colour, so it is already invisible; moving
/// it to the left would put it between the window edge and the sign column, which is where it
/// would start being visible. The asymmetry is in what the leftover is adjacent to, not in the
/// arithmetic.
///
/// **The caller owes one thing for this to be an improvement rather than a swap:** once this
/// returns anything but the full framebuffer, the pixels OUTSIDE the returned rect are no longer
/// repainted by the renderer, so whatever the host clears the framebuffer to becomes visible for
/// the first time. `lib.rs`'s `OUTSIDE_COLOR` doc predicted exactly this ("If `CONTENT_MARGIN`
/// ever becomes non-zero again, this constant -- and a reason to bring back a host-settable
/// override -- becomes reachable once more") and `NeovideEditorPane::set_clear_color` is that
/// override, driven from the theme pipeline's `tokens.bg`.
///
/// Returns `region` unchanged when there is nothing to move or nothing to move it within: a cell
/// height that is not yet a positive, finite number (nvim has not reported a font), no remainder,
/// or a region too short for even one row -- in which case eating the remainder would leave a
/// zero-height rect.
pub(crate) fn snap_region_to_grid(region: &PixelRect<f32>, grid_scale: GridScale) -> PixelRect<f32> {
    let cell_height = grid_scale.height();
    if !cell_height.is_finite() || cell_height <= 0.0 {
        return *region;
    }
    let height = region.max.y - region.min.y;
    let remainder = height - (height / cell_height).floor() * cell_height;
    // Stated as one positive condition rather than as negated comparisons, so NaN -- which makes
    // every comparison false and would silently pass a negated test -- lands on "leave it alone"
    // by construction rather than by the reader noticing.
    let worth_moving = remainder.is_finite() && remainder > 0.0 && remainder < height;
    if !worth_moving {
        return *region;
    }
    PixelRect::from_min_max((region.min.x, region.min.y + remainder), (region.max.x, region.max.y))
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

    /// Pins the 2026-09-16 geometry: `CONTENT_MARGIN` is 0.0, so the editor fills its pane and the
    /// near-black frame that used to surround it cannot come back unnoticed. Deliberately separate
    /// from the offset/scale-factor test below, which builds its own `content_region` literal and
    /// tests the coordinate math rather than this constant.
    ///
    /// It is also the reason `lib.rs`'s per-frame `canvas.clear` is currently invisible: the Skia
    /// surface is wrapped at exactly `fb_width x fb_height` (`ensure_gtk_surface`), so a content region
    /// equal to the framebuffer means whatever draws the frame covers every pixel the clear wrote.
    /// See `OUTSIDE_COLOR`'s doc in `lib.rs`.
    #[test]
    fn content_region_at_zero_margin_is_the_whole_framebuffer() {
        assert_eq!(CONTENT_MARGIN, 0.0);

        // A realistic pane: the content region is the framebuffer, with nothing inset anywhere.
        let region = compute_content_region(1280, 760);
        assert_eq!((region.min.x, region.min.y), (0.0, 0.0));
        assert_eq!((region.max.x, region.max.y), (1280.0, 760.0));

        // The too-small branch still yields a non-degenerate rect rather than a zero-area one --
        // the `.max(1.0)` in the fallback, which is why that branch stays even though at margin 0
        // it agrees with the other one for anything bigger than 20px.
        let degenerate = compute_content_region(0, 0);
        assert!(degenerate.max.x - degenerate.min.x > 0.0);
        assert!(degenerate.max.y - degenerate.min.y > 0.0);

        // And with the content region starting at the origin, framebuffer (0, 0) is cell (0, 0):
        // there is no longer an inset band of pixels that maps to the first cell.
        let grid_scale = GridScale::new(PixelSize::new(9.0, 18.0));
        let grid_size = GridSize::new(142, 42);
        assert_eq!(pixel_to_grid_pos(0.0, 0.0, 1, &region, grid_scale, grid_size), (0, 0));
        assert_eq!(pixel_to_grid_pos(9.0, 18.0, 1, &region, grid_scale, grid_size), (1, 1));
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

#[cfg(test)]
mod snap_tests {
    use super::*;

    /// The exact geometry from the 2026-09-19 report, reproduced as arithmetic so the fix cannot
    /// quietly stop applying: a 1598px-tall pane at a 44px cell is 36 rows plus **14px**, and that
    /// 14 is what the owner saw as "最下面还是有空余". After the snap it is at the top, where the
    /// pixel below it is buffer background of the same colour.
    #[test]
    fn the_reported_band_moves_from_the_bottom_edge_to_the_top() {
        let grid_scale = GridScale::new(PixelSize::new(22.0, 44.0));
        let pane = PixelRect::from_min_max((0.0, 0.0), (1400.0, 1598.0));

        let snapped = snap_region_to_grid(&pane, grid_scale);

        assert_eq!(snapped.min.y, 14.0, "the remainder should now be ABOVE the grid");
        assert_eq!(
            snapped.max.y, 1598.0,
            "the bottom edge must be flush -- that is the fix"
        );
        let height = snapped.max.y - snapped.min.y;
        assert_eq!(height % 44.0, 0.0, "a snapped region is whole cells");
        assert_eq!(height / 44.0, 36.0, "and the same 36 rows as before, not one fewer");

        // The horizontal remainder is deliberately NOT moved: its neighbour at the right edge is
        // ordinary buffer background of the same colour, so it is already invisible, while moving
        // it left would put it between the window edge and the sign column.
        assert_eq!((snapped.min.x, snapped.max.x), (0.0, 1400.0));
    }

    /// The grid keeps the same number of rows either way. Worth pinning because it is the reason
    /// the snap is purely cosmetic: if this were false the fix would be silently resizing nvim.
    #[test]
    fn snapping_never_changes_how_many_rows_fit() {
        let grid_scale = GridScale::new(PixelSize::new(9.0, 18.0));
        for height in [100.0_f32, 360.0, 361.0, 377.0, 1598.0, 1080.0] {
            let raw = PixelRect::from_min_max((0.0, 0.0), (800.0, height));
            let snapped = snap_region_to_grid(&raw, grid_scale);
            let rows_raw = ((raw.max.y - raw.min.y) / 18.0).floor();
            let rows_snapped = ((snapped.max.y - snapped.min.y) / 18.0).floor();
            assert_eq!(rows_raw, rows_snapped, "height {height} changed its row count");
        }
    }

    /// Every case where eating the remainder would be worse than leaving it. The cell height is
    /// genuinely 0 before nvim has reported a font, and a pane can genuinely be shorter than one
    /// row mid-drag -- returning a zero-height or inverted rect there would be a crash or a blank
    /// pane, not a cosmetic issue.
    #[test]
    fn degenerate_inputs_are_left_exactly_alone() {
        let pane = PixelRect::from_min_max((0.0, 0.0), (800.0, 100.0));
        for bad_cell in [0.0_f32, -18.0, f32::NAN, f32::INFINITY] {
            let snapped = snap_region_to_grid(&pane, GridScale::new(PixelSize::new(9.0, bad_cell)));
            assert_eq!((snapped.min.y, snapped.max.y), (0.0, 100.0), "cell height {bad_cell}");
        }

        // Shorter than one row: the whole height is "remainder", and taking it would leave nothing.
        let tiny = PixelRect::from_min_max((0.0, 0.0), (800.0, 10.0));
        let snapped = snap_region_to_grid(&tiny, GridScale::new(PixelSize::new(9.0, 18.0)));
        assert_eq!((snapped.min.y, snapped.max.y), (0.0, 10.0));

        // An exact multiple has no remainder to move and must not drift by a float epsilon.
        let exact = PixelRect::from_min_max((0.0, 0.0), (800.0, 360.0));
        let snapped = snap_region_to_grid(&exact, GridScale::new(PixelSize::new(9.0, 18.0)));
        assert_eq!((snapped.min.y, snapped.max.y), (0.0, 360.0));
    }

    /// A click must still land on the cell the user aimed at. This is the half of the change that
    /// could break silently: `pixel_to_grid_pos` subtracts `content_region.min`, so a caller that
    /// snapped for RENDERING but not for HIT-TESTING would be off by 14/44 of a row -- wrong only
    /// near a row boundary, which is exactly where it would be blamed on the user's aim.
    #[test]
    fn hit_testing_through_the_snapped_region_lands_on_the_right_row() {
        let grid_scale = GridScale::new(PixelSize::new(22.0, 44.0));
        let grid_size = GridSize::new(63_u32, 36);
        let snapped = snap_region_to_grid(&PixelRect::from_min_max((0.0, 0.0), (1400.0, 1598.0)), grid_scale);

        // The first painted pixel row of the grid is y=14, not y=0.
        assert_eq!(pixel_to_grid_pos(0.0, 14.0, 1, &snapped, grid_scale, grid_size).1, 0);
        // One pixel above it is still clamped into row 0 rather than going negative.
        assert_eq!(pixel_to_grid_pos(0.0, 0.0, 1, &snapped, grid_scale, grid_size).1, 0);
        // Row 1 starts 44px later, and 1px earlier is still row 0 -- the boundary the unsnapped
        // rect would have put 14px too high.
        assert_eq!(pixel_to_grid_pos(0.0, 57.0, 1, &snapped, grid_scale, grid_size).1, 0);
        assert_eq!(pixel_to_grid_pos(0.0, 58.0, 1, &snapped, grid_scale, grid_size).1, 1);
        // And the last pixel of the pane is the last row, with nothing past it.
        assert_eq!(pixel_to_grid_pos(0.0, 1597.0, 1, &snapped, grid_scale, grid_size).1, 35);
    }

    fn divide(scale: GridScale) -> impl Fn(&PixelRect<f32>) -> GridSize<u32> {
        move |r| {
            GridSize::new(
                ((r.max.x - r.min.x) / scale.width()).floor() as u32,
                ((r.max.y - r.min.y) / scale.height()).floor() as u32,
            )
        }
    }

    /// Wave 4 (owner: "neovim最下面的空白应该填在上面"): the grid's rows exactly fill the snapped
    /// region, and the region's bottom is the framebuffer's bottom -- for two cell heights, as
    /// after a zoom.
    #[test]
    fn the_grid_fills_the_snapped_region_and_ends_at_the_bottom() {
        for cell in [(20.0_f32, 44.0_f32), (22.0, 49.0)] {
            let scale = GridScale::new(PixelSize::new(cell.0, cell.1));
            let layout = grid_layout(1400, 1598, scale, divide(scale));
            assert_eq!(layout.region.max.y, 1598.0, "flush at the bottom");
            assert_eq!(
                layout.grid.height as f32 * cell.1,
                layout.region.max.y - layout.region.min.y
            );
            assert_eq!(layout.band_top_px, 1598.0 % cell.1);
            assert!(layout.band_top_px < cell.1);
        }
    }

    #[test]
    fn a_framebuffer_that_is_a_whole_number_of_rows_has_no_band() {
        let scale = GridScale::new(PixelSize::new(20.0, 44.0));
        let layout = grid_layout(1400, 44 * 30, scale, divide(scale));
        assert_eq!(
            (layout.band_top_px, layout.region.min.y, layout.grid.height),
            (0.0, 0.0, 30)
        );
    }

    #[test]
    fn a_zero_or_unfonted_framebuffer_is_left_alone() {
        let scale = GridScale::new(PixelSize::new(20.0, 44.0));
        let layout = grid_layout(0, 0, scale, divide(scale));
        assert_eq!(layout.band_top_px, 0.0);
        let nan = GridScale::new(PixelSize::new(f32::NAN, f32::NAN));
        assert_eq!(grid_layout(100, 100, nan, |_| GridSize::new(1, 1)).band_top_px, 0.0);
    }

    /// A NaN cell (before nvim reports a font) must compare equal to itself, or the render
    /// callback's "the scale changed inside render_frame" check would ask for a frame on every
    /// frame -- continuous rendering at idle, the P11 regression.
    #[test]
    fn a_nan_cell_is_the_same_scale_so_an_unfonted_pane_does_not_render_forever() {
        let nan = GridScale::new(PixelSize::new(f32::NAN, f32::NAN));
        assert!(same_grid_scale(nan, nan));
        let s44 = GridScale::new(PixelSize::new(20.0, 44.0));
        assert!(same_grid_scale(s44, GridScale::new(PixelSize::new(20.0, 44.0))));
        assert!(!same_grid_scale(s44, GridScale::new(PixelSize::new(22.0, 49.0))));
    }

    /// Clicks and paint read one region: hit testing's rect is exactly the one the render
    /// callback hands `render_frame` (the 2026-09-19 lesson: "the snap has to reach hit-testing").
    #[test]
    fn the_region_grid_layout_returns_is_the_one_hit_testing_uses() {
        let s44 = GridScale::new(PixelSize::new(20.0, 44.0));
        let hit = current_content_region((1400, 1598), s44);
        let paint = grid_layout(1400, 1598, s44, divide(s44)).region;
        assert_eq!(hit, paint);
        assert_eq!((hit.min.y, hit.max.y), (14.0, 1598.0));
    }

    /// S2's whole justification, as arithmetic: a scale change applied TOGETHER with the framebuffer
    /// resize that always accompanies it -- exactly what `lib.rs`'s `sync_os_scale` guarantees by
    /// running before any grid is computed, in both `connect_resize` and the render callback --
    /// resizes nothing in nvim. The measured cell (11.238282x22, `s1.log`, 2026-09-27 sandbox
    /// investigation) and both sandbox framebuffers from that same run.
    #[test]
    fn a_scale_change_applied_with_its_framebuffer_resizes_nothing() {
        let cell = GridScale::new(PixelSize::new(11.238282, 22.0));
        let cell_x2 = GridScale::new(PixelSize::new(22.476564, 44.0));
        for (fb_w, fb_h) in [(760, 721), (760, 480)] {
            let before = grid_layout(fb_w, fb_h, cell, divide(cell)).grid;
            let after = grid_layout(fb_w * 2, fb_h * 2, cell_x2, divide(cell_x2)).grid;
            assert_eq!(
                before, after,
                "fb {fb_w}x{fb_h}: scale and framebuffer must move together"
            );
        }
    }

    /// The counter-example that motivated S2 -- exactly the probe's own defect (plan's "The fix, as
    /// probed"): the new cell size applied WITHOUT its matching framebuffer resize is a *different*
    /// grid, `33x10` for `760x480`, not the `67x21` the invariant above holds for that same
    /// framebuffer at the matching cell. This is the transient grid a notify-time apply produces
    /// before GTK's own `resize` catches up -- documented here as the reason `set_os_scale_factor`
    /// must never be called from `connect_scale_factor_notify` itself (S2).
    #[test]
    fn applying_the_new_cell_before_the_framebuffer_resize_is_a_different_grid() {
        let cell = GridScale::new(PixelSize::new(11.238282, 22.0));
        let cell_x2 = GridScale::new(PixelSize::new(22.476564, 44.0));
        let matched = grid_layout(760, 480, cell, divide(cell)).grid;
        let mismatched = grid_layout(760, 480, cell_x2, divide(cell_x2)).grid;
        assert_eq!((mismatched.width, mismatched.height), (33, 10));
        assert_ne!(
            matched, mismatched,
            "the new cell size without its matching framebuffer must not equal the matched grid"
        );
    }
}
