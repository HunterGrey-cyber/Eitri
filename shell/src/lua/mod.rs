//! The shell's embedded Lua extension kernel -- a separate, independent runtime from Neovim's
//! own internal Lua (the two never share state). Exposes exactly four v1
//! extension points under an `eitri` global table: `panel.register`, `command.register`, `on`,
//! `config.get`/`config.set`, plus `keymap.prefix`/`keymap.set`/`keymap.del`.
//!
//! The runtime itself is `eitri_core::lua::kernel::Kernel`; what this module adds is the one thing
//! that needs a display: the registry of Lua panels, each holding a WebKitGTK view.

mod panel;

pub(crate) use eitri_core::lua::panel::PanelSlot;
pub(crate) use panel::{install_crash_recovery_into, PanelEntry, PanelRegistry};

use eitri_core::keymap::KeySpec;
use eitri_core::layout::ModuleId;
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
    /// In a page window beside nvim: the one plugin it builds, and what `init.lua` asked of the others.
    page: Option<Rc<PageLoads>>,
    recorded_keys: Rc<RefCell<Vec<(String, KeySpec)>>>,
}

/// What a page window's `init.lua` loaded: its own plugin through the real call, every other load only counted, with
/// the module key a load by name gave, which starts or raises that page's own window.
struct PageLoads {
    id: String,
    /// Shared with the narrowed load, which holds nothing else of the engine's.
    others: Rc<std::cell::Cell<usize>>,
    other_keys: Rc<RefCell<Vec<(String, String)>>>,
    /// `eitri.plugin`, and the real load of the page's own id, for the load after `init.lua` when it made none.
    own_load: RefCell<Option<(mlua::Table, mlua::Function)>>,
}

/// Where the page's own load is reachable while [`LuaEngine::settle_page`] runs it, and the chunk that calls it.
const OWN_LOAD: &str = "__page_window";
const OWN_LOAD_CHUNK: &str = "eitri.plugin.__page_window()";

/// A registration a page window takes and drops, saying once that it has no Lua panels or commands.
fn ignored_registration(lua: &mlua::Lua, told: Rc<std::cell::Cell<bool>>) -> mlua::Result<mlua::Function> {
    lua.create_function(move |_, _spec: mlua::MultiValue| {
        if !told.replace(true) {
            eprintln!("[page] init.lua registers Lua panels or commands; a page window has none");
        }
        Ok(())
    })
}

/// The line a page window logs once when `init.lua` loads plugins it does not build; `None` when there are none.
pub(crate) fn others_line(others: usize, id: &str) -> Option<String> {
    (others > 0).then(|| format!("[page] init.lua loads {others} other plugin(s); a page window builds only {id}"))
}

impl Deref for LuaEngine {
    type Target = Kernel;

    fn deref(&self) -> &Kernel {
        &self.kernel
    }
}

impl LuaEngine {
    #[cfg(test)]
    pub(crate) fn new(config_dir: PathBuf) -> mlua::Result<Self> {
        Self::showing(config_dir, None)
    }

    /// The engine of a window; `shows` is the one module of a chat or page window, which builds no other panel's view.
    pub(crate) fn showing(config_dir: PathBuf, shows: Option<ModuleId>) -> mlua::Result<Self> {
        let panels = Rc::new(RefCell::new(PanelRegistry::default()));
        let for_panels = panels.clone();
        let kernel = Kernel::new(config_dir.clone(), move |lua, eitri| {
            panel::install(lua, eitri, for_panels, config_dir, shows)
        })?;
        Ok(Self {
            kernel,
            panels,
            page: None,
            recorded_keys: Rc::default(),
        })
    }

