//! `eitri-panel`: the agent panel's logic with no toolkit in it.
//!
//! A host supplies the page and the process (`shell` does it with WebKitGTK and GTK on Linux) and drives
//! this crate; it must never name a toolkit: `src/manifest_guard.rs` fails on a GTK/WebKit manifest entry.

pub mod agent_panel;
pub mod panel_csp;
pub mod panel_document;
pub mod panel_pacer;
pub mod panel_page;
pub mod review_editor;
pub mod supervisor_client;
pub mod terminal_handoff;
pub mod trust_gate;
pub mod webview_crash_guard;

#[cfg(test)]
mod manifest_guard;
