//! Performance baseline driver for `LiveHarness` using the GTK4/GLArea/Skia render pipeline.
//! `NeovimRuntime` owns the nvim child's stdio and exposes no second RPC channel, so the
//! workload runs in-process through `send_text_input` and `is_ready`. Keyboard input follows
//! `SerialCommand::Keyboard` to `nvim.input()`, using the same path as interactive input.
//! The workload uses a fixed file, typed text, scroll-key sequence, and 20ms input pacing.
//! This binary runs the window and workload and captures frame intervals for idle, typing,
//! and scrolling. An external Python process samples CPU, RSS, and GPU usage for this process
//! and its embedded nvim child, using stdout markers to identify measurement intervals.
//! `LiveHarness::with_options` uses `OpenMode::None`, so readiness measures a blank buffer.
//! The workload file is opened afterward with `:e <path><CR>` and timed separately.
//! `send_text_input` returns after enqueueing, without waiting for an RPC acknowledgement.
//! The 20ms pacing is between enqueue calls; a 30ms delay allows input to settle at boundaries.
//! Rendering uses GTK's frame-clock tick callback. A warm-up idle, typing, and undo cycle
//! runs before measurement to guard against embedded nvim startup instability.

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, GLArea};

use skia_safe::gpu::gl::{Format as GlFormat, FramebufferInfo, Interface as GlInterface};
use skia_safe::gpu::{backend_render_targets, direct_contexts, surfaces, DirectContext, SurfaceOrigin};
use skia_safe::{Canvas, Color4f, Paint, PaintStyle, Rect, Surface};

use neovide::live_harness::{LiveHarness, LiveHarnessOptions};
use neovide::units::{GridSize, PixelRect};

const APP_ID: &str = "cn.huntergrey.neovibe.neovide_embed_live.perf_baseline";

/// Number of `add_tick_callback` invocations between `TickStats` summary log lines.
/// Keep this cadence aligned with `main.rs`; this binary has its own render and tick callbacks,
/// so changes to the interactive render loop must also be applied here for comparable measurements.
const TICK_LOG_EVERY_N_TICKS: u64 = 300;

// ----------------------------------------------------------------------------
// Workload constants -- verbatim copies of the stock phase's driver.py constants
// (see /tmp/.../scratchpad/p11_perf_baseline/driver.py). Any change here must be mirrored there
// (or vice versa) for the comparison to stay valid.
// ----------------------------------------------------------------------------
const GRID_WIDTH: u32 = 120;
const GRID_HEIGHT: u32 = 40;
const IDLE_DURATION_S: f64 = 13.0;
const SETTLE_S: f64 = 2.0;
const TYPE_KEY_DELAY_MS: u64 = 20;
const SCROLL_KEY_DELAY_MS: u64 = 20;
/// Stands in for the stock driver's synchronous `nvim.api.get_current_line()` barrier at each
/// phase boundary -- see module doc point 2.
const BARRIER_DELAY_MS: u64 = 30;

const TYPED_TEXT: &str = "fn p11_workload_probe(seed: u64) -> u64 {\n    let mut acc: u64 = seed;\n    for i in 0..2000u64 {\n        acc = acc.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);\n        if acc % 101 == 0 {\n            acc ^= i;\n        }\n    }\n    acc\n}\n\n";

fn scroll_keys() -> Vec<&'static str> {
    let unit: [&'static str; 12] =
        ["gg", "<C-d>", "<C-d>", "<C-d>", "<C-d>", "<C-d>", "<C-f>", "<C-f>", "<C-f>", "<C-u>", "<C-u>", "<C-b>"];
    let mut v = Vec::with_capacity(12 * 15 + 4);
    for _ in 0..15 {
        v.extend_from_slice(&unit);
    }
    v.extend_from_slice(&["G", "gg", "G", "gg"]);
    v
}

// ----------------------------------------------------------------------------
// GL/Skia plumbing -- copied verbatim from `neovide_embed_live::main` (itself copied from
// `neovide_embed`/`gl_skia_test`). See that crate's own doc comments for the full GL/Skia-interop
// rationale; nothing about it changes for this self-driving harness.
// ----------------------------------------------------------------------------

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
        let fb_info = FramebufferInfo { fboid: fboid as u32, format: GlFormat::RGBA8.into(), ..Default::default() };
        let render_target = backend_render_targets::make_gl((self.fb_width, self.fb_height), 0, 8, fb_info);
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

