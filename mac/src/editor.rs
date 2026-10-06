//! The panel's link to the nvim that Neovide runs, in this process: the editor's four push sockets (theme,
//! keys, editor context, pane switch), the companion link that installs Eitri's glue into that nvim, and the
//! small state the two share. It is the companion window's editor half without the control socket, the
//! window-manager runner or a second attach source: Neovide's nvim is the only editor.
//!
//! Nothing here blocks or owns a timer. The host calls [`Editor::poll`] every [`TICK`] and acts on the events
//! it returns, so a test drives it with the times it makes up.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use eitri_core::companion::attach::{BandLink, LinkState};
use eitri_core::companion::link::CompanionLink;
use eitri_core::companion::{Sockets, SCRATCH_CALL_LUA};
use eitri_core::editor_context::feed::{EditorContextFeed, EditorContextReader};
use eitri_core::editor_context::{self, ContextSource};
use eitri_core::editor_feeds::EditorFeeds;
use eitri_core::editor_rpc::EditorRpc;
use eitri_core::layout::Direction;
use eitri_core::nvim_keys::feed::{NvimKeysFeed, NvimKeysReader};
use eitri_core::nvim_keys::NvimReport;
use eitri_core::pane_switch::{self, PaneMessage, PaneSwitchChannel, PaneSwitchReader};
use eitri_core::review_editor::Owner;
use eitri_core::scratch::{ScratchDir, ScratchRequest};
use eitri_core::theme::feed::{ThemeFeed, ThemePayloadReader};
use eitri_core::theme::payload::NvimThemePayload;

/// How often the host calls [`Editor::poll`]: the finest feed cadence (the pane switch's).
pub const TICK: Duration = EditorFeeds::TICK;

/// How long after [`Editor::attach`] a failed connect is tried again before it is reported. Neovide
/// starts nvim and returns; nvim creates its socket file a moment before it listens, and the link driver
/// never retries a refused connect on its own.
pub const ATTACH_RETRY_FOR: Duration = Duration::from_secs(2);

/// What one [`Editor::poll`] found, for the host to act on once the poll has returned.
pub enum EditorEvent {
    /// nvim's own navigator ran out of windows toward `Direction`: the keys continue beyond the editor.
    PaneSwitch(Direction),
    /// The editor's colourscheme.
    Theme(NvimThemePayload),
    /// The editor's own keys changed.
    Keys(NvimReport),
    /// Where the panel stands with the editor; the first poll always says.
    LinkChanged(BandLink),
    /// Draft edits waiting on the editor must end (it went away or was swapped).
    CancelDrafts,
}

pub struct Editor {
    nvim_listen: PathBuf,
    feeds: RefCell<EditorFeeds>,
    link: Rc<CompanionLink>,
    /// Kept for their cleanup; the readers hold the listeners.
    theme_feed: Option<ThemeFeed>,
    keys_feed: Option<NvimKeysFeed>,
    context_feed: Option<EditorContextFeed>,
    pane_switch: Option<PaneSwitchChannel>,
    scratch: RefCell<Option<ScratchDir>>,
    scratch_path: Option<PathBuf>,
    /// Whether an editor is attached: the context is nothing while it is not.
    attached: Rc<Cell<bool>>,
    context: ContextSource,
    reset_context: Rc<dyn Fn()>,
    first_poll: Cell<bool>,
    next_link_poll: Cell<Option<Instant>>,
    shut: Cell<bool>,
    /// While set and in the future, a failed attach is tried again instead of reported.
    retry_until: Cell<Option<Instant>>,
}

