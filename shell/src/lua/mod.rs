//! The shell's embedded Lua extension kernel -- a separate, independent runtime from Neovim's
//! own internal Lua (see docs/canonical/neovibe_architecture_decisions.md §3). Exposes exactly four v1
//! extension points under an `eitri` global table: `panel.register`, `command.register`, `on`,
//! `config.get`/`config.set`, plus `keymap.prefix`/`keymap.set`/`keymap.del` (keymap spec §2.3).
//!
//! The runtime itself is `eitri_core::lua::kernel::Kernel`; what this module adds is the one thing
//! that needs a display: the registry of Lua panels, each holding a WebKitGTK view.

mod panel;

pub(crate) use eitri_core::lua::panel::PanelSlot;
pub(crate) use panel::PanelRegistry;

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
}
