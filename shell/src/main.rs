//! The shell window combines custom chrome, a module grid driven by layout data,
//! an embedded editor pane, and an agent panel. The agent panel hosts a frontend
//! in a WebView and bridges it to a lazily started `agent::AgentSession`.

mod agent_panel;
mod chat_views;
mod chrome;
mod close_prompt;
mod companion;
mod editor_feeds;
mod editor_quit;
mod editor_rpc;
mod editor_start_failure;
mod end_signals;
#[cfg(feature = "probe")]
mod grid_probe;
mod gtk_host;
mod hint;
mod kill_pane;
mod layout;
mod layout_state;
mod linked_window;
mod lua;
mod module_grid;
mod pane_switch;
mod panel_super;
mod plugin_page;
mod plugin_panels;
mod prefix_strip;
mod process_memory;
mod terminal;
mod text_size;
mod theme;
mod toast;
mod tray;
mod version;
mod web_host;
mod webkit_sandbox;
mod webkit_zoom;
mod wheel_zoom;
mod window_config;
mod window_mode;
// Kept beside `gtk_host` rather than declared inside it: `tests/gtk_host_conformance.rs` compiles
// `gtk_host` on its own, and the wiring calls the window's own controllers, which only this binary has.
#[path = "gtk_host/wire.rs"]
mod wire;
// Beside `gtk_host` for the same reason: the hooks it registers are the agent panel's, which the
// conformance test's build of `gtk_host` does not have, and its tests read the companion window's.
#[path = "gtk_host/panel_wiring.rs"]
mod panel_wiring;
// Beside `gtk_host` for the same reason: the services and the surface constructors act on the window's own
// registry, pages and grid.
#[path = "gtk_host/services.rs"]
mod gtk_host_services;
#[path = "gtk_host/surfaces.rs"]
mod gtk_host_surfaces;
mod xft_dpi;

// The panel's crash guard lives in `eitri-panel`; the alias keeps `crate::webview_crash_guard::` working.
use eitri_panel::webview_crash_guard;

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::gdk::{Key, ModifierType};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, EventControllerKey};

use agent_panel::ViewId;
use eitri_core::attention::{AgentPlace, Attention};
use eitri_core::keymap::keys_return::Opened as KeysOpened;
use eitri_core::keymap::{Action, TabAction};
use eitri_core::layout::rules::{without_keys, At, OpenPoint, RuleSize};
use eitri_core::layout::{
    Direction, KillScope, Layout, LayoutError, ModuleDecl, ModuleId, ModuleKeys, ModuleKind, OpenRule, OpenRules,
    Placement, Reopen, ScratchShow, Workspaces,
};
use eitri_core::plugin::theme::PageTheme;
use eitri_core::tabs::TabVerb;
use eitri_core::wm::chrome::ChromeTitles;
use eitri_core::wm::module::SurfaceDecl;
use eitri_core::wm::start::StartStep;
use eitri_core::wm::toasts::ToastKind;
use eitri_core::wm::verbs::{selection_gate, SelectionGate};
use eitri_core::wm::{
    Ended, HintEvent, HintTarget, HostEvent, KeyStage, Origin, PointerEv, PointerTarget, ShellCall, SurfaceKind,
    TermEvent, Verdict, WinId,
};
use gtk_host::instance_pool::InstancePool;
use lua::{LuaEngine, PanelSlot};
use module_grid::{HostKind, ModuleGrid};
use neovide_editor::NeovideEditorPane;
use wire::{wire_host, WireDeps, WireKind};

const APP_ID: &str = "cn.huntergrey.eitri";

/// How long the top bar's app name shows the prefix indicator's block when a layout verb is refused
///.
const REFUSAL_FLASH: std::time::Duration = std::time::Duration::from_millis(200);

/// Resolve the user's configuration independently of the project's working directory.
fn config_dir() -> PathBuf {
    eitri_core::config_dir::resolve(&|k| std::env::var_os(k)).unwrap_or_else(|err| {
        eprintln!("{err}");
        std::process::exit(1);
    })
}

/// The guard plugin tasks run under: `eitri-run-guard` from beside this executable, which loads neither GTK nor
/// WebKitGTK, or this executable's own `run-guard` when the helper is not there (a build of the `shell` binary
/// alone) or is another build's.
fn task_guard_command() -> eitri_core::plugin::tasks::GuardCommand {
    let helper = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("eitri-run-guard")))
        .filter(|helper| helper.is_file())
        .filter(|helper| helper_is_this_build(helper));
    match helper {
        Some(program) => eitri_core::plugin::tasks::GuardCommand {
            program,
            args: Vec::new(),
            env: Vec::new(),
        },
        None => eitri_core::plugin::tasks::GuardCommand {
            program: "/proc/self/exe".into(),
            args: vec![eitri_core::run_guard::SUBCOMMAND.into()],
            env: Vec::new(),
        },
    }
}

/// Whether `helper` is the guard this executable was built with. A helper left beside it by an earlier build (`cargo
/// run -p shell` builds only this binary) would run this window's tasks under another build's guard. The helper
/// answers in about a millisecond; one that does not within the bound is passed over.
fn helper_is_this_build(helper: &Path) -> bool {
    use std::io::Read;
    let spawned = std::process::Command::new(helper)
        .arg(eitri_core::run_guard::FINGERPRINT_FLAG)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = spawned else {
        return false;
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
    let mut answer = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut answer);
    }
    let same = answer.trim() == eitri_core::run_guard::fingerprint();
    if !same {
        eprintln!(
            "[eitri] {} is another build's task guard; plugin tasks run under this executable's own",
            helper.display()
        );
    }
    same
}

fn main() -> glib::ExitCode {
    // setenv is not thread-safe, so fill in the display before any thread or GTK code runs.
    if let Some(name) = eitri_core::display_env::wayland_display_fallback(|key| std::env::var_os(key), Path::exists) {
        std::env::set_var("WAYLAND_DISPLAY", name);
        eprintln!("eitri: WAYLAND_DISPLAY was unset; using wayland-0 (the default libwayland connects to)");
    }

    // Collected once and consulted through `flag_given` -- never a raw
    // `args_os().any(|a| a == "...")` -- by every early flag scan below (`--version`, `--legacy`,
    // `--clean`). `flag_given` stops at the first `--`, the same end-of-flags rule
    // `eitri_core::project_root::select_root_source` applies to the project-directory argument
    // itself; before this, each scan ran its own ad-hoc `.any()` with no notion of `--` at all, so
    // `shell -- --version` printed the version line instead of opening a directory literally named
    // `--version`, and `shell -- --legacy` was refused on a release build instead of reaching the
    // directory step. `args_os`, not `args`, and skipping argv[0] (never a
    // flag), for the same non-UTF-8-safety reason `project_root` gives for its own `args_os` use.
    let early_args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // `eitri linked-window` is window 2 of a pair, started by window 1 with the link's end inherited: that end is
    // checked and made this process's own before anything else here can open, inherit or hand on a descriptor,
    // and the window's own log replaces the stdout and stderr it was started with. Its project root is on its
    // command line, so the project resolver below never reads these arguments.
    let start = if early_args
        .first()
        .is_some_and(|a| a == eitri_core::window_link::start::SUBCOMMAND)
    {
        match linked_window::second_first_acts(&early_args[1..]) {
            Ok(start) => start,
            Err(message) => {
                eprintln!("eitri: {message}");
                return glib::ExitCode::from(2);
            }
        }
    } else {
        linked_window::WindowStart::Full
    };
    // `eitri run-guard` holds one plugin task's life for the window that holds the task. It takes its inherited
    // descriptors before anything here can open, close or hand one on, and never reaches GTK.
    if early_args
        .first()
        .is_some_and(|a| a == eitri_core::run_guard::SUBCOMMAND)
    {
        eitri_core::run_guard::serve_inherited();
    }
    // `eitri panel` is its own command line: the project resolver below would read `panel` as a directory.
    if early_args.first().is_some_and(|a| a == "panel") {
        return companion::run(early_args[1..].to_vec());
    }
    if early_args.first().is_some_and(|a| a == "split") {
        return companion::run_split(early_args[1..].to_vec());
    }
    // After the panel branch, which reads `$NVIM` as its default address; before anything that starts a
    // thread or a child. An Eitri started from an nvim `:terminal` would otherwise hand that editor's
    // RPC address -- and, if it is another window's nvim, that window's socket variables -- to the
    // agent, its tools and the bottom shell.
    eitri_core::companion::env::drop_inherited_editor_env();
    let has_flag = |flag: &str| eitri_core::project_root::flag_given(early_args.iter().map(OsString::as_os_str), flag);

    // `--version`, before anything else -- including the stdin
    // detach right below, which is otherwise the very first thing this binary does: printing a
    // version line must never touch GTK, a display, or nvim's stdin.
    // `eitri_core::project_root::KNOWN_FLAGS` also lists `--version`, so `shell --version
    // /some/project` is never a startup failure even though `main()` never reaches `resolve()` on
    // this path -- see that module's own tests.
    if has_flag("--version") {
        println!("{}", version::version_line());
        return glib::ExitCode::SUCCESS;
    }

    // Every plugin task this process starts runs under a guard of its own, so a task lives as long as whichever
    // window holds it and can move with its page. After the companion, `split` and `--version`, none of which
    // runs a plugin task: finding the helper spawns it and waits for its answer.
    eitri_core::plugin::tasks::install_guard(task_guard_command());

    // The backend this launch uses, decided here -- before
    // any window exists, and before the stdin detach right below -- so `--legacy` (or
    // `EITRI_AGENT_BACKEND=legacy`) on a build that did not compile the legacy backend in (every
    // release) exits with its reason and touches nothing else: no GTK, no display, no nvim stdin.
    // `--legacy` joins `--clean` in `eitri_core::project_root::KNOWN_FLAGS` for the same reason
    // `--clean` is there -- so a release still names the real reason rather than "unknown option".
    let legacy_flag = has_flag("--legacy");
    let backend_kind = match eitri_core::agent_backend::BackendKind::from_env(legacy_flag) {
        Ok(kind) => kind,
        Err(err) => {
            eprintln!("eitri: {}", err.message);
            return glib::ExitCode::FAILURE;
        }
    };

    // First, before any thread or fd exists: a pipe, socket or file on stdin would be handed to the
    // embedded nvim by the Neovide fork, which reads it as a buffer and, for a pipe or socket whose
    // writer stays open, blocks until EOF with the editor blank (`neovide_editor::stdin`; the GUI
    // pass of 2026-09-23's defect 2). A terminal is left alone.
    match neovide_editor::detach_stdin_from_nvim() {
        Ok(Some(kind)) => eprintln!("[stdin] a {kind} on stdin would reach nvim as a buffer; stdin is /dev/null now"),
        Ok(None) => {}
        Err(e) => eprintln!("[stdin] could not detach stdin from nvim ({e}); a pipe there will block the editor"),
    }

    // Same `--clean` passthrough convenience as `neovide_embed_live`/`shell_composed`: pass
    // `--clean` on this binary's own command line to launch nvim with `--clean` instead of a
    // real embedding host's actual config. Through `has_flag`, like `--version`/`--legacy` above,
    // for the same `--` reason.
    //
    // `args_os`, not `args`, throughout this function: `std::env::args()` panics on an argument
    // that is not valid UTF-8, and `shell <dir>` makes a path a supported argument -- see
    // `eitri_core::project_root`'s own module doc. A flag match is a byte-for-byte comparison
    // either way.
    //
    // Every flag matched here must also appear in `eitri_core::project_root`'s own (private)
    // `KNOWN_FLAGS` list, which is the only place that can tell a flag from a project directory;
    // one missing from that list makes passing it a hard startup failure rather than a silently
    // wrong project root.
    let want_clean = has_flag("--clean");

    // Resolved once, here, and then carried as a value into every pane that needs it -- see
    // `eitri_core::project_root`'s own module doc for why three separate `current_dir()` reads
    // were one process-global too many.
    let project_root = match &start {
        linked_window::WindowStart::Second { root, .. } => root.clone(),
        linked_window::WindowStart::Full => match eitri_core::project_root::resolve() {
            Ok(root) => root,
            Err(message) => {
                eprintln!("eitri: {message}");
                return glib::ExitCode::FAILURE;
            }
        },
    };
    println!("eitri: project root {}", project_root.display());

    // Which nvim the forked Neovide runtime spawns. Resolved and
    // acted on here -- before `build_application`/`app.run_with_args` bring up GTK's own threads,
    // before `build_ui` starts anything else, and while this is still the only thread in the
    // process -- because a non-`Path`/`Inherited` choice can only reach the fork by being set on
    // *this* process's own environment: the pinned fork's `CmdLineSettings` reads `NEOVIM_BIN`
    // through clap's `env = "NEOVIM_BIN"` (`cmd_line.rs:229`) while `LiveHarness::with_options`
    // builds its settings, before any `child_env` map ever reaches a `Command` -- see
    // `eitri_core::nvim_bin`'s own module doc for the full reasoning, including why this is a
    // deliberate, narrow exception to `pane_switch`'s "never mutate this process's environment"
    // rule (unlike `TMUX`/`TMUX_PANE`, leaking `NEOVIM_BIN` to every other child is harmless: only
    // Neovide reads it, and the bottom terminal's shell removes it itself).
    // Window 2 never starts nvim: which one window 1 runs is window 1's, and a missing nvim is no reason for the
    // window without an editor not to open.
    let second = matches!(start, linked_window::WindowStart::Second { .. });
    match if second {
        Ok(None)
    } else {
        eitri_core::nvim_bin::resolve().map(Some)
    } {
        Ok(None) => {}
        Ok(Some(choice)) => {
            println!("{}", choice.describe());
            if let Some(path) = choice.neovim_bin_to_set() {
                // SAFETY: nothing else in this process has spawned a thread that touches the
                // environment. `resolve()`'s own version probe (if it ran one) never calls
                // `.env`/`.env_clear` on the `Command` it spawns, so it never calls `getenv` and
                // cannot race this `setenv` even if, past its own 2s deadline, it is still running
                // in the background (see `eitri_core::nvim_bin::version_of_binary`'s own doc).
                unsafe { std::env::set_var(eitri_core::nvim_bin::NEOVIM_BIN_ENV, path) };
            }
        }
        Err(message) => {
            eprintln!("eitri: {message}");
            return glib::ExitCode::FAILURE;
        }
    }

    // WebKit's sandbox:
    // decided here, once, while this is still the only thread -- `webkit_sandbox`'s module doc says
    // why that matters for the pending option (B) -- and before any `WebView` exists. Where it cannot
    // start (a stock Ubuntu 24.04 desktop with no AppArmor profile for this binary), no `WebView` is
    // built anywhere, and the agent panel's place shows the same text printed here, instead of WebKit
    // aborting the whole process on its first `WebView`.
    let panel_notice: Option<String> = match webkit_sandbox::decision() {
        webkit_sandbox::Decision::Unavailable { reason } => {
            let notice = webkit_sandbox::unavailable_notice(reason);
            eprint!("{notice}");
            Some(notice)
        }
        webkit_sandbox::Decision::Available | webkit_sandbox::Decision::DisabledByUser => None,
    };

    // Before GTK and before any `WebView`: JavaScriptCore reads its options once, when WebKit first
    // starts it (`process_memory`'s doc says why the UI process runs it without the JIT).
    if !process_memory::ui_process_jsc_without_jit() {
        eprintln!("[memory] JavaScriptCore refused useJIT=false; the UI process keeps its JIT");
    }

    // A full window takes the project's pair lock here, before it reads any file of the pair (its saved tabs are
    // read as the window is built); finding it held, the window runs unpaired.
    let start = RefCell::new(Some(start));
    let app = build_application();
    app.connect_activate(move |app| {
        // `activate` comes once for a window that opts out of single-instance behaviour.
        let Some(start) = start.borrow_mut().take() else { return };
        let pair = match start {
            linked_window::WindowStart::Full => {
                let taken = eitri_core::layout::persist::state_dir(
                    std::env::var_os("XDG_STATE_HOME").as_deref(),
                    std::env::var_os("HOME").as_deref(),
                )
                .ok_or_else(|| std::io::Error::other("no state directory"))
                .and_then(|dir| eitri_core::window_link::pair::PairLock::try_take(&dir, &project_root));
                linked_window::spawn::Pair::full(
                    taken,
                    &project_root,
                    backend_kind == eitri_core::agent_backend::BackendKind::Legacy,
                )
            }
            linked_window::WindowStart::Second { sock, token, .. } => {
                match linked_window::spawn::Pair::second(sock, &project_root) {
                    Ok(pair) => {
                        pair.set_activation(token);
                        pair
                    }
                    Err(err) => {
                        eprintln!("eitri: the link to window 1 could not be started: {err}");
                        std::process::exit(2);
                    }
                }
            }
        };
        build_ui(
            app,
            want_clean,
            &project_root,
            backend_kind,
            panel_notice.as_deref(),
            pair,
        )
    });
    app.run_with_args::<&str>(&[])
}

/// `NON_UNIQUE`, deliberately, and this is the one decision in this file that changes what a
/// *second* `shell` launch does.
///
/// gio's default is single-instance-per-application-id: a second `shell` process registers the
/// same id, discovers the first one owns it, relays a remote `activate` and exits -- so `build_ui`
/// runs a second time *inside the first process*. That gives two windows sharing one process cwd,
/// one `PaneSwitch` directory and one set of `app.add_action` names, none of which this window was
/// ever designed to share. `shell` is a per-project window (its own nvim child, its own agent
/// session, its own project root), so two launches must genuinely be two processes.
///
/// `supervisor/src/bin/eitri_supervisor.rs` solves the same collision the opposite way, with an
/// `app.windows().first()` guard that raises the existing window instead. That is right *there*
/// and wrong here: exactly one dashboard is the supervisor's design, whereas a guard in `shell`
/// would make `shell ~/other-project` silently raise the window for the project already open and
/// never open the one that was asked for. Turning the singleton off is the mechanism that matches
/// what this binary is -- `shell` is the first product binary in this workspace to need it for
/// what a real second launch does, rather than to survive a sandbox that already has one running.
fn build_application() -> Application {
    Application::builder()
        .application_id(APP_ID)
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build()
}

