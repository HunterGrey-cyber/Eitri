//! Which controllers each kind of module host gets, as a value: `plan_host` decides, and the host's toolkit half
//! (`wire_host` in the GTK shell) installs exactly that. What each rule means for a key is the module stage's
//! (`core::wm::keys`); this only says which rules a kind's host has.

use eitri_core::layout::{ModuleId, ModuleKind};

/// Which controllers a host gets: one kind per module the shell hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireKind {
    Editor,
    Terminal,
    Chat,
    LuaPanel,
    PluginPage,
}

impl WireKind {
    /// The kind of `id`'s host; `is_plugin` says whether a Lua panel's module is a plugin's page. `None`
    /// for a kind the shell never hosts.
    pub fn of_module(id: &ModuleId, is_plugin: bool) -> Option<WireKind> {
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
pub struct Wired {
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
    pub fn module_stage(&self) -> bool {
        self.nav || self.terminal_nav || self.editor_nav || self.super_filter
    }
}

/// What `wire_host` installs on a host of `kind`.
pub fn plan_host(kind: WireKind) -> Wired {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
