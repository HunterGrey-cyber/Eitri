//! Every module host's own controllers, from one function: `wire_host` installs on a host exactly
//! what `plan_host` says its kind gets, and nothing else in the shell installs them.
//!
//! One place, because one of these controllers guards permission cards: the module stage is what
//! keeps `Super+a` from answering a card in a page that cannot see Super (`panel_super.rs`). A host
//! made by a path that wired its own controllers could leave the stage off, and nothing on screen
//! would show it until a card was answered by a chord the window or the compositor owns. With every
//! host-creating path calling `wire_host`, a new path gets the stage or gets nothing, and a source
//! scan holds the shell to that.
//!
//! Every host gets the module stage: one capture-phase key controller that hands each key on its way
//! into the module to `WireDeps::module_stage` (the window controller's module stage, or the companion
//! window's own) through `keys::module_key`, which lets WebKit's second delivery of a key through
//! untouched. What the stage decides for each kind is recorded in its `Wired`:
//! - the editor: its `Ctrl+h/j/k/l` are nvim's own (vim-tmux-navigator calls out at a Neovim window
//!   boundary) while nvim runs, and the shell's when no nvim runs in it; and `Ctrl`+wheel zoom.
//! - a terminal: all four `Ctrl+h/j/k/l` are the window's even at an edge, and `Ctrl`+wheel zoom.
//! - a chat view, a Lua panel or a plugin page: the web modules' `Ctrl+h/j/k/l`, no key with Super
//!   or Hyper held, and `Ctrl`+wheel zoom. A chat view's page may keep a chord for itself; a Lua or
//!   plugin page never gets the chord at an edge, so what such a page does with a key never depends
//!   on where its module sits.
//!
//! A web module's host is the `WebHost` around its `WebView`, an ancestor of the focus widget rather
//! than the focus widget itself. Its capture-phase controller still sees every key first: GTK runs
//! capture-phase controllers on every ancestor of the target, top down, before any controller on the
//! target (`gtk_propagate_event_internal`, gtkmain.c, GTK 4.22.5), and WebKit's second delivery of a
//! key the page did not handle starts at the window again (`keys::LetThrough`).
//!
//! The order is the module stage, then the wheel zoom, which answers scroll events only.

use std::cell::RefCell;
use std::rc::Rc;

use eitri_core::layout::{ModuleId, ModuleKind};
use eitri_core::wm::{KeyPress, KeyStage, Verdict};
use gtk4::prelude::*;

use crate::gtk_host::keys::{key_press, module_key, LetThrough, SameEvent};
use crate::text_size::TextSizeController;

/// Which controllers a host gets: one kind per module the shell hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WireKind {
    Editor,
    Terminal,
    Chat,
    LuaPanel,
    PluginPage,
}

impl WireKind {
    /// The kind of `id`'s host; `is_plugin` says whether a Lua panel's module is a plugin's page. `None`
    /// for a kind the shell never hosts.
    pub(crate) fn of_module(id: &ModuleId, is_plugin: bool) -> Option<WireKind> {
        match id.kind() {
            ModuleKind::Editor => Some(WireKind::Editor),
            ModuleKind::Terminal => Some(WireKind::Terminal),
            ModuleKind::Agent => Some(WireKind::Chat),
            ModuleKind::LuaWebview if is_plugin => Some(WireKind::PluginPage),
            ModuleKind::LuaWebview => Some(WireKind::LuaPanel),
            ModuleKind::Canvas => None,
        }
    }
}

/// What a host of one kind gets: the rules its module stage applies, and its wheel zoom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Wired {
    /// The web modules' `Ctrl+h/j/k/l`.
    pub nav: bool,
    /// The terminal's own `Ctrl+h/j/k/l`, all four taken even at an edge.
    pub terminal_nav: bool,
    /// The editor's `Ctrl+h/j/k/l` while no nvim runs in it.
    pub editor_nav: bool,
    /// A chord with no module that way stops at the host instead of reaching the page.
    pub swallow_at_edge: bool,
    /// No key pressed with Super or Hyper held reaches the page (`panel_super`).
    pub super_filter: bool,
    /// `Ctrl`+wheel sets that module's text size.
    pub wheel_zoom: bool,
}

impl Wired {
    /// Whether the host's keys go through a module stage at all.
    fn module_stage(&self) -> bool {
        self.nav || self.terminal_nav || self.editor_nav || self.super_filter
    }
}

