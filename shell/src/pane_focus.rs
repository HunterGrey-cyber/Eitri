//! Tracks keyboard focus and signals it through the editor cursor and panel sign cell.
//! Keyboard arrival in the panel enters BROWSE without a composer caret, so the
//! caret cannot identify the focused pane.
//!
//! The editor forwards focus through `set_focused` and sends nvim `FocusGained`
//! or `FocusLost`. It hides its cursor when unfocused using
//! `HIDE_UNFOCUSED_CURSOR_CMD`. The agent panel receives a `pane_focus` envelope
//! (`AgentPanelHandle::set_pane_focused`) and draws the current row's sign cell
//! solid with focus and hollow without. The hollow cell shows where returning
//! focus will land. There is no separate focus label in the status bar.
//!
//! One function updates these signals from GTK's `GtkWindow:focus-widget` and
//! window activity. A pane counts as focused for its cursor only when it holds
//! the focus widget and the window is active; bright BROWSE means keys go there now.
//! `notify::is-active` reruns the function when activity changes.
//! The last focused module is written to the layout's `focus` even while the
//! window is inactive. It is the destination for the top bar's `Ctrl+j` and the
//! module zoomed by the `Ctrl+a` prefix. `Layout::set_focus` refuses hidden modules.
//! This behavior has not been checked on screen.
//!
//! Panes are keyed by `ModuleId`, which identifies a module independently of its
//! position. Numeric indices cannot identify modules in a layout whose structure varies.

use gtk4::prelude::*;
use neovibe_core::layout::ModuleId;

/// The index of the first pane in `panes` whose element appears in `chain`, walking `chain` in
/// order (the focus widget first, then each ancestor). `None` means focus is in none of them: the
/// top bar's buttons, say, or no focus widget at all.
///
/// Generic so the decision can be unit-tested without a display. The GTK half only builds the
/// chain.
pub(crate) fn owning_pane<T: PartialEq>(chain: impl IntoIterator<Item = T>, panes: &[T]) -> Option<usize> {
    chain.into_iter().find_map(|w| panes.iter().position(|p| *p == w))
}

/// [`owning_pane`], answered with the module's id: the module whose host is nearest to the focus
/// widget on its way up to the window. Generic for the same reason.
pub(crate) fn owning_module<T: PartialEq>(
    chain: impl IntoIterator<Item = T>,
    modules: &[(ModuleId, T)],
) -> Option<ModuleId> {
    chain
        .into_iter()
        .find_map(|w| modules.iter().find(|(_, host)| *host == w))
        .map(|(id, _)| id.clone())
}

/// Which panes' "has the keys" answer changed, as `(index, now)`. `last` holds the previous
/// answer per pane (`None` before the first report), `focused` is the pane holding focus, and
/// `active` is whether the window is. Only changes are reported, so a focus move between two
/// widgets inside the editor sends nothing, and every pane gets one report on the first run.
pub(crate) fn focus_changes(last: &[Option<bool>], focused: Option<usize>, active: bool) -> Vec<(usize, bool)> {
    last.iter()
        .enumerate()
        .filter_map(|(i, prev)| {
            let now = focused == Some(i) && active;
            (*prev != Some(now)).then_some((i, now))
        })
        .collect()
}

/// Wires the tracker to `window` and applies it once for the current focus. `modules` are the
/// module hosts whose subtree counts as that module.
///
/// `on_owner(id)` is called whenever focus lands inside module `id`, whether or not the window is
/// active: that module is where the top bar's `Ctrl+j` goes back to and what the `Ctrl+a` prefix
/// zooms. `on_has_keys(id, has_keys)` is called when a module's "has the keys" answer changes.
pub(crate) fn install(
    window: &gtk4::ApplicationWindow,
    modules: Vec<(ModuleId, gtk4::Widget)>,
    on_owner: impl Fn(&ModuleId) + 'static,
    on_has_keys: impl Fn(&ModuleId, bool) + 'static,
) {
    let last: std::cell::RefCell<Vec<Option<bool>>> = std::cell::RefCell::new(vec![None; modules.len()]);
    let hosts: Vec<gtk4::Widget> = modules.iter().map(|(_, w)| w.clone()).collect();
    let apply = move |window: &gtk4::ApplicationWindow| {
        let chain = std::iter::successors(gtk4::prelude::GtkWindowExt::focus(window), |w| w.parent());
        let focused = owning_pane(chain, &hosts);
        if let Some(i) = focused {
            on_owner(&modules[i].0);
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
            focused.map(|i| modules[i].0.as_str()),
        );
        let changes = focus_changes(&last.borrow(), focused, window.is_active());
        for (i, now) in changes {
            last.borrow_mut()[i] = Some(now);
            on_has_keys(&modules[i].0, now);
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
        std::iter::successors(gtk4::prelude::GtkWindowExt::focus(window), |w| w.parent()),
        modules,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_focus_widget_itself_can_be_the_pane() {
        assert_eq!(owning_pane(["editor", "box", "window"], &["editor", "agent"]), Some(0));
    }

    #[test]
    fn a_widget_inside_a_pane_counts_as_that_pane() {
        assert_eq!(
            owning_pane(["textarea-ish", "agent", "overlay", "window"], &["editor", "agent"]),
            Some(1)
        );
    }

    #[test]
    fn focus_outside_every_pane_is_none() {
        assert_eq!(
            owning_pane(["reload-button", "topbar", "window"], &["editor", "agent"]),
            None
        );
        assert_eq!(owning_pane(Vec::<&str>::new(), &["editor", "agent"]), None);
    }

    #[test]
    fn the_nearest_ancestor_wins() {
        // A pane nested inside another pane (a Lua plugin could do this) belongs to the inner one:
        // that is the one whose keys are being typed into.
        assert_eq!(owning_pane(["inner", "outer", "window"], &["outer", "inner"]), Some(1));
    }

    #[test]
    fn every_pane_gets_one_first_report_then_only_changes() {
        assert_eq!(focus_changes(&[None, None], Some(0), true), vec![(0, true), (1, false)]);
        assert_eq!(focus_changes(&[Some(true), Some(false)], Some(0), true), vec![]);
        assert_eq!(
            focus_changes(&[Some(true), Some(false)], Some(1), true),
            vec![(0, false), (1, true)]
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
        // Alt-tab away: the editor stops drawing its cursor.
        assert_eq!(
            focus_changes(&[Some(true), Some(false)], Some(0), false),
            vec![(0, false)]
        );
        // Focus on the top bar: neither pane has the keys.
        assert_eq!(focus_changes(&[Some(false), Some(true)], None, true), vec![(1, false)]);
    }
}