fn build_ui(
    app: &Application,
    want_clean: bool,
    project_root: &Path,
    backend_kind: eitri_core::agent_backend::BackendKind,
    panel_notice: Option<&str>,
    pair: Rc<linked_window::spawn::Pair>,
) {
    // Window 2 of a pair has no editor: no nvim is started, so none of the editor's feeds, the pane-switch channel
    // or the scratch directory exists in it, and every use of the editor below is behind its `Option`.
    let owns_editor = pair.owns_editor();
    // Initialize `gtk-xft-dpi` before building any agent or Lua panel WebView,
    // so it is set when the WebView reads it.
    xft_dpi::ensure_xft_dpi();
    // The plugin trace lines a GUI pass reads exist only in a probe build, switched on from its environment before
    // any plugin page is made (a page built with the trace on also writes its console to stdout).
    #[cfg(feature = "probe")]
    if std::env::var_os("EITRI_PLUGIN_TRACE").is_some_and(|v| v == "1") {
        eitri_core::plugin::trace::set_enabled(true);
    }
    // The application id is also the icon's name in the icon theme (`packaging/icons` installs it as
    // `cn.huntergrey.eitri`). On Wayland the compositor takes a window's icon from the desktop entry of
    // the same id (`packaging/cn.huntergrey.eitri.desktop`) and ignores this; an X11 session, and the
    // docks that read the window's own icon name, use it.
    gtk4::Window::set_default_icon_name(APP_ID);
    // Painted with the built-in fallback until the embedded nvim sends its first snapshot.
    let theme_css = theme::gtk_css::ThemeCss::install(&eitri_core::theme::ThemeTokens::fallback());
    // Built before the editor pane for the same reason `pane_switch` is: its env and `--cmd` reach
    // nvim only at spawn. `None` (logged) leaves the window on the fallback colours.
    // One timer for the four feeds below; each is read once its consumer calls `listen_*`.
    let feed_pump = editor_feeds::FeedPump::start();
    let mut theme_feed = owns_editor.then(theme::feed::ThemeFeed::new).flatten();

    // Built before the editor pane, because its `child_env()` has to be handed to the pane's
    // constructor -- those variables reach nvim through `Command::env` at spawn time and cannot
    // be added afterwards. `None` means the feature is simply unavailable (see `pane_switch`'s
    // own doc); the editor is constructed with an empty child env and behaves exactly as before.
    let mut pane_switch = owns_editor.then(pane_switch::open).flatten();
    // wire 1's feed, built here for the same reason the other two are: its `child_env()` and its
    // `--cmd` must be handed to the editor pane's constructor, and neither can be added to a child
    // that is already running.
    let mut context_feed = owns_editor
        .then(eitri_core::editor_context::feed::EditorContextFeed::new)
        .flatten();
    // Build the feed for nvim's `mapleader`, `timeoutlen`, and Normal-mode maps here.
    // Its `child_env()` and `--cmd` must reach the editor pane's constructor;
    // neither can be added to an already running child.
    let mut nvim_keys_feed = owns_editor
        .then(eitri_core::nvim_keys::feed::NvimKeysFeed::new)
        .flatten();
    let mut nvim_child_env = pane_switch.as_ref().map(|ps| ps.child_env()).unwrap_or_default();
    nvim_child_env.extend(theme_feed.as_ref().map(|feed| feed.child_env()).unwrap_or_default());
    nvim_child_env.extend(context_feed.as_ref().map(|feed| feed.child_env()).unwrap_or_default());
    nvim_child_env.extend(nvim_keys_feed.as_ref().map(|feed| feed.child_env()).unwrap_or_default());
    let mut nvim_extra_args = theme_feed.as_ref().map(|feed| feed.nvim_args()).unwrap_or_default();
    nvim_extra_args.extend(context_feed.as_ref().map(|feed| feed.nvim_args()).unwrap_or_default());
    nvim_extra_args.extend(nvim_keys_feed.as_ref().map(|feed| feed.nvim_args()).unwrap_or_default());
    // Load the navigation fallback from the pane-switch channel's directory so
    // `Ctrl+h/j/k/l` can leave the editor without vim-tmux-navigator and from Visual mode.
    // Only add it when the channel exists, since navigation needs its socket.
    nvim_extra_args.extend(pane_switch.as_ref().map(|ps| ps.nvim_args()).unwrap_or_default());
    // wire 3. Unconditional and stateless -- no socket, no directory, nothing to fail at startup --
    // because the only reload trigger a normal Neovim config installs is `FocusGained`, and nothing
    // in this shell ever tells nvim it lost or gained focus. Without this a `git checkout`, a
    // formatter, or an edit made anywhere else never reaches the buffer.
    nvim_extra_args.extend(eitri_core::buffer_reload::nvim_args());
    // The two nvim round trips: env and `--cmd` reach nvim only at spawn.
    // Window 2 has no nvim but keeps a scratch directory too: its panel writes a draft there for window 1's editor
    // to edit, and reads the edit's end back from it.
    let scratch_dir = eitri_core::scratch::ScratchDir::new();
    if owns_editor {
        nvim_child_env.extend(scratch_dir.as_ref().map(|dir| dir.child_env()).unwrap_or_default());
        nvim_extra_args.extend(scratch_dir.as_ref().map(|dir| dir.nvim_args()).unwrap_or_default());
    }
    let scratch_path = scratch_dir.as_ref().map(|dir| dir.path().to_path_buf());

    // What the editor is launched with: the feeds' env and `--cmd`, the project root, `--clean`. The
    // editor phase below launches it with exactly this.
    //
    // `cwd` is set from the same resolved root the agent panel gets below, not left to the nvim
    // child's inherited process cwd: the two panes silently disagreeing about which project is
    // open is a worse failure than either one pointing somewhere unexpected.
    let editor_spawn = eitri_core::wm::EditorSpawn {
        env: nvim_child_env,
        args: nvim_extra_args,
        cwd: Some(project_root.to_path_buf()),
        clean: want_clean,
    };

    // The window controller, behind its driver: the GTK host and the GLib scheduler, both attached to the
    // driver. The window's start is the controller's start plan (`eitri_core::wm::start::start_plan`), walked
    // here in its order: each `driver.start` below is one phase, with the shell's own work for that phase beside
    // it, written out in sequence because each phase's work builds what later phases' closures capture.
    let wm_scheduler = gtk_host::scheduler::GlibScheduler::new();
    let wm_host = gtk_host::GtkHost::new();
    let driver = {
        let mut controller = eitri_core::wm::Controller::new(wm_scheduler.clone());
        // What the controller reads at once: the grid's size and measures and the editor's cell, from the host,
        // which answers as no window allocated yet until it is given the grid and the editor below.
        controller.set_reads(wm_host.clone());
        let driver = eitri_core::wm::Driver::new(controller, wm_host.clone(), wm_scheduler.clone());
        let sink: Rc<dyn eitri_core::wm::HostSink> = driver.clone();
        wm_host.attach(Rc::downgrade(&sink));
        wm_scheduler.attach(Rc::downgrade(&sink));
        driver
    };
    // The feeds are bound above: the controller keeps the launch settings they make until the editor phase.
    driver.start(
        WinId::MAIN,
        StartStep::Feeds {
            spawn: editor_spawn.clone(),
        },
    );
    // The window, created and not shown, so no later op names a window that does not exist; its close requests,
    // focus changes and activation reach the controller from here on.
    wm_host.set_application(app);
    // `window.new` starts the second window through the pair; the link it makes reaches the controller once linked.
    wm_host.set_linked_windows(pair.clone());
    let (wm_role, editor_presence) = pair.wm_spec_role();
    driver.start(
        WinId::MAIN,
        StartStep::Window {
            spec: eitri_core::wm::WindowSpec {
                role: wm_role,
                presence: editor_presence,
                title: eitri_core::wm::titles::window_title(
                    &eitri_core::wm::titles::project_display_name(project_root),
                    None,
                ),
            },
        },
    );
    let window: ApplicationWindow = wm_host
        .window(WinId::MAIN)
        .and_then(|window| window.downcast().ok())
        .expect("the window phase opened the window");
    window.add_css_class("shell-root");
    // The editor, launched with `editor_spawn`; its host joins the grid at the surfaces phase. That host
    // is a small overlay carrying a hidden failure-message label, not the bare `GtkGLArea` -- see
    // `editor_start_failure`'s own doc for why this has to wrap the widget before it is ever handed to the grid.
    //
    // `pane` is an `Rc` because two separate closures need it after this function returns: the agent panel's
    // Ctrl+h handler (to hand focus back) and the window's close handler (to shut nvim down).
    // `NeovideEditorPane` is deliberately not `Clone`.
    let editor_maker = gtk_host_surfaces::EditorMaker::new(Rc::downgrade(&wm_host));
    wm_host.set_surface_maker(editor_maker.clone());
    driver.start(WinId::MAIN, StartStep::Editor);
    // `None` in window 2, whose editor phase makes nothing.
    let made_editor = editor_maker.take();
    assert_eq!(
        made_editor.is_some(),
        owns_editor,
        "the editor phase makes the editor exactly when the window owns one"
    );
    let (pane, editor_host, editor_start_failure_label) = match made_editor {
        Some(gtk_host_surfaces::MadeEditor { pane, host, failure }) => (Some(pane), Some(host), Some(failure)),
        None => (None, None, None),
    };
    if let Some(pane) = &pane {
        wm_host.set_editor(WinId::MAIN, pane.clone());
    }

    let config_dir = config_dir();
    let lua_engine = Rc::new(LuaEngine::new(config_dir.clone()).expect("LuaEngine construction must not fail"));
    // `eitri.window` names this window before `init.lua` runs, so a config both windows run can keep a side
    // effect to one of them.
    if let Err(err) = lua_engine.set_window(pair.pair_role()) {
        eprintln!("[lua] eitri.window could not be set: {err}");
    }
    // What the kernel phase's `init.lua` and the requests phase's `shell:ready` run on.
    wm_host.set_lua(Rc::new(gtk_host_services::LuaKernel {
        engine: lua_engine.clone(),
        init_file: config_dir.join("init.lua"),
    }));

    // A feed that failed to start yields a source that always answers `None`, so the panel needs no
    // branch: turns simply go out as the user typed them, exactly as before wire 1 existed.
    // The editor's services across the pair: window 1 tells window 2 what its editor feeds parse and carries out
    // window 2's editor operations and requests; window 2 takes all of it from the link. Each side is wired to a
    // link when one comes up (the pair's `linked` hook below).
    let editor_repeat: linked_window::editor::Repeat =
        Rc::new(|every: std::time::Duration, step: Box<dyn Fn() -> bool>| {
            glib::timeout_add_local(every, move || {
                if step() {
                    glib::ControlFlow::Continue
                } else {
                    glib::ControlFlow::Break
                }
            });
        });
    let window1_editor = owns_editor
        .then(|| linked_window::editor::Window1Editor::new(project_root, scratch_path.clone(), editor_repeat.clone()));
    let window2_editor = (!owns_editor)
        .then(|| linked_window::editor::Window2Editor::new(project_root, scratch_path.clone(), editor_repeat.clone()));
    let editor_context_source = match (context_feed.as_mut(), &window2_editor) {
        (Some(feed), _) => feed_pump.listen_context(feed, scratch_path.clone()).0,
        // Window 1's editor context, as window 1 last told it; none while unlinked.
        (None, Some(two)) => two.context_source(),
        (None, None) => std::rc::Rc::new(|| None),
    };
    let context_for_link = editor_context_source.clone();
    // The pair file, read once by the window that holds the pair, before any other file of the pair: which modules
    // it gives to window 2 and whether this window takes them back. Window 2 is not started again at a launch, so
    // what it held comes back here, and its saved tabs are offered after this window's own.
    let pair_layout_dir = eitri_core::layout::persist::state_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    let window2_restored = false;
    let pair_restore = match pair_layout_dir.as_deref().filter(|_| pair.holds_pair()) {
        Some(dir) => {
            let plan = linked_window::pair_files::read_for_restore(dir, project_root, window2_restored);
            for note in &plan.notes {
                println!("{note}");
            }
            plan
        }
        None => linked_window::pair_files::PairRestore::default(),
    };
    let tab_files = linked_window::pair_files::tab_files(
        pair.pair_role(),
        pair.holds_pair() && !window2_restored,
        pair.holds_pair()
            && linked_window::pair_files::alone_on_project(
                eitri_core::turn_review::review_dir_for(
                    std::env::var_os("XDG_STATE_HOME").as_deref(),
                    std::env::var_os("HOME").as_deref(),
                    project_root,
                )
                .as_deref(),
            ),
        window2_restored,
    );
    if let Some(dir) = pair_layout_dir.as_deref() {
        let writing = pair.holds_pair() && !pair_restore.in_the_way;
        let start = if writing {
            pair_restore.start.clone()
        } else {
            // Window 2's view of the pair, written only once it is orphaned: it is open.
            eitri_core::window_link::pair::PairFile {
                window2_open: true,
                ..Default::default()
            }
        };
        if writing || pair.is_second() {
            let mut writer = linked_window::pair_files::PairWriter::new(dir, project_root, start, writing);
            if writing && pair_restore.write_now {
                writer.write_now();
            }
            pair.set_pair_writer(writer);
        }
    }
    let (agent_widget, agent_panel_handle) = agent_panel::build_agent_panel(
        project_root.to_path_buf(),
        editor_context_source,
        scratch_dir,
        backend_kind,
        panel_notice,
        None,
        tab_files,
    );
    // The panel reads the context when its tick runs, and its tick runs only when woken. Window 1 also tells
    // window 2 each new context, which wakes window 2's panel there.
    if let Some(one) = &window1_editor {
        let panel = agent_panel_handle.clone();
        let one = one.clone();
        feed_pump.on_context(move || {
            panel.wake();
            one.context_changed(context_for_link().as_ref());
        });
    } else {
        crate::editor_feeds::wake_panel_on_context(&feed_pump, &agent_panel_handle);
    }
    if let Some(two) = &window2_editor {
        let panel = agent_panel_handle.clone();
        two.on_context(Rc::new(move || panel.wake()));
    }
    // Turn review across the pair, in a window that can be one of a pair: one presence with the other window, the
    // overlap notes across both, and window 2's review in window 1's editor. Wired to each link as it comes up.
    let pair_review = match (&window1_editor, &window2_editor) {
        (Some(one), _) if pair.holds_pair() => Some(linked_window::review::Role::One(one.clone())),
        (None, Some(two)) => Some(linked_window::review::Role::Two(two.ops())),
        _ => None,
    }
    .map(|role| {
        let side: Rc<dyn linked_window::review::ReviewSide> = Rc::new(agent_panel_handle.clone());
        linked_window::review::PairReview::new(project_root, side, role, editor_repeat.clone())
    });
    if let Some(review) = &pair_review {
        let outbound = Rc::downgrade(review);
        agent_panel_handle.on_pair_outbound(move |out| {
            if let Some(review) = outbound.upgrade() {
                review.outbound(out);
            }
        });
        let running = Rc::downgrade(review);
        pair.set_running_turns(Rc::new(move || {
            running
                .upgrade()
                .map(|review| review.running_turns())
                .unwrap_or_default()
        }));
    }

    // `init.lua`: its Lua panels and the plugins it loads are registered before the config is read.
    driver.start(WinId::MAIN, StartStep::Kernel);

    // The window config, the keymap, the plugins' panels, the module keys and the keys help; the config phase is
    // told what they built at the end of the block.
    let config = match window_config::load(&lua_engine) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("eitri: {message}");
            std::process::exit(1);
        }
    };
    config.apply_to_panel(&agent_panel_handle);
    let keymap = config.keymap.clone();

    // Each `eitri.plugin.load` becomes a Lua panel whose content is the plugin's page. Registered before the
    // Lua panels are read below, so a plugin is placed, keyed, killed and revived like any Lua panel. The
    // theme feed has not started yet, so the pages start from the fallback, as the agent panel does.
    let plugin_panels = match plugin_panels::PluginPanels::install(&lua_engine, &keymap, &PageTheme::fallback()) {
        Ok(panels) => Rc::new(panels),
        Err(message) => {
            eprintln!("eitri: {message}");
            std::process::exit(1);
        }
    };
    plugin_panels.set_window_root(project_root);
    // The socket nvim reports followed buffers and plugin commands to, bound only when a loaded plugin follows the
    // editor or defines nvim commands: a window without one has no such socket at all. Bound now, before any page
    // exists, so the commands work before the module is first shown.
    let mut follow_socket = if plugin_panels.wants_follow_feed() {
        eitri_core::plugin::follow_feed::FollowSocket::bind()
    } else {
        None
    };
    if let Some(socket) = &follow_socket {
        plugin_panels.set_follow_feed(socket.socket_path(), scratch_path.as_deref());
    }

    // The bottom terminal starts hidden on first launch. Its shell starts when it is first shown
    // through any route, via the change hook below. A terminal that is never shown starts no
    // process and touches no font manager. Restoring a layout with the terminal shown starts it at
    // launch.
    //
    // Every terminal in the window has its own PTY and is stored in one registry by id. A killed
    // instance's pane stays idle there for reuse by the same numbered instance, whose host the
    // grid already holds under that id. New panes use the window's current colours, kept up to
    // date by the theme listener below.
    let terminal_colors = Rc::new(std::cell::Cell::new(terminal::colors_from(
        &eitri_core::theme::ThemeTokens::fallback(),
    )));
    let make_terminal_pane: Rc<dyn Fn() -> terminal::TerminalPane> = {
        let root = project_root.to_path_buf();
        let colors = terminal_colors.clone();
        Rc::new(move || {
            let pane = terminal::TerminalPane::new(root.clone());
            // The floor `build_vertical_split` gave the bottom slot on `main`: a terminal dragged to
            // zero reports a 1-row grid to its shell.
            pane.widget().set_size_request(-1, layout::BOTTOM_MIN_HEIGHT);
            pane.set_colors(colors.get());
            pane
        })
    };
    let terminals: Rc<RefCell<InstancePool<terminal::TerminalPane>>> = Rc::new(RefCell::new(InstancePool::new()));
    terminals
        .borrow_mut()
        .take_or_make(&ModuleId::terminal(), || make_terminal_pane());

    // The initial layout has `[editor | agent]` with the terminal hidden below both.
    // Lua panels are placed by `position` in registration order (`terminal::initial_layout`,
    // `Layout::initial`, `PanelSlot::placement`). Every window has editor, agent, and
    // terminal modules outside the Lua registry, so Lua panels do not replace them.
    // A Lua `bottom` panel goes below the terminal.
    let lua_panels: Vec<(ModuleId, PanelSlot, gtk4::Widget)> = {
        let panels = lua_engine.panels.borrow();
        panels
            .entries()
            .iter()
            .map(|entry| {
                (
                    entry.module.clone(),
                    entry.slot,
                    panels
                        .views
                        .widget(&entry.module)
                        .expect("registered panel view")
                        .clone(),
                )
            })
            .collect()
    };
    // A killed panel reloads its URL on reopening. Manual recovery resets the same shared guard
    // that its view's crash handler checks, including a deferred plugin page's handler.
    let lua_pages: gtk_host_services::LuaPages = Rc::new({
        let panels = lua_engine.panels.borrow();
        panels
            .entries()
            .iter()
            .map(|entry| {
                (
                    entry.module.clone(),
                    panels
                        .views
                        .widget(&entry.module)
                        .expect("registered panel view")
                        .clone(),
                    entry.url.clone(),
                    panels.views.guard(&entry.module).expect("registered panel guard"),
                )
            })
            .collect()
    });
    // The plugin modules that start hidden: the window never places them by itself. Each has its host, its
    // page entry and its module key from startup; its page is built the first time it is shown, and its
    // first show opens it by its own rule (`open_plugin_module`).
    let deferred: Vec<ModuleId> = lua_panels
        .iter()
        .map(|(id, _, _)| id.clone())
        .filter(|id| plugin_panels.starts_hidden(id))
        .collect();
    // The Lua panels as `init.lua` registered them, each at its own `position` (a plugin's at its own
    // rule's place when the rule says where), the deferred ones left out. Where the window places
    // modules by itself (`decls`, below) is these with the open rules applied.
    let panel_decls: Vec<ModuleDecl> = lua_panels
        .iter()
        .filter(|(id, _, _)| !deferred.contains(id))
        .map(|(id, slot, _)| ModuleDecl {
            id: id.clone(),
            placement: plugin_panels
                .own_rule(id)
                .and_then(|rule| rule.at)
                .map_or(slot.placement(), |at| without_keys(at).0),
        })
        .collect();
    // The whole declared rule of each plugin that starts shown: a first launch applies its `size` (and a float's
    // `focus`) as well as its place, as the open rules would.
    let own_rules: Vec<(ModuleId, OpenRule)> = lua_panels
        .iter()
        .filter(|(id, _, _)| !deferred.contains(id))
        .filter_map(|(id, _, _)| plugin_panels.own_rule(id).map(|rule| (id.clone(), rule)))
        .collect();
    // Each Lua panel's module key after `Ctrl+a`. A key that is reserved,
    // taken twice or not one character is a startup failure naming it, as `agent.font_size` is:
    // `init.lua`'s own errors are only logged, and a key that silently did nothing is the failure
    // this refuses.
    let lua_keys: Vec<(ModuleId, Option<String>)> = lua_engine
        .panels
        .borrow()
        .entries()
        .iter()
        .map(|entry| (ModuleId::lua(&entry.id), entry.key.clone()))
        .collect();
    let module_keys = match ModuleKeys::build(&lua_keys, &keymap) {
        Ok(keys) => Rc::new(keys),
        Err(err) => {
            eprintln!("eitri: {err}");
            std::process::exit(1);
        }
    };
    // The `?` overlay's window and prefix sections, generated from the keymap;
    // see `keys_help`. It is re-sent, only when the merged panel table changed, every time nvim's
    // own keys report changes (`FeedPump::listen_keys` below).
    let keys_help = eitri_panel::keys_help::KeysHelp::new(
        keymap.clone(),
        module_keys.clone(),
        agent_panel_handle.clone(),
        config.tmux_skipped.clone(),
        eitri_panel::keys_help::HelpScope::Window,
    );
    keys_help.send();
    // nvim's keys, from this window's own feed or, in window 2, as window 1 tells them.
    let apply_nvim_keys: Rc<dyn Fn(eitri_core::nvim_keys::NvimReport)> = {
        let keys_help = keys_help.clone();
        let plugin_panels = plugin_panels.clone();
        Rc::new(move |report: eitri_core::nvim_keys::NvimReport| {
            // A page's multi-key sequences wait as long as the editor's do.
            plugin_panels.set_timeoutlen(report.timeoutlen);
            keys_help.nvim_report(report);
        })
    };
    if let Some(feed) = nvim_keys_feed.as_mut() {
        let apply = apply_nvim_keys.clone();
        let plugin_panels = plugin_panels.clone();
        let one = window1_editor.clone();
        feed_pump.listen_keys(feed, move |report| {
            if let Some(one) = &one {
                one.keys_changed(&report);
            }
            apply(report);
            // The feed reports at nvim's VimEnter: a new editor session may be ready for the plugins' snippets.
            plugin_panels.editor_session();
        });
    }
    if let Some(two) = &window2_editor {
        two.on_keys(apply_nvim_keys.clone());
    }
    driver.start(
        WinId::MAIN,
        StartStep::Config {
            keymap: keymap.clone(),
            module_keys: module_keys.clone(),
        },
    );
    // The layout this window opens with: this project's state file if
    // it can be used, else `init.lua`'s `eitri.layout.default`, else the first launch. A malformed
    // default is a startup failure naming the call; a state file that cannot be used is not, since it
    // is state rather than config -- it is logged and the default opens.
    if let Some(message) = lua_engine.init_refusal() {
        eprintln!("eitri: {message}");
        std::process::exit(1);
    }
    let lua_default = lua_engine
        .layout
        .borrow()
        .default_tree()
        .cloned()
        .map(|tree| tree.expect("the kernel checked the default layout"));
    let rules = Rc::new(
        lua_engine
            .layout
            .borrow()
            .rules()
            .cloned()
            .expect("the kernel checked open rules"),
    );
    // Where the window places a module when no module holds the keys -- a first launch, a reconcile of
    // the file or a template, a module a workspace lacks, a module back after a kill: the Lua panels at
    // their rule's place, else their `position`, and each built-in or instance a rule names. Every one
    // of those reads these, so the rules reach them all.
    let decls: Vec<ModuleDecl> = {
        let (decls, notes) = rules.launch_decls(&panel_decls);
        for note in notes {
            println!("[layout] {note}");
        }
        decls
    };
    // What the controller's module verbs place and kill by: the open rules, where each declared module goes, the
    // plugin modules that start hidden with each one's own rule, where a new terminal's shell starts, and the
    // workspaces `init.lua` defined, which a workspace first made by name starts from.
    driver.set_module_setup(
        WinId::MAIN,
        eitri_core::wm::verbs::modules::ModuleSetup {
            rules: (*rules).clone(),
            decls: decls.clone(),
            start_hidden: deferred
                .iter()
                .map(|id| (id.clone(), plugin_panels.own_rule(id)))
                .collect(),
            project_dir: project_root.to_path_buf(),
            workspaces: lua_engine.layout.borrow().defined_workspaces().to_vec(),
        },
    );
    let state_dir = eitri_core::layout::persist::state_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    // Each window opens its own file; an unpaired window with none of its own yet reads the pair's window 1 file
    // once, read-only.
    let loaded = state_dir
        .as_deref()
        .map(|dir| eitri_core::layout::persist::load_for_role(dir, project_root, pair.pair_role(), &decls, &deferred));
    // The key that shows the editor, as the keymap binds it: named by the note a layout that hides
    // the editor prints, the way the toast names its own way back.
    let show_editor: Option<String> = keymap
        .keys_for(&Action::Module(ModuleId::editor()))
        .first()
        .map(|key| format!("{} {}", keymap.prefix().human(), key.human()));
    // `module_layout` is the current workspace's layout, the one every reader of the live layout holds;
    // `workspaces` holds the window's other workspaces beside it, and what a later build keeps in the
    // file.
    let (workspaces, module_layout, keep_copy) = match eitri_core::wm::persist::choose_startup_layout(
        lua_default.as_ref(),
        loaded,
        &decls,
        &rules,
        show_editor.as_deref(),
        &deferred,
        &own_rules,
    ) {
        Ok((workspaces, layout, notes, keep_copy)) => {
            for note in notes {
                println!("[layout] {note}");
            }
            (
                Rc::new(RefCell::new(workspaces)),
                Rc::new(RefCell::new(layout)),
                keep_copy,
            )
        }
        Err(message) => {
            eprintln!("eitri: {message}");
            std::process::exit(1);
        }
    };
    let defined = lua_engine
        .layout
        .borrow()
        .defined_workspaces()
        .iter()
        .map(|(name, tree)| tree.clone().map(|tree| (name.clone(), tree)))
        .collect::<Result<Vec<_>, _>>();
    let defined = match defined {
        Ok(defined) => defined,
        Err(message) => {
            eprintln!("eitri: {message}");
            std::process::exit(1);
        }
    };
    match eitri_core::wm::persist::add_defined_workspaces(
        &mut workspaces.borrow_mut(),
        &mut module_layout.borrow_mut(),
        &defined,
        &decls,
        &deferred,
    ) {
        Ok(notes) => {
            for note in notes {
                println!("[layout] {note}");
            }
        }
        Err(message) => {
            eprintln!("eitri: {message}");
            std::process::exit(1);
        }
    }
    // What the window's start owes its role, before anything is drawn: window 2's editor leaf retired and the
    // pages, panels and instances the pair does not give it marked away; an unpaired window's copy of window 1's
    // layout with every away mark dropped, since it builds its own instance of everything; window 1's layouts as
    // the pair file has them, which wins over its own file's away marks.
    let mut restore_base: Option<(eitri_core::layout::Layout, eitri_core::layout::Workspaces)> = None;
    if pair.is_second() {
        let frame = eitri_core::layout::Frame::new(eitri_core::layout::rules::NOMINAL_GRID, 1);
        if let Err(err) =
            linked_window::start_second(&mut module_layout.borrow_mut(), &mut workspaces.borrow_mut(), &frame)
        {
            eprintln!("eitri: window 2's layout could not start without its editor: {err}");
            std::process::exit(1);
        }
        // Started by `window.new`, not by a restore: the pair gives it nothing yet.
        linked_window::pair_files::second_start_away(
            &mut module_layout.borrow_mut(),
            &mut workspaces.borrow_mut(),
            &Default::default(),
        );
    } else if pair.is_unpaired() {
        linked_window::ignore_away(&mut module_layout.borrow_mut(), &mut workspaces.borrow_mut());
    } else if pair.holds_pair() {
        // Measured from before the restore: what the pair file changed here is a change, written at once.
        restore_base = Some((module_layout.borrow().clone(), workspaces.borrow().clone()));
        let can_build = |id: &ModuleId| {
            (id.number() >= 2 && id.kind().has_instances())
                || decls.iter().any(|d| d.id == *id)
                || deferred.contains(id)
        };
        for note in linked_window::pair_files::apply_main_restore(
            &pair_restore,
            &mut module_layout.borrow_mut(),
            &mut workspaces.borrow_mut(),
            &rules,
            &decls,
            &can_build,
        ) {
            println!("{note}");
        }
    } else {
        // Alone (the pair lock could not be had): no other window can hold anything, so nothing is away here.
        linked_window::ignore_away(&mut module_layout.borrow_mut(), &mut workspaces.borrow_mut());
    }
    let mut layout_saver = layout_state::saver(
        state_dir,
        pair.pair_role(),
        project_root,
        &module_layout,
        &workspaces,
        decls.clone(),
        keep_copy,
    );
    layout_saver.set_deferred(deferred.clone());
    if let Some((layout, spaces)) = restore_base.take() {
        layout_saver = layout_saver.with_base(&layout, &spaces);
        if layout_saver.submit_now(&module_layout.borrow(), &workspaces.borrow()) {
            println!("[layout] the layout as the pair file left it is written");
        }
    }
    // The window's layouts and its saver, given to the controller before any hook that posts to it.
    driver.start(
        WinId::MAIN,
        StartStep::Layout {
            parts: eitri_core::wm::WindowParts {
                layout: module_layout.clone(),
                workspaces: workspaces.clone(),
                saver: Some(layout_saver),
            },
        },
    );
    // Every module's host, added once and never reparented (`module_grid`'s module doc). It arranges through
    // the controller, which holds the window's layout from the phase above, and the host drives and reads it.
    let grid = ModuleGrid::new(module_layout.clone());
    {
        let allocator: Rc<dyn eitri_core::wm::Allocator> = driver.clone();
        grid.set_allocator(WinId::MAIN, Rc::downgrade(&allocator));
    }
    wm_host.set_grid(WinId::MAIN, Rc::new(grid.downgrade()));
    // With no `WebView` in this process (`webkit_sandbox`), the chat's and each Lua panel's place is
    // a plain GTK notice: allocated directly, as the editor is, rather than held at a settled size
    // like a `WebView` being resized.
    let web_host = if webkit_sandbox::decision().allows_webviews() {
        HostKind::Web
    } else {
        HostKind::Direct
    };
    // Every extra chat view's page, keyed by its module id: a view that leaves the layout keeps its page
    // here, idle, for the next view of that number only (`chat_views`'s module doc). The base view's page
    // is the `related` one of each: the pages share its web process (and so its memory).
    let chat_views: gtk_host_surfaces::ChatPool = Rc::new(RefCell::new(InstancePool::new()));
    let base_webview: Option<webkit6::WebView> = agent_widget.downcast_ref::<webkit6::WebView>().cloned();
    // One constructor per kind of host (`gtk_host_surfaces`): each adds its host to the grid and names it
    // as the module's surface, and wires nothing. The hosts built here are wired once, below, where the
    // window's key handling exists; a host made later is wired by what makes it.
    let surfaces = Rc::new(gtk_host_surfaces::Surfaces {
        host: Rc::downgrade(&wm_host),
        grid: grid.downgrade(),
        web_host,
        terminals: terminals.clone(),
        make_terminal_pane: make_terminal_pane.clone(),
        chat_views: chat_views.clone(),
        agent: agent_panel_handle.clone(),
        base_webview: base_webview.clone(),
        chat_notice: panel_notice.unwrap_or(CHAT_VIEWS_NEED_WEBKIT).to_string(),
        lua_pages: lua_pages.clone(),
        plugin_panels: plugin_panels.clone(),
        base_page: agent_widget.clone(),
        editor: editor_host.zip(pane.as_ref().map(|pane| pane.widget().clone())),
    });
    // Every terminal in the layout the window opens with gets its pane and host now, the base one
    // first and the numbered ones in number order, so the hosts exist before the first `apply`. None
    // starts a shell here: each starts when it is first shown (the controller's module lifecycle).
    let startup_terminals: Vec<ModuleId> = {
        let layout = module_layout.borrow();
        let workspaces = workspaces.borrow();
        let mut ids: Vec<ModuleId> = workspaces
            .each(&layout)
            .into_iter()
            .flat_map(|(_, layout)| layout.modules())
            .filter(|id| id.kind() == ModuleKind::Terminal && id.number() >= 2)
            .collect();
        ids.sort();
        ids.dedup();
        ids.insert(0, ModuleId::terminal());
        ids
    };
    // Every chat view the layout the window opens with holds, in any workspace, gets its page and host
    // now, as the numbered terminals do, so the hosts exist before the first `apply`. Where WebKit cannot
    // start, each view's place is the notice the base view shows.
    let startup_chat_views: Vec<ModuleId> = {
        let layout = module_layout.borrow();
        let workspaces = workspaces.borrow();
        let layouts: Vec<&eitri_core::layout::Layout> =
            workspaces.each(&layout).into_iter().map(|(_, layout)| layout).collect();
        extra_chat_views(&layouts)
    };
    for (_, slot, widget) in &lua_panels {
        // On the `WebView`: its host measures its one child, so the grid sees the same floor.
        if *slot == PanelSlot::Bottom {
            widget.set_size_request(-1, layout::BOTTOM_MIN_HEIGHT);
        }
    }
    // The hosts the window opens with, each made by the controller's `CreateSurface` (which declares it, so its
    // lifecycle effects for the startup layout follow the apply below) and added to the grid in this order: the
    // editor's, the base chat's, every startup terminal's, every startup chat view's, every Lua panel's (a plugin
    // that starts hidden gets its host and its wiring now and its page on its first show).
    //
    // Every web module -- the chat and each Lua panel -- sits at (0,0) of a `WebHost` of its own
    // rather than in the grid directly, and takes the keys through its `WebView` (its focus target):
    // WebKitGTK adds the `WebView`'s position in its parent to the input method's caret rectangle,
    // which GTK4's input methods then translate to the window a second time, so a `WebView` at the
    // module's own x put the candidate window that far right of the caret (`web_host`'s module doc).
    // The notice that stands in for a `WebView` where WebKit cannot start is wrapped the same way, so
    // the shape does not depend on `webkit_sandbox`.
    let startup_surfaces: Vec<SurfaceDecl> = {
        let plugin_modules = plugin_panels.modules();
        let lua_entries: Vec<(ModuleId, String, String, String)> = lua_engine
            .panels
            .borrow()
            .entries()
            .iter()
            .map(|entry| {
                (
                    entry.module.clone(),
                    entry.id.clone(),
                    entry.url.clone(),
                    entry.title.clone(),
                )
            })
            .collect();
        let mut made = vec![
            SurfaceDecl {
                module: ModuleId::editor(),
                kind: SurfaceKind::Editor { spawn: editor_spawn },
                deferred: false,
            },
            SurfaceDecl {
                module: ModuleId::agent(),
                kind: SurfaceKind::Chat { view: ViewId::BASE },
                deferred: false,
            },
        ];
        made.extend(startup_terminals.iter().map(|id| SurfaceDecl {
            module: id.clone(),
            kind: SurfaceKind::Terminal {
                cwd: project_root.to_path_buf(),
            },
            deferred: false,
        }));
        made.extend(startup_chat_views.iter().filter_map(|id| {
            Some(SurfaceDecl {
                module: id.clone(),
                kind: SurfaceKind::Chat {
                    view: ViewId::of_module(id)?,
                },
                deferred: false,
            })
        }));
        made.extend(lua_entries.into_iter().map(|(id, plugin, url, title)| {
            if plugin_modules.contains(&id) {
                SurfaceDecl {
                    deferred: plugin_panels.starts_hidden(&id),
                    kind: SurfaceKind::PluginPage { plugin },
                    module: id,
                }
            } else {
                SurfaceDecl {
                    module: id,
                    kind: SurfaceKind::LuaPanel { url, title },
                    deferred: false,
                }
            }
        }));
        made
    };
    // A window with no editor declares the editor's surface like any other and the controller makes none for it,
    // so only what is made is checked below.
    let startup_modules: Vec<ModuleId> = linked_window::hosted_at_start(&startup_surfaces, &pair.wm_spec_role().1);
    wm_host.set_surface_maker(surfaces.clone());
    driver.start(
        WinId::MAIN,
        StartStep::Surfaces {
            decls: startup_surfaces,
        },
    );
    // The window cannot open without them: each was made from what this window registered a moment ago.
    for id in &startup_modules {
        assert!(grid.has_host(id), "{id}: its host was not made at startup");
    }
    // The panel is told which views there are before the base page's `ready` can start a restore, so a restored
    // tab saved in one of them comes back into it.
    agent_panel_handle.set_layout_views(startup_chat_views.iter().filter_map(ViewId::of_module).collect());
    // A plugin that starts hidden costs no web process until its module is first shown. A layout that
    // reopens one shown builds its page now; every later first show, by any route, builds it through the
    // controller's module lifecycle.
    let shown_at_start: Vec<ModuleId> = {
        let layout = module_layout.borrow();
        deferred.iter().filter(|id| layout.is_shown(id)).cloned().collect()
    };
    for id in &shown_at_start {
        plugin_panels.build_page(id, &plugin_panels.theme());
    }
    // What the controller's module lifecycle calls: a terminal's shell started the first time it is shown,
    // however it got there, a start-hidden plugin's page built on its first show, each plugin host told
    // whether its module is drawn, and a killed module shown again started fresh. Given before the startup
    // `apply`, whose lifecycle effects come right after it.
    // The chat views whose removal is their caller's own, while their tabs close or open: a kill's, and a new view's
    // first tab. The panel's word that such a view holds no tab is then not a request to remove it.
    let chat_busy: gtk_host_services::ChatBusy = Rc::default();
    wm_host.set_services(Rc::new(gtk_host_services::Services {
        editor: pane.clone(),
        terminals: terminals.clone(),
        plugin_panels: plugin_panels.clone(),
        agent: agent_panel_handle.clone(),
        lua_pages: lua_pages.clone(),
        chat_busy: chat_busy.clone(),
        project_dir: project_root.to_path_buf(),
        sink: {
            let sink: Rc<dyn eitri_core::wm::HostSink> = driver.clone();
            Rc::downgrade(&sink)
        },
    }));
    // The startup layout is the controller's first layout batch: what the layout shows mapped (every host
    // starts off screen for it), the float band stacked, the grid allocated, then the lifecycle effects of the
    // modules the surfaces phase declared -- each shown terminal's shell, each shown start-hidden page, each
    // plugin told whether it is drawn. The launch is not a change, so nothing is saved for it. Every host the
    // window opens with was made by the walk, so no module is left to declare here.
    grid.take_every_host_off_screen();
    driver.first_apply(WinId::MAIN, Vec::new());
    // Every change of the arrangement is written to this project's state file, 500ms after the last
    // one, and once more when the window closes if it is still unwritten; a change of the keys alone
    // never is. The controller decides on its own changes and on this, the grid's report of a change a
    // verb still in the shell applied. A pinned row's first length is settled inside the controller's
    // allocation, which tells the saver itself.
    {
        let driver = driver.clone();
        grid.connect_changed(move || driver.post(HostEvent::LayoutChangedByShell { win: WinId::MAIN }));
    }

    // Gives a module the keys, as the controller's `Focus` op does: a start-hidden plugin's page built first, the
    // editor through `NeovideEditorPane::grab_focus`, which also tells the input method, and any other module
    // through its focus target's own `grab_focus` (`ModuleGrid::focus_target`), never its host's: a web module's
    // host is a `WebHost`, which is not focusable itself -- it forwards a grab to its `WebView`, but this does
    // not rely on that (`web_host`'s module doc). Whether the module is on screen is the controller's to know
    // before it asks.
    let grab_module: Rc<dyn Fn(&ModuleId) -> bool> = {
        let pane = pane.clone();
        let grid = grid.clone();
        let plugin_panels = plugin_panels.clone();
        Rc::new(move |id| {
            // A start-hidden plugin's slot holds no view until its page is built, and an empty slot refuses the
            // grab. A workspace switch and an open give the keys before the change hooks that build it run, so
            // the page is built here first, with no layout borrow held.
            if plugin_panels.starts_hidden(id) && !plugin_panels.is_built(id) {
                plugin_panels.build_page(id, &plugin_panels.theme());
            }
            if id.kind() == ModuleKind::Editor {
                // A window with no editor has nothing to give the keys to.
                let Some(pane) = &pane else { return false };
                pane.grab_focus();
                // `NeovideEditorPane::grab_focus` returns nothing, so ask: did the editor's widget become the
                // window's focus widget?
                return pane.widget().is_focus();
            }
            // Read live: a module added after startup is found too.
            grid.focus_target(id).is_some_and(|target| target.grab_focus())
        })
    };
    // The controller's `Focus` ops give the keys this way, the first focus after the window is shown among them.
    wm_host.set_focus(grab_module.clone());
    // The bare grab for the arms still in the shell (a hide, a kill, a workspace switch handing the keys to the
    // module the layout chose), which ask the layout themselves: a hidden module is refused, since GTK4's
    // `grab_focus` does not refuse an unmapped widget and the keys would go to a module nobody can see.
    let focus_module: Rc<dyn Fn(&ModuleId) -> bool> = {
        let module_layout = module_layout.clone();
        let grab_module = grab_module.clone();
        Rc::new(move |id| {
            match module_layout.try_borrow() {
                Ok(layout) if !layout.is_shown(id) => {
                    println!("[focus] {id} is hidden; the keys stay where they are");
                    return false;
                }
                Ok(_) => {}
                // No caller holds the layout for writing while it moves focus (`ModuleGrid::apply`,
                // `hide_module`); this line is how a new one would show up.
                Err(_) => eprintln!("[focus] BUG: the layout was borrowed for writing when {id} was given the keys"),
            }
            grab_module(id)
        })
    };
    // Gives a module the keys by a route still in the shell that chooses one -- a module key, a tray chip,
    // `eitri.layout.focus`. A module on a tab its container does not draw has that tab chosen first
    // (`ModuleGrid::switch_tab`, which maps its host before the grab): a bare grab would put the keys in a
    // host nobody can see, and the layout would then draw a tab the grid never mapped. `focus_module` stays
    // the bare grab, for the hides and kills that hand the keys to a module the layout already draws.
    let give_keys: Rc<dyn Fn(&ModuleId) -> bool> = {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let focus_module = focus_module.clone();
        Rc::new(move |id| {
            let tabbed_away = module_layout.try_borrow().is_ok_and(|layout| layout.is_tabbed_away(id));
            if !tabbed_away {
                return focus_module(id);
            }
            let grabbed = std::cell::Cell::new(false);
            let focus = |to: &ModuleId| {
                let took = focus_module(to);
                grabbed.set(took);
                took
            };
            match grid.switch_tab(id, &focus) {
                Ok(()) => grabbed.get(),
                Err(err) => {
                    println!("[focus] {id}: its tab could not be chosen: {err}");
                    false
                }
            }
        })
    };

    // The chrome: the top bar, the window's content, and every controller and hook, installed in the order below.
    // The window has no HeaderBar (it is undecorated): the custom top bar built here (chrome::build_top_bar) is
    // its only titlebar.
    driver.start(WinId::MAIN, StartStep::Chrome);

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.add_css_class("shell-root");

    // `app.reload-agent-panel` + `prefix r`; the top bar's own `⟳` button points at the same
    // action. Everything about it -- why it is an app action, what it costs, what is still
    // unverified -- is in `agent_panel::install_reload_action`'s doc comment. One call rather than
    // an inline block on purpose: `build_ui` is being edited by two other tracks of the same plan.
    agent_panel::install_reload_action(app, &agent_panel_handle, {
        let module_layout = module_layout.clone();
        Rc::new(move || {
            module_layout
                .try_borrow()
                .map_or(ViewId::BASE, |layout| keys_chat_view(&layout))
        })
    });
    // The panel starts on the same fallback the chrome does, then both follow nvim together.
    // So does the editor's own clear colour: `neovide-editor` snaps the grid's sub-cell remainder
    // to the TOP edge so the bottom sits flush against the window's edge, which puts a thin band
    // outside the rect the renderer paints. Left at that crate's default it is a near-black line
    // under the top bar -- the same defect the `CONTENT_MARGIN` removal fixed -- so it follows
    // `bg`, and is then the same colour as the first text row below it.
    let editor_clear = |tokens: &eitri_core::theme::ThemeTokens| {
        let bg = tokens.bg;
        (bg.r, bg.g, bg.b)
    };
    // The panel's LIVE size: starts at the base and
    // is kept current by `text_size_controller` below. `panel_tokens` reads this rather than the
    // captured `panel_font_size` so a colorscheme change re-derives every OTHER token but never
    // resets a zoom back to the un-zoomed base.
    // The editor's font size (`guifont`), which the panel's text equals unless `agent.font_size` is set:
    // Neovide's default until nvim's first theme payload says otherwise (the listener below), and in the
    // panel's CSS px, which differ from the editor's logical px by WebKitGTK's page zoom (`webkit_zoom`).
    let editor_font = Rc::new(std::cell::Cell::new(webkit_zoom::EditorFont::new(
        gtk4::Settings::default().map_or(xft_dpi::GTK_DEFAULT_XFT_DPI, |s| s.gtk_xft_dpi()),
        eitri_core::theme::guifont_size_px(""),
    )));
    let panel_base = if config.panel_font_follows_editor {
        editor_font.get().css_px()
    } else {
        config.panel_font_size
    };
    let panel_px = Rc::new(std::cell::Cell::new(panel_base));
    // The editor's cell height, reported by `connect_cell_size_changed` below. It is `None` until
    // nvim reports a font, as is `--nv-editor-row`; `panel_tokens` reads it when building the
    // theme. The editor reports GTK logical pixels, converted to panel CSS pixels using
    // WebKitGTK's page zoom when `gtk-xft-dpi` differs from 96 dpi. Read the DPI here: `build_ui`
    // has not returned to the main loop since creating the panel's `WebView`, so neither side
    // could have received a DPI change in between.
    let editor_row = Rc::new(std::cell::Cell::new(webkit_zoom::EditorRow::new(
        gtk4::Settings::default().map_or(xft_dpi::GTK_DEFAULT_XFT_DPI, |s| s.gtk_xft_dpi()),
    )));
    // One helper, so the startup theme and every later one cannot disagree about the size. The
    // editor's clear colour and the GTK chrome do not take it: it is the panel's text, not the
    // window's.
    let panel_tokens = {
        let panel_px = panel_px.clone();
        let editor_row = editor_row.clone();
        move |payload: Option<&eitri_core::theme::payload::NvimThemePayload>| {
            let mut tokens = match payload {
                Some(p) => eitri_core::theme::ThemeTokens::derive(p),
                None => eitri_core::theme::ThemeTokens::fallback(),
            };
            tokens.font_size_px = text_size::live_panel_font_size_px(&panel_px);
            // `live_panel_font_size_px` is unit-tested in `text_size.rs`, but this GTK closure has no
            // headless test to catch using the captured startup `panel_font_size` instead. Check in the
            // GUI that changing the colorscheme after zooming keeps the panel zoomed.
            tokens.editor_row_px = editor_row.get().css_px();
            tokens
        }
    };
    agent_panel_handle.set_theme(&panel_tokens(None));
    if let Some(pane) = &pane {
        pane.set_clear_color(editor_clear(&eitri_core::theme::ThemeTokens::fallback()));
    }
    // Each terminal pane took the fallback colours when created by `make_terminal_pane`. When the
    // editor reports or changes its cell height, save it for `panel_tokens` so later colorscheme
    // changes retain it. Push the height into the panel's live theme through the same `set_theme`
    // path used for font-size zoom.
    if let Some(pane) = &pane {
        let editor_row = editor_row.clone();
        let agent_panel_handle = agent_panel_handle.clone();
        let one = window1_editor.clone();
        pane.connect_cell_size_changed(move |_, h| {
            let mut row = editor_row.get();
            let css_px = row.set_cell_height(h as f32);
            editor_row.set(row);
            agent_panel_handle.set_editor_row_px(css_px);
            if let Some(one) = &one {
                one.cell_height_changed(h as f32);
            }
        });
    }
    // Window 2's panel rows match the editor's, whose cell window 1 measures.
    if let Some(two) = &window2_editor {
        let editor_row = editor_row.clone();
        let agent_panel_handle = agent_panel_handle.clone();
        two.on_cell_height(Rc::new(move |h: f32| {
            let mut row = editor_row.get();
            let css_px = row.set_cell_height(h);
            editor_row.set(row);
            agent_panel_handle.set_editor_row_px(css_px);
        }));
    }
    // A key the user pressed in the editor reached nvim: the typing window that paces the agent
    // panel's stream (`AgentPanelHandle::note_editor_key`). A notification only.
    if let Some(pane) = &pane {
        let agent_panel_handle = agent_panel_handle.clone();
        pane.connect_key_activity(move || agent_panel_handle.note_editor_key());
    }
    // ...and the moment WebKitGTK re-zooms the panel: it follows `notify::gtk-xft-dpi` live
    // (`webkit_zoom`'s module doc), so the same row is a different number of CSS px afterwards.
    // The live value, not the notified one: `xft_dpi`'s own handler may already have replaced it.
    // No hook on the output scale: WebKit's page zoom does not depend on it.
    if let Some(settings) = gtk4::Settings::default() {
        let editor_row = editor_row.clone();
        let agent_panel_handle = agent_panel_handle.clone();
        settings.connect_gtk_xft_dpi_notify(move |settings| {
            let mut row = editor_row.get();
            let resend = row.follow_xft_dpi(settings.gtk_xft_dpi());
            editor_row.set(row);
            if let Some(css_px) = resend {
                agent_panel_handle.set_editor_row_px(css_px);
            }
        });
    }
    // Zoom both panes together, and each one alone, built before the theme listener below, which hands it the
    // editor's new font size. Registers `app.text-larger`/`-smaller`/`-reset` (both panes) and follows
    // `g:neovide_scale_factor` back from nvim; the prefix's one-pane `=`/`-`/`0` reaches `apply_editor`/
    // `apply_panel` directly once the controller has resolved which pane holds the keys (the text-size arm below).
    let text_size_controller = text_size::TextSizeController::install(
        app,
        // Every full window has an editor: a Lua `main` panel hides it rather than replacing it. The
        // pane buffers text-size writes through `ScaleWatch` until nvim starts. Window 2 has no
        // editor, so its text size applies only to its panels.
        pane.clone(),
        agent_panel_handle.clone(),
        panel_base,
        panel_px,
    );
    // nvim's theme, from this window's own feed or, in window 2, as window 1 tells it.
    let apply_theme: Rc<dyn Fn(eitri_core::theme::payload::NvimThemePayload)> = {
        let theme_css = theme_css.clone();
        // What the stylesheet paints, for the widgets GTK leaves on the old one (`theme::restyle`).
        let window_for_theme = window.clone();
        let agent_panel_handle = agent_panel_handle.clone();
        let pane_for_theme = pane.clone();
        let terminals = terminals.clone();
        let terminal_colors = terminal_colors.clone();
        let plugin_panels = plugin_panels.clone();
        let editor_font = editor_font.clone();
        let text_size_controller = text_size_controller.clone();
        let follows_editor = config.panel_font_follows_editor;
        Rc::new(move |payload: eitri_core::theme::payload::NvimThemePayload| {
            // The editor's font first, so the theme sent below already carries the panel's new size.
            if follows_editor {
                let mut font = editor_font.get();
                let moved = font.set_logical_px(eitri_core::theme::guifont_size_px(&payload.options.guifont));
                editor_font.set(font);
                if let Some(base) = moved {
                    text_size_controller.set_base(base);
                }
            }
            let tokens = eitri_core::theme::ThemeTokens::derive(&payload);
            println!(
                "[theme] following nvim colorscheme {:?} (background={})",
                payload.options.colors_name, payload.options.background
            );
            theme_css.update(&tokens, window_for_theme.upcast_ref());
            agent_panel_handle.set_theme(&panel_tokens(Some(&payload)));
            if let Some(pane) = &pane_for_theme {
                pane.set_clear_color(editor_clear(&tokens));
            }
            // Every terminal pane, idle ones too, so one reused later is already in the theme; and
            // the colours a pane made later starts with.
            let colors = terminal::colors_from(&tokens);
            terminal_colors.set(colors);
            let panes: Vec<terminal::TerminalPane> = terminals.borrow().each().cloned().collect();
            for pane in panes {
                pane.set_colors(colors);
            }
            // The derived tokens, not the panel's sized ones: a page's set carries no size, so a size-only
            // change leaves it equal and sends the pages nothing.
            plugin_panels.set_theme(&tokens);
        })
    };
    if let Some(feed) = theme_feed.as_mut() {
        let apply = apply_theme.clone();
        let plugin_panels = plugin_panels.clone();
        let one = window1_editor.clone();
        feed_pump.listen_theme(feed, move |payload| {
            if let Some(one) = &one {
                one.theme_changed(&payload);
            }
            apply(payload);
            // The feed reports at nvim's VimEnter: a new editor session may be ready for the plugins' snippets.
            plugin_panels.editor_session();
        });
    }
    if let Some(two) = &window2_editor {
        two.on_theme(apply_theme.clone());
    }

    // The desktop's text scaling moved WebKit's page zoom, so the editor's size is a different number of
    // the panel's CSS px (the cell height's own handler above does the same for `--nv-editor-row`).
    if config.panel_font_follows_editor {
        if let Some(settings) = gtk4::Settings::default() {
            let controller = text_size_controller.clone();
            settings.connect_gtk_xft_dpi_notify(move |settings| {
                let mut font = editor_font.get();
                let moved = font.follow_xft_dpi(settings.gtk_xft_dpi());
                editor_font.set(font);
                if let Some(base) = moved {
                    controller.set_base(base);
                }
            });
        }
    }

    // Every Lua panel's page follows the window's text size: a plugin's page once its view is built
    // (at startup, or at its first show), any other Lua panel's view now. This attaches the view; its
    // host's `Ctrl`+wheel comes from `wire_host`, when the controller makes that host.
    {
        let controller = text_size_controller.clone();
        plugin_panels.set_page_built(Rc::new(move |id: &ModuleId, view: &webkit6::WebView| {
            controller.attach_page(id.clone(), view);
        }));
        let plugin_modules = plugin_panels.modules();
        for (id, widget, _, _) in lua_pages.iter() {
            if plugin_modules.contains(id) {
                continue;
            }
            if let Some(view) = widget.downcast_ref::<webkit6::WebView>() {
                text_size_controller.attach_page(id.clone(), view);
            }
        }
    }

    let top_bar = chrome::build_top_bar(&window, project_root);
    root.append(&top_bar.widget);
    root.append(&grid);

    // Names for tray chips and the prefix strip: built-ins use their kind, and Lua panels use
    // their registered `title`.
    let lua_titles: Vec<(ModuleId, String)> = lua_engine
        .panels
        .borrow()
        .entries()
        .iter()
        .map(|entry| (ModuleId::lua(&entry.id), entry.title.clone()))
        .collect();
    let module_title: Rc<dyn Fn(&ModuleId) -> String> = {
        let lua_titles = lua_titles.clone();
        Rc::new(move |id| eitri_core::wm::titles::module_title(id, &lua_titles))
    };
    // The tray: a chip for every module off screen, one per module, in the order the
    // grid holds them. Its chips are top-bar items ahead of `↻`, for `h`/`l`, HINT and `Ctrl+k`; the
    // workspace name, left of them on the bar, is the first item.
    let tray_modules: Vec<ModuleId> = grid.hosts().into_iter().map(|(id, _)| id).collect();
    let tray = tray::Tray::build(
        &eitri_core::wm::tray::chip_order(&tray_modules)
            .iter()
            .map(|id| (id.clone(), module_title(id)))
            .collect::<Vec<_>>(),
    );
    top_bar.tray.append(tray.widget());
    let prefix_strip = Rc::new(prefix_strip::PrefixStrip::new());
    top_bar.strip.append(prefix_strip.widget());
    let top_items: Vec<gtk4::Widget> = std::iter::once(top_bar.workspace.clone().upcast())
        .chain(tray.items())
        .chain(top_bar.items.iter().cloned())
        .collect();
    // The global `f` HINT draws its GTK labels (top-bar items, the editor, each Lua panel) as
    // overlay children of this, positioned over whatever they label. Its only child is the whole
    // window content, so the layout is exactly what it was without it; the labels never take a
    // click.
    let hint_overlay = gtk4::Overlay::new();
    hint_overlay.set_child(Some(&root));
    window.set_child(Some(&hint_overlay));
    let toast = toast::Toast::install(&hint_overlay, &top_bar.widget);
    // The window's y/n, for a window close that would interrupt a running tab and for `prefix x`. Built here,
    // before everything that asks it; its key layer is added with the window's others, in their declared order
    // (`gtk_host::keys::install_window_layers`, below).
    let close_prompt = close_prompt::ClosePrompt::new(
        &hint_overlay,
        &window,
        &top_bar.widget,
        top_bar.strip.upcast_ref::<gtk4::Widget>(),
    );
    // A refusal from an arm still in the shell goes to the controller, which draws every flash and toast.
    let refusal = Refusal::new(&driver);
    // The tmux import says it happened once: the first launch where it changes the keys, and again
    // only when what it imports changes (its fingerprint, kept with Eitri's other state).
    let home_dir = std::env::var_os("HOME").map(std::path::PathBuf::from);
    if let Some(import) = config.tmux_import.as_ref() {
        if let Some(text) = eitri_core::keymap::tmux::notice::toast_text(import, &keymap, home_dir.as_deref()) {
            let fingerprint = eitri_core::keymap::tmux::notice::fingerprint(import);
            let state = eitri_core::keymap::tmux::notice::state_file(
                std::env::var_os("XDG_STATE_HOME").as_deref(),
                std::env::var_os("HOME").as_deref(),
            );
            if state
                .as_deref()
                .is_none_or(|file| eitri_core::keymap::tmux::notice::needs_notice(file, &fingerprint))
            {
                toast.show_for(&text, toast::NOTICE_FOR);
                if let Some(file) = state.as_deref() {
                    if let Err(err) = eitri_core::keymap::tmux::notice::remember(file, &fingerprint) {
                        eprintln!(
                            "[keymap] tmux: could not record the notice in {}: {err}",
                            file.display()
                        );
                    }
                }
            }
        }
    }

    // The global `f` HINT. Three
    // triggers reach the same toggle: `prefix f` (the keymap's `hint`, below), `f` on the top bar,
    // and `f` in the panel's BROWSE (the panel's `hint_request`).
    let hint_coordinator = hint::HintCoordinator::new(hint::HintWidgets {
        window: window.clone(),
        overlay: hint_overlay.clone(),
        top_items: top_items.clone(),
        editor: pane.clone(),
        agent_widget: agent_widget.clone(),
        modules: {
            let grid = grid.clone();
            // A chat with no page (`webkit_sandbox`) has no labels of its own to answer
            // `hint_collect` with: left in, every HINT would wait out the panel's 300 ms timeout
            // and still label nothing there. Left out, the rest of the window is labelled at once.
            let chat_has_page = panel_notice.is_none();
            Rc::new(move || {
                grid.hosts_in_tree_order()
                    .into_iter()
                    .filter(|(id, _)| chat_has_page || id.kind() != ModuleKind::Agent)
                    .collect()
            })
        },
        // A landing is the controller's: a module gets the keys, a strip entry is chosen as a click chooses it.
        focus_module: {
            let driver = Rc::downgrade(&driver);
            Rc::new(move |id: &ModuleId| {
                let Some(driver) = driver.upgrade() else { return false };
                driver.post(HostEvent::Hint(HintEvent::Landed {
                    win: WinId::MAIN,
                    target: HintTarget::Module(id.clone()),
                }));
                true
            })
        },
        strips: {
            let grid = grid.clone();
            Rc::new(move || grid.strip_entries())
        },
        activate_strip: {
            let driver = Rc::downgrade(&driver);
            Rc::new(move |id: &ModuleId| {
                if let Some(driver) = driver.upgrade() {
                    driver.post(HostEvent::Hint(HintEvent::Landed {
                        win: WinId::MAIN,
                        target: HintTarget::StripEntry(id.clone()),
                    }));
                }
            })
        },
        agent: agent_panel_handle.clone(),
    });
    // `select.number`'s digits, drawn over every module's host, and HINT's toggle from the prefix.
    wm_host.set_hints(Rc::new(ShellHints {
        coordinator: hint_coordinator.clone(),
        grid: grid.downgrade(),
    }));
    // The panel's hooks, each built below where its captures are, and registered together once the
    // last is built (`panel_wiring`).
    let mut panel_hooks = panel_wiring::PanelHooks::default();
    {
        let coordinator = hint_coordinator.clone();
        panel_hooks.on_hint = Some(Box::new(move |message: agent_panel::HintInbound| {
            coordinator.on_panel(message)
        }));
    }

    // The top bar gave the keys up by its own key (`Ctrl+j`, `Esc`, the bar hiding while it held them): the
    // controller gives them back to the module that last had them.
    let return_to_pane: Rc<dyn Fn()> = {
        let driver = Rc::downgrade(&driver);
        Rc::new(move || {
            if let Some(driver) = driver.upgrade() {
                driver.post(HostEvent::TopBarLeft { win: WinId::MAIN });
            }
        })
    };

    // F11 fullscreen and `prefix F11` immersive both follow `g:neovide_fullscreen`.
    // `return_to_pane` restores focus when the top bar hides while focused; focusing a top-bar
    // item can reveal it.
    let window_modes = window_mode::WindowModes::install(app, &window, &top_bar, pane.clone(), return_to_pane.clone());

    // The window's chrome is the controller's model, drawn here: each `Chrome` op reads it and touches only the
    // widgets whose field changed (`gtk_host::chrome::reconcile`). The widgets are the ones made above, kept as they
    // are; the model gives their text, always set as plain text, whether they show, and their classes. Every flash
    // and every toast of a refusal or a chat comes from it, once per new serial.
    {
        let model_driver = Rc::downgrade(&driver);
        wm_host.set_chrome_model(Rc::new(move |win: WinId| {
            model_driver
                .upgrade()
                .map(|driver| driver.query(move |c| c.chrome(win)))
        }));
        let attention_of = agent_panel_handle.clone();
        wm_host.set_view_attention(Rc::new(move |view: ViewId| attention_of.attention_of(view)));
        let strip_grid = grid.downgrade();
        let bar_grid = grid.downgrade();
        let shown_toast = toast.clone();
        let reveal_modes = window_modes.clone();
        let chip_tray = tray.clone();
        let armed_name = top_bar.app_name.clone();
        let flashed_name = top_bar.app_name.clone();
        let strip = prefix_strip.clone();
        let (workspace_button, workspace_label) = (top_bar.workspace.clone(), top_bar.workspace_label.clone());
        let (selection_label, mark_label) = (top_bar.selection.clone(), top_bar.mark.clone());
        wm_host.set_chrome_widgets(
            WinId::MAIN,
            gtk_host::chrome::ChromeWidgets {
                workspace: Box::new(move |project, text| {
                    chrome::show_workspace(&workspace_button, &workspace_label, project, text)
                }),
                selection: Box::new(move |text| chrome::show_mark(&selection_label, text)),
                mark: Box::new(move |text| chrome::show_mark(&mark_label, text)),
                flash: Box::new(move || show_flash(&flashed_name)),
                chip: Box::new(move |chip| chip_tray.set_chip(chip)),
                strips: Box::new(move |strips| {
                    if let Some(grid) = strip_grid.upgrade() {
                        grid.set_strip_labels(strips);
                    }
                }),
                float_bars: Box::new(move |bars| {
                    if let Some(grid) = bar_grid.upgrade() {
                        grid.set_float_labels(bars);
                    }
                }),
                // The app name's armed block goes with the strip: both say the prefix waits.
                prefix_strip: Box::new(move |pieces| {
                    if pieces.is_some() {
                        armed_name.add_css_class("prefix-armed");
                    } else {
                        armed_name.remove_css_class("prefix-armed");
                    }
                    strip.show(pieces.unwrap_or(&[]));
                }),
                toast: Box::new(move |up| match up.kind {
                    ToastKind::Attention => shown_toast.show_attention(&up.text),
                    ToastKind::Other | ToastKind::Standing => shown_toast.show(&up.text),
                }),
                // A wait whose strip is the only list of what its keys pick (the workspace chooser) brings a hidden
                // top bar out, and it hides again once the wait is over; the keys stay in the module meanwhile, so
                // no focus change would hide it.
                top_bar_revealed: Box::new(move |revealed| {
                    if revealed {
                        reveal_modes.reveal_top_bar();
                    } else {
                        reveal_modes.conceal_top_bar();
                    }
                }),
                // `prefix x`'s y/n on the window's prompt overlay, placed by its own widgets' measures.
                prompt: gtk_host::chrome::PromptWidgets {
                    show: Box::new({
                        let prompt = close_prompt.clone();
                        move |text| prompt.show_model(text)
                    }),
                    measures: Box::new({
                        let prompt = close_prompt.clone();
                        move || prompt.measures()
                    }),
                    place: Box::new({
                        let prompt = close_prompt.clone();
                        move |x, y| prompt.place(x, y)
                    }),
                    hide: Box::new({
                        let prompt = close_prompt.clone();
                        move || prompt.hide_model()
                    }),
                },
                // The top bar's rename entry, holding the name as plain text while the controller has it open.
                rename: gtk_host::chrome::RenameWidgets {
                    open: Box::new({
                        let entry = top_bar.rename.clone();
                        move |text| chrome::fill_rename(&entry, text)
                    }),
                    refill: Box::new({
                        let entry = top_bar.rename.clone();
                        move |text| chrome::fill_rename(&entry, text)
                    }),
                    close: Box::new({
                        let entry = top_bar.rename.clone();
                        move || chrome::close_rename(&entry)
                    }),
                },
            },
        );
    }
    // While the controller's y/n shows, its keys go to the controller's window stage, which answers it.
    close_prompt.set_driver(Rc::downgrade(&driver));
    // What the chrome names things by: the project, each Lua panel by the title it registered with, and the modules
    // the tray has a chip for. The first draw follows, whatever the controller announced meanwhile.
    driver.set_titles(
        WinId::MAIN,
        ChromeTitles {
            project: eitri_core::wm::titles::project_display_name(project_root),
            lua: lua_titles.clone(),
            tray: tray_modules.clone(),
        },
    );
    if let Err(why) = wm_host.draw_chrome(WinId::MAIN) {
        eprintln!("[wm] BUG: the window's chrome was not drawn: {why}");
    }

    // A top-bar item given the keys by the controller (`Ctrl+k` with nothing above): the bar brought out first if
    // it is hidden, then the first item showing from that index on -- a tray chip, or `↻` when the tray is empty.
    {
        let top_items = top_items.clone();
        let window_modes = window_modes.clone();
        wm_host.set_top_bar(Rc::new(move |_, from: u32| {
            window_modes.reveal_top_bar();
            let visible: Vec<bool> = top_items
                .iter()
                .enumerate()
                .map(|(i, item)| i >= from as usize && item.is_visible())
                .collect();
            eitri_core::wm::titles::step_item(&visible, None, 1).is_some_and(|i| top_items[i].grab_focus())
        }));
    }
    // The rename entry given the keys by the controller (`workspace.rename`): the bar brought out first if it is
    // hidden, the entry shown so it can take them; the `Chrome` op after it fills the entry with the name.
    {
        let entry = top_bar.rename.clone();
        let window_modes = window_modes.clone();
        wm_host.set_rename_entry(Rc::new(move |_| {
            window_modes.reveal_top_bar();
            entry.set_visible(true);
            entry.grab_focus()
        }));
    }

    // A module key or tray chip focuses the oldest pending card in the chat if one
    // waits (`focus_oldest_card`). Otherwise it enters BROWSE, like other keyboard
    // arrivals. The page's `App.tsx` arrival effect selects the last row when the
    // reader was following it, or restores the row and scroll position they left.
    let arrive: Rc<dyn Fn(&ModuleId)> = {
        let agent_panel_handle = agent_panel_handle.clone();
        Rc::new(move |id| {
            // Into a chat view: the oldest card among that view's tabs (the base view switches to the
            // tab holding it first), else the view's own arrival.
            if let Some(view) = ViewId::of_module(id) {
                if !agent_panel_handle.focus_oldest_card(view) {
                    agent_panel_handle.arrive(view);
                }
            }
        })
    };

    // A card that arrives while its chat view is not on screen: a toast -- the
    // only sign in Immersive mode -- or, with `modules.chat.on_permission = reveal` and nothing
    // zoomed, that view back where it was, the keys staying where they are. Every view handed a card
    // reacts by its own place, so a card for a hidden view is not lost to one that arrived in a view on
    // screen in the same pump; the toast names the last of them to want one, the newest card's view.
    // The way back to each view, as the keymap binds it: the toast's last words.
    let way_backs: Vec<(ViewId, String, Option<String>)> = (1..=eitri_core::layout::module::MAX_INSTANCE)
        .filter_map(ViewId::new)
        .map(|view| {
            let title = module_title(&view.module());
            let way_back = view_way_back(&keymap, view, &title);
            (view, title, way_back)
        })
        .collect();
    {
        let module_layout = module_layout.clone();
        let grid = grid.clone();
        let driver = Rc::downgrade(&driver);
        let give_keys = give_keys.clone();
        let on_permission = config.on_permission;
        // What each chat view held when the controller was last told, so it hears only of the views that changed.
        let told: RefCell<Vec<(ViewId, Attention)>> = RefCell::default();
        // Cloned so the closure can call back into the handle for each view's attention and newest
        // card: the hook fires from the pump (`report_attention`) after its own state borrow is
        // already dropped, so this is safe and never re-enters a held `RefCell`.
        let agent_panel_handle_for_toast = agent_panel_handle.clone();
        let on_attention = move |_before: Attention, after: Attention, handed: &[ViewId]| {
            let agent = &agent_panel_handle_for_toast;
            let Some(driver) = driver.upgrade() else { return };
            // A view's chip and its strip entry carry its marks, which the controller's model draws: it is told
            // of each view whose attention moved, a card arriving or clearing changing no layout.
            let changed: Vec<ViewId> = {
                let mut told = told.borrow_mut();
                (1..=eitri_core::layout::module::MAX_INSTANCE)
                    .filter_map(ViewId::new)
                    .filter(|view| {
                        let now = agent.attention_of(*view);
                        match told.iter_mut().find(|(seen, _)| seen == view) {
                            Some((_, was)) if *was == now => false,
                            Some((_, was)) => {
                                *was = now;
                                true
                            }
                            None => {
                                told.push((*view, now));
                                true
                            }
                        }
                    })
                    .collect()
            };
            for view in changed {
                driver.post(HostEvent::Chat(eitri_core::wm::ChatEvent::Attention {
                    win: WinId::MAIN,
                    view,
                }));
            }
            let mut toast_for = None;
            for &view in handed {
                let chat = view.module();
                // A view under a float counts as away: its cards may be hidden there. Asked again for
                // each view, as revealing the one before may have moved the layout.
                let place = {
                    let layout = module_layout.borrow();
                    eitri_core::layout::view_place(&layout, &chat, &covered_views(&grid, &layout))
                };
                // The panel already judged that a card arrived in this view, so the view's attention is
                // weighed against none: `react` then asks only whether a card waits and where it is.
                let mine = agent.attention_of(view);
                let reaction = eitri_core::attention::react(on_permission, Default::default(), mine, place);
                println!("[attention] {chat} {mine:?} (window {after:?}, {place:?}) -> {reaction:?}");
                let mut toast_instead = false;
                if reaction.reveal && place == AgentPlace::Covered {
                    // Only scratch floats without the keys are put away; a float still over the view
                    // after that gets the toast the badge policy shows.
                    if reveal_covered_chat(&grid, &module_layout, view, &*give_keys) {
                        println!("[attention] a float still covers {chat}; a toast instead");
                        toast_instead = true;
                    }
                } else if reaction.reveal {
                    // Its tab stays behind when the keys are on a sibling tab of the same container
                    // (drawing the view's would hide them): the toast the badge policy shows is all
                    // there is then, as under a zoom.
                    match reveal_chat_in_grid(&grid, &module_layout, &chat) {
                        Ok(true) => {}
                        Ok(false) => {
                            println!("[attention] {chat}'s tab stays behind the keys' tab; a toast instead");
                            toast_instead = true;
                        }
                        Err(err) => eprintln!("[attention] could not reveal {chat}: {err}"),
                    }
                }
                if reaction.toast || toast_instead {
                    toast_for = Some((view, mine));
                }
            }
            if let Some((view, mine)) = toast_for {
                if let Some((_, title, way_back)) = way_backs.iter().find(|(v, _, _)| *v == view) {
                    // Kept up against refusals for its time: a stray key must not erase a waiting card's notice.
                    driver.post(HostEvent::Chat(eitri_core::wm::ChatEvent::Toast {
                        win: WinId::MAIN,
                        text: eitri_core::wm::toasts::permission_toast_text(
                            title,
                            mine,
                            agent
                                .newest_card_label_in(view)
                                .as_ref()
                                .map(|(n, name)| (*n, name.as_str())),
                            way_back.as_deref(),
                        ),
                        kind: ToastKind::Attention,
                    }));
                }
            }
        };
        panel_hooks.on_attention = Some(Box::new(on_attention));
    }
    // The one-line result of bringing the last window's tabs back.
    {
        let driver = Rc::downgrade(&driver);
        panel_hooks.on_toast = Some(Box::new(move |text: &str| {
            if let Some(driver) = driver.upgrade() {
                driver.post(HostEvent::Chat(eitri_core::wm::ChatEvent::Toast {
                    win: WinId::MAIN,
                    text: text.to_string(),
                    kind: ToastKind::Other,
                }));
            }
        }));
    }
    // The two ways an arm still in the shell brings a module to the user, each written once and shared by those
    // arms (a module key, a chip, a split key and `eitri.layout.*` are the controller's): `focus_and_arrive` and
    // `show_on_screen` below. Each returns the layout's refusal for its caller to report.
    //
    // A zoom ends only when it would keep the module off screen (`Layout::zoom_hides`): a focus onto the zoomed
    // module itself leaves it zoomed, as tmux's `select-pane` onto the zoomed pane does.
    //
    // `focus_and_arrive`: gives `id` the keys and arrives (`arrive`). Refused before anything moves
    // for a module the keys cannot go to (`Layout::can_focus`), so a refused focus does not end a
    // zoom on its way to being refused.
    let focus_and_arrive: Rc<dyn Fn(&ModuleId) -> Result<(), LayoutError>> = {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let give_keys = give_keys.clone();
        let arrive = arrive.clone();
        Rc::new(move |id| {
            let zoom_hides = {
                let layout = module_layout.borrow();
                layout.can_focus(id)?;
                layout.zoom_hides(id)
            };
            if zoom_hides {
                grid.unzoom();
            }
            if give_keys(id) {
                arrive(id);
            }
            Ok(())
        })
    };
    // A click on a strip entry chooses that tab and lands there as a module key would: the controller's.
    {
        let driver = Rc::downgrade(&driver);
        grid.connect_strip_activate(move |id| {
            if let Some(driver) = driver.upgrade() {
                driver.post(HostEvent::Pointer {
                    win: WinId::MAIN,
                    on: PointerTarget::StripEntry(id.clone()),
                    ev: PointerEv::Press {
                        x: 0.0,
                        y: 0.0,
                        button: 1,
                    },
                });
            }
        });
    }
    // A press on a float's bar or its grip, which raised the float already, gives it the keys as a strip entry
    // does. What the bar says is the chrome model's.
    {
        let focus_and_arrive = focus_and_arrive.clone();
        grid.connect_float_bar_press(move |id| {
            if let Err(err) = focus_and_arrive(id) {
                println!("[modules] {id}: its float bar chose nothing: {err}");
            }
        });
    }
    // A card answer from a chat view while a float covers that view is refused and the card stays waiting: the
    // answer would be for a card the user may not see. The controller's to judge, at the moment of each answer, for
    // the view whose page sent it; what it cannot judge it refuses, and its refusal is the toast.
    {
        let driver = Rc::downgrade(&driver);
        panel_hooks.set_answer_gate = Some(Rc::new(move |view: ViewId| {
            let Some(driver) = driver.upgrade() else {
                return Err(eitri_core::wm::feedback::LAYOUT_CHANGING_TEXT.to_string());
            };
            let answer = driver.query(move |c| c.answer_gate(WinId::MAIN, view));
            if let Err(text) = &answer {
                driver.post(HostEvent::AnswerRefused {
                    win: WinId::MAIN,
                    text: text.clone(),
                });
            }
            answer
        }));
    }
    // For attention a chat view under a float is off screen: a turn that finishes there is unread.
    // Asked by the panel each time it decides, for the view it judges; a layout being written right
    // then counts as covered, so the completion keeps its mark rather than being taken for seen.
    {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        agent_panel_handle.set_coverage(Rc::new(move |view: ViewId| match module_layout.try_borrow() {
            Ok(layout) => !view_covering(&grid, &layout, view).is_empty(),
            Err(_) => true,
        }));
    }
    // `show_on_screen` restores `id` where it was hidden and leaves keyboard focus
    // where it is. Lua show calls and module keys for hidden modules use this path.
    // End a zoom first if it would hide `id`, so a successful show puts it on screen.
    // A plugin that starts hidden and is absent from this workspace opens through
    // its own rule (`open_plugin_module`), leaving keyboard focus where it is.
    let show_on_screen: Rc<dyn Fn(&ModuleId) -> Result<(), LayoutError>> = {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let rules = rules.clone();
        let workspaces = workspaces.clone();
        let plugin_panels = plugin_panels.clone();
        let give_keys = give_keys.clone();
        Rc::new(move |id| {
            let (held, keys) = {
                let layout = module_layout.borrow();
                (layout.contains(id), layout.focus().clone())
            };
            if !held && plugin_panels.starts_hidden(id) {
                return open_plugin_module(
                    &grid,
                    &rules,
                    &workspaces,
                    &plugin_panels,
                    id,
                    Some(&keys),
                    Some(false),
                    &*give_keys,
                );
            }
            let zoom_hides = {
                let layout = module_layout.borrow();
                if !layout.contains(id) {
                    return Err(LayoutError::NotInTree(id.clone()));
                }
                layout.zoom_hides(id)
            };
            if zoom_hides {
                grid.unzoom();
            }
            grid.show_module(id).map(|_| ())
        })
    };
    // A plugin page's `layout.focus`: the same helpers the prefix uses, so a hidden editor is shown first.
    {
        let show_on_screen = show_on_screen.clone();
        let focus_and_arrive = focus_and_arrive.clone();
        plugin_panels.set_layout(Rc::new(move |id: &ModuleId| {
            show_on_screen(id).map_err(|e| e.to_string())?;
            focus_and_arrive(id).map_err(|e| e.to_string())
        }));
    }
    // The follow socket's lines: views and gone buffers to their plugins, and the plugins' nvim commands applied to
    // their modules in the order they were typed, the keys left where they are. The socket is read here, as the
    // service glue; what each command does to its module is the controller's.
    if let Some(socket) = follow_socket.as_mut() {
        let window = GridFollowWindow {
            driver: Rc::downgrade(&driver),
        };
        let plugin_panels = plugin_panels.clone();
        let one = window1_editor.clone();
        feed_pump.listen_follow(socket, move |batch| {
            // The lines for a page window 2 holds go there; nvim's side of them stays here.
            let batch = match &one {
                Some(one) => one.route_follow(batch),
                None => batch,
            };
            plugin_panels::apply_follow_commands(&window, plugin_panels.route_follow(batch, std::time::Instant::now()));
        });
    }
    // Window 2's pages follow window 1's editor through the lines window 1 sends them.
    if let Some(two) = &window2_editor {
        let window = GridFollowWindow {
            driver: Rc::downgrade(&driver),
        };
        let plugin_panels = plugin_panels.clone();
        two.on_follow(Rc::new(move |batch: eitri_core::plugin::follow_feed::FollowBatch| {
            plugin_panels::apply_follow_commands(&window, plugin_panels.route_follow(batch, std::time::Instant::now()));
        }));
    }
    // The scratch round trips (`Ctrl+g`, `gf`): the editor is shown and given the keys before nvim
    // is handed the request, and an edit that came back returns the keys to the chat. With no request, only the
    // editor shown and given the keys.
    let drive_editor: Rc<dyn Fn(Option<&eitri_core::scratch::ScratchRequest>) -> Result<(), String>> = {
        let pane = pane.clone();
        let show_on_screen = show_on_screen.clone();
        let give_keys = give_keys.clone();
        let module_layout = module_layout.clone();
        Rc::new(move |request| {
            // `prefix x` quit it, and it cannot come back in this window (`eitri_core::wm::close`).
            if module_layout.borrow().is_gone(&ModuleId::editor()) {
                return Err(LayoutError::Gone(ModuleId::editor()).to_string());
            }
            let Some(pane) = &pane else {
                return Err(linked_window::NO_EDITOR_HERE.to_string());
            };
            // Check `is_running` before sending keys: an exited nvim is also not ready, but keys sent to
            // it would reach nothing.
            if !pane.is_running() {
                return Err("the editor is not ready yet".to_string());
            }
            show_on_screen(&ModuleId::editor()).map_err(|e| e.to_string())?;
            give_keys(&ModuleId::editor());
            if let Some(request) = request {
                pane.send_keys(&request.input_keys());
            }
            Ok(())
        })
    };
    {
        let drive_editor = drive_editor.clone();
        let two = window2_editor.clone();
        let asking = window.downgrade();
        panel_hooks.on_editor_request = Some(Box::new(move |request: &eitri_core::scratch::ScratchRequest| {
            match &two {
                // Window 2's editor is window 1's: the request goes there, with a token that lets window 1 come
                // forward from this window, which has the keys.
                Some(two) => two.request(request, &|| activation_token(&asking)),
                None => drive_editor(Some(request)),
            }
        }));
    }
    // How either window of the pair comes forward from the other, the compositor's way.
    let pair_raise = PairRaise::new(config.companion_wm);
    // Window 2's editor requests, carried out here once window 1 has checked them: this window comes forward with
    // the token window 2 sent, then its editor is driven as for a request of its own.
    if let Some(one) = &window1_editor {
        let drive_editor = drive_editor.clone();
        let raised = window.downgrade();
        let pair_raise = pair_raise.clone();
        one.set_drive(Rc::new(
            move |act: linked_window::editor::EditorAct<'_>, token: Option<String>| {
                if let Some(raised) = raised.upgrade() {
                    pair_raise.raise(&raised, token.as_deref());
                }
                match act {
                    linked_window::editor::EditorAct::Request(request) => drive_editor(Some(request)),
                    linked_window::editor::EditorAct::Focus => drive_editor(None),
                }
            },
        ));
        // A draft's end goes back with a token from here, where the keys are while it is edited.
        let asking = window.downgrade();
        one.set_token_source(Rc::new(move || activation_token(&asking)));
        let panel = agent_panel_handle.clone();
        one.set_own_session(Rc::new(move |session: &str| panel.holds_session(session)));
        let panel = agent_panel_handle.clone();
        one.set_draft_here(Rc::new(
            move |session: Option<String>, result: eitri_core::scratch::EditDone| {
                // A chat that came here while its draft was out in nvim takes the edit's end as its own.
                let taken = session
                    .as_deref()
                    .is_some_and(|session| panel.finish_moved_draft_edit(session, &result));
                if !taken {
                    eprintln!(
                        "[scratch] a draft edit for {} ended, but no tab here has it out: {result:?}",
                        session.as_deref().unwrap_or("a chat")
                    );
                }
            },
        ));
    }
    if let Some(two) = &window2_editor {
        let panel = agent_panel_handle.clone();
        two.set_draft_tab(Rc::new(move |edit: u64| panel.draft_edit_tab(edit)));
        // A draft edited in window 1's editor ended there: this window comes forward, and its panel gives the keys
        // back to the chat.
        let raised = window.downgrade();
        let pair_raise = pair_raise.clone();
        two.set_raise(Rc::new(move |token: Option<String>| {
            if let Some(raised) = raised.upgrade() {
                pair_raise.raise(&raised, token.as_deref());
            }
        }));
    }
    // Turn review's questions to the editor, with the same two refusals: the editor quit for good,
    // or its nvim not running.
    {
        let editor_present: Rc<dyn Fn() -> bool> = {
            let module_layout = module_layout.clone();
            Rc::new(move || !module_layout.borrow().is_gone(&ModuleId::editor()))
        };
        // One handle on this window's editor, shared: the chat's turn review and every plugin page ask through it.
        let editor_rpc: Rc<dyn eitri_core::editor_rpc::EditorRpc> = match &pane {
            Some(pane) => Rc::new(editor_rpc::EmbeddedEditorRpc::new(pane.clone(), editor_present)),
            None => Rc::new(linked_window::NoEditor),
        };
        panel_hooks.set_editor_rpc = Some(editor_rpc.clone());
        if let Some(one) = &window1_editor {
            one.set_rpc(editor_rpc.clone());
        }
        plugin_panels.set_editor(editor_rpc);
    }
    // The review overlay in this window's own nvim: the module goes with the window, and a file the
    // review opened there is shown and given the keys, as a draft being edited is. Window 2 has no nvim: its
    // overlay is window 1's, and a file the review opened there is window 1's editor's to show, window 1 coming
    // forward with this window's token.
    {
        let own_editor = window2_editor.is_none();
        panel_hooks.set_review_owner = Some(Box::new(move || {
            own_editor.then_some(eitri_core::review_editor::Owner::Embedded)
        }));
        let give_keys = give_keys.clone();
        let show_on_screen = show_on_screen.clone();
        let two = window2_editor.clone();
        let asking = window.downgrade();
        panel_hooks.on_review_open = Some(Box::new(move || {
            if let Some(two) = &two {
                if let Err(err) = two.focus_editor(activation_token(&asking)) {
                    eprintln!("[review] could not bring the editor window forward: {err}");
                }
                return;
            }
            if let Err(err) = show_on_screen(&ModuleId::editor()) {
                eprintln!("[review] could not show the editor: {err}");
                return;
            }
            give_keys(&ModuleId::editor());
        }));
    }
    {
        let focus_and_arrive = focus_and_arrive.clone();
        // Back to the view the edit's tab is in; to the base view when that view has gone meanwhile.
        panel_hooks.on_editor_done = Some(Box::new(move |view: ViewId| {
            let returned = focus_and_arrive(&view.module()).or_else(|err| {
                if view.is_base() {
                    Err(err)
                } else {
                    focus_and_arrive(&ModuleId::agent())
                }
            });
            if let Err(err) = returned {
                eprintln!("[scratch] could not return the keys to the chat: {err}");
            }
        }));
    }
    // `prefix x`, tmux's `kill-pane`: every module's but the editor's is the controller's (`eitri_core::wm::close`
    // has the rules). The editor's kill is its quit, still carried out here, and tells the controller once the grid
    // has taken the editor off screen. The editor never comes back (`Reopen::Never`).
    // The editor closed for the rest of this window once nvim is gone (`Reopen::Never`: it cannot
    // come back, `eitri_core::wm::close`), the keys going to its neighbour, and the pane released on the next
    // turn of the loop so nothing reaches the closed connection. Refused as a hide is (the editor is
    // the last module on screen); the caller decides what then.
    // Every other workspace retires it too, since nvim is gone for all of them: where it was the only
    // module on screen there, the chat is shown first (`Workspaces::retire_everywhere`).
    let retire_editor: Rc<dyn Fn() -> Result<(), LayoutError>> = {
        let grid = grid.downgrade();
        let focus_module = focus_module.clone();
        let workspaces = workspaces.clone();
        let released = pane.as_ref().map(Rc::downgrade);
        let plugin_panels = plugin_panels.clone();
        let driver = Rc::downgrade(&driver);
        Rc::new(move || {
            let Some(grid) = grid.upgrade() else { return Ok(()) };
            grid.kill_module(&ModuleId::editor(), Reopen::Never, &*focus_module)?;
            // No editor is coming back to this window: a page following one says so.
            plugin_panels.editor_closed();
            grid.with_current_frame(|frame| workspaces.borrow_mut().retire_everywhere(&ModuleId::editor(), frame));
            if let Some(driver) = driver.upgrade() {
                driver.post(HostEvent::SurfaceEnded {
                    win: WinId::MAIN,
                    module: ModuleId::editor(),
                    why: Ended::Retired,
                });
            }
            println!("[modules] editor: nvim quit; the editor is closed for this window");
            let released = released.clone();
            glib::idle_add_local_once(move || {
                if let Some(pane) = released.and_then(|pane| pane.upgrade()) {
                    pane.release_exited();
                }
            });
            Ok(())
        })
    };
    // Every quit of the editor -- `prefix x` on it, every window close -- and what nvim's exit, a
    // cancel and a declined window close then do: `editor_quit`'s flow,
    // carried out here through `ShellQuitHost`, one GTK call per method.
    let editor_quitting = editor_quit::EditorQuitting::new(Rc::new(ShellQuitHost {
        pane: pane.clone(),
        module_layout: module_layout.clone(),
        grid: grid.downgrade(),
        show_on_screen: show_on_screen.clone(),
        focus_module: focus_module.clone(),
        give_keys: give_keys.clone(),
        retire_editor: retire_editor.clone(),
        prompt: close_prompt.clone(),
        agent: agent_panel_handle.clone(),
        terminals: Rc::downgrade(&terminals),
        window: window.clone(),
        refusal: refusal.clone(),
        toast: toast.clone(),
    }));
    // An extra chat view that holds no tab leaves the window: out of the layout (its keys handed on as a kill hands
    // them, before its host unmaps), out of every other workspace, and its page idle under its id for the next view
    // of that number. Its host stays in the grid, child-invisible. The controller hears it retired, so a module key
    // or a split no longer places it by name and the next view of its number takes its page back. `true` when
    // there was anything to remove.
    let drop_chat_view: Rc<dyn Fn(&ModuleId) -> Result<bool, LayoutError>> = {
        let grid = grid.downgrade();
        let module_layout = module_layout.clone();
        let focus_module = focus_module.clone();
        let workspaces = workspaces.clone();
        let chat_views = chat_views.clone();
        let agent = agent_panel_handle.clone();
        let driver = Rc::downgrade(&driver);
        Rc::new(move |id| {
            let (Some(grid), Some(view)) = (grid.upgrade(), ViewId::of_module(id)) else {
                return Ok(false);
            };
            if view.is_base() {
                return Ok(false);
            }
            let in_layout = module_layout.borrow().contains(id);
            if in_layout {
                grid.kill_module(id, Reopen::Remove, &*focus_module)?;
            }
            let elsewhere = {
                let layout = module_layout.borrow();
                let workspaces = workspaces.borrow();
                workspaces.each(&layout).iter().any(|(_, layout)| layout.contains(id))
            };
            workspaces.borrow_mut().remove_everywhere(id);
            let idled = chat_views.borrow_mut().retire(id);
            if idled {
                agent.idle_page(view);
            }
            // A restore still to come must not bring a tab back into a view that has left: its page is
            // idle, so the tab would show nowhere. The panel is told the views that remain.
            let remaining: Vec<ViewId> = {
                let layout = module_layout.borrow();
                let workspaces = workspaces.borrow();
                let layouts: Vec<&eitri_core::layout::Layout> =
                    workspaces.each(&layout).into_iter().map(|(_, layout)| layout).collect();
                extra_chat_views(&layouts)
                    .iter()
                    .filter_map(ViewId::of_module)
                    .collect()
            };
            agent.set_layout_views(remaining);
            if let Some(driver) = driver.upgrade() {
                driver.post(HostEvent::SurfaceEnded {
                    win: WinId::MAIN,
                    module: id.clone(),
                    why: Ended::Retired,
                });
            }
            Ok(in_layout || elsewhere || idled)
        })
    };
    // An extra view left with no tab -- its tab closed, joined back, or a failed restore discarded, and,
    // once the restore has settled, a view the layout held that no tab came back into -- leaves every
    // workspace. Removed on the loop's next turn, not inside the panel's own call: the panel may be in
    // the middle of its tick. A view given a tab again by then stays.
    {
        let chat_busy = chat_busy.clone();
        let drop_chat_view = drop_chat_view.clone();
        let give_keys = give_keys.clone();
        let show_on_screen = show_on_screen.clone();
        let module_layout = module_layout.clone();
        let host = Rc::downgrade(&wm_host);
        let agent = agent_panel_handle.clone();
        panel_hooks.on_view_empty = Some(Box::new(move |view: ViewId| {
            if view.is_base() || chat_busy.borrow().contains(&view) {
                return;
            }
            let drop_chat_view = drop_chat_view.clone();
            let give_keys = give_keys.clone();
            let show_on_screen = show_on_screen.clone();
            let module_layout = module_layout.clone();
            let host = host.clone();
            let agent = agent.clone();
            glib::idle_add_local_once(move || {
                if agent.views().contains(&view) {
                    return;
                }
                let id = view.module();
                // The view the only module on screen: its removal hides it first, as a kill does, and
                // the layout refuses to hide its last module, so the base view is shown beside it first
                // (the editor where the base view cannot be shown). Without this the empty view would
                // stay for good, since nothing asks again.
                let alone = eitri_core::layout::can_kill(&module_layout.borrow(), &id) == Ok(KillScope::Window);
                if alone {
                    let shown = show_on_screen(&ModuleId::agent()).or_else(|err| {
                        println!("[chat] {id}: the base view could not be shown ({err}); trying the editor");
                        show_on_screen(&ModuleId::editor())
                    });
                    if let Err(err) = shown {
                        println!("[chat] {id} holds no tab but is the only module on screen: {err}");
                        return;
                    }
                }
                // The keys in the view that goes: they go to the base view first, where a chat is.
                if host
                    .upgrade()
                    .and_then(|host| host.focused_module(WinId::MAIN))
                    .as_ref()
                    == Some(&id)
                {
                    give_keys(&ModuleId::agent());
                }
                match drop_chat_view(&id) {
                    Ok(true) => println!("[chat] {id} holds no tab; removed"),
                    Ok(false) => {}
                    Err(err) => println!("[chat] {id} holds no tab and could not be removed: {err}"),
                }
            });
        }));
    }
    // The two kills the controller hands back: the editor's, which is its quit, and the last module's, which is the
    // window close. nvim decides the editor's, through its own `:confirm qall` (`editor_quit::EditorQuitting::
    // kill_editor`): a running nvim is asked, an exited one retired, one that never ran hidden in place.
    let kill_now: Rc<dyn Fn(&ModuleId, Option<String>) -> Result<(), String>> = {
        let module_layout = module_layout.clone();
        let editor_quitting = editor_quitting.clone();
        let driver = Rc::downgrade(&driver);
        Rc::new(move |id, confirmed| {
            let scope = eitri_core::layout::can_kill(&module_layout.borrow(), id).map_err(|e| e.to_string())?;
            match (scope, id.kind()) {
                // nvim first, as for any kill of the editor: its exit closes the window.
                (KillScope::Window, ModuleKind::Editor) => editor_quitting.kill_editor(KillScope::Window, confirmed)?,
                (KillScope::Window, _) => editor_quitting.close_after(confirmed),
                (KillScope::Module, ModuleKind::Editor) => editor_quitting.kill_editor(KillScope::Module, None)?,
                // Asked as the window close, then something else came on screen before the `y` (a chip, a Lua
                // show): the `y` still kills what it asked about, now an ordinary kill, which is the controller's.
                (KillScope::Module, _) => {
                    let driver = driver.upgrade().ok_or("the window is closing")?;
                    println!("[modules] {id}: no longer the last module on screen; killed as any other is");
                    driver.kill_confirmed(WinId::MAIN, id);
                }
            }
            Ok(())
        })
    };
    // A chip is `prefix <key>` for its module from the top bar, which never holds the keys itself: the controller's
    // runner, by the chip's own rule (no gate, never a hide).
    {
        let driver = Rc::downgrade(&driver);
        tray.on_activate(move |id| {
            if let Some(driver) = driver.upgrade() {
                driver.run(WinId::MAIN, Action::Module(id.clone()), Origin::Chip);
            }
        });
    }
    // `focus.mode-toggle`, whose family is still the shell's, lands as every keyboard move between modules does (the
    // controller's `land_by_key`): a zoom ends first, as tmux's `select-pane` does, and a chat arrives where its
    // reader left it, on the last row if the reader was following it. `false`: the module did not take the keys.
    let land_by_key: Rc<dyn Fn(&ModuleId) -> bool> = {
        let grid = grid.clone();
        let give_keys = give_keys.clone();
        let agent_panel_handle = agent_panel_handle.clone();
        Rc::new(move |to| {
            grid.unzoom();
            let grabbed = give_keys(to);
            if let Some(view) = ViewId::of_module(to).filter(|_| grabbed) {
                agent_panel_handle.arrive(view);
            }
            grabbed
        })
    };

    // The page could not apply a `nav_key` against its own current state (the mirror was a keystroke stale, or
    // INPUT is refused on a dead session), so it asks for exactly what the chord would have done: the controller's.
    panel_hooks.on_nav_fallthrough = Some(Box::new({
        let driver = Rc::downgrade(&driver);
        move |view: ViewId, direction: Direction| {
            if let Some(driver) = driver.upgrade() {
                driver.post(HostEvent::Chat(eitri_core::wm::ChatEvent::NavFallthrough {
                    win: WinId::MAIN,
                    view,
                    direction,
                }));
            }
        }
    }));

    // The `?` sheet that `prefix ?` opened was closed, by its own keys or a click outside it unless `return_keys`
    // is false. The controller decides what goes back -- the keys, the hidden chat, the zoom -- from the opening it
    // kept (`driver.keymap_opened`, in the `prefix ?` arm), and only for the latest opening.
    panel_hooks.on_keymap_closed = Some(Box::new({
        let driver = Rc::downgrade(&driver);
        move |opening: u64, return_keys: bool| {
            if let Some(driver) = driver.upgrade() {
                driver.post(HostEvent::Chat(eitri_core::wm::ChatEvent::KeymapClosed {
                    win: WinId::MAIN,
                    view: ViewId::BASE,
                    opening,
                    forced: !return_keys,
                }));
            }
        }
    }));

    // What every terminal pane gets once, when it is made: what its shell's start and end do. Its own
    // `Ctrl+h/j/k/l` is one of its host's controllers, from `wire_host` (`wire`).
    //
    // Each start of a shell -- the first, a fresh one after a kill, and Enter after an exit -- and the
    // end of the pane's current shell are told to the controller, so its module lifecycle knows whether a
    // shell runs there: one shown again with a shell running gets no second one. The end is told before
    // what it does, so a kill it causes comes after it.
    //
    // The shell ending by itself -- `exit`, `Ctrl+d`, a signal -- closes that terminal as `prefix x`
    // does, without asking (`terminal::closes_on_exit` says which ends): the controller's kill, as a `y`
    // to its y/n would run it, which hangs the pane up as it goes. A report from a shell that is
    // no longer the pane's current one (the instance was killed and its number reused since) closes
    // nothing. As the last module on screen, tmux's own rule (closing the last pane closes the window)
    // applies instead: `eitri_core::layout::can_kill` says which (`terminal::on_exit`), and a `Window`
    // scope closes the window through its own ordinary path -- `close window? N running (y/n)` when a
    // tab runs -- rather than the module, leaving the terminal's notice on screen until the window
    // really goes.
    let wire_terminal: Rc<dyn Fn(&ModuleId, &terminal::TerminalPane)> = {
        let grid = grid.downgrade();
        let module_layout = module_layout.clone();
        let terminals = Rc::downgrade(&terminals);
        let window = window.clone();
        // Weak: the registry holds the panes, and the controller's host holds the registry.
        let driver = Rc::downgrade(&driver);
        Rc::new(move |id, pane| {
            {
                let id = id.clone();
                let driver = driver.clone();
                pane.on_shell_start(move |pid| {
                    if let Some(driver) = driver.upgrade() {
                        driver.post(HostEvent::Terminal(TermEvent::Started {
                            win: WinId::MAIN,
                            module: id.clone(),
                            pid,
                        }));
                    }
                });
            }
            {
                let id = id.clone();
                let grid = grid.clone();
                let module_layout = module_layout.clone();
                let terminals = terminals.clone();
                let window = window.clone();
                let driver = driver.clone();
                pane.on_shell_exit(move |reported| {
                    let Some(terminals) = terminals.upgrade() else {
                        // The window is going; nothing is left to close.
                        return false;
                    };
                    let current = match terminals.try_borrow() {
                        // An idle pane (`None`) is a killed instance: its shell's end is late too.
                        Ok(terminals) => terminals.get(&id).map(terminal::TerminalPane::generation),
                        Err(_) => {
                            eprintln!("[terminal] BUG: {id}: the registry was borrowed when its shell ended; it closes nothing");
                            return false;
                        }
                    };
                    if !current.is_some_and(|current| terminal::exit_is_current(reported, current)) {
                        println!("[terminal] {id}: the end of an earlier shell (start {reported}) arrived late; it closes nothing");
                        return false;
                    }
                    if let Some(driver) = driver.upgrade() {
                        driver.post(HostEvent::Terminal(TermEvent::Exited {
                            win: WinId::MAIN,
                            module: id.clone(),
                        }));
                    }
                    if grid.upgrade().is_none() {
                        return false;
                    }
                    let scope = match eitri_core::layout::can_kill(&module_layout.borrow(), &id) {
                        Ok(scope) => scope,
                        Err(err) => {
                            println!("[terminal] {id}: the shell ended, and the terminal stays: {err}");
                            return false;
                        }
                    };
                    match terminal::on_exit(scope) {
                        terminal::OnExit::CloseWindow => {
                            println!("[terminal] {id}: the shell ended; the last module on screen closes Eitri");
                            window.close();
                            false
                        }
                        // The kill's own hang-up closes the pane, so the pane is not closed a second time here.
                        terminal::OnExit::CloseModule => {
                            if let Some(driver) = driver.upgrade() {
                                println!("[terminal] {id}: the shell ended; the terminal is closed");
                                driver.kill_confirmed(WinId::MAIN, &id);
                            }
                            false
                        }
                    }
                });
            }
        })
    };
    {
        let panes: Vec<(ModuleId, terminal::TerminalPane)> = terminals
            .borrow()
            .live()
            .map(|(id, pane)| (id.clone(), pane.clone()))
            .collect();
        for (id, pane) in panes {
            wire_terminal(&id, &pane);
        }
    }

    // What every module host's controllers act through (`wire`): the startup hosts are wired
    // below, once the window's key handling exists, and a host made later is wired where it is made.
    let wire_deps = Rc::new(WireDeps {
        // The controller's module stage decides every key on its way into a module.
        module_stage: {
            let driver = Rc::downgrade(&driver);
            Rc::new(move |stage: &KeyStage, press| match driver.upgrade() {
                Some(driver) => driver.key(WinId::MAIN, stage, press),
                None => Verdict::PassThrough,
            })
        },
        text_size: text_size_controller.clone(),
    });
    // Whether a chat page keeps a navigation key for itself, which the controller asks while it decides that key:
    // the page's mode as it last reported it, read and nothing else.
    {
        let agent = agent_panel_handle.clone();
        wm_host.set_chat_claims(Rc::new(move |view: ViewId, direction: Direction| {
            agent.nav_claims(view, direction)
        }));
    }
    // What the chat's module key and a chat view's kill read of the panel, as it last counted: the view whose tabs
    // hold the oldest waiting card, and what a kill would end -- the base view's every tab, an extra view's one tab,
    // named on its y/n -- and whether its tabs can close now.
    {
        let agent = agent_panel_handle.clone();
        wm_host.set_oldest_card(Rc::new(move |_: WinId| agent.oldest_card_view()));
        let agent = agent_panel_handle.clone();
        wm_host.set_chat_kill_counts(Rc::new(move |view: ViewId| {
            let ((running, queued), tab) = if view.is_base() {
                ((agent.running_count(), agent.queued_count()), None)
            } else {
                (agent.view_running_queued(view), agent.view_tab_label(view))
            };
            eitri_core::wm::ChatKillCounts {
                running,
                queued,
                tab,
                blocked: agent.tabs_cannot_close(view).map(str::to_string),
            }
        }));
        let agent = agent_panel_handle.clone();
        wm_host.set_chat_first_tab(Rc::new(move |view: ViewId, breaks: bool| {
            if breaks {
                agent.can_break_out()
            } else {
                agent.can_open_in(view)
            }
        }));
    }
    // The controller's `CreateSurface` and `DropSurface`: the same constructors, a host made there wired
    // there, and what a kill or a retirement ends.
    wm_host.set_surface_maker(Rc::new(gtk_host_surfaces::SurfaceOps {
        surfaces: surfaces.clone(),
        wire_deps: wire_deps.clone(),
        wire_terminal: wire_terminal.clone(),
    }));
    // The view a page's own `tab_verb` names, for the one action it runs: that page's view is the keys'
    // view then, whatever holds the keys by the time the message is read. Set and taken around that one
    // action only.
    let tab_verb_view: Rc<std::cell::Cell<Option<ViewId>>> = Rc::default();
    // The prefix's `Action::Tab` chords and the panel's `tab_verb` messages use the
    // same code, so behavior is shared across both routes. `keymap.ts` resolves the
    // panel's `<leader>` bindings.
    // In an extra chat view, `tabs::view_plan` chooses that view's own tab or the base
    // view's tabs. Elsewhere, `tabs::plan` chooses the action.
    let run_tab_action: Rc<dyn Fn(TabAction)> = {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let host = Rc::downgrade(&wm_host);
        let agent = agent_panel_handle.clone();
        let give_keys = give_keys.clone();
        let show_on_screen = show_on_screen.clone();
        let refusal = refusal.clone();
        let tab_verb_view = tab_verb_view.clone();
        Rc::new(move |tab_action: TabAction| {
            let chat = ModuleId::agent();
            let keys = match tab_verb_view.take() {
                Some(view) => Some(view.module()),
                None => host.upgrade().and_then(|host| host.focused_module(WinId::MAIN)),
            };
            let keys_view = keys.as_ref().and_then(ViewId::of_module);
            // The base view shown and given the keys, for a verb that needs them there.
            let base_takes_the_keys = || match show_on_screen(&chat) {
                Ok(()) => {
                    give_keys(&chat);
                    true
                }
                Err(err) => {
                    refuse(&refusal, &err);
                    false
                }
            };
            // A new tab in the base view, typed into at once: an empty tab has nothing to browse.
            let new_base_tab = || {
                agent.new_tab();
                agent.enter_input(ViewId::BASE);
            };
            // A base-view tab chosen from elsewhere shows a hidden base view where it was, keys unmoved,
            // and puts away the scratch floats over a covered one; under a zoom only the tray chip
            // changes.
            let reveal_base = || {
                let place = {
                    let layout = module_layout.borrow();
                    eitri_core::layout::view_place(&layout, &chat, &covered_views(&grid, &layout))
                };
                if !eitri_core::tabs::reveal_on_switch(place) {
                    return;
                }
                if place == AgentPlace::Covered {
                    if reveal_covered_chat(&grid, &module_layout, ViewId::BASE, &*give_keys) {
                        println!("[tabs] a float still covers the chat");
                    }
                } else {
                    match reveal_chat_in_grid(&grid, &module_layout, &chat) {
                        Ok(true) => {}
                        Ok(false) => println!("[tabs] the chat's tab stays behind the keys' tab"),
                        Err(err) => eprintln!("[tabs] could not reveal the chat: {err}"),
                    }
                }
            };
            if let Some(plan) = keys_view.and_then(|view| eitri_core::tabs::view_plan(tab_action, view)) {
                let switched = match plan.verb {
                    TabVerb::Rename => {
                        agent.begin_rename(plan.page);
                        true
                    }
                    TabVerb::Close => {
                        // A `y` closes the view's one tab; the view, empty then, goes with it.
                        agent.confirm_close(plan.page);
                        true
                    }
                    TabVerb::Info => {
                        agent.open_detail(plan.page);
                        true
                    }
                    TabVerb::Choose => {
                        agent.open_chooser(plan.page);
                        true
                    }
                    TabVerb::New => {
                        if !base_takes_the_keys() {
                            return;
                        }
                        new_base_tab();
                        true
                    }
                    TabVerb::Step(_) | TabVerb::Last => {
                        let switched = match plan.verb {
                            TabVerb::Step(delta) => agent.step(delta),
                            _ => agent.select_last(),
                        };
                        if switched {
                            reveal_base();
                        }
                        switched
                    }
                    // A tab bound to a view: that view is shown and given the keys (`on_show_view`). A
                    // base-view tab: the base view is.
                    TabVerb::Select(n) => match agent.select_number(n) {
                        Some(eitri_core::tab_set::TabPlace::Base(_)) => {
                            // A refusal to show the base view has said so already.
                            base_takes_the_keys();
                            true
                        }
                        Some(eitri_core::tab_set::TabPlace::View(..)) => true,
                        None => false,
                    },
                    TabVerb::Flash
                    | TabVerb::CloseOthers
                    | TabVerb::Sibling(_)
                    | TabVerb::Terminal(_)
                    | TabVerb::Nothing => false,
                };
                if !switched {
                    flash(&refusal);
                }
                return;
            }
            let keys_in = keys.as_ref().map(ModuleId::kind);
            // `n`/`p` in a terminal: its sibling tabs when it sits in a tabbed or stacked container with
            // another tab to go to, else the terminals on screen, else nothing.
            let (terminals_on_screen, in_tabs) = {
                let layout = module_layout.borrow();
                let on_screen = layout
                    .modules()
                    .iter()
                    .filter(|id| id.kind() == ModuleKind::Terminal && layout.is_visible(id))
                    .count();
                let in_tabs = keys
                    .as_ref()
                    .is_some_and(|id| id.kind() == ModuleKind::Terminal && layout.tab_neighbour(id, true).is_some());
                (on_screen, in_tabs)
            };
            let plan = eitri_core::tabs::plan(tab_action, keys_in, terminals_on_screen, in_tabs);
            let step_to = |to: Option<ModuleId>| match to {
                Some(to) => {
                    let grabbed = give_keys(&to);
                    println!("[tabs] -> {to} (grab_focus={grabbed})");
                }
                None => flash(&refusal),
            };
            if plan.takes_the_keys && !base_takes_the_keys() {
                return;
            }
            let switched = match plan.verb {
                TabVerb::New => {
                    new_base_tab();
                    true
                }
                TabVerb::Step(delta) => agent.step(delta),
                TabVerb::Last => agent.select_last(),
                TabVerb::Select(n) => match agent.select_number(n) {
                    // A tab bound to a view: that view is shown and given the keys (`on_show_view`),
                    // and the base view is left as it is.
                    Some(eitri_core::tab_set::TabPlace::View(..)) => return,
                    Some(eitri_core::tab_set::TabPlace::Base(_)) => true,
                    None => false,
                },
                TabVerb::Rename => {
                    agent.begin_rename(ViewId::BASE);
                    true
                }
                TabVerb::Close => {
                    agent.confirm_close(ViewId::BASE);
                    true
                }
                TabVerb::CloseOthers => {
                    agent.confirm_close_others();
                    true
                }
                TabVerb::Choose => {
                    agent.open_chooser(ViewId::BASE);
                    true
                }
                TabVerb::Info => {
                    agent.open_detail(ViewId::BASE);
                    true
                }
                TabVerb::Flash => false,
                TabVerb::Sibling(delta) => {
                    let to = keys
                        .as_ref()
                        .and_then(|from| module_layout.borrow().tab_neighbour(from, delta > 0));
                    step_to(to);
                    return;
                }
                TabVerb::Terminal(delta) => {
                    let to = keys
                        .as_ref()
                        .and_then(|from| eitri_core::layout::next_terminal(&module_layout.borrow(), from, delta > 0));
                    step_to(to);
                    return;
                }
                TabVerb::Nothing => return,
            };
            if !switched {
                // tmux: "can't find window".
                flash(&refusal);
                return;
            }
            if !plan.takes_the_keys {
                reveal_base();
            }
        })
    };
    // A pick that names a tab bound to an extra view -- a number key, a chooser pick, a resume -- or a
    // resume from an extra page that opened a new base-view tab: that view is shown where it was hidden
    // from, given the keys, and arrived in.
    {
        let show_on_screen = show_on_screen.clone();
        let give_keys = give_keys.clone();
        let refusal = refusal.clone();
        let agent = agent_panel_handle.clone();
        panel_hooks.on_show_view = Some(Box::new(move |view: ViewId| {
            let id = view.module();
            if let Err(err) = show_on_screen(&id) {
                refuse(&refusal, &err);
                return;
            }
            if give_keys(&id) {
                agent.arrive(view);
            }
        }));
    }
    // What `scratchpad.show` says with nothing in the scratchpad: how a module gets there, as the
    // keymap binds `scratchpad.move`.
    let scratch_empty_text: Rc<str> = format!(
        "the scratchpad is empty ({} sends a module there)",
        key_text(&keymap, &Action::ScratchpadMove)
    )
    .into();
    // The shell's arms for the actions whose families are still carried out here. Every key action, every
    // `eitri.layout.run` and every panel tab verb goes through the controller's one runner, so a key and a
    // Lua call cannot drift apart; the runner meets the container selection's and the float's rules, decides
    // the families that are its own, and hands the rest here as a shell verb. The focus and keys verbs are the
    // controller's too: what holds the keys is read from GTK through the host, for the arms here that act on it.
    let run_verb: Rc<dyn Fn(Action)> = {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let editor = linked_window::OwnEditor(pane.clone());
        let agent = agent_panel_handle.clone();
        let host = Rc::downgrade(&wm_host);
        let wm = Rc::downgrade(&driver);
        let refusal = refusal.clone();
        let text_size_controller = text_size_controller.clone();
        let terminals = terminals.clone();
        let scratch_empty_text = scratch_empty_text.clone();
        let focus_module = focus_module.clone();
        let give_keys = give_keys.clone();
        let show_on_screen = show_on_screen.clone();
        let land_by_key = land_by_key.clone();
        let window_modes = window_modes.clone();
        let run_tab_action = run_tab_action.clone();
        let kill_now = kill_now.clone();
        let close_prompt = close_prompt.clone();
        let kill_title = module_title.clone();
        let kill_refused = refusal.clone();
        let keymap_plugins = plugin_panels.clone();
        let rules = rules.clone();
        let panel_decls = panel_decls.clone();
        Rc::new(move |action: Action| {
            // The module that last held the keys: what zoom, resize, hide and swap act on.
            let target = || module_layout.borrow().focus().clone();
            // The module that holds the keys right now, where a key typed now would go.
            let focused = || host.upgrade().and_then(|host| host.focused_module(WinId::MAIN));
            // A covered chat view revealed before something opens in it with the keys: the scratch floats over it
            // that do not hold the keys put away, and the refusal's text when a float still covers it. The
            // controller decides it; the floats it puts away go once this arm is done, before anything is drawn.
            let chat_covered = |view: ViewId| {
                wm.upgrade().map_or(
                    Some(eitri_core::wm::feedback::LAYOUT_CHANGING_TEXT.to_string()),
                    |driver| driver.reveal_chat(WinId::MAIN, view),
                )
            };
            match action {
                // The tiling verbs are the controller's (`eitri_core::wm::verbs::tiling`): its runner never
                // hands one to this arm.
                Action::MoveFar(_)
                | Action::ToggleSplit
                | Action::Rotate { .. }
                | Action::Flip(_)
                | Action::Spread
                | Action::Even(_)
                | Action::MainHorizontal
                | Action::MainVertical
                | Action::Tiled
                | Action::CycleLayout { .. }
                | Action::LayoutUndo
                | Action::MarkToggle
                | Action::MarkClear
                | Action::SwapMarked
                | Action::Zoom
                | Action::Resize { .. }
                | Action::Swap(_)
                | Action::ContainerTabbed
                | Action::ContainerStacked
                | Action::FocusParent
                | Action::FocusChild => eprintln!("[wm] BUG: the tiling verb {} reached the shell", action.name()),
                // The focus and keys verbs are the controller's (`eitri_core::wm::verbs::{focus, keys}`): its runner
                // never hands one to this arm.
                Action::SelectNumber
                | Action::Select(_)
                | Action::SelectLast
                | Action::SelectNext
                | Action::SendPrefix
                | Action::SendKeys(_)
                | Action::Hint
                | Action::CopyMode { .. } => {
                    eprintln!("[wm] BUG: the focus or keys verb {} reached the shell", action.name())
                }
                // The module verbs are the controller's (`eitri_core::wm::verbs::modules`): its runner hands this arm
                // only the editor's kill and the last module's, which is the window close. The split keys wait for a
                // module key in the prefix and never reach it either.
                Action::Module(_) | Action::ModuleHide | Action::InstanceNew { .. } | Action::Split { .. } => {
                    eprintln!("[wm] BUG: the module verb {} reached the shell", action.name())
                }
                Action::ModuleKill => {
                    let id = target();
                    let scope = match eitri_core::layout::can_kill(&module_layout.borrow(), &id) {
                        Ok(scope) => scope,
                        Err(err) => {
                            refuse(&refusal, &err);
                            return;
                        }
                    };
                    let (running, queued) = (agent.running_count(), agent.queued_count());
                    // Counted now, as the window close counts them, so the close compares like with like.
                    let jobs = terminal::jobs::count_terminal_jobs(&terminals.borrow());
                    let text = eitri_core::wm::close::prompt(&id, &kill_title(&id), scope, running, queued, jobs);
                    // What `y` also answers for the window close, if this kill closes it.
                    let confirmed = eitri_core::tabs::window_close_prompt_with_jobs(running, queued, jobs);
                    println!("[modules] {id}: {text}");
                    let kill_now = kill_now.clone();
                    let kill_refused = kill_refused.clone();
                    close_prompt.ask(&text, move || {
                        if let Err(err) = kill_now(&id, confirmed) {
                            println!("[modules] {id}: not killed: {err}");
                            flash(&kill_refused);
                        }
                    });
                }
                Action::Text(change) => match text_size::route_focused_module(focused().as_ref()) {
                    text_size::TextSizeTarget::Editor => text_size_controller.apply_editor(change.into()),
                    text_size::TextSizeTarget::Panel => text_size_controller.apply_panel(change.into()),
                    text_size::TextSizeTarget::Page(page) => text_size_controller.apply_page(&page, change.into()),
                    text_size::TextSizeTarget::Neither => {}
                },
                // `r`, `?` and `:` act in the page of the chat view with the keys; from anywhere else, in
                // the base view's, which comes up with the keys for the two that read them.
                Action::PanelReload => agent
                    .reload_document_by_hand(focused().as_ref().and_then(ViewId::of_module).unwrap_or(ViewId::BASE)),
                Action::PanelKeymap => {
                    // Read before the chat takes the keys: afterwards the chat is what holds them.
                    let from = focused();
                    let plugin = from
                        .clone()
                        .and_then(|module| keymap_plugins.declared_rows(&module))
                        .map(|(title, rows)| eitri_core::plugin::keys::plugin_help(&title, rows));
                    // A chat view that holds the keys opens the sheet in its own page; from anywhere else it
                    // opens in the base view's, which takes the keys.
                    let view = from.as_ref().and_then(ViewId::of_module).unwrap_or(ViewId::BASE);
                    let chat = view.module();
                    // What showing the chat is about to change, so closing the sheet can undo it. The zoom
                    // is kept whole, a container's as well as a module's.
                    let (zoom_before, showed_sheet) = {
                        let layout = module_layout.borrow();
                        (layout.zoom().cloned(), !layout.is_shown(&chat))
                    };
                    if let Err(err) = show_on_screen(&chat) {
                        refuse(&refusal, &err);
                        return;
                    }
                    if let Some(text) = chat_covered(view) {
                        // Nothing opens, so what showing the chat changed is put back.
                        if showed_sheet {
                            if let Err(err) = grid.hide_module(&chat, &*focus_module) {
                                eprintln!("[keys_return] could not hide {chat} again: {err}");
                            }
                        }
                        if let (Some(zoom), Some(keeps)) = (&zoom_before, &from) {
                            if module_layout.borrow().zoom().is_none() {
                                grid.restore_zoom(zoom, keeps);
                            }
                        }
                        refuse_text(&refusal, &text);
                        return;
                    }
                    // Showing the chat ended a zoom that hid it; giving it the keys below chooses its tab if
                    // that tab is not drawn, which ends any zoom left (a container's that holds the chat).
                    let ended_zoom = {
                        let layout = module_layout.borrow();
                        if layout.zoom().is_none() || layout.is_tabbed_away(&chat) {
                            zoom_before
                        } else {
                            None
                        }
                    };
                    let opening = wm.upgrade().map_or(0, |driver| {
                        driver.keymap_opened(
                            WinId::MAIN,
                            view,
                            KeysOpened {
                                from,
                                ended_zoom,
                                showed_sheet,
                            },
                        )
                    });
                    give_keys(&chat);
                    agent.open_keymap(view, plugin, opening);
                }
                // `prefix :` opens the chat with focus in its command line. Opening the line runs nothing, so
                // letters typed after the chord go into the entry instead of acting as panel keys.
                Action::PanelCommandLine => {
                    // A chat view that holds the keys opens the line in its own page; from anywhere else it
                    // opens in the base view's, which takes the keys.
                    let view = focused().as_ref().and_then(ViewId::of_module).unwrap_or(ViewId::BASE);
                    let chat = view.module();
                    if let Err(err) = show_on_screen(&chat) {
                        refuse(&refusal, &err);
                        return;
                    }
                    if let Some(text) = chat_covered(view) {
                        refuse_text(&refusal, &text);
                        return;
                    }
                    // A close of the `?` sheet may still be on its way from the page: the command line is
                    // the newer request, and that close must not move the keys out from under it.
                    if let Some(driver) = wm.upgrade() {
                        driver.keymap_superseded(WinId::MAIN);
                    }
                    give_keys(&chat);
                    agent.open_command_line(view);
                }
                Action::WindowImmersive => window_modes.toggle_immersive(),
                // `run_tab_action` is shared with the panel's `tab_verb` messages.
                // It computes `keys_in` the same way `focused()` does here.
                Action::Tab(tab_action) => run_tab_action(tab_action),
                // i3's `move <dir>` with the keys on a float: it moves by `FLOAT_MOVE_CELLS` editor cells that
                // way, as `M-arrow` resizes a tile. A tile's or a selected container's move is the controller's.
                Action::Move(dir) => {
                    let target = target();
                    if !module_layout.borrow().is_floating(&target) {
                        eprintln!("[wm] BUG: move.{dir:?} on the tile {target} reached the shell");
                        return;
                    }
                    let cell = editor.cell_size().unwrap_or(layout::FALLBACK_CELL);
                    if !grid.move_float(&target, dir, layout::resize_px(dir, cell, FLOAT_MOVE_CELLS)) {
                        println!("[modules] {target}: the float is against the {dir:?} edge");
                    }
                }
                // i3's `floating toggle`: a tile floats; a float sinks beside the most recent tile in
                // view. The keys stay with the module, which moves between the grid's bands. A tile that
                // has never floated takes its open rule's float size, if the rule gives one; one that has
                // floats where it last did.
                Action::FloatToggle => {
                    let target = target();
                    let never_floated = {
                        let layout = module_layout.borrow();
                        !layout.is_floating(&target) && !layout.float_memory().contains_key(&target)
                    };
                    let first_size = if never_floated {
                        rule_float_size(&rules, &panel_decls, &target)
                    } else {
                        None
                    };
                    let floated = match first_size {
                        Some(size) => {
                            let rect = grid.with_current_frame(|frame| {
                                eitri_core::layout::rules::float_rect(Some(size), frame.size)
                            });
                            grid.float_at(&target, Some(rect))
                                .map(|_| eitri_core::layout::FloatChange::Floated)
                        }
                        None => grid.float_toggle(&target),
                    };
                    match floated {
                        Ok(change) => println!("[modules] {target}: {change:?}"),
                        Err(err) => refuse(&refusal, &err),
                    }
                }
                // i3's `focus mode_toggle`: the keys to the most recent module in view on the other
                // layer, arriving as every keyboard arrival does. The target is chosen in view, not on
                // screen, so a zoomed float still finds the tile beneath it: the landing ends the zoom
                // first, or the keys would go to a tile left unmapped while the float stays full window.
                Action::FocusModeToggle => {
                    let to = eitri_core::layout::mode_toggle_target(&module_layout.borrow());
                    match to {
                        Some(id) => {
                            let grabbed = land_by_key(&id);
                            println!("[modules] focus.mode-toggle -> {id} (grab_focus={grabbed})");
                        }
                        None => {
                            let text = "focus.mode-toggle: nothing in view on the other layer";
                            println!("[modules] {text}");
                            refuse_said(&refusal, text);
                        }
                    }
                }
                Action::ScratchpadMove => {
                    let target = target();
                    if let Err(err) = grid.scratchpad_move(&target, &*focus_module) {
                        refuse(&refusal, &err);
                    }
                }
                // i3's `scratchpad show`. A named terminal instance or chat view that does not exist yet is made
                // straight into the scratchpad and shown, by the controller, as every new instance is made.
                Action::ScratchpadShow { module } => match grid.scratchpad_show(module.as_ref(), &*give_keys) {
                    Ok(ScratchShow::Missing(id)) => {
                        println!("[modules] {id}: not made yet; the controller makes it for the scratchpad");
                        if let Some(driver) = wm.upgrade() {
                            driver.scratch_new(WinId::MAIN, &id);
                        }
                    }
                    Ok(change) => println!("[modules] scratchpad.show: {change:?}"),
                    Err(LayoutError::NoScratch) => refuse_text(&refusal, &scratch_empty_text),
                    Err(err) => refuse(&refusal, &err),
                },
                // The workspace verbs are the controller's (`eitri_core::wm::verbs::workspaces`): its runner never
                // hands one to this arm.
                Action::WorkspaceStep { .. }
                | Action::WorkspaceLast
                | Action::WorkspaceNew
                | Action::WorkspaceSwitch { .. }
                | Action::WorkspaceChoose
                | Action::WorkspaceRename => {
                    eprintln!("[wm] BUG: the workspace verb {} reached the shell", action.name())
                }
                // tmux's `break-pane`, on the chat view with the keys: from the base view its active tab
                // goes out into a new view right of it; from an extra view that view's tab comes back
                // into the base view, and the view, empty then, goes (`on_view_empty`).
                Action::TabBreak => match break_plan(focused().as_ref().and_then(ViewId::of_module)) {
                    // Where the open rules say for a broken-out tab: by default right of the base view,
                    // where the key was pressed.
                    // Asked first, so a break that cannot happen makes no view: the controller makes the view and
                    // breaks the tab out into it.
                    BreakPlan::Break => match agent.can_break_out() {
                        Ok(()) => {
                            if let Some(driver) = wm.upgrade() {
                                driver.tab_break(WinId::MAIN);
                            }
                        }
                        Err(text) => refuse_text(&refusal, &text),
                    },
                    BreakPlan::Join(view) => {
                        // Asked before anything moves: a view with no tab has nothing to join, and the
                        // keys would otherwise be left in the base view for a refusal.
                        if !agent.views().contains(&view) {
                            refuse_text(&refusal, &format!("agent {} shows no tab", view.number()));
                            return;
                        }
                        // The keys leave the view before it goes, into the base view the tab comes back to.
                        let chat = ModuleId::agent();
                        if let Err(err) = show_on_screen(&chat) {
                            refuse(&refusal, &err);
                            return;
                        }
                        give_keys(&chat);
                        match agent.join_back(view) {
                            Ok(_) => {
                                agent.arrive(ViewId::BASE);
                                println!("[chat] {}: its tab joined the base view", view.module());
                            }
                            Err(text) => refuse_text(&refusal, &text),
                        }
                    }
                    BreakPlan::Refuse => {
                        let text = format!("{}: the keys are in no chat view", action.name());
                        println!("[prefix] {text}");
                        refuse_said(&refusal, &text);
                    }
                },
                Action::WindowNew
                | Action::WindowOther
                | Action::MoveToWindow { .. }
                | Action::WorkspaceToWindow { .. }
                | Action::WorkspaceSwapWindow => {
                    eprintln!("[wm] BUG: the window verb {} reached the shell", action.name())
                }
            }
        })
    };

    // What the runner hands back: the arm of every shell verb, called by the driver after the batch it came
    // with, the controller holding no borrow.
    {
        let run_verb = run_verb.clone();
        wm_host.set_shell_verb_arm(Box::new(move |_, call: &ShellCall, _keycode: u32| match call {
            ShellCall::Action(action) => run_verb(action.clone()),
        }));
    }
    // `<leader>bd`/`<leader>bo` etc., resolved by the panel's own sequence engine and sent as a
    // `tab_verb` message -- routed through the runner the prefix's `Action::Tab` chord goes through, the
    // container selection's rules included: a tab verb ends a selection whichever route sent it. The run is
    // carried out before it returns: a page message is never read while a batch applies, so the driver is
    // free, and the shell verb's arm reads the view while it is set.
    panel_hooks.on_tab_verb = Some(Box::new({
        let driver = driver.clone();
        let tab_verb_view = tab_verb_view.clone();
        move |view: ViewId, tab_action: TabAction| {
            tab_verb_view.set(Some(view));
            driver.run(WinId::MAIN, Action::Tab(tab_action), Origin::PanelTab);
            // A gate that refused the action never reached `run_tab_action`, which takes it.
            tab_verb_view.set(None);
        }
    }));
    // Every hook is built: the panel gets them all at once. Nothing since the first was built can
    // have made the panel call one, since the window is still being built and no page message is
    // read before the main loop runs again.
    panel_wiring::wire_panel(&agent_panel_handle, panel_hooks);

    // The window's key layers, in their declared order (`gtk_host::keys::window_layers`): HINT's while a HINT
    // runs, then the y/n prompt's while it is open, then the prefix's, which hands every other key to the
    // controller's window stage. Installed after every other window-level controller that exists at startup, so
    // they see keys first; the order among them is the one function's, not this block's place in `build_ui`.
    gtk_host::keys::install_window_layers(window.upcast_ref(), &close_prompt, Rc::downgrade(&driver));
    // A declined window close may follow nvim exiting on its own (`:q` or a crash). Retire the
    // exited editor through `editor_quit::EditorQuitting::window_close_declined`.
    {
        let editor_quitting = editor_quitting.clone();
        close_prompt.connect_window_close_declined(move || editor_quitting.window_close_declined());
    }

    // --- From the editor: decided by Neovim itself, or a cancelled `:confirm qall` clearing its
    // own kill.
    //
    // This half deliberately has no key handler at all for the direction case. The embedded nvim
    // received `TMUX`, `TMUX_PANE` and a fake-`tmux`-carrying `PATH` at spawn time (see
    // `pane_switch`), so the user's real `vim-tmux-navigator` runs its own `wincmd` first and only
    // calls out to "tmux" once the cursor is at a genuine Neovim window boundary -- which is what
    // reaches us here as a letter. Real `:vsplit` navigation therefore keeps working untouched;
    // `shell` never sees the keypresses that Neovim resolved internally.
    if let Some(ps) = pane_switch.as_mut() {
        let driver = Rc::downgrade(&driver);
        let editor_quitting = editor_quitting.clone();
        feed_pump.listen_pane_switch(ps, move |message| match message {
            // Where the keys go from the editor's edge is the controller's.
            pane_switch::PaneMessage::Direction(letter) => match pane_switch::letter_direction(letter) {
                Some(direction) => {
                    if let Some(driver) = driver.upgrade() {
                        driver.post(HostEvent::Editor(eitri_core::wm::EditorEvent::Navigate {
                            win: WinId::MAIN,
                            direction,
                        }));
                    }
                }
                None => println!("[pane_switch] unknown direction {letter:?}, ignoring"),
            },
            pane_switch::PaneMessage::QuitCancelled(g) => match editor_quitting.cancelled(g) {
                editor_quit::Cancel::Cleared => {
                    // The window close's `y`, if it had one, was already taken back when the close
                    // handed the quit to nvim (`EditorQuitting::window_close`), so a later exit asks.
                    println!("[modules] editor: nvim's quit {g} was cancelled; nothing is in flight");
                }
                editor_quit::Cancel::CloseWithdrawn => println!(
                    "[modules] editor: nvim's quit {g}, a window close a later kill joined, was cancelled; \
                     nvim's exit retires the editor only"
                ),
                editor_quit::Cancel::Ignored => {
                    println!("[modules] editor: a quit-cancelled letter for quit {g}, not the one in flight; ignored");
                }
            },
        });
    }

    // --- Every host the window opens with -- the editor, each terminal, each chat view and each Lua
    // panel, a plugin's page among them, built yet or not -- gets its controllers here, once: the keys'
    // routes, the Super filter and `Ctrl`+wheel zoom, as its kind gets them (`wire`). A host
    // made later is wired where it is made (`gtk_host_surfaces::SurfaceOps`, the controller's `CreateSurface`).
    {
        let plugin_modules = plugin_panels.modules();
        for (id, host) in grid.hosts() {
            let Some(kind) = WireKind::of_module(&id, plugin_modules.contains(&id)) else {
                eprintln!("[modules] BUG: {id} has a host, but nothing wires a host of its kind");
                continue;
            };
            wire_host(kind, &id, &host, &wire_deps);
        }
    }

    // --- The top bar, from the keyboard. `h`/`l` move between its items, `Enter`/`Space`
    // activate one (a GTK button's own behaviour), and `Ctrl+j` or `Esc` go back to the pane that
    // last had the keys. Capture phase on the bar, so the chords are seen before a focused button
    // gets them. Only the items `build_top_bar` returned are reachable: the window controls are
    // not focusable at all, so `Enter` here can never close the window.
    // While a rename is typed, every key is the entry's (its own `Esc` cancels it).
    {
        let top_items = top_items.clone();
        let return_to_pane = return_to_pane.clone();
        let window = window.clone();
        let hint_coordinator = hint_coordinator.clone();
        let rename = top_bar.rename.clone();
        let controller = EventControllerKey::new();
        controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        controller.connect_key_pressed(move |_controller, key, _keycode, state| {
            let focus = gtk4::prelude::GtkWindowExt::focus(&window);
            if focus.as_ref().is_some_and(|w| *w == rename || w.is_ancestor(&rename)) {
                return glib::Propagation::Proceed;
            }
            let ctrl = state.contains(ModifierType::CONTROL_MASK);
            if (key == Key::j && ctrl) || key == Key::Escape {
                return_to_pane();
                return glib::Propagation::Stop;
            }
            if ctrl {
                return glib::Propagation::Proceed;
            }
            // `f` here starts the global HINT, as it does in the panel's BROWSE.
            if key == Key::f {
                hint_coordinator.toggle();
                return glib::Propagation::Stop;
            }
            let step: isize = match key {
                Key::h | Key::Left => -1,
                Key::l | Key::Right => 1,
                _ => return glib::Propagation::Proceed,
            };
            // Over the items that are showing: a tray chip whose module is on screen is stepped over.
            let current = top_items.iter().position(|item| Some(item) == focus.as_ref());
            let visible: Vec<bool> = top_items.iter().map(|item| item.is_visible()).collect();
            if let Some(next) = eitri_core::wm::titles::step_item(&visible, current, step) {
                top_items[next].grab_focus();
            }
            glib::Propagation::Stop
        });
        top_bar.widget.add_controller(controller);
    }

    // The workspace name in the top bar, clicked or activated from the keyboard: the chooser, as
    // `workspace.choose` opens it; the controller's prefix waits for its key.
    {
        let driver = Rc::downgrade(&driver);
        top_bar.workspace.connect_clicked(move |_| {
            if let Some(driver) = driver.upgrade() {
                println!("[prefix] the chooser, from the top bar's workspace name");
                driver.await_workspace(WinId::MAIN);
            }
        });
    }

    // The rename entry says what happened to it -- `Enter` with its text, `Esc`, the keys leaving it -- and the
    // controller decides the rename, where the keys go back to, and when the entry closes (its chrome's `rename`).
    {
        let driver = Rc::downgrade(&driver);
        chrome::wire_rename_entry(
            &top_bar.rename,
            Rc::new(move |event| {
                if let Some(driver) = driver.upgrade() {
                    driver.post(event);
                }
            }),
        );
    }

    // nvim exiting on its own (e.g. `:qa!`) has no window to close by itself -- ask this host's
    // window to close, which in turn drives the connect_close_request handler below (mirrors
    // neovide-editor's own `examples/standalone.rs`), unless nvim was asked to quit: then what it was
    // asked for (`editor_quit::EditorQuitting::nvim_exited`).
    if let Some(pane) = &pane {
        let editor_quitting_for_exit = editor_quitting.clone();
        let review_for_exit = agent_panel_handle.clone();
        let plugin_panels = plugin_panels.clone();
        pane.on_exited_unrequested(move || {
            review_for_exit.review_editor_lost();
            // Before the exit is acted on, which may close the window: a page following the editor hears it ended.
            plugin_panels.editor_closed();
            editor_quitting_for_exit.nvim_exited();
        });
        let editor_quitting = editor_quitting.clone();
        let review_for_unreachable = agent_panel_handle.clone();
        pane.on_nvim_unreachable(move || {
            review_for_unreachable.review_editor_lost();
            editor_quitting.nvim_unreachable();
        });
    }

    // Show nvim startup errors in the editor's area through `editor_start_failure`. Startup is
    // asynchronous: `LiveState` moves from `NotStarted` through `Starting` to `Ready` or `Failed`
    // across render callbacks, after the window is shown and initial focus is chosen. If the
    // failed editor holds the keys, move them to the chat so input is not trapped. This is the
    // same fallback used when a window close is declined after the editor exits. Returning to the
    // failed editor still allows Ctrl+h/j/k/l navigation out.
    if let (Some(pane), Some(editor_start_failure_label)) = (&pane, &editor_start_failure_label) {
        let host = Rc::downgrade(&wm_host);
        let focus_and_arrive = focus_and_arrive.clone();
        let show_on_screen = show_on_screen.clone();
        let editor_start_failure_label = editor_start_failure_label.clone();
        pane.on_start_failed(move |message| {
            editor_start_failure::show(&editor_start_failure_label, message);
            let Some(host) = host.upgrade() else { return };
            let focused = host.focused_module(WinId::MAIN);
            if editor_start_failure::keys_after_failure(focused.as_ref())
                == editor_start_failure::KeysAfterFailure::Stay
            {
                // The keys were already elsewhere (or nowhere) when nvim failed: leave them there.
                return;
            }
            if let Err(err) = show_on_screen(&ModuleId::agent()) {
                println!("[modules] editor: start failed, and the chat could not be shown ({err})");
            }
            if let Err(err) = focus_and_arrive(&ModuleId::agent()) {
                println!("[modules] editor: start failed, and the keys could not move to the chat ({err})");
            }
        });
    }

    // `eitri.layout.*` from a command or an event handler: queued by the Lua half while the handler runs, carried
    // out here once it returns, so no layout change happens under a Lua call. Each refusal is logged, as a handler's
    // own error is. A request whose family the controller decides (`eitri_core::wm::lua::moved`: every one but a
    // float's) goes to it whole, its gate and its log line included; `run` meets the
    // runner's rules, as a key does.
    // What a Lua request refused under a container selection logs: how to get back to one module.
    let one_module_text: Rc<str> = eitri_core::wm::verbs::one_module_text(&keymap).into();
    let apply_layout_requests: Rc<dyn Fn()> = {
        let lua_engine = lua_engine.clone();
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let driver = driver.clone();
        let one_module_text = one_module_text.clone();
        Rc::new(move || {
            let requests = lua_engine.layout.borrow_mut().take_requests();
            // A selection a request ended changes no host, so no change hook tells the controller of it.
            let mut selection_reset = false;
            for request in requests {
                use eitri_core::lua::layout::LayoutRequest;
                if eitri_core::wm::lua::moved(&request) {
                    driver.lua_request(WinId::MAIN, request);
                    continue;
                }
                println!("[lua] layout: {request:?}");
                // The container selection's rules by the action each request stands for.
                let gate = eitri_core::wm::lua::lua_request_action(&request)
                    .map(|action| selection_gate(&action, module_layout.borrow().selection()));
                if gate == Some(SelectionGate::Refuse) {
                    eprintln!("[lua] layout: {request:?} refused: {one_module_text}");
                    continue;
                }
                let result = match &request {
                    LayoutRequest::Float(id, rect) => grid.float_at(id, *rect).map(|_| ()),
                    LayoutRequest::Switch(_)
                    | LayoutRequest::Run(_)
                    | LayoutRequest::Mark(_)
                    | LayoutRequest::Open(..)
                    | LayoutRequest::Show(_)
                    | LayoutRequest::Hide(_)
                    | LayoutRequest::Focus(_)
                    | LayoutRequest::Split(..) => {
                        eprintln!("[lua] BUG: {request:?} is the controller's and reached the shell's loop");
                        Ok(())
                    }
                };
                if let Err(err) = result {
                    eprintln!("[lua] layout: {request:?} refused: {err}");
                }
                if gate == Some(SelectionGate::Run { reset: true }) {
                    selection_reset |= clear_selection(&module_layout);
                }
            }
            if selection_reset {
                driver.post(HostEvent::LayoutChangedByShell { win: WinId::MAIN });
            }
        })
    };

    // Wire every Lua-registered command to a real GTK action, right before the window is shown:
    // GTK actions can be added before or after `present()`, so placement here is only for
    // clarity. Each command gets a `gio::SimpleAction` named `cmd-<id>` on `app`, whose
    // `activate` signal invokes the command back through the Lua engine; commands that declared
    // a `keybinding` also get a real accelerator bound via `set_accels_for_action`.
    for (id, entry) in lua_engine.commands.borrow().iter() {
        let action_name = format!("cmd-{id}");
        let action = gtk4::gio::SimpleAction::new(&action_name, None);
        let id_owned = id.clone();
        let lua_engine_for_action = lua_engine.clone();
        let apply_layout_requests = apply_layout_requests.clone();
        action.connect_activate(move |_, _| {
            lua_engine_for_action.invoke_command(&id_owned);
            apply_layout_requests();
        });
        app.add_action(&action);
        if let Some(keybinding) = &entry.keybinding {
            let accel = gtk_host::keys::registered_accel(keybinding);
            app.set_accels_for_action(&format!("app.{action_name}"), &[&accel]);
        }
    }

    // The layout's first focus: where the keys were when this project's last arrangement change was
    // written, from
    // its state file (`eitri_core::wm::persist`'s module doc); else the editor, or a Lua `main` panel that hid
    // it (`Layout::initial`, `reconcile`). Read BEFORE the window is shown: GTK4's `gtk_window_show` moves
    // focus Tab-forward when there is none, and were that ever to land inside a module, the controller's focus
    // report would rewrite the layout's `focus` before this line read it. Today a top-bar item (a tray chip,
    // or `↻`) is the first focusable widget, so it never has.
    let first = module_layout.borrow().focus().clone();
    // The probe starts before the present phase, whose show and first focus are one batch: it runs nothing that
    // needs the window shown, and the tabs it opens in the startup chat views still exist before the first focus
    // arrives in one, as when it started between the two.
    // The probe drives a window's own editor, so it runs only where there is one.
    #[cfg(feature = "probe")]
    if let Some(pane) = &pane {
        grid_probe::start_if_requested(grid_probe::Context {
            app: app.clone(),
            window: window.clone(),
            grid: grid.clone(),
            pane: pane.clone(),
            terminal: terminals
                .borrow()
                .get(&ModuleId::terminal())
                .cloned()
                .expect("the base terminal keeps its pane in the registry"),
            agent: agent_widget.clone(),
            project_root: project_root.to_path_buf(),
            panel: agent_panel_handle.clone(),
            new_chat_view: {
                let driver = Rc::downgrade(&driver);
                let module_layout = module_layout.clone();
                Rc::new(move || {
                    let views = |layout: &eitri_core::layout::Layout| -> Vec<ModuleId> {
                        layout
                            .modules()
                            .into_iter()
                            .filter(|id| id.kind() == ModuleKind::Agent && id.number() >= 2)
                            .collect()
                    };
                    let before = views(&module_layout.borrow());
                    let driver = driver.upgrade().ok_or_else(|| "the window is gone".to_string())?;
                    // Applied before it returns: nothing else is applying while the probe runs.
                    let new = Action::InstanceNew {
                        kind: ModuleKind::Agent,
                    };
                    driver.run(WinId::MAIN, new, Origin::Lua);
                    let after = views(&module_layout.borrow());
                    after
                        .into_iter()
                        .find(|id| !before.contains(id))
                        .ok_or_else(|| "no chat view was opened".to_string())
                })
            },
            kill_chat_view: {
                let driver = Rc::downgrade(&driver);
                Rc::new(move |id: &ModuleId| {
                    if let Some(driver) = driver.upgrade() {
                        driver.kill_confirmed(WinId::MAIN, id);
                    }
                })
            },
            chat_views: chat_views.clone(),
        });
    }
    // The window shown, then the first module given the keys through `focus_module`: after the show, matching
    // standalone.rs's own `window.present(); pane.grab_focus();` ordering -- focusing a not-yet-shown widget is
    // meaningless. A chat view that took the keys is arrived at, as `Ctrl+a a` arrives: a project whose file
    // has the keys in the chat reopens with them there the way every other keyboard route lands. At launch
    // the panel has no session yet -- it is on its start screen, and its page may not have loaded -- so
    // today `enter_input` is refused there and this changes nothing on screen; it is written so that a launch
    // that resumes a session by itself lands in the composer, or on a waiting card, without anyone having to
    // remember this line. No activation token is handed over: GTK presents with the one the launcher set in
    // the environment, as it always has.
    // The link's events, now that what they act on exists: on linking, this window says which modules it holds,
    // and says it again whenever that changes; the other window may ask it to come forward; and when the other
    // window ends, by a clean close or not, this window takes back what it held for it.
    {
        let said: Rc<linked_window::HeldSaid> = Rc::default();
        let say_held: linked_window::spawn::OnLinked = {
            let module_layout = module_layout.clone();
            let workspaces = workspaces.clone();
            let said = said.clone();
            Rc::new(move |link| {
                if !link.linked() {
                    return;
                }
                let (Ok(layout), Ok(workspaces)) = (module_layout.try_borrow(), workspaces.try_borrow()) else {
                    return;
                };
                said.say(link, linked_window::held(&layout, &workspaces));
            })
        };
        {
            let pair = Rc::downgrade(&pair);
            let say_held = say_held.clone();
            let tell: Rc<dyn Fn()> = Rc::new(move || {
                if let Some(link) = pair.upgrade().and_then(|pair| pair.link()) {
                    say_held(&link);
                }
            });
            // A module the controller opens or kills changes what this window holds without the grid changing
            // anything itself, so its changes are heard as well as the grid's own.
            let by_grid = tell.clone();
            grid.connect_changed(move || by_grid());
            grid.connect_changed_by_controller(move || tell());
        }
        let ever_linked = Rc::new(std::cell::Cell::new(false));
        let linked: linked_window::spawn::OnLinked = {
            let ever_linked = ever_linked.clone();
            let wm = Rc::downgrade(&driver);
            // Window 1's other window is window 2, and window 2's is window 1.
            let peer = eitri_core::wm::PeerId(if pair.is_second() { 1 } else { 2 });
            let window1_editor = window1_editor.clone();
            let window2_editor = window2_editor.clone();
            let pair_review = pair_review.clone();
            let toast = toast.clone();
            let pair_weak = Rc::downgrade(&pair);
            Rc::new(move |link| {
                ever_linked.set(true);
                toast.set_standing(None);
                // Window 2's open is written into the pair file at once, so the next launch knows it was open. A
                // window 1 that stopped writing it when an earlier link ended without a goodbye writes it again: the
                // window 2 now linked does not.
                if let Some(pair) = pair_weak.upgrade().filter(|pair| pair.holds_pair()) {
                    pair.with_pair_writer(|writer| {
                        writer.resume();
                        writer.window2_opened();
                    });
                }
                // The window at this link's end has heard nothing yet, whatever an earlier window 2 was told.
                said.new_link();
                say_held(link);
                if let Some(one) = &window1_editor {
                    one.linked(link);
                }
                if let Some(two) = &window2_editor {
                    two.linked(link);
                }
                if let Some(review) = &pair_review {
                    review.linked(link);
                }
                // The move protocol is the controller's: its messages, the link's wakes and its end reach it from here.
                if let Some(wm) = wm.upgrade() {
                    link.attach_controller(WinId::MAIN, peer, &wm);
                }
            })
        };
        // The same raise as the supervisor's request, after the window's first show: the startup's show stays the
        // controller's present phase.
        let raise: Rc<dyn Fn(Option<String>)> = {
            let raised = window.downgrade();
            let pair_raise = pair_raise.clone();
            Rc::new(move |token: Option<String>| {
                let Some(raised) = raised.upgrade() else { return };
                pair_raise.raise(&raised, token.as_deref());
            })
        };
        let ended: linked_window::spawn::OnEnded = {
            let module_layout = module_layout.clone();
            let workspaces = workspaces.clone();
            let grid = grid.downgrade();
            let toast = toast.clone();
            let second = pair.is_second();
            let app = app.downgrade();
            let plugin_panels = plugin_panels.clone();
            let lua_pages = lua_pages.clone();
            let pair_weak = Rc::downgrade(&pair);
            let window1_editor = window1_editor.clone();
            let window2_editor = window2_editor.clone();
            let pair_review = pair_review.clone();
            Rc::new(move |link, end| {
                // The partner counts as another window from here on: it may have died, and this window cannot see
                // its state.
                if let Some(review) = &pair_review {
                    review.unlinked();
                }
                if let Some(one) = &window1_editor {
                    one.unlinked();
                }
                if let Some(two) = &window2_editor {
                    two.unlinked();
                }
                let who = link
                    .partner()
                    .map_or_else(|| "the other window".to_string(), |p| format!("process {}", p.pid));
                if second && !ever_linked.get() {
                    // Window 1 refused the start (another build, or no welcome could be made): a window that was
                    // never linked has nothing of the pair to keep, and closes.
                    eprintln!("[window] window 1 ended the link before welcome ({end:?}); window 2 closes");
                    if let Some(app) = app.upgrade() {
                        app.quit();
                    }
                    return;
                }
                let plan = {
                    let (Ok(mut layout), Ok(mut workspaces)) =
                        (module_layout.try_borrow_mut(), workspaces.try_borrow_mut())
                    else {
                        eprintln!("[window] BUG: the layout was borrowed when the link ended; nothing came back");
                        return;
                    };
                    let plan = linked_window::end_plan(&linked_window::away_everywhere(&layout, &workspaces));
                    linked_window::apply_end(&plan, &mut layout, &mut workspaces);
                    plan
                };
                println!(
                    "[window] the other window ({who}) ended: back {:?}, dropped {:?}",
                    plan.back, plan.dropped
                );
                if let Some(pair) = pair_weak.upgrade() {
                    if second {
                        // Orphaned, window 2 keeps the pair file from now on, naming what it holds.
                        let held = match (module_layout.try_borrow(), workspaces.try_borrow()) {
                            (Ok(layout), Ok(workspaces)) => linked_window::held(&layout, &workspaces),
                            _ => Default::default(),
                        };
                        pair.with_pair_writer(|writer| {
                            writer.window2_holds(&held);
                            writer.take_over();
                        });
                    } else if matches!(end, linked_window::LinkEnd::Bye { .. }) {
                        // Window 2 closed: what it held is back here, and the next launch does not start it.
                        pair.with_pair_writer(|writer| writer.window2_closed());
                    } else {
                        // No goodbye: window 2 may still run, orphaned, and keep the pair file itself.
                        pair.with_pair_writer(|writer| writer.hand_over());
                    }
                }
                if let Some(grid) = grid.upgrade() {
                    grid.apply();
                }
                for id in linked_window::pages_back(&plan) {
                    if linked_window::reload_page_back(&plugin_panels, &lua_pages, &id) {
                        println!("[window] {id}: loaded again, from its store as it is now");
                    }
                }
                if second {
                    // It stays up for as long as this window has no editor window to ask.
                    toast.set_standing(Some(linked_window::UNLINKED_NOTE));
                }
            })
        };
        pair.set_hooks(linked_window::spawn::PairHooks { linked, raise, ended });
    }
    driver.start(
        WinId::MAIN,
        StartStep::Present {
            activation: pair.take_activation().map(eitri_core::wm::ActivationToken),
            arrive: ViewId::of_module(&first),
            first,
        },
    );

    // The one real v1 event: fires once the window is actually up, so any Lua handler reacting
    // to it sees a fully-built shell (panels registered, commands bound, window shown).
    // `eitri.layout.show` and the rest work from here on, `shell:ready` handlers included; `get`
    // reads the live layout, and `run('module.<id>')` may name each registered panel.
    {
        let panel_ids: Vec<String> = lua_engine
            .panels
            .borrow()
            .entries()
            .iter()
            .map(|e| e.id.clone())
            .collect();
        let mut store = lua_engine.layout.borrow_mut();
        store.attach(module_layout.clone());
        store.attach_workspaces(workspaces.clone());
        store.set_lua_panels(panel_ids);
    }
    lua_engine.layout.borrow_mut().accept_requests();
    // `shell:ready`, after the window is shown; what its handlers queue is applied right after.
    driver.start(WinId::MAIN, StartStep::Requests);
    apply_layout_requests();

    // Startup leaves free but resident heap pages behind (the GPU driver's shader compiles, above all);
    // one trim once it has settled gives them back.
    process_memory::trim_heap_after_startup();

    // Waits for a cross-window "come to the front" request from `eitri-supervisor` -- e.g. the
    // dashboard's own UI, or another shell instance, asking this window to raise itself. Cloning
    // `agent_panel_handle` here (rather than after) is load-bearing: the `connect_close_request`
    // closure below takes ownership of the original `agent_panel_handle` by move, so anything
    // that still needs it afterward -- this watch included -- must clone it first, the same
    // pattern `lua_engine`'s own comment two blocks up documents for itself.
    {
        let raised = window.clone();
        agent_panel::watch_supervisor(agent_panel_handle.clone(), Rc::new(move || raised.present()));
    }

    // Registered last: this is `pane`'s final use in this function, so its `Rc` can be moved in
    // outright rather than cloned again. (`pane` is an `Rc<NeovideEditorPane>` because the side
    // panel's Ctrl+h handler above needs it too; `NeovideEditorPane` doesn't derive `Clone`, and
    // every use here only ever needs `&self` methods.) Once nvim is gone, the same
    // connect_close_request -> shutdown() -> glib::Propagation::Proceed pattern as standalone.rs.
    let app_for_close = app.clone();
    // Set by a termination signal (`end_signals`): the close that follows asks nothing.
    let ending_by_signal = Rc::new(std::cell::Cell::new(false));
    {
        let ending_by_signal = ending_by_signal.clone();
        let window = window.clone();
        // The log lines are written only if stdout takes them at once: a panic in this callback aborts the process
        // before the close, and a write blocked on a full pipe would hold the close up.
        let installed = end_signals::install(move |name| {
            if ending_by_signal.replace(true) {
                end_signals::write_if_writable(
                    1,
                    format!("[window] {name}: the window is already closing\n").as_bytes(),
                );
                return;
            }
            end_signals::mark_closing();
            end_signals::write_if_writable(
                1,
                format!(
                    "[window] {name}: closing the window without its questions; nvim is ended as a close ends it\n"
                )
                .as_bytes(),
            );
            window.close();
        });
        if let Err(err) = installed {
            eprintln!("[window] BUG: the termination signals were not handled ({err:?}); they end the process at once");
        }
    }
    window.connect_close_request(move |_window| {
        // Confirm running tabs once, then let nvim handle unsaved buffers through `:confirm qall`, as
        // for `prefix x`. The window closes when nvim exits
        // (`editor_quit::EditorQuitting::window_close`). Proceed only once nvim is gone or never ran;
        // refuse a close if nvim cannot be killed. A termination signal skips the prompts because
        // nobody may be present to answer. Then `pane.shutdown()` ends nvim by closing stdin, sending
        // SIGTERM and, after five seconds, SIGKILL by pid. It preserves swap files rather than using
        // `:qa!`.
        if !ending_by_signal.get() && editor_quitting.window_close() == editor_quit::WindowClose::Wait {
            return glib::Propagation::Stop;
        }
        // First, and synchronously: the debounce may still be waiting on an arrangement change. With
        // none unwritten this writes nothing -- a click never does (`eitri_core::wm::persist`'s module
        // doc says what is written when).
        let layout_written = driver.persist_now(WinId::MAIN);
        // The pair file's last write is waited for, briefly: it is a small file renamed into place. Window 2 closing
        // while it keeps the file ends the pair, so the next launch does not start it again. Once window 1's layout
        // file holds what it took back at its launch, the pair file forgets what window 2 held; while it does not (a
        // save that failed), the pair file keeps naming them, so the next launch takes them back again.
        pair.with_pair_writer(|writer| {
            if pair.is_second() {
                writer.window2_left();
            } else if layout_written {
                writer.forget_window2_holders();
            }
            if !writer.flush(std::time::Duration::from_secs(1)) {
                eprintln!("[window] the pair file was still being written; not waiting longer");
            }
        });
        // The other window of the pair hears this one is closing, rather than finding the link gone.
        if let Some(link) = pair.link() {
            link.close(if pair.is_second() {
                "window 2 closed"
            } else {
                "window 1 closed"
            });
        }
        // Kills nvim by its pid if it does not exit within the fork's wait (codex p1 #2).
        if let Some(pane) = &pane {
            pane.shutdown();
        }
        // Hangs every terminal's shell up without waiting for it (`TerminalSession`'s `Drop`). Should
        // the process exit first, the kernel closing each PTY master hangs it up anyway.
        for terminal in terminals.borrow().each() {
            terminal.shutdown();
        }
        // The scratch directory is owned by the panel's `ScratchDir`, which GTK may never drop.
        if let Some(path) = &scratch_path {
            let _ = std::fs::remove_dir_all(path);
        }
        // Capturing `pane_switch` here is load-bearing twice over. First, it keeps the
        // `PaneSwitch` alive for the window's whole lifetime -- it is otherwise a local of
        // `build_ui`, and its directory (holding the fake-`tmux` symlink and the live socket)
        // would be removed the instant this function returned, so nvim's `tmux` lookup would
        // fail from the very first keypress. Second, `cleanup()` is called explicitly rather
        // than left to `Drop`: a sandbox run confirmed GTK does not deterministically free a
        // signal-handler closure before the process exits, so relying on `Drop` reproducibly
        // left the shim directory behind after every close.
        if let Some(ps) = &pane_switch {
            ps.cleanup();
        }
        // Same two reasons as `pane_switch` just above: capturing it keeps the socket alive for the
        // window's lifetime, and cleanup is explicit because `Drop` is not reliably reached.
        if let Some(feed) = &theme_feed {
            feed.cleanup();
        }
        // wire 1's feed, for the same two reasons again. Its directory is swept by the next launch
        // if this is ever missed, but a sweep is a backstop, not a substitute: a leaked directory
        // per window close is how the other two protocols each learned this.
        if let Some(feed) = &context_feed {
            feed.cleanup();
        }
        // The nvim-keys feed, for the same two reasons: left a
        // local of `build_ui`, its `Drop` removed `k.sock` and `nvim_keys.lua` the moment this
        // function returned, and nvim's mappings, leader and `timeoutlen` never reached the panel.
        if let Some(feed) = &nvim_keys_feed {
            feed.cleanup();
        }
        // The plugins' follow socket, for the same two reasons.
        if let Some(socket) = &follow_socket {
            socket.cleanup();
        }
        // Shut down any `AgentSession` started by the panel. If the mode-selector screen
        // was never left, this is a no-op. The session's `Rc<RefCell<AgentPanelState>>`
        // is otherwise held by internal closures that do not run on window close, so
        // this call ensures settings backup restoration, hook socket cleanup, and child SIGTERM.
        // The panel's `shutdown` starts teardown on workers and returns the remaining
        // work. `hold_until_done` takes a `gio` application hold for it, so the panel's
        // shutdown returns at once and the process stays alive after the window closes
        // until teardown reports completion.
        //
        // `pane.shutdown()` above runs first on this thread and waits for nvim to exit.
        // The close path waits for nvim's `:confirm qall` before reaching this handler,
        // so nvim has already exited and that call returns at once. An nvim that cannot
        // be killed refuses the close instead of reaching this handler.
        // Plugins shut down with the window; the application waits up to the grace period.
        plugin_panels.shutdown(&app_for_close);
        agent_panel::hold_until_done(&app_for_close, agent_panel_handle.shutdown());
        // `lua_engine` is otherwise unused past this point in this task, but referencing it
        // here is what keeps its `Rc` alive for the life of the window rather than dropping as
        // soon as `build_ui` returns -- nothing else in this function holds a reference past
        // here. A later task clones this same `Rc` again for a `gio::SimpleAction` closure
        // elsewhere in `build_ui`; that clone and this one both keep the one canonical
        // `LuaEngine` alive.
        let _ = &lua_engine;
        glib::Propagation::Proceed
    });

    println!(
        "shell running: chrome+editor+agent-panel. scale_factor={} clean={}",
        window.scale_factor(),
        want_clean,
    );
}