    /// The engine of a page window beside nvim (`eitri page <id>`): `init.lua` runs, its `eitri.plugin.load` of `id`
    /// is honoured with its options, every other load is counted and builds nothing (a load by name keeps its key),
    /// and a Lua panel or command is not registered (said once). The plugin must be on the search path: typing the
    /// command grants loading it by its id, never a directory. `Err` is the line `eitri page` exits with.
    pub(crate) fn for_page(config_dir: PathBuf, id: &str) -> Result<Self, String> {
        let search = eitri_core::plugin::search::SearchEnv::from_process();
        if eitri_core::plugin::search::find_plugin(id, &search).is_err() {
            return Err(format!("eitri page: no plugin \"{id}\" on the plugin search path"));
        }
        let lua_handle: Rc<RefCell<Option<mlua::Lua>>> = Rc::default();
        let told = Rc::new(std::cell::Cell::new(false));
        let kernel = {
            let (lua_handle, told) = (lua_handle.clone(), told.clone());
            Kernel::new(config_dir, move |lua, eitri| {
                *lua_handle.borrow_mut() = Some(lua.clone());
                let panel = lua.create_table()?;
                panel.set("register", ignored_registration(lua, told)?)?;
                eitri.set("panel", panel)
            })
        }
        .map_err(|e| format!("eitri page: the Lua kernel could not start: {e}"))?;
        let page = Rc::new(PageLoads {
            id: id.to_owned(),
            others: Rc::default(),
            other_keys: Rc::default(),
            own_load: RefCell::new(None),
        });
        let lua = lua_handle
            .borrow_mut()
            .take()
            .ok_or_else(|| "eitri page: the Lua kernel did not start".to_string())?;
        Self::narrow_plugin_load(&lua, page.clone()).map_err(|e| format!("eitri page: {e}"))?;
        // The kernel installs `eitri.command` after the panel table; its `register` is replaced here.
        let ignore_commands = |told| -> mlua::Result<()> {
            let command: mlua::Table = lua.globals().get::<mlua::Table>("eitri")?.get("command")?;
            command.set("register", ignored_registration(&lua, told)?)
        };
        ignore_commands(told).map_err(|e| format!("eitri page: {e}"))?;
        Ok(Self {
            kernel,
            panels: Rc::new(RefCell::new(PanelRegistry::default())),
            page: Some(page),
            recorded_keys: Rc::default(),
        })
    }

    /// Puts a call in front of the kernel's `eitri.plugin.load` that passes on only a load of the page's own id, by
    /// name, and counts every other one.
    fn narrow_plugin_load(lua: &mlua::Lua, page: Rc<PageLoads>) -> mlua::Result<()> {
        let eitri: mlua::Table = lua.globals().get("eitri")?;
        let plugin: mlua::Table = eitri.get("plugin")?;
        let real: mlua::Function = plugin.get("load")?;
        let own = {
            let (real, id) = (real.clone(), page.id.clone());
            lua.create_function(move |_, ()| real.call::<()>((id.clone(),)))?
        };
        *page.own_load.borrow_mut() = Some((plugin.clone(), own));
        let (id, others, other_keys) = (page.id.clone(), page.others.clone(), page.other_keys.clone());
        let narrowed = lua.create_function(move |_, (arg, opts): (mlua::Value, mlua::Value)| {
            let name = match &arg {
                mlua::Value::String(name) => name.to_str().ok().map(|name| (*name).to_owned()),
                _ => None,
            };
            if name.as_deref() == Some(id.as_str()) {
                return real.call::<()>((arg, opts));
            }
            others.set(others.get() + 1);
            // Only a load by name: the window a key starts is `eitri page <id>`, which finds its plugin by id.
            let named = name.filter(|name| {
                matches!(
                    eitri_core::plugin::search::classify(name),
                    eitri_core::plugin::search::LoadArg::Name(_)
                )
            });
            let key = match &opts {
                mlua::Value::Table(opts) => opts.get::<Option<String>>("key").ok().flatten(),
                _ => None,
            };
            if let (Some(name), Some(key)) = (named, key) {
                let mut keys = other_keys.borrow_mut();
                if !keys.iter().any(|(known, _)| *known == name) {
                    keys.push((name, key));
                }
            }
            Ok(())
        })?;
        plugin.set("load", narrowed)
    }

    /// Whether the page's own plugin is loaded.
    pub(crate) fn loaded_page(&self) -> bool {
        self.page.as_ref().is_some_and(|page| {
            self.kernel
                .plugins
                .borrow()
                .plugins()
                .iter()
                .any(|registration| registration.plugin.manifest.id == page.id)
        })
    }

