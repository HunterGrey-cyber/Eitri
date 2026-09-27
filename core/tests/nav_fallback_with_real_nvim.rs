//! The nav fallback (`core/src/nav_fallback.lua`, spec `2026-09-27-v1-ui-design.md` §5, P1/P14),
//! driven by a real `nvim` over msgpack-RPC.
//!
//! `#[ignore]`d: it needs `nvim` on PATH. It spends no tokens, touches no network, opens no window
//! and needs no display -- `nvim --embed --headless` is the whole harness.
//!
//! RPC rather than `-c` commands, because Visual mode is the point of P14: `:normal` and
//! `feedkeys(…, 'x')` end an unfinished Visual selection, while `nvim_input` leaves it exactly as a
//! typed key would. The channel is the real [`PaneSwitchChannel`] (its `nvim_args()`, its
//! `child_env()`, its bound listener), so what this drives is what `shell` hands the editor pane.
//!
//! "Writes nothing" is proven with a sentinel rather than a sleep: after a move that must not leave,
//! a second key that must leave is pressed, and the first letter to arrive has to be the second
//! key's. The fallback's connect() is issued inside the mapping, so a stray letter from the first
//! key would be queued on the listener ahead of it.
//!
//! Run: `cargo test -p neovibe-core --test nav_fallback_with_real_nvim -- --ignored`

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use neovibe_core::pane_switch::PaneSwitchChannel;
use rmpv::Value;

const DEADLINE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(25);

/// The fallback's `desc` for each direction (spec §5.2's recognition key).
const KEYS: [(&str, &str, &str); 4] = [
    ("<C-h>", "left", "L"),
    ("<C-j>", "down", "D"),
    ("<C-k>", "up", "U"),
    ("<C-l>", "right", "R"),
];

fn fallback_desc(name: &str) -> String {
    format!("neovibe: window or pane {name}")
}

/// A msgpack-RPC client over `nvim --embed`'s stdio -- the same ~60 lines as
/// `editor_quit_with_real_nvim.rs`'s, which is the only other test here that needs one.
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

    fn request(&mut self, method: &str, params: Vec<Value>) -> Value {
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
        assert!(err.is_nil(), "nvim returned an error for {method}: {err:?}");
        result
    }
}

/// What the loader gets on the nvim child.
enum Loader {
    /// `channel.child_env()` whole: the product shape.
    Installed,
    /// `NEOVIBE_NAV_LUA` absent from the env (spec step (g)).
    NavLuaUnset,
    /// `NEOVIBE_NAV_LUA` present but empty.
    NavLuaEmpty,
    /// `NEOVIBE_NAV_LUA` set but `NEOVIBE_PANE_SWITCH_SOCKET` absent: the Lua itself must stand down.
    SocketUnset,
}

struct Nvim {
    child: Child,
    client: Client,
    listener: UnixListener,
    channel: PaneSwitchChannel,
    scratch: PathBuf,
}

