//! What `eitri remote serve` listens on and answers to: given on its command line by the window that starts it, and
//! recorded beside its lock so a window that wants something else can say what differs.
//!
//! `serve [--listen loopback|socket|<address>] [--port <port>] [--origin <name>]... [--audit-keep-days <days>]
//! [--no-notify]`
//!
//! - `--listen`: `loopback` (the default: `127.0.0.1` and `::1`), `socket` (a Unix socket in the runtime directory), or
//!   exactly one address of this machine. A wildcard (`0.0.0.0`, `::`) is refused: it would take every network the
//!   machine is on, and the person at the desk confirmed one. A loopback address given by name is `loopback`.
//! - `--port`: the TCP port; without it, the port chosen once for this user ([`super::port::stable_port`]).
//! - `--origin`: a public name a proxy or Tailscale Serve reaches the server by, as a bare host (`desk.tail1.ts.net`,
//!   with `:<port>` when it is not the scheme's default) or an origin (`https://desk.tail1.ts.net`). A path, a query,
//!   user information or a wildcard is refused: the server answers on one path only, at the root of its origin.
//! - `--audit-keep-days`: how many days of rotated audit files to keep, 1 to 3650; without it, 90 (`remote.audit.keep_days`).
//! - `--no-notify`: fork no notifier helper and post no wake (`remote.notify = false`).
//!
//! The record names the build that wrote it ([`this_build`]): a window or `eitri remote status` from another build
//! (after a package upgrade, an old window meets a new `serve` or the other way round) reads only its pid and says the
//! windows need restarting, rather than reading a record whose shape may have changed.

use std::collections::BTreeSet;
use std::net::IpAddr;

use eitri_core::remote_audit::AUDIT_KEEP_DAYS_DEFAULT;
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
    /// Days of rotated audit files kept.
    pub audit_keep_days: u32,
    /// Whether phones' push endpoints are woken.
    pub notify: bool,
}

/// Whether `args` leave the notifier on: read before anything else, since the helper is forked before the rest of
/// the arguments are checked. A command line that is then refused ends the helper with `serve`.
pub fn wants_notify(args: &[String]) -> bool {
    !args.iter().any(|arg| arg == "--no-notify")
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
        let mut keep_days = None;
        let mut notify = true;
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
                "--audit-keep-days" if keep_days.is_none() => keep_days = Some(parse_keep_days(&value(arg)?)?),
                "--no-notify" if notify => notify = false,
                "--listen" | "--port" | "--audit-keep-days" | "--no-notify" => {
                    return Err(format!("{arg} is given twice"))
                }
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
            audit_keep_days: keep_days.unwrap_or(AUDIT_KEEP_DAYS_DEFAULT),
            notify,
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
        // Left out on the default, so the record reads the same in a build from before the flag.
        if self.audit_keep_days != AUDIT_KEEP_DAYS_DEFAULT {
            args.push("--audit-keep-days".to_owned());
            args.push(self.audit_keep_days.to_string());
        }
        if !self.notify {
            args.push("--no-notify".to_owned());
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
        if self.audit_keep_days != other.audit_keep_days {
            out.push(format!(
                "audit keep days: running {}, asked {}",
                self.audit_keep_days, other.audit_keep_days
            ));
        }
        if self.notify != other.notify {
            let word = |on: bool| if on { "on" } else { "off" };
            out.push(format!(
                "notify: running {}, asked {}",
                word(self.notify),
                word(other.notify)
            ));
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
    /// Why the sandbox is partial; older records carry only the word.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_why: Option<String>,
    /// The build that wrote it ([`this_build`]); a build from before the field wrote none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
}

impl Record {
    /// The sandbox word and, when recorded, the reason for incomplete confinement.
    pub fn sandbox_text(&self) -> String {
        match &self.sandbox_why {
            Some(why) if self.sandbox != "full" => format!("{} ({why})", self.sandbox),
            _ => self.sandbox.clone(),
        }
    }
}

/// The version of [`Record`] this build writes and reads.
pub const RECORD_V: u32 = 1;

const fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
        i += 1;
    }
    hash
}

