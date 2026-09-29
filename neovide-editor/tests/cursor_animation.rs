//! Real GTK/GLArea regressions for cursor animation and presentation-surface lifetime.
//!
//! Run ONLY in the one, isolated GUI sandbox permitted by AGENTS.md:
//! `cargo test -p neovide-editor --test cursor_animation -- --ignored`
//! Since 2026-09-29 it refuses anything else before GTK initialises (`sandbox_wayland_socket`): a
//! `WAYLAND_DISPLAY` that is unset, missing, not a socket, or resolves into the login runtime
//! directory (`/run/user/`, where a desktop's sockets live; every sandbox harness here uses a
//! private runtime directory of its own), and any X display -- `DISPLAY` and `WAYLAND_SOCKET` are
//! dropped and `GDK_BACKEND=wayland` set. Unlike the Xvfb tests (`shell/tests/support/own_x_server.rs`)
//! it cannot start a display of its own: it measures GL/dmabuf presentation on a real compositor.
//! **Fix round 1 (the same day):** a path is not an identity, so it also refuses a socket that IS one
//! of the login runtime directory's, reached through a bind mount or a hard link (same device and
//! inode), and -- once GTK has connected, before any window exists -- a compositor with any output
//! that is not `HEADLESS-<n>` (`require_headless_outputs`): a real screen, or a window on one, as a
//! nested compositor's output is. And it runs GTK's own input method, never an inherited
//! `GTK_IM_MODULE`: with `fcitx` the pane's input context reached for fcitx5 over the session bus as
//! soon as it took focus -- on the desktop's bus, the owner's live one.
//! **Fix round 2 (the same day):** the output check ran only after `gtk4::init()` had connected, so a
//! desktop whose runtime directory is not `/run/user/` (`/tmp/runtime-<user>`) was connected to before
//! it was refused. Now, before GTK, the process listening on the socket is found through `/proc`
//! without connecting, and must have been started with `WLR_BACKENDS=headless`
//! (`require_headless_compositor`); the output check stays as the second, after-connect half. A
//! headless compositor someone watches remotely (wayvnc) still passes both.
//! **Fix round 3 (the same day):** the listener is found by the socket file itself, as the kernel
//! records it, not by the name `/proc/net/unix` prints (a rename, a bind mount or another mount
//! namespace could make that name lead to a desktop's socket); its holder must also map `libwlroots`
//! (a compositor that is not wlroots ignores `WLR_BACKENDS`); and it runs with no session bus, as the
//! Xvfb tests do (`own_x_server::cut_session_bus`). Every run, `--ignored` or not, first holds the
//! refusal chain against fake compositors (`refusal_chain_self_test`), and
//! `shell/tests/own_display_scan.rs` checks that `main` still calls it all, in order.
//! **Fix round 4 (the same day):** the kernel's answer is believed only whole. A `NLMSG_DONE` carrying a
//! negative errno (a dump that failed part way), a reply flagged `NLM_F_DUMP_INTR` (the table changed
//! under it) and an attribute that does not add up are each an error, not the listeners found so far
//! (`parse_unix_diag_reply`, held on synthetic replies by `netlink_parser_self_test`). The refusal chain
//! skips, and says so, a case its machine cannot construct: with no `NETLINK_SOCK_DIAG` nothing can be
//! looked up, and as root or with `CAP_SYS_PTRACE` a non-dumpable holder is readable; the runtime check
//! is relaxed for neither. The scan now also requires GTK to be initialised exactly once, after the checks,
//! and every refusal in `main` to exit. The headless sway this test runs against must be started inside
//! the same `bwrap` wrapper as the test: a sway started outside it is not dumpable (`cap_sys_nice=ep`), so
//! the test refuses it, whatever it was started with.
//! A plain main keeps GTK and winit on the main thread. The test starts a real clean nvim,
//! drives the product pane, and reads its actual GL framebuffer after GTK paints. Red cursor
//! pixels on a black background prove intermediate positions; the harness's cursor getter only
//! reports the destination and would not prove animation. Readback is deliberately test-only.
//!
//! Which presentation path ran is printed, before and after the re-realize (`presentation:`), and
//! so are the pane's counts over the whole run, from its first frame through the motion checks,
//! the three resizes and the re-realize (`presentation counts:`, `NeovideEditorPane::presentation_counts`,
//! never reset). `-- --ignored --expect-own-buffers` fails if a single one of those frames went
//! through `GtkGLArea`'s texture or the own-buffer path failed even once -- a fallback that a later
//! re-realize or size change recovered from included; `NEOVIBE_EDITOR_DMABUF=0 ... -- --ignored
//! --expect-fallback` forces the fallback and fails if any frame was drawn into the own buffers, so
//! both paths pass the same pixel checks.
//!
//! An independent nvim RPC confirms each input's destination BEFORE the GTK loop resumes. This
//! makes the idle-gap regression deterministic instead of depending on whether nvim's redraw
//! happens to arrive before or after an extra, still-unchanged frame. These synchronized cases
//! test animation correctness, not input-to-photon latency or production GPU throughput.

use std::cell::{Cell, RefCell};
use std::ffi::{c_void, CString};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::glib::{self, translate::IntoGlib};
use gtk4::prelude::*;
use gtk4::{GLArea, Window};
use neovide_editor::{NeovideEditorPane, NeovideEditorPaneOptions};

// For `own_x_server::cut_session_bus` only (fix round 3, 2026-09-29): this test cannot use that file's
// Xvfb (`sandbox_wayland_socket` says why), but it runs with no session bus the same way.
#[path = "../../shell/tests/support/own_x_server.rs"]
mod own_x_server;

type Point = (f64, f64);

#[derive(Clone, Copy, Debug)]
struct Sample {
    at: Instant,
    /// When GTK emitted `render` for this frame (the pump of nvim's batch happens inside it).
    render_at: Instant,
    center: Option<Point>,
    pixels: usize,
    /// Near-white pixels in the captured region; only counted while `Capture::count_white` is set.
    white: usize,
}

#[derive(Clone, Copy, Debug)]
struct Region {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[derive(Default)]
struct Capture {
    enabled: Cell<bool>,
    region: Cell<Option<Region>>,
    last_render: Cell<u64>,
    samples: RefCell<Vec<Sample>>,
    error: RefCell<Option<String>>,
    keep_frame: Cell<bool>,
    frame: RefCell<Option<FramePixels>>,
    count_white: Cell<bool>,
}

struct FramePixels {
    region: Region,
    // OpenGL's bottom-to-top row order; pixel() presents widget coordinates to assertions.
    rgba: Vec<u8>,
}

impl FramePixels {
    fn pixel(&self, x: i32, y: i32) -> &[u8] {
        let region = self.region;
        assert!((region.x..region.x + region.width).contains(&x));
        assert!((region.y..region.y + region.height).contains(&y));
        let index = ((region.y + region.height - 1 - y) * region.width + x - region.x) as usize * 4;
        &self.rgba[index..index + 4]
    }

    fn white(&self) -> usize {
        self.rgba.chunks_exact(4).filter(|rgba| is_white(rgba)).count()
    }

