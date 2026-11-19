//! The traits a window's host is written against that do not name a toolkit: what the linked windows' code asks of
//! the host that carries it, which each host (`shell` on Linux, `eitri-mac` later) implements.

use eitri_core::window_link::names::WindowName;
use eitri_core::window_link::Msg;
use eitri_core::wm::{PeerId, WinId};

/// The answer to every op this shell does not carry out through the host yet; its own code still
/// does those, so a controller that emits one learns it went nowhere.
pub const NOT_CARRIED_OUT: &str = "not carried out by this shell yet";

/// What starts the project's second window for `OpenLinkedWindow`: the pair's own code, which belongs to the shell's
/// binary. A host without it cannot start one.
pub trait LinkedWindows {
    /// Starts the window `name`, or raises it when it runs; `token` is the activation token this window's display
    /// gave, so the window that comes forward gets the keys. `Err` says why nothing was started.
    fn open(&self, win: WinId, name: &WindowName, token: Option<String>) -> Result<(), String>;
    /// Ends the process of the window `peer`, when this window started it. `Err` says why nothing was ended.
    fn end_peer(&self, win: WinId, peer: PeerId) -> Result<(), String> {
        let _ = (win, peer);
        Err(NOT_CARRIED_OUT.into())
    }
    /// Starts the window `name` again at this window's launch, as the pair last ended, with no activation token.
    /// `Err` says why it was not; what this window held for window 2 has then been given back here.
    fn restore(&self, win: WinId, name: &WindowName) -> Result<(), String>;
    /// Whether the module window `name` is open or starting, whoever started it.
    fn module_window_open(&self, name: &WindowName) -> bool {
        let _ = name;
        false
    }
    /// Window 1 opens the chat or page window `name`: starts it, or raises it with `token`. With `keep_keys` the window
    /// maps without taking the keys, and one that is open already is left as it is.
    fn open_module(&self, win: WinId, name: &WindowName, token: Option<String>, keep_keys: bool) -> Result<(), String> {
        let _ = (win, name, token, keep_keys);
        Err(NOT_CARRIED_OUT.into())
    }
    /// Sends `msg` to the window `peer` over its link. `Err` says why it was not sent.
    fn send(&self, win: WinId, peer: PeerId, msg: Msg) -> Result<(), String> {
        let _ = (win, peer, msg);
        Err(NOT_CARRIED_OUT.into())
    }
    /// Sends `msg` to window 1, from a window it started. `Err` says why it was not sent.
    fn send_to_hub(&self, win: WinId, msg: Msg) -> Result<(), String> {
        let _ = (win, msg);
        Err(NOT_CARRIED_OUT.into())
    }
    /// Hands the tab holding `agent`'s `session` from the module window `from` to the chat window `to`, which is started
    /// or raised with `token`. The window that asked is told itself when it cannot go; `Err` says why no window could be.
    fn hand_over(
        &self,
        win: WinId,
        from: PeerId,
        agent: &eitri_core::agent_backend::AgentId,
        session: &str,
        to: &WindowName,
        token: Option<String>,
    ) -> Result<(), String> {
        let _ = (win, from, agent, session, to, token);
        Err(NOT_CARRIED_OUT.into())
    }
    /// In a window 2 with no window 1, starts the editor window detached from this process; it links to this
    /// window once it is up. `Err` says why none was started.
    fn start_editor(&self, win: WinId) -> Result<(), String>;
}