fn make_gl_interface() -> GlInterface {
    unsafe {
        let lib = libloading::os::unix::Library::this();
        GlInterface::new_load_with(move |name: &str| resolve_gl_proc(&lib, name))
            .expect("failed to assemble Skia GL interface via process-wide symbol lookup")
    }
}

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

const CONTENT_MARGIN: f32 = 40.0;
const OUTSIDE_COLOR: Color4f = Color4f::new(0.55, 0.15, 0.55, 1.0);
const BORDER_COLOR: Color4f = Color4f::new(0.95, 0.85, 0.25, 1.0);
const STARTING_COLOR: Color4f = Color4f::new(0.12, 0.12, 0.16, 1.0);
const FAILED_COLOR: Color4f = Color4f::new(0.5, 0.05, 0.05, 1.0);

fn compute_content_region(fb_width: i32, fb_height: i32) -> PixelRect<f32> {
    let (w, h) = (fb_width as f32, fb_height as f32);
    if w > CONTENT_MARGIN * 2.0 + 20.0 && h > CONTENT_MARGIN * 2.0 + 20.0 {
        PixelRect::from_min_max((CONTENT_MARGIN, CONTENT_MARGIN), (w - CONTENT_MARGIN, h - CONTENT_MARGIN))
    } else {
        PixelRect::from_min_max((0.0, 0.0), (w.max(1.0), h.max(1.0)))
    }
}

fn fill_content_region(canvas: &Canvas, content_region: &PixelRect<f32>, color: Color4f) {
    let mut paint = Paint::default();
    paint.set_color4f(color, None);
    canvas.draw_rect(
        Rect::from_ltrb(content_region.min.x, content_region.min.y, content_region.max.x, content_region.max.y),
        &paint,
    );
}

// ----------------------------------------------------------------------------
// Measurement-specific state
// ----------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    PreMeasurement,
    Idle,
    Typing,
    Scroll,
}

#[derive(Default)]
struct FrameStats {
    idle: Vec<f32>,
    typing: Vec<f32>,
    scroll: Vec<f32>,
}

/// Messages the background workload-script thread sends to the GTK main thread (the only thread
/// allowed to touch `LiveHarness`, which lives in a non-`Send` `Rc<RefCell<_>>`).
enum Ctrl {
    Input(String),
    SetPhase(Phase),
    Finish,
}

enum LiveState {
    NotStarted,
    Starting,
    Ready(Box<LiveSession>),
    Failed(String),
}

struct LiveSession {
    harness: LiveHarness,
    last_frame: Instant,
    frame_count: u64,
    logged_ready: bool,
    /// The `animating` value returned by `render_frame`, updated after every render callback.
    /// The tick callback uses it to decide whether another frame is needed.
    last_animating: Cell<bool>,
    /// `harness.redraw_batches_seen()` when last checked by the render or tick callback.
    last_seen_batches: Cell<u64>,
    /// Set when a workload thread's `Ctrl::Input` message is applied to request a frame.
    /// Read and cleared by the tick callback.
    wants_frame: Cell<bool>,
}

/// Counts, over rolling windows of `TICK_LOG_EVERY_N_TICKS` tick-callback invocations, how many
/// ticks actually issued a `queue_render()` vs. how many skipped it. Ported verbatim from
/// `main.rs`'s own `TickStats` (see that file for full rationale) so this binary's stdout carries
/// the same direct before/after evidence of the fix.
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

fn stats_json(dts: &[f32]) -> String {
    if dts.is_empty() {
        return "{\"n\":0}".to_string();
    }
    let mut sorted = dts.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = sorted.len();
    let sum: f32 = sorted.iter().sum();
    let avg = sum / n as f32;
    let min = sorted[0];
    let max = sorted[n - 1];
    let p50 = sorted[n / 2];
    let p95_idx = ((n as f32) * 0.95) as usize;
    let p95 = sorted[p95_idx.min(n - 1)];
    let effective_fps = if sum > 0.0 { n as f32 / sum } else { 0.0 };
    format!(
        "{{\"n\":{n},\"wall_seconds\":{sum:.3},\"avg_ms\":{:.3},\"min_ms\":{:.3},\"max_ms\":{:.3},\"p50_ms\":{:.3},\"p95_ms\":{:.3},\"effective_fps\":{:.2}}}",
        avg * 1000.0,
        min * 1000.0,
        max * 1000.0,
        p50 * 1000.0,
        p95 * 1000.0,
        effective_fps
    )
}

