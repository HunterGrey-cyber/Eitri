//! Which `nvim` binary the (forked) Neovide runtime spawns (v1-dist plan Task 6, spec §7):
//! `NEOVIBE_NVIM` above everything, then an inherited `NEOVIM_BIN` the fork already reads for
//! itself, then the user's own `PATH` nvim whenever it is new enough, then the newest neovibe-
//! private copy the installer (Task 11) or `neovibe setup` left under
//! `$XDG_DATA_HOME/neovibe/nvim`, and only then plain `nvim` again -- letting the fork report its
//! own version error rather than this module guessing at one.
//!
//! [`resolve`] is the one impure caller `shell`'s `main()` needs; [`decide`] is the pure decision
//! with every filesystem/exec/env read passed in, which is what every test in this file drives.
//!
//! **A private copy is never put on `PATH`, never linked into `~/.local/bin`, and this module never
//! creates, replaces or removes anything named `nvim`/`vim`/`vi`** (spec §7, owner note on I5) --
//! everything here only ever *reads*.
//!
//! **How the choice reaches the fork.** `main()` does not route this through
//! `NeovideEditorPaneOptions::child_env` the way every other per-child variable in this codebase is
//! (see `shell::pane_switch`'s own module doc for that general rule, and why it exists). It cannot:
//! the pinned fork's `CmdLineSettings` has `#[arg(long = "neovim-bin", env = "NEOVIM_BIN")]`
//! (`cmd_line.rs:229` at the pinned rev), and `LiveHarness::with_options` builds that settings value
//! -- via `CmdLineSettings::default()` or `parse_from(argv)`, `live_harness.rs` around 413-419 at the
//! same rev -- by having *clap* read `NEOVIM_BIN` out of **this process's own environment** at parse
//! time, before any child-env map reaches a `Command` at all. Extra argv cannot carry it either:
//! every extra nvim argument `shell` passes goes after `--`, which is nvim's own arguments, not the
//! fork's. So `main()` must call `unsafe { std::env::set_var(NEOVIM_BIN_ENV, ..) }` on itself, once,
//! before any other thread exists to race it. Unlike `TMUX`/`TMUX_PANE` (`pane_switch`'s own reason
//! for never doing this), leaking `NEOVIM_BIN` into every other child of `shell` -- nvim's own
//! `:terminal` jobs, the sidecar, `claude` and its Bash tool -- is harmless, because only Neovide
//! ever reads it (spec §7). The one child that must never see it is the bottom terminal's shell,
//! which removes it itself (`neovibe_terminal::pty::REMOVED_ENV`).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Above everything: an absolute path to the exact binary to run. A value that is not absolute, or
/// not a file, is a hard startup failure naming this variable (spec §7 step 1).
pub const NVIM_OVERRIDE_ENV: &str = "NEOVIBE_NVIM";

/// The pinned fork's own setting (`CmdLineSettings`'s `#[arg(long = "neovim-bin", env =
/// "NEOVIM_BIN")]`, `cmd_line.rs:229` at the pinned rev `910053d`) -- read here only so [`Choice`]
/// can say what it will use; never neovibe's to set when it is already present (spec §7 step 2).
/// `main()` writes this same variable on the two branches that need it -- see this module's own doc
/// above for why that write cannot go through `child_env` instead.
pub const NEOVIM_BIN_ENV: &str = "NEOVIM_BIN";

/// The fork's own floor (`NEOVIM_REQUIRED_VERSION`, fork `src/bridge/mod.rs:58`) -- LazyVim wants
/// more (0.11.2), but enforcing that is the fork's business, not this resolver's (spec §7).
pub const MINIMUM_VERSION: (u64, u64, u64) = (0, 10, 0);

/// `$XDG_DATA_HOME/neovibe/<this>`: where the installer's (Task 11) `tar --strip-components=1`
/// lands a version, and where [`resolve`] looks for one. The exact path a match is built from is
/// `<root>/<X.Y.Z>/bin/nvim` -- the layout contract this module shares with that task.
pub const PRIVATE_NVIM_SUBDIR: &str = "neovibe/nvim";

/// How long [`resolve`]'s own `<path> --version` probe waits before giving up. A `PATH` `nvim` can
/// be anything -- a wrapper script, a stuck mount -- and startup must never hang on it.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// What [`resolve`] settled on, and everything `main()` needs to act on it (set `NEOVIM_BIN`, or
/// not) and print the one line spec §7 asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// `NEOVIBE_NVIM` named this file explicitly.
    Explicit(PathBuf),
    /// `NEOVIM_BIN` was already set on this process before it decided anything -- the user's or a
    /// launcher's own choice, never neovibe's to overwrite (spec §7 step 2).
    Inherited(PathBuf),
    /// The user's own `PATH` nvim is new enough, or nothing usable was found anywhere -- either way
    /// the fork's own default lookup for plain `"nvim"` is exactly right, and it reports its own
    /// version error when there is one to report.
    Path,
    /// The newest neovibe-private copy: the `PATH` nvim was too old or absent, and at least one
    /// private copy exists.
    Private {
        /// `<private root>/<X.Y.Z>/bin/nvim`.
        path: PathBuf,
        /// That copy's own version, parsed from its directory name (never probed -- an official
        /// release tarball's own name already says what it is).
        version: (u64, u64, u64),
        /// What `PATH` had, for [`Choice::describe`]'s line (see [`PathNvim`]).
        on_path: PathNvim,
    },
}

