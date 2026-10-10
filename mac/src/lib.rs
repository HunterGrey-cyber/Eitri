//! The macOS host of Eitri: Neovide's own window with the agent panel beside the editor.
//!
//! The modules here carry no `cfg(target_os)`: they build and are tested on every OS, so the Linux
//! workstation exercises everything but the AppKit calls. Those are `appkit`, compiled on macOS only.

#[cfg(target_os = "macos")]
pub mod appkit;
pub mod assembly;
pub mod editor;
pub mod keys;
pub mod layout;
/// Lines on stdout and stderr that never panic on a stream nobody reads.
pub mod log;
pub mod panel_pump;
pub mod quit;
pub mod startup;
pub mod user_env;

/// Every socket path built here goes through `agent::socket_path::in_dir`; the core and agent guards read
/// only their own crates' sources.
#[cfg(test)]
mod socket_path_guard;

/// The guard that this crate's manifest never grows a GTK/WebKit dependency.
#[cfg(test)]
mod manifest_guard;

/// Tests of one process share its descriptor table, and a child forked by one test holds a copy of every
/// descriptor another test has open until it execs, including the lock of a project log. A test that asks a log
/// for the exclusive lock after holding it takes [`quiet`](spawn_gate::quiet); anything that spawns a child takes
/// [`spawning`](spawn_gate::spawning) across the spawn, so the two are never in flight together.
#[cfg(test)]
pub(crate) mod spawn_gate {
    use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

    static GATE: RwLock<()> = RwLock::new(());

    pub(crate) fn quiet() -> RwLockReadGuard<'static, ()> {
        GATE.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn spawning() -> RwLockWriteGuard<'static, ()> {
        GATE.write().unwrap_or_else(PoisonError::into_inner)
    }
}