impl Nvim {
    /// `before`/`after` are `--cmd`s placed before and after the loader's own, so a mapping can be
    /// in place when the snippet loads, and an autocommand can be defined after the snippet's.
    fn start(case: &str, loader: Loader, before: &[&str], after: &[&str]) -> Self {
        // Under the target directory, never `/tmp`; nothing here is a socket, so no length cap.
        let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("nav-{}-{case}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).expect("scratch dir");

        // The shim binary is only the fake `tmux` symlink's target, which nothing here runs.
        let mut channel =
            PaneSwitchChannel::bind(Path::new("/nonexistent/neovibe-tmux-shim")).expect("the channel must bind");
        let listener = channel.take_listener().expect("a fresh channel has its listener");

        let mut command = Command::new("nvim");
        command.args(["--clean", "--embed", "--headless", "-n", "-i", "NONE"]);
        for cmd in before {
            command.args(["--cmd", cmd]);
        }
        command.args(channel.nvim_args());
        for cmd in after {
            command.args(["--cmd", cmd]);
        }
        command.env_remove("NEOVIBE_NAV_LUA");
        command.env_remove("NEOVIBE_PANE_SWITCH_SOCKET");
        for (k, v) in channel.child_env() {
            let skip = match loader {
                Loader::Installed => false,
                Loader::NavLuaUnset | Loader::NavLuaEmpty => k == "NEOVIBE_NAV_LUA",
                Loader::SocketUnset => k == "NEOVIBE_PANE_SWITCH_SOCKET",
            };
            if !skip {
                command.env(k, v);
            }
        }
        if let Loader::NavLuaEmpty = loader {
            command.env("NEOVIBE_NAV_LUA", "");
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
        Self {
            child,
            client,
            listener,
            channel,
            scratch,
        }
    }

    fn lua(&mut self, code: &str, args: Vec<Value>) -> Value {
        self.client
            .request("nvim_exec_lua", vec![Value::from(code), Value::Array(args)])
    }

    fn eval(&mut self, expr: &str) -> Value {
        self.client.request("nvim_eval", vec![Value::from(expr)])
    }

    fn input(&mut self, keys: &str) {
        self.client.request("nvim_input", vec![Value::from(keys)]);
    }

    fn command(&mut self, cmd: &str) {
        self.client.request("nvim_command", vec![Value::from(cmd)]);
    }

    /// `maparg(lhs, mode, 0, 1)`'s `desc`, `rhs`, `buffer` and whether it has a callback: what a
    /// key does in the current buffer, buffer-local first.
    fn maparg(&mut self, lhs: &str, mode: &str) -> (Option<String>, Option<String>, i64, bool) {
        let v = self.lua(
            "local lhs, mode = ... \
             local m = vim.fn.maparg(lhs, mode, false, true) \
             return { m.desc or vim.NIL, m.rhs or vim.NIL, m.buffer or 0, m.callback ~= nil }",
            vec![Value::from(lhs), Value::from(mode)],
        );
        let items = v.as_array().expect("an array").clone();
        (
            items[0].as_str().map(str::to_string),
            items[1].as_str().map(str::to_string),
            items[2].as_i64().unwrap_or(0),
            items[3].as_bool().unwrap_or(false),
        )
    }

    /// The *global* mapping's `desc` (`nvim_get_keymap`), whatever a buffer-local one shadows it with.
    fn global_desc(&mut self, lhs: &str, mode: &str) -> Option<String> {
        self.lua(
            "local lhs, mode = ... \
             for _, m in ipairs(vim.api.nvim_get_keymap(mode)) do \
               if m.lhs:lower() == lhs:lower() then return m.desc or vim.NIL end \
             end \
             return vim.NIL",
            vec![Value::from(lhs), Value::from(mode)],
        )
        .as_str()
        .map(str::to_string)
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

    fn wait_for_fallback(&mut self, lhs: &str, mode: &str, name: &str) {
        let want = fallback_desc(name);
        self.wait_until(&format!("the fallback on {mode} {lhs}"), |nvim| {
            nvim.global_desc(lhs, mode).as_deref() == Some(want.as_str())
        });
    }

    fn wait_for_all_fallbacks(&mut self, mode: &str) {
        for (lhs, name, _) in KEYS {
            self.wait_for_fallback(lhs, mode, name);
        }
    }

    /// Blocks (up to [`DEADLINE`]) for one accepted connection and returns what it carried.
    fn wait_for_letter(&mut self) -> String {
        self.listener.set_nonblocking(true).expect("non-blocking");
        let deadline = Instant::now() + DEADLINE;
        loop {
            match self.listener.accept() {
                Ok((mut stream, _addr)) => {
                    stream.set_nonblocking(false).expect("blocking mode");
                    stream.set_read_timeout(Some(DEADLINE)).expect("read timeout");
                    let mut line = String::new();
                    stream.read_to_string(&mut line).expect("read the letter");
                    return line;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        panic!("no letter arrived within {DEADLINE:?}");
                    }
                    std::thread::sleep(POLL);
                }
                Err(e) => panic!("accept failed: {e}"),
            }
        }
    }

    fn mode(&mut self) -> String {
        let result = self.client.request("nvim_get_mode", vec![]);
        result
            .as_map()
            .and_then(|entries| entries.iter().find(|(k, _)| k.as_str() == Some("mode")))
            .and_then(|(_, v)| v.as_str())
            .unwrap_or_default()
            .to_string()
    }

    fn winnr(&mut self) -> i64 {
        self.eval("winnr()").as_i64().expect("winnr() is a number")
    }

    /// Runs one `vim.schedule` round trip: everything scheduled before it has run once it returns.
    fn flush_scheduled(&mut self) {
        self.lua(
            "vim.g.nv_flushed = 0 vim.schedule(function() vim.g.nv_flushed = 1 end)",
            vec![],
        );
        self.wait_until("a scheduled callback", |nvim| {
            nvim.eval("g:nv_flushed").as_i64() == Some(1)
        });
    }

    fn quit(mut self) {
        let _ = self
            .client
            .request("nvim_input", vec![Value::from("<Esc><Cmd>qa!<CR>")]);
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                assert!(status.success(), "nvim exited with {status}");
                break;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("nvim did not exit within {DEADLINE:?}");
            }
            std::thread::sleep(POLL);
        }
        self.channel.cleanup();
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

/// (a) No mapping of the user's: after `VimEnter`, all four keys carry the fallback in Normal and
/// Visual mode. `--clean` leaves nvim's own default `<C-L>` (`:h CTRL-L-default`) in Normal mode,
/// which counts as an empty slot -- otherwise a plain nvim could never leave by `Ctrl+l`.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn with_no_mapping_all_four_keys_get_the_fallback_in_normal_and_visual() {
    let mut nvim = Nvim::start("a", Loader::Installed, &[], &[]);
    nvim.wait_for_all_fallbacks("n");
    nvim.wait_for_all_fallbacks("x");
    for (lhs, name, _) in KEYS {
        for mode in ["n", "x"] {
            let (desc, _, buffer, callback) = nvim.maparg(lhs, mode);
            assert_eq!(desc.as_deref(), Some(fallback_desc(name).as_str()), "{mode} {lhs}");
            assert_eq!((buffer, callback), (0, true), "{mode} {lhs}");
        }
    }
    // The snippet set no option: `mapleader` and `timeoutlen` are nvim's own.
    assert!(nvim.eval("get(g:, 'mapleader', v:null)").is_nil());
    assert_eq!(nvim.eval("&timeoutlen").as_i64(), Some(1000));
    nvim.quit();
}

/// (b) Plain window moves -- in every spelling spec §5.1 lists, and stock LazyVim's own
/// `map("n", "<C-h>", "<C-w>h", { remap = true })` -- are replaced, both when they are in place
/// before the snippet loads and when an autocommand defined *after* the snippet's sets them again on
/// `User VeryLazy` (LazyVim's keymaps.lua does exactly that).
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn plain_window_moves_are_replaced_at_vimenter_and_after_verylazy() {
    let mut nvim = Nvim::start(
        "b",
        Loader::Installed,
        &["nnoremap <C-l> <C-w>l"],
        &[
            "autocmd User VeryLazy nnoremap <C-l> <C-w>l",
            "autocmd User VeryLazy lua vim.keymap.set('n', '<C-h>', '<C-w>h', \
             { desc = 'Go to Left Window', remap = true })",
            "autocmd User VeryLazy nnoremap <C-j> <Cmd>wincmd j<CR>",
            "autocmd User VeryLazy nnoremap <C-k> :wincmd k<CR>",
        ],
    );
    nvim.wait_for_all_fallbacks("n");

    // The re-set really happens after the snippet's own `User VeryLazy` handler: read synchronously,
    // before the scheduled check can run, the four slots hold the plain moves again.
    let rhs = nvim.lua(
        "vim.api.nvim_exec_autocmds('User', { pattern = 'VeryLazy' }) \
         local out = {} \
         for _, k in ipairs({ '<C-h>', '<C-j>', '<C-k>', '<C-l>' }) do \
           out[#out + 1] = vim.fn.maparg(k, 'n', false, true).rhs \
         end \
         return out",
        vec![],
    );
    let rhs: Vec<&str> = rhs.as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(rhs, ["<C-w>h", "<Cmd>wincmd j<CR>", ":wincmd k<CR>", "<C-w>l"]);

    nvim.wait_for_all_fallbacks("n");
    nvim.wait_for_all_fallbacks("x");
    nvim.quit();
}

/// (c) vim-tmux-navigator's own mapping, a Lua-callback mapping (a lazy.nvim key stub and
/// smart-splits both have this shape), and `<C-W>K` -- which moves the *window*, not the cursor, so
/// it is not a plain move even though only the letter's case differs -- are left alone in Normal
/// mode, across a later `User LazyLoad` too. Visual mode, which none of them maps, gets the fallback.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_users_own_mappings_are_left_alone_and_visual_mode_still_gets_the_fallback() {
    let mut nvim = Nvim::start(
        "c",
        Loader::Installed,
        &[
            "nnoremap <silent> <C-l> :<C-U>TmuxNavigateRight<CR>",
            "lua vim.keymap.set('n', '<C-h>', function() end, { desc = 'a lazy.nvim key stub' })",
            "nnoremap <C-k> <C-w>K",
        ],
        &[],
    );
    nvim.wait_for_all_fallbacks("x");
    nvim.wait_for_fallback("<C-j>", "n", "down");

