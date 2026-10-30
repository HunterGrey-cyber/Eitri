//! The presentation thread: one per display, the only reader of a private event queue on the
//! display's own Wayland connection. It binds the globals, dispatches every event for the objects
//! the GTK thread creates on that queue, and hands what it received to the GTK thread through a
//! mailbox the GTK thread drains at GDK's paint priority. It never draws, never touches GL and sends
//! no request but the registry binds, the dmabuf feedback request, its own roundtrips and flushes.
//!
//! **Reading.** Every step follows libwayland's multi-reader protocol: `prepare_read` on the queue
//! (when another reader already queued events there, the step only dispatches them, so whoever
//! waits for one of them sees it before the thread blocks), one `poll` over the display fd and the
//! command eventfd, then `read` if the fd was readable or a cancel otherwise, at once and before any
//! other work. No read intention is held outside that `poll`, so GDK's own reader, which waits in
//! `read_events` for every prepared reader, waits at most for this thread's wake-up.
//!
//! **Lifetime.** The thread, its connection and its queue live as long as the display. The queue is
//! owned by the connection's state in `wayland-backend` and freed with its last strong reference;
//! proxies hold weak ones. Freeing it while a proxy is still attached (a frame callback or a
//! presentation feedback in flight, which have no client-side destroy request; a `wl_buffer` that is
//! not yet released; the registry, the globals and the feedback object) makes libwayland abort the
//! process when that proxy's next event arrives. So a pane going away only removes its routing; the
//! thread is stopped only when the display closes, and a stop it does not confirm within 100 ms, or a
//! failure of the thread itself, leaks the connection rather than dropping it.

