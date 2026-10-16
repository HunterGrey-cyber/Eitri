//! `eitri.plugin.load(dir, opts)`: a local plugin, its manifest checked and its settings, roots and
//! directories made ready while `init.lua` runs. The window turns each loaded plugin into a Lua panel module
//! `lua:<id>` whose content is the plugin's page. Every refusal is a startup failure naming the call
//! (`PluginRegistry::refused`, checked after `init.lua` ran, so a `pcall` around the call changes nothing).

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{Lua, Table, Value};

use super::panel::PanelSlot;
use crate::plugin::load::{load_plugin, LoadedPlugin};
use crate::plugin::state::PluginStore;

const PREFIX: &str = "eitri.plugin.load";

pub struct PluginRegistration {
    pub plugin: Rc<LoadedPlugin>,
    /// The panel's module key after the prefix, as written; checked with every panel's by `ModuleKeys::build`.
    pub key: Option<String>,
    /// Where the panel goes the first time; `side` unless `position` says otherwise.
    pub slot: PanelSlot,
}

#[derive(Default)]
pub struct PluginRegistry {
    plugins: Vec<PluginRegistration>,
    refused: Option<String>,
}

impl PluginRegistry {
    pub fn plugins(&self) -> &[PluginRegistration] {
        &self.plugins
    }

    pub fn refused(&self) -> Option<&str> {
        self.refused.as_deref()
    }

    fn refuse(&mut self, message: String) {
        if self.refused.is_none() {
            self.refused = Some(message);
        }
    }
}

fn text(value: Value, what: &str, dir: &str) -> Result<String, String> {
    match value {
        Value::String(s) => s
            .to_str()
            .map(|t| t.to_string())
            .map_err(|_| format!("{PREFIX}({dir}): {what} is not UTF-8")),
        _ => Err(format!("{PREFIX}({dir}): {what} must be a string")),
    }
}

fn register(
    dir: Value,
    opts: Value,
    state_root: Option<&Path>,
    home: Option<&Path>,
) -> Result<PluginRegistration, String> {
    let dir = text(dir, "the plugin directory", "?")?;
    let opts = match opts {
        Value::Nil => None,
        Value::Table(table) => Some(table),
        _ => return Err(format!("{PREFIX}({dir}): the options must be a table")),
    };
    let mut key = None;
    let mut slot = PanelSlot::Side;
    let mut settings = BTreeMap::new();
    if let Some(opts) = opts {
        for pair in opts.pairs::<Value, Value>() {
            let (k, v) = pair.map_err(|e| format!("{PREFIX}({dir}): the options cannot be read ({e})"))?;
            let k = text(k, "an option name", &dir)?;
            let v = text(v, &format!("option {k}"), &dir)?;
            match k.as_str() {
                "key" => key = Some(v),
                "position" => {
                    slot = PanelSlot::parse(&v)
                        .ok_or_else(|| format!("{PREFIX}({dir}): position {v:?} must be main, side or bottom"))?;
                }
                _ => {
                    settings.insert(k, v);
                }
            }
        }
    }
    let plugin = load_plugin(&dir, &settings, home).map_err(|e| format!("{PREFIX}({dir}): {e}"))?;
    if let Some(root) = state_root {
        let capabilities: Vec<&str> = plugin.manifest.capabilities.iter().map(|c| c.as_str()).collect();
        let recorded = PluginStore::new(root.to_path_buf(), &plugin.manifest.id).record_capabilities(&capabilities);
        match recorded {
            Ok(Some(note)) => eprintln!("[plugin {}] {note}", plugin.manifest.id),
            Ok(None) => {}
            Err(e) => {
                eprintln!(
                    "[plugin {}] its capability list could not be recorded: {}",
                    plugin.manifest.id, e.message
                )
            }
        }
    }
    Ok(PluginRegistration {
        plugin: Rc::new(plugin),
        key,
        slot,
    })
}

