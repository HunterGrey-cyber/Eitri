#![cfg(target_os = "linux")]
//! Words the listener's side of Eitri must never hold, read as text, and where its wiring may stand.
//!
//! The word scans read the raw text, comments and literals in, where `code_only` would blank a literal: both words are
//! strings on the wire, so a literal is exactly how either would get there. Each is spelled here in two pieces, so that
//! searching the tree for it finds no false hit in this file.

use eitri_core::ide_server::PORT_VAR;
use eitri_core::source_scan::{code_only, read_required, require_anchors, rust_sources, sources, Source};

/// An answer the terminal client reads as "allow the proposal unchanged": closing a view must never mean that.
const APPROVING_CLOSE: &str = concat!("TAB_", "CLOSED");

/// A notification the client turns into its own telemetry.
const CLIENT_LOG: &str = concat!("log_", "event");

/// The module, Lua included, and every shell file that names it, so the wiring is read wherever it comes to live.
fn listener_sources() -> Vec<Source> {
    let mut read = sources(&["core/src/ide_server"], &["rs", "lua"]);
    require_anchors(&read, &["IdeHost"]);
    assert!(
        read.iter().any(|source| source.path.ends_with("ide_view.lua")),
        "the view's Lua is read too"
    );
    read.extend(
        sources(&["shell/src"], &["rs"])
            .into_iter()
            .filter(|source| source.text.contains("ide_server")),
    );
    read
}

/// The paths of the files holding `word` anywhere in their text.
fn holding(read: &[Source], word: &str) -> Vec<String> {
    read.iter()
        .filter(|source| source.text.contains(word))
        .map(|source| source.path.clone())
        .collect()
}

fn source(text: String) -> Source {
    Source {
        path: "core/src/ide_server/host.rs".to_owned(),
        text,
    }
}

#[test]
fn no_source_names_the_approving_close_answer() {
    assert_eq!(holding(&listener_sources(), APPROVING_CLOSE), Vec::<String>::new());
}

#[test]
fn no_source_builds_log_event() {
    assert_eq!(holding(&listener_sources(), CLIENT_LOG), Vec::<String>::new());
}

#[test]
fn the_scan_sees_a_word_in_a_literal_and_in_a_comment() {
    for word in [APPROVING_CLOSE, CLIENT_LOG] {
        let literal = source(format!("const ANSWER: &str = \"{word}\";\n"));
        let comment = source(format!("// never send {word}\nfn answer() {{}}\n"));
        let clean = source("fn answer() {}\n".to_owned());
        let hits = holding(&[literal, comment, clean], word);
        assert_eq!(hits.len(), 2, "{word}: {hits:?}");
    }
}

/// A file's text before its unit tests. A `#[cfg(test)]` higher up gates one line, not the rest of the file.
fn before_tests(text: &str) -> &str {
    let end = ["#[cfg(test)]\nmod tests", "#[cfg(test)]\npub(crate) mod tests"]
        .iter()
        .filter_map(|marker| text.find(marker))
        .min()
        .unwrap_or(text.len());
    &text[..end]
}

/// The port is set on the terminal's shells through its one constant, so product code that spells the variable out is
/// code setting it somewhere else: on the sidecar, or on the nvim child.
#[test]
fn the_port_variable_is_named_only_in_its_module() {
    let read = rust_sources(&["agent/src", "core/src", "shell/src", "panel/src", "terminal/engine/src"]);
    require_anchors(
        &read,
        &["fn build_ui(", "fn sidecar_command_for", "fn child_environment_with"],
    );
    let naming: Vec<&str> = read
        .iter()
        .filter(|source| before_tests(&source.text).contains(PORT_VAR))
        .map(|source| source.path.as_str())
        .collect();
    assert_eq!(naming, vec!["core/src/ide_server/types.rs"]);
}

/// A companion window has no terminal and its editor is the person's own nvim, so the key means nothing there.
#[test]
fn the_companion_window_names_no_ide_wiring() {
    let read = rust_sources(&["shell/src/companion"]);
    require_anchors(&read, &["fn run_split"]);
    for word in [
        "ide_wiring",
        "IdeWiring",
        "ide_server",
        "TERMINAL_CLAUDE_IDE_KEY",
        "ShellPort",
    ] {
        assert_eq!(holding(&read, word), Vec::<String>::new(), "{word}");
    }
}

const LINUX_GATE: &str = "#[cfg(target_os = \"linux\")]";

/// Whether the attribute stands on the line above `at`, or above a line `at` is nested in.
fn behind_the_gate(lines: &[&str], mut at: usize) -> bool {
    let indent = |line: &str| line.len() - line.trim_start().len();
    loop {
        if at > 0 && lines[at - 1].trim() == LINUX_GATE {
            return true;
        }
        let outer = (0..at)
            .rev()
            .find(|line| !lines[*line].trim().is_empty() && indent(lines[*line]) < indent(lines[at]));
        match outer {
            Some(outer) => at = outer,
            None => return false,
        }
    }
}

/// The lines of `text` naming the wiring, in code, with no gate over them.
fn ungated(text: &str) -> Vec<String> {
    let code = code_only(text);
    let code: Vec<&str> = code.lines().collect();
    let lines: Vec<&str> = text.lines().collect();
    (0..lines.len())
        .filter(|at| {
            [
                "ide_wiring",
                "IdeWiring",
                "ShellPort",
                "shell_port",
                "claude_ide",
                "ide.end",
            ]
            .iter()
            .any(|word| code[*at].contains(word))
        })
        .filter(|at| !behind_the_gate(&lines, *at))
        .map(|at| lines[at].trim().to_owned())
        .collect()
}

#[test]
fn the_wiring_is_behind_the_linux_gate() {
    for (path, anchor) in [
        ("shell/src/main.rs", "mod ide_wiring;"),
        ("shell/src/gtk_host/services.rs", "crate::ide_wiring::IdeWiring"),
    ] {
        let text = read_required(path).text;
        let product = before_tests(&text);
        assert!(product.contains(anchor), "{path} no longer has the wiring");
        assert_eq!(ungated(product), Vec::<String>::new(), "{path}");
    }
    for path in ["shell/src/launch.rs", "shell/src/window_services.rs"] {
        let text = read_required(path).text;
        assert_eq!(ungated(before_tests(&text)), Vec::<String>::new(), "{path}");
    }
}

#[test]
fn the_gate_check_sees_a_use_with_no_gate() {
    let gated = [
        LINUX_GATE,
        "mod ide_wiring;",
        "fn a() {",
        &format!("    {LINUX_GATE}"),
        "    if b {",
        "        ide_wiring.get();",
        "    }",
        &format!("    {LINUX_GATE}"),
        "    let c = d",
        "        .e(ide_wiring);",
        "}",
    ];
    assert_eq!(ungated(&gated.join("\n")), Vec::<String>::new());
    let bare = [
        "fn a() {",
        &format!("    {LINUX_GATE}"),
        "    let c = 1;",
        "    ide_wiring.get();",
        "}",
    ];
    assert_eq!(ungated(&bare.join("\n")), vec!["ide_wiring.get();".to_owned()]);
    assert_eq!(ungated("// ide_wiring\nfn a() {}\n"), Vec::<String>::new());
}
