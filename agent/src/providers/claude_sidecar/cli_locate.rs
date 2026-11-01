//! Whether a Claude tab can find the `claude` it runs, asked without starting anything.
//!
//! A Claude tab does not locate the CLI itself: the sidecar this process starts does, once, before it binds
//! its socket. It takes `VERDANDI_CLAUDE_CLI_PATH` when that is set to something other than blanks, else the
//! name `claude`, and hands that to `spawnSync`, which looks a bare name up on the `PATH` the sidecar
//! inherited from this process (`sidecar_command` clears neither the environment nor the working directory).
//! [`claude_cli_missing`] makes the same decision over the same environment, so the first-start screen's
//! "not found" line and a tab's "not found" row come from one answer; the test at the bottom runs Node's own
//! `spawnSync` over a set of directory layouts and compares.
//!
//! Only the lookup is mirrored. The sidecar then runs `claude --version`; a `claude` that is on the `PATH` but
//! refuses that call makes the tab fail while this says it is there, which is the side to be wrong on: a line
//! that is silent is better than one that is false, and nothing here runs the program.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Where Claude Code's own install instructions are. The first-start screen, the panel's "not found" row and the
/// installer's warning all give this one address.
pub const CLAUDE_INSTALL_URL: &str = "https://code.claude.com/docs/en/quickstart";

/// What the sidecar's failure text says when it could not run the CLI at all (a lookup that found nothing): the
/// panel's own "not found" row recognises the same words (`agent-ui/web/src/problems.ts`, which a test compares).
pub const CLI_MISSING_FAILURE_MARKER: &str = "could not determine the installed claude CLI version";

/// Whether a tab's failure text is the sidecar saying it found no `claude` to run.
pub fn failure_names_missing_cli(failure: &str) -> bool {
    failure.contains(CLI_MISSING_FAILURE_MARKER)
}

/// The variable the sidecar reads to pick the CLI; the only way to name one that is not `claude` on the `PATH`.
const CLI_PATH_VAR: &str = "VERDANDI_CLAUDE_CLI_PATH";
/// What the sidecar runs when the variable is unset or blank.
const CLI_NAME: &str = "claude";
/// What a lookup searches when the environment has no `PATH` at all.
const DEFAULT_PATH: &str = "/usr/bin:/bin";