/// What `wire_host` installs on a host of `kind`.
pub(crate) fn plan_host(kind: WireKind) -> Wired {
    let none = Wired {
        nav: false,
        terminal_nav: false,
        editor_nav: false,
        swallow_at_edge: false,
        super_filter: false,
        wheel_zoom: false,
    };
    match kind {
        WireKind::Editor => Wired {
            editor_nav: true,
            wheel_zoom: true,
            ..none
        },
        WireKind::Terminal => Wired {
            terminal_nav: true,
            wheel_zoom: true,
            ..none
        },
        WireKind::Chat => Wired {
            nav: true,
            super_filter: true,
            wheel_zoom: true,
            ..none
        },
        WireKind::LuaPanel | WireKind::PluginPage => Wired {
            nav: true,
            swallow_at_edge: true,
            super_filter: true,
            wheel_zoom: true,
            ..none
        },
    }
}

/// The stage's answer for a key a page must never see: a Super or Hyper chord is kept from the page even when the
/// stage could not decide it (a window torn down, or one not yet laid out), since a page reads it as a plain key
/// and a plain key may answer a permission card.
fn withhold_from_page(withheld: bool, verdict: Verdict) -> Verdict {
    match verdict {
        Verdict::PassThrough if withheld => Verdict::Consumed,
        verdict => verdict,
    }
}

/// What a key on its way into a module is answered with: the module's own stage, built once per host so a key
/// copies nothing, and the key as the controller reads it.
pub(crate) type ModuleStage = Rc<dyn Fn(&KeyStage, KeyPress) -> Verdict>;

/// What the controllers act through.
#[derive(Clone)]
pub(crate) struct WireDeps {
    /// Decides each key on its way into a module.
    pub module_stage: ModuleStage,
    /// The text size controller the wheel zoom steps.
    pub text_size: Rc<TextSizeController>,
}

