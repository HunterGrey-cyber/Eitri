//! The wire guard: a scan of this
//! crate's own non-test sources for every way back to a `claude` that runs without the gate.
//!
//! Since 2026-09-27 the CLI's permission mode is a constant. Every session, on both backends, is gated --
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
//! breaks a line never decides the verdict.
//!
//! **Scope:** every `agent/src/**/*.rs`, `src/bin/` included, and `server/src/**/*.rs`.
//!
//! **The forwarding trees** -- the helper (`server/src/`) and the sidecar transport
//! (`agent/src/providers/claude_sidecar/transport/`) -- have rules of their own ([`scan_remote_decisions`]),
//! because they pass a person's permission answers along and must never make or change one. In them the
//! scan refuses: `ResolvePermissionRequest` and `PermissionDecision`, under every name `server/src`,
//! `agent/src` and `core/src` give them, written anywhere but a `use` item or a received parameter's type; a
//! host policy or session request built by a literal, a default or a decode; a decision field (`allow`,
//! `defer`, `reason`, `decision`) assigned -- also in parentheses (after a `=>` or an attribute too), behind an
//! index or on the left of a destructuring assignment -- borrowed `&mut` or `&raw mut`, passed to
//! `mem::replace`/`take`/`swap` or given a method call that can change it; any `ref mut` binding; a received
//! value of a guarded type replaced through `*r =` (or `*(r)`, `*{r}`, `**(&mut r)`) or `mem::*`, given a
//! method call other than a read-only one (`clone`, the `encode` family, `encoded_len`, the readers a decision
//! field may use), or any of its fields written the ways a decision field may not be, plain readers such as
//! `parse` allowed on the others (so an answer is not moved onto another `permission_id`); the place ends at a
//! call's `)` and at `.await`, so `client.resolve_permission(answer).await.map_err(..)` passes; and a merge, clear or decode (`merge`, `merge_length_delimited`,
//! `merge_field`, `clear`, `decode`, `decode_length_delimited`) whose message may be a guarded one -- written
//! through a guarded type, or through the `Message` trait itself (`Message`, a `…::Message` the file imports
//! under any name, or a name the read trees give `prost::Message`), which leaves the message type unsaid,
//! whether called or taken as a function value, or called with a guarded parameter, a guarded type or a
//! decision field among its arguments. Forwarding code decodes by writing the concrete type
//! (`SessionEvent::decode`); the helper's own `decode` functions and other collections' `.clear()` pass. Their
//! site exception list is [`REMOTE_DECISION_SITES`].
//!
//! The rules on a received value match its parameter's name anywhere in the file, not only in its own
//! function: a file with a guarded parameter `answer` refuses `answer.into_inner()` everywhere in it.
//!
//! That is a text scan, and it cannot see what only the compiler knows. It does not see a value of these types that
//! type inference produces without the name being written (`Default::default()`, a generic helper's result passed
//! straight to a function taking the type, its `T::decode`, `Box::decode`, or the trait under a name only a crate
//! outside the read trees gives it); a rebinding, because the rules on a received value follow its parameter's own
//! name (`let s = &mut *r; *s = ..` passes); a write through a borrow of the whole value handed to a helper; a
//! macro's expansion, a raw pointer from `addr_of_mut!(r.allow)` included; or helpers in crates outside the trees
//! it reads. Review covers those.
//!
//! Test code is excluded: a `#[cfg(test)] mod x { … }` block, and a file reached only from test code -- through a
//! `#[cfg(test)] mod x;` declaration (which is how this file excludes itself, and `runtime_policy_verification.rs`)
//! or an `include!("…")` of a file beside or below its own inside such a block -- whose file name (`x.rs`, written
//! whole) no production text mentions, so `#[path = "x.rs"]` or an `include!` that writes the name keeps it
//! scanned. A production `mod x;` this scan resolves to the wrong path (inside an inline module, or in a non-root
//! file under `src/bin/`) and an `include!` that builds the name from pieces (`concat!` of fragments, `env!`) are
//! not seen; the whole-tree tests' exact lists of the files `agent/src`, `server/src` and the forwarding trees
//! leave out are what catch those. `core/src`, which the forwarding scan reads only for names, is not pinned.
//!
//! **Allowlist: exactly one entry**, matched by file and by the arm's own text, never by a line
//! number -- see [`ALLOWED_ARM`].
//!
//! **The CLI's own auto mode (2026-10-02).** Since Verdandi protocol 3.14 a gated sidecar session
//! may ask the CLI to run `auto` under the gate (`ClaudeHostPolicy.cli_permission_mode = AUTO`), so
//! the CLI's mode is no longer the constant `default`. It is still never ungated: the gate stays
//! installed, the session stays INTERACTIVE and unswitchable, and `bypassPermissions` stays
//! unreachable. What this scan adds (rule 6) is that AUTO can only come from the handshake: every
//! `ClaudeHostPolicy` literal states `cli_permission_mode` as UNSPECIFIED or as the one `match` on
//! the requested mode, and each place that can produce the requested `Auto` -- the requested-mode
//! constructor, the proto `Auto` constructor, a non-`false` `cli_auto_mode` capability -- is one of
//! the [`AUTO_SITES`], by file and text. A new one fails until someone adds it there, on purpose.
//!
//! The requested `Auto` is searched for under every name the crate can give it: the enum's aliases
//! (`use ... as R`, `type R = ...`, a re-export in any file), `Self::Auto` inside an `impl` of it, and
//! the variants imported by name or by glob (refused at the import, since a bare `Auto` cannot be told
//! from another type's). An assignment to a guarded field is any assignment operator, compound ones
//! included, however the `.` and the operator are spaced.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The proto enum whose `Bypass` value is the sidecar's ungated policy.
const PROTO_PERMISSION_MODE: &str = "claude_runtime_protocol::v1::PermissionMode";

/// The proto enum whose `Auto` value asks the CLI for its own auto mode under the gate.
const PROTO_CLI_PERMISSION_MODE: &str = "claude_runtime_protocol::v1::CliPermissionMode";
const PROTO_HOST_POLICY: &str = "claude_runtime_protocol::v1::ClaudeHostPolicy";
const PROTO_CREATE_SESSION_REQUEST: &str = "claude_runtime_protocol::v1::CreateSessionRequest";

/// Rule 6: every place that can make a session ask for the CLI's auto mode, as `(file, the text
/// around it)`, collapsed as the scan collapses it. Each occurrence of one of the
/// [`AUTO_NEEDLES`] must sit inside one of these, and each of these must be found exactly once: a
/// site that moved or vanished is a stale entry, and fails like a new site does.
///
/// - the capability itself, true only when the handshake offered both `cli_auto_mode` and
///   `permission_defer` (`capabilities_from_handshake`);
/// - the one constructor of the requested `Auto`, from that capability (`requested_cli_mode`);
/// - the one proto `Auto`, as the value of the `Auto` arm of the request's `match`
///   (`build_create_request_loading`);
/// - the replay seam's reading of a recorded session's settings (`testkit::claude_scripted::replay`, built only
///   with `test-support`): it names the mode a fixture's manifest says the recording session asked for, and
///   reaches no CLI;
/// - the expectation a remote tab's history is judged against when its sidecar no longer runs
///   (`HISTORY_ONLY_EXPECTATION`): it judges what the CLI reported and asks for nothing, since such a provider
///   states no capability and creates no session;
/// - two patterns and a comparison, which construct nothing: the CLI-mode classification of what the CLI
///   reported against what was asked for (`classify_reported_cli_mode`), and the translator's check
///   that an auto session never reports a mode switch.
const AUTO_SITES: &[(&str, &str)] = &[
    (
        "providers/claude_sidecar/mod.rs",
        "cli_auto_mode: CLIENT_IMPLEMENTS_CLI_AUTO_MODE && has(CAP_CLI_AUTO_MODE) && has(CAP_PERMISSION_DEFER),",
    ),
    (
        "providers/claude_sidecar/mod.rs",
        "if capabilities.cli_auto_mode { crate::RequestedCliMode::Auto } else { crate::RequestedCliMode::Default }",
    ),
    (
        "providers/claude_sidecar/mod.rs",
        "cli_permission_mode: match cli_mode { crate::RequestedCliMode::Auto => CliPermissionMode::Auto as i32, \
         crate::RequestedCliMode::Default if offers_plan_mode => CliPermissionMode::Default as i32, \
         crate::RequestedCliMode::Default => CliPermissionMode::Unspecified as i32, },",
    ),
    (
        "testkit/claude_scripted/replay.rs",
        "Some(\"auto\") => RequestedCliMode::Auto,",
    ),
    (
        "providers/claude_sidecar/mod.rs",
        "crate::CliExpectation { requested: crate::RequestedCliMode::Auto, plan_offered: true, };",
    ),
    (
        "process.rs",
        "(RequestedCliMode::Auto, \"auto\") => CliModeReport::Auto,",
    ),
    (
        "process.rs",
        "(RequestedCliMode::Auto, \"default\") => CliModeReport::AutoUnavailable,",
    ),
    (
        "providers/claude_sidecar/translate.rs",
        "if requested == RequestedCliMode::Auto {",
    ),
];

/// What rule 6 looks for. `RequestedCliMode::Auto` and `cli_auto_mode:` are this crate's own names;
/// the proto `Auto` is looked for under every name the file binds the enum to
/// (`proto_aliases`). A `cli_auto_mode:` whose value is `false` (every other backend and test
/// provider) or `bool` (the declaration) is not a site.
const AUTO_NEEDLES: &[&str] = &["RequestedCliMode::Auto", "cli_auto_mode:"];

/// The one place `<P>::Bypass` may appear: the `ProtoEvent::PermissionModeChanged` arm of the
/// sidecar translator, as a match PATTERN (`<P>::Bypass =>`).
///
/// Why it is allowed: it decodes a report the sidecar sends; never constructs a request; it is the
/// tripwire. A `PermissionModeChanged` naming BYPASS is exactly what must close the
/// session, and matching on it is how the translator notices.
const ALLOWED_ARM: (&str, &str) = (
    "providers/claude_sidecar/translate.rs",
    "ProtoEvent::PermissionModeChanged(",
);

/// Case-insensitive substrings no production source may contain (rule 1): the CLI's own name for
/// the ungated mode, and the two `--*dangerously*` flags and the SDK's
/// `allowDangerouslySkipPermissions` that enable it.
const FORBIDDEN_ANY_CASE: &[&str] = &["bypasspermissions", "dangerously", "allow_dangerously"];

/// The one production read of a word [`FORBIDDEN_ANY_CASE`] holds that is not a CLI flag or mode:
/// the Bash tool's own input key, which the permission policy reads off a call the model made so that
/// a call asking to leave the sandbox is carded. It is a key of the model's input, never something
/// Eitri passes to the CLI; allowed only as this exact literal, only in this file, and only once.
const ALLOWED_INPUT_KEY: (&str, &str) = ("permission_policy.rs", "\"dangerouslyDisableSandbox\"");

/// Exact substrings forbidden in production sources: prost setters that could build an
/// ungated policy without a struct literal, and the mid-session permission-mode switch.
const FORBIDDEN_EXACT: &[&str] = &[
    "set_permissions(",
    "set_permission_mode_switchable(",
    "set_permission_mode(",
    "SetPermissionModeRequest",
    // Rule 6: the CLI mode is stated in the one literal, never set on a policy afterwards.
    "set_cli_permission_mode(",
];

/// Whole words no production source may contain (rule 5). Each is a PRE-APPROVAL list: a tool on it
/// runs when the hook gives no answer, which is the fall-through the gate exists to close.
/// Whole words, because `disallowedTools`/`disallowed_tools` contain them and are the opposite.
const FORBIDDEN_WORDS: &[&str] = &["allowedTools", "allowed_tools"];

/// The standard library's file-mode setter, written by its full path: it shares a name with the policy's
/// `set_permissions(` setter in [`FORBIDDEN_EXACT`] and is not it (the helper sets its socket's mode).
const STD_SET_PERMISSIONS: &str = "std::fs::set_permissions(";

/// What one scan found.
#[derive(Debug, Default)]
struct Report {
    offenders: Vec<String>,
    scanned: BTreeSet<String>,
    skipped: BTreeSet<String>,
    allowlisted: usize,
    /// How many times [`ALLOWED_INPUT_KEY`] was read in its own file.
    input_key_reads: usize,
    /// How many times each [`AUTO_SITES`] entry was found, by its index.
    auto_sites_found: BTreeMap<usize, usize>,
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
pub(super) fn collapse(src: &str) -> String {
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

/// The paths the `include!("…")` calls in `code` name, as written; an `include!` inside a literal is none.
fn included_files(code: &str) -> Vec<String> {
    let blank = blank_literals(code);
    let mut paths = Vec::new();
    for (at, _) in blank.match_indices("include!") {
        if blank[..at].chars().next_back().is_some_and(is_ident) {
            continue;
        }
        let Some(rest) = code[at + "include!".len()..].trim_start().strip_prefix('(') else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('"') else {
            continue;
        };
        if let Some(end) = rest.find('"') {
            paths.push(rest[..end].to_string());
        }
    }
    paths
}

/// How production text would name the file at tree key `rel`: its file name, with its directory for a
/// `mod.rs`.
fn file_name_key(rel: &str) -> &str {
    let mut parts = rel.rsplitn(3, '/');
    let file = parts.next().unwrap_or(rel);
    match parts.next() {
        Some(dir) if file == "mod.rs" => &rel[rel.len() - dir.len() - 1 - file.len()..],
        _ => file,
    }
}

/// The tree key of the file `path` (as an `include!` names it) for the including file `rel`: beside it or
/// below it. A path that leaves the directory, or has a `.` part, is not followed, so that file stays
/// production code.
fn included_key(rel: &str, path: &str) -> Option<String> {
    if path.starts_with('/')
        || path
            .split('/')
            .any(|part| part == ".." || part == "." || part.is_empty())
    {
        return None;
    }
    Some(match rel.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{path}"),
        None => path.to_string(),
    })
}

/// Removes every `#[cfg(test)] mod x { … }` from `code` (comment-free, collapsed), and returns what
/// is left with the names of the `#[cfg(test)] mod x;` declarations it also removed and the paths the
/// removed modules `include!`.
fn strip_test_modules(code: &str) -> (String, Vec<String>, Vec<String>) {
    const MARK: &str = "#[cfg(test)]";
    let mark: Vec<char> = MARK.chars().collect();
    let c: Vec<char> = code.chars().collect();
    let mut out = String::with_capacity(code.len());
    let mut test_decls = Vec::new();
    let mut test_includes = Vec::new();
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
                    test_includes.extend(included_files(&c[k..=close].iter().collect::<String>()));
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
    (out, test_decls, test_includes)
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
    proto_aliases(code, PROTO_PERMISSION_MODE)
}

/// Every `use` item in `code`, expanded to (full path, name it binds) pairs; a glob binds `*` and
/// its path is the module it opens.
fn use_bindings(code: &str) -> Vec<(String, String)> {
    let c: Vec<char> = code.chars().collect();
    let mut bound = Vec::new();
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
        expand_use(&rest[..end], "", &mut bound);
        i += 3 + end;
    }
    bound
}

/// Every name `code` can write the proto path `path` as. See [`proto_mode_aliases`].
fn proto_aliases(code: &str, path: &str) -> Vec<String> {
    SourceNames::of(code).aliases(path)
}

/// What one source's `use` items and `type` aliases bind, read once so a search over many files and many
/// names does not lex each file again for every name.
struct SourceNames {
    /// [`use_bindings`].
    bindings: Vec<(String, String)>,
    /// Every `type <name> = <value>;`, as (name, value).
    types: Vec<(String, String)>,
}

