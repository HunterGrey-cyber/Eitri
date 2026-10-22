//! Window 1's side of window 2's editor operations. Window 2 never sends Lua: it names one [`EditorOp`], and
//! this table decides what that op may do in the editor. Each path is checked before anything is built (the
//! project's paths through [`ProjectDir`], a plugin's through its own work tree and then canonicalised), the
//! session or plugin must be one window 2 holds, and every argument is built here from window 1's own records.
//! The review ops send no Lua at all: they become calls on the review driver window 1 already runs, so that
//! driver's install, its one call in flight and its draining apply to window 2's calls as to its own.
//!
//! The only constant named here is the buffer-state chunk; the plugin ops go through the plugin host's own
//! functions. A scan in `core/tests/editor_rpc_scan.rs` holds this file to that.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::editor_rpc::EditorRpc;
use crate::nvim_rpc::Pending;
use crate::plugin::roots::{file_inside, Roots};
use crate::review_editor::{OverlayCall, ShowHunk, ShowMeta};
use crate::turn_review::revert::{buffer_state_args, BUFFER_STATE_LUA};
use crate::turn_review::write::ProjectDir;

use super::editor_op::EditorOp;

/// What window 1 knows that the checks need. Every answer comes from window 1's own state, never from the op.
pub trait OpContext {
    /// The canonical project root, the same in both windows.
    fn project_root(&self) -> &Path;
    /// Whether window 2 holds the chat of `session` (window 1's `session -> holder` map).
    fn session_in_window2(&self, session: &str) -> bool;
    /// The canonical work tree of a plugin whose page window 2 holds, from window 1's own plugin table; `None`
    /// otherwise.
    fn plugin_tree_in_window2(&self, plugin: &str) -> Option<PathBuf>;
    /// The paths window 1 recorded as drawn for window 2.
    fn drawn_for_window2(&self) -> Vec<PathBuf>;
}

/// What an accepted op became.
pub enum Dispatch {
    /// Calls for window 1's own review driver, to be tagged as window 2's before they are queued.
    Overlay(Vec<OverlayCall>),
    /// A Lua call window 1 sent; its answer is the op's reply.
    Rpc(Pending),
}

impl fmt::Debug for Dispatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Dispatch::Overlay(calls) => f.debug_tuple("Overlay").field(calls).finish(),
            Dispatch::Rpc(_) => f.write_str("Rpc(..)"),
        }
    }
}

/// Why an op was refused. Nothing reached the editor for any of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpRefused {
    /// The path leaves the project or the plugin's work tree, or could not be checked.
    Outside(String),
    /// The review names a session whose chat window 2 does not hold.
    SessionNotHeld(String),
    /// The plugin's page is not in window 2.
    PluginNotHeld(String),
    /// A clear of a path window 2 did not draw.
    NotDrawnByWindow2(String),
}

impl fmt::Display for OpRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpRefused::Outside(why) => f.write_str(why),
            OpRefused::SessionNotHeld(session) => write!(f, "session {session} is not held by window 2"),
            OpRefused::PluginNotHeld(plugin) => write!(f, "plugin {plugin} has no page in window 2"),
            OpRefused::NotDrawnByWindow2(path) => write!(f, "{path} was not drawn by window 2"),
        }
    }
}

impl std::error::Error for OpRefused {}

