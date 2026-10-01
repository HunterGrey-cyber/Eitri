//! shell: the real Eitri product window -- custom chrome, the module grid (a layout that is data:
//! docs/superpowers/specs/2026-09-23-modules-and-canvas-design.md), the neovide-editor crate
//! embedded as the real editor pane, and the real agent-ui panel (a WebView-hosted frontend bridged
//! to a lazily-started `agent::AgentSession`). See
//! docs/superpowers/specs/2026-09-06-shell-scaffolding-design.md.

mod agent_panel;
mod chrome;
mod close_prompt;
mod editor_context;
mod editor_quit;
mod editor_start_failure;
mod hint;
mod kill_pane;
mod layout;
mod layout_state;
mod lua;
mod module_grid;
mod nvim_keys;
mod pane_focus;
mod pane_switch;
mod panel_pacer;
mod panel_super;
mod prefix;
mod prefix_strip;
mod supervisor_client;
mod tab_verbs;
mod terminal;
mod terminal_handoff;
mod text_size;
mod theme;
mod toast;
mod tray;
mod version;
mod web_host;
mod webkit_sandbox;
mod webkit_zoom;
mod webview_crash_guard;
mod wheel_zoom;
mod window_mode;
mod xft_dpi;

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::gdk::{Key, ModifierType};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, EventControllerKey};

use eitri_core::keymap::{Action, SwapTarget, TabAction};
use eitri_core::layout::{
    Axis, Direction, KeyAction, KillScope, LayoutError, ModuleDecl, ModuleId, ModuleKeys, ModuleKind, Nav, Reopen,
};
use lua::{LuaEngine, PanelSlot};
use module_grid::{HostKind, ModuleGrid};
use neovide_editor::{NeovideEditorPane, NeovideEditorPaneOptions};
use web_host::WebHost;

const APP_ID: &str = "cn.huntergrey.eitri";

/// How long the top bar's app name shows the prefix indicator's block when a layout verb is refused
/// (spec §3.2: "The refusal flashes the top bar, the same way the prefix indicator shows").
const REFUSAL_FLASH: std::time::Duration = std::time::Duration::from_millis(200);

/// `EITRI_CONFIG_DIR` overrides the config directory (used by sandboxed/manual verification
/// runs so they don't touch a real `~/.config/eitri`); otherwise the real per-user config dir.
fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("EITRI_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").expect("HOME must be set");
    PathBuf::from(home).join(".config").join("eitri")
}

