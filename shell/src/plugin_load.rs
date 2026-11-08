//! A plugin page loaded into the running window (`plugin.load`, `:Eitri plugin load <name>`, the empty tab's Pages
//! row), for this run and with its default settings: no `init.lua` line is written and the page has no module key.
//!
//! The controller decides whether a load may start and asks for it with `PluginOp::Load`; this carries it out. The
//! plugin is found, its manifest read and its private directories made on a worker (`load_named`), together with the
//! window's follow socket when the page is the first here to follow the editor or define nvim commands. Back on the
//! GTK thread the page joins the window's plugin panels exactly as one from `init.lua` would have at start (every
//! window fact recorded so far reaches its host), its slot is entered among the Lua pages, its tray chip is made, and
//! only then is the controller told `PluginEvent::Loaded`: the batch that answers it makes the module's host
//! (`SurfaceOps`'s late page route) and opens it. A refusal or a failure is `PluginEvent::LoadFailed` with the
//! loader's own words. A result that arrives once the window has begun to close is dropped.
//!
//! The same place keeps the empty tab's Pages row current in a window that offers one: the shipped pages that are
//! not loaded here, read off the GTK thread at start and after every load.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use eitri_core::agent_bridge::PageLine;
use eitri_core::keymap::Keymap;
use eitri_core::layout::ModuleId;
use eitri_core::lua::panel::PanelSlot;
use eitri_core::lua::plugin::PluginRegistration;
use eitri_core::plugin::follow_feed::{FollowBatch, FollowSocket};
use eitri_core::plugin::load::LoadedPlugin;
use eitri_core::plugin::manifest::{Capability, Manifest, ShownWhile};
use eitri_core::plugin::search::{PageOffer, SearchEnv};
use eitri_core::wm::window::Tiling;
use eitri_core::wm::{HostEvent, HostSink, PluginEvent, WinId};
use gtk4::glib;

use crate::agent_panel::AgentPanelHandle;
use crate::editor_feeds::FeedPump;
use crate::gtk_host_services::LuaPages;
use crate::linked_window::editor::Window1Editor;
use crate::lua::LuaEngine;
use crate::plugin_panels::PluginPanels;

/// Why a load is refused where WebKit cannot start: the page would have no view.
pub(crate) const NO_WEBKIT: &str = "pages cannot start here: WebKit's sandbox cannot start";

/// The scratch buffer's name for an offered page's `init.lua` line.
pub(crate) const PAGE_LINE_TITLE: &str = "init.lua line";

/// The window's follow socket, shared with the teardown that removes it: bound at start when a plugin from
/// `init.lua` follows the editor, or later by the first loaded page that does, and never twice.
pub(crate) type FollowSlot = Rc<RefCell<Option<FollowSocket>>>;

/// What reads the follow socket's lines in this window: the same reader whichever load bound it.
pub(crate) type FollowReader = Rc<dyn Fn(FollowBatch)>;

/// What kind of window this is, as far as loading a page into it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowKind {
    pub(crate) tiling: Tiling,
    /// The window runs its own editor: false in window 2 of a pair.
    pub(crate) owns_editor: bool,
    /// A chat or page window, which shows its one module alone.
    pub(crate) module_window: bool,
    /// A page window beside a person's own nvim.
    pub(crate) beside: bool,
}

impl WindowKind {
    /// A full window 1 in Eitri's own tiling: the one kind of window a page is loaded into at run time. In compositor
    /// mode a page is a window of its own, and window 2, a module window and a page window beside nvim hold only what
    /// window 1 or their own start gave them.
    pub(crate) fn takes_loads(&self) -> bool {
        self.tiling == Tiling::Eitri && self.owns_editor && !self.module_window && !self.beside
    }

    /// Whether the empty tab shows its Pages row: only where a page can be loaded and built.
    pub(crate) fn offers_pages(&self, webviews: bool) -> bool {
        self.takes_loads() && webviews
    }
}