impl SourceNames {
    fn of(code: &str) -> Self {
        let mut types = Vec::new();
        let mut rest = code;
        while let Some(at) = rest.find("type ") {
            let before = rest[..at].chars().next_back();
            let tail = &rest[at + 5..];
            if !before.is_some_and(is_ident) {
                if let Some((name, value)) = tail.split_once('=') {
                    let value = value.split(';').next().unwrap_or("").trim();
                    let name = name.trim();
                    if !name.is_empty() && name.chars().all(is_ident) {
                        types.push((name.to_string(), value.to_string()));
                    }
                }
            }
            rest = tail;
        }
        Self {
            bindings: use_bindings(code),
            types,
        }
    }

    /// Every name this source can write `path` as. See [`proto_mode_aliases`].
    fn aliases(&self, path: &str) -> Vec<String> {
        let mut aliases = vec![path.to_string()];
        for (full, name) in &self.bindings {
            if name == "*" {
                if let Some(tail) = path.strip_prefix(&format!("{full}::")) {
                    aliases.push(tail.to_string());
                }
            } else if full == path {
                aliases.push(name.clone());
            } else if let Some(tail) = path.strip_prefix(&format!("{full}::")) {
                aliases.push(format!("{name}::{tail}"));
            }
        }
        // `type P = <path>;`: matched on the last segment, so a rename-by-alias is read too.
        for (name, value) in &self.types {
            if last_segment(value) == last_segment(path) {
                aliases.push(name.clone());
            }
        }
        aliases.sort();
        aliases.dedup();
        aliases
    }
}

/// The crate's own enum that says which mode a session asked the CLI for.
const REQUESTED_MODE: &str = "RequestedCliMode";

/// What one file's `use` items and `type` aliases do to [`REQUESTED_MODE`].
#[derive(Debug, Default)]
struct RequestedModeImports {
    /// Names the enum is written as besides its own (`use ... as R`, `type R = ...`).
    aliases: BTreeSet<String>,
    /// The variants are imported as names of their own (`RequestedCliMode::*`, `{Auto}`), which no
    /// `RequestedCliMode::Auto` needle can see.
    variants_imported: bool,
    /// A glob of a module that defines or re-exports the enum (`process`, `crate`, `super`, `self`):
    /// a bare `Auto` after it is not provably some other type's.
    opens_defining_module: bool,
}

fn last_segment(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

fn requested_mode_imports(code: &str) -> RequestedModeImports {
    let mut found = RequestedModeImports::default();
    for (full, name) in use_bindings(code) {
        if name == "*" {
            if last_segment(&full) == REQUESTED_MODE {
                found.variants_imported = true;
            } else if matches!(last_segment(&full), "process" | "crate" | "super" | "self") {
                found.opens_defining_module = true;
            }
        } else if last_segment(&full) == REQUESTED_MODE {
            if name != REQUESTED_MODE {
                found.aliases.insert(name);
            }
        } else if full.ends_with(&format!("{REQUESTED_MODE}::Auto")) {
            found.variants_imported = true;
        }
    }
    // `type R = crate::RequestedCliMode;`
    let mut rest = code;
    while let Some(at) = rest.find("type ") {
        let before = rest[..at].chars().next_back();
        let tail = &rest[at + 5..];
        if !before.is_some_and(is_ident) {
            if let Some((name, value)) = tail.split_once('=') {
                let value = value.split(';').next().unwrap_or("").trim();
                let name = name.trim();
                if !name.is_empty() && name.chars().all(is_ident) && last_segment(value) == REQUESTED_MODE {
                    found.aliases.insert(name.to_string());
                }
            }
        }
        rest = tail;
    }
    found
}

/// The body spans of every `impl` targeting one of `aliases`, including trait implementations.
/// `Self` inside those bodies names that target.
fn type_impl_bodies(code: &str, aliases: &BTreeSet<String>) -> Vec<std::ops::Range<usize>> {
    let c: Vec<char> = code.chars().collect();
    let byte_at: Vec<usize> = code.char_indices().map(|(b, _)| b).chain([code.len()]).collect();
    let mut spans = Vec::new();
    for at in whole_word_positions(code, "impl") {
        let ci = code[..at].chars().count();
        let mut j = ci + 4;
        while j < c.len() && c[j] != '{' && c[j] != ';' {
            j += 1;
        }
        if c.get(j) != Some(&'{') {
            continue;
        }
        let header: String = c[ci + 4..j].iter().collect();
        let header = header.split(" where ").next().unwrap_or("").trim();
        let target = last_segment(header.rsplit(" for ").next().unwrap_or(header)).trim();
        if !aliases.contains(target) {
            continue;
        }
        if let Some(close) = matching_close(&c, j) {
            spans.push(byte_at[j]..byte_at[close]);
        }
    }
    spans
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
    // The policy is written under every name the crate gives it (`use ... as P`, `type P = ...`),
    // and a default one is refused outright: it states none of the fields below.
    let mut names = proto_aliases(code, PROTO_HOST_POLICY);
    names.push("ClaudeHostPolicy".to_string());
    names.sort();
    names.dedup();
    for name in &names {
        for (at, _) in code.match_indices(&format!("{name}::default")) {
            let before = code[..at].chars().next_back();
            if !before.is_some_and(is_ident) {
                offenders.push(format!(
                    "{rel}: `{name}::default`, a policy that states none of the gate's fields: {}",
                    snippet(code, at)
                ));
            }
        }
    }
    let positions: Vec<(usize, &str)> = names
        .iter()
        .flat_map(|name| {
            whole_word_positions(code, name)
                .into_iter()
                .map(move |at| (at, name.as_str()))
        })
        .collect();
    for (at, name) in positions {
        let i = code[..at].chars().count() + name.chars().count();
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
        let mut cli_mode = None;
        let mut setting_sources = None;
        let mut tool_policy = None;
        let mut executable = None;
        let mut configuration = None;
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
                "cli_permission_mode" => cli_mode = Some(value),
                "setting_sources" => setting_sources = Some(value),
                "tool_policy" => tool_policy = Some(value),
                "executable" => executable = Some(value),
                "configuration" => configuration = Some(value),
                _ => {}
            }
        }
        match permissions {
            Some(value) if interactive.contains(&value) => {}
            other => offenders.push(format!(
                "{rel}: a ClaudeHostPolicy literal's `permissions` is {other:?}, not \
                 `<PermissionMode>::Interactive as i32` (every session is gated): {}",
                snippet(code, at)
            )),
        }
        match switchable.as_deref() {
            Some("false") => {}
            other => offenders.push(format!(
                "{rel}: a ClaudeHostPolicy literal's `permission_mode_switchable` is {other:?}, not `false` \
                 (`true` sets --allow-dangerously-skip-permissions on the CLI): {}",
                snippet(code, at)
            )),
        }
        // Workspace trust and the CLI's identity: an absent `setting_sources` falls back to the
        // configuration, which loads every tier of an untrusted root, and an absent `tool_policy`
        // leaves the tool set to the sidecar. Both are stated, the selection through the one
        // function that knows the trust, and the CLI is the user's own on the native profile.
        match setting_sources.as_deref() {
            Some(v)
                if v.starts_with("Some(")
                    && v.contains("sources: proto_sources(crate::setting_sources::for_session(")
                    && !v.contains(".chain(") => {}
            other => offenders.push(format!(
                "{rel}: a ClaudeHostPolicy literal's `setting_sources` is {other:?}, not `Some(..)` built \
                 from `setting_sources::for_session(` (an absent selection loads every settings tier, \
                 an untrusted project's included): {}",
                snippet(code, at)
            )),
        }
        match tool_policy.as_deref() {
            Some(v) if v.starts_with("Some(") => {}
            other => offenders.push(format!(
                "{rel}: a ClaudeHostPolicy literal's `tool_policy` is {other:?}, not `Some(..)`: {}",
                snippet(code, at)
            )),
        }
        match executable.as_deref() {
            Some(v) if v.ends_with("ExecutableSource::HostCli as i32") => {}
            other => offenders.push(format!(
                "{rel}: a ClaudeHostPolicy literal's `executable` is {other:?}, not \
                 `ExecutableSource::HostCli as i32` (the user's own claude): {}",
                snippet(code, at)
            )),
        }
        match configuration.as_deref() {
            Some(v) if v.ends_with("ConfigurationProfile::Native as i32") => {}
            other => offenders.push(format!(
                "{rel}: a ClaudeHostPolicy literal's `configuration` is {other:?}, not \
                 `ConfigurationProfile::Native as i32`: {}",
                snippet(code, at)
            )),
        }
        // Rule 6: stated, never left to a default or a `..` base, and only ever UNSPECIFIED (what a
        // client before the field sends) or the request's one `match` on the mode it was asked for, whose
        // `Default` states DEFAULT exactly when the session offers plan mode (Verdandi refuses the offer
        // without a stated mode) and UNSPECIFIED otherwise.
        let cli_aliases = proto_aliases(code, PROTO_CLI_PERMISSION_MODE);
        let accepted = cli_mode.as_deref().is_some_and(|value| {
            cli_aliases.iter().any(|a| {
                value == format!("{a}::Unspecified as i32")
                    || value
                        == format!(
                            "match cli_mode {{ crate::RequestedCliMode::Auto => {a}::Auto as i32, \
                             crate::RequestedCliMode::Default if offers_plan_mode => {a}::Default as i32, \
                             crate::RequestedCliMode::Default => {a}::Unspecified as i32, }}"
                        )
            })
        });
        if !accepted {
            offenders.push(format!(
                "{rel}: a ClaudeHostPolicy literal's `cli_permission_mode` is {cli_mode:?}, not \
                 `<CliPermissionMode>::Unspecified as i32` or the request's `match cli_mode` (rule 6: the \
                 CLI's auto mode only from the handshake): {}",
                snippet(code, at)
            ));
        }
    }
}

/// Every proto `CreateSessionRequest { .. }` literal states `policy: Some(..)`: a request with no
/// policy leaves the gate, the settings tiers and the CLI to whatever the sidecar assumes, and a
/// `..` base could carry any of it.
fn check_request_literals(rel: &str, code: &str, offenders: &mut Vec<String>) {
    let c: Vec<char> = code.chars().collect();
    let names = proto_aliases(code, PROTO_CREATE_SESSION_REQUEST);
    for name in names {
        for at in whole_word_positions(code, &name) {
            let open = skip_spaces(&c, code[..at].chars().count() + name.chars().count());
            if c.get(open) != Some(&'{') {
                continue;
            }
            let before = code[..at].trim_end();
            if ["->", "impl", "for", "struct", "enum", "trait", "type", "let", "use"]
                .iter()
                .any(|kw| before.ends_with(kw))
            {
                continue;
            }
            let Some(close) = matching_close(&c, open) else {
                offenders.push(format!("{rel}: an unterminated CreateSessionRequest literal"));
                continue;
            };
            let body: String = c[open + 1..close].iter().collect();
            let mut policy = None;
            for entry in split_top_level(&body, ',') {
                let entry = entry.trim();
                if entry.starts_with("..") {
                    offenders.push(format!(
                        "{rel}: a CreateSessionRequest literal takes fields from `{entry}`: {}",
                        snippet(code, at)
                    ));
                } else if let Some((field, value)) = field_split(entry) {
                    if field == "policy" {
                        policy = Some(value);
                    }
                }
            }
            if !policy.as_deref().is_some_and(|v| v.starts_with("Some(")) {
                offenders.push(format!(
                    "{rel}: a CreateSessionRequest literal's `policy` is {policy:?}, not `Some(..)`: {}",
                    snippet(code, at)
                ));
            }
            // `Some(Default::default())` or `Some(<ClaudeHostPolicy as Default>::default())` is
            // `Some(..)` and states none of the policy's fields.
            if policy.as_deref().is_some_and(|v| v.contains("::default(")) {
                offenders.push(format!(
                    "{rel}: a CreateSessionRequest literal's `policy` is built from `default()`, which \
                     states none of the gate's fields: {}",
                    snippet(code, at)
                ));
            }
        }
    }
}

