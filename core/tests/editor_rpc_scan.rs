//! Lua sent to the user's editor is always a compile-time constant; every value goes as an
//! argument. A path or a line of a file spliced into Lua source would run as code in the user's
//! nvim, so this reads `core/src` and `shell/src` as text and fails on:
//!
//! - a call of `exec_lua`, `exec_lua_answered` or `exec_lua_for` whose code argument is not a
//!   string literal or a path ending in an `UPPER_SNAKE` constant. A wrapper that passes its own
//!   caller's constant on carries `// editor-rpc-scan: forwards a caller's constant` on the call's
//!   line or the line above, and every such wrapper is listed in [`EXPECTED_FORWARDERS`], so a new
//!   one is red until someone has looked at it. The chunk inventory also covers `exec_lua_watched`:
//!   its owned-editor quit template is refused, as are its generated forms;
//! - `leak` called as a method or a path (`.leak()`, `Box::leak(..)`, `.map(String::leak)`) outside
//!   a `#[cfg(test)]` module: leaking is the one way to make a built `String` a `&'static str`. A
//!   test module kept in a file of its own is read as product code, which can only turn this red;
//! - a `"nvim_exec_lua"` string literal outside `core/src/nvim_rpc.rs` and test modules: calling the
//!   method by name on a link would go around the typed calls (a test may name it to check what
//!   went over the wire).
//!
//! A constant is not enough for the one chunk that evaluates an argument: `exec_lua(AGENT_EXEC_LUA, anything)` would
//! pass all of the above. So this also fails on:
//!
//! - `AGENT_EXEC_LUA`, or its file's name, anywhere but the exec module;
//! - the chunk's arguments coming from anything but the approval record, or that record being made anywhere but
//!   `from_card`, being public, going by a second name, being cloned, defaulted or deserialized into existence, or
//!   being written to after the card;
//! - a Lua chunk or a Rust literal that can evaluate text, outside the short list in [`EVALUATORS`];
//! - another tool chunk that builds source, by the words `review_lua_builds_no_source` holds `review.lua` to, or that
//!   can run text some other way ([`RUNNERS`]), but for the constant calls in [`LISTED_COMMANDS`];
//! - a caller of the broker's `answer_card` other than a person's answer: a sweep through it with `Key` would make a
//!   real `CardAnswer`. The one call of `TabSet::answer_card` is pinned as well, so the name of a receiver lets nothing
//!   through.
//!
//! Comments and literals are blanked before anything is matched, and calls are found by their
//! tokens, never by where a line ends, so formatting can neither hide a call nor invent one.

use std::collections::BTreeSet;
use std::ops::Range;

use eitri_core::source_scan::{self, word_ends, Source};

const ANNOTATION: &str = "// editor-rpc-scan: forwards a caller's constant";

const RELOAD_PART: &str = "core/src/companion/reload_part.lua";

#[test]
fn the_agent_reload_part_comes_from_its_compile_time_constant() {
    let module = source_scan::read_required("core/src/companion/mod.rs");
    let (_, body) = source_scan::function_in(&[module], "install_args");
    assert!(squeeze(&body).contains("part(,RELOAD_PART_LUA,"));
    assert_eq!(
        eitri_core::companion::RELOAD_PART_LUA,
        source_scan::read_required(RELOAD_PART).text
    );
}

/// Every annotated forwarder, by file (relative to the workspace root) and count. Reviewed: each passes on a
/// `code` its own caller gave it, and that caller's call is checked here in turn.
const EXPECTED_FORWARDERS: &[(&str, usize)] = &[
    ("core/src/companion/driver.rs", 5),
    ("core/src/companion/link.rs", 2),
    ("core/src/editor_rpc.rs", 2),
    ("shell/src/editor_rpc.rs", 1),
    // The takeover window's handle forwards to the pane, and the pane's check runs the takeover's rule on the code it
    // was given.
    ("shell/src/takeover.rs", 2),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Code,
    Comment,
    Literal,
}

/// What each char of `chars` is: code, inside a comment, or inside a string or char literal
/// (quotes included).
fn kinds(chars: &[char]) -> Vec<Kind> {
    let mut out: Vec<Kind> = Vec::with_capacity(chars.len());
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                out.push(Kind::Comment);
                i += 1;
            }
            continue;
        }
        if c == '/' && next == Some('*') {
            let mut depth = 0;
            while i < chars.len() {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    out.extend([Kind::Comment, Kind::Comment]);
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    out.extend([Kind::Comment, Kind::Comment]);
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    out.push(Kind::Comment);
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
                out.extend(std::iter::repeat_n(Kind::Literal, j - i + 1));
                i = j + 1;
                while i < chars.len() {
                    if chars[i] == '"' && (1..=hashes).all(|k| chars.get(i + k) == Some(&'#')) {
                        out.extend(std::iter::repeat_n(Kind::Literal, hashes + 1));
                        i += hashes + 1;
                        break;
                    }
                    out.push(Kind::Literal);
                    i += 1;
                }
                continue;
            }
        }
        if c == '"' {
            out.push(Kind::Literal);
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    out.push(Kind::Literal);
                    i += 1;
                }
                if i < chars.len() {
                    out.push(Kind::Literal);
                    i += 1;
                }
            }
            if i < chars.len() {
                out.push(Kind::Literal);
                i += 1;
            }
            continue;
        }
        if c == '\'' {
            // `'\n'`, `'\u{7b}'` or `'{'`; anything else is a lifetime.
            if next == Some('\\') {
                let mut j = i + 2;
                while j < chars.len() && chars[j] != '\'' {
                    j += 1;
                }
                let end = (j + 1).min(chars.len());
                out.extend(std::iter::repeat_n(Kind::Literal, end - i));
                i = end;
                continue;
            }
            if chars.get(i + 2) == Some(&'\'') {
                out.extend([Kind::Literal; 3]);
                i += 3;
                continue;
            }
        }
        out.push(Kind::Code);
        i += 1;
    }
    out
}

/// `chars` with every comment and literal replaced by spaces (line breaks kept), char for char, so a
/// position in one is the same position in the other.
fn code_only(chars: &[char], kinds: &[Kind]) -> Vec<char> {
    chars
        .iter()
        .zip(kinds)
        .map(|(&c, &kind)| match (kind, c) {
            (Kind::Code, _) | (_, '\n') => c,
            _ => ' ',
        })
        .collect()
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The start of every whole-word occurrence of `word` in `code`.
fn word_starts(code: &[char], word: &str) -> Vec<usize> {
    let word: Vec<char> = word.chars().collect();
    (0..code.len().saturating_sub(word.len() - 1))
        .filter(|&at| code[at..at + word.len()] == word[..])
        .filter(|&at| at == 0 || !is_ident(code[at - 1]))
        .filter(|&at| code.get(at + word.len()).is_none_or(|c| !is_ident(*c)))
        .collect()
}

/// The first position before `at` that is not whitespace.
fn back_over_space(code: &[char], at: usize) -> Option<usize> {
    (0..at).rev().find(|&i| !code[i].is_whitespace())
}

/// The first position from `at` on that is not whitespace.
fn forward_over_space(code: &[char], at: usize) -> Option<usize> {
    (at..code.len()).find(|&i| !code[i].is_whitespace())
}

/// The position of the bracket closing the one at `open`, counting `()`, `[]` and `{}` alike.
fn closing(code: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (at, &c) in code.iter().enumerate().skip(open) {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

/// The arguments between the parenthesis at `open` and the one at `close`, as position ranges,
/// split at top-level commas. A trailing comma makes no empty argument.
fn arguments(code: &[char], open: usize, close: usize) -> Vec<(usize, usize)> {
    let mut args = Vec::new();
    let mut depth = 0usize;
    let mut start = open + 1;
    for (at, &c) in code.iter().enumerate().take(close).skip(open + 1) {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                args.push((start, at));
                start = at + 1;
            }
            _ => {}
        }
    }
    if code[start..close].iter().any(|c| !c.is_whitespace()) {
        args.push((start, close));
    }
    args
}

/// The `#[cfg(test)] mod NAME { .. }` regions, as position ranges, found in blanked code.
fn test_modules(code: &[char]) -> Vec<(usize, usize)> {
    // The attribute with any spacing inside it: matched over the code with whitespace left out.
    let squeezed: Vec<(usize, char)> = code
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, c)| !c.is_whitespace())
        .collect();
    let attribute: Vec<char> = "#[cfg(test)]".chars().collect();
    let mut regions = Vec::new();
    for at in 0..squeezed.len().saturating_sub(attribute.len() - 1) {
        if !squeezed[at..at + attribute.len()]
            .iter()
            .map(|(_, c)| *c)
            .eq(attribute.iter().copied())
        {
            continue;
        }
        // What follows the attribute, in the original positions.
        let mut i = squeezed[at + attribute.len() - 1].0 + 1;
        // Further attributes and a visibility may stand between the attribute and `mod`.
        while let Some(next) = forward_over_space(code, i) {
            if code[next] == '#' {
                match forward_over_space(code, next + 1)
                    .filter(|&b| code[b] == '[')
                    .and_then(|b| closing(code, b))
                {
                    Some(end) => i = end + 1,
                    None => break,
                }
            } else if code[next..].starts_with(&['p', 'u', 'b']) && code.get(next + 3).is_none_or(|c| !is_ident(*c)) {
                i = next + 3;
                if let Some(paren) = forward_over_space(code, i).filter(|&p| code[p] == '(') {
                    match closing(code, paren) {
                        Some(end) => i = end + 1,
                        None => break,
                    }
                }
            } else {
                i = next;
                break;
            }
        }
        if !(code[i..].starts_with(&['m', 'o', 'd']) && code.get(i + 3).is_some_and(|c| c.is_whitespace())) {
            continue;
        }
        let Some(open) = (i + 3..code.len()).find(|&p| code[p] == '{' || code[p] == ';') else {
            continue;
        };
        if code[open] == ';' {
            continue;
        }
        if let Some(close) = closing(code, open) {
            regions.push((open, close));
        }
    }
    regions
}

