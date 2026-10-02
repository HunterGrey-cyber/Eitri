//! [`EditorRpc`] over the editor this window embeds: a question for its nvim runs on a thread of
//! its own (`NeovideEditorPane::exec_lua`), and the answer comes back through a [`Pending`] the
//! panel polls from its tick. Nothing here waits.

use std::rc::Rc;

use neovide_editor::NeovideEditorPane;
use rmpv::Value;

use eitri_core::editor_rpc::EditorRpc;
use eitri_core::nvim_rpc::{Pending, RpcError};

/// The window's own editor, as an [`EditorRpc`].
///
/// Every failure the embedded nvim reports -- its own error, or a connection that ended under the
/// call -- comes back as [`RpcError::Nvim`] with nvim-rs's text, because that is all the pane's
/// reply carries. A caller must treat every `Err` alike, as "the editor could not be asked", and
/// never decide anything by which kind of error it was.
pub(crate) struct EmbeddedEditorRpc {
    pane: Rc<NeovideEditorPane>,
    /// Whether the editor module is still in this window: `prefix x` quits it for good.
    editor_present: Rc<dyn Fn() -> bool>,
}

impl EmbeddedEditorRpc {
    pub(crate) fn new(pane: Rc<NeovideEditorPane>, editor_present: Rc<dyn Fn() -> bool>) -> EmbeddedEditorRpc {
        EmbeddedEditorRpc { pane, editor_present }
    }
}

impl EditorRpc for EmbeddedEditorRpc {
    fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
        if !(self.editor_present)() {
            return Pending::failed(RpcError::Unavailable("the editor is closed".to_owned()));
        }
        if !self.pane.is_running() {
            return Pending::failed(RpcError::Unavailable("the editor is not ready yet".to_owned()));
        }
        let (answer, pending) = Pending::pair();
        // The reply runs on the call's own thread, so it holds only the `Send` answer.
        let reply = Box::new(move |result: Result<Value, String>| answer.send(result.map_err(RpcError::Nvim)));
        // editor-rpc-scan: forwards a caller's constant
        if self.pane.exec_lua(code, args, reply) {
            pending
        } else {
            Pending::failed(RpcError::Unavailable("the editor is not running".to_owned()))
        }
    }

    fn target(&self) -> Option<u64> {
        if !(self.editor_present)() {
            return None;
        }
        self.pane.session_serial()
    }
}