/// Rule 6's site check over one file: every occurrence of an [`AUTO_NEEDLES`] entry, and of the
/// proto `Auto` under any of its names, must lie inside one of this file's [`AUTO_SITES`]. Returns
/// how many times each of this file's sites was found, for the stale-entry check over the tree.
///
/// The requested `Auto` is looked for under every name the crate writes the enum as
/// (`crate_aliases`: the `use ... as R` and `type R = ...` of every file, since a re-export is
/// reachable from all of them, plus this file's own), as `Self::Auto` inside an `impl` of it, and as
/// a variant imported by name -- the last of which cannot be matched by text and is refused where it
/// is imported.
fn check_auto_sites(
    rel: &str,
    code: &str,
    crate_aliases: &BTreeSet<String>,
    offenders: &mut Vec<String>,
) -> BTreeMap<usize, usize> {
    let sites: Vec<(usize, &str)> = AUTO_SITES
        .iter()
        .enumerate()
        .filter(|(_, (file, _))| *file == rel)
        .map(|(i, (_, text))| (i, *text))
        .collect();
    let mut found = BTreeMap::new();
    let mut spans = Vec::new();
    for (i, text) in &sites {
        for (at, _) in code.match_indices(text) {
            spans.push(at..at + text.len());
            *found.entry(*i).or_insert(0) += 1;
        }
    }
    let proto_needles: Vec<String> = proto_aliases(code, PROTO_CLI_PERMISSION_MODE)
        .into_iter()
        .map(|a| format!("{a}::Auto"))
        .collect();
    let imports = requested_mode_imports(code);
    let mut requested_aliases = crate_aliases.clone();
    requested_aliases.extend(imports.aliases.iter().cloned());
    let requested_needles: Vec<String> = requested_aliases.iter().map(|a| format!("{a}::Auto")).collect();
    let mut needles: Vec<String> = AUTO_NEEDLES.iter().map(|n| n.to_string()).collect();
    needles.extend(proto_needles.iter().cloned());
    needles.extend(requested_needles.iter().cloned());
    let mut impl_names = requested_aliases.clone();
    impl_names.insert(REQUESTED_MODE.to_string());
    let own_impls = type_impl_bodies(code, &impl_names);
    // `Self::Auto` is the enum's variant only inside an `impl` of the enum.
    let self_auto = "Self::Auto".to_string();
    needles.push(self_auto.clone());
    for needle in &needles {
        for (at, _) in code.match_indices(needle.as_str()) {
            let before = code[..at].chars().next_back();
            let after = code[at + needle.len()..].chars().next();
            if before.is_some_and(is_ident) || (!needle.ends_with(':') && after.is_some_and(is_ident)) {
                continue;
            }
            // The tail of a longer path: the alias that path is written with is a needle of its own,
            // so the occurrence is reported once, and another crate's `…::CliPermissionMode` is not
            // this one.
            if proto_needles.contains(needle) && code[..at].ends_with("::") {
                continue;
            }
            if *needle == self_auto && !own_impls.iter().any(|body| body.contains(&at)) {
                continue;
            }
            // `a::cli_auto_mode:` is no field; `cli_auto_mode::` is a path.
            if needle.ends_with(':') && (code[..at].ends_with("::") || after == Some(':')) {
                continue;
            }
            if needle == "cli_auto_mode:" {
                let value = code[at + needle.len()..].trim_start();
                if ["false,", "false }", "false}", "bool,", "bool }", "bool}"]
                    .iter()
                    .any(|v| value.starts_with(v))
                {
                    continue;
                }
            }
            if spans.iter().any(|span| span.contains(&at)) {
                continue;
            }
            offenders.push(format!(
                "{rel}: `{needle}` outside the listed sites (rule 6: the CLI's auto mode only from the \
                 handshake; add a deliberate site to AUTO_SITES in agent/src/wire_guard.rs): {}",
                snippet(code, at)
            ));
        }
    }
    if imports.variants_imported {
        offenders.push(format!(
            "{rel}: imports the variants of `{REQUESTED_MODE}` by name (rule 6: a bare `Auto` cannot be \
             told from any other, so the requested mode is only ever written `{REQUESTED_MODE}::Auto` at \
             a listed site)"
        ));
    }
    if imports.opens_defining_module {
        for at in whole_word_positions(code, "Auto") {
            if code[..at].ends_with("::") || code[..at].ends_with('.') {
                continue;
            }
            offenders.push(format!(
                "{rel}: a bare `Auto` in a file that opens a module with a glob import (rule 6: it could \
                 be `{REQUESTED_MODE}`'s; write the enum's name): {}",
                snippet(code, at)
            ));
        }
    }
    found
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

/// Whether `rest` begins with an assignment operator: `=` (not `==`) or any compound one. `=>` and
/// the comparisons `==`, `!=`, `<=` and `>=` are not assignments; `=>` is counted anyway, as it
/// always was, because no field is ever a match pattern and a false alarm there costs nothing.
fn starts_an_assignment(rest: &str) -> bool {
    const COMPOUND: &[&str] = &["+=", "-=", "*=", "/=", "%=", "|=", "&=", "^=", "<<=", ">>="];
    COMPOUND.iter().any(|op| rest.starts_with(op)) || (rest.starts_with('=') && !rest.starts_with("=="))
}

/// Every rule, over one production file's comment-free, test-free, collapsed text.
fn check_file(
    rel: &str,
    code: &str,
    crate_aliases: &BTreeSet<String>,
    allowlisted: &mut usize,
    input_key_reads: &mut usize,
    auto_sites_found: &mut BTreeMap<usize, usize>,
    offenders: &mut Vec<String>,
) {
    for (site, n) in check_auto_sites(rel, code, crate_aliases, offenders) {
        *auto_sites_found.entry(site).or_insert(0) += n;
    }
    let lower = code.to_lowercase();
    for word in FORBIDDEN_ANY_CASE {
        for (at, _) in lower.match_indices(word) {
            let is_the_input_key = rel == ALLOWED_INPUT_KEY.0
                && at > 0
                && code
                    .get(at - 1..)
                    .is_some_and(|rest| rest.starts_with(ALLOWED_INPUT_KEY.1));
            if is_the_input_key && *input_key_reads == 0 {
                *input_key_reads += 1;
                continue;
            }
            offenders.push(format!("{rel}: `{word}` (any case): {}", snippet(code, at)));
            break;
        }
    }
    for text in FORBIDDEN_EXACT {
        // The full path from its root: `crate::std::…` or `my_std::…` is some other item.
        let std_call = |at: usize| {
            let end = at + text.len();
            STD_SET_PERMISSIONS.ends_with(text) && code[..end].ends_with(STD_SET_PERMISSIONS) && {
                let before = &code[..end - STD_SET_PERMISSIONS.len()];
                // A leading `::`, however spaced, may only follow the start of a path, never a name.
                let before = before.trim_end().strip_suffix("::").map_or(before, str::trim_end);
                !before.chars().next_back().is_some_and(|ch| is_ident(ch) || ch == ':')
            }
        };
        if let Some((at, _)) = code.match_indices(text).find(|&(at, _)| !std_call(at)) {
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
    // Assigned rather than stated in a literal: rule 3's "anywhere" half, and rule 6's. Any
    // assignment operator counts, compound ones included, and spaces around the `.` do not hide a
    // field (`policy . cli_permission_mode = 2` is the same text to the compiler).
    let tight = code.replace(" . ", ".").replace(" .", ".").replace(". ", ".");
    for field in [
        "permission_mode_switchable",
        ".permissions",
        ".cli_permission_mode",
        ".cli_auto_mode",
        // The trust and identity fields of a policy, and the request's policy itself: assigned
        // after the literal was checked, they would replace what it stated.
        ".setting_sources",
        ".tool_policy",
        ".executable",
        ".configuration",
        ".policy",
    ] {
        for (at, _) in tight.match_indices(field) {
            let after = &tight[at + field.len()..];
            let before = tight[..at].chars().next_back();
            let whole = !after.chars().next().is_some_and(is_ident)
                && (field.starts_with('.') || !before.is_some_and(is_ident));
            if whole && starts_an_assignment(after.trim_start()) {
                offenders.push(format!("{rel}: `{field}` assigned: {}", snippet(&tight, at)));
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
                "{rel}: `{needle}` -- the sidecar's ungated policy; the only allowed use is the \
                 `PermissionModeChanged` decode arm in {}: {}",
                ALLOWED_ARM.0,
                snippet(code, at)
            ));
        }
    }
    check_host_policy_literals(rel, code, &aliases, offenders);
    check_request_literals(rel, code, offenders);
}

/// Every source of a tree as the scans read it -- comment-free, collapsed, test modules removed --
/// and, apart, the files reached only through a `#[cfg(test)] mod x;` declaration.
fn clean_tree(files: &BTreeMap<String, String>) -> (BTreeMap<String, String>, BTreeSet<String>) {
    let mut cleaned = BTreeMap::new();
    let mut test_only = BTreeSet::new();
    let mut reached = BTreeSet::new();
    for (rel, src) in files {
        let (code, test_decls, test_includes) = strip_test_modules(&collapse(&strip_comments(src)));
        for name in test_decls {
            test_only.extend(module_candidates(rel, &name));
        }
        test_only.extend(test_includes.iter().filter_map(|path| included_key(rel, path)));
        for name in module_declarations(&code) {
            reached.extend(module_candidates(rel, &name));
        }
        reached.extend(included_files(&code).iter().filter_map(|path| included_key(rel, path)));
        cleaned.insert(rel.clone(), code);
    }
    let mut test_files: BTreeSet<String> = test_only.difference(&reached).cloned().collect();
    // A test-only file is one no production text names: production may reach it in ways the two lists above do
    // not follow (`#[path = "x.rs"]`, `include!("./x.rs")`, `concat!`, a raw string, other delimiters), so any
    // mention of its file name outside test code makes it production, and so on until nothing changes.
    loop {
        let named: BTreeSet<String> = test_files
            .iter()
            .filter(|candidate| {
                let name = file_name_key(candidate);
                cleaned
                    .iter()
                    .any(|(rel, code)| !test_files.contains(rel) && code.contains(name))
            })
            .cloned()
            .collect();
        if named.is_empty() {
            break;
        }
        test_files.retain(|rel| !named.contains(rel));
    }
    let production = cleaned
        .into_iter()
        .filter(|(rel, _)| !test_files.contains(rel))
        .collect();
    (production, test_files)
}

/// This crate's production code under `root`, keyed by the path relative to it, as the scans read
/// it: no comments, whitespace collapsed (so a line break never decides a verdict), no test
/// modules, and no file that only a test module declares or `include!`s. For scans living in other modules.
pub(super) fn production_code(root: &Path) -> BTreeMap<String, String> {
    clean_tree(&read_tree(root)).0
}

/// `code` (as [`production_code`] gives it) with the body of the one `fn <name>` cut out. An error
/// when there is no such function or more than one, so an exemption cannot outlive its function
/// or cover a second one.
pub(super) fn without_fn_body(code: &str, name: &str) -> Result<String, String> {
    let (c, open, close) = fn_body_span(code, name)?;
    let before: String = c[..open].iter().collect();
    let after: String = c[close + 1..].iter().collect();
    Ok(format!("{before}{{}}{after}"))
}

/// The body of the one `fn <name>` in `code` (as [`production_code`] gives it), its braces included. An error when
/// there is no such function or more than one, so a scan of one function cannot read another.
pub(super) fn fn_body(code: &str, name: &str) -> Result<String, String> {
    let (c, open, close) = fn_body_span(code, name)?;
    Ok(c[open..=close].iter().collect())
}

/// `code` as chars, and the char offsets of the opening and closing brace of the one `fn <name>`'s body.
fn fn_body_span(code: &str, name: &str) -> Result<(Vec<char>, usize, usize), String> {
    let needle = format!("fn {name}(");
    let starts: Vec<usize> = code.match_indices(&needle).map(|(at, _)| at).collect();
    let [start] = starts[..] else {
        return Err(format!("expected exactly one `{needle}`, found {}", starts.len()));
    };
    let open = code[start..]
        .find('{')
        .map(|at| start + at)
        .ok_or_else(|| format!("`{needle}` has no body"))?;
    let c: Vec<char> = code.chars().collect();
    // `matching_close` counts in chars; the byte offsets above are turned into char offsets first.
    let open_char = code[..open].chars().count();
    let close_char = matching_close(&c, open_char).ok_or_else(|| format!("`{needle}`'s body is not closed"))?;
    Ok((c, open_char, close_char))
}

/// Scans a tree of sources keyed by their path relative to `src/` (`/`-separated).
fn scan_tree(files: &BTreeMap<String, String>) -> Report {
    let mut report = Report::default();
    let (cleaned, test_files) = clean_tree(files);
    report.skipped = test_files.into_iter().filter(|rel| files.contains_key(rel)).collect();
    // The names the enum is written as anywhere in the crate: a re-export is reachable from every
    // file, not only the one that writes it.
    let crate_aliases: BTreeSet<String> = cleaned
        .values()
        .flat_map(|code| requested_mode_imports(code).aliases)
        .collect();
    for (rel, code) in &cleaned {
        report.scanned.insert(rel.clone());
        check_file(
            rel,
            code,
            &crate_aliases,
            &mut report.allowlisted,
            &mut report.input_key_reads,
            &mut report.auto_sites_found,
            &mut report.offenders,
        );
    }
    report
}

// Forwarding layers may carry policy bytes, but must never manufacture a person's decisions.
// Exceptions name one reviewed site by file and complete text; stale sites fail too.
const REMOTE_DECISION_SITES: &[(&str, &str)] = &[];

/// The fields that hold a person's answer: the wire request's `allow`, `defer` and `reason`, and the agent
/// request's `decision`.
const DECISION_FIELDS: [&str; 4] = [".allow", ".defer", ".reason", ".decision"];

/// The methods forwarding code may call on a decision field: each only reads it.
const READ_ONLY_FIELD_METHODS: &[&str] = &[
    "allows",
    "as_deref",
    "as_ref",
    "as_str",
    "bytes",
    "chars",
    "clone",
    "contains",
    "defers",
    "ends_with",
    "eq",
    "is_empty",
    "is_none",
    "is_some",
    "iter",
    "len",
    "ne",
    "reason",
    "starts_with",
    "to_owned",
    "to_string",
];

/// The methods forwarding code may call on a received value of a guarded type, beside the
/// [`READ_ONLY_FIELD_METHODS`]: each only reads it.
const READ_ONLY_VALUE_METHODS: &[&str] = &[
    "encode",
    "encode_length_delimited",
    "encode_length_delimited_to_vec",
    "encode_to_vec",
    "encoded_len",
];

/// The methods forwarding code may call on a received value's fields other than the decision fields, beside the
/// [`READ_ONLY_FIELD_METHODS`]: each only reads the field.
const READ_ONLY_OTHER_FIELD_METHODS: &[&str] = &[
    "as_bytes",
    "cmp",
    "parse",
    "partial_cmp",
    "split",
    "to_ascii_lowercase",
    "to_lowercase",
];

/// `code` with every string, raw string and char literal turned into spaces, byte for byte: a position in one
/// is the same position in the other, and no literal's text can look like a keyword.
pub(super) fn blank_literals(code: &str) -> String {
    let c: Vec<char> = code.chars().collect();
    let mut out = String::with_capacity(code.len());
    let mut i = 0;
    while i < c.len() {
        if let Some(end) = literal_end(&c, i) {
            for ch in &c[i..end] {
                out.extend(std::iter::repeat_n(' ', ch.len_utf8()));
            }
            i = end;
            continue;
        }
        out.push(c[i]);
        i += 1;
    }
    out
}

/// Whether `text` ends with the whole word `word`.
fn ends_with_word(text: &str, word: &str) -> bool {
    text.strip_suffix(word)
        .is_some_and(|before| !before.chars().next_back().is_some_and(is_ident))
}

/// The span of every real `use` item in `blank` (as [`blank_literals`] gives it), from the keyword to its `;`.
/// The keyword must start an item -- at the start, or after `;`, `{`, `}`, an attribute's `]`, `pub` or
/// `pub(..)` -- and be followed by nothing but a path tree. So a `use` in an expression, a raw identifier
/// `r#use` or a literal's text is no import.
pub(super) fn use_item_spans(blank: &str) -> Vec<std::ops::Range<usize>> {
    whole_word_positions(blank, "use")
        .into_iter()
        .filter_map(|at| {
            let before = blank[..at].trim_end();
            let restricted = before.strip_suffix(')').and_then(|inner| {
                let open = inner.rfind("pub(")?;
                (ends_with_word(&inner[..open + 3], "pub")
                    && inner[open + 4..]
                        .chars()
                        .all(|ch| is_ident(ch) || ch == ' ' || ch == ':'))
                .then_some(())
            });
            let starts_an_item = before.is_empty()
                || before.ends_with([';', '{', '}', ']'])
                || ends_with_word(before, "pub")
                || restricted.is_some();
            if !starts_an_item || !matches!(blank[at + 3..].chars().next(), Some(' ' | ':' | '{')) {
                return None;
            }
            let end = at + blank[at..].find(';')?;
            blank[at + 3..end]
                .chars()
                .all(|ch| is_ident(ch) || matches!(ch, ' ' | ':' | '{' | '}' | ',' | '*'))
                .then_some(at..end)
        })
        .collect()
}

/// The only type positions a forwarding layer needs, in `blank` (as [`blank_literals`] gives it): the spans of
/// its real `use` items, and each function parameter's name with the span of its type when that type, after
/// `&`, a lifetime and `mut`, is a plain path. The parameter's span ends at its type's last segment; the caller
/// requires a guarded name to end there, so `Name::Variant` is not a parameter of type `Name`.
#[allow(clippy::type_complexity)]
fn forwarding_type_positions(blank: &str) -> (Vec<std::ops::Range<usize>>, Vec<(String, std::ops::Range<usize>)>) {
    let code = blank;
    let imports = use_item_spans(blank);
    let mut parameters = Vec::new();
    for at in whole_word_positions(code, "fn") {
        let header = code[at + 2..].trim_start();
        // A function pointer's arguments are not received values in a forwarding function.
        if !header.chars().next().is_some_and(is_ident) {
            continue;
        }
        let start = code.len() - header.len();
        let mut depth = 0i32;
        let mut open = None;
        for (offset, ch) in header.char_indices() {
            match ch {
                '<' => depth += 1,
                '>' if !header[..offset].ends_with('-') => depth -= 1,
                '(' if depth == 0 => {
                    open = Some(start + offset);
                    break;
                }
                '{' | ';' if depth == 0 => break,
                _ => {}
            }
        }
        let Some(open) = open else { continue };
        let chars: Vec<char> = code[open..].chars().collect();
        let Some(close) = matching_close(&chars, 0) else {
            continue;
        };
        let end = open + chars[..close].iter().map(|ch| ch.len_utf8()).sum::<usize>();
        let arguments = &code[open + 1..end];
        let mut depth = 0i32;
        let mut from = 0;
        for (offset, ch) in arguments.char_indices().chain([(arguments.len(), ',')]) {
            match ch {
                '(' | '[' | '{' | '<' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                '>' if !arguments[..offset].ends_with('-') => depth -= 1,
                ',' if depth == 0 => {
                    let argument = &arguments[from..offset];
                    if let Some((pattern, ty)) = argument.split_once(':') {
                        let pattern = pattern.trim();
                        let name = pattern.strip_prefix("mut ").unwrap_or(pattern).trim();
                        let mut ty = ty.trim();
                        if let Some(reference) = ty.strip_prefix('&') {
                            ty = reference.trim_start();
                            if ty.starts_with('\'') {
                                ty = ty.trim_start_matches('\'').trim_start_matches(is_ident).trim_start();
                            }
                            if let Some(rest) = ty.strip_prefix("mut ") {
                                ty = rest.trim_start();
                            }
                        }
                        if !ty.is_empty()
                            && ty.trim_start_matches("::").split("::").all(|part| {
                                let part = part.trim();
                                !part.is_empty() && part.chars().all(is_ident)
                            })
                        {
                            let start = ty.as_ptr() as usize - code.as_ptr() as usize;
                            parameters.push((name.to_string(), start..start + ty.len()));
                        }
                    }
                    from = offset + 1;
                }
                _ => {}
            }
        }
    }
    (imports, parameters)
}

/// Every name each guarded path can be written as anywhere in `sources`: the bare name, its aliases (`use ..
/// as`, `type`, re-exports), and theirs in turn, until no source adds one.
fn guarded_names(sources: &[SourceNames], path: &str) -> BTreeSet<String> {
    let bare = last_segment(path);
    let mut names: BTreeSet<String> = sources
        .iter()
        .flat_map(|source| source.aliases(path))
        .chain([bare.to_string()])
        .collect();
    for source in sources {
        for (full, alias) in &source.bindings {
            if last_segment(full) == bare {
                names.insert(alias.clone());
            }
        }
    }
    loop {
        let mut expanded = names.clone();
        for source in sources {
            for name in &names {
                expanded.extend(source.aliases(name));
            }
            for (full, alias) in &source.bindings {
                if names.iter().any(|name| last_segment(name) == last_segment(full)) {
                    expanded.insert(alias.clone());
                }
            }
        }
        if expanded == names {
            return names;
        }
        names = expanded;
    }
}

/// The words before a `(` that open a parenthesized expression rather than a call's arguments.
const KEYWORDS_BEFORE_GROUP: &[&str] = &["break", "else", "if", "in", "match", "mut", "return", "while"];

/// The position in `tight` of the bracket that the `)`, `]` or `}` at `close` closes.
fn matching_open(tight: &str, close: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (at, ch) in tight[..=close].char_indices().rev() {
        match ch {
            ')' | ']' | '}' => depth += 1,
            '(' | '[' | '{' => {
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

/// Whether the `(` at `open` in `tight` starts a call's arguments: it follows a name that is not a keyword, a
/// turbofish's closing `>` (not `=>`, `->` or a comparison), another call's `)`, or an index's `]` (not an
/// attribute's).
fn opens_call_arguments(tight: &str, open: usize) -> bool {
    let before = &tight[..open];
    let word = &before[before.trim_end_matches(is_ident).len()..];
    if !word.is_empty() {
        return !KEYWORDS_BEFORE_GROUP.contains(&word);
    }
    if before.ends_with(')') {
        return true;
    }
    if before.ends_with(']') {
        return matching_open(tight, open - 1).is_some_and(|at| !tight[..at].trim_end_matches('!').ends_with('#'));
    }
    before.ends_with('>') && turbofish_open(before).is_some()
}

/// The position of the `<` of the `::<..>` turbofish that the `>` ending `before` closes, if it closes one.
fn turbofish_open(before: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (at, ch) in before.char_indices().rev() {
        match ch {
            '>' if before[..at].ends_with(['-', '=']) => return None,
            '>' => depth += 1,
            '<' => {
                depth -= 1;
                if depth == 0 {
                    return before[..at].ends_with("::").then_some(at);
                }
            }
            '(' | ')' | '{' | '}' | ';' => return None,
            _ => {}
        }
    }
    None
}

/// `rest`, the text of `tight` after a place that starts at `start`, with every `)` that closes parentheses
/// around the place and every `[..]` index after it skipped: `(r.allow) =` and `r.reason[..].f()` are judged
/// by what follows them. A `)` that ends a call's arguments ends the place.
fn after_place<'a>(spelled: &Tightened, start: usize, mut rest: &'a str) -> &'a str {
    let tight = spelled.tight.as_str();
    loop {
        let at = tight.len() - rest.len();
        if rest.starts_with(')') {
            match matching_open(tight, at) {
                Some(open) if open < start && !spelled.opens_call_arguments(open) => rest = &rest[1..],
                _ => return rest,
            }
        } else if rest.starts_with('[') {
            let chars: Vec<char> = rest.chars().collect();
            let Some(close) = matching_close(&chars, 0) else {
                return rest;
            };
            rest = &rest[chars[..=close].iter().map(|ch| ch.len_utf8()).sum::<usize>()..];
        } else {
            return rest;
        }
    }
}

/// `rest`, the text of `tight` after a received value's name at `start`, with every `)` that closes parentheses
/// around it (not a call's arguments: `send(answer).unwrap()` ends the place at its `)`) and every `}` of a
/// `*{r}` or `({r})` block skipped. Whether a `(` opens a call is judged by the word before it as `spelled` gives
/// it, whitespace kept ([`Tightened::opens_call_arguments`]).
fn after_value<'a>(spelled: &Tightened, start: usize, mut rest: &'a str) -> &'a str {
    let tight = spelled.tight.as_str();
    loop {
        let at = tight.len() - rest.len();
        let skip = match rest.chars().next() {
            Some(')') => {
                matching_open(tight, at).is_some_and(|open| open < start && !spelled.opens_call_arguments(open))
            }
            Some('}') => {
                matching_open(tight, at).is_some_and(|open| open < start && tight[..open].ends_with(['*', '(']))
            }
            _ => false,
        };
        if !skip {
            return rest;
        }
        rest = &rest[1..];
    }
}

/// `blank` (literals blanked) without whitespace and with each raw identifier spelled plainly, and where in
/// `blank` each of its bytes came from: whitespace removal joins words (`for _ in (r)` reads `for_in(r)`), so a
/// question about a word is asked of `blank`.
struct Tightened<'a> {
    blank: &'a str,
    tight: String,
    from: Vec<usize>,
}

impl<'a> Tightened<'a> {
    fn of(blank: &'a str) -> Tightened<'a> {
        let mut spaced = String::with_capacity(blank.len());
        let mut spaced_from = Vec::with_capacity(blank.len());
        for (at, ch) in blank.char_indices().filter(|(_, ch)| !ch.is_whitespace()) {
            spaced.push(ch);
            spaced_from.extend(std::iter::repeat_n(at, ch.len_utf8()));
        }
        let mut tight = String::with_capacity(spaced.len());
        let mut from = Vec::with_capacity(spaced.len());
        let mut at = 0;
        while at < spaced.len() {
            if spaced[at..].starts_with("r#") {
                at += 2;
                continue;
            }
            let ch = spaced[at..].chars().next().expect("at a char boundary");
            tight.push(ch);
            from.extend_from_slice(&spaced_from[at..at + ch.len_utf8()]);
            at += ch.len_utf8();
        }
        Tightened { blank, tight, from }
    }

    /// [`opens_call_arguments`] for the `(` at `open` in `tight`, with the word before it read from `blank`.
    fn opens_call_arguments(&self, open: usize) -> bool {
        let before = self.blank[..self.from[open]].trim_end();
        let word = &before[before.trim_end_matches(is_ident).len()..];
        if !word.is_empty() {
            return !KEYWORDS_BEFORE_GROUP.contains(&word);
        }
        opens_call_arguments(&self.tight, open)
    }
}

/// The writes to a decision field in `tight` (whitespace-free, raw identifiers spelled plainly, literals
/// gone): an assignment to it or to anything inside it, a method call on it that is not one of the
/// [`READ_ONLY_FIELD_METHODS`], and a `&mut` or `&raw mut` of it -- each also with parentheses around the
/// place or an index after it.
fn decision_field_writes(rel: &str, spelled: &Tightened, offenders: &mut Vec<String>) {
    let tight = spelled.tight.as_str();
    for field in DECISION_FIELDS {
        for (at, _) in tight.match_indices(field) {
            let after = &tight[at + field.len()..];
            if after.chars().next().is_some_and(is_ident) {
                continue;
            }
            if let Some(write) = place_write(spelled, at, after, &[]) {
                offenders.push(format!(
                    "{rel}: forwarding code {write} {field}: {}",
                    snippet(tight, at)
                ));
            }
        }
    }
}

/// How the field written at `at` in `tight` (its `.`), followed by `after`, is changed, if it is: assigned
/// (itself or anything inside it), given a method call that is not one of the [`READ_ONLY_FIELD_METHODS`] or
/// `readers`, or borrowed `&mut`/`&raw mut` -- each also with parentheses around the place or an index after it.
fn place_write(spelled: &Tightened, at: usize, after: &str, readers: &[&str]) -> Option<&'static str> {
    let tight = spelled.tight.as_str();
    let mut tail = after_place(spelled, at, after);
    let mut write = None;
    loop {
        if starts_an_assignment(tail) {
            write = Some("assigns");
            break;
        }
        let Some(rest) = tail.strip_prefix('.') else { break };
        let (name, next) = rest.split_at(rest.find(|ch: char| !is_ident(ch)).unwrap_or(rest.len()));
        if name.is_empty() {
            break;
        }
        if next.starts_with('(') || next.starts_with("::<") {
            if !READ_ONLY_FIELD_METHODS.contains(&name) && !readers.contains(&name) {
                write = Some("calls a method that can change");
            }
            break;
        }
        tail = after_place(spelled, at, next);
    }
    let receiver_start = tight[..at]
        .trim_end_matches(|ch: char| is_ident(ch) || matches!(ch, '.' | '(' | ')' | '[' | ']' | '*'))
        .len();
    let receiver = &tight[receiver_start..at];
    if tight[..receiver_start].ends_with('&') && (receiver.starts_with("mut") || receiver.starts_with("rawmut")) {
        write = write.or(Some("borrows mutably"));
    }
    write
}

/// Whether `text` holds a decision field as a whole name.
fn names_a_decision_field(text: &str) -> bool {
    DECISION_FIELDS.iter().any(|field| {
        text.match_indices(field)
            .any(|(at, _)| !text[at + field.len()..].chars().next().is_some_and(is_ident))
    })
}

/// Destructuring assignments in `tight` (as [`decision_field_writes`] reads it) with a decision field on the
/// left: `(r.allow, x) = ..`, `[r.allow, _] = ..`, `S { a: r.allow } = ..`. A pattern (after `let`, in a
/// `match` arm) cannot hold a field access, so a bracketed group holding one right before a plain `=` is an
/// assignee; a group after a name or a closing bracket that opens with `[` is an index, whose place is judged
/// as a place.
fn destructuring_writes(rel: &str, tight: &str, offenders: &mut Vec<String>) {
    for (at, _) in tight.match_indices('=') {
        let before = &tight[..at];
        if !before.ends_with([')', ']', '}']) || matches!(tight[at + 1..].chars().next(), Some('=' | '>')) {
            continue;
        }
        let Some(open) = matching_open(tight, at - 1) else {
            continue;
        };
        let index = tight[open..].starts_with('[')
            && tight[..open]
                .chars()
                .next_back()
                .is_some_and(|ch| is_ident(ch) || matches!(ch, ')' | ']' | '>'));
        if !index && names_a_decision_field(&tight[open..at]) {
            offenders.push(format!(
                "{rel}: forwarding code assigns a decision field by destructuring: {}",
                snippet(tight, open)
            ));
        }
    }
}

/// The calls that merge into, clear or decode a message, by name.
const MESSAGE_REWRITES: [(&str, &str); 6] = [
    ("merge", "merges into"),
    ("merge_length_delimited", "merges into"),
    ("merge_field", "merges into"),
    ("decode", "decodes"),
    ("decode_length_delimited", "decodes"),
    ("clear", "clears"),
];

/// The type a `Type::` or `<Type as Trait>::` qualifier ending `path` (spaces allowed) names: its last
/// segment, `_` for an inferred one, empty when there is none.
fn qualifier_type(path: &str) -> &str {
    let path = path.trim_end();
    let Some(inner) = path.strip_suffix('>') else {
        return &path[path.trim_end_matches(is_ident).len()..];
    };
    let mut depth = 0usize;
    for (at, ch) in inner.char_indices().rev() {
        match ch {
            '>' => depth += 1,
            '<' if depth == 0 => {
                let ty = inner[at + 1..].split(" as ").next().unwrap_or("").trim();
                return last_segment(ty).trim();
            }
            '<' => depth -= 1,
            _ => {}
        }
    }
    ""
}

/// Merges, clears and decodes in `plain` (as [`scan_remote_decisions`] gives it) whose message may be a
/// guarded one: one written through a guarded type (`R::decode`, `<R as Message>::merge`), one through the
/// `Message` trait itself (`Message`, a name this file's `use` items give a `…::Message`, a name the read
/// trees give `prost::Message`, or `<_ as ..>`), whose message type the text does not say, called or taken as
/// a function value, and any call whose arguments name a guarded parameter, a guarded
/// type or a decision field. A method call (`.clear()`) is judged by its receiver, through the field and
/// received-value rules; a function's own definition (`fn decode`) is none. So the helper's own decoders,
/// collection clears and protobuf reads of other messages (`SessionEvent::decode`) pass.
fn message_rewrites(
    rel: &str,
    code: &str,
    plain: &str,
    guarded_parameters: &BTreeSet<&str>,
    every_name: &BTreeSet<&str>,
    message_trait: &BTreeSet<String>,
    offenders: &mut Vec<String>,
) {
    let mut traits: BTreeSet<String> = message_trait
        .iter()
        .map(|name| last_segment(name).to_string())
        .collect();
    traits.insert("Message".to_string());
    for (full, alias) in use_bindings(plain) {
        if last_segment(&full) == "Message" && alias != "*" {
            traits.insert(alias);
        }
    }
    for (word, what) in MESSAGE_REWRITES {
        for at in whole_word_positions(plain, word) {
            let before = plain[..at].trim_end();
            if before.ends_with('.') || ends_with_word(before, "fn") {
                continue;
            }
            let through = before.strip_suffix("::").map(qualifier_type);
            let unsaid = through.is_some_and(|ty| ty.is_empty() || ty == "_" || traits.contains(ty));
            let guarded_type = through.is_some_and(|ty| every_name.contains(ty));
            // The arguments, past a turbofish and the parentheses of `(M::merge)(..)`.
            let mut rest = plain[at + word.len()..].trim_start();
            if let Some(generic) = rest.strip_prefix("::") {
                let generic = generic.trim_start();
                if generic.starts_with('<') {
                    let chars: Vec<char> = generic.chars().collect();
                    let mut depth = 0usize;
                    let close = chars.iter().position(|&ch| {
                        depth = match ch {
                            '<' => depth + 1,
                            '>' => depth.saturating_sub(1),
                            _ => depth,
                        };
                        ch == '>' && depth == 0
                    });
                    if let Some(close) = close {
                        rest = generic[chars[..=close].iter().map(|ch| ch.len_utf8()).sum::<usize>()..].trim_start();
                    }
                }
            }
            let rest = rest.trim_start_matches([')', ' ']);
            let names_a_guarded_argument = rest.starts_with('(') && {
                let chars: Vec<char> = rest.chars().collect();
                matching_close(&chars, 0).is_some_and(|close| {
                    let arguments: String = chars[1..close].iter().collect();
                    let tight: String = arguments.chars().filter(|ch| !ch.is_whitespace()).collect();
                    names_a_decision_field(&tight)
                        || guarded_parameters
                            .iter()
                            .chain(every_name)
                            .any(|name| !whole_word_positions(&arguments, name).is_empty())
                })
            };
            if unsaid || guarded_type || names_a_guarded_argument {
                offenders.push(format!(
                    "{rel}: forwarding code {what} a message that may be a guarded one: {}",
                    snippet(code, at)
                ));
            }
        }
    }
}

/// `std::mem::replace`, `take` and `swap` calls in `blank` (as [`blank_literals`] gives it) whose arguments
/// name a decision field, a guarded parameter or a guarded type, under any `use` of `mem` or of the function.
fn mem_rewrites(
    rel: &str,
    code: &str,
    blank: &str,
    guarded_parameters: &BTreeSet<&str>,
    every_name: &BTreeSet<&str>,
    offenders: &mut Vec<String>,
) {
    const FUNCTIONS: [&str; 3] = ["replace", "take", "swap"];
    let mut calls: BTreeSet<String> = FUNCTIONS.iter().map(|f| format!("mem::{f}")).collect();
    for (full, alias) in use_bindings(blank) {
        if last_segment(&full) == "mem" {
            for f in FUNCTIONS {
                calls.insert(if alias == "*" {
                    f.to_string()
                } else {
                    format!("{alias}::{f}")
                });
            }
        } else if FUNCTIONS.iter().any(|f| full.ends_with(&format!("mem::{f}"))) && alias != "*" {
            calls.insert(alias);
        }
    }
    for call in &calls {
        for (at, _) in blank.match_indices(call.as_str()) {
            let before = blank[..at].chars().next_back();
            let rest = &blank[at + call.len()..];
            if before.is_some_and(is_ident) || rest.chars().next().is_some_and(is_ident) {
                continue;
            }
            let Some(open) = rest.find('(').filter(|open| !rest[..*open].contains(';')) else {
                continue;
            };
            let open = at + call.len() + open;
            let chars: Vec<char> = blank[open..].chars().collect();
            let Some(close) = matching_close(&chars, 0) else {
                continue;
            };
            let end = open + chars[..close].iter().map(|ch| ch.len_utf8()).sum::<usize>();
            let arguments = code[open + 1..end].replace("r#", "");
            let tight: String = arguments.chars().filter(|ch| !ch.is_whitespace()).collect();
            let names_a_field = names_a_decision_field(&tight);
            let names_a_value = guarded_parameters
                .iter()
                .chain(every_name)
                .any(|name| !whole_word_positions(&arguments, name).is_empty());
            if names_a_field || names_a_value {
                offenders.push(format!(
                    "{rel}: forwarding code replaces a decision with {call}: {}",
                    snippet(code, at)
                ));
            }
        }
    }
}

/// The forwarding trees' rules (see the module doc for what they catch and what they cannot), over `files`
/// keyed by workspace-relative path. Every file is read for the guarded types' names; only those under
/// `server/src/` and `agent/src/providers/claude_sidecar/transport/` are judged.
fn scan_remote_decisions(files: &BTreeMap<String, String>) -> Vec<String> {
    scan_remote_decisions_and_skips(files).0
}

/// [`scan_remote_decisions`], with the files it left out as test code.
fn scan_remote_decisions_and_skips(files: &BTreeMap<String, String>) -> (Vec<String>, BTreeSet<String>) {
    let (cleaned, test_files) = clean_tree(files);
    let guarded = [
        PROTO_HOST_POLICY,
        PROTO_CREATE_SESSION_REQUEST,
        "claude_runtime_protocol::v1::ResumeSessionRequest",
        "claude_runtime_protocol::v1::ResolvePermissionRequest",
        "agent::PermissionDecision",
        "claude_runtime_protocol::v1::PermissionDecision",
    ];
    // Read once for every forwarding file: a name given anywhere in the trees read reaches these ones.
    let sources: Vec<SourceNames> = cleaned.values().map(|code| SourceNames::of(code)).collect();
    let names_of: Vec<BTreeSet<String>> = guarded.iter().map(|path| guarded_names(&sources, path)).collect();
    let every_name: BTreeSet<&str> = names_of
        .iter()
        .flatten()
        .map(|name| last_segment(name))
        .filter(|name| name.chars().all(is_ident))
        .collect();
    // The names the read trees give the `Message` trait itself (`pub use prost::Message as Wire;`).
    let message_trait = guarded_names(&sources, "prost::Message");
    let mut offenders = Vec::new();
    let mut found = vec![0; REMOTE_DECISION_SITES.len()];
    for (rel, original) in &cleaned {
        if !rel.starts_with("server/src/") && !rel.starts_with("agent/src/providers/claude_sidecar/transport/") {
            continue;
        }
        let mut code = original.clone();
        for (index, (file, text)) in REMOTE_DECISION_SITES.iter().enumerate() {
            if rel == file {
                let text = collapse(text);
                found[index] += code.matches(&text).count();
                code = code.replace(&text, " ");
            }
        }
        let blank = blank_literals(&code);
        let (imports, parameters) = forwarding_type_positions(&blank);
        // The received parameters whose type is a guarded one, by name.
        let guarded_parameters: BTreeSet<&str> = parameters
            .iter()
            .filter(|(_, ty)| every_name.contains(last_segment(&code[ty.clone()])))
            // `r#decision` is `decision` to the compiler, and to `tight` below.
            .map(|(name, _)| name.strip_prefix("r#").unwrap_or(name))
            .filter(|name| !name.is_empty() && name.chars().all(is_ident))
            .collect();
        // Whitespace-free, literals gone, raw identifiers spelled plainly: `r . r#allow` is `r.allow` to the
        // compiler, and a literal's text is no place.
        let spelled = Tightened::of(&blank);
        let tight = spelled.tight.clone();
        // Literals blanked and each `r#` turned into spaces, so a raw identifier is a plain word at its place.
        let plain = blank.replace("r#", "  ");
        decision_field_writes(rel, &spelled, &mut offenders);
        destructuring_writes(rel, &tight, &mut offenders);
        // A `ref mut` binding writes through to the place it binds, a decision field included; forwarding code
        // never needs one.
        for at in whole_word_positions(&plain, "ref") {
            let rest = plain[at + 3..].trim_start();
            if rest.starts_with("mut") && !rest[3..].chars().next().is_some_and(is_ident) {
                offenders.push(format!(
                    "{rel}: forwarding code binds by `ref mut`: {}",
                    snippet(&code, at)
                ));
            }
        }
        message_rewrites(
            rel,
            &code,
            &plain,
            &guarded_parameters,
            &every_name,
            &message_trait,
            &mut offenders,
        );
        mem_rewrites(rel, &code, &blank, &guarded_parameters, &every_name, &mut offenders);
        // A received value of a guarded type, by its parameter's name anywhere in the file: `*r = ..` (also
        // `*(r)`, `*{r}`, `**(&mut r)`) replaces it whole, a method call on it may only read it
        // (`r.clone_from(earlier)` replaces it too), and every field of it is guarded like a decision field, so an
        // answer is not moved onto another `permission_id`.
        for name in &guarded_parameters {
            for (at, _) in tight.match_indices(name) {
                let rest = &tight[at + name.len()..];
                let head = &tight[..at];
                // `&mut r` reads `&mutr` once whitespace is gone.
                let borrowed = ["&mut", "&rawmut"].iter().find_map(|prefix| head.strip_suffix(prefix));
                let before = head.chars().next_back();
                if rest.chars().next().is_some_and(is_ident)
                    || (borrowed.is_none() && before.is_some_and(|ch| is_ident(ch) || ch == '.'))
                {
                    continue;
                }
                let rest = after_value(&spelled, at, rest);
                let head = borrowed.or_else(|| head.strip_suffix('&')).unwrap_or(head);
                if head.trim_end_matches(['(', '{']).ends_with('*') && starts_an_assignment(rest) {
                    offenders.push(format!(
                        "{rel}: forwarding code replaces the received {name} whole: {}",
                        snippet(&tight, at)
                    ));
                }
                let Some(call) = rest.strip_prefix('.') else { continue };
                let (member, next) = call.split_at(call.find(|ch: char| !is_ident(ch)).unwrap_or(call.len()));
                // `answer.await` is the end of the place, not a field.
                if member.is_empty() || member == "await" {
                    continue;
                }
                if next.starts_with('(') || next.starts_with("::<") {
                    if !READ_ONLY_FIELD_METHODS.contains(&member) && !READ_ONLY_VALUE_METHODS.contains(&member) {
                        offenders.push(format!(
                            "{rel}: forwarding code calls a method that can change the received {name}: {}",
                            snippet(&tight, at)
                        ));
                    }
                } else if let Some(write) =
                    place_write(&spelled, tight.len() - rest.len(), next, READ_ONLY_OTHER_FIELD_METHODS)
                {
                    offenders.push(format!(
                        "{rel}: forwarding code {write} the received {name}'s .{member}: {}",
                        snippet(&tight, at)
                    ));
                }
            }
        }
        for (path, names) in guarded.iter().zip(&names_of) {
            let bare = last_segment(path);
            let impl_names = names.iter().map(|name| last_segment(name).to_string()).collect();
            let own_impls = type_impl_bodies(&code, &impl_names);
            for name in names.iter().map(String::as_str).chain(["Self"]) {
                for at in whole_word_positions(&code, name) {
                    if name == "Self" && !own_impls.iter().any(|body| body.contains(&at)) {
                        continue;
                    }
                    let rest = code[at + name.len()..].trim_start();
                    let before = code[..at].trim_end();
                    // These two may be named only where a forwarding layer receives one; the construction
                    // checks below still run on those places too.
                    if matches!(bare, "ResolvePermissionRequest" | "PermissionDecision") {
                        let imported = imports.iter().any(|span| span.contains(&at));
                        let parameter = parameters
                            .iter()
                            .any(|(_, span)| span.contains(&at) && at + name.len() == span.end);
                        if !imported && !parameter {
                            offenders.push(format!(
                                "{rel}: forwarding code uses {bare} outside a received parameter or use: {}",
                                snippet(&code, at)
                            ));
                        }
                    }
                    let type_position = ["->", "impl", "for", "struct", "enum", "trait", "type"]
                        .iter()
                        .any(|kw| before.ends_with(kw));
                    let literal = rest.starts_with('{') && !type_position;
                    let decision =
                        bare == "PermissionDecision" && rest.starts_with("::") && !rest.starts_with("::decode");
                    // Defaults and typed trait calls build the same wire values as a literal.
                    // Ignore whitespace throughout, including inside qualified trait paths.
                    let tail: String = rest.chars().filter(|ch| !ch.is_whitespace()).collect();
                    let head: String = before.chars().filter(|ch| !ch.is_whitespace()).collect();
                    let makes_value = |text: &str| {
                        ["::default(", "::default::<", "::decode(", "::decode::<"]
                            .iter()
                            .any(|call| text.contains(call))
                    };
                    let associated = tail.starts_with("::default") || tail.starts_with("::decode");
                    let qualified = (tail.starts_with('>') || rest.starts_with("as "))
                        && tail
                            .split_once('>')
                            .is_some_and(|(_, call)| call.starts_with("::default") || call.starts_with("::decode"));
                    let typed = tail.starts_with('=') && makes_value(tail.split(';').next().unwrap());
                    let generic = head.ends_with("::default::<") || head.ends_with("::decode::<");
                    if literal || decision || associated || qualified || typed || generic {
                        offenders.push(format!(
                            "{rel}: forwarding code constructs or rewrites {bare}: {}",
                            snippet(&code, at)
                        ));
                    }
                }
            }
            // A variant imported by name or glob cannot subsequently be distinguished from a local binding.
            for (full, alias) in use_bindings(&code) {
                if bare == "PermissionDecision"
                    && (names.iter().any(|name| full.starts_with(&format!("{name}::")))
                        || alias == "*" && (names.contains(&full) || last_segment(&full) == bare)
                        || full.split("::").any(|part| part == bare) && last_segment(&full) != bare)
                {
                    offenders.push(format!("{rel}: imported PermissionDecision variant: {full}"));
                }
            }
        }
    }
    for (index, site) in REMOTE_DECISION_SITES.iter().enumerate() {
        if found[index] != 1 {
            offenders.push(format!("remote decision site {site:?} must occur exactly once"));
        }
    }
    let skipped = test_files.into_iter().filter(|rel| files.contains_key(rel)).collect();
    (offenders, skipped)
}

/// The trees whose names for the guarded types the forwarding scan reads: the helper, and the two crates it
/// depends on that live in this workspace.
const FORWARDING_ALIAS_TREES: [&str; 3] = ["server/src", "agent/src", "core/src"];

#[test]
fn wire_guard_covers_the_helper_and_the_transport() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut files = BTreeMap::new();
    for dir in FORWARDING_ALIAS_TREES {
        let tree = read_tree(&workspace.join(dir));
        assert!(!tree.is_empty(), "{dir} must contain production sources");
        for (rel, source) in tree {
            files.insert(format!("{dir}/{rel}"), source);
        }
    }
    assert!(
        files
            .keys()
            .any(|path| path.starts_with("agent/src/providers/claude_sidecar/transport/")),
        "the transport directory must be scanned"
    );
    // The helper depends on `eitri-core`, so a name core gives a guarded type is reachable from it.
    assert!(files.contains_key("core/src/lib.rs"), "core's aliases must be read");
    let (offenders, skipped) = scan_remote_decisions_and_skips(&files);
    assert!(offenders.is_empty(), "{}", offenders.join("\n"));
    // Every file of the judged trees left out as test code, by name: a new one is a deliberate edit here.
    let skipped: BTreeSet<&str> = skipped
        .iter()
        .map(String::as_str)
        .filter(|rel| {
            rel.starts_with("server/src/") || rel.starts_with("agent/src/providers/claude_sidecar/transport/")
        })
        .collect();
    assert_eq!(
        skipped,
        BTreeSet::from(["server/src/journal/regressions.rs", "server/src/serve/fake_sidecar.rs"]),
        "the forwarding scan's test-only files changed"
    );
}