    let untouched = |nvim: &mut Nvim| {
        assert_eq!(
            nvim.maparg("<C-l>", "n"),
            (None, Some(":<C-U>TmuxNavigateRight<CR>".to_string()), 0, false)
        );
        assert_eq!(
            nvim.maparg("<C-h>", "n"),
            (Some("a lazy.nvim key stub".to_string()), None, 0, true)
        );
        assert_eq!(nvim.maparg("<C-k>", "n"), (None, Some("<C-w>K".to_string()), 0, false));
    };
    untouched(&mut nvim);

    // A second check, on `User LazyLoad`: drop one Visual fallback so its return proves the check
    // ran, then the Normal-mode mappings must still be the user's.
    nvim.command("xunmap <C-l>");
    nvim.command("doautocmd User LazyLoad");
    nvim.wait_for_fallback("<C-l>", "x", "right");
    untouched(&mut nvim);
    nvim.quit();
}

/// (c, buffer-local) A buffer-local `<C-l>` (netrw has one) is never touched and keeps winning in
/// its buffer; the global slot beneath it -- here nvim's default -- still gets the fallback.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_buffer_local_mapping_is_left_alone() {
    let mut nvim = Nvim::start(
        "c-buffer",
        Loader::Installed,
        &["autocmd VimEnter * nnoremap <buffer> <C-l> :echo 'netrw-like'<CR>"],
        &[],
    );
    nvim.wait_for_fallback("<C-l>", "n", "right");
    assert_eq!(
        nvim.maparg("<C-l>", "n"),
        (None, Some(":echo 'netrw-like'<CR>".to_string()), 1, false)
    );
    nvim.quit();
}

