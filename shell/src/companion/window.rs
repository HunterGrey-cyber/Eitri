//! The companion window: one agent panel in a plain titled window, no editor and no chrome of
//! Eitri's own. A skeleton that the attach, the control socket and the close prompt build on.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow};

use eitri_core::companion::attach::LinkState;
use eitri_core::companion::{Sockets, SCRATCH_CALL_LUA};
use eitri_core::editor_rpc::EditorRpc;
use eitri_core::keymap::TabAction;
use eitri_core::layout::{Direction, Layout, ModuleId, ModuleKeys, ModuleKind};
use eitri_core::pane_switch::PaneSwitchChannel;
use eitri_core::panel_control::{ControlServer, Request};
use eitri_core::review_editor::Owner;
use eitri_core::scratch::ScratchRequest;

use super::close_watch::{CloseWatcher, ExitDecision};
use super::link::{self, CompanionLink};
use super::wm_runner::WmRunner;
use super::Start;
use crate::agent_panel::{self, AgentPanelHandle, HintInbound, ViewId};
use crate::close_prompt::ClosePrompt;
use crate::lua::LuaEngine;
use crate::module_grid::{HostKind, ModuleGrid};
use crate::panel_wiring::{wire_panel, PanelHooks};
use crate::prefix_strip::PrefixStrip;
use crate::text_size::TextSizeController;
use crate::toast::Toast;
use crate::web_host::WebHost;
use crate::window_config::{self, WindowConfig};
use eitri_core::keymap::companion::{self as companion_prefix, CompanionVerb};

/// A window explains unavailable layout keys once; later presses leave a log line.
#[derive(Default)]
struct RefusedKeys(Cell<bool>);

impl RefusedKeys {
    fn say(&self, action: &str, toast: impl FnOnce(&str), log: impl FnOnce(&str)) {
        if self.0.replace(true) {
            log(&format!("[companion] prefix: {action} refused"));
        } else {
            toast(companion_prefix::BESIDE_OFF_TEXT);
        }
    }
}

/// A page follows only the editor attached when its key was pressed.
fn page_context(
    state: &LinkState,
    project: &std::path::Path,
    exe: &std::path::Path,
    control_dir: &std::path::Path,
) -> Result<crate::page_window::BesideCtx, String> {
    let LinkState::Attached {
        addr,
        in_tmux,
        nvim_pid,
        ..
    } = state
    else {
        return Err("no editor attached: run :EitriPanel in nvim".to_owned());
    };
    Ok(crate::page_window::BesideCtx {
        nvim: addr.clone(),
        project: project.to_owned(),
        in_tmux: *in_tmux,
        nvim_pid: Some(*nvim_pid),
        control_dir: control_dir.to_owned(),
        exe: exe.to_owned(),
        exe_args: Vec::new(),
        search: None,
    })
}

/// Moves the keys toward the window on one side of the panel; `false` when nothing was claimed.
type MoveFocus = Rc<dyn Fn(&ModuleId, Direction) -> bool>;

/// Hands a draft or a `gf` to the attached editor; `Err` says why it could not.
type EditorRequest = Rc<dyn Fn(&ScratchRequest) -> Result<(), String>>;

/// What the window needs of its link to reach the agent slot's parts in the attached nvim.
pub(crate) trait SlotCalls {
    /// One named call to the slot's part, guarded by the channel the link is attached on now; `Err` says
    /// why it went nowhere (not attached).
    fn slot_call(&self, name: &str, args: Vec<rmpv::Value>) -> Result<(), String>;
}

impl SlotCalls for CompanionLink {
    fn slot_call(&self, name: &str, args: Vec<rmpv::Value>) -> Result<(), String> {
        CompanionLink::slot_call(self, name, args)
    }
}

/// The panel's activity, carried to the reload part in the attached nvim: the busy state when it flips and
/// again for each editor that attaches (the part starts idle), `now` at each turn end.
pub(crate) struct ReloadCalls {
    link: RefCell<Option<Weak<dyn SlotCalls>>>,
    /// The panel's busy state now.
    busy: Box<dyn Fn() -> bool>,
    /// The channel the busy state was last sent to, so a repeated report of the same attachment sends once.
    announced: Cell<Option<u64>>,
    /// Where the busy edges are said, one line each: the companion's stdout in the product.
    say: Box<dyn Fn(&str)>,
}

impl ReloadCalls {
    pub(crate) fn new(busy: impl Fn() -> bool + 'static) -> Rc<ReloadCalls> {
        Self::with_log(busy, |line| println!("{line}"))
    }

    fn with_log(busy: impl Fn() -> bool + 'static, say: impl Fn(&str) + 'static) -> Rc<ReloadCalls> {
        Rc::new(ReloadCalls {
            link: RefCell::new(None),
            busy: Box::new(busy),
            announced: Cell::new(None),
            say: Box::new(say),
        })
    }

    /// The link the calls go through, held weakly: the link's own poll holds this window's callbacks.
    pub(crate) fn bind(&self, link: Weak<dyn SlotCalls>) {
        *self.link.borrow_mut() = Some(link);
    }

    /// The panel's busy state flipped: said as `[reload] busy on` or `[reload] busy off` (the window pass
    /// reads these), then sent on.
    pub(crate) fn busy_changed(&self, on: bool) {
        (self.say)(if on { "[reload] busy on" } else { "[reload] busy off" });
        self.call("busy", vec![rmpv::Value::from(on)]);
    }

    /// One tab's turn ended.
    pub(crate) fn turn_ended(&self) {
        self.call("now", Vec::new());
    }