/// Whether the blanked `arg` (with its original `raw`) is a compile-time constant: a string
/// literal, or a path whose last segment is `UPPER_SNAKE`.
fn is_constant(arg: &str, raw: &str) -> bool {
    let blank = arg.trim();
    let raw = raw.trim();
    if blank.is_empty() {
        return raw.starts_with('"') || raw.starts_with("r\"") || raw.starts_with("r#");
    }
    let segments: Vec<&str> = blank.split("::").map(str::trim).collect();
    let ident = |s: &str| {
        let mut chars = s.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    let last = segments.last().copied().unwrap_or_default();
    segments.iter().all(|s| ident(s))
        && last.starts_with(|c: char| c.is_ascii_uppercase())
        && last
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// What one file holds.
#[derive(Debug, Default)]
struct Report {
    /// What is wrong, one line each, naming the file and line.
    flagged: Vec<String>,
    /// The lines of the annotated forwarders.
    forwarders: Vec<usize>,
    /// The code argument of every call that passed as a constant.
    constants: Vec<String>,
}

/// The code argument's position among the call's arguments: `exec_lua_for` takes the part first,
/// and the path form takes the receiver first.
fn code_argument(name: &str, path_form: bool) -> usize {
    let base = if name == "exec_lua_for" { 1 } else { 0 };
    base + usize::from(path_form)
}

fn scan(rel: &str, text: &str) -> Report {
    scan_calls(rel, text, &["exec_lua", "exec_lua_answered", "exec_lua_for"])
}

fn scan_calls(rel: &str, text: &str, names: &[&str]) -> Report {
    let chars: Vec<char> = text.chars().collect();
    let kinds = kinds(&chars);
    let code = code_only(&chars, &kinds);
    let line_of = |at: usize| chars[..at].iter().filter(|c| **c == '\n').count() + 1;
    let lines: Vec<&str> = text.lines().collect();
    let annotated = |line: usize| {
        let here = lines.get(line - 1).copied().unwrap_or_default();
        let above = lines[..line - 1]
            .iter()
            .rev()
            .find(|l| !l.trim().is_empty())
            .copied()
            .unwrap_or_default();
        here.contains(ANNOTATION) || above.contains(ANNOTATION)
    };
    let slice = |from: usize, to: usize, of: &[char]| of[from..to].iter().collect::<String>();
    let mut report = Report::default();

    for &name in names {
        for at in word_starts(&code, name) {
            let path_form = match back_over_space(&code, at) {
                Some(p) if code[p] == '.' => false,
                Some(p) if code[p] == ':' && p > 0 && code[p - 1] == ':' => true,
                _ if names.contains(&"exec_lua_watched") => {
                    let prefix: String = code[..at].iter().collect();
                    if prefix.split_whitespace().last() == Some("fn") {
                        continue;
                    }
                    false
                }
                _ => continue,
            };
            let Some(open) = forward_over_space(&code, at + name.len()).filter(|&p| code[p] == '(') else {
                continue;
            };
            let line = line_of(at);
            let Some(close) = closing(&code, open) else {
                report.flagged.push(format!("{rel}:{line}: `{name}(` is never closed"));
                continue;
            };
            let args = arguments(&code, open, close);
            let Some(&(from, to)) = args.get(code_argument(name, path_form)) else {
                report
                    .flagged
                    .push(format!("{rel}:{line}: `{name}(` has no code argument"));
                continue;
            };
            let (blank, raw) = (slice(from, to, &code), slice(from, to, &chars));
            if is_constant(&blank, &raw) {
                report.constants.push(raw.trim().to_owned());
            } else if annotated(line) {
                report.forwarders.push(line);
            } else {
                report.flagged.push(format!(
                    "{rel}:{line}: `{name}` is passed `{}`, not a literal or an UPPER_SNAKE constant",
                    raw.trim()
                ));
            }
        }
    }

    let tests = test_modules(&code);
    let in_tests = |at: usize| tests.iter().any(|&(from, to)| from < at && at < to);
    for at in word_starts(&code, "leak") {
        let called = match back_over_space(&code, at) {
            Some(p) => code[p] == '.' || (code[p] == ':' && p > 0 && code[p - 1] == ':'),
            None => false,
        };
        if called && !in_tests(at) {
            report
                .flagged
                .push(format!("{rel}:{}: `leak` outside a test module", line_of(at)));
        }
    }

    if rel != "core/src/nvim_rpc.rs" {
        let word: Vec<char> = "\"nvim_exec_lua\"".chars().collect();
        for at in 0..chars.len().saturating_sub(word.len() - 1) {
            if chars[at..at + word.len()] == word[..]
                && kinds[at..at + word.len()].iter().all(|k| *k == Kind::Literal)
                && !in_tests(at)
            {
                report
                    .flagged
                    .push(format!("{rel}:{}: a `\"nvim_exec_lua\"` literal", line_of(at)));
            }
        }
    }
    report
}

#[test]
fn editor_rpc_callers_pass_only_constant_lua() {
    let read = eitri_core::source_scan::rust_sources(&["core/src", "shell/src", "panel/src", "mac/src"]);
    eitri_core::source_scan::require_anchors(
        &read,
        &[
            "impl EditorRpc for EmbeddedEditorRpc",
            "impl EditorRpc for CompanionLink",
            "fn handle_inbound_message(",
        ],
    );
    assert!(read.len() >= 40, "the walk found only {} files", read.len());

    let mut flagged = Vec::new();
    let mut forwarders: Vec<(String, usize)> = Vec::new();
    let mut install_seen = false;
    let mut follow_seen = false;
    let mut cwd_seen = false;
    let mut buffers_seen = false;
    for source in &read {
        let rel = source.path.clone();
        let report = scan(&rel, &source.text);
        if rel == "core/src/companion/driver.rs" && report.constants.iter().any(|c| c == "INSTALL_LUA") {
            install_seen = true;
        }
        if rel == "core/src/plugin/editor.rs" && report.constants.iter().any(|c| c == "PLUGIN_FOLLOW_LUA") {
            follow_seen = true;
        }
        if rel == "core/src/plugin/editor.rs" && report.constants.iter().any(|c| c == "PLUGIN_CWD_LUA") {
            cwd_seen = true;
        }
        if rel == "core/src/plugin/editor.rs" && report.constants.iter().any(|c| c == "PLUGIN_BUFFERS_LUA") {
            buffers_seen = true;
        }
        flagged.extend(report.flagged);
        if !report.forwarders.is_empty() {
            forwarders.push((rel, report.forwarders.len()));
        }
    }
    assert!(
        install_seen,
        "the install's own call (`INSTALL_LUA` in core/src/companion/driver.rs) was not found: the scan reads nothing"
    );
    assert!(
        follow_seen,
        "the follow snippet's calls (PLUGIN_FOLLOW_LUA in core/src/plugin/editor.rs) were not found"
    );
    assert!(
        cwd_seen,
        "the cwd snippet calls (PLUGIN_CWD_LUA in core/src/plugin/editor.rs) were not found"
    );
    assert!(
        buffers_seen,
        "the buffer snippet calls (PLUGIN_BUFFERS_LUA in core/src/plugin/editor.rs) were not found"
    );
    assert!(
        flagged.is_empty(),
        "Lua that is not a constant:\n{}",
        flagged.join("\n")
    );
    let mut expected: Vec<(String, usize)> = EXPECTED_FORWARDERS.iter().map(|(f, n)| (f.to_string(), *n)).collect();
    expected.sort();
    forwarders.sort();
    assert_eq!(
        forwarders, expected,
        "the annotated forwarders changed; review the new one and list it"
    );
}

#[test]
fn the_scan_catches_built_lua_a_leak_and_a_bypass() {
    let flagged = |text: &str| scan("x.rs", text).flagged;
    for bad in [
        "fn f() { x.exec_lua(&format!(\"return {}\", n), v); }",
        "fn f() {\n    Wire::exec_lua(\n        &l,\n        code,\n        a,\n    );\n}",
        "fn f() { l.exec_lua_for(\"scratch\", code, a); }",
        "fn f() { l.exec_lua_answered(make(), a); }",
        "fn f() { let s: &'static str = s.leak(); }",
        "fn f() { Box::leak(b); }",
        "fn f() { v.into_iter().map(String::leak); }",
        "fn f() { link.call(\"nvim_exec_lua\", vec![]); }",
    ] {
        assert_eq!(flagged(bad).len(), 1, "not caught: {bad}");
    }

    for good in [
        "fn f() { l.exec_lua(\n  SOME::PATH_LUA,\n v); }",
        "fn f() { l.exec_lua(\"return 1\", v); }",
        "fn f() { l.exec_lua(r#\"return \"x\"\"#, v); }",
        "fn f() { l.exec_lua_for(\"scratch\", SCRATCH_CALL_LUA, a); }",
        "fn f() { Wire::exec_lua(&l, TEARDOWN_LUA, a); }",
        "// x.exec_lua(&format!(\"{}\", n), v)\nfn f() { let s = \"x.exec_lua(format!(y), v)\"; }",
        "fn f() {}\n#[cfg(test)]\nmod t {\n    fn g() { Box::leak(b); }\n}",
        "#[cfg(test)]\npub(crate) mod t { fn g() { s.leak(); } }",
        "fn exec_lua_answered(&self, code: &'static str, args: Vec<Value>) -> Pending { todo!() }",
        "fn f() { pane.exec_lua_watched(lua); }",
        "/// `link.call(\"nvim_exec_lua\", ..)` in a doc\nfn f() {}",
        "#[cfg(test)]\nmod t { fn g() { assert_eq!(method, \"nvim_exec_lua\"); } }",
    ] {
        assert_eq!(flagged(good), Vec::<String>::new(), "flagged: {good}");
    }

    // An annotated forwarder passes, and is counted.
    let forwarder = format!("fn f() {{\n    {ANNOTATION}\n    Wire::exec_lua(\n        &l, code, a);\n}}");
    let report = scan("x.rs", &forwarder);
    assert!(report.flagged.is_empty(), "{:?}", report.flagged);
    assert_eq!(report.forwarders, vec![3]);

    // A leak in a `#[cfg(test)] fn` outside a test module is still product-adjacent code.
    assert_eq!(flagged("#[cfg(test)]\nfn g() { s.leak(); }").len(), 1);
}

/// What the editor-op dispatcher may call of the agent tools: `(tool, function)`.
const TOOL_CALLS: [(&str, &str); 6] = [
    ("open_file", "send"),
    ("editor_state", "send"),
    ("read_buffer", "send"),
    ("diagnostics", "send"),
    ("lsp", "ask"),
    ("lsp", "collect"),
];

/// The typed records those calls take. Named only where one is built.
const TOOL_RECORDS: [&str; 2] = ["diagnostics::DiagnosticsArgs", "lsp::AskArgs"];

/// Values of the tools the dispatcher may read, none of which sends anything: the bounds an op is held to, and the
/// severity a baseline asks from.
const TOOL_VALUES: [&str; 4] = [
    "lsp::LSP_BUDGET",
    "lsp::COLLECT_MARGIN",
    "baseline::BASELINE_LIMIT",
    "diagnostics::Severity::Hint",
];

/// The `tool::function` calls of [`TOOL_CALLS`] this whitespace-free product code makes. Like the plugin host's
/// functions, each is named by its full path at the call, so an alias cannot hide another; `Err` says what breaks
/// that, or names a function outside the table. The stamp's module and `paths::file_check` are checks that send nothing.
fn tool_calls(squeezed: &str) -> Result<std::collections::BTreeSet<String>, String> {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let excerpt = |text: &str| text.chars().take(40).collect::<String>();
    let mut sends = std::collections::BTreeSet::new();
    let mut calls = 0;
    for (at, _) in squeezed.match_indices("editor_tools") {
        if squeezed[..at].chars().next_back().is_some_and(is_ident) {
            continue;
        }
        let after = &squeezed[at + "editor_tools".len()..];
        let Some(rest) = after.strip_prefix("::") else {
            return Err(format!("`editor_tools` is named but not as a path: {}", excerpt(after)));
        };
        if rest.starts_with("stamp::") || rest.starts_with("paths::file_check(") {
            continue;
        }
        let named = rest.strip_prefix("tools::").and_then(|tool_on| {
            let tool: String = tool_on.chars().take_while(|c| is_ident(*c)).collect();
            Some((tool_on[tool.len()..].strip_prefix("::")?, tool))
        });
        let Some((function, tool)) = named else {
            return Err(format!(
                "the dispatcher reaches into the editor tools: {}",
                excerpt(rest)
            ));
        };
        let name: String = function.chars().take_while(|c| is_ident(*c)).collect();
        let after_name = &function[name.len()..];
        let item = format!("{tool}::{name}");
        let path = format!("{tool}::{function}");
        let is_value = TOOL_VALUES.iter().any(|value| {
            path.strip_prefix(value)
                .is_some_and(|after| !after.chars().next().is_some_and(is_ident))
        });
        let is_record = after_name.starts_with('{') && TOOL_RECORDS.contains(&item.as_str());
        if after_name.starts_with('(') && TOOL_CALLS.contains(&(tool.as_str(), name.as_str())) {
            calls += 1;
            sends.insert(item);
        } else if !is_value && !is_record {
            return Err(format!("the dispatcher names {tool}::{}", excerpt(function)));
        }
    }
    let functions: std::collections::BTreeSet<&str> = TOOL_CALLS.iter().map(|(_, function)| *function).collect();
    let spelled: usize = functions
        .iter()
        .map(|function| squeezed.matches(&format!("::{function}(")).count())
        .sum();
    if spelled != calls {
        return Err("the dispatcher makes a call outside its table".to_owned());
    }
    Ok(sends)
}

#[test]
fn a_dispatcher_naming_an_unlisted_tool_function_fails() {
    let listed = |text: &str| tool_calls(text).unwrap().into_iter().collect::<Vec<_>>();
    assert_eq!(
        listed("crate::editor_tools::tools::open_file::send(rpc,&stamp)"),
        ["open_file::send"]
    );
    assert_eq!(
        listed(
            "usecrate::editor_tools::stamp::{wall_ms,Stamp};crate::editor_tools::paths::file_check(&path);\
             crate::editor_tools::tools::diagnostics::send(rpc,&stamp,&crate::editor_tools::tools::diagnostics::DiagnosticsArgs{})"
        ),
        ["diagnostics::send"]
    );
    assert_eq!(
        listed(
            "constMAX:Duration=crate::editor_tools::tools::lsp::LSP_BUDGET.saturating_add(crate::editor_tools::tools::lsp::COLLECT_MARGIN);\
             crate::editor_tools::tools::lsp::ask(rpc,&stamp,&crate::editor_tools::tools::lsp::AskArgs{id});\
             crate::editor_tools::tools::lsp::collect(rpc,&stamp,&id,last);"
        ),
        ["lsp::ask", "lsp::collect"]
    );
    assert_eq!(
        listed(
            "crate::editor_tools::tools::diagnostics::send(rpc,&stamp,&crate::editor_tools::tools::diagnostics::DiagnosticsArgs{\
             min_severity:crate::editor_tools::tools::diagnostics::Severity::Hint,\
             limit:crate::editor_tools::tools::baseline::BASELINE_LIMIT})"
        ),
        ["diagnostics::send"]
    );
    for bad in [
        "crate::editor_tools::tools::nvim_exec::send(rpc,&stamp)",
        "crate::editor_tools::tools::open_file::parse(&value)",
        "crate::editor_tools::tools::open_file::OPEN_FILE::x",
        "usecrate::editor_tools::tools::open_fileaso;o::send(rpc,&stamp)",
        "usecrate::editor_tools::tools;tools::nvim_exec::send(rpc)",
        "usecrate::editor_tools::tools::{open_file};open_file::send(rpc)",
        "usecrate::editor_toolsastools;tools::tools::nvim_exec::send(rpc)",
        "usecrate::editor_tools::{tools};",
        "crate::editor_tools::paths::judge(root,home,raw)",
        "crate::editor_tools::broker::Broker::new()",
        "nvim_exec::send(rpc,&stamp)",
        "crate::editor_tools::tools::lsp::rename(rpc,&stamp)",
        "crate::editor_tools::tools::lsp::send(rpc,&stamp)",
        "crate::editor_tools::tools::open_file::ask(rpc,&stamp)",
        "crate::editor_tools::tools::lsp::parse_asked(&value)",
        "crate::editor_tools::tools::lsp::ask",
        "usecrate::editor_tools::tools::lsp::askasq;q(rpc,&stamp)",
        "usecrate::editor_tools::tools::lsp::{ask};ask(rpc,&stamp)",
        "usecrate::editor_tools::tools::lspasl;l::rename(rpc,&stamp)",
        "crate::editor_tools::tools::lsp::AskArgsasA",
        "crate::editor_tools::tools::lsp::LSP_BUDGET_MORE",
        "crate::editor_tools::tools::lsp::LSP_ASK_LUA",
        "crate::editor_tools::tools::baseline::BaselineRun::new(turn)",
        "crate::editor_tools::tools::diagnostics::Severity::Error",
        "lsp::ask(rpc,&stamp)",
        "lsp::collect(rpc,&stamp,&id,last)",
    ] {
        assert!(tool_calls(bad).is_err(), "not caught: {bad}");
    }
}

/// Window 2 reaches the editor only through the editor-op dispatcher, so the dispatcher's product code may name
/// exactly two Lua constants (the buffer-state chunk and the standalone shown question, each sent by its one
/// `exec_lua` call with arguments its own builder makes), the plugin host's editor functions of its table and the
/// agent tools' functions of theirs, and nothing else that runs in the editor: no follow teardown (the snippet and its
/// commands are window 1's, whatever window 2's page does), no review chunk (the review ops, live ones included,
/// become calls on window 1's own review driver, of the five kinds its table names), no scratch call, no event drain,
/// no navigation fallback, and no pane kill. Comments and literals are blanked and whitespace is dropped, so
/// formatting changes nothing.
#[test]
fn the_editor_op_dispatcher_names_only_its_table() {
    use eitri_core::source_scan::{code_only, modules, read_required, without};

    let source = read_required("core/src/window_link/editor_ops.rs");
    let all = code_only(&source.text);
    let tests = modules(&all, "tests");
    assert!(tests.len() <= 1, "more than one `mod tests` in the dispatcher");
    for body in &tests {
        let start = body.as_ptr() as usize - all.as_ptr() as usize;
        let before: String = all[..start].chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            before.ends_with("#[cfg(test)]modtests"),
            "the dispatcher's `mod tests` is not a test module, so it is read as product code"
        );
    }
    let code = without(&all, &tests);
    let squeezed: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        squeezed.contains("pubfndispatch("),
        "`fn dispatch` was not found: the scan reads nothing"
    );

    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let words: Vec<&str> = code.split(|c: char| !is_ident(c)).filter(|w| !w.is_empty()).collect();
    let constants: BTreeSet<&str> = words.iter().copied().filter(|w| w.ends_with("_LUA")).collect();
    for forbidden in [
        "INSTALL_LUA",
        "TEARDOWN_LUA",
        "PAGE_TEARDOWN_LUA",
        "SLOT_CALL_LUA",
        "SCRATCH_CALL_LUA",
        "REVIEW_LUA",
        "TAKE_EVENTS_LUA",
        "NAV_FALLBACK_LUA",
        "SHOW_LUA",
        "OPEN_AND_SHOW_LUA",
        "CLEAR_LUA",
        "CLEAR_ALL_LUA",
        "LIVE_SHOW_LUA",
        "LIVE_CLEAR_LUA",
        "PLUGIN_OPEN_LUA",
        "PLUGIN_ASSIST_LUA",
        "AGENT_EXEC_LUA",
    ] {
        assert!(!constants.contains(forbidden), "the dispatcher names {forbidden}");
    }
    assert_eq!(
        constants,
        BTreeSet::from(["BUFFER_STATE_LUA", "LIVE_SHOWN_LUA"]),
        "the dispatcher names a Lua constant outside its table"
    );

    // The review ops, the live ones too, become calls on window 1's own driver, of these kinds only: never a
    // clear of every window's overlays.
    let overlay_calls: BTreeSet<String> = squeezed
        .match_indices("OverlayCall::")
        .map(|(at, _)| {
            squeezed[at + "OverlayCall::".len()..]
                .chars()
                .take_while(|c| is_ident(*c))
                .collect()
        })
        .collect();
    assert_eq!(
        overlay_calls,
        ["OpenAndShow", "Show", "Clear", "LiveShow", "LiveClear"]
            .iter()
            .map(|name| name.to_string())
            .collect(),
        "the dispatcher makes an overlay call outside its table"
    );
    let review_ops: BTreeSet<String> = squeezed
        .match_indices("EditorOp::Review")
        .map(|(at, _)| {
            squeezed[at + "EditorOp::".len()..]
                .chars()
                .take_while(|c| is_ident(*c))
                .collect()
        })
        .collect();
    assert_eq!(
        review_ops,
        [
            "ReviewShow",
            "ReviewOpenAndShow",
            "ReviewClear",
            "ReviewClearMine",
            "ReviewLiveNote",
            "ReviewLiveShow",
            "ReviewLiveClear"
        ]
        .iter()
        .map(|name| name.to_string())
        .collect(),
        "the dispatcher's review ops are not the table's"
    );

    // The plugin host's functions are named by their full path at each call, so an alias cannot hide another.
    let mut plugin_calls = BTreeSet::new();
    for (at, _) in squeezed.match_indices("editor::") {
        if squeezed[..at].chars().next_back().is_some_and(is_ident) {
            continue; // `review_editor::` and the like
        }
        let rest = &squeezed[at + "editor::".len()..];
        let name: String = rest.chars().take_while(|c| is_ident(*c)).collect();
        if name == "FollowInstall" {
            assert!(
                rest[name.len()..].starts_with('{'),
                "the follow installation is a typed record"
            );
            continue;
        }
        assert!(
            rest[name.len()..].starts_with('('),
            "`editor::{name}` is not a call: the plugin functions are named only where they are called"
        );
        plugin_calls.insert(name);
    }
    assert_eq!(
        plugin_calls,
        BTreeSet::from([
            "assist_apply".to_string(),
            "assist_clear".to_string(),
            "open".to_string(),
            "follow_text".to_string(),
            "follow_reveal".to_string(),
            "follow_jump".to_string(),
            "follow_install".to_string(),
            "follow_on".to_string()
        ]),
        "the dispatcher calls a plugin editor function outside its table"
    );
    assert_eq!(
        tool_calls(&squeezed),
        Ok(TOOL_CALLS
            .iter()
            .map(|(tool, function)| format!("{tool}::{function}"))
            .collect()),
        "the dispatcher calls a tool function outside its table"
    );
    for (at, _) in squeezed.match_indices("plugin::") {
        let rest = &squeezed[at + "plugin::".len()..];
        // The follow text's two bounds are the ones window 1's own pages are held to: constants, not functions.
        let bound = ["follow::MAX_TEXT_BYTES", "follow::MAX_TEXT_LINES"].iter().any(|name| {
            rest.strip_prefix(name)
                .is_some_and(|after| !after.chars().next().is_some_and(is_ident))
        });
        assert!(
            rest.starts_with("editor::") || rest.starts_with("roots::") || bound,
            "the dispatcher reaches into the plugin host beyond its editor functions, path checks and bounds: {}",
            &rest[..rest.len().min(40)]
        );
    }
    assert!(
        !squeezed.contains("kill::") && !words.contains(&"kill"),
        "the dispatcher names the pane kill"
    );
    assert!(
        !words.contains(&"ClearAll"),
        "the dispatcher can clear window 1's overlays"
    );

    assert_eq!(
        squeezed.matches("exec_lua(").count(),
        2,
        "the dispatcher sends Lua other than through its buffer-state call and its shown question"
    );
    assert!(
        squeezed.contains("exec_lua(BUFFER_STATE_LUA,buffer_state_args("),
        "the dispatcher's buffer-state call does not send the buffer-state chunk with its own arguments"
    );
    assert!(
        squeezed.contains("exec_lua(LIVE_SHOWN_LUA,live_shown_args("),
        "the dispatcher's shown question does not send the shown chunk with arguments it builds itself"
    );

    // Neither the op's type nor its dispatcher has a way to carry code to run.
    let ops = read_required("core/src/window_link/editor_op.rs");
    assert!(
        product_code(&ops.text).contains("enum EditorOp"),
        "`enum EditorOp` was not found: the scan reads nothing"
    );
    for text in [&ops.text, &source.text] {
        assert_eq!(exec_op_wrong(text), Vec::<String>::new());
    }
}

