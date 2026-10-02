//! The GNOME Shell extension's own checks, from the workspace: its unit tests (`node --test`), its
//! metadata, and the one list of files it ships, which five places state independently.
//!
//! Those five are the installer, both nfpm profiles, both AUR PKGBUILDs and the release tooling, and a
//! file added to one and not the others ships a broken extension or leaks test code into a package, so
//! each is read as text and held to exactly the four files the shell loads. `testing.js` (the
//! test-only keyboard and window helpers) and `test/` live in the source tree and never ship.
//!
//! Opens no window and no connection of any kind; it reads files and runs `node` on plain JavaScript.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The four files the shell loads, in the order the installer lists them.
const SHIPPED: [&str; 4] = ["metadata.json", "extension.js", "direction.js", "policy.js"];
const UUID: &str = "eitri@huntergrey.cn";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn read(rel: &str) -> String {
    let path = root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

fn shipped_set() -> BTreeSet<String> {
    SHIPPED.iter().map(|s| s.to_string()).collect()
}

/// The words of a shell assignment `NAME='a b c'` in `text`.
fn quoted_list(text: &str, name: &str) -> BTreeSet<String> {
    let prefix = format!("{name}='");
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix(prefix.as_str()))
        .unwrap_or_else(|| panic!("no {name}='...' line"));
    line.trim_end()
        .strip_suffix('\'')
        .unwrap_or_else(|| panic!("{name} is not closed"))
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// The file names an nfpm profile installs under the extension's directory, with each entry's source
/// checked on the way (it is the same file name under `gnome-extension/`).
fn nfpm_files(rel: &str) -> BTreeSet<String> {
    let text = read(rel);
    let dir = format!("/usr/share/gnome-shell/extensions/{UUID}/");
    let mut files = BTreeSet::new();
    let mut src: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(s) = line.strip_prefix("- src:") {
            src = Some(s.trim().to_string());
        } else if let Some(d) = line.strip_prefix("dst:") {
            let d = d.trim();
            if let Some(name) = d.strip_prefix(&dir) {
                assert_eq!(
                    src.as_deref(),
                    Some(format!("./gnome-extension/{name}").as_str()),
                    "{rel}: source of {d}"
                );
                files.insert(name.to_string());
            } else {
                assert!(
                    !d.contains("gnome-shell"),
                    "{rel}: {d} is under gnome-shell outside the extension's directory"
                );
            }
        }
    }
    files
}

/// The names in a PKGBUILD's `for _ext in a b c d; do` loop.
fn pkgbuild_files(rel: &str) -> BTreeSet<String> {
    let text = read(rel);
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("for _ext in "))
        .unwrap_or_else(|| panic!("{rel}: no `for _ext in` loop"));
    let list = line
        .trim_start()
        .strip_prefix("for _ext in ")
        .unwrap()
        .strip_suffix("; do")
        .unwrap_or_else(|| panic!("{rel}: odd loop line {line:?}"));
    list.split_whitespace().map(str::to_string).collect()
}

/// The file names on the right of `_GNOME_EXT_FILES = { ... }` in release_check.py.
fn release_check_files() -> BTreeSet<String> {
    let text = read("packaging/release_check.py");
    let start = text
        .find("_GNOME_EXT_FILES = {")
        .expect("release_check.py has no _GNOME_EXT_FILES");
    let body = &text[start..];
    let body = &body[..body.find('}').expect("_GNOME_EXT_FILES is not closed")];
    body.lines()
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().trim_end_matches(',').trim_matches('"').to_string())
        .collect()
}

#[test]
fn the_extension_unit_tests_pass() {
    let dir = root().join("gnome-extension").join("test");
    assert!(dir.is_dir(), "{} is missing", dir.display());
    let mut cmd = Command::new("node");
    cmd.args(["--test", "gnome-extension/test"]).current_dir(root());
    match cmd.output() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("skipping: `node` is not on PATH, so gnome-extension/test did not run");
        }
        Err(e) => panic!("cannot run node: {e}"),
        Ok(out) => assert!(
            out.status.success(),
            "node --test gnome-extension/test failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    }
}

#[test]
fn the_metadata_names_the_extension_and_the_shell_versions_it_supports() {
    let text = read("gnome-extension/metadata.json");
    let meta: serde_json::Value = serde_json::from_str(&text).expect("metadata.json is not JSON");
    assert_eq!(meta["uuid"], UUID);
    let versions: Vec<&str> = meta["shell-version"]
        .as_array()
        .expect("shell-version is a list")
        .iter()
        .map(|v| v.as_str().expect("a version is a string"))
        .collect();
    assert_eq!(versions, ["45", "46", "47", "48", "49", "50"]);
}

#[test]
fn every_shipped_file_exists_and_the_test_code_is_not_among_them() {
    for f in SHIPPED {
        assert!(
            root().join("gnome-extension").join(f).is_file(),
            "gnome-extension/{f} is missing"
        );
    }
    assert!(
        root().join("gnome-extension/testing.js").is_file(),
        "testing.js is the test-only file this guard keeps out"
    );
    assert!(!shipped_set().contains("testing.js"));
}

#[test]
fn the_installer_the_nfpm_profiles_the_pkgbuilds_and_the_release_tooling_ship_the_same_four_files() {
    let want = shipped_set();
    assert_eq!(
        quoted_list(&read("packaging/install.sh"), "NV_EXT_FILES"),
        want,
        "install.sh NV_EXT_FILES"
    );
    assert_eq!(
        quoted_list(&read("packaging/install.sh"), "NV_EXT_ID"),
        BTreeSet::from([UUID.to_string()]),
        "install.sh NV_EXT_ID"
    );
    assert_eq!(
        quoted_list(&read("packaging/release.sh"), "GNOME_EXT_FILES"),
        want,
        "release.sh GNOME_EXT_FILES"
    );
    for profile in ["packaging/nfpm.yaml", "packaging/nfpm-public.yaml"] {
        assert_eq!(nfpm_files(profile), want, "{profile}");
    }
    for pkgbuild in ["packaging/aur/eitri-bin/PKGBUILD", "packaging/aur/eitri-git/PKGBUILD"] {
        assert_eq!(pkgbuild_files(pkgbuild), want, "{pkgbuild}");
    }
    assert_eq!(release_check_files(), want, "release_check.py _GNOME_EXT_FILES");
}
