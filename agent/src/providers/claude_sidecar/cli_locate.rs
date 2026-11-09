//! Which `claude` a Claude tab will run, decided without starting anything.
//!
//! A Claude tab does not run the CLI itself: the sidecar this process starts does, once, before it binds its
//! socket. It takes `VERDANDI_CLAUDE_CLI_PATH` when that is set to something other than blanks, else the name
//! `claude`, and hands that to `spawnSync`, which looks a bare name up on the `PATH` the sidecar inherited from
//! this process (`sidecar_command` clears neither the environment nor the working directory).
//!
//! A window started from a desktop launcher or a session manager has that manager's `PATH` (systemd's user
//! manager gives `/usr/local/bin:/usr/bin`), and the `~/.local/bin` Claude Code's own installer puts `claude` in is
//! only on the `PATH` of the person's login shell: the terminal runs `claude` and the window did not. So when the
//! variable is unset and the name is not on the `PATH`, [`locate_claude`] looks in the places Claude Code's
//! installers use ([`install_candidates`], in that fixed order) and takes the first that is a regular executable
//! file only its owner, this user or root, can change, reached through directories only their owners can change
//! (`installed_cli::inspect_installed`). The sidecar is then given that absolute path in the variable
//! ([`sidecar_cli_path`]), so what it runs is what was checked here. A variable the person set is never replaced,
//! even when it names nothing.
//!
//! [`claude_cli_missing_here`] gives the same answer to the first-start screen's "not found" line and a tab's "not
//! found" row; the test at the bottom runs Node's own `spawnSync` over a set of directory layouts, with what the
//! sidecar would be given, and compares.
//!
//! Only the lookup is mirrored. The sidecar then runs `claude --version`; a `claude` that is found but refuses that
//! call makes the tab fail while this says it is there, which is the side to be wrong on: a line that is silent is
//! better than one that is false, and nothing here runs the program.

use crate::installed_cli::{inspect_for_window, npm_prefix, read_user_file, runnable_file, Inspected, NPMRC_CAP};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Where Claude Code's own install instructions are. The first-start screen, the panel's "not found" row and the
/// installer's warning all give this one address.
pub const CLAUDE_INSTALL_URL: &str = "https://code.claude.com/docs/en/quickstart";

/// What the sidecar's failure text says when it could not run the CLI at all (a lookup that found nothing): the
/// panel's own "not found" row recognises the same words (`agent-ui/web/src/problems.ts`, which a test compares).
pub const CLI_MISSING_FAILURE_MARKER: &str = "could not determine the installed claude CLI version";

/// Where a window looks for `claude`, as the first-start screen and the panel's "not found" row say it, in the
/// order [`locate_claude`] looks (`problems.ts` holds the same words, which `problems.test.ts` compares).
pub const CLAUDE_LOOKED_IN: &str = "PATH, ~/.local/bin, ~/.claude/local, npm's global bin";

/// What the sidecar's sentence carries when the program could not be started at all (`spawnSync` gave an error, as
/// for a name not found), as against a program that started and exited (`exit status N`), which was found.
pub const CLI_SPAWN_ERROR_MARKER: &str = "(spawn error:";

/// What a sidecar start adds to its failure when nothing was handed over but an install place held a `claude` that was
/// refused; `problems.ts` reads the same words (which `problems.test.ts` compares) and says it was found but not used.
pub const CLAUDE_REFUSED_NOTE: &str =
    "A claude was found but not used: another user could change it, or what it starts is not on this window's PATH; \
     the window's log says which.";

/// Whether a tab's failure text is the sidecar saying it found no `claude` to run: its sentence, with a spawn error.
/// A `claude` that started and exited (`exit status 127` for a missing interpreter) was found, and is not this.
pub fn failure_names_missing_cli(failure: &str) -> bool {
    failure.find(CLI_MISSING_FAILURE_MARKER).is_some_and(|at| {
        failure[at..]
            .lines()
            .next()
            .is_some_and(|line| line.contains(CLI_SPAWN_ERROR_MARKER))
    })
}

