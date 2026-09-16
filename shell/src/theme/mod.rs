//! The window's colours come from the embedded nvim's highlight groups.
//! nvim sends a snapshot through `nvim_theme.lua` and `feed`; `tokens` produces a
//! complete set used by `gtk_css` for the chrome and by the agent panel's WebView
//! through `agent_bridge::serialize_theme_for_js`. No separate palette is supplied.

pub(crate) mod color;
pub(crate) mod feed;
pub(crate) mod gtk_css;
pub(crate) mod payload;
pub(crate) mod tokens;

pub(crate) use tokens::ThemeTokens;
