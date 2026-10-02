//! The GNOME Shell extension's caller, run against a private bus.
//!
//! Each test starts its own `dbus-daemon` from a scratch config (no `<include>`, no service
//! directories) listening on an explicit address in a scratch directory, and a fake of the
//! extension on a second connection that records every call it gets. Nothing here reads
//! `DBUS_SESSION_BUS_ADDRESS`, so the session bus is never reached: the caller is built with
//! `GnomeShell::with_connector`, and `GnomeShell::new` (the session bus) is not called.
//!
//! The caller runs on a main context of its own, not glib's default one, because other tests in
//! the crate own the default context. A machine without `dbus-daemon` skips with a printed reason.
//!
//! A plain `main` (`harness = false`): the helper that gives a test its own display, and cuts the
//! session bus off, has to run first thing on the main thread before anything else starts, and this
//! test links GTK's crates even though it never opens a window.

// Never reaches a display or the session bus: it starts its own Xvfb, and sets the session bus
// address to a dead one before anything connects (2026-09-29).
#[path = "support/own_x_server.rs"]
mod own_x_server;

// `dead_code`: the product calls more of the module than this test does; `unused_imports`: its own
// unit tests are not collected here (this binary has no libtest harness).
#[allow(dead_code, unused_imports)]
#[path = "../src/companion/gnome_shell.rs"]
mod gnome_shell;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gtk4::gio::{self, DBusConnection, DBusConnectionFlags, DBusNodeInfo};
use gtk4::glib::{self, MainContext, MainLoop};
use gtk4::prelude::ToVariant;

use eitri_core::layout::Direction;
use gnome_shell::{GnomeCalls, GnomeShell, CALL_TIMEOUT_MS};

const INTERFACE_XML: &str = r#"
<node>
  <interface name="cn.huntergrey.Eitri.Shell1">
    <method name="Version"><arg type="u" direction="out" name="version"/></method>
    <method name="FocusDirection"><arg type="s" direction="in" name="direction"/><arg type="b" direction="out" name="moved"/></method>
    <method name="FocusSelfIfNeighbour"><arg type="s" direction="in" name="direction"/><arg type="b" direction="out" name="moved"/></method>
    <method name="SetPartner"><arg type="au" direction="in" name="pids"/><arg type="b" direction="out" name="recorded"/></method>
    <method name="ActivateOwn"><arg type="b" direction="out" name="activated"/></method>
    <method name="ActivatePartner"><arg type="b" direction="out" name="activated"/></method>
  </interface>
</node>"#;

const BUS_CONFIG: &str = r#"<busconfig>
  <type>session</type>
  <listen>unix:path=SOCKET</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#;

/// A `dbus-daemon` started by this test and killed by its captured pid, and the scratch directory
/// it lives in.
struct PrivateBus {
    child: Child,
    dir: PathBuf,
    address: String,
}

