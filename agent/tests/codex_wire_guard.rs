//! What Eitri sends to Codex is an allowlist, and this scan holds the part of it the compiler cannot see:
//! the string literals in `agent/src/providers/codex/`.
//!
//! Codex's wire has a no-ask approval policy (`never`), an unsandboxed mode (`danger-full-access`),
//! answers that widen what later runs without asking (`acceptForSession`, an exec-policy amendment), a
//! session-wide permission grant, and config keys that would trust hooks or pre-approve tool servers other
//! than the editor's own. Any one of them, written once as a literal, would put a Codex tab outside the posture its card and its
//! `prefix i` line describe, without a single signature changing. So:
//!
//! - every literal that is a value of one of the guarded fields (approval policy, reviewer, sandbox mode,
//!   sandbox policy type, a command or file decision, a permission grant's scope) must be an allowed one;
//! - a list of literals is refused outright, whole or inside a longer literal (a JSON text, a method name);
//! - `"untrusted"` -- also an approval policy -- may appear exactly once, as the trust level of a
//!   `projects` entry, on the line annotated `// codex-wire-guard: DirTrust` in `wire.rs`;
//! - no literal names a dotted config path such as `projects./a/b.trust_level`: the app-server splits a
//!   top-level config key on every `.`, so a dotted key names a garbage path and Codex then writes trust
//!   into the user's own file;
//! - `"mcp_servers"` may appear exactly once, as the editor's server, on the line annotated
//!   `// codex-wire-guard: EditorServer` in `config.rs`; tests turn servers off under a test-only key;
//! - a keyed field's refused values are everyday words (`auto`, `prompt`), so they are refused only as the
//!   value of the key: the key is spelled exactly once, as a literal of its own, and the literal after it
//!   is the allowed value;
//! - no identifier spells a refused value either: serde derives wire words from names
//!   (`#[serde(rename_all = "camelCase")]` turns a variant `AcceptForSession` into `"acceptForSession"`), so
//!   a name equal to a refused value once case, `-` and `_` are ignored is refused, except the one trust
//!   level `DirTrust::Untrusted` that the annotated line maps to its word;
//! - no method of Codex's file system (`fs/...`) and no `config/...` method but `config/read`: those read
//!   and write files and the user's own configuration directly.
//!
//! Comments are removed by a lexer that knows string literals (a `//` inside `"http://..."` is not a
//! comment), and test code (`#[cfg(test)]` and `#[cfg(feature = "test-support")]` items, and files reached
//! only through such a `mod`) is out of scope; a second test holds that `test-support` reaches no release
//! build. Rules look at literal contents, never at where rustfmt breaks a line.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const CODEX_DIR: &str = "src/providers/codex";
/// The definition the scan exists to cover: a scan that no longer finds it is reading the wrong place.
const ANCHOR: &str = "fn session_config(";
const ANNOTATION: &str = "codex-wire-guard: DirTrust";
const EDITOR_ANNOTATION: &str = "codex-wire-guard: EditorServer";

/// Each guarded field: every value Codex 0.160.0 accepts for it, and the ones Eitri may send.
const FIELDS: &[(&str, &[&str], &[&str])] = &[
    (
        "approvalPolicy",
        &["on-request", "on-failure", "never", "granular"],
        &["on-request"],
    ),
    (
        "approvalsReviewer",
        &["user", "auto_review", "guardian_subagent"],
        &["user", "auto_review"],
    ),
    (
        "sandbox",
        &["read-only", "workspace-write", "danger-full-access"],
        &["read-only", "workspace-write"],
    ),
    (
        "sandboxPolicy.type",
        &["readOnly", "workspaceWrite", "dangerFullAccess", "externalSandbox"],
        &["readOnly", "workspaceWrite"],
    ),
    (
        "decision",
        &[
            "accept",
            "acceptForSession",
            "acceptWithExecpolicyAmendment",
            "applyNetworkPolicyAmendment",
            "decline",
            "cancel",
        ],
        &["accept", "decline", "cancel"],
    ),
    ("scope", &["turn", "session"], &["turn"]),
];

/// A field whose values are everyday words, which a name (`Handling::Auto`) or a key of something else
/// (`"prompt"` in a sub-agent's call) may spell: its key, every value Codex 0.163.0 accepts, and the ones Eitri
/// may send. The key is what the scan holds, so a value can only be sent through it.
const KEYED_FIELDS: &[(&str, &[&str], &[&str])] = &[(
    "default_tools_approval_mode",
    &["auto", "prompt", "writes", "approve"],
    &["approve"],
)];

