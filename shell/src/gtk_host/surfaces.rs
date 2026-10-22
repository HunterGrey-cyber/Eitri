//! Module surfaces: one constructor per kind of host, as `build_ui` builds them -- the editor's host,
//! the `WebHost` of a chat view, a Lua panel or a plugin page, a terminal pane -- each added to the
//! grid and registered with the GTK host, and none wired: the startup code wires every host it built
//! once, at its one wiring site, and a host made later is wired by its maker, so no host ever gets a
//! second set of controllers (`wire`'s module doc).
//!
//! The window's start makes its surfaces through two makers before that. [`EditorMaker`] is the
//! editor phase's: the editor's pane, launched with what the controller was told it is launched
//! with, and its host, made before the window's layout (and so its grid) exists. `Surfaces` itself is
//! the surfaces phase's: each host the window opens with, the editor's among them, added to the grid
//! in the order the controller lists them, none wired (the startup wiring site wires them once).
//!
//! [`SurfaceOps`] is the GTK host's `CreateSurface` and `DropSurface` from then on: a constructor plus `wire_host`
//! for a module with no host yet, the host left as it is for one that has one (a killed terminal's pane
//! or a dropped chat view's page taken back out of its pool), and the end of what a kill or a
//! retirement ends. Where WebKit cannot start, a chat view's place is the notice the base
//! view shows, as at startup, and it is answered as built.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use eitri_core::layout::{ModuleId, ModuleKind};
use eitri_core::tabs::ViewId;
use eitri_core::wm::{DropWhy, SurfaceKind, WinId};
use eitri_panel::panel_page::PanelPage;
use gtk4::glib;
use gtk4::prelude::*;
use neovide_editor::{NeovideEditorPane, NeovideEditorPaneOptions};

use crate::agent_panel::{self, AgentPanelHandle};
use crate::chat_views::{self, ChatViews};
use crate::gtk_host::{GtkHost, SurfaceMaker};
use crate::gtk_host_services::LuaPages;
use crate::module_grid::{HostKind, ModuleGrid};
use crate::plugin_panels::PluginPanels;
use crate::terminal::registry::{Pane, Reused, Terminals};
use crate::terminal::TerminalPane;
use crate::web_host::WebHost;
use crate::wire::{wire_host, WireDeps, WireKind};

/// Every extra chat view's page, keyed by its module id (`chat_views`'s module doc).
pub(crate) type ChatPool = Rc<RefCell<ChatViews<(webkit6::WebView, Rc<PanelPage>)>>>;

/// What `build_ui` gives every terminal pane once, when it is made.
pub(crate) type WireTerminal = Rc<dyn Fn(&ModuleId, &TerminalPane)>;

/// What the constructors build with.
pub(crate) struct Surfaces {
    /// Weak: the host holds these constructors through its surface maker.
    pub(crate) host: Weak<GtkHost>,
    /// Weak: the grid's hooks reach the host, which holds these constructors.
    pub(crate) grid: glib::WeakRef<ModuleGrid>,
    /// How a web module's host is allocated (`webkit_sandbox`).
    pub(crate) web_host: HostKind,
    pub(crate) terminals: Rc<RefCell<Terminals<TerminalPane>>>,
    pub(crate) make_terminal_pane: Rc<dyn Fn() -> TerminalPane>,
    pub(crate) chat_views: ChatPool,
    pub(crate) agent: AgentPanelHandle,
    /// The base view's page, every extra page's related one; `None` where WebKit cannot start.
    pub(crate) base_webview: Option<webkit6::WebView>,
    /// What a chat view shows where WebKit cannot start.
    pub(crate) chat_notice: String,
    pub(crate) lua_pages: LuaPages,
    pub(crate) plugin_panels: Rc<PluginPanels>,
    /// The base chat view's page, or the notice that stands in for it where WebKit cannot start.
    pub(crate) base_page: gtk4::Widget,
    /// The editor's host and its focus target, as the editor phase made them; `None` for a window
    /// whose editor is not its own.
    pub(crate) editor: Option<(gtk4::Overlay, gtk4::GLArea)>,
}

