//! The acceptance build's marker is one string in three places: the module that defines it, `bundle.sh` (which
//! refuses to bundle a binary whose marker does not match `--acceptance`) and the acceptance driver. A changed
//! marker in one place alone would make the bundle's check fail open. And the binary carries it exactly when it
//! was built with the feature.

use std::path::Path;

const MARKER: &str = "eitri-acceptance-build-marker-1";

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn the_marker_is_the_same_in_the_module_the_bundle_script_and_the_driver() {
    let module = read("src/appkit/acceptance.rs");
    assert!(module.contains(&format!("pub const BUILD_MARKER: &str = \"{MARKER}\";")));
    assert!(module.contains(&format!("*b\"{MARKER}\"")), "the #[used] bytes");
    assert!(read("bundle/bundle.sh").contains(MARKER), "bundle.sh looks for it");
    assert!(
        read("../mac-spike/app-acceptance/src/main.rs").contains(MARKER),
        "the driver looks for it"
    );
}

#[test]
fn the_binary_carries_the_marker_exactly_when_built_with_the_feature() {
    let binary = std::fs::read(env!("CARGO_BIN_EXE_eitri-mac")).unwrap();
    let carries = binary.windows(MARKER.len()).any(|w| w == MARKER.as_bytes());
    assert_eq!(carries, cfg!(feature = "acceptance") && cfg!(target_os = "macos"));
}
