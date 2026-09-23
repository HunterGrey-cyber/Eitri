//! THE BANDWIDTH MEASUREMENT. Real PTYs, real children, 120x40.
//!
//! These tests fork external programs and take tens of seconds, so they are
//! `#[ignore]`d:
//!
//! ```sh
//! cargo test --release --test bandwidth -- --ignored --nocapture --test-threads=1
//! ```
//!
//! ON FAILABILITY
//! --------------
//! `the_bandwidth_gate_is_runnable_here` is deliberately NOT `#[ignore]`d. Every
//! other test in this file is, so a plain `cargo test` is green while the whole
//! measurement silently does not run -- and a measurement that can silently not
//! run is worse than no measurement, because the README still quotes numbers
//! from it. The sentinel asserts the gate is RUNNABLE; if the programs are
//! missing, that is a red test, not a quiet skip.
//!
//! Every measuring test also asserts a floor on what it observed (frames seen,
//! PTY bytes read, screen actually touched), so "0 frames, 0 bytes, 0 B/s" can
//! never be reported as a wonderfully cheap workload.

mod support;

use std::time::Duration;

use support::{base_env, have, measure, Measurement, Size, Step};

/// The plan's canonical geometry.
const SIZE: Size = Size { cols: 120, rows: 40 };

fn report(measurements: &[Measurement]) {
    eprintln!("\n{}", measurements[0].header());
    for measurement in measurements {
        eprintln!("{}", measurement.row());
    }
    eprintln!();
    for measurement in measurements {
        eprintln!(
            "{:<26} {:.2}s  pty {} reads / {} B  cells {}  full {:.0}%  largest rle frame {} B  \
             sync updates {}",
            measurement.name,
            measurement.seconds,
            measurement.pty_reads,
            measurement.pty_bytes,
            measurement.cells,
            measurement.full_fraction() * 100.0,
            measurement.largest_rle_frame,
            measurement.sync_updates,
        );
        if measurement.delta_frames > 0 {
            eprintln!(
                "{:<26}   delta-only: naive {:.0} B/f, rle {:.0} B/f     full-only: naive {:.0} \
                 B/f, rle {:.0} B/f",
                "",
                measurement.naive_delta_bytes as f64 / measurement.delta_frames as f64,
                measurement.rle_delta_bytes as f64 / measurement.delta_frames as f64,
                measurement.naive_full_bytes as f64 / measurement.full_frames.max(1) as f64,
                measurement.rle_full_bytes as f64 / measurement.full_frames.max(1) as f64,
            );
        }
    }
    eprintln!();
}

fn sane(measurement: &Measurement) {
    assert!(measurement.frames > 0, "{}: no frames at all", measurement.name);
    assert!(
        measurement.pty_bytes > 0,
        "{}: the child produced no output",
        measurement.name
    );
    assert!(
        measurement.cells > 0,
        "{}: no cells were ever carried",
        measurement.name
    );
    assert_eq!(
        measurement.frames,
        measurement.full_frames + measurement.delta_frames,
        "{}: frame kinds do not add up",
        measurement.name
    );
    assert!(
        measurement.rle_bytes <= measurement.naive_bytes,
        "{}: the RLE encoding was larger than the naive one",
        measurement.name
    );
}

// ===========================================================================
// (a) idle typing at a shell prompt
// ===========================================================================

fn typing_script() -> Vec<Step> {
    let mut script = vec![Step::Wait(Duration::from_millis(600))];
    // 40 keystrokes at ~60 ms, i.e. a fast human typing a command, then Enter.
    const KEYS: &[&[u8]] = &[
        b"e", b"c", b"h", b"o", b" ", b"t", b"h", b"e", b" ", b"q", b"u", b"i", b"c", b"k", b" ", b"b", b"r", b"o",
        b"w", b"n", b" ", b"f", b"o", b"x", b" ", b"j", b"u", b"m", b"p", b"s", b" ", b"o", b"v", b"e", b"r", b" ",
        b"i", b"t", b"\r",
    ];
    for key in KEYS {
        script.push(Step::Send(key));
        script.push(Step::Wait(Duration::from_millis(60)));
    }
    script.push(Step::Wait(Duration::from_millis(400)));
    script
}

