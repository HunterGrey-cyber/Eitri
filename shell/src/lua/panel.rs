//! Lua panel views and their crash recovery. Registration order and replacement live in core;
//! the shell retains widgets and their shared crash guards by module id.

use eitri_core::layout::ModuleId;
use eitri_core::lua::panel::{parse_panel_spec, resolve_panel_url, PanelSlot};
use gtk4::prelude::*;
use mlua::{Lua, Table};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use webkit6::prelude::*;

type CrashGuard = Rc<RefCell<crate::webview_crash_guard::WebViewCrashGuard>>;

/// Views may be built on either side of registration, so neither operation discards the other half.
pub(crate) struct PanelViews<W = gtk4::Widget> {
    views: std::collections::HashMap<eitri_core::layout::ModuleId, (Option<W>, CrashGuard)>,
}

impl<W> Default for PanelViews<W> {
    fn default() -> Self {
        Self {
            views: Default::default(),
        }
    }
}

impl<W> PanelViews<W> {
    pub(crate) fn insert_guard(&mut self, module: eitri_core::layout::ModuleId, guard: CrashGuard) {
        match self.views.entry(module) {
            std::collections::hash_map::Entry::Occupied(mut entry) => entry.get_mut().1 = guard,
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert((None, guard));
            }
        }
    }

    pub(crate) fn set_widget(&mut self, module: &eitri_core::layout::ModuleId, widget: W) {
        self.views
            .entry(module.clone())
            .or_insert_with(|| {
                (
                    None,
                    Rc::new(RefCell::new(
                        crate::webview_crash_guard::WebViewCrashGuard::with_defaults(),
                    )),
                )
            })
            .0 = Some(widget);
    }

    pub(crate) fn widget(&self, module: &eitri_core::layout::ModuleId) -> Option<&W> {
        self.views.get(module).and_then(|(widget, _)| widget.as_ref())
    }

    pub(crate) fn guard(&self, module: &eitri_core::layout::ModuleId) -> Option<CrashGuard> {
        self.views.get(module).map(|(_, guard)| guard.clone())
    }
}

/// The shell installer hands the declaration and view to their separate owners in one call.
pub(crate) struct PanelEntry {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) slot: PanelSlot,
    pub(crate) key: Option<String>,
    pub(crate) url: String,
    pub(crate) widget: gtk4::Widget,
    pub(crate) crash_guard: CrashGuard,
}

/// Adapter for the shell's installers; core alone decides registration order and replacement.
#[derive(Default)]
pub(crate) struct PanelRegistry {
    registry: eitri_core::lua::panel::PanelRegistry,
    pub(crate) views: PanelViews,
    /// Panels register with an empty place, no view and no web process: the window may yet turn out to be a hub,
    /// whose panels are windows of their own.
    holding: bool,
}

impl PanelRegistry {
    pub(crate) fn register(&mut self, entry: PanelEntry) {
        let module = eitri_core::layout::ModuleId::lua(&entry.id);
        self.views.insert_guard(module.clone(), entry.crash_guard);
        self.views.set_widget(&module, entry.widget);
        self.registry.register(eitri_core::lua::panel::PanelEntry {
            module,
            id: entry.id,
            title: entry.title,
            slot: entry.slot,
            key: entry.key,
            url: entry.url,
        });
    }

    pub(crate) fn entries(&self) -> &[eitri_core::lua::panel::PanelEntry] {
        self.registry.entries()
    }

    /// Panels registered from now on wait for [`settle_views`](Self::settle_views). Nothing waits where WebKit cannot
    /// start: there a panel's notice is its view.
    pub(crate) fn hold_views(&mut self) {
        self.holding = crate::webkit_sandbox::decision().allows_webviews();
    }

