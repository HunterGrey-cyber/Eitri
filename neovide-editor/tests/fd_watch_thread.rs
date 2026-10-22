//! `fd_watch::add_local`'s `!Send` closure must run and drop on its registering thread.
//! Two safe caller actions can otherwise move those operations to another thread:
//! - release the default context after registration and let another thread iterate it;
//! - send the `Send` SourceId to another thread and remove it there, invoking destroy notify.
//! Both must fail loudly through the guard panic or callback abort rather than execute.
//! Each scenario uses a child copy of this test binary because it ends the process.
//! No display is needed.

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use neovide_editor::fd_watch;

const SCENARIO: &str = "EITRI_FD_WATCH_THREAD_SCENARIO";

/// Runs `scenario` in a child and returns (exited successfully, stderr).
fn run_child(test_name: &str, scenario: &str) -> (bool, String) {
    let out = Command::new(std::env::current_exe().expect("the test binary"))
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(SCENARIO, scenario)
        .output()
        .expect("the child runs");
    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// The child half: registers a watch on a readable socket, then does `scenario`.
fn child(scenario: &str) {
    let (read, mut write) = UnixStream::pair().expect("a socket pair");
    std::io::Write::write_all(&mut write, b"x").expect("one byte");
    let context = glib::MainContext::default();
    let owner = context.acquire().expect("the child's only user of the default context");
    // `Rc`: the closure really is `!Send`.
    let ran_off_thread = Arc::new(AtomicBool::new(false));
    let marker = Rc::new(thread::current().id());
    let flag = ran_off_thread.clone();
    let id = fd_watch::add_local(
        read.as_raw_fd(),
        glib::IOCondition::IN,
        fd_watch::EVENT_LOOP_WATCH_PRIORITY,
        move |_| {
            if *marker != thread::current().id() {
                flag.store(true, Ordering::SeqCst);
            }
            glib::ControlFlow::Break
        },
    )
    .expect("this thread owns the default context");
    match scenario {
        "dispatch" => {
            drop(owner);
            thread::spawn(move || {
                let context = glib::MainContext::default();
                let _owner = context.acquire().expect("released by the registering thread");
                context.iteration(false);
            })
            .join()
            .ok();
        }
        "remove" => {
            thread::spawn(move || id.remove()).join().ok();
            drop(owner);
        }
        other => panic!("unknown scenario {other}"),
    }
    // Reached only if nothing stopped it: say what happened for the parent's message.
    eprintln!(
        "child finished; closure ran off thread: {}",
        ran_off_thread.load(Ordering::SeqCst)
    );
}

fn check(test_name: &str, scenario: &str) {
    if let Ok(s) = std::env::var(SCENARIO) {
        if s == scenario {
            child(&s);
        }
        return;
    }
    let (ok, stderr) = run_child(test_name, scenario);
    assert!(
        !ok && stderr.contains("different thread"),
        "{scenario}: the closure must not run or drop off its thread without the guard stopping it; child stderr:\n{stderr}"
    );
}

#[test]
fn dispatch_on_another_thread_is_stopped() {
    check("dispatch_on_another_thread_is_stopped", "dispatch");
}

#[test]
fn removal_on_another_thread_is_stopped() {
    check("removal_on_another_thread_is_stopped", "remove");
}