/// The helper's gate path facts compute what the disk says and decide nothing: both scans read their files as
/// production code, with no exception, and find nothing in them.
#[test]
fn the_server_gate_files_build_no_decision() {
    const GATE_FILES: [&str; 3] = ["gate/mod.rs", "gate/surroundings.rs", "serve/gate.rs"];
    let helper = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("server/src");
    let tree = read_tree(&helper);
    let report = scan_tree(&tree);
    for file in GATE_FILES {
        assert!(report.scanned.contains(file), "server/src/{file} was not scanned");
        assert!(!report.skipped.contains(file), "server/src/{file} was skipped");
        assert!(
            !report.offenders.iter().any(|offender| offender.contains(file)),
            "{}",
            report.offenders.join("\n")
        );
    }
    // The forwarding scan with every tree it reads names from, as the whole-tree test runs it.
    let workspace = helper.parent().unwrap().parent().unwrap();
    let mut files = BTreeMap::new();
    for dir in FORWARDING_ALIAS_TREES {
        for (rel, source) in read_tree(&workspace.join(dir)) {
            files.insert(format!("{dir}/{rel}"), source);
        }
    }
    let (offenders, skipped) = scan_remote_decisions_and_skips(&files);
    for file in GATE_FILES {
        let rel = format!("server/src/{file}");
        assert!(files.contains_key(&rel), "{rel} is missing");
        assert!(!skipped.contains(&rel), "{rel} was skipped by the forwarding scan");
        assert!(
            !offenders.iter().any(|offender| offender.contains(&rel)),
            "{}",
            offenders.join("\n")
        );
        assert!(
            !REMOTE_DECISION_SITES.iter().any(|(site, _)| *site == rel),
            "{rel} has a decision-site exception"
        );
    }
}