/// The variable the sidecar reads to pick the CLI; the only way to name one that is not `claude` on the `PATH`.
pub(crate) const CLI_PATH_VAR: &str = "VERDANDI_CLAUDE_CLI_PATH";
/// What the sidecar runs when the variable is unset or blank.
const CLI_NAME: &str = "claude";
/// What a lookup searches when the environment has no `PATH` at all.
const DEFAULT_PATH: &str = "/usr/bin:/bin";

/// The variable, trimmed, when it is set to something other than blanks.
fn named_command(env: &dyn Fn(&str) -> Option<OsString>) -> Option<OsString> {
    env(CLI_PATH_VAR).and_then(|v| {
        let v = v.to_string_lossy();
        let trimmed = v.trim();
        (!trimmed.is_empty()).then(|| OsString::from(trimmed))
    })
}

/// Every file `command` can name, in the order a lookup tries them: itself when it holds a `/`, else one per
/// `PATH` entry, an empty entry being the working directory.
fn candidates(command: &OsStr, path: Option<&OsStr>) -> Vec<PathBuf> {
    if command.as_encoded_bytes().contains(&b'/') {
        return vec![PathBuf::from(command)];
    }
    let path = path.unwrap_or(OsStr::new(DEFAULT_PATH));
    std::env::split_paths(path)
        .map(|dir| {
            if dir.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                dir
            }
        })
        .map(|dir| dir.join(command))
        .collect()
}

/// The `claude` a window's sidecar will run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeCli {
    /// `VERDANDI_CLAUDE_CLI_PATH` names a `claude` that is there; the sidecar runs it as it is.
    Named,
    /// `claude` is on the `PATH` the sidecar inherits.
    OnPath,
    /// Not on the `PATH`, but where an installer puts it: the sidecar is given this path in the variable.
    Installed(PathBuf),
    /// Nothing to run. `refused` names each install place that held a `claude` this would not hand over, and why.
    Missing { refused: Vec<(PathBuf, String)> },
}

