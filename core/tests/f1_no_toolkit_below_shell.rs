//! The normal dependency graphs of the toolkit-free crates must contain no GTK, GDK, GLib or
//! WebKit packages. This checks `eitri-core`, `eitri-remote`, `eitri-panel`, `eitri-mac`, `agent`,
//! `eitri-terminal`, `terminal-render`, `terminal-frame`, `terminal-sync` and `terminal-input`.
//!
//! Each crate gets its own `cargo tree -p <crate> -e normal --offline --locked --prefix none`.
//! A package name containing gtk, gdk, glib, gio, gobject, webkit, javascriptcore, gsk, pango or
//! cairo fails, case-insensitively. Substring matching catches the corresponding `*-sys`
//! packages without listing each. Normal edges exclude build and dev dependencies; offline
//! uses cached sources, and locked refuses lockfile changes.
//!
//! Per-crate queries also work with a cache populated only for a toolkit-free target, as on
//! macOS. Whole-workspace `cargo metadata` has no `-p` filter and must read dependencies of
//! all workspace members before its graph can be filtered, including shell's GTK and WebKit.
//! With only core's dependencies cached, it can fail on an unrelated missing package such as
//! `ab_glyph` before checking toolkit isolation. `cargo tree -p` needs only the selected
//! package's dependency closure. Reproduced 2026-09-28 with an isolated `CARGO_HOME` fetched only
//! through `cargo build -p eitri-core`: whole-workspace `cargo metadata --offline --locked` failed
//! that way against it, while `cargo tree -p eitri-core -e normal --offline --locked` against the
//! same limited cache succeeded.
//!
//! The test skips only if the exact Cargo binary that built it (`env!("CARGO")`) cannot run.
//! Embedding that path avoids silently skipping a violation when a runner restricts PATH.
//! If Cargo runs and reports a violation or an error, such as a stale lockfile or missing
//! target crate, the test fails.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BANNED_SUBSTRINGS: &[&str] = &[
    "gtk",
    "gdk",
    "glib",
    "gio",
    "gobject",
    "webkit",
    "javascriptcore",
    "gsk",
    "pango",
    "cairo",
];

const TARGET_CRATES: &[&str] = &[
    "eitri-core",
    "eitri-remote",
    "eitri-panel",
    "eitri-mac",
    "agent",
    "eitri-terminal",
    "terminal-render",
    "terminal-frame",
    "terminal-sync",
    "terminal-input",
];

/// The cargo binary that built this test binary, embedded at compile time via Cargo's own
/// documented `CARGO` build-time env var (the Cargo Book, "Environment Variables Cargo Sets for
/// Crates"). Spawning this exact path -- rather than searching `PATH` for the literal string
/// `"cargo"` at runtime -- means a runner that invokes the compiled test binary with a restricted
/// `PATH` still finds it; see this file's header.
const CARGO_BIN: &str = env!("CARGO");

fn workspace_manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("core/ has a parent directory (the workspace root)")
        .join("Cargo.toml")
}

fn is_banned(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    BANNED_SUBSTRINGS.iter().any(|s| lower.contains(s))
}

fn run_cargo_tree(manifest: &Path, crate_name: &str) -> std::io::Result<Output> {
    Command::new(CARGO_BIN)
        .args([
            "tree",
            "--offline",
            "--locked",
            "-e",
            "normal",
            "--prefix",
            "none",
            "--manifest-path",
        ])
        .arg(manifest)
        .args(["-p", crate_name])
        .output()
}

#[test]
fn no_toolkit_in_the_normal_dependency_closure_of_the_toolkit_free_crates() {
    let manifest = workspace_manifest();
    let mut failures: HashSet<String> = HashSet::new();
    for target in TARGET_CRATES {
        let output = match run_cargo_tree(&manifest, target) {
            Ok(output) => output,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "[f1_no_toolkit_below_shell] SKIPPED: {CARGO_BIN} is not runnable ({e}); cannot check the \
                     dependency graph"
                );
                return;
            }
            Err(e) => panic!(
                "failed to run `{CARGO_BIN} tree -p {target}` ({}): {e}",
                manifest.display()
            ),
        };
        assert!(
            output.status.success(),
            "`{CARGO_BIN} tree --offline --locked -e normal --prefix none --manifest-path {} -p {target}` failed \
             (exit {:?}):\n{}",
            manifest.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        // Each line is cargo tree's default `{p}` format with `--prefix none`: `<name> v<version>`,
        // optionally followed by `(<source-or-path>)` and/or `(*)` for a de-duplicated repeat. Only
        // the first whitespace-delimited token is the package name -- a local workspace member's
        // line also carries its checkout PATH in parentheses (e.g. this very repo's own path), which
        // must never be substring-matched: a checkout path containing "cairo" or "gtk" would
        // otherwise false-positive against BANNED_SUBSTRINGS.
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let Some(name) = line.split_whitespace().next() else {
                continue;
            };
            if is_banned(name) {
                failures.insert(format!("{target} -> {name}"));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "no toolkit below the shell: the normal-dependency closure of a toolkit-free crate \
         reaches a GTK/GDK/GLib/WebKit package:\n{}\n\
         eitri-core, agent, eitri-terminal and the four terminal-{{render,frame,sync,input}} \
         crates must build and link on a host with no GTK (the macOS host depends on this).",
        {
            let mut sorted: Vec<&String> = failures.iter().collect();
            sorted.sort();
            sorted.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n")
        }
    );
}

/// The banned-substring matcher itself, checked directly: every named GTK-family crate this project
/// has actually seen matches, and a sample of real, unrelated crate names this workspace also
/// resolves does not -- so a typo in `BANNED_SUBSTRINGS` that made it match nothing (or everything)
/// would fail this test even on a host where the real dependency graph happens to be clean.
#[test]
fn the_banned_substring_matcher_is_neither_empty_nor_too_broad() {
    for name in [
        "gtk4",
        "gtk4-sys",
        "gdk4",
        "gdk-pixbuf-sys",
        "glib",
        "glib-sys",
        "gio",
        "gobject-sys",
        "webkit6",
        "webkit2gtk-sys",
        "javascriptcore-rs-sys",
        "gsk4-sys",
        "pango-sys",
        "cairo-sys-rs",
    ] {
        assert!(is_banned(name), "{name} should match BANNED_SUBSTRINGS but did not");
    }
    for name in [
        "tokio",
        "serde",
        "prost",
        "tonic",
        "uuid",
        "libc",
        "mlua",
        "tower",
        "sha2",
        "alacritty_terminal",
    ] {
        assert!(!is_banned(name), "{name} should not match BANNED_SUBSTRINGS but did");
    }
}
