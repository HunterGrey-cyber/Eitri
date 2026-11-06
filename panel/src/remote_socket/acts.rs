//! `act`: a device's remote message about a tab. Its device and token are checked on the connection's reader thread,
//! against the device store read again for each, and the token goes no further; on the tick the message goes to
//! [`AgentPanelHandle::remote_message`] and its closed list, and the connection gets one `result`.

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
        let line = result_line(
            command_id.as_deref(),
            outcome.as_ref().map(|_| ()).map_err(String::as_str),
        );
        self.answer(conn, line);
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
        RemoteRefusal::Agent(text) => text.replace(['\n', '\r'], " "),
        RemoteRefusal::NotYet => NOT_YET.to_owned(),
    }
}
