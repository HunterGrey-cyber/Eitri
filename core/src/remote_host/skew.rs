use agent::ssh::{ChannelLauncher, DaemonInfo};
use std::cmp::Ordering;
use std::ffi::OsStr;

use super::install::InstallError;
use crate::remote_project::RemoteHost;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attach {
    Same,
    NewForNewWork {
        older: Vec<DaemonInfo>,
    },
    OlderIncompatible {
        daemon: DaemonInfo,
    },
    /// A newer or unorderable identity must never be stopped by this client.
    Newer {
        daemon: DaemonInfo,
    },
}

impl Attach {
    pub fn notice(&self) -> Option<&'static str> {
        matches!(self, Self::Newer { .. })
            .then_some("The server runs a newer Eitri helper; this window attaches nothing.")
    }
}

pub fn decide_attach(
    this_major: u32,
    current_version: &str,
    current_revision: &str,
    running: &[DaemonInfo],
) -> Vec<(DaemonInfo, Attach)> {
    let order = |daemon: &DaemonInfo| version_order(&daemon.version, current_version);
    let older: Vec<_> = running
        .iter()
        .filter(|d| order(d) == Some(Ordering::Less) && d.protocol_major == this_major)
        .cloned()
        .collect();
    running
        .iter()
        .map(|daemon| {
            let attach = match order(daemon) {
                Some(Ordering::Greater) | None => Attach::Newer { daemon: daemon.clone() },
                Some(Ordering::Equal)
                    if daemon.version == current_version
                        && revision_matches(&daemon.rev, current_revision)
                        && daemon.protocol_major == this_major =>
                {
                    Attach::Same
                }
                Some(Ordering::Less) if daemon.protocol_major != this_major => {
                    Attach::OlderIncompatible { daemon: daemon.clone() }
                }
                Some(Ordering::Equal) if daemon.protocol_major != this_major => {
                    Attach::Newer { daemon: daemon.clone() }
                }
                _ => Attach::NewForNewWork { older: older.clone() },
            };
            (daemon.clone(), attach)
        })
        .collect()
}

fn revision_matches(a: &str, b: &str) -> bool {
    if !super::install::valid_revision(a) || !super::install::valid_revision(b) {
        return false;
    }
    if a.len() == b.len() {
        a == b
    } else {
        a[..7] == b[..7]
    }
}

// Semver precedence ignores build metadata; a revision is an identity, not an ordered version.
fn version_order(left: &str, right: &str) -> Option<Ordering> {
    fn identifiers(text: &str, numeric_zeroes: bool) -> Option<Vec<&str>> {
        text.split('.')
            .map(|part| {
                (!part.is_empty()
                    && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    && !(numeric_zeroes
                        && part.len() > 1
                        && part.bytes().all(|b| b.is_ascii_digit())
                        && part.starts_with('0')))
                .then_some(part)
            })
            .collect()
    }
    fn parse(text: &str) -> Option<([u64; 3], Vec<&str>)> {
        let version = if let Some((version, build)) = text.split_once('+') {
            identifiers(build, false)?;
            version
        } else {
            text
        };
        let (core, pre) = match version.split_once('-') {
            Some((core, pre)) => (core, identifiers(pre, true)?),
            None => (version, Vec::new()),
        };
        let numbers: Vec<_> = core
            .split('.')
            .map(|part| {
                if part.is_empty()
                    || !part.bytes().all(|b| b.is_ascii_digit())
                    || (part.len() > 1 && part.starts_with('0'))
                {
                    None
                } else {
                    part.parse().ok()
                }
            })
            .collect::<Option<_>>()?;
        Some((numbers.try_into().ok()?, pre))
    }
    let (a, a_pre) = parse(left)?;
    let (b, b_pre) = parse(right)?;
    if a != b {
        return Some(a.cmp(&b));
    }
    if a_pre.is_empty() || b_pre.is_empty() {
        return Some(a_pre.is_empty().cmp(&b_pre.is_empty()));
    }
    for (a, b) in a_pre.iter().zip(&b_pre) {
        let a_num = a.bytes().all(|b| b.is_ascii_digit());
        let b_num = b.bytes().all(|b| b.is_ascii_digit());
        let order = match (a_num, b_num) {
            (true, true) => a.len().cmp(&b.len()).then_with(|| a.cmp(b)),
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => a.cmp(b),
        };
        if order != Ordering::Equal {
            return Some(order);
        }
    }
    Some(a_pre.len().cmp(&b_pre.len()))
}

