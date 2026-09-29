//! Holds the GTK 4.14 / WebKit 2.40 toolkit floor (spec
//! `docs/superpowers/specs/2026-09-27-v1-dist-design.md` §2.1-2.2, plan Task 2). Two of the three
//! layers §2.2 describes live here; the third (the ABI check over shipped binaries) is
//! `packaging/check-abi-floor.py`.
//!
//! 1. `gtk4_manifests_declare_exactly_the_v4_14_floor` -- the three product manifests that name a
//!    `gtk4` version feature (`shell`, `neovide-editor`, `supervisor`) say `v4_14` and nothing
//!    else, catching a manifest bump directly.
//! 2. `gtk_webkit_feature_unification_never_exceeds_the_floor` -- Cargo's *resolved* feature graph
//!    (`cargo tree -e features -i <crate>`, run with `--locked --offline` so it reads only what is
//!    already fetched) never carries `gtk4`/`gdk4`/`gsk4` above `v4_14` or `webkit6` above `v2_40`
//!    through any dependency path, including one that would enable it through unification rather
//!    than through one of the three manifests above. This is the check that would catch a *new*
//!    dependency raising the floor even if nobody touched a manifest here.
//!
//! Both must fail today (the floor is `v4_18`) before Task 2's manifest edit lands; that red run is
//! recorded in the task report rather than pinned as a third test, since pinning "must currently
//! fail" would make the guard itself a tautology once the fix lands.
//!
//! **Correction (v1-dist verdict #2):** check 2's walk (`collect_enablers`) follows a feature
//! child down through *any* package's own feature, not only a same-crate `vN_M` version chain --
//! the original single-level walk treated every feature child alike and skipped it, which let a
//! forwarded feature (`shell/Cargo.toml`'s `default = ["gtk4/v4_16"]`, reproduced by hand) evade
//! both tests. The `tests` module below pins the red case as a fixture-based unit test, so it does
//! not need a manifest edit or a real `cargo tree` invocation to stay checked.

use std::path::{Path, PathBuf};
use std::process::Command;

/// GTK's own convention: `v4_N`. The floor this plan sets (spec §2.1).
const GTK_FLOOR: u32 = 14;
/// WebKitGTK's convention: `v2_N`. Not currently requested by any manifest here (no manifest names
/// a `webkit6` version feature), but checked anyway so a future dependency raising it is caught the
/// same way a GTK one would be.
const WEBKIT_FLOOR: u32 = 40;

/// The three manifests that declare `gtk4` directly, and whether that line must keep
/// `optional = true` (only `supervisor`'s, gated behind its `gui` feature).
const GTK4_MANIFESTS: &[(&str, bool)] = &[
    ("shell/Cargo.toml", false),
    ("neovide-editor/Cargo.toml", false),
    ("supervisor/Cargo.toml", true),
];

/// `-i` targets: the three `-sys` crates whose version-feature convention this guard polices.
/// `gtk4-sys` catches a manifest bump directly; `gdk4-sys`/`gsk4-sys` catch the same violation
/// reached through gtk4-rs's own internal re-exports (gdk4/gsk4 are gtk4's dependencies, not this
/// workspace's); `webkit6-sys` catches a future webkit version feature the same way.
const INVERT_TARGETS: &[&str] = &["gtk4-sys", "gdk4-sys", "gsk4-sys", "webkit6-sys"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("shell/ has a parent directory: the workspace root")
        .to_path_buf()
}

/// Textually pulls `features = [...]` and whether `optional = true` is present off the single-line
/// `gtk4 = { ... }` dependency declaration in `manifest_path`. No `toml` parser dependency is added
/// for this: all three lines are simple single-line inline tables today, and a manifest edit that
/// breaks this parse (e.g. splitting the table across lines) is exactly the kind of change that
/// should fail loudly here rather than pass silently.
fn parse_gtk4_line(manifest_path: &Path) -> (Vec<String>, bool) {
    let text =
        std::fs::read_to_string(manifest_path).unwrap_or_else(|e| panic!("reading {}: {e}", manifest_path.display()));
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("gtk4 ") || l.trim_start().starts_with("gtk4="))
        .unwrap_or_else(|| panic!("no `gtk4 = ...` dependency line found in {}", manifest_path.display()));

    let features_text = line
        .find("features")
        .map(|idx| &line[idx..])
        .and_then(|rest| {
            let start = rest.find('[')?;
            let end = rest[start..].find(']')?;
            Some(&rest[start + 1..start + end])
        })
        .unwrap_or_else(|| {
            panic!(
                "no `features = [...]` on the gtk4 line in {}: {line}",
                manifest_path.display()
            )
        });
    let features: Vec<String> = features_text
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // `optional = true` on the same single line; good enough for these three manifests, and a
    // false positive (matching the word inside a comment on the same line) is not a shape any of
    // them has.
    let optional = line.contains("optional") && line.contains("true");
    (features, optional)
}

