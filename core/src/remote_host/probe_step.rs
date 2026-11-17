use agent::ssh::{ChannelLauncher, ControlMaster, ProbeReport, SshLauncher};

use super::install::InstallError;

/// The helper's release ABI floor also covers the sidecar's bundled Node runtime.
pub const GLIBC_FLOOR: (u32, u32) = (2, 39);

pub fn validate(probe: &ProbeReport) -> Result<Vec<String>, InstallError> {
    if probe.uname != "Linux x86_64" {
        return Err(InstallError::Invalid(format!(
            "this server is {}; Eitri supports x86_64 Linux servers",
            probe.uname
        )));
    }
    if probe.glibc.is_none_or(|found| found < GLIBC_FLOOR) {
        return Err(InstallError::Invalid(format!(
            "this server needs glibc {}.{} or newer for the Eitri server helper",
            GLIBC_FLOOR.0, GLIBC_FLOOR.1
        )));
    }
    for path in [&probe.data_dir, &probe.state_dir] {
        super::install::checked_path(path)?;
    }
    Ok(
        if probe.linger == Some(false) && probe.kill_user_processes == Some(true) {
            vec!["Server processes may stop at logout (Linger=no, KillUserProcesses=yes). Run loginctl enable-linger on the server.".into()]
        } else {
            Vec::new()
        },
    )
}

pub fn probe(master: &ControlMaster) -> Result<ProbeReport, InstallError> {
    probe_with(&SshLauncher {
        master: master.clone(),
        helper: "/unused".into(),
    })
}

pub fn probe_with(launcher: &dyn ChannelLauncher) -> Result<ProbeReport, InstallError> {
    let child = launcher.spawn_step(super::steps::PROBE, &[])?;
    let bytes = super::install::communicate(child, &[], &|_, _| {})?;
    let text = std::str::from_utf8(&bytes).map_err(|_| InstallError::Invalid("probe output is not UTF-8".into()))?;
    let report = ProbeReport::parse(text).map_err(InstallError::Invalid)?;
    validate(&report)?;
    Ok(report)
}

#[cfg(test)]
pub(crate) fn fixture(root: &std::path::Path) -> ProbeReport {
    ProbeReport {
        uname: "Linux x86_64".into(),
        home: root.into(),
        data_dir: root.join("data"),
        state_dir: root.join("state"),
        login_shell: "/bin/sh".into(),
        login_path: "/usr/bin:/bin".into(),
        claude: None,
        git: None,
        glibc: Some(GLIBC_FLOOR),
        linger: Some(true),
        kill_user_processes: Some(false),
        free_bytes: Some(u64::MAX),
        gzip: true,
        root: agent::ssh::RootState::Missing,
        sync_marker: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aarch64_and_old_glibc_are_refused_with_reasons() {
        let mut report = fixture(std::path::Path::new("/srv/project"));
        report.uname = "Linux aarch64".into();
        assert!(validate(&report).unwrap_err().to_string().contains("Linux aarch64"));
        report.uname = "Linux x86_64".into();
        report.glibc = Some((2, 38));
        assert!(validate(&report).unwrap_err().to_string().contains("glibc 2.39"));
        report.glibc = None;
        assert!(validate(&report).is_err());
        report.glibc = Some(GLIBC_FLOOR);
        for bad in ["/srv/a b", "/srv/$(x)", "relative", "/srv/../escape"] {
            report.data_dir = bad.into();
            assert!(validate(&report).is_err());
        }
    }

    #[test]
    fn linger_off_with_kill_user_processes_warns_with_the_command() {
        let mut report = fixture(std::path::Path::new("/srv/project"));
        report.linger = Some(false);
        report.kill_user_processes = Some(true);
        assert!(validate(&report).unwrap()[0].contains("loginctl enable-linger"));
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn bootstrap_probe_reports_facts_without_writes_or_a_bus_connection() {
        super::super::tests::isolated(
            "remote_host::probe_step::tests::bootstrap_probe_reports_facts_without_writes_or_a_bus_connection",
            |home| {
                use std::os::unix::fs::PermissionsExt;
                // Every bus-facing command is a fixture; neither system nor session services are contacted.
                for (name, script) in [
                    ("loginctl", "printf no"),
                    ("systemd-analyze", "printf \"KillUserProcesses=yes\\n\""),
                    ("uname", "case \"$1\" in -s) printf Linux;; -m) printf x86_64;; esac"),
                    ("getconf", "printf \"glibc 2.39\""),
                ] {
                    let path = home.join("bin").join(name);
                    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
                }
                std::env::remove_var("XDG_DATA_HOME");
                std::env::remove_var("XDG_STATE_HOME");
                let launcher = agent::ssh::LocalExec {
                    helper: home.join("fixture"),
                };
                let report = probe_with(&launcher).unwrap();
                assert_eq!(report.home, home);
                assert_eq!(report.data_dir, home.join(".local/share/eitri-server"));
                assert_eq!(report.state_dir, home.join(".local/state/eitri-server"));
                assert_eq!(report.glibc, Some(GLIBC_FLOOR));
                assert!(report.free_bytes.is_some_and(|free| free > 0));
                assert_eq!(report.linger, Some(false));
                assert_eq!(report.kill_user_processes, Some(true));
                assert!(!report.data_dir.exists());
                assert!(!report.state_dir.exists());
            },
        );
    }
}
