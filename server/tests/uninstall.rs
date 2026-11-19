#[allow(dead_code)]
#[path = "../../core/src/remote_host/steps.rs"]
mod steps;

use std::path::Path;
use std::process::{Child, Command, Stdio};

struct Cleanup {
    root: std::path::PathBuf,
    sidecar: Child,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.sidecar.kill();
        let _ = self.sidecar.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn uninstall(data: &Path, state: &Path) -> std::process::Output {
    use sha2::{Digest, Sha256};
    use std::io::Write;
    let mut child = Command::new("sh")
        .args(["-c", steps::UNINSTALL, "eitri-uninstall"])
        .arg(data)
        .arg(state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let helper = data.join("0.3.0-abcdef0/bin/eitri-server");
    let hash = format!("{:x}", Sha256::digest(std::fs::read(helper).unwrap()));
    writeln!(child.stdin.take().unwrap(), "0.3.0-abcdef0 {hash}").unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn uninstall_cannot_delete_a_live_sidecars_records_after_its_daemon_dies() {
    let root = std::env::temp_dir().join(format!("uninstall-sidecar-{}", std::process::id()));
    let sidecar = Command::new("sleep").arg("3600").stdin(Stdio::null()).spawn().unwrap();
    let mut cleanup = Cleanup { root, sidecar };
    let data = cleanup.root.join("data/eitri-server");
    let state = cleanup.root.join("state/eitri-server");
    let run = state.join("run/i-0123456789abcdef");
    let helper = data.join("0.3.0-abcdef0/bin/eitri-server");
    agent::private_fs::create_private_dir_all(&run, &cleanup.root).unwrap();
    agent::private_fs::create_private_dir_all(helper.parent().unwrap(), &cleanup.root).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_eitri-server"), &helper).unwrap();
    let pid = cleanup.sidecar.id();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start: u64 = stat[stat.rfind(')').unwrap() + 1..]
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let record = run.join("sc-0123456789abcdef.pid");
    let mut metadata =
        serde_json::json!({"pid": pid, "start_time": start, "socket": "", "sidecar_id": "0123456789abcdef"});
    agent::private_fs::write_private(&record, &serde_json::to_vec(&metadata).unwrap()).unwrap();
    let output = uninstall(&data, &state);
    assert_eq!(output.status.code(), Some(76), "{:?}", output);
    assert!(String::from_utf8_lossy(&output.stderr).contains("recorded sidecars may still run"));
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(
        diagnostic.contains("i-0123456789abcdef")
            && diagnostic.contains("0123456789abcdef")
            && diagnostic.contains(&pid.to_string())
            && diagnostic.contains("alive"),
        "{diagnostic}"
    );
    assert!(record.exists() && helper.exists());
    assert!(cleanup.sidecar.try_wait().unwrap().is_none());
    // Malformed records cannot prove that their process has ended.
    agent::private_fs::write_private(&record, b"{").unwrap();
    let output = uninstall(&data, &state);
    assert_eq!(output.status.code(), Some(76));
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(
        diagnostic.contains("i-0123456789abcdef")
            && diagnostic.contains("0123456789abcdef")
            && diagnostic.contains("pid unknown unreadable"),
        "{diagnostic}"
    );
    assert!(record.exists());
    // A reused pid with another start time is no longer this recorded sidecar.
    metadata["start_time"] = (start + 1).into();
    agent::private_fs::write_private(&record, &serde_json::to_vec(&metadata).unwrap()).unwrap();
    assert!(uninstall(&data, &state).status.success());
    assert!(!data.exists() && !state.exists());
    assert!(cleanup.sidecar.try_wait().unwrap().is_none());
}

#[test]
fn recovery_inventory_needs_live_sidecars_and_no_daemon_lock() {
    use sha2::{Digest, Sha256};
    let root = std::env::temp_dir().join(format!("inventory-sidecar-{}", std::process::id()));
    let sidecar = Command::new("sleep").arg("3600").stdin(Stdio::null()).spawn().unwrap();
    let mut cleanup = Cleanup { root, sidecar };
    let state = cleanup.root.join("state/eitri-server");
    let installation = "0.0.1-abcdef0";
    let key = format!(
        "i-{}",
        &format!(
            "{:x}",
            Sha256::digest(format!("eitri-server-installation\0{installation}").as_bytes())
        )[..16]
    );
    let run = state.join("run").join(&key);
    agent::private_fs::create_private_dir_all(&run, &cleanup.root).unwrap();
    let inventory = || {
        Command::new(env!("CARGO_BIN_EXE_eitri-server"))
            .arg("check-sidecars")
            .arg(&state)
            .arg("--live-runs")
            .output()
            .unwrap()
    };
    assert!(inventory().status.success());
    assert!(inventory().stdout.is_empty());
    let pid = cleanup.sidecar.id();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start: u64 = stat[stat.rfind(')').unwrap() + 1..]
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let record = run.join("sc-0123456789abcdef.pid");
    agent::private_fs::write_private(
        &record,
        &serde_json::to_vec(&serde_json::json!({"pid":pid,"start_time":start,"socket":""})).unwrap(),
    )
    .unwrap();
    let info = serde_json::json!({"version":"0.0.1","rev":"abcdef0","pid":42,"protocol_major":1,
        "protocol_minor":0,"started_unix_ms":0,"installation":installation});
    agent::private_fs::write_private(&run.join("info"), &serde_json::to_vec(&info).unwrap()).unwrap();
    let output = inventory();
    assert!(output.status.success(), "{:?}", output);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["run"], key);
    assert_eq!(value["daemon"]["protocol_major"], 1);
    assert_eq!(value["installation"], installation);
    let lock = agent::private_fs::PrivateLock::open(&run.join("live.lock")).unwrap();
    assert!(lock.try_lock().unwrap());
    let output = inventory();
    assert!(output.status.success() && output.stdout.is_empty(), "{:?}", output);
    drop(lock);
    // The pidfd becomes readable before the child is reaped, so zombies never count as survivors.
    cleanup.sidecar.kill().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let output = inventory();
        assert!(output.status.success(), "{:?}", output);
        if output.stdout.is_empty() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "killed child remained alive");
        std::thread::yield_now();
    }
    cleanup.sidecar.wait().unwrap();
}