/// Installs on `host`, module `id`'s host widget, the controllers `plan_host(kind)` names, and
/// returns that plan. Call it once per host, when the host is made.
pub(crate) fn wire_host(kind: WireKind, id: &ModuleId, host: &gtk4::Widget, deps: &WireDeps) -> Wired {
    let wired = plan_host(kind);
    debug_assert!(
        !wired.swallow_at_edge || wired.nav,
        "only a host whose chords move the keys can swallow one at an edge"
    );
    if wired.module_stage() {
        // Capture phase on the host, so the stage answers before the module's own key handling: the terminal's
        // key controller, which hands every key to its shell, and a `WebView`, which consumes a key before a
        // bubble-phase controller on the same widget would run.
        let at = KeyStage::Module(id.clone());
        let stage = deps.module_stage.clone();
        let super_filter = wired.super_filter;
        let let_through = RefCell::new(LetThrough::new());
        let controller = gtk4::EventControllerKey::new();
        controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        controller.connect_key_pressed(move |controller, key, keycode, state| {
            let event = controller.current_event().map(SameEvent);
            let press = key_press(key, keycode, state);
            let withheld = super_filter && press.super_or_hyper;
            module_key(&let_through, event, || withhold_from_page(withheld, stage(&at, press)))
        });
        host.add_controller(controller);
    }
    if wired.wheel_zoom {
        crate::wheel_zoom::install(id.clone(), host, deps.text_size.clone());
    }
    wired
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::source_scan as scan;

    fn build_ui_code() -> String {
        let (_, code) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
        code.split_whitespace().collect()
    }

    /// The body of the block bound by `anchor` (whitespace removed), from its first `{`.
    fn bound_block<'a>(code: &'a str, anchor: &str) -> &'a str {
        let at = code.find(anchor).unwrap_or_else(|| panic!("`{anchor}` in build_ui"));
        scan::braced(code, at + code[at..].find('{').unwrap())
    }

    /// A Super chord on a page's host is kept from the page when the stage could not decide it, and nothing else
    /// the stage answers is changed.
    #[test]
    fn a_super_chord_the_stage_could_not_decide_never_reaches_the_page() {
        assert_eq!(withhold_from_page(true, Verdict::PassThrough), Verdict::Consumed);
        assert_eq!(withhold_from_page(true, Verdict::Consumed), Verdict::Consumed);
        assert_eq!(withhold_from_page(true, Verdict::LetThrough), Verdict::LetThrough);
        assert_eq!(withhold_from_page(false, Verdict::PassThrough), Verdict::PassThrough);
        assert_eq!(withhold_from_page(false, Verdict::LetThrough), Verdict::LetThrough);
    }

    #[test]
    fn plan_host_gives_each_kind_todays_controllers() {
        let plan = |nav, terminal_nav, editor_nav, swallow_at_edge, super_filter, wheel_zoom| Wired {
            nav,
            terminal_nav,
            editor_nav,
            swallow_at_edge,
            super_filter,
            wheel_zoom,
        };
        assert_eq!(
            plan_host(WireKind::Editor),
            plan(false, false, true, false, false, true),
            "the editor's chords are nvim's own while it runs, and the shell's without it"
        );
        assert_eq!(
            plan_host(WireKind::Terminal),
            plan(false, true, false, false, false, true),
            "a terminal gives up its chords and is no web page"
        );
        assert_eq!(
            plan_host(WireKind::Chat),
            plan(true, false, false, false, true, true),
            "a chat gets the chord at an edge, as its own keys expect"
        );
        assert_eq!(
            plan_host(WireKind::LuaPanel),
            plan(true, false, false, true, true, true),
            "a Lua page never gets the chord at an edge"
        );
        assert_eq!(
            plan_host(WireKind::PluginPage),
            plan(true, false, false, true, true, true),
            "a plugin page is wired as a Lua page"
        );
        for kind in [
            WireKind::Editor,
            WireKind::Terminal,
            WireKind::Chat,
            WireKind::LuaPanel,
            WireKind::PluginPage,
        ] {
            assert!(
                plan_host(kind).module_stage(),
                "{kind:?}'s keys go through the module stage"
            );
        }
    }

    #[test]
    fn a_module_kind_names_its_wiring() {
        assert_eq!(WireKind::of_module(&ModuleId::editor(), false), Some(WireKind::Editor));
        assert_eq!(
            WireKind::of_module(&ModuleId::terminal(), false),
            Some(WireKind::Terminal)
        );
        assert_eq!(WireKind::of_module(&ModuleId::agent(), false), Some(WireKind::Chat));
        assert_eq!(
            WireKind::of_module(&ModuleId::lua("notes"), false),
            Some(WireKind::LuaPanel)
        );
        assert_eq!(
            WireKind::of_module(&ModuleId::lua("md"), true),
            Some(WireKind::PluginPage)
        );
    }

    #[test]
    fn a_chat_view_opened_at_runtime_swallows_super_chords() {
        assert!(plan_host(WireKind::Chat).super_filter);
        let code = build_ui_code();
        let body = bound_block(&code, "lethost_chat_view:");
        assert_eq!(
            body.matches("wire_host(WireKind::Chat,").count(),
            1,
            "the runtime path wires its chat view's host as a chat, once"
        );
    }

    #[test]
    fn every_startup_lua_host_is_wired_once_deferred_ones_included() {
        let plugin = plan_host(WireKind::PluginPage);
        assert!(plugin.nav && plugin.swallow_at_edge && plugin.super_filter);

        let code = build_ui_code();
        let runtime: Vec<&str> = ["lethost_terminal:", "lethost_chat_view:"]
            .iter()
            .map(|anchor| bound_block(&code, anchor))
            .collect();
        let startup = scan::without(&code, &runtime);
        let calls: Vec<usize> = startup.match_indices("wire_host(").map(|(at, _)| at).collect();
        assert_eq!(
            calls.len(),
            1,
            "outside the runtime paths, build_ui wires its hosts from one place"
        );
        let head = "for(id,host)ingrid.hosts(){";
        let loops: Vec<usize> = startup.match_indices(head).map(|(at, _)| at).collect();
        let body = loops
            .iter()
            .map(|at| scan::braced(&startup, at + head.len() - 1))
            .find(|body| body.contains("wire_host("))
            .expect("the call sits in a loop over every host the grid holds");
        let at = body.as_ptr() as usize - startup.as_ptr() as usize;
        assert!(at < calls[0] && calls[0] < at + body.len());
        for skip in ["is_built(", "starts_hidden(", "deferred"] {
            assert!(
                !body.contains(skip),
                "a plugin page built later has its host already: the loop wires it whatever `{skip}` says"
            );
        }
    }
}
