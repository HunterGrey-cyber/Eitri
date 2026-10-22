//! The window's `init.lua` keys are read by `eitri_panel::window_config`, which has no toolkit and
//! needs no Lua panel registry. This is the shell's thin front: it hands that loader the ids and keys
//! of the Lua panels the GTK registry holds, and re-exports what the windows use.

use eitri_panel::window_config::LuaPanelMeta;
pub(crate) use eitri_panel::window_config::WindowConfig;

use crate::lua::LuaEngine;

/// [`eitri_panel::window_config::load`] with this engine's panels.
pub(crate) fn load(lua_engine: &LuaEngine) -> Result<WindowConfig, String> {
    if let Some(message) = lua_engine.init_refusal() {
        return Err(message);
    }
    let panels: Vec<LuaPanelMeta> = lua_engine
        .panels
        .borrow()
        .entries()
        .iter()
        .map(|entry| LuaPanelMeta {
            id: entry.id.clone(),
            key: entry.key.clone(),
        })
        .collect();
    eitri_panel::window_config::load(lua_engine, &panels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lua_refusals_reach_window_config_with_the_init_path() {
        let scratch = eitri_core::plugin::test_support::Scratch::new("lua-window-refusal");
        let init = scratch.0.join("init.lua");
        std::fs::write(&init, "pcall(eitri.layout.rule, 42, {})").unwrap();
        let engine = LuaEngine::new(scratch.0.clone()).unwrap();
        let error = engine.run_and_check_init_file(&init).unwrap_err();
        assert!(error.contains(&init.display().to_string()), "{error}");
        assert_eq!(load(&engine).err(), Some(error));
    }
}