/// The command the sidecar will try to run: the variable, trimmed, else `claude`.
fn command_name(env: &dyn Fn(&str) -> Option<OsString>) -> OsString {
    let set = env(CLI_PATH_VAR).and_then(|v| {
        let v = v.to_string_lossy();
        let trimmed = v.trim();
        (!trimmed.is_empty()).then(|| OsString::from(trimmed))
    });
    set.unwrap_or_else(|| OsString::from(CLI_NAME))
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

/// Whether no `claude` the sidecar could start is there, over `env` and `runnable` (a regular file this process
/// may execute). A relative candidate is for `runnable` to resolve against the working directory.
pub fn claude_cli_missing(env: &dyn Fn(&str) -> Option<OsString>, runnable: impl Fn(&Path) -> bool) -> bool {
    let command = command_name(env);
    let path = env("PATH");
    !candidates(&command, path.as_deref()).iter().any(|c| runnable(c))
}

/// A regular file this process may execute.
fn runnable_file(path: &Path) -> bool {
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
    // SAFETY: `c_path` is a NUL-terminated string that outlives the call, which only reads it.
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

/// [`claude_cli_missing`] over this process's environment and file system: what the sidecar this process would
/// start will find. Looks at the disk, so it belongs on a worker.
pub fn claude_cli_missing_here() -> bool {
    claude_cli_missing(&|name| std::env::var_os(name), runnable_file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let owned: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |name| owned.iter().find(|(k, _)| k == name).map(|(_, v)| OsString::from(v))
    }

    fn files(list: &[&str]) -> impl Fn(&Path) -> bool {
        let set: HashSet<PathBuf> = list.iter().map(PathBuf::from).collect();
        move |p| set.contains(p)
    }

    #[test]
    fn a_failure_names_a_missing_cli_by_the_sidecars_own_words() {
        assert!(failure_names_missing_cli(
            "could not determine the installed claude CLI version via \"/no/such/claude\" (spawn error: ENOENT)"
        ));
        assert!(!failure_names_missing_cli(
            "the connection to the provider ended before this session did"
        ));
    }

    #[test]
    fn the_name_is_looked_up_on_path_in_order() {
        let is = files(&["/b/claude"]);
        assert!(!claude_cli_missing(&env_of(&[("PATH", "/a:/b")]), &is));
        assert!(claude_cli_missing(&env_of(&[("PATH", "/a:/c")]), &is));
        assert!(claude_cli_missing(&env_of(&[]), files(&[])));
    }

    #[test]
    fn the_variable_replaces_the_name_and_blanks_do_not() {
        let is = files(&["/opt/cl/claude-x", "/usr/bin/claude", "/bin/mine"]);
        // Set: it alone is looked at, though `claude` is on the PATH.
        let env = env_of(&[("PATH", "/usr/bin"), (CLI_PATH_VAR, "/opt/missing/claude")]);
        assert!(claude_cli_missing(&env, &is));
        let env = env_of(&[("PATH", "/usr/bin"), (CLI_PATH_VAR, "  /opt/cl/claude-x\t")]);
        assert!(!claude_cli_missing(&env, &is));
        // A bare name is a PATH lookup.
        let env = env_of(&[("PATH", "/bin"), (CLI_PATH_VAR, "mine")]);
        assert!(!claude_cli_missing(&env, &is));
        // Blank is unset.
        let env = env_of(&[("PATH", "/usr/bin"), (CLI_PATH_VAR, "   ")]);
        assert!(!claude_cli_missing(&env, &is));
    }

    #[test]
    fn an_empty_path_entry_is_the_working_directory_and_no_path_is_the_default_one() {
        let is = files(&["./claude", "/usr/bin/claude"]);
        assert!(!claude_cli_missing(&env_of(&[("PATH", "/x::/y")]), &is));
        assert!(!claude_cli_missing(&env_of(&[]), &is));
        assert!(claude_cli_missing(
            &env_of(&[("PATH", "/x")]),
            files(&["/usr/bin/claude"])
        ));
    }

    /// The decision above and the sidecar's own lookup are two implementations of one rule, in two languages.
    /// This runs Node's `spawnSync` (what the sidecar's version probe calls) in a directory the test lays out,
    /// under the same environment [`claude_cli_missing`] is given, and compares: a spawn error is "missing".
    #[test]
    fn it_agrees_with_what_nodes_spawn_sync_finds() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let root = std::env::temp_dir().join(format!("eitri-claude-locate-{}", uuid::Uuid::new_v4()));
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
        let d = |rel: &str| root.join(rel).display().to_string();

        // (label, PATH or unset, VERDANDI_CLAUDE_CLI_PATH or unset, working directory under root)
        let cases: Vec<(&str, Option<String>, Option<String>, &str)> = vec![
            (
                "found on the path",
                Some(format!("{}:/nonexistent", d("good"))),
                None,
                "empty",
            ),
            (
                "not on the path",
                Some(format!("{}:/nonexistent", d("empty"))),
                None,
                "empty",
            ),
            ("a file without the execute bit", Some(d("plain")), None, "empty"),
            ("a directory named claude", Some(d("dir")), None, "empty"),
            (
                "a directory, then the real one",
                Some(format!("{}:{}", d("dir"), d("good"))),
                None,
                "empty",
            ),
            (
                "a file without the bit, then the real one",
                Some(format!("{}:{}", d("plain"), d("good"))),
                None,
                "empty",
            ),
            (
                "an empty entry is the working directory",
                Some(":/nonexistent".into()),
                None,
                "cwd",
            ),
            (
                "an empty entry, nothing in the directory",
                Some(":/nonexistent".into()),
                None,
                "empty",
            ),
            ("a relative entry", Some("bin".into()), None, "rel"),
            ("a relative entry, wrong directory", Some("bin".into()), None, "empty"),
            ("an empty PATH", Some(String::new()), None, "cwd"),
            ("no PATH", None, None, "empty"),
            (
                "the variable, absolute and there",
                Some(d("empty")),
                Some(format!("{}/claude", d("good"))),
                "empty",
            ),
            (
                "the variable, absolute and gone, claude on the path",
                Some(d("good")),
                Some(d("empty") + "/claude"),
                "empty",
            ),
            (
                "the variable with blanks around it",
                Some(d("empty")),
                Some(format!("  {}/claude\t", d("good"))),
                "empty",
            ),
            (
                "the variable is blanks, claude on the path",
                Some(d("good")),
                Some("   ".into()),
                "empty",
            ),
            (
                "the variable is a bare name on the path",
                Some(d("named")),
                Some("mine".into()),
                "empty",
            ),
            (
                "the variable is a bare name, not on the path",
                Some(d("good")),
                Some("mine".into()),
                "empty",
            ),
            (
                "the variable is relative with a slash",
                Some(d("empty")),
                Some("./bin/claude".into()),
                "rel",
            ),
            (
                "the variable is relative, wrong directory",
                Some(d("empty")),
                Some("./bin/claude".into()),
                "empty",
            ),
            (
                "the variable names a directory",
                Some(d("good")),
                Some(d("dir") + "/claude"),
                "empty",
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
                             process.stdout.write(r.error ? 'missing' : 'found');";

        let mut disagreements = Vec::new();
        for (label, path, var, cwd) in &cases {
            let cwd = root.join(cwd);
            let env = |name: &str| -> Option<OsString> {
                match name {
                    "PATH" => path.as_ref().map(OsString::from),
                    CLI_PATH_VAR => var.as_ref().map(OsString::from),
                    _ => None,
                }
            };
            // `runnable` sees a relative candidate against `cwd`, as the child's working directory does.
            let rust_missing = claude_cli_missing(&env, |c| {
                if c.is_relative() {
                    runnable_file(&cwd.join(c))
                } else {
                    runnable_file(c)
                }
            });

            let mut probe = Command::new(&node);
            probe.env_clear().current_dir(&cwd).args(["-e", PROBE]);
            if let Some(path) = path {
                probe.env("PATH", path);
            }
            // What the sidecar passes to `spawnSync`, worked out by Node's side of the rule: the variable
            // trimmed, else `claude`.
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
                    "{label}: node says missing={node_missing}, this says missing={rust_missing}"
                ));
            }
        }
        let _ = std::fs::remove_dir_all(&root);
        assert!(disagreements.is_empty(), "{}", disagreements.join("\n"));
    }
}
