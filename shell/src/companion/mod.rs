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
use std::ffi::{OsStr, OsString};
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
/// address) and then removes all five, with everything the full window removes, before any thread
/// exists.
pub(crate) const INHERITED_FROM_EDITOR: [&str; 5] = ["NVIM", "NVIM_LISTEN_ADDRESS", "MYVIMRC", "VIMRUNTIME", "VIM"];

/// The two variables that carry an nvim's RPC address to the programs it starts. The full window
/// removes these (see [`drop_inherited_editor_env`]) but not `VIMRUNTIME`, `VIM` or `MYVIMRC`, which
/// may be the user's own exports, meant for the embedded nvim too.
pub(crate) const EDITOR_RPC_ADDRESS: [&str; 2] = ["NVIM", "NVIM_LISTEN_ADDRESS"];

/// What a window sets on its own embedded nvim child alone: the paths of that window's pane-switch,
/// theme, keys and editor-context sockets and of the Lua each loads. No window reads them from its own
/// environment; it builds them per child. So when they are present in this process, they were
/// inherited from another window's nvim (this one was started from its `:terminal`), and every child
/// of this window -- the sidecar and each agent tool, the bottom shell -- could write fake pane
/// switches, key reports or editor context into that other window's sockets, and this window's own
/// nvim would run the other window's Lua wherever this one has none of its own to set.
pub(crate) const SET_ON_THE_NVIM_CHILD: [&str; 9] = [
    "EITRI_PANE_SWITCH_SOCKET",
    "EITRI_NAV_LUA",
    "EITRI_THEME_SOCKET",
    "EITRI_THEME_LUA",
    "EITRI_KEYS_SOCKET",
    "EITRI_KEYS_LUA",
    "EITRI_EDITOR_SOCKET",
    "EITRI_EDITOR_LUA",
    "EITRI_SCRATCH_LUA",
];

/// The `TMUX`/`TMUX_PANE` pair a window's nvim child gets for the pane-switch shim, recognised by
/// `TMUX`'s socket field (tmux's own `<socket>,<pid>,<session>` shape, the socket possibly holding a
/// comma) naming the inherited pane-switch socket. A real tmux's `TMUX` names its own socket, never
/// that one, so it is kept.
fn is_the_shims_tmux(tmux: Option<&OsStr>, pane_switch_socket: Option<&OsStr>) -> bool {
    let (Some(tmux), Some(socket)) = (tmux.and_then(OsStr::to_str), pane_switch_socket) else {
        return false;
    };
    !socket.is_empty()
        && tmux
            .rsplitn(3, ',')
            .nth(2)
            .is_some_and(|field| OsStr::new(field) == socket)
}

/// Every inherited editor variable [`drop_inherited_editor_env`] removes, given how this process's
/// environment reads (`get`). Decided before anything is removed: the shim's `TMUX` is recognised by
/// the pane-switch socket that is itself on the list.
fn inherited_editor_env(get: &dyn Fn(&str) -> Option<OsString>) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = EDITOR_RPC_ADDRESS
        .iter()
        .chain(&SET_ON_THE_NVIM_CHILD)
        .copied()
        .collect();
    if is_the_shims_tmux(get("TMUX").as_deref(), get("EITRI_PANE_SWITCH_SOCKET").as_deref()) {
        names.extend(["TMUX", "TMUX_PANE"]);
    }
    names
}

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

/// Calls `remove` once for every name in `names`. Separate from the `remove_var` it is given, so a
/// test can see every name reach it.
fn scrub(names: &[&str], remove: &dyn Fn(&str)) {
    for name in names {
        remove(name);
    }
}

