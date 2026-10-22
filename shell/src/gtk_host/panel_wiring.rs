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

use crate::agent_panel::{AgentPanelHandle, HintInbound, ViewId};

/// Every hook registration on the panel handle, in the order `wire_panel` makes them. Read by the
/// tests that hold each window to what it leaves out.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const HOOKS: [&str; 14] = [
    "on_hint",
    "on_attention",
    "on_toast",
    "set_answer_gate",
    "on_editor_request",
    "set_editor_rpc",
    "set_review_owner",
    "on_review_open",
    "on_editor_done",
    "on_view_empty",
    "on_nav_fallthrough",
    "on_keymap_closed",
    "on_show_view",
    "on_tab_verb",
];

/// The registrations the companion window leaves out. It has no tray, no chat view but the base one,
/// no extra views and no `prefix ?` sheet of the window's own to give keys back from, so the attention,
/// show-view, view-empty and keymap-closed hooks have nothing to act on; and with one chat view that
/// nothing can float over, there is no covered chat for an answer gate to refuse.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const COMPANION_OMITS: &[&str] = &[
    "on_attention",
    "on_show_view",
    "on_view_empty",
    "on_keymap_closed",
    "set_answer_gate",
];

/// What the panel calls when cards arrive or clear: the window's attention before and after, and the
/// views handed a card.
pub(crate) type AttentionHook = Box<dyn Fn(Attention, Attention, &[ViewId])>;
/// Shows a one-line result to the user.
pub(crate) type ToastHook = Box<dyn Fn(&str)>;
/// Whether a card answer from a view is taken; `Err` refuses it with the reason.
pub(crate) type AnswerGate = Rc<dyn Fn(ViewId) -> Result<(), String>>;
/// Hands a scratch round trip (`Ctrl+g`, `gf`) to the editor; `Err` says why it could not.
pub(crate) type EditorRequest = Box<dyn Fn(&ScratchRequest) -> Result<(), String>>;
/// Which editor holds the review overlay, if any.
pub(crate) type ReviewOwner = Box<dyn Fn() -> Option<Owner>>;

/// One field per hook registration on the panel handle, each typed as the handle's method takes it;
/// a window sets the ones it has.
#[derive(Default)]
pub(crate) struct PanelHooks {
    pub(crate) on_hint: Option<Box<dyn Fn(HintInbound)>>,
    pub(crate) on_attention: Option<AttentionHook>,
    pub(crate) on_toast: Option<ToastHook>,
    /// Whether a card answer from a view is taken; `Err` refuses it with the reason. A window that
    /// cannot tell refuses, so the card stays.
    pub(crate) set_answer_gate: Option<AnswerGate>,
    pub(crate) on_editor_request: Option<EditorRequest>,
    pub(crate) set_editor_rpc: Option<Rc<dyn EditorRpc>>,
    pub(crate) set_review_owner: Option<ReviewOwner>,
    pub(crate) on_review_open: Option<Box<dyn Fn()>>,
    pub(crate) on_editor_done: Option<Box<dyn Fn(ViewId)>>,
    pub(crate) on_view_empty: Option<Box<dyn Fn(ViewId)>>,
    pub(crate) on_nav_fallthrough: Option<Box<dyn Fn(ViewId, Direction)>>,
    pub(crate) on_keymap_closed: Option<Box<dyn Fn(u64, bool)>>,
    pub(crate) on_show_view: Option<Box<dyn Fn(ViewId)>>,
    pub(crate) on_tab_verb: Option<Box<dyn Fn(ViewId, TabAction)>>,
}

