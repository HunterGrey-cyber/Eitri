//! Turn review observes the permission and send paths; it must never be read by them. A slow or
//! failing snapshot may make a review less precise, but it must never change, delay or decide an
//! answer to a permission request or a prompt being sent.
//!
//! This reads the answer and send paths as source text and fails on any mention of the review's
//! state inside them. Comments and string literals are blanked first, and every item is found by
//! its name and its braces, never by line, so formatting cannot hide a use or invent one. Each
//! region must be found and must hold a token it is known to contain, so a rename turns this red
//! rather than leaving it scanning nothing.

use std::path::Path;

/// What reading the review's state looks like in source: the installed review's field and every
/// type it hands out.
const FORBIDDEN: [&str; 4] = ["turn_review", "TurnReview", "TurnRecord", "ReviewHint"];

/// `text` with every comment, string literal and character literal replaced by spaces (line breaks
/// kept), so neither a brace nor a word inside one counts.
fn code_only(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let blank = |c: char| if c == '\n' { '\n' } else { ' ' };
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        if c == '/' && next == Some('*') {
            let mut depth = 0;
            while i < chars.len() {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    out.extend([' ', ' ']);
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    out.extend([' ', ' ']);
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    out.push(blank(chars[i]));
                    i += 1;
                }
            }
            continue;
        }
        // A raw string: `r"..."`, `r#"..."#`, also after `b`.
        let starts_word = i == 0 || !ident(chars[i - 1]) || (chars[i - 1] == 'b' && (i < 2 || !ident(chars[i - 2])));
        if c == 'r' && starts_word {
            let mut j = i + 1;
            while chars.get(j) == Some(&'#') {
                j += 1;
            }
            if chars.get(j) == Some(&'"') {
                let hashes = j - i - 1;
                out.extend(std::iter::repeat_n(' ', j - i + 1));
                i = j + 1;
                while i < chars.len() {
                    if chars[i] == '"' && (1..=hashes).all(|k| chars.get(i + k) == Some(&'#')) {
                        out.extend(std::iter::repeat_n(' ', hashes + 1));
                        i += hashes + 1;
                        break;
                    }
                    out.push(blank(chars[i]));
                    i += 1;
                }
                continue;
            }
        }
        if c == '"' {
            out.push(' ');
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    out.push(' ');
                    i += 1;
                }
                if i < chars.len() {
                    out.push(blank(chars[i]));
                    i += 1;
                }
            }
            out.push(' ');
            i += 1;
            continue;
        }
        if c == '\'' {
            // `'\n'`, `'\u{7b}'` or `'{'`; anything else is a lifetime.
            if next == Some('\\') {
                let mut j = i + 2;
                while j < chars.len() && chars[j] != '\'' {
                    j += 1;
                }
                out.extend(std::iter::repeat_n(' ', j + 1 - i));
                i = j + 1;
                continue;
            }
            if chars.get(i + 2) == Some(&'\'') {
                out.extend([' ', ' ', ' ']);
                i += 3;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out.into_iter().collect()
}

/// The positions just after every whole-word occurrence of `word`.
fn word_ends(code: &str, word: &str) -> Vec<usize> {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    code.match_indices(word)
        .filter(|(at, _)| !code[..*at].chars().next_back().is_some_and(ident))
        .map(|(at, w)| at + w.len())
        .filter(|end| !code[*end..].chars().next().is_some_and(ident))
        .collect()
}

/// The text from the `{` at `open` to its matching `}`, inclusive.
fn braced(code: &str, open: usize) -> &str {
    let mut depth = 0usize;
    for (at, c) in code[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &code[open..=open + at];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces from byte {open}");
}

/// The body of every `fn NAME`.
fn functions<'a>(code: &'a str, name: &str) -> Vec<&'a str> {
    word_ends(code, "fn")
        .into_iter()
        .filter_map(|after_fn| {
            let rest = &code[after_fn..];
            let trimmed = rest.trim_start();
            if !trimmed.starts_with(name)
                || trimmed[name.len()..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
            {
                return None;
            }
            let start = after_fn + (rest.len() - trimmed.len());
            let open = start + code[start..].find('{')?;
            Some(braced(code, open))
        })
        .collect()
}

/// The body of every match arm whose pattern names `path` (`Enum::Variant`): from its `=>` to the
/// end of the braced block or of the expression.
fn arms<'a>(code: &'a str, path: &str) -> Vec<&'a str> {
    let mut found = Vec::new();
    for end in word_ends(code, path) {
        // Find `=>` at this pattern's own nesting level; a `;`, a `,` or a closing bracket first
        // means this is not a pattern of an arm.
        let mut depth = 0i32;
        let mut arrow = None;
        let bytes = code.as_bytes();
        let mut at = end;
        while at < bytes.len() {
            match bytes[at] {
                b'{' | b'(' | b'[' => depth += 1,
                b'}' | b')' | b']' => {
                    depth -= 1;
                    if depth < 0 {
                        break;
                    }
                }
                b';' | b',' if depth == 0 => break,
                b'=' if depth == 0 && bytes.get(at + 1) == Some(&b'>') => {
                    arrow = Some(at + 2);
                    break;
                }
                _ => {}
            }
            at += 1;
        }
        let Some(arrow) = arrow else { continue };
        let body_start = arrow + (code[arrow..].len() - code[arrow..].trim_start().len());
        if code[body_start..].starts_with('{') {
            found.push(braced(code, body_start));
            continue;
        }
        // An expression body runs to the `,` that ends the arm, braces and all.
        let mut depth = 0i32;
        let mut stop = code.len();
        for (offset, b) in code[body_start..].bytes().enumerate() {
            match b {
                b'{' | b'(' | b'[' => depth += 1,
                b'}' | b')' | b']' => {
                    if depth == 0 {
                        stop = body_start + offset;
                        break;
                    }
                    depth -= 1;
                }
                b',' if depth == 0 => {
                    stop = body_start + offset;
                    break;
                }
                _ => {}
            }
        }
        found.push(&code[body_start..stop]);
    }
    found
}

