//! Where a language server's command is found on the server.
//!
//! The command comes from the laptop's Neovim, so it may name a laptop path. A mason path (`.../mason/bin/<name>`)
//! names a package mason installed on the laptop, so only its name is looked for here. Any other absolute path is
//! used when it is an executable regular file on the server; otherwise its name is looked for too. A name is looked
//! for on the login `PATH` (absolute entries only), then in the server's own mason directory.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The command was not found on the server; `name` is what was looked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Missing {
    pub name: String,
}

/// Resolves `argv0` on the server. `login_path` is the login `PATH` split into entries; `data_home` is the login's
/// `XDG_DATA_HOME` when it is set and absolute (else `<home>/.local/share` is mason's parent).
pub fn command(argv0: &str, login_path: &[PathBuf], home: &Path, data_home: Option<&Path>) -> Result<PathBuf, Missing> {
    let path = Path::new(argv0);
    if path.is_absolute() && !argv0.contains("/mason/bin/") && executable(path) {
        return Ok(path.to_owned());
    }
    let name = match path.file_name().and_then(|name| name.to_str()) {
        Some(name) if !name.is_empty() && name != "." && name != ".." => name,
        _ => return Err(Missing { name: argv0.to_owned() }),
    };
    let missing = || Missing { name: name.to_owned() };
    for dir in login_path.iter().filter(|dir| dir.is_absolute()) {
        let candidate = dir.join(name);
        if executable(&candidate) {
            return Ok(candidate);
        }
    }
    let data_home = match data_home.filter(|dir| dir.is_absolute()) {
        Some(dir) => dir.to_owned(),
        None if home.is_absolute() => home.join(".local/share"),
        None => return Err(missing()),
    };
    let candidate = data_home.join("nvim/mason/bin").join(name);
    if executable(&candidate) {
        return Ok(candidate);
    }
    Err(missing())
}

/// A regular file (after links) with an execute bit.
fn executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(label: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("lsp-resolve-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn tool(path: &Path, mode: u32) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn names_go_through_the_path_then_mason() {
        let dir = scratch("order");
        let (bin, home) = (dir.0.join("bin"), dir.0.join("home"));
        tool(&bin.join("ra"), 0o700);
        tool(&home.join(".local/share/nvim/mason/bin/ra"), 0o700);
        tool(&home.join(".local/share/nvim/mason/bin/lua-ls"), 0o700);
        tool(&bin.join("plain"), 0o600);
        let path = [PathBuf::from("relative"), bin.clone()];
        assert_eq!(command("ra", &path, &home, None), Ok(bin.join("ra")));
        assert_eq!(
            command("lua-ls", &path, &home, None),
            Ok(home.join(".local/share/nvim/mason/bin/lua-ls"))
        );
        assert_eq!(
            command("/laptop/x/mason/bin/ra", &path, &home, None),
            Ok(bin.join("ra"))
        );
        assert_eq!(
            command("plain", &path, &home, None),
            Err(Missing { name: "plain".into() })
        );
        let elsewhere = dir.0.join("data");
        tool(&elsewhere.join("nvim/mason/bin/only-here"), 0o700);
        assert_eq!(
            command("only-here", &path, Path::new(""), Some(&elsewhere)),
            Ok(elsewhere.join("nvim/mason/bin/only-here"))
        );
        assert_eq!(
            command("only-here", &path, Path::new(""), Some(Path::new("rel"))),
            Err(Missing {
                name: "only-here".into()
            })
        );
    }

    #[test]
    fn an_absolute_path_is_used_when_it_runs_here_and_searched_by_name_when_not() {
        let dir = scratch("absolute");
        let bin = dir.0.join("bin");
        tool(&dir.0.join("direct/tool"), 0o755);
        tool(&bin.join("tool"), 0o755);
        let path = [bin.clone()];
        let direct = dir.0.join("direct/tool");
        assert_eq!(
            command(direct.to_str().unwrap(), &path, &dir.0, None),
            Ok(direct.clone())
        );
        assert_eq!(
            command("/not/on/this/server/tool", &path, &dir.0, None),
            Ok(bin.join("tool"))
        );
        for argv0 in ["", "/", "..", "a/.."] {
            assert!(command(argv0, &path, &dir.0, None).is_err(), "{argv0:?}");
        }
    }
}