/// What [`decide`] found on `PATH` when a private copy won, so the startup line says the true reason.
///
/// Three values, not an `Option` of a version: an `Option` read "none" both for a `PATH` with no
/// `nvim` and for one whose `--version` probe failed or timed out, so the line said "your PATH has
/// none" to a user whose `PATH` does have an nvim (v1-dist lane A's whole-branch review).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathNvim {
    /// Nothing named `nvim` on `PATH` (the probe never ran).
    Absent,
    /// An `nvim` whose `--version` did not answer with a parseable first line within the probe's
    /// timeout -- a wrapper script, a stuck mount, a crash.
    Unreadable,
    /// An `nvim` of this version, older than [`MINIMUM_VERSION`] (a new enough one wins outright).
    Version((u64, u64, u64)),
}

impl Choice {
    /// The path `main()` should write into [`NEOVIM_BIN_ENV`] on this process, or `None` on the two
    /// branches that must leave it alone: [`Choice::Inherited`] (spec §7 step 2's "never overwrites
    /// it") and [`Choice::Path`] (the fork's own default lookup already does the right thing).
    pub fn neovim_bin_to_set(&self) -> Option<&Path> {
        match self {
            Choice::Explicit(path) | Choice::Private { path, .. } => Some(path),
            Choice::Inherited(_) | Choice::Path => None,
        }
    }

    /// The line `main()` prints at startup (spec §7): what was chosen, and why.
    pub fn describe(&self) -> String {
        match self {
            Choice::Explicit(path) => format!("nvim: {} ({NVIM_OVERRIDE_ENV})", path.display()),
            Choice::Inherited(path) => {
                format!(
                    "nvim: {} ({NEOVIM_BIN_ENV}, inherited -- not neovibe's own choice)",
                    path.display()
                )
            }
            Choice::Path => "nvim: nvim (on PATH)".to_string(),
            Choice::Private { path, on_path, .. } => {
                let on_path = match on_path {
                    PathNvim::Absent => "none".to_string(),
                    PathNvim::Unreadable => "an nvim that did not report its version".to_string(),
                    PathNvim::Version((a, b, c)) => format!("{a}.{b}.{c}"),
                };
                format!("nvim: {} (neovibe's own copy; your PATH has {on_path})", path.display())
            }
        }
    }
}

/// The pure decision (spec §7 steps 1-5): every filesystem, exec and environment read passed in, so
/// this is driven entirely by this file's own tests with no real process, nvim or filesystem.
///
/// - `nvim_override`: [`NVIM_OVERRIDE_ENV`]'s raw value, if set.
/// - `inherited_neovim_bin`: [`NEOVIM_BIN_ENV`]'s raw value, if the caller's own environment already
///   had it set before anything here decided something -- never what this module itself would go on
///   to write, which only happens in `main()`, after `resolve` has already returned.
/// - `is_file`: validates steps 1 and 2's path. A closure, rather than calling `Path::is_file`
///   directly, so "not a file" can be asserted with no real filesystem.
/// - `path_nvim`: the result of a `PATH` walk for an executable literally named `nvim`, or `None`
///   when nothing on `PATH` matches.
/// - `private_candidates`: every directory entry [`resolve`] found directly under
///   `$XDG_DATA_HOME/neovibe/nvim`, each paired with the `bin/nvim` path inside it. **Unfiltered**
///   -- this function applies the `^[0-9]+\.[0-9]+\.[0-9]+$` pattern and the version sort, so a
///   `nvim-linux-x86_64/`-named sibling (the official tarball's own top directory, never searched)
///   is excluded here rather than by a caller having to agree about which names count.
/// - `version_of`: runs `<path> --version`, parses its first line, and is called **at most once** --
///   and only when `private_candidates` is non-empty *and* `path_nvim` is `Some`. A host with no
///   private copy at all never pays for this probe, because the answer is [`Choice::Path`] either
///   way (spec: "The version probe ... runs only when a neovibe-private nvim exists").
///
/// Returns `Err` only for steps 1 and 2's own validation failure, naming the variable: the value is
/// not an absolute path, or not a file. **Absolute, for both.** Spec §7 says so of `NEOVIBE_NVIM`
/// and asks `NEOVIM_BIN` to be "validated the same way"; the reason is the fork's spawn, which sets
/// the child's working directory to the project, so a relative value would be checked here against
/// `shell`'s own cwd and then run from a different directory -- two files, one name. (A bare command
/// name in an inherited `NEOVIM_BIN`, which the fork would look up on `PATH`, is refused by the same
/// rule; spec §7's own wording, flagged to the owner by lane A's whole-branch review.)
pub fn decide(
    nvim_override: Option<&OsStr>,
    inherited_neovim_bin: Option<&OsStr>,
    is_file: impl Fn(&Path) -> bool,
    path_nvim: Option<&Path>,
    private_candidates: &[(String, PathBuf)],
    version_of: impl FnOnce(&Path) -> Option<(u64, u64, u64)>,
) -> Result<Choice, String> {
    if let Some(raw) = nvim_override {
        let path = PathBuf::from(raw);
        return if !path.is_absolute() {
            Err(format!(
                "{NVIM_OVERRIDE_ENV}={} is not an absolute path (nvim runs from the project directory, \
                 so a relative one would name a different file)",
                path.display()
            ))
        } else if is_file(&path) {
            Ok(Choice::Explicit(path))
        } else {
            Err(format!("{NVIM_OVERRIDE_ENV}={} is not a file", path.display()))
        };
    }
    if let Some(raw) = inherited_neovim_bin {
        let path = PathBuf::from(raw);
        return if !path.is_absolute() {
            Err(format!(
                "{NEOVIM_BIN_ENV}={} (inherited from your own environment) is not an absolute path \
                 (nvim runs from the project directory, so a relative one would name a different file); \
                 set it to an absolute path or unset it",
                path.display()
            ))
        } else if is_file(&path) {
            Ok(Choice::Inherited(path))
        } else {
            Err(format!(
                "{NEOVIM_BIN_ENV}={} (inherited from your own environment) is not a file",
                path.display()
            ))
        };
    }

    let Some((version, private_path)) = newest_private_candidate(private_candidates) else {
        return Ok(Choice::Path);
    };

    let on_path = match path_nvim {
        None => PathNvim::Absent,
        Some(path) => match version_of(path) {
            Some(version) if version >= MINIMUM_VERSION => return Ok(Choice::Path),
            Some(version) => PathNvim::Version(version),
            None => PathNvim::Unreadable,
        },
    };

    Ok(Choice::Private {
        path: private_path,
        version,
        on_path,
    })
}