/// Directories (relative to the workspace root) that hold the panel's code.
const PANEL_DIRS: &[&str] = &["shell/src", "panel/src"];

/// The panel's code: the one file under `PANEL_DIRS` that handles the page's messages. Found by what it
/// defines, not by its path, so the scan follows the panel when it moves and fails when it is gone.
fn panel_code() -> String {
    let read = eitri_core::source_scan::rust_sources(PANEL_DIRS);
    code_only(&eitri_core::source_scan::file_with(&read, "fn handle_inbound_message(").text)
}

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    code_only(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
}

/// Fails on a forbidden word in any of `regions`, and unless some region holds `anchor`.
fn check(what: &str, regions: &[&str], anchor: &str) {
    assert!(!regions.is_empty(), "{what}: not found (renamed? update this scan)");
    assert!(
        regions.iter().any(|r| !word_ends(r, anchor).is_empty()),
        "{what}: found, but none of it holds `{anchor}`; the scan may be reading the wrong item"
    );
    for region in regions {
        for word in FORBIDDEN {
            assert!(
                !region.contains(word),
                "{what} reads turn review state (`{word}`); the answer and send paths must not"
            );
        }
    }
}

#[test]
fn the_permission_and_send_paths_never_read_review_state() {
    // Every answer path of the backend lives in this file, the auto loop, bypass's approvals and
    // the send of a turn among them.
    let backend = read("src/agent_backend.rs");
    check(
        "core/src/agent_backend.rs",
        &[backend.as_str()],
        "answer_what_needs_no_human",
    );
    assert!(
        !functions(&backend, "send_turn").is_empty(),
        "AgentBackend::send_turn not found"
    );

    let tabs = read("src/tab_set.rs");
    for (name, anchor) in [
        ("answer_card", "respond_permission"),
        ("confirm_bypass", "approve_pending"),
        ("flush_queue", "send_turn"),
        ("send_now", "interrupt"),
        ("queue_message", "queued_at_ms"),
    ] {
        check(&format!("TabSet::{name}"), &functions(&tabs, name), anchor);
    }
    check(
        "TabSet::pump's resync arm",
        &arms(&tabs, "RevisedDelivery::Resync"),
        "approve_pending",
    );

    let panel = panel_code();
    check(
        "agent_panel's answer_permission_response",
        &functions(&panel, "answer_permission_response"),
        "answer_card",
    );
    check(
        "agent_panel's apply_confirm_bypass",
        &functions(&panel, "apply_confirm_bypass"),
        "confirm_bypass",
    );
    // The first turn of a session the send started goes out from here once the session is up,
    // outside the send's own arm.
    check(
        "agent_panel's send_first_turn",
        &functions(&panel, "send_first_turn"),
        "send_turn",
    );
    for (variant, anchor) in [
        ("SendMessage", "note_sent"),
        ("SendNow", "send_now"),
        ("QueueMessage", "queue_message"),
        ("PermissionResponse", "answer_permission_response"),
        ("ConfirmBypass", "apply_confirm_bypass"),
    ] {
        check(
            &format!("agent_panel's {variant} arm"),
            &arms(&panel, &format!("InboundMessage::{variant}")),
            anchor,
        );
    }
}