impl Editor {
    /// Binds the four feeds and builds the sockets the glue will write to. Does not attach. `Err` only for a
    /// path that is not absolute; a feed that cannot bind is logged and left out of the install.
    pub fn new(nvim_listen: PathBuf) -> Result<Editor, String> {
        if !nvim_listen.is_absolute() {
            return Err(format!(
                "the editor's socket path {} is not absolute",
                nvim_listen.display()
            ));
        }
        let mut theme_feed = ThemeFeed::new();
        let mut context_feed = EditorContextFeed::new();
        let mut keys_feed = NvimKeysFeed::new();
        let scratch = ScratchDir::new();
        let scratch_path = scratch.as_ref().map(|dir| dir.path().to_path_buf());
        // Without the tmux shim: nvim's environment is Neovide's own, and nothing may change in it.
        pane_switch::sweep_stale_dirs();
        let mut pane_switch = PaneSwitchChannel::bind_without_shim();
        for (name, bound) in [
            ("theme", theme_feed.is_some()),
            ("editor context", context_feed.is_some()),
            ("keys", keys_feed.is_some()),
            ("pane switch", pane_switch.is_some()),
        ] {
            if !bound {
                eprintln!("[editor] the {name} socket could not be bound; that part is not installed");
            }
        }
        let sockets = Sockets {
            editor_context: context_feed.as_ref().map(|feed| feed.socket_path().to_path_buf()),
            theme: theme_feed.as_ref().map(|feed| feed.socket_path().to_path_buf()),
            keys: keys_feed.as_ref().map(|feed| feed.socket_path().to_path_buf()),
            pane_switch: pane_switch.as_ref().map(|channel| channel.socket_path().to_path_buf()),
        };

        // Every reader is added at once: the consumer of each is `poll`'s caller.
        let now = Instant::now();
        let mut feeds = EditorFeeds::new();
        let attached = Rc::new(Cell::new(false));
        let (source, reset_context): (ContextSource, Rc<dyn Fn()>) =
            match context_feed.as_mut().and_then(|feed| feed.take_listener()) {
                Some(listener) => feeds.add_context(EditorContextReader::new(listener), scratch_path.clone(), now),
                None => (Rc::new(|| None), Rc::new(|| {})),
            };
        if let Some(listener) = theme_feed.as_mut().and_then(|feed| feed.take_listener()) {
            feeds.add_theme(ThemePayloadReader::new(listener), now);
        }
        if let Some(listener) = keys_feed.as_mut().and_then(|feed| feed.take_listener()) {
            feeds.add_keys(NvimKeysReader::new(listener), now);
        }
        if let Some(listener) = pane_switch.as_mut().and_then(|channel| channel.take_listener()) {
            feeds.add_pane_switch(PaneSwitchReader::new(listener), now);
        }
        Ok(Editor {
            nvim_listen,
            feeds: RefCell::new(feeds),
            link: Rc::new(CompanionLink::new(None, sockets)),
            theme_feed,
            keys_feed,
            context_feed,
            pane_switch,
            scratch: RefCell::new(scratch),
            scratch_path,
            context: editor_context::gated(source, attached.clone()),
            attached,
            reset_context,
            first_poll: Cell::new(true),
            next_link_poll: Cell::new(None),
            shut: Cell::new(false),
            retry_until: Cell::new(None),
        })
    }

    /// Aim the link at the nvim Neovide started with `--listen`. A connect that fails in the first
    /// [`ATTACH_RETRY_FOR`] is tried again (the socket may exist a moment before nvim listens).
    pub fn attach(&self) {
        self.retry_until.set(Some(Instant::now() + ATTACH_RETRY_FOR));
        self.link.attach(self.nvim_listen.clone());
    }

    /// The feeds every [`TICK`], the link every [`CompanionLink::POLL_INTERVAL`]. Empty once shut down.
    pub fn poll(&self, now: Instant) -> Vec<EditorEvent> {
        if self.shut.get() {
            return Vec::new();
        }
        let mut events = Vec::new();
        if self.first_poll.replace(false) {
            // The state a window is born in is told at once, so the band says what is true from the start.
            events.push(EditorEvent::LinkChanged(self.link.snapshot().1));
        }
        if self.next_link_poll.get().is_none_or(|due| now >= due) {
            self.next_link_poll.set(Some(now + CompanionLink::POLL_INTERVAL));
            self.poll_link(now, &mut events);
        }
        // The poll's borrow of the feeds ends before the events are handed back.
        let found = self.feeds.borrow_mut().poll(now);
        if let Some(payload) = found.theme {
            events.push(EditorEvent::Theme(payload));
        }
        if let Some(report) = found.keys {
            events.push(EditorEvent::Keys(report));
        }
        for message in found.pane {
            if let PaneMessage::Direction(letter) = message {
                match pane_switch::letter_direction(letter) {
                    Some(direction) => events.push(EditorEvent::PaneSwitch(direction)),
                    None => println!("[pane_switch] unknown direction {letter:?}, ignoring"),
                }
            }
        }
        events
    }

    fn poll_link(&self, now: Instant, events: &mut Vec<EditorEvent>) {
        let poll = self.link.poll(now);
        for line in &poll.logs {
            println!("{line}");
        }
        if let Some((state, band, _peer)) = poll.changed {
            let was = self.attached.get();
            let is = matches!(state, LinkState::Attached { .. });
            self.attached.set(is);
            // A new editor starts empty: forget the last one's file and selection as a connect begins and as
            // an attached editor goes. Not on arriving at `Attached`: the install makes the editor send its
            // first context, and that line may be read just before.
            if matches!(state, LinkState::Connecting { .. }) || (was && !is) {
                (self.reset_context)();
            }
            if is {
                self.retry_until.set(None);
            }
            println!("[editor] link: {} {}", band.state, band.text);
            match &state {
                LinkState::Failed { addr, why } if self.retry_until.get().is_some_and(|until| now < until) => {
                    println!("[editor] attach failed ({why}); trying again");
                    self.link.attach(addr.clone());
                }
                _ => events.push(EditorEvent::LinkChanged(band)),
            }
        }
        if poll.cancel_drafts {
            events.push(EditorEvent::CancelDrafts);
        }
    }

