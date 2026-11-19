//! One way to run Lua in the user's editor and read the answer, whichever nvim that is: the one a
//! window embeds, or the one a companion panel is attached to.
//!
//! **The Lua is always a compile-time constant.** `code` is `&'static str` and every value -- a
//! path, a hunk, a line of a file -- travels in `args` as a msgpack value, so text read from disk
//! or written by the agent can never become Lua source in the user's editor. A built `String`
//! cannot be passed without leaking it, and `core/tests/editor_rpc_scan.rs` fails on a call whose
//! code is not a literal or a constant, and on a `leak` outside tests.
//!
//! No `Send` bound: the window holds its transport on the GTK thread, and every answer is a
//! [`Pending`] to poll, never to wait on there.

use rmpv::Value;

use crate::nvim_rpc::{NvimLink, Pending};

/// A compiled module call and the cleanup it adopts into the takeover that sent it.
macro_rules! takeover_chunk {
    ($body:expr, $registration:literal) => {
        concat!(
            r#"
local __takeover_options = select(select('#', ...), ...)
local __takeover_glue
local __takeover_live = rawget(_G, '__eitri_companion')
if __takeover_live and __takeover_live.mode == 'takeover' then
  if type(__takeover_options) ~= 'table' or __takeover_options.takeover_chan ~= __takeover_live.chan then
    return { refused = 'the takeover channel does not own this call' }
  end
end
if type(__takeover_options) == 'table' and __takeover_options.takeover_chan then
  __takeover_glue = rawget(_G, '__eitri_companion')
  if not __takeover_glue or __takeover_glue.torn or __takeover_glue.mode ~= 'takeover'
    or __takeover_glue.chan ~= __takeover_options.takeover_chan then
    return { refused = 'the takeover glue is gone' }
  end
end
local function __takeover_adopt(key, cleanup)
  local registered = __takeover_glue.takeover_registered or {}
  __takeover_glue.takeover_registered = registered
  if __takeover_glue.torn then cleanup(); return end
  if registered[key] then registered[key].cleanup = cleanup; return end
  local record = {cleanup=cleanup}
  registered[key] = record
  local torn = false
  table.insert(__takeover_glue.teardowns, function()
    if torn then return end
    torn = true
    record.cleanup()
  end)
end
local function __takeover_run(...)
"#,
            $body,
            r#"
end
local __takeover_ok, __takeover_result = pcall(__takeover_run, ...)
if __takeover_glue then
"#,
            $registration,
            "\nend\nif not __takeover_ok then error(__takeover_result, 0) end\nreturn __takeover_result\n"
        )
    };
}
pub(crate) use takeover_chunk;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkSide {
    Registered,
    OneShot,
    Refused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkRefused;

impl std::fmt::Display for ChunkRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the Lua chunk is refused in a takeover")
    }
}
impl std::error::Error for ChunkRefused {}

