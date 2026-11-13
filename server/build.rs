use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"))
        .parent()
        .expect("workspace root")
        .to_path_buf();
    println!("cargo:rerun-if-env-changed=EITRI_BUILD_COMMIT");
    let commit = std::env::var("EITRI_BUILD_COMMIT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| git_rev_parse_head(&root))
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=EITRI_BUILD_COMMIT={commit}");
}

/// Only this tree's own checkout may supply its identity; git otherwise climbs into a containing tree.
fn git_rev_parse_head(dir: &Path) -> Option<String> {
    let top = PathBuf::from(git_output(dir, &["rev-parse", "--show-toplevel"])?);
    if top.canonicalize().ok()? != dir.canonicalize().ok()? {
        return None;
    }
    let mut paths = vec!["HEAD".to_owned(), "logs/HEAD".to_owned()];
    if let Some(branch) = git_output(dir, &["symbolic-ref", "-q", "HEAD"]) {
        paths.push(branch);
    }
    for git_path in paths {
        let Some(path) = git_output(dir, &["rev-parse", "--git-path", &git_path]) else {
            continue;
        };
        let path = dir.join(path);
        if path.is_file() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    git_output(dir, &["rev-parse", "HEAD"])
}

fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).current_dir(dir).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}
