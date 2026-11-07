//! The window's `init.lua` keys are read by `eitri_panel::window_config`, which has no toolkit and
//! needs no Lua panel registry. This is the shell's thin front: it hands that loader the ids and keys
//! of the Lua panels the GTK registry holds, and re-exports what the windows use.

use eitri_panel::window_config::LuaPanelMeta;
pub(crate) use eitri_panel::window_config::WindowConfig;

use crate::lua::LuaEngine;

/// [`eitri_panel::window_config::load`] with this engine's panels. A call `init.lua` made that was refused did
/// nothing and is one of the engine's startup problems, not a failure here.
pub(crate) fn load(lua_engine: &LuaEngine) -> Result<WindowConfig, String> {
    load_with_alt_mod(lua_engine, || None)
}

/// The full window's loader with the startup probe's current answer; other window kinds keep the shared defaults.
pub(crate) fn load_with_alt_mod(
    lua_engine: &LuaEngine,
    probe_now: impl FnOnce() -> Option<eitri_core::keymap::alt_mod::Answer>,
) -> Result<WindowConfig, String> {
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
    eitri_panel::window_config::load_with_alt_mod(lua_engine, &panels, probe_now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_call_is_a_problem_with_the_init_path_and_the_window_config_loads() {
        let scratch = eitri_core::plugin::test_support::Scratch::new("lua-window-refusal");
        let init = scratch.0.join("init.lua");
        std::fs::write(&init, "pcall(eitri.layout.rule, 42, {})").unwrap();
        let engine = LuaEngine::new(scratch.0.clone()).unwrap();
        engine.load_init_file(&init);
        assert!(load(&engine).is_ok());
        let problems = engine.startup_problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains(&init.display().to_string()), "{problems:?}");
        assert!(problems[0].contains("eitri.layout.rule"), "{problems:?}");
    }
}
