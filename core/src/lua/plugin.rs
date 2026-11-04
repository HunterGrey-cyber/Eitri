//! `eitri.plugin.load(name_or_dir, opts)`: a local plugin, found by name on the plugin search path or given as a
//! directory, its manifest checked and its settings, roots and
//! directories made ready while `init.lua` runs. The window turns each loaded plugin into a Lua panel module
//! `lua:<id>` whose content is the plugin's page. Every refusal is a startup failure naming the call
//! recorded by the kernel at the function boundary during init, so a `pcall` cannot hide it.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{Lua, Table, Value};

use super::panel::PanelSlot;
use crate::layout::rules::{At, OpenRule, RuleSize};
use crate::plugin::load::{check_window_roots, load_plugin_with, LoadedPlugin};
use crate::plugin::manifest::{ModuleStart, SettingValue};
use crate::plugin::search::{classify, find_plugin, LoadArg, SearchEnv};
use crate::plugin::state::PluginStore;

const PREFIX: &str = "eitri.plugin.load";

pub struct PluginRegistration {
    pub plugin: Rc<LoadedPlugin>,
    /// The panel's module key after the prefix, as written; checked with every panel's by `ModuleKeys::build`.
    pub key: Option<String>,
    /// Where the panel goes the first time; `side` unless `position` says otherwise.
    pub slot: PanelSlot,
    /// The `position` option as `init.lua` gave it; `None` when it gave none.
    pub position: Option<PanelSlot>,
}

impl PluginRegistration {
    /// The plugin's own open rule, counted where a Lua panel's `position` counts: the `position` option when
    /// `init.lua` gave one (it says where only, so `focus` stays the manifest's), else the manifest's `[module]`
    /// `at`/`size`/`focus`, else none.
    pub fn own_rule(&self) -> Option<OpenRule> {
        let module = self.plugin.manifest.module.as_ref();
        if let Some(position) = self.position {
            return Some(OpenRule {
                at: Some(At::Placement(position.placement())),
                size: None,
                focus: module.is_none_or(|m| m.focus),
                float: false,
                scratchpad: false,
            });
        }
        module.map(|m| OpenRule {
            at: m.at,
            size: m.size.map(RuleSize::Share),
            focus: m.focus,
            float: false,
            scratchpad: false,
        })
    }

    /// `[module] start = "hidden"`: the window never places the module by itself; `position` does not change that.
    pub fn starts_hidden(&self) -> bool {
        self.plugin
            .manifest
            .module
            .as_ref()
            .is_some_and(|m| m.start == ModuleStart::Hidden)
    }
}

#[derive(Default)]
pub struct PluginRegistry {
    plugins: Vec<PluginRegistration>,
    init_refusals: Option<Rc<RefCell<super::kernel::InitRefusals>>>,
}

impl PluginRegistry {
    pub fn plugins(&self) -> &[PluginRegistration] {
        &self.plugins
    }