// ---- the exec chunk ------------------------------------------------------------------------------------------------

const EXEC_MODULE: &str = "core/src/editor_tools/tools/nvim_exec.rs";
const EXEC_CHUNK: &str = "core/src/editor_tools/tools/nvim_exec.lua";
/// The panel's one function that calls the broker's `answer_card`.
const CARD_GLUE: &str = "panel/src/editor_glue.rs";
/// The file of the one arm that calls [`CARD_GLUE`]'s function, on an `editor_card_answer` from the page.
const CARD_ARM: &str = "panel/src/inbound.rs";
/// The function of [`CARD_ARM`] holding the one call of `TabSet::answer_card_async`, on a provider's cards.
const PROVIDER_ANSWER: &str = "answer_permission_response";
/// A test module of `tab_set.rs` kept in a file of its own, whose calls are `TabSet::answer_card` on a local `set`
/// (as are the files under `core/src/tab_set/tests/` and the `*_tests.rs` beside it).
const TAB_SET_TESTS: &str = "core/src/tab_set/move_tests.rs";

/// Every file that may evaluate text in the editor, and how. Reviewed: `nvim_exec.lua` takes only the text of an
/// approval, and `install.lua` compiles its parts, the compile-time constants `install_args` names. A new pair is red
/// until someone has looked at it, and a pair that is gone is red too.
const EVALUATORS: &[(&str, &str)] = &[
    (EXEC_CHUNK, "nvim_exec2"),
    (EXEC_CHUNK, "loadstring("),
    ("core/src/companion/install.lua", "loadstring("),
    ("core/src/takeover/waiting_room.lua", "nvim_exec("),
];

/// What `review_lua_builds_no_source` refuses in `review.lua`, but for the word `load(`, read by [`tool_chunk_words`],
/// and without the parens of `execute` and `vim.fn.bufnr`: either runs just the same when it is handed on as a value.
const SOURCE_WORDS: [&str; 8] = [
    "vim.cmd",
    "nvim_command",
    "nvim_exec",
    "loadstring",
    "dofile",
    "fnameescape",
    "execute",
    "vim.fn.bufnr",
];

/// What else runs text in the editor, which [`SOURCE_WORDS`] does not name. Each takes a string or a function's name,
/// and typed keys reach the command line.
const RUNNERS: [&str; 8] = [
    "nvim_cmd",
    "luaeval",
    "nvim_eval",
    ".eval",
    "nvim_call_function",
    "nvim_call_dict_function",
    "feedkeys",
    "nvim_input",
];

/// Words too common to refuse bare (`args.load`), so they count called or handed on: `pcall(load, x)`, `vim.call(x)`,
/// `fn.call(x, {})`. `nvim_buf_call`, which takes a function, is another word.
const CALLED: [(&str, &str); 2] = [("load", "load("), ("call", "call(")];

const LOAD_FILE_CHUNK: &str = "core/src/editor_tools/tools/load_file.lua";

/// The calls of a [`RUNNERS`] word a tool chunk may hold, as [`tight`] reads them. Reviewed: every part is constant
/// text, the `:lua` line calls a function held on `__state`, and `:set` takes one flag of 'shortmess'. Each stands in
/// its chunk once, so a further call there is red, and a call that is gone is red too.
const LISTED_COMMANDS: [(&str, &str); 3] = [
    (
        LOAD_FILE_CHUNK,
        "nvim_cmd({cmd='lua',args={'_G.__eitri_agent.silently()'},mods={silent=true}},{})",
    ),
    (
        LOAD_FILE_CHUNK,
        "nvim_cmd({cmd='set',args={'shortmess+=A'},mods={noautocmd=true}},{})",
    ),
    (
        LOAD_FILE_CHUNK,
        "nvim_cmd,{cmd='set',args={'shortmess-=A'},mods={noautocmd=true}},{})",
    ),
];

