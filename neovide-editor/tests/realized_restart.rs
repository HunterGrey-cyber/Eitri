//! An editor pane whose nvim ended starts a new one on the same widget.
//!
//! A host that keeps a window open after the editor's nvim quit can bring the editor back without
//! building a second `GtkGLArea` or moving the one it has: the pane goes back to its first-start
//! path (placeholder frame, then the blocking launch) with the options the restart names. These
//! cases hold that with real `nvim --embed --clean` children drawn into a realized `GtkGLArea`:
//!
//! - `a_realized_pane_restarts_on_its_own_widget`: nvim quits, the host releases the pane and,
//!   once it has stopped drawing, restarts it; the same widget, never unrealized, shows a new nvim that takes keys and was
//!   started with the restart's options, and the host hears once that its session is ready.
//! - `restart_is_refused_unless_the_pane_exited`: a pane whose nvim is running refuses, and names
//!   the state it is in.
//! - `a_restarted_pane_hears_nothing_from_its_ended_harness`: the release and the restart happen in
//!   the exit report itself, while the ended harness may still have events on the event loop every
//!   harness shares; the new session is never told it exited, and the exit report is not repeated.
//!   That the new session draws nothing of the old one is not observed here: it holds by
//!   construction, since the shared loop hands each harness a route id it never hands out again and
//!   a dropped harness's route is gone with it. Nor does this case notice a restart that forgets to
//!   ask for a frame (its restart runs while the tick is still going); the first case does.
//! - `a_restarted_pane_reports_its_new_nvims_fullscreen_setting`: the ended nvim set
//!   `g:neovide_fullscreen`, and the host, which followed it, hears the new nvim's own value.
//! - `an_ending_timer_leaves_the_restarted_nvim_alone`: nvim is ended while a watched request is
//!   outstanding (SIGTERM now, its stdin closed a second later); it exits at once and the pane is
//!   restarted inside that second, and the late hang-up does not reach the new nvim.
//! - `a_pane_shut_down_while_restarting_leaves_no_nvim`: the window closes after the restart was
//!   asked and before the new nvim started; the pane's shutdown leaves no nvim of this process
//!   running, and no later frame starts one.
//! - `a_pane_whose_launch_failed_starts_again`: a restart whose nvim cannot be launched leaves the
//!   pane failed, the host told why once; a later restart of that failed pane starts a new nvim on
//!   the same widget, and the host hears once that its session is ready.
//!
//! Each case runs in a child copy of this binary on its own `Xvfb` (GTK, winit and the clipboard all
//! connect to a display); the parent connects to nothing. Ignored unless asked for, because it spawns
//! real nvims:
//!
//!     cargo test -p neovide-editor --features test-support --test realized_restart -- --ignored
use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use neovide_editor::{NeovideEditorPane, NeovideEditorPaneOptions};

#[path = "../../shell/tests/support/own_x_server.rs"]
mod own_x_server;

const CASE_ENV: &str = "EITRI_REALIZED_RESTART_CASE";
const CASES: &[&str] = &[
    "a_realized_pane_restarts_on_its_own_widget",
    "restart_is_refused_unless_the_pane_exited",
    "a_restarted_pane_hears_nothing_from_its_ended_harness",
    "a_restarted_pane_reports_its_new_nvims_fullscreen_setting",
    "an_ending_timer_leaves_the_restarted_nvim_alone",
    "a_pane_shut_down_while_restarting_leaves_no_nvim",
    "a_pane_whose_launch_failed_starts_again",
];

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

/// Runs the main context until the pane has presented no frame for `quiet`, so its tick has
/// stopped and only something that asks for a frame makes it draw again.
fn settle(pane: &NeovideEditorPane, _quiet: Duration) {
    assert!(
        run_until(Duration::from_secs(60), || pane.testing_idle()),
        "pane never became idle"
    );
}

fn turns(mut sample: impl FnMut()) {
    for _ in 0..100 {
        glib::MainContext::default().iteration(false);
        sample();
    }
}