fn main() -> glib::ExitCode {
    // Collected once, before anything else, and consulted through `flag_given` -- never a raw
    // `args_os().any(|a| a == "...")` -- by every early flag scan below (`--version`, `--legacy`,
    // `--clean`). `flag_given` stops at the first `--`, the same end-of-flags rule
    // `eitri_core::project_root::select_root_source` applies to the project-directory argument
    // itself; before this, each scan ran its own ad-hoc `.any()` with no notion of `--` at all, so
    // `shell -- --version` printed the version line instead of opening a directory literally named
    // `--version`, and `shell -- --legacy` was refused on a release build instead of reaching the
    // directory step (v1-dist verdict #5). `args_os`, not `args`, and skipping argv[0] (never a
    // flag), for the same non-UTF-8-safety reason `project_root` gives for its own `args_os` use.
    let early_args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let has_flag = |flag: &str| eitri_core::project_root::flag_given(early_args.iter().map(OsString::as_os_str), flag);

    // `--version` (v1-dist plan Task 1, spec §3), before anything else -- including the stdin
    // detach right below, which is otherwise the very first thing this binary does: printing a
    // version line must never touch GTK, a display, or nvim's stdin.
    // `eitri_core::project_root::KNOWN_FLAGS` also lists `--version`, so `shell --version
    // /some/project` is never a startup failure even though `main()` never reaches `resolve()` on
    // this path -- see that module's own tests.
    if has_flag("--version") {
        println!("{}", version::version_line());
        return glib::ExitCode::SUCCESS;
    }

    // The backend this launch uses (v1-dist plan Task 5, spec §10, D10, D16), decided here -- before
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

    // Which nvim the forked Neovide runtime spawns (v1-dist plan Task 6, spec §7). Resolved and
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

    // WebKit's sandbox (v1-dist sub-plan docs/superpowers/plans/2026-09-28-v1-dist-ubuntu-userns.md):
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
    // Ruling S3 (D4, `docs/superpowers/plans/2026-09-27-v1-scale.md`): before anything else, so
    // `gtk-xft-dpi` is never unset by the time the agent panel's (or a Lua panel's) `WebView` reads
    // it. See `xft_dpi`'s module doc.
    xft_dpi::ensure_xft_dpi();
    // The application id is also the icon's name in the icon theme (`packaging/icons` installs it as
    // `cn.huntergrey.eitri`). On Wayland the compositor takes a window's icon from the desktop entry of
    // the same id (`packaging/cn.huntergrey.eitri.desktop`) and ignores this; an X11 session, and the
    // docks that read the window's own icon name, use it.
    gtk4::Window::set_default_icon_name(APP_ID);
    // Painted with the built-in fallback until the embedded nvim sends its first snapshot.
    let theme_css = theme::gtk_css::ThemeCss::install(&eitri_core::theme::ThemeTokens::fallback());
    // Built before the editor pane for the same reason `pane_switch` is: its env and `--cmd` reach
    // nvim only at spawn. `None` (logged) leaves the window on the fallback colours.
    let mut theme_feed = theme::feed::ThemeFeed::new();

    // Built before the editor pane, because its `child_env()` has to be handed to the pane's
    // constructor -- those variables reach nvim through `Command::env` at spawn time and cannot
    // be added afterwards. `None` means the feature is simply unavailable (see `pane_switch`'s
    // own doc); the editor is constructed with an empty child env and behaves exactly as before.
    let mut pane_switch = pane_switch::open();
    // wire 1's feed, built here for the same reason the other two are: its `child_env()` and its
    // `--cmd` must be handed to the editor pane's constructor, and neither can be added to a child
    // that is already running.
    let mut context_feed = editor_context::EditorContextFeed::new();
    // Panel round 2 plan Task 6's fourth push socket (spec §3): nvim's `mapleader`, `timeoutlen`
    // and Normal-mode maps, built here for the same reason the other three are -- its `child_env()`
    // and `--cmd` must reach the editor pane's constructor, which cannot be handed to an already
    // running child.
    let mut nvim_keys_feed = eitri_core::nvim_keys::feed::NvimKeysFeed::new();
    let mut nvim_child_env = pane_switch.as_ref().map(|ps| ps.child_env()).unwrap_or_default();
    nvim_child_env.extend(theme_feed.as_ref().map(|feed| feed.child_env()).unwrap_or_default());
    nvim_child_env.extend(context_feed.as_ref().map(|feed| feed.child_env()).unwrap_or_default());
    nvim_child_env.extend(nvim_keys_feed.as_ref().map(|feed| feed.child_env()).unwrap_or_default());
    let mut nvim_extra_args = theme_feed.as_ref().map(|feed| feed.nvim_args()).unwrap_or_default();
    nvim_extra_args.extend(context_feed.as_ref().map(|feed| feed.nvim_args()).unwrap_or_default());
    nvim_extra_args.extend(nvim_keys_feed.as_ref().map(|feed| feed.nvim_args()).unwrap_or_default());
    // v1 spec §5 (P1/P14): the nav fallback's loader, from the pane-switch channel's own directory,
    // so `Ctrl+h/j/k/l` leave the editor without vim-tmux-navigator and from Visual mode. Only
    // where the channel exists, because leaving can only work where its socket does.
    nvim_extra_args.extend(pane_switch.as_ref().map(|ps| ps.nvim_args()).unwrap_or_default());
    // wire 3. Unconditional and stateless -- no socket, no directory, nothing to fail at startup --
    // because the only reload trigger a normal Neovim config installs is `FocusGained`, and nothing
    // in this shell ever tells nvim it lost or gained focus. Without this a `git checkout`, a
    // formatter, or an edit made anywhere else never reaches the buffer.
    nvim_extra_args.extend(eitri_core::buffer_reload::nvim_args());
    // The two nvim round trips (phase 3 ruling 18): env and `--cmd` reach nvim only at spawn.
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
        Some(feed) => editor_context::listen(feed, scratch_path.clone()),
        None => std::rc::Rc::new(|| None),
    };
    let (agent_widget, agent_panel_handle) = agent_panel::build_agent_panel(
        project_root.to_path_buf(),
        editor_context_source,
        scratch_dir,
        backend_kind,
        panel_notice,
    );

    lua_engine.load_init_file(&config_dir.join("init.lua"));

    // Which local Claude account this window spends. Two sources, and the environment wins:
    // `eitri --account <name>` (and this host's own `VERDANDI_CLAUDE_ACCOUNT`, exported for
    // Verdandi and inherited by every `eitri` started from a terminal) arrives as that variable,
    // and `init.lua`'s `eitri.config.set("agent.account", "<name>")` is the per-machine default
    // underneath it -- which is what pins the account for a launch from the app menu, where no
    // shell configuration has run. Read here, once: this is the first point where `init.lua` has
    // run, and still before anything reads a transcript or starts a sidecar (the panel computes its
    // greeting from a WebView `ready` signal, i.e. after the main loop starts).
    //
    // Nothing set anywhere is the shipped default and changes nothing. A name that is malformed or
    // points at no directory is a hard startup failure naming the source -- never a silent fallback
    // to "whichever shell launched this window", which is the accident that made a resumed session
    // open empty on 2026-09-21 (see `agent::account`).
    // How big the agent panel's text is. One number: every other size in `index.css` is a ratio of
    // it, so this rescales the panel coherently instead of moving one label. It is NOT taken from
    // nvim's `guifont` height, which is the editor's size for a MONOSPACE face while the panel is
    // mostly proportional prose -- following it 1:1 would make the two panes disagree by however
    // much the two faces disagree at the same nominal size. Unset changes nothing.
    //
    // Out of range is a startup failure naming the key, the same discipline as `agent.account`
    // above: silently clamping a number someone typed is how a knob gets reported as broken.
    let panel_font_size = match lua_engine.config.borrow().get("agent.font_size").map(str::to_owned) {
        None => eitri_core::theme::DEFAULT_PANEL_FONT_SIZE_PX,
        Some(raw) => match raw.trim().parse::<f32>() {
            Ok(px) if eitri_core::theme::PANEL_FONT_SIZE_RANGE_PX.contains(&px) => {
                eprintln!("[panel] font size {px}px (init.lua's agent.font_size)");
                px
            }
            Ok(px) => {
                eprintln!(
                    "eitri: eitri.config.set(\"agent.font_size\", {raw:?}): {px} is outside {:?}",
                    eitri_core::theme::PANEL_FONT_SIZE_RANGE_PX
                );
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("eitri: eitri.config.set(\"agent.font_size\", {raw:?}): not a number ({e})");
                std::process::exit(1);
            }
        },
    };

    // How often the agent panel's stream reaches its page while the user types in the editor
    // (owner decision #37: an even cadence, not a hold; `eitri_core::panel_cadence`). Unset is
    // `DEFAULT_CADENCE_HZ` (5) a second; `"off"` is today's full rate; anything else is a startup failure naming the key, like
    // `agent.font_size` above.
    let typing_cadence = match eitri_core::panel_cadence::parse_config(
        lua_engine.config.borrow().get(eitri_core::panel_cadence::CADENCE_KEY),
    ) {
        Ok(cadence) => cadence,
        Err(message) => {
            eprintln!("eitri: {message}");
            std::process::exit(1);
        }
    };
    match typing_cadence {
        Some(hz) => eprintln!("[panel] stream cadence while typing in the editor: {hz}/s"),
        None => eprintln!("[panel] stream cadence while typing in the editor: off (full rate)"),
    }
    agent_panel_handle.set_typing_cadence(typing_cadence);

    // Whether the launch offers the last window's tabs back (`agent.restore`: "offer", the default,
    // "auto" or "off"), and the mode new tabs start in (`agent.default_mode`: "auto" or "bypass").
    // Anything else is a startup failure naming the key, like `agent.font_size` above. Naming bypass
    // here is the one way a window starts in bypass without asking, because the answer is in a file
    // the user wrote.
    let restore_policy = match eitri_core::tab_restore::RestorePolicy::parse(
        lua_engine.config.borrow().get(eitri_core::tab_restore::RESTORE_KEY),
    ) {
        Ok(policy) => policy,
        Err(message) => {
            eprintln!("eitri: {message}");
            std::process::exit(1);
        }
    };
    agent_panel_handle.set_restore_policy(restore_policy);
    let default_mode = match eitri_core::agent_prefs::parse_default_mode(
        lua_engine
            .config
            .borrow()
            .get(eitri_core::agent_prefs::DEFAULT_MODE_KEY),
    ) {
        Ok(mode) => mode,
        Err(message) => {
            eprintln!("eitri: {message}");
            std::process::exit(1);
        }
    };
    if let Some(mode) = default_mode {
        eprintln!(
            "[agent] new tabs start in {} (init.lua's agent.default_mode)",
            mode.as_str()
        );
    }
    agent_panel_handle.set_default_mode(default_mode);

    // What a card for a hidden chat does (modules P2, spec §3.3, decision b): the tray's chip and a
    // toast, or with `reveal` the chat itself. Anything but `badge`/`reveal` is a startup failure
    // naming the key, like `agent.font_size` above.
    let on_permission = match eitri_core::attention::ChatOnPermission::parse(
        lua_engine
            .config
            .borrow()
            .get(eitri_core::attention::ChatOnPermission::KEY),
    ) {
        Ok(policy) => policy,
        Err(message) => {
            eprintln!("eitri: {message}");
            std::process::exit(1);
        }
    };

    let account_from_env = std::env::var("VERDANDI_CLAUDE_ACCOUNT").ok();
    let account_from_config = lua_engine.config.borrow().get("agent.account").map(str::to_owned);
    match agent::account::resolve_for(account_from_env.as_deref(), account_from_config.as_deref()) {
        Ok(Some(account)) => {
            let source = if agent::account::name_to_use(account_from_env.as_deref(), None).is_some() {
                "VERDANDI_CLAUDE_ACCOUNT"
            } else {
                "init.lua's agent.account"
            };
            eprintln!(
                "[account] claude account '{}' from {source} -> {}",
                account.name(),
                account.config_dir().display()
            );
            agent::account::configure(account);
        }
        Ok(None) => {}
        Err(err) => {
            eprintln!("eitri: the configured claude account is unusable: {err}");
            std::process::exit(1);
        }
    }

    // The keymap (keymap spec §2.3): stock tmux's defaults, prefix `Ctrl+b`, with `init.lua`'s
    // `eitri.keymap` calls applied. A bad key, an unknown action or option, or a collision is a
    // startup failure naming both sides, as `agent.font_size` is -- never a keymap nobody wrote.
    let lua_panel_ids: Vec<String> = lua_engine
        .panels
        .borrow()
        .entries()
        .iter()
        .map(|e| e.id.clone())
        .collect();
    let keymap = match eitri_core::keymap::Keymap::apply_user(lua_engine.keymap.borrow().ops(), &lua_panel_ids) {
        Ok(keymap) => Rc::new(keymap),
        Err(err) => {
            eprintln!("eitri: {err}");
            std::process::exit(1);
        }
    };
    // Collision rule 4: a Lua command's accelerator that is the prefix or a root chord would never fire.
    for (id, entry) in lua_engine.commands.borrow().iter() {
        if let Some(keybinding) = &entry.keybinding {
            if let Err(err) = eitri_core::keymap::check_command_keybinding(id, keybinding, &keymap) {
                eprintln!("eitri: {err}");
                std::process::exit(1);
            }
        }
    }
    println!(
        "[keymap] prefix {} ({} bindings)",
        keymap.prefix(),
        keymap.bindings().len()
    );

    // The bottom terminal (docs/superpowers/specs/2026-09-23-bottom-terminal-design.md), a module
    // since the modules design's P1 re-homed it (its plan's Task 11). Hidden in a first launch, and
    // its shell is not started until it is first shown, by any route (the change hook below): a
    // window whose terminal is never shown starts no process, touches no font manager, and looks
    // exactly as it did before the terminal existed. A layout that reopens with it shown starts it
    // at launch.
    let terminal = terminal::TerminalPane::new(project_root.to_path_buf());

    // The layout is data (modules design P1, docs/superpowers/specs/
    // 2026-09-23-modules-and-canvas-design.md §4): `[editor | agent]`, the terminal below both
    // (hidden), then each Lua panel placed by its `position` in the order it was registered
    // (`terminal::initial_layout`, `Layout::initial`, `PanelSlot::placement`). The editor, the agent
    // and the terminal are not in the Lua registry at all: every window has all three, and a Lua
    // panel no longer replaces any of them -- a Lua `bottom` panel goes under the terminal.
    let lua_panels: Vec<(ModuleId, PanelSlot, gtk4::Widget)> = lua_engine
        .panels
        .borrow()
        .entries()
        .iter()
        .map(|entry| (ModuleId::lua(&entry.id), entry.slot, entry.widget.clone()))
        .collect();
    // Each Lua panel's page, for `prefix x`: a killed panel's web process is ended and its key loads
    // this again (`kill_pane`). The 4th element is the same panel's crash-loop guard (fix round 1,
    // `lua::panel::PanelEntry::crash_guard`'s own doc): the revive path below resets it, the same
    // manual-recovery re-arming `AgentPanelHandle::reload_document_by_hand` does for the chat panel.
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
    let decls: Vec<ModuleDecl> = lua_panels
        .iter()
        .map(|(id, slot, _)| ModuleDecl {
            id: id.clone(),
            placement: slot.placement(),
        })
        .collect();
    // Each Lua panel's module key after `Ctrl+a` (modules P2, spec §4.5). A key that is reserved,
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
    // The `?` overlay's window and prefix sections, generated from the keymap (keymap spec §2.9).
    // Panel round 2 plan Task 6: the panel table (spec §3.6's `effective()`) and the new-tab chord
    // travel in the same envelope, and the panel table is recomputed -- and only re-sent when it
    // actually changed -- every time nvim's own keys report changes (`nvim_keys::listen` below).
    // The chord for `Action::Tab(TabAction::New)`, spelled the same way `show_editor`/`way_back`
    // are below: the prefix as a person reads it, then the first key bound to the action.
    let new_tab_chord = keymap
        .keys_for(&Action::Tab(TabAction::New))
        .first()
        .map(|key| format!("{} {}", keymap.prefix().human(), key.human()))
        .unwrap_or_default();
    // `latest.0` is the last nvim report seen (`None` until the feed's first line, or forever if
    // the feed could not start); `latest.1` is the last `PanelKeymap` actually sent, so a report
    // that leaves the merged table unchanged costs no dispatch and no repeated log line.
    let latest: Rc<
        RefCell<(
            Option<eitri_core::nvim_keys::NvimReport>,
            Option<eitri_core::keymap::PanelKeymap>,
        )>,
    > = Rc::new(RefCell::new((None, None)));
    let send_keymap: Rc<dyn Fn()> = {
        let latest = latest.clone();
        let keymap = keymap.clone();
        let module_keys = module_keys.clone();
        let agent_panel_handle = agent_panel_handle.clone();
        let new_tab_chord = new_tab_chord.clone();
        Rc::new(move || {
            let mut latest_ref = latest.borrow_mut();
            let report = latest_ref.0.clone();
            let (panel_keymap, log) = eitri_core::keymap::panel::effective(keymap.panel_user(), report.as_ref());
            for line in log {
                println!("{line}");
            }
            if latest_ref.1.as_ref() != Some(&panel_keymap) {
                agent_panel_handle.set_keymap_help(eitri_core::agent_bridge::serialize_keymap_for_js(
                    &keymap.prefix().human(),
                    &eitri_core::keymap::root::help_rows(),
                    &keymap.help(&module_keys),
                    &panel_keymap,
                    &new_tab_chord,
                ));
                latest_ref.1 = Some(panel_keymap);
            }
        })
    };
    send_keymap();
    if let Some(feed) = nvim_keys_feed.as_mut() {
        let latest = latest.clone();
        let send_keymap = send_keymap.clone();
        nvim_keys::listen(feed, move |report| {
            latest.borrow_mut().0 = Some(report);
            send_keymap();
        });
    }
    // The layout this window opens with (modules P2, spec §4.4, §4.6): this project's state file if
    // it can be used, else `init.lua`'s `eitri.layout.default`, else the first launch. A malformed
    // default is a startup failure naming the call; a state file that cannot be used is not, since it
    // is state rather than config -- it is logged and the default opens.
    let lua_default = match lua_engine.layout.borrow().default_tree().cloned() {
        Some(Ok(tree)) => Some(tree),
        Some(Err(message)) => {
            eprintln!("eitri: {message}");
            std::process::exit(1);
        }
        None => None,
    };
    let state_dir = eitri_core::layout::persist::state_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    let loaded = state_dir
        .as_deref()
        .map(|dir| eitri_core::layout::persist::load(dir, project_root, &decls));
    // The key that shows the editor, as the keymap binds it: named by the note a layout that hides
    // the editor prints, the way the toast names its own way back.
    let show_editor: Option<String> = keymap
        .keys_for(&Action::Module(ModuleId::editor()))
        .first()
        .map(|key| format!("{} {}", keymap.prefix().human(), key.human()));
    let module_layout =
        match layout_state::choose_startup_layout(lua_default.as_ref(), loaded, &decls, show_editor.as_deref()) {
            Ok((layout, notes)) => {
                for note in notes {
                    println!("[layout] {note}");
                }
                Rc::new(RefCell::new(layout))
            }
            Err(message) => {
                eprintln!("eitri: {message}");
                std::process::exit(1);
            }
        };
    let layout_saver = layout_state::LayoutSaver::new(state_dir, project_root, module_layout.clone(), decls.clone());
    // Every module's host, added once and never reparented (`module_grid`'s module doc).
    let grid = ModuleGrid::new(module_layout.clone());
    // R1-4: the editor's host is a small overlay carrying a hidden failure-message label, not the
    // bare `GtkGLArea` -- see `editor_start_failure`'s own doc for why this has to wrap the widget
    // before it is ever handed to the grid.
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
    // The floor `build_vertical_split` gave the bottom slot on `main`: a terminal dragged to zero
    // reports a 1-row grid to its shell.
    terminal.widget().set_size_request(-1, layout::BOTTOM_MIN_HEIGHT);
    grid.add(
        ModuleId::terminal(),
        terminal.widget(),
        terminal.widget(),
        HostKind::Direct,
    );
    for (id, slot, widget) in &lua_panels {
        // On the `WebView`: its host measures its one child, so the grid sees the same floor.
        if *slot == PanelSlot::Bottom {
            widget.set_size_request(-1, layout::BOTTOM_MIN_HEIGHT);
        }
        grid.add(id.clone(), &WebHost::new(widget), widget, web_host);
    }
    // The terminal's shell starts the first time it is shown, however it got there -- `Ctrl+a t`,
    // `Ctrl+a \ t`, its tray chip, `eitri.layout.show`, a saved layout -- rather than only on the
    // `Ctrl+a t` path. `start` does nothing once a shell is running or has exited.
    {
        let module_layout = module_layout.clone();
        let terminal = terminal.clone();
        grid.connect_changed(move || match module_layout.try_borrow() {
            Ok(layout) if layout.is_shown(&ModuleId::terminal()) => {
                drop(layout);
                terminal.start();
            }
            Ok(_) => {}
            // Hooks run with the layout released (`ModuleGrid::connect_changed`); this line is how a
            // caller that breaks that would show up, instead of a terminal that silently never starts
            // (Task 9's review, minor 4).
            Err(_) => eprintln!("[terminal] BUG: the layout was borrowed when it changed; the shell was not started"),
        });
    }
    grid.apply();
    // Every change of the arrangement is written to this project's state file, 500ms after the last
    // one (spec §4.6), and once more when the window closes if it is still unwritten; a change of the
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
            if id.kind() == ModuleKind::Editor {
                pane.grab_focus();
                // `NeovideEditorPane::grab_focus` returns nothing, so ask: did the editor's widget
                // become the window's focus widget? (It used to say `true` unconditionally, so the
                // `grab_focus=` in `[pane_switch]` lines could not be believed for the editor.)
                return pane.widget().is_focus();
            }
            // Read live (modules P2): a module added after startup is found too.
            grid.focus_target(id).is_some_and(|target| target.grab_focus())
        })
    };

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Eitri")
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
    agent_panel::install_reload_action(app, &agent_panel_handle);
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
    // The panel's LIVE size (zoom-together design, spec §5's "L3" bullet): starts at the base and
    // is kept current by `text_size_controller` below. `panel_tokens` reads this rather than the
    // captured `panel_font_size` so a colorscheme change re-derives every OTHER token but never
    // resets a zoom back to the un-zoomed base.
    let panel_px = Rc::new(std::cell::Cell::new(panel_font_size));
    // The editor's own cell height (wave 4, R5), reported by `neovide-editor`'s
    // `connect_cell_size_changed` below -- `None` until nvim has reported a font, same as
    // `--nv-editor-row` itself: see `panel_tokens`'s own use of this and the block that fills it in.
    // Kept in the editor's GTK logical px and handed to the panel in its CSS px, which differ by
    // WebKitGTK's own page zoom whenever `gtk-xft-dpi` is not 96 dpi (`webkit_zoom`'s module doc).
    // Read here, not earlier: `build_ui` has not returned to the main loop since the panel's
    // `WebView` was created, so no `gtk-xft-dpi` change can have reached either side in between.
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
            // known limit (item 3g, 2026-09-23): `live_panel_font_size_px` is unit-tested in
            // `text_size.rs`, but nothing here stops a future edit from replacing this call with
            // `panel_font_size` (the captured startup base) directly -- that compiles and no test
            // catches it, since this closure is GTK wiring with no headless harness. A GUI pass
            // (change the colorscheme after zooming; the panel must stay zoomed) is what would.
            tokens.editor_row_px = editor_row.get().css_px();
            tokens
        }
    };
    agent_panel_handle.set_theme(&panel_tokens(None));
    pane.set_clear_color(editor_clear(&eitri_core::theme::ThemeTokens::fallback()));
    terminal.set_colors(terminal::colors_from(&eitri_core::theme::ThemeTokens::fallback()));
    // Wave 4, R5: the moment the editor reports a cell height (or it changes -- a colorscheme's
    // `guifont`, a zoom), record it for `panel_tokens` (so a later colorscheme change keeps it) and
    // push it into the panel's live theme the same way `text_size_controller` pushes a font-size
    // zoom -- one already-`set_theme`-based call, no new envelope.
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
        let terminal = terminal.clone();
        theme::feed::listen(feed, move |payload| {
            let tokens = eitri_core::theme::ThemeTokens::derive(&payload);
            println!(
                "[theme] following nvim colorscheme {:?} (background={})",
                payload.options.colors_name, payload.options.background
            );
            theme_css.update(&tokens, window_for_theme.upcast_ref());
            agent_panel_handle.set_theme(&panel_tokens(Some(&payload)));
            pane_for_theme.set_clear_color(editor_clear(&tokens));
            terminal.set_colors(terminal::colors_from(&tokens));
        });
    }

    // Zoom both panes together, and each one alone (spec
    // docs/superpowers/specs/2026-09-22-zoom-together-design.md). Registers `app.text-larger`/
    // `-smaller`/`-reset` (both panes) and follows `g:neovide_scale_factor` back from nvim; the
    // `Ctrl+a` prefix's one-pane `=`/`-`/`0` calls `apply_editor`/`apply_panel` directly once it
    // has resolved which pane holds the keys (below, alongside `Zoom`/`Resize`/`SendPrefix`).
    let text_size_controller = text_size::TextSizeController::install(
        app,
        // Every window has the editor since the modules design's P1: a Lua `main` panel hides it
        // rather than replacing it, and a text-size write to an nvim that has not started yet is
        // buffered by the pane (`ScaleWatch`).
        Some(pane.clone()),
        agent_panel_handle.clone(),
        panel_font_size,
        panel_px,
    );

    // `Ctrl`+wheel over a module: the text size of that pane only (keymap spec §2.7). Every host
    // exists by now -- the editor, the agent, the terminal and each Lua panel were added above.
    for (id, host) in grid.hosts() {
        wheel_zoom::install(id, &host, text_size_controller.clone());
    }

    let top_bar = chrome::build_top_bar(&window, project_root);
    root.append(&top_bar.widget);
    root.append(&grid);

    // What each module is called on its tray chip and in the prefix strip (modules P2): the
    // built-ins by kind, a Lua panel by the `title` it registered with.
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
    // The tray (spec §3.3): a chip for every module off screen, one per module, in the order the
    // grid holds them. Its chips are top-bar items ahead of `↻`, for `h`/`l`, HINT and `Ctrl+k`.
    let tray = tray::Tray::build(
        &grid
            .hosts()
            .iter()
            .map(|(id, _)| (id.clone(), module_title(id)))
            .collect::<Vec<_>>(),
    );
    top_bar.tray.append(tray.widget());
    let prefix_strip = Rc::new(prefix_strip::PrefixStrip::new());
    top_bar.strip.append(prefix_strip.widget());
    let top_items: Vec<gtk4::Widget> = tray.items().into_iter().chain(top_bar.items.iter().cloned()).collect();

    // The global `f` HINT draws its GTK labels (top-bar items, the editor, each Lua panel) as
    // overlay children of this, positioned over whatever they label. Its only child is the whole
    // window content, so the layout is exactly what it was without it; the labels never take a
    // click.
    let hint_overlay = gtk4::Overlay::new();
    hint_overlay.set_child(Some(&root));
    window.set_child(Some(&hint_overlay));
    let toast = toast::Toast::install(&hint_overlay, &top_bar.widget);

    // The global `f` HINT (spec: docs/superpowers/specs/2026-09-19-global-hint-design.md). Three
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
        focus_module: focus_module.clone(),
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

    // Which module has the keys. One tracker drives the editor's cursor (solid, or not drawn,
    // via Neovide itself) and the agent panel's cursor and mode block, so they cannot disagree. See
    // `pane_focus`'s module doc. The module that last held them is the layout's `focus`: where the
    // top bar's `Ctrl+j` returns to, what the `Ctrl+a` prefix zooms and resizes around, and the
    // head of the MRU list `neighbor()` breaks ties with.
    {
        let agent_panel_handle = agent_panel_handle.clone();
        let editor = pane.clone();
        let terminal = terminal.clone();
        let module_layout = module_layout.clone();
        let focus_history = focus_history.clone();
        pane_focus::install(
            &window,
            grid.live_hosts(),
            move |id| match module_layout.try_borrow_mut() {
                Ok(mut layout) => {
                    // Refused for a hidden module (`Layout::set_focus`): GTK can put focus in an
                    // unmapped widget, and the layout's `focus` -- what `Ctrl+j` from the top bar
                    // returns to and the prefix acts on -- must stay on one that can be seen. The
                    // history hears only what the layout accepted, so it never names a module the
                    // keys could not have been on.
                    match layout.set_focus(id) {
                        Ok(()) => focus_history.borrow_mut().note(id),
                        Err(err) => eprintln!("[pane_focus] {err}; the layout keeps {}", layout.focus()),
                    }
                }
                // Every writer of the layout releases it before touching focus (`ModuleGrid::apply`,
                // `hide_module`); this line is how a new one that does not would show up.
                Err(_) => eprintln!("[pane_focus] BUG: the layout was borrowed when focus moved to {id}"),
            },
            move |id, has_keys| {
                println!("[pane_focus] {id} has_keys={has_keys}");
                match id.kind() {
                    ModuleKind::Editor => {
                        editor.set_focused(has_keys);
                        // The typing window only counts while the editor holds the keys.
                        agent_panel_handle.set_editor_has_keys(has_keys);
                    }
                    ModuleKind::Agent => agent_panel_handle.set_pane_focused(has_keys),
                    ModuleKind::Terminal => terminal.set_focused(has_keys),
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

    // F11 fullscreen, `prefix F11` immersive, both following g:neovide_fullscreen (spec
    // 2026-09-19-window-modes-design.md §2). Needs `return_to_pane` for a top bar that hides while
    // it holds focus, and gives `focus_top_bar` its reveal.
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

    // Where the keys land after a module key or a tray chip. Into the agent: on its oldest pending
    // card if one waits (spec §3.3, `focus_oldest_card`), else BROWSE (panel round 2 plan spec §8,
    // decision 4: reverses 2026-09-19's "control l直接闪cursor"), as every keyboard arrival there does
    // (`move_focus`) -- on the last row if the reader was following it, else on the row and scroll
    // they left (owner decision #22, 2026-09-29; the page decides, `App.tsx`'s `arrive` effect).
    let arrive: Rc<dyn Fn(&ModuleId)> = {
        let agent_panel_handle = agent_panel_handle.clone();
        Rc::new(move |id| {
            if id.kind() == ModuleKind::Agent {
                // The oldest card across every tab: its tab is switched to first (session tabs).
                if !agent_panel_handle.focus_oldest_card() {
                    agent_panel_handle.arrive();
                }
            }
        })
    };

    // A card that arrives while the chat is not on screen (spec §3.3, decision b): a toast -- the
    // only sign in Immersive mode -- or, with `modules.chat.on_permission = reveal` and nothing
    // zoomed, the chat back where it was, the keys staying where they are.
    // The key that brings the chat back, as the keymap binds it: the toast's last words.
    let way_back: Option<String> = keymap
        .keys_for(&Action::Module(ModuleId::agent()))
        .first()
        .map(|key| format!("{} {}", keymap.prefix().human(), key.human()));
    {
        let module_layout = module_layout.clone();
        let grid = grid.clone();
        let toast = toast.clone();
        let tray = tray.clone();
        let way_back = way_back.clone();
        // Cloned so the closure can call back into the handle for the tab that holds the newest
        // card: the hook fires from the pump (`report_attention`) after its own state borrow is
        // already dropped, so this is safe and never re-enters a held `RefCell`.
        let agent_panel_handle_for_toast = agent_panel_handle.clone();
        agent_panel_handle.on_attention(move |before, after| {
            let place = eitri_core::layout::agent_place(&module_layout.borrow());
            let reaction = eitri_core::attention::react(on_permission, before, after, place);
            println!("[attention] agent {after:?} ({place:?}) -> {reaction:?}");
            tray.refresh(&module_layout.borrow(), after);
            if reaction.reveal {
                if let Err(err) = grid.show_module(&ModuleId::agent()) {
                    eprintln!("[attention] could not reveal the chat: {err}");
                }
            }
            if reaction.toast {
                toast.show(&toast::permission_toast_text(
                    after,
                    agent_panel_handle_for_toast
                        .newest_card_label()
                        .as_ref()
                        .map(|(n, name)| (*n, name.as_str())),
                    way_back.as_deref(),
                ));
            }
        });
    }
    // The one-line result of bringing the last window's tabs back.
    {
        let toast = toast.clone();
        agent_panel_handle.on_toast(move |text| toast.show(text));
    }
    // The three ways a verb brings a module to the user, each written once and shared by every route
    // that does it -- `Ctrl+a <key>` and its tray chip (`open_module` below), `Ctrl+a \`/`"`, and
    // `eitri.layout.*` -- so a change to one reaches them all (the whole-branch review's finding 7:
    // three inlined copies had grown). Each returns the layout's refusal for its caller to report:
    // the prefix and a chip flash the app name, a Lua call logs it.
    //
    // A zoom ends only when it would keep the module off screen (`Layout::zoom_hides`): `Ctrl+a a`,
    // a chip, or `eitri.layout.focus` onto the zoomed chat itself leaves it zoomed, as tmux's
    // `select-pane` onto the zoomed pane does (the whole-branch review's finding 3).
    //
    // `focus_and_arrive`: gives `id` the keys and arrives (`arrive`). Refused before anything moves
    // for a module the keys cannot go to (`Layout::can_focus`), so a refused focus does not end a
    // zoom on its way to being refused.
    let focus_and_arrive: Rc<dyn Fn(&ModuleId) -> Result<(), LayoutError>> = {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let focus_module = focus_module.clone();
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
            if focus_module(id) {
                arrive(id);
            }
            Ok(())
        })
    };
    // `show_on_screen`: shows `id` where it was hidden from, the keys staying where they are
    // (`eitri.layout.show`, and the first half of `Ctrl+a <key>` on a hidden module). A zoom that
    // would keep it off screen ends first, as `Ctrl+a e`/`a` have always ended it: a module shown is a
    // module on screen (Task 12's review, minor 2 -- `show` used to log success and leave it behind
    // the zoom).
    let show_on_screen: Rc<dyn Fn(&ModuleId) -> Result<(), LayoutError>> = {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        Rc::new(move |id| {
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
    // The scratch round trips (`Ctrl+g`, `gf`): the editor is shown and given the keys before nvim
    // is handed the request, and an edit that came back returns the keys to the chat.
    {
        let pane = pane.clone();
        let show_on_screen = show_on_screen.clone();
        let focus_module = focus_module.clone();
        let module_layout = module_layout.clone();
        agent_panel_handle.on_editor_request(move |keys| {
            // `prefix x` quit it, and it cannot come back in this window (`kill_pane`).
            if module_layout.borrow().is_gone(&ModuleId::editor()) {
                return Err(LayoutError::Gone(ModuleId::editor()).to_string());
            }
            // `is_running`, not `is_ready`: an nvim that exited is not ready either, and keys sent
            // to it would reach nothing (R1-2).
            if !pane.is_running() {
                return Err("the editor is not ready yet".to_string());
            }
            show_on_screen(&ModuleId::editor()).map_err(|e| e.to_string())?;
            focus_module(&ModuleId::editor());
            pane.send_keys(keys);
            Ok(())
        });
    }
    {
        let focus_and_arrive = focus_and_arrive.clone();
        agent_panel_handle.on_editor_done(move || {
            if let Err(err) = focus_and_arrive(&ModuleId::agent()) {
                eprintln!("[scratch] could not return the keys to the chat: {err}");
            }
        });
    }
    // `place_and_arrive`: `id` into a new split after the module with the keys, along `axis`, moved
    // there if it is elsewhere, and the keys to it (`Ctrl+a \`/`"` + a key, `Ctrl+a <key>` for a
    // module never placed, `eitri.layout.split`).
    let place_and_arrive: Rc<dyn Fn(&ModuleId, Axis) -> Result<(), LayoutError>> = {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let focus_module = focus_module.clone();
        let arrive = arrive.clone();
        Rc::new(move |id, axis| {
            let target = module_layout.borrow().focus().clone();
            grid.place_module(id, &target, axis)?;
            if focus_module(id) {
                arrive(id);
            }
            Ok(())
        })
    };

    // `Ctrl+a <key>`'s four cases for a module (modules spec §4.3, `eitri_core::layout::key_action`),
    // for the prefix and the tray's chips; a refusal flashes the app name (spec §3.2).
    let open_module: Rc<dyn Fn(&ModuleId, KeyAction)> = {
        let grid = grid.clone();
        let focus_module = focus_module.clone();
        let focus_and_arrive = focus_and_arrive.clone();
        let show_on_screen = show_on_screen.clone();
        let place_and_arrive = place_and_arrive.clone();
        let app_name = top_bar.app_name.clone();
        let toast = toast.clone();
        Rc::new(move |id, action| {
            println!("[modules] {id}: {action:?}");
            let result = match action {
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

    // `prefix x`, tmux's `kill-pane` (owner, 2026-09-26; `kill_pane`'s module doc has the rulings).
    // `killed` holds the modules killed and not shown since: a show brings each back fresh (the hook
    // below). The editor never is -- it cannot come back (`Reopen::Never`). `kill_prompt` is the
    // window's y/n, installed after the prefix below so its controller sees keys first.
    let killed: Rc<RefCell<std::collections::BTreeSet<ModuleId>>> = Rc::default();
    let kill_prompt: Rc<std::cell::OnceCell<Rc<close_prompt::ClosePrompt>>> = Rc::default();
    // The editor closed for the rest of this window once nvim is gone (`Reopen::Never`: it cannot
    // come back, `kill_pane`), the keys going to its neighbour, and the pane released on the next
    // turn of the loop so nothing reaches the closed connection. Refused as a hide is (the editor is
    // the last module on screen); the caller decides what then.
    let retire_editor: Rc<dyn Fn() -> Result<(), LayoutError>> = {
        let grid = grid.downgrade();
        let focus_module = focus_module.clone();
        let released = Rc::downgrade(&pane);
        Rc::new(move || {
            let Some(grid) = grid.upgrade() else { return Ok(()) };
            grid.kill_module(&ModuleId::editor(), Reopen::Never, &*focus_module)?;
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
    // cancel and a declined window close then do (R3, v1 hardening Task 6): `editor_quit`'s flow,
    // carried out here through `ShellQuitHost`, one GTK call per method.
    let editor_quitting = editor_quit::EditorQuitting::new(Rc::new(ShellQuitHost {
        pane: pane.clone(),
        module_layout: module_layout.clone(),
        grid: grid.downgrade(),
        show_on_screen: show_on_screen.clone(),
        focus_module: focus_module.clone(),
        retire_editor: retire_editor.clone(),
        prompt: kill_prompt.clone(),
        agent: agent_panel_handle.clone(),
        window: window.clone(),
        app_name: top_bar.app_name.clone(),
        toast: toast.clone(),
    }));
    let kill_now: Rc<dyn Fn(&ModuleId, Option<String>) -> Result<(), String>> = {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let focus_module = focus_module.clone();
        let terminal = terminal.clone();
        let agent = agent_panel_handle.clone();
        let lua_pages = lua_pages.clone();
        let decls = decls.clone();
        let killed = killed.clone();
        let editor_quitting = editor_quitting.clone();
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
                ModuleKind::Terminal => {
                    grid.kill_module(id, reopen, &*focus_module)
                        .map_err(|e| e.to_string())?;
                    // Hangs the shell up (SIGHUP) and forgets it: the next show starts a fresh one.
                    terminal.close();
                }
                ModuleKind::LuaWebview => {
                    grid.kill_module(id, reopen, &*focus_module)
                        .map_err(|e| e.to_string())?;
                    if let Some((_, widget, _, _)) = lua_pages.iter().find(|(m, _, _, _)| m == id) {
                        if let Some(webview) = widget.downcast_ref::<webkit6::WebView>() {
                            use webkit6::prelude::*;
                            webview.terminate_web_process();
                        }
                    }
                    killed.borrow_mut().insert(id.clone());
                }
                ModuleKind::Agent => {
                    let closed = agent.close_every_tab()?;
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
                    ModuleKind::Agent => agent.open_chooser(),
                    ModuleKind::LuaWebview => {
                        if let Some((_, widget, url, crash_guard)) = lua_pages.iter().find(|(m, _, _, _)| *m == id) {
                            if let Some(webview) = widget.downcast_ref::<webkit6::WebView>() {
                                use webkit6::prelude::*;
                                webview.load_uri(url);
                            }
                            // This IS the manual recovery a give-up message points the owner at
                            // (`prefix x`, then this panel's own key): re-arm the guard so a later,
                            // unrelated crash is caught fresh rather than silently dropped as
                            // `CrashResponse::AlreadyGivenUp` (fix round 1).
                            crash_guard.borrow_mut().reset();
                        }
                    }
                    _ => {}
                }
            }
        });
    }
    // The shell ending by itself -- `exit`, `Ctrl+d`, a signal -- closes the terminal as `prefix x`
    // does, without asking (owner, 2026-09-26: "底下终端exit应该是直接关闭terminal窗口";
    // `terminal::closes_on_exit` says which ends). As the last module on screen, tmux's own rule
    // (closing the last pane closes the window) applies instead: `eitri_core::layout::can_kill`
    // says which (`terminal::on_exit`), and a `Window` scope closes the window through its own
    // ordinary path -- `close window? N running (y/n)` when a tab runs -- rather than the module,
    // leaving the terminal's notice on screen until the window really goes.
    {
        let grid = grid.downgrade();
        let module_layout = module_layout.clone();
        let focus_module = focus_module.clone();
        let decls = decls.clone();
        let window = window.clone();
        terminal.on_shell_exit(move || {
            let Some(grid) = grid.upgrade() else { return false };
            let id = ModuleId::terminal();
            let scope = match eitri_core::layout::can_kill(&module_layout.borrow(), &id) {
                Ok(scope) => scope,
                Err(err) => {
                    println!("[terminal] the shell ended, and the terminal stays: {err}");
                    return false;
                }
            };
            match terminal::on_exit(scope) {
                terminal::OnExit::CloseWindow => {
                    println!("[terminal] the shell ended; the last module on screen closes Eitri");
                    window.close();
                    false
                }
                terminal::OnExit::CloseModule => {
                    match grid.kill_module(&id, kill_pane::reopen(&id, &decls), &*focus_module) {
                        Ok(()) => {
                            println!("[terminal] the shell ended; the terminal is closed");
                            true
                        }
                        Err(err) => {
                            println!("[terminal] the shell ended, and the terminal stays: {err}");
                            false
                        }
                    }
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
    // Every layout change redraws the tray.
    {
        let tray = tray.clone();
        let module_layout = module_layout.clone();
        let agent_panel_handle = agent_panel_handle.clone();
        grid.connect_changed(move || {
            if let Ok(layout) = module_layout.try_borrow() {
                tray.refresh(&layout, agent_panel_handle.attention());
            }
        });
    }
    tray.refresh(&module_layout.borrow(), agent_panel_handle.attention());

    // --- A module takes the keys by keyboard: `Ctrl+h/j/k/l` (`move_focus`, below) and `prefix ;`/
    // `prefix o` (the prefix's `Action::SelectLast`/`SelectNext`) land the same way. A zoom ends
    // first (spec §3.4), as tmux's `select-pane` does. Arriving in the agent lands in BROWSE (panel
    // round 2 plan spec §8, decision 4: reverses 2026-09-19's "control l直接闪cursor"), on the last
    // row if the reader was following it, else where they left it (owner decision #22, 2026-09-29),
    // and `arrive` also drops a bypass prompt. A click on a row does not come through here and
    // still lands in BROWSE on that row. `false`: the module did not take the keys.
    let land_by_key: Rc<dyn Fn(&ModuleId) -> bool> = {
        let grid = grid.clone();
        let focus_module = focus_module.clone();
        let agent_panel_handle = agent_panel_handle.clone();
        Rc::new(move |to| {
            grid.unzoom();
            let grabbed = focus_module(to);
            if grabbed && to.kind() == ModuleKind::Agent {
                agent_panel_handle.arrive();
            }
            grabbed
        })
    };

    // --- Ctrl+h/j/k/l between modules, by geometry (modules design §6.2). One function for every
    // source: the editor's shim letters below, and each web module's capture-phase controller
    // below that. A move unzooms first, as tmux's `select-pane` does (spec §3.4); `Up` with nothing
    // above goes to the top bar, which is above every module; anything else with nothing there is
    // tmux's no-op at the edge of its grid, and `false` lets the key go on to the module -- which
    // is what the agent panel's `Ctrl+l`/`Ctrl+j` always did.
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

    // C1's stale-mirror recovery (spec §3.5): the page could not apply a `nav_key` against its own
    // current state (the mirror was a keystroke stale, or INPUT is refused on a dead session), so it
    // asks Rust to do exactly what the chord would have done.
    agent_panel_handle.on_nav_fallthrough({
        let move_focus = move_focus.clone();
        move |direction| {
            move_focus(&ModuleId::agent(), direction);
        }
    });

    // The prefix's own `Action::Tab` chord and the panel's own `tab_verb` messages (`<leader>`
    // bindings `keymap.ts` resolves, spec §10.2) run the same code, so a verb behaves the same
    // whichever route sent it (panel round 2 plan Task 6).
    let run_tab_action: Rc<dyn Fn(TabAction)> = {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let window = window.clone();
        let agent = agent_panel_handle.clone();
        let focus_module = focus_module.clone();
        let show_on_screen = show_on_screen.clone();
        let refused_name = top_bar.app_name.clone();
        let toast = toast.clone();
        Rc::new(move |tab_action: TabAction| {
            let chat = ModuleId::agent();
            let keys_in = pane_focus::focused_module(&window, &grid.hosts())
                .as_ref()
                .map(ModuleId::kind);
            // One terminal today; `n`/`p` there are swallowed (spec §3.4, D8).
            let plan = tab_verbs::plan(tab_action, keys_in, 1);
            if plan.takes_the_keys {
                if let Err(err) = show_on_screen(&chat) {
                    refuse(&refused_name, &toast, &err);
                    return;
                }
                focus_module(&chat);
            }
            let switched = match plan.verb {
                tab_verbs::TabVerb::New => {
                    agent.new_tab();
                    agent.enter_input();
                    true
                }
                tab_verbs::TabVerb::Step(delta) => agent.step(delta),
                tab_verbs::TabVerb::Last => agent.select_last(),
                tab_verbs::TabVerb::Select(n) => agent.select_number(n),
                tab_verbs::TabVerb::Rename => {
                    agent.begin_rename();
                    true
                }
                tab_verbs::TabVerb::Close => {
                    agent.confirm_close();
                    true
                }
                tab_verbs::TabVerb::CloseOthers => {
                    agent.confirm_close_others();
                    true
                }
                tab_verbs::TabVerb::Choose => {
                    agent.open_chooser();
                    true
                }
                tab_verbs::TabVerb::Info => {
                    agent.open_detail();
                    true
                }
                tab_verbs::TabVerb::Flash => false,
                tab_verbs::TabVerb::Nothing => return,
            };
            if !switched {
                // tmux: "can't find window" (ruling 10).
                flash(&refused_name);
                return;
            }
            // A switch from elsewhere shows a hidden chat where it was, keys unmoved;
            // under a zoom only the tray chip changes (spec §3.4).
            if !plan.takes_the_keys
                && eitri_core::tabs::reveal_on_switch(eitri_core::layout::agent_place(&module_layout.borrow()))
            {
                if let Err(err) = grid.show_module(&chat) {
                    eprintln!("[tabs] could not reveal the chat: {err}");
                }
            }
        })
    };
    // `<leader>bd`/`<leader>bo` etc., resolved by the panel's own sequence engine and sent as a
    // `tab_verb` message -- routed through the very code the prefix's `Action::Tab` arm runs.
    agent_panel_handle.on_tab_verb({
        let run_tab_action = run_tab_action.clone();
        move |tab_action| run_tab_action(tab_action)
    });

    // The prefix (keymap spec §2). Installed after every other window-level controller that exists
    // at startup, so it sees keys first; HINT's, added when a HINT starts, still comes before it.
    // Below `move_focus` because `select.*` uses it.
    {
        let grid = grid.clone();
        let module_layout = module_layout.clone();
        let editor = pane.clone();
        let agent = agent_panel_handle.clone();
        let window_for_focus = window.clone();
        let app_name = top_bar.app_name.clone();
        let refused_name = top_bar.app_name.clone();
        let text_size_controller = text_size_controller.clone();
        let terminal = terminal.clone();
        let focus_module = focus_module.clone();
        let open_module = open_module.clone();
        let place_and_arrive = place_and_arrive.clone();
        let show_on_screen = show_on_screen.clone();
        let move_focus = move_focus.clone();
        let land_by_key = land_by_key.clone();
        let focus_history = focus_history.clone();
        let hint_coordinator = hint_coordinator.clone();
        let window_modes = window_modes.clone();
        let keymap_for_send = keymap.clone();
        let strip_keys = module_keys.clone();
        let strip_verbs = keymap.strip_verbs();
        let strip_keymap = keymap.clone();
        let strip_layout = module_layout.clone();
        let strip_title = module_title.clone();
        let prefix_strip = prefix_strip.clone();
        let run_tab_action = run_tab_action.clone();
        let kill_now = kill_now.clone();
        let kill_prompt = kill_prompt.clone();
        let kill_title = module_title.clone();
        let kill_refused = top_bar.app_name.clone();
        let toast = toast.clone();
        prefix::install(
            &window,
            keymap.clone(),
            module_keys.clone(),
            move |command, keycode| {
                let action = match command {
                    prefix::PrefixCommand::Place { module, axis } => {
                        if let Err(err) = place_and_arrive(&module, axis) {
                            refuse(&refused_name, &toast, &err);
                        }
                        return;
                    }
                    prefix::PrefixCommand::Run(action) => action,
                };
                // The module that last held the keys: what zoom, resize, hide and swap act on.
                let target = || module_layout.borrow().focus().clone();
                // The module that holds the keys right now (`pane_focus`'s own definition).
                let focused = || pane_focus::focused_module(&window_for_focus, &grid.hosts());
                match action {
                    Action::Zoom => grid.toggle_zoom(&target()),
                    Action::Resize { dir, cells } => {
                        let cell = editor.cell_size().unwrap_or(layout::FALLBACK_CELL);
                        grid.resize(&target(), dir, layout::resize_px(dir, cell, cells));
                    }
                    Action::Select(dir) => {
                        move_focus(&target(), dir);
                    }
                    // tmux's `last-pane` and `select-pane -t :.+` (v1 picks, 2026-09-29), landing the
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
                                println!("[prefix] no last module");
                                flash(&refused_name);
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
                                println!("[prefix] no other module on screen");
                                flash(&refused_name);
                            }
                        }
                    }
                    Action::SendPrefix | Action::SendKeys(_) => {
                        let Some(key) = prefix::literal_for(&action, &keymap_for_send) else {
                            return;
                        };
                        match terminal::literal_target(focused().as_ref().map(ModuleId::kind)) {
                            terminal::LiteralTarget::Editor => editor.send_keys(&key.to_vim()),
                            terminal::LiteralTarget::Panel => agent.literal_key(&key),
                            terminal::LiteralTarget::Terminal => match terminal::keys::literal(&key) {
                                Some(inputs) => inputs.into_iter().for_each(|input| terminal.send(input)),
                                None => println!("[prefix] {key}: the terminal takes C-<letter> or one character"),
                            },
                            terminal::LiteralTarget::Neither => {}
                        }
                    }
                    // `Ctrl+a t`'s rule since the terminal's phase 1 (`terminal::toggle_action`), in the
                    // order `ToggleAction::steps` gives and the terminal's own tests pin.
                    Action::Module(id) if id.kind() == ModuleKind::Terminal => {
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
                                terminal::ToggleStep::Start => terminal.start(),
                                terminal::ToggleStep::FocusTerminal => {
                                    focus_module(&id);
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
                        // `v` before the canvas exists (ruling 2), or a module this window has not got.
                        if !grid.hosts().iter().any(|(m, _)| *m == id) {
                            println!("[prefix] {id}: not in this window yet");
                            flash(&refused_name);
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
                        let text = kill_pane::prompt(&id, &kill_title(&id), scope, running, queued);
                        // What `y` also answers for the window close, if this kill closes it.
                        let confirmed = eitri_core::tabs::window_close_prompt(running, queued);
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
                    Action::Even(axis) => grid.even_modules(axis),
                    Action::Text(change) => match text_size::route_focused_module(focused().as_ref()) {
                        text_size::TextSizeTarget::Editor => text_size_controller.apply_editor(change.into()),
                        text_size::TextSizeTarget::Panel => text_size_controller.apply_panel(change.into()),
                        text_size::TextSizeTarget::Neither => {}
                    },
                    Action::Hint => hint_coordinator.toggle_from_prefix(keycode),
                    Action::PanelReload => agent.reload_document_by_hand(),
                    Action::PanelKeymap => {
                        let chat = ModuleId::agent();
                        if let Err(err) = show_on_screen(&chat) {
                            refuse(&refused_name, &toast, &err);
                            return;
                        }
                        focus_module(&chat);
                        agent.open_keymap();
                    }
                    // `prefix :` (owner decision #28, K16): tmux's `command-prompt`. The chat comes up with the
                    // keys and its `:` line opens, which runs nothing -- so the letters typed after the chord
                    // land in a box instead of running as panel keys.
                    Action::PanelCommandLine => {
                        let chat = ModuleId::agent();
                        if let Err(err) = show_on_screen(&chat) {
                            refuse(&refused_name, &toast, &err);
                            return;
                        }
                        focus_module(&chat);
                        agent.open_command_line();
                    }
                    Action::WindowImmersive => window_modes.toggle_immersive(),
                    // Panel round 2 plan Task 6: the body that used to live here is `run_tab_action`,
                    // shared with the panel's own `tab_verb` messages (spec §10.2). Unchanged
                    // behaviour -- `run_tab_action` computes its own `keys_in` the same `focused()`
                    // does here.
                    Action::Tab(tab_action) => run_tab_action(tab_action),
                    // The prefix never hands a split key back: it waits for a module key instead.
                    Action::Split(_) => {}
                    // `prefix [`/`prefix PageUp` (spec §6.2): the terminal's own read-only copy
                    // mode, only while it holds the keys -- elsewhere the same refusal shape as
                    // every other misdirected prefix action (`Action::Module`, above).
                    Action::CopyMode { up } => {
                        if terminal::copy_mode_entry(focused().as_ref().map(ModuleId::kind)) {
                            terminal.enter_copy_mode(up);
                        } else {
                            println!("[prefix] prefix [ scrolls the terminal");
                            flash(&refused_name);
                        }
                    }
                }
            },
            move |waiting| {
                if waiting == prefix::Waiting::No {
                    app_name.remove_css_class("prefix-armed");
                } else {
                    app_name.add_css_class("prefix-armed");
                }
                let pieces = {
                    let layout = strip_layout.borrow();
                    // Armed, the keys that reach each module as the effective table binds them (a
                    // `del`ed `e` is not advertised); after a split key, the fixed module keys
                    // (ruling 8).
                    let entries = match waiting {
                        prefix::Waiting::Command => {
                            eitri_core::layout::strip_direct(&strip_keys, &strip_keymap, &layout)
                        }
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
            },
        );
    }

    // D11 A (spec §3.5): installed after `prefix::install` above, so GTK -- which runs a widget's
    // controllers most-recently-added first -- gives this one the keys before the prefix's own
    // capture controller, while the prompt is open.
    let close_prompt = close_prompt::ClosePrompt::install(
        &hint_overlay,
        &window,
        &top_bar.widget,
        top_bar.strip.upcast_ref::<gtk4::Widget>(),
    );
    let _ = kill_prompt.set(close_prompt.clone());
    // R1-2: the window close was declined, and nvim may already be gone -- it exited on its own
    // (`:q`, a crash) and that exit is what asked to close the window: the editor is retired then
    // (`editor_quit::EditorQuitting::window_close_declined`).
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
        pane_switch::listen(ps, move |message| match message {
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
                    println!("[modules] editor: a quit-cancelled letter for quit {g}, not the one in flight; ignored")
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
    // Only the agent host gets an intercept (C1, spec §3.1, §3.5): `Ctrl+j`/`Ctrl+k` claimed for the
    // composer, decided from the panel's own `panel_keys` mirror. A Lua panel has no such mode and
    // passes `None`, so every chord there still goes straight to `move_focus` as before.
    for (id, host) in grid
        .hosts()
        .iter()
        .filter(|(id, _)| matches!(id.kind(), ModuleKind::Agent | ModuleKind::LuaWebview))
    {
        let intercept: Option<Rc<dyn Fn(Direction) -> bool>> = if id.kind() == ModuleKind::Agent {
            let agent = agent_panel_handle.clone();
            Some(Rc::new(move |direction: Direction| {
                if !claims(agent.panel_keys(), direction) {
                    return false;
                }
                let direction = match direction {
                    Direction::Down => eitri_core::agent_bridge::NavKeyDirection::Down,
                    Direction::Up => eitri_core::agent_bridge::NavKeyDirection::Up,
                    Direction::Left | Direction::Right => {
                        unreachable!("claims() only claims Down (in Browse) or Up (in Input)")
                    }
                };
                agent.nav_key(direction);
                true
            }))
        } else {
            None
        };
        install_module_nav(id.clone(), host, move_focus.clone(), intercept);
        // No key with Super or Hyper held reaches the panel's page: WebKitGTK does not tell the
        // page, so `a`/`d`/`D` and the bypass `y` could not refuse it there (`panel_super.rs`).
        if id.kind() == ModuleKind::Agent {
            panel_super::install(host);
        }
    }

    // --- Ctrl+h/j/k/l in the terminal: Eitri's, always (owner, 2026-09-23: "neovibe的按键优先").
    // Capture phase on its own widget, like the web modules' controllers above, so the chord is
    // claimed before the terminal's key controller hands it to the shell. Unlike theirs, all four
    // are swallowed even with no module that way, so what the shell receives never depends on the
    // layout (bottom-terminal spec §5.4): `Ctrl+k` goes to the most recent module above it (the MRU
    // tie `neighbor()` breaks, which is `main`'s `last_upper_pane`), and the rest are tmux's no-op
    // at the edge unless a module is there. The literal forms the shell loses are `Ctrl+a Ctrl+a`
    // and `Ctrl+a Ctrl+l`, above; the rest are in the spec's remap table.
    {
        let move_focus = move_focus.clone();
        let controller = EventControllerKey::new();
        controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        controller.connect_key_pressed(move |_controller, key, _keycode, state| {
            match terminal::navigation(key, state) {
                Some(direction) => {
                    move_focus(&ModuleId::terminal(), direction);
                    glib::Propagation::Stop
                }
                None => glib::Propagation::Proceed,
            }
        });
        terminal.widget().add_controller(controller);
    }

    // --- Ctrl+h/j/k/l on an editor with no nvim in it (the Opus review's T7-1): the shell's, as from
    // every other module, since nothing inside the editor can resolve them then
    // (`editor_start_failure::navigation`, vim-tmux-navigator's own tmux-side rule). Capture phase on
    // the pane's widget, so it runs before the pane's own key controller, which swallows every key;
    // while nvim runs, nothing is claimed and nvim's navigator decides, as before.
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
    {
        let top_items = top_items.clone();
        let return_to_pane = return_to_pane.clone();
        let window = window.clone();
        let hint_coordinator = hint_coordinator.clone();
        let controller = EventControllerKey::new();
        controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        controller.connect_key_pressed(move |_controller, key, _keycode, state| {
            let ctrl = state.contains(ModifierType::CONTROL_MASK);
            if (key == Key::j && ctrl) || key == Key::Escape {
                return_to_pane();
                return glib::Propagation::Stop;
            }
            if ctrl {
                return glib::Propagation::Proceed;
            }
            // `f` here starts the global HINT, as it does in the panel's BROWSE (spec §2.1).
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
            let focus = gtk4::prelude::GtkWindowExt::focus(&window);
            let current = top_items.iter().position(|item| Some(item) == focus.as_ref());
            let visible: Vec<bool> = top_items.iter().map(|item| item.is_visible()).collect();
            if let Some(next) = chrome::step_item(&visible, current, step) {
                top_items[next].grab_focus();
            }
            glib::Propagation::Stop
        });
        top_bar.widget.add_controller(controller);
    }

    // nvim exiting on its own (e.g. `:qa!`) has no window to close by itself -- ask this host's
    // window to close, which in turn drives the connect_close_request handler below (mirrors
    // neovide-editor's own `examples/standalone.rs`), unless nvim was asked to quit: then what it was
    // asked for (`editor_quit::EditorQuitting::nvim_exited`).
    {
        let editor_quitting_for_exit = editor_quitting.clone();
        pane.on_exited_unrequested(move || editor_quitting_for_exit.nvim_exited());
        let editor_quitting = editor_quitting.clone();
        pane.on_nvim_unreachable(move || editor_quitting.nvim_unreachable());
    }

    // R1-4: nvim failed to start (missing, or older than the fork's floor) -- say why in the
    // editor's own area (`editor_start_failure`) instead of leaving an unexplained dark-red block.
    // Construction is asynchronous (`neovide_editor::LiveState`'s own doc: `NotStarted` ->
    // `Starting` -> `Ready`/`Failed` happens across render callbacks, after the window is already
    // shown and the initial focus already chosen), so the editor can hold the keys when this fires.
    // A pane that cannot accept input must not keep holding them, with only a mouse click or the
    // invisible prefix as a way out -- move the keys to the chat (`editor_start_failure::
    // keys_after_failure`), the same fallback the declined-close handler above uses for "the editor
    // is unusable right now". Reached again later, it lets `Ctrl+h/j/k/l` out (below).
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

    // `eitri.layout.show/hide/focus/split` from a command or an event handler (modules P2, spec
    // §4.5): queued by the Lua half while the handler runs, carried out here once it returns, so no
    // layout change happens under a Lua call. Each refusal is logged, as a handler's own error is.
    let apply_layout_requests: Rc<dyn Fn()> = {
        let lua_engine = lua_engine.clone();
        let grid = grid.clone();
        let focus_module = focus_module.clone();
        let focus_and_arrive = focus_and_arrive.clone();
        let show_on_screen = show_on_screen.clone();
        let place_and_arrive = place_and_arrive.clone();
        Rc::new(move || {
            let requests = lua_engine.layout.borrow_mut().take_requests();
            for request in requests {
                use eitri_core::lua::layout::LayoutRequest;
                println!("[lua] layout: {request:?}");
                // The same three helpers `Ctrl+a` uses, so the two cannot drift apart.
                let result = match &request {
                    LayoutRequest::Show(id) => show_on_screen(id),
                    LayoutRequest::Hide(id) => grid.hide_module(id, &*focus_module),
                    LayoutRequest::Focus(id) => focus_and_arrive(id),
                    LayoutRequest::Split(id, axis) => place_and_arrive(id, *axis),
                };
                if let Err(err) = result {
                    eprintln!("[lua] layout: {request:?} refused: {err}");
                }
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
    // grab_focus() after present(), matching standalone.rs's own
    // `window.present(); pane.grab_focus();` ordering -- focusing a not-yet-shown widget is
    // meaningless. And arrived at, as `Ctrl+a a` arrives (the whole-branch review's window finding
    // 6): a project whose file has the keys in the chat reopens with them there the way every other
    // keyboard route lands. At launch the panel has no session yet -- it is on its start screen, and
    // its page may not have loaded -- so today `enter_input` is refused there and this changes
    // nothing on screen; it is written so that a launch that resumes a session by itself lands in
    // the composer, or on a waiting card, without anyone having to remember this line.
    if focus_module(&first) {
        arrive(&first);
    }

    // The one real v1 event: fires once the window is actually up, so any Lua handler reacting
    // to it sees a fully-built shell (panels registered, commands bound, window shown).
    // `eitri.layout.show` and the rest work from here on, `shell:ready` handlers included.
    lua_engine.layout.borrow_mut().accept_requests();
    lua_engine.emit("shell:ready");
    apply_layout_requests();

    // Polls for a cross-window "come to the front" request from `eitri-supervisor` -- e.g. the
    // dashboard's own UI, or another shell instance, asking this window to raise itself. Cloning
    // `agent_panel_handle` here (rather than after) is load-bearing: the `connect_close_request`
    // closure below takes ownership of the original `agent_panel_handle` by move, so anything
    // that still needs it afterward -- this timer included -- must clone it first, the same
    // pattern `lua_engine`'s own comment two blocks up documents for itself.
    {
        let window = window.clone();
        let agent_panel_handle_for_poll = agent_panel_handle.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
            if agent_panel_handle_for_poll.poll_activate() {
                window.present();
            }
            glib::ControlFlow::Continue
        });
    }

    // Registered last: this is `pane`'s final use in this function, so its `Rc` can be moved in
    // outright rather than cloned again. (`pane` is an `Rc<NeovideEditorPane>` because the side
    // panel's Ctrl+h handler above needs it too; `NeovideEditorPane` doesn't derive `Clone`, and
    // every use here only ever needs `&self` methods.) Once nvim is gone, the same
    // connect_close_request -> shutdown() -> glib::Propagation::Proceed pattern as standalone.rs.
    let app_for_close = app.clone();
    window.connect_close_request(move |_window| {
        // D11 A (spec §3.5), then R3 (v1 hardening Task 6): a running tab is worth one y/n, and then
        // nvim decides through its own `:confirm qall` -- the route `prefix x` on the editor takes --
        // and the window closes when nvim exits (`editor_quit::EditorQuitting::window_close`).
        // `pane.shutdown()` below runs the fork's `:qa!`, which discards unsaved buffers and deletes
        // their swap files, so this is reached only once nvim is gone or never ran -- never for an
        // nvim that could not be killed (T6-1: the close is refused instead).
        if editor_quitting.window_close() == editor_quit::WindowClose::Wait {
            return glib::Propagation::Stop;
        }
        // First, and synchronously (spec §4.6): the debounce may still be waiting on an arrangement
        // change. With none unwritten this writes nothing -- a click never does (`layout_state`'s
        // module doc says what is written when).
        layout_saver.save_now();
        // Kills nvim by its pid if it does not exit within the fork's wait (codex p1 #2).
        pane.shutdown();
        // Hangs the terminal's shell up without waiting for it (`TerminalSession`'s `Drop`). Should
        // the process exit first, the kernel closing the PTY master hangs it up anyway.
        terminal.shutdown();
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
        // The nvim-keys feed, for the same two reasons (panel round 2's whole-branch review): left a
        // local of `build_ui`, its `Drop` removed `k.sock` and `nvim_keys.lua` the moment this
        // function returned, and nvim's mappings, leader and `timeoutlen` never reached the panel.
        if let Some(feed) = &nvim_keys_feed {
            feed.cleanup();
        }
        // Shuts down whatever `AgentSession` the agent panel started (a no-op if the user never
        // left the mode-selector screen) -- without this, a normal window close never runs
        // `AgentSession::shutdown()` at all: the `Rc<RefCell<AgentPanelState>>` that owns the
        // session is otherwise only reachable from closures internal to agent_panel.rs, none of
        // which run on window close on their own. See `AgentPanelHandle`'s own doc for the full
        // consequences (settings backup restore, hook socket cleanup, child SIGTERM) this closes.
        // `app` travels in because the panel does not block this thread waiting for its children:
        // it takes a `gio` application hold instead, so THE PANEL'S HALF returns at once and the
        // process stays alive with no window on screen until the teardown reports. See
        // `AgentPanelHandle::shutdown`.
        //
        // Not the whole handler, and the distinction matters to anyone debugging a slow close:
        // `pane.shutdown()` above runs FIRST and on this thread, and it spins until nvim exits
        // (`LiveHarness::shutdown` in the fork). Since v1 hardening Task 6 nvim has exited before it
        // runs (the close waits for nvim's own `:confirm qall`, and an nvim that cannot be killed
        // refuses the close rather than reaching here), so it returns at once. Named because three
        // consecutive reviews of this path died on a comment that over-claimed.
        agent_panel_handle.shutdown(&app_for_close);
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

/// The top bar's app name shows the prefix indicator's block for [`REFUSAL_FLASH`] (spec §3.2).
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

/// [`editor_quit::QuitHost`] over the real window: one GTK call per method, and the only part of the
/// editor's quit flow no test reaches (`editor_quit`'s module doc; the GUI checklist covers it).
struct ShellQuitHost {
    pane: Rc<NeovideEditorPane>,
    module_layout: Rc<RefCell<eitri_core::layout::Layout>>,
    grid: glib::WeakRef<ModuleGrid>,
    show_on_screen: Rc<dyn Fn(&ModuleId) -> Result<(), LayoutError>>,
    focus_module: Rc<dyn Fn(&ModuleId) -> bool>,
    retire_editor: Rc<dyn Fn() -> Result<(), LayoutError>>,
    /// The window's y/n, set once it is installed (after the prefix, so its controller sees keys
    /// first); every call before that finds none.
    prompt: Rc<std::cell::OnceCell<Rc<close_prompt::ClosePrompt>>>,
    agent: agent_panel::AgentPanelHandle,
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
        (self.show_on_screen)(&ModuleId::editor())
    }
    fn focus_editor(&self) {
        (self.focus_module)(&ModuleId::editor());
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
        eitri_core::tabs::window_close_prompt(self.agent.running_count(), self.agent.queued_count())
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
        flash(&self.app_name);
        self.toast.show(text);
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

/// P8's toast text (spec §7.2): the one refusal that will keep recurring for the rest of this
/// window's life, because [`Reopen::Never`] never lifts. `Some` only for `LayoutError::Gone` on the
/// editor itself -- the one module this window ever marks gone -- so a future second use of `Gone`
/// on some other module does not inherit a message that names the editor by name.
fn gone_editor_toast_text(err: &LayoutError) -> Option<String> {
    match err {
        LayoutError::Gone(id) if *id == ModuleId::editor() => Some(editor_quit::RETIRED_EDITOR_TEXT.to_string()),
        _ => None,
    }
}

/// A layout verb the layout refused -- the last module on screen hidden, a module placed next to
/// itself: said on stdout, and the top bar's app name shows the prefix indicator's block for
/// [`REFUSAL_FLASH`] (spec §3.2). Its own class, so a prefix armed meanwhile keeps its indicator. A
/// second refusal inside the flash restarts it rather than being cut short by the first one's timer
/// (Task 9's review, minor 5). One window per process (`NON_UNIQUE`), so one timer per thread.
/// **P8 (spec §7.2):** a killed editor's `Gone` also reaches the window's toast (already installed
/// in `build_ui`), not only the flash and stdout line every other refusal keeps.
fn refuse(app_name: &gtk4::Label, toast: &Rc<toast::Toast>, err: &LayoutError) {
    println!("[modules] refused: {err}");
    flash(app_name);
    if let Some(text) = gone_editor_toast_text(err) {
        toast.show(&text);
    }
}

/// C1's decision, in Rust (spec §3.5): only `Ctrl+j` (`Direction::Down`) while the panel's
/// `panel_keys` mirror reports `Browse`, or `Ctrl+k` (`Direction::Up`) while it reports `Input`, is
/// claimed for the composer -- every other combination goes to `move_focus` exactly as before. Pure
/// so the full table is a unit test with no display; `install_module_nav`'s agent-only `intercept`
/// closure is the only caller.
fn claims(mirror: eitri_core::agent_bridge::PanelKeys, dir: Direction) -> bool {
    use eitri_core::agent_bridge::PanelKeys;
    matches!(
        (dir, mirror),
        (Direction::Down, PanelKeys::Browse) | (Direction::Up, PanelKeys::Input)
    )
}

/// `Ctrl+h/j/k/l` from a web module (the agent panel, a Lua panel), on its host: a capture-phase
/// controller, because `vim-tmux-navigator` lives inside Neovim, which is not the focused widget
/// here, so nothing would ever run; capture, not bubble, because a `WebView` handles key events
/// itself and would otherwise consume the chord before a bubble-phase controller on the same widget
/// ran. Only a chord that moves is claimed. Called once per web module host, at startup and for one
/// added later (modules P2; P3's canvas is the first).
///
/// `intercept` (C1, spec §3.5) is consulted after the `LetThrough` second-delivery check and before
/// `move_focus`: a chord it claims stops right there, never reaching `move_focus` or the page. Only
/// the agent host's caller passes one; a Lua panel passes `None`, so it behaves exactly as before
/// this parameter existed.
fn install_module_nav(
    id: ModuleId,
    host: &gtk4::Widget,
    move_focus: Rc<dyn Fn(&ModuleId, Direction) -> bool>,
    intercept: Option<Rc<dyn Fn(Direction) -> bool>>,
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
        if move_focus(&id, direction) {
            return glib::Propagation::Stop;
        }
        if let Some(event) = event {
            let_through.borrow_mut().let_through(event);
        }
        glib::Propagation::Proceed
    });
    host.add_controller(controller);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A guard on one builder line, and nothing more than that.
    ///
    /// It asserts only that `build_application` still sets the flag -- it does not launch a second
    /// `shell`, does not exercise gio's registration at all, and would still pass if `build_ui`
    /// were wired to run twice by some other route. That narrowness is the point: `NON_UNIQUE` has
    /// no locally observable effect, so deleting the line breaks nothing here and silently
    /// restores gio's single-instance behaviour, with the damage (`build_ui` running twice in one
    /// process) showing up only when someone launches a second window by hand.
    ///
    /// **That "two launches are two processes" claim is therefore asserted from gio's documented
    /// semantics, not demonstrated here.** Demonstrating it needs two real `shell` processes in a
    /// sandbox; that check is listed, unrun, in `shell/MANUAL_VERIFICATION.md`'s
    /// "Session isolation (A1/A5/A4)" section. See `build_application`'s own doc for the cost.
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

    /// Every `.rs` file under `src/`, with its `#[cfg(test)]` module and its line comments cut
    /// off -- so this test's own copies of the accelerators cannot satisfy it.
    fn sources(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).expect("shell/src is readable") {
            let path = entry.expect("a readable entry").path();
            if path.is_dir() {
                sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).expect("a readable source file");
                let code = text.split("#[cfg(test)]").next().unwrap_or_default();
                let code: String = code
                    .lines()
                    .map(|line| line.split("//").next().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("\n");
                out.push((path.display().to_string(), code));
            }
        }
    }

    /// Every accelerator-shaped string literal in `shell/src`, cut from `.rs` files by
    /// [`sources`]/[`is_accel`]. Shared, so the two tests below scan the same tree the same way.
    fn found_accelerators() -> Vec<(String, String)> {
        let mut files = Vec::new();
        sources(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        let mut found = Vec::new();
        for (name, code) in &files {
            for (index, chunk) in code.split('"').enumerate() {
                if index % 2 == 1 && is_accel(chunk) {
                    found.push((name.clone(), chunk.to_string()));
                }
            }
        }
        found
    }

    /// Spec §2.9: every accelerator is registered from `eitri_core::keymap::root`, so an
    /// accelerator-shaped literal anywhere in `shell/src` is one registered around the keymap --
    /// and one the no-`Ctrl+Shift` check in core never saw. `is_accel` is exercised directly, so
    /// this cannot pass because the scanner went blind.
    #[test]
    fn shell_src_writes_no_accelerator_literal() {
        let mut files = Vec::new();
        sources(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        assert!(
            files.len() >= 20,
            "only {} source files -- the walk is broken",
            files.len()
        );
        assert!(is_accel("<Control><Shift>f") && is_accel("F11") && is_accel("<Control>KP_Add"));
        assert!(!is_accel("<C-b>") && !is_accel("Ctrl+b"));
        assert_eq!(
            found_accelerators(),
            Vec::<(String, String)>::new(),
            "register it through eitri_core::keymap::root instead"
        );
    }

    /// Panel round 2 plan Task 6, spec §8: every keyboard arrival lands in BROWSE now, except the
    /// new-tab path, which still opens the composer -- an empty tab has nothing to browse.
    #[test]
    fn keyboard_arrival_is_browse_and_only_a_new_tab_types() {
        // Split literals, so this test's own text is not counted.
        let src = include_str!("main.rs");
        let calls = src.matches(concat!(".enter", "_input()")).count();
        assert_eq!(calls, 1, "only the new-tab path may land in INPUT (spec 2026-09-26 §8)");
        assert!(
            src.matches(concat!(".arr", "ive()")).count() >= 2,
            "move_focus and arrive both use it"
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

    /// C1's full decision table (spec §3.5): the two claimed cells (`Down`+`Browse`, `Up`+`Input`)
    /// and every other combination, which must fall through to `move_focus` unclaimed.
    #[test]
    fn claims_only_down_in_browse_and_up_in_input() {
        use eitri_core::agent_bridge::PanelKeys;
        for dir in [Direction::Left, Direction::Down, Direction::Up, Direction::Right] {
            for mirror in [PanelKeys::Browse, PanelKeys::Input, PanelKeys::Other] {
                let expected = matches!(
                    (dir, mirror),
                    (Direction::Down, PanelKeys::Browse) | (Direction::Up, PanelKeys::Input)
                );
                assert_eq!(claims(mirror, dir), expected, "{dir:?} + {mirror:?}");
            }
        }
    }

    /// P8 (spec §7.2): `refuse` shows this text in the window's toast for a killed editor, and
    /// says nothing extra for every other refusal (still just the flash and the stdout line).
    #[test]
    /// P8's toast, reworded by the Opus review's T6-5: the spec's "the editor was closed (:qa or
    /// prefix x)" was shown for a crash too, and a relaunch now does bring the editor back.
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
}
