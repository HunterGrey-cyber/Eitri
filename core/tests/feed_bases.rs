//! Environment wiring runs in a subprocess against the normal core library. Only the child with
//! explicit scratch TMPDIR/runtime values bypasses state redirection; no real runtime directory is used.
// The live mentions feed belongs to the IDE server, which is Linux-only.
#[cfg(target_os = "linux")]
use eitri_core::ide_server::mention::LiveMentions;
#[cfg(target_os = "linux")]
use eitri_core::ide_server::types::MentionPort;
use eitri_core::plugin::follow_feed::FollowSocket;
use eitri_core::scratch::ScratchDir;
use std::os::unix::fs::PermissionsExt;

#[test]
fn feed_constructors_use_safe_bases() {
    if let Ok(case) = std::env::var("EITRI_FEED_BASE_TEST") {
        match case.as_str() {
            "unsafe" => {
                assert!(ScratchDir::new().is_none());
                assert!(FollowSocket::bind().is_none());
                #[cfg(target_os = "linux")]
                assert!(LiveMentions::new().open().is_none());
                assert_eq!(std::fs::read_dir(std::env::temp_dir()).unwrap().count(), 0);
            }
            "relative" | "runtime" => {
                let dir = ScratchDir::new().expect("scratch has a safe base");
                let expected = if case == "runtime" {
                    std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap())
                } else {
                    std::path::absolute(std::env::temp_dir()).unwrap()
                };
                assert_eq!(dir.path().parent(), Some(expected.as_path()));
                assert!(dir.path().is_absolute());
            }
            _ => panic!("unknown case"),
        }
        return;
    }
    let root = agent::state_dirs::test_workspace_dir("feed-bases");
    for case in ["unsafe", "relative", "runtime"] {
        let tmp = root.join(case);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::set_permissions(
            &tmp,
            std::fs::Permissions::from_mode(if case == "relative" { 0o700 } else { 0o777 }),
        )
        .unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "feed_constructors_use_safe_bases", "--nocapture"])
            .env("EITRI_FEED_BASE_TEST", case)
            .env_remove("XDG_RUNTIME_DIR")
            .env(
                "TMPDIR",
                if case == "relative" {
                    std::path::Path::new(case)
                } else {
                    &tmp
                },
            )
            .current_dir(&root);
        if case == "runtime" {
            let runtime = root.join("run");
            std::fs::create_dir(&runtime).unwrap();
            std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
            child.env("XDG_RUNTIME_DIR", runtime);
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{case}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }
}
