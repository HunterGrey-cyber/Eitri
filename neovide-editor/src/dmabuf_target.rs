//! The editor's own presentation buffers: dmabufs allocated with a tiled layout, rendered by Skia
//! through GL and handed to GTK as `GdkDmabufTexture`s, instead of the colour texture `GtkGLArea`
//! allocates for itself.
//!
//! **Why this exists (2026-09-28, measured).** `GtkGLArea` (GTK 4.16+) exports its colour texture
//! as a dmabuf on every frame (`gtk_gl_area_snapshot` -> `gdk_gl_context_export_dmabuf`), and
//! Mesa's iris driver prepares a texture for export by reallocating it as shareable
//! (`iris_flush_resource` -> `iris_reallocate_resource_inplace(PIPE_BIND_SHARED | PIPE_BIND_SCANOUT)`).
//! On a device without the kernel's tiling uAPI -- Intel Gfx12.5+, Meteor Lake included -- a
//! shareable texture with no modifier is allocated **linear** (`iris_resource_configure_main`), so
//! every GtkGLArea frame lands in a linear, uncompressed buffer (GTK's own debug output:
//! `AB24:0`). At the owner's 3072x1920 / 150 % display the editor's buffer is 2432x2482 pixels
//! (integer scale 2), and GL timer queries put the one copy from the Skia intermediate into that
//! linear buffer at ~3.4 ms of GPU time per frame, against ~0.9 ms for all of Skia's drawing into
//! its own tiled target. Drawing straight into the linear buffer was ~5 ms. A buffer we allocate
//! ourselves with an explicit tiled (and, where offered, compressed) modifier is already
//! external, so exporting it never reallocates it, Skia can draw into it directly, and the copy
//! disappears. See the dated record, "2026-09-28 (fractional-scale GPU cost)".
//!
//! The buffers are the same pixel size `GtkGLArea` would have allocated (widget size times GTK's
//! integer scale factor), drawn with the same origin and flipped the same way, so every geometry
//! rule in this crate is unchanged. Anything this path cannot set up -- no EGL (GLX), no GBM, no
//! modifier both GL can render to and GTK can import -- is reported once on stderr and the pane
//! falls back to `GtkGLArea`'s own buffer for the rest of that GL context. A frame that fails
//! mid-run (an allocation, an import, an incomplete framebuffer, a texture GTK refuses) is reported
//! too, and the path is tried again at the next size change, a few times per GL context
//! (`editor_area`). `EITRI_EDITOR_DMABUF=0` takes the fallback on purpose, for a driver on which
//! this path draws wrongly without failing (and for the regression test that forces the fallback).
//!
//! **Only on GTK 4.16 and later by default** ([`decide`]). The cost above exists only where
//! `GtkGLArea` exports a dmabuf per frame, which began in 4.16. GTK 4.14 (the v1 floor, Ubuntu
//! 24.04) hands GSK a `GdkGLTexture` with a `GLsync` and exports nothing; there this path would
//! trade the copy out of the intermediate for GSK's per-frame EGL import of a new
//! `GdkDmabufTexture`, which was never measured. `EITRI_EDITOR_DMABUF=1` forces the path there,
//! for measuring it.
//!
//! **What GTK's format list does not promise.** `gdk_display_get_dmabuf_formats` is the union of
//! what GDK's Vulkan, EGL and mmap importers accept (`gdk_display_init_dmabuf_invoke_callback`,
//! 4.22.5). A modifier on it is not a promise that the renderer GSK actually uses imports it
//! directly: where it cannot (GSK's Vulkan device on another GPU than the EGL display -- a hybrid
//! laptop, NVIDIA offload --, a Vulkan driver that does not offer a compressed modifier the GL
//! driver does, or `GSK_RENDERER=cairo`), GSK silently downloads the buffer through another
//! importer every frame, a GPU->CPU->GPU round trip nothing on this side can see. The public API
//! cannot tell which importer will take a texture. `GDK_DEBUG=dmabuf` shows it;
//! `EITRI_EDITOR_DMABUF=0` is the escape hatch. Only Intel (i915, one GPU) has run this path.
//!
//! **The fallback itself can cost a download (2026-09-29).** On GTK 4.16+ with GSK's Vulkan
//! renderer, a driver whose Vulkan cannot import `GtkGLArea`'s implicit-modifier export
//! (`AB24:0xffffffffffffff`; Mesa RADV, seen in a 2026-09-28 measurement record) makes GSK download
//! every frame through its GL renderer and upload it again: ~10 ms of main-thread CPU per frame at
//! 2432x2482, i.e. about +10.75 ms typing at 165 Hz. The workaround is `GSK_RENDERER=gl` (`ngl`
//! before GTK 4.18), which has GSK import the dmabuf through EGL; it is not yet measured on AMD.
//! GTK below 4.16 is not affected (source read): `GtkGLArea` exports nothing there and GSK's
//! default renderer is GL. [`fallback_hint`] prints one stderr line when the fallback is running
//! under a Vulkan renderer; it changes no behaviour.
//!
//! **Lifetime and synchronization, the same contract as GTK's own buffers.** A buffer is drawn
//! into only after GTK has released every texture over it. GSK holds a texture whose image a frame
//! still uses (`gsk_gpu_image_toggle_ref_texture`, GTK 4.14.5 and 4.22.5 alike), so that release
//! comes after the GPU finished reading it -- which is also what `GtkGLArea` 4.22 relies on when it
//! reuses its own exported texture (`release_dmabuf_texture`). The texture keeps the dmabuf fds
//! open until then, whatever happened to the buffer on our side (a resize, an unrealize). Before
//! GTK samples a frame it has to see our writes: `GtkGLArea` 4.22 and GSK's 4.14 GL import rely
//! on the driver's implicit fence for that, and GSK's Vulkan renderer turns that fence into a
//! semaphore (`gdk_dmabuf_export_sync_file`). This path also inserts an explicit one, a native
//! fence sync file after the frame's GL work imported into the dmabuf as a write
//! (`DMA_BUF_IOCTL_IMPORT_SYNC_FILE`), so a consumer does not depend on the driver attaching one at
//! `glFlush`. Where that is unavailable (no `EGL_ANDROID_native_fence_sync`, a kernel before 6.0)
//! it says so once and relies on the implicit fence, as `GtkGLArea` does.
//!
//! GTK API used: `GdkDmabufTextureBuilder` and `gdk_display_get_dmabuf_formats` (both 4.14, the
//! v1 floor). GBM is opened at runtime (`libgbm.so.1`, Mesa's), never linked.