fn squeeze(code: &str) -> String {
    code.chars().filter(|c| !c.is_whitespace()).collect()
}

/// `text` with its comments, its literals and its `#[cfg(test)]` modules blanked. Only a module with the attribute is
/// cut: one without it ships, so it is read.
fn product_code(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut code = code_only(&chars, &kinds(&chars));
    for (from, to) in test_modules(&code) {
        for c in code[from..=to].iter_mut().filter(|c| **c != '\n') {
            *c = ' ';
        }
    }
    code.into_iter().collect()
}

/// The literals of `text`, quotes included, and with `tests` those of its `#[cfg(test)]` modules too.
fn literals(text: &str, tests: bool) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let kinds = kinds(&chars);
    let cut = if tests {
        Vec::new()
    } else {
        test_modules(&code_only(&chars, &kinds))
    };
    let mut found = Vec::new();
    let mut current = String::new();
    for (at, (c, kind)) in chars.iter().zip(&kinds).enumerate() {
        if *kind == Kind::Literal && !cut.iter().any(|&(from, to)| from < at && at < to) {
            current.push(*c);
        } else if !current.is_empty() {
            found.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        found.push(current);
    }
    found
}

/// The text of `body`, a slice of `whole`, as a range of `whole`.
fn span(whole: &str, body: &str) -> Range<usize> {
    let start = body.as_ptr() as usize - whole.as_ptr() as usize;
    start..start + body.len()
}

/// The braces following the first `head` in `text`, as a range.
fn block_after(text: &str, head: &str) -> Option<Range<usize>> {
    let at = text.find(head)?;
    let open = at + text[at..].find('{')?;
    Some(span(text, source_scan::braced(text, open)))
}

/// `AGENT_EXEC_LUA` and its file are named in the exec module and nowhere else. Comments may say the name.
fn exec_chunk_naming(sources: &[Source]) -> Vec<String> {
    let mut wrong = Vec::new();
    let mut named = 0;
    for source in sources {
        let words = word_ends(&source_scan::code_only(&source.text), "AGENT_EXEC_LUA").len();
        let file = literals(&source.text, true).iter().any(|l| l.contains("nvim_exec.lua"));
        if source.path == EXEC_MODULE {
            named = words;
            if !file {
                wrong.push(format!("{EXEC_MODULE} does not load `nvim_exec.lua`"));
            }
        } else if words > 0 || file {
            wrong.push(format!("{} names the exec chunk", source.path));
        }
    }
    if named < 2 {
        wrong.push(format!(
            "{EXEC_MODULE} names `AGENT_EXEC_LUA` {named} times: no `const` and call to check"
        ));
    }
    wrong
}

/// Whether the call's argument, at the start of `rest`, is `exec_args(..)` and the call ends with it.
fn only_exec_args(rest: &str) -> bool {
    let rest: Vec<char> = rest.chars().collect();
    let open = "exec_args".len();
    let named = rest.iter().take(open + 1).collect::<String>() == "exec_args(";
    named
        && closing(&rest, open).is_some_and(|close| {
            let after: String = rest[close + 1..].iter().take(2).collect();
            after.starts_with(')') || after == ",)"
        })
}

/// What in the exec module's text lets code reach the chunk from anywhere but a person's answer on a card. The text is
/// whitespace-squeezed where a shape is matched, so formatting changes nothing.
fn exec_record_wrong(text: &str) -> Vec<String> {
    const CALL: &str = "exec_lua(AGENT_EXEC_LUA,";
    const CONSTRUCTOR: &str = "fnfrom_card(pending:PendingExec,answer:CardAnswer,";
    const ARGS: &str = "fnexec_args(stamp:&Stamp,record:ApprovalRecord)";
    const RECORD: &str = "ApprovalRecord";
    const CLASSIFIER: &str = "pubfnis_exec_chunk(code:&str)->bool{code==AGENT_EXEC_LUA}";

    let code = product_code(text);
    let squeezed = squeeze(&code);
    let mut wrong = Vec::new();
    let mut need = |holds: bool, what: &str| {
        if !holds {
            wrong.push(what.to_owned());
        }
    };

    let calls: Vec<usize> = squeezed.match_indices(CALL).map(|(at, _)| at).collect();
    need(calls.len() == 1, "the chunk has exactly one call");
    need(
        word_ends(&code, "AGENT_EXEC_LUA").len() == 3,
        "`AGENT_EXEC_LUA` stands in its `const`, its call and `is_exec_chunk`, and nowhere else in product code",
    );
    need(
        squeezed.matches(CLASSIFIER).count() == 1,
        "`is_exec_chunk` only compares a string with the chunk, once",
    );
    let after_call = |at: &usize| &squeezed[at + CALL.len()..];
    need(
        calls.iter().all(|at| only_exec_args(after_call(at))),
        "its arguments are exactly `exec_args(..)`",
    );
    let send = block_after(&squeezed, "pubfnsend(");
    need(
        squeezed.matches("pubfnsend(").count() == 1
            && send.is_some_and(|body| calls.iter().all(|at| body.contains(at))),
        "the chunk's call stands in `pub fn send`",
    );

    need(
        squeezed.matches(ARGS).count() == 1,
        "`fn exec_args` takes a stamp and the record, once",
    );
    need(squeezed.matches("exec_args(").count() == 2, "`exec_args` has one call");

    let struct_literals: Vec<usize> = squeezed
        .match_indices("ApprovalRecord{")
        .map(|(at, _)| at)
        .filter(|&at| !squeezed[..at].ends_with("struct") && !squeezed[..at].ends_with("impl"))
        .collect();
    let constructor = block_after(&squeezed, "fnfrom_card(");
    need(
        struct_literals.len() == 1 && constructor.is_some_and(|body| body.contains(&struct_literals[0])),
        "the record has one struct literal, in `from_card`",
    );
    let impls = source_scan::impl_blocks(&code, RECORD);
    need(
        !impls.iter().any(|body| {
            let body = squeeze(body);
            body.contains("Self{") || body.contains("Self(")
        }),
        "the record is not built through `Self`",
    );

    let heads: Vec<usize> = squeezed.match_indices(CONSTRUCTOR).map(|(at, _)| at).collect();
    need(
        heads.len() == 1,
        "`fn from_card` takes a pending call and a card's answer, once",
    );
    need(
        heads.iter().all(|&at| squeezed[..at].ends_with(['{', '}', ';', ']'])),
        "`from_card` is not private",
    );
    // By word as well: `let f = ApprovalRecord::from_card;` is a call the `(` cannot see.
    need(
        squeezed.matches("from_card(").count() == 2 && word_ends(&code, "from_card").len() == 2,
        "`from_card` has one call",
    );

    let name = word_ends(&code, RECORD)
        .into_iter()
        .find(|end| code[..end - RECORD.len()].trim_end().ends_with("struct"));
    let fields = name.and_then(|end| {
        let after = &code[end..];
        let rest = after.trim_start();
        rest.starts_with('{')
            .then(|| source_scan::braced(&code, end + (after.len() - rest.len())))
    });
    need(fields.is_some(), "the record is a struct with named fields");
    need(
        fields.is_none_or(|body| word_ends(body, "pub").is_empty()),
        "the record has a public field",
    );

    // The attributes on the struct: what stands between the last item's end and its name.
    let before = squeezed.find("structApprovalRecord").map_or("", |at| &squeezed[..at]);
    let above = &before[before.rfind([';', '}']).map_or(0, |at| at + 1)..];
    for forged in ["Clone", "Default", "Serialize", "Deserialize"] {
        need(!above.contains(forged), &format!("the record derives {forged}"));
    }

    // Only the inherent block names it: a trait is one more way in, and red until someone has looked at it.
    let trait_impl = word_ends(&code, "impl").into_iter().any(|end| {
        let rest = &code[end..];
        let header = &rest[..rest.find('{').unwrap_or(rest.len())];
        !word_ends(header, RECORD).is_empty() && squeeze(header) != RECORD
    });
    need(!trait_impl, "the record has an `impl` other than the inherent one");

    // A second name is a literal, or a call of `from_card`, that the counts above cannot see.
    let renamed = word_ends(&code, RECORD).into_iter().any(|end| {
        let before = code[..end - RECORD.len()].trim_end();
        let after = code[end..].trim_start();
        let as_before = word_ends(before, "as").last() == Some(&before.len());
        as_before || after.starts_with(';') || after.split_whitespace().next() == Some("as")
    });
    need(!renamed, "the record has another name");

    // What runs is what the card showed: nothing writes to the record or its text after the card made it.
    let written = [".code=", ".kind="].into_iter().any(|field| {
        squeezed
            .match_indices(field)
            .any(|(at, _)| !squeezed[at + field.len()..].starts_with('='))
    });
    need(
        !written && !squeezed.contains("mutrecord") && !impls.iter().any(|body| squeeze(body).contains("mutself")),
        "the record is changed after the card",
    );

    wrong
}

/// `text` with its Lua comments dropped, each for one blank. Strings stay: a word in one counts, and `--` in one opens
/// no comment, so a string cannot hide the code after it.
fn lua_code(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    // The end of what a long bracket at `at` closes, if one opens there: `[[`, `[=[`, ...
    let long_end = |at: usize| {
        if chars.get(at) != Some(&'[') {
            return None;
        }
        let level = chars[at + 1..].iter().take_while(|c| **c == '=').count();
        if chars.get(at + 1 + level) != Some(&'[') {
            return None;
        }
        let close: Vec<char> = std::iter::once(']')
            .chain(std::iter::repeat_n('=', level))
            .chain(std::iter::once(']'))
            .collect();
        let found = (at..chars.len()).find(|&p| chars[p..].starts_with(&close));
        Some(found.map_or(chars.len(), |p| p + close.len()))
    };
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '-' && chars.get(i + 1) == Some(&'-') {
            i = long_end(i + 2).unwrap_or_else(|| (i..chars.len()).find(|&p| chars[p] == '\n').unwrap_or(chars.len()));
            out.push(' ');
        } else if let Some(end) = long_end(i) {
            out.extend(&chars[i..end]);
            i = end;
        } else if c == '"' || c == '\'' {
            out.push(c);
            i += 1;
            while i < chars.len() && chars[i] != c && chars[i] != '\n' {
                let escaped = usize::from(chars[i] == '\\' && i + 1 < chars.len());
                out.extend(&chars[i..=i + escaped]);
                i += 1 + escaped;
            }
            if let Some(quote) = chars.get(i) {
                out.push(*quote);
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// `code` with its blanks dropped, but for one between two words: `vim . cmd (x)` and `vim.cmd\n(x)` read as
/// `vim.cmd(x)`, and `return load` stays two words. Strings are squeezed too: this text is only matched.
fn tight(code: &str) -> String {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::with_capacity(code.len());
    let mut blank = false;
    for c in code.chars() {
        if c.is_whitespace() {
            blank = true;
            continue;
        }
        if blank && word(c) && out.chars().next_back().is_some_and(word) {
            out.push(' ');
        }
        out.push(c);
        blank = false;
    }
    out
}

/// Whether the word `name` stands in `text` with one of `next` after it, past blanks.
fn word_before(text: &str, name: &str, next: &[char]) -> bool {
    word_ends(text, name)
        .into_iter()
        .any(|end| text[end..].trim_start().starts_with(next))
}

/// The word `name` called, or in Lua handed on as a value: `pcall(load, x)` evaluates `x` too. Not `bufload(`, and not
/// an English sentence in a Rust string.
fn names_call(text: &str, name: &str, lua: bool) -> bool {
    let next: &[char] = if lua { &['(', ',', ')'] } else { &['('] };
    word_before(text, name, next)
}

/// What can evaluate text in a file: in a chunk with its comments dropped, in Rust in its string literals outside its
/// test modules, since a chunk can stand in one.
fn evaluators(path: &str, text: &str) -> BTreeSet<&'static str> {
    let lua = path.ends_with(".lua");
    let texts = if lua {
        vec![lua_code(text)]
    } else {
        literals(text, false)
    };
    // These names are no English word: in a chunk the bare name counts, as `local run = loadstring` evaluates too. A
    // Rust literal may name the tool or say the word, so there only a call counts.
    let names = |text: &str, name: &str| {
        if lua {
            !word_ends(text, name).is_empty()
        } else {
            word_before(text, name, &['('])
        }
    };
    let mut found = BTreeSet::new();
    for text in &texts {
        if text.contains("nvim_exec2") {
            found.insert("nvim_exec2");
        }
        if names(text, "loadstring") {
            found.insert("loadstring(");
        }
        if names(text, "nvim_exec") || names(text, "nvim_exec_lua") {
            found.insert("nvim_exec(");
        }
        if names_call(text, "load", lua) {
            found.insert("load(");
        }
    }
    found
}

/// Every evaluator in `sources` that [`EVALUATORS`] does not list.
fn unlisted(sources: &[Source]) -> Vec<String> {
    let mut wrong = Vec::new();
    for source in sources {
        for word in evaluators(&source.path, &source.text) {
            if !EVALUATORS.contains(&(source.path.as_str(), word)) {
                wrong.push(format!("{} holds `{word}`", source.path));
            }
        }
    }
    wrong
}

/// The words of [`SOURCE_WORDS`], [`RUNNERS`] and [`CALLED`] in `code`, a chunk as [`tight`] reads it.
fn words_in(code: &str) -> Vec<&'static str> {
    let mut found: Vec<&'static str> = SOURCE_WORDS
        .into_iter()
        .chain(RUNNERS)
        .filter(|word| code.contains(word))
        .collect();
    found.extend(
        CALLED
            .into_iter()
            .filter(|(name, _)| names_call(code, name, true))
            .map(|(_, word)| word),
    );
    found
}

/// The words of [`words_in`] in a chunk, comments dropped.
fn tool_chunk_words(text: &str) -> Vec<&'static str> {
    words_in(&tight(&lua_code(text)))
}

