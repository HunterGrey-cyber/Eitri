//! What `eitri remote serve` listens on and answers to: given on its command line by the window that starts it, and
//! recorded beside its lock so a window that wants something else can say what differs.
//!
//! `serve [--listen loopback|socket|<address>] [--port <port>] [--origin <name>]...`
//!
//! - `--listen`: `loopback` (the default: `127.0.0.1` and `::1`), `socket` (a Unix socket in the runtime directory), or
//!   exactly one address of this machine. A wildcard (`0.0.0.0`, `::`) is refused: it would take every network the
//!   machine is on, and the person at the desk confirmed one. A loopback address given by name is `loopback`.
//! - `--port`: the TCP port; without it, the port chosen once for this user ([`super::port::stable_port`]).
//! - `--origin`: a public name a proxy or Tailscale Serve reaches the server by, as a bare host (`desk.tail1.ts.net`,
//!   with `:<port>` when it is not the scheme's default) or an origin (`https://desk.tail1.ts.net`). A path, a query,
//!   user information or a wildcard is refused: the server answers on one path only, at the root of its origin.

use std::collections::BTreeSet;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// Where the server listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listen {
    /// `127.0.0.1` and `::1` on the port.
    Loopback,
    /// `remote.sock` in the runtime directory, served to this user's processes only.
    Socket,
    /// One address of this machine, on the interface that holds it. Bound only after the desk said yes to it.
    Address { addr: IpAddr, iface: String },
}

/// What a `serve` listens on and answers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeConfig {
    pub listen: Listen,
    pub port: u16,
    /// Normalised entries of `--origin`: a lowercase authority (`name` or `name:port`), or a lowercase origin
    /// (`https://name[:port]`).
    pub origins: Vec<String>,
}

/// The longest `--origin` entry taken: a DNS name is at most 253 bytes, and a scheme and port add a few.
const ORIGIN_MAX_BYTES: usize = 300;

impl ServeConfig {
    /// Reads `serve`'s arguments. `port` is asked only when `--port` is absent; `iface_of` names the interface that
    /// holds an address, `None` for an address this machine does not have. `Err` is the line to print.
    pub fn from_args(
        args: &[String],
        port: impl FnOnce() -> std::io::Result<u16>,
        iface_of: impl Fn(IpAddr) -> Option<String>,
    ) -> Result<ServeConfig, String> {
        let mut listen = None;
        let mut given_port = None;
        let mut origins = Vec::new();
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            let mut value = |flag: &str| rest.next().cloned().ok_or_else(|| format!("{flag} needs a value"));
            match arg.as_str() {
                "--listen" if listen.is_none() => listen = Some(parse_listen(&value("--listen")?, &iface_of)?),
                "--port" if given_port.is_none() => given_port = Some(parse_port(&value("--port")?)?),
                "--origin" => {
                    let entry = parse_origin(&value("--origin")?)?;
                    if !origins.contains(&entry) {
                        origins.push(entry);
                    }
                }
                "--listen" | "--port" => return Err(format!("{arg} is given twice")),
                other => return Err(format!("unknown argument {other:?}")),
            }
        }
        let port = match given_port {
            Some(port) => port,
            None => port().map_err(|e| format!("could not settle the port: {e}"))?,
        };
        Ok(ServeConfig {
            listen: listen.unwrap_or(Listen::Loopback),
            port,
            origins,
        })
    }

    /// The command line that gives this configuration, the inverse of [`ServeConfig::from_args`] with the port
    /// always written out.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            "--listen".to_owned(),
            self.listen_word(),
            "--port".to_owned(),
            self.port.to_string(),
        ];
        for origin in &self.origins {
            args.push("--origin".to_owned());
            args.push(origin.clone());
        }
        args
    }

    fn listen_word(&self) -> String {
        match &self.listen {
            Listen::Loopback => "loopback".to_owned(),
            Listen::Socket => "socket".to_owned(),
            Listen::Address { addr, .. } => addr.to_string(),
        }
    }

    /// Where it listens, in words: `loopback`, `socket`, or `<address> on <interface>`.
    pub fn listen_text(&self) -> String {
        match &self.listen {
            Listen::Address { addr, iface } => format!("{addr} on {iface}"),
            _ => self.listen_word(),
        }
    }

    /// How `other` (what a window asks for) differs from this (what runs), one line per difference, each naming
    /// what runs and what was asked; empty when they agree. The order of `--origin` entries does not count.
    pub fn differences(&self, other: &ServeConfig) -> Vec<String> {
        let mut out = Vec::new();
        if self.listen != other.listen {
            out.push(format!(
                "listen: running {}, asked {}",
                self.listen_text(),
                other.listen_text()
            ));
        }
        if self.port != other.port {
            out.push(format!("port: running {}, asked {}", self.port, other.port));
        }
        let ours: BTreeSet<&String> = self.origins.iter().collect();
        let theirs: BTreeSet<&String> = other.origins.iter().collect();
        if ours != theirs {
            let list = |set: &BTreeSet<&String>| {
                if set.is_empty() {
                    "none".to_owned()
                } else {
                    set.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ")
                }
            };
            out.push(format!("origins: running {}, asked {}", list(&ours), list(&theirs)));
        }
        out
    }
}

