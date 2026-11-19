use agent::ssh::{ConnectError, CHANNEL_OPTIONS};
use base64::Engine;
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[derive(Debug)]
pub struct FirstContact {
    host: String,
    pub key_type: String,
    pub sha256: String,
    scratch_known_hosts: PathBuf,
    scratch_line: Vec<u8>,
}

impl Drop for FirstContact {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.scratch_known_hosts);
    }
}

/// How long each of first contact's connections may take. A floor sized for a slow link, not a measurement:
/// nothing is connected yet to measure. A connection that stops before authentication is a few round trips, and
/// a lossy or relayed link can stretch that well past ten seconds; only a host that never answers is made to wait
/// this long.
const CONTACT_DEADLINE: Duration = Duration::from_secs(30);

pub fn first_contact(host: &str, private_dir: &Path) -> Result<FirstContact, ConnectError> {
    agent::ssh::validate_host(host)?;
    agent::ssh::check_openssh()?;
    agent::ssh::validate_state_path(private_dir)?;
    agent::private_fs::require_safe_state_parent(private_dir, "SSH first contact")?;
    agent::private_fs::create_private_dir_all(private_dir, private_dir)?;
    if !agent::private_fs::is_private_dir(private_dir) {
        return Err("SSH first contact needs a private real directory".to_owned().into());
    }

    // A scratch file must never turn an existing changed-key refusal into a new-key question.
    let mut strict = contact_command();
    strict.args(["-o", "StrictHostKeyChecking=yes", "--", host, "true"]);
    let output = agent::ssh::run(&mut strict, CONTACT_DEADLINE)?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.timed_out
        || !stderr.contains(" host key is known for ")
        || !stderr.contains("you have requested strict checking")
        || stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
    {
        return Err(output.connect_error(host));
    }

    let path = private_dir.join(format!("kh-first-{}", uuid::Uuid::new_v4().simple()));
    let mut contact = FirstContact {
        host: host.into(),
        key_type: String::new(),
        sha256: String::new(),
        scratch_known_hosts: path,
        scratch_line: Vec::new(),
    };
    agent::private_fs::write_private(&contact.scratch_known_hosts, b"")?;
    let path = contact.scratch_known_hosts.to_str().expect("validated UTF-8 directory");
    let mut command = contact_command();
    command.args([
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        &format!("UserKnownHostsFile=\"{path}\""),
    ]);
    // Capture precedes authentication. A server allowing none authentication runs only the fixed no-op.
    command.args(["-o", "KnownHostsCommand=none", "--", host, "true"]);
    let output = agent::ssh::run(&mut command, CONTACT_DEADLINE)?;
    // The key is written before authentication, including when the server refuses the initial "none" attempt.
    contact.scratch_line = agent::private_fs::read_private_nofollow_bytes_capped(&contact.scratch_known_hosts, 16384)?;
    if contact.scratch_line.is_empty() {
        return Err(output.connect_error(host));
    }
    let (key_type, sha256) = identity(&contact.scratch_line)?;
    contact.key_type = key_type;
    contact.sha256 = sha256;
    Ok(contact)
}

fn contact_command() -> Command {
    let mut command = Command::new("ssh");
    command.args(CHANNEL_OPTIONS).args([
        "-o",
        "KnownHostsCommand=none",
        "-o",
        "VerifyHostKeyDNS=no",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "-o",
        "PreferredAuthentications=none",
        "-o",
        "PubkeyAuthentication=no",
        "-o",
        "PasswordAuthentication=no",
        "-o",
        "KbdInteractiveAuthentication=no",
        "-o",
        "UpdateHostKeys=no",
    ]);
    command
}

fn identity(line: &[u8]) -> io::Result<(String, String)> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "SSH scratch file must contain exactly one host-key line",
        )
    };
    let text = std::str::from_utf8(line).map_err(|_| invalid())?;
    if text.lines().count() != 1 || !text.ends_with('\n') {
        return Err(invalid());
    }
    let fields: Vec<_> = text.split_whitespace().collect();
    let [_host, key_type, key] = fields.as_slice() else {
        return Err(invalid());
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(key)
        .map_err(|_| invalid())?;
    let fingerprint = base64::engine::general_purpose::STANDARD_NO_PAD.encode(Sha256::digest(&bytes));
    Ok(((*key_type).to_owned(), format!("SHA256:{fingerprint}")))
}

