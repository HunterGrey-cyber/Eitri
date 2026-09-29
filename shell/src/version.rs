//! `shell --version` / `neovibe --version` (through the launcher): one line, printed with no GTK
//! and no display touched (v1-dist plan Task 1, spec §3).
//!
//! Deliberately prints **no copyright notices**. Spec
//! `docs/superpowers/specs/2026-09-27-v1-dist-design.md` §11.3, LGPL-3.0 §4(c): that clause only
//! binds "a Combined Work that displays copyright notices during execution" -- `--version` is the
//! one place in this binary that could have started being that, so it deliberately is not. An
//! About dialog or a notices view, if either is ever built, is where nvim-rs's own notice and the
//! GPL/LGPL text references belong instead (see `SOURCE`, which already carries them).

/// `neovibe <version> (commit <12 hex>, neovide fork <7 hex>, verdandi <7 hex>)`.
///
/// The three build identities are baked in by `build.rs` via `cargo:rustc-env` --
/// `NEOVIBE_BUILD_COMMIT` (this repo's own commit, or `NEOVIBE_BUILD_COMMIT` if a release script
/// set it), `NEOVIBE_FORK_REV` (the pinned Neovide fork commit) and `NEOVIBE_VERDANDI_REV` (the
/// pinned `claude-runtime-protocol` commit) -- each already resolved to a real hash or the
/// literal string `unknown` by the time this binary exists. See `build.rs` for where each of the
/// three can come from.
pub fn version_line() -> String {
    format_version_line(
        env!("CARGO_PKG_VERSION"),
        env!("NEOVIBE_BUILD_COMMIT"),
        env!("NEOVIBE_FORK_REV"),
        env!("NEOVIBE_VERDANDI_REV"),
    )
}

/// The formatting `version_line` wraps to the compile-time constants, split out and parameterized
/// so a test can pin the format with values `env!` cannot supply (`env!` is fixed at compile
/// time, before this binary or its tests exist).
fn format_version_line(version: &str, commit: &str, fork_rev: &str, verdandi_rev: &str) -> String {
    format!(
        "neovibe {version} (commit {}, neovide fork {}, verdandi {})",
        truncate(commit, 12),
        truncate(fork_rev, 7),
        truncate(verdandi_rev, 7),
    )
}

/// A prefix of `s`, at most `max_chars` characters (never bytes -- these are hex hashes, so it
/// never matters in practice, but this is still correct on arbitrary UTF-8). Never panics on a
/// string shorter than `max_chars`: `unknown` is 7 characters, shorter than the 12-character
/// commit slot it can be asked to fill.
fn truncate(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((byte_index, _)) => &s[..byte_index],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_the_version_line_and_truncates_each_hash_to_its_own_width() {
        assert_eq!(
            format_version_line("1.0.0", "abcdef0123456789", "1234567890abcdef", "fedcba9876543210",),
            "neovibe 1.0.0 (commit abcdef012345, neovide fork 1234567, verdandi fedcba9)"
        );
    }

    #[test]
    fn the_release_candidate_version_formats_the_same_way() {
        assert_eq!(
            format_version_line("1.0.0-rc.1", "0123456789ab", "abcdef1", "1234567"),
            "neovibe 1.0.0-rc.1 (commit 0123456789ab, neovide fork abcdef1, verdandi 1234567)"
        );
    }

    #[test]
    fn a_short_fallback_value_is_never_padded_or_panicked_on() {
        assert_eq!(
            format_version_line("1.0.0", "unknown", "unknown", "unknown"),
            "neovibe 1.0.0 (commit unknown, neovide fork unknown, verdandi unknown)"
        );
    }

    #[test]
    fn the_line_never_contains_the_word_copyright() {
        // Pins the doc comment's claim as a real assertion, not just prose: LGPL-3.0 §4(c) is not
        // triggered because nothing here displays a copyright notice.
        let line = format_version_line("1.0.0", "abc", "def", "012");
        assert!(!line.to_lowercase().contains("copyright"), "got {line}");
    }
}
