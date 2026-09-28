//! The shell's embedded Lua extension kernel -- a separate, independent runtime from Neovim's
//! own internal Lua. Exposes exactly four v1
//! extension points under a `neovibe` global table: `panel.register`, `command.register`, `on`,
//! `config.get`/`config.set`, plus `keymap.prefix`/`keymap.set`/`keymap.del`.

mod panel;

pub(crate) use neovibe_core::lua::command::CommandRegistry;
pub(crate) use neovibe_core::lua::panel::PanelSlot;
pub(crate) use panel::PanelRegistry;

use mlua::{Lua, Table, Value};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

pub(crate) struct LuaEngine {
    lua: Lua,
    pub(crate) panels: Rc<RefCell<PanelRegistry>>,
    pub(crate) commands: Rc<RefCell<CommandRegistry>>,
    events: Rc<RefCell<neovibe_core::lua::event::EventBus>>,
    /// Kept as a field since 2026-09-21, and the comment in `new()` that said it need not be says
    /// why the reason changed: the shell itself now reads a key out of it (`agent.account`) after
    /// `init.lua` has run.
    pub(crate) config: Rc<RefCell<neovibe_core::lua::config::ConfigStore>>,
    /// Stores `init.lua`'s default layout tree and the layout requests queued by commands.
    pub(crate) layout: Rc<RefCell<neovibe_core::lua::layout::LayoutStore>>,
    /// `neovibe.keymap.*`: the calls `init.lua` made, applied by `main()` once it
    /// has run (`neovibe_core::keymap::Keymap::apply_user`).
    pub(crate) keymap: Rc<RefCell<neovibe_core::lua::keymap::KeymapStore>>,
}

impl LuaEngine {
    pub(crate) fn new(config_dir: PathBuf) -> mlua::Result<Self> {
        let lua = Lua::new();
        let neovibe = lua.create_table()?;

        // `Lua::new()` leaves `package.path` searching `./?.lua` and `./?/init.lua`. The shell's
        // working directory is normally the opened project, so a planted module could shadow a
        // config module loaded by `init.lua`. Restrict filesystem module loading to `<config_dir>/lua/`.
        //
        // Do not interpolate `config_dir` into Lua's `;`- and `?`-based path patterns. A literal
        // `;` in the directory name could inject a relative search entry and restore cwd loading.
        // Empty `package.path` and install a Rust searcher at `package.searchers[2]` that resolves
        // paths with `Path::join`, keeping directory names out of the pattern parser. Empty
        // `package.cpath` and remove searchers 3 and 4 to disable native module loading.
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

        let panels = Rc::new(RefCell::new(PanelRegistry::default()));
        let commands = Rc::new(RefCell::new(CommandRegistry::default()));
        let events = Rc::new(RefCell::new(neovibe_core::lua::event::EventBus::default()));
        // Kept as a field now, and the note this replaces is worth keeping in view: the store is
        // held alive regardless by the `get`/`set` closures `config::install` registers on the
        // `neovibe.config` table, which `lua` (a real field below) owns for as long as
        // `LuaEngine` lives. So this field exists for a reader, not for a lifetime -- `main()`
        // reads `agent.account` out of it once `init.lua` has run.
        let config = Rc::new(RefCell::new(neovibe_core::lua::config::ConfigStore::default()));

        panel::install(&lua, &neovibe, panels.clone(), config_dir)?;
        neovibe_core::lua::command::install(&lua, &neovibe, commands.clone())?;
        neovibe_core::lua::event::install(&lua, &neovibe, events.clone())?;
        neovibe_core::lua::config::install(&lua, &neovibe, config.clone())?;
        let layout = Rc::new(RefCell::new(neovibe_core::lua::layout::LayoutStore::default()));
        neovibe_core::lua::layout::install(&lua, &neovibe, layout.clone())?;
        let keymap = Rc::new(RefCell::new(neovibe_core::lua::keymap::KeymapStore::default()));
        neovibe_core::lua::keymap::install(&lua, &neovibe, keymap.clone())?;

        lua.globals().set("neovibe", neovibe)?;

        Ok(Self {
            lua,
            panels,
            commands,
            events,
            config,
            layout,
            keymap,
        })
    }

    pub(crate) fn emit(&self, event_name: &str) {
        neovibe_core::lua::event::emit(&self.lua, &self.events, event_name, Value::Nil);
    }

