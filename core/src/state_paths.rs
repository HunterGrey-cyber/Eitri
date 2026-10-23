//! Shared locations and filenames for Eitri's private state.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// `<state home>/eitri/<sub>`, using an absolute `XDG_STATE_HOME` or the absolute home's default.
pub fn state_subdir(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>, sub: &str) -> Option<PathBuf> {
    match (xdg_state_home, home) {
        (Some(state), _) if Path::new(state).is_absolute() => Some(PathBuf::from(state).join("eitri").join(sub)),
        (_, Some(home)) if Path::new(home).is_absolute() => {
            Some(PathBuf::from(home).join(".local/state/eitri").join(sub))
        }
        _ => None,
    }
}

/// Creates `dir` -- a [`state_subdir`], `<state home>/eitri/<sub>` -- and anything missing above
/// it 0700, and tightens `dir` and its `eitri` parent to 0700 where an older build left either
/// open. Files inside it are written 0600 through `agent::private_fs::write_private`.
pub fn create_state_dir(dir: &Path) -> std::io::Result<()> {
    agent::private_fs::create_private_dir_all(dir, dir.parent().unwrap_or(dir))
}

/// `<state home>/eitri/layout`, from the two variables that decide it: `XDG_STATE_HOME`, else
/// `$HOME/.local/state`. Empty or relative values are ignored so state never resolves against
/// the working directory, which can be the project itself. `None` when neither gives a directory.
pub fn state_dir(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    state_subdir(xdg_state_home, home, "layout")
}

/// The per-project JSON filename: the shared project key followed by `.json`.
pub fn file_name(project_root: &Path) -> String {
    format!("{}.json", project_key(project_root))
}

/// The first 16 hex digits of the conversation id for the project root, using its path bytes unchanged.
pub fn project_key(project_root: &Path) -> String {
    agent::conversation_id_for_cwd(project_root)[..16].to_string()
}

/// Where a file at `path` is written first: for a `.json` file, `<its name>.<this process's id>.tmp`
/// beside it.
pub fn temporary(path: &Path) -> PathBuf {
    path.with_extension(format!("json.{}.tmp", std::process::id()))
}

/// How many times a store write tries its lock, and how long it pauses between two tries. Counted in
/// tries, so what a caller may be held for is fixed when it is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockTries {
    pub attempts: u32,
    pub pause: std::time::Duration,
}

/// For a write the window's own thread makes and needs the answer to at once. Another window holds the
/// lock only across one small read and rewrite (no `fsync`), well under a millisecond, so a few tries a
/// millisecond apart see it free; past that, the write is refused, not waited for, and the window's
/// thread was held for three 1 ms pauses (about 3 ms) and the four tries.
pub const ON_THE_WINDOW_THREAD: LockTries = LockTries {
    attempts: 4,
    pause: std::time::Duration::from_millis(1),
};

/// For a writer thread, which nothing waits on: about two seconds of tries, enough for a disk that is
/// slow for a moment. A window holding it longer is stopped or hung; the writer keeps what it has and
/// tries again with its next write.
pub const ON_A_WRITER_THREAD: LockTries = LockTries {
    attempts: 500,
    pause: std::time::Duration::from_millis(4),
};