/// Refused anywhere inside a literal: none of these can be part of an innocent sentence.
const DENIED_ANYWHERE: &[&str] = &[
    "danger-full-access",
    "dangerFullAccess",
    "externalSandbox",
    "acceptForSession",
    "acceptWithExecpolicyAmendment",
    "applyNetworkPolicyAmendment",
    "strictAutoReview",
    "bypass_hook_trust",
    "experimentalApi",
    "historyMode",
    "guardian_subagent",
    "account/login",
    "command/exec",
    "process/spawn",
    "thread/approveGuardianDeniedAction",
];

/// Refused as a whole literal, or quoted inside one (`"{\"scope\":\"session\"}"`); short words that
/// could appear in a notice's prose are not refused there.
const DENIED_WORDS: &[&str] = &[
    "never",
    "granular",
    "on-failure",
    "session",
    "hooks",
    "untrusted",
    "mcp_servers",
];

/// Top-level config tables: a literal starting with one of these and a `.` is a dotted config path.
const CONFIG_TABLES: &[&str] = &[
    "projects",
    "skills",
    "features",
    "sandbox_workspace_write",
    "mcp_servers",
    "hooks",
    "permissions",
    "sandbox_mode",
    "approval_policy",
    "approvals_reviewer",
    "model_providers",
    "profiles",
    "shell_environment_policy",
];

#[derive(Debug)]
struct Lit {
    text: String,
    line: usize,
    offset: usize,
}

struct Lexed {
    /// The source with every comment and every literal's contents blanked, newlines kept.
    code: String,
    lits: Vec<Lit>,
    /// `(line, text)` of every comment.
    comments: Vec<(usize, String)>,
}

fn lex(src: &str) -> Lexed {
    let b = src.as_bytes();
    let mut code = String::with_capacity(src.len());
    let mut lits = Vec::new();
    let mut comments = Vec::new();
    let mut line = 1usize;
    let mut i = 0usize;
    // One space per byte, so an offset into the blanked code is the same offset into the source.
    let blank = |code: &mut String, s: &str| {
        for c in s.chars() {
            if c == '\n' {
                code.push('\n');
            } else {
                code.extend(std::iter::repeat_n(' ', c.len_utf8()));
            }
        }
    };
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    while i < b.len() {
        let c = b[i];
        // Comments.
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            let end = src[i..].find('\n').map_or(b.len(), |n| i + n);
            comments.push((line, src[i..end].to_string()));
            blank(&mut code, &src[i..end]);
            i = end;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let mut depth = 0usize;
            let start = i;
            while i < b.len() {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            comments.push((line, src[start..i].to_string()));
            line += src[start..i].matches('\n').count();
            blank(&mut code, &src[start..i]);
            continue;
        }
        // Raw strings and byte strings: r"..", r#".."#, b"..", br#".."#.
        let prev_ident = i > 0 && is_ident(b[i - 1]);
        if !prev_ident && (c == b'r' || c == b'b') {
            let mut j = i;
            if b[j] == b'b' {
                j += 1;
            }
            let raw = b.get(j) == Some(&b'r');
            if raw {
                j += 1;
            }
            let mut hashes = 0;
            while raw && b.get(j) == Some(&b'#') {
                hashes += 1;
                j += 1;
            }
            if b.get(j) == Some(&b'"') && (raw || b[i] == b'b') {
                if raw {
                    let close = format!("\"{}", "#".repeat(hashes));
                    let body_start = j + 1;
                    let end = src[body_start..].find(&close).map_or(b.len(), |n| body_start + n);
                    let text = &src[body_start..end];
                    lits.push(Lit {
                        text: text.to_string(),
                        line,
                        offset: i,
                    });
                    code.push_str(&src[i..body_start]);
                    blank(&mut code, text);
                    line += text.matches('\n').count();
                    let stop = (end + close.len()).min(b.len());
                    code.push_str(&src[end..stop]);
                    i = stop;
                    continue;
                }
                code.push_str(&src[i..j]);
                i = j; // a plain byte string: lexed below as a string
            }
        }
        if b[i] == b'"' {
            let start = i;
            let mut text = String::new();
            i += 1;
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' && i + 1 < b.len() {
                    let e = b[i + 1];
                    match e {
                        b'n' => text.push('\n'),
                        b't' => text.push('\t'),
                        b'"' => text.push('"'),
                        b'\\' => text.push('\\'),
                        b'\n' => {
                            // A line continuation: the newline and the next line's indentation vanish.
                            line += 1;
                            i += 2;
                            while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
                                i += 1;
                            }
                            continue;
                        }
                        _ => text.push(e as char),
                    }
                    i += 2;
                    continue;
                }
                let ch = src[i..].chars().next().unwrap_or('\0');
                if ch == '\n' {
                    line += 1;
                }
                text.push(ch);
                i += ch.len_utf8();
            }
            i = (i + 1).min(b.len());
            code.push('"');
            blank(&mut code, &src[start + 1..i.saturating_sub(1).max(start + 1)]);
            code.push('"');
            lits.push(Lit {
                text,
                line,
                offset: start,
            });
            continue;
        }
        // A char literal or a lifetime.
        if c == b'\'' {
            let close = if b.get(i + 1) == Some(&b'\\') {
                src[i + 2..].find('\'').map(|n| i + 2 + n)
            } else {
                let ch_len = src[i + 1..].chars().next().map_or(1, |c| c.len_utf8());
                (b.get(i + 1 + ch_len) == Some(&b'\'')).then_some(i + 1 + ch_len)
            };
            if let Some(end) = close {
                code.push('\'');
                blank(&mut code, &src[i + 1..end]);
                code.push('\'');
                i = end + 1;
                continue;
            }
        }
        if c == b'\n' {
            line += 1;
        }
        let ch = src[i..].chars().next().unwrap_or('\0');
        code.push(ch);
        i += ch.len_utf8();
    }
    Lexed { code, lits, comments }
}

