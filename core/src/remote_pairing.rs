//! Pairing a phone: the one-time enrollment code, the pairing URI its QR code carries, the desk's SSH host-key
//! fingerprints the phone pins, what kind of link an address to listen on sits on, and the listen address the desk
//! said yes to.
//!
//! An enrollment code is 128 random bits, taken at most once and only within [`ENROLL_CODE_TTL`] of being made: a
//! screen recording or a scanner's history that kept it holds nothing once it was used or ran out. It is not the
//! device's token; that is minted only after the desk's `y`, and only its hash is stored.
//!
//! The pairing URI is
//! `eitri-pair:v=1&c=<code>&i=<instance>&m=<ssh|direct>&s=<https|http>&h=<host>&p=<port>&k=<fingerprint>...`, with
//! one `k` per SSH host key of the desk, so a phone using the SSH recipe refuses any other host key on the connection
//! that carries its token. `m` is the recipe the phone uses and also says in its `enroll`; `s` is what it speaks to
//! `h:p`: `https` to a proxy or Tailscale Serve name, `http` to `serve` itself (a tunnel's address, a confirmed LAN
//! address, or the far end of the SSH forward), which has no TLS of its own. Every value is percent-encoded (a
//! fingerprint's base64 has `+` and `/`).
//!
//! A named listen address takes effect only after the desk's yes ([`record_listen`], read back by
//! [`confirmed_listen`]): a link that is not a known tunnel carries the traffic in plain text, so the desk is told so
//! and asked. The record is a private file beside the device store; like the store, a process of this user can
//! write it.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{self, Read};
use std::net::IpAddr;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long an enrollment code may be presented after it was made.
pub const ENROLL_CODE_TTL: Duration = Duration::from_secs(180);

/// The version of the pairing URI this build writes.
pub const PAIRING_URI_V: u32 = 1;

/// The file beside the device store that records the listen address the desk said yes to.
const LISTEN_FILE: &str = "listen.json";

/// The device store's file in [`remote_state_dir`].
pub const DEVICES_FILE: &str = "devices.json";

/// The largest listen record read.
const LISTEN_MAX_BYTES: u64 = 4096;

/// The listen record's format version.
const LISTEN_FORMAT_V: u32 = 1;

/// The largest SSH public key file read.
const PUBLIC_KEY_MAX_BYTES: u64 = 64 * 1024;

/// The most host keys a pairing URI carries; a desk has one per algorithm.
const HOST_KEYS_MAX: usize = 8;

/// `<state home>/eitri/remote`, where the device store, the listen record and the port `serve` keeps live: from an
/// absolute `XDG_STATE_HOME`, else the absolute home's `.local/state`. `None` when neither gives one.
pub fn remote_state_dir(xdg_state_home: Option<&std::ffi::OsStr>, home: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    crate::state_paths::state_subdir(xdg_state_home, home, "remote")
}

/// Fills `buf` from the kernel's random source: `getrandom(2)` on Linux, retried on `EINTR`, and `/dev/urandom`
/// elsewhere (macOS has no `getrandom`). A short read is an error, so no credential is ever partly zero.
pub fn random_fill(buf: &mut [u8]) -> io::Result<()> {
    #[cfg(not(target_os = "linux"))]
    {
        std::fs::File::open("/dev/urandom")?.read_exact(buf)
    }
    #[cfg(target_os = "linux")]
    {
        random_fill_linux(buf)
    }
}

#[cfg(target_os = "linux")]
fn random_fill_linux(buf: &mut [u8]) -> io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        let rest = &mut buf[filled..];
        // SAFETY: `rest` is a live, writable buffer of `rest.len()` bytes.
        let got = unsafe { libc::getrandom(rest.as_mut_ptr().cast(), rest.len(), 0) };
        if got < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if got == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "getrandom gave no bytes"));
        }
        filled += got as usize;
    }
    Ok(())
}

/// `bytes` as unpadded base64url, the form a code and a token travel in.
pub fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A one-time enrollment code, made by the window that shows it.
#[derive(Clone)]
pub struct EnrollCode {
    pub code: [u8; 16],
    /// The instance whose window made it, so `serve` hands the enrollment to that window.
    pub instance: String,
    /// The moment it may no longer be presented.
    pub expires: Instant,
}

impl std::fmt::Debug for EnrollCode {
    /// Never the code.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrollCode")
            .field("instance", &self.instance)
            .field("expires", &self.expires)
            .finish_non_exhaustive()
    }
}

impl EnrollCode {
    /// A fresh code for `instance`, valid for [`ENROLL_CODE_TTL`] from `now`.
    pub fn new(instance: &str, now: Instant) -> io::Result<EnrollCode> {
        let mut code = [0u8; 16];
        random_fill(&mut code)?;
        Ok(EnrollCode {
            code,
            instance: instance.to_owned(),
            expires: now + ENROLL_CODE_TTL,
        })
    }

