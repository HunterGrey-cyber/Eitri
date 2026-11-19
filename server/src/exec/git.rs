//! The one purpose `Exec` serves: `git`, run as the account would run it at a login prompt, with nothing that waits for
//! a person.
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt;
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
    let mut env: Vec<(OsString, OsString)> = login
        .iter()
        .filter(|(key, _)| {
            !key.to_str().is_some_and(|key| {
                ASKING.contains(&key)
                    || matches!(
                        key,
                        "SSH_ASKPASS"
                            | "GIT_ICASE_PATHSPECS"
                            | "GIT_LITERAL_PATHSPECS"
                            | "GIT_GLOB_PATHSPECS"
                            | "GIT_NOGLOB_PATHSPECS"
                    )
            })
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    env.extend(
        ASKING
            .iter()
            .zip(ANSWERS)
            .map(|(key, value)| (OsString::from(key), OsString::from(value))),
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

/// The first `git` on the login `PATH` that is an executable regular file. Only absolute entries are searched: a
/// relative one would depend on the daemon's own working directory.
fn find_git(login: &Environment) -> Result<PathBuf, String> {
    let path = login.get(OsStr::new("PATH")).cloned().unwrap_or_default();
    for dir in std::env::split_paths(&path).filter(|dir| dir.is_absolute()) {
        let candidate = dir.join("git");
        if std::fs::metadata(&candidate)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        {
            return Ok(candidate);
        }
    }
    Err(format!("git is not on the login PATH ({})", path.to_string_lossy()))
}
