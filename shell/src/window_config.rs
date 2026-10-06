//! The window's `init.lua` keys are read by `eitri_panel::window_config`, which has no toolkit and
//! needs no Lua panel registry. This is the shell's thin front: it hands that loader the ids and keys
//! of the Lua panels the GTK registry holds, and re-exports what the windows use.

use eitri_panel::window_config::LuaPanelMeta;
pub(crate) use eitri_panel::window_config::WindowConfig;

use crate::lua::LuaEngine;

/// [`eitri_panel::window_config::load`] with this engine's panels.
pub(crate) fn load(lua_engine: &LuaEngine) -> Result<WindowConfig, String> {
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
