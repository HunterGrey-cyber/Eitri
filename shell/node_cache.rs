use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

pub(crate) fn out_dir() -> std::path::PathBuf {
    std::env::var_os("OUT_DIR")
        .or_else(|| option_env!("OUT_DIR").map(OsString::from))
        .expect("Cargo supplies OUT_DIR")
        .into()
}

/// Node tooling shares a cache under Cargo's target instead of leaving one in each run's TMPDIR.
pub(crate) fn command(program: impl AsRef<std::ffi::OsStr>, out_dir: &Path) -> Command {
    command_with_env(program, out_dir, |key| std::env::var_os(key))
}

fn command_with_env(
    program: impl AsRef<std::ffi::OsStr>,
    out_dir: &Path,
    env: impl Fn(&str) -> Option<OsString>,
) -> Command {
    let mut cmd = Command::new(program);
    if env("NODE_COMPILE_CACHE").is_none() && env("NODE_DISABLE_COMPILE_CACHE").is_none() {
        cmd.env("NODE_COMPILE_CACHE", out_dir.join("../../node-compile-cache"));
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_cache_is_under_target_instead_of_tmpdir() {
        let cmd = command_with_env("npm", Path::new("/target/debug/build/shell-hash/out"), |_| None);
        let value = cmd
            .get_envs()
            .find(|(key, _)| *key == "NODE_COMPILE_CACHE")
            .unwrap()
            .1
            .unwrap();
        assert_eq!(
            value,
            Path::new("/target/debug/build/shell-hash/out/../../node-compile-cache")
        );
    }

    #[test]
    fn caller_cache_and_disable_settings_are_preserved_even_when_empty() {
        for key in ["NODE_COMPILE_CACHE", "NODE_DISABLE_COMPILE_CACHE"] {
            for value in ["", "caller-value"] {
                let cmd = command_with_env("node", Path::new("/target/out"), |name| {
                    (name == key).then(|| value.into())
                });
                assert_eq!(cmd.get_envs().count(), 0, "inherit {key}={value:?}");
            }
        }
    }
}
