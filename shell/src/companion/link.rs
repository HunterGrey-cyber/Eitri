//! The GTK half of the companion's attach: one `glib` timer that polls the link
//! (`eitri_core::companion::link`), which owns the driver, the connect, the install and every
//! decision. Nothing here blocks the main loop.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use gtk4::glib;

use eitri_core::companion::attach::{BandLink, LinkState};
pub(crate) use eitri_core::companion::link::CompanionLink;
use eitri_core::companion::{InstallKind, Sockets};

/// Starts following `initial` (an nvim address) or nothing. `on_change` is called once at once
/// with the starting state, and again whenever the state changes or a connection begins, with the link, state, band,
/// the pid of the process holding the editor's socket ([`CompanionLink::nvim_pid`]) and whether a connect began
/// (false for the starting state); `on_cancel_drafts` when the
/// editor went away or was swapped and the drafts waiting on it must end. Both run with no
/// borrow of the driver held, so they may call back into the link.
pub(crate) fn start(
    initial: Option<PathBuf>,
    sockets: Sockets,
    kind: InstallKind,
    on_change: impl Fn(&CompanionLink, &LinkState, BandLink, Option<u32>, bool) + 'static,
    on_cancel_drafts: impl Fn() + 'static,
) -> Rc<CompanionLink> {
    let link = Rc::new(CompanionLink::new(initial, sockets, kind));
    announce_initial(&link, &on_change);
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
            on_change(&link, &state, band, peer, poll.connect_began);
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

fn announce_initial(
    link: &CompanionLink,
    on_change: &impl Fn(&CompanionLink, &LinkState, BandLink, Option<u32>, bool),
) {
    let (state, band, peer) = link.snapshot();
    on_change(link, &state, band, peer, false);
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::companion::{feed_line, FeedGeneration};
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn initial_connecting_accepts_context_before_the_first_link_poll() {
        let generation: FeedGeneration = std::sync::Arc::new(AtomicU64::new(0));
        let link = CompanionLink::new(
            Some("/not-opened-before-poll".into()),
            Sockets::default(),
            InstallKind::Agent,
        );
        let published = std::cell::Cell::new(false);
        announce_initial(&link, &|link, state, _, _, _| {
            assert!(
                !published.get(),
                "the callback precedes publication into the window's link slot"
            );
            assert!(matches!(state, LinkState::Connecting { .. }));
            generation.store(link.feed_generation().unwrap_or(0), Ordering::Relaxed);
        });
        published.set(true);
        let stamped = format!(
            "G {} {{\"v\":1,\"file\":\"/first.rs\"}}",
            link.feed_generation().unwrap()
        );
        let context = eitri_core::editor_context::feed::latest_context(vec![feed_line(&stamped, Some(&generation))
            .expect("initial stamp accepted")
            .to_owned()])
        .unwrap();
        assert_eq!(context.file, "/first.rs");
        // The window callback must use the announced link even before its weak slot is filled.
        use eitri_core::source_scan::{braced, code_only, read_required};
        let source = read_required("shell/src/companion/window.rs").text;
        let code: String = code_only(&source).chars().filter(|c| !c.is_whitespace()).collect();
        let callback = "move|link,state,band,peer_pid,connect_began|";
        let at = code.find(callback).expect("the callback receives the announced link");
        let body = braced(&code, at + callback.len());
        assert!(body.contains("letgeneration=link.feed_generation().unwrap_or(0);"));
    }
}