thread_local! {
    /// The timer that ends the refusal flash now showing, if one is ([`show_flash`]).
    static REFUSAL_ENDS: std::cell::RefCell<Option<glib::SourceId>> = const { std::cell::RefCell::new(None) };
}

/// Where a refusal from an arm still in the shell goes: the controller, whose chrome model draws every flash and
/// every toast, so a refusal here looks exactly like one the controller decided (a waiting card's toast stays up
/// against either).
#[derive(Clone)]
struct Refusal(std::rc::Weak<dyn eitri_core::wm::HostSink>);

impl Refusal {
    fn new(driver: &Rc<eitri_core::wm::Driver>) -> Refusal {
        let sink: Rc<dyn eitri_core::wm::HostSink> = driver.clone();
        Refusal(Rc::downgrade(&sink))
    }

    /// The app name flashes and, unless `text` is empty, the toast says it.
    fn post(&self, text: &str) {
        if let Some(sink) = self.0.upgrade() {
            sink.post(HostEvent::ShellRefused {
                win: WinId::MAIN,
                text: text.to_string(),
            });
        }
    }
}

/// A refused key with no words for a person: the app name flashes alone.
fn flash(refusal: &Refusal) {
    refusal.post("");
}

/// The top bar's app name shows the prefix indicator's block for [`REFUSAL_FLASH`]: one flash the
/// chrome model asked for. Its own class, so a prefix armed meanwhile keeps its indicator; a second flash inside
/// one restarts it rather than being cut short by the first one's timer. One window per process (`NON_UNIQUE`),
/// so one timer per thread.
fn show_flash(app_name: &gtk4::Label) {
    app_name.add_css_class("refused");
    let app_name = app_name.clone();
    let ends = glib::timeout_add_local_once(REFUSAL_FLASH, move || {
        REFUSAL_ENDS.with(|ends| ends.borrow_mut().take());
        app_name.remove_css_class("refused");
    });
    if let Some(earlier) = REFUSAL_ENDS.with(|slot| slot.borrow_mut().replace(ends)) {
        earlier.remove();
    }
}

