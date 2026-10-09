//! The editor's four push sockets (theme, keys, editor context, pane switch) read from one place.
//!
//! Each feed used to be drained by a `glib` timer of its own in `shell`; the reading itself never
//! needed a toolkit, only a clock. [`EditorFeeds::poll`] takes the time as an argument and decides
//! which readers are due, so a host drives every feed from a single tick of whatever loop it has,
//! and a test drives it with times it makes up.
//!
//! A reader is added only when its consumer is ready: a socket nobody acts on yet is left unread,
//! so its connections wait in the kernel's queue instead of being consumed and dropped.

use std::cell::RefCell;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::editor_context::feed::EditorContextReader;
use crate::editor_context::{ContextSource, EditorContext};
use crate::nvim_keys::feed::NvimKeysReader;
use crate::nvim_keys::NvimReport;
use crate::pane_switch::{PaneMessage, PaneSwitchReader};
use crate::theme::feed::ThemePayloadReader;
use crate::theme::payload::NvimThemePayload;

/// What one poll of the feeds found, for the host to act on after the poll returned.
#[derive(Default)]
pub struct FeedEvents {
    pub theme: Option<NvimThemePayload>,
    pub keys: Option<NvimReport>,
    pub pane: Vec<PaneMessage>,
    /// The context source answers something new: a host whose panel reads it only when woken must wake it.
    pub context: bool,
}

/// Whether any of `fds` is readable now, without waiting. An error counts as readable: the caller
/// then keeps polling, which is what it did before it could ask.
fn any_readable(fds: &[RawFd]) -> bool {
    if fds.is_empty() {
        return false;
    }
    let mut polled: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    // SAFETY: `polled` is a live array of `polled.len()` entries for the call; a zero timeout
    // never waits.
    let ready = unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, 0) };
    ready != 0
}

/// One reader and the schedule it is read on.
struct Cadenced<R> {
    reader: R,
    every: Duration,
    next: Instant,
}

impl<R> Cadenced<R> {
    /// `now` is the origin of the cadence: the first read is due one interval after it.
    fn new(reader: R, every: Duration, now: Instant) -> Self {
        Self {
            reader,
            every,
            next: now + every,
        }
    }

    /// The reader if a read is due at `now` (the next one is scheduled), otherwise `None`. The schedule advances by
    /// whole intervals from the last due time, so a tick that arrives a little late does not push every
    /// later read back; after a stall longer than an interval it restarts from `now` rather than
    /// reading several times in a row to catch up.
    fn due(&mut self, now: Instant) -> Option<&mut R> {
        if now < self.next {
            return None;
        }
        self.next += self.every;
        if self.next <= now {
            self.next = now + self.every;
        }
        Some(&mut self.reader)
    }
}

/// The context feed's reader and the last context it delivered, which the panel reads at send time.
struct ContextFeed {
    reader: Rc<RefCell<EditorContextReader>>,
    cache: Rc<RefCell<Option<EditorContext>>>,
    scratch_dir: Option<PathBuf>,
}

impl ContextFeed {
    /// Keeps what the reader returns unless it names a scratch buffer.
    ///
    /// A poll that brings nothing leaves the cache alone: nvim only writes when something changed, so
    /// "no news" is not "no context", and forgetting on an empty tick would drop the user's selection
    /// moments after they made it, which is exactly when they move to the panel to ask about it. A report
    /// naming one of Eitri's own scratch buffers (a draft or a view) is dropped too, so the context stays
    /// on the file the user was in before it.
    /// Whether the cache took a new context.
    fn poll(&self) -> bool {
        let Some(context) = self.reader.borrow_mut().poll() else {
            return false;
        };
        let scratch = self
            .scratch_dir
            .as_deref()
            .is_some_and(|dir| crate::scratch::holds(dir, &context.file));
        if !scratch {
            *self.cache.borrow_mut() = Some(context);
        }
        !scratch
    }
}

/// The editor's push sockets, each read at its own cadence from one [`poll`](Self::poll).
/// Readers are added when their consumer is ready, so nothing is read before someone acts on it.
#[derive(Default)]
pub struct EditorFeeds {
    theme: Option<Cadenced<ThemePayloadReader>>,
    keys: Option<Cadenced<NvimKeysReader>>,
    pane: Option<Cadenced<PaneSwitchReader>>,
    context: Option<Cadenced<ContextFeed>>,
}

impl EditorFeeds {
    /// How often the host should call `poll`: the finest feed cadence (the pane switch's).
    pub const TICK: Duration = crate::pane_switch::POLL_INTERVAL;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_theme(&mut self, reader: ThemePayloadReader, now: Instant) {
        self.theme = Some(Cadenced::new(reader, crate::theme::feed::POLL_INTERVAL, now));
    }

