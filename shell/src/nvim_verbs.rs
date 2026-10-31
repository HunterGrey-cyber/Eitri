//! The window's `:Eitri` (`eitri_core::nvim_verbs`): installed at each editor session the feeds report, polled from
//! a GLib timer only while a call to nvim is out, and its words carried out as the layout calls they stand for.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use eitri_core::editor_rpc::EditorRpc;
use eitri_core::keymap::Action;
use eitri_core::layout::ModuleId;
use eitri_core::lua::layout::LayoutRequest;
use eitri_core::nvim_verbs::install::VerbsInstaller;
use eitri_core::nvim_verbs::{parse_words, CommandWords, NvimVerb};
use eitri_core::wm::WinId;
use gtk4::glib;

/// What the window does for one `:Eitri` line.
#[derive(Debug, PartialEq)]
pub(crate) enum Carried {
    /// A request the controller decides, as `eitri.layout` would have queued it.
    Request(LayoutRequest),
    /// The key sheet's action, run as `<prefix> ?` runs it.
    KeySheet(Action),
    /// `window.setup`, the action that opens the first-start screen.
    Setup(Action),
    /// The words were refused; a log line and a toast say why.
    Refused(String),
}

/// What `words` ask of the window; `lua_panels` are the ids of the Lua panels `init.lua` registered.
pub(crate) fn carry(words: &[String], lua_panels: &[String]) -> Carried {
    match parse_words(words, lua_panels) {
        Ok(NvimVerb::Request(request)) => Carried::Request(request),
        Ok(NvimVerb::Keys) => Carried::KeySheet(Action::PanelKeymap),
        Ok(NvimVerb::Setup) => Carried::Setup(Action::WindowSetup),
        Err(why) => Carried::Refused(why),
    }
}

/// What `:Eitri` completes: every action name `run` accepts (the bindings' names add the `module.<id>` of the Lua
/// panels), and the module ids of the built-in modules and of every Lua panel, keyed or not.
pub(crate) fn completion_words(bound_actions: &[String], keyed: &[ModuleId], lua_panels: &[ModuleId]) -> CommandWords {
    let actions: Vec<String> = eitri_core::keymap::action::NAMES
        .iter()
        .map(|name| name.to_string())
        .chain(bound_actions.iter().cloned())
        .collect();
    let mut modules = keyed.to_vec();
    for id in lua_panels {
        if !modules.contains(id) {
            modules.push(id.clone());
        }
    }
    eitri_core::nvim_verbs::completion_words(&actions, &modules)
}

pub(crate) struct NvimVerbs {
    installer: RefCell<VerbsInstaller>,
    timer: Cell<Option<glib::SourceId>>,
}

impl NvimVerbs {
    /// The installer for window `win`, or `None` where no `:Eitri` is defined: only the main window with an embedded
    /// nvim has one (window 2 has no nvim, and a companion's is the user's own session). `args` is asked only then;
    /// it is the snippet's argument, and `None` (no pane-switch socket to send to) means no command either.
    pub(crate) fn for_window(
        win: WinId,
        owns_editor: bool,
        args: impl FnOnce() -> Option<serde_json::Value>,
    ) -> Option<Rc<NvimVerbs>> {
        if win != WinId::MAIN || !owns_editor {
            return None;
        }
        let args = args()?;
        let mut installer = VerbsInstaller::new();
        installer.set_args(args);
        Some(Rc::new(NvimVerbs {
            installer: RefCell::new(installer),
            timer: Cell::new(None),
        }))
    }

    /// The window's one editor handle.
    pub(crate) fn set_editor(&self, editor: Rc<dyn EditorRpc>) {
        self.installer.borrow_mut().set_editor(editor);
    }

    /// The feeds reported: the editor may have a new nvim session ready.
    pub(crate) fn editor_session(self: &Rc<Self>) {
        let next = self.installer.borrow_mut().editor_session(Instant::now());
        self.arm(next);
    }

    fn arm(self: &Rc<Self>, next: Option<Instant>) {
        if let Some(id) = self.timer.take() {
            id.remove();
        }
        let Some(at) = next else { return };
        let wait = at.saturating_duration_since(Instant::now());
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(wait, move || {
            let Some(verbs) = weak.upgrade() else { return };
            // This source has run and is gone; only a newer one may be removed from here on.
            verbs.timer.set(None);
            let next = verbs.installer.borrow_mut().poll(Instant::now());
            verbs.arm(next);
        });
        self.timer.set(Some(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::layout::{Axis, ModuleKind};

    fn words(line: &str) -> Vec<String> {
        line.split(' ').map(str::to_string).collect()
    }

    #[test]
    fn completion_offers_unbound_actions_and_keyless_lua_panels() {
        let bound = ["module.notes".to_string()];
        let words = completion_words(
            &bound,
            &[ModuleId::agent()],
            &[ModuleId::lua("notes"), ModuleId::agent()],
        );
        for expected in ["window.new", "layout.undo", "module.hide", "module.notes", "zoom"] {
            assert!(
                words.first.iter().any(|w| w == expected),
                "{expected} is missing from {words:?}"
            );
        }
        assert_eq!(words.modules[..2], ["agent", "lua:notes"]);
        assert_eq!(words.modules.iter().filter(|w| *w == "agent").count(), 1);
    }

    #[test]
    fn each_kind_of_verb_becomes_the_request_the_layout_call_queues() {
        let at = |line: &str| carry(&words(line), &[]);
        assert_eq!(
            at("split right agent"),
            Carried::Request(LayoutRequest::Split(ModuleId::agent(), Axis::Row))
        );
        assert_eq!(
            at("focus terminal"),
            Carried::Request(LayoutRequest::Focus(ModuleId::terminal()))
        );
        assert_eq!(
            at("open terminal"),
            Carried::Request(LayoutRequest::Open(ModuleKind::Terminal, None))
        );
        assert_eq!(at("workspace w2"), Carried::Request(LayoutRequest::Switch("w2".into())));
        assert_eq!(at("zoom"), Carried::Request(LayoutRequest::Run(Action::Zoom)));
        assert_eq!(at("keys"), Carried::KeySheet(Action::PanelKeymap));
    }

    #[test]
    fn carry_setup_is_the_window_setup_action() {
        assert_eq!(carry(&words("setup"), &[]), Carried::Setup(Action::WindowSetup));
        match carry(&words("setup now"), &[]) {
            Carried::Refused(why) => assert!(why.contains("setup") && why.contains("no words"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn words_nothing_runs_are_refused_with_the_reason() {
        match carry(&words("split sideways agent"), &[]) {
            Carried::Refused(why) => assert!(why.starts_with(":Eitri split"), "{why}"),
            other => panic!("{other:?}"),
        }
        match carry(&words("no-such-action"), &[]) {
            Carried::Refused(why) => assert!(why.starts_with(":Eitri "), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn window_2_and_the_companion_install_no_eitri_command() {
        let args = || Some(serde_json::json!({"socket": "/x", "words": []}));
        assert!(NvimVerbs::for_window(WinId::MAIN, true, args).is_some());
        // Window 2 embeds no nvim; a companion's nvim is the user's own.
        assert!(NvimVerbs::for_window(WinId::MAIN, false, || panic!("asked for the snippet's argument")).is_none());
        assert!(NvimVerbs::for_window(WinId(2), true, || panic!("asked for the snippet's argument")).is_none());
        // No socket to send to: no command.
        assert!(NvimVerbs::for_window(WinId::MAIN, true, || None).is_none());
    }
}
