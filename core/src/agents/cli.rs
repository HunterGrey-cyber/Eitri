//! Agent summaries for a command-line reader, without a window or project resolver.

use std::io::{self, Write};
use std::os::fd::AsFd;
use std::path::Path;
use std::sync::mpsc;

use super::{scan_once, wire, Change, Class, Reader, ReaderEvent, Row, View, LABEL_MAX, SCHEMA_V};

/// Writes the same versioned view body used at the plugin boundary, followed by a newline.
pub fn write_view(view: &View, out: &mut dyn Write, schema: u32) -> io::Result<()> {
    writeln!(out, "{}", wire::view_json(view, schema))?;
    out.flush()
}

fn age(at: u64, since: u64) -> String {
    let seconds = at.saturating_sub(since) / 1_000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => {
            let (minutes, seconds) = (seconds / 60, seconds % 60);
            if seconds == 0 {
                format!("{minutes}m")
            } else {
                format!("{minutes}m{seconds:02}")
            }
        }
        _ => format!("{}h{:02}", seconds / 3_600, (seconds / 60) % 60),
    }
}

fn state(row: &Row) -> String {
    let (glyph, word) = match row.state.as_str() {
        "card" => ("⚑", "card"),
        "question" => ("?", "question"),
        "trust" => ("?", "trust"),
        "failed" => ("!", "failed"),
        "ended" => ("○", "ended"),
        "running" => ("●", "running"),
        "background" => ("◌", "backgr."),
        "starting" => ("◌", "starting"),
        "done" => ("•", "done"),
        "idle" => ("○", "idle"),
        "new" => ("○", "new"),
        word => (
            match row.class {
                Class::Needs => "?",
                Class::Working => "●",
                Class::Quiet => "○",
                Class::Ended => "!",
            },
            word,
        ),
    };
    format!("{glyph} {}", wire::cut(word, LABEL_MAX))
}

fn table(view: &View, out: &mut dyn Write) -> io::Result<()> {
    let mut rows = vec![[
        "PROJECT".into(),
        "WIN".into(),
        "#".into(),
        "AGENT".into(),
        "STATE".into(),
        "AGE".into(),
        "LABEL".into(),
    ]];
    rows.extend(view.rows.iter().map(|row| {
        [
            wire::cut(&row.project.name, LABEL_MAX),
            wire::cut(&row.window.name, LABEL_MAX),
            row.number.to_string(),
            wire::cut(&row.agent, LABEL_MAX),
            state(row),
            age(view.at, row.since),
            wire::cut(&row.label, LABEL_MAX),
        ]
    }));
    let widths: [usize; 6] = std::array::from_fn(|col| rows.iter().map(|row| cells(&row[col])).max().unwrap());
    for row in rows {
        for (col, width) in widths.iter().enumerate() {
            write!(out, "{}{:pad$}  ", row[col], "", pad = width - cells(&row[col]))?;
        }
        writeln!(out, "{}", row[6])?;
    }
    if view.rows.is_empty() {
        writeln!(out, "no window shares agents")?;
    }
    out.flush()
}

/// Terminal columns `s` takes: the East Asian wide and fullwidth blocks and emoji count two, the rest one.
/// Values are display-cut first, so hidden and combining characters are already escaped.
fn cells(s: &str) -> usize {
    s.chars()
        .map(|c| match u32::from(c) {
            0x1100..=0x115F
            | 0x2E80..=0x303E
            | 0x3041..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE6F
            | 0xFF01..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1F64F
            | 0x1F900..=0x1F9FF
            | 0x20000..=0x3FFFD => 2,
            _ => 1,
        })
        .sum()
}

fn watch(out: &mut dyn Write, control_dir: &Path, schema: u32) -> io::Result<()> {
    let stdout = io::stdout().as_fd().try_clone_to_owned()?;
    let (tx, rx) = mpsc::channel();
    let reader = Reader::start(control_dir, schema, move |event| {
        let _ = tx.send(event);
    })?;
    reader.add_fd(stdout);
    let mut initial = true;
    for event in rx {
        match event {
            ReaderEvent::Change(Change::Reset { view }) if initial => write_view(&view, out, schema)?,
            ReaderEvent::Change(change) => {
                writeln!(out, "{}", wire::change_json(&change, schema))?;
                out.flush()?;
            }
            ReaderEvent::Error(why) => return Err(io::Error::other(why)),
        }
        initial = false;
    }
    Ok(())
}

/// Runs before GTK starts. `out` receives the output; watch mode also polls the process's stdout for closure.
/// Invalid arguments return 64, I/O failures return 1, and a closed output pipe ends successfully.
pub fn run(args: &[String], out: &mut dyn Write, control_dir: &Path) -> i32 {
    let mut json = false;
    let mut watching = false;
    let mut schema = SCHEMA_V;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--watch" => watching = true,
            "--schema" => match args.next().and_then(|n| n.parse::<u32>().ok()) {
                Some(n) if n == SCHEMA_V => schema = n,
                _ => {
                    eprintln!("eitri agents: --schema requires {SCHEMA_V}");
                    return 64;
                }
            },
            _ => {
                eprintln!("usage: eitri agents [--json] [--watch] [--schema {SCHEMA_V}]");
                return 64;
            }
        }
    }
    let result = if watching {
        watch(out, control_dir, schema)
    } else {
        scan_once(control_dir, schema).and_then(|view| {
            if json {
                write_view(&view, out, schema)
            } else {
                table(&view, out)
            }
        })
    };
    match result {
        Ok(()) => 0,
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => 0,
        Err(e) => {
            eprintln!("eitri agents: {e}");
            1
        }
    }
}