/// What `chunks` hold beyond [`LISTED_COMMANDS`], and every listed call that is no longer there, once.
fn tool_chunks_wrong(chunks: &[&Source]) -> Vec<String> {
    let mut wrong = Vec::new();
    for (path, _) in LISTED_COMMANDS
        .iter()
        .filter(|(path, _)| !chunks.iter().any(|chunk| chunk.path == *path))
    {
        wrong.push(format!("{path} is gone: the list is out of date"));
    }
    for chunk in chunks {
        let mut code = tight(&lua_code(&chunk.text));
        for (_, call) in LISTED_COMMANDS.iter().filter(|(path, _)| *path == chunk.path) {
            if code.matches(call).count() != 1 {
                wrong.push(format!(
                    "{} does not hold `{call}` once: the list is out of date",
                    chunk.path
                ));
            }
            code = code.replace(call, " ");
        }
        wrong.extend(
            words_in(&code)
                .into_iter()
                .map(|word| format!("{} holds `{word}`", chunk.path)),
        );
    }
    wrong
}

/// The bodies of the `pattern { .. } => { .. }` match arms of `code`, as ranges.
fn arm_bodies(code: &str, pattern: &str) -> Vec<Range<usize>> {
    code.match_indices(pattern)
        .filter_map(|(at, _)| {
            let after = at + pattern.len();
            let open = after + code[after..].find('{')?;
            let end = open + source_scan::braced(code, open).len();
            let body = code[end..].trim_start().strip_prefix("=>")?.trim_start();
            body.starts_with('{')
                .then(|| span(code, source_scan::braced(code, code.len() - body.len())))
        })
        .collect()
}

/// Card answers have fixed callers: the broker's glue and message arm, and the provider's async entry,
/// shared start and completion. Text cannot infer receiver types, so each call is pinned to its receiver,
/// file and enclosing function (or message arm). Passing a method as a value is refused too.
fn card_path_wrong(sources: &[Source]) -> Vec<String> {
    const ARM: &str = "InboundMessage::EditorCardAnswer";
    const TAB_SET: &str = "core/src/tab_set.rs";
    // Each route must occur exactly once. The synchronous wrapper remains available to tests.
    let routes = [
        ("answer_card", CARD_GLUE, "answer_card", "editor_tools."),
        ("answer_card", CARD_ARM, ARM, "editor_glue::"),
        ("answer_card_async", CARD_ARM, PROVIDER_ANSWER, "tabs."),
        ("start_card_answer", TAB_SET, "answer_card", "self."),
        ("start_card_answer", TAB_SET, "answer_card_async", "self."),
        ("finish_card_answer", TAB_SET, "answer_card", "self."),
        ("finish_card_answer", CARD_ARM, "poll_permission_sends", "state.tabs."),
    ];
    let mut counts = vec![0; routes.len()];
    let mut wrong = Vec::new();
    for source in sources {
        let code = product_code(&source.text);
        let bodies: Vec<Vec<Range<usize>>> = routes
            .iter()
            .map(|(_, _, enclosing, _)| {
                if *enclosing == ARM {
                    arm_bodies(&code, ARM)
                } else {
                    source_scan::functions(&code, enclosing)
                        .into_iter()
                        .map(|body| span(&code, body))
                        .collect()
                }
            })
            .collect();
        for name in [
            "answer_card",
            "answer_card_async",
            "start_card_answer",
            "finish_card_answer",
        ] {
            for end in word_ends(&code, name) {
                let at = end - name.len();
                let reversed: String = code[..at]
                    .chars()
                    .rev()
                    .filter(|c| !c.is_whitespace())
                    .take(32)
                    .collect();
                let before: String = reversed.chars().rev().collect();
                let called = code[end..].trim_start().starts_with('(');
                if called && before.ends_with("fn") {
                    continue;
                }
                let route = routes.iter().enumerate().position(|(i, (method, path, _, receiver))| {
                    let receiver_matches = before.strip_suffix(receiver).is_some_and(|rest| {
                        // The broker may be reached through a state field; `tabs` and `self` must be exact.
                        *receiver == "editor_tools."
                            || *receiver == "editor_glue::"
                            || !rest.ends_with(|c: char| c.is_alphanumeric() || c == '_' || c == '.')
                    });
                    called
                        && name == *method
                        && source.path == *path
                        && receiver_matches
                        && bodies[i].iter().any(|body| body.contains(&at))
                });
                if let Some(i) = route {
                    counts[i] += 1;
                } else {
                    let line = code[..at].matches('\n').count() + 1;
                    wrong.push(format!("{}:{line}: `{name}` outside the card path", source.path));
                }
            }
        }
        // Neither the page's handler nor its completion poll may bypass the card entry to answer a provider.
        for handler in [PROVIDER_ANSWER, "poll_permission_sends"] {
            for body in source_scan::functions(&code, handler) {
                for name in [
                    "respond_permission",
                    "respond_permission_async",
                    "resolve_permission",
                    "resolve_permission_async",
                    "approve_pending",
                    "approve_each",
                    "answer_what_needs_no_human",
                    "confirm_bypass",
                ] {
                    if !word_ends(body, name).is_empty() {
                        wrong.push(format!("{}: `{handler}` directly names `{name}`", source.path));
                    }
                }
            }
        }
    }
    for ((name, path, enclosing, _), count) in routes.iter().zip(counts) {
        if count != 1 {
            wrong.push(format!("`{enclosing}` of {path} calls `{name}` {count} times"));
        }
    }
    wrong
}

/// What in a file of the op's type or dispatcher could carry code to run: nothing here may name it, in code or in a
/// wire name.
fn exec_op_wrong(text: &str) -> Vec<String> {
    let said = format!("{}\n{}", product_code(text), literals(text, false).join("\n"));
    ["Exec", "nvim_exec"]
        .into_iter()
        .filter(|word| said.contains(word))
        .map(|word| format!("the op names `{word}`"))
        .collect()
}

#[test]
fn exec_chunk_is_named_only_in_its_module() {
    let read = source_scan::rust_sources(&["core/src", "shell/src", "panel/src", "mac/src"]);
    assert!(
        read.iter().any(|source| source.path == EXEC_MODULE),
        "the walk missed the exec module"
    );
    assert_eq!(exec_chunk_naming(&read), Vec::<String>::new());
}

#[test]
fn exec_code_comes_only_from_the_record() {
    let module = source_scan::read_required(EXEC_MODULE);
    assert!(
        squeeze(&product_code(&module.text)).contains("pubfnsend("),
        "`fn send` was not found: the scan reads nothing"
    );
    assert_eq!(exec_record_wrong(&module.text), Vec::<String>::new());
}

#[test]
fn only_the_exec_chunk_evaluates_an_argument() {
    // Rust as well: only `core/src` holds Lua today, and a directory with no file of the kind asked for is refused.
    let read = source_scan::sources(&["core/src", "shell/src", "panel/src", "mac/src"], &["lua", "rs"]);
    assert_eq!(unlisted(&read), Vec::<String>::new());
    for (path, word) in EVALUATORS {
        let source = read.iter().find(|source| source.path == *path);
        let found = source
            .map(|source| evaluators(&source.path, &source.text))
            .unwrap_or_default();
        assert!(
            found.contains(word),
            "{path} no longer holds `{word}`: the list is out of date"
        );
    }
}

#[test]
fn no_other_tool_chunk_builds_source() {
    let read = source_scan::sources(&["core/src/editor_tools"], &["lua"]);
    let others: Vec<&Source> = read.iter().filter(|source| source.path != EXEC_CHUNK).collect();
    assert_eq!(others.len() + 1, read.len(), "the walk missed the exec chunk");
    assert!(others.len() >= 6, "the walk found only {} chunks", others.len());
    assert_eq!(tool_chunks_wrong(&others), Vec::<String>::new());
}

#[test]
fn only_the_card_path_answers_a_broker_card() {
    let all = source_scan::rust_sources(&["core/src", "panel/src", "shell/src"]);
    let test_only = source_scan::test_only_files(&all);
    for file in [TAB_SET_TESTS, "core/src/tab_set/tests/bypass.rs"] {
        assert!(
            test_only.contains(file),
            "{file} is no longer a test module, so it is read as product code"
        );
    }
    let read: Vec<Source> = all
        .into_iter()
        .filter(|source| !test_only.contains(&source.path))
        .collect();
    assert_eq!(card_path_wrong(&read), Vec::<String>::new());
}

/// The shape of the exec module, with everything the checks read and nothing else.
const EXEC_SHAPE: &str = r#"
const AGENT_EXEC_LUA: &str = chunk!("nvim_exec.lua");

pub fn is_exec_chunk(code: &str) -> bool {
    code == AGENT_EXEC_LUA
}

pub struct ApprovalRecord {
    tab: u64,
    code: String,
}

impl ApprovalRecord {
    fn from_card(pending: PendingExec, answer: CardAnswer, tab: u64) -> Result<ApprovalRecord, ExecRefused> {
        Ok(ApprovalRecord {
            tab,
            code: pending.code,
        })
    }
}

pub fn send(rpc: &dyn EditorRpc, stamp: &Stamp, slot: &ExecSlot) -> Pending {
    let record = slot.take();
    rpc.exec_lua(AGENT_EXEC_LUA, exec_args(stamp, record))
}

fn exec_args(stamp: &Stamp, record: ApprovalRecord) -> Vec<Value> {
    vec![stamp.to_value(), Value::from(record.code)]
}

fn run_approved(pending: PendingExec, answer: CardAnswer) {
    let record = ApprovalRecord::from_card(pending, answer, 1);
}
"#;

/// [`EXEC_SHAPE`] with one piece replaced, which has to be there.
fn shape_with(from: &str, to: &str) -> String {
    assert!(EXEC_SHAPE.contains(from), "the shape has no `{from}`");
    EXEC_SHAPE.replacen(from, to, 1)
}

fn refused(text: &str, why: &str) {
    let wrong = exec_record_wrong(text);
    assert!(wrong.iter().any(|line| line.contains(why)), "`{why}` not in {wrong:?}");
}

fn accepted(text: &str) {
    assert_eq!(exec_record_wrong(text), Vec::<String>::new());
}

fn file(path: &str, text: &str) -> Source {
    Source {
        path: path.to_owned(),
        text: text.to_owned(),
    }
}

#[test]
fn a_file_with_nothing_to_count_fails() {
    assert!(!exec_record_wrong("fn unrelated() {}").is_empty());
    assert!(!exec_chunk_naming(&[file("x.rs", "fn unrelated() {}")]).is_empty());
    assert!(!card_path_wrong(&[file("x.rs", "fn unrelated() {}")]).is_empty());
}

#[test]
fn a_second_caller_of_the_chunk_fails() {
    let second = format!("{EXEC_SHAPE}\nfn other(rpc: &dyn EditorRpc) {{ rpc.exec_lua(AGENT_EXEC_LUA, vec![]); }}");
    refused(&second, "exactly one call");
    let alias = format!("{EXEC_SHAPE}\nconst SAME: &str = AGENT_EXEC_LUA;");
    refused(&alias, "its `const`, its call and `is_exec_chunk`");

    let module = file(EXEC_MODULE, EXEC_SHAPE);
    assert_eq!(exec_chunk_naming(std::slice::from_ref(&module)), Vec::<String>::new());
    for text in [
        "fn other() { use exec::AGENT_EXEC_LUA; }",
        "const X: &str = chunk!(\"nvim_exec.lua\");",
    ] {
        let wrong = exec_chunk_naming(&[module.clone(), file("core/src/other.rs", text)]);
        assert_eq!(wrong.len(), 1, "{text}: {wrong:?}");
        assert!(wrong[0].contains("core/src/other.rs"), "{wrong:?}");
    }
    // A comment may name it.
    let said = file("core/src/other.rs", "// AGENT_EXEC_LUA in nvim_exec.lua\nfn other() {}");
    assert_eq!(exec_chunk_naming(&[module, said]), Vec::<String>::new());
}

