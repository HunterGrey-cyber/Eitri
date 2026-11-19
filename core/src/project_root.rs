//! Where `shell` believes the project it is editing lives -- resolved exactly once, in `main()`,
//! and then carried as a value.
//!
//! Resolving once and passing the answer to the editor, agent panel and bottom terminal prevents
//! them from disagreeing about their project after a later `chdir` or a second activation. It also
//! gives `shell <dir>` one place to select that root.
//!
//! Everything here works in `OsStr`/`OsString`, never `String`. `std::env::args()` is documented
//! to panic on an argument that is not valid UTF-8, and a filesystem path is exactly the argument
//! class most likely to be one -- a module whose whole contract is "a bad input gets a sentence,
//! not a panic" cannot be built on a type that cannot hold the bad input.
//!
//! Two resolvers live here. [`resolve_launch`] is the window's: its command line may name files, and
//! then a file decides the root. [`resolve`], [`resolve_args`] and [`resolve_in`] take one directory
//! and nothing else, for `eitri panel`, `eitri split` and the Mac host.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use agent::project_boundary::{open_to_others, refusal, Identity};

use crate::project_trust::{git_top_level, GitWalk, Limits};

/// The environment variable consulted when no directory was given on the command line. Exists so a
/// launcher (`.desktop` entry, sandbox harness) can point `shell` at a project without having to
/// control its working directory.
const PROJECT_DIR_ENV: &str = "EITRI_PROJECT_DIR";

/// Every option `main()` itself matches on, listed here because this function is the only place
/// that can tell a flag from a path that happens to start with `-`.
///
/// **Adding an option to `main()` without adding it here makes `shell --that-option` a hard
/// startup failure** -- loudly, at the first launch, rather than by silently opening the wrong
/// directory, which is the trade this module exists to make. It takes no value; one that did
/// would need more than a membership test here (see [`select_root_source`]'s doc).
///
/// `--version` is intercepted before `resolve_launch()`, but belongs here so all flag recognition agrees.
/// `--legacy` is also intercepted in builds without the legacy backend (every release). Listing
/// it in every build lets that path report `LEGACY_NOT_IN_BUILD`, rather than an unrecognized
/// option or a silently opened cwd.
///
/// `--new-window`, `--raise` and `--local` are read by [`resolve_launch_in`]; the directory-only resolvers step over
/// them. `--local` opens a remote project's copy on this machine as a local project, with no agent tabs.
const KNOWN_FLAGS: [&str; 6] = ["--clean", "--version", "--legacy", "--new-window", "--raise", "--local"];

/// `--ssh <host>`: the project of the path that follows is on `host`, an OpenSSH destination. It takes a separate
/// value, so it is not in [`KNOWN_FLAGS`]: the parsers that know it take the host from the next token.
pub const SSH_FLAG: &str = "--ssh";

/// What every reader that is not the window says to `--ssh`: only a window opens a remote project.
const SSH_ONLY_IN_A_WINDOW: &str =
    "--ssh opens a remote project in a window of its own (eitri --ssh <host> <path>); it cannot be used here";

/// The conventional end-of-flags separator. What follows it is read verbatim, so a path whose name
/// really does begin with `-` is reachable.
const END_OF_FLAGS: &str = "--";

/// Whether `flag` appears in `args` *before* the first `--` -- the same end-of-flags rule
/// [`select_root_source`] itself applies.
///
/// Sharing this end-of-flags check with the early `--version`/`--legacy`/`--clean` dispatch
/// prevents disagreement with [`select_root_source`]. `shell -- --version` and
/// `shell -- --legacy` name directories, so neither may trigger an early flag action.
pub fn flag_given<'a>(args: impl IntoIterator<Item = &'a OsStr>, flag: &str) -> bool {
    for arg in args {
        if arg == OsStr::new(END_OF_FLAGS) {
            return false;
        }
        if arg == OsStr::new(flag) {
            return true;
        }
    }
    false
}

/// Which of the three candidates won, decided before any filesystem call happens. Separating the
/// choice from the `canonicalize` is what keeps the precedence rule unit-testable without a real
/// directory tree to point at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootSource {
    /// The first positional (non-`-`-prefixed) command-line argument, or whatever followed `--`.
    Argument(OsString),
    /// `EITRI_PROJECT_DIR`.
    Env(OsString),
    /// Neither was given -- fall back to this process's own working directory.
    Cwd,
    /// `--ssh <host> <path>`: a project on a server, its path as typed. Only the window opens one; every
    /// directory-only resolver refuses it.
    Remote { host: String, path: PathBuf },
}

impl RootSource {
    /// How to name this source in an error message, so a failure says which of the three inputs
    /// the operator actually has to go and fix.
    ///
    /// `{:?}` on an `OsStr` quotes it and escapes any non-UTF-8 byte as `\xNN` rather than
    /// refusing to print it, which is what lets a mistyped binary path still be named back to
    /// whoever typed it.
    fn describe(&self) -> String {
        match self {
            RootSource::Argument(path) => format!("the project directory argument {path:?}"),
            RootSource::Env(path) => format!("{PROJECT_DIR_ENV}={path:?}"),
            RootSource::Cwd => "the current working directory".to_string(),
            RootSource::Remote { host, path } => format!("{} on {host}", path.display()),
        }
    }
}

/// Resolves the project root from this process's real arguments and environment.
///
/// `Err` carries a finished, human-readable sentence for `main()` to print before exiting,
/// naming the failing path and what `shell` is trying to do rather than panicking.
///
/// This function is nothing but the three reads of process state; [`resolve_from`] is the part
/// with behaviour in it, and is what the tests drive.
pub fn resolve() -> Result<PathBuf, String> {
    // argv[0] is this binary, never a project directory.
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    resolve_from(
        args.iter().map(OsString::as_os_str),
        std::env::var_os(PROJECT_DIR_ENV),
        std::env::current_dir(),
    )
}

/// [`resolve`] over an explicit argument list (`argv[0]` already stripped), for a command line that
/// is not the process's own: `eitri panel` hands its leftover arguments here after taking its own
/// options out. The environment and the cwd are the process's, exactly as in [`resolve`].
pub fn resolve_args(args: &[OsString]) -> Result<PathBuf, String> {
    resolve_in(args, std::env::var_os(PROJECT_DIR_ENV), std::env::current_dir())
}

/// [`resolve_args`] with the environment variable and the working directory handed in, for a host that
/// decides them itself (and for tests, which must not read the runner's own).
pub fn resolve_in(args: &[OsString], env: Option<OsString>, cwd: io::Result<PathBuf>) -> Result<PathBuf, String> {
    resolve_from(args.iter().map(OsString::as_os_str), env, cwd)
}

/// The explicit editor address and optional project directory for `eitri open`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenArgs {
    pub nvim: String,
    pub dir: Option<PathBuf>,
}

/// Parses takeover arguments without consulting the environment or resolving the directory.
pub fn parse_open_args(args: &[OsString]) -> Result<OpenArgs, String> {
    let mut nvim = None;
    let mut dir = None;
    let mut positional = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if !positional && arg == OsStr::new(END_OF_FLAGS) {
            positional = true;
            continue;
        }
        if !positional && arg == OsStr::new("--nvim") {
            let value = args
                .next()
                .and_then(|value| value.to_str())
                .filter(|value| !value.is_empty() && !value.starts_with('-'))
                .ok_or_else(|| "eitri open: --nvim needs an address in UTF-8".to_owned())?;
            if nvim.replace(value.to_owned()).is_some() {
                return Err("eitri open: --nvim takes one address".to_owned());
            }
        } else if !positional && arg.as_bytes().first() == Some(&b'-') {
            return Err(format!("eitri open: unknown option {}", arg.to_string_lossy()));
        } else if dir.replace(PathBuf::from(arg)).is_some() {
            return Err("eitri open: takes at most one project directory".to_owned());
        }
    }
    let nvim = nvim.ok_or_else(|| "eitri open: --nvim is required".to_owned())?;
    Ok(OpenArgs { nvim, dir })
}

/// What `eitri panel` took from its own command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelArgs {
    /// The address of the nvim this panel attaches to: `--nvim <addr>`, else `$NVIM`.
    pub nvim: Option<PathBuf>,
    /// What is left for [`resolve_args`]: at most one project directory, and when it was given after
    /// `--` the `--` stays in front of it so a directory named like a flag is still read as a path.
    pub rest: Vec<OsString>,
}

/// `eitri panel`'s own options. `--nvim <addr>` takes a value; `--` ends options and the next token
/// is the project, verbatim; any other `-...` before `--` is refused. `nvim_env` is `$NVIM`, used when
/// no `--nvim` was given (an empty value counts as none).
///
/// This is not [`select_root_source`]'s flag handling: the panel has no `--clean` or `--legacy`, so
/// one of those is an error here rather than a skipped flag, and `--nvim` takes a separate value,
/// which a membership test cannot express.
pub fn parse_panel_args(args: &[OsString], nvim_env: Option<OsString>) -> Result<PanelArgs, String> {
    let mut nvim: Option<PathBuf> = None;
    let mut project: Option<(bool, OsString)> = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == OsStr::new(END_OF_FLAGS) {
            if let Some(path) = args.next() {
                if project.is_some() {
                    return Err(TWO_PROJECTS.to_string());
                }
                project = Some((true, path.clone()));
            }
            if args.next().is_some() {
                return Err(TWO_PROJECTS.to_string());
            }
            break;
        }
        if arg == OsStr::new("--nvim") {
            let value = args.next().ok_or_else(|| "--nvim needs an address".to_string())?;
            nvim = Some(PathBuf::from(value));
            continue;
        }
        if arg.as_bytes().first() == Some(&b'-') {
            return Err(format!("eitri panel: unknown option {}", arg.to_string_lossy()));
        }
        if project.is_some() {
            return Err(TWO_PROJECTS.to_string());
        }
        project = Some((false, arg.clone()));
    }
    let nvim = nvim.or_else(|| nvim_env.filter(|value| !value.is_empty()).map(PathBuf::from));
    let mut rest = Vec::new();
    if let Some((after_dashes, path)) = project {
        if after_dashes {
            rest.push(OsString::from(END_OF_FLAGS));
        }
        rest.push(path);
    }
    Ok(PanelArgs { nvim, rest })
}

const TWO_PROJECTS: &str = "eitri panel: takes at most one project directory";

/// `eitri page`'s own options. The caller resolves the directory and supplies `$NVIM` when needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageArgs {
    pub id: String,
    pub nvim: Option<PathBuf>,
    pub dir: Option<PathBuf>,
    pub list: bool,
}

/// Parses a plugin id and an optional directory without converting path bytes to text.
pub fn parse_page_args(args: &[OsString]) -> Result<PageArgs, String> {
    let mut page = PageArgs {
        id: String::new(),
        nvim: None,
        dir: None,
        list: false,
    };
    let mut positional = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == OsStr::new(END_OF_FLAGS) {
            positional.extend(args);
            break;
        }
        if arg == OsStr::new("--nvim") {
            let value = args
                .next()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "eitri page: --nvim needs an address".to_owned())?;
            page.nvim = Some(PathBuf::from(value));
        } else if arg == OsStr::new("--list") {
            page.list = true;
        } else if arg.as_bytes().first() == Some(&b'-') {
            return Err(format!("eitri page: unknown option {}", arg.to_string_lossy()));
        } else {
            positional.push(arg);
        }
    }
    if page.list {
        if !positional.is_empty() {
            return Err("eitri page: --list takes no plugin id or directory".to_owned());
        }
        return Ok(page);
    }
    let id = positional
        .first()
        .and_then(|id| id.to_str())
        .filter(|id| crate::plugin::manifest::id_ok(id))
        .ok_or_else(|| {
            "eitri page: needs a plugin id of 1 to 40 lower-case letters, digits or internal dashes".to_owned()
        })?;
    if positional.len() > 2 {
        return Err("eitri page: takes a plugin id and an optional directory".to_owned());
    }
    page.id = id.to_owned();
    page.dir = positional.get(1).map(PathBuf::from);
    Ok(page)
}

/// What `eitri split` took from its own command line: the project directory, in the form
/// [`resolve_args`] reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitArgs {
    /// At most one project directory; when it was given after `--` the `--` stays in front of it so
    /// a directory named like a flag is still read as a path.
    pub rest: Vec<OsString>,
}

/// `eitri split`'s command line: `[--] [DIR]`, nothing else. The split starts the editor itself, so
/// there is no option to pass on to it: any `-...` before `--` is refused by name.
pub fn parse_split_args(args: &[OsString]) -> Result<SplitArgs, String> {
    const TWO: &str = "eitri split: takes at most one project directory";
    let mut project: Option<(bool, OsString)> = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == OsStr::new(END_OF_FLAGS) {
            if let Some(path) = args.next() {
                if project.is_some() {
                    return Err(TWO.to_string());
                }
                project = Some((true, path.clone()));
            }
            if args.next().is_some() {
                return Err(TWO.to_string());
            }
            break;
        }
        if arg.as_bytes().first() == Some(&b'-') {
            return Err(format!("eitri split: unknown option {}", arg.to_string_lossy()));
        }
        if project.is_some() {
            return Err(TWO.to_string());
        }
        project = Some((false, arg.clone()));
    }
    let mut rest = Vec::new();
    if let Some((after_dashes, path)) = project {
        if after_dashes {
            rest.push(OsString::from(END_OF_FLAGS));
        }
        rest.push(path);
    }
    Ok(SplitArgs { rest })
}