    /// The window's mode is known. A window that hosts its panels (`hosts`) builds the views that waited; a hub
    /// keeps its empty places, for these panels and any registered later.
    pub(crate) fn settle_views(&mut self, hosts: bool) {
        if !self.holding || !hosts {
            return;
        }
        self.holding = false;
        let waiting: Vec<(ModuleId, String, String)> = self
            .entries()
            .iter()
            .map(|entry| (entry.module.clone(), entry.id.clone(), entry.url.clone()))
            .collect();
        for (module, id, url) in waiting {
            let guard = self.views.guard(&module).expect("registered panel guard");
            let webview = webkit6::WebView::new();
            webview.load_uri(&url);
            webview.set_hexpand(true);
            webview.set_vexpand(true);
            install_crash_recovery_into(&webview, id, url, None, guard);
            self.views.set_widget(&module, webview.upcast());
        }
    }
}

/// Whether a window builds the view of the panel `id` as it registers. `shows` is the one module of a chat or page
/// window, which builds no other module's page; `holding` is [`PanelRegistry`]'s. `init.lua` registers every panel in
/// every window, so the panel is still recorded, with an empty widget that no host takes.
fn builds_view(shows: Option<&ModuleId>, holding: bool, id: &str) -> bool {
    !holding && shows.is_none_or(|module| *module == ModuleId::lua(id))
}

pub(crate) fn install(
    lua: &Lua,
    eitri: &Table,
    registry: Rc<RefCell<PanelRegistry>>,
    config_dir: PathBuf,
    shows: Option<ModuleId>,
) -> mlua::Result<()> {
    let panel_table = lua.create_table()?;
    let register_fn = lua.create_function(move |_, spec: Table| {
        let parsed = parse_panel_spec(&spec)?;
        let resolved_url = resolve_panel_url(&config_dir, &parsed.url);
        // WebKit's sandbox cannot start in this process (`crate::webkit_sandbox`, decided once in
        // `main()`): no `WebView` is built -- WebKit would abort the whole process on it -- and the
        // panel is registered as usual with a notice in its place, so its key, position and title
        // still hold and `init.lua` needs no change. `main.rs`'s kill and revive paths downcast the
        // widget to a `WebView` and skip anything else.
        let holding = registry.borrow().holding;
        let (widget, crash_guard) = if !builds_view(shows.as_ref(), holding, &parsed.id) {
            (
                gtk4::Box::new(gtk4::Orientation::Vertical, 0).upcast(),
                Rc::new(RefCell::new(
                    crate::webview_crash_guard::WebViewCrashGuard::with_defaults(),
                )),
            )
        } else if crate::webkit_sandbox::decision().allows_webviews() {
            let webview = webkit6::WebView::new();
            webview.load_uri(&resolved_url);
            webview.set_hexpand(true);
            webview.set_vexpand(true);
            let crash_guard = install_crash_recovery(&webview, parsed.id.clone(), resolved_url.clone(), None);
            (webview.upcast(), crash_guard)
        } else {
            eprintln!(
                "[lua] panel '{}': not loading {resolved_url}: WebKit's sandbox cannot start here",
                parsed.id
            );
            (
                crate::webkit_sandbox::lua_panel_placeholder(&parsed.title),
                Rc::new(RefCell::new(
                    crate::webview_crash_guard::WebViewCrashGuard::with_defaults(),
                )),
            )
        };
        registry.borrow_mut().register(PanelEntry {
            id: parsed.id,
            title: parsed.title,
            slot: parsed.slot,
            key: parsed.key,
            url: resolved_url,
            widget,
            crash_guard,
        });
        Ok(())
    })?;
    panel_table.set("register", register_fn)?;
    eitri.set("panel", panel_table)?;
    Ok(())
}

/// Reload after a crash, but stop after repeated failures. Deliberate process termination is
/// ignored because reopening the module already reloads its page. The shared guard lets that
/// manual recovery reset the same crash count the signal handler checks.
///
/// The handler uses the signal's view argument to avoid a strong reference cycle through the
/// view's own signal storage. `give_up_base` stays inside a navigation-locked page's allowed origin.
pub(crate) fn install_crash_recovery(
    webview: &webkit6::WebView,
    id: String,
    url: String,
    give_up_base: Option<String>,
) -> Rc<RefCell<crate::webview_crash_guard::WebViewCrashGuard>> {
    let guard = Rc::new(RefCell::new(
        crate::webview_crash_guard::WebViewCrashGuard::with_defaults(),
    ));
    install_crash_recovery_into(webview, id, url, give_up_base, guard.clone());
    guard
}

