//! Window 1's side of window 2's editor operations. Window 2 never sends Lua: it names one [`EditorOp`], and
//! this table decides what that op may do in the editor. Each path is checked before anything is built (the
//! project's paths through [`ProjectDir`], a plugin's through its own work tree and then canonicalised), the
//! session or plugin must be one window 2 holds, and every argument is built here from window 1's own records.
//! The review ops send no Lua at all: they become calls on the review driver window 1 already runs, so that
//! driver's install, its one call in flight and its draining apply to window 2's calls as to its own.
//!
//! An agent op comes from a window-2 tab whose broker has already judged the call. Window 2's answers are taken for
//! nothing here: the session, each path, the stamp and, for a load, the file are all checked again, so this window
//! admits only project paths that follow no link, however the tab's own judgement went.
//!
//! The only constants named here are the buffer-state chunk and the standalone shown question a live note asks;
//! the plugin ops go through the plugin host's own functions and the agent ops through their tools' own `send`,
//! `ask` or `collect`. A scan in `core/tests/editor_rpc_scan.rs` holds this file to that.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::editor_rpc::EditorRpc;
use crate::editor_tools::stamp::{wall_ms, Stamp};
use crate::nvim_rpc::Pending;
use crate::plugin::roots::{file_inside, Roots};
use crate::review_editor::{
    linked_request_id, live_shown_args, LiveMeta, OverlayCall, ShowHunk, ShowMeta, LIVE_SHOWN_LUA, MAX_SHOWN_PATHS,
};
use crate::turn_review::live_contest::TurnKey;
use crate::turn_review::revert::{buffer_state_args, BUFFER_STATE_LUA};
use crate::turn_review::write::ProjectDir;
use crate::wm::PeerId;

use super::editor_op::{severity_from_wire, EditorOp, WireLspKind, WireStamp};

/// The most diagnostics window 1 answers one op with, whatever it asks.
const DIAGNOSTICS_LIMIT_MAX: u32 = 2000;

/// The longest id an ask goes under, the limit its chunk holds an ask to.
const LSP_ID_MAX: usize = 40;

/// The longest symbol window 1 hands a language server, as a tool's own argument is held to.
const SYMBOL_MAX: usize = 256;

/// The longest query window 1 hands a language server, as a tool's own argument is held to.
const QUERY_MAX: usize = 1024;

/// The most per-client queries an ask carries, and the longest client name among them.
const QUERY_FOR_MAX: usize = 8;
const CLIENT_NAME_MAX: usize = 64;

/// The longest an ask's entry may stay in the editor: a language server's budget and the last collect's margin.
const LSP_LIFE_MAX: Duration =
    crate::editor_tools::tools::lsp::LSP_BUDGET.saturating_add(crate::editor_tools::tools::lsp::COLLECT_MARGIN);

/// What window 1 knows that the checks need. Every answer comes from window 1's own state, never from the op.
pub trait OpContext {
    /// The canonical project root, the same in both windows.
    fn project_root(&self) -> &Path;
    /// The window the operation came from, which is who an open's answer goes back to.
    fn peer(&self) -> PeerId;
    /// Whether window 2 holds the chat of `session` (window 1's `session -> holder` map).
    fn session_in_window2(&self, session: &str) -> bool;
    /// The named canonical roots of a plugin whose page window 2 holds, from window 1's own table; `None`
    /// otherwise.
    fn plugin_roots_in_window2(&self, plugin: &str) -> Option<BTreeMap<String, PathBuf>>;
    /// Follow installation data from this window's registration and its own feed socket.
    fn plugin_follow(&self, _plugin: &str) -> Option<FollowConfig> {
        None
    }
    /// The paths window 1 recorded as drawn for the asking window.
    fn drawn_for_window2(&self) -> Vec<PathBuf>;
}

/// Values read from window 1's plugin registration, never supplied by a linked page.
#[derive(Clone)]
pub struct FollowConfig {
    pub socket: PathBuf,
    pub scratch: Option<PathBuf>,
    pub filetypes: Vec<String>,
    pub commands: Vec<(String, &'static str)>,
    pub auto: bool,
}

/// A linked window's turn and the files it edited, for window 1's driver to note as that window's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveNote {
    pub key: TurnKey,
    pub started_ns: u64,
    pub ended_ns: Option<u64>,
    /// Window 1's absolute paths.
    pub paths: Vec<PathBuf>,
}

/// What an accepted op became.
pub enum Dispatch {
    /// Calls for window 1's own review driver, to be tagged as window 2's before they are queued.
    Overlay(Vec<OverlayCall>),
    /// A Lua call window 1 sent; its answer is the op's reply.
    Rpc(Pending),
    /// A note for window 1's driver, and, while the turn runs, the editor's answer to which of its files a
    /// window shows, which is the op's reply.
    LiveNote { note: LiveNote, shown: Option<Pending> },
    /// A plugin page's file, still to be checked on disk. [`OpenCheck::run`] reads the filesystem, which can stall on
    /// a hung mount, so the caller runs it off the GTK thread and asks the editor with [`CheckedOpen::open`].
    Check(OpenCheck),
}

/// A plugin page's open as window 1 checks it: what to look at on disk, taken from window 1's own records and the
/// op. Plain data, so it can go to a worker thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCheck {
    plugin: String,
    target: OpenTarget,
    line: Option<u32>,
    col: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OpenTarget {
    /// `rel` in the work tree `tree` of the plugin.
    Tree { tree: PathBuf, rel: String },
    /// The canonical path a document's link resolved to in window 2. It must still be that, unchanged (a directory
    /// on the way swapped for a link since moves it, and is refused rather than followed), and a regular file of
    /// that document's project, found by the rule window 2's own check used ([`crate::plugin::doc_route::project_dir`]
    /// with this window's root and home); a sender that named no document is held to this window's root.
    Link {
        path: String,
        document: Option<PathBuf>,
        window_root: PathBuf,
        home: Option<PathBuf>,
    },
}

/// A checked plugin open: a canonical regular file, and where the cursor goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedOpen {
    file: PathBuf,
    line: Option<u32>,
    col: Option<u32>,
}

impl OpenCheck {
    /// Looks at the filesystem: call it on a worker thread, never on the GTK thread.
    pub fn run(self) -> Result<CheckedOpen, OpRefused> {
        let OpenCheck {
            plugin,
            target,
            line,
            col,
        } = self;
        let file = match target {
            OpenTarget::Tree { tree, rel } => plugin_file(&plugin, &tree, &rel)?,
            OpenTarget::Link {
                path,
                document,
                window_root,
                home,
            } => {
                let project = match document {
                    Some(doc) => crate::plugin::doc_route::project_dir(&doc, Some(&window_root), home.as_deref())
                        .ok_or_else(|| OpRefused::Outside(format!("{} has no project to open from", doc.display())))?,
                    None => window_root,
                };
                let roots = Roots::new(BTreeMap::from([("project".into(), project)]));
                let file = file_inside(&roots, &path).map_err(|e| OpRefused::Outside(e.message(&path)))?;
                if file != Path::new(&path) {
                    return Err(OpRefused::Outside(format!("{path} no longer resolves to itself")));
                }
                file
            }
        };
        Ok(CheckedOpen { file, line, col })
    }

    /// The plugin whose page asked: the asking window must still hold it when the check is done.
    pub fn plugin(&self) -> &str {
        &self.plugin
    }
}

impl CheckedOpen {
    /// The canonical file the check found.
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// Asks the editor to open the checked file: the plugin host's own open, whose snippet checks the file again.
    pub fn open(&self, rpc: &dyn EditorRpc) -> Pending {
        crate::plugin::editor::open(rpc, &self.file, self.line, self.col)
    }
}

impl fmt::Debug for Dispatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Dispatch::Overlay(calls) => f.debug_tuple("Overlay").field(calls).finish(),
            Dispatch::Rpc(_) => f.write_str("Rpc(..)"),
            Dispatch::LiveNote { note, shown } => f
                .debug_struct("LiveNote")
                .field("note", note)
                .field("shown", &shown.as_ref().map(|_| ".."))
                .finish(),
            Dispatch::Check(check) => f.debug_tuple("Check").field(check).finish(),
        }
    }
}

