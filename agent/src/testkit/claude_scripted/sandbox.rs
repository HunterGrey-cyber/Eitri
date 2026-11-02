//! The check every Claude harness test runs first: is this process inside the sandbox `run.py` builds?
//!
//! A harness test drives the real Claude CLI against the scripted model. Outside its sandbox that CLI would
//! see the machine's real home directory, its network and its managed settings, so the test does not run
//! there: [`check`] prints [`SKIPPED`] and returns `false`, and the test returns at once. The runner counts
//! that line as a failure, so it cannot be fooled by a skip, while a blanket `cargo test -- --ignored`
//! anywhere else skips these tests instead of reaching a real account.
//!
//! The sandbox is recognised by what it guarantees, not by a marker it sets: the run directory's form, a
//! network namespace with nothing but loopback, no credential or editor configuration of the real user in
//! view, no managed settings, no `CLAUDE.md`, `.claude/` or `.git` above the session's project, no other run
//! or host daemon's socket in reach, and no remote file system in view.
use std::{
    ffi::{CStr, OsStr},
    io,
    net::{SocketAddr, TcpStream},
    os::unix::{ffi::OsStrExt, fs::FileTypeExt, fs::MetadataExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// The line a harness test prints when it is not in the sandbox; the runner fails a run that printed it.
pub const SKIPPED: &str = "SKIPPED: not in the claude harness sandbox";

/// The line a passing self-check prints, followed by the state of `/etc/claude-code`.
pub const PASSED: &str = "sandbox self-check passed: /etc/claude-code";

/// Where every run directory lives: `<RUN_BASE>/<8 hex>/<scenario>`.
pub const RUN_BASE: &str = "/mnt/eitri-runs/ch";

/// Socket paths under the run directory must fit `sun_path` with room to spare.
pub const MAX_RUN_BYTES: usize = 48;

/// An address in TEST-NET-1: never routed, so only a missing route can make the connect fail at once.
const UNROUTED: &str = "192.0.2.1:9";

/// Paths under the real user's passwd home that must not be visible: credentials, accounts and the editor
/// and Eitri configuration a session could pick up.
const HOME_PATHS: &[&str] = &[".claude", ".claude.json", ".ssh", ".config/eitri", ".config/nvim"];

/// File system types that reach another machine or mount on demand. A developer's synced trees and shares are
/// often mounted that way; none may be in view, and an automount must not be triggered from inside.
const REMOTE_FS: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smbfs",
    "autofs",
    "9p",
    "ceph",
    "glusterfs",
    "afs",
    "davfs",
    "fuse.sshfs",
    "fuse.rclone",
];

/// The CLI's managed-settings directory as the sandbox shows it: missing on the host, or hidden by an empty
/// tmpfs. Anything in it could shape a run without a trace in the scenario.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagedSettings {
    Absent,
    Empty,
}

impl ManagedSettings {
    pub fn word(self) -> &'static str {
        match self {
            ManagedSettings::Absent => "absent",
            ManagedSettings::Empty => "empty",
        }
    }
}

/// What the check reads, so that it can be run against a fake root in tests.
pub struct Inputs<'a> {
    /// The file system root the absolute paths below are read under (`/` in a real run).
    pub root: &'a Path,
    /// `EITRI_HARNESS_RUN`.
    pub run: Option<&'a OsStr>,
    /// `HOME`.
    pub home: Option<&'a OsStr>,
    /// The home directory the password database gives the current uid.
    pub passwd_home: Option<&'a Path>,
    /// Whether a directory lies on a tmpfs (`statfs`), answered for a fake root in tests.
    pub on_tmpfs: &'a dyn Fn(&Path) -> io::Result<bool>,
}

/// Runs the self-check in this process. Prints [`PASSED`] with the managed-settings state and returns `true`
/// inside the sandbox; prints [`SKIPPED`] with the first failed condition and returns `false` elsewhere. The
/// line starts with a newline because the test harness has already printed `test <name> ... ` without one.
pub fn check() -> bool {
    let run = std::env::var_os("EITRI_HARNESS_RUN");
    let home = std::env::var_os("HOME");
    // SAFETY: `getuid` has no preconditions and cannot fail.
    let passwd_home = passwd_home_of(unsafe { libc::getuid() });
    let inputs = Inputs {
        root: Path::new("/"),
        run: run.as_deref(),
        home: home.as_deref(),
        passwd_home: passwd_home.as_deref(),
        on_tmpfs: &on_tmpfs,
    };
    match evaluate(&inputs, connect_unrouted) {
        Ok(managed) => {
            println!("\n{PASSED} {}", managed.word());
            true
        }
        Err(why) => {
            println!("\n{SKIPPED} ({why})");
            false
        }
    }
}

