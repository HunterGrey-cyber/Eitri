//! The AppKit half of the Mac host: what only macOS can do, built on the host-neutral modules beside it.
//!
//! - [`app`]: `main`'s order, waiting for the editor, the question before a quit, and the shutdown.
//! - [`page`]: the agent panel's page in a WKWebView, the `PageSurface` and `PanelHost` the panel drives,
//!   and the run-loop timer that polls the editor.
//! - [`tick`]: the panel's tick, run when its wake is raised or its next deadline comes.
//! - [`window`]: the window's host state, pane focus, and the embedder Neovide calls.
//! - [`keys`]: the key monitor.
//! - [`menu`]: the Edit menu.
//! - `acceptance` (the `acceptance` feature only): a diagnostic socket for the acceptance driver.

#[cfg(feature = "acceptance")]
pub mod acceptance;
pub mod app;
pub mod keys;
pub mod menu;
pub mod page;
pub mod tick;
pub mod window;
