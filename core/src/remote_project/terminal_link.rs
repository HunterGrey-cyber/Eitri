//! Nonblocking mirror transport. Its task owns channels, never the helper's runtime owner.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use agent::remote_client::terminal_transport::{tokio, tokio_stream, tonic, wire};
use agent::remote_client::{HelperClient, LinkChecker};
use eitri_terminal::mirror::{
    FrameRefused, LinkItem, LinkState, MirrorEnd, SnapshotAssembler, SnapshotPartWire, TermLink, WireTail,
};
use eitri_terminal::{ExitInfo, PtySize, TerminalColors, WaitWhy};
use tokio::sync::{mpsc, Notify};
use tokio_stream::StreamExt;
use wire::terminal_client::TerminalClient;
use wire::terminal_client_frame::Frame as Up;
use wire::terminal_server_frame::Frame as Down;

use super::terminal_facts::{TerminalFacts, TerminalFactsHandle};

const SEND_WINDOW: u64 = 256 * 1024;
const ITEMS_BUDGET: usize = 16 * 1024 * 1024;
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const TERMINAL_RECONNECT_BACKOFF: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(10),
];

fn reconnect_delay(attempt: usize) -> Duration {
    TERMINAL_RECONNECT_BACKOFF[attempt.min(TERMINAL_RECONNECT_BACKOFF.len() - 1)]
}

#[derive(Clone, Debug)]
pub enum TerminalTarget {
    Open {
        command_id: String,
        cwd: PathBuf,
    },
    Attach {
        terminal_id: String,
        daemon_instance: String,
        boot_id: String,
    },
}

#[derive(Clone, Debug)]
pub struct TerminalLinkOptions {
    /// The installation that started this terminal; empty selects the window's current installation.
    pub installation: String,
    pub target: TerminalTarget,
    pub size: PtySize,
    pub colors: TerminalColors,
}

/// What the link learned about its terminal, observable after the link has moved into the mirror: the open
/// identity, and whether the link ended because the terminal no longer exists.
#[derive(Clone, Default)]
pub struct OpenAnswer {
    inner: Arc<(Mutex<Option<(String, String, String)>>, Condvar)>,
    gone: Arc<std::sync::atomic::AtomicBool>,
    ended: Arc<std::sync::atomic::AtomicBool>,
}

impl OpenAnswer {
    pub fn get(&self) -> Option<(String, String, String)> {
        self.inner.0.lock().unwrap().clone()
    }