/// Called only after the person accepts the displayed fingerprint.
pub fn accept_first_contact(contact: FirstContact, user_known_hosts: &Path) -> io::Result<()> {
    let line = agent::private_fs::read_private_nofollow_bytes_capped(&contact.scratch_known_hosts, 16384)?;
    if line != contact.scratch_line {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SSH scratch key changed after it was shown",
        ));
    }
    if !user_known_hosts.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "known-hosts path must be absolute",
        ));
    }
    let private_dir = contact.scratch_known_hosts.parent().expect("scratch directory");
    if !agent::private_fs::is_private_dir(private_dir) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SSH acceptance needs a private real directory",
        ));
    }
    let lock = agent::private_fs::PrivateLock::open(&private_dir.join("known-hosts-accept.lock"))?;
    while !lock.try_lock()? {
        std::thread::sleep(Duration::from_millis(10));
    }
    // OpenSSH judges aliases, hashed entries and all configured known-hosts files again under the lock.
    if accepted_since_display(&contact, None)? || accepted_since_display(&contact, Some(user_known_hosts))? {
        return Ok(());
    }
    let target = resolve_user_file(user_known_hosts)?;
    let parent = target.parent().expect("absolute path");
    let meta = std::fs::metadata(parent)?;
    if !meta.is_dir() || meta.uid() != agent::private_fs::euid() || meta.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "known-hosts parent must be an existing directory owned by this user and not writable by others",
        ));
    }
    let mut file = open_user_known_hosts(&target)?;
    let mut previous = Vec::new();
    const MAX_KNOWN_HOSTS_BYTES: u64 = 16 * 1024 * 1024;
    (&mut file).take(MAX_KNOWN_HOSTS_BYTES + 1).read_to_end(&mut previous)?;
    if previous.len() as u64 > MAX_KNOWN_HOSTS_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "known-hosts file exceeds 16 MiB",
        ));
    }
    if previous
        .split(|byte| *byte == b'\n')
        .any(|existing| existing == line.strip_suffix(b"\n").expect("scratch line ends with newline"))
    {
        return Ok(());
    }
    if !previous.is_empty() && !previous.ends_with(b"\n") {
        file.write_all(b"\n")?;
    }
    file.write_all(&line)?;
    file.sync_all()
}