    fn cursor(&self) -> (Option<Point>, usize) {
        let (mut count, mut sum_x, mut sum_y) = (0usize, 0.0, 0.0);
        for (index, rgba) in self.rgba.chunks_exact(4).enumerate() {
            if is_cursor(rgba) {
                count += 1;
                sum_x += f64::from(self.region.x) + (index % self.region.width as usize) as f64 + 0.5;
                sum_y +=
                    f64::from(self.region.y + self.region.height) - (index / self.region.width as usize) as f64 - 0.5;
            }
        }
        ((count > 0).then(|| (sum_x / count as f64, sum_y / count as f64)), count)
    }
}

fn is_white(rgba: &[u8]) -> bool {
    rgba[0] > 200 && rgba[1] > 200 && rgba[2] > 200
}

fn is_cursor(rgba: &[u8]) -> bool {
    rgba[0] > 160 && rgba[1] < 80 && rgba[2] < 80
}

// The pane's render handler returns Stop. A subsequently connected render handler therefore
// cannot count frames reliably; a signal emission hook runs before the handlers/accumulator.
struct HookData {
    area: usize,
    count: Rc<Cell<u64>>,
    framebuffer: Rc<Cell<i32>>,
    emitted_at: Rc<Cell<Instant>>,
}

struct RenderCounter {
    signal: u32,
    hook: libc::c_ulong,
    count: Rc<Cell<u64>>,
    framebuffer: Rc<Cell<i32>>,
    emitted_at: Rc<Cell<Instant>>,
}

impl RenderCounter {
    fn new(area: &GLArea) -> Self {
        unsafe extern "C" fn emitted(
            _: *mut glib::gobject_ffi::GSignalInvocationHint,
            len: u32,
            values: *const glib::gobject_ffi::GValue,
            data: *mut c_void,
        ) -> glib::ffi::gboolean {
            // GTK emits this signal only on the main thread; HookData lives until removal.
            let data = unsafe { &*(data as *const HookData) };
            if len > 0 && unsafe { glib::gobject_ffi::g_value_get_object(values) } as usize == data.area {
                data.emitted_at.set(Instant::now());
                data.count.set(data.count.get() + 1);
                let get = unsafe { gl_proc("glGetIntegerv") };
                if !get.is_null() {
                    let get: GetInteger = unsafe { std::mem::transmute(get) };
                    let mut framebuffer = 0;
                    // GTK attached this frame's target before emitting render. Remember it;
                    // attach_buffers after paint would attach NEXT frame's recycled texture.
                    unsafe { get(0x8CA6, &mut framebuffer) }; // GL_FRAMEBUFFER_BINDING
                    data.framebuffer.set(framebuffer);
                }
            }
            1
        }
        unsafe extern "C" fn destroy(data: *mut c_void) {
            drop(unsafe { Box::from_raw(data as *mut HookData) });
        }

        let count = Rc::new(Cell::new(0));
        let framebuffer = Rc::new(Cell::new(0));
        let emitted_at = Rc::new(Cell::new(Instant::now()));
        let signal =
            unsafe { glib::gobject_ffi::g_signal_lookup(c"render".as_ptr(), GLArea::static_type().into_glib()) };
        assert_ne!(signal, 0, "GtkGLArea has no render signal");
        let data = Box::into_raw(Box::new(HookData {
            area: area.as_ptr() as usize,
            count: count.clone(),
            framebuffer: framebuffer.clone(),
            emitted_at: emitted_at.clone(),
        }));
        let hook = unsafe {
            glib::gobject_ffi::g_signal_add_emission_hook(signal, 0, Some(emitted), data.cast(), Some(destroy))
        };
        assert_ne!(hook, 0, "cannot observe GtkGLArea render emissions");
        Self {
            signal,
            hook,
            count,
            framebuffer,
            emitted_at,
        }
    }
}

impl Drop for RenderCounter {
    fn drop(&mut self) {
        unsafe { glib::gobject_ffi::g_signal_remove_emission_hook(self.signal, self.hook) };
    }
}

type ReadPixels = unsafe extern "C" fn(i32, i32, i32, i32, u32, u32, *mut c_void);
type GetError = unsafe extern "C" fn() -> u32;
type GetInteger = unsafe extern "C" fn(u32, *mut i32);
type BindFramebuffer = unsafe extern "C" fn(u32, u32);

/// libepoxy exposes function-pointer variables, matching the product's own GL loader.
unsafe fn gl_proc(name: &str) -> *const c_void {
    let library = libloading::os::unix::Library::this();
    let epoxy = CString::new(format!("epoxy_{name}")).unwrap();
    if let Ok(symbol) = unsafe { library.get::<*const c_void>(epoxy.as_bytes_with_nul()) } {
        let slot = *symbol as *const *const c_void;
        if !slot.is_null() {
            let address = unsafe { *slot };
            if !address.is_null() {
                return address;
            }
        }
    }
    let name = CString::new(name).unwrap();
    unsafe { library.get::<*const c_void>(name.as_bytes_with_nul()) }
        .map(|symbol| *symbol)
        .unwrap_or(std::ptr::null())
}

fn read_frame(area: &GLArea, framebuffer: i32, region: Option<Region>) -> Result<FramePixels, String> {
    area.make_current();
    if let Some(error) = area.error() {
        return Err(format!("GtkGLArea context: {error}"));
    }
    // gtk_gl_area_snapshot keeps the completed attachment on its FBO but sets its private
    // current texture to NULL after publishing it. attach_buffers() here would consequently
    // allocate/recycle and attach a DIFFERENT texture. Rebind only the FBO recorded at render
    // entry; do not ask GTK to prepare a new frame. See gtk/gtkglarea.c in GNOME/gtk.
    if framebuffer <= 0 {
        return Err("render hook did not capture the GtkGLArea framebuffer".into());
    }
    let read = unsafe { gl_proc("glReadPixels") };
    let error = unsafe { gl_proc("glGetError") };
    let get = unsafe { gl_proc("glGetIntegerv") };
    let bind = unsafe { gl_proc("glBindFramebuffer") };
    if read.is_null() || error.is_null() || get.is_null() || bind.is_null() {
        return Err("libepoxy did not expose the GL framebuffer readback functions".into());
    }
    let read: ReadPixels = unsafe { std::mem::transmute(read) };
    let error: GetError = unsafe { std::mem::transmute(error) };
    let get: GetInteger = unsafe { std::mem::transmute(get) };
    let bind: BindFramebuffer = unsafe { std::mem::transmute(bind) };
    let scale = area.scale_factor();
    let (width, height) = (area.width() * scale, area.height() * scale);
    let region = region.unwrap_or(Region {
        x: 0,
        y: 0,
        width,
        height,
    });
    if region.width <= 0 || region.height <= 0 {
        return Err("empty framebuffer capture".into());
    }
    // A shared GL context need not retain default pixel-pack state. Check the layout BEFORE
    // giving the driver this exact-size Rust buffer: row padding/skip could write past its end,
    // and a bound pixel-pack buffer would reinterpret the pointer as a GPU-buffer offset.
    // These checks leave that state untouched; restore only our changed read target afterwards.
    let mut pixels = vec![0u8; (region.width * region.height * 4) as usize];
    unsafe {
        // Clear any error GTK's preceding rendering left, then check this call itself.
        for _ in 0..16 {
            if error() == 0 {
                break;
            }
        }
        let mut previous_read = 0;
        get(0x8CAA, &mut previous_read); // GL_READ_FRAMEBUFFER_BINDING
        let mut pack = [0i32; 5];
        for (parameter, value) in [
            0x0D05, // GL_PACK_ALIGNMENT
            0x0D02, // GL_PACK_ROW_LENGTH
            0x0D03, // GL_PACK_SKIP_ROWS
            0x0D04, // GL_PACK_SKIP_PIXELS
            0x88ED, // GL_PIXEL_PACK_BUFFER_BINDING
        ]
        .into_iter()
        .zip(pack.iter_mut())
        {
            get(parameter, value);
        }
        let query_error = error();
        if query_error != 0 {
            return Err(format!(
                "cannot establish safe pixel-pack state: GL error 0x{query_error:x}"
            ));
        }
        if !matches!(pack[0], 1 | 2 | 4) || pack[1..].iter().any(|&value| value != 0) {
            return Err(format!(
                "unsupported pixel-pack layout (alignment,row_length,skip_rows,skip_pixels,buffer)={pack:?}"
            ));
        }
        bind(0x8CA8, framebuffer as u32); // GL_READ_FRAMEBUFFER
        read(
            region.x,
            height - region.y - region.height,
            region.width,
            region.height,
            0x1908, // GL_RGBA
            0x1401, // GL_UNSIGNED_BYTE
            pixels.as_mut_ptr().cast(),
        );
        let code = error();
        bind(0x8CA8, previous_read as u32);
        if code != 0 {
            return Err(format!("glReadPixels failed: 0x{code:x}"));
        }
    }
    Ok(FramePixels { region, rgba: pixels })
}

// A re-realized widget may have a different frame clock. Never read an attachment remembered
// from the old context: reconnect this observer and wait for the next real render emission.
struct PaintObserver {
    clock: gtk4::gdk::FrameClock,
    handler: Option<glib::SignalHandlerId>,
}

impl PaintObserver {
    fn new(area: &GLArea, capture: &Rc<Capture>, counter: &RenderCounter) -> Self {
        let clock = area.frame_clock().expect("realized editor frame clock");
        let capture = capture.clone();
        let area = area.clone();
        let count = counter.count.clone();
        let framebuffer = counter.framebuffer.clone();
        let emitted_at = counter.emitted_at.clone();
        let handler = clock.connect_after_paint(move |_| {
            let renders = count.get();
            if !capture.enabled.get() || renders == capture.last_render.get() {
                return;
            }
            capture.last_render.set(renders);
            match read_frame(&area, framebuffer.get(), capture.region.get()) {
                Ok(frame) => {
                    let (center, pixels) = frame.cursor();
                    let white = if capture.count_white.get() { frame.white() } else { 0 };
                    capture.samples.borrow_mut().push(Sample {
                        at: Instant::now(),
                        render_at: emitted_at.get(),
                        center,
                        pixels,
                        white,
                    });
                    if capture.keep_frame.get() {
                        *capture.frame.borrow_mut() = Some(frame);
                    }
                }
                Err(error) => *capture.error.borrow_mut() = Some(error),
            }
        });
        Self {
            clock,
            handler: Some(handler),
        }
    }
}

impl Drop for PaintObserver {
    fn drop(&mut self) {
        if let Some(handler) = self.handler.take() {
            self.clock.disconnect(handler);
        }
    }
}

fn spin(duration: Duration) {
    let until = Instant::now() + duration;
    let context = glib::MainContext::default();
    while Instant::now() < until {
        // One iteration at a time: an always-ready source must not starve our deadline.
        context.iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn rpc(socket: &Path, expression: &str) -> String {
    let output = Command::new("nvim")
        .args(["--server", socket.to_str().unwrap(), "--remote-expr", expression])
        .output()
        .expect("run nvim RPC oracle");
    assert!(
        output.status.success(),
        "nvim RPC failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn wait_cursor(socket: &Path, row: u32, col: u32) {
    let expected = format!("{row},{col}");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if rpc(socket, "printf('%d,%d', line('.'), col('.'))") == expected {
            return;
        }
        assert!(Instant::now() < deadline, "nvim input did not reach {expected}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn distance(a: Point, b: Point) -> f64 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

fn last_center(capture: &Capture) -> Point {
    capture
        .samples
        .borrow()
        .iter()
        .rev()
        .find_map(|sample| sample.center)
        .expect("no red cursor pixels")
}

fn region_for(area: &GLArea, from: Point, to: Point, cell: Point) -> Region {
    let scale = area.scale_factor();
    let x = (from.0.min(to.0) - cell.0 * 2.0).floor().max(0.0) as i32;
    let y = (from.1.min(to.1) - cell.1 * 2.0).floor().max(0.0) as i32;
    let right = (from.0.max(to.0) + cell.0 * 2.0)
        .ceil()
        .min(f64::from(area.width() * scale)) as i32;
    let bottom = (from.1.max(to.1) + cell.1 * 2.0)
        .ceil()
        .min(f64::from(area.height() * scale)) as i32;
    Region {
        x,
        y,
        width: right - x,
        height: bottom - y,
    }
}

#[allow(clippy::too_many_arguments)] // one call per motion case, every argument a different fact of it
fn idle_move(
    pane: &NeovideEditorPane,
    capture: &Capture,
    counter: &RenderCounter,
    socket: &Path,
    name: &str,
    keys: &str,
    destination: (u32, u32),
    delta: (i32, i32),
) -> Result<(), String> {
    spin(Duration::from_millis(400));
    let before_idle = counter.count.get();
    spin(Duration::from_millis(220));
    if counter.count.get() != before_idle {
        return Err(format!("{name}: editor rendered while idle with cursor blink disabled"));
    }
    let from = last_center(capture);
    let factor = f64::from(pane.widget().scale_factor());
    let cell = pane.cell_size().unwrap();
    let cell = (cell.0 * factor, cell.1 * factor);
    let to = (
        from.0 + f64::from(delta.0) * cell.0,
        from.1 + f64::from(delta.1) * cell.1,
    );
    capture.region.set(Some(region_for(pane.widget(), from, to, cell)));
    capture.samples.borrow_mut().clear();
    pane.send_keys(keys);
    // Do not iterate GTK until the real input has taken effect in nvim. See module comment.
    wait_cursor(socket, destination.0, destination.1);
    // nvim's grid notifications still cross the renderer's worker after the RPC reply. Leave
    // that worker time to enqueue its draw batch, without consuming an unchanged GTK frame.
    std::thread::sleep(Duration::from_millis(20));
    spin(Duration::from_millis(450));
    if let Some(error) = capture.error.borrow().as_ref() {
        return Err(error.clone());
    }
    let samples = capture.samples.borrow();
    let first = samples
        .iter()
        .filter_map(|s| s.center)
        .find(|&p| distance(p, from) > 0.5);
    let first = first.ok_or_else(|| format!("{name}: cursor never moved; samples={samples:?}"))?;
    let last = samples.iter().rev().find_map(|s| s.center).unwrap();
    let travel = distance(from, to);
    let progress = ((first.0 - from.0) * (to.0 - from.0) + (first.1 - from.1) * (to.1 - from.1)) / travel.powi(2);
    let intervals: Vec<_> = samples
        .windows(2)
        .map(|pair| pair[1].at.duration_since(pair[0].at).as_secs_f64() * 1000.0)
        .collect();
    println!(
        "{name}: frames={} from={from:?} first={first:?} expected={to:?} last={last:?} first_progress={progress:.4} pixels={} frame_gaps_ms={intervals:?}",
        samples.len(),
        samples.iter().map(|sample| sample.pixels).max().unwrap_or(0)
    );
    if distance(last, to) > 1.5 {
        return Err(format!(
            "{name}: final cursor {last:?} missed actual cell center {to:?}"
        ));
    }
    if !(0.0..0.98).contains(&progress) || distance(first, to) < 0.6 {
        return Err(format!(
            "{name}: first changed frame snapped to destination (progress={progress:.4})"
        ));
    }
    if samples
        .iter()
        .filter_map(|s| s.center)
        .filter(|&p| distance(p, from) > 0.5 && distance(p, to) > 0.6)
        .count()
        == 0
    {
        return Err(format!("{name}: no intermediate cursor pixels were presented"));
    }
    Ok(())
}

fn lua(socket: &Path, body: &str) -> String {
    let expression = format!("(function() {body} end)()").replace('\'', "''");
    rpc(socket, &format!("luaeval('{expression}')"))
}

fn pair(text: &str) -> (u32, u32) {
    let (a, b) = text.split_once(',').expect("two oracle coordinates");
    (a.parse().unwrap(), b.parse().unwrap())
}

struct Scene {
    cursor: (u32, u32),
    markers: [(u32, u32, [u8; 3]); 3],
}

fn install_scene(socket: &Path, variant: u32) -> Scene {
    let (rows, columns) = pair(&rpc(socket, "printf('%d,%d', winheight(0), winwidth(0))"));
    assert!(
        rows >= 12 && columns >= 40,
        "fixture needs at least 40x12 cells, got {columns}x{rows}"
    );
    let colors = [[0, 255, 0], [0, 0, 255], [255, 0, 255]];
    let scene = Scene {
        cursor: (rows / 2, 20 + variant * 2),
        // Deliberately asymmetric; the last patch follows the new bottom/right grid edges.
        markers: [
            (2, 3, colors[variant as usize % 3]),
            (4, 9, colors[(variant as usize + 1) % 3]),
            (rows - 1, columns - 7, colors[(variant as usize + 2) % 3]),
        ],
    };
    let mut body = format!(
        "local ns=vim.api.nvim_create_namespace('copy-regression'); \
         vim.api.nvim_buf_clear_namespace(0,ns,0,-1); \
         vim.api.nvim_buf_set_lines(0,0,-1,false,vim.fn['repeat']({{string.rep(' ',{columns})}},{rows})); \
         vim.api.nvim_buf_set_text(0,6,2,6,4,{{'Fj'}}); \
         vim.api.nvim_set_hl(0,'CopyGlyph',{{fg='#ffffff',bg='#000000'}}); \
         vim.api.nvim_buf_add_highlight(0,ns,'CopyGlyph',6,2,4); "
    );
    for (index, (row, column, rgb)) in scene.markers.iter().enumerate() {
        body.push_str(&format!(
            "vim.api.nvim_set_hl(0,'CopyPatch{index}',{{fg='#000000',bg='#{:02x}{:02x}{:02x}'}}); \
             vim.api.nvim_buf_add_highlight(0,ns,'CopyPatch{index}',{},{},{}); ",
            rgb[0],
            rgb[1],
            rgb[2],
            row - 1,
            column - 1,
            column + 4
        ));
    }
    body.push_str(&format!(
        "vim.api.nvim_win_set_cursor(0,{{{},{}}}); \
         vim.fn.winrestview({{topline=1,leftcol=0}}); vim.cmd('redraw!'); return 'ok'",
        scene.cursor.0,
        scene.cursor.1 - 1
    ));
    assert_eq!(lua(socket, &body), "ok");
    wait_cursor(socket, scene.cursor.0, scene.cursor.1);
    scene
}

fn cell_region(pane: &NeovideEditorPane, socket: &Path, row: u32, column: u32, cells: u32) -> Region {
    let (screen_row, screen_col) = pair(&rpc(
        socket,
        &format!(
            "printf('%d,%d', screenpos(win_getid(),{row},{column}).row, screenpos(win_getid(),{row},{column}).col)"
        ),
    ));
    assert!(
        screen_row > 0 && screen_col > 0,
        "oracle cell {row},{column} is not visible: {}",
        rpc(
            socket,
            "string({'lines': &lines, 'columns': &columns, 'height': winheight(0), 'width': winwidth(0), 'view': winsaveview()})"
        )
    );
    let scale = f64::from(pane.widget().scale_factor());
    let (width, height) = pane.cell_size().unwrap();
    let (width, height) = (width * scale, height * scale);
    // The product's existing layout anchors whole grid rows to the pane's bottom edge;
    // vertical spare pixels are TOP padding (the horizontal remainder stays on the right).
    // Derive the origin from nvim's actual row count, not a copy of snap_region_to_grid.
    let grid_rows: u32 = rpc(socket, "&lines").parse().unwrap();
    let top = f64::from(pane.widget().height()) * scale - f64::from(grid_rows) * height;
    let x = (f64::from(screen_col - 1) * width).floor() as i32;
    let y = (top + f64::from(screen_row - 1) * height).floor() as i32;
    Region {
        x,
        y,
        width: (f64::from(screen_col - 1 + cells) * width).ceil() as i32 - x,
        height: (top + f64::from(screen_row) * height).ceil() as i32 - y,
    }
}

fn contains(region: Region, x: i32, y: i32, padding: i32) -> bool {
    x >= region.x - padding
        && x < region.x + region.width + padding
        && y >= region.y - padding
        && y < region.y + region.height + padding
}

// These assertions catch flipped/wrong-size copies, lost glyph textures, and retained pixels
// from the preceding frame. Expectations come from actual nvim cells and literal RGB colors,
// never from the product's offscreen texture or copy implementation.
fn check_scene(
    pane: &NeovideEditorPane,
    socket: &Path,
    capture: &Capture,
    counter: &RenderCounter,
    name: &str,
    scene: &Scene,
) -> (i32, i32, Vec<u8>) {
    // Resize can animate both the window/scroll and the cursor. Wait for the product to stop
    // requesting frames rather than assuming that a fixed delay covered every animation.
    pane.widget().queue_render();
    let settle_deadline = Instant::now() + Duration::from_secs(5);
    let (mut last_count, mut last_change) = (counter.count.get(), Instant::now());
    while last_change.elapsed() < Duration::from_millis(120) {
        assert!(Instant::now() < settle_deadline, "{name}: editor did not settle");
        spin(Duration::from_millis(10));
        if counter.count.get() != last_count {
            last_count = counter.count.get();
            last_change = Instant::now();
        }
    }
    let before = counter.count.get();
    pane.widget().queue_render();
    let deadline = Instant::now() + Duration::from_secs(3);
    while capture.last_render.get() <= before {
        assert!(
            Instant::now() < deadline,
            "{name}: no completed frame after queue_render"
        );
        spin(Duration::from_millis(10));
    }
    assert!(capture.error.borrow().is_none(), "{name}: {:?}", capture.error.borrow());
    let frame = capture.frame.borrow();
    let frame = frame.as_ref().expect("full scene framebuffer captured");
    let area = pane.widget();
    assert_eq!(
        (frame.region.width, frame.region.height),
        (area.width() * area.scale_factor(), area.height() * area.scale_factor()),
        "{name}: capture size"
    );
    let cursor = cell_region(pane, socket, scene.cursor.0, scene.cursor.1, 1);
    let center = (
        f64::from(cursor.x) + f64::from(cursor.width) / 2.0,
        f64::from(cursor.y) + f64::from(cursor.height) / 2.0,
    );
    let (actual, red_pixels) = frame.cursor();
    assert!(
        actual.is_some_and(|point| distance(point, center) <= 1.5),
        "{name}: cursor {actual:?}, expected {center:?}; last centers={:?}",
        capture
            .samples
            .borrow()
            .iter()
            .rev()
            .take(8)
            .map(|sample| sample.center)
            .collect::<Vec<_>>()
    );
    assert!(
        red_pixels > (cursor.width * cursor.height) as usize / 2,
        "{name}: incomplete block cursor"
    );
    let mut allowed = vec![cursor];
    for &(row, column, rgb) in &scene.markers {
        let patch = cell_region(pane, socket, row, column, 5);
        for y in patch.y + patch.height / 3..patch.y + patch.height * 2 / 3 {
            for x in patch.x + patch.width / 4..patch.x + patch.width * 3 / 4 {
                let pixel = frame.pixel(x, y);
                assert!(
                    pixel[..3].iter().zip(rgb).all(|(&got, want)| got.abs_diff(want) <= 8) && pixel[3] >= 250,
                    "{name}: patch at {row},{column} pixel {x},{y}={pixel:?}, expected {rgb:?}"
                );
            }
        }
        allowed.push(patch);
    }
    let glyph = cell_region(pane, socket, 7, 3, 2);
    let mut glyph_pixels = Vec::new();
    let mut ink = 0;
    let (mut f_ink, mut f_bounds) = (0, (i32::MAX, i32::MAX, i32::MIN, i32::MIN));
    for y in glyph.y..glyph.y + glyph.height {
        for x in glyph.x..glyph.x + glyph.width {
            let pixel = frame.pixel(x, y);
            let white = pixel[..3].iter().all(|&channel| channel > 80);
            ink += usize::from(white);
            if white && x < glyph.x + glyph.width / 2 {
                f_ink += 1;
                f_bounds = (
                    f_bounds.0.min(x),
                    f_bounds.1.min(y),
                    f_bounds.2.max(x),
                    f_bounds.3.max(y),
                );
            }
            glyph_pixels.extend_from_slice(pixel);
        }
    }
    let area_pixels = (glyph.width * glyph.height) as usize;
    assert!(
        ink > area_pixels / 20 && ink < area_pixels * 3 / 4,
        "{name}: Fj glyph is missing or a solid texture block ({ink}/{area_pixels})"
    );
    assert!(f_ink > 0, "{name}: F glyph is missing");
    let f_area = (f_bounds.2 - f_bounds.0 + 1) * (f_bounds.3 - f_bounds.1 + 1);
    assert!(
        f_ink * 5 < f_area * 4,
        "{name}: F's ink fills its bounding box ({f_ink}/{f_area}); expected letter strokes, not a glyph-atlas rectangle"
    );
    allowed.push(glyph);
    for y in 0..frame.region.height {
        for x in 0..frame.region.width {
            let pixel = frame.pixel(x, y);
            assert!(
                !is_cursor(pixel) || contains(cursor, x, y, 2),
                "{name}: red cursor trail at {x},{y}"
            );
            assert!(
                allowed.iter().any(|&region| contains(region, x, y, 2))
                    || (pixel[..3].iter().all(|&channel| channel <= 8) && pixel[3] >= 250),
                "{name}: stale/unexpected content at {x},{y}: {pixel:?}"
            );
        }
    }
    println!(
        "{name}: framebuffer={}x{} cursor={actual:?} red_pixels={red_pixels} glyph_ink={ink}",
        frame.region.width, frame.region.height
    );
    (glyph.width, glyph.height, glyph_pixels)
}

fn wait_grid(pane: &NeovideEditorPane, socket: &Path, width: i32, height: i32, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while (pane.widget().width(), pane.widget().height()) != (width, height) {
        assert!(Instant::now() < deadline, "{name}: did not allocate {width}x{height}");
        spin(Duration::from_millis(10));
    }
    // Realization precedes allocation, and nvim learns the new grid asynchronously. At the
    // fixture's zero margins the grid must fit whole cells into the actual widget allocation.
    let cell = pane.cell_size().unwrap();
    let expected = (
        (f64::from(height) / cell.1).floor() as u32,
        (f64::from(width) / cell.0).floor() as u32,
    );
    loop {
        let actual = pair(&rpc(socket, "printf('%d,%d', &lines, &columns)"));
        if actual == expected {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{name}: nvim grid {actual:?} did not reach {expected:?}"
        );
        spin(Duration::from_millis(10));
    }
}

fn presentation_lifetime(
    pane: &NeovideEditorPane,
    fixed: &gtk4::Fixed,
    socket: &Path,
    capture: &Rc<Capture>,
    counter: &RenderCounter,
    observer: &mut Option<PaintObserver>,
) {
    capture.region.set(None);
    capture.keep_frame.set(true);
    capture.enabled.set(true);
    let original = install_scene(socket, 0);
    let glyph = check_scene(pane, socket, capture, counter, "copy colors and glyph", &original);
    let mut last_scene = original;
    // A fixed child has a controlled allocation even under a tiling compositor that ignores
    // Window::set_default_size. Both dimensions change, including newly exposed bottom/right.
    for (variant, width, height) in [(1, 680, 420), (2, 940, 640), (3, 760, 500)] {
        pane.widget().set_size_request(width, height);
        let name = format!("resize {width}x{height}");
        wait_grid(pane, socket, width, height, &name);
        let scene = install_scene(socket, variant);
        assert_eq!(
            check_scene(pane, socket, capture, counter, &name, &scene),
            glyph,
            "{name}: glyph pixels changed"
        );
        last_scene = scene;
    }

    let pid = rpc(socket, "getpid()");
    let view_expression = "string([getcurpos(), winsaveview().topline, winsaveview().leftcol])";
    let view = rpc(socket, view_expression);
    let size = (pane.widget().width(), pane.widget().height());
    let unrealized = Rc::new(Cell::new(0));
    let realized = Rc::new(Cell::new(0));
    let disconnect_unrealize = {
        let count = unrealized.clone();
        pane.widget().connect_unrealize(move |_| count.set(count.get() + 1))
    };
    let disconnect_realize = {
        let count = realized.clone();
        pane.widget().connect_realize(move |_| count.set(count.get() + 1))
    };
    capture.enabled.set(false);
    drop(observer.take());
    fixed.remove(pane.widget());
    assert!(
        !pane.widget().is_realized() && unrealized.get() == 1,
        "removing the live GLArea must actually unrealize it"
    );
    assert_eq!(rpc(socket, "getpid()"), pid, "unrealize stopped/replaced nvim");
    capture.frame.borrow_mut().take();
    capture.last_render.set(counter.count.get());
    counter.framebuffer.set(0);
    fixed.put(pane.widget(), 0.0, 0.0);
    assert!(
        pane.widget().is_realized() && realized.get() == 1,
        "reparenting must realize the same GLArea again"
    );
    pane.grab_focus();
    pane.set_focused(true);
    *observer = Some(PaintObserver::new(pane.widget(), capture, counter));
    capture.enabled.set(true);
    wait_grid(pane, socket, size.0, size.1, "re-realize");
    // First exercise the retained renderer/cache without rewriting nvim's lines. Replacing the
    // fixture first could conceal stale glyph resources carried over from the previous context.
    assert_eq!(
        check_scene(
            pane,
            socket,
            capture,
            counter,
            "re-realize retained content",
            &last_scene
        ),
        glyph,
        "re-realize lost the previously rendered glyphs"
    );
    assert_eq!(
        rpc(socket, view_expression),
        view,
        "same-size re-realize changed nvim's cursor/view before the new fixture; a transient 0x0 allocation must not resize nvim to 1x1"
    );
    let scene = install_scene(socket, 4);
    assert_eq!(
        check_scene(pane, socket, capture, counter, "after actual re-realize", &scene),
        glyph,
        "re-realize lost or changed glyph textures"
    );
    assert_eq!(rpc(socket, "getpid()"), pid, "re-realize replaced nvim");
    pane.widget().disconnect(disconnect_unrealize);
    pane.widget().disconnect(disconnect_realize);
    capture.keep_frame.set(false);
    capture.enabled.set(false);
}

// ---- Typing-latency cases (2026-09-29, plan `2026-09-29-typing-latency-fix.md`, Task 4) --------------
//
// Redraws are event driven: nvim's batch wakes the pane through an fd watch, the tick runs only while a
// frame is wanted, and a key press no longer forces a stale frame. Each case prints its numbers.

/// Runs the default main context, blocking between events (no sleep granularity of its own, so a
/// wake-up is measured at its own latency), until `done()` holds or `timeout` passes.
fn run_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let context = glib::MainContext::default();
    let expired = Rc::new(Cell::new(false));
    let flag = expired.clone();
    let source = glib::timeout_add_local_once(timeout, move || flag.set(true));
    let met = loop {
        if done() {
            break true;
        }
        if expired.get() {
            break false;
        }
        context.iteration(true);
    };
    if !expired.get() {
        source.remove();
    }
    met
}

fn wait_for(duration: Duration) {
    run_until(duration, || false);
}

/// Until the pane has drawn nothing for `quiet` (animation over, tick stopped).
fn settle(counter: &RenderCounter, quiet: Duration) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let (mut last, mut since) = (counter.count.get(), Instant::now());
    while since.elapsed() < quiet {
        assert!(Instant::now() < deadline, "editor did not settle");
        wait_for(Duration::from_millis(10));
        if counter.count.get() != last {
            last = counter.count.get();
            since = Instant::now();
        }
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[(((sorted.len() - 1) as f64) * p).round() as usize]
}

fn summary(values: &[f64]) -> (f64, f64, f64, f64) {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (
        percentile(&sorted, 0.5),
        percentile(&sorted, 0.9),
        percentile(&sorted, 0.99),
        sorted.last().copied().unwrap_or(f64::NAN),
    )
}

/// White typed text on black, in insert mode on `row`, so a typed key is visible as near-white pixels.
fn typing_setup(socket: &Path, row: u32) {
    lua(
        socket,
        &format!(
            "vim.cmd('highlight Normal guifg=#ffffff guibg=#000000'); \
             vim.api.nvim_win_set_cursor(0,{{{row},0}}); vim.cmd('startinsert'); return 'ok'"
        ),
    );
    assert_eq!(rpc(socket, "mode()"), "i");
}

fn typing_teardown(socket: &Path, counter: &RenderCounter) {
    lua(
        socket,
        "vim.cmd('stopinsert'); vim.cmd('highlight Normal guifg=#000000 guibg=#000000'); return 'ok'",
    );
    settle(counter, Duration::from_millis(200));
}

struct Typing {
    /// Key press to the start of the render whose frame shows it, milliseconds.
    render_ms: Vec<f64>,
    /// Key press to the end of that frame's paint.
    paint_ms: Vec<f64>,
    /// Renders between a key and the one showing it that showed nothing new (settled runs only).
    empty_renders: usize,
    misses: usize,
    /// When each press was sent.
    pressed: Vec<Instant>,
}

/// `presses` alternating `x` and `<BS>` in insert mode, each changing the white pixels of a band across
/// `row`; `interval(i)` is the time from press `i` to press `i + 1` (None: wait for the pane to settle).
fn typing_probe(
    pane: &NeovideEditorPane,
    capture: &Capture,
    counter: &RenderCounter,
    socket: &Path,
    row: u32,
    presses: usize,
    mut interval: impl FnMut(usize) -> Option<Duration>,
) -> Typing {
    settle(counter, Duration::from_millis(200));
    capture.region.set(Some(cell_region(pane, socket, row, 1, 70)));
    capture.count_white.set(true);
    capture.enabled.set(true);
    capture.samples.borrow_mut().clear();
    pane.widget().queue_render();
    assert!(
        run_until(Duration::from_secs(2), || !capture.samples.borrow().is_empty()),
        "no frame to take the typing baseline from"
    );
    settle(counter, Duration::from_millis(100));
    let mut baseline = capture.samples.borrow().last().unwrap().white;
    let mut out = Typing {
        render_ms: Vec::new(),
        paint_ms: Vec::new(),
        empty_renders: 0,
        misses: 0,
        pressed: Vec::new(),
    };
    for i in 0..presses {
        let key = if i % 2 == 0 { "x" } else { "<BS>" };
        let first = capture.samples.borrow().len();
        let pressed = Instant::now();
        out.pressed.push(pressed);
        pane.send_keys(key);
        let found = |samples: &[Sample]| samples[first..].iter().position(|s| s.white.abs_diff(baseline) >= 5);
        let hit = run_until(Duration::from_millis(500), || {
            found(&capture.samples.borrow()).is_some()
        });
        if hit {
            let samples = capture.samples.borrow();
            let index = found(&samples).unwrap();
            let sample = samples[first + index];
            out.render_ms
                .push(sample.render_at.saturating_duration_since(pressed).as_secs_f64() * 1000.0);
            out.paint_ms
                .push(sample.at.saturating_duration_since(pressed).as_secs_f64() * 1000.0);
            out.empty_renders += index;
            baseline = sample.white;
        } else {
            out.misses += 1;
        }
        match interval(i) {
            // Press to press, not hit to press: the caller's cadence is measured from the key.
            Some(period) => wait_for(period.saturating_sub(pressed.elapsed())),
            None => settle(counter, Duration::from_millis(150)),
        }
    }
    capture.enabled.set(false);
    capture.count_white.set(false);
    out
}

fn report(name: &str, run: &Typing) {
    let (p50, p90, p99, max) = summary(&run.render_ms);
    let (q50, _, q99, _) = summary(&run.paint_ms);
    println!(
        "{name}: presses={} misses={} key->render start ms p50={p50:.2} p90={p90:.2} p99={p99:.2} max={max:.2}; \
         key->paint end ms p50={q50:.2} p99={q99:.2}; renders showing nothing new between key and batch: {}",
        run.render_ms.len() + run.misses,
        run.misses,
        run.empty_renders
    );
}

/// Case 2 and F3's empty tick cycle (case 6 after a re-realize, `label`): settled insert-mode typing.
fn settled_typing_cases(
    pane: &NeovideEditorPane,
    capture: &Capture,
    counter: &RenderCounter,
    socket: &Path,
    label: &str,
    failures: &mut Vec<String>,
) {
    typing_setup(socket, 12);
    let clock = pane.widget().frame_clock().expect("realized editor frame clock");
    settle(counter, Duration::from_millis(300));
    let (frames, renders) = (clock.frame_counter(), counter.count.get() as i64);
    let run = typing_probe(pane, capture, counter, socket, 12, 100, |_| None);
    settle(counter, Duration::from_millis(300));
    let (frames, renders) = (clock.frame_counter() - frames, counter.count.get() as i64 - renders);
    typing_teardown(socket, counter);
    report(label, &run);
    // F3: a frame clock frame that ran no render is an empty tick cycle. The last frame of each key's
    // animation stops the tick itself, so there is none (the old code kept one armed after every key).
    println!(
        "{label}: over 100 keys the frame clock ran {frames} frames for {renders} renders \
         ({} without a render)",
        frames - renders
    );
    if frames - renders > 2 {
        failures.push(format!(
            "{label}: {} frame clock frame(s) ran no render over 100 keys (an empty tick cycle after \
             the last animating frame; want <= 2)",
            frames - renders
        ));
    }
    let (p50, ..) = summary(&run.render_ms);
    if run.misses > 0 || run.render_ms.len() < 100 {
        failures.push(format!("{label}: {} of 100 presses never reached a frame", run.misses));
    }
    if p50.is_nan() || p50 > 3.0 {
        failures.push(format!("{label}: key -> render p50 {p50:.2} ms exceeds 3 ms"));
    }
}

fn typing_latency_cases(
    pane: &NeovideEditorPane,
    fixed: &gtk4::Fixed,
    capture: &Capture,
    counter: &RenderCounter,
    socket: &Path,
    failures: &mut Vec<String>,
) {
    capture.enabled.set(false);
    settle(counter, Duration::from_millis(300));

    // Case 1: a settled pane draws nothing. The frame clock's own counter is the pane's clock: with the
    // tick registered only while a frame is wanted, an idle window has no frame to count (pristine ~120).
    let clock = pane.widget().frame_clock().expect("realized editor frame clock");
    let (frames, renders) = (clock.frame_counter(), counter.count.get());
    wait_for(Duration::from_secs(2));
    let advanced = clock.frame_counter() - frames;
    println!(
        "case 1 settled idle 2 s: frame clock advanced {advanced}, renders {}",
        counter.count.get() - renders
    );
    if advanced > 2 {
        failures.push(format!(
            "case 1: settled idle advanced the frame clock {advanced} times (want <= 2)"
        ));
    }

    // Case 2 (and the empty-tick-cycle half of F3, case 5).
    settled_typing_cases(pane, capture, counter, socket, "case 2 settled typing", failures);

    // Case 3: no render between a key and its batch. Insert-mode mappings hand `x` and `<BS>` to nvim after
    // an 8 ms delay, so the batch reliably arrives well after the key. A pane that draws on the next tick
    // after a key press (the old behaviour) draws a stale frame in that gap about half the time (8 of every
    // 16.7 ms); an event-driven one draws nothing until the batch is there.
    typing_setup(socket, 12);
    lua(
        socket,
        "vim.keymap.set('i','x',function() vim.defer_fn(function() vim.api.nvim_feedkeys('x','nt',false) end,8) end); \
         vim.keymap.set('i','<BS>',function() vim.defer_fn(function() \
         vim.api.nvim_feedkeys(vim.keycode('<BS>'),'nt',false) end,8) end); return 'ok'",
    );
    let delayed = typing_probe(pane, capture, counter, socket, 12, 100, |_| None);
    lua(
        socket,
        "vim.keymap.del('i','x'); vim.keymap.del('i','<BS>'); return 'ok'",
    );
    typing_teardown(socket, counter);
    report("case 3 typing, nvim replies 8 ms after the key", &delayed);
    if delayed.misses > 0 || delayed.empty_renders != 0 {
        failures.push(format!(
            "case 3: {} render(s) between a key and its batch showed nothing new (want 0 of 100), {} miss(es)",
            delayed.empty_renders, delayed.misses
        ));
    }

    // Case 5 (F3): keys 100-140 ms apart, spanning the end of each cursor animation, no settling in between.
    // The empty-tick-cycle half of F3 is counted in the settled runs above and below (frame clock frames
    // against renders). Here the timing half is reported: a key landing while an animation still runs waits
    // for that animation's next frame by design (one refresh at most, so it is not judged); one landing after
    // its last frame must not wait for a cycle the old code kept armed. Wayland paces frames by the compositor's
    // frame callbacks, so a key within one refresh of the previous frame waits for the callback whatever the
    // tick does, which is why no gate on a latency is put on keys this close together.
    typing_setup(socket, 12);
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let burst = typing_probe(pane, capture, counter, socket, 12, 80, |_| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        Some(Duration::from_millis(100 + (seed >> 33) % 41))
    });
    typing_teardown(socket, counter);
    report("case 5 keys 100-140 ms apart", &burst);
    if burst.misses > 0 {
        failures.push(format!("case 5: {} key(s) never reached a frame", burst.misses));
    }

    // Case 4 (F1): sustained nvim output must not starve paints.
    let ticks = Rc::new(Cell::new(0u64));
    let tick_id = {
        let ticks = ticks.clone();
        fixed.add_tick_callback(move |_, _| {
            ticks.set(ticks.get() + 1);
            glib::ControlFlow::Continue
        })
    };
    settle(counter, Duration::from_millis(200));
    let refresh_us = clock.refresh_info(clock.frame_time()).0.max(1);
    let refresh_hz = 1_000_000.0 / refresh_us as f64;
    lua(
        socket,
        "local n=0; vim.fn.timer_start(1, function() n=n+1; \
         vim.api.nvim_buf_set_lines(0,0,1,false,{tostring(n)}) end, {['repeat']=-1}); return 'ok'",
    );
    wait_for(Duration::from_millis(300));
    let (renders, widget_ticks, at) = (counter.count.get(), ticks.get(), Instant::now());
    wait_for(Duration::from_secs(2));
    let seconds = at.elapsed().as_secs_f64();
    let (rendered, ticked) = (counter.count.get() - renders, ticks.get() - widget_ticks);
    rpc(socket, "timer_stopall()");
    tick_id.remove();
    println!(
        "case 4 flood, nvim timer every 1 ms for {seconds:.2} s: refresh {refresh_hz:.1} Hz, renders {rendered} \
         ({:.1}/s, {:.0} % of refresh), second widget's ticks {ticked} ({:.1}/s)",
        rendered as f64 / seconds,
        100.0 * rendered as f64 / seconds / refresh_hz,
        ticked as f64 / seconds
    );
    if (rendered as f64 / seconds) < 0.8 * refresh_hz {
        failures.push(format!(
            "case 4: {rendered} renders in {seconds:.2} s under a redraw flood, below 80 % of {refresh_hz:.1} Hz"
        ));
    }
    if (ticked as f64 / seconds) < 0.8 * refresh_hz {
        failures.push(format!(
            "case 4: a second widget's tick ran {ticked} times in {seconds:.2} s under the flood (starved)"
        ));
    }
    lua(
        socket,
        "vim.api.nvim_buf_set_lines(0,0,1,false,{string.rep(' ',80)}); return 'ok'",
    );
    settle(counter, Duration::from_millis(300));

    // Case 7 (F2): a fullscreen change consumed by a paint that is not animating still reaches the host.
    let fullscreen = Rc::new(Cell::new(None::<(Instant, bool)>));
    {
        let fullscreen = fullscreen.clone();
        pane.on_fullscreen_setting(move |value| fullscreen.set(Some((Instant::now(), value))));
    }
    for target in [true, false] {
        fullscreen.set(None);
        settle(counter, Duration::from_millis(200));
        rpc(
            socket,
            &format!(
                "execute('let g:neovide_fullscreen={}')",
                if target { "v:true" } else { "v:false" }
            ),
        );
        // Nothing has iterated the main loop since: the notification is queued on the harness's
        // channel, and the paint below reads it first (measured, not assumed: with the post-render
        // service kick removed this case fails, so the idle-priority fd watch does not win the race).
        std::thread::sleep(Duration::from_millis(30));
        let asked = Instant::now();
        pane.widget().queue_render();
        let reached = run_until(Duration::from_millis(200), || fullscreen.get().is_some());
        match fullscreen.get() {
            Some((at, value)) if reached && value == target => println!(
                "case 7 fullscreen={target}: reached the host {:.1} ms after the paint was asked for",
                at.duration_since(asked).as_secs_f64() * 1000.0
            ),
            other => failures.push(format!(
                "case 7: `let g:neovide_fullscreen={target}` did not reach the host within 200 ms ({other:?})"
            )),
        }
    }
}

/// Case 8 (F2, last: it ends the session): `:qa!` consumed by a paint that is not animating still fires
/// `on_exited_unrequested`.
/// Measured: it stays green with only the F2 kick removed (`exit_pending` in `tick_still_wanted` also keeps the
/// tick for an unhandled exit) and goes red with both removed, so it guards the pair, not F2's exit branch alone.
fn exit_case(pane: &NeovideEditorPane, counter: &RenderCounter, socket: &Path, failures: &mut Vec<String>) {
    let exited = Rc::new(Cell::new(None::<Instant>));
    {
        let exited = exited.clone();
        pane.on_exited_unrequested(move || exited.set(Some(Instant::now())));
    }
    settle(counter, Duration::from_millis(300));
    rpc(socket, "timer_start(0, {-> execute('qa!')})");
    std::thread::sleep(Duration::from_millis(60));
    let asked = Instant::now();
    pane.widget().queue_render();
    let reached = run_until(Duration::from_millis(200), || exited.get().is_some());
    match exited.get() {
        Some(at) if reached => println!(
            "case 8 exit: on_exited fired {:.1} ms after the paint was asked for",
            at.duration_since(asked).as_secs_f64() * 1000.0
        ),
        _ => failures.push("case 8: `:qa!` did not reach on_exited_unrequested within 200 ms".into()),
    }
}

fn presentation(pane: &NeovideEditorPane) -> &'static str {
    if pane.draws_into_own_buffers() {
        "own dmabuf buffers"
    } else {
        "GtkGLArea texture"
    }
}

#[allow(clippy::too_many_arguments)] // the run threads the one fixture through every case
fn run(
    pane: &NeovideEditorPane,
    fixed: &gtk4::Fixed,
    socket: &Path,
    capture: &Rc<Capture>,
    counter: &RenderCounter,
    observer: &mut Option<PaintObserver>,
    expect_own: Option<bool>,
) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while pane.cell_size().is_none() || !socket.exists() || capture.samples.borrow().iter().all(|s| s.center.is_none())
    {
        assert!(
            Instant::now() < deadline,
            "editor never showed a red cursor: {:?}",
            capture.error.borrow()
        );
        spin(Duration::from_millis(20));
    }
    spin(Duration::from_millis(500));
    wait_cursor(socket, 10, 11);
    let mut failures = Vec::new();
    println!("presentation: {}", presentation(pane));
    for (name, keys, target, delta) in [
        ("horizontal after idle", "l", (10, 12), (1, 0)),
        ("vertical after idle", "j", (11, 12), (0, 1)),
        ("long jump after idle", "15l", (11, 27), (15, 0)),
    ] {
        if let Err(error) = idle_move(pane, capture, counter, socket, name, keys, target, delta) {
            failures.push(error);
        }
    }

    // Real asynchronous input between GTK frames, without the RPC barrier used above. Every
    // loop returns to the same cell, and the final rendered pixels must agree with nvim's oracle.
    capture.region.set(None);
    let origin = last_center(capture);
    capture.samples.borrow_mut().clear();
    for _ in 0..8 {
        for key in ["h", "j", "l", "k"] {
            pane.send_keys(key);
            spin(Duration::from_millis(35));
        }
    }
    spin(Duration::from_millis(450));
    wait_cursor(socket, 11, 27);
    let end = last_center(capture);
    if distance(origin, end) > 1.5 {
        failures.push(format!("continuous hjkl ended at {end:?}, expected {origin:?}"));
    }
    let samples = capture.samples.borrow();
    if !samples
        .iter()
        .filter_map(|s| s.center)
        .any(|point| distance(origin, point) > 2.0)
    {
        failures.push("continuous hjkl never moved the rendered cursor".into());
    }
    println!("continuous hjkl: captured_frames={} final={end:?}", samples.len());
    drop(samples);
    capture.enabled.set(false);
    spin(Duration::from_millis(250));
    let before = counter.count.get();
    spin(Duration::from_millis(300));
    let extra = counter.count.get() - before;
    if extra != 0 {
        failures.push(format!(
            "idle regression: {extra} actual render emissions after settling"
        ));
    }
    println!("settled idle: renders={extra}");
    typing_latency_cases(pane, fixed, capture, counter, socket, &mut failures);
    presentation_lifetime(pane, fixed, socket, capture, counter, observer);
    println!("presentation after re-realize: {}", presentation(pane));
    // Case 6: the re-realized pane keeps its session and its fd watch (added once per harness); case 2/3 hold again.
    settled_typing_cases(
        pane,
        capture,
        counter,
        socket,
        "case 6 typing after re-realize",
        &mut failures,
    );
    let counts = pane.presentation_counts();
    println!(
        "presentation counts: own frames={} GtkGLArea frames={} own-buffer failures={}",
        counts.own_frames, counts.fallback_frames, counts.failures
    );
    match expect_own {
        Some(true) if counts.fallback_frames > 0 || counts.failures > 0 || counts.own_frames == 0 => {
            failures.push(format!(
                "expected every frame in own dmabuf buffers: {} drawn there, {} through GtkGLArea's \
                 texture, {} failure(s) of the own-buffer path",
                counts.own_frames, counts.fallback_frames, counts.failures
            ));
        }
        Some(false) if counts.own_frames > 0 || counts.fallback_frames == 0 => {
            failures.push(format!(
                "expected every frame through GtkGLArea's texture: {} drawn into own dmabuf buffers, {} \
                 through GtkGLArea's",
                counts.own_frames, counts.fallback_frames
            ));
        }
        _ => {}
    }
    exit_case(pane, counter, socket, &mut failures);
    failures
}

/// The Wayland socket this test may draw on: the headless-sway sandbox's, never the desktop's.
///
/// "Run ONLY in the isolated GUI sandbox" was a sentence in this file's header until 2026-09-29,
/// when a sibling test that said the same (`shell/tests/panel_stream_scroll.rs`) was run without its
/// wrapper and opened its windows on the owner's own screen. Every sandbox harness in use starts its
/// compositor with a private `XDG_RUNTIME_DIR` of its own (`<harness>/run`), and a desktop session's
/// sockets live in the login runtime directory, `/run/user/<uid>/`. So the socket `WAYLAND_DISPLAY`
/// names -- an absolute path, or relative to `XDG_RUNTIME_DIR`, as libwayland resolves it -- must
/// exist and must resolve (symlinks followed) outside `/run/user/`. `DISPLAY` is not accepted at all:
/// XWayland's `:0` cannot be told apart from an X server of the test's own by its name.
fn sandbox_wayland_socket() -> Result<PathBuf, String> {
    let name = std::env::var_os("WAYLAND_DISPLAY")
        .filter(|name| !name.is_empty())
        .ok_or(
            "WAYLAND_DISPLAY is not set -- run it inside the headless-sway sandbox, with that sandbox's own \
         WAYLAND_DISPLAY and XDG_RUNTIME_DIR",
        )?;
    let socket = if Path::new(&name).is_absolute() {
        PathBuf::from(&name)
    } else {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|dir| !dir.is_empty())
            .ok_or("WAYLAND_DISPLAY is relative and XDG_RUNTIME_DIR is not set")?;
        PathBuf::from(runtime).join(&name)
    };
    let resolved = socket
        .canonicalize()
        .map_err(|e| format!("the Wayland socket {} cannot be resolved ({e})", socket.display()))?;
    let is_socket = std::fs::metadata(&resolved).is_ok_and(|meta| meta.file_type().is_socket());
    if !is_socket {
        return Err(format!(
            "WAYLAND_DISPLAY resolves to {}, which is not a socket",
            resolved.display()
        ));
    }
    if resolved.starts_with("/run/user") {
        return Err(format!(
            "WAYLAND_DISPLAY resolves to {}, in the login session's runtime directory -- a desktop, not the sandbox",
            resolved.display()
        ));
    }
    // `canonicalize` follows symlinks, not bind mounts or hard links: compare the socket itself.
    let meta = std::fs::metadata(&resolved).map_err(|e| format!("{}: {e}", resolved.display()))?;
    if let Some((_, _, login)) = login_runtime_sockets()
        .into_iter()
        .find(|(dev, ino, _)| (*dev, *ino) == (meta.dev(), meta.ino()))
    {
        return Err(format!(
            "WAYLAND_DISPLAY resolves to {}, which is the login session's own socket {} (a bind mount or hard link) \
             -- a desktop, not the sandbox",
            resolved.display(),
            login.display()
        ));
    }
    Ok(resolved)
}

/// Every socket in this user's login runtime directory, `/run/user/<uid>/`, and one level below it,
/// as `(st_dev, st_ino, path)`: what a desktop session's compositor listens on.
fn login_runtime_sockets() -> Vec<(u64, u64, PathBuf)> {
    // SAFETY: `getuid` has no preconditions and cannot fail.
    let root = PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }));
    let mut found = Vec::new();
    let mut dirs = vec![(root, 0)];
    while let Some((dir, depth)) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(meta) = entry.path().symlink_metadata() else {
                continue;
            };
            if meta.file_type().is_socket() {
                found.push((meta.dev(), meta.ino(), entry.path()));
            } else if meta.is_dir() && depth < 1 {
                dirs.push((entry.path(), depth + 1));
            }
        }
    }
    found
}

