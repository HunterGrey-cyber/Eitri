//! The macOS host of Eitri: Neovide's own window with the agent panel beside the editor.
//!
//! The modules here carry no `cfg(target_os)`: they build and are tested on every OS, so the Linux
//! workstation exercises everything but the AppKit calls.

pub mod assembly;
pub mod editor;
pub mod keys;
pub mod layout;
pub mod startup;

/// Every socket path built here goes through `agent::socket_path::in_dir`; the core and agent guards read
/// only their own crates' sources.
#[cfg(test)]
mod socket_path_guard;

/// The guard that this crate's manifest never grows a GTK/WebKit dependency.
#[cfg(test)]
mod manifest_guard;
