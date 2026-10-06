//! Eitri never has nvim force-quit (the round-4 ruling of the v1-hardening fix rounds, 2026-09-27).
//! Ending an editor is only: nvim's own `:confirm qall` (it asks the user); closing its stdin and
//! waiting (on EOF nvim exits keeping the swap files of modified buffers); SIGTERM, which nvim's
//! own deadly-signal handler answers the same way; and SIGKILL by the pid Eitri holds, only when
//! those did not end it within the patience window (`neovide_editor`'s `nvim_child::end`).
//!
//! `:qa!` discards unsaved buffers AND deletes their swap files, and the pinned fork's own shutdown
//! used to send it (`ParallelCommand::Quit` with `confirm_quit` forced off). This scans what could
//! send one: every string literal in the product crates' non-test Rust, every product Lua file, and
//! the pinned fork's lifecycle code, located through `cargo metadata` so it is the revision this
//! workspace actually builds. The behaviour half is `nvim_child`'s real-nvim tests.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Ex commands that end nvim without asking: `:q!`, `:qa!`, `:wq!`, `:x!` and their spellings, and
/// `:cq`, which quits without writing whether or not it has a bang.
const BANGED: &[&str] = &[
    "q", "qu", "qui", "quit", "qa", "qal", "qall", "quita", "quitall", "wq", "wqa", "wqal", "wqall", "x", "xi", "xit",
    "xa", "xal", "xall", "exi", "exit", "wn", "wN",
];
const ALWAYS: &[&str] = &["cq", "cqu", "cqui", "cquit"];

fn force_quits(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let word_at = |end: usize| {
        let start = bytes[..end]
            .iter()
            .rposition(|b| !b.is_ascii_alphabetic())
            .map_or(0, |p| p + 1);
        let boundary = start == 0 || !(bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_');
        (start, boundary)
    };
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'!' {
            let (start, boundary) = word_at(i);
            let word = &text[start..i];
            if boundary && BANGED.contains(&word) {
                found.push(format!("{word}!"));
            }
        }
    }
    let mut start = 0;
    for token in text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        if ALWAYS.contains(&token) {
            found.push(token.to_string());
        }
        start += token.len() + 1;
    }
    let _ = start;
    found
}

/// Rust source read as the compiler reads it (round 5, codex finding 3: splitting on `"` missed a
/// raw string holding quotes and an escaped quote): the contents of every string literal -- plain,
/// raw (`r#"…"#`), byte and C strings, the escapes of all but raw ones decoded ([`unescape`], the
/// quit follow-up) -- with where each starts, and the code with every comment
/// (nested block comments too), literal and char literal blanked out, byte for byte, so offsets
/// agree. Lifetimes are told from char literals by the closing quote.
struct Lexed {
    literals: Vec<(usize, String)>,
    code: String,
}