/// The editor as the editor phase made it: its pane, the overlay that is its host, and the hidden
/// failure message on that overlay.
pub(crate) struct MadeEditor {
    pub(crate) pane: Rc<NeovideEditorPane>,
    pub(crate) host: gtk4::Overlay,
    pub(crate) failure: gtk4::Label,
}

/// The GTK host's `CreateSurface` before the window's layout exists, which only the editor's needs:
/// its pane is launched with the spawn the op carries, and its host is the small overlay carrying the
/// hidden failure message over the editor's own `GtkGLArea` (`editor_start_failure`), registered as
/// the editor's surface. The grid takes that host at the surfaces phase.
pub(crate) struct EditorMaker {
    host: Weak<GtkHost>,
    made: RefCell<Option<MadeEditor>>,
}

impl EditorMaker {
    pub(crate) fn new(host: Weak<GtkHost>) -> Rc<EditorMaker> {
        Rc::new(EditorMaker {
            host,
            made: RefCell::new(None),
        })
    }

    /// The editor this maker made, handed over once.
    pub(crate) fn take(&self) -> Option<MadeEditor> {
        self.made.borrow_mut().take()
    }
}

impl SurfaceMaker for EditorMaker {
    fn create(&self, win: WinId, module: &ModuleId, kind: &SurfaceKind) -> Result<(), String> {
        if win != WinId::MAIN {
            return Err(format!("{win:?}: this shell has one window"));
        }
        let SurfaceKind::Editor { spawn } = kind else {
            return Err(format!(
                "{module}: only the editor is made before the window's layout exists"
            ));
        };
        if self.made.borrow().is_some() {
            return Ok(());
        }
        let Some(gtk_host) = self.host.upgrade() else {
            return Err("the GTK host is gone".into());
        };
        let pane = Rc::new(NeovideEditorPane::with_options(NeovideEditorPaneOptions {
            clean: spawn.clean,
            child_env: spawn.env.clone(),
            cwd: spawn.cwd.clone(),
            extra_nvim_args: spawn.args.clone(),
        }));
        let (host, failure) = crate::editor_start_failure::install(pane.widget());
        gtk_host.register_surface(win, ModuleId::editor(), host.upcast_ref());
        *self.made.borrow_mut() = Some(MadeEditor { pane, host, failure });
        Ok(())
    }

    fn drop_surface(&self, _win: WinId, module: &ModuleId, _why: &DropWhy) -> Result<(), String> {
        Err(format!(
            "{module}: nothing is dropped before the window's layout exists"
        ))
    }
}

/// What the surfaces phase places for a module of each kind: the one choice it makes that depends on
/// whether WebKit can start here is an extra chat view's, its own page or the notice. Every other
/// web module's page was made before this phase (the panel's, and each Lua panel's and plugin's at
/// registration), already a notice where WebKit cannot start, and is placed as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupPlace {
    /// The host the editor phase made.
    EditorHost,
    /// The base chat view's page as the panel made it.
    BasePage,
    /// An extra chat view's own page, built now.
    ChatPage,
    /// The notice the base view shows, in an extra chat view's place.
    ChatNotice,
    /// A Lua panel's or a plugin's view as `init.lua` registered it.
    RegisteredView,
    /// A terminal's pane from the registry.
    TerminalPane,
}

fn startup_place(kind: &SurfaceKind, webviews: bool) -> StartupPlace {
    match kind {
        SurfaceKind::Editor { .. } => StartupPlace::EditorHost,
        SurfaceKind::Chat { view } if view.is_base() => StartupPlace::BasePage,
        SurfaceKind::Chat { .. } if webviews => StartupPlace::ChatPage,
        SurfaceKind::Chat { .. } => StartupPlace::ChatNotice,
        SurfaceKind::LuaPanel { .. } | SurfaceKind::PluginPage { .. } => StartupPlace::RegisteredView,
        SurfaceKind::Terminal { .. } => StartupPlace::TerminalPane,
    }
}

