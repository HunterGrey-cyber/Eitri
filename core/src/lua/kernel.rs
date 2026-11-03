//! The Lua extension kernel without a toolkit: the `mlua` state, the `require` sandbox that resolves
//! only under `<config dir>/lua/`, and the command, event, config, layout and keymap stores behind
//! `eitri.*`, a Lua runtime that remains separate from Neovim's own.
//! A host decides what `eitri.panel.register` does by passing an installer to [`Kernel::new`]: `shell`
//! builds a WebKitGTK view, a host with no Lua panels passes [`refuse_panels`].

use mlua::{Lua, Table, Value};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

/// One call's outcome as the function boundary saw it: the call, the key that tells two calls of one function apart
/// (a workspace's name, a config key), the refusal if it was refused, and the `file:line` of the Lua that made it.
#[derive(Debug)]
struct Outcome {
    name: &'static str,
    key: Option<String>,
    error: Option<String>,
    origin: Option<String>,
}

/// Refusals belong to the Lua state so every function boundary shares the startup decision.
#[derive(Debug)]
pub(crate) struct InitRefusals {
    accepting: bool,
    path: Option<PathBuf>,
    entries: Vec<Outcome>,
    /// Every refusal in the order of the calls, with the `file:line` of each: `entries` keeps one per call and key
    /// for the startup decision, and this keeps them all for what the window says.
    refused: Vec<(Option<String>, String)>,
}

impl Default for InitRefusals {
    fn default() -> Self {
        Self {
            accepting: true,
            path: None,
            entries: Vec::new(),
            refused: Vec::new(),
        }
    }
}

impl InitRefusals {
    /// Whether `init.lua` is running (or has not run yet): once it has, the window has read what it
    /// registered, and a call that only registers for the window to read then has nothing to act on.
    pub(crate) fn in_init(&self) -> bool {
        self.accepting
    }

    pub(crate) fn message(&self, name: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|entry| entry.name == name && entry.error.is_some())
            .and_then(|entry| entry.error.clone())
    }

    /// The refusal of a call of `name` that was about `key`, if there was one.
    pub(crate) fn refusal_for(&self, name: &str, key: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|entry| entry.name == name && entry.key.as_deref() == Some(key))
            .and_then(|entry| entry.error.clone())
    }

    /// The refusals of calls of `name` about a key that starts with `prefix`, as `(key, why)`.
    pub(crate) fn refusals_under(&self, name: &str, prefix: &str) -> Vec<(String, String)> {
        self.entries
            .iter()
            .filter(|entry| entry.name == name)
            .filter_map(|entry| {
                let key = entry.key.as_deref().filter(|key| key.starts_with(prefix))?;
                Some((key.to_string(), entry.error.clone()?))
            })
            .collect()
    }

    fn record(&mut self, name: &'static str, key: Option<String>, error: Option<String>, origin: Option<String>) {
        if !self.accepting {
            return;
        }
        if let Some(error) = &error {
            self.refused.push((origin.clone(), error.clone()));
        }
        let replace = name == "eitri.layout.default" || (name == "eitri.layout.workspace" && key.is_some());
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.name == name && entry.key == key)
        {
            if replace || entry.error.is_none() {
                entry.error = error;
                entry.origin = origin;
            }
        } else {
            self.entries.push(Outcome {
                name,
                key,
                error,
                origin,
            });
        }
    }

    fn first(&self) -> Option<String> {
        self.entries
            .iter()
            .filter_map(|entry| entry.error.as_ref().map(|error| (&entry.name, error)))
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

    /// Every refusal, one line each in the order the calls were made: where the call was, then why it was refused.
    fn lines(&self) -> Vec<String> {
        let file = self.path.as_ref().map(|path| path.display().to_string());
        self.refused
            .iter()
            .map(|(origin, error)| match origin.as_ref().or(file.as_ref()) {
                Some(origin) => format!("{origin}: {error}"),
                None => error.clone(),
            })
            .collect()
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
        let workspace = if name == "eitri.layout.workspace" || name == "eitri.config.set" {
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
        let origin = error.is_some().then(|| caller_origin(lua)).flatten();
        shared.borrow_mut().record(name, workspace, error, origin);
        result
    })
}