fn lex_rust(text: &str) -> Lexed {
    let b = text.as_bytes();
    let mut code = b.to_vec();
    let mut literals = Vec::new();
    let blank = |code: &mut Vec<u8>, from: usize, to: usize| {
        for c in &mut code[from..to.min(b.len())] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    };
    let find = |from: usize, needle: &[u8]| {
        b[from.min(b.len())..]
            .windows(needle.len())
            .position(|w| w == needle)
            .map_or(b.len(), |p| from + p)
    };
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            let end = find(i, b"\n");
            blank(&mut code, i, end);
            i = end;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let (mut depth, mut j) = (1, i + 2);
            while j < b.len() && depth > 0 {
                if b[j] == b'/' && b.get(j + 1) == Some(&b'*') {
                    depth += 1;
                    j += 2;
                } else if b[j] == b'*' && b.get(j + 1) == Some(&b'/') {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            blank(&mut code, i, j);
            i = j;
            continue;
        }
        let word_before = i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
        if !word_before && (c == b'b' || c == b'c' || c == b'r' || c == b'"') {
            let prefix = usize::from(c == b'b' || c == b'c');
            let raw = b.get(i + prefix) == Some(&b'r');
            let mut k = i + prefix + usize::from(raw);
            let mut hashes = 0;
            while raw && b.get(k) == Some(&b'#') {
                hashes += 1;
                k += 1;
            }
            if b.get(k) == Some(&b'"') {
                let start = k + 1;
                let end = if raw {
                    let mut closing = vec![b'"'];
                    closing.extend(std::iter::repeat_n(b'#', hashes));
                    let end = find(start, &closing);
                    literals.push((i, text[start..end].to_string()));
                    end + closing.len()
                } else {
                    let mut j = start;
                    while j < b.len() && b[j] != b'"' {
                        j += if b[j] == b'\\' { 2 } else { 1 };
                    }
                    literals.push((i, unescape(&text[start..j.min(b.len())])));
                    j + 1
                };
                blank(&mut code, i, end);
                i = end;
                continue;
            }
        }
        if c == b'\'' {
            if b.get(i + 1) == Some(&b'\\') {
                let end = find(i + 3, b"'") + 1;
                blank(&mut code, i, end);
                i = end;
                continue;
            }
            let width = text[i + 1..].chars().next().map_or(1, char::len_utf8);
            if b.get(i + 1 + width) == Some(&b'\'') {
                blank(&mut code, i, i + 2 + width);
                i += 2 + width;
                continue;
            }
        }
        i += 1;
    }
    Lexed {
        literals,
        code: String::from_utf8(code).expect("blanking keeps UTF-8"),
    }
}

/// A plain, byte or C string literal's body as the compiler decodes it (the quit follow-up, codex
/// finding 2 of round 5's review: `"qa\x21"` is `qa!` to nvim, and the matcher used to see the
/// source spelling). `\n` `\r` `\t` `\\` `\0` `\'` `\"`, `\x` with two hex digits, `\u{…}` (hex,
/// underscores allowed), and a backslash ending a line, which drops the line break and the
/// whitespace after it. Anything else is kept as written. Raw strings never reach this: they have
/// no escapes.
fn unescape(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('0') => out.push('\0'),
            Some(q @ ('\\' | '\'' | '"')) => out.push(q),
            Some('x') => {
                let hex: String = (0..2).filter_map(|_| chars.next_if(char::is_ascii_hexdigit)).collect();
                match u8::from_str_radix(&hex, 16) {
                    // A byte string's `\x80`..`\xff` is one byte, not a char: as Latin-1 it cannot
                    // spell an ASCII command, which is all the matcher looks for.
                    Ok(byte) if hex.len() == 2 => out.push(char::from(byte)),
                    _ => {
                        out.push_str("\\x");
                        out.push_str(&hex);
                    }
                }
            }
            Some('u') if chars.peek() == Some(&'{') => {
                chars.next();
                let mut hex = String::new();
                while let Some(d) = chars.next_if(|d| *d != '}') {
                    hex.push(d);
                }
                let closed = chars.next_if_eq(&'}').is_some();
                let digits: String = hex.chars().filter(|d| *d != '_').collect();
                match u32::from_str_radix(&digits, 16).ok().and_then(char::from_u32) {
                    Some(decoded) if closed => out.push(decoded),
                    _ => {
                        out.push_str("\\u{");
                        out.push_str(&hex);
                        if closed {
                            out.push('}');
                        }
                    }
                }
            }
            Some('\n') => while chars.next_if(|w| w.is_whitespace()).is_some() {},
            Some('\r') if chars.peek() == Some(&'\n') => {
                chars.next();
                while chars.next_if(|w| w.is_whitespace()).is_some() {}
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Where the test code starts: the first `#[cfg(test)]` or `#[cfg(all(test` in real code.
fn test_start(code: &str) -> usize {
    ["#[cfg(test)]", "#[cfg(all(test"]
        .iter()
        .filter_map(|marker| code.find(marker))
        .min()
        .unwrap_or(code.len())
}

/// A Rust file's code before its test module, comments and literals blanked out.
fn rust_code(text: &str) -> String {
    let lexed = lex_rust(text);
    let cut = test_start(&lexed.code);
    lexed.code[..cut].to_string()
}

fn lua_code(text: &str) -> String {
    text.lines()
        .map(|line| line.split("--").next().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.expect("a readable entry").path();
        if path.is_dir() {
            files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs" || e == "lua") {
            out.push(path);
        }
    }
}

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the workspace root")
        .to_path_buf()
}

/// Every force-quit in `path`'s product code: in a Rust file its string literals, in a Lua file its
/// code; and in Rust code, the fork's own quit command named at all.
fn offences(path: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).expect("a readable source file");
    let mut hits = Vec::new();
    if path.extension().is_some_and(|e| e == "lua") {
        hits.extend(force_quits(&lua_code(&text)));
    } else {
        hits.extend(rust_offences(&text));
    }
    hits.into_iter()
        .map(|hit| format!("{}: {hit}", path.display()))
        .collect()
}

