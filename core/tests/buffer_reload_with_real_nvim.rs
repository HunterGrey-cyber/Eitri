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

/// A buffer the user has edited is never clobbered. nvim's own rule, asserted here because wire 3
/// runs `checktime` unconditionally and this is the reason that is safe: measured 2026-09-18, a
/// modified buffer gets a `W12` warning and keeps its edits.
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
