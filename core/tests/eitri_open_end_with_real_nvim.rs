//! eitri.nvim's report of a window that ended (`:EitriOpen`, `:EitriPage`, `:EitriPanel`), in a real nvim with a real
//! terminal UI attached.
//!
//! A takeover window ends while the terminal's UI is on its way back from the waiting room. A notice wider than the
//! command line raises the hit-enter prompt there, and nvim then waits for a key with the terminal blank (it cannot
//! redraw while it waits) and answers no ordinary request. So the report must fit on one line, whatever the log's path; what was cut is kept whole in nvim's log directory.
#[path = "support/tmux_tui.rs"]
mod tmux_tui;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eitri_core::takeover::room::room_binary;
use rmpv::Value;
use tmux_tui::{wait_until, TmuxTui};

fn nvim() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .find_map(|dir| room_binary(dir.join("nvim").to_str()?))
        .expect("nvim 0.12 is required")
}

fn field<'a>(map: &'a Value, key: &str) -> Option<&'a Value> {
    map.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// What a run of `command` against a stub `eitri` left: nvim's messages and the line shown last, its `v:echospace`,
/// and the terminal.
struct Ended {
    messages: String,
    shown: String,
    echospace: usize,
    tui: TmuxTui,
}

/// Runs `command` in a real nvim whose eitri.nvim calls `stub` (a shell script body that ends by itself), and watches
/// for three seconds that nvim never waits for a key.
fn run(command: &str, stub_body: &str) -> Ended {
    let tui = TmuxTui::start(&nvim());
    let link = tui.link();
    let done = tui.dir.join("done");
    let stub = tui.dir.join("eitri-stub");
    std::fs::write(&stub, format!("#!/bin/sh\n: > '{}'\n{stub_body}\n", done.display())).unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
    let plugin = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("nvim/eitri.nvim");
    tui.lua(
        &link,
        "local plugin, cmd, command = ...; vim.opt.rtp:prepend(plugin); vim.cmd('runtime plugin/eitri.lua'); \
         require('eitri').setup({cmd = cmd}); vim.cmd(command); return true",
        vec![
            Value::from(plugin.to_str().unwrap()),
            Value::from(stub.to_str().unwrap()),
            Value::from(command),
        ],
    );
    wait_until(|| done.exists());

    // `nvim_get_mode` is answered even while nvim waits for a key; watch it until the report is out.
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        let mode = link
            .call("nvim_get_mode", vec![])
            .wait(Duration::from_secs(2))
            .expect("nvim answers nvim_get_mode")
            .unwrap();
        assert_ne!(
            field(&mode, "blocking").and_then(Value::as_bool),
            Some(true),
            "nvim waits for a key after the window ended: {mode:?}\n{}",
            tui.capture()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let messages = tui.lua(
        &link,
        "return vim.api.nvim_exec2('messages', {output = true}).output",
        vec![],
    );
    let messages = messages.as_str().unwrap().to_string();
    let shown = messages.lines().last().unwrap_or("").to_string();
    let echospace = tui.lua(&link, "return vim.v.echospace", vec![]).as_u64().unwrap() as usize;
    assert!(
        shown.chars().count() <= echospace,
        "{shown:?} is wider than {echospace} columns"
    );
    Ended {
        messages,
        shown,
        echospace,
        tui,
    }
}

/// A log path far wider than the 150-column terminal, as a deep XDG_STATE_HOME gives.
fn deep_log() -> String {
    format!("/x/{}/takeover-0123456789abcdef.log", "deep".repeat(40))
}

/// What eitri.nvim kept whole in nvim's log directory (the terminal's nvim runs with its own XDG_STATE_HOME).
fn kept(ended: &Ended) -> String {
    std::fs::read_to_string(ended.tui.dir.join("state/nvim/eitri.log")).unwrap_or_default()
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn a_killed_window_is_reported_on_one_line_and_nvim_waits_for_no_key() {
    let log = deep_log();
    let ended = run(
        "EitriOpen",
        &format!("echo 'eitri open: running; its output goes to {log}' >&2\nkill -9 $$"),
    );
    assert!(
        ended.messages.contains("eitri open: window ended (exit 137)"),
        "the end is reported: {:?}",
        ended.messages
    );
    // Too wide with the whole path, the log is named by its file.
    assert_eq!(
        ended.shown,
        "eitri open: window ended (exit 137); log: takeover-0123456789abcdef.log"
    );
    assert!(
        kept(&ended).contains(&log),
        "the whole line is kept: {:?}",
        kept(&ended)
    );
    wait_until(|| ended.tui.capture().contains("eitri open: window ended (exit 137)"));
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn a_killed_page_or_panel_window_is_reported_on_one_line_too() {
    for (command, kind) in [("EitriPage markdown", "page"), ("EitriPanel", "panel")] {
        let log = deep_log();
        let ended = run(
            command,
            &format!("echo 'eitri {kind}: running; its output goes to {log}' >&2\nkill -9 $$"),
        );
        assert_eq!(
            ended.shown,
            format!("eitri {kind}: window ended (exit 137); log: takeover-0123456789abcdef.log"),
            "{command}: {:?}",
            ended.messages
        );
        assert!(
            kept(&ended).contains(&log),
            "{command}: the whole line is kept: {:?}",
            kept(&ended)
        );
    }
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn a_failure_wider_than_the_line_is_cut_and_kept_whole() {
    let detail = "the room could not start because ".to_string() + &"a very long reason ".repeat(12) + "END";
    let ended = run("EitriOpen", &format!("echo 'eitri open: {detail}' >&2\nexit 1"));
    assert!(
        ended.shown.starts_with("eitri open: the room could not start"),
        "{:?}",
        ended.shown
    );
    assert!(ended.shown.ends_with('…'), "a cut line says so: {:?}", ended.shown);
    assert!(ended.shown.chars().count() <= ended.echospace);
    assert!(
        kept(&ended).contains(&format!("eitri open: {detail}")),
        "the whole failure is kept: {:?}",
        kept(&ended)
    );
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn a_failure_that_fits_is_shown_whole_and_not_kept() {
    let ended = run("EitriOpen", "echo 'eitri open: the room could not start' >&2\nexit 1");
    assert_eq!(ended.shown, "eitri open: the room could not start");
    assert_eq!(kept(&ended), "", "nothing was cut, so nothing is kept");
}
