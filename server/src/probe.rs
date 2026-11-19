//! `eitri-server probe`: what the laptop needs to know about this server before it installs or connects.
use std::ffi::{CStr, OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use eitri_server_proto::probe::{ProbeReport, RootState};

use crate::daemon::login_env::{self, Captured};
use crate::daemon::sidecars::in_normal_form;

/// How long `loginctl` may take; a stalled logind makes the answer unknown, not the probe late.
const LOGINCTL_BUDGET: Duration = Duration::from_secs(3);

/// Prints the report for this server. The optional argument is the project root to check.
pub async fn run(args: &[OsString]) -> io::Result<()> {
    let root = match args {
        [] => None,
        [root] if !Path::new(root).is_absolute() => return Err(crate::invalid("the root must be an absolute path")),
        // A sidecar start refuses any other spelling, so the probe does not call one usable.
        [root] if !in_normal_form(root.as_bytes()) => return Err(crate::invalid("the root must be in normal form")),
        [root] => Some(PathBuf::from(root)),
        _ => return Err(crate::invalid("expected at most one root")),
    };
    let (account, captured) = login_env::capture_account().await?;
    let report = report(&account, &captured, root.as_deref()).await?;
    print!("{}", report.render());
    Ok(())
}

/// Lists one level of directories matching a literal prefix.
pub fn complete_folders(args: &[OsString]) -> io::Result<()> {
    let prefix = match args {
        [prefix] => prefix
            .to_str()
            .filter(|text| text.starts_with('/') && text.len() <= 4096 && !text.contains('\0')),
        _ => None,
    }
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "expected one absolute UTF-8 folder prefix"))?;
    let folders = matching_folders(prefix)?;
    serde_json::to_writer(io::stdout().lock(), &folders)?;
    Ok(())
}

fn matching_folders(prefix: &str) -> io::Result<Vec<String>> {
    let (dir, partial) = prefix.rsplit_once('/').expect("an absolute path has a slash");
    let mut folders = Vec::new();
    let mut encoded_bytes = 2;
    for entry in std::fs::read_dir(Path::new(if dir.is_empty() { "/" } else { dir }))?.take(20_000) {
        let Ok(entry) = entry else { continue };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !name.starts_with(partial) || (name.starts_with('.') && !partial.starts_with('.')) {
            continue;
        }
        if !entry.path().is_dir() {
            continue;
        }
        let folder = format!("{dir}/{name}/");
        encoded_bytes += serde_json::to_vec(&folder)?.len() + 1;
        if encoded_bytes > 48 * 1024 || folders.len() == 100 {
            break;
        }
        folders.push(folder);
    }
    folders.sort_unstable();
    Ok(folders)
}

#[cfg(test)]
mod folder_tests {
    use super::*;

    #[test]
    fn complete_folders_lists_directories_with_a_literal_prefix() {
        let root = agent::state_dirs::test_workspace_dir("complete-folders");
        let path = root.join("project [x]; tree");
        std::fs::create_dir_all(path.join("child")).unwrap();
        std::fs::create_dir(root.join("project x; other")).unwrap();
        std::fs::write(root.join("project [x]; file"), b"file").unwrap();
        let result = matching_folders(root.join("project [x];").to_str().unwrap()).unwrap();
        assert_eq!(result, [format!("{}/", path.display())]);
        std::fs::remove_dir_all(root).unwrap();
    }
}

async fn report(account: &login_env::Environment, captured: &Captured, root: Option<&Path>) -> io::Result<ProbeReport> {
    let text = |env: &login_env::Environment, key: &str| {
        env.get(OsStr::new(key))
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let (data_home, state_home) = crate::run_dir::environment_roots()?;
    let state_dir = state_home.join("eitri-server");
    let login_path = text(&captured.env, "PATH");
    let uid = agent::private_fs::euid();
    let on_path = |name: &str| find_on_path(name, &login_path);
    let state = root.map_or(RootState::Missing, root_state);
    let marker = match root {
        Some(path) if state == RootState::Ok => eitri_core::remote_sync::other_sync::marker(&path.canonicalize()?)?,
        _ => None,
    };
    Ok(ProbeReport {
        uname: uname(),
        home: PathBuf::from(account.get(OsStr::new("HOME")).cloned().unwrap_or_default()),
        data_dir: data_home.join("eitri-server"),
        free_bytes: free_bytes(&state_dir),
        state_dir,
        login_shell: text(account, "SHELL"),
        claude: on_path("claude"),
        git: on_path("git"),
        gzip: on_path("gzip").is_some(),
        login_path,
        glibc: glibc(),
        linger: logind(
            &["show-user", &uid.to_string(), "-p", "Linger"],
            "Linger",
            &captured.env,
        )
        .await,
        kill_user_processes: kill_user_processes(&captured.env).await,
        root: state,
        sync_excluded: marker.as_ref().map(|m| m.excluded),
        sync_ignore_line: marker.as_ref().map(|m| m.line.clone()),
        sync_marker: marker.map(|m| m.path),
    })
}

fn uname() -> String {
    // SAFETY: `utsname` is plain bytes and `uname` fills it.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut name) } != 0 {
        return "unknown".to_owned();
    }
    // SAFETY: the kernel NUL-terminates both fields.
    let field = |bytes: &[libc::c_char]| unsafe { CStr::from_ptr(bytes.as_ptr()) }.to_string_lossy().into_owned();
    format!("{} {}", field(&name.sysname), field(&name.machine))
}

