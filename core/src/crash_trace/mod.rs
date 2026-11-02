//! What a crash leaves behind, and a window's output kept where a person can attach it.
//!
//! A panic, on any thread, writes a record: `<state home>/eitri/log/crash-<utc time>-<pid>.log`, private to this user.
//! It says what a report needs (version, build, thread, location, message, backtrace, the last of the window's
//! output) and nothing of what the window was working on, since the person who attaches it attaches it to a public
//! issue: see [`scrub`]. Nothing is sent anywhere. The next start of a main window says once where the record is.
//!
//! [`relay`] keeps the output of a window not started from a terminal, and makes a failed write harmless.
//!
//! `EITRI_WINDOW_LOG=off` turns both off.

pub mod relay;
pub(crate) mod scrub;

use std::ffi::{CStr, OsStr};
use std::fmt::Write as _;
use std::fs::File;
use std::io::Write as _;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub use relay::{route_for, Route};

/// The switch.
pub const OFF_ENV: &str = "EITRI_WINDOW_LOG";
/// Under `<state home>/eitri`.
pub const LOG_SUBDIR: &str = "log";
const CRASH_START: &str = "crash-";
const CRASH_END: &str = ".log";
/// Which record the last notice named, so each is told once.
const NOTICED: &str = "crash-noticed";
/// How many main windows of one project keep a log.
const MAX_MAIN_WINDOWS: u32 = 8;
/// The records kept: older ones go when a main window starts.
const KEEP: usize = 8;
/// One process writes this many panics, so a worker that panics and is restarted in a loop fills no disk.
const MAX_PANICS: u32 = 8;
/// The room for one panic's words and backtrace, made at the start so the hook has it whatever the allocator's state.
const RECORD_ROOM: usize = 48 * 1024;
/// How long a second panicking thread waits for the first to finish writing.
const WAIT_FOR_RECORD: Duration = Duration::from_millis(500);
/// How long a panic waits for the output on its way to the log.
const WAIT_FOR_OUTPUT: Duration = Duration::from_millis(100);

/// Whether `value`, [`OFF_ENV`]'s, turns this off.
pub fn is_off(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| value == "off")
}

/// `<state home>/eitri/log`.
pub fn log_dir(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    crate::state_paths::state_subdir(xdg_state_home, home, LOG_SUBDIR)
}

/// Opens this main window's log: `main-<project key>.log`, or `-2` and so on when another window of the project holds
/// that one (a project can have more than a pair). A file lock says which is taken, so one window never moves a log
/// another is writing. It is held until the process ends, and goes with it however that happens.
pub fn open_main_log(dir: &Path, project_root: &Path) -> Option<(File, PathBuf)> {
    crate::state_paths::create_state_dir(dir).ok()?;
    let key = crate::state_paths::project_key(project_root);
    for window in 1..=MAX_MAIN_WINDOWS {
        let name = match window {
            1 => format!("main-{key}.log"),
            _ => format!("main-{key}-{window}.log"),
        };
        let lock = agent::private_fs::PrivateLock::open(&dir.join(format!("{name}.lock"))).ok()?;
        if lock.try_lock().ok()? {
            std::mem::forget(lock);
            return relay::open_log(dir, &name).ok();
        }
    }
    None
}

/// The file of the record of this process's panics, once one has been written.
struct Record {
    text: Box<[u8]>,
    file: Option<File>,
}

struct Hook {
    /// The state directory, held open: a panic must not have to walk to it.
    dir: Option<File>,
    version: String,
    home: Option<Vec<u8>>,
    main_thread: libc::pthread_t,
    panics: AtomicU32,
    record: Mutex<Record>,
}

static HOOK: OnceLock<Hook> = OnceLock::new();

/// Writes into a fixed buffer. What does not fit is dropped: a formatted panic must not fail in the hook.
struct Fixed<'a> {
    bytes: &'a mut [u8],
    len: usize,
}

impl std::fmt::Write for Fixed<'_> {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        let take = text.len().min(self.bytes.len() - self.len);
        self.bytes[self.len..self.len + take].copy_from_slice(&text.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

fn utc_now() -> libc::tm {
    // SAFETY: a null `time` argument is allowed, `gmtime_r` fills the struct it is given and reads no environment,
    // which a hook on another thread must not while the main thread sets variables.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::gmtime_r(&now, &mut tm);
        tm
    }
}

