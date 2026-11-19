mod headless_common;
use eitri_terminal::headless::{DetachWhy, HeadlessTerminal, StreamItem};
use eitri_terminal::{Screen, SpawnExtras, SpawnSpec, TerminalColors};
use headless_common::*;
use std::time::Duration;

#[test]
fn exit_status_is_reported() {
    let term = HeadlessTerminal::spawn(config("printf done; exit 7")).unwrap();
    let a = term.attach("one".into()).unwrap();
    loop {
        if let StreamItem::Exited(exit) = next(&a) {
            assert_eq!(exit.code, Some(7));
            break;
        }
    }
    assert_eq!(term.close(Duration::from_millis(100)).unwrap().code, Some(7));
}

#[test]
fn a_second_attach_replaces_the_first_and_drops_its_unwritten_input() {
    let term = HeadlessTerminal::spawn(python(
        r#"
import os, signal, tty
tty.setraw(0)
os.write(1, b'READY')
os.kill(os.getpid(), signal.SIGSTOP)
data = bytearray()
while not data.endswith(b'!'):
    data.extend(os.read(0, 4096))
os.write(1, ('COUNT:%d' % data.count(65)).encode())
"#,
    ))
    .unwrap();
    wait_for(|| term.status().offset == 5);
    let a = term.attach("one".into()).unwrap();
    snapshot(&a);
    term.input(&"one".into(), 0, vec![b'A'; 128 * 1024]);
    let b = term.attach("two".into()).unwrap();
    loop {
        if let StreamItem::Detached(why) = next(&a) {
            assert_eq!(why, DetachWhy::Replaced);
            break;
        }
    }
    assert_eq!(snapshot(&b).input_written, 0);
    // Reattaching the old id exposes exactly how much reached the PTY before replacement.
    let c = term.attach("one".into()).unwrap();
    let written = snapshot(&c).input_written;
    assert!(written < 128 * 1024);
    let d = term.attach("two".into()).unwrap();
    snapshot(&d);
    drop(a);
    drop(b);
    drop(c);
    term.input(&"one".into(), 128 * 1024, vec![b'A'; 100]);
    assert_eq!(unsafe { libc::kill(term.status().pid as i32, libc::SIGCONT) }, 0);
    term.input(&"two".into(), 0, vec![b'!']);
    let mut out = Vec::new();
    loop {
        match next(&d) {
            StreamItem::Output { bytes, .. } => out.extend(bytes),
            StreamItem::Exited(_) => break,
            _ => {}
        }
    }
    assert_eq!(String::from_utf8(out).unwrap(), format!("COUNT:{written}"));
    finish(term);
}

#[test]
fn osc52_bell_and_title_notifications_have_no_effect_headless() {
    let mut s = Screen::headless(size(80, 24), TerminalColors::default());
    let bytes = b"\x1b]52;c;aGk=\x07\x07\x1b]2;kept title\x07\x1b[c";
    let mut control = Screen::new(size(80, 24), TerminalColors::default());
    control.feed(bytes);
    let events = control.take_events();
    assert_eq!(events.clipboard.as_deref(), Some("hi"));
    assert!(events.bell);
    assert_eq!(events.title, Some(Some("kept title".into())));
    s.feed(bytes);
    assert_eq!(s.take_events(), Default::default());
    assert!(!s.take_replies().is_empty());
    let replay = canonical(&mut s);
    assert!(replay.bytes.windows(10).any(|w| w == b"kept title"));
}