/// The compositor listening on the socket file at `resolved` was started as a headless wlroots
/// compositor (`WLR_BACKENDS=headless`, what every sandbox harness here sets), found **without
/// connecting to it** (fix round 2, 2026-09-29): the listener bound to that very file, the process
/// holding it by its `/proc/<pid>/fd` link, and that process's own initial environment and mapped
/// libraries. A desktop compositor -- GNOME's, KDE's, a sway on real outputs, wherever its runtime
/// directory is -- has no such variable, and a nested one (`WLR_BACKENDS=wayland`/`x11`) has
/// another; either is refused before `gtk4::init()` makes a single request of it. A socket file no
/// listener in this network namespace is bound to, or one no process this test can read holds, is
/// refused too, and every holder it can read must have been started so.
/// `require_headless_outputs` still checks, once GTK has connected, what the compositor reports.
///
/// **Fix round 3 (2026-09-29), two ways round 2's version could be satisfied by a desktop** (the
/// round-3 Codex review; both need a deliberately built environment, and both ended at the output
/// check after one connection, before any window):
/// - It found the listener by the path `/proc/net/unix` prints, which is the name the listener was
///   bound under, not the file this test would connect to: a rename, a bind mount or another mount
///   namespace can make that name lead to a different socket (an overlay of the sandbox's runtime
///   directory with the desktop's, in this test's mount namespace, passed it). Now the kernel says
///   which listener is bound to the file itself (`unix_listeners_by_file`: `NETLINK_SOCK_DIAG`'s
///   device and inode of each listener's socket file), compared with this test's own `stat` of it.
/// - `WLR_BACKENDS=headless` in a process's environment says what it was asked, not what it is: a
///   compositor that is not wlroots (Mutter) ignores the variable and runs on real outputs with it
///   inherited. Now the holder must also map a `libwlroots` library -- the only kind of compositor
///   the variable governs.
fn require_headless_compositor(resolved: &Path) -> Result<u32, String> {
    let meta = std::fs::metadata(resolved).map_err(|e| format!("{}: {e}", resolved.display()))?;
    let file = kernel_file_id(&meta);
    let listeners: Vec<u32> = unix_listeners_by_file()?
        .into_iter()
        .filter(|&(_, dev, ino)| (dev, ino) == file)
        .map(|(socket, _, _)| socket)
        .collect();
    if listeners.is_empty() {
        return Err(format!(
            "no listener in this network namespace is bound to the socket file {} -- its owner cannot be \
             checked",
            resolved.display()
        ));
    }
    let (owners, unreadable) = socket_holders(&listeners);
    if owners.is_empty() {
        return Err(if unreadable.is_empty() {
            format!("no process holds the socket at {}", resolved.display())
        } else {
            format!(
                "no process this test can read holds the socket at {}; these processes of this user could \
                 not be read, so whether one is its compositor, and how it was started, cannot be checked: \
                 {unreadable:?}. A process is unreadable when it is not dumpable: `/usr/bin/sway` carries \
                 `cap_sys_nice=ep`, so a sway started outside a user namespace is not, while one started \
                 inside the sandbox's `bwrap` is readable",
                resolved.display()
            )
        });
    }
    for &pid in &owners {
        let unreadable = |what: &str, e: std::io::Error| {
            format!(
                "/proc/{pid}/{what}: {e} (the compositor at {} is not dumpable, or another user's)",
                resolved.display()
            )
        };
        let environ = std::fs::read(format!("/proc/{pid}/environ")).map_err(|e| unreadable("environ", e))?;
        let backends = environ
            .split(|&b| b == 0)
            .find_map(|var| var.strip_prefix(b"WLR_BACKENDS="))
            .map(|v| String::from_utf8_lossy(v).into_owned());
        match backends {
            Some(b) if !b.is_empty() && b.split(',').all(|backend| backend == "headless") => {}
            other => {
                return Err(format!(
                    "the compositor at {} (pid {pid}) was started with WLR_BACKENDS={other:?}, not \"headless\" -- \
                     a desktop's, or one on a screen",
                    resolved.display()
                ))
            }
        }
        let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).map_err(|e| unreadable("maps", e))?;
        if !maps_wlroots(&maps) {
            return Err(format!(
                "the process holding {} (pid {pid}) was started with WLR_BACKENDS=headless but maps no \
                 libwlroots: not a wlroots compositor, the only kind that variable governs (Mutter ignores it)",
                resolved.display()
            ));
        }
    }
    Ok(owners[0])
}

