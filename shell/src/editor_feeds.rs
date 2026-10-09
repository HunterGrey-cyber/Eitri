//! The GTK half of the editor's push sockets: one pump per window that drives
//! `eitri_core::editor_feeds::EditorFeeds`, and the handlers each feed's consumer hands over. The
//! pump polls on a `glib` timer only while a feed has something to read, and otherwise waits on the
//! listening sockets, so an idle window is not woken by it.
//!
//! The sockets, their readers and the cadence each is read at live in `eitri-core`; what stays here
//! is the part that names `glib`. The `listen_*` methods mirror what the feeds' own `listen`
//! functions were, so a call site changes only its callee.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::time::Instant;

use gtk4::glib;

use eitri_core::editor_context::feed::{EditorContextFeed, EditorContextReader};
use eitri_core::editor_context::ContextSource;
use eitri_core::editor_feeds::EditorFeeds;
use eitri_core::nvim_keys::feed::{NvimKeysFeed, NvimKeysReader};
use eitri_core::nvim_keys::NvimReport;
use eitri_core::pane_switch::{PaneMessage, PaneSwitchChannel, PaneSwitchReader};
use eitri_core::theme::feed::{ThemeFeed, ThemePayloadReader};
use eitri_core::theme::payload::NvimThemePayload;

/// What a feed's consumer wants done with each thing that arrives. Held as an `Rc` so the pump can
/// let go of its slot before it calls the handler, and a handler may therefore reach the pump again.
type Handler<T> = Rc<dyn Fn(T)>;

/// What wakes the pump next.
enum Wake {
    /// Its timer, every [`EditorFeeds::TICK`], while a feed has something left to read. The timer
    /// ends itself (`Break`) when the feeds are quiet, so nothing removes it from outside.
    Timer,
    /// A watch on each listening socket, while every feed is quiet: an idle window is not woken at
    /// all until nvim connects to one of them.
    Watches(Vec<glib::SourceId>),
    /// A poll is running; what wakes the pump next is decided when it ends.
    Polling,
}

pub(crate) struct FeedPump {
    feeds: RefCell<EditorFeeds>,
    theme: RefCell<Option<Handler<NvimThemePayload>>>,
    keys: RefCell<Option<Handler<NvimReport>>>,
    pane: RefCell<Option<Handler<PaneMessage>>>,
    context: RefCell<Option<Handler<()>>>,
    wake: RefCell<Wake>,
    /// Each timer and watch holds the pump through this, so it lives as long as something can wake it.
    me: Weak<FeedPump>,
    #[cfg(test)]
    ticks: std::cell::Cell<u32>,
}

impl FeedPump {
    /// The window's pump. Nothing is read, and nothing wakes it, until a `listen_*` method adds a
    /// reader.
    pub(crate) fn start() -> Rc<FeedPump> {
        Rc::new_cyclic(|me| FeedPump {
            feeds: RefCell::new(EditorFeeds::new()),
            theme: RefCell::new(None),
            keys: RefCell::new(None),
            pane: RefCell::new(None),
            context: RefCell::new(None),
            wake: RefCell::new(Wake::Watches(Vec::new())),
            me: me.clone(),
            #[cfg(test)]
            ticks: std::cell::Cell::new(0),
        })
    }

    /// The timer fired: poll, and keep the timer only while there is more to read.
    fn on_timer(&self) -> glib::ControlFlow {
        let timer = std::mem::replace(&mut *self.wake.borrow_mut(), Wake::Polling);
        self.tick();
        if self.feeds.borrow().is_quiet() {
            // The timer's id is dropped, not removed: returning `Break` removes it.
            *self.wake.borrow_mut() = self.watches();
            glib::ControlFlow::Break
        } else {
            *self.wake.borrow_mut() = timer;
            glib::ControlFlow::Continue
        }
    }

    /// A listening socket became readable: poll at once, then wait as the feeds' state says.
    fn on_ready(&self) -> glib::ControlFlow {
        let woken = std::mem::replace(&mut *self.wake.borrow_mut(), Wake::Polling);
        if let Wake::Watches(watches) = woken {
            // The one dispatching now included: GLib defers its destruction until it returns.
            for watch in watches {
                watch.remove();
            }
        }
        self.tick();
        self.settle();
        glib::ControlFlow::Break
    }

    /// After a reader was added or a watch fired: watch the sockets if the feeds are quiet, else
    /// poll on the timer. Does nothing while a poll is running; that poll decides when it ends.
    fn rearm(&self) {
        let woken = std::mem::replace(&mut *self.wake.borrow_mut(), Wake::Polling);
        match woken {
            Wake::Watches(watches) => {
                for watch in watches {
                    watch.remove();
                }
                self.settle();
            }
            other => *self.wake.borrow_mut() = other,
        }
    }