    /// The link's state changed: an editor that has just attached is told the busy state the panel has now,
    /// once its install has answered (the link is `Attached` only then). A state that is not attached forgets
    /// the channel, so the next attachment is told again; so does a connect that began in this report, because
    /// it may have attached a different editor whose channel has the same number (a connect and its install can
    /// finish within one poll, with no other state reported between). Nothing is sent to a channel that has
    /// gone, because the link only calls through the channel it is attached on.
    pub(crate) fn link_changed(&self, state: &LinkState, connect_began: bool) {
        if connect_began {
            self.announced.set(None);
        }
        match state {
            LinkState::Attached { channel, .. } => {
                if self.announced.replace(Some(*channel)) != Some(*channel) {
                    self.call("busy", vec![rmpv::Value::from((self.busy)())]);
                }
            }
            _ => self.announced.set(None),
        }
    }

    fn call(&self, name: &str, args: Vec<rmpv::Value>) {
        let link = self.link.borrow().as_ref().and_then(Weak::upgrade);
        if let Some(link) = link {
            // With no editor attached there is no part to tell: the next attachment is sent the state.
            let _ = link.slot_call(name, args);
        }
    }
}

/// The companion panel as the agents layer's target. Its one chat view is the base view, so a reveal selects the
/// tab there, gives the view the keys and lands as the full window's arrival does (on that tab's oldest waiting card
/// with `card_first`, else in the chat; a stop lands in the chat), then presents the window; `new` is `tab.new`.
/// Nothing here answers a card.
struct CompanionAgents {
    window: gtk4::glib::WeakRef<ApplicationWindow>,
    agent: AgentPanelHandle,
    runner: Rc<WmRunner>,
    focus_module: Rc<dyn Fn(&ModuleId) -> bool>,
    run_tab: Rc<dyn Fn(TabAction)>,
}

impl CompanionAgents {
    fn present(&self, token: Option<&str>) {
        if let Some(window) = self.window.upgrade() {
            if token.is_some() || !window.is_active() {
                crate::linked_window::focus::present_with_token(&window, token);
                self.runner.raise_pid(std::process::id());
            }
        }
    }
}

impl crate::agents_host::AgentsTarget for CompanionAgents {
    fn reveal(&self, req: &eitri_core::wm::RevealRequest, token: Option<&str>) -> eitri_core::agents::control::Reply {
        use eitri_core::wm::RevealRefusal;
        if let Some(tab) = req.tab {
            let refused = refused_before_showing(
                req.intent,
                || self.agent.view_of_tab(tab),
                || self.agent.tab_moving(tab),
                || self.agent.tab_has_work(tab),
            );
            if let Some(why) = refused {
                return crate::agents_host::refusal(why);
            }
            if !self.agent.select_tab(tab) {
                return crate::agents_host::refusal(RevealRefusal::Gone);
            }
        }
        (self.focus_module)(&ModuleId::agent());
        self.present(token);
        let landed = req.card_first
            && match req.tab {
                Some(tab) => self.agent.focus_card_of(tab),
                None => self.agent.focus_oldest_card(ViewId::BASE),
            };
        if !landed || req.intent == eitri_core::agents::Intent::Stop {
            self.agent.arrive_with_intent(ViewId::BASE, req.intent);
        }
        eitri_core::agents::control::Reply::Ok
    }

    fn new_tab(&self, token: Option<&str>) -> eitri_core::agents::control::Reply {
        (self.run_tab)(TabAction::New);
        self.present(token);
        eitri_core::agents::control::Reply::Ok
    }
}

/// Why a request for one tab of the companion panel is refused before anything is shown, the full window's reveal's
/// checks in its order: a tab not in the panel's one view is gone, a moving one waits, and a stop of a tab with nothing
/// running is refused as idle, so the screen and the keys stay as they were.
fn refused_before_showing(
    intent: eitri_core::agents::Intent,
    view: impl FnOnce() -> Option<ViewId>,
    moving: impl FnOnce() -> bool,
    has_work: impl FnOnce() -> bool,
) -> Option<eitri_core::wm::RevealRefusal> {
    use eitri_core::wm::RevealRefusal;
    if view() != Some(ViewId::BASE) {
        return Some(RevealRefusal::Gone);
    }
    if moving() {
        return Some(RevealRefusal::Moving);
    }
    if intent == eitri_core::agents::Intent::Stop && !has_work() {
        return Some(RevealRefusal::Idle);
    }
    None
}

/// Starts the agents layer for the companion panel: its tabs' rows published as the window `companion`, its control
/// socket served here, and both stopped when the window goes. `None` with `agents.enabled = false`.
fn start_agents(
    app: &Application,
    window: &ApplicationWindow,
    lua: &LuaEngine,
    project_root: &std::path::Path,
    target: CompanionAgents,
) -> Option<Rc<crate::agents_host::AgentsHost>> {
    use crate::agents_host::{self, AgentsHost, RowsTo, Setup};
    let agent = target.agent.clone();
    let rows: RowsTo = agents_host::rows_to_panel(agent.clone(), window);
    let token: Rc<dyn Fn() -> Option<String>> = {
        let window = window.downgrade();
        Rc::new(move || crate::window_services::activation_token(&window))
    };
    let host = AgentsHost::start(
        Setup {
            enabled: agents_host::enabled(lua.config.borrow().get(eitri_core::agent_prefs::AGENTS_ENABLED_KEY)),
            control_dir: agents_host::control_dir(),
            project: agents_host::project_ref(project_root),
            window_name: "companion".to_owned(),
            focused: window.is_active(),
            publish: true,
        },
        Rc::new(target),
        token,
        rows,
    )?;
    window.connect_is_active_notify(move |window| agent.set_agents_focused(window.is_active()));
    agents_host::stop_with(app, window, &host);
    agents_host::serve_this_window(&host);
    Some(host)
}

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

