//! The theme, minus its toolkit: the socket protocol nvim pushes highlight groups over (`feed`,
//! which owns `nvim_theme.lua` too) and the derivation that turns them into a complete
//! [`ThemeTokens`] set (`payload`, `color`, `tokens`). GTK-free by construction -- `shell`'s
//! GTK-specific consumers stay in `shell::theme` and depend on this module rather than the other
//! way around: `gtk_css` paints the chrome, and `shell::theme::feed` is the `glib` timer that
//! drives `feed`'s socket.

pub mod color;
pub mod feed;
pub mod payload;
pub mod tokens;

pub use tokens::{ThemeTokens, DEFAULT_PANEL_FONT_SIZE_PX, PANEL_FONT_SIZE_RANGE_PX};