/// Moves what the earlier runs of this sandbox left aside, under `<dir>-<name>` (they stay on disk for anyone
/// looking at a failure), so the next run starts from the state a run starts from: no log, no endpoint record, no
/// CLI configuration or transcript, no task output of the CLI, no open gate. A scenario that runs more than once
/// in one sandbox (a variant beside its base) calls this before each run after the first.
pub fn set_earlier_run_aside(run: &Path, name: &str) {
    // The runner reads `log/test.out` for the whole sandbox run, so the log directory stays and its files go.
    let log = run.join("log");
    let log_aside = run.join(format!("log-{name}"));
    std::fs::create_dir_all(&log_aside).unwrap_or_else(|e| panic!("making log-{name}: {e}"));
    for entry in std::fs::read_dir(&log).into_iter().flatten().flatten() {
        if entry.file_name() != "test.out" {
            std::fs::rename(entry.path(), log_aside.join(entry.file_name()))
                .unwrap_or_else(|e| panic!("setting a log aside: {e}"));
        }
    }
    for dir in ["standin", "claude", "project", "tmp", "r"] {
        let from = run.join(dir);
        if from.exists() {
            std::fs::rename(&from, run.join(format!("{dir}-{name}")))
                .unwrap_or_else(|e| panic!("setting {dir} aside: {e}"));
        }
        std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
            .recursive(true)
            .create(&from)
            .unwrap_or_else(|e| panic!("recreating {dir}: {e}"));
    }
    for entry in std::fs::read_dir(run.join("gates")).into_iter().flatten().flatten() {
        if entry.path().extension().is_some_and(|e| e == "open") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Waits (fifteen seconds at most) for the process `pid` to be gone, so the next run's sidecar and CLI start
/// alone.
pub fn wait_gone(pid: u32) {
    let end = Instant::now() + Duration::from_secs(15);
    while Path::new(&format!("/proc/{pid}")).exists() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Every condition of the self-check, cheapest first, so that outside the sandbox the missing run variable
/// ends it before any connect is tried. `network` must succeed only when a connect outside loopback fails
/// at once for want of a route.
pub fn evaluate(inputs: &Inputs<'_>, network: impl FnOnce() -> Result<(), String>) -> Result<ManagedSettings, String> {
    let run = run_dir(inputs.run)?;
    home_under(inputs.home, &run)?;
    let passwd_home = inputs
        .passwd_home
        .ok_or_else(|| "the password database has no home for this uid".to_string())?;
    real_home_hidden(inputs.root, passwd_home)?;
    let managed = managed_settings(inputs.root)?;
    no_project_markers_above(inputs.root, &run)?;
    other_runs_hidden(inputs.root, &run, inputs.on_tmpfs)?;
    no_host_sockets(inputs.root)?;
    no_remote_mounts(inputs.root)?;
    network()?;
    Ok(managed)
}

/// `EITRI_HARNESS_RUN` must be `/mnt/eitri-runs/ch/<8 lowercase hex>/<scenario>`, at most 48 bytes.
fn run_dir(run: Option<&OsStr>) -> Result<PathBuf, String> {
    let run = run.ok_or_else(|| "EITRI_HARNESS_RUN is not set".to_string())?;
    let text = run
        .to_str()
        .ok_or_else(|| "EITRI_HARNESS_RUN is not UTF-8".to_string())?;
    if text.len() > MAX_RUN_BYTES {
        return Err(format!("EITRI_HARNESS_RUN is longer than {MAX_RUN_BYTES} bytes"));
    }
    let rest = text
        .strip_prefix(RUN_BASE)
        .and_then(|r| r.strip_prefix('/'))
        .ok_or_else(|| format!("EITRI_HARNESS_RUN is not under {RUN_BASE}"))?;
    let mut parts = rest.split('/');
    let (Some(id), Some(scenario), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err("EITRI_HARNESS_RUN is not <run id>/<scenario>".into());
    };
    let id_ok = id.len() == 8 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    let scenario_ok = !scenario.is_empty()
        && !scenario.starts_with('-')
        && scenario
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !id_ok || !scenario_ok {
        return Err("EITRI_HARNESS_RUN is not <8 hex>/<scenario>".into());
    }
    Ok(PathBuf::from(text))
}

/// `HOME` must sit strictly inside the run directory, spelled plainly: `Path` folds away `.` and doubled
/// slashes, so the bytes are read, and no `.`, `..` or empty segment may lead elsewhere.
fn home_under(home: Option<&OsStr>, run: &Path) -> Result<(), String> {
    let home = home.ok_or_else(|| "HOME is not set".to_string())?;
    let plain = home.as_bytes().strip_prefix(b"/").is_some_and(|rest| {
        rest.split(|&b| b == b'/')
            .all(|seg| !seg.is_empty() && seg != b"." && seg != b"..")
    });
    let home = Path::new(home);
    if !plain || home == run || !home.starts_with(run) {
        return Err("HOME is not inside the run directory".into());
    }
    Ok(())
}

/// None of the real user's credential and configuration paths may be visible. The home directory itself
/// may exist: binding the tree back in recreates its parents, which then hold only that tree.
fn real_home_hidden(root: &Path, passwd_home: &Path) -> Result<(), String> {
    let home = under(root, passwd_home);
    for rel in HOME_PATHS {
        let path = home.join(rel);
        if present(&path)? {
            return Err(format!("{} is visible", passwd_home.join(rel).display()));
        }
    }
    // `.claude-<account>` directories have no fixed names, so the home directory is listed.
    match std::fs::read_dir(&home) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|e| format!("cannot list {}: {e}", passwd_home.display()))?;
                if entry.file_name().as_bytes().starts_with(b".claude") {
                    return Err(format!("{} is visible", passwd_home.join(entry.file_name()).display()));
                }
            }
            Ok(())
        }
        Err(e) if gone(&e) => Ok(()),
        Err(e) => Err(format!("cannot list {}: {e}", passwd_home.display())),
    }
}