pub fn install(
    lua: &Lua,
    eitri: &Table,
    registry: Rc<RefCell<PluginRegistry>>,
    state_root: Option<PathBuf>,
    home: Option<PathBuf>,
) -> mlua::Result<()> {
    let table = lua.create_table()?;
    // Both arguments are taken as plain values and checked here, so a wrong type is a refusal this registry
    // keeps, not a conversion error `init.lua` could swallow.
    let load = lua.create_function(move |_, (dir, opts): (Value, Value)| {
        let outcome = register(dir, opts, state_root.as_deref(), home.as_deref()).and_then(|r| {
            let twice = registry
                .borrow()
                .plugins
                .iter()
                .any(|p| p.plugin.manifest.id == r.plugin.manifest.id);
            if twice {
                Err(format!("{PREFIX}: plugin {} is loaded twice", r.plugin.manifest.id))
            } else {
                Ok(r)
            }
        });
        match outcome {
            Ok(registration) => {
                registry.borrow_mut().plugins.push(registration);
                Ok(())
            }
            Err(message) => {
                registry.borrow_mut().refuse(message.clone());
                Err(mlua::Error::RuntimeError(message))
            }
        }
    })?;
    table.set("load", load)?;
    eitri.set("plugin", table)
}

/// For a window with no Lua panels (the companion window, `eitri-mac`): the call logs once and is ignored,
/// so an `init.lua` written for the full window still loads.
pub fn refuse_plugins(lua: &Lua, eitri: &Table) -> mlua::Result<()> {
    let told = Rc::new(Cell::new(false));
    let load = lua.create_function(move |_, _args: mlua::MultiValue| {
        if !told.replace(true) {
            eprintln!("eitri: plugins are not in this window");
        }
        Ok(())
    })?;
    let table = lua.create_table()?;
    table.set("load", load)?;
    eitri.set("plugin", table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::test_support::{demo_plugin, Scratch};

    fn engine(state: &std::path::Path) -> (Lua, Rc<RefCell<PluginRegistry>>) {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(PluginRegistry::default()));
        install(&lua, &eitri, registry.clone(), Some(state.to_path_buf()), None).unwrap();
        lua.globals().set("eitri", eitri).unwrap();
        (lua, registry)
    }

    #[test]
    fn a_plugin_loads_with_its_key_position_and_settings() {
        let s = Scratch::new("lua-load");
        let (dir, settings) = demo_plugin(&s.0);
        let (lua, registry) = engine(&s.0.join("state"));
        lua.load(format!(
            "eitri.plugin.load({:?}, {{ work = {:?}, key = 'y', position = 'main' }})",
            dir.display().to_string(),
            settings["work"]
        ))
        .exec()
        .unwrap();
        let registry = registry.borrow();
        let p = &registry.plugins()[0];
        assert_eq!(
            (p.plugin.manifest.id.as_str(), p.key.as_deref(), p.slot),
            ("demo", Some("y"), PanelSlot::Main)
        );
        assert!(s.0.join("state/plugins/demo/capabilities.json").exists());
        assert_eq!(registry.refused(), None);
    }

    #[test]
    fn a_refusal_is_kept_even_when_init_lua_catches_it() {
        let s = Scratch::new("lua-refuse");
        let (dir, _) = demo_plugin(&s.0);
        let (lua, registry) = engine(&s.0.join("state"));
        let d = dir.display().to_string();
        for (opts, named) in [
            ("{ wrok = '/x' }", "wrok"),
            ("{ key = 1 }", "key"),
            ("{ position = 'top' }", "position"),
            ("'not a table'", "options"),
        ] {
            let err: String = lua
                .load(format!(
                    "local ok, err = pcall(eitri.plugin.load, {d:?}, {opts}) assert(not ok) return tostring(err)"
                ))
                .eval()
                .unwrap();
            assert!(err.contains(named), "{opts}: {err}");
        }
        assert!(
            registry.borrow().refused().unwrap().contains("wrok"),
            "the first refusal is the one kept"
        );
        assert!(registry.borrow().plugins().is_empty());
    }

    #[test]
    fn the_same_plugin_twice_is_refused() {
        let s = Scratch::new("lua-twice");
        let (dir, settings) = demo_plugin(&s.0);
        let (lua, registry) = engine(&s.0.join("state"));
        let call = format!(
            "eitri.plugin.load({:?}, {{ work = {:?} }})",
            dir.display().to_string(),
            settings["work"]
        );
        lua.load(&call).exec().unwrap();
        assert!(lua.load(&call).exec().is_err());
        assert!(registry.borrow().refused().unwrap().contains("twice"));
    }

    #[test]
    fn a_window_without_plugins_logs_and_goes_on() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        refuse_plugins(&lua, &eitri).unwrap();
        lua.globals().set("eitri", eitri).unwrap();
        lua.load("eitri.plugin.load('/nonexistent', { work = '/x' })")
            .exec()
            .expect("no error: the call is ignored");
    }
}
