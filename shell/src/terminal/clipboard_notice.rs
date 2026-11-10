//! A pane-local OSC 52 notice, built only from host identity, targets and byte totals.

use std::time::{Duration, Instant};

use eitri_terminal::{HostEvents, TerminalColors};
use terminal_render::PaintOp;

const BURST: Duration = Duration::from_secs(1);
const SHOW_FOR: Duration = Duration::from_secs(3);

#[derive(Default)]
pub(super) struct ClipboardNotice {
    clipboard: Option<usize>,
    primary: Option<usize>,
    started: Option<Instant>,
}

impl ClipboardNotice {
    /// Returns an expiry only when a new notice starts; writes in its window update the same notice.
    pub(super) fn record(&mut self, events: &HostEvents, on_screen: bool, now: Instant) -> Option<Instant> {
        if events.clipboard_bytes.is_none() && events.primary_bytes.is_none() {
            return None;
        }
        if self
            .started
            .is_some_and(|start| !on_screen || now.saturating_duration_since(start) >= BURST)
        {
            *self = Self::default();
        }
        for (total, bytes) in [
            (&mut self.clipboard, events.clipboard_bytes),
            (&mut self.primary, events.primary_bytes),
        ] {
            if let Some(bytes) = bytes {
                *total = Some(total.unwrap_or(0).saturating_add(bytes));
            }
        }
        if on_screen {
            self.show(now)
        } else {
            None
        }
    }

    /// Hidden writes wait for a map before their visible lifetime starts.
    pub(super) fn show(&mut self, now: Instant) -> Option<Instant> {
        if self.started.is_some() || (self.clipboard.is_none() && self.primary.is_none()) {
            return None;
        }
        self.started = Some(now);
        Some(now + SHOW_FOR)
    }

    pub(super) fn ops(&self, now: Instant, pane: u8, cols: u16, rows: u16, colors: TerminalColors) -> Vec<PaintOp> {
        if cols == 0
            || rows == 0
            || !self
                .started
                .is_some_and(|start| now.saturating_duration_since(start) < SHOW_FOR)
        {
            return Vec::new();
        }
        let targets = [("clipboard", self.clipboard), ("primary", self.primary)]
            .into_iter()
            .filter_map(|(target, bytes)| bytes.map(|bytes| format!("{target}: {bytes} bytes")))
            .collect::<Vec<_>>()
            .join(", ");
        let text = format!("Terminal {pane}: OSC 52 wrote {targets}");
        let lines = text.len().div_ceil(usize::from(cols)).min(usize::from(rows)) as u16;
        // All text is host-authored ASCII, so byte chunks are also character boundaries.
        text.as_bytes()
            .chunks(usize::from(cols))
            .take(usize::from(lines))
            .enumerate()
            .map(|(line, chunk)| PaintOp::DrawNotice {
                row: rows - lines + line as u16,
                col: 0,
                cols,
                text: String::from_utf8(chunk.to_vec()).expect("host-authored ASCII"),
                color: colors.background,
                background: colors.foreground,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_terminal::{PtySize, Screen};

    fn write_events() -> HostEvents {
        let mut screen = Screen::new(
            PtySize {
                cols: 80,
                rows: 4,
                cell_width_px: 8,
                cell_height_px: 16,
            },
            TerminalColors::default(),
        );
        // UTF-8, NUL and a string that must never appear in the notice.
        screen.feed(b"\x1b]52;c;w6kAc2VjcmV0\x07");
        screen.take_events()
    }

    fn ops(notice: &ClipboardNotice, now: Instant) -> Vec<PaintOp> {
        notice.ops(now, 3, 120, 5, TerminalColors::default())
    }

    fn text(ops: &[PaintOp]) -> String {
        ops.iter()
            .map(|op| match op {
                PaintOp::DrawNotice { text, .. } => text.as_str(),
                _ => panic!("only notice ops"),
            })
            .collect()
    }

    #[test]
    fn one_osc52_write_shows_one_notice_with_pane_target_and_bytes_only() {
        let now = Instant::now();
        let events = write_events();
        assert_eq!(events.clipboard.as_deref(), Some("é\0secret"));
        let mut notice = ClipboardNotice::default();
        assert_eq!(notice.record(&events, true, now), Some(now + SHOW_FOR));
        let painted = ops(&notice, now);
        assert_eq!(painted.len(), 1);
        assert_eq!(text(&painted), "Terminal 3: OSC 52 wrote clipboard: 9 bytes");
        assert!(!text(&painted).contains("secret"));
        assert!(!text(&painted).contains('é'));
        assert!(!text(&painted).contains('\0'));
        assert!(ops(&notice, now + SHOW_FOR).is_empty());
    }

    #[test]
    fn ten_osc52_writes_in_a_burst_update_one_notice_and_arm_one_expiry() {
        let now = Instant::now();
        let events = write_events();
        let mut notice = ClipboardNotice::default();
        let mut expiries = 0;
        for n in 0..10 {
            let time = now + Duration::from_millis(n * 90);
            expiries += usize::from(notice.record(&events, true, time).is_some());
            assert_eq!(ops(&notice, time).len(), 1);
        }
        assert_eq!(expiries, 1);
        assert_eq!(
            text(&ops(&notice, now + BURST)),
            "Terminal 3: OSC 52 wrote clipboard: 90 bytes"
        );
        assert_eq!(notice.record(&events, true, now + BURST), Some(now + BURST + SHOW_FOR));
        assert!(text(&ops(&notice, now + BURST)).ends_with("clipboard: 9 bytes"));
    }

    #[test]
    fn hidden_osc52_writes_show_on_map_with_both_targets_and_empty_writes() {
        let now = Instant::now();
        let mut events = write_events();
        events.primary = Some(String::new());
        events.primary_bytes = Some(0);
        let mut notice = ClipboardNotice::default();
        assert_eq!(notice.record(&events, false, now), None);
        assert!(ops(&notice, now).is_empty());
        let mapped = now + Duration::from_secs(60);
        assert_eq!(notice.show(mapped), Some(mapped + SHOW_FOR));
        assert_eq!(
            text(&ops(&notice, mapped)),
            "Terminal 3: OSC 52 wrote clipboard: 9 bytes, primary: 0 bytes"
        );
        assert_eq!(notice.show(mapped), None);
    }

    #[test]
    fn osc52_notice_wraps_inside_a_narrow_pane() {
        let now = Instant::now();
        let mut notice = ClipboardNotice::default();
        notice.record(&write_events(), true, now);
        let painted = notice.ops(now, 3, 24, 4, TerminalColors::default());
        assert_eq!(text(&painted), "Terminal 3: OSC 52 wrote clipboard: 9 bytes");
        assert_eq!(painted.len(), 2);
        for op in painted {
            let PaintOp::DrawNotice { row, col, cols, .. } = op else {
                unreachable!()
            };
            assert!(row < 4);
            assert_eq!((col, cols), (0, 24));
        }
    }

    #[test]
    fn selections_and_reads_create_no_osc52_notice() {
        let now = Instant::now();
        let events = HostEvents {
            selection: Some("host selection".into()),
            ..HostEvents::default()
        };
        let mut notice = ClipboardNotice::default();
        assert_eq!(notice.record(&events, true, now), None);
        assert_eq!(notice.show(now), None);
        assert!(ops(&notice, now).is_empty());
    }
}
