//! Tracks keyboard focus in the companion window. The panel's current row has a solid sign cell
//! while it receives keys and a hollow one otherwise. The companion has no window controller,
//! so its tracker lives here; the main window uses the controller's focus tracking.
//!
//! GTK's focus widget identifies the focused module. Cursors are solid only when that module
//! contains the focus widget and the window is active, so switching windows makes them hollow.
//! Changes to the focus widget and `notify::is-active` run the same update function.
//! The focus callback reports the module holding the focus widget even when the window is inactive.
//!
//! Tracking uses `ModuleId` because positions in a dynamic layout do not identify modules.
//! On every focus change, `install` reads `ModuleGrid::live_hosts` and keeps each module's
//! last focus state by id. Modules added after startup are tracked from their first focus change.

use std::collections::BTreeMap;

use eitri_core::layout::ModuleId;
use gtk4::prelude::*;

use crate::module_grid::ModuleHosts;

/// Returns the module whose host is nearest to the focus widget in `chain`, which starts at
/// the focus widget and continues through its ancestors. Returns `None` for focus outside all
/// module hosts, such as the top bar, or when there is no focus widget.
///
/// Generic so the lookup can be tested without a display. The GTK caller only builds the chain.
pub(crate) fn owning_module<T: PartialEq>(
    chain: impl IntoIterator<Item = T>,
    modules: &[(ModuleId, T)],
) -> Option<ModuleId> {
    chain
        .into_iter()
        .find_map(|w| modules.iter().find(|(_, host)| *host == w))
        .map(|(id, _)| id.clone())
}

/// Which modules' "has the keys" answer changed, as `(id, now)`, in `modules`' order. `last` holds
/// each module's previous answer (none before its first report), `focused` is the module holding
/// focus, and `active` is whether the window is. Only changes are reported, so a focus move between
/// two widgets inside the editor sends nothing, and every module gets one report the first time it
/// is seen -- at startup, or whenever it joined the grid.
pub(crate) fn focus_changes(
    last: &BTreeMap<ModuleId, bool>,
    modules: &[ModuleId],
    focused: Option<&ModuleId>,
    active: bool,
) -> Vec<(ModuleId, bool)> {
    modules
        .iter()
        .filter_map(|id| {
            let now = focused == Some(id) && active;
            (last.get(id) != Some(&now)).then(|| (id.clone(), now))
        })
        .collect()
}

/// Wires the tracker to `window` and applies it once for the current focus. `hosts` answers, each
/// time, with every module and the host whose subtree counts as that module.
///
/// `on_owner(id)` is called whenever focus lands inside module `id`, whether or not the window is
/// active: that module is where the top bar's `Ctrl+j` goes back to and what the `Ctrl+a` prefix
/// zooms. `on_has_keys(id, has_keys)` is called when a module's "has the keys" answer changes.
pub(crate) fn install(
    window: &gtk4::ApplicationWindow,
    hosts: ModuleHosts,
    on_owner: impl Fn(&ModuleId) + 'static,
    on_has_keys: impl Fn(&ModuleId, bool) + 'static,
) {
    let last: std::cell::RefCell<BTreeMap<ModuleId, bool>> = std::cell::RefCell::new(BTreeMap::new());
    let apply = move |window: &gtk4::ApplicationWindow| {
        // Read the module list on every focus change so modules added after `present()` are found.
        // Reading it once before this closure would miss them. Headless tests do not catch that change;
        // focus tracking, `Ctrl+a Ctrl+a`, and `Ctrl+h/j/k/l` need checking with a module added at runtime.
        let modules = hosts();
        let chain = std::iter::successors(
            gtk4::prelude::GtkWindowExt::focus(window),
            gtk4::prelude::WidgetExt::parent,
        );
        let focused = owning_module(chain, &modules);
        if let Some(id) = &focused {
            on_owner(id);
        }
        // Said on every focus change, not only on a module's own transition, because the two states
        // that look identical from the per-module callback are exactly the two a "I cannot type"
        // report has to tell apart (2026-09-20, after one such report this log could not explain):
        // the window went inactive -- ordinary, the keys are in another application -- or the
        // window is STILL ACTIVE and focus is sitting on a widget that is in no module, in which
        // case the keys are going nowhere and that is a bug. `has_keys` is false either way.
        // The widget's type name is what names the culprit: the top bar after `Ctrl+k`, HINT's
        // overlay, or `None` after a control removed itself.
        println!(
            "[pane_focus] active={} focus={} owning_module={:?}",
            window.is_active(),
            gtk4::prelude::GtkWindowExt::focus(window)
                .map(|w| w.type_().name().to_string())
                .unwrap_or_else(|| "none".to_string()),
            focused.as_ref().map(ModuleId::as_str),
        );
        let ids: Vec<ModuleId> = modules.into_iter().map(|(id, _)| id).collect();
        let changes = focus_changes(&last.borrow(), &ids, focused.as_ref(), window.is_active());
        for (id, now) in changes {
            last.borrow_mut().insert(id.clone(), now);
            on_has_keys(&id, now);
        }
    };
    apply(window);
    let apply = std::rc::Rc::new(apply);
    let on_active = apply.clone();
    window.connect_notify_local(Some("focus-widget"), move |window, _| apply(window));
    window.connect_notify_local(Some("is-active"), move |window, _| on_active(window));
}

