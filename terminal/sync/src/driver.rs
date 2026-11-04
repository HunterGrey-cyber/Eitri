//! `SyncDriver` -- owns the parser, feeds it, and reports publication points.

use alacritty_terminal::vte::ansi::{Handler, Processor, Timeout};

use crate::barrier::{PublishReason, SyncBarrier};
use crate::never_buffer::NeverBuffer;
use crate::spy::{SyncSpy, ZeroWidthTarget};

/// Drives one `vte::ansi::Processor` over an authoritative `Handler` (normally
/// `alacritty_terminal::Term`) and calls back at every publication point.
///
/// The engine owns this instead of `alacritty_terminal::event_loop::EventLoop` because that
/// loop advances `Term` with no hook AND hardcodes `Processor<StdSyncHandler>`, whose DECSET
/// 2026 handling buffers raw PTY bytes.
///
/// `T` is the parser's `Timeout`. It is [`NeverBuffer`] in production. Instantiating it with
/// vte's stock `StdSyncHandler` is the negative control: it restores byte buffering and the
/// real-PTY invariants below stop holding.
pub struct SyncDriver<T: Timeout = NeverBuffer> {
    processor: Processor<T>,
    barrier: SyncBarrier,
}

impl<T: Timeout> Default for SyncDriver<T> {
    fn default() -> Self {
        Self {
            processor: Processor::new(),
            barrier: SyncBarrier::new(),
        }
    }
}

impl SyncDriver<NeverBuffer> {
    /// The production driver. `SyncDriver::<T>::default()` builds one over another `Timeout`
    /// (that is how the negative control instantiates vte's stock `StdSyncHandler`).
    pub fn new() -> Self {
        Self::default()
    }
}

impl<T: Timeout> SyncDriver<T> {
    pub fn barrier(&self) -> &SyncBarrier {
        &self.barrier
    }

    pub fn barrier_mut(&mut self) -> &mut SyncBarrier {
        &mut self.barrier
    }

    /// Bytes sitting in vte's raw synchronized-update buffer.
    ///
    /// Permanently `0` under [`NeverBuffer`] -- that is the point, and it is what the negative
    /// control measures. It carries NO volume signal, so do not use it as a bound.
    pub fn sync_bytes_count(&self) -> usize {
        self.processor.sync_bytes_count()
    }

    /// Feed one PTY read.
    ///
    /// `publish` is invoked at each publication point with the handler, so the caller can take a
    /// presentation snapshot of the CURRENT (authoritative) state.
    ///
    /// WHY BYTE AT A TIME. A publication must land at the exact byte that dispatched the ESU.
    /// `Processor::advance` consumes a whole slice with no hook inside it, so feeding a whole
    /// read would push the publication to the end of the read -- and a read containing
    /// `ESU(n) BSU(n+1) <mutations of n+1>` would then publish a TORN mix of frame n and frame
    /// n+1, or, with the naive end-of-read test, drop frame n entirely. Splitting at every byte
    /// is the only split that is exact without re-implementing vte's escape scanner.
    /// `tests/throughput.rs` measures what that costs.
    pub fn feed<H, F>(&mut self, inner: &mut H, bytes: &[u8], mut publish: F)
    where
        H: Handler + ZeroWidthTarget,
        F: FnMut(&mut H, PublishReason),
    {
        for &byte in bytes {
            {
                let mut spy = SyncSpy::new(inner, &mut self.barrier);
                self.processor.advance(&mut spy, &[byte]);
            }
            if self.barrier.take_frame_publication() {
                publish(inner, PublishReason::FrameComplete);
            }
        }
        if self.barrier.take_wakeup_publication() {
            publish(inner, PublishReason::Wakeup);
        }
    }

    /// Close an unterminated synchronized update. See [`SyncBarrier::abort_sync`].
    pub fn abort_sync<H, F>(&mut self, inner: &mut H, mut publish: F)
    where
        H: Handler,
        F: FnMut(&mut H, PublishReason),
    {
        if self.barrier.abort_sync() {
            publish(inner, PublishReason::SyncAborted);
        }
    }
}