/// Brings a window of the pair forward after its first show, when the other window had the keys. GNOME lets a
/// window take the keys from another only with an activation token naming that other window's surface, which the
/// other window sends. sway, Hyprland and niri turn a token activation into an urgency hint, so there the window also
/// has the compositor's own tool focus it, on a worker thread (the adapter is started on first use).
#[derive(Clone)]
struct PairRaise {
    wm: eitri_core::compositor::Wm,
    adapter: Rc<std::cell::OnceCell<Rc<companion::wm_runner::WmRunner>>>,
}

impl PairRaise {
    fn new(choice: eitri_core::compositor::WmChoice) -> PairRaise {
        PairRaise {
            wm: eitri_core::compositor::resolve(choice, &|name| std::env::var_os(name)),
            adapter: Rc::default(),
        }
    }

    fn raise(&self, window: &ApplicationWindow, token: Option<&str>) {
        if let Some(token) = token {
            window.set_startup_id(token);
        }
        window.present();
        use eitri_core::compositor::Wm;
        if matches!(self.wm, Wm::Sway | Wm::Hyprland | Wm::Niri) {
            let wm = self.wm;
            self.adapter
                .get_or_init(|| companion::wm_runner::WmRunner::new(wm))
                .raise_pid(std::process::id());
        }
    }
}