/// `file:line` of the Lua that called the Rust function now running, or `None` outside Lua (a call from Rust). A
/// chunk loaded from a string rather than a file has no file to name.
pub(crate) fn caller_origin(lua: &Lua) -> Option<String> {
    lua.inspect_stack(1, |frame| {
        let source = frame.source().source?.into_owned();
        let file = source.strip_prefix('@')?.to_string();
        Some(format!("{file}:{}", frame.current_line()?))
    })
    .flatten()
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
    /// What `init.lua` asked of Eitri that Eitri did not do, one line each, where the person can find it: a Lua
    /// error, a call refused at the function boundary, a value that is not one, a key nothing reads. The window
    /// opens regardless and says these (`Kernel::startup_problems`).
    problems: RefCell<Vec<String>>,
    /// The Lua error that ended `init.lua`, as `(file:line: why, the error's own first line)`.
    init_error: RefCell<Option<(String, String)>>,
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

        // `Lua::new()` (mlua's vendored lua54) leaves `package.path` at its compiled-in default,
        // whose entries include `./?.lua;./?/init.lua` -- i.e. Lua's own `require` searches the
        // process's *current working directory* by default. `shell` never `chdir`s away from the
        // project it opens, so that cwd is normally the project the user has open, not the
        // user's config directory -- an unguarded `require` in `init.lua` would let a file
        // planted in the opened *project* shadow, or simply run as, one of the user's own config
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
            problems: RefCell::new(Vec::new()),
            init_error: RefCell::new(None),
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

    /// Names a chat or page window for Lua by its window name's text (`"c3"`, `"p:markdown"`): `eitri.window` and
    /// `eitri.layout.get().window` read it, as [`Kernel::set_window`] sets them for the other windows.
    pub fn set_window_name(&self, name: &str) -> mlua::Result<()> {
        self.layout.borrow_mut().set_window(Some(name));
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

    /// Loads init.lua and goes on whatever it did, as nvim does with its own: a Lua error leaves the registrations made
    /// before it, a call refused at the function boundary (even one Lua caught) did nothing, and each is a
    /// [`Kernel::startup_problems`] line naming the file and line, so the window can say it.
    pub fn load_init_file(&self, path: &std::path::Path) {
        let _ = self.run_and_check_init_file(path);
        let lines = self.init_refusals.borrow().lines();
        for line in lines {
            self.report_problem(line);
        }
        // A refused call that nothing caught also ended the file with the same words; they are one problem.
        if let Some((line, message)) = self.init_error.borrow_mut().take() {
            if !self.problems.borrow().iter().any(|known| known.ends_with(&message)) {
                self.report_problem(line);
            }
        }
    }

    /// Adds a line to what [`Kernel::startup_problems`] says, and logs it where a terminal would see it.
    pub fn report_problem(&self, text: impl Into<String>) {
        let text = text.into();
        eprintln!("[init] {text}");
        self.problems.borrow_mut().push(text);
    }

    /// What `init.lua` asked of Eitri that Eitri did not do, in the order it was found. Empty when the file was fine.
    pub fn startup_problems(&self) -> Vec<String> {
        self.problems.borrow().clone()
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

    /// Runs `text` as one chunk under `init.lua`'s rules, with no file behind it: `Err` is the refusal the chunk
    /// caused, caught or not, or else its Lua error. A refusal it did not catch is a Lua error too, and the refusal's
    /// own text is the one that names the call. A refusal made before this chunk is not its own: in a kernel that has
    /// none, the refusal named is the one a strict host would name ([`InitRefusals::first`]); after earlier ones, the
    /// first this chunk made.
    pub fn run_checked_text(&self, name: &str, text: &str) -> Result<(), String> {
        let before = {
            let mut refusals = self.init_refusals.borrow_mut();
            refusals.accepting = true;
            refusals.path = None;
            refusals.refused.len()
        };
        let ran = self.lua.load(text).set_name(name).exec();
        let caused = {
            let mut refusals = self.init_refusals.borrow_mut();
            refusals.accepting = false;
            match before {
                0 => refusals.first(),
                _ => refusals.refused.get(before).map(|(_, error)| error.clone()),
            }
        };
        caused.map_or(ran.map_err(|error| error.to_string()), Err)
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
                let chunk = format!("@{}", path.display());
                if let Err(err) = self.lua.load(&src).set_name(chunk).exec() {
                    *self.init_error.borrow_mut() = Some(lua_error_line(path, &err));
                    eprintln!("[lua] error loading {}: {err}", path.display());
                }
            }
            Err(err) => self.report_problem(format!("{}: could not be read: {err}", path.display())),
        }
    }
}

