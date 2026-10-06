//! The theme feed lives in `eitri_core::theme::feed`: binding the socket, writing `nvim_theme.lua`,
//! accepting, reading, parsing, the cleanup and both halves of the Lua-to-Rust contract. It is read by
//! `eitri_core::editor_feeds` and driven by `shell::editor_feeds::FeedPump`, so nothing here names
//! `gtk4`; this module only gives the window the feed's type under the path it has always used.

pub(crate) use eitri_core::theme::feed::ThemeFeed;
