//! The Lua extension kernel without a toolkit: the `mlua` state, the `require` sandbox that resolves
//! only under `<config dir>/lua/`, and the command, event, config, layout and keymap stores behind
//! `eitri.*`, a Lua runtime that remains separate from Neovim's own.
//! A host decides what `eitri.panel.register` does by passing an installer to [`Kernel::new`]: `shell`
//! builds a WebKitGTK view, a host with no Lua panels passes [`refuse_panels`].

use mlua::{Lua, Table, Value};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

/// Refusals belong to the Lua state so every function boundary shares the startup decision.
#[derive(Debug)]
pub(crate) struct InitRefusals {
    accepting: bool,
    path: Option<PathBuf>,
    entries: Vec<(&'static str, Option<String>, Option<String>)>,
}

impl Default for InitRefusals {
    fn default() -> Self {
        Self {
            accepting: true,
            path: None,
            entries: Vec::new(),
        }
    }
}

impl InitRefusals {
    pub(crate) fn message(&self, name: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|(call, _, error)| *call == name && error.is_some())
            .and_then(|(_, _, error)| error.clone())
    }

    fn record(&mut self, name: &'static str, workspace: Option<String>, error: Option<String>) {
        if !self.accepting {
            return;
        }
        let replace = name == "eitri.layout.default" || (name == "eitri.layout.workspace" && workspace.is_some());
        if let Some((_, _, earlier)) = self
            .entries
            .iter_mut()
            .find(|(call, key, _)| *call == name && *key == workspace)
        {
            if replace || earlier.is_none() {
                *earlier = error;
            }
        } else {
            self.entries.push((name, workspace, error));
        }
    }

    fn first(&self) -> Option<String> {
        self.entries
            .iter()
            .filter_map(|(name, _, error)| error.as_ref().map(|error| (name, error)))
            .min_by_key(|(name, _)| match **name {
                name if name.starts_with("eitri.command.") => 0,
                name if name.starts_with("eitri.config.") => 1,
                name if name.starts_with("eitri.agent.") => 2,
                name if name.starts_with("eitri.plugin.") => 3,
                "eitri.layout.default" => 4,
                "eitri.layout.rule" => 5,
                "eitri.layout.workspace" => 6,
                _ => 7,
            })
            .map(|(_, error)| match &self.path {
                Some(path) => format!("{error} ({})", path.display()),
                None => error.clone(),
            })
    }
}

pub(crate) fn init_refusals(lua: &Lua) -> Rc<RefCell<InitRefusals>> {
    if let Some(shared) = lua.app_data_ref::<Rc<RefCell<InitRefusals>>>() {
        return shared.clone();
    }
    let shared = Rc::new(RefCell::new(InitRefusals::default()));
    lua.set_app_data(shared.clone());
    shared
}

/// Convert arguments inside the boundary too, so a caught type error cannot bypass startup validation.
pub(crate) fn registering_function<A, R>(
    lua: &Lua,
    name: &'static str,
    callback: impl Fn(&Lua, A) -> mlua::Result<R> + 'static,
) -> mlua::Result<mlua::Function>
where
    A: mlua::FromLuaMulti,
    R: mlua::IntoLuaMulti,
{
    let shared = init_refusals(lua);
    lua.create_function(move |lua, args: mlua::MultiValue| {
        let workspace = if name == "eitri.layout.workspace" {
            match args.front() {
                Some(Value::String(name)) => name.to_str().ok().map(|name| name.to_string()),
                _ => None,
            }
        } else {
            None
        };
        let result = A::from_lua_multi(args, lua)
            .and_then(|args| callback(lua, args))
            .and_then(|result| result.into_lua_multi(lua));
        let error = result.as_ref().err().map(|error| match error {
            mlua::Error::RuntimeError(message) => message.clone(),
            _ => format!("{name}: {error}"),
        });
        shared.borrow_mut().record(name, workspace, error);
        result
    })
}