fn phase_marker(name: &str) {
    println!("NEOVIBE_PHASE:{name}");
    let _ = std::io::stdout().flush();
}

fn send_and_wait(tx: &Sender<Ctrl>, text: &str, delay_ms: u64, nvim_exited: &Arc<AtomicBool>) -> bool {
    if tx.send(Ctrl::Input(text.to_string())).is_err() {
        return false;
    }
    thread::sleep(Duration::from_millis(delay_ms));
    !nvim_exited.load(Ordering::Relaxed)
}

/// Runs entirely on a background OS thread (never the GTK thread) so its `thread::sleep` calls
/// never block the render loop -- unlike the stock driver (a standalone Python script that *was*
/// the whole process), this binary must keep rendering (and thus keep capturing frame-pacing data)
/// while this script executes.
fn run_workload_script(
    tx: Sender<Ctrl>,
    nvim_exited: Arc<AtomicBool>,
    workload_path: String,
    sanity_path: String,
) {
    macro_rules! bail_if_dead {
        () => {
            if nvim_exited.load(Ordering::Relaxed) {
                phase_marker("NVIM_DIED_UNEXPECTEDLY");
                let _ = tx.send(Ctrl::Finish);
                return;
            }
        };
    }

    // ---- open the workload file (see module doc point 1: LiveHarness cannot open a file at
    // launch, so this happens post-readiness via the same `:e` a real user's first keystroke
    // would use) ----
    phase_marker("FILE_OPEN_START");
    if !send_and_wait(&tx, &format!(":e {workload_path}<CR>"), 300, &nvim_exited) {
        phase_marker("NVIM_DIED_UNEXPECTEDLY");
        let _ = tx.send(Ctrl::Finish);
        return;
    }
    phase_marker("FILE_OPENED");
    bail_if_dead!();

    // ---- warm-up: identical shape to the stock driver's own warm-up (idle, then a substantial
    // typing burst, then undo) -- see module doc for why this is replicated regardless of whether
    // the specific instability it guards against reproduces under LiveHarness too. ----
    phase_marker("WARMUP_START");
    thread::sleep(Duration::from_secs_f64(IDLE_DURATION_S));
    send_and_wait(&tx, "gg", 30, &nvim_exited);
    send_and_wait(&tx, "i", 30, &nvim_exited);
    for ch in TYPED_TEXT.chars() {
        if !send_and_wait(&tx, &ch.to_string(), TYPE_KEY_DELAY_MS, &nvim_exited) {
            phase_marker("NVIM_DIED_UNEXPECTEDLY");
            let _ = tx.send(Ctrl::Finish);
            return;
        }
    }
    send_and_wait(&tx, "<Esc>", BARRIER_DELAY_MS, &nvim_exited);
    send_and_wait(&tx, ":undo<CR>", BARRIER_DELAY_MS, &nvim_exited);
    phase_marker("WARMUP_DONE");
    bail_if_dead!();

    // ---- settle ----
    phase_marker("SETTLE_START");
    thread::sleep(Duration::from_secs_f64(SETTLE_S));

    // ---- idle measurement window: zero input, matching the stock driver exactly ----
    phase_marker("IDLE_START");
    let _ = tx.send(Ctrl::SetPhase(Phase::Idle));
    thread::sleep(Duration::from_secs_f64(IDLE_DURATION_S));
    phase_marker("IDLE_END");
    bail_if_dead!();

    // ---- workload: typing sub-phase (marker emitted *before* the gg/i lead-in, matching the
    // stock driver's own t_workload_start placement) ----
    phase_marker("TYPING_START");
    let _ = tx.send(Ctrl::SetPhase(Phase::Typing));
    send_and_wait(&tx, "gg", 30, &nvim_exited);
    send_and_wait(&tx, "i", 30, &nvim_exited);
    for ch in TYPED_TEXT.chars() {
        if !send_and_wait(&tx, &ch.to_string(), TYPE_KEY_DELAY_MS, &nvim_exited) {
            phase_marker("NVIM_DIED_UNEXPECTEDLY");
            let _ = tx.send(Ctrl::Finish);
            return;
        }
    }
    send_and_wait(&tx, "<Esc>", BARRIER_DELAY_MS, &nvim_exited);
    phase_marker("TYPING_END");
    bail_if_dead!();

    // ---- workload: fast-scroll sub-phase ----
    let _ = tx.send(Ctrl::SetPhase(Phase::Scroll));
    for key in scroll_keys() {
        if !send_and_wait(&tx, key, SCROLL_KEY_DELAY_MS, &nvim_exited) {
            phase_marker("NVIM_DIED_UNEXPECTEDLY");
            let _ = tx.send(Ctrl::Finish);
            return;
        }
    }
    send_and_wait(&tx, "", BARRIER_DELAY_MS, &nvim_exited); // final settle, mirrors stock's closing barrier
    phase_marker("SCROLL_END");

    // ---- sanity: ask nvim itself to write final buffer/cursor state to a file we can read after
    // shutdown (avoids needing a direct nvim-rs async call cross-runtime -- see phase report) ----
    let sanity_cmd = format!(
        ":call writefile([string(line('$')), string(line('.'))], '{sanity_path}')<CR>"
    );
    send_and_wait(&tx, &sanity_cmd, 200, &nvim_exited);
    phase_marker("SANITY_DONE");

    phase_marker("SHUTDOWN_START");
    let _ = tx.send(Ctrl::Finish);
}