#[test]
fn gtk4_manifests_declare_exactly_the_v4_14_floor() {
    let root = workspace_root();
    for (rel, expect_optional) in GTK4_MANIFESTS {
        let path = root.join(rel);
        let (features, optional) = parse_gtk4_line(&path);
        assert_eq!(
            features,
            vec!["v4_14".to_string()],
            "{rel}: the gtk4 dependency's `features` must be exactly [\"v4_14\"] (spec §2.1), found {features:?}"
        );
        if *expect_optional {
            assert!(
                optional,
                "{rel}: gtk4 must stay `optional = true` (gated behind supervisor's `gui` feature, L2 T6)"
            );
        }
    }
}

/// Cargo's rendered tree indents each ancestor level by exactly 4 characters, using box-drawing
/// glyphs (`│`, `├`, `└`, `─`) and spaces before the node's own text; the node's content starts at
/// the first character that is none of those. Splitting on that boundary (rather than assuming a
/// fixed multiple of 4) is what lets `direct_children` walk the tree without also assuming cargo's
/// exact drawing width, in case a future cargo changes it.
fn indent_and_content(line: &str) -> (usize, &str) {
    let idx = line.find(|c: char| !"│├└─ ".contains(c)).unwrap_or(line.len());
    (idx, &line[idx..])
}

/// A `<crate> feature "vN_M"` node's *direct* children in `cargo tree -i` are exactly the things
/// that require that feature: either another feature -- a same-crate version chain (e.g. `v4_16`
/// required only by `v4_18`) or a *different* package's own named feature forwarding it (e.g.
/// `shell feature "default"` via `default = ["gtk4/v4_16"]`) -- or a real package, the thing that
/// actually asked for it, in a manifest or on the command line. Returns *indices* into `lines`
/// rather than content, so a caller (`collect_enablers`) can recurse into a feature child's own
/// children the same way the outer walk in `feature_floor_violations` visits every line.
/// Grandchildren (anything indented deeper than the first child level) are not direct children of
/// `parent` and are skipped here; the walk still passes over them to find where this node's own
/// block ends (the first sibling or ancestor, at `parent`'s indent or shallower).
fn direct_child_indices(lines: &[&str], parent: usize) -> Vec<usize> {
    let (parent_indent, _) = indent_and_content(lines[parent]);
    let mut out = Vec::new();
    let mut child_indent = None;
    for (offset, line) in lines[parent + 1..].iter().enumerate() {
        let (indent, _) = indent_and_content(line);
        if indent <= parent_indent {
            break;
        }
        let idx = parent + 1 + offset;
        match child_indent {
            None => {
                child_indent = Some(indent);
                out.push(idx);
            }
            Some(ci) if indent == ci => out.push(idx),
            _ => {} // a grandchild or deeper -- not a direct child of `parent`
        }
    }
    out
}