    fn settle(&self) {
        let next = if self.feeds.borrow().is_quiet() {
            self.watches()
        } else {
            self.timer()
        };
        *self.wake.borrow_mut() = next;
    }

    fn timer(&self) -> Wake {
        let Some(pump) = self.me.upgrade() else {
            return Wake::Watches(Vec::new());
        };
        glib::timeout_add_local(EditorFeeds::TICK, move || pump.on_timer());
        Wake::Timer
    }

    /// One watch per listening socket. Falls back to the timer if a watch cannot be added (not on the
    /// main context's own thread), as the pump always polled before it could wait.
    fn watches(&self) -> Wake {
        let Some(pump) = self.me.upgrade() else {
            return Wake::Watches(Vec::new());
        };
        let mut watches = Vec::new();
        for fd in self.feeds.borrow().listener_fds() {
            let pump = pump.clone();
            match neovide_editor::fd_watch::add_local(
                fd,
                glib::IOCondition::IN | glib::IOCondition::ERR | glib::IOCondition::HUP,
                glib::ffi::G_PRIORITY_DEFAULT,
                move |_| pump.on_ready(),
            ) {
                Ok(watch) => watches.push(watch),
                Err(neovide_editor::fd_watch::NotOwner) => {
                    for watch in watches {
                        watch.remove();
                    }
                    return self.timer();
                }
            }
        }
        Wake::Watches(watches)
    }

    /// One poll, then the handlers: the theme's, the keys', then each pane message in order. The poll's
    /// borrow of the feeds ends before any handler runs.
    fn tick(&self) {
        #[cfg(test)]
        self.ticks.set(self.ticks.get() + 1);
        let events = self.feeds.borrow_mut().poll(Instant::now());
        if let Some(payload) = events.theme {
            let handler = self.theme.borrow().clone();
            if let Some(handler) = handler {
                handler(payload);
            }
        }
        if let Some(report) = events.keys {
            let handler = self.keys.borrow().clone();
            if let Some(handler) = handler {
                handler(report);
            }
        }
        for message in events.pane {
            let handler = self.pane.borrow().clone();
            if let Some(handler) = handler {
                handler(message);
            }
        }
        if events.context {
            let handler = self.context.borrow().clone();
            if let Some(handler) = handler {
                handler(());
            }
        }
    }

    /// Calls `on_change` each time the context source [`listen_context`](Self::listen_context) returned
    /// answers something new: the panel reads it only when its tick runs, and is woken for it here.
    pub(crate) fn on_context(&self, on_change: impl Fn() + 'static) {
        *self.context.borrow_mut() = Some(Rc::new(move |()| on_change()));
    }

    /// Calls `on_payload` with the newest valid payload each time one is read. `VimEnter` and
    /// `ColorScheme` often fire back to back; only the last one matters.
    ///
    /// Takes the feed's listener, so a second call on the same feed logs and does nothing rather than
    /// adding a second reader that would race the first for every connection.
    pub(crate) fn listen_theme(&self, feed: &mut ThemeFeed, on_payload: impl Fn(NvimThemePayload) + 'static) {
        let Some(listener) = feed.take_listener() else {
            eprintln!("[theme] listen() called twice -- ignoring");
            return;
        };
        *self.theme.borrow_mut() = Some(Rc::new(on_payload));
        self.feeds
            .borrow_mut()
            .add_theme(ThemePayloadReader::new(listener), Instant::now());
        self.rearm();
    }

    /// Calls `on_report` for every new report the reader returns: the caller wants to react the moment
    /// one arrives, not to read a cache at send time.
    pub(crate) fn listen_keys(&self, feed: &mut NvimKeysFeed, on_report: impl Fn(NvimReport) + 'static) {
        let Some(listener) = feed.take_listener() else {
            eprintln!("[nvim-keys] listen() called twice -- the panel keeps its default keys");
            return;
        };
        *self.keys.borrow_mut() = Some(Rc::new(on_report));
        self.feeds
            .borrow_mut()
            .add_keys(NvimKeysReader::new(listener), Instant::now());
        self.rearm();
    }

    /// Starts reading the editor-context feed and returns the source the panel reads, with a way to
    /// forget what it holds. A second call on the same feed logs and returns a source that is always
    /// empty.
    pub(crate) fn listen_context(
        &self,
        feed: &mut EditorContextFeed,
        scratch_dir: Option<std::path::PathBuf>,
    ) -> (ContextSource, Rc<dyn Fn()>) {
        let Some(listener) = feed.take_listener() else {
            eprintln!("[editor-context] listen() called twice -- turns will carry no editor context");
            return (Rc::new(|| None), Rc::new(|| {}));
        };
        let added =
            self.feeds
                .borrow_mut()
                .add_context(EditorContextReader::new(listener), scratch_dir, Instant::now());
        self.rearm();
        added
    }