/// A live terminal host's record needs a checker that knows terminal hosts: an older helper, which ignores `th-*.pid`
/// and refuses the `--terminals` flag the step then passes, cannot clear it.
#[test]
fn uninstall_needs_a_terminal_aware_checker_for_a_live_terminal_host() {
    let root = std::env::temp_dir().join(format!("uninstall-terminal-{}", std::process::id()));
    let host = Command::new("sleep").arg("3600").stdin(Stdio::null()).spawn().unwrap();
    let cleanup = Cleanup { root, sidecar: host };
    let data = cleanup.root.join("data/eitri-server");
    let state = cleanup.root.join("state/eitri-server");
    let run = state.join("run/i-0123456789abcdef");
    let helper = data.join("0.3.0-abcdef0/bin/eitri-server");
    agent::private_fs::create_private_dir_all(&run, &cleanup.root).unwrap();
    agent::private_fs::create_private_dir_all(helper.parent().unwrap(), &cleanup.root).unwrap();
    let pid = cleanup.sidecar.id();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start: u64 = stat[stat.rfind(')').unwrap() + 1..]
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let id = "0123456789abcdef0123456789abcdef";
    let record = run.join(format!("th-{id}.pid"));
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    agent::private_fs::write_private(
        &record,
        &serde_json::to_vec(
            &serde_json::json!({"pid": pid, "start_time": start, "boot_id": boot.trim(), "socket": ""}),
        )
        .unwrap(),
    )
    .unwrap();
    // A helper from before terminal hosts: it knows no `--terminals` and sees no terminal records.
    std::fs::write(
        &helper,
        "#!/bin/sh\ntest \"$1\" = check-sidecars && test $# = 2 && exit 0\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let output = uninstall(&data, &state);
    assert_eq!(output.status.code(), Some(76), "{:?}", output);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("recorded sidecars or terminals may still run"),
        "{output:?}"
    );
    assert!(record.exists() && helper.exists());
    // This helper knows them, and refuses too, naming the terminal.
    std::fs::remove_file(&helper).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_eitri-server"), &helper).unwrap();
    let output = uninstall(&data, &state);
    assert_eq!(output.status.code(), Some(76), "{:?}", output);
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(
        diagnostic.contains(&format!("run i-0123456789abcdef terminal {id} pid {pid} alive")),
        "{diagnostic}"
    );
    assert!(record.exists());
}