use std::cell::{Cell, Ref, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::panic::{self, AssertUnwindSafe};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use gtk4::{gdk, glib, prelude::*};
use wayland_backend::client::{Backend, WaylandError, WeakBackend};
use wayland_client::{
    backend::ObjectData,
    delegate_noop,
    protocol::{
        wl_buffer::{self, WlBuffer},
        wl_callback::{self, WlCallback},
        wl_compositor::WlCompositor,
        wl_region::WlRegion,
        wl_registry::{self, WlRegistry},
        wl_subcompositor::WlSubcompositor,
        wl_subsurface::WlSubsurface,
        wl_surface::WlSurface,
    },
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols::wp::{
    linux_dmabuf::zv1::client::{
        zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
        zwp_linux_dmabuf_feedback_v1::{self, ZwpLinuxDmabufFeedbackV1},
        zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    },
    presentation_time::client::{
        wp_presentation::WpPresentation,
        wp_presentation_feedback::{self, WpPresentationFeedback},
    },
    viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};

use super::foreign::connection_for;
use super::types::{BufferKey, DmabufFeedback, FrameData, Globals, PaneId, PresentEvent, PresentSink};
use crate::fd_watch;

/// GDK paints at this priority (`GDK_PRIORITY_REDRAW`), so a frame that a callback releases is drawn
/// ahead of default-priority I/O, and a busy default-priority source cannot hold the mailbox back.
const MAILBOX_PRIORITY: std::ffi::c_int = 120;

/// How long the GTK thread waits for the thread to confirm a stop; past it the connection is leaked.
const STOP_WAIT: Duration = Duration::from_millis(100);

/// `wl_surface.damage_buffer` needs version 4; nothing newer is used.
const COMPOSITOR_VERSION: u32 = 4;

/// Version 4 is the first whose feedback names the compositor's main device; nothing newer is used.
const DMABUF_VERSION: u32 = 4;

/// The mailbox and the wake-ups both sides share.
struct Shared {
    mailbox: Mutex<VecDeque<PresentEvent>>,
    /// Readable while the mailbox may hold events; the GTK thread watches it.
    mailbox_wake: OwnedFd,
    /// Readable while a command may be waiting; the thread polls it beside the display fd.
    command_wake: OwnedFd,
    /// Set before `Stop` is sent; the thread checks it after every wake-up.
    stop: AtomicBool,
    /// Set by the thread as the last thing it does.
    exited: AtomicBool,
}

impl Shared {
    fn new() -> Option<Self> {
        Some(Self {
            mailbox: Mutex::new(VecDeque::new()),
            mailbox_wake: eventfd()?,
            command_wake: eventfd()?,
            stop: AtomicBool::new(false),
            exited: AtomicBool::new(false),
        })
    }

    fn mailbox(&self) -> MutexGuard<'_, VecDeque<PresentEvent>> {
        // Nothing panics while the lock is held (one extend or one take), so a poisoned lock still
        // guards a whole queue.
        self.mailbox.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Appends `events` under one lock and wakes the GTK thread only when the mailbox was empty:
    /// a non-empty mailbox already has a wake-up pending, which the drain clears before it takes.
    fn post(&self, events: impl IntoIterator<Item = PresentEvent>) {
        let wake = {
            let mut mailbox = self.mailbox();
            let was_empty = mailbox.is_empty();
            mailbox.extend(events);
            was_empty && !mailbox.is_empty()
        };
        if wake {
            signal(&self.mailbox_wake);
        }
    }

    /// Clears the wake-up, then takes everything. In this order an event posted between the two
    /// steps is either taken now or finds the mailbox empty and signals again: none is left behind
    /// without a wake-up.
    fn take(&self) -> VecDeque<PresentEvent> {
        clear(&self.mailbox_wake);
        std::mem::take(&mut *self.mailbox())
    }
}

fn eventfd() -> Option<OwnedFd> {
    // SAFETY: plain syscall; a non-negative result is a new descriptor owned by nobody else.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    // SAFETY: `fd` was just created above and is owned here alone.
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
}

fn signal(fd: &OwnedFd) {
    let one: u64 = 1;
    // SAFETY: writes 8 bytes from a live u64 to an eventfd owned by the caller. The only failure on
    // a non-blocking eventfd is a counter at its maximum, which already reads as a pending wake-up.
    unsafe { libc::write(fd.as_raw_fd(), (&one as *const u64).cast::<c_void>(), 8) };
}

fn clear(fd: &OwnedFd) {
    let mut count: u64 = 0;
    // SAFETY: reads 8 bytes into a live u64 from an eventfd owned by the caller. An empty counter
    // fails with EAGAIN, which means there was nothing to clear.
    unsafe { libc::read(fd.as_raw_fd(), (&mut count as *mut u64).cast::<c_void>(), 8) };
}

/// What the GTK thread asks of the thread.
enum Command {
    /// The GTK thread's own flush met a full socket: flush once the fd is writable.
    Flush,
    Stop,
    Test(TestCommand),
}

/// Commands only the crate's integration tests send, to put the thread in the states its rules are
/// about. None of them sends a request but the thread's own roundtrip.
#[doc(hidden)]
pub enum TestCommand {
    /// Keeps the thread busy between two steps, holding no read intention; reports the instant the
    /// hold starts and the instant it ends.
    Hold {
        span: Duration,
        report: mpsc::Sender<Instant>,
    },
    /// Panics inside the loop.
    Panic,
    /// Posts `count` frame-done events for `pane`, numbered from 0, in one batch, then reports.
    Burst {
        pane: PaneId,
        count: u64,
        done: mpsc::Sender<()>,
    },
    /// Sends a roundtrip's `wl_display.sync` and flushes it, stays busy for `hold` (long enough
    /// for another reader to read the reply into the queue), then waits for the `done` as the
    /// startup does, and reports.
    Roundtrip { hold: Duration, done: mpsc::Sender<()> },
}

/// The user data of the thread's own `wl_display.sync` callbacks.
pub struct Roundtrip(u64);

/// The user data of a `wl_buffer` the server created from a params object. Its data is made before
/// the `created` event is dispatched, without the parent's key, so the key is set at that dispatch;
/// a release needs an attach, which needs the `BufferCreated` posted after it, so it always finds
/// the key set.
#[derive(Default)]
pub struct CreatedBuffer(OnceLock<BufferKey>);

struct Advertised {
    name: u32,
    interface: String,
    version: u32,
}

/// The globals to bind: (registry name, version to bind).
#[derive(Debug, PartialEq, Eq)]
struct Chosen {
    compositor: (u32, u32),
    subcompositor: (u32, u32),
    dmabuf: (u32, u32),
    viewporter: Option<(u32, u32)>,
    presentation: Option<(u32, u32)>,
}

/// Picks the globals from what the registry advertised, or says why the subsurface path cannot run.
fn choose_globals(advertised: &[Advertised]) -> Result<Chosen, String> {
    let find = |interface: &str| {
        advertised
            .iter()
            .find(|global| global.interface == interface)
            .map(|global| (global.name, global.version))
    };
    let (compositor, compositor_version) =
        find("wl_compositor").ok_or_else(|| "the compositor offers no wl_compositor".to_string())?;
    if compositor_version < COMPOSITOR_VERSION {
        return Err(format!(
            "wl_compositor is version {compositor_version}; version {COMPOSITOR_VERSION} is needed for damage in \
             buffer coordinates"
        ));
    }
    let (subcompositor, _) =
        find("wl_subcompositor").ok_or_else(|| "the compositor offers no wl_subcompositor".to_string())?;
    let (dmabuf, dmabuf_version) =
        find("zwp_linux_dmabuf_v1").ok_or_else(|| "the compositor offers no zwp_linux_dmabuf_v1".to_string())?;
    if dmabuf_version < DMABUF_VERSION {
        return Err(format!(
            "zwp_linux_dmabuf_v1 is version {dmabuf_version}; version {DMABUF_VERSION} is needed for feedback that \
             names the main device"
        ));
    }
    Ok(Chosen {
        compositor: (compositor, COMPOSITOR_VERSION),
        subcompositor: (subcompositor, 1),
        dmabuf: (dmabuf, DMABUF_VERSION),
        viewporter: find("wp_viewporter").map(|(name, _)| (name, 1)),
        presentation: find("wp_presentation").map(|(name, _)| (name, 1)),
    })
}

/// Collects one round of dmabuf feedback events up to its `done`.
#[derive(Default)]
struct FeedbackBuilder {
    /// The last format table received; a round that sends none keeps it.
    table: Vec<(u32, u64)>,
    main_device: Option<u64>,
    tranche_device: Option<u64>,
    tranche_formats: Vec<u16>,
    tranches: Vec<(Option<u64>, Vec<u16>)>,
}

impl FeedbackBuilder {
    fn format_table(&mut self, bytes: &[u8]) {
        self.table = parse_format_table(bytes);
    }

    fn main_device(&mut self, device: &[u8]) {
        self.main_device = parse_dev(device);
    }

    fn tranche_target_device(&mut self, device: &[u8]) {
        self.tranche_device = parse_dev(device);
    }

    fn tranche_formats(&mut self, indices: &[u8]) {
        self.tranche_formats.extend(
            indices
                .chunks_exact(2)
                .map(|pair| u16::from_ne_bytes([pair[0], pair[1]])),
        );
    }

    fn tranche_done(&mut self) {
        let device = self.tranche_device.take();
        let formats = std::mem::take(&mut self.tranche_formats);
        self.tranches.push((device, formats));
    }

    /// Ends the round: the formats of the tranches aimed at the main device, in the feedback's order
    /// of preference, each pair once; `None` when the round named no main device. The next round
    /// starts empty but for the format table. A tranche's target is matched to the main device by
    /// `same_device`, not by `dev_t`: the protocol leaves each one's node type (primary or render)
    /// open, so one GPU may be named by two different `dev_t`s.
    fn done(&mut self, same_device: &dyn Fn(u64, u64) -> bool) -> Option<DmabufFeedback> {
        let main_device = self.main_device.take();
        let tranches = std::mem::take(&mut self.tranches);
        self.tranche_device = None;
        self.tranche_formats.clear();
        let main_device = main_device?;
        let mut seen = HashSet::new();
        let formats = tranches
            .iter()
            .filter(|(device, _)| device.is_some_and(|device| same_device(device, main_device)))
            .flat_map(|(_, indices)| indices.iter())
            .filter_map(|&index| self.table.get(usize::from(index)).copied())
            .filter(|pair| seen.insert(*pair))
            .collect();
        Some(DmabufFeedback { main_device, formats })
    }
}

/// The format table's entries: a 32-bit format, 32 bits of padding, a 64-bit modifier.
fn parse_format_table(bytes: &[u8]) -> Vec<(u32, u64)> {
    bytes
        .chunks_exact(16)
        .map(|entry| {
            let format = u32::from_ne_bytes([entry[0], entry[1], entry[2], entry[3]]);
            let mut modifier = [0u8; 8];
            modifier.copy_from_slice(&entry[8..16]);
            (format, u64::from_ne_bytes(modifier))
        })
        .collect()
}

/// Whether two `dev_t`s name one device: both DRM nodes of a GPU (`card0`, `renderD128`) resolve to
/// the same `/sys/dev/char/<major>:<minor>/device`. A side that does not resolve compares by value.
fn same_device(a: u64, b: u64) -> bool {
    crate::dmabuf_target::same_gpu(a, b, &crate::dmabuf_target::resolve_device)
}

/// A `dev_t` as the protocol sends it: in native byte order, as wide as the platform's `dev_t`.
fn parse_dev(bytes: &[u8]) -> Option<u64> {
    match bytes.len() {
        8 => {
            let mut dev = [0u8; 8];
            dev.copy_from_slice(bytes);
            Some(u64::from_ne_bytes(dev))
        }
        4 => Some(u64::from(u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))),
        _ => None,
    }
}

/// Reads at most `size` bytes of the format table, and never past the end of the file behind `fd`
/// (touching a mapping past it would fault).
fn read_format_table(fd: &OwnedFd, size: u32) -> Vec<u8> {
    // SAFETY: an all-zero `stat` is a valid out-parameter for `fstat`.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is open for the call and `stat` is a live out-parameter.
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } != 0 {
        return Vec::new();
    }
    let len = (size as usize).min(usize::try_from(stat.st_size).unwrap_or(0));
    if len == 0 {
        return Vec::new();
    }
    // SAFETY: a private read-only mapping of `len` bytes, all inside the file as `fstat` saw it.
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            fd.as_raw_fd(),
            0,
        )
    };
    if map == libc::MAP_FAILED {
        return Vec::new();
    }
    // SAFETY: the mapping above is `len` readable bytes until the `munmap` below.
    let bytes = unsafe { std::slice::from_raw_parts(map.cast::<u8>(), len) }.to_vec();
    // SAFETY: unmaps exactly the mapping made above, after its last use.
    unsafe { libc::munmap(map, len) };
    bytes
}

