//! A followed tab's feed: `subscribe` (answered with the tab's greeting, then its frames), `unsubscribe`, `more`, and
//! the frames the tick takes for each observer while its connection's writer has room. A frame whose line is longer
//! than the line cap goes as `chunk` lines, handed to the writer only as its room allows, so a page of one huge turn
//! never fills a writer past its budget; every connection is told the window's colours again when they change.

use super::*;
use eitri_core::remote_greeting::{greeting_line, remote_theme, theme_line};
use eitri_core::remote_rv::{chunk_lines, chunk_lines_over, CHUNK_BYTES};
use eitri_core::theme::ThemeTokens;
use std::cell::Cell;
use std::collections::VecDeque;

/// What the feed keeps across pumps.
#[derive(Default)]
pub(super) struct FeedState {
    /// The window's theme as the connections were last told it: its tokens, and the colours sent.
    told_theme: RefCell<Option<(ThemeTokens, serde_json::Value)>>,
    /// The number the next line sent in chunks takes.
    next_chunked: Cell<u64>,
    /// Per connection, the lines of a frame sent in chunks that its writer had no room for yet, and whatever was taken
    /// after it, in order. A connection with lines here takes no more frames until they are all with its writer.
    unsent: RefCell<BTreeMap<u64, VecDeque<String>>>,
}

impl FeedState {
    /// The `theme` line owed to every connection, when the window's colours changed since the last call. The first call
    /// only notes them: a connection is told the colours in each greeting.
    fn theme_changed(&self, tokens: &ThemeTokens) -> Option<String> {
        let mut told = self.told_theme.borrow_mut();
        match told.as_mut() {
            None => {
                *told = Some((tokens.clone(), remote_theme(tokens)));
                None
            }
            Some((seen, _)) if seen == tokens => None,
            Some((seen, sent)) => {
                *seen = tokens.clone();
                let theme = remote_theme(tokens);
                if theme == *sent {
                    // A change to sizes or fonts only, which a phone is never told.
                    return None;
                }
                *sent = theme;
                Some(theme_line(sent))
            }
        }
    }

    fn chunked(&self) -> u64 {
        let of = self.next_chunked.get();
        self.next_chunked.set(of.wrapping_add(1));
        of
    }

    /// Hands `c`'s waiting lines to its writer while they fit under [`WRITER_BYTES`] (a writer holding nothing takes
    /// one whatever its size). Every waiting line is at most [`CHUNK_BYTES`], newline included, so a writer down to
    /// half its budget, when it raises the wake, always takes the next. Whether lines still wait.
    fn send_unsent(&self, conn: u64, c: &Conn) -> bool {
        let mut unsent = self.unsent.borrow_mut();
        let Some(lines) = unsent.get_mut(&conn) else {
            return false;
        };
        while let Some(line) = lines.front() {
            let held = c.gauge.bytes.load(Ordering::SeqCst);
            if held > 0 && held + line.len() + 1 > WRITER_BYTES {
                return true;
            }
            c.queue(lines.pop_front().expect("a line in front"), false);
        }
        unsent.remove(&conn);
        false
    }

    /// Queues `frame`'s line (or its chunk lines) for `c`, behind whatever already waits, as far as its writer has room.
    fn send_frame(&self, conn: u64, c: &Conn, frame: &ObserverFrame) {
        let line = frame_line(frame);
        let behind = self.unsent.borrow().contains_key(&conn);
        let lines = if behind {
            // Behind waiting lines, every line goes in pieces the half-empty writer takes.
            chunk_lines_over(self.chunked(), &line, CHUNK_BYTES - 1)
        } else {
            chunk_lines(self.chunked(), &line)
        };
        if lines.len() == 1 && !behind {
            // A whole line goes as it always has: one frame may pass the budget alone.
            for line in lines {
                c.queue(line, false);
            }
            return;
        }
        self.unsent.borrow_mut().entry(conn).or_default().extend(lines);
        self.send_unsent(conn, c);
    }

