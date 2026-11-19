//! `eitri host ...`, before anything of a window: no toolkit, no display, no editor environment. ssh runs
//! `eitri host known-hosts-probe` as the masters' `KnownHostsCommand` on every connection, so that one goes first and
//! does nothing but record the key; the other commands are `eitri_core::remote_host::cli`'s, over ssh and the
//! person's terminal.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use eitri_core::lua::config::{parse_remote_dev_enabled, REMOTE_DEV_ENABLED_KEY};
use eitri_core::lua::kernel::{refuse_panels, Kernel};
use eitri_core::remote_host::cli;
use gtk4::glib;

/// The subcommand.
pub(crate) const SUBCOMMAND: &str = "host";

/// Runs `eitri host` when `args` (argv without the program) start with it; `None` for any other command line.
pub(crate) fn run(args: impl IntoIterator<Item = OsString>) -> Option<glib::ExitCode> {
    let mut args = args.into_iter();
    if args.next()? != SUBCOMMAND {
        return None;
    }
    let rest: Vec<OsString> = args.collect();
    if rest.first().is_some_and(|first| first == "mutagen-transport") {
        let error = eitri_core::remote_sync::transport::exec(&rest[1..]).unwrap_err();
        eprintln!(
            "eitri host mutagen-transport: {}",
            eitri_core::remote_sync::terminal_text(&error.to_string())
        );
        return Some(code(255));
    }
    if rest.first().is_some_and(|first| first == "git") {
        let args = rest[1..].strip_prefix(&[OsString::from("--")]).unwrap_or(&rest[1..]);
        let status = match eitri_core::remote_sync::git_shim::ShimEnv::from_process() {
            Ok(env) => eitri_core::remote_sync::git_shim::run(args.to_vec(), env),
            Err(error) => {
                eprintln!("Eitri: {}", eitri_core::remote_sync::terminal_text(&error));
                128
            }
        };
        return Some(code(status));
    }
    if rest.first().is_some_and(|first| first == cli::KNOWN_HOSTS_PROBE) {
        return Some(code(cli::known_hosts_probe(&rest[1..])));
    }
    let Some(rest) = rest
        .iter()
        .map(|arg| arg.to_str().map(str::to_owned))
        .collect::<Option<Vec<String>>>()
    else {
        eprintln!("eitri host: arguments must be valid UTF-8");
        return Some(glib::ExitCode::from(64));
    };
    let state_home = std::env::var_os("XDG_STATE_HOME");
    let home = std::env::var_os("HOME");
    let store = eitri_core::remote_project::RemoteProjects::new(state_home.as_deref(), home.as_deref());
    let Some(ssh_dir) = cli::ssh_dir(state_home.as_deref(), home.as_deref()) else {
        eprintln!("eitri host: no state directory (set HOME or XDG_STATE_HOME)");
        return Some(glib::ExitCode::FAILURE);
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("eitri host: cannot find this Eitri's own executable ({e})");
            return Some(glib::ExitCode::FAILURE);
        }
    };
    let known_hosts = home
        .as_deref()
        .map(|home| PathBuf::from(home).join(".ssh/known_hosts"))
        .unwrap_or_default();
    let mut reach = cli::SshReach::new(ssh_dir, exe, known_hosts);
    // Status and conflicts inspect recorded state without starting an engine. Loading init.lua here
    // would mix its startup output (including a missing-file notice) into the JSON document.
    let needs_settings = match rest.as_slice() {
        [command, verb, ..] if command == "sync" => !matches!(verb.as_str(), "status" | "conflicts"),
        [command, ..] => matches!(command.as_str(), "open" | "stop" | "uninstall"),
        [] => false,
    };
    if needs_settings {
        reach = reach.with_sync_settings(init_sync_settings());
    }
    let mut term = Stdio;
    let enabled = remote_dev_enabled;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let status = cli::run(
        &rest,
        &mut cli::Context {
            store: &store,
            reach: &mut reach,
            term: &mut term,
            remote_dev_enabled: &enabled,
            now,
        },
    );
    Some(code(status))
}

fn code(status: i32) -> glib::ExitCode {
    glib::ExitCode::from(status.clamp(0, 255) as u8)
}