/// The dispatch state of the private queue: every object on it delivers its events here, and
/// what the GTK thread needs to know leaves through `outbox` into the mailbox.
#[derive(Default)]
pub struct ThreadState {
    outbox: Vec<PresentEvent>,
    advertised: Vec<Advertised>,
    roundtrips_done: u64,
    feedback: FeedbackBuilder,
    /// The feedback the latest `done` completed.
    latest_feedback: Option<DmabufFeedback>,
    /// Set once `Ready` has gone out; later feedback goes out as `FeedbackChanged`.
    ready: bool,
}

impl Dispatch<WlRegistry, ()> for ThreadState {
    fn event(
        state: &mut Self,
        _: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                state.advertised.push(Advertised {
                    name,
                    interface,
                    version,
                });
            }
            wl_registry::Event::GlobalRemove { name } => state.advertised.retain(|global| global.name != name),
            _ => {}
        }
    }
}

impl Dispatch<WlCallback, Roundtrip> for ThreadState {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        token: &Roundtrip,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.roundtrips_done = state.roundtrips_done.max(token.0);
        }
    }
}

impl Dispatch<WlCallback, FrameData> for ThreadState {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        frame: &FrameData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            // Taken where the event is received: the GTK thread may run much later.
            let at = Instant::now();
            state.outbox.push(PresentEvent::FrameDone {
                pane: frame.pane,
                seq: frame.seq,
                at,
            });
        }
    }
}

impl Dispatch<WpPresentationFeedback, FrameData> for ThreadState {
    fn event(
        state: &mut Self,
        _: &WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        frame: &FrameData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let (pane, seq) = (frame.pane, frame.seq);
        match event {
            wp_presentation_feedback::Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                refresh,
                ..
            } => {
                state.outbox.push(PresentEvent::Presented {
                    pane,
                    seq,
                    presented_ns: ((u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo))
                        .saturating_mul(1_000_000_000)
                        .saturating_add(u64::from(tv_nsec)),
                    refresh_ns: refresh,
                });
            }
            wp_presentation_feedback::Event::Discarded => state.outbox.push(PresentEvent::Discarded { pane, seq }),
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, BufferKey> for ThreadState {
    fn event(
        state: &mut Self,
        _: &ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        key: &BufferKey,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_linux_buffer_params_v1::Event::Created { buffer } => {
                if let Some(created) = buffer.data::<CreatedBuffer>() {
                    let _ = created.0.set(*key);
                }
                state.outbox.push(PresentEvent::BufferCreated { key: *key, buffer });
            }
            zwp_linux_buffer_params_v1::Event::Failed => state.outbox.push(PresentEvent::BufferFailed { key: *key }),
            _ => {}
        }
    }

    fn event_created_child(_opcode: u16, qh: &QueueHandle<Self>) -> Arc<dyn ObjectData> {
        // `created` is the interface's only event that creates an object.
        qh.make_data::<WlBuffer, CreatedBuffer>(CreatedBuffer::default())
    }
}

impl Dispatch<WlBuffer, CreatedBuffer> for ThreadState {
    fn event(
        state: &mut Self,
        _: &WlBuffer,
        event: wl_buffer::Event,
        created: &CreatedBuffer,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let (wl_buffer::Event::Release, Some(key)) = (event, created.0.get()) {
            state.outbox.push(PresentEvent::BufferReleased { key: *key });
        }
    }
}

impl Dispatch<WlBuffer, BufferKey> for ThreadState {
    fn event(
        state: &mut Self,
        _: &WlBuffer,
        event: wl_buffer::Event,
        key: &BufferKey,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            state.outbox.push(PresentEvent::BufferReleased { key: *key });
        }
    }
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for ThreadState {
    fn event(
        state: &mut Self,
        _: &ZwpLinuxDmabufFeedbackV1,
        event: zwp_linux_dmabuf_feedback_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_linux_dmabuf_feedback_v1::Event;
        match event {
            Event::FormatTable { fd, size } => state.feedback.format_table(&read_format_table(&fd, size)),
            Event::MainDevice { device } => state.feedback.main_device(&device),
            Event::TrancheTargetDevice { device } => state.feedback.tranche_target_device(&device),
            Event::TrancheFormats { indices } => state.feedback.tranche_formats(&indices),
            Event::TrancheDone => state.feedback.tranche_done(),
            Event::Done => {
                if let Some(feedback) = state.feedback.done(&same_device) {
                    if state.ready && state.latest_feedback.as_ref() != Some(&feedback) {
                        state.outbox.push(PresentEvent::FeedbackChanged(feedback.clone()));
                    }
                    state.latest_feedback = Some(feedback);
                }
            }
            _ => {}
        }
    }
}

// Objects whose events, if any, the presentation path does not use. `ignore` rather than the
// macro's default, which would panic on an event.
delegate_noop!(ThreadState: ignore WlCompositor);
delegate_noop!(ThreadState: ignore WlSubcompositor);
delegate_noop!(ThreadState: ignore ZwpLinuxDmabufV1);
delegate_noop!(ThreadState: ignore WpViewporter);
delegate_noop!(ThreadState: ignore WpPresentation);
delegate_noop!(ThreadState: ignore WlSurface);
delegate_noop!(ThreadState: ignore WlSubsurface);
delegate_noop!(ThreadState: ignore WpViewport);
delegate_noop!(ThreadState: ignore WlRegion);

/// Why the loop ended.
enum End {
    Stop,
    Fatal(String),
}

fn would_block(error: &WaylandError) -> bool {
    matches!(error, WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock)
}

/// The thread's side: everything it owns.
struct Loop {
    conn: Connection,
    queue: EventQueue<ThreadState>,
    state: ThreadState,
    shared: Arc<Shared>,
    commands: mpsc::Receiver<Command>,
    flush_owed: bool,
    next_roundtrip: u64,
    registry: Option<WlRegistry>,
    feedback: Option<ZwpLinuxDmabufFeedbackV1>,
}

impl Loop {
    fn run(&mut self) -> End {
        if let Err(end) = self.start() {
            return end;
        }
        loop {
            if let Err(end) = self.step() {
                return end;
            }
        }
    }

