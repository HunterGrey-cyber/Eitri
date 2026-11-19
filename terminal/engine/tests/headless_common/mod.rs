#![allow(dead_code)]

use std::io;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use eitri_terminal::headless::{Attachment, HeadlessConfig, HeadlessTerminal, StreamItem};
use eitri_terminal::{PtySize, Screen, SpawnExtras, SpawnSpec, TerminalColors};

pub const WAIT: Duration = Duration::from_secs(60);
pub fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        cols,
        rows,
        cell_width_px: 8,
        cell_height_px: 16,
    }
}
pub fn spec(script: &str) -> SpawnSpec {
    SpawnSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: "/tmp".into(),
        env: vec![],
        extra_env: vec![],
    }
}
pub fn config(script: &str) -> HeadlessConfig {
    HeadlessConfig {
        spawn: spec(script),
        extras: SpawnExtras {
            exact_env: Some(vec![("PATH".into(), "/usr/bin:/bin".into())]),
            ..SpawnExtras::default()
        },
        size: size(80, 24),
        colors: TerminalColors::default(),
        queue_budget: 4 * 1024 * 1024,
        snapshot_cap: 32 * 1024 * 1024,
    }
}
pub fn next(attachment: &Attachment) -> StreamItem {
    let until = Instant::now() + WAIT;
    loop {
        if let Some(item) = attachment.next() {
            return item;
        }
        assert!(Instant::now() < until, "attachment did not produce an item");
        let mut fd = libc::pollfd {
            fd: attachment.ready().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut fd, 1, 100) };
        assert!(rc >= 0 || io::Error::last_os_error().kind() == io::ErrorKind::Interrupted);
    }
}
pub fn wait_for(mut predicate: impl FnMut() -> bool) {
    let until = Instant::now() + WAIT;
    while !predicate() {
        assert!(Instant::now() < until, "condition did not become true");
        std::thread::sleep(Duration::from_millis(2));
    }
}
pub fn snapshot(attachment: &Attachment) -> eitri_terminal::Snapshot {
    loop {
        if let StreamItem::Snapshot(s) = next(attachment) {
            return s;
        }
    }
}
pub fn screen(snapshot: &eitri_terminal::Snapshot) -> Screen {
    assert_eq!(snapshot.tail, eitri_terminal::Tail::ReadAhead(vec![]));
    Screen::replayed(
        &snapshot.replay,
        size(snapshot.replay.columns, snapshot.replay.rows),
        TerminalColors::default(),
    )
}
pub fn canonical(screen: &mut Screen) -> eitri_terminal::replay::screen_replay::ScreenReplay {
    screen.read_for_move().unwrap()
}
pub fn finish(term: HeadlessTerminal) {
    term.close(Duration::from_millis(100));
}

/// Mode, cursor, history, wide-character, charset and alternate-screen streams.
pub fn corpus() -> Vec<Vec<u8>> {
    let mut cases = vec![
        b"\x1b[?1049h\x1b[H\x1b[2Jvim\x1b[5;5H\x1b7\x1b[8;8H".to_vec(),
        b"\x1b[?1049hless\r\n\x1b[7msearch\x1b[0m\r\nlast".to_vec(),
        b"\x1b[?1002h\x1b[?1006h\x1b[?2004h".to_vec(),
        b"\x1b[>1u\x1b[>3u\x1b[?1049h\x1b[>7u\x1b[=9;1u".to_vec(),
        b"\x1b]2;first\x07\x1b[22t\x1b]2;second\x07\x1b[3g\x1b[4G\x1bH\x1b)0\x0eqx\x0f\x1b[2;8r".to_vec(),
        b"abc\x1b[2J\x1b[4b".to_vec(),
        b"\x1b]4;1;rgb:12/34/56\x07\x1b]8;id=x;https://example.com\x1b\\link\x1b]8;;\x1b\\\x1b[4:3;58;2;10;20;30m"
            .to_vec(),
        "中文 e\u{301} 漢字".as_bytes().to_vec(),
        b"\x1b#8\x1b[2J\x1b[5b".to_vec(),
        b"\x1b[2;6r\x1b[?6h\x1b7\x1b[3;4H\x1b8".to_vec(),
    ];
    for n in [79, 80, 81, 160] {
        cases.push(vec![b'x'; n]);
    }
    let mut lines = Vec::new();
    for i in 0..200 {
        lines.extend_from_slice(format!("\x1b[38;2;{};20;30m{i:05} mixed 漢 e\u{301}\x1b[0m\r\n", i % 256).as_bytes());
    }
    cases.push(lines);
    cases.extend([
        b"\x1b[>1u\x1b[?1049h\x1b[>7u\x1b[?1049l\x1b[=2;2u".to_vec(),
        b"\x1b[>1u\x1b[?1049h\x1b[>3u\x1b[?1049l\x1b]2;t\x07\x1b[22t\x1b[3g\x1b[1;7H\x1bH\x1b[2;9r\x1b)0\x0eabc\x1bc".to_vec(),
        b"\x1b(0\x1b]8;id=p;https://example.com/p\x1b\\\x1b[?1049h\x1b(B\x1b]8;;\x1b\\q x".to_vec(),
        b"\x1b[5;10r\x1b[?6h\x1b8ab\nc".to_vec(),
        b"\x1b[20;7H\x1b7\x1b[5;10r\x1b[?6h\x1b8\n\nxy".to_vec(),
        b"\x1b[?1002h\x1b[?1006h\x1b[?2004h\x1b[?1004h\x1b[?1h\x1b=\x1b[4h\x1b[20h\x1b[?7l\x1b[?25l\x1b[?1007l\x1b[?12h\x1b[3;10r\x1b[?6h\x1b[4;5Hin the region".to_vec(),
        format!("{}\x1b7B", "A".repeat(80)).into_bytes(),
        format!("\x1b[5;1H{}\x1b7B\x1b[1;5r", "A".repeat(80)).into_bytes(),
        format!("{}\x1b7B\x1b[?7l\x1b[4h\x1b[3;3H", "A".repeat(80)).into_bytes(),
        format!("{}\x1b7B\x1b8\x1b[?1049hvim", "A".repeat(80)).into_bytes(),
        format!("\x1b[4h\x1b)0\x0e\x1b[1;35m{}", "q".repeat(80)).into_bytes(),
        format!("{}漢", "0".repeat(78)).into_bytes(),
        "\x1b]4;1;rgb:12/34/56\x1b\\\x1b]4;200;#abcdef\x07\x1b]10;rgb:aa/bb/cc\x07\x1b]11;#102030\x07\x1b]12;rgb:ff/00/ff\x07\x1b]8;id=x;https://example.com/a\x1b\\link e\u{301}\u{302}\x1b[4:3;58;2;255;0;0mcurl".as_bytes().to_vec(),
        "\x1b[>1u".repeat(4095).into_bytes(),
    ]);
    cases
}