/// The places Claude Code's installers put `claude`, in the order they are looked at: the native installer's
/// launcher (`~/.local/bin/claude`, a link into `~/.local/share/claude/versions/`), the local install older
/// versions made (`~/.claude/local/claude`), and npm's global `bin` under the user's own prefix ([`npm_prefix`];
/// npm's default prefix is the system's, whose `bin` is on every `PATH`). Only absolute paths; a `HOME` that is
/// not absolute gives none. (A macOS app started from the Finder takes its `PATH`, Homebrew's included, from the
/// login shell before this runs: `eitri-mac`'s `user_env`.)
pub fn install_candidates(
    env: &dyn Fn(&str) -> Option<OsString>,
    read_file: &dyn Fn(&Path) -> Option<String>,
) -> Vec<PathBuf> {
    let home = env("HOME").map(PathBuf::from).filter(|h| h.is_absolute());
    let mut out = Vec::new();
    if let Some(home) = &home {
        out.push(home.join(".local/bin/claude"));
        out.push(home.join(".claude/local/claude"));
    }
    if let Some(prefix) = npm_prefix(env, home.as_deref(), read_file) {
        out.push(prefix.join("bin/claude"));
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

/// Decides which `claude` the sidecar will run, over `env`, `runnable` (a regular file this process may execute;
/// a relative candidate is for it to resolve against the working directory), `inspect` (judges one install place,
/// [`inspect_installed`]) and `read_file` (for `~/.npmrc`).
///
/// The variable, when set, is the whole answer: [`ClaudeCli::Named`] or nothing. Else `claude` on the `PATH`; else
/// the first [`install_candidates`] entry `inspect` finds usable.
pub fn locate_claude(
    env: &dyn Fn(&str) -> Option<OsString>,
    runnable: &dyn Fn(&Path) -> bool,
    inspect: &dyn Fn(&Path) -> Inspected,
    read_file: &dyn Fn(&Path) -> Option<String>,
) -> ClaudeCli {
    let named = named_command(env);
    let command = named.clone().unwrap_or_else(|| OsString::from(CLI_NAME));
    let path = env("PATH");
    if candidates(&command, path.as_deref()).iter().any(|c| runnable(c)) {
        return if named.is_some() {
            ClaudeCli::Named
        } else {
            ClaudeCli::OnPath
        };
    }
    let mut refused = Vec::new();
    if named.is_none() {
        for candidate in install_candidates(env, read_file) {
            match inspect(&candidate) {
                Inspected::Usable => return ClaudeCli::Installed(candidate),
                Inspected::Refused(why) => refused.push((candidate, why)),
                Inspected::Absent => {}
            }
        }
    }
    ClaudeCli::Missing { refused }
}

/// [`locate_claude`] over this process's environment and file system. Looks at the disk, so it belongs on a worker.
pub fn locate_claude_here() -> ClaudeCli {
    let path_var = std::env::var_os("PATH");
    locate_claude(
        &|name| std::env::var_os(name),
        &runnable_file,
        &|p| inspect_for_window(p, path_var.as_deref()),
        &|p| read_user_file(p, NPMRC_CAP),
    )
}

/// Why a Claude tab has no `claude` to run, as the first-start screen and the panel say it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeProblem {
    /// Nothing anywhere looked.
    Missing,
    /// Nothing usable, but an install place held one that was refused ([`ClaudeCli::Missing`]'s `refused`).
    Refused,
}

/// The problem `located` is, if any.
pub fn claude_problem(located: &ClaudeCli) -> Option<ClaudeProblem> {
    match located {
        ClaudeCli::Missing { refused } if refused.is_empty() => Some(ClaudeProblem::Missing),
        ClaudeCli::Missing { .. } => Some(ClaudeProblem::Refused),
        _ => None,
    }
}

/// [`claude_problem`] of [`locate_claude_here`], logging each refusal (`agent: not running ...`). Looks at the disk,
/// so it belongs on a worker.
pub fn claude_problem_here() -> Option<ClaudeProblem> {
    let located = locate_claude_here();
    if matches!(located, ClaudeCli::Missing { .. }) {
        for line in located_log_lines(&located) {
            eprintln!("agent: {line}");
        }
    }
    claude_problem(&located)
}

/// Whether no `claude` the sidecar this process would start could run is there ([`claude_problem_here`]). Looks at
/// the disk, so it belongs on a worker.
pub fn claude_cli_missing_here() -> bool {
    claude_problem_here().is_some()
}

/// What the sidecar is given in `VERDANDI_CLAUDE_CLI_PATH` for `located`: the install place found, and nothing when
/// the variable was set, `claude` is on the `PATH`, or nothing was found.
pub fn sidecar_cli_path(located: &ClaudeCli) -> Option<&Path> {
    match located {
        ClaudeCli::Installed(path) => Some(path),
        _ => None,
    }
}

/// The lines a sidecar start logs about `located`: the install place handed over, or each one refused. Paths and
/// reasons are escaped (`escape_debug`), so a name holding a newline or a terminal control cannot forge a line.
pub fn located_log_lines(located: &ClaudeCli) -> Vec<String> {
    match located {
        ClaudeCli::Installed(path) => vec![format!(
            "claude is not on this window's PATH; the sidecar runs {} ({CLI_PATH_VAR})",
            path.display().to_string().escape_debug()
        )],
        ClaudeCli::Missing { refused } => refused
            .iter()
            .map(|(path, why)| {
                format!(
                    "not running {}: {}",
                    path.display().to_string().escape_debug(),
                    why.escape_debug()
                )
            })
            .collect(),
        ClaudeCli::Named | ClaudeCli::OnPath => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let owned: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |name| owned.iter().find(|(k, _)| k == name).map(|(_, v)| OsString::from(v))
    }

    fn files(list: &[&str]) -> impl Fn(&Path) -> bool {
        let set: HashSet<PathBuf> = list.iter().map(PathBuf::from).collect();
        move |p| set.contains(p)
    }

    fn no_file(_: &Path) -> Option<String> {
        None
    }

    fn nothing_installed(_: &Path) -> Inspected {
        Inspected::Absent
    }

    /// Whether nothing is found, with no install place holding anything: the `PATH` and the variable alone.
    fn missing_on(env: &dyn Fn(&str) -> Option<OsString>, runnable: &dyn Fn(&Path) -> bool) -> bool {
        matches!(
            locate_claude(env, runnable, &nothing_installed, &no_file),
            ClaudeCli::Missing { .. }
        )
    }

    #[test]
    fn a_failure_names_a_missing_cli_by_the_sidecars_own_words() {
        assert!(failure_names_missing_cli(
            "could not determine the installed claude CLI version via \"/no/such/claude\" (spawn error: ENOENT)"
        ));
        assert!(!failure_names_missing_cli(
            "the connection to the provider ended before this session did"
        ));
        // Found and started, then exited: not a missing claude, whatever the status.
        assert!(!failure_names_missing_cli(
            "could not determine the installed claude CLI version via \"/home/u/.npm/bin/claude\" (exit status 127; \
             stderr: /usr/bin/env: 'node': No such file or directory)"
        ));
        // A spawn error on a later line does not make an exit status one.
        assert!(!failure_names_missing_cli(
            "could not determine the installed claude CLI version via \"claude\" (exit status 1)\nlater: (spawn error: x)"
        ));
    }

    #[test]
    fn a_refusal_is_its_own_problem_and_its_log_lines_are_escaped() {
        assert_eq!(
            claude_problem(&ClaudeCli::Missing { refused: Vec::new() }),
            Some(ClaudeProblem::Missing)
        );
        let refused = ClaudeCli::Missing {
            refused: vec![(
                PathBuf::from("/home/u/.local/bin/claude\nagent: forged"),
                "a\u{1b}[31mb".into(),
            )],
        };
        assert_eq!(claude_problem(&refused), Some(ClaudeProblem::Refused));
        for answer in [ClaudeCli::Named, ClaudeCli::OnPath, ClaudeCli::Installed("/x".into())] {
            assert_eq!(claude_problem(&answer), None);
        }
        let lines = located_log_lines(&refused);
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains('\n') && !lines[0].contains('\u{1b}'), "{lines:?}");
        assert!(lines[0].contains("\\n") && lines[0].contains("\\u{1b}"), "{lines:?}");
        let installed = located_log_lines(&ClaudeCli::Installed(PathBuf::from("/a\nb")));
        assert!(!installed[0].contains('\n'), "{installed:?}");
    }

    #[test]
    fn the_name_is_looked_up_on_path_in_order() {
        let is = files(&["/b/claude"]);
        assert!(!missing_on(&env_of(&[("PATH", "/a:/b")]), &is));
        assert!(missing_on(&env_of(&[("PATH", "/a:/c")]), &is));
        assert!(missing_on(&env_of(&[]), &files(&[])));
    }

    #[test]
    fn the_variable_replaces_the_name_and_blanks_do_not() {
        let is = files(&["/opt/cl/claude-x", "/usr/bin/claude", "/bin/mine"]);
        // Set: it alone is looked at, though `claude` is on the PATH.
        let env = env_of(&[("PATH", "/usr/bin"), (CLI_PATH_VAR, "/opt/missing/claude")]);
        assert!(missing_on(&env, &is));
        let env = env_of(&[("PATH", "/usr/bin"), (CLI_PATH_VAR, "  /opt/cl/claude-x\t")]);
        assert!(!missing_on(&env, &is));
        // A bare name is a PATH lookup.
        let env = env_of(&[("PATH", "/bin"), (CLI_PATH_VAR, "mine")]);
        assert!(!missing_on(&env, &is));
        // Blank is unset.
        let env = env_of(&[("PATH", "/usr/bin"), (CLI_PATH_VAR, "   ")]);
        assert!(!missing_on(&env, &is));
    }

    #[test]
    fn an_empty_path_entry_is_the_working_directory_and_no_path_is_the_default_one() {
        let is = files(&["./claude", "/usr/bin/claude"]);
        assert!(!missing_on(&env_of(&[("PATH", "/x::/y")]), &is));
        assert!(!missing_on(&env_of(&[]), &is));
        assert!(missing_on(&env_of(&[("PATH", "/x")]), &files(&["/usr/bin/claude"])));
    }

    /// Answers `inspect` from a fixed table: what is not in it is absent.
    fn installed(table: &[(&str, Inspected)]) -> impl Fn(&Path) -> Inspected {
        let table: HashMap<PathBuf, Inspected> = table.iter().map(|(p, i)| (PathBuf::from(p), i.clone())).collect();
        move |p| table.get(p).cloned().unwrap_or(Inspected::Absent)
    }

    #[test]
    fn the_install_places_are_the_native_launcher_the_old_local_install_then_npms_prefix() {
        let env = env_of(&[("HOME", "/home/u")]);
        let npmrc = |p: &Path| (p == Path::new("/home/u/.npmrc")).then(|| "prefix=~/.npm-global\n".to_string());
        let expected = vec![
            PathBuf::from("/home/u/.local/bin/claude"),
            PathBuf::from("/home/u/.claude/local/claude"),
            PathBuf::from("/home/u/.npm-global/bin/claude"),
        ];
        assert_eq!(install_candidates(&env, &npmrc), expected);
        // No npm prefix of the user's own: the two home places only.
        assert_eq!(install_candidates(&env, &no_file)[..2], expected[..2]);
        assert!(!install_candidates(&env, &no_file).contains(&expected[2]));
        // A prefix of `~/.local` names the native launcher's place again: it is looked at once.
        let local = |_: &Path| Some("prefix = \"${HOME}/.local\"".to_string());
        assert_eq!(install_candidates(&env, &local)[..2], expected[..2]);
        assert_eq!(
            install_candidates(&env, &local).len(),
            expected.len() - 1,
            "{:?}",
            install_candidates(&env, &local)
        );
    }

    #[test]
    fn a_home_that_is_not_absolute_gives_no_home_places() {
        for home in [None, Some(""), Some("relative/home"), Some("~")] {
            let pairs: Vec<(&str, &str)> = home.map(|h| ("HOME", h)).into_iter().collect();
            let env = env_of(&pairs);
            assert_eq!(
                install_candidates(&env, &|_| Some("prefix=~/.npm".to_string())),
                Vec::<PathBuf>::new(),
                "{home:?}"
            );
        }
    }

    #[test]
    fn not_on_the_path_the_first_usable_install_place_is_handed_to_the_sidecar() {
        let env = env_of(&[("HOME", "/home/u"), ("PATH", "/usr/local/bin:/usr/bin")]);
        let inspect = installed(&[
            ("/home/u/.local/bin/claude", Inspected::Refused("writable".into())),
            ("/home/u/.claude/local/claude", Inspected::Usable),
        ]);
        let located = locate_claude(&env, &files(&[]), &inspect, &no_file);
        assert_eq!(
            located,
            ClaudeCli::Installed(PathBuf::from("/home/u/.claude/local/claude"))
        );
        assert_eq!(
            sidecar_cli_path(&located),
            Some(Path::new("/home/u/.claude/local/claude"))
        );
        // The native launcher comes first when both are usable.
        let both = installed(&[
            ("/home/u/.local/bin/claude", Inspected::Usable),
            ("/home/u/.claude/local/claude", Inspected::Usable),
        ]);
        assert_eq!(
            locate_claude(&env, &files(&[]), &both, &no_file),
            ClaudeCli::Installed(PathBuf::from("/home/u/.local/bin/claude"))
        );
    }

    #[test]
    fn claude_on_the_path_wins_and_nothing_is_handed_over() {
        let env = env_of(&[("HOME", "/home/u"), ("PATH", "/usr/bin")]);
        let inspect = installed(&[("/home/u/.local/bin/claude", Inspected::Usable)]);
        let located = locate_claude(&env, &files(&["/usr/bin/claude"]), &inspect, &no_file);
        assert_eq!(located, ClaudeCli::OnPath);
        assert_eq!(sidecar_cli_path(&located), None);
    }

    #[test]
    fn found_on_the_path_or_named_the_npmrc_is_never_read() {
        let never = |p: &Path| -> Option<String> { panic!("read {p:?}") };
        let inspect = installed(&[]);
        let on_path = env_of(&[("HOME", "/home/u"), ("PATH", "/usr/bin")]);
        assert_eq!(
            locate_claude(&on_path, &files(&["/usr/bin/claude"]), &inspect, &never),
            ClaudeCli::OnPath
        );
        let named = env_of(&[("HOME", "/home/u"), (CLI_PATH_VAR, "/opt/gone/claude")]);
        assert_eq!(
            locate_claude(&named, &files(&[]), &inspect, &never),
            ClaudeCli::Missing { refused: Vec::new() }
        );
    }

    #[test]
    fn a_variable_the_person_set_wins_even_when_it_names_nothing() {
        let inspect = installed(&[("/home/u/.local/bin/claude", Inspected::Usable)]);
        let there = env_of(&[("HOME", "/home/u"), (CLI_PATH_VAR, "/opt/mine/claude")]);
        let located = locate_claude(&there, &files(&["/opt/mine/claude"]), &inspect, &no_file);
        assert_eq!(located, ClaudeCli::Named);
        assert_eq!(sidecar_cli_path(&located), None);
        let gone = env_of(&[("HOME", "/home/u"), (CLI_PATH_VAR, "/opt/gone/claude")]);
        let located = locate_claude(&gone, &files(&[]), &inspect, &no_file);
        assert_eq!(located, ClaudeCli::Missing { refused: Vec::new() });
        assert_eq!(sidecar_cli_path(&located), None);
    }

    #[test]
    fn nothing_usable_is_missing_and_says_what_was_refused() {
        let env = env_of(&[("HOME", "/home/u"), ("PATH", "/usr/bin")]);
        let inspect = installed(&[(
            "/home/u/.local/bin/claude",
            Inspected::Refused("/home/u/.local/bin can be written by other users".into()),
        )]);
        let located = locate_claude(&env, &files(&[]), &inspect, &no_file);
        assert_eq!(
            located,
            ClaudeCli::Missing {
                refused: vec![(
                    PathBuf::from("/home/u/.local/bin/claude"),
                    "/home/u/.local/bin can be written by other users".into()
                )]
            }
        );
        assert_eq!(
            located_log_lines(&located),
            vec!["not running /home/u/.local/bin/claude: /home/u/.local/bin can be written by other users"]
        );
        assert_eq!(sidecar_cli_path(&located), None);
    }

    /// The decision above and the sidecar's own lookup are two implementations of one rule, in two languages.
    /// This runs Node's `spawnSync` (what the sidecar's version probe calls) in a directory the test lays out,
    /// under the same environment [`locate_claude`] is given, with the command the sidecar would run (the found
    /// install place handed over in the variable), and compares: a spawn error, or the exit status 127 `env` gives
    /// for an interpreter it cannot find, is "missing".
    #[test]
    fn it_agrees_with_what_nodes_spawn_sync_finds() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let root = crate::installed_cli::tests::trusted_scratch_base()
            .join(format!("eitri-claude-locate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let make = |rel: &str, mode: Option<u32>| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            match mode {
                Some(mode) => {
                    std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
                    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
                }
                None => std::fs::create_dir_all(&p).unwrap(),
            }
        };
        make("good/claude", Some(0o755));
        make("plain/claude", Some(0o644));
        make("dir/claude", None);
        make("cwd/claude", Some(0o755));
        make("rel/bin/claude", Some(0o755));
        make("named/mine", Some(0o755));
        make("empty/none", Some(0o755));
        // Homes: the native installer's link into its versions directory; the old local install; npm's prefix
        // named in `.npmrc`; and the native layout with a world-writable `bin`, then with a group-writable file.
        make("native/.local/share/claude/versions/2.1.316", Some(0o755));
        std::fs::create_dir_all(root.join("native/.local/bin")).unwrap();
        std::os::unix::fs::symlink(
            "../share/claude/versions/2.1.316",
            root.join("native/.local/bin/claude"),
        )
        .unwrap();
        make("legacy/.claude/local/claude", Some(0o755));
        make("npm/.npm-global/bin/claude", Some(0o755));
        std::fs::write(root.join("npm/.npmrc"), "prefix=${HOME}/.npm-global\n").unwrap();
        make("openbin/.local/bin/claude", Some(0o755));
        std::fs::set_permissions(root.join("openbin/.local/bin"), std::fs::Permissions::from_mode(0o777)).unwrap();
        make("groupfile/.local/bin/claude", Some(0o775));
        make("noexec/.local/bin/claude", Some(0o644));
        // Scripts whose first line asks `env` for a program: one the PATH lacks, one a PATH directory holds.
        make("envmissing/.local/bin/claude", Some(0o755));
        std::fs::write(
            root.join("envmissing/.local/bin/claude"),
            "#!/usr/bin/env eitri-missing-interpreter\nexit 0\n",
        )
        .unwrap();
        make("envfound/.local/bin/claude", Some(0o755));
        std::fs::write(root.join("envfound/.local/bin/claude"), "#!/usr/bin/env sh\nexit 0\n").unwrap();
        std::fs::create_dir_all(root.join("interp")).unwrap();
        std::os::unix::fs::symlink("/bin/sh", root.join("interp/sh")).unwrap();
        let d = |rel: &str| root.join(rel).display().to_string();

        // (label, PATH or unset, VERDANDI_CLAUDE_CLI_PATH or unset, working directory under root, HOME or unset)
        let cases: Vec<(&str, Option<String>, Option<String>, &str, Option<String>)> = vec![
            (
                "found on the path",
                Some(format!("{}:/nonexistent", d("good"))),
                None,
                "empty",
                None,
            ),
            (
                "not on the path",
                Some(format!("{}:/nonexistent", d("empty"))),
                None,
                "empty",
                None,
            ),
            ("a file without the execute bit", Some(d("plain")), None, "empty", None),
            ("a directory named claude", Some(d("dir")), None, "empty", None),
            (
                "a directory, then the real one",
                Some(format!("{}:{}", d("dir"), d("good"))),
                None,
                "empty",
                None,
            ),
            (
                "a file without the bit, then the real one",
                Some(format!("{}:{}", d("plain"), d("good"))),
                None,
                "empty",
                None,
            ),
            (
                "an empty entry is the working directory",
                Some(":/nonexistent".into()),
                None,
                "cwd",
                None,
            ),
            (
                "an empty entry, nothing in the directory",
                Some(":/nonexistent".into()),
                None,
                "empty",
                None,
            ),
            ("a relative entry", Some("bin".into()), None, "rel", None),
            (
                "a relative entry, wrong directory",
                Some("bin".into()),
                None,
                "empty",
                None,
            ),
            ("an empty PATH", Some(String::new()), None, "cwd", None),
            ("no PATH", None, None, "empty", None),
            (
                "the variable, absolute and there",
                Some(d("empty")),
                Some(format!("{}/claude", d("good"))),
                "empty",
                None,
            ),
            (
                "the variable, absolute and gone, claude on the path",
                Some(d("good")),
                Some(d("empty") + "/claude"),
                "empty",
                None,
            ),
            (
                "the variable with blanks around it",
                Some(d("empty")),
                Some(format!("  {}/claude\t", d("good"))),
                "empty",
                None,
            ),
            (
                "the variable is blanks, claude on the path",
                Some(d("good")),
                Some("   ".into()),
                "empty",
                None,
            ),
            (
                "the variable is a bare name on the path",
                Some(d("named")),
                Some("mine".into()),
                "empty",
                None,
            ),
            (
                "the variable is a bare name, not on the path",
                Some(d("good")),
                Some("mine".into()),
                "empty",
                None,
            ),
            (
                "the variable is relative with a slash",
                Some(d("empty")),
                Some("./bin/claude".into()),
                "rel",
                None,
            ),
            (
                "the variable is relative, wrong directory",
                Some(d("empty")),
                Some("./bin/claude".into()),
                "empty",
                None,
            ),
            (
                "the variable names a directory",
                Some(d("good")),
                Some(d("dir") + "/claude"),
                "empty",
                None,
            ),
            (
                "a launcher's PATH, the native installer's link in the home",
                Some(d("empty")),
                None,
                "empty",
                Some(d("native")),
            ),
            (
                "a launcher's PATH, the old local install in the home",
                Some(d("empty")),
                None,
                "empty",
                Some(d("legacy")),
            ),
            (
                "a launcher's PATH, npm's prefix from the home's .npmrc",
                Some(d("empty")),
                None,
                "empty",
                Some(d("npm")),
            ),
            (
                "a launcher's PATH, a world-writable bin in the home",
                Some(d("empty")),
                None,
                "empty",
                Some(d("openbin")),
            ),
            (
                "a launcher's PATH, a group-writable claude in the home",
                Some(d("empty")),
                None,
                "empty",
                Some(d("groupfile")),
            ),
            (
                "a launcher's PATH, a claude without the execute bit in the home",
                Some(d("empty")),
                None,
                "empty",
                Some(d("noexec")),
            ),
            (
                "the variable names nothing, the native installer's link in the home",
                Some(d("empty")),
                Some(d("empty") + "/claude"),
                "empty",
                Some(d("native")),
            ),
            (
                "claude on the path and in the home",
                Some(d("good")),
                None,
                "empty",
                Some(d("native")),
            ),
            (
                "a launcher's PATH, a script in the home whose env interpreter is nowhere",
                Some(d("empty")),
                None,
                "empty",
                Some(d("envmissing")),
            ),
            (
                "a launcher's PATH holding the script's env interpreter",
                Some(format!("{}:{}", d("empty"), d("interp"))),
                None,
                "empty",
                Some(d("envfound")),
            ),
            (
                "a home that is not absolute",
                Some(d("empty")),
                None,
                "native",
                Some(".".into()),
            ),
        ];

        // The node binary, found on this process's own PATH before any case changes the child's.
        let node = std::env::var_os("PATH")
            .and_then(|p| {
                std::env::split_paths(&p)
                    .map(|d| d.join("node"))
                    .find(|n| runnable_file(n))
            })
            .expect(
                "these tests compare against Node's spawnSync: node must be on PATH (the shell build needs it too)",
            );
        const PROBE: &str = "const r = require('child_process').spawnSync(process.env.PROBE_CMD, ['--version']);\n\
                             process.stdout.write(r.error || r.status === 127 ? 'missing' : 'found');";

        let mut disagreements = Vec::new();
        let mut handed_over = 0;
        for (label, path, var, cwd, home) in &cases {
            let cwd = root.join(cwd);
            let env = |name: &str| -> Option<OsString> {
                match name {
                    "PATH" => path.as_ref().map(OsString::from),
                    CLI_PATH_VAR => var.as_ref().map(OsString::from),
                    "HOME" => home.as_ref().map(OsString::from),
                    _ => None,
                }
            };
            // `runnable` sees a relative candidate against `cwd`, as the child's working directory does.
            let located = locate_claude(
                &env,
                &|c: &Path| {
                    if c.is_relative() {
                        runnable_file(&cwd.join(c))
                    } else {
                        runnable_file(c)
                    }
                },
                &|p| inspect_for_window(p, path.as_deref().map(OsStr::new)),
                &|p| read_user_file(p, NPMRC_CAP),
            );
            let rust_missing = matches!(located, ClaudeCli::Missing { .. });

            let mut probe = Command::new(&node);
            probe.env_clear().current_dir(&cwd).args(["-e", PROBE]);
            if let Some(path) = path {
                probe.env("PATH", path);
            }
            // What the sidecar passes to `spawnSync`, worked out by Node's side of the rule: the variable trimmed,
            // else `claude`; the variable is what the window hands over when it found an install place.
            let handed = sidecar_cli_path(&located).map(|p| p.display().to_string());
            handed_over += usize::from(handed.is_some());
            let var = handed.or_else(|| var.clone());
            let cmd = var
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .unwrap_or("claude");
            probe.env("PROBE_CMD", cmd);
            let out = probe.output().expect("node starts");
            assert!(
                out.status.success(),
                "{label}: node failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let node_missing = String::from_utf8_lossy(&out.stdout) == "missing";
            if node_missing != rust_missing {
                disagreements.push(format!(
                    "{label}: node says missing={node_missing}, this says {located:?}"
                ));
            }
        }
        let _ = std::fs::remove_dir_all(&root);
        assert!(disagreements.is_empty(), "{}", disagreements.join("\n"));
        // The four usable install layouts were handed over, and nothing else was.
        assert_eq!(handed_over, 4);
    }
}
