//! The toolkit-free half of a desktop window: what a host does between the controller's decisions and the toolkit's
//! calls, written once for every host.
//!
//! This crate sits above `eitri-panel` and below the hosts (`shell` on Linux, `eitri-mac`). It never depends on
//! `shell`, `eitri-mac` or a toolkit, and `eitri-core`, `eitri-panel`, `eitri-remote` and `eitri-terminal` never depend
//! on it. The Mac CI job builds and tests it, so "neutral" is proved on a Mac. A module that cannot build there yet
//! is declared for Linux only, in its own file named in `core/tests/linux_only_modules_scan.rs`, never inline.
//!
//! The crate holds nothing yet but its guards; each lands before the code they cover.

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
