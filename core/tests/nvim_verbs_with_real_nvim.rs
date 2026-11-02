//! The `:Eitri` snippet (`core/src/nvim_verbs/eitri_commands.lua`), driven by a real `nvim` over msgpack-RPC.
//!
//! `#[ignore]`d: it needs `nvim` on PATH. It spends no tokens, touches no network, opens no window and needs no
//! display -- `nvim --embed --headless` is the whole harness. The socket the snippet writes is a Unix listener the
//! test owns; `which-key` is a stand-in module on a scratch runtime path.
//!
//! "Sends nothing" is proven with a sentinel rather than a sleep: after a refused call a valid one is made, and the
//! first line to arrive has to be the valid one's.
//!
//! Run: `cargo test -p eitri-core --test nvim_verbs_with_real_nvim -- --ignored`

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eitri_core::layout::ModuleId;
use eitri_core::nvim_verbs::install::{install_args, to_msgpack, EITRI_COMMANDS_LUA};
use eitri_core::nvim_verbs::leader::LeaderKey;
use eitri_core::nvim_verbs::{completion_words, CommandWords};
use rmpv::Value;

const DEADLINE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(25);

/// The grammar error `:Eitri` shows for a word it will not send.
fn cannot_send(word: &str) -> String {
    format!(
        ":Eitri: \"{word}\" cannot be sent (letters, digits and . _ : = < > # - only, at most 64 each, at most 6 words)"
    )
}

struct Client {
    stdin: std::process::ChildStdin,
    next_id: u64,
    responses: mpsc::Receiver<(u64, Value, Value)>,
}

impl Client {
    fn attach(child: &mut Child) -> Self {
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
                Ok(_) => {}
                Err(_) => break,
            }
        });
        Self {
            stdin,
            next_id: 1,
            responses: rx,
        }
    }

    fn request(&mut self, method: &str, params: Vec<Value>) -> Result<Value, Value> {
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
        let (got_id, err, result) = self
            .responses
            .recv_timeout(DEADLINE)
            .unwrap_or_else(|_| panic!("nvim did not answer {method} within {DEADLINE:?}"));
        assert_eq!(got_id, id, "a response for a different request arrived");
        if err.is_nil() {
            Ok(result)
        } else {
            Err(err)
        }
    }
}

struct Nvim {
    child: Child,
    client: Client,
    listener: Option<UnixListener>,
    socket: PathBuf,
    scratch: PathBuf,
}

