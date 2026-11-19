//! The toolkit-free half of a desktop window: what a host does between the controller's decisions and the toolkit's
//! calls, written once for every host.
//!
//! This crate sits above `eitri-panel` and below the hosts (`shell` on Linux, `eitri-mac`). It never depends on
//! `shell`, `eitri-mac` or a toolkit, and `eitri-core`, `eitri-panel`, `eitri-remote` and `eitri-terminal` never depend
//! on it. The Mac CI job builds and tests it, so "neutral" is proved on a Mac. A module that cannot build there yet
//! is declared for Linux only, in its own file named in `core/tests/linux_only_modules_scan.rs`, never inline.
//!
//! It holds the linked windows (`linked`, Linux only for now) with the host traits they are written against
//! (`host_traits`), and the guards that cover them.

/// What the linked windows' code asks of the host that carries it.
pub mod host_traits;

/// The linked windows: the project's second window, the chat and page windows, and the link between them. Linux-only
/// as a whole, because `core::window_link::{fdpass, link, start}` and `agent::child` are Linux-only on purpose;
/// making the transport portable is later work.
// With `test-support` (another crate's tests) the test helpers are compiled without the tests that use them.
#[cfg_attr(all(not(test), feature = "test-support"), allow(dead_code, unused_imports))]
#[cfg(target_os = "linux")]
pub mod linked;

/// The crate's own `Cargo.toml` never grows a toolkit or host crate.
#[cfg(test)]
mod manifest_guard;

/// No other crate's manifest depends on this one.
#[cfg(test)]
mod direction_guard;

/// Every socket path built here goes through `agent::socket_path::in_dir`; the other crates' guards read only their
/// own sources.
#[cfg(test)]
mod socket_path_guard;

/// This crate's sources name no toolkit.
#[cfg(test)]
mod toolkit_names_guard;
