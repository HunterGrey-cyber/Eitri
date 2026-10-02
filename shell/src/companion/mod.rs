//! `eitri panel [--nvim <addr>] [DIR]`: a window holding only the agent panel, for use beside the
//! user's own nvim (companion mode). It owns no editor, so nothing here saves or restores an
//! arrangement: the layout is one agent leaf, built fresh every time.
//!
//! The command line is parsed here, before GTK, and the project is resolved from what is left, so
//! `panel` is never read by the ordinary resolver as a directory (see `main`).

mod link;
pub(crate) mod prefix;
mod window;
mod wm_runner;

use std::cell::Cell;
use std::ffi::OsString;
use std::path::PathBuf;

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::Application;

use eitri_core::agent_backend::BackendKind;
use eitri_core::panel_control::{self, Claim, ControlServer, Reply, Request};

/// The panel is its own application, so it never collides with a full window's id.
pub(crate) const PANEL_APP_ID: &str = "cn.huntergrey.eitri.Panel";

/// What a panel started by nvim's `jobstart` inherits from that nvim. Left in this process's
/// environment, every sidecar and every agent tool command would inherit `$NVIM` and could drive the
/// user's editor over RPC, which companion mode must not add. [`run`] reads `$NVIM` (the default
/// address) and then removes all five before any thread exists.
pub(crate) const INHERITED_FROM_EDITOR: [&str; 5] = ["NVIM", "NVIM_LISTEN_ADDRESS", "MYVIMRC", "VIMRUNTIME", "VIM"];

/// What one launch decided before GTK started.
pub(crate) struct Start {
    pub(crate) project_root: PathBuf,
    /// The address of the nvim to attach to, from `--nvim` or `$NVIM`.
    pub(crate) nvim: Option<PathBuf>,
    /// Started from inside an nvim (`$NVIM` was set): its standard streams may be that nvim's pipes.
    pub(crate) from_editor: bool,
    /// The per-project control socket this window owns, taken by `build` (which runs from an `Fn`
    /// closure, so it cannot move it out of the `Start`).
    pub(crate) control: Cell<Option<ControlServer>>,
    pub(crate) backend_kind: BackendKind,
    /// Why this build shows no web panel, where WebKit's sandbox cannot start.
    pub(crate) panel_notice: Option<String>,
}

/// Calls `remove` once for every variable in [`INHERITED_FROM_EDITOR`]. Separate from the
/// `remove_var` it is given, so a test can see all five names reach it.
fn scrub(remove: &dyn Fn(&str)) {
    for name in INHERITED_FROM_EDITOR {
        remove(name);
    }
}

fn fail(message: &str) -> glib::ExitCode {
    // `parse_panel_args` names its own command in the one message that is about an option.
    if message.starts_with("eitri panel:") {
        eprintln!("{message}");
    } else {
        eprintln!("eitri panel: {message}");
    }
    glib::ExitCode::FAILURE
}

/// What a second launch prints once the running panel took its request.
fn forwarded(project_root: &std::path::Path, attached: bool) -> glib::ExitCode {
    let what = if attached { "attached" } else { "raised" };
    println!("eitri panel: {what} the running panel for {}", project_root.display());
    glib::ExitCode::SUCCESS
}

/// Moves the panel's standard streams off the pipes of the nvim that started it
/// (`eitri_core::companion::stdio`), saying once where its output goes from now on. That line is the
/// last the starter reads, so a panel that later exits with an error is reported with its log.
pub(crate) fn leave_editor_pipes(project_root: &std::path::Path) {
    use std::io::Write;
    let dir = eitri_core::companion::stdio::log_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    eitri_core::companion::stdio::leave_pipes(dir.as_deref(), project_root, &|log| {
        // Not `eprintln!`: the nvim that started the panel may already be gone, and a failed write
        // must not panic.
        let _ = match log {
            Some(path) => writeln!(
                std::io::stderr(),
                "eitri panel: running; its output goes to {}",
                path.display()
            ),
            None => writeln!(std::io::stderr(), "eitri panel: running; its output is discarded"),
        };
    });
}

/// A panel for this project is running but does not answer. A pid of 0 means it is not known.
fn unresponsive_message(pid: u32) -> String {
    let pid = if pid == 0 {
        String::new()
    } else {
        format!(" (pid {pid})")
    };
    format!("a panel for this project is running but not answering{pid}")
}