/// Every runtime chunk, compared by content, except the card-approved exec chunk: its text stays private to its own
/// module, which answers for it through `is_exec_chunk` (a `Registered` chunk). Host-only chunks remain listed even
/// though a takeover refuses them.
pub const CHUNKS: &[(&str, ChunkSide)] = &[
    (crate::review_editor::REVIEW_LUA, ChunkSide::Registered),
    (crate::plugin::editor::PLUGIN_BUFFERS_LUA, ChunkSide::Registered),
    (crate::plugin::editor::PLUGIN_ASSIST_LUA, ChunkSide::Registered),
    (crate::plugin::editor::PLUGIN_CWD_LUA, ChunkSide::Registered),
    (crate::plugin::editor::PLUGIN_FOLLOW_LUA, ChunkSide::Registered),
    #[cfg(target_os = "linux")]
    (crate::ide_server::view::IDE_DIFF_OPEN, ChunkSide::Registered),
    #[cfg(target_os = "linux")]
    (crate::ide_server::mention::IDE_MENTION_DEFINE, ChunkSide::Registered),
    (
        crate::editor_tools::tools::open_file::OPEN_FILE_LUA,
        ChunkSide::Registered,
    ),
    (
        crate::editor_tools::tools::editor_state::EDITOR_STATE_LUA,
        ChunkSide::Registered,
    ),
    (
        crate::editor_tools::tools::read_buffer::READ_BUFFER_LUA,
        ChunkSide::Registered,
    ),
    (
        crate::editor_tools::tools::diagnostics::DIAGNOSTICS_LUA,
        ChunkSide::Registered,
    ),
    (
        crate::editor_tools::tools::mention_reads::BUFFERS_LUA,
        ChunkSide::Registered,
    ),
    (
        crate::editor_tools::tools::mention_reads::QUICKFIX_LUA,
        ChunkSide::Registered,
    ),
    (crate::editor_tools::tools::lsp::LSP_ASK_LUA, ChunkSide::Registered),
    (crate::editor_tools::tools::lsp::LSP_COLLECT_LUA, ChunkSide::Registered),
    (crate::editor_tools::tools::lsp::LSP_TARGETS_LUA, ChunkSide::Registered),
    (crate::review_editor::LIVE_SHOWN_LUA, ChunkSide::OneShot),
    (crate::review_editor::LIVE_SHOW_LUA, ChunkSide::OneShot),
    (crate::review_editor::LIVE_CLEAR_LUA, ChunkSide::OneShot),
    (crate::review_editor::SHOW_LUA, ChunkSide::OneShot),
    (crate::review_editor::OPEN_AND_SHOW_LUA, ChunkSide::OneShot),
    (crate::review_editor::CLEAR_LUA, ChunkSide::OneShot),
    (crate::review_editor::CLEAR_ALL_LUA, ChunkSide::OneShot),
    (crate::review_editor::TAKE_EVENTS_LUA, ChunkSide::OneShot),
    (crate::editor_open::EDITOR_OPEN_LUA, ChunkSide::OneShot),
    (crate::plugin::editor::PLUGIN_OPEN_LUA, ChunkSide::OneShot),
    #[cfg(target_os = "linux")]
    (crate::ide_server::view::IDE_DIFF_CLOSE, ChunkSide::OneShot),
    #[cfg(target_os = "linux")]
    (crate::ide_server::view::IDE_TAKE_EVENTS, ChunkSide::OneShot),
    #[cfg(target_os = "linux")]
    (crate::ide_server::mention::IDE_MENTION_REMOVE, ChunkSide::OneShot),
    #[cfg(target_os = "linux")]
    (crate::ide_server::mention::IDE_MENTION_OUTSIDE, ChunkSide::OneShot),
    (crate::turn_review::revert::BUFFER_STATE_LUA, ChunkSide::OneShot),
    (crate::remote_sync::quiet::UNSAVED_UNDER_LUA, ChunkSide::OneShot),
    (crate::nvim_keys::rescan::KEYS_RESCAN_LUA, ChunkSide::OneShot),
    (crate::companion::TEARDOWN_LUA, ChunkSide::OneShot),
    (crate::companion::PAGE_TEARDOWN_LUA, ChunkSide::OneShot),
    (crate::companion::SLOT_CALL_LUA, ChunkSide::OneShot),
    (crate::companion::SCRATCH_CALL_LUA, ChunkSide::OneShot),
    (crate::companion::INSTALL_LUA, ChunkSide::Refused),
    (crate::nvim_verbs::install::EITRI_COMMANDS_LUA, ChunkSide::Refused),
    (
        "pcall(vim.cmd, 'confirm qall') \
         local p = os.getenv('EITRI_PANE_SWITCH_SOCKET') \
         if p then local ok, c = pcall(vim.fn.sockconnect, 'pipe', p, {{rpc = false}}) \
         if ok and c ~= 0 then pcall(vim.fn.chansend, c, 'Q {generation}\\n') pcall(vim.fn.chanclose, c) end end",
        ChunkSide::Refused,
    ),
    (crate::tutor::INSTALL_TUTOR_LUA, ChunkSide::Refused),
    (crate::takeover::room::ROOM_LUA, ChunkSide::Refused),
    (crate::takeover::room::ROOM_OP_LUA, ChunkSide::Refused),
    (crate::takeover::sequence::MOVE_LUA, ChunkSide::Refused),
    (crate::takeover::sequence::FACTS_LUA, ChunkSide::Refused),
    (crate::takeover::sequence::TAKEN_LUA, ChunkSide::Refused),
    ("vim.fn.chanclose(...)", ChunkSide::Refused),
    // The fork's attach records its channel; the takeover window reads it over its own link to check the attach.
    (
        "local a = rawget(_G, '__neovide_attach'); return type(a) == 'table' and a.chan or nil",
        ChunkSide::Refused,
    ),
    (crate::editor_reopen::EDITOR_REOPEN_CAPTURE_LUA, ChunkSide::Refused),
    (crate::editor_reopen::EDITOR_REOPEN_LUA, ChunkSide::Refused),
    (
        "vim.o.guicursor = 'a:block-blinkon0' \
    vim.o.shortmess = vim.o.shortmess .. 'I' \
    vim.o.showmode = false \
    vim.o.ruler = false \
    vim.o.showcmd = false \
    vim.o.number = false \
    vim.o.wrap = false \
    vim.bo.buftype = 'nofile' \
    vim.bo.bufhidden = 'hide' \
    vim.bo.swapfile = false",
        ChunkSide::Refused,
    ),
    ("vim.api.nvim_buf_set_lines(0, 0, -1, false, (...))", ChunkSide::Refused),
    ("vim.cmd('EitriTutor')", ChunkSide::Refused),
];