/// `Some(name)` when `content` is a `<crate> feature "<name>"` node (cargo may append
/// ` (command-line)` and/or the dedup marker ` (*)` after the closing quote); `None` for a plain
/// package node (`<name> vX.Y.Z (<path>)`), which never contains the literal ` feature "`.
fn feature_name(content: &str) -> Option<&str> {
    let start = content.find(" feature \"")? + " feature \"".len();
    let rest = &content[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// `"v4_14"` -> `Some(('4', 14))`, `"v2_40"` -> `Some(('2', 40))`; `None` for anything not of that
/// shape (`"default"`, `"gui"`, ...).
fn version_suffix(feature: &str) -> Option<(char, u32)> {
    let rest = feature.strip_prefix('v')?;
    let (major, minor) = rest.split_once('_')?;
    let major = major.chars().next().filter(|_| major.len() == 1)?;
    Some((major, minor.parse().ok()?))
}

fn run_cargo_tree_inverted(invert: &str, root: &Path) -> String {
    let output = Command::new(env!("CARGO"))
        .args([
            "tree",
            "--locked",
            "--offline",
            "-e",
            "features",
            "-i",
            invert,
            "-p",
            "shell",
            "-p",
            "supervisor",
            "-p",
            "neovide-editor",
        ])
        .current_dir(root)
        .output()
        .unwrap_or_else(|e| panic!("spawning `{} tree -i {invert}`: {e}", env!("CARGO")));
    if !output.status.success() {
        // `--offline` failing for want of a fetched registry is not a reason to skip this guard --
        // it is a reason to fail loudly naming that, so nobody "fixes" it by relaxing --offline
        // instead of running `cargo fetch` first.
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!(
            "`cargo tree --locked --offline -e features -i {invert} -p shell -p supervisor -p \
             neovide-editor` failed (exit {:?}). If this is about the registry or lock file, run \
             `cargo fetch` and re-run this test -- do not drop --offline. stderr:\n{stderr}",
            output.status.code()
        );
    }
    String::from_utf8(output.stdout).unwrap_or_else(|e| panic!("`cargo tree -i {invert}` printed non-UTF-8: {e}"))
}

/// Every real enabler of the above-floor feature node at `parent`: a package that asked for it
/// directly, or -- since the walk was widened to fix v1-dist verdict #2 -- a *different* package's
/// own (non-version-shaped) feature forwarding it, e.g. `shell feature "default"` via
/// `default = ["gtk4/v4_16"]` in `shell/Cargo.toml`. The original, single-level walk treated *any*
/// feature child as "a chained feature, not a direct enabler" and skipped it outright, which is
/// correct for a same-crate version chain (a `vN_M` node whose child is another `vN_M` node -- that
/// child gets its own, separate pass when the outer loop in `violations_in_tree` reaches its own
/// line, so reporting it here too would only duplicate that pass's finding) but was also silently
/// swallowing the case that actually evades the guard: a forwarding feature whose own name is
/// *not* `vN_M`-shaped (`"default"`, `"gui"`, ...) never gets examined by the outer loop at all
/// (`version_suffix` returns `None` for it), so skipping it here left its own real enabler -- the
/// package that pulled it in -- unreported. This walk still recurses into it (rather than treating
/// it as automatically terminal) in case that forwarding feature is itself forwarded again.
fn collect_enablers(
    lines: &[&str],
    parent: usize,
    feat: &str,
    major: char,
    floor: u32,
    invert: &str,
    out: &mut Vec<String>,
) {
    for child_idx in direct_child_indices(lines, parent) {
        let (_, child) = indent_and_content(lines[child_idx]);
        match feature_name(child) {
            Some(child_feat) if version_suffix(child_feat).is_some() => {
                // A same-crate version chain hop (`v4_16` required only by `v4_18`): walked
                // through, not reported here, since it is examined on its own line separately.
                continue;
            }
            Some(_) => {
                let starter = child.split_whitespace().next().unwrap_or(child);
                out.push(format!(
                    "{starter} enables `{feat}` (floor is v{major}_{floor}) through its own feature \
                     `{child}`, via `-i {invert}`"
                ));
                collect_enablers(lines, child_idx, feat, major, floor, invert, out);
            }
            None => {
                let package = child.split_whitespace().next().unwrap_or(child);
                out.push(format!(
                    "{package} enables `{feat}` (floor is v{major}_{floor}) via `-i {invert}`"
                ));
            }
        }
    }
}

/// Every `v4_N`/`v2_N` feature node above its floor and its real enablers (see
/// `collect_enablers`), over already-rendered `cargo tree -i` text -- pure, no subprocess, so it is
/// reachable from a fixture-based unit test (below) without a manifest edit or a real `cargo tree`
/// invocation.
fn violations_in_tree(text: &str, invert: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut violations = Vec::new();
    for i in 0..lines.len() {
        let (_, content) = indent_and_content(lines[i]);
        let Some(feat) = feature_name(content) else { continue };
        let Some((major, n)) = version_suffix(feat) else {
            continue;
        };
        let floor = match major {
            '4' => GTK_FLOOR,
            '2' => WEBKIT_FLOOR,
            _ => continue,
        };
        if n <= floor {
            continue;
        }
        collect_enablers(&lines, i, feat, major, floor, invert, &mut violations);
    }
    violations
}

fn feature_floor_violations(invert: &str, root: &Path) -> Vec<String> {
    violations_in_tree(&run_cargo_tree_inverted(invert, root), invert)
}

#[test]
fn gtk_webkit_feature_unification_never_exceeds_the_floor() {
    let root = workspace_root();
    let mut violations = Vec::new();
    for invert in INVERT_TARGETS {
        violations.extend(feature_floor_violations(invert, &root));
    }
    assert!(
        violations.is_empty(),
        "gtk4/gdk4/gsk4 must not be unified above v4_{GTK_FLOOR}, webkit6 not above v2_{WEBKIT_FLOOR} (spec \
         §2.1-2.2), but:\n{}",
        violations.join("\n")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact tree shape v1-dist verdict #2 reproduced by hand: `cargo tree -e features -i
    /// gtk4-sys`, after temporarily adding `default = ["gtk4/v4_16"]` to `shell/Cargo.toml`'s
    /// `[features]`. `gtk4-sys feature "v4_16"` is enabled two hops down by `gtk4 feature "v4_16"`
    /// (a same-crate version chain, walked through), which is in turn enabled by `shell`'s own
    /// `"default"` feature (a *different* package's own, non-`vN_M`-shaped feature forwarding it) --
    /// not by a direct dependency edge. The pre-fix walk (`direct_children`, one level, skipping
    /// every feature child unconditionally) found nothing here; this is the red case
    /// `collect_enablers` (walking through a non-`vN_M` feature child instead of skipping it) makes
    /// green, without needing a real `cargo tree` invocation or a manifest edit.
    const FORWARDED_FEATURE_TREE: &str = r#"gtk4-sys v0.10.2
├── gtk4-sys feature "default"
│   └── gtk4 feature "default"
│       └── shell v1.0.0 (/workspace/shell) (*)
├── gtk4-sys feature "v4_14"
│   └── gtk4 feature "v4_14"
│       └── shell v1.0.0 (/workspace/shell) (*)
└── gtk4-sys feature "v4_16"
    └── gtk4 feature "v4_16"
        └── shell feature "default"
            └── shell v1.0.0 (/workspace/shell) (command-line)
"#;

    #[test]
    fn a_feature_forwarded_through_a_different_packages_own_feature_is_caught() {
        let violations = violations_in_tree(FORWARDED_FEATURE_TREE, "gtk4-sys");
        assert!(
            !violations.is_empty(),
            "expected a violation for gtk4-sys v4_16 forwarded via shell's own \"default\" feature, found none"
        );
        assert!(
            violations.iter().any(|v| v.contains("shell") && v.contains("v4_16")),
            "got {violations:?}"
        );
    }

    #[test]
    fn a_plain_version_chain_with_nothing_above_the_floor_reports_no_violations() {
        let text = r#"gtk4-sys v0.10.2
├── gtk4-sys feature "default"
│   └── gtk4 feature "default"
│       └── shell v1.0.0 (/workspace/shell) (*)
└── gtk4-sys feature "v4_14"
    └── gtk4 feature "v4_14"
        └── shell v1.0.0 (/workspace/shell) (*)
"#;
        assert_eq!(violations_in_tree(text, "gtk4-sys"), Vec::<String>::new());
    }

    #[test]
    fn a_direct_package_dependency_above_the_floor_is_still_caught() {
        // The pre-existing, already-working case (the "control" from the verdict's own probe): a
        // manifest bumping the `gtk4` line straight to `v4_16` is a plain
        // package-is-the-direct-child violation, unaffected by the fix above.
        let text = r#"gtk4-sys v0.10.2
└── gtk4-sys feature "v4_16"
    └── shell v1.0.0 (/workspace/shell)
"#;
        let violations = violations_in_tree(text, "gtk4-sys");
        assert_eq!(violations.len(), 1, "got {violations:?}");
        assert!(violations[0].contains("shell"));
        assert!(violations[0].contains("v4_16"));
    }
}