    /// How many loads `init.lua` made of plugins other than the page's own.
    #[cfg(test)]
    pub(crate) fn other_loads(&self) -> usize {
        self.page.as_ref().map_or(0, |page| page.others.get())
    }

    /// The module key `init.lua` gave each other page it loaded by name, in load order; empty in any other window.
    pub(crate) fn other_page_keys(&self) -> Vec<(ModuleId, Option<String>)> {
        self.page.as_ref().map_or_else(Vec::new, |page| {
            page.other_keys
                .borrow()
                .iter()
                .map(|(id, key)| (ModuleId::lua(id), Some(key.clone())))
                .collect()
        })
    }

    /// After `init.lua`: says once what it loaded that this window does not build, and loads the page's own plugin by
    /// its id, with its defaults, when `init.lua` did not. Nothing in any other window. `Err` is the startup failure.
    pub(crate) fn settle_page(&self) -> Result<(), String> {
        let Some(page) = &self.page else {
            return Ok(());
        };
        if let Some(line) = others_line(page.others.get(), &page.id) {
            println!("{line}");
        }
        if self.loaded_page() {
            return Ok(());
        }
        // Under init.lua's rules, as a load there is; the call is reachable for this one chunk only.
        let Some((plugin, own)) = page.own_load.borrow_mut().take() else {
            return Ok(());
        };
        plugin.set(OWN_LOAD, own).map_err(|e| e.to_string())?;
        let loaded = self.kernel.run_checked_text("eitri page", OWN_LOAD_CHUNK);
        plugin.set(OWN_LOAD, mlua::Value::Nil).map_err(|e| e.to_string())?;
        loaded
    }

    /// The plugin ids and module keys recorded without loading plugins, in init.lua order.
    pub(crate) fn recorded_plugin_keys(&self) -> Vec<(String, KeySpec)> {
        self.recorded_keys.borrow().clone()
    }

