//! Every turn Eitri starts is in the window's running set before its prompt reaches the agent: the tab
//! set's `begin_turn_send` puts its tab in `starting` and hands back the gate that lets it go. This reads
//! the code that starts turns as source text and fails if any function sends a turn without having come
//! through that gate: one that calls a send primitive must hold a `SendGate`, which only
//! `begin_turn_send` builds, or be one of the backend's own send primitives.

use eitri_core::source_scan::{braced, code_only, read_required, rust_sources, without, word_ends};

/// The files a turn of Eitri's is sent from: the tab set's and the backend's, and every file of the panel,
/// whose code is spread over the files of its crate (so a file it gains is read without naming it here).
fn files() -> Vec<String> {
    let mut files: Vec<String> = [
        "core/src/tab_set.rs",
        "core/src/tab_set/moving.rs",
        "core/src/agent_backend.rs",
    ]
    .map(String::from)
    .to_vec();
    files.extend(rust_sources(&["panel/src"]).into_iter().map(|source| source.path));
    files
}

/// The methods that put a turn on its way to an agent.
const SEND_METHODS: &[&str] = &["send_turn", "send_turn_in", "send_turn_as", "send_turn_with"];

/// The six places a turn leaves for an agent, each of which this scan must find: the tab set's two (its
/// queue, which carries a typed prompt, a queue flush at a turn's end, a review message and a resumed
/// session's prompt, and a new session's first message), the legacy session's and the sidecar
/// conversation's turn, a Claude turn sent through the mode-carrying door, and an external agent's (a
/// Codex `turn/start`, an ACP `session/prompt`).
const SENDS: &[(&str, &str)] = &[
    ("core/src/tab_set.rs", ".send_turn_in(&wire, &typed, mode, origin)"),
    (
        "core/src/tab_set.rs",
        ".send_turn_in(&turn.wire, &turn.typed, mode, turn.origin())",
    ),
    ("core/src/agent_backend.rs", "session.send_turn(wire_text)"),
    (
        "core/src/agent_backend.rs",
        "conversation.send_turn_as(wire_text, as_typed, origin, &client_message_id)",
    ),
    (
        "core/src/agent_backend.rs",
        "self.send_turn(wire_text, as_typed, origin)",
    ),
    (
        "core/src/agent_backend.rs",
        "conversation.send_turn_with(wire_text, turn_policy(mode))",
    ),
];

/// The backend's own send primitives: they are what a gated caller calls.
const PRIMITIVES: &[(&str, &str)] = &[
    ("core/src/agent_backend.rs", "send_turn"),
    ("core/src/agent_backend.rs", "send_turn_in"),
];

/// One function of `code`: its name, its signature (from `fn` to its body) and its body.
struct Function<'a> {
    name: &'a str,
    signature: &'a str,
    body: &'a str,
}

/// Every function with a body in `code`, nested ones included.
fn functions_of(code: &str) -> Vec<Function<'_>> {
    word_ends(code, "fn")
        .into_iter()
        .filter_map(|after_fn| {
            let rest = &code[after_fn..];
            let trimmed = rest.trim_start();
            let name_len = trimmed
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(trimmed.len());
            if name_len == 0 {
                return None;
            }
            let name = &trimmed[..name_len];
            let start = after_fn + (rest.len() - trimmed.len());
            let open = start + body_or_end(&code[start..])?;
            if code[open..].starts_with(';') {
                return None;
            }
            Some(Function {
                name,
                signature: &code[start..open],
                body: braced(code, open),
            })
        })
        .collect()
}

/// Where a function's signature ends: its body's `{`, or the `;` of a declaration, outside any
/// parenthesis or bracket (an array type's `;`, a default's braces).
fn body_or_end(signature: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (at, c) in signature.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            '{' | ';' if depth == 0 => return Some(at),
            _ => {}
        }
    }
    None
}

/// Whether `body` calls one of [`SEND_METHODS`] as a method (`.name(`, blanks allowed).
fn sends(body: &str) -> Vec<&'static str> {
    SEND_METHODS
        .iter()
        .copied()
        .filter(|method| {
            word_ends(body, method).into_iter().any(|end| {
                let before = body[..end - method.len()].trim_end();
                before.ends_with('.') && body[end..].trim_start().starts_with('(')
            })
        })
        .collect()
}

/// The code of `path` that ships: comments and literals blanked, and every module or function compiled
/// only for tests left out.
fn shipped(path: &str) -> String {
    let code = code_only(&read_required(path).text);
    let tests = test_only(&code);
    without(&code, &tests)
}

/// The bodies of the items of `code` marked `#[cfg(test)]`: a module or a function.
fn test_only(code: &str) -> Vec<&str> {
    let mark = "#[cfg(test)]";
    code.match_indices(mark)
        .filter_map(|(at, _)| {
            let item = &code[at + mark.len()..];
            let start = at + mark.len() + (item.len() - item.trim_start().len());
            let head = &code[start..];
            let is_item = ["mod ", "fn ", "pub fn ", "pub(crate) fn ", "pub(super) fn "]
                .iter()
                .any(|kind| head.starts_with(kind));
            if !is_item {
                return None;
            }
            let open = start + body_or_end(head)?;
            code[open..].starts_with('{').then(|| braced(code, open))
        })
        .collect()
}

