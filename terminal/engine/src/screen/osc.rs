//! Holds OSC payloads outside vte until their size is known, so its unbounded OSC buffer never
//! sees an oversized sequence. Other bytes keep their exact parser dispatch points.

use crate::replay::boundary::BoundaryTracker;

pub(super) const OSC_BYTES_MAX: usize = 64 * 1024;
// Mirrors core::host_clipboard::MAX_TEXT_BYTES without making this engine depend on the host.
const CLIPBOARD_TEXT_BYTES_MAX: usize = 16 * 1024 * 1024;
const CLIPBOARD_BASE64_BYTES_MAX: usize = 4 * CLIPBOARD_TEXT_BYTES_MAX.div_ceil(3);

#[derive(Default)]
pub(super) struct OscFilter {
    boundary: BoundaryTracker,
    pending: Vec<u8>,
    released: bool,
    discarding: bool,
    payload_limit: usize,
    single: [u8; 1],
}

impl OscFilter {
    pub(super) fn at_boundary(&self) -> bool {
        !self.discarding && self.boundary.at_boundary()
    }

    /// The returned bytes are consumed before the next call. The initial ESC has already reached
    /// vte; holding the `]` leaves it in Escape with no OSC allocation or dispatch until release.
    pub(super) fn byte(&mut self, byte: u8) -> &[u8] {
        self.clear_released();
        if self.discarding {
            if byte == 0x1b {
                self.discarding = false;
                self.boundary.byte(byte);
                // vte is still in Escape from the opener; the next byte continues that escape.
                return &[];
            }
            if matches!(byte, 0x07 | 0x18 | 0x1a) {
                self.discarding = false;
                self.boundary.byte(byte);
                // BEL must cancel the held Escape without ringing; SUB keeps its own execution.
                self.single[0] = if byte == 0x1a { byte } else { 0x18 };
                return &self.single;
            }
            return &[];
        }

        let was_osc = self.boundary.in_osc();
        self.boundary.byte(byte);
        if !was_osc && self.boundary.in_osc() {
            self.payload_limit = OSC_BYTES_MAX;
            self.push_pending(byte);
            return &[];
        }
        if was_osc {
            if !self.boundary.in_osc() {
                self.push_pending(byte);
                self.released = true;
                return &self.pending;
            }
            if self.pending.len() == OSC_BYTES_MAX + 1 && self.pending.starts_with(b"]52;") {
                // The selector can be empty or contain several bytes. Its separator must already
                // fit within the general cap; only the encoded text gets the larger allowance.
                if let Some(end) = self.pending[4..].iter().position(|&b| b == b';') {
                    self.payload_limit = CLIPBOARD_BASE64_BYTES_MAX + 4 + end;
                }
            }
            if self.pending.len() == self.payload_limit + 1 {
                if self.pending.starts_with(b"]52;") {
                    eprintln!(
                        "[terminal] dropped OSC 52 exceeding {} payload bytes",
                        self.payload_limit
                    );
                }
                self.clear_pending();
                self.discarding = true;
                return &[];
            }
            self.push_pending(byte);
            return &[];
        }
        self.single[0] = byte;
        &self.single
    }

    /// Free large payload storage as soon as the parser has consumed the returned bytes.
    pub(super) fn clear_released(&mut self) {
        if self.released {
            self.clear_pending();
            self.released = false;
        }
    }

    fn push_pending(&mut self, byte: u8) {
        if self.pending.len() == self.pending.capacity() {
            // Grow on demand, but never reserve beyond the payload plus opener and terminator.
            let capacity = (self.pending.capacity() * 2).max(16).min(self.payload_limit + 2);
            self.pending.reserve_exact(capacity - self.pending.len());
        }
        self.pending.push(byte);
    }

    fn clear_pending(&mut self) {
        self.pending.clear();
        if self.pending.capacity() > OSC_BYTES_MAX + 2 {
            self.pending = Vec::new();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_mib_unterminated_osc_retains_only_the_cap() {
        let mut filter = OscFilter::default();
        for &byte in b"\x1b]2;" {
            filter.byte(byte);
        }
        for _ in 0..10 * 1024 * 1024 {
            assert!(filter.byte(b'x').is_empty());
            assert!(filter.pending.len() <= OSC_BYTES_MAX + 2);
            assert!(filter.pending.capacity() <= OSC_BYTES_MAX + 2);
        }
        assert!(filter.discarding);
        assert!(filter.pending.is_empty());
        assert!(!filter.at_boundary());
        // Any ESC ends the OSC, even without a following backslash.
        assert!(filter.byte(0x1b).is_empty());
        assert!(!filter.discarding);
        assert!(!filter.at_boundary());
        assert_eq!(filter.byte(b'['), b"[");
        assert_eq!(filter.byte(b'm'), b"m");
        assert!(filter.at_boundary());
        assert_eq!(filter.byte(b'X'), b"X");
    }

    #[test]
    fn small_osc_does_not_preallocate_the_limit() {
        let mut filter = OscFilter::default();
        for &byte in b"\x1b]2;small\x07X" {
            filter.byte(byte);
        }
        assert!(filter.pending.capacity() < 1024);
    }

    #[test]
    fn discarded_osc_forwards_sub_itself() {
        let mut filter = OscFilter::default();
        for &byte in b"\x1b]2;" {
            filter.byte(byte);
        }
        for _ in 0..OSC_BYTES_MAX {
            filter.byte(b'x');
        }
        assert_eq!(filter.byte(0x1a), b"\x1a");
        assert!(filter.at_boundary());
    }

    #[test]
    fn large_clipboard_buffer_is_bounded_and_released() {
        let limit = 4 * (16 * 1024 * 1024usize).div_ceil(3) + 5;
        for extra in [0, 1] {
            let mut filter = OscFilter::default();
            for &byte in b"\x1b]52;c;" {
                filter.byte(byte);
            }
            for _ in 0..limit - 5 + extra {
                filter.byte(b'e');
                assert!(filter.pending.capacity() <= limit + 2);
            }
            assert_eq!(filter.discarding, extra == 1);
            assert_eq!(filter.byte(0x07).len(), if extra == 0 { limit + 2 } else { 1 });
            filter.clear_released();
            assert!(filter.pending.capacity() <= OSC_BYTES_MAX + 2);
        }
    }
}
