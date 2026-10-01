//! `eitri-core`: the GTK-free half of `shell` -- the agent stack, theme token derivation, the
//! process/filesystem plumbing under them, and the Lua kernel's pure half -- extracted so the
//! compiler enforces the boundary (no `gtk4`/`webkit6`/`glib` dependency here -- see
//! docs/superpowers/specs/2026-09-16-macos-path-design.md, L2). `shell` depends on this crate;
//! this crate must never depend back on `shell`.
//!
//! **Not "platform-independent".** `instance_dir` uses `std::os::unix::net` and, transitively
//! (through `agent::process_probe::pid_is_alive`), `libc::kill` -- this crate does not compile off
//! unix. That is expected, not a regression: unix (Linux and macOS both) is what L2's macOS port
//! actually needs, and the property this crate promises is toolkit independence, not host
//! independence.

pub mod agent_backend;
pub mod agent_bridge;
pub mod agent_prefs;
pub mod attention;
pub mod buffer_reload;
pub mod editor_context;
pub mod hint;
pub mod instance_dir;
pub mod keymap;
pub mod layout;
pub mod line_feed;
pub mod lua;
pub mod nvim_bin;
pub mod nvim_keys;
pub mod pane_switch;
pub mod panel_cadence;
pub mod permission_store;
pub mod project_root;
pub mod prompt_history;
pub mod saved_tabs;
pub mod scratch;
pub mod tab_restore;
pub mod tab_set;
pub mod tabs;
pub mod theme;
pub mod turn_trace;

/// Test doubles shared across this crate's tests, and with `shell`'s through the `test-support`
/// feature.
#[cfg(any(test, feature = "test-support"))]
pub mod test_providers;

/// The crate-wide guard that every socket path here is built through `agent::socket_path`.
/// Test-only, and in its own file so it can exclude itself from its own walk without excluding
/// `lib.rs` -- exactly how `agent::socket_path`'s original does it.
#[cfg(test)]
mod socket_path_guard;

/// The guard that this crate's own manifest never grows a GTK/WebKit dependency -- see its own
/// doc for why that is the actual point of this crate existing at all.
#[cfg(test)]
mod manifest_guard;

/// Shared `#[cfg(test)]` RAII guard for a throwaway `/tmp` directory -- see its own doc.
#[cfg(test)]
mod test_scratch_dir;
