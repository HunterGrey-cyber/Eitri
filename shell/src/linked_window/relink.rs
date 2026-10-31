//! An orphaned window of a group and the window 1 that comes back for it.
//!
//! When the link ends without window 2 asking -- window 1 closed cleanly and said `bye`, or it died -- window 2 stays
//! up with no editor and keeps holding the pair lock, so no other window can take the pair over. It then listens on
//! `w-<16 hex>.sock` in the control directory the companion panel uses, with the same posture: the directory is made
//! 0700 and must be a private directory of this user, the path goes through [`agent::socket_path::in_dir`], only a
//! peer with this process's effective uid is served, and a socket in the way is reclaimed under an exclusive `flock`
//! only when a connect to it is refused (no listener; any other answer proves nothing and removes nothing), and only
//! the very socket that was probed. Nothing polls: the listener is watched on the main context, and each connection
//! is accepted without blocking.
//!
//! A window 1 starting for the same project finds the pair lock held. Before anything of it is drawn it connects to
//! that socket once, on a worker thread bounded by [`RELINK_WITHIN`], and says `hello` as window 1 of its build. The
//! orphan checks the build (another build cannot share the pair's state, and is told with `bye` how to get the pair
//! back), the window it names and that the process saying it is the one that connected, then answers `welcome` with
//! a copy of the pair lock's descriptor. The connection itself is the link from then on: a seqpacket socket, as the
//! socketpair a first start makes, close-on-exec at both ends. The `welcome` names what the orphan holds now, among it
//! the pages it rebuilt when window 1 ended, so the new window 1 holds those away instead of building them, and the
//! turns running in the orphan, so none of them is unseen by the new window's turn review. Neither side has
//! confirmed its presence in the handshake: each counts the other as another window until the other says so.
//!
//! A module window orphaned by its window 1 dying waits the same way, on a socket of its own,
//! `w-<16 hex>-<file form>.sock`, since each orphan serves one connection at a time and a group has several. A window 1
//! that finds no window 2 waiting lists those sockets ([`relink_all`]) and connects to each on a worker of its own, all
//! inside one [`RELINK_WITHIN`]: an orphan that misses the bound is left waiting and the others link. A `bye` to a
//! module window means its group closed, and it closes rather than waits. A late orphan has no timeout of its own: it
//! waits out the window that left it, whose claim keeps other starts off, and the first start after that links it.
//!
//! Each orphan answers whoever comes first, so two starts could each win a part of a group, and the lock copies they
//! adopt do not exclude each other. The [`GroupClaim`] orders them: a start takes it before it connects to any module
//! window and keeps it for as long as it runs, so a start that finds it held links nothing and runs unpaired.
//!
//! The orphan's editor key offers to start the editor window: `eitri <root>` from this executable, in a session of its
//! own with its standard streams on `/dev/null`, its exit reaped by a thread of this window and never waited for.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eitri_core::layout::{ModuleId, ModuleKind};
use eitri_core::window_link::fdpass;
use eitri_core::window_link::link::{Inbound, LinkHandle, PresenceFlag};
use eitri_core::window_link::names::WindowName;
use eitri_core::window_link::pair::{pair_lock_name, GroupClaim, PairLock};
use eitri_core::window_link::start::SECOND_WINDOW;
use eitri_core::window_link::{Msg, Summary, TurnEdge};

use super::driver::{LinkDriver, Partner};
use super::pair_files::WINDOW_2;
use super::spawn::FIRST_WINDOW;

/// How long a window 1 starting for the project waits for the orphan's `welcome`, its connect included. No window
/// of its own exists yet to stall, and an orphan that is up answers at once.
pub const RELINK_WITHIN: Duration = Duration::from_secs(2);

/// The most holders of a `welcome` taken into account: a window holds far fewer modules than this, so a longer list
/// is not one a window of this build sent.
pub const MAX_WELCOME_HOLDERS: usize = 256;

/// How much of a text from the other window goes into a note: enough to recognise it, never a flood.
const NOTE_CHARS: usize = 64;

/// How much of the orphan's `bye` is kept: a refusal of this build's says why and what to do in fewer words.
const BYE_CHARS: usize = 300;

/// What an orphan holds now, read as it welcomes a window 1 that came back.
pub type HeldNow = std::rc::Rc<dyn Fn() -> BTreeSet<String>>;

