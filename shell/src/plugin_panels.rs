//! The window's plugin pages. Each plugin `init.lua` loaded becomes a Lua panel module `lua:<id>` whose
//! content is the plugin's page, with its host pumped from a GLib watch on a wake its threads raise, plus a
//! one-shot timer only when the host asks for one: a plugin with nothing running costs no wake-up. Kill,
//! window close, the layout and the window's editor are connected here.

use std::cell::{Cell, OnceCell, RefCell};
use std::path::Path;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use eitri_core::editor_rpc::EditorRpc;
use eitri_core::keymap::Keymap;
use eitri_core::layout::{ModuleId, OpenRule};
use eitri_core::lua::panel::PanelSlot;
use eitri_core::lua::plugin::PluginRegistration;
use eitri_core::plugin::doc_route::DocSlot;
use eitri_core::plugin::follow_feed::{FollowBatch, FollowCmd, Ordered};
use eitri_core::plugin::host::{LayoutPort, PagePort, PluginHost};
use eitri_core::plugin::keys::EffectiveRow;
use eitri_core::plugin::load::LoadedPlugin;
use eitri_core::plugin::manifest::Capability;
use eitri_core::plugin::page_policy;
use eitri_core::plugin::protocol::FocusTarget;
use eitri_core::plugin::state::PluginStore;
use eitri_core::plugin::tasks::{RunsWaiter, GRACE};
use eitri_core::plugin::theme::PageTheme;
use eitri_core::theme::ThemeTokens;
use eitri_panel::panel_wake::PanelWake;
use gtk4::glib;
use gtk4::prelude::*;

use crate::lua::{install_crash_recovery_into, LuaEngine, PanelEntry};
use crate::plugin_page::{self, PageHooks, PageSpec, WebKitPluginPage};
use crate::webview_crash_guard::WebViewCrashGuard;

/// Runs one host's `pump` when its wake is raised and when the host asked for a time.
struct Pump {
    host: Rc<PluginHost>,
    wake: PanelWake,
    timer: RefCell<Option<glib::SourceId>>,
    me: Weak<Pump>,
}

impl Pump {
    fn start(host: Rc<PluginHost>, wake: PanelWake) -> Rc<Pump> {
        let pump = Rc::new_cyclic(|me| Pump {
            host,
            wake,
            timer: RefCell::new(None),
            me: me.clone(),
        });
        let watched = {
            let pump = pump.clone();
            neovide_editor::fd_watch::add_local(
                pump.wake.fd(),
                glib::IOCondition::IN | glib::IOCondition::ERR | glib::IOCondition::HUP,
                glib::ffi::G_PRIORITY_DEFAULT,
                move |_| {
                    pump.wake.take();
                    pump.kick();
                    glib::ControlFlow::Continue
                },
            )
        };
        if watched.is_err() {
            eprintln!("[plugin] its wake cannot be watched on this thread; output arrives with the page's next call");
        }
        pump
    }

    fn kick(&self) {
        let next = self.host.pump(Instant::now());
        self.arm(next);
    }

    /// Replaces the pending timer with one for `next`, rounded up to a whole millisecond (GLib counts in them).
    fn arm(&self, next: Option<Instant>) {
        let old = self.timer.borrow_mut().take();
        if let Some(old) = old {
            old.remove();
        }
        let (Some(at), Some(me)) = (next, self.me.upgrade()) else {
            return;
        };
        let nanos = at.saturating_duration_since(Instant::now()).as_nanos();
        let wait = Duration::from_millis(u64::try_from(nanos.div_ceil(1_000_000)).unwrap_or(u64::MAX));
        let id = glib::timeout_add_local_once(wait, move || {
            // Fired: its id is spent and must not be removed again.
            me.timer.borrow_mut().take();
            me.kick();
        });
        *self.timer.borrow_mut() = Some(id);
    }
}