/// Every `.rs` file under `dir` (relative to the crate), with its relative path.
fn rust_files(dir: &str) -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
    let mut found = Vec::new();
    let mut pending = vec![root];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next).unwrap_or_else(|e| panic!("{}: {e}", next.display())) {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                found.push((path.display().to_string(), code_only(&text)));
            }
        }
    }
    found.sort();
    found
}

/// `code` with every one of `bodies` (slices of `code`, as `functions` returns them) blanked.
fn without(code: &str, bodies: &[&str]) -> String {
    let mut out = code.to_owned();
    for body in bodies {
        let start = body.as_ptr() as usize - code.as_ptr() as usize;
        out.replace_range(start..start + body.len(), &" ".repeat(body.len()));
    }
    out
}

/// The names of what `body` calls: every identifier directly followed by `(`.
fn called(body: &str) -> std::collections::BTreeSet<String> {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut calls = std::collections::BTreeSet::new();
    let chars: Vec<char> = body.chars().collect();
    for (at, c) in chars.iter().enumerate() {
        if *c != '(' {
            continue;
        }
        let end = at;
        let mut start = end;
        while start > 0 && ident(chars[start - 1]) {
            start -= 1;
        }
        if start < end {
            calls.insert(chars[start..end].iter().collect());
        }
    }
    calls
}