/// A shown window holding one editor pane whose nvim is running, and a count of the pane widget's
/// unrealizes from then on.
fn realized_pane() -> (gtk4::Window, Rc<NeovideEditorPane>, Rc<Cell<u32>>) {
    let pane = Rc::new(NeovideEditorPane::new(true));
    pane.widget().set_size_request(640, 360);
    let window = gtk4::Window::new();
    window.set_default_size(640, 360);
    window.set_child(Some(pane.widget()));
    window.present();
    assert!(
        run_until(Duration::from_secs(30), || pane.is_running()),
        "the first nvim should start"
    );
    assert!(pane.widget().is_realized(), "the pane's widget should be realized");
    let unrealized = Rc::new(Cell::new(0));
    {
        let unrealized = unrealized.clone();
        pane.widget()
            .connect_unrealize(move |_| unrealized.set(unrealized.get() + 1));
    }
    (window, pane, unrealized)
}

/// Counts the pane's unrequested-exit reports.
fn count_exits(pane: &NeovideEditorPane) -> Rc<Cell<u32>> {
    let exits = Rc::new(Cell::new(0));
    let counter = exits.clone();
    pane.on_exited_unrequested(move || counter.set(counter.get() + 1));
    exits
}

/// Options for the second nvim: `--clean`, plus a variable only a spawn with these options sets.
fn second_options() -> NeovideEditorPaneOptions {
    NeovideEditorPaneOptions {
        clean: true,
        extra_nvim_args: vec!["--cmd".into(), "let g:restart_marker = 'second'".into()],
        ..Default::default()
    }
}

/// Asks nvim for `code`'s answer, waiting up to `limit` with the main context running.
fn ask(pane: &NeovideEditorPane, code: &'static str, limit: Duration) -> Option<Result<rmpv::Value, String>> {
    let slot: Arc<Mutex<Option<Result<rmpv::Value, String>>>> = Arc::new(Mutex::new(None));
    let theirs = slot.clone();
    if !pane.exec_lua(
        code,
        vec![],
        Box::new(move |answer| *theirs.lock().unwrap() = Some(answer)),
    ) {
        return None;
    }
    run_until(limit, || slot.lock().unwrap().is_some());
    let mut answer = slot.lock().unwrap();
    answer.take()
}

/// The `nvim` processes descending from this one that still run, by pid. A zombie (ended, waiting
/// for its parent to reap it) runs nothing and is not one.
fn running_nvims() -> Vec<u32> {
    let mut procs = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // `pid (comm) state ppid ...`: the name may hold spaces and parentheses, so it ends at the
        // last `)`.
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let name = stat[open + 1..close].to_string();
        let mut rest = stat[close + 1..].split_whitespace();
        let state = rest.next().unwrap_or_default().to_string();
        let Some(parent) = rest.next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        procs.push((pid, parent, name, state));
    }
    let mut ours = vec![std::process::id()];
    let mut nvims = Vec::new();
    let mut next = 0;
    while next < ours.len() {
        let parent = ours[next];
        next += 1;
        for (pid, ppid, name, state) in &procs {
            if *ppid == parent && !ours.contains(pid) {
                ours.push(*pid);
                if name == "nvim" && state != "Z" {
                    nvims.push(*pid);
                }
            }
        }
    }
    nvims
}

