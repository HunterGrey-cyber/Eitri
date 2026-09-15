//! Where `shell` believes the project it is editing lives -- resolved exactly once, in `main()`,
//! and then carried as a value.
//!
//! Before this existed, `build_ui` read `std::env::current_dir()` separately for the agent panel
//! and for the terminal pane, and the editor pane read nothing at all (its `nvim --embed` child
//! simply inherited the process cwd). Three reads of one process-global is one read too many in a
//! process that may hold more than one window's worth of state: whatever a later `chdir` or a
//! second `activate` does, the three panes must not be able to disagree about which directory they
//! are looking at. Resolving once and passing the answer down makes that disagreement
//! unrepresentable rather than merely unlikely, and gives `shell <dir>` somewhere to land.
//!
//! Everything here works in `OsStr`/`OsString`, never `String`. `std::env::args()` is documented
//! to panic on an argument that is not valid UTF-8, and a filesystem path is exactly the argument
//! class most likely to be one -- a module whose whole contract is "a bad input gets a sentence,
//! not a panic" cannot be built on a type that cannot hold the bad input.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// The environment variable consulted when no directory was given on the command line. Exists so a
/// launcher (`.desktop` entry, sandbox harness) can point `shell` at a project without having to
/// control its working directory.
const PROJECT_DIR_ENV: &str = "NEOVIBE_PROJECT_DIR";

/// Every option `main()` itself matches on, listed here because this function is the only place
/// that can tell a flag from a path that happens to start with `-`.
///
/// **Adding an option to `main()` without adding it here makes `shell --that-option` a hard
/// startup failure** -- loudly, at the first launch, rather than by silently opening the wrong
/// directory, which is the trade this module exists to make. Neither flag takes a value; one that
/// did would need more than a membership test here (see [`select_root_source`]'s doc).
const KNOWN_FLAGS: [&str; 2] = ["--clean", "--terminal"];

/// The conventional end-of-flags separator. The first token after it is the project directory
/// verbatim, so a directory whose name really does begin with `-` is reachable.
const END_OF_FLAGS: &str = "--";

/// Which of the three candidates won, decided before any filesystem call happens. Separating the
/// choice from the `canonicalize` is what keeps the precedence rule unit-testable without a real
/// directory tree to point at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RootSource {
    /// The first positional (non-`-`-prefixed) command-line argument, or whatever followed `--`.
    Argument(OsString),
    /// `NEOVIBE_PROJECT_DIR`.
    Env(OsString),
    /// Neither was given -- fall back to this process's own working directory.
    Cwd,
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
        }
    }
}

/// Resolves the project root from this process's real arguments and environment.
///
/// `Err` carries a finished, human-readable sentence for `main()` to print before exiting -- this
/// replaced an `.expect("cwd")` whose panic output named neither the failing path nor what `shell`
/// had been trying to do with it.
///
/// This function is nothing but the three reads of process state; [`resolve_from`] is the part
/// with behaviour in it, and is what the tests drive.
pub(crate) fn resolve() -> Result<PathBuf, String> {
    // argv[0] is this binary, never a project directory.
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    resolve_from(
        args.iter().map(OsString::as_os_str),
        std::env::var_os(PROJECT_DIR_ENV),
        std::env::current_dir(),
    )
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
/// `args` must already have argv[0] stripped. A token that starts with `-` is a flag, and is
/// either one of [`KNOWN_FLAGS`] (skipped) or an error -- it is never silently ignored, because
/// "ignored, and the cwd was opened instead" is precisely the outcome this module exists to make
/// unreachable: `shell -myproj` and `shell --clen` would both have opened whatever directory the
/// shell happened to be sitting in, with nothing printed. `--` ends flag parsing, so a directory
/// whose name genuinely begins with `-` is still reachable as `shell -- -myproj`.
///
/// The one real limitation left: an option that takes a *separate* value (`--foo bar`) cannot be
/// handled by a membership test -- `bar` would be read here as the project directory. Such an
/// option has to be taught to this function, not just added to `KNOWN_FLAGS`.
pub(crate) fn select_root_source<'a>(
    args: impl IntoIterator<Item = &'a OsStr>,
    env: Option<&OsStr>,
) -> Result<RootSource, String> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == OsStr::new(END_OF_FLAGS) {
            // Whatever follows is a path, whatever it looks like. A bare trailing `--` (nothing
            // after it) is simply "no positional argument", same as no arguments at all.
            if let Some(path) = args.next() {
                return Ok(RootSource::Argument(path.to_os_string()));
            }
            break;
        }
        // Byte-wise, not `to_string_lossy().starts_with()`: a non-UTF-8 path must be classified
        // by what it actually starts with, not by what a replacement character makes of it.
        if arg.as_bytes().first() == Some(&b'-') {
            if KNOWN_FLAGS.iter().any(|flag| arg == OsStr::new(flag)) {
                continue;
            }
            return Err(format!(
                "unrecognized option {arg:?} -- shell accepts {}; if that was meant to be a \
                 project directory, pass it after `--` (shell -- {arg:?})",
                KNOWN_FLAGS.join(" and ")
            ));
        }
        return Ok(RootSource::Argument(arg.to_os_string()));
    }
    Ok(match env {
        // An empty value is treated as unset: `export NEOVIBE_PROJECT_DIR=` (or a launcher
        // interpolating an unset variable) is a far likelier explanation than a request to open a
        // project whose path is the empty string.
        Some(value) if !value.is_empty() => RootSource::Env(value.to_os_string()),
        _ => RootSource::Cwd,
    })
}

