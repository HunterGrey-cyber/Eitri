use std::collections::BTreeMap;
use std::ffi::{CStr, OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::io::AsyncReadExt;

pub type Environment = BTreeMap<OsString, OsString>;
const CAPTURE_BUDGET: Duration = Duration::from_secs(5);
const MAX_ENV_BYTES: u64 = 4 * 1024 * 1024;

pub struct Captured {
    pub env: Environment,
    pub fallback: Option<String>,
}

const BEGIN: &[u8] = b"\0EITRI_ENV_BEGIN\0";
const END: &[u8] = b"\0EITRI_ENV_END\0";

/// Account identity comes from passwd, never the SSH channel's environment.
pub fn minimal_environment() -> io::Result<Environment> {
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0u8; 64 * 1024];
    let code = unsafe {
        libc::getpwuid_r(
            agent::private_fs::euid(),
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if code != 0 || result.is_null() {
        return Err(io::Error::other("cannot resolve daemon uid in passwd"));
    }
    let entry = unsafe { entry.assume_init() };
    let value = |ptr| unsafe { OsString::from_vec(CStr::from_ptr(ptr).to_bytes().to_vec()) };
    Ok(Environment::from([
        ("HOME".into(), value(entry.pw_dir)),
        ("USER".into(), value(entry.pw_name)),
        ("LOGNAME".into(), value(entry.pw_name)),
        ("SHELL".into(), value(entry.pw_shell)),
        ("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()),
        ("LANG".into(), "C.UTF-8".into()),
    ]))
}

#[derive(Default)]
struct PasswdLookup {
    pending: tokio::sync::Mutex<Option<tokio::task::JoinHandle<io::Result<Environment>>>>,
}

pub async fn capture() -> io::Result<Captured> {
    static LOOKUP: OnceLock<PasswdLookup> = OnceLock::new();
    LOOKUP
        .get_or_init(PasswdLookup::default)
        .capture_lookup(minimal_environment, CAPTURE_BUDGET)
        .await
}

impl PasswdLookup {
    async fn capture_lookup(
        &self,
        lookup: impl FnOnce() -> io::Result<Environment> + Send + 'static,
        budget: Duration,
    ) -> io::Result<Captured> {
        let deadline = tokio::time::Instant::now() + budget;
        // NSS can outlive the deadline. Retain its handle so retries wait for the same worker.
        let baseline = tokio::time::timeout_at(deadline, async {
            let mut pending = self.pending.lock().await;
            let worker = pending.get_or_insert_with(|| tokio::task::spawn_blocking(lookup));
            let result = (&mut *worker).await;
            pending.take();
            result.map_err(io::Error::other)?
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "passwd lookup timed out"))??;
        Ok(capture_with_budget(
            baseline,
            deadline.saturating_duration_since(tokio::time::Instant::now()),
        )
        .await)
    }
}

/// The baseline is also the fallback; a failed profile cannot import the daemon's environment.
#[cfg(test)]
pub async fn capture_with(baseline: Environment) -> Captured {
    capture_with_budget(baseline, CAPTURE_BUDGET).await
}

async fn capture_with_budget(baseline: Environment, budget: Duration) -> Captured {
    match capture_from(&baseline, budget).await {
        Ok(env) => Captured { env, fallback: None },
        Err(error) => Captured {
            env: baseline,
            fallback: Some(format!(
                "login environment unavailable; using minimal environment: {error}"
            )),
        },
    }
}

async fn capture_from(inherited: &Environment, budget: Duration) -> io::Result<Environment> {
    let shell = inherited
        .get(OsStr::new("SHELL"))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| io::Error::other("SHELL is unset"))?;
    let mut command = tokio::process::Command::new(shell);
    command
        .args([
            "-lc",
            "printf '\\000EITRI_ENV_BEGIN\\000'; /usr/bin/env -0; printf '\\000EITRI_ENV_END\\000'",
        ])
        .env_clear()
        .envs(inherited)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // Kill the whole capture group on timeout, including descendants holding stdout open.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let mut group = CaptureGroup(Some(child.id().expect("new child has a pid")));
    let mut stdout = child.stdout.take().expect("piped stdout").take(MAX_ENV_BYTES + 1);
    let result = tokio::time::timeout(budget, async {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await?;
        if bytes.len() as u64 > MAX_ENV_BYTES {
            return Err(io::Error::other("login environment exceeds its byte cap"));
        }
        let status = child.wait().await?;
        group.0 = None;
        if !status.success() {
            return Err(io::Error::other("login shell failed"));
        }
        parse(&bytes)
    })
    .await;
    match result {
        Ok(Ok(env)) => Ok(env),
        error => {
            drop(group);
            let _ = child.start_kill();
            match error {
                Ok(Err(error)) => Err(error),
                Err(_) => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "login environment capture timed out",
                )),
                Ok(Ok(_)) => unreachable!(),
            }
        }
    }
}

