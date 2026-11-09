//! `:EitriTutor`: an interactive tutorial of the window's keys in the window's own nvim, built on nvim's tutor
//! framework (`:Tutor`, `runtime/tutor/<lang>/<name>.tutor` with its `.tutor.json` checks).
//!
//! **The lessons are compiled into the binary.** At the window's first editor session a private per-instance
//! directory is made for them ([`TutorDir`]: under `$XDG_RUNTIME_DIR` when that is the user's own, else under a
//! `TMPDIR` nobody else can rename entries in), and [`INSTALL_TUTOR_LUA`] defines `:EitriTutor` with the texts as
//! arguments. The lessons name the window's prefix as `{prefix}` ([`prefix_text`]) and are written for the prefix
//! the window has, not only `Ctrl+b`; the keys after it are the stock ones. The command writes any file missing from (or not whole in) that directory (laid out as an nvim runtime directory:
//! `tutor/en/eitri.tutor`, `tutor/zh/eitri.tutor`, each with its `.json`) after checking the directory is still
//! the user's own and closed to everyone else, puts it on `'runtimepath'` only while nvim's tutor looks for the
//! lessons, and takes it off again. So the lessons a window shows are always the ones written for the key table of
//! the binary showing them, no package, tarball or installer carries a file for them, nothing is written into the
//! user's own nvim configuration or runtime directories, and files something deleted in the meantime come back. The
//! directory goes with the window ([`TutorDir::cleanup`]); a window that died without cleaning up leaves it to the
//! next window's sweep.
//!
//! **The command never replaces one of the user's.** Like a plugin's declared commands, it is defined only after
//! VimEnter (the snippet answers `early` before that and the window asks again), only when neither a global nor a
//! buffer-local command of that name exists, and a name it could not take is logged once:
//! `[tutor] :EitriTutor is already defined; not replacing it`.
//!
//! **Only the window that embeds nvim has it.** A companion panel is beside the user's own nvim, whose commands
//! Eitri leaves alone beyond the glue that panel needs, and the lessons are about the window's grid, which a
//! companion panel does not have.

use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use rmpv::Value;

use crate::compositor::mode::Mode;
use crate::editor_rpc::EditorRpc;
use crate::nvim_rpc::Pending;

pub mod prefix_text;

/// The command the snippet defines.
pub const COMMAND: &str = "EitriTutor";

/// The name `tutor#TutorCmd` looks the lessons up by (the `eitri` of `tutor/<lang>/eitri.tutor`); the lessons are
/// opened by `:EitriTutor`, and `:Tutor eitri` is not an entry point.
pub const TUTORIAL: &str = "eitri";

/// Defines `:EitriTutor`, which writes the lessons and opens them; one table argument, [`install_opts`].
pub const INSTALL_TUTOR_LUA: &str = include_str!("install.lua");

/// The prefix the lessons are installed for until the window says which it has: the stock one.
pub const DEFAULT_PREFIX: &str = "Ctrl+b";

/// The English lessons, the text every translation follows. This and the three below are the sources, with
/// [`prefix_text::PLACEHOLDER`] where they mean the prefix; [`files`] gives what is installed.
pub const LESSONS_EN: &str = include_str!("lessons/en/eitri.tutor");
/// The checks of the English lessons: which lines are exercises and what each should read when done.
pub const CHECKS_EN: &str = include_str!("lessons/en/eitri.tutor.json");
/// The Simplified Chinese lessons. nvim's own Chinese tutorials live under `tutor/zh/`, the directory its
/// language choice gives every `zh_*` locale.
pub const LESSONS_ZH: &str = include_str!("lessons/zh/eitri.tutor");
/// The checks of the Chinese lessons, with the hash of the English lessons they were translated from.
pub const CHECKS_ZH: &str = include_str!("lessons/zh/eitri.tutor.json");

