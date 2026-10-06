//! The companion window's link to the user's own nvim: a [`LinkDriver`] the host polls, and the
//! panel's [`EditorRpc`] over it. The driver does its connecting on worker threads and only ever
//! tries an answer, so a poll never blocks; the host owns the timer that calls [`CompanionLink::poll`]
//! every [`CompanionLink::POLL_INTERVAL`] and acts on what it reports.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rmpv::Value;

use super::attach::{BandLink, LinkState};
use super::driver::LinkDriver;
use super::Sockets;
use crate::editor_rpc::EditorRpc;
use crate::nvim_rpc::Pending;

/// What one poll of the link found.
pub struct LinkPoll {
    /// Lines the driver wants on stdout, in order.
    pub logs: Vec<String>,
    /// The link's state after the poll, when it changed: the state, the band's view of it and the
    /// pid of the process holding the editor's socket ([`CompanionLink::nvim_pid`]).
    pub changed: Option<(LinkState, BandLink, Option<u32>)>,
    /// Draft edits in the editor must end (the editor went away or was swapped).
    pub cancel_drafts: bool,
    /// The driver is shut: stop polling.
    pub shut: bool,
}

pub struct CompanionLink {
    driver: RefCell<LinkDriver>,
}

impl CompanionLink {
    /// How often the host polls.
    pub const POLL_INTERVAL: Duration = Duration::from_millis(50);

    /// Starts following `initial` (an nvim address) or nothing.
    pub fn new(initial: Option<PathBuf>, sockets: Sockets) -> Self {
        CompanionLink {
            driver: RefCell::new(LinkDriver::new(initial, sockets)),
        }
    }

    /// The state, the band's view of it and the pid of the process holding the editor's socket.
    pub fn snapshot(&self) -> (LinkState, BandLink, Option<u32>) {
        let driver = self.driver.borrow();
        (driver.state().clone(), driver.band(), driver.peer_pid())
    }

    /// One step of the driver. No borrow of it is held once this returns, so whatever the host does
    /// with the result may call back into the link.
    pub fn poll(&self, now: Instant) -> LinkPoll {
        let tick = self.driver.borrow_mut().poll(now);
        let changed = tick.state_changed.then(|| self.snapshot());
        LinkPoll {
            logs: tick.logs,
            changed,
            cancel_drafts: tick.cancel_drafts,
            shut: self.driver.borrow().is_shut(),
        }
    }

    /// Aim the panel at this nvim, leaving the one it already follows as it is; the change shows at
    /// the next poll.
    pub fn attach(&self, addr: PathBuf) {
        self.driver.borrow().attach(addr);
    }

    /// Run Lua that needs the install's part `part` in the attached nvim, without waiting for its
    /// answer. `Err` says why not.
    pub fn exec_lua_for(&self, part: &str, code: &str, args: Vec<Value>) -> Result<(), String> {
        // editor-rpc-scan: forwards a caller's constant
        self.driver.borrow_mut().exec_lua_for(Some(part), code, args)
    }

    /// Hand `keys` to the attached nvim as typed (`nvim_input`), without waiting for an answer. Unlike a Lua
    /// call it is answered while nvim waits for a character, so it never lands after a later key. `Err` says
    /// why not.
    pub fn input(&self, keys: &str) -> Result<(), String> {
        self.driver.borrow_mut().input(keys)
    }

    /// Who owns the review module in the attached nvim: this panel, on its channel there. `None`
    /// unless attached.
    pub fn review_owner(&self) -> Option<crate::review_editor::Owner> {
        match self.driver.borrow().state() {
            LinkState::Attached { channel, .. } => Some(crate::review_editor::Owner::Companion { channel: *channel }),
            _ => None,
        }
    }

    /// The editor's pid for finding its window: the process holding the socket, as the kernel says,
    /// never the pid the editor reported (any process that answers the install can report any pid).
    /// `None` unless attached, and where the platform gives no pid.
    pub fn nvim_pid(&self) -> Option<u32> {
        self.driver.borrow().peer_pid()
    }

    /// [`CompanionLink::nvim_pid`], when the link is attached to `addr` itself.
    pub fn attached_to(&self, addr: &Path) -> Option<u32> {
        let driver = self.driver.borrow();
        match driver.state() {
            LinkState::Attached { addr: attached, .. } if attached == addr => driver.peer_pid(),
            _ => None,
        }
    }

    /// Lets go of the editor: the teardown is queued, then the connection closes once it is
    /// written. Never waits.
    pub fn shutdown(&self) {
        self.driver.borrow_mut().shutdown();
    }
}

/// The attached nvim, answered. A poll lets go of the driver before it returns, so a call made by
/// the host while it acts on the result finds the driver free.
impl EditorRpc for CompanionLink {
    fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
        // editor-rpc-scan: forwards a caller's constant
        self.driver.borrow_mut().exec_lua_answered(code, args)
    }

    fn target(&self) -> Option<u64> {
        self.driver.borrow().generation()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_with_no_editor_polls_idle_and_shuts() {
        let link = CompanionLink::new(None, Sockets::default());
        let first = link.poll(Instant::now());
        assert!(first.changed.is_none());
        assert!(!first.cancel_drafts);
        assert!(!first.shut);
        assert!(first.logs.is_empty());
        assert!(matches!(link.snapshot().0, LinkState::NoEditor));
        link.shutdown();
        assert!(link.poll(Instant::now()).shut);
    }
}