/// An activation token from `window`'s display, for the other window of the pair to come forward with: GDK asks the
/// compositor for one naming the surface with the keys and the seat's last input. `None` when none is given.
fn activation_token(window: &glib::WeakRef<ApplicationWindow>) -> Option<String> {
    let window = window.upgrade()?;
    let context = WidgetExt::display(&window).app_launch_context();
    context
        .startup_notify_id(None::<&gtk4::gio::AppInfo>, &[])
        .map(|token| token.to_string())
        .filter(|token| !token.is_empty())
}

/// What a plugin's nvim commands do to its module in this window, as the controller decides them through its runner's
/// gates: a show puts it on screen (opening a start-hidden one by its rule the first time) with the keys where they
/// are, a hide sends the keys where any hide sends them. Whether it is shown, and on screen, is read from the
/// controller too.
struct GridFollowWindow {
    driver: std::rc::Weak<eitri_core::wm::Driver<eitri_core::wm::Controller>>,
}

impl GridFollowWindow {
    fn shown(&self, module: &ModuleId) -> (bool, bool) {
        self.driver.upgrade().map_or((false, false), |driver| {
            driver.query(|c| c.module_shown(WinId::MAIN, module))
        })
    }

    fn follow(&self, module: &ModuleId, show: bool) {
        if let Some(driver) = self.driver.upgrade() {
            driver.follow(WinId::MAIN, module, show);
        }
    }
}

