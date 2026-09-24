//! Real GTK/GLArea regressions for cursor animation and presentation-surface lifetime.
//!
//! Run ONLY in the one, isolated GUI sandbox permitted by AGENTS.md:
//! `cargo test -p neovide-editor --test cursor_animation -- --ignored`
//! A plain main keeps GTK and winit on the main thread. The test starts a real clean nvim,
//! drives the product pane, and reads its actual GL framebuffer after GTK paints. Red cursor
//! pixels on a black background prove intermediate positions; the harness's cursor getter only
//! reports the destination and would not prove animation. Readback is deliberately test-only.
//!
//! An independent nvim RPC confirms each input's destination BEFORE the GTK loop resumes. This
//! makes the idle-gap regression deterministic instead of depending on whether nvim's redraw
//! happens to arrive before or after an extra, still-unchanged frame. These synchronized cases
//! test animation correctness, not input-to-photon latency or production GPU throughput.

use std::cell::{Cell, RefCell};
use std::ffi::{c_void, CString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::glib::{self, translate::IntoGlib};
use gtk4::prelude::*;
use gtk4::{GLArea, Window};
use neovide_editor::{NeovideEditorPane, NeovideEditorPaneOptions};

type Point = (f64, f64);

#[derive(Clone, Copy, Debug)]
struct Sample {
    at: Instant,
    center: Option<Point>,
    pixels: usize,
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

fn is_cursor(rgba: &[u8]) -> bool {
    rgba[0] > 160 && rgba[1] < 80 && rgba[2] < 80
}

// The pane's render handler returns Stop. A subsequently connected render handler therefore
// cannot count frames reliably; a signal emission hook runs before the handlers/accumulator.
struct HookData {
    area: usize,
    count: Rc<Cell<u64>>,
    framebuffer: Rc<Cell<i32>>,
}

struct RenderCounter {
    signal: u32,
    hook: libc::c_ulong,
    count: Rc<Cell<u64>>,
    framebuffer: Rc<Cell<i32>>,
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
        let signal =
            unsafe { glib::gobject_ffi::g_signal_lookup(c"render".as_ptr(), GLArea::static_type().into_glib()) };
        assert_ne!(signal, 0, "GtkGLArea has no render signal");
        let data = Box::into_raw(Box::new(HookData {
            area: area.as_ptr() as usize,
            count: count.clone(),
            framebuffer: framebuffer.clone(),
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
        let handler = clock.connect_after_paint(move |_| {
            let renders = count.get();
            if !capture.enabled.get() || renders == capture.last_render.get() {
                return;
            }
            capture.last_render.set(renders);
            match read_frame(&area, framebuffer.get(), capture.region.get()) {
                Ok(frame) => {
                    let (center, pixels) = frame.cursor();
                    capture.samples.borrow_mut().push(Sample {
                        at: Instant::now(),
                        center,
                        pixels,
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

fn run(
    pane: &NeovideEditorPane,
    fixed: &gtk4::Fixed,
    socket: &Path,
    capture: &Rc<Capture>,
    counter: &RenderCounter,
    observer: &mut Option<PaintObserver>,
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
    presentation_lifetime(pane, fixed, socket, capture, counter, observer);
    failures
}

fn main() {
    if !std::env::args().any(|arg| arg == "--ignored") {
        println!("cursor_animation: skipped (requires an isolated GUI sandbox; pass --ignored)");
        return;
    }
    gtk4::init().expect("GTK initialization requires the isolated sandbox display");
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
        run(&pane, &fixed, &socket, &capture, &counter, &mut observer)
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
