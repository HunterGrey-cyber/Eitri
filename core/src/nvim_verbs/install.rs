//! The snippet that gives the window's embedded nvim `:Eitri`, and the installer that sends it.
//!
//! The chunk ([`EITRI_COMMANDS_LUA`]) takes one table, [`install_args`], and never reads the environment: an nvim
//! started inside another Eitri window inherits that window's variables, and reading them would send the words to
//! the wrong one. It is sent once per nvim session, after `VimEnter` (before it the user's own commands are not
//! defined yet, and one defined here first would make theirs fail or replace this one), the way `core::tutor`
//! sends `:EitriTutor`.

use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

use rmpv::Value;
use serde_json::json;

use super::leader::LeaderKey;
use super::CommandWords;
use crate::editor_rpc::EditorRpc;
use crate::nvim_rpc::Pending;
use crate::tutor::{read_installed, Installed, ANSWER_WAIT, EARLY_AGAIN, EARLY_GIVE_UP, POLL};

/// Defines `:Eitri`, `_G.__eitri_window`, `vim.g.eitri` and `User EitriReady`, and with a group the leader keys.
pub const EITRI_COMMANDS_LUA: &str = include_str!("eitri_commands.lua");

/// The command the snippet defines.
pub const COMMAND: &str = "Eitri";

/// The snippet's one argument as JSON: `{socket, words, names, modules, group}`. `socket` is the pane-switch socket's
/// path, `words`, `names` and `modules` are [`CommandWords`]'s `first`, `known` and `modules`, and `group` is
/// `keymap.nvim_prefix`'s left-hand side with its keys, or absent.
pub fn install_args(socket: &Path, words: &CommandWords, group: Option<(&str, &[LeaderKey])>) -> serde_json::Value {
    let mut args = json!({
        "socket": socket.to_string_lossy(),
        "words": words.first,
        "names": words.known,
        "modules": words.modules,
    });
    if let Some((lhs, keys)) = group {
        let keys: Vec<_> = keys
            .iter()
            .map(|key| json!({"suffix": key.suffix, "words": key.words, "desc": key.desc}))
            .collect();
        args["group"] = json!({"lhs": lhs, "keys": keys});
    }
    args
}

/// A JSON value as the msgpack value nvim receives (null is nil, so a missing field and a null one differ only in
/// what Lua calls them).
pub fn to_msgpack(value: &serde_json::Value) -> Value {
    use serde_json::Value as Json;
    match value {
        Json::Null => Value::Nil,
        Json::Bool(b) => Value::from(*b),
        Json::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => Value::from(i),
            (None, Some(u)) => Value::from(u),
            _ => Value::from(n.as_f64().unwrap_or(0.0)),
        },
        Json::String(s) => Value::from(s.as_str()),
        Json::Array(items) => Value::Array(items.iter().map(to_msgpack).collect()),
        Json::Object(map) => Value::Map(
            map.iter()
                .map(|(k, v)| (Value::from(k.as_str()), to_msgpack(v)))
                .collect(),
        ),
    }
}

struct Out {
    target: u64,
    pending: Option<Pending>,
    /// When the answer stops being waited for, or, with no call out, when the next one goes.
    at: Instant,
    /// When this session was first asked, for [`EARLY_GIVE_UP`].
    since: Instant,
}

/// One window's `:Eitri`, installed once per nvim session and polled only while a call is out. Toolkit-free: the
/// caller runs [`VerbsInstaller::poll`] at the time each call returns.
#[derive(Default)]
pub struct VerbsInstaller {
    editor: Option<Rc<dyn EditorRpc>>,
    args: Option<serde_json::Value>,
    installed_for: Option<u64>,
    out: Option<Out>,
    reported: bool,
}

impl VerbsInstaller {
    pub fn new() -> VerbsInstaller {
        VerbsInstaller::default()
    }

    /// The window's editor handle.
    pub fn set_editor(&mut self, editor: Rc<dyn EditorRpc>) {
        self.editor = Some(editor);
    }

    /// The snippet's argument ([`install_args`]); nothing is sent before it is set.
    pub fn set_args(&mut self, args: serde_json::Value) {
        self.args = Some(args);
    }

