// Decides whether `agent-ui/web`'s `dist/index.html` needs rebuilding, and runs npm when it does
// (v1-dist plan Task 6, P4-A1: `the private review notes`). Before this,
// `build.rs` decided by an mtime walk of `src/` only and ran `npm ci` only when `node_modules` was
// missing -- so a change to `package.json`/`package-lock.json` alone, to `index.html` or a Vite/
// TypeScript config, or a deleted source file, embedded the old bundle, and `publish.sh`/
// `release.sh` built incrementally.
//
// A plain `.rs` file rather than a module, `include!`d by both `build.rs` and
// `tests/web_bundle_freshness.rs`, for the same reason `build_info_lockfile.rs` is (see that
// file's own doc comment): `build.rs` is compiled as a wholly separate binary from `shell` itself,
// so it cannot `use` anything from `src/`, and a `#[cfg(test)]` module written inside `build.rs`
// never runs under `cargo test` (`cargo test` builds `build.rs` as `build-script-build`, never
// with `--test`).
//
// The rule: a sha256 fingerprint over every build input -- each file under `src/` by relative path
// and content (so a deletion changes it), `package.json`, `package-lock.json`, `index.html`, and
// every top-level `vite.config.*`/`tsconfig*.json` -- is written beside the bundle
// (`dist/.inputs-sha256`) after a successful build; missing or differing forces a rebuild. `npm ci`
// runs only when `node_modules` is missing or the lockfile's own sha256 differs from the one
// recorded when `node_modules` was last installed (`node_modules/.neovibe-lock-sha256`, written
// after a successful `npm ci`) -- so editing `package.json` alone (no lockfile change) rebuilds
// without reinstalling, matching what an `npm ci`-clean tree would actually need.
// `NEOVIBE_WEB_CLEAN_BUILD=1` forces both, even when nothing changed: `release.sh` sets it for the
// build of the shipped binaries, so a release never embeds a bundle (or a `node_modules`) left over
// from an earlier, unrelated build in the same tree. It is deliberately not tied to
// `PROFILE=release` -- the LGPL source asset's offline rebuild (spec sec 4(d)(0)) is itself a
// release build with no network, and must not reach npm.

// `Path`/`PathBuf`/`Command` are deliberately not `use`d here: this file is `include!`d into both
// `build.rs` and `tests/web_bundle_freshness.rs`, and each already imports all three at its own top
// -- importing them a second time here would be a duplicate-import error (E0252), not merely dead
// code, since `include!` splices this text into the very same module scope.
use sha2::{Digest, Sha256};
use std::fs;
use std::io;

/// Where the fingerprint is written, relative to `web_dir`.
const FINGERPRINT_REL: &str = "dist/.inputs-sha256";
/// Where the lockfile's own sha256 is recorded after a successful `npm ci`, relative to `web_dir`.
const LOCK_SHA_REL: &str = "node_modules/.neovibe-lock-sha256";

/// What `ensure_web_bundle_built` decided, from facts it already gathered off the filesystem --
/// this function itself touches nothing, which is what makes it the thing a test drives directly
/// for the plan's eight cases rather than having to reproduce every filesystem state by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WebBuildPlan {
    npm_ci: bool,
    npm_build: bool,
}

fn decide_web_build(
    node_modules_present: bool,
    lockfile_sha_matches: bool,
    fingerprint_matches: bool,
    clean_build: bool,
) -> WebBuildPlan {
    // A build is needed only for the clean-build switch or a fingerprint that no longer matches
    // what `dist/index.html` was built from -- never merely because `node_modules` happens to be
    // absent. That distinction is load-bearing: the LGPL source asset (Review Focus #2, spec sec
    // 4(d)(0)) ships `dist/index.html` and a matching `dist/.inputs-sha256` but never
    // `node_modules/` (`release_check.py` forbids any `node_modules` path in it), so its offline
    // rebuild must reach no npm at all when nothing has actually changed -- an earlier revision of
    // this function ran `npm ci` (and so `npm run build`, through `PATH`'s stub, exit 1) for that
    // exact asset even though its bundle was already current, failing `release.sh`'s own offline
    // proof step (Task 12 fix round 1, finding #1 -- reproduced with a fixture in that state).
    let npm_build = clean_build || !fingerprint_matches;
    // `npm ci` only when a build is actually happening (an install with nothing to build for is
    // pointless), and only for the two reasons the doc comment above gives, or the clean-build
    // switch.
    let npm_ci = npm_build && (clean_build || !node_modules_present || !lockfile_sha_matches);
    WebBuildPlan { npm_ci, npm_build }
}

