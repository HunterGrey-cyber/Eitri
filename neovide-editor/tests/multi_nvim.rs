//! Several `LiveHarness` editors in one process, and a fresh one after an nvim quit.
//!
//! The fork's harnesses share one headless winit event loop per thread and each owns a route, so a
//! process can hold two live editors at once and start a third after one of them quit. This checks
//! both with real `nvim --embed --clean` children painted into CPU raster Skia surfaces (no window,
//! no GTK): each editor's keys move only its own cursor, a pump through one harness delivers the
//! other's redraws to the other, one nvim's `:q!` ends only that editor, and a harness built after
//! that works.
//!
//! The scenario runs in a child copy of this binary on its own `Xvfb` (winit and the clipboard
//! connect to a display even though no window is made); the parent connects to nothing. Ignored
//! unless asked for, because it spawns real nvims:
//!
//!     cargo test -p neovide-editor --test multi_nvim -- --ignored
use std::time::{Duration, Instant};

use neovide::{
    live_harness::{LiveHarness, LiveHarnessOptions},
    units::{GridSize, PixelRect},
};
use skia_safe::{surfaces, Surface};

#[path = "../../shell/tests/support/own_x_server.rs"]
mod own_x_server;

const CANVAS: (i32, i32) = (640, 320);
const FRAME_DT: f32 = 1.0 / 60.0;
const CASE_ENV: &str = "EITRI_MULTI_NVIM_CASE";

struct Editor {
    name: &'static str,
    harness: LiveHarness,
    surface: Surface,
}

fn region() -> PixelRect<f32> {
    PixelRect::from_min_max((0.0, 0.0), (CANVAS.0 as f32, CANVAS.1 as f32))
}

fn launch(name: &'static str) -> Editor {
    let harness = LiveHarness::with_options(LiveHarnessOptions {
        grid_size: Some(GridSize::new(40u32, 8u32)),
        extra_nvim_args: vec!["--clean".to_string()],
        minimum_nvim_version: Some(neovide_editor::NVIM_FLOOR),
        ..Default::default()
    })
    .unwrap_or_else(|e| panic!("{name}: LiveHarness::with_options failed: {e:#}"));
    let surface = surfaces::raster_n32_premul(CANVAS).expect("raster surface");
    println!("{name}: launched, event loop fd {}", harness.event_loop_fd());
    Editor { name, harness, surface }
}

fn frame(editor: &mut Editor) {
    let region = region();
    editor
        .harness
        .render_frame(editor.surface.canvas(), Some(&region), FRAME_DT);
}

/// The cursor's destination column, from the renderer's own cursor state (set by the redraw
/// batches this editor received).
fn cursor_col(editor: &Editor) -> i32 {
    let x = editor.harness.cursor_pixel_position().x;
    (x / editor.harness.grid_scale().width()).round() as i32
}