    /// Calls `on_message` with each direction letter or quit-cancelled generation the shim, or a
    /// cancelled `:confirm qall`, sends. Takes the channel's listener, so a second call logs and does
    /// nothing.
    pub(crate) fn listen_pane_switch(
        &self,
        channel: &mut PaneSwitchChannel,
        on_message: impl Fn(PaneMessage) + 'static,
    ) {
        let Some(listener) = channel.take_listener() else {
            eprintln!("[pane_switch] listen() called twice -- ignoring");
            return;
        };
        *self.pane.borrow_mut() = Some(Rc::new(on_message));
        self.feeds
            .borrow_mut()
            .add_pane_switch(PaneSwitchReader::new(listener), Instant::now());
        self.rearm();
    }
}

/// Wakes `panel` each time the context source `pump` handed it answers something new: the panel reads the
/// context only when its tick runs, and its tick runs only when woken. Both windows call this.
pub(crate) fn wake_panel_on_context(pump: &FeedPump, panel: &eitri_panel::agent_panel::AgentPanelHandle) {
    let panel = panel.clone();
    pump.on_context(move || panel.wake());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    /// Runs glib's default main context for `ms`, or until `done` holds.
    fn iterate_until(ms: u64, done: impl Fn() -> bool) -> bool {
        let context = glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < deadline {
            while context.iteration(false) {}
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }

    fn send(path: &std::path::Path, line: &str) {
        let mut stream = UnixStream::connect(path).expect("connect");
        stream.write_all(format!("{line}\n").as_bytes()).expect("write");
    }

    /// With all four feeds listened to and nothing sent, the pump is not woken at all (it polled 40
    /// times a second, measured); a pane message and a theme still arrive, the pane message without
    /// waiting for a tick, and the pump goes quiet again after them.
    #[test]
    fn an_idle_pump_is_not_woken_and_a_message_still_arrives() {
        let _turn = crate::layout_state::DEFAULT_CONTEXT_TURN
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let context = glib::MainContext::default();
        let _owner = context
            .acquire()
            .expect("glib's default main context, which the other tests on it leave to this one");
        let pump = FeedPump::start();
        let mut theme = ThemeFeed::new().unwrap();
        let mut keys = NvimKeysFeed::new().unwrap();
        let mut context_feed = EditorContextFeed::new().unwrap();
        let mut pane = PaneSwitchChannel::bind_without_shim().unwrap();
        let themes = Rc::new(RefCell::new(Vec::new()));
        let panes = Rc::new(RefCell::new(Vec::new()));
        {
            let themes = themes.clone();
            pump.listen_theme(&mut theme, move |payload| {
                themes.borrow_mut().push(payload.options.colors_name)
            });
        }
        pump.listen_keys(&mut keys, |_| {});
        let (context_source, _) = pump.listen_context(&mut context_feed, None);
        let contexts = Rc::new(std::cell::Cell::new(0));
        {
            let contexts = contexts.clone();
            pump.on_context(move || contexts.set(contexts.get() + 1));
        }
        {
            let panes = panes.clone();
            pump.listen_pane_switch(&mut pane, move |message| panes.borrow_mut().push(message));
        }

        iterate_until(300, || false);
        let before = pump.ticks.get();
        iterate_until(1000, || false);
        assert_eq!(pump.ticks.get() - before, 0, "polls in a second with nothing sent");

        let sent = Instant::now();
        send(pane.socket_path(), "L");
        assert!(iterate_until(2000, || !panes.borrow().is_empty()), "the pane message");
        let waited = sent.elapsed();
        assert_eq!(*panes.borrow(), vec![PaneMessage::Direction('L')]);
        assert!(waited < Duration::from_millis(500), "took {waited:?}");

        send(
            theme.socket_path(),
            r#"{"v":1,"groups":{},"options":{"background":"dark","guifont":"","colors_name":"x"}}"#,
        );
        assert!(iterate_until(2000, || !themes.borrow().is_empty()), "the theme");
        assert_eq!(*themes.borrow(), vec!["x".to_string()]);

        // A new context is said, so the panel that reads it can be woken.
        assert_eq!(contexts.get(), 0);
        send(
            context_feed.socket_path(),
            r#"{"v":1,"file":"/p/a.rs","selection":null}"#,
        );
        assert!(iterate_until(2000, || contexts.get() == 1), "the context change");
        assert_eq!(context_source().map(|c| c.file), Some("/p/a.rs".to_string()));

        iterate_until(300, || false);
        let before = pump.ticks.get();
        iterate_until(500, || false);
        assert_eq!(pump.ticks.get() - before, 0, "quiet again after the messages");
        assert_eq!(contexts.get(), 1, "said once");

        theme.cleanup();
        keys.cleanup();
        context_feed.cleanup();
        pane.cleanup();
    }

    /// A reader added from inside a handler, while a poll runs: the pump picks it up when that poll
    /// ends (it neither sleeps past it nor installs a second timer), delivers what it is sent, and
    /// goes quiet again.
    #[test]
    fn a_reader_added_by_a_handler_is_read_and_the_pump_goes_quiet() {
        let _turn = crate::layout_state::DEFAULT_CONTEXT_TURN
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let context = glib::MainContext::default();
        let _owner = context
            .acquire()
            .expect("glib's default main context, which the other tests on it leave to this one");
        let pump = FeedPump::start();
        let mut pane = PaneSwitchChannel::bind_without_shim().unwrap();
        let theme = Rc::new(RefCell::new(ThemeFeed::new().unwrap()));
        let themes = Rc::new(RefCell::new(Vec::new()));
        {
            let weak = Rc::downgrade(&pump);
            let theme = theme.clone();
            let themes = themes.clone();
            pump.listen_pane_switch(&mut pane, move |_| {
                let Some(pump) = weak.upgrade() else { return };
                let themes = themes.clone();
                pump.listen_theme(&mut theme.borrow_mut(), move |payload| {
                    themes.borrow_mut().push(payload.options.colors_name)
                });
            });
        }
        iterate_until(200, || false);
        send(pane.socket_path(), "L");
        assert!(
            iterate_until(2000, || pump.feeds.borrow().listener_fds().len() == 2),
            "the handler added a reader"
        );
        send(
            theme.borrow().socket_path(),
            r#"{"v":1,"groups":{},"options":{"background":"dark","guifont":"","colors_name":"y"}}"#,
        );
        assert!(
            iterate_until(2000, || !themes.borrow().is_empty()),
            "the added reader is read"
        );
        assert_eq!(*themes.borrow(), vec!["y".to_string()]);

        iterate_until(300, || false);
        let before = pump.ticks.get();
        iterate_until(500, || false);
        assert_eq!(pump.ticks.get() - before, 0, "quiet again");
        theme.borrow_mut().cleanup();
        pane.cleanup();
    }

    /// A context the editor sends wakes the panel, through the hookup both windows make; a window that
    /// dropped the call would leave its panel showing an old context until something else woke it.
    #[test]
    fn a_new_context_wakes_the_panel_in_both_windows() {
        let _turn = crate::layout_state::DEFAULT_CONTEXT_TURN
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let context = glib::MainContext::default();
        let _owner = context.acquire().expect("glib's default main context");
        let pump = FeedPump::start();
        let mut feed = EditorContextFeed::new().unwrap();
        let (source, _) = pump.listen_context(&mut feed, None);
        let panel = eitri_panel::agent_panel::AgentPanelHandle::for_host_test(Rc::new(NoPage));
        wake_panel_on_context(&pump, &panel);
        iterate_until(200, || false);
        let now = Instant::now();
        let _ = panel.take_wake(now);
        assert_eq!(panel.take_wake(now), None, "nothing raised before the context");
        send(feed.socket_path(), r#"{"v":1,"file":"/p/a.rs","selection":null}"#);
        assert!(
            iterate_until(2000, || panel.take_wake(Instant::now()).is_some()),
            "the context woke the panel"
        );
        assert_eq!(source().map(|c| c.file), Some("/p/a.rs".to_string()));
        feed.cleanup();

        // Both windows make the hookup.
        use eitri_core::source_scan as scan;
        let sources = scan::rust_sources(&["shell/src"]);
        for anchor in ["fn build_ui(", "fn build(app: &Application, start: &Start)"] {
            let file = scan::file_with(&sources, anchor);
            assert!(
                scan::code_only(&file.text).contains("wake_panel_on_context(&feed_pump"),
                "{} does not wake its panel on a context change",
                file.path
            );
        }
    }

    struct NoPage;

    impl eitri_panel::panel_page::PageSurface for NoPage {
        fn send(&self, _envelope_json: &str) {}
        fn is_visible(&self) -> bool {
            true
        }
        fn load_document(&self, _html: &str, _base_uri: &str) {}
        fn set_background(&self, _rgb: [u8; 3]) {}
    }
}