/// [`resolve`] with its three process-globals passed in, so the wiring between the two halves --
/// which of them is consulted first, and that the chosen source is the one actually
/// canonicalized -- is reachable from a test. A swap of `args` and `env` here would be invisible
/// to every test of the two halves on their own.
fn resolve_from<'a>(
    args: impl IntoIterator<Item = &'a OsStr>,
    env: Option<OsString>,
    cwd: io::Result<PathBuf>,
) -> Result<PathBuf, String> {
    let source = select_root_source(args, env.as_deref())?;
    canonicalize_source(&source, cwd)
}

/// Precedence: an explicit argument beats the environment, which beats the cwd.
///
/// `args` must already have `argv[0]` stripped. A token that starts with `-` is a flag, and is
/// either one of [`KNOWN_FLAGS`] (skipped) or an error -- it is never silently ignored, because
/// "ignored, and the cwd was opened instead" is precisely the outcome this module exists to make
/// unreachable: `shell -myproj` and `shell --clen` would both have opened whatever directory the
/// shell happened to be sitting in, with nothing printed. `--` ends flag parsing, so a directory
/// whose name genuinely begins with `-` is still reachable as `shell -- -myproj`.
///
/// An option that takes a *separate* value (`--foo bar`) cannot be handled by a membership test --
/// `bar` would be read here as the project directory. Such an option has to be taught to this
/// function, not just added to `KNOWN_FLAGS`: `--ssh <host>` is, and its project is the path that
/// follows ([`RootSource::Remote`]), which needs one: the environment and the cwd do not name a
/// project on a server.
pub fn select_root_source<'a>(
    args: impl IntoIterator<Item = &'a OsStr>,
    env: Option<&OsStr>,
) -> Result<RootSource, String> {
    let mut args = args.into_iter();
    let mut ssh: Option<String> = None;
    let mut found: Option<OsString> = None;
    while let Some(arg) = args.next() {
        if arg == OsStr::new(END_OF_FLAGS) {
            // Whatever follows is a path, whatever it looks like. A bare trailing `--` (nothing
            // after it) is simply "no positional argument", same as no arguments at all.
            if found.is_none() {
                found = args.next().map(OsStr::to_os_string);
            }
            break;
        }
        if arg == OsStr::new(SSH_FLAG) {
            let host = ssh_host(args.next())?;
            if ssh.replace(host).is_some() {
                return Err(String::from("--ssh takes one host"));
            }
            continue;
        }
        // After the path only `--ssh` is still looked for; nothing else there is read.
        if found.is_some() {
            continue;
        }
        // Byte-wise, not `to_string_lossy().starts_with()`: a non-UTF-8 path must be classified
        // by what it actually starts with, not by what a replacement character makes of it.
        if arg.as_bytes().first() == Some(&b'-') {
            check_flag(arg)?;
            continue;
        }
        found = Some(arg.to_os_string());
    }
    if let Some(host) = ssh {
        let path = found.ok_or_else(|| format!("--ssh {host} needs the project's path on the server after it"))?;
        return Ok(RootSource::Remote {
            host,
            path: PathBuf::from(path),
        });
    }
    if let Some(path) = found {
        return Ok(RootSource::Argument(path));
    }
    Ok(match env {
        // An empty value is treated as unset: `export EITRI_PROJECT_DIR=` (or a launcher
        // interpolating an unset variable) is a far likelier explanation than a request to open a
        // project whose path is the empty string.
        Some(value) if !value.is_empty() => RootSource::Env(value.to_os_string()),
        _ => RootSource::Cwd,
    })
}

/// The token after `--ssh`: an OpenSSH destination in UTF-8, refused before any ssh runs when it is
/// missing, looks like an option, or holds whitespace or a control character.
fn ssh_host(value: Option<&OsStr>) -> Result<String, String> {
    let host = value
        .ok_or_else(|| String::from("--ssh needs a host (eitri --ssh <host> <path>)"))?
        .to_str()
        .ok_or_else(|| String::from("--ssh needs a host in UTF-8"))?;
    agent::ssh::validate_host(host).map_err(|why| format!("--ssh {host:?}: {why}"))?;
    Ok(host.to_owned())
}

/// A `-...` token before `--`: one of [`KNOWN_FLAGS`], or the sentence refusing it.
fn check_flag(arg: &OsStr) -> Result<(), String> {
    if KNOWN_FLAGS.iter().any(|flag| arg == OsStr::new(flag)) {
        return Ok(());
    }
    Err(format!(
        "unrecognized option {arg:?} -- shell accepts {} and {SSH_FLAG} <host>; if that was meant to be a \
         project directory, pass it after `--` (shell -- {arg:?})",
        KNOWN_FLAGS.join(" and ")
    ))
}

/// Turns a chosen source into a real, absolute, symlink-free directory, or into the message
/// `main()` should print.
///
/// `cwd` is passed in rather than read here so the "this process has no working directory" branch
/// (a deleted or unreadable cwd -- rare, but the exact case the old `.expect("cwd")` turned into a
/// bare panic) is reachable from a test.
fn canonicalize_source(source: &RootSource, cwd: io::Result<PathBuf>) -> Result<PathBuf, String> {
    let raw = match source {
        RootSource::Remote { .. } => return Err(String::from(SSH_ONLY_IN_A_WINDOW)),
        RootSource::Argument(path) | RootSource::Env(path) => PathBuf::from(path),
        RootSource::Cwd => cwd.map_err(|e| {
            format!(
                "cannot determine the current working directory ({e}) -- \
                 pass a project directory or set {PROJECT_DIR_ENV}"
            )
        })?,
    };
    // Canonicalized, not merely made absolute. This one path is handed to the nvim child, to the
    // agent panel and to the bottom terminal's shell, and `agent`'s lease and conversation records
    // key on a "canonical cwd" of their own -- two spellings of one directory (a symlink, a `..`, a
    // relative argument) would read downstream as two different projects.
    let resolved = raw
        .canonicalize()
        .map_err(|e| format!("cannot open {} as a project directory: {e}", source.describe()))?;
    // `canonicalize` succeeds for a regular file, so `shell README.md` would otherwise reach nvim
    // and the agent as a cwd and fail two different ways, none of them here.
    if !resolved.is_dir() {
        return Err(format!(
            "{} is not a directory ({})",
            source.describe(),
            resolved.display()
        ));
    }
    Ok(resolved)
}

/// The line of `eitri + file`, nvim's bare `+`: the end of the buffer. `+4294967295` reads the same,
/// which is where nvim would put a line past the end anyway.
pub const LAST_LINE: u32 = u32::MAX;

/// One file for the editor, and where the cursor goes in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTarget {
    /// Absolute, valid UTF-8 and canonical; for a file that does not exist yet, its parent is.
    pub path: PathBuf,
    /// Only the first target has one: nvim runs a `+cmd` on the first file.
    pub line: Option<u32>,
    pub col: Option<u32>,
}

/// What the window's command line asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// The project root, canonical.
    pub root: PathBuf,
    /// The folders above `root` up to the first file's git top, nearest first: a running window on one of them is
    /// a window of the same project. Empty unless the root came from a file and sits below that top.
    pub wider_roots: Vec<PathBuf>,
    /// What nvim opens, first shown. Empty when no file was named.
    pub open: Vec<OpenTarget>,
    /// `--new-window` before `--`.
    pub new_window: bool,
    /// `--raise` before `--`: a window of this root, if one runs, is asked to come forward.
    pub raise: bool,
    /// Sentences for stderr, without the `eitri: ` the caller puts in front: what was named and is not honoured in
    /// full.
    pub notes: Vec<String>,
    /// `--ssh <host>`: the project at `root` is on `host`, and `root` is the path as typed, which this machine has
    /// as the same directory.
    pub ssh: Option<String>,
    /// `--local`: a remote project's copy here opens as a local project.
    pub local: bool,
}

/// How many repositories the search for a usable top looks at. A file under more unusable
/// repositories than this roots at its folder.
const GIT_TOP_ROUNDS: usize = 4;

impl Launch {
    /// Whether this launch is a request to a running window to come forward. `--new-window` says it wants a window of
    /// its own, and a file goes the way every file goes, by a hand-over.
    pub fn asks_for_a_raise(&self) -> bool {
        self.raise && self.open.is_empty() && !self.new_window
    }
}

/// [`resolve_launch_in`] from this process's arguments, environment, working directory and account.
pub fn resolve_launch() -> Result<Launch, String> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // Canonical, because the walk for a git top compares it with a canonical path's ancestors.
    let home = std::env::var_os("HOME").and_then(|home| PathBuf::from(home).canonicalize().ok());
    resolve_launch_in(
        &args,
        std::env::var_os(PROJECT_DIR_ENV),
        std::env::current_dir(),
        home.as_deref(),
        &Identity::current(),
    )
}

/// The window's command line: `[flags] [+N|+] [DIR] [FILE...] [-- PATH...]`, flags anywhere before
/// `--`. `home` must be canonical.
///
/// With no path this is [`resolve_in`]. The first path decides the rest: a directory is the root,
/// usable or not, as `eitri DIR` always was, and every further path must be a file; a file starts
/// file mode, the root comes from [`root_for_file`], and again every path must be a file. A name that
/// does not exist yet is a file, when its parent does.
///
/// `--raise` takes exactly one path, after `--`, and that path must exist: it names a project someone
/// picked, and a folder gone since then must not become a new buffer in a window rooted at its parent.
pub fn resolve_launch_in(
    args: &[OsString],
    env: Option<OsString>,
    cwd: io::Result<PathBuf>,
    home: Option<&Path>,
    who: &Identity,
) -> Result<Launch, String> {
    let parsed = parse_launch_args(args)?;
    if parsed.raise && !parsed.is_one_verbatim_path() {
        return Err(String::from("--raise takes exactly one path after --"));
    }
    if let Some(host) = &parsed.ssh {
        return remote_launch(host, &parsed);
    }
    if parsed.paths.is_empty() {
        if let Some(spec) = parsed.line_spec {
            return Err(needs_a_file(&spec.token));
        }
        return Ok(Launch {
            root: resolve_from(std::iter::empty(), env, cwd)?,
            wider_roots: Vec::new(),
            open: Vec::new(),
            new_window: parsed.new_window,
            raise: parsed.raise,
            notes: Vec::new(),
            ssh: None,
            local: parsed.local,
        });
    }

    let cwd = cwd.map_err(|e| e.to_string());
    let mut directory = None;
    let mut open = Vec::new();
    for (index, path) in parsed.paths.iter().enumerate() {
        match classify(path, &cwd, parsed.raise)? {
            Named::File(target) => open.push(target),
            Named::Directory(found) if index == 0 => directory = Some(found),
            Named::Directory(_) => {
                return Err(format!(
                    "only the first path may be a directory, and {:?} is one",
                    path.name
                ));
            }
        }
    }

    let mut notes = Vec::new();
    for later in open.iter_mut().skip(1) {
        if later.line.take().is_some() {
            later.col = None;
            notes.push(format!(
                "a line applies to the first file only, dropped for {}",
                later.path.display()
            ));
        }
    }
    match (open.first_mut(), parsed.line_spec) {
        (Some(first), Some(spec)) => {
            first.line = Some(spec.line);
            first.col = None;
        }
        (None, Some(spec)) => return Err(needs_a_file(&spec.token)),
        (_, None) => {}
    }

    let (root, wider_roots) = match directory {
        Some(directory) => (directory, Vec::new()),
        None => {
            let cwd = cwd.ok().and_then(|cwd| cwd.canonicalize().ok());
            // With a first path that is no directory, `open` holds it.
            root_for_file(&open[0].path, cwd.as_deref(), home, who)
        }
    };
    for target in &open {
        if !target.path.starts_with(&root) {
            notes.push(format!(
                "{} is outside the project {}",
                target.path.display(),
                root.display()
            ));
        }
    }
    Ok(Launch {
        root,
        wider_roots,
        open,
        new_window: parsed.new_window,
        raise: parsed.raise,
        notes,
        ssh: None,
        local: parsed.local,
    })
}

/// What a remote project's path on this machine must be until a mirror exists to make it: the same directory, so
/// the editor, the terminal and the agent's paths agree.
pub const REMOTE_PATH_NEEDED: &str = "remote projects need the same path on this machine";

/// The explicit SSH destination before checking for a local mirror.
pub fn ssh_request(args: &[OsString]) -> Result<Option<(String, PathBuf)>, String> {
    let parsed = parse_launch_args(args)?;
    if parsed.raise && !parsed.is_one_verbatim_path() {
        return Err(String::from("--raise takes exactly one path after --"));
    }
    parsed
        .ssh
        .as_ref()
        .map(|host| remote_root(host, &parsed).map(|root| (host.clone(), root)))
        .transpose()
}

