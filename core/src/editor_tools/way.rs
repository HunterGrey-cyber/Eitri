//! How a call reaches an editor: this window's own, or the other window's over the link. A tool names what
//! it wants as an [`EditorCall`], and never sees which way it went.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use super::stamp::Stamp;
use super::step::{EditorReach, EditorSight};
use super::tools::baseline::BASELINE_LIMIT;
use super::tools::diagnostics::{self, DiagnosticsArgs, Severity};
use super::tools::lsp::{self, AskArgs, LspKind};
use super::tools::nvim_exec::{self, ExecSlot};
use super::tools::{editor_state, open_file, read_buffer};
use super::words;
use crate::editor_rpc::EditorRpc;
use crate::nvim_rpc::{Pending, RpcError};
use crate::window_link::editor_op::{severity_to_wire, WireLspKind};
use crate::window_link::{EditorOp, WireStamp};

/// One call on the editor, with every value already judged. No `Clone`: a variant may hold an approval, which
/// may not be copied, so the broker and the ways take it by reference.
#[derive(Debug)]
pub enum EditorCall {
    LspAsk(AskArgs),
    LspTargets {
        paths: Vec<PathBuf>,
    },
    LspCollect {
        id: String,
        last: bool,
    },
    OpenFile {
        path: PathBuf,
        line: Option<u32>,
        end_line: Option<u32>,
        load: bool,
    },
    EditorState {
        root: PathBuf,
    },
    ReadBuffer {
        path: PathBuf,
        start: Option<u32>,
        end: Option<u32>,
    },
    Diagnostics {
        root: PathBuf,
        path: Option<PathBuf>,
        min_severity: Severity,
        limit: u32,
    },
    /// Every diagnostic of every loaded buffer under the root, as the baseline of a turn keeps them.
    Baseline {
        root: PathBuf,
    },
    Exec(ExecSlot),
}

pub trait EditorWay {
    /// `session` is the tab's provider session id; only the link uses it.
    fn call(&self, session: &str, call: &EditorCall, stamp: &Stamp) -> Pending;
    fn reach(&self) -> EditorReach;
    /// Whether the person can see the editor now.
    fn sight(&self) -> EditorSight {
        EditorSight::OnScreen
    }
}

/// This window's own editor.
pub struct LocalWay(pub Rc<dyn EditorRpc>);

impl EditorWay for LocalWay {
    fn call(&self, _session: &str, call: &EditorCall, stamp: &Stamp) -> Pending {
        let rpc = self.0.as_ref();
        match call {
            EditorCall::LspAsk(args) => lsp::ask(rpc, stamp, args),
            EditorCall::LspTargets { paths } => lsp::targets(rpc, stamp, paths),
            EditorCall::LspCollect { id, last } => lsp::collect(rpc, stamp, id, *last),
            EditorCall::OpenFile {
                path,
                line,
                end_line,
                load,
            } => open_file::send(rpc, stamp, path, *line, *end_line, *load),
            EditorCall::EditorState { root } => editor_state::send(rpc, stamp, root),
            EditorCall::ReadBuffer { path, start, end } => read_buffer::send(rpc, stamp, path, *start, *end),
            EditorCall::Diagnostics {
                root,
                path,
                min_severity,
                limit,
            } => diagnostics::send(
                rpc,
                stamp,
                &DiagnosticsArgs {
                    root: root.clone(),
                    path: path.clone(),
                    min_severity: *min_severity,
                    utf16: false,
                    limit: *limit,
                },
            ),
            EditorCall::Baseline { root } => diagnostics::send(
                rpc,
                stamp,
                &DiagnosticsArgs {
                    root: root.clone(),
                    path: None,
                    min_severity: Severity::Hint,
                    utf16: false,
                    limit: BASELINE_LIMIT,
                },
            ),
            EditorCall::Exec(slot) => nvim_exec::send(rpc, stamp, slot),
        }
    }

    fn reach(&self) -> EditorReach {
        EditorReach::Own
    }