impl plugin_panels::FollowWindow for GridFollowWindow {
    fn is_shown(&self, module: &ModuleId) -> bool {
        self.shown(module).0
    }

    fn is_on_screen(&self, module: &ModuleId) -> bool {
        self.shown(module).1
    }

    fn show(&self, module: &ModuleId) {
        self.follow(module, true);
    }

    fn hide(&self, module: &ModuleId) {
        self.follow(module, false);
    }
}

/// [`editor_quit::QuitHost`] over the real window: one GTK call per method, and the only part of the
/// editor's quit flow no test reaches (`editor_quit`'s module doc; the GUI checklist covers it).
struct ShellQuitHost {
    /// `None` in a window with no editor: no nvim runs there, so every close proceeds without asking one.
    pane: Option<Rc<NeovideEditorPane>>,
    module_layout: Rc<RefCell<eitri_core::layout::Layout>>,
    grid: glib::WeakRef<ModuleGrid>,
    show_on_screen: Rc<dyn Fn(&ModuleId) -> Result<(), LayoutError>>,
    focus_module: Rc<dyn Fn(&ModuleId) -> bool>,
    /// `build_ui`'s `give_keys`: a tab not drawn is chosen before the grab.
    give_keys: Rc<dyn Fn(&ModuleId) -> bool>,
    retire_editor: Rc<dyn Fn() -> Result<(), LayoutError>>,
    /// The window's y/n.
    prompt: Rc<close_prompt::ClosePrompt>,
    agent: agent_panel::AgentPanelHandle,
    /// Every terminal pane the window holds, live or idle; weak, as the window's other holders of
    /// the registry are, so the quit flow keeps no pane alive.
    terminals: std::rc::Weak<RefCell<InstancePool<terminal::TerminalPane>>>,
    window: ApplicationWindow,
    refusal: Refusal,
    toast: Rc<toast::Toast>,
}