/// What refuses a load before any disk work: WebKit unable to start, or a name that is a Lua panel's id.
pub(crate) fn refusal(name: &str, webviews: bool, lua_panels: &[String]) -> Option<String> {
    if !webviews {
        return Some(NO_WEBKIT.into());
    }
    lua_panels
        .iter()
        .any(|id| id == name)
        .then(|| format!("{name} is the id of a Lua panel in this window"))
}

/// Whether a page wants the follow socket's lines: it follows the editor or defines nvim commands.
pub(crate) fn wants_follow(manifest: &Manifest) -> bool {
    manifest.has(Capability::EditorFollow) || manifest.has(Capability::EditorCommands)
}

/// A loaded page's registration: its defaults, no module key, placed where a page with no `position` goes.
pub(crate) fn registration_of(plugin: Rc<LoadedPlugin>) -> PluginRegistration {
    PluginRegistration {
        plugin,
        key: None,
        no_key: false,
        slot: PanelSlot::Side,
        position: None,
    }
}

/// What the controller is told once the page has joined: its module, its title, whether it shows while agents wait
/// (then it is not shown by the load), and the rule its first show opens it by.
pub(crate) fn loaded_event(win: WinId, registration: &PluginRegistration) -> PluginEvent {
    let manifest = &registration.plugin.manifest;
    PluginEvent::Loaded {
        win,
        module: ModuleId::lua(&manifest.id),
        title: manifest.title.clone(),
        shown_while: manifest
            .module
            .as_ref()
            .is_some_and(|module| module.shown_while == Some(ShownWhile::AgentsWait)),
        own_rule: registration.own_rule(),
    }
}

/// The Pages row's lines, in the order offered.
pub(crate) fn page_lines(offers: Vec<PageOffer>) -> Vec<PageLine> {
    offers
        .into_iter()
        .map(|offer| PageLine {
            id: offer.id,
            title: offer.title,
            summary: offer.summary,
        })
        .collect()
}

/// What the worker hands back: the plugin, and the follow socket it bound for it.
type Worked = Result<(LoadedPlugin, Option<FollowSocket>), String>;

/// What the loads act through; every part is the window's own, made before its services.
pub(crate) struct LateDeps {
    pub(crate) engine: Rc<LuaEngine>,
    pub(crate) keymap: Rc<Keymap>,
    pub(crate) plugin_panels: Rc<PluginPanels>,
    pub(crate) lua_pages: LuaPages,
    pub(crate) sink: Weak<dyn HostSink>,
    /// Where plugin state lives, as at start.
    pub(crate) state_root: Option<PathBuf>,
    pub(crate) home: Option<PathBuf>,
    /// Eitri's scratch directory, whose buffers no page follows.
    pub(crate) scratch_path: Option<PathBuf>,
    /// Window 1 of a pair: what window 2 needs to follow the editor for a page that moves there.
    pub(crate) window1: Option<Rc<Window1Editor>>,
    pub(crate) agent: AgentPanelHandle,
    /// Whether the empty tab has a Pages row here ([`WindowKind::offers_pages`]).
    pub(crate) offers_pages: bool,
    pub(crate) webviews: bool,
}

/// The window's follow socket and its reader, set once the reader exists.
struct Follow {
    feed_pump: Rc<FeedPump>,
    reader: FollowReader,
}

/// The runtime loads of one window.
pub(crate) struct LateLoads {
    deps: LateDeps,
    follow_slot: FollowSlot,
    follow: RefCell<Option<Follow>>,
    /// What makes a joined page's tray chip, with its module and title.
    chip: RefCell<Option<Rc<dyn Fn(&ModuleId, &str)>>>,
    /// The pages loaded at run time, for the next load's window-roots check.
    late: RefCell<Vec<Rc<LoadedPlugin>>>,
    /// Set by the window's teardown: a result arriving after it is dropped.
    closing: Cell<bool>,
    /// The latest Pages read asked for; an older one that finishes later is dropped.
    offers_asked: Rc<Cell<u64>>,
}