use std::ffi::{c_char, c_int, c_void, CStr, OsStr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use gtk4::gdk;
use gtk4::prelude::*;

use crate::gl_interop::resolve_gl_proc;

/// `DRM_FORMAT_ABGR8888` ("AB24"): bytes R, G, B, A in memory, which is GL's `RGBA8` and the
/// format `GtkGLArea` itself exports on a GLES context.
pub(crate) const DRM_FORMAT_ABGR8888: u32 = 0x3432_4241;
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
const MAX_PLANES: usize = 4;

const EGL_NONE: i32 = 0x3038;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_EXTENSIONS: i32 = 0x3055;
const EGL_DEVICE_EXT: i32 = 0x322C;
const EGL_DRM_RENDER_NODE_FILE_EXT: i32 = 0x3377;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
/// FD, OFFSET, PITCH, MODIFIER_LO, MODIFIER_HI for planes 0..3 (`eglext.h`).
const EGL_PLANE_ATTRS: [[i32; 5]; MAX_PLANES] = [
    [0x3272, 0x3273, 0x3274, 0x3443, 0x3444],
    [0x3275, 0x3276, 0x3277, 0x3445, 0x3446],
    [0x3278, 0x3279, 0x327A, 0x3447, 0x3448],
    [0x3440, 0x3441, 0x3442, 0x3449, 0x344A],
];

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_NEAREST: i32 = 0x2600;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_RENDERBUFFER: u32 = 0x8D41;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_DEPTH_ATTACHMENT: u32 = 0x8D00;
const GL_STENCIL_ATTACHMENT: u32 = 0x8D20;
const GL_DEPTH24_STENCIL8: u32 = 0x88F0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_DEPTH_TEST: u32 = 0x0B71;

const GBM_BO_USE_RENDERING: u32 = 1 << 2;

const EGL_SYNC_NATIVE_FENCE_ANDROID: u32 = 0x3144;
const EGL_NO_NATIVE_FENCE_FD_ANDROID: i32 = -1;
/// `_IOW('b', 3, struct dma_buf_import_sync_file)` (`linux/dma-buf.h`, Linux 6.0).
const DMA_BUF_IOCTL_IMPORT_SYNC_FILE: libc::c_ulong = 0x4008_6203;
const DMA_BUF_SYNC_WRITE: u32 = 2;

#[repr(C)]
struct DmaBufImportSyncFile {
    flags: u32,
    fd: i32,
}

/// The environment variable that turns this path off (`0`, `off`, `false`, `no`) or forces it on
/// below [`MEASURED_FROM_GTK`] (`1`, `on`, `true`, `yes`), any case. Unset, or anything else, is the
/// default: on from GTK 4.16.
pub(crate) const ENV: &str = "EITRI_EDITOR_DMABUF";

/// The first GTK whose `GtkGLArea` exports its texture as a dmabuf every frame: where the cost this
/// path removes exists, and where it was measured (module doc).
pub(crate) const MEASURED_FROM_GTK: (u32, u32) = (4, 16);

/// What `EITRI_EDITOR_DMABUF` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Setting {
    Off,
    On,
    Default,
}

pub(crate) fn setting(value: Option<&OsStr>) -> Setting {
    let Some(value) = value.and_then(OsStr::to_str).map(str::trim) else {
        return Setting::Default;
    };
    let is = |words: [&str; 4]| words.iter().any(|w| value.eq_ignore_ascii_case(w));
    if is(["0", "off", "false", "no"]) {
        Setting::Off
    } else if is(["1", "on", "true", "yes"]) {
        Setting::On
    } else {
        Setting::Default
    }
}

/// The one-line hint for a pane that draws through `GtkGLArea`'s own texture: `Some` only on GTK
/// 4.16+ (where the export exists) when the widget's native renderer is GSK's Vulkan renderer.
/// Pure, so tested; the caller prints it once per pane.
pub(crate) fn fallback_hint(gtk_minor: u32, fell_back: bool, renderer_type_name: &str) -> Option<String> {
    if !fell_back || gtk_minor < MEASURED_FROM_GTK.1 || renderer_type_name != "GskVulkanRenderer" {
        return None;
    }
    Some(
        "GtkGLArea's texture may be exported as an implicit-modifier dmabuf, which GSK's Vulkan renderer may not \
         import on some drivers (e.g. Mesa RADV; Intel was measured exporting linear, with no download): it then downloads every frame (~10 ms of CPU each at HiDPI). \
         `GDK_DEBUG=dmabuf` shows \"for downloading\" lines if so; `GSK_RENDERER=gl` (`ngl` before GTK 4.18) should avoid it (untested)"
            .to_string(),
    )
}

/// Whether to try this path, from `EITRI_EDITOR_DMABUF` and the **runtime** GTK version
/// (`gtk_get_*_version`, not the version the crate was compiled against). `Ok(note)`: try, and
/// print `note` if there is one; `Err(reason)`: fall back, and say why. Pure, so tested.
pub(crate) fn decide(value: Option<&OsStr>, gtk: (u32, u32, u32)) -> Result<Option<String>, String> {
    let shown = || value.map(|v| v.to_string_lossy().into_owned()).unwrap_or_default();
    let (major, minor, micro) = gtk;
    let measured = (major, minor) >= MEASURED_FROM_GTK;
    let (from_major, from_minor) = MEASURED_FROM_GTK;
    match setting(value) {
        Setting::Off => Err(format!("disabled by {ENV}={}", shown())),
        Setting::On if !measured => Ok(Some(format!(
            "own presentation buffers forced by {ENV}={} on GTK {major}.{minor}.{micro}, below \
             {from_major}.{from_minor}, where they were never measured",
            shown()
        ))),
        Setting::On => Ok(None),
        Setting::Default if !measured => Err(format!(
            "not used on GTK {major}.{minor}.{micro}: GtkGLArea exports no dmabuf before \
             {from_major}.{from_minor}, so there is no linear copy to remove, and this path was not \
             measured there ({ENV}=1 forces it)"
        )),
        Setting::Default => Ok(None),
    }
}

