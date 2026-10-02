//! Closing the panel when the editor it was started with exits (`eitri split`).
//!
//! Two halves. [`CloseSlot`] is the pure rule for which attachment the watch belongs to: it follows
//! the attach *requests* the panel accepted, never the link's live state, because nvim's socket
//! usually closes a moment before Neovide exits and the link has let go of its address by then.
//! [`CloseWatch`] is the process half: a pidfd on the editor, watched on the GLib main context.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::rc::Rc;

use gtk4::glib;

use eitri_core::panel_control::CloseWith;

/// What one accepted attach request does to the watch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotChange {
    /// The request names its own editor: whatever was watched is replaced by this.
    Replace(CloseWith),
    /// A request for the address already watched that names no editor: `:EitriPanel` in that same
    /// nvim, which only raises the window.
    Keep,
    /// A request for another address: the watched editor is no longer the panel's.
    Remove,
    /// Nothing was watched and nothing is asked for.
    Nothing,
}

/// What a watched editor's exit means for the panel, once the requests already taken are known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitDecision {
    /// The watch that fired is still the panel's and nothing is on its way: close.
    Close,
    /// The control thread has accepted a request this side has not taken yet. It may replace or
    /// remove the watch, so the decision is made again once it is in.
    Wait,
    /// The watch that fired was replaced or removed: its editor is no longer the panel's.
    Ignore,
}

/// The address the watch belongs to, if there is a watch.
#[derive(Debug, Default)]
pub(crate) struct CloseSlot {
    addr: Option<PathBuf>,
    /// Counts every change of what is watched. A watch that fires carries the number it was opened
    /// under, and is the panel's current one only while that is still the number.
    generation: u64,
}

impl CloseSlot {
    /// Records `request` and says what to do about the watch. A request with `close_with` always
    /// replaces; the caller says through [`CloseSlot::clear`] when the watch could not be opened.
    pub(crate) fn on_request(&mut self, addr: &Path, close_with: Option<CloseWith>) -> SlotChange {
        if let Some(close_with) = close_with {
            self.addr = Some(addr.to_path_buf());
            self.generation += 1;
            return SlotChange::Replace(close_with);
        }
        match &self.addr {
            Some(watched) if watched == addr => SlotChange::Keep,
            Some(_) => {
                self.addr = None;
                self.generation += 1;
                SlotChange::Remove
            }
            None => SlotChange::Nothing,
        }
    }

    /// There is no watch after all (the process was already gone).
    pub(crate) fn clear(&mut self) {
        self.addr = None;
        self.generation += 1;
    }

    /// See [`CloseWatcher::exit_decision`].
    pub(crate) fn exit_decision(&self, generation: u64, accepted: u64, taken: u64) -> ExitDecision {
        if !self.is_current(generation) {
            ExitDecision::Ignore
        } else if taken < accepted {
            ExitDecision::Wait
        } else {
            ExitDecision::Close
        }
    }

    /// The number a watch opened now belongs to.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether a watch opened under `generation` is still the one the panel has: nothing replaced
    /// or removed it since, so its editor exiting is the panel's reason to close.
    pub(crate) fn is_current(&self, generation: u64) -> bool {
        self.addr.is_some() && self.generation == generation
    }

    #[cfg(test)]
    fn watched(&self) -> Option<&Path> {
        self.addr.as_deref()
    }
}

/// A pidfd on the editor process. Linux only: elsewhere there is nothing to open.
struct Pidfd(OwnedFd);

