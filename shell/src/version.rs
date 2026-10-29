//! `shell --version` / `eitri --version` (through the launcher): one line, printed with no GTK
//! and no display touched.
//!
//! Deliberately prints **no copyright notices**. LGPL-3.0
//! §4(c): that clause only
//! binds "a Combined Work that displays copyright notices during execution" -- `--version` is the
//! one place in this binary that could have started being that, so it deliberately is not. An
//! About dialog or a notices view, if either is ever built, is where nvim-rs's own notice and the
//! GPL/LGPL text references belong instead (see `SOURCE`, which already carries them).

/// `eitri <version> (commit <12 hex>, neovide fork <7 hex>, verdandi <7 hex>)`.
///
/// The three build identities are baked in by `build.rs` via `cargo:rustc-env` --
/// `EITRI_BUILD_COMMIT` (this repo's own commit, or `EITRI_BUILD_COMMIT` if a release script
/// set it), `EITRI_FORK_REV` (the pinned Neovide fork commit) and `EITRI_VERDANDI_REV` (the
/// pinned `claude-runtime-protocol` commit) -- each already resolved to a real hash or the
/// literal string `unknown` by the time this binary exists. See `build.rs` for where each of the
/// three can come from.
pub fn version_line() -> String {
    format_version_line(
        env!("CARGO_PKG_VERSION"),
        env!("EITRI_BUILD_COMMIT"),
        env!("EITRI_FORK_REV"),
        env!("EITRI_VERDANDI_REV"),
    )
}

/// What `hello` carries so the two windows of a pair can tell they are one build: `<version>+<commit>`. A build
/// whose commit could not be read (`unknown`) would otherwise match every other such build, so the running
/// executable's size and modification time in nanoseconds are appended then; window 2 runs window 1's own
/// `/proc/self/exe`, so the two still agree, and only a window from another build differs.
pub fn fingerprint() -> String {
    let exe = std::fs::metadata("/proc/self/exe").ok().map(|meta| exe_stamp(&meta));
    format_fingerprint(env!("CARGO_PKG_VERSION"), env!("EITRI_BUILD_COMMIT"), exe)
}

/// The executable's size and modification time in nanoseconds since the epoch.
fn exe_stamp(meta: &std::fs::Metadata) -> (u64, i128) {
    use std::os::unix::fs::MetadataExt;
    let nanos = i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec());
    (meta.size(), nanos)
}

/// [`fingerprint`] with its inputs given, so a test can pin both forms.
fn format_fingerprint(version: &str, commit: &str, exe: Option<(u64, i128)>) -> String {
    match (commit, exe) {
        ("unknown", Some((size, mtime_ns))) => format!("{version}+{commit}+{size}.{mtime_ns}"),
        _ => format!("{version}+{commit}"),
    }
}

/// The formatting `version_line` wraps to the compile-time constants, split out and parameterized
/// so a test can pin the format with values `env!` cannot supply (`env!` is fixed at compile
/// time, before this binary or its tests exist).
fn format_version_line(version: &str, commit: &str, fork_rev: &str, verdandi_rev: &str) -> String {
    format!(
        "eitri {version} (commit {}, neovide fork {}, verdandi {})",
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
            "eitri 1.0.0 (commit abcdef012345, neovide fork 1234567, verdandi fedcba9)"
        );
    }

    #[test]
    fn the_release_candidate_version_formats_the_same_way() {
        assert_eq!(
            format_version_line("1.0.0-rc.1", "0123456789ab", "abcdef1", "1234567"),
            "eitri 1.0.0-rc.1 (commit 0123456789ab, neovide fork abcdef1, verdandi 1234567)"
        );
    }

    #[test]
    fn a_short_fallback_value_is_never_padded_or_panicked_on() {
        assert_eq!(
            format_version_line("1.0.0", "unknown", "unknown", "unknown"),
            "eitri 1.0.0 (commit unknown, neovide fork unknown, verdandi unknown)"
        );
    }

    #[test]
    fn the_fingerprint_is_the_version_and_the_commit() {
        assert_eq!(
            format_fingerprint("0.4.0", "abcdef0123456789", Some((10, 20))),
            "0.4.0+abcdef0123456789",
            "a known commit names the build by itself"
        );
        assert_eq!(
            format_fingerprint("0.4.0", "unknown", Some((123_456, 1_700_000_000_000_000_007))),
            "0.4.0+unknown+123456.1700000000000000007",
            "with no commit, the executable's size and time tell two builds apart"
        );
        assert_ne!(
            format_fingerprint("0.4.0", "unknown", Some((123_456, 1))),
            format_fingerprint("0.4.0", "unknown", Some((123_456, 2))),
            "two executables built at different times differ"
        );
        assert_eq!(format_fingerprint("0.4.0", "unknown", None), "0.4.0+unknown");
        let live = fingerprint();
        assert!(
            live.starts_with(&format!("{}+{}", env!("CARGO_PKG_VERSION"), env!("EITRI_BUILD_COMMIT"))),
            "{live}"
        );
        assert_eq!(live, fingerprint(), "the same process reads the same fingerprint twice");
    }

    #[test]
    fn the_line_never_contains_the_word_copyright() {
        // Pins the doc comment's claim as a real assertion, not just prose: LGPL-3.0 §4(c) is not
        // triggered because nothing here displays a copyright notice.
        let line = format_version_line("1.0.0", "abc", "def", "012");
        assert!(!line.to_lowercase().contains("copyright"), "got {line}");
    }
}
