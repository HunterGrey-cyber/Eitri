//! `eitri host ...`, before anything of a window: no toolkit, no display, no editor environment. ssh runs
//! `eitri host known-hosts-probe` as the masters' `KnownHostsCommand` on every connection, so that one goes first and
//! does nothing but record the key; the other commands are `eitri_core::remote_host::cli`'s, over ssh and the
//! person's terminal.

use std::ffi::OsString;
use std::io::{BufRead, Write};
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

/// The terminal `eitri host` runs in: lines to stdout, questions answered on stdin.
struct Stdio;

impl cli::Terminal for Stdio {
    fn say(&mut self, line: &str) {
        println!("{line}");
    }

    fn ask(&mut self, question: &str) -> bool {
        print!("{question} ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        if std::io::stdin().lock().read_line(&mut answer).is_err() {
            return false;
        }
        matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    }
}
