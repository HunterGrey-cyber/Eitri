//! The `eitri-run-guard` helper tells a window which build of the guard it is, so a window passes over a helper
//! left beside it by another build.

#[test]
fn the_helper_prints_the_fingerprint_of_the_guard_it_was_built_with() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_eitri-run-guard"))
        .arg(eitri_core::run_guard::FINGERPRINT_FLAG)
        .output()
        .expect("the helper runs");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8(out.stdout).expect("text").trim(),
        eitri_core::run_guard::fingerprint()
    );
}