#[test]
fn a_built_code_argument_fails() {
    const CALL: &str = "exec_lua(AGENT_EXEC_LUA, exec_args(stamp, record))";
    for built in [
        "exec_lua(AGENT_EXEC_LUA, vec![Value::from(code)])",
        "exec_lua(AGENT_EXEC_LUA, built(exec_args(stamp, record)))",
        "exec_lua(AGENT_EXEC_LUA, exec_args(stamp, record).into_iter().chain(more).collect())",
    ] {
        refused(&shape_with(CALL, built), "its arguments are exactly");
    }
    accepted(&shape_with(
        CALL,
        "exec_lua(\n        AGENT_EXEC_LUA,\n        exec_args(stamp, record),\n    )",
    ));
    refused(
        &shape_with(
            "pub fn send(rpc: &dyn EditorRpc, stamp: &Stamp, slot: &ExecSlot) -> Pending {",
            "pub fn run(rpc: &dyn EditorRpc, stamp: &Stamp, slot: &ExecSlot) -> Pending {",
        ),
        "`pub fn send`",
    );
    refused(
        &shape_with(
            "fn exec_args(stamp: &Stamp, record: ApprovalRecord) -> Vec<Value> {",
            "fn exec_args(stamp: &Stamp, record: String) -> Vec<Value> {",
        ),
        "`fn exec_args",
    );
    let again = format!("{EXEC_SHAPE}\nfn more(stamp: &Stamp, record: ApprovalRecord) {{ exec_args(stamp, record); }}");
    refused(&again, "`exec_args` has one call");
}

#[test]
fn a_public_constructor_fails() {
    for public in [
        "pub fn from_card(",
        "pub(crate) fn from_card(",
        "pub(super) fn from_card(",
    ] {
        refused(&shape_with("fn from_card(", public), "`from_card` is not private");
    }
    accepted(&shape_with("fn from_card(", "#[must_use]\n    fn from_card("));
    refused(
        &shape_with("answer: CardAnswer, tab", "answer: &CardAnswer, tab"),
        "`fn from_card",
    );
    let second = format!("{EXEC_SHAPE}\nfn again() {{ ApprovalRecord::from_card(p, a, 2); }}");
    refused(&second, "`from_card` has one call");
    refused(
        &shape_with("ApprovalRecord::from_card(pending, answer, 1)", "todo!()"),
        "`from_card` has one call",
    );
    // Taken as a value it is called through a name the count cannot see.
    let handed = format!("{EXEC_SHAPE}\nfn again() {{ let f = ApprovalRecord::from_card; f(p, a, 2); }}");
    refused(&handed, "`from_card` has one call");
}

#[test]
fn a_public_field_fails() {
    refused(
        &shape_with("    tab: u64,\n    code", "    pub tab: u64,\n    code"),
        "a public field",
    );
    refused(
        &shape_with("    code: String,\n}", "    pub(crate) code: String,\n}"),
        "a public field",
    );
    refused(
        &shape_with(
            "pub struct ApprovalRecord {\n    tab: u64,\n    code: String,\n}",
            "pub struct ApprovalRecord(String);",
        ),
        "named fields",
    );
}

#[test]
fn a_derived_clone_fails() {
    for derive in ["Clone", "Default", "Serialize", "Deserialize"] {
        let derived = format!("#[derive(Debug, {derive})]\npub struct ApprovalRecord");
        refused(&shape_with("pub struct ApprovalRecord", &derived), derive);
    }
    accepted(&shape_with(
        "pub struct ApprovalRecord",
        "#[derive(Debug)]\npub struct ApprovalRecord",
    ));
    // A derive on the item above says nothing of the record.
    accepted(&shape_with(
        "pub struct ApprovalRecord",
        "#[derive(Clone)]\npub struct Other {\n    a: u8,\n}\n\npub struct ApprovalRecord",
    ));
}

#[test]
fn an_impl_of_a_trait_for_the_record_fails() {
    for header in [
        "impl Clone for ApprovalRecord {\n    fn clone(&self) -> Self { todo!() }\n}",
        "impl<T> From<T> for ApprovalRecord {\n    fn from(_: T) -> Self { todo!() }\n}",
        "impl From<String> for ApprovalRecord {\n    fn from(_: String) -> Self { todo!() }\n}",
        "impl From<ApprovalRecord> for String {\n    fn from(_: ApprovalRecord) -> Self { todo!() }\n}",
    ] {
        refused(
            &format!("{EXEC_SHAPE}\n{header}"),
            "an `impl` other than the inherent one",
        );
    }
}

#[test]
fn a_second_struct_literal_fails() {
    let forged = "fn forged() -> Option<ApprovalRecord> { Some(ApprovalRecord { tab: 1, code: String::new() }) }";
    refused(&format!("{EXEC_SHAPE}\n{forged}"), "one struct literal");
    let elsewhere = shape_with(
        "Ok(ApprovalRecord {\n            tab,\n            code: pending.code,\n        })",
        "todo!()",
    );
    refused(&format!("{elsewhere}\n{forged}"), "one struct literal");
    let by_self =
        "impl ApprovalRecord {\n    fn forged() -> Option<Self> { Some(Self { tab: 1, code: String::new() }) }\n}";
    refused(&format!("{EXEC_SHAPE}\n{by_self}"), "`Self`");
}

#[test]
fn a_second_name_for_the_record_fails() {
    for named in [
        "type R = ApprovalRecord;\nfn forged() -> R { R { tab: 1, code: String::new() } }",
        "type R = self::ApprovalRecord;",
        "use self::ApprovalRecord as R;",
        "use other::Forged as ApprovalRecord;",
    ] {
        refused(&format!("{EXEC_SHAPE}\n{named}"), "another name");
    }
    // The call of its constructor and a type in a signature are the record under its own name.
    accepted(&format!(
        "{EXEC_SHAPE}\nfn read(record: &ApprovalRecord) -> Option<ApprovalRecord> {{ None }}"
    ));
}

#[test]
fn a_record_changed_after_the_card_fails() {
    for changed in [
        shape_with(
            "let record = slot.take();",
            "let mut record = slot.take();\n    record.code = other();",
        ),
        shape_with(
            "let record = slot.take();",
            "let record = slot.take();\n    forged(&mut record);",
        ),
        format!("{EXEC_SHAPE}\nfn f(slot: &S) {{ slot.get().code = other(); }}"),
        format!("{EXEC_SHAPE}\nfn f(slot: &S) {{ slot.get().kind = other(); }}"),
        format!(
            "{EXEC_SHAPE}\nimpl ApprovalRecord {{\n    pub fn set_code(&mut self, c: String) {{ self.code = c; }}\n}}"
        ),
        format!("{EXEC_SHAPE}\nimpl ApprovalRecord {{\n    pub fn with(mut self, c: String) -> Self {{ self }}\n}}"),
    ] {
        refused(&changed, "changed after the card");
    }
    accepted(&format!(
        "{EXEC_SHAPE}\nfn same(a: &A, b: &B) -> bool {{ a.code == b.code }}"
    ));
    accepted(&format!("{EXEC_SHAPE}\nfn other(mut slot: u8) {{ slot = 1; }}"));
}

#[test]
fn the_impl_block_is_no_literal() {
    accepted(EXEC_SHAPE);
    accepted(&format!(
        "{EXEC_SHAPE}\nimpl ApprovalRecord {{\n    pub fn tab(&self) -> u64 {{ self.tab }}\n}}"
    ));
}

#[test]
fn a_test_module_and_a_comment_are_not_product_code() {
    let tests = "#[cfg(test)]\nmod tests {\n    fn t() { rpc.exec_lua(AGENT_EXEC_LUA, vec![]); }\n}";
    accepted(&format!("{EXEC_SHAPE}\n{tests}"));
    accepted(&format!("{EXEC_SHAPE}\n// rpc.exec_lua(AGENT_EXEC_LUA, vec![])"));
    // Without the attribute it ships, so it counts.
    refused(
        &format!("{EXEC_SHAPE}\n{}", tests.replace("#[cfg(test)]\n", "")),
        "exactly one call",
    );
}

fn flagged(path: &str, text: &str) -> Vec<String> {
    unlisted(&[file(path, text)])
}

#[test]
fn a_new_chunk_with_loadstring_fails() {
    assert_eq!(flagged("core/src/x.lua", "return loadstring(args.code)()").len(), 1);
    assert_eq!(flagged("core/src/x.lua", "vim.api.nvim_exec2(args.code, {})").len(), 1);
    // The Lua of a Rust literal is a chunk too, and a test's is not.
    assert_eq!(
        flagged("core/src/x.rs", "const C: &str = \"return loadstring(c)()\";").len(),
        1
    );
    assert_eq!(
        flagged("core/src/x.rs", "const C: &str = r#\"nvim_exec2(c, {})\"#;").len(),
        1
    );
    assert_eq!(
        flagged(
            "core/src/x.rs",
            "#[cfg(test)]\nmod t { const C: &str = \"loadstring(c)\"; }"
        ),
        Vec::<String>::new()
    );
    // A listed file holds what is listed for it and nothing more.
    assert_eq!(
        flagged("core/src/companion/install.lua", "local f = loadstring(part.src)"),
        Vec::<String>::new()
    );
    assert_eq!(
        flagged("core/src/companion/install.lua", "vim.api.nvim_exec2(x, {})").len(),
        1
    );
    assert_eq!(flagged(EXEC_CHUNK, "return load(args.code)").len(), 1);
    assert_eq!(
        flagged(EXEC_CHUNK, "loadstring(args.code)\nvim.api.nvim_exec2(args.code, {})"),
        Vec::<String>::new()
    );
}

#[test]
fn an_evaluator_handed_on_fails() {
    for bad in [
        "pcall(loadstring, x)",
        "local run = loadstring",
        "vim.api.nvim_exec(x, true)",
        "pcall(vim.api.nvim_exec, x)",
        "vim.api.nvim_exec_lua(x, {})",
        "local exec = vim.api.nvim_exec_lua",
    ] {
        assert_eq!(flagged("core/src/x.lua", bad).len(), 1, "not caught: {bad}");
    }
    assert_eq!(
        flagged("core/src/x.rs", "const C: &str = \"vim.api.nvim_exec(c, true)\";").len(),
        1
    );
    // A Rust literal may name the tool, or say the word in a sentence.
    assert_eq!(
        flagged("core/src/x.rs", "const N: &str = \"nvim_exec\";"),
        Vec::<String>::new()
    );
    assert_eq!(
        flagged("core/src/x.rs", "const N: &str = \"the loadstring of a chunk\";"),
        Vec::<String>::new()
    );
}

#[test]
fn a_string_opens_no_comment() {
    for hidden in [
        "local s = '--' return loadstring(x)",
        "local s = \"--\" return loadstring(x)",
        "local s = [[--]] return loadstring(x)",
        "local s = 'it\\'s --' return loadstring(x)",
        "--[[ long ]] return loadstring(x)",
        "--[==[ long ]] ]==] return loadstring(x)",
    ] {
        assert_eq!(flagged("core/src/x.lua", hidden).len(), 1, "not caught: {hidden}");
    }
}

#[test]
fn load_of_a_function_name_fails() {
    for bad in [
        "return load(name)",
        "return load (name)",
        "local ok = pcall(load, name)",
        "vim.load(name)",
    ] {
        assert_eq!(flagged("core/src/x.lua", bad).len(), 1, "not caught: {bad}");
    }
    assert_eq!(
        flagged("core/src/x.rs", "const C: &str = \"return load(name)\";").len(),
        1
    );
    assert_eq!(tool_chunk_words("return load(name)"), ["load("]);
    assert_eq!(tool_chunk_words("pcall(load, name)"), ["load("]);
}

#[test]
fn bufload_passes() {
    for good in [
        "vim.fn.bufload(buf)",
        "reload(buf)",
        "local function load_file(path) end",
        "-- load(x) and loadstring(y) in a comment",
        "--[[ nvim_exec2(x)\nloadstring(y) ]]",
        "--[=[ load(x) ]] ]=]",
    ] {
        assert_eq!(flagged("core/src/x.lua", good), Vec::<String>::new(), "{good}");
        assert_eq!(tool_chunk_words(good), Vec::<&str>::new(), "{good}");
    }
    // An English sentence in a Rust string is no call.
    assert_eq!(
        flagged("core/src/x.rs", "const C: &str = \"could not load, or load the file\";"),
        Vec::<String>::new()
    );
}

