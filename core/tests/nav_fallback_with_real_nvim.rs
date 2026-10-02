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
//! Run: `cargo test -p eitri-core --test nav_fallback_with_real_nvim -- --ignored`

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eitri_core::pane_switch::{PaneMessage, PaneSwitchChannel, PaneSwitchReader};
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
    format!("eitri: window or pane {name}")
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
    /// `EITRI_NAV_LUA` absent from the env (spec step (g)).
    NavLuaUnset,
    /// `EITRI_NAV_LUA` present but empty.
    NavLuaEmpty,
    /// `EITRI_NAV_LUA` set but `EITRI_PANE_SWITCH_SOCKET` absent: the Lua itself must stand down.
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
            PaneSwitchChannel::bind(Path::new("/nonexistent/eitri-tmux-shim")).expect("the channel must bind");
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
        command.env_remove("EITRI_NAV_LUA");
        command.env_remove("EITRI_PANE_SWITCH_SOCKET");
        for (k, v) in channel.child_env() {
            let skip = match loader {
                Loader::Installed => false,
                Loader::NavLuaUnset | Loader::NavLuaEmpty => k == "EITRI_NAV_LUA",
                Loader::SocketUnset => k == "EITRI_PANE_SWITCH_SOCKET",
            };
            if !skip {
                command.env(k, v);
            }
        }
        if let Loader::NavLuaEmpty = loader {
            command.env("EITRI_NAV_LUA", "");
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

/// (g) The loader is a no-op without `EITRI_NAV_LUA` (absent or empty), and the snippet itself
/// stands down without `EITRI_PANE_SWITCH_SOCKET`: no augroup, no mapping, nvim's default `<C-L>`
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
        assert_eq!(nvim.eval("exists('#eitri_nav')").as_i64(), Some(0), "{case}");
        assert_eq!(
            nvim.maparg("<C-l>", "n").0.as_deref(),
            Some(":help CTRL-L-default"),
            "{case}"
        );
        for (lhs, _, _) in KEYS {
            assert_eq!(nvim.global_desc(lhs, "x"), None, "{case} x {lhs}");
            assert_eq!(nvim.global_desc(lhs, "i"), None, "{case} i {lhs}");
            if lhs != "<C-l>" {
                assert_eq!(nvim.global_desc(lhs, "n"), None, "{case} n {lhs}");
            }
        }
        nvim.quit();
    }
}

/// (h) #24 / K05. Insert mode: only `<C-l>` gets the fallback (leave Insert, then move right); `<C-h>`,
/// `<C-j>` and `<C-k>` keep vim's own Insert meanings (backspace, newline, digraph), so nothing is
/// mapped on them in Insert mode.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn insert_mode_gets_only_ctrl_l() {
    let mut nvim = Nvim::start("h", Loader::Installed, &[], &[]);
    nvim.wait_for_all_fallbacks("n");
    nvim.wait_for_fallback("<C-l>", "i", "right");
    for (lhs, _, _) in KEYS {
        if lhs == "<C-l>" {
            let (desc, _, buffer, callback) = nvim.maparg(lhs, "i");
            assert_eq!(desc.as_deref(), Some(fallback_desc("right").as_str()));
            assert_eq!((buffer, callback), (0, true));
        } else {
            assert_eq!(nvim.global_desc(lhs, "i"), None, "i {lhs} must stay vim's own");
            assert_eq!(nvim.maparg(lhs, "i").0, None, "i {lhs} must stay vim's own");
        }
    }
    nvim.quit();
}

/// (h, edge) In Insert mode at nvim's edge, `<C-l>` writes its letter and leaves Insert mode: the
/// keys are about to go to another pane, so the editor must not be left typing.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn insert_ctrl_l_at_the_edge_writes_its_letter_and_leaves_insert() {
    let mut nvim = Nvim::start("h-edge", Loader::Installed, &[], &[]);
    nvim.wait_for_fallback("<C-l>", "i", "right");
    nvim.input("ihello<C-l>");
    assert_eq!(nvim.wait_for_letter(), "R\n");
    nvim.wait_until("Normal mode", |nvim| nvim.mode() == "n");
    // The typed text stayed, and no ^L was typed into it.
    assert_eq!(nvim.eval("getline(1)").as_str(), Some("hello"));
    nvim.quit();
}