    /// Binds the globals and waits for the first dmabuf feedback, then posts `Ready` or
    /// `Unavailable`. Either way the thread goes on serving its queue: the registry is on it.
    fn start(&mut self) -> Result<(), End> {
        let qh = self.queue.handle();
        let registry = self.conn.display().get_registry(&qh, ());
        self.registry = Some(registry.clone());
        self.roundtrip()?;
        let chosen = match choose_globals(&self.state.advertised) {
            Ok(chosen) => chosen,
            Err(reason) => {
                self.state.outbox.push(PresentEvent::Unavailable(reason));
                self.post();
                return Ok(());
            }
        };
        let compositor = registry.bind::<WlCompositor, _, _>(chosen.compositor.0, chosen.compositor.1, &qh, ());
        let subcompositor =
            registry.bind::<WlSubcompositor, _, _>(chosen.subcompositor.0, chosen.subcompositor.1, &qh, ());
        let dmabuf = registry.bind::<ZwpLinuxDmabufV1, _, _>(chosen.dmabuf.0, chosen.dmabuf.1, &qh, ());
        let viewporter = chosen
            .viewporter
            .map(|(name, version)| registry.bind::<WpViewporter, _, _>(name, version, &qh, ()));
        let presentation = chosen
            .presentation
            .map(|(name, version)| registry.bind::<WpPresentation, _, _>(name, version, &qh, ()));
        self.feedback = Some(dmabuf.get_default_feedback(&qh, ()));
        self.roundtrip()?;
        let event = match self.state.latest_feedback.clone() {
            None => PresentEvent::Unavailable("the compositor sent no dmabuf feedback".to_string()),
            Some(feedback) if feedback.formats.is_empty() => {
                PresentEvent::Unavailable("the dmabuf feedback lists no format for its main device".to_string())
            }
            Some(feedback) => {
                self.state.ready = true;
                PresentEvent::Ready(Box::new(Globals {
                    compositor,
                    subcompositor,
                    dmabuf,
                    viewporter,
                    presentation,
                    feedback,
                }))
            }
        };
        self.state.outbox.push(event);
        self.post();
        Ok(())
    }

    /// Sends a `wl_display.sync` on the private queue and serves the queue until its `done`.
    fn roundtrip(&mut self) -> Result<(), End> {
        let token = self.send_roundtrip();
        self.wait_roundtrip(token)
    }

    fn send_roundtrip(&mut self) -> u64 {
        self.next_roundtrip += 1;
        let token = self.next_roundtrip;
        self.conn.display().sync(&self.queue.handle(), Roundtrip(token));
        self.flush_owed = true;
        token
    }

    fn wait_roundtrip(&mut self, token: u64) -> Result<(), End> {
        while self.state.roundtrips_done < token {
            self.step()?;
        }
        Ok(())
    }

    fn stopping(&self) -> bool {
        self.shared.stop.load(Ordering::Acquire)
    }

