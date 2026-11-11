//! `act`: a device's remote message about a tab. Its device and token are checked on the connection's reader thread,
//! against the device store read again for each, and the token goes no further; on the tick the message goes to
//! [`AgentPanelHandle::remote_message`] and its closed list, and the connection gets its `result`: at once, or, for a
//! call that waits on the provider, from a later pump ([`RemoteSocket::deliver_results`]). A queued message the tab
//! later takes out unsent (its mode widened) gets a second `result`, a refusal.
//!
//! The socket's own verdicts are recorded in the window's audit log: a device and token the store does not hold as a
//! pair (`bad_token`, naming the device id as the peer gave it), authority withdrawn before dispatch (`revoked_token`),
//! a device that may only watch (`grant`), and a row key that is not a key (`tab_not_live`), each with the kind when it
//! is one a phone may send. What [`AgentPanelHandle::remote_message`] decides it records itself.

use super::*;
use eitri_core::remote_audit::{DeviceRef, Event as AuditEvent, Reason};

/// Why an `act` from a device that may only watch is refused.
const WATCH_ONLY: &str = "watch only";

/// Why an `act` whose device and token the store does not hold as a pair is refused.
const NOT_AUTHORIZED: &str = "not authorized";

/// An `act` as the reader hands it to the tick.
pub(super) struct ActInbound {
    key: String,
    authority: Option<eitri_core::remote_devices::AuthorizationStamp>,
    /// The device's id and name when it may steer, else why not.
    verdict: Result<(String, String), &'static str>,
    raw: String,
    /// The message's command id, when it gave one no longer than [`COMMAND_ID_MAX_BYTES`]; a longer one is never
    /// copied.
    command_id: Option<String>,
    /// The device id as the peer gave it, and the store's name for it when the token matched.
    device: String,
    name: Option<String>,
    /// The message's kind, when it names one a phone may send.
    kind: Option<&'static str>,
}

impl ActInbound {
    /// Reads an `act` on the reader's thread; `None` for any other line.
    pub(super) fn read(line: RvInbound, shared: &Shared) -> Option<ActInbound> {
        let RvInbound::Act {
            device,
            token,
            key,
            cmd,
            ..
        } = line
        else {
            return None;
        };
        let (verdict, name, authority) =
            match eitri_core::remote_devices::authorize_act(&shared.store, &device, token.expose()) {
                Ok(Some((name, Grant::WatchSteer, stamp))) => {
                    (Ok((device.clone(), name.clone())), Some(name), Some(stamp))
                }
                Ok(Some((name, Grant::Watch, _))) => (Err(WATCH_ONLY), Some(name), None),
                Ok(None) => (Err(NOT_AUTHORIZED), None, None),
                Err(e) => {
                    eprintln!("[agent_panel] remote: the device store is not used: {e}");
                    (Err(NOT_AUTHORIZED), None, None)
                }
            };
        let kind = cmd
            .get("type")
            .and_then(serde_json::Value::as_str)
            .and_then(eitri_core::remote_audit::command_kind);
        let command_id = cmd
            .get("command_id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| id.len() <= COMMAND_ID_MAX_BYTES)
            .map(str::to_owned);
        Some(ActInbound {
            authority,
            key,
            verdict,
            raw: cmd.to_string(),
            command_id,
            device,
            name,
            kind,
        })
    }
}

impl RemoteSocket {
    /// Carries out an `act` on the tick and answers it.
    pub(super) fn take_act(&self, handle: &AgentPanelHandle, conn: u64, act: ActInbound) {
        let ActInbound {
            authority,
            key,
            verdict,
            raw,
            command_id,
            device,
            name,
            kind,
        } = act;
        let withdrawn = verdict.is_ok() && authority.as_ref().is_none_or(|stamp| !stamp.is_current());
        let verdict = if withdrawn { Err(NOT_AUTHORIZED) } else { verdict };
        let refused = match (&verdict, RowKey::parse(&key)) {
            (Err(_), _) if withdrawn => Some(Reason::RevokedToken),
            (Err(why), _) if *why == WATCH_ONLY => Some(Reason::Grant),
            (Err(_), _) => Some(Reason::BadToken),
            (Ok(_), None) => Some(Reason::TabNotLive),
            (Ok(_), Some(_)) => None,
        };
        if let Some(reason) = refused {
            self.shared.audit.record(AuditEvent::Refused {
                reason,
                device: Some(DeviceRef {
                    id: device,
                    name: name.unwrap_or_default(),
                }),
                listener: None,
                peer: None,
                key: Some(key.clone()),
                kind: kind.map(str::to_owned),
                dropped: None,
            });
        }
        if !self.conns.borrow().contains_key(&conn) {
            return;
        }
        let outcome = match (verdict, RowKey::parse(&key)) {
            (Err(why), _) => Err(why.to_owned()),
            (Ok(_), None) => Err(refusal_text(&RemoteRefusal::NoSuchTab)),
            (Ok((device_id, device)), Some(key)) => handle
                .remote_message(
                    &RemoteOrigin {
                        device_id,
                        authority,
                        device,
                        conn,
                    },
                    &key,
                    &raw,
                )
                .map_err(|why| refusal_text(&why)),
        };
        // A call out to the provider is answered when it comes back.
        if outcome == Ok(RemoteAccepted::Pending) {
            return;
        }
        let line = result_line(
            command_id.as_deref(),
            outcome.as_ref().map(|_| ()).map_err(String::as_str),
        );
        self.answer(conn, line);
    }

