//! Builds `agent-ui/web` (a standalone npm/Vite project, see agent-ui/web/package.json) into a
//! single-file `dist/index.html` before `shell` compiles, since `shell/src/agent_panel.rs`
//! embeds that file's contents at compile time via `include_str!`. Node.js/npm is a hard
//! build-time prerequisite for `shell` as of this task -- see CLAUDE.md's build-commands section.
//!
//! Also bakes `shell --version`'s three build identities (v1-dist plan Task 1, spec §3) into the
//! binary as compile-time env vars, read back by `shell/src/version.rs` through `env!`.

use std::path::{Path, PathBuf};
use std::process::Command;

// Shared with `tests/build_info_lockfile_parsing.rs` -- see that file's include and
// `build_info_lockfile.rs`'s own doc comment for why this is a bare `include!` rather than a
// dependency or a `mod`.
include!("build_info_lockfile.rs");
// Shared with `tests/web_bundle_freshness.rs` the same way -- see `build_web.rs`'s own doc comment.
include!("build_web.rs");

fn main() {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    emit_build_info(&workspace_root);

    let web_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web");
    // `CARGO_MANIFEST_DIR` is baked into THIS binary when the build script itself is compiled, so a
    // build-script binary another checkout left behind in a shared `target/` names a directory that
    // may no longer exist -- and cargo reuses it whenever the unit hash matches, which two checkouts
    // of the same workspace readily produce. Every `Command` below would then fail to spawn with
    // ENOENT, the same errno as a missing `npm`, and `run`'s own message says exactly that. Check the
    // directory first so the real cause is named instead. Reproduced on 2026-09-17; see CLAUDE.md.
    if !web_dir.is_dir() {
        panic!(
            "agent-ui/web is not at {} -- this build script was compiled for a checkout that is no \
             longer there, and cargo reused it from a shared target directory. `cargo clean -p \
             shell` rebuilds it against this one. (Node.js/npm are almost certainly fine.)",
            web_dir.display()
        );
    }
    // The rule (v1-dist plan Task 6, P4-A1): a sha256 fingerprint over every build input, not an
    // mtime walk of src/ alone -- see build_web.rs's own doc comment for why an mtime rule missed a
    // package.json/package-lock.json/index.html/config-only change and a deleted source file.
    for path in web_build_watch_paths(&web_dir) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-env-changed=NEOVIBE_WEB_CLEAN_BUILD");
    let clean_build = std::env::var("NEOVIBE_WEB_CLEAN_BUILD").as_deref() == Ok("1");
    ensure_web_bundle_built(&web_dir, None, clean_build);
}

