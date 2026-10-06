//! The companion window: one agent panel in a plain titled window, no editor and no chrome of
//! Eitri's own. A skeleton that the attach, the control socket and the close prompt build on.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow};

use eitri_core::companion::attach::LinkState;
use eitri_core::companion::{Sockets, SCRATCH_CALL_LUA};
use eitri_core::keymap::TabAction;
use eitri_core::layout::{Direction, Layout, ModuleId, ModuleKeys, ModuleKind};
use eitri_core::pane_switch::PaneSwitchChannel;
use eitri_core::panel_control::{ControlServer, Request};

use super::close_watch::{CloseWatcher, ExitDecision};
use super::link::{self, CompanionLink};
use super::wm_runner::WmRunner;
use super::Start;
use crate::agent_panel::{self, AgentPanelHandle};
use crate::close_prompt::ClosePrompt;
use crate::lua::LuaEngine;
use crate::module_grid::{HostKind, ModuleGrid};
use crate::prefix_strip::PrefixStrip;
use crate::text_size::TextSizeController;
use crate::toast::Toast;
use crate::web_host::WebHost;
use crate::window_config::{self, WindowConfig};
use eitri_core::keymap::companion::{self as companion_prefix, CompanionVerb};

/// Moves the keys toward the window on one side of the panel; `false` when nothing was claimed.
type MoveFocus = Rc<dyn Fn(&ModuleId, Direction) -> bool>;

/// Everything the later steps of a companion window reach into.
#[allow(dead_code)] // read by the attach and the window-manager moves, which come next
pub(crate) struct CompanionWindow {
    pub(crate) window: ApplicationWindow,
    pub(crate) overlay: gtk4::Overlay,
    pub(crate) grid: ModuleGrid,
    pub(crate) agent: AgentPanelHandle,
    pub(crate) agent_widget: gtk4::Widget,
    pub(crate) config: WindowConfig,
    pub(crate) lua: Rc<LuaEngine>,
    pub(crate) toast: Rc<Toast>,
    pub(crate) close_prompt: Rc<ClosePrompt>,
    pub(crate) link: Rc<CompanionLink>,
    /// Bound without the shim; its listener is taken by the window's own focus moves.
    pub(crate) pane_switch: Rc<RefCell<Option<PaneSwitchChannel>>>,
}

/// The prefix's tab verbs, run as the full window runs them: the chat takes the keys first for the
/// verbs that read them, then the verb goes to the panel. Returns what the full window's
/// `run_tab_action` returns to nobody: whether to say "no such tab".
fn run_tab(
    action: TabAction,
    window: &ApplicationWindow,
    grid: &ModuleGrid,
    agent: &AgentPanelHandle,
    focus_module: &dyn Fn(&ModuleId) -> bool,
    toast: &Rc<Toast>,
) {
    use eitri_core::tabs::{plan, TabVerb};
    let keys_in = crate::pane_focus::focused_module(window, &grid.hosts())
        .as_ref()
        .map(ModuleId::kind);
    // One terminal does not exist here, so the count is zero: `n`/`p` step the chat's tabs.
    let plan = plan(action, keys_in, 0);
    if plan.takes_the_keys {
        focus_module(&ModuleId::agent());
    }
    let switched = match plan.verb {
        TabVerb::New => {
            agent.new_tab();
            agent.enter_input();
            true
        }
        TabVerb::Step(delta) => agent.step(delta),
        TabVerb::Last => agent.select_last(),
        TabVerb::Select(n) => agent.select_number(n),
        TabVerb::Rename => {
            agent.begin_rename();
            true
        }
        TabVerb::Close => {
            agent.confirm_close();
            true
        }
        TabVerb::CloseOthers => {
            agent.confirm_close_others();
            true
        }
        TabVerb::Choose => {
            agent.open_chooser();
            true
        }
        TabVerb::Info => {
            agent.open_detail();
            true
        }
        TabVerb::Flash => false,
        TabVerb::Nothing => return,
    };
    if !switched {
        // tmux: "can't find window".
        toast.show("no such tab");
    }
}