/// `serve`'s record of what runs, kept in its record file while it holds the lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub v: u32,
    pub pid: u32,
    /// The command line of [`ServeConfig::to_args`].
    pub args: Vec<String>,
    /// `full` or `partial` ([`super::sandbox::SandboxState::word`]).
    pub sandbox: String,
}

/// The version of [`Record`] this build writes and reads.
pub const RECORD_V: u32 = 1;

fn parse_listen(value: &str, iface_of: &impl Fn(IpAddr) -> Option<String>) -> Result<Listen, String> {
    match value {
        "loopback" => return Ok(Listen::Loopback),
        "socket" => return Ok(Listen::Socket),
        _ => {}
    }
    let addr: IpAddr = value
        .parse()
        .map_err(|_| format!("--listen takes loopback, socket or one address, not {value:?}"))?;
    if addr.is_unspecified() {
        return Err(format!(
            "--listen refuses the wildcard address {addr}: name the one address phones reach this machine by"
        ));
    }
    if addr.is_multicast() {
        return Err(format!("--listen refuses the multicast address {addr}"));
    }
    if addr.is_loopback() {
        return Ok(Listen::Loopback);
    }
    let iface = iface_of(addr).ok_or_else(|| format!("{addr} is not an address of this machine"))?;
    Ok(Listen::Address { addr, iface })
}

fn parse_port(value: &str) -> Result<u16, String> {
    match value.parse::<u16>() {
        Ok(port) if port != 0 && value.bytes().all(|b| b.is_ascii_digit()) => Ok(port),
        _ => Err(format!("--port takes a port from 1 to 65535, not {value:?}")),
    }
}

/// One `--origin` entry, normalised to lowercase: `name`, `name:port`, or `http(s)://name[:port]`.
pub(crate) fn parse_origin(value: &str) -> Result<String, String> {
    let refuse = |why: &str| Err(format!("--origin {value:?}: {why}"));
    if value.is_empty() || value.len() > ORIGIN_MAX_BYTES {
        return refuse("not a host name");
    }
    let lower = value.to_ascii_lowercase();
    let (scheme, authority) = match lower.split_once("://") {
        Some((scheme @ ("http" | "https"), rest)) => (Some(scheme), rest),
        Some(_) => return refuse("only http and https origins"),
        None => (None, lower.as_str()),
    };
    if authority.contains(['/', '?', '#']) {
        return refuse("a path is refused: the server answers at the root of its origin only");
    }
    if authority.contains('@') {
        return refuse("user information is refused");
    }
    if split_authority(authority).is_none() {
        return refuse("not a host name with an optional port");
    }
    Ok(match scheme {
        Some(scheme) => format!("{scheme}://{authority}"),
        None => authority.to_owned(),
    })
}