fn a_realized_pane_restarts_on_its_own_widget() {
    let (_window, pane, unrealized) = realized_pane();
    let exits = count_exits(&pane);
    let widget_before = pane.widget().as_ptr();

    pane.send_keys(":q!<CR>");
    assert!(
        run_until(Duration::from_secs(10), || exits.get() == 1),
        "the first nvim should report its own exit"
    );
    assert!(pane.release_exited(), "the exited nvim should be released");
    assert!(!pane.is_ready(), "a released pane has no nvim");
    // A host restarts whenever the user asks, long after the released pane stopped drawing.
    settle(&pane, Duration::from_millis(800));
    let readies = Rc::new(Cell::new(0u32));
    {
        let readies = readies.clone();
        pane.on_session_ready(move || readies.set(readies.get() + 1));
    }

    assert!(!pane.is_starting(), "a released pane is not starting");
    pane.restart(second_options())
        .expect("a released pane whose nvim ended should accept a restart");
    assert!(
        pane.is_starting(),
        "a restarted pane is starting until its nvim is launched"
    );
    assert!(
        run_until(Duration::from_secs(10), || pane.is_ready()),
        "the restarted pane should have a ready nvim within 10 s"
    );
    assert!(!pane.is_starting());
    assert_eq!(
        readies.get(),
        1,
        "the host hears of the new session once, as it becomes ready"
    );
    assert_eq!(
        pane.widget().as_ptr(),
        widget_before,
        "the restart must reuse the pane's own widget"
    );
    assert_eq!(unrealized.get(), 0, "the restart must not unrealize the widget");
    assert!(
        pane.widget().is_realized(),
        "the widget stays realized through the restart"
    );
    assert!(pane.is_running(), "the new nvim should be running");

    // Typed keys reach the new nvim; nvim queues the request behind input it has not read yet, but
    // the request may also arrive first, so the line is read until it shows the text.
    pane.send_keys("ihello<Esc>");
    let mut line = None;
    let typed = run_until(Duration::from_secs(10), || {
        line = ask(&pane, "return vim.api.nvim_get_current_line()", Duration::from_secs(2));
        matches!(&line, Some(Ok(value)) if value.as_str() == Some("hello"))
    });
    assert!(
        typed,
        "keys sent after the restart should reach the new nvim; it read back {line:?}"
    );

    let marker = ask(&pane, "return vim.g.restart_marker", Duration::from_secs(5));
    assert!(
        matches!(&marker, Some(Ok(value)) if value.as_str() == Some("second")),
        "the new nvim should be started with the restart's options; g:restart_marker is {marker:?}"
    );
    assert_eq!(
        exits.get(),
        1,
        "the new nvim has not exited, so no second exit is reported"
    );
    assert_eq!(readies.get(), 1, "one session, one report");

    assert!(pane.shutdown(), "the new nvim should exit when shut down");
}

fn restart_is_refused_unless_the_pane_exited() {
    let (_window, pane, unrealized) = realized_pane();
    assert!(pane.is_ready());
    let refused = pane
        .restart(second_options())
        .expect_err("a pane whose nvim is running must refuse a restart");
    assert_eq!(refused.state, "Ready", "the refusal names the state the pane is in");
    assert!(pane.is_running(), "a refused restart leaves the running nvim alone");

    // Still the same nvim: it never saw the refused restart's options.
    let marker = ask(&pane, "return vim.g.restart_marker", Duration::from_secs(5));
    assert!(
        matches!(&marker, Some(Ok(rmpv::Value::Nil))),
        "the running nvim was started without the refused options; g:restart_marker is {marker:?}"
    );
    assert_eq!(unrealized.get(), 0);
    assert!(pane.shutdown(), "nvim should exit when shut down");
}

