//! The enrolled devices a remote observer's state-changing messages are checked against.
//!
//! The file holds, for each device, its id, the name it was enrolled with, the SHA-256 of its bearer token, what it
//! may do, how it was enrolled and what is known of it since. The token itself is never stored: a device presents it
//! with each message that would change something, and [`DeviceStore::verify`] hashes what was presented and compares
//! the hashes in constant time, so how long a check takes says nothing about how much of a hash matched.
//!
//! The file decides who may stop an agent's work, so it counts only when it is a regular file of this user that no
//! one else can read or write (no group or other bit in its mode), opened without following a link, and checked on
//! the handle that is read. Anything else is refused whole: a store that is not trusted holds no device.
//!
//! Several processes write it (each window, and `eitri remote` in a terminal), so every write goes through
//! [`update`]: an `flock` on `devices.lock` beside it, held from before the read until after the new file is renamed
//! into place, so a write never starts from a file another writer is about to replace.
//!
//! The format, one JSON object, unknown fields refused. Version 2:
//!
//! ```json
//! {"v":2,"devices":[{"id":"d1","name":"Pixel 8","token_sha256":"<64 lowercase hex>","grant":"watch+steer",
//!   "recipe":"ssh","confirmed":{"at":1762400000},"push_endpoint":null,"last_seen":null,"last_host":null,
//!   "enrolled_host":"127.0.0.1:7341"}]}
//! ```
//!
//! Version 1 (`id`, `name`, `token_sha256` and `grant` only) is still read; what it did not record reads as unknown
//! (`recipe` `unknown`, no confirmation, `enrolled_host` empty), and the next write writes version 2.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The version of the store this build writes.
pub const DEVICES_FORMAT_V: u32 = 2;

/// The older version this build still reads.
const DEVICES_FORMAT_V1: u32 = 1;

/// The largest store read or written; a bigger file is refused unread, and a change that would make one is not
/// written.
pub const DEVICES_MAX_BYTES: u64 = 1024 * 1024;

/// How long [`update`] waits for another writer's lock: a read, a change and a rename of at most
/// [`DEVICES_MAX_BYTES`] hold it far less.
const LOCK_WITHIN: Duration = Duration::from_secs(2);

/// The pause between two tries at the lock.
const LOCK_PAUSE: Duration = Duration::from_millis(4);

/// What a device may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Grant {
    /// Follow tabs only.
    #[serde(rename = "watch")]
    Watch,
    /// Follow tabs and send what the remote list allows.
    #[serde(rename = "watch+steer")]
    WatchSteer,
}

/// How a device reaches `serve`, as it was enrolled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recipe {
    /// Through an SSH forward to the loopback listener.
    Ssh,
    /// To the listener directly (loopback, a tunnel's address, a reverse proxy).
    Direct,
    /// Enrolled before the store recorded it.
    Unknown,
}

/// The desk's yes to a device, recorded with it, so every window sees the one confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Confirmed {
    /// When, in seconds since the Unix epoch.
    pub at: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreFileV1 {
    #[allow(dead_code)]
    v: u32,
    devices: Vec<DeviceEntryV1>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceEntryV1 {
    id: String,
    name: String,
    token_sha256: String,
    grant: Grant,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreFileV2 {
    v: u32,
    devices: Vec<DeviceEntryV2>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceEntryV2 {
    id: String,
    name: String,
    token_sha256: String,
    grant: Grant,
    recipe: Recipe,
    confirmed: Option<Confirmed>,
    push_endpoint: Option<String>,
    last_seen: Option<u64>,
    last_host: Option<String>,
    enrolled_host: String,
}

/// One enrolled device.
#[derive(Clone, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// The SHA-256 of its bearer token; the token is never kept.
    pub token_sha256: [u8; 32],
    pub grant: Grant,
    pub recipe: Recipe,
    /// The desk's yes, if one was given.
    pub confirmed: Option<Confirmed>,
    /// Where its notifications go.
    pub push_endpoint: Option<String>,
    /// When it was last connected, in seconds since the Unix epoch.
    pub last_seen: Option<u64>,
    /// The `Host` it last reached `serve` through.
    pub last_host: Option<String>,
    /// The `Host` it enrolled through.
    pub enrolled_host: String,
}

impl std::fmt::Debug for Device {
    /// Neither the token's hash nor the push endpoint, which may itself be a credential.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("grant", &self.grant)
            .field("recipe", &self.recipe)
            .field("confirmed", &self.confirmed)
            .field("push_endpoint", &self.push_endpoint.as_ref().map(|_| "<set>"))
            .field("last_seen", &self.last_seen)
            .field("last_host", &self.last_host)
            .field("enrolled_host", &self.enrolled_host)
            .finish()
    }
}

/// The devices of one read of the store.
#[derive(Default)]
pub struct DeviceStore {
    devices: Vec<Device>,
}

impl std::fmt::Debug for DeviceStore {
    /// Ids and grants only.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.devices.iter().map(|d| (&d.id, d.grant)))
            .finish()
    }
}