impl Drop for Nvim {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

/// A group of keys to install, as the host would hand it over.
fn key(suffix: &str, words: &[&str], desc: &str) -> LeaderKey {
    LeaderKey {
        suffix: suffix.to_string(),
        words: words.iter().map(|w| w.to_string()).collect(),
        desc: desc.to_string(),
    }
}

/// What the window hands the snippet: the real names, a short first-word list and the built-in modules.
fn words() -> CommandWords {
    let actions: Vec<String> = ["zoom", "tab.new"].map(str::to_string).into();
    let mut words = completion_words(&actions, &[ModuleId::editor(), ModuleId::agent(), ModuleId::terminal()]);
    words.first = ["zoom", "split", "focus", "open"].map(str::to_string).into();
    words
}

impl Nvim {
    /// `before` are `--cmd`s run before the user's config would be (a mapping or a leader in place when the
    /// snippet loads).
    fn start(case: &str, before: &[&str]) -> Self {
        let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("nvv-{}-{case}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(scratch.join("rtp/lua")).expect("scratch dir");
        std::fs::write(
            scratch.join("rtp/lua/which-key.lua"),
            "_G.__wk_calls = {}\nreturn { add = function(spec) table.insert(_G.__wk_calls, spec) end }\n",
        )
        .expect("the stand-in which-key");
        let socket = scratch.join("s");
        let listener = UnixListener::bind(&socket).expect("the test's listener");
        listener.set_nonblocking(true).expect("non-blocking");

        let mut command = Command::new("nvim");
        command.args(["--clean", "--embed", "--headless", "-n", "-i", "NONE"]);
        command.args(["--cmd", &format!("set rtp+={}", scratch.join("rtp").display())]);
        for cmd in before {
            command.args(["--cmd", cmd]);
        }
        let mut child = command
            .current_dir(&scratch)
            .env("XDG_STATE_HOME", scratch.join("state"))
            .env("XDG_DATA_HOME", scratch.join("data"))
            .env("XDG_CONFIG_HOME", scratch.join("config"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("nvim must be on PATH for this test");
        let client = Client::attach(&mut child);
        let mut nvim = Self {
            child,
            client,
            listener: Some(listener),
            socket,
            scratch,
        };
        nvim.wait_until("VimEnter", |nvim| nvim.eval("v:vim_did_enter").as_i64() == Some(1));
        nvim.lua(
            "_G.nv_notes = {} vim.notify = function(msg, level) table.insert(_G.nv_notes, { msg, level }) end",
            vec![],
        );
        nvim
    }

    fn lua(&mut self, code: &str, args: Vec<Value>) -> Value {
        self.client
            .request("nvim_exec_lua", vec![Value::from(code), Value::Array(args)])
            .unwrap_or_else(|err| panic!("nvim_exec_lua failed: {err:?}"))
    }

    fn eval(&mut self, expr: &str) -> Value {
        self.client
            .request("nvim_eval", vec![Value::from(expr)])
            .unwrap_or_else(|err| panic!("nvim_eval {expr:?} failed: {err:?}"))
    }

    fn command(&mut self, cmd: &str) {
        self.client
            .request("nvim_command", vec![Value::from(cmd)])
            .unwrap_or_else(|err| panic!(":{cmd} failed: {err:?}"));
    }

    fn input(&mut self, keys: &str) {
        self.client
            .request("nvim_input", vec![Value::from(keys)])
            .expect("nvim_input");
    }

    /// Runs the snippet the way the host does: its source and one argument table.
    fn install(&mut self, group: Option<(&str, &[LeaderKey])>) -> Value {
        let args = install_args(&self.socket, &words(), group);
        self.lua(
            "local src, opts = ... local f = assert(loadstring(src)) return f(opts)",
            vec![Value::from(EITRI_COMMANDS_LUA), to_msgpack(&args)],
        )
    }

    fn wait_until(&mut self, what: &str, mut ok: impl FnMut(&mut Self) -> bool) {
        let deadline = Instant::now() + DEADLINE;
        while !ok(self) {
            if Instant::now() >= deadline {
                panic!("timed out waiting for {what}");
            }
            std::thread::sleep(POLL);
        }
    }

    /// Everything scheduled before this call has run once it returns.
    fn flush_scheduled(&mut self) {
        self.lua(
            "vim.g.nv_flushed = 0 vim.schedule(function() vim.g.nv_flushed = 1 end)",
            vec![],
        );
        self.wait_until("a scheduled callback", |nvim| {
            nvim.eval("g:nv_flushed").as_i64() == Some(1)
        });
    }

    /// Fires a user event and lets the snippet's scheduled re-check run.
    fn fire(&mut self, pattern: &str) {
        self.lua(
            "vim.api.nvim_exec_autocmds('User', { pattern = ... })",
            vec![Value::from(pattern)],
        );
        self.flush_scheduled();
    }

    /// The first accepted connection's whole content, within [`DEADLINE`].
    fn wait_for_line(&mut self) -> String {
        let listener = self.listener.as_ref().expect("the listener is still there");
        let deadline = Instant::now() + DEADLINE;
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).expect("blocking mode");
                    stream.set_read_timeout(Some(DEADLINE)).expect("read timeout");
                    let mut line = String::new();
                    stream.read_to_string(&mut line).expect("read the line");
                    return line;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        panic!("no line arrived within {DEADLINE:?}");
                    }
                    std::thread::sleep(POLL);
                }
                Err(e) => panic!("accept failed: {e}"),
            }
        }
    }

    /// The messages `vim.notify` got, as `(text, level)`.
    fn notes(&mut self) -> Vec<(String, i64)> {
        let v = self.lua("return _G.nv_notes", vec![]);
        v.as_array()
            .expect("an array")
            .iter()
            .map(|note| {
                let note = note.as_array().expect("a pair");
                (
                    note[0].as_str().unwrap_or_default().to_string(),
                    note[1].as_i64().unwrap_or(-1),
                )
            })
            .collect()
    }

    /// `maparg(lhs, "n")`'s description, buffer-local first; `None` when nothing is mapped.
    fn map_desc(&mut self, lhs: &str) -> Option<String> {
        self.lua(
            "local m = vim.fn.maparg(..., 'n', false, true) return (m.desc ~= nil) and m.desc or (next(m) ~= nil and '' or vim.NIL)",
            vec![Value::from(lhs)],
        )
        .as_str()
        .map(str::to_string)
    }

    /// The descriptions of the global Normal-mode maps that begin with `Eitri: `.
    fn eitri_maps(&mut self) -> Vec<String> {
        let v = self.lua(
            "local out = {} for _, m in ipairs(vim.api.nvim_get_keymap('n')) do \
               if (m.desc or ''):find('^Eitri: ') then out[#out + 1] = m.lhs .. ' ' .. m.desc end end \
             return out",
            vec![],
        );
        v.as_array()
            .expect("an array")
            .iter()
            .map(|s| s.as_str().unwrap_or_default().to_string())
            .collect()
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn eitri_zoom_writes_the_line_to_the_socket() {
    let mut nvim = Nvim::start("zoom", &[]);
    nvim.install(None);
    nvim.command("Eitri zoom");
    assert_eq!(nvim.wait_for_line(), "W zoom\n");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_module_id_with_a_hash_travels_whole() {
    let mut nvim = Nvim::start("hash", &[]);
    nvim.install(None);
    nvim.command("Eitri focus terminal#2");
    assert_eq!(nvim.wait_for_line(), "W focus terminal#2\n");
    nvim.lua("__eitri_window.run('split', 'right', 'agent')", vec![]);
    assert_eq!(nvim.wait_for_line(), "W split right agent\n");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn words_the_host_would_drop_are_refused_in_nvim_and_nothing_is_sent() {
    let mut nvim = Nvim::start("bad", &[]);
    nvim.install(None);
    let sixty_five = "a".repeat(65);
    nvim.command(&format!("Eitri {sixty_five}"));
    nvim.command("Eitri workspace my\\ space");
    nvim.command("Eitri a b c d e f g");
    nvim.lua("__eitri_window.run('caf\\195\\169')", vec![]);
    nvim.command("Eitri zoom");
    assert_eq!(
        nvim.wait_for_line(),
        "W zoom\n",
        "the refused calls must have written nothing before the valid one"
    );
    let notes = nvim.notes();
    let error = 4; // vim.log.levels.ERROR
    assert_eq!(notes.len(), 4, "{notes:?}");
    assert_eq!(notes[0], (cannot_send(&sixty_five), error));
    assert_eq!(notes[1], (cannot_send("my space"), error));
    assert_eq!(notes[2], (cannot_send("g"), error), "the first word past the sixth");
    assert_eq!(notes[3], (cannot_send("caf\u{e9}"), error));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn completion_follows_the_words_already_typed() {
    let mut nvim = Nvim::start("complete", &[]);
    nvim.install(None);
    let mut complete = |line: &str| -> Vec<String> {
        let found = nvim.lua("return vim.fn.getcompletion(..., 'cmdline')", vec![Value::from(line)]);
        found
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    };
    assert_eq!(complete("Eitri "), ["zoom", "split", "focus", "open"]);
    assert_eq!(complete("Eitri fo"), ["focus"]);
    assert_eq!(complete("Eitri split "), ["right", "below"]);
    assert_eq!(complete("Eitri split r"), ["right"], "not an action starting with r");
    assert!(complete("Eitri split right ").contains(&"terminal#2".to_string()));
    let terminals = complete("Eitri focus terminal");
    assert_eq!(terminals[..2], ["terminal", "terminal#2"]);
    assert_eq!(complete("Eitri show ag")[0], "agent");
    assert_eq!(complete("Eitri open "), ["terminal", "agent"]);
    assert!(complete("Eitri focus agent ").is_empty(), "focus takes one module");
    assert!(complete("Eitri zoom ").is_empty());
    assert_eq!(
        complete("vertical Eitri split "),
        ["right", "below"],
        "a modifier before the name"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_first_word_the_host_has_no_reading_for_is_an_error_and_nothing_is_sent() {
    let mut nvim = Nvim::start("unknown", &[plugin_rtp().as_str()]);
    nvim.install(None);
    nvim.command("Eitri zoomm");
    nvim.command("Eitri settle");
    nvim.command("Eitri terminal");
    let sent = nvim.lua("return require('eitri').run('zoomm')", vec![]);
    assert_eq!(sent.as_bool(), Some(false));
    nvim.command("Eitri zoom");
    assert_eq!(
        nvim.wait_for_line(),
        "W zoom\n",
        "the refused calls must have written nothing before the valid one"
    );
    let not_known = |word: &str| {
        (
            format!(":Eitri: \"{word}\" is neither a verb nor an action (Tab lists them)"),
            4,
        )
    };
    assert_eq!(
        nvim.notes(),
        [
            not_known("zoomm"),
            not_known("settle"),
            not_known("terminal"),
            not_known("zoomm")
        ]
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn setup_is_a_verb_and_is_sent_for_the_host_to_open_the_first_start_screen() {
    let mut nvim = Nvim::start("setup-verb", &[]);
    nvim.install(None);
    nvim.command("Eitri setup");
    assert_eq!(nvim.wait_for_line(), "W setup\n");
    assert!(nvim.notes().is_empty(), "{:?}", nvim.notes());
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn what_only_the_host_can_judge_is_sent_for_the_host_to_refuse() {
    let mut nvim = Nvim::start("host-judges", &[]);
    nvim.install(None);
    for (command, line) in [
        ("Eitri tab.new", "W tab.new\n"),
        ("Eitri split right", "W split right\n"),
        ("Eitri instance.new", "W instance.new\n"),
        ("Eitri module.notes", "W module.notes\n"),
    ] {
        nvim.command(command);
        assert_eq!(nvim.wait_for_line(), line);
    }
    assert!(nvim.notes().is_empty(), "{:?}", nvim.notes());
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_global_eitri_command_of_the_users_is_kept_and_the_warning_logged_once() {
    let mut nvim = Nvim::start("global", &["command! Eitri let g:mine = 1"]);
    let answer = nvim.install(None);
    nvim.install(None);
    nvim.command("Eitri");
    assert_eq!(nvim.eval("g:mine").as_i64(), Some(1), "the user's command still runs");
    let skipped = answer
        .as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_str() == Some("skipped"))
        .unwrap();
    assert_eq!(skipped.1.as_array().unwrap().len(), 1);
    let warning = "[nvim] :Eitri is already defined; not replacing it";
    let warn = 3; // vim.log.levels.WARN
    assert_eq!(nvim.notes().first(), Some(&(warning.to_string(), warn)));
    assert_eq!(
        nvim.eval("get(g:, 'eitri', 0)").as_i64(),
        Some(1),
        "the rest of the snippet still runs"
    );
    let run = nvim.lua("return type(__eitri_window.run)", vec![]);
    assert_eq!(run.as_str(), Some("function"));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_buffer_local_eitri_command_of_the_users_is_kept() {
    let mut nvim = Nvim::start("buflocal", &[]);
    nvim.command("command! -buffer Eitri let g:mine = 2");
    nvim.install(None);
    nvim.command("Eitri");
    assert_eq!(nvim.eval("g:mine").as_i64(), Some(2));
    let global = nvim.lua(
        "return vim.api.nvim_get_commands({ builtin = false }).Eitri == nil",
        vec![],
    );
    assert_eq!(global.as_bool(), Some(true), "no global :Eitri beside theirs");
    assert_eq!(nvim.notes().len(), 1);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn vim_g_eitri_and_eitri_ready_arrive_once_after_the_install() {
    let mut nvim = Nvim::start("ready", &[]);
    nvim.lua(
        "vim.api.nvim_create_autocmd('User', { pattern = 'EitriReady', callback = function() \
           vim.g.ready_n = (vim.g.ready_n or 0) + 1 vim.g.ready_saw_command = vim.fn.exists(':Eitri') == 2 and 1 or 0 end })",
        vec![],
    );
    assert_eq!(nvim.eval("exists('g:eitri')").as_i64(), Some(0), "not before");
    assert_eq!(nvim.eval("exists('g:ready_n')").as_i64(), Some(0), "not before");
    nvim.install(None);
    nvim.install(None);
    assert_eq!(nvim.eval("g:eitri").as_i64(), Some(1));
    assert_eq!(nvim.eval("g:ready_n").as_i64(), Some(1));
    assert_eq!(
        nvim.eval("g:ready_saw_command").as_i64(),
        Some(1),
        "the command exists by then"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn the_group_goes_into_empty_slots_and_its_keys_send_their_words() {
    let mut nvim = Nvim::start("group", &["let g:mapleader = ' '"]);
    let keys = [
        key("z", &["zoom"], "zoom"),
        key("%a", &["split", "right", "agent"], "split right agent"),
    ];
    nvim.install(Some(("<leader>W", &keys)));
    assert_eq!(nvim.map_desc("<leader>Wz").as_deref(), Some("Eitri: zoom"));
    assert_eq!(
        nvim.map_desc("<leader>W%a").as_deref(),
        Some("Eitri: split right agent")
    );
    assert!(nvim.notes().is_empty(), "{:?}", nvim.notes());
    nvim.input(" Wz");
    assert_eq!(nvim.wait_for_line(), "W zoom\n");
    nvim.input(" W%a");
    assert_eq!(nvim.wait_for_line(), "W split right agent\n");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn the_group_is_refused_whole_when_its_prefix_is_mapped() {
    // LazyVim's shape: `<leader>w` is a window group of the user's.
    let mut nvim = Nvim::start("lazy", &["let g:mapleader = ' '", "nnoremap <leader>w <C-w>"]);
    let keys = [key("z", &["zoom"], "zoom")];
    nvim.install(Some(("<leader>w", &keys)));
    assert!(nvim.eitri_maps().is_empty());
    assert_eq!(
        nvim.notes(),
        [(
            "keymap.nvim_prefix \"<leader>w\": <Space>w is already mapped (<C-W>); choose another".to_string(),
            3
        )]
    );
    nvim.input(" wz");
    nvim.command("Eitri zoom");
    assert_eq!(
        nvim.wait_for_line(),
        "W zoom\n",
        "the key sent nothing, the command still works"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_map_of_the_users_under_the_prefix_refuses_the_group_global_or_buffer_local() {
    let mut nvim = Nvim::start("under", &["let g:mapleader = ' '", "nnoremap <leader>Wq :echo 1<CR>"]);
    let keys = [key("z", &["zoom"], "zoom")];
    nvim.install(Some(("<leader>W", &keys)));
    assert!(nvim.eitri_maps().is_empty());
    assert_eq!(nvim.notes().len(), 1);
    assert!(
        nvim.notes()[0].0.contains("<Space>Wq is already mapped (:echo 1<CR>)"),
        "{:?}",
        nvim.notes()
    );

    let mut nvim = Nvim::start("under2", &["let g:mapleader = ' '"]);
    nvim.command("nnoremap <buffer> <leader>Wb :echo 2<CR>");
    nvim.install(Some(("<leader>W", &keys)));
    assert!(
        nvim.eitri_maps().is_empty(),
        "a buffer-local map under the prefix counts"
    );
    assert_eq!(nvim.notes().len(), 1);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn the_leader_spelled_as_space_or_as_a_blank_is_one_key() {
    let mut nvim = Nvim::start("spell1", &["let g:mapleader = ' '", "nnoremap <Space>Wq :echo 1<CR>"]);
    nvim.install(Some(("<leader>W", &[key("z", &["zoom"], "zoom")])));
    assert!(nvim.eitri_maps().is_empty(), "<leader>W is <Space>W");
    assert_eq!(nvim.notes().len(), 1);

    let mut nvim = Nvim::start("spell2", &["let g:mapleader = ' '", "nnoremap <leader>Wq :echo 1<CR>"]);
    nvim.install(Some(("<Space>W", &[key("z", &["zoom"], "zoom")])));
    assert!(nvim.eitri_maps().is_empty(), "<Space>W is <leader>W");
    assert_eq!(nvim.notes().len(), 1);

    let mut nvim = Nvim::start("spell3", &["let g:mapleader = '<Space>'"]);
    nvim.install(Some(("<leader>W", &[key("z", &["zoom"], "zoom")])));
    nvim.input(" Wz");
    assert_eq!(nvim.wait_for_line(), "W zoom\n");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_recheck_with_nothing_new_keeps_the_group() {
    let mut nvim = Nvim::start("recheck", &["let g:mapleader = ' '"]);
    let keys = [key("z", &["zoom"], "zoom"), key("o", &["layout.even"], "even")];
    nvim.install(Some(("<leader>W", &keys)));
    let before = nvim.eitri_maps();
    assert_eq!(before.len(), 2);
    nvim.fire("VeryLazy");
    nvim.fire("LazyLoad");
    assert_eq!(nvim.eitri_maps(), before, "the group's own maps are no collision");
    assert!(nvim.notes().is_empty());
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_prefix_the_user_maps_after_the_install_removes_the_group_and_is_logged_once() {
    let mut nvim = Nvim::start("late", &["let g:mapleader = ' '"]);
    let keys = [key("z", &["zoom"], "zoom"), key("o", &["layout.even"], "even")];
    nvim.install(Some(("<leader>w", &keys)));
    assert_eq!(nvim.eitri_maps().len(), 2);
    // LazyVim maps `<leader>w` at `User VeryLazy`, after the install.
    nvim.lua(
        "vim.api.nvim_create_autocmd('User', { pattern = 'VeryLazy', callback = function() \
           vim.keymap.set('n', '<leader>w', '<C-w>', { desc = 'Windows' }) end })",
        vec![],
    );
    nvim.fire("VeryLazy");
    assert!(nvim.eitri_maps().is_empty(), "{:?}", nvim.eitri_maps());
    assert_eq!(
        nvim.map_desc("<leader>w").as_deref(),
        Some("Windows"),
        "the user's map is not touched"
    );
    nvim.fire("VeryLazy");
    nvim.fire("LazyLoad");
    assert_eq!(
        nvim.notes(),
        [(
            "keymap.nvim_prefix \"<leader>w\": <Space>w is already mapped (Windows); choose another".to_string(),
            3
        )]
    );
    assert!(nvim.eitri_maps().is_empty(), "nothing is installed again");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_plugin_that_maps_one_of_the_keys_later_takes_that_key_and_the_rest_stand() {
    let mut nvim = Nvim::start("plugin", &["let g:mapleader = ' '"]);
    let keys = [key("z", &["zoom"], "zoom"), key("o", &["layout.even"], "even")];
    nvim.install(Some(("<leader>W", &keys)));
    nvim.lua(
        "vim.keymap.set('n', '<leader>Wz', function() end, { desc = 'plugin zoom' })",
        vec![],
    );
    nvim.fire("VeryLazy");
    assert_eq!(nvim.map_desc("<leader>Wz").as_deref(), Some("plugin zoom"));
    nvim.fire("VeryLazy");
    nvim.fire("LazyLoad");
    assert_eq!(
        nvim.map_desc("<leader>Wz").as_deref(),
        Some("plugin zoom"),
        "never mapped over again"
    );
    assert_eq!(
        nvim.map_desc("<leader>Wo").as_deref(),
        Some("Eitri: even"),
        "the keys nobody took stay"
    );
    assert!(nvim.notes().is_empty(), "{:?}", nvim.notes());
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_buffer_local_map_on_one_key_hides_it_in_that_buffer_only() {
    let mut nvim = Nvim::start("shadow", &["let g:mapleader = ' '"]);
    let keys = [key("z", &["zoom"], "zoom"), key("o", &["layout.even"], "even")];
    nvim.install(Some(("<leader>W", &keys)));
    nvim.command("nnoremap <buffer> <leader>Wz :echo 1<CR>");
    nvim.fire("VeryLazy");
    assert_eq!(nvim.eitri_maps().len(), 2, "{:?}", nvim.eitri_maps());
    assert!(nvim.notes().is_empty(), "{:?}", nvim.notes());
    nvim.command("nunmap <buffer> <leader>Wz");
    nvim.input(" Wz");
    assert_eq!(
        nvim.wait_for_line(),
        "W zoom\n",
        "the group's key comes back with the map gone"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_late_collision_removes_the_global_keys_a_buffer_local_map_was_hiding() {
    let mut nvim = Nvim::start("shadow-gone", &["let g:mapleader = ' '"]);
    let keys = [key("z", &["zoom"], "zoom"), key("o", &["layout.even"], "even")];
    nvim.install(Some(("<leader>W", &keys)));
    nvim.command("nnoremap <buffer> <leader>Wz :echo 1<CR>");
    nvim.command("nnoremap <leader>Wq :echo 2<CR>");
    nvim.fire("VeryLazy");
    assert!(nvim.eitri_maps().is_empty(), "{:?}", nvim.eitri_maps());
    assert_eq!(
        nvim.eval("maparg('<leader>Wz', 'n')").as_str(),
        Some(":echo 1<CR>"),
        "the buffer's own map is not touched"
    );
    assert_eq!(nvim.notes().len(), 1, "{:?}", nvim.notes());
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn which_key_hears_of_the_group_only_when_it_is_loaded() {
    let mut nvim = Nvim::start("wk-no", &["let g:mapleader = ' '"]);
    nvim.install(Some(("<leader>W", &[key("z", &["zoom"], "zoom")])));
    assert_eq!(
        nvim.lua("return package.loaded['which-key'] == nil", vec![]).as_bool(),
        Some(true)
    );

    let mut nvim = Nvim::start("wk-yes", &["let g:mapleader = ' '"]);
    nvim.lua("require('which-key')", vec![]);
    nvim.install(Some(("<leader>W", &[key("z", &["zoom"], "zoom")])));
    nvim.fire("VeryLazy");
    let calls = nvim.lua("return vim.json.encode(_G.__wk_calls)", vec![]);
    let calls = calls.as_str().unwrap().to_string();
    assert!(
        calls.starts_with("[[") && calls.contains("\"Eitri window\"") && calls.contains("<leader>W"),
        "{calls}"
    );
    assert_eq!(calls.matches("Eitri window").count(), 1, "told once: {calls}");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_socket_that_is_gone_costs_the_call_and_never_an_error() {
    let mut nvim = Nvim::start("gone", &["let g:mapleader = ' '"]);
    nvim.install(Some(("<leader>W", &[key("z", &["zoom"], "zoom")])));
    nvim.listener = None;
    std::fs::remove_file(&nvim.socket).expect("remove the socket");
    nvim.command("Eitri zoom");
    nvim.input(" Wz");
    nvim.flush_scheduled();
    assert_eq!(nvim.eval("v:errmsg").as_str(), Some(""));
    assert!(nvim.notes().is_empty(), "{:?}", nvim.notes());
}

/// eitri.nvim's directory, put on the editor's runtime path the way a plugin manager would.
fn plugin_rtp() -> String {
    format!(
        "set rtp+={}",
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../nvim/eitri.nvim")
            .display()
    )
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn edge_writes_the_letter_of_the_direction() {
    let mut nvim = Nvim::start("edge", &[]);
    nvim.install(None);
    for (word, letter) in [("right", "R\n"), ("left", "L\n"), ("down", "D\n"), ("up", "U\n")] {
        let sent = nvim.lua("return __eitri_window.edge(...)", vec![Value::from(word)]);
        assert_eq!(sent.as_bool(), Some(true), "{word}");
        assert_eq!(nvim.wait_for_line(), letter, "{word}");
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn edge_takes_only_the_four_direction_words_and_sends_nothing_for_others() {
    let mut nvim = Nvim::start("edge-bad", &[]);
    nvim.install(None);
    for bad in ["l", "Right", "", "zoom", "R"] {
        let sent = nvim.lua("return __eitri_window.edge(...)", vec![Value::from(bad)]);
        assert_eq!(sent.as_bool(), Some(false), "{bad:?}");
    }
    let sent = nvim.lua("return __eitri_window.edge(nil)", vec![]);
    assert_eq!(sent.as_bool(), Some(false));
    nvim.lua("__eitri_window.edge('up')", vec![]);
    assert_eq!(
        nvim.wait_for_line(),
        "U\n",
        "the refused calls must have written nothing before the valid one"
    );
    assert!(nvim.notes().is_empty(), "a refused word is quiet: {:?}", nvim.notes());
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn eitri_nvim_run_and_edge_are_false_where_there_is_no_window() {
    let mut nvim = Nvim::start("plugin-none", &[plugin_rtp().as_str()]);
    let run = nvim.lua("return require('eitri').run('zoom')", vec![]);
    assert_eq!(run.as_bool(), Some(false));
    let edge = nvim.lua("return require('eitri').edge('right')", vec![]);
    assert_eq!(edge.as_bool(), Some(false));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn eitri_nvim_run_and_edge_reach_the_window() {
    let mut nvim = Nvim::start("plugin-window", &[plugin_rtp().as_str()]);
    nvim.install(None);
    let run = nvim.lua("return require('eitri').run('split', 'right', 'agent')", vec![]);
    assert_eq!(run.as_bool(), Some(true));
    assert_eq!(nvim.wait_for_line(), "W split right agent\n");
    let edge = nvim.lua("return require('eitri').edge('right')", vec![]);
    assert_eq!(edge.as_bool(), Some(true));
    assert_eq!(nvim.wait_for_line(), "R\n");
    let edge = nvim.lua("return require('eitri').edge('sideways')", vec![]);
    assert_eq!(edge.as_bool(), Some(false));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn eitri_nvim_edge_prefers_the_companions_table_and_falls_back_to_the_window() {
    let mut nvim = Nvim::start("plugin-both", &[plugin_rtp().as_str()]);
    nvim.install(None);
    nvim.lua(
        "_G.__eitri_companion = { edge = function(d) _G.nv_companion_got = d return true end }",
        vec![],
    );
    let edge = nvim.lua("return require('eitri').edge('left')", vec![]);
    assert_eq!(edge.as_bool(), Some(true));
    assert_eq!(nvim.lua("return _G.nv_companion_got", vec![]).as_str(), Some("left"));
    // The companion took it, so the window wrote nothing: the sentinel is the first line.
    nvim.lua("require('eitri').run('zoom')", vec![]);
    assert_eq!(nvim.wait_for_line(), "W zoom\n");

    nvim.lua("_G.__eitri_companion.edge = function() return false end", vec![]);
    let edge = nvim.lua("return require('eitri').edge('down')", vec![]);
    assert_eq!(
        edge.as_bool(),
        Some(true),
        "a table that does not take the move leaves it to the window"
    );
    assert_eq!(nvim.wait_for_line(), "D\n");
}