    /// The editor may have a new nvim session ready: installs there unless that session already has it or a call
    /// to it is out. Returns when to [`poll`](Self::poll).
    pub fn editor_session(&mut self, now: Instant) -> Option<Instant> {
        let target = self.editor.as_ref().and_then(|e| e.target());
        let Some(target) = target else {
            return self.next_poll(now);
        };
        if self.installed_for == Some(target) || self.out.as_ref().is_some_and(|o| o.target == target) {
            return self.next_poll(now);
        }
        self.out = None;
        self.send(target, now, now)
    }

    /// Takes an answer that arrived, or gives one up, or sends the next call after an `early`. Returns when to
    /// poll again; `None` once nothing is out.
    pub fn poll(&mut self, now: Instant) -> Option<Instant> {
        let out = self.out.as_mut()?;
        let Some(pending) = &out.pending else {
            if now < out.at {
                return Some(out.at);
            }
            let (target, since) = (out.target, out.since);
            self.out = None;
            if self.editor.as_ref().and_then(|e| e.target()) != Some(target) {
                return None;
            }
            return self.send(target, since, now);
        };
        let Some(reply) = pending.try_take() else {
            if now >= out.at {
                eprintln!("[nvim] the editor did not answer within {ANSWER_WAIT:?}; no :{COMMAND} in this session");
                self.out = None;
                return None;
            }
            return Some((now + POLL).min(out.at));
        };
        let (target, since) = (out.target, out.since);
        let answer = match reply {
            Ok(value) => read_installed(&value),
            Err(e) => Err(format!("the editor could not be asked: {e:?}")),
        };
        match answer {
            Ok(Installed::Early) if now.saturating_duration_since(since) < EARLY_GIVE_UP => {
                let at = now + EARLY_AGAIN;
                self.out = Some(Out {
                    target,
                    pending: None,
                    at,
                    since,
                });
                Some(at)
            }
            Ok(Installed::Early) => {
                eprintln!("[nvim] the editor stayed before VimEnter for {EARLY_GIVE_UP:?}; no :{COMMAND}");
                self.out = None;
                None
            }
            Ok(Installed::Done { skipped }) => {
                self.out = None;
                self.installed_for = Some(target);
                if !skipped.is_empty() && !self.reported {
                    eprintln!("[nvim] :{COMMAND} is already defined; not replacing it");
                    self.reported = true;
                }
                None
            }
            Err(why) => {
                eprintln!("[nvim] :{COMMAND} could not be installed: {why}");
                self.out = None;
                None
            }
        }
    }

    fn next_poll(&self, now: Instant) -> Option<Instant> {
        self.out.as_ref().map(|o| {
            if o.pending.is_some() {
                o.at.min(now + POLL)
            } else {
                o.at
            }
        })
    }

    fn send(&mut self, target: u64, since: Instant, now: Instant) -> Option<Instant> {
        let (Some(editor), Some(args)) = (&self.editor, &self.args) else {
            return None;
        };
        let pending = editor.exec_lua(EITRI_COMMANDS_LUA, vec![to_msgpack(args)]);
        self.out = Some(Out {
            target,
            pending: Some(pending),
            at: now + ANSWER_WAIT,
            since,
        });
        Some(now + POLL)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::time::Duration;

    use super::*;
    use crate::nvim_rpc::{Answer, RpcError};

    #[derive(Default)]
    struct FakeEditor {
        target: Cell<Option<u64>>,
        calls: RefCell<Vec<(Value, Answer)>>,
    }

    impl EditorRpc for FakeEditor {
        fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
            assert_eq!(code, EITRI_COMMANDS_LUA, "only the :Eitri snippet is sent");
            assert_eq!(args.len(), 1);
            let (answer, pending) = Pending::pair();
            self.calls.borrow_mut().push((args.into_iter().next().unwrap(), answer));
            pending
        }

        fn target(&self) -> Option<u64> {
            self.target.get()
        }
    }

    fn answer(ok: bool, early: bool, skipped: &[&str]) -> Result<Value, RpcError> {
        let skipped = Value::Array(skipped.iter().map(|s| Value::from(*s)).collect());
        let mut fields = vec![(Value::from("ok"), Value::from(ok))];
        if early {
            fields.push((Value::from("early"), Value::from(true)));
        } else {
            fields.push((Value::from("skipped"), skipped));
        }
        Ok(Value::Map(fields))
    }

