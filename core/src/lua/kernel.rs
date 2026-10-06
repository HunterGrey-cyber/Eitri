//! The Lua extension kernel without a toolkit: the `mlua` state, the `require` sandbox that resolves
//! only under `<config dir>/lua/`, and the command, event, config, layout and keymap stores behind
//! `eitri.*` (a Lua runtime separate from Neovim's own; see docs/canonical/neovibe_architecture_decisions.md §3).
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

        // `Lua::new()` (mlua's vendored lua54) leaves `package.path` at its compiled-in default,
        // whose entries include `./?.lua;./?/init.lua` -- i.e. Lua's own `require` searches the
        // process's *current working directory* by default. `shell` never `chdir`s away from the
        // project it opens, so that cwd is normally the project the user has open, not the
        // owner's config directory -- an unguarded `require` in `init.lua` would let a file
        // planted in the opened *project* shadow, or simply run as, one of the owner's own config
        // modules. Route `require` at the config
        // directory only, mirroring nvim's own `lua/` directory convention, and close C module
        // loading (`cpath = ""`) since nothing here ships a native Lua module.
        //
        // The config-dir-only entries are not built by string-interpolating `config_dir` into
        // `package.path`'s own `;`-and-`?`-separated pattern syntax (`"{config_dir}/lua/?.lua;..."`). A `config_dir`
        // that itself contains a literal `;` -- an entirely ordinary Unix path character, and
        // exactly what a misconfigured `EITRI_CONFIG_DIR` could contain -- splits into an
        // extra, *relative* search entry once Lua parses that string, reopening precisely the
        // cwd-search hole this function exists to close (e.g. `EITRI_CONFIG_DIR=
        // "/home/user/config;plugins"` leaves a `plugins/lua/?.lua` entry that resolves against
        // the launch cwd, not the config dir). `package.path` is no longer built by string
        // interpolation at all: it is emptied, and a Rust closure is installed as the "Lua
        // path" searcher (`package.searchers[2]`, replacing Lua's own) that joins `config_dir`
        // with `Path::join` and never re-parses it as pattern syntax, so no character
        // `config_dir` contains, `;` included, can ever inject a second entry. The default C-path
        // searcher and all-in-one loader (`package.searchers[3]`/`[4]`) are removed outright
        // rather than left relying on `cpath == ""` alone to make them harmless.
        //
        // **So `package.path` and `package.cpath` are read by nothing**: an
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
