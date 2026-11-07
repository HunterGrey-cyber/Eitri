//! Binding what [`ServeConfig::listen`] names, and judging a peer at accept.
//!
//! - Loopback: `127.0.0.1` and, where the machine has it, `::1`, on the port.
//! - Socket: `remote.sock` in the runtime directory (through `remote_rv::serve_socket_path`), 0600, a peer served only
//!   when its uid is this process's.
//! - One address: only after the desk said yes to that address; until then nothing is bound and the reason is the
//!   error. Bound to its interface with `SO_BINDTODEVICE` where the kernel lets this user (Linux 5.7 and later, for a
//!   socket not yet bound to a device): Linux takes packets for an address on any interface, so without it a neighbour
//!   who routes the address's subnet through the desk's LAN address would reach the port. Where that is refused, a
//!   peer outside the interface's own networks is closed at accept instead.

use std::ffi::{CStr, CString};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use super::config::{Listen, ServeConfig};

/// What [`bind`] made.
#[derive(Debug)]
pub struct Listeners {
    pub tcp: Vec<TcpListener>,
    /// The Unix socket and its path.
    pub unix: Option<(UnixListener, PathBuf)>,
    /// For an address bound without `SO_BINDTODEVICE`: the networks of its interface a peer must be in.
    pub peer_ranges: Option<Vec<(IpAddr, u8)>>,
}

/// Where `listen` takes connections, as the audit log names it. A named address's link kind is read from
/// `/sys/class/net` here, so call it before the sandbox, which does not let `serve` read there.
pub fn audit_listener(listen: &Listen) -> eitri_core::remote_audit::Listener {
    use eitri_core::remote_audit::Listener;
    match listen {
        Listen::Loopback => Listener::Loopback,
        Listen::Socket => Listener::Socket,
        Listen::Address { addr, iface } => Listener::Address {
            addr: addr.to_string(),
            iface: iface.clone(),
            kind: eitri_core::remote_pairing::interface_kind(iface).as_str().to_owned(),
        },
    }
}

/// The address the desk confirmed, if any. `confirmed` is what was recorded: an address, or an address and port.
pub fn confirmed_ip(confirmed: Option<&str>) -> Option<IpAddr> {
    let text = confirmed?;
    text.parse::<IpAddr>()
        .ok()
        .or_else(|| text.parse::<SocketAddr>().ok().map(|s| s.ip()))
}

/// Binds what `cfg` names. `confirmed` is the address the desk said yes to (an address, or an address and port).
/// `Err` is the line to print.
pub fn bind(cfg: &ServeConfig, control_dir: &Path, confirmed: Option<&str>) -> Result<Listeners, String> {
    match &cfg.listen {
        Listen::Loopback => {
            let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, cfg.port))
                .map_err(|e| format!("could not listen on 127.0.0.1:{}: {e}", cfg.port))?;
            let mut tcp = vec![v4];
            match TcpListener::bind((Ipv6Addr::LOCALHOST, cfg.port)) {
                Ok(v6) => tcp.push(v6),
                // A machine without IPv6 on loopback still serves IPv4.
                Err(e) if matches!(e.raw_os_error(), Some(libc::EADDRNOTAVAIL | libc::EAFNOSUPPORT)) => {}
                Err(e) => return Err(format!("could not listen on [::1]:{}: {e}", cfg.port)),
            }
            Ok(Listeners {
                tcp,
                unix: None,
                peer_ranges: None,
            })
        }
        Listen::Socket => {
            let path = eitri_core::remote_rv::serve_socket_path(control_dir)
                .map_err(|e| format!("no socket path in {}: {e}", control_dir.display()))?;
            remove_stale_socket(&path)?;
            let listener =
                UnixListener::bind(&path).map_err(|e| format!("could not listen on {}: {e}", path.display()))?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("could not set the mode of {}: {e}", path.display()))?;
            Ok(Listeners {
                tcp: Vec::new(),
                unix: Some((listener, path)),
                peer_ranges: None,
            })
        }
        Listen::Address { addr, iface } => {
            if confirmed_ip(confirmed) != Some(*addr) {
                return Err(format!(
                    "{addr} on {iface} is not confirmed at the desk: a window whose remote.listen names it asks there \
                     first, and nothing is bound until it is answered yes"
                ));
            }
            let (listener, bound_to_device) = bind_address(*addr, iface, cfg.port)?;
            let peer_ranges = if bound_to_device {
                None
            } else {
                eprintln!("[remote] {iface} could not be bound to; peers outside its networks are closed at accept");
                Some(interface_networks(iface).map_err(|e| format!("could not read the networks of {iface}: {e}"))?)
            };
            Ok(Listeners {
                tcp: vec![listener],
                unix: None,
                peer_ranges,
            })
        }
    }
}