    fn sight(&self) -> EditorSight {
        if self.0.on_screen() {
            EditorSight::OnScreen
        } else {
            EditorSight::OffScreen
        }
    }
}

/// How an ask's kind goes over the link. The read-only kinds cross; the kinds that edit stay in the editor's window.
/// There is no catch-all, so a new kind does not compile until it has an arm here, and the `Err` is for the arm of
/// one that may not cross: nothing is sent.
fn wire_kind(kind: LspKind) -> Result<WireLspKind, RpcError> {
    match kind {
        LspKind::Usages => Ok(WireLspKind::Usages),
        LspKind::Hover => Ok(WireLspKind::Hover),
        LspKind::DocumentSymbols => Ok(WireLspKind::DocumentSymbols),
        LspKind::WorkspaceSymbols => Ok(WireLspKind::WorkspaceSymbols),
        LspKind::Rename | LspKind::CodeActions | LspKind::ActionEdit | LspKind::Format => {
            Err(RpcError::Encode("LSP edits stay in the editor's window".into()))
        }
    }
}

/// Window 1's editor, from window 2. A call goes as one typed op: its path relative to the root the two windows
/// share, and no root of its own, since window 1 uses its own. `send` answers `Unavailable` while the link is down.
pub struct RemoteWay {
    root: PathBuf,
    send: Box<dyn Fn(EditorOp) -> Pending>,
}

impl RemoteWay {
    pub fn new(root: &Path, send: Box<dyn Fn(EditorOp) -> Pending>) -> RemoteWay {
        RemoteWay {
            root: root.to_path_buf(),
            send,
        }
    }

    /// `path` as an op carries it. Nothing that is not under the root crosses the link.
    fn relative(&self, path: &Path) -> Result<String, RpcError> {
        let rel = path
            .strip_prefix(&self.root)
            .map_err(|_| RpcError::Encode(format!("{} is not inside the project", path.display())))?;
        rel.to_str()
            .map(str::to_owned)
            .ok_or_else(|| RpcError::Encode(format!("{} is not UTF-8", path.display())))
    }

    fn op(&self, session: &str, call: &EditorCall, stamp: &Stamp) -> Result<EditorOp, RpcError> {
        let session = session.to_owned();
        let stamp = WireStamp::from(stamp);
        Ok(match call {
            EditorCall::LspTargets { .. } => {
                return Err(RpcError::Encode("LSP targets stay in the editor's window".into()))
            }
            EditorCall::LspAsk(args) => EditorOp::AgentLspAsk {
                session,
                id: args.id.clone(),
                kind: wire_kind(args.kind)?,
                path: args.path.as_deref().map(|path| self.relative(path)).transpose()?,
                line: args.line,
                symbol: args.symbol.clone(),
                include: args.include.iter().map(|part| (*part).into()).collect(),
                query: args.query.clone(),
                budget_ms: args.budget_ms,
                load: args.load,
                stamp,
            },
            EditorCall::LspCollect { id, last } => EditorOp::AgentLspCollect {
                session,
                id: id.clone(),
                last: *last,
                stamp,
            },
            EditorCall::OpenFile {
                path,
                line,
                end_line,
                load,
            } => EditorOp::AgentOpen {
                session,
                path: self.relative(path)?,
                line: *line,
                end_line: *end_line,
                load: *load,
                stamp,
            },
            EditorCall::EditorState { root: _ } => EditorOp::AgentEditorState { session, stamp },
            EditorCall::ReadBuffer { path, start, end } => EditorOp::AgentReadBuffer {
                session,
                path: self.relative(path)?,
                start: *start,
                end: *end,
                stamp,
            },
            EditorCall::Diagnostics {
                root: _,
                path,
                min_severity,
                limit,
            } => EditorOp::AgentDiagnostics {
                session,
                path: path.as_deref().map(|path| self.relative(path)).transpose()?,
                min_severity: severity_to_wire(*min_severity),
                limit: *limit,
                stamp,
            },
            EditorCall::Baseline { root: _ } => EditorOp::AgentBaseline { session, stamp },
            // No op can carry approved code: it runs only beside the editor, so none of it crosses the link.
            EditorCall::Exec(_) => return Err(RpcError::Unavailable(words::MOVE_TO_EDITOR.to_owned())),
        })
    }
}