impl PanelHooks {
    /// The registrations this value leaves out, by the handle's method names, in [`HOOKS`]' order.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn missing(&self) -> Vec<&'static str> {
        let set = [
            self.on_hint.is_some(),
            self.on_attention.is_some(),
            self.on_toast.is_some(),
            self.set_answer_gate.is_some(),
            self.on_editor_request.is_some(),
            self.set_editor_rpc.is_some(),
            self.set_review_owner.is_some(),
            self.on_review_open.is_some(),
            self.on_editor_done.is_some(),
            self.on_view_empty.is_some(),
            self.on_nav_fallthrough.is_some(),
            self.on_keymap_closed.is_some(),
            self.on_show_view.is_some(),
            self.on_tab_verb.is_some(),
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
pub(crate) fn wire_panel(panel: &AgentPanelHandle, hooks: PanelHooks) {
    let PanelHooks {
        on_hint,
        on_attention,
        on_toast,
        set_answer_gate,
        on_editor_request,
        set_editor_rpc,
        set_review_owner,
        on_review_open,
        on_editor_done,
        on_view_empty,
        on_nav_fallthrough,
        on_keymap_closed,
        on_show_view,
        on_tab_verb,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::source_scan as scan;

    /// Every `.NAME(` in `code`: a method call of that name, whatever it is called on.
    fn method_calls(code: &str, name: &str) -> usize {
        scan::word_ends(code, name)
            .into_iter()
            .filter(|end| code[*end..].trim_start().starts_with('('))
            .filter(|end| code[..end - name.len()].trim_end().ends_with('.'))
            .count()
    }

    /// A shell file as code: comments and literals blanked, its test modules cut.
    fn shell_code(text: &str) -> String {
        let code = scan::code_only(text);
        scan::without(&code, &scan::modules(&code, "tests"))
    }

    fn sorted(mut names: Vec<&'static str>) -> Vec<&'static str> {
        names.sort_unstable();
        names
    }

    #[test]
    fn the_companion_and_the_main_window_wire_the_panel_through_one_function() {
        let sources = scan::rust_sources(&["shell/src"]);
        scan::require_anchors(&sources, &["fn wire_panel(", "fn build_ui(", "fn companion_hooks("]);

        // The registrations happen in `wire_panel`, once each, and nowhere else in the shell.
        let (home, wire_panel) = scan::function_in(&sources, "wire_panel");
        for name in HOOKS {
            assert_eq!(method_calls(&wire_panel, name), 1, "wire_panel registers {name} once");
        }
        for source in &sources {
            let code = shell_code(&source.text);
            let code = if source.path == home.path {
                scan::without(&code, &scan::functions(&code, "wire_panel"))
            } else {
                code
            };
            for name in HOOKS {
                assert_eq!(
                    method_calls(&code, name),
                    0,
                    "{} registers {name} on the panel itself instead of through wire_panel",
                    source.path
                );
            }
        }

        // A hook the panel handle gains is one more field here, not a registration a window makes by hand.
        let panel = shell_code(&scan::read_required("panel/src/agent_panel.rs").text);
        for end in scan::word_ends(&panel, "fn") {
            let rest = panel[end..].trim_start();
            let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            let public = panel[..end - 2].trim_end().ends_with("pub");
            if public && name.starts_with("on_") {
                assert!(
                    HOOKS.contains(&name.as_str()),
                    "the panel's {name} has no field in PanelHooks"
                );
            }
        }

        // Each window wires its panel once.
        let (_, build_ui) = scan::function_in(&sources, "build_ui");
        assert_eq!(scan::calls(&build_ui, "wire_panel"), 1, "build_ui wires the panel once");
        let window = shell_code(&scan::read_required("shell/src/companion/window.rs").text);
        let build = scan::functions(&window, "build");
        assert_eq!(build.len(), 1, "the companion window is built by one function");
        assert_eq!(
            scan::calls(build[0], "wire_panel"),
            1,
            "the companion window wires its panel once"
        );
        assert_eq!(
            scan::calls(build[0], "companion_hooks"),
            1,
            "with the companion's own hooks"
        );

        // What the companion leaves out is exactly its declared list, the answer gate among it.
        let hooks = crate::companion::window::companion_hooks(crate::companion::window::CompanionHookParts {
            toast: Rc::new(|_: &str| {}),
            run_tab: Rc::new(|_: TabAction| {}),
            hint: Rc::new(|_: HintInbound| {}),
            move_focus: Rc::new(|_: &eitri_core::layout::ModuleId, _: Direction| false),
            editor_rpc: Rc::new(NoEditor),
            review_owner: Rc::new(|| None),
            review_open: Rc::new(|| {}),
            editor_request: Rc::new(|_: &ScratchRequest| Ok(())),
            editor_done: Rc::new(|| {}),
        });
        assert_eq!(sorted(hooks.missing()), sorted(COMPANION_OMITS.to_vec()));
        assert!(
            hooks.missing().contains(&"set_answer_gate"),
            "the companion installs no answer gate"
        );
    }

    #[test]
    fn the_main_windows_panel_hooks_miss_nothing() {
        // The main window's hooks capture its live widgets, so what it sets is read from its code:
        // every field is set once, before the one `wire_panel` call takes them.
        let (_, build_ui) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
        let code: String = build_ui.split_whitespace().collect();
        let wired = code.find("wire_panel(").expect("build_ui wires the panel");
        let mut missing = Vec::new();
        for name in HOOKS {
            let set = format!("panel_hooks.{name}=Some(");
            match code.matches(&set).count() {
                0 => missing.push(name),
                1 => assert!(
                    code.find(&set).unwrap() < wired,
                    "{name} is set after the panel is wired"
                ),
                n => panic!("build_ui sets {name} {n} times"),
            }
        }
        assert_eq!(missing, Vec::<&str>::new(), "the main window leaves no hook out");
        assert_eq!(
            PanelHooks::default().missing(),
            HOOKS.to_vec(),
            "an empty value misses every hook"
        );
    }

    /// An editor link that answers nothing, for building the companion's hooks without one.
    struct NoEditor;

    impl EditorRpc for NoEditor {
        fn exec_lua(&self, _code: &'static str, _args: Vec<rmpv::Value>) -> eitri_core::nvim_rpc::Pending {
            eitri_core::nvim_rpc::Pending::failed(eitri_core::nvim_rpc::RpcError::Unavailable("no editor".into()))
        }

        fn target(&self) -> Option<u64> {
            None
        }
    }
}