/// Every file the fingerprint (and `cargo:rerun-if-changed`) covers, relative to nothing --
/// absolute paths under `web_dir` -- in a stable order: `src/` walked depth-first with each
/// directory's entries sorted, then `package.json`, `package-lock.json` and `index.html` if
/// present, then every top-level `vite.config.*`/`tsconfig*.json`, sorted. A name that does not
/// exist is simply absent from this list, not an error -- a minimal test fixture need not carry
/// every one of these, though the real `agent-ui/web` always does.
fn web_build_inputs(web_dir: &Path) -> Vec<PathBuf> {
    let mut inputs = Vec::new();
    walk_sorted(&web_dir.join("src"), &mut inputs);
    for name in ["package.json", "package-lock.json", "index.html"] {
        let path = web_dir.join(name);
        if path.is_file() {
            inputs.push(path);
        }
    }
    if let Ok(entries) = fs::read_dir(web_dir) {
        let mut extra: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .filter(|path| {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                name.starts_with("vite.config.") || (name.starts_with("tsconfig") && name.ends_with(".json"))
            })
            .collect();
        extra.sort();
        inputs.extend(extra);
    }
    inputs
}

/// What `build.rs` should ask Cargo to watch: the `src` directory itself (so Cargo notices an
/// added or removed file under it, not only an edited one) plus every currently-present top-level
/// input file [`web_build_inputs`] also folds into the fingerprint. Never the two state files
/// (`FINGERPRINT_REL`/`LOCK_SHA_REL`) -- watching a file this same build script writes would make
/// Cargo think its own output changed the input, rerunning on every build.
///
/// **Known limitation (Task 12 fix round 1, finding #6/#8):** a *brand-new* top-level
/// `vite.config.*`/`tsconfig*.json` that did not exist at the last build -- e.g. adding
/// `vite.config.js` beside an existing `vite.config.ts`, which Vite prefers -- is not itself
/// watched, so Cargo will not rerun this script for its addition alone; the fingerprint only
/// notices it once something else already-watched changes too. Watching `web_dir` itself would
/// catch it, but is deliberately not done: Cargo's own `rerun-if-changed` scans a directory path
/// recursively, which would pull in `dist/` (this script's own output) and `node_modules/`
/// (rewritten by every `npm ci`), rerunning the script on every single build. Pre-registering the
/// plausible names ahead of their creation does not work either: `watch_git_head` in `build.rs`
/// documents the same fact from the other side -- Cargo treats a `rerun-if-changed` path that does
/// not yet exist as *always* changed, so watching e.g. `vite.config.js` before it exists would
/// rerun this script on every build until the day someone actually creates it.
fn web_build_watch_paths(web_dir: &Path) -> Vec<PathBuf> {
    let src_dir = web_dir.join("src");
    let mut paths = vec![src_dir.clone()];
    paths.extend(
        web_build_inputs(web_dir)
            .into_iter()
            .filter(|p| !p.starts_with(&src_dir)),
    );
    paths
}