struct CaptureGroup(Option<u32>);
impl Drop for CaptureGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // The capture leader is still owned and unreaped, so the group cannot be reused.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}

fn parse(bytes: &[u8]) -> io::Result<Environment> {
    let start = bytes
        .windows(BEGIN.len())
        .position(|w| w == BEGIN)
        .ok_or_else(|| io::Error::other("login environment start marker missing"))?
        + BEGIN.len();
    let end = bytes[start..]
        .windows(END.len())
        .position(|w| w == END)
        .ok_or_else(|| io::Error::other("login environment end marker missing"))?
        + start;
    let bytes = &bytes[start..end];
    if bytes.is_empty() || bytes.last() != Some(&0) {
        return Err(io::Error::other(
            "login shell did not return a NUL-separated environment",
        ));
    }
    let mut env = Environment::new();
    for entry in bytes[..bytes.len() - 1].split(|b| *b == 0) {
        let equals = entry
            .iter()
            .position(|b| *b == b'=')
            .filter(|p| *p > 0)
            .ok_or_else(|| io::Error::other("invalid login environment entry"))?;
        if entry[..equals].iter().any(u8::is_ascii_control) {
            return Err(io::Error::other("invalid login environment name"));
        }
        env.insert(
            OsString::from_vec(entry[..equals].to_vec()),
            OsString::from_vec(entry[equals + 1..].to_vec()),
        );
    }
    Ok(env)
}

