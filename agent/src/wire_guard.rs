//! The wire guard (R07; spec §2.3, §6 and §7's first review focus; plan Task 1): a scan of this
//! crate's own non-test sources for every way back to a `claude` that runs without the gate.
//!
//! R07 made the CLI's permission mode a constant. Every session, on both backends, is gated --
//! `--permission-mode default` plus the `PreToolUse` hook on legacy, `INTERACTIVE` with
//! `permission_mode_switchable: false` on the sidecar -- and "bypass" is Eitri answering `allow`
//! itself. The types carry half of that: no session request has a mode any more. This scan carries
//! the half the compiler cannot see -- a string literal, a proto enum constructor, a setter, a
//! pre-approval list -- any of which would put the CLI back in `bypassPermissions` without a single
//! signature changing.
//!
//! Modelled on `socket_path.rs`'s scanner, with two differences that matter. Comments are removed by
//! a lexer that knows where string literals are (a `//` inside `"http://…"` is not a comment), and
//! every rule matches on the text with all whitespace collapsed to single spaces, so where rustfmt
//! breaks a line never decides the verdict (CLAUDE.md: "do not make it depend on where a line
//! ends").
//!
//! **Scope:** every `agent/src/**/*.rs`, `src/bin/` included. Test code is excluded: a
//! `#[cfg(test)] mod x { … }` block, and a file reached only through a `#[cfg(test)] mod x;`
//! declaration (which is how this file excludes itself, and `runtime_policy_verification.rs`).
//!
//! **Allowlist: exactly one entry**, matched by file and by the arm's own text, never by a line
//! number -- see [`ALLOWED_ARM`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The proto enum whose `Bypass` value is the sidecar's ungated policy.
const PROTO_PERMISSION_MODE: &str = "claude_runtime_protocol::v1::PermissionMode";

/// The one place `<P>::Bypass` may appear: the `ProtoEvent::PermissionModeChanged` arm of the
/// sidecar translator, as a match PATTERN (`<P>::Bypass =>`).
///
/// Why it is allowed: it decodes a report the sidecar sends; never constructs a request; it is the
/// tripwire (spec §6). A `PermissionModeChanged` naming BYPASS is exactly what must close the
/// session, and matching on it is how the translator notices.
const ALLOWED_ARM: (&str, &str) = (
    "providers/claude_sidecar/translate.rs",
    "ProtoEvent::PermissionModeChanged(",
);

/// Case-insensitive substrings no production source may contain (rule 1): the CLI's own name for
/// the ungated mode, and the two `--*dangerously*` flags and the SDK's
/// `allowDangerouslySkipPermissions` that enable it.
const FORBIDDEN_ANY_CASE: &[&str] = &["bypasspermissions", "dangerously", "allow_dangerously"];

/// Exact substrings no production source may contain (rules 3 and 4): the prost setters that would
/// build an ungated policy without a struct literal, and the wave-5 mid-session switch this task
/// deleted.
const FORBIDDEN_EXACT: &[&str] = &[
    "set_permissions(",
    "set_permission_mode_switchable(",
    "set_permission_mode(",
    "SetPermissionModeRequest",
];

/// Whole words no production source may contain (rule 5). Each is a PRE-APPROVAL list: a tool on it
/// runs when the hook gives no answer, which is the fall-through the gate exists to close (spec
/// §2.3). Whole words, because `disallowedTools`/`disallowed_tools` contain them and are the opposite.
const FORBIDDEN_WORDS: &[&str] = &["allowedTools", "allowed_tools"];

/// What one scan found.
#[derive(Debug, Default)]
struct Report {
    offenders: Vec<String>,
    scanned: BTreeSet<String>,
    skipped: BTreeSet<String>,
    allowlisted: usize,
}