/// Whether a `/proc/<pid>/maps` text maps a `libwlroots` shared library.
fn maps_wlroots(maps: &str) -> bool {
    maps.lines()
        .filter_map(|line| line.split_whitespace().nth(5))
        .any(|path| {
            Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("libwlroots"))
        })
}

/// A file's device and inode as `NETLINK_SOCK_DIAG`'s `UNIX_DIAG_VFS` reports them: the device in the
/// kernel's own encoding (`MKDEV`, major << 20 | minor, not `st_dev`'s), the inode's low 32 bits.
fn kernel_file_id(meta: &std::fs::Metadata) -> (u32, u32) {
    let dev = meta.dev();
    ((libc::major(dev) << 20) | libc::minor(dev), meta.ino() as u32)
}

/// The `NETLINK_SOCK_DIAG` messages' constants (linux/netlink.h, linux/sock_diag.h, linux/unix_diag.h) and
/// `TCP_LISTEN` (net/tcp_states.h).
mod diag {
    pub const SOCK_DIAG_BY_FAMILY: u16 = 20;
    pub const UDIAG_SHOW_VFS: u32 = 0x2;
    pub const UNIX_DIAG_VFS: u16 = 1;
    pub const TCP_LISTEN: u8 = 10;
    pub const NLMSG_NOOP: u16 = 1;
    pub const NLMSG_ERROR: u16 = 2;
    pub const NLMSG_DONE: u16 = 3;
    pub const NLMSG_OVERRUN: u16 = 4;
    pub const NLM_F_MULTI: u16 = 0x2;
    pub const NLM_F_DUMP_INTR: u16 = 0x10;
    pub const HEADER: usize = 16; // struct nlmsghdr
}

