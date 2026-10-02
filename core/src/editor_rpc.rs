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

pub trait EditorRpc {
    /// `nvim_exec_lua(code, args)`. `code` is a compile-time constant; every value travels in
    /// `args`. A transport with no editor to ask answers [`crate::nvim_rpc::RpcError::Unavailable`]
    /// at once.
    fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending;

    /// Changes whenever the nvim behind this handle changes (attach, retarget, restart), so a
    /// caller can tell an answer about one editor from a question to the next; `None`: no editor.
    fn target(&self) -> Option<u64>;
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