/// Classify by full content, including whitespace, so a changed chunk needs an explicit decision.
pub fn check_chunk(code: &str) -> Result<ChunkSide, ChunkRefused> {
    if crate::editor_tools::tools::nvim_exec::is_exec_chunk(code) {
        return Ok(ChunkSide::Registered);
    }
    CHUNKS
        .iter()
        .find(|(known, _)| *known == code)
        .map(|(_, side)| *side)
        .ok_or(ChunkRefused)
}

/// A takeover window's single route to its editor. The shared transport keeps its normal ordering.
pub struct TakeoverRpc<'a> {
    transport: std::rc::Rc<dyn EditorRpc + 'a>,
    link_chan: u64,
}

impl<'a> TakeoverRpc<'a> {
    pub fn new(transport: std::rc::Rc<dyn EditorRpc + 'a>, link_chan: u64) -> Self {
        Self { transport, link_chan }
    }
}

impl EditorRpc for TakeoverRpc<'_> {
    fn exec_lua(&self, code: &'static str, mut args: Vec<Value>) -> Pending {
        match check_chunk(code) {
            Err(_) | Ok(ChunkSide::Refused) => {
                return Pending::failed(crate::nvim_rpc::RpcError::Unavailable(ChunkRefused.to_string()))
            }
            Ok(ChunkSide::Registered) => {
                if code == crate::review_editor::REVIEW_LUA {
                    args = crate::review_editor::install_args(crate::review_editor::Owner::Companion {
                        channel: self.link_chan,
                    });
                }
                args.push(Value::Map(vec![("takeover_chan".into(), self.link_chan.into())]));
            }
            Ok(ChunkSide::OneShot) => {}
        }
        // editor-rpc-scan: forwards a caller's constant
        self.transport.exec_lua(code, args)
    }
    fn attached(&self) -> bool {
        true
    }
    fn target(&self) -> Option<u64> {
        self.transport.target()
    }
    fn glue(&self) -> Option<Glue> {
        Some(Glue {
            slot: GlueSlot::Agent,
            chan: self.link_chan,
        })
    }
    fn on_screen(&self) -> bool {
        self.transport.on_screen()
    }
}

/// The glue owned by one connection in the editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlueSlot {
    Agent,
    Page(String),
}

/// Which glue slot a connection owns and the channel it is reached on: the pair the editor checks before it
/// lets a connection's calls touch that slot's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glue {
    pub slot: GlueSlot,
    pub chan: u64,
}

/// **Calls are written in the order they were made.** Every call made through one handle reaches
/// nvim after every call made through it before, and nvim runs them in that order; each is answered
/// by its own request id, so a later call is written at once, never held back until an earlier one
/// is answered. A caller that stops waiting for an answer (its deadline passed) changes nothing: the
/// call stays ahead of every later one. So a caller orders its calls by making them in order, and
/// never holds one back until the previous one is answered. Both transports keep this: the
/// companion's [`NvimLink`] queues every frame to one writer thread, and the embedded editor hands
/// every call to its session's one call line (`neovide_editor`'s `nvim_rpc`), where the window's
/// quit (`NeovideEditorPane::exec_lua_watched`) is written in order with them too. The embedded
/// editor has one gap the companion does not: nvim-rs takes two locks of its own before it writes a
/// request, and when another writer frees one in the instant between two of the line's polls, the
/// later call can be written first (`nvim_rpc`'s module doc). It is rare, but a caller for whom two
/// calls run swapped would leave a wrong state for good makes the older one harmless to run late
/// (the plugin host's assist calls carry the order they were made in, `plugin::editor`).
pub trait EditorRpc {
    /// `nvim_exec_lua(code, args)`. `code` is a compile-time constant; every value travels in
    /// `args`. A transport with no editor to ask answers [`crate::nvim_rpc::RpcError::Unavailable`]
    /// at once.
    fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending;