#[test]
fn a_tool_chunk_with_vim_cmd_fails() {
    assert_eq!(tool_chunk_words("vim.cmd(args.x)"), ["vim.cmd"]);
    for (text, word) in [
        ("vim.cmd.edit(args.x)", "vim.cmd"),
        ("vim.api.nvim_command(args.x)", "nvim_command"),
        ("vim.api.nvim_exec2(args.x, {})", "nvim_exec"),
        ("vim.api.nvim_exec_lua(args.x, {})", "nvim_exec"),
        ("dofile(args.x)", "dofile"),
        ("vim.fn.fnameescape(args.x)", "fnameescape"),
        ("vim.fn.execute(args.x)", "execute"),
        ("vim.fn.bufnr(args.x)", "vim.fn.bufnr"),
    ] {
        assert_eq!(tool_chunk_words(text), [word], "{text}");
    }
    assert_eq!(
        tool_chunk_words("vim.api.nvim_buf_get_lines(0, 0, -1, false)"),
        Vec::<&str>::new()
    );
}

#[test]
fn pcall_of_loadstring_fails() {
    assert_eq!(tool_chunk_words("pcall(loadstring, x)"), ["loadstring"]);
}

#[test]
fn a_tool_chunk_that_runs_text_by_another_name_fails() {
    for (text, word) in [
        ("vim.api.nvim_cmd({ cmd = 'lua', args = { args.x } }, {})", "nvim_cmd"),
        ("pcall(vim.api.nvim_cmd, args.x, {})", "nvim_cmd"),
        ("vim.fn.luaeval(args.x)", "luaeval"),
        ("vim.api.nvim_eval(args.x)", "nvim_eval"),
        ("vim.fn.eval(args.x)", ".eval"),
        ("local fn = vim.fn return fn.eval(args.x)", ".eval"),
        ("pcall(vim.fn.eval, args.x)", ".eval"),
        ("local run = vim.fn.eval", ".eval"),
        ("vim.api.nvim_call_function(args.f, {})", "nvim_call_function"),
        (
            "vim.api.nvim_call_dict_function(d, args.f, {})",
            "nvim_call_dict_function",
        ),
        ("vim.call(args.f)", "call("),
        ("vim.fn.call(args.f, {})", "call("),
        ("vim.api.nvim_feedkeys(args.x, 'n', false)", "feedkeys"),
        ("vim.fn.feedkeys(args.x)", "feedkeys"),
        ("vim.api.nvim_input(args.x)", "nvim_input"),
    ] {
        assert_eq!(tool_chunk_words(text), [word], "{text}");
    }
    for good in [
        "vim.api.nvim_buf_call(buf, function() end)",
        "vim.api.nvim_win_call(win, function() end)",
        "pcall(run, x)",
        "local retrieval = medieval(x)",
        "vim.api.nvim_create_autocmd('SwapExists', { callback = function() end })",
    ] {
        assert_eq!(tool_chunk_words(good), Vec::<&str>::new(), "{good}");
    }
}

/// `load_file.lua`'s listed calls, written as a chunk would.
const LISTED_CHUNK: &str = "
vim.api.nvim_cmd({ cmd = 'lua', args = { '_G.__eitri_agent.silently()' }, mods = { silent = true } }, {})
vim.api.nvim_cmd({ cmd = 'set', args = { 'shortmess+=A' }, mods = { noautocmd = true } }, {})
pcall(vim.api.nvim_cmd, { cmd = 'set', args = { 'shortmess-=A' }, mods = { noautocmd = true } }, {})
";

fn chunks_wrong(path: &str, text: &str) -> Vec<String> {
    let listed = file(LOAD_FILE_CHUNK, LISTED_CHUNK);
    let other = file(path, text);
    if path == LOAD_FILE_CHUNK {
        tool_chunks_wrong(&[&other])
    } else {
        tool_chunks_wrong(&[&listed, &other])
    }
}

#[test]
fn only_a_listed_constant_call_runs_text_in_a_tool_chunk() {
    assert_eq!(chunks_wrong(LOAD_FILE_CHUNK, LISTED_CHUNK), Vec::<String>::new());
    // The same calls laid out otherwise are the same calls.
    assert_eq!(
        chunks_wrong(LOAD_FILE_CHUNK, &LISTED_CHUNK.replace(", ", ",\n    ")),
        Vec::<String>::new()
    );

    let fourth = format!("{LISTED_CHUNK}\nvim.api.nvim_cmd({{ cmd = 'lua', args = {{ args.x }} }}, {{}})");
    assert_eq!(chunks_wrong(LOAD_FILE_CHUNK, &fourth).len(), 1);
    // A call with an argument in place of the constant is a call that is gone and a word that stays.
    let built = LISTED_CHUNK.replace("'shortmess+=A'", "args.x");
    assert_eq!(chunks_wrong(LOAD_FILE_CHUNK, &built).len(), 2);
    let twice = format!("{LISTED_CHUNK}{LISTED_CHUNK}");
    assert_eq!(chunks_wrong(LOAD_FILE_CHUNK, &twice).len(), 3);

    // Listed for one chunk, it is refused in another.
    assert_eq!(chunks_wrong("core/src/editor_tools/tools/x.lua", LISTED_CHUNK).len(), 1);
    assert_eq!(
        chunks_wrong("core/src/editor_tools/tools/x.lua", "local a = 1"),
        Vec::<String>::new()
    );

    // A list that has gone out of date is red.
    assert_eq!(chunks_wrong(LOAD_FILE_CHUNK, "local a = 1").len(), 3);
    assert_eq!(
        tool_chunks_wrong(&[&file("core/src/editor_tools/tools/x.lua", "local a = 1")]).len(),
        3
    );
}

#[test]
fn tight_keeps_words_apart() {
    assert_eq!(tight("return  load\n( x )"), "return load(x)");
    assert_eq!(tight("vim . fn\n.eval"), "vim.fn.eval");
    assert_eq!(tight("local a = b"), "local a=b");
}

#[test]
fn where_a_line_ends_changes_nothing() {
    for (text, word) in [
        ("return vim.fn.execute\n(args.code)", "execute"),
        ("return vim\n.fn\n.execute(args.code)", "execute"),
        ("pcall(vim.fn.execute, args.code)", "execute"),
        ("local run = vim.fn.execute", "execute"),
        ("vim.fn.win_execute(win, args.code)", "execute"),
        ("vim\n.cmd(args.x)", "vim.cmd"),
        ("vim . cmd (args.x)", "vim.cmd"),
        ("vim.fn\n.bufnr\n(args.x)", "vim.fn.bufnr"),
        ("vim.api\n.nvim_cmd\n(args.x, {})", "nvim_cmd"),
        ("return vim.fn\n.eval\n(args.x)", ".eval"),
        ("return load\n(args.x)", "load("),
    ] {
        assert_eq!(tool_chunk_words(text), [word], "{text}");
    }
}

/// The panel's card path: the glue's one call of the broker, and the arm's one call of the glue.
fn card_path() -> [Source; 3] {
    [
        file(
            CARD_GLUE,
            r#"
pub(super) fn answer_card(state: &mut State, tab: TabId) {
    let outs = state
        .editor_tools
        .answer_card(tab, card, decision, origin, now)?;
}
"#,
        ),
        file(
            CARD_ARM,
            r#"
fn handle(state: &mut State) {
    match message {
        InboundMessage::EditorCardAnswer { card_id, .. } => {
            match super::editor_glue::answer_card(state, tab) {}
        }
        InboundMessage::Other => {}
    }
}

fn answer_permission_response(tabs: &mut TabSet) {
    Ok(tabs.answer_card_async(tab, page, id, decision))
}

fn poll_permission_sends(state: &mut State) {
    state.tabs.finish_card_answer(tab, &send);
}
"#,
        ),
        file(
            "core/src/tab_set.rs",
            r#"
fn answer_card(&mut self) {
    let send = self.start_card_answer(id, page, permission_id, decision, false)?;
    self.finish_card_answer(id, &send)
}
fn answer_card_async(&mut self) {
    self.start_card_answer(id, page, permission_id, decision, true)
}
"#,
        ),
    ]
}

fn with_card_path(path: &str, text: &str) -> Vec<String> {
    let mut read = card_path().to_vec();
    match read.iter_mut().find(|source| source.path == path) {
        Some(source) => source.text.push_str(text),
        None => read.push(file(path, text)),
    }
    card_path_wrong(&read)
}

#[test]
fn a_second_answer_card_caller_fails() {
    assert_eq!(card_path_wrong(&card_path()), Vec::<String>::new());
    for (path, text) in [
        (
            "core/src/sweep.rs",
            "fn sweep(s: &mut S) { s.editor_tools.answer_card(t, c, Allow, Key, now); }",
        ),
        (
            CARD_GLUE,
            "fn sweep(s: &mut S) { s.editor_tools.answer_card(t, c, Allow, Key, now); }",
        ),
        (
            CARD_ARM,
            "fn sweep(s: &mut S) { super::editor_glue::answer_card(s, t); }",
        ),
        (
            "core/src/sweep.rs",
            "fn sweep(b: &mut Broker) { b.answer_card(t, c, Allow, Key, now); }",
        ),
        (
            "core/src/sweep.rs",
            "fn sweep(b: &mut Broker) { Broker::answer_card(b, t, c, Allow, Key, now); }",
        ),
        (
            "core/src/sweep.rs",
            "fn sweep() { cards.iter().map(Broker::answer_card); }",
        ),
        (CARD_ARM, "use super::editor_glue::answer_card;"),
        // The name `tabs` says nothing of the type behind it: only the one provider call is let through.
        (
            "core/src/sweep.rs",
            "fn sweep(s: &mut S) { let tabs = &mut s.editor_tools; tabs.answer_card(t, c, Allow, Key, now); }",
        ),
        (
            CARD_ARM,
            "fn sweep(s: &mut S) { let tabs = &mut s.editor_tools; tabs.answer_card(t, c, Allow, Key, now); }",
        ),
        (
            "core/src/sweep.rs",
            "fn sweep(my_tabs: &mut Broker) { my_tabs.answer_card(t, c, Allow, Key, now); }",
        ),
        (
            "core/src/sweep.rs",
            "fn sweep(s: &mut S) { s.tabs.answer_card(t, c, Allow, Key, now); }",
        ),
    ] {
        let wrong = with_card_path(path, text);
        assert_eq!(wrong.len(), 1, "{text}: {wrong:?}");
    }
    // A definition and a test are not callers.
    assert_eq!(
        with_card_path("core/src/t.rs", "pub fn answer_card(&self) {}"),
        Vec::<String>::new()
    );
    assert_eq!(
        with_card_path(
            "core/src/t.rs",
            "#[cfg(test)]\npub(super) mod tests { fn t() { b.answer_card(); } }"
        ),
        Vec::<String>::new()
    );

    // A card path that is gone is refused, not passed.
    let [glue, arm, tabs] = card_path();
    assert!(!card_path_wrong(std::slice::from_ref(&glue)).is_empty());
    assert!(!card_path_wrong(std::slice::from_ref(&arm)).is_empty());
    let outside = file(
        CARD_ARM,
        &arm.text
            .replace("InboundMessage::EditorCardAnswer", "InboundMessage::Sweep"),
    );
    assert!(!card_path_wrong(&[glue.clone(), outside, tabs.clone()]).is_empty());

    // The provider's call is pinned too: a second one beside it, or none, is red.
    const PROVIDER: &str = "Ok(tabs.answer_card_async(tab, page, id, decision))";
    assert!(arm.text.contains(PROVIDER));
    for changed in [
        format!("tabs.answer_card_async(t, c, Allow, Key, now);\n    {PROVIDER}"),
        "Ok(())".to_owned(),
    ] {
        let wrong = card_path_wrong(&[
            glue.clone(),
            file(CARD_ARM, &arm.text.replace(PROVIDER, &changed)),
            tabs.clone(),
        ]);
        assert_eq!(wrong.len(), 1, "{changed}: {wrong:?}");
    }
}