#[test]
fn wire_guard_refuses_remote_decision_construction_under_aliases_and_across_lines() {
    for source in [
        "let p = ClaudeHostPolicy {};",
        "let p = CreateSessionRequest { ..request };",
        "let p = ResumeSessionRequest { ..request };",
        "let p = ResolvePermissionRequest {};",
        "impl CreateSessionRequest { fn make() -> Self { Self { ..request } } }",
        "impl PermissionDecision { fn make() -> Self { Self::Allow } }",
        "use crate::process::PermissionDecision as D; let p = D::Allow;",
        "use proto::PermissionDecision::Allow as Grant; let p = Grant;",
        "use proto::CreateSessionRequest as R; type S = R; let p = S { ..request };",
        "use agent::PermissionDecision as D; use D::Allow as A; let p = A;",
        "use agent::PermissionDecision::*; let p = Allow;",
        "use claude_runtime_protocol::v1::CreateSessionRequest as R; let p = R\n { ..request };",
        "use agent::PermissionDecision as D; let p = D :: Allow;",
        "use agent::PermissionDecision::{Allow as A}; let p = A;",
        "let p = PermissionDecision::Deny { reason: None };",
        "let p = ResolvePermissionRequest::decode(bytes)?; p.encode(&mut out)?;",
        "let mut p = ResolvePermissionRequest::default(); p.allow = true;",
        "let mut p = proto::ResolvePermissionRequest :: default(); p.defer = true;",
        "use proto::ResolvePermissionRequest as R; let mut p = R::default(); p.reason = None;",
        "let p: ResolvePermissionRequest = Default::default();",
        "let p: proto::ResolvePermissionRequest = std::default::Default :: default();",
        "let p = <ResolvePermissionRequest as Default>::default();",
        "let p = <proto::ResolvePermissionRequest as std::default::Default> :: default();",
        "let p: ResolvePermissionRequest = <_ as Default>::default();",
        "let p = <ResolvePermissionRequest>::default();",
        "let mut p = CreateSessionRequest::default(); p.policy = policy;",
        "let p: ResumeSessionRequest = Default::default();",
        "let p: PermissionDecision = Default::default();",
        "let p = <PermissionDecision as Default>::default();",
        "let p = proto::PermissionDecision::default();",
        "let p: ResolvePermissionRequest = prost::Message::decode(buf)?; p.encode(&mut out)?;",
        "let p: ResolvePermissionRequest = Message :: decode(buf)?; p.encode(&mut out)?;",
        "let p = <ResolvePermissionRequest as prost::Message>::decode(buf)?;",
        "let p = Message::decode::<ResolvePermissionRequest>(buf)?;",
        "use proto::ResolvePermissionRequest as R; type S = R; let p: S = Message::decode(buf)?;",
        "let p:\n ResolvePermissionRequest\n = Default\n :: default();",
        "fn forward(mut r: ResolvePermissionRequest) { r.allow = true; send(r); }",
        "fn blank() -> ResolvePermissionRequest { Default::default() }",
        "let r: ResolvePermissionRequest = Message::decode_length_delimited(buf)?; r.encode(&mut out)?;",
        "fn forward(mut r: ResolvePermissionRequest) { r.merge(patch)?; send(r); }",
        "let o: Option<ResolvePermissionRequest> = Some(Default::default());",
        "let mut r: ResolvePermissionRequest; r = Default::default();",
        "fn forward(mut r: ResolvePermissionRequest) { r.defer = true; send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { r.reason = None; send(r); }",
        "r . allow |= true;",
        "r . defer &= false;",
        "r . reason = None;",
        "fn forward(mut r: ResolvePermissionRequest) { r . merge (patch)?; send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { r.merge_length_delimited(patch)?; send(r); }",
        "prost::Message :: merge(&mut r, patch)?;",
        "< _ as prost::Message > :: merge(&mut r, patch)?;",
        "fn forward(mut r: ResolvePermissionRequest) { r.merge::<Bytes>(patch)?; send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { r.merge_length_delimited::<Bytes>(patch)?; send(r); }",
        "fn forward(r: Option<ResolvePermissionRequest>) { send(r); }",
        "fn forward(r: PermissionDecision::Allow) { send(r); }",
        "fn forward(r: Wrapper<PermissionDecision>) { send(r); }",
        "fn blank() -> PermissionDecision { helper() }",
        "let r: PermissionDecision = helper();",
        "fn forward(r: ResolvePermissionRequest) { let _ = ResolvePermissionRequest; send(r); }",
        "use proto::ResolvePermissionRequest as R; fn blank() -> R { helper() }",
        "use agent::PermissionDecision as D; let p: Option<D> = helper();",
    ] {
        for file in [
            "server/src/forward.rs",
            "agent/src/providers/claude_sidecar/transport/forward.rs",
        ] {
            for spelling in [source.to_owned(), source.replace(' ', "\n"), source.replace('\n', " ")] {
                assert!(
                    !scan_remote_decisions(&BTreeMap::from([(file.into(), spelling.clone())])).is_empty(),
                    "{file}: {spelling}"
                );
            }
        }
    }
    let reexported = BTreeMap::from([
        (
            "agent/src/policy_names.rs".into(),
            "pub use claude_runtime_protocol::v1::CreateSessionRequest as WireRequest;".into(),
        ),
        (
            "server/src/forward.rs".into(),
            "use agent::policy_names::WireRequest as R; let p = R { ..request };".into(),
        ),
    ]);
    assert!(
        !scan_remote_decisions(&reexported).is_empty(),
        "a re-export outside the forwarding tree must not hide a literal"
    );
    for source in [
        "fn forward(request: ResolvePermissionRequest) { send(request); }",
        "use proto::ResolvePermissionRequest as R; fn forward(request: R) { send(request); }",
        "use agent::PermissionDecision; fn forward(decision: PermissionDecision) { send(decision); }",
        "fn forward(a: u8, request: proto::ResolvePermissionRequest, b: u8) { send(request); }",
        "fn forward<'a>(request: &'a mut proto::ResolvePermissionRequest) { send(request); }",
        "fn forward<F: Fn() -> ()>(request: ResolvePermissionRequest, send: F) { send(request); }",
        "fn forward(request: ResolvePermissionRequest) { let same = r.allow == true && r.defer != true; send(request); }",
    ] {
        for file in [
            "server/src/forward.rs",
            "agent/src/providers/claude_sidecar/transport/forward.rs",
        ] {
            for spelling in [source.to_owned(), source.replace(' ', "\n")] {
                let offenders = scan_remote_decisions(&BTreeMap::from([(file.into(), spelling.clone())]));
                assert!(offenders.is_empty(), "{file}: {spelling}: {offenders:?}");
            }
        }
    }
}

