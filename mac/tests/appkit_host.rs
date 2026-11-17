//! The host's start and end without a window: the panel assembled on the real page, the band saying the
//! window waits for the editor, the link aimed at nvim's socket as soon as it exists, a quit with nothing at
//! work going straight on, and the shutdown running once whichever way out calls it.
//!
//! AppKit and WebKit want the main thread, so this file is its own `main`. It owns the process: the panel
//! writes its state under the state home it is given.

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    macos::run();
}

#[cfg(target_os = "macos")]
mod macos {
    use std::io::Write;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::rc::Rc;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use eitri_mac::appkit::page::PANEL_DOCUMENT;
    use eitri_mac::appkit::window::Host;
    use eitri_mac::assembly::{assemble, HostActions};
    use eitri_mac::editor::{AttachWait, Editor};
    use eitri_mac::quit::QuitSource;
    use eitri_panel::agent_panel::ViewId;
    use eitri_panel::panel_page::PanelHost;
    use std::cell::{Cell, RefCell};

    use eitri_mac::appkit::app::{click_held_button, quit_alert, quit_chosen};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject};
    use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::NSEventTrackingRunLoopMode;
    use objc2_app_kit::{NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSButton};
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSRunLoop, NSString};

    struct NoLinks;
    impl PanelHost for NoLinks {
        fn open_external(&self, _url: &str) {}
    }

    fn spin_until(limit: Duration, done: impl Fn() -> bool) -> bool {
        // SAFETY: an extern static AppKit's Foundation defines.
        spin_in(unsafe { NSDefaultRunLoopMode }, limit, done)
    }

    /// [`spin_until`] with the run loop in `mode`: the event-tracking mode is the one AppKit runs while a menu is
    /// open or the window is being resized.
    fn spin_in(mode: &NSString, limit: Duration, done: impl Fn() -> bool) -> bool {
        let end = Instant::now() + limit;
        while Instant::now() < end {
            if done() {
                return true;
            }
            // Turns this thread's own run loop once, for at most 20 ms.
            let until = NSDate::dateWithTimeIntervalSinceNow(0.02);
            NSRunLoop::currentRunLoop().runMode_beforeDate(mode, &until);
        }
        done()
    }

    fn spin_once() {
        let until = NSDate::dateWithTimeIntervalSinceNow(0.02);
        // SAFETY: an extern static Foundation defines.
        NSRunLoop::currentRunLoop().runMode_beforeDate(unsafe { NSDefaultRunLoopMode }, &until);
    }

    /// Stands in for Neovide's nvim on `listener`: answers the handshake, the install (with a report of no parts),
    /// and every other request with nil, until `go` says the editor goes; then it closes the connection.
    fn fake_nvim(listener: UnixListener, go: mpsc::Receiver<()>) -> std::thread::JoinHandle<()> {
        use rmpv::Value;
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else { return };
            stream.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let report = Value::Map(vec![
                (Value::from("version"), Value::from(1)),
                (Value::from("installed"), Value::Array(vec![])),
                (Value::from("failed"), Value::Array(vec![])),
                (Value::from("pid"), Value::from(std::process::id())),
                (Value::from("tmux"), Value::from(false)),
            ]);
            loop {
                if go.try_recv().is_ok() {
                    return;
                }
                let value = match rmpv::decode::read_value(&mut reader) {
                    Ok(value) => value,
                    Err(rmpv::decode::Error::InvalidMarkerRead(e)) | Err(rmpv::decode::Error::InvalidDataRead(e))
                        if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                    {
                        continue
                    }
                    Err(_) => return,
                };
                let items = value.as_array().cloned().unwrap_or_default();
                if items.first().and_then(Value::as_u64) != Some(0) {
                    continue;
                }
                let answer = match items.get(2).and_then(Value::as_str) {
                    Some("nvim_get_api_info") => Value::Array(vec![Value::from(7), Value::Map(vec![])]),
                    Some("nvim_exec_lua") => report.clone(),
                    _ => Value::Nil,
                };
                let mut bytes = Vec::new();
                let response = Value::Array(vec![Value::from(1), items[1].clone(), Value::Nil, answer]);
                rmpv::encode::write_value(&mut bytes, &response).unwrap();
                if stream.write_all(&bytes).is_err() {
                    return;
                }
            }
        })
    }

    pub fn run() {
        let mtm = MainThreadMarker::new().expect("a test without the harness runs on the main thread");
        let mut failed = 0;
        let mut check = |name: &str, ok: bool, detail: String| {
            println!("{} {name}: {detail}", if ok { "ok  " } else { "FAIL" });
            if !ok {
                failed += 1;
            }
        };

        // A short base: nvim's socket path must fit macOS's limit.
        let tmp = tempfile::Builder::new()
            .tempdir_in("/tmp")
            .expect("a scratch directory");
        let (state_home, config_dir, project) = (tmp.path().join("s"), tmp.path().join("c"), tmp.path().join("p"));
        for dir in [&state_home, &config_dir, &project] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::env::set_var("XDG_STATE_HOME", &state_home);
        std::env::remove_var("VERDANDI_CLAUDE_ACCOUNT");
        std::env::remove_var("EITRI_AGENT_BACKEND");
        std::fs::write(
            config_dir.join("init.lua"),
            "eitri.config.set(\"keymap.from_tmux\", \"off\")\n",
        )
        .unwrap();
        // The socket in a directory of its own, as the app makes it, so its removal can be seen.
        std::fs::create_dir(tmp.path().join("nl")).unwrap();
        let nvim_listen = tmp.path().join("nl/n.sock");

        let editor = Rc::new(Editor::new(nvim_listen.clone()).expect("an editor"));
        let host = Host::new(mtm);
        let assembly = Rc::new(
            assemble(
                &project,
                &config_dir,
                Some(host.page()),
                Rc::new(NoLinks),
                &PANEL_DOCUMENT,
                editor,
                host.clone() as Rc<dyn HostActions>,
            )
            .unwrap_or_else(|why| panic!("assemble: {why}")),
        );
        host.attach(assembly.clone());

        let waiting = spin_until(Duration::from_secs(60), || {
            assembly.editor_link() == Some((AttachWait::WAITING.0.into(), AttachWait::WAITING.1.into()))
        });
        check(
            "the band says the window waits for the editor",
            waiting && host.life.attach_phase() == "waiting for nvim's socket",
            format!("{:?}, {}", assembly.editor_link(), host.life.attach_phase()),
        );

        // The panel's tick: started at the attach, run once at once, then only when something is due. Looked at
        // before nvim listens, while the editor's link has nothing to report.
        check(
            "the panel's tick is driven from the attach",
            host.panel_ticking(),
            String::new(),
        );
        let panel = assembly.panel.clone();
        let ticked = spin_until(Duration::from_secs(60), || panel.ticks_run() > 0);
        check(
            "its first tick runs on the loop's first turns",
            ticked,
            format!("{} ticks", panel.ticks_run()),
        );
        // Idle starts once the document has loaded AND said `ready` (its message arrives after the load finishes, and
        // the panel's answer to it is the tick's legitimate work), and the tick has been quiet for 250 ms with no
        // timer armed.
        let (seen, quiet_since) = (Cell::new(panel.ticks_run()), Cell::new(Instant::now()));
        let idle = spin_until(Duration::from_secs(60), || {
            if panel.ticks_run() != seen.get() {
                seen.set(panel.ticks_run());
                quiet_since.set(Instant::now());
            }
            !host.panel_view().document_loading()
                && panel.document_is_ready(ViewId::BASE)
                && !host.panel_timer_armed()
                && quiet_since.get().elapsed() >= Duration::from_millis(250)
        });
        let before = panel.ticks_run();
        // When each tick of the window ran (the turn of the loop, and milliseconds since it began), for the failure line.
        let (began, mut seen, mut when) = (Instant::now(), before, Vec::new());
        // Half a second of loop turns: a tick on the steady cadence (a few dozen milliseconds) would show many times.
        let mut turn = 0;
        while began.elapsed() < Duration::from_millis(500) {
            turn += 1;
            spin_once();
            if panel.ticks_run() != seen {
                seen = panel.ticks_run();
                when.push(format!(
                    "turn {turn} at {} ms, timer armed {}",
                    began.elapsed().as_millis(),
                    host.panel_timer_armed()
                ));
            }
        }
        let ticks = panel.ticks_run() - before;
        // One tick is allowed: a page message or a worker's report can still arrive after the settle (seen once in ten
        // runs, a single tick with no timer armed), and each raises the wake on purpose. A tick on a cadence, or a timer
        // armed while idle, is what this is here to catch.
        check(
            "an idle panel is not ticked",
            idle && ticks <= 1 && !host.panel_timer_armed(),
            format!(
                "{ticks} ticks {when:?}; settled {idle}, document ready {}, loading {}",
                panel.document_is_ready(ViewId::BASE),
                host.panel_view().document_loading()
            ),
        );
        let before = panel.ticks_run();
        panel.new_tab();
        let woke = spin_until(Duration::from_secs(60), || panel.ticks_run() > before);
        check(
            "a call into the panel wakes its tick (the wake's dispatch source)",
            woke,
            String::new(),
        );
        let before = panel.ticks_run();
        panel.new_tab();
        // SAFETY: an extern static AppKit defines.
        let tracking = unsafe { NSEventTrackingRunLoopMode };
        let woke = spin_in(tracking, Duration::from_secs(60), || panel.ticks_run() > before);
        check(
            "and also while a menu is open or the window resizes",
            woke,
            String::new(),
        );

        // nvim starts listening.
        let listener = UnixListener::bind(&nvim_listen).expect("a socket where nvim would listen");
        let (go, gone) = mpsc::channel();
        let nvim = fake_nvim(listener, gone);
        let attached = spin_until(Duration::from_secs(60), || {
            host.life.attach_phase().starts_with("attach requested")
        });
        check(
            "the link is aimed at nvim within a poll of its socket appearing",
            attached,
            host.life.attach_phase(),
        );

        // The editor's poll: on its timer while the link attaches, then on its sockets alone.
        let editor = assembly.editor.clone();
        let band = || assembly.editor_link().map(|(state, _)| state).unwrap_or_default();
        let linked = spin_until(Duration::from_secs(60), || band() == "attached");
        check(
            "the link attaches to an nvim that answers",
            linked,
            format!("{:?}", assembly.editor_link()),
        );
        let settled = spin_until(Duration::from_secs(60), || host.editor_ticking() == Some(false));
        let before = editor.polls_run();
        for _ in 0..64 {
            spin_once();
        }
        let polls = editor.polls_run() - before;
        check(
            "an attached, idle editor is not polled",
            settled && polls == 0 && host.editor_ticking() == Some(false),
            format!("{polls} polls, ticking {:?}", host.editor_ticking()),
        );
        let before = editor.polls_run();
        let mut feed = UnixStream::connect(editor.context_socket().expect("the context feed")).expect("connect");
        feed.write_all(b"{\"v\":1,\"file\":\"/p/a.rs\",\"selection\":null}\n")
            .expect("write");
        drop(feed);
        let read = spin_until(Duration::from_secs(60), || {
            (assembly.editor.context())().is_some_and(|context| context.file == "/p/a.rs")
        });
        check(
            "a feed's connection wakes it, and what it carried is read",
            read && editor.polls_run() > before,
            format!("{} polls", editor.polls_run() - before),
        );
        let back = spin_until(Duration::from_secs(60), || host.editor_ticking() == Some(false));
        check(
            "then it waits on its sockets again",
            back,
            format!("ticking {:?}", host.editor_ticking()),
        );
        go.send(()).unwrap();
        nvim.join().unwrap();
        let detached = spin_until(Duration::from_secs(60), || band() == "detached");
        check(
            "the link's own news wakes it: the editor going is told",
            detached,
            format!("{:?}", assembly.editor_link()),
        );

        // A ticker let go of without `stop` cancels what it armed: libdispatch keeps an uncancelled source alive
        // after its last owner, with a handler that reaches nothing.
        spin_until(Duration::from_secs(60), || host.editor_ticking() == Some(false));
        let sources = host.editor_watch_sources();
        let armed = !sources.is_empty() && sources.iter().all(|s| s.testcancel() == 0);
        host.drop_editor_tick_without_stopping();
        let cancelled = sources.iter().all(|s| s.testcancel() != 0);
        check(
            "a poll dropped without stop cancels its sources",
            armed && cancelled,
            format!("{} sources, armed {armed}, cancelled {cancelled}", sources.len()),
        );

        check(
            "a quit with nothing at work goes straight on",
            host.quit_chord(false)
                && host.quit_chord(true)
                && host.quit_requested(QuitSource::Menu)
                && !host.life.quit_asking(),
            format!("{:?}", host.life.quit_log()),
        );
        check("no window yet", host.ns_window().is_none(), String::new());

        let started = Instant::now();
        host.finish("the test's first way out", true);
        let took = started.elapsed();
        host.finish("the test's second way out", false);
        check(
            "the shutdown runs once, for whichever way out came first",
            host.life.finished_via() == Some("the test's first way out"),
            format!("{:?} in {} ms", host.life.finished_via(), took.as_millis()),
        );
        let after = panel.ticks_run();
        check(
            "the shutdown stops the editor's poll",
            host.editor_ticking().is_none(),
            format!("{:?}", host.editor_ticking()),
        );
        check(
            "the shutdown stops the panel's tick",
            !host.panel_ticking() && panel.ticks_run() == after,
            format!("{} ticks after it", panel.ticks_run() - after),
        );
        check(
            "with nvim gone, its socket directory is removed",
            !tmp.path().join("nl").exists(),
            String::new(),
        );
        check(
            "after the shutdown a quit is never held",
            host.quit_requested(QuitSource::CloseButton),
            String::new(),
        );

        // The quit question as it is built: Cancel first and answering Return, Quit second with no key of its own.
        let (alert, cancel) = quit_alert(mtm, "1 turn is running");
        let buttons: Vec<(String, String)> = alert
            .buttons()
            .iter()
            .map(|b| (b.title().to_string(), b.keyEquivalent().to_string()))
            .collect();
        check(
            "the quit question: Cancel answers Return, Quit has no key",
            buttons.len() == 2
                && buttons[0] == ("Cancel".to_owned(), "\r".to_owned())
                && buttons[1].0 == "Quit"
                && !["\r", "\u{1b}"].contains(&buttons[1].1.as_str())
                && std::ptr::eq(
                    Retained::as_ptr(&cancel),
                    Retained::as_ptr(&alert.buttons().objectAtIndex(0)),
                ),
            format!("{buttons:?}"),
        );
        check(
            "its first button is Cancel, any other Quit",
            !quit_chosen(NSAlertFirstButtonReturn) && quit_chosen(NSAlertSecondButtonReturn),
            String::new(),
        );

        // Escape's click: the held button is clicked once, its action finds no borrow held (it takes the button
        // out, as the answer does), and with none held nothing is clicked.
        let slot: Rc<RefCell<Option<Retained<NSButton>>>> = Rc::new(RefCell::new(None));
        let target = Answer::new(mtm, slot.clone());
        let button = NSButton::new(mtm);
        // SAFETY: plain setters on the main thread; the target outlives the button's use below.
        unsafe {
            button.setTarget(Some(&target));
            button.setAction(Some(sel!(answered:)));
        }
        *slot.borrow_mut() = Some(button);
        let clicked = click_held_button(&slot);
        let again = click_held_button(&slot);
        check(
            "Escape clicks the held Cancel once, and nothing once it is answered",
            clicked && !again && target.ivars().clicks.get() == 1 && slot.borrow().is_none(),
            format!(
                "clicked {clicked}, again {again}, answers {}",
                target.ivars().clicks.get()
            ),
        );

        println!("{failed} failed");
        std::process::exit(if failed == 0 { 0 } else { 1 });
    }

    pub struct AnswerIvars {
        slot: Rc<RefCell<Option<Retained<NSButton>>>>,
        clicks: Cell<u32>,
    }

    define_class!(
        /// What the question's completion does with the slot: takes the button out.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = AnswerIvars]
        struct Answer;

        impl Answer {
            #[unsafe(method(answered:))]
            fn answered(&self, _sender: Option<&AnyObject>) {
                self.ivars().clicks.set(self.ivars().clicks.get() + 1);
                self.ivars().slot.borrow_mut().take();
            }
        }
    );

    impl Answer {
        fn new(mtm: MainThreadMarker, slot: Rc<RefCell<Option<Retained<NSButton>>>>) -> Retained<Answer> {
            let this = Answer::alloc(mtm).set_ivars(AnswerIvars {
                slot,
                clicks: Cell::new(0),
            });
            // SAFETY: NSObject's `init` on a freshly allocated object.
            unsafe { msg_send![super(this), init] }
        }
    }
}