impl LateLoads {
    pub(crate) fn new(deps: LateDeps, follow_slot: FollowSlot) -> Rc<LateLoads> {
        Rc::new(LateLoads {
            deps,
            follow_slot,
            follow: RefCell::new(None),
            chip: RefCell::new(None),
            late: RefCell::new(Vec::new()),
            closing: Cell::new(false),
            offers_asked: Rc::default(),
        })
    }

    /// The reader the window reads the follow socket with, and the pump it reads on.
    pub(crate) fn set_follow(&self, feed_pump: Rc<FeedPump>, reader: FollowReader) {
        *self.follow.borrow_mut() = Some(Follow { feed_pump, reader });
    }

    /// What makes a joined page's tray chip.
    pub(crate) fn set_chip(&self, chip: Rc<dyn Fn(&ModuleId, &str)>) {
        *self.chip.borrow_mut() = Some(chip);
    }

    /// The window is closing: nothing joins it from now on.
    pub(crate) fn window_closing(&self) {
        self.closing.set(true);
    }

    fn post(&self, event: PluginEvent) {
        if let Some(sink) = self.deps.sink.upgrade() {
            sink.post(HostEvent::Plugin(event));
        }
    }

    fn fail(&self, win: WinId, name: &str, why: String) {
        eprintln!("[plugin {name}] not loaded: {why}");
        self.post(PluginEvent::LoadFailed {
            win,
            name: name.into(),
            why,
        });
    }

    /// The ids of every Lua panel registered here that is no plugin's page.
    fn lua_panel_ids(&self) -> Vec<String> {
        let plugins = self.deps.plugin_panels.modules();
        self.deps
            .engine
            .panels
            .borrow()
            .entries()
            .iter()
            .filter(|entry| !plugins.contains(&entry.module))
            .map(|entry| entry.id.clone())
            .collect()
    }

