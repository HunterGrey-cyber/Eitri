//! Drives `ensure_web_bundle_built` (v1-dist plan Task 6, P4-A1) over a scratch copy of a small
//! fixture web dir, with a stub `npm` intercepted in its place -- never the real npm, no network.
//! See `../build_web.rs`'s own doc comment for why this is an `include!` of that file rather than
//! a dependency (`build.rs` is compiled as a wholly separate binary from `shell`), the same pattern
//! `build_info_lockfile_parsing.rs` already uses for `build_info_lockfile.rs`.
//!
//! Every case asserts exactly which npm commands the *second* call made, against a fixture that a
//! first call already fully installed and built (see [`Fixture::installed`]) -- that first call is
//! itself always `ci` then `run build` (nothing is installed yet), which is not one of the eight
//! cases the plan lists and is discarded before each test's own mutation and assertion.

include!("../build_web.rs");

// `fs` comes from `build_web.rs`'s own `use std::fs;` above (`include!` splices it into this same
// module scope, and item resolution in a module is not order-dependent) -- a second `use std::fs;`
// here would be the same duplicate-import error (E0252) `build_web.rs`'s own doc comment explains.
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A scratch directory under Cargo's own per-target-dir tmp (`CARGO_TARGET_TMPDIR`, rooted inside
/// this worktree's own `target/` -- never `/tmp`), named for the test that owns it so parallel
/// tests never collide, and removed (then recreated fresh) up front in case an earlier crashed run
/// left it behind, and again on drop so a panicking assertion still cleans up.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(label: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("web-bundle-freshness-{label}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        ScratchDir(dir)
    }
}

impl std::ops::Deref for ScratchDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_fixture(web_dir: &Path) {
    fs::write(
        web_dir.join("package.json"),
        "{\"name\":\"fixture\",\"version\":\"1.0.0\"}\n",
    )
    .unwrap();
    fs::write(
        web_dir.join("package-lock.json"),
        "{\"name\":\"fixture\",\"lockfileVersion\":3}\n",
    )
    .unwrap();
    fs::write(web_dir.join("index.html"), "<!doctype html><html></html>\n").unwrap();
    fs::write(web_dir.join("vite.config.ts"), "export default {};\n").unwrap();
    fs::write(web_dir.join("tsconfig.json"), "{\"compilerOptions\":{}}\n").unwrap();
    // A file this crate's build never watches -- must never appear in the npm log either.
    fs::write(web_dir.join("vitest.config.ts"), "export default {};\n").unwrap();
    fs::create_dir_all(web_dir.join("src/nested")).unwrap();
    fs::write(web_dir.join("src/main.ts"), "export const x = 1;\n").unwrap();
    fs::write(web_dir.join("src/nested/util.ts"), "export const y = 2;\n").unwrap();
}

/// The directory holding the one stub `npm` every test in this binary runs: it appends its argv
/// (space-joined) as one line to `../npm.log` -- relative to its cwd, which `ensure_web_bundle_built`
/// always sets to the fixture's `web` dir, so each fixture still gets its own log at
/// `<scratch>/npm.log` -- then fakes the one side effect each of the two real invocations
/// `ensure_web_bundle_built` ever makes would have in its cwd: `ci` creates `node_modules/`, `run
/// build` writes `dist/index.html`. Never a real npm, no network.
///
/// **Written once, before any test can spawn it, and never written again.** An earlier revision
/// wrote a fresh stub per test and exec'd it straight away, which failed with `Text file busy (os
/// error 26)` in 13 of 200 parallel runs of this binary, each time in a different test: another
/// test thread's `Command::spawn` could fork while this thread still held its stub open for
/// writing, and `execve` of a file that any process holds open for writing is `ETXTBSY` until that
/// child execs and its inherited copy of the descriptor closes. Every spawn in this binary goes
/// through this function first, and the `OnceLock` holds them all until the write is closed, so no
/// child of this process can inherit it. (The rename only keeps a leftover `npm` from an earlier
/// run from ever being seen half-rewritten; two concurrent runs of this binary are not supported
/// anyway, since [`ScratchDir`]s are named per test.)
fn npm_stub_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("web-bundle-freshness-npm-stub");
        fs::create_dir_all(&dir).unwrap();
        let script = r#"#!/bin/sh
printf '%s\n' "$*" >> ../npm.log
case "$1" in
  ci) mkdir -p node_modules ;;
  run) [ "$2" = build ] && { mkdir -p dist && printf '<html></html>\n' > dist/index.html; } ;;
esac
"#;
        let staged = dir.join(format!("npm.{}.tmp", std::process::id()));
        fs::write(&staged, script).unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&staged, dir.join("npm")).unwrap();
        dir
    })
}

