//! Every hook a window hands its agent panel, registered by one function: `wire_panel` takes a
//! [`PanelHooks`] and registers each hook it holds, and nothing else in the shell calls the panel
//! handle's hook registrations.
//!
//! One place, because the main window and the companion window each wired the panel by hand and
//! drifted apart without anything noticing: a hook one window installs and the other forgets fails
//! silently, as a verb or a card reaction that does nothing. With both windows filling a
//! `PanelHooks` and calling `wire_panel`, what a window leaves out is a named, checked list.
//!
//! The hooks' closures are still built where their window builds them, with the captures they need
//! there; only the registration moves. `build_ui` registers them all at once, after the last of them
//! is built: nothing between the first and the last can make the panel call one, since the window is
//! still being built and no page message is read before the main loop runs again.
//!
//! Not here: what the panel is pushed on every change (`set_theme`, `set_pane_focused`), which are
//! values rather than hooks, and the coverage query (`set_coverage`), which only the main window has.

use std::rc::Rc;

use eitri_core::attention::Attention;
use eitri_core::editor_rpc::EditorRpc;
use eitri_core::keymap::TabAction;
use eitri_core::layout::Direction;
use eitri_core::review_editor::Owner;
use eitri_core::scratch::ScratchRequest;

use eitri_panel::agent_panel::{AgentPanelHandle, ChatMoveEvent, EditorAnswer, HintInbound, ViewId};

/// Every hook registration on the panel handle, in the order `wire_panel` makes them. Read by the
/// tests that hold each window to what it leaves out.
#[cfg_attr(not(test), allow(dead_code))]
pub const HOOKS: [&str; 22] = [
    "on_hint",
    "on_attention",
    "on_toast",
    "on_warning",
    "set_answer_gate",
    "on_editor_request",
    "set_editor_rpc",
    "set_review_owner",
    "on_review_open",
    "on_editor_done",
    "on_edit_selection",
    "on_choose_project",
    "on_view_empty",
    "on_nav_fallthrough",
    "on_keymap_closed",
    "on_show_view",
    "on_tab_verb",
    "on_chat_move",
    "on_busy_changed",
    "on_turn_ended",
    "on_load_page",
    "on_page_line",
];

/// The registrations the companion window leaves out. It has no tray, no chat view but the base one,
/// no extra views and no `prefix ?` sheet of the window's own to give keys back from, so the attention,
/// show-view, view-empty and keymap-closed hooks have nothing to act on; with one chat view that
/// nothing can float over, there is no covered chat for an answer gate to refuse; and it is never one of a
/// linked pair, so no chat moves out of it. It cannot choose another project either, and it loads no plugin page, so
/// its empty tab offers none.
#[cfg_attr(not(test), allow(dead_code))]
pub const COMPANION_OMITS: &[&str] = &[
    "on_choose_project",
    "on_load_page",
    "on_page_line",
    "on_attention",
    "on_show_view",
    "on_view_empty",
    "on_keymap_closed",
    "set_answer_gate",
    "on_chat_move",
];

/// What the panel calls when cards arrive or clear: the window's attention before and after, and the
/// views handed a card.
pub type AttentionHook = Box<dyn Fn(Attention, Attention, &[ViewId])>;
/// Shows a one-line result to the user.
pub type ToastHook = Box<dyn Fn(&str)>;
/// Whether a card answer from a view is taken; `Err` refuses it with the reason.
pub type AnswerGate = Rc<dyn Fn(ViewId) -> Result<(), String>>;
/// Hands a scratch round trip (`Ctrl+g`, `gf`) to the editor; what it answers is [`EditorAnswer`] (a linked window's
/// view is taken with its outcome to come).
pub type EditorRequest = Box<dyn Fn(&ScratchRequest) -> EditorAnswer>;
/// Which editor holds the review overlay, if any.
pub type ReviewOwner = Box<dyn Fn() -> Option<Owner>>;

/// One field per hook registration on the panel handle, each typed as the handle's method takes it;
/// a window sets the ones it has.
#[derive(Default)]
pub struct PanelHooks {
    pub on_hint: Option<Box<dyn Fn(HintInbound)>>,
    pub on_attention: Option<AttentionHook>,
    pub on_toast: Option<ToastHook>,
    pub on_warning: Option<ToastHook>,
    /// Whether a card answer from a view is taken; `Err` refuses it with the reason. A window that
    /// cannot tell refuses, so the card stays.
    pub set_answer_gate: Option<AnswerGate>,
    pub on_editor_request: Option<EditorRequest>,
    pub set_editor_rpc: Option<Rc<dyn EditorRpc>>,
    pub set_review_owner: Option<ReviewOwner>,
    pub on_review_open: Option<Box<dyn Fn()>>,
    pub on_editor_done: Option<Box<dyn Fn(ViewId)>>,
    pub on_choose_project: Option<Box<dyn Fn()>>,
    pub on_edit_selection: Option<Box<dyn Fn()>>,
    pub on_view_empty: Option<Box<dyn Fn(ViewId)>>,
    pub on_nav_fallthrough: Option<Box<dyn Fn(ViewId, Direction)>>,
    pub on_keymap_closed: Option<Box<dyn Fn(u64, bool)>>,
    pub on_show_view: Option<Box<dyn Fn(ViewId)>>,
    pub on_tab_verb: Option<Box<dyn Fn(ViewId, TabAction)>>,
    pub on_chat_move: Option<Box<dyn Fn(ChatMoveEvent)>>,
    /// The window's aggregate busy state flipped (a turn or background work running in any tab).
    pub on_busy_changed: Option<Box<dyn Fn(bool)>>,
    /// One tab's turn ended.
    pub on_turn_ended: Option<Box<dyn Fn()>>,
    /// Enter on a page the empty tab offers: load it into the window, with its id.
    pub on_load_page: Option<Box<dyn Fn(String)>>,
    /// `o` on a page the empty tab offers: show its `init.lua` line, with its id.
    pub on_page_line: Option<Box<dyn Fn(String)>>,
}

