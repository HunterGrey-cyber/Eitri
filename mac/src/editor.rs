//! The panel's link to the nvim that Neovide runs, in this process: the editor's four push sockets (theme,
//! keys, editor context, pane switch), the companion link that installs Eitri's glue into that nvim, and the
//! small state the two share. It is the companion window's editor half without the control socket, the
//! window-manager runner or a second attach source: Neovide's nvim is the only editor.
//!
//! Nothing here blocks or owns a timer. The host calls [`Editor::poll`] every [`TICK`] while [`Editor::wait`]
//! says [`EditorWait::Tick`], and acts on the events it returns, so a test drives it with the times it makes up.
//! Once nothing is left to read and the link has settled, the host instead waits for one of
//! [`Editor::wake_fds`] to become readable: an idle window is not woken by the editor's half at all.

use std::cell::{Cell, RefCell};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
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

/// Waiting for Neovide's nvim to listen before the link is aimed at it: the socket is looked for every
/// [`AttachWait::POLL`]. After [`AttachWait::BUDGET`] (the wait `eitri split` allows its Neovide) the band says
/// so, and the host keeps looking.
#[derive(Debug, Clone, Copy)]
pub struct AttachWait {
    deadline: Instant,
}

/// What [`AttachWait::step`] says to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachStep {
    Wait,
    /// The socket is there: attach now.
    Attach,
    /// It has not appeared within the budget.
    GiveUp,
}

impl AttachWait {
    pub const POLL: Duration = Duration::from_millis(50);
    pub const BUDGET: Duration = Duration::from_secs(15);

    /// The panel's band while waiting.
    pub const WAITING: (&'static str, &'static str) = ("attaching", "waiting for the editor to start\u{2026}");
    /// The panel's band once the budget has passed; the socket is still looked for.
    pub const GAVE_UP: (&'static str, &'static str) = (
        // Still attaching, as the text says: the panel styles `failed` as an error.
        "attaching",
        "the editor has not started listening yet; its link attaches when it does",
    );

    pub fn new(now: Instant) -> AttachWait {
        AttachWait {
            deadline: now + Self::BUDGET,
        }
    }

    /// `listening`: whether nvim's socket exists now.
    pub fn step(&self, now: Instant, listening: bool) -> AttachStep {
        if listening {
            AttachStep::Attach
        } else if now >= self.deadline {
            AttachStep::GiveUp
        } else {
            AttachStep::Wait
        }
    }
}

/// How the host waits for the next [`Editor::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorWait {
    /// Poll again in [`TICK`]: a feed has a connection it is reading, or the link is on its way somewhere (a
    /// connect, an install, a retry).
    Tick,
    /// Poll when one of [`Editor::wake_fds`] is readable, and not before.
    Sockets,
    /// Shut down: nothing to poll any more.
    Stop,
}

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
    /// The context source answers something new; the panel reads it when its tick runs, so it is woken.
    ContextChanged,
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
    /// Read end of the link's wake: a byte arrives each time a link thread has news for `poll`. `None` when the
    /// pair could not be made; then the host keeps ticking.
    link_wake: Option<UnixStream>,
    polls: Cell<u64>,
}

