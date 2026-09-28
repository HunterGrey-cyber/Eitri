//! `neovibe.panel.register({ id, title, position, content })` -- registers a plugin panel.
//! `content` must be `{ type = "webview", url = "..." }` in v1 (see the spec's "Panel content
//! model" section); this is validated here, not left to fail confusingly deep in GTK code.
//!
//! **Since the modules design's P1 the registry holds Lua panels only**
//! (docs/superpowers/specs/2026-09-23-modules-and-canvas-design.md, decision 1). The editor and the
//! agent used to be registered here too, into slots a Lua panel could take from them; now they are
//! modules every window has, and a Lua panel is one more module placed by its `position`
//! (`PanelSlot::placement`). "Built-in and plugin panels share one path" became "every module is a
//! leaf of one layout" -- `main.rs` turns this registry into `ModuleDecl`s.
//!
//! The pure parts -- `PanelSlot`, `ParsedPanelSpec`, `parse_panel_spec`, `resolve_panel_url` --
//! moved to `neovibe_core::lua::panel` (L2 T4): no GTK/WebKit touched there. What stays here is
//! only what actually needs a display: `PanelEntry` (holds a `gtk4::Widget`) and `install`
//! (builds a real `webkit6::WebView`).

use gtk4::prelude::*;
use mlua::{Lua, Table};
use neovibe_core::lua::panel::{parse_panel_spec, resolve_panel_url, PanelSlot};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use webkit6::prelude::*;

/// One Lua panel. Generic over the widget only so the registry's rules are tested without a
/// display; the product always holds a `gtk4::Widget`.
pub(crate) struct PanelEntry<W = gtk4::Widget> {
    pub(crate) id: String,
    /// Its tray chip's and the prefix strip's name for it (modules P2).
    pub(crate) title: String,
    /// Where it goes the first time (`PanelSlot::placement`), nothing more.
    pub(crate) slot: PanelSlot,
    /// Its module key after `Ctrl+a`, as written (modules P2). `main.rs` checks every panel's
    /// together (`ModuleKeys::build`) once `init.lua` has run.
    pub(crate) key: Option<String>,
    /// The page it loads, resolved against the config directory: what a `prefix x`'d panel loads
    /// afresh when its key reopens it (2026-09-26).
    pub(crate) url: String,
    pub(crate) widget: W,
    /// This panel's crash-loop guard (fix round 1, v1 hardening review, R1-5). `main.rs`'s
    /// revive-on-next-show path (`prefix x`, then this panel's own key) resets it right where it
    /// reruns `load_uri` -- the manual recovery the panel's own give-up message tells the user to
    /// try -- the same way `AgentPanelHandle::reload_document_by_hand` resets the chat's own guard.
    /// Without that, a guard that already gave up stays terminal forever (see
    /// `WebViewCrashGuard::reset`'s own doc), so a LATER, unrelated crash after a successful manual
    /// recovery would be silently swallowed with no reload and no message. `Rc<RefCell<_>>` because
    /// `install_crash_recovery`'s own closure, connected on the `WebView`, holds the other handle to
    /// the same guard.
    pub(crate) crash_guard: Rc<RefCell<crate::webview_crash_guard::WebViewCrashGuard>>,
}

/// The Lua panels, in registration order -- the order `Layout::initial` places them in.
pub(crate) struct PanelRegistry<W = gtk4::Widget> {
    entries: Vec<PanelEntry<W>>,
}

impl<W> Default for PanelRegistry<W> {
    fn default() -> Self {
        PanelRegistry { entries: Vec::new() }
    }
}

impl<W> PanelRegistry<W> {
    /// A second registration with the same `id` replaces the first, as a second registration to
    /// the same slot always did; it goes last, where it was registered. Two panels with different
    /// ids and the same `position` are both placed now -- one of them no longer disappears.
    pub(crate) fn register(&mut self, entry: PanelEntry<W>) {
        if let Some(i) = self.entries.iter().position(|e| e.id == entry.id) {
            eprintln!(
                "[lua] panel '{}' registered again -- replaced by the new registration",
                entry.id
            );
            self.entries.remove(i);
        }
        self.entries.push(entry);
    }

    pub(crate) fn entries(&self) -> &[PanelEntry<W>] {
        &self.entries
    }
}

pub(crate) fn install(
    lua: &Lua,
    neovibe: &Table,
    registry: Rc<RefCell<PanelRegistry>>,
    config_dir: PathBuf,
) -> mlua::Result<()> {
    let panel_table = lua.create_table()?;
    let register_fn = lua.create_function(move |_, spec: Table| {
        let parsed = parse_panel_spec(&spec)?;
        let resolved_url = resolve_panel_url(&config_dir, &parsed.url);
        let webview = webkit6::WebView::new();
        webview.load_uri(&resolved_url);
        webview.set_hexpand(true);
        webview.set_vexpand(true);
        let crash_guard = install_crash_recovery(&webview, parsed.id.clone(), resolved_url.clone());
        registry.borrow_mut().register(PanelEntry {
            id: parsed.id,
            title: parsed.title,
            slot: parsed.slot,
            key: parsed.key,
            url: resolved_url,
            widget: webview.upcast(),
            crash_guard,
        });
        Ok(())
    })?;
    panel_table.set("register", register_fn)?;
    neovibe.set("panel", panel_table)?;
    Ok(())
}

