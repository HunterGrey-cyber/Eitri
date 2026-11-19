//! A mirror session against the headless terminal that owns the program (in memory, no network), and against a
//! scripted link for the records a server could send. Runs without the `paint` feature.

mod headless_common;

use std::collections::VecDeque;
use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use eitri_terminal::headless::{Attachment, DetachWhy, HeadlessTerminal, StreamItem};
use eitri_terminal::mirror::{
    FrameRefused, LinkItem, LinkState, MirrorConfig, MirrorEnd, MirrorReport, TermLink, PENDING_INPUT_MAX,
};
use eitri_terminal::{
    DetachRefused, ExitInfo, HostEvents, PtySize, Screen, SessionCommand, Snapshot, Tail, TerminalColors,
    TerminalSession,
};
use headless_common::{config, finish, size, wait_for, WAIT};
use terminal_input::keys::{Key, KeyEvent, ModifiersState, NamedKey};
use terminal_input::NormalizedInput;
use terminal_render::PaintList;

const CLIENT: &str = "mirror";
/// Printed after sequences that change nothing visible, so a frame shows they were parsed.
const MARK: &[u8] = b"MARK";

fn nonblocking(fd: BorrowedFd<'_>) {
    // SAFETY: fcntl on a live descriptor.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        assert!(flags >= 0);
        assert!(libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) >= 0);
    }
}

/// The program's side, in memory: a headless terminal and this client's attachment to it.
struct HeadlessLink {
    term: Arc<HeadlessTerminal>,
    attachment: Attachment,
    sent: u64,
    said_live: bool,
    writes: Arc<Mutex<Vec<u8>>>,
}

impl TermLink for HeadlessLink {
    fn readiness(&self) -> BorrowedFd<'_> {
        self.attachment.ready()
    }
    fn take(&mut self) -> Vec<LinkItem> {
        let mut items = Vec::new();
        if !self.said_live {
            self.said_live = true;
            items.push(LinkItem::Link {
                state: LinkState::Live,
                lost_input_bytes: 0,
            });
        }
        while let Some(item) = self.attachment.next() {
            items.push(match item {
                StreamItem::Snapshot(s) => LinkItem::Snapshot(s),
                StreamItem::Output { offset, bytes } => LinkItem::Output { offset, bytes },
                StreamItem::Resized { offset, size } => LinkItem::Resized { offset, size },
                StreamItem::InputWritten(n) => LinkItem::InputWritten(n),
                StreamItem::Exited(e) => LinkItem::Exited(e),
                StreamItem::Waiting(w) => LinkItem::Waiting(w),
                StreamItem::Detached(DetachWhy::Replaced) => LinkItem::Ended(MirrorEnd::Replaced),
                StreamItem::Detached(DetachWhy::Closed) => LinkItem::Ended(MirrorEnd::Closed),
                StreamItem::Detached(DetachWhy::Stopped) => LinkItem::Ended(MirrorEnd::Stopped),
            });
        }
        items
    }
    fn write_input(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writes.lock().unwrap().extend_from_slice(bytes);
        self.term.input(&CLIENT.into(), self.sent, bytes.to_vec());
        self.sent += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn request_resize(&mut self, size: PtySize) {
        self.term.resize(size);
    }
    fn set_colors(&mut self, colors: &TerminalColors) {
        self.term.set_colors(*colors);
    }
}

#[derive(Default)]
struct Scripted {
    items: VecDeque<LinkItem>,
    writes: Vec<u8>,
    resizes: Vec<PtySize>,
    colors: usize,
    connected: bool,
    /// Bytes the send window takes before answering `WouldBlock`; `None` takes everything.
    window: Option<usize>,
    /// A state change is queued and not yet taken: writes are refused, as the link's contract says.
    unread_change: bool,
    /// Refuse every write, as a link does when its connection changed after the mirror admitted input.
    refuse_writes: bool,
}

/// A link whose records the test writes.
struct ScriptedLink {
    state: Arc<Mutex<Scripted>>,
    ready: PipeReader,
}

struct Script {
    state: Arc<Mutex<Scripted>>,
    bell: PipeWriter,
}

impl Script {
    fn push(&self, item: LinkItem) {
        let mut state = self.state.lock().unwrap();
        if matches!(item, LinkItem::Link { .. }) {
            state.unread_change = true;
        }
        state.items.push_back(item);
        drop(state);
        let _ = (&self.bell).write(&[1]);
    }
    fn state(&self, state: LinkState) {
        self.state.lock().unwrap().connected = state == LinkState::Live;
        self.push(LinkItem::Link {
            state,
            lost_input_bytes: 0,
        });
    }
    fn live(&self) {
        self.state(LinkState::Live);
    }
    fn down(&self) {
        self.state(LinkState::Disconnected { since_ms: 0 });
    }
    fn writes(&self) -> Vec<u8> {
        self.state.lock().unwrap().writes.clone()
    }
    fn resizes(&self) -> Vec<PtySize> {
        self.state.lock().unwrap().resizes.clone()
    }
    fn colors(&self) -> usize {
        self.state.lock().unwrap().colors
    }
    fn window(&self, window: Option<usize>) {
        self.state.lock().unwrap().window = window;
        let _ = (&self.bell).write(&[1]);
    }
}

