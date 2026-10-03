//! No `match` in `shell/src` takes a `RefCell` borrow of a panel's `state` in its scrutinee.
//!
//! **Why.** A temporary in a `match` scrutinee lives until the end of the whole `match` (edition 2021),
//! so `match gate_start(&mut state.borrow_mut(), ..) { Ok(()) => send_tabs(state, ..), .. }` still
//! holds the mutable borrow when `send_tabs` borrows `state` again. That panics with "RefCell already
//! borrowed", and inside a WebKit signal handler a panic cannot unwind: the whole window aborts. It
//! happened on the first message sent in a project that needed the workspace-trust question. The value
//! is bound by a `let` first, which drops the borrow at the end of that statement.
//!
//! **What it reads.** Every `.rs` file under `shell/src` as text, comments and string and character
//! literals blanked out first (so a message or a doc comment that names the shape is not one). For
//! each `match` keyword, the scrutinee is everything up to the `{` that opens the arms -- a brace
//! inside parentheses or brackets (a closure body) does not count, and what such a braced body
//! borrows is its own business -- with its whitespace normalised,
//! so a scrutinee split over several lines reads the same as one on a single line. It fails when that
//! scrutinee calls `borrow`, `borrow_mut`, `try_borrow` or `try_borrow_mut` on a receiver named
//! `state` (or ending in `state`, `self.state` included).
//!
//! **What it does not read.** `if let` and `while let` scrutinees keep their temporaries through the
//! body in the same way; there are many of them whose bodies never reach the same cell, so they are not
//! forbidden here. Other `RefCell`s (a layout, a driver) are not named `state` and are not checked.

use std::fs;
use std::path::{Path, PathBuf};

/// Replaces comments and string and character literals with spaces, keeping every newline so the
/// line numbers stay true.
fn blank_comments_and_literals(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let blank = |c: char| if c == '\n' { '\n' } else { ' ' };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                out.push(' ');
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            let mut depth = 0;
            while i < chars.len() {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    out.push_str("  ");
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    out.push_str("  ");
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    out.push(blank(chars[i]));
                    i += 1;
                }
            }
        } else if c == 'r' && matches!(next, Some('"') | Some('#')) && !prev_is_ident(&chars, i) {
            // A raw string: r"..", r#".."#, r##".."##.
            let mut j = i + 1;
            let mut hashes = 0;
            while chars.get(j) == Some(&'#') {
                hashes += 1;
                j += 1;
            }
            if chars.get(j) != Some(&'"') {
                out.push(c);
                i += 1;
                continue;
            }
            j += 1;
            loop {
                if j >= chars.len() {
                    break;
                }
                if chars[j] == '"' && (0..hashes).all(|k| chars.get(j + 1 + k) == Some(&'#')) {
                    j += 1 + hashes;
                    break;
                }
                j += 1;
            }
            for &ch in &chars[i..j.min(chars.len())] {
                out.push(blank(ch));
            }
            i = j;
        } else if c == '"' {
            out.push(' ');
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    out.push(' ');
                    out.push(blank(chars[i + 1]));
                    i += 2;
                } else {
                    out.push(blank(chars[i]));
                    i += 1;
                }
            }
            if i < chars.len() {
                out.push(' ');
                i += 1;
            }
        } else if c == '\'' {
            // A character literal ('x', '\n', '\u{..}'), not a lifetime ('a).
            let end = if next == Some('\\') {
                (i + 2..chars.len().min(i + 12)).find(|&k| chars[k] == '\'')
            } else if chars.get(i + 2) == Some(&'\'') {
                Some(i + 2)
            } else {
                None
            };
            match end {
                Some(end) => {
                    for _ in i..=end {
                        out.push(' ');
                    }
                    i = end + 1;
                }
                None => {
                    out.push(c);
                    i += 1;
                }
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

fn prev_is_ident(chars: &[char], i: usize) -> bool {
    i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_')
}

/// The scrutinee text of the `match` keyword at `at` (the byte index of `m`), whitespace-normalised,
/// or `None` when no arms' `{` follows.
fn scrutinee(code: &str, at: usize) -> Option<String> {
    let mut parens = 0i32;
    let mut braces = 0i32;
    let mut raw = String::new();
    for c in code[at + "match".len()..].chars() {
        match c {
            '(' | '[' => parens += 1,
            ')' | ']' => parens -= 1,
            '{' if parens <= 0 && braces == 0 => {
                let words: Vec<&str> = raw.split_whitespace().collect();
                let joined = words.join(" ");
                // `state . borrow ()` and `state\n    .borrow()` read the same.
                return Some(joined.replace(" .", ".").replace(". ", ".").replace(" (", "("));
            }
            '{' => braces += 1,
            '}' => braces -= 1,
            ';' if parens <= 0 && braces == 0 => return None,
            _ => {}
        }
        // A closure's braced body drops its own temporaries when it returns; only what the
        // scrutinee itself evaluates counts.
        if braces == 0 && c != '}' {
            raw.push(c);
        }
    }
    None
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Does the (normalised) scrutinee call a borrow method on a receiver named `state`?
fn borrows_state(scrutinee: &str) -> bool {
    for method in [".borrow(", ".borrow_mut(", ".try_borrow(", ".try_borrow_mut("] {
        let mut from = 0;
        while let Some(found) = scrutinee[from..].find(method) {
            let end = from + found;
            let receiver: String = scrutinee[..end]
                .chars()
                .rev()
                .take_while(|&c| is_ident(c))
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            if receiver.ends_with("state") {
                return true;
            }
            from = end + method.len();
        }
    }
    false
}

/// `(line, scrutinee)` of every offending `match` in `src`.
fn offences(src: &str) -> Vec<(usize, String)> {
    let code = blank_comments_and_literals(src);
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(rel) = code[from..].find("match") {
        let at = from + rel;
        from = at + "match".len();
        let before = code[..at].chars().next_back();
        let after = code[from..].chars().next();
        if before.is_some_and(is_ident) || after.is_some_and(is_ident) {
            continue;
        }
        if let Some(scrut) = scrutinee(&code, at) {
            if borrows_state(&scrut) {
                let line = code[..at].matches('\n').count() + 1;
                found.push((line, scrut));
            }
        }
    }
    found
}

fn rust_files(dir: &Path, into: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display())) {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            rust_files(&path, into);
        } else if path.extension().is_some_and(|e| e == "rs") {
            into.push(path);
        }
    }
}