#[test]
fn spawn_exact_env_arg0_umask_and_cwd_fd() {
    use eitri_terminal::PtyChild;
    use std::fs::File;
    let mut s = spec("printf '%s\n' \"$0\"; /usr/bin/env; umask; pwd");
    s.cwd = "/a-directory-that-does-not-exist".into();
    s.env = vec![("UNWANTED_SPEC".into(), "bad".into())];
    s.extra_env = vec![("UNWANTED_EXTRA".into(), "bad".into())];
    let extras = SpawnExtras {
        exact_env: Some(vec![("ONLY_GIVEN".into(), "yes".into())]),
        arg0: Some("login-shell".into()),
        umask: Some(0o027),
        cwd_fd: Some(File::open("/").unwrap().into()),
    };
    // A script's $0 comes from its argument; argv[0] itself is checked against a directly executed binary below.
    s.args.push("script-name".into());
    let mut pty = PtyChild::spawn_with(&s, size(80, 24), &extras).unwrap();
    let bytes = read_child(&mut pty);
    assert!(pty.wait().unwrap().success());
    let text = String::from_utf8(bytes).unwrap().replace('\r', "");
    assert!(text.starts_with("script-name\n"), "{text}");
    assert!(text.ends_with("0027\n/\n"), "{text}");
    for line in text.lines().skip(1).take_while(|line| line.contains('=')) {
        assert!(
            line == "ONLY_GIVEN=yes" || line == "PWD=/" || line.starts_with("SHLVL=") || line.starts_with("_="),
            "unexpected inherited variable: {line}"
        );
    }
    // env does not add PWD, so it witnesses an exact environment independently of shell startup.
    let mut env = SpawnSpec::shell("/usr/bin/env".into(), "/tmp".into());
    env.args = vec!["-0".into()];
    let mut pty = PtyChild::spawn_with(&env, size(80, 24), &extras).unwrap();
    let bytes = read_child(&mut pty);
    assert!(pty.wait().unwrap().success());
    assert_eq!(bytes, b"ONLY_GIVEN=yes\0");
    // bash preserves the supplied argv[0] in $0 when no script name follows -c.
    let mut shell = spec("printf '%s' \"$0\"");
    shell.program = "/bin/bash".into();
    let mut pty = PtyChild::spawn_with(&shell, size(80, 24), &extras).unwrap();
    let bytes = read_child(&mut pty);
    assert!(pty.wait().unwrap().success());
    assert_eq!(bytes, b"login-shell");
}

fn read_child(pty: &mut eitri_terminal::PtyChild) -> Vec<u8> {
    let until = std::time::Instant::now() + WAIT;
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        assert!(std::time::Instant::now() < until, "PTY did not close");
        match pty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(2)),
            Err(e) => panic!("PTY read: {e}"),
        }
    }
    out
}

#[test]
fn inherited_spawn_is_unchanged() {
    use eitri_terminal::{child_environment, PtyChild, SpawnSpec};
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let mut spawn = SpawnSpec::shell("/usr/bin/env".into(), "/tmp".into());
    spawn.args = vec!["-0".into()];
    let mut pty = PtyChild::spawn(&spawn, size(80, 24)).unwrap();
    let bytes = read_child(&mut pty);
    pty.wait().unwrap();
    let got: BTreeMap<_, _> = bytes
        .split(|b| *b == 0)
        .filter(|b| !b.is_empty())
        .map(|pair| {
            let at = pair.iter().position(|b| *b == b'=').unwrap();
            (
                OsString::from_vec(pair[..at].to_vec()),
                OsString::from_vec(pair[at + 1..].to_vec()),
            )
        })
        .collect();
    let expected: BTreeMap<_, _> = child_environment(std::env::vars_os()).into_iter().collect();
    assert_eq!(got, expected);
}