impl TermLink for ScriptedLink {
    fn readiness(&self) -> BorrowedFd<'_> {
        self.ready.as_fd()
    }
    fn take(&mut self) -> Vec<LinkItem> {
        let mut drain = [0u8; 64];
        while matches!((&self.ready).read(&mut drain), Ok(n) if n > 0) {}
        let mut state = self.state.lock().unwrap();
        state.unread_change = false;
        state.items.drain(..).collect()
    }
    fn write_input(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut state = self.state.lock().unwrap();
        if !state.connected || state.unread_change || state.refuse_writes {
            return Err(io::ErrorKind::NotConnected.into());
        }
        let n = state.window.map_or(bytes.len(), |w| w.min(bytes.len()));
        if n == 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        if let Some(w) = &mut state.window {
            *w -= n;
        }
        state.writes.extend_from_slice(&bytes[..n]);
        Ok(n)
    }
    fn request_resize(&mut self, size: PtySize) {
        self.state.lock().unwrap().resizes.push(size);
    }
    fn set_colors(&mut self, _colors: &TerminalColors) {
        self.state.lock().unwrap().colors += 1;
    }
}

fn scripted() -> (ScriptedLink, Script) {
    let (ready, bell) = std::io::pipe().unwrap();
    nonblocking(ready.as_fd());
    nonblocking(bell.as_fd());
    let state = Arc::new(Mutex::new(Scripted::default()));
    (
        ScriptedLink {
            state: state.clone(),
            ready,
        },
        Script { state, bell },
    )
}

/// The host's side of a mirror: what its updates said so far.
struct Host {
    session: TerminalSession,
    wakes: Arc<AtomicUsize>,
    frame: Option<PaintList>,
    reports: Vec<MirrorReport>,
    events: Vec<HostEvents>,
    exits: Vec<ExitInfo>,
}

