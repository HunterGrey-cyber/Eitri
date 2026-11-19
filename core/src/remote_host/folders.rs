//! Folder suggestions over the verified host's fixed helper channel.

use std::path::Path;

use agent::ssh::ChannelLauncher;

use super::{install, steps};

pub fn complete(launcher: &dyn ChannelLauncher, helper: &Path, key: &str, prefix: &str) -> Result<Vec<String>, String> {
    if !prefix.starts_with('/') || prefix.contains('\0') || prefix.len() > 4096 {
        return Err("Enter an absolute server folder to complete.".into());
    }
    let hex = agent::ssh::encode_args(&[prefix.as_ref()]);
    let reply = (|| {
        let child = {
            let _key = launcher.hold_host_key(key)?;
            install::attached(launcher.spawn_step(steps::COMPLETE_FOLDERS, &[install::checked_path(helper)?, &hex])?)?
        };
        install::communicate_start(child, &[], &|_, _| {})
    })()
    .map_err(|e| e.to_string())?;
    let folders: Vec<String> = serde_json::from_slice(&reply).map_err(|e| format!("bad folder listing: {e}"))?;
    Ok(folders
        .into_iter()
        .filter(|folder| {
            !folder.contains('\0')
                && folder
                    .strip_prefix(prefix)
                    .and_then(|suffix| suffix.strip_suffix('/'))
                    .is_some_and(|name| !name.contains('/'))
        })
        .take(100)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_completion_goes_through_the_fixed_step() {
        use std::process::{Child, Command, Stdio};
        struct Local;
        impl ChannelLauncher for Local {
            fn spawn(&self, _: &[std::ffi::OsString]) -> std::io::Result<Child> {
                panic!("expected the fixed completion step")
            }

            fn hold_host_key(&self, key: &str) -> std::io::Result<Box<dyn Send>> {
                assert_eq!(key, "SHA256:example");
                Ok(Box::new(()))
            }

            fn spawn_step(&self, literal: &'static str, args: &[&str]) -> std::io::Result<Child> {
                assert_eq!(literal, steps::COMPLETE_FOLDERS);
                assert_eq!(
                    args,
                    [
                        "/srv/bin/eitri-server",
                        &agent::ssh::encode_args(&["/srv/project [x];".as_ref()])
                    ]
                );
                Command::new("sh")
                    .args([
                        "-c",
                        r#"printf '%s\n' eitri-lifecycle-attached-v1 '["/srv/project [x]; a/","/elsewhere/"]'"#,
                    ])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
            }
        }
        assert_eq!(
            complete(
                &Local,
                Path::new("/srv/bin/eitri-server"),
                "SHA256:example",
                "/srv/project [x];"
            )
            .unwrap(),
            ["/srv/project [x]; a/"]
        );
    }
}
