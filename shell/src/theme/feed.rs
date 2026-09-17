//! The GTK half of the theme feed: a `glib` timer on the main loop that drains the socket
//! `neovibe_core::theme::feed` owns.
//!
//! Everything else -- binding the socket, writing `nvim_theme.lua`, accepting, reading, parsing,
//! the cleanup, and both halves of the Lua↔Rust contract -- moved into `neovibe-core` in L2 T5
//! (2026-09-17). What is left here is exactly the part that names `gtk4`, which is what made the
//! rest unportable: see `docs/superpowers/specs/2026-09-16-macos-path-design.md`, L2, "协议搬进核
//! 心，轮询驱动留在壳里".

use gtk4::glib;

pub(crate) use neovibe_core::theme::feed::ThemeFeed;
use neovibe_core::theme::feed::{accept_pending_lines, latest_payload, POLL_INTERVAL};
use neovibe_core::theme::payload::NvimThemePayload;

/// Polls the feed's socket on the GTK main loop and calls `on_payload` with the newest valid
/// payload each tick. `VimEnter` and `ColorScheme` often fire back to back; only the last one
/// matters.
///
/// Takes the feed's listener, so a second call on the same feed logs and does nothing rather than
/// installing a second timer that would race the first for every connection.
pub(crate) fn listen(feed: &mut ThemeFeed, on_payload: impl Fn(NvimThemePayload) + 'static) {
    let Some(listener) = feed.take_listener() else {
        eprintln!("[theme] listen() called twice -- ignoring");
        return;
    };
    glib::timeout_add_local(POLL_INTERVAL, move || {
        if let Some(payload) = latest_payload(accept_pending_lines(&listener)) {
            on_payload(payload);
        }
        glib::ControlFlow::Continue
    });
}