impl DeviceStore {
    /// Reads the store at `path`. Refuses (`PermissionDenied`) a link, anything but a regular file, a file of another
    /// user and one with any group or other permission bit; refuses (`InvalidData`) a file over
    /// [`DEVICES_MAX_BYTES`], a version this build does not read, a field it does not know, a hash that is not 64
    /// lowercase hex digits and an id given twice.
    pub fn load(path: &Path) -> io::Result<DeviceStore> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.uid() != agent::private_fs::euid() || meta.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is not a private file of this user", path.display()),
            ));
        }
        let over_cap = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is over {DEVICES_MAX_BYTES} bytes", path.display()),
            )
        };
        if meta.len() > DEVICES_MAX_BYTES {
            return Err(over_cap());
        }
        let mut bytes = Vec::new();
        file.take(DEVICES_MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > DEVICES_MAX_BYTES {
            return Err(over_cap());
        }
        DeviceStore::parse(&bytes)
    }

    fn parse(bytes: &[u8]) -> io::Result<DeviceStore> {
        let invalid = |why: String| io::Error::new(io::ErrorKind::InvalidData, why);
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| invalid(format!("device store: {e}")))?;
        let version = value.get("v").and_then(serde_json::Value::as_u64);
        let entries: Vec<DeviceEntryV2> = match version {
            Some(v) if v == u64::from(DEVICES_FORMAT_V) => {
                let file: StoreFileV2 =
                    serde_json::from_value(value).map_err(|e| invalid(format!("device store: {e}")))?;
                file.devices
            }
            Some(v) if v == u64::from(DEVICES_FORMAT_V1) => {
                let file: StoreFileV1 =
                    serde_json::from_value(value).map_err(|e| invalid(format!("device store: {e}")))?;
                file.devices
                    .into_iter()
                    .map(|entry| DeviceEntryV2 {
                        id: entry.id,
                        name: entry.name,
                        token_sha256: entry.token_sha256,
                        grant: entry.grant,
                        recipe: Recipe::Unknown,
                        confirmed: None,
                        push_endpoint: None,
                        last_seen: None,
                        last_host: None,
                        enrolled_host: String::new(),
                    })
                    .collect()
            }
            other => {
                return Err(invalid(format!(
                    "device store version {other:?} is not {DEVICES_FORMAT_V1} or {DEVICES_FORMAT_V}"
                )))
            }
        };
        let mut devices: Vec<Device> = Vec::with_capacity(entries.len());
        for entry in entries {
            if devices.iter().any(|d| d.id == entry.id) {
                return Err(invalid(format!("device {:?} is listed twice", entry.id)));
            }
            let token_sha256 = parse_hash(&entry.token_sha256)
                .ok_or_else(|| invalid(format!("device {:?}: the token hash is not 64 lowercase hex", entry.id)))?;
            devices.push(Device {
                id: entry.id,
                name: entry.name,
                token_sha256,
                grant: entry.grant,
                recipe: entry.recipe,
                confirmed: entry.confirmed,
                push_endpoint: entry.push_endpoint,
                last_seen: entry.last_seen,
                last_host: entry.last_host,
                enrolled_host: entry.enrolled_host,
            });
        }
        Ok(DeviceStore { devices })
    }

    /// The store as version 2 writes it.
    fn to_bytes(&self) -> Vec<u8> {
        let file = StoreFileV2 {
            v: DEVICES_FORMAT_V,
            devices: self
                .devices
                .iter()
                .map(|d| DeviceEntryV2 {
                    id: d.id.clone(),
                    name: d.name.clone(),
                    token_sha256: hex(&d.token_sha256),
                    grant: d.grant,
                    recipe: d.recipe,
                    confirmed: d.confirmed,
                    push_endpoint: d.push_endpoint.clone(),
                    last_seen: d.last_seen,
                    last_host: d.last_host.clone(),
                    enrolled_host: d.enrolled_host.clone(),
                })
                .collect(),
        };
        // Strings, numbers and enums of unit variants always serialize.
        serde_json::to_vec(&file).unwrap_or_default()
    }

    /// Every device, in the order they were added.
    pub fn devices(&self) -> &[Device] {
        &self.devices
    }

    pub fn get(&self, id: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Device> {
        self.devices.iter_mut().find(|d| d.id == id)
    }

    /// Adds `device`; `false`, changing nothing, when its id is taken.
    pub fn insert(&mut self, device: Device) -> bool {
        if self.get(&device.id).is_some() {
            return false;
        }
        self.devices.push(device);
        true
    }

    /// Takes the device `id` out, and with it everything kept of it.
    pub fn remove(&mut self, id: &str) -> Option<Device> {
        let at = self.devices.iter().position(|d| d.id == id)?;
        Some(self.devices.remove(at))
    }

    /// The name and grant of `device` when `token` is its token. The token's hash is compared with the stored one
    /// byte by byte without stopping at the first difference; an unknown device is compared with a hash no token
    /// has, so it costs the same.
    pub fn verify(&self, device: &str, token: &str) -> Option<(String, Grant)> {
        let presented: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let found = self.devices.iter().find(|d| d.id == device);
        let stored = found.map_or([0u8; 32], |d| d.token_sha256);
        let same = hashes_equal(&presented, &stored);
        match found {
            Some(d) if same => Some((d.name.clone(), d.grant)),
            _ => None,
        }
    }
}

