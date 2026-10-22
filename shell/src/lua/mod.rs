//! The shell's embedded Lua extension kernel -- a separate, independent runtime from Neovim's
//! own internal Lua. Exposes exactly four v1
//! extension points under an `eitri` global table: `panel.register`, `command.register`, `on`,
//! `config.get`/`config.set`, plus `keymap.prefix`/`keymap.set`/`keymap.del`.
//!
//! The runtime itself is `eitri_core::lua::kernel::Kernel`; what this module adds is the one thing
//! that needs a display: the registry of Lua panels, each holding a WebKitGTK view.

mod panel;

pub(crate) use eitri_core::lua::panel::PanelSlot;
pub(crate) use panel::{install_crash_recovery_into, PanelEntry, PanelRegistry};

use eitri_core::lua::kernel::Kernel;
use std::cell::RefCell;
use std::ops::Deref;
use std::path::PathBuf;
use std::rc::Rc;

/// The kernel plus the GTK registry of Lua panels. Everything but `panels` is the kernel's, reached
/// through `Deref` (`commands`, `config`, `layout`, `keymap`, `emit`, `invoke_command`,
/// `load_init_file`, `run_and_check_init_file`).
pub(crate) struct LuaEngine {
    kernel: Kernel,
    pub(crate) panels: Rc<RefCell<PanelRegistry>>,
}

impl Deref for LuaEngine {
    type Target = Kernel;

    fn deref(&self) -> &Kernel {
        &self.kernel
    }
}

impl LuaEngine {
    pub(crate) fn new(config_dir: PathBuf) -> mlua::Result<Self> {
        let panels = Rc::new(RefCell::new(PanelRegistry::default()));
        let for_panels = panels.clone();
        let kernel = Kernel::new(config_dir.clone(), move |lua, eitri| {
            panel::install(lua, eitri, for_panels, config_dir)
        })?;
        Ok(Self { kernel, panels })
    }

    /// The engine for a window that shows no plugin pages (the companion window): panels are still
    /// recorded so the window can say how many it ignores, but `eitri.plugin.load` logs once and does
    /// nothing, so it neither fails the start nor writes anything under a plugin's state directory.
    pub(crate) fn without_plugins(config_dir: PathBuf) -> mlua::Result<Self> {
        let panels = Rc::new(RefCell::new(PanelRegistry::default()));
        let for_panels = panels.clone();
        let kernel = Kernel::new(config_dir.clone(), move |lua, eitri| {
            panel::install(lua, eitri, for_panels, config_dir)?;
            eitri_core::lua::plugin::refuse_plugins(lua, eitri)
        })?;
        Ok(Self { kernel, panels })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sandbox and stores are the kernel's own tests; this checks only the join: the engine
    /// builds with no display, starts with no panels, and reaches the kernel's stores.
    #[test]
    fn the_engine_starts_with_no_panels_and_reaches_the_kernels_stores() {
        let engine = LuaEngine::new(std::env::temp_dir().join("eitri-no-such-config")).unwrap();
        assert!(engine.panels.borrow().entries().is_empty());
        assert!(engine.commands.borrow().is_empty());
        assert!(engine.config.borrow().get("agent.font_size").is_none());
    }

    /// A plugin load that would fail under the real call (no such directory) and one naming a real
    /// plugin both pass as no-ops: the start is not refused and nothing is recorded.
    #[test]
    fn the_engine_without_plugins_ignores_every_plugin_load() {
        let scratch = eitri_core::plugin::test_support::Scratch::new("companion-engine");
        let config_dir = scratch.0.join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let (plugin_dir, _settings) = eitri_core::plugin::test_support::demo_plugin(&scratch.0);
        let missing = scratch.0.join("no-such-plugin");
        std::fs::write(
            config_dir.join("init.lua"),
            format!(
                "eitri.plugin.load({:?})\neitri.plugin.load({:?})\n",
                missing.display().to_string(),
                plugin_dir.display().to_string()
            ),
        )
        .unwrap();
        let engine = LuaEngine::without_plugins(config_dir.clone()).unwrap();
        assert_eq!(engine.run_and_check_init_file(&config_dir.join("init.lua")), Ok(()));
        assert!(engine.plugins.borrow().refused().is_none());
        assert!(engine.plugins.borrow().plugins().is_empty());

        // The full window's engine refuses the same missing directory, so the no-op above is the
        // companion engine's own doing.
        let missing_only = config_dir.join("missing.lua");
        std::fs::write(
            &missing_only,
            format!("eitri.plugin.load({:?})\n", missing.display().to_string()),
        )
        .unwrap();
        let full = LuaEngine::new(config_dir.clone()).unwrap();
        assert!(full.run_and_check_init_file(&missing_only).is_err());
    }
}