/// Whether `daemon` is newer than this build, or cannot be ordered against it: one this client must never stop. A
/// daemon of this build's version speaking another protocol major is a newer one too, as [`decide_attach`] reads it
/// (a development build whose protocol moved before its version did).
pub fn is_newer_than_this_build(daemon: &DaemonInfo) -> bool {
    match version_order(&daemon.version, env!("CARGO_PKG_VERSION")) {
        Some(Ordering::Less) => false,
        Some(Ordering::Equal) => daemon.protocol_major != agent::ssh::PROTOCOL_MAJOR,
        Some(Ordering::Greater) | None => true,
    }
}

/// Stops `daemon`, whatever its protocol, with the `stop-daemon` of an installation `host` records (`preferred`
/// first), that installation's recorded hash checked before it runs. `run` is the daemon's key as `list-daemons`
/// names it (its installation's name; a helper that keyed daemons by version alone gives none, and the version is the
/// key). Only for what the person asked for by name (`eitri host stop` and `uninstall`): a newer daemon is refused, as
/// everywhere.
pub fn stop_daemon_with(
    launcher: &dyn ChannelLauncher,
    host: &RemoteHost,
    preferred: Option<&str>,
    daemon: &DaemonInfo,
    run: &str,
) -> Result<(), InstallError> {
    let daemon_version = daemon.version.as_str();
    if is_newer_than_this_build(daemon) {
        return Err(InstallError::Invalid(format!(
            "Eitri server {daemon_version} is newer than this Eitri; stop it with that Eitri"
        )));
    }
    let mut installations: Vec<(&String, &String)> = host.helpers.iter().collect();
    installations.sort_by_key(|(name, _)| Some(name.as_str()) != preferred);
    let mut error = InstallError::Invalid("no installation of Eitri server is recorded for this host".into());
    for (installation, hash) in installations {
        match stop_with(launcher, host, installation, hash, run) {
            Ok(()) => return Ok(()),
            Err(failure) => error = failure,
        }
    }
    Err(error)
}