fn a_restarted_pane_hears_nothing_from_its_ended_harness() {
    let (_window, pane, _unrealized) = realized_pane();
    let exits = Rc::new(Cell::new(0u32));
    let restarted = Rc::new(Cell::new(false));
    {
        // The release and the restart run inside the exit report, before anything else on the main
        // context: whatever the ended harness still has queued is pumped by the new session.
        let weak = Rc::downgrade(&pane);
        let exits = exits.clone();
        let restarted = restarted.clone();
        pane.on_exited_unrequested(move || {
            exits.set(exits.get() + 1);
            let Some(pane) = weak.upgrade() else { return };
            if restarted.get() {
                return;
            }
            assert!(
                pane.release_exited(),
                "the exited nvim should be released in its exit report"
            );
            pane.restart(NeovideEditorPaneOptions {
                clean: true,
                ..Default::default()
            })
            .expect("the released pane should accept a restart in its exit report");
            restarted.set(true);
        });
    }

    // Redraws and the quit in one burst, with the main loop held, so the ended nvim's batches and
    // its exit are queued together on the shared loop.
    let old = ask(&pane, "return vim.fn.getpid()", Duration::from_secs(60))
        .unwrap()
        .unwrap()
        .as_u64()
        .unwrap() as u32;
    pane.send_keys("ihello<Esc>:redraw!<CR>:q!<CR>");
    let deadline = Instant::now() + Duration::from_secs(60);
    while running_nvims().contains(&old) {
        assert!(Instant::now() < deadline, "old nvim never exited");
        std::thread::yield_now();
    }
    assert!(
        run_until(Duration::from_secs(10), || restarted.get()),
        "the first nvim should report its exit and the pane restart"
    );

    assert!(run_until(Duration::from_secs(60), || pane.is_running()));
    settle(&pane, Duration::ZERO);
    let mut exited_seen = false;
    turns(|| exited_seen |= pane.nvim_exited());
    assert!(
        !exited_seen,
        "the restarted pane reported nvim exited while the ended harness's events drained"
    );
    assert_eq!(
        exits.get(),
        1,
        "the ended nvim's exit is reported once, and never to the new session"
    );

    assert!(
        run_until(Duration::from_secs(10), || pane.is_running()),
        "the new nvim should be running"
    );
    let mut exited_seen = false;
    settle(&pane, Duration::ZERO);
    turns(|| exited_seen |= pane.nvim_exited());
    assert!(!exited_seen, "the new nvim is reported exited although it is running");
    assert_eq!(exits.get(), 1);
    assert!(pane.shutdown(), "the new nvim should exit when shut down");
}

fn a_restarted_pane_reports_its_new_nvims_fullscreen_setting() {
    let (_window, pane, _unrealized) = realized_pane();
    let exits = count_exits(&pane);
    let reported = Rc::new(std::cell::RefCell::new(Vec::new()));
    {
        let reported = reported.clone();
        pane.on_fullscreen_setting(move |value| reported.borrow_mut().push(value));
    }

    pane.send_keys(":let g:neovide_fullscreen = v:true<CR>");
    assert!(
        run_until(Duration::from_secs(10), || reported.borrow().last() == Some(&true)),
        "the first nvim's fullscreen setting should reach the host; it heard {:?}",
        reported.borrow()
    );

    pane.send_keys(":q!<CR>");
    assert!(
        run_until(Duration::from_secs(10), || exits.get() == 1),
        "the first nvim should report its own exit"
    );
    assert!(pane.release_exited(), "the exited nvim should be released");
    pane.restart(second_options())
        .expect("a released pane whose nvim ended should accept a restart");
    assert!(
        run_until(Duration::from_secs(10), || pane.is_running()),
        "the restarted pane should have a running nvim"
    );
    assert!(
        run_until(Duration::from_secs(5), || reported.borrow().last() == Some(&false)),
        "the host followed the ended nvim into fullscreen, so it must hear that the new one is not; it heard {:?}",
        reported.borrow()
    );
    assert_eq!(
        *reported.borrow(),
        vec![true, false],
        "each change is reported once: the ended nvim's and the new nvim's"
    );
    assert!(pane.shutdown(), "the new nvim should exit when shut down");
}

