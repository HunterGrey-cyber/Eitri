//! Worker-side remote host connection and resume operations.

pub mod cli;
pub mod cli_sync;
pub mod first_connect;
pub mod first_contact;
pub mod folders;
pub mod host_list;
pub mod install;
pub mod manifest;
pub mod probe_step;
pub mod provenance;
pub mod resume;
pub mod skew;
pub mod steps;
pub mod window;

pub use first_contact::{accept_first_contact, first_contact, FirstContact};
pub use resume::{on_resume, ResumeAction};

#[cfg(test)]
pub(crate) mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::Command;

    /// A channel that waits before attaching, while a separate worker tries replacing its master.
    pub(crate) struct DelayedAttachment {
        pub local: agent::ssh::LocalExec,
        pub target: &'static str,
        pub root: std::path::PathBuf,
        pub held: std::sync::Arc<std::sync::atomic::AtomicBool>,
        pub refused: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    struct KeyHold(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for KeyHold {
        fn drop(&mut self) {
            self.0.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl agent::ssh::ChannelLauncher for DelayedAttachment {
        fn spawn(&self, tail: &[std::ffi::OsString]) -> std::io::Result<std::process::Child> {
            self.local.spawn(tail)
        }

        fn hold_host_key(&self, key: &str) -> std::io::Result<Box<dyn Send>> {
            assert_eq!(key, "SHA256:example");
            assert!(!self.held.swap(true, std::sync::atomic::Ordering::SeqCst));
            Ok(Box::new(KeyHold(self.held.clone())))
        }

        fn spawn_step(&self, literal: &'static str, args: &[&str]) -> std::io::Result<std::process::Child> {
            use std::process::Stdio;
            if literal != self.target {
                return Command::new("sh")
                    .args(["-c", literal, "sh"])
                    .args(args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn();
            }
            let waiting = self.root.join("unattached");
            let release = self.root.join("attach");
            let child = Command::new("sh")
                .args(["-c", r#"touch "$1"; n=0; while ! test -e "$2"; do n=$((n+1)); test "$n" -lt 6000 || exit 1; sleep 0.01; done; shift 2; exec sh -c "$@""#, "sh"])
                .arg(&waiting).arg(&release).arg(literal).arg("sh").args(args)
                .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
            let held = self.held.clone();
            let refused = self.refused.clone();
            super::install::before_attachment::HOOK.with(|cell| {
                *cell.borrow_mut() = Some(Box::new(move || {
                    std::thread::spawn(move || {
                        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                        while !waiting.exists() {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "child never reached its attach gate"
                            );
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        // Replacement uses the same exclusion as hold_host_key. Try while the child is unattached.
                        let blocked = held
                            .compare_exchange(
                                false,
                                true,
                                std::sync::atomic::Ordering::SeqCst,
                                std::sync::atomic::Ordering::SeqCst,
                            )
                            .is_err();
                        refused.store(blocked, std::sync::atomic::Ordering::SeqCst);
                        if !blocked {
                            held.store(false, std::sync::atomic::Ordering::SeqCst);
                        }
                        std::fs::write(release, b"attach").unwrap();
                    })
                    .join()
                    .unwrap();
                }));
            });
            Ok(child)
        }
    }

    pub fn isolated(name: &str, body: impl FnOnce(&Path)) {
        if std::env::var_os("EITRI_REMOTE_HOST_TEST").as_deref() == Some(std::ffi::OsStr::new(name)) {
            body(Path::new(&std::env::var_os("EITRI_SSH_FIXTURE").unwrap()));
            return;
        }
        // Leave room for OpenSSH's temporary socket suffix even under a long driver TMPDIR.
        let dir = loop {
            use std::os::unix::fs::DirBuilderExt;
            let path = std::env::temp_dir().join(&uuid::Uuid::new_v4().simple().to_string()[..6]);
            match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => break path,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create SSH scratch: {error}"),
            }
        };
        struct Clean(std::path::PathBuf);
        impl Drop for Clean {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        agent::private_fs::create_private_dir_all(&dir, &dir).unwrap();
        let _clean = Clean(dir.clone());
        let bin = dir.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let ssh = bin.join("ssh");
        std::fs::write(&ssh, r##"#!/bin/sh
set -eu
state=$EITRI_SSH_FIXTURE
printf '%s\n' BEGIN "$@" END >> "$state/argv"
if [ "$1" = -V ]; then echo "${EITRI_SSH_VERSION:-OpenSSH_10.0p2, test}" >&2; exit 0; fi
for arg do
    case "$arg" in ForkAfterAuthentication=*)
        case "${EITRI_SSH_VERSION:-}" in OpenSSH_8.[56]*) echo 'Bad configuration option: forkafterauthentication' >&2; exit 255;; esac ;;
    esac
done
op=
previous=
scratch=
contact=
accept_new=
observer=
for arg do
    case "$arg" in
        UserKnownHostsFile=*) scratch=${arg#UserKnownHostsFile=} ;;
        PreferredAuthentications=none) contact=yes ;;
        StrictHostKeyChecking=accept-new) accept_new=yes ;;
        KnownHostsCommand=*) observer=${arg#*known-hosts-probe }; observer=${observer%% %H*}; observer=${observer#\"}; observer=${observer%\"} ;;
    esac
    if [ "$previous" = -O ]; then op=$arg; fi
    previous=$arg
done
case "$op" in
    check)
        if [ -f "$state/slow-check" ]; then rm "$state/slow-check"; exec sleep 60; fi
        test -f "$state/live"; exit $? ;;
    exit) rm -f "$state/live"; echo exit >> "$state/exits"; exit 0 ;;
esac
if [ "$previous" = "sh -c 'true'" ]; then
    echo probe >> "$state/probes"
    if [ -f "$state/hung" ]; then exec sleep 60; fi
    test -f "$state/live"; exit $?
fi
if [ -n "$scratch" ]; then
    scratch=${scratch#\"}
    scratch=${scratch%\"}
fi
server_line="example-host ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
if [ -f "$state/server-line" ]; then server_line=$(cat "$state/server-line"); fi
if [ -n "$accept_new" ]; then
    printf "%s\n" "$server_line" > "$scratch"
    echo 'Permission denied (publickey).' >&2
    exit 255
fi
if [ -n "$contact" ]; then
    if [ -f "$state/slow-preflight" ] && [ -z "$scratch" ]; then sleep "$(cat "$state/slow-preflight")"; fi
    known=${scratch:-$state/known_hosts}
    match=
    if [ -f "$known" ]; then match=$(awk '$1 == "example-host" || substr($1,1,3) == "|1|" {print $2 " " $3}' "$known"); fi
    server_key=${server_line#* }
    if [ -f "$state/changed" ] || [ -f "$state/different-type" ] || { [ -n "$match" ] && [ "$match" != "$server_key" ]; }; then
        echo '@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@' >&2
        echo '@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @' >&2
        echo '@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@' >&2
        if [ -f "$state/different-type" ]; then echo 'Offending RSA key in /keys:3' >&2; fi
        echo 'Host key verification failed.' >&2
    elif [ -n "$match" ]; then
        fingerprint="SHA256:kmYcvdi2GkPeWxB6XLjrZB8JHsy2Hm8luHMFp9GMvqk"
        if [ -f "$state/server-fingerprint" ]; then fingerprint=$(cat "$state/server-fingerprint"); fi
        if [ -n "$observer" ]; then printf '["example-host","ssh-ed25519","%s"]' "$fingerprint" > "$observer"; chmod 600 "$observer"; fi
        echo 'Permission denied (publickey).' >&2
    else
        echo 'No ED25519 host key is known for example-host and you have requested strict checking.' >&2
        echo 'Host key verification failed.' >&2
    fi
    exit 255
fi
echo start >> "$state/starts"
if [ -f "$state/master-fingerprint" ] && [ -n "$observer" ]; then
    printf '["example-host","ssh-ed25519","%s"]' "$(cat "$state/master-fingerprint")" > "$observer"; chmod 600 "$observer"
fi
touch "$state/live"
"##).unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        // The first run of a new script can take seconds on macOS under load (the system checks it first), and
        // `check_openssh` gives `ssh -V` two: run it once here, against a scratch state, so the child's runs are fast.
        let warm = dir.join("warm");
        std::fs::create_dir(&warm).unwrap();
        let _ = Command::new(&ssh).arg("-V").env("EITRI_SSH_FIXTURE", &warm).output();
        std::fs::remove_dir_all(&warm).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env("EITRI_REMOTE_HOST_TEST", name)
            .env("EITRI_SSH_FIXTURE", &dir)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "child ran no test"
        );
    }
}
