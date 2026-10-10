//! The panel's tick as the Mac window drives it, without AppKit: the assembled panel (the real `init.lua`, trust
//! gate, saved tabs and review under a scratch state home) run by `PanelPump` in a poll(2) loop, which is what the
//! AppKit half's dispatch source and one-shot timer do. Each duty the window drives is checked here: the first
//! tick at the start, nothing while idle, a call into the panel, a page message, a new editor context read by the
//! editor's own poll, the typing cadence's deadline with no wake at all, and the close stopping it.
//!
//! One test, because it owns the process: the panel writes under the state home, and the account and settings
//! tiers are process-wide.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use eitri_mac::assembly::{assemble, HostActions};
use eitri_mac::editor::Editor;
use eitri_mac::keys::Pane;
use eitri_mac::panel_pump::{PanelPump, Timer};
use eitri_panel::panel_document::PanelDocument;
use eitri_panel::panel_page::{PageSurface, PanelHost, PanelPage};

static DOCUMENT: LazyLock<PanelDocument> = LazyLock::new(|| {
    PanelDocument::new("<!doctype html><html><head></head><body><script type=\"module\">1</script></body></html>")
});

struct NoLinks;
impl PanelHost for NoLinks {
    fn open_external(&self, _url: &str) {}
}

struct NoActions;
impl HostActions for NoActions {
    fn focus(&self, _pane: Pane) -> bool {
        true
    }
    fn toast(&self, _text: &str) {}
    fn show_editor(&self) {}
    fn set_panel_hidden(&self, _hidden: bool) {}
    fn panel_visible(&self) -> bool {
        true
    }
}

#[derive(Default)]
struct Recording(RefCell<Vec<String>>);
impl PageSurface for Recording {
    fn send(&self, envelope_json: &str) {
        self.0.borrow_mut().push(envelope_json.to_owned());
    }
    fn is_visible(&self) -> bool {
        true
    }
    fn load_document(&self, _html: &str, _base_uri: &str) {}
    fn set_background(&self, _rgb: [u8; 3]) {}
}

impl Recording {
    fn len(&self) -> usize {
        self.0.borrow().len()
    }
    fn any_since(&self, from: usize, needle: &str) -> bool {
        self.0.borrow()[from..].iter().any(|e| e.contains(needle))
    }
}

/// The window's loop: wait on the wake's descriptor until the armed timer is due, and do what each call says.
/// `between` runs once per round, as the window's other sources would.
struct Loop<'a> {
    pump: &'a PanelPump,
    armed: Option<Instant>,
    stopped: bool,
}

impl Loop<'_> {
    fn act(&mut self, timer: Timer) {
        match timer {
            Timer::Arm(at) => self.armed = Some(at),
            Timer::Keep => {}
            Timer::Stop => {
                self.armed = None;
                self.stopped = true;
            }
        }
    }

    fn run(&mut self, run: Duration, mut between: impl FnMut()) {
        let fd = self.pump.wake_fd().expect("a wake");
        let end = Instant::now() + run;
        while !self.stopped {
            between();
            let now = Instant::now();
            if now >= end {
                return;
            }
            let until = self.armed.map_or(end, |at| at.min(end));
            let wait_ms = until.saturating_duration_since(now).as_millis().min(i32::MAX as u128) as i32;
            let mut polled = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one live `pollfd` for the call.
            let ready = unsafe { libc::poll(&mut polled, 1, wait_ms) };
            if ready > 0 {
                let timer = self.pump.on_wake(Instant::now());
                self.act(timer);
            }
            if self.armed.is_some_and(|at| Instant::now() >= at) {
                self.armed = None;
                let timer = self.pump.on_timer(Instant::now());
                self.act(timer);
            }
        }
    }
}

impl Loop<'_> {
    /// As [`run`](Self::run) with the wake's descriptor left unread: the tick runs only when the armed timer is due.
    fn run_on_the_timer_alone(&mut self, run: Duration) {
        let end = Instant::now() + run;
        while !self.stopped {
            let now = Instant::now();
            if now >= end {
                return;
            }
            let Some(at) = self.armed.filter(|at| *at < end) else {
                std::thread::sleep(end - now);
                return;
            };
            std::thread::sleep(at.saturating_duration_since(now));
            self.armed = None;
            let timer = self.pump.on_timer(Instant::now());
            self.act(timer);
        }
    }
}