/// A socket left at `path` by an earlier `serve` of this user: the lock is held now, so no other `serve` owns it.
pub(super) fn remove_stale_socket(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() && meta.uid() == agent::private_fs::euid() => {
            std::fs::remove_file(path).map_err(|e| format!("could not remove the old socket {}: {e}", path.display()))
        }
        Ok(_) => Err(format!(
            "{} is in the way and is not an old socket of this user",
            path.display()
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("could not read {}: {e}", path.display())),
    }
}

/// A listening TCP socket on `addr:port`, and whether it is bound to `iface`.
fn bind_address(addr: IpAddr, iface: &str, port: u16) -> Result<(TcpListener, bool), String> {
    let fail = |what: &str, e: io::Error| format!("could not {what} for {addr} on {iface}: {e}");
    let family = if addr.is_ipv4() { libc::AF_INET } else { libc::AF_INET6 };
    // SAFETY: plain system call; the descriptor is owned at once below.
    let raw = unsafe { libc::socket(family, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(fail("open a socket", io::Error::last_os_error()));
    }
    // SAFETY: `socket` just returned this descriptor and nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let raw: RawFd = std::os::fd::AsRawFd::as_raw_fd(&fd);
    set_int(raw, libc::SOL_SOCKET, libc::SO_REUSEADDR, 1).map_err(|e| fail("set SO_REUSEADDR", e))?;
    if addr.is_ipv6() {
        set_int(raw, libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, 1).map_err(|e| fail("set IPV6_V6ONLY", e))?;
    }
    let name = CString::new(iface).map_err(|_| format!("{iface:?} is not an interface name"))?;
    let bound_to_device = match bind_to_device(raw, &name) {
        Ok(()) => true,
        Err(e) if e.raw_os_error() == Some(libc::EPERM) => false,
        Err(e) => return Err(fail("bind to the interface", e)),
    };
    let sockaddr = SocketAddr::new(addr, port);
    // SAFETY: the storage is zeroed and filled for the family, and its length passed with it.
    let rc = unsafe {
        let mut storage: libc::sockaddr_storage = std::mem::zeroed();
        let len = match sockaddr {
            SocketAddr::V4(v4) => {
                let sin = &mut *(&mut storage as *mut _ as *mut libc::sockaddr_in);
                sin.sin_family = libc::AF_INET as libc::sa_family_t;
                sin.sin_port = v4.port().to_be();
                sin.sin_addr = libc::in_addr {
                    s_addr: u32::from_ne_bytes(v4.ip().octets()),
                };
                std::mem::size_of::<libc::sockaddr_in>()
            }
            SocketAddr::V6(v6) => {
                let sin6 = &mut *(&mut storage as *mut _ as *mut libc::sockaddr_in6);
                sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                sin6.sin6_port = v6.port().to_be();
                sin6.sin6_addr = libc::in6_addr {
                    s6_addr: v6.ip().octets(),
                };
                sin6.sin6_scope_id = v6.scope_id();
                std::mem::size_of::<libc::sockaddr_in6>()
            }
        };
        libc::bind(
            raw,
            &storage as *const _ as *const libc::sockaddr,
            len as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(fail("bind", io::Error::last_os_error()));
    }
    // SAFETY: plain system call on the descriptor owned here.
    if unsafe { libc::listen(raw, 128) } != 0 {
        return Err(fail("listen", io::Error::last_os_error()));
    }
    Ok((TcpListener::from(fd), bound_to_device))
}

fn set_int(fd: RawFd, level: libc::c_int, name: libc::c_int, value: libc::c_int) -> io::Result<()> {
    // SAFETY: `value` lives for the call and its exact size is passed.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            &value as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn bind_to_device(fd: RawFd, iface: &CStr) -> io::Result<()> {
    let bytes = iface.to_bytes_with_nul();
    // SAFETY: `bytes` lives for the call and its exact length is passed.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            bytes.as_ptr() as *const libc::c_void,
            bytes.len() as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Every address of this machine with the interface that holds it and its prefix length.
pub fn interface_addresses() -> io::Result<Vec<(String, IpAddr, u8)>> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `getifaddrs` fills `head` with a list freed below with `freeifaddrs`.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut out = Vec::new();
    let mut at = head;
    while !at.is_null() {
        // SAFETY: `at` is an entry of the list `getifaddrs` returned, which stays allocated until `freeifaddrs`.
        let entry = unsafe { &*at };
        at = entry.ifa_next;
        if entry.ifa_addr.is_null() || entry.ifa_name.is_null() {
            continue;
        }
        // SAFETY: a NUL-terminated name owned by the list.
        let name = unsafe { CStr::from_ptr(entry.ifa_name) }.to_string_lossy().into_owned();
        // SAFETY: `ifa_addr` and `ifa_netmask` point at socket addresses of the family their first field names.
        let (addr, mask) = unsafe {
            match i32::from((*entry.ifa_addr).sa_family) {
                libc::AF_INET => {
                    let a = &*(entry.ifa_addr as *const libc::sockaddr_in);
                    let addr = IpAddr::V4(Ipv4Addr::from(a.sin_addr.s_addr.to_ne_bytes()));
                    let mask = (!entry.ifa_netmask.is_null()).then(|| {
                        let m = &*(entry.ifa_netmask as *const libc::sockaddr_in);
                        u32::from_ne_bytes(m.sin_addr.s_addr.to_ne_bytes()).count_ones() as u8
                    });
                    (addr, mask.unwrap_or(32))
                }
                libc::AF_INET6 => {
                    let a = &*(entry.ifa_addr as *const libc::sockaddr_in6);
                    let addr = IpAddr::V6(Ipv6Addr::from(a.sin6_addr.s6_addr));
                    let mask = (!entry.ifa_netmask.is_null()).then(|| {
                        let m = &*(entry.ifa_netmask as *const libc::sockaddr_in6);
                        u128::from_be_bytes(m.sin6_addr.s6_addr).count_ones() as u8
                    });
                    (addr, mask.unwrap_or(128))
                }
                _ => continue,
            }
        };
        out.push((name, addr, mask));
    }
    // SAFETY: the list `getifaddrs` returned, freed once.
    unsafe { libc::freeifaddrs(head) };
    Ok(out)
}

/// The interface that holds `addr`, if this machine has it.
pub fn iface_of(addr: IpAddr) -> Option<String> {
    interface_addresses()
        .ok()?
        .into_iter()
        .find(|(_, a, _)| *a == addr)
        .map(|(name, _, _)| name)
}

/// The networks (`address`, prefix length) of `iface`.
pub fn interface_networks(iface: &str) -> io::Result<Vec<(IpAddr, u8)>> {
    Ok(interface_addresses()?
        .into_iter()
        .filter(|(name, _, _)| name == iface)
        .map(|(_, addr, prefix)| (addr, prefix))
        .collect())
}

/// Whether `peer` is in one of `networks`. An IPv4 peer reaching an IPv6 socket is read as its IPv4 address.
pub fn peer_in(peer: IpAddr, networks: &[(IpAddr, u8)]) -> bool {
    let peer = match peer {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(peer, IpAddr::V4),
        v4 => v4,
    };
    networks.iter().any(|(net, prefix)| match (peer, net) {
        (IpAddr::V4(p), IpAddr::V4(n)) => {
            let bits = u32::from(*prefix.min(&32));
            let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
            u32::from(p) & mask == u32::from(*n) & mask
        }
        (IpAddr::V6(p), IpAddr::V6(n)) => {
            let bits = u32::from(*prefix.min(&128));
            let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
            u128::from(p) & mask == u128::from(*n) & mask
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            PathBuf::from(option_env!("CARGO_TARGET_DIR").unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../target")))
                .join("tmp/remote-listen-tests")
                .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    /// An address this machine has that is not loopback, and its interface; `None` on a machine with none.
    fn own_address() -> Option<(IpAddr, String)> {
        interface_addresses()
            .unwrap()
            .into_iter()
            .find(|(_, addr, _)| !addr.is_loopback() && addr.is_ipv4())
            .map(|(name, addr, _)| (addr, name))
    }

    #[test]
    fn unconfirmed_address_is_not_bound() {
        let Some((addr, iface)) = own_address() else {
            eprintln!("no address but loopback on this machine: nothing to bind");
            return;
        };
        let probe = TcpListener::bind((addr, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let cfg = ServeConfig {
            listen: Listen::Address {
                addr,
                iface: iface.clone(),
            },
            port,
            origins: Vec::new(),
            audit_keep_days: eitri_core::remote_audit::AUDIT_KEEP_DAYS_DEFAULT,
        };
        let dir = scratch("unconfirmed");
        for confirmed in [None, Some("192.0.2.1"), Some("not an address")] {
            let err = bind(&cfg, &dir, confirmed).unwrap_err();
            assert!(err.contains("not confirmed at the desk"), "{confirmed:?}: {err}");
            assert!(
                std::net::TcpStream::connect((addr, port)).is_err(),
                "{confirmed:?}: something listens on {addr}:{port}"
            );
        }
        let with_port = format!("{}", SocketAddr::new(addr, port));
        let bound = bind(&cfg, &dir, Some(&with_port)).unwrap();
        assert_eq!(bound.tcp.len(), 1);
        assert_eq!(bound.tcp[0].local_addr().unwrap(), SocketAddr::new(addr, port));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_socket_is_private_and_replaces_an_old_one() {
        let dir = scratch("socket");
        let cfg = ServeConfig {
            listen: Listen::Socket,
            port: 41000,
            origins: Vec::new(),
            audit_keep_days: eitri_core::remote_audit::AUDIT_KEEP_DAYS_DEFAULT,
        };
        let first = bind(&cfg, &dir, None).unwrap();
        let (_, path) = first.unix.as_ref().unwrap();
        let mode = std::fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        drop(first);
        let again = bind(&cfg, &dir, None).unwrap();
        assert!(again.unix.is_some());
        drop(again);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("remote.sock"), b"not a socket").unwrap();
        assert!(bind(&cfg, &dir, None).unwrap_err().contains("in the way"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn peers_are_judged_by_the_interface_networks() {
        let nets: Vec<(IpAddr, u8)> = vec![
            ("198.51.100.1".parse().unwrap(), 24),
            ("2001:db8:7::1".parse().unwrap(), 64),
        ];
        assert!(peer_in("198.51.100.9".parse().unwrap(), &nets));
        assert!(peer_in("::ffff:198.51.100.9".parse().unwrap(), &nets));
        assert!(peer_in("2001:db8:7::abcd".parse().unwrap(), &nets));
        assert!(!peer_in("198.51.101.9".parse().unwrap(), &nets));
        assert!(!peer_in("203.0.113.5".parse().unwrap(), &nets));
        assert!(!peer_in("2001:db8:8::1".parse().unwrap(), &nets));
    }

    #[test]
    fn confirmed_reads_an_address_with_or_without_its_port() {
        assert_eq!(
            confirmed_ip(Some("198.51.100.2")),
            Some("198.51.100.2".parse().unwrap())
        );
        assert_eq!(
            confirmed_ip(Some("198.51.100.2:7341")),
            Some("198.51.100.2".parse().unwrap())
        );
        assert_eq!(
            confirmed_ip(Some("[2001:db8::1]:7341")),
            Some("2001:db8::1".parse().unwrap())
        );
        assert_eq!(confirmed_ip(Some("desk")), None);
        assert_eq!(confirmed_ip(None), None);
    }
}