pub struct Kernel {
    lua: Lua,
    init_refusals: Rc<RefCell<InitRefusals>>,
    pub commands: Rc<RefCell<super::command::CommandRegistry>>,
    events: Rc<RefCell<super::event::EventBus>>,
    /// Read by the host once `init.lua` has run (`agent.account`, `agent.font_size`, ...). The
    /// `get`/`set` closures on `eitri.config` keep the store alive regardless; the field is here so
    /// a host can read it.
    pub config: Rc<RefCell<super::config::ConfigStore>>,
    /// `eitri.layout.*`: `init.lua`'s default tree, and the requests commands queue.
    pub layout: Rc<RefCell<super::layout::LayoutStore>>,
    /// `eitri.keymap.*`: the calls `init.lua` made, applied by the host once it has run
    /// (`crate::keymap::Keymap::apply_user`).
    pub keymap: Rc<RefCell<super::keymap::KeymapStore>>,
    /// `eitri.agent.register`: the ACP agents `init.lua` registered, read by the host once it has run.
    pub agents: Rc<RefCell<super::agent::AgentRegistry>>,
    /// `eitri.plugin.load`: the plugins `init.lua` loaded, read by the host once it has run.
    pub plugins: Rc<RefCell<super::plugin::PluginRegistry>>,
}

/// The panel installer for a host that has no Lua panels (the companion window, `eitri-mac`):
/// `eitri.panel.register` logs once and does nothing, so an `init.lua` written for the full window
/// still loads and every later line of it still runs.
pub fn refuse_panels(lua: &Lua, eitri: &Table) -> mlua::Result<()> {
    let told = Rc::new(Cell::new(false));
    let register = registering_function(lua, "eitri.panel.register", move |_, _spec: Value| {
        if !told.replace(true) {
            eprintln!("eitri: Lua panels are not in this window");
        }
        Ok(())
    })?;
    let panel = lua.create_table()?;
    panel.set("register", register)?;
    eitri.set("panel", panel)?;
    super::plugin::refuse_plugins(lua, eitri)?;
    Ok(())
}

