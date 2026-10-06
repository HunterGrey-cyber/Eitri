//! `eitri panel [--nvim <addr>] [DIR]`: a window holding only the agent panel, for use beside the
//! user's own nvim (companion mode). It owns no editor, so nothing here saves or restores an
//! arrangement: the layout is one agent leaf, built fresh every time.
//!
//! The command line is parsed here, before GTK, and the project is resolved from what is left, so
//! `panel` is never read by the ordinary resolver as a directory (see `main`).
//!
//! `eitri split [DIR]` is the same window with an editor of its own: it starts upstream Neovide on a
//! private nvim socket, attaches to that, and closes when that Neovide exits ([`run_split`]).

mod close_watch;
mod gnome_shell;
mod link;
mod requests;
mod window;
mod wm_runner;

use std::cell::Cell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::Application;

use eitri_core::agent_backend::BackendKind;
use eitri_core::companion::env::{inherited_editor_env, scrub, split_scrub_names, INHERITED_FROM_EDITOR};
use eitri_core::panel_control::{self, Claim, CloseWith, ControlServer, Reply, Request};

/// The panel is its own application, so it never collides with a full window's id.
pub(crate) const PANEL_APP_ID: &str = "cn.huntergrey.eitri.Panel";

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
    /// The editor this window is to close with, when `eitri split` started one: its identity, and
    /// the `Child` to collect once it exits. Taken by `build`, like `control`.
    pub(crate) close_with: Cell<Option<(CloseWith, Option<Child>)>>,
    pub(crate) backend_kind: BackendKind,
    /// Why this build shows no web panel, where WebKit's sandbox cannot start.
    pub(crate) panel_notice: Option<String>,
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
    let mut names = INHERITED_FROM_EDITOR.to_vec();
    for name in inherited_editor_env(&|name| std::env::var_os(name)) {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    scrub(&names, &|name| {
        // SAFETY: only this thread exists. GTK, the panel and the sidecar have not started, and the
        // two calls above spawned nothing, so no other thread can be reading the environment.
        unsafe { std::env::remove_var(name) }
    });
    let project_root = match eitri_core::project_root::resolve_args(&parsed.rest) {
        Ok(root) => root,
        Err(message) => return fail(&message),
    };
    println!("eitri panel: project root {}", project_root.display());
    let request = parsed.nvim.as_ref().map(|addr| Request::Attach {
        addr: addr.clone(),
        close_with: None,
    });
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
    run_window(project_root, parsed.nvim, from_editor, server, None)
}

/// How long `eitri split` waits for the editor it started to listen.
const EDITOR_LISTENS_WITHIN: Duration = Duration::from_secs(15);

/// What `eitri split` prints and exits with for a failure. Its own parser and the core half already
/// name the command; everything else gets the name here.
fn split_failure_line(message: &str) -> String {
    if message.starts_with("eitri split:") {
        message.to_owned()
    } else {
        format!("eitri split: {message}")
    }
}

fn fail_split(message: &str) -> glib::ExitCode {
    eprintln!("{}", split_failure_line(message));
    glib::ExitCode::FAILURE
}

/// What a split prints once the running panel for the project took its attach.
fn split_forwarded_message(project_root: &Path) -> String {
    format!(
        "eitri split: the running panel for {} attached to this Neovide",
        project_root.display()
    )
}

/// What a split says when the running panel refused its attach. A panel that predates `close_with`
/// answers a request of protocol 2 with "unsupported protocol version"; that one is told as what it
/// is, since the way out is to close the old panel.
fn split_refused_message(project_root: &Path, why: &str) -> String {
    if why.starts_with("unsupported protocol version") {
        format!(
            "eitri split: the Eitri panel already open for {} is older than this eitri; close it and run eitri split again",
            project_root.display()
        )
    } else {
        format!("eitri split: the running panel refused: {why}")
    }
}