    fn installer(editor: &Rc<FakeEditor>) -> VerbsInstaller {
        let mut installer = VerbsInstaller::new();
        installer.set_editor(editor.clone());
        installer.set_args(install_args(Path::new("/run/eitri/s"), &CommandWords::default(), None));
        installer
    }

    #[test]
    fn the_arguments_carry_the_socket_the_words_and_a_group_only_when_there_is_one() {
        let words = CommandWords {
            first: vec!["zoom".to_string()],
            known: vec!["zoom".to_string(), "tab.new".to_string()],
            modules: vec!["agent".to_string()],
        };
        let plain = install_args(Path::new("/run/s"), &words, None);
        assert_eq!(
            plain,
            json!({"socket": "/run/s", "words": ["zoom"], "names": ["zoom", "tab.new"], "modules": ["agent"]})
        );
        let keys = [LeaderKey {
            suffix: "z".into(),
            words: words.first.clone(),
            desc: "zoom".into(),
        }];
        let grouped = install_args(Path::new("/run/s"), &words, Some(("<leader>W", &keys)));
        assert_eq!(
            grouped["group"],
            json!({"lhs": "<leader>W", "keys": [{"suffix": "z", "words": ["zoom"], "desc": "zoom"}]})
        );
    }

    #[test]
    fn json_becomes_the_same_msgpack_nvim_would_read() {
        let value = to_msgpack(&json!({"a": [1, true, "x", null]}));
        assert_eq!(
            value,
            Value::Map(vec![(
                Value::from("a"),
                Value::Array(vec![Value::from(1), Value::from(true), Value::from("x"), Value::Nil])
            )])
        );
    }

    #[test]
    fn it_installs_once_per_nvim_session_and_again_for_the_next_one() {
        let editor = Rc::new(FakeEditor::default());
        let mut installer = installer(&editor);
        let now = Instant::now();
        assert_eq!(installer.editor_session(now), None, "no editor, nothing to ask");
        editor.target.set(Some(7));
        assert!(installer.editor_session(now).is_some());
        assert!(installer.editor_session(now).is_some(), "a call out is not repeated");
        assert_eq!(editor.calls.borrow().len(), 1);
        let (_, reply) = editor.calls.borrow_mut().remove(0);
        reply.send(answer(true, false, &[]));
        assert_eq!(installer.poll(now), None);
        assert_eq!(installer.editor_session(now), None, "this session has it");
        assert!(editor.calls.borrow().is_empty());
        editor.target.set(Some(8));
        installer.editor_session(now);
        assert_eq!(editor.calls.borrow().len(), 1, "a new session is asked");
    }

    #[test]
    fn before_vim_enter_it_asks_again_until_it_gives_up() {
        let editor = Rc::new(FakeEditor::default());
        editor.target.set(Some(1));
        let mut installer = installer(&editor);
        let start = Instant::now();
        installer.editor_session(start);
        let mut now = start;
        for _ in 0..3 {
            let (_, reply) = editor.calls.borrow_mut().remove(0);
            reply.send(answer(false, true, &[]));
            let again = installer.poll(now).expect("asks again");
            assert_eq!(again, now + EARLY_AGAIN);
            now = again;
            assert_eq!(installer.poll(now - Duration::from_millis(1)), Some(again));
            installer.poll(now);
            assert_eq!(editor.calls.borrow().len(), 1, "the next call went out");
        }
        let (_, reply) = editor.calls.borrow_mut().remove(0);
        reply.send(answer(false, true, &[]));
        assert_eq!(installer.poll(start + EARLY_GIVE_UP + POLL), None);
        assert!(editor.calls.borrow().is_empty());
    }

    #[test]
    fn nothing_is_sent_before_the_arguments_are_set() {
        let editor = Rc::new(FakeEditor::default());
        editor.target.set(Some(1));
        let mut installer = VerbsInstaller::new();
        installer.set_editor(editor.clone());
        assert_eq!(installer.editor_session(Instant::now()), None);
        assert!(editor.calls.borrow().is_empty());
    }
}