#[test]
#[ignore = "forks a real bash on a real PTY"]
fn workload_a_idle_typing_at_a_shell_prompt() {
    let mut env = base_env();
    env.push(("PS1", "verdandi$ "));
    let measurement = measure(
        "(a) shell: idle typing",
        "bash",
        &["--norc", "--noprofile", "-i"],
        &env,
        SIZE,
        &typing_script(),
        Duration::from_millis(300),
    )
    .expect("spawn bash");
    report(std::slice::from_ref(&measurement));
    sane(&measurement);
    assert!(
        measurement.frames >= 30,
        "only {} frames for 39 keystrokes; the shell was not echoing",
        measurement.frames
    );
}

/// A shell that actually RUNS things, so the screen scrolls.
///
/// This is the workload the earlier "full=13 / partial=7 over 20 samples"
/// observation came from, and it is kept separate from (a) because the two give
/// opposite answers: the Full/Partial split is decided entirely by whether the
/// workload scrolls, not by how busy it is.
fn command_script() -> Vec<Step> {
    let mut script = vec![Step::Wait(Duration::from_millis(600))];
    // Fill the screen FIRST. Nothing scrolls until the cursor reaches the last
    // row, and at 120x40 a handful of short commands never gets there -- which
    // is itself the point: whether a shell session produces Full frames depends
    // on whether the screen is full yet.
    script.push(Step::Send(b"seq 1 60\r"));
    script.push(Step::Wait(Duration::from_millis(500)));
    const COMMANDS: &[&[u8]] = &[
        b"echo one two three\r",
        b"pwd\r",
        b"printf 'a\\nb\\nc\\n'\r",
        b"echo $((6*7))\r",
        b"seq 1 12\r",
        b"echo done\r",
        b"true\r",
        b"echo last\r",
    ];
    for command in COMMANDS {
        script.push(Step::Send(command));
        script.push(Step::Wait(Duration::from_millis(250)));
    }
    script
}

#[test]
#[ignore = "forks a real bash on a real PTY"]
fn workload_a2_shell_running_commands_is_dominated_by_full_frames() {
    let mut env = base_env();
    env.push(("PS1", "verdandi$ "));
    let measurement = measure(
        "(a2) shell: commands",
        "bash",
        &["--norc", "--noprofile", "-i"],
        &env,
        SIZE,
        &command_script(),
        Duration::from_millis(300),
    )
    .expect("spawn bash");
    report(std::slice::from_ref(&measurement));
    sane(&measurement);
    // The correction to the earlier reading: it is SCROLLING that forces Full,
    // via `Term::scroll_up_relative` -> `mark_fully_damaged`. A shell that runs
    // commands scrolls; a shell you are only typing at does not.
    assert!(
        measurement.full_frames > 0,
        "a shell that scrolls must produce Full frames; it produced none"
    );
}

// ===========================================================================
// (b) a redraw-heavy TUI
// ===========================================================================

#[test]
#[ignore = "forks a real nvim on a real PTY"]
fn workload_b_redraw_heavy_tui_nvim() {
    let mut env = base_env();
    env.push(("NVIM_APPNAME", "terminal-frame-bandwidth"));
    // Generate a file to scroll, then scroll it hard.
    let mut script = vec![Step::Wait(Duration::from_millis(1200))];
    // 400 lines of content.
    script.push(Step::Send(b"i"));
    script.push(Step::Wait(Duration::from_millis(200)));
    for _ in 0..40 {
        script.push(Step::Send(
            b"the quick brown fox jumps over the lazy dog 0123456789 abcdefghij\r",
        ));
        script.push(Step::Wait(Duration::from_millis(30)));
    }
    script.push(Step::Send(b"\x1b"));
    script.push(Step::Wait(Duration::from_millis(200)));
    script.push(Step::Send(b"yy200p"));
    script.push(Step::Wait(Duration::from_millis(1500)));
    script.push(Step::Send(b"gg"));
    script.push(Step::Wait(Duration::from_millis(300)));
    // Page down repeatedly: whole-screen redraws, which is the point.
    for _ in 0..60 {
        script.push(Step::Send(b"\x06")); // Ctrl-F
        script.push(Step::Wait(Duration::from_millis(40)));
    }
    script.push(Step::Wait(Duration::from_millis(400)));

    let measurement = measure(
        "(b) nvim: page-down burst",
        "nvim",
        &["--clean", "-n", "-i", "NONE"],
        &env,
        SIZE,
        &script,
        Duration::from_millis(500),
    )
    .expect("spawn nvim");
    report(std::slice::from_ref(&measurement));
    sane(&measurement);
    assert!(
        measurement.frames >= 100,
        "only {} frames from nvim",
        measurement.frames
    );
    assert!(
        measurement.cells > 50_000,
        "only {} cells; nvim was not redrawing whole screens",
        measurement.cells
    );
}

