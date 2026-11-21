//! Purpose `COMMAND`: one program the account could run at a login prompt (a formatter, a linter, `make`), in a
//! directory of the server, with nothing that waits for a person. The same user could `ssh` in and run it; this adds
//! no ability that login does not already give.
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use eitri_server_proto::ExecStart;

use super::Prepared;
use crate::daemon::login_env::Environment;
use crate::daemon::sync::audit_word;

/// Checks a start and prepares it to run in `login` (the login environment the daemon captured): `cwd` absolute, in
/// normal form and an existing directory (anywhere the account can reach, not only under a project's root);
/// `argv[0]` a bare name found on the login `PATH` (absolute entries only) or an absolute path in normal form to an
/// executable regular file. The child gets the login environment with `TERM=dumb` and everything that could ask a
/// person answered as git's runs have it, and nothing from the laptop. The audit names only the program's file name:
/// the other arguments may carry secrets. Looks at the disk, so callers run it off the async runtime.
pub fn prepare(start: &ExecStart, login: &Environment) -> Result<Prepared, String> {
    if !start.cwd.starts_with('/') || !crate::daemon::sidecars::in_normal_form(start.cwd.as_bytes()) {
        return Err(format!(
            "the working directory must be absolute and in normal form: {:?}",
            start.cwd
        ));
    }
    if start.cwd.bytes().any(|byte| byte == 0) {
        return Err("the working directory holds a NUL".to_owned());
    }
    let Some(name) = start.argv.first() else {
        return Err("no command was given".to_owned());
    };
    if start.argv.iter().any(|arg| arg.bytes().any(|byte| byte == 0)) {
        return Err("an argument holds a NUL".to_owned());
    }
    if !std::fs::metadata(&start.cwd).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(format!(
            "the working directory {:?} is not a directory on this server",
            start.cwd
        ));
    }
    let program = resolve(name, login)
        .ok_or_else(|| format!("`{}` is not on the login PATH of this account", name.escape_debug()))?;
    let mut env = super::git::without_prompts(login, &["TERM"]);
    env.push((OsString::from("TERM"), OsString::from("dumb")));
    let file_name = program
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Prepared {
        program,
        args: start.argv[1..].to_vec(),
        cwd: start.cwd.clone(),
        env,
        audit: format!("command {}", audit_word(&file_name)),
        deadline: super::EXEC_DEADLINE,
    })
}

/// Where `name` runs from: a bare name (no `/`, not `.` or `..`) as the first executable regular file of that name on
/// the login `PATH`, or an absolute path in normal form that is one. A relative path would depend on the daemon's own
/// working directory, never the one the laptop named.
fn resolve(name: &str, login: &Environment) -> Option<PathBuf> {
    if name.starts_with('/') {
        let path = Path::new(name);
        return (crate::daemon::sidecars::in_normal_form(name.as_bytes()) && super::is_executable_file(path))
            .then(|| path.to_owned());
    }
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return None;
    }
    super::on_login_path(login, name)
}