/// `remote_dev.enabled` as `init.lua` sets it, read the way the project chooser reads its key: a kernel of its own
/// that runs `init.lua` and builds no panel. A value that is not a switch leaves remote development off, said once.
pub(crate) fn remote_dev_enabled() -> bool {
    let Ok(config_dir) = eitri_core::config_dir::resolve(&|key| std::env::var_os(key)) else {
        return false;
    };
    let Ok(kernel) = Kernel::new(config_dir.clone(), refuse_panels) else {
        return false;
    };
    kernel.load_init_file(&config_dir.join("init.lua"));
    let value = kernel.config.borrow().get(REMOTE_DEV_ENABLED_KEY).map(str::to_owned);
    parse_remote_dev_enabled(value.as_deref()).unwrap_or_else(|why| {
        eprintln!("eitri: {why}; remote development stays off");
        false
    })
}

/// `remote_dev.*` for the sync verbs, read from `init.lua` the way [`remote_dev_enabled`] reads its key.
pub(crate) fn init_sync_settings() -> eitri_core::remote_host::cli_sync::SyncSettings {
    let Ok(config_dir) = eitri_core::config_dir::resolve(&|key| std::env::var_os(key)) else {
        return Default::default();
    };
    let Ok(kernel) = Kernel::new(config_dir.clone(), refuse_panels) else {
        return Default::default();
    };
    kernel.load_init_file(&config_dir.join("init.lua"));
    let (settings, problems) = sync_settings(&kernel.config.borrow());
    for problem in problems {
        eprintln!("eitri: {problem}");
    }
    settings
}

/// The terminal `eitri host` runs in: lines to stdout, questions answered on stdin.
struct Stdio;

impl cli::Terminal for Stdio {
    fn say(&mut self, line: &str) {
        say_to(&mut std::io::stdout().lock(), line);
    }

    fn ask(&mut self, question: &str) -> bool {
        ask_to(&mut std::io::stdout().lock(), question);
        let mut answer = String::new();
        if cli::read_answer(&mut answer).is_err() {
            return false;
        }
        matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    }
}

/// A line of `eitri host` on `out`. Lines name server files and repeat server-side messages: control characters are
/// escaped first, so none reaches the terminal as an escape sequence.
fn say_to(out: &mut dyn Write, line: &str) {
    let _ = writeln!(out, "{}", eitri_core::remote_sync::terminal_text(line));
}

/// A question of `eitri host` on `out`, escaped as [`say_to`] escapes a line, left open for the answer.
fn ask_to(out: &mut dyn Write, question: &str) {
    let _ = write!(out, "{} ", eitri_core::remote_sync::terminal_text(question));
    let _ = out.flush();
}

/// Prepare the fixed script before GTK. Settings come from the window's existing kernel after it loads.
pub(crate) fn prepare_git_shim(
    origin: &eitri_core::remote_project::WindowOrigin,
) -> Result<Option<eitri_core::remote_sync::shim_dir::ShimDir>, String> {
    use eitri_core::remote_sync::shim_dir::{ShimDir, ShimOptions};
    if matches!(origin, eitri_core::remote_project::WindowOrigin::Local) {
        return Ok(None);
    }
    ShimDir::prepare(
        std::env::current_exe().map_err(|e| e.to_string())?,
        eitri_core::remote_sync::shim_dir::real_git_from_process()?,
        ShimOptions::default(),
    )
    .map(Some)
    .map_err(|e| format!("git shim unavailable: {e}"))
}

pub(crate) fn git_shim_options(
    config: &eitri_core::lua::config::ConfigStore,
) -> (eitri_core::remote_sync::shim_dir::ShimOptions, Vec<String>) {
    use eitri_core::lua::config::*;
    let mut problems = vec![];
    let mut fallback = |error: String| {
        problems.push(format!("{error}; git shim uses the default"));
    };
    let enabled = parse_remote_dev_enabled(config.get(REMOTE_DEV_ENABLED_KEY)).unwrap_or_else(|e| {
        fallback(e);
        false
    });
    let flush_cap = parse_remote_dev_flush_cap(config.get(REMOTE_DEV_FLUSH_CAP_KEY)).unwrap_or_else(|e| {
        fallback(e);
        3.0
    });
    let mutagen = parse_remote_dev_mutagen(config.get(REMOTE_DEV_MUTAGEN_KEY)).unwrap_or_else(|e| {
        fallback(e);
        None
    });
    (
        eitri_core::remote_sync::shim_dir::ShimOptions {
            enabled,
            flush_cap,
            mutagen,
        },
        problems,
    )
}