/// Bakes `NEOVIBE_BUILD_COMMIT`, `NEOVIBE_FORK_REV` and `NEOVIBE_VERDANDI_REV` into the binary via
/// `cargo:rustc-env`, for `shell/src/version.rs`'s `env!` reads. Each falls back to the literal
/// string `"unknown"` rather than failing the build -- `--version` is meant to work even from a
/// tree this can't fully identify (a shallow clone, a source tarball with `.git` stripped out).
fn emit_build_info(workspace_root: &Path) {
    // `NEOVIBE_BUILD_COMMIT`: set by `release.sh` to the public clone's `HEAD` (spec §4.1), or by
    // a rebuild from the source asset (which has no `.git`) to the commit `SOURCE` names; falls
    // back to this tree's own `git rev-parse` for an ordinary developer build.
    println!("cargo:rerun-if-env-changed=NEOVIBE_BUILD_COMMIT");
    let commit = std::env::var("NEOVIBE_BUILD_COMMIT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| git_rev_parse_head(workspace_root))
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=NEOVIBE_BUILD_COMMIT={commit}");

    // The other two identities come from `Cargo.lock`'s own record of what this build resolved,
    // not from a second, possibly-disagreeing source -- see `parse_git_rev_from_lockfile`.
    let lockfile_path = workspace_root.join("Cargo.lock");
    println!("cargo:rerun-if-changed={}", lockfile_path.display());
    let lockfile = std::fs::read_to_string(&lockfile_path).unwrap_or_default();

    // The public tree's `neovide/` submodule is a path dependency, which `Cargo.lock` records with
    // no `source` line at all -- then `NEOVIBE_BUILD_FORK_COMMIT` (the source asset has no `.git`
    // in `neovide/`, so a rebuild from it sets this as `SOURCE` says), then that checkout itself.
    println!("cargo:rerun-if-env-changed=NEOVIBE_BUILD_FORK_COMMIT");
    let fork_rev = resolve_fork_rev(&lockfile, std::env::var("NEOVIBE_BUILD_FORK_COMMIT").ok(), || {
        git_rev_parse_head(&workspace_root.join("neovide"))
    })
    .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=NEOVIBE_FORK_REV={fork_rev}");

    let verdandi_rev =
        parse_git_rev_from_lockfile(&lockfile, "claude-runtime-protocol").unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=NEOVIBE_VERDANDI_REV={verdandi_rev}");
}

/// `git rev-parse HEAD` in `dir`, or `None` if `dir` doesn't exist, isn't the root of its own git
/// checkout, or the command otherwise fails -- every one of those is "this identity is unavailable",
/// never a build failure (see `emit_build_info`'s doc).
///
/// **The root of its own checkout, not merely inside one.** `git` climbs to the nearest enclosing
/// repository, so without this check a tree with no `.git` of its own -- the source asset, unpacked
/// anywhere (spec §4.3), or its `neovide/` directory -- would report whatever repository happens to
/// contain it as neovibe's commit or the fork's rev. `--show-toplevel` must name `dir` itself.
///
/// Also registers `dir`'s HEAD with cargo ([`watch_git_head`]), so a development build reruns this
/// script when HEAD moves and `--version` names the commit it was built from. Before that, the
/// script reran only on `NEOVIBE_BUILD_COMMIT`, `Cargo.lock` and the web sources, and a build after
/// a commit kept printing the commit the script last ran at.
fn git_rev_parse_head(dir: &Path) -> Option<String> {
    if !dir.is_dir() {
        return None;
    }
    let toplevel = PathBuf::from(git_output(dir, &["rev-parse", "--show-toplevel"])?);
    if toplevel.canonicalize().ok()? != dir.canonicalize().ok()? {
        return None;
    }
    watch_git_head(dir);
    git_output(dir, &["rev-parse", "HEAD"])
}

/// `cargo:rerun-if-changed` for what moves when `dir`'s HEAD does: `HEAD` itself (a checkout of
/// another branch), its reflog `logs/HEAD` (appended on every commit, checkout, reset and rebase
/// step, whether the branch ref is loose or packed), and the branch ref's loose file when there is
/// one. Resolved through `git rev-parse --git-path`, so a linked worktree's per-worktree `HEAD` and
/// the shared ref store are both found. **Only paths that exist are emitted**: cargo treats a missing
/// `rerun-if-changed` path as changed on every build, which would recompile `shell` every time.
/// `packed-refs` is left out on purpose -- `git gc` rewrites it without moving HEAD. The one gap: a
/// repository with reflogs switched off (`core.logAllRefUpdates=false`) whose branch ref is packed
/// is not noticed until something else reruns this script.
fn watch_git_head(dir: &Path) {
    let mut git_paths = vec!["HEAD".to_string(), "logs/HEAD".to_string()];
    if let Some(branch_ref) = git_output(dir, &["symbolic-ref", "-q", "HEAD"]) {
        git_paths.push(branch_ref);
    }
    for git_path in git_paths {
        let Some(path) = git_output(dir, &["rev-parse", "--git-path", &git_path]) else {
            continue;
        };
        // `--git-path` answers relative to `dir` inside an ordinary checkout, absolute in a linked
        // worktree; `join` handles both.
        let path = dir.join(path);
        if path.is_file() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

/// `git <args>` in `dir`: its trimmed stdout, or `None` on any failure or empty output.
fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).current_dir(dir).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}