impl Kernel {
    /// `install_panels` runs once, with the Lua state and the `eitri` table, and sets
    /// `eitri.panel`; it runs before the other stores are installed.
    pub fn new(
        config_dir: PathBuf,
        install_panels: impl FnOnce(&Lua, &Table) -> mlua::Result<()>,
    ) -> mlua::Result<Kernel> {
        let lua = Lua::new();
        let init_refusals = init_refusals(&lua);
        let eitri = lua.create_table()?;

        // `Lua::new()` includes `./?.lua;./?/init.lua` in its default `package.path`. Since `shell`
        // keeps the opened project's working directory, an unguarded `require` could run a module
        // planted in that project instead of a configuration module. Limit `require` to the config
        // directory's `lua/` subtree and disable native C module loading.
        //
        // Do not interpolate `config_dir` into Lua's `;`- and `?`-based path syntax. A literal `;` in
        // the directory name could inject a relative search entry and reopen the cwd search.
        // Empty `package.path` and replace `package.searchers[2]` with a Rust closure that uses
        // `Path::join`, so directory characters are never parsed as search patterns. Remove the C
        // searcher and all-in-one loader (`package.searchers[3]` and `[4]`) as well as emptying `cpath`.
        //
        // `require` therefore ignores both `package.path` and `package.cpath`. Extending either in
        // `init.lua` has no effect, and `package.searchpath` describes a path `require` does not use.
        // `<config_dir>/lua/` is the only place `require` looks.
        let package: Table = lua.globals().get("package")?;
        package.set("path", "")?;
        package.set("cpath", "")?;
        let config_lua_dir = config_dir.join("lua");
        let require_from_config_dir = lua.create_function(move |lua, name: String| {
            // Mirrors Lua's own dotted-module convention (`require("a.b")` -> `a/b.lua` or
            // `a/b/init.lua`), resolved with real filesystem joins so nothing in `name` or
            // `config_lua_dir` is ever treated as `;`/`?` pattern syntax.
            let relative = name.replace('.', "/");
            for candidate in [format!("{relative}.lua"), format!("{relative}/init.lua")] {
                let path = config_lua_dir.join(&candidate);
                // `PathBuf::join` silently *replaces* the base with its argument whenever that
                // argument is itself absolute (a documented `Path::join`/`push` behaviour, not a
                // bug in this crate) -- so a `require` name whose '.'-to-'/' conversion above
                // happens to produce a leading '/' (e.g. `require(".etc.passwd")` ->
                // `/etc/passwd`) would otherwise let `path` escape `config_lua_dir` entirely and
                // read an arbitrary absolute file. Refuse anything `join` did not keep contained.
                if !path.starts_with(&config_lua_dir) {
                    continue;
                }
                if let Ok(src) = std::fs::read_to_string(&path) {
                    let chunk_name = format!("@{}", path.display());
                    let f = lua.load(&src).set_name(chunk_name).into_function()?;
                    return Ok(Value::Function(f));
                }
            }
            // `require` concatenates every searcher's "not found" message into its own error
            // when none of them find the module -- same convention Lua's own path searcher uses.
            let message = format!(
                "\n\tno file '{}'\n\tno file '{}'",
                config_lua_dir.join(format!("{relative}.lua")).display(),
                config_lua_dir.join(format!("{relative}/init.lua")).display(),
            );
            Ok(Value::String(lua.create_string(message)?))
        })?;
        let searchers: Table = package.get("searchers")?;
        searchers.set(2, require_from_config_dir)?;
        searchers.set(3, Value::Nil)?;
        searchers.set(4, Value::Nil)?;

        let commands = Rc::new(RefCell::new(super::command::CommandRegistry::default()));
        let events = Rc::new(RefCell::new(super::event::EventBus::default()));
        let config = Rc::new(RefCell::new(super::config::ConfigStore::default()));

        install_panels(&lua, &eitri)?;
        // A host that has no plugin pages sets `eitri.plugin` itself (`refuse_panels`); every other host gets the
        // real call, recording into this registry.
        let plugins = Rc::new(RefCell::new(super::plugin::PluginRegistry::default()));
        if matches!(eitri.get::<Value>("plugin")?, Value::Nil) {
            super::plugin::install(
                &lua,
                &eitri,
                plugins.clone(),
                crate::plugin::state::PluginStore::eitri_root_from_env(),
                std::env::var_os("HOME").map(PathBuf::from),
                crate::plugin::search::SearchEnv::from_process(),
            )?;
        }
        super::command::install(&lua, &eitri, commands.clone())?;
        super::event::install(&lua, &eitri, events.clone())?;
        super::config::install(&lua, &eitri, config.clone())?;
        let layout = Rc::new(RefCell::new(super::layout::LayoutStore::default()));
        super::layout::install(&lua, &eitri, layout.clone())?;
        let keymap = Rc::new(RefCell::new(super::keymap::KeymapStore::default()));
        super::keymap::install(&lua, &eitri, keymap.clone())?;
        let agents = Rc::new(RefCell::new(super::agent::AgentRegistry::default()));
        super::agent::install(&lua, &eitri, agents.clone())?;

        lua.globals().set("eitri", eitri)?;

        Ok(Kernel {
            lua,
            init_refusals,
            commands,
            events,
            config,
            layout,
            keymap,
            agents,
            plugins,
        })
    }

    /// Names this window for Lua before `init.lua` runs: `eitri.window` (`"1"`, `"2"`, or nil for a
    /// window whose pair another window holds) and `eitri.layout.get().window`. Both windows run the
    /// same `init.lua`, so this is how it keeps a side effect to one of them.
    pub fn set_window(&self, role: crate::window_link::pair::Role) -> mlua::Result<()> {
        let name = super::layout::window_name(role);
        self.layout.borrow_mut().set_window(name);
        let eitri: Table = self.lua.globals().get("eitri")?;
        eitri.set("window", name)
    }

    pub fn emit(&self, event_name: &str) {
        super::event::emit(&self.lua, &self.events, event_name, Value::Nil);
    }

    /// Same reentrancy hazard and same fix as `super::event::emit` (see that function's doc comment):
    /// the command's `action` key is cloned out of `self.commands` under a scoped borrow, which
    /// is dropped *before* the action is actually called -- so an action that itself calls
    /// `eitri.command.register(...)` (registering another command, from inside a command's own
    /// action) doesn't hit `CommandRegistry`'s `borrow_mut()` while this function's own borrow is
    /// still held.
    pub fn invoke_command(&self, id: &str) {
        let action = {
            let commands = self.commands.borrow();
            let Some(entry) = commands.get(id) else { return };
            entry.action.clone()
        };
        let Ok(f) = self.lua.registry_value::<mlua::Function>(&action) else {
            return;
        };
        if let Err(err) = f.call::<()>(()) {
            eprintln!("[lua] command '{id}' handler error: {err}");
        }
    }

