use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKey {
    pub key_type: String,
    pub sha256: String,
}

#[derive(Debug)]
pub enum HostKeyRefusal {
    NoKey,
    Mismatch { expected: String, observed: String },
}

impl std::fmt::Display for HostKeyRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoKey => formatter.write_str("SSH did not record a host key"),
            Self::Mismatch { expected, observed } => write!(
                formatter,
                "SSH host key changed from {expected} to {observed}; refusing lifecycle command"
            ),
        }
    }
}

impl std::error::Error for HostKeyRefusal {}

/// Prints nothing: SSH's own known-hosts files remain the authority for accepting a key.
pub fn probe_main(args: &[OsString]) -> i32 {
    if record(args).is_ok() {
        0
    } else {
        1
    }
}

fn record(args: &[OsString]) -> io::Result<()> {
    let [record, host, key_type, sha256] = args else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected record, host, key type and fingerprint",
        ));
    };
    let record = Path::new(record);
    let parent = record
        .parent()
        .filter(|parent| crate::private_fs::is_private_dir(parent))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "host-key record needs a private directory",
            )
        })?;
    let name = record
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| {
            name.strip_prefix("kh-")
                .map(|id| id.strip_suffix(super::master::TERMINAL_SUFFIX).unwrap_or(id))
                .is_some_and(|id| id.len() == 16 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid host-key record name"))?;
    let values: Vec<_> = [host, key_type, sha256]
        .into_iter()
        .map(|value| {
            value
                .to_str()
                .filter(|value| !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid host-key observation"))
        })
        .collect::<io::Result<_>>()?;
    let bytes = serde_json::to_vec(&values)?;
    let directory = crate::private_fs::open_dir(parent)?;
    crate::private_fs::publish_private_file(&directory, OsStr::new(name), &bytes)?;
    Ok(())
}

pub fn read_key(record: &Path) -> io::Result<HostKey> {
    let text = crate::private_fs::read_private_nofollow_capped(record, 16384)?;
    let [_host, key_type, sha256]: [String; 3] = serde_json::from_str(&text)?;
    if key_type.is_empty() || key_type == "NONE" || !sha256.starts_with("SHA256:") || sha256.len() <= 7 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, HostKeyRefusal::NoKey));
    }
    Ok(HostKey { key_type, sha256 })
}

/// Quoting here is OpenSSH's local argv syntax, not a remote shell command.
pub fn command(exe: &Path, record: &Path) -> io::Result<OsString> {
    fn token(path: &Path) -> io::Result<String> {
        super::validate_option_path(path, true)?;
        Ok(format!("\"{}\"", path.to_str().expect("validated UTF-8")))
    }
    super::validate_state_path(record)?;
    Ok(format!(
        "KnownHostsCommand={} host known-hosts-probe {} %H %t %f",
        token(exe)?,
        token(record)?
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_hosts_probe_prints_nothing_and_records_the_key() {
        super::super::tests::isolated(
            "ssh::known_hosts::tests::known_hosts_probe_prints_nothing_and_records_the_key",
            |dir| {
                let path = dir.join("kh-0123456789abcdef");
                assert_eq!(
                    probe_main(&[
                        path.clone().into(),
                        "key-alias".into(),
                        "ssh-ed25519".into(),
                        "SHA256:example".into()
                    ]),
                    0
                );
                assert_eq!(
                    crate::private_fs::read_private_nofollow_capped(&path, 16384).unwrap(),
                    "[\"key-alias\",\"ssh-ed25519\",\"SHA256:example\"]"
                );
                assert_eq!(
                    read_key(&path).unwrap(),
                    HostKey {
                        key_type: "ssh-ed25519".into(),
                        sha256: "SHA256:example".into()
                    }
                );
                assert_eq!(probe_main(&[]), 1);
                // The terminals' master records under its own name; nothing else past the id is a record.
                let observe = |name: &str| {
                    probe_main(&[
                        dir.join(name).into(),
                        "key-alias".into(),
                        "ssh-ed25519".into(),
                        "SHA256:example".into(),
                    ])
                };
                assert_eq!(observe("kh-0123456789abcdef-t"), 0);
                assert_eq!(
                    read_key(&dir.join("kh-0123456789abcdef-t")).unwrap().sha256,
                    "SHA256:example"
                );
                for name in [
                    "kh-0123456789abcdef-x",
                    "kh-0123456789abcdef-t-t",
                    "kh-0123456789abcde-t",
                ] {
                    assert_eq!(observe(name), 1, "{name}");
                    assert!(!dir.join(name).exists(), "{name}");
                }
            },
        );
    }

    #[test]
    fn callback_refuses_expansion_and_tokenization_in_paths() {
        for path in [
            "/opt/a b",
            "/opt/a${HOME}",
            "/opt/a'b",
            "/opt/a\"b",
            "/opt/a\\b",
            "/opt/a\nb",
        ] {
            assert!(
                command(Path::new(path), Path::new("/private/kh-0123456789abcdef")).is_err(),
                "{path}"
            );
        }
        assert!(command(Path::new("/eitri"), Path::new("/private/100%/kh-0123456789abcdef")).is_err());
    }

    #[test]
    fn callback_preserves_local_paths_and_ssh_tokens() {
        let command = command(Path::new("/opt/100%/eitri"), Path::new("/private/kh-0123456789abcdef")).unwrap();
        assert_eq!(
            command,
            "KnownHostsCommand=\"/opt/100%/eitri\" host known-hosts-probe \"/private/kh-0123456789abcdef\" %H %t %f"
        );
    }
}
