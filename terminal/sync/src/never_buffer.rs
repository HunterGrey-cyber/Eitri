//! `NeverBuffer` -- the escape hatch out of vte's raw-byte synchronized-update buffer.

use std::time::Duration;

use alacritty_terminal::vte::ansi::Timeout;

/// A `vte::ansi::Timeout` whose `pending_timeout()` is unconditionally `false`.
///
/// vte 0.15.0 implements DECSET 2026 by buffering up to 2 MiB of raw PTY bytes
/// (`SYNC_BUFFER_SIZE = 0x20_0000`, `ansi.rs:39`) and replaying them at ESU. That freezes
/// `Term`; instead, parsing and mutation must continue while only publication waits.
///
/// `Processor::advance` (`ansi.rs:298`) enters its buffering branch only when the generic
/// `Timeout` reports `pending_timeout()`. Returning `false` makes that branch unreachable.
/// BSU still reaches `SyncSpy` (`ansi.rs:1607` sets the inert timeout and ends the parser run),
/// and ESU is parsed as ordinary `CSI ? 2026 l` (`ansi.rs:1672`).
///
/// `Processor::sync_bytes_count()` therefore stays zero and supplies no secondary volume
/// bound on an unterminated update; a volume bound would need a separate mechanism.
/// The upstream 150 ms deadline is not inherited either: vte calls `set_timeout` at BSU,
/// but `clear_timeout` is reached through `stop_sync_internal`, from `advance_sync`,
/// `advance_sync_csi` or `stop_sync`. Those require the disabled branch or an explicit call
/// this driver never makes. Upstream's event loop (`alacritty_terminal/src/event_loop.rs:229-247`)
/// reads `sync_timeout()` for its poll timeout and calls `stop_sync()` on expiry; the parser
/// does not enforce it. With no buffer to flush, the engine controls publication deadlines
/// through [`crate::barrier::SyncBarrier::abort_sync`].
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