#[test]
fn snapshot_then_stream_equals_one_uninterrupted_screen() {
    for bytes in corpus() {
        let mut seed = 123456789u64;
        for _ in 0..50 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let at = seed as usize % (bytes.len() + 1);
            let mut c = python(
                r#"
import os, sys, tty
tty.setraw(0)
def write(data):
    while data:
        data = data[os.write(1, data):]
write(b'READY')
data = bytes.fromhex(sys.argv[1])
at = int(sys.argv[2])
write(data[:at])
os.read(0, 1)
write(data[at:])
"#,
            );
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            c.spawn.args.extend([hex.into(), at.to_string().into()]);
            let term = HeadlessTerminal::spawn(c).unwrap();
            wait_for(|| term.status().offset == 5 + at as u64);
            let a = term.attach("one".into()).unwrap();
            let mut sent = false;
            let s = loop {
                match next(&a) {
                    StreamItem::Waiting(_) => {
                        if !sent {
                            term.input(&"one".into(), 0, vec![b'!']);
                            sent = true;
                        }
                    }
                    StreamItem::Snapshot(s) => break s,
                    StreamItem::InputWritten(1) if sent => {}
                    item => panic!("unexpected item before snapshot: {item:?}"),
                }
            };
            let mut mirror = screen(&s);
            let mut offset = s.offset;
            if !sent {
                term.input(&"one".into(), 0, vec![b'!']);
            }
            loop {
                match next(&a) {
                    StreamItem::Output { offset: at, bytes } => {
                        assert_eq!(at, offset);
                        offset += bytes.len() as u64;
                        mirror.feed(&bytes);
                    }
                    StreamItem::Exited(_) => break,
                    StreamItem::InputWritten(_) => {}
                    item => panic!("unexpected item after snapshot: {item:?}"),
                }
            }
            assert_eq!(offset, 5 + bytes.len() as u64);
            let mut twin = Screen::new(size(80, 24), TerminalColors::default());
            twin.feed(b"READY");
            twin.feed(&bytes);
            assert_eq!(mirror.render(true), twin.render(true));
            assert_eq!(canonical(&mut mirror), canonical(&mut twin));
            finish(term);
        }
    }
}

