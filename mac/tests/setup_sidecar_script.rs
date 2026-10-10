//! `bundle/setup-sidecar.sh` puts the sidecar where Eitri's own resolution finds it, and refuses one
//! built from another revision. Runs on macOS only: the script is the darwin-arm64 step.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use agent::{sidecar_availability, user_sidecar_path, SidecarAvailability, EXPECTED_VERDANDI_REVISION};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn run_script(xdg: &Path, args: &[&str]) -> Output {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("bundle/setup-sidecar.sh");
    Command::new("/bin/sh")
        .arg(script)
        .args(args)
        .env("XDG_DATA_HOME", xdg)
        .output()
        .unwrap()
}

/// The artifact the script is given: a real one named by `EITRI_SETUP_SIDECAR_ARTIFACT` (with its
/// checkout beside it), else a stand-in that prints the four lines a sidecar's `--version` does.
fn artifact(dir: &Path) -> (PathBuf, bool) {
    if let Some(real) = std::env::var_os("EITRI_SETUP_SIDECAR_ARTIFACT") {
        return (PathBuf::from(real), true);
    }
    let path = dir.join("stand-in-sidecar");
    std::fs::write(
        &path,
        "#!/bin/sh\nprintf '%s\\n' 'verdandi-claude-sidecar 0.2.0 (protocol 3, node v22.23.2, build 0)' \
         'claude-agent-sdk 0.3.252 (bundled claude code 2.1.252)' 'supported claude code CLI: >=2.1.252 <3.0.0' \
         'executable sources served: host_cli'\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (path, false)
}

// One test, because it sets process-wide variables that `sidecar_availability` reads.
#[test]
fn the_installed_sidecar_is_the_one_eitri_resolves() {
    let dir = tempfile::tempdir().unwrap();
    let xdg = dir.path().join("data");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let (artifact, in_checkout) = artifact(dir.path());
    let expected = user_sidecar_path(Some(xdg.as_os_str()), None, EXPECTED_VERDANDI_REVISION).unwrap();

    std::env::remove_var("EITRI_SIDECAR_BINARY");
    std::env::remove_var("EITRI_VERDANDI_CHECKOUT");
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_DATA_HOME", &xdg);
    assert_eq!(
        sidecar_availability(),
        SidecarAvailability::Missing,
        "nothing installed yet"
    );

    // Another revision is refused and installs nothing.
    let artifact_str = artifact.to_str().unwrap();
    if !in_checkout {
        let refused = run_script(&xdg, &["--artifact", artifact_str, "--verdandi-rev", "deadbee"]);
        assert!(!refused.status.success());
        assert!(String::from_utf8_lossy(&refused.stderr).contains("not the one this Eitri expects"));
        assert!(!expected.exists());
        assert_eq!(sidecar_availability(), SidecarAvailability::Missing);
    }

    let mut args = vec!["--artifact", artifact_str];
    if !in_checkout {
        args.extend(["--verdandi-rev", EXPECTED_VERDANDI_REVISION]);
    }
    let out = run_script(&xdg, &args);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), expected.to_str().unwrap());
    assert!(expected.is_file());
    assert_eq!(expected.metadata().unwrap().permissions().mode() & 0o111, 0o111);
    assert!(expected.with_file_name("BUILD").is_file());
    assert_eq!(sidecar_availability(), SidecarAvailability::Runnable);
}

#[test]
fn a_revision_fragment_is_refused_before_anything_is_installed() {
    let xdg = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (artifact, _) = artifact(dir.path());
    // `8` is a prefix of the expected revision, which the prefix rule alone would accept.
    for rev in ["8", "8f1d2f", "8F1D2F9", "8f1d2f9-x"] {
        let out = run_script(
            xdg.path(),
            &["--artifact", artifact.to_str().unwrap(), "--verdandi-rev", rev],
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{rev}: {stderr}");
        assert!(stderr.contains("is not") && stderr.contains("hex"), "{rev}: {stderr}");
    }
    assert!(!xdg.path().join("eitri").exists(), "nothing installed");
}

#[test]
fn a_refused_artifact_leaves_no_sidecar_directory_behind() {
    let xdg = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let wrong = dir.path().join("wrong-sidecar");
    std::fs::write(
        &wrong,
        "#!/bin/sh\nprintf '%s\\n' 'verdandi-claude-sidecar 0.2.0 (protocol 2, node v22.23.2, build 0)' x x \
         'executable sources served: host_cli'\n",
    )
    .unwrap();
    std::fs::set_permissions(&wrong, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = run_script(
        xdg.path(),
        &[
            "--artifact",
            wrong.to_str().unwrap(),
            "--verdandi-rev",
            EXPECTED_VERDANDI_REVISION,
        ],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("refusing to install it"));
    let rev7 = &EXPECTED_VERDANDI_REVISION[..7];
    let left = xdg.path().join("eitri/sidecar").join(rev7);
    assert!(!left.exists(), "{} was left behind", left.display());
}
