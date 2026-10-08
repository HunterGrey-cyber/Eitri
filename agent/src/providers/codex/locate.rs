//! Which `codex` to run: a pure decision over injected lookups, so every rule has a test that needs
//! no process and no `PATH`.
//!
//! The order is the variable, then the `init.lua` key, then `PATH`. Every `PATH` lookup skips empty and
//! relative entries and yields an absolute path: the child runs in the project root, where a relative
//! name would resolve to a file the project supplied.

use crate::jsonrpc_link::resolve_program;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};

/// The variable that names the binary and beats everything else.
pub const CODEX_ENV: &str = "EITRI_CODEX";
/// The `init.lua` key that names the binary.
pub const CODEX_COMMAND_KEY: &str = "agent.codex.command";
/// The binary's name on `PATH`.
const CODEX_NAME: &str = "codex";

/// Why no `codex` could be chosen. Naming the variable or key that was wrong is the point: a value the
/// user set must never be quietly replaced by one further down the order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocateError {
    /// `EITRI_CODEX` is not an absolute path.
    EnvNotAbsolute(PathBuf),
    /// `EITRI_CODEX` names no file.
    EnvNotAFile(PathBuf),
    /// `agent.codex.command` is neither an absolute path nor a bare name.
    CommandNotNameOrAbsolute(String),
    /// `agent.codex.command` names no file, or no file of that name is on `PATH`.
    CommandNotFound(String),
    /// Nothing named `codex` is on `PATH`.
    NotOnPath,
}

impl fmt::Display for LocateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LocateError::EnvNotAbsolute(p) => write!(
                f,
                "{CODEX_ENV}={} is not an absolute path (codex runs from the project directory, so a relative \
                 one would name a different file)",
                p.display()
            ),
            LocateError::EnvNotAFile(p) => write!(f, "{CODEX_ENV}={} is not a file", p.display()),
            LocateError::CommandNotNameOrAbsolute(c) => write!(
                f,
                "{CODEX_COMMAND_KEY} = {c:?} is neither an absolute path nor a bare command name"
            ),
            LocateError::CommandNotFound(c) => write!(f, "{CODEX_COMMAND_KEY} = {c:?} names no file"),
            LocateError::NotOnPath => write!(f, "{}", super::provider::NOT_FOUND),
        }
    }
}

impl std::error::Error for LocateError {}

/// Chooses the `codex` binary.
///
/// - `env_codex`: `EITRI_CODEX`. Absolute and a file, else an error naming the variable. An empty value
///   counts as unset.
/// - `config_command`: `agent.codex.command`. An absolute path that is a file, or a bare name looked up
///   on `PATH`. An empty value counts as unset.
/// - `path_var`: `PATH`.
/// - `is_file`: whether a path is a regular file.
pub fn decide_codex(
    env_codex: Option<&OsStr>,
    config_command: Option<&str>,
    path_var: Option<&OsStr>,
    is_file: impl Fn(&Path) -> bool,
) -> Result<PathBuf, LocateError> {
    if let Some(raw) = env_codex.filter(|v| !v.is_empty()) {
        let path = PathBuf::from(raw);
        if !path.is_absolute() {
            return Err(LocateError::EnvNotAbsolute(path));
        }
        if !is_file(&path) {
            return Err(LocateError::EnvNotAFile(path));
        }
        return Ok(path);
    }
    if let Some(command) = config_command.filter(|c| !c.is_empty()) {
        let name = Path::new(command);
        if !name.is_absolute() && name.components().count() != 1 {
            return Err(LocateError::CommandNotNameOrAbsolute(command.to_string()));
        }
        return match resolve_program(OsStr::new(command), path_var, &is_file) {
            Some(found) if is_file(&found) => Ok(found),
            _ => Err(LocateError::CommandNotFound(command.to_string())),
        };
    }
    resolve_program(OsStr::new(CODEX_NAME), path_var, &is_file).ok_or(LocateError::NotOnPath)
}

/// [`decide_codex`] over this process's environment and file system.
pub fn locate_codex(config_command: Option<&str>) -> Result<PathBuf, LocateError> {
    decide_codex(
        std::env::var_os(CODEX_ENV).as_deref(),
        config_command,
        std::env::var_os("PATH").as_deref(),
        |p| p.is_file(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn files(list: &[&str]) -> impl Fn(&Path) -> bool {
        let set: HashSet<PathBuf> = list.iter().map(PathBuf::from).collect();
        move |p| set.contains(p)
    }

    #[test]
    fn decide_codex_order_and_absolute_rules() {
        let is_file = files(&["/opt/x/codex", "/usr/bin/codex", "/cfg/bin/mycodex", "/env/codex"]);
        let path = Some(OsStr::new("/usr/bin:/cfg/bin"));

        // The variable beats the key and PATH.
        let got = decide_codex(Some(OsStr::new("/env/codex")), Some("mycodex"), path, &is_file);
        assert_eq!(got, Ok(PathBuf::from("/env/codex")));
        // The key beats PATH, by absolute path or by name.
        assert_eq!(
            decide_codex(None, Some("/opt/x/codex"), path, &is_file),
            Ok(PathBuf::from("/opt/x/codex"))
        );
        assert_eq!(
            decide_codex(None, Some("mycodex"), path, &is_file),
            Ok(PathBuf::from("/cfg/bin/mycodex"))
        );
        // PATH last.
        assert_eq!(
            decide_codex(None, None, path, &is_file),
            Ok(PathBuf::from("/usr/bin/codex"))
        );

        // An empty value is unset.
        assert_eq!(
            decide_codex(Some(OsStr::new("")), Some(""), path, &is_file),
            Ok(PathBuf::from("/usr/bin/codex"))
        );

        // The variable is a startup failure naming it, never replaced by a later rule.
        assert_eq!(
            decide_codex(Some(OsStr::new("codex")), None, path, &is_file),
            Err(LocateError::EnvNotAbsolute(PathBuf::from("codex")))
        );
        assert_eq!(
            decide_codex(Some(OsStr::new("/env/missing")), Some("mycodex"), path, &is_file),
            Err(LocateError::EnvNotAFile(PathBuf::from("/env/missing")))
        );
        let message = LocateError::EnvNotAFile(PathBuf::from("/env/missing")).to_string();
        assert!(message.contains("EITRI_CODEX"), "{message}");

        // The key: a relative path with a directory part is refused, a name nobody has is not found.
        assert_eq!(
            decide_codex(None, Some("bin/codex"), path, &is_file),
            Err(LocateError::CommandNotNameOrAbsolute("bin/codex".into()))
        );
        assert_eq!(
            decide_codex(None, Some("/opt/y/codex"), path, &is_file),
            Err(LocateError::CommandNotFound("/opt/y/codex".into()))
        );
        assert_eq!(
            decide_codex(None, Some("nothing"), path, &is_file),
            Err(LocateError::CommandNotFound("nothing".into()))
        );
        assert!(LocateError::CommandNotFound("nothing".into())
            .to_string()
            .contains("agent.codex.command"));

        // Empty and relative PATH entries never supply the program: the project root is the cwd.
        let from_project = files(&["codex", "./codex", "bin/codex"]);
        assert_eq!(
            decide_codex(None, None, Some(OsStr::new(":.:bin:relative/dir")), &from_project),
            Err(LocateError::NotOnPath)
        );
        let message = LocateError::NotOnPath.to_string();
        assert!(
            message.contains("codex login") && message.contains("EITRI_CODEX"),
            "{message}"
        );
        // No PATH at all.
        assert_eq!(decide_codex(None, None, None, &is_file), Err(LocateError::NotOnPath));
    }
}
