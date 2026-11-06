//! A followed tab's feed: `subscribe` and `unsubscribe`, `more`, and the frames the tick takes for each observer
//! while its connection's writer has room.

use super::*;

/// A feed line as the reader hands it to the tick.
pub(super) enum FeedInbound {
    Subscribe {
        key: String,
    },
    Unsubscribe {
        key: String,
    },
    /// Older turns of the followed tab; not carried out by this build.
    More {
        key: String,
    },
}

impl FeedInbound {
    /// Reads a feed line on the reader's thread; `None` for any other line.
    pub(super) fn read(line: RvInbound) -> Option<FeedInbound> {
        match line {
            RvInbound::Subscribe { key, .. } => Some(FeedInbound::Subscribe { key }),
            RvInbound::Unsubscribe { key, .. } => Some(FeedInbound::Unsubscribe { key }),
            RvInbound::More { key, .. } => Some(FeedInbound::More { key }),
            _ => None,
        }
    }
}

impl RemoteSocket {
    /// Carries out a feed line on the tick.
    pub(super) fn take_feed(&self, handle: &AgentPanelHandle, conn: u64, line: FeedInbound) {
        match line {
            FeedInbound::Subscribe { key } => self.subscribe(handle, conn, &key),
            FeedInbound::Unsubscribe { key } => {
                let Some(key) = RowKey::parse(&key) else {
                    return;
                };
                let id = self
                    .conns
                    .borrow_mut()
                    .get_mut(&conn)
                    .and_then(|c| c.observers.remove(&key));
                if let Some(id) = id {
                    handle.state.borrow_mut().tabs.unobserve(id);
                }
            }
            FeedInbound::More { key } => {
                let key = RowKey::parse(&key);
                self.answer(conn, refused_line(key.as_ref(), NOT_YET));
            }
        }
    }

    fn subscribe(&self, handle: &AgentPanelHandle, conn: u64, key: &str) {
        let mut conns = self.conns.borrow_mut();
        let Some(c) = conns.get_mut(&conn) else {
            return;
        };
        let Some(key) = RowKey::parse(key) else {
            drop(conns);
            return self.answer(conn, refused_line(None, "no such tab"));
        };
        let refusal = if c.observers.contains_key(&key) {
            Some("already subscribed")
        } else if c.observers.len() >= RV_MAX_OBSERVERS_PER_CONNECTION {
            Some("too many subscriptions")
        } else {
            let mut state = handle.state.borrow_mut();
            // The observers of a socket dropped without `close` are left here, for the tab set to forget.
            self.reaper
                .borrow_mut()
                .get_or_insert_with(|| state.tabs.observer_reaper());
            match state.tabs.observe(&key) {
                Ok(id) => {
                    c.observers.insert(key.clone(), id);
                    None
                }
                Err(ObserveError::NoSuchTab) => Some("no such tab"),
                Err(ObserveError::NotLive) => Some("not live"),
            }
        };
        drop(conns);
        if let Some(why) = refusal {
            self.answer(conn, refused_line(Some(&key), why));
        }
    }

    /// Takes frames for observers whose writer has room, at most the room left; marks the writers that frames wait
    /// for as starved.
    pub(super) fn send_frames(&self, handle: &AgentPanelHandle) {
        let mut conns = self.conns.borrow_mut();
        if conns.is_empty() {
            return;
        }
        let mut state = handle.state.borrow_mut();
        let owners: BTreeMap<ObserverId, u64> = conns
            .iter()
            .flat_map(|(conn, c)| c.observers.values().map(move |id| (*id, *conn)))
            .collect();
        for id in state.tabs.observers_with_frames() {
            let Some(conn) = owners.get(&id) else {
                continue;
            };
            let Some(c) = conns.get_mut(conn) else {
                continue;
            };
            if !c.has_room() {
                continue;
            }
            let budget = WRITER_BYTES.saturating_sub(c.gauge.bytes.load(Ordering::SeqCst));
            for frame in state.tabs.take_observer_frames(id, budget) {
                if let ObserverFrame::Gone { key, .. } = &frame {
                    c.observers.remove(key);
                }
                c.queue(frame_line(&frame), false);
            }
        }
        // Frames the budget left behind, or that found the writer full, wait for that writer's room.
        let waiting: std::collections::BTreeSet<u64> = state
            .tabs
            .observers_with_frames()
            .iter()
            .filter_map(|id| owners.get(id).copied())
            .collect();
        for conn in waiting {
            if let Some(c) = conns.get(&conn) {
                c.wait_for_room(&self.shared.wake);
            }
        }
    }
}
