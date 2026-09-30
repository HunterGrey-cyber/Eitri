//! The GTK-free pieces of `shell`'s embedded Lua extension kernel (see
//! docs/superpowers/specs/2026-09-16-macos-path-design.md, L2). `shell::lua::LuaEngine` cannot
//! move here yet -- constructing it installs the panel-registration closure, which builds a real
//! `webkit6::WebView` -- so `shell`'s own `lua` module stays the caller of everything below.
//!
//! `panel` here is only the pure validation/resolution half of `eitri.panel.register`.
//! `PanelEntry`, `PanelRegistry` and the real widget-constructing `install` stay in
//! `shell::lua::panel`, because `PanelEntry` holds a `gtk4::Widget`.

pub mod command;
pub mod config;
pub mod event;
pub mod keymap;
pub mod layout;
pub mod panel;