/// Checks `op` and carries it out. `id` is the `editor_op` message's id: an overlay call's request id is built
/// from it, so window 1 can route the driver's answer back to the message that asked.
pub fn dispatch(id: u64, op: EditorOp, ctx: &dyn OpContext, rpc: &dyn EditorRpc) -> Result<Dispatch, OpRefused> {
    match op {
        EditorOp::ReviewShow { path, meta, hunks } => {
            held_session(ctx, &meta.session)?;
            let path = project_path(ctx, &path)?;
            Ok(Dispatch::Overlay(vec![OverlayCall::Show {
                path,
                meta: ShowMeta::from(meta),
                hunks: hunks.into_iter().map(ShowHunk::from).collect(),
            }]))
        }
        EditorOp::ReviewOpenAndShow {
            path,
            line,
            meta,
            hunks,
        } => {
            held_session(ctx, &meta.session)?;
            let path = project_path(ctx, &path)?;
            Ok(Dispatch::Overlay(vec![OverlayCall::OpenAndShow {
                request_id: format!("w2:{id}"),
                path,
                line,
                meta: ShowMeta::from(meta),
                hunks: hunks.map(|hunks| hunks.into_iter().map(ShowHunk::from).collect()),
            }]))
        }
        EditorOp::ReviewClear { path: rel } => {
            let path = project_path(ctx, &rel)?;
            if !ctx.drawn_for_window2().contains(&path) {
                return Err(OpRefused::NotDrawnByWindow2(rel));
            }
            Ok(Dispatch::Overlay(vec![OverlayCall::Clear { path }]))
        }
        // One clear per path drawn for window 2, never a clear of everything: that would take window 1's
        // overlays with it.
        EditorOp::ReviewClearMine {} => Ok(Dispatch::Overlay(
            ctx.drawn_for_window2()
                .into_iter()
                .map(|path| OverlayCall::Clear { path })
                .collect(),
        )),
        EditorOp::BufferState { path, lines } => {
            let path = project_path(ctx, &path)?;
            Ok(Dispatch::Rpc(
                rpc.exec_lua(BUFFER_STATE_LUA, buffer_state_args(&path, lines)),
            ))
        }
        EditorOp::PluginOpen {
            plugin,
            path,
            line,
            col,
        } => {
            let tree = held_plugin(ctx, &plugin)?;
            let file = plugin_file(&plugin, &tree, &path)?;
            Ok(Dispatch::Rpc(crate::plugin::editor::open(rpc, &file, line, col)))
        }
        // The op names only the plugin; the owner the editor records and the root it applies to are window 1's
        // own: the name it found in its table and that plugin's work tree.
        EditorOp::PluginAssistApply {
            plugin,
            diagnostics,
            inlay_hints,
        } => {
            let tree = held_plugin(ctx, &plugin)?;
            Ok(Dispatch::Rpc(crate::plugin::editor::assist_apply(
                rpc,
                &plugin,
                &tree,
                diagnostics,
                inlay_hints,
            )))
        }
        EditorOp::PluginAssistClear { plugin } => {
            held_plugin(ctx, &plugin)?;
            Ok(Dispatch::Rpc(crate::plugin::editor::assist_clear(rpc, &plugin)))
        }
    }
}

fn held_session(ctx: &dyn OpContext, session: &str) -> Result<(), OpRefused> {
    if ctx.session_in_window2(session) {
        Ok(())
    } else {
        Err(OpRefused::SessionNotHeld(session.to_owned()))
    }
}

fn held_plugin(ctx: &dyn OpContext, plugin: &str) -> Result<PathBuf, OpRefused> {
    ctx.plugin_tree_in_window2(plugin)
        .ok_or_else(|| OpRefused::PluginNotHeld(plugin.to_owned()))
}

/// `rel` checked against the directory at `root` the way turn review checks its own paths: plain names only (no
/// absolute path, `..`, `.`, empty component or `.git`), and no link or non-directory among the parents, each
/// opened from the one before without following a link. The last component is not looked at here.
fn checked_under(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let dir = ProjectDir::open(root).map_err(|e| format!("{} cannot be opened: {e}", root.display()))?;
    dir.target(Path::new(rel), false).map_err(|e| e.to_string())?;
    Ok(root.join(rel))
}

/// A project path from window 2, as window 1's absolute path. Its last component may be missing (a file a turn
/// deleted) but may not be a link: the editor follows one, so a link inside the project would name a file anywhere,
/// and turn review itself never follows a link in that place either.
fn project_path(ctx: &dyn OpContext, rel: &str) -> Result<PathBuf, OpRefused> {
    let path = checked_under(ctx.project_root(), rel).map_err(OpRefused::Outside)?;
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(OpRefused::Outside(format!(
            "{rel} is not inside the project: it is a link"
        ))),
        Ok(_) => Ok(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Err(e) => Err(OpRefused::Outside(format!("{rel} cannot be looked at: {e}"))),
    }
}

