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
mod hint;
mod kill_pane;
mod layout;
mod layout_state;
mod lua;
mod module_grid;
mod pane_focus;
mod pane_switch;
mod panel_super;
mod plugin_page;
mod plugin_panels;
mod prefix;
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
use chat_views::ChatViews;
use eitri_core::attention::AgentPlace;
use eitri_core::keymap::action::{FloatRule, SelectionRule, SplitCwd};
use eitri_core::keymap::keys_return::{AtClose, KeysReturn, Opened as KeysOpened};
use eitri_core::keymap::prefix::WorkspacePick;
use eitri_core::keymap::{Action, SwapTarget, TabAction};
use eitri_core::layout::rules::{without_keys, At, OpenPoint, RuleSize};
use eitri_core::layout::workspace::MAIN;
use eitri_core::layout::{
    declared_placement, Axis, ConLayout, Direction, KeyAction, KillScope, Layout, LayoutError, ModuleDecl, ModuleId,
    ModuleKeys, ModuleKind, Nav, OpenRule, OpenRules, Opened, Placement, Preset, Reopen, ScratchShow, Selection,
    WorkspaceError, Workspaces,
};
use eitri_core::plugin::theme::PageTheme;
use eitri_core::tabs::TabVerb;
use eitri_panel::panel_page::PanelPage;
use lua::{LuaEngine, PanelSlot};
use module_grid::{HostKind, ModuleGrid};
use neovide_editor::{NeovideEditorPane, NeovideEditorPaneOptions};
use terminal::registry::Terminals;
use web_host::WebHost;

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

