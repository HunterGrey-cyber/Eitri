//! The user's tmux keys: the prefix and the prefix table, read from tmux's own configuration files
//! (`keymap.from_tmux`, on unless `init.lua` turns it off) and turned into Eitri actions where one
//! exists. Everything else -- root keys, copy-mode tables, commands Eitri has no equivalent of,
//! anything that would need tmux to run or evaluate something -- is skipped and listed with the
//! reason, never guessed at.
//!
//! **Static reading only.** The files are read as text; no tmux server is asked (the answer would
//! depend on whether one happens to run, and on which socket) and none is started (a config's
//! `run-shell` lines -- a plugin manager, a theme script -- would run). The same files always give
//! the same keys.
//!
//! **Which files, in which order (tmux 3.7):** tmux's build sets its default configuration to the
//! list `/etc/tmux.conf:~/.tmux.conf:$XDG_CONFIG_HOME/tmux/tmux.conf:~/.config/tmux/tmux.conf`
//! (its `Makefile.am`, the `TMUX_CONF` define). At startup it expands each entry -- a leading `~/`
//! to the home directory, a leading `$NAME` to that variable, dropping the entry when the variable
//! is unset -- drops an entry whose expanded text it already has (text, not the resolved path: a
//! trailing slash on `$XDG_CONFIG_HOME` makes the same file load twice), and then loads every one
//! that exists, in that order, quietly skipping the missing ones (`tmux.c`'s `expand_paths`,
//! `cfg.c`'s `start_cfg`). A later file's binding wins. Checked against a scratch server too.
//!
//! **`source-file` with a relative path** is resolved by tmux against the working directory of
//! the client that started the server, not against the file doing the sourcing
//! (`cmd-source-file.c` globs `<cwd>/<path>`, the cwd being `server_client_get_cwd`, which is the
//! first client's while the config loads). Measured with a scratch server: a `source-file
//! rel.conf` in `~/.config/tmux/tmux.conf` loaded `./rel.conf` from where `tmux` was run. That
//! directory is unknowable from here, so such a line is skipped with that reason rather than
//! resolved against some other directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{Action, KeySpec};

pub mod lex;
mod map;
pub mod notice;

#[cfg(test)]
pub(crate) mod tests;

pub use map::every_pane_takes;

/// The `init.lua` key (`eitri.config.set`) that turns the import off.
pub const SETTING: &str = "keymap.from_tmux";

/// Files read at most, `source-file`s included.
pub const MAX_FILES: usize = 64;
/// A file bigger than this is not read.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Lines read at most, over every file.
pub const MAX_LINES: usize = 20_000;
/// How deep `source-file`s may nest below a file tmux loads itself.
pub const MAX_DEPTH: usize = 10;
/// Steps and skips recorded at most, over every file: a line can hold many commands, and each one
/// kept costs its own record.
pub const MAX_ENTRIES: usize = 4096;
/// Directory entries every `source-file` glob together may look at over one import. Kept apart
/// from the matcher's steps: an entry is a `readdir`/`stat`, cheap on a warm local disk but slow on
/// a cold or network one, so it gets the tighter bound.
pub const MAX_GLOB_ENTRIES: usize = 10_000;
/// Matcher steps every `source-file` glob together may take over one import: each step of matching
/// a name against a pattern, every bracket-class character included, costs one, so a pattern that
/// is costly to match is bounded however few entries it meets. Everything else the import does is
/// linear in input that is already bounded (files, lines, entries, expansion).
pub const MAX_GLOB_WORK: usize = 20_000_000;

/// What `source-file` globs may still do over the import: entries looked at, and matcher steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GlobBudget {
    pub entries: usize,
    pub work: usize,
}

