//! With `$TMUX` set and only Eitri's `tmux` shim on `PATH`, nvim's clipboard provider is `tmux`.
//! A yank to `+` must then say it reached no clipboard, once with the install hint, and never
//! repeat the yanked text. Needs a real `nvim`; no display, no tokens.

use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("clip-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).expect("scratch dir");
    dir
}

/// Runs nvim headless with `lua` on a `PATH` holding only the shim as `tmux` (and `nvim` itself),
/// and returns what it printed.
fn run_nvim(dir: &PathBuf, lua: &str) -> String {
    let nvim = Command::new("sh").args(["-c", "command -v nvim"]).output().expect("sh");
    let nvim = String::from_utf8(nvim.stdout).unwrap().trim().to_string();
    assert!(!nvim.is_empty(), "nvim is not on PATH");
    let bin = dir.join("bin");
    let _ = std::fs::remove_file(bin.join("tmux"));
    symlink(env!("CARGO_BIN_EXE_eitri-tmux-shim"), bin.join("tmux")).expect("shim link");
    let _ = std::fs::remove_file(bin.join("nvim"));
    symlink(&nvim, bin.join("nvim")).expect("nvim link");
    let script = dir.join("t.lua");
    std::fs::write(&script, lua).unwrap();
    let out = Command::new(bin.join("nvim"))
        .args(["--clean", "--headless", "-n", "-i", "NONE", "-l"])
        .arg(&script)
        .env_clear()
        .env("HOME", dir)
        .env("PATH", format!("{}:/nonexistent", bin.display()))
        .env("TMUX", "/x,1,0")
        .env("EITRI_PANE_SWITCH_SOCKET", dir.join("sock"))
        .output()
        .expect("nvim runs");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

const YANK_TWICE: &str = r#"
vim.o.clipboard = "unnamedplus"
vim.api.nvim_buf_set_lines(0, 0, -1, false, { "needle-text" })
print("provider=" .. tostring(vim.fn["provider#clipboard#Executable"]()))
vim.cmd('normal! "+yy')
vim.wait(1500)
vim.cmd('normal! "+yy')
vim.wait(1500)
print("MESSAGES")
print(vim.api.nvim_exec2("messages", { output = true }).output)
vim.cmd("qa!")
"#;

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_yank_with_no_clipboard_tool_reports_it_once_with_the_hint_and_never_the_text() {
    let dir = scratch("yank");
    let out = run_nvim(&dir, YANK_TWICE);
    assert!(
        out.contains("provider=tmux"),
        "the shim should be the provider here: {out}"
    );
    assert!(
        out.contains("clipboard: error invoking tmux"),
        "the yank must not look successful: {out}"
    );
    let messages = out.split("MESSAGES").nth(1).expect("messages marker");
    assert_eq!(
        messages.matches("install wl-clipboard (Wayland) or").count(),
        1,
        "hint once: {out}"
    );
    assert!(
        out.contains("no clipboard tool"),
        "the second yank gets the short line: {out}"
    );
    assert!(
        !out.contains("needle-text"),
        "the yanked text must not appear anywhere: {out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