/// Text-size events use the same controller as a full window, over the companion's sole chat layout (`parts`,
/// whose views the grid holds).
fn text_size_driver(
    parts: eitri_core::wm::WindowParts,
    agent: &agent_panel::AgentPanelHandle,
    panel_px: Rc<Cell<f32>>,
    base: f32,
) -> Rc<eitri_core::wm::Driver> {
    use crate::gtk_host::{GtkHost, ServiceSet};
    use eitri_core::wm::*;

    struct TextServices {
        agent: agent_panel::AgentPanelHandle,
        panel_px: Rc<Cell<f32>>,
    }
    impl ServiceSet for TextServices {
        fn editor(&self, _: &EditorCall) -> Result<(), String> {
            Err("this window has no editor".into())
        }
        fn terminal(&self, op: &TermOp) -> Result<(), String> {
            // Every size push carries the terminals' zoom; with none here it is done, not refused.
            if let TermOp::SetZoom { .. } = op {
                return Ok(());
            }
            Err("this window has no terminal".into())
        }
        fn plugin(&self, _: &PluginOp) -> Result<(), String> {
            Err("this window has no plugin page".into())
        }
        fn chat(&self, op: &ChatOp) -> Result<(), String> {
            if let ChatOp::SetFontSize { px, .. } = op {
                self.panel_px.set(*px);
                self.agent.set_panel_font_size_px(*px);
                Ok(())
            } else {
                Err("not carried out by this shell yet".into())
            }
        }
    }
    let scheduler = crate::gtk_host::scheduler::GlibScheduler::new();
    let host = GtkHost::new();
    host.set_services(Rc::new(TextServices {
        agent: agent.clone(),
        panel_px,
    }));
    let mut controller = Controller::new(scheduler.clone());
    controller.open_window(
        WinId::MAIN,
        WindowSpec {
            role: WindowRole::Main,
            presence: EditorPresence::None,
            title: String::new(),
        },
        parts,
    );
    controller.set_text_size(WinId::MAIN, base, 1.0);
    let driver = Driver::new(controller, host.clone(), scheduler.clone());
    let sink: Rc<dyn HostSink> = driver.clone();
    host.attach(Rc::downgrade(&sink));
    scheduler.attach(Rc::downgrade(&sink));
    driver
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
    use eitri_core::tabs::plan;
    let keys_in = super::focus::focused_module(window, &grid.hosts())
        .as_ref()
        .map(ModuleId::kind);
    // One terminal does not exist here, so the count is zero: `n`/`p` step the chat's tabs.
    let plan = plan(action, keys_in, 0, false);
    if plan.takes_the_keys {
        focus_module(&ModuleId::agent());
    }
    let answer = eitri_core::wm::verbs::tabs::base_call(&plan.verb)
        .map(|call| crate::gtk_host_services::apply_tab_call(agent, &call));
    if tab_failed(&plan.verb, answer) {
        toast.show("no such tab");
    }
}

pub(crate) fn tab_failed(verb: &eitri_core::tabs::TabVerb, answer: Option<bool>) -> bool {
    matches!(verb, eitri_core::tabs::TabVerb::Flash) || answer == Some(false)
}

/// What the companion's panel hooks act on, each built by the window from its own pieces.
pub(crate) struct CompanionHookParts {
    /// The one-line result of a restore at launch (`agent.restore`), in the window's toast.
    pub(crate) toast: Rc<dyn Fn(&str)>,
    /// The panel's own `<leader>` tab verbs (`bd`, `bo`, ...), run as the prefix's tab chords run them.
    pub(crate) run_tab: Rc<dyn Fn(TabAction)>,
    /// The panel's half of the global `f` HINT.
    pub(crate) hint: Rc<dyn Fn(HintInbound)>,
    /// A move toward the window on one side of the panel; `false` when nothing was claimed.
    pub(crate) move_focus: MoveFocus,
    /// The attached nvim, which turn review asks through the window's link.
    pub(crate) editor_rpc: Rc<dyn EditorRpc>,
    /// Which editor holds the review overlay, as the link knows it.
    pub(crate) review_owner: Rc<dyn Fn() -> Option<Owner>>,
    /// A file the review opened is in the user's own window: brings it forward.
    pub(crate) review_open: Rc<dyn Fn()>,
    /// A draft or a `gf`, handed to the attached editor as one call.
    pub(crate) editor_request: EditorRequest,
    /// An edit came back: the keys return to the panel.
    pub(crate) editor_done: Rc<dyn Fn()>,
    /// The editor's "edit the selection" key: the window comes forward with the keys in the panel and its
    /// composer open.
    pub(crate) edit_selection: Rc<dyn Fn()>,
    /// The attached editor's reload part, told the panel's activity.
    pub(crate) reload: Rc<ReloadCalls>,
}