/// The user's download directory (XDG's), else `~/Downloads`, else the current directory.
fn downloads_dir() -> std::path::PathBuf {
    glib::user_special_dir(glib::UserDirectory::Downloads)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Downloads")))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// Shows a module if it is hidden, then gives it the keys.
type ShowAndFocus = Rc<dyn Fn(&ModuleId) -> Result<(), String>>;

/// The window's layout as a plugin sees it: the editor, or its own module, each shown first if hidden.
struct WindowLayout {
    own: ModuleId,
    show_and_focus: ShowAndFocus,
}

impl LayoutPort for WindowLayout {
    fn focus(&self, target: FocusTarget) -> Result<(), String> {
        match target {
            FocusTarget::Editor => (self.show_and_focus)(&ModuleId::editor()),
            FocusTarget::Page => (self.show_and_focus)(&self.own),
        }
    }
}

/// The page port of a plugin whose page is not built yet: what the host sends before then is dropped (the page
/// that is built later says hello and gets the current state), and from the build on it is the page's own port.
#[derive(Default)]
struct LazyPort(RefCell<Option<Rc<dyn PagePort>>>);

impl LazyPort {
    /// Cloned out, so a page that answers inside a send finds the port free.
    fn page(&self) -> Option<Rc<dyn PagePort>> {
        self.0.borrow().clone()
    }
}

impl PagePort for LazyPort {
    fn send(&self, message: &str) {
        if let Some(page) = self.page() {
            page.send(message);
        }
    }

    fn set_theme(&self, theme: &PageTheme) {
        if let Some(page) = self.page() {
            page.set_theme(theme);
        }
    }
}

/// Builds the plugin's page into its module's container and returns the page's port; `None` when WebKit cannot
/// start here. Called at most once.
type BuildPage = Box<dyn FnOnce(&PageTheme) -> Option<Rc<dyn PagePort>>>;

/// One plugin's module: its host (none where WebKit cannot start), the port its host sends through, and the page
/// build still to run for a plugin whose module has not been shown yet.
struct Panel {
    module: ModuleId,
    pump: Option<Rc<Pump>>,
    port: Rc<LazyPort>,
    build: RefCell<Option<BuildPage>>,
    hidden_start: bool,
    own_rule: Option<OpenRule>,
    /// Filled by the build.
    webview: Rc<RefCell<Option<webkit6::WebView>>>,
    /// Whether the host was last told its module is drawn; `None` until it is first told. A layout change that
    /// leaves it where it was tells the host nothing, so a divider drag wakes no plugin.
    on_screen: Cell<Option<bool>>,
}

/// What `init.lua` registered for one plugin, read once at startup.
struct Registered {
    plugin: Rc<LoadedPlugin>,
    key: Option<String>,
    slot: PanelSlot,
    hidden_start: bool,
    own_rule: Option<OpenRule>,
}

impl Registered {
    fn of(registration: &PluginRegistration) -> Registered {
        Registered {
            plugin: registration.plugin.clone(),
            key: registration.key.clone(),
            slot: registration.slot,
            hidden_start: registration.starts_hidden(),
            own_rule: registration.own_rule(),
        }
    }
}

impl Panel {
    /// The panel of `registered`, whose start and open rule it answers with.
    fn new(
        registered: &Registered,
        pump: Option<Rc<Pump>>,
        port: Rc<LazyPort>,
        build: Option<BuildPage>,
        webview: Rc<RefCell<Option<webkit6::WebView>>>,
    ) -> Panel {
        Panel {
            module: ModuleId::lua(&registered.plugin.manifest.id),
            pump,
            port,
            build: RefCell::new(build),
            hidden_start: registered.hidden_start,
            own_rule: registered.own_rule.clone(),
            webview,
            on_screen: Cell::new(None),
        }
    }
}

/// The empty place a plugin that starts hidden holds in the grid until its page is built into it: one child,
/// laid out at its own origin as a web module's host lays out its view, and a grab of the keys passed to that
/// child, since the module's focus target is this widget and GTK refuses a grab on a widget that is not
/// focusable itself.
mod page_slot {
    use gtk4::glib;
    use gtk4::prelude::*;
    use gtk4::subclass::prelude::*;

    mod imp {
        use super::*;

        #[derive(Default)]
        pub struct PageSlot;

        #[glib::object_subclass]
        impl ObjectSubclass for PageSlot {
            const NAME: &'static str = "EitriPluginPageSlot";
            type Type = super::PageSlot;
            type ParentType = gtk4::Widget;

            fn class_init(klass: &mut Self::Class) {
                klass.set_layout_manager_type::<gtk4::BinLayout>();
            }
        }

        impl ObjectImpl for PageSlot {
            fn dispose(&self) {
                while let Some(child) = self.obj().first_child() {
                    child.unparent();
                }
            }
        }

        impl WidgetImpl for PageSlot {
            fn grab_focus(&self) -> bool {
                self.obj().first_child().is_some_and(|child| child.grab_focus())
            }
        }
    }

    glib::wrapper! {
        pub struct PageSlot(ObjectSubclass<imp::PageSlot>)
            @extends gtk4::Widget,
            @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
    }

    impl PageSlot {
        pub(crate) fn new() -> PageSlot {
            let slot: PageSlot = glib::Object::new();
            slot.set_hexpand(true);
            slot.set_vexpand(true);
            slot
        }

        /// The page, filling the slot from its origin: a margin or an alignment would move the view inside its
        /// parent, which WebKit adds to the caret rectangle it gives the input method.
        pub(crate) fn set_page(&self, page: &impl IsA<gtk4::Widget>) {
            let page = page.as_ref();
            page.set_margin_start(0);
            page.set_margin_end(0);
            page.set_margin_top(0);
            page.set_margin_bottom(0);
            page.set_halign(gtk4::Align::Fill);
            page.set_valign(gtk4::Align::Fill);
            page.set_hexpand(true);
            page.set_vexpand(true);
            page.set_parent(self);
        }
    }
}

/// What the window does with a plugin's nvim commands; `main` implements it over the grid, a test records.
pub(crate) trait FollowWindow {
    fn is_shown(&self, module: &ModuleId) -> bool;
    /// Shown and drawn: not behind a zoom, not on a tab its container does not draw.
    fn is_on_screen(&self, module: &ModuleId) -> bool;
    /// Opens or shows the module with the keys where they are (its page built the first time), and brings it on
    /// screen as far as that leaves the module with the keys drawn.
    fn show(&self, module: &ModuleId);
    /// Hides it; if it held the keys they go where a hide sends them.
    fn hide(&self, module: &ModuleId);
}

/// Applies the commands in order, each reading the window at its own turn, so several in one batch end where as
/// many key presses would. `Show` brings a module that is not on screen there (a hidden one, or one behind a zoom
/// or another tab) and does nothing to one on screen; `Toggle` hides one on screen and otherwise shows it, so the
/// command never hides a preview the reader cannot see; `Hide` hides a shown one wherever it is. An `Auto` that
/// reached here shows only a hidden module: it fires on every new buffer, and must not end a zoom or switch a tab
/// the user chose.
pub(crate) fn apply_follow_commands(window: &dyn FollowWindow, commands: Vec<(ModuleId, FollowCmd)>) {
    for (module, cmd) in commands {
        match cmd {
            FollowCmd::Show | FollowCmd::Toggle if !window.is_on_screen(&module) => window.show(&module),
            FollowCmd::Toggle | FollowCmd::Hide if window.is_shown(&module) => window.hide(&module),
            FollowCmd::Auto if !window.is_shown(&module) => window.show(&module),
            FollowCmd::Show | FollowCmd::Toggle | FollowCmd::Hide | FollowCmd::Auto => {}
        }
    }
}

/// How many follow lines the window has read, for the probe build's trace.
#[derive(Default)]
struct FollowTotals {
    views: u64,
    cmds: u64,
    /// The totals the last trace line printed, and when.
    printed: Option<(u64, u64, Instant)>,
}

impl FollowTotals {
    /// Adds a batch; the line to print when the totals moved since the last one and that was a second ago or more.
    fn add(&mut self, views: u64, cmds: u64, now: Instant) -> Option<String> {
        self.views += views;
        self.cmds += cmds;
        if let Some((views, cmds, at)) = self.printed {
            let moved = (views, cmds) != (self.views, self.cmds);
            if !moved || now.saturating_duration_since(at) < Duration::from_secs(1) {
                return None;
            }
        } else if self.views == 0 && self.cmds == 0 {
            return None;
        }
        self.printed = Some((self.views, self.cmds, now));
        Some(format!("[follow] trace: views={} cmds={}", self.views, self.cmds))
    }
}

pub(crate) struct PluginPanels {
    panels: Vec<Panel>,
    /// The window's theme as the pages last got it: what a page built later starts with.
    theme: RefCell<PageTheme>,
    follow_totals: RefCell<FollowTotals>,
    /// The editor's end was told to the hosts and no editor session has been seen since: an exit that is reported
    /// twice (nvim's own exit, then the editor's retirement) tells the pages once.
    editor_gone: Cell<bool>,
    /// Told of each page's view once it exists, so the window's text size reaches it.
    page_built: RefCell<Option<PageBuilt>>,
}

/// What [`PluginPanels::set_page_built`] calls with a module and its new view.
pub(crate) type PageBuilt = Rc<dyn Fn(&ModuleId, &webkit6::WebView)>;

impl PluginPanels {
    /// Registers every loaded plugin as a Lua panel entry, each host checking its page's keys against the
    /// window's prefix and applying the user's `plugin:<id>` overrides, and each page starting in `theme`. A
    /// plugin whose module starts hidden gets its host now and its page the first time the module is shown
    /// ([`PluginPanels::build_page`]); its entry's widget is the empty slot the page is built into.
    /// `Err` is a startup failure's text.
    pub(crate) fn install(engine: &LuaEngine, keymap: &Keymap, theme: &PageTheme) -> Result<PluginPanels, String> {
        let loaded: Vec<Registered> = engine.plugins.borrow().plugins().iter().map(Registered::of).collect();
        let store_root = PluginStore::eitri_root_from_env();
        let mut panels = Vec::new();
        let keys_settings =
            eitri_core::plugin::kit_settings::from_config(|key| engine.config.borrow().get(key).map(str::to_owned))?;
        for registration in loaded {
            let plugin = registration.plugin.clone();
            let key = registration.key.clone();
            let slot = registration.slot;
            let id = plugin.manifest.id.clone();
            let clash = engine.panels.borrow().entries().iter().any(|e| e.id == id);
            if clash {
                return Err(format!("eitri.plugin.load: plugin {id} has the id of a Lua panel"));
            }
            let url = page_policy::entry_uri(&id, &plugin.manifest.entry);
            let title = plugin.manifest.title.clone();
            let wake = if crate::webkit_sandbox::decision().allows_webviews() {
                PanelWake::new()
                    .map_err(|e| eprintln!("[plugin {id}] cannot start: {e}"))
                    .ok()
            } else {
                eprintln!("[plugin {id}] not loading its page: WebKit's sandbox cannot start here");
                None
            };
            let port = Rc::new(LazyPort::default());
            let webview: Rc<RefCell<Option<webkit6::WebView>>> = Rc::default();
            let crash_guard = Rc::new(RefCell::new(WebViewCrashGuard::with_defaults()));
            let (widget, pump, build) = match wake {
                Some(wake) => {
                    let cell: Rc<OnceCell<Rc<Pump>>> = Rc::new(OnceCell::new());
                    let hooks = PageHooks {
                        on_message: {
                            let cell = cell.clone();
                            Rc::new(move |raw: &str| {
                                if let Some(pump) = cell.get() {
                                    pump.host.page_message(raw, Instant::now());
                                    pump.kick();
                                }
                            })
                        },
                        on_committed: {
                            let cell = cell.clone();
                            Rc::new(move || {
                                if let Some(pump) = cell.get() {
                                    pump.host.page_committed();
                                }
                            })
                        },
                        open_outside: Rc::new(plugin_page::open_outside),
                    };
                    let doc_slot = plugin
                        .manifest
                        .has(Capability::EditorFollow)
                        .then(|| Rc::new(RefCell::new(DocSlot::default())));
                    let spec = PageSpec {
                        id: id.clone(),
                        site_root: plugin.site_root.clone(),
                        entry: plugin.manifest.entry.clone(),
                        origins: plugin.manifest.origins.clone(),
                        downloads: downloads_dir(),
                        doc_slot: doc_slot.clone(),
                        remote_images: plugin.remote_images,
                        console_to_stdout: eitri_core::plugin::trace::enabled(),
                    };
                    let store = store_root.clone().map(|root| PluginStore::new(root, &id));
                    let overrides = keymap.plugin_user(&id).to_vec();
                    let host = Rc::new(PluginHost::new(
                        plugin.clone(),
                        port.clone(),
                        wake.waker(),
                        store,
                        overrides,
                    ));
                    host.set_prefix_key(keymap.prefix());
                    host.set_keys_settings(keys_settings.clone());
                    // A view built now or later wears `theme`; the host records it so it sends nothing for the
                    // same set.
                    host.set_theme(theme);
                    host.set_module_key(key.as_deref());
                    // The host fills the slot with the followed document's project; the page's scheme handler
                    // reads the same slot.
                    if let Some(slot) = &doc_slot {
                        host.set_doc_slot(slot.clone());
                    }
                    let pump = Pump::start(host, wake);
                    let _ = cell.set(pump.clone());
                    // A module that starts hidden holds an empty slot until its first show; one that starts
                    // shown is its view.
                    let slot_widget = registration.hidden_start.then(page_slot::PageSlot::new);
                    let build: BuildPage = {
                        let id = id.clone();
                        let url = url.clone();
                        let crash_guard = crash_guard.clone();
                        let webview = webview.clone();
                        let slot_widget = slot_widget.clone();
                        Box::new(move |theme: &PageTheme| {
                            let view = plugin_page::build(&spec, hooks, theme);
                            if let Some(slot_widget) = &slot_widget {
                                slot_widget.set_page(&view);
                            }
                            install_crash_recovery_into(
                                &view,
                                id.clone(),
                                url,
                                Some(page_policy::host_page_base(&id)),
                                crash_guard,
                            );
                            let page: Rc<dyn PagePort> = Rc::new(WebKitPluginPage(view.downgrade()));
                            *webview.borrow_mut() = Some(view);
                            eitri_core::plugin::trace::line(&id, "page built", "");
                            Some(page)
                        })
                    };
                    match slot_widget {
                        Some(slot_widget) => (slot_widget.upcast::<gtk4::Widget>(), Some(pump), Some(build)),
                        None => {
                            *port.0.borrow_mut() = build(theme);
                            let view = webview.borrow().clone().expect("the page was just built");
                            (view.upcast::<gtk4::Widget>(), Some(pump), None)
                        }
                    }
                }
                None => (crate::webkit_sandbox::lua_panel_placeholder(&title), None, None),
            };
            engine.panels.borrow_mut().register(PanelEntry {
                id,
                title,
                slot,
                key,
                url,
                widget,
                crash_guard,
            });
            panels.push(Panel::new(&registration, pump, port, build, webview));
        }
        Ok(PluginPanels {
            panels,
            theme: RefCell::new(theme.clone()),
            follow_totals: RefCell::default(),
            editor_gone: Cell::new(false),
            page_built: RefCell::new(None),
        })
    }

    fn panel(&self, module: &ModuleId) -> Option<&Panel> {
        self.panels.iter().find(|p| p.module == *module)
    }

    /// Whether `module` is a plugin's that starts hidden: the window never places it by itself.
    pub(crate) fn starts_hidden(&self, module: &ModuleId) -> bool {
        self.panel(module).is_some_and(|p| p.hidden_start)
    }

    /// The plugin's own open rule (its `position`, else its `[module]` table), counted where a Lua panel's
    /// `position` counts.
    pub(crate) fn own_rule(&self, module: &ModuleId) -> Option<OpenRule> {
        self.panel(module).and_then(|p| p.own_rule.clone())
    }

    /// Whether the plugin's page exists. A plugin with no page here (WebKit cannot start) never has one.
    pub(crate) fn is_built(&self, module: &ModuleId) -> bool {
        self.panel(module).is_some_and(|p| p.port.page().is_some())
    }

    /// Builds the page the first time; afterwards nothing. The host's port sends to the page from then on, and the
    /// page says hello and gets the host's current state.
    pub(crate) fn build_page(&self, module: &ModuleId, theme: &PageTheme) {
        let Some(panel) = self.panel(module) else {
            return;
        };
        // Taken before it runs, so a build that comes back here finds nothing left to run.
        let build = panel.build.borrow_mut().take();
        if let Some(build) = build {
            let page = build(theme);
            *panel.port.0.borrow_mut() = page;
            let (hook, view) = (self.page_built.borrow().clone(), panel.webview.borrow().clone());
            if let (Some(hook), Some(view)) = (hook, view) {
                hook(&panel.module, &view);
            }
        }
    }

    /// `hook` hears of every page's view: now, of each built already (a page that starts shown is built at
    /// startup, before the window's text size exists), and later of each built at its first show.
    pub(crate) fn set_page_built(&self, hook: PageBuilt) {
        for panel in &self.panels {
            let view = panel.webview.borrow().clone();
            if let Some(view) = view {
                hook(&panel.module, &view);
            }
        }
        *self.page_built.borrow_mut() = Some(hook);
    }

    /// The window's theme as the pages last got it (what a page built later starts with).
    pub(crate) fn theme(&self) -> PageTheme {
        self.theme.borrow().clone()
    }

    /// The module's built view, for a kill (its web process ended) and a revive (loaded again).
    pub(crate) fn webview(&self, module: &ModuleId) -> Option<webkit6::WebView> {
        self.panel(module).and_then(|p| p.webview.borrow().clone())
    }

    fn hosts(&self) -> impl Iterator<Item = (&ModuleId, &Rc<Pump>)> {
        self.panels
            .iter()
            .filter_map(|panel| panel.pump.as_ref().map(|p| (&panel.module, p)))
    }

    /// Every plugin's module, in the order `init.lua` loaded them.
    pub(crate) fn modules(&self) -> Vec<ModuleId> {
        self.panels.iter().map(|p| p.module.clone()).collect()
    }

    /// The host of the plugin `owner` names, when it is loaded here and wants the follow socket's lines.
    fn follower(&self, owner: &str) -> Option<&Rc<Pump>> {
        self.hosts()
            .map(|(_, pump)| pump)
            .find(|pump| pump.host.plugin().manifest.id == owner && pump.host.wants_follow_feed())
    }

    /// Whether any loaded plugin follows the editor or defines nvim commands: only then is the follow socket bound.
    pub(crate) fn wants_follow_feed(&self) -> bool {
        self.hosts().any(|(_, pump)| pump.host.wants_follow_feed())
    }

    /// The window's follow socket and Eitri's scratch directory, to every host that wants them.
    pub(crate) fn set_follow_feed(&self, socket: &Path, scratch_dir: Option<&Path>) {
        for (_, pump) in self.hosts() {
            if pump.host.wants_follow_feed() {
                pump.host
                    .set_follow_feed(socket.to_path_buf(), scratch_dir.map(Path::to_path_buf));
            }
        }
    }

    /// Views and gone lines to their hosts (each host pumped once after); commands back in order for the window:
    /// `Show`, `Hide` and `Toggle` as sent, `Auto` as sent only when that host's `auto_show` is on (still `Auto`, so
    /// the window shows only a hidden module and leaves a zoom or a tab the user chose alone). A line for an
    /// owner that is not a loaded plugin wanting the feed is dropped. Views go first: the reader lets a gone line
    /// out only when it is newer than every view of its plugin let out so far, so this is the order nvim sent
    /// them in. After the editor's end and before a new session, views and gone lines are dropped: they describe
    /// buffers of an editor that is no more, and a view would bring its page back to one.
    pub(crate) fn route_follow(&self, batch: FollowBatch, now: Instant) -> Vec<(ModuleId, FollowCmd)> {
        let cmd_lines = batch
            .ordered
            .iter()
            .filter(|line| matches!(line, Ordered::Cmd { .. }))
            .count() as u64;
        let trace = self
            .follow_totals
            .borrow_mut()
            .add(batch.views_received, cmd_lines, now);
        if let (Some(line), true) = (trace, eitri_core::plugin::trace::enabled()) {
            println!("{line}");
        }
        let mut touched: Vec<Rc<Pump>> = Vec::new();
        let mut touch = |pump: &Rc<Pump>| {
            if !touched.iter().any(|p| Rc::ptr_eq(p, pump)) {
                touched.push(pump.clone());
            }
        };
        let editor_gone = self.editor_gone.get();
        for view in batch.views {
            if editor_gone {
                break;
            }
            if let Some(pump) = self.follower(&view.owner) {
                pump.host.follow_view(view, now);
                touch(pump);
            }
        }
        let mut commands = Vec::new();
        for line in batch.ordered {
            match line {
                Ordered::Gone { owner, buf } => {
                    if editor_gone {
                        continue;
                    }
                    if let Some(pump) = self.follower(&owner) {
                        pump.host.follow_gone(buf, now);
                        touch(pump);
                    }
                }
                Ordered::Cmd { owner, cmd, .. } => {
                    let Some(pump) = self.follower(&owner) else { continue };
                    let module = ModuleId::lua(&owner);
                    match cmd {
                        FollowCmd::Auto if pump.host.auto_show() => commands.push((module, FollowCmd::Auto)),
                        FollowCmd::Auto => {}
                        _ => commands.push((module, cmd)),
                    }
                }
            }
        }
        for pump in touched {
            pump.kick();
        }
        commands
    }

    /// Whether the plugin's module is drawn now, from the window's layout. Told to its host only when it changed.
    pub(crate) fn set_on_screen(&self, module: &ModuleId, on: bool) {
        let Some(panel) = self.panel(module) else { return };
        let Some(pump) = &panel.pump else { return };
        if panel.on_screen.replace(Some(on)) == Some(on) {
            return;
        }
        pump.host.set_on_screen(on, Instant::now());
    }

    /// The window's project root: a followed document inside it has it as its project.
    pub(crate) fn set_window_root(&self, root: &Path) {
        for (_, pump) in self.hosts() {
            pump.host.set_window_root(root.to_path_buf());
        }
    }

    /// The window's editor ended for good: every following page is told, once.
    pub(crate) fn editor_closed(&self) {
        if self.editor_gone.replace(true) {
            return;
        }
        let now = Instant::now();
        for (_, pump) in self.hosts() {
            pump.host.editor_closed(now);
            pump.kick();
        }
    }

    /// Each host looks at the editor's session now (nvim may have just started).
    pub(crate) fn editor_session(&self) {
        self.editor_gone.set(false);
        let now = Instant::now();
        for (_, pump) in self.hosts() {
            pump.host.editor_session(now);
        }
    }

    /// The window's one editor handle, shared with the chat.
    pub(crate) fn set_editor(&self, editor: Rc<dyn EditorRpc>) {
        for (_, pump) in self.hosts() {
            pump.host.set_editor(editor.clone());
        }
    }

    pub(crate) fn set_layout(&self, show_and_focus: ShowAndFocus) {
        for (module, pump) in self.hosts() {
            pump.host.set_layout(Rc::new(WindowLayout {
                own: module.clone(),
                show_and_focus: show_and_focus.clone(),
            }));
        }
    }

    /// Whether a plugin's module holds the keys, from the window's focus tracking. Any other module: nothing.
    pub(crate) fn set_has_keys(&self, module: &ModuleId, has_keys: bool) {
        if let Some((_, pump)) = self.hosts().find(|(m, _)| *m == module) {
            pump.host.set_has_keys(has_keys);
        }
    }

    /// Every page follows the window's colorscheme. A change that leaves the page's set as it was (a size
    /// alone, say) sends nothing.
    pub(crate) fn set_theme(&self, tokens: &ThemeTokens) {
        let theme = PageTheme::from_tokens(tokens);
        *self.theme.borrow_mut() = theme.clone();
        for (_, pump) in self.hosts() {
            pump.host.set_theme(&theme);
        }
    }

    /// The editor's `timeoutlen`, which every page's multi-key sequences wait for.
    pub(crate) fn set_timeoutlen(&self, ms: u32) {
        for (_, pump) in self.hosts() {
            pump.host.set_timeoutlen(ms);
        }
    }

    /// The plugin's title and the key table its current page declared (`None` inside: not declared yet), or
    /// `None` when `module` is not a plugin page with a running host.
    pub(crate) fn declared_rows(&self, module: &ModuleId) -> Option<(String, Option<Vec<EffectiveRow>>)> {
        let (_, pump) = self.hosts().find(|(m, _)| *m == module)?;
        Some((pump.host.plugin().manifest.title.clone(), pump.host.declared_rows()))
    }

    /// `prefix x` on a plugin's module: its runs stop and its assist is put back.
    pub(crate) fn stop(&self, module: &ModuleId) {
        if let Some((_, pump)) = self.hosts().find(|(m, _)| *m == module) {
            pump.host.stop(Instant::now());
            pump.kick();
        }
    }

    /// The window is closing: every run is stopped, and the application is held until they have ended (at most
    /// the grace and a second), so no task outlives the window.
    pub(crate) fn shutdown(&self, app: &gtk4::Application) {
        let waiters: Vec<RunsWaiter> = self
            .hosts()
            .map(|(_, pump)| pump.host.shutdown(Instant::now()))
            .collect();
        if waiters.iter().all(RunsWaiter::finished) {
            return;
        }
        let mut hold = Some(app.hold());
        let deadline = Instant::now() + GRACE + Duration::from_secs(1);
        glib::timeout_add_local(Duration::from_millis(50), move || {
            if waiters.iter().all(RunsWaiter::finished) || Instant::now() >= deadline {
                hold.take();
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::plugin::load::load_plugin;
    use eitri_core::plugin::test_support::{demo_plugin, Scratch};

    #[test]
    fn a_plugin_moves_the_keys_to_the_editor_or_to_its_own_module() {
        let asked: Rc<RefCell<Vec<ModuleId>>> = Rc::default();
        let layout = WindowLayout {
            own: ModuleId::lua("demo"),
            show_and_focus: {
                let asked = asked.clone();
                Rc::new(move |id: &ModuleId| {
                    asked.borrow_mut().push(id.clone());
                    Ok(())
                })
            },
        };
        layout.focus(FocusTarget::Editor).unwrap();
        layout.focus(FocusTarget::Page).unwrap();
        assert_eq!(*asked.borrow(), [ModuleId::editor(), ModuleId::lua("demo")]);
    }

    /// What a host did to its page: `set_theme` for the port, or the name of an event it sent.
    #[derive(Default)]
    struct Recorded(RefCell<Vec<String>>);

    impl eitri_core::plugin::host::PagePort for Recorded {
        fn send(&self, message: &str) {
            let value: serde_json::Value = serde_json::from_str(message).expect("the host sends JSON");
            let event = value["event"].as_str().unwrap_or("(not an event)").to_owned();
            self.0.borrow_mut().push(event);
        }

        fn set_theme(&self, _theme: &PageTheme) {
            self.0.borrow_mut().push("set_theme".to_owned());
        }
    }

    /// A demo plugin as `init.lua` registers it, with `[module]` cut from its manifest when `module` is false.
    fn registered(scratch: &Scratch, name: &str, module: bool, position: Option<PanelSlot>) -> Registered {
        registered_with(scratch, name, module, position, |text| text)
    }

    /// [`registered`], its manifest's text passed through `edit` first.
    fn registered_with(
        scratch: &Scratch,
        name: &str,
        module: bool,
        position: Option<PanelSlot>,
        edit: impl Fn(String) -> String,
    ) -> Registered {
        let (dir, settings) = demo_plugin(&scratch.0.join(name));
        let manifest = dir.join("eitri-plugin.toml");
        // Each plugin its own id, so each has its own module.
        let text = edit(std::fs::read_to_string(&manifest).unwrap());
        let mut text = text.replacen("id = \"demo\"", &format!("id = \"{name}\""), 1);
        if !module {
            // The follow capabilities need the `[follow]` table, which follows `[module]` and goes with it.
            text = text[..text.find("\n[module]\n").unwrap() + 1].replacen(
                ", \"editor.follow\", \"editor.commands\"]",
                "]",
                1,
            );
        }
        std::fs::write(&manifest, text).unwrap();
        let plugin = Rc::new(load_plugin(dir.to_str().unwrap(), &settings, None).unwrap());
        assert_eq!(plugin.manifest.id, name);
        Registered::of(&PluginRegistration {
            plugin,
            key: None,
            slot: position.unwrap_or(PanelSlot::Side),
            position,
        })
    }

    /// A panel over `registered` whose host sends through `port`, its pump made directly rather than through
    /// `Pump::start`: no wake is watched, so nothing runs on a main loop.
    fn panel(registered: &Registered, port: Rc<LazyPort>, build: Option<BuildPage>) -> Panel {
        panel_waking(registered, port, build, std::sync::Arc::new(|| {}))
    }

    /// [`panel`], its host raising `wake` where the window's watch would run its pump.
    fn panel_waking(
        registered: &Registered,
        port: Rc<LazyPort>,
        build: Option<BuildPage>,
        wake: agent::UiWake,
    ) -> Panel {
        let host = Rc::new(PluginHost::new(
            registered.plugin.clone(),
            port.clone(),
            wake,
            None,
            Vec::new(),
        ));
        let pump = Rc::new_cyclic(|me| Pump {
            host,
            wake: PanelWake::new().unwrap(),
            timer: RefCell::new(None),
            me: me.clone(),
        });
        Panel::new(registered, Some(pump), port, build, Rc::default())
    }

    fn plugin_panels(panels: Vec<Panel>) -> PluginPanels {
        PluginPanels {
            panels,
            theme: RefCell::new(PageTheme::fallback()),
            follow_totals: RefCell::default(),
            editor_gone: Cell::new(false),
            page_built: RefCell::new(None),
        }
    }

    /// A set of colours that differs from the fallback the hosts start with, so setting it sends something.
    fn tokens(background: &str, fg: u32, bg: u32) -> ThemeTokens {
        use eitri_core::theme::payload::{HlAttrs, NvimOptions, NvimThemePayload, PAYLOAD_VERSION};
        ThemeTokens::derive(&NvimThemePayload {
            v: PAYLOAD_VERSION,
            groups: [(
                "Normal".to_owned(),
                HlAttrs {
                    fg: Some(fg),
                    bg: Some(bg),
                    reverse: false,
                },
            )]
            .into_iter()
            .collect(),
            options: NvimOptions {
                background: background.to_owned(),
                guifont: String::new(),
                colors_name: format!("{background}-test"),
                cmdheight: None,
            },
        })
    }

    #[test]
    fn set_theme_reaches_every_host() {
        let scratch = Scratch::new("panels-theme");
        let mut panels = Vec::new();
        let mut pages = Vec::new();
        for name in ["one", "two"] {
            let page = Rc::new(Recorded::default());
            let port = Rc::new(LazyPort::default());
            *port.0.borrow_mut() = Some(page.clone());
            panels.push(panel(&registered(&scratch, name, false, None), port, None));
            pages.push(page);
        }
        let plugin_panels = plugin_panels(panels);
        // A light set: the hosts start from the dark fallback, which would send nothing.
        let light = tokens("light", 0x575279, 0xfaf4ed);
        plugin_panels.set_theme(&light);
        for page in &pages {
            assert_eq!(*page.0.borrow(), ["set_theme", "theme"]);
        }
        assert_eq!(plugin_panels.theme(), PageTheme::from_tokens(&light));
    }

    #[test]
    fn a_start_hidden_plugin_has_no_webview_until_its_first_show() {
        let scratch = Scratch::new("panels-lazy");
        let registered = registered(&scratch, "demo", true, None);
        assert!(registered.hidden_start);
        let page = Rc::new(Recorded::default());
        let builds: Rc<RefCell<Vec<PageTheme>>> = Rc::default();
        let build: BuildPage = {
            let page = page.clone();
            let builds = builds.clone();
            Box::new(move |theme: &PageTheme| {
                builds.borrow_mut().push(theme.clone());
                Some(page as Rc<dyn PagePort>)
            })
        };
        let module = ModuleId::lua("demo");
        let panels = plugin_panels(vec![panel(&registered, Rc::default(), Some(build))]);
        assert!(!panels.is_built(&module));
        assert!(panels.webview(&module).is_none());

        let light = tokens("light", 0x575279, 0xfaf4ed);
        panels.set_theme(&light);
        assert!(
            page.0.borrow().is_empty(),
            "what the host sent before the page existed went nowhere"
        );
        assert!(builds.borrow().is_empty());

        panels.build_page(&module, &panels.theme());
        assert_eq!(
            *builds.borrow(),
            [PageTheme::from_tokens(&light)],
            "built once, in the window's theme"
        );
        assert!(panels.is_built(&module));

        panels.set_theme(&tokens("dark", 0xe0def4, 0x191724));
        assert_eq!(
            *page.0.borrow(),
            ["set_theme", "theme"],
            "the built page hears the host"
        );

        panels.build_page(&module, &panels.theme());
        assert_eq!(builds.borrow().len(), 1, "the page is built once");
        // A module that is not a plugin's: nothing to build, and nothing breaks.
        panels.build_page(&ModuleId::lua("other"), &panels.theme());
        assert!(!panels.is_built(&ModuleId::lua("other")));
    }

    #[test]
    fn a_start_hidden_plugin_answers_its_rule_and_kind() {
        let scratch = Scratch::new("panels-rule");
        let hidden = registered(&scratch, "hidden", true, None);
        let placed = registered(&scratch, "placed", true, Some(PanelSlot::Bottom));
        let plain = registered(&scratch, "plain", false, None);
        let side = registered(&scratch, "side", false, Some(PanelSlot::Main));
        let expected: Vec<(bool, Option<OpenRule>)> = [&hidden, &placed, &plain, &side]
            .iter()
            .map(|r| (r.hidden_start, r.own_rule.clone()))
            .collect();
        let panels = plugin_panels(
            [hidden, placed, plain, side]
                .iter()
                .map(|r| panel(r, Rc::default(), None))
                .collect(),
        );
        let answered: Vec<(bool, Option<OpenRule>)> = ["hidden", "placed", "plain", "side"]
            .iter()
            .map(|name| {
                let id = ModuleId::lua(name);
                (panels.starts_hidden(&id), panels.own_rule(&id))
            })
            .collect();
        assert_eq!(answered, expected, "as the registration says");
        use eitri_core::layout::rules::At;
        use eitri_core::layout::{Direction, Placement};
        assert_eq!(
            answered,
            [
                (
                    true,
                    Some(OpenRule {
                        at: Some(At::Beside(Direction::Right)),
                        size: Some(eitri_core::layout::rules::RuleSize::Share(0.4)),
                        focus: false,
                        float: false,
                        scratchpad: false,
                    })
                ),
                (
                    true,
                    Some(OpenRule {
                        at: Some(At::Placement(Placement::BelowRoot)),
                        size: None,
                        focus: false,
                        float: false,
                        scratchpad: false,
                    })
                ),
                (false, None),
                (
                    false,
                    Some(OpenRule {
                        at: Some(At::Placement(Placement::InPlaceOfEditor)),
                        size: None,
                        focus: true,
                        float: false,
                        scratchpad: false,
                    })
                ),
            ]
        );
        // Not a plugin's module at all.
        assert!(!panels.starts_hidden(&ModuleId::lua("ghost")));
        assert_eq!(panels.own_rule(&ModuleId::lua("ghost")), None);
    }

    fn cmd(owner: &str, cmd: FollowCmd) -> Ordered {
        Ordered::Cmd {
            owner: owner.to_owned(),
            cmd,
            buf: (cmd == FollowCmd::Auto).then_some(3),
        }
    }

    fn batch(ordered: Vec<Ordered>) -> FollowBatch {
        FollowBatch {
            ordered_received: ordered.len() as u64,
            ordered,
            ..FollowBatch::default()
        }
    }

    fn demo() -> ModuleId {
        ModuleId::lua("demo")
    }

    /// `demo` (follows, defines commands, starts hidden) and `other` (neither), each with a page that records.
    fn demo_and_other(scratch: &Scratch, auto: bool) -> (PluginPanels, Rc<Recorded>) {
        let demo = registered_with(scratch, "demo", true, None, |text| {
            if auto {
                text.replacen(
                    "auto = { default = false, kind = \"bool\" }",
                    "auto = { default = true, kind = \"bool\" }",
                    1,
                )
            } else {
                text
            }
        });
        let other = registered(scratch, "other", false, None);
        let page = Rc::new(Recorded::default());
        let port = Rc::new(LazyPort::default());
        *port.0.borrow_mut() = Some(page.clone());
        let panels = plugin_panels(vec![panel(&demo, port, None), panel(&other, Rc::default(), None)]);
        (panels, page)
    }

    #[test]
    fn commands_come_back_in_order_and_auto_only_where_its_setting_is_on() {
        let lines = || {
            batch(vec![
                cmd("demo", FollowCmd::Toggle),
                cmd("ghost", FollowCmd::Show),
                cmd("demo", FollowCmd::Auto),
                cmd("other", FollowCmd::Show),
                cmd("demo", FollowCmd::Hide),
                Ordered::Gone {
                    owner: "demo".into(),
                    buf: 3,
                },
            ])
        };
        let now = Instant::now();

        let scratch = Scratch::new("panels-follow-cmds");
        let (panels, _) = demo_and_other(&scratch, false);
        assert!(panels.wants_follow_feed());
        assert_eq!(
            panels.route_follow(lines(), now),
            [(demo(), FollowCmd::Toggle), (demo(), FollowCmd::Hide)],
            "an unknown owner, an owner without the capability and an auto show that is off are dropped"
        );

        let scratch = Scratch::new("panels-follow-auto");
        let (panels, _) = demo_and_other(&scratch, true);
        assert_eq!(
            panels.route_follow(lines(), now),
            [
                (demo(), FollowCmd::Toggle),
                (demo(), FollowCmd::Auto),
                (demo(), FollowCmd::Hide)
            ],
            "an automatic show passes where its setting is on, still marked automatic"
        );

        // A window whose plugins neither follow nor define commands binds no socket.
        let scratch = Scratch::new("panels-follow-none");
        let other = registered(&scratch, "other", false, None);
        let panels = plugin_panels(vec![panel(&other, Rc::default(), None)]);
        assert!(!panels.wants_follow_feed());
    }

    /// The window as the commands see it: a start-hidden module whose show builds its page, as the grid's change
    /// hook does.
    struct RecordingWindow<'a> {
        panels: &'a PluginPanels,
        shown: Cell<bool>,
        /// Shown but kept off screen (behind a zoom, on a tab not drawn) until a show brings it forward.
        covered: Cell<bool>,
        calls: RefCell<Vec<&'static str>>,
    }

    impl FollowWindow for RecordingWindow<'_> {
        fn is_shown(&self, module: &ModuleId) -> bool {
            *module == demo() && self.shown.get()
        }

        fn is_on_screen(&self, module: &ModuleId) -> bool {
            self.is_shown(module) && !self.covered.get()
        }

        fn show(&self, module: &ModuleId) {
            self.calls.borrow_mut().push("show");
            self.shown.set(true);
            self.covered.set(false);
            self.panels.build_page(module, &self.panels.theme());
        }

        fn hide(&self, _module: &ModuleId) {
            self.calls.borrow_mut().push("hide");
            self.shown.set(false);
        }
    }

    #[test]
    fn toggles_in_one_batch_apply_in_order_and_build_one_page() {
        let scratch = Scratch::new("panels-follow-toggles");
        let registered = registered(&scratch, "demo", true, None);
        assert!(registered.hidden_start);
        let builds = Rc::new(Cell::new(0));
        let build: BuildPage = {
            let builds = builds.clone();
            Box::new(move |_: &PageTheme| {
                builds.set(builds.get() + 1);
                Some(Rc::new(Recorded::default()) as Rc<dyn PagePort>)
            })
        };
        let panels = plugin_panels(vec![panel(&registered, Rc::default(), Some(build))]);
        let window = RecordingWindow {
            panels: &panels,
            shown: Cell::new(false),
            covered: Cell::new(false),
            calls: RefCell::default(),
        };
        let toggles = panels.route_follow(batch(vec![cmd("demo", FollowCmd::Toggle); 5]), Instant::now());
        assert_eq!(toggles.len(), 5);
        apply_follow_commands(&window, toggles);
        assert_eq!(*window.calls.borrow(), ["show", "hide", "show", "hide", "show"]);
        assert!(window.is_shown(&demo()), "an odd count leaves it shown");
        assert_eq!(builds.get(), 1, "one page");
        assert!(panels.is_built(&demo()));

        window.calls.borrow_mut().clear();
        apply_follow_commands(&window, vec![(demo(), FollowCmd::Show)]);
        assert!(window.calls.borrow().is_empty(), "already shown");
        apply_follow_commands(&window, vec![(demo(), FollowCmd::Hide), (demo(), FollowCmd::Hide)]);
        assert_eq!(*window.calls.borrow(), ["hide"]);
        assert_eq!(builds.get(), 1);
    }

    #[test]
    fn a_shown_module_off_screen_is_brought_on_screen_by_show_and_toggle_but_not_by_auto() {
        let scratch = Scratch::new("panels-follow-covered");
        let registered = registered(&scratch, "demo", true, None);
        let panels = plugin_panels(vec![panel(&registered, Rc::default(), None)]);
        let window = RecordingWindow {
            panels: &panels,
            shown: Cell::new(true),
            covered: Cell::new(true),
            calls: RefCell::default(),
        };
        apply_follow_commands(&window, vec![(demo(), FollowCmd::Auto)]);
        assert!(
            window.calls.borrow().is_empty(),
            "an automatic show leaves a zoom or a tab alone"
        );
        apply_follow_commands(&window, vec![(demo(), FollowCmd::Show)]);
        assert_eq!(*window.calls.borrow(), ["show"]);
        assert!(window.is_on_screen(&demo()));

        window.calls.borrow_mut().clear();
        window.covered.set(true);
        apply_follow_commands(&window, vec![(demo(), FollowCmd::Toggle), (demo(), FollowCmd::Toggle)]);
        assert_eq!(
            *window.calls.borrow(),
            ["show", "hide"],
            "a toggle shows what the reader cannot see"
        );

        window.calls.borrow_mut().clear();
        window.shown.set(true);
        window.covered.set(true);
        apply_follow_commands(&window, vec![(demo(), FollowCmd::Hide)]);
        assert_eq!(*window.calls.borrow(), ["hide"], "a hide hides it wherever it is");
    }

    #[test]
    fn an_automatic_show_routed_from_the_feed_leaves_a_zoomed_preview_alone() {
        let scratch = Scratch::new("panels-follow-auto-zoom");
        let (panels, _) = demo_and_other(&scratch, true);
        let window = RecordingWindow {
            panels: &panels,
            shown: Cell::new(true),
            covered: Cell::new(true),
            calls: RefCell::default(),
        };
        let routed = panels.route_follow(batch(vec![cmd("demo", FollowCmd::Auto)]), Instant::now());
        apply_follow_commands(&window, routed);
        assert!(
            window.calls.borrow().is_empty(),
            "a new buffer must not end the zoom or switch the tab over a preview already shown"
        );

        window.shown.set(false);
        window.covered.set(false);
        let routed = panels.route_follow(batch(vec![cmd("demo", FollowCmd::Auto)]), Instant::now());
        apply_follow_commands(&window, routed);
        assert_eq!(
            *window.calls.borrow(),
            ["show"],
            "a hidden preview opens on a new buffer"
        );
    }

    fn view(owner: &str) -> eitri_core::plugin::follow_feed::ViewLine {
        eitri_core::plugin::follow_feed::ViewLine {
            owner: owner.into(),
            seq: 1,
            buf: 3,
            name: "/w/notes/a.md".into(),
            filetype: "markdown".into(),
            tick: 41,
            current: true,
            line_count: 100,
            top: 1,
            top_fraction: 0.0,
            bottom: 40,
            cursor_line: 1,
            cursor_col: 0,
            cause: eitri_core::plugin::protocol::ViewCause::User,
            token: None,
        }
    }

    /// An editor that counts what it is asked and answers that it is not there.
    #[derive(Default)]
    struct CountingEditor(Cell<usize>);

    impl EditorRpc for CountingEditor {
        fn exec_lua(&self, _code: &'static str, _args: Vec<rmpv::Value>) -> eitri_core::nvim_rpc::Pending {
            self.0.set(self.0.get() + 1);
            eitri_core::nvim_rpc::Pending::failed(eitri_core::nvim_rpc::RpcError::Unavailable("test".into()))
        }

        fn target(&self) -> Option<u64> {
            Some(1)
        }
    }

    #[test]
    fn views_reach_their_host_and_wake_it() {
        // The follow feed adds its reader to glib's default main context, which other tests in this crate own
        // while they run, so this one takes its turn and holds the context for its whole run.
        let _turn = crate::layout_state::DEFAULT_CONTEXT_TURN
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let context = glib::MainContext::default();
        let _owner = context
            .acquire()
            .expect("glib's default main context, which the other tests on it leave to this one");
        let scratch = Scratch::new("panels-follow-views");
        let registered = registered(&scratch, "demo", true, None);
        let page = Rc::new(Recorded::default());
        let port = Rc::new(LazyPort::default());
        *port.0.borrow_mut() = Some(page.clone());
        let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wake: agent::UiWake = {
            let wakes = wakes.clone();
            std::sync::Arc::new(move || {
                wakes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            })
        };
        let panels = plugin_panels(vec![panel_waking(&registered, port, None, wake)]);
        panels.set_follow_feed(&scratch.0.join("f-socket"), None);
        let editor = Rc::new(CountingEditor::default());
        panels.set_editor(editor.clone());
        panels.editor_session();
        let host = panels.follower("demo").expect("demo follows").host.clone();
        let request = |id: u64, verb: &str| {
            let text = serde_json::json!({"v": 1, "id": id, "verb": verb, "args": {}}).to_string();
            host.page_message(&text, Instant::now());
        };
        request(1, "host.hello");
        request(2, "editor.follow");
        panels.set_on_screen(&demo(), true);
        page.0.borrow_mut().clear();
        let before = wakes.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(editor.0.get(), 0, "nothing has run the host's pump yet");

        let mut lines = batch(Vec::new());
        lines.views = vec![view("ghost"), view("demo")];
        lines.views_received = 2;
        assert!(panels.route_follow(lines, Instant::now()).is_empty());
        assert_eq!(
            page.0.borrow().iter().filter(|e| *e == "editor.view").count(),
            1,
            "the view for demo reached its page; the one for ghost went nowhere"
        );
        assert!(
            wakes.load(std::sync::atomic::Ordering::Relaxed) > before,
            "the host was woken"
        );
        // The wake alone would run the pump on the main loop's next turn; the routing runs it now, which
        // installs the follow snippet in the editor.
        assert!(editor.0.get() > 0, "the routing ran the host's pump");
    }

    #[test]
    fn on_screen_reaches_the_host_only_when_it_changes() {
        // The follow feed adds its reader to glib's default main context, which other tests in this crate own
        // while they run, so this one takes its turn and holds the context for its whole run.
        let _turn = crate::layout_state::DEFAULT_CONTEXT_TURN
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let context = glib::MainContext::default();
        let _owner = context
            .acquire()
            .expect("glib's default main context, which the other tests on it leave to this one");
        let scratch = Scratch::new("panels-follow-screen");
        let registered = registered(&scratch, "demo", true, None);
        let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wake: agent::UiWake = {
            let wakes = wakes.clone();
            std::sync::Arc::new(move || {
                wakes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            })
        };
        let panels = plugin_panels(vec![panel_waking(&registered, Rc::default(), None, wake)]);
        panels.set_follow_feed(&scratch.0.join("f-socket"), None);
        let count = || wakes.load(std::sync::atomic::Ordering::Relaxed);
        let start = count();
        panels.set_on_screen(&demo(), true);
        let told = count();
        assert!(told > start, "the first report reaches the host");
        panels.set_on_screen(&demo(), true);
        panels.set_on_screen(&demo(), true);
        assert_eq!(count(), told, "the same answer again tells it nothing");
        panels.set_on_screen(&demo(), false);
        assert!(count() > told);
        // Not a plugin's module: nothing.
        panels.set_on_screen(&ModuleId::lua("ghost"), true);
    }

    #[test]
    fn the_editors_end_is_told_once_until_a_new_session() {
        // The follow feed adds its reader to glib's default main context, which other tests in this crate own
        // while they run, so this one takes its turn and holds the context for its whole run.
        let _turn = crate::layout_state::DEFAULT_CONTEXT_TURN
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let context = glib::MainContext::default();
        let _owner = context
            .acquire()
            .expect("glib's default main context, which the other tests on it leave to this one");
        let scratch = Scratch::new("panels-follow-end");
        let registered = registered(&scratch, "demo", true, None);
        let page = Rc::new(Recorded::default());
        let port = Rc::new(LazyPort::default());
        *port.0.borrow_mut() = Some(page.clone());
        let panels = plugin_panels(vec![panel(&registered, port, None)]);
        panels.set_follow_feed(&scratch.0.join("f-socket"), None);
        let host = panels.follower("demo").expect("demo follows").host.clone();
        for (id, verb) in [(1, "host.hello"), (2, "editor.follow")] {
            let text = serde_json::json!({"v": 1, "id": id, "verb": verb, "args": {}}).to_string();
            host.page_message(&text, Instant::now());
        }
        panels.set_on_screen(&demo(), true);
        let closed = || page.0.borrow().iter().filter(|e| *e == "editor.closed").count();
        panels.editor_closed();
        assert_eq!(closed(), 1);
        panels.editor_closed();
        assert_eq!(closed(), 1, "an exit reported twice is told once");
        // A view the dead editor sent before it ended, read only now, does not bring the page back to it.
        let views = || page.0.borrow().iter().filter(|e| *e == "editor.view").count();
        let late = || {
            let mut lines = batch(vec![Ordered::Gone {
                owner: "demo".into(),
                buf: 3,
            }]);
            lines.views = vec![view("demo")];
            lines.views_received = 1;
            lines
        };
        assert!(panels.route_follow(late(), Instant::now()).is_empty());
        assert_eq!(views(), 0);
        assert_eq!(closed(), 1, "nor does its gone line tell the page anything");
        panels.editor_session();
        assert!(panels.route_follow(late(), Instant::now()).is_empty());
        assert_eq!(views(), 1, "a new session's views reach it");
        panels.editor_closed();
        assert_eq!(closed(), 3, "the gone line, then a later editor's end, are told again");
    }

    #[test]
    fn the_follow_trace_prints_totals_at_most_once_a_second_and_only_when_they_moved() {
        let mut totals = FollowTotals::default();
        let t0 = Instant::now();
        assert_eq!(totals.add(0, 0, t0), None, "nothing read yet");
        assert_eq!(totals.add(3, 1, t0).as_deref(), Some("[follow] trace: views=3 cmds=1"));
        assert_eq!(
            totals.add(2, 0, t0 + Duration::from_millis(500)),
            None,
            "within the second"
        );
        assert_eq!(
            totals.add(0, 0, t0 + Duration::from_secs(1)).as_deref(),
            Some("[follow] trace: views=5 cmds=1")
        );
        assert_eq!(totals.add(0, 0, t0 + Duration::from_secs(5)), None, "unchanged");
    }
}
