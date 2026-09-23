//! `NeverBuffer` -- the escape hatch out of vte's raw-byte synchronized-update buffer.

use std::time::Duration;

use alacritty_terminal::vte::ansi::Timeout;

/// A `vte::ansi::Timeout` whose `pending_timeout()` is unconditionally `false`.
///
/// WHY THIS EXISTS. vte 0.15.0 -- not alacritty_terminal -- is what actually implements DECSET
/// 2026, and it implements it by BUFFERING RAW PTY BYTES (`SYNC_BUFFER_SIZE = 0x20_0000`,
/// i.e. 2 MiB, `ansi.rs:39`) and replaying them at ESU. That freezes `Term` for the whole
/// synchronized update, which the locked semantic model forbids: Term must keep parsing and
/// mutating, and only PUBLICATION is deferred.
///
/// `Processor::advance` (`ansi.rs:298`) guards the buffering branch on exactly one call:
///
/// ```text
/// while processed != bytes.len() {
///     if self.state.sync_state.timeout.pending_timeout() {
///         processed += self.advance_sync(handler, &bytes[processed..]);   // <- buffers
///     } else {
///         ... self.parser.advance_until_terminated(&mut performer, ...)   // <- normal parse
///     }
/// }
/// ```
///
/// `Timeout` is a public trait and `Processor<T: Timeout = StdSyncHandler>` is generic over it,
/// so returning `false` here takes the buffering branch permanently out of reach. BSU is still
/// dispatched to the handler (`ansi.rs:1607` sets the -- now inert -- timeout and terminates the
/// parser run), and ESU is then parsed as an ordinary `CSI ? 2026 l` through the normal
/// `('l', [b'?'])` branch (`ansi.rs:1672`). Both reach our `SyncSpy`.
///
/// CONSEQUENCES, stated plainly:
///
/// * `Processor::sync_bytes_count()` is permanently `0`. The sync buffer is never populated, so
///   there is no "2 MiB analogue" left to use as a secondary bound on an unterminated update.
///   Any volume bound would have to be invented elsewhere; we do not invent one.
/// * The "we keep upstream's 150 ms deadline for free" claim is only HALF true. `set_timeout`
///   is still called by vte at BSU, but nothing ever consults it: `clear_timeout` is reachable
///   only from `stop_sync_internal`, which is reachable only from `advance_sync` /
///   `advance_sync_csi` / `stop_sync` -- all of them behind the branch this type disables (or
///   an explicit call we never make). Upstream does NOT enforce the deadline in the parser
///   either: `alacritty_terminal/src/event_loop.rs:229-247` reads `parser.sync_timeout()` to
///   compute a poll timeout and calls `parser.stop_sync()` in the APPLICATION event loop. Under
///   `NeverBuffer` there is nothing to stop, so that whole mechanism is simply absent; see
///   [`crate::barrier::SyncBarrier::abort_sync`] for what we offer instead.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NeverBuffer;

impl Timeout for NeverBuffer {
    #[inline]
    fn set_timeout(&mut self, _duration: Duration) {}

    #[inline]
    fn clear_timeout(&mut self) {}

    /// Always `false`. This single method is the entire mechanism.
    #[inline]
    fn pending_timeout(&self) -> bool {
        false
    }
}