impl Host {
    fn start(link: Box<dyn TermLink>, size: PtySize) -> Host {
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = wakes.clone();
        let session = TerminalSession::mirror(
            link,
            MirrorConfig {
                size,
                colors: TerminalColors::default(),
                focused: true,
            },
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
            },
        )
        .unwrap();
        Host {
            session,
            wakes,
            frame: None,
            reports: Vec::new(),
            events: Vec::new(),
            exits: Vec::new(),
        }
    }

    fn pump(&mut self) {
        let update = self.session.take_update();
        if let Some(frame) = update.frame {
            self.frame = Some(frame);
        }
        if let Some(report) = update.mirror {
            self.reports.push(report);
        }
        if update.events != HostEvents::default() {
            self.events.push(update.events);
        }
        if let Some(exit) = update.exited {
            self.exits.push(exit);
        }
    }

    fn wait(&mut self, what: &str, mut done: impl FnMut(&Host) -> bool) {
        let until = Instant::now() + WAIT;
        loop {
            self.pump();
            if done(self) {
                return;
            }
            assert!(Instant::now() < until, "{what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn ended(&self) -> Option<MirrorEnd> {
        self.reports.iter().rev().find_map(|r| r.ended.clone())
    }
    fn syncs(&self) -> usize {
        self.reports.iter().filter(|r| r.synced.is_some()).count()
    }
    fn synced(&self) -> bool {
        self.syncs() > 0
    }
    fn refused(&self) -> u64 {
        self.reports.iter().map(|r| r.refused_input_bytes).sum()
    }
    fn saw_link(&self, want: impl Fn(&LinkState) -> bool) -> bool {
        self.reports.iter().any(|r| r.link.as_ref().is_some_and(&want))
    }

    fn key(&self, key: Key) {
        self.session.send(SessionCommand::Input(NormalizedInput::Key {
            event: KeyEvent::press(key),
            mods: ModifiersState::empty(),
        }));
    }
    fn paste(&self, text: String) {
        self.session
            .send(SessionCommand::Input(NormalizedInput::Paste { text, bracketed: true }));
    }
}

/// What a local screen of `size` shows after `steps`, rendered as the mirror renders.
enum Step<'a> {
    Feed(&'a [u8]),
    Resize(PtySize),
}

fn reference(size: PtySize, steps: &[Step]) -> PaintList {
    let mut screen = Screen::new(size, TerminalColors::default());
    for step in steps {
        match step {
            Step::Feed(bytes) => screen.feed(bytes),
            Step::Resize(size) => screen.resize(*size),
        }
    }
    screen.render(true)
}

fn python(script: &str) -> eitri_terminal::HeadlessConfig {
    let mut c = config("");
    c.spawn.program = "/usr/bin/python3".into();
    c.spawn.args = vec!["-c".into(), script.into()];
    c
}

/// A raw-mode program that prints `READY`, then the next chunk of its arguments (hex) for every byte it reads,
/// then waits to be closed.
const CHUNKS: &str = r#"
import os, sys, tty
tty.setraw(0)
def write(data):
    while data:
        data = data[os.write(1, data):]
write(b'READY')
for chunk in sys.argv[1:]:
    os.read(0, 1)
    write(bytes.fromhex(chunk))
while os.read(0, 4096):
    pass
"#;

/// The same program printing its first chunk at once, before anyone attaches.
const AT_ONCE: &str = r#"
import os, sys, tty
tty.setraw(0)
def write(data):
    while data:
        data = data[os.write(1, data):]
write(b'READY' + bytes.fromhex(sys.argv[1]))
while os.read(0, 4096):
    pass
"#;

fn program(script: &str, chunks: &[&[u8]], size: PtySize) -> Arc<HeadlessTerminal> {
    let mut c = python(script);
    c.size = size;
    for chunk in chunks {
        c.spawn
            .args
            .push(chunk.iter().map(|b| format!("{b:02x}")).collect::<String>().into());
    }
    Arc::new(HeadlessTerminal::spawn(c).unwrap())
}

fn chunks_program(chunks: &[&[u8]], size: PtySize) -> Arc<HeadlessTerminal> {
    let term = program(CHUNKS, chunks, size);
    wait_for(|| term.status().offset == 5);
    term
}

fn mirror_of(term: &Arc<HeadlessTerminal>, size: PtySize) -> (Host, Arc<Mutex<Vec<u8>>>) {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let link = HeadlessLink {
        term: term.clone(),
        attachment: term.attach(CLIENT.into()).unwrap(),
        sent: 0,
        said_live: false,
        writes: writes.clone(),
    };
    let mut host = Host::start(Box::new(link), size);
    host.wait("the first snapshot", Host::synced);
    (host, writes)
}

fn close(term: Arc<HeadlessTerminal>, host: Host) {
    // The mirror's thread holds the link, and the link holds the terminal: let it end first.
    host.session.shutdown_and_wait();
    let term = Arc::try_unwrap(term)
        .ok()
        .expect("only the test holds the terminal now");
    finish(term);
}

#[test]
fn mirror_equals_a_local_screen_after_the_corpus_streamed_live() {
    for bytes in headless_common::corpus() {
        let s = size(80, 24);
        // The mark makes even a case that changes nothing visible show that it was parsed.
        let chunk = [&bytes[..], MARK].concat();
        let term = chunks_program(&[&chunk], s);
        let (mut host, _) = mirror_of(&term, s);
        host.key(Key::Character("g".into()));
        let want = reference(s, &[Step::Feed(b"READY"), Step::Feed(&chunk)]);
        host.wait("the mirror shows what a local screen shows", |h| {
            h.frame.as_ref() == Some(&want)
        });
        assert_eq!(host.ended(), None);
        close(term, host);
    }
}

#[test]
fn mirror_equals_a_local_screen_when_the_corpus_came_in_its_snapshot() {
    // The whole corpus reaches the mirror through the untrusted replay path, not as live output.
    for bytes in headless_common::corpus() {
        let s = size(80, 24);
        let term = program(AT_ONCE, &[&bytes], s);
        wait_for(|| term.status().offset == 5 + bytes.len() as u64);
        let (mut host, _) = mirror_of(&term, s);
        let want = reference(s, &[Step::Feed(b"READY"), Step::Feed(&bytes)]);
        host.wait("the replayed mirror shows what a local screen shows", |h| {
            h.frame.as_ref() == Some(&want)
        });
        assert_eq!(host.ended(), None);
        close(term, host);
    }
}

#[test]
fn mirror_equals_the_server_across_resizes_at_their_markers() {
    let sizes = [
        size(80, 24),
        size(100, 30),
        size(60, 20),
        size(120, 40),
        size(80, 24),
        size(90, 10),
    ];
    let lines: Vec<u8> = (0..300)
        .flat_map(|i| format!("\x1b[3{}mline {i:04} {}\x1b[0m\r\n", i % 8, "w".repeat(i % 97)).into_bytes())
        .collect();
    let chunks: Vec<&[u8]> = lines.chunks(lines.len() / 5 + 1).collect();
    let term = chunks_program(&chunks, sizes[0]);
    let (mut host, _) = mirror_of(&term, sizes[0]);
    let mut steps = vec![Step::Feed(b"READY")];
    let mut printed = 5u64;
    for (i, chunk) in chunks.iter().enumerate() {
        let next = sizes[i + 1];
        host.session.send(SessionCommand::Resize(next));
        wait_for(|| {
            let now = term.status().size;
            now.cols == next.cols && now.rows == next.rows
        });
        steps.push(Step::Resize(next));
        host.key(Key::Character("g".into()));
        steps.push(Step::Feed(chunk));
        // The chunk is parsed on the server before the next resize, as the reference has it.
        printed += chunk.len() as u64;
        wait_for(|| term.status().offset == printed);
    }
    let want = reference(sizes[0], &steps);
    host.wait("the mirror reflowed where the server did", |h| {
        h.frame.as_ref() == Some(&want)
    });
    assert_eq!(host.ended(), None);
    close(term, host);
}

#[test]
fn mirror_never_generates_a_reply_and_sends_only_what_was_typed() {
    // DA, DSR, a colour query and size reports: the server answers each, the mirror none.
    let queries: &[u8] = b"\x1b[c\x1b[6n\x1b]11;?\x07\x1b[18t\x1b[14tMARK";
    let term = chunks_program(&[queries], size(80, 24));
    let (mut host, writes) = mirror_of(&term, size(80, 24));
    host.key(Key::Character("g".into()));
    let want = reference(size(80, 24), &[Step::Feed(b"READY"), Step::Feed(queries)]);
    host.wait("the queries reached the mirror", |h| h.frame.as_ref() == Some(&want));
    let mut live = Screen::mirror(size(80, 24), TerminalColors::default());
    live.feed(queries);
    assert!(live.take_replies().is_empty(), "live queries queued an answer");
    assert_eq!(writes.lock().unwrap().as_slice(), b"g");
    close(term, host);
}

#[test]
fn keys_and_pastes_encode_against_the_mirrored_mode() {
    let cases: [(&[u8], &[u8]); 3] = [
        (b"\x1b[?1hMARK", b"\x1bOA"),
        (b"\x1b[?1lMARK", b"\x1b[A"),
        (b"\x1b[?2004hMARK", b"\x1b[200~p\x1b[201~"),
    ];
    for (i, (setup, want_bytes)) in cases.into_iter().enumerate() {
        let term = chunks_program(&[setup], size(80, 24));
        let (mut host, writes) = mirror_of(&term, size(80, 24));
        host.key(Key::Character("g".into()));
        let want = reference(size(80, 24), &[Step::Feed(b"READY"), Step::Feed(setup)]);
        host.wait("the mode reached the mirror", |h| h.frame.as_ref() == Some(&want));
        if i < 2 {
            host.key(Key::Named(NamedKey::ArrowUp));
        } else {
            host.paste("p".into());
        }
        wait_for(|| writes.lock().unwrap().len() > 1);
        assert_eq!(&writes.lock().unwrap()[1..], want_bytes);
        close(term, host);
    }
}

#[test]
fn a_replaced_mirror_ends_and_says_so() {
    let term = chunks_program(&[b"x"], size(80, 24));
    let (mut host, _) = mirror_of(&term, size(80, 24));
    let other = term.attach("other".into()).unwrap();
    host.wait("the mirror heard it was replaced", |h| h.ended().is_some());
    assert_eq!(host.ended(), Some(MirrorEnd::Replaced));
    drop(other);
    close(term, host);
}

fn blank_snapshot(size: PtySize, offset: u64) -> Snapshot {
    let mut screen = Screen::new(size, TerminalColors::default());
    Snapshot {
        offset,
        replay: screen.read_for_move().unwrap(),
        tail: Tail::ReadAhead(Vec::new()),
        history_at_cap: false,
        history_trimmed: 0,
        output_while_detached: false,
        input_written: 0,
    }
}

fn synced_scripted(at: PtySize) -> (Host, Script) {
    let (link, script) = scripted();
    let mut host = Host::start(Box::new(link), at);
    script.live();
    script.push(LinkItem::Snapshot(blank_snapshot(at, 0)));
    host.wait("the scripted snapshot", Host::synced);
    (host, script)
}

#[test]
fn a_replacement_in_the_same_batch_takes_precedence_over_a_refusal() {
    let (mut host, script) = synced_scripted(size(80, 24));
    script.state.lock().unwrap().items.extend([
        LinkItem::Resized {
            offset: 0,
            size: size(100, 30),
        },
        LinkItem::Ended(MirrorEnd::Replaced),
    ]);
    let _ = (&script.bell).write(&[1]);
    host.wait("the mirror ended", |h| h.ended().is_some());
    assert_eq!(host.ended(), Some(MirrorEnd::Replaced));
}

#[test]
fn snapshots_after_printing_del_or_c1_attach() {
    let s = size(80, 24);
    for (c, escaped) in [('\x7f', r"\177"), ('\u{85}', r"\302\205"), ('\u{9b}', r"\302\233")] {
        let command = format!(r"printf 'a{escaped}'; exec sleep 60");
        let term = HeadlessTerminal::spawn(config(&command)).unwrap();
        wait_for(|| term.status().offset == 1 + c.len_utf8() as u64);
        let a = term.attach(CLIENT.into()).unwrap();
        let snapshot = headless_common::snapshot(&a);
        assert_eq!(snapshot.replay.last_input, Some(c));
        let (link, script) = scripted();
        let mut host = Host::start(Box::new(link), s);
        script.live();
        script.push(LinkItem::Snapshot(snapshot));
        host.wait("the mirror processed the snapshot", |h| {
            h.synced() || h.ended().is_some()
        });
        assert_eq!(host.ended(), None);
        assert!(host.synced());
        finish(term);
    }
}

#[test]
fn a_size_never_requested_is_refused() {
    let (link, script) = scripted();
    let mut host = Host::start(Box::new(link), size(80, 24));
    script.live();
    script.push(LinkItem::Snapshot(blank_snapshot(size(200, 50), 0)));
    host.wait("the mirror refused the snapshot", |h| h.ended().is_some());
    assert_eq!(host.ended(), Some(MirrorEnd::Refused(FrameRefused::SizeNotRequested)));

    let (mut host, script) = synced_scripted(size(80, 24));
    script.push(LinkItem::Resized {
        offset: 0,
        size: size(100, 30),
    });
    host.wait("the mirror refused the marker", |h| h.ended().is_some());
    assert_eq!(host.ended(), Some(MirrorEnd::Refused(FrameRefused::SizeNotRequested)));
}

#[test]
fn resize_waits_for_the_marker_and_one_request_is_in_flight() {
    let (mut host, script) = synced_scripted(size(80, 24));
    let asked = script.resizes().len();
    host.session.send(SessionCommand::Resize(size(100, 30)));
    host.session.send(SessionCommand::Resize(size(110, 35)));
    host.session.send(SessionCommand::Resize(size(120, 40)));
    wait_for(|| script.resizes().len() == asked + 1);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        script.resizes()[asked..],
        [size(100, 30)],
        "later sizes wait for the answer"
    );
    script.push(LinkItem::Resized {
        offset: 0,
        size: size(100, 30),
    });
    wait_for(|| script.resizes().len() == asked + 2);
    assert_eq!(
        script.resizes()[asked + 1],
        size(120, 40),
        "the latest size is asked next"
    );
    script.push(LinkItem::Resized {
        offset: 0,
        size: size(120, 40),
    });
    script.push(LinkItem::Output {
        offset: 0,
        bytes: b"after".to_vec(),
    });
    let want = reference(
        size(80, 24),
        &[
            Step::Resize(size(100, 30)),
            Step::Resize(size(120, 40)),
            Step::Feed(b"after"),
        ],
    );
    host.wait("the mirror resized at the markers", |h| h.frame.as_ref() == Some(&want));
    assert_eq!(host.ended(), None);
}

