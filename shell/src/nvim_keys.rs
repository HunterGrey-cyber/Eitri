//! The GTK half of the nvim keys feed (panel round 2 plan Task 6, spec §3): a `glib` timer that
//! drains `eitri_core::nvim_keys::feed`'s socket and hands each new report to `main.rs`.
//!
//! Deliberately simpler than `shell::editor_context`, which caches the latest value for the panel
//! to read lazily: here the caller (`main.rs`'s `send_keymap`) wants to react the moment a report
//! arrives, not to poll a cache at send time, so this takes a callback instead of returning a
//! source.

use gtk4::glib;

use eitri_core::nvim_keys::feed::{NvimKeysFeed, NvimKeysReader, POLL_INTERVAL};
use eitri_core::nvim_keys::NvimReport;

/// Starts draining the feed, calling `on_report` for every new report the reader returns.
///
/// Takes the feed's listener, so a second call logs and installs no second timer -- a second
/// timer would race the first for every connection nvim makes to this socket.
pub(crate) fn listen(feed: &mut NvimKeysFeed, on_report: impl Fn(NvimReport) + 'static) {
    let Some(listener) = feed.take_listener() else {
        eprintln!("[nvim-keys] listen() called twice -- the panel keeps its default keys");
        return;
    };
    let mut reader = NvimKeysReader::new(listener);
    glib::timeout_add_local(POLL_INTERVAL, move || {
        if let Some(report) = reader.poll() {
            on_report(report);
        }
        glib::ControlFlow::Continue
    });
}