/// (h, split) With a split, Insert `<C-l>` from the left window leaves Insert, moves to the right
/// window and writes nothing; the sentinel `<C-k>` (Normal mode, the top edge) is then the first
/// letter to arrive.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn insert_ctrl_l_between_windows_moves_and_writes_nothing() {
    let mut nvim = Nvim::start("h-split", Loader::Installed, &[], &[]);
    nvim.wait_for_fallback("<C-l>", "i", "right");
    nvim.wait_for_all_fallbacks("n");
    nvim.command("vsplit");
    assert_eq!(nvim.winnr(), 1);
    nvim.input("iab<C-l>");
    nvim.wait_until("the cursor in the right window, in Normal mode", |nvim| {
        nvim.winnr() == 2 && nvim.mode() == "n"
    });
    nvim.input("<C-k>");
    assert_eq!(nvim.wait_for_letter(), "U\n", "the move must not have written a letter");
    assert_eq!(nvim.eval("getline(1)").as_str(), Some("ab"));
    nvim.quit();
}

/// (h, two buffers) Fix round (review of #24): `:stopinsert` only takes effect once the mapping's
/// callback returns, so moving the window inside that callback ended Insert mode in the DESTINATION
/// window: its cursor stepped left a column, `InsertLeave` fired in its buffer, and the source window's
/// cursor was never stepped back. Insert must end in the window it was started in, then the key moves on.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn insert_ctrl_l_ends_insert_in_the_source_window_and_leaves_the_destination_cursor_alone() {
    let mut nvim = Nvim::start("h-two-buffers", Loader::Installed, &[], &[]);
    nvim.wait_for_fallback("<C-l>", "i", "right");
    nvim.wait_for_all_fallbacks("n");
    // Left window: 'abcdefgh' with the cursor before the 'd'. Right window: another buffer, cursor on the '5'.
    nvim.command("vsplit");
    nvim.command("wincmd l");
    nvim.command("enew");
    nvim.command("call setline(1, '12345678')");
    nvim.command("call cursor(1, 5)");
    nvim.command("wincmd h");
    nvim.command("call setline(1, 'abcdefgh')");
    nvim.command("call cursor(1, 4)");
    assert_eq!(nvim.winnr(), 1);
    let source_buf = nvim.eval("bufnr('%')").as_i64().expect("a buffer number");
    let source_win = nvim.eval("win_getid()").as_i64().expect("a window id");
    let destination_buf = nvim.eval("winbufnr(2)").as_i64().expect("a buffer number");
    assert_ne!(source_buf, destination_buf);
    nvim.lua(
        "vim.g.leave_bufs = {} \
         vim.api.nvim_create_autocmd('InsertLeave', { callback = function() \
           local t = vim.g.leave_bufs t[#t + 1] = vim.api.nvim_get_current_buf() vim.g.leave_bufs = t \
         end })",
        vec![],
    );
    nvim.input("i<C-l>");
    nvim.wait_until("the cursor in the right window, in Normal mode", |nvim| {
        nvim.winnr() == 2 && nvim.mode() == "n"
    });
    nvim.flush_scheduled();
    // The destination window's cursor stayed on its '5' (1-based column 5).
    assert_eq!(
        nvim.eval("getcurpos()[2]").as_i64(),
        Some(5),
        "the destination cursor must not shift left"
    );
    // Insert ended in the SOURCE buffer, once, before the move.
    assert_eq!(
        nvim.eval("g:leave_bufs")
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_i64).collect::<Vec<_>>()),
        Some(vec![source_buf]),
        "InsertLeave must fire once, in the source buffer"
    );
    // And the source window's cursor stepped back one column, as <Esc> would have: 0-based 3 -> 2.
    let cursor = nvim.lua("return vim.api.nvim_win_get_cursor(...)", vec![Value::from(source_win)]);
    assert_eq!(
        cursor.as_array().and_then(|c| c[1].as_i64()),
        Some(2),
        "the source cursor steps back one"
    );
    nvim.quit();
}

