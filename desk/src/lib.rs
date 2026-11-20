//! The toolkit-free half of a desktop window: what a host does between the controller's decisions and the toolkit's
//! calls, written once for every host.
//!
//! This crate sits above `eitri-panel` and below the hosts (`shell` on Linux, `eitri-mac`). It never depends on
//! `shell`, `eitri-mac` or a toolkit, and `eitri-core`, `eitri-panel`, `eitri-remote` and `eitri-terminal` never depend
//! on it. The Mac CI job builds and tests it, so "neutral" is proved on a Mac. A module that cannot build there yet
//! is declared for Linux only, in its own file named in `core/tests/linux_only_modules_scan.rs`, never inline.
//!
//! It holds the linked windows (`linked`, Linux only for now) with the host traits they are written against
//! (`host_traits`), the op router that carries the controller's ops out through those traits (`router`), the
//! module instance pool, the wiring plan of a module's host, the panel's hook registration, the fit of the prefix's
//! hint panel and the scheduler's book of armed sources, and the guards that cover them.

/// What the linked windows' code and the op router ask of the host that carries them.
pub mod host_traits;

/// The op router: every op the window controller emits is carried out through the host's traits.
pub mod router;

/// Live and idle module instances, kept by the id they were made for.
pub mod instance_pool;

/// Which controllers each kind of module host gets, as a value.
pub mod wire_plan;

/// Every hook a window hands its agent panel, registered by one function.
pub mod panel_wiring;

/// What of the prefix's hint panel fits the window, decided from measures a toolkit gives.
pub mod prefix_fit;

/// A scheduler's book of armed sources, one per token of the window contract.
pub mod source_book;

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