    /// The code as the phone presents it: unpadded base64url.
    pub fn text(&self) -> String {
        base64url(&self.code)
    }
}

/// Why a presented code was not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeRefused {
    /// No code of this window has that value (never made, already taken, or replaced by a newer one).
    Unknown,
    /// The code was right and had run out; it is spent too.
    Expired,
}

impl CodeRefused {
    /// What the phone is told.
    pub fn why(self) -> &'static str {
        match self {
            CodeRefused::Unknown => "that enrollment code is not this desk's, or was already used",
            CodeRefused::Expired => "that enrollment code ran out; show a new one at the desk",
        }
    }
}

/// The code a window shows now. Making a new one replaces the last, so only the QR code on screen counts.
#[derive(Debug, Default)]
pub struct EnrollCodes {
    current: Option<EnrollCode>,
}

impl EnrollCodes {
    /// `code` is the one shown from now on.
    pub fn issue(&mut self, code: EnrollCode) {
        self.current = Some(code);
    }

    /// Takes the code `presented` names, at `now`. A code is taken once, whatever the outcome: a right code past its
    /// time is spent as well. Compared without stopping at the first differing byte.
    pub fn take(&mut self, presented: &str, now: Instant) -> Result<EnrollCode, CodeRefused> {
        let Some(current) = self.current.as_ref() else {
            return Err(CodeRefused::Unknown);
        };
        let wanted: [u8; 32] = Sha256::digest(current.text().as_bytes()).into();
        let given: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        let mut diff = 0u8;
        for (a, b) in wanted.iter().zip(given.iter()) {
            diff |= std::hint::black_box(a ^ b);
        }
        if std::hint::black_box(diff) != 0 {
            return Err(CodeRefused::Unknown);
        }
        let code = self.current.take().ok_or(CodeRefused::Unknown)?;
        if now >= code.expires {
            return Err(CodeRefused::Expired);
        }
        Ok(code)
    }

    /// Whether a code is shown and still in time at `now`.
    pub fn showing(&self, now: Instant) -> bool {
        self.current.as_ref().is_some_and(|c| now < c.expires)
    }
}

/// How the phone reaches `serve`, as the pairing URI says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairMode {
    /// Through the app's SSH client to the desk's loopback listener.
    Ssh,
    /// To the listener's address directly (a tunnel's address, a proxy's name, a confirmed LAN address).
    Direct,
}

impl PairMode {
    /// The word the pairing URI's `m` and the phone's `enroll` carry.
    pub fn as_str(self) -> &'static str {
        match self {
            PairMode::Ssh => "ssh",
            PairMode::Direct => "direct",
        }
    }
}

/// What the phone speaks to the pairing URI's `h:p`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairScheme {
    /// TLS, ended by the proxy or Tailscale Serve in front of `serve`.
    Https,
    /// Plain HTTP and WebSocket: `serve` itself, reached over a tunnel, a confirmed address or an SSH forward.
    Http,
}

impl PairScheme {
    /// The word the pairing URI's `s` carries.
    pub fn as_str(self) -> &'static str {
        match self {
            PairScheme::Https => "https",
            PairScheme::Http => "http",
        }
    }
}

/// Where the phone connects: the recipe, what it speaks, the host it names and the port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub mode: PairMode,
    pub scheme: PairScheme,
    pub host: String,
    /// 0 when `host` is the path of `serve`'s Unix socket, which an SSH forward reaches by its path.
    pub port: u16,
}

impl Endpoint {
    /// What a phone paired with this endpoint dials, as the desk is told it: "directly to <host>:<port> (https)",
    /// "through SSH to <host>:<port> on this desk" or "through SSH to the socket <path> on this desk". An IPv6 host is
    /// put in brackets.
    pub fn describe(&self) -> String {
        let host_port = || {
            if self.host.contains(':') {
                format!("[{}]:{}", self.host, self.port)
            } else {
                format!("{}:{}", self.host, self.port)
            }
        };
        match (self.mode, self.port) {
            (PairMode::Direct, _) => format!("directly to {} ({})", host_port(), self.scheme.as_str()),
            (PairMode::Ssh, 0) => format!("through SSH to the socket {} on this desk", self.host),
            (PairMode::Ssh, _) => format!("through SSH to {} on this desk", host_port()),
        }
    }
}

/// The ways a window can send phones to `serve`: the SSH recipe and the direct one, where each exists. A plain `eitri
/// remote add-device` takes the direct one when there is one (a proxy, Tailscale Serve or a named address is what the
/// person set up for phones) and the SSH one otherwise; `--ssh` and `--direct` ask for one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingRoutes {
    pub ssh: Option<Endpoint>,
    pub direct: Option<Endpoint>,
}

/// What an enrollment asking for the SSH recipe is told when there is none.
pub const NO_SSH_ROUTE: &str = "this window has no SSH route for phones: serve listens on the address remote.listen \
                                names; the SSH recipe needs remote.listen = \"loopback\" or \"socket\"";