/// (h, the user's own) An Insert `<C-l>` of the user's -- a plain mapping, a Lua callback, or a
/// buffer-local one -- is never replaced, across a later `User LazyLoad` too.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_users_own_insert_ctrl_l_is_left_alone() {
    let mut nvim = Nvim::start("h-own", Loader::Installed, &["inoremap <C-l> <Right>"], &[]);
    nvim.wait_for_all_fallbacks("n");
    nvim.wait_for_all_fallbacks("x");
    assert_eq!(nvim.maparg("<C-l>", "i"), (None, Some("<Right>".to_string()), 0, false));
    nvim.command("xunmap <C-l>");
    nvim.command("doautocmd User LazyLoad");
    nvim.wait_for_fallback("<C-l>", "x", "right");
    assert_eq!(nvim.maparg("<C-l>", "i"), (None, Some("<Right>".to_string()), 0, false));
    nvim.quit();

    let mut nvim = Nvim::start(
        "h-own-lua",
        Loader::Installed,
        &["lua vim.keymap.set('i', '<C-l>', function() end, { desc = 'a completion key' })"],
        &[],
    );
    nvim.wait_for_all_fallbacks("n");
    assert_eq!(
        nvim.maparg("<C-l>", "i"),
        (Some("a completion key".to_string()), None, 0, true)
    );
    nvim.quit();

    let mut nvim = Nvim::start(
        "h-own-buffer",
        Loader::Installed,
        &["autocmd VimEnter * inoremap <buffer> <C-l> <Right>"],
        &[],
    );
    nvim.wait_for_all_fallbacks("n");
    // The buffer-local mapping keeps winning in its buffer; the global slot beneath it is empty,
    // so it gets the fallback.
    nvim.wait_for_fallback("<C-l>", "i", "right");
    assert_eq!(nvim.maparg("<C-l>", "i"), (None, Some("<Right>".to_string()), 1, false));
    nvim.quit();
}

/// P5-A1, the pane half (`the private review notes`, "P5-A1"): `send()`'s
/// `pipe:connect` is asynchronous, so the actual write is queued for a *later* turn of nvim's event
/// loop rather than happening inside the mapping's own call. [`PaneSwitchReader`] (Task 5/R5) already
/// retains an accepted-but-empty connection across polls rather than dropping it on a short timeout,
/// which closes the *eventual-delivery* half of the verdict's own probe on its own (a busy-then-idle
/// nvim still gets its write out, and this crate's reader is patient enough to wait for it) -- so this
/// test does not repeat that probe. It isolates what only a synchronous sender can guarantee: the
/// write must be **on the wire before the mapping call returns**, not merely before some later
/// deadline. A marker file, written by the same callback right after issuing the key and the send,
/// pins the moment nvim's Lua has returned from running the mapping; the real nvim process is SIGKILLed
/// the instant that marker appears, cutting off event-loop turns after that point but not before it.
/// Async `pipe:connect`'s write is scheduled on exactly such a later turn, so it is provably never
/// written; a synchronous `chansend` inside the mapping has already completed, and the bytes already
/// sit in the kernel's own socket buffer, unaffected by the sender's death.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn the_letter_is_already_written_before_the_mapping_returns_even_if_nvim_dies_right_after() {
    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("nav-sync-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("scratch dir");
    let marker = scratch.join("mapping-returned");

    let mut channel =
        PaneSwitchChannel::bind(Path::new("/nonexistent/eitri-tmux-shim")).expect("the channel must bind");
    let listener = channel.take_listener().expect("a fresh channel has its listener");
    let mut reader = PaneSwitchReader::new(listener);

    // <C-l> fires from a deferred callback (as a real keypress would dispatch through nvim's own
    // input queue); the marker is written the instant that call returns, i.e. after `send()` --
    // synchronous or not -- has already been called and, if synchronous, has already completed. The
    // busy-wait after the marker only keeps the process alive long enough for the harness to observe
    // the marker and deliver the kill signal before nvim would otherwise move on.
    let trigger = format!(
        "lua vim.defer_fn(function() \
           vim.cmd.normal(vim.api.nvim_replace_termcodes('<C-l>', true, false, true)) \
           local f = io.open({marker:?}, 'w') \
           f:write('x') \
           f:close() \
           local start = vim.uv.hrtime() \
           while vim.uv.hrtime() - start < 5000 * 1000000 do end \
         end, 10)"
    );

    let mut child = Command::new("nvim")
        .args(["--clean", "--headless", "-n", "-i", "NONE"])
        .args(channel.nvim_args())
        .envs(channel.child_env())
        .current_dir(&scratch)
        .env("XDG_STATE_HOME", scratch.join("state"))
        .env("XDG_DATA_HOME", scratch.join("data"))
        .env("XDG_CONFIG_HOME", scratch.join("config"))
        // -c commands precede VimEnter; exercise the same scheduled installer this file's other
        // tests do, and the same case comment on why.
        .args(["-c", "doautocmd VimEnter"])
        .args(["-c", &trigger])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("nvim must be on PATH for this test");

    let deadline = Instant::now() + DEADLINE;
    while !marker.exists() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the mapping never returned (marker file never appeared) within {DEADLINE:?}");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    // Cut nvim off mid-busy-loop, well before its own 5s busy-wait would end on its own: a pending
    // (not yet run) `pipe:connect` write callback never gets another event-loop turn to run in.
    child.kill().expect("SIGKILL the nvim child");
    let status = child.wait().expect("wait for nvim");
    assert!(!status.success(), "expected nvim to die by signal, got {status}");

    let poll_deadline = Instant::now() + Duration::from_millis(300);
    let mut messages = Vec::new();
    while Instant::now() < poll_deadline {
        messages.extend(reader.poll());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        messages,
        vec![PaneMessage::Direction('R')],
        "the letter must already be on the wire before the mapping returns, even though nvim was \
         killed immediately afterward"
    );

    channel.cleanup();
    let _ = std::fs::remove_dir_all(&scratch);
}

