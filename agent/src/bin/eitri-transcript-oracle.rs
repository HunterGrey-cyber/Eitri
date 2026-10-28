//! `eitri-transcript-oracle`: a Claude CLI session's own files against Eitri's records of the same
//! conversation.
//!
//! ```text
//! eitri-transcript-oracle --cli <session>.jsonl [--cli <next>.jsonl ...] [--subagents <dir>]
//!     [--history-a <file.json>] [--restore-b] [--from-turn N] [--show-text] [--json <out>]
//! ```
//!
//! - `--cli`: the session's `.jsonl`, oldest first when `/clear` continued it in another file.
//! - `--subagents`: the directory of the session's subagent transcripts.
//! - `--history-a`: Eitri's stored history of the session; `--restore-b`: Eitri's restore reader run on the
//!   same `.jsonl` files. At least one of the two.
//! - `--from-turn N`: report from the CLI's turn N on.
//! - `--json <out>`: the report as JSON too.
//! - `--show-text`: print text and values. Without it the report is redacted: structure, counts, words
//!   from closed lists, built-in tool names, input key names and digests keyed for this run only. Error
//!   messages are redacted the same way: they name an input by its flag and position, never by its path.
//!
//! It reads only the files it is given. Exit codes: 0 equal (allowances aside), 1 differences, 2 an input
//! that could not be read or a command line it does not take.

use agent::history::HistoryLoadError;
use agent::testkit::oracle::align::align;
use agent::testkit::oracle::cli_jsonl::read_chain;
use agent::testkit::oracle::eitri::{read_a_file, read_b, B_TURNS_HELD};
use agent::testkit::oracle::redact::Redactor;
use agent::testkit::oracle::report::{Report, EXIT_UNREADABLE};
use std::ffi::OsString;
use std::path::PathBuf;

const NAME: &str = "eitri-transcript-oracle";

const USAGE: &str = "usage: eitri-transcript-oracle --cli <session.jsonl> [--cli <next.jsonl> ...] \
[--subagents <dir>] [--history-a <file.json>] [--restore-b] [--from-turn N] [--show-text] [--json <out>]";

/// Why the run stopped before a report: printed as is, so it holds only flags, positions and fixed words
/// unless text is shown.
struct Stop(String);

#[derive(Default)]
struct Args {
    cli: Vec<PathBuf>,
    subagents: Option<PathBuf>,
    history_a: Option<PathBuf>,
    restore_b: bool,
    from_turn: Option<usize>,
    show_text: bool,
    json: Option<PathBuf>,
}

fn parse(raw: &[OsString]) -> Result<Args, Stop> {
    let mut args = Args::default();
    let mut i = 0;
    let value = |i: &mut usize, flag: &str| -> Result<PathBuf, Stop> {
        *i += 1;
        raw.get(*i)
            .map(PathBuf::from)
            .ok_or_else(|| Stop(format!("{flag} needs a value")))
    };
    let once = |slot: &Option<PathBuf>, flag: &str| -> Result<(), Stop> {
        if slot.is_some() {
            Err(Stop(format!("{flag} given twice")))
        } else {
            Ok(())
        }
    };
    while i < raw.len() {
        match raw[i].to_str().unwrap_or("") {
            "--cli" => {
                let path = value(&mut i, "--cli")?;
                args.cli.push(path);
            }
            "--subagents" => {
                once(&args.subagents, "--subagents")?;
                args.subagents = Some(value(&mut i, "--subagents")?);
            }
            "--history-a" => {
                once(&args.history_a, "--history-a")?;
                args.history_a = Some(value(&mut i, "--history-a")?);
            }
            "--json" => {
                once(&args.json, "--json")?;
                args.json = Some(value(&mut i, "--json")?);
            }
            "--restore-b" => args.restore_b = true,
            "--show-text" => args.show_text = true,
            "--from-turn" => {
                i += 1;
                let n = raw
                    .get(i)
                    .and_then(|v| v.to_str())
                    .and_then(|v| v.parse::<usize>().ok())
                    .filter(|n| *n >= 1)
                    .ok_or_else(|| Stop("--from-turn needs a whole number of at least 1".to_owned()))?;
                args.from_turn = Some(n);
            }
            "--help" | "-h" => return Err(Stop(USAGE.to_owned())),
            _ => {
                // The argument itself may be a path or anything else typed: it is named by position.
                return Err(Stop(format!(
                    "argument #{} is not one this program takes\n{USAGE}",
                    i + 1
                )));
            }
        }
        i += 1;
    }
    if args.cli.is_empty() {
        return Err(Stop(format!("--cli is required\n{USAGE}")));
    }
    if args.history_a.is_none() && !args.restore_b {
        return Err(Stop(format!(
            "nothing to compare: give --history-a or --restore-b\n{USAGE}"
        )));
    }
    Ok(args)
}