/// The first executable file called `name` in `path`'s directories.
fn find_in_path(path: &std::ffi::OsStr, name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path).map(|dir| dir.join(name)).find(|candidate| {
        candidate
            .metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
}

/// Runs `eitri split`. `args` is the command line after `split`. Like [`run`], everything before the
/// application is built happens on this one thread, with the `NVIM*` variables scrubbed before the
/// editor starts: the split is often run from a shell inside an nvim, and neither this process nor
/// the Neovide it starts may carry that nvim's address.
///
/// The editor is started first and the control socket claimed after it listens, so that the attach
/// the claim forwards (or acts on) names an editor that exists. Every failure after the editor
/// started leaves it running: it is the user's window now.
pub(crate) fn run_split(args: Vec<OsString>) -> glib::ExitCode {
    let parsed = match eitri_core::project_root::parse_split_args(&args) {
        Ok(parsed) => parsed,
        Err(message) => return fail_split(&message),
    };
    // A split started from an nvim `:terminal`, even another Eitri window's, must hand neither that
    // editor's address nor that window's sockets to Neovide, the panel or anything they start.
    let names = split_scrub_names(&|name| std::env::var_os(name));
    scrub(&names, &|name| {
        // SAFETY: only this thread exists. Nothing above spawned one, and the editor, GTK and the
        // panel have not started.
        unsafe { std::env::remove_var(name) }
    });
    let project_root = match eitri_core::project_root::resolve_args(&parsed.rest) {
        Ok(root) => root,
        Err(message) => return fail_split(&message),
    };
    let program = match eitri_core::split::neovide_program(&|name| std::env::var_os(name), &|name| {
        std::env::var_os("PATH").and_then(|path| find_in_path(&path, name))
    }) {
        Ok(program) => program,
        Err(message) => return fail_split(&message),
    };
    println!("eitri split: project root {}", project_root.display());
    let control_dir = panel_control::control_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), &std::env::temp_dir());
    if let Err(message) = panel_control::ensure_dir(&control_dir) {
        return fail_split(&message);
    }
    let sock = match eitri_core::split::fresh_socket_path(
        &control_dir,
        &project_root,
        std::process::id(),
        &mut eitri_core::split::random_nonce,
    ) {
        Ok(sock) => sock,
        Err(message) => return fail_split(&message),
    };
    let mut child = match eitri_core::split::neovide_command(&program, &sock, &project_root).spawn() {
        Ok(child) => child,
        Err(e) => return fail_split(&format!("could not start {}: {e}", program.display())),
    };
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if let Err(message) = eitri_core::split::wait_for_socket(&sock, &mut child, euid, EDITOR_LISTENS_WITHIN) {
        return fail_split(&message);
    }
    let Some(start) = eitri_core::split::proc_start_time(child.id()) else {
        return fail_split(&format!(
            "could not read when neovide (pid {}) started, so the panel cannot close with it",
            child.id()
        ));
    };
    let close_with = CloseWith { pid: child.id(), start };
    let request = Request::Attach {
        addr: sock.clone(),
        close_with: Some(close_with),
    };
    // One panel per project: when one is running, it takes this editor and this process is done.
    match panel_control::claim(&control_dir, &project_root, Some(&request)) {
        Ok(Claim::Ours(server)) => run_window(project_root, Some(sock), false, server, Some((close_with, Some(child)))),
        Ok(Claim::Forwarded(Reply::Ok)) => {
            println!("{}", split_forwarded_message(&project_root));
            glib::ExitCode::SUCCESS
        }
        Ok(Claim::Forwarded(Reply::Refused(why))) => {
            eprintln!("{}", split_refused_message(&project_root, &why));
            glib::ExitCode::FAILURE
        }
        Ok(Claim::Unresponsive { pid }) => fail_split(&unresponsive_message(pid)),
        Err(message) => fail_split(&message),
    }
}

/// The part of [`run`] and [`run_split`] after the control socket is theirs: the backend and WebKit
/// decisions, then the application.
fn run_window(
    project_root: PathBuf,
    nvim: Option<PathBuf>,
    from_editor: bool,
    server: ControlServer,
    close_with: Option<(CloseWith, Option<Child>)>,
) -> glib::ExitCode {
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
        nvim,
        from_editor,
        control: Cell::new(Some(server)),
        close_with: Cell::new(close_with),
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

    #[test]
    fn a_split_failure_always_names_the_split() {
        assert_eq!(
            split_failure_line("eitri split: unknown option -x"),
            "eitri split: unknown option -x"
        );
        assert_eq!(
            split_failure_line("a panel for this project is running but not answering"),
            "eitri split: a panel for this project is running but not answering"
        );
    }

    #[test]
    fn a_split_that_was_forwarded_says_the_running_panel_took_this_neovide() {
        assert_eq!(
            split_forwarded_message(Path::new("/work/proj")),
            "eitri split: the running panel for /work/proj attached to this Neovide"
        );
    }

    #[test]
    fn an_older_panel_is_told_as_an_older_panel_and_any_other_refusal_as_it_was_said() {
        assert_eq!(
            split_refused_message(Path::new("/work/proj"), "unsupported protocol version 2"),
            "eitri split: the Eitri panel already open for /work/proj is older than this eitri; \
             close it and run eitri split again"
        );
        assert_eq!(
            split_refused_message(Path::new("/work/proj"), "close_with is not the sender's child"),
            "eitri split: the running panel refused: close_with is not the sender's child"
        );
    }

    #[test]
    fn the_path_search_finds_the_first_executable_file_and_skips_the_rest() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("find-in-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (first, second) = (root.join("one"), root.join("two"));
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        // Present but not executable in the first directory, a directory of that name in the second.
        std::fs::write(first.join("tool"), "").unwrap();
        std::fs::create_dir(second.join("tool")).unwrap();
        let third = root.join("three");
        std::fs::create_dir_all(&third).unwrap();
        std::fs::write(third.join("tool"), "").unwrap();
        std::fs::set_permissions(third.join("tool"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([&first, &second, &third]).unwrap();
        assert_eq!(find_in_path(&path, "tool"), Some(third.join("tool")));
        assert_eq!(find_in_path(&path, "absent"), None);
        let _ = std::fs::remove_dir_all(&root);
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