/// What a Lua error says, as `(file:line: why, why)`: its first line with the file Lua names (cut short when the
/// path is long) written out in full. An error a Rust function raised names no place in its first line; the Lua
/// line that called it is the first of the stack traceback below.
fn lua_error_line(init: &std::path::Path, error: &mlua::Error) -> (String, String) {
    let text = error.to_string();
    let message = text
        .lines()
        .filter(|line| !line.starts_with('\t') && !line.starts_with("stack traceback") && *line != "callback error")
        .map(|line| {
            ["caused by: ", "runtime error: ", "syntax error: "]
                .iter()
                .fold(line, |line, prefix| line.trim_start_matches(prefix))
        })
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let full = init.display().to_string();
    if let Some((at, _)) = located(message) {
        let chunk = &message[..at];
        let why = message[at..].trim_start_matches(|c: char| c == ':' || c.is_ascii_digit() || c == ' ');
        if chunk.starts_with("[string") || full.ends_with(chunk.trim_start_matches("...")) {
            return (format!("{full}{}", &message[at..]), why.to_string());
        }
        return (message.to_string(), why.to_string());
    }
    // A Rust function raised it: the Lua line that called it is the first stack frame in this file.
    let line = text
        .lines()
        .filter(|line| line.starts_with('\t'))
        .find_map(|line| {
            let (at, digits) = located(line)?;
            full.ends_with(line[..at].trim().trim_start_matches("..."))
                .then(|| digits.to_string())
        })
        .map_or_else(String::new, |digits| format!(":{digits}"));
    (format!("{full}{line}: {message}"), message.to_string())
}

