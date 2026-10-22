//! Two realized editor panes in one window, both fed by the one event loop their harnesses share.
//!
//! A pump through either pane's harness applies every pane's pending redraw batches, and leaves the
//! shared loop's fd drained. A pane whose batches were applied that way has new content and nothing
//! that tells it so, unless whatever watches the fd also hears about pumps made outside the watch.
//! These cases hold that, with real `nvim --embed --clean` children drawn into real `GtkGLArea`s:
//!
//! - `one_watch_wakes_a_pane_whose_batches_another_panes_pump_applied`: B's nvim redraws while the
//!   main thread is blocked, so B's batch waits in the shared loop; A is queued to draw, and A's draw
//!   (GDK's paint, which outranks any fd watch) pumps B's batch through A's harness before any watch
//!   is dispatched. B must still present a new frame.
//! - `releasing_one_pane_keeps_the_watch_for_the_others`: A's nvim quits and the host releases A;
//!   B's redraws still reach B's frames, though nothing but the watch pumps any more.
//!
//! Each case runs in a child copy of this binary on its own `Xvfb` (GTK, winit and the clipboard all
//! connect to a display); the parent connects to nothing. Ignored unless asked for, because it spawns
//! real nvims:
//!
//!     cargo test -p neovide-editor --test shared_watch -- --ignored
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use neovide_editor::NeovideEditorPane;

#[path = "../../shell/tests/support/own_x_server.rs"]
mod own_x_server;

const CASE_ENV: &str = "EITRI_SHARED_WATCH_CASE";
const CASES: &[&str] = &[
    "one_watch_wakes_a_pane_whose_batches_another_panes_pump_applied",
    "releasing_one_pane_keeps_the_watch_for_the_others",
];

/// Frames a pane has handed to GTK, by either presentation path.
fn frames(pane: &NeovideEditorPane) -> u64 {
    let counts = pane.presentation_counts();
    counts.own_frames + counts.fallback_frames
}