/// `remote_dev.mutagen`, `remote_dev.tripwire_mib` and `remote_dev.flush_cap` for the sync actor and the sync verbs;
/// a value that does not parse leaves the default, with the problem said.
pub(crate) fn sync_settings(
    config: &eitri_core::lua::config::ConfigStore,
) -> (eitri_core::remote_host::cli_sync::SyncSettings, Vec<String>) {
    use eitri_core::lua::config::*;
    let mut problems = vec![];
    let mut fallback = |error: String| problems.push(format!("{error}; sync uses the default"));
    let defaults = eitri_core::remote_host::cli_sync::SyncSettings::default();
    let mutagen = parse_remote_dev_mutagen(config.get(REMOTE_DEV_MUTAGEN_KEY)).unwrap_or_else(|e| {
        fallback(e);
        None
    });
    let tripwire_mib = parse_remote_dev_tripwire_mib(config.get(REMOTE_DEV_TRIPWIRE_MIB_KEY)).unwrap_or_else(|e| {
        fallback(e);
        defaults.tripwire_mib
    });
    let flush_cap = parse_remote_dev_flush_cap(config.get(REMOTE_DEV_FLUSH_CAP_KEY))
        .map(std::time::Duration::from_secs_f64)
        .unwrap_or_else(|e| {
            fallback(e);
            defaults.flush_cap
        });
    (
        eitri_core::remote_host::cli_sync::SyncSettings {
            enabled: parse_remote_dev_enabled(config.get(REMOTE_DEV_ENABLED_KEY)).unwrap_or(false),
            mutagen,
            tripwire_mib,
            flush_cap,
            send_waits_for_flush: parse_remote_dev_send_waits_for_flush(
                config.get(REMOTE_DEV_SEND_WAITS_FOR_FLUSH_KEY),
            )
            .unwrap_or_else(|e| {
                fallback(e);
                defaults.send_waits_for_flush
            }),
        },
        problems,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    #[test]
    fn host_lines_and_questions_carry_no_escape_byte() {
        let mut out = Vec::new();
        say_to(&mut out, "file modes on h: 1 sync writes into /p: \u{1b}]52;c;x\u{7}");
        ask_to(&mut out, "Move \u{9b}2J? [y/N]");
        assert!(!out.iter().any(|b| *b == 0x1b || *b == 0x07), "{out:?}");
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains('\u{9b}'), "{text:?}");
        assert!(
            text.starts_with("file modes on h: 1 sync writes into /p: \\u{1b}]52;c;x\\u{7}\n"),
            "{text:?}"
        );
        assert!(text.ends_with("Move \\u{9b}2J? [y/N] "), "{text:?}");
    }

    #[test]
    fn sync_inspection_writes_one_complete_json_document() {
        use eitri_core::remote_project::{RemoteProject, RemoteProjects};
        use eitri_core::remote_sync::{actor, status_file, status_schema::ConflictEntry};
        let root = agent::state_dirs::test_workspace_dir("host-json");
        let state = root.join("state");
        let project = RemoteProject {
            version: 1,
            host: "example-host".into(),
            host_key_sha256: "key".into(),
            root: root.clone(),
            helper_version: "test".into(),
            sync: None,
            created_at: 0,
            queued_stops: vec![],
        };
        RemoteProjects::new(Some(state.as_os_str()), None)
            .save(&project)
            .unwrap();
        let dir = actor::project_dir(&state, &project);
        status_file::prepare_dir(&state, &dir).unwrap();
        let path = "file\u{202e}\u{9b}2J\u{2028}\nname";
        let mut status = status_file::compose(&project.host, &Default::default(), None, None, None, 1);
        status.conflicts.push(ConflictEntry {
            path: path.into(),
            laptop_kind: "file".into(),
            server_kind: "file".into(),
        });
        status_file::write_status(&dir, &status).unwrap();
        for verb in ["status", "conflicts"] {
            let output = Command::new(shell_binary())
                .args(["host", "sync", verb, "--json"])
                .current_dir(&root)
                .env("HOME", &root)
                .env("XDG_STATE_HOME", &state)
                .env("EITRI_CONFIG_DIR", &root)
                .env_remove("DISPLAY")
                .env_remove("WAYLAND_DISPLAY")
                .env_remove("DBUS_SESSION_BUS_ADDRESS")
                .output()
                .unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            // A person may run --json in a terminal: no control or bidirectional character arrives raw.
            let text = String::from_utf8(output.stdout.clone()).unwrap();
            let body = text.strip_suffix('\n').unwrap();
            assert!(
                !body.chars().any(eitri_core::agent_bridge::is_control_like),
                "{verb}: {body:?}"
            );
            let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            let conflicts = if verb == "status" {
                &document["conflicts"]
            } else {
                &document
            };
            assert_eq!(conflicts[0]["path"], path);
        }
    }

    #[test]
    fn the_early_arm_reads_no_init_lua() {
        let root = agent::state_dirs::test_workspace_dir("transport-config");
        std::fs::write(root.join("init.lua"), "io.stderr:write('INIT_LUA_TRIPWIRE')").unwrap();
        let output = Command::new(shell_binary())
            .args([
                "host",
                "mutagen-transport",
                "ssh",
                "--",
                "eitri-0000000000000000",
                "cmd",
            ])
            .env("EITRI_CONFIG_DIR", &root)
            .env("XDG_STATE_HOME", &root)
            .env("HOME", &root)
            .env(
                "PATH",
                std::env::join_paths([root.join("eitri/mutagen/transport"), root.join("empty-bin")]).unwrap(),
            )
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("DBUS_SYSTEM_BUS_ADDRESS")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(255), "{stderr}");
        assert!(stderr.contains("no remote project records a session"), "{stderr}");
        assert!(!stderr.contains("INIT_LUA_TRIPWIRE"), "{stderr}");
    }

    fn shell_binary() -> PathBuf {
        if let Some(path) = std::env::var_os("EITRI_TEST_SHELL") {
            return path.into();
        }
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target"));
        let path = target.join("debug/shell");
        assert!(
            path.is_file(),
            "build shell before testing the early command: cargo build -p shell --bin shell"
        );
        path
    }

    /// Wall time of one run whose standard output starts with `prefix`.
    fn elapsed_until(program: &Path, args: &[&str], scratch: &Path, real_git: &Path, prefix: &[u8]) -> Duration {
        let start = Instant::now();
        let output = Command::new(program)
            .args(args)
            .current_dir(scratch)
            .env("EITRI_REAL_GIT", real_git)
            .env("HOME", scratch)
            .env_remove("EITRI_GIT_SHIM_OPTIONS")
            .env_remove("EITRI_GIT_SHIM_OPTIONS_FILE")
            .env("EITRI_CONFIG_DIR", scratch)
            .env("XDG_STATE_HOME", scratch.join("state"))
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("DBUS_SYSTEM_BUS_ADDRESS")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        if args == ["host", "known-hosts-probe"] {
            assert_eq!(output.status.code(), Some(1));
        } else {
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        }
        assert!(!String::from_utf8_lossy(&output.stderr).contains("INIT_LUA_TRIPWIRE"));
        assert!(!scratch.join("init-ran").exists(), "the git arm ran init.lua");
        assert!(
            output.stdout.starts_with(prefix),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        start.elapsed()
    }

    /// The shim's cost is the product binary's own start (its dynamic libraries) plus the git it runs: the arm itself
    /// must add no work of its own, such as the Lua kernel or the toolkit. Measured against the binary's earliest arm
    /// and a calibration run, never against an absolute bound; the added cost over plain git is reported.
    #[test]
    fn shim_overhead_against_plain_git() {
        let binary = eitri_core::remote_sync::shim_dir::shim_executable(&shell_binary());
        let real_git = eitri_core::remote_sync::shim_dir::locate_real_git(&std::env::var_os("PATH").unwrap()).unwrap();
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let scratch_guard = Scratch(agent::state_dirs::test_workspace_dir("git-shim-overhead"));
        let scratch = scratch_guard.0.clone();
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("init.lua"), format!("io.stderr:write('INIT_LUA_TRIPWIRE\\n'); local f=assert(io.open({:?}, 'w')); f:write('ran'); f:close()", scratch.join("init-ran").to_str().unwrap())).unwrap();
        let init = Command::new(&real_git)
            .args(["init", "-q", "-b", "main"])
            .current_dir(&scratch)
            .output()
            .unwrap();
        assert!(init.status.success());
        for (key, value) in [("gc.auto", "0"), ("maintenance.auto", "false")] {
            assert!(Command::new(&real_git)
                .args(["config", key, value])
                .current_dir(&scratch)
                .status()
                .unwrap()
                .success());
        }
        let earliest = || elapsed_until(&binary, &["host", "known-hosts-probe"], &scratch, &real_git, b"");
        let mut calibration: Vec<_> = (0..20).map(|_| earliest()).collect();
        calibration.sort();
        let spread = calibration[19] - calibration[0];
        for recorded in [false, true] {
            if recorded {
                use eitri_core::remote_project::{RemoteProject, RemoteProjects, SyncRecord};
                let mut project: RemoteProject = serde_json::from_value(serde_json::json!({
                    "version":1,"host":"example-host","host_key_sha256":"SHA256:example","root":scratch,"helper_version":"0.4.0","created_at":0
                })).unwrap();
                let record: SyncRecord = serde_json::from_value(serde_json::json!({"engine":"mutagen","host_key_sha256":"SHA256:example","tree_session":"recorded-tree","git_session":"recorded-git","git_ready":true})).unwrap();
                project.set_sync_record(&record);
                RemoteProjects::new(Some(scratch.join("state").as_os_str()), None)
                    .save(&project)
                    .unwrap();
            }
            let (mut plain, mut start, mut shim) = (vec![], vec![], vec![]);
            for _ in 0..20 {
                plain.push(elapsed_until(
                    &real_git,
                    &["rev-parse", "--is-inside-work-tree"],
                    &scratch,
                    &real_git,
                    b"true",
                ));
                start.push(earliest());
                shim.push(elapsed_until(
                    &binary,
                    &["host", "git", "--", "rev-parse", "--is-inside-work-tree"],
                    &scratch,
                    &real_git,
                    b"true",
                ));
            }
            plain.sort();
            start.sort();
            shim.sort();
            eprintln!("git shim recorded={recorded} p50: {:?}; earliest arm p50: {:?}; plain git p50: {:?}; added p50: {:?}; calibration spread: {:?}", shim[10], start[10], plain[10], shim[10].saturating_sub(plain[10]), spread);
            assert!(
                shim[10] <= start[10] + plain[10] * 2 + spread,
                "the git arm does work beyond binary start and Git"
            );
        }
    }

    #[test]
    fn send_waits_for_flush_false_skips_it() {
        let project = eitri_core::remote_project::RemoteProject {
            version: 1,
            host: "example-host".into(),
            host_key_sha256: "key".into(),
            root: "/srv/project".into(),
            helper_version: "test".into(),
            sync: None,
            created_at: 0,
            queued_stops: vec![],
        };
        let mut config = eitri_core::lua::config::ConfigStore::default();
        for (setting, expected) in [("false", false), ("true", true)] {
            config.set_text("remote_dev.send_waits_for_flush", setting);
            let (settings, problems) = sync_settings(&config);
            assert!(problems.is_empty());
            assert_eq!(
                crate::remote_window::send_port(&settings, &project, false, Err("offline".into())).is_some(),
                expected
            );
        }
    }

    #[test]
    fn bad_shim_settings_keep_the_shim_with_defaults_and_visible_problems() {
        let mut config = eitri_core::lua::config::ConfigStore::default();
        config.set_text("remote_dev.enabled", "true");
        config.set_text("remote_dev.flush_cap", "broken");
        config.set_text("remote_dev.mutagen", "relative/path");
        let (options, problems) = git_shim_options(&config);
        assert!(options.enabled);
        assert_eq!(options.flush_cap, 3.0);
        assert_eq!(options.mutagen, None);
        assert_eq!(problems.len(), 2);
        assert!(problems[0].contains("remote_dev.flush_cap"));
        assert!(problems[1].contains("remote_dev.mutagen"));
        let source = eitri_core::source_scan::read_required("shell/src/main.rs");
        let body: String = eitri_core::source_scan::functions(&source.text, "build_ui")[0]
            .split_whitespace()
            .collect();
        assert!(body.contains("lua_engine.report_problem(problem)"));
        assert!(body.contains("GIT_SHIM_PROBLEM.with"));
        let main: String = eitri_core::source_scan::functions(&source.text, "main")[0]
            .split_whitespace()
            .collect();
        assert!(main.contains("Err(error)=>GIT_SHIM_PROBLEM.with"));
        assert!(body.contains("host_command::git_shim_options(&lua_engine.config.borrow())"));
        let source = eitri_core::source_scan::read_required("shell/src/host_command.rs");
        let prepare = eitri_core::source_scan::functions(&source.text, "prepare_git_shim")[0];
        assert!(!prepare.contains("Kernel::") && !prepare.contains("load_init_file"));
    }

    #[test]
    fn unrelated_unreadable_record_and_deleted_cwd_do_not_break_plain_git() {
        use eitri_core::remote_project::{project_key, RemoteProjects};
        use std::os::unix::process::CommandExt;
        let root = agent::state_dirs::test_workspace_dir("git-shim-deleted-cwd");
        std::fs::create_dir_all(&root).unwrap();
        let state = root.join("state");
        let store = RemoteProjects::new(Some(state.as_os_str()), None);
        // Save a valid sibling first to prepare the private records directory.
        let project: eitri_core::remote_project::RemoteProject = serde_json::from_value(serde_json::json!({"version":1,"host":"example-host","host_key_sha256":"SHA256:example","root":root.join("other"),"helper_version":"0.4.0","created_at":0})).unwrap();
        store.save(&project).unwrap();
        let records = state.join("eitri/hosts");
        let record = records.join(format!("{}.json", project_key(&project.root)));
        assert!(record.exists());
        std::fs::write(&record, "broken").unwrap();
        let git = eitri_core::remote_sync::shim_dir::locate_real_git(&std::env::var_os("PATH").unwrap()).unwrap();
        let expected = Command::new(&git).arg("--exec-path").output().unwrap();
        assert!(expected.status.success());
        for with_pwd in [true, false] {
            let cwd = root.join(if with_pwd { "deleted-with-pwd" } else { "deleted" });
            std::fs::create_dir(&cwd).unwrap();
            let mut command = Command::new(shell_binary());
            command
                .args(["host", "git", "--", "--exec-path"])
                .current_dir(&cwd)
                .env("EITRI_REAL_GIT", &git)
                .env("XDG_STATE_HOME", &state)
                .env_remove("EITRI_GIT_SHIM_OPTIONS_FILE")
                .env_remove("EITRI_GIT_SHIM_OPTIONS");
            if with_pwd {
                command.env("PWD", &cwd);
            } else {
                command.env_remove("PWD");
            }
            unsafe {
                command.pre_exec(move || std::fs::remove_dir(&cwd));
            }
            let output = command.output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            assert_eq!(output.stdout, expected.stdout);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn host_git_routes_before_reading_settings_and_defaults_once_for_forwards() {
        use eitri_core::remote_project::RemoteProjects;
        use eitri_core::remote_sync::shim_dir::ShimOptions;
        let root = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| shell_binary().parent().unwrap().parent().unwrap().join("tmp"))
            .join(format!("git-shim-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("init.lua"), "io.stderr:write('INIT_LUA_TRIPWIRE')").unwrap();
        let path = root.join("options.json");
        let git = eitri_core::remote_sync::shim_dir::locate_real_git(&std::env::var_os("PATH").unwrap()).unwrap();
        let project_root = root.join("project");
        std::fs::create_dir(&project_root).unwrap();
        for args in [
            &["init", "-b", "main"][..],
            &["config", "gc.auto", "0"],
            &["config", "maintenance.auto", "false"],
        ] {
            assert!(Command::new(&git)
                .args(args)
                .current_dir(&project_root)
                .output()
                .unwrap()
                .status
                .success());
        }
        let state = root.join("state");
        let store = RemoteProjects::new(Some(state.as_os_str()), None);
        let project = serde_json::from_value(serde_json::json!({
            "version": 1, "host": "example-host", "host_key_sha256": "SHA256:example",
            "root": project_root, "helper_version": "0.4.0", "created_at": 0,
            "sync": {"engine": "mutagen", "host_key_sha256": "SHA256:example",
                "tree_session": "recorded-tree", "git_session": "recorded-git", "git_ready": true}
        }))
        .unwrap();
        store.save(&project).unwrap();
        let run = |cwd: &Path, args: &[&str]| {
            Command::new(shell_binary())
                .args(["host", "git", "--"])
                .args(args)
                .current_dir(cwd)
                .env("XDG_STATE_HOME", &state)
                .env("EITRI_CONFIG_DIR", &root)
                .env("EITRI_REAL_GIT", &git)
                .env("EITRI_GIT_SHIM_OPTIONS", "broken")
                .env("EITRI_GIT_SHIM_OPTIONS_FILE", &path)
                .env_remove("DISPLAY")
                .env_remove("WAYLAND_DISPLAY")
                .env_remove("DBUS_SESSION_BUS_ADDRESS")
                .env_remove("DBUS_SYSTEM_BUS_ADDRESS")
                .output()
                .unwrap()
        };
        let defaults = serde_json::to_vec(&ShimOptions::default()).unwrap();
        for snapshot in [Some(defaults.clone()), Some(b"broken".to_vec()), None] {
            let needs_defaults = snapshot.as_deref() != Some(defaults.as_slice());
            if let Some(bytes) = &snapshot {
                agent::private_fs::write_private(&path, bytes).unwrap();
            } else {
                std::fs::remove_file(&path).unwrap();
            }
            for (cwd, args) in [
                (root.as_path(), &["--version"][..]),
                (project_root.as_path(), &["status", "--porcelain"][..]),
            ] {
                let output = run(cwd, args);
                assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
                assert!(output.stderr.is_empty(), "{}", String::from_utf8_lossy(&output.stderr));
            }
            let output = run(&project_root, &["add", "file"]);
            assert_eq!(output.status.code(), Some(1));
            let err = String::from_utf8_lossy(&output.stderr);
            assert!(err.contains("remote_dev.enabled"), "{err}");
            assert_eq!(
                err.matches("using default git shim settings").count(),
                usize::from(needs_defaults)
            );
            assert!(!err.contains("INIT_LUA_TRIPWIRE"));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn host_command_git_arm_returns_before_config_or_gtk() {
        let source = eitri_core::source_scan::read_required("shell/src/host_command.rs");
        let body: String = eitri_core::source_scan::functions(&source.text, "run")[0]
            .split_whitespace()
            .collect();
        let git = body.find("first==\"git\"").unwrap();
        let config = body.find("letenabled=remote_dev_enabled").unwrap();
        assert!(git < config);
        let arm = &body[git..body.find("cli::KNOWN_HOSTS_PROBE").unwrap()];
        assert!(arm.contains("returnSome(code(status))"));
        assert!(!arm.contains("exec_real_git"), "every Git form must reach routing");
        assert!(arm.contains("ShimEnv::from_process()") && arm.contains("git_shim::run("));
        let source = eitri_core::source_scan::read_required("core/src/remote_sync/git_shim/mod.rs");
        for name in [
            "from_process",
            "run",
            "run_with",
            "select_project",
            "repository_targets",
            "moving_environment",
            "unreadable_record",
        ] {
            let body = eitri_core::source_scan::functions(&source.text, name)[0];
            assert!(
                !body.contains("Kernel::") && !body.contains("init.lua") && !body.contains("gtk4::"),
                "{name}"
            );
        }
        assert!(!arm.contains("Kernel::") && !arm.contains("init.lua") && !arm.contains("gtk4::"));
    }
}