    /// Called from the pump: the results of commands that came after they were taken, each to its connection while
    /// it is open. One whose connection has gone is dropped; the phone shows that command as not confirmed.
    pub(super) fn deliver_results(&self, handle: &AgentPanelHandle) {
        for result in handle.take_remote_results() {
            if !self.conns.borrow().contains_key(&result.conn) {
                continue;
            }
            let outcome = result.outcome.map_err(|why| refusal_text(&why));
            let line = result_line(
                Some(&result.command_id),
                outcome.as_ref().map(|_| ()).map_err(String::as_str),
            );
            self.answer(result.conn, line);
        }
    }
}

/// How a refusal reads in a `result` line.
pub(super) fn refusal_text(why: &RemoteRefusal) -> String {
    match why {
        RemoteRefusal::Parse => "malformed".to_owned(),
        RemoteRefusal::NoSuchTab => "no such tab".to_owned(),
        RemoteRefusal::NotClaude => "not a Claude tab".to_owned(),
        RemoteRefusal::NotLive => "not live".to_owned(),
        RemoteRefusal::WorkChanged => "work changed".to_owned(),
        RemoteRefusal::Duplicate => "duplicate command id".to_owned(),
        RemoteRefusal::Agent(text) | RemoteRefusal::Busy(text) => text.replace(['\n', '\r'], " "),
        RemoteRefusal::NotYet => NOT_YET.to_owned(),
        RemoteRefusal::ModeNotAllowed => "mode not allowed".to_owned(),
        RemoteRefusal::NotStarted => "not started".to_owned(),
        RemoteRefusal::SlashOrBang => "starts with / or !".to_owned(),
        RemoteRefusal::TooLong => "too long".to_owned(),
        RemoteRefusal::NotYours => "nothing of this device's is queued".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Rig, PATIENCE};
    use super::*;

    #[test]
    fn an_act_withdrawn_before_dispatch_is_refused_without_reaching_the_tab() {
        use crate::agent_panel::tests::{handle_with_two_pages, live_tab};
        use eitri_core::remote_devices::{token_hash_hex, update};
        for withdrawal in ["revoke", "downgrade", "replace-token"] {
            let dir = agent::state_dirs::test_workspace_dir(withdrawal);
            let store = dir.join("devices.json");
            agent::private_fs::write_private(
                &store,
                serde_json::json!({"v": 1, "devices": [{
                    "id": "d1", "name": "phone", "token_sha256": token_hash_hex("tok"), "grant": "watch+steer",
                }]})
                .to_string()
                .as_bytes(),
            )
            .unwrap();
            let (handle, _, _) = handle_with_two_pages();
            let (tab, provider) = live_tab(&handle, &dir);
            let key = handle
                .state
                .borrow()
                .tabs
                .get(tab)
                .unwrap()
                .row_key()
                .as_str()
                .to_owned();
            let (inbound, rx) = mpsc::sync_channel(INBOUND_LINES);
            let wake: agent::UiWake = Arc::new(|| {});
            let shared = Arc::new(Shared {
                inbound,
                wake: wake.clone(),
                store: store.clone(),
                deadlines: Deadlines::default(),
                live: AtomicUsize::new(0),
                next_conn: AtomicU64::new(1),
                stopped: AtomicBool::new(false),
                devices: devices::DevicesShared::new("b4e1c09a7d3f5e21"),
                serve_probe: std::sync::Mutex::new(None),
                audit: AuditSink::open(&dir, "b4e1c09a7d3f5e21", wake),
            });
            let (stream, _peer) = UnixStream::pair().unwrap();
            let (out, answers) = mpsc::channel();
            let socket = RemoteSocket {
                listener: None,
                shared,
                inbound: rx,
                conns: RefCell::new(BTreeMap::from([(
                    1,
                    Conn {
                        stream,
                        out,
                        gauge: Arc::new(Gauge::default()),
                        observers: BTreeMap::new(),
                    },
                )])),
                reaper: RefCell::new(None),
                feed: feed::FeedState::default(),
                closes: RefCell::new(Vec::new()),
            };
            let act = ActInbound::read(
                RvInbound::Act {
                    v: eitri_core::remote_rv::RV_PROTOCOL,
                    device: "d1".into(),
                    token: eitri_core::remote_rv::Secret::new("tok"),
                    key,
                    cmd: serde_json::json!({"type": "send", "command_id": "withdrawn", "text": "hello"}),
                },
                &socket.shared,
            )
            .unwrap();
            assert!(act.verdict.is_ok());
            update(&store, |s| match withdrawal {
                "revoke" => {
                    s.remove("d1");
                }
                "downgrade" => s.get_mut("d1").unwrap().grant = Grant::Watch,
                _ => s.get_mut("d1").unwrap().token_sha256 = [42; 32],
            })
            .unwrap();
            socket.take_act(&handle, 1, act);
            let result: serde_json::Value =
                serde_json::from_str(&answers.try_recv().expect("a refusal is answered").line).unwrap();
            assert_eq!(result["ok"], false);
            assert_eq!(result["why"], NOT_AUTHORIZED);
            socket.audit().flush();
            let records =
                eitri_core::remote_audit::read(&eitri_core::remote_audit::audit_dir(&dir), &Default::default(), None)
                    .unwrap();
            assert!(records
                .iter()
                .any(|record| record.fields.get("reason") == Some(&serde_json::json!("revoked_token"))));
            assert!(!records
                .iter()
                .any(|record| record.fields.get("reason") == Some(&serde_json::json!("bad_token"))));
            assert_eq!(result["command_id"], "withdrawn");
            assert!(provider.turns().is_empty());
            assert_eq!(handle.state.borrow().tabs.remote_sends_out(), 0);
        }
    }

    #[test]
    fn withdrawn_queued_messages_leave_one_count_only_notice() {
        use crate::agent_panel::tests::{handle_with_two_pages, live_tab, tick_handle_until};
        use eitri_core::remote_devices::{token_hash_hex, update};
        for revoke in [true, false] {
            let dir = agent::state_dirs::test_workspace_dir("withdraw-notice");
            let store = dir.join("devices.json");
            let name = "<img src=x>\u{202e}phone";
            agent::private_fs::write_private(
                &store,
                serde_json::json!({"v": 1, "devices": [
                    {"id": "d1", "name": name, "token_sha256": token_hash_hex("tok"), "grant": "watch+steer"},
                    {"id": "d2", "name": name, "token_sha256": token_hash_hex("tok2"), "grant": "watch+steer"}
                ]})
                .to_string()
                .as_bytes(),
            )
            .unwrap();
            let (handle, base, _) = handle_with_two_pages();
            let (tab, provider) = live_tab(&handle, &dir);
            let key = handle
                .state
                .borrow()
                .tabs
                .get(tab)
                .unwrap()
                .row_key()
                .as_str()
                .to_owned();
            provider.queue(agent::AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
            tick_handle_until(&handle, "running", || {
                handle.state.borrow().tabs.get(tab).unwrap().turn_running()
            });
            let (inbound, rx) = mpsc::sync_channel(INBOUND_LINES);
            let wake: agent::UiWake = Arc::new(|| {});
            let shared = Arc::new(Shared {
                inbound,
                wake: wake.clone(),
                store: store.clone(),
                deadlines: Deadlines::default(),
                live: AtomicUsize::new(0),
                next_conn: AtomicU64::new(1),
                stopped: AtomicBool::new(false),
                devices: devices::DevicesShared::new("b4e1c09a7d3f5e21"),
                serve_probe: std::sync::Mutex::new(None),
                audit: AuditSink::open(&dir, "b4e1c09a7d3f5e21", wake),
            });
            let (stream, _peer) = UnixStream::pair().unwrap();
            let (out, answers) = mpsc::channel();
            let socket = RemoteSocket {
                listener: None,
                shared,
                inbound: rx,
                conns: RefCell::new(BTreeMap::from([(
                    1,
                    Conn {
                        stream,
                        out,
                        gauge: Arc::new(Gauge::default()),
                        observers: BTreeMap::new(),
                    },
                )])),
                reaper: RefCell::new(None),
                feed: feed::FeedState::default(),
                closes: RefCell::new(Vec::new()),
            };

            for (device, token, id, text) in [
                ("d1", "tok", "q1", "SECRET ONE"),
                ("d1", "tok", "q2", "SECRET TWO"),
                ("d2", "tok2", "q3", "other phone"),
            ] {
                let act = ActInbound::read(
                    RvInbound::Act {
                        v: eitri_core::remote_rv::RV_PROTOCOL,
                        device: device.into(),
                        token: eitri_core::remote_rv::Secret::new(token),
                        key: key.clone(),
                        cmd: serde_json::json!({"type": "queue", "command_id": format!("{revoke}-{id}"), "text": text}),
                    },
                    &socket.shared,
                )
                .unwrap();
                socket.take_act(&handle, 1, act);
                let result: serde_json::Value = serde_json::from_str(&answers.try_recv().unwrap().line).unwrap();
                assert_eq!(result["ok"], true);
            }
            base.sent.borrow_mut().clear();
            update(&store, |s| {
                if revoke {
                    s.remove("d1");
                } else {
                    s.get_mut("d1").unwrap().grant = Grant::Watch;
                }
            })
            .unwrap();
            handle.tick();
            let notices = handle
                .state
                .borrow()
                .tabs
                .get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .notices
                .clone();
            assert_eq!(notices.len(), 1, "one withdrawal row per device and tab");
            assert_eq!(
                notices[0].content.as_deref(),
                Some(format!("Withdrew 2 queued messages from {name}: device steering permission withdrawn").as_str())
            );
            assert_eq!(notices[0].level, agent::NoticeLevel::Notice);
            assert!(!notices[0].content.as_ref().unwrap().contains("SECRET"));
            assert_eq!(handle.state.borrow().tabs.queue_texts(tab), vec!["other phone"]);
            base.sent.borrow_mut().clear();
            handle.tick();
            assert_eq!(
                handle
                    .state
                    .borrow()
                    .tabs
                    .get(tab)
                    .unwrap()
                    .live()
                    .unwrap()
                    .projection()
                    .notices
                    .len(),
                1
            );
            assert!(
                base.sent
                    .borrow()
                    .iter()
                    .any(|e| e.contains("device steering permission withdrawn")),
                "the row reaches the page"
            );
            provider.queue(agent::AgentDomainEvent::TurnCompleted {
                turn_id: "t1".into(),
                outcome: agent::TurnOutcome::Completed,
                result_text: String::new(),
                stop_reason: None,
                usage: None,
                detail: Default::default(),
            });
            tick_handle_until(&handle, "other phone runs", || !provider.turns().is_empty());
            assert_eq!(provider.turns(), vec!["other phone"]);
            crate::agent_panel::tests::shut_down_every_tab(&handle);
        }
    }

    #[test]
    fn a_pending_call_is_answered_on_its_connection_once_the_provider_answers() {
        let rig = Rig::new("remote-act-pending");
        let (tab, provider, key) = rig.tab();
        rig.start_turn(tab, &provider, "t1");
        let mut client = rig.connect();
        let release = provider.hold_calls();
        let generation = rig.generation(tab);
        let stop = serde_json::json!({
            "type": "interrupt", "command_id": "p1",
            "expect_work": { "turn": { "id": "t1", "generation": generation } },
        });
        rig.act(&mut client, "d1", "tok", &key, stop);
        let deadline = Instant::now() + PATIENCE;
        while provider.calls_waiting() == 0 {
            assert!(Instant::now() < deadline, "the stop never reached the provider");
            rig.turn();
            std::thread::sleep(Duration::from_millis(2));
        }
        for _ in 0..3 {
            rig.turn();
        }
        client.send(&RvInbound::Ping { v: 1 }).unwrap();
        let lines = rig.until(&mut client, "the held call's pong", |l| l["kind"] == "pong");
        assert!(
            lines.iter().all(|l| l["kind"] != "result"),
            "no result while the provider waits"
        );
        drop(release);
        let lines = rig.until(&mut client, "the result", |l| l["kind"] == "result");
        let result = lines.last().unwrap();
        assert_eq!(
            (&result["command_id"], &result["ok"]),
            (&serde_json::json!("p1"), &serde_json::json!(true))
        );
        assert_eq!(provider.interrupts(), 1);
    }

    #[test]
    fn a_result_for_a_closed_connection_is_dropped() {
        let rig = Rig::new("remote-act-closed");
        let (tab, provider, key) = rig.tab();
        rig.start_turn(tab, &provider, "t1");
        let mut gone = rig.connect();
        let release = provider.hold_calls();
        let generation = rig.generation(tab);
        let stop = serde_json::json!({
            "type": "interrupt", "command_id": "p2",
            "expect_work": { "turn": { "id": "t1", "generation": generation } },
        });
        rig.act(&mut gone, "d1", "tok", &key, stop);
        let deadline = Instant::now() + PATIENCE;
        while provider.calls_waiting() == 0 {
            assert!(Instant::now() < deadline, "the stop never reached the provider");
            rig.turn();
            std::thread::sleep(Duration::from_millis(2));
        }
        drop(gone);
        let deadline = Instant::now() + PATIENCE;
        loop {
            let closed = rig.socket().conns.borrow().is_empty();
            if closed {
                break;
            }
            assert!(Instant::now() < deadline, "the closed connection was never forgotten");
            rig.turn();
            std::thread::yield_now();
        }
        let mut other = rig.connect();
        drop(release);
        let deadline = Instant::now() + PATIENCE;
        loop {
            let pending = rig.handle.state.borrow().remote.needs_polling();
            if !pending {
                break;
            }
            assert!(Instant::now() < deadline, "the provider's result was never consumed");
            rig.turn();
            std::thread::yield_now();
        }
        assert_eq!(provider.interrupts(), 1);
        assert!(rig.handle.take_remote_results().is_empty(), "the pump took the result");
        other.send(&RvInbound::Ping { v: 1 }).unwrap();
        let lines = rig.until(&mut other, "the other connection's pong", |l| l["kind"] == "pong");
        assert!(
            lines.iter().all(|l| l["kind"] != "result"),
            "another connection is not told another's result"
        );
    }
}