/// Why an op was refused. Nothing reached the editor for any of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpRefused {
    /// The path leaves the project or the plugin's work tree or could not be checked, or a value of the op is not one
    /// this window takes.
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
                request_id: linked_request_id(ctx.peer(), id),
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
        // One clear per path drawn for this window, never a clear of everything: that would take every other
        // window's overlays with it.
        EditorOp::ReviewClearMine {} => Ok(Dispatch::Overlay(
            ctx.drawn_for_window2()
                .into_iter()
                .map(|path| OverlayCall::Clear { path })
                .collect(),
        )),
        // The paths are checked by their text alone: this runs on window 1's GTK thread for up to a batch of
        // paths, and nvim answers which are shown from its own buffer list, opening nothing.
        EditorOp::ReviewLiveNote {
            session,
            turn_id,
            started_ns,
            ended_ns,
            paths,
        } => {
            held_session(ctx, &session)?;
            if paths.len() > MAX_SHOWN_PATHS {
                return Err(OpRefused::Outside(format!(
                    "a note names {} paths, more than {MAX_SHOWN_PATHS}",
                    paths.len()
                )));
            }
            let paths = paths
                .iter()
                .map(|rel| lexical_project_path(ctx, rel))
                .collect::<Result<Vec<_>, _>>()?;
            // Only a running turn's edits may still be drawn: an ended one's note asks nothing.
            let shown = (ended_ns.is_none() && !paths.is_empty())
                .then(|| rpc.exec_lua(LIVE_SHOWN_LUA, live_shown_args(&paths)));
            Ok(Dispatch::LiveNote {
                note: LiveNote {
                    key: TurnKey { session, turn_id },
                    started_ns,
                    ended_ns,
                    paths,
                },
                shown,
            })
        }
        EditorOp::ReviewLiveShow { path, meta, hunks } => {
            held_session(ctx, &meta.session)?;
            let path = project_path(ctx, &path)?;
            Ok(Dispatch::Overlay(vec![OverlayCall::LiveShow {
                path,
                meta: LiveMeta::from(meta),
                hunks: hunks.into_iter().map(ShowHunk::from).collect(),
            }]))
        }
        // Names no session: window 1's driver takes off only a live drawing this window made, and a live clear
        // leaves any other drawing there.
        EditorOp::ReviewLiveClear { path } => {
            let path = project_path(ctx, &path)?;
            Ok(Dispatch::Overlay(vec![OverlayCall::LiveClear { path }]))
        }
        EditorOp::BufferState { path, lines } => {
            let path = project_path(ctx, &path)?;
            Ok(Dispatch::Rpc(
                rpc.exec_lua(BUFFER_STATE_LUA, buffer_state_args(&path, lines)),
            ))
        }
        EditorOp::PluginOpen {
            plugin,
            root,
            path,
            line,
            col,
        } => {
            let roots = held_plugin_roots(ctx, &plugin)?;
            let tree = roots
                .get(&root)
                .ok_or_else(|| OpRefused::Outside(format!("plugin {plugin} has no declared root named {root}")))?;
            Ok(Dispatch::Check(OpenCheck {
                plugin,
                target: OpenTarget::Tree {
                    tree: tree.clone(),
                    rel: path,
                },
                line,
                col,
            }))
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
        EditorOp::PluginInstall { plugin } => {
            held_plugin(ctx, &plugin)?;
            let config = ctx
                .plugin_follow(&plugin)
                .ok_or_else(|| OpRefused::PluginNotHeld(plugin.clone()))?;
            Ok(Dispatch::Rpc(crate::plugin::editor::follow_install(
                rpc,
                &crate::plugin::editor::FollowInstall {
                    owner: &plugin,
                    socket: &config.socket,
                    scratch_dir: config.scratch.as_deref(),
                    filetypes: &config.filetypes,
                    commands: &config.commands,
                    auto: config.auto,
                },
            )))
        }
        EditorOp::PluginFollow { plugin, on } => {
            held_plugin(ctx, &plugin)?;
            Ok(Dispatch::Rpc(crate::plugin::editor::follow_on(rpc, &plugin, on)))
        }
        EditorOp::PluginText { plugin, buf } => {
            held_plugin(ctx, &plugin)?;
            // The bounds window 1 holds a following page of its own to: window 2's request names none.
            Ok(Dispatch::Rpc(crate::plugin::editor::follow_text(
                rpc,
                &plugin,
                buf,
                crate::plugin::follow::MAX_TEXT_BYTES,
                crate::plugin::follow::MAX_TEXT_LINES,
            )))
        }
        EditorOp::PluginReveal {
            plugin,
            buf,
            line,
            token,
            cursor,
            row,
        } => {
            held_plugin(ctx, &plugin)?;
            if line == 0 {
                return Err(OpRefused::Outside("line starts at one".into()));
            }
            if cursor == Some(0) {
                return Err(OpRefused::Outside("cursor starts at one".into()));
            }
            if row == Some(0) {
                return Err(OpRefused::Outside("row starts at one".into()));
            }
            if row.is_some() && cursor.is_none() {
                return Err(OpRefused::Outside("row needs a cursor".into()));
            }
            Ok(Dispatch::Rpc(crate::plugin::editor::follow_reveal(
                rpc, &plugin, buf, line, token, cursor, row,
            )))
        }
        EditorOp::PluginJump {
            plugin,
            buf,
            line,
            col,
            select,
        } => {
            held_plugin(ctx, &plugin)?;
            if line == 0 {
                return Err(OpRefused::Outside("line starts at one".into()));
            }
            if select.is_some_and(|s| s.line == 0) {
                return Err(OpRefused::Outside("select.line starts at one".into()));
            }
            Ok(Dispatch::Rpc(crate::plugin::editor::follow_jump(
                rpc, &plugin, buf, line, col, select,
            )))
        }
        EditorOp::PluginOpenLink {
            plugin,
            path,
            line,
            document,
        } => {
            held_plugin(ctx, &plugin)?;
            let document = match document {
                Some(doc) if Path::new(&doc).is_absolute() => Some(PathBuf::from(doc)),
                Some(doc) => return Err(OpRefused::Outside(format!("{doc:?} is not an absolute document path"))),
                None => None,
            };
            Ok(Dispatch::Check(OpenCheck {
                plugin,
                target: OpenTarget::Link {
                    path,
                    document,
                    window_root: ctx.project_root().to_owned(),
                    home: std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from),
                },
                line,
                col: None,
            }))
        }
        EditorOp::PluginAssistClear { plugin } => {
            held_plugin(ctx, &plugin)?;
            Ok(Dispatch::Rpc(crate::plugin::editor::assist_clear(rpc, &plugin)))
        }
        EditorOp::AgentOpen {
            session,
            path,
            line,
            end_line,
            load,
            stamp,
        } => {
            held_session(ctx, &session)?;
            let path = project_path(ctx, &path)?;
            let stamp = checked_stamp(stamp)?;
            // Loading is what could block on a FIFO or take in a huge file, and window 2 looked at the file a while
            // ago and from another process: only what is there now counts.
            if load {
                crate::editor_tools::paths::file_check(&path).map_err(|refused| OpRefused::Outside(refused.0))?;
            }
            Ok(Dispatch::Rpc(crate::editor_tools::tools::open_file::send(
                rpc, &stamp, &path, line, end_line, load,
            )))
        }
        EditorOp::AgentEditorState { session, stamp } => {
            held_session(ctx, &session)?;
            let stamp = checked_stamp(stamp)?;
            Ok(Dispatch::Rpc(crate::editor_tools::tools::editor_state::send(
                rpc,
                &stamp,
                ctx.project_root(),
            )))
        }
        EditorOp::AgentReadBuffer {
            session,
            path,
            start,
            end,
            stamp,
        } => {
            held_session(ctx, &session)?;
            let path = project_path(ctx, &path)?;
            let stamp = checked_stamp(stamp)?;
            Ok(Dispatch::Rpc(crate::editor_tools::tools::read_buffer::send(
                rpc, &stamp, &path, start, end,
            )))
        }
        EditorOp::AgentDiagnostics {
            session,
            path,
            min_severity,
            limit,
            stamp,
        } => {
            held_session(ctx, &session)?;
            let path = path.map(|path| project_path(ctx, &path)).transpose()?;
            let stamp = checked_stamp(stamp)?;
            let min_severity = severity_from_wire(min_severity)
                .ok_or_else(|| OpRefused::Outside(format!("{min_severity} is not a severity")))?;
            Ok(Dispatch::Rpc(crate::editor_tools::tools::diagnostics::send(
                rpc,
                &stamp,
                &crate::editor_tools::tools::diagnostics::DiagnosticsArgs {
                    root: ctx.project_root().to_owned(),
                    path,
                    min_severity,
                    utf16: false,
                    limit: limit.min(DIAGNOSTICS_LIMIT_MAX),
                },
            )))
        }
        EditorOp::AgentLspAsk {
            session,
            id,
            kind,
            path,
            line,
            symbol,
            include,
            query,
            query_for,
            budget_ms,
            load,
            stamp,
        } => {
            held_session(ctx, &session)?;
            let stamp = checked_stamp(stamp)?;
            checked_id(&id)?;
            checked_text("symbol", symbol.as_deref(), SYMBOL_MAX)?;
            checked_text("query", query.as_deref(), QUERY_MAX)?;
            if query_for.len() > QUERY_FOR_MAX {
                return Err(OpRefused::Outside(format!(
                    "an ask carries at most {QUERY_FOR_MAX} per-client queries"
                )));
            }
            for (client, query) in &query_for {
                checked_text("client", Some(client), CLIENT_NAME_MAX)?;
                checked_text("query", Some(query), QUERY_MAX)?;
            }
            if u128::from(budget_ms) > LSP_LIFE_MAX.as_millis() {
                return Err(OpRefused::Outside(format!("{budget_ms} ms is past an ask's limit")));
            }
            if path.is_none() && kind != WireLspKind::WorkspaceSymbols {
                return Err(OpRefused::Outside("only a workspace search names no file".into()));
            }
            let path = path.map(|path| project_path(ctx, &path)).transpose()?;
            // As for `AgentOpen`: what window 2 saw a while ago says nothing of what a load would read now.
            if let (true, Some(path)) = (load, &path) {
                crate::editor_tools::paths::file_check(path).map_err(|refused| OpRefused::Outside(refused.0))?;
            }
            Ok(Dispatch::Rpc(crate::editor_tools::tools::lsp::ask(
                rpc,
                &stamp,
                &crate::editor_tools::tools::lsp::AskArgs {
                    id,
                    kind: kind.into(),
                    root: ctx.project_root().to_owned(),
                    path,
                    line,
                    symbol,
                    include: include.into_iter().map(Into::into).collect(),
                    query,
                    query_for,
                    // The edits never cross the link, so the wire ask carries neither.
                    new_name: None,
                    action: None,
                    budget_ms,
                    load,
                },
            )))
        }
        EditorOp::AgentLspCollect {
            session,
            id,
            last,
            stamp,
        } => {
            held_session(ctx, &session)?;
            let stamp = checked_stamp(stamp)?;
            checked_id(&id)?;
            Ok(Dispatch::Rpc(crate::editor_tools::tools::lsp::collect(
                rpc, &stamp, &id, last,
            )))
        }
        // The arguments of window 1's own `EditorCall::Baseline`, and nothing of them is the op's.
        EditorOp::AgentBaseline { session, stamp } => {
            held_session(ctx, &session)?;
            let stamp = checked_stamp(stamp)?;
            Ok(Dispatch::Rpc(crate::editor_tools::tools::diagnostics::send(
                rpc,
                &stamp,
                &crate::editor_tools::tools::diagnostics::DiagnosticsArgs {
                    root: ctx.project_root().to_owned(),
                    path: None,
                    min_severity: crate::editor_tools::tools::diagnostics::Severity::Hint,
                    utf16: false,
                    limit: crate::editor_tools::tools::baseline::BASELINE_LIMIT,
                },
            )))
        }
    }
}