fn main() -> glib::ExitCode {
    let t_process_start = Instant::now();

    let workload_path = std::env::var("NEOVIBE_WORKLOAD_FILE").unwrap_or_else(|_| {
        "/tmp/neovibe_p11_scratch/workload.rs".to_string()
    });
    let sanity_path = std::env::var("NEOVIBE_SANITY_FILE").unwrap_or_else(|_| "/tmp/neovibe_p11_sanity.txt".to_string());
    let results_path = std::env::var("NEOVIBE_RESULTS_JSON").unwrap_or_else(|_| "/tmp/neovibe_p11_results.json".to_string());

    println!("NEOVIBE_TIMING:main_entry t={:.4}", t_process_start.elapsed().as_secs_f64());
    let app = Application::builder().application_id(APP_ID).build();
    println!("NEOVIBE_TIMING:app_built t={:.4}", t_process_start.elapsed().as_secs_f64());
    app.connect_activate(move |app| {
        println!("NEOVIBE_TIMING:activated t={:.4}", t_process_start.elapsed().as_secs_f64());
        let _ = std::io::stdout().flush();
        build_ui(app, t_process_start, workload_path.clone(), sanity_path.clone(), results_path.clone())
    });
    let _ = std::io::stdout().flush();
    app.run_with_args::<&str>(&[])
}