/// One datagram of a `NETLINK_SOCK_DIAG` dump reply, parsed: every listener it lists goes into `found` as
/// `(socket inode, device, file inode)`. `Ok(true)` when the datagram ends the dump, `Ok(false)` when
/// more is to come.
///
/// **Fail closed (fix round 4, 2026-09-29):** anything that is not a whole, consistent, successful dump is
/// an error, and the listeners found before it are not returned -- a partial list would be judged as if it
/// were the whole one. Round 3's parser returned what it had on a `NLMSG_DONE` carrying a negative errno
/// (the kernel's way of saying the dump failed part way), ignored `NLM_F_DUMP_INTR` (the dump changed
/// underneath it), and stopped only its attribute loop, not the parse, on an attribute that did not add up.
fn parse_unix_diag_reply(data: &[u8], found: &mut Vec<(u32, u32, u32)>) -> Result<bool, String> {
    use diag::*;
    let u32_at = |data: &[u8], at: usize| u32::from_ne_bytes(data[at..at + 4].try_into().unwrap());
    let u16_at = |data: &[u8], at: usize| u16::from_ne_bytes(data[at..at + 2].try_into().unwrap());
    let n = data.len();
    let mut at = 0;
    while at < n {
        if n - at < HEADER {
            return Err(format!(
                "NETLINK_SOCK_DIAG: {} stray bytes after the last message",
                n - at
            ));
        }
        let len = u32_at(data, at) as usize;
        if len < HEADER || at + len > n {
            return Err(format!(
                "NETLINK_SOCK_DIAG: a malformed reply ({len} bytes at {at} of {n})"
            ));
        }
        if u16_at(data, at + 6) & NLM_F_DUMP_INTR != 0 {
            return Err(
                "NETLINK_SOCK_DIAG: the dump was interrupted (NLM_F_DUMP_INTR): the table changed under it, \
                        so what it listed is not a snapshot"
                    .into(),
            );
        }
        let payload = &data[at + HEADER..at + len];
        match u16_at(data, at + 4) {
            NLMSG_NOOP => {}
            NLMSG_DONE => {
                // The dump's own status: 0, or the negative errno it failed with.
                let status = payload
                    .get(..4)
                    .map_or(0, |b| i32::from_ne_bytes(b.try_into().unwrap()));
                if status < 0 {
                    return Err(format!(
                        "NETLINK_SOCK_DIAG: the dump failed part way: {}",
                        std::io::Error::from_raw_os_error(-status)
                    ));
                }
                return Ok(true);
            }
            NLMSG_ERROR => {
                let errno = if len >= HEADER + 4 {
                    -(u32_at(data, at + HEADER) as i32)
                } else {
                    0
                };
                return Err(format!(
                    "NETLINK_SOCK_DIAG: {}",
                    std::io::Error::from_raw_os_error(errno)
                ));
            }
            NLMSG_OVERRUN => {
                return Err(
                    "NETLINK_SOCK_DIAG: the dump had an overrun and the kernel dropped part of it (NLMSG_OVERRUN)"
                        .into(),
                );
            }
            SOCK_DIAG_BY_FAMILY => {
                // struct unix_diag_msg (16 bytes), then its attributes.
                if payload.len() < 16 {
                    return Err(format!(
                        "NETLINK_SOCK_DIAG: a message of {} bytes, shorter than a unix_diag_msg's 16",
                        payload.len()
                    ));
                }
                if payload[2] == TCP_LISTEN {
                    let socket = u32_at(payload, 4);
                    let mut attr = 16;
                    while attr < payload.len() {
                        let attr_len = if payload.len() - attr >= 4 {
                            u16_at(payload, attr) as usize
                        } else {
                            0
                        };
                        if attr_len < 4 || attr + attr_len > payload.len() {
                            return Err(format!(
                                "NETLINK_SOCK_DIAG: a malformed attribute (length {attr_len} at {attr} of a \
                                 {}-byte message)",
                                payload.len()
                            ));
                        }
                        if u16_at(payload, attr + 2) & 0x3fff == UNIX_DIAG_VFS {
                            // struct unix_diag_vfs { udiag_vfs_ino, udiag_vfs_dev }
                            if attr_len < 12 {
                                return Err(format!(
                                    "NETLINK_SOCK_DIAG: a UNIX_DIAG_VFS attribute of {attr_len} bytes, too short \
                                     to hold a device and an inode (12)"
                                ));
                            }
                            found.push((socket, u32_at(payload, attr + 8), u32_at(payload, attr + 4)));
                        }
                        attr += (attr_len + 3) & !3;
                    }
                }
            }
            other => return Err(format!("NETLINK_SOCK_DIAG: an unexpected message type {other}")),
        }
        at += (len + 3) & !3;
    }
    Ok(false)
}