/// The surfaces phase's `CreateSurface`: each host the window opens with, added to the grid and
/// registered, and wired by no one here. Set on the GTK host for that phase only; [`SurfaceOps`]
/// replaces it once the window's key handling exists.
impl SurfaceMaker for Surfaces {
    fn create(&self, win: WinId, module: &ModuleId, kind: &SurfaceKind) -> Result<(), String> {
        if win != WinId::MAIN {
            return Err(format!("{win:?}: this shell has one window"));
        }
        match startup_place(kind, self.base_webview.is_some()) {
            StartupPlace::EditorHost => {
                let (host, area) = self
                    .editor
                    .as_ref()
                    .ok_or_else(|| format!("{module}: this window's editor is not its own"))?;
                self.place_editor(host, area)
            }
            StartupPlace::BasePage => self.base_chat(&self.base_page),
            StartupPlace::ChatPage => {
                let base = self
                    .base_webview
                    .as_ref()
                    .ok_or_else(|| format!("{module}: WebKit cannot start here"))?;
                self.chat_page_host(module, base)?;
                println!("[chat] {module}: its page and host made at startup");
                Ok(())
            }
            StartupPlace::ChatNotice => {
                self.chat_notice_host(module)?;
                println!("[chat] {module}: its page and host made at startup");
                Ok(())
            }
            StartupPlace::RegisteredView => {
                let widget = self
                    .lua_pages
                    .iter()
                    .find(|(id, _, _, _)| id == module)
                    .map(|(_, widget, _, _)| widget.clone())
                    .ok_or_else(|| format!("{module} is not a Lua panel this window registered"))?;
                self.lua_panel(module, &widget)
            }
            StartupPlace::TerminalPane => {
                if module.kind() != ModuleKind::Terminal {
                    return Err(format!("{module} is not a terminal"));
                }
                self.startup_terminal(module)
            }
        }
    }

    fn drop_surface(&self, _win: WinId, module: &ModuleId, _why: &DropWhy) -> Result<(), String> {
        Err(format!("{module}: nothing is dropped while the window starts"))
    }
}

impl Surfaces {
    fn grid(&self) -> Result<ModuleGrid, String> {
        self.grid
            .upgrade()
            .ok_or_else(|| "the window's grid is gone".to_string())
    }

    /// Adds `host` to the grid as `id`'s and names it as `id`'s surface.
    fn place(
        &self,
        grid: &ModuleGrid,
        id: &ModuleId,
        host: &impl IsA<gtk4::Widget>,
        focus: &impl IsA<gtk4::Widget>,
        kind: HostKind,
    ) {
        grid.add(id.clone(), host, focus, kind);
        match self.host.upgrade() {
            Some(gtk_host) => gtk_host.register_surface(WinId::MAIN, id.clone(), host.upcast_ref()),
            None => eprintln!("[modules] BUG: {id}: its host was built with no GTK host to register it with"),
        }
    }

    /// The editor's host, made by the editor phase, in the grid; its focus target stays the editor's own
    /// `GtkGLArea`.
    fn place_editor(&self, host: &gtk4::Overlay, area: &gtk4::GLArea) -> Result<(), String> {
        let grid = self.grid()?;
        self.place(&grid, &ModuleId::editor(), host, area, HostKind::Direct);
        Ok(())
    }

    /// The base chat view's host: its page, or the notice that stands in for one, at (0,0) of a
    /// `WebHost` (`web_host`'s module doc).
    pub(crate) fn base_chat(&self, page: &gtk4::Widget) -> Result<(), String> {
        let grid = self.grid()?;
        self.place(&grid, &ModuleId::agent(), &WebHost::new(page), page, self.web_host);
        Ok(())
    }

    /// A terminal the layout opens with: its pane from the registry, its host the pane itself.
    pub(crate) fn startup_terminal(&self, id: &ModuleId) -> Result<(), String> {
        let grid = self.grid()?;
        let (pane, _) = self
            .terminals
            .borrow_mut()
            .take_or_make(id, || (self.make_terminal_pane)());
        self.place(&grid, id, pane.widget(), pane.widget(), HostKind::Direct);
        Ok(())
    }

