//! `eitri-core`: the GTK-free half of `shell` -- the agent stack, theme token derivation, the
//! process/filesystem plumbing under them, and the Lua kernel's pure half -- extracted so the
//! compiler enforces the boundary (no `gtk4`/`webkit6`/`glib` dependency here, so a macOS host
//! can build on it). `shell` depends on this crate;
//! this crate must never depend back on `shell`.
//!
//! **Not "platform-independent".** `instance_dir` uses `std::os::unix::net` and, transitively
//! (through `agent::process_probe::pid_is_alive`), `libc::kill` -- this crate does not compile off
//! unix. That is expected, not a regression: unix (Linux and macOS both) is what the macOS port
//! actually needs, and the property this crate promises is toolkit independence, not host
//! independence.

pub mod agent_backend;
pub mod agent_bridge;
pub mod agent_prefs;
pub mod agents;
pub mod attention;
pub mod buffer_reload;
pub mod companion;
pub mod compositor;
pub mod config_dir;
pub mod config_switch;
pub mod crash_trace;
pub mod display_env;
pub mod editor_context;
pub mod editor_feeds;
pub mod editor_lines;
pub mod editor_open;
pub mod editor_record;
pub mod editor_reopen;
pub mod editor_reopen_start;
pub mod editor_rpc;
pub mod editor_tools;
pub mod hint;
pub mod host_clipboard;
#[cfg(target_os = "linux")]
pub mod ide_server;
pub mod init_notice;
pub mod instance_dir;
pub mod keymap;
pub mod layout;
pub mod line_feed;
pub mod lua;
pub mod mentions;
pub mod nvim_bin;
pub mod nvim_keys;
pub mod nvim_rpc;
pub mod nvim_verbs;
pub mod onboarding;
pub mod open_socket;
pub mod pane_switch;
pub mod panel_cadence;
pub mod panel_control;
pub mod path_complete;
pub mod permission_store;
pub mod plugin;
pub mod proc_chain;
pub mod project_chooser;
pub mod project_root;
pub mod project_trust;
pub mod prompt_history;
pub mod recent_projects;
pub mod review_editor;
#[cfg(target_os = "linux")]
pub mod run_guard;
pub mod saved_tabs;
pub mod scratch;
pub mod shell_line;
pub mod split;
pub mod state_paths;
pub mod tab_restore;
pub mod tab_set;
pub mod tabs;
pub mod takeover;
pub mod theme;
pub mod transcript_titles;
pub mod turn_review;
pub mod turn_trace;
pub mod tutor;
pub mod window_link;
pub mod wm;

/// Test doubles shared across this crate's tests, and with `shell`'s through the `test-support`
/// feature.
#[cfg(any(test, feature = "test-support"))]
pub mod test_providers;

/// A counting allocator, for the tests that pin that a path does not copy.
#[cfg(any(test, feature = "test-support"))]
pub mod alloc_count;

#[cfg(test)]
#[global_allocator]
static COUNTING: alloc_count::Counting = alloc_count::Counting;

/// Reading the workspace's own sources as text, for the tests that hold an invariant by scanning code,
/// here and in `shell`'s and `eitri-panel`'s tests.
#[cfg(any(test, feature = "test-support"))]
pub mod source_scan;

/// Where the user guide is, public tree or private, for the tests that read it.
#[cfg(test)]
mod guide_files;

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