/// Reads the store at `path` under its lock, lets `f` change it, and writes it back when it changed: 0600, to a
/// temporary file beside it, then renamed into place, so a reader sees the old store or the new one, never part of
/// one. A missing store starts empty, and its directory is made (0700). A store [`DeviceStore::load`] refuses is left
/// as it is and the error returned, so nothing is written over a file that was not ours to read. Waits up to two
/// seconds for another writer's lock (`WouldBlock` after that): never call it on the UI thread.
pub fn update<T>(path: &Path, f: impl FnOnce(&mut DeviceStore) -> T) -> io::Result<T> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a device store path with no directory"))?;
    agent::private_fs::create_private_dir_all(dir, dir.parent().unwrap_or(dir))?;
    let _lock = lock(path)?;
    let mut store = match DeviceStore::load(path) {
        Ok(store) => store,
        Err(e) if e.kind() == io::ErrorKind::NotFound => DeviceStore::default(),
        Err(e) => return Err(e),
    };
    let before = store.to_bytes();
    let out = f(&mut store);
    let after = store.to_bytes();
    if after != before {
        if after.len() as u64 > DEVICES_MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the device store would pass {DEVICES_MAX_BYTES} bytes"),
            ));
        }
        save(path, &after)?;
    }
    Ok(out)
}

/// `devices.lock` beside the store at `path`.
fn lock_path(path: &Path) -> PathBuf {
    path.with_extension("lock")
}

/// The store's lock, tried until [`LOCK_WITHIN`] has passed.
fn lock(path: &Path) -> io::Result<agent::private_fs::PrivateLock> {
    let lock = agent::private_fs::PrivateLock::open(&lock_path(path))?;
    let deadline = Instant::now() + LOCK_WITHIN;
    loop {
        if lock.try_lock()? {
            return Ok(lock);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("another process is writing {}", path.display()),
            ));
        }
        std::thread::sleep(LOCK_PAUSE);
    }
}