    /// An extra chat view the layout opens with: its page from the pool, the base view's page as its
    /// related one, or, where WebKit cannot start, the notice the base view shows.
    pub(crate) fn startup_chat_view(&self, id: &ModuleId) -> Result<gtk4::Widget, String> {
        match &self.base_webview {
            Some(base) => self.chat_page_host(id, base),
            None => self.chat_notice_host(id),
        }
    }

    /// An extra chat view's page from the pool, the base view's page its related one, at (0,0) of its host.
    fn chat_page_host(&self, id: &ModuleId, base: &webkit6::WebView) -> Result<gtk4::Widget, String> {
        let grid = self.grid()?;
        let view = ViewId::of_module(id).ok_or_else(|| format!("{id} is not a chat view"))?;
        let ((webview, _), _) = self
            .chat_views
            .borrow_mut()
            .take_or_make(id, || agent_panel::build_chat_page(&self.agent, view, Some(base)));
        let host = WebHost::new(&webview);
        self.place(&grid, id, &host, &webview, self.web_host);
        Ok(host.upcast())
    }

    /// The notice the base view shows, in an extra chat view's place, at (0,0) of its host.
    fn chat_notice_host(&self, id: &ModuleId) -> Result<gtk4::Widget, String> {
        let grid = self.grid()?;
        ViewId::of_module(id).ok_or_else(|| format!("{id} is not a chat view"))?;
        let notice = crate::webkit_sandbox::notice_widget(&self.chat_notice);
        let host = WebHost::new(&notice);
        self.place(&grid, id, &host, &notice, self.web_host);
        Ok(host.upcast())
    }

    /// A Lua panel's host, a plugin's page among them: its registered view (a start-hidden plugin's
    /// slot, which holds no view until its page is built) at (0,0) of a `WebHost`.
    pub(crate) fn lua_panel(&self, id: &ModuleId, widget: &gtk4::Widget) -> Result<(), String> {
        let grid = self.grid()?;
        self.place(&grid, id, &WebHost::new(widget), widget, self.web_host);
        Ok(())
    }

    /// A terminal instance's pane, with its shell to start in `dir`: the registry's idle pane of that
    /// number if it was killed before, whose host the grid already holds, else a new pane with its own
    /// host, off screen until something shows it. The pane when its host was made now (the caller wires
    /// it), `None` when the grid already held it.
    pub(crate) fn terminal(&self, id: &ModuleId, dir: PathBuf) -> Result<Option<TerminalPane>, String> {
        let grid = self.grid()?;
        let (pane, reused) = self
            .terminals
            .borrow_mut()
            .take_or_make(id, || (self.make_terminal_pane)());
        pane.set_start_dir(dir);
        if grid.has_host(id) {
            return Ok(None);
        }
        if reused == Reused::Yes {
            eprintln!("[terminal] BUG: {id} reused a pane the grid has no host for; giving it one");
        }
        self.place(&grid, id, pane.widget(), pane.widget(), HostKind::Direct);
        // Off screen until a placement shows it (`ModuleGrid::apply` maps it then).
        pane.widget().set_child_visible(false);
        Ok(Some(pane))
    }

