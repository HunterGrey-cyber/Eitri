//! The reopen capture's call into the window's nvim: `EDITOR_REOPEN_CAPTURE_LUA` run through the window's
//! `EditorRpc` with the controller's arguments, its answer looked for from a GLib timer while it is out, and posted to
//! the controller as one `EditorEvent::Captured` with the token it was asked with.
//!
//! There is no give-up: an nvim at a prompt answers when the prompt ends, and the controller asks for no other capture
//! until this one has answered, so a call given up on here would stop the captures for good. The timer looks every
//! 50 ms for the first second, then every 250 ms, so a call held at a prompt costs four wakes a second.

use std::rc::Weak;
use std::time::Duration;

use eitri_core::editor_rpc::EditorRpc;
use eitri_core::nvim_rpc::{Pending, RpcError};
use eitri_core::wm::{EditorEvent, HostEvent, HostSink, WinId};
use gtk4::glib;
use rmpv::Value;

/// How often an answer is looked for at first, and for how many looks.
const LOOK_EVERY: Duration = Duration::from_millis(50);
const QUICK_LOOKS: u32 = 20;
/// How often it is looked for after that.
const LOOK_LATER: Duration = Duration::from_millis(250);

/// Asks the editor for its record and posts the answer, whenever it comes.
pub(crate) fn ask(rpc: &dyn EditorRpc, win: WinId, token: String, args: Value, sink: Weak<dyn HostSink>) {
    let pending = rpc.exec_lua(eitri_core::editor_reopen::EDITOR_REOPEN_CAPTURE_LUA, vec![args]);
    let watch = Watch::new(pending, win, token, sink);
    look_after(watch, LOOK_EVERY, 0);
}

fn look_after(mut watch: Watch, wait: Duration, looks: u32) {
    glib::timeout_add_local_once(wait, move || {
        if watch.look() {
            return;
        }
        let looks = looks.saturating_add(1);
        let wait = if looks < QUICK_LOOKS { LOOK_EVERY } else { LOOK_LATER };
        look_after(watch, wait, looks);
    });
}

/// One capture out: its answer, posted once.
struct Watch {
    pending: Pending,
    win: WinId,
    /// Taken when the answer is posted.
    token: Option<String>,
    sink: Weak<dyn HostSink>,
}

impl Watch {
    fn new(pending: Pending, win: WinId, token: String, sink: Weak<dyn HostSink>) -> Watch {
        Watch {
            pending,
            win,
            token: Some(token),
            sink,
        }
    }

    /// One look: whether the answer has been posted (or there is nothing left to post it to).
    fn look(&mut self) -> bool {
        if self.token.is_none() {
            return true;
        }
        let Some(reply) = self.pending.try_take() else {
            return false;
        };
        let Some(token) = self.token.take() else {
            return true;
        };
        if let Some(sink) = self.sink.upgrade() {
            sink.post(HostEvent::Editor(EditorEvent::Captured {
                win: self.win,
                token,
                reply: decode(reply),
            }));
        }
        true
    }
}

/// The capture's answer as the controller takes it: `nil` before this nvim's restore is done, else a table of the
/// record's JSON and whether it is the restore's own.
fn decode(reply: Result<Value, RpcError>) -> Result<Option<(String, bool)>, String> {
    let value = reply.map_err(|e| e.to_string())?;
    let Value::Map(entries) = value else {
        return match value {
            Value::Nil => Ok(None),
            other => Err(format!("the capture answered {other}, not a record")),
        };
    };
    let field = |name: &str| entries.iter().find(|(k, _)| k.as_str() == Some(name)).map(|(_, v)| v);
    let json = match field("json") {
        Some(Value::String(s)) => s.as_str().map(str::to_owned),
        Some(Value::Binary(bytes)) => String::from_utf8(bytes.clone()).ok(),
        _ => None,
    }
    .ok_or("the capture's answer has no JSON text")?;
    let same = match field("same") {
        Some(Value::Boolean(same)) => *same,
        _ => return Err("the capture's answer does not say whether it is the restore's own".into()),
    };
    Ok(Some((json, same)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::nvim_rpc::Pending;
    use eitri_core::wm::conformance::RecordingSink;
    use std::rc::Rc;

    /// What a capture answered, as the controller is told it.
    type Reply = Result<Option<(String, bool)>, String>;

    fn captured(sink: &RecordingSink) -> Vec<(String, Reply)> {
        sink.events()
            .into_iter()
            .filter_map(|(_, event)| match event {
                HostEvent::Editor(EditorEvent::Captured { token, reply, .. }) => Some((token, reply)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_capture_answer_is_posted_once_however_late() {
        let sink = Rc::new(RecordingSink::default());
        let weak: Weak<dyn HostSink> = Rc::downgrade(&(sink.clone() as Rc<dyn HostSink>));
        let (answer, pending) = Pending::pair();
        let mut watch = Watch::new(pending, WinId::MAIN, "0123456789abcdef".into(), weak);
        // Far past any give-up a call that waits on a prompt could meet: still looking, nothing posted.
        for _ in 0..100_000 {
            assert!(!watch.look());
        }
        assert!(captured(&sink).is_empty());
        answer.send(Ok(Value::Map(vec![
            (Value::from("json"), Value::from("{\"v\":1}")),
            (Value::from("same"), Value::from(true)),
        ])));
        assert!(watch.look());
        assert!(watch.look(), "a watch that posted is done");
        assert_eq!(
            captured(&sink),
            vec![(
                "0123456789abcdef".to_string(),
                Ok(Some(("{\"v\":1}".to_string(), true)))
            )],
            "posted once, with the token it was asked with"
        );
    }

    #[test]
    fn a_nil_answer_an_error_and_a_wrong_shape_are_told_apart() {
        assert_eq!(decode(Ok(Value::Nil)), Ok(None));
        assert_eq!(
            decode(Err(RpcError::Nvim("E5108: boom".into()))),
            Err("E5108: boom".to_string())
        );
        assert!(decode(Ok(Value::from(3))).is_err());
        assert!(decode(Ok(Value::Map(vec![(Value::from("json"), Value::from("{}"))]))).is_err());
        assert_eq!(
            decode(Ok(Value::Map(vec![
                (Value::from("same"), Value::from(false)),
                (Value::from("json"), Value::Binary(b"{}".to_vec())),
            ]))),
            Ok(Some(("{}".to_string(), false)))
        );
    }
}