#[test]
fn an_oversized_first_snapshot_allows_smaller_resize_requests_without_markers() {
    use eitri_terminal::headless::WaitWhy;

    let (link, script) = scripted();
    let initial = size(160, 48);
    let first = size(120, 40);
    let second = size(100, 30);
    let last = size(80, 24);
    let mut host = Host::start(Box::new(link), initial);
    script.live();
    host.wait("the link is live", |h| {
        h.reports.iter().any(|r| r.link == Some(LinkState::Live))
    });
    host.session.send(SessionCommand::Resize(first));
    script.push(LinkItem::Waiting(WaitWhy::SnapshotTooLarge));
    wait_for(|| script.resizes().last() == Some(&first));
    for smaller in [second, last] {
        host.session.send(SessionCommand::Resize(smaller));
        wait_for(|| script.resizes().last() == Some(&smaller));
    }

    script.push(LinkItem::Snapshot(blank_snapshot(first, 0)));
    script.push(LinkItem::Resized {
        offset: 0,
        size: second,
    });
    script.push(LinkItem::Resized { offset: 0, size: last });
    script.push(LinkItem::Output {
        offset: 0,
        bytes: b"ready".to_vec(),
    });
    let want = reference(last, &[Step::Feed(b"ready")]);
    host.wait("the snapshot and later resize markers were applied", |h| {
        h.frame.as_ref() == Some(&want)
    });
    assert_eq!(host.ended(), None);
}

