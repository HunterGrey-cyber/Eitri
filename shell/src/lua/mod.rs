//! The shell's embedded Lua extension kernel -- a separate, independent runtime from Neovim's
//! own internal Lua (see docs/canonical/neovibe_architecture_decisions.md §3). Exposes exactly four v1
//! extension points under an `eitri` global table: `panel.register`, `command.register`, `on`,
//! `config.get`/`config.set`, plus `keymap.prefix`/`keymap.set`/`keymap.del` (keymap spec §2.3).

mod panel;

pub(crate) use eitri_core::lua::command::CommandRegistry;
pub(crate) use eitri_core::lua::panel::PanelSlot;
pub(crate) use panel::PanelRegistry;

use mlua::{Lua, Table, Value};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

pub(crate) struct LuaEngine {
    lua: Lua,
    pub(crate) panels: Rc<RefCell<PanelRegistry>>,
    pub(crate) commands: Rc<RefCell<CommandRegistry>>,
    events: Rc<RefCell<eitri_core::lua::event::EventBus>>,
    /// Kept as a field since 2026-09-21, and the comment in `new()` that said it need not be says
    /// why the reason changed: the shell itself now reads a key out of it (`agent.account`) after
    /// `init.lua` has run.
    pub(crate) config: Rc<RefCell<eitri_core::lua::config::ConfigStore>>,
    /// `eitri.layout.*` (modules P2): `init.lua`'s default tree, and the requests commands queue.
    pub(crate) layout: Rc<RefCell<eitri_core::lua::layout::LayoutStore>>,
    /// `eitri.keymap.*` (keymap spec §2.3): the calls `init.lua` made, applied by `main()` once it
    /// has run (`eitri_core::keymap::Keymap::apply_user`).
    pub(crate) keymap: Rc<RefCell<eitri_core::lua::keymap::KeymapStore>>,
}

impl LuaEngine {
    pub(crate) fn new(config_dir: PathBuf) -> mlua::Result<Self> {
        let lua = Lua::new();
        let eitri = lua.create_table()?;

        // `Lua::new()` (mlua's vendored lua54) leaves `package.path` at its compiled-in default,
        // whose entries include `./?.lua;./?/init.lua` -- i.e. Lua's own `require` searches the
        // process's *current working directory* by default. `shell` never `chdir`s away from the
        // project it opens, so that cwd is normally the project the user has open, not the
        // owner's config directory -- an unguarded `require` in `init.lua` would let a file
        // planted in the opened *project* shadow, or simply run as, one of the owner's own config
        // modules (sweep verdict "sw-lua-1",
        // the private review notes). Route `require` at the config
        // directory only, mirroring nvim's own `lua/` directory convention, and close C module
        // loading (`cpath = ""`) since nothing here ships a native Lua module.
        //
        // **Correction (codex-sweep round 1):** the first cut of this fix built the config-dir-only
        // entries by string-interpolating `config_dir` straight into `package.path`'s own
        // `;`-and-`?`-separated pattern syntax (`"{config_dir}/lua/?.lua;..."`). A `config_dir`
        // that itself contains a literal `;` -- an entirely ordinary Unix path character, and
        // exactly what a misconfigured `EITRI_CONFIG_DIR` could contain -- splits into an
        // extra, *relative* search entry once Lua parses that string, reopening precisely the
        // cwd-search hole this function exists to close (e.g. `EITRI_CONFIG_DIR=
        // "/home/user/config;plugins"` leaves a `plugins/lua/?.lua` entry that resolves against
        // the launch cwd, not the config dir). `package.path` is no longer built by string
        // interpolation at all -- it is emptied, and a Rust closure is installed as the "Lua
        // path" searcher (`package.searchers[2]`, replacing Lua's own) that joins `config_dir`
        // with `Path::join` and never re-parses it as pattern syntax, so no character
        // `config_dir` contains, `;` included, can ever inject a second entry. The default C-path
        // searcher and all-in-one loader (`package.searchers[3]`/`[4]`) are removed outright
        // rather than left relying on `cpath == ""` alone to make them harmless.
        //
        // **So `package.path` and `package.cpath` are read by nothing** (whole-branch review): an
        // `init.lua` that extends either -- the usual Lua idiom for a second module directory --
        // changes nothing, silently, and `package.searchpath` answers for a path `require` never
        // consults. `<config_dir>/lua/` is the only place `require` looks.
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
        let events = Rc::new(RefCell::new(eitri_core::lua::event::EventBus::default()));
        // Kept as a field now, and the note this replaces is worth keeping in view: the store is
        // held alive regardless by the `get`/`set` closures `config::install` registers on the
        // `eitri.config` table, which `lua` (a real field below) owns for as long as
        // `LuaEngine` lives. So this field exists for a reader, not for a lifetime -- `main()`
        // reads `agent.account` out of it once `init.lua` has run.
        let config = Rc::new(RefCell::new(eitri_core::lua::config::ConfigStore::default()));

        panel::install(&lua, &eitri, panels.clone(), config_dir)?;
        eitri_core::lua::command::install(&lua, &eitri, commands.clone())?;
        eitri_core::lua::event::install(&lua, &eitri, events.clone())?;
        eitri_core::lua::config::install(&lua, &eitri, config.clone())?;
        let layout = Rc::new(RefCell::new(eitri_core::lua::layout::LayoutStore::default()));
        eitri_core::lua::layout::install(&lua, &eitri, layout.clone())?;
        let keymap = Rc::new(RefCell::new(eitri_core::lua::keymap::KeymapStore::default()));
        eitri_core::lua::keymap::install(&lua, &eitri, keymap.clone())?;

        lua.globals().set("eitri", eitri)?;

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
        eitri_core::lua::event::emit(&self.lua, &self.events, event_name, Value::Nil);
    }

