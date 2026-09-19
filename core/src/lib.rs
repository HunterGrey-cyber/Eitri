//! The GTK-free half of `shell`: the agent stack, theme token derivation, process and filesystem
//! plumbing, and the Lua kernel's pure half. No `gtk4`, `webkit6` or `glib` dependency belongs here.
//! `shell` depends on this crate; this crate must never depend back on `shell`.
//!
//! This crate requires Unix: `instance_dir` uses `std::os::unix::net` and, through
//! `agent::process_probe::pid_is_alive`, `libc::kill`. This permits Linux and macOS builds
//! without depending on a UI toolkit.

pub mod agent_backend;
pub mod agent_bridge;
pub mod buffer_reload;
pub mod editor_context;
pub mod hint;
pub mod instance_dir;
pub mod lua;
pub mod pane_switch;
pub mod project_root;
pub mod theme;
pub mod turn_trace;

/// The crate-wide guard that every socket path here is built through `agent::socket_path`.
/// Test-only, and in its own file so it can exclude itself from its own walk without excluding
/// `lib.rs` -- exactly how `agent::socket_path`'s original does it.
#[cfg(test)]
mod socket_path_guard;

/// The guard that this crate's own manifest never grows a GTK/WebKit dependency -- see its own
/// doc for why that is the actual point of this crate existing at all.
#[cfg(test)]
mod manifest_guard;
