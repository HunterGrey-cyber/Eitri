// The commit hash `Cargo.lock` recorded for a git-dependency package, read as text rather than
// through a TOML parser -- `Cargo.lock`'s own format (one `[[package]]` table per package, each
// key on its own line) is stable enough that this project's other lockfile readers already do
// the same (`terminal-sync/build.rs`'s vte-version lookup).
//
// A plain `.rs` file rather than a module (not `//!`-documented, since `include!` can splice this
// file in anywhere, never only at the top of one): `build.rs` is compiled as a wholly separate
// binary from `shell` itself, so it cannot `use` anything from `src/`. `include!`ing this file's
// single path from both `build.rs` and `tests/build_info_lockfile_parsing.rs` is what lets one
// function serve both without being duplicated, and lets its parsing logic actually run under
// `cargo test` -- a `#[cfg(test)]` module written inside `build.rs` itself never does (`cargo
// test` builds `build.rs` as `build-script-build`, never with `--test`).

/// A `[[package]]` block's `source` line looks like
/// `source = "git+ssh://…?rev=<short>#<full 40-hex sha>"`; this returns everything after the
/// last `#`, which is the exact commit, not the caller-chosen `rev=` short form. `None` if
/// `package_name` isn't in the lockfile, or its block has no `source` line at all (a path
/// dependency, which is the caller's cue to fall back to reading the checkout directly).
fn parse_git_rev_from_lockfile(lockfile: &str, package_name: &str) -> Option<String> {
    let lines: Vec<&str> = lockfile.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() != "[[package]]" {
            i += 1;
            continue;
        }
        // Scan this block (up to the next `[[package]]` or EOF) for `name` and `source`.
        let mut j = i + 1;
        let mut is_target = false;
        let mut rev = None;
        while j < lines.len() && lines[j].trim() != "[[package]]" {
            let trimmed = lines[j].trim();
            if let Some(name) = trimmed.strip_prefix("name = \"").and_then(|s| s.strip_suffix('"')) {
                is_target = name == package_name;
            } else if let Some(source) = trimmed.strip_prefix("source = \"").and_then(|s| s.strip_suffix('"')) {
                rev = source.rsplit_once('#').map(|(_, sha)| sha.to_string());
            }
            j += 1;
        }
        if is_target {
            return rev;
        }
        i = j;
    }
    None
}

/// The Neovide fork's commit for `--version`. `Cargo.lock`'s own record wins where it has one (the
/// private tree, a git dependency). A path dependency -- the public tree's `neovide/` -- has none,
/// so next comes `NEOVIBE_BUILD_FORK_COMMIT` (`env_override`, empty meaning unset): `release.sh`
/// sets it, and so does a rebuild from the source asset following `SOURCE`, which has no `.git` in
/// `neovide/` to ask. Last, `git_head`, that checkout's own `git rev-parse HEAD`. `None` when all
/// three are silent.
fn resolve_fork_rev(
    lockfile: &str,
    env_override: Option<String>,
    git_head: impl FnOnce() -> Option<String>,
) -> Option<String> {
    parse_git_rev_from_lockfile(lockfile, "neovide")
        .or_else(|| env_override.filter(|value| !value.is_empty()))
        .or_else(git_head)
}