fn history_error(error: &HistoryLoadError, show_text: bool) -> String {
    let word = match error {
        HistoryLoadError::Io(e) if e.kind() == std::io::ErrorKind::NotFound => "not found".to_owned(),
        HistoryLoadError::Io(_) => "unreadable".to_owned(),
        HistoryLoadError::Malformed(_) => "not a stored history".to_owned(),
        HistoryLoadError::UnknownVersion { found, expected } => {
            format!("format version {found}, this build reads {expected}")
        }
    };
    if show_text {
        format!("--history-a: {word}: {error}")
    } else {
        format!("--history-a: {word}")
    }
}

fn io_word(error: &std::io::Error) -> &'static str {
    match error.kind() {
        std::io::ErrorKind::NotFound => "not found",
        std::io::ErrorKind::PermissionDenied => "permission denied",
        std::io::ErrorKind::InvalidData => "not text",
        _ => "unreadable",
    }
}

fn run(raw: &[OsString], show_text_requested: bool) -> Result<i32, Stop> {
    let args = parse(raw)?;
    let detail = |e: &std::io::Error| {
        if show_text_requested {
            format!(": {e}")
        } else {
            String::new()
        }
    };
    let redactor = Redactor::from_urandom().map_err(|e| Stop(format!("cannot draw a digest key{}", detail(&e))))?;
    let redactor = if args.show_text {
        redactor.showing_text()
    } else {
        redactor
    };

    // Each file is opened once on its own first, so a failure names the input it belongs to.
    for (k, path) in args.cli.iter().enumerate() {
        std::fs::File::open(path).map_err(|e| Stop(format!("--cli #{}: {}{}", k + 1, io_word(&e), detail(&e))))?;
    }
    if let Some(dir) = &args.subagents {
        std::fs::read_dir(dir).map_err(|e| Stop(format!("--subagents: {}{}", io_word(&e), detail(&e))))?;
    }
    let cli = read_chain(&args.cli, args.subagents.as_deref())
        .map_err(|e| Stop(format!("--cli or --subagents: {}{}", io_word(&e), detail(&e))))?;

    let mut alignments = Vec::new();
    if let Some(path) = &args.history_a {
        let a = read_a_file(path).map_err(|e| Stop(history_error(&e, args.show_text)))?;
        alignments.push(align(&cli.turns, &a));
    }
    if args.restore_b {
        let b =
            read_b(&args.cli, B_TURNS_HELD).map_err(|e| Stop(format!("--restore-b: {}{}", io_word(&e), detail(&e))))?;
        alignments.push(align(&cli.turns, &b));
    }
    let report = Report::build(&cli, alignments, args.from_turn);
    print!("{}", report.render(&redactor));
    if let Some(out) = &args.json {
        let text = serde_json::to_string_pretty(&report.to_json(&redactor)).map_err(|e| {
            Stop(format!(
                "--json: cannot encode{}",
                if args.show_text {
                    format!(": {e}")
                } else {
                    String::new()
                }
            ))
        })?;
        std::fs::write(out, text + "\n")
            .map_err(|e| Stop(format!("--json: cannot write: {}{}", io_word(&e), detail(&e))))?;
    }
    Ok(report.exit_code())
}

fn main() {
    // Arguments are taken as the OS gives them: a path need not be UTF-8.
    let raw: Vec<OsString> = std::env::args_os().skip(1).collect();
    let show_text = raw.iter().any(|a| a.to_str() == Some("--show-text"));
    if !show_text {
        // A panic's message may quote what it was reading; redacted runs print none of it.
        std::panic::set_hook(Box::new(|_| eprintln!("{NAME}: internal error")));
    }
    let code = match std::panic::catch_unwind(|| run(&raw, show_text)) {
        Ok(Ok(code)) => code,
        Ok(Err(Stop(message))) => {
            eprintln!("{NAME}: {message}");
            EXIT_UNREADABLE
        }
        Err(_) => EXIT_UNREADABLE,
    };
    std::process::exit(code);
}
