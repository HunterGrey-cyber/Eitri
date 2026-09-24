//! The window's colours come from the embedded nvim's highlight groups.
//! nvim sends a snapshot through `nvim_theme.lua` and `feed`; token derivation produces
//! a complete set used by `gtk_css` for the chrome and by the agent panel's WebView.
//! No separate palette is supplied.
//!
//! Token derivation and the socket protocol live in the core without GTK dependencies.
//! This crate keeps the GTK CSS provider and the `glib` timer that polls the feed.

pub(crate) mod feed;
pub(crate) mod gtk_css;
pub(crate) mod restyle;
