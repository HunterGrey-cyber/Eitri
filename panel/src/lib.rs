//! `eitri-panel`: the agent panel's logic with no toolkit in it.
//!
//! A host supplies the page and the process (`shell` does it with WebKitGTK and GTK on Linux) and drives
//! this crate; it must never name a toolkit: `src/manifest_guard.rs` fails on a GTK/WebKit manifest entry.

pub mod agent_panel;
pub mod agents_glue;
pub mod keys_help;
pub mod panel_csp;
pub mod panel_document;
pub mod panel_pacer;
pub mod panel_page;
pub mod panel_wake;
pub mod review_editor;
pub mod startup_config;
pub mod terminal_handoff;
pub mod trust_gate;
pub mod webview_crash_guard;
pub mod window_config;

#[cfg(test)]
mod manifest_guard;

#[cfg(test)]
#[global_allocator]
static COUNTING: eitri_core::alloc_count::Counting = eitri_core::alloc_count::Counting;
