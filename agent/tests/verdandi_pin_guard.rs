//! Fitness function F15 (2026-09-27 Codex audit P6, `the private review notes`
//! appendix, verdict SUPPORTED in `the private review notes`):
//! `agent/Cargo.toml`'s `claude-runtime-protocol` `rev` and `agent::EXPECTED_VERDANDI_REVISION` must
//! name the same commit. The manifest has held both a full 40-character revision and a short one
//! (`the private review notes`), so the comparison is by prefix
//! in either direction, never equality -- `cargo`'s own `git` dependency resolution accepts either
//! shape, and this crate's own runtime checkout-drift warning
//! (`providers::claude_sidecar::spawn::describe_checkout`) makes the same `starts_with` choice for the
//! same reason.
//!
//! This test is static and offline: it reads `Cargo.toml` as text and compares it against the
//! compiled-in constant, with no checkout and no network. It is not a replacement for
//! `describe_checkout`'s runtime warning (which compares the constant against an actual running
//! Verdandi checkout's `git rev-parse`); it catches a *different* failure -- someone bumping one of
//! the two without the other, which the runtime check cannot see because it never reads `Cargo.toml`.
//!
//! No regenerate step: there is nothing generated here to regenerate. Bump both together
//! deliberately (`CLAUDE.md`'s "Verdandi pins" table names the drill) and this test passes again.

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
/// instead of it (2026-09-28 fix round, a Codex finding on the first version of this test).
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
         Bump both together and re-verify against the real proto diff (CLAUDE.md's \"Verdandi pins\" \
         table names the drill: `git diff <old>..<new> -- proto crates` should be empty for a \
         mechanical follow-up, or the capability/wire changes it adds should be named in the dated \
         record)."
    );
}

/// A Codex finding on the first version of this test (2026-09-28 fix round, confirmed with the
/// mutation below before the fix): a stale pin left commented out *above* the real dependency line
/// was read instead of it, because `.lines().find()` takes whichever line matches first regardless
/// of a leading `#`. Synthetic manifest text only -- this never touches the real `Cargo.toml` or its
/// git dependency resolution (which was deliberately rejected as a mutation vehicle for the OTHER
/// test in this file, `manifest_pin_and_expected_revision_name_the_same_commit`'s own doc comment,
/// for exactly that reason: an unresolvable `rev` fails the whole build before any assertion runs).
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