#[test]
fn wire_guard_refuses_forwarding_rewrites_through_borrows_raw_names_and_fake_imports() {
    let refused = [
        // A string or a raw identifier holding the word `use` is no import.
        "let r = (log(\"port in use\"), ResolvePermissionRequest { permission_id: id, allow: true, defer: false, \
         reason: String::new(), ..Default::default() }).1; send(r);",
        "let r#use = 1; let r = (r#use, ResolvePermissionRequest { allow: true, ..Default::default() }).1; send(r);",
        "let r = (log(\"use\"), ResolvePermissionRequest { allow: true, ..Default::default() }).1; send(r);",
        // Decoding: these trees forward bytes.
        "fn forward(r: ResolvePermissionRequest) { send(r); } \
         fn relay(buf: &[u8]) { forward(prost::Message::decode(buf).unwrap()); }",
        "fn relay(buf: &[u8]) { forward(Message::decode_length_delimited(buf).unwrap()); }",
        "fn relay(buf: &[u8]) { forward(prost::Message::r#decode(buf).unwrap()); }",
        // Whole-value replacement through a received parameter.
        "fn fix(r: &mut ResolvePermissionRequest) { *r = Default::default(); }",
        "fn fix(r: &mut ResolvePermissionRequest) { *(r) = helper(); }",
        "fn fix(r: &mut ResolvePermissionRequest, buf: &[u8]) { *r = prost::Message::decode(buf).unwrap(); }",
        "fn fix2(r: &mut ResolvePermissionRequest) { let _old = std::mem::take(r); }",
        "fn fix(a: &mut ResolvePermissionRequest, b: &mut ResolvePermissionRequest) { std::mem::swap(a, b); }",
        "use std::mem; fn fix(r: &mut ResolvePermissionRequest) { mem::replace(r, other()); }",
        "use std::mem::take as t; fn fix(r: &mut ResolvePermissionRequest) { t(r); }",
        "use core::mem::*; fn fix(mut r: ResolvePermissionRequest) { swap(&mut r, &mut other); }",
        "use agent::PermissionDecision as D; fn fix(d: &mut D) { *d = other(); }",
        // Field writes the plain `.allow =` text does not show.
        "fn forward(mut r: ResolvePermissionRequest) { *(&mut r.allow) = true; send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { let a = &mut r.defer; *a = false; send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { std::mem::replace(&mut r.allow, true); send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { std::mem::take(&mut r.reason); send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { let d = &mut r.decision; send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { let a = &raw mut r.allow; send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { let a = &mut (r).defer; send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { r.r#allow = true; send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { r.r#reason = None; send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { r.reason.clear(); r.reason.push_str(\"ok\"); send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { r.reason.push_str(\"ok\"); send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { r.decision.clone_from(&other); send(r); }",
        // Merges and clears under any path or method name.
        "use prost::Message as M; \
         fn forward(mut r: ResolvePermissionRequest, p: &[u8]) { M::merge(&mut r, p).unwrap(); send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { r.clear(); send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { Message::clear(&mut r); send(r); }",
        "fn forward(mut r: ResolvePermissionRequest, p: &[u8]) { r.merge_field(1, w, &mut p, c).unwrap(); send(r); }",
        // The agent-side request's own decision field.
        "use agent::ResolvePermissionRequest; fn forward(mut r: ResolvePermissionRequest) { \
         r.decision = eitri_core::agent_bridge::DecisionChoice::Allow.into_decision(None); send(r); }",
        "r . r#decision = d;",
        "r.decision |= d;",
        // A raw parameter name is the plain one to the compiler.
        "fn rewrite(r#decision: &mut agent::PermissionDecision, earlier: agent::PermissionDecision) { \
         *r#decision = earlier; }",
        // A parenthesized received value after a loop's `in` is a group, not a call's arguments.
        "fn forward(mut answer: ResolvePermissionRequest) { for _ in (answer).permission_id.drain(..) {} send(answer); }",
        "fn forward(mut answer: ResolvePermissionRequest) { for _ in (answer.permission_id).drain(..) {} send(answer); }",
        "fn forward(mut answer: ResolvePermissionRequest) { for _ in (answer.decision).drain(..) {} send(answer); }",
        "fn forward(mut answer: ResolvePermissionRequest) { while let Some(_) = (answer.reason).pop() {} send(answer); }",

    ];
    let mut passed = Vec::new();
    for source in refused {
        for file in [
            "server/src/forward.rs",
            "agent/src/providers/claude_sidecar/transport/forward.rs",
        ] {
            for spelling in [source.to_owned(), source.replace(' ', "\n")] {
                if scan_remote_decisions(&BTreeMap::from([(file.into(), spelling.clone())])).is_empty() {
                    passed.push(format!("{file}: {spelling}"));
                }
            }
        }
    }
    assert!(passed.is_empty(), "passed the scan:\n{}", passed.join("\n"));
    // A name core gives a guarded type, used in the helper.
    let through_core = BTreeMap::from([
        (
            "core/src/lib.rs".into(),
            "pub type Answer = agent::PermissionDecision;".into(),
        ),
        (
            "server/src/relay.rs".into(),
            "fn relay() { send(eitri_core::Answer::Allow); }".into(),
        ),
    ]);
    assert!(!scan_remote_decisions(&through_core).is_empty());
    // The `Message` trait under a name another file in the read trees gives it.
    let trait_alias = BTreeMap::from([
        ("server/src/lib.rs".into(), "pub use prost::Message as Wire;".into()),
        (
            "server/src/relay.rs".into(),
            "use crate::Wire; fn relay(b: &[u8]) { forward(Wire::decode(b).unwrap()); }".into(),
        ),
    ]);
    assert!(!scan_remote_decisions(&trait_alias).is_empty());
    for source in [
        "use agent::ResolvePermissionRequest; fn forward(request: ResolvePermissionRequest) { send(request); }",
        "pub use agent::{PermissionDecision, ResolvePermissionRequest}; fn forward(r: ResolvePermissionRequest) {}",
        "#[allow(unused)] use agent::ResolvePermissionRequest as R; fn forward(request: R) { send(request); }",
        "fn forward(request: ResolvePermissionRequest) { log(request.reason.as_str()); send(request); }",
        "fn forward(request: &ResolvePermissionRequest) { if request.decision.allows() { count(); } send(request); }",
        "fn forward(request: &mut ResolvePermissionRequest) { send(request); }",
        "fn forward(request: ResolvePermissionRequest, buffer: &mut Vec<u8>) { buffer.push(1); send(request); }",
    ] {
        for file in [
            "server/src/forward.rs",
            "agent/src/providers/claude_sidecar/transport/forward.rs",
        ] {
            for spelling in [source.to_owned(), source.replace(' ', "\n")] {
                let offenders = scan_remote_decisions(&BTreeMap::from([(file.into(), spelling.clone())]));
                assert!(offenders.is_empty(), "{file}: {spelling}: {offenders:?}");
            }
        }
    }
}