    /// The companion records page keys while loading no plugin code and writing no plugin state.
    pub(crate) fn without_plugins(config_dir: PathBuf) -> mlua::Result<Self> {
        let panels = Rc::new(RefCell::new(PanelRegistry::default()));
        let for_panels = panels.clone();
        let recorded_keys = Rc::new(RefCell::new(Vec::new()));
        let keys = recorded_keys.clone();
        let kernel = Kernel::new(config_dir.clone(), move |lua, eitri| {
            panel::install(lua, eitri, for_panels, config_dir, None)?;
            let load = lua.create_function(move |_, (arg, opts): (mlua::Value, mlua::Value)| {
                let mlua::Value::String(id) = arg else { return Ok(()) };
                let id = id.to_str()?.to_string();
                if !eitri_core::plugin::manifest::id_ok(&id) {
                    return Ok(());
                }
                let mlua::Value::Table(opts) = opts else { return Ok(()) };
                let Some(key) = opts.get::<Option<String>>("key").ok().flatten() else {
                    return Ok(());
                };
                if let Ok(key) = KeySpec::parse(&key) {
                    // The first load of an id decides its key, as in the window that builds a page.
                    let mut keys = keys.borrow_mut();
                    if !keys.iter().any(|(known, _)| *known == id) {
                        keys.push((id, key));
                    }
                }
                Ok(())
            })?;
            let plugin = lua.create_table()?;
            plugin.set("load", load)?;
            eitri.set("plugin", plugin)
        })?;
        Ok(Self {
            kernel,
            panels,
            page: None,
            recorded_keys,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plugin the search finds by its id beside the running executable (the install step, which no environment
    /// variable moves), under an id no other copy has; removed with its directory when dropped.
    struct InstalledPlugin {
        id: String,
        dir: PathBuf,
    }

    impl InstalledPlugin {
        fn new(label: &str) -> InstalledPlugin {
            let id = format!("pw-{label}-{}", std::process::id());
            let exe = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
            let dir = exe.parent().unwrap().join("plugins").join(&id);
            std::fs::create_dir_all(dir.join("site")).unwrap();
            std::fs::write(
                dir.join("eitri-plugin.toml"),
                format!(
                    "id = \"{id}\"\ntitle = \"Page test\"\nprotocol = 1\nsite = \"site\"\nentry = \"index.html\"\n\
                     capabilities = [\"state\", \"net\"]\n"
                ),
            )
            .unwrap();
            std::fs::write(dir.join("site/index.html"), "<!doctype html><p>page</p>\n").unwrap();
            InstalledPlugin { id, dir }
        }
    }

    impl Drop for InstalledPlugin {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn config_with(scratch: &eitri_core::plugin::test_support::Scratch, init: &str) -> PathBuf {
        let config_dir = scratch.0.join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("init.lua"), init).unwrap();
        config_dir
    }

    /// The page's own load keeps the options `init.lua` gave it; every other load, by name or by path, is counted and
    /// built nowhere; a Lua panel is not registered; and with no load of its own the page is loaded by its id after
    /// `init.lua`, with its defaults.
    #[test]
    fn for_page_honours_its_id_records_others_and_loads_it_when_init_lua_did_not() {
        let plugin = InstalledPlugin::new("own");
        let scratch = eitri_core::plugin::test_support::Scratch::new("page-engine");
        let config_dir = config_with(
            &scratch,
            &format!(
                "eitri.plugin.load(\"other-page\")\n\
                 eitri.plugin.load({:?}, {{ key = \"n\" }})\n\
                 eitri.plugin.load(\"/no/such/dir\")\n\
                 eitri.panel.register({{ id = \"side\", title = \"Side\", url = \"https://example.org\" }})\n",
                plugin.id
            ),
        );
        let engine = LuaEngine::for_page(config_dir.clone(), &plugin.id).unwrap();
        assert_eq!(engine.run_and_check_init_file(&config_dir.join("init.lua")), Ok(()));
        assert!(engine.loaded_page(), "init.lua loaded the page itself");
        assert_eq!(engine.other_loads(), 2);
        assert_eq!(engine.settle_page(), Ok(()));
        let plugins = engine.plugins.borrow();
        assert_eq!(plugins.plugins().len(), 1, "only the page's own plugin is built");
        assert_eq!(plugins.plugins()[0].plugin.manifest.id, plugin.id);
        assert_eq!(
            plugins.plugins()[0].key.as_deref(),
            Some("n"),
            "with the options init.lua gave it"
        );
        assert!(
            engine.panels.borrow().entries().is_empty(),
            "a Lua panel is not this window's"
        );
        drop(plugins);
        assert_eq!(
            others_line(2, &plugin.id).as_deref(),
            Some(
                format!(
                    "[page] init.lua loads 2 other plugin(s); a page window builds only {}",
                    plugin.id
                )
                .as_str()
            )
        );
        assert_eq!(others_line(0, &plugin.id), None);

        let quiet = config_with(&scratch, "-- nothing here loads a plugin\n");
        let engine = LuaEngine::for_page(quiet.clone(), &plugin.id).unwrap();
        assert_eq!(engine.run_and_check_init_file(&quiet.join("init.lua")), Ok(()));
        assert!(!engine.loaded_page());
        assert_eq!(engine.settle_page(), Ok(()));
        assert!(engine.loaded_page(), "loaded by its id once init.lua has run");
        let plugins = engine.plugins.borrow();
        assert_eq!(plugins.plugins().len(), 1);
        assert_eq!(plugins.plugins()[0].key, None, "its defaults");
    }

    /// Another page's load builds nothing here but keeps its key, so that key starts or raises that page's own window;
    /// a load by path keeps none (a page window is started by id), and no Lua command is registered.
    #[test]
    fn for_page_keeps_the_other_pages_keys_and_registers_no_command() {
        let plugin = InstalledPlugin::new("keys");
        let scratch = eitri_core::plugin::test_support::Scratch::new("page-engine-keys");
        let config_dir = config_with(
            &scratch,
            &format!(
                "eitri.plugin.load(\"other-page\", {{ key = \"m\" }})\n\
                 eitri.plugin.load(\"keyless-page\")\n\
                 eitri.plugin.load(\"/no/such/dir\", {{ key = \"p\" }})\n\
                 eitri.plugin.load({:?})\n\
                 eitri.command.register({{ id = \"hello\", title = \"Hello\", keybinding = \"<Control>h\", \
                 action = function() end }})\n",
                plugin.id
            ),
        );
        let engine = LuaEngine::for_page(config_dir.clone(), &plugin.id).unwrap();
        engine.load_init_file(&config_dir.join("init.lua"));
        assert_eq!(engine.startup_problems(), Vec::<String>::new());
        assert_eq!(engine.other_loads(), 3);
        assert_eq!(
            engine.other_page_keys(),
            vec![(ModuleId::lua("other-page"), Some("m".to_owned()))]
        );
        assert!(
            engine.commands.borrow().is_empty(),
            "a Lua command is not this window's"
        );
        assert!(engine.loaded_page());
    }

    /// A refusal `init.lua` caught was said as a startup problem; the page's own load after `init.lua` still opens it.
    #[test]
    fn settle_page_loads_the_page_after_a_refusal_init_lua_caught() {
        let plugin = InstalledPlugin::new("caught");
        let scratch = eitri_core::plugin::test_support::Scratch::new("page-engine-caught");
        let config_dir = config_with(&scratch, "pcall(eitri.config.set, {}, true)\n");
        let engine = LuaEngine::for_page(config_dir.clone(), &plugin.id).unwrap();
        engine.load_init_file(&config_dir.join("init.lua"));
        assert_eq!(engine.startup_problems().len(), 1, "{:?}", engine.startup_problems());
        assert_eq!(engine.settle_page(), Ok(()));
        assert!(engine.loaded_page());
    }

    #[test]
    fn for_page_refuses_a_missing_plugin_with_the_exact_text() {
        let scratch = eitri_core::plugin::test_support::Scratch::new("page-engine-missing");
        let config_dir = config_with(&scratch, "");
        let id = format!("pw-missing-{}", std::process::id());
        match LuaEngine::for_page(config_dir, &id) {
            Err(text) => assert_eq!(
                text,
                format!("eitri page: no plugin \"{id}\" on the plugin search path")
            ),
            Ok(_) => panic!("a plugin nowhere on the search path was found"),
        }
    }

    /// The sandbox and stores are the kernel's own tests; this checks only the join: the engine
    /// builds with no display, starts with no panels, and reaches the kernel's stores.
    #[test]
    fn the_engine_starts_with_no_panels_and_reaches_the_kernels_stores() {
        let engine = LuaEngine::new(std::env::temp_dir().join("eitri-no-such-config")).unwrap();
        assert!(engine.panels.borrow().entries().is_empty());
        assert!(engine.commands.borrow().is_empty());
        assert!(engine.config.borrow().get("agent.font_size").is_none());
    }

    #[test]
    fn without_plugins_records_ids_and_keys_and_loads_nothing() {
        let scratch = eitri_core::plugin::test_support::Scratch::new("companion-plugin-keys");
        let config = scratch.0.join("config");
        std::fs::create_dir_all(&config).unwrap();
        let init = config.join("init.lua");
        std::fs::write(
            &init,
            r#"
            eitri.plugin.load("notes", { key = "m" })
            eitri.plugin.load("other", { key = "p" })
            eitri.plugin.load("unkeyed")
            eitri.plugin.load("/missing/path", { key = "z" })
            eitri.plugin.load("notes", { key = "q" })
        "#,
        )
        .unwrap();
        let engine = LuaEngine::without_plugins(config).unwrap();
        assert_eq!(engine.run_and_check_init_file(&init), Ok(()));
        assert_eq!(
            engine.recorded_plugin_keys(),
            vec![
                ("notes".into(), eitri_core::keymap::KeySpec::char('m', false, false)),
                ("other".into(), eitri_core::keymap::KeySpec::char('p', false, false)),
            ]
        );
        assert!(engine.plugins.borrow().plugins().is_empty());
        assert!(engine.plugins.borrow().refused().is_none());
        assert!(engine.panels.borrow().entries().is_empty());
        assert!(!scratch.0.join("state").exists());
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