#[test]
fn a_same_connection_resync_keeps_pending_resize_markers_eligible() {
    use eitri_terminal::headless::WaitWhy;

    let initial = size(160, 48);
    let first = size(120, 40);
    let second = size(100, 30);
    let last = size(80, 24);
    let (link, script) = scripted();
    let mut host = Host::start(Box::new(link), initial);
    script.live();
    script.push(LinkItem::Waiting(WaitWhy::SnapshotTooLarge));
    host.wait("the mirror is waiting for the snapshot", |h| {
        h.reports
            .iter()
            .any(|r| r.waiting == Some(Some(WaitWhy::SnapshotTooLarge)))
    });
    for smaller in [first, second, last] {
        host.session.send(SessionCommand::Resize(smaller));
        wait_for(|| script.resizes().last() == Some(&smaller));
    }

    script.push(LinkItem::Snapshot(blank_snapshot(first, 0)));
    host.wait("the initial snapshot was applied", |h| h.syncs() == 1);
    script.push(LinkItem::Snapshot(blank_snapshot(first, 20)));
    host.wait("the resync snapshot was applied", |h| h.syncs() == 2);
    script.push(LinkItem::Resized {
        offset: 20,
        size: second,
    });
    script.push(LinkItem::Output {
        offset: 20,
        bytes: b"ready".to_vec(),
    });
    let want = reference(second, &[Step::Feed(b"ready")]);
    host.wait("the mirror processed the intermediate resize marker", |h| {
        h.ended().is_some() || h.frame.as_ref() == Some(&want)
    });
    assert_eq!(host.ended(), None);

    script.push(LinkItem::Resized { offset: 25, size: last });
    let want = reference(second, &[Step::Feed(b"ready"), Step::Resize(last)]);
    host.wait("the latest resize marker was applied", |h| {
        h.frame.as_ref() == Some(&want)
    });
}

#[test]
fn a_long_shrink_drag_keeps_the_first_resize_eligible_for_the_snapshot() {
    use eitri_terminal::headless::WaitWhy;

    let (link, script) = scripted();
    let mut host = Host::start(Box::new(link), size(160, 48));
    script.live();
    script.push(LinkItem::Waiting(WaitWhy::SnapshotTooLarge));
    host.wait("the mirror is waiting for the snapshot", |h| {
        h.reports
            .iter()
            .any(|r| r.waiting == Some(Some(WaitWhy::SnapshotTooLarge)))
    });
    let first = size(120, 40);
    let last = size(80, 40);
    for cols in (last.cols..=first.cols).rev() {
        host.session.send(SessionCommand::Resize(size(cols, first.rows)));
    }
    wait_for(|| script.resizes().last() == Some(&last));

    script.push(LinkItem::Snapshot(blank_snapshot(first, 0)));
    host.wait("the mirror processed the snapshot at the start of the drag", |h| {
        h.synced() || h.ended().is_some()
    });
    assert_eq!(host.ended(), None);

    script.push(LinkItem::Resized {
        offset: 0,
        size: size(110, 40),
    });
    script.push(LinkItem::Resized { offset: 0, size: last });
    script.push(LinkItem::Output {
        offset: 0,
        bytes: b"ready".to_vec(),
    });
    let want = reference(last, &[Step::Feed(b"ready")]);
    host.wait("the latest grid was applied", |h| h.frame.as_ref() == Some(&want));
}

#[test]
fn a_cell_size_change_is_asked_for_and_colours_go_at_every_connection() {
    let (mut host, script) = synced_scripted(size(80, 24));
    assert!(script.colors() >= 1, "the pane's colours go to the link at the start");
    let asked = script.resizes().len();
    let finer = PtySize {
        cell_width_px: 9,
        cell_height_px: 18,
        ..size(80, 24)
    };
    host.session.send(SessionCommand::Resize(finer));
    wait_for(|| script.resizes().len() == asked + 1);
    assert_eq!(script.resizes()[asked], finer);
    let colors_before = script.colors();
    script.down();
    script.live();
    host.wait("the link came back", |h| {
        h.reports.iter().filter(|r| r.link == Some(LinkState::Live)).count() >= 2
    });
    wait_for(|| script.colors() > colors_before);
}

#[test]
fn a_reattach_snapshot_at_a_size_asked_before_the_link_dropped_is_taken() {
    let (mut host, script) = synced_scripted(size(80, 24));
    host.session.send(SessionCommand::Resize(size(100, 30)));
    wait_for(|| script.resizes().last() == Some(&size(100, 30)));
    // The server applied 100x30, but the link dropped before its marker came; meanwhile the pane went back.
    script.down();
    host.wait("the link dropped", |h| {
        h.saw_link(|s| matches!(s, LinkState::Disconnected { .. }))
    });
    host.session.send(SessionCommand::Resize(size(80, 24)));
    script.live();
    // The server is still at 100x30. Whichever the mirror read first (the reconnect or the pane's resize), the
    // reattach snapshot at that size is taken and 80x24 is asked for by the time it is.
    script.push(LinkItem::Snapshot(blank_snapshot(size(100, 30), 0)));
    host.wait("the reattach snapshot was taken", |h| h.syncs() == 2);
    assert_eq!(host.ended(), None);
    wait_for(|| script.resizes().last() == Some(&size(80, 24)));
    script.push(LinkItem::Resized {
        offset: 0,
        size: size(80, 24),
    });
    script.push(LinkItem::Output {
        offset: 0,
        bytes: b"back".to_vec(),
    });
    let want = reference(size(100, 30), &[Step::Resize(size(80, 24)), Step::Feed(b"back")]);
    host.wait("the pane's size came back through the marker", |h| {
        h.frame.as_ref() == Some(&want)
    });
    assert_eq!(host.ended(), None);
}