/// Writes `bytes` beside `path` and renames them into place.
fn save(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = crate::state_paths::temporary(path);
    let written = (|| {
        let mut file = agent::private_fs::open_private(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Whether the two hashes are equal, looking at every byte whatever the first difference.
fn hashes_equal(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= std::hint::black_box(x ^ y);
    }
    std::hint::black_box(diff) == 0
}

/// The 32 bytes 64 lowercase hex digits spell, or `None`.
fn parse_hash(text: &str) -> Option<[u8; 32]> {
    let digits = text.as_bytes();
    if digits.len() != 64 {
        return None;
    }
    let value = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (i, pair) in digits.chunks(2).enumerate() {
        out[i] = value(pair[0])? << 4 | value(pair[1])?;
    }
    Some(out)
}

/// The store's text form of a token's hash: SHA-256, 64 lowercase hex digits.
pub fn token_hash_hex(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()).into())
}

/// `bytes` as lowercase hex digits.
fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The longest push endpoint kept.
pub const PUSH_ENDPOINT_MAX_BYTES: usize = 512;

/// What a phone is told when its push endpoint is not one.
pub const BAD_PUSH_ENDPOINT: &str =
    "a push endpoint is an http or https URL of at most 512 bytes, with a host and no user information";

/// Whether `url` may be kept as a push endpoint: ASCII without spaces or control characters, at most
/// [`PUSH_ENDPOINT_MAX_BYTES`], `http://` or `https://`, then an authority with a host, no `@` (user information
/// would be a password kept beside the store's hashes), and a port of digits if one is given.
pub fn push_endpoint_ok(url: &str) -> Result<(), &'static str> {
    let bad = Err(BAD_PUSH_ENDPOINT);
    if url.len() > PUSH_ENDPOINT_MAX_BYTES || !url.bytes().all(|b| b.is_ascii_graphic()) || url.contains('\\') {
        return bad;
    }
    let lower = url.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://")) else {
        return bad;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return bad;
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(bracketed) => match bracketed.split_once(']') {
            Some((host, "")) => (host, None),
            Some((host, after)) => match after.strip_prefix(':') {
                Some(port) => (host, Some(port)),
                None => return bad,
            },
            None => return bad,
        },
        None => match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    if host.is_empty() || (!authority.starts_with('[') && host.contains(['[', ']'])) {
        return bad;
    }
    if let Some(port) = port {
        if !port.bytes().all(|b| b.is_ascii_digit()) || !matches!(port.parse::<u16>(), Ok(p) if p > 0) {
            return bad;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn store_file(label: &str, body: &str, mode: u32) -> PathBuf {
        let dir = agent::state_dirs::test_workspace_dir(label);
        let path = dir.join("devices.json");
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    fn one_device(grant: &str) -> String {
        format!(
            r#"{{"v":1,"devices":[{{"id":"d1","name":"phone","token_sha256":"{}","grant":"{grant}"}}]}}"#,
            token_hash_hex("tok")
        )
    }

    #[test]
    fn store_verifies_hash_and_grant() {
        let path = store_file("remote-devices-verify", &one_device("watch+steer"), 0o600);
        let store = DeviceStore::load(&path).expect("a private store loads");
        assert_eq!(store.verify("d1", "tok"), Some(("phone".to_owned(), Grant::WatchSteer)));
        assert_eq!(store.verify("d1", "tok2"), None, "a wrong token");
        assert_eq!(store.verify("d1", ""), None, "no token");
        assert_eq!(store.verify("d2", "tok"), None, "an unknown device");

        let watch = store_file("remote-devices-watch", &one_device("watch"), 0o600);
        assert_eq!(
            DeviceStore::load(&watch).unwrap().verify("d1", "tok"),
            Some(("phone".to_owned(), Grant::Watch))
        );
    }

    #[test]
    fn store_refuses_a_group_readable_file() {
        let path = store_file("remote-devices-0640", &one_device("watch+steer"), 0o640);
        let err = DeviceStore::load(&path).expect_err("a group-readable store is refused");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let other = store_file("remote-devices-0604", &one_device("watch+steer"), 0o604);
        assert!(DeviceStore::load(&other).is_err(), "an other-readable store is refused");
    }

    #[test]
    fn store_refuses_a_link_in_its_place() {
        let target = store_file("remote-devices-link", &one_device("watch+steer"), 0o600);
        let link = target.with_file_name("link.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(DeviceStore::load(&link).is_err(), "a link is not followed");
    }

    #[test]
    fn store_refuses_malformed_content() {
        let hash = token_hash_hex("tok");
        for body in [
            r#"{"v":3,"devices":[]}"#.to_owned(),
            r#"{"v":1,"devices":[],"extra":1}"#.to_owned(),
            format!(r#"{{"v":1,"devices":[{{"id":"d1","name":"p","token_sha256":"{hash}","grant":"admin"}}]}}"#),
            format!(
                r#"{{"v":1,"devices":[{{"id":"d1","name":"p","token_sha256":"{}","grant":"watch"}}]}}"#,
                &hash[..62]
            ),
            format!(
                r#"{{"v":1,"devices":[{{"id":"d1","name":"p","token_sha256":"{}","grant":"watch"}}]}}"#,
                hash.to_uppercase()
            ),
            format!(
                r#"{{"v":1,"devices":[{{"id":"d1","name":"p","token_sha256":"{hash}","grant":"watch"}},{{"id":"d1","name":"q","token_sha256":"{hash}","grant":"watch"}}]}}"#
            ),
        ] {
            assert!(DeviceStore::parse(body.as_bytes()).is_err(), "{body}");
        }
    }

    fn hash_of(token: &str) -> [u8; 32] {
        Sha256::digest(token.as_bytes()).into()
    }

    fn device(id: &str, token: &str) -> Device {
        Device {
            id: id.into(),
            name: format!("phone {id}"),
            token_sha256: hash_of(token),
            grant: Grant::Watch,
            recipe: Recipe::Ssh,
            confirmed: Some(Confirmed { at: 1_762_400_000 }),
            push_endpoint: None,
            last_seen: None,
            last_host: None,
            enrolled_host: "127.0.0.1:7341".into(),
        }
    }

    #[test]
    fn store_v1_files_load_as_v2() {
        let path = store_file("remote-devices-v1", &one_device("watch+steer"), 0o600);
        let store = DeviceStore::load(&path).expect("a v1 store loads");
        let d1 = store.get("d1").expect("its device");
        assert_eq!(
            (d1.recipe, d1.confirmed, d1.enrolled_host.as_str(), d1.last_seen),
            (Recipe::Unknown, None, "", None),
            "what v1 did not record is unknown"
        );
        assert_eq!(store.verify("d1", "tok"), Some(("phone".to_owned(), Grant::WatchSteer)));

        update(&path, |store| store.get_mut("d1").unwrap().last_seen = Some(42)).expect("the update writes");
        let text = std::fs::read_to_string(&path).unwrap();
        let written: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(written["v"], 2, "written back as v2: {text}");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let again = DeviceStore::load(&path).unwrap();
        assert_eq!(again.get("d1").unwrap().last_seen, Some(42));
        assert_eq!(again.verify("d1", "tok"), Some(("phone".to_owned(), Grant::WatchSteer)));
    }

    #[test]
    fn store_v2_round_trips_every_field() {
        let dir = agent::state_dirs::test_workspace_dir("remote-devices-v2");
        let path = dir.join("remote").join("devices.json");
        let mut full = device("d1", "tok");
        full.push_endpoint = Some("https://ntfy.example/t".into());
        full.last_host = Some("127.0.0.1:7341".into());
        full.last_seen = Some(9);
        let kept = full.clone();
        assert!(
            update(&path, move |store| store.insert(full)).unwrap(),
            "a new id is taken"
        );
        assert!(
            !update(&path, |store| store.insert(device("d1", "other"))).unwrap(),
            "an id taken is refused"
        );
        let store = DeviceStore::load(&path).unwrap();
        assert_eq!(store.devices(), [kept]);
        let shown = format!("{store:?} {:?}", store.devices()[0]);
        assert!(!shown.contains("ntfy"), "the endpoint is not shown: {shown}");
        let dir_mode = std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "the store's directory is made private");
    }

    #[test]
    fn update_leaves_an_untrusted_store_alone() {
        let path = store_file("remote-devices-untrusted", &one_device("watch"), 0o644);
        let err = update(&path, |store| store.remove("d1")).expect_err("a store others can read is not used");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            one_device("watch"),
            "and not rewritten"
        );
    }

    #[test]
    fn interleaved_revoke_and_add_keep_the_revoke() {
        let path = store_file("remote-devices-interleaved", &one_device("watch+steer"), 0o600);
        let (inside, entered) = std::sync::mpsc::channel();
        let revoking = {
            let path = path.clone();
            std::thread::spawn(move || {
                update(&path, |store| {
                    inside.send(()).unwrap();
                    // The other writer starts now and must wait for this one's write.
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    store.remove("d1")
                })
            })
        };
        entered.recv().unwrap();
        let adding = {
            let path = path.clone();
            std::thread::spawn(move || update(&path, |store| store.insert(device("d2", "tok2"))))
        };
        assert!(revoking.join().unwrap().unwrap().is_some(), "d1 was there to revoke");
        assert!(adding.join().unwrap().unwrap(), "d2 was added");
        let store = DeviceStore::load(&path).unwrap();
        assert!(store.get("d1").is_none(), "the revoke survives the add");
        assert!(store.get("d2").is_some(), "and the add survives too");
        assert_eq!(store.verify("d1", "tok"), None);
    }

    #[test]
    fn hashes_compare_whole() {
        let a = [7u8; 32];
        let mut b = a;
        assert!(hashes_equal(&a, &b));
        b[31] ^= 1;
        assert!(!hashes_equal(&a, &b));
        b = a;
        b[0] ^= 0x80;
        assert!(!hashes_equal(&a, &b));
    }
}