impl EditorWay for RemoteWay {
    fn call(&self, session: &str, call: &EditorCall, stamp: &Stamp) -> Pending {
        match self.op(session, call, stamp) {
            Ok(op) => (self.send)(op),
            Err(error) => Pending::failed(error),
        }
    }

    fn reach(&self) -> EditorReach {
        EditorReach::Link
    }

    fn sight(&self) -> EditorSight {
        EditorSight::OtherWindow
    }
}

#[cfg(test)]
mod tests {
    use super::super::tools::diagnostics::DIAGNOSTICS_LIMIT;
    use super::super::tools::diagnostics::DIAGNOSTICS_LUA;
    use super::super::tools::editor_state::EDITOR_STATE_LUA;
    use super::super::tools::lsp::UsagePart;
    use super::super::tools::open_file::OPEN_FILE_LUA;
    use super::super::tools::read_buffer::READ_BUFFER_LUA;
    use super::*;
    use crate::window_link::editor_op::WireUsagePart;
    use rmpv::Value;
    use std::cell::RefCell;

    /// Keeps what it was asked and answers nothing useful.
    #[derive(Default)]
    struct Recording(RefCell<Vec<(&'static str, Vec<Value>)>>);

    impl EditorRpc for Recording {
        fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
            self.0.borrow_mut().push((code, args));
            Pending::failed(RpcError::Closed)
        }

        fn target(&self) -> Option<u64> {
            Some(1)
        }
    }

    struct Hidden;

    impl EditorRpc for Hidden {
        fn exec_lua(&self, _code: &'static str, _args: Vec<Value>) -> Pending {
            Pending::failed(RpcError::Closed)
        }

        fn target(&self) -> Option<u64> {
            Some(1)
        }

        fn on_screen(&self) -> bool {
            false
        }
    }

    #[test]
    fn each_way_says_whether_the_person_can_see_the_editor() {
        assert_eq!(LocalWay(Rc::new(Recording::default())).sight(), EditorSight::OnScreen);
        assert_eq!(LocalWay(Rc::new(Hidden)).sight(), EditorSight::OffScreen);
        let remote = RemoteWay::new(Path::new("/p"), Box::new(|_| Pending::failed(RpcError::Closed)));
        assert_eq!(remote.sight(), EditorSight::OtherWindow);
    }

    fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
        let map = value.as_map().expect("a map");
        map.iter()
            .find(|(key, _)| key.as_str() == Some(name))
            .map(|(_, value)| value)
    }

    fn sent(call: EditorCall) -> (&'static str, Value) {
        let rpc = Rc::new(Recording::default());
        let stamp = Stamp {
            deadline_ms: 9,
            scope: "s-1".to_owned(),
            seq: 3,
        };
        let way = LocalWay(rpc.clone());
        let _answer = way.call("session", &call, &stamp);
        let (code, args) = rpc.0.borrow_mut().pop().expect("one call");
        assert_eq!(args.len(), 2, "every chunk takes (stamp, args)");
        assert_eq!(args[0], stamp.to_value());
        (code, args[1].clone())
    }

    fn path_bytes(text: &str) -> Value {
        Value::Binary(text.as_bytes().to_vec())
    }

