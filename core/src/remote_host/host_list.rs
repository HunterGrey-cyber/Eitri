//! Literal SSH aliases and previously opened hosts for the project chooser.

use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::remote_project::RemoteProjects;

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

pub fn hosts(
    home: Option<&Path>,
    store: &RemoteProjects,
    mut skipped: impl FnMut(String),
) -> Result<Vec<String>, String> {
    let mut hosts = BTreeSet::new();
    if let Some(home) = home {
        let ssh = home.join(".ssh");
        read(&ssh.join("config"), home, &ssh, true, &mut hosts, &mut skipped);
    }
    let (projects, _) = store.list().map_err(|e| e.to_string())?;
    hosts.extend(
        projects
            .into_iter()
            .map(|project| project.host)
            .filter(|host| valid(host)),
    );
    Ok(hosts.into_iter().collect())
}

fn valid(host: &str) -> bool {
    !host.starts_with('!') && !host.contains(['*', '?']) && agent::ssh::validate_host(host).is_ok()
}

fn read_config(path: &Path) -> io::Result<String> {
    let check = |metadata: fs::Metadata| {
        if !metadata.is_file() {
            Err(io::Error::other("expected a regular file"))
        } else if metadata.len() > MAX_CONFIG_BYTES {
            Err(io::Error::other("file exceeds 1 MiB"))
        } else {
            Ok(())
        }
    };
    check(fs::symlink_metadata(path)?)?;
    // A replacement between the type check and open must neither block on a FIFO nor be read.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)?;
    check(file.metadata()?)?;
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(io::Error::other("file exceeds 1 MiB"));
    }
    String::from_utf8(bytes).map_err(io::Error::other)
}

fn read(
    path: &Path,
    home: &Path,
    ssh: &Path,
    includes: bool,
    hosts: &mut BTreeSet<String>,
    skipped: &mut impl FnMut(String),
) {
    let config = match read_config(path) {
        Ok(config) => config,
        Err(error) => {
            if !includes || error.kind() != io::ErrorKind::NotFound {
                skipped(format!(
                    "Skipped SSH config {} ({error})",
                    crate::onboarding::display::display_name(&path.to_string_lossy())
                ));
            }
            return;
        }
    };
    for line in config.lines() {
        let line = line.trim_start();
        let end = line
            .find(|ch: char| ch.is_ascii_whitespace() || ch == '=')
            .unwrap_or(line.len());
        let keyword = &line[..end];
        let args = line[end..].trim_start_matches(|ch: char| ch.is_ascii_whitespace() || ch == '=');
        if keyword.eq_ignore_ascii_case("host") {
            hosts.extend(words(args).into_iter().filter(|alias| valid(alias)));
        }
        if includes && keyword.eq_ignore_ascii_case("include") {
            for pattern in words(args) {
                let pattern = match pattern.strip_prefix("~/") {
                    Some(tail) => home.join(tail),
                    None => ssh.join(pattern),
                };
                for file in expand(&pattern) {
                    read(&file, home, ssh, false, hosts, skipped);
                }
            }
        }
    }
}

fn words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in line.chars() {
        if escaped {
            word.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                word.push(ch);
            }
        } else if matches!(ch, '"' | '\'') {
            quote = Some(ch);
        } else if ch == '#' {
            break;
        } else if ch.is_ascii_whitespace() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(ch);
        }
    }
    if quote.is_none() && !escaped && !word.is_empty() {
        words.push(word);
    }
    words
}

fn expand(pattern: &Path) -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::new()];
    for part in pattern.components() {
        let name = part.as_os_str();
        if !name.as_bytes().iter().any(|ch| matches!(ch, b'*' | b'?' | b'[')) {
            for path in &mut paths {
                path.push(name);
            }
            continue;
        }
        let Ok(pattern) = CString::new(name.as_bytes()) else {
            return Vec::new();
        };
        paths = paths
            .into_iter()
            .flat_map(|dir| std::fs::read_dir(dir).into_iter().flatten().flatten())
            .filter(|entry| {
                let Ok(name) = CString::new(entry.file_name().as_bytes()) else {
                    return false;
                };
                // SAFETY: Both arguments are live NUL-terminated strings.
                unsafe { libc::fnmatch(pattern.as_ptr(), name.as_ptr(), libc::FNM_PERIOD) == 0 }
            })
            .map(|entry| entry.path())
            .collect();
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_list_comes_from_ssh_config_without_wildcards_and_from_records() {
        let scratch = agent::state_dirs::test_workspace_dir("chooser-host-list");
        let ssh = scratch.join(".ssh");
        std::fs::create_dir_all(ssh.join("config.d")).unwrap();
        std::fs::write(
            ssh.join("config"),
            "Host example-host *.example !excluded unknown?\nInclude config.d/*.conf\nHOST=also\n",
        )
        .unwrap();
        std::fs::write(
            ssh.join("config.d/team.conf"),
            "Host \"work-host\" # comment\nInclude nested\n",
        )
        .unwrap();
        std::fs::write(ssh.join("nested"), "Host too-deep\n").unwrap();
        let store = RemoteProjects::new(Some(scratch.as_os_str()), None);
        let project = serde_json::from_value(serde_json::json!({
            "version": 1, "host": "recent-host", "host_key_sha256": "SHA256:example",
            "root": "/srv/project", "helper_version": "0.4.0", "created_at": 0
        }))
        .unwrap();
        store.save(&project).unwrap();
        assert_eq!(
            hosts(Some(&scratch), &store, |line| panic!("{line}")).unwrap(),
            ["also", "example-host", "recent-host", "work-host"]
        );
        std::fs::remove_dir_all(scratch).unwrap();
    }

    #[test]
    fn host_list_reports_skipped_large_and_nonregular_configs() {
        use crate::test_scratch_dir::ScratchDir;

        let scratch = ScratchDir::new("remote", "chooser-config-files");
        let ssh = scratch.join(".ssh");
        fs::create_dir(&ssh).unwrap();
        let oversized = ssh.join("oversized");
        fs::write(&oversized, "Host too-large\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&oversized)
            .unwrap()
            .set_len(MAX_CONFIG_BYTES + 1)
            .unwrap();
        let store = RemoteProjects::new(Some(scratch.as_os_str()), None);
        let mut lines = Vec::new();
        fs::write(ssh.join("config"), "Host example-host\nInclude oversized\n").unwrap();
        assert_eq!(
            hosts(Some(&scratch), &store, |line| lines.push(line)).unwrap(),
            ["example-host"]
        );
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("oversized") && lines[0].contains("exceeds 1 MiB"));

        let fifo = CString::new(ssh.join("fifo").as_os_str().as_bytes()).unwrap();
        // SAFETY: The NUL-terminated path names a new entry in the test's SSH directory.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        fs::write(ssh.join("config"), "Include fifo /dev/null /dev/stdin\n").unwrap();
        lines.clear();
        assert!(hosts(Some(&scratch), &store, |line| lines.push(line))
            .unwrap()
            .is_empty());
        assert_eq!(lines.len(), 3);
        for (line, name) in lines.iter().zip(["fifo", "/dev/null", "/dev/stdin"]) {
            assert!(
                line.contains(name) && line.contains("expected a regular file"),
                "{line}"
            );
        }

        fs::remove_file(ssh.join("config")).unwrap();
        fs::rename(oversized, ssh.join("config")).unwrap();
        lines.clear();
        assert!(hosts(Some(&scratch), &store, |line| lines.push(line))
            .unwrap()
            .is_empty());
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("exceeds 1 MiB"));
    }
}