#[test]
fn async_card_entries_and_completions_are_pinned() {
    for (index, call) in [
        (1, "tabs.answer_card_async(tab, page, id, decision)"),
        (1, "state.tabs.finish_card_answer(tab, &send)"),
        (2, "self.start_card_answer(id, page, permission_id, decision, false)"),
        (2, "self.start_card_answer(id, page, permission_id, decision, true)"),
        (2, "self.finish_card_answer(id, &send)"),
    ] {
        for replacement in ["unrelated()".to_owned(), format!("{call}; {call}")] {
            let mut sources = card_path();
            assert!(sources[index].text.contains(call));
            sources[index].text = sources[index].text.replacen(call, &replacement, 1);
            assert!(
                !card_path_wrong(&sources).is_empty(),
                "accepted {call} -> {replacement}"
            );
        }
        let mut sources = card_path();
        sources[index].text = sources[index].text.replacen(call, "unrelated()", 1);
        sources[index].text.push_str(&format!("\nfn sweep() {{ {call}; }}"));
        assert!(!card_path_wrong(&sources).is_empty(), "accepted moved {call}");
    }
    for name in ["answer_card_async", "start_card_answer", "finish_card_answer"] {
        for text in [
            format!("fn sweep() {{ tabs.{name}(id); }}"),
            format!("fn sweep() {{ let answer = TabSet::{name}; }}"),
        ] {
            for path in [CARD_ARM, "panel/src/sweep.rs", "core/src/tab_set.rs"] {
                assert!(!with_card_path(path, &text).is_empty(), "accepted {path}: {text}");
            }
        }
    }
}

#[test]
fn permission_handlers_cannot_answer_around_the_card_path() {
    for handler in ["answer_permission_response", "poll_permission_sends"] {
        for name in [
            "answer_card",
            "answer_card_async",
            "start_card_answer",
            "finish_card_answer",
            "respond_permission",
            "respond_permission_async",
            "resolve_permission",
            "resolve_permission_async",
            "approve_pending",
            "approve_each",
            "answer_what_needs_no_human",
            "confirm_bypass",
        ] {
            for extra in [
                format!("backend.\n{name}\n(id);"),
                format!("let answer = Backend::{name};"),
            ] {
                let mut sources = card_path();
                let body = source_scan::functions(&sources[1].text, handler)[0];
                let at = span(&sources[1].text, body).start + 1;
                sources[1].text.insert_str(at, &extra);
                assert!(!card_path_wrong(&sources).is_empty(), "accepted {handler}: {extra}");
            }
        }
    }
}

#[test]
fn card_path_ignores_line_breaks_comments_literals_and_tests() {
    let mut sources = card_path();
    for source in &mut sources {
        source.text = source.text.replace('.', "\n.\n").replace('(', "\n(\n");
        source.text.push_str(
            r#"
// tabs.answer_card_async(); state.tabs.finish_card_answer();
const TEXT: &str = "tabs.answer_card_async(); self.start_card_answer();";
#[cfg(test)]
mod tests {
    fn test() { tabs.answer_card_async(); self.start_card_answer(); state.tabs.finish_card_answer(); }
}
"#,
        );
    }
    assert_eq!(card_path_wrong(&sources), Vec::<String>::new());
}

#[test]
fn an_exec_op_fails() {
    for bad in [
        "pub enum EditorOp { Exec { code: String } }",
        "pub enum EditorOp { NvimExec { code: String } }",
        "fn dispatch() { crate::editor_tools::tools::nvim_exec::send(rpc, &stamp, &slot); }",
        "#[serde(rename = \"nvim_exec\")]\npub struct Op;",
    ] {
        assert!(!exec_op_wrong(bad).is_empty(), "not caught: {bad}");
    }
    for good in [
        "pub enum EditorOp { Open { path: String } }",
        "// no Exec op, and no nvim_exec\npub enum EditorOp {}",
        "pub enum EditorOp {}\n#[cfg(test)]\nmod tests { const NAME: &str = \"nvim_exec\"; }",
        "fn f() { rpc.exec_lua(BUFFER_STATE_LUA, args); }",
    ] {
        assert_eq!(exec_op_wrong(good), Vec::<String>::new(), "flagged: {good}");
    }
}

#[test]
fn lua_comments_are_dropped_and_strings_kept() {
    assert_eq!(lua_code("a -- b\nc"), "a  \nc");
    assert_eq!(lua_code("a --[[ b\nc ]] d"), "a   d");
    assert_eq!(lua_code("'a -- b' c"), "'a -- b' c");
    assert_eq!(lua_code("[==[ -- ]] ]==] c"), "[==[ -- ]] ]==] c");
}

#[test]
fn literals_are_read_with_and_without_the_tests() {
    let text = "const A: &str = \"one\";\n#[cfg(test)]\nmod t { const B: &str = \"two\"; }\n";
    assert_eq!(literals(text, false), ["\"one\""]);
    assert_eq!(literals(text, true), ["\"one\"", "\"two\""]);
}

#[test]
fn every_exec_lua_code_argument_is_in_the_chunk_table() {
    let sources = source_scan::rust_sources(&["core/src", "shell/src"]);
    let mut seen = BTreeSet::new();
    let mut watched = 0;
    for source in &sources {
        let text = product_text(&source.text);
        let calls = scan_calls(
            &source.path,
            &text,
            &[
                "exec_lua",
                "exec_lua_answered",
                "exec_lua_for",
                "exec_lua_watched",
                "nvim_exec_lua",
            ],
        );
        for flagged in &calls.flagged {
            watched += 1;
            assert!(
                source.path == "shell/src/gtk_host/services.rs"
                    && flagged.contains("`exec_lua_watched` is passed `lua`"),
                "{flagged}"
            );
            // The owned-editor quit embeds its generation in the source. Its template is refused,
            // and every instantiated form is unknown, so the attached pane rejects both.
            let owner = source_scan::read_required("core/src/layout/kill.rs");
            let (_, body) = source_scan::function_in_raw(&[owner.clone()], "editor_quit_lua");
            let template = literals(&body, false);
            assert_eq!(template.len(), 1);
            let template = rust_string(&template[0]);
            assert_eq!(
                eitri_core::editor_rpc::check_chunk(&template),
                Ok(eitri_core::editor_rpc::ChunkSide::Refused)
            );
            for generation in [0, 1, u32::MAX] {
                assert!(
                    eitri_core::editor_rpc::check_chunk(&eitri_core::layout::kill::editor_quit_lua(generation))
                        .is_err()
                );
            }
        }
        for argument in calls.constants {
            let content = chunk_content(&sources, source, &argument);
            // The approved-code chunk is not a table row: its module classifies it without handing out the text.
            if eitri_core::editor_tools::tools::nvim_exec::is_exec_chunk(&content) {
                assert_eq!(
                    source.path, EXEC_MODULE,
                    "{}: {argument} is the approved-code chunk",
                    source.path
                );
                assert_eq!(
                    eitri_core::editor_rpc::check_chunk(&content),
                    Ok(eitri_core::editor_rpc::ChunkSide::Registered)
                );
                seen.insert(argument.split("::").last().unwrap().to_owned());
                continue;
            }
            assert!(
                eitri_core::editor_rpc::CHUNKS.iter().any(|(code, _)| *code == content),
                "{}: {argument} has no chunk classification",
                source.path
            );
            seen.insert(argument.split("::").last().unwrap().to_owned());
        }
    }
    for required in [
        "IDE_DIFF_CLOSE",
        "IDE_TAKE_EVENTS",
        "IDE_DIFF_OPEN",
        "SETUP_LUA",
        "SET_LINES_LUA",
        "TUTOR_SNIPPET",
        "eitri_core::editor_reopen::EDITOR_REOPEN_CAPTURE_LUA",
        "AGENT_EXEC_LUA",
        "REVIEW_LUA",
    ] {
        assert!(
            seen.contains(required.split("::").last().unwrap()),
            "missing call site: {required}"
        );
    }
    assert_eq!(watched, 1, "the watched quit must remain covered");
}

fn product_text(text: &str) -> String {
    let chars: Vec<_> = text.chars().collect();
    let code = code_only(&chars, &kinds(&chars));
    let mut out = chars;
    for (from, to) in test_modules(&code) {
        out[from..=to].fill(' ');
    }
    out.into_iter().collect()
}

fn rust_string(text: &str) -> String {
    let text = text.trim();
    if text.starts_with('r') {
        let start = text.find('"').unwrap();
        return text[start + 1..text.len() - start].to_owned();
    }
    let mut out = String::new();
    let mut chars = text[1..text.len() - 1].chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next().unwrap() {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            '0' => out.push('\0'),
            '\n' => {
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
            }
            other => panic!("unsupported string escape {other}"),
        }
    }
    out
}

fn chunk_content(sources: &[Source], source: &Source, expression: &str) -> String {
    let expression = expression.trim();
    if expression.starts_with('"') || expression.starts_with("r#") || expression.starts_with("r\"") {
        return rust_string(expression);
    }
    if expression.starts_with("include_str!") {
        let path = literals(expression, true)
            .pop()
            .unwrap_or_else(|| panic!("unresolved include in {}: {expression}", source.path));
        let path = std::path::Path::new(&source.path)
            .parent()
            .unwrap()
            .join(rust_string(&path));
        return source_scan::read_required(path.to_str().unwrap()).text;
    }
    if let Some(open) = expression.find('!').map(|at| at + 1) {
        let chars: Vec<_> = expression.chars().collect();
        let tags = kinds(&chars);
        let mut code = code_only(&chars, &tags);
        for i in 0..code.len() {
            if tags[i] == Kind::Literal && (i == 0 || tags[i - 1] != Kind::Literal) {
                code[i] = 'L';
            }
        }
        let close = closing(&code, open).unwrap();
        let args: Vec<String> = arguments(&code, open, close)
            .into_iter()
            .map(|(a, b)| chars[a..b].iter().collect())
            .collect();
        let name = expression[..open - 1].trim().split("::").last().unwrap();
        if name == "concat" {
            return args.iter().map(|arg| chunk_content(sources, source, arg)).collect();
        }
        let definition = sources
            .iter()
            .find_map(|s| {
                let head = format!("macro_rules! {name}");
                s.text.find(&head).map(|at| (s, at))
            })
            .unwrap_or_else(|| panic!("unknown macro {name}"));
        let (owner, at) = definition;
        let rest = &owner.text[at..];
        let start = rest.find("concat!(").unwrap();
        let tail = &rest[start..];
        let chars: Vec<_> = tail.chars().collect();
        let code = code_only(&chars, &kinds(&chars));
        let end = closing(&code, 7).unwrap();
        let mut expanded: String = chars[..=end].iter().collect();
        // Macro bodies here have literal or expression parameters, in declaration order.
        let parameters: Vec<_> = rest[..start]
            .split('$')
            .skip(1)
            .filter_map(|p| p.split_once(':').map(|(name, _)| name.trim()))
            .collect();
        for (parameter, value) in parameters.iter().zip(args) {
            expanded = expanded.replace(&format!("${parameter}"), &value);
        }
        // include_str! paths in chunk macros are relative to the invocation's module.
        return chunk_content(sources, source, &expanded);
    }
    let name = expression.split("::").last().unwrap().trim();
    let mut candidates = Vec::new();
    for candidate in sources {
        if !candidate.text.contains(name) {
            continue;
        }
        let chars: Vec<_> = candidate.text.chars().collect();
        let code = code_only(&chars, &kinds(&chars));
        for at in word_starts(&code, name) {
            let before: String = code[..at]
                .iter()
                .rev()
                .take(16)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            if !before.trim_end().ends_with("const") && !before.trim_end().ends_with("static") {
                continue;
            }
            let Some(eq) = (at..code.len()).find(|i| code[*i] == '=') else {
                continue;
            };
            let end = (eq + 1..code.len()).find(|i| code[*i] == ';').unwrap();
            candidates.push((candidate, chars[eq + 1..end].iter().collect::<String>()));
        }
    }
    let (owner, value) = candidates
        .iter()
        .find(|(owner, _)| owner.path == source.path)
        .or_else(|| (candidates.len() == 1).then(|| &candidates[0]))
        .unwrap_or_else(|| panic!("{}: cannot resolve {expression}", source.path));
    chunk_content(sources, owner, value)
}

#[test]
fn chunk_inventory_finds_bare_calls_and_calls_split_across_lines() {
    let names = &["exec_lua", "exec_lua_watched", "nvim_exec_lua"];
    for text in [
        "fn exec_lua(code: &str, args: A) {} fn f() { exec_lua(IDE_DIFF_CLOSE, args); rpc.nvim_exec_lua(IDE_TAKE_EVENTS, args); pane.exec_lua_watched(QUIT); }",
        "fn exec_lua (code: &str, args: A) {} fn f() { exec_lua\n(\nIDE_DIFF_CLOSE, args); rpc\n.\nnvim_exec_lua\n(IDE_TAKE_EVENTS, args); pane\n.exec_lua_watched\n(QUIT); }",
    ] {
        let report = scan_calls("core/src/example.rs", text, names);
        assert!(report.flagged.is_empty());
        assert_eq!(report.constants, ["IDE_DIFF_CLOSE", "QUIT", "IDE_TAKE_EVENTS"]);
    }
}
