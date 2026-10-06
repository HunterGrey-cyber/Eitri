//! The GTK half of the editor's push sockets: one `glib` timer per window that drives
//! `eitri_core::editor_feeds::EditorFeeds`, and the handlers each feed's consumer hands over.
//!
//! The sockets, their readers and the cadence each is read at live in `eitri-core`; what stays here
//! is the part that names `glib`. The `listen_*` methods mirror what the feeds' own `listen`
//! functions were, so a call site changes only its callee.

use std::cell::RefCell;
use std::rc::Rc;
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

pub(crate) struct FeedPump {
    feeds: RefCell<EditorFeeds>,
    theme: RefCell<Option<Handler<NvimThemePayload>>>,
    keys: RefCell<Option<Handler<NvimReport>>>,
    pane: RefCell<Option<Handler<PaneMessage>>>,
}

impl FeedPump {
    /// Installs the window's one timer. Nothing is read until a `listen_*` method adds a reader.
    pub(crate) fn start() -> Rc<FeedPump> {
        let pump = Rc::new(FeedPump {
            feeds: RefCell::new(EditorFeeds::new()),
            theme: RefCell::new(None),
            keys: RefCell::new(None),
            pane: RefCell::new(None),
        });
        let ticking = pump.clone();
        glib::timeout_add_local(EditorFeeds::TICK, move || {
            ticking.tick();
            glib::ControlFlow::Continue
        });
        pump
    }

    /// One poll, then the handlers: the theme's, the keys', then each pane message in order. The poll's
    /// borrow of the feeds ends before any handler runs.
    fn tick(&self) {
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
        self.feeds
            .borrow_mut()
            .add_context(EditorContextReader::new(listener), scratch_dir, Instant::now())
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
    }
}
