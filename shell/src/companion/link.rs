//! The GTK half of the companion's attach: one `glib` timer that polls the link driver
//! (`eitri_core::companion::driver`), which owns the connect, the install and every decision. The
//! driver does its connecting on worker threads and only ever tries an answer, so nothing here
//! blocks the main loop.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::glib;
use rmpv::Value;

use eitri_core::companion::attach::{BandLink, LinkState};
use eitri_core::companion::driver::LinkDriver;
use eitri_core::companion::Sockets;

const POLL_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) struct CompanionLink {
    driver: RefCell<LinkDriver>,
}

impl CompanionLink {
    /// Starts following `initial` (an nvim address) or nothing. `on_change` is called once at once
    /// with the starting state, and again whenever the state changes, with the state, the band and
    /// the pid of the process holding the editor's socket ([`CompanionLink::nvim_pid`]); `on_cancel_drafts` when the
    /// editor went away or was swapped and the drafts waiting on it must end. Both run with no
    /// borrow of the driver held, so they may call back into the link.
    pub(crate) fn start(
        initial: Option<PathBuf>,
        sockets: Sockets,
        on_change: impl Fn(&LinkState, BandLink, Option<u32>) + 'static,
        on_cancel_drafts: impl Fn() + 'static,
    ) -> Rc<CompanionLink> {
        let link = Rc::new(CompanionLink {
            driver: RefCell::new(LinkDriver::new(initial, sockets)),
        });
        {
            let (state, band, peer) = link.snapshot();
            on_change(&state, band, peer);
        }
        let weak = Rc::downgrade(&link);
        glib::timeout_add_local(POLL_INTERVAL, move || {
            let Some(link) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let tick = link.driver.borrow_mut().poll(Instant::now());
            for line in &tick.logs {
                println!("{line}");
            }
            if tick.state_changed {
                let (state, band, peer) = link.snapshot();
                on_change(&state, band, peer);
            }
            if tick.cancel_drafts {
                on_cancel_drafts();
            }
            if link.driver.borrow().is_shut() {
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
        link
    }

    fn snapshot(&self) -> (LinkState, BandLink, Option<u32>) {
        let driver = self.driver.borrow();
        (driver.state().clone(), driver.band(), driver.peer_pid())
    }

    /// Aim the panel at this nvim, leaving the one it already follows as it is; the change shows at
    /// the next poll.
    pub(crate) fn attach(&self, addr: PathBuf) {
        self.driver.borrow().attach(addr);
    }

    /// Run Lua in the attached nvim without waiting for its answer. `Err` says why not.
    #[allow(dead_code)] // `exec_lua_for` is the one the window uses; this is its plain form
    pub(crate) fn exec_lua(&self, code: &str, args: Vec<Value>) -> Result<(), String> {
        self.driver.borrow_mut().exec_lua(code, args)
    }

    /// [`CompanionLink::exec_lua`] for code that needs the install's part `part`.
    pub(crate) fn exec_lua_for(&self, part: &str, code: &str, args: Vec<Value>) -> Result<(), String> {
        self.driver.borrow_mut().exec_lua_for(Some(part), code, args)
    }

    /// The editor's pid for finding its window: the process holding the socket, as the kernel says,
    /// never the pid the editor reported (any process that answers the install can report any pid).
    /// `None` unless attached, and where the platform gives no pid.
    pub(crate) fn nvim_pid(&self) -> Option<u32> {
        self.driver.borrow().peer_pid()
    }

    /// [`CompanionLink::nvim_pid`], when the link is attached to `addr` itself.
    pub(crate) fn attached_to(&self, addr: &std::path::Path) -> Option<u32> {
        let driver = self.driver.borrow();
        match driver.state() {
            LinkState::Attached { addr: attached, .. } if attached == addr => driver.peer_pid(),
            _ => None,
        }
    }

    /// Lets go of the editor: the teardown is queued, then the connection closes once it is
    /// written. Never waits.
    pub(crate) fn shutdown(&self) {
        self.driver.borrow_mut().shutdown();
    }
}