    /// Changes whenever the nvim behind this handle changes (attach, retarget, restart), so a
    /// caller can tell an answer about one editor from a question to the next; `None`: no editor.
    /// The order above holds within one target: a call to the next editor is not ordered with one
    /// still out at the last.
    fn target(&self) -> Option<u64>;

    /// Whether this handle reaches the terminal nvim held by a takeover.
    fn attached(&self) -> bool {
        false
    }

    /// The slot held by this connection, when it installed glue in the user's own editor.
    fn glue(&self) -> Option<Glue> {
        None
    }

    /// Whether the person can see this editor now. Only a window that lays the editor out can say no: any other
    /// transport reaches an nvim of the person's own, in a place Eitri does not know.
    fn on_screen(&self) -> bool {
        true
    }
}

impl EditorRpc for NvimLink {
    fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
        // editor-rpc-scan: forwards a caller's constant
        NvimLink::exec_lua(self, code, args)
    }

    /// The channel nvim gave this connection while it is up. A link that ended has no editor
    /// behind it, whatever its number was.
    fn target(&self) -> Option<u64> {
        self.is_alive().then(|| self.channel_id())
    }
}

#[cfg(test)]
mod takeover_tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Recorder(RefCell<Vec<(&'static str, Vec<Value>)>>);
    impl EditorRpc for Recorder {
        fn target(&self) -> Option<u64> {
            Some(7)
        }
        fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
            self.0.borrow_mut().push((code, args));
            let (answer, pending) = Pending::pair();
            answer.send(Ok(Value::Nil));
            pending
        }
    }

    #[test]
    fn every_classified_chunk_compiles_and_has_one_classification() {
        let lua = mlua::Lua::new();
        for (code, side) in CHUNKS {
            lua.load(*code).into_function().expect("classified Lua must compile");
            assert_eq!(check_chunk(code), Ok(*side));
        }
    }

    #[test]
    fn registered_calls_use_the_link_channel_and_one_shots_keep_their_arguments() {
        let transport = std::rc::Rc::new(Recorder::default());
        let rpc = TakeoverRpc::new(transport.clone(), 42);
        rpc.exec_lua(
            crate::review_editor::REVIEW_LUA,
            crate::review_editor::install_args(crate::review_editor::Owner::Embedded),
        );
        rpc.exec_lua(crate::editor_open::EDITOR_OPEN_LUA, vec![Value::from("unchanged")]);
        let calls = transport.0.borrow();
        assert_eq!(calls[0].1[0].as_str(), Some("companion:42"));
        assert_eq!(
            calls[0].1.last(),
            Some(&Value::Map(vec![("takeover_chan".into(), 42.into())]))
        );
        assert_eq!(calls[1].1, vec![Value::from("unchanged")]);
    }

    #[test]
    fn an_attached_rpc_refuses_an_unknown_chunk_and_a_refused_one() {
        let transport = std::rc::Rc::new(Recorder::default());
        let rpc = TakeoverRpc::new(transport.clone(), 42);
        assert!(rpc.attached());
        assert!(rpc
            .exec_lua("unexpected takeover chunk", vec![])
            .try_take()
            .unwrap()
            .is_err());
        assert!(rpc
            .exec_lua(crate::tutor::INSTALL_TUTOR_LUA, vec![])
            .try_take()
            .unwrap()
            .is_err());
        assert!(transport.0.borrow().is_empty());
        assert!(check_chunk("unexpected takeover chunk").is_err());
        assert_eq!(check_chunk(crate::tutor::INSTALL_TUTOR_LUA), Ok(ChunkSide::Refused));
    }
}