#[test]
fn wire_guard_refuses_decision_writes_through_places_function_values_and_whole_value_methods() {
    let refused = [
        // A decision field written in parentheses, behind an index, under `ref mut` or on the left of a
        // destructuring assignment.
        "use agent::ResolvePermissionRequest; fn forward(mut r: ResolvePermissionRequest) { \
         (r.decision) = eitri_core::agent_bridge::DecisionChoice::Allow.into_decision(None); send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { (r.allow) = true; send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { ((r.allow)) |= true; send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { (r.allow, r.defer) = (true, false); send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { (r.allow, x) = (true, 1); send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { [r.allow, x] = [true, false]; send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { Pair { a: r.allow, b: x } = pair; send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { let ref mut a = r.allow; *a = true; send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { if let Some(ref mut s) = r.reason { s.push_str(\"ok\"); } \
         send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { (r.reason).push_str(\"ok\"); send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { r.reason[..].make_ascii_uppercase(); send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { let x = (r.reason).take(); send(r); }",
        // Merging and clearing through a function value.
        "use prost::Message as M; \
         fn forward(mut r: ResolvePermissionRequest, p: &[u8]) { (M::merge)(&mut r, p).unwrap(); send(r); }",
        "use prost::Message as M; \
         fn forward(mut r: ResolvePermissionRequest, p: &[u8]) { let f = M::merge; f(&mut r, p).unwrap(); send(r); }",
        "use prost::Message as M; fn forward(mut r: ResolvePermissionRequest, p: &[u8]) { \
         let f = M::merge_length_delimited; f(&mut r, p).unwrap(); send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { let c = prost::Message::clear; c(&mut r); send(r); }",
        // A merge, clear or decode through a guarded type, through the trait, or on a guarded argument.
        "use proto::ResolvePermissionRequest as R; fn forward(mut x: R, p: &[u8]) { R::merge(&mut x, p)?; send(x); }",
        "fn forward(r: ResolvePermissionRequest, b: &[u8]) { let e = Message::decode::<SessionEvent>(b)?; send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { clear(&mut r); send(r); }",
        "fn forward(mut r: ResolvePermissionRequest, p: &[u8]) { Envelope::merge(&mut r.reason, p); send(r); }",
        // A place in parentheses after a match arm's `=>` or a statement attribute.
        "fn forward(mut r: ResolvePermissionRequest) { match x { _ => (r.reason).push_str(\"ok\"), } send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { match x { _ => (r.allow) |= true, } send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { #[rustfmt::skip] (r.allow) |= true; send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { #[allow(unused_parens)] (r.reason).push_str(\"ok\"); send(r); }",
        // The received value borrowed in place.
        "fn relay(r: &mut ResolvePermissionRequest, e: &ResolvePermissionRequest) { (&mut r).clone_from(e); }",
        "fn relay(r: &mut ResolvePermissionRequest, e: &ResolvePermissionRequest) { **(&mut r) = e.clone(); }",
        // The received value's other fields: an answer moved onto another permission.
        "fn forward(mut r: ResolvePermissionRequest, other: String) { r.permission_id = other; send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { r.permission_id.push_str(\"x\"); send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { r.session_id.clear(); send(r); }",
        "fn forward(mut r: ResolvePermissionRequest) { let p = &mut r.permission_id; send(r); }",
        // Whole-value replacement by a method or through a block.
        "fn relay(r: &mut ResolvePermissionRequest, earlier: &ResolvePermissionRequest) { r.clone_from(earlier); }",
        "fn relay(r: &mut ResolvePermissionRequest) { *{r} = Default::default(); }",
        // The guarded name inside what an any-`use`-word span would have taken for an import.
        "let x = (log(\"in use\"), make::<ResolvePermissionRequest>()).1; send(x);",
        "let r#use = 1; let x = (r#use, make::<ResolvePermissionRequest>()).1; send(x);",
    ];
    let mut passed = Vec::new();
    for source in refused {
        for file in [
            "server/src/forward.rs",
            "agent/src/providers/claude_sidecar/transport/forward.rs",
        ] {
            for spelling in [source.to_owned(), source.replace(' ', "\n")] {
                if scan_remote_decisions(&BTreeMap::from([(file.into(), spelling.clone())])).is_empty() {
                    passed.push(format!("{file}: {spelling}"));
                }
            }
        }
    }
    assert!(passed.is_empty(), "passed the scan:\n{}", passed.join("\n"));
    for source in [
        "fn forward(request: ResolvePermissionRequest, out: &mut Vec<u8>) { request.encode(out).unwrap(); \
         send(request.clone()); }",
        "fn forward(request: &ResolvePermissionRequest) { log(request.encoded_len()); log((request.reason).len()); }",
        "fn forward(request: ResolvePermissionRequest) { log(&request.reason).unwrap(); send(request); }",
        "fn forward(request: ResolvePermissionRequest) { if (request.allow) == true { count(); } send(request); }",
        "fn forward(request: ResolvePermissionRequest) { match (request.allow, request.defer) { (true, false) => \
         count(), _ => {} } send(request); }",
        "fn forward(request: ResolvePermissionRequest) { let (a, b) = (request.allow, request.defer); send(request); }",
        "fn forward(request: ResolvePermissionRequest, seen: &mut [u8]) { seen[usize::from(request.allow)] = 1; \
         send(request); }",
        "fn forward(request: ResolvePermissionRequest, out: &mut Vec<u8>) { out.clear(); send(request); }",
        // The helper's own shapes: collection clears, its journal's frame decoder, other messages' decodes.
        "impl Children { fn stop(&mut self) { for child in &self.children { kill(child); } self.children.clear(); } }",
        "fn settle(state: &mut SyncState) { state.dirty.clear(); state.inflight.clear(); }",
        "fn settle(pending: &mut Pending, sequence: u64) -> io::Result<()> { pending.clear(sequence) }",
        "use segment::{bounds, corruption, decode, encode}; use super::segment::decode as read_frame; \
         fn read(path: &Path, data: &[u8]) { let decoded = decode(path, &data, 0); }",
        "pub(super) fn decode(path: &Path, data: &[u8], offset: usize) -> io::Result<Option<(Record, usize)>> { \
         frame(path, data, offset) }",
        "impl Decoder for Raw { fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Bytes>, Status> { \
         take(src) } }",
        "fn on_frame(bytes: &mut Vec<u8>) { if let Ok(created) = CreateSessionResponse::decode(&bytes[5..]) { \
         note(created); } bytes.clear(); }",
        "fn on_event(bytes: &[u8]) -> io::Result<()> { let event = SessionEvent::decode(bytes).map_err(other)?; \
         send(event) }",
        "fn gap(status: &Status) -> bool { ErrorDetail::decode(status.details()).is_ok_and(|d| d.code == 7) }",
        "fn events(frames: Vec<Bytes>) { let _ = frames.into_iter().map(SessionEvent::decode); }",
        "fn forward(request: ResolvePermissionRequest) { match x { _ => (request.reason).len(), } send(request); }",
        "fn forward(request: ResolvePermissionRequest) { let v = f::<u8>(request.allow); send(request); }",
        "fn forward(r: ResolvePermissionRequest) { log(r.permission_id.as_str()); send(r.permission_id.clone()); }",
        // Passing the received value on: the call's `)` ends the place.
        "async fn resolve(answer: ResolvePermissionRequest) -> R { \
         client.resolve_permission(answer).await.map_err(f)?; }",
        "async fn resolve(answer: ResolvePermissionRequest) { client.resolve_permission(answer).await.unwrap(); }",
        "async fn resolve(answer: ResolvePermissionRequest) -> bool { \
         client.resolve_permission(answer).await.is_ok() }",
        "async fn resolve(answer: ResolvePermissionRequest) -> R { \
         client.resolve_permission(tonic::Request::new(answer)).await.map_err(f)?; }",
        "fn resolve(answer: ResolvePermissionRequest) { tx.send(answer).unwrap(); }",
        "fn resolve(answer: ResolvePermissionRequest) { let _ = Some(answer).map(send); }",
        "fn resolve(answer: ResolvePermissionRequest) -> R { Ok(answer).map_err(f) }",
        // Plain reads of the received value's other fields.
        "fn route(answer: ResolvePermissionRequest) { let a = answer.session_id.as_bytes(); \
         let b = answer.permission_id.parse::<u64>(); let c = answer.session_id.to_lowercase(); send(answer); }",
    ] {
        for file in [
            "server/src/forward.rs",
            "agent/src/providers/claude_sidecar/transport/forward.rs",
        ] {
            for spelling in [source.to_owned(), source.replace(' ', "\n")] {
                let offenders = scan_remote_decisions(&BTreeMap::from([(file.into(), spelling.clone())]));
                assert!(offenders.is_empty(), "{file}: {spelling}: {offenders:?}");
            }
        }
    }
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
    let mut report = scan_tree(&read_tree(&root));
    let helper = root.parent().unwrap().parent().unwrap().join("server/src");
    let helper_report = scan_tree(&read_tree(&helper));
    report.offenders.extend(helper_report.offenders.iter().cloned());
    // Every file left out as test code, by name: a new one is a deliberate edit here.
    let skipped = |report: &Report| report.skipped.iter().cloned().collect::<Vec<_>>();
    assert_eq!(
        skipped(&report),
        [
            "history/archive/tests.rs",
            "ingestion/eviction/record_tests.rs",
            "ingestion/eviction/tests.rs",
            "projection/planning/tests.rs",
            "providers/claude_sidecar/runtime_policy_verification.rs",
            "test_tmp.rs",
            "test_umask.rs",
            "wire_guard.rs",
        ],
        "agent/src's test-only files changed"
    );
    assert_eq!(
        skipped(&helper_report),
        ["journal/regressions.rs", "serve/fake_sidecar.rs"],
        "server/src's test-only files changed"
    );

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
        "production code that could reach an ungated CLI (see agent/src/wire_guard.rs):\n{}",
        report.offenders.join("\n")
    );
    // A stale allowlist entry is a rotting one: the arm it names must still be there, once.
    assert_eq!(
        report.allowlisted, 1,
        "the allowlisted `PermissionModeChanged` decode arm in {} was not found",
        ALLOWED_ARM.0
    );
    assert_eq!(
        report.input_key_reads, 1,
        "the allowlisted read of {} in {} was not found exactly once",
        ALLOWED_INPUT_KEY.1, ALLOWED_INPUT_KEY.0
    );
    // And so is a stale auto site: each must be there, once.
    for (i, (file, text)) in AUTO_SITES.iter().enumerate() {
        assert_eq!(
            report.auto_sites_found.get(&i).copied().unwrap_or(0),
            1,
            "AUTO_SITES entry in {file} must be found exactly once: {text}"
        );
    }
}

fn one(rel: &str, src: &str) -> Report {
    scan_tree(&BTreeMap::from([(rel.to_string(), src.to_string())]))
}

/// The fields the trust and identity checks read, stated correctly, for the fixtures that test
/// other rules.
const COMPLETE: &str = "configuration: ConfigurationProfile::Native as i32, executable: ExecutableSource::HostCli as i32, \
     setting_sources: Some(SettingSourceSelection { sources: proto_sources(crate::setting_sources::for_session(u, p)) }), \
     tool_policy: Some(ToolPolicy { deny: vec![], unrestricted: true, allow: None }),";

fn policy(permissions: &str, switchable: &str) -> String {
    policy_with_cli_mode(permissions, switchable, "CliPermissionMode::Unspecified as i32")
}

fn policy_with_cli_mode(permissions: &str, switchable: &str, cli_mode: &str) -> String {
    format!(
        "use claude_runtime_protocol::v1::{{ClaudeHostPolicy, CliPermissionMode, PermissionMode as ProtoPermissionMode}};\n\
         fn p() -> Option<ClaudeHostPolicy> {{\n    Some(ClaudeHostPolicy {{\n        \
         permissions: {permissions},\n        permission_mode_switchable: {switchable},\n        \
         cli_permission_mode: {cli_mode},\n        {COMPLETE}\n    }})\n}}\n"
    )
}

/// A policy that leaves the settings selection, the tool policy, the executable or the profile
/// unstated, or builds the policy from `default`, is refused; the complete fixture passes.
#[test]
fn the_trust_and_identity_fields_of_a_policy_are_stated() {
    let interactive = "ProtoPermissionMode::Interactive as i32";
    let good = policy(interactive, "false");
    assert!(one("x.rs", &good).offenders.is_empty());
    for (what, from, to) in [
        ("no selection", "setting_sources: Some(SettingSourceSelection { sources: proto_sources(crate::setting_sources::for_session(u, p)) }),", ""),
        ("a None selection", "setting_sources: Some(SettingSourceSelection { sources: proto_sources(crate::setting_sources::for_session(u, p)) }),", "setting_sources: None,"),
        ("a hand-written selection", "setting_sources: Some(SettingSourceSelection { sources: proto_sources(crate::setting_sources::for_session(u, p)) }),", "setting_sources: Some(SettingSourceSelection { sources: vec![] }),"),
        ("no tool policy", "tool_policy: Some(ToolPolicy { deny: vec![], unrestricted: true, allow: None }),", ""),
        ("a None tool policy", "tool_policy: Some(ToolPolicy { deny: vec![], unrestricted: true, allow: None }),", "tool_policy: None,"),
        ("the bundled CLI", "executable: ExecutableSource::HostCli as i32,", "executable: ExecutableSource::SdkBundled as i32,"),
        ("no executable", "executable: ExecutableSource::HostCli as i32,", ""),
        ("an isolated profile", "configuration: ConfigurationProfile::Native as i32,", "configuration: ConfigurationProfile::Isolated as i32,"),
    ] {
        let bad = good.replace(from, to);
        assert_ne!(bad, good, "{what}: the fixture changed");
        assert!(!one("x.rs", &bad).offenders.is_empty(), "{what} must be refused");
    }
}

#[test]
fn a_default_policy_or_one_written_under_an_alias_is_refused() {
    let default = "use claude_runtime_protocol::v1::ClaudeHostPolicy;\nfn p() -> ClaudeHostPolicy { ClaudeHostPolicy::default() }\n";
    assert!(!one("x.rs", default).offenders.is_empty());
    let aliased = "use claude_runtime_protocol::v1::ClaudeHostPolicy as P;\nfn p() -> P { P { permissions: 1 } }\n";
    let report = one("x.rs", aliased);
    assert!(
        report.offenders.iter().any(|o| o.contains("permissions")),
        "a literal under an alias is read: {:?}",
        report.offenders
    );
    let alias_default = "type P = claude_runtime_protocol::v1::ClaudeHostPolicy;\nfn p() -> P { P::default() }\n";
    assert!(!one("x.rs", alias_default).offenders.is_empty());
}

#[test]
fn a_session_request_states_a_policy() {
    let head = "use claude_runtime_protocol::v1::CreateSessionRequest as R;\n";
    let ok = format!("{head}fn r() -> R {{ R {{ cwd: c, policy: Some(x()), resume: None }} }}\n");
    assert!(
        one("x.rs", &ok).offenders.is_empty(),
        "{:?}",
        one("x.rs", &ok).offenders
    );
    for body in ["cwd: c, policy: None", "cwd: c", "cwd: c, policy", "cwd: c, ..base"] {
        let bad = format!("{head}fn r() -> R {{ R {{ {body} }} }}\n");
        assert!(!one("x.rs", &bad).offenders.is_empty(), "`{body}` must be refused");
    }
    // A policy built from `default` is `Some(..)` and states nothing.
    for value in [
        "Some(Default::default())",
        "Some(<ClaudeHostPolicy as Default>::default())",
        "Some(ClaudeHostPolicy::default())",
        "Some( Default :: default() )",
    ] {
        let bad = format!("{head}fn r() -> R {{ R {{ cwd: c, policy: {value} }} }}\n");
        assert!(!one("x.rs", &bad).offenders.is_empty(), "`{value}` must be refused");
    }
    // The crate's own request type of the same name is not the wire's.
    let own =
        "struct CreateSessionRequest { cwd: u8 }\nfn r() -> CreateSessionRequest { CreateSessionRequest { cwd: 1 } }\n";
    assert!(one("x.rs", own).offenders.is_empty());
}

/// Rule 6, the literal half: UNSPECIFIED passes, and so does the request's one `match` on the
/// requested mode, whose `Default` states DEFAULT only under `offers_plan_mode` -- but only in `mod.rs`,
/// where it is a listed site, since its proto `Auto` is a site. Anything else as the value fails: the
/// proto `Auto` stated outright, DEFAULT outright or under another condition, a number, a different
/// `match` (the one before plan mode included), or no statement at all.
#[test]
fn the_cli_mode_is_unspecified_or_the_one_gated_match() {
    let interactive = "ProtoPermissionMode::Interactive as i32";
    assert!(one("x.rs", &policy(interactive, "false")).offenders.is_empty());
    let gated = "match cli_mode {\n crate::RequestedCliMode::Auto => CliPermissionMode::Auto as i32,\n \
                 crate::RequestedCliMode::Default if offers_plan_mode => CliPermissionMode::Default as i32,\n \
                 crate::RequestedCliMode::Default => CliPermissionMode::Unspecified as i32,\n }";
    let in_mod = one(
        "providers/claude_sidecar/mod.rs",
        &policy_with_cli_mode(interactive, "false", gated),
    );
    assert!(in_mod.offenders.is_empty(), "{:?}", in_mod.offenders);
    assert_eq!(in_mod.auto_sites_found.values().sum::<usize>(), 1);
    assert_eq!(
        one("x.rs", &policy_with_cli_mode(interactive, "false", gated))
            .offenders
            .len(),
        2,
        "elsewhere, both the literal's gated match and its proto Auto are outside a site"
    );
    for value in [
        "CliPermissionMode::Auto as i32",
        "CliPermissionMode::Default as i32",
        "2",
        "match cli_mode { _ => CliPermissionMode::Unspecified as i32 }",
        // The match without its plan-mode arm, or with DEFAULT stated whatever the session offers.
        "match cli_mode { crate::RequestedCliMode::Auto => CliPermissionMode::Auto as i32, \
         crate::RequestedCliMode::Default => CliPermissionMode::Unspecified as i32, }",
        "match cli_mode { crate::RequestedCliMode::Auto => CliPermissionMode::Auto as i32, \
         crate::RequestedCliMode::Default => CliPermissionMode::Default as i32, }",
        "match cli_mode { crate::RequestedCliMode::Auto => CliPermissionMode::Auto as i32, \
         crate::RequestedCliMode::Default if true => CliPermissionMode::Default as i32, \
         crate::RequestedCliMode::Default => CliPermissionMode::Unspecified as i32, }",
    ] {
        let report = one(
            "providers/claude_sidecar/mod.rs",
            &policy_with_cli_mode(interactive, "false", value),
        );
        assert!(
            report.offenders.iter().any(|o| o.contains("cli_permission_mode")),
            "{value}: {:?}",
            report.offenders
        );
    }
    // Not stated at all.
    let unstated = "fn p() -> ClaudeHostPolicy { ClaudeHostPolicy { permissions: \
                    claude_runtime_protocol::v1::PermissionMode::Interactive as i32, \
                    permission_mode_switchable: false, COMPLETE_FIELDS } }";
    let unstated = unstated.replace("COMPLETE_FIELDS", COMPLETE);
    let report = one("x.rs", &unstated);
    assert_eq!(report.offenders.len(), 1, "{:?}", report.offenders);
    assert!(report.offenders[0].contains("cli_permission_mode"));
}

