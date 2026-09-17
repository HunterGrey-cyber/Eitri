//! The window's colours, derived from the embedded nvim's own highlight groups.
//!
//! nvim pushes a snapshot (`nvim_theme.lua` → `feed`), `neovibe_core::theme::tokens` turns it into
//! a complete token set, and two consumers paint with it: `gtk_css` (the chrome) and the agent
//! panel's WebView (through `neovibe_core::agent_bridge::serialize_theme_for_js`). neovibe ships
//! no palette of its own. See `docs/superpowers/specs/2026-09-16-neovibe-ui-design.md` §1.
//!
//! Token derivation itself (`color`/`payload`/`tokens`) moved to `neovibe_core::theme` (L2, see
//! `docs/superpowers/specs/2026-09-16-macos-path-design.md`) -- it has no GTK dependency, unlike
//! `gtk_css`, and unlike `feed`'s `glib` timer, which is all of `feed` that stayed here. The socket
//! protocol and `nvim_theme.lua` went to `neovibe_core::theme::feed` in L2 T5: this sentence used to
//! say `feed` stayed for want of portability, which its protocol half then disproved.

pub(crate) mod feed;
pub(crate) mod gtk_css;