/// The lock file that stands for `file`'s read-modify-write: `<file's name>.lock` beside it.
pub fn lock_path(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

/// Takes `file`'s store lock (an `flock` on [`lock_path`], through `agent::private_fs::PrivateLock`),
/// trying as `tries` says and never blocking in between. Every writer of a store that more than one
/// window writes holds this from before it reads the file until after its rename, so a write never
/// starts from a file another window is about to replace. Fails with `WouldBlock`, naming the file,
/// when every try found it held.
pub fn lock_store(file: &Path, tries: LockTries) -> std::io::Result<agent::private_fs::PrivateLock> {
    let path = lock_path(file);
    let lock = agent::private_fs::PrivateLock::open(&path)?;
    for attempt in 1..=tries.attempts.max(1) {
        if lock.try_lock()? {
            return Ok(lock);
        }
        #[cfg(test)]
        test_hooks::on_busy();
        if attempt < tries.attempts {
            std::thread::sleep(tries.pause);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        format!("another window is writing {}", file.display()),
    ))
}

/// Hooks a test sets on its own thread to stop a store write at a chosen point, so two writers can be
/// interleaved on purpose. Thread-local: a test running beside it never trips them.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::cell::RefCell;

    thread_local! {
        static AFTER_READ: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
        static ON_BUSY: RefCell<Option<Box<dyn FnMut()>>> = RefCell::new(None);
    }

    /// Runs `hook` once, the next time a store write on this thread has read its file.
    pub(crate) fn set_after_read(hook: impl FnOnce() + 'static) {
        AFTER_READ.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    /// Runs `hook` each time a store write on this thread finds its lock held by someone else.
    pub(crate) fn set_on_busy(hook: impl FnMut() + 'static) {
        ON_BUSY.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(crate) fn after_read() {
        let hook = AFTER_READ.with(|h| h.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }

    pub(crate) fn on_busy() {
        ON_BUSY.with(|h| {
            if let Some(hook) = h.borrow_mut().as_mut() {
                hook();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::layout::persist::file_name_v2;

    #[test]
    fn every_state_path_is_unchanged() {
        // A child keeps fixed environment values away from tests running on other threads.
        if std::env::var_os("XDG_STATE_HOME").as_deref() != Some(OsStr::new("/state"))
            || std::env::var_os("HOME").as_deref() != Some(OsStr::new("/home/test"))
        {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "state_paths::tests::every_state_path_is_unchanged",
                    "--nocapture",
                ])
                .env("XDG_STATE_HOME", "/state")
                .env("HOME", "/home/test")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            return;
        }
        let root = Path::new("/projects/state-paths");
        let state = Some(OsStr::new("/state"));
        let home = Some(OsStr::new("/home/test"));
        let paths = [
            (state_dir(state, home), "/state/eitri/layout"),
            (
                crate::permission_store::state_dir(state, home),
                "/state/eitri/permissions",
            ),
            (crate::agent_prefs::state_dir(state, home), "/state/eitri/agent"),
            (crate::saved_tabs::state_dir(state, home), "/state/eitri/tabs"),
            (crate::prompt_history::state_dir(state, home), "/state/eitri/history"),
            (crate::companion::stdio::log_dir(state, home), "/state/eitri/companion"),
            (
                crate::turn_review::shadow::review_dir_for(state, home, root),
                "/state/eitri/review/410d926fb96fb107",
            ),
            (
                crate::keymap::tmux::notice::state_file(state, home),
                "/state/eitri/keymap-import.json",
            ),
            (state_subdir(state, home, "trust"), "/state/eitri/trust"),
            (state_subdir(state, home, "plugins"), "/state/eitri/plugins"),
            (crate::plugin::state::PluginStore::eitri_root_from_env(), "/state/eitri"),
            (state_subdir(state, home, ""), "/state/eitri/"),
            (state_subdir(state, home, "neovide"), "/state/eitri/neovide"),
            (state_subdir(state, home, "mac"), "/state/eitri/mac"),
        ];
        for (actual, expected) in paths {
            assert_eq!(actual.unwrap().as_os_str(), OsStr::new(expected));
        }

        assert_eq!(file_name(root), "410d926fb96fb107.json");
        assert_eq!(file_name_v2(root), "410d926fb96fb107.v2.json");
        assert_eq!(
            crate::permission_store::path(Path::new("/state/eitri/permissions"), root),
            Path::new("/state/eitri/permissions/410d926fb96fb107.json"),
        );
        assert_eq!(
            crate::prompt_history::path(Path::new("/state/eitri/history"), root),
            Path::new("/state/eitri/history/410d926fb96fb107.jsonl"),
        );
        assert_eq!(crate::companion::stdio::log_file_name(root), "410d926fb96fb107.log");
        let runtime = Path::new("/runtime/eitri");
        assert_eq!(
            crate::panel_control::socket_path(runtime, root).unwrap(),
            agent::socket_path::in_dir(runtime, "p-410d926fb96fb107.sock").unwrap(),
        );
        assert_eq!(
            crate::panel_control::split_socket_path(runtime, root, 1234, 0xabcdef).unwrap(),
            agent::socket_path::in_dir(runtime, "split-410d926fb96fb107-1234-00abcdef.sock").unwrap(),
        );
        assert_eq!(
            crate::panel_control::pid_path(runtime, root),
            Path::new("/runtime/eitri/p-410d926fb96fb107.pid"),
        );
        for (path, expected) in [
            (
                "/state/eitri/layout/410d926fb96fb107.v2.json",
                format!(
                    "/state/eitri/layout/410d926fb96fb107.v2.json.{}.tmp",
                    std::process::id()
                ),
            ),
            (
                "/state/eitri/history/410d926fb96fb107.jsonl",
                format!("/state/eitri/history/410d926fb96fb107.json.{}.tmp", std::process::id()),
            ),
        ] {
            assert_eq!(temporary(Path::new(path)), PathBuf::from(expected));
        }
    }

    #[test]
    fn project_keys_keep_the_original_path_bytes() {
        use std::os::unix::ffi::OsStrExt;

        assert_eq!(
            super::project_key(Path::new("/projects/state-paths")),
            "410d926fb96fb107"
        );
        assert_eq!(super::project_key(Path::new("/projects/a/../b")), "f929b2147857018c");
        assert_eq!(
            super::project_key(Path::new(OsStr::from_bytes(b"/projects/non-utf8-\xff"))),
            "ba9ae4de287db8c1",
        );
    }

    #[test]
    fn unusable_state_homes_fall_back_only_to_an_absolute_home() {
        for state in [None, Some(OsStr::new("")), Some(OsStr::new("relative"))] {
            assert_eq!(
                state_dir(state, Some(OsStr::new("/home/test"))),
                Some(PathBuf::from("/home/test/.local/state/eitri/layout")),
            );
            for home in [None, Some(OsStr::new("")), Some(OsStr::new("relative"))] {
                assert_eq!(state_dir(state, home), None);
            }
        }
        assert_eq!(
            state_dir(Some(OsStr::new("/state")), None),
            Some(PathBuf::from("/state/eitri/layout"))
        );
    }
}