/// Every file of the runtime directory, by its path inside it, as the sources.
pub const FILES: [(&str, &str); 4] = [
    ("tutor/en/eitri.tutor", LESSONS_EN),
    ("tutor/en/eitri.tutor.json", CHECKS_EN),
    ("tutor/zh/eitri.tutor", LESSONS_ZH),
    ("tutor/zh/eitri.tutor.json", CHECKS_ZH),
];

/// The compositor lessons use the same runtime names, so nvim's language selection and command stay the same.
pub const COMPOSITOR_FILES: [(&str, &str); 4] = [
    ("tutor/en/eitri.tutor", include_str!("lessons/en/compositor.tutor")),
    (
        "tutor/en/eitri.tutor.json",
        include_str!("lessons/en/compositor.tutor.json"),
    ),
    ("tutor/zh/eitri.tutor", include_str!("lessons/zh/compositor.tutor")),
    (
        "tutor/zh/eitri.tutor.json",
        include_str!("lessons/zh/compositor.tutor.json"),
    ),
];

/// The window's lessons for its prefix and resolved mode; an unknown mode uses Eitri's own tiling lessons.
pub fn files(prefix: &str, mode: Option<Mode>) -> Vec<(&'static str, String)> {
    let sources = match mode {
        Some(Mode::Compositor) => &COMPOSITOR_FILES,
        Some(Mode::Eitri) | None => &FILES,
    };
    sources
        .iter()
        .map(|(rel, text)| {
            let text = if rel.ends_with(".json") {
                prefix_text::substitute_checks(text, prefix)
            } else {
                prefix_text::substitute(text, prefix)
            };
            (*rel, text)
        })
        .collect()
}

/// `<base>/nv-tu-<pid>-<uuid>`, swept like the other per-instance directories.
const DIR_PREFIX: &str = "nv-tu-";
/// No socket lives here; the sweep's socket probe finds nothing and the pid alone decides. The namespace caveat
/// `instance_dir::sweep_stale_instance_dirs` documents applies: a live window in another pid namespace sharing the
/// same base directory could lose its lessons' directory, and its next `:EitriTutor` then says it could not write
/// them, rather than anything crashing.
const NO_SOCKET: &str = "none";

/// Characters `'runtimepath'` or `globpath()` read as something other than themselves even when escaped the way
/// the snippet escapes a comma: a path holding one would not be found on `'runtimepath'` again, and nvim's tutor
/// would not find the lessons in it.
pub const SPECIAL_IN_RUNTIMEPATH: &[u8] = b"\\[]*?{}$~";

/// The lessons' directory, laid out as a runtime directory nvim can be pointed at. Only the directory is made
/// here: the files are written into it by the command itself, each time it runs and finds one missing or cut short.
pub struct TutorDir {
    dir: PathBuf,
}

impl TutorDir {
    /// Under `$XDG_RUNTIME_DIR` or `TMPDIR` ([`TutorDir::in_base`]), after sweeping the directories of windows
    /// that are gone. `None`, logged, when it cannot be made: the window then has no `:EitriTutor`.
    pub fn new() -> Option<TutorDir> {
        let runtime = agent::state_dirs::runtime_dir();
        // The snippet takes only an absolute path, and a relative `TMPDIR` would hand it a relative one.
        let tmp = match std::path::absolute(std::env::temp_dir()) {
            Ok(tmp) => tmp,
            Err(e) => {
                eprintln!(
                    "[tutor] could not make the temporary directory {} absolute: {e} -- no :{COMMAND}",
                    std::env::temp_dir().display()
                );
                return None;
            }
        };
        match TutorDir::in_base(runtime.as_deref(), &tmp) {
            Ok(dir) => Some(dir),
            Err(e) => {
                eprintln!("[tutor] no place for the lessons: {e} -- no :{COMMAND}");
                None
            }
        }
    }