/// One fixture web dir under its own [`ScratchDir`], run through the shared [`npm_stub_dir`] stub.
struct Fixture {
    _scratch: ScratchDir,
    web_dir: PathBuf,
    log_path: PathBuf,
}

impl Fixture {
    /// A fresh, never-built fixture: the source tree exists, but nothing has been installed.
    fn new(label: &str) -> Self {
        let scratch = ScratchDir::new(label);
        let web_dir = scratch.join("web");
        // Where the stub's `../npm.log`, run in `web_dir`, lands.
        let log_path = scratch.join("npm.log");
        fs::create_dir_all(&web_dir).unwrap();
        write_fixture(&web_dir);
        Fixture {
            _scratch: scratch,
            web_dir,
            log_path,
        }
    }

    /// A fixture that has already been through one successful `ensure_web_bundle_built` call (so
    /// `node_modules/`, `dist/index.html` and both state files all exist and agree with the
    /// fixture's current, unmodified content) -- the "installed and built, nothing changed since"
    /// starting point every one of the plan's eight cases mutates from.
    fn installed(label: &str) -> Self {
        let fixture = Self::new(label);
        fixture.run_and_log(false);
        fixture
    }

    /// Runs `ensure_web_bundle_built` against this fixture, through the stub, and returns exactly
    /// the npm invocations it made (one string per line, e.g. `"ci"` or `"run build"`), in order.
    /// The log is reset first, so a case's own assertion only ever sees this one call's commands,
    /// never `installed`'s own initial `ci` + `run build`.
    fn run_and_log(&self, clean_build: bool) -> Vec<String> {
        let _ = fs::remove_file(&self.log_path);
        ensure_web_bundle_built(&self.web_dir, Some(npm_stub_dir()), clean_build);
        fs::read_to_string(&self.log_path)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn append(path: &Path, suffix: &str) {
    let mut content = fs::read_to_string(path).unwrap();
    content.push_str(suffix);
    fs::write(path, content).unwrap();
}

#[test]
fn nothing_changed_runs_no_npm_command() {
    let fixture = Fixture::installed("nothing-changed");
    assert_eq!(fixture.run_and_log(false), Vec::<String>::new());
}

#[test]
fn an_edited_source_file_reruns_the_build_only() {
    let fixture = Fixture::installed("source-edited");
    append(&fixture.web_dir.join("src/main.ts"), "// changed\n");
    assert_eq!(fixture.run_and_log(false), vec!["run build".to_string()]);
}

#[test]
fn a_deleted_source_file_reruns_the_build_only() {
    let fixture = Fixture::installed("source-deleted");
    fs::remove_file(fixture.web_dir.join("src/nested/util.ts")).unwrap();
    assert_eq!(fixture.run_and_log(false), vec!["run build".to_string()]);
}

#[test]
fn package_json_alone_reruns_the_build_only() {
    let fixture = Fixture::installed("package-json-alone");
    append(
        &fixture.web_dir.join("package.json"),
        "\n// note: package-lock.json untouched\n",
    );
    assert_eq!(fixture.run_and_log(false), vec!["run build".to_string()]);
}

#[test]
fn package_lock_alone_runs_ci_then_the_build() {
    let fixture = Fixture::installed("package-lock-alone");
    append(
        &fixture.web_dir.join("package-lock.json"),
        "\n// note: package.json untouched\n",
    );
    assert_eq!(
        fixture.run_and_log(false),
        vec!["ci".to_string(), "run build".to_string()]
    );
}

#[test]
fn index_html_or_a_vite_or_tsconfig_file_alone_reruns_the_build_only() {
    for name in ["index.html", "vite.config.ts", "tsconfig.json"] {
        let fixture = Fixture::installed(&format!("top-level-{name}"));
        append(&fixture.web_dir.join(name), "\n// changed\n");
        assert_eq!(
            fixture.run_and_log(false),
            vec!["run build".to_string()],
            "changing {name}"
        );
    }
}

/// `vitest.config.ts` is not one of the fingerprinted inputs (the plan names `vite.config.*` and
/// `tsconfig*.json` only) -- changing it must not trigger a rebuild at all.
#[test]
fn vitest_config_is_not_a_watched_input() {
    let fixture = Fixture::installed("vitest-config-not-watched");
    append(&fixture.web_dir.join("vitest.config.ts"), "\n// changed\n");
    assert_eq!(fixture.run_and_log(false), Vec::<String>::new());
}

/// `node_modules` missing alone, with the bundle and its fingerprint still current, is exactly the
/// state of the LGPL source asset (Review Focus #2, spec sec 4(d)(0)): it ships `dist/index.html`
/// and a matching `dist/.inputs-sha256`, but `release_check.py` forbids it from ever shipping
/// `node_modules/`. Its offline rebuild must reach no npm at all -- an earlier revision of
/// `decide_web_build` ran `ci` (and so `run build`, which `release.sh`'s proof step fails on
/// through a stub `npm` that exits 1) here even though nothing needed rebuilding, breaking that
/// proof (Task 12 fix round 1, finding #1). This replaces the plan's original, narrower
/// `a_missing_node_modules_runs_ci_then_the_build`, whose fixture was this exact state and so
/// could not simultaneously hold both this case and the next one.
#[test]
fn node_modules_missing_with_the_bundle_current_runs_no_npm_command() {
    let fixture = Fixture::installed("node-modules-missing-bundle-current");
    fs::remove_dir_all(fixture.web_dir.join("node_modules")).unwrap();
    assert_eq!(fixture.run_and_log(false), Vec::<String>::new());
}

/// The plan's original case, narrowed to when it actually holds: `node_modules` missing is only a
/// reason to `ci` when a build is *also* needed (here, because a source file changed too) -- see
/// `node_modules_missing_with_the_bundle_current_runs_no_npm_command` just above for the case
/// where it is not.
#[test]
fn node_modules_missing_and_a_build_needed_runs_ci_then_the_build() {
    let fixture = Fixture::installed("node-modules-missing-and-a-build-needed");
    fs::remove_dir_all(fixture.web_dir.join("node_modules")).unwrap();
    append(&fixture.web_dir.join("src/main.ts"), "// changed\n");
    assert_eq!(
        fixture.run_and_log(false),
        vec!["ci".to_string(), "run build".to_string()]
    );
}

#[test]
fn the_clean_build_switch_forces_ci_and_the_build_even_with_nothing_changed() {
    let fixture = Fixture::installed("clean-build-switch");
    assert_eq!(
        fixture.run_and_log(true),
        vec!["ci".to_string(), "run build".to_string()]
    );
}

/// `build.rs`'s own `cargo:rerun-if-changed` list, over this fixture: the `src` directory itself
/// (covering every file under it in one watch, additions and removals included, so no file under
/// `src/` is separately listed) plus every top-level input the fingerprint also covers -- and
/// nothing else, `vitest.config.ts` included (see `vitest_config_is_not_a_watched_input` above).
#[test]
fn watch_paths_cover_every_top_level_input_and_the_src_directory_only() {
    let fixture = Fixture::new("watch-paths");
    let watched: std::collections::BTreeSet<PathBuf> = web_build_watch_paths(&fixture.web_dir).into_iter().collect();
    assert!(watched.contains(&fixture.web_dir.join("src")));
    for name in [
        "package.json",
        "package-lock.json",
        "index.html",
        "vite.config.ts",
        "tsconfig.json",
    ] {
        assert!(
            watched.contains(&fixture.web_dir.join(name)),
            "missing {name} from the watch list"
        );
    }
    assert!(!watched.contains(&fixture.web_dir.join("vitest.config.ts")));
    let src_dir = fixture.web_dir.join("src");
    assert!(
        !watched.iter().any(|p| *p != src_dir && p.starts_with(&src_dir)),
        "a file under src/ was listed individually: {watched:?}"
    );
}

/// Pins one literal sha256 for `compute_fingerprint` over a minimal, fixed fixture (a single
/// `src/a.ts` holding `x\n`, nothing else) -- the same fixture and the same literal hash
/// `packaging/tests/test_release_layout.py`'s own `WebBundleFingerprintTests` asserts against
/// `release_check.py`'s independent `web_bundle_fingerprint`. The two sides are separate
/// implementations of the same algorithm, kept in step only by their doc comments and by whatever
/// exercises each one -- nothing else compares them against each other directly, so a change to
/// either one that silently drifted from the other would otherwise only surface as every real
/// release's source asset failing `release_check.py`'s `check_neovibe_source_tree` (Task 12 fix
/// round 1, finding #3).
#[test]
fn fingerprint_matches_the_pinned_value_the_python_side_also_asserts() {
    let scratch = ScratchDir::new("pinned-fingerprint");
    fs::create_dir_all(scratch.join("src")).unwrap();
    fs::write(scratch.join("src/a.ts"), "x\n").unwrap();
    assert_eq!(
        compute_fingerprint(&scratch).unwrap(),
        "ae53f67ca66e61114b0f5453e451fa06c6ed8d9f8d44bee54b35d51b23fe3408"
    );
}