/// A lowercase authority's host and port: `name`, `name:port`, `[v6]`, `[v6]:port`, the host checked for the
/// characters a DNS name or an address may hold. `None` for anything else, a wildcard or an empty label included.
pub(crate) fn split_authority(authority: &str) -> Option<(&str, Option<u16>)> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (inside, after) = rest.split_once(']')?;
        inside.parse::<std::net::Ipv6Addr>().ok()?;
        let host = &authority[..inside.len() + 2];
        match after {
            "" => (host, None),
            _ => (host, Some(after.strip_prefix(':')?)),
        }
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    let port = match port {
        None => None,
        Some(digits) if !digits.is_empty() && digits.len() <= 5 && digits.bytes().all(|b| b.is_ascii_digit()) => {
            Some(digits.parse::<u16>().ok().filter(|p| *p != 0)?)
        }
        Some(_) => return None,
    };
    if !host.starts_with('[') {
        let labels_ok = !host.is_empty()
            && host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            });
        if !labels_ok {
            return None;
        }
    }
    Some((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    fn tailnet(addr: IpAddr) -> Option<String> {
        (addr.to_string() == "198.51.100.2").then(|| "wg0".to_owned())
    }

    fn parsed(words: &[&str]) -> Result<ServeConfig, String> {
        ServeConfig::from_args(&args(words), || Ok(41000), tailnet)
    }

    #[test]
    fn defaults_are_loopback_on_the_stable_port() {
        let cfg = parsed(&[]).unwrap();
        assert_eq!(cfg.listen, Listen::Loopback);
        assert_eq!(cfg.port, 41000);
        assert!(cfg.origins.is_empty());
        assert_eq!(parsed(&["--port", "7341"]).unwrap().port, 7341);
    }

    #[test]
    fn wildcard_listen_is_refused() {
        for wildcard in ["0.0.0.0", "::", "0:0:0:0:0:0:0:0"] {
            let err = parsed(&["--listen", wildcard]).unwrap_err();
            assert!(err.contains("wildcard"), "{wildcard}: {err}");
        }
        assert!(parsed(&["--listen", "224.0.0.1"]).is_err());
        assert!(parsed(&["--listen", "192.0.2.7"])
            .unwrap_err()
            .contains("not an address of this machine"));
    }

    #[test]
    fn a_named_address_carries_its_interface() {
        assert_eq!(
            parsed(&["--listen", "198.51.100.2"]).unwrap().listen,
            Listen::Address {
                addr: "198.51.100.2".parse().unwrap(),
                iface: "wg0".to_owned()
            }
        );
        assert_eq!(parsed(&["--listen", "127.0.0.1"]).unwrap().listen, Listen::Loopback);
        assert_eq!(parsed(&["--listen", "socket"]).unwrap().listen, Listen::Socket);
    }

    #[test]
    fn bad_arguments_are_refused() {
        for words in [
            &["--port"][..],
            &["--port", "0"],
            &["--port", "70000"],
            &["--port", "+80"],
            &["--listen", "everywhere"],
            &["--listen", "socket", "--listen", "loopback"],
            &["--bind", "x"],
        ] {
            assert!(parsed(words).is_err(), "{words:?}");
        }
    }

    #[test]
    fn origin_entries_are_normalised_and_checked() {
        assert_eq!(parse_origin("Desk.Tail1.ts.net").unwrap(), "desk.tail1.ts.net");
        assert_eq!(
            parse_origin("HTTPS://desk.example:8443").unwrap(),
            "https://desk.example:8443"
        );
        assert_eq!(parse_origin("[::1]:9").unwrap(), "[::1]:9");
        for bad in [
            "",
            "https://desk.example/eitri",
            "desk.example/",
            "desk.example?x",
            "user@desk",
            "ftp://desk.example",
            "*.example",
            "desk..example",
            "desk.example:0",
            "desk.example:",
            "desk example",
        ] {
            assert!(parse_origin(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn to_args_reads_back_the_same() {
        let cfg = parsed(&["--listen", "198.51.100.2", "--origin", "desk.tail1.ts.net"]).unwrap();
        assert_eq!(ServeConfig::from_args(&cfg.to_args(), || Ok(1), tailnet).unwrap(), cfg);
    }

    #[test]
    fn differences_name_what_runs_and_what_was_asked() {
        let running = parsed(&["--origin", "a.example", "--origin", "b.example"]).unwrap();
        let reordered = parsed(&["--origin", "b.example", "--origin", "a.example"]).unwrap();
        assert!(running.differences(&reordered).is_empty());
        let asked = parsed(&["--listen", "198.51.100.2", "--port", "7341"]).unwrap();
        assert_eq!(
            running.differences(&asked),
            vec![
                "listen: running loopback, asked 198.51.100.2 on wg0".to_owned(),
                "port: running 41000, asked 7341".to_owned(),
                "origins: running a.example b.example, asked none".to_owned(),
            ]
        );
    }
}