/// Rule 6, the site half: the requested `Auto`, the proto `Auto` under any name, and a capability
/// set to anything but `false` are each refused outside a listed site; assigning the mode or the
/// capability after the fact is refused everywhere.
#[test]
fn auto_comes_only_from_a_listed_site() {
    for src in [
        "fn f() -> RequestedCliMode { RequestedCliMode::Auto }",
        "fn f() -> crate::RequestedCliMode { crate::RequestedCliMode::Auto }",
        "use claude_runtime_protocol::v1::CliPermissionMode as C;\nfn f() -> i32 { C::Auto as i32 }",
        "fn f() -> i32 { claude_runtime_protocol::v1::CliPermissionMode::Auto as i32 }",
        "fn c() -> ProviderCapabilities { ProviderCapabilities { cli_auto_mode: true, } }",
        "fn c(x: bool) -> ProviderCapabilities { ProviderCapabilities { cli_auto_mode: x } }",
        "fn f(p: &mut ClaudeHostPolicy) { p.cli_permission_mode = 2; }",
        "fn f(c: &mut ProviderCapabilities) { c.cli_auto_mode = true; }",
        "fn f(p: &mut ClaudeHostPolicy) { p.set_cli_permission_mode(m); }",
    ] {
        for file in ["x.rs", "providers/claude_sidecar/mod.rs"] {
            assert_eq!(one(file, src).offenders.len(), 1, "{file}: {src}");
        }
    }
    // Patterns and comparisons are refused too unless listed: a new reading of the mode is a site.
    assert_eq!(
        one(
            "x.rs",
            "fn f(r: RequestedCliMode) -> bool { r == RequestedCliMode::Auto }"
        )
        .offenders
        .len(),
        1
    );
    // A `false` capability, the declaration, and a mere path through the field name are not sites.
    for src in [
        "fn c() -> ProviderCapabilities { ProviderCapabilities { cli_auto_mode: false, } }",
        "fn c() -> ProviderCapabilities { ProviderCapabilities { cli_auto_mode: false } }",
        "pub struct ProviderCapabilities { pub cli_auto_mode: bool, }",
        "fn f(c: ProviderCapabilities) -> bool { c.cli_auto_mode && c.resume }",
    ] {
        assert!(
            one("x.rs", src).offenders.is_empty(),
            "{src}: {:?}",
            one("x.rs", src).offenders
        );
    }
    // The listed capability site passes in its own file, and only there.
    let site = "fn c() -> ProviderCapabilities { ProviderCapabilities { cli_auto_mode: \
                CLIENT_IMPLEMENTS_CLI_AUTO_MODE && has(CAP_CLI_AUTO_MODE) && has(CAP_PERMISSION_DEFER), } }";
    assert!(one("providers/claude_sidecar/mod.rs", site).offenders.is_empty());
    assert_eq!(one("x.rs", site).offenders.len(), 1);
    // Dropping one of the two capabilities is no longer the listed site.
    let one_half = site.replace(" && has(CAP_PERMISSION_DEFER)", "");
    assert_eq!(one("providers/claude_sidecar/mod.rs", &one_half).offenders.len(), 1);
}

/// Every assignment operator after a guarded field is an assignment, not only a plain `=`, and
/// spaces around the `.` do not hide the field; the comparisons stay what they are.
#[test]
fn compound_assignments_and_spaced_fields_are_assignments() {
    for field in [
        "p.cli_permission_mode",
        "p.permissions",
        "p.permission_mode_switchable",
        "c.cli_auto_mode",
        "p.setting_sources",
        "p.tool_policy",
        "p.executable",
        "p.configuration",
        "req.policy",
    ] {
        for op in ["=", "+=", "-=", "*=", "/=", "%=", "|=", "&=", "^=", "<<=", ">>="] {
            let src = format!("fn f() {{ {field} {op} 1; }}");
            assert_eq!(one("x.rs", &src).offenders.len(), 1, "{src}");
        }
    }
    for src in [
        "fn f(p: &mut ClaudeHostPolicy) { policy . cli_permission_mode = 2; }",
        "fn f(p: &mut ClaudeHostPolicy) { policy\n    .\n    cli_permission_mode\n    |= 2; }",
        "fn f(p: &mut ClaudeHostPolicy) { policy. permissions  += 3; }",
        "fn f(p: &mut ClaudeHostPolicy) { policy .permission_mode_switchable ^= true; }",
        "fn f(c: &mut ProviderCapabilities) { c . cli_auto_mode &= x; }",
        "fn f(p: &mut ClaudeHostPolicy) { p . setting_sources = None; }",
        "fn f(p: &mut ClaudeHostPolicy) { p.tool_policy\n    = None; }",
        "fn f(p: &mut ClaudeHostPolicy) { p.executable = 1; }",
        "fn f(p: &mut ClaudeHostPolicy) { p . configuration |= 1; }",
        "fn f(r: &mut CreateSessionRequest) { r .policy = None; }",
    ] {
        assert_eq!(one("x.rs", src).offenders.len(), 1, "{src}");
    }
    // Reading the field is not assigning it.
    for src in [
        "fn f(p: &ClaudeHostPolicy) -> bool { p.permissions == 3 }",
        "fn f(p: &ClaudeHostPolicy) -> bool { p . cli_permission_mode != 0 && p.permissions >= 1 }",
        "fn f(p: &ClaudeHostPolicy) -> bool { p.permissions <= 2 || p.cli_permission_mode >> 1 > 0 }",
        "fn f(p: &ClaudeHostPolicy) -> bool { p.setting_sources == q && p.executable != 1 && p.configuration == 0 }",
        "fn f(r: &CreateSessionRequest) -> bool { r.policy == None || r.tool_policy.is_none() }",
        "fn f(r: &R) -> usize { r.policy_count + r.configuration_len }",
    ] {
        assert!(
            one("x.rs", src).offenders.is_empty(),
            "{src}: {:?}",
            one("x.rs", src).offenders
        );
    }
}

/// The requested `Auto` under an alias of the enum, in the file that writes the alias and in any
/// other file when the alias is a re-export; a variant imported by name or by glob; a glob of the
/// defining module followed by a bare `Auto`; and `Self::Auto` inside an `impl` of the enum.
#[test]
fn the_requested_auto_cannot_hide_behind_an_alias_an_import_or_self() {
    for src in [
        "use crate::process::RequestedCliMode as R;\nfn f() -> R { R::Auto }",
        "use crate::{process::{RequestedCliMode as R}};\nfn f() -> R { R::Auto }",
        "use crate::process::RequestedCliMode::{self as R};\nfn f() -> R { R::Auto }",
        "type R = crate::process::RequestedCliMode;\nfn f() -> R { R::Auto }",
        "use crate::process::RequestedCliMode::*;\nfn f() -> RequestedCliMode { Auto }",
        "use crate::process::RequestedCliMode::{Auto};\nfn f() -> RequestedCliMode { Auto }",
        "use crate::process::RequestedCliMode::Auto as A;\nfn f() -> RequestedCliMode { A }",
        "use crate::process::*;\nfn f() -> RequestedCliMode { Auto }",
        "use super::*;\nfn f() -> RequestedCliMode { Auto }",
        "impl RequestedCliMode { fn a() -> Self { Self::Auto } }",
        "impl Default for RequestedCliMode { fn default() -> Self { Self::Auto } }",
        "impl crate::process::RequestedCliMode { fn a() -> Self { if true { Self::Auto } else { Self::Default } } }",
    ] {
        for file in ["x.rs", "providers/claude_sidecar/mod.rs"] {
            assert!(!one(file, src).offenders.is_empty(), "{file}: {src}");
        }
    }
    // A re-export in one file is an alias in all of them.
    let tree = BTreeMap::from([
        (
            "lib.rs".to_string(),
            "pub use process::RequestedCliMode as Requested;".to_string(),
        ),
        (
            "x.rs".to_string(),
            "fn f() -> crate::Requested { crate::Requested::Auto }".to_string(),
        ),
    ]);
    let report = scan_tree(&tree);
    assert_eq!(report.offenders.len(), 1, "{:?}", report.offenders);
    assert!(report.offenders[0].starts_with("x.rs"), "{:?}", report.offenders);
    // Nothing else is caught by these: a glob with no bare `Auto`, an alias used for `Default`,
    // `Self::Auto` in another type's `impl`, and the glob of a module outside the crate.
    for src in [
        "use crate::process::*;\nfn f() -> RequestedCliMode { RequestedCliMode::Default }",
        "use crate::process::RequestedCliMode as R;\nfn f() -> R { R::Default }",
        "impl CliModeReport { fn a() -> Self { Self::Auto } }",
        "impl CliModeReport { fn a() -> Self { Self::Auto } }\nimpl Other for RequestedCliMode { fn b() {} }",
        "use claude_runtime_protocol::v1::*;\nfn f() -> bool { Auto == 1 }",
    ] {
        assert!(
            one("x.rs", src).offenders.is_empty(),
            "{src}: {:?}",
            one("x.rs", src).offenders
        );
    }
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
    // A `..` base could carry anything -- here an unstated CLI mode too, rule 6's own offence.
    let spread = "fn p(b: ClaudeHostPolicy) -> ClaudeHostPolicy { ClaudeHostPolicy { permissions: \
                  claude_runtime_protocol::v1::PermissionMode::Interactive as i32, \
                  permission_mode_switchable: false, ..b } }";
    let spread = spread.replace("..b", &format!("{COMPLETE} ..b"));
    let report = one("x.rs", &spread);
    assert_eq!(report.offenders.len(), 2, "{:?}", report.offenders);
    assert!(report.offenders[0].contains("..b"), "{:?}", report.offenders);
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
    assert_eq!(one("x.rs", "Policy::set_permissions(&mut p, m)").offenders.len(), 1);
    assert_eq!(
        one("x.rs", "crate::std::fs::set_permissions(&mut p, m)")
            .offenders
            .len(),
        1
    );
    assert_eq!(one("x.rs", "my_std::fs::set_permissions(&mut p, m)").offenders.len(), 1);
    assert_eq!(
        one("x.rs", "crate:: std::fs::set_permissions(&mut p, m)")
            .offenders
            .len(),
        1
    );
    // The standard library's file-mode call is not the policy's setter.
    let chmod = "fn f(p: &Path) { std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).unwrap(); }";
    assert!(
        one("x.rs", chmod).offenders.is_empty(),
        "{:?}",
        one("x.rs", chmod).offenders
    );
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
    // A file a test module pulls in with `include!` is test code; one production code also includes is scanned.
    let included = BTreeMap::from([
        (
            "lib.rs".to_string(),
            "mod a;\ninclude!(\"generated.rs\");\n#[cfg(test)]\nmod t {\n    include!(\"generated.rs\");\n}\n"
                .to_string(),
        ),
        (
            "a/mod.rs".to_string(),
            "fn f() {}\n#[cfg(test)]\nmod tests {\n    include!(\"cases.rs\");\n}\n".to_string(),
        ),
        (
            "a/cases.rs".to_string(),
            "const M: &str = \"bypassPermissions\";".to_string(),
        ),
        (
            "generated.rs".to_string(),
            "const M: &str = \"bypassPermissions\";".to_string(),
        ),
    ]);
    let report = scan_tree(&included);
    assert_eq!(report.skipped, BTreeSet::from(["a/cases.rs".to_string()]));
    // Production reaching the test-included file in any other spelling keeps it scanned.
    for production in [
        "#[path = \"cases.rs\"] mod fixtures;",
        "include!(\"./cases.rs\");",
        "include!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/src/a/cases.rs\"));",
        "include![\"cases.rs\"];",
        "include!(r\"cases.rs\");",
    ] {
        let mut tree = included.clone();
        tree.insert(
            "a/mod.rs".to_string(),
            format!("{production}\nfn f() {{}}\n#[cfg(test)]\nmod tests {{\n    include!(\"cases.rs\");\n}}\n"),
        );
        let report = scan_tree(&tree);
        assert!(report.skipped.is_empty(), "{production}: {:?}", report.skipped);
        assert!(
            report.offenders.iter().any(|o| o.starts_with("a/cases.rs")),
            "{production}: {:?}",
            report.offenders
        );
    }
    assert_eq!(report.offenders.len(), 1, "{:?}", report.offenders);
    assert!(
        report.offenders[0].starts_with("generated.rs"),
        "{:?}",
        report.offenders
    );
}

/// The Bash input key the permission policy reads passes only as that exact literal, in that file,
/// once; the flags and the mode the guard exists for are still caught there and everywhere else.
#[test]
fn the_allowlisted_input_key_passes_only_as_itself_in_its_own_file() {
    let read = "fn f(key: &str) -> bool { key == \"dangerouslyDisableSandbox\" }\n";
    let report = one(ALLOWED_INPUT_KEY.0, read);
    assert!(report.offenders.is_empty(), "{:?}", report.offenders);
    assert_eq!(report.input_key_reads, 1);
    // The same literal in any other file fails.
    assert_eq!(one("process.rs", read).offenders.len(), 1);
    // A second copy in the right file fails.
    let twice = format!("{read}fn g(k: &str) -> bool {{ k == \"dangerouslyDisableSandbox\" }}\n");
    assert_eq!(one(ALLOWED_INPUT_KEY.0, &twice).offenders.len(), 1);
    // Another spelling of the word in the right file fails: the flag, the SDK option, the key unquoted.
    for text in [
        "const A: &str = \"--dangerously-skip-permissions\";",
        "const A: &str = \"--allow-dangerously-skip-permissions\";",
        "const A: &str = \"allowDangerouslySkipPermissions\";",
        "fn f(i: &I) -> bool { i.dangerouslyDisableSandbox }",
        "const A: &str = \"DangerouslyDisableSandbox\";",
    ] {
        let report = one(ALLOWED_INPUT_KEY.0, &format!("{read}{text}\n"));
        assert_eq!(report.offenders.len(), 1, "{text}: {:?}", report.offenders);
    }
    // And the allowed read does not hide a flag or the ungated mode beside it.
    let beside =
        format!("{read}const B: &str = \"--dangerously-skip-permissions\"; const C: &str = \"bypassPermissions\";\n");
    assert_eq!(one(ALLOWED_INPUT_KEY.0, &beside).offenders.len(), 2);
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