fn main() -> glib::ExitCode {
    // Collected once, before anything else, and consulted through `flag_given` -- never a raw
    // `args_os().any(|a| a == "...")` -- by every early flag scan below (`--version`, `--legacy`,
    // `--clean`). `flag_given` stops at the first `--`, the same end-of-flags rule
    // `eitri_core::project_root::select_root_source` applies to the project-directory argument
    // itself; before this, each scan ran its own ad-hoc `.any()` with no notion of `--` at all, so
    // `shell -- --version` printed the version line instead of opening a directory literally named
    // `--version`, and `shell -- --legacy` was refused on a release build instead of reaching the
    // directory step. `args_os`, not `args`, and skipping argv[0] (never a
    // flag), for the same non-UTF-8-safety reason `project_root` gives for its own `args_os` use.
    let early_args: Vec<OsString> = std::env::args_os().skip(1).collect();
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
    let project_root = match eitri_core::project_root::resolve() {
        Ok(root) => root,
        Err(message) => {
            eprintln!("eitri: {message}");
            return glib::ExitCode::FAILURE;
        }
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
    match eitri_core::nvim_bin::resolve() {
        Ok(choice) => {
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

    let app = build_application();
    app.connect_activate(move |app| build_ui(app, want_clean, &project_root, backend_kind, panel_notice.as_deref()));
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
) {
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
    let mut theme_feed = theme::feed::ThemeFeed::new();

    // Built before the editor pane, because its `child_env()` has to be handed to the pane's
    // constructor -- those variables reach nvim through `Command::env` at spawn time and cannot
    // be added afterwards. `None` means the feature is simply unavailable (see `pane_switch`'s
    // own doc); the editor is constructed with an empty child env and behaves exactly as before.
    let mut pane_switch = pane_switch::open();
    // wire 1's feed, built here for the same reason the other two are: its `child_env()` and its
    // `--cmd` must be handed to the editor pane's constructor, and neither can be added to a child
    // that is already running.
    let mut context_feed = eitri_core::editor_context::feed::EditorContextFeed::new();
    // Build the feed for nvim's `mapleader`, `timeoutlen`, and Normal-mode maps here.
    // Its `child_env()` and `--cmd` must reach the editor pane's constructor;
    // neither can be added to an already running child.
    let mut nvim_keys_feed = eitri_core::nvim_keys::feed::NvimKeysFeed::new();
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
    let scratch_dir = eitri_core::scratch::ScratchDir::new();
    nvim_child_env.extend(scratch_dir.as_ref().map(|dir| dir.child_env()).unwrap_or_default());
    nvim_extra_args.extend(scratch_dir.as_ref().map(|dir| dir.nvim_args()).unwrap_or_default());
    let scratch_path = scratch_dir.as_ref().map(|dir| dir.path().to_path_buf());

    // `Rc` because two separate closures need it after this function returns: the agent panel's
    // Ctrl+h handler (to hand focus back) and the window's close handler (to shut nvim down).
    // `NeovideEditorPane` is deliberately not `Clone`.
    //
    // `cwd` is set from the same resolved root the agent panel gets below, not left to the nvim
    // child's inherited process cwd: the two panes silently disagreeing about which project is
    // open is a worse failure than either one pointing somewhere unexpected.
    let pane = Rc::new(NeovideEditorPane::with_options(NeovideEditorPaneOptions {
        clean: want_clean,
        child_env: nvim_child_env,
        cwd: Some(project_root.to_path_buf()),
        extra_nvim_args: nvim_extra_args,
    }));

    let config_dir = config_dir();
    let lua_engine = Rc::new(LuaEngine::new(config_dir.clone()).expect("LuaEngine construction must not fail"));

    // A feed that failed to start yields a source that always answers `None`, so the panel needs no
    // branch: turns simply go out as the user typed them, exactly as before wire 1 existed.
    let editor_context_source = match context_feed.as_mut() {
        Some(feed) => feed_pump.listen_context(feed, scratch_path.clone()).0,
        None => std::rc::Rc::new(|| None),
    };
    let (agent_widget, agent_panel_handle) = agent_panel::build_agent_panel(
        project_root.to_path_buf(),
        editor_context_source,
        scratch_dir,
        backend_kind,
        panel_notice,
        None,
    );
    // The panel reads the context when its tick runs, and its tick runs only when woken.
    crate::editor_feeds::wake_panel_on_context(&feed_pump, &agent_panel_handle);

    lua_engine.load_init_file(&config_dir.join("init.lua"));

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
    let terminals: Rc<RefCell<Terminals<terminal::TerminalPane>>> = Rc::new(RefCell::new(Terminals::new()));
    terminals
        .borrow_mut()
        .take_or_make(&ModuleId::terminal(), || make_terminal_pane());

    // The initial layout has `[editor | agent]` with the terminal hidden below both.
    // Lua panels are placed by `position` in registration order (`terminal::initial_layout`,
    // `Layout::initial`, `PanelSlot::placement`). Every window has editor, agent, and
    // terminal modules outside the Lua registry, so Lua panels do not replace them.
    // A Lua `bottom` panel goes below the terminal.
    let lua_panels: Vec<(ModuleId, PanelSlot, gtk4::Widget)> = lua_engine
        .panels
        .borrow()
        .entries()
        .iter()
        .map(|entry| (ModuleId::lua(&entry.id), entry.slot, entry.widget.clone()))
        .collect();
    // Keep each Lua panel's page for `prefix x`: killing ends its web process,
    // and its module key loads this page again (`kill_pane`). The fourth element is
    // its crash-loop guard. Revival resets the guard, re-arming automatic recovery
    // just as `AgentPanelHandle::reload_document_by_hand` does for the chat panel.
    let lua_pages: Rc<
        Vec<(
            ModuleId,
            gtk4::Widget,
            String,
            Rc<RefCell<webview_crash_guard::WebViewCrashGuard>>,
        )>,
    > = Rc::new(
        lua_engine
            .panels
            .borrow()
            .entries()
            .iter()
            .map(|entry| {
                (
                    ModuleId::lua(&entry.id),
                    entry.widget.clone(),
                    entry.url.clone(),
                    entry.crash_guard.clone(),
                )
            })
            .collect(),
    );
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
    if let Some(feed) = nvim_keys_feed.as_mut() {
        let keys_help = keys_help.clone();
        let plugin_panels = plugin_panels.clone();
        feed_pump.listen_keys(feed, move |report| {
            // A page's multi-key sequences wait as long as the editor's do.
            plugin_panels.set_timeoutlen(report.timeoutlen);
            keys_help.nvim_report(report);
            // The feed reports at nvim's VimEnter: a new editor session may be ready for the plugins' snippets.
            plugin_panels.editor_session();
        });
    }
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
    let state_dir = eitri_core::layout::persist::state_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    let loaded = state_dir
        .as_deref()
        .map(|dir| eitri_core::layout::persist::load_with(dir, project_root, &decls, &deferred));
    // The key that shows the editor, as the keymap binds it: named by the note a layout that hides
    // the editor prints, the way the toast names its own way back.
    let show_editor: Option<String> = keymap
        .keys_for(&Action::Module(ModuleId::editor()))
        .first()
        .map(|key| format!("{} {}", keymap.prefix().human(), key.human()));
    // `module_layout` is the current workspace's layout, the one every reader of the live layout holds;
    // `workspaces` holds the window's other workspaces beside it, and what a later build keeps in the
    // file.
    let (workspaces, module_layout, keep_copy) = match layout_state::choose_startup_layout(
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
    match layout_state::add_defined_workspaces(
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
    let layout_saver = layout_state::LayoutSaver::new(
        state_dir,
        project_root,
        module_layout.clone(),
        workspaces.clone(),
        decls.clone(),
        keep_copy,
    );
    layout_saver.set_deferred(deferred.clone());
    // Every module's host, added once and never reparented (`module_grid`'s module doc).
    let grid = ModuleGrid::new(module_layout.clone());
    // Wrap the editor in an overlay with a hidden failure-message label before adding it to the
    // grid, so startup errors can appear in the editor's area without reparenting its widget.
    let (editor_host, editor_start_failure_label) = editor_start_failure::install(pane.widget());
    grid.add(ModuleId::editor(), &editor_host, pane.widget(), HostKind::Direct);
    // With no `WebView` in this process (`webkit_sandbox`), the chat's and each Lua panel's place is
    // a plain GTK notice: allocated directly, as the editor is, rather than held at a settled size
    // like a `WebView` being resized.
    let web_host = if webkit_sandbox::decision().allows_webviews() {
        HostKind::Web
    } else {
        HostKind::Direct
    };
    // Every web module -- the chat and each Lua panel -- sits at (0,0) of a `WebHost` of its own
    // rather than in the grid directly, and takes the keys through its `WebView` (its focus target):
    // WebKitGTK adds the `WebView`'s position in its parent to the input method's caret rectangle,
    // which GTK4's input methods then translate to the window a second time, so a `WebView` at the
    // module's own x put the candidate window that far right of the caret (`web_host`'s module doc).
    // The notice that stands in for a `WebView` where WebKit cannot start is wrapped the same way, so
    // the shape does not depend on `webkit_sandbox`.
    grid.add(ModuleId::agent(), &WebHost::new(&agent_widget), &agent_widget, web_host);
    // Every terminal in the layout the window opens with gets its pane and host now, the base one
    // first and the numbered ones in number order, so the hosts exist before the first `apply`. None
    // starts a shell here: each starts when it is first shown (the change hook below).
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
    for id in startup_terminals {
        let (pane, _) = terminals.borrow_mut().take_or_make(&id, || make_terminal_pane());
        grid.add(id, pane.widget(), pane.widget(), HostKind::Direct);
    }
    // Every extra chat view's page, keyed by its module id: a view that leaves the layout keeps its page
    // here, idle, for the next view of that number only (`chat_views`'s module doc). The base view's page
    // is the `related` one of each: the pages share its web process (and so its memory).
    let chat_views: Rc<RefCell<ChatViews<(webkit6::WebView, Rc<PanelPage>)>>> = Rc::new(RefCell::new(ChatViews::new()));
    let base_webview: Option<webkit6::WebView> = agent_widget.downcast_ref::<webkit6::WebView>().cloned();
    // Every chat view the layout the window opens with holds, in any workspace, gets its page and host
    // now, as the numbered terminals do, so the hosts exist before the first `apply`. The panel is told
    // which views there are before the base page's `ready` can start a restore, so a restored tab saved
    // in one of them comes back into it. Where WebKit cannot start, each view's place is the notice the
    // base view shows.
    let startup_chat_views: Vec<ModuleId> = {
        let layout = module_layout.borrow();
        let workspaces = workspaces.borrow();
        let layouts: Vec<&eitri_core::layout::Layout> =
            workspaces.each(&layout).into_iter().map(|(_, layout)| layout).collect();
        extra_chat_views(&layouts)
    };
    for id in &startup_chat_views {
        let view = ViewId::of_module(id).expect("a chat module");
        match &base_webview {
            Some(base) => {
                let ((webview, _), _) = chat_views.borrow_mut().take_or_make(id, || {
                    agent_panel::build_chat_page(&agent_panel_handle, view, Some(base))
                });
                grid.add(id.clone(), &WebHost::new(&webview), &webview, web_host);
            }
            None => {
                let notice = webkit_sandbox::notice_widget(panel_notice.unwrap_or(CHAT_VIEWS_NEED_WEBKIT));
                grid.add(id.clone(), &WebHost::new(&notice), &notice, web_host);
            }
        }
        println!("[chat] {id}: its page and host made at startup");
    }
    agent_panel_handle.set_layout_views(startup_chat_views.iter().filter_map(ViewId::of_module).collect());
    for (id, slot, widget) in &lua_panels {
        // On the `WebView`: its host measures its one child, so the grid sees the same floor.
        if *slot == PanelSlot::Bottom {
            widget.set_size_request(-1, layout::BOTTOM_MIN_HEIGHT);
        }
        grid.add(id.clone(), &WebHost::new(widget), widget, web_host);
    }
    // A plugin that starts hidden costs no web process until its module is first shown. A layout that
    // reopens one shown builds its page now; every later first show, by any route, builds it in the
    // change hook below.
    let shown_at_start: Vec<ModuleId> = {
        let layout = module_layout.borrow();
        deferred.iter().filter(|id| layout.is_shown(id)).cloned().collect()
    };
    for id in &shown_at_start {
        plugin_panels.build_page(id, &plugin_panels.theme());
    }
    // A terminal's shell starts the first time it is shown, however it got there -- `Ctrl+a t`,
    // `Ctrl+a \ t`, a new instance, its tray chip, `eitri.layout.show`, a saved layout -- rather than
    // only on the `Ctrl+a t` path. Every live pane that is shown and has no shell running is started;
    // `start` does nothing for one whose shell has exited and waits for Enter.
    {
        let module_layout = module_layout.clone();
        let terminals = terminals.clone();
        grid.connect_changed(move || {
            let starting: Vec<terminal::TerminalPane> = {
                // Hooks run with the layout released (`ModuleGrid::connect_changed`), and nothing
                // holds the registry across a grid call; these lines are how a caller that breaks
                // either would show up, instead of a terminal that silently never starts.
                let Ok(layout) = module_layout.try_borrow() else {
                    eprintln!("[terminal] BUG: the layout was borrowed when it changed; no shell was started");
                    return;
                };
                let Ok(terminals) = terminals.try_borrow() else {
                    eprintln!(
                        "[terminal] BUG: the registry was borrowed when the layout changed; no shell was started"
                    );
                    return;
                };
                terminals
                    .live()
                    .filter(|(id, pane)| layout.is_shown(id) && pane.shell_pid().is_none())
                    .map(|(_, pane)| pane.clone())
                    .collect()
            };
            for pane in starting {
                pane.start();
            }
        });
    }
    // A plugin that starts hidden gets its page the first time its module is shown, however it got there:
    // its key, `module.<id>`, a split key, a tray chip, a workspace switch, `eitri.layout.show`.
    {
        let module_layout = module_layout.clone();
        let plugin_panels = plugin_panels.clone();
        let deferred = deferred.clone();
        grid.connect_changed(move || {
            // Bound before any build: a build adds a widget, and nothing may hold the layout across it.
            let building: Vec<ModuleId> = {
                let Ok(layout) = module_layout.try_borrow() else {
                    eprintln!("[plugin] BUG: the layout was borrowed when it changed; no page was built");
                    return;
                };
                deferred
                    .iter()
                    .filter(|id| layout.is_shown(id) && !plugin_panels.is_built(id))
                    .cloned()
                    .collect()
            };
            for id in building {
                plugin_panels.build_page(&id, &plugin_panels.theme());
            }
        });
    }
    // Each plugin host hears whether its module is drawn: nvim follows a buffer for a page only while it is. Hooked
    // before the startup `apply`, which runs it once for the layout the window opens with.
    {
        let module_layout = module_layout.clone();
        let plugin_panels = plugin_panels.clone();
        let modules = plugin_panels.modules();
        grid.connect_changed(move || {
            // Read and released before any host is told: nothing may hold the layout across a host call.
            let visible: Vec<ModuleId> = {
                let Ok(layout) = module_layout.try_borrow() else {
                    eprintln!("[plugin] BUG: the layout was borrowed when it changed; no plugin was told it is drawn");
                    return;
                };
                layout.visible_modules()
            };
            for id in &modules {
                plugin_panels.set_on_screen(id, visible.contains(id));
            }
        });
    }
    grid.apply();
    // Every change of the arrangement is written to this project's state file, 500ms after the last
    // one, and once more when the window closes if it is still unwritten; a change of the
    // keys alone never is. Hooked up only now, after the startup `apply`: a launch is not a change
    // (`layout_state`'s module doc).
    {
        let layout_saver = layout_saver.clone();
        grid.connect_changed(move || layout_saver.changed());
    }
    // A pinned row getting its length on its first frame is not a change: the saver takes it as
    // what the window opened with (`layout_state`'s module doc).
    {
        let layout_saver = layout_saver.clone();
        grid.connect_settled(move || layout_saver.settled());
    }

    // Gives a module the keys. The editor goes through `NeovideEditorPane::grab_focus`, which also
    // tells the input method; any other module is its focus target's own `grab_focus`
    // (`ModuleGrid::focus_target`), never its host's: a web module's host is a `WebHost`, which is
    // not focusable itself -- it forwards a grab to its `WebView`, but this does not rely on that
    // (`web_host`'s module doc).
    // A hidden module is refused: GTK4's `grab_focus` does not refuse an unmapped widget, so the keys would
    // go to a module nobody can see (the editor under a Lua `main` panel). Every caller today
    // already avoids one -- `navigate` sees only shown modules, HINT only mapped ones, and the
    // layout's `focus` is never hidden (`Layout::set_focus`) -- so this holds it for the next caller.
    let focus_module: Rc<dyn Fn(&ModuleId) -> bool> = {
        let pane = pane.clone();
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let plugin_panels = plugin_panels.clone();
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
            // A start-hidden plugin's slot holds no view until its page is built, and an empty slot refuses the
            // grab. A workspace switch and an open give the keys before the change hooks that build it run, so
            // the page is built here first, with no layout borrow held.
            if plugin_panels.starts_hidden(id) && !plugin_panels.is_built(id) {
                plugin_panels.build_page(id, &plugin_panels.theme());
            }
            if id.kind() == ModuleKind::Editor {
                pane.grab_focus();
                // `NeovideEditorPane::grab_focus` returns nothing, so ask: did the editor's widget
                // become the window's focus widget? (It used to say `true` unconditionally, so the
                // `grab_focus=` in `[pane_switch]` lines could not be believed for the editor.)
                return pane.widget().is_focus();
            }
            // Read live so modules added after startup are found too.
            grid.focus_target(id).is_some_and(|target| target.grab_focus())
        })
    };
    // Gives a module the keys by any route that chooses one -- a module key, `Ctrl+h/j/k/l`, a digit,
    // a strip entry, HINT, a tray chip, `eitri.layout.focus`. A module on a tab its container does not
    // draw has that tab chosen first (`ModuleGrid::switch_tab`, which maps its host before the grab):
    // a bare grab would put the keys in a host nobody can see, and the layout would then draw a tab
    // the grid never mapped. `focus_module` stays the bare grab, for the hides and kills that hand the
    // keys to a module the layout already draws.
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

    let window = ApplicationWindow::builder()
        .application(app)
        .title(chrome::window_title(&chrome::project_display_name(project_root), None))
        .default_width(1280)
        .default_height(760)
        // No HeaderBar: decorated(false) suppresses GTK's own CSD titlebar entirely -- the
        // custom top bar built below (chrome::build_top_bar) is the only titlebar.
        .decorated(false)
        .build();
    window.add_css_class("shell-root");

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
    // Filled in once `text_size_controller` exists (below): the theme listener, registered before it,
    // hands it the editor's new font size through this.
    let text_size_slot: Rc<std::cell::RefCell<Option<Rc<text_size::TextSizeController>>>> = Rc::default();
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
    pane.set_clear_color(editor_clear(&eitri_core::theme::ThemeTokens::fallback()));
    // Each terminal pane took the fallback colours when created by `make_terminal_pane`. When the
    // editor reports or changes its cell height, save it for `panel_tokens` so later colorscheme
    // changes retain it. Push the height into the panel's live theme through the same `set_theme`
    // path used for font-size zoom.
    {
        let editor_row = editor_row.clone();
        let agent_panel_handle = agent_panel_handle.clone();
        pane.connect_cell_size_changed(move |_, h| {
            let mut row = editor_row.get();
            let css_px = row.set_cell_height(h as f32);
            editor_row.set(row);
            agent_panel_handle.set_editor_row_px(css_px);
        });
    }
    // A key the user pressed in the editor reached nvim: the typing window that paces the agent
    // panel's stream (`AgentPanelHandle::note_editor_key`). A notification only.
    {
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
    if let Some(feed) = theme_feed.as_mut() {
        let theme_css = theme_css.clone();
        // What the stylesheet paints, for the widgets GTK leaves on the old one (`theme::restyle`).
        let window_for_theme = window.clone();
        let agent_panel_handle = agent_panel_handle.clone();
        let pane_for_theme = pane.clone();
        let terminals = terminals.clone();
        let terminal_colors = terminal_colors.clone();
        let plugin_panels = plugin_panels.clone();
        let editor_font = editor_font.clone();
        let text_size_slot = text_size_slot.clone();
        let follows_editor = config.panel_font_follows_editor;
        feed_pump.listen_theme(feed, move |payload| {
            // The editor's font first, so the theme sent below already carries the panel's new size.
            if follows_editor {
                let mut font = editor_font.get();
                let moved = font.set_logical_px(eitri_core::theme::guifont_size_px(&payload.options.guifont));
                editor_font.set(font);
                if let (Some(base), Some(controller)) = (moved, text_size_slot.borrow().as_ref()) {
                    controller.set_base(base);
                }
            }
            let tokens = eitri_core::theme::ThemeTokens::derive(&payload);
            println!(
                "[theme] following nvim colorscheme {:?} (background={})",
                payload.options.colors_name, payload.options.background
            );
            theme_css.update(&tokens, window_for_theme.upcast_ref());
            agent_panel_handle.set_theme(&panel_tokens(Some(&payload)));
            pane_for_theme.set_clear_color(editor_clear(&tokens));
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
            // The feed reports at nvim's VimEnter: a new editor session may be ready for the plugins' snippets.
            plugin_panels.editor_session();
        });
    }

    // Zoom both panes together or each one alone. `app.text-larger`, `app.text-smaller`,
    // and `app.text-reset` affect both panes and follow `g:neovide_scale_factor` from nvim.
    // The `Ctrl+a` prefix's one-pane `=`/`-`/`0` calls `apply_editor` or `apply_panel`
    // after resolving which pane holds the keys.
    let text_size_controller = text_size::TextSizeController::install(
        app,
        // Every window has an editor: a Lua `main` panel hides it rather than replacing it. The pane
        // buffers text-size writes through `ScaleWatch` until nvim starts.
        Some(pane.clone()),
        agent_panel_handle.clone(),
        panel_base,
        panel_px,
    );
    *text_size_slot.borrow_mut() = Some(text_size_controller.clone());
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

    // `Ctrl`+wheel over a module: the text size of that pane only. Every host
    // exists by now -- the editor, the agent, the terminal and each Lua panel were added above.
    for (id, host) in grid.hosts() {
        wheel_zoom::install(id, &host, text_size_controller.clone());
    }

    let top_bar = chrome::build_top_bar(&window, project_root);
    root.append(&top_bar.widget);
    root.append(&grid);

    // Names for tray chips and the prefix strip: built-ins use their kind, and Lua panels use
    // their registered `title`.
    let module_title: Rc<dyn Fn(&ModuleId) -> String> = {
        let lua_titles: Vec<(ModuleId, String)> = lua_engine
            .panels
            .borrow()
            .entries()
            .iter()
            .map(|entry| (ModuleId::lua(&entry.id), entry.title.clone()))
            .collect();
        Rc::new(move |id| tray::module_title(id, &lua_titles))
    };
    // The tray: a chip for every module off screen, one per module, in the order the
    // grid holds them. Its chips are top-bar items ahead of `↻`, for `h`/`l`, HINT and `Ctrl+k`; the
    // workspace name, left of them on the bar, is the first item.
    let tray = tray::Tray::build(
        &tray::chip_order(&grid.hosts().into_iter().map(|(id, _)| id).collect::<Vec<_>>())
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
    // A strip entry reads as its module's chip would, so a chat view on a tab not drawn shows its own
    // tab's cards' mark there (`agent 2 ⚑1`): the tray has no chip for a module its strip already names.
    {
        let title = module_title.clone();
        let agent = agent_panel_handle.clone();
        grid.set_strip_label(move |id| tray::strip_label(id, &title(id), view_attention(&agent, id)));
    }
    // The selected container, as text in the top bar beside the mark. Refreshed on every change of
    // the grid, after every action and Lua request, on the prefix's own `Esc`, and when the keys move
    // (a module taking them ends a selection: `Layout::set_focus`).
    let refresh_selection: Rc<dyn Fn()> = {
        let module_layout = module_layout.clone();
        let label = top_bar.selection.clone();
        let title = module_title.clone();
        Rc::new(move || {
            let Ok(layout) = module_layout.try_borrow() else {
                eprintln!("[modules] BUG: the layout was borrowed while the selection was redrawn");
                return;
            };
            let text = tray::selection_text(&layout, &*title);
            drop(layout);
            chrome::show_mark(&label, text.as_deref());
        })
    };
    {
        let refresh_selection = refresh_selection.clone();
        grid.connect_changed(move || refresh_selection());
    }

    // The global `f` HINT draws its GTK labels (top-bar items, the editor, each Lua panel) as
    // overlay children of this, positioned over whatever they label. Its only child is the whole
    // window content, so the layout is exactly what it was without it; the labels never take a
    // click.
    let hint_overlay = gtk4::Overlay::new();
    hint_overlay.set_child(Some(&root));
    window.set_child(Some(&hint_overlay));
    let toast = toast::Toast::install(&hint_overlay, &top_bar.widget);
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
        editor: Some(pane.clone()),
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
        focus_module: give_keys.clone(),
        strips: {
            let grid = grid.clone();
            Rc::new(move || grid.strip_entries())
        },
        activate_strip: {
            let grid = grid.downgrade();
            Rc::new(move |id: &ModuleId| {
                if let Some(grid) = grid.upgrade() {
                    grid.activate_strip_entry(id);
                }
            })
        },
        agent: agent_panel_handle.clone(),
    });
    {
        let coordinator = hint_coordinator.clone();
        agent_panel_handle.on_hint(move |message| coordinator.on_panel(message));
    }

    // The module that held the keys before the one holding them now: `prefix ;`'s target (tmux's
    // `last-pane`). Real history, fed by the same owner reports that write the layout's `focus`
    // below: the layout's own MRU list starts in tree order, so its second entry can name a module
    // nobody visited (`eitri_core::layout::FocusHistory`). The tracker below and the prefix each
    // hold a clone.
    let focus_history = Rc::new(RefCell::new(eitri_core::layout::FocusHistory::default()));
    // Where `prefix ?` found the keys, so closing its sheet by its own keys can give them back
    // (`eitri_core::keymap::keys_return`). The pane-focus tracker below forgets it when the keys move on.
    let keys_return = Rc::new(RefCell::new(KeysReturn::default()));

    // Which module has the keys. One tracker drives the editor's cursor (solid, or not drawn,
    // via Neovide itself) and the agent panel's cursor and mode block, so they cannot disagree. See
    // `pane_focus`'s module doc. The module that last held them is the layout's `focus`: where the
    // top bar's `Ctrl+j` returns to, what the `Ctrl+a` prefix zooms and resizes around, and the
    // head of the MRU list `neighbor()` breaks ties with.
    {
        let agent_panel_handle = agent_panel_handle.clone();
        let editor = pane.clone();
        let terminals = terminals.clone();
        let module_layout = module_layout.clone();
        let focus_history = focus_history.clone();
        let keys_return = keys_return.clone();
        let plugin_panels = plugin_panels.clone();
        let refresh_selection = refresh_selection.clone();
        let raise_grid = grid.downgrade();
        pane_focus::install(
            &window,
            grid.live_hosts(),
            move |id| {
                match module_layout.try_borrow_mut() {
                    Ok(mut layout) => {
                        // Refused for a hidden module (`Layout::set_focus`): GTK can put focus in an
                        // unmapped widget, and the layout's `focus` -- what `Ctrl+j` from the top bar
                        // returns to and the prefix acts on -- must stay on one that can be seen. The
                        // history hears only what the layout accepted, so it never names a module the
                        // keys could not have been on.
                        let raises = module_grid::would_raise(&layout, id);
                        match layout.set_focus(id) {
                            Ok(()) => {
                                focus_history.borrow_mut().note(id);
                                // A return is kept only for a sheet in the base view.
                                keys_return.borrow_mut().focus_moved(id, &ModuleId::agent());
                                // A float given the keys is on top of the z-order now; its host goes to
                                // the top of the grid once this focus change is over, not inside it, and
                                // when that moved it the change hooks hear of it: what it covers changed.
                                if layout.is_floating(id) {
                                    let grid = raise_grid.clone();
                                    glib::idle_add_local_once(move || {
                                        if let Some(grid) = grid.upgrade() {
                                            if raises {
                                                grid.restacked_after_raise();
                                            } else {
                                                grid.restack();
                                            }
                                        }
                                    });
                                }
                            }
                            Err(err) => eprintln!("[pane_focus] {err}; the layout keeps {}", layout.focus()),
                        }
                    }
                    // Every writer of the layout releases it before touching focus (`ModuleGrid::apply`,
                    // `hide_module`); this line is how a new one that does not would show up.
                    Err(_) => eprintln!("[pane_focus] BUG: the layout was borrowed when focus moved to {id}"),
                }
                // The keys moving to another module ended any selection; the top bar says so.
                refresh_selection();
            },
            move |id, has_keys| {
                println!("[pane_focus] {id} has_keys={has_keys}");
                match id.kind() {
                    ModuleKind::Editor => {
                        editor.set_focused(has_keys);
                        // The typing window only counts while the editor holds the keys.
                        agent_panel_handle.set_editor_has_keys(has_keys);
                    }
                    // Each chat view's page is told for itself, so exactly one is told it has the keys.
                    ModuleKind::Agent => {
                        agent_panel_handle.set_pane_focused(ViewId::of_module(id).expect("a chat module"), has_keys)
                    }
                    // One solid cursor among the terminals: the one taking the keys draws it, and
                    // every other live terminal is told it has not got them.
                    ModuleKind::Terminal => {
                        let panes: Vec<(ModuleId, terminal::TerminalPane)> = match terminals.try_borrow() {
                            Ok(terminals) => terminals.live().map(|(m, p)| (m.clone(), p.clone())).collect(),
                            Err(_) => {
                                eprintln!("[pane_focus] BUG: the registry was borrowed when {id} has_keys={has_keys}");
                                return;
                            }
                        };
                        for (m, p) in panes {
                            if m == *id {
                                p.set_focused(has_keys);
                            } else if has_keys {
                                p.set_focused(false);
                            }
                        }
                    }
                    // A plugin page hears this as its `focus` event; a Lua panel that is no plugin has no host.
                    ModuleKind::LuaWebview => plugin_panels.set_has_keys(id, has_keys),
                    _ => {}
                }
            },
        );
    }

    // Moves focus back to the module that last had it. Used by the top bar's `Ctrl+j`/`Esc`.
    let return_to_pane: Rc<dyn Fn()> = {
        let focus_module = focus_module.clone();
        let module_layout = module_layout.clone();
        Rc::new(move || {
            // Cloned out first: the grab below re-enters `pane_focus`, which writes the layout.
            let target = module_layout.borrow().focus().clone();
            focus_module(&target);
        })
    };

    // F11 fullscreen and `prefix F11` immersive both follow `g:neovide_fullscreen`.
    // `return_to_pane` restores focus when the top bar hides while focused; `focus_top_bar` can
    // reveal it.
    let window_modes =
        window_mode::WindowModes::install(app, &window, &top_bar, Some(pane.clone()), return_to_pane.clone());

    let focus_top_bar = {
        let top_items = top_items.clone();
        let window_modes = window_modes.clone();
        std::rc::Rc::new(move || {
            window_modes.reveal_top_bar();
            // The first item that is showing: a tray chip, or `↻` when the tray is empty.
            let visible: Vec<bool> = top_items.iter().map(|item| item.is_visible()).collect();
            chrome::step_item(&visible, None, 1).is_some_and(|i| top_items[i].grab_focus())
        })
    };

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
        let toast = toast.clone();
        let tray = tray.clone();
        let give_keys = give_keys.clone();
        let on_permission = config.on_permission;
        // Cloned so the closure can call back into the handle for each view's attention and newest
        // card: the hook fires from the pump (`report_attention`) after its own state borrow is
        // already dropped, so this is safe and never re-enters a held `RefCell`.
        let agent_panel_handle_for_toast = agent_panel_handle.clone();
        agent_panel_handle.on_attention(move |_before, after, handed| {
            let agent = &agent_panel_handle_for_toast;
            {
                let layout = module_layout.borrow();
                tray.refresh(&layout, &|id| view_attention(agent, id), &covered_views(&grid, &layout));
            }
            // A chat on a tab not drawn has no chip: its strip entry carries the mark, and a card
            // arriving or clearing changes no layout, so the entries are relabelled here.
            grid.refresh_strips();
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
                    toast.show_attention(&toast::permission_toast_text(
                        title,
                        mine,
                        agent
                            .newest_card_label_in(view)
                            .as_ref()
                            .map(|(n, name)| (*n, name.as_str())),
                        way_back.as_deref(),
                    ));
                }
            }
        });
    }
    // The one-line result of bringing the last window's tabs back.
    {
        let toast = toast.clone();
        agent_panel_handle.on_toast(move |text| toast.show(text));
    }
    // Share the three module arrival operations across module keys, tray chips,
    // split keys, and Lua layout calls so every route uses the same behavior.
    // Each returns layout refusals for its caller: the prefix and chips flash the
    // app name, while Lua calls log them.
    // A zoom ends only if it would hide the destination (`Layout::zoom_hides`).
    // Focusing the already zoomed chat leaves it zoomed.
    // `focus_and_arrive` gives `id` the keys and calls `arrive`. It checks
    // `Layout::can_focus` before changing anything, so refused focus leaves zoom intact.
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
    // A click on a strip entry chooses that tab and lands there as a module key would.
    {
        let focus_and_arrive = focus_and_arrive.clone();
        grid.connect_strip_activate(move |id| {
            if let Err(err) = focus_and_arrive(id) {
                println!("[modules] {id}: its strip entry chose nothing: {err}");
            }
        });
    }
    // A float's bar names its module as its chip would; a press on the bar or its grip, which raised
    // the float already, gives it the keys as a strip entry does.
    {
        let title = module_title.clone();
        grid.set_float_label(move |id| title(id));
    }
    {
        let focus_and_arrive = focus_and_arrive.clone();
        grid.connect_float_bar_press(move |id| {
            if let Err(err) = focus_and_arrive(id) {
                println!("[modules] {id}: its float bar chose nothing: {err}");
            }
        });
    }
    // A card answer from a chat view while a float covers that view is refused and the card stays
    // waiting: the answer would be for a card the user may not see. Asked at the moment of each answer,
    // for the view whose page sent it, from the arrangement then; the refusal names that view, the
    // topmost float over it and the key that clears it. A float over another view does not count.
    {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let title = module_title.clone();
        let refused_name = top_bar.app_name.clone();
        let toast = toast.clone();
        let stow_key = key_text(&keymap, &Action::ScratchpadShow { module: None });
        let sink_key = key_text(&keymap, &Action::FloatToggle);
        agent_panel_handle.set_answer_gate(Rc::new(move |view: ViewId| {
            let chat = view.module();
            let Ok(layout) = module_layout.try_borrow() else {
                // Nothing holds the layout across a page message; were something to, the answer is
                // not taken unchecked.
                let text = "the layout is changing: answer again".to_string();
                refuse_text(&refused_name, &toast, &text);
                return Err(text);
            };
            let covering = view_covering(&grid, &layout, view);
            let Some(top) = covering.last() else {
                return Ok(());
            };
            let scratch = layout.float_of(top).is_some_and(|float| float.scratch);
            drop(layout);
            let key = if scratch { &stow_key } else { &sink_key };
            let text = covered_text(&title(&chat), &title(top), key, scratch);
            refuse_text(&refused_name, &toast, &text);
            Err(text)
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
    // `prefix ?` and `prefix :` open something in a chat view and give it the keys: under a float the
    // sheet or the line would be out of sight while every key went to it. Scratch floats over the view
    // that do not hold the keys are put away first, as for a card that reveals; a float still over it
    // refuses the key with the text a card answer gets. `None` when the view can take the keys in view.
    let chat_covered_refusal: Rc<dyn Fn(ViewId) -> Option<String>> = {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let title = module_title.clone();
        let give_keys = give_keys.clone();
        let stow_key = key_text(&keymap, &Action::ScratchpadShow { module: None });
        let sink_key = key_text(&keymap, &Action::FloatToggle);
        Rc::new(move |view: ViewId| {
            let chat = view.module();
            if module_layout.borrow().is_floating(&chat) {
                // Taking the keys raises a floating chat over every other float.
                return None;
            }
            if !reveal_covered_chat(&grid, &module_layout, view, &*give_keys) {
                return None;
            }
            let layout = module_layout.borrow();
            let top = view_covering(&grid, &layout, view).last().cloned()?;
            let scratch = layout.float_of(&top).is_some_and(|float| float.scratch);
            drop(layout);
            let key = if scratch { &stow_key } else { &sink_key };
            Some(covered_text(&title(&chat), &title(&top), key, scratch))
        })
    };
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
    // their modules in the order they were typed, the keys left where they are.
    if let Some(socket) = follow_socket.as_mut() {
        let window = GridFollowWindow {
            module_layout: module_layout.clone(),
            grid: grid.downgrade(),
            show_on_screen: show_on_screen.clone(),
            focus_module: focus_module.clone(),
        };
        let plugin_panels = plugin_panels.clone();
        feed_pump.listen_follow(socket, move |batch| {
            plugin_panels::apply_follow_commands(&window, plugin_panels.route_follow(batch, std::time::Instant::now()));
        });
    }
    // The scratch round trips (`Ctrl+g`, `gf`): the editor is shown and given the keys before nvim
    // is handed the request, and an edit that came back returns the keys to the chat.
    {
        let pane = pane.clone();
        let show_on_screen = show_on_screen.clone();
        let give_keys = give_keys.clone();
        let module_layout = module_layout.clone();
        agent_panel_handle.on_editor_request(move |request| {
            // `prefix x` quit it, and it cannot come back in this window (`kill_pane`).
            if module_layout.borrow().is_gone(&ModuleId::editor()) {
                return Err(LayoutError::Gone(ModuleId::editor()).to_string());
            }
            // Check `is_running` before sending keys: an exited nvim is also not ready, but keys sent to
            // it would reach nothing.
            if !pane.is_running() {
                return Err("the editor is not ready yet".to_string());
            }
            show_on_screen(&ModuleId::editor()).map_err(|e| e.to_string())?;
            give_keys(&ModuleId::editor());
            pane.send_keys(&request.input_keys());
            Ok(())
        });
    }
    // Turn review's questions to the editor, with the same two refusals: the editor quit for good,
    // or its nvim not running.
    {
        let editor_present: Rc<dyn Fn() -> bool> = {
            let module_layout = module_layout.clone();
            Rc::new(move || !module_layout.borrow().is_gone(&ModuleId::editor()))
        };
        // One handle on this window's editor, shared: the chat's turn review and every plugin page ask through it.
        let editor_rpc: Rc<dyn eitri_core::editor_rpc::EditorRpc> =
            Rc::new(editor_rpc::EmbeddedEditorRpc::new(pane.clone(), editor_present));
        agent_panel_handle.set_editor_rpc(editor_rpc.clone());
        plugin_panels.set_editor(editor_rpc);
    }
    // The review overlay in this window's own nvim: the module goes with the window, and a file the
    // review opened there is shown and given the keys, as a draft being edited is.
    {
        agent_panel_handle.set_review_owner(|| Some(eitri_core::review_editor::Owner::Embedded));
        let give_keys = give_keys.clone();
        let show_on_screen = show_on_screen.clone();
        agent_panel_handle.on_review_open(move || {
            if let Err(err) = show_on_screen(&ModuleId::editor()) {
                eprintln!("[review] could not show the editor: {err}");
                return;
            }
            give_keys(&ModuleId::editor());
        });
    }
    {
        let focus_and_arrive = focus_and_arrive.clone();
        // Back to the view the edit's tab is in; to the base view when that view has gone meanwhile.
        agent_panel_handle.on_editor_done(move |view| {
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
        });
    }
    // `place_and_arrive`: `id` into a new split after the module with the keys, along `axis`, moved
    // there if it is elsewhere, and the keys to it (`Ctrl+a \`/`"` + a key, `Ctrl+a <key>` for a
    // module never placed, `eitri.layout.split`). A start-hidden plugin module placed this way is then
    // held, hidden, by every other workspace that lacks it, as one opened by its rule is.
    let place_and_arrive: Rc<dyn Fn(&ModuleId, Axis) -> Result<(), LayoutError>> = {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let terminals = terminals.clone();
        let chat_views = chat_views.clone();
        let rules = rules.clone();
        let workspaces = workspaces.clone();
        let plugin_panels = plugin_panels.clone();
        let give_keys = give_keys.clone();
        let arrive = arrive.clone();
        Rc::new(move |id, axis| {
            let live = match id.kind() {
                ModuleKind::Agent => chat_views.borrow().get(id).is_some(),
                _ => terminals.borrow().get(id).is_some(),
            };
            placeable_by_id(id, live)?;
            let target = module_layout.borrow().focus().clone();
            grid.place_module(id, &target, axis)?;
            if plugin_panels.starts_hidden(id) {
                let placement = plugin_hidden_placement(&rules, plugin_panels.own_rule(id).as_ref(), id);
                workspaces.borrow_mut().add_everywhere_at(id, placement);
            }
            if give_keys(id) {
                arrive(id);
            }
            Ok(())
        })
    };

    // `Ctrl+a <key>`'s four cases for a module,
    // for the prefix and the tray's chips; a refusal flashes the app name.
    // A plugin module that starts hidden is opened by its own rule rather than split beside the keys, and
    // its key gives it the keys as every module key does (`open_plugin_module`).
    let open_module: Rc<dyn Fn(&ModuleId, KeyAction)> = {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let rules = rules.clone();
        let workspaces = workspaces.clone();
        let plugin_panels = plugin_panels.clone();
        let give_keys = give_keys.clone();
        let focus_module = focus_module.clone();
        let focus_and_arrive = focus_and_arrive.clone();
        let show_on_screen = show_on_screen.clone();
        let place_and_arrive = place_and_arrive.clone();
        let app_name = top_bar.app_name.clone();
        let toast = toast.clone();
        Rc::new(move |id, action| {
            println!("[modules] {id}: {action:?}");
            let result = match action {
                KeyAction::Place | KeyAction::ShowAndFocus if plugin_panels.starts_hidden(id) => {
                    let keys = module_layout.borrow().focus().clone();
                    open_plugin_module(
                        &grid,
                        &rules,
                        &workspaces,
                        &plugin_panels,
                        id,
                        Some(&keys),
                        Some(true),
                        &*give_keys,
                    )
                }
                KeyAction::ShowAndFocus => show_on_screen(id).and_then(|()| focus_and_arrive(id)),
                KeyAction::Focus => focus_and_arrive(id),
                KeyAction::Place => place_and_arrive(id, Axis::Row),
                // The keys go to the neighbour first, then it unmaps (`module_grid::hide_then_unmap`).
                KeyAction::Hide => grid.hide_module(id, &*focus_module),
            };
            if let Err(err) = result {
                refuse(&app_name, &toast, &err);
            }
        })
    };

    // `prefix x`, tmux's `kill-pane`.
    // `killed` holds the modules killed and not shown since: a show brings each back fresh (the hook
    // below). The editor never is -- it cannot come back (`Reopen::Never`). `kill_prompt` is the
    // window's y/n, installed after the prefix below so its controller sees keys first.
    let killed: Rc<RefCell<std::collections::BTreeSet<ModuleId>>> = Rc::default();
    let kill_prompt: Rc<std::cell::OnceCell<Rc<close_prompt::ClosePrompt>>> = Rc::default();
    // The editor closed for the rest of this window once nvim is gone (`Reopen::Never`: it cannot
    // come back, `kill_pane`), the keys going to its neighbour, and the pane released on the next
    // turn of the loop so nothing reaches the closed connection. Refused as a hide is (the editor is
    // the last module on screen); the caller decides what then.
    // Every other workspace retires it too, since nvim is gone for all of them: where it was the only
    // module on screen there, the chat is shown first (`Workspaces::retire_everywhere`).
    let retire_editor: Rc<dyn Fn() -> Result<(), LayoutError>> = {
        let grid = grid.downgrade();
        let focus_module = focus_module.clone();
        let workspaces = workspaces.clone();
        let released = Rc::downgrade(&pane);
        let plugin_panels = plugin_panels.clone();
        Rc::new(move || {
            let Some(grid) = grid.upgrade() else { return Ok(()) };
            grid.kill_module(&ModuleId::editor(), Reopen::Never, &*focus_module)?;
            // No editor is coming back to this window: a page following one says so.
            plugin_panels.editor_closed();
            grid.with_current_frame(|frame| workspaces.borrow_mut().retire_everywhere(&ModuleId::editor(), frame));
            println!("[modules] editor: nvim quit; the editor is closed for this window");
            let released = released.clone();
            glib::idle_add_local_once(move || {
                if let Some(pane) = released.upgrade() {
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
        prompt: kill_prompt.clone(),
        agent: agent_panel_handle.clone(),
        terminals: Rc::downgrade(&terminals),
        window: window.clone(),
        app_name: top_bar.app_name.clone(),
        toast: toast.clone(),
    }));
    // A terminal's kill, by `prefix x` or by its shell ending: it leaves the screen as a hide does --
    // the keys go to the module the layout chooses, before its host unmaps -- then its shell is hung
    // up (SIGHUP) and forgotten. A numbered instance leaves the layout and every workspace, and its pane
    // goes idle under its id; its host stays in the grid, child-invisible, for the next instance of
    // that number. The base terminal goes back below the editor, hidden, and keeps its pane, so its
    // next show starts a fresh shell there.
    let kill_terminal: Rc<dyn Fn(&ModuleId) -> Result<(), LayoutError>> = {
        let grid = grid.downgrade();
        let focus_module = focus_module.clone();
        // Weak: every pane's own end calls this, and the registry holds the panes.
        let terminals = Rc::downgrade(&terminals);
        let workspaces = workspaces.clone();
        let decls = decls.clone();
        Rc::new(move |id| {
            let (Some(grid), Some(terminals)) = (grid.upgrade(), terminals.upgrade()) else {
                return Ok(());
            };
            grid.kill_module(id, kill_pane::reopen(id, &decls), &*focus_module)?;
            if id.number() >= 2 {
                terminals.borrow_mut().retire(id);
                workspaces.borrow_mut().remove_everywhere(id);
            } else {
                let base = terminals.borrow().get(id).cloned();
                if let Some(base) = base {
                    base.close();
                }
            }
            Ok(())
        })
    };
    // The chat views whose removal is their caller's own, while it closes or moves their tab: the kill
    // of a view, and a new view taking its tab back after a refusal. The panel's word that such a view
    // holds no tab is then not a request to remove it.
    let chat_busy: Rc<RefCell<std::collections::BTreeSet<ViewId>>> = Rc::default();
    // An extra chat view leaves the window: out of the layout (its keys handed on as a kill hands them,
    // before its host unmaps), out of every other workspace, and its page idle under its id for the next
    // view of that number. Its host stays in the grid, child-invisible. `true` when there was anything
    // to remove.
    let drop_chat_view: Rc<dyn Fn(&ModuleId) -> Result<bool, LayoutError>> = {
        let grid = grid.downgrade();
        let module_layout = module_layout.clone();
        let focus_module = focus_module.clone();
        let workspaces = workspaces.clone();
        let chat_views = chat_views.clone();
        let agent = agent_panel_handle.clone();
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
        let window = window.clone();
        let grid = grid.downgrade();
        let agent = agent_panel_handle.clone();
        agent_panel_handle.on_view_empty(move |view| {
            if view.is_base() || chat_busy.borrow().contains(&view) {
                return;
            }
            let drop_chat_view = drop_chat_view.clone();
            let give_keys = give_keys.clone();
            let show_on_screen = show_on_screen.clone();
            let module_layout = module_layout.clone();
            let window = window.clone();
            let grid = grid.clone();
            let agent = agent.clone();
            glib::idle_add_local_once(move || {
                if agent.views().contains(&view) {
                    return;
                }
                let Some(grid) = grid.upgrade() else { return };
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
                if pane_focus::focused_module(&window, &grid.hosts()).as_ref() == Some(&id) {
                    give_keys(&ModuleId::agent());
                }
                match drop_chat_view(&id) {
                    Ok(true) => println!("[chat] {id} holds no tab; removed"),
                    Ok(false) => {}
                    Err(err) => println!("[chat] {id} holds no tab and could not be removed: {err}"),
                }
            });
        });
    }
    let kill_now: Rc<dyn Fn(&ModuleId, Option<String>) -> Result<(), String>> = {
        let grid = grid.clone();
        let chat_busy = chat_busy.clone();
        let drop_chat_view = drop_chat_view.clone();
        let module_layout = module_layout.clone();
        let focus_module = focus_module.clone();
        let kill_terminal = kill_terminal.clone();
        let agent = agent_panel_handle.clone();
        let lua_pages = lua_pages.clone();
        let decls = decls.clone();
        let killed = killed.clone();
        let editor_quitting = editor_quitting.clone();
        let plugin_panels = plugin_panels.clone();
        Rc::new(move |id, confirmed| {
            let scope = eitri_core::layout::can_kill(&module_layout.borrow(), id).map_err(|e| e.to_string())?;
            if scope == KillScope::Window {
                if id.kind() == ModuleKind::Editor {
                    // nvim first, as for any kill of the editor: its exit closes the window.
                    editor_quitting.kill_editor(KillScope::Window, confirmed)?;
                } else {
                    editor_quitting.close_after(confirmed);
                }
                return Ok(());
            }
            let reopen = kill_pane::reopen(id, &decls);
            match id.kind() {
                ModuleKind::Terminal => kill_terminal(id).map_err(|e| e.to_string())?,
                ModuleKind::LuaWebview => {
                    grid.kill_module(id, reopen, &*focus_module)
                        .map_err(|e| e.to_string())?;
                    // A no-op for a Lua panel that is not a plugin.
                    plugin_panels.stop(id);
                    if let Some((_, widget, _, _)) = lua_pages.iter().find(|(m, _, _, _)| m == id) {
                        // A plugin page built after startup sits inside its module's slot; one never built
                        // has no web process to end.
                        let webview = plugin_panels
                            .webview(id)
                            .or_else(|| widget.downcast_ref::<webkit6::WebView>().cloned());
                        if let Some(webview) = webview {
                            use webkit6::prelude::*;
                            webview.terminate_web_process();
                        }
                    }
                    killed.borrow_mut().insert(id.clone());
                }
                // An extra view: its one tab closed, then the view leaves the layout and every workspace,
                // and its page goes idle under its id.
                ModuleKind::Agent if id.number() >= 2 => {
                    let view = ViewId::of_module(id).expect("a chat module");
                    chat_busy.borrow_mut().insert(view);
                    let closed = agent.close_view_tabs(view);
                    chat_busy.borrow_mut().remove(&view);
                    let closed = closed?;
                    drop_chat_view(id).map_err(|e| e.to_string())?;
                    println!("[modules] {id}: killed, {closed} session tab(s) closed; the view left the layout");
                }
                // The base view: only its own tabs close. A tab shown in another view is that view's.
                ModuleKind::Agent => {
                    let closed = agent.close_view_tabs(ViewId::BASE)?;
                    grid.kill_module(id, reopen, &*focus_module)
                        .map_err(|e| e.to_string())?;
                    println!("[modules] {id}: killed, {closed} session tab(s) closed");
                    killed.borrow_mut().insert(id.clone());
                }
                // nvim decides, through its own `:confirm qall` (`editor_quit::EditorQuitting::
                // kill_editor`): a running nvim is asked, an exited one retired, one that never ran
                // hidden in place.
                ModuleKind::Editor => editor_quitting.kill_editor(KillScope::Module, None)?,
                ModuleKind::Canvas => return Err(format!("{id} is not in this window yet")),
            }
            Ok(())
        })
    };
    // A killed module shown again, however it got there -- its key, its tray chip,
    // `eitri.layout.show`, a split key -- comes back fresh: the chat opens its session chooser, a
    // Lua panel loads its page again. (The terminal's shell starts through the hook above, which
    // starts one whenever it is shown and none runs.)
    {
        let module_layout = module_layout.clone();
        let killed = killed.clone();
        let agent = agent_panel_handle.clone();
        let lua_pages = lua_pages.clone();
        let plugin_panels = plugin_panels.clone();
        grid.connect_changed(move || {
            let revived: Vec<ModuleId> = {
                let Ok(layout) = module_layout.try_borrow() else {
                    eprintln!("[modules] BUG: the layout was borrowed when it changed; nothing revived");
                    return;
                };
                killed
                    .borrow()
                    .iter()
                    .filter(|id| layout.is_shown(id))
                    .cloned()
                    .collect()
            };
            for id in revived {
                killed.borrow_mut().remove(&id);
                println!("[modules] {id}: shown again after a kill, starting it fresh");
                match id.kind() {
                    ModuleKind::Agent => agent.open_chooser(ViewId::of_module(&id).expect("a chat module")),
                    ModuleKind::LuaWebview => {
                        if let Some((_, widget, url, crash_guard)) = lua_pages.iter().find(|(m, _, _, _)| *m == id) {
                            let webview = plugin_panels
                                .webview(&id)
                                .or_else(|| widget.downcast_ref::<webkit6::WebView>().cloned());
                            if let Some(webview) = webview {
                                use webkit6::prelude::*;
                                webview.load_uri(url);
                            }
                            // This IS the manual recovery a give-up message points the user at
                            // (`prefix x`, then this panel's own key): re-arm the guard so a later,
                            // unrelated crash is caught fresh rather than silently dropped as
                            // `CrashResponse::AlreadyGivenUp`.
                            crash_guard.borrow_mut().reset();
                        }
                    }
                    _ => {}
                }
            }
        });
    }
    // A chip is `Ctrl+a <key>` for its module, from the top bar: it never holds the keys itself.
    {
        let open_module = open_module.clone();
        let module_layout = module_layout.clone();
        tray.on_activate(move |id| {
            let action = eitri_core::layout::key_action(&module_layout.borrow(), id, false);
            open_module(id, action);
        });
    }
    // Every layout change redraws the tray, a float moving onto or off a chat view included. A float
    // leaving a view brings it on screen as a map would: what finished under the float is seen then.
    {
        let tray = tray.clone();
        let module_layout = module_layout.clone();
        let agent_panel_handle = agent_panel_handle.clone();
        let changed_grid = grid.downgrade();
        let was_covered: Rc<RefCell<Vec<ModuleId>>> = Rc::default();
        grid.connect_changed(move || {
            let Some(grid) = changed_grid.upgrade() else { return };
            let uncovered: Vec<ViewId> = {
                let Ok(layout) = module_layout.try_borrow() else { return };
                let covered = covered_views(&grid, &layout);
                tray.refresh(&layout, &|id| view_attention(&agent_panel_handle, id), &covered);
                let was = was_covered.replace(covered.clone());
                was.iter()
                    .filter(|id| !covered.contains(id))
                    .filter_map(ViewId::of_module)
                    .collect()
            };
            // Once this change is over, never inside it: the panel's attention hook reads the layout
            // and the grid again. The panel checks then that the page is mapped and clear, so a view
            // that left the screen with its float sees nothing.
            if !uncovered.is_empty() {
                let panel = agent_panel_handle.clone();
                glib::idle_add_local_once(move || {
                    for view in uncovered {
                        panel.page_shown(view);
                    }
                });
            }
        });
    }
    {
        let layout = module_layout.borrow();
        tray.refresh(
            &layout,
            &|id| view_attention(&agent_panel_handle, id),
            &covered_views(&grid, &layout),
        );
    }

    // --- A module takes the keys by keyboard: `Ctrl+h/j/k/l` (`move_focus`, below) and `prefix ;`/
    // `prefix o` (the prefix's `Action::SelectLast`/`SelectNext`) land the same way. A zoom ends
    // first, as tmux's `select-pane` does. Arriving in the agent lands in BROWSE rather than in
    // the composer, on the last row if the reader was following it, else where they left it,
    // and `arrive` also drops a bypass prompt. A click on a row does not come through here and
    // still lands in BROWSE on that row. `false`: the module did not take the keys.
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

    // Ctrl+h/j/k/l navigates between modules by geometry. The editor's shim and each web module's
    // capture controller share this function. A move unzooms first. Up with nothing above goes to
    // the top bar; other directions with no neighbour return `false`, letting the key continue to
    // the module.
    let move_focus: Rc<dyn Fn(&ModuleId, Direction) -> bool> = {
        let grid = grid.clone();
        let land_by_key = land_by_key.clone();
        let focus_top_bar = focus_top_bar.clone();
        Rc::new(move |from, direction| match grid.navigate(from, direction) {
            Nav::Module(to) => {
                let grabbed = land_by_key(&to);
                println!("[pane_switch] {from} {direction:?} -> {to} (grab_focus={grabbed})");
                true
            }
            Nav::TopBar => {
                let grabbed = focus_top_bar();
                println!("[pane_switch] {from} {direction:?} -> the top bar (grab_focus={grabbed})");
                true
            }
            Nav::Nothing => {
                // Not "ignoring": from a web module the key goes on to that module (`false`), which
                // is where the panel's own `Ctrl+l`/`Ctrl+j` have always gone.
                println!("[pane_switch] {from} {direction:?} -> no module that way, not moving");
                false
            }
        })
    };

    // C1's stale-mirror recovery: the page could not apply a `nav_key` against its own
    // current state (the mirror was a keystroke stale, or INPUT is refused on a dead session), so it
    // asks Rust to do exactly what the chord would have done.
    agent_panel_handle.on_nav_fallthrough({
        let move_focus = move_focus.clone();
        move |view: ViewId, direction| {
            move_focus(&view.module(), direction);
        }
    });

    // The `?` sheet that `prefix ?` opened was closed. When its own keys (or a click outside it) closed it
    // and the keys are still in the chat, they go back to the module that held them, with the zoom and the
    // hidden chat that opening it changed put back. Every borrow is released before anything moves, since
    // moving the keys re-enters the layout and this hook's own state through `pane_focus`.
    agent_panel_handle.on_keymap_closed({
        let grid = grid.clone();
        let window = window.clone();
        let module_layout = module_layout.clone();
        let focus_module = focus_module.clone();
        let give_keys = give_keys.clone();
        let keys_return = keys_return.clone();
        move |opening, return_keys| {
            // Only an opening in the base view keeps a return: a chat view holding the keys opens the
            // sheet in its own page, where the keys already are.
            let chat = ModuleId::agent();
            let focused = pane_focus::focused_module(&window, &grid.hosts());
            let ret = {
                let layout = module_layout.borrow();
                let shown = |id: &ModuleId| layout.is_shown(id);
                let zoom_hides = |id: &ModuleId| layout.zoom_hides(id);
                let at = AtClose {
                    focused: focused.as_ref(),
                    zoom: layout.zoom(),
                    zoom_hides: &zoom_hides,
                    sheet_shown: layout.is_shown(&chat),
                    shown: &shown,
                };
                keys_return.borrow_mut().closed(opening, return_keys, at, &chat)
            };
            let Some(ret) = ret else { return };
            if ret.hide_sheet {
                if let Err(err) = grid.hide_module(&chat, &*focus_module) {
                    eprintln!("[keys_return] could not hide {chat} again: {err}");
                }
            }
            // A module hidden before the close got no `Return` at all; this refusal is only a backstop
            // for one the hide just above took away.
            if !give_keys(&ret.focus) {
                println!("[keys_return] {} could not take the keys back", ret.focus);
                return;
            }
            // After the keys, not before: giving them to a module on a tab not drawn chooses that tab,
            // which ends any zoom, and a module whose tab is not drawn cannot be zoomed.
            if let Some(zoom) = &ret.zoom {
                if !grid.restore_zoom(zoom, &ret.focus) {
                    println!(
                        "[keys_return] the zoom {zoom:?} could not be put back around {}",
                        ret.focus
                    );
                }
            }
        }
    });

    // What every terminal pane gets once, when it is made: its own end, and its own `Ctrl+h/j/k/l`.
    //
    // The shell ending by itself -- `exit`, `Ctrl+d`, a signal -- closes that terminal as `prefix x`
    // does, without asking (`terminal::closes_on_exit` says which ends). A report from a shell that is
    // no longer the pane's current one (the instance was killed and its number reused since) closes
    // nothing. As the last module on screen, tmux's own rule (closing the last pane closes the window)
    // applies instead: `eitri_core::layout::can_kill` says which (`terminal::on_exit`), and a `Window`
    // scope closes the window through its own ordinary path -- `close window? N running (y/n)` when a
    // tab runs -- rather than the module, leaving the terminal's notice on screen until the window
    // really goes.
    //
    // `Ctrl+h/j/k/l` in a terminal are Eitri's, always: a capture-phase controller on its own widget,
    // like the web modules' controllers below, so the chord is claimed before the terminal's key
    // controller hands it to the shell. Unlike theirs, all four are swallowed even with no module that
    // way, so what the shell receives never depends on the layout: `Ctrl+k` goes to the most recent
    // module above it, and the rest are tmux's no-op at the edge unless a module is there. Each moves
    // from the terminal's own id. The literal forms the shell loses are `Ctrl+a Ctrl+a` and
    // `Ctrl+a Ctrl+l`.
    let wire_terminal: Rc<dyn Fn(&ModuleId, &terminal::TerminalPane)> = {
        let grid = grid.downgrade();
        let module_layout = module_layout.clone();
        let terminals = Rc::downgrade(&terminals);
        let kill_terminal = kill_terminal.clone();
        let window = window.clone();
        let move_focus = move_focus.clone();
        Rc::new(move |id, pane| {
            {
                let id = id.clone();
                let grid = grid.clone();
                let module_layout = module_layout.clone();
                let terminals = terminals.clone();
                let kill_terminal = kill_terminal.clone();
                let window = window.clone();
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
                        terminal::OnExit::CloseModule => match kill_terminal(&id) {
                            Ok(()) => {
                                println!("[terminal] {id}: the shell ended; the terminal is closed");
                                true
                            }
                            Err(err) => {
                                println!("[terminal] {id}: the shell ended, and the terminal stays: {err}");
                                false
                            }
                        },
                    }
                });
            }
            let id = id.clone();
            let move_focus = move_focus.clone();
            let controller = EventControllerKey::new();
            controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
            controller.connect_key_pressed(move |_controller, key, _keycode, state| {
                match terminal::navigation(key, state) {
                    Some(direction) => {
                        move_focus(&id, direction);
                        glib::Propagation::Stop
                    }
                    None => glib::Propagation::Proceed,
                }
            });
            pane.widget().add_controller(controller);
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

    // A terminal instance's pane, with its shell to start in `dir`: the registry's idle pane of that
    // number if it was killed before, whose host the grid already holds, else a new pane with its own
    // host, off screen until something shows it.
    let host_terminal: Rc<dyn Fn(&ModuleId, PathBuf)> = {
        let grid = grid.clone();
        let terminals = terminals.clone();
        let make_terminal_pane = make_terminal_pane.clone();
        let wire_terminal = wire_terminal.clone();
        let text_size_controller = text_size_controller.clone();
        Rc::new(move |id, dir| {
            let (pane, reused) = terminals.borrow_mut().take_or_make(id, || make_terminal_pane());
            pane.set_start_dir(dir);
            if !grid.has_host(id) {
                if reused == terminal::registry::Reused::Yes {
                    eprintln!("[terminal] BUG: {id} reused a pane the grid has no host for; giving it one");
                }
                grid.add(id.clone(), pane.widget(), pane.widget(), HostKind::Direct);
                // Off screen until a placement shows it (`ModuleGrid::apply` maps it then).
                pane.widget().set_child_visible(false);
                wire_terminal(id, &pane);
                wheel_zoom::install(id.clone(), pane.widget().upcast_ref(), text_size_controller.clone());
            }
        })
    };
    // A new terminal instance: the lowest number no workspace holds and no data kept for a later build
    // names, a pane from the registry -- the idle one of that number if it was killed before, whose host
    // the grid already holds, else a new pane with its own host -- placed as `how` says, added hidden
    // to every other workspace at its declared place, and given the keys unless its open rule keeps
    // them where they are. A split key from a terminal says where (`OpenHow::Beside`, never a rule);
    // `instance.new`, `M-Enter` and `eitri.layout.open` go where the open rules say. Its shell starts
    // in `dir` through the change hook, when the placement shows it. A split's `beside` is checked
    // here, not where the request was made: a split that read its shell's directory off the GTK thread
    // finds out only now whether its target is still there.
    let new_terminal: Rc<dyn Fn(OpenHow, PathBuf) -> Result<ModuleId, String>> = {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let workspaces = workspaces.clone();
        let terminals = terminals.clone();
        let host_terminal = host_terminal.clone();
        let give_keys = give_keys.clone();
        let rules = rules.clone();
        let decls = decls.clone();
        Rc::new(move |how, dir| {
            let id = {
                let layout = module_layout.borrow();
                if let OpenHow::Beside(beside, _) = &how {
                    if !layout.contains(beside) {
                        return Err(format!("{beside} has left the layout; no terminal was opened"));
                    }
                }
                let workspaces = workspaces.borrow();
                let layouts: Vec<&eitri_core::layout::Layout> =
                    workspaces.each(&layout).into_iter().map(|(_, layout)| layout).collect();
                new_instance_id(ModuleKind::Terminal, &layouts, &workspaces.reserved())
            }
            .ok_or_else(|| "at most 9 terminals".to_string())?;
            host_terminal(&id, dir);
            let opened = match open_instance(&grid, &rules, &id, &how, &*give_keys) {
                Ok(opened) => opened,
                Err(err) => {
                    terminals.borrow_mut().retire(&id);
                    return Err(err.to_string());
                }
            };
            workspaces
                .borrow_mut()
                .add_everywhere_at(&id, declared_placement(&decls, &id, Placement::BelowEditor));
            println!("[terminal] {id}: opened {}", how.describe(&opened));
            Ok(id)
        })
    };
    // `scratchpad.show { module = "terminal#N" }` for an instance that does not exist yet: made as
    // `new_terminal` makes one, but added as a float rather than placed in the tree, centred at its
    // open rule's float size, else the default share of the grid, and hidden in every other workspace
    // at its declared place. Not given the keys here: the caller asks the scratchpad to show it, which
    // does that. Refused for a number another workspace holds or data kept for a later build names,
    // since two modules must never share a name.
    let new_scratch_terminal: Rc<dyn Fn(&ModuleId, PathBuf) -> Result<(), String>> = {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let workspaces = workspaces.clone();
        let terminals = terminals.clone();
        let host_terminal = host_terminal.clone();
        let rules = rules.clone();
        let decls = decls.clone();
        Rc::new(move |id, dir| {
            {
                let layout = module_layout.borrow();
                let workspaces = workspaces.borrow();
                if workspaces.reserved().contains(id) {
                    return Err(format!("{id}: a later build's saved layout holds that number"));
                }
                if workspaces.each(&layout).iter().any(|(_, layout)| layout.contains(id)) {
                    return Err(format!("{id}: another workspace holds it"));
                }
            }
            host_terminal(id, dir);
            let size = rule_for(&rules, id, OpenPoint::NewInstance, None).size;
            let rect = grid.with_current_frame(|frame| eitri_core::layout::rules::float_rect(size, frame.size));
            // Added as a plain float: the scratchpad's show that follows makes it a scratch float and
            // shows it, where a float already in the scratchpad and on screen would be stowed instead.
            let added = module_layout.borrow_mut().add_float(id, rect, false);
            if let Err(err) = added {
                terminals.borrow_mut().retire(id);
                return Err(err.to_string());
            }
            workspaces
                .borrow_mut()
                .add_everywhere_at(id, declared_placement(&decls, id, Placement::BelowEditor));
            println!("[terminal] {id}: opened as a float for the scratchpad");
            Ok(())
        })
    };

    // An extra chat view's page and host: its page from the pool -- the live one, else the idle one
    // recorded under this id, attached again with a fresh document, else a new page built for this view
    // by the one constructor, with the base view's page as its related one -- and, the first time, its
    // host, off screen until a placement shows it, with the keys' routes every web module has.
    let host_chat_view: Rc<dyn Fn(&ModuleId) -> Result<(), String>> = {
        let grid = grid.clone();
        let chat_views = chat_views.clone();
        let agent = agent_panel_handle.clone();
        let base_webview = base_webview.clone();
        let move_focus = move_focus.clone();
        let text_size_controller = text_size_controller.clone();
        Rc::new(move |id| {
            let Some(base) = base_webview.as_ref() else {
                return Err(CHAT_VIEWS_NEED_WEBKIT.to_string());
            };
            let view = ViewId::of_module(id)
                .filter(|view| !view.is_base())
                .ok_or_else(|| format!("{id} is not an extra chat view"))?;
            let was_live = chat_views.borrow().get(id).is_some();
            let ((webview, page), reused) = chat_views
                .borrow_mut()
                .take_or_make(id, || agent_panel::build_chat_page(&agent, view, Some(base)));
            // Says which of the pool's two answers this was, so a log shows a page reused rather than built.
            println!(
                "[chat] {id}: page {}",
                if reused == chat_views::Reused::Yes {
                    "reused"
                } else {
                    "built"
                }
            );
            if reused == chat_views::Reused::Yes && !was_live {
                // The same page, still answering for this view only: it starts again from an empty
                // record and a fresh document.
                agent.attach_page(view, page);
                agent.load_document(view);
            }
            if !grid.has_host(id) {
                if reused == chat_views::Reused::Yes {
                    eprintln!("[chat] BUG: {id} reused a page the grid has no host for; giving it one");
                }
                let host = WebHost::new(&webview);
                grid.add(id.clone(), &host, &webview, web_host);
                // Off screen until a placement shows it (`ModuleGrid::apply` maps it then).
                host.set_child_visible(false);
                let host: gtk4::Widget = host.upcast();
                install_module_nav(
                    id.clone(),
                    &host,
                    move_focus.clone(),
                    Some(agent.nav_intercept(view)),
                    false,
                );
                panel_super::install(&host);
                wheel_zoom::install(id.clone(), &host, text_size_controller.clone());
            }
            Ok(())
        })
    };
    // A page that never got its view placed waits idle under its id.
    let retire_chat_page: Rc<dyn Fn(&ModuleId)> = {
        let chat_views = chat_views.clone();
        let agent = agent_panel_handle.clone();
        Rc::new(move |id| {
            if chat_views.borrow_mut().retire(id) {
                if let Some(view) = ViewId::of_module(id) {
                    agent.idle_page(view);
                }
            }
        })
    };
    // A new chat view: the lowest `agent#N` no workspace holds and no kept data reserves, its page and
    // host (`host_chat_view`), its tab -- a new empty tab of the window's default agent, or the base
    // view's active tab brought out of it -- placed as `how` says, added hidden to every other
    // workspace at its declared place, and given the keys unless its open rule keeps them where they
    // are. A split key then the chat's key with the keys in a chat view says where
    // (`OpenHow::Beside`); `instance.new { kind = "agent" }`, `eitri.layout.open("agent")` and
    // `tab.break` go where the open rules say. A refused placement takes the tab back (the empty tab
    // closed, a brought tab joined back) and leaves the page idle.
    let new_chat_view: Rc<dyn Fn(OpenHow, NewViewTab) -> Result<ModuleId, String>> = {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let workspaces = workspaces.clone();
        let agent = agent_panel_handle.clone();
        let host_chat_view = host_chat_view.clone();
        let retire_chat_page = retire_chat_page.clone();
        let chat_busy = chat_busy.clone();
        let give_keys = give_keys.clone();
        let rules = rules.clone();
        let decls = decls.clone();
        Rc::new(move |how, tab| {
            let id = {
                let layout = module_layout.borrow();
                if let OpenHow::Beside(beside, _) = &how {
                    if !layout.contains(beside) {
                        return Err(format!("{beside} has left the layout; no chat view was opened"));
                    }
                }
                let workspaces = workspaces.borrow();
                let layouts: Vec<&eitri_core::layout::Layout> =
                    workspaces.each(&layout).into_iter().map(|(_, layout)| layout).collect();
                new_instance_id(ModuleKind::Agent, &layouts, &workspaces.reserved())
            }
            .ok_or_else(|| "at most 9 chat views".to_string())?;
            host_chat_view(&id)?;
            let view = ViewId::of_module(&id).expect("a chat module");
            chat_busy.borrow_mut().insert(view);
            let placed = (|| {
                match tab {
                    NewViewTab::Empty => agent.open_in(view).map(drop)?,
                    NewViewTab::Break => agent.break_out(view).map(drop)?,
                }
                open_instance(&grid, &rules, &id, &how, &*give_keys).map_err(|err| {
                    let back = match tab {
                        NewViewTab::Empty => agent.close_view_tabs(view).map(drop).map_err(String::from),
                        NewViewTab::Break => agent.join_back(view).map(drop),
                    };
                    if let Err(back) = back {
                        eprintln!("[chat] {id}: its tab could not be taken back: {back}");
                    }
                    err.to_string()
                })
            })();
            chat_busy.borrow_mut().remove(&view);
            let opened = match placed {
                Ok(opened) => opened,
                Err(err) => {
                    retire_chat_page(&id);
                    return Err(err);
                }
            };
            workspaces
                .borrow_mut()
                .add_everywhere_at(&id, declared_placement(&decls, &id, Placement::BelowEditor));
            if opened.takes_keys {
                agent.arrive(view);
            }
            println!("[chat] {id}: opened {}", how.describe(&opened));
            Ok(id)
        })
    };
    // `scratchpad.show { module = "agent#N" }` for a view that does not exist yet: made as
    // `new_chat_view` makes one, with a new empty tab, but added as a float rather than placed in the
    // tree, centred at its open rule's float size, else the default share of the grid, and hidden in
    // every other workspace at its declared place. Not given the keys here: the caller asks the
    // scratchpad to show it, which does that. Refused for a number another workspace holds or data kept
    // for a later build names, since two modules must never share a name.
    let new_scratch_chat_view: Rc<dyn Fn(&ModuleId) -> Result<(), String>> = {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let workspaces = workspaces.clone();
        let agent = agent_panel_handle.clone();
        let host_chat_view = host_chat_view.clone();
        let retire_chat_page = retire_chat_page.clone();
        let chat_busy = chat_busy.clone();
        let rules = rules.clone();
        let decls = decls.clone();
        Rc::new(move |id| {
            {
                let layout = module_layout.borrow();
                let workspaces = workspaces.borrow();
                if workspaces.reserved().contains(id) {
                    return Err(format!("{id}: a later build's saved layout holds that number"));
                }
                if workspaces.each(&layout).iter().any(|(_, layout)| layout.contains(id)) {
                    return Err(format!("{id}: another workspace holds it"));
                }
            }
            host_chat_view(id)?;
            let view = ViewId::of_module(id).expect("a chat module");
            chat_busy.borrow_mut().insert(view);
            let added = (|| {
                agent.open_in(view)?;
                let size = rule_for(&rules, id, OpenPoint::NewInstance, None).size;
                let rect = grid.with_current_frame(|frame| eitri_core::layout::rules::float_rect(size, frame.size));
                // Added as a plain float: the scratchpad's show that follows makes it a scratch float
                // and shows it.
                let added = module_layout.borrow_mut().add_float(id, rect, false);
                added.map_err(|err| {
                    if let Err(back) = agent.close_view_tabs(view) {
                        eprintln!("[chat] {id}: its tab could not be closed: {back}");
                    }
                    err.to_string()
                })
            })();
            chat_busy.borrow_mut().remove(&view);
            if let Err(err) = added {
                retire_chat_page(id);
                return Err(err);
            }
            workspaces
                .borrow_mut()
                .add_everywhere_at(id, declared_placement(&decls, id, Placement::BelowEditor));
            println!("[chat] {id}: opened as a float for the scratchpad");
            Ok(())
        })
    };

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
        let window = window.clone();
        let agent = agent_panel_handle.clone();
        let give_keys = give_keys.clone();
        let show_on_screen = show_on_screen.clone();
        let refused_name = top_bar.app_name.clone();
        let toast = toast.clone();
        let tab_verb_view = tab_verb_view.clone();
        Rc::new(move |tab_action: TabAction| {
            let chat = ModuleId::agent();
            let keys = match tab_verb_view.take() {
                Some(view) => Some(view.module()),
                None => pane_focus::focused_module(&window, &grid.hosts()),
            };
            let keys_view = keys.as_ref().and_then(ViewId::of_module);
            // The base view shown and given the keys, for a verb that needs them there.
            let base_takes_the_keys = || match show_on_screen(&chat) {
                Ok(()) => {
                    give_keys(&chat);
                    true
                }
                Err(err) => {
                    refuse(&refused_name, &toast, &err);
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
                    flash(&refused_name);
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
                None => flash(&refused_name),
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
                flash(&refused_name);
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
        let refused_name = top_bar.app_name.clone();
        let toast = toast.clone();
        let agent = agent_panel_handle.clone();
        agent_panel_handle.on_show_view(move |view| {
            let id = view.module();
            if let Err(err) = show_on_screen(&id) {
                refuse(&refused_name, &toast, &err);
                return;
            }
            if give_keys(&id) {
                agent.arrive(view);
            }
        });
    }
    // The mark `swap.marked` swaps with, as text in the top bar. Refreshed after every mark verb and
    // every change of the grid: a module retired or killed takes its mark with it.
    let refresh_mark: Rc<dyn Fn()> = {
        let module_layout = module_layout.clone();
        let label = top_bar.mark.clone();
        let title = module_title.clone();
        Rc::new(move || {
            let Ok(layout) = module_layout.try_borrow() else {
                eprintln!("[modules] BUG: the layout was borrowed while the mark was redrawn");
                return;
            };
            let text = chrome::mark_text(layout.mark(), &*title);
            drop(layout);
            chrome::show_mark(&label, text.as_deref());
        })
    };
    {
        let refresh_mark = refresh_mark.clone();
        grid.connect_changed(move || refresh_mark());
    }
    // The current workspace's name in the top bar, while there are several. Refreshed on every change
    // of the grid -- a switch is one, through `apply` -- and after a rename or a delete, which move no
    // module.
    let refresh_workspace: Rc<dyn Fn()> = {
        let workspaces = workspaces.clone();
        let button = top_bar.workspace.clone();
        let label = top_bar.workspace_label.clone();
        let project = chrome::project_display_name(project_root);
        Rc::new(move || {
            let Ok(workspaces) = workspaces.try_borrow() else {
                eprintln!("[workspace] BUG: the workspaces were borrowed while the top bar was redrawn");
                return;
            };
            let text = chrome::workspace_text(workspaces.order(), workspaces.current());
            drop(workspaces);
            chrome::show_workspace(&button, &label, &project, text.as_deref());
        })
    };
    {
        let refresh_workspace = refresh_workspace.clone();
        grid.connect_changed(move || refresh_workspace());
    }
    refresh_workspace();
    // A rename or a delete changes what the state file holds without changing any layout, so the
    // grid's change hook never hears of it: the saver and the top bar are told here.
    let workspaces_changed: Rc<dyn Fn()> = {
        let layout_saver = layout_saver.clone();
        let refresh_workspace = refresh_workspace.clone();
        Rc::new(move || {
            layout_saver.changed();
            refresh_workspace();
        })
    };

    // `select.number`: the numbers as drawn, which the digit typed next is looked up in -- not the
    // layout as it is by then -- and the prefix, which exists only once `prefix::install` returns
    // below but is what the runner starts the digit wait on.
    let number_plan: Rc<RefCell<Vec<(char, ModuleId)>>> = Rc::default();
    let prefix_cell: Rc<std::cell::OnceCell<Rc<RefCell<prefix::Prefix>>>> = Rc::default();

    // Every change of what the prefix waits for: the app name's block, the strip, and the digits,
    // which come down whenever the digit wait ends however it ended. Shared, because the digit wait's
    // timer ends that wait from outside the prefix's own controller.
    let on_waiting: Rc<dyn Fn(prefix::Waiting)> = {
        let app_name = top_bar.app_name.clone();
        let strip_keys = module_keys.clone();
        let strip_verbs = keymap.strip_verbs();
        let strip_keymap = keymap.clone();
        let strip_layout = module_layout.clone();
        let strip_title = module_title.clone();
        let prefix_strip = prefix_strip.clone();
        let hint_coordinator = hint_coordinator.clone();
        let number_plan = number_plan.clone();
        let strip_workspaces = workspaces.clone();
        let window_modes = window_modes.clone();
        let revealed_for_wait = Rc::new(std::cell::Cell::new(false));
        Rc::new(move |waiting| {
            if waiting != prefix::Waiting::Number {
                hint_coordinator.hide_numbers();
            }
            // The chooser's strip is the only list of what its keys pick, so a hidden top bar
            // (immersive) is revealed while it waits and hidden again once it stops waiting; the
            // keys stay in the module, so no focus change would hide it.
            if wait_needs_the_top_bar(waiting) {
                window_modes.reveal_top_bar();
                revealed_for_wait.set(true);
            } else if revealed_for_wait.replace(false) {
                window_modes.conceal_top_bar();
            }
            if waiting == prefix::Waiting::No {
                app_name.remove_css_class("prefix-armed");
            } else {
                app_name.add_css_class("prefix-armed");
            }
            let pieces = if waiting == prefix::Waiting::Number {
                prefix_strip::number_pieces(&number_plan.borrow(), &*strip_title)
            } else if let prefix::Waiting::Workspace { deleting } = waiting {
                let workspaces = strip_workspaces.borrow();
                prefix_strip::workspace_pieces(workspaces.order(), workspaces.current(), deleting)
            } else {
                let layout = strip_layout.borrow();
                // Armed, the keys that reach each module as the effective table binds them (a
                // `del`ed `e` is not advertised); after a split key, the fixed module keys
                //.
                let entries = match waiting {
                    prefix::Waiting::Command => eitri_core::layout::strip_direct(&strip_keys, &strip_keymap, &layout),
                    _ => eitri_core::layout::strip(&strip_keys, &layout),
                };
                prefix_strip::strip_pieces(
                    waiting,
                    &entries,
                    &strip_title(layout.focus()),
                    &*strip_title,
                    &strip_verbs,
                )
            };
            prefix_strip.show(&pieces);
        })
    };

    // What `scratchpad.show` says with nothing in the scratchpad: how a module gets there, as the
    // keymap binds `scratchpad.move`.
    let scratch_empty_text: Rc<str> = format!(
        "the scratchpad is empty ({} sends a module there)",
        key_text(&keymap, &Action::ScratchpadMove)
    )
    .into();
    // A workspace switch by any route -- the previous/next/last keys, the chooser, `workspace.switch`,
    // `eitri.layout.switch` -- in the grid's order: the keys to the arriving workspace's module first,
    // then the unmap and the restack (`ModuleGrid::switch_workspace`). The saver, the tray, the mark,
    // the selection and the top bar's name all hang off the grid's change hook, which that order runs.
    let switch_workspace: Rc<dyn Fn(&str) -> Result<bool, WorkspaceError>> = {
        let grid = grid.clone();
        let workspaces = workspaces.clone();
        let decls = decls.clone();
        let focus_module = focus_module.clone();
        let keys_return = keys_return.clone();
        Rc::new(move |to| {
            let switched = grid.switch_workspace(&workspaces, to, &decls, &*focus_module)?;
            if switched {
                // What an open `?` sheet would give back -- the module the keys came from, the chat to
                // hide, the zoom to put back -- belongs to the workspace left behind: closing the sheet
                // here must not act on it.
                keys_return.borrow_mut().superseded();
                println!("[workspace] switched to {to}");
            }
            Ok(switched)
        })
    };
    // `workspace.switch { name }` and `eitri.layout.switch(name)`: the workspace of that name, made first
    // when there is none -- from `init.lua`'s `eitri.layout.workspace` of that name, else as a copy of
    // the current arrangement -- then switched to. A name that cannot be a workspace's makes nothing.
    let switch_or_make: Rc<dyn Fn(&str) -> Result<bool, String>> = {
        let grid = grid.clone();
        let workspaces = workspaces.clone();
        let module_layout = module_layout.clone();
        let lua_engine = lua_engine.clone();
        let decls = decls.clone();
        let deferred = deferred.clone();
        let switch_workspace = switch_workspace.clone();
        Rc::new(move |name| {
            let exists = workspaces.borrow().order().iter().any(|existing| existing == name);
            if !exists {
                Workspaces::valid_name(name).map_err(|err| err.to_string())?;
                let template = lua_engine
                    .layout
                    .borrow()
                    .defined_workspaces()
                    .iter()
                    .find(|(defined, _)| defined == name)
                    .map(|(_, tree)| tree.clone());
                let (layout, notes, gone, spread) = {
                    let live = module_layout.borrow();
                    let (layout, notes) = new_workspace_layout(template, &live, &decls, &deferred)?;
                    let gone: Vec<ModuleId> = live.modules().into_iter().filter(|id| live.is_gone(id)).collect();
                    // A start-hidden plugin module the definition names and the window holds nowhere yet: once
                    // this workspace holds it, every other one holds it hidden, as at startup.
                    let spread: Vec<ModuleId> = deferred
                        .iter()
                        .filter(|id| layout.contains(id) && !live.contains(id))
                        .cloned()
                        .collect();
                    (layout, notes, gone, spread)
                };
                for note in notes {
                    println!("[layout] workspace '{name}': {note}");
                }
                workspaces
                    .borrow_mut()
                    .create(name, layout)
                    .map_err(|err| err.to_string())?;
                // A module this window has retired -- the editor once nvim is gone -- cannot come back
                // in a workspace made after it.
                for id in gone {
                    grid.with_current_frame(|frame| workspaces.borrow_mut().retire_everywhere(&id, frame));
                }
                for id in spread {
                    let mut live = module_layout.borrow_mut();
                    let mut modules = live.modules();
                    modules.push(id.clone());
                    for note in eitri_core::layout::fit_to_with(&mut live, &modules, &decls) {
                        println!("[layout] workspace '{}': {note}", workspaces.borrow().current());
                    }
                    workspaces
                        .borrow_mut()
                        .add_everywhere_at(&id, declared_placement(&decls, &id, Placement::BelowEditor));
                }
                println!("[workspace] made {name}");
            }
            switch_workspace(name).map_err(|err| err.to_string())
        })
    };
    // `workspace.rename`: the entry in the top bar, holding the current name, selected. `main` keeps
    // its name, so its rename is refused before anything opens.
    let start_rename: Rc<dyn Fn()> = {
        let workspaces = workspaces.clone();
        let entry = top_bar.rename.clone();
        let window_modes = window_modes.clone();
        let refused_name = top_bar.app_name.clone();
        let toast = toast.clone();
        Rc::new(move || {
            let current = workspaces.borrow().current().to_string();
            if current == MAIN {
                refuse_text(&refused_name, &toast, &WorkspaceError::Main.to_string());
                return;
            }
            // The bar may be hidden (immersive): shown first, as `Ctrl+k` shows it.
            window_modes.reveal_top_bar();
            chrome::start_rename(&entry, &current);
        })
    };

    // One runner for every key action, from the prefix and from `eitri.layout.run`, so a key and a
    // Lua call cannot drift apart. `keycode` is the key that ran it, which only `hint` reads (that
    // key is still down); Lua passes 0 and cannot run `hint`, which is not a layout action.
    // Below `move_focus` because `select.*` uses it. Reached only through `run_action` below, which
    // holds the container selection's rules around it.
    let run_verb: Rc<dyn Fn(Action, u32)> = {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let editor = pane.clone();
        let agent = agent_panel_handle.clone();
        let window_for_focus = window.clone();
        let refused_name = top_bar.app_name.clone();
        let text_size_controller = text_size_controller.clone();
        let terminals = terminals.clone();
        let new_terminal = new_terminal.clone();
        let new_scratch_terminal = new_scratch_terminal.clone();
        let new_chat_view = new_chat_view.clone();
        let new_scratch_chat_view = new_scratch_chat_view.clone();
        let scratch_empty_text = scratch_empty_text.clone();
        let project_dir = project_root.to_path_buf();
        let focus_module = focus_module.clone();
        let give_keys = give_keys.clone();
        let open_module = open_module.clone();
        let show_on_screen = show_on_screen.clone();
        let move_focus = move_focus.clone();
        let land_by_key = land_by_key.clone();
        let focus_history = focus_history.clone();
        let hint_coordinator = hint_coordinator.clone();
        let window_modes = window_modes.clone();
        let keymap_for_send = keymap.clone();
        let run_tab_action = run_tab_action.clone();
        let kill_now = kill_now.clone();
        let kill_prompt = kill_prompt.clone();
        let kill_title = module_title.clone();
        let kill_refused = top_bar.app_name.clone();
        let toast = toast.clone();
        let refresh_mark = refresh_mark.clone();
        let number_plan = number_plan.clone();
        let prefix_cell = prefix_cell.clone();
        let on_waiting = on_waiting.clone();
        let keymap_plugins = plugin_panels.clone();
        let keys_return = keys_return.clone();
        let chat_covered_refusal = chat_covered_refusal.clone();
        let workspaces = workspaces.clone();
        let switch_workspace = switch_workspace.clone();
        let switch_or_make = switch_or_make.clone();
        let start_rename = start_rename.clone();
        let rules = rules.clone();
        let panel_decls = panel_decls.clone();
        Rc::new(move |action: Action, keycode: u32| {
            // The module that last held the keys: what zoom, resize, hide and swap act on.
            let target = || module_layout.borrow().focus().clone();
            // The module that holds the keys right now (`pane_focus`'s own definition).
            let focused = || pane_focus::focused_module(&window_for_focus, &grid.hosts());
            // A terminal's pane by its id: the base terminal or a live instance.
            let pane_of = |id: &ModuleId| terminals.borrow().get(id).cloned();
            match action {
                Action::MoveFar(dir) => {
                    let target = target();
                    match grid.move_far(&target, dir) {
                        Ok(true) => {}
                        // Already against that edge: nothing to do, as a swap with nothing that way.
                        Ok(false) => println!("[modules] {target}: already at the far {dir:?} edge"),
                        Err(err) => refuse(&refused_name, &toast, &err),
                    }
                }
                Action::ToggleSplit => {
                    if !grid.toggle_split(&target()) {
                        let text = "toggle-split: no split around the module on screen";
                        println!("[modules] {text}");
                        refuse_said(&refused_name, &toast, text);
                    }
                }
                Action::Rotate { down } => {
                    if !grid.rotate(down) {
                        let text = "rotate: fewer than two modules on screen";
                        println!("[modules] {text}");
                        refuse_said(&refused_name, &toast, text);
                    }
                }
                Action::Flip(axis) => {
                    if !grid.flip(axis) {
                        let text = "flip: no split that way on screen";
                        println!("[modules] {text}");
                        refuse_said(&refused_name, &toast, text);
                    }
                }
                Action::Spread => {
                    if !grid.spread(&target()) {
                        let text = "spread: no split around the module on screen";
                        println!("[modules] {text}");
                        refuse_said(&refused_name, &toast, text);
                    }
                }
                // `even-*` are presets too, so `layout.next` goes on from them.
                Action::Even(axis) => {
                    let preset = match axis {
                        Axis::Row => Preset::EvenHorizontal,
                        Axis::Column => Preset::EvenVertical,
                    };
                    if let Err(err) = grid.preset(preset) {
                        refuse(&refused_name, &toast, &err);
                    }
                }
                Action::MainHorizontal | Action::MainVertical | Action::Tiled => {
                    let preset = match action {
                        Action::MainHorizontal => Preset::MainHorizontal,
                        Action::MainVertical => Preset::MainVertical,
                        _ => Preset::Tiled,
                    };
                    if let Err(err) = grid.preset(preset) {
                        refuse(&refused_name, &toast, &err);
                    }
                }
                Action::CycleLayout { forward } => match grid.cycle_preset(forward) {
                    Ok(preset) => println!("[modules] layout -> {preset:?}"),
                    Err(err) => refuse(&refused_name, &toast, &err),
                },
                Action::LayoutUndo => {
                    if let Err(err) = grid.undo() {
                        refuse(&refused_name, &toast, &err);
                    }
                }
                Action::MarkToggle => {
                    let id = target();
                    let toggled = module_layout.borrow_mut().toggle_mark(&id);
                    match toggled {
                        Ok(marked) => println!("[modules] {id}: marked={marked}"),
                        Err(err) => refuse(&refused_name, &toast, &err),
                    }
                    refresh_mark();
                }
                Action::MarkClear => {
                    let cleared = module_layout.borrow_mut().clear_mark();
                    if !cleared {
                        println!("[modules] no module is marked");
                    }
                    refresh_mark();
                }
                Action::SwapMarked => {
                    let target = target();
                    match grid.swap_marked(&target) {
                        Ok(marked) => println!("[modules] {target}: swapped with the marked {marked}"),
                        Err(err) => refuse(&refused_name, &toast, &err),
                    }
                }
                // tmux's `display-panes`: the digits go up first, then the prefix waits for one,
                // so a window that cannot draw them never swallows the next key.
                Action::SelectNumber => {
                    let plan = eitri_core::layout::number_plan(&module_layout.borrow());
                    if plan.is_empty() {
                        let text = "select.number: no module on screen to number";
                        println!("[prefix] {text}");
                        refuse_said(&refused_name, &toast, text);
                        return;
                    }
                    let Some(prefix) = prefix_cell.get() else {
                        return;
                    };
                    let drawn = hint_coordinator.show_numbers(&plan, &grid.hosts_in_tree_order());
                    println!("[prefix] select.number: {} numbered, {drawn} drawn", plan.len());
                    *number_plan.borrow_mut() = plan;
                    prefix.borrow_mut().await_number(std::time::Instant::now());
                    let prefix = Rc::downgrade(prefix);
                    let on_waiting = on_waiting.clone();
                    glib::timeout_add_local_once(prefix::DISPLAY_PANES_TIME, move || {
                        let Some(prefix) = prefix.upgrade() else { return };
                        // `false` when the wait already ended (a digit, a key, a click) or a later
                        // `select.number` started its own, with its own timer.
                        let ended = prefix.borrow_mut().expire(std::time::Instant::now());
                        if ended {
                            println!("[prefix] armed=false waiting=No (no digit in time)");
                            on_waiting(prefix::Waiting::No);
                        }
                    });
                }
                Action::Zoom => grid.toggle_zoom(&target()),
                Action::Resize { dir, cells } => {
                    let cell = editor.cell_size().unwrap_or(layout::FALLBACK_CELL);
                    grid.resize(&target(), dir, layout::resize_px(dir, cell, cells));
                }
                Action::Select(dir) => {
                    move_focus(&target(), dir);
                }
                // tmux's `last-pane` and `select-pane -t :.+`, landing the
                // way `Ctrl+h/j/k/l` do (`land_by_key`). No `RefCell` borrow is held across that
                // call: `on_owner` runs inside it and writes both the layout and the history.
                Action::SelectLast => {
                    let previous = focus_history.borrow().previous().cloned();
                    match previous.filter(|id| module_layout.borrow().can_focus(id).is_ok()) {
                        Some(id) => {
                            let grabbed = land_by_key(&id);
                            println!("[prefix] select.last -> {id} (grab_focus={grabbed})");
                        }
                        None => {
                            let text = "no last module";
                            println!("[prefix] {text}");
                            refuse_said(&refused_name, &toast, text);
                        }
                    }
                }
                Action::SelectNext => {
                    let next = {
                        let layout = module_layout.borrow();
                        eitri_core::layout::next_on_screen(&layout, layout.focus())
                    };
                    match next {
                        Some(id) => {
                            let grabbed = land_by_key(&id);
                            println!("[prefix] select.next -> {id} (grab_focus={grabbed})");
                        }
                        None => {
                            let text = "no other module on screen";
                            println!("[prefix] {text}");
                            refuse_said(&refused_name, &toast, text);
                        }
                    }
                }
                Action::SendPrefix | Action::SendKeys(_) => {
                    let Some(key) = prefix::literal_for(&action, &keymap_for_send) else {
                        return;
                    };
                    match terminal::literal_target(focused().as_ref().map(ModuleId::kind)) {
                        terminal::LiteralTarget::Editor => editor.send_keys(&key.to_vim()),
                        terminal::LiteralTarget::Panel => {
                            // To the page of the chat view that holds the keys.
                            let view = focused().as_ref().and_then(ViewId::of_module).unwrap_or(ViewId::BASE);
                            agent.literal_key(view, &key)
                        }
                        terminal::LiteralTarget::Terminal => match terminal::keys::literal(&key) {
                            // To the terminal that holds the keys, whichever instance it is.
                            Some(inputs) => {
                                if let Some(pane) = focused().as_ref().and_then(pane_of) {
                                    inputs.into_iter().for_each(|input| pane.send(input));
                                }
                            }
                            None => println!("[prefix] {key}: the terminal takes C-<letter> or one character"),
                        },
                        terminal::LiteralTarget::Neither => {}
                    }
                }
                // `Ctrl+a t`'s rule since the terminal's phase 1 (`terminal::toggle_action`), in the
                // order `ToggleAction::steps` gives and the terminal's own tests pin. The base id is the
                // key `t`, which reaches the terminal with the keys, else the most recent one, else the
                // base; a numbered id (`module.terminal#2`) reaches that instance only.
                Action::Module(id) if id.kind() == ModuleKind::Terminal => {
                    let id = eitri_core::layout::module_target(&module_layout.borrow(), &id);
                    if !module_layout.borrow().contains(&id) {
                        let text = format!("{id}: not in this window");
                        println!("[prefix] {text}");
                        refuse_said(&refused_name, &toast, &text);
                        return;
                    }
                    let has_keys = focused().as_ref() == Some(&id);
                    let shown = module_layout.borrow().is_shown(&id);
                    for step in terminal::toggle_action(shown, has_keys).steps() {
                        match step {
                            terminal::ToggleStep::Unzoom => {
                                grid.unzoom();
                            }
                            terminal::ToggleStep::Show => {
                                if let Err(err) = grid.show_module(&id) {
                                    eprintln!("[terminal] not shown: {err}");
                                }
                            }
                            terminal::ToggleStep::Start => {
                                if let Some(pane) = pane_of(&id) {
                                    pane.start();
                                }
                            }
                            terminal::ToggleStep::FocusTerminal => {
                                give_keys(&id);
                            }
                            terminal::ToggleStep::Hide => {
                                if let Err(err) = grid.hide_module(&id, &*focus_module) {
                                    refuse(&refused_name, &toast, &err);
                                }
                            }
                        }
                    }
                }
                Action::Module(id) => {
                    // The chat's key reaches the chat view with the keys, else the one holding the oldest
                    // waiting card, else the most recent one; a numbered view's key addresses that view.
                    let id = if id == ModuleId::agent() {
                        chat_target(&module_layout.borrow(), agent.oldest_card_view())
                    } else {
                        id
                    };
                    // `v` before the canvas exists, or a module this window has not got.
                    if !grid.has_host(&id) {
                        let text = format!("{id}: not in this window yet");
                        println!("[prefix] {text}");
                        refuse_said(&refused_name, &toast, &text);
                        return;
                    }
                    let has_keys = focused().as_ref() == Some(&id);
                    let action = eitri_core::layout::key_action(&module_layout.borrow(), &id, has_keys);
                    open_module(&id, action);
                }
                Action::ModuleHide => open_module(&target(), KeyAction::Hide),
                Action::ModuleKill => {
                    let id = target();
                    let scope = match eitri_core::layout::can_kill(&module_layout.borrow(), &id) {
                        Ok(scope) => scope,
                        Err(err) => {
                            refuse(&refused_name, &toast, &err);
                            return;
                        }
                    };
                    let (running, queued) = (agent.running_count(), agent.queued_count());
                    // Counted now, as the window close counts them, so the close compares like with like.
                    let jobs = terminal::jobs::count_terminal_jobs(&terminals.borrow());
                    // An extra chat view's y/n names the one tab it closes, with that tab's own counts.
                    let text = match ViewId::of_module(&id).filter(|view| !view.is_base()) {
                        Some(view) if scope == KillScope::Module => {
                            let (running, queued) = agent.view_running_queued(view);
                            let tab = agent.view_tab_label(view);
                            kill_pane::prompt_closing_tab(
                                &id,
                                &kill_title(&id),
                                scope,
                                tab.as_deref(),
                                running,
                                queued,
                                jobs,
                            )
                        }
                        _ => kill_pane::prompt(&id, &kill_title(&id), scope, running, queued, jobs),
                    };
                    // What `y` also answers for the window close, if this kill closes it.
                    let confirmed = eitri_core::tabs::window_close_prompt_with_jobs(running, queued, jobs);
                    let Some(prompt) = kill_prompt.get() else {
                        return;
                    };
                    println!("[modules] {id}: {text}");
                    let kill_now = kill_now.clone();
                    let kill_refused = kill_refused.clone();
                    prompt.ask(&text, move || {
                        if let Err(err) = kill_now(&id, confirmed) {
                            println!("[modules] {id}: not killed: {err}");
                            flash(&kill_refused);
                        }
                    });
                }
                Action::Swap(SwapTarget::Toward(dir)) => {
                    let target = target();
                    if grid.swap_modules(&target, dir).is_none() {
                        println!("[modules] {target}: nothing to swap with {dir:?}");
                    }
                }
                Action::Swap(which) => {
                    let target = target();
                    if grid.swap_adjacent(&target, which == SwapTarget::Next).is_none() {
                        println!("[modules] {target}: nothing to swap with");
                    }
                }
                Action::Text(change) => match text_size::route_focused_module(focused().as_ref()) {
                    text_size::TextSizeTarget::Editor => text_size_controller.apply_editor(change.into()),
                    text_size::TextSizeTarget::Panel => text_size_controller.apply_panel(change.into()),
                    text_size::TextSizeTarget::Neither => {}
                },
                Action::Hint => hint_coordinator.toggle_from_prefix(keycode),
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
                        refuse(&refused_name, &toast, &err);
                        return;
                    }
                    if let Some(text) = chat_covered_refusal(view) {
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
                        refuse_text(&refused_name, &toast, &text);
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
                    let opening = keys_return.borrow_mut().opened(
                        KeysOpened {
                            from,
                            ended_zoom,
                            showed_sheet,
                        },
                        &chat,
                    );
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
                        refuse(&refused_name, &toast, &err);
                        return;
                    }
                    if let Some(text) = chat_covered_refusal(view) {
                        refuse_text(&refused_name, &toast, &text);
                        return;
                    }
                    // A close of the `?` sheet may still be on its way from the page: the command line is
                    // the newer request, and that close must not move the keys out from under it.
                    keys_return.borrow_mut().superseded();
                    give_keys(&chat);
                    agent.open_command_line(view);
                }
                Action::WindowImmersive => window_modes.toggle_immersive(),
                // `run_tab_action` is shared with the panel's `tab_verb` messages.
                // It computes `keys_in` the same way `focused()` does here.
                Action::Tab(tab_action) => run_tab_action(tab_action),
                // The prefix never hands a split key back: it waits for a module key instead.
                Action::Split { .. } => {}
                // i3's `move <dir>`: the module with the keys, or the selected container. A float
                // moves by `FLOAT_MOVE_CELLS` editor cells that way, as `M-arrow` resizes a tile.
                Action::Move(dir) => {
                    let target = target();
                    if module_layout.borrow().is_floating(&target) {
                        let cell = editor.cell_size().unwrap_or(layout::FALLBACK_CELL);
                        if !grid.move_float(&target, dir, layout::resize_px(dir, cell, FLOAT_MOVE_CELLS)) {
                            println!("[modules] {target}: the float is against the {dir:?} edge");
                        }
                        return;
                    }
                    match grid.move_dir(&target, dir) {
                        Ok(true) => {}
                        Ok(false) => println!("[modules] {target}: nowhere to move {dir:?}"),
                        Err(err) => refuse(&refused_name, &toast, &err),
                    }
                }
                // i3's `layout tabbed`/`layout stacking`; pressed again, the container's split again.
                Action::ContainerTabbed | Action::ContainerStacked => {
                    let mode = if action == Action::ContainerTabbed {
                        ConLayout::Tabbed
                    } else {
                        ConLayout::Stacked
                    };
                    let target = target();
                    match grid.container_mode(&target, mode) {
                        Ok(true) => {}
                        Ok(false) => {
                            println!("[modules] {}: no container around {target}", action.name());
                            flash(&refused_name);
                        }
                        Err(err) => refuse(&refused_name, &toast, &err),
                    }
                }
                // i3's `focus parent`/`focus child`: the selection widens or narrows; the top bar
                // says what is selected (`run_action` redraws it after every action).
                Action::FocusParent | Action::FocusChild => {
                    let moved = if action == Action::FocusParent {
                        grid.focus_parent()
                    } else {
                        grid.focus_child()
                    };
                    if !moved {
                        let text = format!("{}: nothing to select", action.name());
                        println!("[modules] {text}");
                        refuse_said(&refused_name, &toast, &text);
                    }
                }
                // A new terminal where the open rules say -- by default right of the module with the
                // keys -- its shell in the project root.
                Action::InstanceNew {
                    kind: ModuleKind::Terminal,
                } => {
                    if let Err(text) = new_terminal(OpenHow::new_instance(target(), None), project_dir.clone()) {
                        refuse_text(&refused_name, &toast, &text);
                    }
                }
                // A new chat view where the open rules say -- by default right of the module with the
                // keys -- holding a new empty tab.
                Action::InstanceNew {
                    kind: ModuleKind::Agent,
                } => {
                    if let Err(text) = new_chat_view(OpenHow::new_instance(target(), None), NewViewTab::Empty) {
                        refuse_text(&refused_name, &toast, &text);
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
                        Err(err) => refuse(&refused_name, &toast, &err),
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
                            refuse_said(&refused_name, &toast, text);
                        }
                    }
                }
                Action::ScratchpadMove => {
                    let target = target();
                    if let Err(err) = grid.scratchpad_move(&target, &*focus_module) {
                        refuse(&refused_name, &toast, &err);
                    }
                }
                // i3's `scratchpad show`. A named terminal instance or chat view that does not exist yet
                // is made straight into the scratchpad and shown.
                Action::ScratchpadShow { module } => {
                    let shown = match grid.scratchpad_show(module.as_ref(), &*give_keys) {
                        Ok(ScratchShow::Missing(id)) => {
                            let made = match id.kind() {
                                ModuleKind::Terminal => new_scratch_terminal(&id, project_dir.clone()),
                                ModuleKind::Agent => new_scratch_chat_view(&id),
                                _ => Err(format!("{id}: not in this window")),
                            };
                            if let Err(text) = made {
                                refuse_text(&refused_name, &toast, &text);
                                return;
                            }
                            grid.scratchpad_show(Some(&id), &*give_keys)
                        }
                        other => other,
                    };
                    match shown {
                        Ok(ScratchShow::Missing(id)) => {
                            println!("[modules] {id}: made, and still not in the layout");
                            flash(&refused_name);
                        }
                        Ok(change) => println!("[modules] scratchpad.show: {change:?}"),
                        Err(LayoutError::NoScratch) => refuse_text(&refused_name, &toast, &scratch_empty_text),
                        Err(err) => refuse(&refused_name, &toast, &err),
                    }
                }
                // With one workspace there is nothing to step to: a flash, as a swap with nothing
                // that way.
                Action::WorkspaceStep { forward } => {
                    let next = workspaces.borrow().step(forward);
                    match next {
                        Some(name) => {
                            if let Err(err) = switch_workspace(&name) {
                                refuse_text(&refused_name, &toast, &err.to_string());
                            }
                        }
                        None => {
                            let text = format!("{}: this window has one workspace", action.name());
                            println!("[workspace] {text}");
                            refuse_said(&refused_name, &toast, &text);
                        }
                    }
                }
                Action::WorkspaceLast => {
                    let last = workspaces.borrow().last().map(str::to_string);
                    match last {
                        Some(name) => {
                            if let Err(err) = switch_workspace(&name) {
                                refuse_text(&refused_name, &toast, &err.to_string());
                            }
                        }
                        None => refuse_text(&refused_name, &toast, NO_LAST_WORKSPACE),
                    }
                }
                // A copy of the current arrangement, named with the next free number, and gone to: a
                // workspace made and not shown would be found only by stepping through them all.
                Action::WorkspaceNew => {
                    let name = {
                        let live = module_layout.borrow();
                        workspaces.borrow_mut().create_copy(&live)
                    };
                    println!("[workspace] made {name}");
                    if let Err(err) = switch_workspace(&name) {
                        refuse_text(&refused_name, &toast, &err.to_string());
                    }
                }
                Action::WorkspaceSwitch { name } => {
                    if let Err(text) = switch_or_make(&name) {
                        refuse_text(&refused_name, &toast, &text);
                    }
                }
                Action::WorkspaceChoose => {
                    // Announcing the new wait draws the chooser strip (`on_waiting`); the prefix does
                    // that for a command it ran, and the top bar's click does it itself.
                    if let Some(prefix) = prefix_cell.get() {
                        prefix.borrow_mut().await_workspace();
                    }
                }
                Action::WorkspaceRename => start_rename(),
                Action::InstanceNew { .. } => {
                    let text = format!("{}: not in this build yet", action.name());
                    println!("[prefix] {text}");
                    refuse_said(&refused_name, &toast, &text);
                }
                // tmux's `break-pane`, on the chat view with the keys: from the base view its active tab
                // goes out into a new view right of it; from an extra view that view's tab comes back
                // into the base view, and the view, empty then, goes (`on_view_empty`).
                Action::TabBreak => match break_plan(focused().as_ref().and_then(ViewId::of_module)) {
                    // Where the open rules say for a broken-out tab: by default right of the base view,
                    // where the key was pressed.
                    BreakPlan::Break => {
                        let how = OpenHow::Rule {
                            point: OpenPoint::Break,
                            keys: ModuleId::agent(),
                            explicit: None,
                        };
                        if let Err(text) = new_chat_view(how, NewViewTab::Break) {
                            refuse_text(&refused_name, &toast, &text);
                        }
                    }
                    BreakPlan::Join(view) => {
                        // Asked before anything moves: a view with no tab has nothing to join, and the
                        // keys would otherwise be left in the base view for a refusal.
                        if !agent.views().contains(&view) {
                            refuse_text(&refused_name, &toast, &format!("agent {} shows no tab", view.number()));
                            return;
                        }
                        // The keys leave the view before it goes, into the base view the tab comes back to.
                        let chat = ModuleId::agent();
                        if let Err(err) = show_on_screen(&chat) {
                            refuse(&refused_name, &toast, &err);
                            return;
                        }
                        give_keys(&chat);
                        match agent.join_back(view) {
                            Ok(_) => {
                                agent.arrive(ViewId::BASE);
                                println!("[chat] {}: its tab joined the base view", view.module());
                            }
                            Err(text) => refuse_text(&refused_name, &toast, &text),
                        }
                    }
                    BreakPlan::Refuse => {
                        let text = format!("{}: the keys are in no chat view", action.name());
                        println!("[prefix] {text}");
                        refuse_said(&refused_name, &toast, &text);
                    }
                },
                // `prefix [`/`prefix PageUp`: the terminal's own read-only copy
                // mode, only while it holds the keys -- elsewhere the same refusal shape as
                // every other misdirected prefix action (`Action::Module`, above).
                Action::CopyMode { up } => {
                    let focused = focused();
                    if terminal::copy_mode_entry(focused.as_ref().map(ModuleId::kind)) {
                        if let Some(pane) = focused.as_ref().and_then(pane_of) {
                            pane.enter_copy_mode(up);
                        }
                    } else {
                        let text = "prefix [ scrolls the terminal";
                        println!("[prefix] {text}");
                        refuse_said(&refused_name, &toast, text);
                    }
                }
            }
        })
    };

    // The container selection's rules around every action, from a key or `eitri.layout.run`
    // (`selection_gate`): an action on one module is refused while a container is selected, saying
    // how to get back to one; one that neither acts on the selection nor refuses it ends it.
    // Then the float's (`float_gate`): with the keys on a float, a verb that needs a split around the
    // module is refused, saying how to sink it.
    let one_module_text = one_module_text(&keymap);
    let no_split_text: Rc<str> = format!(
        "a float has no split: {} sinks it",
        key_text(&keymap, &Action::FloatToggle)
    )
    .into();
    let run_action: Rc<dyn Fn(Action, u32)> = {
        let run_verb = run_verb.clone();
        let module_layout = module_layout.clone();
        let refused_name = top_bar.app_name.clone();
        let toast = toast.clone();
        let refresh_selection = refresh_selection.clone();
        let one_module_text = one_module_text.clone();
        Rc::new(move |action: Action, keycode: u32| {
            let gate = selection_gate(&action, module_layout.borrow().selection());
            if gate == SelectionGate::Refuse {
                println!("[modules] {}: {one_module_text}", action.name());
                refuse_said(&refused_name, &toast, &one_module_text);
                return;
            }
            let keys_on_a_float = {
                let layout = module_layout.borrow();
                layout.is_floating(layout.focus())
            };
            if float_gate(&action, keys_on_a_float) == FloatGate::Refuse {
                println!("[modules] {}: {no_split_text}", action.name());
                refuse_said(&refused_name, &toast, &no_split_text);
                return;
            }
            run_verb(action, keycode);
            if gate == (SelectionGate::Run { reset: true }) {
                clear_selection(&module_layout);
            }
            refresh_selection();
        })
    };
    // `<leader>bd`/`<leader>bo` etc., resolved by the panel's own sequence engine and sent as a
    // `tab_verb` message -- routed through the very code the prefix's `Action::Tab` arm runs, the
    // container selection's rules included: a tab verb ends a selection whichever route sent it.
    agent_panel_handle.on_tab_verb({
        let run_action = run_action.clone();
        let tab_verb_view = tab_verb_view.clone();
        move |view, tab_action| {
            tab_verb_view.set(Some(view));
            run_action(Action::Tab(tab_action), 0);
            // A gate that refused the action never reached `run_tab_action`, which takes it.
            tab_verb_view.set(None);
        }
    });

    // The prefix. Installed after every other window-level controller that exists
    // at startup, so it sees keys first; HINT's, added when a HINT starts, still comes before it.
    {
        let refused_name = top_bar.app_name.clone();
        let place_and_arrive = place_and_arrive.clone();
        let land_by_key = land_by_key.clone();
        let give_keys = give_keys.clone();
        let module_layout = module_layout.clone();
        let agent = agent_panel_handle.clone();
        let toast = toast.clone();
        let number_plan = number_plan.clone();
        let run_action = run_action.clone();
        let on_waiting = on_waiting.clone();
        let refresh_selection = refresh_selection.clone();
        let escape_layout = module_layout.clone();
        let escape_refresh = refresh_selection.clone();
        let pick_workspaces = workspaces.clone();
        let switch_workspace = switch_workspace.clone();
        let workspaces_changed = workspaces_changed.clone();
        let split_new_chat_view = new_chat_view.clone();
        let split_new_terminal = split_new_terminal(
            new_terminal.clone(),
            terminals.clone(),
            project_root.to_path_buf(),
            top_bar.app_name.clone(),
            toast.clone(),
        );
        let prefix = prefix::install(
            &window,
            keymap.clone(),
            module_keys.clone(),
            move |command, keycode| match command {
                // A split key's second half: `split.*` ends a container selection, as its rule says.
                // From a terminal, the terminal's key makes a new one (`split_choice`); a chat's key from
                // a chat view makes a new chat view, holding a new empty tab.
                prefix::PrefixCommand::Place { module, axis, cwd } => {
                    let (choice, target) = {
                        let layout = module_layout.borrow();
                        (
                            eitri_core::layout::split_choice(&layout, &module),
                            layout.focus().clone(),
                        )
                    };
                    match choice {
                        eitri_core::layout::SplitChoice::Existing(id) => {
                            if let Err(err) = place_and_arrive(&id, axis) {
                                refuse(&refused_name, &toast, &err);
                            }
                        }
                        eitri_core::layout::SplitChoice::NewInstance(ModuleKind::Terminal) => {
                            split_new_terminal(target, axis, cwd);
                        }
                        eitri_core::layout::SplitChoice::NewInstance(ModuleKind::Agent) => {
                            if let Err(text) = split_new_chat_view(OpenHow::Beside(target, axis), NewViewTab::Empty) {
                                refuse_text(&refused_name, &toast, &text);
                            }
                        }
                        eitri_core::layout::SplitChoice::NewInstance(kind) => {
                            println!("[modules] split: no new instance of {kind:?} can be made");
                            flash(&refused_name);
                        }
                    }
                    clear_selection(&module_layout);
                    refresh_selection();
                }
                // The digits are already down (the wait's end hid them); the number is looked up in
                // what they showed. A digit with no module there does nothing, as in tmux.
                prefix::PrefixCommand::Number(digit) => {
                    let to = number_plan
                        .borrow()
                        .iter()
                        .find(|(d, _)| *d == digit)
                        .map(|(_, id)| id.clone());
                    match to {
                        // Zoomed, `1` is the zoomed module: it gets the keys and the zoom stays, as
                        // tmux keeps a zoom when the pane picked is the one zoomed. Anywhere else
                        // the keys land as `Ctrl+h/j/k/l` land them.
                        Some(id) => {
                            let zoomed_here = module_layout.borrow().zoomed() == Some(&id);
                            let grabbed = if zoomed_here {
                                let grabbed = give_keys(&id);
                                if let Some(view) = ViewId::of_module(&id).filter(|_| grabbed) {
                                    agent.arrive(view);
                                }
                                grabbed
                            } else {
                                land_by_key(&id)
                            };
                            println!("[prefix] number {digit} -> {id} (grab_focus={grabbed})");
                        }
                        None => println!("[prefix] number {digit}: no module has it"),
                    }
                }
                // The chooser's key, read against the workspaces as they are now.
                prefix::PrefixCommand::Workspace(pick) => {
                    let resolved = {
                        let workspaces = pick_workspaces.borrow();
                        resolve_pick(workspaces.order(), workspaces.current(), pick)
                    };
                    match resolved {
                        Pick::Switch(name) => {
                            if let Err(err) = switch_workspace(&name) {
                                refuse_text(&refused_name, &toast, &err.to_string());
                            }
                        }
                        Pick::New => run_action(Action::WorkspaceNew, keycode),
                        Pick::Delete(name) => {
                            let deleted = pick_workspaces.borrow_mut().delete(&name);
                            match deleted {
                                Ok(()) => {
                                    println!("[workspace] deleted {name}");
                                    workspaces_changed();
                                }
                                Err(err) => refuse_text(&refused_name, &toast, &err.to_string()),
                            }
                        }
                        Pick::Refuse(text) => refuse_text(&refused_name, &toast, &text),
                    }
                }
                prefix::PrefixCommand::Run(action) => run_action(action, keycode),
            },
            move |waiting| on_waiting(waiting),
            // `prefix Esc` resets a container selection; a bare `Esc` is the module's own key.
            move || {
                if clear_selection(&escape_layout) {
                    println!("[modules] the selection is back on the module with the keys");
                }
                escape_refresh();
            },
        );
        let _ = prefix_cell.set(prefix);
    }

    // Install after `prefix::install`: GTK runs a widget's most recently added controller first,
    // so the open close prompt receives keys before the prefix capture controller.
    let close_prompt = close_prompt::ClosePrompt::install(
        &hint_overlay,
        &window,
        &top_bar.widget,
        top_bar.strip.upcast_ref::<gtk4::Widget>(),
    );
    let _ = kill_prompt.set(close_prompt.clone());
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
        let move_focus = move_focus.clone();
        let editor_quitting = editor_quitting.clone();
        feed_pump.listen_pane_switch(ps, move |message| match message {
            pane_switch::PaneMessage::Direction(letter) => match pane_switch::letter_direction(letter) {
                Some(direction) => {
                    move_focus(&ModuleId::editor(), direction);
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

    // --- From a web module (the agent panel, a Lua panel): a capture-phase controller on its host
    // (`install_module_nav`), one per web module here; a web module added later installs its own.
    // The host is the `WebHost` around the `WebView` since 2026-09-29, an ancestor of the focus
    // widget rather than the focus widget itself. That changes nothing here: GTK runs capture-phase
    // controllers on every ancestor of the target, top down, before any controller on the target
    // (`gtk_propagate_event_internal`, gtkmain.c, GTK 4.22.5), and WebKit's second delivery of a key
    // the page did not handle starts at the window again (`pane_switch::LetThrough`).
    // Only the agent host gets an intercept: `Ctrl+j`/`Ctrl+k` claimed for the
    // composer, decided from the panel's own `panel_keys` mirror. A Lua panel has no such mode and
    // passes `None`, so every chord there still goes straight to `move_focus`.
    // A Lua panel's page (a plugin's among them) never gets `Ctrl+h/j/k/l`, even with no module that way:
    // what such a page does with a key must not depend on where its module sits. The agent panel still
    // gets the chord at an edge, as its own keys expect.
    for (id, host) in grid
        .hosts()
        .iter()
        .filter(|(id, _)| matches!(id.kind(), ModuleKind::Agent | ModuleKind::LuaWebview))
    {
        // Each chat view's intercept reads its own page's mode.
        let intercept: Option<Rc<dyn Fn(Direction) -> bool>> =
            ViewId::of_module(id).map(|view| agent_panel_handle.nav_intercept(view));
        let swallow_at_edge = id.kind() == ModuleKind::LuaWebview;
        install_module_nav(id.clone(), host, move_focus.clone(), intercept, swallow_at_edge);
        // No key with Super or Hyper held reaches a web module's page: WebKitGTK does not tell the
        // page, so the agent panel's `a`/`d`/`D` and the bypass `y`, or a plugin page's own keys,
        // could not refuse it there (`panel_super.rs`).
        panel_super::install(host);
    }

    // The shell handles Ctrl+h/j/k/l when the editor has no running nvim, since nothing inside can
    // resolve navigation then. Capture phase runs before the pane's key controller, which swallows
    // every key. While nvim runs, this controller claims nothing and its navigator handles the
    // keys.
    {
        let move_focus = move_focus.clone();
        let pane_for_nav = Rc::downgrade(&pane);
        let controller = EventControllerKey::new();
        controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        controller.connect_key_pressed(move |_controller, key, _keycode, state| {
            let running = pane_for_nav.upgrade().is_some_and(|pane| pane.is_running());
            match editor_start_failure::navigation(key, state, running) {
                Some(direction) => {
                    move_focus(&ModuleId::editor(), direction);
                    glib::Propagation::Stop
                }
                None => glib::Propagation::Proceed,
            }
        });
        pane.widget().add_controller(controller);
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
            if let Some(next) = chrome::step_item(&visible, current, step) {
                top_items[next].grab_focus();
            }
            glib::Propagation::Stop
        });
        top_bar.widget.add_controller(controller);
    }

    // The workspace name in the top bar, clicked or activated from the keyboard: the chooser, as
    // `workspace.choose` opens it. Outside the prefix's own controller, so the new wait is announced
    // here.
    {
        let prefix_cell = prefix_cell.clone();
        let on_waiting = on_waiting.clone();
        top_bar.workspace.connect_clicked(move |_| {
            let Some(prefix) = prefix_cell.get() else { return };
            prefix.borrow_mut().await_workspace();
            let waiting = prefix.borrow().waiting();
            println!("[prefix] armed=true waiting={waiting:?} (the top bar's workspace name)");
            on_waiting(waiting);
        });
    }

    // The rename entry: `Enter` renames the current workspace (a refusal flashes and says why), `Esc`
    // leaves it as it was; either way the keys go back to the module that had them and the entry
    // hides. The keys leaving it any other way -- a click elsewhere -- cancels it too, so it is never
    // left open with no one typing.
    {
        let entry = top_bar.rename.clone();
        let workspaces = workspaces.clone();
        let workspaces_changed = workspaces_changed.clone();
        let return_to_pane = return_to_pane.clone();
        let refused_name = top_bar.app_name.clone();
        let toast = toast.clone();
        let activate_return = return_to_pane.clone();
        entry.connect_activate(move |entry| {
            let to = entry.text().to_string();
            let renamed = workspaces.borrow_mut().rename_current(&to);
            activate_return();
            entry.set_visible(false);
            match renamed {
                Ok(()) => {
                    println!("[workspace] renamed to {to}");
                    workspaces_changed();
                }
                Err(err) => refuse_text(&refused_name, &toast, &err.to_string()),
            }
        });
        let keys = EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        {
            let entry = entry.downgrade();
            let return_to_pane = return_to_pane.clone();
            keys.connect_key_pressed(move |_, key, _, _| {
                if key != Key::Escape {
                    return glib::Propagation::Proceed;
                }
                return_to_pane();
                if let Some(entry) = entry.upgrade() {
                    entry.set_visible(false);
                }
                glib::Propagation::Stop
            });
        }
        entry.add_controller(keys);
        let left = gtk4::EventControllerFocus::new();
        {
            let entry = entry.downgrade();
            left.connect_leave(move |_| {
                if let Some(entry) = entry.upgrade() {
                    entry.set_visible(false);
                }
            });
        }
        entry.add_controller(left);
    }

    // nvim exiting on its own (e.g. `:qa!`) has no window to close by itself -- ask this host's
    // window to close, which in turn drives the connect_close_request handler below (mirrors
    // neovide-editor's own `examples/standalone.rs`), unless nvim was asked to quit: then what it was
    // asked for (`editor_quit::EditorQuitting::nvim_exited`).
    {
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
    {
        let window = window.clone();
        let grid = grid.downgrade();
        let focus_and_arrive = focus_and_arrive.clone();
        let show_on_screen = show_on_screen.clone();
        let editor_start_failure_label = editor_start_failure_label.clone();
        pane.on_start_failed(move |message| {
            editor_start_failure::show(&editor_start_failure_label, message);
            let Some(grid) = grid.upgrade() else { return };
            let focused = pane_focus::focused_module(&window, &grid.hosts());
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

    // Lua `layout.show/hide/focus/split/run/mark` requests are queued while the
    // command or event handler runs and applied after it returns, so layout changes
    // do not happen under a Lua call. Each refusal is logged like a handler error.
    // `run` uses the prefix's runner, which flashes and logs its own refusals.
    let apply_layout_requests: Rc<dyn Fn()> = {
        let lua_engine = lua_engine.clone();
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let focus_module = focus_module.clone();
        let focus_and_arrive = focus_and_arrive.clone();
        let show_on_screen = show_on_screen.clone();
        let place_and_arrive = place_and_arrive.clone();
        let rules = rules.clone();
        let workspaces = workspaces.clone();
        let plugin_panels = plugin_panels.clone();
        let give_keys = give_keys.clone();
        let run_action = run_action.clone();
        let refresh_mark = refresh_mark.clone();
        let refresh_selection = refresh_selection.clone();
        let one_module_text = one_module_text.clone();
        let switch_or_make = switch_or_make.clone();
        let new_terminal = new_terminal.clone();
        let new_chat_view = new_chat_view.clone();
        let project_dir = project_root.to_path_buf();
        Rc::new(move || {
            let requests = lua_engine.layout.borrow_mut().take_requests();
            for request in requests {
                use eitri_core::lua::layout::LayoutRequest;
                println!("[lua] layout: {request:?}");
                // The container selection's rules by the action each request stands for; `run`
                // meets them in `run_action` itself.
                let gate = lua_request_action(&request)
                    .map(|action| selection_gate(&action, module_layout.borrow().selection()));
                if gate == Some(SelectionGate::Refuse) {
                    eprintln!("[lua] layout: {request:?} refused: {one_module_text}");
                    continue;
                }
                // The same three helpers `Ctrl+a` uses, so the two cannot drift apart.
                let result = match &request {
                    // A new terminal, or a new chat view holding a new empty tab, as `instance.new` makes
                    // one: where the call's options say when it gave them, else where the open rules do.
                    // Its refusal is the maker's text, logged here as a workspace switch's is.
                    LayoutRequest::Open(kind, explicit) => {
                        let how = OpenHow::new_instance(module_layout.borrow().focus().clone(), explicit.clone());
                        let made = match kind {
                            ModuleKind::Terminal => new_terminal(how, project_dir.clone()).map(drop),
                            ModuleKind::Agent => new_chat_view(how, NewViewTab::Empty).map(drop),
                            other => Err(format!("no new instance of {other:?} can be made")),
                        };
                        if let Err(text) = made {
                            eprintln!("[lua] layout: {request:?} refused: {text}");
                        }
                        Ok(())
                    }
                    // Its refusal is a workspace's, not a layout's: said here, as the others are below.
                    LayoutRequest::Switch(name) => {
                        if let Err(text) = switch_or_make(name) {
                            eprintln!("[lua] layout: {request:?} refused: {text}");
                        }
                        Ok(())
                    }
                    // A plugin module that starts hidden, which this workspace does not hold yet: its own
                    // rule's `focus` decides whether it takes the keys.
                    LayoutRequest::Show(id)
                        if plugin_panels.starts_hidden(id) && !module_layout.borrow().contains(id) =>
                    {
                        let keys = module_layout.borrow().focus().clone();
                        open_plugin_module(
                            &grid,
                            &rules,
                            &workspaces,
                            &plugin_panels,
                            id,
                            Some(&keys),
                            None,
                            &*give_keys,
                        )
                    }
                    LayoutRequest::Show(id) => show_on_screen(id),
                    LayoutRequest::Hide(id) => grid.hide_module(id, &*focus_module),
                    LayoutRequest::Focus(id) => focus_and_arrive(id),
                    LayoutRequest::Split(id, axis) => place_and_arrive(id, *axis),
                    LayoutRequest::Run(action) => {
                        run_action(action.clone(), 0);
                        Ok(())
                    }
                    LayoutRequest::Float(id, rect) => grid.float_at(id, *rect).map(|_| ()),
                    LayoutRequest::Mark(id) => {
                        // Released before the redraw, which reads the layout.
                        let marked = module_layout.borrow_mut().set_mark(id);
                        refresh_mark();
                        marked
                    }
                };
                if let Err(err) = result {
                    eprintln!("[lua] layout: {request:?} refused: {err}");
                }
                if gate == Some(SelectionGate::Run { reset: true }) {
                    clear_selection(&module_layout);
                }
            }
            refresh_selection();
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
            app.set_accels_for_action(&format!("app.{action_name}"), &[keybinding.as_str()]);
        }
    }

    // The layout's first focus: where the keys were when this project's last arrangement change was
    // written, from
    // its state file (`layout_state`'s module doc); else the editor, or a Lua `main` panel that hid
    // it (`Layout::initial`, `reconcile`). Read BEFORE `present()`: GTK4's `gtk_window_show` moves
    // focus Tab-forward when there is none, and were that ever to land inside a module, `pane_focus`
    // would rewrite the layout's `focus` before this line read it. Today a top-bar item (a tray chip,
    // or `↻`) is the first focusable widget, so it never has.
    let first = module_layout.borrow().focus().clone();
    window.present();
    #[cfg(feature = "probe")]
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
        new_chat_view: new_chat_view.clone(),
        kill_chat_view: {
            let kill_now = kill_now.clone();
            Rc::new(move |id: &ModuleId| {
                if let Err(err) = kill_now(id, None) {
                    println!("[modules] {id}: not killed: {err}");
                }
            })
        },
        chat_views: chat_views.clone(),
    });
    // Call `grab_focus()` after `present()`, since a widget must be shown to take focus.
    // Then call `arrive` as module keys do, so restored chat focus uses the same
    // arrival behavior as other keyboard routes. At launch the panel may still be
    // on its start screen or loading, so arrival may have no visible effect.
    if focus_module(&first) {
        arrive(&first);
    }

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
    lua_engine.emit("shell:ready");
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
        let window = window.clone();
        agent_panel::watch_supervisor(agent_panel_handle.clone(), Rc::new(move || window.present()));
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
        // First, and synchronously: the debounce may still be waiting on an arrangement
        // change. With none unwritten this writes nothing -- a click never does (`layout_state`'s
        // module doc says what is written when).
        layout_saver.save_now();
        // Kills nvim by its pid if it does not exit within the fork's wait (codex p1 #2).
        pane.shutdown();
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
    /// The timer that ends the refusal flash now showing, if one is ([`refuse`]).
    static REFUSAL_ENDS: std::cell::RefCell<Option<glib::SourceId>> = const { std::cell::RefCell::new(None) };
}

/// The top bar's app name shows the prefix indicator's block for [`REFUSAL_FLASH`].
fn flash(app_name: &gtk4::Label) {
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

/// What a plugin's nvim commands do to its module in this window: a show puts it on screen (opening a start-hidden
/// one by its rule the first time) with the keys where they are, a hide sends the keys where any hide sends them.
struct GridFollowWindow {
    module_layout: Rc<RefCell<eitri_core::layout::Layout>>,
    grid: glib::WeakRef<ModuleGrid>,
    show_on_screen: Rc<dyn Fn(&ModuleId) -> Result<(), LayoutError>>,
    focus_module: Rc<dyn Fn(&ModuleId) -> bool>,
}

impl plugin_panels::FollowWindow for GridFollowWindow {
    fn is_shown(&self, module: &ModuleId) -> bool {
        self.module_layout.borrow().is_shown(module)
    }

    fn is_on_screen(&self, module: &ModuleId) -> bool {
        self.module_layout.borrow().is_visible(module)
    }

    fn show(&self, module: &ModuleId) {
        if let Err(err) = (self.show_on_screen)(module) {
            println!("[modules] {module}: the editor asked to show it, refused: {err}");
            return;
        }
        // Showing ends a zoom that hides it, but a tab its container does not draw stays behind; its tab is drawn
        // unless that would hide the module with the keys.
        let brought = {
            let mut layout = self.module_layout.borrow_mut();
            layout.is_tabbed_away(module) && layout.bring_forward(module)
        };
        if brought {
            if let Some(grid) = self.grid.upgrade() {
                grid.apply();
            }
        } else if self.module_layout.borrow().is_tabbed_away(module) {
            println!("[modules] {module}: shown, but its tab stays behind the one with the keys");
        }
    }

    fn hide(&self, module: &ModuleId) {
        let Some(grid) = self.grid.upgrade() else { return };
        if let Err(err) = grid.hide_module(module, &*self.focus_module) {
            println!("[modules] {module}: the editor asked to hide it, refused: {err}");
        }
    }
}

/// [`editor_quit::QuitHost`] over the real window: one GTK call per method, and the only part of the
/// editor's quit flow no test reaches (`editor_quit`'s module doc; the GUI checklist covers it).
struct ShellQuitHost {
    pane: Rc<NeovideEditorPane>,
    module_layout: Rc<RefCell<eitri_core::layout::Layout>>,
    grid: glib::WeakRef<ModuleGrid>,
    show_on_screen: Rc<dyn Fn(&ModuleId) -> Result<(), LayoutError>>,
    focus_module: Rc<dyn Fn(&ModuleId) -> bool>,
    /// `build_ui`'s `give_keys`: a tab not drawn is chosen before the grab.
    give_keys: Rc<dyn Fn(&ModuleId) -> bool>,
    retire_editor: Rc<dyn Fn() -> Result<(), LayoutError>>,
    /// The window's y/n, set once it is installed (after the prefix, so its controller sees keys
    /// first); every call before that finds none.
    prompt: Rc<std::cell::OnceCell<Rc<close_prompt::ClosePrompt>>>,
    agent: agent_panel::AgentPanelHandle,
    /// Every terminal pane the window holds, live or idle; weak, as the window's other holders of
    /// the registry are, so the quit flow keeps no pane alive.
    terminals: std::rc::Weak<RefCell<Terminals<terminal::TerminalPane>>>,
    window: ApplicationWindow,
    app_name: gtk4::Label,
    toast: Rc<toast::Toast>,
}

impl editor_quit::QuitHost for ShellQuitHost {
    fn nvim_running(&self) -> bool {
        self.pane.is_running()
    }
    fn nvim_exited(&self) -> bool {
        self.pane.nvim_exited()
    }
    fn nvim_exit_pending(&self) -> bool {
        self.pane.nvim_exit_pending()
    }
    fn watched_call(&self) -> Option<neovide_editor::CallWatch> {
        self.pane.watched_call()
    }
    fn settled_watch(&self) -> Option<neovide_editor::CallWatch> {
        self.pane.settled_watched_call()
    }
    fn send_quit(&self, lua: &str) -> bool {
        self.pane.exec_lua_watched(lua)
    }
    fn end_nvim(&self) -> bool {
        self.pane.end_nvim()
    }
    fn reveal(&self) -> kill_pane::Reveal {
        kill_pane::reveal_for_prompt(&self.module_layout.borrow(), &ModuleId::editor())
    }
    fn editor_gone(&self) -> bool {
        self.module_layout.borrow().is_gone(&ModuleId::editor())
    }
    fn show_editor(&self) -> Result<(), LayoutError> {
        let editor = ModuleId::editor();
        (self.show_on_screen)(&editor)?;
        // An editor on a tab not drawn is off screen too (`kill_pane::reveal_for_prompt`): nvim's
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
        self.prompt.get().is_some_and(|prompt| prompt.confirmed())
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
        match self.prompt.get() {
            Some(prompt) => prompt.ask_to_close_window(text),
            None => eprintln!("[window] BUG: the close asked {text:?} before the window's y/n existed"),
        }
    }
    fn withdraw_close_confirmation(&self) {
        if let Some(prompt) = self.prompt.get() {
            prompt.withdraw_confirmation();
        }
    }
    fn close_window_confirmed(&self) {
        match self.prompt.get() {
            Some(prompt) => prompt.close_window_confirmed(),
            None => self.window.close(),
        }
    }
    fn close_window(&self) {
        self.window.close();
    }
    fn refuse(&self, text: &str) {
        refuse_said(&self.app_name, &self.toast, text);
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

/// Toast text for an editor that cannot reopen because `Reopen::Never` remains in effect for
/// this window. Returns `Some` only for `LayoutError::Gone` on the editor, so another module
/// marked gone cannot inherit an editor-specific message.
fn gone_editor_toast_text(err: &LayoutError) -> Option<String> {
    match err {
        LayoutError::Gone(id) if *id == ModuleId::editor() => Some(editor_quit::RETIRED_EDITOR_TEXT.to_string()),
        _ => None,
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

/// What a container selection does to an action, before and after it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionGate {
    /// The action acts on one module, and a container is selected: it does not run.
    Refuse,
    /// It runs; with `reset`, the selection goes back to the module with the keys afterwards.
    Run { reset: bool },
}

/// The selection's rule for `action` (`Action::selection_rule`) against what is selected now.
fn selection_gate(action: &Action, selection: Selection) -> SelectionGate {
    match (action.selection_rule(), selection) {
        (SelectionRule::Refuses, Selection::Con(_)) => SelectionGate::Refuse,
        (SelectionRule::Resets, _) => SelectionGate::Run { reset: true },
        _ => SelectionGate::Run { reset: false },
    }
}

/// The key action an `eitri.layout.*` request stands for, so the selection treats a Lua call as it
/// treats the key: `hide` is `module.hide`, `mark` is `mark.toggle`, `show` and `focus` a module key,
/// `split` a split key, `float` `float.toggle`. `None` for `run`, which goes through `run_action` and
/// its rules itself.
fn lua_request_action(request: &eitri_core::lua::layout::LayoutRequest) -> Option<Action> {
    use eitri_core::lua::layout::LayoutRequest;
    match request {
        LayoutRequest::Open(kind, _) => Some(Action::InstanceNew { kind: *kind }),
        LayoutRequest::Hide(_) => Some(Action::ModuleHide),
        LayoutRequest::Mark(_) => Some(Action::MarkToggle),
        LayoutRequest::Show(id) | LayoutRequest::Focus(id) => Some(Action::Module(id.clone())),
        LayoutRequest::Split(_, axis) => Some(Action::Split {
            axis: *axis,
            cwd: Default::default(),
        }),
        LayoutRequest::Float(..) => Some(Action::FloatToggle),
        LayoutRequest::Run(_) | LayoutRequest::Switch(_) => None,
    }
}

/// What a refusal under a container selection says: how to get back to one module, as the keymap
/// binds `focus.child` (`acts on one module: C-b M-d`), or its name when no key reaches it.
fn one_module_text(keymap: &eitri_core::keymap::Keymap) -> Rc<str> {
    format!("acts on one module: {}", key_text(keymap, &Action::FocusChild)).into()
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

/// What the keys being on a float does to an action, before it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FloatGate {
    Run,
    /// The action needs a split around the module, which a float has not got.
    Refuse,
}

/// The float's rule for `action` (`Action::on_a_float`) when the module with the keys floats: a verb
/// on the tiled tree runs and leaves the floats alone, a verb on the float acts on it, and a verb that
/// needs a split around the module is refused.
fn float_gate(action: &Action, keys_on_a_float: bool) -> FloatGate {
    match (action.on_a_float(), keys_on_a_float) {
        (FloatRule::Refuses, true) => FloatGate::Refuse,
        (FloatRule::Tiles | FloatRule::Acts | FloatRule::Refuses, _) => FloatGate::Run,
    }
}

/// The refusal of a card answer from a covered chat: `agent is under terminal 2: C-b M-- from terminal 2
/// stows it` for a scratch float, which the scratchpad's key puts away; `... C-b M-S-Space from terminal
/// sinks it` for another, which only sinking clears. Both keys act on the float only while it holds
/// the keys: pressed in the chat, where an answer comes from, the scratchpad's key first moves the keys
/// into a shown scratch float, and `float.toggle` would float the chat itself. So the text says where
/// to press it. `key` is how the keymap reaches that action ([`key_text`]).
fn covered_text(chat: &str, float: &str, key: &str, scratch: bool) -> String {
    let verb = if scratch { "stows" } else { "sinks" };
    format!("{chat} is under {float}: {key} from {float} {verb} it")
}

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

/// `prefix a`'s instance: the chat view with the keys, so the key still hides the one you are in; else the
/// view holding the oldest waiting card, when the layout holds it; else the most recently used chat view, then
/// the base view.
fn chat_target(layout: &Layout, oldest: Option<ViewId>) -> ModuleId {
    if layout.focus().kind() == ModuleKind::Agent {
        return layout.focus().clone();
    }
    if let Some(v) = oldest.filter(|v| layout.contains(&v.module())) {
        return v.module();
    }
    eitri_core::layout::module_target(layout, &ModuleId::agent())
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

/// A split key from a terminal: a new terminal beside `target`, the module that held the keys at the
/// keypress, along `axis`. With `cwd = "pane"` and `target` a terminal, the new shell starts where
/// `target`'s shell is now: `/proc/<pid>/cwd` is read on a worker thread (a slow or hung filesystem
/// must never stall the window), and the new terminal is made when the answer comes back -- beside
/// the target captured here, or refused if it has left the layout in the meantime. A shell that has
/// ended, or a directory that cannot be read, gives the project root and one log line
/// (`terminal::start_dir`). Otherwise the new shell starts in the project root.
fn split_new_terminal(
    new_terminal: Rc<dyn Fn(OpenHow, PathBuf) -> Result<ModuleId, String>>,
    terminals: Rc<RefCell<Terminals<terminal::TerminalPane>>>,
    root: PathBuf,
    app_name: gtk4::Label,
    toast: Rc<toast::Toast>,
) -> impl Fn(ModuleId, Axis, SplitCwd) {
    move |target, axis, cwd| {
        let from_pane = cwd == SplitCwd::Pane && target.kind() == ModuleKind::Terminal;
        let start_of = {
            let terminals = terminals.clone();
            move |target: &ModuleId| {
                terminals
                    .borrow()
                    .get(target)
                    .map(|pane| ShellStart::of(pane.generation(), pane.shell_pid()))
            }
        };
        let captured = start_of(&target);
        let pid = captured.and_then(|start| start.pid);
        let make = {
            let new_terminal = new_terminal.clone();
            let target = target.clone();
            let app_name = app_name.clone();
            let toast = toast.clone();
            move |dir: PathBuf| {
                if let Err(text) = new_terminal(OpenHow::Beside(target.clone(), axis), dir) {
                    refuse_text(&app_name, &toast, &text);
                }
            }
        };
        if !from_pane {
            make(root.clone());
            return;
        }
        let root = root.clone();
        let app_name = app_name.clone();
        let toast = toast.clone();
        glib::spawn_future_local(async move {
            let read = match pid {
                // The stat that tells a removed directory runs here too, off the GTK thread.
                Some(pid) => gtk4::gio::spawn_blocking(move || {
                    terminal::usable_dir(std::fs::read_link(format!("/proc/{pid}/cwd")))
                })
                .await
                .unwrap_or_else(|_| Err(std::io::Error::other("the reading thread panicked"))),
                None => Err(std::io::Error::other("its shell is not running")),
            };
            // The number alone does not name the terminal the key was pressed in: it may have been
            // killed while the directory was read, and its number given to a new terminal since.
            if !captured.is_some_and(|captured| captured.still(start_of(&target))) {
                refuse_text(
                    &app_name,
                    &toast,
                    &format!("{target} closed while its directory was read; no terminal was opened"),
                );
                return;
            }
            let (dir, note) = terminal::start_dir(read, &root);
            if let Some(note) = note {
                println!("[terminal] {note}");
            }
            make(dir);
        });
    }
}

/// Whether `id` may be placed by its name (`Ctrl+a \`/`"` + a key, `eitri.layout.split`), given
/// whether its pool holds a live pane or page for it. A numbered terminal that was killed keeps its
/// host in the grid, for the next terminal of that number, but its pane is idle: placed again by name
/// it would be a terminal with no shell, which the start hook (live panes only) never starts. A
/// removed chat view is the same: its host stays, but its page is idle and holds no tab, so placed by
/// name it would take the keys and show nothing, and every message it sent would be dropped. Only a
/// new instance brings either back. The base chat and every other module are the grid's to judge.
fn placeable_by_id(id: &ModuleId, live: bool) -> Result<(), LayoutError> {
    let pooled = match id.kind() {
        ModuleKind::Terminal => true,
        ModuleKind::Agent => id.number() >= 2,
        _ => false,
    };
    if pooled && !live {
        return Err(LayoutError::NotInTree(id.clone()));
    }
    Ok(())
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
fn refuse_text(app_name: &gtk4::Label, toast: &Rc<toast::Toast>, text: &str) {
    println!("[modules] refused: {text}");
    refuse_said(app_name, toast, text);
}

/// A worded refusal whose caller has printed its own stdout line, under its own prefix: the app name
/// flashes and the toast says `text`. A waiting card's toast stays up instead
/// ([`toast::Toast::show_refusal`]); the flash and the stdout line still say a key was refused.
fn refuse_said(app_name: &gtk4::Label, toast: &Rc<toast::Toast>, text: &str) {
    flash(app_name);
    toast.show_refusal(text);
}

/// Whether what the prefix waits for can be answered only by reading the strip in the top bar: the
/// workspace chooser, whose strip is the list its digits pick from. The other waits' strips are
/// reminders of fixed keys.
fn wait_needs_the_top_bar(waiting: prefix::Waiting) -> bool {
    matches!(waiting, prefix::Waiting::Workspace { .. })
}

/// The chat view the keys are in, from the layout's focus (the module that last held them); the base
/// view's when that is no chat view.
fn keys_chat_view(layout: &Layout) -> ViewId {
    ViewId::of_module(layout.focus()).unwrap_or(ViewId::BASE)
}

/// What the chat view a module shows owes the user, for its chip and its strip entry; nothing for any
/// other module.
fn view_attention(agent: &agent_panel::AgentPanelHandle, id: &ModuleId) -> eitri_core::attention::Attention {
    ViewId::of_module(id)
        .map(|view| agent.attention_of(view))
        .unwrap_or_default()
}

/// What a request for a new chat view says where no `WebView` can be built in this process.
const CHAT_VIEWS_NEED_WEBKIT: &str = "chat views need WebKit, which cannot start here";

/// What `workspace.last` says when no workspace was left, or the one left has been deleted since.
const NO_LAST_WORKSPACE: &str = "no workspace to go back to";

/// What a key in the workspace chooser comes to, against the window's workspaces as they are.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pick {
    Switch(String),
    New,
    Delete(String),
    /// Nothing happens; the text says why.
    Refuse(String),
}

/// The chooser's key against `order` (the window's workspaces, in order) and `current`: a digit past
/// the order reaches no workspace, and `main` and the current workspace are never deleted.
fn resolve_pick(order: &[String], current: &str, pick: WorkspacePick) -> Pick {
    let named = |index: usize| {
        order.get(index).cloned().ok_or_else(|| {
            let digit = prefix_strip::workspace_digit(index).map_or_else(|| (index + 1).to_string(), String::from);
            format!("no workspace {digit}")
        })
    };
    match pick {
        WorkspacePick::New => Pick::New,
        WorkspacePick::Switch(index) => match named(index) {
            Ok(name) => Pick::Switch(name),
            Err(text) => Pick::Refuse(text),
        },
        WorkspacePick::Delete(index) => match named(index) {
            Ok(name) if name == MAIN => Pick::Refuse(WorkspaceError::Main.to_string()),
            Ok(name) if name == current => Pick::Refuse(WorkspaceError::Current(name).to_string()),
            Ok(name) => Pick::Delete(name),
            Err(text) => Pick::Refuse(text),
        },
    }
}

/// The layout a workspace made by name at runtime starts from: `init.lua`'s definition of that name,
/// reconciled as the window's startup reconciles it and brought to the window's modules (`fit_to_with`:
/// an instance the window lacks is left out, a module the definition does not place is hidden at its
/// declared place in `lua`, else below the editor), else a copy of `live`, the current arrangement. A
/// `deferred` module the definition names is kept, as the startup keeps it. The notes say what was left
/// out or added.
fn new_workspace_layout(
    template: Option<Result<eitri_core::lua::layout::LuaTree, String>>,
    live: &Layout,
    lua: &[ModuleDecl],
    deferred: &[ModuleId],
) -> Result<(Layout, Vec<String>), String> {
    let Some(template) = template else {
        return Ok((live.fresh_copy(), Vec::new()));
    };
    let tree = template?;
    let reconciled = eitri_core::layout::reconcile_default_floating_with(tree.tiled, tree.floats, lua, deferred)
        .map_err(|err| format!("eitri.layout.workspace: {err}"))?;
    let mut layout = reconciled.layout;
    let mut notes = reconciled.notes;
    notes.extend(eitri_core::layout::fit_to_with(&mut layout, &live.modules(), lua));
    Ok((layout, notes))
}

/// The id a new instance of `kind` (a terminal, a chat view) takes: the lowest number from 2 to 9 that
/// no workspace's layout holds and that no data kept for a later build names (that build may still
/// place an instance of that number, and two modules must never share a name). `None` when every
/// number is taken.
fn new_instance_id(
    kind: ModuleKind,
    layouts: &[&eitri_core::layout::Layout],
    reserved: &[ModuleId],
) -> Option<ModuleId> {
    let taken: Vec<ModuleId> = layouts
        .iter()
        .flat_map(|layout| layout.modules())
        .chain(reserved.iter().cloned())
        .collect();
    ModuleId::lowest_free(kind, &taken)
}

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

/// What a new chat view shows first: a new empty tab, or the base view's active tab brought out of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NewViewTab {
    Empty,
    Break,
}

/// How a new terminal or chat view is put on screen.
#[derive(Debug, Clone, PartialEq)]
enum OpenHow {
    /// A split key: beside this module along this axis. The user said where, so no open rule is read.
    Beside(ModuleId, Axis),
    /// Where the open rules say for `point`, `keys` being the module with the keys; `explicit` is a
    /// call's own options (`eitri.layout.open(kind, opts)`), which decide in place of the rules.
    Rule {
        point: OpenPoint,
        keys: ModuleId,
        explicit: Option<OpenRule>,
    },
}

impl OpenHow {
    /// `instance.new`'s and `eitri.layout.open`'s way: a new instance, the rules deciding unless the
    /// call gave its own.
    fn new_instance(keys: ModuleId, explicit: Option<OpenRule>) -> OpenHow {
        OpenHow::Rule {
            point: OpenPoint::NewInstance,
            keys,
            explicit,
        }
    }

    /// What the log says about where it went.
    fn describe(&self, opened: &Opened) -> String {
        let place = match self {
            OpenHow::Beside(beside, axis) => format!("beside {beside} ({axis:?})"),
            OpenHow::Rule { point, keys, explicit } => format!(
                "where {} says ({point:?}, the keys in {keys})",
                if explicit.is_some() {
                    "the call"
                } else {
                    "its open rule"
                }
            ),
        };
        if opened.stowed {
            format!("{place}, stowed in the scratchpad")
        } else if opened.takes_keys {
            place
        } else {
            format!("{place}, the keys left where they were")
        }
    }
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

/// The rule a runtime open follows: `explicit` (an `eitri.layout.open` with options) wins, else the rules'.
fn rule_for(rules: &OpenRules, id: &ModuleId, point: OpenPoint, explicit: Option<&OpenRule>) -> OpenRule {
    match explicit {
        Some(rule) => rule.clone(),
        None => rules.resolve(id, point, None).0,
    }
}

/// Puts the new instance `id`, whose host the grid holds, on screen as `how` says, and gives it the keys
/// through `focus` when it takes them: always after a split key, as its open rule says otherwise
/// (`ModuleGrid::open_module`). The rule's notes are logged.
fn open_instance(
    grid: &ModuleGrid,
    rules: &OpenRules,
    id: &ModuleId,
    how: &OpenHow,
    focus: &dyn Fn(&ModuleId) -> bool,
) -> Result<Opened, LayoutError> {
    match how {
        OpenHow::Beside(beside, axis) => {
            grid.place_module(id, beside, *axis)?;
            focus(id);
            Ok(Opened {
                takes_keys: true,
                stowed: false,
                notes: Vec::new(),
            })
        }
        OpenHow::Rule { point, keys, explicit } => {
            let rule = rule_for(rules, id, *point, explicit.as_ref());
            let opened = grid.open_module(id, &rule, Some(keys), focus)?;
            for note in &opened.notes {
                println!("[layout] {note}");
            }
            Ok(opened)
        }
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
/// itself: said on stdout, and the top bar's app name shows the prefix indicator's block for
/// [`REFUSAL_FLASH`]. Its own class, so a prefix armed meanwhile keeps its indicator. A
/// second refusal inside the flash restarts it rather than being cut short by the first one's timer
///. One window per process (`NON_UNIQUE`), so one timer per thread.
/// The toast says why ([`refusal_text`]), so a refused key is not a silent blink.
fn refuse(app_name: &gtk4::Label, toast: &Rc<toast::Toast>, err: &LayoutError) {
    println!("[modules] refused: {err}");
    refuse_said(app_name, toast, &refusal_text(err));
}

/// What the toast says for a layout refusal: a killed editor's own sentence, since that refusal
/// recurs for the rest of the window's life and must say how to get the editor back; the layout's
/// own words for every other one.
fn refusal_text(err: &LayoutError) -> String {
    gone_editor_toast_text(err).unwrap_or_else(|| err.to_string())
}

/// Installs Ctrl+h/j/k/l navigation on a web module host, both at startup and for modules added
/// later. The navigator inside Neovim never sees these keys, because Neovim is not the focused
/// widget here. Capture phase runs before the `WebView` can consume the chord; bubble phase would
/// be too late. A successful focus move consumes the chord.
///
/// `intercept` runs after the `LetThrough` second-delivery check and before `move_focus`. A
/// claimed chord reaches neither `move_focus` nor the page. Only the agent host passes an
/// interceptor; Lua panels pass `None`.
///
/// With `swallow_at_edge`, a chord with no module in that direction stops here rather than
/// reaching the page, so WebKit has no event to return to GTK.
fn install_module_nav(
    id: ModuleId,
    host: &gtk4::Widget,
    move_focus: Rc<dyn Fn(&ModuleId, Direction) -> bool>,
    intercept: Option<Rc<dyn Fn(Direction) -> bool>>,
    swallow_at_edge: bool,
) {
    // A chord this controller lets through to the page comes back through it once more if the page
    // does not handle it: WebKit puts the same event back on the queue for GTK's own bindings
    // (`pane_switch::LetThrough`). The second delivery is not a second press.
    let let_through = RefCell::new(pane_switch::LetThrough::new());
    let controller = EventControllerKey::new();
    controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
    controller.connect_key_pressed(move |controller, key, _keycode, state| {
        let Some(direction) = pane_switch::nav_direction(key, state) else {
            return glib::Propagation::Proceed;
        };
        let event = controller.current_event().map(pane_switch::SameEvent);
        if event
            .as_ref()
            .is_some_and(|event| let_through.borrow_mut().is_second_delivery(event))
        {
            return glib::Propagation::Proceed;
        }
        if intercept.as_ref().is_some_and(|intercept| intercept(direction)) {
            return glib::Propagation::Stop;
        }
        match pane_switch::nav_outcome(move_focus(&id, direction), swallow_at_edge) {
            pane_switch::NavOutcome::Stop => glib::Propagation::Stop,
            pane_switch::NavOutcome::LetThrough => {
                if let Some(event) = event {
                    let_through.borrow_mut().let_through(event);
                }
                glib::Propagation::Proceed
            }
        }
    });
    host.add_controller(controller);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The chooser's keys against three workspaces, `study` current: a digit names a workspace by its
    /// place, a digit past the last one (here `0`, the tenth) reaches none, and neither `main` nor the
    /// current workspace is deleted.
    #[test]
    fn workspace_picks_resolve_against_the_order() {
        let order: Vec<String> = ["main", "study", "review"].iter().map(|n| n.to_string()).collect();
        let pick = |p| resolve_pick(&order, "study", p);
        assert_eq!(pick(WorkspacePick::Switch(0)), Pick::Switch("main".into()));
        assert_eq!(pick(WorkspacePick::Switch(2)), Pick::Switch("review".into()));
        assert_eq!(pick(WorkspacePick::Switch(9)), Pick::Refuse("no workspace 0".into()));
        assert_eq!(pick(WorkspacePick::Switch(3)), Pick::Refuse("no workspace 4".into()));
        assert_eq!(pick(WorkspacePick::New), Pick::New);
        assert_eq!(pick(WorkspacePick::Delete(2)), Pick::Delete("review".into()));
        assert_eq!(
            pick(WorkspacePick::Delete(0)),
            Pick::Refuse(WorkspaceError::Main.to_string())
        );
        assert_eq!(
            pick(WorkspacePick::Delete(1)),
            Pick::Refuse(WorkspaceError::Current("study".into()).to_string())
        );
        assert_eq!(pick(WorkspacePick::Delete(5)), Pick::Refuse("no workspace 6".into()));
    }

    /// In immersive mode the top bar is hidden; the chooser's strip is the one strip whose wait cannot
    /// be answered without reading it, so it alone brings the bar out.
    #[test]
    fn only_the_workspace_chooser_needs_the_top_bar_shown() {
        use prefix::Waiting;
        assert!(wait_needs_the_top_bar(Waiting::Workspace { deleting: false }));
        assert!(wait_needs_the_top_bar(Waiting::Workspace { deleting: true }));
        for waiting in [
            Waiting::No,
            Waiting::Command,
            Waiting::Number,
            Waiting::Module(Axis::Row),
        ] {
            assert!(!wait_needs_the_top_bar(waiting), "{waiting:?}");
        }
    }

    /// A workspace made by name with no definition in `init.lua` is a copy of the current arrangement;
    /// one with a definition is that definition, holding every module of the window and none it lacks.
    #[test]
    fn a_workspace_made_by_name_is_its_definition_fitted_to_the_window_else_a_copy() {
        let live = crate::terminal::initial_layout(&[]).unwrap();
        let (copy, notes) = new_workspace_layout(None, &live, &[], &[]).unwrap();
        assert_eq!(copy.root(), live.root(), "a copy keeps the arrangement");
        assert!(notes.is_empty());

        let two = ModuleId::parse("terminal#2").unwrap();
        let template = eitri_core::lua::layout::LuaTree {
            tiled: eitri_core::layout::Node::split(
                Axis::Row,
                0.5,
                eitri_core::layout::Node::Leaf(ModuleId::editor()),
                eitri_core::layout::Node::split(
                    Axis::Column,
                    0.5,
                    eitri_core::layout::Node::Leaf(ModuleId::agent()),
                    eitri_core::layout::Node::Leaf(two.clone()),
                ),
            ),
            floats: Vec::new(),
        };
        let (made, notes) = new_workspace_layout(Some(Ok(template)), &live, &[], &[]).unwrap();
        assert!(
            !made.contains(&two),
            "an instance the window lacks is left out: {notes:?}"
        );
        for id in live.modules() {
            assert!(made.contains(&id), "{id} is in the made workspace");
        }
        assert!(made.is_visible(&ModuleId::editor()));
        assert_eq!(
            new_workspace_layout(Some(Err("bad tree".into())), &live, &[], &[]).map(|_| ()),
            Err("bad tree".to_string())
        );
    }

    /// A definition that names a start-hidden plugin module keeps it, as the startup keeps one a template
    /// names; without the window's list of them the leaf is left out as a module the window lacks.
    #[test]
    fn a_workspace_made_by_name_keeps_a_deferred_module_its_definition_names() {
        let live = crate::terminal::initial_layout(&[]).unwrap();
        let preview = ModuleId::lua("markdown");
        let template = || eitri_core::lua::layout::LuaTree {
            tiled: eitri_core::layout::Node::split(
                Axis::Row,
                0.6,
                eitri_core::layout::Node::split(
                    Axis::Row,
                    0.5,
                    eitri_core::layout::Node::Leaf(ModuleId::editor()),
                    eitri_core::layout::Node::Leaf(ModuleId::agent()),
                ),
                eitri_core::layout::Node::Leaf(preview.clone()),
            ),
            floats: Vec::new(),
        };
        let (made, notes) = new_workspace_layout(Some(Ok(template())), &live, &[], &[preview.clone()]).unwrap();
        assert!(made.is_shown(&preview), "{notes:?}");
        let (made, notes) = new_workspace_layout(Some(Ok(template())), &live, &[], &[]).unwrap();
        assert!(!made.contains(&preview));
        assert!(notes.iter().any(|n| n.contains("left out 'lua:markdown'")), "{notes:?}");
    }

    /// A killed numbered terminal, whose host the grid keeps, is not placed again by name; a live one
    /// and every other kind are left to the grid.
    #[test]
    fn a_killed_instance_is_not_placed_by_name() {
        let id = |text: &str| ModuleId::parse(text).unwrap();
        assert_eq!(
            placeable_by_id(&id("terminal#2"), false),
            Err(LayoutError::NotInTree(id("terminal#2")))
        );
        assert_eq!(placeable_by_id(&id("terminal#2"), true), Ok(()));
        assert_eq!(placeable_by_id(&id("terminal"), true), Ok(()));
        assert_eq!(placeable_by_id(&id("agent"), false), Ok(()));
        assert_eq!(placeable_by_id(&id("editor"), false), Ok(()));
        assert_eq!(
            placeable_by_id(&id("agent#2"), false),
            Err(LayoutError::NotInTree(id("agent#2"))),
            "a removed chat view's page is idle"
        );
        assert_eq!(placeable_by_id(&id("agent#2"), true), Ok(()));
    }

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

    /// A new terminal takes the lowest free number, skipping one a later build's kept data names
    /// (it may place that terminal itself), and is refused once every number is taken.
    #[test]
    fn a_new_instance_skips_a_reserved_number() {
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
        let layout = below(&["terminal", "terminal#2"]);
        assert_eq!(
            new_instance_id(ModuleKind::Terminal, &[&layout], &[]),
            Some(id("terminal#3"))
        );
        assert_eq!(
            new_instance_id(ModuleKind::Terminal, &[&layout], &[id("terminal#3")]),
            Some(id("terminal#4")),
            "a reserved number is passed over"
        );
        let other = below(&["terminal", "terminal#4"]);
        assert_eq!(
            new_instance_id(ModuleKind::Terminal, &[&layout, &other], &[id("terminal#3")]),
            Some(id("terminal#5")),
            "a number another workspace holds is taken too"
        );
        let full: Vec<String> = (2..=9).map(|n| format!("terminal#{n}")).collect();
        let full: Vec<&str> = std::iter::once("terminal")
            .chain(full.iter().map(String::as_str))
            .collect();
        assert_eq!(
            new_instance_id(ModuleKind::Terminal, &[&below(&full)], &[]),
            None,
            "at most 9 terminals"
        );
    }

    /// A call's own options decide where its one module opens; without them a rule for the module wins
    /// over its kind's, and with neither the point's built-in default: right of the keys for a new
    /// instance, below the editor for a terminal back after a kill.
    #[test]
    fn an_explicit_open_beats_the_rules_and_the_rules_beat_the_built_in() {
        use eitri_core::layout::rules::{parse_at, At, RuleTarget};
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let t3 = ModuleId::parse("terminal#3").unwrap();
        let at = |name: &str| OpenRule {
            at: parse_at(name),
            ..OpenRule::default()
        };
        let none = OpenRules::default();
        assert_eq!(
            rule_for(&none, &t2, OpenPoint::NewInstance, None).at,
            Some(At::Beside(Direction::Right))
        );
        assert_eq!(
            rule_for(&none, &ModuleId::parse("agent#2").unwrap(), OpenPoint::Break, None).at,
            Some(At::Beside(Direction::Right))
        );
        assert_eq!(
            rule_for(&none, &ModuleId::terminal(), OpenPoint::Reopen, None).at,
            Some(At::Placement(Placement::BelowEditor))
        );
        let mut rules = OpenRules::default();
        rules.add(RuleTarget::parse("terminal").unwrap(), at("below")).unwrap();
        rules.add(RuleTarget::parse("terminal#3").unwrap(), at("tab")).unwrap();
        assert_eq!(
            rule_for(&rules, &t2, OpenPoint::NewInstance, None),
            at("below"),
            "the kind's rule"
        );
        assert_eq!(
            rule_for(&rules, &t3, OpenPoint::NewInstance, None),
            at("tab"),
            "the module's own"
        );
        let explicit = OpenRule {
            focus: false,
            ..at("left")
        };
        assert_eq!(rule_for(&rules, &t3, OpenPoint::NewInstance, Some(&explicit)), explicit);
        assert_eq!(rule_for(&none, &t2, OpenPoint::NewInstance, Some(&explicit)), explicit);
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

    /// A new chat view takes the lowest `agent#N` from 2 that no workspace holds and no kept data
    /// reserves, as a new terminal does, and is refused once all nine are taken.
    #[test]
    fn a_new_chat_view_takes_the_lowest_free_number_not_reserved() {
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
        assert_eq!(
            new_instance_id(ModuleKind::Agent, &[&current, &other], &[id("agent#3")]),
            Some(id("agent#4")),
            "agent#2 is held in another workspace and agent#3 is reserved"
        );
        assert_eq!(
            new_instance_id(ModuleKind::Agent, &[&current], &[]),
            Some(id("agent#2")),
            "the base view is not a number a new view can take"
        );
        assert_eq!(
            new_instance_id(ModuleKind::Agent, &[&current, &other], &[id("terminal#3")]),
            Some(id("agent#3")),
            "a terminal's number reserves no chat view"
        );
        let full: Vec<String> = (2..=9).map(|n| format!("agent#{n}")).collect();
        let full: Vec<&str> = std::iter::once("terminal")
            .chain(full.iter().map(String::as_str))
            .collect();
        assert_eq!(
            new_instance_id(ModuleKind::Agent, &[&below(&full)], &[]),
            None,
            "at most 9 chat views"
        );
        let both = below(&["terminal", "agent#3", "agent#2"]);
        assert_eq!(
            extra_chat_views(&[&both, &other, &current]),
            vec![id("agent#2"), id("agent#3")],
            "the extra views every layout holds, once each and never the base view"
        );
        assert!(extra_chat_views(&[&current]).is_empty());
    }

    /// `eitri.layout.hide` and `mark` act on one module, so a selected container refuses them as it
    /// refuses `module.hide` and `mark.toggle`; `show`, `focus` and `split` end the selection as a
    /// module key and a split key do; with the module selected, everything runs.
    #[test]
    fn a_lua_hide_is_refused_while_a_container_is_selected() {
        use eitri_core::layout::ConId;
        use eitri_core::lua::layout::LayoutRequest;
        let con = Selection::Con(ConId::fresh());
        let gate = |request: LayoutRequest, selection: Selection| {
            lua_request_action(&request).map(|action| selection_gate(&action, selection))
        };
        let t = ModuleId::terminal();
        assert_eq!(gate(LayoutRequest::Hide(t.clone()), con), Some(SelectionGate::Refuse));
        assert_eq!(gate(LayoutRequest::Mark(t.clone()), con), Some(SelectionGate::Refuse));
        for request in [
            LayoutRequest::Show(t.clone()),
            LayoutRequest::Focus(t.clone()),
            LayoutRequest::Split(t.clone(), Axis::Row),
        ] {
            assert_eq!(
                gate(request.clone(), con),
                Some(SelectionGate::Run { reset: true }),
                "{request:?}"
            );
        }
        assert_eq!(
            gate(LayoutRequest::Float(t.clone(), None), con),
            Some(SelectionGate::Refuse),
            "eitri.layout.float is float.toggle's"
        );
        assert_eq!(
            gate(LayoutRequest::Run(Action::Zoom), con),
            None,
            "run meets the rules itself"
        );
        assert_eq!(
            gate(LayoutRequest::Hide(t.clone()), Selection::Module),
            Some(SelectionGate::Run { reset: false })
        );
        assert_eq!(
            gate(LayoutRequest::Mark(t), Selection::Module),
            Some(SelectionGate::Run { reset: false })
        );
        // The keys' own path: a verb that takes the selection keeps it.
        assert_eq!(
            selection_gate(&Action::ContainerTabbed, con),
            SelectionGate::Run { reset: false }
        );
        assert_eq!(selection_gate(&Action::ModuleKill, con), SelectionGate::Refuse);
    }

    /// With the keys on a float, a verb on the tiled tree and a verb on the float run, and a verb that
    /// needs a split around the module is refused; with the keys on a tile, everything runs.
    #[test]
    fn float_gate_runs_tiles_and_acts_and_refuses_the_rest() {
        for (action, rule) in [
            (Action::Tiled, FloatRule::Tiles),
            (Action::Rotate { down: true }, FloatRule::Tiles),
            (Action::Move(Direction::Left), FloatRule::Acts),
            (Action::Zoom, FloatRule::Acts),
            (Action::FloatToggle, FloatRule::Acts),
            (Action::Spread, FloatRule::Refuses),
            (Action::ContainerTabbed, FloatRule::Refuses),
            (Action::Swap(SwapTarget::Next), FloatRule::Refuses),
            (Action::FocusParent, FloatRule::Refuses),
        ] {
            assert_eq!(action.on_a_float(), rule, "{action:?}");
            let on_a_float = if rule == FloatRule::Refuses {
                FloatGate::Refuse
            } else {
                FloatGate::Run
            };
            assert_eq!(
                float_gate(&action, true),
                on_a_float,
                "{action:?} with the keys on a float"
            );
            assert_eq!(
                float_gate(&action, false),
                FloatGate::Run,
                "{action:?} with the keys on a tile"
            );
        }
    }

    /// The covered refusal names the chat, the topmost float over it, the key that clears it -- the
    /// scratchpad's for a scratch float, `float.toggle`'s for another, or the action's name when
    /// nothing binds it -- and that it is pressed from the float.
    #[test]
    fn the_covered_refusal_names_the_float_and_the_key_that_clears_it() {
        assert_eq!(
            covered_text("agent", "terminal 2", "C-b M--", true),
            "agent is under terminal 2: C-b M-- from terminal 2 stows it"
        );
        assert_eq!(
            covered_text("agent", "terminal", "C-b M-S-Space", false),
            "agent is under terminal: C-b M-S-Space from terminal sinks it"
        );
        let keymap = eitri_core::keymap::Keymap::defaults();
        let stow = key_text(&keymap, &Action::ScratchpadShow { module: None });
        match keymap.keys_for(&Action::ScratchpadShow { module: None }).first() {
            Some(key) => assert_eq!(stow, format!("{} {}", keymap.prefix().human(), key.human())),
            None => assert_eq!(stow, "scratchpad.show"),
        }
        let sink = key_text(&keymap, &Action::FloatToggle);
        match keymap.keys_for(&Action::FloatToggle).first() {
            Some(key) => assert_eq!(sink, format!("{} {}", keymap.prefix().human(), key.human())),
            None => assert_eq!(sink, "float.toggle"),
        }
        assert_eq!(
            key_text(&keymap, &Action::WorkspaceNew),
            "workspace.new",
            "an unbound action is named"
        );
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

    /// The covered refusal names the view the answer came from, not the base chat.
    #[test]
    fn the_covered_refusal_names_the_view_and_the_float() {
        let title = tray::module_title(&ModuleId::parse("agent#2").unwrap(), &[]);
        assert_eq!(
            covered_text(&title, "terminal 2", "C-b M--", true),
            "agent 2 is under terminal 2: C-b M-- from terminal 2 stows it"
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

    /// `prefix a`'s instance: the chat view with the keys first, then the view with the oldest card when
    /// the layout holds it, then the most recently used chat view.
    #[test]
    fn prefix_a_picks_the_view_with_the_keys_before_the_oldest_card() {
        use eitri_core::layout::{Axis, Layout, Node};
        let id = |text: &str| ModuleId::parse(text).unwrap();
        let (two, three) = (id("agent#2"), id("agent#3"));
        let oldest = ViewId::of_module(&two);
        let tree = || {
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::split(
                    Axis::Column,
                    0.5,
                    Node::Leaf(ModuleId::agent()),
                    Node::split(Axis::Column, 0.5, Node::Leaf(two.clone()), Node::Leaf(three.clone())),
                ),
            )
        };
        let mut layout = Layout::new(tree(), three.clone()).unwrap();
        assert_eq!(
            chat_target(&layout, oldest),
            three,
            "keys in agent#3: the key hides that one"
        );
        layout.set_focus(&ModuleId::agent()).unwrap();
        assert_eq!(chat_target(&layout, oldest), ModuleId::agent(), "keys in the base view");
        layout.set_focus(&ModuleId::editor()).unwrap();
        assert_eq!(
            chat_target(&layout, oldest),
            two,
            "keys in the editor: the oldest card's view"
        );
        let mut without = Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::split(
                    Axis::Column,
                    0.5,
                    Node::Leaf(ModuleId::agent()),
                    Node::Leaf(three.clone()),
                ),
            ),
            three.clone(),
        )
        .unwrap();
        without.set_focus(&ModuleId::editor()).unwrap();
        assert_eq!(
            chat_target(&without, oldest),
            eitri_core::layout::module_target(&without, &ModuleId::agent()),
            "the card's view not in the layout: the most recently used chat view"
        );
        assert_eq!(
            chat_target(&without, None),
            eitri_core::layout::module_target(&without, &ModuleId::agent())
        );
    }

    /// The refusal says how to get back to one module, as the keymap binds `focus.child`.
    #[test]
    fn the_one_module_refusal_names_the_way_back() {
        let keymap = eitri_core::keymap::Keymap::defaults();
        let text = one_module_text(&keymap);
        assert!(text.starts_with("acts on one module: "), "{text}");
        match keymap.keys_for(&Action::FocusChild).first() {
            Some(key) => assert!(text.ends_with(&key.human()), "{text}"),
            None => assert!(text.ends_with("focus.child"), "{text}"),
        }
    }

    /// A tab verb ends a container selection whichever route sent it: the panel's own `gt` reaches
    /// `run_action` as the prefix's chord does, and this is the rule it meets there.
    #[test]
    fn a_tab_verb_ends_a_container_selection() {
        let con = Selection::Con(eitri_core::layout::ConId::fresh());
        for tab_action in [TabAction::Next, TabAction::Prev, TabAction::Select(2), TabAction::New] {
            assert_eq!(
                selection_gate(&Action::Tab(tab_action), con),
                SelectionGate::Run { reset: true },
                "{tab_action:?}"
            );
        }
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

    /// Keyboard arrivals enter BROWSE. A new tab opens the composer instead,
    /// since an empty tab has nothing to browse.
    #[test]
    fn keyboard_arrival_is_browse_and_only_a_new_tab_types() {
        // Split literals, so this test's own text is not counted.
        let src = include_str!("main.rs");
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
        let code = eitri_core::source_scan::code_only(src);
        let land_by_key = body_after(&code, concat!("let land_by_key: Rc<dyn Fn(&Module", "Id) -> bool>"));
        assert!(
            land_by_key.contains(concat!(".arr", "ive(view)")),
            "landing by key must tell the agent panel the view arrived"
        );
        let move_focus = body_after(
            &code,
            concat!("let move_focus: Rc<dyn Fn(&Module", "Id, Direction) -> bool>"),
        );
        assert!(
            move_focus.contains(concat!("land_by_", "key(&to)")),
            "a Ctrl+h/j/k/l move must land through land_by_key"
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
        let ui = &src[src.find(concat!("fn build_", "ui(")).expect("build_ui")..];
        let ui = &ui[..ui
            .find(concat!("\nfn ", "flash("))
            .expect("the function after build_ui")];
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
        // Split literals, so this test's own text is not counted.
        let src = include_str!("main.rs");
        let ui = &src[src.find(concat!("fn build_", "ui(")).expect("build_ui")..];
        let ui = &ui[..ui
            .find(concat!("\nfn ", "flash("))
            .expect("the function after build_ui")];
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
    /// Eitri (`terminal::navigation`, asked about each letter) must be named there.
    #[test]
    fn every_chord_the_terminal_gives_up_is_in_the_root_help() {
        let documented: std::collections::HashSet<String> = eitri_core::keymap::root::help_rows()
            .into_iter()
            .flat_map(|row| row.keys.split(" / ").map(str::to_string).collect::<Vec<_>>())
            .collect();
        let mut taken = 0;
        for letter in 'a'..='z' {
            let key = gtk4::gdk::Key::from_name(letter.to_string()).expect("a letter keyval");
            if terminal::navigation(key, gtk4::gdk::ModifierType::CONTROL_MASK).is_some() {
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

    /// `refuse` shows this toast for a killed editor. Other refusals with text show their own
    /// message through `refusal_text`.
    #[test]
    /// The toast covers normal nvim exits and crashes. The editor cannot restart
    /// in this window; relaunching restores it.
    fn the_gone_editor_toast_text_matches_the_spec() {
        assert_eq!(
            gone_editor_toast_text(&LayoutError::Gone(ModuleId::editor())),
            Some(
                "nvim exited and cannot restart in this window \u{2014} relaunch Eitri to get the editor back"
                    .to_string()
            )
        );
        assert_eq!(gone_editor_toast_text(&LayoutError::Gone(ModuleId::agent())), None);
        assert_eq!(
            gone_editor_toast_text(&LayoutError::LastVisible(ModuleId::editor())),
            None
        );
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
            ("no container around", 1, "the words name a module by its internal id"),
            (
                "made, and still not in the layout",
                1,
                "the words describe an internal state",
            ),
            ("no new instance of", 1, "the words print a module kind as debug text"),
        ];
        let sources = scan::rust_sources(&["shell/src"]);
        scan::require_anchors(&sources, &["fn build_ui(", "fn flash(", "fn refuse_said("]);
        let file = scan::file_with(&sources, "fn build_ui(");
        let code_text = scan::code_only(&file.text);
        let raw: Vec<char> = file.text.chars().collect();
        let code: Vec<char> = code_text.chars().collect();
        assert_eq!(
            raw.len(),
            code.len(),
            "code_only keeps one char for every char it reads"
        );
        let bodies = scan::functions(&code_text, "build_ui");
        assert_eq!(bodies.len(), 1, "one build_ui in {}", file.path);
        let body_at = bodies[0].as_ptr() as usize - code_text.as_ptr() as usize;
        let start = code_text[..body_at].chars().count();
        let end = start + bodies[0].chars().count();
        let ident = |c: char| c.is_alphanumeric() || c == '_';
        let word: Vec<char> = "flash".chars().collect();
        let mut found = vec![0usize; FLASH_ONLY.len()];
        let mut bare = Vec::new();
        for at in start..end - word.len() {
            if code[at..at + word.len()] != word[..]
                || ident(code[at - 1])
                || code[at - 1] == '.'
                || ident(code[at + word.len()])
                || code[at + word.len()..end].iter().find(|c| !c.is_whitespace()) != Some(&'(')
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
                    '}' => depth += 1,
                    '{' if depth == 0 => break,
                    '{' => depth -= 1,
                    _ => {}
                }
            }
            let head = (start..open)
                .rev()
                .find(|&j| matches!(code[j], ';' | '{' | '}'))
                .unwrap_or(start);
            let arm: String = raw[head + 1..at].iter().collect();
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
