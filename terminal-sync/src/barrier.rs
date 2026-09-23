//! `SyncBarrier` -- the publication gate.

/// Why a snapshot is being published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishReason {
    /// An ESU (`CSI ? 2026 l`) closed a synchronized update that had actually mutated `Term`.
    /// Published at the exact byte that dispatched the ESU, not at the end of the PTY read.
    FrameComplete,
    /// End of a PTY read (a wakeup) with pending mutations and no open synchronized update.
    Wakeup,
    /// The engine gave up on an unterminated BSU via [`SyncBarrier::abort_sync`].
    SyncAborted,
}

/// Decides WHEN a presentation snapshot may be published. It never touches `Term`.
///
/// Invariants:
/// * `dirty` is set only by an actual `vte::ansi::Handler` DISPATCH, never by byte arrival.
/// * The two DECSET/DECRST-2026 dispatches do not count as mutations, so a bare
///   `BSU ... ESU` with nothing between it publishes nothing.
/// * An ESU that closes a dirty update latches `frame_ready`, which is drained at the ESU byte
///   itself. That is what stops a read containing `ESU(n) BSU(n+1)` from dropping frame `n`:
///   the naive `if !in_sync && dirty` test at end-of-read would see `in_sync == true` again
///   and frame `n` would never be published.
#[derive(Debug, Default)]
pub struct SyncBarrier {
    in_sync: bool,
    dirty: bool,
    frame_ready: bool,
    dispatches: u64,
    bsu_count: u64,
    esu_count: u64,
}

impl SyncBarrier {
    pub fn new() -> Self {
        Self::default()
    }

    // ----- signals from SyncSpy ---------------------------------------------------------------

    /// An ordinary Handler dispatch reached the inner handler.
    #[inline]
    pub fn note_dispatch(&mut self) {
        self.dispatches += 1;
        self.dirty = true;
    }

    /// `CSI ? 2026 h` was dispatched.
    #[inline]
    pub fn begin_sync(&mut self) {
        self.dispatches += 1;
        self.bsu_count += 1;
        // A BSU while already in an update is idempotent. vte does the same (it just resets its
        // timeout); the 2026 spec leaves nesting undefined.
        self.in_sync = true;
    }

    /// `CSI ? 2026 l` was dispatched.
    #[inline]
    pub fn end_sync(&mut self) {
        self.dispatches += 1;
        self.esu_count += 1;
        if self.in_sync {
            self.in_sync = false;
            if self.dirty {
                self.frame_ready = true;
            }
        }
        // An ESU with no matching BSU changes nothing: whatever is dirty will be published by
        // the ordinary end-of-read wakeup.
    }

    // ----- decisions consumed by the driver ---------------------------------------------------

    /// Drain a completed synchronized frame. Must be polled after every byte fed to the parser,
    /// so publication lands exactly at the ESU and not at the end of the read.
    #[inline]
    pub fn take_frame_publication(&mut self) -> bool {
        if self.frame_ready {
            self.frame_ready = false;
            self.dirty = false;
            true
        } else {
            false
        }
    }

    /// Drain an end-of-read (wakeup) publication.
    #[inline]
    pub fn take_wakeup_publication(&mut self) -> bool {
        if !self.in_sync && self.dirty {
            self.dirty = false;
            true
        } else {
            false
        }
    }

    // ----- unterminated BSU -------------------------------------------------------------------

    /// Force the open synchronized update closed and let its mutations be published.
    ///
    /// UNTERMINATED BSU, DECIDED AND DOCUMENTED. We ship NO default deadline, because:
    ///
    /// * The DEC 2026 specification mandates no timeout.
    /// * vte's `Processor` does not self-enforce one either: `pending_timeout()` on the stock
    ///   `StdSyncHandler` is `self.timeout.is_some()` and never checks expiry
    ///   (`vte-0.15.0/src/ansi.rs:469`).
    /// * Upstream alacritty's 150 ms is an APPLICATION-event-loop policy, not a parser rule:
    ///   `alacritty_terminal/src/event_loop.rs:229-247` turns `sync_timeout()` into a poll
    ///   timeout and calls `Processor::stop_sync` when the poll times out.
    /// * Under [`crate::NeverBuffer`] there is nothing buffered to flush, so an unterminated BSU
    ///   does NOT freeze `Term`; it only suspends PUBLICATION. That is a strictly milder failure
    ///   than upstream's (upstream freezes the visible screen).
    ///
    /// So the policy lives where upstream puts it -- in the engine's event loop -- and this is
    /// the hook for it. Returns `true` if a publication is now due.
    pub fn abort_sync(&mut self) -> bool {
        if self.in_sync {
            self.in_sync = false;
        }
        if self.dirty {
            self.dirty = false;
            true
        } else {
            false
        }
    }

    // ----- observation ------------------------------------------------------------------------

    #[inline]
    pub fn in_sync(&self) -> bool {
        self.in_sync
    }

    /// Pending, unpublished mutations.
    #[inline]
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Total Handler dispatches observed, including the two 2026 mode toggles.
    #[inline]
    pub fn dispatch_count(&self) -> u64 {
        self.dispatches
    }

    #[inline]
    pub fn bsu_count(&self) -> u64 {
        self.bsu_count
    }

    #[inline]
    pub fn esu_count(&self) -> u64 {
        self.esu_count
    }
}