    /// One wake-up: prepare, poll, read or cancel, stop check, dispatch and post, commands, flush;
    /// or, when the queue already holds events, only their dispatch.
    fn step(&mut self) -> Result<(), End> {
        if self.stopping() {
            return Err(End::Stop);
        }
        if self.flush_owed {
            self.flush()?;
        }
        let Some(guard) = self.queue.prepare_read() else {
            // Another reader (GDK) already read events into this queue. Dispatch them and return
            // before polling: what they change (a roundtrip's `done`) may be what the caller waits
            // for, and nothing more may arrive to wake the poll.
            return self.dispatch();
        };
        let mut fds = [
            libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN | if self.flush_owed { libc::POLLOUT } else { 0 },
                revents: 0,
            },
            libc::pollfd {
                fd: self.shared.command_wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: two initialised pollfds; both descriptors stay open across the call (the display
        // fd outlives this thread's use of the connection, the eventfd is owned by `shared`).
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        let (display, commands) = if ready > 0 {
            (fds[0].revents, fds[1].revents)
        } else {
            (0, 0)
        };
        // Read or cancel at once: every other reader of the connection waits for this.
        if display & libc::POLLIN != 0 {
            if let Err(error) = guard.read() {
                if !would_block(&error) {
                    return Err(End::Fatal(format!("reading the Wayland connection failed: {error}")));
                }
            }
        } else {
            drop(guard);
        }
        if self.stopping() {
            return Err(End::Stop);
        }
        if display & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 && display & libc::POLLIN == 0 {
            return Err(End::Fatal("the Wayland connection hung up".to_string()));
        }
        self.dispatch()?;
        if commands & libc::POLLIN != 0 {
            clear(&self.shared.command_wake);
            self.commands()?;
        }
        if self.flush_owed && display & libc::POLLOUT != 0 {
            self.flush()?;
        }
        Ok(())
    }

    /// Dispatches what libwayland queued for the private queue, then posts what that produced.
    fn dispatch(&mut self) -> Result<(), End> {
        self.conn
            .backend()
            .dispatch_inner_queue()
            .map_err(|error| End::Fatal(format!("dispatching the Wayland queue failed: {error}")))?;
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|error| End::Fatal(format!("dispatching the Wayland queue failed: {error}")))?;
        self.post();
        Ok(())
    }

    fn post(&mut self) {
        if !self.state.outbox.is_empty() {
            self.shared.post(self.state.outbox.drain(..));
        }
    }

    fn flush(&mut self) -> Result<(), End> {
        match self.conn.flush() {
            Ok(()) => {
                self.flush_owed = false;
                Ok(())
            }
            Err(error) if would_block(&error) => {
                self.flush_owed = true;
                Ok(())
            }
            Err(error) => Err(End::Fatal(format!("flushing the Wayland connection failed: {error}"))),
        }
    }

    fn commands(&mut self) -> Result<(), End> {
        while let Ok(command) = self.commands.try_recv() {
            match command {
                Command::Flush => self.flush_owed = true,
                Command::Stop => return Err(End::Stop),
                Command::Test(test) => self.test(test)?,
            }
        }
        Ok(())
    }

    fn test(&mut self, command: TestCommand) -> Result<(), End> {
        match command {
            TestCommand::Hold { span, report } => {
                let _ = report.send(Instant::now());
                std::thread::sleep(span);
                let _ = report.send(Instant::now());
            }
            TestCommand::Panic => panic!("a test asked the presentation thread to panic"),
            TestCommand::Burst { pane, count, done } => {
                let at = Instant::now();
                self.shared
                    .post((0..count).map(|seq| PresentEvent::FrameDone { pane, seq, at }));
                let _ = done.send(());
            }
            TestCommand::Roundtrip { hold, done } => {
                let token = self.send_roundtrip();
                self.flush()?;
                std::thread::sleep(hold);
                self.wait_roundtrip(token)?;
                let _ = done.send(());
            }
        }
        Ok(())
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        format!("the presentation thread panicked: {message}")
    } else if let Some(message) = payload.downcast_ref::<String>() {
        format!("the presentation thread panicked: {message}")
    } else {
        "the presentation thread panicked".to_string()
    }
}

fn thread_main(mut owned: Loop, exit: mpsc::Sender<()>) {
    let shared = Arc::clone(&owned.shared);
    let failure = match panic::catch_unwind(AssertUnwindSafe(|| owned.run())) {
        Ok(End::Stop) => None,
        Ok(End::Fatal(message)) => Some(message),
        Err(payload) => Some(panic_message(payload.as_ref())),
    };
    match failure {
        None => drop(owned),
        Some(message) => {
            // The loop has left its `poll`, so no read intention is held. The connection, the
            // queue and the proxies are leaked, not dropped: dropping them would free the queue
            // under proxies that may still be attached. Their last events pile up there, unread.
            std::mem::forget(owned);
            shared.post([PresentEvent::ThreadFailed(message)]);
        }
    }
    let _ = exit.send(());
    shared.exited.store(true, Ordering::Release);
}

#[derive(Clone)]
enum Status {
    Starting,
    Ready,
    Unavailable(String),
    Failed(String),
}

/// The presentation thread as the GTK thread sees it: one per display, kept until the display
/// closes. Its methods run on the GTK thread only.
pub struct PresenterThread {
    shared: Arc<Shared>,
    commands: mpsc::Sender<Command>,
    exit: RefCell<Option<mpsc::Receiver<()>>>,
    conn: RefCell<Option<Connection>>,
    qh: QueueHandle<ThreadState>,
    sinks: RefCell<HashMap<PaneId, Weak<dyn PresentSink>>>,
    globals: RefCell<Option<Globals>>,
    status: RefCell<Status>,
    watch: RefCell<Option<glib::SourceId>>,
    next_pane: Cell<u64>,
    shut_down: Cell<bool>,
    wakes: Cell<u64>,
    dropped: Cell<u64>,
}

impl PresenterThread {
    /// The display's presentation thread, started on first use and kept until the display closes;
    /// `None` off Wayland or when it could not start (said once, on stderr, and not retried).
    pub(crate) fn for_display(display: &gdk::Display) -> Option<Rc<Self>> {
        let key = glib::Quark::from_str("eitri-subsurface-presenter-thread");
        // SAFETY: this private key holds only an `Option<Rc<PresenterThread>>`, set and read on the
        // GTK thread; the borrowed display keeps it alive while it is cloned.
        if let Some(slot) = unsafe { display.qdata::<Option<Rc<Self>>>(key) } {
            // SAFETY: as above.
            return unsafe { slot.as_ref() }.clone();
        }
        let conn = connection_for(display)?;
        let thread = Self::start(conn);
        // SAFETY: the key holds only this type; see above.
        unsafe { display.set_qdata(key, thread.clone()) };
        if thread.is_some() {
            display.connect_closure(
                "closed",
                true,
                glib::closure_local!(move |display: &gdk::Display, _error: bool| {
                    // SAFETY: the key holds only this type, read on the GTK thread.
                    if let Some(Some(thread)) = unsafe { display.steal_qdata::<Option<Rc<Self>>>(key) } {
                        thread.shutdown();
                    }
                }),
            );
        }
        thread
    }