    /// An extra chat view's page and host: its page from the pool -- the live one, else the idle one
    /// recorded under this id, attached again with a fresh document, else a new page built for this
    /// view by the one constructor, with the base view's page as its related one -- and, the first
    /// time, its host, off screen until a placement shows it. The host when it was made now (the
    /// caller wires it), `None` when the grid already held it. Refused where WebKit cannot start.
    pub(crate) fn chat_view(&self, id: &ModuleId, no_webkit: &str) -> Result<Option<gtk4::Widget>, String> {
        let grid = self.grid()?;
        let Some(base) = self.base_webview.as_ref() else {
            return Err(no_webkit.to_string());
        };
        let view = ViewId::of_module(id)
            .filter(|view| !view.is_base())
            .ok_or_else(|| format!("{id} is not an extra chat view"))?;
        let was_live = self.chat_views.borrow().get(id).is_some();
        let ((webview, page), reused) = self
            .chat_views
            .borrow_mut()
            .take_or_make(id, || agent_panel::build_chat_page(&self.agent, view, Some(base)));
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
            // The same page, still answering for this view only: it starts again from an empty record
            // and a fresh document.
            self.agent.attach_page(view, page);
            self.agent.load_document(view);
        }
        if grid.has_host(id) {
            return Ok(None);
        }
        if reused == chat_views::Reused::Yes {
            eprintln!("[chat] BUG: {id} reused a page the grid has no host for; giving it one");
        }
        let host = WebHost::new(&webview);
        self.place(&grid, id, &host, &webview, self.web_host);
        // Off screen until a placement shows it (`ModuleGrid::apply` maps it then).
        host.set_child_visible(false);
        Ok(Some(host.upcast()))
    }

    /// A page that never got its view placed, or whose view left, waits idle under its id.
    pub(crate) fn retire_chat_page(&self, id: &ModuleId) {
        if self.chat_views.borrow_mut().retire(id) {
            if let Some(view) = ViewId::of_module(id) {
                self.agent.idle_page(view);
            }
        }
    }

    /// A killed web module's web process ends: the plugin's own view, for a page built after startup,
    /// else the view in its slot. A page never built has no web process to end.
    pub(crate) fn end_web_process(&self, id: &ModuleId) {
        let Some((_, widget, _, _)) = self.lua_pages.iter().find(|(m, _, _, _)| m == id) else {
            return;
        };
        let webview = self
            .plugin_panels
            .webview(id)
            .or_else(|| widget.downcast_ref::<webkit6::WebView>().cloned());
        if let Some(webview) = webview {
            use webkit6::prelude::*;
            webview.terminate_web_process();
        }
    }
}

/// Whether `id`'s pane or page is in use. A killed terminal's pane and a dropped chat view's page wait
/// idle in their pools; every other module's host is its whole surface.
fn in_use<T: Pane + Clone, C: Clone>(id: &ModuleId, terminals: &Terminals<T>, chat_views: &ChatViews<C>) -> bool {
    match id.kind() {
        ModuleKind::Terminal => terminals.get(id).is_some(),
        ModuleKind::Agent if ViewId::of_module(id).is_some_and(|view| !view.is_base()) => chat_views.get(id).is_some(),
        _ => true,
    }
}

/// What [`SurfaceOps::create`] does for one module, decided from its kind, whether the grid holds its host,
/// whether that host's pane or page is live, and whether WebKit can start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreateRoute {
    /// The grid's host is in use: it keeps its controllers, since wiring it again would install a second set.
    Keep,
    /// Made once, at startup: the editor with its pane, the base view with the panel, every registered Lua panel
    /// and plugin page.
    Refuse,
    /// The chat view's constructor, which takes an idle page back out with a fresh document, or builds one.
    ChatView,
    /// A held chat view where WebKit cannot start: the notice is the whole view and nothing waits in the pool.
    HeldNotice,
    /// A new chat view where WebKit cannot start: a notice host, hidden until placed.
    NewNotice,
    /// The terminal's constructor, which takes an idle pane back out with a fresh start directory, or builds one.
    Terminal,
}

/// A killed terminal's pane and a dropped chat view's page wait idle in their pools while their hosts stay in the
/// grid, so a held host short-circuits only while `live`; the rest go to their constructors, which take the idle
/// one back out and, finding the host, leave it as it is.
fn create_route(kind: &SurfaceKind, held: bool, live: bool, webviews: bool) -> CreateRoute {
    if held && live {
        return CreateRoute::Keep;
    }
    match kind {
        SurfaceKind::Editor { .. } | SurfaceKind::LuaPanel { .. } | SurfaceKind::PluginPage { .. } => {
            CreateRoute::Refuse
        }
        SurfaceKind::Chat { view } if view.is_base() => CreateRoute::Refuse,
        SurfaceKind::Chat { .. } if webviews => CreateRoute::ChatView,
        SurfaceKind::Chat { .. } if held => CreateRoute::HeldNotice,
        SurfaceKind::Chat { .. } => CreateRoute::NewNotice,
        SurfaceKind::Terminal { .. } => CreateRoute::Terminal,
    }
}