    /// The directory the lessons' directory is made in. The directory goes on nvim's `'runtimepath'` while the
    /// command runs, so whoever could replace it could have nvim run their files: `runtime_dir` when it is an
    /// absolute real directory of the effective user with no group or other bit (nobody else can enter it, and
    /// nothing ages files out of it), else `tmp` when nobody else can rename entries in it
    /// (`agent::private_fs::require_safe_parent`), else an error.
    pub fn base(runtime_dir: Option<&Path>, tmp: &Path) -> io::Result<PathBuf> {
        crate::instance_dir::private_base(runtime_dir, tmp, "the tutorial's lessons")
    }

    /// Sweeps the stale lessons' directories in [`TutorDir::base`], then makes this window's there.
    pub fn in_base(runtime_dir: Option<&Path>, tmp: &Path) -> io::Result<TutorDir> {
        let base = TutorDir::base(runtime_dir, tmp)?;
        crate::instance_dir::sweep_stale_dirs_in_bases(&base, tmp, DIR_PREFIX, NO_SOCKET, "tutor");
        TutorDir::in_dir(&base)
    }

    /// A fresh directory under `base`, exactly 0700 whatever the umask. `base` is taken as it is: callers other
    /// than a test go through [`TutorDir::in_base`]. A path with a character of [`SPECIAL_IN_RUNTIMEPATH`] is
    /// refused (`InvalidInput`) before anything is made.
    pub fn in_dir(base: &Path) -> io::Result<TutorDir> {
        use std::os::unix::ffi::OsStrExt;
        let dir = crate::instance_dir::instance_dir_path(base, DIR_PREFIX);
        if let Some(c) = dir
            .as_os_str()
            .as_bytes()
            .iter()
            .find(|b| SPECIAL_IN_RUNTIMEPATH.contains(b))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} holds {:?}, which nvim's 'runtimepath' does not take literally",
                    dir.display(),
                    char::from(*c)
                ),
            ));
        }
        crate::instance_dir::create_private_dir(&dir)?;
        Ok(TutorDir { dir })
    }

    /// The directory that goes on `'runtimepath'` while the command runs.
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Called from the window's close handler, since GTK does not reliably run `Drop` before the process exits.
    pub fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Drop for TutorDir {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Asks the editor to define `:EitriTutor` for `dir`, handing it the lessons to write there when the command runs,
/// written for `prefix` and the window's mode.
pub fn install(rpc: &dyn EditorRpc, dir: &Path, prefix: &str, mode: Option<Mode>) -> Pending {
    rpc.exec_lua(INSTALL_TUTOR_LUA, vec![install_opts(dir, prefix, mode)])
}

/// The snippet's one argument: `{dir, uid, files}`, the directory as bytes, the effective uid the directory and
/// everything in it must belong to, and each file's text by its path inside the directory, with `prefix` (as a
/// person reads it, `Ctrl+a`) where the lessons name the prefix. The snippet writes what it is given.
pub fn install_opts(dir: &Path, prefix: &str, mode: Option<Mode>) -> Value {
    use std::os::unix::ffi::OsStrExt;
    let files = files(prefix, mode)
        .into_iter()
        .map(|(rel, text)| (Value::from(rel), Value::from(text)))
        .collect();
    Value::Map(vec![
        (Value::from("dir"), Value::Binary(dir.as_os_str().as_bytes().to_vec())),
        (Value::from("uid"), Value::from(agent::private_fs::euid())),
        (Value::from("files"), Value::Map(files)),
    ])
}

/// What the snippet answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installed {
    /// Asked before VimEnter: nothing changed, ask again.
    Early,
    /// Done; the names it left alone because the user (or nvim) already had them.
    Done { skipped: Vec<String> },
}

fn field<'a>(answer: &'a Value, key: &str) -> Option<&'a Value> {
    answer
        .as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// Reads the snippet's answer.