/// A hash of what `serve` and a window must agree on: the `rv-` grammar, the socket's name and the instance grammar in
/// it, the phone's grammar, and this record. Two builds of the same version that differ there differ here.
const SPOKEN_HASH: u64 = fnv1a(
    fnv1a(
        fnv1a(
            fnv1a(0xcbf2_9ce4_8422_2325, include_bytes!("../../../core/src/remote_rv.rs")),
            include_bytes!("../../../core/src/control_listener.rs"),
        ),
        include_bytes!("../../../core/src/remote_wire.rs"),
    ),
    include_bytes!("config.rs"),
);

/// This build, as its record names it: the version and [`SPOKEN_HASH`].
pub fn this_build() -> String {
    format!("{}-{SPOKEN_HASH:016x}", env!("CARGO_PKG_VERSION"))
}

fn parse_keep_days(value: &str) -> Result<u32, String> {
    use eitri_core::lua::config::{REMOTE_AUDIT_KEEP_DAYS_MAX as MAX, REMOTE_AUDIT_KEEP_DAYS_MIN as MIN};
    match value.parse::<u32>() {
        Ok(days) if value.bytes().all(|b| b.is_ascii_digit()) && (MIN..=MAX).contains(&days) => Ok(days),
        _ => Err(format!(
            "--audit-keep-days takes a number of days from {MIN} to {MAX}, not {value:?}"
        )),
    }
}

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
    fn sandbox_records_read_old_words_and_preserve_new_reasons() {
        let old = r#"{"v":1,"pid":1,"args":[],"sandbox":"partial"}"#;
        let mut record: Record = serde_json::from_str(old).unwrap();
        assert_eq!(record.sandbox_why, None);
        assert_eq!(record.sandbox_text(), "partial");
        record.sandbox_why = Some("the kernel has no Landlock".into());
        let record: Record = serde_json::from_slice(&serde_json::to_vec(&record).unwrap()).unwrap();
        assert_eq!(record.sandbox_text(), "partial (the kernel has no Landlock)");
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

    #[test]
    fn no_notify_is_read_back_and_named() {
        let running = parsed(&[]).unwrap();
        assert!(running.notify);
        assert!(!running.to_args().contains(&"--no-notify".to_owned()));
        let asked = parsed(&["--no-notify"]).unwrap();
        assert!(!asked.notify);
        assert_eq!(
            ServeConfig::from_args(&asked.to_args(), || Ok(1), tailnet).unwrap(),
            asked
        );
        assert_eq!(running.differences(&asked), ["notify: running on, asked off"]);
        assert!(parsed(&["--no-notify", "--no-notify"]).is_err());
        assert!(wants_notify(&[]));
        assert!(!wants_notify(&[
            "--listen".to_owned(),
            "socket".to_owned(),
            "--no-notify".to_owned()
        ]));
    }

    #[test]
    fn differences_names_keep_days() {
        let running = parsed(&[]).unwrap();
        assert_eq!(
            running.audit_keep_days,
            eitri_core::remote_audit::AUDIT_KEEP_DAYS_DEFAULT
        );
        let asked = parsed(&["--audit-keep-days", "30"]).unwrap();
        assert_eq!(asked.audit_keep_days, 30);
        assert_eq!(running.differences(&asked), ["audit keep days: running 90, asked 30"]);
        assert_eq!(
            ServeConfig::from_args(&asked.to_args(), || Ok(1), tailnet).unwrap(),
            asked,
            "the record reads back the same"
        );
        assert!(
            !running.to_args().iter().any(|a| a == "--audit-keep-days"),
            "the default is not written, so a record reads the same in a build that predates the key"
        );
    }

    #[test]
    fn keep_days_is_1_to_3650() {
        assert_eq!(parsed(&["--audit-keep-days", "1"]).unwrap().audit_keep_days, 1);
        assert_eq!(parsed(&["--audit-keep-days", "3650"]).unwrap().audit_keep_days, 3650);
        for bad in ["0", "3651", "-3", "7d", "", "+7"] {
            let err = parsed(&["--audit-keep-days", bad]).unwrap_err();
            assert!(err.contains("--audit-keep-days"), "{bad:?}: {err}");
        }
        assert!(parsed(&["--audit-keep-days", "7", "--audit-keep-days", "8"]).is_err());
    }

    #[test]
    fn this_build_names_the_version() {
        let build = this_build();
        assert!(build.starts_with(env!("CARGO_PKG_VERSION")), "{build}");
        assert_eq!(build, this_build(), "a build names itself the same every time");
    }
}
