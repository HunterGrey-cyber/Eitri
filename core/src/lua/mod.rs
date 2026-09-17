//! The GTK-free pieces of `shell`'s embedded Lua extension kernel.
//! `shell::lua::LuaEngine` stays in `shell` because its panel-registration closure constructs a
//! `webkit6::WebView`. `shell`'s `lua` module calls the pure pieces here.
//!
//! `panel` contains only panel registration's validation and resolution.
//! `PanelEntry`, `PanelRegistry` and the widget-constructing `install` stay in `shell::lua::panel`
//! because `PanelEntry` holds a `gtk4::Widget`.

pub mod command;
pub mod config;
pub mod event;
pub mod panel;