/// The directory the orphan's socket lives in: the companion panel's control directory.
pub fn orphan_dir() -> PathBuf {
    eitri_core::panel_control::control_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), &std::env::temp_dir())
}

/// Window 2's socket, `dir/w-<16 hex>.sock`, checked against the socket path limit.
pub fn socket_path(dir: &Path, project_root: &Path) -> io::Result<PathBuf> {
    socket_path_for(dir, project_root, &WindowName::Second)
}

/// The socket the window `name` waits on, checked against the socket path limit: window 2's `w-<16 hex>.sock`, a module
/// window's `w-<16 hex>-<file form>.sock`. A page's file form is a hash of fixed length, so the length does not depend
/// on the page's id.
pub fn socket_path_for(dir: &Path, project_root: &Path, name: &WindowName) -> io::Result<PathBuf> {
    agent::socket_path::in_dir(dir, &format!("{}.sock", file_stem(project_root, name)))
}

/// `dir/<the same name>.lock`, held while a socket in the way is judged and reclaimed.
fn lock_path_for(dir: &Path, project_root: &Path, name: &WindowName) -> PathBuf {
    dir.join(format!("{}.lock", file_stem(project_root, name)))
}

fn file_stem(project_root: &Path, name: &WindowName) -> String {
    let key = eitri_core::state_paths::project_key(project_root);
    match name {
        WindowName::Second => format!("w-{key}"),
        module => format!("w-{key}-{}", module.file_form()),
    }
}

/// What the user calls the window `name` in a refusal.
fn called(name: &WindowName) -> String {
    match name {
        WindowName::Second => "window 2".to_string(),
        module => format!("the window {}", module.text()),
    }
}

fn euid() -> u32 {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

/// `(device, inode)` of `path` itself, never of what a link there points at.
fn identity(path: &Path) -> Option<(u64, u64)> {
    path.symlink_metadata().ok().map(|meta| (meta.dev(), meta.ino()))
}

/// At most [`NOTE_CHARS`] characters of `text`, quoted with every control character escaped.
pub fn shown(text: &str) -> String {
    shown_within(text, NOTE_CHARS)
}

fn shown_within(text: &str, chars: usize) -> String {
    let cut: String = text.chars().take(chars).collect();
    if cut.len() < text.len() {
        format!("{cut:?}...")
    } else {
        format!("{cut:?}")
    }
}

/// The `sockaddr_un` for `path`, or why it cannot name one.
fn address(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: a zeroed `sockaddr_un` is a valid empty address.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.len() >= addr.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} cannot name a socket", path.display()),
        ));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let len = (std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
    Ok((addr, len))
}

