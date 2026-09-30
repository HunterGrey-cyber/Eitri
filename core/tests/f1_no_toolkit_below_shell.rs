//! Fitness function F1 (2026-09-27 Codex audit P6, `the private review notes`
//! appendix, verdict SUPPORTED in `the private review notes`):
//! "Resolved dependency graphs for core, agent and terminal contain no GTK/GDK/GLib/WebKit packages".
//! K3's own check (`the private review notes`) ran
//! `cargo tree -p {eitri-core,agent,eitri-terminal} --locked --offline -e normal --prefix none`
//! by hand; this is the same check, as a real test, extended to all seven crates the fitness
//! function's brief names: `eitri-core`, `agent`, `eitri-terminal`, `terminal-render`,
//! `terminal-frame`, `terminal-sync`, `terminal-input`.
//!
//! For each target crate, runs `cargo tree -p <crate> -e normal --offline --locked --prefix none`
//! (one subprocess per crate, matching K3's own by-hand invocation exactly) and fails if any line's
//! package name contains one of gtk / gdk / glib / gio / gobject / webkit / javascriptcore / gsk /
//! pango / cairo (case-insensitive). That substring match is deliberately broad: it also catches
//! every `*-sys` crate of each (`gdk-pixbuf-sys`, `cairo-sys-rs`, `webkit2gtk-sys`, ...) without
//! naming them one by one. `-e normal` walks the NORMAL-dependency closure only (a build- or
//! dev-only edge is not followed); offline: no network, resolves only from the checked-in
//! `Cargo.lock`; locked: fails rather than silently updating that lock file.
//!
//! **Deliberately per-crate `cargo tree -p`, not whole-workspace `cargo metadata`** (2026-09-28 fix
//! round, a Codex finding on the first version of this test): `cargo metadata` has no `-p` flag at
//! all, so producing its `resolve.nodes` graph -- even filtered down to these seven crates
//! afterward, as the first version of this test did -- requires cargo to have already read the
//! manifest of every package reachable from EVERY workspace member, including `shell`'s own
//! `gtk4`/`webkit6`. On a machine whose registry cache holds only what `cargo build -p
//! eitri-core` itself needed -- precisely the macOS M2 scenario this fitness function exists for
//! (`docs/superpowers/specs/2026-09-16-macos-path-design.md`) -- whole-workspace `cargo metadata
//! --offline --locked` fails with an unrelated-looking "failed to download `ab_glyph`... --offline
//! was specified" before this test's own GTK check ever runs, so the very host it is meant to
//! protect could never get a real answer from it. Reproduced 2026-09-28 with an isolated
//! `CARGO_HOME` fetched only via `cargo build -p eitri-core`: whole-workspace `cargo metadata
//! --offline --locked` failed exactly that way against it, while `cargo tree -p eitri-core -e
//! normal --offline --locked` against the SAME limited cache succeeded -- `cargo tree -p` resolves
//! only the named package's own closure and does not need the rest of the workspace fetched.
//!
//! Skips (does not fail) only when the cargo binary that built this test binary (`env!("CARGO")`,
//! embedded at compile time rather than searched for on `PATH` at runtime -- another 2026-09-28 fix
//! round finding: a runner that invokes this compiled test binary with a restricted `PATH` would
//! otherwise report a silent, false pass on a real GTK violation) cannot be run at all -- a
//! different condition from "cargo ran and found a violation" or "cargo ran and errored" (a stale
//! lock file under `--locked`, for instance, or a target crate that no longer exists), both of which
//! this test still fails loudly on.

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
        "F1 (\"no toolkit below the shell\"): the normal-dependency closure of a toolkit-free crate \
         reaches a GTK/GDK/GLib/WebKit package:\n{}\n\
         eitri-core, agent, eitri-terminal and the four terminal-{{render,frame,sync,input}} \
         crates must build and link on a host with no GTK (the macOS track's M1/M2 depend on this; \
         docs/superpowers/specs/2026-09-16-macos-path-design.md).",
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