/// The GTK host's `CreateSurface` and `DropSurface`, set once the window's key handling exists, since
/// a host made here is wired here.
pub(crate) struct SurfaceOps {
    pub(crate) surfaces: Rc<Surfaces>,
    pub(crate) wire_deps: Rc<WireDeps>,
    /// What every terminal pane gets once, when it is made: what its shell's start and end do.
    pub(crate) wire_terminal: WireTerminal,
}

impl SurfaceMaker for SurfaceOps {
    fn create(&self, win: WinId, module: &ModuleId, kind: &SurfaceKind) -> Result<(), String> {
        if win != WinId::MAIN {
            return Err(format!("{win:?}: this shell has one window"));
        }
        let held = self.surfaces.grid()?.has_host(module);
        let live = held
            && in_use(
                module,
                &self.surfaces.terminals.borrow(),
                &self.surfaces.chat_views.borrow(),
            );
        match create_route(kind, held, live, self.surfaces.base_webview.is_some()) {
            CreateRoute::Keep | CreateRoute::HeldNotice => Ok(()),
            CreateRoute::Refuse => Err(match kind {
                SurfaceKind::Editor { .. } => "the editor's host is made with its pane at startup".into(),
                SurfaceKind::Chat { .. } => "the base chat view's host is made at startup".into(),
                _ => format!("{module} is not a Lua panel this window registered"),
            }),
            route @ (CreateRoute::ChatView | CreateRoute::NewNotice) => {
                let host = if route == CreateRoute::ChatView {
                    self.surfaces.chat_view(module, &self.surfaces.chat_notice)?
                } else {
                    let host = self.surfaces.startup_chat_view(module)?;
                    host.set_child_visible(false);
                    Some(host)
                };
                if let Some(host) = host {
                    wire_host(WireKind::Chat, module, &host, &self.wire_deps);
                }
                Ok(())
            }
            CreateRoute::Terminal => {
                if module.kind() != ModuleKind::Terminal {
                    return Err(format!("{module} is not a terminal"));
                }
                let SurfaceKind::Terminal { cwd } = kind else {
                    return Err(format!("{module}: only a terminal surface has a start directory"));
                };
                if let Some(pane) = self.surfaces.terminal(module, cwd.clone())? {
                    (self.wire_terminal)(module, &pane);
                    wire_host(WireKind::Terminal, module, pane.widget().upcast_ref(), &self.wire_deps);
                }
                Ok(())
            }
        }
    }