/// The force-quits in a Rust file's product code: in its string literals, and `ParallelCommand::
/// Quit` named in its code.
fn rust_offences(text: &str) -> Vec<String> {
    let lexed = lex_rust(text);
    let cut = test_start(&lexed.code);
    let mut hits: Vec<String> = lexed
        .literals
        .iter()
        .filter(|(at, _)| *at < cut)
        .flat_map(|(_, literal)| force_quits(literal))
        .collect();
    if lexed.code[..cut].contains("ParallelCommand::Quit") {
        hits.push("ParallelCommand::Quit".into());
    }
    hits
}

/// Round 5, codex finding 3: every literal Rust can spell, found as the compiler reads it -- a raw
/// string holding quotes, an escaped quote, a byte string, a `'"'` char before a string, lifetimes
/// -- and nothing in a comment, a nested block comment or the test module.
#[test]
fn every_rust_literal_is_read_as_the_compiler_reads_it() {
    let found = |src: &str| rust_offences(src);
    assert_eq!(
        found(r####"fn f() { g(r#"vim.cmd("qa!")"#); }"####),
        ["qa!"],
        "raw string holding quotes"
    );
    assert_eq!(
        found(r#"fn f() { g("a \"x\" qa! b"); }"#),
        ["qa!"],
        "escaped quotes before"
    );
    assert_eq!(
        found(r#"fn f() { g("a \"qa!\" b"); }"#),
        ["qa!"],
        "escaped quotes around"
    );
    assert_eq!(
        found(r#"fn f() { g("\\"); h("qa!"); }"#),
        ["qa!"],
        "an escaped backslash last"
    );
    assert_eq!(
        found(r#"fn f() { let q = '"'; g("qa!"); }"#),
        ["qa!"],
        "a quote char literal"
    );
    assert_eq!(
        found(r#"fn f() { let q = '"'; let r = '"'; g("x"); } // "qa!""#),
        Vec::<String>::new()
    );
    assert_eq!(
        found(r#"fn f<'a>(x: &'a str) -> &'a str { "qa!" }"#),
        ["qa!"],
        "lifetimes"
    );
    assert_eq!(found(r#"const B: &[u8] = b"qall!";"#), ["qall!"], "a byte string");
    assert_eq!(found(r##"const B: &[u8] = br#"cq"#;"##), ["cq"], "a raw byte string");
    assert!(
        found(r#"/* "qa!" /* nested */ "qa!" */ fn f() {}"#).is_empty(),
        "block comments"
    );
    assert!(found(
        "/// `:qa!` in a doc comment
fn f() {}"
    )
    .is_empty());
    assert!(found(
        "fn f() {}
#[cfg(test)]
mod tests { const X: &str = \"qa!\"; }"
    )
    .is_empty());
    assert_eq!(
        found("fn f() { send_ui(ParallelCommand::Quit, h); }"),
        ["ParallelCommand::Quit"]
    );
    assert!(
        found(r#"fn f() { g("ParallelCommand::Quit is gone"); }"#).is_empty(),
        "named in a string"
    );
}

/// The quit follow-up, codex finding 2 of round 5's review: a force-quit spelt with escapes is the
/// force-quit Rust hands nvim -- `\x`, `\u{…}` (in the bang or in the command name), a line
/// continuation, in plain, byte and C strings -- while a raw string, or an escaped backslash, keeps
/// the backslash nvim then sees, which is no force-quit at all.
#[test]
fn an_escaped_force_quit_is_read_as_the_compiler_decodes_it() {
    let found = |src: &str| rust_offences(src);
    assert_eq!(
        found(r#"fn f() { pane.exec_lua_watched("vim.cmd('qa\x21')"); }"#),
        ["qa!"],
        "codex's own case"
    );
    assert_eq!(
        found(r#"fn f() { g("\x71a!"); }"#),
        ["qa!"],
        "an escape in the command name"
    );
    assert_eq!(
        found(r#"fn f() { g("q\u{61}ll\u{21}"); }"#),
        ["qall!"],
        "unicode escapes"
    );
    assert_eq!(
        found(r#"fn f() { g("qa\u{0_0_21}"); }"#),
        ["qa!"],
        "underscores in a unicode escape"
    );
    assert_eq!(found(r#"fn f() { g("\x63q"); }"#), ["cq"], "cq needs no bang");
    assert_eq!(found(r#"fn f() { g("\u{63}\u{71}"); }"#), ["cq"]);
    assert_eq!(
        found("fn f() { g(\"q\\\n        a!\"); }"),
        ["qa!"],
        "a line continuation drops the break and the indent"
    );
    assert_eq!(found(r#"const B: &[u8] = b"wq\x21";"#), ["wq!"], "a byte string");
    assert_eq!(found(r#"const C: &CStr = c"qa\x21";"#), ["qa!"], "a C string");
    assert_eq!(
        found(r#"fn f() { g("a \"\x71a!\" b"); }"#),
        ["qa!"],
        "an escape next to escaped quotes"
    );
    assert!(
        found(r#"fn f() { g(r"qa\x21"); }"#).is_empty(),
        "a raw string has no escapes"
    );
    assert!(
        found(r#"fn f() { g("qa\\x21"); }"#).is_empty(),
        "an escaped backslash: nvim gets `qa\\x21`"
    );
    assert!(
        found(r#"fn f() { g("qa\x2"); }"#).is_empty(),
        "half an escape decodes to nothing"
    );
    assert_eq!(unescape(r"a\tb\n\0\'\\"), "a\tb\n\0'\\");
    assert_eq!(unescape(r"\q\u{zz}\u{21"), r"\q\u{zz}\u{21", "kept as written");
}

#[test]
fn the_matcher_sees_every_spelling_and_nothing_else() {
    assert_eq!(force_quits("vim.cmd('qa!')"), ["qa!"]);
    assert_eq!(force_quits("<Cmd>qall!<CR>"), ["qall!"]);
    assert_eq!(force_quits(":wq!"), ["wq!"]);
    assert_eq!(force_quits("silent! cq"), ["cq"]);
    assert!(force_quits("confirm qall").is_empty());
    assert!(
        force_quits("Quit! nothing? equal! acquire squash!").is_empty(),
        "{:?}",
        force_quits("equal! acquire")
    );
    assert!(force_quits("pcall(vim.cmd, 'confirm qall')").is_empty());
}

/// The product crates' source directories this scan reads.
const SCANNED: &[&str] = &[
    "shell/src",
    "panel/src",
    "mac/src",
    "core/src",
    "neovide-editor/src",
    "agent/src",
    "terminal/src",
    "supervisor/src",
];

/// Product crates whose sources this scan does not read, each with why. Copied terminal drawing
/// libraries never start, drive or stop an nvim.
const NOT_SCANNED: &[(&str, &str)] = &[
    ("terminal-frame", "a terminal drawing library; it never talks to nvim"),
    ("terminal-input", "a key encoder; it never talks to nvim"),
    ("terminal-render", "a terminal drawing library; it never talks to nvim"),
    ("terminal-sync", "a terminal drawing library; it never talks to nvim"),
];

/// The `default-members` of the workspace manifest, i.e. the product crates.
fn default_members() -> Vec<String> {
    let manifest = std::fs::read_to_string(workspace().join("Cargo.toml")).expect("the workspace manifest");
    let start = manifest
        .find("default-members = [")
        .expect("default-members in the workspace manifest");
    let list = &manifest[start..];
    let list = &list[list.find('[').unwrap() + 1..list.find(']').expect("default-members closes")];
    let members: Vec<String> = list
        .lines()
        .map(|l| {
            l.split('#')
                .next()
                .unwrap()
                .trim()
                .trim_end_matches(',')
                .trim_matches('"')
                .to_string()
        })
        .filter(|m| !m.is_empty())
        .collect();
    assert!(members.len() >= 5, "default-members parsed to {members:?}");
    members
}

#[test]
fn every_product_crate_is_scanned_or_exempt() {
    let members = default_members();
    for member in &members {
        let scanned = SCANNED.contains(&format!("{member}/src").as_str());
        let exempt = NOT_SCANNED.iter().any(|(name, _)| name == member);
        assert!(
            scanned || exempt,
            "{member} is a product crate this scan does not read; add `{member}/src` to SCANNED"
        );
    }
    for (name, _) in NOT_SCANNED {
        assert!(
            members.iter().any(|m| m == name),
            "NOT_SCANNED names {name}, which is no longer a product crate"
        );
    }
}

/// Eitri's own product code sends no force-quit to nvim.
#[test]
fn no_product_code_force_quits_nvim() {
    let root = workspace();
    let mut paths = Vec::new();
    for crate_src in SCANNED {
        files(&root.join(crate_src), &mut paths);
    }
    assert!(paths.len() > 50, "only {} files -- the walk is broken", paths.len());
    let offences: Vec<String> = paths.iter().flat_map(|p| offences(p)).collect();
    assert_eq!(offences, Vec::<String>::new(), "Eitri never force-quits nvim");
}

/// The pinned fork's root, as cargo resolved it for this workspace.
fn fork_root() -> PathBuf {
    let output = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--offline",
            "--locked",
            "--manifest-path",
        ])
        .arg(workspace().join("Cargo.toml"))
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).expect("metadata is JSON");
    let package = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .find(|p| p["name"] == "neovide")
        .expect("the neovide fork is a dependency");
    Path::new(package["manifest_path"].as_str().expect("a manifest path"))
        .parent()
        .expect("its directory")
        .to_path_buf()
}

/// The fork's lifecycle code: the harness Eitri embeds never sends nvim the fork's quit, which is
/// `:qa!` with `confirm_quit` off; the one quit nvim itself can still trigger there (`<D-q>`, from
/// the fork's `lua/init.lua`) confirms; and the only `qa!` left in the fork's Lua is stock
/// Neovide's, which an embedding never reaches.
#[test]
fn the_pinned_forks_harness_never_force_quits_nvim() {
    let fork = fork_root();
    let read = |rel: &str| std::fs::read_to_string(fork.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
    let harness = rust_code(&read("src/live_harness.rs"));
    assert!(
        !harness.contains("ParallelCommand::Quit"),
        "LiveHarness sends ParallelCommand::Quit ({})",
        fork.display()
    );
    assert!(
        harness.contains("hang_up_neovim()"),
        "LiveHarness ends nvim by closing its stdin"
    );
    assert!(
        harness.contains("NEVER_FORCE_QUIT.store(true"),
        "LiveHarness makes every Quit confirm"
    );
    let ui_commands = rust_code(&read("src/bridge/ui_commands.rs"));
    assert!(
        ui_commands.contains("NEVER_FORCE_QUIT.load"),
        "the Quit command passes the embedding's rule"
    );
    let exit_handler = lua_code(&read("lua/exit_handler.lua"));
    assert!(
        exit_handler.contains("always_confirm or"),
        "the exit handler confirms when the embedding says so"
    );
    let mut lua = Vec::new();
    files(&fork.join("lua"), &mut lua);
    let offences: Vec<String> = lua.iter().flat_map(|p| offences(p)).collect();
    assert_eq!(
        offences,
        vec![format!("{}: qa!", fork.join("lua/exit_handler.lua").display())],
        "stock Neovide's own unconfirmed quit is the only one left"
    );
}