/// What an enrollment asking for the direct recipe is told when there is none.
pub const NO_DIRECT_ROUTE: &str = "this window has no direct address for phones: name the proxy or Tailscale Serve in \
                                   front of serve in remote.origins, or an address in remote.listen";

impl PairingRoutes {
    /// The endpoint for `asked`, or with `None` the default one; `Err` says why there is none.
    pub fn pick(&self, asked: Option<PairMode>) -> Result<&Endpoint, &'static str> {
        match asked {
            Some(PairMode::Ssh) => self.ssh.as_ref().ok_or(NO_SSH_ROUTE),
            Some(PairMode::Direct) => self.direct.as_ref().ok_or(NO_DIRECT_ROUTE),
            None => self.direct.as_ref().or(self.ssh.as_ref()).ok_or(NO_DIRECT_ROUTE),
        }
    }
}

/// One SSH host key's fingerprint in OpenSSH's form, `SHA256:<unpadded base64 of the key blob's SHA-256>`, and the
/// key's algorithm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshKeyFingerprint {
    pub algorithm: String,
    pub sha256: String,
}

/// The pairing URI for `code`, reaching `endpoint`, pinning `ssh_host_keys`.
pub fn pairing_uri(code: &EnrollCode, endpoint: &Endpoint, ssh_host_keys: &[SshKeyFingerprint]) -> String {
    let mut uri = format!(
        "eitri-pair:v={PAIRING_URI_V}&c={}&i={}&m={}&s={}&h={}&p={}",
        percent(&code.text()),
        percent(&code.instance),
        endpoint.mode.as_str(),
        endpoint.scheme.as_str(),
        percent(&endpoint.host),
        endpoint.port
    );
    for key in ssh_host_keys.iter().take(HOST_KEYS_MAX) {
        uri.push_str("&k=");
        uri.push_str(&percent(&key.sha256));
    }
    uri
}

/// `text` with every byte outside RFC 3986's unreserved set percent-encoded.
fn percent(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The fingerprint of one OpenSSH public key line (`<algorithm> <base64 blob> [comment]`), or `None` for anything
/// else: a private key, an options prefix, a blob that is not base64, does not name its own algorithm or does not
/// carry that algorithm's key.
pub fn public_key_fingerprint(line: &str) -> Option<SshKeyFingerprint> {
    let mut words = line.split_ascii_whitespace();
    let algorithm = words.next()?;
    let blob = base64::engine::general_purpose::STANDARD.decode(words.next()?).ok()?;
    let fields = ssh_strings(&blob)?;
    if fields.first() != Some(&algorithm.as_bytes()) || !carries_key(algorithm, &fields[1..]) {
        return None;
    }
    let digest = Sha256::digest(&blob);
    Some(SshKeyFingerprint {
        algorithm: algorithm.to_owned(),
        sha256: format!(
            "SHA256:{}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest)
        ),
    })
}

/// A key blob as SSH strings (each a 4-byte big-endian length, then that many bytes); `None` unless they fill it
/// exactly.
fn ssh_strings(blob: &[u8]) -> Option<Vec<&[u8]>> {
    let mut fields = Vec::new();
    let mut rest = blob;
    while !rest.is_empty() {
        let len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let end = 4usize.checked_add(len)?;
        fields.push(rest.get(4..end)?);
        rest = &rest[end..];
    }
    Some(fields)
}

/// Whether `fields`, the blob after its algorithm's name, are that algorithm's public key: Ed25519's 32 bytes, an
/// ECDSA curve's name and uncompressed point, RSA's exponent and modulus. A kind not known here (a security key's,
/// say) needs at least one field, none of them empty.
fn carries_key(algorithm: &str, fields: &[&[u8]]) -> bool {
    let point =
        |curve: &[u8], q: &[u8], name: &str, len: usize| curve == name.as_bytes() && q.len() == len && q[0] == 4;
    match (algorithm, fields) {
        ("ssh-ed25519", [key]) => key.len() == 32,
        ("ssh-rsa", [e, n]) => !e.is_empty() && !n.is_empty(),
        ("ecdsa-sha2-nistp256", [curve, q]) => point(curve, q, "nistp256", 65),
        ("ecdsa-sha2-nistp384", [curve, q]) => point(curve, q, "nistp384", 97),
        ("ecdsa-sha2-nistp521", [curve, q]) => point(curve, q, "nistp521", 133),
        ("ssh-ed25519" | "ssh-rsa" | "ecdsa-sha2-nistp256" | "ecdsa-sha2-nistp384" | "ecdsa-sha2-nistp521", _) => false,
        (_, fields) => !fields.is_empty() && fields.iter().all(|f| !f.is_empty()),
    }
}

/// The fingerprints of the host keys in `dir` (`/etc/ssh` on a desk): every `ssh_host_*_key.pub` that reads as one
/// public key, in file-name order. A file that cannot be read or is not a public key is left out.
pub fn ssh_host_key_fingerprints(dir: &Path) -> Vec<SshKeyFingerprint> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("ssh_host_") && n.ends_with("_key.pub"))
        })
        .collect();
    names.sort();
    names
        .iter()
        .filter_map(|path| {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(path)
                .ok()?;
            if !file.metadata().ok()?.is_file() {
                return None;
            }
            let mut text = String::new();
            file.take(PUBLIC_KEY_MAX_BYTES).read_to_string(&mut text).ok()?;
            public_key_fingerprint(text.lines().next()?)
        })
        .take(HOST_KEYS_MAX)
        .collect()
}