/// Every listening Unix socket in this network namespace that is bound to a file, as `(socket inode,
/// device, file inode)` with the device and file inode as [`kernel_file_id`] encodes them: the kernel's
/// own record of the file each listener is bound to (fix round 3, 2026-09-29). `/proc/net/unix`'s path
/// is only the name it was bound under.
fn unix_listeners_by_file() -> Result<Vec<(u32, u32, u32)>, String> {
    use diag::*;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let error = |what: &str| format!("NETLINK_SOCK_DIAG {what}: {}", std::io::Error::last_os_error());

    // SAFETY: a plain syscall; the descriptor it returns is owned by `fd` alone.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            libc::NETLINK_SOCK_DIAG,
        )
    };
    if fd < 0 {
        return Err(error("socket"));
    }
    // SAFETY: `fd` was just opened here and nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // struct nlmsghdr, then struct unix_diag_req: every listening Unix socket, with its file.
    let mut request = Vec::with_capacity(40);
    request.extend_from_slice(&40u32.to_ne_bytes());
    request.extend_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
    request.extend_from_slice(&((libc::NLM_F_REQUEST | libc::NLM_F_DUMP) as u16).to_ne_bytes());
    request.extend_from_slice(&1u32.to_ne_bytes());
    request.extend_from_slice(&0u32.to_ne_bytes());
    request.extend_from_slice(&[libc::AF_UNIX as u8, 0, 0, 0]);
    request.extend_from_slice(&(1u32 << TCP_LISTEN).to_ne_bytes());
    request.extend_from_slice(&0u32.to_ne_bytes());
    request.extend_from_slice(&UDIAG_SHOW_VFS.to_ne_bytes());
    request.extend_from_slice(&[0xff; 8]);
    // SAFETY: an all-zero `sockaddr_nl` is a valid value (the kernel's address, once the family is set).
    let mut kernel: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    kernel.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    // SAFETY: `request` and `kernel` are valid for reads of the lengths given.
    let sent = unsafe {
        libc::sendto(
            fd.as_raw_fd(),
            request.as_ptr().cast(),
            request.len(),
            0,
            (&kernel as *const libc::sockaddr_nl).cast(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if sent != request.len() as isize {
        return Err(error("send"));
    }
    let mut found = Vec::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        // SAFETY: `buffer` is valid for writes of its length.
        let n = unsafe { libc::recv(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len(), 0) };
        if n <= 0 {
            return Err(error("recv"));
        }
        if parse_unix_diag_reply(&buffer[..n as usize], &mut found)? {
            return Ok(found);
        }
    }
}

/// The processes holding any of the sockets `inodes` (a `/proc/<pid>/fd` link to `socket:[<inode>]`),
/// and, as `pid (name)`, every process of this user whose descriptors this test may not read.
fn socket_holders(inodes: &[u32]) -> (Vec<u32>, Vec<String>) {
    let links: Vec<String> = inodes.iter().map(|inode| format!("socket:[{inode}]")).collect();
    // SAFETY: `getuid` has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() }.to_string();
    let mut owners = Vec::new();
    let mut unreadable = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        match std::fs::read_dir(entry.path().join("fd")) {
            Ok(fds) => {
                let holds = fds.flatten().any(|fd| {
                    std::fs::read_link(fd.path())
                        .is_ok_and(|target| links.iter().any(|l| target.as_os_str() == l.as_str()))
                });
                if holds {
                    owners.push(pid);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                // `status` stays readable when `fd` is not: its first `Uid:` field is the real uid.
                let status = std::fs::read_to_string(entry.path().join("status")).unwrap_or_default();
                let mine = status
                    .lines()
                    .find_map(|line| line.strip_prefix("Uid:"))
                    .and_then(|ids| ids.split_whitespace().next())
                    .is_some_and(|real| real == uid);
                if mine {
                    let name = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
                    unreadable.push(format!("{pid} ({})", name.trim()));
                }
            }
            Err(_) => {}
        }
    }
    (owners, unreadable)
}

/// The compositor GTK connected to has only headless outputs (wlroots' `HEADLESS-<n>`, what every
/// sandbox harness here runs): no real screen, and no window on one. Checked right after
/// `gtk4::init()`, before any surface exists. A desktop -- or a compositor nested in one, whose
/// output (`WL-<n>`, `X11-<n>`) is a window on the owner's screen -- is refused whatever its socket's
/// path, which the checks in `sandbox_wayland_socket` cannot establish on their own.
fn require_headless_outputs() -> Result<Vec<String>, String> {
    let display = gtk4::gdk::Display::default().ok_or("GTK has no default display")?;
    let monitors = display.monitors();
    let deadline = Instant::now() + Duration::from_secs(2);
    let connectors = loop {
        let connectors: Vec<Option<String>> = (0..monitors.n_items())
            .filter_map(|i| monitors.item(i).and_downcast::<gtk4::gdk::Monitor>())
            .map(|monitor| monitor.connector().map(|c| c.to_string()))
            .collect();
        if (!connectors.is_empty() && connectors.iter().all(Option::is_some)) || Instant::now() > deadline {
            break connectors;
        }
        glib::MainContext::default().iteration(false);
        std::thread::sleep(Duration::from_millis(10));
    };
    let names: Vec<String> = connectors
        .into_iter()
        .map(|c| c.unwrap_or_else(|| "<unnamed>".into()))
        .collect();
    if !names.is_empty() && names.iter().all(|name| name.starts_with("HEADLESS-")) {
        Ok(names)
    } else {
        Err(format!(
            "the compositor's outputs are {names:?}, not only headless ones -- a real screen, or a window on one"
        ))
    }
}

/// Set on a copy of this binary that plays a compositor for [`refusal_chain_self_test`]: the socket
/// path it listens on. [`FAKE_WLROOTS`] names a library it maps without running any of it, and
/// [`FAKE_UNDUMPABLE`] makes it non-dumpable, as a sway with file capabilities is.
const FAKE_COMPOSITOR: &str = "CURSOR_ANIMATION_FAKE_COMPOSITOR";
const FAKE_WLROOTS: &str = "CURSOR_ANIMATION_FAKE_WLROOTS";
const FAKE_UNDUMPABLE: &str = "CURSOR_ANIMATION_FAKE_UNDUMPABLE";

/// A fake compositor: listens on `socket`, says so on stdout, and exits once its stdin closes (when
/// the self-test is done with it, or has died).
fn fake_compositor(socket: &Path) -> ! {
    use std::io::{Read, Write};
    let listener = std::os::unix::net::UnixListener::bind(socket).unwrap_or_else(|e| {
        eprintln!("fake compositor: cannot listen on {}: {e}", socket.display());
        std::process::exit(1)
    });
    if let Some(library) = std::env::var_os(FAKE_WLROOTS) {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(&library).expect("the library to map");
        // SAFETY: a private, read-only mapping of an open file; nothing ever reads it.
        let mapped = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(mapped, libc::MAP_FAILED, "mapping {library:?}");
    }
    if std::env::var_os(FAKE_UNDUMPABLE).is_some() {
        // SAFETY: `prctl` with constant arguments.
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) }, 0);
    }
    println!("listening");
    let _ = std::io::stdout().flush();
    let _ = std::io::stdin().read(&mut [0u8; 1]);
    drop(listener);
    std::process::exit(0)
}

