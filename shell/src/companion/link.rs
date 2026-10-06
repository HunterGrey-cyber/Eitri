//! The GTK half of the companion's attach: one `glib` timer that polls the link
//! (`eitri_core::companion::link`), which owns the driver, the connect, the install and every
//! decision. Nothing here blocks the main loop.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use gtk4::glib;

use eitri_core::companion::attach::{BandLink, LinkState};
pub(crate) use eitri_core::companion::link::CompanionLink;
use eitri_core::companion::Sockets;

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
    let link = Rc::new(CompanionLink::new(initial, sockets));
    {
        let (state, band, peer) = link.snapshot();
        on_change(&state, band, peer);
    }
    let weak = Rc::downgrade(&link);
    glib::timeout_add_local(CompanionLink::POLL_INTERVAL, move || {
        let Some(link) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let poll = link.poll(Instant::now());
        for line in &poll.logs {
            println!("{line}");
        }
        if let Some((state, band, peer)) = poll.changed {
            on_change(&state, band, peer);
        }
        if poll.cancel_drafts {
            on_cancel_drafts();
        }
        if poll.shut {
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
    link
}
