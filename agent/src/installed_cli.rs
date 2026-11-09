//! Whether a program an installer put in the user's home may be run: the part of finding `claude` and `codex` that
//! is the same for both.
//!
//! A window started from a desktop launcher or a session manager has that manager's `PATH` (systemd's user manager
//! gives `/usr/local/bin:/usr/bin`), and the `~/.local/bin` the Claude Code and Codex installers put their programs in
//! is only on the `PATH` of the person's login shell. The lookups (`claude_sidecar::cli_locate`,
//! `codex::locate`) then look in those places themselves, and [`inspect_installed`] decides whether what is there may
//! be run: a regular file only its owner, this user or root, can change, reached through directories only their
//! owners can change, every link followed ([`walk`]), and one this process may execute. [`npm_prefix`] is where
//! `npm install -g` puts programs when the user moved npm's prefix into the home.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// The most symbolic links [`walk`] follows for one candidate, as the kernel's own limit.
const MAX_LINKS: usize = 40;

/// What [`walk`] found at one install place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inspected {
    /// Nothing there (or a path through a file).
    Absent,
    /// Something is there that is not handed over, and why.
    Refused(String),
    /// A regular file only its owner can change, reached through directories only their owners can change.
    Usable,
}

/// The most of an npm configuration file read; a longer one counts as unreadable.
pub const NPMRC_CAP: u64 = 64 * 1024;

/// A configuration file of the user's own, read so that nothing there can hang or grow the read: opened without
/// blocking (a FIFO answers at once), a regular file owned by this process's effective user, at most `cap` bytes
/// long. Anything else is `None`. A link is followed: what it reaches must pass the same checks.
/// User-edited configuration may be group-writable; the installed program and its ancestors are checked separately.
pub fn read_user_file(path: &Path, cap: u64) -> Option<String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.uid() != crate::private_fs::euid() || meta.len() > cap {
        return None;
    }
    let mut text = String::new();
    file.take(cap.saturating_add(1)).read_to_string(&mut text).ok()?;
    (text.len() as u64 <= cap).then_some(text)
}

/// npm's global prefix as the user set it: `NPM_CONFIG_PREFIX` (npm reads its variables in either case), else the
/// last `prefix` of the user configuration file before any `[section]` -- `NPM_CONFIG_USERCONFIG` when that is an
/// absolute path, else `~/.npmrc` -- read through `read_file`. `~/` and `${HOME}/` at the start are the home, as npm
/// expands them; a value that is not then absolute is no prefix.
pub fn npm_prefix(
    env: &dyn Fn(&str) -> Option<OsString>,
    home: Option<&Path>,
    read_file: &dyn Fn(&Path) -> Option<String>,
) -> Option<PathBuf> {
    let set = |names: [&str; 2]| {
        names.iter().find_map(|name| {
            env(name)
                .map(|v| v.to_string_lossy().trim().to_string())
                .filter(|v| !v.is_empty())
        })
    };
    let raw = set(["NPM_CONFIG_PREFIX", "npm_config_prefix"]).or_else(|| {
        let userconfig = set(["NPM_CONFIG_USERCONFIG", "npm_config_userconfig"]);
        let file = match userconfig {
            Some(named) => Some(PathBuf::from(named)).filter(|p| p.is_absolute())?,
            None => home?.join(".npmrc"),
        };
        npmrc_prefix(&read_file(&file)?)
    })?;
    let expanded = if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("${HOME}/")) {
        home?.join(rest.trim_start_matches('/'))
    } else {
        PathBuf::from(raw)
    };
    expanded.is_absolute().then_some(expanded)
}

/// The `prefix` an `.npmrc` sets, read as npm's `ini` reads it: a leading byte-order mark is dropped; a line starting
/// with `;` or `#` is a comment; keys after the first `[section]` belong to that section and are not the top-level
/// one; a later `prefix` replaces an earlier one; a quoted value is taken inside its quotes, an unquoted one ends at
/// the first `;` or `#` not escaped with `\`.
pub fn npmrc_prefix(text: &str) -> Option<String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            break;
        }
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "prefix" {
            continue;
        }
        let value = ini_value(value.trim());
        if !value.is_empty() {
            found = Some(value);
        }
    }
    found
}

/// One `ini` value: inside its quotes when quoted, else up to the first unescaped `;` or `#`, with `\;`, `\#` and
/// `\\` read as the character, trimmed.
fn ini_value(value: &str) -> String {
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return value[1..value.len() - 1].to_string();
        }
    }
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(next @ ('\\' | ';' | '#')) => out.push(next),
                Some(next) => {
                    out.push('\\');
                    out.push(next);
                }
                None => out.push('\\'),
            },
            ';' | '#' => break,
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

