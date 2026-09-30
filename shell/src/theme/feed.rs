//! The GTK half of the theme feed: a `glib` timer on the main loop that drains the socket
//! `eitri_core::theme::feed` owns.
//!
//! Everything else -- binding the socket, writing `nvim_theme.lua`, accepting, reading, parsing,
//! the cleanup, and both halves of the Lua↔Rust contract -- moved into `eitri-core` in L2 T5
//! (2026-09-17). What is left here is exactly the part that names `gtk4`, which is what made the
//! rest unportable: see `docs/superpowers/specs/2026-09-16-macos-path-design.md`, L2, "协议搬进核
//! 心，轮询驱动留在壳里".

use gtk4::glib;

pub(crate) use eitri_core::theme::feed::ThemeFeed;
use eitri_core::theme::feed::{ThemePayloadReader, POLL_INTERVAL};
use eitri_core::theme::payload::NvimThemePayload;

/// Polls the feed's socket on the GTK main loop and calls `on_payload` with the newest valid
/// payload each tick. `VimEnter` and `ColorScheme` often fire back to back; only the last one
/// matters. `ThemePayloadReader` never blocks this thread (sw-theme-1): a stalled or oversized
/// sender is bounded rather than read on a blocking socket.
///
/// Takes the feed's listener, so a second call on the same feed logs and does nothing rather than
/// installing a second timer that would race the first for every connection.
pub(crate) fn listen(feed: &mut ThemeFeed, on_payload: impl Fn(NvimThemePayload) + 'static) {
    let Some(listener) = feed.take_listener() else {
        eprintln!("[theme] listen() called twice -- ignoring");
        return;
    };
    let mut reader = ThemePayloadReader::new(listener);
    glib::timeout_add_local(POLL_INTERVAL, move || {
        if let Some(payload) = reader.poll() {
            on_payload(payload);
        }
        glib::ControlFlow::Continue
    });
}
