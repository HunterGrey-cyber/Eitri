//! Points inside a collection and a recovery where a test runs code of its own, to put another
//! window's git run in the one moment a race needs it. Each is set on the thread that will reach
//! it, runs there once and is gone; nothing of this is built outside the crate's own tests.

use std::cell::RefCell;
use std::thread::LocalKey;

/// A hook: run once, on the thread it was set on.
pub(crate) type Hook = RefCell<Option<Box<dyn FnOnce()>>>;

thread_local! {
    pub(crate) static BEFORE_CURRENT_READ: Hook = const { RefCell::new(None) };
    /// In a live drawing's worker, after storing the disk bytes and before diffing them.
    pub(crate) static AFTER_LIVE_STORE: Hook = const { RefCell::new(None) };
    /// In [`Shadow::trim`](super::shadow::Shadow::trim)'s second step, with the store lock held
    /// shared, right before its `git gc`.
    pub(crate) static BEFORE_GC: Hook = const { RefCell::new(None) };
    /// In [`Shadow::trim`](super::shadow::Shadow::trim), after its `git gc` and before it waits for
    /// the store lock exclusively to prune.
    pub(crate) static BEFORE_PRUNE: Hook = const { RefCell::new(None) };
    /// In a recovery's restore, right after the bytes the file holds now are stored and before the
    /// revert anchor that keeps them is written.
    pub(crate) static AFTER_RECOVERY_STORE: Hook = const { RefCell::new(None) };
}

/// Sets `hook` to run `then` the next time this thread reaches it.
pub(crate) fn set(hook: &'static LocalKey<Hook>, then: impl FnOnce() + 'static) {
    hook.with(|slot| *slot.borrow_mut() = Some(Box::new(then)));
}

/// Runs what `hook` was set to on this thread, if anything.
pub(crate) fn run(hook: &'static LocalKey<Hook>) {
    if let Some(then) = hook.with(|slot| slot.borrow_mut().take()) {
        then();
    }
}