/// (d) One window: each key in Normal mode is at nvim's edge, so it writes its letter.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn in_one_window_each_key_writes_its_letter() {
    let mut nvim = Nvim::start("d", Loader::Installed, &[], &[]);
    nvim.wait_for_all_fallbacks("n");
    for (lhs, _, letter) in KEYS {
        nvim.input(lhs);
        assert_eq!(nvim.wait_for_letter(), format!("{letter}\n"), "{lhs}");
    }
    assert_eq!(nvim.mode(), "n");
    nvim.quit();
}

/// (e) With a split, `<C-l>` from the left window moves to the right one and writes nothing; the
/// sentinel `<C-k>` (the top edge) is then the first letter to arrive.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_move_between_windows_writes_nothing() {
    let mut nvim = Nvim::start("e", Loader::Installed, &[], &[]);
    nvim.wait_for_all_fallbacks("n");
    nvim.command("vsplit");
    assert_eq!(nvim.winnr(), 1);
    nvim.input("<C-l>");
    nvim.wait_until("the cursor in the right window", |nvim| nvim.winnr() == 2);
    nvim.input("<C-k>");
    assert_eq!(nvim.wait_for_letter(), "U\n", "the move must not have written a letter");
    assert_eq!(nvim.winnr(), 2);
    nvim.quit();
}