/// `/etc/claude-code` must be missing or an empty directory.
fn managed_settings(root: &Path) -> Result<ManagedSettings, String> {
    let dir = under(root, Path::new("/etc/claude-code"));
    let meta = match std::fs::symlink_metadata(&dir) {
        Ok(meta) => meta,
        Err(e) if gone(&e) => return Ok(ManagedSettings::Absent),
        Err(e) => return Err(format!("cannot stat /etc/claude-code: {e}")),
    };
    if !meta.is_dir() {
        return Err("/etc/claude-code is not a directory".into());
    }
    let mut entries = std::fs::read_dir(&dir).map_err(|e| format!("cannot list /etc/claude-code: {e}"))?;
    match entries.next() {
        None => Ok(ManagedSettings::Empty),
        Some(_) => Err("/etc/claude-code holds files".into()),
    }
}

/// The CLI walks up from its working directory for `CLAUDE.md`, `.claude/` and the git repository; none of
/// them may exist from the project's parent (the run directory) up to `/`.
fn no_project_markers_above(root: &Path, run: &Path) -> Result<(), String> {
    for dir in run.ancestors() {
        for name in ["CLAUDE.md", ".claude", ".git"] {
            if present(&under(root, &dir.join(name)))? {
                return Err(format!("{} exists above the project", dir.join(name).display()));
            }
        }
    }
    Ok(())
}

/// Every directory from `/mnt` down to the run directory's parent must lie on a tmpfs: the one the sandbox puts
/// over `/mnt`, which holds nothing but the directories bound back in. The host's `/mnt` holds other runs and
/// build trees with live editor sockets, and a Unix socket in the file system is reachable from any network
/// namespace; an editor's RPC socket runs commands outside the sandbox.
fn other_runs_hidden(root: &Path, run: &Path, on_tmpfs: &dyn Fn(&Path) -> io::Result<bool>) -> Result<(), String> {
    let parent = run.parent().unwrap_or(run);
    for dir in parent.ancestors().take_while(|d| d.parent().is_some()) {
        match on_tmpfs(&under(root, dir)) {
            Ok(true) => {}
            Ok(false) => {
                return Err(format!(
                    "{} is not on the sandbox's tmpfs: other runs and their sockets are in view",
                    dir.display()
                ));
            }
            Err(e) => return Err(format!("cannot statfs {}: {e}", dir.display())),
        }
    }
    Ok(())
}

/// Where host daemons keep their sockets: `/run` (and so `/var/run`, a link to it) for the container engine's
/// or a running sidecar's, and the GSS-API proxy's credential broker.
const SOCKET_DIRS: &[&str] = &["/run", "/var/lib/gssproxy"];

/// No Unix socket may be in [`SOCKET_DIRS`]: a host daemon's socket is reachable from any network namespace
/// and acts outside the sandbox.
fn no_host_sockets(root: &Path) -> Result<(), String> {
    for dir in SOCKET_DIRS {
        find_socket(&under(root, Path::new(dir)), Path::new(dir))?;
    }
    Ok(())
}

fn find_socket(dir: &Path, shown: &Path) -> Result<(), String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if gone(&e) => return Ok(()),
        Err(e) => return Err(format!("cannot list {}: {e}", shown.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot list {}: {e}", shown.display()))?;
        let shown = shown.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|e| format!("cannot stat {}: {e}", shown.display()))?;
        if kind.is_socket() {
            return Err(format!("{} is a socket in reach", shown.display()));
        }
        if kind.is_dir() {
            find_socket(&entry.path(), &shown)?;
        }
    }
    Ok(())
}

