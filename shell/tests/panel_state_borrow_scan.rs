//! No `match`, `if let` or `while let` in `shell/src` takes a `RefCell` borrow of a panel's `state` in
//! its scrutinee.
//!
//! **Why.** A temporary in a `match` scrutinee lives until the end of the whole `match` (edition 2021),
//! so `match gate_start(&mut state.borrow_mut(), ..) { Ok(()) => send_tabs(state, ..), .. }` still
//! holds the mutable borrow when `send_tabs` borrows `state` again. That panics with "RefCell already
//! borrowed", and inside a WebKit signal handler a panic cannot unwind: the whole window aborts. It
//! happened on the first message sent in a project that needed the workspace-trust question. The value
//! is bound by a `let` first, which drops the borrow at the end of that statement. An `if let` and a
//! `while let` scrutinee keep their temporaries the same way, through the body and, for `if let`, the
//! `else` block as well (this crate is edition 2021), so the same rule holds for them: a body one
//! helper call away from `send_tabs` (or from anything that emits a signal whose handler borrows
//! `state`) is a latent abort even when it is harmless today.
//!
//! **What it reads.** Every `.rs` file under the directories in `SCANNED` (relative to the workspace
//! root; one that is missing or holds no such file fails the scan, and so does a missing anchor: the
//! panel's state, its message handler and the terminal pane's `send_command`, so a scan whose code moved
//! away cannot pass on what is left) as text, comments and string and character
//! literals blanked out first (so a message or a doc comment that names the shape is not one). For
//! each `match` keyword, and each `if` or `while` keyword followed by `let`, the scrutinee is
//! everything up to the `{` that opens the arms or the body -- a brace inside parentheses or brackets
//! (a closure body) does not count, and what such a braced body borrows is its own business; for the
//! `let` forms the scrutinee also starts after the pattern's `=`, so a struct pattern's braces do not
//! end it early -- with its whitespace normalised, so a scrutinee split over several lines reads the
//! same as one on a single line. It fails when that scrutinee calls `borrow`, `borrow_mut`,
//! `try_borrow` or `try_borrow_mut` on a receiver named `state` (or ending in `state`, `self.state`
//! included).
//!
//! **What it does not read.** Other `RefCell`s (a layout, a driver, a pane-switch channel) are not
//! named `state` and are not checked: their `if let` bodies were audited one by one and none reaches
//! its own cell again. A `let` that binds a guard (`let mut state_ref = state.borrow_mut();`) and an
//! `if` condition (whose temporaries end before the body) are not scrutinees.

use eitri_core::source_scan as scan;

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

/// What a scrutinee belongs to.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Form {
    Match,
    IfLet,
    WhileLet,
}

impl Form {
    fn keyword(self) -> &'static str {
        match self {
            Form::Match => "match",
            Form::IfLet => "if let",
            Form::WhileLet => "while let",
        }
    }
}

/// Is `c` at `at` the `=` of a `let` (not part of `==`, `..=`, `>=`, `<=`, `!=`, `=>`)?
fn is_binding_eq(chars: &[char], at: usize) -> bool {
    chars[at] == '='
        && !(at > 0 && matches!(chars[at - 1], '.' | '=' | '!' | '<' | '>'))
        && !matches!(chars.get(at + 1), Some('=') | Some('>'))
}