fn accepted_since_display(contact: &FirstContact, user_file: Option<&Path>) -> io::Result<bool> {
    let private_dir = contact.scratch_known_hosts.parent().expect("scratch directory");
    let record = private_dir.join(format!("kh-{}", &uuid::Uuid::new_v4().simple().to_string()[..16]));
    struct Observation(PathBuf);
    impl Drop for Observation {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _observation = Observation(record.clone());
    let callback = agent::ssh::known_hosts::command(&std::env::current_exe()?, &record)?;
    let mut command = contact_command();
    command.args(["-o", "FingerprintHash=sha256", "-o", "StrictHostKeyChecking=yes", "-o"]);
    command.arg(callback);
    if let Some(file) = user_file {
        // OpenSSH's option tokenizer sees this path before it runs any command.
        agent::ssh::validate_state_path(file).map_err(io::Error::other)?;
        command
            .arg("-o")
            .arg(format!("UserKnownHostsFile=\"{}\"", file.display()));
    }
    command.args(["--", &contact.host, "true"]);
    let output = agent::ssh::run(&mut command, CONTACT_DEADLINE).map_err(io::Error::other)?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.timed_out
        && stderr.contains(" host key is known for ")
        && stderr.contains("you have requested strict checking")
        && !stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
    {
        return Ok(false);
    }
    if !output.timed_out && (output.status.success() || output.connect_error(&contact.host).login_hint.is_some()) {
        let key = agent::ssh::known_hosts::read_key(&record)?;
        if key.key_type == contact.key_type && key.sha256 == contact.sha256 {
            return Ok(true);
        }
        return Err(io::Error::other(
            "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! The key differs from the displayed fingerprint.",
        ));
    }
    Err(io::Error::other(output.connect_error(&contact.host)))
}

// Resolve the user's links once, including a link whose final target does not exist yet.
fn resolve_user_file(path: &Path) -> io::Result<PathBuf> {
    let mut target = path.to_owned();
    for _ in 0..40 {
        match std::fs::canonicalize(&target) {
            Ok(target) => return Ok(target),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        match std::fs::read_link(&target) {
            Ok(link) => {
                target = if link.is_absolute() {
                    link
                } else {
                    target.parent().expect("absolute path").join(link)
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound || error.kind() == io::ErrorKind::InvalidInput => {
                let parent = std::fs::canonicalize(target.parent().expect("absolute path"))?;
                return Ok(parent.join(
                    target
                        .file_name()
                        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "known-hosts needs a file name"))?,
                ));
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "too many known-hosts symlinks",
    ))
}

// User configuration keeps its inode, mode and owner. Only a newly created file is tightened.
fn open_user_known_hosts(path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .append(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match options.create_new(true).mode(0o600).open(path) {
                Ok(file) => {
                    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                    file
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => options.create_new(false).open(path)?,
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != agent::private_fs::euid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "known-hosts must be a regular file owned by this user",
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn first_contact_uses_the_same_known_host_sources_as_the_master() {
        let command = contact_command();
        assert!(command.get_args().any(|arg| arg == "KnownHostsCommand=none"));
    }

    #[test]
    fn first_contact_uses_a_scratch_known_hosts_and_only_a_fixed_noop() {
        super::super::tests::isolated(
            "remote_host::first_contact::tests::first_contact_uses_a_scratch_known_hosts_and_only_a_fixed_noop",
            |dir| {
                let contact = first_contact("example-host", dir).unwrap();
                assert_eq!(contact.key_type, "ssh-ed25519");
                assert_eq!(contact.sha256, "SHA256:kmYcvdi2GkPeWxB6XLjrZB8JHsy2Hm8luHMFp9GMvqk");
                assert!(contact.scratch_known_hosts.starts_with(dir));
                let argv = std::fs::read_to_string(dir.join("argv")).unwrap();
                assert!(argv.contains("StrictHostKeyChecking=accept-new\n"));
                for invocation in argv.split("BEGIN\n").filter(|part| !part.is_empty()) {
                    if invocation.starts_with("-V\n") {
                        continue;
                    }
                    assert!(!invocation.contains("ForkAfterAuthentication="));
                    assert!(invocation.ends_with("--\nexample-host\ntrue\nEND\n"));
                    assert!(invocation.contains("RemoteCommand=none\n"));
                }
                let scratch = contact.scratch_known_hosts.clone();
                drop(contact);
                assert!(!scratch.exists());
            },
        );
    }

    #[test]
    fn a_preflight_slower_than_fifteen_seconds_still_reaches_the_key() {
        super::super::tests::isolated(
            "remote_host::first_contact::tests::a_preflight_slower_than_fifteen_seconds_still_reaches_the_key",
            |dir| {
                std::fs::write(dir.join("slow-preflight"), "16").unwrap();
                let contact = first_contact("example-host", dir).unwrap();
                assert_eq!(contact.key_type, "ssh-ed25519");
            },
        );
    }

    #[test]
    fn accept_writes_exactly_the_scratch_line() {
        super::super::tests::isolated(
            "remote_host::first_contact::tests::accept_writes_exactly_the_scratch_line",
            |dir| {
                let contact = first_contact("example-host", dir).unwrap();
                let scratch = contact.scratch_known_hosts.clone();
                let line = agent::private_fs::read_private_nofollow_bytes_capped(&scratch, 16384).unwrap();
                let known = dir.join("known_hosts");
                agent::private_fs::write_private(&known, b"existing line\n").unwrap();
                accept_first_contact(contact, &known).unwrap();
                let mut expected = b"existing line\n".to_vec();
                expected.extend(line);
                assert_eq!(std::fs::read(known).unwrap(), expected);
                assert!(!scratch.exists());
            },
        );
    }

    #[test]
    fn a_changed_key_is_refused_without_a_scratch_connection() {
        super::super::tests::isolated(
            "remote_host::first_contact::tests::a_changed_key_is_refused_without_a_scratch_connection",
            |dir| {
                std::fs::write(dir.join("changed"), "").unwrap();
                let error = first_contact("example-host", dir).unwrap_err();
                assert!(error
                    .stderr_line
                    .contains("WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!"));
                assert!(!std::fs::read_to_string(dir.join("argv"))
                    .unwrap()
                    .contains("accept-new"));
            },
        );
    }

    #[test]
    fn first_contact_supports_openssh_8_5_and_8_6() {
        super::super::tests::isolated(
            "remote_host::first_contact::tests::first_contact_supports_openssh_8_5_and_8_6",
            |dir| {
                for version in ["OpenSSH_8.5p1, test", "OpenSSH_8.6p1, test"] {
                    std::env::set_var("EITRI_SSH_VERSION", version);
                    first_contact("example-host", dir).unwrap();
                }
            },
        );
    }

    #[test]
    fn a_different_key_type_is_refused_without_scratch_capture() {
        super::super::tests::isolated(
            "remote_host::first_contact::tests::a_different_key_type_is_refused_without_scratch_capture",
            |dir| {
                std::fs::write(dir.join("different-type"), "").unwrap();
                let error = first_contact("example-host", dir).unwrap_err();
                assert!(error.stderr_line.contains("Offending RSA key in /keys:3"));
                assert!(!std::fs::read_to_string(dir.join("argv"))
                    .unwrap()
                    .contains("accept-new"));
            },
        );
    }

    #[test]
    fn acceptance_preserves_user_modes_and_symlinks_and_skips_duplicates() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        super::super::tests::isolated(
            "remote_host::first_contact::tests::acceptance_preserves_user_modes_and_symlinks_and_skips_duplicates",
            |dir| {
                let target = dir.join("user_known_hosts");
                std::fs::write(&target, b"existing line").unwrap();
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
                let before = std::fs::metadata(&target).unwrap();
                let known = dir.join("known_hosts");
                std::os::unix::fs::symlink(&target, &known).unwrap();
                let contacts = [
                    first_contact("example-host", dir).unwrap(),
                    first_contact("example-host", dir).unwrap(),
                ];
                let mut expected = b"existing line\n".to_vec();
                expected.extend(&contacts[0].scratch_line);
                let barrier = std::sync::Barrier::new(2);
                std::thread::scope(|scope| {
                    let writers: Vec<_> = contacts
                        .into_iter()
                        .map(|contact| {
                            let barrier = &barrier;
                            let known = &known;
                            scope.spawn(move || {
                                barrier.wait();
                                accept_first_contact(contact, known).unwrap();
                            })
                        })
                        .collect();
                    for writer in writers {
                        writer.join().unwrap();
                    }
                });
                assert_eq!(std::fs::read(&target).unwrap(), expected);
                let after = std::fs::metadata(&target).unwrap();
                assert_eq!(
                    (after.mode(), after.uid(), after.gid(), after.ino()),
                    (before.mode(), before.uid(), before.gid(), before.ino())
                );
                assert!(known.symlink_metadata().unwrap().is_symlink());
            },
        );
    }

    #[test]
    fn competing_distinct_keys_for_one_host_cannot_both_be_accepted() {
        super::super::tests::isolated(
            "remote_host::first_contact::tests::competing_distinct_keys_for_one_host_cannot_both_be_accepted",
            |dir| {
                let one = first_contact("example-host", dir).unwrap();
                let mut two = first_contact("example-host", dir).unwrap();
                two.scratch_line =
                    b"example-host ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n"
                        .to_vec();
                let (kind, fingerprint) = identity(&two.scratch_line).unwrap();
                two.key_type = kind;
                two.sha256 = fingerprint;
                agent::private_fs::write_private(&two.scratch_known_hosts, &two.scratch_line).unwrap();
                let expected = one.scratch_line.clone();
                let known = dir.join("known_hosts");
                accept_first_contact(one, &known).unwrap();
                let error = accept_first_contact(two, &known).unwrap_err();
                assert!(error.to_string().contains("REMOTE HOST IDENTIFICATION HAS CHANGED"));
                assert_eq!(std::fs::read(known).unwrap(), expected);
            },
        );
    }

    #[test]
    fn acceptance_skips_a_hashed_entry_with_the_same_key() {
        super::super::tests::isolated(
            "remote_host::first_contact::tests::acceptance_skips_a_hashed_entry_with_the_same_key",
            |dir| {
                let contact = first_contact("example-host", dir).unwrap();
                let known = dir.join("known_hosts");
                let line = String::from_utf8(contact.scratch_line.clone()).unwrap();
                let hashed = line.replacen(
                    "example-host",
                    "|1|ZXhhbXBsZSBmaXh0dXJlIHNhbHQ=|6zNuN8XFnJ0UzLzpkwXwFpJEFx0=",
                    1,
                );
                std::fs::write(&known, &hashed).unwrap();
                accept_first_contact(contact, &known).unwrap();
                assert_eq!(std::fs::read_to_string(known).unwrap(), hashed);
            },
        );
    }

    #[test]
    fn acceptance_skips_an_exact_final_line_without_a_newline() {
        super::super::tests::isolated(
            "remote_host::first_contact::tests::acceptance_skips_an_exact_final_line_without_a_newline",
            |dir| {
                let contact = first_contact("example-host", dir).unwrap();
                let expected = contact.scratch_line.strip_suffix(b"\n").unwrap().to_vec();
                let known = dir.join("known_hosts");
                std::fs::write(&known, &expected).unwrap();
                accept_first_contact(contact, &known).unwrap();
                assert_eq!(std::fs::read(&known).unwrap(), expected);
            },
        );
    }

    #[test]
    fn acceptance_follows_a_relative_symlink_to_a_missing_target() {
        super::super::tests::isolated(
            "remote_host::first_contact::tests::acceptance_follows_a_relative_symlink_to_a_missing_target",
            |dir| {
                let contact = first_contact("example-host", dir).unwrap();
                let expected = contact.scratch_line.clone();
                let known = dir.join("known_hosts");
                std::os::unix::fs::symlink("user_known_hosts", &known).unwrap();
                accept_first_contact(contact, &known).unwrap();
                assert_eq!(std::fs::read(dir.join("user_known_hosts")).unwrap(), expected);
                assert!(known.symlink_metadata().unwrap().is_symlink());
            },
        );
    }

    #[test]
    fn acceptance_creates_only_the_file_in_an_existing_user_directory() {
        use std::os::unix::fs::PermissionsExt;
        super::super::tests::isolated(
            "remote_host::first_contact::tests::acceptance_creates_only_the_file_in_an_existing_user_directory",
            |dir| {
                let missing = dir.join("missing/known_hosts");
                assert!(accept_first_contact(first_contact("example-host", dir).unwrap(), &missing).is_err());
                assert!(!missing.parent().unwrap().exists());
                let known = dir.join("known_hosts");
                accept_first_contact(first_contact("example-host", dir).unwrap(), &known).unwrap();
                assert_eq!(std::fs::metadata(&known).unwrap().permissions().mode() & 0o777, 0o600);
            },
        );
    }

    #[test]
    fn accepting_refuses_a_scratch_line_changed_after_display() {
        super::super::tests::isolated(
            "remote_host::first_contact::tests::accepting_refuses_a_scratch_line_changed_after_display",
            |dir| {
                let contact = first_contact("example-host", dir).unwrap();
                agent::private_fs::write_private(&contact.scratch_known_hosts, b"different line\n").unwrap();
                let known = dir.join("known_hosts");
                assert!(accept_first_contact(contact, &known).is_err());
                assert!(!known.exists());
            },
        );
    }
}