/// Where the next test-only attribute starts and ends: `#[cfg(test)]`, or `#[cfg(feature = "test-support")]`
/// (its feature name is blanked in `code`, so it is read from `src`, which has the same offsets).
fn next_test_attr(code: &str, src: &str, from: usize) -> Option<(usize, usize)> {
    let plain = find_squeezed(code, from, "#[cfg(test)]");
    let mut feature = None;
    let mut at = from;
    while let Some((start, after)) = find_squeezed(code, at, "#[cfg(feature=\"") {
        let name = "test-support";
        let close = after + name.len();
        if src.get(after..close) == Some(name) {
            if let Some((_, end)) = find_squeezed(code, close, "\")]").filter(|&(s, _)| s == close) {
                feature = Some((start, end));
                break;
            }
        }
        at = start + 1;
    }
    match (plain, feature) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

/// Byte ranges of test-only items in the blanked code, and the names of test-only `mod x;` lines.
fn test_items(code: &str, src: &str) -> (Vec<(usize, usize)>, Vec<String>) {
    let b = code.as_bytes();
    let mut ranges = Vec::new();
    let mut test_mods = Vec::new();
    let mut from = 0;
    while let Some(pos) = next_test_attr(code, src, from) {
        let (start, after) = pos;
        let mut i = after;
        let mut depth = 0usize;
        let mut end = b.len();
        while i < b.len() {
            match b[i] {
                b'{' => depth += 1,
                b'}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        end = i + 1;
                        break;
                    }
                }
                b';' if depth == 0 => {
                    end = i + 1;
                    let item: String = code[after..i].split_whitespace().collect::<Vec<_>>().join(" ");
                    if let Some(name) = item.strip_prefix("mod ").or_else(|| item.strip_prefix("pub mod ")) {
                        test_mods.push(name.trim().to_string());
                    }
                    break;
                }
                _ => {}
            }
            i += 1;
        }
        ranges.push((start, end));
        from = end;
    }
    (ranges, test_mods)
}

/// Finds `needle` (written without spaces) allowing whitespace between its characters.
fn find_squeezed(code: &str, from: usize, needle: &str) -> Option<(usize, usize)> {
    let b = code.as_bytes();
    let n = needle.as_bytes();
    let mut s = from;
    while s < b.len() {
        if b[s] == n[0] {
            let (mut i, mut k) = (s, 0);
            while i < b.len() && k < n.len() {
                if b[i] == n[k] {
                    k += 1;
                    i += 1;
                } else if b[i].is_ascii_whitespace() {
                    i += 1;
                } else {
                    break;
                }
            }
            if k == n.len() {
                return Some((s, i));
            }
        }
        s += 1;
    }
    None
}