/// A `libwlroots` shared library on this machine, if there is one: what a real wlroots compositor maps.
fn find_libwlroots() -> Option<PathBuf> {
    [
        "/usr/lib",
        "/usr/lib64",
        "/usr/local/lib",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
    ]
    .iter()
    .filter_map(|dir| std::fs::read_dir(dir).ok())
    .flat_map(|entries| entries.flatten())
    .map(|entry| entry.path())
    .find(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("libwlroots") && name.contains(".so"))
    })
}

/// `require_headless_compositor` against fake compositors -- copies of this binary that only listen
/// on a socket, so no display is opened -- run by every plain `cargo test` (fix round 3, 2026-09-29).
/// Until then this refusal chain's only evidence was scratch scripts outside the repository, and
/// deleting a check changed nothing that ran. Each case is one way a compositor that is not a headless
/// wlroots one could pass, and a positive control shows the check can pass at all.
///
/// **A case this machine cannot construct is skipped, with a note, not failed (fix round 4, 2026-09-29):**
/// the chain is a property of `require_headless_compositor`, and two environments cannot exercise it
/// without changing what it is. Without `NETLINK_SOCK_DIAG` (a container's seccomp filter, a kernel
/// without `CONFIG_UNIX_DIAG`) it refuses every socket -- correctly: it fails closed -- so no case can be
/// told from another. As root, or with `CAP_SYS_PTRACE` (a user namespace's root has it there), it can read
/// a non-dumpable process, so "a holder it cannot read" does not exist. Neither skip relaxes the check
/// itself, and a dump that answers wrongly -- empty, or without the listener this test just bound -- is a
/// failure, not a skip.
fn refusal_chain_self_test() {
    use std::io::BufRead;
    use std::process::{Child, Stdio};
    let dir = std::env::temp_dir().join(format!("cursor-animation-refusals-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory for the fake compositors' sockets");
    // Can the kernel be asked who listens on a socket file at all? Ask about a listener made just now.
    let probe = dir.join("probe");
    let probe_listener = std::os::unix::net::UnixListener::bind(&probe).expect("a listener to look for");
    let probe_file = kernel_file_id(&std::fs::metadata(&probe).expect("the probe's socket file"));
    let listed = unix_listeners_by_file();
    if let Err(why) = &listed {
        // What the skip rests on, held rather than assumed: with no way to look a listener up, the check
        // refuses the socket, and never accepts it.
        let verdict = require_headless_compositor(&probe);
        drop(probe_listener);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            verdict.is_err(),
            "`require_headless_compositor` accepted a socket it could not look up: {verdict:?}"
        );
        println!(
            "cursor_animation: refusal chain: skipped, no case can be constructed: NETLINK_SOCK_DIAG is \
             unavailable here ({why}). `require_headless_compositor` refuses every socket without it (it \
             fails closed), so one case cannot be told from another here"
        );
        return;
    }
    // A dump that answers, but does not list a listener this test just bound (or lists nothing at all), is
    // not an environment that lacks the interface: it is a defect -- in the parser, in `kernel_file_id`, or
    // in the kernel's answer -- and skipping it would hide the very thing the chain below stands on.
    let listed = listed.unwrap_or_default();
    let missing = !listed.iter().any(|&(_, dev, ino)| (dev, ino) == probe_file);
    drop(probe_listener);
    if missing {
        let _ = std::fs::remove_dir_all(&dir);
        panic!(
            "NETLINK_SOCK_DIAG answers ({} listeners) but not with the socket this test just bound at {}: \
             `unix_listeners_by_file` or `kernel_file_id` is wrong for this kernel",
            listed.len(),
            probe.display()
        );
    }
    let wlroots = find_libwlroots();
    let mut fakes: Vec<Child> = Vec::new();
    let mut start = |name: &str, backends: Option<&str>, maps_wlroots: bool, undumpable: bool| -> (PathBuf, u32) {
        let socket = dir.join(name);
        let mut command = Command::new(std::env::current_exe().expect("this test's own binary"));
        command
            .env(FAKE_COMPOSITOR, &socket)
            .env_remove("WLR_BACKENDS")
            .env_remove(FAKE_WLROOTS);
        command
            .env_remove(FAKE_UNDUMPABLE)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        if let Some(backends) = backends {
            command.env("WLR_BACKENDS", backends);
        }
        if let (true, Some(library)) = (maps_wlroots, &wlroots) {
            command.env(FAKE_WLROOTS, library);
        }
        if undumpable {
            command.env(FAKE_UNDUMPABLE, "1");
        }
        let mut child = command.spawn().expect("starting a fake compositor");
        let mut line = String::new();
        let _ = std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line);
        assert_eq!(line.trim(), "listening", "the fake compositor {name} did not start");
        let pid = child.id();
        fakes.push(child);
        (socket, pid)
    };
    let mut failures = Vec::new();
    let mut expect_refusal = |case: &str, socket: &Path, naming: &str| match require_headless_compositor(socket) {
        Err(e) if e.contains(naming) => println!("cursor_animation: refusal chain: {case}: refused ({e})"),
        other => failures.push(format!("{case}: expected a refusal naming {naming:?}, got {other:?}")),
    };

    // WLR_BACKENDS says a nested or real backend.
    let (socket, _) = start("nested", Some("wayland"), true, false);
    expect_refusal("WLR_BACKENDS=wayland", &socket, "WLR_BACKENDS=Some(\"wayland\")");
    // No WLR_BACKENDS at all: a desktop's compositor.
    let (socket, _) = start("desktop", None, true, false);
    expect_refusal("no WLR_BACKENDS", &socket, "WLR_BACKENDS=None");
    // Codex's (b): WLR_BACKENDS=headless inherited by a compositor that is not wlroots.
    let (socket, pid) = start("not-wlroots", Some("headless"), false, false);
    expect_refusal(
        "headless, not wlroots",
        &socket,
        &format!("(pid {pid}) was started with WLR_BACKENDS=headless but maps no libwlroots"),
    );
    // Codex's (a): the name a headless listener was bound under now leads to a different socket
    // (renamed here; a bind mount or another mount namespace does the same). `/proc/net/unix` still
    // prints the headless one's path, so round 2's check approved it; the file is the other's.
    let (headless, _) = start("renamed", Some("headless"), true, false);
    let (other, other_pid) = start("impostor", None, true, false);
    std::fs::rename(&headless, dir.join("renamed.moved")).expect("moving the headless socket away");
    std::fs::rename(&other, &headless).expect("putting the other socket in its place");
    expect_refusal(
        "a re-pointed name",
        &headless,
        &format!("(pid {other_pid}) was started with WLR_BACKENDS=None"),
    );
    // A socket file nothing listens on any more.
    let orphan = dir.join("orphan");
    drop(std::os::unix::net::UnixListener::bind(&orphan).expect("a listener to close"));
    expect_refusal(
        "nobody listening",
        &orphan,
        "no listener in this network namespace is bound to",
    );
    // A holder this test cannot read (sway started outside a user namespace, with its capability).
    let (socket, pid) = start("undumpable", Some("headless"), true, true);
    if std::fs::read_dir(format!("/proc/{pid}/fd")).is_ok() {
        println!(
            "cursor_animation: refusal chain: not dumpable: skipped, this process can read a non-dumpable \
             one's /proc/{pid}/fd (root, or CAP_SYS_PTRACE), so a holder it cannot read cannot be constructed here"
        );
    } else {
        expect_refusal(
            "not dumpable",
            &socket,
            "could not be read, so whether one is its compositor",
        );
        expect_refusal("not dumpable, named", &socket, &format!("{pid} ("));
    }
    // The positive control: a check that cannot pass proves nothing.
    match &wlroots {
        Some(library) => {
            let (socket, pid) = start("headless", Some("headless"), true, false);
            match require_headless_compositor(&socket) {
                Ok(found) if found == pid => {
                    println!(
                        "cursor_animation: refusal chain: headless and mapping {}: accepted",
                        library.display()
                    )
                }
                other => failures.push(format!(
                    "a headless wlroots fake (pid {pid}): expected Ok({pid}), got {other:?}"
                )),
            }
        }
        None => {
            println!("cursor_animation: refusal chain: no libwlroots on this machine; the positive control is skipped")
        }
    }

    for mut fake in fakes {
        drop(fake.stdin.take());
        let _ = fake.wait();
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(failures.is_empty(), "the refusal chain:\n{}", failures.join("\n"));
    println!("cursor_animation: refusal chain: all cases held");
}

