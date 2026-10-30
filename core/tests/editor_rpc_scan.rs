//! Lua sent to the user's editor is always a compile-time constant; every value goes as an
//! argument. A path or a line of a file spliced into Lua source would run as code in the user's
//! nvim, so this reads `core/src` and `shell/src` as text and fails on:
//!
//! - a call of `exec_lua`, `exec_lua_answered` or `exec_lua_for` whose code argument is not a
//!   string literal or a path ending in an `UPPER_SNAKE` constant. A wrapper that passes its own
//!   caller's constant on carries `// editor-rpc-scan: forwards a caller's constant` on the call's
//!   line or the line above, and every such wrapper is listed in [`EXPECTED_FORWARDERS`], so a new
//!   one is red until someone has looked at it. `exec_lua_watched` (the editor's quit, which runs
//!   one fixed chunk) is a different name and out of scope;
//! - `leak` called as a method or a path (`.leak()`, `Box::leak(..)`, `.map(String::leak)`) outside
//!   a `#[cfg(test)]` module: leaking is the one way to make a built `String` a `&'static str`. A
//!   test module kept in a file of its own is read as product code, which can only turn this red;
//! - a `"nvim_exec_lua"` string literal outside `core/src/nvim_rpc.rs` and test modules: calling the
//!   method by name on a link would go around the typed calls (a test may name it to check what
//!   went over the wire).
//!
//! Comments and literals are blanked before anything is matched, and calls are found by their
//! tokens, never by where a line ends, so formatting can neither hide a call nor invent one.

const ANNOTATION: &str = "// editor-rpc-scan: forwards a caller's constant";

/// Every annotated forwarder, by file (relative to the workspace root) and count. Reviewed: each passes on a
/// `code` its own caller gave it, and that caller's call is checked here in turn.
const EXPECTED_FORWARDERS: &[(&str, usize)] = &[
    ("core/src/companion/driver.rs", 5),
    ("core/src/companion/link.rs", 2),
    ("core/src/editor_rpc.rs", 1),
    ("shell/src/editor_rpc.rs", 1),
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

    for name in ["exec_lua", "exec_lua_answered", "exec_lua_for"] {
        for at in word_starts(&code, name) {
            let path_form = match back_over_space(&code, at) {
                Some(p) if code[p] == '.' => false,
                Some(p) if code[p] == ':' && p > 0 && code[p - 1] == ':' => true,
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

/// Window 2 reaches the editor only through the editor-op dispatcher, so the dispatcher's product code may name
/// exactly one Lua constant (the buffer-state chunk, sent by its one `exec_lua` call) and the plugin host's editor
/// functions of its table, and nothing else that runs in the editor: no follow teardown (the snippet and its commands
/// are window 1's, whatever window 2's page does), no review chunk (the review ops become calls on window 1's own
/// review driver), no scratch call, no event drain, no navigation fallback, and no pane kill. Comments and literals
/// are blanked and whitespace is dropped, so formatting changes nothing.
#[test]
fn the_editor_op_dispatcher_names_only_its_table() {
    use eitri_core::source_scan::{code_only, modules, read_required, without};
    use std::collections::BTreeSet;

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
        "SCRATCH_CALL_LUA",
        "REVIEW_LUA",
        "TAKE_EVENTS_LUA",
        "NAV_FALLBACK_LUA",
        "SHOW_LUA",
        "OPEN_AND_SHOW_LUA",
        "CLEAR_LUA",
        "CLEAR_ALL_LUA",
        "PLUGIN_OPEN_LUA",
        "PLUGIN_ASSIST_LUA",
    ] {
        assert!(!constants.contains(forbidden), "the dispatcher names {forbidden}");
    }
    assert_eq!(
        constants,
        BTreeSet::from(["BUFFER_STATE_LUA"]),
        "the dispatcher names a Lua constant outside its table"
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
        1,
        "the dispatcher sends Lua other than through its one buffer-state call"
    );
    assert!(
        squeezed.contains("exec_lua(BUFFER_STATE_LUA,"),
        "the dispatcher's one `exec_lua` call does not send the buffer-state chunk"
    );
}