/// The scrutinee text that starts at byte `from` (just after the keyword or the `let`),
/// whitespace-normalised, or `None` when no arms' or body's `{` follows. With `after_eq`, the pattern
/// comes first and the scrutinee is what follows its `=`; a brace before that `=` belongs to the pattern.
fn scrutinee(code: &str, from: usize, after_eq: bool) -> Option<String> {
    let mut parens = 0i32;
    let mut braces = 0i32;
    let mut raw = String::new();
    let mut seen_eq = !after_eq;
    let chars: Vec<char> = code[from..].chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if !seen_eq {
            match c {
                '(' | '[' | '{' => parens += 1,
                ')' | ']' | '}' => parens -= 1,
                ';' => return None,
                _ if parens == 0 && is_binding_eq(&chars, i) => seen_eq = true,
                _ => {}
            }
            continue;
        }
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

/// `(line, form, scrutinee)` of every `match`, `if let` and `while let` in `src` whose scrutinee
/// borrows a `state` cell.
fn offences(src: &str) -> Vec<(usize, Form, String)> {
    let code = blank_comments_and_literals(src);
    let mut found = Vec::new();
    for (word, form) in [("match", Form::Match), ("if", Form::IfLet), ("while", Form::WhileLet)] {
        let mut from = 0;
        while let Some(rel) = code[from..].find(word) {
            let at = from + rel;
            from = at + word.len();
            let before = code[..at].chars().next_back();
            let after = code[from..].chars().next();
            if before.is_some_and(is_ident) || after.is_some_and(is_ident) {
                continue;
            }
            let start = if form == Form::Match {
                from
            } else {
                // `if` and `while` count only when `let` follows, however far down the line.
                let rest = &code[from..];
                let trimmed = rest.trim_start();
                match trimmed.strip_prefix("let") {
                    Some(tail) if !tail.chars().next().is_some_and(is_ident) => code.len() - tail.len(),
                    _ => continue,
                }
            };
            if let Some(scrut) = scrutinee(&code, start, form != Form::Match) {
                if borrows_state(&scrut) {
                    let line = code[..at].matches('\n').count() + 1;
                    found.push((line, form, scrut));
                }
            }
        }
    }
    found.sort_by_key(|(line, ..)| *line);
    found
}

/// Directories scanned, relative to the workspace root.
const SCANNED: &[&str] = &["shell/src", "panel/src", "mac/src"];

#[test]
fn no_scrutinee_holds_a_state_borrow() {
    let read = scan::rust_sources(SCANNED);
    // The panel's state, its message handler, and the terminal pane's own `state`.
    scan::require_anchors(
        &read,
        &[
            "struct AgentPanelState",
            "fn handle_inbound_message(",
            "fn send_command(",
        ],
    );
    let mut problems = Vec::new();
    for source in &read {
        for (line, form, scrut) in offences(&source.text) {
            problems.push(format!("{}:{line}: `{} {scrut}`", source.path, form.keyword()));
        }
    }
    assert!(
        problems.is_empty(),
        "a `match`, `if let` or `while let` scrutinee keeps its temporaries until the end of the whole \
         statement (the `else` block of an `if let` included), so a `state` borrow taken there is still \
         held in every arm and in the body; one that borrows `state` again (as `send_tabs` does) panics \
         inside a WebKit signal handler and aborts the window. Bind the value with a `let` first:\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_scan_sees_the_shape_that_crashed_and_not_its_bound_form() {
    let crashed = "fn f() {\n    match gate_start(&mut state.borrow_mut(), tab, start) {\n        Ok(()) => send_tabs(state, webview),\n        Err(why) => refuse(webview, &why),\n    }\n}\n";
    let hits = offences(crashed);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].0, 2);
    assert_eq!(hits[0].1, Form::Match);

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

#[test]
fn the_scan_sees_an_if_let_that_holds_a_state_borrow() {
    // The borrow outlives the then-block and the else block alike.
    let held = "fn f() {\n    if let Some(t) = state.borrow_mut().tabs.get_mut(tab) {\n        t.n = 1;\n    } else {\n        send_tabs(state, webview);\n    }\n}\n";
    let hits = offences(held);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!((hits[0].0, hits[0].1), (2, Form::IfLet));

    // Split over lines and spaces, an `else if let`, a `try_` borrow, a field receiver, a struct
    // pattern whose braces come before the `=`, and a pattern holding `..=`.
    let split = "if\n    let\n    Err(why) = self\n        .state\n        .try_borrow_mut()\n        .map(|s| s.n)\n{\n    return;\n}";
    assert_eq!(offences(split).len(), 1);
    let chained = "if x {\n} else if let Some(t) = state . borrow ( ).tabs.first() {\n}";
    let hits = offences(chained);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].0, 2);
    let pattern = "if let Tab { id, .. } = state.borrow().tabs.active() { f(id) }";
    assert_eq!(offences(pattern).len(), 1);
    let range = "if let 1..=5 = state.borrow().n { f() }";
    assert_eq!(offences(range).len(), 1);

    // The bound form, a guard named in a block, another cell, a pattern that borrows nothing, a closure
    // body in the scrutinee, a plain `if` whose condition borrows (its temporaries end first),
    // comments, strings and longer identifiers are all quiet.
    let quiet = "let begun = state.borrow_mut().tabs.begin(tab);\n\
        if let Err(why) = begun { refuse(&why) }\n\
        { let mut state_ref = state.borrow_mut(); if let Some(t) = state_ref.tabs.get_mut(tab) { t.n = 1; } }\n\
        if let Some(s) = layout.borrow().first() { f(s) }\n\
        if let Some(s) = pane_switch.borrow_mut().as_mut() { f(s) }\n\
        if let Some(n) = items.iter().map(|x| { state.borrow().n + x }).next() { f(n) }\n\
        if state.borrow().tabs.is_empty() { send_tabs(state, webview) }\n\
        // if let Some(t) = state.borrow() { }\n\
        let m = \"if let Some(t) = state.borrow() {\";\n\
        let notif = 1; let whiletrue = 2; let iflet = state.borrow().n;\n";
    assert_eq!(offences(quiet), vec![], "{:?}", offences(quiet));
}

#[test]
fn the_scan_sees_a_while_let_that_holds_a_state_borrow() {
    let held =
        "fn f() {\n    while let Some(job) = state.borrow_mut().jobs.pop() {\n        run(job, state);\n    }\n}\n";
    let hits = offences(held);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!((hits[0].0, hits[0].1), (2, Form::WhileLet));

    let split = "while\n    let Some(job) =\n        self.state\n            .borrow_mut()\n            .jobs\n            .pop()\n{\n    run(job);\n}";
    assert_eq!(offences(split).len(), 1);

    // Popped into a binding first (the borrow ends with that statement), another cell, a plain `while`
    // condition and a closure body are quiet.
    let quiet = "loop { let next = state.borrow_mut().jobs.pop(); let Some(job) = next else { break }; run(job); }\n\
        let mut next = state.borrow_mut().jobs.pop();\n\
        while let Some(job) = next { run(job); next = state.borrow_mut().jobs.pop(); }\n\
        while let Some(job) = queue.borrow_mut().pop() { run(job); }\n\
        while state.borrow().n > 0 { step(); }\n\
        while let Some(job) = jobs.drain(..).find(|j| { state.borrow().ok(j) }) { run(job); }\n";
    assert_eq!(offences(quiet), vec![], "{:?}", offences(quiet));
}
