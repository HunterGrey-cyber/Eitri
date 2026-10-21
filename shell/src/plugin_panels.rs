//! The window's plugin pages. Each plugin `init.lua` loaded becomes a Lua panel module `lua:<id>` whose
//! content is the plugin's page, with its host pumped from a GLib watch on a wake its threads raise, plus a
//! one-shot timer only when the host asks for one: a plugin with nothing running costs no wake-up. Kill,
//! window close, the layout and the window's editor are connected here.

use std::cell::{OnceCell, RefCell};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use eitri_core::editor_rpc::EditorRpc;
use eitri_core::keymap::Keymap;
use eitri_core::layout::ModuleId;
use eitri_core::plugin::host::{LayoutPort, PluginHost};
use eitri_core::plugin::keys::EffectiveRow;
use eitri_core::plugin::page_policy;
use eitri_core::plugin::protocol::FocusTarget;
use eitri_core::plugin::state::PluginStore;
use eitri_core::plugin::tasks::{RunsWaiter, GRACE};
use eitri_core::plugin::theme::PageTheme;
use eitri_core::theme::ThemeTokens;
use eitri_panel::panel_wake::PanelWake;
use gtk4::glib;
use gtk4::prelude::*;

use crate::lua::{install_crash_recovery, LuaEngine, PanelEntry};
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

pub(crate) struct PluginPanels {
    panels: Vec<(ModuleId, Option<Rc<Pump>>)>,
}

impl PluginPanels {
    /// Registers every loaded plugin as a Lua panel entry, each host checking its page's keys against the
    /// window's prefix and applying the user's `plugin:<id>` overrides, and each page starting in `theme`.
    /// `Err` is a startup failure's text.
    pub(crate) fn install(engine: &LuaEngine, keymap: &Keymap, theme: &PageTheme) -> Result<PluginPanels, String> {
        let loaded: Vec<_> = engine
            .plugins
            .borrow()
            .plugins()
            .iter()
            .map(|r| (r.plugin.clone(), r.key.clone(), r.slot))
            .collect();
        let store_root = PluginStore::eitri_root_from_env();
        let mut panels = Vec::new();
        let keys_settings =
            eitri_core::plugin::kit_settings::from_config(|key| engine.config.borrow().get(key).map(str::to_owned))?;
        for (plugin, key, slot) in loaded {
            let id = plugin.manifest.id.clone();
            let clash = engine.panels.borrow().entries().iter().any(|e| e.id == id);
            if clash {
                return Err(format!("eitri.plugin.load: plugin {id} has the id of a Lua panel"));
            }
            let module = ModuleId::lua(&id);
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
            let (widget, crash_guard, pump) = match wake {
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
                    let spec = PageSpec {
                        id: id.clone(),
                        site_root: plugin.site_root.clone(),
                        entry: plugin.manifest.entry.clone(),
                        origins: plugin.manifest.origins.clone(),
                        downloads: downloads_dir(),
                    };
                    let webview = plugin_page::build(&spec, hooks, theme);
                    let page = Rc::new(WebKitPluginPage(webview.downgrade()));
                    let store = store_root.clone().map(|root| PluginStore::new(root, &id));
                    let overrides = keymap.plugin_user(&id).to_vec();
                    let host = Rc::new(PluginHost::new(plugin.clone(), page, wake.waker(), store, overrides));
                    host.set_prefix_key(keymap.prefix());
                    host.set_keys_settings(keys_settings.clone());
                    // The view already wears `theme`; the host records it so it sends nothing for the same set.
                    host.set_theme(theme);
                    host.set_module_key(key.as_deref());
                    let pump = Pump::start(host, wake);
                    let _ = cell.set(pump.clone());
                    let guard = install_crash_recovery(
                        &webview,
                        id.clone(),
                        url.clone(),
                        Some(page_policy::host_page_base(&id)),
                    );
                    (webview.upcast::<gtk4::Widget>(), guard, Some(pump))
                }
                None => (
                    crate::webkit_sandbox::lua_panel_placeholder(&title),
                    Rc::new(RefCell::new(WebViewCrashGuard::with_defaults())),
                    None,
                ),
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
            panels.push((module, pump));
        }
        Ok(PluginPanels { panels })
    }

    fn hosts(&self) -> impl Iterator<Item = (&ModuleId, &Rc<Pump>)> {
        self.panels
            .iter()
            .filter_map(|(module, pump)| pump.as_ref().map(|p| (module, p)))
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

    #[test]
    fn set_theme_reaches_every_host() {
        use eitri_core::plugin::load::load_plugin;
        use eitri_core::plugin::test_support::{demo_plugin, Scratch};
        use eitri_core::theme::payload::{HlAttrs, NvimOptions, NvimThemePayload, PAYLOAD_VERSION};

        let scratch = Scratch::new("panels-theme");
        let mut panels = Vec::new();
        let mut pages = Vec::new();
        for name in ["one", "two"] {
            let (dir, settings) = demo_plugin(&scratch.0.join(name));
            let plugin = Rc::new(load_plugin(dir.to_str().unwrap(), &settings, None).unwrap());
            let page = Rc::new(Recorded::default());
            let host = Rc::new(PluginHost::new(
                plugin,
                page.clone(),
                std::sync::Arc::new(|| {}),
                None,
                Vec::new(),
            ));
            // Built directly, not through `Pump::start`: no wake is watched, so nothing runs on a main loop.
            let pump = Rc::new_cyclic(|me| Pump {
                host,
                wake: PanelWake::new().unwrap(),
                timer: RefCell::new(None),
                me: me.clone(),
            });
            panels.push((ModuleId::lua(name), Some(pump)));
            pages.push(page);
        }
        let plugin_panels = PluginPanels { panels };
        // A light set: the hosts start from the dark fallback, which would send nothing.
        let light = NvimThemePayload {
            v: PAYLOAD_VERSION,
            groups: [(
                "Normal".to_owned(),
                HlAttrs {
                    fg: Some(0x575279),
                    bg: Some(0xfaf4ed),
                    reverse: false,
                },
            )]
            .into_iter()
            .collect(),
            options: NvimOptions {
                background: "light".to_owned(),
                guifont: String::new(),
                colors_name: "dawn".to_owned(),
                cmdheight: None,
            },
        };
        plugin_panels.set_theme(&ThemeTokens::derive(&light));
        for page in &pages {
            assert_eq!(*page.0.borrow(), ["set_theme", "theme"]);
        }
    }
}
