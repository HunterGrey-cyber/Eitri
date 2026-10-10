//! The app's own lines on stdout and stderr, written so that a stream nobody reads any more costs nothing.
//!
//! `println!` and `eprintln!` panic when the write fails, and a write to a pipe whose reader has gone fails
//! with `EPIPE` (Rust ignores `SIGPIPE`): `eitri-mac proj | head` would end the app at its eleventh line, and a
//! panic inside an AppKit callback cannot unwind, so the process aborts with its sessions. Every line of this
//! crate goes through [`log_line!`](crate::log_line) or [`log_error!`](crate::log_error) instead, which drop a
//! failed write. Other crates' prints are covered by moving the streams off a pipe at start
//! (`startup::move_output_to_log`).

use std::fmt;
use std::io::{self, Write};

/// Writes `args` and a newline to `out`, ignoring a failed write.
pub fn write_line(out: &mut dyn Write, args: fmt::Arguments) {
    let _ = writeln!(out, "{args}");
}

/// A line on stdout; a failed write is dropped.
pub fn stdout_line(args: fmt::Arguments) {
    // The test harness captures only its own print macros' output.
    #[cfg(test)]
    println!("{args}");
    #[cfg(not(test))]
    write_line(&mut io::stdout().lock(), args);
}

/// A line on stderr; a failed write is dropped.
pub fn stderr_line(args: fmt::Arguments) {
    #[cfg(test)]
    eprintln!("{args}");
    #[cfg(not(test))]
    write_line(&mut io::stderr().lock(), args);
}

/// `println!` that never panics.
#[macro_export]
macro_rules! log_line {
    ($($arg:tt)*) => {
        $crate::log::stdout_line(format_args!($($arg)*))
    };
}

/// `eprintln!` that never panics.
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::log::stderr_line(format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::source_scan;

    /// A stream whose reader has gone.
    struct DeadPipe;
    impl Write for DeadPipe {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
    }

    #[test]
    fn a_line_to_a_dead_pipe_is_dropped_without_a_panic() {
        write_line(&mut DeadPipe, format_args!("[mac] {} lines", 11));
        let mut kept = Vec::new();
        write_line(&mut kept, format_args!("[mac] {}", "kept"));
        assert_eq!(kept, b"[mac] kept\n");
    }

    /// `code` (comments and literals already blanked) with every item under `#[cfg(test)]` blanked too: a test
    /// module, a test-only function, or a single statement such as this file's own harness prints. Line breaks
    /// are kept, so a line number still names the source line.
    fn without_test_items(code: &str) -> String {
        let mut out = code.as_bytes().to_vec();
        for (at, _) in code.match_indices("#[cfg(test)]") {
            let rest = &code[at..];
            let item_end = match (rest.find('{'), rest.find(';')) {
                (Some(open), semi) if semi.is_none_or(|semi| open < semi) => {
                    open + source_scan::braced(rest, open).len()
                }
                (_, Some(semi)) => semi + 1,
                _ => rest.len(),
            };
            for byte in &mut out[at..at + item_end] {
                if *byte != b'\n' {
                    *byte = b' ';
                }
            }
        }
        // Every byte of a blanked range is now ASCII and each range starts and ends on an ASCII byte: still UTF-8.
        String::from_utf8(out).expect("blanked at character boundaries")
    }

    /// The 1-based lines of `text` that call a macro writing to stdout or stderr outside a test item.
    fn print_sites(text: &str) -> Vec<usize> {
        let code = without_test_items(&source_scan::code_only(text));
        let mut lines: Vec<usize> = ["println", "eprintln", "print", "eprint", "dbg"]
            .iter()
            .flat_map(|name| source_scan::word_ends(&code, name))
            .filter(|end| code[*end..].starts_with('!'))
            .map(|end| code[..end].matches('\n').count() + 1)
            .collect();
        lines.sort_unstable();
        lines.dedup();
        lines
    }

    #[test]
    fn the_print_scan_reads_code_not_text() {
        let text = r##"
fn a() {
    let url = "https://example.org"; println!("{url}");
    // println!("only a comment");
    let s = "eprintln!(\"only a string\")";
    dbg!(s);
    eprint ! ("spaced");
}
#[cfg(test)]
fn helper() { println!("a test's"); }
#[cfg(test)]
eprintln!("a test-only statement");
#[cfg(test)]
mod tests {
    fn t() { println!("{}", 1); }
}
fn b() { print!("after the tests"); }
"##;
        // The `eprint !` with a blank before the `!` is not a macro call this scan knows; rustfmt never writes it.
        assert_eq!(print_sites(text), [3, 6, 17]);
    }

    /// Every line this crate writes outside its tests goes through the macros above. Tests may print: they
    /// run under the harness, which captures their output.
    #[test]
    fn no_print_macro_outside_tests() {
        let read = source_scan::rust_sources(&["mac/src"]);
        // A file this scan exists for on each side of the crate: the host-neutral keys and the AppKit window.
        source_scan::require_anchors(&read, &["fn give_panel_keys(", "fn on_key_down(", "pub fn run_main("]);
        let found: Vec<String> = read
            .iter()
            .flat_map(|source| {
                print_sites(&source.text).into_iter().map(move |n| {
                    let line = source.text.lines().nth(n - 1).unwrap_or("").trim();
                    format!("{}:{n}: {line}", source.path)
                })
            })
            .collect();
        assert!(
            found.is_empty(),
            "print macros that panic on a dead pipe:\n{}",
            found.join("\n")
        );
    }
}