/// The most of a file's start read: its `#!` line, and for a wrapper script the `exec` line after it.
const START_CAP: usize = 4096;

/// `env`'s options that take the next word as their value (GNU's and BSD's): that word is no program.
const ENV_VALUE_OPTIONS: [&str; 7] = ["-u", "--unset", "-C", "--chdir", "-a", "--argv0", "-P"];

/// The program a script's first line asks `env` to find on the `PATH` (`#!/usr/bin/env node`, `#!/bin/env -S node
/// --flag`, `#!/usr/bin/env -u NAME node`), past `env`'s options, their values and `NAME=value` words, else `None`
/// (no `#!`, or an interpreter named by its path, which the kernel finds itself).
pub fn env_interpreter(start: &[u8]) -> Option<String> {
    let rest = start.strip_prefix(b"#!")?;
    let line = rest.split(|b| *b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let mut words = line.split_whitespace();
    let program = words.next()?;
    if program != "/usr/bin/env" && program != "/bin/env" {
        return None;
    }
    while let Some(word) = words.next() {
        if word == "--" {
            return words.next().map(str::to_string);
        }
        if ENV_VALUE_OPTIONS.contains(&word) {
            words.next();
            continue;
        }
        if word.starts_with("--") || word.contains('=') {
            continue;
        }
        if let Some(flags) = word.strip_prefix('-') {
            // A cluster such as `-iu NAME`: a value option last in it takes the next word; one before its end
            // (`-uNAME`) has its value attached.
            if !flags.is_empty() && flags.find(|c| "uCaP".contains(c)) == Some(flags.len() - 1) {
                words.next();
            }
            continue;
        }
        return Some(word.to_string());
    }
    None
}

/// The absolute program a wrapper script runs with `exec` (`exec "/home/u/.claude/local/node_modules/.bin/claude"
/// "$@"`, the old local install's launcher), the first such line after its `#!` line; quotes around the path are
/// dropped. `None` for anything else.
pub fn wrapper_exec(start: &[u8]) -> Option<PathBuf> {
    let text = std::str::from_utf8(start).ok().or_else(|| {
        // A start cut inside a character: what comes before it.
        let valid = std::str::from_utf8(start).err()?.valid_up_to();
        std::str::from_utf8(&start[..valid]).ok()
    })?;
    if !text.starts_with("#!") {
        return None;
    }
    text.lines().skip(1).find_map(|line| {
        let rest = line.trim_start().strip_prefix("exec")?;
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        let word = rest.split_whitespace().next()?;
        let word = word.trim_matches(|c| c == '"' || c == '\'');
        let path = PathBuf::from(word);
        path.is_absolute().then_some(path)
    })
}

/// The start of `path`, at most [`START_CAP`] bytes, read without blocking; `None` when it cannot be read.
pub fn read_start(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut start = Vec::new();
    file.take(START_CAP as u64).read_to_end(&mut start).ok()?;
    Some(start)
}

/// Why `path`, a script whose start is `start`, would not run from this window: its first line asks `env` for a
/// program that is not on `path_var` (the `PATH` the program inherits; unset is `env`'s own default, `/bin:/usr/bin`).
/// Only absolute `PATH` entries count, and `runnable` judges each file. Nothing is run.
pub fn interpreter_refusal(
    path: &Path,
    start: Option<&[u8]>,
    path_var: Option<&std::ffi::OsStr>,
    runnable: &dyn Fn(&Path) -> bool,
) -> Option<String> {
    let interpreter = env_interpreter(start?)?;
    let found = if interpreter.contains('/') {
        runnable(Path::new(&interpreter))
    } else {
        std::env::split_paths(path_var.unwrap_or(std::ffi::OsStr::new("/bin:/usr/bin")))
            .filter(|dir| dir.is_absolute())
            .any(|dir| runnable(&dir.join(&interpreter)))
    };
    (!found).then(|| {
        format!(
            "{} starts with `env {interpreter}`, and {interpreter} is not on this window's PATH",
            path.display()
        )
    })
}

/// What one entry is, as `lstat` sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Link,
    Other,
}

/// One entry's type, owner and mode, as `lstat` gives them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    pub uid: u32,
    pub mode: u32,
}