pub fn read_installed(answer: &Value) -> Result<Installed, String> {
    let ok = field(answer, "ok")
        .and_then(Value::as_bool)
        .ok_or_else(|| "the editor's answer has no ok".to_string())?;
    if !ok {
        if field(answer, "early").and_then(Value::as_bool) == Some(true) {
            return Ok(Installed::Early);
        }
        return Err("the editor did not define the command".to_string());
    }
    // An empty Lua table comes back as an empty array or an empty map, whichever nvim makes of it.
    let skipped = match field(answer, "skipped") {
        Some(Value::Array(names)) => names
            .iter()
            .map(|v| match v {
                Value::String(s) => Ok(String::from_utf8_lossy(s.as_bytes()).into_owned()),
                _ => Err("the editor's answer has a skipped name that is not text".to_string()),
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(Value::Map(m)) if m.is_empty() => Vec::new(),
        _ => return Err("the editor's answer has no skipped list".to_string()),
    };
    Ok(Installed::Done { skipped })
}

/// How often an answer still on its way is looked for.
pub const POLL: Duration = Duration::from_millis(50);
/// How long one answer is waited for before the call is given up (logged; the next editor session asks again).
pub const ANSWER_WAIT: Duration = Duration::from_secs(5);
/// How soon an `early` answer is followed by the next call.
pub const EARLY_AGAIN: Duration = Duration::from_millis(250);
/// How long `early` answers are followed up before the window gives up on this session.
pub const EARLY_GIVE_UP: Duration = Duration::from_secs(30);

struct Out {
    target: u64,
    pending: Option<Pending>,
    /// When the answer stops being waited for, or, with no call out, when the next one goes.
    at: Instant,
    /// When this session was first asked, for [`EARLY_GIVE_UP`].
    since: Instant,
}

/// One window's `:EitriTutor`: its directory made at the first editor session, installed once per nvim session, and
/// polled by the window only while a call is out. Toolkit-free: the caller runs [`TutorInstaller::poll`] at the
/// time each call returns.
pub struct TutorInstaller {
    make_dir: Box<dyn Fn() -> Option<TutorDir>>,
    dir: Option<TutorDir>,
    dir_failed: bool,
    prefix: String,
    mode: Option<Mode>,
    editor: Option<Rc<dyn EditorRpc>>,
    installed_for: Option<u64>,
    out: Option<Out>,
    reported: Vec<String>,
}

impl Default for TutorInstaller {
    fn default() -> Self {
        TutorInstaller::new()
    }
}

impl TutorInstaller {
    /// Makes its directory ([`TutorDir::new`]) when first needed.
    pub fn new() -> TutorInstaller {
        TutorInstaller::with_dir(Box::new(TutorDir::new))
    }

    /// With the directory made by `make_dir` when first needed (a test's own).
    pub fn with_dir(make_dir: Box<dyn Fn() -> Option<TutorDir>>) -> TutorInstaller {
        TutorInstaller {
            make_dir,
            dir: None,
            dir_failed: false,
            prefix: DEFAULT_PREFIX.to_string(),
            mode: None,
            editor: None,
            installed_for: None,
            out: None,
            reported: Vec::new(),
        }
    }

    /// The window's prefix as a person reads it (`KeySpec::human`), which the lessons name wherever they mean it.
    /// Calls to nvim sessions made after this carry it; the window sets it when its config is read, before any
    /// editor session reports.
    pub fn set_prefix(&mut self, human: String) {
        self.prefix = human;
    }

    /// The mode resolved at window startup, set before the first editor session and kept across nvim restarts.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = Some(mode);
    }

    /// The window's editor handle.
    pub fn set_editor(&mut self, editor: Rc<dyn EditorRpc>) {
        self.editor = Some(editor);
    }

    /// The lessons' directory, once made.
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_ref().map(TutorDir::path)
    }

    /// The command names the window logged as already defined.
    pub fn reported(&self) -> &[String] {
        &self.reported
    }

    /// Removes the lessons' directory (the window's close).
    pub fn cleanup(&self) {
        if let Some(dir) = &self.dir {
            dir.cleanup();
        }
    }

    /// The editor may have a new nvim session ready (its feeds report at VimEnter): installs there unless that
    /// session already has it or a call to it is out. Returns when to [`poll`](Self::poll).
    pub fn editor_session(&mut self, now: Instant) -> Option<Instant> {
        let target = self.editor.as_ref().and_then(|e| e.target());
        let Some(target) = target else {
            return self.next_poll(now);
        };
        if self.installed_for == Some(target) || self.out.as_ref().is_some_and(|o| o.target == target) {
            return self.next_poll(now);
        }
        if self.editor.as_ref().is_some_and(|e| e.attached()) {
            // Eitri adds no tutor to a taken-over nvim: nothing of the window's own stays in the person's nvim.
            eprintln!("[tutor] :{COMMAND} is not offered in a taken-over nvim; nothing is installed there");
            self.out = None;
            self.installed_for = Some(target);
            return None;
        }
        self.out = None;
        self.send(target, now, now)
    }

    /// Takes an answer that arrived, or gives one up, or sends the next call after an `early`. Returns when to
    /// poll again; `None` once nothing is out.
    pub fn poll(&mut self, now: Instant) -> Option<Instant> {
        let out = self.out.as_mut()?;
        let Some(pending) = &out.pending else {
            // Waiting to ask again after an `early`.
            if now < out.at {
                return Some(out.at);
            }
            let (target, since) = (out.target, out.since);
            self.out = None;
            if self.editor.as_ref().and_then(|e| e.target()) != Some(target) {
                // That session is gone; the next one reports itself.
                return None;
            }
            return self.send(target, since, now);
        };
        let Some(reply) = pending.try_take() else {
            if now >= out.at {
                eprintln!("[tutor] the editor did not answer within {ANSWER_WAIT:?}; no :{COMMAND} in this session");
                self.out = None;
                return None;
            }
            return Some((now + POLL).min(out.at));
        };
        let (target, since) = (out.target, out.since);
        let answer = match reply {
            Ok(value) => read_installed(&value),
            Err(e) => Err(format!("the editor could not be asked: {e:?}")),
        };
        match answer {
            Ok(Installed::Early) if now.saturating_duration_since(since) < EARLY_GIVE_UP => {
                let at = now + EARLY_AGAIN;
                self.out = Some(Out {
                    target,
                    pending: None,
                    at,
                    since,
                });
                Some(at)
            }
            Ok(Installed::Early) => {
                eprintln!("[tutor] the editor stayed before VimEnter for {EARLY_GIVE_UP:?}; no :{COMMAND}");
                self.out = None;
                None
            }
            Ok(Installed::Done { skipped }) => {
                self.out = None;
                self.installed_for = Some(target);
                for name in skipped {
                    if !self.reported.contains(&name) {
                        eprintln!("[tutor] :{name} is already defined; not replacing it");
                        self.reported.push(name);
                    }
                }
                None
            }
            Err(why) => {
                eprintln!("[tutor] :{COMMAND} could not be installed: {why}");
                self.out = None;
                None
            }
        }
    }

    fn next_poll(&self, now: Instant) -> Option<Instant> {
        self.out.as_ref().map(|o| {
            if o.pending.is_some() {
                o.at.min(now + POLL)
            } else {
                o.at
            }
        })
    }

    fn send(&mut self, target: u64, since: Instant, now: Instant) -> Option<Instant> {
        if self.dir.is_none() && !self.dir_failed {
            self.dir = (self.make_dir)();
            self.dir_failed = self.dir.is_none();
        }
        let (Some(dir), Some(editor)) = (&self.dir, &self.editor) else {
            return None;
        };
        let pending = install(&**editor, dir.path(), &self.prefix, self.mode);
        self.out = Some(Out {
            target,
            pending: Some(pending),
            at: now + ANSWER_WAIT,
            since,
        });
        Some(now + POLL)
    }
}

#[cfg(test)]
mod tests;