    /// Forgets what waited for connections that are gone.
    fn forget_closed(&self, conns: &BTreeMap<u64, Conn>) {
        self.unsent.borrow_mut().retain(|conn, _| conns.contains_key(conn));
    }
}

/// A feed line as the reader hands it to the tick.
pub(super) enum FeedInbound {
    Subscribe {
        key: String,
    },
    Unsubscribe {
        key: String,
    },
    /// The turns of the followed tab's session `generation` before `before_turn`.
    More {
        key: String,
        generation: u64,
        before_turn: u64,
    },
}

impl FeedInbound {
    /// Reads a feed line on the reader's thread; `None` for any other line.
    pub(super) fn read(line: RvInbound) -> Option<FeedInbound> {
        match line {
            RvInbound::Subscribe { key, .. } => Some(FeedInbound::Subscribe { key }),
            RvInbound::Unsubscribe { key, .. } => Some(FeedInbound::Unsubscribe { key }),
            RvInbound::More {
                key,
                generation,
                before_turn,
                ..
            } => Some(FeedInbound::More {
                key,
                generation,
                before_turn,
            }),
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
            FeedInbound::More {
                key,
                generation,
                before_turn,
            } => {
                let Some(key) = RowKey::parse(&key) else {
                    return self.answer(conn, refused_line(None, "no such tab"));
                };
                let id = self
                    .conns
                    .borrow()
                    .get(&conn)
                    .and_then(|c| c.observers.get(&key).copied());
                // At most one page owed per observer: the page is built at its next take, within that take's
                // budget, so a peer that asks again and again is answered `refused` and holds no more than one.
                let outcome = match id {
                    None => Err("not subscribed"),
                    Some(id) => handle
                        .state
                        .borrow_mut()
                        .tabs
                        .observer_more(id, generation, before_turn)
                        .map_err(|refusal| refusal.why()),
                };
                if let Err(why) = outcome {
                    self.answer(conn, refused_line(Some(&key), why));
                }
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
                    // Before the snapshot, which the tick takes from the tab set later in this same pump.
                    if let Some(greeting) = state
                        .tabs
                        .remote_greeting(&key, state.root_note.as_deref(), &state.theme)
                    {
                        c.queue(greeting_line(&key, &greeting), false);
                    }
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
        if let Some(line) = self.feed.theme_changed(&state.theme) {
            for c in conns.values() {
                c.queue(line.clone(), false);
            }
        }
        self.feed.forget_closed(&conns);
        let mut waiting: std::collections::BTreeSet<u64> = conns
            .iter()
            .filter(|(conn, c)| self.feed.send_unsent(**conn, c))
            .map(|(conn, _)| *conn)
            .collect();
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
            // The rest of a line sent in chunks goes before anything taken after it.
            if waiting.contains(conn) || !c.has_room() {
                continue;
            }
            let budget = WRITER_BYTES.saturating_sub(c.gauge.bytes.load(Ordering::SeqCst));
            for frame in state.tabs.take_observer_frames(id, budget) {
                if let ObserverFrame::Gone { key, .. } = &frame {
                    c.observers.remove(key);
                }
                self.feed.send_frame(*conn, c, &frame);
            }
            if self.feed.unsent.borrow().contains_key(conn) {
                waiting.insert(*conn);
            }
        }
        // Frames the budget left behind, chunks still to go, or frames that found the writer full wait for that
        // writer's room.
        waiting.extend(
            state
                .tabs
                .observers_with_frames()
                .iter()
                .filter_map(|id| owners.get(id).copied()),
        );
        for conn in waiting {
            if let Some(c) = conns.get(&conn) {
                c.wait_for_room(&self.shared.wake);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use crate::agent_panel::tests::*;
    use std::io::{BufRead, BufReader};

    /// `turns` completed turns, each a prompt and a reply of `reply_bytes`.
    fn turns(turns: usize, reply_bytes: usize) -> Vec<agent::AgentDomainEvent> {
        let reply = "r".repeat(reply_bytes);
        (0..turns)
            .flat_map(|n| {
                let turn = format!("turn-{n}");
                [
                    agent::AgentDomainEvent::TurnStarted { turn_id: turn.clone() },
                    agent::AgentDomainEvent::UserPromptSubmitted {
                        text: format!("prompt {n}"),
                    },
                    delta(&turn, &reply),
                    agent::AgentDomainEvent::TurnCompleted {
                        turn_id: turn,
                        outcome: agent::TurnOutcome::Completed,
                        result_text: String::new(),
                        stop_reason: None,
                        usage: None,
                        detail: Default::default(),
                    },
                ]
            })
            .collect()
    }

    /// A peer on a raw stream whose lines a thread reads as they come, so its writer never waits on it, unless the
    /// peer is told to `hold`: then it reads no further line until let go.
    struct Peer {
        stream: UnixStream,
        lines: Receiver<serde_json::Value>,
        hold: Arc<AtomicBool>,
        reader: Option<std::thread::JoinHandle<()>>,
    }

    impl Peer {
        fn connect(rig: &Rig) -> Peer {
            let stream = UnixStream::connect(&rig.path).unwrap();
            let reading = stream.try_clone().unwrap();
            let (tx, lines) = mpsc::channel();
            let hold = Arc::new(AtomicBool::new(false));
            let held = Arc::clone(&hold);
            let reader = std::thread::spawn(move || {
                let mut reading = BufReader::new(reading);
                loop {
                    while held.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    let mut line = String::new();
                    // A line the connection's end cut short is not one of its lines.
                    match reading.read_line(&mut line) {
                        Ok(_) if line.ends_with('\n') => {}
                        _ => break,
                    }
                    let value: serde_json::Value =
                        serde_json::from_str(line.trim_end()).expect("one JSON object per line");
                    if tx.send(value).is_err() {
                        break;
                    }
                }
            });
            Peer {
                stream,
                lines,
                hold,
                reader: Some(reader),
            }
        }

        fn send(&self, line: &RvInbound) {
            (&self.stream)
                .write_all(format!("{}\n", serde_json::to_string(line).unwrap()).as_bytes())
                .unwrap();
        }

        /// Turns until a line `done` accepts has come; every line on the way, that one last.
        fn until(&self, rig: &Rig, what: &str, done: impl Fn(&serde_json::Value) -> bool) -> Vec<serde_json::Value> {
            let deadline = Instant::now() + PATIENCE;
            let mut got = Vec::new();
            loop {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {what}: {:?}",
                    summary(&got)
                );
                rig.turn();
                while let Ok(line) = self.lines.recv_timeout(Duration::from_millis(2)) {
                    let found = done(&line);
                    got.push(line);
                    if found {
                        return got;
                    }
                }
            }
        }
    }

    impl Drop for Peer {
        fn drop(&mut self) {
            self.hold.store(false, Ordering::SeqCst);
            let _ = self.stream.shutdown(Shutdown::Both);
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }

    #[test]
    fn more_flood_stays_within_the_writer_budget() {
        const MORE_LINES: usize = 1000;
        let rig = Rig::new("remote-more-flood");
        let (tab, provider, key) = rig.tab();
        // The snapshot holds the last 100 of 300 turns of about 3 kB; a page another 100.
        let events = turns(300, 3_000);
        let target = rig.revision(tab) + events.len() as u64;
        provider.queue_all(events);
        tick_handle_until(&rig.handle, "the turns", || rig.revision(tab) >= target);
        let peer = Peer::connect(&rig);
        peer.send(&RvInbound::Subscribe {
            v: 1,
            key: key.to_string(),
        });
        let snapshot = peer
            .until(&rig, "the snapshot", |l| l["kind"] == "snapshot")
            .pop()
            .unwrap();
        let first_turn = snapshot["first_turn"].as_u64().unwrap();
        assert_eq!(first_turn, 200);
        let more = RvInbound::More {
            v: 1,
            key: key.to_string(),
            generation: snapshot["generation"].as_u64().unwrap(),
            before_turn: first_turn,
        };
        let flood = format!("{}\n", serde_json::to_string(&more).unwrap()).repeat(MORE_LINES);
        let writing = peer.stream.try_clone().unwrap();
        let writer = std::thread::spawn(move || (&writing).write_all(flood.as_bytes()));

        let (mut pages, mut refused, mut peak) = (0, 0, 0);
        let deadline = Instant::now() + PATIENCE * 2;
        while pages + refused < MORE_LINES {
            assert!(
                Instant::now() < deadline,
                "{pages} pages and {refused} refusals of {MORE_LINES}"
            );
            rig.turn();
            peak = peak.max(rig.writer_bytes());
            // The peer reads everything, so the answers drain before the next pump takes more lines.
            let drained = Instant::now() + PATIENCE;
            while rig
                .socket()
                .conns
                .borrow()
                .values()
                .any(|c| c.gauge.control.load(Ordering::SeqCst) > 0)
            {
                assert!(Instant::now() < drained, "the answers never drained");
                std::thread::sleep(Duration::from_millis(1));
            }
            while let Ok(line) = peer.lines.try_recv() {
                match line["kind"].as_str() {
                    Some("page") => {
                        // As many of the 100 turns asked for as the writer had room for.
                        let from = line["from_turn"].as_u64().unwrap();
                        assert!((100..=199).contains(&from), "{}", line["from_turn"]);
                        assert_eq!(line["through_turn"], 199);
                        pages += 1;
                    }
                    Some("refused") => {
                        assert_eq!(line["why"], "a page is on its way", "{line}");
                        refused += 1;
                    }
                    _ => panic!("only pages and refusals: {line}"),
                }
            }
        }
        writer.join().unwrap().unwrap();
        assert!(
            peak <= WRITER_BYTES,
            "the writer held {peak} bytes, over its budget of {WRITER_BYTES}"
        );
        assert!(pages >= 1 && refused >= 1, "{pages} pages, {refused} refusals");
        for _ in 0..5 {
            rig.turn();
        }
        assert!(peer.lines.try_recv().is_err(), "every line answered once");
        assert_eq!(rig.handle.state.borrow().tabs.observer_count(), 1);
    }

    #[test]
    fn a_huge_page_reaches_the_writer_only_as_it_has_room() {
        const REPLY_BYTES: usize = 3 * 1024 * 1024;
        let rig = Rig::new("remote-huge-page");
        let (tab, provider, key) = rig.tab();
        // Turn 0 holds one reply far longer than a writer's budget; turns 1 and 2 are short.
        let mut events = turns(1, REPLY_BYTES);
        events.extend(turns(2, 100));
        let target = rig.revision(tab) + events.len() as u64;
        provider.queue_all(events);
        tick_handle_until(&rig.handle, "the turns", || rig.revision(tab) >= target);
        let peer = Peer::connect(&rig);
        peer.send(&RvInbound::Subscribe {
            v: 1,
            key: key.to_string(),
        });
        let snapshot = peer
            .until(&rig, "the snapshot", |l| l["kind"] == "snapshot")
            .pop()
            .unwrap();
        assert_eq!(snapshot["first_turn"], 0);
        assert_eq!(snapshot["cut"].as_array().unwrap().len(), 1, "{}", snapshot["cut"]);

        // The phone reads nothing more while the page goes out: the writer holds what it was given.
        peer.hold.store(true, Ordering::SeqCst);
        peer.send(&RvInbound::More {
            v: 1,
            key: key.to_string(),
            generation: snapshot["generation"].as_u64().unwrap(),
            before_turn: 1,
        });
        let mut peak = 0;
        for _ in 0..50 {
            rig.turn();
            peak = peak.max(rig.writer_bytes());
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(peak > 0, "the page is on its way");
        assert!(
            peak <= WRITER_BYTES,
            "the writer held {peak} bytes, over its budget of {WRITER_BYTES}"
        );

        peer.hold.store(false, Ordering::SeqCst);
        let lines = peer.until(&rig, "the page's last piece", |l| {
            l["kind"] == "chunk" && l["last"] == true
        });
        let mut joined = String::new();
        for line in lines.iter().filter(|l| l["kind"] == "chunk") {
            joined.push_str(line["data"].as_str().unwrap());
        }
        let page: serde_json::Value = serde_json::from_str(&joined).expect("the pieces make one line");
        assert_eq!(page["kind"], "page");
        assert_eq!(
            (page["from_turn"].as_u64(), page["through_turn"].as_u64()),
            (Some(0), Some(0))
        );
        let reply = page["frame"]["state"]["transcript"][0]["text"].as_str().unwrap();
        assert_eq!(reply.len(), REPLY_BYTES, "the reply comes whole");
    }

    #[test]
    fn theme_change_reaches_every_connection() {
        let rig = Rig::new("remote-theme");
        let (_tab, _provider, key) = rig.tab();
        let following = Peer::connect(&rig);
        let quiet = Peer::connect(&rig);
        following.send(&RvInbound::Subscribe {
            v: 1,
            key: key.to_string(),
        });
        let lines = following.until(&rig, "the snapshot", |l| l["kind"] == "snapshot");
        let before = remote_theme(&rig.handle.state.borrow().theme);
        assert_eq!(lines[0]["kind"], "greeting");
        assert_eq!(
            lines[0]["greeting"]["theme"], before,
            "the greeting carries the colours"
        );
        let deadline = Instant::now() + PATIENCE;
        while rig.socket().conns.borrow().len() < 2 {
            assert!(Instant::now() < deadline, "the second connection was never taken in");
            rig.turn();
        }

        let mut tokens = rig.handle.state.borrow().theme.clone();
        tokens.bg.r = tokens.bg.r.wrapping_add(41);
        rig.handle.set_theme(&tokens);
        let after = remote_theme(&tokens);
        assert_ne!(after, before);
        for peer in [&following, &quiet] {
            let lines = peer.until(&rig, "the theme", |l| l["kind"] == "theme");
            assert_eq!(lines.last().unwrap()["theme"], after);
            assert_eq!(lines.iter().filter(|l| l["kind"] == "theme").count(), 1);
        }

        // A size is the desk's own: nothing is sent for it.
        rig.handle.set_panel_font_size_px(tokens.font_size_px + 3.0);
        for _ in 0..20 {
            rig.turn();
        }
        for peer in [&following, &quiet] {
            while let Ok(line) = peer.lines.recv_timeout(Duration::from_millis(20)) {
                assert_ne!(line["kind"], "theme", "{line}");
            }
        }

        // A greeting after the change carries the new colours.
        let (_other, _p, other_key) = rig.tab();
        quiet.send(&RvInbound::Subscribe {
            v: 1,
            key: other_key.to_string(),
        });
        let lines = quiet.until(&rig, "the greeting", |l| l["kind"] == "greeting");
        assert_eq!(lines.last().unwrap()["greeting"]["theme"], after);
    }

    #[test]
    fn subscribe_sends_the_greeting_before_the_snapshot_and_more_needs_a_subscription() {
        let rig = Rig::new("remote-greeting-first");
        let (tab, provider, key) = rig.tab();
        rig.start_turn(tab, &provider, "t1");
        let peer = Peer::connect(&rig);
        peer.send(&RvInbound::More {
            v: 1,
            key: key.to_string(),
            generation: rig.generation(tab),
            before_turn: 1,
        });
        let lines = peer.until(&rig, "the refusal", |l| l["kind"] == "refused");
        assert_eq!(lines.last().unwrap()["why"], "not subscribed");
        peer.send(&RvInbound::Subscribe {
            v: 1,
            key: key.to_string(),
        });
        let lines = peer.until(&rig, "the snapshot", |l| l["kind"] == "snapshot");
        let kinds: Vec<&str> = lines.iter().map(|l| l["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["greeting", "snapshot"]);
        let greeting = &lines[0]["greeting"];
        assert_eq!(greeting["tab"]["agent"], "claude");
        assert_eq!(greeting["send_block"], serde_json::Value::Null);
        assert_eq!(lines[1]["first_turn"], 0);
        assert_eq!(lines[1]["cut"], serde_json::json!([]));
    }
}