/// The newest entry matching `^[0-9]+\.[0-9]+\.[0-9]+$`, compared as `(major, minor, patch)` rather
/// than as text -- so `0.11.10` outranks `0.11.9` (spec §7's own worked example). `None` when the
/// listing is empty or nothing in it matches.
fn newest_private_candidate(candidates: &[(String, PathBuf)]) -> Option<((u64, u64, u64), PathBuf)> {
    candidates
        .iter()
        .filter_map(|(name, path)| parse_version_dirname(name).map(|v| (v, path.clone())))
        .max_by_key(|(v, _)| *v)
}

/// `name` is exactly `<digits>.<digits>.<digits>` -- no leading `v`, no suffix, no extra component.
fn parse_version_dirname(name: &str) -> Option<(u64, u64, u64)> {
    let mut parts = name.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    let patch = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    Some((parse_digits(major)?, parse_digits(minor)?, parse_digits(patch)?))
}

fn parse_digits(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Parses `nvim --version`'s first line: `NVIM v0.11.4`, or a dev build's
/// `NVIM v0.12.0-dev-1234+g<sha>`. Only the `PATH` nvim is ever probed with this -- the private
/// copies [`decide`] picks are official release tarballs already named by their own `<X.Y.Z>` (see
/// that function's own doc on when `version_of` even runs).
pub fn parse_nvim_version_line(first_line: &str) -> Option<(u64, u64, u64)> {
    let rest = first_line.trim().strip_prefix("NVIM v")?;
    let core = rest.split(|c: char| c != '.' && !c.is_ascii_digit()).next()?;
    parse_version_dirname(core)
}

/// `$XDG_DATA_HOME/neovibe/nvim`, or `<home>/.local/share/neovibe/nvim` when `XDG_DATA_HOME` is
/// unset, empty or not absolute -- the same three-case rule `core::layout::persist::state_subdir`
/// uses for `XDG_STATE_HOME`, and `agent::providers::claude_sidecar::user_sidecar_path` uses for its
/// own directory under this same `XDG_DATA_HOME` (that function's own doc: `core` depends on
/// `agent`, never the reverse, so the rule is reproduced here rather than shared). `None` when
/// neither gives a usable absolute directory.
pub fn private_nvim_root(xdg_data_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    match xdg_data_home {
        Some(dir) if Path::new(dir).is_absolute() => Some(PathBuf::from(dir).join(PRIVATE_NVIM_SUBDIR)),
        _ => match home {
            Some(home) if Path::new(home).is_absolute() => {
                Some(PathBuf::from(home).join(".local/share").join(PRIVATE_NVIM_SUBDIR))
            }
            _ => None,
        },
    }
}

/// Every directory directly under `root`, paired with the `bin/nvim` path inside it -- [`decide`]
/// applies the version filter and sort; this only lists, once, with no subprocess. A `root` that
/// does not exist yet (`neovibe setup`/the installer never ran, the ordinary case) is silently
/// empty, not an error -- absence is the normal state here, not a fault.
fn list_private_candidates(root: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let path = entry.path().join("bin").join("nvim");
            Some((name, path))
        })
        .collect()
}