#[test]
#[ignore = "runs a spinner script on a real PTY"]
fn workload_b_redraw_heavy_tui_spinner() {
    // A pure-POSIX spinner: no dependency on any editor being installed, and a
    // clean high-frequency small-delta workload. It also uses DECSET 2026, so
    // the sync-update count is non-zero and the "frames per read vs frames per
    // update" difference is visible.
    const SCRIPT: &str = r#"
printf '\033[2J\033[H'
i=0
while [ $i -lt 400 ]; do
  i=$((i+1))
  printf '\033[?2026h\033[5;10Hframe %04d  \033[6;10H[%s]\033[?2026l' "$i" "$(printf '#%.0s' $(seq 1 $((i % 40))))"
  sleep 0.01
done
"#;
    let measurement = measure(
        "(b) spinner: small deltas",
        "sh",
        &["-c", SCRIPT],
        &base_env(),
        SIZE,
        &[Step::Wait(Duration::from_secs(9))],
        Duration::from_millis(200),
    )
    .expect("spawn sh");
    report(std::slice::from_ref(&measurement));
    sane(&measurement);
    assert!(
        measurement.frames >= 100,
        "only {} frames from the spinner",
        measurement.frames
    );
    assert!(
        measurement.sync_updates >= 100,
        "only {} DECSET 2026 updates; the spinner is not exercising the sync path",
        measurement.sync_updates
    );
}

#[test]
#[ignore = "starts the real Claude Code TUI and lets it idle -- submits no turn, costs no credits"]
fn workload_b_redraw_heavy_tui_claude_code() {
    // THE ACTUAL TARGET. Starting the TUI and letting it sit is free; nothing
    // here submits a turn.
    let mut env = base_env();
    env.push(("PATH", "/home/user/.local/bin:/usr/bin:/bin:/usr/local/bin"));
    let measurement = measure(
        "(b) claude: TUI idling",
        "claude",
        &[],
        &env,
        SIZE,
        &[Step::Wait(Duration::from_secs(12))],
        Duration::from_millis(500),
    )
    .expect("spawn claude");
    report(std::slice::from_ref(&measurement));
    sane(&measurement);
}

// ===========================================================================
// (c) a scrolling burst
// ===========================================================================

#[test]
#[ignore = "runs seq 1 20000 on a real PTY"]
fn workload_c_scrolling_burst() {
    let measurement = measure(
        "(c) seq 1 20000",
        "sh",
        &["-c", "seq 1 20000"],
        &base_env(),
        SIZE,
        &[Step::Wait(Duration::from_secs(6))],
        Duration::from_millis(300),
    )
    .expect("spawn sh");
    report(std::slice::from_ref(&measurement));
    sane(&measurement);
    assert!(
        measurement.pty_bytes > 100_000,
        "only {} PTY bytes; seq did not run to completion",
        measurement.pty_bytes
    );
    assert_eq!(
        measurement.full_frames, measurement.frames,
        "a scrolling burst should be ALL full frames: scrolling marks full damage"
    );
}

// ===========================================================================
// Everything at once, for the README table.
// ===========================================================================

