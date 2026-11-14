//! The TCP port `serve` listens on when `--port` is not given: chosen once for this user, then kept, so a phone's saved
//! address and an SSH forward written for it keep working across restarts.

use std::io;
use std::ops::RangeInclusive;
use std::path::Path;

/// The ports a kept port file may hold: above the ports services usually take. A port kept from an earlier choice
/// anywhere in it stays valid, since a phone may rely on it.
pub const PORT_RANGE: RangeInclusive<u16> = 20000..=60999;

/// Where a new port is chosen: the part of [`PORT_RANGE`] below 32768, where Linux's default ephemeral range
/// (`net.ipv4.ip_local_port_range`, 32768-60999) starts. A port in that range can be held by any outgoing connection
/// to a local address on this machine, so `serve` would fail to bind it now and then.
pub const PICK_RANGE: RangeInclusive<u16> = 20000..=32767;

/// The file under the remote state directory that keeps the port.
const PORT_FILE: &str = "port";

/// The port kept in `state_dir` (`<state home>/eitri/remote`), chosen at random in [`PICK_RANGE`] and stored 0600 the
/// first time. A stored value that is not a port in the range is an error naming the file, never replaced: a phone may
/// rely on it.
pub fn stable_port(state_dir: &Path) -> io::Result<u16> {
    stable_port_with(state_dir, random_port)
}

/// [`stable_port`] with the first choice made by `pick`.
pub fn stable_port_with(state_dir: &Path, pick: impl FnOnce() -> io::Result<u16>) -> io::Result<u16> {
    let path = state_dir.join(PORT_FILE);
    match read_port(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        other => return other,
    }
    eitri_core::state_paths::create_state_dir(state_dir)?;
    let port = pick()?;
    // Written whole under a name of this process's own and linked into place, so a reader never sees half of it,
    // and of two first starts at once the one that links first decides.
    let temporary = state_dir.join(format!("{PORT_FILE}.{}.tmp", std::process::id()));
    agent::private_fs::write_private(&temporary, format!("{port}\n").as_bytes())?;
    let linked = std::fs::hard_link(&temporary, &path);
    std::fs::remove_file(&temporary)?;
    match linked {
        Ok(()) => Ok(port),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => read_port(&path),
        Err(e) => Err(e),
    }
}

fn read_port(path: &Path) -> io::Result<u16> {
    let text = agent::private_fs::read_private_nofollow_capped(path, 16)?;
    text.trim_end_matches('\n')
        .parse::<u16>()
        .ok()
        .filter(|port| PORT_RANGE.contains(port))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} does not hold a port from {} to {}",
                    path.display(),
                    PORT_RANGE.start(),
                    PORT_RANGE.end()
                ),
            )
        })
}

fn random_port() -> io::Result<u16> {
    let mut bytes = [0u8; 32];
    eitri_core::ide_server::handshake::getrandom_fill(&mut bytes)?;
    let span = u32::from(PICK_RANGE.end() - PICK_RANGE.start()) + 1;
    let n = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) % span;
    Ok(PICK_RANGE.start() + n as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(
            option_env!("CARGO_TARGET_DIR").unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../target")),
        )
        .join("tmp/remote-port-tests")
        .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn stable_port_is_kept_across_starts() {
        let dir = scratch("kept").join("eitri/remote");
        let first = stable_port(&dir).unwrap();
        assert!(PORT_RANGE.contains(&first));
        let mode = std::fs::metadata(dir.join(PORT_FILE)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        for _ in 0..3 {
            assert_eq!(
                stable_port_with(&dir, || panic!("a kept port is not chosen again")).unwrap(),
                first
            );
        }
        std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn a_damaged_port_file_is_an_error_not_a_new_port() {
        let dir = scratch("damaged");
        std::fs::create_dir_all(&dir).unwrap();
        for bad in ["80\n", "seven\n", "", "70000\n"] {
            agent::private_fs::write_private(&dir.join(PORT_FILE), bad.as_bytes()).unwrap();
            let err = stable_port_with(&dir, || Ok(30000)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{bad:?}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn random_ports_stay_in_the_range() {
        for _ in 0..64 {
            let port = random_port().unwrap();
            assert!(PICK_RANGE.contains(&port) && PORT_RANGE.contains(&port), "{port}");
        }
    }

    #[test]
    fn a_new_port_is_never_one_linux_hands_to_outgoing_connections() {
        assert!(PORT_RANGE.contains(PICK_RANGE.start()) && PORT_RANGE.contains(PICK_RANGE.end()));
        // Linux's default `net.ipv4.ip_local_port_range`.
        let (low, high) = (32768, 60999);
        assert!(
            *PICK_RANGE.end() < low || *PICK_RANGE.start() > high,
            "ports are chosen in {PICK_RANGE:?}, which meets the ephemeral range {low}-{high}"
        );
    }

    #[test]
    fn a_port_kept_from_the_wider_range_is_still_used() {
        let dir = scratch("kept-high");
        std::fs::create_dir_all(&dir).unwrap();
        agent::private_fs::write_private(&dir.join(PORT_FILE), b"45000\n").unwrap();
        assert_eq!(
            stable_port_with(&dir, || panic!("a kept port is not chosen again")).unwrap(),
            45000
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
