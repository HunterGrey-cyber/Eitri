//! shell: the real Eitri product window -- custom chrome, the module grid (a layout that is data),
//! the neovide-editor crate
//! embedded as the real editor pane, and the real agent-ui panel (a WebView-hosted frontend bridged
//! to a lazily-started `agent::AgentSession`). See
//! the crate's own module docs.

mod agent_panel;
mod chat_views;
mod chrome;
mod close_prompt;
mod companion;
mod compositor_mode;
mod editor_feeds;
mod editor_hole;
mod editor_presentation;
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
mod nvim_verbs;
mod onboarding_detect;
mod onboarding_host;
mod open_requests;
mod pane_switch;
mod panel_super;
mod plugin_page;
mod plugin_panels;
mod prefix_strip;
mod process_memory;
mod root_conflicts;
mod terminal;
mod text_size;
mod theme;
mod toast;
mod tray;
mod tutor;
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
use linked_window::focus::PairRaise;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::gdk::{Key, ModifierType};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, EventControllerKey};

use agent_panel::ViewId;
#[cfg(test)]
use eitri_core::attention::AgentPlace;
use eitri_core::attention::Attention;
use eitri_core::keymap::{Action, TabAction};
use eitri_core::layout::rules::without_keys;
use eitri_core::layout::{Direction, Layout, ModuleDecl, ModuleId, ModuleKeys, ModuleKind, OpenRule};
use eitri_core::plugin::theme::PageTheme;
use eitri_core::window_link::names::WindowName;
use eitri_core::wm::chrome::ChromeTitles;
use eitri_core::wm::module::SurfaceDecl;
use eitri_core::wm::start::StartStep;
use eitri_core::wm::toasts::ToastKind;
use eitri_core::wm::{
    ChatEvent, HintEvent, HintTarget, HostEvent, KeyStage, Origin, PointerEv, PointerTarget, SurfaceKind, TermEvent,
    Verdict, WinId,
};
use lua::{LuaEngine, PanelSlot};
use module_grid::{HostKind, ModuleGrid};
use neovide_editor::NeovideEditorPane;
use wire::{wire_host, WireDeps, WireKind};

const APP_ID: &str = "cn.huntergrey.eitri";
/// A page window's application id, the one its desktop file is named by.
const PAGE_APP_ID: &str = "cn.huntergrey.eitri.Page";

/// Which kind of process this is, and so which application id it registers: a full window or window 2 is the main
/// application, a chat window is the panel's, a page window has its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppKind {
    Main,
    Chat,
    Page,
}

impl AppKind {
    fn of(start: &linked_window::WindowStart) -> AppKind {
        match start {
            linked_window::WindowStart::Module {
                name: WindowName::Chat(_),
                ..
            } => AppKind::Chat,
            linked_window::WindowStart::Module {
                name: WindowName::Page(_),
                ..
            } => AppKind::Page,
            _ => AppKind::Main,
        }
    }

    fn id(self) -> &'static str {
        match self {
            AppKind::Main => APP_ID,
            AppKind::Chat => companion::PANEL_APP_ID,
            AppKind::Page => PAGE_APP_ID,
        }
    }
}

/// The module a chat or page window shows alone: a chat window shows its chat in its own base view, whatever its
/// number, and a page window its plugin's page.
fn module_of_window(name: &WindowName) -> Option<ModuleId> {
    match name {
        WindowName::Second => None,
        WindowName::Chat(_) => Some(ModuleId::agent()),
        WindowName::Page(_) => name.module(),
    }
}

/// How long the top bar's app name shows the prefix indicator's block when a layout verb is refused
/// (the refusal flashes the top bar, the same way the prefix indicator shows).
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
    // `eitri linked-window` is window 2 of a pair or, with `--module`, a chat or page window, started by window 1 with
    // the link's end inherited: that end is checked and made this process's own before anything else here can open,
    // inherit or hand on a descriptor, and the window's own log replaces the stdout and stderr it was started with.
    // Its project root is on its command line, so the project resolver below never reads these arguments.
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
    //
    // What a launch hands a file over with is read first: the scrub takes the one, GTK the other.
    let open_hint = open_requests::hint_from_env();
    let open_token = open_requests::token_from_env();
    eitri_core::companion::env::drop_inherited_editor_env();
    let has_flag = |flag: &str| eitri_core::project_root::flag_given(early_args.iter().map(OsString::as_os_str), flag);

    // `--version`, before anything else -- including the stdin
    // detach right below, which is otherwise the very first thing this binary does: printing a
    // version line must never touch GTK, a display, or nvim's stdin.
    // `eitri_core::project_root::KNOWN_FLAGS` also lists `--version`, so `shell --version
    // /some/project` is never a startup failure even though `main()` never reaches `resolve_launch()` on
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
    //
    // What the command line names to open comes out of the same resolution as nvim's own arguments; window 2 and a
    // module window have no editor and carry none.
    let (project_root, open) = match &start {
        linked_window::WindowStart::Second { root, .. } | linked_window::WindowStart::Module { root, .. } => {
            (root.clone(), Vec::new())
        }
        linked_window::WindowStart::Full => match eitri_core::project_root::resolve_launch() {
            Ok(launch) => {
                for note in &launch.notes {
                    eprintln!("eitri: {note}");
                }
                // Ahead of everything a window needs: a launch a running window takes ends here, with no GTK, no
                // display and no nvim. The one thread this may start only connects, and never reads the environment.
                if let Some(code) = open_requests::hand_over(
                    &launch,
                    want_clean,
                    backend_kind,
                    open_hint.as_deref(),
                    open_token.as_deref(),
                ) {
                    return code;
                }
                (launch.root, eitri_core::project_root::nvim_open_args(&launch.open))
            }
            Err(message) => {
                eprintln!("eitri: {message}");
                return glib::ExitCode::FAILURE;
            }
        },
    };
    println!("eitri: project root {}", project_root.display());
    // Once, before GTK: the `lstat` and the group files it reads belong off the GTK thread.
    let root_note = agent_panel::root_note_for(&project_root);

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
    let second = !matches!(start, linked_window::WindowStart::Full);
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

    // WebKit's sandbox (the Ubuntu user-namespace case):
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
    // read as the window is built). Finding it held, it asks once whether an orphaned window 2 holds it and waits
    // for a window 1, and links to it as window 1; otherwise it runs unpaired.
    // The compositor is asked whether it answers while GTK comes up, off this thread and from the environment alone;
    // the mode's resolution reads whatever has come in by then. After the environment has last been written above.
    if matches!(start, linked_window::WindowStart::Full) {
        let wm = eitri_core::compositor::detect(&|name: &str| std::env::var_os(name));
        compositor_mode::start_probe(wm);
    }
    let kind = AppKind::of(&start);
    let start = RefCell::new(Some(start));
    let app = build_application(kind);
    app.connect_activate(move |app| {
        // `activate` comes once for a window that opts out of single-instance behaviour.
        let Some(start) = start.borrow_mut().take() else { return };
        // Why an orphaned window 2 refused this window, shown once the window is up.
        let mut start_note: Option<String> = None;
        let pair = match start {
            linked_window::WindowStart::Full => {
                let layout_dir = eitri_core::layout::persist::state_dir(
                    std::env::var_os("XDG_STATE_HOME").as_deref(),
                    std::env::var_os("HOME").as_deref(),
                );
                let taken = layout_dir
                    .as_deref()
                    .ok_or_else(|| std::io::Error::other("no state directory"))
                    .and_then(|dir| eitri_core::window_link::pair::PairLock::try_take(dir, &project_root));
                let legacy = backend_kind == eitri_core::agent_backend::BackendKind::Legacy;
                use linked_window::relink::StartRelink;
                let relinked = match (&taken, layout_dir.as_deref()) {
                    // Before anything is drawn: the workers this waits for are bounded, and no window exists yet to
                    // stall.
                    (Ok(None), Some(dir)) => {
                        let (found, note) = linked_window::relink::relink_at_start(
                            &linked_window::relink::orphan_dir(),
                            dir,
                            &project_root,
                            version::fingerprint(),
                            linked_window::relink::RELINK_WITHIN,
                        );
                        start_note = note;
                        found
                    }
                    _ => StartRelink::Nothing,
                };
                match relinked {
                    StartRelink::Window2(relinked) => {
                        let pair = linked_window::spawn::Pair::relinked(relinked, &project_root, legacy, want_clean);
                        if let Some(link) = pair.link() {
                            link.watch();
                        }
                        pair
                    }
                    StartRelink::Modules(group, claim) => {
                        let pair = linked_window::spawn::Pair::relinked_group(group, &project_root, legacy, want_clean);
                        pair.hold_group_claim(Some(claim));
                        pair.watch_module_links();
                        pair
                    }
                    StartRelink::Nothing => {
                        // A first start leads its group too: an orphan a failed link left behind is not another
                        // start's to take while this window runs.
                        let claim = match (&taken, layout_dir.as_deref()) {
                            (Ok(Some(_)), Some(dir)) => {
                                eitri_core::window_link::pair::GroupClaim::try_take(dir, &project_root)
                                    .inspect_err(|err| println!("[window] the group cannot be claimed: {err}"))
                                    .unwrap_or_default()
                            }
                            _ => None,
                        };
                        let pair = linked_window::spawn::Pair::full(taken, &project_root, legacy, want_clean);
                        pair.hold_group_claim(claim);
                        pair
                    }
                }
            }
            linked_window::WindowStart::Second {
                sock, token, restore, ..
            } => linked_pair(
                linked_window::spawn::Pair::second(sock, &project_root),
                token,
                restore,
                backend_kind,
                want_clean,
            ),
            linked_window::WindowStart::Module {
                name,
                sock,
                token,
                restore,
                ..
            } => linked_pair(
                linked_window::spawn::Pair::module(sock, name, &project_root),
                token,
                restore,
                backend_kind,
                want_clean,
            ),
        };
        build_ui(
            app,
            want_clean,
            &project_root,
            backend_kind,
            panel_notice.as_deref(),
            root_note.clone(),
            pair,
            start_note,
            open.clone(),
        )
    });
    app.run_with_args::<&str>(&[])
}