    /// Loads init.lua and exits on a validated API refusal, even when Lua catches the error.
    /// Other Lua errors are logged and leave the registrations made before the error available.
    pub fn load_init_file(&self, path: &std::path::Path) {
        if let Err(message) = self.run_and_check_init_file(path) {
            eprintln!("eitri: {message}");
            std::process::exit(1);
        }
    }

    /// [`Self::load_init_file`] without the exit: `Err` is the startup-failure text after "eitri: ".
    pub fn run_and_check_init_file(&self, path: &std::path::Path) -> Result<(), String> {
        {
            let mut refusals = self.init_refusals.borrow_mut();
            refusals.accepting = true;
            refusals.path = Some(path.to_path_buf());
        }
        self.run_init_file(path);
        self.init_refusals.borrow_mut().accepting = false;
        self.init_refusal().map_or(Ok(()), Err)
    }

    /// The shared startup refusal, including the init.lua path once it has been loaded.
    pub fn init_refusal(&self) -> Option<String> {
        self.init_refusals.borrow().first()
    }

    fn run_init_file(&self, path: &std::path::Path) {
        if !path.exists() {
            println!(
                "[lua] no init.lua at {} -- continuing with built-ins only",
                path.display()
            );
            return;
        }
        match std::fs::read_to_string(path) {
            Ok(src) => {
                if let Err(err) = self.lua.load(&src).set_name(path.to_string_lossy().to_string()).exec() {
                    eprintln!("[lua] error loading {}: {err}", path.display());
                }
            }
            Err(err) => eprintln!("[lua] could not read {}: {err}", path.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_layout_call_refused_inside_pcall_still_stops_startup() {
        let scratch = crate::plugin::test_support::Scratch::new("lua-boundary");
        let init = scratch.0.join("init.lua");
        std::fs::write(&init, "pcall(eitri.layout.new_call)").unwrap();
        let kernel = Kernel::new(scratch.0.clone(), refuse_panels).unwrap();
        let eitri: Table = kernel.lua.globals().get("eitri").unwrap();
        let layout: Table = eitri.get("layout").unwrap();
        layout
            .set(
                "new_call",
                registering_function(&kernel.lua, "eitri.layout.new_call", |_, (): ()| {
                    Err::<(), _>(mlua::Error::RuntimeError(
                        "eitri.layout.new_call: unavailable during init".into(),
                    ))
                })
                .unwrap(),
            )
            .unwrap();
        let error = kernel.run_and_check_init_file(&init).unwrap_err();
        assert!(
            error.contains("eitri.layout.new_call: unavailable during init"),
            "{error}"
        );
        assert!(error.contains(&init.display().to_string()), "{error}");
    }

    #[test]
    fn a_later_default_replaces_a_refused_one() {
        let scratch = crate::plugin::test_support::Scratch::new("lua-default-replacement");
        let init = scratch.0.join("init.lua");
        std::fs::write(
            &init,
            r#"pcall(function() eitri.layout.default(42) end)
            eitri.layout.default({"row", {"editor"}, {"agent"}})"#,
        )
        .unwrap();
        let kernel = Kernel::new(scratch.0.clone(), refuse_panels).unwrap();
        assert_eq!(kernel.run_and_check_init_file(&init), Ok(()));
        assert!(kernel.layout.borrow().default_tree().unwrap().is_ok());
    }

    #[test]
    fn workspace_replacements_clear_only_the_same_names_refusal() {
        for (source, fails) in [
            (
                "pcall(eitri.layout.workspace, 'work', 42); eitri.layout.workspace('work', {'row', {'editor'}, {'agent'}})",
                false,
            ),
            (
                "pcall(eitri.layout.workspace, 'work', 42); eitri.layout.workspace('other', {'row', {'editor'}, {'agent'}})",
                true,
            ),
            (
                "pcall(eitri.layout.workspace, 42, {'row', {'editor'}, {'agent'}}); eitri.layout.workspace('work', {'row', {'editor'}, {'agent'}})",
                true,
            ),
        ] {
            let scratch = crate::plugin::test_support::Scratch::new("lua-workspace-replacement");
            let init = scratch.0.join("init.lua");
            std::fs::write(&init, source).unwrap();
            let kernel = Kernel::new(scratch.0.clone(), refuse_panels).unwrap();
            assert_eq!(kernel.run_and_check_init_file(&init).is_err(), fails, "{source}");
        }
    }

    #[test]
    fn a_rule_refusal_survives_a_later_valid_rule() {
        let scratch = crate::plugin::test_support::Scratch::new("lua-rule-refusal");
        let init = scratch.0.join("init.lua");
        std::fs::write(
            &init,
            "pcall(eitri.layout.rule, 42, {}); eitri.layout.rule('terminal', {at = 'side'})",
        )
        .unwrap();
        let kernel = Kernel::new(scratch.0.clone(), refuse_panels).unwrap();
        let error = kernel.run_and_check_init_file(&init).unwrap_err();
        assert!(
            error.contains("eitri.layout.rule: the target must be a string"),
            "{error}"
        );
    }

    #[test]
    fn startup_refusals_keep_api_precedence_and_capture_argument_conversion() {
        let calls = [
            "pcall(eitri.command.register, 42)",
            "pcall(eitri.config.set, {}, true)",
            "pcall(eitri.agent.register, {})",
            "pcall(eitri.plugin.load, '/nonexistent/plugin')",
            "pcall(eitri.layout.default, 42)",
        ];
        let names = [
            "eitri.command.register",
            "eitri.config.set",
            "eitri.agent.register",
            "eitri.plugin.load",
            "eitri.layout.default",
        ];
        for first in 0..calls.len() {
            let scratch = crate::plugin::test_support::Scratch::new("lua-refusal-precedence");
            let init = scratch.0.join("init.lua");
            std::fs::write(
                &init,
                calls[first..].iter().rev().copied().collect::<Vec<_>>().join("; "),
            )
            .unwrap();
            let kernel = Kernel::new(scratch.0.clone(), |_, _| Ok(())).unwrap();
            let error = kernel.run_and_check_init_file(&init).unwrap_err();
            assert!(error.starts_with(names[first]), "{error}");
            assert!(error.ends_with(&format!("({})", init.display())), "{error}");
        }
    }

    #[test]
    fn runtime_refusals_do_not_change_the_startup_decision() {
        let scratch = crate::plugin::test_support::Scratch::new("lua-runtime-refusal");
        let kernel = Kernel::new(scratch.0.clone(), refuse_panels).unwrap();
        assert_eq!(kernel.run_and_check_init_file(&scratch.0.join("missing.lua")), Ok(()));
        kernel.layout.borrow_mut().accept_requests();
        kernel
            .lua
            .load("assert(not pcall(eitri.layout.default, 42)); assert(not pcall(eitri.command.register, 42))")
            .exec()
            .unwrap();
        assert_eq!(kernel.init_refusal(), None);
        assert_eq!(kernel.commands.borrow().refused(), None);
        assert_eq!(kernel.layout.borrow().default_tree(), None);
    }

    #[test]
    fn eitri_window_is_readable_from_init_lua() {
        use crate::window_link::pair::Role;
        let scratch = crate::plugin::test_support::Scratch::new("lua-eitri-window");
        let init = scratch.0.join("init.lua");
        std::fs::write(
            &init,
            "eitri.config.set('seen', tostring(eitri.window))\n\
             if eitri.window == '2' then eitri.config.set('only.second', true) end\n",
        )
        .unwrap();
        for (role, seen, only_second) in [
            (Some(Role::Main), "1", None),
            (Some(Role::Second), "2", Some("true")),
            (Some(Role::Unpaired), "nil", None),
            (None, "nil", None),
        ] {
            let kernel = Kernel::new(scratch.0.clone(), refuse_panels).unwrap();
            if let Some(role) = role {
                kernel.set_window(role).unwrap();
            }
            assert_eq!(kernel.run_and_check_init_file(&init), Ok(()));
            let config = kernel.config.borrow();
            assert_eq!(config.get("seen"), Some(seen), "{role:?}");
            assert_eq!(config.get("only.second"), only_second, "{role:?}");
        }
    }

    #[test]
    fn constructs_and_exposes_an_empty_eitri_table() {
        let engine = Kernel::new(PathBuf::from("/tmp/eitri-test-config"), refuse_panels).unwrap();
        let ty: String = engine.lua.load("return type(eitri)").eval().unwrap();
        assert_eq!(ty, "table");
    }

    #[test]
    fn eitri_keymap_is_installed_and_records_into_the_engines_store() {
        let engine = Kernel::new(PathBuf::from("/tmp/eitri-test-config"), refuse_panels).unwrap();
        engine.lua.load(r#"eitri.keymap.prefix("C-a")"#).exec().unwrap();
        assert_eq!(
            engine.keymap.borrow().ops(),
            [crate::keymap::KeymapOp::Prefix { key: "C-a".into() }]
        );
    }

    /// A host with no Lua panels still runs a whole `init.lua`: `eitri.panel.register` returns
    /// without raising, so the lines after it (here a config value) are reached.
    #[test]
    fn a_kernel_without_panels_runs_init_lua_and_refuses_a_panel() {
        let dir = std::env::temp_dir().join(format!("nv-kernel-test-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let init = dir.join("init.lua");
        std::fs::write(
            &init,
            "eitri.config.set(\"agent.font_size\", 14)\n\
             eitri.panel.register({ id = \"notes\", title = \"Notes\", position = \"side\", \
             content = { type = \"webview\", url = \"notes.html\" } })\n\
             eitri.config.set(\"after.panel\", \"reached\")\n",
        )
        .unwrap();
        let kernel = Kernel::new(dir.clone(), refuse_panels).unwrap();
        let outcome = kernel.run_and_check_init_file(&init);
        let font = kernel.config.borrow().get("agent.font_size").map(str::to_owned);
        let after = kernel.config.borrow().get("after.panel").map(str::to_owned);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(outcome, Ok(()));
        assert_eq!(font.as_deref(), Some("14"));
        assert_eq!(
            after.as_deref(),
            Some("reached"),
            "register must return without an error"
        );
    }

    /// A refused plugin is a startup failure naming the call, like a refused command id. (The plugin directory
    /// does not exist, so nothing is written into this machine's state home.)
    #[test]
    fn a_refused_plugin_load_fails_the_start() {
        let dir = std::env::temp_dir().join(format!("nv-kernel-plugin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let init = dir.join("init.lua");
        std::fs::write(&init, "pcall(eitri.plugin.load, '/nonexistent/plugin', {})\n").unwrap();
        let kernel = Kernel::new(dir.clone(), |_, _| Ok(())).unwrap();
        let outcome = kernel.run_and_check_init_file(&init);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(outcome.unwrap_err().contains("eitri.plugin.load"));
    }

    /// The documented consequence of the custom searcher: extending `package.path` from `init.lua`
    /// does not add a place `require` looks, even an absolute one. (The tests that change the
    /// process cwd are in `core/tests/lua_require_sandbox.rs`, a process of its own.)
    #[test]
    fn extending_package_path_adds_no_place_require_looks() {
        let elsewhere = std::env::temp_dir().join(format!("nv-lua-test-elsewhere-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&elsewhere).expect("create a module directory outside the config dir");
        std::fs::write(elsewhere.join("extra.lua"), "return 'from-elsewhere'").expect("plant a module there");
        let config_dir = std::env::temp_dir().join(format!("nv-lua-test-config-{}", uuid::Uuid::new_v4()));

        let engine = Kernel::new(config_dir.clone(), refuse_panels).expect("construct Kernel");
        let dir = elsewhere.display().to_string();
        let found: mlua::Result<mlua::Value> = engine
            .lua
            .load(format!(
                "package.path = {dir:?} .. '/?.lua;' .. package.path; return require('extra')"
            ))
            .eval();
        let _ = std::fs::remove_dir_all(&elsewhere);
        assert!(found.is_err(), "require followed an edit to package.path: {found:?}");
    }
}