/// What kind of link an interface is, read from the link itself and never from its name alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IfaceKind {
    /// A WireGuard link (the kernel's `wireguard` device type).
    WireGuard,
    /// Tailscale's TUN device.
    Tailscale,
    /// Another TUN device (a VPN client's).
    Tun,
    /// Anything else: traffic on it is as private as the network it is on.
    Plain,
}

impl IfaceKind {
    /// Whether traffic on such a link is carried by a tunnel rather than in plain text.
    pub fn is_tunnel(self) -> bool {
        !matches!(self, IfaceKind::Plain)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            IfaceKind::WireGuard => "wireguard",
            IfaceKind::Tailscale => "tailscale",
            IfaceKind::Tun => "tun",
            IfaceKind::Plain => "plain",
        }
    }
}

/// The kind of the interface `iface`, from `/sys/class/net`.
pub fn interface_kind(iface: &str) -> IfaceKind {
    interface_kind_in(Path::new("/sys/class/net"), iface)
}

/// [`interface_kind`] read under `sys_class_net`. A link whose `uevent` says `DEVTYPE=wireguard` is WireGuard; one
/// with `tun_flags` is a TUN device, Tailscale's when it is named `tailscale0`; anything else, an unknown or
/// unreadable link included, is plain. A name that is not one path component is plain.
pub fn interface_kind_in(sys_class_net: &Path, iface: &str) -> IfaceKind {
    if iface.is_empty() || iface == "." || iface == ".." || iface.contains('/') {
        return IfaceKind::Plain;
    }
    let link = sys_class_net.join(iface);
    let uevent = std::fs::File::open(link.join("uevent")).ok().and_then(|f| {
        let mut text = String::new();
        f.take(PUBLIC_KEY_MAX_BYTES).read_to_string(&mut text).ok()?;
        Some(text)
    });
    if uevent
        .as_deref()
        .is_some_and(|text| text.lines().any(|l| l.trim() == "DEVTYPE=wireguard"))
    {
        return IfaceKind::WireGuard;
    }
    if link.join("tun_flags").is_file() {
        return if iface == "tailscale0" {
            IfaceKind::Tailscale
        } else {
            IfaceKind::Tun
        };
    }
    IfaceKind::Plain
}

/// The interface that holds `addr`, from `getifaddrs(3)`; `None` when no interface has it.
pub fn interface_of(addr: IpAddr) -> Option<String> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `getifaddrs` writes a list head into `list`, freed below with `freeifaddrs`.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return None;
    }
    let mut found = None;
    let mut at = list;
    while !at.is_null() {
        // SAFETY: `at` is a node of the list `getifaddrs` returned, alive until `freeifaddrs`.
        let node = unsafe { &*at };
        at = node.ifa_next;
        if node.ifa_addr.is_null() || node.ifa_name.is_null() {
            continue;
        }
        // SAFETY: `ifa_addr` points at a socket address whose family says its layout.
        let here = unsafe {
            match i32::from((*node.ifa_addr).sa_family) {
                libc::AF_INET => {
                    let sin = &*(node.ifa_addr as *const libc::sockaddr_in);
                    Some(IpAddr::from(u32::from_be(sin.sin_addr.s_addr).to_be_bytes()))
                }
                libc::AF_INET6 => {
                    let sin6 = &*(node.ifa_addr as *const libc::sockaddr_in6);
                    Some(IpAddr::from(sin6.sin6_addr.s6_addr))
                }
                _ => None,
            }
        };
        if here == Some(addr) {
            // SAFETY: `ifa_name` is a NUL-terminated name owned by the list.
            let name = unsafe { std::ffi::CStr::from_ptr(node.ifa_name) };
            found = name.to_str().ok().map(str::to_owned);
            break;
        }
    }
    // SAFETY: `list` came from `getifaddrs` and is freed once.
    unsafe { libc::freeifaddrs(list) };
    found
}

