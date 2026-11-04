//! Proves the Apache-2.0 s.4(b) claim in `NOTICE`: the fenced regions of
//! `src/vendored/build_sequence.rs` are byte-identical to upstream, zero lines
//! changed. If someone "just tweaks" a vendored line, this fails.

const VENDORED: &str = include_str!("../src/vendored/build_sequence.rs");
const UPSTREAM_133_172: &str = include_str!("upstream_extract/keyboard_133_172.rs.txt");
const UPSTREAM_291_718: &str = include_str!("upstream_extract/keyboard_291_718.rs.txt");

fn region(begin: &str, end: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in VENDORED.lines() {
        if line.contains(end) {
            inside = false;
        }
        if inside {
            out.push_str(line);
            out.push('\n');
        }
        if line.contains(begin) {
            inside = true;
        }
    }
    assert!(!out.is_empty(), "region marker {begin:?} produced nothing");
    out
}

#[test]
fn region_133_172_is_byte_identical_to_upstream() {
    let got = region(
        "vendored region: alacritty/src/input/keyboard.rs lines 133-172 (begin)",
        "vendored region: alacritty/src/input/keyboard.rs lines 133-172 (end)",
    );
    assert_eq!(got, UPSTREAM_133_172, "vendored region 133-172 diverges from upstream");
    assert_eq!(got.lines().count(), 40, "upstream 133..=172 is 40 lines");
}

#[test]
fn region_291_718_is_byte_identical_to_upstream() {
    let got = region(
        "vendored region: alacritty/src/input/keyboard.rs lines 291-718 (begin)",
        "vendored region: alacritty/src/input/keyboard.rs lines 291-718 (end)",
    );
    assert_eq!(got, UPSTREAM_291_718, "vendored region 291-718 diverges from upstream");
    assert_eq!(got.lines().count(), 428, "upstream 291..=718 is 428 lines");
}

#[test]
fn provenance_header_and_licence_are_present() {
    // Apache-2.0 s.4(a): retain the licence. s.4(b): state that files changed.
    let licence = include_str!("../LICENSE-APACHE");
    assert!(
        licence.contains("Apache License"),
        "LICENSE-APACHE must be the Apache 2.0 text"
    );
    assert!(licence.contains("Version 2.0, January 2004"));

    let notice = include_str!("../NOTICE");
    assert!(notice.contains("NOTICE OF MODIFICATION (Apache-2.0 section 4(b))"));
    assert!(notice.contains("94e7c8874e526b1e67b349d9ba30ddf81669119e"));

    for needle in [
        "94e7c8874e526b1e67b349d9ba30ddf81669119e",
        "alacritty/src/input/keyboard.rs",
        "lines:  133-172",
        "                  291-718",
        "THIS FILE CONTAINS MODIFIED COPIES OF UPSTREAM FILES",
        "Apache-2.0",
    ] {
        assert!(
            VENDORED.contains(needle),
            "vendored provenance header is missing {needle:?}"
        );
    }
}