/// The companion's panel hooks: every one the main window installs but the five in
/// [`COMPANION_OMITS`](crate::panel_wiring::COMPANION_OMITS). The window has one chat view, the
/// base one, so the view a hook is handed is always that view.
pub(crate) fn companion_hooks(parts: CompanionHookParts) -> PanelHooks {
    let CompanionHookParts {
        toast,
        run_tab,
        hint,
        move_focus,
        editor_rpc,
        review_owner,
        review_open,
        editor_request,
        editor_done,
        edit_selection,
        reload,
    } = parts;
    PanelHooks {
        on_hint: Some(Box::new(move |message: HintInbound| hint(message))),
        on_toast: Some(Box::new({
            let toast = toast.clone();
            move |text: &str| toast(text)
        })),
        on_warning: Some(Box::new(move |text: &str| toast(text))),
        on_editor_request: Some(Box::new(move |request: &ScratchRequest| editor_request(request))),
        set_editor_rpc: Some(editor_rpc),
        set_review_owner: Some(Box::new(move || review_owner())),
        on_review_open: Some(Box::new(move || review_open())),
        on_editor_done: Some(Box::new(move |_view: ViewId| editor_done())),
        on_edit_selection: Some(Box::new(move || edit_selection())),
        // The page's stale-mirror recovery and its `pane_nav` ask for the move the chord would have made.
        on_nav_fallthrough: Some(Box::new(move |_view: ViewId, direction: Direction| {
            move_focus(&ModuleId::agent(), direction);
        })),
        on_tab_verb: Some(Box::new(move |_view: ViewId, action: TabAction| run_tab(action))),
        on_busy_changed: Some(Box::new({
            let reload = reload.clone();
            move |busy: bool| reload.busy_changed(busy)
        })),
        on_turn_ended: Some(Box::new(move || reload.turn_ended())),
        ..PanelHooks::default()
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
    let theme_css = crate::theme::gtk_css::ThemeCss::install(&fallback, None);

    let config_dir = crate::config_dir();
    eitri_core::luals::refresh_in_background();
    let lua = Rc::new(LuaEngine::without_plugins(config_dir.clone()).expect("LuaEngine construction must not fail"));
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
    for line in config.keymap.dropped_for_root() {
        eprintln!("[keymap] {line}");
    }
    // Page keys name separate windows; no plugin is loaded into this panel.
    let page_keys: Vec<_> = lua
        .recorded_plugin_keys()
        .into_iter()
        .map(|(id, key)| (ModuleId::lua(&id), Some(key.to_string())))
        .collect();
    // As in the full window, a page key that cannot be one is reported and the page has no key; it never keeps
    // the window from opening.
    let module_keys = {
        let (keys, refused) = ModuleKeys::build_settled(&page_keys, &config.keymap);
        for error in refused {
            eprintln!("[companion] {error}; the page has no key");
        }
        Rc::new(keys)
    };
    // Every start-up failure is behind us, and nothing has started a child yet (the sidecar, the
    // window-manager tool, WebKit's processes), so none of them inherits a pipe from nvim.
    crate::window_log::start_companion(&start.project_root, start.from_editor);

    // How focus moves to another window, and how a window is brought forward: through the window
    // manager's own tool, run off the GTK thread.
    let runner = WmRunner::new(eitri_core::compositor::resolve(config.companion_wm, &|name| {
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
        // The panel's activity drives the reload timer through the link's busy and now calls.
        reload_always: false,
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
        true,
        start.root_note.clone(),
        None,
        eitri_core::window_link::pair::Role::Main,
        fallback.clone(),
    );
    config.apply_to_panel(&agent);
    // A companion window is never one of a pair: it offers its own saved tabs, once `agent.restore` is known.
    agent.set_tab_files(Default::default());
    // The panel reads the context when its tick runs, and its tick runs only when woken.
    crate::editor_feeds::wake_panel_on_context(&feed_pump, &agent);
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

    // The companion's one layout: the chat alone, given to its text-size controller below, the grid reading it.
    let parts = eitri_core::wm::WindowParts::new(Layout::companion(), eitri_core::layout::Workspaces::single(), None);
    let grid = ModuleGrid::new(parts.views().0);
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
    // One chat page: the reload is always the base view's.
    agent_panel::install_reload_action(app, &agent, Rc::new(|| ViewId::BASE));
    let panel_px = Rc::new(std::cell::Cell::new(config.panel_font_size));
    // With no editor, a size change scales the panel alone.
    let text_driver = text_size_driver(parts, &agent, panel_px.clone(), config.panel_font_size);
    let text_size = TextSizeController::install(app, None, text_driver);
    let agent_host = grid
        .hosts()
        .into_iter()
        .find(|(id, _)| id.kind() == ModuleKind::Agent)
        .map(|(_, host)| host)
        .expect("the companion layout holds the agent");

    // Which module has the keys: the panel's cursor is solid while the window is active and the
    // keys are in it, hollow when the window manager moves focus to another window.
    {
        let agent = agent.clone();
        super::focus::install(
            &window,
            grid.live_hosts(),
            |_| {},
            move |id, has_keys| {
                if id.kind() == ModuleKind::Agent {
                    agent.set_pane_focused(ViewId::BASE, has_keys);
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
    let toast_hook: Rc<dyn Fn(&str)> = {
        let toast = toast.clone();
        Rc::new(move |text: &str| toast.show(text))
    };
    let strip = PrefixStrip::new();
    let strip_frame = crate::prefix_strip::floating_frame(&strip);
    overlay.add_overlay(&strip_frame);
    let strip = Rc::new(strip);

    let focus_module: Rc<dyn Fn(&ModuleId) -> bool> = {
        let grid = grid.clone();
        Rc::new(move |id| grid.focus_target(id).is_some_and(|target| target.grab_focus()))
    };

    // The panel's own `<leader>` tab verbs (`bd`, `bo`, ...) run the code the prefix's tab chords
    // run, so a verb behaves the same whichever route sent it.
    let run_tab_hook: Rc<dyn Fn(TabAction)> = {
        let window = window.clone();
        let grid = grid.clone();
        let agent_for_tabs = agent.clone();
        let focus_module = focus_module.clone();
        let toast = toast.clone();
        Rc::new(move |action: TabAction| run_tab(action, &window, &grid, &agent_for_tabs, &*focus_module, &toast))
    };

    // The agents layer: this panel's tabs are rows like any window's, and another window's request brings one
    // forward here. Started off this thread; nothing is made with `agents.enabled = false`.
    let agents = start_agents(
        app,
        &window,
        &lua,
        &start.project_root,
        CompanionAgents {
            window: window.downgrade(),
            agent: agent.clone(),
            runner: runner.clone(),
            focus_module: focus_module.clone(),
            run_tab: run_tab_hook.clone(),
        },
    );
    // Remote control, as in a full window: this panel's tabs are published, so a phone that sees them reaches them
    // through this window's `rv-` socket.
    #[cfg(target_os = "linux")]
    crate::remote_switch::startup_notices(&lua.startup_problems(), &*crate::remote_switch::toast_notice(&toast));
    #[cfg(target_os = "linux")]
    crate::remote_switch::start(
        &window,
        &agent,
        agents.as_ref().map(|host| host.instance()),
        &lua.config.borrow(),
        crate::remote_switch::toast_notice(&toast),
    );

    // The global `f` HINT, over the panel alone.
    let hint = crate::hint::HintCoordinator::new(crate::hint::HintWidgets {
        window: window.clone(),
        overlay: overlay.clone(),
        top_items: Rc::default(),
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
        // The panel's window has no containers, so no strips.
        strips: Rc::new(Vec::new),
        activate_strip: Rc::new(|_: &ModuleId| {}),
        agent: agent.clone(),
    });
    let hint_hook: Rc<dyn Fn(HintInbound)> = {
        let hint = hint.clone();
        Rc::new(move |message: HintInbound| hint.on_panel(message))
    };

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
    let page_link: Rc<RefCell<Option<Weak<CompanionLink>>>> = Rc::default();
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
        let refused = RefusedKeys::default();
        let page_link_for_key = page_link.clone();
        let page_project = start.project_root.clone();
        let page_exe = std::env::current_exe().expect("the running executable has a path");
        let page_control_dir = eitri_core::panel_control::control_dir(
            std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
            &std::env::temp_dir(),
        );
        let strip_modules = module_keys.clone();
        let strip_keymap = config.keymap.clone();
        let strip = strip.clone();
        let strip_frame = strip_frame.clone();
        super::prefix::install(
            &window,
            keymap.clone(),
            module_keys,
            move |command, keycode| {
                let refuse = |action: String| refused.say(&action, |text| toast.show(text), |line| println!("{line}"));
                let action = match command {
                    super::prefix::PrefixCommand::Number(digit) => {
                        refuse(format!("number {digit}"));
                        return;
                    }
                    super::prefix::PrefixCommand::Workspace(pick) => {
                        refuse(format!("workspace {pick:?}"));
                        return;
                    }
                    super::prefix::PrefixCommand::Run(action) => action,
                    super::prefix::PrefixCommand::Place { module, axis, cwd: _ } => {
                        if module.kind() == ModuleKind::LuaWebview {
                            eitri_core::keymap::Action::Module(module)
                        } else {
                            refuse(format!("place {module} {axis:?}"));
                            return;
                        }
                    }
                    // A module key after the hide key: there is no layout to hide it from.
                    super::prefix::PrefixCommand::Hide { module } => {
                        refuse(format!("hide {module}"));
                        return;
                    }
                };
                match companion_prefix::classify(&action) {
                    CompanionVerb::Tab(tab) => run_tab(tab, &window_for_tabs, &grid, &agent, &*focus_module, &toast),
                    CompanionVerb::Reload => agent.reload_document_by_hand(ViewId::BASE),
                    CompanionVerb::Keymap => {
                        focus_module(&ModuleId::agent());
                        agent.open_keymap(ViewId::BASE, None, 0);
                    }
                    CompanionVerb::CommandLine => {
                        focus_module(&ModuleId::agent());
                        agent.open_command_line(ViewId::BASE);
                    }
                    CompanionVerb::Text(change) => text_size.all_step(change.into()),
                    CompanionVerb::Hint => hint.toggle_from_prefix(keycode),
                    CompanionVerb::Literal => {
                        if let Some(key) = super::prefix::literal_for(&action, &keymap) {
                            agent.literal_key(ViewId::BASE, &key);
                        }
                    }
                    CompanionVerb::Select(direction) => {
                        move_focus(&ModuleId::agent(), direction);
                    }
                    CompanionVerb::Page(id) => {
                        let link = page_link_for_key.borrow().as_ref().and_then(Weak::upgrade);
                        let (state, peer) = link.as_ref().map_or((LinkState::NoEditor, None), |link| {
                            let (state, _, peer) = link.snapshot();
                            (state, peer)
                        });
                        if let Err(why) = page_context(&state, &page_project, &page_exe, &page_control_dir) {
                            toast.show(&why);
                            return;
                        }
                        let (project, exe, control_dir) =
                            (page_project.clone(), page_exe.clone(), page_control_dir.clone());
                        let carrier = crate::page_window::Carrier {
                            ctx: Rc::new(move || {
                                let mut ctx = page_context(&state, &project, &exe, &control_dir)
                                    .expect("the key captured an attached editor");
                                ctx.nvim_pid = peer.or(ctx.nvim_pid);
                                ctx
                            }),
                            raise_editor: Rc::new(|_| {}),
                            raise_self: Rc::new(|| {}),
                        };
                        let later_toast = toast.clone();
                        if let Err(why) = crate::gtk_host::BesidePort::carry_out(
                            &carrier,
                            &eitri_core::wm::BesideOp::Page(id.as_str().trim_start_matches("lua:").to_owned()),
                            Box::new(move |why| later_toast.show(&why)),
                        ) {
                            toast.show(&why);
                        }
                    }
                    CompanionVerb::Round(step) => {
                        // The keys are on the panel, and on its shown tab: the round's stops include this panel's.
                        let tab = agent.tab_shown_in(ViewId::BASE).map(|tab| tab.0);
                        let (toast, focus_module) = (toast.clone(), focus_module.clone());
                        crate::agents_host::round_key_here(
                            step,
                            ModuleId::agent().as_str().to_owned(),
                            tab,
                            move |outcome| {
                                println!("[agents] the round: {outcome:?}");
                                if let eitri_core::wm::agents_round::RoundOutcome::BackTo(module) = &outcome {
                                    focus_module(module);
                                }
                                if let Some(text) = outcome.text() {
                                    toast.show_for(&text, eitri_core::wm::agents_round::STATUS_FOR);
                                }
                            },
                        );
                    }
                    CompanionVerb::Refuse => refuse(format!("{action:?}")),
                }
            },
            move |waiting| {
                let pieces = companion_prefix::strip_pieces_with_pages(waiting, &strip_keymap, &strip_modules);
                strip.show(&pieces);
                strip_frame.set_visible(!pieces.is_empty());
            },
            // Nothing here selects a container, so the prefix's own `Esc` has nothing to reset.
            || {},
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
    // toward the neighbouring window (`move_focus`, which the panel's fallthrough hook also takes).
    // The panel's host gets what every chat host gets (`wire`): its `Ctrl+h/j/k/l`, no Super chord
    // on its page, and `Ctrl`+wheel zoom. This window has no window controller, so its stage is its
    // own, by the controller's rules: a move with nothing that way goes on to the page.
    crate::wire::wire_host(
        crate::wire::WireKind::Chat,
        &ModuleId::agent(),
        &agent_host,
        &crate::wire::WireDeps {
            module_stage: {
                let agent = agent.clone();
                let move_focus = move_focus.clone();
                Rc::new(
                    move |stage: &eitri_core::wm::KeyStage, press: eitri_core::wm::KeyPress| {
                        use eitri_core::wm::{keys, KeyStage, Verdict};
                        // `wire_host` only ever asks with its host's module stage.
                        let KeyStage::Module(id) = stage else {
                            return Verdict::PassThrough;
                        };
                        if let Some(direction) = keys::nav_direction(&press) {
                            if agent.nav_claims(ViewId::BASE, direction) {
                                agent.nav_key(ViewId::BASE, direction);
                                return Verdict::Consumed;
                            }
                            return match keys::nav_outcome(move_focus(id, direction), false) {
                                keys::NavOutcome::Stop => Verdict::Consumed,
                                keys::NavOutcome::LetThrough => Verdict::LetThrough,
                            };
                        }
                        if press.super_or_hyper {
                            Verdict::Consumed
                        } else {
                            Verdict::PassThrough
                        }
                    },
                )
            },
            text_size: text_size.clone(),
        },
    );

    // Where the panel stands with the user's nvim. The first call of `on_change` is at once, so the
    // band says what is true from the first `ready`.
    let reload = ReloadCalls::new({
        let agent = agent.clone();
        move || agent.busy()
    });
    let link = {
        let attached = attached.clone();
        let reload = reload.clone();
        let agent_for_change = agent.clone();
        let agent_for_cancel = agent.clone();
        let runner_for_change = runner.clone();
        link::start(
            start.nvim.clone(),
            sockets,
            eitri_core::companion::InstallKind::Agent,
            move |state, band, peer_pid, connect_began| {
                let was = attached.get();
                let now = matches!(state, LinkState::Attached { .. });
                attached.set(now);
                // A new editor starts empty: forget the last one's file and selection as a connect
                // begins and as an attached editor goes. Not on arriving at `Attached`: the install
                // makes the editor send its first context, and that line may be read just before. A
                // connect that began and attached within one poll still resets: losing the new editor's
                // first context costs a cursor move, keeping the old editor's would send it with a turn.
                if connect_began || matches!(state, LinkState::Connecting { .. }) || (was && !now) {
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
                    LinkState::NoEditor
                    | LinkState::Detached { .. }
                    | LinkState::Failed { .. }
                    | LinkState::Refused { .. } => runner_for_change.set_partner_from(None),
                    LinkState::Connecting { .. } | LinkState::Attaching { .. } => {}
                }
                println!("[companion] link: {} {}", band.state, band.text);
                agent_for_change.set_editor_link(band);
                reload.link_changed(state, connect_began);
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

    *page_link.borrow_mut() = Some(Rc::downgrade(&link));

    {
        let calls: Rc<dyn SlotCalls> = link.clone();
        reload.bind(Rc::downgrade(&calls));
    }

    // Turn review asks the attached nvim through the same link, and polls its answers.
    let review_owner: Rc<dyn Fn() -> Option<Owner>> = {
        let owner = link.clone();
        Rc::new(move || owner.review_owner())
    };
    // A file the review opened is in the user's own window: bringing it forward is the window
    // manager's job, as for a draft.
    let review_open: Rc<dyn Fn()> = {
        let link = link.clone();
        let runner = runner.clone();
        Rc::new(move || {
            if let Some(pid) = link.nvim_pid() {
                runner.raise_editor(pid);
            }
        })
    };

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
    let editor_request: EditorRequest = {
        let link = link.clone();
        let runner = runner.clone();
        Rc::new(move |request: &ScratchRequest| {
            link.exec_lua_for("scratch", SCRATCH_CALL_LUA, vec![request.hex().into()])?;
            // The editor is the user's own window, so bringing it forward is the window manager's
            // job; nothing here waits for it.
            if let Some(pid) = link.nvim_pid() {
                runner.raise_editor(pid);
            }
            Ok(())
        })
    };
    // An edit that came back: the keys return to the panel, on the oldest card if one waits.
    let editor_done: Rc<dyn Fn()> = {
        let agent_for_done = agent.clone();
        let focus_module = focus_module.clone();
        let runner = runner.clone();
        Rc::new(move || {
            focus_module(&ModuleId::agent());
            runner.raise_pid(std::process::id());
            if !agent_for_done.focus_oldest_card(ViewId::BASE) {
                agent_for_done.arrive(ViewId::BASE);
            }
        })
    };
    // The editor's "edit the selection" key: this window is the chat, so it comes forward (where the compositor
    // lets it, as for an edit that came back), takes the keys and opens its composer.
    let edit_selection: Rc<dyn Fn()> = {
        let agent_for_edit = agent.clone();
        let focus_module = focus_module.clone();
        let runner = runner.clone();
        Rc::new(move || {
            focus_module(&ModuleId::agent());
            runner.raise_pid(std::process::id());
            agent_for_edit.edit_selection(ViewId::BASE);
        })
    };
    // Every hook the panel gets here, registered once its last one is built. Nothing above can make
    // the panel call one: the window is still being built, and no page message is read before the
    // main loop runs again.
    wire_panel(
        &agent,
        companion_hooks(CompanionHookParts {
            toast: toast_hook,
            run_tab: run_tab_hook,
            hint: hint_hook,
            move_focus: move_focus.clone(),
            editor_rpc: link.clone(),
            review_owner,
            review_open,
            editor_request,
            editor_done,
            edit_selection,
            reload,
        }),
    );

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
            if let Some(text) = eitri_core::tabs::window_close_prompt(
                agent.running_count(),
                agent.queued_count(),
                agent.background_count(),
            ) {
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
            if let Some(agents) = &agents {
                agents.stop();
            }
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
            agent_panel::hold_until_done(&app, agent.shutdown());
            if let Some(path) = &scratch_path {
                eitri_core::scratch::cleanup_detached(path.clone());
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn a_stop_of_a_companion_tab_with_nothing_running_is_refused_before_anything_is_shown() {
        use eitri_core::agents::Intent;
        use eitri_core::wm::RevealRefusal;
        let base = || Some(ViewId::BASE);
        assert_eq!(
            refused_before_showing(Intent::Stop, base, || false, || false),
            Some(RevealRefusal::Idle)
        );
        assert_eq!(refused_before_showing(Intent::Stop, base, || false, || true), None);
        assert_eq!(
            refused_before_showing(Intent::Show, base, || false, || panic!("a show does not ask for work")),
            None
        );
        assert_eq!(
            refused_before_showing(Intent::Stop, || None, || false, || true),
            Some(RevealRefusal::Gone)
        );
        assert_eq!(
            refused_before_showing(Intent::Stop, base, || true, || true),
            Some(RevealRefusal::Moving)
        );
    }

    #[test]
    fn refused_keys_toast_once_then_log() {
        let refused = RefusedKeys::default();
        let mut toasts = Vec::new();
        let mut logs = Vec::new();
        for action in ["zoom", "hide", "zoom"] {
            refused.say(
                action,
                |text| toasts.push(text.to_owned()),
                |line| logs.push(line.to_owned()),
            );
        }
        assert_eq!(toasts, [companion_prefix::BESIDE_OFF_TEXT]);
        assert_eq!(
            logs,
            ["[companion] prefix: hide refused", "[companion] prefix: zoom refused"]
        );
        let another = RefusedKeys::default();
        another.say("zoom", |text| toasts.push(text.to_owned()), |_| {});
        assert_eq!(toasts.len(), 2);
    }

    #[test]
    fn a_page_key_with_no_editor_attached_says_so() {
        for state in [
            LinkState::NoEditor,
            LinkState::Connecting {
                addr: PathBuf::from("/run/nvim.sock"),
            },
        ] {
            let result = page_context(
                &state,
                &PathBuf::from("/project"),
                &PathBuf::from("/eitri"),
                &PathBuf::from("/run"),
            );
            assert_eq!(
                result.err().as_deref(),
                Some("no editor attached: run :EitriPanel in nvim")
            );
        }
        let ctx = page_context(
            &attached(3),
            &PathBuf::from("/project"),
            &PathBuf::from("/eitri"),
            &PathBuf::from("/run"),
        )
        .unwrap();
        assert_eq!(ctx.nvim, PathBuf::from("/run/nvim.sock"));
        assert_eq!(ctx.project, PathBuf::from("/project"));
    }

    /// A link that records every call it takes, by the channel it was attached on when it took it.
    #[derive(Default)]
    struct FakeLink {
        channel: Cell<Option<u64>>,
        taken: RefCell<Vec<(u64, String, Vec<rmpv::Value>)>>,
    }

    impl FakeLink {
        fn calls_on(&self, channel: u64) -> Vec<(String, Vec<rmpv::Value>)> {
            self.taken
                .borrow()
                .iter()
                .filter(|(on, _, _)| *on == channel)
                .map(|(_, name, args)| (name.clone(), args.clone()))
                .collect()
        }
    }

    impl SlotCalls for FakeLink {
        fn slot_call(&self, name: &str, args: Vec<rmpv::Value>) -> Result<(), String> {
            let channel = self.channel.get().ok_or("not attached")?;
            self.taken.borrow_mut().push((channel, name.to_owned(), args));
            Ok(())
        }
    }

    fn attached(channel: u64) -> LinkState {
        LinkState::Attached {
            addr: PathBuf::from("/run/nvim.sock"),
            channel,
            nvim_pid: 1,
            in_tmux: false,
        }
    }

    fn busy_arg(call: &(String, Vec<rmpv::Value>)) -> Option<bool> {
        assert_eq!(call.0, "busy");
        call.1.first().and_then(rmpv::Value::as_bool)
    }

    /// The link and the calls over it, with a busy state the test sets.
    fn calls_over_fake() -> (Rc<FakeLink>, Rc<ReloadCalls>, Rc<Cell<bool>>) {
        let busy = Rc::new(Cell::new(false));
        let calls = ReloadCalls::new({
            let busy = busy.clone();
            move || busy.get()
        });
        let link = Rc::new(FakeLink::default());
        let dynamic: Rc<dyn SlotCalls> = link.clone();
        calls.bind(Rc::downgrade(&dynamic));
        (link, calls, busy)
    }

    #[test]
    fn busy_is_sent_again_after_a_reattach_and_never_to_the_old_channel() {
        let (link, calls, busy) = calls_over_fake();

        // The first editor attaches while the panel is idle, then a turn starts.
        link.channel.set(Some(3));
        calls.link_changed(&attached(3), false);
        assert_eq!(link.calls_on(3).iter().map(busy_arg).collect::<Vec<_>>(), [Some(false)]);
        busy.set(true);
        calls.busy_changed(true);
        assert_eq!(
            link.calls_on(3).iter().map(busy_arg).collect::<Vec<_>>(),
            [Some(false), Some(true)]
        );

        // The editor goes away mid-turn: nothing is sent while there is no channel, so nothing reaches channel 3.
        link.channel.set(None);
        calls.link_changed(&LinkState::NoEditor, false);
        calls.turn_ended();
        calls.busy_changed(true);
        assert_eq!(link.calls_on(3).len(), 2, "the old channel heard nothing after it went");

        // A second editor attaches mid-turn and is told the busy state the panel has now.
        link.channel.set(Some(4));
        calls.link_changed(&attached(4), false);
        assert_eq!(link.calls_on(4).iter().map(busy_arg).collect::<Vec<_>>(), [Some(true)]);
        assert_eq!(link.calls_on(3).len(), 2, "and the old channel still heard nothing");

        // A repeated report of the same attachment sends nothing more.
        calls.link_changed(&attached(4), false);
        assert_eq!(link.calls_on(4).len(), 1);
    }

    #[test]
    fn a_connect_that_attached_another_editor_on_the_same_channel_number_is_told_the_busy_state() {
        let (link, calls, busy) = calls_over_fake();
        link.channel.set(Some(3));
        calls.link_changed(&attached(3), false);
        busy.set(true);
        calls.busy_changed(true);
        assert_eq!(link.calls_on(3).len(), 2);

        // A second nvim answers its install within the poll that began its connect: the link reports
        // `Attached` on the same channel number with nothing reported between, and its part starts idle.
        calls.link_changed(&attached(3), true);
        let heard = link.calls_on(3).iter().map(busy_arg).collect::<Vec<_>>();
        assert_eq!(heard, [Some(false), Some(true), Some(true)]);

        // The same editor reported again, with no connect, is not told twice.
        calls.link_changed(&attached(3), false);
        assert_eq!(link.calls_on(3).len(), 3);
    }

    #[test]
    fn the_busy_edges_are_said_in_the_lines_the_window_pass_reads() {
        let said = Rc::new(RefCell::new(Vec::<String>::new()));
        let calls = ReloadCalls::with_log(|| false, {
            let said = said.clone();
            move |line| said.borrow_mut().push(line.to_owned())
        });
        calls.busy_changed(true);
        calls.busy_changed(false);
        assert_eq!(*said.borrow(), ["[reload] busy on", "[reload] busy off"]);
    }

    #[test]
    fn a_turn_end_reloads_through_the_attached_channel_only() {
        let (link, calls, _busy) = calls_over_fake();
        calls.turn_ended();
        assert!(link.taken.borrow().is_empty(), "no editor, no call");
        link.channel.set(Some(7));
        calls.link_changed(&attached(7), false);
        calls.turn_ended();
        let on_seven = link.calls_on(7);
        assert_eq!(on_seven.last(), Some(&("now".to_owned(), Vec::new())));
    }

    #[test]
    fn a_dropped_link_takes_no_call() {
        let calls = ReloadCalls::new(|| true);
        {
            let link = Rc::new(FakeLink::default());
            link.channel.set(Some(1));
            let dynamic: Rc<dyn SlotCalls> = link;
            calls.bind(Rc::downgrade(&dynamic));
        }
        // Nothing to call, and nothing panics.
        calls.busy_changed(true);
        calls.link_changed(&attached(1), false);
        calls.turn_ended();
    }
}