/// The address `remote.listen` names, read as an IP address with no port (`198.51.100.2`, `fd00::2`, `[fd00::2]`).
/// Wildcards and loopback are not named addresses.
pub fn named_listen_addr(text: &str) -> Option<IpAddr> {
    let bare = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')).unwrap_or(text);
    let addr: IpAddr = bare.parse::<IpAddr>().ok()?.to_canonical();
    if addr.is_unspecified() || addr.is_loopback() {
        return None;
    }
    Some(addr)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListenRecord {
    v: u32,
    addr: String,
    kind: IfaceKind,
    at: u64,
}

/// The listen address the desk said yes to, and the kind of link it was on then, recorded in `state_dir` (the
/// device store's directory). `None` when nothing was recorded.
pub fn confirmed_listen(state_dir: &Path) -> io::Result<Option<String>> {
    Ok(confirmed_listen_with_kind(state_dir)?.map(|(addr, _)| addr))
}

/// [`confirmed_listen`] with the kind of link recorded with it. A record that is not a private regular file of this
/// user, or does not parse, is refused (`PermissionDenied`, `InvalidData`) rather than read as no yes.
pub fn confirmed_listen_with_kind(state_dir: &Path) -> io::Result<Option<(String, IfaceKind)>> {
    let path = state_dir.join(LISTEN_FILE);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != agent::private_fs::euid() || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a private file of this user", path.display()),
        ));
    }
    let mut bytes = Vec::new();
    file.take(LISTEN_MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LISTEN_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the listen record is too long",
        ));
    }
    let record: ListenRecord =
        serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if record.v != LISTEN_FORMAT_V {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("listen record version {} is not {LISTEN_FORMAT_V}", record.v),
        ));
    }
    Ok(Some((record.addr, record.kind)))
}

/// The address the desk said yes to, while the link that holds it now is of the kind it was on when the desk was asked:
/// a yes given for a tunnel does not cover the same address on a plain link. `None` when nothing was recorded, the
/// address is on no link of this machine, or the link's kind changed.
pub fn confirmed_listen_now(state_dir: &Path) -> io::Result<Option<String>> {
    confirmed_listen_on(state_dir, |addr| interface_of(addr).map(|iface| interface_kind(&iface)))
}

/// [`confirmed_listen_now`] with `kind_of` saying what kind of link holds an address now (`None`: none does).
pub fn confirmed_listen_on(
    state_dir: &Path,
    kind_of: impl Fn(IpAddr) -> Option<IfaceKind>,
) -> io::Result<Option<String>> {
    let Some((addr, kind)) = confirmed_listen_with_kind(state_dir)? else {
        return Ok(None);
    };
    let Some(ip) = named_listen_addr(&addr) else {
        return Ok(None);
    };
    Ok((kind_of(ip) == Some(kind)).then_some(addr))
}

