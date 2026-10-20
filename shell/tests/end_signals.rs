//! A termination signal sent to the process reaches `end_signals`' handler on the main context instead of ending
//! the process (the window's half -- the close it then runs -- is held by `main.rs`'s own test,
//! `a_termination_signal_closes_the_window_without_its_questions`), and when the main context is blocked the
//! watchdog ends the process by itself. Its own test binary: it signals itself, and without the handler the first
//! signal ends the whole binary, which is the failure the first test guards against. The blocked window is a child
//! process (this binary again, running only `blocked_window_child`), so its end is observed from outside.

// `mark_closing` belongs to the window's handler, which no test here plays.
#[allow(dead_code)]
#[path = "../src/end_signals.rs"]
mod end_signals;

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

const CHILD_ENV: &str = "EITRI_END_SIGNALS_CHILD";
const READY: &str = "END-SIGNALS-CHILD-READY";

#[test]
fn each_termination_signal_reaches_the_handler_and_the_process_goes_on() {
    let context = glib::MainContext::default();
    let _owner = context
        .acquire()
        .expect("this test's thread owns the default main context");
    let seen = Rc::new(RefCell::new(Vec::new()));
    {
        let seen = seen.clone();
        end_signals::install(move |name| seen.borrow_mut().push(name)).expect("installed on the owning thread");
    }
    for (signum, name) in end_signals::SIGNALS {
        // SAFETY: a signal to this process; the handler installed above is glib's, which only marks it for the
        // main context.
        assert_eq!(unsafe { libc::kill(libc::getpid(), signum) }, 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !seen.borrow().contains(&name) {
            assert!(
                Instant::now() < deadline,
                "{name} never reached the handler: {:?}",
                seen.borrow()
            );
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    // Every signal was answered and no close was marked: the watchdog left the process alone.
    assert_eq!(*seen.borrow(), ["SIGTERM", "SIGINT", "SIGHUP"]);
}

/// The blocked window: handlers installed on the default context, which then never runs again. Only does
/// anything in the child the two tests below start.
#[test]
fn blocked_window_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let context = glib::MainContext::default();
    let _owner = context.acquire().expect("the child owns its default main context");
    end_signals::install(|_| {}).expect("installed on the owning thread");
    println!("{READY}");
    std::thread::sleep(Duration::from_secs(120));
    std::process::exit(0);
}

/// Starts the blocked window, sends `signals` (a pause between them), and returns its exit code and stderr.
fn end_a_blocked_window(signals: &[i32]) -> (Option<i32>, String) {
    let mut child = Command::new(std::env::current_exe().expect("this test binary"))
        .args(["blocked_window_child", "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the child starts");
    let pid = child.id() as i32;
    let mut out = BufReader::new(child.stdout.take().expect("its stdout"));
    let mut line = String::new();
    loop {
        line.clear();
        let n = out.read_line(&mut line).expect("reading the child's stdout");
        assert!(n > 0, "the child ended before it was ready");
        if line.contains(READY) {
            break;
        }
    }
    for (i, &signum) in signals.iter().enumerate() {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(300));
        }
        // SAFETY: a signal to the child this test started and has not reaped.
        assert_eq!(unsafe { libc::kill(pid, signum) }, 0);
    }
    // Only a guard against a child that never ends; no time is asserted.
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("waiting for the child") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the blocked window did not end within 60 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut err = String::new();
    child
        .stderr
        .take()
        .expect("its stderr")
        .read_to_string(&mut err)
        .expect("reading the child's stderr");
    (status.code(), err)
}

#[test]
fn an_unanswered_signal_ends_a_blocked_window_with_its_own_number() {
    let (code, err) = end_a_blocked_window(&[libc::SIGTERM]);
    assert_eq!(code, Some(128 + libc::SIGTERM), "stderr: {err}");
    assert!(err.contains("did not answer"), "stderr: {err}");
}

#[test]
fn a_second_signal_ends_a_blocked_window_without_waiting_for_the_bound() {
    // The second signal's number, not the first's: the exit came from the second-signal rule, not the bound.
    let (code, err) = end_a_blocked_window(&[libc::SIGTERM, libc::SIGINT]);
    assert_eq!(code, Some(128 + libc::SIGINT), "stderr: {err}");
    assert!(err.contains("a second termination signal"), "stderr: {err}");
}
