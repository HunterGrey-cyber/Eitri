use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Mutex};
use std::time::Duration;

use rmpv::Value;

pub struct EmbedNvim {
    child: Child,
    input: Mutex<ChildStdin>,
    replies: mpsc::Receiver<Value>,
    reader: Option<std::thread::JoinHandle<()>>,
    dir: PathBuf,
}

impl EmbedNvim {
    pub fn manual_timers(&self) {
        self.exec_lua(include_str!("manual_timers.lua"), vec![]);
    }

    pub fn advance_timers(&self, ms: u64) {
        self.exec_lua("test_timers.advance(...)", vec![Value::from(ms)]);
    }

    pub fn start() -> Self {
        let dir = std::env::temp_dir().join(format!("es-{}", uuid::Uuid::new_v4().simple()));
        agent::private_fs::create_private_dir_all(&dir, &dir).unwrap();
        let mut command = Command::new("nvim");
        command
            .args(["--clean", "--headless", "--embed", "-n", "-i", "NONE"])
            .current_dir(&dir)
            .env("HOME", &dir)
            .env("NVIM_LOG_FILE", dir.join("nvim.log"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("NVIM")
            .env_remove("NVIM_LISTEN_ADDRESS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (name, _) in std::env::vars_os() {
            if name.to_str().is_some_and(|s| s.starts_with("EITRI_")) {
                command.env_remove(name);
            }
        }
        let mut child = command.spawn().expect("nvim must be on PATH");
        let input = Mutex::new(child.stdin.take().unwrap());
        let mut output = child.stdout.take().unwrap();
        let (send, replies) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            while let Ok(value) = rmpv::decode::read_value(&mut output) {
                if send.send(value).is_err() {
                    break;
                }
            }
        });
        let nvim = Self {
            child,
            input,
            replies,
            reader: Some(reader),
            dir,
        };
        assert_eq!(nvim.exec_lua("return vim.v.vim_did_enter", vec![]).as_i64(), Some(1));
        nvim
    }

    pub fn exec_lua(&self, code: &str, args: Vec<Value>) -> Value {
        self.try_exec_lua(code, args)
            .unwrap_or_else(|error| panic!("nvim Lua error: {error}"))
    }

    pub fn try_exec_lua(&self, code: &str, args: Vec<Value>) -> Result<Value, Value> {
        let request = Value::Array(vec![
            Value::from(0),
            Value::from(1),
            Value::from("nvim_exec_lua"),
            Value::Array(vec![Value::from(code), Value::Array(args)]),
        ]);
        let mut input = self.input.lock().map_err(|e| Value::from(e.to_string()))?;
        rmpv::encode::write_value(&mut *input, &request).map_err(|e| Value::from(e.to_string()))?;
        input.flush().map_err(|e| Value::from(e.to_string()))?;
        loop {
            let reply = self
                .replies
                .recv_timeout(Duration::from_secs(5))
                .map_err(|e| Value::from(format!("nvim must answer over stdio: {e}")))?;
            let items = reply.as_array().ok_or_else(|| Value::from("expected an RPC message"))?;
            if items.first().and_then(Value::as_u64) == Some(1) {
                if items.len() != 4 || items[1].as_u64() != Some(1) {
                    return Err(Value::from("unexpected RPC response"));
                }
                return if items[2].is_nil() {
                    Ok(items[3].clone())
                } else {
                    Err(items[2].clone())
                };
            }
        }
    }

    pub fn helper_channel(&self) -> u64 {
        self.exec_lua(
            "return vim.fn.jobstart({vim.v.progpath, '--clean', '--headless', '--embed', '-n', '-i', 'NONE'}, {rpc = true})",
            vec![],
        ).as_u64().filter(|id| *id > 0).expect("a helper RPC channel")
    }

    /// A closed channel has no `id`; nvim 0.10 answers it with a typed empty dictionary that `next()`
    /// does not see as empty.
    pub fn close_channel(&self, chan: u64) {
        assert_eq!(
            self.exec_lua("return vim.fn.jobstop(...)", vec![Value::from(chan)])
                .as_i64(),
            Some(1)
        );
        self.exec_lua("local chan = ...; assert(vim.wait(1000, function() return vim.api.nvim_get_chan_info(chan).id == nil end, 10))", vec![Value::from(chan)]);
    }
}

impl Drop for EmbedNvim {
    fn drop(&mut self) {
        // Stop helpers before their parent's pipes disappear, including after an assertion fails.
        let _ = self.try_exec_lua("for _, c in ipairs(vim.api.nvim_list_chans()) do if c.stream == 'job' then pcall(vim.fn.jobstop, c.id) end end", vec![]);
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