    #[test]
    fn each_call_reaches_its_own_chunk() {
        let (code, args) = sent(EditorCall::LspAsk(AskArgs {
            id: "ask-1".into(),
            root: "/p".into(),
            path: Some("/p/a.rs".into()),
            budget_ms: 15000,
            ..Default::default()
        }));
        assert_eq!(code, lsp::LSP_ASK_LUA);
        assert_eq!(field(&args, "root"), Some(&path_bytes("/p")));
        assert_eq!(field(&args, "path"), Some(&path_bytes("/p/a.rs")));
        assert_eq!(field(&args, "id"), Some(&Value::from("ask-1")));
        assert_eq!(field(&args, "budget_ms"), Some(&Value::from(15000)));
        let (code, args) = sent(EditorCall::LspCollect {
            id: "ask-1".into(),
            last: true,
        });
        assert_eq!(code, lsp::LSP_COLLECT_LUA);
        assert_eq!(field(&args, "id"), Some(&Value::from("ask-1")));
        assert_eq!(field(&args, "last"), Some(&Value::from(true)));

        let (code, args) = sent(EditorCall::LspTargets {
            paths: vec!["/p/a.rs".into()],
        });
        assert_eq!(code, lsp::LSP_TARGETS_LUA);
        assert_eq!(field(&args, "paths"), Some(&Value::Array(vec![path_bytes("/p/a.rs")])));

        let (code, args) = sent(EditorCall::OpenFile {
            path: "/p/a.rs".into(),
            line: Some(4),
            end_line: None,
            load: true,
        });
        assert_eq!(code, OPEN_FILE_LUA);
        assert_eq!(field(&args, "path"), Some(&path_bytes("/p/a.rs")));
        assert_eq!(field(&args, "line"), Some(&Value::from(4)));
        assert_eq!(field(&args, "end_line"), None, "an absent key reaches Lua as nil");
        assert_eq!(field(&args, "load"), Some(&Value::from(true)));

        let (code, args) = sent(EditorCall::EditorState { root: "/p".into() });
        assert_eq!(code, EDITOR_STATE_LUA);
        assert_eq!(field(&args, "root"), Some(&path_bytes("/p")));

        let (code, args) = sent(EditorCall::ReadBuffer {
            path: "/p/a.rs".into(),
            start: None,
            end: Some(9),
        });
        assert_eq!(code, READ_BUFFER_LUA);
        assert_eq!(field(&args, "path"), Some(&path_bytes("/p/a.rs")));
        assert_eq!(field(&args, "start"), None);
        assert_eq!(field(&args, "end"), Some(&Value::from(9)));

        let (code, args) = sent(EditorCall::Diagnostics {
            root: "/p".into(),
            path: None,
            min_severity: Severity::Warning,
            limit: DIAGNOSTICS_LIMIT,
        });
        assert_eq!(code, DIAGNOSTICS_LUA);
        assert_eq!(field(&args, "root"), Some(&path_bytes("/p")));
        assert_eq!(field(&args, "path"), None);
        assert_eq!(field(&args, "min_severity"), Some(&Value::from("warning")));
        assert_eq!(field(&args, "utf16"), Some(&Value::from(false)));
        assert_eq!(field(&args, "limit"), Some(&Value::from(DIAGNOSTICS_LIMIT)));
    }

    #[test]
    fn the_baseline_is_the_diagnostics_chunk_asked_for_everything() {
        let (code, args) = sent(EditorCall::Baseline { root: "/p".into() });
        assert_eq!(code, DIAGNOSTICS_LUA);
        assert_eq!(field(&args, "root"), Some(&path_bytes("/p")));
        assert_eq!(field(&args, "path"), None);
        assert_eq!(field(&args, "min_severity"), Some(&Value::from("hint")));
        assert_eq!(field(&args, "utf16"), Some(&Value::from(false)));
        assert_eq!(field(&args, "limit"), Some(&Value::from(BASELINE_LIMIT)));
    }

    #[test]
    fn the_baseline_goes_with_no_root_and_no_limit_of_its_own() {
        let (ops, answer) = sent_over_the_link(EditorCall::Baseline { root: "/p".into() });
        assert_eq!(
            ops,
            vec![EditorOp::AgentBaseline {
                session: "session".to_owned(),
                stamp: wire_stamp(),
            }]
        );
        assert!(matches!(answer.try_take(), Some(Err(RpcError::Unavailable(_)))));
    }

