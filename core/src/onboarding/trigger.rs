//! When the screen opens by itself, and in what mode `window.setup` opens it.
//!
//! Both decisions look at the file system (the configuration directory, `init.lua`, the marker), so they
//! run on a worker thread, never on the GTK thread.

use std::ffi::OsString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use agent::private_fs::probe_user_config_dir;

use super::marker;
use super::{OpenMode, ShowWhy};

/// The line a launch that names a file logs when the screen would otherwise have opened.
pub const OPENS_FILES_LOG_LINE: &str =
    "[onboarding] a launch with a file leaves the first-start screen closed; :Eitri setup opens it";

/// The one line a start logs when the marker is what keeps the screen closed.
pub const CLOSED_LOG_LINE: &str =
    "[onboarding] the first-start screen is not shown again; :Eitri setup or window.setup opens it";

/// The name the generated file has in the configuration directory.
const INIT_NAME: &std::ffi::CStr = c"init.lua";

/// How many times "not now" may be said before the screen stops coming back.
const NOT_NOW_LIMIT: u32 = 2;

/// What the start knows before the file system is asked.
#[derive(Debug, Clone)]
pub struct Inputs {
    /// `--clean` was given.
    pub clean: bool,
    /// `EITRI_CONFIG_DIR`, as the environment has it.
    pub config_dir_env: Option<OsString>,
    /// `EITRI_FIRST_START`, as the environment has it.
    pub first_start_env: Option<OsString>,
    /// This is window 1 of a full launch.
    pub full_window: bool,
    /// The window starts on a plugin's own-window root.
    pub own_window: bool,
    /// The command line named a file: whoever typed it is here for that file, and the screen would take every key.
    pub opens_files: bool,
    /// The directory `init.lua` is read from.
    pub config_dir: PathBuf,
    /// The marker file; `None` when the state home cannot be told.
    pub marker: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Open,
    Closed,
}

/// Whether the screen opens by itself, and the lines to log. Each condition is tested in turn and the first
/// that fails closes it.
pub fn decide(inputs: &Inputs) -> (Decision, Vec<String>) {
    let mut log = Vec::new();
    if inputs.clean || non_empty(&inputs.config_dir_env) {
        return (Decision::Closed, log);
    }
    if let Some(value) = inputs.first_start_env.as_ref().filter(|v| !v.is_empty()) {
        if value == "off" {
            return (Decision::Closed, log);
        }
        log.push(format!(
            "EITRI_FIRST_START={} is not \"off\"; ignored",
            value.to_string_lossy()
        ));
    }
    if !inputs.full_window || inputs.own_window {
        return (Decision::Closed, log);
    }
    match init_lua_is_absent(&inputs.config_dir) {
        Ok(true) => {}
        Ok(false) => return (Decision::Closed, log),
        Err(refusal) => {
            log.push(format!(
                "[onboarding] the first-start screen stays closed: {:?} {refusal}",
                inputs.config_dir
            ));
            return (Decision::Closed, log);
        }
    }
    if let Some(path) = &inputs.marker {
        let (marker, note) = marker::read(path);
        log.extend(note);
        if marker.declined.is_some() || marker.not_now >= NOT_NOW_LIMIT {
            log.push(CLOSED_LOG_LINE.to_owned());
            return (Decision::Closed, log);
        }
    }
    // Nothing was asked, so nothing counts as an answer: no marker, and a later plain launch opens the screen as if
    // this one had not happened.
    if inputs.opens_files {
        log.push(OPENS_FILES_LOG_LINE.to_owned());
        return (Decision::Closed, log);
    }
    (Decision::Open, log)
}

fn non_empty(value: &Option<OsString>) -> bool {
    value.as_ref().is_some_and(|v| !v.is_empty())
}

/// `Ok(true)` when the directory passes the writer's test and holds no `init.lua` of any kind; `Ok(false)`
/// when something is there; the refusal when the directory fails the test.
fn init_lua_is_absent(config_dir: &Path) -> Result<bool, agent::private_fs::DirRefusal> {
    Ok(match probe_user_config_dir(config_dir)? {
        None => true,
        Some(dir) => nothing_at_init_lua(&dir),
    })
}

/// Only `ENOENT` from `fstatat` on the handle, without following a link, means nothing is there: a file, a
/// dangling link, a directory and a permission error all mean something is.
fn nothing_at_init_lua(dir: &OwnedFd) -> bool {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the descriptor is `dir`'s own and open, the name is NUL-terminated, and `stat` is a buffer of
    // the size `fstatat` writes; all three outlive the call.
    let looked = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            INIT_NAME.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    looked != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT)
}