fn remote_root(host: &str, parsed: &ParsedLaunch) -> Result<PathBuf, String> {
    if parsed.local {
        return Err(String::from("--ssh and --local cannot be given together"));
    }
    if parsed.line_spec.is_some() {
        return Err(String::from("--ssh opens a project folder, not a file at a line"));
    }
    let [path] = parsed.paths.as_slice() else {
        return Err(format!(
            "--ssh {host} takes one project path (eitri --ssh <host> <path>)"
        ));
    };
    let root = PathBuf::from(&path.name);
    let text = root
        .to_str()
        .ok_or_else(|| format!("{:?}: a remote project's path must be UTF-8", path.name))?;
    if !is_normal_absolute(text) {
        return Err(format!(
            "{text:?}: --ssh needs the project's absolute path on the server, with no trailing slash, `.` or `..`"
        ));
    }
    Ok(root)
}

/// An ordinary remote launch opens the mirror at the same path on this machine.
fn remote_launch(host: &str, parsed: &ParsedLaunch) -> Result<Launch, String> {
    let root = remote_root(host, parsed)?;
    let text = root.display();
    match std::fs::metadata(&root) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "{text} is not a directory on this machine; {REMOTE_PATH_NEEDED}"
            ))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(format!(
                "{text}: {REMOTE_PATH_NEEDED}; {}",
                crate::remote_project::missing_mirror_hint(host, &root)
            ))
        }
        Err(e) => {
            return Err(format!(
                "cannot open {text} on this machine ({e}); {REMOTE_PATH_NEEDED}"
            ))
        }
    }
    let canonical = root
        .canonicalize()
        .map_err(|e| format!("cannot open {text} on this machine ({e}); {REMOTE_PATH_NEEDED}"))?;
    if canonical != root {
        return Err(format!(
            "{text} is {} on this machine; a remote project needs the same path on both machines",
            canonical.display()
        ));
    }
    Ok(Launch {
        root,
        wider_roots: Vec::new(),
        open: Vec::new(),
        new_window: parsed.new_window,
        raise: parsed.raise,
        notes: Vec::new(),
        ssh: Some(host.to_owned()),
        local: false,
    })
}

