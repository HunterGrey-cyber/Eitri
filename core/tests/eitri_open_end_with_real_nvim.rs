//! `:EitriOpen`'s report of a window that ended, in a real nvim with a real terminal UI attached.
//!
//! A takeover window ends while the terminal's UI is on its way back from the waiting room. A notice wider than the
//! command line raises the hit-enter prompt there, and nvim then waits for a key with the terminal blank (it cannot
//! redraw while it waits) and answers no ordinary request. So the report must fit on one line, whatever the log's path.
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

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn a_killed_window_is_reported_on_one_line_and_nvim_waits_for_no_key() {
    let tui = TmuxTui::start(&nvim());
    let link = tui.link();
    // A log path far wider than the 150-column terminal, as a deep XDG_STATE_HOME gives.
    let log = format!("/x/{}/takeover-0123456789abcdef.log", "deep".repeat(40));
    let killed = tui.dir.join("killed");
    let stub = tui.dir.join("eitri-stub");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\necho 'eitri open: running; its output goes to {log}' >&2\n: > '{}'\nkill -9 $$\n",
            killed.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
    let plugin = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("nvim/eitri.nvim");
    tui.lua(
        &link,
        "local plugin, cmd = ...; vim.opt.rtp:prepend(plugin); vim.cmd('runtime plugin/eitri.lua'); \
         require('eitri').setup({cmd = cmd}); vim.cmd('EitriOpen'); return true",
        vec![
            Value::from(plugin.to_str().unwrap()),
            Value::from(stub.to_str().unwrap()),
        ],
    );
    wait_until(|| killed.exists());

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
    let messages = messages.as_str().unwrap();
    assert!(
        messages.contains("eitri open: window ended (exit 137)"),
        "the end is reported: {messages:?}"
    );
    let shown = messages.lines().last().unwrap();
    // Too wide with the whole path, the log is named by its file.
    assert_eq!(
        shown,
        "eitri open: window ended (exit 137); log: takeover-0123456789abcdef.log"
    );
    let echospace = tui.lua(&link, "return vim.v.echospace", vec![]).as_u64().unwrap() as usize;
    assert!(
        shown.chars().count() <= echospace,
        "{shown:?} is wider than {echospace} columns"
    );
    wait_until(|| tui.capture().contains("eitri open: window ended (exit 137)"));
}