impl editor_quit::QuitHost for ShellQuitHost {
    fn nvim_running(&self) -> bool {
        self.pane.as_ref().is_some_and(|pane| pane.is_running())
    }
    fn nvim_exited(&self) -> bool {
        self.pane.as_ref().is_some_and(|pane| pane.nvim_exited())
    }
    fn nvim_exit_pending(&self) -> bool {
        self.pane.as_ref().is_some_and(|pane| pane.nvim_exit_pending())
    }
    fn watched_call(&self) -> Option<neovide_editor::CallWatch> {
        self.pane.as_ref().and_then(|pane| pane.watched_call())
    }
    fn settled_watch(&self) -> Option<neovide_editor::CallWatch> {
        self.pane.as_ref().and_then(|pane| pane.settled_watched_call())
    }
    fn send_quit(&self, lua: &str) -> bool {
        self.pane.as_ref().is_some_and(|pane| pane.exec_lua_watched(lua))
    }
    fn end_nvim(&self) -> bool {
        self.pane.as_ref().is_some_and(|pane| pane.end_nvim())
    }
    fn reveal(&self) -> eitri_core::wm::close::Reveal {
        eitri_core::wm::close::reveal_for_prompt(&self.module_layout.borrow(), &ModuleId::editor())
    }
    fn editor_gone(&self) -> bool {
        self.module_layout.borrow().is_gone(&ModuleId::editor())
    }
    fn show_editor(&self) -> Result<(), LayoutError> {
        let editor = ModuleId::editor();
        (self.show_on_screen)(&editor)?;
        // An editor on a tab not drawn is off screen too (`eitri_core::wm::close::reveal_for_prompt`): nvim's
        // prompt must be seen before it is answered, so its tab is chosen, which also gives it the
        // keys the next step gives it anyway.
        let tabbed_away = self.module_layout.borrow().is_tabbed_away(&editor);
        if tabbed_away {
            if let Some(grid) = self.grid.upgrade() {
                grid.switch_tab(&editor, &*self.focus_module)?;
            }
        }
        Ok(())
    }
    fn focus_editor(&self) {
        (self.give_keys)(&ModuleId::editor());
    }
    fn hide_editor(&self) -> Result<(), LayoutError> {
        match self.grid.upgrade() {
            Some(grid) => grid.hide_module(&ModuleId::editor(), &*self.focus_module),
            None => Ok(()),
        }
    }
    fn retire_editor(&self) -> Result<(), LayoutError> {
        (self.retire_editor)()
    }
    fn kill_editor_in_place(&self) -> Result<(), LayoutError> {
        match self.grid.upgrade() {
            Some(grid) => grid.kill_module(&ModuleId::editor(), Reopen::InPlace, &*self.focus_module),
            None => Ok(()),
        }
    }
    fn show_chat(&self) -> Result<(), LayoutError> {
        (self.show_on_screen)(&ModuleId::agent())
    }
    fn close_confirmed(&self) -> bool {
        self.prompt.confirmed()
    }
    fn window_close_prompt(&self) -> Option<String> {
        // A registry held by a caller up the stack counts as no jobs rather than panicking the close.
        let jobs = self
            .terminals
            .upgrade()
            .and_then(|terminals| {
                terminals
                    .try_borrow()
                    .ok()
                    .map(|t| terminal::jobs::count_terminal_jobs(&t))
            })
            .unwrap_or(0);
        eitri_core::tabs::window_close_prompt_with_jobs(self.agent.running_count(), self.agent.queued_count(), jobs)
    }
    fn ask_to_close_window(&self, text: &str) {
        self.prompt.ask_to_close_window(text);
    }
    fn withdraw_close_confirmation(&self) {
        self.prompt.withdraw_confirmation();
    }
    fn close_window_confirmed(&self) {
        self.prompt.close_window_confirmed();
    }
    fn close_window(&self) {
        self.window.close();
    }
    fn refuse(&self, text: &str) {
        refuse_said(&self.refusal, text);
    }
    fn toast(&self, text: &str) {
        self.toast.show(text);
    }
    fn every_second(&self, mut tick: Box<dyn FnMut() -> bool>) {
        glib::timeout_add_local(std::time::Duration::from_secs(1), move || {
            if tick() {
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });
    }
}

/// Brings the chat view `chat` back for a reveal that leaves the keys where they are: shown where it was
/// hidden from and, when it sits in a tabbed or stacked container, its tab drawn. A hidden view's
/// container may have gone on drawing a sibling, so showing it alone can leave it on a tab nobody sees;
/// both steps are taken whichever place it started from. `Ok((changed, drawn))`: `drawn` is `false`
/// when its tab stays behind the keys' own (drawing it would hide them), so the caller can say so
/// instead.
fn reveal_chat(layout: &mut eitri_core::layout::Layout, chat: &ModuleId) -> Result<(bool, bool), LayoutError> {
    let shown = layout.show(chat)?;
    let tabbed_away = layout.is_tabbed_away(chat);
    let brought = tabbed_away && layout.bring_forward(chat);
    Ok((shown || brought, !tabbed_away || brought))
}

/// [`reveal_chat`] on the window's layout, redrawing the grid when it changed anything. `Ok(true)`
/// when the view is on its container's drawn tab now.
fn reveal_chat_in_grid(
    grid: &ModuleGrid,
    layout: &RefCell<eitri_core::layout::Layout>,
    chat: &ModuleId,
) -> Result<bool, LayoutError> {
    let (changed, drawn) = reveal_chat(&mut layout.borrow_mut(), chat)?;
    if changed {
        grid.apply();
    }
    Ok(drawn)
}

/// How a refusal names the way to `action`: `<prefix> <key>` as the keymap binds it first, or the
/// action's name when no key reaches it.
fn key_text(keymap: &eitri_core::keymap::Keymap, action: &Action) -> String {
    keymap
        .keys_for(action)
        .first()
        .map(|key| format!("{} {}", keymap.prefix().human(), key.human()))
        .unwrap_or_else(|| action.name())
}

/// How far `move.<dir>` moves a float: the same step in editor cells as `M-arrow`'s resize.
const FLOAT_MOVE_CELLS: u16 = 5;

/// What revealing a covered chat does: the covering floats (bottom first, each with its scratch flag) to stow --
/// every scratch float that does not hold the keys -- and whether the chat is still covered after them.
/// A plain float stays (a reveal never sinks one) and so does a float with the keys (the user is working
/// in it).
fn covered_reveal(covering: &[(ModuleId, bool)], keys: &ModuleId) -> (Vec<ModuleId>, bool) {
    let (stow, stay): (Vec<_>, Vec<_>) = covering.iter().partition(|(id, scratch)| *scratch && id != keys);
    (stow.into_iter().map(|(id, _)| id.clone()).collect(), !stay.is_empty())
}

/// The floats over the chat view `view` now, bottom first (`ModuleGrid::covered_now`): each view is
/// judged by what lies over its own module, so a float over one view leaves every other view clear.
/// With no float in the layout there is nothing to arrange for: every divider drag asks this.
fn view_covering(grid: &ModuleGrid, layout: &eitri_core::layout::Layout, view: ViewId) -> Vec<ModuleId> {
    covering_of_view(layout, view, &|id| grid.covered_now(id))
}

/// [`view_covering`] over any way of naming the floats over a module, so it can be judged without a
/// window. A view the layout does not hold has nothing over it.
fn covering_of_view(
    layout: &eitri_core::layout::Layout,
    view: ViewId,
    covered_now: &dyn Fn(&ModuleId) -> Vec<ModuleId>,
) -> Vec<ModuleId> {
    let chat = view.module();
    if layout.floats().is_empty() || !layout.contains(&chat) {
        return Vec::new();
    }
    covered_now(&chat)
}

/// The chat views covered now, as `view_place` and the tray take them: every chat module in the layout
/// with a float over it. The floats themselves are not what is covered; passed as they are, the views
/// would count as on screen and the floats would get the chips.
fn covered_views(grid: &ModuleGrid, layout: &eitri_core::layout::Layout) -> Vec<ModuleId> {
    covered_views_of(layout, &|id| grid.covered_now(id))
}

/// [`covered_views`] over any way of naming the floats over a module.
fn covered_views_of(
    layout: &eitri_core::layout::Layout,
    covered_now: &dyn Fn(&ModuleId) -> Vec<ModuleId>,
) -> Vec<ModuleId> {
    if layout.floats().is_empty() {
        return Vec::new();
    }
    layout
        .modules()
        .into_iter()
        .filter_map(|id| ViewId::of_module(&id))
        .filter(|view| !covering_of_view(layout, *view, covered_now).is_empty())
        .map(ViewId::module)
        .collect()
}

/// A reveal of the chat view `view` on screen under floats: stows the scratch floats over it that do not
/// hold the keys ([`covered_reveal`]), the keys staying where they are. `true` when a float still covers
/// it, for the caller to fall back to the toast.
fn reveal_covered_chat(
    grid: &ModuleGrid,
    layout: &RefCell<eitri_core::layout::Layout>,
    view: ViewId,
    focus: &dyn Fn(&ModuleId) -> bool,
) -> bool {
    let (stow, mut still) = {
        let layout = layout.borrow();
        let covering: Vec<(ModuleId, bool)> = view_covering(grid, &layout, view)
            .into_iter()
            .map(|id| {
                let scratch = layout.float_of(&id).is_some_and(|float| float.scratch);
                (id, scratch)
            })
            .collect();
        covered_reveal(&covering, layout.focus())
    };
    let chat = view.module();
    for id in stow {
        match grid.scratchpad_show(Some(&id), focus) {
            Ok(ScratchShow::Stowed(_)) => println!("[attention] {id}: stowed off {chat}"),
            Ok(other) => {
                eprintln!("[attention] {id}: asked to stow, the scratchpad answered {other:?}");
                still = true;
            }
            Err(err) => {
                eprintln!("[attention] {id}: not stowed: {err}");
                still = true;
            }
        }
    }
    still
}

/// What `prefix !` does with the keys in `keys_view` (`None`: the keys are in no chat view).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BreakPlan {
    /// The base view's active tab goes out into a new chat view of its own.
    Break,
    /// That extra view's tab comes back into the base view, and the view goes.
    Join(ViewId),
    /// Like tmux's `break-pane`, it acts on the pane with the keys: anywhere but a chat view there is
    /// no tab to break.
    Refuse,
}

fn break_plan(keys_view: Option<ViewId>) -> BreakPlan {
    match keys_view {
        Some(view) if view.is_base() => BreakPlan::Break,
        Some(view) => BreakPlan::Join(view),
        None => BreakPlan::Refuse,
    }
}

/// How a card's toast names the way back to chat view `view`: the key bound to its module (`C-b a` for
/// the base view), else its chip, which is reachable from the top bar whenever the view is off screen.
fn view_way_back(keymap: &eitri_core::keymap::Keymap, view: ViewId, title: &str) -> Option<String> {
    let key = keymap
        .keys_for(&Action::Module(view.module()))
        .first()
        .map(|key| format!("{} {}", keymap.prefix().human(), key.human()));
    match key {
        Some(key) => Some(key),
        None if view.is_base() => None,
        None => Some(format!("the {title} chip")),
    }
}

/// Puts the selection back on the module with the keys; `true` if a container was selected. A
/// layout borrowed right now (nothing holds it across an action) leaves the selection as it is.
fn clear_selection(layout: &RefCell<eitri_core::layout::Layout>) -> bool {
    layout.try_borrow_mut().is_ok_and(|mut layout| layout.clear_selection())
}

/// Which shell of a terminal pane a split key was pressed against: the pane's start count, and the
/// shell's pid when one was running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ShellStart {
    generation: u64,
    pid: Option<u32>,
}

impl ShellStart {
    fn of(generation: u64, pid: Option<u32>) -> Self {
        Self { generation, pid }
    }

    /// Whether `now` (the target's live pane today, `None` once it is idle or gone) still runs the
    /// shell captured here. A new start bumps the count, so a pane killed and reused under the same
    /// number fails; a shell that was running must be the same process, so a pane closed in place
    /// (the base terminal keeps its pane when killed) fails too. A shell not yet spawned when the key
    /// was pressed may have spawned since, under the same start.
    fn still(self, now: Option<ShellStart>) -> bool {
        let Some(now) = now else {
            return false;
        };
        now.generation == self.generation && (self.pid.is_none() || now.pid == self.pid)
    }
}

/// A refusal that has its own words: said on stdout, the app name flashes and the toast says why.
fn refuse_text(refusal: &Refusal, text: &str) {
    println!("[modules] refused: {text}");
    refuse_said(refusal, text);
}

/// A worded refusal whose caller has printed its own stdout line, under its own prefix: the app name
/// flashes and the toast says `text`. A waiting card's toast stays up instead (the chrome model's
/// rule); the flash and the stdout line still say a key was refused.
fn refuse_said(refusal: &Refusal, text: &str) {
    refusal.post(text);
}

/// The window's `select.number` digits and HINT's toggle, as the controller's `Hint` ops reach them: the digits
/// drawn over every module's host in the layout's tree order, a chat whose page is a notice included.
struct ShellHints {
    coordinator: Rc<hint::HintCoordinator>,
    grid: glib::WeakRef<ModuleGrid>,
}

impl gtk_host::HintPort for ShellHints {
    fn show_digits(&self, _win: WinId, digits: &[(ModuleId, u8)]) {
        let Some(grid) = self.grid.upgrade() else { return };
        let plan: Vec<(char, ModuleId)> = digits
            .iter()
            .filter_map(|(id, value)| Some((char::from_digit(u32::from(*value), 10)?, id.clone())))
            .collect();
        let drawn = self.coordinator.show_numbers(&plan, &grid.hosts_in_tree_order());
        println!("[prefix] select.number: {drawn} drawn");
    }
    fn hide_digits(&self, _win: WinId) {
        self.coordinator.hide_numbers();
    }
    fn toggle(&self, _win: WinId, keycode: u32) {
        self.coordinator.toggle_from_prefix(keycode);
    }
}

impl gtk_host::EditorReads for NeovideEditorPane {
    fn cell(&self) -> Option<(f64, f64)> {
        self.cell_size()
    }
    fn running(&self) -> bool {
        self.is_running()
    }
}

/// The chat view the keys are in, from the layout's focus (the module that last held them); the base
/// view's when that is no chat view.
fn keys_chat_view(layout: &Layout) -> ViewId {
    ViewId::of_module(layout.focus()).unwrap_or(ViewId::BASE)
}

/// What a request for a new chat view says where no `WebView` can be built in this process.
const CHAT_VIEWS_NEED_WEBKIT: &str = "chat views need WebKit, which cannot start here";

