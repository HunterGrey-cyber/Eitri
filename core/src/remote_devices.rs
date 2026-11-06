//! The enrolled devices a remote observer's state-changing messages are checked against.
//!
//! The file holds, for each device, its id, the name it was enrolled with, the SHA-256 of its bearer token and what it
//! may do. The token itself is never stored: a device presents it with each message that would change something, and
//! [`DeviceStore::verify`] hashes what was presented and compares the hashes in constant time, so how long a check
//! takes says nothing about how much of a hash matched.
//!
//! The file decides who may stop an agent's work, so it counts only when it is a regular file of this user that no
//! one else can read or write (no group or other bit in its mode), opened without following a link, and checked on
//! the handle that is read. Anything else is refused whole: a store that is not trusted holds no device.
//!
//! The format, one JSON object, unknown fields refused:
//!
//! ```json
//! {"v":1,"devices":[{"id":"d1","name":"Pixel 8","token_sha256":"<64 lowercase hex>","grant":"watch+steer"}]}
//! ```

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

/// The only version of the store this build reads.
pub const DEVICES_FORMAT_V: u32 = 1;

/// The largest store read; a bigger file is refused unread.
pub const DEVICES_MAX_BYTES: u64 = 1024 * 1024;

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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreFile {
    v: u32,
    devices: Vec<DeviceEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceEntry {
    id: String,
    name: String,
    token_sha256: String,
    grant: Grant,
}

struct Device {
    id: String,
    name: String,
    token_sha256: [u8; 32],
    grant: Grant,
}

/// The devices of one read of the store.
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
    /// [`DEVICES_MAX_BYTES`], another version, a field it does not know, a hash that is not 64 lowercase hex digits
    /// and an id given twice.
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
        let file: StoreFile = serde_json::from_slice(bytes).map_err(|e| invalid(format!("device store: {e}")))?;
        if file.v != DEVICES_FORMAT_V {
            return Err(invalid(format!(
                "device store version {} is not {DEVICES_FORMAT_V}",
                file.v
            )));
        }
        let mut devices: Vec<Device> = Vec::with_capacity(file.devices.len());
        for entry in file.devices {
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
            });
        }
        Ok(DeviceStore { devices })
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
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
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
            r#"{"v":2,"devices":[]}"#.to_owned(),
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