/// The module holding the window's focus widget, or `None` for the top bar or no focus widget.
/// This is where a key typed now would go, rather than the last focused module. `Ctrl+a Ctrl+a`
/// forwards its `Ctrl+a` here.
pub(crate) fn focused_module(
    window: &gtk4::ApplicationWindow,
    modules: &[(ModuleId, gtk4::Widget)],
) -> Option<ModuleId> {
    owning_module(
        std::iter::successors(
            gtk4::prelude::GtkWindowExt::focus(window),
            gtk4::prelude::WidgetExt::parent,
        ),
        modules,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last(answers: &[(ModuleId, bool)]) -> BTreeMap<ModuleId, bool> {
        answers.iter().cloned().collect()
    }

    #[test]
    fn every_module_gets_one_first_report_then_only_changes() {
        let (e, a) = (ModuleId::editor(), ModuleId::agent());
        let both = [e.clone(), a.clone()];
        assert_eq!(
            focus_changes(&last(&[]), &both, Some(&e), true),
            vec![(e.clone(), true), (a.clone(), false)]
        );
        assert_eq!(
            focus_changes(&last(&[(e.clone(), true), (a.clone(), false)]), &both, Some(&e), true),
            vec![]
        );
        assert_eq!(
            focus_changes(&last(&[(e.clone(), true), (a.clone(), false)]), &both, Some(&a), true),
            vec![(e, false), (a, true)]
        );
    }

    /// A module added after `install` receives its first report on the next focus change that
    /// sees it. Known modules whose focus state has not changed report nothing new.
    #[test]
    fn a_module_added_later_gets_its_first_report_and_nothing_else_moves() {
        let (e, a, canvas) = (
            ModuleId::editor(),
            ModuleId::agent(),
            ModuleId::parse("canvas").unwrap(),
        );
        let known = last(&[(e.clone(), true), (a.clone(), false)]);
        assert_eq!(
            focus_changes(&known, &[e.clone(), a.clone(), canvas.clone()], Some(&e), true),
            vec![(canvas, false)]
        );
    }

    #[test]
    fn the_owning_module_is_named_by_its_id_not_its_position() {
        let modules = [
            (ModuleId::agent(), "agent-host"),
            (ModuleId::editor(), "editor-host"),
            (ModuleId::lua("notes"), "notes-host"),
        ];
        assert_eq!(
            owning_module(["editor-host", "grid", "window"], &modules),
            Some(ModuleId::editor())
        );
        assert_eq!(
            owning_module(["textarea", "notes-host", "grid"], &modules),
            Some(ModuleId::lua("notes"))
        );
        assert_eq!(owning_module(["reload-button", "topbar"], &modules), None);
        assert_eq!(
            owning_module(Vec::<&str>::new(), &modules),
            None,
            "no focus widget at all"
        );
    }

    #[test]
    fn a_module_nested_in_another_module_owns_its_own_focus() {
        let modules = [(ModuleId::agent(), "outer"), (ModuleId::lua("inner"), "inner")];
        assert_eq!(
            owning_module(["inner", "outer", "window"], &modules),
            Some(ModuleId::lua("inner"))
        );
    }

    #[test]
    fn an_inactive_window_gives_no_pane_the_keys() {
        let (e, a) = (ModuleId::editor(), ModuleId::agent());
        let both = [e.clone(), a.clone()];
        // Alt-tab away: the editor stops drawing its cursor.
        assert_eq!(
            focus_changes(&last(&[(e.clone(), true), (a.clone(), false)]), &both, Some(&e), false),
            vec![(e.clone(), false)]
        );
        // Focus on the top bar: neither module has the keys.
        assert_eq!(
            focus_changes(&last(&[(e, false), (a.clone(), true)]), &both, None, true),
            vec![(a, false)]
        );
    }
}
