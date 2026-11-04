//! The other window's counts as this window's top bar shows them, and when a window sends its own: a short while
//! after they last changed, so a burst of changes is one message.

use std::time::{Duration, Instant};

use super::msg::Summary;

/// `2: ⚑1 •2 ●3 +1`: cards waiting, turns finished unseen, running, queued; a zero count is left out, and the
/// window's name stands alone when all are zero.
pub fn marks(window: &str, s: &Summary) -> String {
    let mut out = format!("{window}:");
    for (count, mark) in [(s.waiting, "⚑"), (s.unseen, "•"), (s.running, "●"), (s.queued, "+")] {
        if count > 0 {
            out.push_str(&format!(" {mark}{count}"));
        }
    }
    out
}

/// The most workspace names of the other window's summary kept: far more than a window holds, few enough that the
/// prefix strip and the chooser stay bounded whatever the other window sends.
pub const MAX_PEER_WORKSPACES: usize = 64;

/// The other window's summary as this window keeps it: its counts as sent, and of its workspace names only those a
/// window could have made (1 to 32 characters, no control characters, `Workspaces::valid_name`), each once, at most
/// [`MAX_PEER_WORKSPACES`]. The summary is that window's input, and its names are drawn in this window's chrome.
pub fn bounded(mut summary: Summary) -> Summary {
    let mut seen = std::collections::BTreeSet::new();
    summary.workspaces.retain(|name| {
        crate::layout::Workspaces::valid_name(name).is_ok()
            && seen.len() < MAX_PEER_WORKSPACES
            && seen.insert(name.clone())
    });
    summary
}

/// Whether a summary names something a close would cut short or lose: a turn running, a message queued, background
/// work, an unsent draft that would not come back.
pub fn has_work(s: &Summary) -> bool {
    s.running + s.queued + s.background + s.drafts > 0
}

#[derive(Debug, Default)]
pub struct SummaryDebounce {
    sent: Option<Summary>,
    pending: Option<(Summary, Instant)>,
}

impl SummaryDebounce {
    pub const DELAY: Duration = Duration::from_millis(200);

    pub fn new() -> Self {
        Self::default()
    }

    /// What `hello` or `welcome` carried.
    pub fn sent_now(&mut self, summary: Summary) {
        self.sent = Some(summary);
        self.pending = None;
    }

    /// This window's summary now. Every change pushes the send back by [`Self::DELAY`], except work starting where
    /// the last summary sent had none: that goes at once, since window 1 closes a group whose windows all read as idle
    /// without asking, and must not read a turn just begun as nothing.
    pub fn observe(&mut self, now: Instant, current: Summary) {
        let changed = match &self.pending {
            Some((pending, _)) => *pending != current,
            None => self.sent.as_ref() != Some(&current),
        };
        if changed {
            let starts_work = has_work(&current) && !self.sent.as_ref().is_some_and(has_work);
            let at = if starts_work { now } else { now + Self::DELAY };
            self.pending = Some((current, at));
        }
    }

    /// When the summary waiting to be sent is due, if one waits.
    pub fn due_at(&self) -> Option<Instant> {
        self.pending.as_ref().map(|(_, at)| *at)
    }