/// Where in `line` its `:<digits>: ` is, and the digits.
fn located(line: &str) -> Option<(usize, &str)> {
    line.match_indices(':').find_map(|(at, _)| {
        let rest = &line[at + 1..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        (digits > 0 && rest[digits..].starts_with(": ")).then(|| (at, &rest[..digits]))
    })
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
    fn every_refused_call_is_a_problem_even_when_two_read_alike() {
        let scratch = crate::plugin::test_support::Scratch::new("lua-every-refusal");
        let init = scratch.0.join("init.lua");
        std::fs::write(
            &init,
            "pcall(eitri.layout.rule, 42, {})\npcall(eitri.layout.rule, true, {})",
        )
        .unwrap();
        let kernel = Kernel::new(scratch.0.clone(), refuse_panels).unwrap();
        kernel.load_init_file(&init);
        let problems = kernel.startup_problems();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems.iter().all(|line| line.contains(&init.display().to_string())),
            "{problems:?}"
        );
    }

    #[test]
    fn checked_text_reports_a_caught_refusal_without_a_path() {
        let kernel = Kernel::new(PathBuf::from("/nonexistent/eitri-kernel-test"), refuse_panels).unwrap();
        let error = kernel
            .run_checked_text("=text", "pcall(eitri.config.set, {}, true)")
            .unwrap_err();
        assert_eq!(error, "eitri.config.set: the key must be a string");
    }

    #[test]
    fn an_uncaught_refusal_is_reported_as_the_refusal() {
        let kernel = Kernel::new(PathBuf::from("/nonexistent/eitri-kernel-test"), refuse_panels).unwrap();
        let error = kernel
            .run_checked_text("=text", "eitri.config.set({}, true)")
            .unwrap_err();
        assert_eq!(error, "eitri.config.set: the key must be a string");
    }

    /// A refusal an earlier text made, of a higher rank, is not this text's: a later text that refuses nothing runs
    /// clean, and one that refuses names its own refusal.
    #[test]
    fn checked_text_names_only_a_refusal_it_made_itself() {
        let kernel = Kernel::new(PathBuf::from("/nonexistent/eitri-kernel-test"), refuse_panels).unwrap();
        let earlier = kernel
            .run_checked_text(
                "=init",
                "pcall(eitri.command.register, { id = 'a b', title = 't', action = function() end })",
            )
            .unwrap_err();
        assert!(
            earlier.starts_with("eitri.command.register: invalid command id"),
            "{earlier}"
        );
        assert_eq!(
            kernel.run_checked_text("=text", "eitri.config.set('a.key', 'b')"),
            Ok(())
        );
        assert_eq!(
            kernel.run_checked_text("=text", "pcall(eitri.config.set, {}, true)"),
            Err("eitri.config.set: the key must be a string".to_owned())
        );
    }

    #[test]
    fn checked_text_reports_a_lua_error_under_the_name_it_was_given() {
        let kernel = Kernel::new(PathBuf::from("/nonexistent/eitri-kernel-test"), refuse_panels).unwrap();
        let error = kernel.run_checked_text("=text", "eitri.keymap.prefix(").unwrap_err();
        assert!(error.contains("text:1:"), "{error}");
    }

    #[test]
    fn checked_text_records_what_it_runs_and_closes_recording_after() {
        let kernel = Kernel::new(PathBuf::from("/nonexistent/eitri-kernel-test"), refuse_panels).unwrap();
        let text = "eitri.keymap.prefix('C-a')\neitri.config.set('a.key', 200)\n";
        assert_eq!(kernel.run_checked_text("=text", text), Ok(()));
        assert_eq!(
            kernel.keymap.borrow().ops(),
            [crate::keymap::KeymapOp::Prefix { key: "C-a".into() }]
        );
        assert_eq!(kernel.config.borrow().get("a.key"), Some("200"));
        kernel
            .lua
            .load("assert(not pcall(eitri.config.set, {}, true))")
            .exec()
            .unwrap();
        assert_eq!(kernel.init_refusal(), None);
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

    /// Plugins are read once, when `init.lua` has run: a load after that would report success and show
    /// nothing, so it is refused and loads nothing.
    #[test]
    fn a_plugin_load_after_init_is_refused() {
        let scratch = crate::plugin::test_support::Scratch::new("lua-late-plugin-load");
        let kernel = Kernel::new(scratch.0.clone(), |_, _| Ok(())).unwrap();
        assert_eq!(kernel.run_and_check_init_file(&scratch.0.join("missing.lua")), Ok(()));
        let error: String = kernel
            .lua
            .load("local ok, e = pcall(eitri.plugin.load, '/nonexistent/plugin'); assert(not ok); return tostring(e)")
            .eval()
            .unwrap();
        assert!(error.contains("eitri.plugin.load: only in init.lua"), "{error}");
        assert!(kernel.plugins.borrow().plugins().is_empty());
        assert_eq!(kernel.init_refusal(), None);
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
    fn a_module_window_reads_its_own_name_as_eitri_window() {
        let scratch = crate::plugin::test_support::Scratch::new("lua-eitri-module-window");
        let init = scratch.0.join("init.lua");
        std::fs::write(&init, "eitri.config.set('seen', eitri.window)\n").unwrap();
        for name in ["c3", "p:markdown"] {
            let kernel = Kernel::new(scratch.0.clone(), refuse_panels).unwrap();
            kernel.set_window_name(name).unwrap();
            assert_eq!(kernel.run_and_check_init_file(&init), Ok(()));
            assert_eq!(kernel.config.borrow().get("seen"), Some(name));
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