    /// Same reentrancy hazard and same fix as `eitri_core::lua::event::emit` (see that function's doc comment):
    /// the command's `action` key is cloned out of `self.commands` under a scoped borrow, which
    /// is dropped *before* the action is actually called -- so an action that itself calls
    /// `eitri.command.register(...)` (registering another command, from inside a command's own
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
    ///
    /// **Two exceptions, startup failures: a command id `eitri.command.register` refused**
    /// (`CommandRegistry::refused`), **and a value `eitri.config.set` could not store**
    /// (`ConfigStore::refused`: a table, a function, a string that is not UTF-8, a key
    /// that is not a string). Both are config values Eitri validates, and like `agent.font_size` or
    /// a keybinding collision (`main.rs`) they exit 1 naming themselves -- rather than a window
    /// that silently lacks everything init.lua registered after them, or runs on a key's default:
    /// an `agent.account` that never reached the store spends whichever account launched the
    /// window. Checked after the file has run, so a `pcall` around the call changes nothing.
    pub(crate) fn load_init_file(&self, path: &std::path::Path) {
        if let Err(message) = self.run_and_check_init_file(path) {
            eprintln!("eitri: {message}");
            std::process::exit(1);
        }
    }

    /// [`Self::load_init_file`] without the exit: `Err` is the startup-failure text after "eitri: ".
    pub(crate) fn run_and_check_init_file(&self, path: &std::path::Path) -> Result<(), String> {
        self.run_init_file(path);
        let commands = self.commands.borrow();
        let config = self.config.borrow();
        match commands.refused().or(config.refused()) {
            Some(refused) => Err(format!("{} ({})", refused, path.display())),
            None => Ok(()),
        }
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
    fn constructs_and_exposes_an_empty_eitri_table() {
        let engine = LuaEngine::new(PathBuf::from("/tmp/eitri-test-config")).unwrap();
        let ty: String = engine.lua.load("return type(eitri)").eval().unwrap();
        assert_eq!(ty, "table");
    }

    #[test]
    fn eitri_keymap_is_installed_and_records_into_the_engines_store() {
        let engine = LuaEngine::new(PathBuf::from("/tmp/eitri-test-config")).unwrap();
        engine.lua.load(r#"eitri.keymap.prefix("C-a")"#).exec().unwrap();
        assert_eq!(
            engine.keymap.borrow().ops(),
            [eitri_core::keymap::KeymapOp::Prefix { key: "C-a".into() }]
        );
    }

    /// Serializes this file's cwd-mutating test against itself (and any later one added here) --
    /// same pattern as `agent::transcript::CLAUDE_CONFIG_DIR_TEST_LOCK`: cargo runs a crate's unit
    /// tests multi-threaded in one process by default, and `std::env::set_current_dir` is
    /// process-wide, so an unguarded second test could observe (or fight over) a cwd this one is
    /// still using.
    static CWD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Regression test for sw-lua-1 (`the private review notes`): before
    /// the fix, `Lua::new()`'s default `package.path` included `./?.lua;./?/init.lua`, so
    /// `require` searched the process's current working directory -- which for `shell` is
    /// normally the project the user opened, never the owner's own config directory. A module
    /// planted in an opened project could therefore shadow, or simply run as, one of the owner's
    /// own `init.lua` modules. This reproduces the exact failure the verdict's probe found (a
    /// `local_settings`-shaped module resolving from cwd) and the fix this closes it with (the
    /// same name resolving from `<config_dir>/lua/` instead, and `package.cpath` empty).
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

    /// The documented consequence of the searcher above (whole-branch review): extending
    /// `package.path` from `init.lua` does not add a place `require` looks, even an absolute one.
    #[test]
    fn extending_package_path_adds_no_place_require_looks() {
        let elsewhere = std::env::temp_dir().join(format!("nv-lua-test-elsewhere-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&elsewhere).expect("create a module directory outside the config dir");
        std::fs::write(elsewhere.join("extra.lua"), "return 'from-elsewhere'").expect("plant a module there");
        let config_dir = std::env::temp_dir().join(format!("nv-lua-test-config-{}", uuid::Uuid::new_v4()));

        let engine = LuaEngine::new(config_dir.clone()).expect("construct LuaEngine");
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

    /// Regression test for a codex-sweep round-1 finding on sw-lua-1's own fix: the first cut
    /// built `package.path` by string-interpolating `config_dir` into `;`-and-`?`-separated
    /// pattern syntax. A `config_dir` containing a literal `;` (a perfectly ordinary Unix path
    /// character -- the finding's own example is `EITRI_CONFIG_DIR="/home/user/config;plugins"`)
    /// then split into an extra, *relative* search entry once Lua parsed that string, reopening
    /// exactly the cwd-search hole sw-lua-1 was meant to close. Plants the module the old bug's
    /// injected relative entry (`plugins/lua/?.lua`, resolved against the launch cwd) would have
    /// found, and a real module under the semicolon-bearing config dir's own `lua/`, and checks
    /// both directions: the smuggled one must not resolve, the real one still must.
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

    /// Regression test for a self-found hazard in the custom searcher this fix installs, caught
    /// while working through the codex-sweep round-1 semicolon finding rather than reported by
    /// it: `PathBuf::join` replaces its base entirely whenever the joined argument is itself
    /// absolute. A `require` name whose `.`-to-`/` conversion happens to produce a leading `/`
    /// (e.g. `require(".etc.passwd")` -> the relative-looking string `/etc/passwd`) would
    /// otherwise let the searcher read an arbitrary absolute `.lua` file with no connection to
    /// `config_lua_dir` at all -- a strictly worse escape than the cwd search sw-lua-1 closed.
    /// Constructs exactly that shape: an absolute target outside both the config dir and the cwd,
    /// and a `require` name engineered so `name.replace('.', "/")` reproduces that target's path
    /// byte-for-byte.
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
