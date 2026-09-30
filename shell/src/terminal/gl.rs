//! Moved from `terminal-pane/src/gl.rs` on `freeze/terminal-stack` @ `1e715ab` (2026-09-23),
//! unchanged below this paragraph. It lives in `shell` rather than in `eitri-terminal` because the
//! libepoxy lookup it does is how a GtkGLArea's GL is reached, not how a terminal paints: a macOS
//! host would bring its own.
//!
//! GL/Skia interop for a `GtkGLArea`.
//!
//! Carried over from `neovide-editor/src/gl_interop.rs`, which is a private module (`mod gl_interop;`)
//! with every item `pub(crate)`, so it cannot be imported. Four other crates in this repository have
//! already copied it for the same reason. Factoring it into a shared crate would be the better move
//! and is deliberately not attempted here: it would mean changing `neovide-editor`, and this round
//! is an integration, not a refactor of the editor pane.
//!
//! Two hazards are carried over with it, both found the hard way and both fatal to get wrong:
//!
//! 1. **`GlInterface::new_native()` does not work here.** libepoxy on this machine exports only
//!    *data* symbols named `epoxy_glClear` and the like, so the interface has to be assembled by
//!    dlsym'ing `epoxy_<name>` and then dereferencing the slot once more to reach the function
//!    pointer. Skipping that second dereference is a real SIGSEGV, not a subtle wrongness.
//! 2. **Skia's cached GL state must be invalidated every frame.** See
//!    [`SkiaState::invalidate_cached_gl_state`].

use skia_safe::gpu::gl::{Format as GlFormat, FramebufferInfo, Interface as GlInterface};
use skia_safe::gpu::{backend_render_targets, surfaces, DirectContext, SurfaceOrigin};
use skia_safe::Surface;

pub(crate) struct SkiaState {
    pub(crate) gr_context: DirectContext,
    pub(crate) surface: Option<Surface>,
    pub(crate) fb_width: i32,
    pub(crate) fb_height: i32,
}

impl SkiaState {
    pub(crate) fn new() -> Option<Self> {
        let interface = make_gl_interface()?;
        let gr_context = skia_safe::gpu::direct_contexts::make_gl(interface, None)?;
        Some(Self {
            gr_context,
            surface: None,
            fb_width: 0,
            fb_height: 0,
        })
    }

    /// Tell Skia the GL context was touched by someone else since the last frame, so it re-emits its
    /// own state instead of trusting its cache. **Call at the top of every render callback.**
    ///
    /// Not a precaution. `GrDirectContext` caches which texture is bound to which unit, and that
    /// assumption does not hold inside a `GtkGLArea`: GTK drives the same GL context around the
    /// render callback and, on any resize, `gtk_gl_area_allocate_buffers()` leaves its own new colour
    /// texture bound to unit 0. Skia then skips a `glBindTexture` it needed, and every glyph-mask
    /// draw samples GTK's framebuffer texture instead of the A8 glyph atlas — every character paints
    /// as a solid block the shape of its bounding box. It persists, too, because render gating
    /// correctly stops issuing frames once nothing is animating, leaving that one bad frame on
    /// screen. Measured in this repo at ~1-4us against a ~3ms callback; resetting only on resize
    /// frames was tried and does NOT fix it.
    pub(crate) fn invalidate_cached_gl_state(&mut self) {
        self.gr_context.reset(None);
    }

    pub(crate) fn ensure_surface(&mut self) {
        if self.surface.is_some() || self.fb_width <= 0 || self.fb_height <= 0 {
            return;
        }
        let fb_info = FramebufferInfo {
            fboid: current_bound_framebuffer() as u32,
            format: GlFormat::RGBA8.into(),
            ..Default::default()
        };
        // stencil_bits 8 because the GLArea is built with `has_stencil_buffer(true)`; sample_count 0
        // because it is not configured for MSAA. Both must match how the area was created.
        let render_target = backend_render_targets::make_gl((self.fb_width, self.fb_height), 0, 8, fb_info);
        self.surface = surfaces::wrap_backend_render_target(
            &mut self.gr_context,
            &render_target,
            SurfaceOrigin::BottomLeft,
            skia_safe::ColorType::RGBA8888,
            None,
            None,
        );
    }

    pub(crate) fn resize(&mut self, width: i32, height: i32) {
        if (width, height) != (self.fb_width, self.fb_height) {
            self.fb_width = width;
            self.fb_height = height;
            self.surface = None; // rebuilt against the new framebuffer on the next frame
        }
    }
}

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

fn make_gl_interface() -> Option<GlInterface> {
    unsafe {
        let lib = libloading::os::unix::Library::this();
        GlInterface::new_load_with(move |name: &str| resolve_gl_proc(&lib, name))
    }
}

/// Resolves a GL entry point through libepoxy's data symbols.
///
/// # Safety
/// Dereferences a symbol looked up in the current process. The double dereference is required and
/// is the whole point: `epoxy_glClear` is a *pointer slot*, not the function.
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
