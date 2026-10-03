//! Registers plugin panels with `id`, `title`, `position`, and `content`.
//! `content` must be `{ type = "webview", url = "..." }` in v1. It is validated here
//! so invalid content fails before reaching GTK.
//!
//! The registry holds only Lua panels. Every window has editor and agent modules;
//! each Lua panel adds a module placed by its `position` (`PanelSlot::placement`).
//! `main.rs` turns this registry into `ModuleDecl`s in the shared layout.
//!
//! `PanelSlot`, `ParsedPanelSpec`, `parse_panel_spec`, and `resolve_panel_url` live
//! in the core Lua panel module and do not touch GTK or WebKit. This module keeps
//! `PanelEntry`, which holds a `gtk4::Widget`, and `install`, which builds a
//! `webkit6::WebView`.

use eitri_core::lua::panel::{parse_panel_spec, resolve_panel_url, PanelSlot};
use gtk4::prelude::*;
use mlua::{Lua, Table};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use webkit6::prelude::*;

/// One Lua panel. Generic over the widget only so the registry's rules are tested without a
/// display; the product always holds a `gtk4::Widget`.
pub(crate) struct PanelEntry<W = gtk4::Widget> {
    pub(crate) id: String,
    /// Its name on the tray chip and in the prefix strip.
    pub(crate) title: String,
    /// Where it goes the first time (`PanelSlot::placement`), nothing more.
    pub(crate) slot: PanelSlot,
    /// Its module key after `Ctrl+a`, as written. `main.rs` checks all panel keys together with
    /// `ModuleKeys::build` once `init.lua` has run.
    pub(crate) key: Option<String>,
    /// The page it loads, resolved against the config directory: what a `prefix x`'d panel loads
    /// afresh when its key reopens it (2026-09-26).
    pub(crate) url: String,
    pub(crate) widget: W,
    /// This panel's crash-loop guard. `main.rs`'s
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
    eitri: &Table,
    registry: Rc<RefCell<PanelRegistry>>,
    config_dir: PathBuf,
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
        let (widget, crash_guard) = if crate::webkit_sandbox::decision().allows_webviews() {
            let webview = webkit6::WebView::new();
            webview.load_uri(&resolved_url);
            webview.set_hexpand(true);
            webview.set_vexpand(true);
            let crash_guard = install_crash_recovery(&webview, parsed.id.clone(), resolved_url.clone());
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

/// Reloads a Lua panel after its web process crashes, using `WebViewCrashGuard` to limit crash
/// loops. Skips `TerminatedByApi`: `kill_pane` deliberately terminates the process for
/// `prefix x`, and showing the module again already reloads it. `id` is only for logging.
///
/// Returns the shared guard so the caller can reset it when reviving the panel on its next
/// show.
///
/// The signal closure uses the callback's `WebView` argument rather than capturing an owned
/// clone. Capturing a clone would create a strong reference cycle through the view's
/// signal-handler storage, preventing a replaced panel from being freed.
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
        // Taken before the `match`, so the guard's borrow is not held while the arms act.
        let response = guard_for_signal.borrow_mut().on_crash();
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