fn quoted(word: &str) -> String {
    format!("\"{word}\"")
}

/// The one `config/` method Eitri may call: it only reads.
const CONFIG_READ: &str = "config/read";

/// Method names a literal may carry: the whole literal when it starts like one, and every quoted one
/// inside a JSON text.
fn method_names(t: &str) -> Vec<&str> {
    let mut found = Vec::new();
    for prefix in ["fs/", "config/"] {
        if t.starts_with(prefix) {
            found.push(t);
        }
        let quoted_prefix = format!("\"{prefix}");
        let mut from = 0;
        while let Some(n) = t[from..].find(&quoted_prefix) {
            let start = from + n + 1;
            let end = t[start..].find('"').map_or(t.len(), |e| start + e);
            found.push(&t[start..end]);
            from = end;
        }
    }
    found
}

/// A wire word or a Rust name with case, `-` and `_` ignored: `acceptForSession` and `AcceptForSession`
/// are the same, as are `on-failure` and `OnFailure`.
fn normalized(word: &str) -> String {
    word.chars()
        .filter(|c| *c != '-' && *c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Every identifier in code whose comments and literals are blanked, with its byte offset.
fn identifiers(code: &str) -> Vec<(usize, &str)> {
    let b = code.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_alphabetic() || b[i] == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push((start, &code[start..i]));
        } else if b[i].is_ascii_digit() {
            // A number's suffix (`1u8`) is not a name.
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    out
}

/// Every rule over a set of `(file name, source)` pairs; returns one line per violation.
fn violations(files: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::new();
    let mut anchor_seen = false;
    let mut annotated_untrusted = 0usize;
    let mut annotated_editor = 0usize;
    let mut keys_seen: Vec<&str> = Vec::new();
    let lexed: Vec<(String, &str, Lexed)> = files.iter().map(|(n, s)| (n.clone(), s.as_str(), lex(s))).collect();
    let mut test_only_files = BTreeSet::new();
    for (_, src, l) in &lexed {
        let (_, mods) = test_items(&l.code, src);
        for m in mods {
            test_only_files.insert(format!("{m}.rs"));
            test_only_files.insert(format!("{m}/mod.rs"));
        }
    }
    let allowed_anywhere: BTreeSet<&str> = FIELDS.iter().flat_map(|(_, _, ok)| ok.iter().copied()).collect();
    let denied_idents: BTreeSet<String> = FIELDS
        .iter()
        .flat_map(|(_, universe, ok)| universe.iter().filter(move |v| !ok.contains(v)))
        .filter(|v| !allowed_anywhere.contains(*v))
        .chain(KEYED_FIELDS.iter().map(|(key, _, _)| key))
        .chain(DENIED_ANYWHERE.iter())
        .chain(DENIED_WORDS.iter())
        .filter(|v| !v.contains('/'))
        .map(|v| normalized(v))
        .collect();
    for (name, src, l) in &lexed {
        if test_only_files
            .iter()
            .any(|t| name == t || name.ends_with(&format!("/{t}")))
        {
            continue;
        }
        let (ranges, _) = test_items(&l.code, src);
        let in_test = |off: usize| ranges.iter().any(|&(s, e)| off >= s && off < e);
        let live_code: String = {
            let mut c = l.code.clone().into_bytes();
            for &(s, e) in &ranges {
                for b in &mut c[s..e] {
                    if *b != b'\n' {
                        *b = b' ';
                    }
                }
            }
            String::from_utf8(c).unwrap_or_default()
        };
        if find_squeezed(&live_code, 0, ANCHOR).is_some() || live_code.contains(ANCHOR) {
            anchor_seen = true;
        }
        let lines_marked = |mark: &str| -> BTreeSet<usize> {
            l.comments
                .iter()
                .filter(|(_, t)| t.contains(mark))
                .map(|(n, _)| *n)
                .collect()
        };
        let annotated_lines = lines_marked(ANNOTATION);
        let editor_lines = lines_marked(EDITOR_ANNOTATION);
        let mut annotated_used = BTreeSet::new();
        let mut editor_used = BTreeSet::new();
        let live_lits: Vec<&Lit> = l.lits.iter().filter(|lit| !in_test(lit.offset)).collect();
        for (index, lit) in live_lits.iter().enumerate() {
            let t = lit.text.as_str();
            let at = format!("{name}:{}", lit.line);
            for (field, universe, ok) in FIELDS {
                if universe.contains(&t) && !ok.contains(&t) && !allowed_anywhere.contains(t) {
                    out.push(format!("{at}: {t:?} is a {field} value Eitri never sends"));
                }
            }
            for d in DENIED_ANYWHERE {
                if t.contains(d) {
                    out.push(format!("{at}: {t:?} contains the refused {d:?}"));
                }
            }
            for w in DENIED_WORDS {
                let whole = t == *w;
                let inside = t.contains(&quoted(w));
                if !(whole || inside) {
                    continue;
                }
                if *w == "untrusted" && whole && name.ends_with("wire.rs") && annotated_lines.contains(&lit.line) {
                    annotated_untrusted += 1;
                    annotated_used.insert(lit.line);
                    continue;
                }
                if *w == "mcp_servers" && whole && name == "config.rs" && editor_lines.contains(&lit.line) {
                    annotated_editor += 1;
                    editor_used.insert(lit.line);
                    continue;
                }
                out.push(format!("{at}: {t:?} is refused ({w:?})"));
            }
            for (key, universe, ok) in KEYED_FIELDS {
                if t == *key {
                    keys_seen.push(key);
                    match live_lits.get(index + 1).map(|next| next.text.as_str()) {
                        Some(value) if ok.contains(&value) => {}
                        Some(value) if universe.contains(&value) => {
                            out.push(format!("{at}: {value:?} is a {key} value Eitri never sends"));
                        }
                        value => out.push(format!(
                            "{at}: {key} is followed by {value:?}: its value must be the next literal"
                        )),
                    }
                } else if t.contains(key) {
                    out.push(format!(
                        "{at}: {t:?} holds {key:?} inside a longer literal; it is written as a key of its own"
                    ));
                }
            }
            for table in CONFIG_TABLES {
                if t.starts_with(&format!("{table}.")) {
                    out.push(format!("{at}: {t:?} is a dotted config path; send nested objects"));
                }
            }
            for method in method_names(t) {
                if method.starts_with("fs/") || (method.starts_with("config/") && method != CONFIG_READ) {
                    out.push(format!(
                        "{at}: {t:?} names {method:?}, which reads or writes files or the user's configuration"
                    ));
                }
            }
        }
        for (offset, ident) in identifiers(&live_code) {
            let key = normalized(ident);
            if !denied_idents.contains(&key) {
                continue;
            }
            let before = live_code[..offset].trim_end();
            let dir_trust = before
                .strip_suffix("::")
                .is_some_and(|b| b.trim_end().ends_with("DirTrust"))
                && ident == "Untrusted";
            if dir_trust {
                continue;
            }
            let line = live_code[..offset].matches('\n').count() + 1;
            out.push(format!(
                "{name}:{line}: the name {ident:?} spells a refused wire value (serde derives words from names)"
            ));
        }
        for n in annotated_lines.difference(&annotated_used) {
            out.push(format!(
                "{name}:{n}: the {ANNOTATION:?} annotation marks no \"untrusted\" literal"
            ));
        }
        for n in editor_lines.difference(&editor_used) {
            out.push(format!(
                "{name}:{n}: the {EDITOR_ANNOTATION:?} annotation marks no \"mcp_servers\" literal"
            ));
        }
    }
    if !anchor_seen {
        out.push(format!(
            "no file defines {ANCHOR:?}: the scan is not reading the codex provider"
        ));
    }
    if annotated_untrusted != 1 {
        out.push(format!(
            "\"untrusted\" must be spelled exactly once, as the annotated trust level in wire.rs; found {annotated_untrusted}"
        ));
    }
    if annotated_editor != 1 {
        out.push(format!(
            "\"mcp_servers\" must be spelled exactly once, as the annotated editor server in config.rs; found {annotated_editor}"
        ));
    }
    for (key, _, _) in KEYED_FIELDS {
        let found = keys_seen.iter().filter(|seen| *seen == key).count();
        if found != 1 {
            out.push(format!("{key:?} must be spelled exactly once; found {found}"));
        }
    }
    out
}

fn codex_sources() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(CODEX_DIR);
    let mut files = Vec::new();
    collect(&dir, &dir, &mut files);
    files.sort();
    assert!(!files.is_empty(), "{} holds no .rs file", dir.display());
    files
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    for entry in entries {
        let path: PathBuf = entry.expect("a directory entry").path();
        if path.is_dir() {
            collect(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
            out.push((rel, text));
        }
    }
}

#[test]
fn codex_wire_guard_allowlist() {
    let found = violations(&codex_sources());
    assert!(
        found.is_empty(),
        "the Codex send allowlist is broken:\n{}",
        found.join("\n")
    );
}

// ---- the scan itself ----

fn with_base(extra: &str) -> Vec<(String, String)> {
    let wire = "fn level(t: T) -> &'static str { match t { A => \"trusted\", B => \"untrusted\", // codex-wire-guard: DirTrust\n } }\n";
    let config = "pub fn session_config(p: &P) {}\n";
    vec![
        ("wire.rs".into(), wire.into()),
        ("config.rs".into(), format!("{config}{EDITOR_SERVER}{extra}")),
    ]
}

/// The editor's server as `config.rs` spells it: the annotated key and the one approval mode.
const EDITOR_SERVER: &str = "fn s(c: &mut C) { c.insert(\"mcp_servers\", e()); // codex-wire-guard: EditorServer\n}\n\
     fn e() -> V { json!({ \"default_tools_approval_mode\": \"approve\" }) }\n";

#[test]
fn a_clean_tree_passes() {
    assert_eq!(violations(&with_base("")), Vec::<String>::new());
}

#[test]
fn refused_values_fail() {
    for planted in [
        "const P: &str = \"never\";",
        "const P: &str = \"on-failure\";",
        "const S: &str = \"danger-full-access\";",
        "const T: &str = \"dangerFullAccess\";",
        "const T: &str = \"externalSandbox\";",
        "fn d() -> &'static str { \"acceptForSession\" }",
        "const J: &str = r#\"{\"scope\":\"session\"}\"#;",
        "const J: &str = \"{\\\"approvalPolicy\\\":\\\"never\\\"}\";",
        "const K: &str = \"hooks\";",
        "const M: &str = \"mcp_servers\";",
        "const X: &str = \"experimentalApi\";",
        "const H: &str = \"historyMode\";",
        "const L: &str = \"account/login/start\";",
        "const R: &str = \"auto_review\"; const G: &str = \"guardian_subagent\";",
    ] {
        assert!(!violations(&with_base(planted)).is_empty(), "not caught: {planted}");
    }
}

#[test]
fn untrusted_only_on_the_annotated_line() {
    // Unannotated, in another file, or twice: each fails.
    assert!(!violations(&with_base("const U: &str = \"untrusted\";")).is_empty());
    assert!(!violations(&with_base(
        "const U: &str = \"untrusted\"; // codex-wire-guard: DirTrust"
    ))
    .is_empty());
    let none = vec![("config.rs".to_string(), "pub fn session_config(p: &P) {}".to_string())];
    assert!(!violations(&none).is_empty(), "the annotated spelling is required");
    // An annotation that marks nothing is stale.
    let stale = with_base("// codex-wire-guard: DirTrust\nconst A: u8 = 1;");
    assert!(!violations(&stale).is_empty());
}

#[test]
fn an_annotated_mcp_servers_passes_once() {
    assert_eq!(violations(&with_base("")), Vec::<String>::new());
    // Gone: the scan no longer sees the one place it was told about.
    let mut none = with_base("");
    none[1].1 = none[1].1.replace("\"mcp_servers\"", "\"other\"");
    let found = violations(&none);
    assert!(found.iter().any(|v| v.contains("mcp_servers")), "{found:?}");
    // An annotation with no literal under it, and the annotated line in another file.
    assert!(!violations(&with_base("// codex-wire-guard: EditorServer\nconst A: u8 = 1;")).is_empty());
    let mut moved = with_base("");
    moved.swap(0, 1);
    moved[0].0 = "provider.rs".into();
    assert!(!violations(&moved).is_empty());
}

#[test]
fn a_second_mcp_servers_fails() {
    for second in [
        "const M: &str = \"mcp_servers\";",
        "const M: &str = \"mcp_servers\"; // codex-wire-guard: EditorServer",
        "const J: &str = \"{\\\"mcp_servers\\\":{}}\";",
        "const K: &str = \"mcp_servers.eitri.enabled\";",
        "pub struct Config { pub mcp_servers: u8 }",
    ] {
        assert!(!violations(&with_base(second)).is_empty(), "{second}");
    }
}

#[test]
fn another_approval_mode_fails() {
    for mode in ["auto", "prompt", "writes", "other"] {
        let mut files = with_base("");
        files[1].1 = files[1].1.replace("\"approve\"", &format!("\"{mode}\""));
        assert!(!violations(&files).is_empty(), "{mode}");
    }
    for spelling in [
        // The value out of the scan's sight, the key inside a longer text, the key a second time, a derived key.
        "const M: &str = \"auto\"; fn f() { json!({ \"default_tools_approval_mode\": M }) }",
        "const J: &str = \"{\\\"default_tools_approval_mode\\\":\\\"auto\\\"}\";",
        "fn f() { json!({ \"default_tools_approval_mode\": \"approve\" }) }",
        "pub struct Entry { pub default_tools_approval_mode: u8 }",
    ] {
        assert!(!violations(&with_base(spelling)).is_empty(), "{spelling}");
    }
    // The words themselves stay free away from the key: a sub-agent's call has a `prompt`.
    let free =
        "enum H { Auto, Prompt } fn f(c: &C) -> V { let auto = 1; json!({ \"prompt\": c.prompt, \"x\": \"writes\" }) }";
    assert_eq!(violations(&with_base(free)), Vec::<String>::new());
}

#[test]
fn dotted_config_paths_fail() {
    for planted in [
        "const K: &str = \"projects./a.b/c.trust_level\";",
        "fn k(d: &str) -> String { format!(\"projects.{d}.trust_level\") }",
        "const K: &str = \"sandbox_workspace_write.network_access\";",
    ] {
        assert!(!violations(&with_base(planted)).is_empty(), "not caught: {planted}");
    }
}

#[test]
fn comments_and_test_code_are_out_of_scope() {
    let ok = "// never and \"never\" in a comment are prose\n\
              /* \"danger-full-access\" */\n\
              const U: &str = \"http://localhost//x\";\n\
              #[cfg(test)]\nmod tests { const N: &str = \"never\"; fn f() { let s = \"}\"; } }\n\
              const C: char = '\"';\n\
              fn l<'a>(x: &'a str) -> &'a str { x }\n";
    assert_eq!(violations(&with_base(ok)), Vec::<String>::new());
    // A file reached only through `#[cfg(test)] mod` is test code too.
    let mut files = with_base("#[cfg(test)]\nmod fixtures;\n");
    files.push(("fixtures.rs".into(), "const N: &str = \"never\";".into()));
    assert_eq!(violations(&files), Vec::<String>::new());
    // The anchor is required.
    let no_anchor = vec![("wire.rs".to_string(), with_base("")[0].1.clone())];
    assert!(!violations(&no_anchor).is_empty());
}

#[test]
fn names_serde_turns_into_refused_words_fail() {
    for planted in [
        "#[derive(Serialize)]\n#[serde(rename_all = \"camelCase\")]\npub enum Decision { Accept, AcceptForSession, Decline }",
        "pub enum Sandbox { WorkspaceWrite, DangerFullAccess }",
        "pub enum SandboxPolicy { ExternalSandbox }",
        "#[serde(rename_all = \"kebab-case\")] pub enum Policy { OnRequest, OnFailure }",
        "pub enum Policy { OnRequest, Never }",
        "pub enum Scope { Turn, Session }",
        "pub struct Config { pub mcp_servers: u8 }",
        "pub struct Config { hooks: u8 }",
        "pub struct Init { experimental_api: bool }",
        "pub enum Reviewer { User, GuardianSubagent }",
        "pub enum Trust { Trusted, Untrusted }",
    ] {
        assert!(!violations(&with_base(planted)).is_empty(), "not caught: {planted}");
    }
    // The trust level the annotated line maps to its word is the one exception.
    let ok = "fn t(d: DirTrust) -> bool { matches!(d, DirTrust :: Untrusted) }";
    assert_eq!(violations(&with_base(ok)), Vec::<String>::new());
}

#[test]
fn file_system_and_config_writing_methods_fail() {
    for planted in [
        "const W: &str = \"fs/writeFile\";",
        "const R: &str = \"fs/readFile\";",
        "const V: &str = \"config/value/write\";",
        "const B: &str = \"config/batchWrite\";",
        "const L: &str = \"config/mcpServer/reload\";",
        "const J: &str = r#\"{\"method\":\"fs/remove\"}\"#;",
        "fn m(x: &str) -> String { format!(\"fs/{x}\") }",
    ] {
        assert!(!violations(&with_base(planted)).is_empty(), "not caught: {planted}");
    }
    let ok = "const C: &str = \"config/read\"; const Q: &str = \"configRequirements/read\";";
    assert_eq!(violations(&with_base(ok)), Vec::<String>::new());
}

#[test]
fn offsets_survive_non_ascii_text() {
    // Wide characters before a test module must not shift where the scan thinks that module is.
    let dashes = "\u{2014}".repeat(20);
    let planted = format!(
        "// {dashes}\nconst A: &str = \"never\";\n#[cfg(test)]\nmod tests {{ const B: u8 = 1; const C: u8 = 2; \
         const D: u8 = 3; const E: u8 = 4; }}\n"
    );
    assert!(
        !violations(&with_base(&planted)).is_empty(),
        "not caught after non-ASCII text"
    );
    let in_test = format!("// {dashes}\n#[cfg(test)]\nmod tests {{ const A: &str = \"never\"; }}\n");
    assert_eq!(violations(&with_base(&in_test)), Vec::<String>::new());
    let lexed = lex(&planted);
    assert_eq!(lexed.code.len(), planted.len());
}

#[test]
fn test_support_items_are_test_code_and_other_features_are_not() {
    let ok = "#[cfg(feature = \"test-support\")]\nconst M: &str = \"mcp_servers\";\n";
    assert_eq!(violations(&with_base(ok)), Vec::<String>::new());
    let other = "#[cfg(feature = \"legacy-backend\")]\nconst M: &str = \"mcp_servers\";\n";
    assert!(!violations(&with_base(other)).is_empty());
    let longer = "#[cfg(feature = \"test-supportx\")]\nconst M: &str = \"mcp_servers\";\n";
    assert!(!violations(&with_base(longer)).is_empty());
}

/// Every `Cargo.toml` of the workspace: the root's and those one or two directories below it.
fn workspace_manifests() -> Vec<(PathBuf, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the agent crate sits in the workspace")
        .to_path_buf();
    let mut out = Vec::new();
    let mut dirs = vec![(root.clone(), 0)];
    while let Some((dir, depth)) = dirs.pop() {
        let manifest = dir.join("Cargo.toml");
        if let Ok(text) = std::fs::read_to_string(&manifest) {
            out.push((manifest, text));
        }
        if depth == 2 {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() && !name.starts_with('.') && name != "target" && name != "node_modules" {
                dirs.push((path, depth + 1));
            }
        }
    }
    out
}

/// What makes `test-support` items test code above: no manifest turns the feature on for a build that
/// ships. It may be named only by the agent crate's own `[features]` table (not in `default`) and by a
/// dev-dependency on the agent crate.
#[test]
fn test_support_reaches_no_release_build() {
    let manifests = workspace_manifests();
    assert!(
        manifests.iter().any(|(p, _)| p.ends_with("agent/Cargo.toml")),
        "the agent crate's manifest was not found"
    );
    let mut found = Vec::new();
    for (path, text) in &manifests {
        let mut section = String::new();
        let mut default_list = false;
        for (n, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.starts_with('[') {
                section = line.to_string();
                default_list = false;
                continue;
            }
            if section == "[features]" && line.starts_with("default") {
                default_list = true;
            }
            if !line.contains("test-support") {
                if line.ends_with(']') {
                    default_list = false;
                }
                continue;
            }
            let at = format!("{}:{}", path.display(), n + 1);
            if section == "[features]" {
                if default_list || line.contains("agent/test-support") || line.contains("agent?/test-support") {
                    found.push(format!("{at}: {line}"));
                }
                if line.ends_with(']') {
                    default_list = false;
                }
                continue;
            }
            let names_agent = line.starts_with("agent ") || line.starts_with("agent=") || section.ends_with(".agent]");
            if names_agent && !section.contains("dev-dependencies") {
                found.push(format!("{at}: {line}"));
            }
        }
    }
    assert!(
        found.is_empty(),
        "the agent crate's test-support feature can reach a release build:\n{}",
        found.join("\n")
    );
}
