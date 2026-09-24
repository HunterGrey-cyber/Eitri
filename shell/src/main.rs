//! shell: the real neovibe product window -- custom chrome, the module grid (a layout that is data:
//! docs/superpowers/specs/2026-09-23-modules-and-canvas-design.md), the neovide-editor crate
//! embedded as the real editor pane, and the real agent-ui panel (a WebView-hosted frontend bridged
//! to a lazily-started `agent::AgentSession`). See
//! docs/superpowers/specs/2026-09-06-shell-scaffolding-design.md.

mod agent_panel;
mod chrome;
mod editor_context;
mod hint;
mod layout;
mod layout_state;
mod lua;
mod module_grid;
mod pane_focus;
mod pane_switch;
mod prefix;
mod prefix_strip;
mod supervisor_client;
mod terminal;
mod terminal_handoff;
mod text_size;
mod theme;
mod toast;
mod tray;
mod window_mode;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::gdk::{Key, ModifierType};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, EventControllerKey};

use lua::{LuaEngine, PanelSlot};
use module_grid::{HostKind, ModuleGrid};
use neovibe_core::layout::{
    Axis, Direction, KeyAction, LayoutError, ModuleDecl, ModuleId, ModuleKeys, ModuleKind, Nav,
};
use neovide_editor::{NeovideEditorPane, NeovideEditorPaneOptions};

const APP_ID: &str = "cn.huntergrey.neovibe";

/// How long the top bar's app name shows the prefix indicator's block when a layout verb is refused
/// (spec §3.2: "The refusal flashes the top bar, the same way the prefix indicator shows").
const REFUSAL_FLASH: std::time::Duration = std::time::Duration::from_millis(200);

/// `NEOVIBE_CONFIG_DIR` overrides the config directory (used by sandboxed/manual verification
/// runs so they don't touch a real `~/.config/neovibe`); otherwise the real per-user config dir.
fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("NEOVIBE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").expect("HOME must be set");
    PathBuf::from(home).join(".config").join("neovibe")
}

