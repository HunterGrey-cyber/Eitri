//! `exit_wake::Wake`, the one-shot wakes a pane arms when the fork reported nvim's exit while nvim's process still ran:
//! one fires when the process exits (its pidfd), or by a poll where there is no pidfd, one after a delay; each fires
//! once, and one dropped before it fired never does. Real child processes, the default main context owned by the test
//! thread; no display.

use std::cell::{Cell, RefCell};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Child, Command};
use std::rc::Rc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use neovide_editor::exit_wake::{Wake, EXIT_POLL};

/// One test at a time owns the default main context.
static CONTEXT: Mutex<()> = Mutex::new(());

/// Iterates the default main context until `done` or `within` has passed; whether `done` held.
fn run_until(within: Duration, done: impl Fn() -> bool) -> bool {
    let context = glib::MainContext::default();
    let until = Instant::now() + within;
    while !done() {
        if Instant::now() >= until {
            return false;
        }
        if !context.iteration(false) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    true
}

/// A child that runs until it is killed, and a pidfd on it.
fn sleeper() -> (Child, OwnedFd) {
    let child = Command::new("sleep").arg("30").spawn().expect("sleep");
    // SAFETY: a plain syscall with no pointers; a non-negative return is a new fd we own.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id() as libc::pid_t, 0) };
    assert!(fd >= 0, "pidfd_open (Linux 5.3)");
    // SAFETY: as above.
    (child, unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

fn counter() -> (Rc<Cell<u32>>, impl Fn() + 'static) {
    let fired = Rc::new(Cell::new(0));
    let count = fired.clone();
    (fired, move || count.set(count.get() + 1))
}

#[test]
fn a_wake_on_a_pidfd_fires_once_when_the_process_exits() {
    let _serial = CONTEXT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let context = glib::MainContext::default();
    let _owner = context.acquire().expect("the default context");
    let (mut child, pidfd) = sleeper();
    let (fired, fire) = counter();
    let wake = Wake::on_exit(
        Some(pidfd.as_raw_fd()),
        || panic!("a watched pidfd is not polled"),
        fire,
    );
    let early = run_until(Duration::from_millis(200), || fired.get() > 0);
    let pending_while_running = wake.pending();
    child.kill().expect("kill");
    // Not reaped yet: an exited process's pidfd is readable before its parent waits for it.
    let fired_on_exit = run_until(Duration::from_secs(5), || fired.get() > 0);
    run_until(Duration::from_millis(200), || false);
    let _ = child.wait();
    assert!(!early && pending_while_running, "nothing while the process runs");
    assert!(fired_on_exit, "its exit fires the wake");
    assert_eq!(fired.get(), 1, "once");
    assert!(!wake.pending());
    drop(wake);
    drop(pidfd);
}

#[test]
fn a_wake_dropped_before_it_fired_never_fires() {
    let _serial = CONTEXT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let context = glib::MainContext::default();
    let _owner = context.acquire().expect("the default context");
    let (mut child, pidfd) = sleeper();
    let (fired, fire) = counter();
    let wake = Wake::on_exit(Some(pidfd.as_raw_fd()), || true, fire);
    drop(wake);
    child.kill().expect("kill");
    let _ = child.wait();
    run_until(Duration::from_millis(300), || false);
    // The source was removed with the wake, before the fd closes (`fd_watch`'s contract).
    drop(pidfd);
    run_until(Duration::from_millis(100), || false);
    assert_eq!(fired.get(), 0);

    let (fired, fire) = counter();
    drop(Wake::after(Duration::from_millis(20), fire));
    run_until(Duration::from_millis(200), || false);
    assert_eq!(fired.get(), 0, "a delayed wake dropped first never fires either");
}

#[test]
fn with_no_pidfd_the_wake_polls_until_the_process_is_gone() {
    let _serial = CONTEXT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let context = glib::MainContext::default();
    let _owner = context.acquire().expect("the default context");
    let alive = Rc::new(Cell::new(true));
    let asked = Rc::new(Cell::new(0u32));
    let (fired, fire) = counter();
    let wake = {
        let (alive, asked) = (alive.clone(), asked.clone());
        Wake::on_exit(
            None,
            move || {
                asked.set(asked.get() + 1);
                alive.get()
            },
            fire,
        )
    };
    // A few polls go by while it lives.
    let polled = run_until(EXIT_POLL * 10, || asked.get() >= 2);
    let fired_while_alive = fired.get();
    alive.set(false);
    let fired_once_gone = run_until(Duration::from_secs(5), || fired.get() > 0);
    run_until(EXIT_POLL * 3, || false);
    assert!(polled, "it asks again and again");
    assert_eq!(fired_while_alive, 0);
    assert!(fired_once_gone);
    assert_eq!(fired.get(), 1, "once");
    assert!(!wake.pending());
}

#[test]
fn a_delayed_wake_fires_once_and_may_drop_itself() {
    let _serial = CONTEXT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let context = glib::MainContext::default();
    let _owner = context.acquire().expect("the default context");
    // What a pane does: the wake lives in the session, and what it fires may end the session.
    let held: Rc<RefCell<Option<Wake>>> = Rc::new(RefCell::new(None));
    let fired = Rc::new(Cell::new(0u32));
    let wake = {
        let (held, fired) = (held.clone(), fired.clone());
        Wake::after(Duration::from_millis(20), move || {
            fired.set(fired.get() + 1);
            drop(held.borrow_mut().take());
        })
    };
    *held.borrow_mut() = Some(wake);
    assert!(run_until(Duration::from_secs(5), || fired.get() > 0));
    run_until(Duration::from_millis(100), || false);
    assert_eq!(fired.get(), 1);
    assert!(
        held.borrow().is_none(),
        "the wake was dropped inside its own fire, and nothing removed it twice"
    );
}