/// `/`, or an absolute path whose components are all non-empty and none is `.` or `..`: the form a remote
/// project's record and its server use.
fn is_normal_absolute(path: &str) -> bool {
    path == "/"
        || (path.starts_with('/')
            && path[1..]
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."))
}

/// The arguments for nvim after `--embed`. Every path starts with `/`, so none can read as an option
/// or a command, and the column goes through `cursor()` because a `+N` has no column.
pub fn nvim_open_args(open: &[OpenTarget]) -> Vec<String> {
    let position = open.first().and_then(|first| match (first.line?, first.col) {
        (line, Some(col)) => Some(format!("+call cursor({line},{col})")),
        (LAST_LINE, None) => Some(String::from("+")),
        (line, None) => Some(format!("+{line}")),
    });
    position
        .into_iter()
        .chain(open.iter().map(|target| target.path.to_string_lossy().into_owned()))
        .collect()
}

/// A `+` or `+N` on the command line, with its token for the message that may have to quote it.
struct LineSpec {
    token: OsString,
    line: u32,
}

/// One path as typed.
struct PathArg {
    name: OsString,
    /// Given after `--`: the name is the whole name, with no `:LINE` to find in it.
    verbatim: bool,
}

struct ParsedLaunch {
    new_window: bool,
    raise: bool,
    local: bool,
    ssh: Option<String>,
    line_spec: Option<LineSpec>,
    paths: Vec<PathArg>,
}

impl ParsedLaunch {
    /// One path after `--` and nothing that places a cursor, all a `--raise` may carry.
    fn is_one_verbatim_path(&self) -> bool {
        self.line_spec.is_none() && matches!(self.paths.as_slice(), [only] if only.verbatim)
    }
}

/// The tokens, read with no filesystem call. Before `--` a `-...` token is a flag or an error wherever
/// it stands, and a `+...` token is a line spec before the first path or an error; after `--` every
/// token is a path.
fn parse_launch_args(args: &[OsString]) -> Result<ParsedLaunch, String> {
    let mut parsed = ParsedLaunch {
        new_window: false,
        raise: false,
        local: false,
        ssh: None,
        line_spec: None,
        paths: Vec::new(),
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == OsStr::new(END_OF_FLAGS) {
            break;
        }
        if arg == OsStr::new(SSH_FLAG) {
            let host = ssh_host(args.next().map(OsString::as_os_str))?;
            if parsed.ssh.replace(host).is_some() {
                return Err(String::from("--ssh takes one host"));
            }
            continue;
        }
        match arg.as_bytes().first() {
            Some(b'-') => {
                check_flag(arg)?;
                parsed.new_window |= arg == OsStr::new("--new-window");
                parsed.raise |= arg == OsStr::new("--raise");
                parsed.local |= arg == OsStr::new("--local");
            }
            Some(b'+') => match line_of_spec(arg.as_bytes()) {
                Some(line) if parsed.paths.is_empty() => {
                    parsed.line_spec = Some(LineSpec {
                        token: arg.clone(),
                        line,
                    });
                }
                _ => return Err(looks_like_a_command(arg)),
            },
            _ => parsed.paths.push(PathArg {
                name: arg.clone(),
                verbatim: false,
            }),
        }
    }
    parsed.paths.extend(args.map(|name| PathArg {
        name: name.clone(),
        verbatim: true,
    }));
    Ok(parsed)
}

fn line_of_spec(token: &[u8]) -> Option<u32> {
    match token {
        b"+" => Some(LAST_LINE),
        [b'+', digits @ ..] => number(digits),
        _ => None,
    }
}

/// Plain ASCII digits fitting `u32`; `str::parse` alone would also take a leading `+`.
fn number(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

fn looks_like_a_command(token: &OsStr) -> String {
    let token = token.to_string_lossy();
    format!("{token} looks like an nvim command. To open a file with that name use ./{token} or eitri -- {token}")
}

fn needs_a_file(token: &OsStr) -> String {
    format!("{} needs a file", token.to_string_lossy())
}

/// What a path on the command line turned out to name.
enum Named {
    Directory(PathBuf),
    File(OpenTarget),
}

/// `name` made absolute. With no working directory only an absolute name means anything.
fn anchored(name: &OsStr, cwd: &Result<PathBuf, String>) -> Result<PathBuf, String> {
    let path = Path::new(name);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    match cwd {
        Ok(cwd) => Ok(cwd.join(path)),
        Err(why) => Err(format!(
            "cannot open {name:?}: cannot determine the current working directory ({why})"
        )),
    }
}

/// Reads one path: stats it, follows it, and refuses what nvim cannot open. Nothing is opened, so a
/// FIFO is refused without blocking on it. With `existing_only` a missing name is refused where it was
/// found to be missing, so nothing stands between that look and what is done with it.
fn classify(arg: &PathArg, cwd: &Result<PathBuf, String>, existing_only: bool) -> Result<Named, String> {
    // Joined to a cwd, an empty name would be the cwd.
    if arg.name.is_empty() {
        return Err(String::from("cannot open an empty path"));
    }
    let (name, line, col) = if arg.verbatim {
        (arg.name.clone(), None, None)
    } else {
        split_position(&arg.name, cwd)?
    };
    let path = anchored(&name, cwd)?;
    let resolved = match path.symlink_metadata() {
        Ok(_) => {
            let canonical = path.canonicalize().map_err(|e| format!("cannot open {name:?}: {e}"))?;
            let metadata = canonical.metadata().map_err(|e| format!("cannot open {name:?}: {e}"))?;
            if metadata.is_dir() {
                return Ok(Named::Directory(canonical));
            }
            if !metadata.is_file() {
                return Err(format!("{name:?} is not a regular file"));
            }
            canonical
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if existing_only {
                return Err(format!("{name:?} is not there"));
            }
            new_file_path(&name, &path)?
        }
        Err(e) => return Err(format!("cannot open {name:?}: {e}")),
    };
    if resolved.to_str().is_none() {
        return Err(format!("cannot open {name:?}: path is not valid UTF-8"));
    }
    Ok(Named::File(OpenTarget {
        path: resolved,
        line,
        col,
    }))
}

/// Where a file that does not exist yet would be: its canonical parent and its name. `path` has been
/// seen not to exist. A name that can only be a directory (`foo/`, `foo/.`) is not a new file.
fn new_file_path(name: &OsStr, path: &Path) -> Result<PathBuf, String> {
    let bytes = name.as_bytes();
    let last = bytes.rsplit(|b| *b == b'/').next().unwrap_or(bytes);
    let missing = || format!("cannot open {name:?}: no such directory");
    if matches!(last, b"" | b"." | b"..") {
        return Err(missing());
    }
    let (Some(parent), Some(file_name)) = (path.parent(), path.file_name()) else {
        return Err(missing());
    };
    let parent = parent.canonicalize().map_err(|_| missing())?;
    if !parent.is_dir() {
        return Err(missing());
    }
    Ok(parent.join(file_name))
}

/// `name:LINE` and `name:LINE:COL`, each read only when the whole name does not exist and the shorter
/// name does. Looked at in that order, so one suffix comes off first: `a:5:7` with a file `a:5` is
/// line 7 of that file. A directory takes no line, and leaves the name whole.
fn split_position(name: &OsStr, cwd: &Result<PathBuf, String>) -> Result<(OsString, Option<u32>, Option<u32>), String> {
    let whole = (name.to_os_string(), None, None);
    let exists = !matches!(
        anchored(name, cwd)?.symlink_metadata(),
        Err(e) if e.kind() == io::ErrorKind::NotFound
    );
    if exists {
        return Ok(whole);
    }
    let is_a_file = |short: &[u8]| -> Result<bool, String> {
        let metadata = anchored(OsStr::from_bytes(short), cwd)?.metadata();
        Ok(metadata.is_ok_and(|metadata| !metadata.is_dir()))
    };
    let Some((short, last)) = number_suffix(name.as_bytes()) else {
        return Ok(whole);
    };
    if is_a_file(short)? {
        return Ok((OsStr::from_bytes(short).to_os_string(), Some(last), None));
    }
    let Some((shorter, line)) = number_suffix(short) else {
        return Ok(whole);
    };
    if is_a_file(shorter)? {
        return Ok((OsStr::from_bytes(shorter).to_os_string(), Some(line), Some(last)));
    }
    Ok(whole)
}

/// `name:N` as `name` and `N`; `None` when the tail is no number or nothing stands before the colon.
fn number_suffix(name: &[u8]) -> Option<(&[u8], u32)> {
    let colon = name.iter().rposition(|b| *b == b':')?;
    let (short, digits) = (&name[..colon], &name[colon + 1..]);
    if short.is_empty() {
        return None;
    }
    Some((short, number(digits)?))
}

/// The root for a file, which is canonical. Never refuses: a root that fails [`is_usable`] is what
/// the permission gate and the panel's note are for, and a file you asked for still opens.
///
/// The nearest usable git top holds the project, and a usable cwd inside it narrows that to where the
/// user is working. With no top, a usable cwd holding the file serves, else the file's own folder.
///
/// Second in the pair: the folders a narrowed root leaves out, up to that top. They are `Launch::wider_roots`.
fn root_for_file(file: &Path, cwd: Option<&Path>, home: Option<&Path>, who: &Identity) -> (PathBuf, Vec<PathBuf>) {
    let folder = file.parent().unwrap_or(file);
    let cwd = cwd.filter(|cwd| file.starts_with(cwd) && is_usable(cwd, home, who));
    match usable_git_top(folder, home, who) {
        Some(top) => match cwd {
            Some(cwd) if cwd.starts_with(&top) => {
                let above = cwd.ancestors().skip(1).take_while(|dir| dir.starts_with(&top));
                (cwd.to_path_buf(), above.map(Path::to_path_buf).collect())
            }
            _ => (top, Vec::new()),
        },
        None => (cwd.unwrap_or(folder).to_path_buf(), Vec::new()),
    }
}

/// The test the gate applies. `dir` must be canonical: a spelling through a link reads as a root that
/// has moved.
fn is_usable(dir: &Path, home: Option<&Path>, who: &Identity) -> bool {
    refusal(dir, home, who).is_none()
}

/// The nearest git top above `start` that is usable and whose `.git` is too. An unusable top does not
/// end the search, which goes on from its parent; nothing above a `$HOME` that `start` is under is
/// looked at, as in trust discovery, and nothing there could be usable.
fn usable_git_top(start: &Path, home: Option<&Path>, who: &Identity) -> Option<PathBuf> {
    let stop_at = home.filter(|home| start.starts_with(home));
    let mut from = start.to_path_buf();
    for _ in 0..GIT_TOP_ROUNDS {
        let walk = git_top_level(&from, stop_at, Limits::default().max_walk_up);
        let top = walk.top.as_deref()?;
        if is_usable(top, home, who) && git_dir_is_private(top, &walk, who) {
            return walk.top;
        }
        if stop_at == Some(top) {
            return None;
        }
        from = top.parent()?.to_path_buf();
    }
    None
}

/// Whether nobody else can change `top`'s `.git`, and for a linked worktree its common directory:
/// whoever can write hooks or config there runs code in every session at this root. Tested with
/// `lstat`, as the root is.
fn git_dir_is_private(top: &Path, walk: &GitWalk, who: &Identity) -> bool {
    let private = |path: &Path| {
        path.symlink_metadata()
            .is_ok_and(|metadata| open_to_others(metadata.uid(), metadata.gid(), metadata.mode(), who).is_none())
    };
    let common = walk
        .main_checkout
        .as_ref()
        .map(|checkout| checkout.join(".git"))
        .or_else(|| walk.unplaced_common.clone());
    private(&top.join(".git")) && common.is_none_or(|common| private(&common))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_trust::nofollow::tests::{canonical_scratch, mkfifo};
    use agent::project_boundary::NotABoundary;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn open_args_need_nvim() {
        for args in [os(&[]), os(&["project"]), os(&["--nvim"]), os(&["--nvim", ""])] {
            assert!(parse_open_args(&args).unwrap_err().contains("--nvim"));
        }
    }

    #[test]
    fn open_args_refuse_new_window() {
        let error = parse_open_args(&os(&["--nvim", "/r/n", "--new-window"])).unwrap_err();
        assert!(error.contains("--new-window"), "{error}");
    }

    #[test]
    fn open_args_never_read_nvim_env() {
        const CHILD: &str = "EITRI_TEST_TAKEOVER_ENV_CHILD";
        if std::env::var_os(CHILD).is_some() {
            assert!(std::env::var_os("NVIM").is_some());
            assert!(parse_open_args(&[]).is_err());
            let parsed = parse_open_args(&os(&["--nvim", "explicit"])).unwrap();
            assert_eq!(parsed.nvim, "explicit");
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "project_root::tests::open_args_never_read_nvim_env"])
            .env(CHILD, "1")
            .env("NVIM", "/r/inherited")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
    }

    #[test]
    fn open_args_take_a_relative_dir() {
        let parsed = parse_open_args(&os(&["--nvim", "/r/a b", "../a directory"])).unwrap();
        assert_eq!(parsed.nvim, "/r/a b");
        assert_eq!(parsed.dir, Some(PathBuf::from("../a directory")));
        assert_eq!(parse_open_args(&os(&["--nvim", "addr"])).unwrap().dir, None);
        assert_eq!(
            parse_open_args(&os(&["--nvim", "addr", "--", "-project"])).unwrap().dir,
            Some(PathBuf::from("-project"))
        );
        assert!(parse_open_args(&os(&["--nvim", "addr", "one", "two"])).is_err());
        assert!(parse_open_args(&os(&["--nvim", "addr", "--unknown"])).is_err());
    }

    #[test]
    fn page_args_defaults_and_refusals() {
        assert_eq!(
            parse_page_args(&os(&["notes"])),
            Ok(PageArgs {
                id: "notes".into(),
                nvim: None,
                dir: None,
                list: false,
            })
        );
        let listed = parse_page_args(&os(&["--list"])).unwrap();
        assert!(listed.list);
        assert!(listed.id.is_empty() && listed.nvim.is_none() && listed.dir.is_none());
        for flag in ["--account", "--quiet", "--clean", "--legacy", "--new-window", "--bogus"] {
            let err = parse_page_args(&os(&["notes", flag])).unwrap_err();
            assert!(err.contains(flag), "{err}");
        }
        for args in [
            vec![],
            vec!["../notes"],
            vec!["Notes"],
            vec!["-notes"],
            vec!["notes-"],
            vec!["notes", "one", "two"],
            vec!["notes", "--nvim"],
            vec!["--list", "notes"],
        ] {
            assert!(parse_page_args(&os(&args)).is_err(), "{args:?}");
        }
        assert!(parse_page_args(&[OsString::from("a".repeat(41))]).is_err());
        assert!(parse_page_args(&[OsString::from("a".repeat(40))]).is_ok());
        assert_eq!(
            parse_page_args(&os(&["notes", "--", "--clean"])).unwrap().dir,
            Some("--clean".into())
        );
    }

    #[test]
    fn page_args_take_a_path_with_spaces_and_chinese() {
        let page = parse_page_args(&os(&["notes", "--nvim", "/tmp/编辑 器.addr", "/work/中文 项目"])).unwrap();
        assert_eq!(page.id, "notes");
        assert_eq!(page.nvim, Some(PathBuf::from("/tmp/编辑 器.addr")));
        assert_eq!(page.dir, Some(PathBuf::from("/work/中文 项目")));
        use std::os::unix::ffi::OsStringExt;
        let bytes = OsString::from_vec(b"/work/invalid-\xff".to_vec());
        assert_eq!(
            parse_page_args(&["notes".into(), bytes.clone()]).unwrap().dir,
            Some(PathBuf::from(bytes))
        );
        assert!(parse_page_args(&[OsString::from_vec(vec![0xff])]).is_err());
    }

    /// `select_root_source` over `&str` literals, which is what almost every case below wants.
    fn select<'a>(args: impl IntoIterator<Item = &'a str>, env: Option<&str>) -> Result<RootSource, String> {
        let args: Vec<OsString> = args.into_iter().map(OsString::from).collect();
        select_root_source(args.iter().map(OsString::as_os_str), env.map(OsStr::new))
    }

    fn argument(path: &str) -> RootSource {
        RootSource::Argument(OsString::from(path))
    }

    #[test]
    fn a_positional_argument_wins_over_everything() {
        assert_eq!(
            select(["/srv/project"], Some("/from/env")),
            Ok(argument("/srv/project"))
        );
    }

    #[test]
    fn flags_are_not_positional_arguments() {
        // `shell --clean` must still resolve to the cwd, not to a project named "--clean" -- it
        // is a real flag, matched by `main()` itself.
        assert_eq!(select(["--clean"], None), Ok(RootSource::Cwd));
        // ...and a real path after it is still found.
        assert_eq!(select(["--clean", "/srv/project"], None), Ok(argument("/srv/project")));
    }

    #[test]
    fn version_is_a_known_flag_and_never_a_project_directory() {
        // `main()` intercepts `--version` before `resolve_launch()`, but this module must still treat it
        // as a known flag like `--clean`, never a project directory named "--version".
        assert_eq!(select(["--version"], None), Ok(RootSource::Cwd));
        // `shell --version /x` is not an error for the resolver -- the flag is skipped and the
        // real positional argument after it is still found, the same shape as `--clean` above.
        assert_eq!(select(["--version", "/x"], None), Ok(argument("/x")));
    }

    #[test]
    fn legacy_is_a_known_flag_in_every_build_and_never_a_project_directory() {
        // `main()` intercepts `--legacy` before `resolve_launch()` when legacy was not compiled in.
        // Listing it here regardless of that feature preserves the real `LEGACY_NOT_IN_BUILD`
        // diagnostic instead of reporting an unrecognized option.
        assert_eq!(select(["--legacy"], None), Ok(RootSource::Cwd));
        assert_eq!(select(["--legacy", "/x"], None), Ok(argument("/x")));
    }

    #[test]
    fn an_unrecognized_option_is_an_error_rather_than_a_silent_fallback_to_the_cwd() {
        // `shell -myproj` and a mistyped `--clen` must report an error, never silently open cwd.
        let err = select(["-myproj"], None).expect_err("a leading-dash token must not be ignored");
        assert!(err.contains("-myproj"), "got {err}");
        assert!(err.contains("--"), "the message must point at the escape hatch: {err}");
        let err = select(["--clen"], None).expect_err("a mistyped flag must not be ignored");
        assert!(err.contains("--clean"), "the message must list the real flags: {err}");
    }

    #[test]
    fn a_double_dash_ends_flag_parsing() {
        // The escape hatch the error above advertises has to actually work, including for a name
        // that is character-for-character one of the real flags.
        assert_eq!(select(["--", "-myproj"], None), Ok(argument("-myproj")));
        // The load-bearing half, and the reason this is not a duplicate of the line above: the
        // token after `--` here *is* a member of `KNOWN_FLAGS`, so this is the only assertion in
        // the suite that pins `END_OF_FLAGS` short-circuiting the `KNOWN_FLAGS` membership test
        // rather than merely beating the leading-`-` check. Reorder those two in
        // `select_root_source` and `shell -- --clean` silently swallows the token as a flag and
        // opens the cwd instead of the directory literally named `--clean` -- the exact
        // silent-wrong-project outcome this module exists to make unreachable.
        // `--clean` exercises a known flag after the separator; `--terminal` below is unknown.
        assert_eq!(select(["--", "--clean"], None), Ok(argument("--clean")));
        assert_eq!(
            select(["--clean", "--", "--terminal"], None),
            Ok(argument("--terminal"))
        );
        // A bare trailing `--` is "no positional argument", not an error and not an empty path.
        assert_eq!(
            select(["--clean", "--"], Some("/from/env")),
            Ok(RootSource::Env(OsString::from("/from/env")))
        );
    }

    #[test]
    fn a_non_utf8_argument_is_a_path_like_any_other() {
        use std::os::unix::ffi::OsStringExt;
        // `std::env::args()` panics on this input; `args_os()` and `OsString` preserve it for reporting.
        let raw = OsString::from_vec(b"/tmp/\xff-not-valid-utf8".to_vec());
        let args = [raw.clone()];
        assert_eq!(
            select_root_source(args.iter().map(OsString::as_os_str), None),
            Ok(RootSource::Argument(raw.clone()))
        );
        // ...and it reaches a real error message rather than a panic, with its bytes escaped
        // rather than dropped.
        let err = canonicalize_source(&RootSource::Argument(raw), Ok(PathBuf::from("/tmp")))
            .expect_err("a path that cannot exist must be an error");
        assert!(err.contains("not-valid-utf8"), "got {err}");
        assert!(err.contains("\\xFF") || err.contains("\\xff"), "got {err}");
    }

    #[test]
    fn the_environment_is_used_only_when_no_argument_was_given() {
        assert_eq!(
            select([], Some("/from/env")),
            Ok(RootSource::Env(OsString::from("/from/env")))
        );
    }

    #[test]
    fn an_empty_environment_value_counts_as_unset() {
        // `export EITRI_PROJECT_DIR=` is a common shell accident; canonicalizing "" fails with a
        // bare ENOENT that reads as a missing directory rather than as an unset variable.
        assert_eq!(select([], Some("")), Ok(RootSource::Cwd));
    }

    #[test]
    fn nothing_at_all_falls_back_to_the_cwd() {
        assert_eq!(select([], None), Ok(RootSource::Cwd));
    }

    #[test]
    fn flag_given_finds_a_flag_before_the_double_dash() {
        let args: Vec<OsString> = ["--legacy", "--", "/proj"].iter().map(OsString::from).collect();
        assert!(flag_given(args.iter().map(OsString::as_os_str), "--legacy"));
    }

    #[test]
    fn flag_given_does_not_find_a_flag_after_the_double_dash() {
        // `shell -- --version` must open a directory
        // literally named `--version`, not have `main()`'s own early scan still detect the flag
        // past the `--`.
        let args: Vec<OsString> = ["--", "--version"].iter().map(OsString::from).collect();
        assert!(!flag_given(args.iter().map(OsString::as_os_str), "--version"));
    }

    #[test]
    fn flag_given_works_with_no_double_dash_present() {
        let args: Vec<OsString> = ["--clean"].iter().map(OsString::from).collect();
        assert!(flag_given(args.iter().map(OsString::as_os_str), "--clean"));
        assert!(!flag_given(args.iter().map(OsString::as_os_str), "--version"));
    }

    #[test]
    fn a_real_directory_canonicalizes_to_an_absolute_path() {
        let dir = std::env::temp_dir();
        let resolved = canonicalize_source(
            &argument(&dir.display().to_string()),
            Ok(PathBuf::from("/nowhere-this-must-not-be-used")),
        )
        .expect("a real directory must resolve");
        assert!(resolved.is_absolute());
        assert_eq!(resolved, dir.canonicalize().expect("temp_dir exists"));
    }

    #[test]
    fn a_missing_directory_is_an_error_naming_both_the_path_and_where_it_came_from() {
        let err = canonicalize_source(
            &RootSource::Env(OsString::from("/definitely/not/a/real/eitri/project")),
            Ok(PathBuf::from("/tmp")),
        )
        .expect_err("a missing directory must not silently fall back to the cwd");
        assert!(err.contains("/definitely/not/a/real/eitri/project"), "got {err}");
        assert!(err.contains(PROJECT_DIR_ENV), "got {err}");
    }

    #[test]
    fn a_file_is_rejected_rather_than_used_as_a_project_root() {
        // `shell README.md` would otherwise hand a regular file to nvim's cwd and to the agent's
        // project_dir, each of which fails differently and later.
        let file = std::env::temp_dir().join(format!(
            "eitri-project-root-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&file, b"not a directory").expect("write a temp file");
        let err = canonicalize_source(&argument(&file.display().to_string()), Ok(PathBuf::from("/tmp")))
            .expect_err("a regular file must not be accepted as a project root");
        let _ = std::fs::remove_file(&file);
        assert!(err.contains("not a directory"), "got {err}");
    }

    #[test]
    fn an_unreadable_cwd_is_reported_rather_than_panicked_on() {
        // This is the branch the old `std::env::current_dir().expect("cwd")` turned into a panic
        // with no path and no explanation.
        let err = canonicalize_source(
            &RootSource::Cwd,
            Err(io::Error::new(io::ErrorKind::NotFound, "no such directory")),
        )
        .expect_err("an unreadable cwd must be an error");
        assert!(err.contains("working directory"), "got {err}");
        assert!(err.contains("no such directory"), "got {err}");
    }

    /// The seam between the two halves above: `resolve()` (which `resolve_launch()` extends) is only the three reads of
    /// process state, and this is everything it wires together. An args/env swap, or a source
    /// chosen and then a *different* one canonicalized, would show up only here.
    #[test]
    fn the_selected_source_is_the_one_that_gets_canonicalized() {
        let real = std::env::temp_dir().canonicalize().expect("temp_dir exists");
        let missing = "/definitely/not/a/real/eitri/project";

        // Argument beats env, all the way through to the returned path.
        let args = [OsString::from(real.display().to_string())];
        assert_eq!(
            resolve_from(
                args.iter().map(OsString::as_os_str),
                Some(OsString::from(missing)),
                Ok(PathBuf::from("/nowhere")),
            ),
            Ok(real.clone())
        );

        // Env beats cwd, and its failure is reported as the env's, not the cwd's.
        let err = resolve_from(
            std::iter::empty(),
            Some(OsString::from(missing)),
            Ok(PathBuf::from("/nowhere-this-must-not-be-used")),
        )
        .expect_err("a missing env directory must not fall through to the cwd");
        assert!(err.contains(PROJECT_DIR_ENV), "got {err}");

        // Nothing given: the cwd is used, and it is the cwd that was passed in.
        assert_eq!(resolve_from(std::iter::empty(), None, Ok(real.clone())), Ok(real));

        // A flag error short-circuits before any filesystem call -- note the cwd here is fine.
        let args = [OsString::from("-myproj")];
        let err = resolve_from(args.iter().map(OsString::as_os_str), None, Ok(std::env::temp_dir()))
            .expect_err("an unrecognized option must not be resolved past");
        assert!(err.contains("-myproj"), "got {err}");
    }

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn panel_takes_nvim_with_a_value_and_hands_the_rest_on() {
        let parsed = parse_panel_args(&os(&["--nvim", "/r/nvim-addr", "/p"]), None).unwrap();
        assert_eq!(parsed.nvim, Some(PathBuf::from("/r/nvim-addr")));
        assert_eq!(parsed.rest, os(&["/p"]));
        // The option may come after the project too.
        let parsed = parse_panel_args(&os(&["/p", "--nvim", "/r/nvim-addr"]), None).unwrap();
        assert_eq!(
            (parsed.nvim, parsed.rest),
            (Some(PathBuf::from("/r/nvim-addr")), os(&["/p"]))
        );
    }

    #[test]
    fn panel_without_nvim_falls_back_to_the_env_and_ignores_an_empty_one() {
        let parsed = parse_panel_args(&os(&["/p"]), Some(OsString::from("/env/nvim-addr"))).unwrap();
        assert_eq!(parsed.nvim, Some(PathBuf::from("/env/nvim-addr")));
        let parsed = parse_panel_args(&os(&["/p"]), Some(OsString::new())).unwrap();
        assert_eq!(parsed.nvim, None);
        assert_eq!(
            parse_panel_args(&[], None).unwrap(),
            PanelArgs {
                nvim: None,
                rest: vec![]
            }
        );
        // The option beats the environment.
        let parsed = parse_panel_args(&os(&["--nvim", "/a"]), Some(OsString::from("/env"))).unwrap();
        assert_eq!(parsed.nvim, Some(PathBuf::from("/a")));
    }

    #[test]
    fn a_missing_nvim_value_is_refused() {
        assert_eq!(
            parse_panel_args(&os(&["--nvim"]), None),
            Err("--nvim needs an address".to_string())
        );
        assert_eq!(
            parse_panel_args(&os(&["/p", "--nvim"]), Some(OsString::from("/env"))),
            Err("--nvim needs an address".to_string())
        );
    }

    #[test]
    fn double_dash_makes_the_next_token_the_project_even_if_it_starts_with_a_dash() {
        let parsed = parse_panel_args(&os(&["--", "-myproj"]), None).unwrap();
        assert_eq!(parsed.rest, os(&["--", "-myproj"]));
        // `--nvim` after `--` is a project name, not an option.
        let parsed = parse_panel_args(&os(&["--", "--nvim"]), None).unwrap();
        assert_eq!((parsed.nvim, parsed.rest), (None, os(&["--", "--nvim"])));
        // A bare trailing `--` is no project at all.
        assert_eq!(
            parse_panel_args(&os(&["--"]), None).unwrap().rest,
            Vec::<OsString>::new()
        );
        // What the resolver then does with it: the dashed name is read as a path, not refused.
        let err = resolve_args(&parsed_rest(&["--", "-no-such-eitri-dir"])).expect_err("not a directory");
        assert!(err.contains("-no-such-eitri-dir"), "got {err}");
    }

    #[test]
    fn split_takes_no_directory_one_directory_or_one_after_double_dash() {
        assert_eq!(parse_split_args(&[]).unwrap().rest, Vec::<OsString>::new());
        assert_eq!(parse_split_args(&os(&["/p"])).unwrap().rest, os(&["/p"]));
        assert_eq!(parse_split_args(&os(&["--", "/p"])).unwrap().rest, os(&["--", "/p"]));
        assert_eq!(
            parse_split_args(&os(&["--", "-odd"])).unwrap().rest,
            os(&["--", "-odd"])
        );
        // A bare trailing `--` is no project at all.
        assert_eq!(parse_split_args(&os(&["--"])).unwrap().rest, Vec::<OsString>::new());
    }

    #[test]
    fn split_refuses_two_directories_and_every_option_by_name() {
        let two = Err("eitri split: takes at most one project directory".to_string());
        assert_eq!(parse_split_args(&os(&["/a", "/b"])), two);
        assert_eq!(parse_split_args(&os(&["/a", "--", "/b"])), two);
        assert_eq!(parse_split_args(&os(&["--", "/a", "/b"])), two);
        for option in ["--nvim", "--clean", "-x", "--legacy"] {
            assert_eq!(
                parse_split_args(&os(&[option, "/p"])),
                Err(format!("eitri split: unknown option {option}"))
            );
        }
        // After `--` a dashed token is a directory name, not an option.
        assert!(parse_split_args(&os(&["--", "--nvim"])).is_ok());
    }

    fn parsed_rest(args: &[&str]) -> Vec<OsString> {
        parse_panel_args(&os(args), None).unwrap().rest
    }

    #[test]
    fn an_unknown_option_is_refused_by_name() {
        assert_eq!(
            parse_panel_args(&os(&["--clean"]), None),
            Err("eitri panel: unknown option --clean".to_string())
        );
        assert_eq!(
            parse_panel_args(&os(&["--legacy", "/p"]), None),
            Err("eitri panel: unknown option --legacy".to_string())
        );
        assert_eq!(
            parse_panel_args(&os(&["-x"]), None),
            Err("eitri panel: unknown option -x".to_string())
        );
    }

    #[test]
    fn two_positionals_are_refused() {
        assert!(parse_panel_args(&os(&["/a", "/b"]), None).is_err());
        assert!(parse_panel_args(&os(&["/a", "--", "/b"]), None).is_err());
        assert!(parse_panel_args(&os(&["--", "/a", "/b"]), None).is_err());
    }

    #[test]
    fn resolve_args_canonicalizes_like_resolve() {
        let real = std::env::temp_dir().canonicalize().expect("temp_dir exists");
        let through_dot = real.join(".");
        assert_eq!(resolve_args(&[through_dot.into_os_string()]), Ok(real));
        let err = resolve_args(&os(&["/definitely/not/a/real/eitri/project"])).expect_err("a missing directory");
        assert!(err.contains("/definitely/not/a/real/eitri/project"), "got {err}");
    }

    /// The account running the test, with a private group: a directory's mode alone then decides,
    /// under whatever umask the machine has.
    fn me() -> Identity {
        // SAFETY: neither call has preconditions, and neither can fail.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        Identity {
            uid,
            gid,
            private_group: true,
        }
    }

    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A directory only its owner can write to, with its parents.
    fn dir(path: PathBuf) -> PathBuf {
        std::fs::create_dir_all(&path).unwrap();
        chmod(&path, 0o700);
        path
    }

    fn file(path: PathBuf) -> PathBuf {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"text").unwrap();
        path
    }

    /// A minimal git directory by hand, the layout `git_top_level` accepts.
    fn fake_repo(dir: &Path) {
        std::fs::create_dir_all(dir.join(".git/objects")).unwrap();
        std::fs::create_dir_all(dir.join(".git/refs/heads")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
    }

    /// `wt` as a linked worktree of the repository at `main`.
    fn fake_worktree(main: &Path, wt: &Path) {
        let git_dir = main.join(".git/worktrees/wt");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), b"ref: refs/heads/wt\n").unwrap();
        std::fs::write(git_dir.join("commondir"), b"../..\n").unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", git_dir.display())).unwrap();
    }

    fn text(path: &Path) -> &str {
        path.to_str().unwrap()
    }

    fn at(home: &Path, args: &[&str], cwd: &Path) -> Result<Launch, String> {
        resolve_launch_in(&os(args), None, Ok(cwd.to_path_buf()), Some(home), &me())
    }

    fn plain(path: PathBuf) -> OpenTarget {
        OpenTarget {
            path,
            line: None,
            col: None,
        }
    }

    fn placed(path: PathBuf, line: Option<u32>, col: Option<u32>) -> OpenTarget {
        OpenTarget { path, line, col }
    }

    /// A scratch tree and a home inside it, so no walk leaves the tree for a repository the machine
    /// running the test happens to have above its temp dir.
    fn scratch_home(label: &str) -> (impl Drop, PathBuf) {
        let (guard, top) = canonical_scratch(label);
        let home = dir(top.join("home"));
        (guard, home)
    }

    #[test]
    fn a_file_in_the_cwd_of_a_repo_takes_the_repo() {
        let (_s, home) = scratch_home("rl-repo");
        let repo = dir(home.join("repo"));
        fake_repo(&repo);
        let lib = file(repo.join("src/lib.rs"));
        let launch = at(&home, &["src/lib.rs"], &repo).unwrap();
        assert_eq!(launch.root, repo);
        assert_eq!(launch.open, [plain(lib)]);
        assert!(launch.notes.is_empty(), "{:?}", launch.notes);
        assert!(!launch.new_window);
    }

    #[test]
    fn a_subdirectory_cwd_inside_the_repo_wins() {
        let (_s, home) = scratch_home("rl-sub");
        let repo = dir(home.join("repo"));
        fake_repo(&repo);
        let member = dir(repo.join("member"));
        let x = file(member.join("src/x.rs"));
        let launch = at(&home, &["src/x.rs"], &member).unwrap();
        assert_eq!((launch.root, launch.open), (member, vec![plain(x)]));
        assert_eq!(launch.wider_roots, [repo]);
    }

    #[test]
    fn the_wider_roots_climb_from_the_narrowed_root_to_the_git_top_nearest_first() {
        let (_s, home) = scratch_home("rl-wider");
        let repo = dir(home.join("repo"));
        fake_repo(&repo);
        let member = dir(repo.join("crates/member"));
        let crates = repo.join("crates");
        file(member.join("x.rs"));
        let launch = at(&home, &["x.rs"], &member).unwrap();
        assert_eq!(launch.root, member);
        assert_eq!(launch.wider_roots, [crates, repo]);
    }

    /// There is nothing above a root that is the git top, and the search stops at the top of the file's own
    /// repository: a window on an outer repository is another project.
    #[test]
    fn the_wider_roots_stop_at_the_nearest_top_and_only_a_narrowed_root_has_any() {
        let (_s, home) = scratch_home("rl-wider-stop");
        let outer = dir(home.join("outer"));
        fake_repo(&outer);
        let inner = dir(outer.join("vendor/inner"));
        fake_repo(&inner);
        let member = dir(inner.join("member"));
        let x = file(member.join("x.rs"));
        let elsewhere = dir(home.join("elsewhere"));
        let below = dir(elsewhere.join("below"));
        file(below.join("y.rs"));

        let cases: [(&[&str], &Path); 5] = [
            // In the inner repository: up to its top, no further.
            (&["x.rs"], &member),
            // The cwd is the top.
            (&["member/x.rs"], &inner),
            // The cwd does not hold the file, so the root is the top.
            (&[text(&x)], &elsewhere),
            // No repository.
            (&["y.rs"], &below),
            // A directory names its root outright.
            (&[text(&inner), text(&x)], &member),
        ];
        let wider: Vec<Vec<PathBuf>> = cases
            .iter()
            .map(|(args, cwd)| at(&home, args, cwd).unwrap().wider_roots)
            .collect();
        assert_eq!(wider, [vec![inner.clone()], vec![], vec![], vec![], vec![]]);
    }

    #[test]
    fn a_launch_with_no_file_has_no_wider_roots() {
        let (_s, home) = scratch_home("rl-wider-none");
        let repo = dir(home.join("repo"));
        fake_repo(&repo);
        let member = dir(repo.join("member"));
        assert!(at(&home, &[], &member).unwrap().wider_roots.is_empty());
    }

    #[test]
    fn a_cwd_outside_the_repo_gives_the_repo() {
        let (_s, home) = scratch_home("rl-out");
        let repo = dir(home.join("repo"));
        fake_repo(&repo);
        let a = file(repo.join("a.rs"));
        assert_eq!(at(&home, &[text(&a)], &home).unwrap().root, repo);
        // A usable cwd that does not contain the file is no better than the home above.
        let elsewhere = dir(home.join("elsewhere"));
        assert_eq!(at(&home, &[text(&a)], &elsewhere).unwrap().root, repo);
        // Nor is a usable cwd that contains the repo.
        let code = dir(home.join("code"));
        let inner = dir(code.join("inner"));
        fake_repo(&inner);
        let b = file(inner.join("b.rs"));
        assert_eq!(at(&home, &[text(&b)], &code).unwrap().root, inner);
    }

    #[test]
    fn no_repo_and_a_usable_cwd_gives_the_cwd() {
        let (_s, home) = scratch_home("rl-cwd");
        let scratch = dir(home.join("scratch"));
        let a = file(scratch.join("sub/a.py"));
        assert_eq!(at(&home, &["sub/a.py"], &scratch).unwrap().root, scratch);
        assert_eq!(at(&home, &[text(&a)], &scratch).unwrap().root, scratch);
    }

    #[test]
    fn no_repo_and_an_unusable_cwd_gives_the_parent() {
        let (_s, home) = scratch_home("rl-parent");
        let scratch = dir(home.join("code/scratch"));
        let a = file(scratch.join("a.py"));
        assert_eq!(at(&home, &["code/scratch/a.py"], &home).unwrap().root, scratch);
        assert_eq!(at(&home, &[text(&a)], &home).unwrap().root, scratch);
    }

    #[test]
    fn a_home_that_is_a_repo_is_no_git_top() {
        let (_s, home) = scratch_home("rl-homerepo");
        fake_repo(&home);
        let scratch = dir(home.join("code/scratch"));
        let a = file(scratch.join("a.py"));
        assert_eq!(at(&home, &[text(&a)], &home).unwrap().root, scratch);
    }

    #[test]
    fn a_file_in_home_takes_home_as_root() {
        let (_s, home) = scratch_home("rl-home");
        let notes = file(home.join("notes.md"));
        let launch = at(&home, &["notes.md"], &home).unwrap();
        assert_eq!((launch.root, launch.open), (home.clone(), vec![plain(notes.clone())]));
        let elsewhere = dir(home.join("elsewhere"));
        assert_eq!(at(&home, &[text(&notes)], &elsewhere).unwrap().root, home);
    }

    #[test]
    fn an_unusable_parent_is_still_the_root() {
        let (_s, home) = scratch_home("rl-shared");
        let shared = dir(home.join("shared"));
        chmod(&shared, 0o777);
        let a = file(shared.join("a.py"));
        assert_eq!(at(&home, &[text(&a)], &home).unwrap().root, shared);
        // The cwd itself being that folder changes nothing: it is unusable, and the parent is it.
        assert_eq!(at(&home, &["a.py"], &shared).unwrap().root, shared);
    }

    #[test]
    fn a_foreign_owned_dot_git_is_skipped() {
        let (_s, home) = scratch_home("rl-foreign");
        let repo = dir(home.join("repo"));
        fake_repo(&repo);
        let src = dir(repo.join("src"));
        let a = file(src.join("a.rs"));
        chmod(&repo.join(".git"), 0o775);
        let launch = |who: &Identity| resolve_launch_in(&os(&[text(&a)]), None, Ok(home.clone()), Some(&home), who);
        // A private group makes a 0775 `.git` the caller's alone.
        assert_eq!(launch(&me()).unwrap().root, repo);
        // A shared group does not. A test cannot chown, so this is the case where the directory
        // passes and only its `.git` fails.
        let shared = Identity {
            private_group: false,
            ..me()
        };
        assert_eq!(launch(&shared).unwrap().root, src);
        let someone_else = Identity {
            uid: me().uid.wrapping_add(1),
            ..me()
        };
        assert_eq!(launch(&someone_else).unwrap().root, src);
    }

    #[test]
    fn a_dot_git_file_of_a_linked_worktree_is_a_top() {
        let (_s, home) = scratch_home("rl-wt");
        let main = dir(home.join("main"));
        fake_repo(&main);
        let wt = dir(home.join("wt"));
        fake_worktree(&main, &wt);
        let a = file(wt.join("src/a.rs"));
        assert_eq!(at(&home, &[text(&a)], &home).unwrap().root, wt);
    }

    #[test]
    fn a_linked_worktree_of_a_repository_others_can_write_to_is_no_top() {
        let (_s, home) = scratch_home("rl-wt-shared");
        let main = dir(home.join("main"));
        fake_repo(&main);
        chmod(&main.join(".git"), 0o777);
        let wt = dir(home.join("wt"));
        fake_worktree(&main, &wt);
        let src = dir(wt.join("src"));
        let a = file(src.join("a.rs"));
        assert_eq!(at(&home, &[text(&a)], &home).unwrap().root, src);
    }

    #[test]
    fn a_symlinked_dot_git_counts_as_absent() {
        let (_s, home) = scratch_home("rl-gitlink");
        let real = dir(home.join("real"));
        fake_repo(&real);
        let linked = dir(home.join("linked"));
        symlink(real.join(".git"), linked.join(".git")).unwrap();
        let src = dir(linked.join("src"));
        let a = file(src.join("a.rs"));
        assert_eq!(at(&home, &[text(&a)], &home).unwrap().root, src);
    }

    #[test]
    fn a_second_round_finds_the_usable_top_above_an_unusable_one() {
        let (_s, home) = scratch_home("rl-round");
        let outer = dir(home.join("outer"));
        fake_repo(&outer);
        let inner = dir(outer.join("inner"));
        fake_repo(&inner);
        chmod(&inner, 0o777);
        let a = file(inner.join("a.rs"));
        assert_eq!(at(&home, &[text(&a)], &home).unwrap().root, outer);
    }

    #[test]
    fn the_search_for_a_usable_top_stops_after_four_rounds() {
        let (_s, home) = scratch_home("rl-rounds");
        let mut repo = dir(home.join("outer"));
        fake_repo(&repo);
        for _ in 0..GIT_TOP_ROUNDS {
            repo = dir(repo.join("nested"));
            fake_repo(&repo);
            chmod(&repo, 0o777);
        }
        let a = file(repo.join("a.rs"));
        assert_eq!(at(&home, &[text(&a)], &home).unwrap().root, repo);
    }

    #[test]
    fn an_unreadable_cwd_counts_as_absent() {
        let (_s, home) = scratch_home("rl-nocwd");
        let scratch = dir(home.join("scratch"));
        let a = file(scratch.join("a.py"));
        let no_cwd = |name: &str| {
            resolve_launch_in(
                &os(&[name]),
                None,
                Err(io::Error::new(io::ErrorKind::NotFound, "gone")),
                Some(&home),
                &me(),
            )
        };
        assert_eq!(no_cwd(text(&a)).unwrap().root, scratch);
        // A relative name has nothing to be relative to.
        let err = no_cwd("a.py").expect_err("a relative name with no cwd");
        assert!(err.contains("working directory") && err.contains("gone"), "got {err}");
    }

    #[test]
    fn a_symlink_roots_at_its_target() {
        let (_s, home) = scratch_home("rl-link");
        let repo = dir(home.join("repo"));
        fake_repo(&repo);
        let real = file(repo.join("real.md"));
        symlink(&real, home.join("link.md")).unwrap();
        let launch = at(&home, &["link.md"], &home).unwrap();
        assert_eq!((launch.root, launch.open), (repo, vec![plain(real)]));
    }

    #[test]
    fn a_dangling_link_is_refused() {
        let (_s, home) = scratch_home("rl-dangling");
        let scratch = dir(home.join("scratch"));
        symlink(home.join("nowhere"), scratch.join("link.md")).unwrap();
        let err = at(&home, &["link.md"], &scratch).expect_err("a link to nothing has no canonical path");
        assert!(err.contains("link.md"), "got {err}");
    }

    #[test]
    fn a_new_file_uses_its_parent() {
        let (_s, home) = scratch_home("rl-new");
        let scratch = dir(home.join("scratch"));
        let launch = at(&home, &["new.rs"], &scratch).unwrap();
        assert_eq!(launch.open, [plain(scratch.join("new.rs"))]);
        assert_eq!(launch.root, scratch);
        assert!(!scratch.join("new.rs").exists(), "resolving creates nothing");
        // The parent is spelt through a link: the new path is canonical all the same.
        symlink(&scratch, home.join("via")).unwrap();
        assert_eq!(
            at(&home, &["via/new.rs"], &home).unwrap().open,
            [plain(scratch.join("new.rs"))]
        );
    }

    #[test]
    fn a_missing_parent_is_refused() {
        let (_s, home) = scratch_home("rl-noparent");
        let scratch = dir(home.join("scratch"));
        let err = at(&home, &["missing/dir/new.rs"], &scratch).expect_err("nowhere to write it");
        assert!(err.contains("no such directory"), "got {err}");
        // A parent that is a file is no directory either.
        file(scratch.join("a.rs"));
        assert!(at(&home, &["a.rs/new.rs"], &scratch).is_err());
    }

    #[test]
    fn a_trailing_slash_on_a_missing_name_is_refused() {
        let (_s, home) = scratch_home("rl-slash");
        let scratch = dir(home.join("scratch"));
        for name in ["foo/", "foo/.", "foo/..", "foo//"] {
            let err = at(&home, &[name], &scratch).expect_err(name);
            assert!(err.contains("no such directory"), "{name}: {err}");
        }
    }

    #[test]
    fn an_empty_name_is_refused_rather_than_read_as_the_cwd() {
        let (_s, home) = scratch_home("rl-empty");
        let scratch = dir(home.join("scratch"));
        assert!(at(&home, &[""], &scratch).is_err());
    }

    #[test]
    fn a_fifo_is_refused() {
        let (_s, home) = scratch_home("rl-fifo");
        let scratch = dir(home.join("scratch"));
        mkfifo(&scratch.join("pipe"));
        let err = at(&home, &["pipe"], &scratch).expect_err("nvim would block reading it");
        assert!(err.contains("is not a regular file"), "got {err}");
        file(scratch.join("a.rs"));
        let err = at(&home, &["a.rs", "pipe"], &scratch).expect_err("a later one too");
        assert!(err.contains("is not a regular file"), "got {err}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_non_utf8_file_is_refused() {
        use std::os::unix::ffi::OsStringExt;
        let (_s, home) = scratch_home("rl-bytes");
        let scratch = dir(home.join("scratch"));
        let name = OsString::from_vec(b"a\xff.rs".to_vec());
        std::fs::write(scratch.join(&name), b"text").unwrap();
        let args = [name];
        let launch = resolve_launch_in(&args, None, Ok(scratch.clone()), Some(&home), &me());
        assert!(launch.unwrap_err().contains("path is not valid UTF-8"));
        // A new file's name is an open path as well.
        let args = [OsString::from_vec(b"new\xff.rs".to_vec())];
        let launch = resolve_launch_in(&args, None, Ok(scratch), Some(&home), &me());
        assert!(launch.unwrap_err().contains("path is not valid UTF-8"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_non_utf8_directory_still_opens() {
        use std::os::unix::ffi::OsStringExt;
        let (_s, home) = scratch_home("rl-bytesdir");
        let project = dir(home.join(OsString::from_vec(b"proj\xff".to_vec())));
        let args = [project.clone().into_os_string()];
        let launch = resolve_launch_in(&args, None, Ok(home.clone()), Some(&home), &me()).unwrap();
        assert_eq!((launch.root, launch.open), (project, vec![]));
    }

    #[test]
    fn dir_then_files_keeps_dir_as_root_and_notes_one_outside() {
        let (_s, home) = scratch_home("rl-dirfiles");
        let project = dir(home.join("project"));
        let a = file(project.join("a.rs"));
        let b = file(home.join("other/b.rs"));
        let launch = at(&home, &[text(&project), text(&a), text(&b)], &home).unwrap();
        assert_eq!(launch.root, project);
        assert_eq!(launch.open, [plain(a), plain(b.clone())]);
        assert_eq!(launch.notes.len(), 1, "{:?}", launch.notes);
        assert!(launch.notes[0].contains(text(&b)), "{:?}", launch.notes);
        assert!(!launch.notes[0].starts_with("eitri:"), "{:?}", launch.notes);
        // A name after the directory is relative to the cwd, as for nvim, never to the directory.
        let launch = at(&home, &["project", "other/b.rs"], &home).unwrap();
        assert_eq!(launch.open, [plain(b)]);
    }

    #[test]
    fn dir_keeps_its_root_with_no_usable_test() {
        let (_s, home) = scratch_home("rl-dirhome");
        let a = file(home.join("a.rs"));
        let launch = at(&home, &[text(&home), text(&a)], &home).unwrap();
        assert_eq!(launch.root, home);
        assert_eq!(launch.open, [plain(a)]);
    }

    #[test]
    fn several_files_root_at_the_first_and_note_one_outside() {
        let (_s, home) = scratch_home("rl-several");
        let repo = dir(home.join("repo"));
        fake_repo(&repo);
        let a = file(repo.join("a.rs"));
        let b = file(repo.join("b.rs"));
        let c = file(home.join("c.rs"));
        let launch = at(&home, &[text(&a), text(&b), text(&c)], &home).unwrap();
        assert_eq!(launch.root, repo);
        assert_eq!(launch.open, [plain(a), plain(b), plain(c.clone())]);
        assert_eq!(launch.notes.len(), 1, "{:?}", launch.notes);
        assert!(launch.notes[0].contains(text(&c)), "{:?}", launch.notes);
    }

    #[test]
    fn dir_dir2_and_file_dir_are_refused() {
        let (_s, home) = scratch_home("rl-twodirs");
        let one = dir(home.join("one"));
        let two = dir(home.join("two"));
        let a = file(one.join("a.rs"));
        for args in [[text(&one), text(&two)], [text(&a), text(&one)]] {
            let err = at(&home, &args, &home).expect_err("one directory, and only first");
            assert!(err.contains("directory"), "got {err}");
        }
    }

    #[test]
    fn line_specs_only_before_the_first_path() {
        let (_s, home) = scratch_home("rl-spec");
        let a = file(home.join("a.rs"));
        assert_eq!(
            at(&home, &["+42", "a.rs"], &home).unwrap().open,
            [placed(a.clone(), Some(42), None)]
        );
        assert_eq!(
            at(&home, &["+42", "--", "a.rs"], &home).unwrap().open,
            [placed(a.clone(), Some(42), None)]
        );
        assert_eq!(
            at(&home, &["a.rs", "+42"], &home),
            Err("+42 looks like an nvim command. To open a file with that name use ./+42 or eitri -- +42".to_string())
        );
    }

    #[test]
    fn a_bare_plus_is_the_last_line() {
        let (_s, home) = scratch_home("rl-plus");
        let a = file(home.join("a.rs"));
        assert_eq!(
            at(&home, &["+", "a.rs"], &home).unwrap().open,
            [placed(a, Some(LAST_LINE), None)]
        );
    }

    #[test]
    fn plus_word_is_refused_with_the_way_out() {
        let (_s, home) = scratch_home("rl-word");
        file(home.join("+page.svelte"));
        file(home.join("a.rs"));
        assert_eq!(
            at(&home, &["+page.svelte"], &home),
            Err(
                "+page.svelte looks like an nvim command. To open a file with that name use \
                 ./+page.svelte or eitri -- +page.svelte"
                    .to_string()
            )
        );
        for word in ["+/pat", "+cmd", "+-1", "++1"] {
            assert_eq!(
                at(&home, &[word, "a.rs"], &home),
                Err(format!(
                    "{word} looks like an nvim command. To open a file with that name use ./{word} or \
                     eitri -- {word}"
                ))
            );
        }
        // The two ways out the message names both open it.
        let page = home.join("+page.svelte");
        assert_eq!(
            at(&home, &["./+page.svelte"], &home).unwrap().open,
            [plain(page.clone())]
        );
        assert_eq!(at(&home, &["--", "+page.svelte"], &home).unwrap().open, [plain(page)]);
    }

    #[test]
    fn plus_n_with_no_file_is_refused() {
        let (_s, home) = scratch_home("rl-nofile");
        let needs = Err("+42 needs a file".to_string());
        assert_eq!(at(&home, &["+42"], &home), needs);
        assert_eq!(at(&home, &["+42", text(&home)], &home), needs);
        assert_eq!(at(&home, &["+"], &home), Err("+ needs a file".to_string()));
        // The variable is no file either, whatever it names.
        let launch = resolve_launch_in(
            &os(&["+42"]),
            Some(home.clone().into_os_string()),
            Ok(home.clone()),
            Some(&home),
            &me(),
        );
        assert_eq!(launch, needs);
    }

    /// `main` prints a refusal or a note behind one `eitri: `, so the text itself carries none.
    #[test]
    fn a_refusal_prints_behind_one_prefix() {
        let (_s, home) = scratch_home("rl-printed");
        file(home.join("a.rs"));
        let printed = |message: String| format!("eitri: {message}");
        assert_eq!(
            printed(at(&home, &["+page.svelte"], &home).unwrap_err()),
            "eitri: +page.svelte looks like an nvim command. To open a file with that name use ./+page.svelte or \
             eitri -- +page.svelte"
        );
        assert_eq!(
            printed(at(&home, &["+42"], &home).unwrap_err()),
            "eitri: +42 needs a file"
        );
    }

    #[test]
    fn plus_n_too_big_is_refused() {
        let (_s, home) = scratch_home("rl-big");
        file(home.join("a.rs"));
        let token = "+4294967296";
        assert_eq!(
            at(&home, &[token, "a.rs"], &home),
            Err(format!(
                "{token} looks like an nvim command. To open a file with that name use ./{token} or \
                 eitri -- {token}"
            ))
        );
    }

    #[test]
    fn after_double_dash_everything_is_a_path() {
        let (_s, home) = scratch_home("rl-dashes");
        let scratch = dir(home.join("scratch"));
        let a = file(scratch.join("a.rs"));
        for name in ["+42", "-weird.txt", "--new-window", "--", "+", "a.rs:7"] {
            let launch = at(&home, &["--", name], &scratch).unwrap();
            assert_eq!(launch.open, [plain(scratch.join(name))], "{name}");
            assert!(!launch.new_window, "{name}");
        }
        // Several, in order, and none of them takes a line.
        let launch = at(&home, &["--", "+42", "a.rs:7", "a.rs"], &scratch).unwrap();
        assert_eq!(
            launch.open,
            [plain(scratch.join("+42")), plain(scratch.join("a.rs:7")), plain(a)]
        );
        assert!(launch.notes.is_empty(), "{:?}", launch.notes);
    }

    #[test]
    fn a_name_that_reads_as_something_else_stays_one_absolute_argument() {
        let (_s, home) = scratch_home("rl-odd");
        let scratch = dir(home.join("scratch"));
        for name in ["a b", "a\nb", "%", "#", "#1", "-x", "+x", "$HOME", "*", "a'b"] {
            let path = file(scratch.join(name));
            let launch = at(&home, &["--", name], &scratch).unwrap();
            assert_eq!(nvim_open_args(&launch.open), [text(&path)], "{name:?}");
        }
        let launch = at(&home, &["./-x"], &scratch).unwrap();
        assert_eq!(nvim_open_args(&launch.open), [text(&scratch.join("-x"))]);
    }

    #[test]
    fn file_colon_line_splits_only_when_the_short_name_exists() {
        let (_s, home) = scratch_home("rl-colon");
        let scratch = dir(home.join("scratch"));
        let a = file(scratch.join("a.rs"));
        assert_eq!(
            at(&home, &["a.rs:42"], &scratch).unwrap().open,
            [placed(a.clone(), Some(42), None)]
        );
        // No short name: a new file with the whole name.
        assert_eq!(
            at(&home, &["new.rs:42"], &scratch).unwrap().open,
            [plain(scratch.join("new.rs:42"))]
        );
        // The literal name wins over the short one.
        let literal = file(scratch.join("b.rs:42"));
        file(scratch.join("b.rs"));
        assert_eq!(at(&home, &["b.rs:42"], &scratch).unwrap().open, [plain(literal)]);
        // Not a number, or none that fits: part of the name.
        for name in ["a.rs:", "a.rs:x", "a.rs:4294967296", "a.rs:+4"] {
            assert_eq!(
                at(&home, &[name], &scratch).unwrap().open,
                [plain(scratch.join(name))],
                "{name}"
            );
        }
        // A directory takes no line: the name stays whole.
        dir(scratch.join("src"));
        assert_eq!(
            at(&home, &["src:42"], &scratch).unwrap().open,
            [plain(scratch.join("src:42"))]
        );
    }

    #[test]
    fn file_colon_line_colon_col() {
        let (_s, home) = scratch_home("rl-col");
        let scratch = dir(home.join("scratch"));
        let a = file(scratch.join("a.rs"));
        assert_eq!(
            at(&home, &["a.rs:42:7"], &scratch).unwrap().open,
            [placed(a, Some(42), Some(7))]
        );
        // The whole name first.
        let literal = file(scratch.join("b.rs:42:7"));
        file(scratch.join("b.rs"));
        assert_eq!(at(&home, &["b.rs:42:7"], &scratch).unwrap().open, [plain(literal)]);
        // Then the name with one suffix off, which reads as `file:LINE` on a file named `c.rs:42`.
        let named = file(scratch.join("c.rs:42"));
        file(scratch.join("c.rs"));
        assert_eq!(
            at(&home, &["c.rs:42:7"], &scratch).unwrap().open,
            [placed(named, Some(7), None)]
        );
    }

    #[test]
    fn plus_n_beats_a_file_suffix() {
        let (_s, home) = scratch_home("rl-beats");
        let a = file(home.join("a.rs"));
        assert_eq!(
            at(&home, &["+5", "a.rs:42"], &home).unwrap().open,
            [placed(a.clone(), Some(5), None)]
        );
        assert_eq!(
            at(&home, &["+5", "a.rs:42:7"], &home).unwrap().open,
            [placed(a, Some(5), None)]
        );
    }

    #[test]
    fn a_line_on_a_later_file_is_dropped_with_a_note() {
        let (_s, home) = scratch_home("rl-later");
        let a = file(home.join("a.rs"));
        let b = file(home.join("b.rs"));
        let launch = at(&home, &["a.rs", "b.rs:9:3"], &home).unwrap();
        assert_eq!(launch.open, [plain(a.clone()), plain(b.clone())]);
        assert_eq!(launch.notes.len(), 1, "{:?}", launch.notes);
        assert!(launch.notes[0].contains(text(&b)), "{:?}", launch.notes);
        assert!(!launch.notes[0].starts_with("eitri:"), "{:?}", launch.notes);
        // With a directory first, the first file is still the one with the line.
        let launch = at(&home, &[text(&home), "a.rs:3", "b.rs"], &home).unwrap();
        assert_eq!(launch.open, [placed(a, Some(3), None), plain(b)]);
        assert!(launch.notes.is_empty(), "{:?}", launch.notes);
    }

    #[test]
    fn options_after_files_are_checked() {
        let (_s, home) = scratch_home("rl-options");
        file(home.join("a.rs"));
        let err = at(&home, &["a.rs", "--bogus"], &home).expect_err("an unknown option after a file");
        assert!(err.contains("--bogus"), "got {err}");
        assert!(at(&home, &["a.rs", "--clean"], &home).is_ok());
    }

    #[test]
    fn new_window_anywhere_before_double_dash_is_a_flag() {
        let (_s, home) = scratch_home("rl-newwin");
        let a = file(home.join("a.rs"));
        for args in [["--new-window", "a.rs"], ["a.rs", "--new-window"]] {
            let launch = at(&home, &args, &home).unwrap();
            assert!(launch.new_window, "{args:?}");
            assert_eq!(launch.open, [plain(a.clone())], "{args:?}");
        }
        assert!(at(&home, &["--new-window"], &home).unwrap().new_window);
        assert!(!at(&home, &["a.rs", "--", "--new-window"], &home).unwrap().new_window);
        // The directory-only resolver steps over it the same way.
        assert_eq!(select(["--new-window", "/x"], None), Ok(argument("/x")));
    }

    #[test]
    fn raise_is_a_flag_before_dashes_and_a_path_after() {
        let (_s, home) = scratch_home("rl-raise");
        let project = dir(home.join("project"));
        let launch = at(&home, &["--raise", "--", text(&project)], &home).unwrap();
        assert!(launch.raise && launch.asks_for_a_raise());
        assert_eq!((launch.root, launch.open), (project.clone(), vec![]));
        assert!(!at(&home, &[text(&project)], &home).unwrap().raise);
        // After `--` it is a name like any other, and the launch a plain one.
        let named = dir(home.join("--raise"));
        let launch = at(&home, &["--", "--raise"], &home).unwrap();
        assert_eq!((launch.root, launch.raise), (named, false));
    }

    #[test]
    fn new_window_beats_raise() {
        let (_s, home) = scratch_home("rl-raise-new");
        let project = dir(home.join("project"));
        for flags in [["--new-window", "--raise"], ["--raise", "--new-window"]] {
            let args = [flags[0], flags[1], "--", text(&project)];
            let launch = at(&home, &args, &home).unwrap();
            assert!(launch.new_window, "{args:?}");
            assert!(!launch.asks_for_a_raise(), "{args:?}");
        }
    }

    #[test]
    fn raise_takes_exactly_one_path() {
        let (_s, home) = scratch_home("rl-raise-one");
        let one = dir(home.join("one"));
        let two = dir(home.join("two"));
        let a = file(one.join("a.rs"));
        let only_one = Err("--raise takes exactly one path after --".to_string());
        assert_eq!(at(&home, &["--raise"], &home), only_one);
        assert_eq!(at(&home, &["--raise", "--"], &home), only_one);
        assert_eq!(at(&home, &["--raise", "--", text(&one), text(&two)], &home), only_one);
        assert_eq!(at(&home, &["--raise", "--", text(&a), text(&a)], &home), only_one);
        // Before `--` a name could read as a line or a `name:LINE`, and only Eitri itself writes this command.
        assert_eq!(at(&home, &["--raise", text(&one)], &home), only_one);
        assert_eq!(at(&home, &["+5", "--raise", "--", text(&a)], &home), only_one);
        // The variable names no window to raise.
        let launch = resolve_launch_in(
            &os(&["--raise"]),
            Some(one.clone().into_os_string()),
            Ok(home.clone()),
            Some(&home),
            &me(),
        );
        assert_eq!(launch, only_one);
    }

    #[test]
    fn raise_on_a_missing_name_with_a_living_parent_is_refused_in_the_not_found_branch() {
        let (_s, home) = scratch_home("rl-raise-gone");
        let parent = dir(home.join("parent"));
        let gone = parent.join("gone");
        let refused = format!("{:?} is not there", gone.as_os_str());
        assert_eq!(at(&home, &["--raise", "--", text(&gone)], &home), Err(refused));
        assert!(!gone.exists(), "resolving creates nothing");
        // The one that would otherwise be a new buffer in a window rooted at the parent.
        assert_eq!(at(&home, &[text(&gone)], &home).unwrap().root, parent);
        // A name `new_file_path` has an answer of its own for never gets that far.
        let slashed = format!("{}/", text(&gone));
        let err = at(&home, &["--raise", "--", &slashed], &home).expect_err("still missing");
        assert!(err.contains("is not there"), "got {err}");
        // Taken whole, so a `name:LINE` that a plain launch would split is a missing name too.
        let a = file(parent.join("a.rs"));
        let with_line = format!("{}:5", text(&a));
        let err = at(&home, &["--raise", "--", &with_line], &home).expect_err("no such name");
        assert!(err.contains("is not there"), "got {err}");
        // A link to nothing has no canonical path, under this flag as under any.
        symlink(home.join("nowhere"), parent.join("link")).unwrap();
        let link = parent.join("link");
        assert!(at(&home, &["--raise", "--", text(&link)], &home).is_err());
    }

    #[test]
    fn raise_on_a_file_keeps_the_root_rule() {
        let (_s, home) = scratch_home("rl-raise-file");
        let repo = dir(home.join("repo"));
        fake_repo(&repo);
        let lib = file(repo.join("src/lib.rs"));
        let launch = at(&home, &["--raise", "--", text(&lib)], &home).unwrap();
        assert_eq!(launch.root, repo);
        assert_eq!(launch.open, [plain(lib)]);
        assert!(launch.raise);
        assert!(!launch.asks_for_a_raise(), "a file is handed over");
    }

    /// `--ssh` takes the next token as its host, whatever follows it; the project is the path after it, and only
    /// the window opens one: the directory-only resolvers refuse it by name.
    #[test]
    fn ssh_flag_takes_a_separate_host_value() {
        let remote = |host: &str, path: &str| RootSource::Remote {
            host: host.into(),
            path: path.into(),
        };
        assert_eq!(
            select(["--ssh", "example-host", "/srv/project"], None),
            Ok(remote("example-host", "/srv/project"))
        );
        assert_eq!(
            select(["--ssh", "example-host", "--", "/srv/project"], None),
            Ok(remote("example-host", "/srv/project"))
        );
        assert_eq!(
            select(["/srv/project", "--ssh", "example-host"], Some("/from/env")),
            Ok(remote("example-host", "/srv/project"))
        );
        assert_eq!(
            select(["--clean", "--ssh", "user@example-host", "/srv/project"], None),
            Ok(remote("user@example-host", "/srv/project"))
        );
        assert!(select(["--ssh"], None).unwrap_err().contains("needs a host"));
        assert!(select(["/srv/project", "--ssh"], None).is_err());
        assert!(select(["--ssh", "--clean", "/srv/project"], None).is_err());
        assert!(select(["--ssh", "example-host"], Some("/from/env")).is_err());
        assert!(select(["--ssh", "a", "--ssh", "b", "/p"], None).is_err());
        assert_eq!(select(["--", "--ssh"], None), Ok(argument("--ssh")));
        assert_eq!(select(["--", "--ssh", "example-host"], None), Ok(argument("--ssh")));
        let args: Vec<OsString> = ["--ssh", "example-host", "/srv/project"].map(OsString::from).to_vec();
        let refused = resolve_in(&args, None, Ok(PathBuf::from("/nowhere"))).unwrap_err();
        assert!(refused.contains("--ssh") && refused.contains("window"), "got {refused}");
        let (_s, home) = scratch_home("rl-ssh-args");
        let project = dir(home.join("project"));
        let launch = at(&home, &["--ssh", "example-host", text(&project)], &home).unwrap();
        assert_eq!(launch.ssh.as_deref(), Some("example-host"));
        assert_eq!(launch.root, project);
        assert!(launch.open.is_empty() && !launch.local);
        let launch = at(
            &home,
            &["--raise", "--ssh", "example-host", "--", text(&project)],
            &home,
        )
        .unwrap();
        assert!(launch.asks_for_a_raise());
        assert!(at(&home, &["--ssh"], &home).is_err());
        assert!(at(&home, &["--ssh", "example-host"], &home).is_err());
        assert!(at(&home, &["--ssh", "example-host", text(&project), text(&project)], &home).is_err());
        assert!(at(&home, &["--ssh", "example-host", "--local", text(&project)], &home).is_err());
        assert!(at(&home, &["--ssh", "example-host", "+3", text(&project)], &home).is_err());
        assert!(at(&home, &["--ssh", "example-host", "project"], &home).is_err());
        let trailing = format!("{}/", text(&project));
        assert!(at(&home, &["--ssh", "example-host", &trailing], &home).is_err());
        let dotted = format!("{}/.", text(&project));
        assert!(at(&home, &["--ssh", "example-host", &dotted], &home).is_err());
    }

    /// `--local` is a flag of the window's; the directory-only resolvers step over it.
    #[test]
    fn local_flag_is_known() {
        assert_eq!(select(["--local"], None), Ok(RootSource::Cwd));
        assert_eq!(select(["--local", "/x"], None), Ok(argument("/x")));
        assert_eq!(select(["--", "--local"], None), Ok(argument("--local")));
        let (_s, home) = scratch_home("rl-local");
        let project = dir(home.join("project"));
        let launch = at(&home, &["--local", text(&project)], &home).unwrap();
        assert!(launch.local);
        assert_eq!(launch.ssh, None);
        assert_eq!(launch.root, project);
        assert!(!at(&home, &[text(&project)], &home).unwrap().local);
        let launch = at(&home, &["--local"], &project).unwrap();
        assert!(launch.local && launch.root == project);
    }

    /// With no mirror yet, a remote project's path must already be the same directory here: one missing, a file,
    /// or one that resolves elsewhere is refused with the reason.
    #[test]
    fn missing_local_path_is_refused() {
        let (_s, home) = scratch_home("rl-ssh-missing");
        let missing = home.join("project");
        let err = at(&home, &["--ssh", "example-host", text(&missing)], &home).unwrap_err();
        assert!(
            err.contains(REMOTE_PATH_NEEDED) && err.contains(text(&missing)),
            "got {err}"
        );
        let plain = file(home.join("notes.txt"));
        let err = at(&home, &["--ssh", "example-host", text(&plain)], &home).unwrap_err();
        assert!(err.contains("not a directory"), "got {err}");
        let real = dir(home.join("real"));
        let link = home.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = at(&home, &["--ssh", "example-host", text(&link)], &home).unwrap_err();
        assert!(
            err.contains("same path on both machines") && err.contains(text(&real)),
            "got {err}"
        );
    }

    #[test]
    fn the_directory_only_resolvers_step_over_raise() {
        let real = std::env::temp_dir().canonicalize().expect("temp_dir exists");
        assert_eq!(select(["--raise", "/x"], None), Ok(argument("/x")));
        assert_eq!(select(["--raise"], None), Ok(RootSource::Cwd));
        assert_eq!(select(["--", "--raise"], None), Ok(argument("--raise")));
        let args = [OsString::from("--raise"), real.clone().into_os_string()];
        assert_eq!(resolve_in(&args, None, Ok(PathBuf::from("/nowhere"))), Ok(real));
    }

    #[test]
    fn env_naming_a_file_is_refused() {
        let (_s, home) = scratch_home("rl-envfile");
        let a = file(home.join("a.rs"));
        let launch = resolve_launch_in(&[], Some(a.into_os_string()), Ok(home.clone()), Some(&home), &me());
        let err = launch.expect_err("the variable stays directory-only");
        assert!(
            err.contains("not a directory") && err.contains(PROJECT_DIR_ENV),
            "got {err}"
        );
    }

    #[test]
    fn a_positional_file_beats_the_env() {
        let (_s, home) = scratch_home("rl-envpos");
        let scratch = dir(home.join("scratch"));
        let a = file(scratch.join("a.py"));
        let other = dir(home.join("other"));
        let launch = resolve_launch_in(
            &os(&[text(&a)]),
            Some(other.into_os_string()),
            Ok(scratch.clone()),
            Some(&home),
            &me(),
        )
        .unwrap();
        assert_eq!((launch.root, launch.open), (scratch, vec![plain(a)]));
    }

    #[test]
    fn the_empty_command_line_is_todays_root() {
        let (_s, home) = scratch_home("rl-empty-line");
        let scratch = dir(home.join("scratch"));
        let other = dir(home.join("other"));
        let bare = |args: &[&str], env: Option<&Path>| {
            let env = env.map(|path| path.as_os_str().to_owned());
            resolve_launch_in(&os(args), env, Ok(scratch.clone()), Some(&home), &me()).unwrap()
        };
        for args in [&[][..], &["--clean"][..], &["--"][..]] {
            let launch = bare(args, None);
            assert_eq!((&launch.root, launch.open.len()), (&scratch, 0), "{args:?}");
            assert_eq!(bare(args, Some(&other)).root, other, "{args:?}");
        }
        // A directory given is what `resolve_in` makes of it, even one that is no boundary.
        let launch = at(&home, &[text(&home)], &scratch).unwrap();
        assert_eq!((launch.root, launch.open), (home.clone(), vec![]));
        // No working directory is what `resolve_in` says about it, too.
        let launch = resolve_launch_in(
            &[],
            None,
            Err(io::Error::new(io::ErrorKind::NotFound, "gone")),
            Some(&home),
            &me(),
        );
        assert!(launch.unwrap_err().contains("working directory"));
    }

    #[test]
    fn every_refusal_the_gate_has_is_a_refusal_here() {
        let (_s, home) = scratch_home("rl-refusals");
        let own = dir(home.join("own"));
        let linked = home.join("linked");
        symlink(&own, &linked).unwrap();
        let shared = dir(home.join("shared"));
        chmod(&shared, 0o777);
        let someone_else = Identity {
            uid: me().uid.wrapping_add(1),
            ..me()
        };
        assert!(is_usable(&own, Some(&home), &me()));
        for reason in NotABoundary::ALL {
            let (path, who) = match reason {
                NotABoundary::Unresolvable => (home.join("missing"), me()),
                NotABoundary::FilesystemRoot => (PathBuf::from("/"), me()),
                NotABoundary::KernelTree => (PathBuf::from("/dev"), me()),
                NotABoundary::NoLongerItself => (linked.clone(), me()),
                NotABoundary::HomeOrAbove => (home.clone(), me()),
                NotABoundary::NotYours => (own.clone(), someone_else),
                NotABoundary::OthersCanWrite => (shared.clone(), me()),
            };
            assert_eq!(refusal(&path, Some(&home), &who), Some(reason), "{}", path.display());
            assert!(!is_usable(&path, Some(&home), &who), "{reason:?}");
        }
    }

    #[test]
    fn nvim_args_line_col_and_last_line() {
        let a = || PathBuf::from("/a");
        assert_eq!(nvim_open_args(&[placed(a(), Some(42), None)]), ["+42", "/a"]);
        assert_eq!(
            nvim_open_args(&[placed(a(), Some(42), Some(7))]),
            ["+call cursor(42,7)", "/a"]
        );
        assert_eq!(nvim_open_args(&[placed(a(), Some(LAST_LINE), None)]), ["+", "/a"]);
        assert_eq!(
            nvim_open_args(&[placed(a(), Some(3), None), plain(PathBuf::from("/b"))]),
            ["+3", "/a", "/b"]
        );
        assert_eq!(nvim_open_args(&[plain(a()), plain(PathBuf::from("/b"))]), ["/a", "/b"]);
        assert!(nvim_open_args(&[]).is_empty());
    }
}