/// For the full window: an Eitri started from an nvim `:terminal` inherits that editor's RPC address,
/// and every child -- the sidecar and each agent tool under it, the bottom terminal's shell, the
/// embedded nvim -- would inherit it in turn and could drive the outer editor over RPC. When that
/// nvim is another window's, it also carries that window's socket variables
/// ([`SET_ON_THE_NVIM_CHILD`]) and the shim's `TMUX`. The full window reads none of them, so they
/// are removed from this process. `main` calls this before any thread or child exists.
pub(crate) fn drop_inherited_editor_env() {
    let names = inherited_editor_env(&|name| std::env::var_os(name));
    scrub(&names, &|name| {
        // SAFETY: `main` calls this on its only thread, before GTK, the backend check, the nvim probe or
        // any child has started, so nothing else can be reading the environment.
        unsafe { std::env::remove_var(name) }
    });
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
    fn editor_rpc_address_is_nvims_and_inside_the_companion_list() {
        assert!(EDITOR_RPC_ADDRESS.contains(&"NVIM"));
        assert!(EDITOR_RPC_ADDRESS.contains(&"NVIM_LISTEN_ADDRESS"));
        for name in EDITOR_RPC_ADDRESS {
            assert!(INHERITED_FROM_EDITOR.contains(&name), "{name}");
        }
    }

    /// Every socket or Lua path a window hands its nvim child is one an inner window must drop. Read
    /// off `eitri-core`'s sources, every string literal naming an `EITRI_..._SOCKET` or
    /// `EITRI_..._LUA` variable, so a new feed cannot be added without joining the list.
    #[test]
    fn every_nvim_child_socket_variable_is_dropped() {
        fn scan(dir: &std::path::Path, found: &mut std::collections::BTreeSet<String>) {
            for entry in std::fs::read_dir(dir).expect("a readable source dir") {
                let path = entry.expect("a readable entry").path();
                if path.is_dir() {
                    scan(&path, found);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("a readable source file");
                for (at, _) in text.match_indices(concat!("\"", "EITRI_")) {
                    let rest = &text[at + 1..];
                    let name: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_uppercase() || *c == '_')
                        .collect();
                    if rest[name.len()..].starts_with('"') && (name.ends_with("_SOCKET") || name.ends_with("_LUA")) {
                        found.insert(name);
                    }
                }
            }
        }
        let core_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/src");
        let mut found = std::collections::BTreeSet::new();
        scan(&core_src, &mut found);
        assert!(found.len() >= 9, "{found:?}");
        for name in &found {
            assert!(SET_ON_THE_NVIM_CHILD.contains(&name.as_str()), "{name} is not dropped");
        }
    }

    #[test]
    fn an_inner_window_drops_the_outer_windows_nvim_variables() {
        let socket = "/tmp/eitri-ps-1,2/s.sock";
        let outer = |name: &str| -> Option<OsString> {
            match name {
                "TMUX" => Some(format!("{socket},4242,0").into()),
                "EITRI_PANE_SWITCH_SOCKET" => Some(socket.into()),
                _ => None,
            }
        };
        let names = inherited_editor_env(&outer);
        for name in EDITOR_RPC_ADDRESS.iter().chain(&SET_ON_THE_NVIM_CHILD) {
            assert!(names.contains(name), "{name}");
        }
        assert!(names.contains(&"TMUX") && names.contains(&"TMUX_PANE"), "{names:?}");
        for kept in ["VIMRUNTIME", "VIM", "MYVIMRC", "PATH"] {
            assert!(!names.contains(&kept), "{kept}");
        }
    }

    /// A real tmux's `TMUX` stays: only the shim's names the pane-switch socket.
    #[test]
    fn a_real_tmux_is_kept() {
        let real = |name: &str| -> Option<OsString> {
            match name {
                "TMUX" => Some("/tmp/tmux-1000/default,1234,0".into()),
                "EITRI_PANE_SWITCH_SOCKET" => Some("/tmp/eitri-ps-1/s.sock".into()),
                _ => None,
            }
        };
        assert!(!inherited_editor_env(&real).contains(&"TMUX"));
        let only_tmux =
            |name: &str| -> Option<OsString> { (name == "TMUX").then(|| "/tmp/tmux-1000/default,1234,0".into()) };
        assert!(!inherited_editor_env(&only_tmux).contains(&"TMUX"));
        assert!(!is_the_shims_tmux(
            Some(OsStr::new("s.sock")),
            Some(OsStr::new("s.sock"))
        ));
        assert!(!is_the_shims_tmux(Some(OsStr::new(",1,0")), Some(OsStr::new(""))));
    }

    #[test]
    fn editor_env_is_scrubbed_before_any_thread() {
        for names in [&INHERITED_FROM_EDITOR[..], &EDITOR_RPC_ADDRESS[..]] {
            let seen = RefCell::new(Vec::new());
            scrub(names, &|name| seen.borrow_mut().push(name.to_string()));
            assert_eq!(seen.into_inner(), names.to_vec());
        }
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