fn glibc() -> Option<(u32, u32)> {
    // SAFETY: glibc returns a static NUL-terminated string.
    let version = unsafe { CStr::from_ptr(libc::gnu_get_libc_version()) }.to_str().ok()?;
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// The first executable file called `name` in an absolute directory of `path`.
fn find_on_path(name: &str, path: &str) -> Option<PathBuf> {
    path.split(':')
        .filter(|dir| Path::new(dir).is_absolute())
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| {
            std::fs::metadata(candidate).is_ok_and(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0)
        })
}

/// Free space for an unprivileged writer on the filesystem that holds (or would hold) `dir`.
fn free_bytes(dir: &Path) -> Option<u64> {
    eitri_server::journal::available_bytes(dir).ok()
}

/// logind's KillUserProcesses. `show-session` with no session names the manager, which always has the property.
async fn kill_user_processes(env: &login_env::Environment) -> Option<bool> {
    logind(&["show-session", "-p", "KillUserProcesses"], "KillUserProcesses", env).await
}

/// One `Key=yes|no` property from `loginctl`; `None` when loginctl is absent, slow, failing or says something else.
async fn logind(args: &[&str], key: &str, env: &login_env::Environment) -> Option<bool> {
    let path = env
        .get(OsStr::new("PATH"))
        .map(|path| path.to_string_lossy().into_owned());
    let program = find_on_path("loginctl", path.as_deref().unwrap_or(""))
        .or_else(|| find_on_path("loginctl", "/usr/bin:/bin"))?;
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(LOGINCTL_BUDGET, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    property(&String::from_utf8(output.stdout).ok()?, key)
}

/// The `key=yes|no` line of `loginctl show-*` output.
fn property(text: &str, key: &str) -> Option<bool> {
    match text
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))?
    {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

fn root_state(root: &Path) -> RootState {
    match std::fs::metadata(root) {
        Err(_) => RootState::Missing,
        Ok(metadata) if !metadata.is_dir() => RootState::NotADirectory,
        Ok(metadata) if metadata.uid() != agent::private_fs::euid() => RootState::NotOwned,
        Ok(_) => RootState::Ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("probe-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        agent::private_fs::create_private_dir_all(&path, &path).unwrap();
        path
    }

    #[test]
    fn root_states_follow_what_is_on_disk() {
        let dir = scratch("root");
        std::fs::write(dir.join("file"), b"").unwrap();
        assert_eq!(root_state(&dir), RootState::Ok);
        assert_eq!(root_state(&dir.join("file")), RootState::NotADirectory);
        assert_eq!(root_state(&dir.join("absent")), RootState::Missing);
        if agent::private_fs::euid() != 0 {
            assert_eq!(root_state(Path::new("/")), RootState::NotOwned);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_sync_marker_is_the_nearest_one_at_or_above_the_root() {
        let sync_marker = |root: &Path| {
            eitri_core::remote_sync::other_sync::marker(root)
                .unwrap()
                .map(|m| m.path)
        };
        let dir = scratch("marker");
        let project = dir.join("a/b");
        std::fs::create_dir_all(&project).unwrap();
        assert_eq!(sync_marker(&project), None);
        std::fs::create_dir(dir.join(".stfolder")).unwrap();
        assert_eq!(sync_marker(&project), Some(dir.join(".stfolder")));
        std::fs::create_dir(dir.join("a/.stfolder")).unwrap();
        assert_eq!(sync_marker(&project), Some(dir.join("a/.stfolder")));
        std::fs::write(project.join(".stfolder"), b"").unwrap();
        assert_eq!(sync_marker(&project), Some(project.join(".stfolder")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn root_probe_reports_the_exclusion_at_the_marker() {
        let dir = scratch("sync-exclusion");
        let project = dir.join("work/project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir(dir.join(".stfolder")).unwrap();
        std::os::unix::fs::symlink("/bin/false", dir.join("loginctl")).unwrap();
        let env = login_env::Environment::from([("PATH".into(), dir.clone().into_os_string())]);
        let captured = Captured {
            env: env.clone(),
            fallback: None,
        };
        for (rules, excluded) in [("/work/", true), ("/work/\n!/work/project", false)] {
            std::fs::write(dir.join(".stignore"), rules).unwrap();
            let found = report(&env, &captured, Some(&project)).await.unwrap();
            let wire = ProbeReport::parse(&found.render()).unwrap();
            assert_eq!(wire.sync_marker, Some(dir.join(".stfolder")));
            assert_eq!(wire.sync_ignore_line.as_deref(), Some("/work/project"));
            assert_eq!(wire.sync_excluded, Some(excluded));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn programs_are_found_only_as_executables_in_absolute_path_entries() {
        let dir = scratch("path");
        std::fs::write(dir.join("tool"), b"").unwrap();
        std::fs::write(dir.join("plain"), b"").unwrap();
        std::fs::set_permissions(dir.join("tool"), std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let path = format!("relative:{}", dir.display());
        assert_eq!(find_on_path("tool", &path), Some(dir.join("tool")));
        assert_eq!(find_on_path("plain", &path), None);
        assert_eq!(find_on_path("tool", "relative"), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn logind_properties_are_yes_or_no_and_nothing_else() {
        assert_eq!(property("KillUserProcesses=yes\n", "KillUserProcesses"), Some(true));
        assert_eq!(property("Linger=no\n", "Linger"), Some(false));
        assert_eq!(property("Other=yes\nLinger=no\n", "Linger"), Some(false));
        for text in [
            "Linger=maybe\n",
            "Linger=\n",
            "LingerX=yes\n",
            "Linger yes\n",
            "",
            "garbage",
        ] {
            assert_eq!(property(text, "Linger"), None, "{text:?}");
        }
    }

    #[tokio::test]
    async fn kill_user_processes_queries_the_logind_manager_property() {
        let dir = scratch("logind");
        write_script(
            &dir.join("loginctl"),
            "#!/bin/sh\n[ \"$*\" = 'show-session -p KillUserProcesses' ] || exit 1\necho KillUserProcesses=yes\n",
        );
        let env = login_env::Environment::from([("PATH".into(), dir.clone().into_os_string())]);
        assert_eq!(kill_user_processes(&env).await, Some(true));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn kill_user_processes_is_read_where_logind_runs() {
        if !Path::new("/run/systemd/system").exists() {
            return;
        }
        let Some(loginctl) = find_on_path("loginctl", "/usr/bin:/bin") else {
            return;
        };
        // The systemd runtime directory is also visible in sandboxes without access to logind.
        let available = tokio::time::timeout(
            LOGINCTL_BUDGET,
            tokio::process::Command::new(loginctl)
                .arg("list-sessions")
                .env_clear()
                .kill_on_drop(true)
                .output(),
        )
        .await;
        if matches!(available, Ok(Ok(output)) if output.status.success()) {
            let env = login_env::Environment::from([("PATH".into(), "/usr/bin:/bin".into())]);
            assert!(kill_user_processes(&env).await.is_some());
        }
    }

    /// An executable script at `path`, written by a child so no descendant of a test thread holds it open for writing.
    fn write_script(path: &Path, body: &str) {
        use std::io::Write;
        let mut writer = std::process::Command::new("/bin/sh")
            .args(["-c", "cat >\"$1\" && chmod 700 \"$1\"", "writer"])
            .arg(path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        writer.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
        assert!(writer.wait().unwrap().success());
    }

    #[tokio::test]
    async fn the_report_takes_shell_and_path_from_the_login_shell_not_the_developers_profile() {
        let dir = scratch("shell");
        let shell = dir.join("login-shell");
        let login_path = format!("{}:/usr/bin:/bin", dir.join("bin").display());
        // Called as `<shell> -lc <command>`, as the capture calls a login shell.
        write_script(
            &shell,
            &format!("#!/bin/sh\nPATH='{login_path}'\nexport PATH\neval \"$2\"\n"),
        );
        let mut account = login_env::minimal_environment().unwrap();
        account.insert("SHELL".into(), shell.clone().into_os_string());
        let captured = login_env::capture_with(account.clone()).await;
        assert_eq!(captured.fallback, None);
        let report = report(&account, &captured, Some(&dir)).await.unwrap();
        assert_eq!(report.login_shell, shell.to_str().unwrap());
        assert_eq!(report.login_path, login_path);
        assert_eq!(report.root, RootState::Ok);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn an_empty_passwd_shell_is_reported_as_written() {
        let mut account = login_env::minimal_environment().unwrap();
        account.insert("SHELL".into(), "".into());
        let captured = login_env::capture_with(account.clone()).await;
        assert!(captured.fallback.is_some());
        let report = report(&account, &captured, None).await.unwrap();
        assert_eq!(report.login_shell, "");
        assert_eq!(ProbeReport::parse(&report.render()).unwrap(), report);
    }

    #[test]
    fn free_space_is_read_from_the_nearest_existing_directory() {
        let dir = scratch("free");
        assert!(free_bytes(&dir.join("not/yet/there")).is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