impl PanelHooks {
    /// The registrations this value leaves out, by the handle's method names, in [`HOOKS`]' order.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn missing(&self) -> Vec<&'static str> {
        let set = [
            self.on_hint.is_some(),
            self.on_attention.is_some(),
            self.on_toast.is_some(),
            self.on_warning.is_some(),
            self.set_answer_gate.is_some(),
            self.on_editor_request.is_some(),
            self.set_editor_rpc.is_some(),
            self.set_review_owner.is_some(),
            self.on_review_open.is_some(),
            self.on_editor_done.is_some(),
            self.on_edit_selection.is_some(),
            self.on_choose_project.is_some(),
            self.on_view_empty.is_some(),
            self.on_nav_fallthrough.is_some(),
            self.on_keymap_closed.is_some(),
            self.on_show_view.is_some(),
            self.on_tab_verb.is_some(),
            self.on_chat_move.is_some(),
            self.on_busy_changed.is_some(),
            self.on_turn_ended.is_some(),
            self.on_load_page.is_some(),
            self.on_page_line.is_some(),
        ];
        HOOKS
            .iter()
            .zip(set)
            .filter(|(_, set)| !set)
            .map(|(name, _)| *name)
            .collect()
    }
}

/// Registers every hook `hooks` holds on `panel`, in [`HOOKS`]' order; a hook it does not hold is left
/// as the panel has it.
pub fn wire_panel(panel: &AgentPanelHandle, hooks: PanelHooks) {
    let PanelHooks {
        on_hint,
        on_attention,
        on_toast,
        on_warning,
        set_answer_gate,
        on_editor_request,
        set_editor_rpc,
        set_review_owner,
        on_review_open,
        on_editor_done,
        on_edit_selection,
        on_choose_project,
        on_view_empty,
        on_nav_fallthrough,
        on_keymap_closed,
        on_show_view,
        on_tab_verb,
        on_chat_move,
        on_busy_changed,
        on_turn_ended,
        on_load_page,
        on_page_line,
    } = hooks;
    if let Some(hook) = on_hint {
        panel.on_hint(hook);
    }
    if let Some(hook) = on_attention {
        panel.on_attention(hook);
    }
    if let Some(hook) = on_toast {
        panel.on_toast(hook);
    }
    if let Some(hook) = on_warning {
        panel.on_warning(hook);
    }
    if let Some(gate) = set_answer_gate {
        panel.set_answer_gate(gate);
    }
    if let Some(hook) = on_editor_request {
        panel.on_editor_request(hook);
    }
    if let Some(rpc) = set_editor_rpc {
        panel.set_editor_rpc(rpc);
    }
    if let Some(owner) = set_review_owner {
        panel.set_review_owner(owner);
    }
    if let Some(hook) = on_review_open {
        panel.on_review_open(hook);
    }
    if let Some(hook) = on_editor_done {
        panel.on_editor_done(hook);
    }
    if let Some(hook) = on_choose_project {
        panel.on_choose_project(hook);
    }
    if let Some(hook) = on_edit_selection {
        panel.on_edit_selection(hook);
    }
    if let Some(hook) = on_view_empty {
        panel.on_view_empty(hook);
    }
    if let Some(hook) = on_nav_fallthrough {
        panel.on_nav_fallthrough(hook);
    }
    if let Some(hook) = on_keymap_closed {
        panel.on_keymap_closed(hook);
    }
    if let Some(hook) = on_show_view {
        panel.on_show_view(hook);
    }
    if let Some(hook) = on_tab_verb {
        panel.on_tab_verb(hook);
    }
    if let Some(hook) = on_chat_move {
        panel.on_chat_move(hook);
    }
    if let Some(hook) = on_busy_changed {
        panel.on_busy_changed(hook);
    }
    if let Some(hook) = on_turn_ended {
        panel.on_turn_ended(hook);
    }
    if let Some(hook) = on_load_page {
        panel.on_load_page(hook);
    }
    if let Some(hook) = on_page_line {
        panel.on_page_line(hook);
    }
}