    /// A worker may wait for the answer. Nothing here holds input: the mirror opens input at the snapshot, so a pane
    /// that must record the answer before the first key holds its own keys until `opened` has returned.
    /// Returns early, with what there is, once the link has ended.
    pub fn wait(&self, deadline: Instant) -> Option<(String, String, String)> {
        let mut answer = self.inner.0.lock().unwrap();
        while answer.is_none() && !self.ended() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            answer = self.inner.1.wait_timeout(answer, remaining).unwrap().0;
        }
        answer.clone()
    }

    /// Whether the link's `Gone` ending says the terminal no longer exists on its server (an attach answered "not
    /// found"). Every other `Gone` (a refused connection or Hello, a protocol refusal, a host key that no longer
    /// matches) leaves a terminal that may still run, so its owner keeps the record.
    pub fn terminal_gone(&self) -> bool {
        self.gone.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether the link has ended (or was dropped): an answer still missing then never comes, so nothing should
    /// wait for it.
    pub fn ended(&self) -> bool {
        self.ended.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Set by the link when it ends or is dropped, waking every waiter; public for links built elsewhere.
    pub fn finish(&self) {
        let _answer = self.inner.0.lock().unwrap();
        self.ended.store(true, std::sync::atomic::Ordering::SeqCst);
        self.inner.1.notify_all();
    }

    /// Set by the link before it reports that ending; public for links built elsewhere.
    pub fn mark_gone(&self) {
        self.gone.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn set(&self, identity: (String, String, String)) {
        let mut answer = self.inner.0.lock().unwrap();
        if answer.is_none() {
            *answer = Some(identity);
            self.inner.1.notify_all();
        }
    }
}

struct State {
    items: VecDeque<LinkItem>,
    bytes: usize,
    pending_state: bool,
    live: bool,
    ready: bool,
    sent: u64,
    written: u64,
    sender: Option<mpsc::Sender<wire::TerminalClientFrame>>,
    size: PtySize,
    colors: wire::TermColors,
    terminal_id: Option<String>,
    opened: OpenAnswer,
    bell: PipeWriter,
    /// The helper connection the attachment runs on, once the link has one (not while it retries its first).
    connection: Weak<TerminalConnection>,
    /// The link's own lines (`[remote terminal …] …`): `None` writes them to stderr; `Some` keeps them for
    /// [`TerminalLink::take_notes`].
    notes: Option<Vec<String>>,
}

impl State {
    fn ring(&mut self) {
        let _ = self.bell.write(&[1]);
    }

    fn push(&mut self, item: LinkItem) {
        self.pending_state |= matches!(item, LinkItem::Link { .. } | LinkItem::Ended(_));
        self.bytes += item_bytes(&item);
        self.items.push_back(item);
        self.ring();
    }

    fn write_input(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if !self.live || !self.ready || self.pending_state {
            return Err(io::ErrorKind::NotConnected.into());
        }
        let room = SEND_WINDOW.saturating_sub(self.sent - self.written) as usize;
        let n = bytes.len().min(room).min(64 * 1024);
        if n == 0 && !bytes.is_empty() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let next = self.sent.checked_add(n as u64).ok_or(io::ErrorKind::InvalidData)?;
        let frame = wire::TerminalClientFrame {
            frame: Some(Up::Input(wire::InputBytes {
                offset: self.sent,
                data: bytes[..n].to_vec(),
            })),
        };
        match self.sender.as_ref().ok_or(io::ErrorKind::NotConnected)?.try_send(frame) {
            Ok(()) => {
                self.sent = next;
                Ok(n)
            }
            Err(mpsc::error::TrySendError::Full(_)) => Err(io::ErrorKind::WouldBlock.into()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(io::ErrorKind::NotConnected.into()),
        }
    }

    fn snapshot(&mut self, written: u64, previous_sent: Option<u64>) -> Result<(), Failure> {
        if written > self.sent || written < self.written {
            return Err(gone("the terminal server sent an invalid snapshot input offset"));
        }
        self.establish(written, previous_sent);
        self.ready = true;
        Ok(())
    }

    /// The host's written count for this client at a reattach that left bytes unacknowledged, told before any
    /// snapshot: what did not arrive is reported once, and input resumes from there. Readiness still waits for the
    /// snapshot or wait that follows.
    fn settle(&mut self, written: u64, previous_sent: u64) -> Result<(), Failure> {
        if written > self.sent || written < self.written {
            return Err(gone("the terminal server acknowledged an invalid input offset"));
        }
        self.establish(written, Some(previous_sent));
        Ok(())
    }

    fn establish(&mut self, written: u64, previous_sent: Option<u64>) {
        if let Some(sent) = previous_sent {
            let lost = sent.saturating_sub(written);
            self.sent = written;
            if lost > 0 {
                self.link(LinkState::Live, lost);
            }
        }
        self.written = written;
    }

    fn link(&mut self, state: LinkState, lost_input_bytes: u64) {
        self.live = state == LinkState::Live;
        if !self.live {
            self.ready = false;
            self.sender = None;
        }
        self.push(LinkItem::Link {
            state,
            lost_input_bytes,
        });
    }
}

fn item_bytes(item: &LinkItem) -> usize {
    std::mem::size_of::<LinkItem>()
        + match item {
            LinkItem::Output { bytes, .. } => bytes.len(),
            LinkItem::Snapshot(snapshot) => {
                snapshot.replay.bytes.len()
                    + match &snapshot.tail {
                        eitri_terminal::Tail::ReadAhead(bytes) | eitri_terminal::Tail::Raw { bytes, .. } => bytes.len(),
                    }
            }
            _ => 0,
        }
}

/// The runtime owner and one facts refresher shared by this installation's attachments.
pub(super) struct TerminalConnection {
    client: HelperClient,
    channel: tonic::transport::Channel,
    runtime: tokio::runtime::Handle,
    checker: LinkChecker,
    owners: Arc<Mutex<Vec<Weak<FactOwner>>>>,
    refresh: tokio::task::JoinHandle<()>,
}

impl TerminalConnection {
    pub(super) fn new(client: HelperClient) -> io::Result<Arc<Self>> {
        let (channel, runtime) = client.terminal_channel().map_err(io::Error::other)?;
        let checker = client.link_checker(None)?;
        let owners = Arc::new(Mutex::new(Vec::new()));
        let refresh = runtime.spawn(refresh_facts(channel.clone(), owners.clone()));
        Ok(Arc::new(Self {
            client,
            channel,
            runtime,
            checker,
            owners,
            refresh,
        }))
    }
}

impl Drop for TerminalConnection {
    fn drop(&mut self) {
        self.refresh.abort();
    }
}

struct FactOwner {
    facts: TerminalFactsHandle,
    state: Mutex<(Option<String>, bool)>,
}

impl FactOwner {
    fn activate(&self, id: &str) {
        let mut state = self.state.lock().unwrap();
        if !state.1 {
            state.0 = Some(id.to_owned());
        }
    }

    fn clear(&self, closed: bool) {
        let mut state = self.state.lock().unwrap();
        state.1 |= closed;
        if let Some(id) = state.0.take() {
            self.facts.remove(&id);
        }
    }
}

async fn refresh_facts(channel: tonic::transport::Channel, owners: Arc<Mutex<Vec<Weak<FactOwner>>>>) {
    refresh_facts_with(owners, move || {
        let mut service = TerminalClient::new(channel.clone()).max_decoding_message_size(2 * 1024 * 1024);
        async move {
            bounded(service.list_terminals(wire::ListTerminalsRequest {}))
                .await
                .map(|r| r.into_inner())
        }
    })
    .await;
}

async fn refresh_facts_with<F, Fut>(owners: Arc<Mutex<Vec<Weak<FactOwner>>>>, mut list: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<wire::ListTerminalsResponse, tonic::Status>>,
{
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut refresh_at = tokio::time::Instant::now();
    loop {
        tick.tick().await;
        let active: Vec<_> = {
            let mut owners = owners.lock().unwrap();
            owners.retain(|owner| owner.strong_count() > 0);
            owners.iter().filter_map(Weak::upgrade).collect()
        };
        let mut requested = false;
        let mut any = false;
        for owner in &active {
            let state = owner.state.lock().unwrap();
            if let Some(id) = &state.0 {
                any = true;
                requested |= !owner
                    .facts
                    .take_refresh_requests_for(&BTreeSet::from([id.clone()]))
                    .is_empty();
            }
        }
        if !any || (!requested && tokio::time::Instant::now() < refresh_at) {
            continue;
        }
        // Metadata never shares a wait or a failure path with an attachment.
        let result = list().await;
        refresh_at = tokio::time::Instant::now() + Duration::from_secs(2);
        if let Ok(list) = result {
            cache_facts(&active, list);
        }
    }
}

fn cache_facts(owners: &[Arc<FactOwner>], list: wire::ListTerminalsResponse) {
    let at = Instant::now();
    for owner in owners {
        // The active id is checked again after the RPC, so a disconnect cannot publish stale facts.
        let state = owner.state.lock().unwrap();
        let Some(id) = &state.0 else { continue };
        let info = list.terminals.iter().take(wire::TERM_LIST_MAX).find(|info| {
            info.terminal_id == *id
                && valid_id(&info.terminal_id)
                && info.exit.as_ref().is_none_or(|exit| exit_info(exit.clone()).is_ok())
        });
        if info.is_some_and(|info| info.exit.is_some()) {
            // An ended terminal has no job to ask about.
            owner.facts.set(
                id.clone(),
                TerminalFacts {
                    busy: Some(false),
                    name: None,
                    cwd: None,
                    at,
                },
            );
        } else if let Some(info) = info {
            let cap = |value: &str, max: usize| {
                let mut end = value.len().min(max);
                while !value.is_char_boundary(end) {
                    end -= 1;
                }
                value[..end].to_owned()
            };
            let name = cap(&info.foreground_name, wire::TERM_NAME_MAX);
            let cwd = cap(&info.cwd, wire::TERM_PATH_MAX);
            owner.facts.set(
                id.clone(),
                TerminalFacts {
                    busy: info.foreground_job,
                    name: (!name.is_empty()).then_some(name),
                    cwd: cwd.starts_with('/').then(|| PathBuf::from(cwd)),
                    at,
                },
            );
        } else {
            owner.facts.remove(id);
        }
    }
}

/// One attachment, held by the mirror. Dropping it cancels its stream without stopping the server terminal.
pub struct TerminalLink {
    state: Arc<Mutex<State>>,
    reader: PipeReader,
    changed: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
    // The owner stays outside the runtime task, so its last drop can join the runtime thread.
    _connection: LinkConnection,
    owner: Arc<FactOwner>,
}

type OpenConnection = dyn Fn() -> io::Result<Arc<TerminalConnection>> + Send + Sync;

enum LinkConnection {
    Ready(Arc<TerminalConnection>),
    Retry {
        client: HelperClient,
        error: String,
        open: Arc<OpenConnection>,
    },
}

enum Start {
    Ready(tonic::transport::Channel, LinkChecker),
    Retry(String, Weak<OpenConnection>),
}

pub(super) fn retry_terminal_connection(error: &io::Error) -> bool {
    !matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::InvalidInput
    )
}

impl TerminalLink {
    /// `client` is the window's separate terminal connection. No RPC waits on this caller.
    pub fn new(client: HelperClient, options: TerminalLinkOptions, facts: TerminalFactsHandle) -> io::Result<Self> {
        Self::on_connection(TerminalConnection::new(client)?, options, facts)
    }

    pub(super) fn on_connection(
        connection: Arc<TerminalConnection>,
        options: TerminalLinkOptions,
        facts: TerminalFactsHandle,
    ) -> io::Result<Self> {
        Self::start(LinkConnection::Ready(connection), options, facts)
    }

    pub(super) fn retry_connection(
        client: HelperClient,
        error: io::Error,
        options: TerminalLinkOptions,
        facts: TerminalFactsHandle,
        open: impl Fn() -> io::Result<Arc<TerminalConnection>> + Send + Sync + 'static,
    ) -> io::Result<Self> {
        Self::start(
            LinkConnection::Retry {
                client,
                error: error.to_string(),
                open: Arc::new(open),
            },
            options,
            facts,
        )
    }

    fn start(connection: LinkConnection, options: TerminalLinkOptions, facts: TerminalFactsHandle) -> io::Result<Self> {
        if let TerminalTarget::Attach { terminal_id, .. } = &options.target {
            if !valid_id(terminal_id) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid terminal id"));
            }
        }
        let (reader, writer) = io::pipe()?;
        nonblocking(&reader)?;
        nonblocking(&writer)?;
        let state = Arc::new(Mutex::new(State {
            items: VecDeque::new(),
            bytes: 0,
            pending_state: false,
            live: false,
            ready: false,
            sent: 0,
            written: 0,
            sender: None,
            size: options.size,
            colors: colors_wire(&options.colors),
            terminal_id: None,
            opened: OpenAnswer::default(),
            bell: writer,
            connection: Weak::new(),
            notes: None,
        }));
        state.lock().unwrap().link(LinkState::Connecting, 0);
        let changed = Arc::new(Notify::new());
        let owner = Arc::new(FactOwner {
            facts,
            state: Mutex::new((None, false)),
        });
        let (host, runtime, start) = match &connection {
            LinkConnection::Ready(connection) => {
                connection.owners.lock().unwrap().push(Arc::downgrade(&owner));
                state.lock().unwrap().connection = Arc::downgrade(connection);
                (
                    connection.client.host(),
                    connection.runtime.clone(),
                    Start::Ready(connection.channel.clone(), connection.checker.clone()),
                )
            }
            LinkConnection::Retry { client, error, open } => {
                let (_, runtime) = client.terminal_channel().map_err(io::Error::other)?;
                (
                    client.host(),
                    runtime,
                    Start::Retry(error.clone(), Arc::downgrade(open)),
                )
            }
        };
        let worker = Worker {
            state: state.clone(),
            changed: changed.clone(),
            owner: owner.clone(),
            host: host.to_owned(),
            client_id: uuid::Uuid::new_v4().simple().to_string(),
            options,
            failures: HashSet::new(),
            started: Instant::now(),
            established: false,
        };
        let task = runtime.spawn(worker.start(start));
        Ok(Self {
            state,
            reader,
            changed,
            task,
            _connection: connection,
            owner,
        })
    }

    /// The answered terminal id, daemon instance and boot id. The pane must persist these through
    /// the record's `opened` before forwarding keys; the mirror itself permits input at its snapshot.
    pub fn opened(&self) -> Option<(String, String, String)> {
        self.answer_handle().get()
    }

    pub fn answer_handle(&self) -> OpenAnswer {
        self.state.lock().unwrap().opened.clone()
    }

    pub fn terminal_id(&self) -> Option<String> {
        self.state.lock().unwrap().terminal_id.clone()
    }

    /// Input bytes put on the stream that the server has not said it wrote yet: what a close sent now could overtake.
    pub fn input_unwritten(&self) -> u64 {
        let state = self.state.lock().unwrap();
        state.sent.saturating_sub(state.written)
    }

    /// Closes the terminal `terminal_id` over this link's own helper connection, giving up at `deadline`: no new
    /// connection (no new ssh child) is opened for it. `None` when the link has no connection yet (its first is still
    /// being retried). Blocking: not for the GTK thread.
    pub fn close_terminal(&self, terminal_id: &str, deadline: Instant) -> Option<io::Result<Option<String>>> {
        let connection = self.state.lock().unwrap().connection.upgrade()?;
        Some(super::terminals::close_before(
            &connection.client,
            terminal_id,
            deadline,
        ))
    }

    /// From now on the link's own lines are kept for [`Self::take_notes`] instead of going to stderr: for a caller
    /// whose stderr is the terminal it draws on in raw mode, where a bare newline would not return the carriage.
    pub fn keep_notes(&self) {
        self.state.lock().unwrap().notes.get_or_insert_with(Vec::new);
    }

    /// The lines kept since the last call (each already escaped for display).
    pub fn take_notes(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .notes
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }
}

impl Drop for TerminalLink {
    fn drop(&mut self) {
        self.owner.clear(true);
        self.task.abort();
        let answer = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .opened
            .clone();
        answer.finish();
    }
}

impl TermLink for TerminalLink {
    fn readiness(&self) -> BorrowedFd<'_> {
        self.reader.as_fd()
    }

    fn take(&mut self) -> Vec<LinkItem> {
        // Drain and clear under the producer's lock: an arrival can never lose its readable doorbell.
        let mut state = self.state.lock().unwrap();
        let mut buf = [0; 256];
        while self.reader.read(&mut buf).is_ok_and(|n| n > 0) {}
        let items = state.items.drain(..).collect();
        state.bytes = 0;
        state.pending_state = false;
        self.changed.notify_one();
        items
    }

    fn write_input(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.state.lock().unwrap().write_input(bytes)
    }

    fn request_resize(&mut self, size: PtySize) {
        self.state.lock().unwrap().size = size;
        self.changed.notify_one();
    }

    fn set_colors(&mut self, colors: &TerminalColors) {
        self.state.lock().unwrap().colors = colors_wire(colors);
        self.changed.notify_one();
    }
}

struct Worker {
    state: Arc<Mutex<State>>,
    changed: Arc<Notify>,
    owner: Arc<FactOwner>,
    host: String,
    client_id: String,
    options: TerminalLinkOptions,
    failures: HashSet<FailureKind>,
    started: Instant,
    established: bool,
}

#[derive(Debug)]
enum Failure {
    Retry(tonic::Status),
    End(MirrorEnd),
    Finished,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum FailureKind {
    Rpc(tonic::Code),
    Link,
    Connect,
}

fn gone(problem: &str) -> Failure {
    Failure::End(MirrorEnd::Gone(problem.to_owned()))
}

impl From<tonic::Status> for Failure {
    fn from(status: tonic::Status) -> Self {
        if matches!(
            status.code(),
            tonic::Code::InvalidArgument
                | tonic::Code::PermissionDenied
                | tonic::Code::FailedPrecondition
                | tonic::Code::Unimplemented
        ) {
            gone(&format!("the terminal connection was refused ({})", status.code()))
        } else {
            Self::Retry(status)
        }
    }
}
impl From<FrameRefused> for Failure {
    fn from(why: FrameRefused) -> Self {
        Self::End(MirrorEnd::Refused(why))
    }
}

impl Worker {
    async fn start(mut self, start: Start) {
        let mut attempt = 0usize;
        match start {
            Start::Ready(channel, checker) => self.run(channel, checker, attempt).await,
            Start::Retry(error, open) => {
                let Some(connection) = self.recover(error, open, &mut attempt).await else {
                    return;
                };
                connection.owners.lock().unwrap().push(Arc::downgrade(&self.owner));
                self.state.lock().unwrap().connection = Arc::downgrade(&connection);
                // Keep the recovered connection alive outside its runtime. Dropping the link aborts this
                // task, and dropping the set in turn aborts the attachment on the terminal runtime.
                let mut attachment = tokio::task::JoinSet::new();
                attachment.spawn_on(
                    self.run(connection.channel.clone(), connection.checker.clone(), attempt),
                    &connection.runtime,
                );
                let _ = attachment.join_next().await;
            }
        }
    }

    async fn run(mut self, channel: tonic::transport::Channel, checker: LinkChecker, mut attempt: usize) {
        loop {
            self.established = false;
            let result = self.connect(&channel).await;
            self.offline();
            if self.established {
                attempt = 0;
            }
            match result {
                Err(Failure::End(end)) => {
                    let mut state = self.state.lock().unwrap();
                    state.push(LinkItem::Ended(end));
                    state.opened.finish();
                    return;
                }
                Err(Failure::Finished) => {
                    self.state.lock().unwrap().opened.finish();
                    return;
                }
                Ok(()) => unreachable!(),
                Err(Failure::Retry(status)) => {
                    let kind = FailureKind::Rpc(status.code());
                    if self.note_failure(kind) {
                        self.note(format!(
                            "[remote terminal {}] {}",
                            crate::onboarding::display::display_name(&self.host),
                            crate::onboarding::display::display_text_cut(&status.to_string(), 1024)
                        ));
                    }
                }
            }
            self.disconnected();
            loop {
                self.wait_to_retry(&mut attempt).await;
                if let agent::ssh::LinkCheck::Failed(error) = checker.check().await {
                    if self.note_failure(FailureKind::Link) {
                        self.note(format!(
                            "[remote terminal {}] link check: {}",
                            crate::onboarding::display::display_name(&self.host),
                            crate::onboarding::display::display_text_cut(&error.to_string(), 1024)
                        ));
                    }
                } else {
                    break;
                }
            }
        }
    }

    async fn recover(
        &mut self,
        mut error: String,
        open: Weak<OpenConnection>,
        attempt: &mut usize,
    ) -> Option<Arc<TerminalConnection>> {
        loop {
            if self.note_failure(FailureKind::Connect) {
                self.note(format!(
                    "[remote terminal {}] connecting: {}",
                    crate::onboarding::display::display_name(&self.host),
                    crate::onboarding::display::display_text_cut(&error, 1024)
                ));
            }
            self.disconnected();
            self.wait_to_retry(attempt).await;
            let open = open.upgrade()?;
            let (tell, result) = tokio::sync::oneshot::channel();
            // A blocking connection must be free to finish without delaying runtime shutdown.
            let started = std::thread::Builder::new()
                .name("terminal-connect".into())
                .spawn(move || {
                    let _ = tell.send(open());
                });
            let connection = match started {
                Ok(_) => result.await.unwrap_or_else(|e| Err(io::Error::other(e))),
                Err(e) => Err(e),
            };
            match connection {
                Ok(ready) => return Some(ready),
                Err(e) if retry_terminal_connection(&e) => error = e.to_string(),
                Err(e) => {
                    let mut state = self.state.lock().unwrap();
                    state.push(LinkItem::Ended(MirrorEnd::Gone(e.to_string())));
                    state.opened.finish();
                    return None;
                }
            }
        }
    }

    fn disconnected(&self) {
        self.state.lock().unwrap().link(
            LinkState::Disconnected {
                since_ms: self.started.elapsed().as_millis() as u64,
            },
            0,
        );
    }

    async fn wait_to_retry(&self, attempt: &mut usize) {
        let wait = reconnect_delay(*attempt);
        self.state.lock().unwrap().link(
            LinkState::Reconnecting {
                attempt: attempt.saturating_add(1).min(u32::MAX as usize) as u32,
                next_in_ms: wait.as_millis() as u64,
            },
            0,
        );
        tokio::time::sleep(wait).await;
        *attempt = attempt.saturating_add(1);
    }

    fn note_failure(&mut self, kind: FailureKind) -> bool {
        self.failures.len() < 64 && self.failures.insert(kind)
    }

    /// One of the link's own lines: to stderr, or kept for the owner that asked ([`TerminalLink::keep_notes`]).
    fn note(&self, line: String) {
        let mut state = self.state.lock().unwrap();
        let Some(notes) = state.notes.as_mut() else {
            drop(state);
            eprintln!("{line}");
            return;
        };
        // `note_failure` already lets each kind through once; the bound is for an owner that never takes them.
        if notes.len() < 64 {
            notes.push(line);
            state.ring();
        }
    }

    fn offline(&self) {
        let mut state = self.state.lock().unwrap();
        state.live = false;
        state.ready = false;
        state.sender = None;
        drop(state);
        self.owner.clear(false);
    }

    async fn connect(&mut self, channel: &tonic::transport::Channel) -> Result<(), Failure> {
        let mut helper = wire::helper_client::HelperClient::new(channel.clone());
        let hello = bounded(helper.hello(wire::HelloRequest {
            protocol_major: wire::PROTOCOL_MAJOR,
            protocol_minor: wire::PROTOCOL_MINOR,
            client_version: env!("CARGO_PKG_VERSION").into(),
        }))
        .await?
        .into_inner();
        check_hello(&hello, &self.host)?;
        let mut service = TerminalClient::new(channel.clone()).max_decoding_message_size(2 * 1024 * 1024);
        if let TerminalTarget::Open { command_id, cwd } = &self.options.target {
            let (size, colors) = {
                let state = self.state.lock().unwrap();
                (size_wire(state.size), state.colors.clone())
            };
            let request = wire::OpenTerminalRequest {
                command_id: command_id.clone(),
                cwd: cwd
                    .to_str()
                    .ok_or_else(|| gone("the terminal directory is not UTF-8"))?
                    .into(),
                size: Some(size),
                colors: Some(colors),
            };
            let mut retry = 0;
            let opened = loop {
                let result = bounded(service.open_terminal(request.clone())).await;
                match result {
                    Ok(response) => break response.into_inner(),
                    Err(status) if retry < 2 && retryable(&status) => {
                        tokio::time::sleep([Duration::from_millis(500), Duration::from_secs(2)][retry]).await;
                        retry += 1;
                    }
                    Err(status) if retryable(&status) => return Err(status.into()),
                    Err(_) => {
                        return Err(Failure::End(MirrorEnd::Gone(format!(
                            "the terminal on {} could not be opened",
                            self.host
                        ))))
                    }
                }
            };
            if !valid_id(&opened.terminal_id) || !valid_identity(&opened.daemon_instance) {
                return Err(gone("the terminal server sent an invalid open identity"));
            }
            {
                let mut state = self.state.lock().unwrap();
                state.opened.set((
                    opened.terminal_id.clone(),
                    opened.daemon_instance.clone(),
                    hello.boot_id.clone(),
                ));
                state.ring();
            }
            self.options.target = TerminalTarget::Attach {
                terminal_id: opened.terminal_id,
                daemon_instance: opened.daemon_instance,
                boot_id: hello.boot_id.clone(),
            };
        }
        let TerminalTarget::Attach { terminal_id, .. } = &self.options.target else {
            unreachable!()
        };
        let id = terminal_id.clone();
        self.state.lock().unwrap().terminal_id = Some(id.clone());
        let (tx, rx) = mpsc::channel(8);
        let (size, colors, sent, written_before_attach) = {
            let state = self.state.lock().unwrap();
            (state.size, state.colors.clone(), state.sent, state.written)
        };
        tx.send(wire::TerminalClientFrame {
            frame: Some(Up::Hello(wire::AttachHello {
                terminal_id: id.clone(),
                client_id: self.client_id.clone(),
                input_sent: sent,
                size: Some(size_wire(size)),
                colors: Some(colors.clone()),
            })),
        })
        .await
        .map_err(|_| tonic::Status::unavailable("attachment ended"))?;
        let response = bounded(service.attach(tokio_stream::wrappers::ReceiverStream::new(rx))).await;
        let stream = match response {
            Ok(response) => response.into_inner(),
            Err(status) => return Err(self.attach_error(status, &hello)),
        };
        {
            let state = self.state.lock().unwrap();
            state
                .opened
                .set((id.clone(), hello.daemon_instance.clone(), hello.boot_id.clone()));
        }
        self.owner.activate(&id);
        self.follow(tx, stream, &hello, (size, colors, sent, written_before_attach))
            .await
    }

    async fn follow(
        &mut self,
        tx: mpsc::Sender<wire::TerminalClientFrame>,
        mut stream: impl tokio_stream::Stream<Item = Result<wire::TerminalServerFrame, tonic::Status>> + Unpin,
        hello: &wire::HelloResponse,
        initial: (PtySize, wire::TermColors, u64, u64),
    ) -> Result<(), Failure> {
        let (mut size, mut colors, sent, written_before_attach) = initial;
        {
            let mut state = self.state.lock().unwrap();
            state.sender = Some(tx.clone());
            state.link(LinkState::Live, 0);
        }
        let mut assembler = SnapshotAssembler::new();
        // The host's written count for this client is known: nothing was left unacknowledged, or the host told it on
        // this stream (its `InputWritten` before anything else, or a snapshot).
        let mut settled = sent == written_before_attach;
        loop {
            // Do not poll the stream past the budget: HTTP/2 then applies pressure to the host's bounded queue.
            let can_read = self.state.lock().unwrap().bytes < ITEMS_BUDGET;
            let (wanted_size, wanted_colors) = {
                let state = self.state.lock().unwrap();
                (state.size, state.colors.clone())
            };
            let owed = size != wanted_size || colors != wanted_colors;
            tokio::select! {
                frame = stream.next(), if can_read => {
                    if !matches!(&frame, Some(Ok(_))) { self.offline(); }
                    let frame = match frame {
                        Some(Ok(frame)) => frame,
                        None if assembler.in_progress() => return Err(FrameRefused::SnapshotIncomplete.into()),
                        None => return Err(tonic::Status::unavailable("attachment stream ended").into()),
                        Some(Err(status)) => return Err(self.attach_error(status, hello)),
                    };
                    let frame = frame.frame.ok_or_else(|| gone("the terminal server sent an empty frame"))?;
                    let item = decode(frame, &mut assembler)?;
                    if let Some(mut item) = item {
                        // An exit with no snapshot or wait before it is the terminal's saved exit record (its host has
                        // gone): passed on as it is, so the mirror shows how the program ended, whether it exited by
                        // itself or was closed (a hang-up shows as its signal). It is final even when the program
                        // left the terminal: no host is left to report a later end, so another attach would only
                        // read the same record again.
                        if let LinkItem::Exited(exit) = &mut item {
                            if !self.established {
                                exit.detached = false;
                            }
                        }
                        let ended = matches!(item, LinkItem::Ended(_));
                        let finished = matches!(item, LinkItem::Exited(ExitInfo { detached: false, .. }));
                        {
                            let mut state = self.state.lock().unwrap();
                            match &item {
                                LinkItem::Snapshot(snapshot) => {
                                    let previous_sent = (!settled).then_some(sent);
                                    state.snapshot(snapshot.input_written, previous_sent)?;
                                    settled = true;
                                    self.established = true;
                                }
                                LinkItem::InputWritten(upto) if !settled => {
                                    // The host's count for this client, told before the screen: the same settling as a
                                    // snapshot's, so a wait that comes first can take keys.
                                    state.settle(*upto, sent)?;
                                    settled = true;
                                }
                                LinkItem::InputWritten(upto) => {
                                    if *upto < state.written || *upto > state.sent { return Err(gone("the terminal server acknowledged an invalid input offset")); }
                                    state.written = *upto;
                                }
                                LinkItem::Waiting(_) => { self.established = true; state.ready = settled; }
                                _ => {}
                            }
                            if ended || finished { state.live = false; state.ready = false; state.sender = None; }
                            state.push(item);
                        }
                        if ended || finished { return Err(Failure::Finished); }
                    }
                }
                _ = self.changed.notified() => {},
                permit = tx.reserve(), if owed => {
                    let permit = permit.map_err(|_| tonic::Status::unavailable("attachment request side ended"))?;
                    let frame = if size != wanted_size {
                        size = wanted_size;
                        Up::Resize(size_wire(size))
                    } else {
                        colors = wanted_colors;
                        Up::Colors(colors.clone())
                    };
                    permit.send(wire::TerminalClientFrame { frame: Some(frame) });
                }
            }
        }
    }

    fn attach_error(&self, status: tonic::Status, hello: &wire::HelloResponse) -> Failure {
        self.offline();
        if status.code() != tonic::Code::NotFound {
            return status.into();
        }
        let TerminalTarget::Attach {
            boot_id,
            daemon_instance,
            ..
        } = &self.options.target
        else {
            unreachable!()
        };
        let reason = if boot_id != &hello.boot_id {
            format!("the server {} restarted", self.host)
        } else if status.message() == wire::TERM_GONE_NOTE {
            format!("the terminal's process on {} ended (killed, or out of memory); programs that ignore hangups may still run there", self.host)
        } else if daemon_instance != &hello.daemon_instance {
            format!(
                "the terminal on {} is no longer recorded after its Eitri server restarted",
                self.host
            )
        } else {
            format!("the terminal on {} was closed", self.host)
        };
        // Told before the ending is pushed, so whoever reads the ending reads this with it.
        self.state.lock().unwrap().opened.mark_gone();
        Failure::End(MirrorEnd::Gone(reason))
    }
}

fn check_hello(hello: &wire::HelloResponse, host: &str) -> Result<(), Failure> {
    if hello.protocol_major != wire::PROTOCOL_MAJOR || !hello.capabilities.iter().any(|cap| cap == wire::CAP_TERMINAL) {
        return Err(gone(&format!(
            "running on an incompatible Eitri server on {host}; stop it with `eitri host stop`"
        )));
    }
    if !valid_identity(&hello.daemon_instance) || !valid_identity(&hello.boot_id) {
        return Err(gone("the terminal server sent an invalid Hello identity"));
    }
    Ok(())
}

async fn bounded<T>(future: impl std::future::Future<Output = Result<T, tonic::Status>>) -> Result<T, tonic::Status> {
    tokio::time::timeout(RPC_TIMEOUT, future)
        .await
        .unwrap_or_else(|_| Err(tonic::Status::deadline_exceeded("terminal RPC timed out")))
}

fn retryable(status: &tonic::Status) -> bool {
    matches!(
        status.code(),
        tonic::Code::Unavailable
            | tonic::Code::Unknown
            | tonic::Code::DeadlineExceeded
            | tonic::Code::Cancelled
            | tonic::Code::ResourceExhausted
    )
}

fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn valid_identity(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}
fn nonblocking(fd: &impl AsRawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn size_wire(size: PtySize) -> wire::TermSize {
    wire::TermSize {
        columns: size.cols.into(),
        rows: size.rows.into(),
        cell_width: size.cell_width_px.into(),
        cell_height: size.cell_height_px.into(),
    }
}
fn colors_wire(colors: &TerminalColors) -> wire::TermColors {
    let rgb = |r: u8, g: u8, b: u8| (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
    wire::TermColors {
        background: rgb(colors.background.r, colors.background.g, colors.background.b),
        foreground: rgb(colors.foreground.r, colors.foreground.g, colors.foreground.b),
        cursor: colors.cursor.map(|c| rgb(c.r, c.g, c.b)),
    }
}
fn exit_info(exit: wire::TerminalExited) -> Result<ExitInfo, Failure> {
    if exit.code.is_some_and(|n| !(0..=255).contains(&n))
        || exit.signal.is_some_and(|n| !(1..=64).contains(&n))
        || (exit.code.is_some() && exit.signal.is_some())
        || (exit.left && (exit.code.is_some() || exit.signal.is_some()))
    {
        return Err(gone("the terminal server sent an invalid program exit"));
    }
    Ok(ExitInfo {
        code: exit.code,
        signal: exit.signal,
        detached: exit.left,
    })
}
fn decode(frame: Down, assembler: &mut SnapshotAssembler) -> Result<Option<LinkItem>, Failure> {
    if assembler.in_progress() && !matches!(frame, Down::Snapshot(_)) {
        return Err(FrameRefused::SnapshotIncomplete.into());
    }
    Ok(Some(match frame {
        Down::Snapshot(part) => {
            let tail_kind = match wire::TailKind::try_from(part.tail_kind) {
                Ok(wire::TailKind::None) => WireTail::None,
                Ok(wire::TailKind::ReadAhead) => WireTail::ReadAhead,
                Ok(wire::TailKind::Raw) => WireTail::Raw,
                Err(_) => return Err(gone("the terminal server sent an unknown snapshot tail kind")),
            };
            return assembler
                .part(SnapshotPartWire {
                    offset: part.offset,
                    columns: part.columns,
                    rows: part.rows,
                    total_bytes: part.total_bytes,
                    replay: part.replay,
                    last: part.last,
                    last_input: part.last_input,
                    tail_kind,
                    tail: part.tail,
                    tail_calls: part.tail_calls,
                    history_at_cap: part.history_at_cap,
                    history_trimmed: part.history_trimmed,
                    output_while_detached: part.output_while_detached,
                    input_written: part.input_written,
                })
                .map(|snapshot| snapshot.map(LinkItem::Snapshot))
                .map_err(Into::into);
        }
        Down::Output(output) => {
            if output.data.len() > 64 * 1024 {
                return Err(FrameRefused::PartTooLarge.into());
            }
            output
                .offset
                .checked_add(output.data.len() as u64)
                .ok_or(FrameRefused::OffsetOverflow)?;
            LinkItem::Output {
                offset: output.offset,
                bytes: output.data,
            }
        }
        Down::Resized(marker) => {
            let size = marker.size.ok_or(FrameRefused::SizeOutOfRange)?;
            if !(1..=u32::from(eitri_terminal::headless::TERM_COLUMNS_MAX)).contains(&size.columns)
                || !(1..=u32::from(eitri_terminal::headless::TERM_ROWS_MAX)).contains(&size.rows)
                || size.cell_width > u16::MAX.into()
                || size.cell_height > u16::MAX.into()
            {
                return Err(FrameRefused::SizeOutOfRange.into());
            }
            LinkItem::Resized {
                offset: marker.offset,
                size: PtySize {
                    cols: size.columns as u16,
                    rows: size.rows as u16,
                    cell_width_px: size.cell_width as u16,
                    cell_height_px: size.cell_height as u16,
                },
            }
        }
        Down::InputWritten(written) => LinkItem::InputWritten(written.upto),
        Down::Exited(exit) => LinkItem::Exited(exit_info(exit)?),
        Down::Waiting(wait) => LinkItem::Waiting(match wire::WaitReason::try_from(wait.reason) {
            Ok(wire::WaitReason::OpenSequence) => WaitWhy::OpenSequence,
            Ok(wire::WaitReason::TooDeep) => WaitWhy::TooDeep,
            Ok(wire::WaitReason::SnapshotTooLarge) => WaitWhy::SnapshotTooLarge,
            _ => return Err(gone("the terminal server sent an unknown wait reason")),
        }),
        Down::Detached(detach) => LinkItem::Ended(match wire::DetachReason::try_from(detach.reason) {
            Ok(wire::DetachReason::Replaced) => MirrorEnd::Replaced,
            Ok(wire::DetachReason::Closed) => MirrorEnd::Closed,
            Ok(wire::DetachReason::Stopped) => MirrorEnd::Stopped,
            _ => return Err(gone("the terminal server sent an unknown detach reason")),
        }),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_reconnect_delays_stay_at_ten_seconds_after_a_long_cut() {
        let seconds: Vec<_> = (0..8).map(|attempt| reconnect_delay(attempt).as_secs()).collect();
        assert_eq!(seconds, [1, 2, 4, 8, 10, 10, 10, 10]);
        assert_eq!(reconnect_delay(usize::MAX), Duration::from_secs(10));
    }

    fn state() -> (State, PipeReader, mpsc::Receiver<wire::TerminalClientFrame>) {
        let (reader, bell) = io::pipe().unwrap();
        nonblocking(&reader).unwrap();
        nonblocking(&bell).unwrap();
        let (sender, receiver) = mpsc::channel(8);
        (
            State {
                items: VecDeque::new(),
                bytes: 0,
                pending_state: false,
                live: true,
                ready: true,
                sent: 0,
                written: 0,
                sender: Some(sender),
                size: PtySize {
                    cols: 80,
                    rows: 24,
                    cell_width_px: 8,
                    cell_height_px: 16,
                },
                colors: colors_wire(&TerminalColors::default()),
                terminal_id: None,
                opened: OpenAnswer::default(),
                bell,
                connection: Weak::new(),
                notes: None,
            },
            reader,
            receiver,
        )
    }

    #[test]
    fn an_answer_handles_wait_and_clones_share_the_published_identity() {
        let (state, _reader, _) = state();
        let answer = state.opened.clone();
        assert!(answer.get().is_none());
        assert!(answer.wait(Instant::now()).is_none());
        let waiting = answer.clone();
        let worker = std::thread::spawn(move || waiting.wait(Instant::now() + Duration::from_secs(10)));
        let identity = ("a".repeat(32), "daemon".into(), "boot".into());
        state.opened.set(identity.clone());
        assert_eq!(worker.join().unwrap(), Some(identity.clone()));
        // A reattach must not replace the answer whose generation is being saved.
        state
            .opened
            .set(("b".repeat(32), "later-daemon".into(), "later-boot".into()));
        drop(state);
        assert_eq!(answer.get(), Some(identity));
    }

    #[test]
    fn a_wait_for_an_answer_ends_when_the_link_does() {
        let answer = OpenAnswer::default();
        let waiting = answer.clone();
        let (told, woke) = std::sync::mpsc::channel();
        std::thread::spawn(move || told.send(waiting.wait(Instant::now() + Duration::from_secs(3600))));
        answer.finish();
        // Without the end the waiter would sleep its hour; this guard only keeps a broken build from hanging.
        assert_eq!(woke.recv_timeout(Duration::from_secs(600)).unwrap(), None);
        assert!(answer.ended());
        assert!(answer.wait(Instant::now() + Duration::from_secs(3600)).is_none());
    }

    #[test]
    fn input_window_and_offsets_follow_written_bytes() {
        let (mut state, _reader, mut wire) = state();
        for offset in (0..SEND_WINDOW).step_by(64 * 1024) {
            assert_eq!(state.write_input(&vec![b'x'; 70 * 1024]).unwrap(), 64 * 1024);
            let Some(Up::Input(input)) = wire.try_recv().unwrap().frame else {
                panic!()
            };
            assert_eq!(input.offset, offset);
            assert_eq!(input.data.len(), 64 * 1024);
        }
        assert_eq!(state.write_input(b"x").unwrap_err().kind(), io::ErrorKind::WouldBlock);
        state.written = 1;
        assert_eq!(state.write_input(b"xy").unwrap(), 1);
    }

    #[test]
    fn unread_state_changes_close_the_input_gate_even_after_reconnect() {
        let (mut state, _reader, _) = state();
        state.link(LinkState::Disconnected { since_ms: 0 }, 0);
        assert_eq!(state.write_input(b"x").unwrap_err().kind(), io::ErrorKind::NotConnected);
        state.link(LinkState::Live, 0);
        state.ready = true;
        assert_eq!(state.write_input(b"x").unwrap_err().kind(), io::ErrorKind::NotConnected);
    }

    #[test]
    fn lost_input_is_counted_in_bytes_and_next_input_starts_at_written() {
        let (mut state, _reader, mut wire) = state();
        state.sent = 128 * 1024;
        state.written = 64 * 1024;
        state.snapshot(64 * 1024, Some(128 * 1024)).unwrap();
        assert!(matches!(
            state.items.pop_front(),
            Some(LinkItem::Link {
                lost_input_bytes: 65_536,
                ..
            })
        ));
        assert_eq!(
            state.write_input(b"new").unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        state.pending_state = false;
        state.write_input(b"new").unwrap();
        let Some(Up::Input(input)) = wire.try_recv().unwrap().frame else {
            panic!()
        };
        assert_eq!(input.offset, 65_536);
        assert_eq!(input.data, b"new");
        state.snapshot(65_539, None).unwrap();
        assert!(state.items.is_empty(), "loss is reported once");
    }

    #[test]
    fn a_waiting_connections_new_input_is_not_mistaken_for_lost_input() {
        let (mut state, _reader, mut wire) = state();
        state.write_input(b"key").unwrap();
        state.snapshot(0, None).unwrap();
        assert_eq!(state.sent, 3);
        assert_eq!(state.written, 0);
        assert!(state.items.is_empty());
        assert!(wire.try_recv().is_ok());
        assert!(state.snapshot(4, None).is_err());
    }

    #[test]
    fn queued_items_make_the_doorbell_readable_and_count_toward_backpressure() {
        let (mut state, mut reader, _) = state();
        assert_eq!(reader.read(&mut [0]).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        state.push(LinkItem::Output {
            offset: 0,
            bytes: vec![0; 64 * 1024],
        });
        assert_eq!(reader.read(&mut [0]).unwrap(), 1);
        assert!(state.bytes > 64 * 1024);
        for _ in 0..256 {
            state.push(LinkItem::Waiting(WaitWhy::TooDeep));
        }
        assert!(reader.read(&mut [0]).is_ok());
    }

    #[test]
    fn malformed_frames_are_refused_before_narrowing_or_assembling() {
        let mut assembler = SnapshotAssembler::new();
        assert!(matches!(
            decode(
                Down::Snapshot(wire::SnapshotPart {
                    total_bytes: u64::MAX,
                    columns: 80,
                    rows: 24,
                    ..Default::default()
                }),
                &mut assembler
            )
            .unwrap_err(),
            Failure::End(MirrorEnd::Refused(FrameRefused::SnapshotTooLarge))
        ));
        assert!(decode(
            Down::Resized(wire::Resized {
                size: Some(wire::TermSize {
                    columns: 65_537,
                    rows: 24,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            &mut assembler
        )
        .is_err());
        assert!(decode(Down::Waiting(wire::Waiting { reason: 42 }), &mut assembler).is_err());
        assert!(decode(
            Down::Exited(wire::TerminalExited {
                signal: Some(-1),
                ..Default::default()
            }),
            &mut assembler
        )
        .is_err());
        assert!(decode(
            Down::Output(wire::OutputBytes {
                offset: u64::MAX,
                data: vec![1]
            }),
            &mut assembler
        )
        .is_err());
    }

    #[test]
    fn snapshot_parts_and_all_end_and_wait_reasons_map_to_mirror_items() {
        let mut assembler = SnapshotAssembler::new();
        let part = wire::SnapshotPart {
            columns: 80,
            rows: 24,
            total_bytes: 2,
            replay: vec![b'a'],
            ..Default::default()
        };
        assert!(decode(Down::Snapshot(part.clone()), &mut assembler).unwrap().is_none());
        assert!(matches!(
            decode(
                Down::Snapshot(wire::SnapshotPart { last: true, ..part }),
                &mut assembler
            )
            .unwrap(),
            Some(LinkItem::Snapshot(_))
        ));
        for (reason, expected) in [
            (wire::DetachReason::Closed, MirrorEnd::Closed),
            (wire::DetachReason::Stopped, MirrorEnd::Stopped),
            (wire::DetachReason::Replaced, MirrorEnd::Replaced),
        ] {
            assert!(
                matches!(decode(Down::Detached(wire::Detached { reason: reason as i32 }), &mut assembler).unwrap(), Some(LinkItem::Ended(end)) if end == expected)
            );
        }
        for reason in [
            wire::WaitReason::OpenSequence,
            wire::WaitReason::TooDeep,
            wire::WaitReason::SnapshotTooLarge,
        ] {
            assert!(matches!(
                decode(Down::Waiting(wire::Waiting { reason: reason as i32 }), &mut assembler).unwrap(),
                Some(LinkItem::Waiting(_))
            ));
        }
        assert!(matches!(
            decode(
                Down::Exited(wire::TerminalExited {
                    left: true,
                    ..Default::default()
                }),
                &mut assembler
            )
            .unwrap(),
            Some(LinkItem::Exited(ExitInfo { detached: true, .. }))
        ));
    }
    fn owner(id: &str, facts: TerminalFactsHandle) -> Arc<FactOwner> {
        Arc::new(FactOwner {
            facts,
            state: Mutex::new((Some(id.into()), false)),
        })
    }

    fn hello() -> wire::HelloResponse {
        wire::HelloResponse {
            protocol_major: wire::PROTOCOL_MAJOR,
            capabilities: vec![wire::CAP_TERMINAL.into()],
            daemon_instance: "daemon".into(),
            boot_id: "boot".into(),
            ..Default::default()
        }
    }

    fn worker() -> (Worker, PipeReader) {
        let (mut state, reader, _) = state();
        state.live = false;
        state.ready = false;
        state.sender = None;
        (
            Worker {
                state: Arc::new(Mutex::new(state)),
                changed: Arc::new(Notify::new()),
                owner: owner(&"a".repeat(32), TerminalFactsHandle::default()),
                host: "example-host".into(),
                client_id: "client".into(),
                options: TerminalLinkOptions {
                    installation: String::new(),
                    target: TerminalTarget::Attach {
                        terminal_id: "a".repeat(32),
                        daemon_instance: "daemon".into(),
                        boot_id: "boot".into(),
                    },
                    size: PtySize {
                        cols: 80,
                        rows: 24,
                        cell_width_px: 8,
                        cell_height_px: 16,
                    },
                    colors: TerminalColors::default(),
                },
                failures: HashSet::new(),
                started: Instant::now(),
                established: false,
            },
            reader,
        )
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    async fn until(mut predicate: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !predicate() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker hung");
    }

    fn frame(frame: Down) -> Result<wire::TerminalServerFrame, tonic::Status> {
        Ok(wire::TerminalServerFrame { frame: Some(frame) })
    }

    #[test]
    fn empty_input_is_a_noop_even_when_disconnected() {
        let (mut state, _reader, mut rx) = state();
        assert_eq!(state.write_input(&[]).unwrap(), 0);
        assert!(rx.try_recv().is_err());
        state.link(LinkState::Disconnected { since_ms: 0 }, 0);
        assert_eq!(state.write_input(&[]).unwrap(), 0);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn permanent_rpc_refusals_end_and_resource_exhaustion_retries() {
        for code in [
            tonic::Code::InvalidArgument,
            tonic::Code::PermissionDenied,
            tonic::Code::FailedPrecondition,
            tonic::Code::Unimplemented,
        ] {
            let failure = Failure::from(tonic::Status::new(code, "untrusted prose"));
            assert!(matches!(failure, Failure::End(MirrorEnd::Gone(why))
                if why.contains("refused") && !why.contains("untrusted")));
        }
        for code in [
            tonic::Code::Unavailable,
            tonic::Code::DeadlineExceeded,
            tonic::Code::ResourceExhausted,
        ] {
            let status = tonic::Status::new(code, "retry");
            assert!(retryable(&status));
            assert!(matches!(Failure::from(status), Failure::Retry(_)));
        }
    }

    #[test]
    fn incompatible_versions_and_bad_identities_have_the_links_own_reasons() {
        for major in [wire::PROTOCOL_MAJOR - 1, wire::PROTOCOL_MAJOR + 1] {
            let mut hello = hello();
            hello.protocol_major = major;
            assert!(
                matches!(check_hello(&hello, "example-host"), Err(Failure::End(MirrorEnd::Gone(why)))
                if why.contains("incompatible") && !why.contains("older"))
            );
        }
        let mut hello = hello();
        hello.boot_id = "bad identity".into();
        assert!(
            matches!(check_hello(&hello, "example-host"), Err(Failure::End(MirrorEnd::Gone(why)))
            if why.contains("Hello identity"))
        );
        let mut assembler = SnapshotAssembler::new();
        for (frame, problem) in [
            (Down::Waiting(wire::Waiting { reason: 42 }), "wait reason"),
            (Down::Detached(wire::Detached { reason: 42 }), "detach reason"),
            (
                Down::Snapshot(wire::SnapshotPart {
                    tail_kind: 42,
                    ..Default::default()
                }),
                "tail kind",
            ),
        ] {
            assert!(
                matches!(decode(frame, &mut assembler), Err(Failure::End(MirrorEnd::Gone(why)))
                if why.contains(problem))
            );
        }
        let (mut state, _, _) = state();
        assert!(
            matches!(state.snapshot(1, None), Err(Failure::End(MirrorEnd::Gone(why)))
            if why.contains("input offset"))
        );
    }

    #[test]
    fn gone_notes_are_exact_and_a_changed_instance_does_not_claim_a_kill() {
        runtime().block_on(async {
            let (worker, _reader) = worker();
            assert!(matches!(worker.attach_error(tonic::Status::not_found(wire::TERM_GONE_NOTE), &hello()),
                Failure::End(MirrorEnd::Gone(why)) if why.contains("killed, or out of memory")));
            assert!(!worker.state.lock().unwrap().live);
            assert!(worker.state.lock().unwrap().sender.is_none());
            assert!(matches!(worker.attach_error(tonic::Status::not_found(format!("{} extra", wire::TERM_GONE_NOTE)), &hello()),
                Failure::End(MirrorEnd::Gone(why)) if why.contains("was closed")));
            let mut next = hello();
            next.daemon_instance = "replacement".into();
            assert!(matches!(worker.attach_error(tonic::Status::not_found("missing"), &next),
                Failure::End(MirrorEnd::Gone(why)) if why.contains("no longer recorded") && !why.contains("killed")));
        });
    }

    #[test]
    fn an_exit_record_without_a_screen_is_a_closed_terminal() {
        runtime().block_on(async {
            for first in [
                None,
                Some(Down::Waiting(wire::Waiting {
                    reason: wire::WaitReason::OpenSequence as i32,
                })),
                Some(Down::Snapshot(wire::SnapshotPart {
                    columns: 80,
                    rows: 24,
                    last: true,
                    ..Default::default()
                })),
            ] {
                let has_screen = first.is_some();
                let (mut worker, _reader) = worker();
                let (tx, _rx) = mpsc::channel(8);
                let frames = first.into_iter().chain([Down::Exited(wire::TerminalExited {
                    signal: Some(1),
                    ..Default::default()
                })]);
                let initial = (worker.options.size, colors_wire(&worker.options.colors), 0, 0);
                let result = worker
                    .follow(tx, tokio_stream::iter(frames.map(frame)), &hello(), initial)
                    .await;
                assert!(matches!(result, Err(Failure::Finished)));
                let mut state = worker.state.lock().unwrap();
                let last = state.items.pop_back().unwrap();
                // With or without a screen before it, the exit is passed on as the exit it was.
                let _ = has_screen;
                assert!(matches!(last, LinkItem::Exited(ExitInfo { signal: Some(1), .. })));
                assert!(!state.live);
                assert!(state.sender.is_none());
            }
        });
    }

    #[test]
    fn a_saved_exit_ends_the_link_even_when_the_program_had_left_its_terminal() {
        runtime().block_on(async {
            let (mut saved, _saved_reader) = worker();
            let (tx, _rx) = mpsc::channel(8);
            let left = Down::Exited(wire::TerminalExited {
                left: true,
                ..Default::default()
            });
            let initial = (saved.options.size, colors_wire(&saved.options.colors), 0, 0);
            let result = saved
                .follow(
                    tx,
                    tokio_stream::iter([left.clone()].map(frame)),
                    &hello(),
                    initial.clone(),
                )
                .await;
            assert!(
                matches!(result, Err(Failure::Finished)),
                "no reconnect to the same record"
            );
            let last = saved.state.lock().unwrap().items.pop_back().unwrap();
            assert!(matches!(last, LinkItem::Exited(ExitInfo { detached: false, .. })));

            // On a live screen the program leaving is not the end: its real end may follow.
            let (mut live, _live_reader) = worker();
            let (tx, _rx) = mpsc::channel(8);
            let screen = Down::Snapshot(wire::SnapshotPart {
                columns: 80,
                rows: 24,
                last: true,
                ..Default::default()
            });
            let result = live
                .follow(tx, tokio_stream::iter([screen, left].map(frame)), &hello(), initial)
                .await;
            assert!(matches!(result, Err(Failure::Retry(_))));
            let last = live.state.lock().unwrap().items.pop_back().unwrap();
            assert!(matches!(last, LinkItem::Exited(ExitInfo { detached: true, .. })));
        });
    }

    #[test]
    fn only_an_attach_answered_not_found_says_the_terminal_is_gone() {
        runtime().block_on(async {
            let (worker, _reader) = worker();
            let answer = worker.state.lock().unwrap().opened.clone();
            for code in [
                tonic::Code::InvalidArgument,
                tonic::Code::PermissionDenied,
                tonic::Code::FailedPrecondition,
                tonic::Code::Unimplemented,
            ] {
                assert!(matches!(
                    worker.attach_error(tonic::Status::new(code, "refused"), &hello()),
                    Failure::End(MirrorEnd::Gone(_))
                ));
                assert!(!answer.terminal_gone(), "{code:?} leaves a terminal that may still run");
            }
            assert!(matches!(
                check_hello(&wire::HelloResponse::default(), "example-host"),
                Err(Failure::End(MirrorEnd::Gone(_)))
            ));
            assert!(!answer.terminal_gone());
            assert!(matches!(
                worker.attach_error(tonic::Status::not_found("missing"), &hello()),
                Failure::End(MirrorEnd::Gone(_))
            ));
            assert!(answer.terminal_gone());
        });
    }

    #[test]
    fn a_reattach_whose_first_answer_is_a_wait_settles_from_the_hosts_written_count() {
        runtime().block_on(async {
            let (mut worker, _reader) = worker();
            let state = worker.state.clone();
            {
                let mut state = state.lock().unwrap();
                state.sent = 10;
                state.written = 4;
            }
            let (tx, mut rx) = mpsc::channel(8);
            let frames = [
                Down::InputWritten(wire::InputWritten { upto: 7 }),
                Down::Waiting(wire::Waiting {
                    reason: wire::WaitReason::OpenSequence as i32,
                }),
            ];
            let stream = tokio_stream::iter(frames.map(frame)).chain(tokio_stream::pending());
            let initial = (worker.options.size, colors_wire(&worker.options.colors), 10, 4);
            let hello = hello();
            tokio::select! {
                _ = worker.follow(tx, stream, &hello, initial) => panic!("the stream has not ended"),
                () = until(|| state.lock().unwrap().ready) => {}
            }
            let mut state = state.lock().unwrap();
            assert_eq!((state.sent, state.written), (7, 7));
            assert!(state.items.iter().any(|item| matches!(
                item,
                LinkItem::Link {
                    lost_input_bytes: 3,
                    ..
                }
            )));
            state.pending_state = false;
            assert_eq!(state.write_input(b"x").unwrap(), 1);
            let Some(Up::Input(input)) = rx.try_recv().unwrap().frame else {
                panic!("an input frame")
            };
            assert_eq!(input.offset, 7);
        });
    }

    #[test]
    fn failure_logs_are_bounded_and_deduplicate_by_kind() {
        runtime().block_on(async {
            let (mut worker, _reader) = worker();
            for _ in 0..1000 {
                worker.note_failure(FailureKind::Link);
                worker.note_failure(FailureKind::Rpc(tonic::Code::Unavailable));
            }
            assert_eq!(worker.failures.len(), 2);
            assert!(!worker.note_failure(FailureKind::Link));
        });
    }

    #[test]
    fn malformed_inventory_entries_are_isolated_and_strings_are_capped() {
        let facts = TerminalFactsHandle::default();
        let a = owner(&"a".repeat(32), facts.clone());
        let b = owner(&"b".repeat(32), facts.clone());
        let unrelated = TerminalFacts {
            busy: Some(true),
            name: None,
            cwd: None,
            at: Instant::now(),
        };
        facts.set("other-connection", unrelated);
        cache_facts(
            &[a.clone(), b.clone()],
            wire::ListTerminalsResponse {
                terminals: vec![
                    wire::TerminalInfo {
                        terminal_id: "invalid".into(),
                        ..Default::default()
                    },
                    wire::TerminalInfo {
                        terminal_id: "a".repeat(32),
                        cwd: format!("/{}", "é".repeat(wire::TERM_PATH_MAX)),
                        foreground_name: "é".repeat(wire::TERM_NAME_MAX),
                        foreground_job: Some(true),
                        ..Default::default()
                    },
                    wire::TerminalInfo {
                        terminal_id: "b".repeat(32),
                        exit: Some(wire::TerminalExited {
                            code: Some(-1),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ]
                .into_iter()
                .chain(std::iter::repeat_n(wire::TerminalInfo::default(), wire::TERM_LIST_MAX))
                .collect(),
                ..Default::default()
            },
        );
        let saved = facts.get(&"a".repeat(32)).unwrap();
        assert_eq!(saved.busy, Some(true));
        assert_eq!(saved.name.unwrap().len(), wire::TERM_NAME_MAX);
        assert!(saved.cwd.unwrap().as_os_str().len() <= wire::TERM_PATH_MAX);
        assert!(facts.get(&"b".repeat(32)).is_none());
        assert!(facts.get("other-connection").is_some());
        a.clear(false);
        assert!(facts.get("other-connection").is_some());
        cache_facts(
            &[a],
            wire::ListTerminalsResponse {
                terminals: vec![wire::TerminalInfo {
                    terminal_id: "a".repeat(32),
                    foreground_job: Some(true),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        assert!(facts.get(&"a".repeat(32)).is_none());
    }

    #[test]
    fn response_headers_do_not_reset_backoff_but_snapshot_or_wait_does() {
        runtime().block_on(async {
            for first in [
                None,
                Some(Down::Waiting(wire::Waiting {
                    reason: wire::WaitReason::OpenSequence as i32,
                })),
                Some(Down::Snapshot(wire::SnapshotPart {
                    columns: 80,
                    rows: 24,
                    last: true,
                    ..Default::default()
                })),
            ] {
                let established = first.is_some();
                let (mut worker, _reader) = worker();
                let (tx, _rx) = mpsc::channel(8);
                let mut frames = Vec::new();
                if let Some(first) = first {
                    frames.push(frame(first));
                }
                frames.push(Err(tonic::Status::unavailable("broken")));
                let initial = (worker.options.size, colors_wire(&worker.options.colors), 0, 0);
                let result = worker.follow(tx, tokio_stream::iter(frames), &hello(), initial).await;
                assert!(matches!(result, Err(Failure::Retry(_))));
                assert_eq!(worker.established, established);
                assert!(!worker.state.lock().unwrap().live);
                assert!(worker.state.lock().unwrap().sender.is_none());
            }
        });
    }

    #[test]
    fn resize_and_colour_backpressure_does_not_hold_output_or_errors() {
        runtime().block_on(async {
            let (mut worker, _reader) = worker();
            let state = worker.state.clone();
            let changed = worker.changed.clone();
            let initial = (worker.options.size, colors_wire(&worker.options.colors), 0, 0);
            let (tx, _rx) = mpsc::channel(1);
            tx.try_send(wire::TerminalClientFrame::default()).unwrap();
            let (frames, incoming) = mpsc::channel(8);
            let task = tokio::spawn(async move {
                worker
                    .follow(
                        tx,
                        tokio_stream::wrappers::ReceiverStream::new(incoming),
                        &hello(),
                        initial,
                    )
                    .await
            });
            until(|| state.lock().unwrap().pending_state).await;
            {
                let mut state = state.lock().unwrap();
                state.size.cols += 1;
                state.colors.background ^= 1;
            }
            changed.notify_one();
            frames
                .send(frame(Down::Waiting(wire::Waiting {
                    reason: wire::WaitReason::OpenSequence as i32,
                })))
                .await
                .unwrap();
            frames
                .send(frame(Down::Output(wire::OutputBytes {
                    offset: 0,
                    data: b"still reading".to_vec(),
                })))
                .await
                .unwrap();
            until(|| {
                state
                    .lock()
                    .unwrap()
                    .items
                    .iter()
                    .any(|i| matches!(i, LinkItem::Output { .. }))
            })
            .await;
            frames
                .send(Err(tonic::Status::permission_denied("denied")))
                .await
                .unwrap();
            assert!(matches!(task.await.unwrap(), Err(Failure::End(MirrorEnd::Gone(_)))));
            assert!(!state.lock().unwrap().live);
            assert!(state.lock().unwrap().sender.is_none());
        });
    }

    #[test]
    fn a_hung_or_failed_refresh_never_holds_the_attachment() {
        runtime().block_on(async {
            let (mut worker, _reader) = worker();
            let state = worker.state.clone();
            let facts = worker.owner.facts.clone();
            let id = "a".repeat(32);
            facts.set(
                id.clone(),
                TerminalFacts {
                    busy: Some(false),
                    name: None,
                    cwd: None,
                    at: Instant::now(),
                },
            );
            let at = facts.get(&id).unwrap().at;
            let owners = Arc::new(Mutex::new(vec![Arc::downgrade(&worker.owner)]));
            let (called, mut calls) = mpsc::unbounded_channel();
            let release = Arc::new(Notify::new());
            let unblock = release.clone();
            let refresh = tokio::spawn(refresh_facts_with(owners, move || {
                called.send(()).unwrap();
                let unblock = unblock.clone();
                async move {
                    unblock.notified().await;
                    Err(tonic::Status::unavailable("list failed"))
                }
            }));
            calls.recv().await.unwrap();
            let initial = (worker.options.size, colors_wire(&worker.options.colors), 0, 0);
            let (tx, _rx) = mpsc::channel(8);
            let (frames, incoming) = mpsc::channel(8);
            let attachment = tokio::spawn(async move {
                worker
                    .follow(
                        tx,
                        tokio_stream::wrappers::ReceiverStream::new(incoming),
                        &hello(),
                        initial,
                    )
                    .await
            });
            frames
                .send(frame(Down::Snapshot(wire::SnapshotPart {
                    columns: 80,
                    rows: 24,
                    last: true,
                    ..Default::default()
                })))
                .await
                .unwrap();
            frames
                .send(frame(Down::Output(wire::OutputBytes {
                    offset: 0,
                    data: b"output".to_vec(),
                })))
                .await
                .unwrap();
            frames
                .send(frame(Down::InputWritten(wire::InputWritten { upto: 0 })))
                .await
                .unwrap();
            until(|| {
                state
                    .lock()
                    .unwrap()
                    .items
                    .iter()
                    .any(|i| matches!(i, LinkItem::InputWritten(0)))
            })
            .await;
            release.notify_one();
            // The failed list leaves the cached timestamp and the stream alone.
            tokio::task::yield_now().await;
            assert_eq!(facts.get(&id).unwrap().at, at);
            assert!(state.lock().unwrap().live);
            frames
                .send(frame(Down::Detached(wire::Detached {
                    reason: wire::DetachReason::Closed as i32,
                })))
                .await
                .unwrap();
            assert!(matches!(attachment.await.unwrap(), Err(Failure::Finished)));
            refresh.abort();
        });
    }
    #[test]
    fn one_refresh_covers_all_owned_ids_without_consuming_another_connections_request() {
        runtime().block_on(async {
            let facts = TerminalFactsHandle::default();
            let a = owner(&"a".repeat(32), facts.clone());
            let b = owner(&"b".repeat(32), facts.clone());
            let outside = "c".repeat(32);
            for id in [&"a".repeat(32), &"b".repeat(32), &outside] {
                facts.request_refresh(id.clone());
            }
            let owners = Arc::new(Mutex::new(vec![Arc::downgrade(&a), Arc::downgrade(&b)]));
            let (called, mut calls) = mpsc::unbounded_channel();
            let task = tokio::spawn(refresh_facts_with(owners, move || {
                called.send(()).unwrap();
                async {
                    Ok(wire::ListTerminalsResponse {
                        terminals: ["a", "b"]
                            .into_iter()
                            .map(|id| wire::TerminalInfo {
                                terminal_id: id.repeat(32),
                                foreground_job: Some(false),
                                ..Default::default()
                            })
                            .collect(),
                        ..Default::default()
                    })
                }
            }));
            calls.recv().await.unwrap();
            until(|| facts.get(&"b".repeat(32)).is_some()).await;
            assert_eq!(
                facts.get(&"a".repeat(32)).unwrap().at,
                facts.get(&"b".repeat(32)).unwrap().at
            );
            assert!(calls.try_recv().is_err());
            assert_eq!(
                facts.take_refresh_requests_for(&BTreeSet::from([outside.clone()])),
                [outside]
            );
            assert!(facts
                .take_refresh_requests_for(&BTreeSet::from(["a".repeat(32), "b".repeat(32)]))
                .is_empty());
            task.abort();
        });
    }

    #[test]
    fn invalid_input_written_offsets_have_the_links_own_reason() {
        runtime().block_on(async {
            for upto in [2, 6] {
                let (mut worker, _reader) = worker();
                {
                    let mut state = worker.state.lock().unwrap();
                    state.sent = 5;
                    state.written = 3;
                }
                let (tx, _rx) = mpsc::channel(8);
                let initial = (worker.options.size, colors_wire(&worker.options.colors), 5, 3);
                let result = worker
                    .follow(
                        tx,
                        tokio_stream::iter([frame(Down::InputWritten(wire::InputWritten { upto }))]),
                        &hello(),
                        initial,
                    )
                    .await;
                assert!(
                    matches!(result, Err(Failure::End(MirrorEnd::Gone(why))) if why.contains("invalid input offset"))
                );
            }
        });
    }
}