fn an_ending_timer_leaves_the_restarted_nvim_alone() {
    let (_window, pane, _unrealized) = realized_pane();
    let exits = Rc::new(Cell::new(0u32));
    let restarted = Rc::new(Cell::new(false));
    {
        // The host lets the pane go and starts it again in the exit report, as soon as it can.
        let weak = Rc::downgrade(&pane);
        let exits = exits.clone();
        let restarted = restarted.clone();
        pane.on_exited_unrequested(move || {
            exits.set(exits.get() + 1);
            let Some(pane) = weak.upgrade() else { return };
            if restarted.get() {
                return;
            }
            assert!(
                pane.release_exited(),
                "the ended nvim should be released in its exit report"
            );
            pane.restart(NeovideEditorPaneOptions {
                clean: true,
                ..Default::default()
            })
            .expect("the released pane should accept a restart in its exit report");
            restarted.set(true);
        });
    }

    // A round trip first, so the pane knows nvim's own pid and the ending signals it.
    let answer = ask(&pane, "return 1", Duration::from_secs(5));
    assert!(
        matches!(&answer, Some(Ok(value)) if value.as_u64() == Some(1)),
        "the first nvim should answer; it said {answer:?}"
    );
    // A request nvim does not answer until a key comes: while it is outstanding nvim may be in a
    // prompt, so the ending signals first and closes nvim's stdin only a second later.
    assert!(
        pane.exec_lua_watched("vim.fn.getchar()"),
        "the watched request should be sent"
    );
    assert!(run_until(Duration::from_secs(60), || pane
        .watched_call()
        .is_some_and(|call| call.last_answer.is_some() && !call.done)));
    neovide_editor::test_support::hold_ending_timer();
    assert!(pane.end_nvim(), "a running nvim can be ended");

    assert!(
        run_until(Duration::from_secs(5), || restarted.get() && pane.is_running()),
        "the ended nvim should exit and the pane restart"
    );
    settle(&pane, Duration::ZERO);
    neovide_editor::test_support::fire_ending_timer();
    assert!(run_until(
        Duration::from_secs(60),
        neovide_editor::test_support::ending_timer_finished
    ));
    turns(|| assert!(pane.is_running(), "the late ending affected the restarted session"));
    assert!(
        pane.is_running(),
        "the ended nvim's late hang-up must not close the new nvim's stdin"
    );
    assert_eq!(exits.get(), 1, "only the ended nvim's exit is reported");
    let answer = ask(&pane, "return 7", Duration::from_secs(5));
    assert!(
        matches!(&answer, Some(Ok(value)) if value.as_u64() == Some(7)),
        "the new nvim should still answer; it said {answer:?}"
    );
    assert!(pane.shutdown(), "the new nvim should exit when shut down");
}

fn a_pane_shut_down_while_restarting_leaves_no_nvim() {
    let (_window, pane, _unrealized) = realized_pane();
    let exits = count_exits(&pane);
    assert!(
        !running_nvims().is_empty(),
        "the first nvim should be seen as this process's own, or the check below proves nothing"
    );

    pane.send_keys(":q!<CR>");
    assert!(
        run_until(Duration::from_secs(10), || exits.get() == 1),
        "the first nvim should report its own exit"
    );
    assert!(pane.release_exited(), "the exited nvim should be released");
    pane.restart(second_options())
        .expect("a released pane whose nvim ended should accept a restart");
    assert!(!pane.is_ready(), "the restart's nvim starts on a later frame");
    // The window closes now, its editor restarting: the close shuts the pane down.
    pane.shutdown();

    // Read every tenth of a second: a closer look buys nothing, and `/proc` is the whole system's.
    let mut started = false;
    let mut left = running_nvims();
    let mut looked = Instant::now();
    let gone = run_until(Duration::from_secs(10), || {
        started |= pane.is_ready();
        if looked.elapsed() >= Duration::from_millis(100) {
            looked = Instant::now();
            left = running_nvims();
        }
        left.is_empty()
    });
    assert!(gone, "an nvim outlived the pane's shutdown: pids {left:?}");
    assert!(run_until(Duration::from_secs(60), || pane.testing_idle()));
    turns(|| {
        started |= pane.is_ready();
        left = running_nvims();
        assert!(left.is_empty(), "a child started after shutdown: {left:?}");
    });
    assert!(
        left.is_empty(),
        "an nvim started after the pane's shutdown: pids {left:?}"
    );
    assert!(!started, "the shut-down pane started the restart's nvim");
}

/// Options whose launch fails: the child's `PATH` names a directory that does not exist, so no
/// `nvim` is found for it.
fn unlaunchable_options() -> NeovideEditorPaneOptions {
    NeovideEditorPaneOptions {
        clean: true,
        child_env: vec![("PATH".into(), "/eitri-realized-restart-has-no-nvim".into())],
        ..Default::default()
    }
}

