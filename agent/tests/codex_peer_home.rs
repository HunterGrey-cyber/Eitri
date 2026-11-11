//! The real-Codex suites keep every `codex` child, the version probes included, out of the home the caller exported.
//! Needs no Codex: a stand-in records the `CODEX_HOME` it was run under. One test in its binary, because the home is set
//! for the whole process.
#![cfg(target_os = "linux")]

#[path = "support/codex_peer.rs"]
mod codex_peer;

use std::path::Path;

use agent::providers::codex::BinaryKey;
use codex_peer::{real_codex, require_supported_codex, Scratch};

#[test]
fn every_probe_runs_in_a_private_home_whatever_the_caller_exported() {
    let s = Scratch::new("probe-home-check");
    let exported = s.dir("exported");
    let seen = s.path("seen");
    let program = s.path("codex");
    script_fixture::write_executable_script(
        &program,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$CODEX_HOME\" >> '{}'\necho 'codex-cli 0.166.0'\n",
            seen.display()
        ),
        0o755,
    )
    .expect("an executable stand-in");
    if !agent::testkit::environment::in_child(&[
        ("EITRI_CODEX", Some(program.as_os_str())),
        ("CODEX_HOME", Some(exported.as_os_str())),
    ]) {
        return;
    }

    // Three ways a suite meets `--version`: the print, the floor's check, and the product's own probe.
    let found = real_codex();
    let seen = found.parent().unwrap().join("seen");
    let exported = std::env::var_os("CODEX_HOME").unwrap();
    require_supported_codex(&found);
    BinaryKey::of(&found).expect("the stand-in's version");

    let homes = std::fs::read_to_string(&seen).expect("the stand-in ran");
    let homes: Vec<&str> = homes.lines().collect();
    assert!(homes.len() >= 3, "{homes:?}");
    let target = std::fs::canonicalize(env!("CARGO_TARGET_TMPDIR")).expect("the target's scratch");
    for home in homes {
        assert_ne!(Path::new(home), exported, "a probe ran in the caller's home");
        assert!(Path::new(home).starts_with(&target), "a probe ran in {home:?}");
    }
}

#[cfg(test)]
mod script_fixture {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/executable.rs"));
}
