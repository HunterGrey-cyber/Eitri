//! Every module host's own controllers, from one function: `wire_host` installs on a host exactly
//! what `plan_host` says its kind gets, and nothing else in the shell installs them.
//!
//! One place, because one of these controllers guards permission cards: the Super filter
//! (`panel_super.rs`) is what keeps `Super+a` from answering a card in a page that cannot see Super.
//! A host made by a path that wired its own controllers could leave that filter off, and nothing on
//! screen would show it until a card was answered by a chord the window or the compositor owns.
//! With every host-creating path calling `wire_host`, a new path gets the filter or gets nothing,
//! and a source scan holds the shell to that.
//!
//! What each kind gets:
//! - the editor: `Ctrl`+wheel zoom only. Its `Ctrl+h/j/k/l` are nvim's own (vim-tmux-navigator
//!   calls out at a Neovim window boundary), and its page is not a web page.
//! - a terminal: its own capture-phase `Ctrl+h/j/k/l`, which swallows all four even at an edge, and
//!   `Ctrl`+wheel zoom.
//! - a chat view, a Lua panel or a plugin page: the web modules' `Ctrl+h/j/k/l` (`install_module_nav`),
//!   the Super filter and `Ctrl`+wheel zoom. A chat view's chord first asks its page's intercept; a
//!   Lua or plugin page never gets the chord at an edge, so what such a page does with a key never
//!   depends on where its module sits.
//!
//! A web module's host is the `WebHost` around its `WebView`, an ancestor of the focus widget rather
//! than the focus widget itself. Its capture-phase controllers still see every key first: GTK runs
//! capture-phase controllers on every ancestor of the target, top down, before any controller on the
//! target (`gtk_propagate_event_internal`, gtkmain.c, GTK 4.22.5), and WebKit's second delivery of a
//! key the page did not handle starts at the window again (`pane_switch::LetThrough`).
//!
//! The order is the navigation, then the Super filter, then the wheel zoom. The first two are both
//! capture-phase key controllers on the same widget, which GTK runs in the order they were added; the
//! wheel zoom answers scroll events only.

use std::rc::Rc;

use eitri_core::layout::{Direction, ModuleId, ModuleKind};
use eitri_core::tabs::ViewId;
use gtk4::glib;
use gtk4::prelude::*;

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

/// What a host of one kind gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Wired {
    /// The web modules' `Ctrl+h/j/k/l` (`install_module_nav`).
    pub nav: bool,
    /// The terminal's own `Ctrl+h/j/k/l`.
    pub terminal_nav: bool,
    /// A chord with no module that way stops at the host instead of reaching the page.
    pub swallow_at_edge: bool,
    /// No key pressed with Super or Hyper held reaches the page (`panel_super`).
    pub super_filter: bool,
    /// `Ctrl`+wheel sets that module's text size.
    pub wheel_zoom: bool,
}

/// What `wire_host` installs on a host of `kind`.
pub(crate) fn plan_host(kind: WireKind) -> Wired {
    let none = Wired {
        nav: false,
        terminal_nav: false,
        swallow_at_edge: false,
        super_filter: false,
        wheel_zoom: false,
    };
    match kind {
        WireKind::Editor => Wired {
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

/// Moves the keys from a module toward a direction; `false` when no module is that way.
pub(crate) type MoveFocus = Rc<dyn Fn(&ModuleId, Direction) -> bool>;
/// Whether a chat page claims a `Ctrl+h/j/k/l` for itself (its composer).
pub(crate) type NavIntercept = Rc<dyn Fn(Direction) -> bool>;

/// What the controllers act through.
#[derive(Clone)]
pub(crate) struct WireDeps {
    /// Moves the keys from a module toward a direction.
    pub move_focus: MoveFocus,
    /// A chat view's intercept, asked for by its view.
    pub nav_intercept: Rc<dyn Fn(ViewId) -> NavIntercept>,
    /// The text size controller the wheel zoom steps.
    pub text_size: Rc<TextSizeController>,
}

/// Installs on `host`, module `id`'s host widget, the controllers `plan_host(kind)` names, and
/// returns that plan. Call it once per host, when the host is made.
pub(crate) fn wire_host(kind: WireKind, id: &ModuleId, host: &gtk4::Widget, deps: &WireDeps) -> Wired {
    let wired = plan_host(kind);
    if wired.nav {
        let intercept = match kind {
            WireKind::Chat => ViewId::of_module(id).map(|view| (deps.nav_intercept)(view)),
            _ => None,
        };
        crate::install_module_nav(
            id.clone(),
            host,
            deps.move_focus.clone(),
            intercept,
            wired.swallow_at_edge,
        );
    }
    if wired.terminal_nav {
        // `Ctrl+h/j/k/l` in a terminal are Eitri's, always: capture phase on the terminal's own
        // widget, so the chord is claimed before the terminal's key controller hands it to the
        // shell. Unlike a web module's, all four are swallowed even with no module that way, so what
        // the shell receives never depends on the layout: `Ctrl+k` goes to the most recent module
        // above it, and the rest are tmux's no-op at the edge unless a module is there. Each moves
        // from the terminal's own id. The literal forms the shell loses are `Ctrl+a Ctrl+a` and
        // `Ctrl+a Ctrl+l`.
        let id = id.clone();
        let move_focus = deps.move_focus.clone();
        let controller = gtk4::EventControllerKey::new();
        controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        controller.connect_key_pressed(move |_controller, key, _keycode, state| {
            match crate::terminal::navigation(key, state) {
                Some(direction) => {
                    move_focus(&id, direction);
                    glib::Propagation::Stop
                }
                None => glib::Propagation::Proceed,
            }
        });
        host.add_controller(controller);
    }
    if wired.super_filter {
        crate::panel_super::install(host);
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

    #[test]
    fn plan_host_gives_each_kind_todays_controllers() {
        let plan = |nav, terminal_nav, swallow_at_edge, super_filter, wheel_zoom| Wired {
            nav,
            terminal_nav,
            swallow_at_edge,
            super_filter,
            wheel_zoom,
        };
        assert_eq!(
            plan_host(WireKind::Editor),
            plan(false, false, false, false, true),
            "the editor's chords are nvim's own: wheel zoom only"
        );
        assert_eq!(
            plan_host(WireKind::Terminal),
            plan(false, true, false, false, true),
            "a terminal claims its chords itself and is no web page"
        );
        assert_eq!(
            plan_host(WireKind::Chat),
            plan(true, false, false, true, true),
            "a chat gets the chord at an edge, as its own keys expect"
        );
        assert_eq!(
            plan_host(WireKind::LuaPanel),
            plan(true, false, true, true, true),
            "a Lua page never gets the chord at an edge"
        );
        assert_eq!(
            plan_host(WireKind::PluginPage),
            plan(true, false, true, true, true),
            "a plugin page is wired as a Lua page"
        );
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
