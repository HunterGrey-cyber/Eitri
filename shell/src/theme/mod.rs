//! The window's colours, derived from the embedded nvim's own highlight groups.
//!
//! nvim pushes a snapshot (`nvim_theme.lua` → `feed`), `tokens` turns it into a complete token set,
//! and two consumers paint with it: `gtk_css` (the chrome) and the agent panel's WebView (through
//! `agent_bridge::serialize_theme_for_js`). neovibe ships no palette of its own. See
//! `docs/superpowers/specs/2026-09-16-neovibe-ui-design.md` §1.

pub(crate) mod color;
pub(crate) mod feed;
pub(crate) mod gtk_css;
pub(crate) mod payload;
pub(crate) mod tokens;

pub(crate) use tokens::ThemeTokens;