/// `crash-<utc time>-<pid>.log`, NUL-terminated, so names sort by time.
fn record_name<'a>(tm: &libc::tm, buffer: &'a mut [u8; 64]) -> &'a CStr {
    let mut name = Fixed {
        bytes: &mut buffer[..63],
        len: 0,
    };
    let _ = write!(
        name,
        "{CRASH_START}{:04}{:02}{:02}T{:02}{:02}{:02}Z-{}{CRASH_END}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        std::process::id()
    );
    let len = name.len;
    buffer[len] = 0;
    CStr::from_bytes_until_nul(&buffer[..=len]).unwrap_or(c"crash.log")
}

/// Calls `say` with the name a thread was given, or `main` for the one that installed the hook. `pthread_getname_np`
/// is used because `std::thread::current` is not to be called while a thread's locals are going, where a panic can
/// happen.
fn with_thread_name(main_thread: libc::pthread_t, say: impl FnOnce(&str)) {
    let mut name = [0 as libc::c_char; 16];
    // SAFETY: the buffer is as long as the length passed, and `pthread_self` is always valid.
    let got = unsafe {
        let me = libc::pthread_self();
        if me == main_thread {
            return say("main");
        }
        libc::pthread_getname_np(me, name.as_mut_ptr(), name.len())
    };
    // SAFETY: on success the name is NUL-terminated in the buffer; the last byte is never written otherwise.
    let name = (got == 0).then(|| unsafe { CStr::from_ptr(name.as_ptr()) }.to_string_lossy());
    say(name.as_deref().unwrap_or("unnamed"));
}

fn write_kernel(text: &mut Fixed<'_>) {
    // SAFETY: `uname` fills the struct; the fields are NUL-terminated on success. The node name is not read: a host's
    // name is nothing a report needs.
    unsafe {
        let mut name: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut name) == 0 {
            let _ = writeln!(
                text,
                "system: {} {} {}",
                CStr::from_ptr(name.sysname.as_ptr()).to_string_lossy(),
                CStr::from_ptr(name.release.as_ptr()).to_string_lossy(),
                CStr::from_ptr(name.machine.as_ptr()).to_string_lossy()
            );
        }
    }
}

const RECORD_HEAD: &str =
    "Eitri crash record. It stays on this machine until you attach it. It holds no conversation, \
                           prompt or tool output; read it first if you like.\n\n";