/// [`parse_unix_diag_reply`] on synthetic replies -- no socket, so it runs anywhere -- for the ways a dump
/// can fail or lie that the kernel's real one never shows this test (fix round 4, 2026-09-29): a `NLMSG_DONE`
/// that carries an error, a dump flagged inconsistent, an attribute that does not add up. Each must be an
/// error: a parser that returned what it had found so far would leave the caller judging a compositor by
/// half a list.
fn netlink_parser_self_test() {
    use diag::*;
    /// A netlink message (`struct nlmsghdr`, then `payload`), padded to four bytes.
    fn message(ty: u16, flags: u16, payload: &[u8]) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&((HEADER + payload.len()) as u32).to_ne_bytes());
        m.extend_from_slice(&ty.to_ne_bytes());
        m.extend_from_slice(&flags.to_ne_bytes());
        m.extend_from_slice(&1u32.to_ne_bytes());
        m.extend_from_slice(&0u32.to_ne_bytes());
        m.extend_from_slice(payload);
        m.resize(m.len().next_multiple_of(4), 0);
        m
    }
    /// One attribute (`struct nlattr`, then `payload`), padded to four bytes.
    fn attribute(ty: u16, payload: &[u8]) -> Vec<u8> {
        let mut a = Vec::new();
        a.extend_from_slice(&((4 + payload.len()) as u16).to_ne_bytes());
        a.extend_from_slice(&ty.to_ne_bytes());
        a.extend_from_slice(payload);
        a.resize(a.len().next_multiple_of(4), 0);
        a
    }
    /// A `struct unix_diag_msg` for a socket in `state` with inode `socket`, then `attributes`, as a message.
    fn diag_message(state: u8, socket: u32, attributes: &[u8], flags: u16) -> Vec<u8> {
        let mut payload = vec![libc::AF_UNIX as u8, 0, state, 0];
        payload.extend_from_slice(&socket.to_ne_bytes());
        payload.extend_from_slice(&[0; 8]); // udiag_cookie
        payload.extend_from_slice(attributes);
        message(SOCK_DIAG_BY_FAMILY, flags, &payload)
    }
    let vfs = |ino: u32, dev: u32| attribute(UNIX_DIAG_VFS, &[ino.to_ne_bytes(), dev.to_ne_bytes()].concat());
    let listener = |socket: u32, attributes: &[u8]| diag_message(TCP_LISTEN, socket, attributes, NLM_F_MULTI);
    let done = |status: i32| message(NLMSG_DONE, NLM_F_MULTI, &status.to_ne_bytes());
    let raw_attribute = |len: u16, ty: u16| [len.to_ne_bytes(), ty.to_ne_bytes()].concat();

    #[derive(Debug)]
    enum Want {
        Done(Vec<(u32, u32, u32)>),
        More(Vec<(u32, u32, u32)>),
        Error(&'static str),
    }
    let mut failures = Vec::new();
    let mut cases = 0;
    let mut check = |name: &str, parts: &[Vec<u8>], want: Want| {
        cases += 1;
        let mut found = Vec::new();
        let got = parse_unix_diag_reply(&parts.concat(), &mut found);
        let held = match (&want, &got) {
            (Want::Done(list), Ok(true)) | (Want::More(list), Ok(false)) => &found == list,
            (Want::Error(needle), Err(e)) => e.contains(needle),
            _ => false,
        };
        if !held {
            failures.push(format!("  {name}: wanted {want:?}, got {got:?} with {found:?}"));
        }
    };

    // What a good dump looks like, so the builders are known to build one.
    check(
        "a listener, then the end",
        &[listener(7, &vfs(100, 200)), done(0)],
        Want::Done(vec![(7, 200, 100)]),
    );
    check(
        "a listener, no end yet",
        &[listener(7, &vfs(100, 200))],
        Want::More(vec![(7, 200, 100)]),
    );
    check(
        "the file attribute after another, padded one",
        &[listener(7, &[attribute(2, b"/x\0"), vfs(100, 200)].concat()), done(0)],
        Want::Done(vec![(7, 200, 100)]),
    );
    check(
        "a listener with no file (abstract)",
        &[listener(8, &[]), done(0)],
        Want::Done(vec![]),
    );
    check(
        "a socket that is not listening",
        &[diag_message(1, 9, &vfs(1, 2), NLM_F_MULTI), done(0)],
        Want::Done(vec![]),
    );
    check(
        "a no-op message",
        &[message(NLMSG_NOOP, 0, &[]), done(0)],
        Want::Done(vec![]),
    );

    // A dump that failed: the listeners found before the failure are not a list.
    check(
        "done carrying ENOMEM after a listener (Codex)",
        &[listener(7, &vfs(100, 200)), done(-12)],
        Want::Error("dump failed"),
    );
    check("done carrying EIO alone", &[done(-5)], Want::Error("dump failed"));
    check(
        "an error message",
        &[message(
            NLMSG_ERROR,
            0,
            &[(-13i32).to_ne_bytes().to_vec(), vec![0; 16]].concat(),
        )],
        Want::Error("Permission denied"),
    );
    check(
        "an overrun",
        &[listener(7, &vfs(100, 200)), message(NLMSG_OVERRUN, 0, &[])],
        Want::Error("overrun"),
    );
    // A dump that changed underneath it.
    check(
        "a listener flagged NLM_F_DUMP_INTR (Codex)",
        &[
            diag_message(TCP_LISTEN, 7, &vfs(100, 200), NLM_F_MULTI | NLM_F_DUMP_INTR),
            done(0),
        ],
        Want::Error("interrupted"),
    );
    check(
        "a done flagged NLM_F_DUMP_INTR",
        &[
            listener(7, &vfs(100, 200)),
            message(NLMSG_DONE, NLM_F_MULTI | NLM_F_DUMP_INTR, &0i32.to_ne_bytes()),
        ],
        Want::Error("interrupted"),
    );
    // A reply that does not add up: the whole parse fails, not just the loop it was in.
    check(
        "an attribute shorter than its own header, after a good one (Codex)",
        &[listener(7, &[vfs(100, 200), raw_attribute(2, 99)].concat()), done(0)],
        Want::Error("malformed attribute"),
    );
    check(
        "two stray bytes after the last attribute",
        &[listener(7, &[vfs(100, 200), vec![1, 2]].concat()), done(0)],
        Want::Error("malformed attribute"),
    );
    check(
        "an attribute longer than its message",
        &[listener(7, &[vfs(100, 200), raw_attribute(40, 99)].concat()), done(0)],
        Want::Error("malformed attribute"),
    );
    check(
        "a file attribute too short to hold a file",
        &[listener(7, &attribute(UNIX_DIAG_VFS, &[0; 4])), done(0)],
        Want::Error("UNIX_DIAG_VFS"),
    );
    check(
        "a message shorter than a unix_diag_msg",
        &[message(SOCK_DIAG_BY_FAMILY, NLM_F_MULTI, &[0; 8]), done(0)],
        Want::Error("shorter"),
    );
    check(
        "a message of a type no dump sends",
        &[message(99, 0, &[0; 16]), done(0)],
        Want::Error("unexpected message type"),
    );
    check(
        "stray bytes after the last message",
        &[listener(7, &vfs(100, 200)), vec![1, 2, 3]],
        Want::Error("stray"),
    );
    check(
        "a message longer than the datagram",
        &[{
            let mut m = listener(7, &vfs(100, 200));
            m[..4].copy_from_slice(&500u32.to_ne_bytes());
            m
        }],
        Want::Error("malformed reply"),
    );

    assert!(
        failures.is_empty(),
        "the NETLINK_SOCK_DIAG reply parser, {} of {cases} cases:\n{}",
        failures.len(),
        failures.join("\n")
    );
    println!("cursor_animation: netlink parser: {cases} cases held");
}

fn main() {
    if let Some(socket) = std::env::var_os(FAKE_COMPOSITOR) {
        fake_compositor(Path::new(&socket));
    }
    // The refusal chain below, held against fake compositors first -- on every run, `--ignored` or
    // not, so a plain `cargo test` fails if a check stops refusing (fix round 3, 2026-09-29).
    refusal_chain_self_test();
    netlink_parser_self_test();
    if !std::env::args().any(|arg| arg == "--ignored") {
        println!("cursor_animation: skipped (requires an isolated GUI sandbox; pass --ignored)");
        return;
    }
    let expect_own = match (
        std::env::args().any(|arg| arg == "--expect-own-buffers"),
        std::env::args().any(|arg| arg == "--expect-fallback"),
    ) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        (false, false) => None,
        (true, true) => panic!("--expect-own-buffers and --expect-fallback contradict each other"),
    };
    // As `shell`'s own `main` does: a pipe or socket on stdin whose writer stays open would reach the
    // embedded nvim as a buffer and block it at startup (`neovide_editor::stdin`). A harness that
    // launches this test with such a stdin (2026-09-28: an agent tool's socket) otherwise times out
    // with "never showed a red cursor", on either presentation path.
    if let Ok(Some(kind)) = neovide_editor::detach_stdin_from_nvim() {
        println!("[stdin] a {kind} on stdin would reach nvim as a buffer; stdin is /dev/null now");
    }
    // No session bus, as every guarded test runs (`own_x_server::cut_session_bus`, fix round 3): a
    // bare run on the desktop's own bus would ask it for the accessibility bus and D-Bus-activate the
    // portal and GVfs services, which then run with the owner's real `HOME`.
    own_x_server::cut_session_bus("cursor_animation");
    let sandbox_socket = match sandbox_wayland_socket() {
        Ok(socket) => {
            println!("cursor_animation: sandbox Wayland display {}", socket.display());
            socket
        }
        Err(e) => {
            eprintln!("cursor_animation: refusing to open a window: {e}");
            std::process::exit(1);
        }
    };
    match require_headless_compositor(&sandbox_socket) {
        Ok(pid) => println!("cursor_animation: compositor pid {pid}, started headless"),
        Err(e) => {
            eprintln!("cursor_animation: refusing to connect: {e}");
            std::process::exit(1);
        }
    }
    // Before GTK initialises, while no other thread reads the environment: nothing left to fall back
    // to but the checked Wayland socket (no X server, XWayland's included), named by the absolute
    // path that was checked, so libwayland cannot resolve the name to anything else.
    // `WAYLAND_SOCKET` (an inherited, already-connected fd) would win over `WAYLAND_DISPLAY` in
    // libwayland; a compositor sets it for a client it spawns, so it may name the desktop's.
    std::env::set_var("WAYLAND_DISPLAY", &sandbox_socket);
    std::env::remove_var("WAYLAND_SOCKET");
    std::env::remove_var("DISPLAY");
    std::env::set_var("GDK_BACKEND", "wayland");
    // GTK's own input method, as `own_x_server::isolate` sets: an inherited `GTK_IM_MODULE=fcitx`
    // makes the pane's input context reach for fcitx5 over the session bus the moment it takes focus
    // (seen 2026-09-29 on a private bus: `GetNameOwner("org.fcitx.Fcitx5")` and a `NameOwnerChanged`
    // match for it). On the desktop's bus that is the owner's live fcitx5, and this test's focus-in
    // would reach it.
    for var in ["GTK_IM_MODULE", "XMODIFIERS"] {
        if let Some(inherited) = std::env::var_os(var) {
            println!(
                "cursor_animation: ignoring the inherited {var}={}; GTK's own input method",
                inherited.to_string_lossy()
            );
        }
    }
    std::env::set_var("GTK_IM_MODULE", "gtk-im-context-simple");
    std::env::remove_var("XMODIFIERS");
    gtk4::init().expect("GTK initialization requires the isolated sandbox display");
    match require_headless_outputs() {
        Ok(outputs) => println!("cursor_animation: headless outputs {outputs:?}"),
        Err(e) => {
            eprintln!("cursor_animation: refusing to open a window: {e}");
            std::process::exit(1);
        }
    }
    let scratch = PathBuf::from(format!("/tmp/neovibe-cursor-animation-{}", std::process::id()));
    std::fs::create_dir(&scratch).expect("create unique test scratch directory");
    let socket = scratch.join("nvim.sock");
    let init = concat!(
        "lua vim.o.guifont='monospace:h16'; vim.o.number=false; vim.o.relativenumber=false; ",
        "vim.o.signcolumn='no'; vim.o.laststatus=0; vim.o.cmdheight=0; vim.o.ruler=false; ",
        "vim.o.showcmd=false; vim.o.showmode=false; vim.o.scrolloff=0; vim.o.wrap=false; ",
        "vim.o.guicursor='a:block-Cursor-blinkon0'; vim.g.neovide_cursor_vfx_mode=''; ",
        "vim.cmd('highlight Normal guifg=#000000 guibg=#000000'); ",
        "vim.cmd('highlight NormalNC guifg=#000000 guibg=#000000'); ",
        "vim.cmd('highlight Cursor guifg=#ff0000 guibg=#ff0000'); ",
        "vim.api.nvim_buf_set_lines(0,0,-1,false,vim.fn['repeat']({string.rep(' ',80)},40)); ",
        "vim.api.nvim_win_set_cursor(0,{10,10})"
    );
    let pane = NeovideEditorPane::with_options(NeovideEditorPaneOptions {
        clean: true,
        cwd: Some(scratch.clone()),
        extra_nvim_args: vec![
            "-i".into(),
            "NONE".into(),
            "--listen".into(),
            socket.to_string_lossy().into_owned(),
            "-c".into(),
            init.into(),
        ],
        ..Default::default()
    });
    // Match the fixture's Normal background, including the host-owned top remainder strip.
    pane.set_clear_color((0, 0, 0));
    let fixed = gtk4::Fixed::new();
    pane.widget().set_size_request(820, 560);
    fixed.put(pane.widget(), 0.0, 0.0);
    let window = Window::builder()
        .title("neovibe cursor animation regression")
        .default_width(1000)
        .default_height(700)
        .child(&fixed)
        .build();
    let counter = RenderCounter::new(pane.widget());
    window.present();
    pane.grab_focus();
    pane.set_focused(true);
    let capture = Rc::new(Capture::default());
    capture.enabled.set(true);
    let mut observer = Some(PaintObserver::new(pane.widget(), &capture, &counter));
    // Assertions stay outside GLib callbacks, and teardown also runs for a failed assertion.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(&pane, &fixed, &socket, &capture, &counter, &mut observer, expect_own)
    }));
    drop(observer);
    let shutdown = pane.shutdown();
    window.close();
    spin(Duration::from_millis(30));
    let _ = std::fs::remove_dir_all(&scratch);
    assert!(shutdown, "nvim did not shut down cleanly");
    let failures = result.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    assert!(
        failures.is_empty(),
        "cursor animation failures:\n{}",
        failures.join("\n")
    );
    println!("cursor_animation: passed motion, idle, copy pixels, resize, and actual re-realize");
}
