//! The panel's tick as the Mac window drives it, without AppKit: the assembled panel (the real `init.lua`, trust
//! gate, saved tabs and review under a scratch state home) run by `PanelPump` with an advancing clock, driving the same actions the
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

/// The host's loop with an explicit clock. Worker completion is observed with a bounded hang guard.
struct Loop<'a> {
    pump: &'a PanelPump,
    armed: Option<Instant>,
    stopped: bool,
    timer_ticks: u64,
    now: Instant,
    clock: eitri_panel::panel_pacer::TestClock,
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

    fn timer(&mut self) {
        let at = self.armed.take().expect("a timer is armed");
        self.now = self.now.max(at);
        self.clock.set(self.now);
        self.timer_ticks += 1;
        self.act(self.pump.on_timer(self.now));
    }

    fn step(&mut self) {
        self.act(self.pump.on_wake(self.now));
        if self.armed.is_some() {
            self.timer();
        }
    }

    fn until(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        let guard = Instant::now() + Duration::from_secs(60);
        loop {
            self.step();
            if done(self) {
                return;
            }
            assert!(Instant::now() < guard, "never observed {what}");
            std::thread::yield_now();
        }
    }

    fn idle(&mut self) {
        self.until("the idle pump", |l| l.armed.is_none());
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
    panel.load_document(eitri_panel::agent_panel::ViewId::BASE);
    panel.page_message(
        eitri_panel::agent_panel::ViewId::BASE,
        r#"{"type":"ready","request_id":"r1"}"#,
    );

    let pump = PanelPump::new(panel.clone());
    let now = Instant::now();
    let mut looped = Loop {
        pump: &pump,
        armed: None,
        stopped: false,
        timer_ticks: 0,
        now,
        clock: eitri_panel::panel_pacer::TestClock::new(now),
    };
    // The first tick runs at once: what the window set up before its loop (the hello the ready asked for) is sent.
    let first = pump.start(looped.now);
    assert!(matches!(first, Timer::Arm(_)), "{first:?}");
    looped.act(first);
    looped.until("the first tick", |_| panel.ticks_run() >= 1);
    assert!(panel.ticks_run() >= 1, "the first tick ran");
    assert!(
        surface.any_since(0, "\"kind\":\"hello\""),
        "and answered the page's ready"
    );

    // Settled (the start's trust check and the like answer on workers meanwhile, and are polled until they do),
    // the panel is not ticked.
    looped.idle();
    let before = panel.ticks_run();
    looped.now += Duration::from_secs(3600);
    looped.clock.set(looped.now);
    looped.step();
    assert!(looped.armed.is_none());
    assert_eq!(panel.ticks_run(), before, "idle leaves no timer to fire");

    // A call the window makes into the panel wakes it, and the tick sends what it changed.
    let (before, sent) = (panel.ticks_run(), surface.len());
    panel.new_tab();
    looped.until("the new tab", |_| surface.any_since(sent, "\"kind\":\"tabs\""));
    assert!(panel.ticks_run() > before, "a call woke the tick");
    assert!(
        surface.any_since(sent, "\"kind\":\"tabs\""),
        "the new tab reached the page"
    );

    // A page message wakes it.
    let before = panel.ticks_run();
    panel.page_message(
        eitri_panel::agent_panel::ViewId::BASE,
        r#"{"type":"ready","request_id":"r2"}"#,
    );
    looped.until("the page message's tick", |_| panel.ticks_run() > before);
    assert!(panel.ticks_run() > before, "a page message woke the tick");

    // A new editor context, read by the editor's own poll (the window's editor tick), raises the panel's wake:
    // nothing else would, and its next send would carry a stale file and selection. Looked at on the wake itself,
    // outside the loop, since other work can tick the panel meanwhile.
    looped.idle();
    assert_eq!(panel.take_wake(looped.now), None, "nothing raised before the context");
    let path = editor.context_socket().expect("the context feed").to_path_buf();
    let mut stream = std::os::unix::net::UnixStream::connect(&path).expect("connect to the context feed");
    stream
        .write_all(b"{\"v\":1,\"file\":\"/p/a.rs\",\"selection\":null}\n")
        .unwrap();
    drop(stream);
    // The editor's poll as the window's editor tick runs it (`Assembly::poll_editor`), one event at a time, so
    // the wake is seen to come from the context and not from another event of the same poll.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut woke = None;
    while woke.is_none() && Instant::now() < deadline {
        for event in editor.poll(Instant::now()) {
            let context = matches!(event, eitri_mac::editor::EditorEvent::ContextChanged);
            let _ = panel.take_wake(looped.now);
            assembly.handle(event);
            let due = panel.take_wake(looped.now);
            if context {
                woke = Some(due.expect("the editor's context woke the panel"));
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let due = woke.expect("the editor said its context changed");
    // The tick that wake asks for, as the pump would run it.
    let before = panel.ticks_run();
    looped.act(Timer::Arm(due));
    looped.until("the context tick", |_| panel.ticks_run() > before);

    // The typing cadence: a key in the editor opens a typing window, which the page is told of; its end is a
    // deadline the tick named, reached by the timer alone (nothing raises the wake for it).
    assembly.report_keys(Pane::Editor, true);
    let sent = surface.len();
    panel.note_editor_key();
    looped.until("the typing window", |_| surface.any_since(sent, "\"typing\":true"));
    assert!(surface.any_since(sent, "\"typing\":true"), "the typing window was told");
    assert!(looped.armed.is_some(), "and its end is armed");
    // From here the wake is not listened to: only the armed timer can tick the panel, so the end can be told only
    // if its deadline was armed. A background thread may raise the wake meanwhile, which is not read here, so
    // what is checked is that the end came on at least one timer tick and that no tick ran that the loop did not
    // count as the timer's (a tick the panel ran on its own, or one this loop ran for another reason).
    let ticks = panel.ticks_run();
    let timer_ticks = looped.timer_ticks;
    for _ in 0..1000 {
        if surface.any_since(sent, "\"typing\":false") {
            break;
        }
        looped.timer();
    }
    assert!(
        surface.any_since(sent, "\"typing\":false"),
        "its end was told on the timer"
    );
    assert!(looped.timer_ticks > timer_ticks, "the end was reached by a timer tick");
    assert_eq!(
        panel.ticks_run() - ticks,
        looped.timer_ticks - timer_ticks,
        "a tick in the interval was not the timer's"
    );

    // The close stops the pump for good.
    editor.shutdown();
    let watch = panel.shutdown();
    looped.until("the stopped pump", |l| l.stopped);
    assert!(looped.stopped, "the close stopped the pump");
    assert_eq!(pump.on_wake(Instant::now()), Timer::Stop);
    eitri_mac::quit::wait_for_the_close(watch);
}