    pub fn add_keys(&mut self, reader: NvimKeysReader, now: Instant) {
        self.keys = Some(Cadenced::new(reader, crate::nvim_keys::feed::POLL_INTERVAL, now));
    }

    /// The pane switch is a stream of discrete key presses, so it is read on every poll: the host's
    /// tick is its cadence.
    pub fn add_pane_switch(&mut self, reader: PaneSwitchReader, now: Instant) {
        self.pane = Some(Cadenced::new(reader, Duration::ZERO, now));
    }

    /// The context feed: what it last read, minus scratch buffers, is what the returned source answers;
    /// the returned closure forgets it and discards what the reader still holds. A companion window
    /// attaches to editors that come and go, and the cache would otherwise keep the last one's file
    /// and selection for good. The reader forgets what is queued or half read as well, so a line the
    /// old editor wrote just before it was left cannot land in the cache after the reset and ride on a
    /// turn.
    pub fn add_context(
        &mut self,
        reader: EditorContextReader,
        scratch_dir: Option<PathBuf>,
        now: Instant,
    ) -> (ContextSource, Rc<dyn Fn()>) {
        let feed = ContextFeed {
            reader: Rc::new(RefCell::new(reader)),
            cache: Rc::new(RefCell::new(None)),
            scratch_dir,
        };
        let cache = feed.cache.clone();
        let source: ContextSource = Rc::new({
            let cache = cache.clone();
            move || cache.borrow().clone()
        });
        let reset_reader = feed.reader.clone();
        let reset: Rc<dyn Fn()> = Rc::new(move || {
            reset_reader.borrow_mut().discard();
            *cache.borrow_mut() = None;
        });
        self.context = Some(Cadenced::new(feed, crate::editor_context::feed::POLL_INTERVAL, now));
        (source, reset)
    }

    /// The listening sockets of the readers added so far: what a host waits on while the feeds are
    /// [quiet](Self::is_quiet).
    pub fn listener_fds(&self) -> Vec<RawFd> {
        let mut fds = Vec::new();
        if let Some(feed) = &self.theme {
            fds.push(feed.reader.lines().listener_fd());
        }
        if let Some(feed) = &self.keys {
            fds.push(feed.reader.lines().listener_fd());
        }
        if let Some(feed) = &self.context {
            fds.push(feed.reader.reader.borrow().lines().listener_fd());
        }
        if let Some(feed) = &self.pane {
            fds.push(feed.reader.listener_fd());
        }
        fds
    }

    /// Whether there is nothing to read: no reader holds a connection it accepted and has not
    /// finished, and no listener has a connection waiting. A host that finds the feeds quiet may stop
    /// polling until one of [`Self::listener_fds`] becomes readable; while they are not, it polls
    /// on its tick, since a connection that is accepted but not yet written to, or one a feed is not
    /// due to read yet, wakes no listener.
    pub fn is_quiet(&self) -> bool {
        let holding = self
            .theme
            .as_ref()
            .is_some_and(|f| f.reader.lines().holds_connections())
            || self.keys.as_ref().is_some_and(|f| f.reader.lines().holds_connections())
            || self
                .context
                .as_ref()
                .is_some_and(|f| f.reader.reader.borrow().lines().holds_connections())
            || self.pane.as_ref().is_some_and(|f| f.reader.holds_connections());
        !holding && !any_readable(&self.listener_fds())
    }