/// A new seqpacket socket, close-on-exec and non-blocking.
fn seqpacket() -> io::Result<OwnedFd> {
    // SAFETY: plain socket creation; the descriptor is owned below.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just created, owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A listening seqpacket socket bound at `path`.
fn listen_at(path: &Path) -> io::Result<OwnedFd> {
    let (addr, len) = address(path)?;
    let fd = seqpacket()?;
    // SAFETY: `addr` is a valid `sockaddr_un` of `len` bytes for the call.
    if unsafe { libc::bind(fd.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on a bound socket this process owns.
    if unsafe { libc::listen(fd.as_raw_fd(), 4) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// A seqpacket connection to `path`, made without waiting: a listener whose queue is full answers at once too.
fn connect_to(path: &Path) -> io::Result<OwnedFd> {
    let (addr, len) = address(path)?;
    let fd = seqpacket()?;
    // SAFETY: `addr` is a valid `sockaddr_un` of `len` bytes for the call.
    if unsafe { libc::connect(fd.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// What a connect to a socket in the way says about it.
#[derive(Debug, PartialEq, Eq)]
enum InTheWay {
    /// The connect was refused: nothing listens, it is a leftover.
    Stale,
    /// Something answered, or the connect failed for a reason that says nothing about a listener.
    Live(String),
}

fn probe(path: &Path) -> InTheWay {
    match connect_to(path) {
        Ok(_) => InTheWay::Live("something listens there".into()),
        Err(err) if matches!(err.raw_os_error(), Some(libc::ECONNREFUSED) | Some(libc::ENOENT)) => InTheWay::Stale,
        Err(err) => InTheWay::Live(format!("a connect to it failed ({err})")),
    }
}

/// An exclusive `flock` on the reclaim lock file, released when it is dropped. Taken without waiting: one window of the
/// group is the only one that binds a given name, so a lock held by another means something else is at work here, and
/// nothing is reclaimed.
struct ReclaimLock(#[allow(dead_code)] std::fs::File);

impl ReclaimLock {
    fn take(path: &Path) -> Result<ReclaimLock, String> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|e| format!("could not open {}: {e}", path.display()))?;
        let meta = file
            .metadata()
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
        if !meta.is_file() || meta.uid() != euid() {
            return Err(format!(
                "{} is not this user's regular file; not using it",
                path.display()
            ));
        }
        // SAFETY: `file` owns a valid descriptor for the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = io::Error::last_os_error();
            return Err(format!(
                "{} is held by another claim ({err}); not reclaiming",
                path.display()
            ));
        }
        Ok(ReclaimLock(file))
    }
}

/// The orphan's listening socket. Removed when it is dropped, if the file there is still the one bound here.
#[derive(Debug)]
pub struct OrphanSocket {
    listener: OwnedFd,
    path: PathBuf,
    bound: (u64, u64),
}

impl OrphanSocket {
    /// Binds window 2's socket.
    #[cfg(test)]
    pub fn bind(dir: &Path, project_root: &Path) -> Result<OrphanSocket, String> {
        OrphanSocket::bind_as(dir, project_root, &WindowName::Second)
    }

    /// Binds the socket of the window `name` ([`socket_path_for`]) for `project_root` in `dir`, which is made if missing
    /// and must be a private directory of this user. A socket in the way is taken over only when nothing listens on it.
    pub fn bind_as(dir: &Path, project_root: &Path, name: &WindowName) -> Result<OrphanSocket, String> {
        eitri_core::panel_control::ensure_dir(dir)?;
        let path = socket_path_for(dir, project_root, name).map_err(|e| e.to_string())?;
        let busy = |e: &io::Error| e.kind() == io::ErrorKind::AddrInUse;
        match listen_at(&path) {
            Ok(fd) => return OrphanSocket::bound(fd, path),
            Err(e) if busy(&e) => {}
            Err(e) => return Err(format!("could not bind {}: {e}", path.display())),
        }
        // Held until the new socket is bound, so a reclaim and its bind are never split by another claim.
        let _lock = ReclaimLock::take(&lock_path_for(dir, project_root, name))?;
        match listen_at(&path) {
            Ok(fd) => return OrphanSocket::bound(fd, path),
            Err(e) if busy(&e) => {}
            Err(e) => return Err(format!("could not bind {}: {e}", path.display())),
        }
        let probed = match path.symlink_metadata() {
            Ok(meta) if meta.file_type().is_socket() && meta.uid() == euid() => (meta.dev(), meta.ino()),
            _ => return Err(format!("{} is not this user's socket; not touching it", path.display())),
        };
        if let InTheWay::Live(why) = probe(&path) {
            return Err(format!("{} is in use ({why}); not reclaiming it", path.display()));
        }
        if identity(&path) != Some(probed) {
            return Err(format!(
                "{} changed while it was judged; not reclaiming it",
                path.display()
            ));
        }
        std::fs::remove_file(&path).map_err(|e| format!("could not remove the stale {}: {e}", path.display()))?;
        let fd = listen_at(&path).map_err(|e| format!("could not bind {}: {e}", path.display()))?;
        OrphanSocket::bound(fd, path)
    }

    fn bound(listener: OwnedFd, path: PathBuf) -> Result<OrphanSocket, String> {
        let remove = |why: String| {
            let _ = std::fs::remove_file(&path);
            Err(why)
        };
        // `bind` leaves the umask's mode on the file.
        if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
            return remove(format!("could not set the mode of {}: {e}", path.display()));
        }
        let Some(bound) = identity(&path) else {
            return remove(format!("could not read {}", path.display()));
        };
        Ok(OrphanSocket { listener, path, bound })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn raw_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    /// The next connection waiting, close-on-exec, from a process of this user; `None` when none waits. A peer of
    /// another uid is closed unanswered.
    pub fn accept(&self) -> io::Result<Option<OwnedFd>> {
        loop {
            // SAFETY: accepting on a listening socket this process owns; the address is not asked for.
            let fd = unsafe {
                libc::accept4(
                    self.listener.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                )
            };
            if fd < 0 {
                let err = io::Error::last_os_error();
                return match err.kind() {
                    io::ErrorKind::WouldBlock => Ok(None),
                    io::ErrorKind::Interrupted => continue,
                    _ => Err(err),
                };
            }
            // SAFETY: a new descriptor the kernel just handed this process.
            let conn = unsafe { OwnedFd::from_raw_fd(fd) };
            match fdpass::peer_cred(conn.as_fd()) {
                Ok((_, uid)) if uid == euid() => return Ok(Some(conn)),
                Ok((pid, uid)) => {
                    eprintln!("[window] process {pid} of uid {uid} connected to the re-link socket; closed")
                }
                Err(err) => eprintln!("[window] a connection to the re-link socket names no peer ({err}); closed"),
            }
        }
    }
}

impl Drop for OrphanSocket {
    fn drop(&mut self) {
        if identity(&self.path) == Some(self.bound) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The process on the other end of `conn`, by its pid and start time, when both can be read.
pub fn peer_of(conn: &OwnedFd) -> Option<Partner> {
    let (pid, _) = fdpass::peer_cred(conn.as_fd()).ok()?;
    let start = agent::child::start_time(pid).ok()?;
    (pid != 0).then_some(Partner { pid, start })
}

/// What the orphan `who` tells a window 1 of another build with `bye`, with the way to get the pair back.
pub fn another_build_text(who: &str, theirs: &str, ours: &str) -> String {
    format!(
        "{who} runs another build of Eitri ({ours}, the new window is {}): close {who}, then run eitri again",
        shown(theirs)
    )
}

/// [`check_relink_hello_to`] for window 2.
#[cfg(test)]
pub fn check_relink_hello(msg: &Msg, build: &str, peer: Option<Partner>) -> Result<Partner, String> {
    check_relink_hello_to("window 2", msg, build, peer)
}

/// Checks the `hello` of a window 1 that connected to the orphan `who`: of this `build`, calling itself window 1, said
/// by the process that connected (`peer`). The partner it names, or why it is refused.
fn check_relink_hello_to(who: &str, msg: &Msg, build: &str, peer: Option<Partner>) -> Result<Partner, String> {
    let Msg::Hello {
        build: theirs,
        window,
        pid,
        start,
        ..
    } = msg
    else {
        return Err(format!(
            "the new window's first message was `{}`, not hello",
            msg.kind()
        ));
    };
    if theirs != build {
        return Err(another_build_text(who, theirs, build));
    }
    if window != FIRST_WINDOW {
        return Err(format!("the new window calls itself window {}", shown(window)));
    }
    let Some(peer) = peer else {
        return Err("the process that connected cannot be told apart from another".into());
    };
    if (*pid, *start) != (peer.pid, peer.start) {
        return Err(format!(
            "hello names process {pid} (started at {start}), not the one that connected ({}, {})",
            peer.pid, peer.start
        ));
    }
    Ok(peer)
}

/// The `hello` of a window 1 coming back for an orphan: its build and its process, no turn running yet (its panel
/// does not exist), its presence not confirmed.
pub fn hello_as_window_1(build: String, pid: u32, start: u64) -> Msg {
    Msg::Hello {
        build,
        window: FIRST_WINDOW.to_string(),
        pid,
        start,
        presence_ready: false,
        running: Vec::new(),
        summary: Summary::default(),
    }
}

/// The holders an orphan names in its `welcome`: each module of `held` that only one window can have (a numbered
/// instance, a page) is window 2's. The terminal and the chat every window has of its own are no one's to hold.
pub fn welcome_holders(held: &BTreeSet<String>) -> BTreeMap<String, String> {
    held.iter()
        .filter(|text| ModuleId::parse(text).is_ok_and(|id| id.kind() == ModuleKind::LuaWebview || id.number() >= 2))
        .map(|text| (text.clone(), WINDOW_2.to_string()))
        .collect()
}

/// The orphan's `welcome` to a window 1 that came back: what it holds now and the turns running in it, its
/// presence not confirmed yet. The feeds are window 1's to send.
pub fn relink_welcome(held: &BTreeSet<String>, running: Vec<TurnEdge>) -> Msg {
    welcome_from(SECOND_WINDOW.to_string(), welcome_holders(held), running)
}

/// As [`relink_welcome`], from the orphan `name`. A module window says its own name and holds its one module, which
/// its name gives whatever its layout calls it (a chat window's base chat is its `agent#N`).
pub fn relink_welcome_as(name: &WindowName, held: &BTreeSet<String>, running: Vec<TurnEdge>) -> Msg {
    match name.module() {
        Some(module) => welcome_from(
            name.text(),
            BTreeMap::from([(module.as_str().to_string(), name.text())]),
            running,
        ),
        None => relink_welcome(held, running),
    }
}

fn welcome_from(window: String, holders: BTreeMap<String, String>, running: Vec<TurnEdge>) -> Msg {
    Msg::Welcome {
        window,
        presence_ready: false,
        running,
        summary: Summary::default(),
        holders,
        context: None,
        theme: None,
        keys: None,
    }
}

/// The orphan's answer to a `hello` on a connection it accepted: checked, then `welcome` with a copy of `lock`, and
/// the link is linked to `peer`. A refused hello is answered with `bye` naming why, and the connection closes.
pub fn answer_relink_hello(
    link: &LinkDriver,
    msg: &Msg,
    build: &str,
    peer: Option<Partner>,
    lock: &PairLock,
    welcome: Msg,
) -> Result<Partner, String> {
    let refuse = |why: String| {
        eprintln!("[link] {why}; the re-link is refused");
        link.close(&why);
        Err(why)
    };
    if link.linked() {
        return Err("a second hello on a linked link".into());
    }
    let who = match &welcome {
        Msg::Welcome { window, .. } => WindowName::parse(window).map_or_else(|_| "this window".into(), |n| called(&n)),
        _ => "this window".to_string(),
    };
    let partner = match check_relink_hello_to(&who, msg, build, peer) {
        Ok(partner) => partner,
        Err(why) => return refuse(why),
    };
    let shared = match lock.share() {
        Ok(fd) => fd,
        Err(err) => return refuse(format!("the pair lock could not be shared ({err})")),
    };
    if let Err(err) = link.send(welcome, vec![shared]) {
        return refuse(format!("welcome could not be sent ({err})"));
    }
    link.set_partner(partner);
    if let Msg::Hello { running, .. } = msg {
        link.set_partner_running(running.clone());
    }
    link.set_linked();
    Ok(partner)
}

/// A window 1 linked to an orphan, handed from the worker that made the link to the window it starts.
pub struct Relinked {
    /// The orphan's name, as its `welcome` said it and its socket's name confirmed it.
    pub name: WindowName,
    pub handle: LinkHandle,
    pub inbound: Receiver<Inbound>,
    /// Set while the orphan counts as another window for turn review: from the handshake until it confirms.
    pub presence: PresenceFlag,
    pub lock: PairLock,
    pub partner: Partner,
    /// The turns the orphan said were running as it welcomed this window.
    pub running: Vec<TurnEdge>,
    /// What the orphan said it holds, as it said it: the other window's input, checked where it is used.
    pub holders: BTreeMap<String, String>,
}

/// Why there is no re-link when no orphaned window 2 listens: the pair lock is another full window's, and this window
/// runs unpaired as it always has, with nothing to tell the user.
pub const NO_ORPHAN_TEXT: &str = "no window 2 waits for this project";

/// What a window that found an orphaned window 2 and could not link to it shows on screen, so the user learns why
/// it runs unpaired and how to get the pair back (the refusal's own words name it); nothing when no orphan
/// listened.
pub fn refusal_note(why: &str) -> Option<String> {
    (why != NO_ORPHAN_TEXT).then(|| format!("not linked to window 2: {why}; this window runs unpaired"))
}

/// As [`refusal_note`], for the module windows waiting when no window 2 was.
pub fn group_refusal_note(why: &str) -> String {
    format!("not linked to the windows waiting for this project: {why}; this window runs unpaired")
}

/// Who a re-link expects at the end of a socket.
enum Waiting {
    Second,
    /// A module window, by the file form its socket's name carries.
    Module {
        form: String,
    },
}

impl Waiting {
    fn who(&self) -> String {
        match self {
            Waiting::Second => "window 2".to_string(),
            Waiting::Module { form } => format!("the window {form}"),
        }
    }

    /// The window a `welcome` on this socket names, when that is the one that waits here. A name is the other
    /// window's input: only its match with the socket's own name makes it a name this window keeps.
    fn named(&self, said: &str) -> Result<WindowName, String> {
        let name = WindowName::parse(said).ok().filter(|name| match self {
            Waiting::Second => *name == WindowName::Second,
            Waiting::Module { form } => *name != WindowName::Second && name.file_form() == *form,
        });
        name.ok_or_else(|| format!("welcome came from window {}", shown(said)))
    }
}

/// A window 1 starting for a project whose pair lock is held: connects once to the orphan's socket in `dir`, on a
/// worker thread this call joins, bounded by `within`. `layout_dir` is where this project's pair lock file is, which
/// the lock in the `welcome` must be. `Err` says why there is no re-link; the window then runs unpaired.
pub fn relink(
    dir: &Path,
    layout_dir: &Path,
    project_root: &Path,
    build: String,
    within: Duration,
) -> Result<Relinked, String> {
    let path = socket_path(dir, project_root).map_err(|e| e.to_string())?;
    let lock_file = layout_dir.join(pair_lock_name(project_root));
    let deadline = Instant::now() + within;
    let worker = std::thread::Builder::new()
        .name("eitri-relink".into())
        .spawn(move || relink_on(&path, &lock_file, build, deadline, within, &Waiting::Second))
        .map_err(|e| format!("the re-link could not be started ({e})"))?;
    worker
        .join()
        .unwrap_or_else(|_| Err("the re-link stopped unexpectedly".into()))
}

/// The module windows' sockets waiting in `dir` for this project, each with the file form its name carries. Window 2's
/// socket has no part after the key and is not among them.
fn module_sockets(dir: &Path, project_root: &Path) -> Result<Vec<(PathBuf, String)>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("{} could not be listed ({err})", dir.display())),
    };
    let prefix = format!("w-{}-", eitri_core::state_paths::project_key(project_root));
    let mut sockets = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(text) = file_name.to_str() else { continue };
        let form = text.strip_prefix(&prefix).and_then(|rest| rest.strip_suffix(".sock"));
        if let (Some(form), Ok(path)) = (form, agent::socket_path::in_dir(dir, text)) {
            sockets.push((path, form.to_string()));
        }
    }
    Ok(sockets)
}

/// A window 1 that found no window 2 waiting: connects to every module window waiting in `dir`, each on a worker of its
/// own, all inside one `within` from now, and gives back every one that welcomed it in time, each with its own copy of
/// the pair lock, checked as [`relink`] checks it. One that misses the bound or refuses is left to go on waiting. A
/// socket nothing listens on is a leftover, which is no refusal. `Err` says why none linked when one refused and none
/// came.
pub fn relink_all(
    dir: &Path,
    layout_dir: &Path,
    project_root: &Path,
    build: String,
    within: Duration,
) -> Result<Vec<Relinked>, String> {
    let sockets = module_sockets(dir, project_root)?;
    let lock_file = layout_dir.join(pair_lock_name(project_root));
    let deadline = Instant::now() + within;
    let (done, results) = mpsc::channel();
    let mut refusals = Vec::new();
    let mut asked = 0;
    for (path, form) in sockets {
        let (done, lock_file, build) = (done.clone(), lock_file.clone(), build.clone());
        let started = std::thread::Builder::new().name("eitri-relink".into()).spawn(move || {
            let waiting = Waiting::Module { form };
            // Nobody reads this after the bound: the link it made goes with it, and the orphan waits again.
            let _ = done.send(relink_on(&path, &lock_file, build, deadline, within, &waiting));
        });
        match started {
            Ok(_) => asked += 1,
            Err(err) => refusals.push(format!("the re-link could not be started ({err})")),
        }
    }
    drop(done);
    let mut relinked = Vec::new();
    for _ in 0..asked {
        // Waits on the results rather than joining the workers: a worker past the bound holds nobody up.
        match results.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(one)) => relinked.push(one),
            Ok(Err(why)) if why == NO_ORPHAN_TEXT => {}
            Ok(Err(why)) => refusals.push(why),
            Err(_) => {
                refusals.push(format!("a window did not answer within {} ms", within.as_millis()));
                break;
            }
        }
    }
    for why in &refusals {
        println!("[window] a waiting window is not linked: {why}");
    }
    relinked.sort_by(|a, b| a.name.cmp(&b.name));
    match refusals.into_iter().next() {
        Some(why) if relinked.is_empty() => Err(why),
        _ => Ok(relinked),
    }
}

/// What a window 1 starting for a project whose pair lock is held found waiting.
pub enum StartRelink {
    /// Nobody it could link to: it runs unpaired.
    Nothing,
    Window2(Relinked),
    /// Every orphan that linked, and the claim on their group, which the window keeps for as long as it runs.
    Modules(Vec<Relinked>, GroupClaim),
}

/// A window 1's start on a project whose pair lock is held: window 2's orphan first, and only when none waits, every
/// module window. What it found, and the note to show when something waiting refused it.
pub fn relink_at_start(
    dir: &Path,
    layout_dir: &Path,
    project_root: &Path,
    build: String,
    within: Duration,
) -> (StartRelink, Option<String>) {
    let why = match relink(dir, layout_dir, project_root, build.clone(), within) {
        Ok(one) => return (StartRelink::Window2(one), None),
        Err(why) if why != NO_ORPHAN_TEXT => {
            println!("[window] no re-link: {why}; this window runs unpaired");
            return (StartRelink::Nothing, refusal_note(&why));
        }
        Err(why) => why,
    };
    let claim = match GroupClaim::try_take(layout_dir, project_root) {
        Ok(Some(claim)) => claim,
        Ok(None) => {
            println!("[window] no re-link: another window leads this project's group; this window runs unpaired");
            return (StartRelink::Nothing, None);
        }
        Err(err) => {
            println!("[window] no re-link: the group cannot be claimed ({err}); this window runs unpaired");
            return (StartRelink::Nothing, None);
        }
    };
    match relink_all(dir, layout_dir, project_root, build, within) {
        Ok(group) if !group.is_empty() => (StartRelink::Modules(group, claim), None),
        Ok(_) => {
            println!("[window] no re-link: {why}; this window runs unpaired");
            (StartRelink::Nothing, None)
        }
        Err(why) => {
            println!("[window] no re-link: {why}; this window runs unpaired");
            (StartRelink::Nothing, Some(group_refusal_note(&why)))
        }
    }
}

fn relink_on(
    path: &Path,
    lock_file: &Path,
    build: String,
    deadline: Instant,
    within: Duration,
    waiting: &Waiting,
) -> Result<Relinked, String> {
    let who = waiting.who();
    let conn = connect_to(path).map_err(|e| match e.raw_os_error() {
        Some(libc::ENOENT) | Some(libc::ECONNREFUSED) => NO_ORPHAN_TEXT.to_string(),
        _ => format!("{} could not be reached ({e})", path.display()),
    })?;
    let (pid, uid) = fdpass::peer_cred(conn.as_fd()).map_err(|e| format!("the listener names no peer ({e})"))?;
    if uid != euid() {
        return Err(format!("{} belongs to uid {uid}; not linking to it", path.display()));
    }
    let start = agent::child::start_time(pid).map_err(|e| format!("{who} (process {pid}) cannot be named ({e})"))?;
    // SAFETY: neither call has preconditions or can fail.
    let me = unsafe { libc::getpid() } as u32;
    let my_start = agent::child::start_time(me).map_err(|e| format!("this process cannot be named ({e})"))?;
    let presence = PresenceFlag(Arc::new(AtomicBool::new(true)));
    let (handle, inbound) =
        LinkHandle::start(conn, presence.clone()).map_err(|e| format!("the link could not be started ({e})"))?;
    handle
        .send(hello_as_window_1(build, me, my_start), Vec::new())
        .map_err(|e| format!("hello could not be sent ({e})"))?;
    let left = || deadline.saturating_duration_since(Instant::now());
    let (window, running, holders, fds) = match inbound.recv_timeout(left()) {
        Ok(Inbound::Msg {
            msg:
                Msg::Welcome {
                    window,
                    running,
                    holders,
                    ..
                },
            fds,
        }) => (window, running, holders, fds),
        Ok(Inbound::Msg {
            msg: Msg::Bye { why }, ..
        }) => {
            return Err(format!("{who} refused the re-link: {}", shown_within(&why, BYE_CHARS)));
        }
        Ok(Inbound::Msg { msg, .. }) => {
            return Err(format!("{who}'s first message was `{}`, not welcome", msg.kind()));
        }
        Ok(Inbound::Refused(err)) => return Err(format!("{who}'s answer was refused ({err})")),
        Ok(Inbound::Closed) => return Err(format!("{who} closed the connection")),
        Err(RecvTimeoutError::Timeout) => {
            return Err(format!("{who} did not answer within {} ms", within.as_millis()));
        }
        Err(RecvTimeoutError::Disconnected) => return Err(format!("the link ended before {who} answered")),
    };
    let name = waiting.named(&window)?;
    // The link checked that `welcome` carries exactly one descriptor.
    let fd = fds.into_iter().next().ok_or("welcome carried no pair lock")?;
    let theirs = std::fs::File::from(fd.try_clone().map_err(|e| e.to_string())?)
        .metadata()
        .map(|meta| (meta.dev(), meta.ino()))
        .map_err(|e| format!("the pair lock in welcome cannot be read ({e})"))?;
    if identity(lock_file) != Some(theirs) {
        return Err(format!(
            "the lock in welcome is not this project's pair lock ({})",
            lock_file.display()
        ));
    }
    let lock = PairLock::adopt(fd).map_err(|e| format!("the pair lock in welcome is not one ({e})"))?;
    if holders.len() > MAX_WELCOME_HOLDERS {
        return Err(format!(
            "welcome names {} holders; a window holds far fewer",
            holders.len()
        ));
    }
    Ok(Relinked {
        name,
        handle,
        inbound,
        presence,
        lock,
        partner: Partner { pid, start },
        running,
        holders,
    })
}

/// The flags of the window 2 that starts an editor window, which that window runs with too.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EditorFlags {
    /// `--legacy`: its chats run the legacy backend, as window 2's do.
    pub legacy: bool,
    /// `--clean`: it neither reads nor writes the remembered state, as window 2 does not.
    pub clean: bool,
}

/// An editor window this window started: its pid, and whether its process still runs, as its reaper sees it.
#[derive(Debug, Clone)]
pub struct StartedEditor {
    pid: u32,
    running: Arc<AtomicBool>,
}

impl StartedEditor {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether the process still runs. One that has no reaper counts as ended: nothing would ever say it did.
    pub fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

/// The editor window's arguments: window 2's flags, then the project root.
fn editor_args(project_root: &Path, flags: EditorFlags) -> Vec<std::ffi::OsString> {
    let mut args = Vec::new();
    if flags.legacy {
        args.push("--legacy".into());
    }
    if flags.clean {
        args.push("--clean".into());
    }
    args.push(project_root.as_os_str().to_os_string());
    args
}

/// Starts `exe [--legacy] [--clean] <root>` (`flags`) detached from this window: a session of its own, its standard
/// streams on `/dev/null`, its working directory the project. Its exit is reaped by a thread of this window and never
/// waited for here. The pid it got, and whether it still runs.
pub fn start_editor(exe: &Path, project_root: &Path, flags: EditorFlags) -> io::Result<StartedEditor> {
    if !project_root.is_absolute() || !project_root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("the project {} is not a directory", project_root.display()),
        ));
    }
    let mut command = Command::new(exe);
    command
        .args(editor_args(project_root, flags))
        .current_dir(project_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe and the closure touches nothing else of this process.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let pid = child.id();
    let running = Arc::new(AtomicBool::new(true));
    let reaper = {
        let running = running.clone();
        std::thread::Builder::new()
            .name("editor-window-reaper".into())
            .spawn(move || {
                match child.wait() {
                    Ok(status) => println!("[window] the editor window started here (process {pid}) ended: {status}"),
                    Err(err) => eprintln!("[window] the editor window (process {pid}) could not be waited for: {err}"),
                }
                running.store(false, Ordering::SeqCst);
            })
    };
    if let Err(err) = reaper {
        // The process runs on; with no thread to reap it, it is left for this window's exit to reap.
        eprintln!("[window] the editor window (process {pid}) has no reaper ({err})");
        running.store(false, Ordering::SeqCst);
    }
    Ok(StartedEditor { pid, running })
}

#[cfg(test)]
mod tests;