    /// The ops a [`RemoteWay`] rooted at `/p` sent for `call`, and its answer.
    fn sent_over_the_link(call: EditorCall) -> (Vec<EditorOp>, Pending) {
        let sent = Rc::new(RefCell::new(Vec::new()));
        let kept = sent.clone();
        let way = RemoteWay::new(
            Path::new("/p"),
            Box::new(move |op| {
                kept.borrow_mut().push(op);
                Pending::failed(RpcError::Unavailable("down".to_owned()))
            }),
        );
        assert_eq!(way.reach(), EditorReach::Link);
        let answer = way.call("session", &call, &stamp());
        let ops = sent.borrow().clone();
        (ops, answer)
    }

    fn stamp() -> Stamp {
        Stamp {
            deadline_ms: 9,
            scope: "s-1".to_owned(),
            seq: 3,
        }
    }

    fn wire_stamp() -> WireStamp {
        WireStamp::from(&stamp())
    }

    #[test]
    fn each_call_goes_as_its_op_with_its_path_relative_to_the_root() {
        let (ops, _) = sent_over_the_link(EditorCall::OpenFile {
            path: "/p/src/a.rs".into(),
            line: Some(4),
            end_line: Some(6),
            load: true,
        });
        assert_eq!(
            ops,
            vec![EditorOp::AgentOpen {
                session: "session".to_owned(),
                path: "src/a.rs".to_owned(),
                line: Some(4),
                end_line: Some(6),
                load: true,
                stamp: wire_stamp(),
            }]
        );

        let (ops, _) = sent_over_the_link(EditorCall::EditorState { root: "/p".into() });
        assert_eq!(
            ops,
            vec![EditorOp::AgentEditorState {
                session: "session".to_owned(),
                stamp: wire_stamp(),
            }]
        );

        let (ops, _) = sent_over_the_link(EditorCall::ReadBuffer {
            path: "/p/a.rs".into(),
            start: None,
            end: Some(9),
        });
        assert_eq!(
            ops,
            vec![EditorOp::AgentReadBuffer {
                session: "session".to_owned(),
                path: "a.rs".to_owned(),
                start: None,
                end: Some(9),
                stamp: wire_stamp(),
            }]
        );

        for (path, relative) in [(None, None), (Some("/p/src/a.rs"), Some("src/a.rs"))] {
            let (ops, _) = sent_over_the_link(EditorCall::Diagnostics {
                root: "/p".into(),
                path: path.map(PathBuf::from),
                min_severity: Severity::Warning,
                limit: DIAGNOSTICS_LIMIT,
            });
            assert_eq!(
                ops,
                vec![EditorOp::AgentDiagnostics {
                    session: "session".to_owned(),
                    path: relative.map(str::to_owned),
                    min_severity: 2,
                    limit: DIAGNOSTICS_LIMIT,
                    stamp: wire_stamp(),
                }]
            );
        }
    }

    #[test]
    fn an_ask_goes_with_its_path_relative_to_the_root_and_no_root() {
        let (ops, _) = sent_over_the_link(EditorCall::LspAsk(AskArgs {
            id: "one-1".into(),
            kind: LspKind::Usages,
            root: "/p".into(),
            path: Some("/p/src/a.rs".into()),
            line: Some(4),
            symbol: Some("frob".into()),
            include: vec![UsagePart::References, UsagePart::Implementations],
            query: None,
            query_for: Vec::new(),
            new_name: None,
            action: None,
            budget_ms: 15000,
            load: true,
        }));
        assert_eq!(
            ops,
            vec![EditorOp::AgentLspAsk {
                session: "session".to_owned(),
                id: "one-1".to_owned(),
                kind: WireLspKind::Usages,
                path: Some("src/a.rs".to_owned()),
                line: Some(4),
                symbol: Some("frob".to_owned()),
                include: vec![WireUsagePart::References, WireUsagePart::Implementations],
                query: None,
                budget_ms: 15000,
                load: true,
                stamp: wire_stamp(),
            }]
        );
    }