/// Runs the panel. `args` is the command line after `panel`. Everything before the application is
/// built happens on this one thread, in this order: the options (reading `$NVIM`), then the scrub,
/// then the project, the backend and the WebKit decision. `parse_panel_args` and `resolve_args`
/// start no thread.
pub(crate) fn run(args: Vec<OsString>) -> glib::ExitCode {
    let nvim_env = std::env::var_os("NVIM");
    let from_editor = nvim_env.is_some();
    let parsed = match eitri_core::project_root::parse_panel_args(&args, nvim_env) {
        Ok(parsed) => parsed,
        Err(message) => return fail(&message),
    };
    scrub(&|name| {
        // SAFETY: only this thread exists. GTK, the panel and the sidecar have not started, and the
        // two calls above spawned nothing, so no other thread can be reading the environment.
        unsafe { std::env::remove_var(name) }
    });
    let project_root = match eitri_core::project_root::resolve_args(&parsed.rest) {
        Ok(root) => root,
        Err(message) => return fail(&message),
    };
    println!("eitri panel: project root {}", project_root.display());
    let request = parsed.nvim.as_ref().map(|addr| Request::Attach(addr.clone()));
    if let Some(addr) = &parsed.nvim {
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        if let Err(message) = panel_control::validate_nvim_addr(addr, unsafe { libc::geteuid() }) {
            return fail(&message);
        }
    }
    // One panel per project: a second launch hands its request to the first and exits.
    let control_dir = panel_control::control_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), &std::env::temp_dir());
    let server = match panel_control::claim(&control_dir, &project_root, request.as_ref()) {
        Ok(Claim::Ours(server)) => server,
        Ok(Claim::Forwarded(Reply::Ok)) => {
            return forwarded(&project_root, request.is_some());
        }
        Ok(Claim::Forwarded(Reply::Refused(why))) => {
            return fail(&format!("the running panel refused: {why}"));
        }
        Ok(Claim::Unresponsive { pid }) => return fail(&unresponsive_message(pid)),
        Err(message) => return fail(&message),
    };
    let backend_kind = match BackendKind::from_env(false) {
        Ok(kind) => kind,
        Err(err) => {
            eprintln!("eitri: {}", err.message);
            return glib::ExitCode::FAILURE;
        }
    };
    let panel_notice = match crate::webkit_sandbox::decision() {
        crate::webkit_sandbox::Decision::Unavailable { reason } => {
            let notice = crate::webkit_sandbox::unavailable_notice(reason);
            eprint!("{notice}");
            Some(notice)
        }
        crate::webkit_sandbox::Decision::Available | crate::webkit_sandbox::Decision::DisabledByUser => None,
    };
    let start = Start {
        project_root,
        nvim: parsed.nvim,
        from_editor,
        control: Cell::new(Some(server)),
        backend_kind,
        panel_notice,
    };
    let app = Application::builder()
        .application_id(PANEL_APP_ID)
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| {
        window::build(app, &start);
    });
    app.run_with_args::<&str>(&[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn editor_env_list_names_nvims_rpc_address() {
        assert!(INHERITED_FROM_EDITOR.contains(&"NVIM"));
        assert!(INHERITED_FROM_EDITOR.contains(&"NVIM_LISTEN_ADDRESS"));
    }

    #[test]
    fn editor_env_is_scrubbed_before_any_thread() {
        let seen = RefCell::new(Vec::new());
        scrub(&|name| seen.borrow_mut().push(name.to_string()));
        assert_eq!(seen.into_inner(), INHERITED_FROM_EDITOR.to_vec());
    }

    /// A companion window shows one fixed arrangement and has no editor to put back, so nothing in
    /// its sources may write or read a saved layout. The names are split so this file does not match
    /// itself; the scan takes line comments and the test module off, as `main`'s scan does.
    #[test]
    fn companion_sources_never_save_a_layout() {
        let saver = ["Layout", "Saver"].concat();
        let persist = ["layout::", "persist"].concat();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/companion");
        let mut scanned = 0;
        for entry in std::fs::read_dir(&dir).expect("shell/src/companion is readable") {
            let path = entry.expect("a readable entry").path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            scanned += 1;
            let text = std::fs::read_to_string(&path).expect("a readable source file");
            let code = text.split("#[cfg(test)]").next().unwrap_or_default();
            let code: String = code
                .lines()
                .map(|line| line.split("//").next().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(
                code.matches(&saver).count(),
                0,
                "{} names the layout saver",
                path.display()
            );
            assert_eq!(
                code.matches(&persist).count(),
                0,
                "{} reaches layout persistence",
                path.display()
            );
        }
        assert!(
            scanned >= 2,
            "the scan found {scanned} source files; the directory moved?"
        );
    }
}