/// Depth-first, each directory's own entries sorted first -- so two identical trees always produce
/// the same order regardless of the filesystem's own directory-entry order, which is what makes
/// [`compute_fingerprint`]'s hash-over-this-sequence deterministic.
fn walk_sorted(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut children: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    children.sort();
    for path in children {
        if path.is_dir() {
            walk_sorted(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// The fingerprint over every path [`web_build_inputs`] names: each entry's path relative to
/// `web_dir` (so moving `web_dir` itself never changes it) followed by its content, in the
/// deterministic order `web_build_inputs` returns. A deletion (one fewer entry in the sequence) or
/// an addition (one more) changes the hash even when every remaining file's own bytes do not.
fn compute_fingerprint(web_dir: &Path) -> io::Result<String> {
    let mut hasher = Sha256::new();
    for path in web_build_inputs(web_dir) {
        let rel = path.strip_prefix(web_dir).unwrap_or(&path);
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update([0u8]);
        hasher.update(fs::read(&path)?);
        hasher.update([0u8]);
    }
    Ok(hex(hasher.finalize().as_slice()))
}

fn sha256_file(path: &Path) -> io::Result<String> {
    Ok(hex(Sha256::digest(fs::read(path)?).as_slice()))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Reads `web_dir`'s current state, decides via [`decide_web_build`], and runs whatever it calls
/// for -- `npm ci` and/or `npm run build`. Panics the same way a build script always has on a
/// command failure or a missing `dist/index.html` after a build: a build script has no better way
/// to report either.
///
/// `clean_build` is `NEOVIBE_WEB_CLEAN_BUILD=1`, read by the one real caller (`build.rs`'s `main`)
/// so this function itself never touches process environment -- `tests/web_bundle_freshness.rs`
/// passes it as a plain argument instead, which is what lets its eight cases run with Rust's
/// default parallel test execution rather than needing every test to serialize on a shared env var.
///
/// `npm_override_dir`, when given, is resolved directly as `npm_override_dir.join("npm")` rather
/// than searched for on `PATH` -- the real caller always passes `None`; the test file passes
/// `Some` to point at a logging stub instead of a real npm, with no `PATH`/process-env mutation
/// (and so no cross-test race) needed to do it.
fn ensure_web_bundle_built(web_dir: &Path, npm_override_dir: Option<&Path>, clean_build: bool) {
    let dist_index = web_dir.join("dist/index.html");
    let node_modules_present = web_dir.join("node_modules").is_dir();

    let lockfile_path = web_dir.join("package-lock.json");
    let current_lock_sha = if lockfile_path.is_file() {
        sha256_file(&lockfile_path).unwrap_or_else(|e| panic!("failed to read {}: {e}", lockfile_path.display()))
    } else {
        String::new()
    };
    // `.trim()`ed on read, matching `release_check.py`'s own `web_bundle_fingerprint` callers
    // (`.strip()`) -- neither side ever *writes* a trailing newline, but a byte-exact comparison
    // that tolerated one only on the Python side could pass `release_check.py` while still making
    // this function reach npm for the very same file (Task 12 fix round 1, finding #3).
    let recorded_lock_sha = fs::read_to_string(web_dir.join(LOCK_SHA_REL)).unwrap_or_default();
    let lockfile_sha_matches = !current_lock_sha.is_empty() && current_lock_sha == recorded_lock_sha.trim();

    let current_fingerprint =
        compute_fingerprint(web_dir).unwrap_or_else(|e| panic!("failed to fingerprint {}: {e}", web_dir.display()));
    let recorded_fingerprint = fs::read_to_string(web_dir.join(FINGERPRINT_REL)).unwrap_or_default();
    let fingerprint_matches = dist_index.is_file() && current_fingerprint == recorded_fingerprint.trim();

    let plan = decide_web_build(
        node_modules_present,
        lockfile_sha_matches,
        fingerprint_matches,
        clean_build,
    );

    if plan.npm_ci {
        run(web_dir, "npm", &["ci"], npm_override_dir);
        write_state_file(&web_dir.join(LOCK_SHA_REL), &current_lock_sha);
    }
    if plan.npm_build {
        run(web_dir, "npm", &["run", "build"], npm_override_dir);
        if !dist_index.is_file() {
            panic!("agent-ui/web build did not produce dist/index.html -- check the npm build output above");
        }
        // Reuses `current_fingerprint` (computed before `npm ci`/`npm run build` ran) rather than
        // recomputing now: `npm run build` writes only into `dist/`, never into anything the
        // fingerprint covers, so the two are equal when nothing else touched this tree while the
        // build ran -- but recomputing *after* the build, as an earlier revision of this function
        // did, actively hides the one case where they are not equal: an input edited concurrently
        // with `npm run build` would be captured as already-built by a recompute, even though the
        // bundle embedded moments later was built from the state before that edit -- exactly the
        // stale-embedded-bundle failure P4-A1 exists to catch (Task 12 fix round 1, finding #2).
        write_state_file(&web_dir.join(FINGERPRINT_REL), &current_fingerprint);
    }
}

fn write_state_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|e| panic!("failed to create {}: {e}", parent.display()));
    }
    fs::write(path, content).unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
}

/// `program args...` in `dir`. `override_dir.join(program)` when given (an explicit path, never
/// searched for on `PATH` -- see [`ensure_web_bundle_built`]'s own doc comment for why), the bare
/// `program` name (searched on the inherited `PATH`, as ever) otherwise.
fn run(dir: &Path, program: &str, args: &[&str], override_dir: Option<&Path>) {
    let resolved: PathBuf = match override_dir {
        Some(bin_dir) => bin_dir.join(program),
        None => PathBuf::from(program),
    };
    let status = Command::new(&resolved)
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| {
            // Both halves matter: a missing program and a missing working directory are the same
            // ENOENT here, and naming only the first one sent a real investigation after `npm`.
            panic!(
                "failed to run `{} {}` in {}: {e} -- is Node.js/npm installed and on PATH, and does \
             that directory exist?",
                resolved.display(),
                args.join(" "),
                dir.display()
            )
        });
    if !status.success() {
        panic!("`{} {}` failed with {status}", resolved.display(), args.join(" "));
    }
}
