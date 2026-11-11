//! A byte-for-byte relay in front of the real Claude CLI that records its stdio for the test harness.
//!
//! It runs `$EITRI_HARNESS_CLAUDE_REAL` with its own arguments, copies stdin to the child and the
//! child's stdout to its own stdout unchanged, and appends every complete line of each direction to
//! `$EITRI_HARNESS_RUN/log/cli-stdin.jsonl` and `cli-stdout.jsonl` as
//! `{"index": n, "time": seconds, "line": "<text>"}`. `index` is one counter shared by both
//! directions, taken as a line completes, so the two logs merge into the order the tap saw; `time` is
//! monotonic seconds since the run's first tap started. The counter and the clock's origin live in
//! `log/cli-tap-clock` and are taken under a lock, so a second tap on the same run (a resumed session)
//! continues both where the last one stopped instead of starting again at zero. A final line without
//! a newline is logged at end of input.
//! The child's stderr goes to `cli-stderr.log`; `cli-env.json` lists the environment names the child
//! receives that a scenario may assert on (`ANTHROPIC_API_KEY` only as `"present"`).
//!
//! SIGINT, SIGTERM and SIGHUP are forwarded to the child, and the tap exits with the child's status
//! (`128 + n` for a child killed by signal `n`). A lone `--version` runs the child with inherited stdio
//! and records nothing. `EITRI_HARNESS_TAP_PRELOAD` is appended to the child's `LD_PRELOAD`.
//! Never shipped.
use serde_json::{json, Map, Value};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::io::{AsRawFd, RawFd},
    os::unix::{fs::OpenOptionsExt, process::ExitStatusExt},
    path::Path,
    process::{self, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering},
        mpsc, Arc, Mutex, PoisonError,
    },
    thread,
    time::{Duration, Instant},
};

/// How long the stdout pump may sit idle after the child exits before the tap stops waiting for it.
const DRAIN_IDLE: Duration = Duration::from_secs(5);

static CHILD_PID: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward_signal(signal: libc::c_int) {
    let pid = CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: kill(2) is async-signal-safe and takes plain integers.
        unsafe { libc::kill(pid, signal) };
    }
}

fn install_forwarders() {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: the handler only reads an atomic and calls kill(2); the zeroed action is filled in
        // field by field before it is installed.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = forward_signal as extern "C" fn(libc::c_int) as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

fn die(message: &str) -> ! {
    eprintln!("eitri-stdio-tap: {message}");
    process::exit(2)
}

/// Whether an environment name is one the child's recorded environment lists.
fn recorded_name(name: &str) -> bool {
    if name == "ANTHROPIC_API_KEY" || name == "ANTHROPIC_BASE_URL" || name == "CLAUDE_CONFIG_DIR" {
        return true;
    }
    if name.starts_with("BASH_") {
        return true;
    }
    name.starts_with("CLAUDE_CODE_") && !["KEY", "TOKEN", "SECRET"].iter().any(|word| name.contains(word))
}

fn child_env(preload: Option<OsString>) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    if let Some(extra) = preload.filter(|p| !p.is_empty()) {
        let existing = env
            .iter()
            .position(|(k, _)| k == "LD_PRELOAD")
            .map(|i| env.remove(i).1)
            .filter(|v| !v.is_empty());
        let mut joined = existing.unwrap_or_default();
        if !joined.is_empty() {
            joined.push(":");
        }
        joined.push(extra);
        env.push(("LD_PRELOAD".into(), joined));
    }
    env
}

fn env_record(env: &[(OsString, OsString)]) -> Value {
    let mut names = BTreeMap::new();
    for (name, value) in env {
        let name = name.to_string_lossy();
        if !recorded_name(&name) {
            continue;
        }
        let shown = if name == "ANTHROPIC_API_KEY" {
            "present".to_string()
        } else {
            value.to_string_lossy().into_owned()
        };
        names.insert(name.into_owned(), Value::String(shown));
    }
    Value::Object(names.into_iter().collect::<Map<_, _>>())
}

fn open_log(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).mode(0o600).open(path)
}

/// CLOCK_MONOTONIC in nanoseconds: one clock for every process on the machine, so taps started at
/// different times measure from the same origin.
fn monotonic_ns() -> u128 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime writes the timespec it is given and reads nothing else.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u128 * 1_000_000_000 + ts.tv_nsec as u128
}

/// The arrival counter and clock origin every tap of one run shares, in a file holding
/// `<next index> <origin in CLOCK_MONOTONIC nanoseconds>`. The mutex orders this tap's two pumps; the
/// file lock orders taps in different processes.
struct Counter {
    file: Mutex<File>,
}

/// Holds an exclusive flock on a descriptor until dropped.
struct Locked(RawFd);

