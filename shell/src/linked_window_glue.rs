//! The GTK half of the linked windows: what the toolkit-free pair code is given by this binary: its scheduling and
//! readiness watches, application holds, the stamp of each activation, and the reload of a returned page's webview.

pub(crate) mod focus;

use std::os::fd::RawFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use eitri_core::layout::ModuleId;
use gtk4::prelude::{ApplicationExtManual, GtkWindowExt};
use gtk4::{gio, glib};

use crate::linked_window::host_loop::{Armed, Flow, HoldGuard, Interest, LinkWatch, NotOnLoop, PairClock, Priority};
use crate::linked_window::spawn::PairEnv;
use crate::project_stamp::StampSlot;

/// Supplies the executable, build identity and process services for one pair.
pub(crate) fn pair_env() -> Rc<PairEnv> {
    Rc::new(PairEnv {
        // Use the running executable so a window still starts this build after an upgrade replaces the installed file.
        exe: PathBuf::from("/proc/self/exe"),
        build: crate::version::fingerprint(),
        since_first_signal: crate::end_signals::since_first_signal,
        clock: Rc::new(GtkClock),
    })
}

/// Hands every activation of `window` to its stamp. A window with no lock never fills the slot, and writes nothing.
pub(crate) fn watch_activation(window: &gtk4::ApplicationWindow, slot: Rc<StampSlot>) {
    window.connect_is_active_notify(move |window| {
        if window.is_active() {
            slot.activated(SystemTime::now());
        }
    });
}

/// Loads `module`'s page again, from its store as it is now: a plugin's page with no view here (never built, or
/// dropped when it left for the other window) is built, an away host is reopened, and a page already loaded for
/// this return is reused. A Lua panel's view always loads its URL. Whether the page is available.
pub(crate) fn reload_page_back(
    plugin_panels: &crate::plugin_panels::PluginPanels,
    lua_pages: &crate::gtk_host_services::LuaPages,
    module: &ModuleId,
) -> bool {
    use gtk4::prelude::*;
    use webkit6::prelude::*;
    if plugin_panels.modules().contains(module) {
        return plugin_panels.reload_back(module);
    }
    let Some((_, widget, url, _)) = crate::gtk_host_services::lua_page(lua_pages, module) else {
        return false;
    };
    match widget.downcast_ref::<webkit6::WebView>() {
        Some(view) => {
            view.load_uri(&url);
            true
        }
        None => false,
    }
}

struct GtkClock;

/// Watches `fd` on the default main context at the GLib priority that `priority` names, calling `on_ready` as long as it
/// answers `Flow::Continue`. `NotOnLoop` unless this thread owns that context.
fn arm_source(
    fd: RawFd,
    interest: Interest,
    priority: Priority,
    mut on_ready: Box<dyn FnMut() -> Flow>,
) -> Result<glib::SourceId, NotOnLoop> {
    let condition = match interest {
        Interest::Readable => glib::IOCondition::IN,
    };
    let priority = match priority {
        Priority::Default => glib::ffi::G_PRIORITY_DEFAULT,
    };
    neovide_editor::fd_watch::add_local(fd, condition, priority, move |_| match on_ready() {
        Flow::Continue => glib::ControlFlow::Continue,
        Flow::Stop => glib::ControlFlow::Break,
    })
    .map_err(|_| NotOnLoop)
}

impl LinkWatch for GtkClock {
    fn arm(
        &self,
        fd: RawFd,
        interest: Interest,
        priority: Priority,
        on_ready: Box<dyn FnMut() -> Flow>,
    ) -> Result<Box<dyn Armed>, NotOnLoop> {
        Ok(Box::new(GtkArmed(arm_source(fd, interest, priority, on_ready)?)))
    }
}

impl PairClock for GtkClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn after(&self, within: Duration, run: Box<dyn FnOnce()>) {
        glib::timeout_add_local_once(within, run);
    }

    fn every(&self, period: Duration, mut tick: Box<dyn FnMut() -> Flow>) {
        glib::timeout_add_local(period, move || match tick() {
            Flow::Continue => glib::ControlFlow::Continue,
            Flow::Stop => glib::ControlFlow::Break,
        });
    }

    fn blocking(&self, job: Box<dyn FnOnce() + Send>, done: Box<dyn FnOnce()>) {
        let joined = gio::spawn_blocking(job);
        glib::spawn_future_local(async move {
            let _ = joined.await;
            done();
        });
    }

    fn hold(&self) -> Option<Box<dyn HoldGuard>> {
        gio::Application::default().map(|app| Box::new(GtkHold { _guard: app.hold() }) as Box<dyn HoldGuard>)
    }
}