/// Relative PATH entries would search the helper's working directory during CLI discovery.
pub fn absolute_path(env: &mut Environment) {
    if let Some(path) = env.get(OsStr::new("PATH")) {
        let paths: Vec<_> = std::env::split_paths(path).filter(|p| p.is_absolute()).collect();
        if paths.is_empty() {
            // An empty PATH still denotes the current directory to CLI lookup. Absence uses its
            // absolute system default instead.
            env.remove(OsStr::new("PATH"));
        } else {
            // Joining components just split from PATH cannot introduce a separator into a component.
            env.insert("PATH".into(), std::env::join_paths(paths).expect("PATH components"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn passwd_lookup_is_off_worker_and_inside_the_capture_deadline() {
        let (release, held) = std::sync::mpsc::channel();
        let worker = std::thread::current().id();
        let lookup = PasswdLookup::default();
        let result = lookup
            .capture_lookup(
                move || {
                    assert_ne!(std::thread::current().id(), worker);
                    held.recv().unwrap();
                    minimal_environment()
                },
                Duration::from_millis(20),
            )
            .await;
        // Release even a timed-out blocking lookup so runtime shutdown never waits on the fixture.
        release.send(()).unwrap();
        assert_eq!(result.err().unwrap().kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn timed_out_retries_share_one_unfinished_passwd_worker() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let lookup = PasswdLookup::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let (release, held) = std::sync::mpsc::channel();
        let first_calls = calls.clone();
        let first = lookup
            .capture_lookup(
                move || {
                    first_calls.fetch_add(1, Ordering::SeqCst);
                    held.recv().unwrap();
                    Ok(Environment::from([("SHELL".into(), "/missing-login-shell".into())]))
                },
                Duration::from_millis(20),
            )
            .await;
        assert_eq!(first.err().unwrap().kind(), io::ErrorKind::TimedOut);
        for _ in 0..3 {
            let retry_calls = calls.clone();
            let retry = lookup
                .capture_lookup(
                    move || {
                        retry_calls.fetch_add(1, Ordering::SeqCst);
                        Err(io::Error::other("must reuse the unfinished lookup"))
                    },
                    Duration::from_millis(20),
                )
                .await;
            assert_eq!(retry.err().unwrap().kind(), io::ErrorKind::TimedOut);
        }
        // Release before assertions so even a regression cannot strand runtime shutdown.
        release.send(()).unwrap();
        let captured = lookup
            .capture_lookup(|| panic!("must collect the original worker"), CAPTURE_BUDGET)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(captured.env[OsStr::new("SHELL")], "/missing-login-shell");
        assert!(captured.fallback.is_some());
        assert!(lookup.pending.lock().await.is_none());
        // Once the worker completes, a later attempt may run a fresh lookup.
        let error = lookup
            .capture_lookup(|| Err(io::Error::other("new lookup")), CAPTURE_BUDGET)
            .await
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "new lookup");
    }

    #[tokio::test]
    async fn passwd_lookup_errors_are_returned_without_using_daemon_environment() {
        let lookup = PasswdLookup::default();
        let error = lookup
            .capture_lookup(|| Err(io::Error::other("passwd unavailable")), CAPTURE_BUDGET)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("passwd unavailable"));
    }

    #[test]
    fn capture_parsing_preserves_values_and_refuses_unframed_output() {
        let env = parse(b"\0EITRI_ENV_BEGIN\0PATH=/bin\0BYTES=\xff\0EMPTY=\0\0EITRI_ENV_END\0").unwrap();
        assert_eq!(env[OsStr::new("BYTES")], OsString::from_vec(vec![255]));
        assert_eq!(env[OsStr::new("EMPTY")], "");
        for invalid in [b"banner\nPATH=/bin\0".as_slice(), b"PATH=/bin", b"\0", b"=bad\0"] {
            assert!(parse(invalid).is_err());
        }
    }

    #[test]
    fn banners_outside_environment_markers_are_ignored() {
        let env = parse(b"Welcome\n\0EITRI_ENV_BEGIN\0PATH=/bin\0\0EITRI_ENV_END\0Bye\n").unwrap();
        assert_eq!(env[OsStr::new("PATH")], "/bin");
    }

    #[test]
    fn discovery_path_has_only_absolute_entries() {
        let mut env = Environment::from([("PATH".into(), ":.:relative:/usr/bin:/bin".into())]);
        absolute_path(&mut env);
        assert_eq!(env[OsStr::new("PATH")], "/usr/bin:/bin");
        env.insert("PATH".into(), ":.:relative".into());
        absolute_path(&mut env);
        assert!(!env.contains_key(OsStr::new("PATH")));
    }

    #[tokio::test]
    async fn failed_shell_capture_returns_an_error_for_the_fallback() {
        assert!(capture_from(&Environment::new(), CAPTURE_BUDGET).await.is_err());
        let env = Environment::from([("SHELL".into(), "/bin/false".into())]);
        assert!(capture_from(&env, CAPTURE_BUDGET)
            .await
            .unwrap_err()
            .to_string()
            .contains("login shell failed"));
    }
    #[tokio::test]
    async fn failed_capture_uses_only_the_supplied_minimal_baseline() {
        let mut baseline = minimal_environment().unwrap();
        baseline.insert("SHELL".into(), "/bin/false".into());
        let captured = capture_with(baseline.clone()).await;
        assert_eq!(captured.env, baseline);
        assert!(captured.fallback.unwrap().contains("using minimal environment"));
        for key in ["SSH_AUTH_SOCK", "SSH_CONNECTION", "EITRI_DAEMON_ONLY"] {
            assert!(!captured.env.contains_key(OsStr::new(key)));
        }
    }

    #[test]
    fn passwd_baseline_has_only_the_identity_and_fixed_defaults() {
        let env = minimal_environment().unwrap();
        assert_eq!(env.len(), 6);
        assert_eq!(env[OsStr::new("USER")], env[OsStr::new("LOGNAME")]);
        assert_eq!(env[OsStr::new("PATH")], "/usr/local/bin:/usr/bin:/bin");
        assert_eq!(env[OsStr::new("LANG")], "C.UTF-8");
        assert!(std::path::Path::new(&env[OsStr::new("HOME")]).is_absolute());
    }
    #[tokio::test]
    async fn capture_timeout_falls_back_to_the_same_minimal_environment() {
        let root = std::env::temp_dir().join(format!("login-timeout-{}", std::process::id()));
        agent::private_fs::create_private_dir_all(&root, &root).unwrap();
        let shell = root.join("shell");
        assert!(std::process::Command::new("/bin/sh")
            .args(["-c", "printf '%s' \"$2\" >\"$1\"; chmod 700 \"$1\"", "writer"])
            .arg(&shell)
            .arg("#!/bin/sh\nexec /bin/sleep 30\n")
            .status()
            .unwrap()
            .success());
        let mut baseline = minimal_environment().unwrap();
        baseline.insert("SHELL".into(), shell.into_os_string());
        let captured = capture_with(baseline.clone()).await;
        assert_eq!(captured.env, baseline);
        assert!(captured.fallback.unwrap().contains("timed out"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