/// Opens `path` without waiting on it: a FIFO opened for reading blocks until a writer appears, and
/// `O_NONBLOCK` makes that open return at once instead.
fn open_nonblocking(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

/// Reads at most `limit` bytes of `file`, as text: one byte past the limit is enough to know it is
/// too big, whatever the file claims its size is (`/dev/zero` claims none).
fn read_bounded(file: std::fs::File, limit: u64, path: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{} cannot be read: {e}", path.display()))?;
    if bytes.len() as u64 > limit {
        let size = if limit.is_multiple_of(1024 * 1024) {
            format!("{} MiB", limit / (1024 * 1024))
        } else {
            format!("{limit} bytes")
        };
        return Err(format!("{} is bigger than {size}; not read", path.display()));
    }
    String::from_utf8(bytes).map_err(|_| format!("{} cannot be read: it is not UTF-8 text", path.display()))
}

/// `path`'s text, read with the same bound the import uses, whatever kind of file it is.
pub fn read_limited(path: &Path, limit: u64) -> Result<String, String> {
    let file = open_nonblocking(path).map_err(|e| format!("{} cannot be read: {e}", path.display()))?;
    read_bounded(file, limit, path)
}

/// `path`'s text if it is a regular file (after symlinks) no bigger than [`MAX_FILE_BYTES`].
fn read_config(path: &Path) -> Result<String, String> {
    read_regular(path, MAX_FILE_BYTES)
}

/// `path`'s text if it is a regular file (after symlinks) of at most `limit` bytes: every file the
/// import reads at startup goes through this. A FIFO, a device or a directory is refused before it
/// is opened, and the open handle is checked again, so one swapped in between is refused too.
pub(crate) fn read_regular(path: &Path, limit: u64) -> Result<String, String> {
    let not_regular = || format!("{} is not a regular file; not read", path.display());
    let meta = std::fs::metadata(path).map_err(|e| format!("{} cannot be read: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(not_regular());
    }
    let file = open_nonblocking(path).map_err(|e| format!("{} cannot be read: {e}", path.display()))?;
    match file.metadata() {
        Ok(meta) if meta.is_file() => {}
        _ => return Err(not_regular()),
    }
    read_bounded(file, limit, path)
}

/// `keymap.from_tmux` as `init.lua` set it: unset or `"on"` imports, `"off"` does not, anything
/// else is a startup failure naming the key -- the same discipline as Eitri's other settings.
pub fn enabled(value: Option<&str>) -> Result<bool, String> {
    match value {
        None | Some("on") => Ok(true),
        Some("off") => Ok(false),
        Some(other) => Err(format!(
            "eitri.config.set(\"{SETTING}\", {other:?}): it is \"on\" (the default) or \"off\""
        )),
    }
}

/// What the import reads with: the home directory, the environment tmux would have, and where the
/// system-wide file is.
#[derive(Debug, Clone)]
pub struct TmuxEnv {
    pub home: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub system_file: PathBuf,
}

impl TmuxEnv {
    /// From a home directory and environment variables as the OS gives them: an empty or relative
    /// home is no home, and a variable whose name or value is not UTF-8 is left out -- a tmux
    /// config could only reach it through `$NAME`, and a skipped variable expands to nothing.
    pub fn from_vars(
        home: Option<std::ffi::OsString>,
        vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    ) -> TmuxEnv {
        TmuxEnv {
            home: home
                .filter(|h| !h.is_empty())
                .map(PathBuf::from)
                .filter(|h| h.is_absolute()),
            env: vars
                .into_iter()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .collect(),
            system_file: PathBuf::from("/etc/tmux.conf"),
        }
    }

    /// This process's: `$HOME` (unset or empty is no home, as tmux treats it before it falls back to
    /// the password database, which Eitri does not read) and its environment.
    pub fn from_process() -> TmuxEnv {
        TmuxEnv::from_vars(std::env::var_os("HOME"), std::env::vars_os())
    }

    /// The files tmux 3.7 loads at startup, in its order, each expanded path once (this module's
    /// doc), whether or not they exist.
    pub fn config_files(&self) -> Vec<PathBuf> {
        let home = self.home.as_ref().map(|h| h.display().to_string());
        let xdg = self.env.get("XDG_CONFIG_HOME");
        let candidates = [
            Some(self.system_file.display().to_string()),
            home.as_ref().map(|h| format!("{h}/.tmux.conf")),
            xdg.map(|x| format!("{x}/tmux/tmux.conf")),
            home.as_ref().map(|h| format!("{h}/.config/tmux/tmux.conf")),
        ];
        let mut out: Vec<String> = Vec::new();
        for candidate in candidates.into_iter().flatten() {
            if !out.contains(&candidate) {
                out.push(candidate);
            }
        }
        out.into_iter().map(PathBuf::from).collect()
    }

    fn vars(&self) -> lex::Vars {
        lex::Vars::new(self.home.as_ref().map(|h| h.display().to_string()), self.env.clone())
    }
}

/// Where a step or a skip came from: the file, its line (0 for the file as a whole), and the
/// statement as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub file: PathBuf,
    pub line: usize,
    pub text: String,
}

/// A line that did not come across, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub origin: Origin,
    pub reason: String,
}