/// A `which`-style `PATH` walk for an executable literally named `nvim`: the first `PATH` entry
/// holding a file at `<dir>/nvim` this process can execute. No `$PATHEXT` handling -- this crate
/// does not compile off unix (this crate's own top-level doc).
///
/// **A relative (or empty, PATH's own "means `.`" convention) entry is skipped, never joined**
/// (M8): this function's caller resolves it against *this* process's own cwd, but the pinned
/// fork's own `NEOVIM_BIN` spawn resolves a relative value against the *project* directory instead
/// (`current_dir(project)`, fork `src/bridge/command.rs:79-89`) -- so a relative match here could
/// name a different file from the one that actually runs.
fn which_nvim(path_var: Option<&OsStr>) -> Option<PathBuf> {
    let path_var = path_var?;
    std::env::split_paths(path_var).find_map(|dir| {
        if !dir.is_absolute() {
            return None;
        }
        let candidate = dir.join("nvim");
        is_executable_file(&candidate).then_some(candidate)
    })
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Above this many bytes of `<path> --version`'s stdout, [`version_of_binary`] stops reading (M7):
/// a real `nvim`'s first line -- the only line [`parse_nvim_version_line`] ever looks at -- is a
/// few dozen bytes, and its whole `--version` dump is a few hundred more.
const VERSION_PROBE_MAX_OUTPUT_BYTES: u64 = 4096;

/// Runs `<path> --version`, parses its first line, gives up after [`VERSION_PROBE_TIMEOUT`].
///
/// **Bounded two ways (M7), because `path` is a `PATH` entry this process does not control** (spec
/// §7's own reason `which_nvim` exists at all): stdout is piped and read through
/// [`VERSION_PROBE_MAX_OUTPUT_BYTES`]-capped [`std::io::Read::take`] rather than
/// `Command::output()`'s unbounded buffer, so a binary that never stops printing cannot grow this
/// process's memory without bound; and the child is always `kill()`ed before `wait()`, on every
/// path out of this function, not only the timeout one -- a capped read can finish (having read
/// its fill) well before such a binary would ever exit on its own, and an unkilled child still
/// writing into a pipe nobody is draining would otherwise block `wait()` forever. Signalling an
/// already-exited, unreaped child is a harmless no-op on Unix (there is nothing left to receive
/// it), so `wait()` still reports that real exit status rather than the kill's, and a well-behaved
/// `nvim --version` (which closes stdout and exits immediately after printing) is unaffected.
/// stdin is null and stderr is discarded; neither is read.
///
/// The reader thread is **not joined** on the timeout path: past the deadline it is abandoned
/// rather than made this function's problem, the same trade this codebase already makes for other
/// bounded subprocess waits -- killing the child there closes its end of the pipe, which is what
/// lets that thread's own blocked read return and the thread end on its own. This never touches
/// this process's own environment -- no `.env`/`.env_clear` call, so `Command::spawn` inherits
/// `environ` directly at `exec` time rather than reading it through `getenv` -- which is what makes
/// `main()`'s later `unsafe { std::env::set_var(NEOVIM_BIN_ENV, ..) }` (documented at that call
/// site) safe even if a probe from an earlier `resolve()` call is still outstanding: nothing here
/// ever calls `getenv`/`setenv` concurrently with it.
fn version_of_binary(path: &Path) -> Option<(u64, u64, u64)> {
    let mut child = std::process::Command::new(path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = stdout.take(VERSION_PROBE_MAX_OUTPUT_BYTES).read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    let stdout_bytes = match rx.recv_timeout(VERSION_PROBE_TIMEOUT) {
        Ok(bytes) => bytes,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };

    let _ = child.kill();
    let status = child.wait().ok()?;
    if !status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&stdout_bytes);
    parse_nvim_version_line(text.lines().next()?)
}

/// The one impure caller `shell`'s `main()` needs: reads [`NVIM_OVERRIDE_ENV`], [`NEOVIM_BIN_ENV`],
/// walks `PATH` for `nvim`, lists the private nvim root, and -- only when that listing is non-empty
/// -- probes the `PATH` nvim's version. See [`decide`] for the actual decision.
pub fn resolve() -> Result<Choice, String> {
    let nvim_override = std::env::var_os(NVIM_OVERRIDE_ENV);
    let inherited = std::env::var_os(NEOVIM_BIN_ENV);
    let path_nvim = which_nvim(std::env::var_os("PATH").as_deref());
    let home = std::env::var_os("HOME");
    let xdg_data_home = std::env::var_os("XDG_DATA_HOME");
    let private_candidates = private_nvim_root(xdg_data_home.as_deref(), home.as_deref())
        .map(|root| list_private_candidates(&root))
        .unwrap_or_default();
    decide(
        nvim_override.as_deref(),
        inherited.as_deref(),
        |p| p.is_file(),
        path_nvim.as_deref(),
        &private_candidates,
        version_of_binary,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn always_is_file(_: &Path) -> bool {
        true
    }

    fn never_is_file(_: &Path) -> bool {
        false
    }

    #[test]
    fn neovibe_nvim_wins_when_it_is_a_file() {
        let chosen = decide(
            Some(OsStr::new("/opt/my-nvim")),
            None,
            always_is_file,
            None,
            &[],
            |_| panic!("must not probe a version when NEOVIBE_NVIM already decided it"),
        )
        .unwrap();
        assert_eq!(chosen, Choice::Explicit(PathBuf::from("/opt/my-nvim")));
    }

    #[test]
    fn neovibe_nvim_naming_a_non_file_is_an_error_naming_the_variable() {
        let err = decide(
            Some(OsStr::new("/opt/does-not-exist")),
            None,
            never_is_file,
            None,
            &[],
            |_| panic!("not reached"),
        )
        .unwrap_err();
        assert!(err.contains("NEOVIBE_NVIM"), "{err}");
        assert!(err.contains("/opt/does-not-exist"), "{err}");
    }

    #[test]
    fn an_inherited_neovim_bin_wins_over_path_and_private_and_is_never_overwritten() {
        let chosen = decide(
            None,
            Some(OsStr::new("/opt/inherited-nvim")),
            always_is_file,
            Some(Path::new("/usr/bin/nvim")),
            &[("0.99.0".to_string(), PathBuf::from("/private/0.99.0/bin/nvim"))],
            |_| panic!("must not probe a version when NEOVIM_BIN is already inherited"),
        )
        .unwrap();
        assert_eq!(chosen, Choice::Inherited(PathBuf::from("/opt/inherited-nvim")));
        // The type `main()` actually acts on: `Inherited` must never be written back.
        assert_eq!(chosen.neovim_bin_to_set(), None);
    }

    #[test]
    fn an_inherited_neovim_bin_naming_a_non_file_is_an_error_naming_the_variable() {
        let err = decide(None, Some(OsStr::new("/opt/gone")), never_is_file, None, &[], |_| {
            panic!("not reached")
        })
        .unwrap_err();
        assert!(err.contains("NEOVIM_BIN"), "{err}");
        assert!(err.contains("/opt/gone"), "{err}");
    }

    #[test]
    fn a_new_enough_path_nvim_wins_even_over_a_private_copy() {
        let calls = Cell::new(0);
        let chosen = decide(
            None,
            None,
            always_is_file,
            Some(Path::new("/usr/bin/nvim")),
            &[("0.9.5".to_string(), PathBuf::from("/private/0.9.5/bin/nvim"))],
            |p| {
                calls.set(calls.get() + 1);
                assert_eq!(p, Path::new("/usr/bin/nvim"));
                Some((0, 11, 4))
            },
        )
        .unwrap();
        assert_eq!(chosen, Choice::Path);
        assert_eq!(calls.get(), 1, "the PATH nvim's version is probed exactly once");
        assert_eq!(chosen.neovim_bin_to_set(), None);
    }

    #[test]
    fn a_too_old_path_nvim_falls_back_to_the_newest_private_copy() {
        let chosen = decide(
            None,
            None,
            always_is_file,
            Some(Path::new("/usr/bin/nvim")),
            &[
                ("0.11.9".to_string(), PathBuf::from("/private/0.11.9/bin/nvim")),
                ("0.11.10".to_string(), PathBuf::from("/private/0.11.10/bin/nvim")),
                ("0.9.5".to_string(), PathBuf::from("/private/0.9.5/bin/nvim")),
            ],
            |_| Some((0, 9, 5)),
        )
        .unwrap();
        assert_eq!(
            chosen,
            Choice::Private {
                path: PathBuf::from("/private/0.11.10/bin/nvim"),
                version: (0, 11, 10),
                on_path: PathNvim::Version((0, 9, 5)),
            },
            "0.11.10 must outrank 0.11.9 numerically, not lexically"
        );
    }

    #[test]
    fn an_absent_path_nvim_falls_back_to_the_newest_private_copy_with_no_probe() {
        let chosen = decide(
            None,
            None,
            always_is_file,
            None,
            &[("0.11.4".to_string(), PathBuf::from("/private/0.11.4/bin/nvim"))],
            |_| panic!("nothing on PATH to probe"),
        )
        .unwrap();
        assert_eq!(
            chosen,
            Choice::Private {
                path: PathBuf::from("/private/0.11.4/bin/nvim"),
                version: (0, 11, 4),
                on_path: PathNvim::Absent
            }
        );
    }

    #[test]
    fn a_private_nvim_linux_x86_64_subdirectory_is_never_searched() {
        let chosen = decide(
            None,
            None,
            always_is_file,
            None,
            &[
                (
                    "nvim-linux-x86_64".to_string(),
                    PathBuf::from("/private/nvim-linux-x86_64/bin/nvim"),
                ),
                ("0.11.4".to_string(), PathBuf::from("/private/0.11.4/bin/nvim")),
            ],
            |_| panic!("not reached"),
        )
        .unwrap();
        assert_eq!(
            chosen,
            Choice::Private {
                path: PathBuf::from("/private/0.11.4/bin/nvim"),
                version: (0, 11, 4),
                on_path: PathNvim::Absent
            }
        );
    }

    #[test]
    fn nothing_anywhere_falls_back_to_plain_path() {
        let chosen = decide(None, None, always_is_file, None, &[], |_| {
            panic!("no private copy exists")
        })
        .unwrap();
        assert_eq!(chosen, Choice::Path);
        assert_eq!(chosen.neovim_bin_to_set(), None);
    }

    #[test]
    fn the_version_probe_is_never_called_with_no_private_copy_even_when_path_has_an_old_nvim() {
        let chosen = decide(
            None,
            None,
            always_is_file,
            Some(Path::new("/usr/bin/nvim")),
            &[],
            |_| panic!("no private copy exists; the PATH version must not be probed"),
        )
        .unwrap();
        assert_eq!(chosen, Choice::Path);
    }

    #[test]
    fn version_lines_parse() {
        assert_eq!(parse_nvim_version_line("NVIM v0.11.4"), Some((0, 11, 4)));
        assert_eq!(
            parse_nvim_version_line("NVIM v0.12.0-dev-1234+g0123456"),
            Some((0, 12, 0))
        );
    }

    #[test]
    fn private_directory_names_sort_numerically_not_lexically() {
        let candidates = vec![
            ("0.9.9".to_string(), PathBuf::from("a")),
            ("0.11.9".to_string(), PathBuf::from("b")),
            ("0.11.10".to_string(), PathBuf::from("c")),
            ("0.2.0".to_string(), PathBuf::from("d")),
        ];
        let (version, path) = newest_private_candidate(&candidates).unwrap();
        assert_eq!(version, (0, 11, 10));
        assert_eq!(path, PathBuf::from("c"));
    }

    #[test]
    fn private_dirname_pattern_rejects_anything_not_exactly_three_numeric_components() {
        for bad in ["v0.11.4", "0.11", "0.11.4.1", "0.11.4-dev", "", "nvim-linux-x86_64"] {
            assert_eq!(parse_version_dirname(bad), None, "{bad:?} must not parse");
        }
        assert_eq!(parse_version_dirname("0.11.4"), Some((0, 11, 4)));
    }

    #[test]
    fn describe_matches_spec_7_wording_for_a_private_win() {
        let chosen = Choice::Private {
            path: PathBuf::from("/home/x/.local/share/neovibe/nvim/0.11.4/bin/nvim"),
            version: (0, 11, 4),
            on_path: PathNvim::Version((0, 9, 5)),
        };
        assert_eq!(
            chosen.describe(),
            "nvim: /home/x/.local/share/neovibe/nvim/0.11.4/bin/nvim (neovibe's own copy; your PATH has 0.9.5)"
        );
        let chosen_no_path = Choice::Private {
            path: PathBuf::from("/x/bin/nvim"),
            version: (0, 11, 4),
            on_path: PathNvim::Absent,
        };
        assert!(chosen_no_path.describe().ends_with("your PATH has none)"));
        let chosen_unreadable = Choice::Private {
            path: PathBuf::from("/x/bin/nvim"),
            version: (0, 11, 4),
            on_path: PathNvim::Unreadable,
        };
        assert!(
            chosen_unreadable
                .describe()
                .ends_with("your PATH has an nvim that did not report its version)"),
            "{}",
            chosen_unreadable.describe()
        );
    }

    /// A `PATH` nvim whose probe fails or times out is not "none": the private copy still wins (it
    /// cannot be shown to be new enough), but the line must not tell the user their `PATH` has no
    /// nvim when it does (v1-dist lane A's whole-branch review).
    #[test]
    fn a_path_nvim_whose_probe_fails_is_unreadable_not_absent() {
        let chosen = decide(
            None,
            None,
            always_is_file,
            Some(Path::new("/usr/bin/nvim")),
            &[("0.11.4".to_string(), PathBuf::from("/private/0.11.4/bin/nvim"))],
            |_| None,
        )
        .unwrap();
        assert_eq!(
            chosen,
            Choice::Private {
                path: PathBuf::from("/private/0.11.4/bin/nvim"),
                version: (0, 11, 4),
                on_path: PathNvim::Unreadable,
            }
        );
    }

    /// Spec §7: `NEOVIBE_NVIM` is an absolute path, and an inherited `NEOVIM_BIN` is validated the
    /// same way. A relative value would be checked against `shell`'s cwd and then run by the fork
    /// from the project directory, so it is refused before `is_file` is even asked -- here with an
    /// `is_file` that says yes to everything, which is what a same-named file in `shell`'s cwd does.
    #[test]
    fn a_relative_neovibe_nvim_or_neovim_bin_is_an_error_naming_the_variable() {
        for relative in ["bin/nvim", "./nvim", "nvim"] {
            let err = decide(Some(OsStr::new(relative)), None, always_is_file, None, &[], |_| {
                panic!("not reached")
            })
            .unwrap_err();
            assert!(err.contains("NEOVIBE_NVIM"), "{err}");
            assert!(err.contains("not an absolute path"), "{err}");

            let err = decide(None, Some(OsStr::new(relative)), always_is_file, None, &[], |_| {
                panic!("not reached")
            })
            .unwrap_err();
            assert!(err.contains("NEOVIM_BIN"), "{err}");
            assert!(err.contains("not an absolute path"), "{err}");
        }
    }

    #[test]
    fn private_nvim_root_follows_the_same_three_case_rule_as_the_sidecar_directory() {
        assert_eq!(
            private_nvim_root(Some(OsStr::new("/data")), Some(OsStr::new("/home/x"))),
            Some(PathBuf::from("/data/neovibe/nvim"))
        );
        assert_eq!(
            private_nvim_root(None, Some(OsStr::new("/home/x"))),
            Some(PathBuf::from("/home/x/.local/share/neovibe/nvim"))
        );
        assert_eq!(private_nvim_root(None, None), None);
        // A relative `XDG_DATA_HOME` is ignored, falling back to `$HOME` -- the XDG spec's own rule.
        assert_eq!(
            private_nvim_root(Some(OsStr::new("relative")), Some(OsStr::new("/home/x"))),
            Some(PathBuf::from("/home/x/.local/share/neovibe/nvim"))
        );
    }

    #[test]
    fn list_private_candidates_is_empty_for_a_root_that_does_not_exist() {
        assert_eq!(list_private_candidates(Path::new("/does/not/exist/at/all")), Vec::new());
    }

    #[test]
    fn list_private_candidates_reads_real_directories_and_builds_the_bin_nvim_path() {
        let scratch = std::env::temp_dir().join(format!("nvim-bin-test-list-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(scratch.join("0.11.4").join("bin")).unwrap();
        std::fs::create_dir_all(scratch.join("nvim-linux-x86_64").join("bin")).unwrap();
        std::fs::write(scratch.join("a-stray-file"), b"").unwrap();

        let mut found = list_private_candidates(&scratch);
        found.sort();
        assert_eq!(
            found,
            vec![
                ("0.11.4".to_string(), scratch.join("0.11.4").join("bin").join("nvim")),
                (
                    "nvim-linux-x86_64".to_string(),
                    scratch.join("nvim-linux-x86_64").join("bin").join("nvim")
                ),
            ]
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn which_nvim_finds_the_first_path_entry_with_an_executable_nvim() {
        let scratch = std::env::temp_dir().join(format!("nvim-bin-test-which-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&scratch);
        let empty_dir = scratch.join("empty");
        let real_dir = scratch.join("real");
        std::fs::create_dir_all(&empty_dir).unwrap();
        std::fs::create_dir_all(&real_dir).unwrap();
        let nvim_path = real_dir.join("nvim");
        std::fs::write(&nvim_path, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&nvim_path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        // A same-named entry that is not executable must not count.
        let non_exec_dir = scratch.join("non-exec");
        std::fs::create_dir_all(&non_exec_dir).unwrap();
        std::fs::write(non_exec_dir.join("nvim"), b"").unwrap();

        let path_var = std::env::join_paths([&non_exec_dir, &empty_dir, &real_dir]).unwrap();
        assert_eq!(which_nvim(Some(&path_var)), Some(nvim_path));
        assert_eq!(which_nvim(None), None);

        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// Removes its directory on drop, including on an `assert!` panic mid-test (unwinding still
    /// runs destructors) -- used below only for the one scratch directory this file's tests ever
    /// create *inside the real checkout* rather than under `std::env::temp_dir()`, so a failing
    /// assertion can never leave it behind for `git status` to find.
    struct RemoveDirOnDrop(PathBuf);
    impl Drop for RemoveDirOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn which_nvim_skips_a_relative_or_empty_path_entry() {
        // M8: the fork resolves `NEOVIM_BIN` with `current_dir(project)` (this module's top-level
        // doc, and fork `src/bridge/command.rs:79-89`), a directory that has nothing to do with
        // wherever this test process's own cwd happens to be -- so a relative (or PATH's-own-
        // empty-means-"." convention) entry that resolves to a real executable here, against this
        // process's cwd, must never be joined and returned: it would name a different file from
        // the one the fork would actually run.
        let cwd = std::env::current_dir().unwrap();
        let relative_name = format!("nvim-bin-test-cwd-relative-{}-{}", std::process::id(), line!());
        let relative_dir = PathBuf::from(&relative_name);
        let absolute_dir = cwd.join(&relative_name);
        let _ = std::fs::remove_dir_all(&absolute_dir);
        std::fs::create_dir_all(&absolute_dir).unwrap();
        let _cleanup = RemoveDirOnDrop(absolute_dir.clone());
        let relative_nvim = absolute_dir.join("nvim");
        std::fs::write(&relative_nvim, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&relative_nvim, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

        // Also a real absolute entry after the bad ones, to prove skipping continues the walk
        // rather than aborting it.
        let scratch = std::env::temp_dir().join(format!("nvim-bin-test-which-abs-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).unwrap();
        let absolute_nvim = scratch.join("nvim");
        std::fs::write(&absolute_nvim, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&absolute_nvim, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

        let path_var = std::env::join_paths([OsStr::new(""), relative_dir.as_os_str(), scratch.as_os_str()]).unwrap();
        assert_eq!(which_nvim(Some(&path_var)), Some(absolute_nvim));

        // With nothing absolute at all, the whole walk must find nothing rather than fall back to
        // a relative match.
        let relative_only = std::env::join_paths([OsStr::new(""), relative_dir.as_os_str()]).unwrap();
        assert_eq!(which_nvim(Some(&relative_only)), None);

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn resolve_falls_back_to_plain_path_when_the_private_root_does_not_exist() {
        // A real end-to-end smoke test of `resolve()` itself. Deliberately does NOT touch `PATH`:
        // `pane_switch`'s own tests read the real `PATH` concurrently in this same test binary
        // (`cargo test`'s default runner), and this assertion does not need it touched anyway --
        // with no private nvim root at all, `decide` returns `Choice::Path` unconditionally,
        // before it would ever look at what is on `PATH` (see `decide`'s own doc on when the
        // version probe runs). `XDG_DATA_HOME` is pointed at a directory that cannot exist so this
        // developer's own real one (if any) is never consulted.
        //
        // SAFETY: `cargo test`'s default multi-threaded runner makes any process-wide mutation a
        // little uncomfortable; every value read and restored here is captured up front rather than
        // assumed, and nothing else in this crate's test suite reads or writes these three names.
        let saved_xdg = std::env::var_os("XDG_DATA_HOME");
        let saved_nvim = std::env::var_os(NVIM_OVERRIDE_ENV);
        let saved_neovim_bin = std::env::var_os(NEOVIM_BIN_ENV);
        unsafe {
            std::env::remove_var(NVIM_OVERRIDE_ENV);
            std::env::remove_var(NEOVIM_BIN_ENV);
            std::env::set_var("XDG_DATA_HOME", "/does/not/exist/at/all/xdg-data");
        }

        let result = resolve();

        unsafe {
            match saved_xdg {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
            match saved_nvim {
                Some(v) => std::env::set_var(NVIM_OVERRIDE_ENV, v),
                None => std::env::remove_var(NVIM_OVERRIDE_ENV),
            }
            match saved_neovim_bin {
                Some(v) => std::env::set_var(NEOVIM_BIN_ENV, v),
                None => std::env::remove_var(NEOVIM_BIN_ENV),
            }
        }

        assert_eq!(result, Ok(Choice::Path));
    }

    /// Writes and chmods `path` from inside a short-lived `sh` child, not this process (matching
    /// `agent::process::tests::fake_binary_script`'s own fix for the identical flake, `e5376654`):
    /// a write fd this process held open here would be copied, still open, into any child a
    /// concurrent test thread forks in the meantime, and running `path` fails with `ETXTBSY` until
    /// that unrelated child execs and its copy of the fd finally closes -- reproduced here as one
    /// `version_of_binary_*` test failing intermittently under `--test-threads=4` (reproduced with
    /// `spawn err: Os { code: 26, kind: ExecutableFileBusy, .. }`).
    fn write_executable_script(path: &Path, script: &str) {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(r#"printf '%s' "$1" > "$2" && chmod 755 "$2""#)
            .arg("sh")
            .arg(script)
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success(), "writing the test script failed: {status}");
    }

    #[test]
    fn version_of_binary_parses_a_normal_versions_output() {
        let scratch = std::env::temp_dir().join(format!(
            "nvim-bin-test-version-normal-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).unwrap();
        let script = scratch.join("nvim");
        write_executable_script(
            &script,
            "#!/bin/sh\necho 'NVIM v0.11.4'\necho 'more build info'\nexit 0\n",
        );

        assert_eq!(version_of_binary(&script), Some((0, 11, 4)));

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn version_of_binary_caps_output_and_kills_a_binary_that_never_stops_printing() {
        // M7: before the fix, this called `Command::output()` with no cap, so a PATH `nvim` that
        // never stops printing grew that buffer without bound. The fix's capped reader must finish
        // (and the child must be reaped) well inside `VERSION_PROBE_TIMEOUT`, not by hitting it --
        // proving the fix actually bounds this case rather than merely bounding the separate
        // "prints nothing at all" hang below.
        let scratch = std::env::temp_dir().join(format!(
            "nvim-bin-test-version-infinite-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).unwrap();
        let script = scratch.join("nvim");
        write_executable_script(&script, "#!/bin/sh\nwhile true; do printf '%01024d' 0; done\n");

        let start = std::time::Instant::now();
        let result = version_of_binary(&script);
        let elapsed = start.elapsed();

        assert_eq!(result, None);
        assert!(
            elapsed < VERSION_PROBE_TIMEOUT,
            "an unbounded printer took {elapsed:?}, at or past the {VERSION_PROBE_TIMEOUT:?} timeout \
             meant for the separate no-output case"
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn version_of_binary_gives_up_and_reaps_a_binary_that_hangs_with_no_output() {
        let scratch =
            std::env::temp_dir().join(format!("nvim-bin-test-version-hang-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).unwrap();
        let script = scratch.join("nvim");
        write_executable_script(&script, "#!/bin/sh\nwhile true; do sleep 1; done\n");

        let start = std::time::Instant::now();
        let result = version_of_binary(&script);
        let elapsed = start.elapsed();

        assert_eq!(result, None);
        assert!(elapsed >= VERSION_PROBE_TIMEOUT, "returned after only {elapsed:?}");
        assert!(
            elapsed < VERSION_PROBE_TIMEOUT + Duration::from_secs(2),
            "took {elapsed:?} past the timeout -- the child was not reaped promptly"
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }
}