    pub fn refused(&self) -> Option<String> {
        self.init_refusals
            .as_ref()
            .and_then(|shared| shared.borrow().message("eitri.plugin.load"))
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

/// An option's value: a string, a boolean for a `bool` setting, or a whole number for an `int` setting (`16` and
/// `16.0` are both 16; `16.5` is refused rather than rounded; so is a float beyond +-9007199254740991, the integers a
/// float holds exactly and a page reads back unchanged).
fn option_value(value: Value, name: &str, dir: &str) -> Result<SettingValue, String> {
    match value {
        Value::Boolean(b) => Ok(SettingValue::Bool(b)),
        Value::Integer(n) => Ok(SettingValue::Int(i64::from(n))),
        Value::Number(f) if f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_991.0 => Ok(SettingValue::Int(f as i64)),
        Value::Number(f) => Err(format!(
            "{PREFIX}({dir}): option {name} = {f} must be a whole number within +-9007199254740991"
        )),
        Value::String(_) => text(value, &format!("option {name}"), dir).map(SettingValue::Text),
        _ => Err(format!(
            "{PREFIX}({dir}): option {name} must be a string, a boolean or a number"
        )),
    }
}

/// `arg` is a plugin name (a valid id, looked up on the plugin search path) or a directory (absolute after `~`, as
/// it always was). Lines the window should log go to `log`; the returned label says which place the plugin came
/// from (`path` for a load by path).
fn register(
    arg: Value,
    opts: Value,
    state_root: Option<&Path>,
    home: Option<&Path>,
    search: &SearchEnv,
    log: &dyn Fn(&str),
) -> Result<(PluginRegistration, &'static str), String> {
    let dir = text(arg, "the plugin name or directory", "?")?;
    let opts = match opts {
        Value::Nil => None,
        Value::Table(table) => Some(table),
        _ => return Err(format!("{PREFIX}({dir}): the options must be a table")),
    };
    let mut key = None;
    let mut position = None;
    let mut settings: BTreeMap<String, SettingValue> = BTreeMap::new();
    if let Some(opts) = opts {
        for pair in opts.pairs::<Value, Value>() {
            let (k, v) = pair.map_err(|e| format!("{PREFIX}({dir}): the options cannot be read ({e})"))?;
            let k = text(k, "an option name", &dir)?;
            let v = option_value(v, &k, &dir)?;
            match k.as_str() {
                "key" | "position" => {
                    let SettingValue::Text(v) = v else {
                        return Err(format!("{PREFIX}({dir}): option {k} must be a string"));
                    };
                    if k == "key" {
                        key = Some(v);
                    } else {
                        position =
                            Some(PanelSlot::parse(&v).ok_or_else(|| {
                                format!("{PREFIX}({dir}): position {v:?} must be main, side or bottom")
                            })?);
                    }
                }
                _ => {
                    settings.insert(k, v);
                }
            }
        }
    }
    let (raw_dir, label) = match classify(&dir) {
        LoadArg::Path(path) => (path.to_string(), "path"),
        LoadArg::Name(name) => {
            let found = find_plugin(name, search);
            let skipped = match &found {
                Ok(found) => &found.skipped,
                Err(not_found) => &not_found.skipped,
            };
            for empty in skipped {
                log(&format!(
                    "[plugin {name}] skipped {}: no eitri-plugin.toml",
                    empty.display()
                ));
            }
            match found {
                Ok(found) => {
                    let text = found
                        .dir
                        .to_str()
                        .ok_or_else(|| format!("{PREFIX}(\"{name}\"): {} is not UTF-8", found.dir.display()))?;
                    (text.to_string(), found.step.label())
                }
                Err(not_found) => {
                    let looked: Vec<String> = not_found.tried.iter().map(|d| d.display().to_string()).collect();
                    return Err(format!(
                        "{PREFIX}(\"{name}\"): no plugin named {name}; looked in {}",
                        looked.join(", ")
                    ));
                }
            }
        }
    };
    let plugin = load_plugin_with(&raw_dir, &settings, home).map_err(|e| format!("{PREFIX}({dir}): {e}"))?;
    // A load by name always gives the module, keymap table and origin the user wrote.
    if let LoadArg::Name(name) = classify(&dir) {
        if plugin.manifest.id != name {
            return Err(format!(
                "{PREFIX}(\"{name}\"): {} holds plugin {}, not {name}",
                plugin.dir.display(),
                plugin.manifest.id
            ));
        }
    }
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
    Ok((
        PluginRegistration {
            plugin: Rc::new(plugin),
            key,
            slot: position.unwrap_or(PanelSlot::Side),
            position,
        },
        label,
    ))
}

pub fn install(
    lua: &Lua,
    eitri: &Table,
    registry: Rc<RefCell<PluginRegistry>>,
    state_root: Option<PathBuf>,
    home: Option<PathBuf>,
    search: SearchEnv,
) -> mlua::Result<()> {
    install_with_log(
        lua,
        eitri,
        registry,
        state_root,
        home,
        search,
        Rc::new(|line: &str| eprintln!("{line}")),
    )
}

/// `install` with the window's log as a function, so tests can read the lines a load writes.
fn install_with_log(
    lua: &Lua,
    eitri: &Table,
    registry: Rc<RefCell<PluginRegistry>>,
    state_root: Option<PathBuf>,
    home: Option<PathBuf>,
    search: SearchEnv,
    log: Rc<dyn Fn(&str)>,
) -> mlua::Result<()> {
    let refusals = super::kernel::init_refusals(lua);
    registry.borrow_mut().init_refusals = Some(refusals.clone());
    let table = lua.create_table()?;
    // Each loaded plugin with the call that loaded it, so a refusal that involves two loads can name both.
    let calls: RefCell<Vec<(Rc<LoadedPlugin>, String)>> = RefCell::new(Vec::new());
    // Plain values let validation name the argument that cannot be loaded.
    let load = super::kernel::registering_function(lua, "eitri.plugin.load", move |l, (arg, opts): (Value, Value)| {
        // The window reads the loaded plugins once, when `init.lua` has run: a load after that would say it
        // loaded and show nothing.
        if !refusals.borrow().in_init() {
            return Err(mlua::Error::RuntimeError(format!(
                "{PREFIX}: only in init.lua, before the window exists"
            )));
        }
        let outcome = register(arg, opts, state_root.as_deref(), home.as_deref(), &search, &*log).and_then(
            |(registration, label)| {
                let twice = registry
                    .borrow()
                    .plugins
                    .iter()
                    .any(|p| p.plugin.manifest.id == registration.plugin.manifest.id);
                if twice {
                    return Err(format!(
                        "{PREFIX}: plugin {} is loaded twice",
                        registration.plugin.manifest.id
                    ));
                }
                // Two plugins that own one directory's window would each claim it; neither may win silently.
                let call = format!(
                    "plugin {} (loaded at {})",
                    registration.plugin.manifest.id,
                    call_site(l)
                );
                let earlier = calls.borrow();
                let mut all: Vec<(&LoadedPlugin, &str)> = earlier
                    .iter()
                    .map(|(plugin, call)| (&**plugin, call.as_str()))
                    .collect();
                all.push((&*registration.plugin, call.as_str()));
                check_window_roots(&all).map_err(|e| format!("{PREFIX}: {e}"))?;
                Ok((registration, label, call))
            },
        );
        match outcome {
            Ok((registration, label, call)) => {
                calls.borrow_mut().push((registration.plugin.clone(), call));
                log(&format!(
                    "[plugin {}] loaded {} ({label})",
                    registration.plugin.manifest.id,
                    registration.plugin.dir.display()
                ));
                registry.borrow_mut().plugins.push(registration);
                Ok(())
            }
            Err(message) => Err(mlua::Error::RuntimeError(message)),
        }
    })?;
    table.set("load", load)?;
    eitri.set("plugin", table)
}

/// Where the running `eitri.plugin.load` was called from: the nearest Lua frame's chunk and line (the call may
/// go through `pcall`, a C frame with no line), or `?` when no Lua frame has one.
fn call_site(lua: &Lua) -> String {
    (1..)
        .map_while(|level| {
            lua.inspect_stack(level, |frame| {
                let line = frame.current_line()?;
                let source = frame.source();
                // The window names `init.lua`'s chunk by its path; Lua's own short form would quote and
                // truncate it.
                let named = source
                    .source
                    .as_deref()
                    .map(|name| name.strip_prefix(['@', '=']).unwrap_or(name))
                    .filter(|name| !name.contains('\n'));
                let chunk = named.or(source.short_src.as_deref()).unwrap_or("?").to_string();
                Some(format!("{chunk}:{line}"))
            })
        })
        .flatten()
        .next()
        .unwrap_or_else(|| "?".to_string())
}

/// For a window with no Lua panels (the companion window, `eitri-mac`): the call logs once and is ignored,
/// so an `init.lua` written for the full window still loads.
pub fn refuse_plugins(lua: &Lua, eitri: &Table) -> mlua::Result<()> {
    let told = Rc::new(Cell::new(false));
    let load = super::kernel::registering_function(lua, "eitri.plugin.load", move |_, _args: mlua::MultiValue| {
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
    use crate::plugin::search::SearchEnv;
    use crate::plugin::test_support::{demo_plugin, Scratch};
    use std::ffi::OsString;

    /// A search with no user step and no system directory that exists: a load by name finds nothing unless a
    /// test puts a copy where `env` points.
    fn no_search(root: &std::path::Path) -> SearchEnv {
        SearchEnv {
            xdg_data_dirs: Some(root.join("no-system").into_os_string()),
            ..SearchEnv::default()
        }
    }

    /// The user data directory under `root/data`, the binary's directory `root/lib`, one system directory
    /// `root/sys`; no source tree step.
    fn search_in(root: &std::path::Path) -> SearchEnv {
        SearchEnv {
            xdg_data_home: Some(root.join("data").into_os_string()),
            home: None,
            xdg_data_dirs: Some(OsString::from(root.join("sys"))),
            exe_dir: Some(root.join("lib")),
            source_plugins: None,
        }
    }

    /// A window's Lua, its plugin registry, and the lines its loads logged.
    type Engine = (Lua, Rc<RefCell<PluginRegistry>>, Rc<RefCell<Vec<String>>>);

    fn engine_with(state: &std::path::Path, search: SearchEnv) -> Engine {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(PluginRegistry::default()));
        let lines = Rc::new(RefCell::new(Vec::new()));
        let sink = lines.clone();
        install_with_log(
            &lua,
            &eitri,
            registry.clone(),
            Some(state.to_path_buf()),
            None,
            search,
            Rc::new(move |line: &str| sink.borrow_mut().push(line.to_string())),
        )
        .unwrap();
        lua.globals().set("eitri", eitri).unwrap();
        (lua, registry, lines)
    }

    fn engine(state: &std::path::Path) -> (Lua, Rc<RefCell<PluginRegistry>>) {
        let (lua, registry, _) = engine_with(state, no_search(state));
        (lua, registry)
    }

    /// The demo plugin moved to `place` (any depth); returns its `work` setting.
    fn demo_at(root: &std::path::Path, place: &std::path::Path) -> String {
        let (dir, settings) = demo_plugin(root);
        std::fs::create_dir_all(place.parent().unwrap()).unwrap();
        std::fs::rename(&dir, place).unwrap();
        settings["work"].clone()
    }

    /// The refusal message of a call wrapped in `pcall`, asserting that it failed.
    fn refusal(lua: &Lua, call: &str) -> String {
        lua.load(format!(
            "local ok, err = pcall({call}) assert(not ok) return tostring(err)"
        ))
        .eval()
        .unwrap()
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
        assert_eq!(p.position, Some(PanelSlot::Main));
        assert!(s.0.join("state/plugins/demo/capabilities.json").exists());
        assert_eq!(registry.refused(), None);
    }

    #[test]
    fn position_replaces_where_but_not_whether_or_focus() {
        use crate::layout::module::Placement;
        use crate::layout::rules::parse_at;

        fn load(manifest: Option<&str>, position: Option<&str>) -> PluginRegistration {
            let s = Scratch::new("lua-own-rule");
            let (dir, settings) = demo_plugin(&s.0);
            if let Some(text) = manifest {
                std::fs::write(dir.join("eitri-plugin.toml"), text).unwrap();
            }
            let (lua, registry) = engine(&s.0.join("state"));
            let position = position.map(|p| format!(", position = {p:?}")).unwrap_or_default();
            lua.load(format!(
                "eitri.plugin.load({:?}, {{ work = {:?}{position} }})",
                dir.display().to_string(),
                settings["work"]
            ))
            .exec()
            .unwrap();
            registry.take().plugins.remove(0)
        }

        let p = load(None, None);
        assert_eq!(
            p.own_rule(),
            Some(OpenRule {
                at: parse_at("right"),
                size: Some(RuleSize::Share(0.4)),
                focus: false,
                float: false,
                scratchpad: false
            })
        );
        assert!(p.starts_hidden());

        let p = load(None, Some("bottom"));
        assert_eq!(
            p.own_rule(),
            Some(OpenRule {
                at: Some(At::Placement(Placement::BelowRoot)),
                size: None,
                focus: false,
                float: false,
                scratchpad: false
            })
        );
        assert!(
            p.starts_hidden(),
            "position never changes whether the module starts hidden"
        );

        let demo = include_str!("../plugin/testdata/demo-manifest.toml");
        let no_module = &demo[..demo.find("\n[module]\n").unwrap() + 1];
        let no_module = no_module.replacen(", \"editor.follow\", \"editor.commands\"]", "]", 1);
        let p = load(Some(&no_module), None);
        assert_eq!(p.plugin.manifest.module, None);
        assert_eq!(p.own_rule(), None);
        assert!(!p.starts_hidden());

        let p = load(Some(&no_module), Some("main"));
        assert_eq!(
            p.own_rule(),
            Some(OpenRule {
                at: Some(At::Placement(Placement::InPlaceOfEditor)),
                size: None,
                focus: true,
                float: false,
                scratchpad: false
            })
        );
        assert!(!p.starts_hidden());
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
    #[test]
    fn bool_and_choice_options_reach_the_plugin_and_no_position_is_none() {
        let s = Scratch::new("lua-kinds");
        let (dir, settings) = demo_plugin(&s.0);
        let (lua, registry) = engine(&s.0.join("state"));
        lua.load(format!(
            "eitri.plugin.load({:?}, {{ work = {:?}, auto = true, images = 'off' }})",
            dir.display().to_string(),
            settings["work"]
        ))
        .exec()
        .unwrap();
        let registry = registry.borrow();
        let p = &registry.plugins()[0];
        assert_eq!(p.plugin.bool_setting("auto"), Some(true));
        assert_eq!(p.plugin.values["images"], SettingValue::Text("off".into()));
        assert!(!p.plugin.remote_images);
        assert_eq!((p.slot, p.position), (PanelSlot::Side, None));
    }

    #[test]
    fn a_value_of_the_wrong_kind_is_a_refusal_naming_the_option() {
        let s = Scratch::new("lua-kinds-refused");
        let (dir, settings) = demo_plugin(&s.0);
        let (lua, registry) = engine(&s.0.join("state"));
        let d = dir.display().to_string();
        let w = settings["work"].clone();
        for (opts, named) in [
            (
                format!("{{ work = {w:?}, auto = 'yes' }}"),
                "a bool setting takes true or false",
            ),
            (
                format!("{{ work = {w:?}, images = 'http' }}"),
                "a choice setting takes one of \"https\", \"off\"",
            ),
            ("{ work = true }".to_string(), "a dir setting takes a path"),
            (
                format!("{{ work = {w:?}, auto = 1 }}"),
                "a bool setting takes true or false",
            ),
            (
                format!("{{ work = {w:?}, auto = {{}} }}"),
                "option auto must be a string, a boolean or a number",
            ),
            (
                format!("{{ work = {w:?}, auto = 1.5 }}"),
                "option auto = 1.5 must be a whole number",
            ),
            (
                format!("{{ work = {w:?}, auto = 1e300 }}"),
                "must be a whole number within +-9007199254740991",
            ),
            (
                // A whole float in range is a number like any other: the bool setting's own refusal, not this one.
                format!("{{ work = {w:?}, auto = 1e15 }}"),
                "a bool setting takes true or false",
            ),
            (format!("{{ work = {w:?}, key = true }}"), "option key must be a string"),
            (
                format!("{{ work = {w:?}, position = false }}"),
                "option position must be a string",
            ),
        ] {
            let err: String = lua
                .load(format!(
                    "local ok, err = pcall(eitri.plugin.load, {d:?}, {opts}) assert(not ok) return tostring(err)"
                ))
                .eval()
                .unwrap();
            assert!(err.contains(named), "{opts}: {err}");
        }
        assert!(registry.borrow().plugins().is_empty());
    }

    #[test]
    fn a_load_by_name_finds_the_user_copy_first_and_logs_where_it_came_from() {
        let s = Scratch::new("lua-name-user");
        let search = search_in(&s.0);
        let work = demo_at(&s.0.join("a"), &s.0.join("data/eitri/plugins/demo"));
        demo_at(&s.0.join("b"), &s.0.join("sys/eitri/plugins/demo"));
        let (lua, registry, lines) = engine_with(&s.0.join("state"), search);
        lua.load(format!("eitri.plugin.load('demo', {{ work = {work:?} }})"))
            .exec()
            .unwrap();
        let user = s.0.join("data/eitri/plugins/demo");
        assert_eq!(registry.borrow().plugins()[0].plugin.dir, user);
        assert_eq!(registry.borrow().refused(), None);
        assert_eq!(
            *lines.borrow(),
            vec![format!("[plugin demo] loaded {} (user data directory)", user.display())]
        );
    }

    #[test]
    fn the_copy_beside_the_binary_wins_over_the_system_copy() {
        let s = Scratch::new("lua-name-install");
        let work = demo_at(&s.0.join("a"), &s.0.join("lib/plugins/demo"));
        demo_at(&s.0.join("b"), &s.0.join("sys/eitri/plugins/demo"));
        let (lua, registry, lines) = engine_with(&s.0.join("state"), search_in(&s.0));
        lua.load(format!("eitri.plugin.load('demo', {{ work = {work:?} }})"))
            .exec()
            .unwrap();
        let own = s.0.join("lib/plugins/demo");
        assert_eq!(registry.borrow().plugins()[0].plugin.dir, own);
        assert_eq!(
            *lines.borrow(),
            vec![format!(
                "[plugin demo] loaded {} (beside the eitri binary)",
                own.display()
            )]
        );
    }

    #[test]
    fn a_directory_without_a_manifest_is_skipped_with_a_line_and_the_next_copy_loads() {
        let s = Scratch::new("lua-name-skip");
        std::fs::create_dir_all(s.0.join("data/eitri/plugins/demo")).unwrap();
        let work = demo_at(&s.0.join("a"), &s.0.join("sys/eitri/plugins/demo"));
        let (lua, registry, lines) = engine_with(&s.0.join("state"), search_in(&s.0));
        lua.load(format!("eitri.plugin.load('demo', {{ work = {work:?} }})"))
            .exec()
            .unwrap();
        let system = s.0.join("sys/eitri/plugins/demo");
        assert_eq!(registry.borrow().plugins()[0].plugin.dir, system);
        assert_eq!(
            *lines.borrow(),
            vec![
                format!(
                    "[plugin demo] skipped {}: no eitri-plugin.toml",
                    s.0.join("data/eitri/plugins/demo").display()
                ),
                format!("[plugin demo] loaded {} (system data directory)", system.display()),
            ]
        );
    }

    #[test]
    fn a_name_whose_manifest_has_another_id_is_refused_naming_both() {
        let s = Scratch::new("lua-name-id");
        let work = demo_at(&s.0.join("a"), &s.0.join("sys/eitri/plugins/other"));
        let (lua, registry, lines) = engine_with(&s.0.join("state"), search_in(&s.0));
        let err = refusal(&lua, &format!("eitri.plugin.load, 'other', {{ work = {work:?} }}"));
        let want = format!(
            "eitri.plugin.load(\"other\"): {} holds plugin demo, not other",
            s.0.join("sys/eitri/plugins/other").display()
        );
        assert!(err.contains(&want), "{err}");
        assert_eq!(registry.borrow().refused().as_deref(), Some(want.as_str()));
        assert!(registry.borrow().plugins().is_empty());
        assert!(lines.borrow().is_empty(), "{:?}", lines.borrow());
        assert!(
            !s.0.join("state/plugins/demo").exists(),
            "a refused plugin records no capability list"
        );
    }

    #[test]
    fn an_unknown_name_lists_every_directory_in_order_and_the_refusal_survives_pcall() {
        let s = Scratch::new("lua-name-unknown");
        let (lua, registry, _) = engine_with(&s.0.join("state"), search_in(&s.0));
        let err = refusal(&lua, "eitri.plugin.load, 'markdwn'");
        let want = format!(
            "eitri.plugin.load(\"markdwn\"): no plugin named markdwn; looked in {}, {}, {}",
            s.0.join("data/eitri/plugins/markdwn").display(),
            s.0.join("lib/plugins/markdwn").display(),
            s.0.join("sys/eitri/plugins/markdwn").display()
        );
        assert!(err.contains(&want), "{err}");
        assert_eq!(registry.borrow().refused().as_deref(), Some(want.as_str()));
    }

    #[test]
    fn a_broken_manifest_found_by_name_is_the_plugin_and_does_not_fall_through() {
        let s = Scratch::new("lua-name-broken");
        let user = s.0.join("data/eitri/plugins/demo");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(user.join("eitri-plugin.toml"), "id = \"demo\"\n").unwrap();
        let work = demo_at(&s.0.join("a"), &s.0.join("sys/eitri/plugins/demo"));
        let (lua, registry, lines) = engine_with(&s.0.join("state"), search_in(&s.0));
        let err = refusal(&lua, &format!("eitri.plugin.load, 'demo', {{ work = {work:?} }}"));
        assert!(err.contains(&user.display().to_string()), "{err}");
        assert!(err.contains("eitri-plugin.toml"), "{err}");
        assert!(registry.borrow().plugins().is_empty());
        assert!(lines.borrow().is_empty(), "{:?}", lines.borrow());
    }

    #[test]
    fn two_plugins_with_one_window_root_fail_at_startup_naming_both_calls() {
        let s = Scratch::new("lua-window-root-twice");
        let first = s.0.join("a/demo");
        let work = demo_at(&s.0.join("a"), &first);
        let second = s.0.join("b/other");
        demo_at(&s.0.join("b"), &second);
        for (dir, id) in [(&first, "demo"), (&second, "other")] {
            let path = dir.join("eitri-plugin.toml");
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.starts_with("id = \"demo\""), "the demo manifest opens with its id");
            let text = text.replacen("id = \"demo\"", &format!("id = \"{id}\""), 1);
            std::fs::write(
                path,
                format!("{text}\n[window]\nroot = \"${{work}}\"\ntitle = 'Demo'\nlayout = {{ module = 'page' }}\n"),
            )
            .unwrap();
        }
        let (lua, registry, _) = engine_with(&s.0.join("state"), search_in(&s.0));
        let load = |dir: &std::path::Path| {
            format!(
                "eitri.plugin.load({:?}, {{ work = {work:?} }})",
                dir.display().to_string()
            )
        };
        let err = lua
            .load(format!("{}\n{}", load(&first), load(&second)))
            .set_name("/home/u/init.lua")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("plugin demo (loaded at /home/u/init.lua:1)")
                && err.contains("plugin other (loaded at /home/u/init.lua:2)")
                && err.contains("is declared twice"),
            "{err}"
        );
        let loaded: Vec<String> = registry
            .borrow()
            .plugins
            .iter()
            .map(|p| p.plugin.manifest.id.clone())
            .collect();
        assert_eq!(loaded, ["demo"], "the second plugin is not loaded");
    }

    #[test]
    fn the_same_id_by_name_and_by_path_is_refused() {
        let s = Scratch::new("lua-name-twice");
        let work = demo_at(&s.0.join("a"), &s.0.join("data/eitri/plugins/demo"));
        let other = s.0.join("elsewhere/demo");
        demo_at(&s.0.join("b"), &other);
        let (lua, registry, _) = engine_with(&s.0.join("state"), search_in(&s.0));
        lua.load(format!("eitri.plugin.load('demo', {{ work = {work:?} }})"))
            .exec()
            .unwrap();
        let err = refusal(
            &lua,
            &format!(
                "eitri.plugin.load, {:?}, {{ work = {work:?} }}",
                other.display().to_string()
            ),
        );
        assert!(err.contains("plugin demo is loaded twice"), "{err}");
        assert_eq!(registry.borrow().plugins().len(), 1);
    }

    #[test]
    fn a_path_keeps_todays_rule_and_logs_path() {
        let s = Scratch::new("lua-path");
        let (dir, settings) = demo_plugin(&s.0);
        // A copy the search would find, to show a path never searches.
        demo_at(&s.0.join("a"), &s.0.join("data/eitri/plugins/demo"));
        let (lua, registry, lines) = engine_with(&s.0.join("state"), search_in(&s.0));
        for relative in ["./demo", "demo/x", "Demo"] {
            let err = refusal(
                &lua,
                &format!("eitri.plugin.load, {relative:?}, {{ work = {:?} }}", settings["work"]),
            );
            assert!(err.contains("is not an absolute path"), "{relative}: {err}");
        }
        assert!(registry.borrow().plugins().is_empty());
        assert!(lines.borrow().is_empty(), "{:?}", lines.borrow());
        // A fresh window: the absolute path loads that directory, not the copy the search would find.
        let (lua, registry, lines) = engine_with(&s.0.join("state2"), search_in(&s.0));
        lua.load(format!(
            "eitri.plugin.load({:?}, {{ work = {:?} }})",
            dir.display().to_string(),
            settings["work"]
        ))
        .exec()
        .unwrap();
        assert_eq!(registry.borrow().plugins()[0].plugin.dir, dir);
        assert_eq!(
            *lines.borrow(),
            vec![format!("[plugin demo] loaded {} (path)", dir.display())]
        );
    }
}