    fn drop_surface(&self, _win: WinId, module: &ModuleId, why: &DropWhy) -> Result<(), String> {
        match why {
            DropWhy::Killed => self.surfaces.end_web_process(module),
            DropWhy::Retired => self.surfaces.retire_chat_page(module),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct Idle;

    impl Pane for Idle {
        fn close(&self) {}
    }

    fn instance(kind: ModuleKind, n: u8) -> ModuleId {
        ModuleId::instance(kind, n).unwrap()
    }

    /// Where WebKit cannot start, the surfaces phase builds no page: the extra chat view gets the notice, and
    /// the base view, each Lua panel and each plugin get the view made for them before the phase, which
    /// their own makers made a notice there.
    #[test]
    fn a_sandbox_refusal_leaves_the_webview_placeholders() {
        let chat = instance(ModuleKind::Agent, 2);
        let kinds = [
            SurfaceKind::Chat {
                view: ViewId::of_module(&chat).unwrap(),
            },
            SurfaceKind::Chat { view: ViewId::BASE },
            SurfaceKind::LuaPanel {
                url: "about:blank".into(),
                title: "panel".into(),
            },
            SurfaceKind::PluginPage {
                plugin: "plugin".into(),
            },
        ];
        let refused: Vec<StartupPlace> = kinds.iter().map(|kind| startup_place(kind, false)).collect();
        assert_eq!(
            refused,
            vec![
                StartupPlace::ChatNotice,
                StartupPlace::BasePage,
                StartupPlace::RegisteredView,
                StartupPlace::RegisteredView
            ]
        );
        assert!(
            !refused.contains(&StartupPlace::ChatPage),
            "no page is built where WebKit cannot start"
        );
        assert_eq!(
            startup_place(&kinds[0], true),
            StartupPlace::ChatPage,
            "where it can, an extra chat view gets its own page"
        );
    }

    #[test]
    fn a_killed_pane_or_a_dropped_page_is_not_in_use_while_its_host_stays() {
        let mut terminals = Terminals::new();
        let mut chat_views = ChatViews::new();
        let terminal = instance(ModuleKind::Terminal, 2);
        let chat = instance(ModuleKind::Agent, 2);
        terminals.take_or_make(&terminal, || Idle);
        chat_views.take_or_make(&chat, || ());
        assert!(
            in_use(&terminal, &terminals, &chat_views),
            "a running terminal keeps its pane"
        );
        assert!(
            in_use(&chat, &terminals, &chat_views),
            "an open chat view keeps its page"
        );
        terminals.retire(&terminal);
        chat_views.retire(&chat);
        assert!(
            !in_use(&terminal, &terminals, &chat_views),
            "a killed terminal's pane waits idle"
        );
        assert!(
            !in_use(&chat, &terminals, &chat_views),
            "a dropped chat view's page waits idle"
        );
        // Never in either pool, so the grid's host is the whole surface.
        for id in [ModuleId::editor(), ModuleId::agent()] {
            assert!(in_use(&id, &terminals, &chat_views), "{id} has no pool to wait in");
        }
    }

    /// A `CreateSurface` for a module whose host the grid holds: kept while its pane or page is live, otherwise sent to
    /// its constructor, which takes the idle one back out. Keeping a killed terminal's host would leave its pane idle
    /// and the next show's start refused; sending a live one to its constructor would wire its host twice.
    #[test]
    fn a_held_host_is_kept_only_while_live_and_an_idle_one_goes_to_its_constructor() {
        let terminal = SurfaceKind::Terminal {
            cwd: PathBuf::from("/project"),
        };
        let chat = SurfaceKind::Chat {
            view: ViewId::of_module(&instance(ModuleKind::Agent, 2)).unwrap(),
        };
        let cases = [
            (&terminal, true, true, true, CreateRoute::Keep, "a running terminal"),
            (&chat, true, true, true, CreateRoute::Keep, "an open chat view"),
            (&terminal, true, false, true, CreateRoute::Terminal, "killed terminal"),
            (&terminal, false, false, true, CreateRoute::Terminal, "a new terminal"),
            (&chat, true, false, true, CreateRoute::ChatView, "dropped chat view"),
            (&chat, false, false, true, CreateRoute::ChatView, "a new chat view"),
            (&chat, true, false, false, CreateRoute::HeldNotice, "held, no WebKit"),
            (&chat, false, false, false, CreateRoute::NewNotice, "new, no WebKit"),
        ];
        for (kind, held, live, webviews, want, what) in cases {
            assert_eq!(create_route(kind, held, live, webviews), want, "{what}");
        }

        // A held pane or page that is not live always reaches a constructor, whether WebKit can start or not.
        for webviews in [true, false] {
            for kind in [&terminal, &chat] {
                let got = create_route(kind, true, false, webviews);
                assert_ne!(got, CreateRoute::Keep, "{kind:?}");
            }
        }

        let startup_only = [
            SurfaceKind::Editor {
                spawn: eitri_core::wm::EditorSpawn {
                    env: Vec::new(),
                    args: Vec::new(),
                    cwd: None,
                    clean: false,
                },
            },
            SurfaceKind::Chat { view: ViewId::BASE },
            SurfaceKind::LuaPanel {
                url: "about:blank".into(),
                title: "panel".into(),
            },
            SurfaceKind::PluginPage {
                plugin: "plugin".into(),
            },
        ];
        for kind in &startup_only {
            let held = create_route(kind, true, true, true);
            assert_eq!(held, CreateRoute::Keep, "{kind:?} held");
            let missing = create_route(kind, false, false, true);
            assert_eq!(missing, CreateRoute::Refuse, "{kind:?} not held");
        }
    }
}
