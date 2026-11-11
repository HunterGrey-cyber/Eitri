//! The display fixture's process environment, checked without opening a display or a bus.

#[path = "support/own_x_server.rs"]
mod own_x_server;

use std::process::Command;

#[test]
fn children_inherit_bus_isolation_without_changing_webkit_sandbox() {
    const CHILD: &str = "EITRI_DISPLAY_ENVIRONMENT_CHILD";
    if std::env::var_os(CHILD).is_some() {
        // Only this subprocess changes its environment, before starting any toolkit or worker.
        own_x_server::cut_session_bus("own_display_environment");
        let out = Command::new("/usr/bin/env").output().unwrap();
        assert!(out.status.success());
        let env = String::from_utf8(out.stdout).unwrap();
        assert!(env
            .lines()
            .any(|line| line == format!("DBUS_SESSION_BUS_ADDRESS={}", own_x_server::NO_SESSION_BUS)));
        assert!(!env.lines().any(|line| line.starts_with("AT_SPI_BUS_ADDRESS=")));
        assert!(env
            .lines()
            .any(|line| line == "WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=0"));
        return;
    }
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "children_inherit_bus_isolation_without_changing_webkit_sandbox",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/dev/null/inherited-session-bus")
        .env("AT_SPI_BUS_ADDRESS", "unix:path=/dev/null/inherited-accessibility-bus")
        .env("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS", "0")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("1 passed"));
}