fn write_panic(hook: &Hook, record: &mut Record, info: &PanicHookInfo<'_>) {
    let Record { text, file } = record;
    let tm = utc_now();
    let mut words = Fixed {
        bytes: &mut text[..],
        len: 0,
    };
    if file.is_none() {
        let _ = words.write_str(RECORD_HEAD);
    }
    let _ = writeln!(words, "version: {}", hook.version);
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    let _ = writeln!(
        words,
        "build: {profile}, {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    write_kernel(&mut words);
    let _ = writeln!(
        words,
        "time: {:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    );
    with_thread_name(hook.main_thread, |name| {
        let _ = writeln!(words, "thread: {name}");
    });
    if let Some(location) = info.location() {
        let _ = writeln!(words, "location: {location}");
    }
    let _ = writeln!(words, "message: {}", scrub::panic_message(info.payload()));
    let len = words.len;
    if file.is_none() {
        let Some(dir) = &hook.dir else { return };
        let mut name = [0; 64];
        *file = agent::private_fs::create_private_file_at(dir.as_fd(), record_name(&tm, &mut name)).ok();
    }
    let Some(file) = file.as_ref() else { return };
    // The words first: a backtrace is the one step here that can go wrong, and what is written stays written.
    write_scrubbed(file, &mut text[..len], hook.home.as_deref());
    let mut trace = Fixed {
        bytes: &mut text[..],
        len: 0,
    };
    let _ = write!(trace, "\nbacktrace:\n{}\n", std::backtrace::Backtrace::force_capture());
    let len = trace.len;
    write_scrubbed(file, &mut text[..len], hook.home.as_deref());
    relay::flush(WAIT_FOR_OUTPUT);
    let mut said = false;
    relay::tail(|bytes| {
        if !said && !bytes.is_empty() {
            said = true;
            let mut heading = file;
            let _ = heading.write_all(b"\nwhat the window printed last:\n");
        }
        let mut out = file;
        let _ = out.write_all(bytes);
    });
    let mut end = file;
    let _ = end.write_all(b"\n");
}

fn write_scrubbed(mut file: &File, text: &mut [u8], home: Option<&[u8]>) {
    let len = home.map_or(text.len(), |home| scrub::strip_home(text, home));
    let _ = file.write_all(&text[..len]);
}

/// What the standard hook says, whole, to where stderr went before the relay. Only a person's own terminal, script or
/// journal gets this; it is what they got before there was a log.
fn say_outside_the_log(hook: &Hook, mut out: &File, info: &PanicHookInfo<'_>) {
    let payload = info.payload();
    let message = payload
        .downcast_ref::<&'static str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("Box<dyn Any>");
    with_thread_name(hook.main_thread, |name| {
        let _ = match info.location() {
            Some(location) => writeln!(out, "thread '{name}' panicked at {location}:\n{message}"),
            None => writeln!(out, "thread '{name}' panicked:\n{message}"),
        };
    });
}

fn on_panic(hook: &Hook, info: &PanicHookInfo<'_>) {
    if hook.panics.fetch_add(1, Ordering::Relaxed) >= MAX_PANICS {
        return;
    }
    let end = Instant::now() + WAIT_FOR_RECORD;
    loop {
        match hook.record.try_lock() {
            Ok(mut record) => return write_panic(hook, &mut record, info),
            // Another thread stopped inside this hook. Its record is as good as it got, and this one can go on.
            Err(std::sync::TryLockError::Poisoned(_)) => return,
            Err(std::sync::TryLockError::WouldBlock) if Instant::now() >= end => return,
            Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

/// Installs the panic hook, once per process. `dir` is where records go; without one (or when it cannot be made) a
/// panic says what it always said and nothing is written. A panic's own words stay out of the log: with the relay on
/// fd 2 they go straight to where stderr went, and the previous hook runs only when it is not.
pub fn install(version: &str, dir: Option<&Path>, home: Option<&OsStr>) {
    let dir = dir
        .filter(|dir| crate::state_paths::create_state_dir(dir).is_ok())
        .and_then(|dir| agent::private_fs::open_dir(dir).ok());
    let home = home
        .map(OsStr::as_bytes)
        .filter(|home| home.first() == Some(&b'/') && home.len() > 1)
        .map(<[u8]>::to_vec);
    let hook = Hook {
        dir,
        version: version.to_string(),
        home,
        // SAFETY: `pthread_self` is always valid.
        main_thread: unsafe { libc::pthread_self() },
        panics: AtomicU32::new(0),
        record: Mutex::new(Record {
            text: vec![0; RECORD_ROOM].into_boxed_slice(),
            file: None,
        }),
    };
    if HOOK.set(hook).is_err() {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let hook = HOOK.get();
        if let Some(hook) = hook {
            on_panic(hook, info);
        }
        match (hook, relay::stderr_outside_the_log()) {
            (Some(hook), Some(out)) => say_outside_the_log(hook, out, info),
            _ => previous(info),
        }
    }));
}

fn log_header(hook: &Hook) -> String {
    format!("{}, started {}", hook.version, utc_stamp())
}

/// Starts keeping the streams `stdout` and `stderr` route, in `log` when there is one. `false` when nothing changed:
/// [`install`] has not run (the switch is off), no stream is routed, or a pipe or a thread could not be had.
pub fn keep_output(log: Option<(File, PathBuf)>, stdout: Route, stderr: Route) -> bool {
    let Some(hook) = HOOK.get() else { return false };
    let header = log_header(hook);
    relay::start(relay::Start {
        log: log.map(|(file, path)| (file, path, header)),
        stdout,
        stderr,
        home: hook.home.clone(),
    })
    .unwrap_or(false)
}

/// Gives a relay started without one its log.
pub fn attach_log(file: File, path: PathBuf) {
    if let Some(hook) = HOOK.get() {
        relay::attach_log(file, path, &log_header(hook));
    }
}

fn utc_stamp() -> String {
    let tm = utc_now();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// What the state directory holds under the names records have, oldest first.
fn records_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(CRASH_START) && name.ends_with(CRASH_END))
        .collect();
    names.sort();
    names
}

fn sort_key(name: &str) -> &str {
    name.rsplit_once('-').map_or(name, |(time, _)| time)
}

/// The one line for a record no window has told of yet, and the record is remembered as told. Older records past
/// [`KEEP`] go here too.
pub fn new_crash_notice(dir: &Path, home: Option<&OsStr>) -> Option<String> {
    let names = records_in(dir);
    for old in names.iter().take(names.len().saturating_sub(KEEP)) {
        let _ = std::fs::remove_file(dir.join(old));
    }
    let newest = names.last()?;
    let told = agent::private_fs::read_private_to_string_capped(&dir.join(NOTICED), 256).unwrap_or_default();
    if !told.is_empty() && sort_key(newest) <= sort_key(told.trim()) {
        return None;
    }
    let _ = agent::private_fs::write_private(&dir.join(NOTICED), newest.as_bytes());
    let path = dir.join(newest);
    let shown = match home.map(Path::new).and_then(|home| path.strip_prefix(home).ok()) {
        Some(inside) => Path::new("~").join(inside),
        None => path,
    };
    Some(format!(
        "Eitri hit a bug in its last run. To report it, attach {} to a GitHub issue.",
        shown.display()
    ))
}

static PENDING: Mutex<Option<String>> = Mutex::new(None);

/// Keeps `notice` until the window can show it.
pub fn set_pending_notice(notice: Option<String>) {
    if let Ok(mut pending) = PENDING.lock() {
        *pending = notice;
    }
}

/// The line [`set_pending_notice`] kept, once.
pub fn take_pending_notice() -> Option<String> {
    PENDING.lock().ok().and_then(|mut pending| pending.take())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> crate::test_scratch_dir::ScratchDir {
        crate::test_scratch_dir::ScratchDir::new("crash-trace", label)
    }

    #[test]
    fn only_the_word_off_turns_it_off() {
        assert!(is_off(Some(OsStr::new("off"))));
        assert!(!is_off(Some(OsStr::new("1"))));
        assert!(!is_off(None));
    }

    #[test]
    fn a_fixed_buffer_drops_what_does_not_fit() {
        let mut bytes = [0; 4];
        let mut fixed = Fixed {
            bytes: &mut bytes,
            len: 0,
        };
        assert!(write!(fixed, "{}", "too long").is_ok());
        assert_eq!(fixed.len, 4);
        assert_eq!(&bytes, b"too ");
    }

    #[test]
    fn a_record_name_sorts_by_time() {
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        tm.tm_year = 126;
        tm.tm_mon = 10;
        tm.tm_mday = 2;
        tm.tm_hour = 9;
        let mut buffer = [0; 64];
        let name = record_name(&tm, &mut buffer).to_str().unwrap().to_string();
        assert!(name.starts_with("crash-20261102T090000Z-"), "{name}");
        assert!(name.ends_with(".log"), "{name}");
    }

    #[test]
    fn a_record_is_told_of_once_with_the_home_replaced() {
        let dir = scratch("notice");
        std::fs::write(dir.join("crash-20261102T090000Z-7.log"), "x").unwrap();
        let home = dir.parent().unwrap().as_os_str();
        let line = new_crash_notice(&dir, Some(home)).unwrap();
        assert!(line.contains("~/"), "{line}");
        assert!(line.contains("crash-20261102T090000Z-7.log"), "{line}");
        assert_eq!(new_crash_notice(&dir, Some(home)), None);
        std::fs::write(dir.join("crash-20261102T090001Z-8.log"), "x").unwrap();
        assert!(new_crash_notice(&dir, None).unwrap().contains("-8.log"));
    }

    #[test]
    fn no_record_says_nothing_and_makes_nothing() {
        let dir = scratch("none");
        assert_eq!(new_crash_notice(&dir, None), None);
        assert!(!dir.join(NOTICED).exists());
    }

    #[test]
    fn only_the_newest_records_stay_and_only_records_go() {
        let dir = scratch("keep");
        for second in 0..(KEEP + 3) {
            std::fs::write(dir.join(format!("crash-20261102T0900{second:02}Z-1.log")), "x").unwrap();
        }
        std::fs::write(dir.join("main-410d926fb96fb107.log"), "x").unwrap();
        new_crash_notice(&dir, None).unwrap();
        assert_eq!(records_in(&dir).len(), KEEP + 3 - 3);
        assert!(records_in(&dir)
            .last()
            .unwrap()
            .contains(&format!("0900{:02}Z", KEEP + 2)));
        assert!(dir.join("main-410d926fb96fb107.log").exists());
    }

    #[test]
    fn a_link_with_a_records_name_is_not_a_record() {
        let dir = scratch("link");
        std::os::unix::fs::symlink("/etc/passwd", dir.join("crash-20261102T090000Z-1.log")).unwrap();
        assert_eq!(new_crash_notice(&dir, None), None);
        assert!(dir.join("crash-20261102T090000Z-1.log").symlink_metadata().is_ok());
    }

    #[test]
    fn the_main_windows_log_sits_with_the_module_windows_logs() {
        assert_eq!(
            log_dir(None, Some(OsStr::new("/home/ann"))).unwrap(),
            Path::new("/home/ann/.local/state/eitri/log")
        );
    }

    #[test]
    fn a_second_window_of_a_project_keeps_a_log_of_its_own() {
        let dir = scratch("windows");
        let (_, first) = open_main_log(&dir, Path::new("/some/project")).unwrap();
        let (_, second) = open_main_log(&dir, Path::new("/some/project")).unwrap();
        let name = |path: &PathBuf| path.file_name().unwrap().to_str().unwrap().to_string();
        assert!(
            name(&first).starts_with("main-") && name(&first).ends_with(".log"),
            "{first:?}"
        );
        assert!(name(&second).ends_with("-2.log"), "{second:?}");
        assert_ne!(first, second);
    }
}