#[test]
fn an_ordinary_reattach_snapshot_retires_old_resize_requests() {
    let current = size(80, 24);
    let stale = size(100, 30);
    let (mut host, script) = synced_scripted(current);
    host.session.send(SessionCommand::Resize(stale));
    wait_for(|| script.resizes().last() == Some(&stale));
    script.down();
    host.wait("the link dropped", |h| {
        h.saw_link(|s| matches!(s, LinkState::Disconnected { .. }))
    });

    let colors_before = script.colors();
    host.session.send(SessionCommand::Resize(current));
    host.session.send(SessionCommand::SetColors(TerminalColors::default()));
    wait_for(|| script.colors() > colors_before);
    script.live();
    script.push(LinkItem::Snapshot(blank_snapshot(current, 0)));
    host.wait("the reattach snapshot used the current grid", |h| h.syncs() == 2);
    assert_eq!(script.resizes().last(), Some(&current));

    script.push(LinkItem::Resized { offset: 0, size: stale });
    script.push(LinkItem::Output {
        offset: 0,
        bytes: b"stale".to_vec(),
    });
    let stale_frame = reference(stale, &[Step::Feed(b"stale")]);
    host.wait("the mirror processed the stale marker", |h| {
        h.ended().is_some() || h.frame.as_ref() == Some(&stale_frame)
    });
    assert_eq!(host.ended(), Some(MirrorEnd::Refused(FrameRefused::SizeNotRequested)));
}

#[test]
fn an_offset_gap_and_output_before_a_snapshot_end_the_mirror_with_their_reason() {
    let (mut host, script) = synced_scripted(size(80, 24));
    script.push(LinkItem::Output {
        offset: 0,
        bytes: b"abc".to_vec(),
    });
    script.push(LinkItem::Output {
        offset: 4,
        bytes: b"e".to_vec(),
    });
    host.wait("the gap ended the mirror", |h| h.ended().is_some());
    assert_eq!(
        host.ended(),
        Some(MirrorEnd::Refused(FrameRefused::OffsetGap { expected: 3, got: 4 }))
    );

    let (link, script) = scripted();
    let mut host = Host::start(Box::new(link), size(80, 24));
    script.live();
    script.push(LinkItem::Output {
        offset: 0,
        bytes: b"early".to_vec(),
    });
    host.wait("early output ended the mirror", |h| h.ended().is_some());
    assert_eq!(
        host.ended(),
        Some(MirrorEnd::Refused(FrameRefused::OutputBeforeSnapshot))
    );
}

#[test]
fn an_offset_past_the_end_of_any_stream_is_refused() {
    let (link, script) = scripted();
    let mut host = Host::start(Box::new(link), size(80, 24));
    script.live();
    script.push(LinkItem::Snapshot(blank_snapshot(size(80, 24), u64::MAX - 2)));
    script.push(LinkItem::Output {
        offset: u64::MAX - 2,
        bytes: b"12345".to_vec(),
    });
    host.wait("the overflow ended the mirror", |h| h.ended().is_some());
    assert_eq!(host.ended(), Some(MirrorEnd::Refused(FrameRefused::OffsetOverflow)));
}

#[test]
fn a_replay_ending_inside_a_sequence_or_with_a_c0_control_is_refused() {
    let mut unfinished = blank_snapshot(size(80, 24), 0);
    unfinished.replay.bytes.extend_from_slice(b"\x1b]2;never closed");
    let mut control = blank_snapshot(size(80, 24), 0);
    control.replay.last_input = Some('\x1b');
    for snapshot in [unfinished, control] {
        let (link, script) = scripted();
        let mut host = Host::start(Box::new(link), size(80, 24));
        script.live();
        script.push(LinkItem::Snapshot(snapshot));
        host.wait("the replay was refused", |h| h.ended().is_some());
        assert_eq!(host.ended(), Some(MirrorEnd::Refused(FrameRefused::ReplayUnfinished)));
    }
}

#[test]
fn a_hostile_replay_is_bounded() {
    let mut replay = Vec::new();
    for _ in 0..2_000_000 {
        replay.extend_from_slice(b"\x1b[c");
    }
    replay.extend_from_slice(b"\x1b]2;");
    replay.extend(std::iter::repeat_n(b'a', 20 << 20));
    replay.push(0x07);
    for _ in 0..100_000 {
        replay.extend_from_slice(b"\x1b[22;0t");
    }
    for _ in 0..5_000 {
        replay.extend_from_slice(b"\x1b[22;0t\x1b[>1u");
    }
    assert!(replay.len() < eitri_terminal::mirror::SNAPSHOT_MAX);
    let mut snapshot = blank_snapshot(size(80, 24), 0);
    snapshot.replay.bytes = replay;
    let mut screen = Screen::replayed_untrusted(&snapshot.replay, size(80, 24), TerminalColors::default()).unwrap();
    assert!(screen.take_replies().is_empty(), "replay queries queued an answer");
    drop(screen);
    let (link, script) = scripted();
    let mut host = Host::start(Box::new(link), size(80, 24));
    script.live();
    script.push(LinkItem::Snapshot(snapshot));
    host.wait("the hostile snapshot was taken in", Host::synced);
    host.key(Key::Character("k".into()));
    wait_for(|| !script.writes().is_empty());
    assert_eq!(script.writes(), b"k");
    assert_eq!(host.ended(), None);
    // The 20 MiB title never reached the host: OSC payloads keep their bound in a replay too.
    host.pump();
    assert!(host.events.iter().all(|e| e
        .title
        .as_ref()
        .and_then(|t| t.as_ref())
        .is_none_or(|t| t.len() <= 4096)));
}