fn build_ui(
    app: &Application,
    t_process_start: Instant,
    workload_path: String,
    sanity_path: String,
    results_path: String,
) {
    let gl_area = GLArea::builder()
        .hexpand(true)
        .vexpand(true)
        .has_stencil_buffer(true)
        .auto_render(true)
        .focusable(false)
        .build();

    let window = ApplicationWindow::builder()
        .application(app)
        .title("neovibe P11 perf baseline (self-driving, no GUI input)")
        .default_width(1000)
        .default_height(700)
        .child(&gl_area)
        .build();

    let skia_state: Rc<RefCell<Option<SkiaState>>> = Rc::new(RefCell::new(None));
    let live_state: Rc<RefCell<LiveState>> = Rc::new(RefCell::new(LiveState::NotStarted));
    let phase: Rc<RefCell<Phase>> = Rc::new(RefCell::new(Phase::PreMeasurement));
    let frame_stats: Rc<RefCell<FrameStats>> = Rc::new(RefCell::new(FrameStats::default()));
    let redraw_count = Arc::new(AtomicU64::new(0));
    let nvim_exited = Arc::new(AtomicBool::new(false));
    let workload_started = Rc::new(RefCell::new(false));

    let (tx, rx): (Sender<Ctrl>, Receiver<Ctrl>) = std::sync::mpsc::channel();

    {
        let skia_state = skia_state.clone();
        gl_area.connect_resize(move |_widget, width, height| {
            let mut state = skia_state.borrow_mut();
            if let Some(state) = state.as_mut() {
                state.fb_width = width;
                state.fb_height = height;
                state.surface = None;
            }
        });
    }

    // --- render: identical LiveState lifecycle to main.rs, plus per-frame dt bucketing by phase
    // and a one-shot spawn of the background workload script the moment `is_ready()` first flips.
    {
        let skia_state = skia_state.clone();
        let live_state = live_state.clone();
        let phase = phase.clone();
        let frame_stats = frame_stats.clone();
        let redraw_count = redraw_count.clone();
        let nvim_exited = nvim_exited.clone();
        let workload_started = workload_started.clone();
        let tx = tx.clone();
        let workload_path = workload_path.clone();
        let sanity_path = sanity_path.clone();

        gl_area.connect_render(move |widget, _gl_ctx| {
            let mut state_slot = skia_state.borrow_mut();
            if state_slot.is_none() {
                let interface = make_gl_interface();
                let gr_context = direct_contexts::make_gl(interface, None)
                    .expect("failed to create Skia GL DirectContext");
                let width = widget.width() * widget.scale_factor();
                let height = widget.height() * widget.scale_factor();
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
            canvas.clear(OUTSIDE_COLOR);

            let mut live = live_state.borrow_mut();
            match &mut *live {
                LiveState::NotStarted => {
                    println!("NEOVIBE_TIMING:first_render_callback t={:.4}", t_process_start.elapsed().as_secs_f64());
                    let _ = std::io::stdout().flush();
                    fill_content_region(canvas, &content_region, STARTING_COLOR);
                    *live = LiveState::Starting;
                }
                LiveState::Starting => {
                    fill_content_region(canvas, &content_region, STARTING_COLOR);
                    let os_scale_factor = widget.scale_factor() as f64;
                    let options = LiveHarnessOptions {
                        os_scale_factor,
                        grid_size: Some(GridSize { width: GRID_WIDTH, height: GRID_HEIGHT }),
                        extra_nvim_args: vec!["--clean".to_string()],
                        ..Default::default()
                    };
                    println!("NEOVIBE_TIMING:before_with_options t={:.4}", t_process_start.elapsed().as_secs_f64());
                    let _ = std::io::stdout().flush();
                    match LiveHarness::with_options(options) {
                        Ok(harness) => {
                            let now = Instant::now();
                            println!("NEOVIBE_TIMING:after_with_options t={:.4}", t_process_start.elapsed().as_secs_f64());
                            let _ = std::io::stdout().flush();
                            *live = LiveState::Ready(Box::new(LiveSession {
                                harness,
                                last_frame: now,
                                frame_count: 0,
                                logged_ready: false,
                                last_animating: Cell::new(true),
                                last_seen_batches: Cell::new(0),
                                wants_frame: Cell::new(false),
                            }));
                        }
                        Err(err) => {
                            *live = LiveState::Failed(format!("{err:#}"));
                        }
                    }
                }
                LiveState::Ready(session) => {
                    let now = Instant::now();
                    let dt = (now - session.last_frame).as_secs_f32();
                    session.last_frame = now;
                    session.frame_count += 1;

                    let animating = session.harness.render_frame(canvas, Some(&content_region), dt);
                    // Share animation and redraw-batch state with the tick callback so it can request more frames.
                    session.last_animating.set(animating);
                    let batches_now = session.harness.redraw_batches_seen();
                    session.last_seen_batches.set(batches_now);

                    redraw_count.store(batches_now, Ordering::Relaxed);
                    nvim_exited.store(session.harness.has_neovim_exited(), Ordering::Relaxed);

                    match *phase.borrow() {
                        Phase::PreMeasurement => {}
                        Phase::Idle => frame_stats.borrow_mut().idle.push(dt),
                        Phase::Typing => frame_stats.borrow_mut().typing.push(dt),
                        Phase::Scroll => frame_stats.borrow_mut().scroll.push(dt),
                    }

                    if !session.logged_ready && session.harness.is_ready() {
                        session.logged_ready = true;
                        let t_ready_s = (now - t_process_start).as_secs_f64();
                        println!("NEOVIBE_READY t_ready_s={t_ready_s:.4}");
                        let _ = std::io::stdout().flush();

                        if !*workload_started.borrow() {
                            *workload_started.borrow_mut() = true;
                            let tx2 = tx.clone();
                            let nvim_exited2 = nvim_exited.clone();
                            let workload_path2 = workload_path.clone();
                            let sanity_path2 = sanity_path.clone();
                            thread::spawn(move || {
                                run_workload_script(tx2, nvim_exited2, workload_path2, sanity_path2);
                            });
                        }
                    }
                }
                LiveState::Failed(message) => {
                    fill_content_region(canvas, &content_region, FAILED_COLOR);
                    println!("NEOVIBE_FAILED: {message}");
                    let _ = std::io::stdout().flush();
                }
            }
            drop(live);

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

    // Call `queue_render()` only when another frame is needed, avoiding continuous idle rendering.
    // This binary has its own render loop, so keep its scheduling aligned with the interactive loop
    // for comparable measurements.
    {
        let live_state = live_state.clone();
        let gl_area_for_tick = gl_area.clone();
        let tick_stats = Rc::new(TickStats::new());
        gl_area.add_tick_callback(move |_widget, _clock| {
            let mut live = live_state.borrow_mut();
            let issued = match &mut *live {
                LiveState::Ready(session) => {
                    session.harness.pump(Duration::ZERO);
                    let batches_now = session.harness.redraw_batches_seen();
                    let new_content = batches_now != session.last_seen_batches.get();
                    if new_content {
                        session.last_seen_batches.set(batches_now);
                    }
                    let wants_frame = session.wants_frame.replace(false);
                    session.last_animating.get() || new_content || wants_frame
                }
                LiveState::NotStarted | LiveState::Starting | LiveState::Failed(_) => true,
            };
            drop(live);

            if issued {
                gl_area_for_tick.queue_render();
            }
            tick_stats.record(issued);

            glib::ControlFlow::Continue
        });
    }

    // --- poll the background thread's control-message queue on the GTK thread every 5ms (well
    // under the 20ms input cadence) -- this is the only thread allowed to touch `live_state`. ---
    {
        let live_state = live_state.clone();
        let phase = phase.clone();
        let frame_stats = frame_stats.clone();
        let redraw_count = redraw_count.clone();
        let app = app.clone();
        let results_path = results_path.clone();
        let sanity_path = sanity_path.clone();
        glib::timeout_add_local(Duration::from_millis(5), move || {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    Ctrl::Input(text) => {
                        let mut live = live_state.borrow_mut();
                        if let LiveState::Ready(session) = &mut *live {
                            if !session.harness.has_neovim_exited() {
                                session.harness.send_text_input(&text);
                                // Request a frame for workload input on the next tick, just as for a keypress.
                                session.wants_frame.set(true);
                            }
                        }
                    }
                    Ctrl::SetPhase(p) => {
                        *phase.borrow_mut() = p;
                        // Post-idle-render-fix measurement artifact, found and fixed during the
                        // idle-render-fix follow-up phase (kept as a comment, not silently
                        // squashed -- same policy PHASE_REPORT.md's own deviation #5 used): once
                        // the tick callback can legitimately skip *every* frame for an entire
                        // phase (e.g. all 13s of Idle now that the fix works), `last_frame` goes
                        // stale for the whole gap, and the next phase's first real render computes
                        // a `dt` covering that whole gap instead of one true frame interval --
                        // polluting that phase's frame-timing bucket with one multi-second outlier
                        // (observed: a >14s "frame" landing in `typing_frame_stats` immediately
                        // after a 13s idle window). Resetting the clock at the phase boundary
                        // itself (before any render has to decide what dt to report) keeps each
                        // bucket's samples to genuine inter-frame intervals within that phase.
                        if let LiveState::Ready(session) = &mut *live_state.borrow_mut() {
                            session.last_frame = Instant::now();
                        }
                    }
                    Ctrl::Finish => {
                        let mut live = live_state.borrow_mut();
                        let mut shutdown_ok = false;
                        if let LiveState::Ready(session) = &mut *live {
                            shutdown_ok = session.harness.shutdown();
                        }
                        drop(live);

                        let stats = frame_stats.borrow();
                        let total_redraws = redraw_count.load(Ordering::Relaxed);
                        let json = format!(
                            "{{\"shutdown_ok\":{shutdown_ok},\"total_redraw_batches_seen\":{total_redraws},\"idle_frame_stats\":{},\"typing_frame_stats\":{},\"scroll_frame_stats\":{}}}",
                            stats_json(&stats.idle),
                            stats_json(&stats.typing),
                            stats_json(&stats.scroll),
                        );
                        println!("NEOVIBE_RESULT_JSON:{json}");
                        let _ = std::io::stdout().flush();
                        if let Err(e) = std::fs::write(&results_path, &json) {
                            eprintln!("failed to write results json to {results_path}: {e}");
                        }
                        let _ = sanity_path; // read back by the external Python driver after exit
                        phase_marker("SHUTDOWN_DONE");
                        app.quit();
                        return glib::ControlFlow::Break;
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    }

    println!("NEOVIBE_TIMING:before_present t={:.4}", t_process_start.elapsed().as_secs_f64());
    window.present();
    println!("NEOVIBE_TIMING:after_present t={:.4}", t_process_start.elapsed().as_secs_f64());
    let _ = std::io::stdout().flush();
}
