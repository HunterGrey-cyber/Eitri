//! The `Host` and `Origin` a request must carry, checked before anything else is done with it.
//!
//! A browser page on the desk can reach a listener on loopback: through DNS rebinding (its own name resolved to
//! `127.0.0.1`, so its requests carry its own name as `Host`) or by opening a WebSocket to the port from another site
//! (its `Origin` names that site). Both are refused here. A native client sends no `Origin` and is accepted; the device
//! token, never a cookie, is the second wall, so a page has nothing ambient to ride on.
//!
//! `Host` is accepted when it is any loopback name on any port (`127.0.0.1`, `[::1]`, `localhost`, `*.localhost`: an
//! SSH forward may use any port on the phone's side, and a rebinding page always sends its own name), the listen
//! address with this port, or one of `--origin`'s names. `Origin`, when sent, must be `http` or `https` with the same
//! authority rule, and match the scheme of an `--origin` entry that names one.

use std::net::IpAddr;

use super::config::{split_authority, Listen, ServeConfig};

/// Why a request's `Host` was refused: a constant, so nothing of the request reaches a log.
pub const HOST_REFUSED: &str = "its Host is not a name this server answers to";
/// Why a request's `Origin` was refused.
pub const ORIGIN_REFUSED: &str = "its Origin is not a page this server answers to";

/// `Ok` when `host` (the `Host` header) and `origin` (the `Origin` header, absent for a native client) are ones this
/// server answers to under `cfg`; `Err` names the header refused.
pub fn check_request(host: &str, origin: Option<&str>, cfg: &ServeConfig) -> Result<(), &'static str> {
    let host = host.to_ascii_lowercase();
    if !authority_allowed(&host, None, cfg) {
        return Err(HOST_REFUSED);
    }
    if let Some(origin) = origin {
        let origin = origin.to_ascii_lowercase();
        let Some((scheme, authority)) = origin.split_once("://") else {
            return Err(ORIGIN_REFUSED);
        };
        if !matches!(scheme, "http" | "https") || !authority_allowed(authority, Some(scheme), cfg) {
            return Err(ORIGIN_REFUSED);
        }
    }
    Ok(())
}

/// Whether the lowercase `authority` is a loopback name on any port, the listen address on this port, or an
/// `--origin` name. With `scheme` (an `Origin`'s), an `--origin` entry that names a scheme must name that one.
/// The audit log's word for a refusal of the handshake ([`HOST_REFUSED`], [`ORIGIN_REFUSED`], the path's), `None`
/// for one it has no word for (a malformed request, a request that asks no upgrade).
pub fn refusal_reason(why: &str) -> Option<eitri_core::remote_audit::Reason> {
    use eitri_core::remote_audit::Reason;
    match why {
        HOST_REFUSED => Some(Reason::Host),
        ORIGIN_REFUSED => Some(Reason::Origin),
        super::ws::PATH_REFUSED => Some(Reason::PathPrefix),
        _ => None,
    }
}

fn authority_allowed(authority: &str, scheme: Option<&str>, cfg: &ServeConfig) -> bool {
    let Some((host, port)) = split_authority(authority) else {
        return false;
    };
    if is_loopback_name(host) {
        return true;
    }
    if let Listen::Address { addr, .. } = &cfg.listen {
        if port == Some(cfg.port) && address_of(host) == Some(*addr) {
            return true;
        }
    }
    cfg.origins.iter().any(|entry| match entry.split_once("://") {
        Some((entry_scheme, entry_authority)) => {
            entry_authority == authority && scheme.is_none_or(|scheme| scheme == entry_scheme)
        }
        None => entry == authority,
    })
}

/// `127.0.0.1`, `[::1]`, `localhost` or a name under `localhost.`, which resolvers answer with loopback only.
fn is_loopback_name(host: &str) -> bool {
    host == "127.0.0.1" || host == "[::1]" || host == "localhost" || host.ends_with(".localhost")
}

