//! GTK pacing probe; run only in the isolated GUI sandbox, with a release build.
//! `cargo run --release -p neovide-editor --example frame_pacing -- [--user-config]`
//! Optional NEOVIBE_PACING_WIDTH/HEIGHT override the 1200x800 logical-pixel window.
//! after_paint is GTK completion, not presentation; entry-to-after_paint is an upper bound
//! on render callback wall time, not GPU time or input-to-photon latency. No pixel readback.
//! send_keys bypasses GDK/IME and queues a redraw; this measures drawing, not physical input.
//! Compare revisions with the same output size, refresh, scaling and background CPU/GPU load.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gtk4::glib::{self, translate::IntoGlib, ControlFlow};
use gtk4::prelude::*;
use neovide_editor::{NeovideEditorPane, NeovideEditorPaneOptions};

#[derive(Default)]
struct Samples {
    entries: Vec<Instant>,
    paints: Vec<Instant>,
    render_upper_ms: Vec<f64>,
    input_late_ms: Vec<f64>,
}

#[derive(Default)]
struct Probe {
    phase: Cell<Option<usize>>,
    completed_phases: Cell<usize>,
    pending: Cell<Option<(usize, Instant)>>,
    total_frames: Cell<u64>,
    samples: RefCell<[Samples; 2]>,
}

struct HookData {
    area: usize,
    probe: Rc<Probe>,
}

unsafe extern "C" fn render_entry(
    _: *mut glib::gobject_ffi::GSignalInvocationHint,
    count: u32,
    values: *const glib::gobject_ffi::GValue,
    data: *mut c_void,
) -> glib::ffi::gboolean {
    // GTK invokes this on its main thread; removal releases the boxed hook data.
    let hook = unsafe { &*(data as *const HookData) };
    if count > 0 && unsafe { glib::gobject_ffi::g_value_get_object(values) } as usize == hook.area {
        let now = Instant::now();
        hook.probe.total_frames.set(hook.probe.total_frames.get() + 1);
        if let Some(phase) = hook.probe.phase.get() {
            hook.probe.samples.borrow_mut()[phase].entries.push(now);
            hook.probe.pending.set(Some((phase, now)));
        }
    }
    1
}

unsafe extern "C" fn destroy_hook(data: *mut c_void) {
    drop(unsafe { Box::from_raw(data as *mut HookData) });
}

fn start_phase(phase: usize, pane: Rc<NeovideEditorPane>, probe: Rc<Probe>, main_loop: glib::MainLoop) {
    let interval = Duration::from_millis(if phase == 0 { 35 } else { 20 });
    let started = Instant::now();
    let mut due = started + interval;
    let mut input = 0usize;
    probe.phase.set(Some(phase));
    glib::timeout_add_local(interval, move || {
        let now = Instant::now();
        if now.duration_since(started) >= Duration::from_secs(8) {
            probe.phase.set(None);
            probe.completed_phases.set(phase + 1);
            if phase == 0 {
                start_phase(1, pane.clone(), probe.clone(), main_loop.clone());
            } else {
                main_loop.quit();
            }
            return ControlFlow::Break;
        }
        probe.samples.borrow_mut()[phase]
            .input_late_ms
            .push(now.saturating_duration_since(due).as_secs_f64() * 1000.0);
        // Match GLib's repeating-timer semantics: record dispatch lateness, never catch up in bursts.
        due = now + interval;
        pane.send_keys(if phase == 0 {
            ["h", "j", "l", "k"][input % 4]
        } else {
            "j"
        });
        input += 1;
        ControlFlow::Continue
    });
}

fn distribution(label: &str, mut values: Vec<f64>) {
    values.sort_by(f64::total_cmp);
    if values.is_empty() {
        println!("  {label}: n=0");
        return;
    }
    let percentile = |p: f64| values[((values.len() as f64 * p).ceil() as usize).saturating_sub(1)];
    println!(
        "  {label}: n={} p50={:.3} p95={:.3} p99={:.3} max={:.3} ms",
        values.len(),
        percentile(0.50),
        percentile(0.95),
        percentile(0.99),
        values[values.len() - 1]
    );
}

fn intervals(times: &[Instant]) -> Vec<f64> {
    times
        .windows(2)
        .map(|pair| pair[1].duration_since(pair[0]).as_secs_f64() * 1000.0)
        .collect()
}