    /// The editor's file and selection, nothing while no editor is attached.
    pub fn context(&self) -> ContextSource {
        self.context.clone()
    }

    pub fn rpc(&self) -> Rc<dyn EditorRpc> {
        self.link.clone()
    }

    pub fn review_owner(&self) -> Option<Owner> {
        self.link.review_owner()
    }

    /// The scratch directory for the panel's drafts and views, handed over once.
    pub fn take_scratch(&self) -> Option<ScratchDir> {
        self.scratch.borrow_mut().take()
    }

    /// A draft or a `gf`: the request goes to the editor as one call.
    pub fn scratch_call(&self, request: &ScratchRequest) -> Result<(), String> {
        self.link
            .exec_lua_for("scratch", SCRATCH_CALL_LUA, vec![request.hex().into()])
    }

    /// `keys` in nvim's notation, as typed, through `nvim_input` itself: it is answered while nvim waits
    /// for a character (`f`, `r`, `q`, `<C-w>`, a hit-enter prompt), where a Lua call would be held and run
    /// after the user's next key. Not waited for: `Err` says why nothing was sent (no editor attached).
    pub fn send_keys(&self, keys: &str) -> Result<(), String> {
        self.link.input(keys)
    }

    /// Lets go of the editor and removes what this side made. Idempotent, and never waits on nvim: the
    /// glue's teardown is queued and the connection closes once it is written. The directory of nvim's own
    /// socket is nvim's; the next start sweeps it.
    pub fn shutdown(&self) {
        if self.shut.replace(true) {
            return;
        }
        self.link.shutdown();
        if let Some(channel) = &self.pane_switch {
            channel.cleanup();
        }
        if let Some(feed) = &self.theme_feed {
            feed.cleanup();
        }
        if let Some(feed) = &self.context_feed {
            feed.cleanup();
        }
        if let Some(feed) = &self.keys_feed {
            feed.cleanup();
        }
        if let Some(path) = &self.scratch_path {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    /// The socket nvim listens on.
    pub fn nvim_listen(&self) -> &Path {
        &self.nvim_listen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> Editor {
        Editor::new(std::env::temp_dir().join("eitri-mac-never-bound")).expect("an absolute path")
    }

    #[test]
    fn a_relative_socket_path_is_refused() {
        assert!(Editor::new(PathBuf::from("nvim-listen")).is_err());
    }

    #[test]
    fn a_new_editor_reports_no_editor_first_and_gives_no_context() {
        let editor = editor();
        let first = editor.poll(Instant::now());
        match first.as_slice() {
            [EditorEvent::LinkChanged(band)] => assert_eq!(band.state, "none"),
            other => panic!("expected the one first band, got {} events", other.len()),
        }
        assert!((editor.context())().is_none());
        assert!(editor.poll(Instant::now()).is_empty(), "nothing changed since");
        editor.shutdown();
    }

    #[test]
    fn shutdown_removes_its_directories() {
        let editor = editor();
        let sockets = [
            editor.theme_feed.as_ref().map(|feed| feed.socket_path()),
            editor.keys_feed.as_ref().map(|feed| feed.socket_path()),
            editor.context_feed.as_ref().map(|feed| feed.socket_path()),
            editor.pane_switch.as_ref().map(|channel| channel.socket_path()),
        ];
        let mut gone = Vec::new();
        for socket in sockets {
            let parent = socket.and_then(|path| path.parent()).expect("a bound socket");
            assert!(parent.exists());
            gone.push(parent.to_path_buf());
        }
        let scratch = editor.scratch_path.clone().expect("a scratch directory");
        assert!(scratch.exists());
        editor.shutdown();
        editor.shutdown();
        for dir in gone.iter().chain([&scratch]) {
            assert!(!dir.exists(), "{} is still there", dir.display());
        }
        assert!(editor.poll(Instant::now()).is_empty(), "a shut editor polls nothing");
    }

    #[test]
    fn send_keys_without_an_editor_says_why() {
        let editor = editor();
        let error = editor.send_keys("<C-l>").expect_err("nothing is attached");
        assert!(error.contains("not connected"), "{error}");
        editor.shutdown();
    }

    #[test]
    fn the_scratch_directory_is_handed_over_once() {
        let editor = editor();
        assert!(editor.take_scratch().is_some());
        assert!(editor.take_scratch().is_none());
        editor.shutdown();
    }
}
