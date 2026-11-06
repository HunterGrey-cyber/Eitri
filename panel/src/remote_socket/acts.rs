//! `act`: a device's remote message about a tab. Its device and token are checked on the connection's reader thread,
//! against the device store read again for each, and the token goes no further; on the tick the message goes to
//! [`AgentPanelHandle::remote_message`] and its closed list, and the connection gets its `result`: at once, or, for a
//! call that waits on the provider, from a later pump ([`RemoteSocket::deliver_results`]). A queued message the tab
//! later takes out unsent (its mode widened) gets a second `result`, a refusal.

use super::*;

/// An `act` as the reader hands it to the tick.
pub(super) struct ActInbound {
    key: String,
    /// The device's id and name when it may steer, else why not.
    verdict: Result<(String, String), &'static str>,
    raw: String,
    /// The message's command id, when it gave one no longer than [`COMMAND_ID_MAX_BYTES`]; a longer one is never
    /// copied.
    command_id: Option<String>,
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
        let verdict = match DeviceStore::load(&shared.store) {
            Ok(store) => match store.verify(&device, token.expose()) {
                Some((name, Grant::WatchSteer)) => Ok((device, name)),
                Some((_, Grant::Watch)) => Err("watch only"),
                None => Err("not authorized"),
            },
            Err(e) => {
                eprintln!("[agent_panel] remote: the device store is not used: {e}");
                Err("not authorized")
            }
        };
        let command_id = cmd
            .get("command_id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| id.len() <= COMMAND_ID_MAX_BYTES)
            .map(str::to_owned);
        Some(ActInbound {
            key,
            verdict,
            raw: cmd.to_string(),
            command_id,
        })
    }
}

impl RemoteSocket {
    /// Carries out an `act` on the tick and answers it.
    pub(super) fn take_act(&self, handle: &AgentPanelHandle, conn: u64, act: ActInbound) {
        if !self.conns.borrow().contains_key(&conn) {
            return;
        }
        let ActInbound {
            key,
            verdict,
            raw,
            command_id,
        } = act;
        let outcome = match (verdict, RowKey::parse(&key)) {
            (Err(why), _) => Err(why.to_owned()),
            (Ok(_), None) => Err(refusal_text(&RemoteRefusal::NoSuchTab)),
            (Ok((device_id, device)), Some(key)) => handle
                .remote_message(
                    &RemoteOrigin {
                        device_id,
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
        assert_eq!(
            client
                .recv(Duration::from_millis(20))
                .expect("the connection stays open"),
            None,
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
        let mut other = rig.connect();
        drop(release);
        let deadline = Instant::now() + PATIENCE;
        while provider.interrupts() == 0 {
            assert!(Instant::now() < deadline, "the provider never answered");
            rig.turn();
            std::thread::sleep(Duration::from_millis(2));
        }
        for _ in 0..5 {
            rig.turn();
        }
        assert!(rig.handle.take_remote_results().is_empty(), "the pump took the result");
        assert_eq!(
            other
                .recv(Duration::from_millis(20))
                .expect("the connection stays open"),
            None,
            "another connection is not told another's result"
        );
    }
}