/// Turns a chosen source into a real, absolute, symlink-free directory, or into the message
/// `main()` should print.
///
/// `cwd` is passed in rather than read here so the "this process has no working directory" branch
/// (a deleted or unreadable cwd -- rare, but the exact case the old `.expect("cwd")` turned into a
/// bare panic) is reachable from a test.
fn canonicalize_source(source: &RootSource, cwd: io::Result<PathBuf>) -> Result<PathBuf, String> {
    let raw = match source {
        RootSource::Argument(path) | RootSource::Env(path) => PathBuf::from(path),
        RootSource::Cwd => cwd.map_err(|e| {
            format!(
                "cannot determine the current working directory ({e}) -- \
                 pass a project directory or set {PROJECT_DIR_ENV}"
            )
        })?,
    };
    // Canonicalized, not merely made absolute. This one path is handed to the nvim child, to the
    // agent panel and to the terminal session, and `agent`'s lease and conversation records key on
    // a "canonical cwd" of their own -- two spellings of one directory (a symlink, a `..`, a
    // relative argument) would read downstream as two different projects.
    let resolved = raw
        .canonicalize()
        .map_err(|e| format!("cannot open {} as a project directory: {e}", source.describe()))?;
    // `canonicalize` succeeds for a regular file, so `shell README.md` would otherwise reach nvim,
    // the agent and the terminal as a cwd and fail three different ways, none of them here.
    if !resolved.is_dir() {
        return Err(format!("{} is not a directory ({})", source.describe(), resolved.display()));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `select_root_source` over `&str` literals, which is what almost every case below wants.
    fn select<'a>(
        args: impl IntoIterator<Item = &'a str>,
        env: Option<&str>,
    ) -> Result<RootSource, String> {
        let args: Vec<OsString> = args.into_iter().map(OsString::from).collect();
        select_root_source(args.iter().map(OsString::as_os_str), env.map(OsStr::new))
    }

    fn argument(path: &str) -> RootSource {
        RootSource::Argument(OsString::from(path))
    }

    #[test]
    fn a_positional_argument_wins_over_everything() {
        assert_eq!(select(["/srv/project"], Some("/from/env")), Ok(argument("/srv/project")));
    }

    #[test]
    fn flags_are_not_positional_arguments() {
        // `shell --clean --terminal` must still resolve to the cwd, not to a project named
        // "--clean" -- both flags are real and are matched by `main()` itself.
        assert_eq!(select(["--clean", "--terminal"], None), Ok(RootSource::Cwd));
        // ...and a real path after them is still found.
        assert_eq!(select(["--clean", "/srv/project"], None), Ok(argument("/srv/project")));
    }

    #[test]
    fn an_unrecognized_option_is_an_error_rather_than_a_silent_fallback_to_the_cwd() {
        // The failure this replaces: `shell -myproj` (or a mistyped `--clen`) used to be dropped
        // on the floor and the cwd opened instead, with nothing printed anywhere.
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
        assert_eq!(select(["--clean", "--", "--terminal"], None), Ok(argument("--terminal")));
        // A bare trailing `--` is "no positional argument", not an error and not an empty path.
        assert_eq!(select(["--clean", "--"], Some("/from/env")), Ok(RootSource::Env(OsString::from("/from/env"))));
    }

    #[test]
    fn a_non_utf8_argument_is_a_path_like_any_other() {
        use std::os::unix::ffi::OsStringExt;
        // `std::env::args()` panics outright on this; `args_os()` is why the module is built on
        // `OsString`. The old code could not even represent this input, let alone report it.
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
        assert_eq!(select([], Some("/from/env")), Ok(RootSource::Env(OsString::from("/from/env"))));
    }

    #[test]
    fn an_empty_environment_value_counts_as_unset() {
        // `export NEOVIBE_PROJECT_DIR=` is a common shell accident; canonicalizing "" fails with a
        // bare ENOENT that reads as a missing directory rather than as an unset variable.
        assert_eq!(select([], Some("")), Ok(RootSource::Cwd));
    }

    #[test]
    fn nothing_at_all_falls_back_to_the_cwd() {
        assert_eq!(select([], None), Ok(RootSource::Cwd));
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
            &RootSource::Env(OsString::from("/definitely/not/a/real/neovibe/project")),
            Ok(PathBuf::from("/tmp")),
        )
        .expect_err("a missing directory must not silently fall back to the cwd");
        assert!(err.contains("/definitely/not/a/real/neovibe/project"), "got {err}");
        assert!(err.contains(PROJECT_DIR_ENV), "got {err}");
    }

    #[test]
    fn a_file_is_rejected_rather_than_used_as_a_project_root() {
        // `shell README.md` would otherwise hand a regular file to nvim's cwd, to the agent's
        // project_dir and to the terminal's cwd, each of which fails differently and later.
        let file = std::env::temp_dir().join(format!(
            "neovibe-project-root-test-{}-{}",
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

    /// The seam between the two halves above: `resolve()` itself is only the three reads of
    /// process state, and this is everything it wires together. An args/env swap, or a source
    /// chosen and then a *different* one canonicalized, would show up only here.
    #[test]
    fn the_selected_source_is_the_one_that_gets_canonicalized() {
        let real = std::env::temp_dir().canonicalize().expect("temp_dir exists");
        let missing = "/definitely/not/a/real/neovibe/project";

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
}
