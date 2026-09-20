//! Builds `agent-ui/web` (a standalone npm/Vite project, see agent-ui/web/package.json) into a
//! single-file `dist/index.html` before `shell` compiles, since `shell/src/agent_panel.rs`
//! embeds that file's contents at compile time via `include_str!`. Node.js/npm is a hard
//! build-time prerequisite for `shell` as of this task -- see CLAUDE.md's build-commands section.

use std::path::Path;
use std::process::Command;

fn main() {
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
    let dist_index = web_dir.join("dist/index.html");

    println!("cargo:rerun-if-changed={}", web_dir.join("src").display());
    println!("cargo:rerun-if-changed={}", web_dir.join("package.json").display());

    // Skip the (slow) npm build if dist/index.html already exists and is newer than every file
    // under src/ -- avoids a full rebuild on every `cargo build` when the frontend hasn't
    // changed. A missing dist/index.html always triggers a real build regardless.
    if dist_index.exists() && is_dist_up_to_date(&web_dir, &dist_index) {
        return;
    }

    let node_modules = web_dir.join("node_modules");
    if !node_modules.exists() {
        run(&web_dir, "npm", &["ci"]);
    }
    run(&web_dir, "npm", &["run", "build"]);

    if !dist_index.exists() {
        panic!("agent-ui/web build did not produce dist/index.html -- check the npm build output above");
    }
}

fn is_dist_up_to_date(web_dir: &Path, dist_index: &Path) -> bool {
    let dist_mtime = match std::fs::metadata(dist_index).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let src_dir = web_dir.join("src");
    walk_newer_than(&src_dir, dist_mtime).is_none()
}

/// Returns `Some(path)` for the first file under `dir` newer than `threshold`, or `None` if
/// every file is older (meaning the existing build output is still current).
fn walk_newer_than(dir: &Path, threshold: std::time::SystemTime) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = walk_newer_than(&path, threshold) {
                return Some(found);
            }
        } else if let Ok(meta) = entry.metadata() {
            if let Ok(mtime) = meta.modified() {
                if mtime > threshold {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn run(dir: &Path, program: &str, args: &[&str]) {
    let status = Command::new(program)
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| {
            // Both halves matter: a missing program and a missing working directory are the same
            // ENOENT here, and naming only the first one sent a real investigation after `npm`.
            panic!(
                "failed to run `{program} {}` in {}: {e} -- is Node.js/npm installed and on PATH, \
                 and does that directory exist?",
                args.join(" "),
                dir.display()
            )
        });
    if !status.success() {
        panic!("`{program} {}` failed with {status}", args.join(" "));
    }
}
