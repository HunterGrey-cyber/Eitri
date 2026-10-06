//! The Lua extension kernel without a toolkit: the `mlua` state, the `require` sandbox that resolves
//! only under `<config dir>/lua/`, and the command, event, config, layout and keymap stores behind
//! `eitri.*`.
//! A host decides what `eitri.panel.register` does by passing an installer to [`Kernel::new`]: `shell`
//! builds a WebKitGTK view, a host with no Lua panels passes [`refuse_panels`].

use mlua::{Lua, Table, Value};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

pub struct Kernel {
    lua: Lua,
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
}

/// The panel installer for a host that has no Lua panels (the companion window, `eitri-mac`):
/// `eitri.panel.register` logs once and does nothing, so an `init.lua` written for the full window
/// still loads and every later line of it still runs.
pub fn refuse_panels(lua: &Lua, eitri: &Table) -> mlua::Result<()> {
    let told = Rc::new(Cell::new(false));
    let register = lua.create_function(move |_, _spec: Value| {
        if !told.replace(true) {
            eprintln!("eitri: Lua panels are not in this window");
        }
        Ok(())
    })?;
    let panel = lua.create_table()?;
    panel.set("register", register)?;
    eitri.set("panel", panel)?;
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
        super::command::install(&lua, &eitri, commands.clone())?;
        super::event::install(&lua, &eitri, events.clone())?;
        super::config::install(&lua, &eitri, config.clone())?;
        let layout = Rc::new(RefCell::new(super::layout::LayoutStore::default()));
        super::layout::install(&lua, &eitri, layout.clone())?;
        let keymap = Rc::new(RefCell::new(super::keymap::KeymapStore::default()));
        super::keymap::install(&lua, &eitri, keymap.clone())?;

        lua.globals().set("eitri", eitri)?;

        Ok(Kernel {
            lua,
            commands,
            events,
            config,
            layout,
            keymap,
        })
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

    /// Loads and executes `path` (expected: `<config_dir>/init.lua`). Never propagates a Lua
    /// error up to the caller -- a broken or missing init.lua must not crash the window; this
    /// logs and the shell continues with whatever it registered before the error (the editor and
    /// the agent are not registered here at all: every window has them).
    ///
    /// **Two exceptions, startup failures: a command id `eitri.command.register` refused**
    /// (`CommandRegistry::refused`), **and a value `eitri.config.set` could not store**
    /// (`ConfigStore::refused`: a table, a function, a string that is not UTF-8, a key
    /// that is not a string). Both are config values Eitri validates, and like `agent.font_size` or
    /// a keybinding collision (in the host's `main()`) they exit 1 naming themselves -- rather than a window
    /// that silently lacks everything init.lua registered after them, or runs on a key's default:
    /// an `agent.account` that never reached the store spends whichever account launched the
    /// window. Checked after the file has run, so a `pcall` around the call changes nothing.
    pub fn load_init_file(&self, path: &std::path::Path) {
        if let Err(message) = self.run_and_check_init_file(path) {
            eprintln!("eitri: {message}");
            std::process::exit(1);
        }
    }

    /// [`Self::load_init_file`] without the exit: `Err` is the startup-failure text after "eitri: ".
    pub fn run_and_check_init_file(&self, path: &std::path::Path) -> Result<(), String> {
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

    /// Serializes this file's cwd-mutating test against itself (and any later one added here) --
    /// same pattern as `agent::transcript::CLAUDE_CONFIG_DIR_TEST_LOCK`: cargo runs a crate's unit
    /// tests multi-threaded in one process by default, and `std::env::set_current_dir` is
    /// process-wide, so an unguarded second test could observe (or fight over) a cwd this one is
    /// still using.
    static CWD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Lua's default `package.path` searches the working directory, normally the opened project,
    /// so a planted module could shadow a configuration module. Check that `require` instead
    /// resolves from `<config_dir>/lua/`, ignoring the same module name in cwd, with `package.cpath`
    /// empty.
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
            let engine = Kernel::new(config_dir.clone(), refuse_panels).expect("construct Kernel");

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

    /// The documented consequence of the searcher above: extending
    /// `package.path` from `init.lua` does not add a place `require` looks, even an absolute one.
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

    /// Building `package.path` by string-interpolating `config_dir` into `;`-and-`?`-separated
    /// pattern syntax would go wrong for a `config_dir` containing a literal `;` (a perfectly
    /// ordinary Unix path character, e.g. `EITRI_CONFIG_DIR="/home/user/config;plugins"`): it
    /// would split into an extra, *relative* search entry once Lua parsed that string, reopening
    /// the cwd-search hole the sandbox closes. Plants the module the old bug's
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
            let engine = Kernel::new(config_dir.clone(), refuse_panels).expect("construct Kernel");

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

    /// A hazard in the custom searcher itself: `PathBuf::join` replaces its base entirely whenever the joined argument is itself
    /// absolute. A `require` name whose `.`-to-`/` conversion happens to produce a leading `/`
    /// (e.g. `require(".etc.passwd")` -> the relative-looking string `/etc/passwd`) would
    /// otherwise let the searcher read an arbitrary absolute `.lua` file with no connection to
    /// `config_lua_dir` at all -- a strictly worse escape than the cwd search the sandbox closes.
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
            let engine = Kernel::new(config_dir.clone(), refuse_panels).expect("construct Kernel");

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
