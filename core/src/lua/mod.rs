//! The embedded Lua extension kernel without a toolkit. `kernel::Kernel` is the whole runtime; the
//! host passes it the installer for `eitri.panel.register` (`shell` builds a WebKitGTK view,
//! `kernel::refuse_panels` is for a host with no Lua panels).
//!
//! `panel` here is only the pure validation/resolution half of `eitri.panel.register`.
//! `PanelEntry`, `PanelRegistry` and the real widget-constructing `install` stay in
//! `shell::lua::panel`, because `PanelEntry` holds a `gtk4::Widget`.

pub mod command;
pub mod config;
pub mod event;
pub mod kernel;
pub mod keymap;
pub mod layout;
pub mod panel;
