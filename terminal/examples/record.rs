//! Records what a program writes to an Eitri terminal, byte for byte, for the corpus replay
//! (`tests/corpus.rs`). The program runs in a real `TerminalSession` -- the product's PTY, the
//! host's environment, and a `Term` that answers its queries -- so the recording is what the
//! program really says to this terminal, not to foot.
//!
//!     cargo run -p eitri-terminal --example record -- OUT_DIR NAME COLSxROWS [--send MS TEXT]... -- PROGRAM [ARGS]...
//!
//! writes `OUT_DIR/NAME.COLSxROWS.bytes`. `--send MS TEXT` types TEXT into the program MS
//! milliseconds after it starts (`\e` is Escape, `\r` Enter, `\n` a newline); that is how an
//! interactive program is quit. The input goes into this example's own PTY, never to the desktop.
//! A program still running 30 s after start is hung up.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eitri_terminal::{PtySize, SessionCommand, SessionConfig, SpawnSpec, TerminalColors, TerminalSession};
use terminal_input::NormalizedInput;

const MAX_RUN: Duration = Duration::from_secs(30);

fn unescape(text: &str) -> String {
    text.replace("\\e", "\x1b").replace("\\r", "\r").replace("\\n", "\n")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: record OUT_DIR NAME COLSxROWS [--send MS TEXT]... -- PROGRAM [ARGS]...";
    let split = args.iter().position(|a| a == "--").expect(usage);
    let (head, program) = (&args[..split], &args[split + 1..]);
    assert!(head.len() >= 3 && !program.is_empty(), "{usage}");
    let (dir, name, geometry) = (PathBuf::from(&head[0]), &head[1], &head[2]);
    let (cols, rows) = geometry.split_once('x').expect(usage);
    let size = PtySize {
        cols: cols.parse().expect(usage),
        rows: rows.parse().expect(usage),
        cell_width_px: 9,
        cell_height_px: 18,
    };
    let mut sends: Vec<(Duration, String)> = head[3..]
        .chunks(3)
        .map(|c| {
            assert!(c.len() == 3 && c[0] == "--send", "{usage}");
            (Duration::from_millis(c[1].parse().expect("--send MS")), unescape(&c[2]))
        })
        .collect();
    sends.reverse();

    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join(format!("{name}.{geometry}.bytes"));
    let file = Arc::new(Mutex::new(File::create(&out).unwrap()));
    let sink = file.clone();
    let (woken_tx, woken) = mpsc::channel();
    let session = TerminalSession::spawn(
        SessionConfig {
            spawn: SpawnSpec {
                program: PathBuf::from(&program[0]),
                args: program[1..].iter().map(Into::into).collect(),
                cwd: std::env::current_dir().unwrap(),
            },
            size,
            colors: TerminalColors::default(),
            focused: true,
            tap: Some(Box::new(move |bytes: &[u8]| {
                sink.lock().unwrap().write_all(bytes).unwrap()
            })),
        },
        move || {
            let _ = woken_tx.send(());
        },
    )
    .unwrap();

    let started = Instant::now();
    let status = loop {
        while sends.last().is_some_and(|(at, _)| started.elapsed() >= *at) {
            let (_, text) = sends.pop().unwrap();
            session.send(SessionCommand::Input(NormalizedInput::Paste { text, bracketed: false }));
        }
        let _ = woken.recv_timeout(Duration::from_millis(20));
        if let Some(exit) = session.take_update().exited {
            break format!("{exit:?}");
        }
        if started.elapsed() > MAX_RUN {
            break format!("hung up after {MAX_RUN:?}: {:?}", session.shutdown_and_wait());
        }
    };
    file.lock().unwrap().flush().unwrap();
    eprintln!(
        "{}: {} bytes, {status}",
        out.display(),
        std::fs::metadata(&out).unwrap().len()
    );
}