/// Whether libEGL is already loaded. libepoxy `abort()`s the process when it cannot resolve a
/// function it is asked to call, and it loads EGL on demand, so no EGL entry point may be touched
/// unless GTK's context already loaded it. A GLX context (X11) may leave it unloaded.
fn egl_loaded() -> bool {
    let handle = unsafe { libc::dlopen(c"libEGL.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD) };
    if handle.is_null() {
        return false;
    }
    unsafe { libc::dlclose(handle) };
    true
}

type EglDisplay = *mut c_void;
type EglImage = *mut c_void;

/// Every EGL/GL entry point this module calls, resolved once through libepoxy (which GTK links)
/// like the rest of this crate's GL (`gl_interop::resolve_gl_proc`). What that yields are
/// libepoxy's dispatch stubs, which resolve on first call and abort if they cannot: each
/// extension function here is called only after its extension was checked.
struct Fns {
    egl_get_current_display: unsafe extern "C" fn() -> EglDisplay,
    egl_query_string: unsafe extern "C" fn(EglDisplay, i32) -> *const c_char,
    egl_query_display_attrib: unsafe extern "C" fn(EglDisplay, i32, *mut isize) -> u32,
    egl_query_device_string: unsafe extern "C" fn(*mut c_void, i32) -> *const c_char,
    egl_query_dmabuf_modifiers: unsafe extern "C" fn(EglDisplay, i32, i32, *mut u64, *mut u32, *mut i32) -> u32,
    egl_create_image: unsafe extern "C" fn(EglDisplay, *mut c_void, u32, *mut c_void, *const i32) -> EglImage,
    egl_destroy_image: unsafe extern "C" fn(EglDisplay, EglImage) -> u32,
    gl_egl_image_target_texture_2d: unsafe extern "C" fn(u32, EglImage),
    gl_gen_textures: unsafe extern "C" fn(i32, *mut u32),
    gl_delete_textures: unsafe extern "C" fn(i32, *const u32),
    gl_bind_texture: unsafe extern "C" fn(u32, u32),
    gl_tex_parameteri: unsafe extern "C" fn(u32, u32, i32),
    gl_gen_framebuffers: unsafe extern "C" fn(i32, *mut u32),
    gl_delete_framebuffers: unsafe extern "C" fn(i32, *const u32),
    gl_bind_framebuffer: unsafe extern "C" fn(u32, u32),
    gl_framebuffer_texture_2d: unsafe extern "C" fn(u32, u32, u32, u32, i32),
    gl_gen_renderbuffers: unsafe extern "C" fn(i32, *mut u32),
    gl_delete_renderbuffers: unsafe extern "C" fn(i32, *const u32),
    gl_bind_renderbuffer: unsafe extern "C" fn(u32, u32),
    gl_renderbuffer_storage: unsafe extern "C" fn(u32, u32, i32, i32),
    gl_framebuffer_renderbuffer: unsafe extern "C" fn(u32, u32, u32, u32),
    gl_check_framebuffer_status: unsafe extern "C" fn(u32) -> u32,
    gl_disable: unsafe extern "C" fn(u32),
    gl_flush: unsafe extern "C" fn(),
    egl_create_sync: unsafe extern "C" fn(EglDisplay, u32, *const i32) -> *mut c_void,
    egl_destroy_sync: unsafe extern "C" fn(EglDisplay, *mut c_void) -> u32,
    egl_dup_native_fence_fd: unsafe extern "C" fn(EglDisplay, *mut c_void) -> i32,
    has_egl_extension: unsafe extern "C" fn(EglDisplay, *const c_char) -> bool,
    has_gl_extension: unsafe extern "C" fn(*const c_char) -> bool,
}

/// Reinterprets a resolved symbol as the function-pointer type of the field it is stored in.
///
/// # Safety
/// `p` must be a non-null function of exactly that signature.
unsafe fn fn_ptr<F: Copy>(p: *const c_void) -> F {
    assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<*const c_void>());
    unsafe { std::mem::transmute_copy(&p) }
}

impl Fns {
    /// # Safety
    /// Only resolves symbols; nothing is called.
    unsafe fn load() -> Result<Self, String> {
        let lib = libloading::os::unix::Library::this();
        let get = |name: &str| {
            let p = unsafe { resolve_gl_proc(&lib, name) };
            if p.is_null() {
                Err(format!("{name} not found"))
            } else {
                Ok(p)
            }
        };
        macro_rules! f {
            ($name:literal) => {
                unsafe { fn_ptr(get($name)?) }
            };
        }
        // `epoxy_has_*_extension` are plain functions libepoxy exports, not dispatch slots.
        let has_egl =
            unsafe { lib.get::<unsafe extern "C" fn(EglDisplay, *const c_char) -> bool>(b"epoxy_has_egl_extension\0") }
                .map_err(|e| format!("epoxy_has_egl_extension: {e}"))?;
        let has_gl = unsafe { lib.get::<unsafe extern "C" fn(*const c_char) -> bool>(b"epoxy_has_gl_extension\0") }
            .map_err(|e| format!("epoxy_has_gl_extension: {e}"))?;
        Ok(Self {
            egl_get_current_display: f!("eglGetCurrentDisplay"),
            egl_query_string: f!("eglQueryString"),
            egl_query_display_attrib: f!("eglQueryDisplayAttribEXT"),
            egl_query_device_string: f!("eglQueryDeviceStringEXT"),
            egl_query_dmabuf_modifiers: f!("eglQueryDmaBufModifiersEXT"),
            egl_create_image: f!("eglCreateImageKHR"),
            egl_destroy_image: f!("eglDestroyImageKHR"),
            gl_egl_image_target_texture_2d: f!("glEGLImageTargetTexture2DOES"),
            gl_gen_textures: f!("glGenTextures"),
            gl_delete_textures: f!("glDeleteTextures"),
            gl_bind_texture: f!("glBindTexture"),
            gl_tex_parameteri: f!("glTexParameteri"),
            gl_gen_framebuffers: f!("glGenFramebuffers"),
            gl_delete_framebuffers: f!("glDeleteFramebuffers"),
            gl_bind_framebuffer: f!("glBindFramebuffer"),
            gl_framebuffer_texture_2d: f!("glFramebufferTexture2D"),
            gl_gen_renderbuffers: f!("glGenRenderbuffers"),
            gl_delete_renderbuffers: f!("glDeleteRenderbuffers"),
            gl_bind_renderbuffer: f!("glBindRenderbuffer"),
            gl_renderbuffer_storage: f!("glRenderbufferStorage"),
            gl_framebuffer_renderbuffer: f!("glFramebufferRenderbuffer"),
            gl_check_framebuffer_status: f!("glCheckFramebufferStatus"),
            gl_disable: f!("glDisable"),
            gl_flush: f!("glFlush"),
            egl_create_sync: f!("eglCreateSyncKHR"),
            egl_destroy_sync: f!("eglDestroySyncKHR"),
            egl_dup_native_fence_fd: f!("eglDupNativeFenceFDANDROID"),
            has_egl_extension: *has_egl,
            has_gl_extension: *has_gl,
        })
    }
}

/// Every GBM entry point [`Gbm`] calls. All of them are resolved before a device is created, so a
/// libgbm missing one (older than Mesa 21.1 lacks `gbm_bo_get_fd_for_plane`) fails with nothing
/// to leak: resolving them after `gbm_create_device` leaked the device, and closed its fd under it.
struct GbmFns {
    create_device: unsafe extern "C" fn(c_int) -> *mut c_void,
    device_destroy: unsafe extern "C" fn(*mut c_void),
    bo_create_with_modifiers2:
        Option<unsafe extern "C" fn(*mut c_void, u32, u32, u32, *const u64, u32, u32) -> *mut c_void>,
    bo_create_with_modifiers: unsafe extern "C" fn(*mut c_void, u32, u32, u32, *const u64, u32) -> *mut c_void,
    bo_get_plane_count: unsafe extern "C" fn(*mut c_void) -> c_int,
    bo_get_fd_for_plane: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    bo_get_stride_for_plane: unsafe extern "C" fn(*mut c_void, c_int) -> u32,
    bo_get_offset: unsafe extern "C" fn(*mut c_void, c_int) -> u32,
    bo_get_modifier: unsafe extern "C" fn(*mut c_void) -> u64,
    bo_destroy: unsafe extern "C" fn(*mut c_void),
}

impl GbmFns {
    /// # Safety
    /// Every non-null address `get` returns must be the GBM function of that name.
    unsafe fn resolve(get: impl Fn(&CStr) -> Option<*const c_void>) -> Result<Self, String> {
        let found = |name: &CStr| get(name).filter(|p| !p.is_null());
        let need = |name: &CStr| found(name).ok_or_else(|| format!("libgbm has no {}", name.to_string_lossy()));
        unsafe {
            Ok(Self {
                create_device: fn_ptr(need(c"gbm_create_device")?),
                device_destroy: fn_ptr(need(c"gbm_device_destroy")?),
                bo_create_with_modifiers2: found(c"gbm_bo_create_with_modifiers2").map(|p| fn_ptr(p)),
                bo_create_with_modifiers: fn_ptr(need(c"gbm_bo_create_with_modifiers")?),
                bo_get_plane_count: fn_ptr(need(c"gbm_bo_get_plane_count")?),
                bo_get_fd_for_plane: fn_ptr(need(c"gbm_bo_get_fd_for_plane")?),
                bo_get_stride_for_plane: fn_ptr(need(c"gbm_bo_get_stride_for_plane")?),
                bo_get_offset: fn_ptr(need(c"gbm_bo_get_offset")?),
                bo_get_modifier: fn_ptr(need(c"gbm_bo_get_modifier")?),
                bo_destroy: fn_ptr(need(c"gbm_bo_destroy")?),
            })
        }
    }
}

/// Mesa's GBM, opened at runtime. Only allocation is used: each buffer object is destroyed as
/// soon as its planes are exported as dmabuf fds, which keep the memory alive.
struct Gbm {
    fns: GbmFns,
    device: *mut c_void,
    /// Only kept open: closed after the device, which `Drop::drop` destroys first.
    _render_node: OwnedFd,
    /// Unloaded last. `None` only for the tests' stand-in functions.
    _lib: Option<libloading::Library>,
}

impl Gbm {
    fn open(render_node: &CStr) -> Result<Self, String> {
        let lib = unsafe { libloading::Library::new("libgbm.so.1") }.map_err(|e| format!("libgbm.so.1: {e}"))?;
        let get = |name: &CStr| {
            unsafe { lib.get::<*const c_void>(name.to_bytes_with_nul()) }
                .ok()
                .map(|s| *s)
        };
        // SAFETY: the addresses come from libgbm itself, under the names GbmFns calls them by.
        let mut gbm = unsafe { Self::open_from(get, render_node) }?;
        gbm._lib = Some(lib);
        Ok(gbm)
    }

    /// Resolves every function (`GbmFns`), and only then opens `render_node` and creates the device.
    ///
    /// # Safety
    /// As [`GbmFns::resolve`].
    unsafe fn open_from(get: impl Fn(&CStr) -> Option<*const c_void>, render_node: &CStr) -> Result<Self, String> {
        let fns = unsafe { GbmFns::resolve(get) }?;
        Self::open_with(fns, render_node)
    }

    fn open_with(fns: GbmFns, render_node: &CStr) -> Result<Self, String> {
        let fd = unsafe { libc::open(render_node.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(format!(
                "open {}: {}",
                render_node.to_string_lossy(),
                std::io::Error::last_os_error()
            ));
        }
        let render_node = unsafe { OwnedFd::from_raw_fd(fd) };
        let device = unsafe { (fns.create_device)(render_node.as_raw_fd()) };
        if device.is_null() {
            return Err("gbm_create_device failed".into());
        }
        Ok(Self {
            fns,
            device,
            _render_node: render_node,
            _lib: None,
        })
    }
}

impl Drop for Gbm {
    fn drop(&mut self) {
        unsafe { (self.fns.device_destroy)(self.device) };
    }
}

/// One presentation buffer. The dmabuf fds are shared with every `GdkTexture` built over it and
/// close only when the last of those is released, so GTK never holds a closed fd.
struct Buffer {
    width: i32,
    height: i32,
    fds: Arc<Vec<OwnedFd>>,
    strides: [u32; MAX_PLANES],
    offsets: [u32; MAX_PLANES],
    modifier: u64,
    image: EglImage,
    texture: u32,
    /// Set while a `GdkTexture` over this buffer is held by GTK ([`Reservation`]); cleared when
    /// GTK releases it (on any thread). A buffer is drawn into only while clear.
    in_use: Arc<AtomicBool>,
}

/// What a `GdkTexture` over a buffer holds: the fds it names, and the buffer's `in_use` flag,
/// cleared when this is dropped.
struct Lease {
    _fds: Arc<Vec<OwnedFd>>,
    in_use: Arc<AtomicBool>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.in_use.store(false, Ordering::Release);
    }
}

/// A buffer's reservation for one texture. [`Reservation::end`] is idempotent and ends it from
/// either side: GTK's release function, or our own error path when GTK refused to build the
/// texture -- GTK then neither calls nor frees the release function it was handed, so a lease
/// moved into that closure would keep the fds open for good.
#[derive(Clone)]
struct Reservation(Arc<Mutex<Option<Lease>>>);

impl Reservation {
    fn new(fds: &Arc<Vec<OwnedFd>>, in_use: &Arc<AtomicBool>) -> Self {
        in_use.store(true, Ordering::Release);
        Self(Arc::new(Mutex::new(Some(Lease {
            _fds: fds.clone(),
            in_use: in_use.clone(),
        }))))
    }

    fn end(&self) {
        let lease = self.0.lock().unwrap_or_else(PoisonError::into_inner).take();
        drop(lease);
    }
}

/// Whether frames get an explicit write fence (module doc). Decided at the first frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExplicitFence {
    Untried,
    Available,
    Unavailable,
}

