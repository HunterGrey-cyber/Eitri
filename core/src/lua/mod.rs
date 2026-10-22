//! The embedded Lua extension kernel without a toolkit. `kernel::Kernel` is the whole runtime; the
//! host passes it the installer for `eitri.panel.register` (`shell` builds a WebKitGTK view,
//! `kernel::refuse_panels` is for a host with no Lua panels).
//!
//! `panel` owns registration validation, URL resolution and the widgetless `PanelEntry` /
//! `PanelRegistry`. The shell owns `PanelViews`, its widgets and their crash guards, and the
//! widget-constructing installer.

pub mod agent;
pub mod command;
pub mod config;
pub mod event;
pub mod kernel;
pub mod keymap;
pub mod layout;
pub mod panel;
pub mod plugin;