/// One thing the config does to the prefix table, in Eitri's terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Prefix(KeySpec),
    Bind {
        key: KeySpec,
        action: Action,
        repeatable: bool,
    },
    Unbind(KeySpec),
    /// `unbind -a`: the whole prefix table.
    UnbindAll,
}

/// Everything the files said, in tmux's order. `files` are the ones tmux loads itself that exist
/// and were read; the files they source are not listed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TmuxRead {
    pub files: Vec<PathBuf>,
    pub steps: Vec<(Step, Origin)>,
    pub skipped: Vec<Skipped>,
}

/// Reads the configuration tmux would load for `env`. Never fails: whatever cannot be read is a
/// skip with its reason.
pub fn read(env: &TmuxEnv) -> TmuxRead {
    let mut reader = Reader {
        vars: env.vars(),
        out: TmuxRead::default(),
        files_read: 0,
        lines_read: 0,
        glob_budget: GlobBudget {
            entries: MAX_GLOB_ENTRIES,
            work: MAX_GLOB_WORK,
        },
        stack: Vec::new(),
        stopped: false,
    };
    for path in env.config_files() {
        if reader.stopped {
            break;
        }
        match std::fs::metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            _ => {}
        }
        let whole = Origin {
            file: path.clone(),
            line: 0,
            text: String::new(),
        };
        if reader.file(&path, 0, &whole) {
            reader.out.files.push(path);
        }
    }
    reader.out
}

struct Reader {
    vars: lex::Vars,
    out: TmuxRead,
    files_read: usize,
    lines_read: usize,
    /// What `source-file` globs may still do.
    glob_budget: GlobBudget,
    /// The files being read, outermost first: a file sourcing one of these is a cycle.
    stack: Vec<PathBuf>,
    /// A bound was reached; nothing more is read.
    stopped: bool,
}

impl Reader {
    /// Whether there is room for one more step or skip; at the bound, says so once and stops.
    fn room(&mut self, origin: &Origin) -> bool {
        if self.stopped {
            return false;
        }
        if self.out.steps.len() + self.out.skipped.len() >= MAX_ENTRIES {
            self.out.skipped.push(Skipped {
                origin: origin.clone(),
                reason: format!("more than {MAX_ENTRIES} keys and skipped lines; the rest is not read"),
            });
            self.stopped = true;
            return false;
        }
        true
    }

    fn skip(&mut self, origin: &Origin, reason: impl Into<String>) {
        if self.room(origin) {
            self.out.skipped.push(Skipped {
                origin: origin.clone(),
                reason: reason.into(),
            });
        }
    }

    fn step(&mut self, step: Step, origin: &Origin) {
        if self.room(origin) {
            self.out.steps.push((step, origin.clone()));
        }
    }

    /// Reads `path`, `depth` files below one tmux loads itself; `from` is the line that asked for it
    /// (or the file itself, at the top), where a refusal is reported. Whether it was read.
    fn file(&mut self, path: &Path, depth: usize, from: &Origin) -> bool {
        if depth > MAX_DEPTH {
            self.skip(from, format!("nested more than {MAX_DEPTH} deep; not read"));
            return false;
        }
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if self.stack.contains(&canonical) {
            self.skip(from, "that file is already being read (a cycle); not read again");
            return false;
        }
        if self.files_read >= MAX_FILES {
            self.skip(from, format!("more than {MAX_FILES} files; the rest is not read"));
            self.stopped = true;
            return false;
        }
        let text = match read_config(path) {
            Ok(text) => text,
            Err(why) => {
                self.skip(from, why);
                return false;
            }
        };
        self.files_read += 1;
        self.stack.push(canonical);
        self.statements(path, &text, depth);
        self.stack.pop();
        true
    }

