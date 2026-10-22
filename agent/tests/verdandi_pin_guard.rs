//! `agent/Cargo.toml`'s `claude-runtime-protocol` revision and
//! `agent::EXPECTED_VERDANDI_REVISION` must name the same commit. Either can be a full hash
//! or a short prefix, so compare prefixes in either direction, as Cargo accepts both forms.
//! `providers::claude_sidecar::spawn::describe_checkout` uses the same comparison for runtime drift.
//!
//! This static, offline check reads the manifest and compares it with the compiled constant.
//! The runtime drift warning compares that constant with a running checkout and never reads
//! the manifest, so it cannot catch a manifest-only bump. Updating both pins together keeps
//! this check passing; there is no generated file to regenerate.

use std::fs;
use std::path::PathBuf;

use agent::EXPECTED_VERDANDI_REVISION;

/// Pulls the `rev = "..."` value off `agent/Cargo.toml`'s `claude-runtime-protocol` dependency line,
/// by text, deliberately not by parsing TOML -- this test wants to fail loudly if that one line's
/// shape ever stops looking like a `git` dependency with a `rev`, not silently accept some other
/// shape a TOML parser would happily normalize.
fn cargo_toml_rev() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    rev_from_manifest_text(&text, &path.display().to_string())
}

/// The parsing logic `cargo_toml_rev` runs, pulled out so it can be exercised on a synthetic
/// manifest string (`the_active_line_wins_over_a_commented_one_above_it`, below) without touching
/// the real `Cargo.toml` or its git dependency resolution.
///
/// Skips any line that is commented out in TOML (its first non-whitespace character is `#`) --
/// `.lines().find()` otherwise returns the FIRST line mentioning both `claude-runtime-protocol` and
/// `rev`, comment or not, so a stale pin left commented out *above* the real one would be read
/// instead of it.
fn rev_from_manifest_text(text: &str, path_for_errors: &str) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim_start().starts_with('#') && l.contains("claude-runtime-protocol") && l.contains("rev"))
        .unwrap_or_else(|| {
            panic!(
                "{path_for_errors}: no active (non-comment) line mentions both claude-runtime-protocol and rev \
                 -- has the pin moved to a different shape (a [patch] section, a path override, a version), or \
                 is it commented out?"
            )
        });
    let rev_at = line
        .find("rev")
        .unwrap_or_else(|| panic!("{line:?}: 'rev' vanished between the two .contains() checks and this find"));
    let after_rev = &line[rev_at..];
    let open_quote = after_rev
        .find('"')
        .unwrap_or_else(|| panic!("{line:?}: no opening quote after 'rev'"));
    let rest = &after_rev[open_quote + 1..];
    let close_quote = rest
        .find('"')
        .unwrap_or_else(|| panic!("{line:?}: no closing quote for rev's value"));
    rest[..close_quote].to_string()
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[test]
fn manifest_pin_and_expected_revision_name_the_same_commit() {
    let manifest_rev = cargo_toml_rev();
    let expected = EXPECTED_VERDANDI_REVISION;

    assert!(
        is_hex(&manifest_rev) && manifest_rev.len() >= 7,
        "agent/Cargo.toml's claude-runtime-protocol rev {manifest_rev:?} must be at least 7 hex characters"
    );
    assert!(
        is_hex(expected) && expected.len() >= 7,
        "agent::EXPECTED_VERDANDI_REVISION {expected:?} must be at least 7 hex characters"
    );
    assert!(
        manifest_rev.starts_with(expected) || expected.starts_with(manifest_rev.as_str()),
        "agent/Cargo.toml pins claude-runtime-protocol at {manifest_rev:?}, but \
         agent::EXPECTED_VERDANDI_REVISION is {expected:?} -- these must name the same commit (F15). \
         Bump both together and re-verify against the real proto diff \
         (the drill: `git diff <old>..<new> -- proto crates` \
         should be empty for a \
         mechanical follow-up, or the capability/wire changes it adds should be named in the dated \
         record)."
    );
}

/// A stale commented pin above the dependency must not win: `.lines().find()` otherwise
/// returns the first matching line even if it starts with `#`. Use synthetic manifest text
/// rather than mutating the real manifest, because an unresolvable revision would fail Cargo
/// before the pin-agreement test's assertion could run.
#[test]
fn the_active_line_wins_over_a_commented_one_above_it() {
    let manifest = "[dependencies]\n\
                     # claude-runtime-protocol = { git = \"ssh://git@git.example.org/owner/verdandi.git\", rev = \"b3aa188\" }\n\
                     claude-runtime-protocol = { git = \"ssh://git@git.example.org/owner/verdandi.git\", rev = \"c0b309e\" }\n";
    assert_eq!(
        rev_from_manifest_text(manifest, "<synthetic>"),
        "c0b309e",
        "the commented-out b3aa188 pin above the active c0b309e one must not win"
    );
}

/// The same fixture with no active line at all (every mention commented out) must fail loudly,
/// naming the file, rather than silently falling back to the commented value.
#[test]
#[should_panic(expected = "no active (non-comment) line")]
fn an_entirely_commented_out_dependency_is_not_a_pin() {
    let manifest = "[dependencies]\n\
                     # claude-runtime-protocol = { git = \"ssh://git@git.example.org/owner/verdandi.git\", rev = \"b3aa188\" }\n";
    rev_from_manifest_text(manifest, "<synthetic>");
}

/// A regression this test exists to catch, run in reverse: two revisions that are genuinely
/// different (not prefixes of each other) must fail the same comparison this test makes -- otherwise
/// the prefix check itself could be silently satisfying everything.
#[test]
fn the_prefix_comparison_itself_rejects_two_different_revisions() {
    let a = "b3aa1889111111111111111111111111111111";
    let b = "c0b309e0222222222222222222222222222222";
    assert!(
        !(a.starts_with(b) || b.starts_with(a)),
        "these two fixture revisions must not be prefixes of each other"
    );
}