    /// The summary to send now, if one is due and differs from what was last sent.
    pub fn poll(&mut self, now: Instant) -> Option<Summary> {
        let due = self.pending.as_ref().is_some_and(|(_, at)| now >= *at);
        if !due {
            return None;
        }
        let (summary, _) = self.pending.take()?;
        if self.sent.as_ref() == Some(&summary) {
            return None;
        }
        self.sent = Some(summary.clone());
        Some(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(waiting: u32, unseen: u32, running: u32, queued: u32) -> Summary {
        Summary {
            waiting,
            unseen,
            running,
            queued,
            background: 0,
            drafts: 0,
            workspaces: vec!["main".into()],
        }
    }

    #[test]
    fn the_other_windows_workspace_names_are_kept_only_as_a_window_could_make_them() {
        let mut names: Vec<String> = vec![
            "main".into(),
            "".into(),
            "a\u{1b}[31mred".into(),
            "x".repeat(33),
            "main".into(),
        ];
        names.extend((0..100).map(|n| format!("w{n}")));
        let kept = bounded(Summary {
            running: 2,
            workspaces: names,
            ..Default::default()
        });
        assert_eq!(kept.running, 2);
        assert_eq!(kept.workspaces.len(), MAX_PEER_WORKSPACES);
        assert_eq!(kept.workspaces[0], "main");
        assert_eq!(
            kept.workspaces[1], "w0",
            "the empty, control-character, too-long and repeated names are gone"
        );
    }

    #[test]
    fn a_summary_polled_early_still_says_when_it_is_due() {
        let t0 = Instant::now();
        let mut d = SummaryDebounce::new();
        d.observe(t0, s(1, 0, 0, 0));
        let due = t0 + SummaryDebounce::DELAY;
        assert_eq!(d.poll(t0 + Duration::from_millis(50)), None);
        assert_eq!(
            d.due_at(),
            Some(due),
            "an early poll leaves it waiting for its own instant"
        );
        assert_eq!(d.poll(due), Some(s(1, 0, 0, 0)));
        assert_eq!(d.due_at(), None);
    }

    #[test]
    fn marks_leave_out_zero_counts() {
        assert_eq!(marks("2", &s(1, 0, 2, 0)), "2: ⚑1 ●2");
        assert_eq!(marks("1", &s(0, 0, 0, 0)), "1:");
        assert_eq!(marks("2", &s(3, 1, 4, 2)), "2: ⚑3 •1 ●4 +2");
    }

    #[test]
    fn a_burst_of_changes_is_one_send_after_the_last() {
        let t0 = Instant::now();
        let mut d = SummaryDebounce::new();
        d.sent_now(s(0, 0, 1, 0));
        d.observe(t0, s(1, 0, 1, 0));
        d.observe(t0 + Duration::from_millis(100), s(1, 0, 2, 0));
        assert_eq!(
            d.poll(t0 + Duration::from_millis(250)),
            None,
            "the second change pushed it back"
        );
        assert_eq!(d.poll(t0 + Duration::from_millis(300)), Some(s(1, 0, 2, 0)));
        assert_eq!(d.poll(t0 + Duration::from_millis(900)), None);
    }

    #[test]
    fn work_starting_after_none_goes_at_once_and_later_changes_wait() {
        let t0 = Instant::now();
        let mut d = SummaryDebounce::new();
        d.sent_now(s(0, 0, 0, 0));
        d.observe(t0, s(0, 0, 1, 0));
        assert_eq!(d.due_at(), Some(t0), "a turn begun where none ran");
        assert_eq!(d.poll(t0), Some(s(0, 0, 1, 0)));
        d.observe(t0 + Duration::from_millis(10), s(0, 0, 2, 0));
        assert_eq!(
            d.due_at(),
            Some(t0 + Duration::from_millis(10) + SummaryDebounce::DELAY)
        );
        let background = Summary {
            background: 1,
            ..s(0, 0, 0, 0)
        };
        let mut d = SummaryDebounce::new();
        d.sent_now(s(0, 0, 0, 0));
        d.observe(t0, background.clone());
        assert_eq!(d.poll(t0), Some(background), "background work counts as work");
        let draft = Summary {
            drafts: 1,
            ..s(0, 0, 0, 0)
        };
        let mut d = SummaryDebounce::new();
        d.sent_now(s(0, 0, 0, 0));
        d.observe(t0, draft.clone());
        assert_eq!(d.poll(t0), Some(draft), "an unsent draft that would be lost counts too");
        let mut d = SummaryDebounce::new();
        d.sent_now(s(0, 0, 0, 0));
        d.observe(t0, s(1, 1, 0, 0));
        assert_eq!(
            d.poll(t0),
            None,
            "a card or an unseen turn is no work a close cuts short"
        );
    }

    #[test]
    fn a_change_and_its_undo_inside_the_delay_send_nothing() {
        let t0 = Instant::now();
        let mut d = SummaryDebounce::new();
        d.sent_now(s(0, 0, 0, 0));
        d.observe(t0, s(1, 0, 0, 0));
        d.observe(t0 + Duration::from_millis(50), s(0, 0, 0, 0));
        assert_eq!(d.poll(t0 + Duration::from_millis(400)), None);
    }

    #[test]
    fn the_first_summary_goes_without_a_hello() {
        let t0 = Instant::now();
        let mut d = SummaryDebounce::new();
        d.observe(t0, s(0, 0, 0, 0));
        assert_eq!(d.poll(t0 + SummaryDebounce::DELAY), Some(s(0, 0, 0, 0)));
    }
}