    /// Same reentrancy hazard and same fix as `neovibe_core::lua::event::emit` (see that function's doc comment):
    /// the command's `action` key is cloned out of `self.commands` under a scoped borrow, which
    /// is dropped *before* the action is actually called -- so an action that itself calls
    /// `neovibe.command.register(...)` (registering another command, from inside a command's own
    /// action) doesn't hit `CommandRegistry`'s `borrow_mut()` while this function's own borrow is
    /// still held.
    pub(crate) fn invoke_command(&self, id: &str) {
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

    /// Loads and executes `path` (expected: `<config_dir>/init.lua`). Never propagates a Lua
    /// error up to the caller -- a broken or missing init.lua must not crash the shell; this
    /// logs and the shell continues with whatever it registered before the error (the editor and
    /// the agent are not registered here at all: every window has them).
    pub(crate) fn load_init_file(&self, path: &std::path::Path) {
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
    fn constructs_and_exposes_an_empty_neovibe_table() {
        let engine = LuaEngine::new(PathBuf::from("/tmp/neovibe-test-config")).unwrap();
        let ty: String = engine.lua.load("return type(neovibe)").eval().unwrap();
        assert_eq!(ty, "table");
    }

    #[test]
    fn neovibe_keymap_is_installed_and_records_into_the_engines_store() {
        let engine = LuaEngine::new(PathBuf::from("/tmp/neovibe-test-config")).unwrap();
        engine.lua.load(r#"neovibe.keymap.prefix("C-a")"#).exec().unwrap();
        assert_eq!(
            engine.keymap.borrow().ops(),
            [neovibe_core::keymap::KeymapOp::Prefix { key: "C-a".into() }]
        );
    }

    /// Serializes this file's cwd-mutating test against itself (and any later one added here) --
    /// same pattern as `agent::transcript::CLAUDE_CONFIG_DIR_TEST_LOCK`: cargo runs a crate's unit
    /// tests multi-threaded in one process by default, and `std::env::set_current_dir` is
    /// process-wide, so an unguarded second test could observe (or fight over) a cwd this one is
    /// still using.
    static CWD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Checks that `require` resolves filesystem modules from `<config_dir>/lua/` rather than
    /// the launch cwd. Lua's default `package.path` searches the opened project, allowing a
    /// planted file to shadow a config module. The test places the same module name in both
    /// locations and checks that the config copy is used and `package.cpath` is empty.
    #[test]
    fn require_resolves_only_against_the_config_directory_never_the_launch_cwd() {
        let _cwd_guard = CWD_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let original_cwd = std::env::current_dir().expect("cwd");

        let project_dir = std::env::temp_dir().join(format!("nv-lua-test-project-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&project_dir).expect("create fake project dir");
        std::fs::write(project_dir.join("planted.lua"), "return 'planted-from-project'")
            .expect("plant a module in the fake project");

        let config_dir = std::env::temp_dir().join(format!("nv-lua-test-config-{}", uuid::Uuid::new_v4()));
        let config_lua_dir = config_dir.join("lua");
        std::fs::create_dir_all(&config_lua_dir).expect("create fake config/lua dir");
        std::fs::write(config_lua_dir.join("mymodule.lua"), "return 'from-config'")
            .expect("plant a module in the fake config dir");

        std::env::set_current_dir(&project_dir).expect("chdir into the fake project");
        // Caught rather than propagated with `?`/`unwrap`, so the cwd (a process-wide resource
        // other tests share) is always restored below even if an assertion fails.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let engine = LuaEngine::new(config_dir.clone()).expect("construct LuaEngine");

            let planted: mlua::Result<mlua::Value> = engine.lua.load("return require('planted')").eval();
            assert!(
                planted.is_err(),
                "require found a module planted in the launch cwd, via './?.lua': {planted:?}"
            );

            let found: String = engine
                .lua
                .load("return require('mymodule')")
                .eval()
                .expect("require should find a module under <config_dir>/lua/");
            assert_eq!(found, "from-config");

            let package: mlua::Table = engine.lua.globals().get("package").expect("global 'package' table");
            let cpath: String = package.get("cpath").expect("package.cpath");
            assert_eq!(cpath, "", "package.cpath must be empty -- no native module loading");
        }));

        std::env::set_current_dir(&original_cwd).expect("restore the real cwd");
        let _ = std::fs::remove_dir_all(&project_dir);
        let _ = std::fs::remove_dir_all(&config_dir);

        outcome.unwrap();
    }

    /// Checks that a semicolon in `config_dir` cannot inject a relative Lua search entry.
    /// Interpolating the directory into `package.path` would let Lua split it at `;` and search the
    /// launch cwd. The injected `plugins/lua/?.lua` module must not resolve, while a module in the
    /// config directory's own `lua/` must still resolve.
    #[test]
    fn a_semicolon_in_the_config_dir_cannot_smuggle_in_a_relative_search_entry() {
        let _cwd_guard = CWD_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let original_cwd = std::env::current_dir().expect("cwd");

        let project_dir = std::env::temp_dir().join(format!("nv-lua-test-project-{}", uuid::Uuid::new_v4()));
        // The relative entry the old (string-interpolated) code would have produced by splitting
        // `config_dir` on `;` and gluing `lua/?.lua` onto whatever followed it.
        std::fs::create_dir_all(project_dir.join("plugins/lua")).expect("create fake project's plugins/lua");
        std::fs::write(
            project_dir.join("plugins/lua/smuggled.lua"),
            "return 'smuggled-via-semicolon'",
        )
        .expect("plant the module the old bug would have found");

        // A config_dir that itself contains a literal `;` -- an ordinary, valid Unix path
        // character, and exactly the shape the finding names.
        let config_dir = std::env::temp_dir().join(format!("nv-lua-test-config-{};plugins", uuid::Uuid::new_v4()));
        let config_lua_dir = config_dir.join("lua");
        std::fs::create_dir_all(&config_lua_dir).expect("create the semicolon-bearing config/lua dir");
        std::fs::write(config_lua_dir.join("real.lua"), "return 'from-the-real-config-dir'")
            .expect("plant a module under the real (semicolon-bearing) config dir");

        std::env::set_current_dir(&project_dir).expect("chdir into the fake project");
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let engine = LuaEngine::new(config_dir.clone()).expect("construct LuaEngine");

            let smuggled: mlua::Result<mlua::Value> = engine.lua.load("return require('smuggled')").eval();
            assert!(
                smuggled.is_err(),
                "require found the module a semicolon-injected relative entry would have smuggled \
                 in from the launch cwd: {smuggled:?}"
            );

            let found: String = engine
                .lua
                .load("return require('real')")
                .eval()
                .expect("require should still find a module under the semicolon-bearing config dir's own lua/");
            assert_eq!(found, "from-the-real-config-dir");
        }));