/// The pair the link's threads wake the host through. Both ends never block: a full buffer already says "poll".
fn link_wake_pair() -> Option<(UnixStream, UnixStream)> {
    let (read, write) = UnixStream::pair().ok()?;
    read.set_nonblocking(true).ok()?;
    write.set_nonblocking(true).ok()?;
    Some((read, write))
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
                crate::log_error!("[editor] the {name} socket could not be bound; that part is not installed");
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
            match context_feed.as_mut().and_then(EditorContextFeed::take_listener) {
                Some(listener) => feeds.add_context(EditorContextReader::new(listener), scratch_path.clone(), now),
                None => (Rc::new(|| None), Rc::new(|| {})),
            };
        if let Some(listener) = theme_feed.as_mut().and_then(ThemeFeed::take_listener) {
            feeds.add_theme(ThemePayloadReader::new(listener), now);
        }
        if let Some(listener) = keys_feed.as_mut().and_then(NvimKeysFeed::take_listener) {
            feeds.add_keys(NvimKeysReader::new(listener), now);
        }
        if let Some(listener) = pane_switch.as_mut().and_then(PaneSwitchChannel::take_listener) {
            feeds.add_pane_switch(PaneSwitchReader::new(listener), now);
        }
        let link = Rc::new(CompanionLink::new(None, sockets));
        let link_wake = match link_wake_pair() {
            Some((read, write)) => {
                link.set_wake(Arc::new(move || {
                    let _ = (&write).write(&[1]);
                }));
                Some(read)
            }
            None => {
                crate::log_error!("[editor] the link's wake could not be made; the editor is polled on its tick");
                None
            }
        };
        Ok(Editor {
            nvim_listen,
            feeds: RefCell::new(feeds),
            link,
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
            link_wake,
            polls: Cell::new(0),
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
        self.polls.set(self.polls.get() + 1);
        let mut events = Vec::new();
        if self.first_poll.replace(false) {
            // The state a window is born in is told at once, so the band says what is true from the start.
            events.push(EditorEvent::LinkChanged(self.link.snapshot().1));
        }
        // A link thread's news is read now whatever the link's cadence says: the host may have been asleep.
        let woken = self.drain_link_wake();
        if woken || self.next_link_poll.get().is_none_or(|due| now >= due) {
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
        if found.context {
            events.push(EditorEvent::ContextChanged);
        }
        for message in found.pane {
            if let PaneMessage::Direction(letter) = message {
                match pane_switch::letter_direction(letter) {
                    Some(direction) => events.push(EditorEvent::PaneSwitch(direction)),
                    None => crate::log_line!("[pane_switch] unknown direction {letter:?}, ignoring"),
                }
            }
        }
        events
    }

    /// Whether a link thread said something since the last call; empties the wake.
    fn drain_link_wake(&self) -> bool {
        let Some(read) = &self.link_wake else { return false };
        let mut buf = [0u8; 64];
        let mut any = false;
        while let Ok(n @ 1..) = (&*read).read(&mut buf) {
            any = true;
            if n < buf.len() {
                break;
            }
        }
        any
    }

    /// How the host waits after the poll it just made. Ticking goes on while a feed holds a connection or one is
    /// waiting, while the link is connecting or installing, and while a failed attach may still be retried; else
    /// nothing is due until a socket says so (nvim connects to a feed, or a link thread has news).
    pub fn wait(&self, now: Instant) -> EditorWait {
        if self.shut.get() {
            return EditorWait::Stop;
        }
        let settled = match self.link.snapshot().0 {
            LinkState::NoEditor | LinkState::Attached { .. } | LinkState::Detached { .. } => true,
            LinkState::Failed { .. } => self.retry_until.get().is_none_or(|until| now >= until),
            LinkState::Connecting { .. } | LinkState::Attaching { .. } => false,
        };
        if self.first_poll.get() || self.link_wake.is_none() || !settled || !self.feeds.borrow().is_quiet() {
            EditorWait::Tick
        } else {
            EditorWait::Sockets
        }
    }

    /// What to watch for reading while [`Editor::wait`] says [`EditorWait::Sockets`]: the feeds' listening
    /// sockets and the link's wake.
    pub fn wake_fds(&self) -> Vec<RawFd> {
        let mut fds = self.feeds.borrow().listener_fds();
        fds.extend(self.link_wake.as_ref().map(AsRawFd::as_raw_fd));
        fds
    }

    /// How many polls have run: what a test reads to see an idle editor left alone.
    pub fn polls_run(&self) -> u64 {
        self.polls.get()
    }

    fn poll_link(&self, now: Instant, events: &mut Vec<EditorEvent>) {
        let poll = self.link.poll(now);
        for line in &poll.logs {
            crate::log_line!("{line}");
        }
        if let Some((state, band, _peer)) = poll.changed {
            let was = self.attached.get();
            let is = matches!(state, LinkState::Attached { .. });
            self.attached.set(is);
            // A new editor starts empty: forget the last one's file and selection as a connect begins and as
            // an attached editor goes. Not on arriving at `Attached`: the install makes the editor send its
            // first context, and that line may be read just before.
            if poll.connect_began || matches!(state, LinkState::Connecting { .. }) || (was && !is) {
                (self.reset_context)();
            }
            if is {
                self.retry_until.set(None);
            }
            crate::log_line!("[editor] link: {} {}", band.state, band.text);
            match &state {
                LinkState::Failed { addr, why } if self.retry_until.get().is_some_and(|until| now < until) => {
                    crate::log_line!("[editor] attach failed ({why}); trying again");
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

    /// The editor-context feed's socket, where nvim's glue writes the file and selection; `None` when the feed
    /// could not be made.
    pub fn context_socket(&self) -> Option<&Path> {
        self.context_feed.as_ref().map(EditorContextFeed::socket_path)
    }

    /// The socket nvim listens on.
    pub fn nvim_listen(&self) -> &Path {
        &self.nvim_listen
    }
}

#[cfg(test)]
mod attach_wait_tests {
    use super::*;

    #[test]
    fn the_socket_is_awaited_for_fifteen_seconds_then_given_up() {
        let start = Instant::now();
        let wait = AttachWait::new(start);
        assert_eq!(wait.step(start, false), AttachStep::Wait);
        assert_eq!(
            wait.step(start + Duration::from_millis(14_950), false),
            AttachStep::Wait
        );
        assert_eq!(wait.step(start + Duration::from_secs(15), false), AttachStep::GiveUp);
        assert_eq!(
            wait.step(start + Duration::from_secs(20), true),
            AttachStep::Attach,
            "late is still found"
        );
        assert_eq!(wait.step(start, true), AttachStep::Attach);
        assert_eq!(AttachWait::POLL, Duration::from_millis(50));
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

    /// A context line read from the feed is said, so the window wakes the panel that reads it; the context
    /// itself stays gated on an attached editor.
    #[test]
    fn a_new_context_is_said() {
        use std::io::Write;
        let editor = editor();
        let start = Instant::now();
        editor.poll(start);
        let path = editor
            .context_feed
            .as_ref()
            .expect("the context feed")
            .socket_path()
            .to_path_buf();
        let mut stream = std::os::unix::net::UnixStream::connect(&path).expect("connect");
        stream
            .write_all(b"{\"v\":1,\"file\":\"/p/a.rs\",\"selection\":null}\n")
            .expect("write");
        drop(stream);
        let mut said = 0;
        for step in 1..=40u32 {
            let events = editor.poll(start + Duration::from_millis(100) * step);
            said += events
                .iter()
                .filter(|e| matches!(e, EditorEvent::ContextChanged))
                .count();
            if said > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(said, 1, "the new context was said once");
        assert!((editor.context())().is_none(), "no editor is attached");
        editor.shutdown();
    }

    #[test]
    fn shutdown_removes_its_directories() {
        let editor = editor();
        let sockets = [
            editor.theme_feed.as_ref().map(ThemeFeed::socket_path),
            editor.keys_feed.as_ref().map(NvimKeysFeed::socket_path),
            editor.context_feed.as_ref().map(EditorContextFeed::socket_path),
            editor.pane_switch.as_ref().map(PaneSwitchChannel::socket_path),
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
    fn the_host_ticks_only_while_something_is_left_to_read() {
        let editor = editor();
        let start = Instant::now();
        assert_eq!(editor.wait(start), EditorWait::Tick, "the first poll says the band");
        editor.poll(start);
        assert_eq!(editor.wait(start), EditorWait::Sockets, "no editor, nothing to read");
        assert_eq!(editor.wake_fds().len(), 5, "four feeds and the link's wake");

        // nvim connects to a feed: ticking until it is read and closed.
        let path = editor.context_socket().expect("the context feed").to_path_buf();
        let stream = UnixStream::connect(&path).expect("connect");
        assert_eq!(editor.wait(start), EditorWait::Tick, "a connection is waiting");
        drop(stream);
        let mut now = start;
        for _ in 0..40 {
            now += TICK;
            editor.poll(now);
            if editor.wait(now) == EditorWait::Sockets {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(editor.wait(now), EditorWait::Sockets, "read, and quiet again");

        // An attach: the link is on its way, and its thread's news is read at once when it comes.
        editor.attach();
        let polls = editor.polls_run();
        editor.poll(now);
        assert_eq!(editor.wait(now), EditorWait::Tick, "connecting");
        assert_eq!(editor.polls_run(), polls + 1);
        editor.shutdown();
        assert_eq!(editor.wait(now), EditorWait::Stop);
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
