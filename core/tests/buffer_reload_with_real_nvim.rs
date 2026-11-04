//! wire 3's `--cmd`, driven by a real `nvim`.
//!
//! `#[ignore]`d: needs `nvim` on PATH. No tokens, no network, no display.
//!
//! The unit tests assert things ABOUT the command string -- that it schedules, that it keeps its
//! handle alive, that it does not second-guess nvim. None of them can tell whether the timer
//! actually fires, and both of the mistakes those tests describe (a fast-event `vim.cmd`, a
//! collected handle) fail at RUNTIME and only sometimes. So this runs it.
//!
//! Run: `cargo test -p eitri-core --test buffer_reload_with_real_nvim -- --ignored`

use eitri_core::buffer_reload::{nvim_args, RELOAD_INTERVAL_MS};

/// Edits `file`, rewrites it on disk from inside nvim, waits out one timer period, and returns what
/// the buffer holds afterwards.
///
/// **No `checktime` anywhere in the command sequence**, which is the whole point: whatever the
/// buffer says at the end, the timer put there.
fn buffer_after_a_disk_change(extra_args: &[String], modify_buffer_first: bool) -> String {
    let dir = std::env::temp_dir().join(format!("nv-w3-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("f.txt");
    std::fs::write(&file, "one\ntwo\n").unwrap();

    let mut command = std::process::Command::new("nvim");
    // `--clean`: the user's own config is irrelevant here and, on this machine, actively misleading
    // -- LazyVim installs a `checktime` autocommand of its own, on `FocusGained`, which cannot fire
    // in Eitri but CAN fire in some other harness and would make this pass for the wrong reason.
    command.arg("--headless").arg("--clean");
    for arg in extra_args {
        command.arg(arg);
    }
    command.args(["-c", &format!("edit {}", file.display())]);
    if modify_buffer_first {
        command.args(["-c", "normal! ggIEDITED "]);
    }
    let out = dir.join("out.txt");
    let status = command
        .args([
            "-c",
            &format!(
                "lua vim.fn.system('printf \"DISK\\\\nchange\\\\n\" > {}')",
                file.display()
            ),
        ])
        .args(["-c", &format!("sleep {}m", RELOAD_INTERVAL_MS + 800)])
        .args([
            "-c",
            &format!(
                "lua vim.fn.writefile(vim.api.nvim_buf_get_lines(0,0,-1,false), '{}')",
                out.display()
            ),
        ])
        .args(["-c", "qa!"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("nvim must be on PATH for this test");
    assert!(status.success(), "nvim exited with {status}");
    let content = std::fs::read_to_string(&out).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    content
}

/// The property wire 3 exists for, end to end and with no explicit `checktime`.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_file_changed_on_disk_reaches_an_unmodified_buffer_with_no_help() {
    let reloaded = buffer_after_a_disk_change(&nvim_args(), false);
    assert!(
        reloaded.contains("DISK"),
        "the timer did not reload the buffer; it holds: {reloaded:?}"
    );
    assert!(!reloaded.contains("one"), "the old contents survived: {reloaded:?}");
}

/// The negative control, and it is not decoration: without the `--cmd` this test's own harness would
/// still pass if nvim reloaded for some reason of its own -- a `sleep` is not an absence of events.
/// This pins that the reload is THIS code's doing.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn without_the_cmd_the_same_buffer_stays_stale() {
    let stale = buffer_after_a_disk_change(&[], false);
    assert!(
        stale.contains("one"),
        "nvim reloaded on its own, so the positive test proves nothing: {stale:?}"
    );
    assert!(!stale.contains("DISK"), "{stale:?}");
}

/// A buffer the user has edited is never clobbered: the timer does not check a modified buffer.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_buffer_with_unsaved_edits_is_left_alone() {
    let kept = buffer_after_a_disk_change(&nvim_args(), true);
    assert!(kept.contains("EDITED"), "the user's unsaved edit was lost: {kept:?}");
    assert!(
        !kept.contains("DISK"),
        "a modified buffer must not be reloaded: {kept:?}"
    );
}

#[path = "support/embed_client.rs"]
mod embed_client;
#[path = "support/reload_cases.rs"]
mod reload_cases;

/// With a UI attached, `checktime` on a modified buffer is a `W12` dialog that takes the person's
/// next keys. A person typing in insert mode while the file changes on disk stays in insert mode,
/// with nothing blocking and the typed text kept.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn typing_in_a_file_changed_on_disk_meets_no_dialog() {
    use embed_client::{opts_map, Embed};
    use rmpv::Value;

    let scratch = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("br-typ-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    let file = scratch.join("f.txt");
    std::fs::write(&file, "one\ntwo\n").unwrap();

    let mut nvim = Embed::start(&scratch, &[], &[]);
    nvim.wait_until("VimEnter", |n| n.eval("v:vim_did_enter").as_i64() == Some(1));
    nvim.request(
        "nvim_ui_attach",
        vec![
            Value::from(80),
            Value::from(24),
            opts_map(&[("ext_linegrid", Value::from(true))]),
        ],
    );
    nvim.command(&format!("edit {}", file.display()));
    nvim.inject(
        eitri_core::buffer_reload::RELOAD_CMD.trim_start_matches("lua "),
        opts_map(&[]),
    );
    let mode = |nvim: &mut Embed| -> (String, bool) {
        let m = nvim.request("nvim_get_mode", vec![]);
        let get = |key: &str| {
            m.as_map()
                .and_then(|entries| entries.iter().find(|(k, _)| k.as_str() == Some(key)))
                .map(|(_, v)| v.clone())
        };
        (
            get("mode")
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
            get("blocking").and_then(|v| v.as_bool()).unwrap_or(false),
        )
    };
    nvim.input("ix");
    nvim.wait_until("insert mode", |n| mode(n).0 == "i");
    std::fs::write(&file, "DISK\nchange\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2 * RELOAD_INTERVAL_MS + 500));
    assert_eq!(
        mode(&mut nvim),
        ("i".to_string(), false),
        "a dialog or a mode change met the typing"
    );

    nvim.input("<Esc>");
    nvim.wait_until("normal mode", |n| mode(n).0 == "n");
    let first = nvim.eval("getline(1)");
    assert_eq!(first.as_str(), Some("xone"), "the typed text was lost");
    nvim.teardown();
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// Injected into a running nvim (what a companion panel does), the timer reloads a changed file, and
/// the teardown it returns stops the timer and clears its global.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn injected_reload_timer_reloads_and_teardown_stops_it() {
    use embed_client::{opts_map, Embed};
    use rmpv::Value;

    let scratch = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("br-inj-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    let file = scratch.join("f.txt");
    std::fs::write(&file, "one\ntwo\n").unwrap();

    let mut nvim = Embed::start(&scratch, &[], &[]);
    nvim.wait_until("VimEnter", |n| n.eval("v:vim_did_enter").as_i64() == Some(1));
    nvim.command(&format!("edit {}", file.display()));
    let baseline = nvim.footprint_of(&[], &["eitri_reload_timer"]);
    assert!(baseline.globals.is_empty(), "{baseline:?}");

    nvim.inject(
        eitri_core::buffer_reload::RELOAD_CMD.trim_start_matches("lua "),
        opts_map(&[]),
    );
    let running = nvim.footprint_of(&[], &["eitri_reload_timer"]);
    assert_eq!(running.active_timers, baseline.active_timers + 1, "{running:?}");
    assert_eq!(running.globals, vec!["eitri_reload_timer".to_string()]);

    let lines = |nvim: &mut Embed| -> Vec<String> {
        nvim.request(
            "nvim_buf_get_lines",
            vec![Value::from(0), Value::from(0), Value::from(-1), Value::from(false)],
        )
        .as_array()
        .expect("lines")
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
    };
    std::fs::write(&file, "DISK\nchange\n").unwrap();
    nvim.wait_until("the timer's reload", |n| {
        lines(n).first().map(String::as_str) == Some("DISK")
    });

    nvim.teardown();
    let stopped = nvim.footprint_of(&[], &["eitri_reload_timer"]);
    assert!(stopped.globals.is_empty(), "{stopped:?}");
    assert_eq!(stopped.active_timers, baseline.active_timers, "{stopped:?}");

    std::fs::write(&file, "LATER AND LONGER\nchange\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2500));
    assert_eq!(
        lines(&mut nvim).first().map(String::as_str),
        Some("DISK"),
        "a stopped timer reloaded"
    );
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// An nvim with a UI attached (80x24), `f.txt` (`one`, `two`) open and the timer injected.
fn ui_with_timer(label: &str) -> (embed_client::Embed, std::path::PathBuf, std::path::PathBuf) {
    ui_with_timer_env(label, &[])
}

fn ui_with_timer_env(
    label: &str,
    env: &[(&str, &str)],
) -> (embed_client::Embed, std::path::PathBuf, std::path::PathBuf) {
    use embed_client::{opts_map, Embed};
    use rmpv::Value;

    let scratch = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("br-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    let file = scratch.join("f.txt");
    std::fs::write(&file, "one\ntwo\n").unwrap();
    let mut nvim = Embed::start(&scratch, &[], env);
    nvim.wait_until("VimEnter", |n| n.eval("v:vim_did_enter").as_i64() == Some(1));
    nvim.request(
        "nvim_ui_attach",
        vec![
            Value::from(80),
            Value::from(24),
            opts_map(&[("ext_linegrid", Value::from(true))]),
        ],
    );
    nvim.command(&format!("edit {}", file.display()));
    nvim.inject(
        eitri_core::buffer_reload::RELOAD_CMD.trim_start_matches("lua "),
        opts_map(&[]),
    );
    (nvim, scratch, file)
}

/// `nvim_get_mode`'s mode and whether nvim is blocked waiting for input.
fn ui_mode(nvim: &mut embed_client::Embed) -> (String, bool) {
    let m = nvim.request("nvim_get_mode", vec![]);
    let get = |key: &str| {
        m.as_map()
            .and_then(|entries| entries.iter().find(|(k, _)| k.as_str() == Some(key)))
            .map(|(_, v)| v.clone())
    };
    (
        get("mode")
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default(),
        get("blocking").and_then(|v| v.as_bool()).unwrap_or(false),
    )
}

/// A reload's autocommand that closes a floating window the loop has not reached yet costs that
/// window its step, not an error in the scheduled callback (which would be a message stopping nvim at
/// a hit-enter prompt).
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_window_a_reload_closes_is_skipped_without_an_error() {
    let (mut nvim, scratch, file) = ui_with_timer("float");
    nvim.lua(
        "local main = vim.api.nvim_get_current_buf() \
         local scratch = vim.api.nvim_create_buf(false, true) \
         _G.float = vim.api.nvim_open_win(scratch, false, { relative = 'editor', row = 1, col = 1, width = 10, height = 2 }) \
         vim.api.nvim_create_autocmd('BufReadPost', { callback = function(a) \
           if a.buf == main then pcall(vim.api.nvim_win_close, _G.float, true) end end })",
        vec![],
    );
    nvim.command("messages clear");
    std::fs::write(&file, "DISK\nchange\n").unwrap();
    nvim.wait_until("the timer's reload", |n| n.eval("getline(1)").as_str() == Some("DISK"));
    std::thread::sleep(std::time::Duration::from_millis(RELOAD_INTERVAL_MS + 300));
    assert_eq!(
        ui_mode(&mut nvim),
        ("n".to_string(), false),
        "an error stopped nvim at a prompt"
    );
    let messages = nvim.lua(
        "return vim.api.nvim_exec2('messages', { output = true }).output",
        vec![],
    );
    assert_eq!(messages.as_str(), Some(""), "{messages:?}");
    assert_eq!(
        nvim.lua("return vim.api.nvim_win_is_valid(_G.float)", vec![]),
        rmpv::Value::from(false),
        "the autocommand closed the float"
    );
    nvim.teardown();
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// With `autoread` off the person chose no automatic reload: under a UI `checktime` would raise `W11`
/// as a dialog every tick. The timer leaves such a buffer alone.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_noautoread_buffer_meets_no_dialog_and_is_not_reloaded() {
    let (mut nvim, scratch, file) = ui_with_timer("noar");
    nvim.command("setlocal noautoread");
    std::fs::write(&file, "DISK\nchange\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2 * RELOAD_INTERVAL_MS + 500));
    assert_eq!(ui_mode(&mut nvim), ("n".to_string(), false), "a dialog met the buffer");
    assert_eq!(
        nvim.eval("getline(1)").as_str(),
        Some("one"),
        "a noautoread buffer was reloaded"
    );
    nvim.teardown();
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// The longest an `nvim_eval` took to answer over `window`: a pause inside nvim shows as one slow answer.
fn slowest_answer(nvim: &mut embed_client::Embed, window: std::time::Duration) -> std::time::Duration {
    let until = std::time::Instant::now() + window;
    let mut slowest = std::time::Duration::ZERO;
    while std::time::Instant::now() < until {
        let began = std::time::Instant::now();
        nvim.eval("1");
        slowest = slowest.max(began.elapsed());
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    slowest
}

fn the_next_key_reaches_nvim(nvim: &mut embed_client::Embed) {
    assert_eq!(ui_mode(nvim), ("n".to_string(), false), "a dialog met the buffer");
    nvim.input("i");
    nvim.wait_until("insert mode", |n| ui_mode(n).0 == "i");
    nvim.input("<Esc>");
    nvim.wait_until("normal mode", |n| ui_mode(n).0 == "n");
}

/// A file the agent creates after the person opened its missing path: `W13`, a dialog, would take the
/// next key. The timer had seen the file missing, so it leaves the buffer to nvim.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_file_created_after_the_buffer_was_opened_meets_no_dialog() {
    let (mut nvim, scratch, _file) = ui_with_timer("w13");
    let made = scratch.join("new.rs");
    nvim.command(&format!("edit {}", made.display()));
    std::thread::sleep(std::time::Duration::from_millis(RELOAD_INTERVAL_MS + 300));
    std::fs::write(&made, "made by the agent\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2 * RELOAD_INTERVAL_MS + 300));
    the_next_key_reaches_nvim(&mut nvim);
    nvim.teardown();
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// The same, with the file created before the timer's first look at the buffer.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_file_created_before_the_first_tick_meets_no_dialog() {
    let (mut nvim, scratch, _file) = ui_with_timer("w13first");
    let made = scratch.join("new.rs");
    nvim.command(&format!("edit {}", made.display()));
    std::fs::write(&made, "made by the agent\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2 * RELOAD_INTERVAL_MS + 300));
    the_next_key_reaches_nvim(&mut nvim);
    nvim.teardown();
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// The file `name` (made in `dir`, which nvim has as its working directory) is shown, unmodified, and
/// the timer is running with the reload chunk.
fn shown_file_with_timer(nvim: &mut embed_client::Embed, dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    nvim.teardown();
    nvim.command(&format!("cd {}", dir.display()));
    let file = dir.join(name);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "one\ntwo\n").unwrap();
    nvim.command(&format!("edit {name}"));
    nvim.inject(
        eitri_core::buffer_reload::RELOAD_CMD.trim_start_matches("lua "),
        embed_client::opts_map(&[]),
    );
    file
}

/// A shown file the agent deletes: nvim answers a check with `E211` and, in Normal mode, waits a second.
/// That wait is measured on the same nvim with a bare `checktime` first; the timer causes none, and a
/// file that comes back is reloaded.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_deleted_file_does_not_stall_nvim() {
    let (mut nvim, scratch, _file) = ui_with_timer("e211");
    let stock = shown_file_with_timer(&mut nvim, &scratch, "stock.txt");
    nvim.teardown();
    std::fs::remove_file(&stock).unwrap();
    let began = std::time::Instant::now();
    nvim.command("checktime");
    let stall = began.elapsed();
    assert!(
        stall >= std::time::Duration::from_millis(500),
        "calibration: a bare checktime on a deleted file waits ({stall:?})"
    );
    let file = shown_file_with_timer(&mut nvim, &scratch, "f.txt");
    nvim.command("silent! bwipeout! stock.txt");
    // A bare check clears nvim's "already warned" flag, which the calibration's warning set and which would
    // turn the next warning into a plain error message.
    nvim.command("checktime");

    std::fs::remove_file(&file).unwrap();
    let slowest = slowest_answer(
        &mut nvim,
        std::time::Duration::from_millis(2 * RELOAD_INTERVAL_MS + 500),
    );
    assert!(
        slowest < stall / 2,
        "nvim paused for {slowest:?}; a stalled check pauses about {stall:?}"
    );

    std::fs::write(&file, "back\n").unwrap();
    nvim.wait_until("the timer's reload", |n| n.eval("getline(1)").as_str() == Some("back"));
    the_next_key_reaches_nvim(&mut nvim);
    nvim.teardown();
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// The same deletion, where the file's name is long: `E211` does not fit on the command line and nvim
/// asks for a key (hit-enter) after the wait. The calibration shows a bare `checktime` doing that here.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_deleted_file_with_a_long_name_raises_no_prompt() {
    let (mut nvim, scratch, _file) = ui_with_timer("e211long");
    nvim.teardown();
    let dir =
        scratch.join("a-directory-name-long-enough-that-the-deleted-file-warning-needs-more-than-one-screen-line");
    std::fs::create_dir_all(&dir).unwrap();
    let stock = dir.join("stock.txt");
    std::fs::write(&stock, "stock\n").unwrap();
    nvim.command(&format!("edit {}", stock.display()));
    std::fs::remove_file(&stock).unwrap();
    nvim.lua("vim.schedule(function() vim.cmd('checktime') end)", vec![]);
    nvim.wait_until("calibration: a bare checktime on a deleted file asks for a key", |n| {
        ui_mode(n).1
    });
    nvim.input("<CR>");
    nvim.wait_until("normal mode", |n| ui_mode(n) == ("n".to_string(), false));
    nvim.command("silent! bwipeout! stock.txt");
    nvim.command("checktime");

    let file = dir.join("f.txt");
    std::fs::write(&file, "one\ntwo\n").unwrap();
    nvim.command(&format!("edit {}", file.display()));
    nvim.inject(
        eitri_core::buffer_reload::RELOAD_CMD.trim_start_matches("lua "),
        embed_client::opts_map(&[]),
    );
    std::fs::remove_file(&file).unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_millis(2 * RELOAD_INTERVAL_MS + 500);
    while std::time::Instant::now() < until {
        assert_eq!(
            ui_mode(&mut nvim),
            ("n".to_string(), false),
            "a prompt met the deletion"
        );
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    nvim.teardown();
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A file that holds one empty line is not a file that holds nothing: its buffer is reloaded when it changes.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_file_holding_only_a_newline_is_reloaded() {
    let (mut nvim, scratch, _file) = ui_with_timer("nl");
    let nl = scratch.join("nl.txt");
    std::fs::write(&nl, "\n").unwrap();
    nvim.command(&format!("edit {}", nl.display()));
    std::thread::sleep(std::time::Duration::from_millis(RELOAD_INTERVAL_MS + 300));
    std::fs::write(&nl, "DISK\n").unwrap();
    nvim.wait_until("the timer's reload", |n| n.eval("getline(1)").as_str() == Some("DISK"));
    the_next_key_reaches_nvim(&mut nvim);
    nvim.teardown();
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// Editing a buffer opened on a missing path and undoing it leaves it unmodified and still marked new by
/// nvim: the file that appears then is read, with no dialog.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_edited_and_undone_buffer_meets_no_dialog() {
    let (mut nvim, scratch, _file) = ui_with_timer("undo");
    let made = scratch.join("new.rs");
    nvim.command(&format!("edit {}", made.display()));
    std::thread::sleep(std::time::Duration::from_millis(RELOAD_INTERVAL_MS + 300));
    std::fs::write(&made, "made by the agent\n").unwrap();
    nvim.command("normal! ix");
    nvim.command("undo");
    assert_eq!(nvim.eval("&modified").as_i64(), Some(0));
    nvim.wait_until("the file is read", |n| {
        n.eval("getline(1)").as_str() == Some("made by the agent")
    });
    the_next_key_reaches_nvim(&mut nvim);
    nvim.teardown();
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A file created empty before the timer looked, and an empty file that was always there and is filled:
/// no dialog, and the buffer is read.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn buffers_over_empty_files_meet_no_dialog_and_are_read() {
    let (mut nvim, scratch, _file) = ui_with_timer("w13empty");
    let touched = scratch.join("touched.rs");
    nvim.command(&format!("edit {}", touched.display()));
    std::fs::write(&touched, "").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(RELOAD_INTERVAL_MS + 500));
    the_next_key_reaches_nvim(&mut nvim);
    std::fs::write(&touched, "filled later\n").unwrap();
    nvim.wait_until("the fill is read", |n| {
        n.eval("getline(1)").as_str() == Some("filled later")
    });

    let always = scratch.join("always.rs");
    std::fs::write(&always, "").unwrap();
    nvim.command(&format!("edit {}", always.display()));
    std::fs::write(&always, "filled before the first tick\n").unwrap();
    nvim.wait_until("the fill is read", |n| {
        n.eval("getline(1)").as_str() == Some("filled before the first tick")
    });
    the_next_key_reaches_nvim(&mut nvim);
    nvim.teardown();
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

fn timer_case(
    label: &str,
    env: &[(&str, &str)],
    case: fn(&mut embed_client::Embed, &std::path::Path, reload_cases::Tick<'_>),
) {
    let (mut nvim, scratch, _file) = ui_with_timer_env(label, env);
    case(&mut nvim, &scratch, &mut |_| {
        std::thread::sleep(std::time::Duration::from_millis(RELOAD_INTERVAL_MS + 400))
    });
    nvim.teardown();
    nvim.quit();
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_reread_in_a_split_that_is_not_the_current_window() {
    timer_case("c-split", &[], reload_cases::a_split_that_is_not_the_current_window);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_reread_of_a_file_shown_only_in_a_second_tab_page() {
    timer_case("c-tab", &[], reload_cases::a_second_tab_page);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_failed_reread_is_not_followed_by_a_check() {
    timer_case(
        "c-cmdwin",
        &[],
        reload_cases::the_command_line_window_open_when_the_file_appears,
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_reread_meets_no_swap_file_dialog() {
    timer_case(
        "c-swap",
        &[("EMBED_KEEP_SWAP", "1")],
        reload_cases::a_swap_file_left_by_another_nvim,
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_fifo_is_not_read_again() {
    timer_case("c-fifo", &[], reload_cases::a_fifo);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn undo_after_a_reread_brings_back_the_empty_buffer() {
    timer_case("c-undo", &[], reload_cases::undo_after_a_reread);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_reread_leaves_the_directory_options_and_events_alone() {
    timer_case("c-narrow", &[], reload_cases::the_persons_directory_options_and_events);
}