#[test]
fn no_match_scrutinee_holds_a_state_borrow() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    assert!(
        files.len() > 10,
        "the scan found only {} files under {}",
        files.len(),
        root.display()
    );
    let mut problems = Vec::new();
    for file in &files {
        let src = fs::read_to_string(file).unwrap_or_else(|e| panic!("cannot read {}: {e}", file.display()));
        for (line, scrut) in offences(&src) {
            problems.push(format!(
                "{}:{line}: `match {scrut}`",
                file.strip_prefix(&root).unwrap().display()
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "a `match` scrutinee keeps its temporaries until the end of the whole match, so a `state` \
         borrow taken there is still held in every arm; an arm that borrows `state` again (as \
         `send_tabs` does) panics inside a WebKit signal handler and aborts the window. Bind the value \
         with a `let` first:\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_scan_sees_the_shape_that_crashed_and_not_its_bound_form() {
    let crashed = "fn f() {\n    match gate_start(&mut state.borrow_mut(), tab, start) {\n        Ok(()) => send_tabs(state, webview),\n        Err(why) => refuse(webview, &why),\n    }\n}\n";
    let hits = offences(crashed);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].0, 2);

    // Split over lines and spaces, a shared and a `try_` borrow, a field receiver.
    let split = "match self\n    .state\n    .try_borrow_mut()\n    .map(|s| s.n)\n{\n    _ => {}\n}";
    assert_eq!(offences(split).len(), 1);
    let shared = "let a = match state . borrow ( ).tabs.get(1) {\n    _ => 0,\n};";
    assert_eq!(offences(shared).len(), 1);

    // The bound form, a closure body in the scrutinee, a name that is not a panel state, comments,
    // strings and a longer identifier are all quiet.
    let quiet = "let started = gate_start(&mut state.borrow_mut(), tab, start);\n\
        match started { Ok(()) => send_tabs(state, webview), Err(_) => {} }\n\
        match layout.borrow().focus() { _ => {} }\n\
        match items.iter().map(|x| { state.borrow().n + x }).count() { _ => {} }\n\
        // match state.borrow() { _ => {} }\n\
        let m = \"match state.borrow() {\";\n\
        let rematch = 1; let matches = 2;\n\
        match x { _ => state.borrow().n }\n";
    assert_eq!(offences(quiet), vec![], "{:?}", offences(quiet));
}