#[test]
fn live_osc52_reaches_the_clipboard_but_a_snapshot_does_not() {
    let osc52 = b"\x1b]52;c;aGVsbG8=\x07";
    let mut snapshot = blank_snapshot(size(80, 24), 0);
    snapshot.replay.bytes.extend_from_slice(osc52);
    snapshot.replay.bytes.extend_from_slice(b"\x07");
    let (link, script) = scripted();
    let mut host = Host::start(Box::new(link), size(80, 24));
    script.live();
    script.push(LinkItem::Snapshot(snapshot));
    host.wait("the snapshot", Host::synced);
    script.push(LinkItem::Output {
        offset: 0,
        bytes: MARK.to_vec(),
    });
    let want = reference(size(80, 24), &[Step::Feed(MARK)]);
    host.wait("output after the snapshot", |h| h.frame.as_ref() == Some(&want));
    host.pump();
    assert!(host.events.iter().all(|e| e.clipboard.is_none() && !e.bell));
    script.push(LinkItem::Output {
        offset: MARK.len() as u64,
        bytes: osc52.to_vec(),
    });
    host.wait("the live copy reached the host", |h| {
        h.events.iter().any(|e| e.clipboard.as_deref() == Some("hello"))
    });
}

#[test]
fn a_resync_snapshot_replaces_the_screen_and_resets_the_view() {
    let (mut host, script) = synced_scripted(size(80, 24));
    let lines: Vec<u8> = (0..60).flat_map(|i| format!("old {i}\r\n").into_bytes()).collect();
    script.push(LinkItem::Output {
        offset: 0,
        bytes: lines.clone(),
    });
    let old = reference(size(80, 24), &[Step::Feed(&lines)]);
    host.wait("the old lines", |h| h.frame.as_ref() == Some(&old));
    host.session
        .send(SessionCommand::Scroll(eitri_terminal::ScrollRequest::Pages(1)));
    host.wait("scrolled back", |h| h.frame.as_ref() != Some(&old));
    let mut resync = blank_snapshot(size(80, 24), lines.len() as u64);
    resync.replay.bytes.extend_from_slice(b"new");
    script.push(LinkItem::Snapshot(resync));
    let want = reference(size(80, 24), &[Step::Feed(b"new")]);
    host.wait("the resync shows only the new state, at the bottom", |h| {
        h.frame.as_ref() == Some(&want)
    });
}

#[test]
fn a_mirror_refuses_to_move() {
    let (host, _script) = synced_scripted(size(80, 24));
    let (tx, rx) = mpsc::channel();
    host.session.send(SessionCommand::Detach(tx));
    let answer = rx.recv_timeout(WAIT).unwrap();
    assert_eq!(answer.err(), Some(DetachRefused::ServerTerminal));
}

#[test]
fn a_mirror_has_no_local_pid() {
    let (host, _script) = synced_scripted(size(80, 24));
    assert_eq!(host.session.local_pid(), None);
    let pid = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.session.pid()));
    assert!(
        pid.is_err(),
        "pid() must not hand out a number for a terminal on another machine"
    );
}

#[test]
fn input_typed_while_disconnected_is_refused_however_late_it_is_read() {
    let (mut host, script) = synced_scripted(size(80, 24));
    script.down();
    host.wait("the link went down", |h| {
        h.saw_link(|s| matches!(s, LinkState::Disconnected { .. }))
    });
    host.key(Key::Character("x".into()));
    host.key(Key::Named(NamedKey::Enter));
    // The link comes back at once; the keys may be read before or after that, and are refused either way.
    script.live();
    script.push(LinkItem::Snapshot(blank_snapshot(size(80, 24), 0)));
    host.wait("the keys were refused", |h| h.refused() == 2);
    host.wait("the reconnect's snapshot", |h| h.syncs() == 2);
    host.key(Key::Character("y".into()));
    wait_for(|| !script.writes().is_empty());
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(script.writes(), b"y", "nothing typed while disconnected arrives later");
}

#[test]
fn input_before_the_reconnect_snapshot_is_refused() {
    let (mut host, script) = synced_scripted(size(80, 24));
    script.down();
    script.live();
    host.wait("the link is live again", |h| {
        h.reports.iter().filter(|r| r.link == Some(LinkState::Live)).count() >= 2
    });
    host.key(Key::Character("z".into()));
    host.wait("the key was refused: no snapshot of this connection yet", |h| {
        h.refused() == 1
    });
    assert!(script.writes().is_empty());
}

#[test]
fn input_waiting_for_the_window_is_refused_when_the_link_drops() {
    let (mut host, script) = synced_scripted(size(80, 24));
    script.window(Some(0));
    host.paste("abcdef".into());
    host.key(Key::Character("g".into()));
    std::thread::sleep(Duration::from_millis(50));
    assert!(script.writes().is_empty());
    script.down();
    host.wait("what waited was refused", |h| h.refused() == 7);
    script.window(None);
    script.live();
    script.push(LinkItem::Snapshot(blank_snapshot(size(80, 24), 0)));
    host.wait("the reconnect's snapshot", |h| h.syncs() == 2);
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        script.writes().is_empty(),
        "nothing that waited for the old window is sent later"
    );
}