fn main() -> glib::ExitCode {
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
    // real embedding host's actual config.
    //
    // `args_os`, not `args`, throughout this function: `std::env::args()` panics on an argument
    // that is not valid UTF-8, and `shell <dir>` makes a path a supported argument -- see
    // `neovibe_core::project_root`'s own module doc. A flag match is a byte-for-byte comparison
    // either way.
    //
    // Every flag matched here must also appear in `neovibe_core::project_root`'s own (private)
    // `KNOWN_FLAGS` list, which is the only place that can tell a flag from a project directory;
    // one missing from that list makes passing it a hard startup failure rather than a silently
    // wrong project root.
    let want_clean = std::env::args_os().any(|arg| arg == "--clean");

    // Resolved once, here, and then carried as a value into every pane that needs it -- see
    // `neovibe_core::project_root`'s own module doc for why three separate `current_dir()` reads
    // were one process-global too many.
    let project_root = match neovibe_core::project_root::resolve() {
        Ok(root) => root,
        Err(message) => {
            eprintln!("neovibe: {message}");
            return glib::ExitCode::FAILURE;
        }
    };
    println!("neovibe: project root {}", project_root.display());

    let app = build_application();
    app.connect_activate(move |app| build_ui(app, want_clean, &project_root));
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
/// `supervisor/src/bin/neovibe_supervisor.rs` solves the same collision the opposite way, with an
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

fn build_ui(app: &Application, want_clean: bool, project_root: &Path) {
    // Painted with the built-in fallback until the embedded nvim sends its first snapshot.
    let theme_css = theme::gtk_css::ThemeCss::install(&neovibe_core::theme::ThemeTokens::fallback());
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
    let mut nvim_child_env = pane_switch.as_ref().map(|ps| ps.child_env()).unwrap_or_default();
    nvim_child_env.extend(theme_feed.as_ref().map(|feed| feed.child_env()).unwrap_or_default());
    nvim_child_env.extend(context_feed.as_ref().map(|feed| feed.child_env()).unwrap_or_default());
    let mut nvim_extra_args = theme_feed.as_ref().map(|feed| feed.nvim_args()).unwrap_or_default();
    nvim_extra_args.extend(context_feed.as_ref().map(|feed| feed.nvim_args()).unwrap_or_default());
    // wire 3. Unconditional and stateless -- no socket, no directory, nothing to fail at startup --
    // because the only reload trigger a normal Neovim config installs is `FocusGained`, and nothing
    // in this shell ever tells nvim it lost or gained focus. Without this a `git checkout`, a
    // formatter, or an edit made anywhere else never reaches the buffer.
    nvim_extra_args.extend(neovibe_core::buffer_reload::nvim_args());

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
        Some(feed) => editor_context::listen(feed),
        None => std::rc::Rc::new(|| None),
    };
    let (agent_widget, agent_panel_handle) =
        agent_panel::build_agent_panel(project_root.to_path_buf(), editor_context_source);

    lua_engine.load_init_file(&config_dir.join("init.lua"));

    // Which local Claude account this window spends. Two sources, and the environment wins:
    // `neovibe --account <name>` (and this host's own `VERDANDI_CLAUDE_ACCOUNT`, exported for
    // Verdandi and inherited by every `neovibe` started from a terminal) arrives as that variable,
    // and `init.lua`'s `neovibe.config.set("agent.account", "<name>")` is the per-machine default
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
        None => neovibe_core::theme::DEFAULT_PANEL_FONT_SIZE_PX,
        Some(raw) => match raw.trim().parse::<f32>() {
            Ok(px) if neovibe_core::theme::PANEL_FONT_SIZE_RANGE_PX.contains(&px) => {
                eprintln!("[panel] font size {px}px (init.lua's agent.font_size)");
                px
            }
            Ok(px) => {
                eprintln!(
                    "neovibe: neovibe.config.set(\"agent.font_size\", {raw:?}): {px} is outside {:?}",
                    neovibe_core::theme::PANEL_FONT_SIZE_RANGE_PX
                );
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("neovibe: neovibe.config.set(\"agent.font_size\", {raw:?}): not a number ({e})");
                std::process::exit(1);
            }
        },
    };

    // What a card for a hidden chat does (modules P2, spec §3.3, decision b): the tray's chip and a
    // toast, or with `reveal` the chat itself. Anything but `badge`/`reveal` is a startup failure
    // naming the key, like `agent.font_size` above.
    let on_permission = match neovibe_core::attention::ChatOnPermission::parse(
        lua_engine
            .config
            .borrow()
            .get(neovibe_core::attention::ChatOnPermission::KEY),
    ) {
        Ok(policy) => policy,
        Err(message) => {
            eprintln!("neovibe: {message}");
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
            eprintln!("neovibe: the configured claude account is unusable: {err}");
            std::process::exit(1);
        }
    }

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
    let module_keys = match ModuleKeys::build(&lua_keys) {
        Ok(keys) => Rc::new(keys),
        Err(err) => {
            eprintln!("neovibe: {err}");
            std::process::exit(1);
        }
    };
    // The layout this window opens with (modules P2, spec §4.4, §4.6): this project's state file if
    // it can be used, else `init.lua`'s `neovibe.layout.default`, else the first launch. A malformed
    // default is a startup failure naming the call; a state file that cannot be used is not, since it
    // is state rather than config -- it is logged and the default opens.
    let lua_default = match lua_engine.layout.borrow().default_tree().cloned() {
        Some(Ok(tree)) => Some(tree),
        Some(Err(message)) => {
            eprintln!("neovibe: {message}");
            std::process::exit(1);
        }
        None => None,
    };
    let state_dir = neovibe_core::layout::persist::state_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    let loaded = state_dir
        .as_deref()
        .map(|dir| neovibe_core::layout::persist::load(dir, project_root, &decls));
    let module_layout = match layout_state::choose_startup_layout(lua_default.as_ref(), loaded, &decls) {
        Ok((layout, notes)) => {
            for note in notes {
                println!("[layout] {note}");
            }
            Rc::new(RefCell::new(layout))
        }
        Err(message) => {
            eprintln!("neovibe: {message}");
            std::process::exit(1);
        }
    };
    let layout_saver = layout_state::LayoutSaver::new(state_dir, project_root, module_layout.clone(), decls.clone());
    // Every module's host, added once and never reparented (`module_grid`'s module doc).
    let grid = ModuleGrid::new(module_layout.clone());
    grid.add(ModuleId::editor(), pane.widget(), HostKind::Direct);
    grid.add(ModuleId::agent(), &agent_widget, HostKind::Web);
    // The floor `build_vertical_split` gave the bottom slot on `main`: a terminal dragged to zero
    // reports a 1-row grid to its shell.
    terminal.widget().set_size_request(-1, layout::BOTTOM_MIN_HEIGHT);
    grid.add(ModuleId::terminal(), terminal.widget(), HostKind::Direct);
    for (id, slot, widget) in &lua_panels {
        if *slot == PanelSlot::Bottom {
            widget.set_size_request(-1, layout::BOTTOM_MIN_HEIGHT);
        }
        grid.add(id.clone(), widget, HostKind::Web);
    }
    // The terminal's shell starts the first time it is shown, however it got there -- `Ctrl+a t`,
    // `Ctrl+a \ t`, its tray chip, `neovibe.layout.show`, a saved layout -- rather than only on the
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
    // tells the input method; any other module is its host widget's own `grab_focus`. A hidden
    // module is refused: GTK4's `grab_focus` does not refuse an unmapped widget, so the keys would
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
            grid.hosts()
                .iter()
                .find(|(m, _)| m == id)
                .is_some_and(|(_, host)| host.grab_focus())
        })
    };

    let window = ApplicationWindow::builder()
        .application(app)
        .title("neovibe")
        .default_width(1280)
        .default_height(760)
        // No HeaderBar: decorated(false) suppresses GTK's own CSD titlebar entirely -- the
        // custom top bar built below (chrome::build_top_bar) is the only titlebar.
        .decorated(false)
        .build();
    window.add_css_class("shell-root");

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.add_css_class("shell-root");

    // `app.reload-agent-panel` + Ctrl+Shift+R; the top bar's own `⟳` button points at the same
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
    let editor_clear = |tokens: &neovibe_core::theme::ThemeTokens| {
        let bg = tokens.bg;
        (bg.r, bg.g, bg.b)
    };
    // The panel's LIVE size (zoom-together design, spec §5's "L3" bullet): starts at the base and
    // is kept current by `text_size_controller` below. `panel_tokens` reads this rather than the
    // captured `panel_font_size` so a colorscheme change re-derives every OTHER token but never
    // resets a zoom back to the un-zoomed base.
    let panel_px = Rc::new(std::cell::Cell::new(panel_font_size));
    // One helper, so the startup theme and every later one cannot disagree about the size. The
    // editor's clear colour and the GTK chrome do not take it: it is the panel's text, not the
    // window's.
    let panel_tokens = {
        let panel_px = panel_px.clone();
        move |payload: Option<&neovibe_core::theme::payload::NvimThemePayload>| {
            let mut tokens = match payload {
                Some(p) => neovibe_core::theme::ThemeTokens::derive(p),
                None => neovibe_core::theme::ThemeTokens::fallback(),
            };
            tokens.font_size_px = text_size::live_panel_font_size_px(&panel_px);
            // known limit (item 3g, 2026-09-23): `live_panel_font_size_px` is unit-tested in
            // `text_size.rs`, but nothing here stops a future edit from replacing this call with
            // `panel_font_size` (the captured startup base) directly -- that compiles and no test
            // catches it, since this closure is GTK wiring with no headless harness. A GUI pass
            // (change the colorscheme after zooming; the panel must stay zoomed) is what would.
            tokens
        }
    };
    agent_panel_handle.set_theme(&panel_tokens(None));
    pane.set_clear_color(editor_clear(&neovibe_core::theme::ThemeTokens::fallback()));
    terminal.set_colors(terminal::colors_from(&neovibe_core::theme::ThemeTokens::fallback()));
    if let Some(feed) = theme_feed.as_mut() {
        let theme_css = theme_css.clone();
        // What the stylesheet paints, for the widgets GTK leaves on the old one (`theme::restyle`).
        let window_for_theme = window.clone();
        let agent_panel_handle = agent_panel_handle.clone();
        let pane_for_theme = pane.clone();
        let terminal = terminal.clone();
        theme::feed::listen(feed, move |payload| {
            let tokens = neovibe_core::theme::ThemeTokens::derive(&payload);
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
    // triggers reach the same toggle: `Ctrl+Shift+F` from anywhere (the `app.hint` accelerator,
    // which GTK fires from the window's capture-phase `gtk-shortcut-manager-capture` controller,
    // before the editor or the panel see the key -- not from `gtk-application-shortcuts`, which is
    // GLOBAL-scoped and handles nothing itself; see `hint`'s module doc), `f` on the top bar
    // (below), and `f` in the panel's BROWSE (the panel's `hint_request`).
    let hint_coordinator = hint::HintCoordinator::new(hint::HintWidgets {
        window: window.clone(),
        overlay: hint_overlay.clone(),
        top_items: top_items.clone(),
        editor: pane.clone(),
        agent_widget: agent_widget.clone(),
        modules: {
            let grid = grid.clone();
            Rc::new(move || grid.hosts_in_tree_order())
        },
        focus_module: focus_module.clone(),
        agent: agent_panel_handle.clone(),
    });
    {
        let coordinator = hint_coordinator.clone();
        agent_panel_handle.on_hint(move |message| coordinator.on_panel(message));
    }
    {
        let action = gtk4::gio::SimpleAction::new("hint", None);
        let coordinator = hint_coordinator.clone();
        action.connect_activate(move |_, _| coordinator.toggle_from_chord());
        app.add_action(&action);
        app.set_accels_for_action("app.hint", &["<Control><Shift>f"]);
    }

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
        pane_focus::install(
            &window,
            grid.live_hosts(),
            move |id| match module_layout.try_borrow_mut() {
                Ok(mut layout) => {
                    // Refused for a hidden module (`Layout::set_focus`): GTK can put focus in an
                    // unmapped widget, and the layout's `focus` -- what `Ctrl+j` from the top bar
                    // returns to and the prefix acts on -- must stay on one that can be seen.
                    if let Err(err) = layout.set_focus(id) {
                        eprintln!("[pane_focus] {err}; the layout keeps {}", layout.focus());
                    }
                }
                // Every writer of the layout releases it before touching focus (`ModuleGrid::apply`,
                // `hide_module`); this line is how a new one that does not would show up.
                Err(_) => eprintln!("[pane_focus] BUG: the layout was borrowed when focus moved to {id}"),
            },
            move |id, has_keys| {
                println!("[pane_focus] {id} has_keys={has_keys}");
                match id.kind() {
                    ModuleKind::Editor => editor.set_focused(has_keys),
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

    // F11 fullscreen, Ctrl+Shift+F11 immersive, both following g:neovide_fullscreen (spec
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
    // card if one waits (spec §3.3, `focus_permission`), else "I want to type" (owner, 2026-09-19),
    // as every keyboard arrival there does (`move_focus`).
    let arrive: Rc<dyn Fn(&ModuleId)> = {
        let agent_panel_handle = agent_panel_handle.clone();
        Rc::new(move |id| {
            if id.kind() == ModuleKind::Agent {
                if agent_panel_handle.attention().pending > 0 {
                    agent_panel_handle.focus_permission();
                } else {
                    agent_panel_handle.enter_input();
                }
            }
        })
    };

    // A card that arrives while the chat is not on screen (spec §3.3, decision b): a toast -- the
    // only sign in Immersive mode -- or, with `modules.chat.on_permission = reveal` and nothing
    // zoomed, the chat back where it was, the keys staying where they are.
    {
        let module_layout = module_layout.clone();
        let grid = grid.clone();
        let toast = toast.clone();
        let tray = tray.clone();
        agent_panel_handle.on_attention(move |before, after| {
            let place = neovibe_core::layout::agent_place(&module_layout.borrow());
            let reaction = neovibe_core::attention::react(on_permission, before, after, place);
            println!("[attention] agent {after:?} ({place:?}) -> {reaction:?}");
            tray.refresh(&module_layout.borrow(), after);
            if reaction.reveal {
                if let Err(err) = grid.show_module(&ModuleId::agent()) {
                    eprintln!("[attention] could not reveal the chat: {err}");
                }
            }
            if reaction.toast {
                toast.show(&toast::permission_toast_text(after));
            }
        });
    }
    // The three ways a verb brings a module to the user, each written once and shared by every route
    // that does it -- `Ctrl+a <key>` and its tray chip (`open_module` below), `Ctrl+a \`/`"`, and
    // `neovibe.layout.*` -- so a change to one reaches them all (the whole-branch review's finding 7:
    // three inlined copies had grown). Each returns the layout's refusal for its caller to report:
    // the prefix and a chip flash the app name, a Lua call logs it.
    //
    // A zoom ends only when it would keep the module off screen (`Layout::zoom_hides`): `Ctrl+a a`,
    // a chip, or `neovibe.layout.focus` onto the zoomed chat itself leaves it zoomed, as tmux's
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
    // (`neovibe.layout.show`, and the first half of `Ctrl+a <key>` on a hidden module). A zoom that
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
    // `place_and_arrive`: `id` into a new split after the module with the keys, along `axis`, moved
    // there if it is elsewhere, and the keys to it (`Ctrl+a \`/`"` + a key, `Ctrl+a <key>` for a
    // module never placed, `neovibe.layout.split`).
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

    // `Ctrl+a <key>`'s four cases for a module (modules spec §4.3, `neovibe_core::layout::key_action`),
    // for the prefix and the tray's chips; a refusal flashes the app name (spec §3.2).
    let open_module: Rc<dyn Fn(&ModuleId, KeyAction)> = {
        let grid = grid.clone();
        let focus_module = focus_module.clone();
        let focus_and_arrive = focus_and_arrive.clone();
        let show_on_screen = show_on_screen.clone();
        let place_and_arrive = place_and_arrive.clone();
        let app_name = top_bar.app_name.clone();
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
                refuse(&app_name, &err);
            }
        })
    };

    // A chip is `Ctrl+a <key>` for its module, from the top bar: it never holds the keys itself.
    {
        let open_module = open_module.clone();
        let module_layout = module_layout.clone();
        tray.on_activate(move |id| {
            let action = neovibe_core::layout::key_action(&module_layout.borrow(), id, false);
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

    // tmux's own prefix (spec 2026-09-19-window-modes-design.md §3). Installed after every other
    // window-level controller that exists at startup, so it sees keys first; HINT's, added when a
    // HINT starts, still comes before it.
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
        let module_keys = module_keys.clone();
        let strip_keys = module_keys.clone();
        let strip_layout = module_layout.clone();
        let strip_title = module_title.clone();
        let prefix_strip = prefix_strip.clone();
        prefix::install(
            &window,
            &module_keys.lua_keys(),
            move |command| match command {
                // Both act on the module that last held the keys, as the prefix always has.
                prefix::PrefixCommand::Zoom => {
                    let target = module_layout.borrow().focus().clone();
                    grid.toggle_zoom(&target);
                }
                prefix::PrefixCommand::Resize(direction) => {
                    let target = module_layout.borrow().focus().clone();
                    let cell = editor.cell_size().unwrap_or(layout::FALLBACK_CELL);
                    grid.resize(&target, direction, layout::resize_px(direction, cell));
                }
                // Keyed by the kind of the module that holds the keys (`terminal::literal_target`, a
                // tested pure function), not re-derived here.
                prefix::PrefixCommand::SendPrefix => {
                    let focused = pane_focus::focused_module(&window_for_focus, &grid.hosts());
                    match terminal::literal_target(focused.as_ref().map(ModuleId::kind)) {
                        terminal::LiteralTarget::Editor => editor.send_keys("<C-a>"),
                        terminal::LiteralTarget::Panel => agent.select_all(),
                        terminal::LiteralTarget::Terminal => {
                            for input in terminal::keys::control_letter('a') {
                                terminal.send(input);
                            }
                        }
                        terminal::LiteralTarget::Neither => {}
                    }
                }
                // Only the terminal takes a literal `Ctrl+l`: in nvim `<C-l>` is
                // vim-tmux-navigator's move-right, and the panel has no use for one. Routed
                // through `terminal::literal_target`, the same tested pure function `SendPrefix`
                // uses just above, rather than re-deriving its Terminal arm inline (`main`'s review
                // 2026-09-23, task-8 minor 2).
                prefix::PrefixCommand::SendCtrlL => {
                    let focused = pane_focus::focused_module(&window_for_focus, &grid.hosts());
                    if terminal::literal_target(focused.as_ref().map(ModuleId::kind))
                        == terminal::LiteralTarget::Terminal
                    {
                        for input in terminal::keys::control_letter('l') {
                            terminal.send(input);
                        }
                    }
                }
                // `Ctrl+a t` (bottom-terminal spec §2.4) on the module grid, in the order
                // `ToggleAction::steps` gives and the terminal's own tests pin: every action unzooms
                // first. The keys leave before the terminal hides, inside `Hide` (`hide_module`).
                // Shown or hidden is the layout's (`is_shown`, which a zoom does not change).
                prefix::PrefixCommand::ToggleTerminal => {
                    let id = ModuleId::terminal();
                    let has_keys = pane_focus::focused_module(&window_for_focus, &grid.hosts()).as_ref() == Some(&id);
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
                            // `hide_module` gives the keys to the module the layout chooses -- the
                            // most recent of those next to the terminal, which is the editor or the
                            // agent above it unless a Lua panel under or beside it was more recent --
                            // and only then unmaps it (S3 change 3; `module_grid::hide_then_unmap`,
                            // tested).
                            // The last module on screen is refused with the top bar's flash, as
                            // `Ctrl+a x` refuses it (spec §3.2).
                            terminal::ToggleStep::Hide => {
                                if let Err(err) = grid.hide_module(&id, &*focus_module) {
                                    refuse(&refused_name, &err);
                                }
                            }
                        }
                    }
                }
                // The module that HOLDS THE KEYS right now (`pane_focus`'s own definition), not
                // the layout's `focus`: with any other focus (top bar, a plugin pane) this is a no-op
                // (spec §3), the same way `SendPrefix` just above already reads it. Routing is
                // `text_size::route_focused_module`, not re-derived here, so a swap of the two arms
                // fails that module's own test rather than only a GUI pass (item 3g).
                prefix::PrefixCommand::TextSize(step) => {
                    let focused = pane_focus::focused_module(&window_for_focus, &grid.hosts());
                    match text_size::route_focused_module(focused.as_ref()) {
                        text_size::TextSizeTarget::Editor => text_size_controller.apply_editor(step),
                        text_size::TextSizeTarget::Panel => text_size_controller.apply_panel(step),
                        text_size::TextSizeTarget::Neither => {}
                    }
                }
                // Modules P2 (spec §6.3). `x` hides the module that last held the keys, as `m`
                // zooms it: the layout's focus.
                prefix::PrefixCommand::Hide => {
                    let target = module_layout.borrow().focus().clone();
                    open_module(&target, KeyAction::Hide);
                }
                prefix::PrefixCommand::Module(key) => {
                    let Some(id) = module_keys.module(key).cloned() else {
                        return;
                    };
                    let has_keys = pane_focus::focused_module(&window_for_focus, &grid.hosts()).as_ref() == Some(&id);
                    let action = neovibe_core::layout::key_action(&module_layout.borrow(), &id, has_keys);
                    open_module(&id, action);
                }
                prefix::PrefixCommand::Place { key, axis } => {
                    let Some(id) = module_keys.module(key).cloned() else {
                        return;
                    };
                    if let Err(err) = place_and_arrive(&id, axis) {
                        refuse(&refused_name, &err);
                    }
                }
                prefix::PrefixCommand::Swap(direction) => {
                    let target = module_layout.borrow().focus().clone();
                    if grid.swap_modules(&target, direction).is_none() {
                        println!("[modules] {target}: nothing to swap with {direction:?}");
                    }
                }
                prefix::PrefixCommand::Even(axis) => grid.even_modules(axis),
            },
            move |waiting| {
                if waiting == prefix::Waiting::No {
                    app_name.remove_css_class("prefix-armed");
                } else {
                    app_name.add_css_class("prefix-armed");
                }
                // The module keys and the verbs, or after `\`/`"` the module keys alone (spec §6.5).
                let pieces = {
                    let layout = strip_layout.borrow();
                    let entries = neovibe_core::layout::strip(&strip_keys, &layout);
                    prefix_strip::strip_pieces(waiting, &entries, &strip_title(layout.focus()), &*strip_title)
                };
                prefix_strip.show(&pieces);
            },
        );
    }

    // --- Ctrl+h/j/k/l between modules, by geometry (modules design §6.2). One function for every
    // source: the editor's shim letters below, and each web module's capture-phase controller
    // below that. A move unzooms first, as tmux's `select-pane` does (spec §3.4); `Up` with nothing
    // above goes to the top bar, which is above every module; anything else with nothing there is
    // tmux's no-op at the edge of its grid, and `false` lets the key go on to the module -- which
    // is what the agent panel's `Ctrl+l`/`Ctrl+j` always did.
    let move_focus: Rc<dyn Fn(&ModuleId, Direction) -> bool> = {
        let grid = grid.clone();
        let focus_module = focus_module.clone();
        let focus_top_bar = focus_top_bar.clone();
        let agent_panel_handle = agent_panel_handle.clone();
        Rc::new(move |from, direction| match grid.navigate(from, direction) {
            Nav::Module(to) => {
                grid.unzoom();
                let grabbed = focus_module(&to);
                println!("[pane_switch] {from} {direction:?} -> {to} (grab_focus={grabbed})");
                // Arriving by keyboard means "I want to type": open the composer with the caret in
                // it (owner, 2026-09-19). A click on a row does not come through here and still
                // lands in BROWSE on that row.
                if grabbed && to.kind() == ModuleKind::Agent {
                    agent_panel_handle.enter_input();
                }
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

    // --- From the editor: decided by Neovim itself.
    //
    // This half deliberately has no key handler at all. The embedded nvim received `TMUX`,
    // `TMUX_PANE` and a fake-`tmux`-carrying `PATH` at spawn time (see `pane_switch`), so the
    // user's real `vim-tmux-navigator` runs its own `wincmd` first and only calls out to "tmux"
    // once the cursor is at a genuine Neovim window boundary -- which is what reaches us here as a
    // letter. Real `:vsplit` navigation therefore keeps working untouched; `shell` never sees the
    // keypresses that Neovim resolved internally.
    if let Some(ps) = pane_switch.as_mut() {
        let move_focus = move_focus.clone();
        pane_switch::listen(ps, move |letter| match pane_switch::letter_direction(letter) {
            Some(direction) => {
                move_focus(&ModuleId::editor(), direction);
            }
            None => println!("[pane_switch] unknown direction {letter:?}, ignoring"),
        });
    }

    // --- From a web module (the agent panel, a Lua panel): a capture-phase controller on its host
    // (`install_module_nav`), one per web module here; a web module added later installs its own.
    for (id, host) in grid
        .hosts()
        .iter()
        .filter(|(id, _)| matches!(id.kind(), ModuleKind::Agent | ModuleKind::LuaWebview))
    {
        install_module_nav(id.clone(), host, move_focus.clone());
    }

    // --- Ctrl+h/j/k/l in the terminal: neovibe's, always (owner, 2026-09-23: "neovibe的按键优先").
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

    // nvim exiting on its own (e.g. `:qa!`) has no window to close by itself -- ask this
    // host's window to close, which in turn drives the connect_close_request handler below.
    // Mirrors neovide-editor's own `examples/standalone.rs` exactly.
    {
        let window = window.clone();
        pane.on_exited_unrequested(move || {
            window.close();
        });
    }

    // `neovibe.layout.show/hide/focus/split` from a command or an event handler (modules P2, spec
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
                use neovibe_core::lua::layout::LayoutRequest;
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
    // `neovibe.layout.show` and the rest work from here on, `shell:ready` handlers included.
    lua_engine.layout.borrow_mut().accept_requests();
    lua_engine.emit("shell:ready");
    apply_layout_requests();

    // Polls for a cross-window "come to the front" request from `neovibe-supervisor` -- e.g. the
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
    // every use here only ever needs `&self` methods.) Same connect_close_request -> shutdown()
    // -> unconditional glib::Propagation::Proceed pattern as standalone.rs: the app is exiting
    // either way, regardless of whether a clean `NeovimExited` was actually observed.
    let app_for_close = app.clone();
    window.connect_close_request(move |_window| {
        // First, and synchronously (spec §4.6): the debounce may still be waiting on an arrangement
        // change. With none unwritten this writes nothing -- a click never does (`layout_state`'s
        // module doc says what is written when).
        layout_saver.save_now();
        pane.shutdown();
        // Hangs the terminal's shell up without waiting for it (`TerminalSession`'s `Drop`). Should
        // the process exit first, the kernel closing the PTY master hangs it up anyway.
        terminal.shutdown();
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
        // (`LiveHarness::shutdown` in the fork). A slow `:qa!` therefore still holds the window on
        // screen, with nothing below involved. Pre-existing, out of scope here, and named because
        // three consecutive reviews of this path died on a comment that over-claimed.
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

/// A layout verb the layout refused -- the last module on screen hidden, a module placed next to
/// itself: said on stdout, and the top bar's app name shows the prefix indicator's block for
/// [`REFUSAL_FLASH`] (spec §3.2). Its own class, so a prefix armed meanwhile keeps its indicator. A
/// second refusal inside the flash restarts it rather than being cut short by the first one's timer
/// (Task 9's review, minor 5). One window per process (`NON_UNIQUE`), so one timer per thread.
fn refuse(app_name: &gtk4::Label, err: &LayoutError) {
    println!("[modules] refused: {err}");
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

/// `Ctrl+h/j/k/l` from a web module (the agent panel, a Lua panel), on its host: a capture-phase
/// controller, because `vim-tmux-navigator` lives inside Neovim, which is not the focused widget
/// here, so nothing would ever run; capture, not bubble, because a `WebView` handles key events
/// itself and would otherwise consume the chord before a bubble-phase controller on the same widget
/// ran. Only a chord that moves is claimed. Called once per web module host, at startup and for one
/// added later (modules P2; P3's canvas is the first).
fn install_module_nav(id: ModuleId, host: &gtk4::Widget, move_focus: Rc<dyn Fn(&ModuleId, Direction) -> bool>) {
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

    /// Every spelling a `keys:` row offers, each `/`-separated alternative on its own. Shared by
    /// every accelerator-guard test below, not just `every_app_accelerator_is_in_the_panel_keymap`
    /// (item 3f: these used to live nested inside that one test function, so no other test could
    /// use them without a second, drift-prone copy).
    ///
    /// Each row read as JavaScript reads it ([`js_keys_value`]), so `Ctrl+a \\` is one backslash and
    /// `Ctrl+a \"` is not cut at its quote (Task 9's review, minor 6).
    fn documented_keys(keymap: &str) -> std::collections::HashSet<String> {
        let mut rows = std::collections::HashSet::new();
        for value in keymap.lines().filter_map(js_keys_value) {
            for alternative in value.split(" / ") {
                rows.insert(alternative.to_string());
            }
            rows.insert(value);
        }
        rows
    }

    /// The `keys:` value on one `keymap.ts` row, as JavaScript reads it: `\\` is a backslash and
    /// `\"` a quote. Modules P2's `Ctrl+a \\` and `Ctrl+a \"` rows need both; splitting the raw
    /// line at `"` read the second as the row `Ctrl+a \\`. The row may open on an earlier line
    /// (`{` alone, then `keys: "..."`).
    fn js_keys_value(line: &str) -> Option<String> {
        let start = line.find("keys: \"")? + "keys: \"".len();
        let mut value = String::new();
        let mut chars = line[start..].chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => value.push(chars.next()?),
                '"' => return Some(value),
                _ => value.push(c),
            }
        }
        None
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

    /// A GTK accelerator (`<Control><Shift>f`) as `keymap.ts` spells it (`Ctrl+Shift+F`).
    fn spell(accel: &str) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut rest = accel;
        while let Some(end) = rest.strip_prefix('<').and_then(|r| r.find('>')) {
            parts.push(match &rest[1..end + 1] {
                "Control" | "Primary" => "Ctrl".to_string(),
                other => other.to_string(),
            });
            rest = &rest[end + 2..];
        }
        parts.push(match NAMED_KEYSYMS.iter().find(|(keysym, _)| *keysym == rest) {
            Some((_, spelled)) => spelled.to_string(),
            None if rest.chars().count() == 1 => rest.to_uppercase(),
            None => rest.to_string(),
        });
        parts.join("+")
    }

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

    /// The panel's `?` keymap lists the window's keys by hand (`agent-ui/web/src/keymap.ts`,
    /// `WINDOW_KEYS`), since nothing on the web side can read what GTK binds. This test reads the
    /// registrations rather than restating them: it walks this crate's own sources for
    /// accelerator-shaped string literals and asserts each one has a row in that list.
    ///
    /// It looks for the literals rather than for `set_accels_for_action` calls because
    /// `window_mode.rs` passes its two through a `format!` over a table, so a scan of the call
    /// sites alone would miss `F11` and `<Control><Shift>F11` entirely. In `shell/src` an
    /// accelerator-shaped literal is always an accelerator; a new one that is something else fails
    /// loudly here, which is the safe direction.
    ///
    /// Reading the sources is also what gives this **both** directions. The hand-written list this
    /// replaced could only catch a registration whose row was deleted; a key added in a new file
    /// and never documented now fails too.
    ///
    /// The containment check is row-shaped, not substring-shaped: an accelerator must equal a whole
    /// `keys:` value, or one `/`-separated alternative inside one. `keymap.ts.contains("Ctrl+Shift+F")`
    /// was satisfied by the `Ctrl+Shift+F11` row, so deleting HINT's own row left the old test green.
    ///
    /// **known limit (item 3f, 2026-09-23; corrected 2026-09-23, later):** this scans complete,
    /// double-quoted string literals. An earlier revision of this comment claimed
    /// `format!("<Control>{}", "equal")` was the shape that goes unnoticed -- that is wrong, and an
    /// adversarial recheck caught it: `"<Control>{}"` IS one complete literal (the `{}` is just two
    /// ordinary characters in the source until the `format!` macro runs), so `is_accel` sees it
    /// whole, finds a modifier bracket whose key part (`"{}"`) is neither a bare letter/digit, an
    /// `Fnn` key, nor a `NAMED_KEYSYM`, and hits its own panic guard -- loudly, not silently.
    /// The shape that actually goes unnoticed is a split into two or more WHOLE separate literals,
    /// e.g. `format!("{}{}", "<Control>", "Home")`: `sources`/`is_accel` see `"<Control>"` and
    /// `"Home"` as two independent literals, and each is individually rejected on its own merits
    /// (`"<Control>"` alone has an empty key part; `"Home"` alone has no modifier bracket and is
    /// not a function key) -- so neither `found` nor `is_accel`'s panic guard is ever given a
    /// literal that looks malformed enough to complain about. Every accelerator this crate
    /// registers today is written as one complete literal (`text_size::TEXT_SIZE_ACTIONS`'s array
    /// entries, each row of `window_mode.rs`'s per-setting table, `main.rs`'s own inline calls);
    /// the one genuinely runtime-assembled accelerator (`main.rs`'s Lua `keybinding` binding, built
    /// from a value `init.lua` supplies) is deliberately out of this scan's scope already, since it
    /// cannot be known at compile time. If a FUTURE accelerator is ever built by concatenating two
    /// or more whole literal pieces, this scan will not catch it going undocumented -- closing that
    /// in general would need parsing `format!` call sites rather than scanning quoted text, which
    /// was rejected on `window_mode.rs`'s own account above (the call-site shape there already
    /// defeated a call-site scan once).
    #[test]
    fn every_app_accelerator_is_in_the_panel_keymap() {
        let found = found_accelerators();

        // A floor, not a list: it catches a walk that silently stopped finding anything, without
        // restating what the sources say.
        assert!(
            found.len() >= 4,
            "only {} accelerators found -- the source walk is broken",
            found.len()
        );

        let documented = documented_keys(include_str!("../../agent-ui/web/src/keymap.ts"));
        for (file, accel) in found {
            let spelled = spell(&accel);
            assert!(
                documented.contains(&spelled),
                "{file} binds {accel}, but no keymap.ts row says `keys: \"{spelled}\"`"
            );
        }
    }

    /// The reverse half of `every_app_accelerator_is_in_the_panel_keymap`, which only ever asked
    /// "is every accelerator this crate registers documented" -- never "does every documented
    /// accelerator still correspond to a real registration". Dropping `<Control>plus` from
    /// `text_size.rs` (item 3f) left `keymap.ts`'s `Ctrl++` row undisturbed and that test still
    /// green, because nothing read `keymap.ts` looking for a STALE row.
    ///
    /// Scoped to `text_size::TEXT_SIZE_ACTIONS`'s own three "both panes" rows rather than the
    /// whole file: most of `keymap.ts` (`Ctrl+h`/`Ctrl+k`/BROWSE's `j`/`k`/etc.) documents keys
    /// that are not `set_accels_for_action` registrations at all, so a blanket reverse scan across
    /// every row would be comparing accelerators against things that were never accelerators.
    #[test]
    fn every_documented_both_panes_text_size_key_is_still_bound_in_text_size_rs() {
        let mut bound: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (_, accels, _) in text_size::TEXT_SIZE_ACTIONS.iter().copied() {
            for accel in accels {
                bound.insert(spell(accel));
            }
        }
        let keymap = include_str!("../../agent-ui/web/src/keymap.ts");
        let mut checked_any = false;
        for line in keymap
            .lines()
            .filter(|l| l.contains("Text size") && l.contains("both panes"))
        {
            let Some(value) = line.split("keys: \"").nth(1).and_then(|rest| rest.split('"').next()) else {
                continue;
            };
            for alternative in value.split(" / ") {
                checked_any = true;
                assert!(
                    bound.contains(alternative),
                    "keymap.ts documents {alternative:?} as a both-panes text-size key, but \
                     text_size::TEXT_SIZE_ACTIONS no longer binds it"
                );
            }
        }
        assert!(
            checked_any,
            "no both-panes text-size row found in keymap.ts -- the scan is broken"
        );
    }

    /// `PREFIX_KEYS` (`agent-ui/web/src/keymap.ts`) documents `shell/src/prefix.rs`'s own
    /// `Ctrl+a` command table by hand, since nothing on the web side can read what that table
    /// binds either -- and, like the accelerator guard above, this used to check only one
    /// direction. Deleting BOTH of `text_size.rs`'s `PREFIX_KEYS` rows (`=`/`-` and `0`) left every
    /// existing test green, because nothing previously read `PREFIX_KEYS` at all (item 3f).
    ///
    /// Checked both ways against [`prefix::bound_keys`], which reads the actual bound characters
    /// out of `prefix::command`'s own match arms rather than a third, hand-typed copy of them. The
    /// `Ctrl+a Ctrl+<letter>` rows are held the same way to [`prefix::bound_control_keys`], which
    /// drives the prefix itself: before that, a one-character scan skipped them, and the
    /// `Ctrl+a Ctrl+l` row the bottom terminal added was unguarded (review 2026-09-23, finding 19).
    ///
    /// A `Ctrl+a <split> then <keys>` row is held to what the prefix takes after that split key,
    /// through its own `AwaitModule` state (spec §6.5), and each module key after `then` to exactly
    /// one character: reading only a token's first character let `then editor / agent` pass for
    /// `then e / a` (Task 9's review, minor 7).
    #[test]
    fn prefix_keys_documents_exactly_the_chars_prefix_rs_binds() {
        let keymap = include_str!("../../agent-ui/web/src/keymap.ts");
        let mut documented: std::collections::HashSet<char> = std::collections::HashSet::new();
        let mut documented_control: std::collections::HashSet<char> = std::collections::HashSet::new();
        // `Ctrl+a \\ then e / a / t`: the split key, and the module keys it is documented to take.
        let mut documented_then: Vec<(char, Vec<char>)> = Vec::new();
        for line in keymap.lines() {
            let Some(value) = js_keys_value(line) else { continue };
            let Some(value) = value.strip_prefix("Ctrl+a ") else {
                continue;
            };
            if let Some((split, modules)) = value.split_once(" then ") {
                let mut split = split.chars();
                let (Some(split), None) = (split.next(), split.next()) else {
                    panic!("a `then` row names one key before `then`: {value:?}");
                };
                documented.insert(split);
                let mut keys: Vec<char> = modules
                    .split(" / ")
                    .map(|token| {
                        let mut key = token.chars();
                        match (key.next(), key.next()) {
                            (Some(key), None) => key,
                            _ => panic!("a module key after `then` is one character, not {token:?}: {value:?}"),
                        }
                    })
                    .collect();
                keys.sort_unstable();
                documented_then.push((split, keys));
                continue;
            }
            for token in value.split(" / ") {
                let mut chars = token.chars();
                if let (Some(ch), None) = (chars.next(), chars.next()) {
                    documented.insert(ch);
                }
                let mut letter = token.strip_prefix("Ctrl+").unwrap_or_default().chars();
                if let (Some(ch), None) = (letter.next(), letter.next()) {
                    documented_control.insert(ch);
                }
            }
        }
        let bound_control: std::collections::HashSet<char> = prefix::bound_control_keys().into_iter().collect();
        assert_eq!(
            documented_control, bound_control,
            "PREFIX_KEYS' Ctrl+a Ctrl+<letter> rows and what prefix.rs binds after Ctrl+a disagree"
        );
        let bound: std::collections::HashSet<char> = prefix::bound_keys().into_iter().collect();

        for &ch in &bound {
            assert!(
                documented.contains(&ch),
                "prefix.rs binds {ch:?} after Ctrl+a, but no PREFIX_KEYS row mentions it"
            );
        }
        for &ch in &documented {
            assert!(
                bound.contains(&ch),
                "PREFIX_KEYS documents {ch:?} after Ctrl+a, but prefix::command no longer binds it"
            );
        }

        // The split keys, through the prefix's own `AwaitModule` state (spec §6.5).
        let mut taken = prefix::module_keys_after();
        for (_, keys) in &mut taken {
            keys.sort_unstable();
        }
        documented_then.sort();
        assert_eq!(
            documented_then, taken,
            "PREFIX_KEYS' `Ctrl+a <split> then <keys>` rows and what prefix.rs takes after a split key disagree"
        );
    }

    /// `WINDOW_KEYS` names `Ctrl+h/j/k/l` by hand, and its `Ctrl+j` row exists only because of the
    /// terminal. Every chord the terminal gives up to neovibe (`terminal::navigation`, asked about
    /// each letter) must be named there. The reverse is not checked: those rows also describe the
    /// editor's and the panel's own routes.
    #[test]
    fn every_chord_the_terminal_gives_up_is_in_window_keys() {
        let keymap = include_str!("../../agent-ui/web/src/keymap.ts");
        let start = keymap.find("export const WINDOW_KEYS").expect("WINDOW_KEYS");
        let end = start + keymap[start..].find("];").expect("the end of WINDOW_KEYS");
        let documented: std::collections::HashSet<&str> = keymap[start..end]
            .lines()
            .filter_map(|line| line.trim_start().strip_prefix("{ keys: \""))
            .filter_map(|rest| rest.split('"').next())
            .flat_map(|value| value.split(" / "))
            .collect();
        let mut taken = 0;
        for letter in 'a'..='z' {
            let key = gtk4::gdk::Key::from_name(letter.to_string()).expect("a letter keyval");
            if terminal::navigation(key, gtk4::gdk::ModifierType::CONTROL_MASK).is_some() {
                taken += 1;
                let name = format!("Ctrl+{letter}");
                assert!(
                    documented.contains(name.as_str()),
                    "the terminal gives up {name}, but no WINDOW_KEYS row names it"
                );
            }
        }
        assert_eq!(taken, 4, "Ctrl+h/j/k/l, and nothing else");
    }
}