impl PrivateBus {
    /// `None`, with the reason printed, when there is no `dbus-daemon` to start.
    fn start(name: &str) -> Option<PrivateBus> {
        if Command::new("dbus-daemon")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            eprintln!("skipping {name}: no dbus-daemon on this machine");
            return None;
        }
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("gsb-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("bus");
        let config = dir.join("bus.conf");
        std::fs::write(&config, BUS_CONFIG.replace("SOCKET", socket.to_str().unwrap())).unwrap();
        let child = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .arg("--nofork")
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .expect("dbus-daemon starts");
        let bus = PrivateBus {
            child,
            dir,
            address: format!("unix:path={}", socket.display()),
        };
        let started = Instant::now();
        while !socket.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the private bus never listened"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        Some(bus)
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Call {
    method: String,
    /// The type string of the arguments the service received, and their text.
    args: String,
}

thread_local! {
    /// The fake's connection, reachable only from its own thread: the name is released and taken
    /// again by code that runs there.
    static FAKE_CONNECTION: RefCell<Option<DBusConnection>> = const { RefCell::new(None) };
}

fn bus_name_call(method: &str, flags_arg: bool) -> Option<u32> {
    FAKE_CONNECTION.with(|slot| {
        let connection = slot.borrow().clone().expect("the fake's connection");
        let args = if flags_arg {
            (gnome_shell::BUS_NAME, 0u32).to_variant()
        } else {
            (gnome_shell::BUS_NAME,).to_variant()
        };
        let reply = connection
            .call_sync(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                method,
                Some(&args),
                None,
                gio::DBusCallFlags::NONE,
                5000,
                gio::Cancellable::NONE,
            )
            .expect("the bus answers");
        reply.get::<(u32,)>().map(|(code,)| code)
    })
}

/// The extension's interface on its own connection, on its own thread and main context.
struct FakeExtension {
    calls: Arc<Mutex<Vec<Call>>>,
    stalled: Arc<AtomicBool>,
    main_loop: MainLoop,
    context: MainContext,
    thread: Option<JoinHandle<()>>,
}

impl FakeExtension {
    fn start(address: &str) -> FakeExtension {
        let calls: Arc<Mutex<Vec<Call>>> = Arc::default();
        let stalled = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let (address, record, stall) = (address.to_string(), calls.clone(), stalled.clone());
        let thread = std::thread::spawn(move || {
            let context = MainContext::new();
            context
                .with_thread_default(|| {
                    let connection = DBusConnection::for_address_sync(
                        &address,
                        DBusConnectionFlags::AUTHENTICATION_CLIENT | DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
                        None,
                        gio::Cancellable::NONE,
                    )
                    .expect("the fake connects to the private bus");
                    let info = DBusNodeInfo::for_xml(INTERFACE_XML).unwrap();
                    let interface = info.lookup_interface(gnome_shell::INTERFACE).unwrap();
                    connection
                        .register_object(gnome_shell::OBJECT_PATH, &interface)
                        .method_call(move |_, _, _, _, method, params, invocation| {
                            record.lock().unwrap().push(Call {
                                method: method.to_string(),
                                args: format!("{} {}", params.type_(), params),
                            });
                            let answer = match method {
                                "Version" => (1u32,).to_variant(),
                                // Only "left" has something beside the panel, to see both answers.
                                "FocusDirection" | "FocusSelfIfNeighbour" => {
                                    (params.child_value(0).str() == Some("left"),).to_variant()
                                }
                                _ => (true,).to_variant(),
                            };
                            if method == "ActivateOwn" && stall.load(Ordering::SeqCst) {
                                // Past the caller's timeout, so the caller gives up first.
                                glib::timeout_add_local_once(Duration::from_millis(3000), move || {
                                    invocation.return_value(Some(&answer));
                                });
                            } else {
                                invocation.return_value(Some(&answer));
                            }
                        })
                        .build()
                        .unwrap();
                    FAKE_CONNECTION.with(|slot| *slot.borrow_mut() = Some(connection.clone()));
                    assert_eq!(bus_name_call("RequestName", true), Some(1), "primary owner of the name");
                    let main_loop = MainLoop::new(Some(&context), false);
                    tx.send((main_loop.clone(), context.clone())).unwrap();
                    main_loop.run();
                    let _ = connection.close_sync(gio::Cancellable::NONE);
                })
                .unwrap();
        });
        let (main_loop, context) = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the fake service came up");
        FakeExtension {
            calls,
            stalled,
            main_loop,
            context,
            thread: Some(thread),
        }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    /// Runs a bus-name call on the fake's thread and waits for it, as a disabled extension (a
    /// screen lock) gives up its name and an enabled one takes it.
    fn on_its_thread(&self, method: &'static str, flags_arg: bool, expect: u32) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.context.invoke(move || {
            let _ = tx.send(bus_name_call(method, flags_arg));
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), Some(expect));
    }

    /// The extension is disabled: its name has no owner.
    fn release_name(&self) {
        self.on_its_thread("ReleaseName", false, 1);
    }

    /// The extension is enabled again.
    fn own_name(&self) {
        self.on_its_thread("RequestName", true, 1);
    }
}

impl Drop for FakeExtension {
    fn drop(&mut self) {
        self.main_loop.quit();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn caller(address: &str) -> Rc<GnomeShell> {
    let address = address.to_string();
    GnomeShell::with_connector(Box::new(move || {
        Box::pin(DBusConnection::for_address_future(
            &address,
            DBusConnectionFlags::AUTHENTICATION_CLIENT | DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
            None,
        ))
    }))
}

/// Runs `body` on a main context of its own, as the thread-default one, so the caller's
/// `spawn_future_local` and gio's callbacks land there.
fn on_own_context<T>(body: impl std::future::Future<Output = T>) -> T {
    let context = MainContext::new();
    context.with_thread_default(|| context.block_on(body)).unwrap()
}

async fn until(what: &str, ms: u64, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(
            started.elapsed() < Duration::from_millis(ms),
            "timed out waiting for {what}"
        );
        glib::timeout_future(Duration::from_millis(5)).await;
    }
}

type Answer = Rc<RefCell<Option<Option<bool>>>>;

fn answer() -> (Answer, Box<dyn FnOnce(Option<bool>)>) {
    let slot: Answer = Rc::default();
    let sink = slot.clone();
    (slot, Box::new(move |value| *sink.borrow_mut() = Some(value)))
}

fn each_method_reaches_the_service_by_name_with_the_documented_argument_types() {
    let Some(bus) = PrivateBus::start("methods") else {
        return;
    };
    let service = FakeExtension::start(&bus.address);
    let calls = on_own_context(async {
        let shell = caller(&bus.address);
        assert_eq!(shell.present(), None, "nothing is known before the first call");
        shell.probe();
        until("the version probe", 5000, || shell.present() == Some(true)).await;

        let (left, done) = answer();
        shell.focus_direction(Direction::Left, done);
        let (up, done_up) = answer();
        shell.focus_self_if_neighbour(Direction::Up, done_up);
        let (set, done_set) = answer();
        shell.set_partner(vec![300, 250], done_set);
        let (clear, done_clear) = answer();
        shell.set_partner(Vec::new(), done_clear);
        let (own, done_own) = answer();
        shell.activate_own(done_own);
        let (partner, done_partner) = answer();
        shell.activate_partner(done_partner);
        for (name, slot) in [
            ("focus_direction", &left),
            ("focus_self_if_neighbour", &up),
            ("set_partner", &set),
            ("set_partner clear", &clear),
            ("activate_own", &own),
            ("activate_partner", &partner),
        ] {
            until(name, 5000, || slot.borrow().is_some()).await;
        }
        // The extension's boolean reaches the callback: a `false` is an answer, not a failure.
        assert_eq!(*left.borrow(), Some(Some(true)));
        assert_eq!(*up.borrow(), Some(Some(false)));
        assert_eq!(*set.borrow(), Some(Some(true)));
        assert_eq!(*own.borrow(), Some(Some(true)));
        service.calls()
    });
    let seen = |method: &str| -> Vec<String> {
        calls
            .iter()
            .filter(|c| c.method == method)
            .map(|c| c.args.clone())
            .collect()
    };
    assert_eq!(seen("Version"), vec!["() ()"]);
    assert_eq!(seen("FocusDirection"), vec!["(s) ('left',)"]);
    assert_eq!(seen("FocusSelfIfNeighbour"), vec!["(s) ('up',)"]);
    // The chain goes as an array of unsigned integers, and an empty chain is still an `au`.
    assert_eq!(seen("SetPartner"), vec!["(au) ([uint32 300, 250],)", "(au) (@au [],)"]);
    assert_eq!(seen("ActivateOwn"), vec!["() ()"]);
    assert_eq!(seen("ActivatePartner"), vec!["() ()"]);
    assert_eq!(calls.len(), 7, "{calls:?}");
}

fn a_bus_with_no_owner_of_the_name_marks_the_extension_absent_and_fails_the_call() {
    let Some(bus) = PrivateBus::start("absent") else { return };
    on_own_context(async {
        let shell = caller(&bus.address);
        shell.probe();
        until("the probe's verdict", 5000, || shell.present().is_some()).await;
        assert_eq!(shell.present(), Some(false));
        // A call made anyway is a failure the callback hears about, and the verdict stands.
        let (slot, done) = answer();
        shell.focus_direction(Direction::Right, done);
        until("the failed call", 5000, || slot.borrow().is_some()).await;
        assert_eq!(*slot.borrow(), Some(None));
        assert_eq!(shell.present(), Some(false));
    });
}

fn a_call_not_answered_in_time_fails_without_calling_the_extension_absent() {
    let Some(bus) = PrivateBus::start("timeout") else {
        return;
    };
    let service = FakeExtension::start(&bus.address);
    service.stalled.store(true, Ordering::SeqCst);
    on_own_context(async {
        let shell = caller(&bus.address);
        let started = Instant::now();
        let (slot, done) = answer();
        shell.activate_own(done);
        until("the call to give up", 5000, || slot.borrow().is_some()).await;
        let waited = started.elapsed();
        assert_eq!(*slot.borrow(), Some(None));
        let timeout = Duration::from_millis(CALL_TIMEOUT_MS as u64);
        assert_eq!(CALL_TIMEOUT_MS, 1000);
        assert!(
            waited >= timeout - Duration::from_millis(100) && waited < timeout + Duration::from_millis(1500),
            "gave up after {waited:?}"
        );
        // The service is there, only slow: that is not the absence the notice is about.
        assert_ne!(shell.present(), Some(false));
    });
    assert_eq!(service.calls().iter().filter(|c| c.method == "ActivateOwn").count(), 1);
}

/// The extension's name goes away and comes back while the panel runs, as it does across a screen
/// lock: no "install the extension" verdict, and the partner is told again on the way back.
fn a_name_that_vanishes_and_returns_is_not_absent_and_runs_the_hook_again() {
    let Some(bus) = PrivateBus::start("lock") else { return };
    let service = FakeExtension::start(&bus.address);
    on_own_context(async {
        let shell = caller(&bus.address);
        let appeared = Rc::new(std::cell::Cell::new(0u32));
        {
            let appeared = appeared.clone();
            shell.on_appeared(Box::new(move || appeared.set(appeared.get() + 1)));
        }
        shell.probe();
        until("the first owner", 5000, || {
            shell.present() == Some(true) && appeared.get() == 1
        })
        .await;
        let versions = || service.calls().iter().filter(|c| c.method == "Version").count();
        until("the version read", 5000, || versions() == 1).await;

        service.release_name();
        until("the name to vanish", 5000, || shell.present() != Some(true)).await;
        assert_eq!(shell.present(), None, "gone for now is not 'not installed'");
        // A call made while it is gone fails, and does not turn that into the absent verdict.
        let (slot, done) = answer();
        shell.focus_direction(Direction::Right, done);
        until("the failed call", 5000, || slot.borrow().is_some()).await;
        assert_eq!(*slot.borrow(), Some(None));
        assert_eq!(shell.present(), None);
        assert_eq!(appeared.get(), 1);

        service.own_name();
        until("the name to return", 5000, || {
            shell.present() == Some(true) && appeared.get() == 2
        })
        .await;
        // The new owner's version is read again.
        until("the second version read", 5000, || versions() == 2).await;
        // And it takes calls.
        let (slot, done) = answer();
        shell.set_partner(vec![7, 6], done);
        until("the call", 5000, || slot.borrow().is_some()).await;
        assert_eq!(*slot.borrow(), Some(Some(true)));
    });
}

/// An extension enabled after the panel started: the "not installed" verdict does not stand.
fn an_extension_that_appears_after_a_first_absent_verdict_is_found() {
    let Some(bus) = PrivateBus::start("late") else { return };
    on_own_context(async {
        let shell = caller(&bus.address);
        let appeared = Rc::new(std::cell::Cell::new(0u32));
        {
            let appeared = appeared.clone();
            shell.on_appeared(Box::new(move || appeared.set(appeared.get() + 1)));
        }
        shell.probe();
        until("the absent verdict", 5000, || shell.present() == Some(false)).await;
        assert_eq!(appeared.get(), 0);
        let service = FakeExtension::start(&bus.address);
        until("the extension to be found", 5000, || {
            shell.present() == Some(true) && appeared.get() == 1
        })
        .await;
        until("its version", 5000, || {
            service.calls().iter().any(|c| c.method == "Version")
        })
        .await;
    });
}

fn main() {
    let server = own_x_server::isolate("gnome_shell_bus", "640x480x24");
    let tests: [(&str, fn()); 5] = [
        (
            "each_method_reaches_the_service_by_name_with_the_documented_argument_types",
            each_method_reaches_the_service_by_name_with_the_documented_argument_types,
        ),
        (
            "a_bus_with_no_owner_of_the_name_marks_the_extension_absent_and_fails_the_call",
            a_bus_with_no_owner_of_the_name_marks_the_extension_absent_and_fails_the_call,
        ),
        (
            "a_call_not_answered_in_time_fails_without_calling_the_extension_absent",
            a_call_not_answered_in_time_fails_without_calling_the_extension_absent,
        ),
        (
            "a_name_that_vanishes_and_returns_is_not_absent_and_runs_the_hook_again",
            a_name_that_vanishes_and_returns_is_not_absent_and_runs_the_hook_again,
        ),
        (
            "an_extension_that_appears_after_a_first_absent_verdict_is_found",
            an_extension_that_appears_after_a_first_absent_verdict_is_found,
        ),
    ];
    let mut failed = 0;
    for (name, test) in tests {
        match std::panic::catch_unwind(test) {
            Ok(()) => println!("test {name} ... ok"),
            Err(_) => {
                println!("test {name} ... FAILED");
                failed += 1;
            }
        }
    }
    println!("gnome_shell_bus: {} passed, {failed} failed", tests.len() - failed);
    server.exit(i32::from(failed > 0));
}