/// Builds and presents the window. The agent panel is built after `init.lua` has run and
/// `window_config::load` has read its keys, because that call applies `agent.account` and
/// `agent.user_settings`, which have to be in place before anything reads a transcript or starts a
/// sidecar.
pub(crate) fn build(app: &Application, start: &Start) -> Rc<CompanionWindow> {
    crate::xft_dpi::ensure_xft_dpi();
    gtk4::Window::set_default_icon_name(crate::APP_ID);
    let fallback = eitri_core::theme::ThemeTokens::fallback();
    let theme_css = crate::theme::gtk_css::ThemeCss::install(&fallback);

    let config_dir = crate::config_dir();
    let lua = Rc::new(LuaEngine::new(config_dir.clone()).expect("LuaEngine construction must not fail"));
    lua.load_init_file(&config_dir.join("init.lua"));
    // A companion window has no place for a Lua panel or command, and nothing runs `shell:ready`.
    let (panels, commands) = (lua.panels.borrow().entries().len(), lua.commands.borrow().len());
    if panels + commands > 0 {
        println!(
            "[companion] init.lua registers {panels} panel(s)/{commands} command(s); a companion window shows none"
        );
    }

    let config = match window_config::load(&lua) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("eitri: {message}");
            std::process::exit(1);
        }
    };
    // The prefix. A companion window has no Lua panels, so the module keys are the built-in ones.
    let module_keys = match ModuleKeys::build(&[], &config.keymap) {
        Ok(keys) => Rc::new(keys),
        Err(err) => {
            eprintln!("eitri: {err}");
            std::process::exit(1);
        }
    };
    // Every start-up failure is behind us, and nothing has started a child yet (the sidecar, the
    // window-manager tool, WebKit's processes), so none of them inherits a pipe from nvim.
    if start.from_editor {
        super::leave_editor_pipes(&start.project_root);
    }

    // How focus moves to another window, and how a window is brought forward: through the window
    // manager's own tool, run off the GTK thread.
    let runner = WmRunner::new(eitri_core::wm::resolve(config.companion_wm, &|name| {
        std::env::var_os(name)
    }));

    // The feeds are built before the panel so its context source can be handed over at
    // construction. Each sweeps its own stale directories. They carry no child environment: the
    // editor is the user's own, already running, and gets its sockets through the install.
    let feed_pump = crate::editor_feeds::FeedPump::start();
    let mut theme_feed = crate::theme::feed::ThemeFeed::new();
    let mut context_feed = eitri_core::editor_context::feed::EditorContextFeed::new();
    let mut keys_feed = eitri_core::nvim_keys::feed::NvimKeysFeed::new();
    let scratch_dir = eitri_core::scratch::ScratchDir::new();
    let scratch_path = scratch_dir.as_ref().map(|dir| dir.path().to_path_buf());
    // Without the tmux shim: nothing may change in the user's nvim environment. The channel's
    // listener is taken by the focus moves, not here.
    eitri_core::pane_switch::sweep_stale_dirs();
    let pane_switch = Rc::new(RefCell::new(PaneSwitchChannel::bind_without_shim()));
    let sockets = Sockets {
        editor_context: context_feed.as_ref().map(|feed| feed.socket_path().to_path_buf()),
        theme: theme_feed.as_ref().map(|feed| feed.socket_path().to_path_buf()),
        keys: keys_feed.as_ref().map(|feed| feed.socket_path().to_path_buf()),
        pane_switch: pane_switch
            .borrow()
            .as_ref()
            .map(|channel| channel.socket_path().to_path_buf()),
    };
    // Whether an editor is attached: the context is nothing while it is not, and starts empty for
    // the next one (the cache otherwise keeps a gone editor's file and selection for good).
    let attached = Rc::new(Cell::new(false));
    let (context_source, reset_context) = match context_feed.as_mut() {
        Some(feed) => {
            let (source, reset) = feed_pump.listen_context(feed, scratch_path.clone());
            (source, reset)
        }
        None => (
            Rc::new(|| None) as eitri_core::editor_context::ContextSource,
            Rc::new(|| {}) as Rc<dyn Fn()>,
        ),
    };
    let (agent_widget, agent) = crate::agent_panel::build_agent_panel(
        start.project_root.clone(),
        eitri_core::editor_context::gated(context_source, attached.clone()),
        scratch_dir,
        start.backend_kind,
        start.panel_notice.as_deref(),
    );
    config.apply_to_panel(&agent);
    // The panel's own `Ctrl+h/j/k/l`, the prefix's Select and an edge letter from nvim all end here.
    // `false` (no adapter) leaves the key to the page, and the band says why once.
    let move_focus: MoveFocus = {
        let runner = runner.clone();
        let agent = agent.clone();
        Rc::new(move |_from, direction| runner.move_focus(direction, &|text| agent.show_notice(text)))
    };
    let mut tokens = fallback;
    tokens.font_size_px = config.panel_font_size;
    agent.set_theme(&tokens);

    let grid = ModuleGrid::new(Rc::new(RefCell::new(Layout::companion())));
    // The same cached decision `run` read for the notice, so the two cannot disagree.
    let kind = if crate::webkit_sandbox::decision().allows_webviews() {
        HostKind::Web
    } else {
        HostKind::Direct
    };
    grid.add(ModuleId::agent(), &WebHost::new(&agent_widget), &agent_widget, kind);
    grid.apply();

    let dir_name = start
        .project_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let window = ApplicationWindow::builder()
        .application(app)
        .title(format!("Eitri \u{b7} {dir_name}"))
        .default_width(560)
        .default_height(800)
        .build();
    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(&grid));
    window.set_child(Some(&overlay));

    // The panel is the only module, so these are the pieces the full window wires around its
    // modules, each with the editor left out.
    agent_panel::install_reload_action(app, &agent);
    let panel_px = Rc::new(std::cell::Cell::new(config.panel_font_size));
    // With no editor, a size change scales the panel alone.
    let text_size = TextSizeController::install(app, None, agent.clone(), config.panel_font_size, panel_px.clone());
    let agent_host = grid
        .hosts()
        .into_iter()
        .find(|(id, _)| id.kind() == ModuleKind::Agent)
        .map(|(_, host)| host)
        .expect("the companion layout holds the agent");
    crate::wheel_zoom::install(ModuleId::agent(), &agent_host, text_size.clone());

    // Which module has the keys: the panel's cursor is solid while the window is active and the
    // keys are in it, hollow when the window manager moves focus to another window.
    {
        let agent = agent.clone();
        crate::pane_focus::install(
            &window,
            grid.live_hosts(),
            |_| {},
            move |id, has_keys| {
                if id.kind() == ModuleKind::Agent {
                    agent.set_pane_focused(has_keys);
                }
            },
        );
    }

    // There is no top bar. The toast and the prompt read a bar's height to stay below it, so they
    // are given a hidden one of no height.
    let bar = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    bar.set_visible(false);
    let toast = Toast::install(&overlay, bar.upcast_ref());
    // The one-line result of a restore at launch (`agent.restore`).
    {
        let toast = toast.clone();
        agent.on_toast(move |text| toast.show(text));
    }
    let strip = PrefixStrip::new();
    let strip_frame = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    strip_frame.add_css_class("module-toast");
    strip_frame.set_halign(gtk4::Align::End);
    strip_frame.set_valign(gtk4::Align::Start);
    strip_frame.set_margin_top(6);
    strip_frame.set_margin_end(6);
    strip_frame.set_can_target(false);
    strip_frame.set_visible(false);
    strip_frame.append(strip.widget());
    overlay.add_overlay(&strip_frame);
    let strip = Rc::new(strip);

    let focus_module: Rc<dyn Fn(&ModuleId) -> bool> = {
        let grid = grid.clone();
        Rc::new(move |id| grid.focus_target(id).is_some_and(|target| target.grab_focus()))
    };

    // The panel's own `<leader>` tab verbs (`bd`, `bo`, ...) run the code the prefix's tab chords
    // run, so a verb behaves the same whichever route sent it.
    {
        let window = window.clone();
        let grid = grid.clone();
        let agent_for_tabs = agent.clone();
        let focus_module = focus_module.clone();
        let toast = toast.clone();
        agent.on_tab_verb(move |action| run_tab(action, &window, &grid, &agent_for_tabs, &*focus_module, &toast));
    }

    // The global `f` HINT, over the panel alone.
    let hint = crate::hint::HintCoordinator::new(crate::hint::HintWidgets {
        window: window.clone(),
        overlay: overlay.clone(),
        top_items: vec![],
        editor: None,
        agent_widget: agent_widget.clone(),
        modules: {
            let grid = grid.clone();
            // A panel with no page (`webkit_sandbox`) has no labels to answer `hint_collect` with.
            let chat_has_page = start.panel_notice.is_none();
            Rc::new(move || {
                grid.hosts_in_tree_order()
                    .into_iter()
                    .filter(|(id, _)| chat_has_page || id.kind() != ModuleKind::Agent)
                    .collect()
            })
        },
        focus_module: focus_module.clone(),
        agent: agent.clone(),
    });
    {
        let hint = hint.clone();
        agent.on_hint(move |message| hint.on_panel(message));
    }

    // The `?` overlay's window and prefix sections, re-sent whenever the attached editor's own keys
    // report changes.
    let keys_help = eitri_panel::keys_help::KeysHelp::new(
        config.keymap.clone(),
        module_keys.clone(),
        agent.clone(),
        config.tmux_skipped.clone(),
        eitri_panel::keys_help::HelpScope::Companion,
    );
    keys_help.send();
    if let Some(feed) = keys_feed.as_mut() {
        let keys_help = keys_help.clone();
        feed_pump.listen_keys(feed, move |report| keys_help.nvim_report(report));
    }
    // The window's colours follow the attached editor's colourscheme, with the panel's own live size.
    if let Some(feed) = theme_feed.as_mut() {
        let theme_css = theme_css.clone();
        let window = window.clone();
        let agent = agent.clone();
        let panel_px = panel_px.clone();
        feed_pump.listen_theme(feed, move |payload| {
            let tokens = eitri_core::theme::ThemeTokens::derive(&payload);
            println!(
                "[theme] following nvim colorscheme {:?} (background={})",
                payload.options.colors_name, payload.options.background
            );
            theme_css.update(&tokens, window.upcast_ref());
            // There is no editor here, so the editor's row height stays what `derive` left.
            let mut panel_tokens = eitri_core::theme::ThemeTokens::derive(&payload);
            panel_tokens.font_size_px = crate::text_size::live_panel_font_size_px(&panel_px);
            agent.set_theme(&panel_tokens);
        });
    }
    {
        let keymap = config.keymap.clone();
        let agent = agent.clone();
        let window_for_tabs = window.clone();
        let grid = grid.clone();
        let focus_module = focus_module.clone();
        let toast = toast.clone();
        let text_size = text_size.clone();
        let hint = hint.clone();
        let move_focus = move_focus.clone();
        let strip_keymap = config.keymap.clone();
        let strip = strip.clone();
        let strip_frame = strip_frame.clone();
        crate::prefix::install(
            &window,
            keymap.clone(),
            module_keys,
            move |command, keycode| {
                let action = match command {
                    crate::prefix::PrefixCommand::Run(action) => action,
                    // A module key after a split key: there is nothing to place.
                    crate::prefix::PrefixCommand::Place { module, axis } => {
                        println!("[companion] prefix: place {module} {axis:?} refused");
                        toast.show(companion_prefix::REFUSED_TEXT);
                        return;
                    }
                };
                match companion_prefix::classify(&action) {
                    CompanionVerb::Tab(tab) => run_tab(tab, &window_for_tabs, &grid, &agent, &*focus_module, &toast),
                    CompanionVerb::Reload => agent.reload_document_by_hand(),
                    CompanionVerb::Keymap => {
                        focus_module(&ModuleId::agent());
                        agent.open_keymap();
                    }
                    CompanionVerb::CommandLine => {
                        focus_module(&ModuleId::agent());
                        agent.open_command_line();
                    }
                    CompanionVerb::Text(change) => text_size.apply_panel(change.into()),
                    CompanionVerb::Hint => hint.toggle_from_prefix(keycode),
                    CompanionVerb::Literal => {
                        if let Some(key) = crate::prefix::literal_for(&action, &keymap) {
                            agent.literal_key(&key);
                        }
                    }
                    CompanionVerb::Select(direction) => {
                        move_focus(&ModuleId::agent(), direction);
                    }
                    CompanionVerb::Refuse => {
                        println!("[companion] prefix: {action:?} refused");
                        toast.show(companion_prefix::REFUSED_TEXT);
                    }
                }
            },
            move |waiting| {
                let pieces = companion_prefix::strip_pieces(waiting, &strip_keymap);
                strip.show(&pieces);
                strip_frame.set_visible(!pieces.is_empty());
            },
        );
    }

    // Installed after the prefix, so its capture controller sees a key before the prefix's while
    // the question is open.
    let close_prompt = ClosePrompt::install(
        &overlay,
        &window,
        bar.upcast_ref(),
        strip_frame.upcast_ref::<gtk4::Widget>(),
    );

    // The panel's own `Ctrl+h/j/k/l`: claimed for the composer where the panel says so, else a move
    // toward the neighbouring window.
    // The page's stale-mirror recovery and its `pane_nav` ask for the move the chord would have made.
    {
        let move_focus = move_focus.clone();
        agent.on_nav_fallthrough(move |direction| {
            move_focus(&ModuleId::agent(), direction);
        });
    }
    crate::install_module_nav(
        ModuleId::agent(),
        &agent_host,
        move_focus.clone(),
        Some(agent.nav_intercept()),
    );
    // No key with Super or Hyper held reaches the panel's page (`panel_super.rs`).
    crate::panel_super::install(&agent_host);

    // `eitri-supervisor` asks a window to come to the front.
    {
        let window = window.downgrade();
        let agent = agent.clone();
        let runner = runner.clone();
        gtk4::glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
            let Some(window) = window.upgrade() else {
                return gtk4::glib::ControlFlow::Break;
            };
            if agent.poll_activate() {
                window.present();
                runner.raise_pid(std::process::id());
            }
            gtk4::glib::ControlFlow::Continue
        });
    }

    // Where the panel stands with the user's nvim. The first call of `on_change` is at once, so the
    // band says what is true from the first `ready`.
    let link = {
        let attached = attached.clone();
        let agent_for_change = agent.clone();
        let agent_for_cancel = agent.clone();
        let runner_for_change = runner.clone();
        link::start(
            start.nvim.clone(),
            sockets,
            move |state, band, peer_pid| {
                let was = attached.get();
                let now = matches!(state, LinkState::Attached { .. });
                attached.set(now);
                // A new editor starts empty: forget the last one's file and selection as a connect
                // begins and as an attached editor goes. Not on arriving at `Attached`: the install
                // makes the editor send its first context, and that line may be read just before.
                if matches!(state, LinkState::Connecting { .. }) || (was && !now) {
                    reset_context();
                }
                // GNOME's extension is told which window the editor is once it is known, and that
                // there is none once it is gone. A link still connecting or installing leaves the
                // partner as it is: a request that just brought this panel forward named its own. The
                // window is looked for from the pid of the process holding the editor's socket, not
                // from the pid the editor reported about itself, and where the platform gives none
                // there is no partner.
                match state {
                    LinkState::Attached { .. } => runner_for_change.set_partner_from(peer_pid),
                    LinkState::NoEditor | LinkState::Detached { .. } | LinkState::Failed { .. } => {
                        runner_for_change.set_partner_from(None)
                    }
                    LinkState::Connecting { .. } | LinkState::Attaching { .. } => {}
                }
                println!("[companion] link: {} {}", band.state, band.text);
                agent_for_change.set_editor_link(band);
            },
            move || {
                let cut = agent_for_cancel.editor_detached();
                if cut > 0 {
                    println!("[companion] ended {cut} draft edit(s) out in the editor");
                }
                agent_for_cancel.review_editor_lost();
            },
        )
    };

    // Turn review asks the attached nvim through the same link, and polls its answers.
    agent.set_editor_rpc(link.clone());
    {
        let owner = link.clone();
        agent.set_review_owner(move || owner.review_owner());
        // A file the review opened is in the user's own window: bringing it forward is the window
        // manager's job, as for a draft.
        let link = link.clone();
        let runner = runner.clone();
        agent.on_review_open(move || {
            if let Some(pid) = link.nvim_pid() {
                runner.raise_editor(pid);
            }
        });
    }

    // The edge letters nvim's own navigator sends when the cursor is at nvim's last window: they
    // continue as a move toward the window beyond it. The channel carries no shim, so only a
    // navigator that was told the socket (the installed nvim glue) ever writes here.
    if let Some(channel) = pane_switch.borrow_mut().as_mut() {
        let runner = runner.clone();
        let agent = agent.clone();
        feed_pump.listen_pane_switch(channel, move |message| {
            if let crate::pane_switch::PaneMessage::Direction(letter) = message {
                match crate::pane_switch::letter_direction(letter) {
                    Some(direction) => {
                        runner.editor_edge(direction, &|text| agent.show_notice(text));
                    }
                    None => println!("[pane_switch] unknown direction {letter:?}, ignoring"),
                }
            }
        });
    }

    // A draft or a `gf` goes to the editor as one call; the editor is not shown or focused, it is
    // the user's own window.
    {
        let link = link.clone();
        let runner = runner.clone();
        agent.on_editor_request(move |request| {
            link.exec_lua_for("scratch", SCRATCH_CALL_LUA, vec![request.hex().into()])?;
            // The editor is the user's own window, so bringing it forward is the window manager's
            // job; nothing here waits for it.
            if let Some(pid) = link.nvim_pid() {
                runner.raise_editor(pid);
            }
            Ok(())
        });
    }
    // An edit that came back: the keys return to the panel, on the oldest card if one waits.
    {
        let agent_for_done = agent.clone();
        let focus_module = focus_module.clone();
        let runner = runner.clone();
        agent.on_editor_done(move || {
            focus_module(&ModuleId::agent());
            runner.raise_pid(std::process::id());
            if !agent_for_done.focus_oldest_card() {
                agent_for_done.arrive();
            }
        });
    }

    // What a second `eitri panel` or `:EitriPanel` asked of this one, read off the control socket's
    // thread. Only the last attach counts: two inside one tick would otherwise connect twice.
    let server: Option<Rc<ControlServer>> = start.control.take().map(Rc::new);
    // The panel closes when the editor it was started with exits (`eitri split`). The watch belongs
    // to the attachment the accepted requests chose, so the drain below feeds it every request.
    let close_watcher = Rc::new(RefCell::new(CloseWatcher::new()));
    // How many accepted requests this side has taken off the control thread's queue.
    let taken = Rc::new(Cell::new(0u64));
    // Takes what the control thread has already accepted and acts on it. It runs on a timer, and
    // again the moment a watched editor exits: the control thread answers `ok` before this side
    // reads the request, so an editor can exit between the two, and its exit must not close a panel
    // that was just given another editor. `false` once the window is gone.
    let drain_requests: Rc<dyn Fn() -> bool> = {
        let window = window.downgrade();
        let link = link.clone();
        let runner = runner.clone();
        let close_watcher = Rc::downgrade(&close_watcher);
        let server = server.clone();
        let taken = taken.clone();
        Rc::new(move || {
            let Some(window) = window.upgrade() else {
                return false;
            };
            let (Some(server), Some(close_watcher)) = (&server, close_watcher.upgrade()) else {
                return true;
            };
            let mut attach = None;
            let mut partner = None;
            let mut present = false;
            for received in server.requests().try_iter() {
                taken.set(taken.get() + 1);
                // Taken before the request is taken apart; only the last attach counts, partner included.
                let chain = super::requests::partner_chain(&received).map(<[u32]>::to_vec);
                match received.request {
                    Request::Attach { addr, close_with } => {
                        close_watcher.borrow_mut().on_request(&addr, close_with);
                        attach = Some(addr);
                        partner = chain;
                        present = true;
                    }
                    Request::Raise => present = true,
                }
            }
            // Read before the request is handed to the link: a request for the editor already attached
            // changes nothing there, and the raise then has to give the partner back to that editor.
            let editor_pid = attach.as_deref().and_then(|addr| link.attached_to(addr));
            if let Some(addr) = attach {
                link.attach(addr);
            }
            if present {
                window.present();
                runner.raise_for_request(partner, editor_pid);
            }
            true
        })
    };
    // The generation of an exit that is waiting for a request still on its way; decided again on
    // each drain until that request is in. A request that replaces the watch makes it moot.
    let waiting_exit: Rc<Cell<Option<u64>>> = Rc::new(Cell::new(None));
    // What an editor's exit means once the requests taken so far are known.
    let settle_exit: Rc<dyn Fn(u64)> = {
        let window = window.downgrade();
        let watcher = Rc::downgrade(&close_watcher);
        let server = server.clone();
        let (taken, waiting_exit) = (taken.clone(), waiting_exit.clone());
        Rc::new(move |generation| {
            let Some(watcher) = watcher.upgrade() else { return };
            let accepted = server.as_ref().map_or(0, |server| server.accepted());
            let decision = watcher.borrow().exit_decision(generation, accepted, taken.get());
            waiting_exit.set((decision == ExitDecision::Wait).then_some(generation));
            if decision == ExitDecision::Close {
                if let Some(window) = window.upgrade() {
                    // The same close as the window's own: a running turn still asks first.
                    window.close();
                }
            }
        })
    };
    {
        let drain_requests = drain_requests.clone();
        let settle_exit = settle_exit.clone();
        close_watcher.borrow_mut().set_on_exit(move |generation| {
            // Requests that were already accepted come first: one of them may have replaced the
            // watch that fired, and that editor's exit then says nothing about this panel. One the
            // control thread answered `ok` but has not delivered yet defers the decision.
            if drain_requests() {
                settle_exit(generation);
            }
        });
    }
    if let (Some((close_with, child)), Some(addr)) = (start.close_with.take(), start.nvim.clone()) {
        close_watcher.borrow_mut().install(addr, close_with, child);
    }
    // The one strong holder of the close watcher, for the window's life: everything else refers to
    // it weakly, since it holds the exit callback that holds the drain.
    {
        let (drain_requests, settle_exit, waiting_exit) =
            (drain_requests.clone(), settle_exit.clone(), waiting_exit.clone());
        gtk4::glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
            let _keeps = &close_watcher;
            if drain_requests() {
                if let Some(generation) = waiting_exit.get() {
                    settle_exit(generation);
                }
                gtk4::glib::ControlFlow::Continue
            } else {
                gtk4::glib::ControlFlow::Break
            }
        });
    }

    {
        let app = app.clone();
        let agent = agent.clone();
        let lua = lua.clone();
        let close_prompt = close_prompt.clone();
        let link = link.clone();
        let pane_switch = pane_switch.clone();
        window.connect_close_request(move |_| {
            // A running or queued turn is worth one y/n; `y` closes the window again and this sees
            // the confirmation.
            if let Some(text) = eitri_core::tabs::window_close_prompt(agent.running_count(), agent.queued_count()) {
                if !close_prompt.confirmed() {
                    close_prompt.ask_to_close_window(&text);
                    return gtk4::glib::Propagation::Stop;
                }
            }
            // Nothing below waits on nvim: the teardown is queued and the connection closes once
            // it is written. Everything is explicit and safe to run twice, because GTK does not
            // reliably free this closure (and so run any `Drop`) before the process ends, and the
            // `y` answer comes back through here.
            link.shutdown();
            if let Some(server) = &server {
                server.cleanup();
            }
            if let Some(channel) = pane_switch.borrow().as_ref() {
                channel.cleanup();
            }
            if let Some(feed) = &theme_feed {
                feed.cleanup();
            }
            if let Some(feed) = &context_feed {
                feed.cleanup();
            }
            if let Some(feed) = &keys_feed {
                feed.cleanup();
            }
            if let Some(path) = &scratch_path {
                let _ = std::fs::remove_dir_all(path);
            }
            agent_panel::hold_until_done(&app, agent.shutdown());
            // Held for the window's life: GTK does not reliably free this closure before the
            // process ends, and both must outlive the panel's teardown.
            let _ = (&lua, &theme_css);
            gtk4::glib::Propagation::Proceed
        });
    }
    window.present();
    // As the full window gives its first module the keys after `present()`: focusing an unshown
    // widget is meaningless.
    if let Some(target) = grid.focus_target(&ModuleId::agent()) {
        target.grab_focus();
    }
    println!("[companion] panel window up, project {}", start.project_root.display());

    Rc::new(CompanionWindow {
        window,
        overlay,
        grid,
        agent,
        agent_widget,
        config,
        lua,
        toast,
        close_prompt,
        link,
        pane_switch,
    })
}