impl Locked {
    fn new(file: &File) -> io::Result<Self> {
        let fd = file.as_raw_fd();
        loop {
            // SAFETY: flock only takes the descriptor, which the caller's File keeps open while the
            // guard lives (the guard never outlives the borrow that created it in this file).
            if unsafe { libc::flock(fd, libc::LOCK_EX) } == 0 {
                return Ok(Self(fd));
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        // SAFETY: as in new(); unlocking the lock this guard took on a still-open descriptor.
        unsafe { libc::flock(self.0, libc::LOCK_UN) };
    }
}

impl Counter {
    /// Opens the run's counter, starting it (index 0, origin now) when no tap has yet.
    fn open(path: &Path) -> io::Result<Self> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        {
            let _lock = Locked::new(&file)?;
            if Self::read(&mut file)?.is_none() {
                Self::write(&mut file, 0, monotonic_ns())?;
            }
        }
        Ok(Self { file: Mutex::new(file) })
    }

    fn read(file: &mut File) -> io::Result<Option<(u64, u128)>> {
        let mut text = String::new();
        file.seek(SeekFrom::Start(0))?;
        file.read_to_string(&mut text)?;
        let mut words = text.split_whitespace();
        Ok(match (words.next().map(str::parse), words.next().map(str::parse)) {
            (Some(Ok(next)), Some(Ok(origin))) => Some((next, origin)),
            _ => None,
        })
    }

    fn write(file: &mut File, next: u64, origin: u128) -> io::Result<()> {
        let text = format!("{next} {origin}\n");
        file.seek(SeekFrom::Start(0))?;
        file.set_len(0)?;
        file.write_all(text.as_bytes())
    }

    /// Takes the next index and appends `line`'s entry to `log` in one write, both under the lock, so
    /// each log's entries are in index order whichever tap or pump wrote them.
    fn record(&self, log: &mut File, line: &[u8]) {
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        let Ok(_lock) = Locked::new(&file) else {
            die("cannot lock the arrival counter")
        };
        let (index, origin) = Self::read(&mut file)
            .ok()
            .flatten()
            .unwrap_or_else(|| die("the arrival counter is unreadable"));
        if Self::write(&mut file, index + 1, origin).is_err() {
            die("cannot advance the arrival counter");
        }
        let elapsed = monotonic_ns().saturating_sub(origin);
        let entry = json!({
            "index": index,
            "time": elapsed as f64 / 1e9,
            "line": String::from_utf8_lossy(line),
        });
        let mut bytes = entry.to_string().into_bytes();
        bytes.push(b'\n');
        let _ = log.write_all(&bytes);
    }
}

/// What a pump is doing right now, so the exit path can tell a pump that is stuck reading a pipe a
/// grandchild holds open from one that is still delivering output to a slow consumer.
#[derive(Default)]
struct Activity {
    writing: AtomicBool,
    forwarded: AtomicUsize,
    write_gate: Option<std::path::PathBuf>,
}

fn wait_for_gate(path: &Path) {
    wait_for_gate_with_pause(path, thread::sleep);
}

fn wait_for_gate_with_pause(path: &Path, mut pause: impl FnMut(Duration)) {
    let end = Instant::now() + Duration::from_secs(60);
    for _ in 0..12_000 {
        if path.exists() {
            return;
        }
        assert!(Instant::now() < end, "tap gate was never released: {}", path.display());
        pause(Duration::from_millis(5));
    }
    panic!("tap gate exhausted its polling budget: {}", path.display());
}

/// Copies `from` to `to` unchanged and logs each line (and a final unterminated one) with the shared index.
/// A chunk's complete lines are logged before the chunk is forwarded: the receiver may answer at once,
/// and the answer must not take an earlier index than the line that caused it.
fn pump(mut from: impl Read, mut to: impl Write, mut log: File, counter: &Counter, activity: &Activity) {
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    let mut sink_open = true;
    let record = |line: &[u8], log: &mut File| counter.record(log, line);
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        pending.extend_from_slice(&buf[..n]);
        while let Some(end) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            record(&line[..end], &mut log);
        }
        if sink_open {
            activity.writing.store(true, Ordering::SeqCst);
            if let Some(gate) = &activity.write_gate {
                fs::write(gate.with_extension("writing"), b"").unwrap();
                wait_for_gate(&gate.with_extension("write"));
            }
            if to.write_all(&buf[..n]).and_then(|_| to.flush()).is_err() {
                sink_open = false;
            }
            activity.forwarded.fetch_add(n, Ordering::SeqCst);
            activity.writing.store(false, Ordering::SeqCst);
        }
        if !sink_open {
            break;
        }
    }
    if !pending.is_empty() {
        record(&pending, &mut log);
    }
}