/// The only message a review sends is the one the user confirmed with `s` and `y`, and it takes the
/// ordinary queue and flush: nothing else in the review code sends, queues or flushes a turn.
#[test]
fn only_the_confirmed_review_send_reaches_the_send_path() {
    const SEND_PATH: [&str; 3] = ["send_turn", "queue_message", "flush_queue"];
    // The confirmed send's function in `flow.rs`: where the review flow hands the confirmed text to
    // `TabSet::send_review`.
    const CONFIRMED_SEND: &str = "confirmed_send";

    let files = rust_files("src/turn_review");
    assert!(
        files.iter().any(|(path, _)| path.ends_with("turn_review/draft.rs")),
        "the scan found no review sources"
    );
    for (path, code) in &files {
        let code = if path.ends_with("turn_review/flow.rs") {
            without(code, &functions(code, CONFIRMED_SEND))
        } else {
            code.clone()
        };
        for name in SEND_PATH {
            assert!(
                word_ends(&code, name).is_empty(),
                "{path} reaches `{name}`; only flow.rs's `{CONFIRMED_SEND}` may send a review"
            );
        }
    }

    // `TabSet::send_review` queues and flushes, and does nothing else.
    let tabs = read("src/tab_set.rs");
    let bodies = functions(&tabs, "send_review");
    assert_eq!(bodies.len(), 1, "TabSet::send_review not found");
    let calls = called(bodies[0]);
    assert_eq!(
        calls.iter().map(String::as_str).collect::<Vec<_>>(),
        ["Ok", "flush_queue", "queue_message", "to_owned"],
        "send_review calls only queue_message and flush_queue (and what shapes their arguments)"
    );
    for word in FORBIDDEN {
        assert!(!bodies[0].contains(word), "send_review reads review state (`{word}`)");
    }

    // And nothing on the answer and ordinary send paths reaches it.
    let panel = panel_code();
    let mut regions: Vec<&str> = Vec::new();
    for name in [
        "answer_card",
        "confirm_bypass",
        "flush_queue",
        "send_now",
        "queue_message",
    ] {
        regions.extend(functions(&tabs, name));
    }
    for name in ["answer_permission_response", "apply_confirm_bypass", "send_first_turn"] {
        regions.extend(functions(&panel, name));
    }
    for variant in [
        "SendMessage",
        "SendNow",
        "QueueMessage",
        "PermissionResponse",
        "ConfirmBypass",
    ] {
        regions.extend(arms(&panel, &format!("InboundMessage::{variant}")));
    }
    regions.extend(arms(&tabs, "RevisedDelivery::Resync"));
    assert!(regions.len() >= 12, "the answer and send paths were not all found");
    for region in regions {
        assert!(
            word_ends(region, "send_review").is_empty(),
            "an answer or ordinary send path reaches send_review"
        );
    }
}

/// The scan's own helpers: an exception is by function name, and a call is found by its name.
#[test]
fn the_send_scan_blanks_one_function_and_reads_calls() {
    let code = code_only("fn confirmed_send() { send_turn(); }\nfn other() { send_turn(); }\n");
    let body = functions(&code, "confirmed_send");
    assert_eq!(body.len(), 1);
    let rest = without(&code, &body);
    assert_eq!(
        word_ends(&rest, "send_turn").len(),
        1,
        "only the other function still calls it"
    );
    assert_eq!(
        called("{ self.queue_message(a, b.to_owned()); Ok(x) }")
            .into_iter()
            .collect::<Vec<_>>(),
        ["Ok", "queue_message", "to_owned"]
    );
}

/// The scan itself: it sees a use however the item is formatted, and ignores comments and strings.
#[test]
fn the_scan_finds_a_use_and_ignores_comments_and_strings() {
    let source = r##"
        // fn answer_card() { turn_review }
        pub fn
            answer_card(
            x: u8,
        ) -> Result<(), String> {
            let brace = '{';
            let text = "turn_review }";
            let raw = r#"TurnReview { "#;
            /* TurnRecord { */
            self.turn_review.as_ref();
            respond_permission();
        }
        fn answer_card_too() { turn_review }
        match delivery {
            RevisedDelivery::Resync => {
                approve_pending();
            }
            RevisedDelivery::Nothing => turn_review.poll(),
        }
        match m {
            InboundMessage::SendNow { text, .. } => send_now(match x { _ => 1 }),
            other => {}
        }
        let built = InboundMessage::SendNow { text: 1 };
    "##;
    let code = code_only(source);
    let found = functions(&code, "answer_card");
    assert_eq!(found.len(), 1, "the name is matched as a whole word");
    assert_eq!(
        found[0].matches("turn_review").count(),
        1,
        "only the real use: {}",
        found[0]
    );
    assert!(
        found[0].contains("respond_permission"),
        "the braces in a char and a string do not end it"
    );

    let resync = arms(&code, "RevisedDelivery::Resync");
    assert_eq!(resync.len(), 1);
    assert!(resync[0].contains("approve_pending") && !resync[0].contains("turn_review"));

    let send_now = arms(&code, "InboundMessage::SendNow");
    assert_eq!(send_now.len(), 1, "a construction is not an arm");
    assert!(send_now[0].contains("send_now") && !send_now[0].contains("other"));
}