    /// Carries out `PluginOp::Load`: the refusals first, then the worker, then [`LateLoads::join`] on this thread.
    pub(crate) fn load(self: &Rc<Self>, win: WinId, name: &str) {
        if let Some(why) = refusal(name, self.deps.webviews, &self.lua_panel_ids()) {
            self.fail(win, name, why);
            return;
        }
        let bind_follow = self.follow_slot.borrow().is_none();
        let (state_root, home) = (self.deps.state_root.clone(), self.deps.home.clone());
        let worker_name = name.to_owned();
        let name = name.to_owned();
        let me = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let worked: Worked = gtk4::gio::spawn_blocking(move || {
                let env = SearchEnv::from_process();
                let plugin = eitri_core::lua::plugin::load_named(
                    &worker_name,
                    state_root.as_deref(),
                    home.as_deref(),
                    &env,
                    &|line| eprintln!("{line}"),
                )?;
                let socket = if bind_follow && wants_follow(&plugin.manifest) {
                    FollowSocket::bind()
                } else {
                    None
                };
                Ok((plugin, socket))
            })
            .await
            .unwrap_or_else(|_| Err("the loading thread panicked".into()));
            let Some(me) = me.upgrade().filter(|me| !me.closing.get()) else {
                if let Ok((_, Some(socket))) = worked {
                    drop_off_thread(socket);
                }
                return;
            };
            me.join(win, &name, worked);
        });
    }

    /// The worker's result on the GTK thread: the page joins, its follow socket and its window-1 registration are
    /// set, and the controller hears of it last, so the batch that answers finds its slot.
    fn join(&self, win: WinId, name: &str, worked: Worked) {
        let (plugin, socket) = match worked {
            Ok(worked) => worked,
            Err(why) => return self.fail(win, name, why),
        };
        if let Some(why) = refusal(name, self.deps.webviews, &self.lua_panel_ids()) {
            if let Some(socket) = socket {
                drop_off_thread(socket);
            }
            return self.fail(win, name, why);
        }
        if let Err(why) = self.check_roots(&plugin) {
            if let Some(socket) = socket {
                drop_off_thread(socket);
            }
            return self.fail(win, name, why);
        }
        if let Some(socket) = socket {
            self.take_follow_socket(socket);
        }
        let registration = registration_of(Rc::new(plugin));
        let joined = self
            .deps
            .plugin_panels
            .join(&self.deps.engine, &self.deps.keymap, &registration);
        if let Err(why) = joined {
            return self.fail(win, name, why);
        }
        let module = ModuleId::lua(&registration.plugin.manifest.id);
        let entry = {
            let panels = self.deps.engine.panels.borrow();
            let url = panels
                .entries()
                .iter()
                .find(|entry| entry.module == module)
                .map(|entry| entry.url.clone());
            url.zip(panels.views.widget(&module).cloned())
                .zip(panels.views.guard(&module))
                .map(|((url, widget), guard)| (module.clone(), widget, url, guard))
        };
        match entry {
            Some(entry) => self.deps.lua_pages.borrow_mut().push(entry),
            None => eprintln!("[plugin {name}] BUG: joined without a registered view"),
        }
        self.late.borrow_mut().push(registration.plugin.clone());
        self.register_follow(name);
        let chip = self.chip.borrow().clone();
        if let Some(chip) = chip {
            chip(&module, &registration.plugin.manifest.title);
        }
        self.post(loaded_event(win, &registration));
        self.refresh_offers();
    }

    /// Two plugins that own one directory's window would each claim it: a page whose `[window] root` another plugin
    /// here declared too is refused.
    fn check_roots(&self, plugin: &LoadedPlugin) -> Result<(), String> {
        if plugin.window_root.is_none() {
            return Ok(());
        }
        let started: Vec<Rc<LoadedPlugin>> = self
            .deps
            .engine
            .plugins
            .borrow()
            .plugins()
            .iter()
            .map(|registration| registration.plugin.clone())
            .collect();
        let late = self.late.borrow().clone();
        let calls: Vec<(Rc<LoadedPlugin>, String)> = started
            .into_iter()
            .map(|p| {
                let call = format!("plugin {} (loaded by init.lua)", p.manifest.id);
                (p, call)
            })
            .chain(late.into_iter().map(|p| {
                let call = format!("plugin {} (loaded for this run)", p.manifest.id);
                (p, call)
            }))
            .collect();
        let this = format!("plugin {} (loaded for this run)", plugin.manifest.id);
        let mut all: Vec<(&LoadedPlugin, &str)> = calls.iter().map(|(p, call)| (&**p, call.as_str())).collect();
        all.push((plugin, this.as_str()));
        eitri_core::plugin::load::check_window_roots(&all)
    }

    /// The socket a worker bound becomes the window's, read by the window's reader and told to every page that wants
    /// it; one bound by a load that lost the race to another is let go.
    fn take_follow_socket(&self, mut socket: FollowSocket) {
        let follow = self
            .follow
            .borrow()
            .as_ref()
            .map(|follow| (follow.feed_pump.clone(), follow.reader.clone()));
        let free = self.follow_slot.borrow().is_none();
        let (Some((feed_pump, reader)), true) = (follow, free) else {
            drop_off_thread(socket);
            return;
        };
        feed_pump.listen_follow(&mut socket, move |batch| reader(batch));
        let path = socket.socket_path().to_owned();
        *self.follow_slot.borrow_mut() = Some(socket);
        self.deps
            .plugin_panels
            .set_follow_feed(&path, self.deps.scratch_path.as_deref());
    }

    /// Window 1 tells window 2 how a page that moves there follows the editor, as it does for the pages from start.
    fn register_follow(&self, owner: &str) {
        let Some(one) = &self.deps.window1 else { return };
        let path = match self.follow_slot.borrow().as_ref() {
            Some(socket) => socket.socket_path().to_owned(),
            None => return,
        };
        if let Some(config) = self
            .deps
            .plugin_panels
            .follow_config(owner, &path, self.deps.scratch_path.as_deref())
        {
            one.set_plugin_follow(owner, config);
        }
    }

    /// Reads the shipped pages not loaded here off the GTK thread and offers them on the empty tab. A window with no
    /// Pages row is never offered any.
    pub(crate) fn refresh_offers(&self) {
        if !self.deps.offers_pages || self.closing.get() {
            return;
        }
        let asked = self.offers_asked.get() + 1;
        self.offers_asked.set(asked);
        let loaded: Vec<String> = self
            .deps
            .engine
            .panels
            .borrow()
            .entries()
            .iter()
            .map(|entry| entry.id.clone())
            .collect();
        let agent = self.deps.agent.clone();
        let offers_asked = Rc::downgrade(&self.offers_asked);
        glib::spawn_future_local(async move {
            let offers = gtk4::gio::spawn_blocking(move || {
                eitri_core::plugin::search::offers(&SearchEnv::from_process(), &loaded)
            })
            .await;
            let Ok(offers) = offers else {
                eprintln!("[plugin] the pages on offer could not be read: the reading thread panicked");
                return;
            };
            if offers_asked.upgrade().is_some_and(|latest| latest.get() == asked) {
                agent.set_pages(page_lines(offers));
            }
        });
    }
}