/// (f) P14. Visual mode at nvim's edge writes the letter and stays in Visual mode with the
/// selection intact (charwise and linewise); not at the edge, it leaves Visual mode and moves,
/// writing nothing.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn visual_mode_leaves_at_the_edge_and_keeps_its_selection() {
    let mut nvim = Nvim::start("f", Loader::Installed, &[], &[]);
    nvim.wait_for_all_fallbacks("x");
    nvim.lua("vim.fn.setline(1, { 'one', 'two', 'three', 'four' })", vec![]);

    nvim.input("vj<C-l>");
    assert_eq!(nvim.wait_for_letter(), "R\n");
    assert_eq!(nvim.mode(), "v");
    assert_eq!(
        nvim.eval("[line('v'), line('.')]"),
        Value::from(vec![Value::from(1), Value::from(2)])
    );

    nvim.input("<Esc>ggVj<C-h>");
    assert_eq!(nvim.wait_for_letter(), "L\n");
    assert_eq!(nvim.mode(), "V");
    assert_eq!(
        nvim.eval("[line('v'), line('.')]"),
        Value::from(vec![Value::from(1), Value::from(2)])
    );

    nvim.input("<Esc>");
    nvim.wait_until("Normal mode", |nvim| nvim.mode() == "n");
    nvim.command("vsplit");
    nvim.input("ggvj<C-l>");
    nvim.wait_until("Normal mode in the right window", |nvim| {
        nvim.mode() == "n" && nvim.winnr() == 2
    });
    nvim.input("<C-k>");
    assert_eq!(nvim.wait_for_letter(), "U\n", "the move must not have written a letter");
    nvim.quit();
}

/// (g) The loader is a no-op without `NEOVIBE_NAV_LUA` (absent or empty), and the snippet itself
/// stands down without `NEOVIBE_PANE_SWITCH_SOCKET`: no augroup, no mapping, nvim's default `<C-L>`
/// still in place -- even after a `User VeryLazy` and a scheduled round trip.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn without_its_environment_the_loader_installs_nothing() {
    for (case, loader) in [
        ("g-unset", Loader::NavLuaUnset),
        ("g-empty", Loader::NavLuaEmpty),
        ("g-socket", Loader::SocketUnset),
    ] {
        let mut nvim = Nvim::start(case, loader, &[], &[]);
        nvim.wait_until("VimEnter", |nvim| nvim.eval("v:vim_did_enter").as_i64() == Some(1));
        nvim.command("doautocmd User VeryLazy");
        nvim.flush_scheduled();
        assert_eq!(nvim.eval("exists('#neovibe_nav')").as_i64(), Some(0), "{case}");
        assert_eq!(
            nvim.maparg("<C-l>", "n").0.as_deref(),
            Some(":help CTRL-L-default"),
            "{case}"
        );
        for (lhs, _, _) in KEYS {
            assert_eq!(nvim.global_desc(lhs, "x"), None, "{case} x {lhs}");
            if lhs != "<C-l>" {
                assert_eq!(nvim.global_desc(lhs, "n"), None, "{case} n {lhs}");
            }
        }
        nvim.quit();
    }
}