// ---- the snippet injected into a running nvim, and torn down again ------------------------------

#[path = "support/embed_client.rs"]
mod embed_client;

use embed_client::{opts_map, Embed};

const NAV_SRC: &str = include_str!("../src/nav_fallback.lua");

/// The user's own configuration for the injection tests: `<C-h>` is a plain window move (LazyVim's),
/// `<C-j>` is a callback of theirs.
const INJECT_INIT: &str = r#"
vim.keymap.set("n", "<C-h>", "<C-w>h", { remap = true })
vim.keymap.set("n", "<C-j>", function() vim.g.user_cj = 1 end)
"#;

/// A scratch directory for an injected-nvim case, and a bound non-blocking listener inside it. The
/// listener lives under `temp_dir` (a socket path is capped at 103 bytes, which a worktree's target
/// directory runs past); everything else goes under the target directory.
struct Inject {
    nvim: Embed,
    listener: UnixListener,
    socket: String,
    sockdir: PathBuf,
}

impl Inject {
    fn start(case: &str, init: &str, env: &[(&str, &str)]) -> Self {
        let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("nav-inj-{}-{case}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).expect("scratch dir");
        let init_path = scratch.join("init.lua");
        std::fs::write(&init_path, init).expect("init.lua");
        let (listener, socket, sockdir) = bound_listener(case, "a.sock");
        let mut nvim = Embed::start(&scratch, &["-u", init_path.to_str().unwrap()], env);
        nvim.wait_until("VimEnter", |n| n.eval("v:vim_did_enter").as_i64() == Some(1));
        Self {
            nvim,
            listener,
            socket,
            sockdir,
        }
    }

    fn inject(&mut self, opts: &[(&str, Value)]) {
        let mut all = vec![("socket", Value::from(self.socket.as_str()))];
        all.extend(opts.iter().map(|(k, v)| (*k, v.clone())));
        self.nvim.inject(NAV_SRC, opts_map(&all));
    }

    fn finish(self) {
        self.nvim.quit();
        let _ = std::fs::remove_dir_all(&self.sockdir);
    }
}

fn bound_listener(case: &str, name: &str) -> (UnixListener, String, PathBuf) {
    let dir = std::env::temp_dir().join(format!("nv-t1-{}-{case}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("socket dir");
    let path = agent::socket_path::in_dir(&dir, name).expect("under the cap");
    let listener = UnixListener::bind(&path).expect("bind");
    listener.set_nonblocking(true).expect("non-blocking");
    (listener, path.display().to_string(), dir)
}

fn letter_on(listener: &UnixListener) -> String {
    let deadline = Instant::now() + DEADLINE;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false).expect("blocking mode");
                stream.set_read_timeout(Some(DEADLINE)).expect("read timeout");
                let mut line = String::new();
                stream.read_to_string(&mut line).expect("read the letter");
                return line;
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "no letter arrived within {DEADLINE:?}");
                std::thread::sleep(POLL);
            }
            Err(e) => panic!("accept failed: {e}"),
        }
    }
}

