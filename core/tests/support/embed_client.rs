//! A real `nvim --embed --headless` driven over msgpack-RPC, for the tests that inject a snippet into
//! a running nvim the way a companion panel does and then tear it down again.
//!
//! RPC rather than `-c` commands because injection happens after VimEnter, from outside, and because
//! `nvim_input` leaves Visual mode alive where `:normal` would end it. Included with
//! `#[path = "support/embed_client.rs"] mod embed_client;` by each test file that uses it, so each
//! one uses only part of it.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rmpv::Value;

const DEADLINE: Duration = Duration::from_secs(5);
const STEP: Duration = Duration::from_millis(50);

/// What a snippet leaves behind in nvim, read by [`Embed::footprint`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footprint {
    /// Autocommands in the named groups.
    pub autocmds: usize,
    /// Active, not-closing libuv timers in the whole process.
    pub active_timers: usize,
    /// `"<mode> <lhs>"` of every global mapping whose description starts with `eitri`.
    pub eitri_maps: Vec<String>,
    /// Which of the named globals are set.
    pub globals: Vec<String>,
}

const FOOTPRINT_LUA: &str = r#"
local groups, globals = ...
local a = 0
for _, g in ipairs(groups) do
  local ok, x = pcall(vim.api.nvim_get_autocmds, { group = g })
  if ok then a = a + #x end