    /// Reads every feed whose interval has passed since its last read (theme, keys and context every
    /// `POLL_INTERVAL` of theirs, the pane switch on every call), one payload per feed per read.
    pub fn poll(&mut self, now: Instant) -> FeedEvents {
        let mut events = FeedEvents::default();
        if let Some(reader) = self.theme.as_mut().and_then(|feed| feed.due(now)) {
            events.theme = reader.poll();
        }
        if let Some(reader) = self.keys.as_mut().and_then(|feed| feed.due(now)) {
            events.keys = reader.poll();
        }
        if let Some(feed) = self.context.as_mut().and_then(|feed| feed.due(now)) {
            events.context = feed.poll();
        }
        if let Some(reader) = self.pane.as_mut().and_then(|feed| feed.due(now)) {
            events.pane = reader.poll();
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_context::feed::EditorContextFeed;
    use crate::nvim_keys::feed::NvimKeysFeed;
    use crate::pane_switch::PaneSwitchChannel;
    use crate::theme::feed::ThemeFeed;
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    fn send(path: &std::path::Path, line: &str) {
        let mut stream = UnixStream::connect(path).expect("connect");
        stream.write_all(format!("{line}\n").as_bytes()).expect("write");
    }

    fn theme_line(colors_name: &str) -> String {
        format!(
            r#"{{"v":1,"groups":{{}},"options":{{"background":"dark","guifont":"","colors_name":"{colors_name}"}}}}"#
        )
    }

    fn context_line(file: &str) -> String {
        format!(r#"{{"v":1,"file":"{file}","selection":null}}"#)
    }

    fn file(source: &ContextSource) -> Option<String> {
        source().map(|context| context.file)
    }

    fn context_feeds(
        scratch_dir: Option<PathBuf>,
    ) -> (EditorFeeds, EditorContextFeed, ContextSource, Rc<dyn Fn()>, Instant) {
        let mut feed = EditorContextFeed::new().expect("feed");
        let mut feeds = EditorFeeds::new();
        let now = Instant::now();
        let reader = EditorContextReader::new(feed.take_listener().unwrap());
        let (source, reset) = feeds.add_context(reader, scratch_dir, now);
        (feeds, feed, source, reset, now)
    }

    #[test]
    fn a_feed_is_read_only_at_its_own_cadence() {
        let mut feed = ThemeFeed::new().expect("feed");
        let mut feeds = EditorFeeds::new();
        let now = Instant::now();
        feeds.add_theme(ThemePayloadReader::new(feed.take_listener().unwrap()), now);
        send(feed.socket_path(), &theme_line("a"));

        assert!(
            feeds.poll(now + Duration::from_millis(50)).theme.is_none(),
            "not due yet"
        );
        assert!(
            feeds.poll(now + Duration::from_millis(99)).theme.is_none(),
            "still not due"
        );
        let found = feeds.poll(now + Duration::from_millis(100)).theme;
        assert_eq!(found.expect("due at one interval").options.colors_name, "a");

        send(feed.socket_path(), &theme_line("b"));
        assert!(
            feeds.poll(now + Duration::from_millis(150)).theme.is_none(),
            "the next read is one interval after the last"
        );
        let found = feeds.poll(now + Duration::from_millis(200)).theme;
        assert_eq!(found.expect("due again").options.colors_name, "b");
        feed.cleanup();
    }

    #[test]
    fn a_late_tick_does_not_push_the_following_reads_back() {
        let mut feed = NvimKeysFeed::new().expect("feed");
        let mut feeds = EditorFeeds::new();
        let now = Instant::now();
        feeds.add_keys(NvimKeysReader::new(feed.take_listener().unwrap()), now);
        let report = r#"{"v":1,"mapleader":null,"timeoutlen":300,"timeout":true,"maps":[]}"#;

        send(feed.socket_path(), report);
        assert!(feeds.poll(now + Duration::from_millis(110)).keys.is_some());
        send(feed.socket_path(), report);
        assert!(
            feeds.poll(now + Duration::from_millis(199)).keys.is_none(),
            "not due before the 200 ms mark"
        );
        assert!(
            feeds.poll(now + Duration::from_millis(200)).keys.is_some(),
            "the schedule stayed on its own grid"
        );
        feed.cleanup();
    }

    #[test]
    fn the_pane_switch_is_read_every_tick() {
        let mut channel = PaneSwitchChannel::bind_without_shim().expect("bind");
        let mut feeds = EditorFeeds::new();
        let now = Instant::now();
        feeds.add_pane_switch(PaneSwitchReader::new(channel.take_listener().unwrap()), now);

        send(channel.socket_path(), "L");
        let first = feeds.poll(now);
        assert_eq!(first.pane, vec![PaneMessage::Direction('L')]);
        send(channel.socket_path(), "Q 7");
        let second = feeds.poll(now + EditorFeeds::TICK);
        assert_eq!(second.pane, vec![PaneMessage::QuitCancelled(7)]);
        assert!(feeds.poll(now + EditorFeeds::TICK * 2).pane.is_empty());
        channel.cleanup();
    }

    #[test]
    fn a_scratch_buffer_never_becomes_the_context() {
        let scratch = Some(PathBuf::from("/tmp/s"));
        let (mut feeds, feed, source, _reset, now) = context_feeds(scratch);
        let every = crate::editor_context::feed::POLL_INTERVAL;

        send(feed.socket_path(), &context_line("/p/a.rs"));
        assert!(feeds.poll(now + every).context, "a new context is said to be new");
        send(feed.socket_path(), &context_line("/tmp/s/1-draft.md"));
        assert!(!feeds.poll(now + every * 2).context, "a dropped one changes nothing");
        assert_eq!(file(&source), Some("/p/a.rs".to_owned()));
        feed.cleanup();
    }

    #[test]
    fn a_poll_that_brings_no_line_leaves_the_context_alone() {
        let (mut feeds, feed, source, _reset, now) = context_feeds(None);
        let every = crate::editor_context::feed::POLL_INTERVAL;

        send(feed.socket_path(), &context_line("/p/a.rs"));
        assert!(feeds.poll(now + every).context);
        assert!(!feeds.poll(now + every * 2).context, "no line, no news");
        assert_eq!(file(&source), Some("/p/a.rs".to_owned()));
        feed.cleanup();
    }

    #[test]
    fn a_reset_forgets_the_context_and_what_the_reader_held() {
        let (mut feeds, feed, source, reset, now) = context_feeds(None);
        let every = crate::editor_context::feed::POLL_INTERVAL;

        send(feed.socket_path(), &context_line("/p/a.rs"));
        feeds.poll(now + every);
        assert_eq!(file(&source), Some("/p/a.rs".to_owned()));

        // A line the old editor wrote just before it was left, not yet read.
        send(feed.socket_path(), &context_line("/p/old.rs"));
        reset();
        assert_eq!(file(&source), None);
        feeds.poll(now + every * 2);
        assert_eq!(file(&source), None, "what the reader still held was discarded");

        send(feed.socket_path(), &context_line("/p/b.rs"));
        feeds.poll(now + every * 3);
        assert_eq!(
            file(&source),
            Some("/p/b.rs".to_owned()),
            "the next editor starts empty"
        );
        feed.cleanup();
    }

    /// Quiet means nothing to read: a connection waiting for a feed that is not due yet, or one
    /// accepted with its line unfinished, keeps the feeds from being quiet until it is read.
    #[test]
    fn the_feeds_are_quiet_only_with_nothing_left_to_read() {
        let (mut feeds, feed, source, _reset, now) = context_feeds(None);
        let every = crate::editor_context::feed::POLL_INTERVAL;
        assert_eq!(feeds.listener_fds().len(), 1);
        assert!(feeds.is_quiet(), "nothing sent");

        send(feed.socket_path(), &context_line("/p/a.rs"));
        assert!(!feeds.is_quiet(), "a connection waits");
        feeds.poll(now + every / 2);
        assert!(!feeds.is_quiet(), "still waits: the feed was not due");
        feeds.poll(now + every);
        assert_eq!(file(&source), Some("/p/a.rs".to_owned()));
        assert!(feeds.is_quiet(), "read");

        // Connected and half written: accepted by the read, but not finished.
        let mut half = UnixStream::connect(feed.socket_path()).expect("connect");
        half.write_all(br#"{"v":1,"file":"/p/b.rs""#).expect("write");
        feeds.poll(now + every * 2);
        assert!(!feeds.is_quiet(), "a line is half read");
        half.write_all(b",\"selection\":null}\n").expect("write");
        drop(half);
        feeds.poll(now + every * 3);
        assert_eq!(file(&source), Some("/p/b.rs".to_owned()));
        assert!(feeds.is_quiet());
        feed.cleanup();
    }

    #[test]
    fn the_pane_switch_is_quiet_once_its_messages_are_read() {
        let mut channel = PaneSwitchChannel::bind_without_shim().expect("bind");
        let mut feeds = EditorFeeds::new();
        let now = Instant::now();
        assert!(feeds.listener_fds().is_empty() && feeds.is_quiet(), "nothing added");
        feeds.add_pane_switch(PaneSwitchReader::new(channel.take_listener().unwrap()), now);
        assert!(feeds.is_quiet());
        send(channel.socket_path(), "L");
        assert!(!feeds.is_quiet());
        assert_eq!(feeds.poll(now).pane, vec![PaneMessage::Direction('L')]);
        assert!(feeds.is_quiet());
        channel.cleanup();
    }

    #[test]
    fn a_feed_not_added_reads_nothing() {
        let mut feed = ThemeFeed::new().expect("feed");
        let mut feeds = EditorFeeds::new();
        let now = Instant::now();
        send(feed.socket_path(), &theme_line("a"));

        let events = feeds.poll(now + Duration::from_secs(10));
        assert!(events.theme.is_none() && events.keys.is_none() && events.pane.is_empty());
        // The line is still queued for whoever adds the reader later.
        feeds.add_theme(ThemePayloadReader::new(feed.take_listener().unwrap()), now);
        let found = feeds.poll(now + Duration::from_secs(10)).theme;
        assert_eq!(found.expect("still there").options.colors_name, "a");
        feed.cleanup();
    }
}
