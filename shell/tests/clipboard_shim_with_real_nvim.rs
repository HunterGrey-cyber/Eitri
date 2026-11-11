//! Real nvim's clipboard provider through the window's tmux shim, with a private in-memory host.
//! No display, session bus, user configuration, clipboard tools or model accounts are reachable.

use eitri_core::host_clipboard::{self, Admission, Answer, ClipboardServer, Reply, Request, NOT_NVIM, SOCKET_NAME};
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct Fixture {
    dir: PathBuf,
    held: Arc<Mutex<Vec<u8>>>,
    copies: Arc<AtomicUsize>,
    pastes: Arc<AtomicUsize>,
    server: Option<ClipboardServer>,
}

fn command_path(name: &str) -> PathBuf {
    let out = Command::new("sh")
        .args(["-c", "command -v \"$1\"", "sh", name])
        .output()
        .expect("command lookup");
    assert!(out.status.success(), "required executable is missing: {name}");
    let path = PathBuf::from(String::from_utf8(out.stdout).expect("executable path is UTF-8").trim());
    assert!(
        path.is_absolute(),
        "required executable must have an absolute path: {name}"
    );
    path
}

impl Fixture {
    /// `None` leaves the socket absent; `silent` retains paste replies until the server times out.
    fn new(name: &str, initial: Option<&[u8]>, silent: bool) -> Self {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("clip-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bin = dir.join("chan/bin");
        std::fs::create_dir_all(&bin).expect("scratch dir");
        symlink(env!("CARGO_BIN_EXE_eitri-tmux-shim"), bin.join("tmux")).expect("shim link");
        for name in ["nvim", "sh", "sleep"] {
            symlink(command_path(name), bin.join(name)).expect("executable link");
        }
        let held = Arc::new(Mutex::new(initial.unwrap_or_default().to_vec()));
        let copies = Arc::new(AtomicUsize::new(0));
        let pastes = Arc::new(AtomicUsize::new(0));
        let server = initial.map(|_| {
            let listener = UnixListener::bind(dir.join("chan").join(SOCKET_NAME)).expect("clipboard socket");
            let host_held = Arc::clone(&held);
            let host_copies = Arc::clone(&copies);
            let host_pastes = Arc::clone(&pastes);
            let copied = dir.join("copies");
            let pending = Mutex::new(Vec::<Reply>::new());
            // This test process plays the window: nvim is its direct child, started with the switch socket.
            let admission = Admission::for_window(&dir.join("chan/s.sock"));
            host_clipboard::serve(listener, admission, move |request, reply| match request {
                Request::Copy(text) => {
                    *host_held.lock().expect("clipboard lock") = text;
                    let count = host_copies.fetch_add(1, Ordering::SeqCst) + 1;
                    reply.send(Answer::Done);
                    // Only counts cross this test acknowledgement path, never clipboard text.
                    std::fs::write(&copied, count.to_string()).expect("copy acknowledgement");
                }
                Request::Paste => {
                    host_pastes.fetch_add(1, Ordering::SeqCst);
                    if silent {
                        pending.lock().expect("pending replies lock").push(reply);
                    } else {
                        reply.send(Answer::Text(host_held.lock().expect("clipboard lock").clone()));
                    }
                }
            })
            .expect("clipboard server")
        });
        Self {
            dir,
            held,
            copies,
            pastes,
            server,
        }
    }

    fn run(&self, lua: &str, forbidden: &[&str]) -> String {
        let script = self.dir.join("t.lua");
        std::fs::write(&script, format!("{PRELUDE}\n{lua}\n{FINISH}")).expect("Lua script");
        let bin = self.dir.join("chan/bin");
        let mut command = Command::new(bin.join("nvim"));
        command
            .args(["--clean", "--headless", "-n", "-i", "NONE", "-l"])
            .arg(&script)
            .current_dir(&self.dir)
            .env_clear()
            .env("HOME", &self.dir)
            .env("PATH", format!("{}:/nonexistent", bin.display()))
            .env("TMUX", "/x,1,0")
            .env("EITRI_PANE_SWITCH_SOCKET", self.dir.join("chan/s.sock"));
        let out = command.output().expect("nvim runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        for text in forbidden {
            assert!(
                !stdout.contains(text) && !stderr.contains(text),
                "clipboard text appeared in nvim output"
            );
        }
        // Checked above to hold none of the clipboard text, so it may be shown.
        assert!(out.status.success(), "nvim's Lua checks failed: {stdout}{stderr}");
        format!("{stdout}{stderr}")
    }

    fn assert_host(&self, text: &[u8], copies: usize) {
        // Boolean comparisons keep even a failing test from printing clipboard contents.
        let held = self.held.lock().expect("clipboard lock").clone();
        assert!(
            held == text,
            "host clipboard differs: {} bytes held, {} expected",
            held.len(),
            text.len()
        );
        assert_eq!(self.copies.load(Ordering::SeqCst), copies, "host copy count");
    }

    /// Also proves this fixture can reach the shim when the preferred provider is absent.
    fn tmux_control(&self) {
        let out = self.run(
            r#"
assert(provider() == "tmux", "control provider differs")
vim.api.nvim_buf_set_lines(0, 0, -1, false, { "control-secret" })
vim.cmd('normal! "+yy')
wait_copies(1)
"#,
            &["control-secret"],
        );
        assert!(!out.contains("clipboard: error"), "control copy failed");
        self.assert_host(b"control-secret\n", 1);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.stop_and_join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const PRELUDE: &str = r#"
vim.o.clipboard = "unnamedplus"
local function provider()
  return vim.fn["provider#clipboard#Executable"]()
end
local function messages()
  return vim.api.nvim_exec2("messages", { output = true }).output
end
local function jobs_done()
  for _, channel in ipairs(vim.api.nvim_list_chans()) do
    if channel.stream == "job" then return false end
  end
  return true
end
local function wait_copies(count)
  assert(vim.wait(6000, function()
    local path = vim.env.HOME .. "/copies"
    if vim.fn.filereadable(path) == 0 then return false end
    return (tonumber(vim.fn.readfile(path)[1]) or 0) >= count and jobs_done()
  end, 10), "copy did not finish")
end
local function wait_message(text)
  assert(vim.wait(6000, function()
    return messages():find(text, 1, true) ~= nil and jobs_done()
  end, 10), "clipboard diagnostic did not arrive")
end
"#;

const FINISH: &str = r#"
print("MESSAGES")
print(messages())
vim.cmd("qa!")
"#;

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_line_yank_reaches_the_host_without_a_clipboard_error() {
    let f = Fixture::new("line", Some(b""), false);
    let out = f.run(
        r#"
print("provider=" .. provider())
vim.api.nvim_buf_set_lines(0, 0, -1, false, { "needle-one" })
vim.cmd('normal! "+yy')
wait_copies(1)
"#,
        &["needle-one"],
    );
    assert!(out.contains("provider=tmux"), "provider differs");
    assert!(!out.contains("clipboard: error"), "copy reported an error");
    f.assert_host(b"needle-one\n", 1);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn linewise_and_characterwise_clipboards_keep_their_shape() {
    let f = Fixture::new("shapes", Some(b""), false);
    let out = f.run(
        r#"
assert(provider() == "tmux", "provider differs")
vim.api.nvim_buf_set_lines(0, 0, -1, false, { "l1", "l2" })
vim.cmd('normal! "+yj')
wait_copies(1)
"#,
        &["l1", "l2"],
    );
    assert!(!out.contains("clipboard: error"), "copy reported an error");
    f.assert_host(b"l1\nl2\n", 1);

    for (text, regtype, expected) in [
        (b"x1\nx2\n".as_slice(), "V", r#"{ "anchor", "x1", "x2" }"#),
        (b"abc".as_slice(), "v", r#"{ "aabcnchor" }"#),
    ] {
        *f.held.lock().expect("clipboard lock") = text.to_vec();
        let out = f.run(
            &format!(
                r#"
assert(provider() == "tmux", "provider differs")
vim.api.nvim_buf_set_lines(0, 0, -1, false, {{ "anchor" }})
vim.api.nvim_win_set_cursor(0, {{ 1, 0 }})
vim.cmd('normal! "+p')
assert(vim.deep_equal(vim.api.nvim_buf_get_lines(0, 0, -1, false), {expected}), "paste shape differs")
assert(vim.fn.getregtype('+') == "{regtype}", "register type differs")
"#
            ),
            &["x1", "x2", "abc", "anchor"],
        );
        assert!(!out.contains("clipboard: error"), "paste reported an error");
        f.assert_host(text, 1);
    }
    assert!(f.pastes.load(Ordering::SeqCst) >= 2, "paste did not reach the host");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_block_yank_and_paste_in_one_nvim_keeps_the_block_type() {
    let f = Fixture::new("block", Some(b""), false);
    let out = f.run(
        r#"
assert(provider() == "tmux", "provider differs")
vim.api.nvim_buf_set_lines(0, 0, -1, false, { "abcd", "efgh" })
vim.cmd('normal! gg0' .. string.char(22) .. 'jl"+y')
wait_copies(1)
vim.cmd('normal! gg0"+p')
assert(vim.fn.getregtype('+'):sub(1, 1) == string.char(22), "block type was lost")
assert(vim.deep_equal(vim.api.nvim_buf_get_lines(0, 0, -1, false), { "aabbcd", "eeffgh" }), "block paste differs")
"#,
        &["abcd", "efgh", "aabbcd", "eeffgh"],
    );
    assert!(!out.contains("clipboard: error"), "block clipboard reported an error");
    // nvim hands a block over line by line, each ending in a newline; the block type comes back from nvim's own
    // copy, which it keeps while the clipboard still holds what it yanked.
    f.assert_host(b"ab\nef\n", 1);
    assert!(f.pastes.load(Ordering::SeqCst) > 0, "paste did not reach the host");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_host_that_keeps_a_paste_reply_is_bounded() {
    let f = Fixture::new("silent", Some(b"silent-secret"), true);
    // Whose words come back is the check: the server's own limit ("the clipboard did not answer"), not
    // the shim giving up ("the window did not answer"); a server with no limit would leave only the latter.
    let out = f.run(
        r#"
assert(provider() == "tmux", "provider differs")
-- A failed read leaves `+` empty, so the paste itself ends in E353 after the clipboard's own line.
local ok, err = pcall(vim.cmd, 'normal! "+p')
assert(not ok and tostring(err):find("E353", 1, true), "the failed paste should leave + empty")
assert(messages():find("the clipboard did not answer", 1, true), "the server's own limit should answer")
"#,
        &["silent-secret"],
    );
    assert!(
        out.contains("the clipboard did not answer"),
        "timeout diagnostic missing"
    );
    assert!(
        !out.contains("the window did not answer"),
        "the shim gave up before the server did"
    );
    assert!(f.pastes.load(Ordering::SeqCst) > 0, "paste did not reach the host");
    f.assert_host(b"silent-secret", 0);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_unreachable_window_gives_the_hint_once_then_a_short_line() {
    let f = Fixture::new("absent", None, false);
    let out = f.run(
        r#"
assert(provider() == "tmux", "provider differs")
vim.api.nvim_buf_set_lines(0, 0, -1, false, { "unreachable-secret-one" })
vim.cmd('normal! "+yy')
wait_message("to bypass it")
vim.api.nvim_buf_set_lines(0, 0, -1, false, { "unreachable-secret-two" })
vim.cmd('normal! "+yy')
assert(vim.wait(6000, function()
  local _, count = messages():gsub("the window's clipboard is not reachable", "")
  return count == 2 and jobs_done()
end, 10), "second refusal missing")
"#,
        &["unreachable-secret-one", "unreachable-secret-two"],
    );
    let messages = out.split_once("MESSAGES").expect("messages marker").1;
    // The hint's whole text is the shim's unit tests'; its end is enough to count it here.
    let full = "eitri: the window's clipboard is not reachable; install ";
    let short = "eitri: the window's clipboard is not reachable";
    assert_eq!(messages.matches(full).count(), 1, "full hint count");
    assert_eq!(messages.matches("to bypass it").count(), 1, "full hint count");
    assert_eq!(messages.matches(short).count(), 2, "unreachable diagnostic count");
    assert_eq!(
        messages.lines().filter(|line| line.trim_end().ends_with(short)).count(),
        1
    );
    assert!(std::fs::read(f.dir.join("chan/clipboard-told"))
        .expect("marker")
        .is_empty());
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_oversized_yank_is_refused_before_the_host_sees_a_copy() {
    let f = Fixture::new("oversized", Some(b""), false);
    let out = f.run(
        r#"
assert(provider() == "tmux", "provider differs")
local lines = {}
for i = 1, 17 do lines[i] = string.rep("q", 1024 * 1024) end
vim.api.nvim_buf_set_lines(0, 0, -1, false, lines)
vim.cmd('normal! gg"+yG')
wait_message("over the clipboard limit")
"#,
        &[&"q".repeat(64)],
    );
    assert!(out.contains("over the clipboard limit"), "limit diagnostic missing");
    assert!(
        out.contains("the text stays in nvim's register"),
        "register diagnostic missing"
    );
    f.assert_host(b"", 0);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_lua_function_provider_takes_precedence_over_the_shim() {
    let f = Fixture::new("custom", Some(b""), false);
    let out = f.run(
        r#"
local copied = false
local function copy(lines, regtype)
  copied = vim.deep_equal(lines, { "custom-secret", "" }) and regtype == "V"
end
local function paste() return { { "custom-secret", "" }, "V" } end
vim.g.clipboard = {
  name = "test-functions", copy = { ['+'] = copy, ['*'] = copy },
  paste = { ['+'] = paste, ['*'] = paste }, cache_enabled = 0,
}
assert(provider() == "test-functions", "custom provider was not selected")
vim.api.nvim_buf_set_lines(0, 0, -1, false, { "custom-secret" })
vim.cmd('normal! "+yy')
assert(copied, "function provider did not receive the yank")
"#,
        &["custom-secret"],
    );
    assert!(!out.contains("clipboard: error"), "custom provider reported an error");
    f.assert_host(b"", 0);
    f.tmux_control();
}

/// `lemonade` stands for every tool nvim looks for before `tmux` (its list in `provider/clipboard.vim`): it is
/// one that needs no display, so this test reaches none, and the order is nvim's own.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_clipboard_tool_on_path_takes_precedence_over_the_shim() {
    let f = Fixture::new("tool", Some(b""), false);
    let bin = f.dir.join("chan/bin");
    let tool = bin.join("lemonade");
    // Shell built-ins only: the test's PATH holds nothing but the bin directory.
    let script = "#!/bin/sh\nif [ \"$1\" = copy ]; then\n  while IFS= read -r line; do printf '%s\\n' \"$line\"; done > \"$HOME/tool-copy\"\nelse\n  printf called > \"$HOME/tool-paste\"\n  while IFS= read -r line; do printf '%s\\n' \"$line\"; done < \"$HOME/tool-copy\"\nfi\n";
    script_fixture::write_executable_script(&tool, script, 0o700).expect("executable stand-in");
    let out = f.run(
        r#"
print("provider=" .. provider())
vim.api.nvim_buf_set_lines(0, 0, -1, false, { "tool-secret" })
vim.cmd('normal! "+yy')
assert(vim.wait(6000, function()
  return vim.fn.filereadable(vim.env.HOME .. "/tool-copy") == 1 and jobs_done()
end, 10), "the tool's copy did not finish")
vim.cmd('normal! "+p')
"#,
        &["tool-secret"],
    );
    assert!(out.contains("provider=lemonade"), "the tool was not selected");
    assert!(!out.contains("clipboard: error"), "the tool reported an error");
    assert!(std::fs::read(f.dir.join("tool-copy")).expect("tool recording") == b"tool-secret\n");
    assert!(f.dir.join("tool-paste").is_file(), "the tool's paste was not called");
    f.assert_host(b"", 0);
    assert_eq!(f.pastes.load(Ordering::SeqCst), 0);
    std::fs::remove_file(&tool).expect("remove stand-in");
    f.tmux_control();
}

/// Both of nvim's calls through `sh -c` (the default paste already is; here the copy too): the shim is
/// then nvim's grandchild, and it is still the window's nvim that it descends from.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn copy_and_paste_through_a_shell_still_descend_from_nvim() {
    let f = Fixture::new("via-sh", Some(b""), false);
    let out = f.run(
        r#"
vim.g.clipboard = {
  name = "via-sh",
  copy = { ['+'] = { "sh", "-c", "tmux load-buffer -" }, ['*'] = { "sh", "-c", "tmux load-buffer -" } },
  paste = { ['+'] = { "sh", "-c", "tmux save-buffer -" }, ['*'] = { "sh", "-c", "tmux save-buffer -" } },
}
assert(provider() == "via-sh", "provider differs")
vim.api.nvim_buf_set_lines(0, 0, -1, false, { "shell-secret" })
vim.cmd('normal! "+yy')
wait_copies(1)
vim.api.nvim_buf_set_lines(0, 0, -1, false, { "anchor" })
vim.cmd('normal! "+p')
assert(vim.deep_equal(vim.api.nvim_buf_get_lines(0, 0, -1, false), { "anchor", "shell-secret" }), "paste differs")
"#,
        &["shell-secret"],
    );
    assert!(!out.contains("clipboard: error"), "a call through sh was refused");
    f.assert_host(b"shell-secret\n", 1);
    assert!(f.pastes.load(Ordering::SeqCst) > 0, "paste did not reach the host");
}

/// A process that is not under the window's nvim (here the test process itself) is refused with the
/// reason, and the host never hears of it.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_process_outside_nvim_is_refused() {
    let f = Fixture::new("outsider", Some(b"outsider-secret"), false);
    let socket = f.dir.join("chan").join(SOCKET_NAME);
    for request in [Request::Paste, Request::Copy(b"planted".to_vec())] {
        let got = host_clipboard::request(&socket, &request, host_clipboard::CLIENT_WAIT).expect("an answer");
        assert!(
            got == Answer::Failed(NOT_NVIM.to_string()),
            "outsider was served: {got:?}"
        );
    }
    f.assert_host(b"outsider-secret", 0);
    assert_eq!(f.pastes.load(Ordering::SeqCst), 0);
}

/// Other plugins' buffer calls (vim-slime's named buffer, a file, an extra flag) exit 1 and reach no
/// clipboard, and a call left with an open stdin does not wait on it.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn other_buffer_calls_exit_1_and_reach_no_clipboard() {
    let f = Fixture::new("plugins", Some(b"kept"), false);
    let out = f.run(
        r#"
for _, argv in ipairs({
  { "tmux", "load-buffer", "-b", "slime", "-" },
  { "tmux", "load-buffer", vim.env.HOME .. "/f" },
  { "tmux", "save-buffer", "-b", "slime", "-" },
  { "tmux", "save-buffer", vim.env.HOME .. "/g" },
  { "tmux", "-S", "/x", "save-buffer", "-" },
}) do
  vim.fn.system(argv, "plugin-text")
  assert(vim.v.shell_error == 1, table.concat(argv, " ") .. " exited " .. vim.v.shell_error)
end
-- stdin left open: a refusal ends without reading it (the wait is a hang guard, the code is the check)
local job = vim.fn.jobstart({ "tmux", "load-buffer", "-b", "slime", "-" })
assert(vim.fn.jobwait({ job }, 60000)[1] == 1, "the open-stdin call did not exit 1")
"#,
        &["plugin-text"],
    );
    assert!(!out.contains("plugin-text"), "plugin text appeared");
    f.assert_host(b"kept", 0);
    assert_eq!(f.pastes.load(Ordering::SeqCst), 0);
    assert!(!f.dir.join("g").exists(), "save-buffer to a file wrote one");
}

#[cfg(test)]
mod script_fixture {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/executable.rs"));
}