/// Every extra chat view (`agent#2`..`agent#9`) any of `layouts` holds, ascending and once each.
fn extra_chat_views(layouts: &[&eitri_core::layout::Layout]) -> Vec<ModuleId> {
    let mut ids: Vec<ModuleId> = layouts
        .iter()
        .flat_map(|layout| layout.modules())
        .filter(|id| id.kind() == ModuleKind::Agent && id.number() >= 2)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// The float size `id`'s open rule gives (at a new instance's point, a Lua panel's `position` counted),
/// if it gives one: what `float.toggle` floats a tile that never floated at.
fn rule_float_size(rules: &OpenRules, panel_decls: &[ModuleDecl], id: &ModuleId) -> Option<RuleSize> {
    let position = panel_decls.iter().find(|d| d.id == *id).map(|d| d.placement);
    match rules.resolve(id, OpenPoint::NewInstance, position).0.size {
        Some(size @ RuleSize::Float { .. }) => Some(size),
        _ => None,
    }
}

/// The rule [`open_plugin_module`] opens `id` by: the open rules', the plugin's own counted where a Lua panel's
/// `position` counts, at a new instance's point, with `focus` replaced by `take_keys` when the way it was opened
/// decides who has the keys (`None`: the rule's own).
fn plugin_open_rule(rules: &OpenRules, own: Option<&OpenRule>, id: &ModuleId, take_keys: Option<bool>) -> OpenRule {
    let (mut rule, _) = rules.resolve_own(id, OpenPoint::NewInstance, own);
    if let Some(take) = take_keys {
        rule.focus = take;
    }
    rule
}

/// Where a start-hidden plugin's module goes, hidden, in a workspace that lacks it once another holds it: its rule's
/// place as a tiled one (hidden there, it needs no float).
fn plugin_hidden_placement(rules: &OpenRules, own: Option<&OpenRule>, id: &ModuleId) -> Placement {
    let rule = plugin_open_rule(rules, own, id, None);
    without_keys(rule.at.unwrap_or(At::Beside(Direction::Right))).0
}

/// Puts a start-hidden plugin's module on screen: shown where it was if the layout holds it; else opened by its
/// rule ([`plugin_open_rule`]) beside `keys`, and put hidden into every other workspace that lacks it, as a new
/// instance is. The keys move, through `focus`, only when it takes them. A zoom that would hide it ends first. Its
/// page is built once the module is on screen (the grid's change hook) or about to take the keys (`focus_module`),
/// so an open the grid refuses builds no page and starts no web process.
#[allow(clippy::too_many_arguments)]
fn open_plugin_module(
    grid: &ModuleGrid,
    rules: &OpenRules,
    workspaces: &RefCell<Workspaces>,
    panels: &plugin_panels::PluginPanels,
    id: &ModuleId,
    keys: Option<&ModuleId>,
    take_keys: Option<bool>,
    focus: &dyn Fn(&ModuleId) -> bool,
) -> Result<(), LayoutError> {
    let rule = plugin_open_rule(rules, panels.own_rule(id).as_ref(), id, take_keys);
    let (held, zoom_hides) = {
        let layout = grid.layout();
        let layout = layout.borrow();
        (layout.contains(id), layout.zoom_hides(id))
    };
    if held {
        if zoom_hides {
            grid.unzoom();
        }
        grid.show_module(id)?;
        if rule.focus {
            focus(id);
        }
        return Ok(());
    }
    let opened = grid.open_module(id, &rule, keys, focus)?;
    for note in &opened.notes {
        println!("[layout] {note}");
    }
    let placement = plugin_hidden_placement(rules, panels.own_rule(id).as_ref(), id);
    workspaces.borrow_mut().add_everywhere_at(id, placement);
    println!(
        "[modules] {id}: opened by its rule{}",
        if opened.takes_keys {
            ""
        } else {
            ", the keys left where they were"
        }
    );
    Ok(())
}

/// A layout verb the layout refused -- the last module on screen hidden, a module placed next to
/// itself: said on stdout, the top bar's app name flashes ([`show_flash`]) and the toast says why
/// ([`refusal_text`]), so a refused key is not a silent blink.
fn refuse(refusal: &Refusal, err: &LayoutError) {
    println!("[modules] refused: {err}");
    refuse_said(refusal, &refusal_text(err));
}

/// What the toast says for a layout refusal: a killed editor's own sentence, since that refusal
/// recurs for the rest of the window's life and must say how to get the editor back; the layout's
/// own words for every other one.
fn refusal_text(err: &LayoutError) -> String {
    eitri_core::wm::feedback::layout_refusal_text(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::layout::Axis;

    /// A split from a terminal goes ahead after its directory is read only if the same shell is
    /// still there: not once the pane is idle, not after a new start under the same number, not after
    /// the running shell was closed in place.
    #[test]
    fn a_split_from_a_pane_needs_the_same_shell_when_its_directory_comes_back() {
        let captured = ShellStart::of(3, Some(4100));
        assert!(captured.still(Some(ShellStart::of(3, Some(4100)))));
        assert!(!captured.still(None), "killed: the pane went idle");
        assert!(
            !captured.still(Some(ShellStart::of(4, Some(4200)))),
            "the number reused by a new terminal"
        );
        assert!(
            !captured.still(Some(ShellStart::of(4, Some(4100)))),
            "a new start, whatever its pid"
        );
        assert!(
            !captured.still(Some(ShellStart::of(3, None))),
            "the shell closed in place"
        );
        let unspawned = ShellStart::of(1, None);
        assert!(
            unspawned.still(Some(ShellStart::of(1, Some(4300)))),
            "spawned since, same start"
        );
        assert!(!unspawned.still(Some(ShellStart::of(2, Some(4300)))));
    }

    /// A start-hidden plugin's module opens by the open rules, its own declared rule where none names it, with the
    /// keys as the way it was opened asks: a module key gives them, a show from nvim leaves them, and anything
    /// else follows the rule's own `focus`.
    #[test]
    fn a_plugin_module_opens_by_its_own_rule_with_the_keys_as_asked() {
        use eitri_core::layout::rules::RuleTarget;
        let id = ModuleId::lua("preview");
        let own = OpenRule {
            at: Some(At::Beside(Direction::Right)),
            focus: false,
            ..OpenRule::default()
        };
        let none = OpenRules::default();
        let rule = plugin_open_rule(&none, Some(&own), &id, Some(true));
        assert_eq!((rule.at, rule.focus), (Some(At::Beside(Direction::Right)), true));
        let rule = plugin_open_rule(&none, Some(&own), &id, Some(false));
        assert_eq!((rule.at, rule.focus), (Some(At::Beside(Direction::Right)), false));
        assert_eq!(
            plugin_open_rule(&none, Some(&own), &id, None),
            own,
            "the rule's own focus"
        );
        // No declared rule: a Lua panel's built-in place, the side.
        let rule = plugin_open_rule(&none, None, &id, None);
        assert_eq!(
            (rule.at, rule.focus),
            (Some(At::Placement(Placement::RightOfRoot)), true)
        );

        let mut rules = OpenRules::default();
        rules
            .add(
                RuleTarget::Module(id.clone()),
                OpenRule {
                    at: Some(At::Beside(Direction::Down)),
                    focus: true,
                    ..OpenRule::default()
                },
            )
            .unwrap();
        let rule = plugin_open_rule(&rules, Some(&own), &id, None);
        assert_eq!(
            (rule.at, rule.focus),
            (Some(At::Beside(Direction::Down)), true),
            "a rule for the module wins"
        );
        let rule = plugin_open_rule(&rules, Some(&own), &id, Some(false));
        assert_eq!((rule.at, rule.focus), (Some(At::Beside(Direction::Down)), false));
    }

    /// Enumerating extra views merges workspaces without duplicating or including the base view.
    #[test]
    fn extra_chat_views_are_sorted_and_unique_across_workspaces() {
        use eitri_core::layout::{Layout, ModuleDecl, Placement};
        let id = |text: &str| ModuleId::parse(text).unwrap();
        let below = |ids: &[&str]| -> Layout {
            let decls: Vec<ModuleDecl> = ids
                .iter()
                .map(|text| ModuleDecl {
                    id: id(text),
                    placement: Placement::BelowEditor,
                })
                .collect();
            Layout::initial(&decls).unwrap()
        };
        let current = below(&["terminal"]);
        let other = below(&["terminal", "agent#2"]);
        let both = below(&["terminal", "agent#3", "agent#2"]);
        assert_eq!(
            extra_chat_views(&[&both, &other, &current]),
            vec![id("agent#2"), id("agent#3")],
            "the extra views every layout holds, once each and never the base view"
        );
        assert!(extra_chat_views(&[&current]).is_empty());
    }

    /// Revealing a covered chat stows the scratch floats over it that do not hold the keys and leaves
    /// every other float where it is, saying whether the chat is still covered.
    #[test]
    fn revealing_a_covered_chat_stows_scratch_floats_that_do_not_hold_the_keys() {
        let id = |text: &str| ModuleId::parse(text).unwrap();
        let (scratch, plain) = (id("terminal#2"), id("terminal#3"));
        let editor = ModuleId::editor();
        assert_eq!(
            covered_reveal(&[(scratch.clone(), true), (plain.clone(), false)], &editor),
            (vec![scratch.clone()], true),
            "a plain float stays, so the chat is still covered"
        );
        assert_eq!(
            covered_reveal(&[(scratch.clone(), true), (plain.clone(), false)], &scratch),
            (vec![], true),
            "the scratch float with the keys stays"
        );
        assert_eq!(
            covered_reveal(&[(scratch.clone(), true), (plain.clone(), true)], &editor),
            (vec![scratch, plain], false),
            "every one stowed, bottom first: the chat is clear"
        );
        assert_eq!(covered_reveal(&[], &editor), (vec![], false));
    }

    /// A float over the chat makes the chat the covered module: its place reads `Covered`, it gets the
    /// chip, and the float on screen gets none. The floats the grid names are not what is covered.
    #[test]
    fn a_float_over_the_chat_covers_the_chat_not_itself() {
        use eitri_core::layout::{
            arrange, covering, tray_covered, view_place, Axis, FloatRect, Frame, Layout, Node, Size,
        };
        let size = Size { w: 1280, h: 721 };
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let chat = ModuleId::agent();
        let mut layout = Layout::new(
            Node::split(Axis::Row, 0.5, Node::Leaf(ModuleId::editor()), Node::Leaf(chat.clone())),
            ModuleId::editor(),
        )
        .unwrap();
        layout.add_float(&t2, FloatRect::centred(size, 0.9), false).unwrap();
        let frame = Frame::new(size, 1);
        let floats = covering_of_view(&layout, ViewId::BASE, &|id| covering(&arrange(&layout, &frame), id));
        assert_eq!(floats, vec![t2.clone()], "the float is over the chat");
        let covered = covered_views_of(&layout, &|id| covering(&arrange(&layout, &frame), id));
        assert_eq!(covered, vec![chat.clone()]);
        assert_eq!(view_place(&layout, &chat, &covered), AgentPlace::Covered);
        let chips = tray_covered(&layout, &covered);
        assert!(chips.contains(&chat), "{chips:?}");
        assert!(!chips.contains(&t2), "{chips:?}");
        let clear = Layout::new(
            Node::split(Axis::Row, 0.5, Node::Leaf(ModuleId::editor()), Node::Leaf(chat.clone())),
            ModuleId::editor(),
        )
        .unwrap();
        assert_eq!(
            covered_views_of(&clear, &|id| covering(&arrange(&clear, &frame), id)),
            Vec::<ModuleId>::new(),
            "nothing over it: nothing covered"
        );
    }

    /// Each chat view is judged by the floats over its own module: a float over `agent#2` covers view 2
    /// and leaves the base view, beside it, clear -- so an answer from the base page is taken while one
    /// from view 2's page is refused.
    #[test]
    fn a_covered_view_is_judged_by_its_own_floats() {
        use eitri_core::layout::{arrange, covering, view_place, Axis, FloatRect, Frame, Layout, Node, Rect, Size};
        let size = Size { w: 1280, h: 721 };
        let frame = Frame::new(size, 1);
        let two = ModuleId::parse("agent#2").unwrap();
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let mut layout = Layout::new(
            Node::split(Axis::Row, 0.5, Node::Leaf(ModuleId::agent()), Node::Leaf(two.clone())),
            ModuleId::agent(),
        )
        .unwrap();
        let rect_of_two = arrange(&layout, &frame).rect_of(&two).expect("view 2 is tiled");
        // A small float well inside view 2, clear of the base view on its left.
        let over_two = Rect {
            x: rect_of_two.x + rect_of_two.w / 4,
            y: rect_of_two.y + rect_of_two.h / 4,
            w: rect_of_two.w / 2,
            h: rect_of_two.h / 2,
        };
        layout
            .add_float(&t2, FloatRect::of_rect(over_two, size), false)
            .unwrap();
        let now = |id: &ModuleId| covering(&arrange(&layout, &frame), id);
        let view2 = ViewId::of_module(&two).unwrap();
        assert_eq!(
            covering_of_view(&layout, view2, &now),
            vec![t2.clone()],
            "covered for view 2"
        );
        assert_eq!(
            covering_of_view(&layout, ViewId::BASE, &now),
            Vec::<ModuleId>::new(),
            "the base view is clear"
        );
        let covered = covered_views_of(&layout, &now);
        assert_eq!(covered, vec![two.clone()], "only view 2 is covered");
        assert_eq!(view_place(&layout, &two, &covered), AgentPlace::Covered);
        assert_eq!(view_place(&layout, &ModuleId::agent(), &covered), AgentPlace::OnScreen);
        let three = ViewId::new(3).unwrap();
        assert_eq!(
            covering_of_view(&layout, three, &now),
            Vec::<ModuleId>::new(),
            "a view not in the layout"
        );
    }

    /// `prefix !` breaks from the base view, joins back from an extra one, and does nothing with the keys
    /// in no chat view, as tmux's `break-pane` acts on the pane with the keys.
    #[test]
    fn break_plan_breaks_from_the_base_view_joins_from_an_extra_one_and_refuses_elsewhere() {
        let two = ViewId::new(2).unwrap();
        assert_eq!(break_plan(Some(ViewId::BASE)), BreakPlan::Break);
        assert_eq!(break_plan(Some(two)), BreakPlan::Join(two));
        assert_eq!(break_plan(ViewId::of_module(&ModuleId::editor())), BreakPlan::Refuse);
        assert_eq!(break_plan(ViewId::of_module(&ModuleId::terminal())), BreakPlan::Refuse);
        assert_eq!(break_plan(None), BreakPlan::Refuse);
    }

    /// A chat hidden from a tabbed container whose other tab then took the keys comes back shown but
    /// behind that tab, which holds the keys: the reveal reports it so the caller toasts instead. In a
    /// container beside the keys its tab is drawn and the keys stay where they are.
    #[test]
    fn a_hidden_chat_in_a_tabbed_container_is_drawn_or_reported_behind() {
        use eitri_core::layout::{hide, Child, Con, ConLayout, Frame, Layout, Node, Size};
        let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
        let chat = ModuleId::agent();

        let mut tabs = Con::new(
            ConLayout::Tabbed,
            vec![
                Child::new(Node::Leaf(ModuleId::editor()), 0.5),
                Child::new(Node::Leaf(ModuleId::agent()), 0.5),
            ],
        );
        tabs.focus = 1;
        let mut layout = Layout::new(Node::Con(tabs), chat.clone()).unwrap();
        let next = hide(&mut layout, &chat, &frame)
            .unwrap()
            .expect("the chat held the keys");
        layout.set_focus(&next).unwrap();
        assert_eq!(next, ModuleId::editor());
        assert_eq!(reveal_chat(&mut layout, &chat).unwrap(), (true, false));
        assert!(layout.is_tabbed_away(&chat), "shown, on the tab not drawn");
        assert_eq!(layout.focus(), &ModuleId::editor());

        let mut tabs = Con::new(
            ConLayout::Tabbed,
            vec![
                Child::new(Node::Leaf(ModuleId::agent()), 0.5),
                Child::new(Node::Leaf(ModuleId::terminal()), 0.5),
            ],
        );
        tabs.focus = 1;
        let mut layout = Layout::new(
            Node::split(Axis::Row, 0.5, Node::Leaf(ModuleId::editor()), Node::Con(tabs)),
            ModuleId::editor(),
        )
        .unwrap();
        hide(&mut layout, &chat, &frame).unwrap();
        assert!(!layout.is_shown(&chat));
        assert_eq!(reveal_chat(&mut layout, &chat).unwrap(), (true, true));
        assert!(layout.is_visible(&chat));
        assert_eq!(layout.focus(), &ModuleId::editor());

        assert_eq!(
            reveal_chat(&mut layout, &chat).unwrap(),
            (false, true),
            "already on screen"
        );
    }

    /// Checks that `build_application` sets `NON_UNIQUE`, which disables gio's
    /// single-instance behavior. Removing the flag fails this assertion;
    /// its runtime effect appears when a second window is launched.
    /// This test does not launch a second `shell` or exercise gio registration, and
    /// would still pass if another route ran `build_ui` twice in one process.
    /// Separate processes on separate launches follow gio's semantics; demonstrating
    /// that behavior requires launching two real `shell` processes.
    #[test]
    fn build_application_still_opts_out_of_gios_single_instance_behaviour() {
        let app = build_application();
        assert!(app.flags().contains(gtk4::gio::ApplicationFlags::NON_UNIQUE));
    }

    /// Keysyms named literally (not a single character and not an `Fnn` function key) that
    /// this test knows how to read for a person, and how it spells each one. The zoom-together
    /// design's text-size accelerators (`<Control>equal` and friends) are the first accelerators
    /// in this crate whose key part isn't a bare letter/digit or a function key, so `spell` and
    /// `is_accel` both need to know them by name -- this table is the one place that names them
    /// for both.
    const NAMED_KEYSYMS: [(&str, &str); 7] = [
        ("equal", "="),
        ("plus", "+"),
        ("minus", "-"),
        ("KP_Add", "Keypad+"),
        ("KP_Subtract", "Keypad-"),
        ("KP_0", "Keypad0"),
        // Keypad 0 with NumLock off reports as KP_Insert, not KP_0 -- text-reset binds both
        // (2026-09-23).
        ("KP_Insert", "Keypad0 (NumLock off)"),
    ];

    /// `<Mod>…Key`, with at least one modifier or a function key. The shape the sources are
    /// scanned for; anything else in a string literal is not an accelerator.
    ///
    /// A literal with a valid modifier-bracket prefix (so it is clearly meant as an
    /// accelerator) whose key part is neither a bare letter/digit, an `Fnn` function key, nor
    /// one of `NAMED_KEYSYMS` is a scanner FAILURE, not a silent skip: it panics, naming the
    /// literal and the file, rather than quietly leaving `found` blind to it. That is the
    /// direction this whole test is written to fail in -- see its own doc comment on "both
    /// directions" -- and the reason it exists concretely: before `NAMED_KEYSYMS` exhausted,
    /// `<Control>KP_Add` (underscore is not alphanumeric) would otherwise have been dropped
    /// from `found` with nothing here noticing the guard had gone blind to it.
    fn is_accel(literal: &str) -> bool {
        let mut rest = literal;
        let mut modifiers = 0;
        while let Some(end) = rest.strip_prefix('<').and_then(|r| r.find('>')) {
            if !rest[1..end + 1].chars().all(|c| c.is_ascii_alphabetic()) {
                return false;
            }
            modifiers += 1;
            rest = &rest[end + 2..];
        }
        let function_key = rest.starts_with('F') && rest.len() > 1 && rest[1..].chars().all(|c| c.is_ascii_digit());
        if rest.is_empty() || (modifiers == 0 && !function_key) {
            return false;
        }
        if function_key
            || rest.chars().all(|c| c.is_ascii_alphanumeric())
            || NAMED_KEYSYMS.iter().any(|(keysym, _)| *keysym == rest)
        {
            return true;
        }
        panic!(
            "{literal:?} has a modifier bracket ({modifiers} of them) but its key part {rest:?} \
             is not one `spell`/`NAMED_KEYSYMS` knows how to read -- teach both, or this was \
             never meant to be an accelerator"
        );
    }

    /// Directories scanned, relative to the workspace root.
    const SCANNED: &[&str] = &["shell/src", "panel/src"];

    /// Every `.rs` file of [`SCANNED`], with its `#[cfg(test)]` module and its line comments cut
    /// off -- so this test's own copies of the accelerators cannot satisfy it. Fails when a
    /// directory is missing or the code the scan exists to cover is not in what it read.
    fn sources() -> Vec<(String, String)> {
        use eitri_core::source_scan as scan;
        let read = scan::rust_sources(SCANNED);
        scan::require_anchors(
            &read,
            &["fn build_ui(", "fn handle_inbound_message(", "struct TrustGate"],
        );
        read.into_iter()
            .map(|source| {
                let code = source.text.split("#[cfg(test)]").next().unwrap_or_default();
                let code: String = code
                    .lines()
                    .map(|line| line.split("//").next().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("\n");
                (source.path, code)
            })
            .collect()
    }

    /// Every accelerator-shaped string literal in `shell/src`, cut from `.rs` files by
    /// [`sources`]/[`is_accel`]. Shared, so the two tests below scan the same tree the same way.
    fn found_accelerators() -> Vec<(String, String)> {
        let mut found = Vec::new();
        for (name, code) in &sources() {
            for (index, chunk) in code.split('"').enumerate() {
                if index % 2 == 1 && is_accel(chunk) {
                    found.push((name.clone(), chunk.to_string()));
                }
            }
        }
        found
    }

    /// Register every accelerator through the core keymap. An accelerator literal in
    /// `shell/src` bypasses the keymap and its check against `Ctrl+Shift` bindings.
    /// Exercise `is_accel` directly so a broken scanner cannot make this test pass.
    #[test]
    fn shell_src_writes_no_accelerator_literal() {
        sources();
        assert!(is_accel("<Control><Shift>f") && is_accel("F11") && is_accel("<Control>KP_Add"));
        assert!(!is_accel("<C-b>") && !is_accel("Ctrl+b"));
        assert_eq!(
            found_accelerators(),
            Vec::<(String, String)>::new(),
            "register it through eitri_core::keymap::root instead"
        );
    }

    /// Every keyboard arrival lands in BROWSE, except the
    /// new-tab path, which still opens the composer -- an empty tab has nothing to browse.
    #[test]
    fn keyboard_arrival_is_browse_and_only_a_new_tab_types() {
        let (_, src) =
            eitri_core::source_scan::function_in(&eitri_core::source_scan::rust_sources(&["shell/src"]), "build_ui");
        let calls = src.matches(concat!(".enter", "_input(")).count();
        assert_eq!(calls, 1, "only the new-tab path may land in INPUT");
        // Scoped to the two closures: a count over the whole file stays green while other
        // callers of `arrive` remain, even after the keyboard path stops calling it.
        fn body_after<'a>(code: &'a str, anchor: &str) -> &'a str {
            let at = code
                .find(anchor)
                .unwrap_or_else(|| panic!("no `{anchor}` in main.rs; was it renamed?"));
            let open = at
                + code[at..]
                    .find('{')
                    .unwrap_or_else(|| panic!("no body after `{anchor}`"));
            eitri_core::source_scan::braced(code, open)
        }
        let code = src;
        let land_by_key = body_after(&code, concat!("let land_by_key: Rc<dyn Fn(&Module", "Id) -> bool>"));
        assert!(
            land_by_key.contains(concat!(".arr", "ive(view)")),
            "landing by key must tell the agent panel the view arrived"
        );
        // `Ctrl+h/j/k/l` moves are the controller's: they land through its `land_by_key`, whose arrival is the
        // one that lands where the reader left the chat.
        let focus =
            eitri_core::source_scan::code_only(&eitri_core::source_scan::read_required("core/src/wm/focus.rs").text);
        let move_focus = eitri_core::source_scan::functions(&focus, "move_focus");
        assert_eq!(move_focus.len(), 1, "one move between modules");
        assert!(
            move_focus[0].contains(concat!("land_by_", "key(w, win, &to, batch)")),
            "a Ctrl+h/j/k/l move must land through land_by_key"
        );
        let land = eitri_core::source_scan::functions(&focus, "land_by_key");
        assert!(
            land[0].contains("arrive_tail(win, to, false)"),
            "landing by key arrives where the reader left the chat"
        );
    }

    /// The full window drops the outer nvim's variables before anything can inherit them: after the
    /// panel branch (which reads `$NVIM`), ahead of the backend check, the nvim probe and GTK. Byte
    /// offsets, not lines, so the order holds however the lines are wrapped.
    #[test]
    fn main_drops_the_inherited_editor_env_before_any_child() {
        // Split literals, so this test's own text is not counted.
        let src = include_str!("main.rs");
        let start = src.find(concat!("fn main() -> glib", "::ExitCode {")).expect("main");
        let end = start
            + src[start..]
                .find(concat!("fn build_", "application"))
                .expect("build_application");
        let body = &src[start..end];
        let call = concat!("companion::env::drop_inherited", "_editor_env()");
        assert_eq!(body.matches(call).count(), 1, "called exactly once in main");
        let at = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("{needle} in main"));
        let order = [
            at(concat!("companion::run(", "early_args")),
            at(call),
            at(concat!("BackendKind::", "from_env")),
            at(concat!("nvim_bin::", "resolve()")),
            at(concat!("build_", "application()")),
        ];
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
    }

    /// Window 2's link end and log come first: before any other subcommand, before the editor environment is
    /// touched, before any child or thread, and before the project resolver, which would read its arguments as a
    /// directory.
    #[test]
    fn the_linked_window_subcommand_is_dispatched_before_the_project_resolver() {
        // Split literals, so this test's own text is not counted.
        let src = include_str!("main.rs");
        let start = src.find(concat!("fn main() -> glib", "::ExitCode {")).expect("main");
        let end = start
            + src[start..]
                .find(concat!("fn build_", "application"))
                .expect("build_application");
        let body = &src[start..end];
        let at = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("{needle} in main"));
        let order = [
            at(concat!("args_os().skip(1)", ".collect()")),
            at(concat!("start::", "SUBCOMMAND")),
            at(concat!("second_first_acts(", "&early_args[1..])")),
            at(concat!("run_guard::", "SUBCOMMAND")),
            at(concat!("companion::run(", "early_args")),
            at(concat!("drop_inherited", "_editor_env()")),
            at(concat!("tasks::install", "_guard(")),
            at(concat!("project_root::", "resolve()")),
        ];
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
        assert_eq!(
            eitri_core::window_link::start::SUBCOMMAND,
            "linked-window",
            "the subcommand window 1 starts window 2 with"
        );
    }

    /// Window 2 builds every module but the editor, so nothing in `build_ui` may assume the editor exists: its
    /// pane is an `Option` from the editor phase on, and nothing takes the pane out of it by force.
    #[test]
    fn pane_none_builds_every_module_but_the_editor() {
        use eitri_core::source_scan as scan;
        let (_, body) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
        assert!(
            body.contains("let (pane, editor_host, editor_start_failure_label) = match made_editor"),
            "the editor's pane is taken from the editor phase as an Option"
        );
        assert!(body.contains("Some(pane), Some(host), Some(failure)") && body.contains("None => (None, None, None)"));
        let forced: Vec<String> = scan::word_ends(&body, "pane")
            .into_iter()
            .map(|end| {
                let rest = &body[end..];
                let stop = rest.find([';', '{', '\n']).unwrap_or(rest.len());
                rest[..stop].to_string()
            })
            .filter(|rest| {
                ["unwrap", "expect"]
                    .iter()
                    .any(|forced| rest.starts_with(&format!(".{forced}(")) || rest.contains(&format!(").{forced}(")))
            })
            .collect();
        assert!(
            forced.is_empty(),
            "the editor's pane forced out of its Option: {forced:?}"
        );
        assert!(
            !body.contains("editor_maker.take().expect("),
            "the editor phase's answer is not expected to hold an editor"
        );
    }

    /// JavaScriptCore reads its options once, when WebKit first starts it, so the UI process's JIT
    /// switch must come before GTK and every `WebView`: inside `main`, ahead of `build_application`
    /// (whose `activate` builds the panel). The heap trim is scheduled from `build_ui`, once.
    #[test]
    fn main_turns_the_ui_jit_off_before_gtk_and_build_ui_schedules_the_trim() {
        // Split literals, so this test's own text is not counted.
        let src = include_str!("main.rs");
        let start = src.find(concat!("fn main() -> glib", "::ExitCode {")).expect("main");
        let end = start
            + src[start..]
                .find(concat!("fn build_", "application"))
                .expect("build_application");
        let body = &src[start..end];
        let jit = concat!("process_memory::ui_process_jsc", "_without_jit()");
        assert_eq!(body.matches(jit).count(), 1, "called exactly once in main");
        assert!(body.find(jit) < body.find(concat!("build_", "application()")));
        let (_, ui) =
            eitri_core::source_scan::function_in(&eitri_core::source_scan::rust_sources(&["shell/src"]), "build_ui");
        assert_eq!(
            ui.matches(concat!("process_memory::trim_heap", "_after_startup()"))
                .count(),
            1
        );
        // The companion window (`eitri panel`, `eitri split`) builds the same panel in a process of its
        // own and does both too.
        let companion = include_str!("companion/mod.rs");
        let jit_at = companion
            .find(concat!("process_memory::ui_process_jsc", "_without_jit()"))
            .expect("the companion turns its JIT off");
        assert!(
            jit_at
                < companion
                    .find(concat!("Application::", "builder()"))
                    .expect("its application")
        );
        assert_eq!(
            companion
                .matches(concat!("process_memory::trim_heap", "_after_startup()"))
                .count(),
            1
        );
    }

    /// A termination signal closes the window through its own close, which ends nvim by its pid, and the close
    /// asks nothing after one: the handlers are installed in `build_ui` before the close handler, and that
    /// handler reads the signal's flag before it lets `editor_quitting` ask or wait. The signal's own handler
    /// marks the close for the watchdog and writes nothing that can panic or block.
    #[test]
    fn a_termination_signal_closes_the_window_without_its_questions() {
        let (_, ui) =
            eitri_core::source_scan::function_in(&eitri_core::source_scan::rust_sources(&["shell/src"]), "build_ui");
        let install = ui
            .find(concat!("end_signals::", "install("))
            .expect("build_ui installs the handlers");
        assert_eq!(ui.matches(concat!("end_signals::", "install(")).count(), 1);
        let handler = &ui[install..];
        let close_at = handler
            .find(concat!("window.", "close()"))
            .expect("a signal closes the window");
        let close_request = ui
            .find(concat!("window.connect_", "close_request("))
            .expect("the close handler");
        assert!(
            install < close_request,
            "installed before the close handler takes what it moves"
        );
        assert!(
            install + close_at < close_request,
            "the handler itself closes the window"
        );
        // A failed write panics in `println!`, and a panic in this GLib callback aborts the process; the close
        // is marked first, so the watchdog lets a second signal force.
        let body = &handler[..close_at];
        assert!(
            !body.contains(concat!("println", "!(")),
            "the handler's own lines may not panic"
        );
        assert!(
            !body.contains(concat!("writeln", "!(")),
            "the handler's own lines may not block on a full pipe"
        );
        assert_eq!(
            body.matches(concat!("end_signals::", "write_if_writable(")).count(),
            2,
            "both lines go through the write that never blocks"
        );
        assert!(body.contains(concat!("end_signals::", "mark_closing()")));
        let close = &ui[close_request..];
        let flag = close
            .find(concat!("!ending_by_", "signal.get()"))
            .expect("the close reads the flag");
        let asks = close
            .find(concat!("editor_quitting.", "window_close()"))
            .expect("the close's questions");
        let shutdown = close
            .find(concat!("pane.", "shutdown();"))
            .expect("the close ends nvim");
        assert!(
            flag < asks && asks < shutdown,
            "the flag is read first, and nvim is ended after both"
        );
        assert_eq!(
            end_signals::SIGNALS.map(|(signum, _)| signum),
            [libc::SIGTERM, libc::SIGINT, libc::SIGHUP]
        );
    }

    /// The overlay's "Anywhere in the window" rows come from `eitri_core::keymap::root`, and its
    /// `Ctrl+j` row exists only because of the terminal: every chord the terminal gives up to
    /// Eitri (the controller's `nav_direction` over the key as GTK delivers it, asked about each letter) must be
    /// named there.
    #[test]
    fn every_chord_the_terminal_gives_up_is_in_the_root_help() {
        let documented: std::collections::HashSet<String> = eitri_core::keymap::root::help_rows()
            .into_iter()
            .flat_map(|row| row.keys.split(" / ").map(str::to_string).collect::<Vec<_>>())
            .collect();
        let mut taken = 0;
        for letter in 'a'..='z' {
            let key = gtk4::gdk::Key::from_name(letter.to_string()).expect("a letter keyval");
            let press = gtk_host::keys::key_press(key, 0, gtk4::gdk::ModifierType::CONTROL_MASK);
            if eitri_core::wm::keys::nav_direction(&press).is_some() {
                taken += 1;
                let name = format!("Ctrl+{letter}");
                assert!(
                    documented.contains(&name),
                    "the terminal gives up {name}, but no root help row names it"
                );
            }
        }
        assert_eq!(taken, 4, "Ctrl+h/j/k/l, and nothing else");
    }

    /// A refused layout verb says why on the toast: the layout's own words, and for a killed editor
    /// the sentence that says how to get it back.
    #[test]
    fn refuse_toasts_every_layout_error() {
        for err in [
            LayoutError::LastVisible(ModuleId::editor()),
            LayoutError::NoScratch,
            LayoutError::NothingToUndo,
            LayoutError::Gone(ModuleId::agent()),
        ] {
            assert_eq!(
                refusal_text(&err),
                err.to_string(),
                "{err:?} shows the layout's own words"
            );
        }
        assert_eq!(refusal_text(&LayoutError::NoScratch), "the scratchpad is empty");
        assert_eq!(
            refusal_text(&LayoutError::Gone(ModuleId::editor())),
            editor_quit::RETIRED_EDITOR_TEXT,
            "a killed editor's refusal says how to get the editor back"
        );
    }

    /// Every refused key in `build_ui` that has words for a person shows them on the toast, through
    /// `refuse`, `refuse_text` or `refuse_said`. A bare `flash(` there is one of the few refusals
    /// whose words are internal or absent, each listed with why it stays a blink; a new one fails
    /// here until it says why on the toast or joins the list with a reason.
    #[test]
    fn no_refusal_in_build_ui_only_flashes() {
        use eitri_core::source_scan as scan;
        // (text in the arm before its `flash(`, how many such arms, why it only flashes)
        const FLASH_ONLY: &[(&str, usize, &str)] = &[
            (
                "if !switched",
                2,
                "a tab verb that found no tab prints nothing; tmux's own answer to it is a flash",
            ),
            (
                "step_to",
                1,
                "a terminal step with nowhere to go prints nothing, as the tab verbs",
            ),
            ("not killed:", 1, "the words are an internal error's text"),
        ];
        let sources = scan::rust_sources(&["shell/src"]);
        scan::require_anchors(&sources, &["fn build_ui(", "fn flash(", "fn refuse_said("]);
        let (_, code_text) = scan::function_in(&sources, "build_ui");
        let (_, raw_text) = scan::function_in_raw(&sources, "build_ui");
        let code = code_text.as_bytes();
        let start = 0;
        let end = code.len();
        let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
        let word = b"flash";
        let mut found = vec![0usize; FLASH_ONLY.len()];
        let mut bare = Vec::new();
        for at in start + 1..end - word.len() {
            if code[at..at + word.len()] != word[..]
                || ident(code[at - 1])
                || code[at - 1] == b'.'
                || ident(code[at + word.len()])
                || code[at + word.len()..end].iter().find(|c| !c.is_ascii_whitespace()) != Some(&b'(')
            {
                continue;
            }
            // The arm is the innermost block around the call together with what opens it (an `if`,
            // a `match` arm, a closure), back to the statement before.
            let mut depth = 0usize;
            let mut open = at;
            loop {
                open -= 1;
                match code[open] {
                    b'}' => depth += 1,
                    b'{' if depth == 0 => break,
                    b'{' => depth -= 1,
                    _ => {}
                }
            }
            let head = (start..open)
                .rev()
                .find(|&j| matches!(code[j], b';' | b'{' | b'}'))
                .unwrap_or(start);
            let arm = &raw_text[head + 1..at];
            let hits: Vec<usize> = (0..FLASH_ONLY.len())
                .filter(|&k| arm.contains(FLASH_ONLY[k].0))
                .collect();
            match hits.as_slice() {
                [one] => found[*one] += 1,
                [] => bare.push(arm.split_whitespace().collect::<Vec<_>>().join(" ")),
                many => panic!("an arm matches more than one flash-only entry {many:?}: {arm}"),
            }
        }
        assert!(
            bare.is_empty(),
            "a refusal in build_ui only flashes; show its words with refuse_said (or refuse_text), \
             or list it with its reason: {bare:#?}"
        );
        for (k, (needle, count, why)) in FLASH_ONLY.iter().enumerate() {
            assert_eq!(found[k], *count, "flash-only arms holding {needle:?} ({why})");
        }
    }
}