/// Runs the default main context without blocking until `done` holds; `false` at `limit`.
fn run_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let context = gtk4::glib::MainContext::default();
    let deadline = Instant::now() + limit;
    loop {
        while context.iteration(false) {}
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Runs the main context until neither pane has presented a frame for `quiet`, so every tick has
/// stopped and only a wake can make either draw again.
fn settle(panes: &[&NeovideEditorPane], quiet: Duration) {
    let mut last: Vec<u64> = panes.iter().map(|p| frames(p)).collect();
    let mut since = Instant::now();
    let settled = run_until(Duration::from_secs(20), || {
        let now: Vec<u64> = panes.iter().map(|p| frames(p)).collect();
        if now != last {
            last = now;
            since = Instant::now();
        }
        since.elapsed() >= quiet
    });
    assert!(settled, "the panes never stopped drawing (frames {last:?})");
}

/// A window holding two editor panes side by side, both with a ready nvim that has drawn.
fn two_panes() -> (gtk4::Window, Rc<NeovideEditorPane>, Rc<NeovideEditorPane>) {
    let a = Rc::new(NeovideEditorPane::new(true));
    let b = Rc::new(NeovideEditorPane::new(true));
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    row.set_homogeneous(true);
    for pane in [&a, &b] {
        pane.widget().set_size_request(400, 300);
        pane.widget().set_hexpand(true);
        pane.widget().set_vexpand(true);
        row.append(pane.widget());
    }
    let window = gtk4::Window::new();
    window.set_default_size(800, 300);
    window.set_child(Some(&row));
    window.present();
    let ready = run_until(Duration::from_secs(30), || a.is_running() && b.is_running());
    assert!(
        ready,
        "both nvims should start (A running: {}, B running: {})",
        a.is_running(),
        b.is_running()
    );
    settle(&[&a, &b], Duration::from_millis(800));
    (window, a, b)
}

fn one_watch_wakes_a_pane_whose_batches_another_panes_pump_applied() {
    let (_window, a, b) = two_panes();
    let (a_before, b_before) = (frames(&a), frames(&b));

    b.send_keys("ibbb<Esc>");
    a.widget().queue_draw();
    // Nothing on the main context runs while this thread sleeps: B's nvim answers, its batch waits
    // in the shared loop, and A's queued paint becomes due. GDK's frame clock (priorities 1 and 120)
    // then runs before any source at a lower priority, so A's render pumps B's batch into B's
    // renderer before a watch on the shared fd could be dispatched.
    std::thread::sleep(Duration::from_millis(600));

    let woke = run_until(Duration::from_secs(2), || frames(&b) > b_before);
    println!(
        "A frames {a_before} -> {}, B frames {b_before} -> {}",
        frames(&a),
        frames(&b)
    );
    assert!(
        frames(&a) > a_before,
        "A was queued to draw and did not: the case did not happen"
    );
    assert!(
        woke,
        "B's batch was applied through A's pump and B never presented it within 2 s: B was left stale"
    );
    assert_eq!(
        neovide_editor::event_loop_watch_priority(),
        Some(neovide_editor::fd_watch::EVENT_LOOP_WATCH_PRIORITY),
        "one watch on the shared fd, at the priority that leaves paints their rate"
    );

    assert!(a.shutdown(), "A's nvim should exit when shut down");
    assert!(b.shutdown(), "B's nvim should exit when shut down");
    assert_eq!(
        neovide_editor::event_loop_watch_priority(),
        None,
        "with no live pane left, nothing watches the shared fd"
    );
}

fn releasing_one_pane_keeps_the_watch_for_the_others() {
    let (_window, a, b) = two_panes();
    let a_exited = Rc::new(Cell::new(false));
    {
        let a_exited = a_exited.clone();
        a.on_exited_unrequested(move || a_exited.set(true));
    }

    a.send_keys(":q!<CR>");
    assert!(
        run_until(Duration::from_secs(10), || a_exited.get()),
        "A's nvim should report its own exit"
    );
    assert!(a.release_exited(), "A's exited nvim should be released");
    assert!(b.is_running(), "B's nvim must not be ended by A's");
    assert_eq!(
        neovide_editor::event_loop_watch_priority(),
        Some(neovide_editor::fd_watch::EVENT_LOOP_WATCH_PRIORITY),
        "B still needs the watch after A was released"
    );
    settle(&[&b], Duration::from_millis(800));

    // A is released, so nothing pumps the shared loop for B but the watch.
    let b_before = frames(&b);
    b.send_keys("ibbb<Esc>");
    let woke = run_until(Duration::from_secs(5), || frames(&b) > b_before);
    println!("B frames {b_before} -> {}", frames(&b));
    assert!(woke, "B's redraw never reached B's frames after A was released");

    assert!(b.shutdown(), "B's nvim should exit when shut down");
}

fn main() {
    if let Some(case) = std::env::var_os(CASE_ENV) {
        if let Ok(Some(kind)) = neovide_editor::detach_stdin_from_nvim() {
            println!("[stdin] a {kind} on stdin would reach nvim as a buffer; stdin is /dev/null now");
        }
        let _server = own_x_server::init_gtk("shared_watch", "1024x768x24");
        match case.to_str() {
            Some("one_watch_wakes_a_pane_whose_batches_another_panes_pump_applied") => {
                one_watch_wakes_a_pane_whose_batches_another_panes_pump_applied()
            }
            Some("releasing_one_pane_keeps_the_watch_for_the_others") => {
                releasing_one_pane_keeps_the_watch_for_the_others()
            }
            other => panic!("unknown case {other:?}"),
        }
        println!("RESULT pass");
        return;
    }
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("shared_watch: ignored (spawns real nvims); run with `-- --ignored`");
        return;
    }
    let mut failed = Vec::new();
    for case in CASES {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .env(CASE_ENV, case)
            .output()
            .expect("re-exec of the test binary failed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let passed = stdout.lines().any(|l| l == "RESULT pass");
        println!("--- {case}: {}", if passed { "pass" } else { "FAIL" });
        if !passed {
            println!("{stdout}");
            println!("{}", String::from_utf8_lossy(&out.stderr));
            failed.push(*case);
        }
    }
    assert!(failed.is_empty(), "shared_watch cases failed: {failed:?}");
}
