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
