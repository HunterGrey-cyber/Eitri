//! shell: the real neovibe product window -- custom chrome, a two-pane layout, the
//! neovide-editor crate embedded as the real editor pane, and the real agent-ui panel (a
//! WebView-hosted frontend bridged to a lazily-started `agent::AgentSession`). See
//! docs/superpowers/specs/2026-09-06-shell-scaffolding-design.md.

mod agent_bridge;
mod agent_panel;
mod chrome;
mod layout;
mod pane_switch;
mod supervisor_client;
mod theme;
mod lua;

use std::path::PathBuf;
use std::rc::Rc;

use gtk4::gdk::{Key, ModifierType};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, EventControllerKey};

use lua::{LuaEngine, PanelEntry, PanelSlot};
use neovide_editor::NeovideEditorPane;

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
    let want_clean = std::env::args().any(|arg| arg == "--clean");

    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| build_ui(app, want_clean));
    app.run_with_args::<&str>(&[])
}

fn build_ui(app: &Application, want_clean: bool) {
    let theme = theme::Theme::dark();
    chrome::apply_css(&theme.to_css());

    // Built before the editor pane, because its `child_env()` has to be handed to the pane's
    // constructor -- those variables reach nvim through `Command::env` at spawn time and cannot
    // be added afterwards. `None` means the feature is simply unavailable (see `pane_switch`'s
    // own doc); the editor is constructed with an empty child env and behaves exactly as before.
    let mut pane_switch = pane_switch::PaneSwitch::new();
    let nvim_child_env = pane_switch.as_ref().map(|ps| ps.child_env()).unwrap_or_default();

    // `Rc` because two separate closures need it after this function returns: the agent panel's
    // Ctrl+h handler (to hand focus back) and the window's close handler (to shut nvim down).
    // `NeovideEditorPane` is deliberately not `Clone`.
    let pane = Rc::new(NeovideEditorPane::with_child_env(want_clean, nvim_child_env));

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
    let (agent_widget, agent_panel_handle) =
        agent_panel::build_agent_panel(std::env::current_dir().expect("cwd"));
    lua_engine.register_builtin_panel(
        PanelSlot::Side,
        PanelEntry { id: "agent".into(), title: "Agent".into(), widget: agent_widget },
    );

    lua_engine.load_init_file(&config_dir.join("init.lua"));

    // Build the real layout from whatever ended up in the registry -- this is what makes
    // "built-in and plugin panels share one path" a fact about the running app, not just
    // documentation. `.expect()` here is deliberate: both slots are guaranteed non-empty by this
    // point (the two `register_builtin_panel` calls above ran unconditionally, and even if
    // init.lua replaced one, replacement never leaves a slot empty).
    let (main_widget, side_widget) = {
        let panels = lua_engine.panels.borrow();
        let main_widget = panels
            .get(PanelSlot::Main)
            .map(|e| e.widget.clone())
            .expect("main slot must be populated by this point");
        let side_widget = panels
            .get(PanelSlot::Side)
            .map(|e| e.widget.clone())
            .expect("side slot must be populated by this point");
        (main_widget, side_widget)
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

    let (content_widget, _paned) = layout::build_content_area(&main_widget, &side_widget);

    root.append(&chrome::build_top_bar(&window));
    root.append(&content_widget);
    root.append(&chrome::build_status_bar());

    window.set_child(Some(&root));

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
        ps.listen(move |direction| match direction {
            'R' => {
                // A `WebView` is an ordinary focusable GTK widget -- unlike a bare `GtkGLArea`,
                // which needs `focusable(true)` set explicitly before `grab_focus()` does
                // anything. Verified in the sandbox rather than assumed; see
                // `shell/MANUAL_VERIFICATION.md`.
                let grabbed = side_widget.grab_focus();
                println!("[pane_switch] direction R -> focusing the side panel (grab_focus={grabbed})");
            }
            // The editor is already the leftmost pane, and `shell` has no vertical layout, so
            // these are the same no-ops real tmux performs at the edge of its own pane grid.
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
        let controller = EventControllerKey::new();
        controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        controller.connect_key_pressed(move |_controller, key, _keycode, state| {
            if key == Key::h && state.contains(ModifierType::CONTROL_MASK) {
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
        // left `/tmp/neovibe-pane-switch-<pid>/` behind after every close.
        if let Some(ps) = &pane_switch {
            ps.cleanup();
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