/// A socket nobody reads is removed on a thread of its own: its `Drop` removes its directory.
fn drop_off_thread(socket: FollowSocket) {
    let dropped = std::thread::Builder::new()
        .name("follow-socket-drop".into())
        .spawn(move || drop(socket));
    if let Err(err) = dropped {
        eprintln!("[follow] an unused socket's directory was removed on this thread ({err})");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::layout::OpenRule;
    use eitri_core::plugin::load::load_plugin;
    use eitri_core::plugin::test_support::{demo_plugin, Scratch};

    fn demo(edit: impl Fn(String) -> String) -> (Scratch, Rc<LoadedPlugin>) {
        let scratch = Scratch::new("late-load");
        let (dir, settings) = demo_plugin(&scratch.0);
        let manifest = dir.join("eitri-plugin.toml");
        let text = edit(std::fs::read_to_string(&manifest).unwrap());
        std::fs::write(&manifest, text).unwrap();
        let plugin = Rc::new(load_plugin(dir.to_str().unwrap(), &settings, None).unwrap());
        (scratch, plugin)
    }

    fn kind(tiling: Tiling) -> WindowKind {
        WindowKind {
            tiling,
            owns_editor: true,
            module_window: false,
            beside: false,
        }
    }

    #[test]
    fn only_a_full_window_1_in_eitri_tiling_takes_loads_and_offers_pages() {
        assert!(kind(Tiling::Eitri).takes_loads());
        assert!(kind(Tiling::Eitri).offers_pages(true));
        assert!(
            !kind(Tiling::Eitri).offers_pages(false),
            "no row where no page can be built"
        );
        for tiling in [Tiling::Hub, Tiling::ModuleWindow] {
            assert!(!kind(tiling).takes_loads(), "{tiling:?}");
            assert!(!kind(tiling).offers_pages(true), "{tiling:?}");
        }
        let window2 = WindowKind {
            owns_editor: false,
            ..kind(Tiling::Eitri)
        };
        let module_window = WindowKind {
            module_window: true,
            ..kind(Tiling::Eitri)
        };
        let beside = WindowKind {
            beside: true,
            ..kind(Tiling::Eitri)
        };
        for other in [window2, module_window, beside] {
            assert!(!other.takes_loads(), "{other:?}");
            assert!(!other.offers_pages(true), "{other:?}");
        }
    }

    #[test]
    fn a_load_is_refused_without_webkit_or_for_a_lua_panels_id() {
        assert_eq!(refusal("demo", false, &[]).as_deref(), Some(NO_WEBKIT));
        let why = refusal("notes", true, &["notes".into()]).unwrap();
        assert!(why.contains("notes") && why.contains("Lua panel"), "{why}");
        assert_eq!(refusal("demo", true, &["notes".into()]), None);
    }

    #[test]
    fn the_registration_has_defaults_no_key_and_the_side_slot() {
        let (_scratch, plugin) = demo(|text| text);
        let registration = registration_of(plugin.clone());
        assert!(Rc::ptr_eq(&registration.plugin, &plugin));
        assert_eq!(registration.key, None);
        assert_eq!(registration.slot, PanelSlot::Side);
        assert_eq!(registration.position, None);
    }

    #[test]
    fn the_loaded_event_carries_the_manifests_title_rule_and_wait_show() {
        let (_scratch, plugin) = demo(|text| text);
        let registration = registration_of(plugin);
        let PluginEvent::Loaded {
            win,
            module,
            title,
            shown_while,
            own_rule,
        } = loaded_event(WinId::MAIN, &registration)
        else {
            panic!("a loaded event");
        };
        assert_eq!(win, WinId::MAIN);
        assert_eq!(module, ModuleId::lua("demo"));
        assert_eq!(title, "Demo");
        assert!(!shown_while);
        let rule: OpenRule = own_rule.expect("the manifest's [module] rule");
        assert_eq!(Some(rule.clone()), registration.own_rule());
        assert!(!rule.focus);
        assert!(rule.at.is_some());

        let (_scratch, plugin) = demo(|text| {
            // `agents` and `net` are never declared together.
            text.replacen(
                "capabilities = [\"tasks\",",
                "capabilities = [\"agents\", \"tasks\",",
                1,
            )
            .replacen(" \"net\",", "", 1)
            .replacen(
                "[net]\norigins = [\"https://learn.example.org\"]\nimages = \"${images}\"\n",
                "",
                1,
            )
            .replacen(
                "start = \"hidden\"\n",
                "start = \"hidden\"\nshown_while = \"agents-wait\"\n",
                1,
            )
        });
        let PluginEvent::Loaded { shown_while, .. } = loaded_event(WinId::MAIN, &registration_of(plugin)) else {
            panic!("a loaded event");
        };
        assert!(
            shown_while,
            "a page that shows while agents wait is not shown by its load"
        );

        let (_scratch, plugin) = demo(|text| {
            let text = text[..text.find("\n[module]\n").unwrap() + 1].to_string();
            text.replacen(", \"editor.follow\", \"editor.commands\"]", "]", 1)
        });
        let registration = registration_of(plugin);
        let PluginEvent::Loaded {
            shown_while, own_rule, ..
        } = loaded_event(WinId::MAIN, &registration)
        else {
            panic!("a loaded event");
        };
        assert!(!shown_while);
        assert_eq!(own_rule, None, "no [module]: no rule of its own");
        assert!(!wants_follow(&registration.plugin.manifest));
    }

    #[test]
    fn a_page_that_follows_or_has_commands_wants_the_follow_socket() {
        let (_scratch, plugin) = demo(|text| text);
        assert!(wants_follow(&plugin.manifest));
        let (_scratch, plugin) = demo(|text| {
            text.replacen(", \"editor.follow\", \"editor.commands\"]", ", \"editor.commands\"]", 1)
                .replacen(
                    "[follow]\nfiletypes = [\"markdown\", \"rst\"]\nauto_show = \"auto\"\n",
                    "",
                    1,
                )
        });
        assert!(wants_follow(&plugin.manifest));
    }

    #[test]
    fn page_lines_keep_the_offers_order_and_words() {
        let lines = page_lines(vec![
            PageOffer {
                id: "markdown".into(),
                title: "Markdown".into(),
                summary: Some("Read".into()),
            },
            PageOffer {
                id: "agents".into(),
                title: "Agents".into(),
                summary: None,
            },
        ]);
        assert_eq!(
            lines,
            vec![
                PageLine {
                    id: "markdown".into(),
                    title: "Markdown".into(),
                    summary: Some("Read".into()),
                },
                PageLine {
                    id: "agents".into(),
                    title: "Agents".into(),
                    summary: None,
                },
            ]
        );
    }
}