/// Why `entry` at `path` may not be on the way to a `claude` that is handed over: owned by someone other than
/// `me` or root, or writable by its group or everyone. A sticky bit does not make a writable directory acceptable:
/// it keeps others from replacing a name that is there, not from creating one that is not yet.
fn changeable_by_others(path: &Path, entry: Entry, me: u32) -> Option<String> {
    if entry.uid != me && entry.uid != 0 {
        return Some(format!("{} is owned by uid {}", path.display(), entry.uid));
    }
    if entry.mode & 0o022 != 0 {
        return Some(format!("{} can be written by other users", path.display()));
    }
    None
}

/// Follows `path` (absolute) one name at a time with `lstat` and `readlink`, every link included, and judges what
/// it reaches: every directory on the way, the root included, and the file at the end must each be owned by `me`
/// or root and writable by neither group nor others, and the end must be a regular file. A name that does not
/// exist, or a path through a file, is [`Inspected::Absent`]; any other error, a directory or a special file at
/// the end, or more than [`MAX_LINKS`] links is [`Inspected::Refused`]. Whether the file may be executed is the
/// caller's to ask.
pub fn walk(
    path: &Path,
    me: u32,
    lstat: &dyn Fn(&Path) -> std::io::Result<Entry>,
    readlink: &dyn Fn(&Path) -> std::io::Result<PathBuf>,
) -> Inspected {
    if !path.is_absolute() {
        return Inspected::Refused(format!("{} is not an absolute path", path.display()));
    }
    let root = Path::new("/");
    match lstat(root) {
        Ok(entry) => {
            if let Some(why) = changeable_by_others(root, entry, me) {
                return Inspected::Refused(why);
            }
        }
        Err(e) => return Inspected::Refused(format!("/: {e}")),
    }
    // The names still to walk, the next one last.
    let mut pending: Vec<OsString> = Vec::new();
    push_components(&mut pending, path);
    let mut at = PathBuf::from("/");
    let mut links = 0;
    while let Some(name) = pending.pop() {
        if name == "." {
            continue;
        }
        if name == ".." {
            at.pop();
            continue;
        }
        let next = at.join(&name);
        let entry = match lstat(&next) {
            Ok(entry) => entry,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Inspected::Absent,
            Err(e) => return Inspected::Refused(format!("{}: {e}", next.display())),
        };
        match entry.kind {
            Kind::Link => {
                links += 1;
                if links > MAX_LINKS {
                    return Inspected::Refused(format!("{}: too many links", path.display()));
                }
                let target = match readlink(&next) {
                    Ok(target) => target,
                    Err(e) => return Inspected::Refused(format!("{}: {e}", next.display())),
                };
                if target.is_absolute() {
                    at = PathBuf::from("/");
                }
                push_components(&mut pending, &target);
            }
            Kind::Dir => {
                if let Some(why) = changeable_by_others(&next, entry, me) {
                    return Inspected::Refused(why);
                }
                if pending.is_empty() {
                    return Inspected::Refused(format!("{} is a directory", next.display()));
                }
                at = next;
            }
            Kind::File => {
                if !pending.is_empty() {
                    return Inspected::Absent;
                }
                return match changeable_by_others(&next, entry, me) {
                    Some(why) => Inspected::Refused(why),
                    None => Inspected::Usable,
                };
            }
            Kind::Other => return Inspected::Refused(format!("{} is not a regular file", next.display())),
        }
    }
    Inspected::Refused(format!("{} is a directory", path.display()))
}

/// Pushes `path`'s names onto `pending` so the first is popped first; the root and `.` add nothing.
fn push_components(pending: &mut Vec<OsString>, path: &Path) {
    let names: Vec<OsString> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_owned()),
            Component::ParentDir => Some(OsString::from("..")),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
        })
        .collect();
    pending.extend(names.into_iter().rev());
}

/// A regular file this process's effective user may execute.
pub(crate) fn runnable_file(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // As the effective user, as `execve` checks and as `walk` judges ownership. SAFETY: `c_path` is a NUL-terminated
    // string that outlives the call, which only reads it.
    unsafe { libc::faccessat(libc::AT_FDCWD, c_path.as_ptr(), libc::X_OK, libc::AT_EACCESS) == 0 }
}

/// `lstat` as [`walk`] reads it.
pub(crate) fn lstat_entry(path: &Path) -> std::io::Result<Entry> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path)?;
    let file_type = meta.file_type();
    let kind = if file_type.is_symlink() {
        Kind::Link
    } else if file_type.is_dir() {
        Kind::Dir
    } else if file_type.is_file() {
        Kind::File
    } else {
        Kind::Other
    };
    Ok(Entry {
        kind,
        uid: meta.uid(),
        mode: meta.mode(),
    })
}