    fn statements(&mut self, path: &Path, text: &str, depth: usize) {
        let statements = lex::statements(text, &mut self.vars);
        let lines_left = MAX_LINES.saturating_sub(self.lines_read);
        self.lines_read += text.lines().count();
        // How deep in `%if` blocks the current statement is.
        let mut conditional = 0usize;
        for statement in statements {
            if self.stopped {
                return;
            }
            let origin = Origin {
                file: path.to_path_buf(),
                line: statement.line,
                text: statement.text.clone(),
            };
            if statement.line > lines_left {
                self.skip(&origin, format!("more than {MAX_LINES} lines; the rest is not read"));
                self.stopped = true;
                return;
            }
            let item = match statement.item {
                Ok(item) => item,
                Err(why) => {
                    let reason = if conditional > 0 {
                        "inside a %if block, which only tmux can evaluate".to_string()
                    } else {
                        format!("tmux refuses it: {why}")
                    };
                    self.skip(&origin, reason);
                    continue;
                }
            };
            match item {
                lex::Item::If => {
                    if conditional == 0 {
                        self.skip(
                            &origin,
                            "%if: only tmux can evaluate the condition, so nothing in the block is read",
                        );
                    } else {
                        self.skip(&origin, "inside a %if block, which only tmux can evaluate");
                    }
                    conditional += 1;
                    continue;
                }
                lex::Item::Elif | lex::Item::Else => {
                    if conditional == 0 {
                        self.skip(&origin, "tmux refuses it: no %if before it");
                    }
                    continue;
                }
                lex::Item::Endif => {
                    if conditional == 0 {
                        self.skip(&origin, "tmux refuses it: no %if before it");
                    }
                    conditional = conditional.saturating_sub(1);
                    continue;
                }
                _ if conditional > 0 => {
                    self.skip(&origin, "inside a %if block, which only tmux can evaluate");
                    continue;
                }
                lex::Item::Hidden => self.skip(&origin, "%hidden is not read"),
                lex::Item::Assign => {}
                lex::Item::Commands(commands) => {
                    for command in &commands {
                        let outcome = map::top_level(command, &self.vars);
                        if let (Some(name), false) = (&statement.depends_on, outcome == map::Outcome::Ignore) {
                            self.skip(
                                &origin,
                                format!(
                                    "it reads ${name}, which a %if block or %hidden line may have set; \
                                     only tmux knows its value"
                                ),
                            );
                            continue;
                        }
                        match outcome {
                            map::Outcome::Step(step) => self.step(step, &origin),
                            map::Outcome::Skip(why) => self.skip(&origin, why),
                            map::Outcome::Ignore => {}
                            map::Outcome::Source { quiet, paths } => {
                                for written in paths {
                                    self.source(&written, quiet, depth, &origin);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// One path of a `source-file`, globbed as tmux globs it.
    fn source(&mut self, written: &str, quiet: bool, depth: usize, origin: &Origin) {
        if written == "-" {
            self.skip(origin, "source-file - reads standard input, which Eitri does not");
            return;
        }
        if !written.starts_with('/') {
            self.skip(
                origin,
                "a relative path, which tmux resolves against the directory tmux was started in; \
                 Eitri cannot know it",
            );
            return;
        }
        let (matches, exhausted) = glob(written, &mut self.glob_budget);
        if exhausted {
            self.skip(
                origin,
                format!(
                    "{written}: the globs looked at more than {MAX_GLOB_ENTRIES} entries or took more than \
                     {MAX_GLOB_WORK} matching steps; the rest is not read"
                ),
            );
        }
        if matches.is_empty() {
            if !quiet {
                self.skip(origin, format!("{written}: no such file"));
            }
            return;
        }
        for path in matches {
            if self.stopped {
                return;
            }
            self.file(&path, depth + 1, origin);
        }
    }
}

/// The files an absolute path pattern names, as `glob(3)` with no flags finds them: `*`, `?` and
/// `[...]` within one path component, a leading `.` matched only by a pattern that starts with
/// one, sorted. A pattern with no wildcard names itself if it exists. Every directory entry looked
/// at and every path tried costs one of `budget.entries`, every step of matching a name one of
/// `budget.work`; when either runs out the search stops and says so (the `bool`).
pub(crate) fn glob(pattern: &str, budget: &mut GlobBudget) -> (Vec<PathBuf>, bool) {
    let mut found = vec![PathBuf::from("/")];
    for part in pattern.split('/').filter(|p| !p.is_empty()) {
        let mut next = Vec::new();
        for dir in &found {
            if !part.contains(['*', '?', '[']) {
                if budget.entries == 0 {
                    return (Vec::new(), true);
                }
                budget.entries -= 1;
                let candidate = dir.join(part);
                if std::fs::symlink_metadata(&candidate).is_ok() {
                    next.push(candidate);
                }
                continue;
            }
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            let mut names = Vec::new();
            for entry in entries {
                if budget.entries == 0 {
                    return (Vec::new(), true);
                }
                budget.entries -= 1;
                let Ok(entry) = entry else { continue };
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                if name.starts_with('.') && !part.starts_with('.') {
                    continue;
                }
                match fnmatch_within(part, &name, &mut budget.work) {
                    Some(true) => names.push(name),
                    Some(false) => {}
                    None => return (Vec::new(), true),
                }
            }
            names.sort();
            next.extend(names.into_iter().map(|name| dir.join(name)));
        }
        found = next;
    }
    // Whatever exists is returned, as glob(3) does; reading it is what refuses a FIFO, a device
    // or a directory, and says so.
    (found, false)
}

/// One `[...]` bracket at the start of `p` against `c`: `Some((matched, its length))`, or `None`
/// when it has no closing `]`, which makes the `[` an ordinary character. Each character of the
/// class looked at costs one of `work`.
fn bracket(p: &[char], c: char, work: &mut usize) -> Option<(bool, usize)> {
    let mut i = 1;
    let negate = matches!(p.get(1), Some('!' | '^'));
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < p.len() && (p[i] != ']' || first) {
        *work = work.saturating_sub(1);
        first = false;
        if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
            matched |= p[i] <= c && c <= p[i + 2];
            i += 3;
        } else {
            matched |= p[i] == c;
            i += 1;
        }
    }
    (i < p.len()).then_some((matched != negate, i + 1))
}

/// `fnmatch(3)` for one path component, with no bound on its work: for short patterns in tests.
#[cfg(test)]
pub(crate) fn fnmatch(pattern: &str, name: &str) -> bool {
    let mut work = usize::MAX;
    fnmatch_within(pattern, name, &mut work).expect("an unbounded budget")
}

/// `fnmatch(3)` for one path component: `*`, `?`, `[...]` (with `!` or `^` negation and ranges),
/// and a backslash escaping the next character. Iterative, going back only to the last `*` seen,
/// so never exponential; and every step, every bracket character included, costs one of `work`,
/// so a pattern a `*` makes it rescan many times is bounded too. `None` when `work` runs out.
pub(crate) fn fnmatch_within(pattern: &str, name: &str, work: &mut usize) -> Option<bool> {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0, 0);
    // Where the last `*` was, and where in the name it last resumed.
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if *work == 0 {
            return None;
        }
        *work -= 1;
        let step = match p.get(pi) {
            Some('*') => {
                while p.get(pi) == Some(&'*') {
                    pi += 1;
                }
                star = Some((pi, ni));
                continue;
            }
            Some('?') => Some(1),
            Some('[') => match bracket(&p[pi..], n[ni], work) {
                Some((true, len)) => Some(len),
                Some((false, _)) => None,
                None => (n[ni] == '[').then_some(1),
            },
            Some('\\') if pi + 1 < p.len() => (n[ni] == p[pi + 1]).then_some(2),
            Some(c) => (n[ni] == *c).then_some(1),
            None => None,
        };
        match (step, star) {
            (Some(len), _) => {
                pi += len;
                ni += 1;
            }
            (None, Some((after, resumed))) => {
                pi = after;
                ni = resumed + 1;
                star = Some((after, resumed + 1));
            }
            (None, None) => return Some(false),
        }
    }
    while p.get(pi) == Some(&'*') {
        pi += 1;
    }
    Some(pi == p.len())
}

#[cfg(test)]
mod glob_tests {
    use super::fnmatch;

    #[test]
    fn fnmatch_matches_as_glob_does() {
        assert!(fnmatch("*.conf", "a.conf"));
        assert!(!fnmatch("*.conf", "a.conf.bak"));
        assert!(fnmatch("a?.conf", "ab.conf"));
        assert!(fnmatch("[ab]*", "b1"));
        assert!(!fnmatch("[!ab]*", "b1"));
        assert!(fnmatch("[a-c]x", "bx"));
        assert!(fnmatch("\\*", "*"));
        assert!(!fnmatch("\\*", "a"));
    }
}