impl Pidfd {
    /// Opens a pidfd on `pid`, then checks that the process started when `start` says. The check is
    /// after the open on purpose: the sender verified the pair a moment ago, and a pid that was
    /// reused since must name nothing. `Err` says why, for the log.
    #[cfg(target_os = "linux")]
    fn open(pid: u32, start: u64) -> Result<Pidfd, String> {
        // SAFETY: `pidfd_open` takes a pid and flags and returns a new descriptor or -1; no pointer
        // is involved.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0 as libc::c_uint) };
        if fd < 0 {
            return Err(format!("pidfd_open({pid}): {}", std::io::Error::last_os_error()));
        }
        // SAFETY: `fd` is a descriptor `pidfd_open` just returned, owned by nobody else.
        let owned = unsafe { OwnedFd::from_raw_fd(fd as std::os::fd::RawFd) };
        match eitri_core::split::proc_start_time(pid) {
            Some(now) if now == start => Ok(Pidfd(owned)),
            Some(_) => Err(format!(
                "pid {pid} is not the process that was named (its start time changed)"
            )),
            None => Err(format!("pid {pid} is already gone")),
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn open(pid: u32, _start: u64) -> Result<Pidfd, String> {
        Err(format!("no pidfd on this platform to watch pid {pid} with"))
    }

    /// Whether the process has exited, by polling the descriptor without waiting.
    #[cfg(test)]
    fn readable(&self, wait_ms: i32) -> bool {
        let mut poll = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid `pollfd` and a count of one.
        unsafe { libc::poll(&mut poll, 1, wait_ms) == 1 && poll.revents & libc::POLLIN != 0 }
    }
}

/// One close watch: a pidfd on the editor, watched on the main context, and the editor's `Child`
/// when this process started it (so it is reaped once it exits). Dropping it cancels the watch.
pub(crate) struct CloseWatch {
    source: Option<glib::SourceId>,
    /// Set once the callback has run: a source that returned `Break` is already destroyed, and
    /// removing it again is a GLib critical.
    fired: Rc<std::cell::Cell<bool>>,
    child: Rc<std::cell::RefCell<Option<Child>>>,
    // Declared last so it closes after the source is removed (`fd_watch::add_local`'s contract).
    _fd: Pidfd,
}

impl CloseWatch {
    /// Watches `close_with` and calls `on_exit` once when that process exits. `None` (the reason is
    /// logged) when the process is gone or is no longer the one named. Must run on the GTK thread.
    pub(crate) fn open(
        close_with: CloseWith,
        addr: &Path,
        child: Option<Child>,
        on_exit: impl FnOnce() + 'static,
    ) -> Option<CloseWatch> {
        let pidfd = match Pidfd::open(close_with.pid, close_with.start) {
            Ok(pidfd) => pidfd,
            Err(why) => {
                println!("[companion] no close watch for {}: {why}", addr.display());
                return None;
            }
        };
        let fired = Rc::new(std::cell::Cell::new(false));
        let child = Rc::new(std::cell::RefCell::new(child));
        let mut on_exit = Some(on_exit);
        let source = {
            let fired = fired.clone();
            let child = child.clone();
            neovide_editor::fd_watch::add_local(
                pidfd.0.as_raw_fd(),
                glib::IOCondition::IN,
                glib::ffi::G_PRIORITY_DEFAULT,
                move |_| {
                    fired.set(true);
                    // The editor is our own child when this process started it: collect it, so it
                    // is not left a zombie for as long as the panel lives.
                    if let Some(mut child) = child.borrow_mut().take() {
                        let _ = child.try_wait();
                    }
                    if let Some(on_exit) = on_exit.take() {
                        on_exit();
                    }
                    glib::ControlFlow::Break
                },
            )
        };
        match source {
            Ok(source) => Some(CloseWatch {
                source: Some(source),
                fired,
                child,
                _fd: pidfd,
            }),
            Err(neovide_editor::fd_watch::NotOwner) => {
                println!(
                    "[companion] no close watch for {}: not on the main thread",
                    addr.display()
                );
                None
            }
        }
    }
}

impl Drop for CloseWatch {
    fn drop(&mut self) {
        if !self.fired.get() {
            if let Some(source) = self.source.take() {
                source.remove();
            }
            // A child that is still running cannot be waited for here. Something has to collect it
            // when it exits, or it stays a zombie until this process ends.
            if let Some(mut child) = self.child.borrow_mut().take() {
                let _ = std::thread::Builder::new()
                    .name("eitri-reap-editor".into())
                    .spawn(move || {
                        let _ = child.wait();
                    });
            }
        }
    }
}

/// The panel's one close watch and the rule that decides which attachment it belongs to.
pub(crate) struct CloseWatcher {
    slot: CloseSlot,
    watch: Option<CloseWatch>,
    /// Called with the generation of the watch whose editor exited. It decides what that means:
    /// requests the control thread already accepted may still be waiting to be taken, and one of
    /// them can have replaced the watch that fired.
    on_exit: Option<Rc<dyn Fn(u64)>>,
}

impl CloseWatcher {
    pub(crate) fn new() -> CloseWatcher {
        CloseWatcher {
            slot: CloseSlot::default(),
            watch: None,
            on_exit: None,
        }
    }

    /// Where an editor's exit is reported. Set once, before the first watch is installed.
    pub(crate) fn set_on_exit(&mut self, on_exit: impl Fn(u64) + 'static) {
        self.on_exit = Some(Rc::new(on_exit));
    }

    /// What the exit of the watch opened under `generation` means, given how many requests the
    /// control thread has accepted and how many this side has taken. A request that was accepted but
    /// not yet taken is the one case where the answer is not known: the editor that sent it may
    /// have been the panel's new one, and it was told `ok`.
    pub(crate) fn exit_decision(&self, generation: u64, accepted: u64, taken: u64) -> ExitDecision {
        if self.watch.is_none() {
            return ExitDecision::Ignore;
        }
        self.slot.exit_decision(generation, accepted, taken)
    }

    /// The editor this process started: watched for the address it was told to listen on.
    pub(crate) fn install(&mut self, addr: PathBuf, close_with: CloseWith, child: Option<Child>) {
        self.replace(&addr, close_with, child);
    }

    /// One accepted attach request, in the order they were accepted.
    pub(crate) fn on_request(&mut self, addr: &Path, close_with: Option<CloseWith>) {
        match self.slot.on_request(addr, close_with) {
            SlotChange::Replace(close_with) => self.replace(addr, close_with, None),
            SlotChange::Remove => self.watch = None,
            SlotChange::Keep | SlotChange::Nothing => {}
        }
    }

    fn replace(&mut self, addr: &Path, close_with: CloseWith, child: Option<Child>) {
        // The old watch goes first, so a pid that appears in both is never watched twice.
        self.watch = None;
        // The slot is told only now: a watch that could not be opened is not a watch.
        self.slot.on_request(addr, Some(close_with));
        let generation = self.slot.generation();
        let on_exit = self.on_exit.clone();
        self.watch = CloseWatch::open(close_with, addr, child, move || {
            if let Some(on_exit) = on_exit {
                on_exit(generation);
            }
        });
        if self.watch.is_none() {
            self.slot.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const A: &str = "/run/user/1000/eitri/a";
    const B: &str = "/run/user/1000/eitri/b";

    fn with(pid: u32) -> Option<CloseWith> {
        Some(CloseWith { pid, start: 1 })
    }

    #[test]
    fn a_request_with_close_with_installs_and_replaces() {
        let mut slot = CloseSlot::default();
        assert_eq!(
            slot.on_request(Path::new(A), with(5)),
            SlotChange::Replace(CloseWith { pid: 5, start: 1 })
        );
        assert_eq!(slot.watched(), Some(Path::new(A)));
        // A second editor, even for the same address, replaces the first.
        assert_eq!(
            slot.on_request(Path::new(A), with(6)),
            SlotChange::Replace(CloseWith { pid: 6, start: 1 })
        );
        assert_eq!(
            slot.on_request(Path::new(B), with(7)),
            SlotChange::Replace(CloseWith { pid: 7, start: 1 })
        );
        assert_eq!(slot.watched(), Some(Path::new(B)));
    }

    #[test]
    fn the_watch_follows_requests_not_the_link() {
        let mut slot = CloseSlot::default();
        slot.on_request(Path::new(A), with(5));
        // `:EitriPanel` in the same nvim: the watch stays.
        assert_eq!(slot.on_request(Path::new(A), None), SlotChange::Keep);
        assert_eq!(slot.watched(), Some(Path::new(A)));
        // The link going `Detached` is not a request: nothing here asks the slot about it, and the
        // watch is still there for the exit callback, which consults nothing but its own pidfd.
        assert_eq!(slot.watched(), Some(Path::new(A)));
        // Another nvim: the editor that was watched is no longer this panel's.
        assert_eq!(slot.on_request(Path::new(B), None), SlotChange::Remove);
        assert_eq!(slot.watched(), None);
        // With no watch, an ordinary request leaves it that way.
        assert_eq!(slot.on_request(Path::new(B), None), SlotChange::Nothing);
        assert_eq!(slot.on_request(Path::new(A), None), SlotChange::Nothing);
    }

    #[test]
    fn a_watch_that_fired_is_current_only_until_a_request_replaces_or_removes_it() {
        let mut slot = CloseSlot::default();
        slot.on_request(Path::new(A), with(5));
        let first = slot.generation();
        assert!(slot.is_current(first));
        // `:EitriPanel` in the same nvim leaves the watch as it is.
        slot.on_request(Path::new(A), None);
        assert!(slot.is_current(first));
        // A split for another editor replaced it: the first editor exiting is no reason to close.
        slot.on_request(Path::new(B), with(6));
        assert!(!slot.is_current(first));
        let second = slot.generation();
        assert!(slot.is_current(second));
        // An attach to a third nvim removed the watch.
        slot.on_request(Path::new(A), None);
        assert!(!slot.is_current(second));
        // Nor does a number that was current before a watch could not be opened.
        slot.on_request(Path::new(B), with(7));
        let third = slot.generation();
        slot.clear();
        assert!(!slot.is_current(third));
    }

    #[test]
    fn an_exit_with_a_request_still_on_its_way_waits_and_is_then_decided_by_what_it_was() {
        let mut slot = CloseSlot::default();
        slot.on_request(Path::new(A), with(5));
        let first = slot.generation();
        // Nothing accepted that this side has not taken: the editor's exit closes the panel.
        assert_eq!(slot.exit_decision(first, 0, 0), ExitDecision::Close);
        assert_eq!(slot.exit_decision(first, 3, 3), ExitDecision::Close);
        // The control thread said `ok` to something this side has not read yet.
        assert_eq!(slot.exit_decision(first, 4, 3), ExitDecision::Wait);
        // It was a split for another editor: once taken, the first editor is no longer the panel's.
        slot.on_request(Path::new(B), with(6));
        assert_eq!(slot.exit_decision(first, 4, 4), ExitDecision::Ignore);
        // Or it was a raise: the watch stands, and the panel closes.
        let second = slot.generation();
        assert_eq!(slot.exit_decision(second, 5, 4), ExitDecision::Wait);
        assert_eq!(slot.exit_decision(second, 5, 5), ExitDecision::Close);
        // A request that was withdrawn (its reply failed) brings the count back down.
        assert_eq!(slot.exit_decision(second, 4, 5), ExitDecision::Close);
    }

    #[test]
    fn a_watch_that_could_not_be_opened_leaves_nothing_to_keep() {
        let mut slot = CloseSlot::default();
        slot.on_request(Path::new(A), with(5));
        slot.clear();
        assert_eq!(slot.on_request(Path::new(A), None), SlotChange::Nothing);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_child_that_exits_makes_the_pidfd_readable() {
        let mut child = Command::new("sleep").arg("0.3").spawn().unwrap();
        let pid = child.id();
        let start = eitri_core::split::proc_start_time(pid).expect("a live child has a start time");
        let pidfd = Pidfd::open(pid, start).expect("the pair is right");
        assert!(!pidfd.readable(0), "the child is still running");
        assert!(pidfd.readable(5000), "the child exited");
        child.wait().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_wrong_start_time_or_a_dead_pid_opens_nothing() {
        let mut child = Command::new("sleep").arg("5").spawn().unwrap();
        let pid = child.id();
        let start = eitri_core::split::proc_start_time(pid).unwrap();
        let err = Pidfd::open(pid, start + 1).err().expect("a different start time");
        assert!(err.contains("start time"), "{err}");
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(Pidfd::open(pid, start).is_err());
    }
}
