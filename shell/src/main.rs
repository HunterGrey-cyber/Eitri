//! shell: the real neovibe product window -- custom chrome, a two-pane layout, the
//! neovide-editor crate embedded as the real editor pane, and the real agent-ui panel (a
//! WebView-hosted frontend bridged to a lazily-started `agent::AgentSession`). See
//! docs/superpowers/specs/2026-09-06-shell-scaffolding-design.md.

mod agent_panel;
mod chrome;
mod editor_context;
mod hint;
mod layout;
mod pane_focus;
mod pane_switch;
mod prefix;
mod supervisor_client;
mod terminal_handoff;
mod theme;
mod lua;
mod window_mode;

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::gdk::{Key, ModifierType};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, EventControllerKey};

use lua::{LuaEngine, PanelEntry, PanelSlot};
use neovide_editor::{NeovideEditorPane, NeovideEditorPaneOptions};

const APP_ID: &str = "cn.huntergrey.neovibe";

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
    let lua_engine = Rc::new(
        LuaEngine::new(config_dir.clone()).expect("LuaEngine construction must not fail"),
    );

    // Register built-in panels FIRST, through the exact same `PanelRegistry::register` that a
    // Lua plugin's `neovibe.panel.register` call goes through -- then load init.lua, which may
    // register more panels (including replacing a built-in slot: a later registration to an
    // already-occupied slot just replaces the earlier one, per `PanelRegistry::register`).
    let editor_widget: gtk4::Widget = pane.widget().clone().upcast();
    lua_engine.register_builtin_panel(
        PanelSlot::Main,
        PanelEntry { id: "editor".into(), title: "Editor".into(), widget: editor_widget },
    );
    // A feed that failed to start yields a source that always answers `None`, so the panel needs no
    // branch: turns simply go out as the user typed them, exactly as before wire 1 existed.
    let editor_context_source = match context_feed.as_mut() {
        Some(feed) => editor_context::listen(feed),
        None => std::rc::Rc::new(|| None),
    };
    let (agent_widget, agent_panel_handle) =
        agent_panel::build_agent_panel(project_root.to_path_buf(), editor_context_source);
    lua_engine.register_builtin_panel(
        PanelSlot::Side,
        PanelEntry { id: "agent".into(), title: "Agent".into(), widget: agent_widget.clone() },
    );

    lua_engine.load_init_file(&config_dir.join("init.lua"));

    // Build the real layout from whatever ended up in the registry -- this is what makes
    // "built-in and plugin panels share one path" a fact about the running app, not just
    // documentation. `.expect()` here is deliberate: both slots are guaranteed non-empty by this
    // point (the two `register_builtin_panel` calls above ran unconditionally, and even if
    // init.lua replaced one, replacement never leaves a slot empty).
    //
    // The bottom slot is the one that can legitimately be empty -- no plugin claiming it -- so
    // it is an `Option` rather than an `expect`, and an empty one means the
    // window is built with no vertical split at all.
    let (main_widget, side_widget, bottom_widget) = {
        let panels = lua_engine.panels.borrow();
        let main = panels.get(PanelSlot::Main).expect("main slot must be populated by this point");
        let side = panels.get(PanelSlot::Side).expect("side slot must be populated by this point");
        let bottom = panels.get(PanelSlot::Bottom);
        (main.widget.clone(), side.widget.clone(), bottom.map(|e| e.widget.clone()))
    };

    // A Lua plugin can take the main slot, in which case the editor is not pane 0 and must not
    // be told it has the keys when that plugin does; also whether `window_mode`'s F11 sync has an
    // nvim to write to at all (§2.4).
    let editor_is_main = main_widget == pane.widget().clone().upcast::<gtk4::Widget>();

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

    let (content_widget, across) = layout::build_content_area(&main_widget, &side_widget);
    let (content_widget, down) = match &bottom_widget {
        Some(bottom) => {
            let (widget, paned) = layout::build_vertical_split(&content_widget, bottom);
            (widget, Some(paned))
        }
        None => (content_widget, None),
    };
    // Zoom and resize for the `Ctrl+a` prefix (spec 2026-09-19-window-modes-design.md §3.4-3.5).
    let pane_layout = layout::PaneLayout::new(across, down);

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
    agent_panel_handle.set_theme(&neovibe_core::theme::ThemeTokens::fallback());
    pane.set_clear_color(editor_clear(&neovibe_core::theme::ThemeTokens::fallback()));
    if let Some(feed) = theme_feed.as_mut() {
        let theme_css = theme_css.clone();
        let agent_panel_handle = agent_panel_handle.clone();
        let pane_for_theme = pane.clone();
        theme::feed::listen(feed, move |payload| {
            let tokens = neovibe_core::theme::ThemeTokens::derive(&payload);
            println!(
                "[theme] following nvim colorscheme {:?} (background={})",
                payload.options.colors_name, payload.options.background
            );
            theme_css.update(&tokens);
            agent_panel_handle.set_theme(&tokens);
            pane_for_theme.set_clear_color(editor_clear(&tokens));
        });
    }

    let top_bar = chrome::build_top_bar(&window, project_root);
    root.append(&top_bar.widget);
    root.append(&content_widget);

    // The global `f` HINT draws its GTK labels (top bar, editor, bottom slot) as overlay children
    // of this, positioned over whatever they label. Its only child is the whole window content, so
    // the layout is exactly what it was without it; the labels never take a click.
    let hint_overlay = gtk4::Overlay::new();
    hint_overlay.set_child(Some(&root));
    window.set_child(Some(&hint_overlay));

    // The global `f` HINT (spec: docs/superpowers/specs/2026-09-19-global-hint-design.md). Three
    // triggers reach the same toggle: `Ctrl+Shift+F` from anywhere (the `app.hint` accelerator,
    // which GTK fires from the window's capture-phase `gtk-shortcut-manager-capture` controller,
    // before the editor or the panel see the key -- not from `gtk-application-shortcuts`, which is
    // GLOBAL-scoped and handles nothing itself; see `hint`'s module doc), `f` on the top bar
    // (below), and `f` in the panel's BROWSE (the panel's `hint_request`).
    let hint_coordinator = hint::HintCoordinator::new(hint::HintWidgets {
        window: window.clone(),
        overlay: hint_overlay.clone(),
        top_items: top_bar.items.clone(),
        editor: pane.clone(),
        main_widget: main_widget.clone(),
        side_widget: side_widget.clone(),
        agent_widget: agent_widget.clone(),
        bottom: bottom_widget.clone(),
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

    // Which pane has the keys. One tracker drives the editor's cursor (solid, or not drawn,
    // via Neovide itself) and the agent panel's cursor and mode block, so they cannot disagree. See
    // `pane_focus`'s module doc.
    let pane_contents: Vec<gtk4::Widget> = {
        let mut v = vec![main_widget.clone(), side_widget.clone()];
        v.extend(bottom_widget.clone());
        v
    };
    let last_pane = {
        let agent_panel_handle = agent_panel_handle.clone();
        let editor = pane.clone();
        pane_focus::install(&window, pane_contents.clone(), move |index, has_keys| {
            println!("[pane_focus] pane {index} has_keys={has_keys}");
            match index {
                0 if editor_is_main => editor.set_focused(has_keys),
                1 => agent_panel_handle.set_pane_focused(has_keys),
                _ => {}
            }
        })
    };

    // Moves focus back to the pane that last had it. Used by the top bar's `Ctrl+j`/`Esc`. The
    // editor goes through `NeovideEditorPane::grab_focus`, which also tells the input method.
    let return_to_pane: Rc<dyn Fn()> = {
        let pane = pane.clone();
        let pane_contents = pane_contents.clone();
        let main_widget = main_widget.clone();
        let last_pane = last_pane.clone();
        std::rc::Rc::new(move || {
            let target = &pane_contents[last_pane.get().min(pane_contents.len() - 1)];
            if *target == main_widget {
                pane.grab_focus();
            } else {
                target.grab_focus();
            }
        })
    };

    // F11 fullscreen, Ctrl+Shift+F11 immersive, both following g:neovide_fullscreen (spec
    // 2026-09-19-window-modes-design.md §2). Needs `return_to_pane` for a top bar that hides while
    // it holds focus, and gives `focus_top_bar` its reveal.
    let window_modes = window_mode::WindowModes::install(
        app,
        &window,
        &top_bar,
        editor_is_main.then(|| pane.clone()),
        return_to_pane.clone(),
    );

    let focus_top_bar = {
        let top_items = top_bar.items.clone();
        let window_modes = window_modes.clone();
        std::rc::Rc::new(move || {
            window_modes.reveal_top_bar();
            top_items.first().is_some_and(|item| item.grab_focus())
        })
    };

    // tmux's own prefix (spec 2026-09-19-window-modes-design.md §3). Installed after every other
    // window-level controller that exists at startup, so it sees keys first; HINT's, added when a
    // HINT starts, still comes before it.
    {
        let pane_layout = pane_layout.clone();
        let last_pane = last_pane.clone();
        let editor = pane.clone();
        let agent = agent_panel_handle.clone();
        let window_for_focus = window.clone();
        let pane_contents = pane_contents.clone();
        let side_is_agent = side_widget == agent_widget;
        let app_name = top_bar.app_name.clone();
        prefix::install(
            &window,
            move |command| match command {
                prefix::PrefixCommand::Zoom => pane_layout.toggle_zoom(last_pane.get()),
                prefix::PrefixCommand::Resize(direction) => {
                    let cell = if editor_is_main { editor.cell_size() } else { None };
                    pane_layout.resize(direction, last_pane.get(), cell.unwrap_or(layout::FALLBACK_CELL));
                }
                prefix::PrefixCommand::SendPrefix => {
                    match pane_focus::focused_pane(&window_for_focus, &pane_contents) {
                        Some(0) if editor_is_main => editor.send_keys("<C-a>"),
                        Some(1) if side_is_agent => agent.select_all(),
                        _ => {}
                    }
                }
            },
            move |armed| {
                if armed {
                    app_name.add_css_class("prefix-armed");
                } else {
                    app_name.remove_css_class("prefix-armed");
                }
            },
        );
    }

    // --- Ctrl+l: editor -> agent panel, decided by Neovim itself.
    //
    // This half deliberately has no key handler at all. The embedded nvim received `TMUX`,
    // `TMUX_PANE` and a fake-`tmux`-carrying `PATH` at spawn time (see `pane_switch`), so the
    // user's real `vim-tmux-navigator` runs its own `wincmd l` first and only calls out to
    // "tmux" once the cursor is at a genuine Neovim window boundary -- which is what reaches us
    // here as an `'R'`. Real `:vsplit` navigation therefore keeps working untouched; `shell`
    // never sees the keypresses that Neovim resolved internally.
    if let Some(ps) = pane_switch.as_mut() {
        let side_widget = side_widget.clone();
        let focus_top_bar = focus_top_bar.clone();
        let agent_panel_handle = agent_panel_handle.clone();
        let pane_layout = pane_layout.clone();
        // Only the built-in agent panel has a composer. A Lua panel in the side slot just gets focus.
        let side_is_agent = side_widget == agent_widget;
        pane_switch::listen(ps, move |direction| match direction {
            'R' => {
                // tmux `select-pane` unzooms first (spec §3.4).
                pane_layout.unzoom();
                // A `WebView` is an ordinary focusable GTK widget -- unlike a bare `GtkGLArea`,
                // which needs `focusable(true)` set explicitly before `grab_focus()` does
                // anything. Verified in the sandbox rather than assumed; see
                // `shell/MANUAL_VERIFICATION.md`.
                let grabbed = side_widget.grab_focus();
                println!("[pane_switch] direction R -> focusing the side panel (grab_focus={grabbed})");
                // Arriving by keyboard means "I want to type": open the composer with the caret in
                // it (owner, 2026-09-19). A click on a row does not go through here and still lands
                // in BROWSE on that row.
                if grabbed && side_is_agent {
                    agent_panel_handle.enter_input();
                }
            }
            // Ctrl+k at nvim's topmost window: the top bar is above every pane.
            'U' => {
                let grabbed = focus_top_bar();
                println!("[pane_switch] direction U -> focusing the top bar (grab_focus={grabbed})");
            }
            // The editor is already the leftmost pane, and nothing routes focus into the bottom
            // slot: a Lua-registered panel can still occupy it (`layout::build_vertical_split`
            // above), but the `'D'` arm went with the native terminal pane on 2026-09-19 and was
            // never wired for plugin panels, so that slot is mouse-reachable only. These are
            // therefore the same no-ops real tmux performs at the edge of its own pane grid.
            other => println!("[pane_switch] direction {other} -> no pane in that direction, ignoring"),
        });
    }

    // --- Ctrl+h: agent panel -> editor.
    //
    // The reverse direction cannot go through the mechanism above: `vim-tmux-navigator` lives
    // inside Neovim, which is not the focused widget here, so nothing would ever run. This is a
    // direct GTK capture-phase handler on whatever widget currently occupies the side slot
    // (which may be a Lua-registered panel rather than the built-in agent panel -- attaching to
    // the slot's widget rather than to `agent_widget` specifically is what keeps this working in
    // that case). Capture phase, not bubble: a `WebView` handles key events itself and would
    // otherwise consume the chord before a bubble-phase controller on the same widget ran.
    {
        let pane = pane.clone();
        let focus_top_bar = focus_top_bar.clone();
        let pane_layout = pane_layout.clone();
        let controller = EventControllerKey::new();
        controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        controller.connect_key_pressed(move |_controller, key, _keycode, state| {
            if key == Key::k && state.contains(ModifierType::CONTROL_MASK) {
                focus_top_bar();
                println!("[pane_switch] Ctrl+k in the side panel -> focusing the top bar");
                return glib::Propagation::Stop;
            }
            if key == Key::h && state.contains(ModifierType::CONTROL_MASK) {
                // tmux `select-pane` unzooms first (spec §3.4).
                pane_layout.unzoom();
                // `NeovideEditorPane::grab_focus()`, not a bare `widget().grab_focus()`: the
                // pane's own method also calls `im_context.focus_in()`, without which the input
                // method keeps believing the panel still owns the keyboard.
                pane.grab_focus();
                println!("[pane_switch] Ctrl+h in the side panel -> focusing the editor");
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        side_widget.add_controller(controller);
    }

    // --- The top bar, from the keyboard. `h`/`l` move between its items, `Enter`/`Space`
    // activate one (a GTK button's own behaviour), and `Ctrl+j` or `Esc` go back to the pane that
    // last had the keys. Capture phase on the bar, so the chords are seen before a focused button
    // gets them. Only the items `build_top_bar` returned are reachable: the window controls are
    // not focusable at all, so `Enter` here can never close the window.
    {
        let top_items = top_bar.items.clone();
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
            let focus = gtk4::prelude::GtkWindowExt::focus(&window);
            let current = top_items.iter().position(|item| Some(item) == focus.as_ref()).unwrap_or(0);
            let next = (current as isize + step).clamp(0, top_items.len() as isize - 1) as usize;
            top_items[next].grab_focus();
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
        action.connect_activate(move |_, _| {
            lua_engine_for_action.invoke_command(&id_owned);
        });
        app.add_action(&action);
        if let Some(keybinding) = &entry.keybinding {
            app.set_accels_for_action(&format!("app.{action_name}"), &[keybinding.as_str()]);
        }
    }

    window.present();
    // grab_focus() after present(), matching standalone.rs's own
    // `window.present(); pane.grab_focus();` ordering -- focusing a not-yet-shown widget is
    // meaningless.
    pane.grab_focus();

    // The one real v1 event: fires once the window is actually up, so any Lua handler reacting
    // to it sees a fully-built shell (panels registered, commands bound, window shown).
    lua_engine.emit("shell:ready");

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
    window.connect_close_request(move |_window| {
        pane.shutdown();
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
        agent_panel_handle.shutdown();
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
    #[test]
    fn every_app_accelerator_is_in_the_panel_keymap() {
        /// Every spelling a `keys:` row offers, each `/`-separated alternative on its own.
        fn documented_keys(keymap: &str) -> std::collections::HashSet<String> {
            let mut rows = std::collections::HashSet::new();
            for after in keymap.split("keys: \"").skip(1) {
                let Some(value) = after.split('"').next() else { continue };
                rows.insert(value.to_string());
                for alternative in value.split(" / ") {
                    rows.insert(alternative.to_string());
                }
            }
            rows
        }

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
            parts.push(if rest.chars().count() == 1 { rest.to_uppercase() } else { rest.to_string() });
            parts.join("+")
        }

        /// `<Mod>…Key`, with at least one modifier or a function key. The shape the sources are
        /// scanned for; anything else in a string literal is not an accelerator.
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
            let function_key =
                rest.starts_with('F') && rest.len() > 1 && rest[1..].chars().all(|c| c.is_ascii_digit());
            !rest.is_empty()
                && rest.chars().all(|c| c.is_ascii_alphanumeric())
                && (modifiers > 0 || function_key)
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
                    let code: String =
                        code.lines().map(|line| line.split("//").next().unwrap_or_default()).collect::<Vec<_>>().join("\n");
                    out.push((path.display().to_string(), code));
                }
            }
        }

        let mut files = Vec::new();
        sources(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        let mut found: Vec<(String, String)> = Vec::new();
        for (name, code) in &files {
            for (index, chunk) in code.split('"').enumerate() {
                if index % 2 == 1 && is_accel(chunk) {
                    found.push((name.clone(), chunk.to_string()));
                }
            }
        }

        // A floor, not a list: it catches a walk that silently stopped finding anything, without
        // restating what the sources say.
        assert!(found.len() >= 4, "only {} accelerators found -- the source walk is broken", found.len());

        let documented = documented_keys(include_str!("../../agent-ui/web/src/keymap.ts"));
        for (file, accel) in found {
            let spelled = spell(&accel);
            assert!(
                documented.contains(&spelled),
                "{file} binds {accel}, but no keymap.ts row says `keys: \"{spelled}\"`"
            );
        }
    }
}