/// The hub's compositor record is kept up to date only on a normal start: `--clean` neither reads it nor writes
/// the default set over it, so the next normal start still finds the saved windows.
fn cm_record_kept(
    start: Option<eitri_core::window_link::cm_file::CmFile>,
    want_clean: bool,
) -> Option<eitri_core::window_link::cm_file::CmFile> {
    start.filter(|_| !want_clean)
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
///
/// `eitri FILE` going to a running window changes none of this: the launch that hands its file over exits before it
/// gets here (`open_requests::hand_over`), so every window is still a process of its own.
fn build_application(kind: AppKind) -> Application {
    Application::builder()
        .application_id(kind.id())
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build()
}

/// A window started by window 1 takes its start's token and flags. With no link to window 1 it has nothing to be.
fn linked_pair(
    pair: std::io::Result<Rc<linked_window::spawn::Pair>>,
    token: Option<String>,
    restore: bool,
    backend_kind: eitri_core::agent_backend::BackendKind,
    want_clean: bool,
) -> Rc<linked_window::spawn::Pair> {
    match pair {
        Ok(pair) => {
            pair.set_activation(token);
            pair.set_restored(restore);
            pair.set_launch_flags(
                backend_kind == eitri_core::agent_backend::BackendKind::Legacy,
                want_clean,
            );
            pair
        }
        Err(err) => {
            eprintln!("eitri: the link to window 1 could not be started: {err}");
            std::process::exit(2);
        }
    }
}

/// What this window's start knows of itself before `init.lua` runs.
fn early_facts(pair: &linked_window::spawn::Pair, shows_chat: bool) -> eitri_core::wm::start::EarlyFacts {
    let env = |name: &str| std::env::var_os(name);
    let module_orphans = pair.relinked_module_windows();
    eitri_core::wm::start::EarlyFacts {
        started: pair.is_second(),
        shows_chat,
        mac: cfg!(target_os = "macos"),
        wayland: eitri_core::compositor::mode::wayland_session(&env),
        wm: eitri_core::compositor::detect(&env),
        window2_orphan: pair.relink_holders().is_some() && !module_orphans,
        module_orphans,
        unpaired: pair.is_unpaired(),
        alone: pair.is_alone(),
        choice: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_ui(
    app: &Application,
    want_clean: bool,
    project_root: &Path,
    backend_kind: eitri_core::agent_backend::BackendKind,
    panel_notice: Option<&str>,
    root_note: Option<String>,
    pair: Rc<linked_window::spawn::Pair>,
    start_note: Option<String>,
    open: Vec<String>,
) {
    use eitri_core::theme::start::{StartReport, StartTheme};
    use eitri_core::theme::store::ThemeWriter;

    // Window 2 of a pair has no editor: no nvim is started, so none of the editor's feeds, the pane-switch channel
    // or the scratch directory exists in it, and every use of the editor below is behind its `Option`.
    let owns_editor = pair.owns_editor();
    // A chat or page window shows its one module alone: no editor, no layout file, no top bar, and the only page it
    // builds is its own.
    let module_window: Option<(WindowName, ModuleId)> = pair
        .module_window()
        .and_then(|name| module_of_window(&name).map(|module| (name, module)));
    let shows_a_chat = module_window
        .as_ref()
        .is_none_or(|(name, _)| matches!(name, WindowName::Chat(_)));
    // Before anything else, so
    // `gtk-xft-dpi` is never unset by the time the agent panel's (or a Lua panel's) `WebView` reads
    // it. See `xft_dpi`'s module doc.
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
    // Every surface needs its start colours before its first widget is made.
    let theme_dir = eitri_core::theme::store::state_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    let start_theme = StartTheme::read(theme_dir.as_deref(), project_root, want_clean);
    if let Some(report) = start_theme.report() {
        match report {
            StartReport::Out(line) => println!("{line}"),
            StartReport::Err(line) => eprintln!("{line}"),
        }
    }
    let theme_css = theme::gtk_css::ThemeCss::install(&start_theme.tokens, start_theme.prefers_dark());
    // Built before the editor pane for the same reason `pane_switch` is: its env and `--cmd` reach
    // nvim only at spawn. `None` (logged) leaves the window on its start colours.
    // One timer for the four feeds below; each is read once its consumer calls `listen_*`.
    let feed_pump = editor_feeds::FeedPump::start();
    let mut theme_feed = owns_editor.then(theme::feed::ThemeFeed::new).flatten();
    let theme_writer = (owns_editor && !want_clean).then(|| {
        Rc::new(RefCell::new(ThemeWriter::new(
            theme_dir.clone(),
            project_root.to_path_buf(),
            start_theme.payload.clone(),
        )))
    });

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
    // The fourth push socket: nvim's `mapleader`, `timeoutlen`
    // and Normal-mode maps, built here for the same reason the other three are -- its `child_env()`
    // and `--cmd` must reach the editor pane's constructor, which cannot be handed to an already
    // running child.
    let mut nvim_keys_feed = owns_editor
        .then(eitri_core::nvim_keys::feed::NvimKeysFeed::new)
        .flatten();
    // `:EitriTutor`, installed in each nvim session once the feeds report it started. Only the window that
    // embeds nvim has one.
    let tutor = owns_editor.then(tutor::Tutor::new);
    let mut nvim_child_env = pane_switch.as_ref().map(|ps| ps.child_env()).unwrap_or_default();
    nvim_child_env.extend(theme_feed.as_ref().map(|feed| feed.child_env()).unwrap_or_default());
    nvim_child_env.extend(context_feed.as_ref().map(|feed| feed.child_env()).unwrap_or_default());
    nvim_child_env.extend(nvim_keys_feed.as_ref().map(|feed| feed.child_env()).unwrap_or_default());
    let mut nvim_extra_args = theme_feed.as_ref().map(|feed| feed.nvim_args()).unwrap_or_default();
    nvim_extra_args.extend(context_feed.as_ref().map(|feed| feed.nvim_args()).unwrap_or_default());
    nvim_extra_args.extend(nvim_keys_feed.as_ref().map(|feed| feed.nvim_args()).unwrap_or_default());
    // The nav fallback's loader, from the pane-switch channel's own directory,
    // so `Ctrl+h/j/k/l` leave the editor without vim-tmux-navigator and from Visual mode. Only
    // where the channel exists, because leaving can only work where its socket does.
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
    let opens_files = !open.is_empty();
    let editor_spawn = eitri_core::wm::EditorSpawn {
        env: nvim_child_env,
        args: nvim_extra_args,
        cwd: Some(project_root.to_path_buf()),
        clean: want_clean,
        background: start_theme.editor_background(),
        open,
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
    // Bound here because the terminals made below are told its name; nothing is taken from it until the watch at the
    // end of this function, once the window can act on a request.
    let incoming_opens = matches!(editor_presence, eitri_core::wm::EditorPresence::Owned)
        .then(|| {
            let sink: Rc<dyn eitri_core::wm::HostSink> = driver.clone();
            open_requests::OpenRequests::claim(project_root, Rc::downgrade(&sink))
        })
        .flatten();
    driver.start(
        WinId::MAIN,
        StartStep::Window {
            spec: eitri_core::wm::WindowSpec {
                role: wm_role,
                presence: editor_presence,
                title: {
                    let project = eitri_core::wm::titles::project_display_name(project_root);
                    match &module_window {
                        Some((name, _)) => eitri_core::wm::titles::module_window_title(&project, name, None),
                        None => eitri_core::wm::titles::window_title(&project, None),
                    }
                },
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
    // Ctrl+h handler (to hand focus back) and the window's services (to quit, end and shut nvim down).
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
    let lua_engine = Rc::new(
        LuaEngine::showing(
            config_dir.clone(),
            module_window.as_ref().map(|(_, module)| module.clone()),
        )
        .expect("LuaEngine construction must not fail"),
    );
    // `eitri.window` names this window before `init.lua` runs, so a config both windows run can keep a side
    // effect to one of them.
    let named = match &module_window {
        Some((name, _)) => lua_engine.set_window_name(&name.text()),
        None => lua_engine.set_window(pair.pair_role()),
    };
    if let Err(err) = named {
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
    // The first panel document needs the editor's remembered size in WebKit's CSS pixels.
    let editor_font = Rc::new(std::cell::Cell::new(webkit_zoom::EditorFont::new(
        gtk4::Settings::default().map_or(xft_dpi::GTK_DEFAULT_XFT_DPI, |s| s.gtk_xft_dpi()),
        eitri_core::theme::guifont_size_px(start_theme.guifont()),
    )));
    // The panel without a page: the controller says when the page is built, and in a hub it never is.
    let agent_panel_handle = agent_panel::make_panel_handle(
        project_root.to_path_buf(),
        editor_context_source,
        scratch_dir,
        backend_kind,
        root_note,
        pair.pair_role(),
        {
            let mut tokens = start_theme.tokens.clone();
            if start_theme.payload.is_some() {
                tokens.font_size_px = editor_font.get().css_px();
            }
            tokens
        },
    );
    let base_page = gtk_host_surfaces::BasePage::new(agent_panel_handle.clone(), panel_notice);
    wm_host.set_base_page_maker(base_page.clone());
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
    // Turn review across linked windows, in a window that can be one of a group: one presence with the other windows,
    // the overlap notes across them, every linked window's review in window 1's editor, and window 1 deciding every
    // revert of a group. Wired to each link as it comes up.
    let pair_review = match (&window1_editor, &window2_editor) {
        (Some(one), _) if pair.holds_pair() => Some(linked_window::review::Role::One(one.clone())),
        (None, Some(two)) if pair.is_module() => Some(linked_window::review::Role::Module(two.ops())),
        (None, Some(two)) => Some(linked_window::review::Role::Two(two.ops())),
        _ => None,
    }
    .map(|role| {
        let side: Rc<dyn linked_window::review::ReviewSide> = Rc::new(agent_panel_handle.clone());
        linked_window::review::GroupReview::new(project_root, side, role, editor_repeat.clone())
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

    // `init.lua`: its Lua panels and the plugins it loads are registered before the config is read. A window that may
    // yet turn out a hub, by what `init.lua` says, builds no panel's view until its mode is known; the base page's
    // start step comes from the same facts.
    let mut early = early_facts(&pair, shows_a_chat);
    driver.set_early_facts(WinId::MAIN, early);
    if eitri_core::wm::start::base_page_at(&early) != eitri_core::wm::start::PageAt::Kernel {
        lua_engine.panels.borrow_mut().hold_views();
    }
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
    // The window's mode, known as soon as `init.lua` has run: what `window.tiling` says, the pair's facts and whether
    // this project is a plugin's own window. A hub keeps no tabs of its own and hosts no panel.
    let tiling_choice = config.tiling;
    early.choice = Some(tiling_choice);
    driver.set_early_facts(WinId::MAIN, early);
    let plugins = lua_engine
        .plugins
        .borrow()
        .plugins()
        .iter()
        .filter_map(|registration| {
            registration
                .plugin
                .window_root
                .as_ref()
                .map(|root| (registration.plugin.manifest.id.clone(), root.clone()))
        })
        .collect::<Vec<_>>();
    let own_window = eitri_core::wm::start::own_window(project_root, &plugins, pair.pair_role());
    // Whether the compositor answered is read as it stands, never waited for: no answer yet counts as not live.
    let (tiling, resolved) = if module_window.is_some() {
        (eitri_core::wm::window::Tiling::ModuleWindow, None)
    } else {
        let (tiling, mode, why) = early.tiling(own_window.is_some(), compositor_mode::probe_now());
        (tiling, (!early.started).then_some((mode, why)))
    };
    // What the window says of the mode it opened in, shown with the first toast; a window another started has no
    // mode of its own to say anything of.
    let mut back_from_compositor = false;
    let (mode_notes, first_toast) = resolved
        .map(|(mode, why)| {
            let state_dir = eitri_core::layout::persist::state_dir(
                std::env::var_os("XDG_STATE_HOME").as_deref(),
                std::env::var_os("HOME").as_deref(),
            );
            let prev = state_dir
                .as_deref()
                .map(|dir| eitri_core::compositor::mode::read_mode_state(dir, project_root))
                .unwrap_or_default();
            back_from_compositor = mode == eitri_core::compositor::mode::Mode::Eitri
                && prev.last == Some(eitri_core::compositor::mode::Mode::Compositor);
            let settled = compositor_mode::settle(mode, why, early.wm, tiling_choice, prev);
            println!("{}", settled.log);
            if let Some(dir) = state_dir.as_deref().filter(|_| settled.state != prev) {
                if let Err(err) = eitri_core::compositor::mode::write_mode_state(dir, project_root, &settled.state) {
                    println!("[mode] could not record the mode in {}: {err}", dir.display());
                }
            }
            (settled.notes, settled.first_toast)
        })
        .unzip();
    let hub = tiling == eitri_core::wm::window::Tiling::Hub;
    lua_engine.panels.borrow_mut().settle_views(!hub);
    if hub {
        agent_panel_handle.keep_no_tabs();
        agent_panel::start_review_pump(&agent_panel_handle);
    } else if module_window.is_some() {
        // Window 1 saves the group's tabs: this window tells it its own.
        agent_panel_handle.keep_no_tabs();
    }
    config.apply_to_panel(&agent_panel_handle);
    let keymap = config.keymap.clone();
    if let Some(tutor) = &tutor {
        tutor.set_prefix(keymap.prefix().human());
    }
    for line in keymap.dropped_for_root() {
        eprintln!("[keymap] {line}");
    }

    // Each `eitri.plugin.load` becomes a Lua panel whose content is the plugin's page. Registered before the
    // Lua panels are read below, so a plugin is placed, keyed, killed and revived like any Lua panel. Its
    // pages take the same start colours as the agent panel and the chrome.
    let plugin_panels = match plugin_panels::PluginPanels::install(
        &lua_engine,
        &keymap,
        &PageTheme::from_tokens(&start_theme.tokens),
    ) {
        Ok(panels) => Rc::new(panels),
        Err(message) => {
            eprintln!("eitri: {message}");
            std::process::exit(1);
        }
    };
    {
        let driver = Rc::downgrade(&driver);
        plugin_panels.set_move_events(Rc::new(move |event| {
            if let Some(driver) = driver.upgrade() {
                driver.post(event);
            }
        }));
    }
    if let Some(one) = &window1_editor {
        let panels = Rc::downgrade(&plugin_panels);
        let project = project_root.to_owned();
        let roots_of: linked_window::editor::PageRoots = Rc::new(move |owner| {
            let panels = panels.upgrade()?;
            panels.modules().contains(&ModuleId::lua(owner)).then(|| {
                panels
                    .plugin_roots(owner)
                    .unwrap_or_else(|| BTreeMap::from([("project".into(), project.clone())]))
            })
        });
        one.set_page_roots(roots_of.clone());
        let one = one.clone();
        let pair = Rc::downgrade(&pair);
        plugin_panels.set_holder_hook(Rc::new(move |owner, held| {
            let peer = pair
                .upgrade()
                .map_or(linked_window::spawn::SECOND_PEER, |pair| pair.page_peer(owner));
            let roots = if held { None } else { roots_of(owner) };
            one.set_plugin_in_window(owner, (!held).then_some(peer), roots);
        }));
    }
    // A page window is titled by its plugin's name, known only now that the plugins are loaded. Set once, before
    // the window is shown, and never again: window rules and switchers read the title as it is.
    if let Some((name @ WindowName::Page(id), _)) = &module_window {
        let plugin_title = lua_engine
            .plugins
            .borrow()
            .plugins()
            .iter()
            .find(|registration| registration.plugin.manifest.id == *id)
            .map(|registration| registration.plugin.manifest.title.clone());
        let panel_titles: Vec<(ModuleId, String)> = lua_engine
            .panels
            .borrow()
            .entries()
            .iter()
            .map(|entry| (entry.module.clone(), entry.title.clone()))
            .collect();
        window.set_title(Some(&eitri_core::wm::titles::module_window_title(
            &eitri_core::wm::titles::project_display_name(project_root),
            name,
            eitri_core::wm::titles::page_name(id, plugin_title.as_deref(), &panel_titles),
        )));
    }
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
        if let Some(one) = &window1_editor {
            for module in plugin_panels.modules() {
                let owner = module.as_str().trim_start_matches("lua:");
                if let Some(config) = plugin_panels.follow_config(owner, socket.socket_path(), scratch_path.as_deref())
                {
                    one.set_plugin_follow(owner, config);
                }
            }
        }
    }

    // The bottom terminal is a module
    // since the modules design. Hidden in a first launch, and
    // its shell is not started until it is first shown, by any route (the change hook below): a
    // window whose terminal is never shown starts no process, touches no font manager, and looks
    // exactly as it did before the terminal existed. A layout that reopens with it shown starts it
    // at launch.
    //
    // Every terminal of the window -- the base one and each numbered instance, each with its own PTY --
    // is in one registry keyed by its id. A killed instance's pane waits there, idle, and is reused
    // only by a new instance of the same number, whose host the grid already holds under that id.
    // New panes take the colours the window has now; the theme listener below keeps this current.
    let terminal_colors = Rc::new(std::cell::Cell::new(terminal::colors_from(&start_theme.tokens)));
    // Their face: the editor's `guifont`, under `init.lua`'s `terminal.font` (a bad value is a startup failure
    // naming the key); and the window's terminal text zoom, which the host's `TermOp::SetZoom` keeps current.
    let terminal_font = match eitri_core::lua::config::parse_terminal_font(
        lua_engine
            .config
            .borrow()
            .get(eitri_core::lua::config::TERMINAL_FONT_KEY),
    ) {
        Ok(setting) => setting,
        Err(message) => {
            eprintln!("eitri: {message}");
            std::process::exit(1);
        }
    };
    let terminal_face = Rc::new(RefCell::new(terminal::face_for(
        start_theme.guifont(),
        terminal_font.as_ref(),
    )));
    let terminal_zoom = Rc::new(std::cell::Cell::new(1.0_f32));
    let make_terminal_pane: Rc<dyn Fn() -> terminal::TerminalPane> = {
        let root = project_root.to_path_buf();
        let shell_env = open_requests::shell_env(incoming_opens.as_deref(), project_root);
        let colors = terminal_colors.clone();
        let face = terminal_face.clone();
        let zoom = terminal_zoom.clone();
        Rc::new(move || {
            let pane = terminal::TerminalPane::new(root.clone());
            pane.set_shell_env(shell_env.clone());
            // The floor `build_vertical_split` gave the bottom slot on `main`: a terminal dragged to
            // zero reports a 1-row grid to its shell.
            pane.widget().set_size_request(-1, layout::BOTTOM_MIN_HEIGHT);
            pane.set_colors(colors.get());
            // Before any metrics exist, so the first build is already in the window's face and size.
            pane.set_face(face.borrow().clone());
            pane.set_zoom(zoom.get());
            pane
        })
    };
    let terminals: Rc<gtk_host_services::Registry<terminal::TerminalPane>> =
        Rc::new(gtk_host_services::Registry::new());
    terminals.take_or_make(&ModuleId::terminal(), || make_terminal_pane());

    // The layout is data (the modules design):
    // `[editor | agent]`, the terminal below both
    // (hidden), then each Lua panel placed by its `position` in the order it was registered
    // (`terminal::initial_layout`, `Layout::initial`, `PanelSlot::placement`). The editor, the agent
    // and the terminal are not in the Lua registry at all: every window has all three, and a Lua
    // panel no longer replaces any of them -- a Lua `bottom` panel goes under the terminal.
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
    // first show opens it by its own rule (the controller's `open_plugin_module`).
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
    // Each Lua panel's module key after `Ctrl+a` (the modules design). A key that is reserved,
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
    // `:Eitri`, for window 1's nvim only: it sends its words to the pane-switch socket, and what it completes and
    // the group of keys under `keymap.nvim_prefix` come from this window's keymap and module keys.
    let nvim_verbs = nvim_verbs::NvimVerbs::for_window(WinId::MAIN, owns_editor, || {
        let socket = pane_switch.as_ref()?.socket_path().to_path_buf();
        let actions: Vec<String> = keymap.bindings().iter().map(|binding| binding.action.name()).collect();
        let keyed: Vec<ModuleId> = module_keys.entries().iter().map(|(_, id)| id.clone()).collect();
        let panels: Vec<ModuleId> = lua_keys.iter().map(|(id, _)| id.clone()).collect();
        let words = nvim_verbs::completion_words(&actions, &keyed, &panels);
        let panel_ids: Vec<String> = panels
            .iter()
            .filter_map(|id| id.as_str().strip_prefix("lua:").map(str::to_string))
            .collect();
        let keys = eitri_core::nvim_verbs::leader::leader_group(&keymap, &module_keys, &panel_ids);
        let group = config.nvim_prefix.as_deref().map(|lhs| (lhs, keys.as_slice()));
        Some(eitri_core::nvim_verbs::install::install_args(&socket, &words, group))
    });
    // The `?` overlay's window and prefix sections, generated from the keymap;
    // see `keys_help`. It is re-sent, only when the merged panel table changed, every time nvim's
    // own keys report changes (`FeedPump::listen_keys` below).
    let keys_help = eitri_panel::keys_help::KeysHelp::new(
        keymap.clone(),
        module_keys.clone(),
        agent_panel_handle.clone(),
        config.tmux_skipped.clone(),
        if module_window.is_some() {
            eitri_panel::keys_help::HelpScope::ModuleWindow
        } else {
            eitri_panel::keys_help::HelpScope::Window
        },
    );
    keys_help.send();
    // The first-start screen's host, filled in where the window's overlay and driver exist: each report of the editor's
    // mappings reaches it, and it keeps the last for a screen that opens later.
    let onboarding_slot: Rc<RefCell<Option<Rc<onboarding_host::OnboardingHost>>>> = Rc::default();
    // nvim's keys, from this window's own feed or, in window 2, as window 1 tells them.
    let apply_nvim_keys: Rc<dyn Fn(eitri_core::nvim_keys::NvimReport)> = {
        let keys_help = keys_help.clone();
        let plugin_panels = plugin_panels.clone();
        let onboarding_slot = onboarding_slot.clone();
        Rc::new(move |report: eitri_core::nvim_keys::NvimReport| {
            // A page's multi-key sequences wait as long as the editor's do.
            plugin_panels.set_timeoutlen(report.timeoutlen);
            let onboarding = onboarding_slot.borrow().clone();
            if let Some(onboarding) = onboarding {
                onboarding.nvim_report(report.clone());
            }
            keys_help.nvim_report(report);
        })
    };
    if let Some(feed) = nvim_keys_feed.as_mut() {
        let apply = apply_nvim_keys.clone();
        let plugin_panels = plugin_panels.clone();
        let one = window1_editor.clone();
        let tutor = tutor.clone();
        let nvim_verbs = nvim_verbs.clone();
        feed_pump.listen_keys(feed, move |report| {
            if let Some(one) = &one {
                one.keys_changed(&report);
            }
            apply(report);
            // The feed reports at nvim's VimEnter: a new editor session may be ready for the plugins' snippets.
            plugin_panels.editor_session();
            if let Some(tutor) = &tutor {
                tutor.editor_session();
            }
            if let Some(verbs) = &nvim_verbs {
                verbs.editor_session();
            }
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
    // The hint panel's delay, kept by the controller for the window the layout step makes (window 2 is this same
    // code in its own process).
    driver.set_hint_delay(WinId::MAIN, config.which_key_delay);
    // The layout this window opens with (the modules design): this project's state file if
    // it can be used, else `init.lua`'s `eitri.layout.default`, else the first launch. A malformed
    // default is a startup failure naming the call; a state file that cannot be used is not, since it
    // is state rather than config -- it is logged and the default opens.
    if let Some(message) = lua_engine.init_refusal() {
        eprintln!("eitri: {message}");
        std::process::exit(1);
    }
    let editor_presentation = match editor_presentation::from_config(&lua_engine) {
        Ok(value) => value,
        Err(message) => {
            eprintln!("eitri: {message}");
            std::process::exit(1);
        }
    };
    if let (Some(pane), Some(value)) = (&pane, editor_presentation) {
        pane.set_presentation(value);
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
    // The pair file, read once by the window that holds the pair, before any other file of the pair (the saved tabs
    // just below, the layout files after them), and only once `init.lua` has run, since `window.restore` decides
    // whether window 2 comes back: which modules the file gives to window 2 and whether this window takes them back.
    // A window 2 reads it too, read-only: for its output, and only when restored for what it builds.
    let window_restore = match eitri_core::lua::config::parse_window_restore(
        lua_engine
            .config
            .borrow()
            .get(eitri_core::lua::config::WINDOW_RESTORE_KEY),
    ) {
        Ok(on) => on,
        Err(message) => {
            eprintln!("eitri: {message}");
            std::process::exit(1);
        }
    };
    // A chat or page window and a hub read and write none of the pair's files.
    let pair_layout_dir = eitri_core::wm::persist::layout_dir(
        eitri_core::layout::persist::state_dir(
            std::env::var_os("XDG_STATE_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        ),
        tiling,
    );
    let pair_restore = match pair_layout_dir.as_deref().filter(|_| pair.keeps_pair_file()) {
        Some(dir) => {
            // Window 2's own layout file, read here only to know it can be opened: one it would have to set aside
            // gives it nothing to come back to.
            let second_layout_usable = || {
                !matches!(
                    eitri_core::layout::persist::load_as(
                        dir,
                        project_root,
                        eitri_core::window_link::pair::Role::Second,
                        &decls,
                        &deferred,
                    ),
                    eitri_core::layout::persist::Loaded::Unusable(_)
                )
            };
            // A window that linked to an orphan at its start takes what that window holds as away; nothing of window 2
            // is started again or taken back.
            let plan = match pair.relink_holders() {
                Some(holders) => linked_window::pair_files::read_for_relink(dir, project_root, &holders),
                None => linked_window::pair_files::read_for_restore(
                    dir,
                    project_root,
                    window_restore,
                    &second_layout_usable,
                ),
            };
            for note in &plan.notes {
                println!("{note}");
            }
            plan
        }
        None => linked_window::pair_files::PairRestore::default(),
    };
    let second_restore = match pair_layout_dir
        .as_deref()
        .filter(|_| pair.is_second() && pair.restored())
    {
        Some(dir) => {
            let restore =
                linked_window::pair_files::second_restore(&eitri_core::window_link::pair::read_pair(dir, project_root));
            for note in &restore.notes {
                println!("{note}");
            }
            restore
        }
        None => linked_window::pair_files::SecondRestore::default(),
    };
    let second_placement = match pair_layout_dir
        .as_deref()
        .filter(|_| pair.is_second() && !pair.restored() && !want_clean)
    {
        Some(dir) => {
            linked_window::pair_files::second_placement(&eitri_core::window_link::pair::read_pair(dir, project_root))
        }
        None => None,
    };
    // The compositor group's record is separate from every tiled-window and pair file.
    let cm_layout_dir = (hub || module_window.is_some())
        .then(|| {
            eitri_core::state_paths::state_dir(
                std::env::var_os("XDG_STATE_HOME").as_deref(),
                std::env::var_os("HOME").as_deref(),
            )
        })
        .flatten();
    let cm_restore = if window_restore && !want_clean && (hub || pair.restored()) {
        cm_layout_dir.as_deref().and_then(
            |dir| match eitri_core::window_link::cm_file::read_cm(dir, project_root) {
                eitri_core::window_link::cm_file::ReadCm::Read(file, notes) => {
                    for note in notes {
                        println!("[layout] {note}");
                    }
                    Some(file)
                }
                eitri_core::window_link::cm_file::ReadCm::Unusable(why) => {
                    eprintln!("[layout] {why}");
                    None
                }
                eitri_core::window_link::cm_file::ReadCm::Missing => None,
            },
        )
    } else {
        None
    };
    let window2_restored = pair_restore.restore_second;
    // Window 1 offers window 2's saved tabs after its own when window 2 does not come back; a window 2 started again
    // offers its own. A hub is told none: giving them reads the files and claims other windows' tabs at once, and a
    // group's tabs belong to its chat windows.
    if !hub {
        agent_panel_handle.set_tab_files(linked_window::pair_files::tab_files(
            pair.pair_role(),
            pair.holds_pair() && !window2_restored && pair.relink_holders().is_none(),
            pair.holds_pair()
                && linked_window::pair_files::alone_on_project(
                    eitri_core::turn_review::review_dir_for(
                        std::env::var_os("XDG_STATE_HOME").as_deref(),
                        std::env::var_os("HOME").as_deref(),
                        project_root,
                    )
                    .as_deref(),
                ),
            // A chat or page window started with `--restore` only keeps the keys in window 1: it is no window 2 come
            // back, and offers no saved tabs of window 2's.
            pair.restored() && module_window.is_none(),
        ));
    }
    // A hub's tabs are the group's, one file for every chat window: read here once, and the windows that hold tabs open
    // at the start beside the layout's.
    let tab_windows = if hub && agent_panel_handle.remembers_tabs() {
        let group_tabs = linked_window::group_tabs::GroupTabs::read(
            eitri_core::saved_tabs::state_dir(
                std::env::var_os("XDG_STATE_HOME").as_deref(),
                std::env::var_os("HOME").as_deref(),
            )
            .as_deref(),
            project_root,
            linked_window::pair_files::alone_on_project(
                eitri_core::turn_review::review_dir_for(
                    std::env::var_os("XDG_STATE_HOME").as_deref(),
                    std::env::var_os("HOME").as_deref(),
                    project_root,
                )
                .as_deref(),
            ),
        );
        let windows = group_tabs.windows();
        pair.keep_group_tabs(group_tabs);
        windows
    } else {
        Vec::new()
    };
    if window2_restored {
        agent_panel_handle.hold_launch_restore();
    }
    if let Some(dir) = pair_layout_dir.as_deref() {
        let writing = pair.keeps_pair_file() && !pair_restore.in_the_way;
        let start = if writing {
            pair_restore.start.clone()
        } else if pair.restored() {
            // A window 2 started again keeps the pair as the file had it (window 1's monitor among it), open.
            second_restore.file.clone()
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
    // Where this window goes at its start: the monitor the pair file names for it, and, in the window holding the
    // pair, window 2 started again once this window's start is done.
    driver.set_placement(
        WinId::MAIN,
        eitri_core::wm::WindowPlacement {
            role: if pair.restored() {
                eitri_core::wm::PlacementRole::RestoredSecond {
                    // Window 1 spawns restored window 2 directly; the link uses the same parent identity.
                    first_pid: unsafe { libc::getppid() as u32 },
                }
            } else if module_window.is_some() {
                eitri_core::wm::PlacementRole::NewModule
            } else if pair.is_second() {
                eitri_core::wm::PlacementRole::NewSecond
            } else {
                eitri_core::wm::PlacementRole::Main
            },
            remembered: if let Some((name, _)) = &module_window {
                cm_restore
                    .as_ref()
                    .and_then(|file| file.output(name))
                    .map(str::to_owned)
            } else if pair.holds_pair() {
                linked_window::pair_files::window1_start_monitor(&pair_restore.start, window2_restored)
            } else if pair.restored() {
                second_restore.remembered.clone()
            } else {
                second_placement
            },
            restore_second: window2_restored,
        },
    );
    // With no state directory the window reads no layout file and its saver writes none.
    let state_dir = eitri_core::wm::persist::layout_dir(
        eitri_core::layout::persist::state_dir(
            std::env::var_os("XDG_STATE_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        ),
        tiling,
    );
    // The key that shows the editor, as the keymap binds it: named by the note a layout that hides
    // the editor prints, the way the toast names its own way back.
    let show_editor: Option<String> = keymap
        .keys_for(&Action::Module(ModuleId::editor()))
        .first()
        .map(|key| format!("{} {}", keymap.prefix().human(), key.human()));
    let has_own_window = own_window.is_some();
    let own_window_decl = own_window.as_ref().and_then(|id| {
        lua_engine
            .plugins
            .borrow()
            .plugins()
            .iter()
            .find(|registration| registration.plugin.manifest.id == *id)
            .and_then(|registration| registration.plugin.manifest.window.clone())
    });
    // What the window is named by, on its title and its top bar: a plugin's own window by its declared title.
    let chrome_project = ChromeTitles::project_name(
        project_root,
        own_window_decl.as_ref().map(|window| window.title.as_str()),
    );
    plugin_panels.set_window_root(project_root);
    plugin_panels.set_own_window(own_window.as_deref());
    let plugin_window =
        own_window
            .as_ref()
            .zip(own_window_decl.as_ref())
            .map(|(id, window)| eitri_core::wm::persist::PluginWindow {
                page: ModuleId::lua(id),
                tree: &window.layout,
                focus: window.focus,
            });
    // In a plugin's own window its page is read as a deferred module, so a file that lacks it leaves it out and the
    // page then comes back at its `[window]` place; the saver reads the file with the same lists.
    let (layout_decls, layout_deferred) =
        eitri_core::wm::persist::own_window_lists(plugin_window.as_ref().map(|window| &window.page), &decls, &deferred);
    // Each window opens its own file; an unpaired window with none of its own yet reads the pair's window 1 file
    // once, read-only.
    let loaded = state_dir.as_deref().map(|dir| {
        eitri_core::layout::persist::load_for_role(dir, project_root, pair.pair_role(), &layout_decls, &layout_deferred)
    });
    // A chat or page window starts from its one leaf in one workspace, whatever the files and `init.lua` say.
    let module_start = module_window.as_ref().map(|(_, module)| {
        eitri_core::layout::reconcile_module(eitri_core::layout::Node::Leaf(module.clone()), module)
            .map(|one| (eitri_core::layout::Workspaces::single(), one.layout, one.notes, None))
            .map_err(|err| format!("the layout of the window for {module} could not start: {err}"))
    });
    // `start_layout` is the current workspace's layout, `start_workspaces` the window's other workspaces beside it
    // and what a later build keeps in the file: this window's own values until the controller takes them, so what
    // the window's start owes its role (below) is written to them before anything reads them.
    let (mut start_workspaces, mut start_layout, keep_copy) = match module_start.unwrap_or_else(|| {
        eitri_core::wm::persist::choose_startup_layout(
            lua_default.as_ref(),
            plugin_window,
            loaded,
            &layout_decls,
            &rules,
            show_editor.as_deref(),
            &layout_deferred,
            &own_rules,
        )
    }) {
        Ok((workspaces, layout, notes, keep_copy)) => {
            for note in notes {
                println!("[layout] {note}");
            }
            (workspaces, layout, keep_copy)
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
        .filter(|_| tiling == eitri_core::wm::window::Tiling::Eitri)
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
        &mut start_workspaces,
        &mut start_layout,
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
    // A hub's shape is fixed: the layout chosen above decides which chats and pages open as windows, and nothing more.
    let mut restore_base: Option<(eitri_core::layout::Layout, eitri_core::layout::Workspaces)> = None;
    let mut hub_cm_start = None;
    if hub {
        let (mut layout, mut open) = eitri_core::wm::start::hub_layout(&start_layout);
        if let Some(file) = &cm_restore {
            layout = file.terminal.layout();
            open = file
                .ordered_windows()
                .into_iter()
                .filter(|name| {
                    let Some(module) = name.module() else { return false };
                    let available = module.kind() == ModuleKind::Agent
                        || decls.iter().any(|d| d.id == module)
                        || deferred.contains(&module);
                    if !available {
                        println!("[layout] {} is not registered; its window stays closed", name.text());
                    }
                    available
                })
                .collect();
        }
        let mut file = eitri_core::window_link::cm_file::CmFile::new(&layout, &open);
        for window in &mut file.windows {
            window.output = cm_restore
                .as_ref()
                .and_then(|file| file.windows.iter().find(|w| w.name == window.name))
                .and_then(|w| w.output.clone());
        }
        hub_cm_start = Some(file);
        driver.set_start_windows(
            WinId::MAIN,
            eitri_core::wm::start::StartWindows {
                open: linked_window::group_tabs::beside_the_layout(open, tab_windows),
                unnamed: if cm_restore.is_some() {
                    Vec::new()
                } else {
                    eitri_core::wm::start::unnamed_pages(&start_layout)
                },
            },
        );
        start_layout = layout;
        start_workspaces = eitri_core::layout::Workspaces::single();
    } else if pair.is_second() && module_window.is_none() {
        let frame = eitri_core::layout::Frame::new(eitri_core::layout::rules::NOMINAL_GRID, 1);
        if let Err(err) = linked_window::start_second(&mut start_layout, &mut start_workspaces, &frame) {
            eprintln!("eitri: window 2's layout could not start without its editor: {err}");
            std::process::exit(1);
        }
        // Started by `window.new`, the pair gives it nothing yet; started again at a launch, what the pair file says
        // it held.
        linked_window::pair_files::second_start_away(&mut start_layout, &mut start_workspaces, &second_restore.given);
        let can_build = |id: &ModuleId| {
            (id.number() >= 2 && id.kind().has_instances())
                || decls.iter().any(|d| d.id == *id)
                || deferred.contains(id)
        };
        for note in linked_window::pair_files::place_given(
            &mut start_layout,
            &mut start_workspaces,
            &second_restore.given,
            &rules,
            &decls,
            &can_build,
        ) {
            println!("{note}");
        }
    } else if pair.is_unpaired() {
        linked_window::ignore_away(&mut start_layout, &mut start_workspaces);
    } else if pair.holds_pair() {
        // Measured from before the restore: what the pair file changed here is a change, written at once.
        restore_base = Some((start_layout.clone(), start_workspaces.clone()));
        let can_build = |id: &ModuleId| {
            (id.number() >= 2 && id.kind().has_instances())
                || decls.iter().any(|d| d.id == *id)
                || deferred.contains(id)
        };
        for note in linked_window::pair_files::apply_main_restore(
            &pair_restore,
            &mut start_layout,
            &mut start_workspaces,
            &rules,
            &decls,
            &can_build,
        ) {
            println!("{note}");
        }
    } else {
        // Alone (the pair lock could not be had): no other window can hold anything, so nothing is away here.
        linked_window::ignore_away(&mut start_layout, &mut start_workspaces);
    }
    // The layout files stayed as they were through the compositor run, and the chats it left are in the tabs file, in
    // views: the window opens with those views, or every chat would come back in the base view's strip.
    let saved_views = (back_from_compositor
        && !want_clean
        && own_window.is_none()
        && pair.pair_role() == eitri_core::window_link::pair::Role::Main
        && agent_panel_handle.remembers_tabs())
    .then(|| {
        eitri_core::saved_tabs::state_dir(
            std::env::var_os("XDG_STATE_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )
    })
    .flatten()
    .map(|dir| match eitri_core::saved_tabs::load(&dir, project_root) {
        eitri_core::saved_tabs::Loaded::Saved(saved) => eitri_core::saved_tabs::views_holding(&saved),
        _ => Vec::new(),
    })
    .unwrap_or_default();
    for note in eitri_core::wm::persist::tile_saved_views(&mut start_layout, &mut start_workspaces, &saved_views) {
        println!("[layout] {note}");
    }
    for module in plugin_panels.modules() {
        if start_layout.is_away(&module) {
            plugin_panels.mark_away(&module);
            if let Some(one) = &window1_editor {
                let owner = module.as_str().trim_start_matches("lua:");
                one.set_plugin_in_window(
                    owner,
                    Some(pair.page_peer(owner)),
                    Some(
                        plugin_panels
                            .plugin_roots(owner)
                            .unwrap_or_else(|| BTreeMap::from([("project".into(), project_root.to_owned())])),
                    ),
                );
            }
        }
    }
    let layout_saver = (!hub).then(|| {
        let mut layout_saver = layout_state::saver(
            state_dir,
            pair.pair_role(),
            project_root,
            &start_layout,
            &start_workspaces,
            layout_decls.clone(),
            keep_copy,
        );
        layout_saver.set_deferred(layout_deferred.clone());
        if let Some((layout, spaces)) = restore_base.take() {
            layout_saver = layout_saver.with_base(&layout, &spaces);
            if layout_saver.submit_now(&start_layout, &start_workspaces) {
                println!("[layout] the layout as the pair file left it is written");
            }
        }
        layout_saver
    });
    // The window's layouts and its saver, given to the controller before any hook that posts to it. From here on
    // the controller is the one writer of both; `module_layout` and `workspaces` are every reader's views on them.
    let parts = eitri_core::wm::WindowParts::new(start_layout, start_workspaces, layout_saver);
    let (module_layout, workspaces) = parts.views();
    let cm_start = cm_record_kept(hub_cm_start, want_clean);
    let cm_session = cm_start.zip(cm_layout_dir.as_deref()).and_then(|(file, dir)| {
        linked_window::focus::CmSession::new(dir, project_root, file)
            .inspect_err(|err| eprintln!("[layout] compositor layout writer: {err}"))
            .ok()
            .map(|session| Rc::new(RefCell::new(session)))
    });
    driver.start(
        WinId::MAIN,
        StartStep::Layout {
            parts,
            own_window,
            tiling,
        },
    );
    // The base page's place, whichever start step built it; a window that builds none has an empty place no host takes.
    let agent_widget: gtk4::Widget = base_page
        .take()
        .unwrap_or_else(|| gtk4::Box::new(gtk4::Orientation::Vertical, 0).upcast());
    // What the verbs that differ between `c1` and the other chat windows read.
    if let Some((name, _)) = &module_window {
        driver.set_module_name(WinId::MAIN, name.clone());
    }
    // Every module's host, added once and never reparented (`module_grid`'s module doc). It arranges through
    // the controller, which holds the window's layout from the phase above, and the host drives and reads it.
    let grid = ModuleGrid::new(module_layout.clone());
    // An arriving page is covered with the grid's background until it is revealed; it follows the theme below.
    grid.set_page_cover(&start_theme.tokens);
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
    let chat_views: gtk_host_surfaces::ChatPool = Rc::new(gtk_host_services::Registry::new());
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
        let layout = module_layout.read();
        let workspaces = workspaces.read();
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
        let layout = module_layout.read();
        let workspaces = workspaces.read();
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
        // A hub makes the editor's host and the bottom terminal's: chats and pages are windows of their own.
        if hub {
            made.retain(|decl| matches!(decl.module.kind(), ModuleKind::Editor | ModuleKind::Terminal));
        }
        // A chat or page window makes the host of its one module and no other.
        if let Some((_, module)) = &module_window {
            made.retain(|decl| decl.module == *module);
            if made.is_empty() {
                eprintln!("eitri: this window shows {module}, which has no page in this project");
                std::process::exit(1);
            }
        }
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
    // Hosts already know the window's ownership before their first page is built. Shown-start plugins
    // get their pages now; a hidden-start plugin waits until its layout shows it, here or through the lifecycle.
    let shown_at_start: Vec<ModuleId> = {
        let layout = module_layout.read();
        plugin_panels
            .modules()
            .into_iter()
            .filter(|id| !plugin_panels.starts_hidden(id) || layout.is_shown(id))
            .filter(|_| !hub)
            .filter(|id| module_window.as_ref().is_none_or(|(_, module)| id == module))
            .collect()
    };
    for id in &shown_at_start {
        plugin_panels.build_page(id, &plugin_panels.theme());
    }
    // The panel's LIVE size starts at the base and is kept current by the host's SetFontSize op.
    // `panel_tokens` reads this rather than the
    // captured `panel_font_size` so a colorscheme change re-derives every OTHER token but never
    // resets a zoom back to the un-zoomed base.
    // The panel's text equals the editor's font size (`editor_font`, made above before the first document)
    // unless `agent.font_size` is set. It is in the panel's CSS px, which differ from the editor's logical px
    // by WebKitGTK's page zoom (`webkit_zoom`).
    let panel_base = if config.panel_font_follows_editor {
        editor_font.get().css_px()
    } else {
        config.panel_font_size
    };
    let panel_px = Rc::new(std::cell::Cell::new(panel_base));
    // What the controller's module lifecycle calls: a terminal's shell started the first time it is shown,
    // however it got there, a start-hidden plugin's page built on its first show, each plugin host told
    // whether its module is drawn, and a killed module shown again started fresh. Given before the startup
    // `apply`, whose lifecycle effects come right after it.
    // What the teardown ends beyond the modules is handed over once the window's build has made all of it (below).
    let teardown: Rc<RefCell<Option<Rc<gtk_host_services::Teardown>>>> = Rc::default();
    // One handle on this window's editor, shared: the window's own opens, the chat's turn review and every plugin
    // page ask through it, with two refusals: the editor gone from the window, or its nvim not running (ended, or a
    // restarted one still starting).
    let editor_rpc: Rc<dyn eitri_core::editor_rpc::EditorRpc> = match &pane {
        Some(pane) => {
            let module_layout = module_layout.clone();
            let editor_present: Rc<dyn Fn() -> bool> =
                Rc::new(move || !module_layout.read().is_gone(&ModuleId::editor()));
            Rc::new(editor_rpc::EmbeddedEditorRpc::new(pane.clone(), editor_present))
        }
        None => Rc::new(linked_window::NoEditor),
    };
    wm_host.set_services(Rc::new(PanelServices {
        cm_session: cm_session.clone().map(|session| (session, app.clone())),
        inner: gtk_host_services::Services {
            editor: pane.clone(),
            editor_rpc: editor_rpc.clone(),
            terminals: terminals.clone(),
            plugin_panels: plugin_panels.clone(),
            agent: agent_panel_handle.clone(),
            panel_px: panel_px.clone(),
            terminal_zoom: terminal_zoom.clone(),
            lua_pages: lua_pages.clone(),
            project_dir: project_root.to_path_buf(),
            sink: {
                let sink: Rc<dyn eitri_core::wm::HostSink> = driver.clone();
                Rc::downgrade(&sink)
            },
            teardown: teardown.clone(),
            freezes: Rc::default(),
            review_hold: pair_review.as_ref().map(|review| {
                let review = Rc::downgrade(review);
                let hold: gtk_host_services::ReviewHold = Rc::new(move |session: &str, hold: bool, here: bool| {
                    review
                        .upgrade()
                        .ok_or_else(|| "the pair's review has ended".to_string())?
                        .hold_session(session, hold, here)
                });
                hold
            }),
        },
    }));
    // The startup layout is the controller's first layout batch: what the layout shows mapped (every host
    // starts off screen for it), the float band stacked, the grid allocated, then the lifecycle effects of the
    // modules the surfaces phase declared -- each shown terminal's shell, each shown start-hidden page, each
    // plugin told whether it is drawn. The launch is not a change, so nothing is saved for it. Every host the
    // window opens with was made by the walk, so no module is left to declare here.
    if let Some(session) = &cm_session {
        let (session, layout) = (session.clone(), module_layout.clone());
        grid.connect_changed_by_controller(move || {
            if let Some(layout) = layout.try_read() {
                session.borrow_mut().terminal(&layout);
            }
        });
    }
    grid.take_every_host_off_screen();
    driver.first_apply(WinId::MAIN, Vec::new());
    // Every change of the arrangement is written to this project's state file, 500ms after the last
    // one, and once more when the window closes if it is still unwritten; a change of the keys alone
    // never is. Every change is the controller's, which arms the save's debounce itself. A pinned row's
    // first length is settled inside the controller's allocation, which tells the saver itself.

    // Gives a module the keys, as the controller's `Focus` op does: a plugin's page with no view here built first, the
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
            // A plugin's slot holds no view until its page is built: a page that starts hidden before its first
            // show, any page after its view was dropped when it left for the other window. An empty slot refuses
            // the grab, and a workspace switch and an open give the keys before the change hooks that build it run,
            // so the page is built here first, with no layout borrow held.
            if !plugin_panels.is_built(id) {
                plugin_panels.activate_holding(id);
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
    wm_host.set_focus(grab_module);

    // The chrome: the top bar, the window's content, and every controller and hook, installed in the order below.
    // The window has no HeaderBar (it is undecorated): the custom top bar built here (chrome::build_top_bar) is
    // its only titlebar.
    driver.start(WinId::MAIN, StartStep::Chrome);

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.add_css_class("shell-root");
    let backdrop = editor_hole::HoleBackdrop::new();
    let backdrop_color = |bg: eitri_core::theme::color::Rgb| {
        gtk4::gdk::RGBA::new(
            f32::from(bg.r) / 255.0,
            f32::from(bg.g) / 255.0,
            f32::from(bg.b) / 255.0,
            1.0,
        )
    };
    backdrop.set_background(backdrop_color(start_theme.tokens.bg));
    backdrop.set_hole_widget(pane.as_ref().map(|pane| pane.widget().upcast_ref()));

    // `app.reload-agent-panel` + `prefix r`; the top bar's own `⟳` button points at the same
    // action. Everything about it -- why it is an app action, what it costs, what is still
    // unverified -- is in `agent_panel::install_reload_action`'s doc comment. One call rather than
    // an inline block on purpose: `build_ui` is being edited by two other tracks of the same plan.
    agent_panel::install_reload_action(app, &agent_panel_handle, {
        let module_layout = module_layout.clone();
        Rc::new(move || {
            module_layout
                .try_read()
                .map_or(ViewId::BASE, |layout| keys_chat_view(&layout))
        })
    });
    // The panel starts on the same start colours the chrome does, then both follow nvim together.
    // So does the editor's own clear colour: `neovide-editor` snaps the grid's sub-cell remainder
    // to the TOP edge so the bottom sits flush against the window's edge, which puts a thin band
    // outside the rect the renderer paints. Left at that crate's default it is a near-black line
    // under the top bar -- the same defect the `CONTENT_MARGIN` removal fixed -- so it follows
    // `bg`, and is then the same colour as the first text row below it.
    let editor_clear = |tokens: &eitri_core::theme::ThemeTokens| {
        let bg = tokens.bg;
        (bg.r, bg.g, bg.b)
    };
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
        let start_tokens = start_theme.tokens.clone();
        move |payload: Option<&eitri_core::theme::payload::NvimThemePayload>| {
            let mut tokens = match payload {
                Some(p) => eitri_core::theme::ThemeTokens::derive(p),
                None => start_tokens.clone(),
            };
            tokens.font_size_px = text_size::live_panel_font_size_px(&panel_px);
            // `live_panel_font_size_px` is unit-tested in `text_size.rs`, but this GTK closure has no
            // headless test to catch using the captured startup `panel_font_size` instead. Check in the
            // GUI that changing the colorscheme after zooming keeps the panel zoomed.
            tokens.editor_row_px = editor_row.get().css_px();
            tokens
        }
    };
    agent_panel_handle.set_theme(&panel_tokens(start_theme.payload.as_ref()));
    if let Some(pane) = &pane {
        pane.set_clear_color(editor_clear(&start_theme.tokens));
    }
    // Each terminal pane took the start colours when created by `make_terminal_pane`. When the
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
    // `g:neovide_scale_factor` back from nvim; the prefix's one-pane `=`/`-`/`0` reaches the text verb runner
    // directly once the controller has resolved which pane holds the keys.
    // Every full window has the editor: a Lua `main` panel hides it rather than replacing it, and a text-size
    // write to an nvim that has not started yet is buffered by the pane (`ScaleWatch`). Window 2 has none,
    // so its text size is its panels' alone.
    // Seed `reported_s` from the pane's own live value (its `1.0` default before nvim exists is exactly
    // `NeovideEditorPane::scale_factor_setting`'s own documented fallback), not this module's own separate
    // assumption -- a value `init.lua` already set before this controller was constructed must not be forgotten.
    let reported_s = pane.as_ref().map(|pane| pane.scale_factor_setting()).unwrap_or(1.0);
    driver.set_text_size(WinId::MAIN, panel_base, reported_s);
    let text_size_controller = text_size::TextSizeController::install(app, pane.clone(), driver.clone());
    text_size_controller.set_base(panel_base);
    // nvim's theme, from this window's own feed or, in window 2, as window 1 tells it.
    let apply_theme: Rc<dyn Fn(eitri_core::theme::payload::NvimThemePayload)> = {
        let theme_css = theme_css.clone();
        // What the stylesheet paints, for the widgets GTK leaves on the old one (`theme::restyle`).
        let window_for_theme = window.clone();
        let agent_panel_handle = agent_panel_handle.clone();
        let pane_for_theme = pane.clone();
        let terminals = terminals.clone();
        let terminal_colors = terminal_colors.clone();
        let terminal_face = terminal_face.clone();
        let plugin_panels = plugin_panels.clone();
        let editor_font = editor_font.clone();
        let text_size_controller = text_size_controller.clone();
        let follows_editor = config.panel_font_follows_editor;
        let grid_for_theme = grid.downgrade();
        let backdrop = backdrop.downgrade();
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
            if let Some(backdrop) = backdrop.upgrade() {
                backdrop.set_background(backdrop_color(tokens.bg));
            }
            if let Some(grid) = grid_for_theme.upgrade() {
                grid.set_page_cover(&tokens);
            }
            agent_panel_handle.set_theme(&panel_tokens(Some(&payload)));
            if let Some(pane) = &pane_for_theme {
                pane.set_clear_color(editor_clear(&tokens));
            }
            // Every terminal pane, idle ones too, so one reused later is already in the theme; and
            // the colours a pane made later starts with.
            let colors = terminal::colors_from(&tokens);
            terminal_colors.set(colors);
            for pane in terminals.each() {
                pane.set_colors(colors);
            }
            // Every terminal's face follows the editor's `guifont`, the parts `terminal.font` names aside; a new
            // face rebuilds each shown terminal's metrics and resizes its PTY.
            let face = terminal::face_for(&payload.options.guifont, terminal_font.as_ref());
            if *terminal_face.borrow() != face {
                terminal_face.replace(face.clone());
                for pane in terminals.each() {
                    pane.set_face(face.clone());
                }
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
        let tutor = tutor.clone();
        let nvim_verbs = nvim_verbs.clone();
        let text_size_controller = text_size_controller.clone();
        feed_pump.listen_theme(feed, move |payload| {
            if let Some(w) = &theme_writer {
                w.borrow_mut().remember(&payload);
            }
            if let Some(scale) = payload.options.start_scale {
                text_size_controller.set_configured(scale);
            }
            if let Some(one) = &one {
                one.theme_changed(&payload);
            }
            apply(payload);
            // The feed reports at nvim's VimEnter: a new editor session may be ready for the plugins' snippets.
            plugin_panels.editor_session();
            if let Some(tutor) = &tutor {
                tutor.editor_session();
            }
            if let Some(verbs) = &nvim_verbs {
                verbs.editor_session();
            }
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

    let top_bar = chrome::build_top_bar(&window, &chrome_project);
    // A chat or page window shows no top bar: its widgets still exist, so every hook that reads one finds it, but
    // the bar has no parent and is hidden, so the toast and the y/n prompt place themselves as if it were away.
    if module_window.is_none() {
        root.append(&top_bar.widget);
    } else {
        top_bar.widget.set_visible(false);
    }
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
    // A chat or page window's top bar is never shown: the strip floats over its page instead, or `Ctrl+b` would change
    // nothing on screen there.
    let floating_strip = module_window
        .is_some()
        .then(|| prefix_strip::floating_frame(&prefix_strip));
    if floating_strip.is_none() {
        top_bar.strip.append(prefix_strip.widget());
    }
    let top_items: Vec<gtk4::Widget> = std::iter::once(top_bar.workspace.clone().upcast())
        .chain(tray.items())
        .chain(top_bar.items.iter().cloned())
        .collect();
    // The measured root supplies the window's minimum size above the backdrop. HINT labels, toasts and prompts
    // share its overlay and draw above the content; the HINT labels never take a click.
    let hint_overlay = gtk4::Overlay::new();
    hint_overlay.set_child(Some(&backdrop));
    hint_overlay.add_overlay(&root);
    hint_overlay.set_measure_overlay(&root, true);
    if let Some(frame) = &floating_strip {
        hint_overlay.add_overlay(frame);
    }
    window.set_child(Some(&hint_overlay));
    if let Some(pane) = &pane {
        let refresh = {
            let pane = Rc::downgrade(pane);
            let window = window.downgrade();
            let backdrop = backdrop.downgrade();
            let grid = grid.downgrade();
            move || {
                let (Some(pane), Some(window), Some(backdrop), Some(grid)) =
                    (pane.upgrade(), window.upgrade(), backdrop.upgrade(), grid.upgrade())
                else {
                    return;
                };
                let open = pane.presentation_hole().is_some();
                if open {
                    window.add_css_class("editor-hole");
                } else {
                    window.remove_css_class("editor-hole");
                }
                backdrop.set_open(open);
                let hole = (open && pane.widget().is_mapped())
                    .then(|| pane.widget().compute_bounds(&grid))
                    .flatten();
                grid.set_editor_hole(hole);
            }
        };
        refresh();
        pane.connect_hole_changed(refresh);
    }
    let toast = toast::Toast::install(&hint_overlay, &top_bar.widget);
    // The first-start screen: above the toast, below the y/n, so a y/n raised over it stays visible and a toast sits
    // under it.
    let onboarding_screen = gtk_host::onboarding::OnboardingScreen::new();
    hint_overlay.add_overlay(onboarding_screen.widget());
    // A window 1 an orphaned window 2 refused runs unpaired: the user sees why, and the refusal says what brings the
    // pair back.
    // The mode's own notes follow it: they are said once, and a toast that replaced the relink's would hide it.
    let notices: Vec<&str> = start_note
        .iter()
        .chain(mode_notes.iter().flatten())
        .map(String::as_str)
        .collect();
    if !notices.is_empty() {
        toast.show_for(&notices.join("; "), toast::NOTICE_FOR);
    }
    // The window's y/n, for a window close that would interrupt a running tab and for `prefix x`. Built here,
    // before everything that asks it; its key layer is added with the window's others, in their declared order
    // (`gtk_host::keys::install_window_layers`, below).
    let close_prompt = close_prompt::ClosePrompt::new(
        &hint_overlay,
        &top_bar.widget,
        top_bar.strip.upcast_ref::<gtk4::Widget>(),
    );
    // The hint panel while the prefix waits: an overlay child like the prompt above, never a click target.
    let prefix_panel = Rc::new(gtk_host::prefix_panel::PrefixPanelView::new());
    hint_overlay.add_overlay(prefix_panel.widget());
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

    // A root key of `init.lua` that an input method or the desktop may take first is logged, and toasted once per
    // change. Nothing starts when the root table is empty.
    root_conflicts::start(keymap.root_bindings(), toast.clone());

    // The global `f` HINT (`hint.rs`). Three
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

    // The first-start screen's work: the checks, the detection and the write on worker threads, the marker, and what
    // the screen starts in the editor. Its ops are the controller's `HostOp::Onboarding`; what it learns comes back
    // as events.
    let onboarding_host = {
        let panels_of = lua_engine.clone();
        let host = onboarding_host::OnboardingHost::new(onboarding_host::Deps {
            driver: Rc::downgrade(&driver),
            win: WinId::MAIN,
            screen: onboarding_screen.clone(),
            config_dir: config_dir.clone(),
            marker: eitri_core::onboarding::marker::state_file(
                std::env::var_os("XDG_STATE_HOME").as_deref(),
                std::env::var_os("HOME").as_deref(),
            ),
            keymap: keymap.clone(),
            lang: eitri_core::onboarding::Lang::from_env(&|name| std::env::var_os(name)),
            tmux: config.tmux_import.clone(),
            trust_open: {
                let panel = agent_panel_handle.clone();
                Rc::new(move || panel.trust_question_open())
            },
            panels: Rc::new(move || {
                let entries = panels_of.panels.borrow();
                let ids = entries.entries().iter().map(|entry| entry.id.clone()).collect();
                let claims = eitri_core::keymap::LuaClaims {
                    panel_keys: entries
                        .entries()
                        .iter()
                        .filter_map(|entry| entry.key.clone().map(|key| (entry.id.clone(), key)))
                        .collect(),
                    command_keybindings: panels_of
                        .commands
                        .borrow()
                        .iter()
                        .filter_map(|(id, entry)| entry.keybinding.clone().map(|key| (id.clone(), key)))
                        .collect(),
                };
                (ids, claims)
            }),
            notice: {
                let toast = toast.clone();
                let project_root = project_root.to_path_buf();
                Rc::new(move |text| {
                    toast.show_notice_when_free(text);
                    compositor_mode::toast_went_up(&project_root);
                })
            },
            view_in_editor: {
                let panel = agent_panel_handle.clone();
                Rc::new(move |title: &str, text: &str| panel.view_in_editor(title, text))
            },
            log: onboarding_host::log_from_env(&|name| std::env::var_os(name)),
        });
        *onboarding_slot.borrow_mut() = Some(host.clone());
        {
            let host = Rc::downgrade(&host);
            onboarding_screen.on_button(move |key| {
                if let Some(host) = host.upgrade() {
                    host.pressed(key);
                }
            });
        }
        {
            let screen = onboarding_screen.clone();
            wm_host.set_onboarding_focus(Rc::new(move |_| screen.grab_keys()));
        }
        {
            let host = host.clone();
            wm_host.set_onboarding(Rc::new(move |op| host.apply(op)));
        }
        {
            let host = Rc::downgrade(&host);
            agent_panel::on_trust_question_change(Rc::new(move |open| {
                if let Some(host) = host.upgrade() {
                    host.trust_changed(open);
                }
            }));
        }
        host
    };

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
        let panel = prefix_panel.clone();
        let (workspace_button, workspace_label) = (top_bar.workspace.clone(), top_bar.workspace_label.clone());
        let other_label = top_bar.other.clone();
        let (selection_label, mark_label) = (top_bar.selection.clone(), top_bar.mark.clone());
        wm_host.set_chrome_widgets(
            WinId::MAIN,
            gtk_host::chrome::ChromeWidgets {
                workspace: Box::new(move |project, text| {
                    chrome::show_workspace(&workspace_button, &workspace_label, project, text)
                }),
                selection: Box::new(move |text| chrome::show_mark(&selection_label, text)),
                mark: Box::new(move |text| chrome::show_mark(&mark_label, text)),
                other: Box::new(move |text| chrome::show_mark(&other_label, text)),
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
                    if let Some(frame) = &floating_strip {
                        frame.set_visible(pieces.is_some_and(|pieces| !pieces.is_empty()));
                    }
                    strip.show(pieces.unwrap_or(&[]));
                }),
                prefix_panel: Box::new(move |model| match model {
                    Some(model) => panel.show(model),
                    None => panel.hide(),
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
                // The first-start screen, drawn from the model's view; the host logs each change when asked to.
                onboarding: Box::new({
                    let screen = onboarding_screen.clone();
                    let host = Rc::downgrade(&onboarding_host);
                    move |view| {
                        screen.show(view);
                        if let Some(host) = host.upgrade() {
                            host.view_drawn(view);
                        }
                    }
                }),
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
            project: chrome_project.clone(),
            lua: lua_titles.clone(),
            tray: tray_modules.clone(),
        },
    );
    if let Err(why) = wm_host.draw_chrome(WinId::MAIN) {
        eprintln!("[wm] BUG: the window's chrome was not drawn: {why}");
    }
    if let Some(text) = first_toast.flatten() {
        driver.hold_first_toast(WinId::MAIN, text);
    }
    // With no `init.lua`, the first launch of a full window offers to write one. Decided on a worker once the config
    // phase is over; the screen opens only if the answer comes, and every other start is left as it was.
    onboarding_host.start_trigger(eitri_core::onboarding::trigger::Inputs {
        clean: want_clean,
        config_dir_env: std::env::var_os("EITRI_CONFIG_DIR"),
        first_start_env: std::env::var_os("EITRI_FIRST_START"),
        full_window: !pair.is_second() && !pair.is_unpaired(),
        own_window: has_own_window,
        opens_files,
        config_dir: config_dir.clone(),
        marker: eitri_core::onboarding::marker::state_file(
            std::env::var_os("XDG_STATE_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        ),
    });

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
        let driver = Rc::downgrade(&driver);
        let on_permission = config.on_permission;
        // What each chat view held when the controller was last told, so it hears only of the views that changed.
        let told: RefCell<Vec<(ViewId, Attention)>> = RefCell::default();
        // Cloned so the closure can call back into the handle for each view's attention and newest
        // card: the hook fires from the pump (`report_attention`) after its own state borrow is
        // already dropped, so this is safe and never re-enters a held `RefCell`.
        let agent_panel_handle_for_toast = agent_panel_handle.clone();
        let on_attention = move |_before: Attention, _after: Attention, handed: &[ViewId]| {
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
                use eitri_core::wm::verbs::chat::AttentionChoice;
                let mine = agent.attention_of(view);
                let choice = driver.query_or(AttentionChoice::Toast, |c| {
                    c.attention(WinId::MAIN, view, on_permission, mine)
                });
                let choice = match choice {
                    AttentionChoice::Reveal(event) => {
                        driver.post(HostEvent::Chat(event.clone()));
                        driver.query_or(AttentionChoice::Toast, |c| c.attention_revealed(&event))
                    }
                    choice => choice,
                };
                if choice == AttentionChoice::Toast {
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
    {
        let driver = Rc::downgrade(&driver);
        agent_panel_handle.on_busy(move |changed| {
            let Some(driver) = driver.upgrade() else { return };
            for &(view, busy) in changed {
                driver.post(HostEvent::Chat(eitri_core::wm::ChatEvent::Busy {
                    win: WinId::MAIN,
                    view,
                    busy,
                }));
            }
        });
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
    // A card answer from a chat view while a float covers that view is refused and the card stays waiting: the
    // answer would be for a card the user may not see. The controller's to judge, at the moment of each answer, for
    // the view whose page sent it; what it cannot judge it refuses, and its refusal is the toast.
    {
        let driver = Rc::downgrade(&driver);
        panel_hooks.set_answer_gate = Some(Rc::new(move |view: ViewId| {
            let Some(driver) = driver.upgrade() else {
                return Err(eitri_core::wm::feedback::LAYOUT_CHANGING_TEXT.to_string());
            };
            let answer = driver.query_or(
                Err(eitri_core::wm::feedback::LAYOUT_CHANGING_TEXT.to_string()),
                move |c| c.answer_gate(WinId::MAIN, view),
            );
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
        agent_panel_handle.set_coverage(Rc::new(move |view: ViewId| match module_layout.try_read() {
            Some(layout) => !view_covering(&grid, &layout, view).is_empty(),
            None => true,
        }));
    }
    // A plugin page's `layout.focus`: the controller's, as the module key lands, so a hidden editor is shown first;
    // the page is answered with its refusal. Its `terminal.at` is the controller's too, answered by an op.
    {
        let driver = Rc::downgrade(&driver);
        let at = driver.clone();
        plugin_panels.set_layout(
            Rc::new(move |id: &ModuleId| match driver.upgrade() {
                Some(driver) => driver.plugin_focus(WinId::MAIN, id),
                None => Err("the window is gone".to_string()),
            }),
            Rc::new(move |plugin: &str, dir: PathBuf, ticket: u64| match at.upgrade() {
                Some(driver) => driver.plugin_terminal_at(WinId::MAIN, plugin, dir, ticket),
                None => Err("the window is gone".to_string()),
            }),
        );
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
    // The scratch round trips of window 2 (`Ctrl+g`, `gf`): the controller's, as a chat's own request is. It
    // refuses an editor gone, not this window's, ended or still starting, and returns show failures to the caller.
    // It shows the editor and gives it the keys before nvim is handed the request. With no request, only the
    // editor shown and given the keys.
    let drive_editor: Rc<dyn Fn(Option<&eitri_core::scratch::ScratchRequest>) -> Result<(), String>> = {
        let driver = Rc::downgrade(&driver);
        Rc::new(move |request| {
            let Some(driver) = driver.upgrade() else {
                return Err("the window is gone".to_string());
            };
            driver.editor_request(
                WinId::MAIN,
                request.map(|request| request.input_keys()).unwrap_or_default(),
            )
        })
    };
    {
        let drive_editor = drive_editor.clone();
        let two = window2_editor.clone();
        let asking = window.downgrade();
        panel_hooks.on_editor_request = Some(Box::new(move |request: &eitri_core::scratch::ScratchRequest| {
            // A linked editor receives the request through its own transport and owns its layout there.
            if let Some(two) = &two {
                return two.request(request, &|| activation_token(&asking));
            }
            drive_editor(Some(request))
        }));
    }
    // How either window of the pair comes forward from the other, the compositor's way.
    let pair_raise = PairRaise::new(config.companion_wm);
    if module_window.is_some() {
        pair_raise.prepare_module();
    }
    // A restored window's first show asks for no focus of its own. Placement returns the keys to window 1 on
    // sway and Hyprland after mapping, including when the compositor chooses the remembered output itself. Neither
    // does the first show of a window outside a pair. A chat or page window's first show only presents it with its
    // start's token: the compositor's own focus follows once it is mapped (the new module window's placement), so it
    // comes forward once.
    let raises = pair_raise.used_for_window(
        pair.holds_pair() || pair.is_second(),
        pair.restored() || module_window.is_some(),
        incoming_opens.is_some(),
    );
    // A window put back on its monitor moves through the same adapter, whatever its part in a pair.
    wm_host.set_monitor_placer(Rc::new(pair_raise.clone()));
    if raises {
        // Every present of a window of the pair goes through this raise, the startup's first show included: there
        // too sway, Hyprland and niri focus the window by its pid, unless that show keeps the keys (a window 2
        // started again at a launch). So does the present of a file handed to this window, whatever its part in a
        // pair.
        wm_host.set_pair_raise(Rc::new(pair_raise.clone()));
    }
    {
        let wm = Rc::downgrade(&driver);
        // The pair file names this window's monitor, written at once when it changes (the other window's comes over
        // the link).
        let own_window = if pair.is_second() {
            linked_window::pair_files::WINDOW_2
        } else {
            linked_window::pair_files::WINDOW_1
        };
        let reports = RefCell::new(linked_window::pair_files::MonitorReports::default());
        let pair_for_monitor = Rc::downgrade(&pair);
        let cm_session = cm_session.clone();
        linked_window::focus::watch(
            &window,
            WinId::MAIN,
            Rc::new(move |event| {
                if let (Some(session), HostEvent::MonitorsChanged { monitors }) = (&cm_session, &event) {
                    session.borrow_mut().outputs_changed(monitors);
                }
                if let Some(monitor) = reports.borrow_mut().report(&event) {
                    if let Some(pair) = pair_for_monitor.upgrade() {
                        pair.with_pair_writer(|writer| writer.monitor(own_window, monitor));
                    }
                }
                if let Some(wm) = wm.upgrade() {
                    wm.post(event);
                }
            }),
        );
    }
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
    // Turn review's questions to the editor.
    {
        panel_hooks.set_editor_rpc = Some(editor_rpc.clone());
        if let Some(one) = &window1_editor {
            one.set_rpc(editor_rpc.clone());
        }
        if let Some(tutor) = &tutor {
            tutor.set_editor(editor_rpc.clone());
        }
        if let Some(verbs) = &nvim_verbs {
            verbs.set_editor(editor_rpc.clone());
        }
        onboarding_host.set_editor(editor_rpc.clone());
        plugin_panels.set_editor(editor_rpc);
        if let Some(two) = &window2_editor {
            plugin_panels.set_link_editor(two.ops());
        }
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
        let driver = driver.clone();
        let two = window2_editor.clone();
        let asking = window.downgrade();
        panel_hooks.on_review_open = Some(Box::new(move || {
            if let Some(two) = &two {
                if let Err(err) = two.focus_editor(activation_token(&asking)) {
                    eprintln!("[review] could not bring the editor window forward: {err}");
                }
                return;
            }
            driver.post(HostEvent::Chat(ChatEvent::ReviewOpen { win: WinId::MAIN }));
        }));
    }
    {
        let driver = driver.clone();
        panel_hooks.on_editor_done = Some(Box::new(move |view: ViewId| {
            driver.post(HostEvent::Chat(ChatEvent::EditorDone { win: WinId::MAIN, view }));
        }));
    }
    {
        let driver = driver.clone();
        panel_hooks.on_view_empty = Some(Box::new(move |view: ViewId| {
            driver.post(HostEvent::Chat(ChatEvent::ViewEmpty { win: WinId::MAIN, view }));
        }));
    }
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
    // is false. The controller decides what goes back -- the keys, the hidden chat, the zoom -- from the opening
    // its panel verb kept, and only for the latest opening.
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
    // The shell ending by itself -- `exit`, `Ctrl+d`, a signal -- is told to the controller, which closes that
    // terminal as `prefix x` does, without asking, or, as the last module on screen, closes the window through its
    // own close (`eitri_core::wm::close::on_shell_exit`), leaving the terminal's notice on screen until the window
    // really goes. A report from a shell that is no longer the pane's current one (the instance was killed and its
    // number reused since) is not told at all. The hook never asks its pane to close itself: the kill's own hang-up
    // closes the pane.
    let wire_terminal: Rc<dyn Fn(&ModuleId, &terminal::TerminalPane)> = {
        let terminals = Rc::downgrade(&terminals);
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
                let terminals = terminals.clone();
                let driver = driver.clone();
                pane.on_shell_exit(move |reported| {
                    let Some(terminals) = terminals.upgrade() else {
                        // The window is going; nothing is left to close.
                        return false;
                    };
                    // An idle pane (`None`) is a killed instance: its shell's end is late too.
                    let current = terminals.get(&id).map(|pane| pane.generation());
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
                    false
                });
            }
        })
    };
    {
        for (id, pane) in terminals.live() {
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
        wm_host.set_chat_summary(Rc::new(move |_| agent.chat_summary()));
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
                background: agent.view_background_count(view),
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
    {
        let driver = driver.clone();
        panel_hooks.on_show_view = Some(Box::new(move |view: ViewId| {
            driver.post(HostEvent::Chat(ChatEvent::ShowView { win: WinId::MAIN, view }));
        }));
    }
    // `prefix F11`'s `SetMode`, carried out as the toggle the controller decided: the window's real mode is the
    // shell's (`window_mode`), which a bare F11 and nvim's `g:neovide_fullscreen` change too.
    {
        let window_modes = window_modes.clone();
        wm_host.set_mode_toggle(Rc::new(move |_| window_modes.toggle_immersive()));
    }
    {
        let driver = driver.clone();
        agent_panel_handle.on_chooser_partner_pick(move |view, session| {
            driver.post(HostEvent::Chat(ChatEvent::ChooserPartnerPick {
                win: WinId::MAIN,
                view,
                session,
            }));
        });
    }
    panel_hooks.on_tab_verb = Some(Box::new({
        let driver = driver.clone();
        move |view: ViewId, tab_action: TabAction| {
            driver.post(HostEvent::Chat(ChatEvent::TabVerb {
                win: WinId::MAIN,
                view,
                action: Action::Tab(tab_action),
            }));
        }
    }));
    // A chat's move answers later: the panel's freeze, with the move's own id, and the presence an arriving chat
    // waits for before move_done. A hand-over's answers go to window 1 over the link: no move of this controller's
    // carries one.
    panel_hooks.on_chat_move = Some(Box::new({
        let driver = driver.clone();
        let pair = Rc::downgrade(&pair);
        move |event: agent_panel::ChatMoveEvent| match event {
            agent_panel::ChatMoveEvent::HandOver(answer) => {
                if let Some(pair) = pair.upgrade() {
                    pair.answer_hand_over(answer);
                }
            }
            event => {
                if let Some(event) = chat_move_event(WinId::MAIN, event) {
                    driver.post(HostEvent::Chat(event));
                }
            }
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
    // The first-start screen's key log is installed after the layers, so it is read first and sees a key the prefix layer
    // goes on to consume (the layers' own rule: the last installed sees keys first).
    onboarding_host.watch_keys(window.upcast_ref());

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
        let lua_engine = lua_engine.clone();
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
            // nvim's `:confirm qall` was cancelled there: what that clears is the quit flow's.
            pane_switch::PaneMessage::QuitCancelled(g) => {
                if let Some(driver) = driver.upgrade() {
                    driver.post(HostEvent::Editor(eitri_core::wm::EditorEvent::QuitCancelled {
                        win: WinId::MAIN,
                        generation: u64::from(g),
                    }));
                }
            }
            // `:Eitri` in this window's nvim: the words are checked again here, whatever the snippet checked, since
            // the socket is only as private as its directory. A request is the controller's, as a Lua call's is.
            pane_switch::PaneMessage::Verb(words) => {
                let Some(driver) = driver.upgrade() else { return };
                let lua_panels: Vec<String> = lua_engine
                    .panels
                    .borrow()
                    .entries()
                    .iter()
                    .map(|entry| entry.id.clone())
                    .collect();
                match nvim_verbs::carry(&words, &lua_panels) {
                    nvim_verbs::Carried::Request(request) => driver.nvim_request(WinId::MAIN, request),
                    nvim_verbs::Carried::KeySheet(action) | nvim_verbs::Carried::Setup(action) => {
                        driver.run(WinId::MAIN, action, Origin::Lua)
                    }
                    nvim_verbs::Carried::Refused(why) => driver.nvim_refused(WinId::MAIN, &why),
                }
            }
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

    // nvim exiting on its own (e.g. `:qa!`), or ending a quit it was asked for: the quit flow decides what follows --
    // the window closes, the editor goes, or a quit still in flight is settled. Whether nvim had answered that quit
    // is read here, before the exit is posted, from the pane's watch as its worker settles it (a short wait on this
    // thread, the same one the exit has always made), so the controller never waits for it.
    if let Some(pane) = &pane {
        let review_for_exit = agent_panel_handle.clone();
        let plugin_panels = plugin_panels.clone();
        let exited = Rc::downgrade(pane);
        let driver_for_exit = Rc::downgrade(&driver);
        pane.on_exited_unrequested(move || {
            review_for_exit.review_editor_lost();
            // Before the exit is acted on, which may close the window: a page following the editor hears it ended.
            plugin_panels.editor_closed();
            let settled = exited
                .upgrade()
                .and_then(|pane| pane.settled_watched_call())
                .as_ref()
                .map(kill_pane::nvim_answer);
            if let Some(driver) = driver_for_exit.upgrade() {
                driver.post(HostEvent::Editor(eitri_core::wm::EditorEvent::Exited {
                    win: WinId::MAIN,
                    settled,
                }));
            }
        });
        // Each nvim the pane launches, the first and every restart's: the controller hears that the editor runs
        // again, and has what was installed into the nvim before installed into this one. A launch that failed
        // before it said why over the pane; the nvim now running there makes that message stale.
        let driver_for_ready = Rc::downgrade(&driver);
        let failure_label_for_ready = editor_start_failure_label.clone();
        pane.on_session_ready(move || {
            if let Some(label) = &failure_label_for_ready {
                editor_start_failure::hide(label);
            }
            if let Some(driver) = driver_for_ready.upgrade() {
                driver.post(HostEvent::Editor(eitri_core::wm::EditorEvent::Ready {
                    win: WinId::MAIN,
                }));
            }
        });
        let review_for_unreachable = agent_panel_handle.clone();
        let driver_for_unreachable = Rc::downgrade(&driver);
        pane.on_nvim_unreachable(move || {
            review_for_unreachable.review_editor_lost();
            if let Some(driver) = driver_for_unreachable.upgrade() {
                driver.post(HostEvent::Editor(eitri_core::wm::EditorEvent::Unreachable {
                    win: WinId::MAIN,
                }));
            }
        });
    }

    // Show nvim startup errors in the editor's area through `editor_start_failure`. Startup is
    // asynchronous: `LiveState` moves from `NotStarted` through `Starting` to `Ready` or `Failed`
    // across render callbacks, after the window is shown and initial focus is chosen. If the
    // failed editor holds the keys, move them to the chat so input is not trapped. The controller
    // handles that focus change through `keys_after_failure`. Returning to the failed editor still
    // allows Ctrl+h/j/k/l navigation out.
    if let (Some(pane), Some(editor_start_failure_label)) = (&pane, &editor_start_failure_label) {
        let editor_start_failure_label = editor_start_failure_label.clone();
        let driver_for_failure = Rc::downgrade(&driver);
        pane.on_start_failed(move |message| {
            editor_start_failure::show(&editor_start_failure_label, message);
            // A restart that failed leaves the editor ended, so the next show tries again rather than waiting for an
            // nvim that never comes; keys still in the failed editor go to the chat.
            if let Some(driver) = driver_for_failure.upgrade() {
                driver.post(HostEvent::Editor(eitri_core::wm::EditorEvent::StartFailed {
                    win: WinId::MAIN,
                    why: message.to_string(),
                }));
            }
        });
    }

    // `eitri.layout.*` from a command or an event handler: queued by the Lua half while the handler runs, carried
    // out here once it returns, so no layout change happens under a Lua call. Each refusal is logged, as a handler's
    // own error is. Every request goes whole to the controller, its gate and log line included;
    // `run` meets the runner's rules as a key does.
    let apply_layout_requests: Rc<dyn Fn()> = {
        let lua_engine = lua_engine.clone();
        let driver = driver.clone();
        Rc::new(move || {
            let requests = lua_engine.layout.borrow_mut().take_requests();
            if !requests.is_empty() {
                driver.post(HostEvent::Lua(eitri_core::wm::LuaEvent::LayoutRequests {
                    win: WinId::MAIN,
                    requests,
                }));
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
    let first = module_layout.read().focus().clone();
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
                .get(&ModuleId::terminal())
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
                    let before = views(&module_layout.read());
                    let driver = driver.upgrade().ok_or_else(|| "the window is gone".to_string())?;
                    // Applied before it returns: nothing else is applying while the probe runs.
                    let new = Action::InstanceNew {
                        kind: ModuleKind::Agent,
                    };
                    driver.run(WinId::MAIN, new, Origin::Lua);
                    let after = views(&module_layout.read());
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
            show_module: {
                let driver = Rc::downgrade(&driver);
                let module_layout = module_layout.clone();
                Rc::new(move |id: &ModuleId| {
                    let hidden = !module_layout.read().is_shown(id);
                    if let Some(driver) = driver.upgrade() {
                        // Applied before it returns: nothing else is applying while the probe runs.
                        driver.post(HostEvent::Lua(eitri_core::wm::LuaEvent::LayoutRequests {
                            win: WinId::MAIN,
                            requests: vec![eitri_core::lua::layout::LayoutRequest::Show(id.clone())],
                        }));
                    }
                    hidden && module_layout.read().is_shown(id)
                })
            },
            hide_module: {
                let driver = Rc::downgrade(&driver);
                Rc::new(move |id: &ModuleId| {
                    if let Some(driver) = driver.upgrade() {
                        driver.post(HostEvent::Lua(eitri_core::wm::LuaEvent::LayoutRequests {
                            win: WinId::MAIN,
                            requests: vec![eitri_core::lua::layout::LayoutRequest::Hide(id.clone())],
                        }));
                    }
                })
            },
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
        // What this window holds now, unless the layout is being written.
        let held_now: Rc<dyn Fn() -> Option<std::collections::BTreeSet<String>>> = {
            let module_layout = module_layout.clone();
            let workspaces = workspaces.clone();
            Rc::new(move || {
                let (Some(layout), Some(workspaces)) = (module_layout.try_read(), workspaces.try_read()) else {
                    return None;
                };
                Some(linked_window::held(&layout, &workspaces))
            })
        };
        let say_held: linked_window::spawn::LinkMade = {
            let held_now = held_now.clone();
            let said = said.clone();
            Rc::new(move |link| {
                if !link.linked() {
                    return;
                }
                if let Some(held) = held_now() {
                    said.say(link, held);
                }
            })
        };
        {
            let pair = Rc::downgrade(&pair);
            let say_held = say_held.clone();
            let held_now = held_now.clone();
            let tell: Rc<dyn Fn()> = Rc::new(move || {
                let Some(pair) = pair.upgrade() else { return };
                if let Some(link) = pair.link() {
                    say_held(&link);
                }
                if let Some(held) = held_now() {
                    pair.say_held_to_modules(&held);
                }
            });
            // Every change of what this window holds is the controller's, and ends with the allocation it asks for.
            grid.connect_changed_by_controller(move || tell());
        }
        let ever_linked = Rc::new(std::cell::Cell::new(false));
        let linked: linked_window::spawn::OnLinked = {
            let cm_session = cm_session.clone();
            let panel = agent_panel_handle.clone();
            let partner_window = if pair.is_second() { 1 } else { 2 };
            let ever_linked = ever_linked.clone();
            let wm = Rc::downgrade(&driver);
            let second = pair.is_second();
            let window1_editor = window1_editor.clone();
            let window2_editor = window2_editor.clone();
            let pair_review = pair_review.clone();
            let toast = toast.clone();
            let pair_weak = Rc::downgrade(&pair);
            let window_weak = window.downgrade();
            Rc::new(move |name, peer, link| {
                // The move protocol is the controller's: its messages, the link's wakes and its end reach it from here.
                let attach = || {
                    if let Some(wm) = wm.upgrade() {
                        link.attach_controller(WinId::MAIN, peer, name.clone(), &wm);
                    }
                };
                // A module window of this window 1: the editor and the turn review serve it as they serve window 2,
                // and a page window holds its plugin's page from here on, while the panel's partner and the pair file
                // serve window 2's one link.
                if !second && *name != WindowName::Second {
                    if let Some(session) = &cm_session {
                        session.borrow_mut().linked(peer, name);
                    }
                    if let (Some(pair), Some(held)) = (pair_weak.upgrade(), held_now()) {
                        pair.say_held_to_modules(&held);
                    }
                    if let Some(one) = &window1_editor {
                        one.linked(peer, name.clone(), link);
                    }
                    if let Some(review) = &pair_review {
                        review.linked(peer, link);
                    }
                    attach();
                    return;
                }
                panel.set_partner(link.partner().map(|p| (p.pid, p.start, partner_window).into()));
                ever_linked.set(true);
                toast.set_standing(None);
                // A window 2 started again at this launch is back: this window's own saved tabs are all it restores.
                panel.release_launch_restore();
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
                    one.linked(peer, name.clone(), link);
                }
                if let Some(two) = &window2_editor {
                    two.linked(link);
                }
                if let Some(review) = &pair_review {
                    review.linked(peer, link);
                }
                attach();
                // A chat or page window that took the keyboard before its link was up could not say so then: it
                // says so now, or window 1 would send a chat key to the window that held the keys before it.
                if window_weak.upgrade().is_some_and(|window| window.is_active()) {
                    if let Some(pair) = pair_weak.upgrade() {
                        pair.say_window_focused();
                    }
                }
                // Tabs that changed with no link up go now, and a window 1 new to this window hears what it holds.
                if let Some(pair) = pair_weak.upgrade() {
                    pair.say_tabs();
                }
            })
        };
        let post: Rc<dyn Fn(eitri_core::wm::PeerEvent)> = {
            let cm_session = cm_session.clone();
            let wm = Rc::downgrade(&driver);
            Rc::new(move |event| {
                if let Some(session) = &cm_session {
                    session.borrow_mut().event(&event);
                }
                if let Some(wm) = wm.upgrade() {
                    wm.post(HostEvent::Peer(event));
                }
            })
        };
        let ended: linked_window::spawn::OnEnded = {
            let cm_session = cm_session.clone();
            let panel = agent_panel_handle.clone();
            let module_layout = module_layout.clone();
            let workspaces = workspaces.clone();
            let wm = Rc::downgrade(&driver);
            let toast = toast.clone();
            let second = pair.is_second();
            let app = app.downgrade();
            let pair_weak = Rc::downgrade(&pair);
            let window1_editor = window1_editor.clone();
            let window2_editor = window2_editor.clone();
            let pair_review = pair_review.clone();
            Rc::new(move |name, peer, link, end| {
                // A module window held nothing of this window's, and its link's end detaches its peer by itself; the
                // turn review and the editor forget it as they forget window 2.
                if !second && *name != WindowName::Second {
                    if let Some(session) = &cm_session {
                        session
                            .borrow_mut()
                            .link_ended(peer, name, end, link.partner().is_some());
                    }
                    println!("[window] {} ended ({end:?})", name.text());
                    if let Some(review) = &pair_review {
                        review.unlinked(peer);
                    }
                    if let Some(one) = &window1_editor {
                        one.unlinked(peer);
                    }
                    return;
                }
                panel.set_partner(None);
                // The partner counts as another window from here on: it may have died, and this window cannot see
                // its state.
                if let Some(review) = &pair_review {
                    review.unlinked(peer);
                }
                if let Some(one) = &window1_editor {
                    one.unlinked(peer);
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
                // What this window held away for the other window comes back, or goes, as the controller decides:
                // its batch lays the change out, then tells the pair file (an orphaned window 2 keeps it from now
                // on, naming what it holds) and loads pages whose return needs it.
                // This hook runs from the link's fd watch while the driver is idle, so the post is handled
                // synchronously: Orphaned takes over the pair file before window 2 waits for a relink below.
                if let Some(wm) = wm.upgrade() {
                    wm.post(HostEvent::Pair(eitri_core::wm::PairEvent::LinkEnded {
                        win: WinId::MAIN,
                        who,
                    }));
                }
                if let Some(pair) = pair_weak.upgrade() {
                    if second && !pair.waits_for_relink_after(end) {
                        println!("[window] window 1 closed the group; this window closes with it");
                        if let Some(wm) = wm.upgrade() {
                            wm.post(HostEvent::GroupClosed { win: WinId::MAIN });
                        }
                        return;
                    } else if second {
                        // A window 1 started for the project now finds this window waiting, and links to it.
                        let held_now: linked_window::relink::HeldNow = {
                            let module_layout = module_layout.clone();
                            let workspaces = workspaces.clone();
                            Rc::new(move || match (module_layout.try_read(), workspaces.try_read()) {
                                (Some(layout), Some(workspaces)) => linked_window::held(&layout, &workspaces),
                                _ => Default::default(),
                            })
                        };
                        if let Err(why) = pair.wait_for_relink(&linked_window::relink::orphan_dir(), held_now) {
                            eprintln!("[window] window 2 cannot wait for a window 1: {why}");
                        }
                    } else {
                        pair.window2_link_ended(link, end);
                    }
                }
                if second {
                    // It stays up for as long as this window has no editor window to ask.
                    toast.set_standing(Some(linked_window::ORPHAN_NOTE));
                }
            })
        };
        pair.set_hooks(linked_window::spawn::PairHooks { linked, post, ended });
        // A shell this window took over from the other window ends there, in its parent: how it ended comes back here.
        let terminals = Rc::downgrade(&terminals);
        pair.set_child_exited(Rc::new(move |pid, start, status| {
            let given = terminals
                .upgrade()
                .is_some_and(|terminals| gtk_host_services::child_exited_here(&terminals, pid, start, status));
            if !given {
                println!("[terminal] the shell {pid} ended; no terminal here runs it");
            }
        }));
        {
            // A chat or page window this window started that never links: the controller drops what waited for it,
            // so its key starts it again. On an idle: the hook runs from the link's own pump.
            let wm = Rc::downgrade(&driver);
            pair.on_module_failed(Rc::new(move |name: &WindowName, why: &str| {
                let (wm, name, why) = (wm.clone(), name.clone(), why.to_string());
                glib::idle_add_local_once(move || {
                    if let Some(wm) = wm.upgrade() {
                        wm.post(HostEvent::ModuleWindowFailed {
                            win: WinId::MAIN,
                            name,
                            why,
                        });
                    }
                });
            }));
        }
        // A window 2 started again at this launch that does not come back -- its start refused, or it ended or never
        // finished its handshake -- leaves this window holding placeholders nobody builds: they come back here as at
        // a link's end, the pair file says window 2 is not open, and window 2's saved tabs are offered. On an idle:
        // a refused start is told from inside the controller's own batch.
        if pair.holds_pair() {
            let wm = Rc::downgrade(&driver);
            pair.on_restore_failed(Rc::new(move |why: &str| {
                eprintln!("[window] window 2 does not come back ({why}); what it held comes back here");
                let wm = wm.clone();
                // The controller takes the placeholders back and lays the change out; its batch then tells the
                // pair file window 2 is not open, offers window 2's saved tabs, and loads the pages that came back.
                glib::idle_add_local_once(move || {
                    if let Some(wm) = wm.upgrade() {
                        wm.post(HostEvent::Pair(eitri_core::wm::PairEvent::RestoreFailed {
                            win: WinId::MAIN,
                        }));
                    }
                });
            }));
        }
    }
    if module_window.is_some() {
        // What window 1 asks of this window's tabs in a hand-over.
        pair.set_tab_holder(Rc::new(linked_window::hand_over::PanelTabs::new(
            agent_panel_handle.clone(),
            Rc::downgrade(&driver),
        )));
        // Each change of this window's tabs, for window 1 to save.
        {
            let pair = Rc::downgrade(&pair);
            agent_panel_handle.report_tabs(move |tabs| {
                if let Some(pair) = pair.upgrade() {
                    pair.tabs_changed(tabs.clone());
                }
            });
        }
        // What window 1 sends to run here: an action by its name and options, parsed as a key's binding is and run as
        // it is, never sent back. A name that does not parse here is logged and nothing runs.
        let driver = Rc::downgrade(&driver);
        let panels = lua_engine.panels.clone();
        pair.set_peer_action(Rc::new(move |name, opts| {
            let ids: Vec<String> = panels.borrow().entries().iter().map(|e| e.id.clone()).collect();
            if let Some(action) = linked_window::registry::peer_action(&name, &opts, &ids) {
                if let Some(driver) = driver.upgrade() {
                    driver.run(WinId::MAIN, action, eitri_core::wm::Origin::Hub);
                }
            }
        }));
        // Window 1 is told each time this window takes the keyboard, for the window the keys are in.
        let pair = Rc::downgrade(&pair);
        window.connect_is_active_notify(move |window| {
            if window.is_active() {
                if let Some(pair) = pair.upgrade() {
                    pair.say_window_focused();
                }
            }
        });
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
        // Copies taken when Lua asks, through the controller's views: a handler never holds the layout.
        let (layout, spaces) = (module_layout.clone(), workspaces.clone());
        store.attach(eitri_core::lua::layout::Query::new(move || layout.copy()));
        store.attach_workspaces(eitri_core::lua::layout::Query::new(move || spaces.copy()));
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
    // dashboard's own UI, or another shell instance, asking this window to raise itself.
    {
        let raised = window.clone();
        agent_panel::watch_supervisor(agent_panel_handle.clone(), Rc::new(move || raised.present()));
    }

    // What the window's teardown ends beyond its modules, handed to its services now that each is made and wired:
    // held there, each lives exactly as long as the window, and each is removed by its own teardown step.
    *teardown.borrow_mut() = Some(Rc::new(gtk_host_services::Teardown {
        app: app.clone(),
        pair: pair.clone(),
        scratch_path: scratch_path.clone(),
        pane_switch,
        theme_feed,
        context_feed,
        nvim_keys_feed,
        follow_socket,
        tutor,
        open_requests: incoming_opens.clone(),
        lua_engine: lua_engine.clone(),
    }));
    // Last: the answer to a request goes through the teardown's handle, and a request is acted on as soon as it is read.
    if let Some(requests) = &incoming_opens {
        requests.watch();
    }

    // A termination signal closes the window without its questions: nobody may be there to answer, and the process
    // was told to end. The controller tears the window down at once, nvim ended as a close ends it (never with
    // `:qa!`: stdin closed, SIGTERM, SIGKILL by its pid after five seconds), its swap files kept. The close is marked
    // first, so the watchdog lets a second signal force.
    // The log lines are written only if stdout takes them at once: a panic in this callback aborts the process
    // before the close, and a write blocked on a full pipe would hold the close up.
    {
        let ending_by_signal = std::cell::Cell::new(false);
        let driver = Rc::downgrade(&driver);
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
            if let Some(driver) = driver.upgrade() {
                driver.post(HostEvent::EndSignal { name });
            }
        });
        if let Err(err) = installed {
            eprintln!("[window] BUG: the termination signals were not handled ({err:?}); they end the process at once");
        }
    }

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

/// Adds the panel's effects and read-only facts to the window's module services.
/// A move's answer from the panel, as the controller hears it: a frozen tab's descriptors become the handoff the
/// link sends unread.
fn chat_move_event(win: WinId, event: agent_panel::ChatMoveEvent) -> Option<ChatEvent> {
    use agent_panel::ChatMoveEvent as Panel;
    use eitri_core::window_link::moves::MoveId;
    Some(match event {
        Panel::Frozen { view, mv, record, fds } => ChatEvent::Frozen {
            win,
            view,
            mv: MoveId(mv),
            record,
            handoff: gtk_host::peer::handoff_of(fds),
        },
        Panel::FreezeRefused { view, mv, why } => ChatEvent::FreezeRefused {
            win,
            view,
            mv: MoveId(mv),
            why,
        },
        Panel::PresenceHeld { mv } => ChatEvent::PresenceHeld { win, mv: MoveId(mv) },
        Panel::PresenceRefused { mv, why } => ChatEvent::PresenceRefused {
            win,
            mv: MoveId(mv),
            why,
        },
        Panel::LastTabClosed => ChatEvent::TabLeft { win },
        Panel::HandOver(_) => return None,
    })
}

struct PanelServices {
    cm_session: Option<(Rc<RefCell<linked_window::focus::CmSession>>, gtk4::Application)>,
    inner: gtk_host_services::Services,
}

impl gtk_host::ServiceSet for PanelServices {
    fn page_built(&self, module: &ModuleId) -> bool {
        self.inner.page_built(module)
    }

    fn tab_can_step(&self, delta: i32) -> bool {
        self.inner.tab_can_step(delta)
    }
    fn tab_has_last(&self) -> bool {
        self.inner.tab_has_last()
    }
    fn tab_place(&self, n: u16) -> Option<eitri_core::tab_set::TabPlace> {
        self.inner.tab_place(n)
    }
    fn plugin_keys_help(&self, module: &ModuleId) -> Option<eitri_core::plugin::keys::PluginKeysHelp> {
        self.inner
            .plugin_panels
            .declared_rows(module)
            .map(|(title, rows)| eitri_core::plugin::keys::plugin_help(&title, rows))
    }
    fn chat_views(&self) -> Vec<ViewId> {
        self.inner.agent.views()
    }
    fn can_break_out(&self) -> Result<(), String> {
        self.inner.agent.can_break_out()
    }
    fn chat_move(&self, view: ViewId, session: Option<&str>) -> Result<eitri_core::wm::ChatMoveFacts, String> {
        self.inner.agent.chat_move_facts(view, session)
    }
    fn running_chats(&self) -> Vec<(ModuleId, Option<eitri_core::tabs::TabId>)> {
        self.inner.agent.running_chats()
    }
    fn terminal_job_modules(&self) -> Vec<ModuleId> {
        self.inner.terminal_job_modules()
    }
    fn pages_with_tasks(&self) -> Vec<ModuleId> {
        self.inner.pages_with_tasks()
    }
    fn base_tabs(&self) -> Vec<eitri_core::tabs::TabId> {
        self.inner.agent.base_tabs()
    }
    fn active_base_tab(&self) -> Option<eitri_core::tabs::TabId> {
        self.inner.agent.active_base_tab()
    }
    fn arrived_tab(&self, mv: eitri_core::window_link::moves::MoveId) -> Option<eitri_core::tabs::TabId> {
        self.inner.agent.arrived_tab(mv.0)
    }
    fn tab_move(&self, tab: eitri_core::tabs::TabId) -> Result<eitri_core::wm::ChatMoveFacts, String> {
        self.inner.agent.tab_move_facts(tab)
    }
    fn tab_hand_over(&self, tab: eitri_core::tabs::TabId) -> Result<eitri_core::wm::TabHandOver, String> {
        self.inner.agent.tab_hand_over_facts(tab)
    }
    fn moved_tab_key(&self, tab: eitri_core::tabs::TabId) -> Option<eitri_core::window_link::base_tabs::MovedTabKey> {
        self.inner.agent.moved_tab_key(tab)
    }
    fn order_base_tabs(
        &self,
        tabs: &[eitri_core::tabs::TabId],
        active: Option<eitri_core::tabs::TabId>,
    ) -> Result<(), String> {
        self.inner.agent.order_base_tabs(tabs, active)
    }
    fn view_of_session(&self, session: &str) -> Option<ViewId> {
        self.inner.agent.view_of_session(session)
    }
    fn terminal_jobs(&self) -> usize {
        self.inner.terminal_jobs()
    }
    fn cleanup(&self, op: &eitri_core::wm::CleanupOp) -> Result<(), String> {
        if matches!(op, eitri_core::wm::CleanupOp::PairOnClose { .. }) {
            if let Some((session, app)) = &self.cm_session {
                session.borrow_mut().finish(app);
            }
        }
        self.inner.cleanup(op)
    }
    fn pair(&self, op: &eitri_core::wm::PairOp) -> Result<(), String> {
        self.inner.pair(op)
    }
    fn editor(&self, op: &eitri_core::wm::EditorCall) -> Result<(), String> {
        self.inner.editor(op)
    }
    fn open_answer(&self, id: u64, accepted: bool) -> Result<(), String> {
        self.inner.open_answer(id, accepted)
    }
    fn restart_editor(&self, spawn: &eitri_core::wm::EditorSpawn) -> Result<(), gtk_host::RestartRefused> {
        self.inner.restart_editor(spawn)
    }
    fn terminal(&self, op: &eitri_core::wm::TermOp) -> Result<(), String> {
        self.inner.terminal(op)
    }
    fn plugin(&self, op: &eitri_core::wm::PluginOp) -> Result<(), String> {
        self.inner.plugin(op)
    }
    fn chat(&self, op: &eitri_core::wm::ChatOp) -> Result<(), String> {
        use eitri_core::wm::ChatOp;
        let agent = &self.inner.agent;
        match op {
            // A page closes its keys sheet, its `:` line and a tab's details whenever it hears where the keys
            // are, and when the batch moved the keys into this chat, that news is the host's own focus report,
            // which reaches the page only once the batch has been carried out. Sent now, the sheet, the line or
            // the details would be shut by it at once; sent from an idle, they land after the page knows it has
            // the keys.
            ChatOp::OpenKeymap {
                view, plugin, opening, ..
            } => {
                let (agent, view, plugin, opening) = (agent.clone(), *view, plugin.clone(), *opening);
                gtk4::glib::idle_add_local_once(move || agent.open_keymap(view, plugin, opening));
            }
            ChatOp::CommandLine { view, .. } => {
                let (agent, view) = (agent.clone(), *view);
                gtk4::glib::idle_add_local_once(move || agent.open_command_line(view));
            }
            // The tab's details popover is shut by the same news, so it opens from an idle too; every other tab
            // call is carried out at once.
            ChatOp::Tab {
                call: eitri_core::wm::verbs::tabs::TabCall::Info(view),
                ..
            } => {
                let (agent, view) = (agent.clone(), *view);
                gtk4::glib::idle_add_local_once(move || agent.open_detail(view));
            }
            ChatOp::Reload { view, .. } => agent.reload_document_by_hand(*view),
            ChatOp::JoinBack { view, .. } => match agent.join_back(*view) {
                Ok(_) => {
                    agent.arrive(ViewId::BASE);
                    println!("[chat] {}: its tab joined the base view", view.module());
                }
                Err(text) => return Err(text),
            },
            _ => return self.inner.chat(op),
        }
        Ok(())
    }
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
    fn facts(&self) -> eitri_core::wm::EditorFacts {
        kill_pane::editor_facts(
            self.is_running(),
            self.nvim_exited(),
            self.nvim_exit_pending(),
            self.is_starting(),
            self.watched_call().as_ref(),
        )
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_start_does_not_keep_the_compositor_record_to_rewrite() {
        use eitri_core::window_link::cm_file::CmFile;
        let file = CmFile::new(&eitri_core::layout::hub_tree().layout, &[]);
        assert_eq!(cm_record_kept(Some(file.clone()), false), Some(file.clone()));
        assert_eq!(cm_record_kept(Some(file), true), None);
    }

    /// The last tab closed by hand reaches the controller as a tab having left the window; a hand-over's answers go
    /// over the link instead.
    #[test]
    fn a_closed_last_tab_reaches_the_controller_as_a_tab_that_left() {
        let event = chat_move_event(WinId::MAIN, agent_panel::ChatMoveEvent::LastTabClosed);
        assert!(matches!(event, Some(ChatEvent::TabLeft { win: WinId::MAIN })));
    }

    /// Only a run that follows compositor mode gives saved views tiles: a layout file `eitri` mode wrote says what it
    /// wants, and a window with `window.restore` off asked for its default layout.
    #[test]
    fn only_a_way_back_from_compositor_mode_tiles_the_saved_views() {
        use eitri_core::source_scan as scan;
        let flat = |text: &str| text.split_whitespace().collect::<String>();
        let (_, code) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
        let code = flat(&code);
        let guarded = code
            .find(concat!("letsaved_views=(back_from_", "compositor"))
            .expect("the views are read only then");
        let tiled = code
            .find(concat!("tile_saved_", "views(&mutstart_layout"))
            .expect("the start layout gets them");
        let views = code
            .find(concat!("letstartup_chat_", "views"))
            .expect("the window builds its chat views from that layout");
        assert!(guarded < tiled && tiled < views);
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

    /// Checks that `build_application` sets `NON_UNIQUE`, which disables gio's
    /// single-instance behavior. Removing the flag fails this assertion;
    /// its runtime effect appears when a second window is launched.
    /// This test does not launch a second `shell` or exercise gio registration, and
    /// would still pass if another route ran `build_ui` twice in one process.
    /// Separate processes on separate launches follow gio's semantics; demonstrating
    /// that behavior requires launching two real `shell` processes.
    #[test]
    fn build_application_still_opts_out_of_gios_single_instance_behaviour() {
        for kind in [AppKind::Main, AppKind::Chat, AppKind::Page] {
            let app = build_application(kind);
            assert!(
                app.flags().contains(gtk4::gio::ApplicationFlags::NON_UNIQUE),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn each_kind_of_window_has_its_app_id() {
        assert_eq!(AppKind::Main.id(), "cn.huntergrey.eitri");
        assert_eq!(AppKind::Chat.id(), "cn.huntergrey.eitri.Panel");
        assert_eq!(AppKind::Page.id(), "cn.huntergrey.eitri.Page");
        let module = |name: WindowName| linked_window::WindowStart::Module {
            name,
            root: PathBuf::from("/p"),
            sock: std::os::fd::OwnedFd::from(std::fs::File::open("/dev/null").unwrap()),
            token: None,
            restore: false,
        };
        assert_eq!(AppKind::of(&module(WindowName::Chat(1))), AppKind::Chat);
        assert_eq!(AppKind::of(&module(WindowName::Chat(4))), AppKind::Chat);
        assert_eq!(AppKind::of(&module(WindowName::Page("markdown".into()))), AppKind::Page);
        assert_eq!(AppKind::of(&module(WindowName::Second)), AppKind::Main);
        assert_eq!(AppKind::of(&linked_window::WindowStart::Full), AppKind::Main);
    }

    /// A chat or page window shows one module: `build_ui` makes no editor for it, reads and writes no layout file,
    /// hangs no top bar in the window and builds no other module's host, and a chat window shows its chat in its
    /// own base view.
    #[test]
    fn the_module_window_branch_builds_only_its_module() {
        use eitri_core::source_scan as scan;
        let flat = |text: &str| text.split_whitespace().collect::<String>();
        let (_, code) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
        for needle in [
            concat!("module_window.is_none() {\n        root.append(&top_bar", ".widget)"),
            concat!("made.retain(|decl| decl.module == ", "*module)"),
            concat!("reconcile_", "module("),
            concat!("Tiling::", "ModuleWindow"),
            concat!("set_page_", "roots("),
        ] {
            assert!(flat(&code).contains(&flat(needle)), "build_ui lacks {needle}");
        }
        assert!(
            !code.contains(concat!("set_transient", "_for")),
            "a module window is not a dialog of window 1"
        );
        let (_, modes) = scan::function_in(&[scan::read_required("shell/src/window_mode.rs")], "sync_widgets");
        assert!(
            flat(&modes).contains(&flat(concat!("self.top_bar.parent()", ".is_some() &&"))),
            "a top bar with no parent is never shown again by a window mode or a reveal"
        );
    }

    /// A `bye` to a module window is the group closing: the window closes instead of waiting for a window 1.
    #[test]
    fn a_module_window_that_hears_its_group_close_closes_rather_than_waits() {
        use eitri_core::source_scan as scan;
        let flat = |text: &str| text.split_whitespace().collect::<String>();
        let (_, code) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
        let code = flat(&code);
        let asked = code
            .find(concat!("waits_for_relink_", "after(end)"))
            .expect("the hook asks how the link ended");
        let closes = code[asked..]
            .find(concat!("HostEvent::", "GroupClosed"))
            .expect("a group's end closes the window");
        let waits = code[asked..]
            .find(concat!("pair.wait_for_", "relink("))
            .expect("any other end waits");
        assert!(closes < waits, "the close is the branch that does not wait");
    }

    /// A chat window saves nothing itself: its panel's writer is off from the moment the mode is known, each change of
    /// its tabs goes to window 1, and a link that comes up hears the latest.
    #[test]
    fn a_module_window_tells_window_1_its_tabs_and_keeps_no_file() {
        use eitri_core::source_scan as scan;
        let flat = |text: &str| text.split_whitespace().collect::<String>();
        let (_, code) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
        let code = flat(&code);
        assert_eq!(
            code.matches(&flat(concat!("agent_panel_handle.keep_no", "_tabs();")))
                .count(),
            2
        );
        for wired in [
            concat!("agent_panel_handle.report_", "tabs(move |tabs|"),
            concat!("pair.tabs_", "changed(tabs.clone());"),
            concat!("pair.say_", "tabs();"),
            concat!("linked_window::group_tabs::beside_the_", "layout(open, tab_windows)"),
        ] {
            assert_eq!(code.matches(&flat(wired)).count(), 1, "{wired}");
        }
    }

    /// What a chat or page window needs of `build_ui` besides its module: its name for the controller, the keys left
    /// in window 1 when it was started restored, no saved tabs of window 2's, the focus it gained before it linked,
    /// and window 1's way to hear that a window it started never linked.
    #[test]
    fn the_module_window_branch_names_itself_and_wires_its_start_and_focus() {
        use eitri_core::source_scan as scan;
        let flat = |text: &str| text.split_whitespace().collect::<String>();
        let (_, code) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
        for needle in [
            concat!("driver.set_module_", "name(WinId::MAIN, name.clone())"),
            concat!("pair.restored() && module_window.", "is_none(),"),
            concat!("PlacementRole::", "NewModule"),
            concat!("pair.restored() || module_window.", "is_some()"),
            concat!("window_weak.upgrade().is_some_and(|window| window.", "is_active())"),
            concat!("pair.say_window_", "focused()"),
            concat!("pair.on_module_", "failed("),
            concat!("HostEvent::ModuleWindow", "Failed {"),
        ] {
            assert!(flat(&code).contains(&flat(needle)), "build_ui lacks {needle}");
        }
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

    /// Every accelerator is registered from `eitri_core::keymap::root`, so an
    /// accelerator-shaped literal anywhere in `shell/src` is one registered around the keymap --
    /// and one the no-`Ctrl+Shift` check in core never saw. `is_accel` is exercised directly, so
    /// this cannot pass because the scanner went blind.
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
        assert_eq!(calls, 0, "the shell's wiring never enters INPUT");
        let services = eitri_core::source_scan::code_only(
            &eitri_core::source_scan::read_required("shell/src/gtk_host/services.rs").text,
        );
        let tab_calls = eitri_core::source_scan::functions(&services, "tab_call");
        assert_eq!(tab_calls.len(), 1);
        assert_eq!(tab_calls[0].matches(".enter_input(").count(), 1);
        assert!(tab_calls[0].contains("agent.enter_input(ViewId::BASE)"));
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
            at(concat!("build_", "application(kind)")),
        ];
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
    }

    /// What a launch hands a file over with is gone from the environment after the scrub and after GTK, and a launch
    /// that hands its file over must end before anything of a window exists.
    #[test]
    fn main_reads_the_open_socket_before_the_scrub_and_delivers_before_gtk() {
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
            at(concat!("open_requests::hint_", "from_env()")),
            at(concat!("open_requests::token_", "from_env()")),
            at(concat!("companion::env::drop_inherited", "_editor_env()")),
            at(concat!("resolve_", "launch()")),
            at(concat!("open_requests::hand_", "over(")),
            at(concat!("nvim_bin::", "resolve()")),
            at(concat!("build_", "application(kind)")),
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
            at(concat!("project_root::", "resolve_launch()")),
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
        assert!(body.find(jit) < body.find(concat!("build_", "application(kind)")));
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

    /// A shell source as code: comments and literals blanked, its test modules cut.
    fn shell_file_code(path: &str) -> String {
        use eitri_core::source_scan as scan;
        let code = scan::code_only(&scan::read_required(path).text);
        scan::without(&code, &scan::modules(&code, "tests"))
    }

    /// The body of the closure handed to the first `call` (ending in its `(`) in `code`.
    fn closure_of<'a>(code: &'a str, call: &str) -> &'a str {
        let at = code.find(call).unwrap_or_else(|| panic!("`{call}`"));
        eitri_core::source_scan::braced(code, at + code[at..].find('{').expect("a closure"))
    }

    /// A termination signal's handler only marks the close and says so, through the write that never blocks, then
    /// posts the signal: the controller tears the window down, asking nothing. It closes nothing itself, and is
    /// installed once the teardown has what it ends, so a signal at any moment after finds it there. A failed write
    /// panics in `println!`, and a panic in this GLib callback aborts the process.
    #[test]
    fn the_end_signal_posts_and_closes_nothing_itself() {
        let (_, ui) =
            eitri_core::source_scan::function_in(&eitri_core::source_scan::rust_sources(&["shell/src"]), "build_ui");
        assert_eq!(ui.matches(concat!("end_signals::", "install(")).count(), 1);
        let install = ui.find(concat!("end_signals::", "install(")).unwrap();
        let handed = ui
            .find(concat!("*teardown.", "borrow_mut() = Some("))
            .expect("the teardown is handed what it ends");
        assert!(handed < install, "the teardown has what it ends before any signal");
        let handler = closure_of(&ui[install..], "install(");
        let post = handler
            .find(concat!("HostEvent::", "EndSignal { name }"))
            .expect("the handler posts the signal");
        assert!(
            !handler.contains(concat!("close", "(")),
            "the handler closes nothing itself"
        );
        let before = &handler[..post];
        assert!(
            !before.contains(concat!("println", "!(")),
            "the handler's own lines may not panic"
        );
        assert!(
            !before.contains(concat!("writeln", "!(")),
            "the handler's own lines may not block on a full pipe"
        );
        assert_eq!(
            before.matches(concat!("end_signals::", "write_if_writable(")).count(),
            2,
            "both lines go through the write that never blocks"
        );
        let marked = before
            .find(concat!("end_signals::", "mark_closing()"))
            .expect("the close is marked for the watchdog");
        let said = before.rfind(concat!("end_signals::", "write_if_writable(")).unwrap();
        assert!(marked < said, "marked before it is said");
        assert_eq!(
            end_signals::SIGNALS.map(|(signum, _)| signum),
            [libc::SIGTERM, libc::SIGINT, libc::SIGHUP]
        );
    }

    /// The window's close request decides nothing: a close the host's own `CloseWindow` started proceeds, and every
    /// other one is the controller's to answer, which stops it and tears the window down later if it may go.
    /// `build_ui` adds no close handler of its own.
    #[test]
    fn the_close_handler_decides_nothing() {
        let (_, ui) =
            eitri_core::source_scan::function_in(&eitri_core::source_scan::rust_sources(&["shell/src"]), "build_ui");
        assert!(
            !ui.contains(concat!("connect_", "close_request(")),
            "build_ui handles the close itself"
        );
        let host = shell_file_code("shell/src/gtk_host/mod.rs");
        assert_eq!(host.matches(concat!("connect_", "close_request(")).count(), 1);
        let handler: String = closure_of(&host, concat!("connect_", "close_request("))
            .split_whitespace()
            .collect();
        let flag = handler
            .find("host.closing.borrow().contains(&win)")
            .expect("the host's own close goes through");
        let asked = handler
            .find("sink.close_requested(win,CloseBy::User)")
            .expect("every other close is the controller's");
        assert!(flag < asked, "the host's own close never asks");
        for decision in [
            "shutdown(",
            "persist",
            "cleanup(",
            "remove_dir",
            ".close()",
            "Editor",
            "quit",
            "prompt",
        ] {
            assert!(!handler.contains(decision), "the close handler does `{decision}`");
        }
    }

    /// The editor's quit and the window's close are the controller's (`eitri_core::wm::{quit, close}`): nothing of
    /// the flow the shell carried out is left in it, and `build_ui` closes no window itself.
    #[test]
    fn no_quit_flow_is_left_in_the_shell() {
        use eitri_core::source_scan as scan;
        for source in scan::rust_sources(&["shell/src"]) {
            let code = scan::code_only(&source.text);
            let code = scan::without(&code, &scan::modules(&code, "tests"));
            for name in [
                concat!("Editor", "Quitting"),
                concat!("Quit", "Host"),
                concat!("Shell", "QuitHost"),
                concat!("persist", "_now("),
                concat!("Prompt", "Withdrawn"),
            ] {
                assert!(!code.contains(name), "{} still has `{name}`", source.path);
            }
        }
        let (_, ui) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
        assert!(
            !ui.contains(concat!("window.", "close()")),
            "build_ui closes the window itself"
        );
        assert!(
            !ui.contains(concat!("retire", "_editor")),
            "build_ui retires the editor itself"
        );
    }

    /// The window's service set wraps `gtk_host_services::Services`: a service the wrapper leaves to the trait's
    /// default is answered "not carried out" whatever the inner set does, so every one the trait declares is written
    /// out in the wrapper.
    #[test]
    fn the_windows_service_set_forwards_every_service() {
        use eitri_core::source_scan as scan;
        let names = |code: &str| -> Vec<String> {
            scan::word_ends(code, "fn")
                .into_iter()
                .filter_map(|end| {
                    let name: String = code[end..]
                        .trim_start()
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    (!name.is_empty()).then_some(name)
                })
                .collect()
        };
        let block = |path: &str, head: &str| -> String {
            let code = scan::code_only(&scan::read_required(path).text);
            let code = code.split_whitespace().collect::<Vec<_>>().join(" ");
            let at = code.find(head).unwrap_or_else(|| panic!("{path} has no `{head}`"));
            let open = at + code[at..].find('{').expect("a body after it");
            scan::braced(&code, open).to_string()
        };
        let declared = names(&block("shell/src/gtk_host/mod.rs", "trait ServiceSet"));
        let written = names(&block("shell/src/main.rs", "ServiceSet for PanelServices"));
        assert!(
            declared.iter().any(|name| name == "restart_editor") && declared.len() > 5,
            "the trait was not read: {declared:?}"
        );
        let missing: Vec<&String> = declared.iter().filter(|name| !written.contains(name)).collect();
        assert!(
            missing.is_empty(),
            "PanelServices leaves {missing:?} to the trait's default"
        );
    }

    /// A terminal's shell ending is told to the controller, which judges it (the terminal's kill, or the window's
    /// close as the last module on screen): the hook kills and closes nothing itself, and answers `false` on every
    /// path, since the kill's own hang-up closes the pane.
    #[test]
    fn the_terminal_exit_hook_posts_and_answers_false() {
        let (_, ui) =
            eitri_core::source_scan::function_in(&eitri_core::source_scan::rust_sources(&["shell/src"]), "build_ui");
        assert_eq!(ui.matches(concat!("pane.on_", "shell_exit(")).count(), 1);
        let hook: String = closure_of(&ui, concat!("pane.on_", "shell_exit("))
            .split_whitespace()
            .collect();
        assert!(
            hook.contains("HostEvent::Terminal(TermEvent::Exited{"),
            "the end is posted"
        );
        for call in ["close(", "kill_confirmed(", "can_kill("] {
            assert!(!hook.contains(call), "the hook does `{call}`");
        }
        assert!(!hook.contains("true"), "the hook never asks its pane to close");
        let returns = hook.matches("return").count();
        assert_eq!(
            hook.matches("returnfalse;").count(),
            returns,
            "every early return answers false"
        );
        assert!(hook.ends_with("false}"), "and so does its end");
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

    /// Every refusal is the controller's, drawn from its chrome model as a flash and a toast: `build_ui` keeps no way
    /// of refusing a key itself, so no refusal there can blink without its words or differ from the controller's.
    #[test]
    fn build_ui_refuses_nothing_itself() {
        use eitri_core::source_scan as scan;
        let (_, code) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
        assert!(
            !code.contains(concat!("Shell", "Refused")),
            "build_ui posts a refusal of its own"
        );
        let flashes = code.matches(concat!("flash", "(")).count();
        let drawn = code.matches(concat!("show_flash", "(")).count();
        assert_eq!(flashes, drawn, "the only flash in build_ui is the chrome model's own");
    }
}
