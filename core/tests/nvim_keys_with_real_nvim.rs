//! The Lua half of the nvim-keys feed, driven by a real `nvim`.
//!
//! `#[ignore]`d: it needs `nvim` on PATH. It spends no tokens, touches no network, opens no window
//! and needs no display -- `--headless` is the whole harness.
//!
//! `nvim_keys.lua` and `nvim_keys::parse_report` are one contract in two languages; the unit tests
//! pin field names by substring, and only running nvim checks the values (`keytrans`'d leader and
//! lhs, a callback's `desc`, a string rhs) and the "send only a difference" rule.
//!
//! `-c` commands run before `VimEnter` (nvim `:help startup`), and `:sleep` lets timers fire, so the
//! test drives `FocusLost` by hand. Lines are counted off a raw listener, not the newest-wins
//! reader, so "nothing re-sent" is observable.
//!
//! Run: `cargo test -p neovibe-core --test nvim_keys_with_real_nvim -- --ignored`

use std::io::{ErrorKind, Read};
use std::os::unix::net::UnixListener;

use neovibe_core::nvim_keys::feed::NvimKeysFeed;
use neovibe_core::nvim_keys::parse_report;

/// Accepts until `WouldBlock` and reads each connection to EOF. nvim has exited by the time this
/// runs, so every sender has already written and closed.
fn drain_all_lines(listener: UnixListener) -> Vec<String> {
    let mut lines = Vec::new();
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                let mut text = String::new();
                stream.read_to_string(&mut text).unwrap();
                lines.extend(text.lines().map(str::to_string));
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => panic!("accept failed: {e}"),
        }
    }
    lines
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_real_nvim_reports_its_leader_and_buffer_maps_and_only_on_change() {
    let mut feed = NvimKeysFeed::new().expect("the feed must bind");
    let listener = feed.take_listener().unwrap();
    let mut command = std::process::Command::new("nvim");
    command.args(["--clean", "--headless"]);
    for arg in feed.nvim_args() {
        command.arg(arg);
    }
    for (k, v) in feed.child_env() {
        command.env(k, v);
    }
    let status = command
        .args(["-c", "lua vim.g.mapleader = ' '; vim.o.timeoutlen = 300"])
        .args([
            "-c",
            "lua vim.keymap.set('n', '<leader>bd', function() end, { desc = 'Delete Buffer' })",
        ])
        .args(["-c", "doautocmd FocusLost", "-c", "sleep 400m"])
        .args([
            "-c",
            "nnoremap H :bnext<CR>",
            "-c",
            "doautocmd FocusLost",
            "-c",
            "sleep 400m",
        ])
        .args(["-c", "doautocmd FocusLost", "-c", "sleep 400m", "-c", "qa!"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("nvim must be on PATH");
    assert!(status.success());
    let lines = drain_all_lines(listener);
    assert_eq!(
        lines.len(),
        2,
        "one report per change, none for the third FocusLost: {lines:?}"
    );
    let first = parse_report(lines[0].as_bytes()).unwrap();
    assert_eq!((first.mapleader.as_deref(), first.timeoutlen), (Some("<Space>"), 300));
    assert!(first
        .maps
        .iter()
        .any(|m| m.lhs == "<Space>bd" && m.callback && m.desc.as_deref() == Some("Delete Buffer")));
    let second = parse_report(lines[1].as_bytes()).unwrap();
    assert!(second
        .maps
        .iter()
        .any(|m| m.lhs == "H" && m.rhs.as_deref() == Some(":bnext<CR>")));
    // nvim 0.11+'s own [b/]b are callbacks with desc ":bprevious"/":bnext" (probed 2026-09-26)
    assert!(second.maps.iter().any(|m| m.lhs == "[b"));
    feed.cleanup();
}
