//! The agents layer's disk and socket work never runs on the window's thread, read from the shell's and the
//! panel's code: starting the publisher (it makes the agents directory and takes its lock), binding the control
//! socket, sending to another window's socket and scanning the rows directory each sit inside a closure handed
//! to a new thread. The rows themselves reach the publisher only through the panel's glue, whose calls only
//! queue for the publisher's own thread; no other file names the publisher's queueing calls.

use eitri_core::source_scan as scan;

/// Calls that touch the disk or a socket and may block.
const BLOCKING: &[&str] = &[
    "Publisher::start(",
    "Publisher::start_with(",
    "Server::bind(",
    "control::send(",
    "scan_once(",
    "Reader::start(",
];

/// The publisher's queueing calls, a method on whatever holds it.
const QUEUEING: &[&str] = &[".offer_now(", ".departing("];

/// The one file that hands rows to the publisher.
const GLUE: &str = "panel/src/agents_glue.rs";

/// A source file as code (comments and literals blanked, its tests cut), with its byte offsets kept.
fn production(source: &scan::Source) -> String {
    let code = scan::code_only(&source.text);
    scan::without(&code, &scan::modules(&code, "tests"))
}

/// The byte ranges of every closure handed to a new thread in `code`: the parenthesised arguments of each
/// `spawn(` whose first argument is a `move` closure (`std::thread::spawn` and `thread::Builder::spawn`).
fn worker_closures(code: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    for (at, call) in code.match_indices("spawn(") {
        let open = at + call.len() - 1;
        if !code[open + 1..].trim_start().starts_with("move") {
            continue;
        }
        let before = code[..at].trim_end();
        if !(before.ends_with('.') || before.ends_with("::")) {
            continue;
        }
        let mut depth = 0usize;
        for (i, c) in code[open..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        ranges.push(open..open + i);
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    ranges
}

/// Each place in `code` where one of `needles` is called outside every worker closure, as `line: needle`.
fn outside_workers(code: &str, needles: &[&str]) -> Vec<String> {
    let workers = worker_closures(code);
    let mut found = Vec::new();
    for needle in needles {
        for (at, _) in code.match_indices(needle) {
            // `fn scan_once(` defines rather than calls.
            if code[..at].trim_end().ends_with("fn") {
                continue;
            }
            if !workers.iter().any(|range| range.contains(&at)) {
                let line = code[..at].matches('\n').count() + 1;
                found.push(format!("{line}: {needle}"));
            }
        }
    }
    found
}

/// The `.offer(` calls in `code` that pass something: a call with no argument is another type's method.
fn offers_with_rows(code: &str) -> usize {
    code.match_indices(".offer(")
        .filter(|(at, call)| !code[at + call.len()..].trim_start().starts_with(')'))
        .count()
}

#[test]
fn no_agents_io_on_the_gtk_thread() {
    let sources = scan::rust_sources(&["shell/src", "panel/src"]);
    scan::require_anchors(
        &sources,
        &[
            "Publisher::start(",
            "Server::bind(",
            "control::send(",
            "self.sink.offer(",
            "self.sink.offer_now(",
            "self.sink.departing(",
        ],
    );
    let test_files = scan::test_only_files(&sources);
    let mut wrong = Vec::new();
    for source in sources.iter().filter(|s| !test_files.contains(&s.path)) {
        let code = production(source);
        for place in outside_workers(&code, BLOCKING) {
            wrong.push(format!("{}:{place} runs on the calling thread", source.path));
        }
        if source.path == GLUE {
            continue;
        }
        for needle in QUEUEING {
            if code.contains(needle) {
                wrong.push(format!("{}: {needle} outside the glue", source.path));
            }
        }
        if offers_with_rows(&code) > 0 {
            wrong.push(format!("{}: .offer(rows) outside the glue", source.path));
        }
    }
    assert!(
        wrong.is_empty(),
        "agents disk and socket work off the workers: {wrong:#?}"
    );
}

#[test]
fn a_send_on_the_calling_thread_fails_the_scan() {
    let on_worker = "fn f() { std::thread::Builder::new().spawn(move || { let r = control::send(&d, &i, &q); }); }";
    assert!(outside_workers(on_worker, BLOCKING).is_empty());
    let on_caller = "fn f() { let r = control::send(&d, &i, &q); std::thread::spawn(move || {}); }";
    assert_eq!(outside_workers(on_caller, BLOCKING), ["1: control::send("]);
    let defined = "pub fn scan_once(dir: &Path) {}";
    assert!(outside_workers(defined, BLOCKING).is_empty());
    assert_eq!(offers_with_rows("plan.offer() + p.offer(rows)"), 1);
}