        std::env::set_current_dir(&original_cwd).expect("restore the real cwd");
        let _ = std::fs::remove_dir_all(&project_dir);
        let _ = std::fs::remove_dir_all(&config_dir);

        outcome.unwrap();
    }

    /// Checks that a `require` name cannot escape `config_lua_dir` through an absolute path.
    /// `PathBuf::join` discards its base when the joined path is absolute, so a name whose dots
    /// become a leading slash must be rejected. The target is outside both the config directory and
    /// the cwd, and converting the test name's dots to slashes reproduces its path exactly.
    #[test]
    fn a_require_name_cannot_join_into_an_absolute_path_outside_the_config_dir() {
        let _cwd_guard = CWD_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let original_cwd = std::env::current_dir().expect("cwd");

        // No '.' in this path, so replacing '/' with '.' to build the `require` name, then
        // replacing '.' with '/' back inside the searcher, round-trips exactly.
        let absolute_target = std::env::temp_dir().join(format!("nv-lua-test-escape-{}", uuid::Uuid::new_v4()));
        let absolute_target_str = absolute_target.display().to_string();
        assert!(
            !absolute_target_str.contains('.'),
            "test fixture assumption broken: the target path must contain no '.' for the \
             name<->path round-trip below to hold"
        );
        std::fs::write(
            format!("{absolute_target_str}.lua"),
            "return 'escaped-outside-config-dir'",
        )
        .expect("plant the file an escaping join would read");
        let escaping_name = absolute_target_str.replace('/', ".");

        let config_dir = std::env::temp_dir().join(format!("nv-lua-test-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(config_dir.join("lua")).expect("create fake config/lua dir");

        std::env::set_current_dir(std::env::temp_dir()).expect("chdir somewhere unrelated");
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let engine = LuaEngine::new(config_dir.clone()).expect("construct LuaEngine");

            let escaped: mlua::Result<mlua::Value> =
                engine.lua.load(format!("return require('{escaping_name}')")).eval();
            assert!(
                escaped.is_err(),
                "require escaped config_lua_dir via an absolute join and read {absolute_target_str}.lua: \
                 {escaped:?}"
            );
        }));

        std::env::set_current_dir(&original_cwd).expect("restore the real cwd");
        let _ = std::fs::remove_file(format!("{absolute_target_str}.lua"));
        let _ = std::fs::remove_dir_all(&config_dir);

        outcome.unwrap();
    }
}