/// A file of a plugin's work tree, canonical. The lexical and parent checks come first; then the path is
/// canonicalised with the tree as the only root, which resolves the last component too, so a link there that
/// leads out of the tree, or to anything but a regular file, is refused. The canonical path is what the editor
/// is given, never the joined one, so nothing can be swapped in under the name after the check.
fn plugin_file(plugin: &str, tree: &Path, rel: &str) -> Result<PathBuf, OpRefused> {
    let outside = |why: String| OpRefused::Outside(format!("in the work tree of {plugin}: {why}"));
    let joined = checked_under(tree, rel).map_err(outside)?;
    let raw = joined
        .to_str()
        .ok_or_else(|| outside(format!("{} is not UTF-8", joined.display())))?;
    let roots = Roots::new(BTreeMap::from([(plugin.to_owned(), tree.to_path_buf())]));
    file_inside(&roots, raw).map_err(|refusal| outside(refusal.message(raw)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvim_rpc::RpcError;
    use crate::plugin::editor::{PLUGIN_ASSIST_LUA, PLUGIN_OPEN_LUA};
    use crate::test_scratch_dir::ScratchDir;
    use crate::window_link::editor_op::{WireEol, WireHunk, WireLine, WireMeta, WireScope};
    use rmpv::Value;
    use std::cell::RefCell;
    use std::os::unix::fs::symlink;

    /// Records every Lua call and answers that there is no editor.
    #[derive(Default)]
    struct Recorder(RefCell<Vec<(&'static str, Vec<Value>)>>);

    impl EditorRpc for Recorder {
        fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
            self.0.borrow_mut().push((code, args));
            Pending::failed(RpcError::Unavailable("recorder".into()))
        }
        fn target(&self) -> Option<u64> {
            Some(1)
        }
    }

    impl Recorder {
        fn calls(&self) -> Vec<(&'static str, Vec<Value>)> {
            self.0.borrow().clone()
        }
    }

    struct Ctx {
        root: PathBuf,
        sessions: Vec<String>,
        plugins: BTreeMap<String, PathBuf>,
        drawn: Vec<PathBuf>,
    }

    impl OpContext for Ctx {
        fn project_root(&self) -> &Path {
            &self.root
        }
        fn session_in_window2(&self, session: &str) -> bool {
            self.sessions.iter().any(|s| s == session)
        }
        fn plugin_tree_in_window2(&self, plugin: &str) -> Option<PathBuf> {
            self.plugins.get(plugin).cloned()
        }
        fn drawn_for_window2(&self) -> Vec<PathBuf> {
            self.drawn.clone()
        }
    }

    /// A project with a source file, a `.git`, a link `out -> /etc`, a link `src/secret.rs` to a file outside the
    /// project, a link `src/alias.rs` to its neighbour, and a plugin work tree inside it holding a
    /// file, a link to that file, a link to a file outside the project and a link to `/etc`.
    struct Fixture {
        _dir: ScratchDir,
        ctx: Ctx,
        tree: PathBuf,
    }

    fn fixture(label: &str) -> Fixture {
        let dir = ScratchDir::new("nv-editor-ops", label);
        let base = std::fs::canonicalize(&*dir).unwrap();
        let root = base.join("project");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("a/b"), "b\n").unwrap();
        std::fs::write(root.join(".git/config"), "[core]\n").unwrap();
        symlink("/etc", root.join("out")).unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();
        std::fs::write(base.join("outside/secret.rs"), "secret\n").unwrap();
        symlink(base.join("outside/secret.rs"), root.join("src/secret.rs")).unwrap();
        symlink("a.rs", root.join("src/alias.rs")).unwrap();
        let tree = root.join("work/snotra");
        std::fs::create_dir_all(tree.join("ch01")).unwrap();
        std::fs::write(tree.join("real.rs"), "fn main() {}\n").unwrap();
        std::fs::write(tree.join("ch01/main.rs"), "fn main() {}\n").unwrap();
        symlink("real.rs", tree.join("alias.rs")).unwrap();
        symlink(base.join("outside/secret.rs"), tree.join("leak.rs")).unwrap();
        symlink("/etc", tree.join("dir")).unwrap();
        let ctx = Ctx {
            root: root.clone(),
            sessions: vec!["s-held".into()],
            plugins: BTreeMap::from([("snotra".to_string(), tree.clone())]),
            drawn: vec![],
        };
        Fixture { _dir: dir, ctx, tree }
    }

    fn meta(session: &str) -> WireMeta {
        WireMeta {
            tab: 3,
            session: session.into(),
            turn: 2,
            scope: WireScope::Turn,
        }
    }

    fn hunk() -> WireHunk {
        WireHunk {
            id: 1,
            old_start: 1,
            old_len: 1,
            new_start: 1,
            new_len: 1,
            old_lines: vec![WireLine {
                text: b"fn b() {}".to_vec(),
                eol: WireEol::Lf,
            }],
            new_lines: vec![WireLine {
                text: b"fn a() {}".to_vec(),
                eol: WireEol::Lf,
            }],
        }
    }

    fn show(path: &str, session: &str) -> EditorOp {
        EditorOp::ReviewShow {
            path: path.into(),
            meta: meta(session),
            hunks: vec![hunk()],
        }
    }

    fn open_and_show(path: &str, session: &str) -> EditorOp {
        EditorOp::ReviewOpenAndShow {
            path: path.into(),
            line: Some(1),
            meta: meta(session),
            hunks: Some(vec![hunk()]),
        }
    }

    fn plugin_open(plugin: &str, path: &str) -> EditorOp {
        EditorOp::PluginOpen {
            plugin: plugin.into(),
            path: path.into(),
            line: Some(2),
            col: None,
        }
    }

    fn overlay(result: Result<Dispatch, OpRefused>) -> Vec<OverlayCall> {
        match result {
            Ok(Dispatch::Overlay(calls)) => calls,
            other => panic!("expected overlay calls, got {other:?}"),
        }
    }

    fn opened_path(args: &[Value]) -> Value {
        args[0]
            .as_map()
            .unwrap()
            .iter()
            .find(|(k, _)| k.as_str() == Some("path"))
            .unwrap()
            .1
            .clone()
    }

    #[test]
    fn ops_naming_paths_outside_the_project_or_the_plugin_tree_are_refused() {
        let f = fixture("outside");
        let rpc = Recorder::default();
        for path in [
            "../x",
            "/etc/passwd",
            "out/passwd",
            "a//b",
            ".git/config",
            "",
            "./src/a.rs",
            "src/../a/b",
            // A link as the last component: one leading out, one to a directory, and one that stays inside, which
            // is refused too, since what it names can change after the check.
            "src/secret.rs",
            "out",
            "src/alias.rs",
        ] {
            let ops = [
                show(path, "s-held"),
                open_and_show(path, "s-held"),
                EditorOp::ReviewClear { path: path.into() },
                EditorOp::BufferState {
                    path: path.into(),
                    lines: None,
                },
            ];
            for op in ops {
                let name = op.name();
                let result = dispatch(1, op, &f.ctx, &rpc);
                assert!(
                    matches!(result, Err(OpRefused::Outside(_))),
                    "{name} of {path:?} was not refused as outside: {result:?}"
                );
            }
        }
        for path in ["../../src/a.rs", "/etc/passwd", "ch01//main.rs", "./real.rs", ""] {
            let result = dispatch(1, plugin_open("snotra", path), &f.ctx, &rpc);
            assert!(
                matches!(result, Err(OpRefused::Outside(_))),
                "plugin_open of {path:?} was not refused as outside: {result:?}"
            );
        }
        assert!(rpc.calls().is_empty(), "a refused op reached the editor");
    }

    #[test]
    fn a_plugin_file_that_is_a_link_out_of_the_tree_is_refused() {
        let f = fixture("leak");
        let rpc = Recorder::default();
        for path in ["leak.rs", "dir/passwd", "ch01", "missing.rs"] {
            let result = dispatch(1, plugin_open("snotra", path), &f.ctx, &rpc);
            assert!(
                matches!(result, Err(OpRefused::Outside(_))),
                "plugin_open of {path:?} was not refused as outside: {result:?}"
            );
        }
        assert!(rpc.calls().is_empty(), "a refused op reached the editor");
    }

    #[test]
    fn a_plugin_file_is_opened_by_its_canonical_path() {
        let f = fixture("alias");
        let rpc = Recorder::default();
        assert!(matches!(
            dispatch(1, plugin_open("snotra", "alias.rs"), &f.ctx, &rpc),
            Ok(Dispatch::Rpc(_))
        ));
        let calls = rpc.calls();
        assert_eq!(calls.len(), 1);
        assert!(std::ptr::eq(calls[0].0, PLUGIN_OPEN_LUA));
        let real = f.tree.join("real.rs");
        assert_eq!(
            opened_path(&calls[0].1),
            Value::Binary(real.as_os_str().as_encoded_bytes().to_vec()),
            "the editor must be given the file the link resolved to, not the link"
        );
    }

    #[test]
    fn the_overlay_request_id_is_built_from_the_messages_id() {
        let f = fixture("request-id");
        let calls = overlay(dispatch(
            42,
            open_and_show("src/a.rs", "s-held"),
            &f.ctx,
            &Recorder::default(),
        ));
        match calls.as_slice() {
            [OverlayCall::OpenAndShow { request_id, .. }] => assert_eq!(request_id, "w2:42"),
            other => panic!("expected one open-and-show, got {other:?}"),
        }
    }

    #[test]
    fn a_review_op_for_a_session_window_2_does_not_hold_is_refused() {
        let f = fixture("session");
        let rpc = Recorder::default();
        for op in [show("src/a.rs", "s-other"), open_and_show("src/a.rs", "s-other")] {
            assert_eq!(
                dispatch(1, op, &f.ctx, &rpc).unwrap_err(),
                OpRefused::SessionNotHeld("s-other".into())
            );
        }
        assert!(rpc.calls().is_empty());
    }

    #[test]
    fn review_clear_mine_clears_only_window_2s_paths_and_never_all() {
        let mut f = fixture("clear-mine");
        let mine = vec![f.ctx.root.join("src/a.rs"), f.ctx.root.join("a/b")];
        f.ctx.drawn = mine.clone();
        let rpc = Recorder::default();
        let calls = overlay(dispatch(1, EditorOp::ReviewClearMine {}, &f.ctx, &rpc));
        assert_eq!(
            calls,
            mine.iter()
                .map(|path| OverlayCall::Clear { path: path.clone() })
                .collect::<Vec<_>>()
        );
        assert!(
            !calls.contains(&OverlayCall::ClearAll),
            "a clear of everything takes window 1's overlays"
        );

        f.ctx.drawn.clear();
        assert_eq!(overlay(dispatch(1, EditorOp::ReviewClearMine {}, &f.ctx, &rpc)), vec![]);

        // One path at a time: only a path recorded for window 2.
        f.ctx.drawn = vec![f.ctx.root.join("a/b")];
        assert_eq!(
            dispatch(
                1,
                EditorOp::ReviewClear {
                    path: "src/a.rs".into()
                },
                &f.ctx,
                &rpc
            )
            .unwrap_err(),
            OpRefused::NotDrawnByWindow2("src/a.rs".into())
        );
        assert_eq!(
            overlay(dispatch(1, EditorOp::ReviewClear { path: "a/b".into() }, &f.ctx, &rpc)),
            vec![OverlayCall::Clear {
                path: f.ctx.root.join("a/b")
            }]
        );
        assert!(rpc.calls().is_empty(), "the review ops send no Lua of their own");
    }

    #[test]
    fn review_ops_become_overlay_calls_with_window_1s_absolute_paths_and_a_w2_request_id() {
        let f = fixture("overlay");
        let rpc = Recorder::default();
        let abs = f.ctx.root.join("src/a.rs");
        assert_eq!(
            overlay(dispatch(5, show("src/a.rs", "s-held"), &f.ctx, &rpc)),
            vec![OverlayCall::Show {
                path: abs.clone(),
                meta: ShowMeta::from(meta("s-held")),
                hunks: vec![ShowHunk::from(hunk())],
            }]
        );
        assert_eq!(
            overlay(dispatch(6, open_and_show("src/a.rs", "s-held"), &f.ctx, &rpc)),
            vec![OverlayCall::OpenAndShow {
                request_id: "w2:6".into(),
                path: abs.clone(),
                line: Some(1),
                meta: ShowMeta::from(meta("s-held")),
                hunks: Some(vec![ShowHunk::from(hunk())]),
            }]
        );
        // A file not on disk yet (a turn's new file under a new directory) still names its place in the project.
        let only_open = EditorOp::ReviewOpenAndShow {
            path: "new/dir/b.rs".into(),
            line: None,
            meta: meta("s-held"),
            hunks: None,
        };
        assert_eq!(
            overlay(dispatch(7, only_open, &f.ctx, &rpc)),
            vec![OverlayCall::OpenAndShow {
                request_id: "w2:7".into(),
                path: f.ctx.root.join("new/dir/b.rs"),
                line: None,
                meta: ShowMeta::from(meta("s-held")),
                hunks: None,
            }]
        );
        assert!(rpc.calls().is_empty(), "the review ops send no Lua of their own");
    }

    #[test]
    fn buffer_state_and_plugin_ops_send_exactly_their_one_constant_with_window_1s_arguments() {
        let f = fixture("constants");
        let rpc = Recorder::default();
        let op = EditorOp::BufferState {
            path: "src/a.rs".into(),
            lines: Some((3, 2)),
        };
        assert!(matches!(dispatch(1, op, &f.ctx, &rpc), Ok(Dispatch::Rpc(_))));
        let op = plugin_open("snotra", "ch01/main.rs");
        assert!(matches!(dispatch(2, op, &f.ctx, &rpc), Ok(Dispatch::Rpc(_))));
        let op = EditorOp::PluginAssistApply {
            plugin: "snotra".into(),
            diagnostics: true,
            inlay_hints: false,
        };
        assert!(matches!(dispatch(3, op, &f.ctx, &rpc), Ok(Dispatch::Rpc(_))));
        let op = EditorOp::PluginAssistClear {
            plugin: "snotra".into(),
        };
        assert!(matches!(dispatch(4, op, &f.ctx, &rpc), Ok(Dispatch::Rpc(_))));

        // What window 1's own callers would send for the same file and plugin.
        let expected = Recorder::default();
        let _ = expected.exec_lua(
            BUFFER_STATE_LUA,
            buffer_state_args(&f.ctx.root.join("src/a.rs"), Some((3, 2))),
        );
        let _ = crate::plugin::editor::open(&expected, &f.tree.join("ch01/main.rs"), Some(2), None);
        let _ = crate::plugin::editor::assist_apply(&expected, "snotra", &f.tree, true, false);
        let _ = crate::plugin::editor::assist_clear(&expected, "snotra");

        let calls = rpc.calls();
        let expected = expected.calls();
        assert_eq!(calls.len(), 4, "one call per op");
        assert_eq!(calls[0].0, BUFFER_STATE_LUA);
        assert!(std::ptr::eq(calls[1].0, PLUGIN_OPEN_LUA));
        assert!(std::ptr::eq(calls[2].0, PLUGIN_ASSIST_LUA));
        assert!(std::ptr::eq(calls[3].0, PLUGIN_ASSIST_LUA));
        // Each assist call carries its place in the order this process made them, so the dispatched call and the
        // later one made here differ there and only there; it must still be the earlier of the two.
        for (got, want) in calls.iter().zip(&expected) {
            let (got_args, got_order) = without_order(&got.1);
            let (want_args, want_order) = without_order(&want.1);
            assert_eq!(got_args, want_args, "the arguments differ from window 1's own call");
            assert_eq!(
                got_order.is_some(),
                want_order.is_some(),
                "only one of the two calls is ordered"
            );
            if let (Some(got_order), Some(want_order)) = (got_order, want_order) {
                assert!(
                    got_order < want_order,
                    "the dispatched call is not ordered before the later one"
                );
            }
        }
    }

    /// The arguments with an option map's `order` taken out, and that order.
    fn without_order(args: &[Value]) -> (Vec<Value>, Option<u64>) {
        let mut order = None;
        let args = args
            .iter()
            .map(|arg| match arg {
                Value::Map(entries) => Value::Map(
                    entries
                        .iter()
                        .filter(|(key, value)| {
                            if key.as_str() == Some("order") {
                                order = value.as_u64();
                                false
                            } else {
                                true
                            }
                        })
                        .cloned()
                        .collect(),
                ),
                other => other.clone(),
            })
            .collect();
        (args, order)
    }

    #[test]
    fn a_plugin_window_2_does_not_hold_is_refused() {
        let f = fixture("plugin-held");
        let rpc = Recorder::default();
        let ops = [
            plugin_open("other", "real.rs"),
            EditorOp::PluginAssistApply {
                plugin: "other".into(),
                diagnostics: true,
                inlay_hints: true,
            },
            EditorOp::PluginAssistClear { plugin: "other".into() },
        ];
        for op in ops {
            assert_eq!(
                dispatch(1, op, &f.ctx, &rpc).unwrap_err(),
                OpRefused::PluginNotHeld("other".into())
            );
        }
        assert!(rpc.calls().is_empty(), "a refused op reached the editor");
    }
}
