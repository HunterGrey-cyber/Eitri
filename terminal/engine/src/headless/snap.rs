//! Snapshot construction consumes the source emulator, then restores it from the full record.

use std::sync::PoisonError;
use std::time::{Duration, Instant};

use super::{SnapshotAttempt, WaitWhy, Worker};
use crate::{Screen, Snapshot, Tail, BOUNDARY_WAIT, RAW_TAIL_MAX};

pub(super) const RETRY_AFTER: Duration = Duration::from_secs(1);

impl Worker {
    pub(super) fn snapshot_if_ready(&mut self) {
        let Some(client) = &self.client else {
            return;
        };
        let queue = client.queue.clone();
        {
            let q = queue.lock().unwrap_or_else(PoisonError::into_inner);
            if !q.owed || !q.demand {
                return;
            }
        }
        if self.eof {
            self.screen.abort_sync();
        }
        let reason = if !self.screen.at_boundary() || self.screen.open_update().is_some() {
            Some(WaitWhy::OpenSequence)
        } else if self.screen.too_deep_to_move() {
            Some(WaitWhy::TooDeep)
        } else {
            None
        };
        if let Some(reason) = reason {
            if self.waiting != Some(reason) {
                self.waiting_announced = false;
                self.boundary_since = None;
            }
            self.waiting = Some(reason);
            let announce = if reason == WaitWhy::OpenSequence {
                let since = self.boundary_since.get_or_insert_with(Instant::now);
                self.eof || since.elapsed() >= BOUNDARY_WAIT || self.screen.unfinished().1 > RAW_TAIL_MAX
            } else {
                true
            };
            if announce && !self.waiting_announced {
                self.waiting_announced = true;
                let mut q = queue.lock().unwrap_or_else(PoisonError::into_inner);
                q.push(super::StreamItem::Waiting(reason));
            }
            // EOF cannot complete an unfinished sequence or reduce the keyboard stack.
            if self.eof {
                let mut q = queue.lock().unwrap_or_else(PoisonError::into_inner);
                q.owed = false;
                q.demand = false;
                if let Some(exit) = self.exit {
                    q.push(super::StreamItem::Exited(exit));
                }
            }
            return;
        }
        self.boundary_since = None;
        if self
            .snapshot_attempt
            .as_ref()
            .is_some_and(|attempt| attempt.size == self.size && attempt.at.elapsed() < RETRY_AFTER)
            && !self.eof
        {
            self.waiting = Some(WaitWhy::SnapshotTooLarge);
            self.waiting_announced = true;
            return;
        }
        self.snapshot_attempt = Some(SnapshotAttempt {
            at: Instant::now(),
            size: self.size,
            offset: self.offset,
        });
        #[cfg(test)]
        self.counts.snapshots.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Ok(full) = self.screen.read_for_move() else {
            return;
        };
        let history = self.screen.history_lines();
        let at_cap = self.screen.history_at_cap();
        self.screen = Screen::replayed_headless(&full, self.size, self.colors);
        self.screen.take_events();
        let mut retained = history;
        let replay = if full.bytes.len() <= self.snapshot_cap {
            full
        } else {
            // Proportionate sizing starts close to the cap; bisection bounds correction work.
            let mut lower = 0;
            let mut upper = history;
            retained = history.saturating_mul(self.snapshot_cap) / full.bytes.len();
            let mut best = None;
            for attempt in 0..6 {
                let mut copy = Screen::replayed_headless(&full, self.size, self.colors);
                let replay = copy
                    .read_with_history(retained)
                    .expect("a replay preserves stack depth");
                let len = replay.bytes.len();
                if len <= self.snapshot_cap {
                    lower = retained;
                    best = Some((retained, replay));
                } else {
                    upper = retained;
                }
                if upper.saturating_sub(lower) <= 1 {
                    break;
                }
                if attempt == 0 && len > self.snapshot_cap && len < full.bytes.len() {
                    // Include fixed grid overhead using the measured cost of the removed rows.
                    let extra = (len - self.snapshot_cap)
                        .saturating_mul(history - retained)
                        .div_ceil(full.bytes.len() - len);
                    retained = retained.saturating_sub(extra.max(1));
                } else {
                    retained = lower + (upper - lower) / 2;
                }
            }
            let (kept, replay) = best.unwrap_or_else(|| {
                let mut copy = Screen::replayed_headless(&full, self.size, self.colors);
                (0, copy.read_with_history(0).expect("a replay preserves stack depth"))
            });
            retained = kept;
            replay
        };
        // Visible cells are never removed. The caller must provision enough space for its grid.
        if replay.bytes.len() > self.snapshot_cap {
            let mut q = queue.lock().unwrap_or_else(PoisonError::into_inner);
            if self.waiting != Some(WaitWhy::SnapshotTooLarge) {
                q.push(super::StreamItem::Waiting(WaitWhy::SnapshotTooLarge));
            }
            self.waiting = Some(WaitWhy::SnapshotTooLarge);
            if self.eof {
                q.owed = false;
                q.demand = false;
                if let Some(exit) = self.exit {
                    q.push(super::StreamItem::Exited(exit));
                }
            }
            return;
        }
        let client = self.client.as_ref().unwrap();
        let snapshot = Snapshot {
            offset: self.offset,
            replay,
            tail: Tail::ReadAhead(vec![]),
            history_at_cap: at_cap,
            history_trimmed: (history - retained) as u32,
            output_while_detached: self.output_while_detached,
            input_written: self.inputs[&client.id].written,
        };
        queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .install(snapshot, self.exit);
        self.waiting = None;
        self.waiting_announced = false;
        self.snapshot_attempt = None;
    }

    pub(super) fn snapshot_owed(&self) -> bool {
        self.client.as_ref().is_some_and(|c| {
            let q = c.queue.lock().unwrap_or_else(PoisonError::into_inner);
            q.owed && q.demand
        })
    }
}