/// R1-5 (v1 hardening review): a Lua panel's `WebView` had no `web-process-terminated` handling at
/// all, so a crashed web process left the panel blank until the user found `prefix x` then the
/// panel's own key by hand (the only thing that reruns `load_uri` -- `main.rs`'s revive-on-next-show
/// path). Reload it automatically instead, guarded against a crash loop the same way
/// `shell::agent_panel` guards the chat's own `WebView` (`crate::webview_crash_guard`); see that
/// module's own doc for why `TerminatedByApi` is skipped before the guard is ever asked -- it is
/// `main.rs`'s `kill_pane` calling `terminate_web_process()` on a deliberate `prefix x`, which
/// already reloads this same `WebView` when the module is shown again and must not also trigger
/// this automatic path. `id` is only for the log line.
///
/// Returns the guard so `main.rs`'s revive-on-next-show path can reset it (`PanelEntry::crash_guard`'s
/// own doc) -- the caller owns the other handle to the same `Rc<RefCell<_>>`.
///
/// **The closure below takes its `WebView` from the signal callback's own first argument, and never
/// captures an owned clone of `webview` itself** (fix round 1, an independent reviewer's finding): a
/// `webview.clone()` captured inside a closure that `connect_web_process_terminated` attaches to that
/// SAME `webview` is a reference cycle -- the `WebView`'s own signal-handler storage would then hold
/// a strong reference back to itself -- so a panel replaced by a second `register()` call for the
/// same id (`PanelRegistry::register`'s own doc) could never be freed even once nothing else in the
/// registry or the widget tree still points at it. The signal's own callback argument is exactly the
/// same `WebView`, valid for the call, so nothing is lost by using it instead.
fn install_crash_recovery(
    webview: &webkit6::WebView,
    id: String,
    url: String,
) -> Rc<RefCell<crate::webview_crash_guard::WebViewCrashGuard>> {
    let guard = Rc::new(RefCell::new(
        crate::webview_crash_guard::WebViewCrashGuard::with_defaults(),
    ));
    let guard_for_signal = guard.clone();
    webview.connect_web_process_terminated(move |webview, reason| {
        if reason == webkit6::WebProcessTerminationReason::TerminatedByApi {
            return;
        }
        use crate::webview_crash_guard::CrashResponse;
        match guard_for_signal.borrow_mut().on_crash() {
            CrashResponse::Reload => {
                eprintln!("[lua] panel '{id}': the web process terminated ({reason:?}); reloading it");
                webview.load_uri(&url);
            }
            CrashResponse::GiveUp => {
                eprintln!("[lua] panel '{id}': the web process kept terminating; giving up on automatic reload");
                let html = crate::webview_crash_guard::crash_message_html(
                    "Close and reopen this panel (prefix x, then its key) to try again.",
                );
                webview.load_html(&html, None);
            }
            CrashResponse::AlreadyGivenUp => {}
        }
    });
    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, slot: PanelSlot, widget: &'static str) -> PanelEntry<&'static str> {
        PanelEntry {
            id: id.to_string(),
            title: id.to_string(),
            slot,
            key: None,
            url: format!("file:///{id}.html"),
            widget,
            // `WebViewCrashGuard` is GTK-free (`shell::webview_crash_guard`'s own doc), so these
            // registry-only tests -- which deliberately never touch a real `WebView` -- can still
            // construct a real one rather than needing an `Option`.
            crash_guard: Rc::new(RefCell::new(
                crate::webview_crash_guard::WebViewCrashGuard::with_defaults(),
            )),
        }
    }

    #[test]
    fn panels_keep_their_registration_order_and_two_in_one_position_both_stay() {
        let mut registry = PanelRegistry::default();
        registry.register(entry("a", PanelSlot::Bottom, "a1"));
        registry.register(entry("b", PanelSlot::Bottom, "b1"));
        registry.register(entry("c", PanelSlot::Side, "c1"));
        let ids: Vec<&str> = registry.entries().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn registering_an_id_again_replaces_it_and_moves_it_last() {
        let mut registry = PanelRegistry::default();
        registry.register(entry("a", PanelSlot::Bottom, "a1"));
        registry.register(entry("b", PanelSlot::Side, "b1"));
        registry.register(entry("a", PanelSlot::Main, "a2"));
        let seen: Vec<(&str, PanelSlot, &str)> = registry
            .entries()
            .iter()
            .map(|e| (e.id.as_str(), e.slot, e.widget))
            .collect();
        assert_eq!(seen, [("b", PanelSlot::Side, "b1"), ("a", PanelSlot::Main, "a2")]);
    }
}