fn exit_code(status: process::ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let real = std::env::var_os("EITRI_HARNESS_CLAUDE_REAL")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| die("EITRI_HARNESS_CLAUDE_REAL is not set"));

    if args.len() == 1 && args[0] == OsStr::new("--version") {
        match Command::new(&real).args(&args).status() {
            Ok(status) => process::exit(exit_code(status)),
            Err(e) => die(&format!("cannot run {}: {e}", real.to_string_lossy())),
        }
    }

    let run = std::env::var_os("EITRI_HARNESS_RUN")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| die("EITRI_HARNESS_RUN is not set"));
    let log_dir = Path::new(&run).join("log");
    if let Err(e) = fs::create_dir_all(&log_dir) {
        die(&format!("cannot create {}: {e}", log_dir.display()));
    }
    let open = |name: &str| open_log(&log_dir.join(name)).unwrap_or_else(|e| die(&format!("cannot open {name}: {e}")));
    let (stdin_log, stdout_log, stderr_log) = (
        open("cli-stdin.jsonl"),
        open("cli-stdout.jsonl"),
        open("cli-stderr.log"),
    );

    let env = child_env(std::env::var_os("EITRI_HARNESS_TAP_PRELOAD"));
    let record = serde_json::to_vec_pretty(&env_record(&env)).expect("env record serializes");
    if let Err(e) = fs::write(log_dir.join("cli-env.json"), record) {
        die(&format!("cannot write cli-env.json: {e}"));
    }

    let mut child = Command::new(&real)
        .args(&args)
        .env_clear()
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_log))
        .spawn()
        .unwrap_or_else(|e| die(&format!("cannot run {}: {e}", real.to_string_lossy())));
    CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
    install_forwarders();

    let counter = Arc::new(
        Counter::open(&log_dir.join("cli-tap-clock"))
            .unwrap_or_else(|e| die(&format!("cannot open cli-tap-clock: {e}"))),
    );
    let child_stdin = child.stdin.take().expect("piped stdin");
    let child_stdout = child.stdout.take().expect("piped stdout");
    // The stdin pump may sit in a read the exit does not interrupt, so it is detached; the stdout
    // pump is waited for, so the last output is logged before the tap exits.
    let c = counter.clone();
    let stdin_activity = Activity::default();
    thread::spawn(move || pump(io::stdin(), child_stdin, stdin_log, &c, &stdin_activity));
    let (done_tx, done_rx) = mpsc::channel();
    let c = counter.clone();
    let gate = std::env::var_os("EITRI_TAP_DRAIN_GATE").map(std::path::PathBuf::from);
    let out_activity = Arc::new(Activity {
        write_gate: gate.clone(),
        ..Activity::default()
    });
    let a = out_activity.clone();
    thread::spawn(move || {
        pump(child_stdout, io::stdout(), stdout_log, &c, &a);
        let _ = done_tx.send(());
    });

    let status = loop {
        match child.wait() {
            Ok(status) => break status,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => die(&format!("wait failed: {e}")),
        }
    };
    // A grandchild that kept the stdout pipe open must not hold the tap past the child's exit, but a
    // consumer that is merely slow to take the last output must not lose it: the wait ends only when
    // the pump is idle in a read that delivered nothing for the whole period.
    let mut waits = 0;
    drain_output(&out_activity, || {
        waits += 1;
        if let Some(gate) = &gate {
            if waits == 1 {
                wait_for_gate(&gate.with_extension("writing"));
                return Err(mpsc::RecvTimeoutError::Timeout);
            }
            fs::write(gate.with_extension("continued"), b"").unwrap();
            wait_for_gate(&gate.with_extension("drain"));
        }
        done_rx.recv_timeout(DRAIN_IDLE)
    });
    if let Some(gate) = &gate {
        fs::write(gate.with_extension("finished"), b"").unwrap();
    }
    process::exit(exit_code(status));
}

fn drain_output(activity: &Activity, mut wait: impl FnMut() -> Result<(), mpsc::RecvTimeoutError>) {
    loop {
        let before = activity.forwarded.load(Ordering::SeqCst);
        match wait() {
            Err(mpsc::RecvTimeoutError::Timeout)
                if activity.writing.load(Ordering::SeqCst) || activity.forwarded.load(Ordering::SeqCst) != before => {}
            _ => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreleased_gate_pauses_before_checking_again() {
        let gate = agent::state_dirs::test_workspace_dir("tap-gate").join("release");
        let mut pauses = 0;
        wait_for_gate_with_pause(&gate, |duration| {
            assert_eq!(duration, Duration::from_millis(5));
            pauses += 1;
            assert_eq!(pauses, 1);
            fs::write(&gate, b"").unwrap();
        });
        assert_eq!(
            pauses, 1,
            "the gate cannot finish before its pause callback releases it"
        );
    }

    #[test]
    fn a_timeout_keeps_output_that_is_still_being_written_or_forwarded() {
        for writing in [false, true] {
            let activity = Activity::default();
            activity.writing.store(writing, Ordering::SeqCst);
            let mut calls = 0;
            drain_output(&activity, || {
                calls += 1;
                if calls == 1 {
                    if !writing {
                        activity.forwarded.fetch_add(16, Ordering::SeqCst);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout)
                } else {
                    Ok(())
                }
            });
            assert_eq!(calls, 2, "a timeout while output moves must wait again");
        }
    }

    #[test]
    fn an_idle_timeout_or_a_finished_pump_ends_the_drain() {
        for result in [
            Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout),
            Err(mpsc::RecvTimeoutError::Disconnected),
        ] {
            let mut calls = 0;
            drain_output(&Activity::default(), || {
                calls += 1;
                assert_eq!(calls, 1);
                result
            });
            assert_eq!(calls, 1);
        }
    }
}