fn main() {
    gtk4::init().expect("run inside the isolated GUI sandbox");
    let user_config = std::env::args().any(|arg| arg == "--user-config");
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let scratch = std::env::temp_dir().join(format!("neovibe-pacing-{}-{stamp}", std::process::id()));
    std::fs::create_dir(&scratch).expect("create isolated nvim directory");
    let ready = scratch.join("ready");
    let init = concat!(
        "lua vim.o.wrap=false; vim.o.guicursor='a:block-blinkon0'; ",
        "local lines={}; for i=1,500 do lines[i]=string.format('%04d ',i)..string.rep('frame pacing text ',12) end; ",
        "vim.api.nvim_buf_set_lines(0,0,-1,false,lines); vim.api.nvim_win_set_cursor(0,{10,10}); ",
        "vim.fn.writefile({'ready'},vim.env.NEOVIBE_PACING_READY)"
    );
    let pane = Rc::new(NeovideEditorPane::with_options(NeovideEditorPaneOptions {
        clean: !user_config,
        cwd: Some(scratch.clone()),
        child_env: vec![("NEOVIBE_PACING_READY".into(), ready.to_string_lossy().into_owned())],
        extra_nvim_args: vec!["-i".into(), "NONE".into(), "-c".into(), init.into()],
    }));
    let size = |name, fallback| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<i32>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(fallback)
    };
    let (width, height) = (size("NEOVIBE_PACING_WIDTH", 1200), size("NEOVIBE_PACING_HEIGHT", 800));
    let window = gtk4::Window::builder()
        .title("Neovibe frame pacing probe")
        .default_width(width)
        .default_height(height)
        .child(pane.widget())
        .build();
    let main_loop = glib::MainLoop::new(None, false);
    let probe = Rc::new(Probe::default());
    let signal =
        unsafe { glib::gobject_ffi::g_signal_lookup(c"render".as_ptr(), gtk4::GLArea::static_type().into_glib()) };
    let hook_data = Box::into_raw(Box::new(HookData {
        area: pane.widget().as_ptr() as usize,
        probe: probe.clone(),
    }));
    let hook = unsafe {
        glib::gobject_ffi::g_signal_add_emission_hook(
            signal,
            0,
            Some(render_entry),
            hook_data.cast(),
            Some(destroy_hook),
        )
    };
    assert_ne!(hook, 0, "cannot observe GtkGLArea render signal");
    window.present();
    println!(
        "GSK renderer: {:?}",
        window.renderer().map(|r| r.type_().name().to_owned())
    );
    pane.grab_focus();
    pane.set_focused(true);
    let clock = pane.widget().frame_clock().expect("realized frame clock");
    let capture = probe.clone();
    let observer = clock.connect_after_paint(move |_| {
        if let Some((phase, entered)) = capture.pending.take() {
            let now = Instant::now();
            let mut samples = capture.samples.borrow_mut();
            samples[phase].paints.push(now);
            samples[phase]
                .render_upper_ms
                .push(now.duration_since(entered).as_secs_f64() * 1000.0);
        }
    });
    let run_loop = main_loop.clone();
    let run_pane = pane.clone();
    let run_probe = probe.clone();
    let mut warmup = None;
    glib::timeout_add_local(Duration::from_millis(20), move || {
        if ready.exists() && run_pane.cell_size().is_some() {
            let since = warmup.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_millis(300) {
                start_phase(0, run_pane.clone(), run_probe.clone(), run_loop.clone());
                return ControlFlow::Break;
            }
        }
        ControlFlow::Continue
    });
    let timed_out = Rc::new(Cell::new(false));
    let expired = timed_out.clone();
    let deadline_loop = main_loop.clone();
    glib::timeout_add_local_once(Duration::from_secs(40), move || {
        expired.set(true);
        deadline_loop.quit();
    });
    let close_loop = main_loop.clone();
    window.connect_close_request(move |_| {
        close_loop.quit();
        glib::Propagation::Proceed
    });
    pane.on_exited_unrequested({
        let main_loop = main_loop.clone();
        move || main_loop.quit()
    });
    main_loop.run();
    clock.disconnect(observer);
    unsafe { glib::gobject_ffi::g_signal_remove_emission_hook(signal, hook) };
    let shutdown = pane.shutdown();
    let physical_width = pane.widget().width() * pane.widget().scale_factor();
    let physical_height = pane.widget().height() * pane.widget().scale_factor();
    window.close();
    println!("physical framebuffer: {physical_width}x{physical_height}");
    println!("frame_pacing: config={} size={width}x{height} total_frames={} completed_phases={} timed_out={} shutdown={shutdown}",
        if user_config { "user" } else { "clean" }, probe.total_frames.get(), probe.completed_phases.get(), timed_out.get());
    for (name, samples) in ["hjkl / 35ms / 8s", "j scroll / 20ms / 8s"]
        .iter()
        .zip(probe.samples.borrow().iter())
    {
        println!(
            "{name}: render_entries={} completed_frames={} inputs={}",
            samples.entries.len(),
            samples.paints.len(),
            samples.input_late_ms.len()
        );
        distribution("render entry intervals", intervals(&samples.entries));
        distribution("after_paint intervals", intervals(&samples.paints));
        distribution("entry-to-after_paint upper bound", samples.render_upper_ms.clone());
        distribution("input timer lateness", samples.input_late_ms.clone());
    }
    let _ = std::fs::remove_dir_all(scratch);
    assert!(!timed_out.get(), "frame pacing probe timed out");
    assert_eq!(probe.completed_phases.get(), 2, "frame pacing probe ended early");
    assert!(shutdown, "nvim did not acknowledge shutdown");
}