/// No remote or automount file system may be in view. A remote mount the sandbox covered is still in the
/// mount table under the cover, so each is looked up at its mount point: it is in view when what is found
/// there is that file system. The lookup does not follow the last component, so it triggers no automount.
fn no_remote_mounts(root: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(under(root, Path::new("/proc/self/mountinfo")))
        .map_err(|e| format!("cannot read /proc/self/mountinfo: {e}"))?;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mount = parse_mountinfo(line).ok_or_else(|| format!("cannot read the mount table line {line:?}"))?;
        if !REMOTE_FS.contains(&mount.fs_type.as_str()) {
            continue;
        }
        match std::fs::symlink_metadata(under(root, &mount.point)) {
            Ok(meta) if dev_numbers(meta.dev()) == mount.dev => {
                return Err(format!("{} ({}) is in view", mount.point.display(), mount.fs_type));
            }
            Ok(_) => {}
            Err(e) if gone(&e) => {}
            Err(e) => return Err(format!("cannot stat {}: {e}", mount.point.display())),
        }
    }
    Ok(())
}

struct Mount {
    dev: (u64, u64),
    point: PathBuf,
    fs_type: String,
}

/// One `/proc/self/mountinfo` line: `id parent major:minor root point options [optional...] - type source ...`.
fn parse_mountinfo(line: &str) -> Option<Mount> {
    let fields: Vec<&str> = line.split(' ').collect();
    let (major, minor) = fields.get(2)?.split_once(':')?;
    let point = unescape_mount_path(fields.get(4)?)?;
    let sep = fields.iter().skip(6).position(|f| *f == "-")? + 6;
    Some(Mount {
        dev: (major.parse().ok()?, minor.parse().ok()?),
        point,
        fs_type: (*fields.get(sep + 1)?).to_string(),
    })
}

/// The kernel writes a space, tab, newline or backslash in a mount path as a three-digit octal escape.
fn unescape_mount_path(field: &str) -> Option<PathBuf> {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let digits = bytes.get(i + 1..i + 4)?;
            let text = std::str::from_utf8(digits).ok()?;
            out.push(u8::from_str_radix(text, 8).ok()?);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(OsStr::from_bytes(&out)))
}

/// A device number split as the kernel's mount table prints it (glibc's `major`/`minor` encoding).
fn dev_numbers(dev: u64) -> (u64, u64) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major, minor)
}

