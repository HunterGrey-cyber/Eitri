//! Purpose `GIT`: `git`, run as the account would run it at a login prompt, with nothing that waits for a person.
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use eitri_server_proto::{ExecPurpose, ExecStart};

use super::Prepared;
use crate::daemon::login_env::Environment;
use crate::daemon::sync::audit_word;

/// Checks a start and prepares it to run in `login` (the login environment the daemon captured): purpose `GIT`, `cwd`
/// absolute and in normal form, `argv[0]` exactly `git`, found as the first executable `git` on the login `PATH`. The
/// child gets the login environment with everything that could ask a person answered (see [`ASKING`]), so a command
/// that would ask fails instead of waiting. A refusal says what is wrong.
/// Looks at the disk, so callers run it off the async runtime.
pub fn prepare(start: &ExecStart, login: &Environment) -> Result<Prepared, String> {
    if start.purpose != ExecPurpose::Git as i32 {
        return Err("the helper runs only git for a laptop".to_owned());
    }
    if !start.cwd.starts_with('/') || !crate::daemon::sidecars::in_normal_form(start.cwd.as_bytes()) {
        return Err(format!(
            "the working directory must be absolute and in normal form: {:?}",
            start.cwd
        ));
    }
    if start.cwd.bytes().any(|byte| byte == 0) {
        return Err("the working directory holds a NUL".to_owned());
    }
    match start.argv.first().map(String::as_str) {
        Some("git") => {}
        Some(other) => return Err(format!("only git runs here, not {other:?}")),
        None => return Err("no command was given".to_owned()),
    }
    if start.argv.iter().any(|arg| arg.bytes().any(|byte| byte == 0)) {
        return Err("an argument holds a NUL".to_owned());
    }
    let program = find_git(login)?;
    // The shim puts its pathspec settings in argv; the login's switches would change what they mean.
    let env = without_prompts(
        login,
        &[
            "GIT_ICASE_PATHSPECS",
            "GIT_LITERAL_PATHSPECS",
            "GIT_GLOB_PATHSPECS",
            "GIT_NOGLOB_PATHSPECS",
        ],
    );
    let audit = match subcommand(&start.argv[1..]) {
        Some(subcommand) => format!("git {}", audit_word(subcommand)),
        None => "git".to_owned(),
    };
    Ok(Prepared {
        program,
        args: start.argv[1..].to_vec(),
        cwd: start.cwd.clone(),
        env,
        audit,
        deadline: super::EXEC_DEADLINE,
    })
}

/// Everything that could ask a person for something, and what it gets instead: no editor, no terminal prompt, an
/// empty askpass (git then asks nothing), ssh never asking through a program, Git Credential Manager never
/// interactive. `SSH_ASKPASS` is removed as well.
const ASKING: [&str; 6] = [
    "GIT_EDITOR",
    "GIT_SEQUENCE_EDITOR",
    "GIT_TERMINAL_PROMPT",
    "GIT_ASKPASS",
    "SSH_ASKPASS_REQUIRE",
    "GCM_INTERACTIVE",
];
const ANSWERS: [&str; 6] = ["false", "false", "0", "", "never", "never"];

/// `login` with everything in [`ASKING`] answered, `SSH_ASKPASS` and the keys in `removed` taken out: the environment
/// of a run that must fail instead of waiting for a person.
pub(super) fn without_prompts(login: &Environment, removed: &[&str]) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = login
        .iter()
        .filter(|(key, _)| {
            !key.to_str()
                .is_some_and(|key| ASKING.contains(&key) || key == "SSH_ASKPASS" || removed.contains(&key))
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    env.extend(
        ASKING
            .iter()
            .zip(ANSWERS)
            .map(|(key, value)| (OsString::from(key), OsString::from(value))),
    );
    env
}

/// Git's options before the subcommand that take the next argument as their value.
const GLOBAL_WITH_VALUE: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--super-prefix",
    "--config-env",
    "--attr-source",
    "--exec-path",
    "--list-cmds",
];

/// The subcommand in git's arguments (after the program name): the first argument past git's own options, skipping
/// an option's separate value (`-C <dir>`, `-c <name>=<value>`, `--git-dir <dir>`); `--opt=value` is one argument.
pub fn subcommand(args: &[String]) -> Option<&str> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if GLOBAL_WITH_VALUE.contains(&arg.as_str()) {
            args.next();
        } else if arg == "--" {
            return args.next().map(String::as_str);
        } else if !arg.starts_with('-') {
            return Some(arg);
        }
    }
    None
}

/// The first `git` on the login `PATH` that is an executable regular file.
fn find_git(login: &Environment) -> Result<PathBuf, String> {
    super::on_login_path(login, "git").ok_or_else(|| {
        let path = login.get(OsStr::new("PATH")).cloned().unwrap_or_default();
        format!("git is not on the login PATH ({})", path.to_string_lossy())
    })
}