    fn start(conn: Connection) -> Option<Rc<Self>> {
        let Some(shared) = Shared::new().map(Arc::new) else {
            eprintln!("[editor] subsurface: the presentation thread could not start: no eventfd");
            return None;
        };
        let queue = conn.new_event_queue::<ThreadState>();
        let qh = queue.handle();
        let (command_tx, command_rx) = mpsc::channel();
        let (exit_tx, exit_rx) = mpsc::channel();
        let owned = Loop {
            conn: conn.clone(),
            queue,
            state: ThreadState::default(),
            shared: Arc::clone(&shared),
            commands: command_rx,
            flush_owed: false,
            next_roundtrip: 0,
            registry: None,
            feedback: None,
        };
        // Nothing is attached to the queue until the thread runs, so a failed spawn drops it safely.
        let spawned = std::thread::Builder::new()
            .name("eitri-present".to_string())
            .spawn(move || thread_main(owned, exit_tx));
        if let Err(error) = spawned {
            eprintln!("[editor] subsurface: the presentation thread could not start: {error}");
            return None;
        }
        let this = Rc::new(Self {
            shared: Arc::clone(&shared),
            commands: command_tx,
            exit: RefCell::new(Some(exit_rx)),
            conn: RefCell::new(Some(conn)),
            qh,
            sinks: RefCell::new(HashMap::new()),
            globals: RefCell::new(None),
            status: RefCell::new(Status::Starting),
            watch: RefCell::new(None),
            next_pane: Cell::new(1),
            shut_down: Cell::new(false),
            wakes: Cell::new(0),
            dropped: Cell::new(0),
        });
        let weak = Rc::downgrade(&this);
        let watch = fd_watch::add_local(
            shared.mailbox_wake.as_raw_fd(),
            glib::IOCondition::IN,
            MAILBOX_PRIORITY,
            move |_| match weak.upgrade() {
                Some(thread) => {
                    thread.drain();
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            },
        );
        match watch {
            Ok(watch) => *this.watch.borrow_mut() = Some(watch),
            Err(_) => {
                eprintln!("[editor] subsurface: the presentation thread's mailbox needs the main context's own thread");
                this.shutdown();
                return None;
            }
        }
        Some(this)
    }

    /// Routes `pane`'s events to `sink` from now on. If the thread already knows its outcome the sink
    /// hears it at once: `globals_ready`, or the `Unavailable` or `ThreadFailed` it would have got.
    pub fn subscribe(&self, pane: PaneId, sink: Weak<dyn PresentSink>) {
        if self.shut_down.get() {
            return;
        }
        let strong = sink.upgrade();
        self.sinks.borrow_mut().insert(pane, sink);
        let Some(sink) = strong else { return };
        let status = self.status.borrow().clone();
        match status {
            Status::Starting => {}
            Status::Ready => sink.globals_ready(),
            Status::Unavailable(reason) => sink.event(PresentEvent::Unavailable(reason)),
            Status::Failed(message) => sink.event(PresentEvent::ThreadFailed(message)),
        }
    }

    /// Stops routing `pane`'s events; later ones are dropped. The thread keeps running: objects the
    /// pane created may still be attached to its queue.
    pub fn unsubscribe(&self, pane: PaneId) {
        self.sinks.borrow_mut().remove(&pane);
    }

    /// The globals from `Ready`, with every later feedback change applied; `None` before `Ready`,
    /// when unavailable, and after the shutdown.
    pub fn globals(&self) -> Option<Ref<'_, Globals>> {
        Ref::filter_map(self.globals.borrow(), Option::as_ref).ok()
    }

    /// The private queue's handle: objects created with it deliver their events on the thread.
    pub fn queue_handle(&self) -> QueueHandle<ThreadState> {
        self.qh.clone()
    }

    /// Asks the thread to flush the connection once its fd is writable, after a flush on the GTK
    /// thread met a full socket.
    pub fn request_flush(&self) {
        if self.shut_down.get() || matches!(*self.status.borrow(), Status::Failed(_)) {
            return;
        }
        self.command(Command::Flush);
    }

    pub fn new_pane_id(&self) -> PaneId {
        let id = self.next_pane.get();
        self.next_pane.set(id + 1);
        PaneId(id)
    }

    fn command(&self, command: Command) {
        let _ = self.commands.send(command);
        signal(&self.shared.command_wake);
    }

    /// Stops the thread when the display closes; called once, later calls do nothing. Waits at most
    /// 100 ms for the thread to confirm. Confirmed, the thread holds nothing and this side drops its
    /// reference to the connection, which frees the queue (the display is still connected while its
    /// `closed` handlers run). Not confirmed, the connection and the globals are leaked instead: the
    /// thread may still be using them.
    pub fn shutdown(&self) {
        if self.shut_down.replace(true) {
            return;
        }
        if let Some(watch) = self.watch.borrow_mut().take() {
            watch.remove();
        }
        self.sinks.borrow_mut().clear();
        self.shared.stop.store(true, Ordering::Release);
        self.command(Command::Stop);
        let confirmed = self
            .exit
            .borrow_mut()
            .take()
            .is_some_and(|exit| exit.recv_timeout(STOP_WAIT).is_ok());
        let conn = self.conn.borrow_mut().take();
        let globals = self.globals.borrow_mut().take();
        if confirmed {
            // After a failure the thread leaked its own reference, so this one is not the last.
            drop(globals);
            drop(conn);
        } else {
            eprintln!("[editor] subsurface thread did not stop within 100 ms");
            std::mem::forget(globals);
            std::mem::forget(conn);
        }
    }

    /// Runs on the GTK thread for each wake-up of the mailbox: takes everything and routes it.
    fn drain(&self) {
        self.wakes.set(self.wakes.get() + 1);
        for event in self.shared.take() {
            self.route(event);
        }
    }

    fn route(&self, event: PresentEvent) {
        match event {
            PresentEvent::FrameDone { pane, .. }
            | PresentEvent::Presented { pane, .. }
            | PresentEvent::Discarded { pane, .. } => self.deliver(pane, event),
            PresentEvent::BufferCreated { key, .. }
            | PresentEvent::BufferFailed { key }
            | PresentEvent::BufferReleased { key } => self.deliver(key.pane, event),
            PresentEvent::Ready(globals) => {
                *self.globals.borrow_mut() = Some(*globals);
                *self.status.borrow_mut() = Status::Ready;
                for sink in self.live_sinks() {
                    sink.globals_ready();
                }
            }
            PresentEvent::Unavailable(reason) => {
                *self.status.borrow_mut() = Status::Unavailable(reason.clone());
                for sink in self.live_sinks() {
                    sink.event(PresentEvent::Unavailable(reason.clone()));
                }
            }
            PresentEvent::FeedbackChanged(feedback) => {
                if let Some(globals) = self.globals.borrow_mut().as_mut() {
                    globals.feedback = feedback.clone();
                }
                for sink in self.live_sinks() {
                    sink.event(PresentEvent::FeedbackChanged(feedback.clone()));
                }
            }
            PresentEvent::ThreadFailed(message) => {
                eprintln!("[editor] subsurface thread failed: {message}");
                *self.status.borrow_mut() = Status::Failed(message.clone());
                for sink in self.live_sinks() {
                    sink.event(PresentEvent::ThreadFailed(message.clone()));
                }
            }
        }
    }