/// Whether `path` lies on a tmpfs.
#[cfg(target_os = "linux")]
fn on_tmpfs(path: &Path) -> io::Result<bool> {
    const TMPFS_MAGIC: i64 = 0x0102_1994;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL byte"))?;
    // SAFETY: an all-zero `statfs` is a valid value of the plain C struct; `statfs` overwrites it.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is NUL-terminated and `st` is a live `statfs` the call fills.
    if unsafe { libc::statfs(c_path.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[allow(clippy::unnecessary_cast)]
    let kind = st.f_type as i64;
    Ok(kind == TMPFS_MAGIC)
}

/// The harness sandbox exists only on Linux; elsewhere nothing is on its tmpfs.
#[cfg(not(target_os = "linux"))]
fn on_tmpfs(_: &Path) -> io::Result<bool> {
    Ok(false)
}

/// A connect outside loopback must fail at once with "network unreachable": inside a network namespace that
/// holds only `lo` there is no route at all, while on a host the packet would leave or time out.
fn connect_unrouted() -> Result<(), String> {
    let addr: SocketAddr = UNROUTED.parse().expect("a literal socket address");
    let started = Instant::now();
    let got = TcpStream::connect_timeout(&addr, Duration::from_secs(2));
    let took = started.elapsed();
    match got {
        Ok(_) => Err(format!("a connect to {UNROUTED} succeeded")),
        Err(e) if e.raw_os_error() == Some(libc::ENETUNREACH) && took < Duration::from_secs(1) => Ok(()),
        Err(e) => Err(format!(
            "a connect to {UNROUTED} failed with {e} after {took:?}, not at once with ENETUNREACH"
        )),
    }
}

/// An absolute path read under `root`.
fn under(root: &Path, abs: &Path) -> PathBuf {
    root.join(abs.strip_prefix("/").unwrap_or(abs))
}

/// Whether anything (a dangling symlink included) is at `path`; a path that cannot be examined counts as a
/// failure of the check, never as absent.
fn present(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if gone(&e) => Ok(false),
        Err(e) => Err(format!("cannot stat {}: {e}", path.display())),
    }
}

fn gone(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
}

/// The home directory of `uid` in the password database, through the C library so that every configured
/// source is asked.
fn passwd_home_of(uid: libc::uid_t) -> Option<PathBuf> {
    let mut size = 4096usize;
    loop {
        let mut buf: Vec<libc::c_char> = vec![0; size];
        // SAFETY: an all-zero `passwd` is a valid value of the plain C struct; `getpwuid_r` overwrites it.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is to a live local of the right type, and `buf.len()` is the buffer's size.
        let rc = unsafe { libc::getpwuid_r(uid, &mut entry, buf.as_mut_ptr(), buf.len(), &mut found) };
        if rc == libc::ERANGE && size < (1 << 20) {
            size *= 4;
            continue;
        }
        if rc != 0 || found.is_null() || entry.pw_dir.is_null() {
            return None;
        }
        // SAFETY: on success `pw_dir` points to a NUL-terminated string inside `buf`, which is still alive.
        let dir = unsafe { CStr::from_ptr(entry.pw_dir) };
        return Some(PathBuf::from(OsStr::from_bytes(dir.to_bytes())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, fs, os::unix::fs::symlink, os::unix::net::UnixListener};

    const RUN: &str = "/mnt/eitri-runs/ch/abcd1234/c00";
    const PASSWD_HOME: &str = "/home/someone";

    #[test]
    fn a_run_set_aside_leaves_the_next_one_empty_directories_and_the_runners_log_in_place() {
        let run = crate::state_dirs::test_workspace_dir("sandbox-aside");
        for dir in ["log", "standin", "claude/projects", "project", "tmp", "r", "gates"] {
            fs::create_dir_all(run.join(dir)).unwrap();
        }
        fs::write(run.join("log/test.out"), "the runner reads this").unwrap();
        fs::write(run.join("log/cli-stdout.jsonl"), "x").unwrap();
        fs::write(run.join("standin/endpoint.jsonl"), "x").unwrap();
        fs::write(run.join("claude/projects/s.jsonl"), "x").unwrap();
        fs::write(run.join("gates/wait"), "#!/bin/sh").unwrap();
        fs::write(run.join("gates/k.open"), "").unwrap();

        set_earlier_run_aside(&run, "before-run3");

        assert_eq!(
            fs::read_to_string(run.join("log/test.out")).unwrap(),
            "the runner reads this"
        );
        assert!(!run.join("log/cli-stdout.jsonl").exists(), "the tap's log moved");
        assert!(run.join("log-before-run3/cli-stdout.jsonl").is_file());
        assert!(run.join("standin-before-run3/endpoint.jsonl").is_file());
        assert!(run.join("claude-before-run3/projects/s.jsonl").is_file());
        for dir in ["standin", "claude", "project", "tmp", "r"] {
            assert_eq!(fs::read_dir(run.join(dir)).unwrap().count(), 0, "{dir} starts empty");
        }
        assert!(run.join("gates/wait").is_file(), "the gate script stays");
        assert!(!run.join("gates/k.open").exists(), "an opened gate is closed again");
    }

    /// The mount table of a sandbox on a host with a synced tree: the remote mount is covered, so nothing at
    /// its mount point is that file system.
    const MOUNTINFO: &str = "22 1 259:2 / / ro,relatime shared:1 - ext4 /dev/nvme0n1p2 rw\n\
        40 22 0:4051 / /nfs/syncthing rw,relatime master:20 - nfs4 nas:/sync rw\n\
        41 22 0:4052 / /mnt rw,nosuid - tmpfs tmpfs rw\n";

    /// A fake root laid out as the sandbox shows it: the run directory with its home and project, the real
    /// home recreated only as the parents of the source tree bound back in, `/etc` without managed settings,
    /// an empty `/run`, and a mount table whose only remote mount is covered. Its `/mnt` counts as the
    /// sandbox's tmpfs.
    struct FakeRoot {
        dir: PathBuf,
    }

    impl FakeRoot {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("agent-sandbox-check-{}", uuid::Uuid::new_v4()));
            for sub in [
                "mnt/eitri-runs/ch/abcd1234/c00/home",
                "mnt/eitri-runs/ch/abcd1234/c00/project",
                "home/someone/.cache/remote-runs/tree",
                "etc",
                "run/user",
                "proc/self",
            ] {
                fs::create_dir_all(dir.join(sub)).unwrap();
            }
            fs::write(dir.join("proc/self/mountinfo"), MOUNTINFO).unwrap();
            FakeRoot { dir }
        }

        fn mnt_on_tmpfs(&self) -> impl Fn(&Path) -> io::Result<bool> {
            let mnt = self.dir.join("mnt");
            move |p: &Path| Ok(p.starts_with(&mnt))
        }

        fn inputs<'a>(&'a self, on_tmpfs: &'a dyn Fn(&Path) -> io::Result<bool>) -> Inputs<'a> {
            Inputs {
                root: &self.dir,
                run: Some(OsStr::new(RUN)),
                home: Some(OsStr::new("/mnt/eitri-runs/ch/abcd1234/c00/home")),
                passwd_home: Some(Path::new(PASSWD_HOME)),
                on_tmpfs,
            }
        }

        fn path(&self, abs: &str) -> PathBuf {
            under(&self.dir, Path::new(abs))
        }

        fn eval_with(
            &self,
            run: Option<&str>,
            home: Option<&str>,
            passwd_home: Option<&str>,
        ) -> Result<ManagedSettings, String> {
            let on_tmpfs = self.mnt_on_tmpfs();
            let inputs = Inputs {
                root: &self.dir,
                run: run.map(OsStr::new),
                home: home.map(OsStr::new),
                passwd_home: passwd_home.map(Path::new),
                on_tmpfs: &on_tmpfs,
            };
            evaluate(&inputs, || Ok(()))
        }

        fn eval(&self) -> Result<ManagedSettings, String> {
            self.eval_with(Some(RUN), Some(&format!("{RUN}/home")), Some(PASSWD_HOME))
        }
    }

    impl Drop for FakeRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn assert_refused(got: Result<ManagedSettings, String>, needle: &str) {
        match got {
            Ok(m) => panic!("expected a refusal naming {needle:?}, the check passed ({m:?})"),
            Err(why) => assert!(why.contains(needle), "refusal {why:?} does not name {needle:?}"),
        }
    }

    #[test]
    fn the_sandbox_as_run_py_builds_it_passes() {
        let root = FakeRoot::new();
        assert_eq!(root.eval(), Ok(ManagedSettings::Absent));
    }

    #[test]
    fn the_rebound_tree_parents_in_the_real_home_are_not_a_credential() {
        let root = FakeRoot::new();
        assert!(root.path("/home/someone/.cache/remote-runs/tree").is_dir());
        assert_eq!(root.eval(), Ok(ManagedSettings::Absent));
    }

    #[test]
    fn a_real_home_missing_altogether_passes() {
        let root = FakeRoot::new();
        fs::remove_dir_all(root.path("/home/someone")).unwrap();
        assert_eq!(root.eval(), Ok(ManagedSettings::Absent));
    }

    #[test]
    fn an_unset_run_variable_refuses_before_any_connect() {
        let root = FakeRoot::new();
        let tried = Cell::new(false);
        let on_tmpfs = root.mnt_on_tmpfs();
        let inputs = Inputs {
            root: &root.dir,
            run: None,
            home: Some(OsStr::new("/home/someone")),
            passwd_home: Some(Path::new(PASSWD_HOME)),
            on_tmpfs: &on_tmpfs,
        };
        let got = evaluate(&inputs, || {
            tried.set(true);
            Ok(())
        });
        assert_refused(got, "EITRI_HARNESS_RUN is not set");
        assert!(!tried.get(), "outside the sandbox the check must not try the network");
    }

    #[test]
    fn a_run_directory_of_another_form_refuses() {
        let root = FakeRoot::new();
        for bad in [
            "/mnt/eitri-runs/abcd1234/c00",
            "/mnt/eitri-runs/ch/abcd123/c00",
            "/mnt/eitri-runs/ch/abcd12345/c00",
            "/mnt/eitri-runs/ch/ABCD1234/c00",
            "/mnt/eitri-runs/ch/abcd1234",
            "/mnt/eitri-runs/ch/abcd1234/",
            "/mnt/eitri-runs/ch/abcd1234/c00/x",
            "/mnt/eitri-runs/ch/abcd1234/..",
            "/mnt/eitri-runs/ch/abcd1234/-r1",
            "/mnt/eitri-runs/ch/abcd1234/a_scenario_id_far_too_long",
            "mnt/eitri-runs/ch/abcd1234/c00",
        ] {
            let got = root.eval_with(Some(bad), Some(&format!("{bad}/home")), Some(PASSWD_HOME));
            assert!(got.is_err(), "{bad} was accepted as a run directory");
        }
    }

    #[test]
    fn a_repetition_suffix_and_the_48_byte_limit_are_accepted() {
        let root = FakeRoot::new();
        let run = "/mnt/eitri-runs/ch/abcd1234/c16w_stop-r12";
        fs::create_dir_all(root.path(run)).unwrap();
        assert_eq!(
            root.eval_with(Some(run), Some(&format!("{run}/home")), Some(PASSWD_HOME)),
            Ok(ManagedSettings::Absent)
        );
        let at_limit = "/mnt/eitri-runs/ch/abcd1234/c16w_stop_abcde-r123";
        assert_eq!(at_limit.len(), MAX_RUN_BYTES);
        assert!(root
            .eval_with(Some(at_limit), Some(&format!("{at_limit}/home")), Some(PASSWD_HOME))
            .is_ok());
        let over = "/mnt/eitri-runs/ch/abcd1234/c16w_stop_abcdef-r123";
        assert_refused(
            root.eval_with(Some(over), Some(&format!("{over}/home")), Some(PASSWD_HOME)),
            "longer than",
        );
    }

    #[test]
    fn home_must_be_inside_the_run_directory() {
        let root = FakeRoot::new();
        for home in [
            None,
            Some("/home/someone"),
            Some(RUN),
            Some("/mnt/eitri-runs/ch/abcd1234/c00x/home"),
            Some("/mnt/eitri-runs/ch/abcd1234/c00/../c01/home"),
            Some("/mnt/eitri-runs/ch/abcd1234/c00/./home"),
            Some("/mnt/eitri-runs/ch/abcd1234/c00//home"),
            Some("/mnt/eitri-runs/ch/abcd1234/c00/home/"),
            Some("mnt/eitri-runs/ch/abcd1234/c00/home"),
        ] {
            let got = root.eval_with(Some(RUN), home, Some(PASSWD_HOME));
            assert!(got.is_err(), "HOME {home:?} was accepted");
        }
    }

    #[test]
    fn no_passwd_home_refuses() {
        let root = FakeRoot::new();
        assert_refused(
            root.eval_with(Some(RUN), Some(&format!("{RUN}/home")), None),
            "password database",
        );
    }

    #[test]
    fn each_credential_or_configuration_path_of_the_real_home_refuses() {
        for rel in [
            ".claude",
            ".claude.json",
            ".ssh",
            ".config/eitri",
            ".config/nvim",
            ".claude-work",
            ".claude-test",
        ] {
            let root = FakeRoot::new();
            let path = root.path(&format!("{PASSWD_HOME}/{rel}"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            if rel.ends_with(".json") {
                fs::write(&path, b"{}").unwrap();
            } else {
                fs::create_dir(&path).unwrap();
            }
            assert_refused(root.eval(), &format!("{PASSWD_HOME}/{rel} is visible"));
        }
    }

    #[test]
    fn a_dangling_symlink_counts_as_visible() {
        let root = FakeRoot::new();
        symlink("/nowhere", root.path(&format!("{PASSWD_HOME}/.claude"))).unwrap();
        assert_refused(root.eval(), ".claude is visible");
    }

    #[test]
    fn managed_settings_absent_or_empty_pass_and_any_content_refuses() {
        let root = FakeRoot::new();
        assert_eq!(root.eval(), Ok(ManagedSettings::Absent));
        fs::create_dir(root.path("/etc/claude-code")).unwrap();
        assert_eq!(root.eval(), Ok(ManagedSettings::Empty));
        fs::write(root.path("/etc/claude-code/managed-settings.json"), b"{}").unwrap();
        assert_refused(root.eval(), "/etc/claude-code holds files");
    }

    #[test]
    fn managed_settings_as_a_file_refuses() {
        let root = FakeRoot::new();
        fs::write(root.path("/etc/claude-code"), b"").unwrap();
        assert_refused(root.eval(), "not a directory");
    }

    #[test]
    fn a_project_marker_in_any_ancestor_refuses() {
        for (dir, name) in [
            (RUN, "CLAUDE.md"),
            (RUN, ".claude"),
            ("/mnt/eitri-runs/ch/abcd1234", ".git"),
            ("/mnt/eitri-runs", "CLAUDE.md"),
            ("/mnt", ".claude"),
            ("/", ".git"),
        ] {
            let root = FakeRoot::new();
            let path = root.path(dir).join(name);
            if name == "CLAUDE.md" {
                fs::write(&path, b"x").unwrap();
            } else {
                fs::create_dir(&path).unwrap();
            }
            assert_refused(root.eval(), "exists above the project");
        }
    }

    #[test]
    fn a_repository_in_the_project_itself_is_the_scenario_s_business() {
        let root = FakeRoot::new();
        fs::create_dir(root.path(&format!("{RUN}/project/.git"))).unwrap();
        assert_eq!(root.eval(), Ok(ManagedSettings::Absent));
    }

    #[test]
    fn a_reachable_network_refuses() {
        let root = FakeRoot::new();
        let on_tmpfs = root.mnt_on_tmpfs();
        let inputs = root.inputs(&on_tmpfs);
        assert_refused(evaluate(&inputs, || Err("connected".into())), "connected");
        assert_eq!(evaluate(&inputs, || Ok(())), Ok(ManagedSettings::Absent));
    }

    #[test]
    fn each_directory_above_the_run_off_the_sandbox_tmpfs_refuses() {
        let root = FakeRoot::new();
        for host_dir in [
            "/mnt",
            "/mnt/eitri-runs",
            "/mnt/eitri-runs/ch",
            "/mnt/eitri-runs/ch/abcd1234",
        ] {
            let host = root.path(host_dir);
            let mnt = root.path("/mnt");
            let on_tmpfs = move |p: &Path| -> io::Result<bool> { Ok(p.starts_with(&mnt) && p != host) };
            assert_refused(
                evaluate(&root.inputs(&on_tmpfs), || Ok(())),
                &format!("{host_dir} is not on the sandbox's tmpfs"),
            );
        }
        let failing = |_: &Path| -> io::Result<bool> { Err(io::Error::from_raw_os_error(libc::EACCES)) };
        assert_refused(
            evaluate(&root.inputs(&failing), || Ok(())),
            "cannot statfs /mnt/eitri-runs/ch/abcd1234",
        );
    }

    #[test]
    fn the_run_directory_itself_need_not_be_on_tmpfs() {
        let root = FakeRoot::new();
        let run = root.path(RUN);
        let mnt = root.path("/mnt");
        let on_tmpfs = move |p: &Path| -> io::Result<bool> { Ok(p.starts_with(&mnt) && !p.starts_with(&run)) };
        assert_eq!(
            evaluate(&root.inputs(&on_tmpfs), || Ok(())),
            Ok(ManagedSettings::Absent)
        );
    }

    #[test]
    fn a_socket_anywhere_under_run_or_the_gss_proxy_directory_refuses() {
        for abs in [
            "/run/docker.sock",
            "/run/user/1000/bus",
            "/var/lib/gssproxy/default.sock",
        ] {
            let root = FakeRoot::new();
            let path = root.path(abs);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let _listener = UnixListener::bind(&path).unwrap();
            assert_refused(root.eval(), &format!("{abs} is a socket in reach"));
        }
    }

    #[test]
    fn files_and_directories_under_run_pass_and_a_missing_run_passes() {
        let root = FakeRoot::new();
        fs::create_dir_all(root.path("/run/lock/sub")).unwrap();
        fs::write(root.path("/run/lock/sub/pid"), b"1").unwrap();
        symlink("/nowhere", root.path("/run/link")).unwrap();
        assert_eq!(root.eval(), Ok(ManagedSettings::Absent));
        fs::remove_dir_all(root.path("/run")).unwrap();
        assert_eq!(root.eval(), Ok(ManagedSettings::Absent));
    }

    fn mount_line(root: &FakeRoot, point: &str, fs_type: &str, same_device: bool) -> String {
        let (major, minor) = dev_numbers(fs::symlink_metadata(root.path(point)).unwrap().dev());
        let minor = if same_device { minor } else { minor + 1 };
        format!("60 22 {major}:{minor} / {point} rw shared:30 - {fs_type} server:/x rw\n")
    }

    #[test]
    fn a_remote_mount_in_view_refuses() {
        for fs_type in ["nfs4", "cifs", "autofs", "fuse.sshfs"] {
            let root = FakeRoot::new();
            fs::create_dir_all(root.path("/srv/share")).unwrap();
            let table = format!("{MOUNTINFO}{}", mount_line(&root, "/srv/share", fs_type, true));
            fs::write(root.path("/proc/self/mountinfo"), table).unwrap();
            assert_refused(root.eval(), &format!("/srv/share ({fs_type}) is in view"));
        }
    }

    #[test]
    fn a_covered_or_local_mount_passes() {
        let root = FakeRoot::new();
        fs::create_dir_all(root.path("/srv/share")).unwrap();
        let table = format!(
            "{MOUNTINFO}{}{}",
            mount_line(&root, "/srv/share", "nfs4", false),
            mount_line(&root, "/srv/share", "ext4", true)
        );
        fs::write(root.path("/proc/self/mountinfo"), table).unwrap();
        assert_eq!(root.eval(), Ok(ManagedSettings::Absent));
    }

    #[test]
    fn an_unreadable_or_garbled_mount_table_refuses() {
        let root = FakeRoot::new();
        fs::write(root.path("/proc/self/mountinfo"), "22 1 259:2 / /\n").unwrap();
        assert_refused(root.eval(), "cannot read the mount table line");
        fs::remove_file(root.path("/proc/self/mountinfo")).unwrap();
        assert_refused(root.eval(), "cannot read /proc/self/mountinfo");
    }

    #[test]
    fn mount_paths_are_unescaped_and_device_numbers_split() {
        let mount = parse_mountinfo("30 22 259:65539 / /a\\040b\\134c rw shared:2 master:3 - nfs4 h:/x rw").unwrap();
        assert_eq!(mount.point, PathBuf::from("/a b\\c"));
        assert_eq!(mount.dev, (259, 65539));
        assert_eq!(mount.fs_type, "nfs4");
        // glibc's makedev(259, 65539): the low 8 bits of the minor, the low 12 of the major, then the rest.
        let dev: u64 = (0x103 << 8) | 0x03 | (0x10000 << 12);
        assert_eq!(dev_numbers(dev), (259, 65539));
        assert_eq!(dev_numbers(0x0803), (8, 3));
    }

    #[test]
    fn the_password_database_knows_this_uid() {
        // SAFETY: `getuid` has no preconditions and cannot fail.
        let home = passwd_home_of(unsafe { libc::getuid() });
        assert!(
            home.is_some_and(|h| h.is_absolute()),
            "no absolute passwd home for the test's own uid"
        );
    }

    /// Run by `run.py --smoke` inside the sandbox, where it must pass, and outside it, where it must print
    /// the skip line; `cargo test -- --ignored` elsewhere prints the skip line and passes.
    #[test]
    #[ignore = "meaningful only inside the claude harness sandbox: run.py --smoke runs it"]
    fn self_check_in_the_harness_sandbox() {
        if !check() {
            return;
        }
    }
}