/// How `window.setup` opens the screen in `config_dir`: asking when the directory passes the test and holds
/// no `init.lua`, showing only otherwise, with the reason.
pub fn reopen_mode(config_dir: &Path) -> (OpenMode, Option<ShowWhy>) {
    match init_lua_is_absent(config_dir) {
        Ok(true) => (OpenMode::Asked, None),
        Ok(false) => (OpenMode::ShowOnly, Some(ShowWhy::HasInit)),
        Err(refusal) => (OpenMode::ShowOnly, Some(ShowWhy::RefusedDir(refusal.to_string()))),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{symlink, PermissionsExt};

    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("eitri-trigger-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            Scratch(dir)
        }

        /// A 0700 configuration directory under the scratch root.
        fn config(&self) -> PathBuf {
            let dir = self.0.join("config");
            if !dir.exists() {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            dir
        }

        fn inputs(&self) -> Inputs {
            Inputs {
                clean: false,
                config_dir_env: None,
                first_start_env: None,
                full_window: true,
                own_window: false,
                opens_files: false,
                config_dir: self.config(),
                marker: Some(self.0.join("state").join("first-start.json")),
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            // A directory a test made unreadable cannot be walked.
            let _ = std::fs::set_permissions(self.0.join("config"), std::fs::Permissions::from_mode(0o700));
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn closed(inputs: &Inputs) -> Vec<String> {
        let (decision, log) = decide(inputs);
        assert_eq!(decision, Decision::Closed, "{log:?}");
        log
    }

    #[test]
    fn a_plain_first_start_opens() {
        let s = Scratch::new("plain");
        assert_eq!(decide(&s.inputs()), (Decision::Open, Vec::new()));
    }

    #[test]
    fn clean_keeps_it_closed() {
        let s = Scratch::new("clean");
        closed(&Inputs {
            clean: true,
            ..s.inputs()
        });
    }

    #[test]
    fn an_explicit_config_dir_keeps_it_closed() {
        let s = Scratch::new("explicit");
        closed(&Inputs {
            config_dir_env: Some("/somewhere".into()),
            ..s.inputs()
        });
    }

    #[test]
    fn an_empty_config_dir_env_does_not() {
        let s = Scratch::new("empty-env");
        let inputs = Inputs {
            config_dir_env: Some("".into()),
            ..s.inputs()
        };
        assert_eq!(decide(&inputs).0, Decision::Open);
    }

    #[test]
    fn first_start_off_keeps_it_closed() {
        let s = Scratch::new("off");
        let log = closed(&Inputs {
            first_start_env: Some("off".into()),
            ..s.inputs()
        });
        assert!(log.is_empty());
    }

    #[test]
    fn another_value_is_ignored_and_logged() {
        let s = Scratch::new("other");
        let (decision, log) = decide(&Inputs {
            first_start_env: Some("on".into()),
            ..s.inputs()
        });
        assert_eq!(decision, Decision::Open);
        assert_eq!(log, ["EITRI_FIRST_START=on is not \"off\"; ignored"]);
        let (decision, log) = decide(&Inputs {
            first_start_env: Some("".into()),
            ..s.inputs()
        });
        assert_eq!((decision, log.len()), (Decision::Open, 0));
    }

    #[test]
    fn a_launch_with_a_file_keeps_it_closed_and_counts_for_nothing() {
        let scratch = Scratch::new("opens-files");
        let inputs = Inputs {
            opens_files: true,
            ..scratch.inputs()
        };
        let log = closed(&inputs);
        assert_eq!(log, vec![OPENS_FILES_LOG_LINE.to_owned()]);
        assert!(
            !inputs.marker.as_ref().unwrap().exists(),
            "no marker: nothing was asked"
        );
        assert_eq!(
            decide(&scratch.inputs()).0,
            Decision::Open,
            "the next plain launch opens it"
        );
    }

    #[test]
    fn a_launch_with_a_file_says_nothing_where_the_screen_would_not_have_opened() {
        let scratch = Scratch::new("opens-files-quiet");
        let config = scratch.config();
        std::fs::create_dir_all(&config).unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(config.join("init.lua"), "").unwrap();
        let log = closed(&Inputs {
            opens_files: true,
            ..scratch.inputs()
        });
        assert!(log.is_empty(), "{log:?}");
    }

    #[test]
    fn a_second_window_keeps_it_closed() {
        let s = Scratch::new("second");
        closed(&Inputs {
            full_window: false,
            ..s.inputs()
        });
    }

    #[test]
    fn an_own_window_keeps_it_closed() {
        let s = Scratch::new("own");
        closed(&Inputs {
            own_window: true,
            ..s.inputs()
        });
    }

    #[test]
    fn a_refused_directory_keeps_it_closed_and_logs_why() {
        let s = Scratch::new("refused");
        let config = s.config();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o770)).unwrap();
        let log = closed(&s.inputs());
        assert_eq!(log.len(), 1, "{log:?}");
        assert!(log[0].contains("its group or others can write to it"), "{log:?}");
        assert_eq!(
            reopen_mode(&config),
            (
                OpenMode::ShowOnly,
                Some(ShowWhy::RefusedDir("its group or others can write to it".into()))
            )
        );
    }

    #[test]
    fn a_missing_directory_with_a_safe_parent_opens() {
        let s = Scratch::new("missing");
        let inputs = Inputs {
            config_dir: s.0.join("not-yet"),
            ..s.inputs()
        };
        assert_eq!(decide(&inputs).0, Decision::Open);
        assert_eq!(reopen_mode(&inputs.config_dir), (OpenMode::Asked, None));
    }

    #[test]
    fn a_symlinked_good_directory_opens() {
        let s = Scratch::new("link");
        let real = s.0.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = s.0.join("link");
        symlink(&real, &link).unwrap();
        let inputs = Inputs {
            config_dir: link,
            ..s.inputs()
        };
        assert_eq!(decide(&inputs).0, Decision::Open);
    }

    #[test]
    fn a_dangling_symlink_at_init_lua_keeps_it_closed() {
        let s = Scratch::new("dangling");
        symlink(s.0.join("nowhere"), s.config().join("init.lua")).unwrap();
        closed(&s.inputs());
        assert_eq!(reopen_mode(&s.config()), (OpenMode::ShowOnly, Some(ShowWhy::HasInit)));
    }

    #[test]
    fn a_directory_at_init_lua_keeps_it_closed() {
        let s = Scratch::new("dir");
        std::fs::create_dir(s.config().join("init.lua")).unwrap();
        closed(&s.inputs());
    }

    #[test]
    fn eacces_at_init_lua_keeps_it_closed() {
        let s = Scratch::new("eacces");
        let config = s.config();
        // Readable but not searchable: the directory test passes, and a lookup inside it is refused.
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        let lookup = std::fs::symlink_metadata(config.join("init.lua"));
        if lookup.map_err(|e| e.kind()).err() != Some(std::io::ErrorKind::PermissionDenied) {
            // Running as a user who can search anyway (root): the refusal cannot be made.
            return;
        }
        closed(&s.inputs());
        assert_eq!(reopen_mode(&config), (OpenMode::ShowOnly, Some(ShowWhy::HasInit)));
    }

    #[test]
    fn a_present_file_keeps_it_closed() {
        let s = Scratch::new("file");
        std::fs::write(s.config().join("init.lua"), "-- mine\n").unwrap();
        closed(&s.inputs());
        assert_eq!(reopen_mode(&s.config()), (OpenMode::ShowOnly, Some(ShowWhy::HasInit)));
    }

    #[test]
    fn an_empty_directory_reopens_asking() {
        let s = Scratch::new("asked");
        assert_eq!(reopen_mode(&s.config()), (OpenMode::Asked, None));
    }

    fn with_marker(s: &Scratch, text: &str) -> Inputs {
        let inputs = s.inputs();
        let path = inputs.marker.clone().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        inputs
    }

    #[test]
    fn declined_keeps_it_closed_with_the_log_line() {
        let s = Scratch::new("declined");
        let log = closed(&with_marker(&s, r#"{"v":1,"not_now":0,"declined":"2026-11-01"}"#));
        assert_eq!(log, [CLOSED_LOG_LINE]);
    }

    #[test]
    fn not_now_zero_opens() {
        let s = Scratch::new("nn0");
        assert_eq!(
            decide(&with_marker(&s, r#"{"v":1,"not_now":0}"#)),
            (Decision::Open, Vec::new())
        );
    }

    #[test]
    fn not_now_one_opens() {
        let s = Scratch::new("nn1");
        assert_eq!(decide(&with_marker(&s, r#"{"v":1,"not_now":1}"#)).0, Decision::Open);
    }

    #[test]
    fn not_now_two_keeps_it_closed_with_the_log_line() {
        let s = Scratch::new("nn2");
        let log = closed(&with_marker(&s, r#"{"v":1,"not_now":2}"#));
        assert_eq!(log, [CLOSED_LOG_LINE]);
    }

    #[test]
    fn a_corrupt_marker_opens_and_logs() {
        let s = Scratch::new("corrupt");
        let (decision, log) = decide(&with_marker(&s, "{ nope"));
        assert_eq!(decision, Decision::Open);
        assert_eq!(log.len(), 1);
        assert!(log[0].contains("counted as absent"), "{log:?}");
    }

    #[test]
    fn no_marker_path_opens() {
        let s = Scratch::new("nomarker");
        assert_eq!(
            decide(&Inputs {
                marker: None,
                ..s.inputs()
            })
            .0,
            Decision::Open
        );
    }
}