/// GL-context-bound state of the editor's own presentation path. Created with the `GtkGLArea`'s
/// context current; `release` must also run with it current (the pane's unrealize).
pub(crate) struct Presenter {
    fns: Fns,
    display: EglDisplay,
    gbm: Gbm,
    modifiers: Vec<u64>,
    framebuffer: u32,
    depth_stencil: u32,
    depth_stencil_size: (i32, i32),
    buffers: Vec<Buffer>,
    current: Option<usize>,
    logged_modifier: bool,
    warned_buffer_count: bool,
    fence: ExplicitFence,
}

/// Which modifiers to ask GBM for: those EGL can render into (not external-only), that GTK can
/// import, and that are not linear -- a linear buffer is exactly the cost this path exists to
/// avoid. Pure, so the choice is tested without a GPU.
pub(crate) fn usable_modifiers(egl: &[(u64, bool)], gtk_imports: impl Fn(u64) -> bool) -> Vec<u64> {
    egl.iter()
        .filter(|(m, external_only)| {
            !*external_only && *m != DRM_FORMAT_MOD_LINEAR && *m != DRM_FORMAT_MOD_INVALID && gtk_imports(*m)
        })
        .map(|(m, _)| *m)
        .collect()
}

impl Presenter {
    /// Requires the GL context the frames will be drawn with to be current.
    pub(crate) fn new(gdk_display: &gdk::Display) -> Result<Self, String> {
        if !egl_loaded() {
            return Err("libEGL is not loaded, so the GL context is not an EGL one".into());
        }
        let fns = unsafe { Fns::load()? };
        let display = unsafe { (fns.egl_get_current_display)() };
        if display.is_null() {
            return Err("the GL context is not an EGL context".into());
        }
        let has_egl = |name: &CStr| unsafe { (fns.has_egl_extension)(display, name.as_ptr()) };
        for ext in [c"EGL_EXT_image_dma_buf_import_modifiers", c"EGL_KHR_image_base"] {
            if !has_egl(ext) {
                return Err(format!("{} missing", ext.to_string_lossy()));
            }
        }
        if !unsafe { (fns.has_gl_extension)(c"GL_OES_EGL_image".as_ptr()) } {
            return Err("GL_OES_EGL_image missing".into());
        }
        let client = unsafe { (fns.egl_query_string)(std::ptr::null_mut(), EGL_EXTENSIONS) };
        let client = if client.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(client) }.to_string_lossy().into_owned()
        };
        if !client
            .split(' ')
            .any(|e| e == "EGL_EXT_device_query" || e == "EGL_EXT_device_base")
        {
            return Err("EGL_EXT_device_query missing".into());
        }
        let mut device: isize = 0;
        if unsafe { (fns.egl_query_display_attrib)(display, EGL_DEVICE_EXT, &mut device) } == 0 || device == 0 {
            return Err("eglQueryDisplayAttribEXT(EGL_DEVICE_EXT) failed".into());
        }
        let device = device as *mut c_void;
        let device_exts = unsafe { (fns.egl_query_device_string)(device, EGL_EXTENSIONS) };
        let device_exts = if device_exts.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(device_exts) }.to_string_lossy().into_owned()
        };
        if !device_exts.split(' ').any(|e| e == "EGL_EXT_device_drm_render_node") {
            return Err("EGL_EXT_device_drm_render_node missing".into());
        }
        let node = unsafe { (fns.egl_query_device_string)(device, EGL_DRM_RENDER_NODE_FILE_EXT) };
        if node.is_null() {
            return Err("the EGL device has no render node".into());
        }
        let node = unsafe { CStr::from_ptr(node) }.to_owned();

        let mut count = 0i32;
        unsafe {
            (fns.egl_query_dmabuf_modifiers)(
                display,
                DRM_FORMAT_ABGR8888 as i32,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut count,
            )
        };
        let mut mods = vec![0u64; count.max(0) as usize];
        let mut external = vec![0u32; count.max(0) as usize];
        if count > 0 {
            let asked = count;
            let ok = unsafe {
                (fns.egl_query_dmabuf_modifiers)(
                    display,
                    DRM_FORMAT_ABGR8888 as i32,
                    asked,
                    mods.as_mut_ptr(),
                    external.as_mut_ptr(),
                    &mut count,
                )
            };
            if ok == 0 {
                count = 0;
            }
            let returned = count.clamp(0, asked) as usize;
            mods.truncate(returned);
            external.truncate(returned);
        }
        let egl: Vec<(u64, bool)> = mods.iter().zip(&external).map(|(m, e)| (*m, *e != 0)).collect();
        let gtk_formats = gdk_display.dmabuf_formats();
        let modifiers = usable_modifiers(&egl, |m| gtk_formats.contains(DRM_FORMAT_ABGR8888, m));
        if modifiers.is_empty() {
            return Err(format!(
                "no tiled AB24 modifier both EGL renders to and GTK imports (EGL offered {})",
                egl.len()
            ));
        }
        let gbm = Gbm::open(&node)?;

        let mut framebuffer = 0;
        unsafe { (fns.gl_gen_framebuffers)(1, &mut framebuffer) };
        let mut depth_stencil = 0;
        unsafe { (fns.gl_gen_renderbuffers)(1, &mut depth_stencil) };
        Ok(Self {
            fns,
            display,
            gbm,
            modifiers,
            framebuffer,
            depth_stencil,
            depth_stencil_size: (0, 0),
            buffers: Vec::new(),
            current: None,
            logged_modifier: false,
            warned_buffer_count: false,
            fence: ExplicitFence::Untried,
        })
    }

    /// Picks a buffer GTK is not reading, of exactly `width x height` device pixels (allocating
    /// one if none is free), and binds this presenter's framebuffer with it and a
    /// depth/stencil renderbuffer attached -- the same attachments `GtkGLArea` gives its own
    /// framebuffer with `has_stencil_buffer`. The GL context must be current.
    pub(crate) fn begin_frame(&mut self, width: i32, height: i32) -> Result<(), String> {
        self.current = None;
        // Free buffers of another size go now; busy ones when GTK has let go of them.
        let mut i = 0;
        while i < self.buffers.len() {
            let b = &self.buffers[i];
            if (b.width, b.height) != (width, height) && !b.in_use.load(Ordering::Acquire) {
                let b = self.buffers.swap_remove(i);
                self.destroy_gl(&b);
            } else {
                i += 1;
            }
        }
        let index = match self
            .buffers
            .iter()
            .position(|b| (b.width, b.height) == (width, height) && !b.in_use.load(Ordering::Acquire))
        {
            Some(index) => index,
            None => {
                let buffer = self.allocate(width, height)?;
                self.buffers.push(buffer);
                if self.buffers.len() > 6 && !self.warned_buffer_count {
                    self.warned_buffer_count = true;
                    eprintln!(
                        "[editor] {} presentation buffers alive: GTK is holding more frames than expected",
                        self.buffers.len()
                    );
                }
                self.buffers.len() - 1
            }
        };
        let f = &self.fns;
        unsafe {
            if self.depth_stencil_size != (width, height) {
                (f.gl_bind_renderbuffer)(GL_RENDERBUFFER, self.depth_stencil);
                (f.gl_renderbuffer_storage)(GL_RENDERBUFFER, GL_DEPTH24_STENCIL8, width, height);
                self.depth_stencil_size = (width, height);
            }
            (f.gl_bind_framebuffer)(GL_FRAMEBUFFER, self.framebuffer);
            (f.gl_framebuffer_texture_2d)(
                GL_FRAMEBUFFER,
                GL_COLOR_ATTACHMENT0,
                GL_TEXTURE_2D,
                self.buffers[index].texture,
                0,
            );
            (f.gl_framebuffer_renderbuffer)(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, GL_RENDERBUFFER, self.depth_stencil);
            (f.gl_framebuffer_renderbuffer)(
                GL_FRAMEBUFFER,
                GL_STENCIL_ATTACHMENT,
                GL_RENDERBUFFER,
                self.depth_stencil,
            );
            let status = (f.gl_check_framebuffer_status)(GL_FRAMEBUFFER);
            if status != GL_FRAMEBUFFER_COMPLETE {
                return Err(format!("framebuffer incomplete: {status:#x}"));
            }
            // As `gtk_gl_area_snapshot` does for an area without a depth buffer.
            (f.gl_disable)(GL_DEPTH_TEST);
        }
        if !self.logged_modifier {
            self.logged_modifier = true;
            let b = &self.buffers[index];
            println!(
                "[editor] drawing into own {}x{} presentation buffers, AB24 modifier {:#018x} ({} plane(s)) \
                 instead of GtkGLArea's texture",
                width,
                height,
                b.modifier,
                b.fds.len()
            );
        }
        self.current = Some(index);
        Ok(())
    }

    fn allocate(&self, width: i32, height: i32) -> Result<Buffer, String> {
        let g = &self.gbm;
        let gf = &g.fns;
        let (w, h) = (width as u32, height as u32);
        let n = self.modifiers.len() as u32;
        let bo = unsafe {
            match gf.bo_create_with_modifiers2 {
                Some(create2) => create2(
                    g.device,
                    w,
                    h,
                    DRM_FORMAT_ABGR8888,
                    self.modifiers.as_ptr(),
                    n,
                    GBM_BO_USE_RENDERING,
                ),
                None => (gf.bo_create_with_modifiers)(g.device, w, h, DRM_FORMAT_ABGR8888, self.modifiers.as_ptr(), n),
            }
        };
        if bo.is_null() {
            return Err(format!("gbm could not allocate a {width}x{height} tiled buffer"));
        }
        let planes = unsafe { (gf.bo_get_plane_count)(bo) };
        let modifier = unsafe { (gf.bo_get_modifier)(bo) };
        let mut fds = Vec::new();
        let mut strides = [0u32; MAX_PLANES];
        let mut offsets = [0u32; MAX_PLANES];
        let mut failed = None;
        if !(1..=MAX_PLANES as c_int).contains(&planes) {
            failed = Some(format!("gbm buffer has {planes} planes"));
        } else {
            for p in 0..planes {
                let fd = unsafe { (gf.bo_get_fd_for_plane)(bo, p) };
                if fd < 0 {
                    failed = Some(format!("gbm_bo_get_fd_for_plane({p}) failed"));
                    break;
                }
                fds.push(unsafe { OwnedFd::from_raw_fd(fd) });
                strides[p as usize] = unsafe { (gf.bo_get_stride_for_plane)(bo, p) };
                offsets[p as usize] = unsafe { (gf.bo_get_offset)(bo, p) };
            }
        }
        // The fds keep the memory; the buffer object is only the allocator's handle.
        unsafe { (gf.bo_destroy)(bo) };
        if let Some(err) = failed {
            return Err(err);
        }
        if modifier == DRM_FORMAT_MOD_LINEAR || modifier == DRM_FORMAT_MOD_INVALID {
            return Err(format!("gbm chose modifier {modifier:#x}, not a tiled one"));
        }

        let mut attrs = vec![
            EGL_WIDTH,
            width,
            EGL_HEIGHT,
            height,
            EGL_LINUX_DRM_FOURCC_EXT,
            DRM_FORMAT_ABGR8888 as i32,
        ];
        for (p, fd) in fds.iter().enumerate() {
            let [a_fd, a_offset, a_pitch, a_lo, a_hi] = EGL_PLANE_ATTRS[p];
            attrs.extend_from_slice(&[
                a_fd,
                fd.as_raw_fd(),
                a_offset,
                offsets[p] as i32,
                a_pitch,
                strides[p] as i32,
                a_lo,
                (modifier & 0xffff_ffff) as u32 as i32,
                a_hi,
                (modifier >> 32) as u32 as i32,
            ]);
        }
        attrs.push(EGL_NONE);
        let f = &self.fns;
        let image = unsafe {
            (f.egl_create_image)(
                self.display,
                std::ptr::null_mut(),
                EGL_LINUX_DMA_BUF_EXT,
                std::ptr::null_mut(),
                attrs.as_ptr(),
            )
        };
        if image.is_null() {
            return Err(format!(
                "EGL could not import the {width}x{height} buffer (modifier {modifier:#x})"
            ));
        }
        let mut texture = 0;
        unsafe {
            (f.gl_gen_textures)(1, &mut texture);
            (f.gl_bind_texture)(GL_TEXTURE_2D, texture);
            (f.gl_tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            (f.gl_tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            (f.gl_egl_image_target_texture_2d)(GL_TEXTURE_2D, image);
        }
        Ok(Buffer {
            width,
            height,
            fds: Arc::new(fds),
            strides,
            offsets,
            modifier,
            image,
            texture,
            in_use: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Submits the frame drawn since `begin_frame`, fences it (module doc) and wraps its buffer as
    /// a `GdkTexture` for this frame's snapshot. The buffer stays reserved until GTK releases that
    /// texture.
    pub(crate) fn finish_frame(&mut self, gdk_display: &gdk::Display) -> Result<gdk::Texture, String> {
        let index = self.current.take().ok_or("finish_frame without begin_frame")?;
        self.submit_and_fence(index);
        let b = &self.buffers[index];
        let mut builder = gdk::DmabufTextureBuilder::new()
            .set_display(gdk_display)
            .set_width(b.width as u32)
            .set_height(b.height as u32)
            .set_fourcc(DRM_FORMAT_ABGR8888)
            .set_modifier(b.modifier)
            .set_premultiplied(true)
            .set_n_planes(b.fds.len() as u32);
        for (p, fd) in b.fds.iter().enumerate() {
            // SAFETY: the reservation below holds `fds` until GTK releases the texture, so every
            // fd outlives the texture that names it.
            builder = unsafe { builder.set_fd(p as u32, fd.as_raw_fd()) }
                .set_stride(p as u32, b.strides[p])
                .set_offset(p as u32, b.offsets[p]);
        }
        let reservation = Reservation::new(&b.fds, &b.in_use);
        let for_gtk = reservation.clone();
        let built = unsafe { builder.build_with_release_func(move || for_gtk.end()) };
        built.map_err(|e| {
            reservation.end();
            format!("GTK refused the buffer: {e}")
        })
    }

    /// `glFlush`es the frame and, where the platform allows, inserts a native fence for it into
    /// the buffer's dmabuf as a write (module doc). Falls back to the implicit fence alone, once.
    fn submit_and_fence(&mut self, index: usize) {
        let f = &self.fns;
        if self.fence == ExplicitFence::Untried {
            let has = |name: &CStr| unsafe { (f.has_egl_extension)(self.display, name.as_ptr()) };
            self.fence = if has(c"EGL_KHR_fence_sync") && has(c"EGL_ANDROID_native_fence_sync") {
                ExplicitFence::Available
            } else {
                eprintln!("[editor] no EGL_ANDROID_native_fence_sync; presentation relies on the implicit fence");
                ExplicitFence::Unavailable
            };
        }
        if self.fence != ExplicitFence::Available {
            unsafe { (f.gl_flush)() };
            return;
        }
        let attrs = [EGL_NONE];
        let sync = unsafe { (f.egl_create_sync)(self.display, EGL_SYNC_NATIVE_FENCE_ANDROID, attrs.as_ptr()) };
        // The native fence's fd exists only once the commands before it are flushed.
        unsafe { (f.gl_flush)() };
        let result = if sync.is_null() {
            Err("eglCreateSyncKHR failed".to_string())
        } else {
            let fd = unsafe { (f.egl_dup_native_fence_fd)(self.display, sync) };
            unsafe { (f.egl_destroy_sync)(self.display, sync) };
            if fd == EGL_NO_NATIVE_FENCE_FD_ANDROID || fd < 0 {
                Err("eglDupNativeFenceFDANDROID returned no fd".to_string())
            } else {
                let fence = unsafe { OwnedFd::from_raw_fd(fd) };
                import_write_fence(&self.buffers[index].fds, &fence)
            }
        };
        if let Err(err) = result {
            eprintln!("[editor] explicit presentation fence unavailable ({err}); relying on the implicit fence");
            self.fence = ExplicitFence::Unavailable;
        }
    }

    fn destroy_gl(&self, b: &Buffer) {
        unsafe {
            (self.fns.gl_delete_textures)(1, &b.texture);
            (self.fns.egl_destroy_image)(self.display, b.image);
        }
    }

    /// Deletes every GL object if the context they live in is current (otherwise they go with
    /// it), and every EGL image, which belongs to the display. Buffers GTK still holds keep their
    /// memory through the fds its textures own.
    pub(crate) fn release(mut self, context_is_current: bool) {
        for b in std::mem::take(&mut self.buffers) {
            if context_is_current {
                unsafe { (self.fns.gl_delete_textures)(1, &b.texture) };
            }
            unsafe { (self.fns.egl_destroy_image)(self.display, b.image) };
        }
        if context_is_current {
            unsafe {
                (self.fns.gl_delete_framebuffers)(1, &self.framebuffer);
                (self.fns.gl_delete_renderbuffers)(1, &self.depth_stencil);
            }
        }
    }
}

/// Inserts `fence` into every plane's dmabuf as a write, so a reader that waits on the dmabuf's
/// implicit fences waits for it (`DMA_BUF_IOCTL_IMPORT_SYNC_FILE`).
fn import_write_fence(fds: &[OwnedFd], fence: &OwnedFd) -> Result<(), String> {
    for fd in fds {
        let mut arg = DmaBufImportSyncFile {
            flags: DMA_BUF_SYNC_WRITE,
            fd: fence.as_raw_fd(),
        };
        if unsafe { libc::ioctl(fd.as_raw_fd(), DMA_BUF_IOCTL_IMPORT_SYNC_FILE, &mut arg) } != 0 {
            return Err(format!(
                "DMA_BUF_IOCTL_IMPORT_SYNC_FILE: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fallback_hint_needs_gtk_4_16_the_fallback_and_a_vulkan_renderer() {
        let vk = "GskVulkanRenderer";
        let hint = fallback_hint(16, true, vk).expect("hint");
        assert!(
            hint.contains("GDK_DEBUG=dmabuf") && hint.contains("for downloading") && hint.contains("GSK_RENDERER=gl")
        );
        assert!(fallback_hint(22, true, vk).is_some());
        assert!(fallback_hint(14, true, vk).is_none(), "GTK 4.14 exports nothing");
        assert!(fallback_hint(22, false, vk).is_none(), "own buffers in use");
        assert!(fallback_hint(22, true, "GskGLRenderer").is_none());
        assert!(fallback_hint(22, true, "GskNglRenderer").is_none());
        assert!(fallback_hint(22, true, "").is_none());
    }

    const MTL_RC_CCS_CC: u64 = 0x0100_0000_0000_000f;
    const TILE4: u64 = 0x0100_0000_0000_0009;

    #[test]
    fn linear_is_never_asked_for_even_when_it_is_the_only_common_modifier() {
        let egl = [(DRM_FORMAT_MOD_LINEAR, false), (TILE4, false)];
        assert_eq!(
            usable_modifiers(&egl, |m| m == DRM_FORMAT_MOD_LINEAR),
            Vec::<u64>::new()
        );
    }

    #[test]
    fn external_only_and_not_importable_modifiers_are_dropped_and_order_is_kept() {
        let egl = [
            (MTL_RC_CCS_CC, false),
            (0x0100_0000_0000_000e, true), // media-compressed: EGL can only sample it
            (TILE4, false),
            (DRM_FORMAT_MOD_INVALID, false),
            (0x0100_0000_0000_0001, false), // X-tiled, which this GTK does not list
        ];
        let gtk = [MTL_RC_CCS_CC, 0x0100_0000_0000_000e, TILE4, DRM_FORMAT_MOD_INVALID];
        assert_eq!(usable_modifiers(&egl, |m| gtk.contains(&m)), vec![MTL_RC_CCS_CC, TILE4]);
    }

    #[test]
    fn the_setting_reads_explicit_off_and_on_values_and_nothing_else() {
        for off in ["0", "off", "OFF", "false", "No", " 0 "] {
            assert_eq!(setting(Some(OsStr::new(off))), Setting::Off, "{off:?}");
        }
        for on in ["1", "on", "True", "YES", " 1"] {
            assert_eq!(setting(Some(OsStr::new(on))), Setting::On, "{on:?}");
        }
        for other in ["", "2", "dmabuf", "auto"] {
            assert_eq!(setting(Some(OsStr::new(other))), Setting::Default, "{other:?}");
        }
        assert_eq!(setting(None), Setting::Default);
    }

    const GTK_4_14: (u32, u32, u32) = (4, 14, 5);
    const GTK_4_16: (u32, u32, u32) = (4, 16, 0);
    const GTK_4_22: (u32, u32, u32) = (4, 22, 5);

    #[test]
    fn by_default_the_path_runs_from_gtk_4_16_and_says_why_it_does_not_below() {
        assert_eq!(decide(None, GTK_4_16), Ok(None));
        assert_eq!(decide(None, GTK_4_22), Ok(None));
        assert_eq!(decide(None, (5, 0, 0)), Ok(None));
        assert_eq!(decide(Some(OsStr::new("")), GTK_4_22), Ok(None));
        for below in [GTK_4_14, (4, 15, 9)] {
            let reason = decide(None, below).expect_err("off below 4.16 by default");
            let (major, minor, micro) = below;
            assert!(reason.contains(&format!("GTK {major}.{minor}.{micro}")), "{reason}");
            assert!(reason.contains("EITRI_EDITOR_DMABUF=1 forces it"), "{reason}");
        }
    }

    #[test]
    fn one_forces_the_path_below_gtk_4_16_and_says_so_and_zero_forces_the_fallback_everywhere() {
        let note = decide(Some(OsStr::new("1")), GTK_4_14)
            .expect("forced on")
            .expect("a forced run below 4.16 is noted");
        assert!(note.contains("forced by EITRI_EDITOR_DMABUF=1 on GTK 4.14.5"), "{note}");
        assert_eq!(
            decide(Some(OsStr::new("on")), GTK_4_22),
            Ok(None),
            "nothing to note where measured"
        );
        for gtk in [GTK_4_14, GTK_4_16, GTK_4_22] {
            assert_eq!(
                decide(Some(OsStr::new("0")), gtk),
                Err("disabled by EITRI_EDITOR_DMABUF=0".to_string())
            );
        }
    }

    // Stand-ins for libgbm's functions, counting per thread (each test is one thread).
    thread_local! {
        static CREATED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        static DESTROYED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        static CREATE_RETURNS_NULL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    unsafe extern "C" fn fake_create_device(fd: c_int) -> *mut c_void {
        assert!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0,
            "the render node is open"
        );
        CREATED.with(|c| c.set(c.get() + 1));
        if CREATE_RETURNS_NULL.with(|n| n.get()) {
            std::ptr::null_mut()
        } else {
            std::ptr::NonNull::<u8>::dangling().as_ptr().cast()
        }
    }
    unsafe extern "C" fn fake_device_destroy(_device: *mut c_void) {
        DESTROYED.with(|c| c.set(c.get() + 1));
    }
    /// Never called: every other slot only has to resolve.
    unsafe extern "C" fn fake_unused() {
        unreachable!("only the device's creation and destruction are called here");
    }

    fn fake_libgbm(missing: &'static CStr) -> impl Fn(&CStr) -> Option<*const c_void> {
        move |name| {
            if name == missing {
                None
            } else if name == c"gbm_create_device" {
                Some(fake_create_device as *const c_void)
            } else if name == c"gbm_device_destroy" {
                Some(fake_device_destroy as *const c_void)
            } else {
                Some(fake_unused as *const c_void)
            }
        }
    }

    #[test]
    fn a_libgbm_missing_any_function_creates_no_device() {
        for missing in [c"gbm_bo_get_fd_for_plane", c"gbm_device_destroy", c"gbm_bo_destroy"] {
            let err = unsafe { Gbm::open_from(fake_libgbm(missing), c"/dev/null") }
                .map(|_| ())
                .expect_err("a missing function fails");
            assert!(err.contains(&*missing.to_string_lossy()), "{err}");
        }
        assert_eq!(CREATED.with(|c| c.get()), 0, "no device was created, so none leaked");
    }

    #[test]
    fn an_opened_device_is_destroyed_once_and_a_failed_one_never() {
        // The optional function may be absent.
        let gbm =
            unsafe { Gbm::open_from(fake_libgbm(c"gbm_bo_create_with_modifiers2"), c"/dev/null") }.expect("opens");
        assert!(gbm.fns.bo_create_with_modifiers2.is_none());
        assert_eq!((CREATED.with(|c| c.get()), DESTROYED.with(|c| c.get())), (1, 0));
        drop(gbm);
        assert_eq!(DESTROYED.with(|c| c.get()), 1);

        CREATE_RETURNS_NULL.with(|n| n.set(true));
        let err = unsafe { Gbm::open_from(fake_libgbm(c"none"), c"/dev/null") }
            .map(|_| ())
            .expect_err("no device");
        assert_eq!(err, "gbm_create_device failed");
        assert_eq!((CREATED.with(|c| c.get()), DESTROYED.with(|c| c.get())), (2, 1));
    }

    /// A pipe stands in for a dmabuf fd: its read end sees EOF exactly when every copy of the
    /// write end is closed, which tells whether the texture's reservation still holds the fds
    /// without reading a recyclable fd number.
    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        assert_eq!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
            0
        );
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    fn write_end_open(read: &OwnedFd) -> bool {
        let mut byte = 0u8;
        let n = unsafe { libc::read(read.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) };
        // EAGAIN: nothing written but a writer is still open. 0: EOF, every writer closed.
        n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock
    }

    #[test]
    fn a_texture_keeps_the_fds_open_after_the_buffer_is_gone_and_releasing_frees_the_buffer() {
        let (read, write) = pipe();
        let fds = Arc::new(vec![write]);
        let in_use = Arc::new(AtomicBool::new(false));
        let reservation = Reservation::new(&fds, &in_use);
        let for_gtk = reservation.clone();
        assert!(in_use.load(Ordering::Acquire));
        drop(fds); // the buffer itself is destroyed (resize, unrealize) while GTK holds the texture
        assert!(write_end_open(&read), "GTK's texture still names the fd");
        for_gtk.end(); // GTK's release function
        assert!(!in_use.load(Ordering::Acquire));
        assert!(!write_end_open(&read), "the last holder closed it");
        reservation.end(); // idempotent
    }

    #[test]
    fn a_texture_gtk_refused_to_build_leaks_neither_the_fds_nor_the_reservation() {
        let (read, write) = pipe();
        let fds = Arc::new(vec![write]);
        let in_use = Arc::new(AtomicBool::new(false));
        let reservation = Reservation::new(&fds, &in_use);
        // GTK neither calls nor frees the release function of a texture it refused: model that
        // closure as leaked.
        std::mem::forget(reservation.clone());
        reservation.end(); // `finish_frame`'s error path
        assert!(!in_use.load(Ordering::Acquire), "the buffer is free again");
        drop(fds);
        assert!(!write_end_open(&read), "the leaked closure holds no fd");
    }
}