    #[test]
    fn a_workspace_search_goes_with_no_path() {
        let (ops, _) = sent_over_the_link(EditorCall::LspAsk(AskArgs {
            id: "one-1".into(),
            kind: LspKind::WorkspaceSymbols,
            root: "/p".into(),
            query: Some("frob".into()),
            budget_ms: 15000,
            ..Default::default()
        }));
        assert_eq!(
            ops,
            vec![EditorOp::AgentLspAsk {
                session: "session".to_owned(),
                id: "one-1".to_owned(),
                kind: WireLspKind::WorkspaceSymbols,
                path: None,
                line: None,
                symbol: None,
                include: vec![],
                query: Some("frob".to_owned()),
                budget_ms: 15000,
                load: false,
                stamp: wire_stamp(),
            }]
        );
    }

    #[test]
    fn a_collect_goes_as_its_op() {
        let (ops, _) = sent_over_the_link(EditorCall::LspCollect {
            id: "one-1".into(),
            last: true,
        });
        assert_eq!(
            ops,
            vec![EditorOp::AgentLspCollect {
                session: "session".to_owned(),
                id: "one-1".to_owned(),
                last: true,
                stamp: wire_stamp(),
            }]
        );
    }

    #[test]
    fn a_path_not_under_the_root_never_crosses_the_link() {
        let calls = [
            EditorCall::ReadBuffer {
                path: "/elsewhere/a.rs".into(),
                start: None,
                end: None,
            },
            EditorCall::OpenFile {
                path: "/pa/a.rs".into(),
                line: None,
                end_line: None,
                load: false,
            },
            EditorCall::Diagnostics {
                root: "/p".into(),
                path: Some("/elsewhere/a.rs".into()),
                min_severity: Severity::Hint,
                limit: 1,
            },
            EditorCall::LspAsk(AskArgs {
                id: "one-1".into(),
                path: Some("/elsewhere/a.rs".into()),
                ..Default::default()
            }),
        ];
        for call in calls {
            let (ops, answer) = sent_over_the_link(call);
            assert!(ops.is_empty(), "{ops:?}");
            assert!(matches!(answer.try_take(), Some(Err(RpcError::Encode(_)))));
        }
    }

    #[test]
    fn calls_that_edit_or_pick_targets_never_cross_the_link() {
        let mut calls = vec![EditorCall::LspTargets {
            paths: vec!["/p/a.rs".into()],
        }];
        for kind in [
            LspKind::Rename,
            LspKind::CodeActions,
            LspKind::ActionEdit,
            LspKind::Format,
        ] {
            calls.push(EditorCall::LspAsk(AskArgs {
                id: "one-1".into(),
                kind,
                path: Some("/p/a.rs".into()),
                ..Default::default()
            }));
        }
        for call in calls {
            let (ops, answer) = sent_over_the_link(call);
            assert!(ops.is_empty(), "{ops:?}");
            assert!(matches!(answer.try_take(), Some(Err(RpcError::Encode(_)))));
        }
    }

    #[test]
    fn a_path_that_is_not_utf8_never_crosses_the_link() {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::OsStr::from_bytes(b"/p/\xff.rs");
        let (ops, answer) = sent_over_the_link(EditorCall::ReadBuffer {
            path: name.into(),
            start: None,
            end: None,
        });
        assert!(ops.is_empty(), "{ops:?}");
        assert!(matches!(answer.try_take(), Some(Err(RpcError::Encode(_)))));
    }

    #[test]
    fn the_links_own_answer_is_the_calls_answer() {
        let (ops, answer) = sent_over_the_link(EditorCall::EditorState { root: "/p".into() });
        assert_eq!(ops.len(), 1);
        assert!(matches!(answer.try_take(), Some(Err(RpcError::Unavailable(_)))));
    }
}