#[test]
fn dropped_mouse_and_focus_reports_do_not_count_as_refused_keys() {
    use eitri_terminal::mouse::{MouseInput, MouseKind};

    let at = size(80, 24);
    let (mut host, script) = synced_scripted(at);
    let modes = b"\x1b[?1003h\x1b[?1006h\x1b[?1004hMARK";
    script.push(LinkItem::Output {
        offset: 0,
        bytes: modes.to_vec(),
    });
    let want = reference(at, &[Step::Feed(modes)]);
    host.wait("the report modes were parsed", |h| h.frame.as_ref() == Some(&want));
    // Only the first byte of the focus report fits, so its remainder and the mouse report wait.
    script.window(Some(1));
    host.session.send(SessionCommand::FocusReport(true));
    host.session.send(SessionCommand::Mouse(MouseInput {
        kind: MouseKind::Motion(None),
        col: 0,
        line: 0,
        mods: Default::default(),
    }));
    host.key(Key::Character("k".into()));
    let resized = size(81, 24);
    host.session.send(SessionCommand::Resize(resized));
    wait_for(|| script.resizes().contains(&resized));
    assert_eq!(script.writes(), b"\x1b");
    script.down();
    host.wait("the mirror saw the disconnect", |h| {
        h.saw_link(|link| matches!(link, LinkState::Disconnected { .. }))
    });
    assert_eq!(host.refused(), 1);
}

#[test]
fn a_paste_too_large_for_the_buffer_is_refused_whole() {
    let (mut host, script) = synced_scripted(size(80, 24));
    script.window(Some(0));
    let big = "x".repeat(PENDING_INPUT_MAX + 1);
    host.paste(big);
    host.wait("the paste was refused", |h| h.refused() > PENDING_INPUT_MAX as u64);
    script.window(None);
    host.key(Key::Character("k".into()));
    wait_for(|| !script.writes().is_empty());
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(script.writes(), b"k", "no part of the refused paste was sent");
}

#[test]
fn a_program_that_left_the_terminal_keeps_the_mirror_until_its_real_end() {
    let (mut host, script) = synced_scripted(size(80, 24));
    script.push(LinkItem::Exited(ExitInfo {
        code: None,
        signal: None,
        detached: true,
    }));
    host.wait("the detach reached the host", |h| !h.exits.is_empty());
    script.push(LinkItem::Exited(ExitInfo {
        code: Some(3),
        signal: None,
        detached: false,
    }));
    host.wait("the real end reached the host", |h| h.exits.len() == 2);
    assert!(host.exits[0].detached);
    assert_eq!(host.exits[1].code, Some(3));
    assert!(host.wakes.load(Ordering::SeqCst) > 0);
}

#[test]
fn the_last_screen_is_published_when_the_mirror_ends() {
    let (mut host, script) = synced_scripted(size(80, 24));
    script.push(LinkItem::Output {
        offset: 0,
        bytes: b"final words".to_vec(),
    });
    script.push(LinkItem::Ended(MirrorEnd::Replaced));
    let want = reference(size(80, 24), &[Step::Feed(b"final words")]);
    host.wait("the end and the last screen", |h| {
        h.ended().is_some() && h.frame.as_ref() == Some(&want)
    });
}

#[test]
fn a_write_the_link_refuses_is_counted_and_never_sent_later() {
    // The gate was open when the key was sent, but the link's connection changed before the write: the link
    // refuses it, and the mirror never tries it again.
    let (mut host, script) = synced_scripted(size(80, 24));
    script.state.lock().unwrap().refuse_writes = true;
    host.key(Key::Character("a".into()));
    host.wait("the refused write was counted", |h| h.refused() == 1);
    script.state.lock().unwrap().refuse_writes = false;
    host.key(Key::Character("b".into()));
    wait_for(|| !script.writes().is_empty());
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(script.writes(), b"b");
}

#[test]
fn input_opens_while_the_server_waits_for_a_boundary() {
    // After a reconnect the server cannot take a snapshot until the program finishes a sequence, which may need a
    // key: the mirror lets keys through against the mode it last saw.
    let (mut host, script) = synced_scripted(size(80, 24));
    script.down();
    script.live();
    host.wait("the link is live again", |h| {
        h.reports.iter().filter(|r| r.link == Some(LinkState::Live)).count() >= 2
    });
    script.push(LinkItem::Waiting(eitri_terminal::headless::WaitWhy::OpenSequence));
    host.wait("the wait reached the host", |h| {
        h.reports.iter().any(|r| r.waiting.flatten().is_some())
    });
    host.key(Key::Named(NamedKey::Enter));
    wait_for(|| !script.writes().is_empty());
    assert_eq!(script.writes(), b"\r");
}

#[test]
fn many_short_connections_asking_one_size_keep_an_older_grid_acceptable() {
    let (mut host, script) = synced_scripted(size(80, 24));
    host.session.send(SessionCommand::Resize(size(100, 30)));
    wait_for(|| script.resizes().last() == Some(&size(100, 30)));
    // The server took 100x30; then the pane went back to 80x24 over twenty connections that never answered.
    host.session.send(SessionCommand::Resize(size(80, 24)));
    for round in 0..20 {
        script.down();
        script.live();
        host.wait("the connection came and went", |h| {
            h.reports.iter().filter(|r| r.link == Some(LinkState::Live)).count() >= round + 2
        });
    }
    script.push(LinkItem::Snapshot(blank_snapshot(size(100, 30), 0)));
    host.wait("the snapshot at the server's grid was taken", |h| h.syncs() == 2);
    assert_eq!(host.ended(), None);
}