    fn deliver(&self, pane: PaneId, event: PresentEvent) {
        // The borrow ends before the sink runs, so a sink may subscribe or unsubscribe.
        let sink = self.sinks.borrow().get(&pane).and_then(Weak::upgrade);
        match sink {
            Some(sink) => sink.event(event),
            None => {
                self.dropped.set(self.dropped.get() + 1);
                // A buffer created for a pane that is gone is attached to nothing and known to no
                // pool; destroying it here is the only way it is ever freed.
                if let PresentEvent::BufferCreated { buffer, .. } = event {
                    buffer.destroy();
                }
            }
        }
    }

    /// The subscribed sinks still alive; dead entries are removed. Collected first, so no borrow is
    /// held while a sink runs.
    fn live_sinks(&self) -> Vec<Rc<dyn PresentSink>> {
        let mut sinks = self.sinks.borrow_mut();
        sinks.retain(|_, sink| sink.strong_count() > 0);
        sinks.values().filter_map(Weak::upgrade).collect()
    }

    /// The same thread over a `wl_display` that is not GDK's, for tests with no `gdk::Display`.
    ///
    /// # Safety
    /// `display` is a live `wl_display` that stays connected for the rest of the process.
    #[doc(hidden)]
    pub unsafe fn start_on_foreign_display(display: *mut c_void) -> Option<Rc<Self>> {
        if display.is_null() {
            return None;
        }
        // SAFETY: the caller keeps the display connected for the rest of the process; the backend
        // only borrows it and never disconnects it.
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        Self::start(Connection::from_backend(backend))
    }

    /// Sends a command only the integration tests use.
    #[doc(hidden)]
    pub fn send_test_command(&self, command: TestCommand) {
        self.command(Command::Test(command));
    }

    /// Sends a `wl_display.sync` on the private queue as a pane's frame callback, the way a pane
    /// sends its requests: from the GTK thread with the queue's handle. Returns the callback's
    /// protocol id; `None` after the shutdown.
    #[doc(hidden)]
    pub fn test_sync(&self, pane: PaneId, seq: u64) -> Option<u32> {
        let conn = self.conn.borrow();
        let conn = conn.as_ref()?;
        let callback = conn.display().sync(&self.qh, FrameData { pane, seq });
        if let Err(error) = conn.flush() {
            if would_block(&error) {
                self.request_flush();
            }
        }
        Some(callback.id().protocol_id())
    }

    /// Watches whether anything still holds the connection, without holding it.
    #[doc(hidden)]
    pub fn connection_probe(&self) -> Option<ConnectionProbe> {
        self.conn
            .borrow()
            .as_ref()
            .map(|conn| ConnectionProbe(conn.backend().downgrade()))
    }

    /// How many times the mailbox woke the GTK thread.
    #[doc(hidden)]
    pub fn mailbox_wakes(&self) -> u64 {
        self.wakes.get()
    }

    /// How many pane events were dropped for want of a subscribed pane.
    #[doc(hidden)]
    pub fn dropped_events(&self) -> u64 {
        self.dropped.get()
    }