/// Renders every editor in turn until `done` holds, failing after 15 s.
fn wait(editors: &mut [&mut Editor], what: &str, done: impl Fn(&[&mut Editor]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        for editor in editors.iter_mut() {
            frame(editor);
        }
        if done(editors) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A short settle with every editor rendered, so a misrouted batch would have been applied.
fn settle(editors: &mut [&mut Editor]) {
    for _ in 0..30 {
        for editor in editors.iter_mut() {
            frame(editor);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn report(editor: &Editor) {
    println!(
        "{}: col={} batches={} ready={} exited={}",
        editor.name,
        cursor_col(editor),
        editor.harness.redraw_batches_seen(),
        editor.harness.is_ready(),
        editor.harness.has_neovim_exited()
    );
}

fn scenario() {
    // (a) Two editors at once, each with its own nvim, on one shared loop.
    let mut a = launch("A");
    let mut b = launch("B");
    assert_eq!(
        a.harness.event_loop_fd(),
        b.harness.event_loop_fd(),
        "the loop is shared"
    );
    wait(&mut [&mut a, &mut b], "A and B ready", |e| {
        e.iter().all(|e| e.harness.is_ready())
    });
    settle(&mut [&mut a, &mut b]);
    report(&a);
    report(&b);
    assert_eq!((cursor_col(&a), cursor_col(&b)), (0, 0));

    // A's keys, pumped only through B: A's redraws must still reach A, and none reach B.
    let (a0, b0) = (a.harness.redraw_batches_seen(), b.harness.redraw_batches_seen());
    a.harness.send_text_input("iaaaaaaaaa<Esc>");
    let deadline = Instant::now() + Duration::from_secs(15);
    while a.harness.redraw_batches_seen() == a0 {
        assert!(Instant::now() < deadline, "A's redraws never arrived through B's pump");
        frame(&mut b);
        std::thread::sleep(Duration::from_millis(10));
    }
    println!(
        "A: {} batch(es) applied through B's pump",
        a.harness.redraw_batches_seen() - a0
    );
    wait(&mut [&mut a, &mut b], "A's cursor at col 8", |e| cursor_col(e[0]) == 8);
    settle(&mut [&mut a, &mut b]);
    report(&a);
    report(&b);
    assert_eq!(b.harness.redraw_batches_seen(), b0, "B received a redraw for A's keys");
    assert_eq!(cursor_col(&b), 0);

    // B's keys move only B.
    let a1 = a.harness.redraw_batches_seen();
    b.harness.send_text_input("ibbb<Esc>");
    wait(&mut [&mut a, &mut b], "B's cursor at col 2", |e| cursor_col(e[1]) == 2);
    settle(&mut [&mut a, &mut b]);
    report(&a);
    report(&b);
    assert_eq!(a.harness.redraw_batches_seen(), a1, "A received a redraw for B's keys");
    assert_eq!(cursor_col(&a), 8);

    // (b) A's nvim quits: only A sees its exit; B keeps working.
    a.harness.send_text_input(":q!<CR>");
    wait(&mut [&mut a, &mut b], "A's nvim exited", |e| {
        e[0].harness.has_neovim_exited()
    });
    settle(&mut [&mut a, &mut b]);
    report(&a);
    report(&b);
    assert!(!b.harness.has_neovim_exited(), "B was told A's nvim exited");
    b.harness.send_text_input("Abb<Esc>");
    wait(&mut [&mut b], "B's cursor at col 4 after A quit", |e| {
        cursor_col(e[0]) == 4
    });
    assert!(a.harness.shutdown(), "A's shutdown did not observe its exit");
    drop(a);
    println!("A: shut down and dropped");

    // A fresh harness and nvim after the first one's exited, beside the still-running B.
    let mut c = launch("C");
    assert_eq!(
        c.harness.event_loop_fd(),
        b.harness.event_loop_fd(),
        "C joined the same loop"
    );
    wait(&mut [&mut b, &mut c], "C ready", |e| e[1].harness.is_ready());
    let b2 = b.harness.redraw_batches_seen();
    c.harness.send_text_input("icccccc<Esc>");
    wait(&mut [&mut b, &mut c], "C's cursor at col 5", |e| cursor_col(e[1]) == 5);
    settle(&mut [&mut b, &mut c]);
    report(&b);
    report(&c);
    assert_eq!(b.harness.redraw_batches_seen(), b2, "B received a redraw for C's keys");
    assert_eq!(cursor_col(&b), 4);

    // Both end cleanly, each observing its own exit.
    assert!(b.harness.shutdown(), "B's shutdown did not observe its exit");
    assert!(c.harness.shutdown(), "C's shutdown did not observe its exit");
    println!("B, C: shut down");
}

fn main() {
    if std::env::var_os(CASE_ENV).is_some() {
        if let Ok(Some(kind)) = neovide_editor::detach_stdin_from_nvim() {
            println!("[stdin] a {kind} on stdin would reach nvim as a buffer; stdin is /dev/null now");
        }
        let _server = own_x_server::isolate("multi_nvim", "640x480x24");
        scenario();
        println!("RESULT pass");
        return;
    }
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("multi_nvim: ignored (spawns real nvims); run with `-- --ignored`");
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .env(CASE_ENV, "1")
        .output()
        .expect("re-exec of the test binary failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    println!("{stdout}");
    if !stdout.lines().any(|l| l == "RESULT pass") {
        panic!(
            "multi_nvim failed ({})\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