#[test]
fn every_duty_of_the_panels_tick_is_driven_by_the_macs_pump() {
    let tmp = tempfile::tempdir().expect("a scratch directory");
    let state_home = tmp.path().join("state");
    let config_dir = tmp.path().join("config");
    let project = tmp.path().join("project");
    for dir in [&state_home, &config_dir, &project] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::env::set_var("XDG_STATE_HOME", &state_home);
    std::env::remove_var("VERDANDI_CLAUDE_ACCOUNT");
    std::env::remove_var("EITRI_AGENT_BACKEND");
    std::fs::write(
        config_dir.join("init.lua"),
        "eitri.config.set(\"keymap.from_tmux\", \"off\")\neitri.config.set(\"agent.typing_cadence_hz\", 2)\n",
    )
    .unwrap();

    let surface = Rc::new(Recording::default());
    let page = Rc::new(PanelPage::new(surface.clone()));
    // A short base: the editor's sockets must fit macOS's cap.
    let sockets = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let editor = Rc::new(Editor::new(sockets.path().join("n.sock")).expect("an editor"));
    let assembly = assemble(
        &project,
        &config_dir,
        Some(page),
        Rc::new(NoLinks),
        &DOCUMENT,
        editor.clone(),
        Rc::new(NoActions),
    )
    .unwrap_or_else(|why| panic!("assemble: {why}"));
    let panel = assembly.panel.clone();
    panel.load_first_document();
    panel.page_message(r#"{"type":"ready","request_id":"r1"}"#);

    let pump = PanelPump::new(panel.clone());
    let mut looped = Loop {
        pump: &pump,
        armed: None,
        stopped: false,
    };
    // The first tick runs at once: what the window set up before its loop (the hello the ready asked for) is sent.
    let first = pump.start(Instant::now());
    assert!(matches!(first, Timer::Arm(_)), "{first:?}");
    looped.act(first);
    looped.run(Duration::from_millis(100), || {});
    assert!(panel.ticks_run() >= 1, "the first tick ran");
    assert!(
        surface.any_since(0, "\"kind\":\"hello\""),
        "and answered the page's ready"
    );

    // Settled (the start's trust check and the like answer on workers meanwhile, and are polled until they do),
    // the panel is not ticked.
    looped.run(Duration::from_secs(2), || {});
    let before = panel.ticks_run();
    looped.run(Duration::from_secs(1), || {});
    let idle = panel.ticks_run() - before;
    assert!(idle <= 1, "an idle panel ticked {idle} times in a second");

    // A call the window makes into the panel wakes it, and the tick sends what it changed.
    let (before, sent) = (panel.ticks_run(), surface.len());
    let mut once = Some(());
    looped.run(Duration::from_millis(300), || {
        if once.take().is_some() {
            panel.new_tab();
        }
    });
    assert!(panel.ticks_run() > before, "a call woke the tick");
    assert!(
        surface.any_since(sent, "\"kind\":\"tabs\""),
        "the new tab reached the page"
    );

    // A page message wakes it.
    let before = panel.ticks_run();
    let mut once = Some(());
    looped.run(Duration::from_millis(300), || {
        if once.take().is_some() {
            panel.page_message(r#"{"type":"ready","request_id":"r2"}"#);
        }
    });
    assert!(panel.ticks_run() > before, "a page message woke the tick");

    // A new editor context, read by the editor's own poll (the window's editor tick), raises the panel's wake:
    // nothing else would, and its next send would carry a stale file and selection. Looked at on the wake itself,
    // outside the loop, since other work can tick the panel meanwhile.
    looped.run(Duration::from_secs(1), || {});
    assert_eq!(
        panel.take_wake(Instant::now()),
        None,
        "nothing raised before the context"
    );
    let path = editor.context_socket().expect("the context feed").to_path_buf();
    let mut stream = std::os::unix::net::UnixStream::connect(&path).expect("connect to the context feed");
    stream
        .write_all(b"{\"v\":1,\"file\":\"/p/a.rs\",\"selection\":null}\n")
        .unwrap();
    drop(stream);
    // The editor's poll as the window's editor tick runs it (`Assembly::poll_editor`), one event at a time, so
    // the wake is seen to come from the context and not from another event of the same poll.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut woke = None;
    while woke.is_none() && Instant::now() < deadline {
        for event in editor.poll(Instant::now()) {
            let context = matches!(event, eitri_mac::editor::EditorEvent::ContextChanged);
            let _ = panel.take_wake(Instant::now());
            assembly.handle(event);
            let due = panel.take_wake(Instant::now());
            if context {
                woke = Some(due.expect("the editor's context woke the panel"));
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let due = woke.expect("the editor said its context changed");
    // The tick that wake asks for, as the pump would run it.
    looped.act(Timer::Arm(due));
    looped.run(Duration::from_millis(200), || {});

    // The typing cadence: a key in the editor opens a typing window, which the page is told of; its end is a
    // deadline the tick named, reached by the timer alone (nothing raises the wake for it).
    assembly.report_keys(Pane::Editor, true);
    let sent = surface.len();
    let mut once = Some(());
    looped.run(Duration::from_millis(200), || {
        if once.take().is_some() {
            panel.note_editor_key();
        }
    });
    assert!(surface.any_since(sent, "\"typing\":true"), "the typing window was told");
    assert!(looped.armed.is_some(), "and its end is armed");
    // From here the wake is not listened to: only the armed timer can tick the panel, so the end can be told only
    // if its deadline was armed, and nothing may have raised the wake meanwhile.
    looped.run_on_the_timer_alone(Duration::from_secs(2));
    assert!(
        surface.any_since(sent, "\"typing\":false"),
        "its end was told on the timer"
    );
    assert_eq!(
        panel.take_wake(Instant::now()),
        None,
        "nothing raised the wake for the typing window's end"
    );

    // The close stops the pump for good.
    editor.shutdown();
    let watch = panel.shutdown();
    looped.run(Duration::from_millis(300), || {});
    assert!(looped.stopped, "the close stopped the pump");
    assert_eq!(pump.on_wake(Instant::now()), Timer::Stop);
    eitri_mac::quit::wait_for_the_close(watch);
}