/// The address an authority's host spells, brackets taken off an IPv6 one.
fn address_of(host: &str) -> Option<IpAddr> {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(listen: Listen, origins: &[&str]) -> ServeConfig {
        ServeConfig {
            listen,
            port: 41000,
            origins: origins.iter().map(|o| (*o).to_owned()).collect(),
            audit_keep_days: eitri_core::remote_audit::AUDIT_KEEP_DAYS_DEFAULT,
        }
    }

    fn tailnet() -> Listen {
        Listen::Address {
            addr: "198.51.100.2".parse().unwrap(),
            iface: "wg0".to_owned(),
        }
    }

    #[test]
    fn rebinding_host_is_refused() {
        let cfg = cfg(Listen::Loopback, &[]);
        for host in [
            "evil.example",
            "evil.example:41000",
            "127.0.0.1.evil.example",
            "localhost.evil.example",
            "127.0.0.2:41000",
            "198.51.100.2:41000",
            "evil.example@127.0.0.1",
            "",
            "*.localhost",
        ] {
            assert_eq!(check_request(host, None, &cfg), Err(HOST_REFUSED), "{host:?}");
        }
    }

    #[test]
    fn foreign_origin_is_refused() {
        let cfg = cfg(Listen::Loopback, &["https://desk.tail1.ts.net"]);
        for origin in [
            "https://evil.example",
            "http://evil.example:41000",
            "null",
            "",
            "file://",
            "chrome-extension://abc",
            "https://desk.tail1.ts.net.evil.example",
            "http://desk.tail1.ts.net",
            "https://desk.tail1.ts.net/path",
        ] {
            assert_eq!(
                check_request("127.0.0.1:41000", Some(origin), &cfg),
                Err(ORIGIN_REFUSED),
                "{origin:?}"
            );
        }
    }

    #[test]
    fn absent_origin_is_accepted() {
        let cfg = cfg(Listen::Loopback, &[]);
        assert_eq!(check_request("127.0.0.1:41000", None, &cfg), Ok(()));
        assert_eq!(
            check_request("127.0.0.1:41000", Some("http://localhost:5173"), &cfg),
            Ok(())
        );
    }

    #[test]
    fn any_loopback_port_is_accepted() {
        let cfg = cfg(Listen::Loopback, &[]);
        for host in [
            "127.0.0.1",
            "127.0.0.1:41000",
            "127.0.0.1:2222",
            "[::1]:9",
            "[::1]",
            "localhost:8080",
            "LOCALHOST",
            "phone.localhost:7000",
        ] {
            assert_eq!(check_request(host, None, &cfg), Ok(()), "{host:?}");
            assert_eq!(
                check_request(host, Some(&format!("http://{host}")), &cfg),
                Ok(()),
                "{host:?} as its own origin"
            );
        }
        assert_eq!(check_request("127.0.0.1:x", None, &cfg), Err(HOST_REFUSED));
    }

    #[test]
    fn the_listen_address_counts_only_with_the_port() {
        let cfg = cfg(tailnet(), &[]);
        assert_eq!(check_request("198.51.100.2:41000", None, &cfg), Ok(()));
        assert_eq!(check_request("198.51.100.2:41001", None, &cfg), Err(HOST_REFUSED));
        assert_eq!(check_request("198.51.100.2", None, &cfg), Err(HOST_REFUSED));
        let loopback = super::super::config::ServeConfig {
            listen: Listen::Loopback,
            ..cfg
        };
        assert_eq!(check_request("198.51.100.2:41000", None, &loopback), Err(HOST_REFUSED));
    }

    #[test]
    fn tailscale_and_proxy_names_from_origins() {
        // Tailscale Serve passes its public name on 443; a proxy may pass the client's name with a port, or its
        // upstream's (a loopback name, accepted anyway).
        let cfg = cfg(
            Listen::Loopback,
            &["desk.tail1.ts.net", "https://eitri.example.org", "proxy.example:8443"],
        );
        assert_eq!(
            check_request("desk.tail1.ts.net", Some("https://desk.tail1.ts.net"), &cfg),
            Ok(())
        );
        assert_eq!(
            check_request("eitri.example.org", Some("https://eitri.example.org"), &cfg),
            Ok(())
        );
        assert_eq!(
            check_request("proxy.example:8443", Some("https://proxy.example:8443"), &cfg),
            Ok(())
        );
        assert_eq!(
            check_request("127.0.0.1:41000", Some("https://eitri.example.org"), &cfg),
            Ok(())
        );
        assert_eq!(check_request("proxy.example", None, &cfg), Err(HOST_REFUSED));
        assert_eq!(
            check_request("eitri.example.org", Some("http://eitri.example.org"), &cfg),
            Err(ORIGIN_REFUSED)
        );
        assert_eq!(check_request("other.ts.net", None, &cfg), Err(HOST_REFUSED));
    }

    #[test]
    fn each_refusal_names_its_audit_reason() {
        use eitri_core::remote_audit::Reason;
        assert_eq!(refusal_reason(HOST_REFUSED), Some(Reason::Host));
        assert_eq!(refusal_reason(ORIGIN_REFUSED), Some(Reason::Origin));
        assert_eq!(refusal_reason(super::super::ws::PATH_REFUSED), Some(Reason::PathPrefix));
        assert_eq!(refusal_reason("not a well-formed request"), None);
    }
}