#[test]
#[ignore = "runs every workload; this is the one that produces the README table"]
fn all_workloads() {
    let mut out = Vec::new();

    let mut env = base_env();
    env.push(("PS1", "verdandi$ "));
    out.push(
        measure(
            "(a) shell: idle typing",
            "bash",
            &["--norc", "--noprofile", "-i"],
            &env,
            SIZE,
            &typing_script(),
            Duration::from_millis(300),
        )
        .expect("bash"),
    );

    out.push(
        measure(
            "(a2) shell: commands",
            "bash",
            &["--norc", "--noprofile", "-i"],
            &env,
            SIZE,
            &command_script(),
            Duration::from_millis(300),
        )
        .expect("bash"),
    );

    const SPINNER: &str = r#"
printf '\033[2J\033[H'
i=0
while [ $i -lt 400 ]; do
  i=$((i+1))
  printf '\033[?2026h\033[5;10Hframe %04d  \033[6;10H[%s]\033[?2026l' "$i" "$(printf '#%.0s' $(seq 1 $((i % 40))))"
  sleep 0.01
done
"#;
    out.push(
        measure(
            "(b) spinner: small deltas",
            "sh",
            &["-c", SPINNER],
            &base_env(),
            SIZE,
            &[Step::Wait(Duration::from_secs(9))],
            Duration::from_millis(200),
        )
        .expect("sh"),
    );

    let mut nvim_env = base_env();
    nvim_env.push(("NVIM_APPNAME", "terminal-frame-bandwidth"));
    let mut nvim_script = vec![Step::Wait(Duration::from_millis(1200)), Step::Send(b"i")];
    nvim_script.push(Step::Wait(Duration::from_millis(200)));
    for _ in 0..40 {
        nvim_script.push(Step::Send(
            b"the quick brown fox jumps over the lazy dog 0123456789 abcdefghij\r",
        ));
        nvim_script.push(Step::Wait(Duration::from_millis(30)));
    }
    nvim_script.push(Step::Send(b"\x1b"));
    nvim_script.push(Step::Wait(Duration::from_millis(200)));
    nvim_script.push(Step::Send(b"yy200p"));
    nvim_script.push(Step::Wait(Duration::from_millis(1500)));
    nvim_script.push(Step::Send(b"gg"));
    nvim_script.push(Step::Wait(Duration::from_millis(300)));
    for _ in 0..60 {
        nvim_script.push(Step::Send(b"\x06"));
        nvim_script.push(Step::Wait(Duration::from_millis(40)));
    }
    out.push(
        measure(
            "(b) nvim: page-down burst",
            "nvim",
            &["--clean", "-n", "-i", "NONE"],
            &nvim_env,
            SIZE,
            &nvim_script,
            Duration::from_millis(500),
        )
        .expect("nvim"),
    );

    let mut claude_env = base_env();
    claude_env.push(("PATH", "/home/user/.local/bin:/usr/bin:/bin:/usr/local/bin"));
    out.push(
        measure(
            "(b) claude: TUI idling",
            "claude",
            &[],
            &claude_env,
            SIZE,
            &[Step::Wait(Duration::from_secs(12))],
            Duration::from_millis(500),
        )
        .expect("claude"),
    );

    out.push(
        measure(
            "(c) seq 1 20000",
            "sh",
            &["-c", "seq 1 20000"],
            &base_env(),
            SIZE,
            &[Step::Wait(Duration::from_secs(6))],
            Duration::from_millis(300),
        )
        .expect("seq"),
    );

    report(&out);
    for measurement in &out {
        sane(measurement);
    }
}

// ===========================================================================
// UNGATED SENTINEL
// ===========================================================================

/// Deliberately NOT `#[ignore]`d. See the module docs.
#[test]
fn the_bandwidth_gate_is_runnable_here() {
    let missing: Vec<&str> = ["bash", "sh", "nvim", "seq", "claude"]
        .into_iter()
        .filter(|p| !have(p))
        .collect();
    assert!(
        missing.is_empty(),
        "the bandwidth gate cannot run here -- missing: {missing:?}. Every other test in this \
         file is #[ignore]d, so without these the suite is green while the README quotes \
         numbers nothing produced. Install them, or run \
         `cargo test --release --test bandwidth -- --ignored --nocapture --test-threads=1` \
         elsewhere and record the result."
    );
}

/// A second sentinel, for the thing a missing binary cannot catch: the meter
/// itself silently measuring nothing. Runs a trivial child, ungated, in under a
/// second.
#[test]
fn the_meter_actually_meters() {
    let measurement = measure(
        "sentinel",
        "sh",
        &["-c", "printf 'hello from a real pty\\n'; sleep 0.2"],
        &base_env(),
        Size { cols: 40, rows: 6 },
        &[Step::Wait(Duration::from_millis(600))],
        Duration::from_millis(100),
    )
    .expect("spawn sh");
    assert!(
        measurement.pty_bytes >= 20,
        "meter read {} bytes",
        measurement.pty_bytes
    );
    assert!(measurement.frames >= 1, "meter produced {} frames", measurement.frames);
    assert!(measurement.rle_bytes > 0 && measurement.naive_bytes > 0);
    assert!(measurement.rle_bytes <= measurement.naive_bytes);
    assert!(measurement.seconds > 0.0);
}