/// Window 2's stamp, held to this window's limits; the chunk is given only what comes back.
fn checked_stamp(stamp: WireStamp) -> Result<Stamp, OpRefused> {
    Stamp::from(stamp).checked(wall_ms()).map_err(OpRefused::Outside)
}

/// An ask's id, the shape its chunk takes: it becomes part of a key in the editor's table of asks, and a collect's
/// chunk does not look at it.
fn checked_id(id: &str) -> Result<(), OpRefused> {
    let shaped = !id.is_empty()
        && id.len() <= LSP_ID_MAX
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if shaped {
        Ok(())
    } else {
        Err(OpRefused::Outside(format!("an ask's id has the wrong shape: {id:?}")))
    }
}

/// Text on its way to a language server, held to what the tools hold an agent's to.
fn checked_text(name: &str, text: Option<&str>, max: usize) -> Result<(), OpRefused> {
    match text {
        Some(text) if text.len() > max => Err(OpRefused::Outside(format!("{name} is over {max} bytes"))),
        Some(text) if text.chars().any(char::is_control) => {
            Err(OpRefused::Outside(format!("{name} may not hold a control character")))
        }
        _ => Ok(()),
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
    held_plugin_roots(ctx, plugin)?
        .into_values()
        .next()
        .ok_or_else(|| OpRefused::PluginNotHeld(plugin.to_owned()))
}

fn held_plugin_roots(ctx: &dyn OpContext, plugin: &str) -> Result<BTreeMap<String, PathBuf>, OpRefused> {
    ctx.plugin_roots_in_window2(plugin)
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

/// A project path from window 2 by its text alone: relative, plain names only, no NUL. Nothing on disk is looked at.
fn lexical_project_path(ctx: &dyn OpContext, rel: &str) -> Result<PathBuf, OpRefused> {
    let plain = !rel.is_empty()
        && !rel.contains('\0')
        && Path::new(rel).components().all(|c| matches!(c, Component::Normal(_)));
    if plain {
        Ok(ctx.project_root().join(rel))
    } else {
        Err(OpRefused::Outside(format!("{rel:?} is not a path inside the project")))
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
    use crate::editor_tools::stamp::EDITOR_BUDGET_MAX;
    use crate::editor_tools::tools::baseline::BASELINE_LIMIT;
    use crate::editor_tools::tools::diagnostics::{self, DiagnosticsArgs, Severity, DIAGNOSTICS_LIMIT};
    use crate::editor_tools::tools::lsp::{self, AskArgs, LspKind, UsagePart, COLLECT_MARGIN, LSP_BUDGET};
    use crate::editor_tools::tools::{editor_state, open_file, read_buffer};
    use crate::editor_tools::way::{EditorCall, EditorWay, LocalWay};
    use crate::nvim_rpc::RpcError;
    use crate::plugin::editor::{PLUGIN_ASSIST_LUA, PLUGIN_OPEN_LUA};
    use crate::test_scratch_dir::ScratchDir;
    use crate::window_link::editor_op::{
        WireEol, WireHunk, WireLine, WireLiveMeta, WireMeta, WireScope, WireUsagePart,
    };
    use rmpv::Value;
    use std::cell::RefCell;
    use std::os::unix::fs::symlink;
    use std::rc::Rc;

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
        plugins: BTreeMap<String, BTreeMap<String, PathBuf>>,
        drawn: Vec<PathBuf>,
    }

    impl OpContext for Ctx {
        fn project_root(&self) -> &Path {
            &self.root
        }
        fn peer(&self) -> PeerId {
            PeerId(2)
        }
        fn session_in_window2(&self, session: &str) -> bool {
            self.sessions.iter().any(|s| s == session)
        }
        fn plugin_roots_in_window2(&self, plugin: &str) -> Option<BTreeMap<String, PathBuf>> {
            self.plugins.get(plugin).cloned()
        }
        fn plugin_follow(&self, _plugin: &str) -> Option<FollowConfig> {
            Some(FollowConfig {
                socket: self.root.join("feed"),
                scratch: None,
                filetypes: vec!["markdown".into()],
                commands: vec![("MarkdownPreview".into(), "toggle")],
                auto: true,
            })
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
        let second_tree = root.join("work/second");
        std::fs::create_dir_all(second_tree.join("src")).unwrap();
        std::fs::write(second_tree.join("src/lib.rs"), "fn second() {}\n").unwrap();
        symlink(tree.join("real.rs"), second_tree.join("leak.rs")).unwrap();
        let ctx = Ctx {
            root: root.clone(),
            sessions: vec!["s-held".into()],
            plugins: BTreeMap::from([(
                "snotra".to_string(),
                BTreeMap::from([("a".into(), tree.clone()), ("b".into(), second_tree)]),
            )]),
            drawn: vec![],
        };
        Fixture { _dir: dir, ctx, tree }
    }

    #[test]
    fn window_2_has_no_op_that_takes_the_follow_snippet_down() {
        // The snippet's autocmds and commands (`:MarkdownPreview`) are window 1's: a page ending in window 2 turns
        // its following off and leaves them.
        let teardown = serde_json::json!({ "op": "plugin_teardown", "plugin": "snotra" });
        assert!(serde_json::from_value::<EditorOp>(teardown).is_err());
    }

    #[test]
    fn the_markdown_previews_host_calls_from_window_2_go_through_typed_editor_ops() {
        let f = fixture("markdown-typed");
        let rpc = Recorder::default();
        let ops = [
            EditorOp::PluginText {
                plugin: "snotra".into(),
                buf: 3,
            },
            EditorOp::PluginReveal {
                plugin: "snotra".into(),
                buf: 3,
                line: 2,
                token: 9,
                cursor: None,
                row: None,
            },
            EditorOp::PluginJump {
                plugin: "snotra".into(),
                buf: 3,
                line: 2,
                col: 1,
                select: None,
            },
            EditorOp::PluginJump {
                plugin: "snotra".into(),
                buf: 3,
                line: 2,
                col: 1,
                select: Some(crate::plugin::protocol::JumpSelect {
                    line: 4,
                    col: 3,
                    mode: crate::plugin::protocol::SelectMode::Char,
                }),
            },
            EditorOp::PluginFollow {
                plugin: "snotra".into(),
                on: true,
            },
            EditorOp::PluginInstall {
                plugin: "snotra".into(),
            },
            EditorOp::PluginOpenLink {
                plugin: "snotra".into(),
                path: f.ctx.root.join("src/a.rs").to_str().unwrap().into(),
                line: Some(2),
                document: None,
            },
        ];
        for op in ops {
            let name = op.name();
            let value = serde_json::to_value(&op).unwrap();
            assert_eq!(value["op"], name);
            let back = serde_json::from_value(value).unwrap();
            assert!(matches!(dispatch_now(1, back, &f.ctx, &rpc), Ok(Dispatch::Rpc(_))));
        }
        let calls = rpc.0.borrow();
        assert_eq!(calls.len(), 7);
        assert!(calls[..6]
            .iter()
            .all(|(code, _)| std::ptr::eq(*code, crate::plugin::editor::PLUGIN_FOLLOW_LUA)));
        assert!(std::ptr::eq(calls[6].0, crate::plugin::editor::PLUGIN_OPEN_LUA));
        // The selection goes on to the snippet as its three fields.
        let field = |key: &str| {
            calls[3].1[1]
                .as_map()
                .unwrap()
                .iter()
                .find(|(k, _)| k.as_str() == Some(key))
                .map(|(_, v)| v.clone())
        };
        assert_eq!(
            (field("select_line"), field("select_col"), field("select_mode")),
            (Some(4.into()), Some(3.into()), Some("char".into()))
        );
        assert_eq!(
            calls[0].1[1]
                .as_map()
                .unwrap()
                .iter()
                .find(|(k, _)| k.as_str() == Some("max_lines"))
                .unwrap()
                .1
                .as_u64(),
            Some(50_000)
        );
        drop(calls);
        for op in [
            EditorOp::PluginText {
                plugin: "absent".into(),
                buf: 3,
            },
            EditorOp::PluginReveal {
                plugin: "snotra".into(),
                buf: 3,
                line: 0,
                token: 1,
                cursor: None,
                row: None,
            },
            EditorOp::PluginReveal {
                plugin: "snotra".into(),
                buf: 3,
                line: 2,
                token: 1,
                cursor: Some(0),
                row: None,
            },
            EditorOp::PluginOpenLink {
                plugin: "snotra".into(),
                path: "/etc/passwd".into(),
                line: None,
                document: None,
            },
            EditorOp::PluginJump {
                plugin: "snotra".into(),
                buf: 3,
                line: 2,
                col: 1,
                select: Some(crate::plugin::protocol::JumpSelect {
                    line: 0,
                    col: 1,
                    mode: crate::plugin::protocol::SelectMode::Line,
                }),
            },
        ] {
            assert!(dispatch_now(1, op, &f.ctx, &rpc).is_err());
        }
        assert_eq!(rpc.0.borrow().len(), 7, "a refused call never reaches nvim");
        for op in [
            r#"{"op":"plugin_text","plugin":"snotra","buf":3,"lua":"return 1"}"#,
            r#"{"op":"plugin_install","plugin":"snotra","socket":"/foreign"}"#,
            r#"{"op":"plugin_jump","plugin":"snotra","buf":3,"line":2,"col":1,"select":{"line":2,"col":1,"mode":"block"}}"#,
            r#"{"op":"plugin_jump","plugin":"snotra","buf":3,"line":2,"col":1,"select":{"line":2,"col":1,"mode":"char","x":1}}"#,
        ] {
            assert!(serde_json::from_str::<EditorOp>(op).is_err());
        }
    }

    #[test]
    fn a_reveals_row_must_start_at_one_and_have_a_cursor() {
        let f = fixture("reveal-row");
        let rpc = Recorder::default();
        for (cursor, row, why) in [
            (Some(5), Some(0), "row starts at one"),
            (None, Some(9), "row needs a cursor"),
        ] {
            let op = EditorOp::PluginReveal {
                plugin: "snotra".into(),
                buf: 3,
                line: 2,
                token: 1,
                cursor,
                row,
            };
            assert!(matches!(dispatch_now(1, op, &f.ctx, &rpc), Err(OpRefused::Outside(message)) if message == why));
        }
        assert!(rpc.0.borrow().is_empty(), "a refused row never reaches nvim");
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

    /// [`dispatch`], with a plugin open's file check run at once, as the shell's worker runs it, and the editor
    /// then asked.
    fn dispatch_now(id: u64, op: EditorOp, ctx: &dyn OpContext, rpc: &dyn EditorRpc) -> Result<Dispatch, OpRefused> {
        match dispatch(id, op, ctx, rpc)? {
            Dispatch::Check(check) => Ok(Dispatch::Rpc(check.run()?.open(rpc))),
            other => Ok(other),
        }
    }

    fn plugin_open(plugin: &str, path: &str) -> EditorOp {
        EditorOp::PluginOpen {
            plugin: plugin.into(),
            root: "a".into(),
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
    fn a_plugin_open_is_looked_at_on_disk_only_by_its_check() {
        let f = fixture("open-check-later");
        let rpc = Recorder::default();
        let link = |path: &str| EditorOp::PluginOpenLink {
            plugin: "snotra".into(),
            path: path.into(),
            line: None,
            document: None,
        };
        for op in [plugin_open("snotra", "leak.rs"), link("/etc/passwd")] {
            // What a dispatch on the GTK thread decides from window 1's own records alone: nothing refused yet.
            let Ok(Dispatch::Check(check)) = dispatch(1, op, &f.ctx, &rpc) else {
                panic!("a plugin open is handed on to its check");
            };
            assert!(matches!(check.run(), Err(OpRefused::Outside(_))));
        }
        assert!(rpc.calls().is_empty());
        let Ok(Dispatch::Check(check)) = dispatch(1, plugin_open("snotra", "real.rs"), &f.ctx, &rpc) else {
            panic!("a plugin open is handed on to its check");
        };
        assert_eq!(
            check.run().unwrap().file(),
            std::fs::canonicalize(f.tree.join("real.rs")).unwrap()
        );
        // A plugin window 2 does not hold is refused at once: that needs no disk.
        assert_eq!(
            dispatch(1, plugin_open("other", "real.rs"), &f.ctx, &rpc).unwrap_err(),
            OpRefused::PluginNotHeld("other".into())
        );
    }

    #[test]
    fn a_forwarded_link_is_checked_against_its_documents_project() {
        let f = fixture("link-document");
        let rpc = Recorder::default();
        // A document of another repository, open in this window's editor: its links reach that repository.
        let other = f.ctx.root.parent().unwrap().join("other");
        std::fs::create_dir_all(other.join(".git")).unwrap();
        std::fs::write(other.join("doc.md"), "[x](x.rs)\n").unwrap();
        std::fs::write(other.join("x.rs"), "x\n").unwrap();
        let link = |path: &Path, document: Option<&Path>| EditorOp::PluginOpenLink {
            plugin: "snotra".into(),
            path: path.to_str().unwrap().into(),
            line: Some(1),
            document: document.map(|d| d.to_str().unwrap().into()),
        };
        let doc = other.join("doc.md");
        assert!(matches!(
            dispatch_now(1, link(&other.join("x.rs"), Some(&doc)), &f.ctx, &rpc),
            Ok(Dispatch::Rpc(_))
        ));
        assert_eq!(
            opened_path(&rpc.calls().last().unwrap().1),
            Value::Binary(other.join("x.rs").as_os_str().as_encoded_bytes().to_vec())
        );
        // Not this window's project, then: a file of it is outside that document's.
        assert!(matches!(
            dispatch_now(1, link(&f.ctx.root.join("src/a.rs"), Some(&doc)), &f.ctx, &rpc),
            Err(OpRefused::Outside(_))
        ));
        // A directory on the way swapped for a link since window 2's check: the path no longer names itself, and the
        // file the link leads to now is not opened in its place.
        let elsewhere = f.ctx.root.parent().unwrap().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("x.rs"), "elsewhere\n").unwrap();
        symlink(&elsewhere, other.join("sub")).unwrap();
        let refused = dispatch_now(
            1,
            link(&other.join("sub/x.rs"), Some(&other.join("sub/doc.md"))),
            &f.ctx,
            &rpc,
        );
        assert!(
            matches!(&refused, Err(OpRefused::Outside(why)) if why.contains("no longer resolves to itself")),
            "{refused:?}"
        );
        // A sender that names no document is held to this window's root, as before.
        assert!(matches!(
            dispatch_now(1, link(&other.join("x.rs"), None), &f.ctx, &rpc),
            Err(OpRefused::Outside(_))
        ));
        assert!(matches!(
            dispatch(1, link(&other.join("x.rs"), Some(Path::new("doc.md"))), &f.ctx, &rpc),
            Err(OpRefused::Outside(_))
        ));
        assert_eq!(rpc.calls().len(), 1);
    }

    #[test]
    fn plugin_open_uses_the_named_root_even_when_the_relative_path_exists_in_both() {
        let f = fixture("named-root");
        let rpc = Recorder::default();
        let second = &f.ctx.plugins["snotra"]["b"];
        for in_first in [false, true] {
            if in_first {
                std::fs::create_dir_all(f.tree.join("src")).unwrap();
                std::fs::write(f.tree.join("src/lib.rs"), "fn first() {}\n").unwrap();
            }
            let op = EditorOp::PluginOpen {
                plugin: "snotra".into(),
                root: "b".into(),
                path: "src/lib.rs".into(),
                line: Some(2),
                col: None,
            };
            assert!(matches!(dispatch_now(1, op, &f.ctx, &rpc), Ok(Dispatch::Rpc(_))));
            assert_eq!(
                opened_path(&rpc.calls().last().unwrap().1),
                Value::Binary(
                    std::fs::canonicalize(second.join("src/lib.rs"))
                        .unwrap()
                        .as_os_str()
                        .as_encoded_bytes()
                        .to_vec()
                )
            );
        }
    }

    #[test]
    fn plugin_open_refuses_unknown_roots_and_escapes_from_the_named_root() {
        let f = fixture("named-root-outside");
        let rpc = Recorder::default();
        for (root, path) in [("unknown", "src/lib.rs"), ("b", "../snotra/real.rs"), ("b", "leak.rs")] {
            let op = EditorOp::PluginOpen {
                plugin: "snotra".into(),
                root: root.into(),
                path: path.into(),
                line: None,
                col: None,
            };
            let result = dispatch_now(1, op, &f.ctx, &rpc);
            assert!(matches!(result, Err(OpRefused::Outside(_))), "{result:?}");
            if root == "unknown" {
                assert!(result
                    .unwrap_err()
                    .to_string()
                    .contains("no declared root named unknown"));
            }
        }
        assert!(rpc.calls().is_empty());
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
                let result = dispatch_now(1, op, &f.ctx, &rpc);
                assert!(
                    matches!(result, Err(OpRefused::Outside(_))),
                    "{name} of {path:?} was not refused as outside: {result:?}"
                );
            }
        }
        for path in ["../../src/a.rs", "/etc/passwd", "ch01//main.rs", "./real.rs", ""] {
            let result = dispatch_now(1, plugin_open("snotra", path), &f.ctx, &rpc);
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
            let result = dispatch_now(1, plugin_open("snotra", path), &f.ctx, &rpc);
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
            dispatch_now(1, plugin_open("snotra", "alias.rs"), &f.ctx, &rpc),
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
        let calls = overlay(dispatch_now(
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
                dispatch_now(1, op, &f.ctx, &rpc).unwrap_err(),
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
        let calls = overlay(dispatch_now(1, EditorOp::ReviewClearMine {}, &f.ctx, &rpc));
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
        assert_eq!(
            overlay(dispatch_now(1, EditorOp::ReviewClearMine {}, &f.ctx, &rpc)),
            vec![]
        );

        // One path at a time: only a path recorded for window 2.
        f.ctx.drawn = vec![f.ctx.root.join("a/b")];
        assert_eq!(
            dispatch_now(
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
            overlay(dispatch_now(
                1,
                EditorOp::ReviewClear { path: "a/b".into() },
                &f.ctx,
                &rpc
            )),
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
            overlay(dispatch_now(5, show("src/a.rs", "s-held"), &f.ctx, &rpc)),
            vec![OverlayCall::Show {
                path: abs.clone(),
                meta: ShowMeta::from(meta("s-held")),
                hunks: vec![ShowHunk::from(hunk())],
            }]
        );
        assert_eq!(
            overlay(dispatch_now(6, open_and_show("src/a.rs", "s-held"), &f.ctx, &rpc)),
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
            overlay(dispatch_now(7, only_open, &f.ctx, &rpc)),
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
        assert!(matches!(dispatch_now(1, op, &f.ctx, &rpc), Ok(Dispatch::Rpc(_))));
        let op = plugin_open("snotra", "ch01/main.rs");
        assert!(matches!(dispatch_now(2, op, &f.ctx, &rpc), Ok(Dispatch::Rpc(_))));
        let op = EditorOp::PluginAssistApply {
            plugin: "snotra".into(),
            diagnostics: true,
            inlay_hints: false,
        };
        assert!(matches!(dispatch_now(3, op, &f.ctx, &rpc), Ok(Dispatch::Rpc(_))));
        let op = EditorOp::PluginAssistClear {
            plugin: "snotra".into(),
        };
        assert!(matches!(dispatch_now(4, op, &f.ctx, &rpc), Ok(Dispatch::Rpc(_))));

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
                dispatch_now(1, op, &f.ctx, &rpc).unwrap_err(),
                OpRefused::PluginNotHeld("other".into())
            );
        }
        assert!(rpc.calls().is_empty(), "a refused op reached the editor");
    }

    const SCOPE: &str = "abc-3-1";

    fn stamp() -> WireStamp {
        WireStamp {
            deadline_ms: wall_ms() + 1000,
            scope: SCOPE.into(),
            seq: 4,
        }
    }

    fn agent_open(session: &str, path: &str, load: bool, stamp: WireStamp) -> EditorOp {
        EditorOp::AgentOpen {
            session: session.into(),
            path: path.into(),
            line: Some(3),
            end_line: Some(5),
            load,
            stamp,
        }
    }

    fn agent_read(session: &str, path: &str) -> EditorOp {
        EditorOp::AgentReadBuffer {
            session: session.into(),
            path: path.into(),
            start: Some(1),
            end: None,
            stamp: stamp(),
        }
    }

    fn agent_state(session: &str) -> EditorOp {
        EditorOp::AgentEditorState {
            session: session.into(),
            stamp: stamp(),
        }
    }

    fn agent_diagnostics(session: &str, path: Option<&str>) -> EditorOp {
        EditorOp::AgentDiagnostics {
            session: session.into(),
            path: path.map(str::to_owned),
            min_severity: 2,
            limit: DIAGNOSTICS_LIMIT,
            stamp: stamp(),
        }
    }

    /// A `usages` ask of `path`, as a tab's run makes it.
    fn lsp_ask(session: &str, path: &str, load: bool, stamp: WireStamp) -> EditorOp {
        EditorOp::AgentLspAsk {
            session: session.into(),
            id: "abc-3-1".into(),
            kind: WireLspKind::Usages,
            path: Some(path.into()),
            line: Some(3),
            symbol: Some("frob".into()),
            include: vec![WireUsagePart::Definitions, WireUsagePart::References],
            query: None,
            query_for: Vec::new(),
            budget_ms: LSP_LIFE_MAX.as_millis() as u32,
            load,
            stamp,
        }
    }

    fn lsp_search(query: &str, id: &str) -> EditorOp {
        EditorOp::AgentLspAsk {
            session: "s-held".into(),
            id: id.into(),
            kind: WireLspKind::WorkspaceSymbols,
            path: None,
            line: None,
            symbol: None,
            include: vec![],
            query: Some(query.into()),
            query_for: Vec::new(),
            budget_ms: 1000,
            load: false,
            stamp: stamp(),
        }
    }

    fn lsp_collect(session: &str, id: &str) -> EditorOp {
        EditorOp::AgentLspCollect {
            session: session.into(),
            id: id.into(),
            last: true,
            stamp: stamp(),
        }
    }

    fn agent_baseline(session: &str) -> EditorOp {
        EditorOp::AgentBaseline {
            session: session.into(),
            stamp: stamp(),
        }
    }

    /// One recorded call's stamp, read back from what the chunk would get.
    fn sent_stamp(args: &[Value]) -> (u64, String) {
        let field = |name: &str| {
            args[0]
                .as_map()
                .unwrap()
                .iter()
                .find(|(k, _)| k.as_str() == Some(name))
                .unwrap()
                .1
                .clone()
        };
        (
            field("deadline_ms").as_u64().unwrap(),
            field("scope").as_str().unwrap().to_owned(),
        )
    }

    #[test]
    fn an_agent_open_runs_its_chunk_on_window_1s_path() {
        let f = fixture("agent-open");
        let rpc = Recorder::default();
        let wire = stamp();
        let op = agent_open("s-held", "src/a.rs", true, wire.clone());
        assert!(matches!(dispatch_now(1, op, &f.ctx, &rpc), Ok(Dispatch::Rpc(_))));
        let expected = Recorder::default();
        let _ = open_file::send(
            &expected,
            &Stamp::from(wire),
            &f.ctx.root.join("src/a.rs"),
            Some(3),
            Some(5),
            true,
        );
        assert_eq!(rpc.calls(), expected.calls());
        assert_eq!(rpc.calls()[0].0, open_file::OPEN_FILE_LUA);
    }

    #[test]
    fn each_read_op_runs_its_tools_own_chunk_with_window_1s_values() {
        let f = fixture("agent-reads");
        let rpc = Recorder::default();
        let path = f.ctx.root.join("src/a.rs");
        let wire = stamp();
        let local = Stamp::from(wire.clone());
        for op in [
            EditorOp::AgentEditorState {
                session: "s-held".into(),
                stamp: wire.clone(),
            },
            EditorOp::AgentReadBuffer {
                session: "s-held".into(),
                path: "src/a.rs".into(),
                start: Some(2),
                end: Some(4),
                stamp: wire.clone(),
            },
            EditorOp::AgentDiagnostics {
                session: "s-held".into(),
                path: Some("src/a.rs".into()),
                min_severity: 2,
                limit: DIAGNOSTICS_LIMIT,
                stamp: wire.clone(),
            },
        ] {
            assert!(matches!(dispatch_now(1, op, &f.ctx, &rpc), Ok(Dispatch::Rpc(_))));
        }
        let expected = Recorder::default();
        let _ = editor_state::send(&expected, &local, &f.ctx.root);
        let _ = read_buffer::send(&expected, &local, &path, Some(2), Some(4));
        let _ = diagnostics::send(
            &expected,
            &local,
            &DiagnosticsArgs {
                root: f.ctx.root.clone(),
                path: Some(path.clone()),
                min_severity: Severity::Warning,
                utf16: false,
                limit: DIAGNOSTICS_LIMIT,
            },
        );
        assert_eq!(rpc.calls(), expected.calls());
    }

    #[test]
    fn a_file_not_on_disk_can_still_be_asked_about_as_a_buffer() {
        let f = fixture("agent-missing");
        let rpc = Recorder::default();
        for op in [
            agent_open("s-held", "new/b.rs", false, stamp()),
            agent_read("s-held", "new/b.rs"),
            agent_diagnostics("s-held", Some("new/b.rs")),
        ] {
            assert!(matches!(dispatch_now(1, op, &f.ctx, &rpc), Ok(Dispatch::Rpc(_))));
        }
        assert_eq!(rpc.calls().len(), 3);
    }

    /// Every op that names `path` is refused as outside, and nothing reaches the editor.
    fn refused_as_outside(f: &Fixture, path: &str) {
        let rpc = Recorder::default();
        for op in [
            agent_open("s-held", path, false, stamp()),
            agent_open("s-held", path, true, stamp()),
            agent_read("s-held", path),
            agent_diagnostics("s-held", Some(path)),
            lsp_ask("s-held", path, false, stamp()),
            lsp_ask("s-held", path, true, stamp()),
        ] {
            let name = op.name();
            let result = dispatch_now(1, op, &f.ctx, &rpc);
            assert!(
                matches!(result, Err(OpRefused::Outside(_))),
                "{name} of {path:?}: {result:?}"
            );
        }
        assert!(rpc.calls().is_empty(), "a refused op reached the editor");
    }

    #[test]
    fn an_outside_path_is_refused_in_window_1() {
        let f = fixture("agent-outside");
        for path in [
            "../x",
            "/etc/passwd",
            "a//b",
            ".git/config",
            "",
            "./src/a.rs",
            "src/../a/b",
        ] {
            refused_as_outside(&f, path);
        }
    }

    #[test]
    fn a_link_is_refused_in_window_1() {
        let f = fixture("agent-link");
        for path in ["out/passwd", "out", "src/secret.rs", "src/alias.rs"] {
            refused_as_outside(&f, path);
        }
    }

    #[test]
    fn a_session_window_1_holds_is_refused() {
        let f = fixture("agent-session");
        let rpc = Recorder::default();
        for op in [
            agent_open("s-other", "src/a.rs", true, stamp()),
            agent_state("s-other"),
            agent_read("s-other", "src/a.rs"),
            agent_diagnostics("s-other", None),
            lsp_ask("s-other", "src/a.rs", true, stamp()),
            agent_baseline("s-other"),
        ] {
            assert_eq!(
                dispatch_now(1, op, &f.ctx, &rpc).unwrap_err(),
                OpRefused::SessionNotHeld("s-other".into())
            );
        }
        assert!(rpc.calls().is_empty(), "a refused op reached the editor");
    }

    #[test]
    fn a_scope_of_the_wrong_shape_is_refused() {
        let f = fixture("agent-scope");
        let rpc = Recorder::default();
        let bad = || WireStamp {
            scope: "Not A Scope".into(),
            ..stamp()
        };
        for op in [
            agent_open("s-held", "src/a.rs", false, bad()),
            EditorOp::AgentEditorState {
                session: "s-held".into(),
                stamp: bad(),
            },
            EditorOp::AgentReadBuffer {
                session: "s-held".into(),
                path: "src/a.rs".into(),
                start: None,
                end: None,
                stamp: bad(),
            },
            EditorOp::AgentDiagnostics {
                session: "s-held".into(),
                path: None,
                min_severity: 1,
                limit: 1,
                stamp: bad(),
            },
        ] {
            let result = dispatch_now(1, op, &f.ctx, &rpc);
            assert!(matches!(result, Err(OpRefused::Outside(_))), "{result:?}");
        }
        assert!(rpc.calls().is_empty(), "a refused op reached the editor");
    }

    #[test]
    fn a_far_deadline_is_held_to_the_limit() {
        let f = fixture("agent-deadline");
        let rpc = Recorder::default();
        let before = wall_ms();
        let far = WireStamp {
            deadline_ms: u64::MAX,
            ..stamp()
        };
        assert!(matches!(
            dispatch_now(1, agent_open("s-held", "src/a.rs", false, far), &f.ctx, &rpc),
            Ok(Dispatch::Rpc(_))
        ));
        let after = wall_ms();
        let (deadline, scope) = sent_stamp(&rpc.calls()[0].1);
        let limit = EDITOR_BUDGET_MAX.as_millis() as u64;
        assert!(
            (before + limit..=after + limit).contains(&deadline),
            "{deadline} is not {limit} ms from now"
        );
        assert_eq!(scope, SCOPE);
    }

    #[test]
    fn a_fifo_named_with_load_is_refused() {
        let f = fixture("agent-fifo");
        let path = f.ctx.root.join("src/pipe");
        let name = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let rpc = Recorder::default();
        let result = dispatch_now(1, agent_open("s-held", "src/pipe", true, stamp()), &f.ctx, &rpc);
        assert!(matches!(result, Err(OpRefused::Outside(_))), "{result:?}");
        assert!(rpc.calls().is_empty(), "a refused op reached the editor");
        // Without a load the chunk only looks among the editor's buffers, so no file is touched.
        assert!(matches!(
            dispatch_now(2, agent_open("s-held", "src/pipe", false, stamp()), &f.ctx, &rpc),
            Ok(Dispatch::Rpc(_))
        ));
        let calls = rpc.calls().len();
        let result = dispatch_now(3, lsp_ask("s-held", "src/pipe", true, stamp()), &f.ctx, &rpc);
        assert!(matches!(result, Err(OpRefused::Outside(_))), "{result:?}");
        assert_eq!(rpc.calls().len(), calls, "a refused op reached the editor");
        assert!(matches!(
            dispatch_now(4, lsp_ask("s-held", "src/pipe", false, stamp()), &f.ctx, &rpc),
            Ok(Dispatch::Rpc(_))
        ));
    }

    #[test]
    fn an_lsp_ask_runs_its_chunk_on_window_1s_path() {
        let f = fixture("agent-lsp");
        let rpc = Recorder::default();
        let wire = stamp();
        let op = lsp_ask("s-held", "src/a.rs", true, wire.clone());
        assert!(matches!(dispatch_now(1, op, &f.ctx, &rpc), Ok(Dispatch::Rpc(_))));
        let expected = Recorder::default();
        let _ = lsp::ask(
            &expected,
            &Stamp::from(wire),
            &AskArgs {
                id: "abc-3-1".into(),
                kind: LspKind::Usages,
                root: f.ctx.root.clone(),
                path: Some(f.ctx.root.join("src/a.rs")),
                line: Some(3),
                symbol: Some("frob".into()),
                include: vec![UsagePart::Definitions, UsagePart::References],
                query: None,
                query_for: Vec::new(),
                new_name: None,
                action: None,
                budget_ms: (LSP_BUDGET + COLLECT_MARGIN).as_millis() as u32,
                load: true,
            },
        );
        assert_eq!(rpc.calls(), expected.calls());
        assert_eq!(rpc.calls()[0].0, lsp::LSP_ASK_LUA);
    }

    #[test]
    fn per_client_queries_cross_the_link_as_bounded_data() {
        let f = fixture("agent-client-queries");
        let valid = vec![
            ("gopls".to_owned(), "Type.Name".to_owned()),
            ("rust-analyzer".into(), "Name#".into()),
        ];
        for (queries, allowed) in [
            (valid, true),
            (vec![("gopls".into(), "x".repeat(QUERY_MAX)); QUERY_FOR_MAX], true),
            (vec![("gopls".into(), "Name".into()); QUERY_FOR_MAX + 1], false),
            (vec![("x".repeat(CLIENT_NAME_MAX + 1), "Name".into())], false),
            (vec![("gopls".into(), "x".repeat(QUERY_MAX + 1))], false),
            (vec![("gopls\n".into(), "Name".into())], false),
            (vec![("gopls".into(), "Name\n".into())], false),
        ] {
            let rpc = Recorder::default();
            let mut op = lsp_search("Name", "gd-1");
            if let EditorOp::AgentLspAsk { query_for, .. } = &mut op {
                *query_for = queries.clone();
            }
            let result = dispatch_now(1, op, &f.ctx, &rpc);
            assert_eq!(result.is_ok(), allowed, "{result:?}");
            if allowed {
                let expected = Value::Map(
                    queries
                        .into_iter()
                        .map(|(client, query)| (client.into(), query.into()))
                        .collect(),
                );
                assert_eq!(sent_field(&rpc.calls()[0].1, "query_for"), expected);
                assert_eq!(sent_field(&rpc.calls()[0].1, "load"), Value::Boolean(false));
            } else {
                assert!(rpc.calls().is_empty());
            }
        }
    }
    #[test]
    fn each_kind_reaches_the_chunk_under_its_own_word() {
        let f = fixture("agent-lsp-kinds");
        for (kind, word) in [
            (WireLspKind::Usages, "usages"),
            (WireLspKind::Hover, "hover"),
            (WireLspKind::DocumentSymbols, "document_symbols"),
            (WireLspKind::WorkspaceSymbols, "workspace_symbols"),
        ] {
            let rpc = Recorder::default();
            let op = EditorOp::AgentLspAsk {
                session: "s-held".into(),
                id: "abc-3-1".into(),
                kind,
                path: Some("src/a.rs".into()),
                line: None,
                symbol: None,
                include: vec![],
                query: None,
                query_for: Vec::new(),
                budget_ms: 1000,
                load: false,
                stamp: stamp(),
            };
            assert!(matches!(dispatch_now(1, op, &f.ctx, &rpc), Ok(Dispatch::Rpc(_))));
            assert_eq!(sent_field(&rpc.calls()[0].1, "kind").as_str(), Some(word));
        }
    }

    #[test]
    fn an_lsp_collect_runs_its_chunk() {
        let f = fixture("agent-collect");
        let rpc = Recorder::default();
        let wire = stamp();
        let op = EditorOp::AgentLspCollect {
            session: "s-held".into(),
            id: "abc-3-1".into(),
            last: true,
            stamp: wire.clone(),
        };
        assert!(matches!(dispatch_now(1, op, &f.ctx, &rpc), Ok(Dispatch::Rpc(_))));
        let expected = Recorder::default();
        let _ = lsp::collect(&expected, &Stamp::from(wire), "abc-3-1", true);
        assert_eq!(rpc.calls(), expected.calls());
        assert_eq!(rpc.calls()[0].0, lsp::LSP_COLLECT_LUA);
    }

    #[test]
    fn a_collect_for_a_session_it_does_not_hold_is_refused() {
        let f = fixture("agent-collect-session");
        let rpc = Recorder::default();
        assert_eq!(
            dispatch_now(1, lsp_collect("s-other", "abc-3-1"), &f.ctx, &rpc).unwrap_err(),
            OpRefused::SessionNotHeld("s-other".into())
        );
        assert!(rpc.calls().is_empty(), "a refused op reached the editor");
    }

    #[test]
    fn an_id_of_the_wrong_shape_is_refused() {
        let f = fixture("agent-id");
        let rpc = Recorder::default();
        let longest = "a".repeat(LSP_ID_MAX);
        for id in [longest.as_str(), "0123abcdef-7-1"] {
            assert!(matches!(
                dispatch_now(1, lsp_collect("s-held", id), &f.ctx, &rpc),
                Ok(Dispatch::Rpc(_))
            ));
        }
        let accepted = rpc.calls().len();
        let long = "a".repeat(LSP_ID_MAX + 1);
        for id in ["", "Abc", "a_b", "a b", "a:b", "a\nb", "é", long.as_str()] {
            for op in [lsp_collect("s-held", id), lsp_search("frob", id)] {
                let result = dispatch_now(1, op, &f.ctx, &rpc);
                assert!(matches!(result, Err(OpRefused::Outside(_))), "{id:?}: {result:?}");
            }
        }
        assert_eq!(rpc.calls().len(), accepted, "a refused op reached the editor");
    }

    #[test]
    fn a_budget_past_its_limit_is_refused() {
        let f = fixture("agent-budget");
        let rpc = Recorder::default();
        let limit = (LSP_BUDGET + COLLECT_MARGIN).as_millis() as u32;
        let with_budget = |budget_ms: u32| EditorOp::AgentLspAsk {
            session: "s-held".into(),
            id: "abc-3-1".into(),
            kind: WireLspKind::Hover,
            path: Some("src/a.rs".into()),
            line: Some(1),
            symbol: Some("a".into()),
            include: vec![],
            query: None,
            query_for: Vec::new(),
            budget_ms,
            load: false,
            stamp: stamp(),
        };
        assert!(matches!(
            dispatch_now(1, with_budget(limit), &f.ctx, &rpc),
            Ok(Dispatch::Rpc(_))
        ));
        for budget_ms in [limit + 1, u32::MAX] {
            let result = dispatch_now(1, with_budget(budget_ms), &f.ctx, &rpc);
            assert!(matches!(result, Err(OpRefused::Outside(_))), "{result:?}");
        }
        assert_eq!(rpc.calls().len(), 1, "a refused op reached the editor");
    }

    #[test]
    fn a_symbol_with_a_control_character_is_refused() {
        let f = fixture("agent-symbol");
        let rpc = Recorder::default();
        let with_symbol = |symbol: String| EditorOp::AgentLspAsk {
            session: "s-held".into(),
            id: "abc-3-1".into(),
            kind: WireLspKind::Usages,
            path: Some("src/a.rs".into()),
            line: Some(1),
            symbol: Some(symbol),
            include: vec![],
            query: None,
            query_for: Vec::new(),
            budget_ms: 1000,
            load: false,
            stamp: stamp(),
        };
        assert!(matches!(
            dispatch_now(1, with_symbol("a".repeat(SYMBOL_MAX)), &f.ctx, &rpc),
            Ok(Dispatch::Rpc(_))
        ));
        for symbol in ["a\nb".to_owned(), "a\u{7}".to_owned(), "a".repeat(SYMBOL_MAX + 1)] {
            let result = dispatch_now(1, with_symbol(symbol), &f.ctx, &rpc);
            assert!(matches!(result, Err(OpRefused::Outside(_))), "{result:?}");
        }
        assert!(matches!(
            dispatch_now(1, lsp_search(&"q".repeat(QUERY_MAX), "abc-3-1"), &f.ctx, &rpc),
            Ok(Dispatch::Rpc(_))
        ));
        for query in ["a\rb".to_owned(), "q".repeat(QUERY_MAX + 1)] {
            let result = dispatch_now(1, lsp_search(&query, "abc-3-1"), &f.ctx, &rpc);
            assert!(matches!(result, Err(OpRefused::Outside(_))), "{result:?}");
        }
        assert_eq!(rpc.calls().len(), 2, "a refused op reached the editor");
    }

    #[test]
    fn only_a_workspace_search_names_no_file() {
        let f = fixture("agent-no-path");
        let rpc = Recorder::default();
        for kind in [WireLspKind::Usages, WireLspKind::Hover, WireLspKind::DocumentSymbols] {
            let op = EditorOp::AgentLspAsk {
                session: "s-held".into(),
                id: "abc-3-1".into(),
                kind,
                path: None,
                line: None,
                symbol: None,
                include: vec![],
                query: None,
                query_for: Vec::new(),
                budget_ms: 1000,
                load: true,
                stamp: stamp(),
            };
            let result = dispatch_now(1, op, &f.ctx, &rpc);
            assert!(matches!(result, Err(OpRefused::Outside(_))), "{kind:?}: {result:?}");
        }
        assert!(rpc.calls().is_empty(), "a refused op reached the editor");
    }

    #[test]
    fn a_stamp_of_the_wrong_shape_is_refused_on_the_lsp_ops() {
        let f = fixture("agent-lsp-scope");
        let rpc = Recorder::default();
        let bad = || WireStamp {
            scope: "Not A Scope".into(),
            ..stamp()
        };
        for op in [
            lsp_ask("s-held", "src/a.rs", false, bad()),
            EditorOp::AgentLspCollect {
                session: "s-held".into(),
                id: "abc-3-1".into(),
                last: false,
                stamp: bad(),
            },
            EditorOp::AgentBaseline {
                session: "s-held".into(),
                stamp: bad(),
            },
        ] {
            let result = dispatch_now(1, op, &f.ctx, &rpc);
            assert!(matches!(result, Err(OpRefused::Outside(_))), "{result:?}");
        }
        assert!(rpc.calls().is_empty(), "a refused op reached the editor");
    }

    /// One argument of a recorded call.
    fn sent_field(args: &[Value], name: &str) -> Value {
        args[1]
            .as_map()
            .unwrap()
            .iter()
            .find(|(k, _)| k.as_str() == Some(name))
            .unwrap()
            .1
            .clone()
    }

    /// The panel's `gd` in window 2 builds its calls through `RemoteWay`: a cited `usages` ask, a workspace search
    /// worded per server, and their collect, each under the gd stamp scope and ids, with the budgets it gives them.
    #[test]
    fn a_gd_ask_built_as_window_2_builds_it_passes_window_1s_checks() {
        use crate::editor_tools::stamp::Sequencer;
        use crate::editor_tools::way::RemoteWay;
        use std::rc::Rc;
        let f = fixture("agent-gd-remote");
        let built = Rc::new(RefCell::new(Vec::new()));
        let keep = built.clone();
        let way = RemoteWay::new(
            &f.ctx.root,
            Box::new(move |op| {
                keep.borrow_mut().push(op);
                Pending::failed(RpcError::Unavailable("kept for the test".into()))
            }),
        );
        let mut sequencer = Sequencer::new(format!("gd-{}-{:x}", std::process::id(), wall_ms()));
        // The cited step lives 1.5 s and the symbol search 3.5 s, each with a second's margin.
        let cited = AskArgs {
            id: "gd-1".into(),
            kind: LspKind::Usages,
            root: f.ctx.root.clone(),
            path: Some(f.ctx.root.join("src/a.rs")),
            line: Some(1),
            symbol: Some("a".into()),
            include: vec![UsagePart::Definitions],
            budget_ms: 2500,
            load: false,
            ..Default::default()
        };
        let search = AskArgs {
            id: "gd-2".into(),
            kind: LspKind::WorkspaceSymbols,
            root: f.ctx.root.clone(),
            query: Some("Name".into()),
            query_for: vec![
                ("gopls".into(), "pkg.Name".into()),
                ("rust-analyzer".into(), "Name#".into()),
                ("rust_analyzer".into(), "Name#".into()),
            ],
            budget_ms: 4500,
            load: false,
            ..Default::default()
        };
        let calls = [
            EditorCall::LspAsk(cited),
            EditorCall::LspAsk(search),
            EditorCall::LspCollect {
                id: "gd-2".into(),
                last: true,
            },
        ];
        for call in &calls {
            let stamp = sequencer.stamp(Duration::from_millis(3500));
            drop(way.call("s-held", call, &stamp));
        }
        let ops = built.borrow().clone();
        assert_eq!(ops.len(), 3, "every call crossed as an op: {ops:?}");
        assert!(matches!(&ops[0], EditorOp::AgentLspAsk { path: Some(path), .. } if path == "src/a.rs"));
        let rpc = Recorder::default();
        for op in ops {
            let name = op.name();
            assert!(
                matches!(dispatch_now(1, op, &f.ctx, &rpc), Ok(Dispatch::Rpc(_))),
                "window 1 refused {name}"
            );
        }
        let sent = rpc.calls();
        assert_eq!(
            sent.iter().map(|(code, _)| *code).collect::<Vec<_>>(),
            [lsp::LSP_ASK_LUA, lsp::LSP_ASK_LUA, lsp::LSP_COLLECT_LUA]
        );
        assert_eq!(
            sent_field(&sent[0].1, "path"),
            Value::Binary(f.ctx.root.join("src/a.rs").as_os_str().as_encoded_bytes().to_vec())
        );
        assert_eq!(sent_field(&sent[0].1, "id").as_str(), Some("gd-1"));
        assert_eq!(sent_field(&sent[1].1, "query").as_str(), Some("Name"));
        assert_eq!(
            sent_field(&sent[1].1, "query_for"),
            Value::Map(vec![
                ("gopls".into(), "pkg.Name".into()),
                ("rust-analyzer".into(), "Name#".into()),
                ("rust_analyzer".into(), "Name#".into()),
            ])
        );
        for (_, args) in &sent[..2] {
            assert_eq!(sent_field(args, "load"), Value::Boolean(false));
        }
    }

    #[test]
    fn a_query_uses_window_1s_root() {
        let f = fixture("agent-query");
        let rpc = Recorder::default();
        assert!(matches!(
            dispatch_now(1, lsp_search("frob", "abc-3-1"), &f.ctx, &rpc),
            Ok(Dispatch::Rpc(_))
        ));
        let calls = rpc.calls();
        assert_eq!(calls[0].0, lsp::LSP_ASK_LUA);
        assert_eq!(
            sent_field(&calls[0].1, "root"),
            Value::Binary(f.ctx.root.as_os_str().as_encoded_bytes().to_vec())
        );
        assert_eq!(sent_field(&calls[0].1, "query").as_str(), Some("frob"));
    }

    #[test]
    fn a_baseline_uses_window_1s_root_and_limit() {
        let f = fixture("agent-baseline");
        let rpc = Recorder::default();
        let wire = stamp();
        let op = EditorOp::AgentBaseline {
            session: "s-held".into(),
            stamp: wire.clone(),
        };
        assert!(matches!(dispatch_now(1, op, &f.ctx, &rpc), Ok(Dispatch::Rpc(_))));
        assert_eq!(rpc.calls()[0].0, diagnostics::DIAGNOSTICS_LUA);
        assert_eq!(
            sent_field(&rpc.calls()[0].1, "limit"),
            Value::from(BASELINE_LIMIT),
            "the op names no limit: window 1 picks it"
        );
        assert_eq!(
            sent_field(&rpc.calls()[0].1, "answer_max"),
            Value::from(diagnostics::answer_max())
        );
        // Exactly what a tab of window 1 sends its own editor.
        let expected = Rc::new(Recorder::default());
        let _ = LocalWay(expected.clone()).call(
            "any",
            &EditorCall::Baseline {
                root: f.ctx.root.clone(),
            },
            &Stamp::from(wire),
        );
        assert_eq!(rpc.calls(), expected.calls());
    }

    #[test]
    fn a_file_gone_since_window_2_looked_is_refused_for_a_load() {
        let f = fixture("agent-gone");
        let rpc = Recorder::default();
        let result = dispatch_now(1, agent_open("s-held", "src/gone.rs", true, stamp()), &f.ctx, &rpc);
        assert!(matches!(result, Err(OpRefused::Outside(_))), "{result:?}");
        assert!(rpc.calls().is_empty(), "a refused op reached the editor");
    }

    #[test]
    fn the_editor_state_root_is_window_1s_own() {
        let f = fixture("agent-root");
        let rpc = Recorder::default();
        assert!(matches!(
            dispatch_now(1, agent_state("s-held"), &f.ctx, &rpc),
            Ok(Dispatch::Rpc(_))
        ));
        assert!(matches!(
            dispatch_now(2, agent_diagnostics("s-held", None), &f.ctx, &rpc),
            Ok(Dispatch::Rpc(_))
        ));
        let calls = rpc.calls();
        let root = Value::Binary(f.ctx.root.as_os_str().as_encoded_bytes().to_vec());
        for (_, args) in &calls {
            let sent = args[1]
                .as_map()
                .unwrap()
                .iter()
                .find(|(k, _)| k.as_str() == Some("root"))
                .unwrap()
                .1
                .clone();
            assert_eq!(sent, root);
        }
    }

    #[test]
    fn a_severity_no_tool_has_is_refused() {
        let f = fixture("agent-severity");
        let rpc = Recorder::default();
        for min_severity in [0, 5, u8::MAX] {
            let op = EditorOp::AgentDiagnostics {
                session: "s-held".into(),
                path: None,
                min_severity,
                limit: 1,
                stamp: stamp(),
            };
            let result = dispatch_now(1, op, &f.ctx, &rpc);
            assert!(matches!(result, Err(OpRefused::Outside(_))), "{result:?}");
        }
        assert!(rpc.calls().is_empty(), "a refused op reached the editor");
    }

    #[test]
    fn a_large_diagnostics_limit_is_held_to_window_1s_largest() {
        let f = fixture("agent-limit");
        let rpc = Recorder::default();
        let op = EditorOp::AgentDiagnostics {
            session: "s-held".into(),
            path: None,
            min_severity: 4,
            limit: u32::MAX,
            stamp: stamp(),
        };
        assert!(matches!(dispatch_now(1, op, &f.ctx, &rpc), Ok(Dispatch::Rpc(_))));
        let limit = rpc.calls()[0].1[1]
            .as_map()
            .unwrap()
            .iter()
            .find(|(k, _)| k.as_str() == Some("limit"))
            .unwrap()
            .1
            .as_u64();
        assert_eq!(limit, Some(u64::from(DIAGNOSTICS_LIMIT_MAX)));
    }

    // ---- live drawings ---------------------------------------------------------------------------------------

    fn live_note(session: &str, ended_ns: Option<u64>, paths: &[&str]) -> EditorOp {
        EditorOp::ReviewLiveNote {
            session: session.into(),
            turn_id: "t-3".into(),
            started_ns: 100,
            ended_ns,
            paths: paths.iter().map(|p| p.to_string()).collect(),
        }
    }

    fn live_meta(session: &str) -> WireLiveMeta {
        WireLiveMeta {
            tab: 3,
            session: session.into(),
            turn_id: "t-3".into(),
            turn: 2,
            running: true,
            read_lines: 1,
        }
    }

    fn noted(result: Result<Dispatch, OpRefused>) -> (LiveNote, Option<Pending>) {
        match result {
            Ok(Dispatch::LiveNote { note, shown }) => (note, shown),
            other => panic!("expected a live note, got {other:?}"),
        }
    }

    #[test]
    fn live_note_refuses_a_session_not_held_a_path_outside_and_257_paths() {
        let f = fixture("live-note-refused");
        let rpc = Recorder::default();
        assert_eq!(
            dispatch_now(1, live_note("s-other", None, &["src/a.rs"]), &f.ctx, &rpc).unwrap_err(),
            OpRefused::SessionNotHeld("s-other".into())
        );
        for path in ["../x", "/etc/passwd", "", "./src/a.rs", "src/../a/b", "a\0b"] {
            let result = dispatch_now(1, live_note("s-held", None, &["src/a.rs", path]), &f.ctx, &rpc);
            assert!(
                matches!(result, Err(OpRefused::Outside(_))),
                "a note naming {path:?} was not refused as outside: {result:?}"
            );
        }
        let many: Vec<String> = (0..=MAX_SHOWN_PATHS).map(|n| format!("src/{n}.rs")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(matches!(
            dispatch_now(1, live_note("s-held", None, &many), &f.ctx, &rpc),
            Err(OpRefused::Outside(_))
        ));
        assert!(rpc.calls().is_empty(), "a refused note reached the editor");

        // One batch is what a note may carry; window 1 asks about all of it.
        let (note, shown) = noted(dispatch_now(1, live_note("s-held", None, &many[1..]), &f.ctx, &rpc));
        assert_eq!(note.paths.len(), MAX_SHOWN_PATHS);
        assert!(shown.is_some());
        let calls = rpc.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, LIVE_SHOWN_LUA, "the note asks with the live-shown script");
        assert_eq!(calls[0].1, live_shown_args(&note.paths));

        // The live draw and clear are a review's: the session, then the path, as a show's are.
        let show = EditorOp::ReviewLiveShow {
            path: "src/a.rs".into(),
            meta: live_meta("s-other"),
            hunks: vec![hunk()],
        };
        assert_eq!(
            dispatch_now(1, show, &f.ctx, &rpc).unwrap_err(),
            OpRefused::SessionNotHeld("s-other".into())
        );
        for path in ["../x", "out/passwd", "src/secret.rs", "src/alias.rs", ""] {
            let show = EditorOp::ReviewLiveShow {
                path: path.into(),
                meta: live_meta("s-held"),
                hunks: vec![hunk()],
            };
            let clear = EditorOp::ReviewLiveClear { path: path.into() };
            for op in [show, clear] {
                let name = op.name();
                let result = dispatch_now(1, op, &f.ctx, &rpc);
                assert!(
                    matches!(result, Err(OpRefused::Outside(_))),
                    "{name} of {path:?} was not refused as outside: {result:?}"
                );
            }
        }
        assert_eq!(rpc.calls().len(), 1, "a refused live op reached the editor");
    }

    #[test]
    fn live_note_after_the_end_asks_nothing() {
        let f = fixture("live-note-ended");
        let rpc = Recorder::default();
        let (note, shown) = noted(dispatch_now(
            1,
            live_note("s-held", Some(250), &["src/a.rs", "a/b"]),
            &f.ctx,
            &rpc,
        ));
        assert!(shown.is_none(), "an ended turn's note asks the editor nothing");
        assert!(rpc.calls().is_empty());
        assert_eq!(
            note,
            LiveNote {
                key: TurnKey {
                    session: "s-held".into(),
                    turn_id: "t-3".into(),
                },
                started_ns: 100,
                ended_ns: Some(250),
                paths: vec![f.ctx.root.join("src/a.rs"), f.ctx.root.join("a/b")],
            }
        );

        // While the turn runs the same note asks, and the arguments are window 1's own.
        let (note, shown) = noted(dispatch_now(1, live_note("s-held", None, &["src/a.rs"]), &f.ctx, &rpc));
        assert!(shown.is_some());
        assert_eq!(note.ended_ns, None);
        let calls = rpc.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, LIVE_SHOWN_LUA, "the note asks with the live-shown script");
        assert_eq!(calls[0].1, live_shown_args(&[f.ctx.root.join("src/a.rs")]));

        // A running turn's note with no path has nothing to ask.
        let (_, shown) = noted(dispatch_now(1, live_note("s-held", None, &[]), &f.ctx, &rpc));
        assert!(shown.is_none());
        assert_eq!(rpc.calls().len(), 1);
    }

    #[test]
    fn live_note_touches_no_file_system() {
        // A root that is not there at all: a check that looked at the disk would refuse every path.
        let f = fixture("live-note-no-disk");
        let ctx = Ctx {
            root: f.ctx.root.join("not-here"),
            sessions: f.ctx.sessions.clone(),
            plugins: BTreeMap::new(),
            drawn: vec![],
        };
        let rpc = Recorder::default();
        let paths = [
            "missing/dir/file.rs",
            "out/passwd",
            "src/secret.rs",
            ".git/config",
            "dir with space/\u{4e2d}.rs",
            "new\nline.rs",
        ];
        let (note, shown) = noted(dispatch_now(1, live_note("s-held", None, &paths), &ctx, &rpc));
        assert!(shown.is_some(), "answered, not refused");
        assert_eq!(
            note.paths,
            paths.iter().map(|p| ctx.root.join(p)).collect::<Vec<_>>(),
            "each path is the project's by its text alone"
        );
    }

    #[test]
    fn live_show_and_clear_become_window_1s_driver_calls() {
        let f = fixture("live-overlay");
        let rpc = Recorder::default();
        let abs = f.ctx.root.join("src/a.rs");
        let show = EditorOp::ReviewLiveShow {
            path: "src/a.rs".into(),
            meta: live_meta("s-held"),
            hunks: vec![hunk()],
        };
        assert_eq!(
            overlay(dispatch_now(5, show, &f.ctx, &rpc)),
            vec![OverlayCall::LiveShow {
                path: abs.clone(),
                meta: LiveMeta::from(live_meta("s-held")),
                hunks: vec![ShowHunk::from(hunk())],
            }]
        );
        // A clear names no session: window 1's driver takes off only a live drawing this window made.
        assert_eq!(
            overlay(dispatch_now(
                6,
                EditorOp::ReviewLiveClear {
                    path: "src/a.rs".into()
                },
                &f.ctx,
                &rpc
            )),
            vec![OverlayCall::LiveClear { path: abs }]
        );
        assert!(
            rpc.calls().is_empty(),
            "the live draw and clear send no Lua of their own"
        );
    }
}