fn stop_with(
    launcher: &dyn ChannelLauncher,
    host: &RemoteHost,
    current: &str,
    hash: &str,
    version: &str,
) -> Result<(), InstallError> {
    if !super::install::safe_name(version) || !super::install::safe_name(current) {
        return Err(InstallError::Invalid("invalid daemon version".into()));
    }
    let manifest = super::manifest::Manifest {
        lines: vec![(hash.into(), "bin/eitri-server".into())],
    };
    let hex = agent::ssh::encode_args(&[OsStr::new(version)]);
    let args = [super::install::checked_path(&host.data_dir)?, current, &hex];
    let child = {
        let _key = launcher.hold_host_key(&host.host_key_sha256)?;
        super::install::attached(launcher.spawn_step(super::steps::STOP, &args)?)?
    };
    super::install::communicate(child, &manifest.render()?, &|_, _| {})?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn revision_identity_compares_full_commits_and_the_installed_rev7() {
        let mut same = daemon("0.3.0", 1);
        same.rev = format!("abcdef0{}", "1".repeat(33));
        assert_eq!(decide(&[same])[0].1, Attach::Same);
        assert!(!revision_matches(
            &format!("abcdef0{}", "1".repeat(33)),
            &format!("abcdef0{}", "2".repeat(33))
        ));
        assert!(!revision_matches("unknown", "unknown"));
    }

    fn daemon(version: &str, major: u32) -> DaemonInfo {
        DaemonInfo {
            version: version.into(),
            rev: "abcdef0".into(),
            pid: 1,
            protocol_major: major,
            protocol_minor: 0,
            started_unix_ms: 0,
        }
    }
    fn decide(running: &[DaemonInfo]) -> Vec<(DaemonInfo, Attach)> {
        decide_attach(1, "0.3.0", "abcdef0", running)
    }
    /// `eitri host stop` leaves alone what this build must not stop: a newer or unorderable version, and this
    /// version speaking another protocol major; an older one of any protocol, and this build's own, it stops.
    #[test]
    fn host_stop_never_takes_a_newer_daemon() {
        let this = env!("CARGO_PKG_VERSION");
        let major = agent::ssh::PROTOCOL_MAJOR;
        assert!(is_newer_than_this_build(&daemon("99.0.0", major)));
        assert!(is_newer_than_this_build(&daemon("junk", major)));
        assert!(is_newer_than_this_build(&daemon(this, major + 1)));
        assert!(!is_newer_than_this_build(&daemon(this, major)));
        assert!(!is_newer_than_this_build(&daemon("0.0.1", major + 1)));
    }

    #[test]
    fn newer_daemons_never_become_stop_candidates() {
        for major in [1, 2] {
            let choice = decide(&[daemon("99.0.0", major)]).remove(0).1;
            assert!(matches!(choice, Attach::Newer { .. }));
            assert!(choice.notice().unwrap().contains("attaches nothing"));
        }
    }
    #[test]
    fn same_requires_the_revision_too() {
        assert_eq!(decide(&[daemon("0.3.0", 1)])[0].1, Attach::Same);
        let mut other = daemon("0.3.0", 1);
        other.rev = "7654321".into();
        assert!(matches!(decide(&[other])[0].1, Attach::NewForNewWork { .. }));
    }
    #[test]
    fn older_compatible_daemon_keeps_its_tabs() {
        let older = daemon("0.2.0", 1);
        assert_eq!(
            decide(&[older.clone()])[0].1,
            Attach::NewForNewWork { older: vec![older] }
        );
        assert!(matches!(
            decide(&[daemon("0.2.0", 0)])[0].1,
            Attach::OlderIncompatible { .. }
        ));
    }
    #[test]
    fn semantic_versions_have_numeric_and_prerelease_precedence() {
        let ordered = [
            "0.3.0-alpha",
            "0.3.0-alpha.2",
            "0.3.0-alpha.10",
            "0.3.0-beta",
            "0.3.0",
            "0.10.0",
            "1.0.0",
        ];
        for pair in ordered.windows(2) {
            assert_eq!(version_order(pair[0], pair[1]), Some(Ordering::Less));
            assert_eq!(version_order(pair[1], pair[0]), Some(Ordering::Greater));
        }
        assert_eq!(version_order("0.3.0+build.2", "0.3.0+build.1"), Some(Ordering::Equal));
        for bad in ["junk", "0.3", "00.3.0", "0.3.0-01", "0.3.0+", "0.3.0++"] {
            assert_eq!(version_order(bad, "0.3.0"), None);
            assert!(matches!(decide(&[daemon(bad, 0)])[0].1, Attach::Newer { .. }));
        }
    }
    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn stop_checks_the_helper_and_executes_under_one_publication_lock() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_scratch_dir::ScratchDir::new("remote", "skew");
        let helper = dir.join("current-present/bin/eitri-server");
        std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
        std::fs::write(&helper, "#!/bin/sh\ntest \"$1\" = stop-daemon && test \"$2\" = --args-hex && test \"$3\" = 0000000b696e7374616c6c2d72756e || exit 1\nif flock -n -x ../.install.lock true; then exit 2; fi\nprintf stopped > stopped\n").unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let hash = super::super::manifest::hash_file(&helper).unwrap();
        let host = RemoteHost {
            version: 1,
            host_key_sha256: "SHA256:example".into(),
            data_dir: dir.to_path_buf(),
            state_dir: dir.join("state"),
            helpers: [
                ("current-absent".into(), hash.clone()),
                ("current-present".into(), hash.clone()),
            ]
            .into(),
            sidecars: Default::default(),
            consented: true,
        };
        let launcher = agent::ssh::LocalExec {
            helper: dir.join("fixture"),
        };
        // Refused: this version on another protocol major, a newer version, an unorderable one.
        let this = env!("CARGO_PKG_VERSION");
        let other_major = agent::ssh::PROTOCOL_MAJOR + 1;
        for candidate in [daemon(this, other_major), daemon("99.0.0", 0), daemon("junk", 0)] {
            assert!(stop_daemon_with(&launcher, &host, Some("current-absent"), &candidate, "install-run").is_err());
            assert!(!dir.join("current-present/stopped").exists());
        }
        stop_daemon_with(
            &launcher,
            &host,
            Some("current-absent"),
            &daemon("0.2.0", 0),
            "install-run",
        )
        .unwrap();
        assert!(dir.join("current-present/stopped").exists());
        std::fs::remove_file(dir.join("current-present/stopped")).unwrap();
        std::fs::write(helper, b"tampered").unwrap();
        assert!(stop_daemon_with(
            &launcher,
            &host,
            Some("current-absent"),
            &daemon("0.2.0", 0),
            "install-run"
        )
        .is_err());
        assert!(!dir.join("current-present/stopped").exists());
    }
}