/// Records the desk's yes to listening on `addr`, a link of `kind`, in `state_dir` (made 0700 if missing): a 0600
/// file written beside its place and renamed into it.
pub fn record_listen(state_dir: &Path, addr: &str, kind: IfaceKind, at: u64) -> io::Result<()> {
    agent::private_fs::create_private_dir_all(state_dir, state_dir.parent().unwrap_or(state_dir))?;
    let path = state_dir.join(LISTEN_FILE);
    let body = serde_json::to_vec(&ListenRecord {
        v: LISTEN_FORMAT_V,
        addr: addr.to_owned(),
        kind,
        at,
    })
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = crate::state_paths::temporary(&path);
    agent::private_fs::replace_private(&tmp, &path, &body, agent::private_fs::Flush::File)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Made with `ssh-keygen -t <kind>`; the fingerprints are what `ssh-keygen -lf` printed for them.
    const ED25519: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHsW2+maVvnEFmizxiNHThNbEgyl9S+3nG+QUAh2rC9R fixture-ed25519";
    const ED25519_FP: &str = "SHA256:Ump3fnzhgbBMN+6ZSW3PXL/me3tqMS6N4REE8QbyW0g";
    const ECDSA: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBIKHjx4LQJhnOei/m0u9w0ZnY6CD8by6wKufYum5v7HMmowgmWN7aRI056uis6Y1IHih6tZh2GMD4cjU5EdKX5Q= fixture-ecdsa";
    const ECDSA_FP: &str = "SHA256:eCKj/KlQAxaOvpu1RLl5kRIbU3FzONC49aj0RtBPNZI";
    const RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQCrKHvrsxEdnG55fWQQIMoVFP3+sTx0d6UA06pBu78m+djHaDCvn9SQWqE2niwEO9hH6ZYlKgBJOTFQm2NTnz70TFAppvvAf0EIPKYLwi+tARs8sg8I55fGb9ASt1CBqhmAvHQu4V1mvQHQI2YvA+rfC05SxS+MNbNvnNGM/DicSi/Unr129MY3XvAHarmZvPD4pD5pxumO1+RsWe5WBoh2JBgEN8MsNiZ/7vCKY5qz5WrwuFbBDesLUN/Gi7FZK2BBH0KTUk7cPeHxgYhr4COcRr6LzYRtAIWJlS6Bnv1efbTvTkZHriOYJAvz3C65kdaconQAwz7WyBZRVlEldHjV3tIYDoYaP5ukFfUuRJtSwiEkd6HO805G852s7AhE4IXIeOJKiuxtGQmFT1cwQYnOfLUH9Ke+7UHVIDy8yiOaAwGEOsOQEMAVu/rWOkFRgPHonoktY6eNBOerUiNyHt6wv2P6IY5evHKRVDVzFx8DfWvjim1XwKbsD+50jbb8dMU= fixture-rsa";
    const RSA_FP: &str = "SHA256:BeZ9Y/ywElYnNyCDB5IdqSRj4x8UvdhCd5APXVzFeeo";

    fn code(instance: &str, now: Instant) -> EnrollCode {
        EnrollCode::new(instance, now).expect("getrandom works")
    }

    #[test]
    fn code_is_single_use() {
        let now = Instant::now();
        let mut codes = EnrollCodes::default();
        let shown = code("b4e1c09a7d3f5e21", now);
        let text = shown.text();
        assert_eq!(text.len(), 22, "16 bytes of unpadded base64url");
        codes.issue(shown);
        assert_eq!(
            codes.take("AAAAAAAAAAAAAAAAAAAAAA", now).unwrap_err(),
            CodeRefused::Unknown
        );
        let taken = codes.take(&text, now).expect("the shown code is taken once");
        assert_eq!(taken.instance, "b4e1c09a7d3f5e21");
        assert_eq!(
            codes.take(&text, now).unwrap_err(),
            CodeRefused::Unknown,
            "and never again"
        );

        // A newer code replaces the one shown before it.
        let first = code("i", now);
        let first_text = first.text();
        codes.issue(first);
        let second = code("i", now);
        let second_text = second.text();
        codes.issue(second);
        assert_eq!(codes.take(&first_text, now).unwrap_err(), CodeRefused::Unknown);
        assert!(codes.take(&second_text, now).is_ok());
        assert_ne!(first_text, second_text, "each code is random");
    }

    #[test]
    fn code_expires_after_three_minutes() {
        let start = Instant::now();
        let mut codes = EnrollCodes::default();
        let shown = code("i", start);
        let text = shown.text();
        codes.issue(shown);
        let almost = start + ENROLL_CODE_TTL - Duration::from_millis(1);
        assert!(codes.showing(almost));
        assert!(!codes.showing(start + ENROLL_CODE_TTL));
        assert_eq!(ENROLL_CODE_TTL, Duration::from_secs(180));
        assert_eq!(
            codes.take(&text, start + ENROLL_CODE_TTL).unwrap_err(),
            CodeRefused::Expired
        );
        assert_eq!(
            codes.take(&text, start).unwrap_err(),
            CodeRefused::Unknown,
            "an expired code is spent too"
        );

        let shown = code("i", start);
        let text = shown.text();
        codes.issue(shown);
        assert!(
            codes.take(&text, almost).is_ok(),
            "in time a millisecond before the end"
        );
    }

    #[test]
    fn uri_carries_every_ssh_host_key() {
        let dir = agent::state_dirs::test_workspace_dir("pairing-host-keys");
        for (name, line) in [
            ("ssh_host_ed25519_key.pub", ED25519),
            ("ssh_host_ecdsa_key.pub", ECDSA),
            ("ssh_host_rsa_key.pub", RSA),
        ] {
            std::fs::write(dir.join(name), format!("{line}\n")).unwrap();
        }
        // Neither a private key nor another file is read as a host key.
        std::fs::write(dir.join("ssh_host_ed25519_key"), format!("{ED25519}\n")).unwrap();
        std::fs::write(dir.join("ssh_config"), "Host *\n").unwrap();
        std::fs::write(dir.join("ssh_host_bogus_key.pub"), "ssh-ed25519 not-base64!\n").unwrap();

        let keys = ssh_host_key_fingerprints(&dir);
        let mut found: Vec<(&str, &str)> = keys.iter().map(|k| (k.algorithm.as_str(), k.sha256.as_str())).collect();
        found.sort();
        assert_eq!(
            found,
            vec![
                ("ecdsa-sha2-nistp256", ECDSA_FP),
                ("ssh-ed25519", ED25519_FP),
                ("ssh-rsa", RSA_FP),
            ]
        );

        let shown = EnrollCode {
            code: [7u8; 16],
            instance: "b4e1c09a7d3f5e21".into(),
            expires: Instant::now(),
        };
        let endpoint = Endpoint {
            mode: PairMode::Ssh,
            scheme: PairScheme::Http,
            host: "desk.example.net".into(),
            port: 7341,
        };
        let uri = pairing_uri(&shown, &endpoint, &keys);
        assert!(
            uri.starts_with(
                "eitri-pair:v=1&c=BwcHBwcHBwcHBwcHBwcHBw&i=b4e1c09a7d3f5e21&m=ssh&s=http&h=desk.example.net&p=7341&k="
            ),
            "{uri}"
        );
        for fp in [ED25519_FP, ECDSA_FP, RSA_FP] {
            let encoded = fp.replace(':', "%3A").replace('+', "%2B").replace('/', "%2F");
            assert!(uri.contains(&format!("&k={encoded}")), "{fp} in {uri}");
        }
        assert_eq!(uri.matches("&k=").count(), 3);

        let direct = pairing_uri(
            &shown,
            &Endpoint {
                mode: PairMode::Direct,
                scheme: PairScheme::Https,
                host: "fd00::2".into(),
                port: 7341,
            },
            &[],
        );
        assert!(direct.ends_with("&m=direct&s=https&h=fd00%3A%3A2&p=7341"), "{direct}");
    }

    #[test]
    fn an_endpoint_is_described_as_the_phone_dials_it() {
        let of = |mode, scheme, host: &str, port| {
            Endpoint {
                mode,
                scheme,
                host: host.into(),
                port,
            }
            .describe()
        };
        assert_eq!(
            of(PairMode::Direct, PairScheme::Https, "desk.tail1.ts.net", 443),
            "directly to desk.tail1.ts.net:443 (https)"
        );
        assert_eq!(
            of(PairMode::Direct, PairScheme::Http, "fd00::2", 7341),
            "directly to [fd00::2]:7341 (http)"
        );
        assert_eq!(
            of(PairMode::Ssh, PairScheme::Http, "127.0.0.1", 41000),
            "through SSH to 127.0.0.1:41000 on this desk"
        );
        assert_eq!(
            of(PairMode::Ssh, PairScheme::Http, "/run/user/1000/eitri/serve-socket", 0),
            "through SSH to the socket /run/user/1000/eitri/serve-socket on this desk"
        );
    }

    #[test]
    fn a_public_key_line_must_name_its_own_algorithm() {
        assert_eq!(public_key_fingerprint(ED25519).unwrap().sha256, ED25519_FP);
        let swapped = ED25519.replacen("ssh-ed25519", "ssh-rsa", 1);
        assert_eq!(public_key_fingerprint(&swapped), None);
        assert_eq!(public_key_fingerprint("ssh-ed25519"), None);
        assert_eq!(public_key_fingerprint(""), None);
    }

    /// An OpenSSH key line of `algorithm` whose blob is `fields`, each as an SSH string.
    fn wire_line(algorithm: &str, fields: &[&[u8]]) -> String {
        let mut blob = Vec::new();
        for field in std::iter::once(algorithm.as_bytes()).chain(fields.iter().copied()) {
            blob.extend_from_slice(&(field.len() as u32).to_be_bytes());
            blob.extend_from_slice(field);
        }
        format!("{algorithm} {}", base64::engine::general_purpose::STANDARD.encode(blob))
    }

    #[test]
    fn a_public_key_must_carry_its_key_material() {
        for line in [ED25519, ECDSA, RSA] {
            assert!(public_key_fingerprint(line).is_some(), "{line}");
        }
        let point = [4u8; 65];
        let refused = [
            // The algorithm's name and nothing after it.
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5".to_owned(),
            wire_line("ssh-ed25519", &[]),
            wire_line("ssh-ed25519", &[&[7; 31][..]]),
            wire_line("ssh-ed25519", &[&[7; 32][..], &b"more"[..]]),
            wire_line("ssh-rsa", &[&[1, 0, 1][..]]),
            wire_line("ssh-rsa", &[&[1, 0, 1][..], &[][..]]),
            wire_line("ecdsa-sha2-nistp256", &[&b"nistp384"[..], &point[..]]),
            wire_line("ecdsa-sha2-nistp256", &[&b"nistp256"[..], &point[..33]]),
            wire_line("some-new-kind", &[]),
            wire_line("some-new-kind", &[&[][..]]),
        ];
        for line in &refused {
            assert_eq!(public_key_fingerprint(line), None, "{line}");
        }
        // Bytes after the last field.
        let mut blob = base64::engine::general_purpose::STANDARD
            .decode(ED25519.split_ascii_whitespace().nth(1).unwrap())
            .unwrap();
        blob.push(0);
        let trailing = format!("ssh-ed25519 {}", base64::engine::general_purpose::STANDARD.encode(blob));
        assert_eq!(public_key_fingerprint(&trailing), None);
        // Whole fields that carry something pass, for a kind not known here too.
        assert!(public_key_fingerprint(&wire_line("ecdsa-sha2-nistp256", &[&b"nistp256"[..], &point[..]])).is_some());
        assert!(public_key_fingerprint(&wire_line("some-new-kind", &[&b"material"[..]])).is_some());
    }

    #[test]
    fn interface_kind_reads_the_link_not_its_name() {
        let sys = agent::state_dirs::test_workspace_dir("pairing-sys-class-net");
        let link = |name: &str, uevent: &str, tun: bool| {
            let dir = sys.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("uevent"), uevent).unwrap();
            if tun {
                std::fs::write(dir.join("tun_flags"), "0x1001\n").unwrap();
            }
        };
        link("wg0", "DEVTYPE=wireguard\nINTERFACE=wg0\nIFINDEX=5\n", false);
        link("home", "DEVTYPE=wireguard\nINTERFACE=home\n", false);
        link("tailscale0", "INTERFACE=tailscale0\n", true);
        link("tun0", "INTERFACE=tun0\n", true);
        link("eth0", "INTERFACE=eth0\n", false);
        // Named like a tunnel, and a plain link all the same.
        link("wg1", "INTERFACE=wg1\n", false);
        link("tailscale1", "INTERFACE=tailscale1\n", false);
        assert_eq!(interface_kind_in(&sys, "wg0"), IfaceKind::WireGuard);
        assert_eq!(interface_kind_in(&sys, "home"), IfaceKind::WireGuard);
        assert_eq!(interface_kind_in(&sys, "tailscale0"), IfaceKind::Tailscale);
        assert_eq!(interface_kind_in(&sys, "tun0"), IfaceKind::Tun);
        assert_eq!(interface_kind_in(&sys, "eth0"), IfaceKind::Plain);
        assert_eq!(interface_kind_in(&sys, "wg1"), IfaceKind::Plain);
        assert_eq!(interface_kind_in(&sys, "tailscale1"), IfaceKind::Plain);
        assert_eq!(interface_kind_in(&sys, "missing"), IfaceKind::Plain);
        assert_eq!(interface_kind_in(&sys, "../wg0"), IfaceKind::Plain);
        assert!(!IfaceKind::Plain.is_tunnel() && IfaceKind::Tun.is_tunnel());
    }

    #[test]
    fn interface_of_finds_loopback_and_nothing_for_an_unheld_address() {
        assert!(interface_of("127.0.0.1".parse().unwrap()).is_some());
        assert_eq!(
            interface_of("192.0.2.123".parse().unwrap()),
            None,
            "TEST-NET-1 is held by no link"
        );
    }

    #[test]
    fn named_listen_address_is_one_address_and_no_wildcard() {
        assert_eq!(named_listen_addr("198.51.100.2"), Some("198.51.100.2".parse().unwrap()));
        assert_eq!(named_listen_addr("[fd00::2]"), Some("fd00::2".parse().unwrap()));
        assert_eq!(
            named_listen_addr("::ffff:198.51.100.2"),
            Some("198.51.100.2".parse().unwrap()),
            "a mapped address is the IPv4 one"
        );
        for refused in [
            "0.0.0.0",
            "::",
            "127.0.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "[::ffff:0.0.0.0]",
            "198.51.100.2:7341",
            "desk.lan",
            "",
        ] {
            assert_eq!(named_listen_addr(refused), None, "{refused}");
        }
    }

    #[test]
    fn the_listen_yes_is_recorded_privately_and_read_back() {
        let root = agent::state_dirs::test_workspace_dir("pairing-listen");
        let dir = root.join("remote");
        assert_eq!(confirmed_listen(&dir).unwrap(), None);
        record_listen(&dir, "198.51.100.2", IfaceKind::Plain, 1_762_400_000).unwrap();
        assert_eq!(confirmed_listen(&dir).unwrap().as_deref(), Some("198.51.100.2"));
        assert_eq!(
            confirmed_listen_with_kind(&dir).unwrap(),
            Some(("198.51.100.2".to_owned(), IfaceKind::Plain))
        );
        let mode = std::fs::metadata(dir.join(LISTEN_FILE)).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);

        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join(LISTEN_FILE), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            confirmed_listen(&dir).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied,
            "a record others could have written is no yes"
        );
    }

    #[test]
    fn a_yes_covers_its_address_only_on_a_link_of_the_kind_asked_about() {
        let root = agent::state_dirs::test_workspace_dir("pairing-listen-kind");
        let dir = root.join("remote");
        let on = |kind: Option<IfaceKind>| confirmed_listen_on(&dir, move |_| kind).unwrap();
        assert_eq!(on(Some(IfaceKind::WireGuard)), None, "nothing recorded yet");
        record_listen(&dir, "198.51.100.2", IfaceKind::WireGuard, 1_762_400_000).unwrap();
        assert_eq!(on(Some(IfaceKind::WireGuard)).as_deref(), Some("198.51.100.2"));
        assert_eq!(on(Some(IfaceKind::Plain)), None, "the address moved to a plain link");
        assert_eq!(
            on(Some(IfaceKind::Tailscale)),
            None,
            "the address moved to another tunnel"
        );
        assert_eq!(on(None), None, "no link holds the address");
        // The address is looked up, not the record's text taken on trust.
        let asked = std::cell::Cell::new(None);
        confirmed_listen_on(&dir, |addr| {
            asked.set(Some(addr));
            Some(IfaceKind::WireGuard)
        })
        .unwrap();
        assert_eq!(asked.get(), Some("198.51.100.2".parse().unwrap()));
    }
}