/// The application stays up while this lives; dropping it releases the hold.
struct GtkHold {
    _guard: gio::ApplicationHoldGuard,
}

impl HoldGuard for GtkHold {}

struct GtkArmed(glib::SourceId);

impl Armed for GtkArmed {
    fn disarm(self: Box<Self>) {
        self.0.remove();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ide_wiring::ShellPort;
    use crate::layout_state::default_context_turn;
    use crate::linked_window::driver::tests::pump_until;
    use crate::linked_window::spawn::tests::{end_stand_in_in_grace, port_env, two_windows, wait_for_port, PORT};
    use crate::linked_window::spawn::{release_when_finished, Pair, PairEnv};
    use eitri_core::window_link::fdpass;
    use std::cell::{Cell, RefCell};
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;

    /// Says a test was skipped because another thread holds the default main context.
    fn skipped() {
        eprintln!("skipped: another thread holds the default main context");
    }

    /// Iterates the default context until `done`, which a hang guard bounds.
    fn iterate_until(context: &glib::MainContext, mut done: impl FnMut() -> bool) {
        let guard = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(
                Instant::now() < guard,
                "the main context never reached the state awaited"
            );
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The watch is made at GLib's default priority, and only on the thread that owns the default context.
    #[test]
    fn a_readable_watch_is_armed_at_the_default_priority_on_the_loops_thread_only() {
        let _turn = default_context_turn();
        let context = glib::MainContext::default();
        let Ok(_owner) = context.acquire() else {
            return skipped();
        };
        let (reader, _writer) = std::io::pipe().unwrap();
        let id = arm_source(
            reader.as_raw_fd(),
            Interest::Readable,
            Priority::Default,
            Box::new(|| Flow::Stop),
        )
        .expect("armed on the thread that owns the context");
        assert_eq!(
            neovide_editor::fd_watch::source_priority(&id),
            Some(glib::ffi::G_PRIORITY_DEFAULT)
        );
        id.remove();

        let fd = reader.as_raw_fd();
        let off_thread = std::thread::spawn(move || {
            GtkClock
                .arm(fd, Interest::Readable, Priority::Default, Box::new(|| Flow::Stop))
                .err()
        })
        .join()
        .unwrap();
        assert_eq!(off_thread, Some(NotOnLoop));
    }

    /// `Continue` keeps a watch, `Stop` ends it, and disarming removes it; each is the host's own flow.
    #[test]
    fn a_watch_keeps_on_continue_ends_on_stop_and_goes_when_disarmed() {
        let _turn = default_context_turn();
        let context = glib::MainContext::default();
        let Ok(_owner) = context.acquire() else {
            return skipped();
        };
        // The byte stays unread, so a kept watch is ready again at every iteration.
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer.write_all(b"x").unwrap();
        let fd = reader.as_raw_fd();

        let calls = Rc::new(Cell::new(0u32));
        let counted = calls.clone();
        let kept = GtkClock
            .arm(
                fd,
                Interest::Readable,
                Priority::Default,
                Box::new(move || {
                    counted.set(counted.get() + 1);
                    Flow::Continue
                }),
            )
            .unwrap();
        iterate_until(&context, || calls.get() >= 3);
        kept.disarm();
        let after_disarm = calls.get();
        for _ in 0..20 {
            context.iteration(false);
        }
        assert_eq!(calls.get(), after_disarm, "a disarmed watch is not called again");

        let calls = Rc::new(Cell::new(0u32));
        let counted = calls.clone();
        let id = arm_source(
            fd,
            Interest::Readable,
            Priority::Default,
            Box::new(move || {
                counted.set(counted.get() + 1);
                Flow::Stop
            }),
        )
        .unwrap();
        iterate_until(&context, || calls.get() >= 1);
        for _ in 0..20 {
            context.iteration(false);
        }
        assert_eq!(calls.get(), 1, "a watch that answered Stop is not called again");
        assert!(context.find_source_by_id(&id).is_none(), "and the host has removed it");
    }

    /// A one-shot timer runs once; a repeating one runs until its tick answers `Stop`, and no more.
    #[test]
    fn the_timers_run_once_and_repeat_until_stopped() {
        let _turn = default_context_turn();
        let context = glib::MainContext::default();
        let Ok(_owner) = context.acquire() else {
            return skipped();
        };
        let period = Duration::from_millis(5);
        let past_two_periods = |start: Instant| Instant::now() >= start + period * 3;

        let runs = Rc::new(Cell::new(0u32));
        let counted = runs.clone();
        let start = Instant::now();
        GtkClock.after(period, Box::new(move || counted.set(counted.get() + 1)));
        iterate_until(&context, || runs.get() >= 1 && past_two_periods(start));
        assert_eq!(runs.get(), 1, "a one-shot timer runs once");

        let ticks = Rc::new(Cell::new(0u32));
        let counted = ticks.clone();
        GtkClock.every(
            period,
            Box::new(move || {
                counted.set(counted.get() + 1);
                if counted.get() >= 3 {
                    Flow::Stop
                } else {
                    Flow::Continue
                }
            }),
        );
        iterate_until(&context, || ticks.get() >= 3);
        let stopped_at = Instant::now();
        iterate_until(&context, || past_two_periods(stopped_at));
        assert_eq!(ticks.get(), 3, "Continue kept the timer and Stop ended it");
    }

    /// The main loop keeps the hold while the ending thread is parked, then releases it after the join.
    #[test]
    fn a_finished_ending_is_released_without_waiting_for_a_timer() {
        use std::sync::mpsc;
        let context = gtk4::glib::MainContext::new();
        let released = Rc::new(Cell::new(false));
        context
            .with_thread_default(|| {
                let (finish, held) = mpsc::channel();
                let ending = std::thread::spawn(move || {
                    held.recv_timeout(Duration::from_secs(10))
                        .expect("the test releases the ending");
                });
                {
                    let released = released.clone();
                    release_when_finished(ending, move || released.set(true), &GtkClock);
                }
                let (marked, marker_seen) = mpsc::channel();
                let marker = gtk4::glib::idle_source_new(None, gtk4::glib::Priority::DEFAULT_IDLE, move || {
                    marked.send(()).unwrap();
                    gtk4::glib::ControlFlow::Break
                });
                marker.attach(Some(&context));
                let deadline = Instant::now() + Duration::from_secs(10);
                while marker_seen.try_recv().is_err() {
                    assert!(Instant::now() < deadline, "the main loop never dispatched the marker");
                    context.iteration(false);
                    std::thread::yield_now();
                }
                assert!(!released.get(), "the hold cannot go before the ending finishes");
                finish.send(()).unwrap();
                while !released.get() {
                    assert!(Instant::now() < deadline, "the finished ending never released its hold");
                    context.iteration(false);
                    std::thread::yield_now();
                }
            })
            .unwrap();
        assert!(released.get());
    }

    /// `GtkClock` with its grace timer collapsed to the next idle: the delay it was given is recorded, and the flag
    /// is set once the scheduled run has completed, so a test can see both without waiting the grace out.
    struct GraceClock {
        within: RefCell<Option<Duration>>,
        completed: Rc<Cell<bool>>,
    }

    impl LinkWatch for GraceClock {
        fn arm(
            &self,
            fd: RawFd,
            interest: Interest,
            priority: Priority,
            on_ready: Box<dyn FnMut() -> Flow>,
        ) -> Result<Box<dyn Armed>, NotOnLoop> {
            GtkClock.arm(fd, interest, priority, on_ready)
        }
    }

    impl PairClock for GraceClock {
        fn now(&self) -> Instant {
            GtkClock.now()
        }

        fn after(&self, within: Duration, run: Box<dyn FnOnce()>) {
            *self.within.borrow_mut() = Some(within);
            let completed = self.completed.clone();
            glib::idle_add_local_once(move || {
                run();
                completed.set(true);
            });
        }

        fn every(&self, period: Duration, tick: Box<dyn FnMut() -> Flow>) {
            GtkClock.every(period, tick);
        }

        fn blocking(&self, job: Box<dyn FnOnce() + Send>, done: Box<dyn FnOnce()>) {
            GtkClock.blocking(job, done);
        }

        fn hold(&self) -> Option<Box<dyn HoldGuard>> {
            GtkClock.hold()
        }
    }

    /// An application whose last window has closed must not exit while a window 2 still waits for its SIGKILL: `run`
    /// returns only once the ending is done, on the thread and on the main-loop timer alike.
    ///
    /// Ignored because `g_application_run` iterates the default main context, which two tests in two threads would
    /// fight over. Run it alone:
    /// `cargo test -p shell --bin shell linked_window_glue::tests::the_application_outlives_an_ending_in_its_grace -- --ignored --exact`.
    #[test]
    #[ignore = "runs a real gio main loop on the default main context; must not race the rest of the suite"]
    fn the_application_outlives_an_ending_in_its_grace() {
        let _turn = crate::layout_state::default_context_turn();
        use gtk4::gio;
        use gtk4::prelude::*;
        let grace = Duration::from_secs(400);
        for on_timer in [false, true] {
            let app = gio::Application::new(None, gio::ApplicationFlags::NON_UNIQUE);
            let second = Rc::new(RefCell::new(None));
            let completed = Rc::new(Cell::new(false));
            let clock = Rc::new(GraceClock {
                within: RefCell::new(None),
                completed: completed.clone(),
            });
            let (process, observed, activating) = (second.clone(), completed.clone(), clock.clone());
            app.connect_activate(move |_| {
                let (stand_in, release) = end_stand_in_in_grace(&*activating, grace, on_timer);
                *process.borrow_mut() = Some(stand_in);
                if let Some(release) = release {
                    let observed = observed.clone();
                    gtk4::glib::idle_add_local_once(move || {
                        observed.set(true);
                        release.send(()).unwrap();
                    });
                }
            });
            let expired = Rc::new(Cell::new(false));
            let expired_in_timer = expired.clone();
            let quitting = app.clone();
            // The harness watchdog needs a source handle so it can be cancelled after `run` returns.
            let guard = gtk4::glib::timeout_add_local_once(Duration::from_secs(60), move || {
                expired_in_timer.set(true);
                quitting.quit();
            });
            app.run_with_args(&["shell-ending-hold"]);
            if let Some(source) = gtk4::glib::MainContext::default().find_source_by_id(&guard) {
                source.destroy();
            }
            assert!(!expired.get(), "the application never released its hold");
            assert!(completed.get(), "the application returned before the grace boundary");
            if on_timer {
                assert_eq!(
                    *clock.within.borrow(),
                    Some(grace),
                    "the timer was armed with the whole grace"
                );
            }
            let second = second
                .borrow_mut()
                .take()
                .expect("the application started the stand-in");
            let status = second.wait_reaped(Duration::from_secs(60)).expect("reaped");
            assert_eq!(status.signal(), Some(libc::SIGKILL));
        }
    }

    #[test]
    fn a_window_2_restored_with_a_terminal_shown_has_the_port_on_its_first_shell() {
        let (_window1, two) = fdpass::seqpacket_pair().unwrap();
        let second = Pair::second(PairEnv::for_tests(), two, Path::new("/p")).unwrap();
        second.set_ide_port(Some(PORT));
        assert!(!second.link().unwrap().linked(), "no welcome has come");
        assert_eq!(ShellPort::shared(second.ide_port_cell()).env(), port_env(PORT));
    }

    #[test]
    fn a_relink_brings_the_new_port() {
        let (window1, window2) = two_windows();
        let shell = ShellPort::shared(window2.ide_port_cell());
        assert_eq!(shell.env(), Vec::new(), "welcome said nothing of a port");

        window1.set_ide_port(Some(PORT));
        window1.say_ide_port(&window1.link().unwrap());

        wait_for_port(&window2, Some(PORT));
        assert_eq!(shell.env(), port_env(PORT));
    }

    #[test]
    fn a_shell_started_while_orphaned_has_no_port() {
        let (window1, window2) = two_windows();
        window2.set_ide_port(Some(PORT));
        let shell = ShellPort::shared(window2.ide_port_cell());
        let link = window2.link().unwrap();

        window1.link().unwrap().close("window 1 closed");

        assert!(pump_until(&link, || link.ended().is_some()));
        assert_eq!(shell.env(), Vec::new());
    }

    #[test]
    fn a_port_message_changes_no_running_shell() {
        let (window1, window2) = two_windows();
        window2.set_ide_port(Some(PORT));
        let shell = ShellPort::shared(window2.ide_port_cell());
        let running = shell.env();

        window1.ide_port_changed(Some(PORT + 1));
        wait_for_port(&window2, Some(PORT + 1));

        assert_eq!(running, port_env(PORT), "a shell keeps what it started with");
        assert_eq!(shell.env(), port_env(PORT + 1), "the next one has the new port");
    }
}