/// The functions of `code` (read from `path`) that send a turn without a gate, by name.
fn ungated<'a>(path: &str, code: &'a str) -> Vec<&'a str> {
    functions_of(code)
        .into_iter()
        .filter(|f| !sends(f.body).is_empty())
        .filter(|f| !PRIMITIVES.contains(&(path, f.name)))
        .filter(|f| !f.signature.contains("SendGate"))
        .map(|f| f.name)
        .collect()
}

/// Every function of `sources` (each a path and its shipped code) that sends a turn without a gate, as
/// `path: name`: each file is read on its own, so a send that moves to another file is still found.
fn ungated_in(sources: &[(String, String)]) -> Vec<String> {
    sources
        .iter()
        .flat_map(|(path, code)| {
            ungated(path, code)
                .into_iter()
                .map(move |name| format!("{path}: {name}"))
        })
        .collect()
}

/// Whether the `SendGate` ending at `at` in `code` builds one (`SendGate { .. }` as an expression), not
/// names the type where it is declared, implemented or returned.
fn builds_a_gate(code: &str, at: usize) -> bool {
    let before = code[..at - "SendGate".len()].trim_end();
    code[at..].trim_start().starts_with('{')
        && !["struct", "impl", "for", "->"]
            .iter()
            .any(|word| before.ends_with(word))
}

#[test]
fn every_turn_starting_send_goes_through_begin_turn_send() {
    for (path, send) in SENDS {
        let code = shipped(path);
        let compact = |s: &str| s.split_whitespace().collect::<String>();
        assert!(
            compact(&code).contains(&compact(send)),
            "{path} no longer has `{send}`: a turn now leaves for its agent from somewhere this scan does not name"
        );
    }
    let sources: Vec<(String, String)> = files()
        .into_iter()
        .map(|path| {
            let code = shipped(&path);
            (path, code)
        })
        .collect();
    let open = ungated_in(&sources);
    assert!(
        open.is_empty(),
        "{open:?} send a turn without the gate `begin_turn_send` hands out, so the turn could reach the agent \
         before its tab is in the running set"
    );
    // A gate is the proof its tab entered `starting`: only `begin_turn_send` may build one.
    let code = shipped("core/src/tab_set/moving.rs");
    let builder = functions_of(&code)
        .into_iter()
        .find(|f| f.name == "begin_turn_send")
        .expect("the tab set's `begin_turn_send` is in core/src/tab_set/moving.rs");
    let start = builder.body.as_ptr() as usize - code.as_ptr() as usize;
    let inside = start..start + builder.body.len();
    for at in word_ends(&code, "SendGate") {
        assert!(
            !builds_a_gate(&code, at) || inside.contains(&at),
            "a `SendGate` is built outside `begin_turn_send`, so a send could hold one whose tab never entered \
             `starting`"
        );
    }
    for path in files().iter().filter(|p| p.as_str() != "core/src/tab_set/moving.rs") {
        let code = shipped(path);
        let built = word_ends(&code, "SendGate")
            .into_iter()
            .any(|at| builds_a_gate(&code, at));
        assert!(!built, "{path} builds a `SendGate` itself");
    }
}

#[test]
fn a_send_outside_the_gate_is_caught() {
    let code = "fn free(b: &mut B) { b.send_turn_in(&w, &t, m); }\n\
                fn gated(&mut self, gate: SendGate) { tab . send_turn ( x ); }\n\
                fn send_turn(&mut self) { self.inner.send_turn(x); }\n\
                fn unrelated() { resend_turn(); not_a_send_turn_in(); }\n\
                fn declared(&self);\n\
                fn arrays(x: [u8; 3], b: &mut B) { b.send_turn_with(x); }";
    assert_eq!(ungated("core/src/agent_backend.rs", code), vec!["free", "arrays"]);
    assert_eq!(
        ungated("core/src/tab_set.rs", code),
        vec!["free", "send_turn", "arrays"]
    );
}

#[test]
fn a_send_outside_the_gate_in_a_second_panel_file_is_caught() {
    // The panel's code is spread over the files of its crate: a gated send in one file must not hide an
    // ungated one in another.
    let sources = [
        (
            "panel/src/agent_panel.rs".to_string(),
            "fn gated(&mut self, gate: SendGate) { tab.send_turn(x); }".to_string(),
        ),
        (
            "panel/src/starts.rs".to_string(),
            "fn free(b: &mut B) { b.send_turn_in(&w, &t, m); }".to_string(),
        ),
    ];
    assert_eq!(ungated_in(&sources), vec!["panel/src/starts.rs: free"]);
    assert!(
        files().iter().any(|path| path == "panel/src/starts.rs"),
        "the scan reads every file of panel/src, so a send in a file other than agent_panel.rs is checked"
    );
}