fn a_pane_whose_launch_failed_starts_again() {
    let (_window, pane, unrealized) = realized_pane();
    let exits = count_exits(&pane);
    let failures = Rc::new(Cell::new(0u32));
    {
        let failures = failures.clone();
        pane.on_start_failed(move |_| failures.set(failures.get() + 1));
    }
    let readies = Rc::new(Cell::new(0u32));
    {
        let readies = readies.clone();
        pane.on_session_ready(move || readies.set(readies.get() + 1));
    }

    pane.send_keys(":q!<CR>");
    assert!(
        run_until(Duration::from_secs(10), || exits.get() == 1),
        "the first nvim should report its own exit"
    );
    assert!(pane.release_exited(), "the exited nvim should be released");
    pane.restart(unlaunchable_options())
        .expect("a released pane whose nvim ended should accept a restart");
    assert!(
        run_until(Duration::from_secs(10), || failures.get() == 1),
        "a launch with no nvim to find should be reported failed"
    );
    assert!(
        !pane.is_ready() && !pane.is_starting(),
        "the failed pane has no nvim and starts none"
    );
    assert_eq!(readies.get(), 0, "a failed launch is never reported ready");
    // A failed pane stops drawing once its failure is painted, so the restart must ask for frames.
    settle(&pane, Duration::from_millis(800));

    pane.restart(second_options())
        .expect("a pane whose launch failed should accept a restart");
    assert!(
        run_until(Duration::from_secs(10), || pane.is_ready()),
        "the failed pane's restart should have a ready nvim within 10 s"
    );
    assert_eq!(readies.get(), 1, "the new session is reported ready once");
    assert_eq!(failures.get(), 1, "the launch that worked is not reported failed");
    let marker = ask(&pane, "return vim.g.restart_marker", Duration::from_secs(5));
    assert!(
        matches!(&marker, Some(Ok(value)) if value.as_str() == Some("second")),
        "the new nvim should be started with the second restart's options; g:restart_marker is {marker:?}"
    );
    assert_eq!(unrealized.get(), 0, "neither restart may unrealize the widget");
    assert!(pane.shutdown(), "the new nvim should exit when shut down");
}

fn main() {
    if let Some(case) = std::env::var_os(CASE_ENV) {
        if let Ok(Some(kind)) = neovide_editor::detach_stdin_from_nvim() {
            println!("[stdin] a {kind} on stdin would reach nvim as a buffer; stdin is /dev/null now");
        }
        let _server = own_x_server::init_gtk("realized_restart", "1024x768x24");
        match case.to_str() {
            Some("a_realized_pane_restarts_on_its_own_widget") => a_realized_pane_restarts_on_its_own_widget(),
            Some("restart_is_refused_unless_the_pane_exited") => restart_is_refused_unless_the_pane_exited(),
            Some("a_restarted_pane_hears_nothing_from_its_ended_harness") => {
                a_restarted_pane_hears_nothing_from_its_ended_harness()
            }
            Some("a_restarted_pane_reports_its_new_nvims_fullscreen_setting") => {
                a_restarted_pane_reports_its_new_nvims_fullscreen_setting()
            }
            Some("an_ending_timer_leaves_the_restarted_nvim_alone") => {
                an_ending_timer_leaves_the_restarted_nvim_alone()
            }
            Some("a_pane_shut_down_while_restarting_leaves_no_nvim") => {
                a_pane_shut_down_while_restarting_leaves_no_nvim()
            }
            Some("a_pane_whose_launch_failed_starts_again") => a_pane_whose_launch_failed_starts_again(),
            other => panic!("unknown case {other:?}"),
        }
        println!("RESULT pass");
        return;
    }
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("realized_restart: ignored (spawns real nvims); run with `-- --ignored`");
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
    assert!(failed.is_empty(), "realized_restart cases failed: {failed:?}");
}