fn is_ident(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// Where the string, raw string or char literal starting at `i` ends (one past it), if one starts
/// there. A lifetime (`'a`) is not a literal and returns `None`.
fn literal_end(c: &[char], i: usize) -> Option<usize> {
    let ch = c[i];
    if ch == 'r' {
        let prev_ok = match i.checked_sub(1).map(|p| c[p]) {
            None => true,
            Some('b') | Some('c') => i < 2 || !is_ident(c[i - 2]),
            Some(p) => !is_ident(p),
        };
        if prev_ok {
            let mut j = i + 1;
            let mut hashes = 0;
            while c.get(j) == Some(&'#') {
                hashes += 1;
                j += 1;
            }
            if c.get(j) == Some(&'"') {
                j += 1;
                while j < c.len() {
                    if c[j] == '"' && (1..=hashes).all(|k| c.get(j + k) == Some(&'#')) {
                        return Some(j + 1 + hashes);
                    }
                    j += 1;
                }
                return Some(c.len());
            }
        }
        return None;
    }
    if ch == '"' {
        let mut j = i + 1;
        while j < c.len() {
            match c[j] {
                '\\' => j += 2,
                '"' => return Some(j + 1),
                _ => j += 1,
            }
        }
        return Some(c.len());
    }
    if ch == '\'' {
        if c.get(i + 1) == Some(&'\\') {
            let mut j = i + 3;
            while j < c.len() && c[j] != '\'' {
                j += 1;
            }
            return Some((j + 1).min(c.len()));
        }
        if c.get(i + 2) == Some(&'\'') {
            return Some(i + 3);
        }
    }
    None
}

/// The source with every comment (`//`, `///`, `//!`, `/* */`, nested) replaced by a space, and
/// every literal left exactly as written.
fn strip_comments(src: &str) -> String {
    let c: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < c.len() {
        if c[i] == '/' && c.get(i + 1) == Some(&'/') {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
            out.push(' ');
            continue;
        }
        if c[i] == '/' && c.get(i + 1) == Some(&'*') {
            let mut depth = 1;
            i += 2;
            while i < c.len() && depth > 0 {
                if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if c[i] == '*' && c.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            out.push(' ');
            continue;
        }
        if let Some(end) = literal_end(&c, i) {
            out.extend(&c[i..end]);
            i = end;
            continue;
        }
        out.push(c[i]);
        i += 1;
    }
    out
}

/// Every run of whitespace, newlines included, as one space; and no space either side of `::`.
fn collapse(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut pending_space = false;
    for ch in src.chars() {
        if ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && !out.is_empty() {
            out.push(' ');
        }
        pending_space = false;
        out.push(ch);
    }
    out.replace(" ::", "::").replace(":: ", "::")
}

/// The index of the bracket closing the one at `open`, skipping literals.
fn matching_close(c: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < c.len() {
        if let Some(end) = literal_end(c, i) {
            i = end;
            continue;
        }
        match c[i] {
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn skip_spaces(c: &[char], mut i: usize) -> usize {
    while c.get(i) == Some(&' ') {
        i += 1;
    }
    i
}

fn read_ident(c: &[char], i: usize) -> (String, usize) {
    let mut j = i;
    while j < c.len() && is_ident(c[j]) {
        j += 1;
    }
    (c[i..j].iter().collect(), j)
}

/// Removes every `#[cfg(test)] mod x { … }` from `code` (comment-free, collapsed), and returns what
/// is left with the names of the `#[cfg(test)] mod x;` declarations it also removed.
fn strip_test_modules(code: &str) -> (String, Vec<String>) {
    const MARK: &str = "#[cfg(test)]";
    let mark: Vec<char> = MARK.chars().collect();
    let c: Vec<char> = code.chars().collect();
    let mut out = String::with_capacity(code.len());
    let mut test_decls = Vec::new();
    let mut i = 0;
    while i < c.len() {
        if !c[i..].starts_with(&mark) {
            if let Some(end) = literal_end(&c, i) {
                out.extend(&c[i..end]);
                i = end;
            } else {
                out.push(c[i]);
                i += 1;
            }
            continue;
        }
        let mut j = skip_spaces(&c, i + mark.len());
        // Further attributes on the same item.
        while c.get(j) == Some(&'#') && c.get(j + 1) == Some(&'[') {
            match matching_close(&c, j + 1) {
                Some(close) => j = skip_spaces(&c, close + 1),
                None => break,
            }
        }
        let (word, after) = read_ident(&c, j);
        if word == "pub" {
            j = skip_spaces(&c, after);
            if c.get(j) == Some(&'(') {
                if let Some(close) = matching_close(&c, j) {
                    j = skip_spaces(&c, close + 1);
                }
            }
        }
        let (word, after) = read_ident(&c, j);
        if word != "mod" {
            out.push(c[i]);
            i += 1;
            continue;
        }
        let (name, after) = read_ident(&c, skip_spaces(&c, after));
        let k = skip_spaces(&c, after);
        match c.get(k) {
            Some('{') => match matching_close(&c, k) {
                Some(close) => {
                    out.push(' ');
                    i = close + 1;
                }
                None => {
                    out.push(c[i]);
                    i += 1;
                }
            },
            Some(';') if !name.is_empty() => {
                test_decls.push(name);
                out.push(' ');
                i = k + 1;
            }
            _ => {
                out.push(c[i]);
                i += 1;
            }
        }
    }
    (out, test_decls)
}

/// The names of the `mod x;` declarations left in `code` (test modules already stripped).
fn module_declarations(code: &str) -> Vec<String> {
    let c: Vec<char> = code.chars().collect();
    let mut names = Vec::new();
    for i in 0..c.len() {
        if !c[i..].starts_with(&['m', 'o', 'd', ' ']) || (i > 0 && is_ident(c[i - 1])) {
            continue;
        }
        let (name, after) = read_ident(&c, i + 4);
        if !name.is_empty() && c.get(skip_spaces(&c, after)) == Some(&';') {
            names.push(name);
        }
    }
    names
}

/// Where Rust looks for `mod name;` declared in `declaring` (a path relative to `src/`, with `/`).
fn module_candidates(declaring: &str, name: &str) -> [String; 2] {
    let (dir, file) = declaring.rsplit_once('/').unwrap_or(("", declaring));
    let crate_root_like = matches!(file, "mod.rs" | "lib.rs" | "main.rs") || dir == "bin";
    let base = if crate_root_like {
        dir.to_string()
    } else {
        let stem = file.trim_end_matches(".rs");
        if dir.is_empty() {
            stem.to_string()
        } else {
            format!("{dir}/{stem}")
        }
    };
    let join = |tail: String| {
        if base.is_empty() {
            tail
        } else {
            format!("{base}/{tail}")
        }
    };
    [join(format!("{name}.rs")), join(format!("{name}/mod.rs"))]
}

/// Expands one `use` tree into (full path, name it binds) pairs. A glob binds `*`.
fn expand_use(tree: &str, prefix: &str, out: &mut Vec<(String, String)>) {
    let tree = tree.trim().trim_start_matches("::").trim();
    let join = |a: &str, b: &str| {
        if a.is_empty() {
            b.to_string()
        } else {
            format!("{a}::{b}")
        }
    };
    if let Some(inner) = tree.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
        for item in split_top_level(inner, ',') {
            expand_use(&item, prefix, out);
        }
        return;
    }
    if let Some(at) = tree.find("::{") {
        let path = &tree[..at];
        expand_use(&tree[at + 2..], &join(prefix, path), out);
        return;
    }
    let (path, alias) = match tree.split_once(" as ") {
        Some((path, alias)) => (path.trim(), Some(alias.trim())),
        None => (tree, None),
    };
    if path == "self" {
        let name = alias.unwrap_or_else(|| prefix.rsplit("::").next().unwrap_or(prefix));
        out.push((prefix.to_string(), name.to_string()));
        return;
    }
    let full = join(prefix, path);
    if let Some(module) = full.strip_suffix("::*") {
        out.push((module.to_string(), "*".to_string()));
        return;
    }
    let name = alias.unwrap_or_else(|| full.rsplit("::").next().unwrap_or(&full));
    out.push((full.clone(), name.to_string()));
}

fn split_top_level(text: &str, sep: char) -> Vec<String> {
    let c: Vec<char> = text.chars().collect();
    let mut items = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    let mut i = 0;
    while i < c.len() {
        if let Some(end) = literal_end(&c, i) {
            i = end;
            continue;
        }
        match c[i] {
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => depth -= 1,
            ch if ch == sep && depth == 0 => {
                items.push(c[start..i].iter().collect::<String>());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    items.push(c[start..].iter().collect());
    items.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

/// Every name `code` can write `claude_runtime_protocol::v1::PermissionMode` as: the path itself,
/// and whatever its `use` items bind to it or to a module above it.
fn proto_mode_aliases(code: &str) -> Vec<String> {
    let mut aliases = vec![PROTO_PERMISSION_MODE.to_string()];
    let c: Vec<char> = code.chars().collect();
    let mut i = 0;
    while i + 3 <= c.len() {
        let is_use = c[i..].starts_with(&['u', 's', 'e'])
            && (i == 0 || !is_ident(c[i - 1]))
            && matches!(c.get(i + 3), Some(' ') | Some(':') | Some('{'));
        if !is_use {
            i += 1;
            continue;
        }
        let rest: String = c[i + 3..].iter().collect();
        let Some(end) = rest.find(';') else { break };
        let mut bound = Vec::new();
        expand_use(&rest[..end], "", &mut bound);
        for (full, name) in bound {
            if name == "*" {
                if let Some(tail) = PROTO_PERMISSION_MODE.strip_prefix(&format!("{full}::")) {
                    aliases.push(tail.to_string());
                }
            } else if full == PROTO_PERMISSION_MODE {
                aliases.push(name);
            } else if let Some(tail) = PROTO_PERMISSION_MODE.strip_prefix(&format!("{full}::")) {
                aliases.push(format!("{name}::{tail}"));
            }
        }
        i += 3 + end;
    }
    aliases.sort();
    aliases.dedup();
    aliases
}

/// Every position in `code` where `word` occurs with no identifier character before or after it.
fn whole_word_positions(code: &str, word: &str) -> Vec<usize> {
    code.match_indices(word)
        .filter(|(at, _)| {
            let before = code[..*at].chars().next_back();
            let after = code[at + word.len()..].chars().next();
            !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
        })
        .map(|(at, _)| at)
        .collect()
}

fn snippet(code: &str, at: usize) -> String {
    let start = code[..at].char_indices().rev().nth(40).map_or(0, |(i, _)| i);
    let end = code[at..].char_indices().nth(60).map_or(code.len(), |(i, _)| at + i);
    format!("…{}…", &code[start..end])
}

/// Rule 3: every `ClaudeHostPolicy { … }` literal states `permissions: <P>::Interactive as i32` and
/// `permission_mode_switchable: false`, explicitly, and takes nothing from a `..` base -- which
/// could carry any value at all.
fn check_host_policy_literals(rel: &str, code: &str, aliases: &[String], offenders: &mut Vec<String>) {
    let c: Vec<char> = code.chars().collect();
    let interactive: Vec<String> = aliases.iter().map(|a| format!("{a}::Interactive as i32")).collect();
    for at in whole_word_positions(code, "ClaudeHostPolicy") {
        let i = code[..at].chars().count() + "ClaudeHostPolicy".len();
        let open = skip_spaces(&c, i);
        if c.get(open) != Some(&'{') {
            continue;
        }
        // A type position (`-> ClaudeHostPolicy {`, `impl … for ClaudeHostPolicy {`) is not a
        // literal.
        let before = code[..at].trim_end();
        if ["->", "impl", "for", "struct", "enum", "trait", "type"]
            .iter()
            .any(|kw| before.ends_with(kw))
        {
            continue;
        }
        let Some(close) = matching_close(&c, open) else {
            offenders.push(format!("{rel}: an unterminated ClaudeHostPolicy literal"));
            continue;
        };
        let body: String = c[open + 1..close].iter().collect();
        let mut permissions = None;
        let mut switchable = None;
        for entry in split_top_level(&body, ',') {
            let entry = entry.trim();
            if entry.starts_with("..") {
                offenders.push(format!(
                    "{rel}: a ClaudeHostPolicy literal takes fields from `{entry}`, which can carry any \
                     permission mode: {}",
                    snippet(code, at)
                ));
                continue;
            }
            let (name, value) = match field_split(entry) {
                Some((name, value)) => (name, value),
                None => (entry.to_string(), format!("<shorthand {entry}>")),
            };
            match name.as_str() {
                "permissions" => permissions = Some(value),
                "permission_mode_switchable" => switchable = Some(value),
                _ => {}
            }
        }
        match permissions {
            Some(value) if interactive.contains(&value) => {}
            other => offenders.push(format!(
                "{rel}: a ClaudeHostPolicy literal's `permissions` is {other:?}, not \
                 `<PermissionMode>::Interactive as i32` (R07: every session is gated): {}",
                snippet(code, at)
            )),
        }
        match switchable.as_deref() {
            Some("false") => {}
            other => offenders.push(format!(
                "{rel}: a ClaudeHostPolicy literal's `permission_mode_switchable` is {other:?}, not `false` \
                 (R07; `true` sets --allow-dangerously-skip-permissions on the CLI): {}",
                snippet(code, at)
            )),
        }
    }
}

/// `name: value` for a struct-literal field, splitting at the first `:` that is not half of `::`.
fn field_split(entry: &str) -> Option<(String, String)> {
    let c: Vec<char> = entry.chars().collect();
    for i in 0..c.len() {
        if c[i] == ':' && c.get(i + 1) != Some(&':') && (i == 0 || c[i - 1] != ':') {
            let name: String = c[..i].iter().collect();
            let value: String = c[i + 1..].iter().collect();
            return Some((name.trim().to_string(), value.trim().to_string()));
        }
    }
    None
}

/// Every rule, over one production file's comment-free, test-free, collapsed text.
fn check_file(rel: &str, code: &str, allowlisted: &mut usize, offenders: &mut Vec<String>) {
    let lower = code.to_lowercase();
    for word in FORBIDDEN_ANY_CASE {
        if let Some(at) = lower.find(word) {
            offenders.push(format!("{rel}: `{word}` (any case): {}", snippet(code, at)));
        }
    }
    for text in FORBIDDEN_EXACT {
        if let Some(at) = code.find(text) {
            offenders.push(format!("{rel}: `{text}`: {}", snippet(code, at)));
        }
    }
    for word in FORBIDDEN_WORDS {
        if let Some(&at) = whole_word_positions(code, word).first() {
            offenders.push(format!(
                "{rel}: `{word}`, a pre-approval list (it grants when the hook gives no answer): {}",
                snippet(code, at)
            ));
        }
    }
    // Assigned rather than stated in a literal: rule 3's "anywhere" half.
    for field in ["permission_mode_switchable", ".permissions"] {
        for (at, _) in code.match_indices(field) {
            let after = &code[at + field.len()..];
            let before = code[..at].chars().next_back();
            let whole = !after.chars().next().is_some_and(is_ident)
                && (field.starts_with('.') || !before.is_some_and(is_ident));
            let rest = after.trim_start();
            if whole && rest.starts_with('=') && !rest.starts_with("==") {
                offenders.push(format!("{rel}: `{field}` assigned: {}", snippet(code, at)));
            }
        }
    }

    let aliases = proto_mode_aliases(code);
    for alias in &aliases {
        let needle = format!("{alias}::Bypass");
        for (at, _) in code.match_indices(&needle) {
            let before = code[..at].chars().next_back();
            let after = code[at + needle.len()..].chars().next();
            if before.is_some_and(is_ident) || after.is_some_and(is_ident) {
                continue;
            }
            let pattern_not_constructor = code[at + needle.len()..].trim_start().starts_with("=>");
            let in_allowed_arm = rel == ALLOWED_ARM.0
                && code[..at]
                    .rfind("ProtoEvent::")
                    .is_some_and(|arm| code[arm..].starts_with(ALLOWED_ARM.1));
            if pattern_not_constructor && in_allowed_arm && *allowlisted == 0 {
                *allowlisted += 1;
                continue;
            }
            offenders.push(format!(
                "{rel}: `{needle}` -- the sidecar's ungated policy (R07); the only allowed use is the \
                 `PermissionModeChanged` decode arm in {}: {}",
                ALLOWED_ARM.0,
                snippet(code, at)
            ));
        }
    }
    check_host_policy_literals(rel, code, &aliases, offenders);
}

/// Scans a tree of sources keyed by their path relative to `src/` (`/`-separated).
fn scan_tree(files: &BTreeMap<String, String>) -> Report {
    let mut report = Report::default();
    let mut cleaned = BTreeMap::new();
    let mut test_only = BTreeSet::new();
    let mut reached = BTreeSet::new();
    for (rel, src) in files {
        let (code, test_decls) = strip_test_modules(&collapse(&strip_comments(src)));
        for name in test_decls {
            test_only.extend(module_candidates(rel, &name));
        }
        for name in module_declarations(&code) {
            reached.extend(module_candidates(rel, &name));
        }
        cleaned.insert(rel.clone(), code);
    }
    for (rel, code) in &cleaned {
        if test_only.contains(rel) && !reached.contains(rel) {
            report.skipped.insert(rel.clone());
            continue;
        }
        report.scanned.insert(rel.clone());
        check_file(rel, code, &mut report.allowlisted, &mut report.offenders);
    }
    report
}

fn read_tree(root: &Path) -> BTreeMap<String, String> {
    fn visit(root: &Path, dir: &Path, files: &mut BTreeMap<String, String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, files);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                files.insert(rel, std::fs::read_to_string(&path).unwrap());
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

/// The assertion over this crate. Every offender is listed, with the file and the text around it.
#[test]
fn no_production_source_can_reach_an_ungated_cli() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let report = scan_tree(&read_tree(&root));

    // Not vacuous: the files that build what goes on the wire were read, and the ones that are
    // test-only were recognised as such.
    for must in [
        "lib.rs",
        "process.rs",
        "providers/claude_sidecar/mod.rs",
        "providers/claude_sidecar/translate.rs",
    ] {
        assert!(report.scanned.contains(must), "{must} was not scanned: {report:?}");
    }
    for must in [
        "wire_guard.rs",
        "providers/claude_sidecar/runtime_policy_verification.rs",
    ] {
        assert!(
            report.skipped.contains(must),
            "{must} should be skipped as test-only: {report:?}"
        );
    }
    assert!(
        report.offenders.is_empty(),
        "production code that could reach an ungated CLI (R07; see agent/src/wire_guard.rs):\n{}",
        report.offenders.join("\n")
    );
    // A stale allowlist entry is a rotting one: the arm it names must still be there, once.
    assert_eq!(
        report.allowlisted, 1,
        "the allowlisted `PermissionModeChanged` decode arm in {} was not found",
        ALLOWED_ARM.0
    );
}

fn one(rel: &str, src: &str) -> Report {
    scan_tree(&BTreeMap::from([(rel.to_string(), src.to_string())]))
}

fn policy(permissions: &str, switchable: &str) -> String {
    format!(
        "use claude_runtime_protocol::v1::{{ClaudeHostPolicy, PermissionMode as ProtoPermissionMode}};\n\
         fn p() -> Option<ClaudeHostPolicy> {{\n    Some(ClaudeHostPolicy {{\n        configuration: 1,\n        \
         permissions: {permissions},\n        permission_mode_switchable: {switchable},\n    }})\n}}\n"
    )
}

#[test]
fn a_switchable_value_other_than_false_fails() {
    let report = one("x.rs", &policy("ProtoPermissionMode::Interactive as i32", "switchable"));
    assert_eq!(report.offenders.len(), 1, "{:?}", report.offenders);
    assert!(report.offenders[0].contains("permission_mode_switchable"));
}

#[test]
fn false_wrapped_onto_its_own_line_passes() {
    let report = one(
        "x.rs",
        &policy("ProtoPermissionMode::Interactive\n            as i32", "\n    false"),
    );
    assert!(report.offenders.is_empty(), "{:?}", report.offenders);
}

#[test]
fn an_aliased_bypass_constructor_fails() {
    let src = "use claude_runtime_protocol::v1::PermissionMode as PM;\nfn f() -> i32 { PM::Bypass as i32 }\n";
    let report = one("x.rs", src);
    assert_eq!(report.offenders.len(), 1, "{:?}", report.offenders);
    assert!(report.offenders[0].contains("PM::Bypass"));
    // And through a grouped, nested `use`, and a module alias.
    let nested = "use claude_runtime_protocol::{v1::{PermissionMode as Q}};\nfn f() -> i32 { Q::Bypass as i32 }\n";
    assert_eq!(one("x.rs", nested).offenders.len(), 1);
    let module = "use claude_runtime_protocol::v1 as pv;\nfn f() -> i32 { pv::PermissionMode::Bypass as i32 }\n";
    assert_eq!(one("x.rs", module).offenders.len(), 1);
    // Not the agent's own `PermissionMode`, which is the host's answer mode.
    let own = "use crate::PermissionMode;\nfn f() -> PermissionMode { PermissionMode::Bypass }\n";
    assert!(one("x.rs", own).offenders.is_empty());
}

#[test]
fn a_numeric_permissions_value_fails() {
    let report = one("x.rs", &policy("3", "false"));
    assert_eq!(report.offenders.len(), 1, "{:?}", report.offenders);
    assert!(report.offenders[0].contains("permissions"));
    // A `..` base could carry anything.
    let spread = "fn p(b: ClaudeHostPolicy) -> ClaudeHostPolicy { ClaudeHostPolicy { permissions: \
                  claude_runtime_protocol::v1::PermissionMode::Interactive as i32, \
                  permission_mode_switchable: false, ..b } }";
    assert_eq!(one("x.rs", spread).offenders.len(), 1);
}

#[test]
fn disallowed_tools_passes_and_allowed_tools_fails() {
    assert!(
        one("x.rs", "fn f() { let disallowed_tools = 1; g(\"--disallowedTools\"); }")
            .offenders
            .is_empty()
    );
    assert_eq!(one("x.rs", "fn f(allowed_tools: Vec<String>) {}").offenders.len(), 1);
    assert_eq!(one("x.rs", "fn f() { g(\"--allowedTools\"); }").offenders.len(), 1);
}

#[test]
fn the_cli_flags_fail_in_any_case_and_comments_do_not() {
    assert_eq!(one("x.rs", "const M: &str = \"bypassPermissions\";").offenders.len(), 1);
    assert_eq!(one("x.rs", "g(\"--dangerously-skip-permissions\");").offenders.len(), 1);
    assert_eq!(
        one("x.rs", "o.allowDangerouslySkipPermissions = true;").offenders.len(),
        1
    );
    // Mentioning them in prose is how the history is kept; a `//` inside a string is not a comment.
    let prose = "/// never `bypassPermissions`\n// nor --dangerously-skip-permissions\n/* allowedTools */\n\
                 const URL: &str = \"http://[::]:50051\"; // bypassPermissions\n";
    assert!(
        one("x.rs", prose).offenders.is_empty(),
        "{:?}",
        one("x.rs", prose).offenders
    );
}

#[test]
fn the_deleted_switch_stays_deleted() {
    assert_eq!(one("x.rs", "client.set_permission_mode(r)").offenders.len(), 1);
    assert_eq!(one("x.rs", "struct SetPermissionModeRequest;").offenders.len(), 1);
    assert_eq!(one("x.rs", "p.set_permissions(m)").offenders.len(), 1);
    assert_eq!(one("x.rs", "p.permission_mode_switchable = true;").offenders.len(), 1);
    assert_eq!(one("x.rs", "policy.permissions = 3;").offenders.len(), 1);
}

#[test]
fn test_code_is_not_scanned() {
    // An inline test module, whatever attributes and visibility it carries.
    let inline = "fn f() {}\n#[cfg(test)]\n#[allow(dead_code)]\npub(crate) mod tests {\n    const M: &str = \"bypassPermissions\";\n}\n";
    assert!(one("x.rs", inline).offenders.is_empty());
    // A file reached only through `#[cfg(test)] mod x;` beside its declaring file.
    let tree = BTreeMap::from([
        ("lib.rs".to_string(), "mod a;\n#[cfg(test)]\nmod probe;\n".to_string()),
        ("a/mod.rs".to_string(), "#[cfg(test)] mod deep;\n".to_string()),
        (
            "probe.rs".to_string(),
            "const M: &str = \"bypassPermissions\";".to_string(),
        ),
        (
            "a/deep.rs".to_string(),
            "const M: &str = \"bypassPermissions\";".to_string(),
        ),
    ]);
    let report = scan_tree(&tree);
    assert!(report.offenders.is_empty(), "{:?}", report.offenders);
    assert_eq!(
        report.skipped,
        BTreeSet::from(["probe.rs".to_string(), "a/deep.rs".to_string()])
    );
    // The same file also declared outside a test is production code, and is scanned.
    let mut both = tree.clone();
    both.insert(
        "lib.rs".to_string(),
        "mod a;\nmod probe;\n#[cfg(test)]\nmod probe;\n".to_string(),
    );
    let report = scan_tree(&both);
    assert_eq!(report.offenders.len(), 1, "{:?}", report.offenders);
    assert!(report.offenders[0].starts_with("probe.rs"), "{:?}", report.offenders);
    assert_eq!(report.skipped, BTreeSet::from(["a/deep.rs".to_string()]));
}

#[test]
fn the_allowlisted_arm_passes_only_in_its_own_file_and_arm() {
    let arm = "use claude_runtime_protocol::v1::{PermissionMode as ProtoPermissionMode};\n\
               fn t(e: E) { match e { ProtoEvent::SessionReady(r) => {}\n\
               ProtoEvent::PermissionModeChanged(changed) => { let m = match changed.mode() {\n\
               ProtoPermissionMode::Bypass => 1, _ => 0 }; } } }\n";
    let report = one(ALLOWED_ARM.0, arm);
    assert!(report.offenders.is_empty(), "{:?}", report.offenders);
    assert_eq!(report.allowlisted, 1);
    // The same text anywhere else fails.
    assert_eq!(one("providers/claude_sidecar/mod.rs", arm).offenders.len(), 1);
    // In the right file but another arm, or as a constructor in the right arm, it fails.
    let other_arm = arm.replace(
        "ProtoEvent::PermissionModeChanged(changed)",
        "ProtoEvent::Other(changed)",
    );
    assert_eq!(one(ALLOWED_ARM.0, &other_arm).offenders.len(), 1);
    let constructor = arm.replace(
        "ProtoPermissionMode::Bypass => 1",
        "_ if x == ProtoPermissionMode::Bypass as i32 => 1",
    );
    assert_eq!(one(ALLOWED_ARM.0, &constructor).offenders.len(), 1);
    // And only once.
    let twice = arm.replace("_ => 0", "ProtoPermissionMode::Bypass => 2, _ => 0");
    assert_eq!(one(ALLOWED_ARM.0, &twice).offenders.len(), 1);
}