/// What a global mapping is, as `maparg(lhs, mode, 0, 1)` reports it.
#[derive(Debug, PartialEq, Eq)]
struct Slot {
    desc: Option<String>,
    rhs: Option<String>,
    noremap: i64,
    callback: bool,
}

fn slot(nvim: &mut Embed, lhs: &str, mode: &str) -> Slot {
    let v = nvim.lua(
        "local lhs, mode = ... \
         local m = vim.fn.maparg(lhs, mode, false, true) \
         return { m.desc or vim.NIL, m.rhs or vim.NIL, m.noremap or 0, m.callback ~= nil }",
        vec![Value::from(lhs), Value::from(mode)],
    );
    let items = v.as_array().expect("an array").clone();
    Slot {
        desc: items[0].as_str().map(str::to_string),
        rhs: items[1].as_str().map(str::to_string),
        noremap: items[2].as_i64().unwrap_or(0),
        callback: items[3].as_bool().unwrap_or(false),
    }
}

/// `maparg` hands the right-hand side back as it was typed (`<cr>` stays `<cr>`), so a comparison is
/// made in lower case.
fn rhs_lower(slot: &Slot) -> String {
    slot.rhs.as_deref().unwrap_or_default().to_lowercase()
}

fn is_fallback(nvim: &mut Embed, lhs: &str, mode: &str, name: &str) -> bool {
    slot(nvim, lhs, mode).desc.as_deref() == Some(fallback_desc(name).as_str())
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn injected_after_vimenter_installs_at_once_and_teardown_restores_every_slot() {
    let mut t = Inject::start("restore", INJECT_INIT, &[]);
    t.inject(&[]);
    // <C-h> was a plain move, <C-k> and <C-l> were empty or nvim's own; <C-j> is the user's callback.
    for (lhs, name, _) in [KEYS[0], KEYS[2], KEYS[3]] {
        t.nvim
            .wait_until(&format!("the fallback on n {lhs}"), |n| is_fallback(n, lhs, "n", name));
    }
    for (lhs, name, _) in KEYS {
        t.nvim
            .wait_until(&format!("the fallback on x {lhs}"), |n| is_fallback(n, lhs, "x", name));
    }
    let user_cj = slot(&mut t.nvim, "<C-j>", "n");
    assert!(user_cj.callback && user_cj.desc.is_none(), "{user_cj:?}");

    t.nvim.teardown();

    let footprint = t.nvim.footprint(&["eitri_nav"]);
    assert_eq!(footprint.autocmds, 0);
    assert!(footprint.eitri_maps.is_empty(), "{footprint:?}");
    let ch = slot(&mut t.nvim, "<C-h>", "n");
    assert_eq!(ch.rhs.as_deref(), Some("<C-W>h"), "{ch:?}");
    assert_eq!(ch.noremap, 0, "{ch:?}");
    let cl = slot(&mut t.nvim, "<C-l>", "n");
    let default_rhs = cl
        .rhs
        .as_deref()
        .is_some_and(|rhs| rhs.to_lowercase() == "<cmd>nohlsearch<bar>diffupdate<bar>normal! <c-l><cr>");
    assert!(
        cl.desc.as_deref() == Some(":help CTRL-L-default") || default_rhs,
        "nvim's own <C-l> must be back: {cl:?}"
    );
    for (lhs, _, _) in [KEYS[1], KEYS[2]] {
        // <C-k> had no mapping in Normal mode: nothing may remain of ours.
        let s = slot(&mut t.nvim, lhs, "n");
        assert!(!s.desc.as_deref().unwrap_or("").starts_with("eitri"), "{lhs}: {s:?}");
    }
    let user_cj = slot(&mut t.nvim, "<C-j>", "n");
    assert!(
        user_cj.callback && user_cj.desc.is_none(),
        "the user's <C-j> was lost: {user_cj:?}"
    );
    t.finish();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_slot_the_user_remapped_after_install_keeps_the_users_map() {
    let mut t = Inject::start("remapped", INJECT_INIT, &[]);
    t.inject(&[]);
    t.nvim
        .wait_until("the fallback on n <C-l>", |n| is_fallback(n, "<C-l>", "n", "right"));
    t.nvim.lua("vim.keymap.set('n', '<C-l>', '<cmd>echo 1<cr>')", vec![]);
    t.nvim.teardown();
    let cl = slot(&mut t.nvim, "<C-l>", "n");
    assert_eq!(rhs_lower(&cl), "<cmd>echo 1<cr>", "{cl:?}");
    t.finish();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn with_opts_the_environment_is_never_read() {
    let (a, a_socket, a_dir) = bound_listener("env-a", "a.sock");
    let mut t = Inject::start("env", INJECT_INIT, &[("EITRI_PANE_SWITCH_SOCKET", a_socket.as_str())]);
    t.inject(&[]);
    t.nvim
        .wait_until("the fallback on n <C-l>", |n| is_fallback(n, "<C-l>", "n", "right"));
    t.nvim.input("<C-l>");
    assert_eq!(
        letter_on(&t.listener),
        "R\n",
        "the letter goes to the socket in the options"
    );
    match a.accept() {
        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
        other => panic!("the environment's socket must receive nothing, got {other:?}"),
    }
    t.finish();
    let _ = std::fs::remove_dir_all(a_dir);
}

const NAVIGATOR_INIT: &str = r#"
vim.cmd([[command! TmuxNavigateLeft wincmd h]])
vim.cmd([[nnoremap <silent> <c-h> :<C-U>TmuxNavigateLeft<cr>]])
"#;

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn navigator_maps_are_plain_moves_only_for_a_companion_outside_tmux() {
    // A companion outside tmux: the navigator's slot is taken, and given back on teardown.
    let mut t = Inject::start("nav-companion", NAVIGATOR_INIT, &[]);
    t.inject(&[("companion", Value::from(true))]);
    t.nvim
        .wait_until("the fallback on n <C-h>", |n| is_fallback(n, "<C-h>", "n", "left"));
    t.nvim.teardown();
    let ch = slot(&mut t.nvim, "<C-h>", "n");
    assert_eq!(rhs_lower(&ch), ":<c-u>tmuxnavigateleft<cr>", "{ch:?}");
    assert!(ch.desc.is_none(), "{ch:?}");
    t.finish();

    // Not a companion: the slot is left alone, though the check has demonstrably run.
    let mut t = Inject::start("nav-plain", NAVIGATOR_INIT, &[]);
    t.inject(&[]);
    t.nvim
        .wait_until("the fallback on n <C-k>", |n| is_fallback(n, "<C-k>", "n", "up"));
    let ch = slot(&mut t.nvim, "<C-h>", "n");
    assert_eq!(rhs_lower(&ch), ":<c-u>tmuxnavigateleft<cr>", "{ch:?}");
    assert!(ch.desc.is_none(), "{ch:?}");
    t.finish();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn inside_tmux_the_navigator_maps_are_left_alone() {
    let mut t = Inject::start("nav-tmux", NAVIGATOR_INIT, &[("TMUX", "/tmp/x,1,0")]);
    t.inject(&[("companion", Value::from(true))]);
    t.nvim
        .wait_until("the fallback on n <C-k>", |n| is_fallback(n, "<C-k>", "n", "up"));
    let ch = slot(&mut t.nvim, "<C-h>", "n");
    assert_eq!(rhs_lower(&ch), ":<c-u>tmuxnavigateleft<cr>", "{ch:?}");
    assert!(ch.desc.is_none(), "{ch:?}");
    t.finish();
}

/// The check the late install queues must not run after the teardown, and must not put the maps
/// back. Both happen in one request, so nothing scheduled in between can run.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_teardown_before_the_scheduled_check_installs_nothing() {
    let mut t = Inject::start("early-teardown", INJECT_INIT, &[]);
    let mut before = Vec::new();
    for mode in ["n", "x"] {
        for (lhs, _, _) in KEYS {
            before.push(slot(&mut t.nvim, lhs, mode));
        }
    }
    t.nvim
        .inject_and_teardown(NAV_SRC, opts_map(&[("socket", Value::from(t.socket.as_str()))]));
    t.nvim.flush_scheduled();
    let footprint = t.nvim.footprint(&["eitri_nav"]);
    assert_eq!(footprint.autocmds, 0);
    assert!(footprint.eitri_maps.is_empty(), "{footprint:?}");
    let mut after = Vec::new();
    for mode in ["n", "x"] {
        for (lhs, _, _) in KEYS {
            after.push(slot(&mut t.nvim, lhs, mode));
        }
    }
    assert_eq!(before, after, "every slot must hold what it held before");
    t.finish();
}