end
local t = 0
vim.uv.walk(function(h) if h:get_type() == "timer" and h:is_active() and not h:is_closing() then t = t + 1 end end)
local maps = {}
for _, mode in ipairs({ "n", "x", "i" }) do
  for _, m in ipairs(vim.api.nvim_get_keymap(mode)) do
    if (m.desc or ""):find("^eitri") then maps[#maps + 1] = mode .. " " .. m.lhs end
  end
end
local present = {}
for _, name in ipairs(globals) do if rawget(_G, name) ~= nil then present[#present + 1] = name end end
return { autocmds = a, timers = t, maps = maps, globals = present }
"#;

const INJECT_LUA: &str = "local src, opts = ... local f = assert(loadstring(src)) _G.__test_td = f(opts)";

pub struct Embed {
    child: Child,
    stdin: std::process::ChildStdin,
    next_id: u64,
    responses: mpsc::Receiver<(u64, Value, Value)>,
}

impl Embed {
    /// Runs `nvim --clean --embed --headless -n -i NONE <extra_args>` in `dir`, with `env` added and
    /// every XDG directory pointed under `dir`, so nothing of the user's reaches it.
    pub fn start(dir: &Path, extra_args: &[&str], env: &[(&str, &str)]) -> Embed {
        std::fs::create_dir_all(dir).expect("scratch dir");
        let mut command = Command::new("nvim");
        // Nothing of the outer environment may decide what the snippets do: an nvim started inside tmux
        // or inside another Eitri window inherits both, and the snippets must be shown not to care.
        for (name, _) in std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v))) {
            if name == "TMUX" || name == "TMUX_PANE" || name == "NVIM" || name.starts_with("EITRI_") {
                command.env_remove(name);
            }
        }
        command.args(["--clean", "--embed", "--headless", "-n", "-i", "NONE"]);
        command.args(extra_args);
        for (k, v) in env {
            command.env(k, v);
        }
        let mut child = command
            .current_dir(dir)
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("nvim must be on PATH for this test");
        let stdin = child.stdin.take().expect("nvim's stdin must be piped");
        let mut stdout = child.stdout.take().expect("nvim's stdout must be piped");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || loop {
            match rmpv::decode::read_value(&mut stdout) {
                Ok(Value::Array(items)) if items.len() == 4 && items[0].as_u64() == Some(1) => {
                    let id = items[1].as_u64().unwrap_or(u64::MAX);
                    if tx.send((id, items[2].clone(), items[3].clone())).is_err() {
                        break;
                    }
                }
                // Requests and notifications from nvim are not expected; dropping them keeps the
                // pipe from filling.
                Ok(_) => {}
                Err(_) => break,
            }
        });
        Embed {
            child,
            stdin,
            next_id: 1,
            responses: rx,
        }
    }

    /// Sends `method(params)` and waits for its answer, panicking on an nvim error or on silence.
    pub fn request(&mut self, method: &str, params: Vec<Value>) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let message = Value::Array(vec![
            Value::from(0),
            Value::from(id),
            Value::from(method),
            Value::Array(params),
        ]);
        rmpv::encode::write_value(&mut self.stdin, &message).expect("write to nvim's stdin");
        self.stdin.flush().expect("flush nvim's stdin");
        let deadline = Instant::now() + DEADLINE;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let (got_id, err, result) = self
                .responses
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("nvim did not answer {method} within {DEADLINE:?}"));
            if got_id == id {
                assert!(err.is_nil(), "nvim returned an error for {method}: {err:?}");
                return result;
            }
        }
    }

    pub fn lua(&mut self, code: &str, args: Vec<Value>) -> Value {
        self.request("nvim_exec_lua", vec![Value::from(code), Value::Array(args)])
    }

    pub fn input(&mut self, keys: &str) {
        self.request("nvim_input", vec![Value::from(keys)]);
    }

    pub fn command(&mut self, cmd: &str) {
        self.request("nvim_command", vec![Value::from(cmd)]);
    }

    pub fn eval(&mut self, expr: &str) -> Value {
        self.request("nvim_eval", vec![Value::from(expr)])
    }

    /// Polls `f` every 50 ms for up to 5 s and panics naming `what` if it never holds.
    pub fn wait_until(&mut self, what: &str, mut f: impl FnMut(&mut Embed) -> bool) {
        let deadline = Instant::now() + DEADLINE;
        while !f(self) {
            if Instant::now() >= deadline {
                panic!("timed out waiting for {what}");
            }
            std::thread::sleep(STEP);
        }
    }

    /// One `vim.schedule` round trip: everything scheduled before it has run once this returns.
    pub fn flush_scheduled(&mut self) {
        self.lua(
            "vim.g.embed_flushed = 0 vim.schedule(function() vim.g.embed_flushed = 1 end)",
            vec![],
        );
        self.wait_until("a scheduled callback", |nvim| {
            nvim.eval("g:embed_flushed").as_i64() == Some(1)
        });
    }

    pub fn footprint(&mut self, groups: &[&str]) -> Footprint {
        self.footprint_of(groups, &[])
    }

    /// [`Embed::footprint`], also reporting which of the global names `globals` are set.
    pub fn footprint_of(&mut self, groups: &[&str], globals: &[&str]) -> Footprint {
        let names = |list: &[&str]| Value::Array(list.iter().map(|g| Value::from(*g)).collect());
        let result = self.lua(FOOTPRINT_LUA, vec![names(groups), names(globals)]);
        let entries: HashMap<String, Value> = result
            .as_map()
            .expect("the footprint is a table")
            .iter()
            .map(|(k, v)| (k.as_str().unwrap_or_default().to_string(), v.clone()))
            .collect();
        // An empty Lua table may arrive as an empty map or an empty array.
        let strings = |name: &str| -> Vec<String> {
            entries[name]
                .as_array()
                .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        };
        Footprint {
            autocmds: entries["autocmds"].as_u64().expect("a count") as usize,
            active_timers: entries["timers"].as_u64().expect("a count") as usize,
            eitri_maps: strings("maps"),
            globals: strings("globals"),
        }
    }

    /// Loads `src` as a chunk, calls it with `opts` and keeps what it returns as the teardown.
    pub fn inject(&mut self, src: &str, opts: Value) {
        self.lua(INJECT_LUA, vec![Value::from(src), opts]);
    }

    /// Calls the teardown the last [`Embed::inject`] stored.
    pub fn teardown(&mut self) {
        self.lua("_G.__test_td()", vec![]);
    }

    /// `inject` and `teardown` in one request: nothing scheduled in between can run.
    pub fn inject_and_teardown(&mut self, src: &str, opts: Value) {
        self.lua(
            "local src, opts = ... local f = assert(loadstring(src)) f(opts)() ",
            vec![Value::from(src), opts],
        );
    }

    /// Quits nvim and reaps it.
    pub fn quit(mut self) {
        let _ = self.request("nvim_input", vec![Value::from("<Esc><Cmd>qa!<CR>")]);
        let deadline = Instant::now() + DEADLINE;
        while self.child.try_wait().expect("try_wait").is_none() {
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Embed {
    fn drop(&mut self) {
        // A test that panicked must not leave its nvim behind. Only this child, by handle.
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A table of string keys to values, as `nvim_exec_lua` takes it.
pub fn opts_map(entries: &[(&str, Value)]) -> Value {
    Value::Map(entries.iter().map(|(k, v)| (Value::from(*k), v.clone())).collect())
}