#[test]
fn an_open_sequence_defers_the_snapshot_and_parsing_continues() {
    use eitri_terminal::headless::WaitWhy;
    let term = HeadlessTerminal::spawn(config(r#"stty raw -echo; printf '\033]2;'; head -c 204800 /dev/zero | tr '\000' x; dd bs=1 count=1 of=/dev/null 2>/dev/null; printf more; dd bs=1 count=1 of=/dev/null 2>/dev/null; printf '\007done'"#)).unwrap();
    wait_for(|| term.status().offset == 204804);
    let a = term.attach("one".into()).unwrap();
    assert!(matches!(next(&a), StreamItem::Waiting(WaitWhy::OpenSequence)));
    let before = term.status().offset;
    term.input(&"one".into(), 0, vec![b'a']);
    wait_for(|| term.status().offset == before + 4);
    assert!(term.status().alive);
    assert!(matches!(next(&a), StreamItem::InputWritten(1)));
    assert!(a.next().is_none(), "the sequence remains open while output is counted");
    term.input(&"one".into(), 1, vec![b'b']);
    let s = snapshot(&a);
    assert!(s.offset >= before + 5);
    loop {
        if matches!(next(&a), StreamItem::Exited(_)) {
            break;
        }
    }
    finish(term);
}

#[test]
fn too_deep_waits_and_recovers() {
    use eitri_terminal::headless::WaitWhy;
    let term = HeadlessTerminal::spawn(config(r#"stty raw -echo; i=0; while [ "$i" -lt 4096 ]; do printf '\033[>1u'; i=$((i+1)); done; dd bs=1 count=1 of=/dev/null 2>/dev/null; printf '\033[<1u'; sleep 30"#)).unwrap();
    wait_for(|| term.status().offset >= 4096 * 5);
    let a = term.attach("one".into()).unwrap();
    assert!(matches!(next(&a), StreamItem::Waiting(WaitWhy::TooDeep)));
    term.input(&"one".into(), 0, vec![b'a']);
    snapshot(&a);
    finish(term);
}

#[test]
fn queries_are_answered_with_no_client() {
    let term = HeadlessTerminal::spawn(config(
        r#"stty raw -echo; printf '\033[c'; dd bs=1 count=5 2>/dev/null | od -An -tx1"#,
    ))
    .unwrap();
    wait_for(|| !term.status().alive);
    let a = term.attach("one".into()).unwrap();
    let s = snapshot(&a);
    let text = String::from_utf8_lossy(&s.replay.bytes);
    assert!(text.contains("1b 5b 3f 36 63"), "{text}");
    finish(term);
}

#[test]
fn output_with_no_client_is_in_the_next_snapshot() {
    for count in [500, 12_000] {
        let term = HeadlessTerminal::spawn(config(&format!(
            "i=0; while [ $i -lt {count} ]; do printf '%05d\\n' $i; i=$((i+1)); done"
        )))
        .unwrap();
        wait_for(|| !term.status().alive);
        let a = term.attach("one".into()).unwrap();
        let s = snapshot(&a);
        assert!(s.output_while_detached);
        assert_eq!(s.history_at_cap, count == 12_000);
        let mut rebuilt = screen(&s);
        let replay = canonical(&mut rebuilt);
        let history = (count + 1usize).saturating_sub(24).min(10_000);
        assert_eq!(replay.bytes.windows(2).filter(|w| *w == b"\r\n").count(), history + 23);
        assert!(String::from_utf8_lossy(&replay.bytes).contains(&format!("{:05}", count - 1)));
        finish(term);
    }
}

#[test]
fn resize_marker_sits_where_the_resize_applied() {
    let term = HeadlessTerminal::spawn(config(
        r#"stty raw -echo; printf READY; while read line; do printf '%s\n' "$line"; done"#,
    ))
    .unwrap();
    wait_for(|| term.status().offset == 5);
    let a = term.attach("one".into()).unwrap();
    let first = snapshot(&a);
    let mut mirror = screen(&first);
    let mut offset = first.offset;
    let mut input = 0;
    for (i, columns) in [40, 91, 65, 120, 80].into_iter().enumerate() {
        let text = format!("{}\n", "x".repeat(150 + i));
        let end = offset + text.len() as u64;
        term.input(&"one".into(), input, text.as_bytes().to_vec());
        input += text.len() as u64;
        term.resize(size(columns, 24));
        let mut resized = false;
        while !resized || offset < end {
            match next(&a) {
                StreamItem::Output { offset: at, bytes } => {
                    assert_eq!(at, offset);
                    offset += bytes.len() as u64;
                    mirror.feed(&bytes);
                }
                StreamItem::Resized { offset: at, size } => {
                    assert_eq!(at, offset);
                    mirror.resize(size);
                    resized = true;
                }
                _ => {}
            }
        }
    }
    let b = term.attach("two".into()).unwrap();
    let final_state = snapshot(&b);
    assert_eq!(final_state.offset, offset);
    assert_eq!(canonical(&mut mirror), final_state.replay);
    finish(term);
}

fn python(script: &str) -> eitri_terminal::HeadlessConfig {
    let mut c = config("");
    c.spawn.program = "/usr/bin/python3".into();
    c.spawn.args = vec!["-c".into(), script.into()];
    c
}

#[test]
fn input_after_a_stream_ends_is_never_written() {
    let term = HeadlessTerminal::spawn(python(
        r#"
import os, signal, tty
tty.setraw(0)
os.write(1, b'READY')
os.kill(os.getpid(), signal.SIGSTOP)
data = bytearray()
while not data.endswith(b'!'):
    data.extend(os.read(0, 4096))
os.write(1, ('COUNT:%d' % data.count(65)).encode())
"#,
    ))
    .unwrap();
    wait_for(|| term.status().offset == 5);
    let a = term.attach("one".into()).unwrap();
    snapshot(&a);
    term.input(&"one".into(), 0, vec![b'A'; 128 * 1024]);
    let written = loop {
        if let StreamItem::InputWritten(n) = next(&a) {
            break n;
        }
    };
    assert!(written > 0 && written < 128 * 1024);
    term.detach(&"one".into());
    term.input(&"one".into(), 128 * 1024, vec![b'A'; 100]);
    wait_for(|| term.status().attached.is_none());
    let b = term.attach("one".into()).unwrap();
    let state = snapshot(&b);
    assert!(state.input_written >= written && state.input_written < 128 * 1024);
    let written = state.input_written;
    assert_eq!(unsafe { libc::kill(term.status().pid as i32, libc::SIGCONT) }, 0);
    term.input(&"one".into(), written, vec![b'!']);
    let mut out = Vec::new();
    loop {
        match next(&b) {
            StreamItem::Output { bytes, .. } => out.extend(bytes),
            StreamItem::Exited(_) => break,
            _ => {}
        }
    }
    assert_eq!(String::from_utf8(out).unwrap(), format!("COUNT:{written}"));
    finish(term);
}

#[test]
fn input_written_counts_partial_writes() {
    let term = HeadlessTerminal::spawn(config(
        r#"stty raw -echo; printf READY; dd bs=1 count=65536 2>/dev/null | tr -cd a | wc -c"#,
    ))
    .unwrap();
    wait_for(|| term.status().offset == 5);
    let a = term.attach("one".into()).unwrap();
    snapshot(&a);
    // A gap is rejected and does not move the next expected offset.
    term.input(&"one".into(), 4, vec![b'!']);
    term.input(&"other".into(), 0, vec![b'!']);
    term.input(&"one".into(), 0, vec![b'a'; 65536]);
    term.input(&"one".into(), 0, vec![b'!']);
    let mut written = 0;
    let mut output = Vec::new();
    loop {
        match next(&a) {
            StreamItem::InputWritten(n) => {
                assert!(n > written && n <= 65536);
                written = n;
            }
            StreamItem::Output { bytes, .. } => output.extend(bytes),
            StreamItem::Exited(_) => break,
            _ => {}
        }
    }
    assert_eq!(written, 65536);
    assert_eq!(String::from_utf8(output).unwrap().trim(), "65536");
    let b = term.attach("one".into()).unwrap();
    assert_eq!(snapshot(&b).input_written, 65536);
    finish(term);
}

#[test]
fn job_and_cwd_follow_the_foreground() {
    let mut c = config("");
    c.spawn.args = vec!["-i".into()];
    c.extras.exact_env = Some(vec![
        ("PS1".into(), "READY> ".into()),
        ("PATH".into(), "/usr/bin:/bin".into()),
    ]);
    let term = HeadlessTerminal::spawn(c).unwrap();
    wait_for(|| term.status().offset > 0 && term.status().job.is_some_and(|j| !j.busy));
    let a = term.attach("one".into()).unwrap();
    snapshot(&a);
    let command = b"cd /tmp; sleep 30\n";
    term.input(&"one".into(), 0, command.to_vec());
    wait_for(|| {
        term.status()
            .job
            .is_some_and(|j| j.busy && j.foreground_name.as_deref() == Some(std::ffi::OsStr::new("sleep")))
    });
    assert_eq!(term.status().cwd.as_deref(), Some(std::path::Path::new("/tmp")));
    term.input(&"one".into(), command.len() as u64, vec![3]);
    wait_for(|| term.status().job.is_some_and(|j| !j.busy));
    finish(term);
}

static HEAVY: std::sync::Mutex<()> = std::sync::Mutex::new(());
const FLOOD: usize = 200 * 1024 * 1024;
fn flood(count: usize) -> String {
    format!("head -c {count} /dev/zero | tr '\\000' x")
}

#[test]
fn a_client_that_reads_nothing_bounds_memory_and_gets_one_snapshot() {
    let _heavy = HEAVY.lock().unwrap_or_else(|e| e.into_inner());
    let term = HeadlessTerminal::spawn(config(&flood(FLOOD))).unwrap();
    let a = term.attach("one".into()).unwrap();
    wait_for(|| !term.status().alive);
    assert_eq!(term.status().offset, FLOOD as u64);
    assert!(a.queued_bytes() <= a.budget());
    assert!(a.high_water() <= a.budget());
    assert_eq!(a.resyncs(), 1);
    let s = snapshot(&a);
    assert_eq!(s.offset, FLOOD as u64);
    assert!(matches!(next(&a), StreamItem::Exited(_)));
    assert!(a.next().is_none());
    assert_eq!(a.resyncs(), 1);
    finish(term);
}

#[test]
fn resync_converges_under_a_throttled_reader() {
    let _heavy = HEAVY.lock().unwrap_or_else(|e| e.into_inner());
    const COUNT: usize = 16 * 1024 * 1024;
    let term = HeadlessTerminal::spawn(config(&flood(COUNT))).unwrap();
    let a = term.attach("one".into()).unwrap();
    let mut mirror = None;
    let mut offset = 0;
    loop {
        match next(&a) {
            StreamItem::Snapshot(s) => {
                offset = s.offset;
                mirror = Some(screen(&s));
            }
            StreamItem::Output { offset: at, bytes } => {
                assert_eq!(at, offset);
                offset += bytes.len() as u64;
                mirror.as_mut().unwrap().feed(&bytes);
                // One MiB per second; this is workload pacing, never a pass/fail timing bound.
                std::thread::sleep(Duration::from_secs_f64(bytes.len() as f64 / (1024.0 * 1024.0)));
            }
            StreamItem::Exited(_) => break,
            _ => {}
        }
    }
    assert_eq!(offset, COUNT as u64);
    assert!(a.resyncs() > 0 && a.resyncs() <= (COUNT / (4 * 1024 * 1024) + 1) as u64);
    let b = term.attach("two".into()).unwrap();
    let final_state = snapshot(&b);
    assert_eq!(canonical(mirror.as_mut().unwrap()), final_state.replay);
    finish(term);
}

#[test]
fn the_pty_is_never_blocked_by_a_client() {
    let _heavy = HEAVY.lock().unwrap_or_else(|e| e.into_inner());
    let run = |attach: bool| {
        let term = HeadlessTerminal::spawn(config(&flood(FLOOD))).unwrap();
        let a = attach.then(|| term.attach("one".into()).unwrap());
        let start = std::time::Instant::now();
        wait_for(|| !term.status().alive);
        let elapsed = start.elapsed();
        assert_eq!(term.status().offset, FLOOD as u64);
        drop(a);
        finish(term);
        elapsed
    };
    let alone = run(false);
    let stalled = run(true);
    assert!(stalled < alone * 8, "stalled {stalled:?}, unattached {alone:?}");
}

#[test]
fn an_over_cap_history_is_trimmed_and_counted() {
    let _heavy = HEAVY.lock().unwrap_or_else(|e| e.into_inner());
    // Both per-cell colours and underline colours prevent the replay from collapsing long runs.
    let mut c = python(
        r#"
import os
for row in range(10025):
    line = ''.join('\x1b[38;2;%d;20;30;48;2;30;%d;40;58;2;10;30;%dmx' % (col, col, col) for col in range(120))
    os.write(1, (line + '\x1b[0m\r\n').encode())
"#,
    );
    c.size = size(120, 24);
    let term = HeadlessTerminal::spawn(c).unwrap();
    wait_for(|| !term.status().alive);
    let a = term.attach("one".into()).unwrap();
    let s = snapshot(&a);
    assert!(s.replay.bytes.len() <= 32 * 1024 * 1024);
    assert!(s.history_at_cap);
    assert!(s.history_trimmed > 0);
    let mut expected = Screen::new(size(120, 24), TerminalColors::default());
    for _ in 0..23 {
        let line: String = (0..120)
            .map(|col| format!("\x1b[38;2;{col};20;30;48;2;30;{col};40;58;2;10;30;{col}mx"))
            .collect();
        expected.feed(format!("{line}\x1b[0m\r\n").as_bytes());
    }
    assert_eq!(screen(&s).render(true), expected.render(true));
    let b = term.attach("two".into()).unwrap();
    let again = snapshot(&b);
    assert_eq!(
        again.history_trimmed, s.history_trimmed,
        "the server retains its full history"
    );
    assert_eq!(again.replay, s.replay);
    finish(term);
}

#[test]
fn dropping_an_attachment_leaves_the_terminal_running() {
    let term = HeadlessTerminal::spawn(config("sleep 30")).unwrap();
    let a = term.attach("one".into()).unwrap();
    snapshot(&a);
    drop(a);
    wait_for(|| term.status().attached.is_none());
    assert!(term.status().alive);
    assert!(term.status().detached_since.is_some());
    finish(term);
}

#[test]
fn a_snapshot_waits_for_a_synchronized_update_to_end() {
    let term = HeadlessTerminal::spawn(config(r#"stty raw -echo; printf '\033[?2026hinside'; dd bs=1 count=1 of=/dev/null 2>/dev/null; printf '\033[?2026lafter'"#)).unwrap();
    wait_for(|| term.status().offset > 0);
    let a = term.attach("one".into()).unwrap();
    let s = snapshot(&a); // The worker's update deadline closes an update whose program is waiting for input.
    assert!(String::from_utf8_lossy(&s.replay.bytes).contains("inside"));
    term.input(&"one".into(), 0, vec![b'a']);
    loop {
        if let StreamItem::Exited(_) = next(&a) {
            break;
        }
    }
    finish(term);
}

#[test]
fn resize_requests_are_clamped_before_allocation() {
    let term = HeadlessTerminal::spawn(config("sleep 30")).unwrap();
    let a = term.attach("one".into()).unwrap();
    snapshot(&a);
    term.resize(size(0, u16::MAX));
    loop {
        if let StreamItem::Resized { size, .. } = next(&a) {
            assert_eq!((size.cols, size.rows), (1, 500));
            break;
        }
    }
    term.resize(size(u16::MAX, 0));
    loop {
        if let StreamItem::Resized { size, .. } = next(&a) {
            assert_eq!((size.cols, size.rows), (1000, 1));
            break;
        }
    }
    finish(term);
}

#[test]
fn a_cap_smaller_than_the_visible_grid_waits_without_trimming_cells() {
    use eitri_terminal::WaitWhy;
    let mut c = config("printf visible; sleep 30");
    c.snapshot_cap = 1;
    let term = HeadlessTerminal::spawn(c).unwrap();
    wait_for(|| term.status().offset >= 7);
    let a = term.attach("one".into()).unwrap();
    assert!(matches!(next(&a), StreamItem::Waiting(WaitWhy::SnapshotTooLarge)));
    assert_eq!(term.status().offset, 7);
    assert!(term.status().alive);
    finish(term);
}

#[test]
fn signal_and_left_terminal_exits_are_distinct() {
    let term = HeadlessTerminal::spawn(config("kill -TERM $$")).unwrap();
    let a = term.attach("one".into()).unwrap();
    loop {
        if let StreamItem::Exited(exit) = next(&a) {
            assert_eq!(exit.signal, Some(libc::SIGTERM));
            assert_eq!(exit.code, None);
            assert!(!exit.detached);
            break;
        }
    }
    finish(term);
    let term = HeadlessTerminal::spawn(config("exec 0<&- 1>&- 2>&-; exec sleep 30")).unwrap();
    let a = term.attach("one".into()).unwrap();
    loop {
        if let StreamItem::Exited(exit) = next(&a) {
            assert!(exit.detached);
            assert_eq!(exit.code, None);
            assert_eq!(exit.signal, None);
            break;
        }
    }
    assert!(term.status().alive);
    finish(term);
}

#[test]
fn an_exit_completes_an_owed_snapshot_or_reports_why_it_cannot() {
    use eitri_terminal::WaitWhy;
    for sequence in ["\\033]2;abc", "\\033Pabc", "complete"] {
        for attached in [false, true] {
            let script = if attached {
                format!("stty raw -echo; printf '{sequence}'; dd bs=1 count=1 of=/dev/null 2>/dev/null; exit 7")
            } else {
                format!("printf '{sequence}'; exit 7")
            };
            let term = HeadlessTerminal::spawn(config(&script)).unwrap();
            wait_for(|| term.status().offset > 0);
            if !attached {
                wait_for(|| !term.status().alive);
            }
            let a = term.attach("one".into()).unwrap();
            if attached {
                term.input(&"one".into(), 0, vec![b'!']);
            }
            let first = match next(&a) {
                StreamItem::InputWritten(1) if attached => next(&a),
                item => item,
            };
            if sequence == "complete" {
                assert!(matches!(first, StreamItem::Snapshot(_)));
            } else {
                assert!(matches!(first, StreamItem::Waiting(WaitWhy::OpenSequence)));
            }
            loop {
                match next(&a) {
                    StreamItem::Exited(exit) => {
                        assert_eq!(exit.code, Some(7));
                        break;
                    }
                    StreamItem::InputWritten(_) => {}
                    item => panic!("unexpected final item: {item:?}"),
                }
            }
            finish(term);
        }
    }
}

#[test]
fn eof_with_an_open_sequence_reports_waiting_then_left_terminal() {
    use eitri_terminal::WaitWhy;
    let term = HeadlessTerminal::spawn(config("printf '\\033]2;abc'; exec 0<&- 1>&- 2>&-; exec sleep 30")).unwrap();
    wait_for(|| term.status().offset == 7);
    let a = term.attach("one".into()).unwrap();
    assert!(matches!(next(&a), StreamItem::Waiting(WaitWhy::OpenSequence)));
    assert!(matches!(next(&a), StreamItem::Exited(exit) if exit.detached));
    finish(term);
}

#[test]
fn trimming_keeps_the_newest_history_and_stays_close_to_the_minimum() {
    let mut bytes = Vec::new();
    for i in 0..1024 {
        bytes.extend_from_slice(format!("{i:05}\r\n").as_bytes());
    }
    // Omitting a known number of equal-width rows gives an independent cap for that minimum.
    for removed in [1, 40, 300] {
        let mut copy = Screen::headless(size(80, 24), TerminalColors::default());
        copy.feed(&bytes[removed * 7..]);
        let cap = canonical(&mut copy).bytes.len();
        let mut c = config("i=0; while [ $i -lt 1024 ]; do printf '%05d\\n' $i; i=$((i+1)); done");
        c.snapshot_cap = cap;
        let term = HeadlessTerminal::spawn(c).unwrap();
        wait_for(|| !term.status().alive);
        let a = term.attach("one".into()).unwrap();
        let s = snapshot(&a);
        assert!(s.replay.bytes.len() <= cap);
        assert!(
            s.history_trimmed >= removed as u32 && s.history_trimmed <= (2 * removed) as u32,
            "trimmed {}, minimum {removed}",
            s.history_trimmed
        );
        let text = String::from_utf8_lossy(&s.replay.bytes);
        assert!(text.contains("01000"), "the newest history row survives");
        assert!(!text.contains("00000"), "the oldest row is trimmed");
        finish(term);
    }
}

#[test]
fn an_exit_at_the_keyboard_depth_limit_is_delivered() {
    use eitri_terminal::WaitWhy;
    for attached in [false, true] {
        let gate = if attached {
            "dd bs=1 count=1 of=/dev/null 2>/dev/null;"
        } else {
            ""
        };
        let term = HeadlessTerminal::spawn(config(&format!(
            "stty raw -echo; i=0; while [ $i -lt 4096 ]; do printf '\\033[>1u'; i=$((i+1)); done; {gate} exit 7"
        )))
        .unwrap();
        wait_for(|| term.status().offset == 4096 * 5);
        if !attached {
            wait_for(|| !term.status().alive);
        }
        let a = term.attach("one".into()).unwrap();
        assert!(matches!(next(&a), StreamItem::Waiting(WaitWhy::TooDeep)));
        if attached {
            term.input(&"one".into(), 0, vec![b'!']);
        }
        loop {
            match next(&a) {
                StreamItem::Exited(exit) => {
                    assert_eq!(exit.code, Some(7));
                    break;
                }
                StreamItem::InputWritten(_) => {}
                item => panic!("unexpected final item: {item:?}"),
            }
        }
        finish(term);
    }
}