/// [`install_crash_recovery`] with a guard made earlier: a view built after its panel was registered answers to
/// the guard its views already hold, so the revive path resets the one the signal asks.
pub(crate) fn install_crash_recovery_into(
    webview: &webkit6::WebView,
    id: String,
    url: String,
    give_up_base: Option<String>,
    guard: Rc<RefCell<crate::webview_crash_guard::WebViewCrashGuard>>,
) {
    webview.connect_web_process_terminated(move |webview, reason| {
        if reason == webkit6::WebProcessTerminationReason::TerminatedByApi {
            return;
        }
        use crate::webview_crash_guard::CrashResponse;
        // Taken before the `match`, so the guard's borrow is not held while the arms act.
        let response = guard.borrow_mut().on_crash();
        match response {
            CrashResponse::Reload => {
                eprintln!("[lua] panel '{id}': the web process terminated ({reason:?}); reloading it");
                webview.load_uri(&url);
            }
            CrashResponse::GiveUp => {
                eprintln!("[lua] panel '{id}': the web process kept terminating; giving up on automatic reload");
                let html = crate::webview_crash_guard::crash_message_html(
                    "Close and reopen this panel (prefix x, then its key) to try again.",
                );
                webview.load_html(&html, give_up_base.as_deref());
            }
            CrashResponse::AlreadyGivenUp => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chat_or_page_window_builds_no_other_panels_view() {
        assert!(
            builds_view(None, false, "notes"),
            "a window with no one module builds every panel"
        );
        let page = ModuleId::lua("notes");
        assert!(builds_view(Some(&page), false, "notes"));
        assert!(!builds_view(Some(&page), false, "other"));
        assert!(!builds_view(Some(&ModuleId::agent()), false, "notes"));

        let (_, code) = eitri_core::source_scan::function_in(
            &[eitri_core::source_scan::read_required("shell/src/lua/panel.rs")],
            "install",
        );
        let decided = code.find("builds_view(").expect("the panel's view is decided");
        let built = code.find("WebView::new(").expect("the panel's view is built");
        assert!(decided < built, "no view is built before it is decided");
    }

    #[test]
    fn a_window_that_may_be_a_hub_builds_no_panel_view_as_it_registers() {
        assert!(!builds_view(None, true, "notes"));
        // Nothing in this test builds a view: a hub leaves what waited alone, and stays holding.
        let mut registry = PanelRegistry {
            holding: true,
            ..PanelRegistry::default()
        };
        registry.settle_views(false);
        assert!(
            registry.holding,
            "a panel registered later builds no view in a hub either"
        );
    }

    #[test]
    fn a_view_built_after_its_panel_registered_finds_its_crash_guard() {
        let module = eitri_core::layout::ModuleId::lua("notes");
        let mut registry = eitri_core::lua::panel::PanelRegistry::default();
        let mut views = PanelViews::<&'static str>::default();
        let guard = Rc::new(RefCell::new(
            crate::webview_crash_guard::WebViewCrashGuard::with_defaults(),
        ));
        registry.register(eitri_core::lua::panel::PanelEntry {
            module: module.clone(),
            id: "notes".into(),
            title: "Notes".into(),
            slot: PanelSlot::Side,
            key: None,
            url: "file:///notes.html".into(),
        });
        views.insert_guard(module.clone(), guard.clone());
        views.set_widget(&module, "notes view");
        assert!(Rc::ptr_eq(&views.guard(&module).unwrap(), &guard));
        assert_eq!(views.widget(&module), Some(&"notes view"));
    }
    #[test]
    fn a_view_built_before_registration_keeps_its_widget_when_the_guard_arrives() {
        let module = eitri_core::layout::ModuleId::lua("notes");
        let mut views = PanelViews::<&'static str>::default();
        views.set_widget(&module, "notes view");
        let guard = Rc::new(RefCell::new(
            crate::webview_crash_guard::WebViewCrashGuard::with_defaults(),
        ));
        views.insert_guard(module.clone(), guard.clone());
        assert_eq!(views.widget(&module), Some(&"notes view"));
        assert!(Rc::ptr_eq(&views.guard(&module).unwrap(), &guard));
    }
}