    /// Whether the thread has left its loop for good.
    #[doc(hidden)]
    pub fn thread_exited(&self) -> bool {
        self.shared.exited.load(Ordering::Acquire)
    }
}

impl Drop for PresenterThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Whether a connection is still alive: some strong reference, a leaked one included, still holds it.
#[doc(hidden)]
pub struct ConnectionProbe(WeakBackend);

impl ConnectionProbe {
    pub fn is_alive(&self) -> bool {
        self.0.upgrade().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advertised(list: &[(&str, u32)]) -> Vec<Advertised> {
        list.iter()
            .enumerate()
            .map(|(index, (interface, version))| Advertised {
                name: index as u32 + 1,
                interface: interface.to_string(),
                version: *version,
            })
            .collect()
    }

    #[test]
    fn every_required_global_is_bound_at_the_version_the_path_uses() {
        let chosen = choose_globals(&advertised(&[
            ("wl_compositor", 6),
            ("wl_subcompositor", 1),
            ("zwp_linux_dmabuf_v1", 5),
            ("wp_viewporter", 1),
            ("wp_presentation", 2),
        ]))
        .unwrap();
        assert_eq!(
            chosen,
            Chosen {
                compositor: (1, 4),
                subcompositor: (2, 1),
                dmabuf: (3, 4),
                viewporter: Some((4, 1)),
                presentation: Some((5, 1)),
            }
        );
    }

    #[test]
    fn the_viewporter_and_presentation_time_are_optional() {
        let chosen = choose_globals(&advertised(&[
            ("wl_compositor", 4),
            ("wl_subcompositor", 1),
            ("zwp_linux_dmabuf_v1", 4),
        ]))
        .unwrap();
        assert_eq!((chosen.viewporter, chosen.presentation), (None, None));
    }

    #[test]
    fn a_missing_or_too_old_required_global_says_which() {
        let cases: [(&[(&str, u32)], &str); 5] = [
            (
                &[("wl_subcompositor", 1), ("zwp_linux_dmabuf_v1", 4)],
                "no wl_compositor",
            ),
            (
                &[
                    ("wl_compositor", 3),
                    ("wl_subcompositor", 1),
                    ("zwp_linux_dmabuf_v1", 4),
                ],
                "wl_compositor is version 3",
            ),
            (
                &[("wl_compositor", 4), ("zwp_linux_dmabuf_v1", 4)],
                "no wl_subcompositor",
            ),
            (
                &[("wl_compositor", 4), ("wl_subcompositor", 1)],
                "no zwp_linux_dmabuf_v1",
            ),
            (
                &[
                    ("wl_compositor", 4),
                    ("wl_subcompositor", 1),
                    ("zwp_linux_dmabuf_v1", 3),
                ],
                "zwp_linux_dmabuf_v1 is version 3",
            ),
        ];
        for (list, expected) in cases {
            let reason = choose_globals(&advertised(list)).unwrap_err();
            assert!(reason.contains(expected), "{reason:?} does not say {expected:?}");
        }
    }

    fn table(entries: &[(u32, u64)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (format, modifier) in entries {
            bytes.extend_from_slice(&format.to_ne_bytes());
            bytes.extend_from_slice(&[0; 4]);
            bytes.extend_from_slice(&modifier.to_ne_bytes());
        }
        bytes
    }

    fn indices(list: &[u16]) -> Vec<u8> {
        list.iter().flat_map(|index| index.to_ne_bytes()).collect()
    }

    const XR24: u32 = 0x3432_5258;
    const AR24: u32 = 0x3432_5241;

    #[test]
    fn the_feedback_keeps_the_main_devices_tranches_in_order_and_each_pair_once() {
        let main = 0xe280_u64;
        let other = 0xe281_u64;
        let mut builder = FeedbackBuilder::default();
        builder.format_table(&table(&[(XR24, 0), (AR24, 0), (XR24, 7), (AR24, 9)]));
        builder.main_device(&main.to_ne_bytes());
        builder.tranche_target_device(&main.to_ne_bytes());
        builder.tranche_formats(&indices(&[2, 0]));
        builder.tranche_done();
        builder.tranche_target_device(&other.to_ne_bytes());
        builder.tranche_formats(&indices(&[3]));
        builder.tranche_done();
        builder.tranche_target_device(&main.to_ne_bytes());
        builder.tranche_formats(&indices(&[0, 1, 99]));
        builder.tranche_done();
        let feedback = builder.done(&same_dev_t).unwrap();
        assert_eq!(feedback.main_device, main);
        assert_eq!(feedback.formats, vec![(XR24, 7), (XR24, 0), (AR24, 0)]);
    }

    /// Matches devices by value only, as for two `dev_t`s that do not resolve to a device.
    fn same_dev_t(a: u64, b: u64) -> bool {
        a == b
    }

    #[test]
    fn a_tranche_aimed_at_another_node_of_the_main_gpu_is_kept() {
        // The main device named by its primary node, the tranches by the render node of the same
        // GPU and by a node of another GPU.
        let (card0, render128, other_gpu) = (0xe200_u64, 0xe280_u64, 0xe281_u64);
        let one_gpu = |a: u64, b: u64| a == b || (a != other_gpu && b != other_gpu);
        let mut builder = FeedbackBuilder::default();
        builder.format_table(&table(&[(XR24, 0), (AR24, 0)]));
        builder.main_device(&card0.to_ne_bytes());
        builder.tranche_target_device(&render128.to_ne_bytes());
        builder.tranche_formats(&indices(&[0]));
        builder.tranche_done();
        builder.tranche_target_device(&other_gpu.to_ne_bytes());
        builder.tranche_formats(&indices(&[1]));
        builder.tranche_done();
        let feedback = builder.done(&one_gpu).unwrap();
        assert_eq!(feedback.main_device, card0);
        assert_eq!(feedback.formats, vec![(XR24, 0)]);
    }

    #[test]
    fn a_dev_t_is_always_its_own_device() {
        assert!(same_device(0xe280, 0xe280));
        // Two dev_ts that name no character device are not taken for one.
        assert!(!same_device(libc::makedev(4095, 1), libc::makedev(4095, 2)));
    }

    #[test]
    fn a_later_round_keeps_the_format_table_and_starts_its_tranches_afresh() {
        let main = 0xe280_u64;
        let mut builder = FeedbackBuilder::default();
        builder.format_table(&table(&[(XR24, 0), (AR24, 0)]));
        builder.main_device(&main.to_ne_bytes());
        builder.tranche_target_device(&main.to_ne_bytes());
        builder.tranche_formats(&indices(&[0]));
        builder.tranche_done();
        builder.done(&same_dev_t).unwrap();
        builder.main_device(&main.to_ne_bytes());
        builder.tranche_target_device(&main.to_ne_bytes());
        builder.tranche_formats(&indices(&[1]));
        builder.tranche_done();
        assert_eq!(builder.done(&same_dev_t).unwrap().formats, vec![(AR24, 0)]);
    }

    #[test]
    fn a_round_without_a_main_device_completes_no_feedback() {
        let mut builder = FeedbackBuilder::default();
        builder.format_table(&table(&[(XR24, 0)]));
        builder.tranche_formats(&indices(&[0]));
        builder.tranche_done();
        assert_eq!(builder.done(&same_dev_t), None);
    }

    #[test]
    fn a_dev_t_is_read_at_either_width_and_nothing_else() {
        assert_eq!(parse_dev(&0x1234_5678_9abc_u64.to_ne_bytes()), Some(0x1234_5678_9abc));
        assert_eq!(parse_dev(&0xe280_u32.to_ne_bytes()), Some(0xe280));
        assert_eq!(parse_dev(&[1, 2, 3]), None);
    }

    #[test]
    fn a_partial_format_table_entry_is_ignored() {
        let mut bytes = table(&[(XR24, 5)]);
        bytes.extend_from_slice(&[1, 2, 3]);
        assert_eq!(parse_format_table(&bytes), vec![(XR24, 5)]);
    }

    /// Reads the eventfd's counter, which also clears it.
    fn read_counter(fd: &OwnedFd) -> u64 {
        let mut count: u64 = 0;
        // SAFETY: reads 8 bytes into a live u64 from an eventfd the test owns.
        let read = unsafe { libc::read(fd.as_raw_fd(), (&mut count as *mut u64).cast::<c_void>(), 8) };
        if read == 8 {
            count
        } else {
            0
        }
    }

    #[test]
    fn the_mailbox_signals_only_when_it_stops_being_empty() {
        let shared = Shared::new().unwrap();
        let pane = PaneId(1);
        let at = Instant::now();
        shared.post([PresentEvent::FrameDone { pane, seq: 0, at }]);
        shared.post([PresentEvent::FrameDone { pane, seq: 1, at }]);
        shared.post((2..5).map(|seq| PresentEvent::FrameDone { pane, seq, at }));
        shared.post(std::iter::empty());
        assert_eq!(
            read_counter(&shared.mailbox_wake),
            1,
            "one signal for five events in four posts"
        );
        signal(&shared.mailbox_wake);
        assert_eq!(shared.take().len(), 5);
        assert_eq!(read_counter(&shared.mailbox_wake), 0, "taking clears the wake-up");
        shared.post([PresentEvent::FrameDone { pane, seq: 5, at }]);
        assert_eq!(
            read_counter(&shared.mailbox_wake),
            1,
            "the first post after a take signals again"
        );
    }
}