/// [`walk`] over this file system as this process's effective user, then whether this process may execute the
/// file.
pub fn inspect_installed(path: &Path) -> Inspected {
    match walk(path, crate::private_fs::euid(), &lstat_entry, &|p| {
        std::fs::read_link(p)
    }) {
        Inspected::Usable if !runnable_file(path) => {
            Inspected::Refused(format!("{} cannot be executed by this user", path.display()))
        }
        other => other,
    }
}

/// [`inspect_installed`], then [`interpreter_refusal`] against `path_var`, the `PATH` the program will inherit: what
/// a window may hand over.
pub fn inspect_for_window(path: &Path, path_var: Option<&std::ffi::OsStr>) -> Inspected {
    match inspect_installed(path) {
        Inspected::Usable => match start_refusal(path, path_var, &read_start, &runnable_file) {
            Some(why) => Inspected::Refused(why),
            None => Inspected::Usable,
        },
        other => other,
    }
}

/// [`interpreter_refusal`] for `path`, and, when `path` is a wrapper whose `exec` runs an absolute program
/// ([`wrapper_exec`]), for that program too (one level: the old local install's `~/.claude/local/claude` runs
/// `node_modules/.bin/claude`, a `#!/usr/bin/env node` script).
pub fn start_refusal(
    path: &Path,
    path_var: Option<&std::ffi::OsStr>,
    read: &dyn Fn(&Path) -> Option<Vec<u8>>,
    runnable: &dyn Fn(&Path) -> bool,
) -> Option<String> {
    let start = read(path);
    if let Some(why) = interpreter_refusal(path, start.as_deref(), path_var, runnable) {
        return Some(why);
    }
    let target = wrapper_exec(start.as_deref()?)?;
    interpreter_refusal(&target, read(&target).as_deref(), path_var, runnable)
        .map(|why| format!("{} runs {}, and {why}", path.display(), target.display()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsStr;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let owned: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |name| owned.iter().find(|(k, _)| k == name).map(|(_, v)| OsString::from(v))
    }

    fn no_file(_: &Path) -> Option<String> {
        None
    }

    #[test]
    fn npms_prefix_comes_from_its_variable_else_the_last_prefix_line_of_the_users_npmrc() {
        let home = Some(Path::new("/home/u"));
        let npmrc = |text: &'static str| move |p: &Path| (p == Path::new("/home/u/.npmrc")).then(|| text.to_string());
        let unset = env_of(&[]);
        assert_eq!(
            npm_prefix(
                &unset,
                home,
                &npmrc("# prefix=/no\n; prefix=/no\nprefix=/opt/a\nprefix = '/opt/b'\n")
            ),
            Some(PathBuf::from("/opt/b"))
        );
        assert_eq!(
            npm_prefix(&unset, home, &npmrc("prefix=~/.npm-global")),
            Some(PathBuf::from("/home/u/.npm-global"))
        );
        assert_eq!(
            npm_prefix(&unset, home, &npmrc("prefix=${HOME}/.npm")),
            Some(PathBuf::from("/home/u/.npm"))
        );
        // Relative, empty, or another key: no prefix.
        assert_eq!(npm_prefix(&unset, home, &npmrc("prefix=npm-global")), None);
        assert_eq!(npm_prefix(&unset, home, &npmrc("prefix=")), None);
        assert_eq!(npm_prefix(&unset, home, &npmrc("cache=/opt/cache")), None);
        assert_eq!(npm_prefix(&unset, home, &no_file), None);
        // The variable, either case, beats the file; blanks count as unset.
        let set = env_of(&[("NPM_CONFIG_PREFIX", " /opt/env ")]);
        assert_eq!(
            npm_prefix(&set, home, &npmrc("prefix=/opt/a")),
            Some(PathBuf::from("/opt/env"))
        );
        let lower = env_of(&[("npm_config_prefix", "/opt/lower")]);
        assert_eq!(npm_prefix(&lower, home, &no_file), Some(PathBuf::from("/opt/lower")));
        let blank = env_of(&[("NPM_CONFIG_PREFIX", "  ")]);
        assert_eq!(
            npm_prefix(&blank, home, &npmrc("prefix=/opt/a")),
            Some(PathBuf::from("/opt/a"))
        );
        // `~/` with no home: nothing to expand it to.
        assert_eq!(npm_prefix(&unset, None, &|_| Some("prefix=~/.npm".to_string())), None);
    }

    #[test]
    fn an_npmrc_is_read_as_npms_ini_reads_it() {
        let read = |text: &str| npmrc_prefix(text);
        // Comments after an unquoted value, escaped ones kept, quoted values whole.
        assert_eq!(read("prefix=/opt/a ;old one"), Some("/opt/a".into()));
        assert_eq!(read("prefix=/opt/a#x"), Some("/opt/a".into()));
        assert_eq!(read(r"prefix=/opt/a\;b"), Some("/opt/a;b".into()));
        assert_eq!(read("prefix=\"/opt/a ;b\""), Some("/opt/a ;b".into()));
        // A section's keys are not the top-level one; the last top-level one wins.
        assert_eq!(read("prefix=/opt/a\n[scoped]\nprefix=/opt/s\n"), Some("/opt/a".into()));
        assert_eq!(read("[scoped]\nprefix=/opt/s\n"), None);
        assert_eq!(read("prefix=/opt/a\nprefix=/opt/b\n"), Some("/opt/b".into()));
        // A byte-order mark at the start.
        assert_eq!(read("\u{feff}prefix=/opt/bom"), Some("/opt/bom".into()));
        // An empty value is none.
        assert_eq!(read("prefix= ;nothing"), None);
        // `~//abs` is under the home, not at the root.
        let home = Some(Path::new("/home/u"));
        let file = |text: &'static str| move |p: &Path| (p == Path::new("/home/u/.npmrc")).then(|| text.to_string());
        assert_eq!(
            npm_prefix(&env_of(&[]), home, &file("prefix=~//npm")),
            Some(PathBuf::from("/home/u/npm"))
        );
    }

    #[test]
    fn npms_user_configuration_file_can_be_named() {
        let home = Some(Path::new("/home/u"));
        let files = |p: &Path| match p.to_str() {
            Some("/home/u/.npmrc") => Some("prefix=/opt/home".to_string()),
            Some("/etc/u/npmrc") => Some("prefix=/opt/named".to_string()),
            _ => None,
        };
        let named = env_of(&[("NPM_CONFIG_USERCONFIG", "/etc/u/npmrc")]);
        assert_eq!(npm_prefix(&named, home, &files), Some(PathBuf::from("/opt/named")));
        // A relative name is no file at all, not a reason to fall back to ~/.npmrc.
        let relative = env_of(&[("npm_config_userconfig", "npmrc")]);
        assert_eq!(npm_prefix(&relative, home, &files), None);
        assert_eq!(npm_prefix(&env_of(&[]), home, &files), Some(PathBuf::from("/opt/home")));
    }

    /// A FIFO, a file over the cap and a file another user owns are not read; a FIFO does not block.
    #[test]
    fn a_user_file_is_read_only_when_it_is_a_small_regular_file_of_this_user() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;
        let root = crate::test_tmp::TmpDir::new("eitri-npmrc", "reader");
        let good = root.join("good");
        std::fs::write(&good, "prefix=/opt/x\n").unwrap();
        std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o664)).unwrap();
        assert_eq!(read_user_file(&good, NPMRC_CAP).as_deref(), Some("prefix=/opt/x\n"));
        let link = root.join("link");
        std::os::unix::fs::symlink(&good, &link).unwrap();
        assert_eq!(read_user_file(&link, NPMRC_CAP).as_deref(), Some("prefix=/opt/x\n"));
        let fifo = root.join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c` is a NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert_eq!(read_user_file(&fifo, NPMRC_CAP), None);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "a FIFO must not block the read"
        );
        let big = root.join("big");
        std::fs::write(&big, vec![b'#'; NPMRC_CAP as usize + 1]).unwrap();
        assert_eq!(read_user_file(&big, NPMRC_CAP), None);
        // A link to a file root owns (this test does not run as root): another user's file.
        if crate::private_fs::euid() != 0 {
            let other = root.join("other");
            std::os::unix::fs::symlink("/etc/passwd", &other).unwrap();
            assert_eq!(read_user_file(&other, NPMRC_CAP), None);
        }
    }

    #[test]
    fn env_interpreters_are_read_from_the_first_line() {
        assert_eq!(
            env_interpreter(b"#!/usr/bin/env node\nconsole.log(1)"),
            Some("node".into())
        );
        assert_eq!(
            env_interpreter(b"#!/bin/env -S node --no-warnings\n"),
            Some("node".into())
        );
        assert_eq!(env_interpreter(b"#!/usr/bin/env -i FOO=1 bun\n"), Some("bun".into()));
        assert_eq!(env_interpreter(b"#!/bin/sh\nexec x"), None);
        assert_eq!(env_interpreter(b"\x7fELF\x02\x01"), None);
        assert_eq!(env_interpreter(b"#!/usr/bin/env"), None);
    }

    #[test]
    fn env_options_that_take_a_value_are_no_program() {
        assert_eq!(env_interpreter(b"#!/usr/bin/env -u NAME node\n"), Some("node".into()));
        assert_eq!(
            env_interpreter(b"#!/usr/bin/env --unset NAME node\n"),
            Some("node".into())
        );
        assert_eq!(
            env_interpreter(b"#!/usr/bin/env --unset=NAME node\n"),
            Some("node".into())
        );
        assert_eq!(env_interpreter(b"#!/usr/bin/env -uNAME node\n"), Some("node".into()));
        assert_eq!(env_interpreter(b"#!/usr/bin/env -iu NAME node\n"), Some("node".into()));
        assert_eq!(
            env_interpreter(b"#!/usr/bin/env -C /tmp -S node --x\n"),
            Some("node".into())
        );
        assert_eq!(env_interpreter(b"#!/usr/bin/env -- node\n"), Some("node".into()));
        assert_eq!(env_interpreter(b"#!/usr/bin/env - node\n"), Some("node".into()));
        assert_eq!(env_interpreter(b"#!/usr/bin/env -u NAME\n"), None);
    }

    #[test]
    fn a_wrappers_exec_target_is_checked_too() {
        let wrapper = b"#!/bin/bash\nexec \"/home/u/.claude/local/node_modules/.bin/claude\" \"$@\"\n".to_vec();
        assert_eq!(
            wrapper_exec(&wrapper),
            Some(PathBuf::from("/home/u/.claude/local/node_modules/.bin/claude"))
        );
        assert_eq!(
            wrapper_exec(b"#!/bin/sh\nexec node \"$0.js\"\n"),
            None,
            "not an absolute program"
        );
        assert_eq!(wrapper_exec(b"#!/bin/sh\nexecutable=/x\n"), None);
        assert_eq!(wrapper_exec(b"\x7fELF exec /x"), None);
        let files = |p: &Path| match p.to_str() {
            Some("/home/u/.claude/local/claude") => Some(wrapper.clone()),
            Some("/home/u/.claude/local/node_modules/.bin/claude") => Some(b"#!/usr/bin/env node\n".to_vec()),
            _ => None,
        };
        let has_node = |p: &Path| p == Path::new("/usr/bin/node");
        let wrapper_path = Path::new("/home/u/.claude/local/claude");
        let why = start_refusal(wrapper_path, Some(OsStr::new("/usr/local/bin")), &files, &has_node)
            .expect("node is not on that PATH");
        assert!(
            why.contains("runs /home/u/.claude/local/node_modules/.bin/claude") && why.contains("env node"),
            "{why}"
        );
        assert_eq!(
            start_refusal(wrapper_path, Some(OsStr::new("/usr/bin")), &files, &has_node),
            None
        );
    }

    #[test]
    fn a_script_whose_env_interpreter_is_not_on_the_path_is_refused() {
        let runnable = |p: &Path| p == Path::new("/usr/bin/node") || p == Path::new("/opt/n/node");
        let script = Path::new("/home/u/.npm/bin/claude");
        let start: &[u8] = b"#!/usr/bin/env node\n";
        assert_eq!(
            interpreter_refusal(script, Some(start), Some(OsStr::new("/usr/bin")), &runnable),
            None
        );
        let why = interpreter_refusal(script, Some(start), Some(OsStr::new("/usr/local/bin:/nope")), &runnable)
            .expect("node is not there");
        assert!(
            why.contains("env node") && why.contains("not on this window's PATH"),
            "{why}"
        );
        // A relative PATH entry does not count; no PATH is env's own default.
        assert!(interpreter_refusal(script, Some(start), Some(OsStr::new("bin:.")), &runnable).is_some());
        assert!(interpreter_refusal(script, Some(start), None, &runnable).is_none());
        // An interpreter by path, a binary, an unreadable start: nothing to say.
        assert!(interpreter_refusal(script, Some(b"#!/usr/bin/env /opt/n/node\n"), None, &runnable).is_none());
        assert!(interpreter_refusal(script, Some(b"#!/usr/bin/env /opt/x/node\n"), None, &runnable).is_some());
        assert!(interpreter_refusal(script, Some(b"\x7fELF"), Some(OsStr::new("")), &runnable).is_none());
        assert!(interpreter_refusal(script, None, Some(OsStr::new("")), &runnable).is_none());
    }

    /// A file system for [`walk`]: every entry with its type, owner and mode, and each link's target.
    struct FakeFs {
        entries: HashMap<PathBuf, Entry>,
        links: HashMap<PathBuf, PathBuf>,
    }

    const ME: u32 = 1000;

    impl FakeFs {
        /// `/`, `/home` and `/home/u` as a usual system has them.
        fn new() -> Self {
            let mut fs = FakeFs {
                entries: HashMap::new(),
                links: HashMap::new(),
            };
            fs.dir("/", 0, 0o755).dir("/home", 0, 0o755).dir("/home/u", ME, 0o700);
            fs
        }
        fn dir(&mut self, p: &str, uid: u32, mode: u32) -> &mut Self {
            self.entries.insert(
                p.into(),
                Entry {
                    kind: Kind::Dir,
                    uid,
                    mode: 0o040000 | mode,
                },
            );
            self
        }
        fn file(&mut self, p: &str, uid: u32, mode: u32) -> &mut Self {
            self.entries.insert(
                p.into(),
                Entry {
                    kind: Kind::File,
                    uid,
                    mode: 0o100000 | mode,
                },
            );
            self
        }
        fn link(&mut self, p: &str, target: &str) -> &mut Self {
            self.entries.insert(
                p.into(),
                Entry {
                    kind: Kind::Link,
                    uid: ME,
                    mode: 0o120777,
                },
            );
            self.links.insert(p.into(), target.into());
            self
        }
        fn walk(&self, p: &str) -> Inspected {
            walk(
                Path::new(p),
                ME,
                &|p| {
                    self.entries
                        .get(p)
                        .copied()
                        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
                },
                &|p| {
                    self.links
                        .get(p)
                        .cloned()
                        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))
                },
            )
        }
    }

    /// The native installer's layout: `~/.local/bin/claude`, a link into `~/.local/share/claude/versions/`.
    fn native() -> FakeFs {
        let mut fs = FakeFs::new();
        fs.dir("/home/u/.local", ME, 0o755)
            .dir("/home/u/.local/bin", ME, 0o755)
            .dir("/home/u/.local/share", ME, 0o755)
            .dir("/home/u/.local/share/claude", ME, 0o755)
            .dir("/home/u/.local/share/claude/versions", ME, 0o755)
            .file("/home/u/.local/share/claude/versions/2.1.316", ME, 0o755)
            .link("/home/u/.local/bin/claude", "../share/claude/versions/2.1.316");
        fs
    }

    #[test]
    fn the_native_layout_is_usable_through_its_relative_link() {
        assert_eq!(native().walk("/home/u/.local/bin/claude"), Inspected::Usable);
        // The same through an absolute link, and through a link to a link.
        let mut fs = native();
        fs.link("/home/u/.local/bin/claude", "/home/u/.local/bin/hop")
            .link("/home/u/.local/bin/hop", "/home/u/.local/share/claude/versions/2.1.316");
        assert_eq!(fs.walk("/home/u/.local/bin/claude"), Inspected::Usable);
        // A file root owns (a package's) is usable too.
        let mut fs = native();
        fs.file("/home/u/.local/share/claude/versions/2.1.316", 0, 0o755);
        assert_eq!(fs.walk("/home/u/.local/bin/claude"), Inspected::Usable);
    }

    #[test]
    fn a_file_or_directory_others_can_change_is_refused_wherever_it_is_on_the_way() {
        let refused = |fs: &FakeFs, needle: &str| match fs.walk("/home/u/.local/bin/claude") {
            Inspected::Refused(why) => assert!(why.contains(needle), "{why:?} lacks {needle:?}"),
            other => panic!("expected a refusal naming {needle}, got {other:?}"),
        };
        // The file: group- or world-writable, or another user's.
        let mut fs = native();
        fs.file("/home/u/.local/share/claude/versions/2.1.316", ME, 0o775);
        refused(&fs, "2.1.316 can be written by other users");
        let mut fs = native();
        fs.file("/home/u/.local/share/claude/versions/2.1.316", ME, 0o757);
        refused(&fs, "can be written by other users");
        let mut fs = native();
        fs.file("/home/u/.local/share/claude/versions/2.1.316", 1001, 0o755);
        refused(&fs, "2.1.316 is owned by uid 1001");
        // The directory the link sits in, and one the link leads through.
        let mut fs = native();
        fs.dir("/home/u/.local/bin", ME, 0o777);
        refused(&fs, "/home/u/.local/bin can be written by other users");
        let mut fs = native();
        fs.dir("/home/u/.local/share/claude", 1001, 0o755);
        refused(&fs, "/home/u/.local/share/claude is owned by uid 1001");
        // Higher up, the root included; a sticky bit does not make a writable directory acceptable.
        let mut fs = native();
        fs.dir("/home", 0, 0o1777);
        refused(&fs, "/home can be written by other users");
        let mut fs = native();
        fs.dir("/", 0, 0o775);
        refused(&fs, "/ can be written by other users");
        // A link target that passes through somewhere others can write.
        let mut fs = native();
        fs.dir("/tmp", 0, 0o1777)
            .file("/tmp/claude", ME, 0o755)
            .link("/home/u/.local/bin/claude", "/tmp/claude");
        refused(&fs, "/tmp can be written by other users");
    }

    #[test]
    fn what_is_not_there_is_absent_and_what_is_not_a_file_is_refused() {
        assert_eq!(FakeFs::new().walk("/home/u/.local/bin/claude"), Inspected::Absent);
        // A dangling link.
        let mut fs = native();
        fs.link("/home/u/.local/bin/claude", "../share/claude/versions/9.9.9");
        assert_eq!(fs.walk("/home/u/.local/bin/claude"), Inspected::Absent);
        // A path through a file.
        let mut fs = FakeFs::new();
        fs.file("/home/u/.local", ME, 0o644);
        assert_eq!(fs.walk("/home/u/.local/bin/claude"), Inspected::Absent);
        // A directory named claude, a special file, a loop of links.
        let mut fs = native();
        fs.dir("/home/u/.local/share/claude/versions/2.1.316", ME, 0o755);
        assert!(
            matches!(fs.walk("/home/u/.local/bin/claude"), Inspected::Refused(why) if why.contains("is a directory"))
        );
        let mut fs = native();
        fs.entries.insert(
            "/home/u/.local/share/claude/versions/2.1.316".into(),
            Entry {
                kind: Kind::Other,
                uid: ME,
                mode: 0o010755,
            },
        );
        assert!(
            matches!(fs.walk("/home/u/.local/bin/claude"), Inspected::Refused(why) if why.contains("not a regular file"))
        );
        let mut fs = native();
        fs.link("/home/u/.local/bin/claude", "claude");
        assert!(
            matches!(fs.walk("/home/u/.local/bin/claude"), Inspected::Refused(why) if why.contains("too many links"))
        );
        assert!(matches!(FakeFs::new().walk("relative/claude"), Inspected::Refused(_)));
    }

    /// The real file system, as this process: a file without the execute bit is refused even where `walk` passes.
    #[test]
    fn inspect_installed_asks_whether_the_file_can_be_executed() {
        use std::os::unix::fs::PermissionsExt;
        let base = trusted_scratch_base();
        let root = base.join(format!("eitri-claude-inspect-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let file = root.join("claude");
        std::fs::write(&file, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let plain = inspect_installed(&file);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        let executable = inspect_installed(&file);
        let _ = std::fs::remove_dir_all(&root);
        assert!(matches!(plain, Inspected::Refused(why) if why.contains("cannot be executed")));
        assert_eq!(executable, Inspected::Usable);
    }

    /// A directory to lay test files out under whose every ancestor `walk` accepts: the temporary directory when it
    /// is one (`/tmp` itself is world-writable, so it is not), else the directory the test binary is in.
    pub(crate) fn trusted_scratch_base() -> PathBuf {
        let accepted = |dir: &Path| {
            let probe = dir.join(format!(".eitri-claude-probe-{}", uuid::Uuid::new_v4()));
            if std::fs::write(&probe, "").is_err() {
                return false;
            }
            let ok = walk(&probe, crate::private_fs::euid(), &lstat_entry, &|p| {
                std::fs::read_link(p)
            }) == Inspected::Usable;
            let _ = std::fs::remove_file(&probe);
            ok
        };
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(Path::to_path_buf));
        [Some(std::env::temp_dir()), exe_dir]
            .into_iter()
            .flatten()
            .find(|d| accepted(d))
            .expect("no scratch directory without a group- or world-writable ancestor (TMPDIR, the test binary's)")
    }
}
