//! Tracks keyboard focus and updates the pane indicators from GTK's focus widget.
//!
//! `Ctrl+l` enters the panel in BROWSE without a textarea caret. Cursor indicators make the
//! focused pane visible while keeping the three-mode keyboard model.
//!
//! The editor forwards focus through `NeovideEditorPane::set_focused`, which also sends nvim
//! `FocusGained`/`FocusLost`. `neovide_editor::HIDE_UNFOCUSED_CURSOR_CMD` hides the editor cursor
//! when unfocused. The panel receives a `pane_focus` envelope through
//! `AgentPanelHandle::set_pane_focused`. Its current row's sign cell is solid with focus and
//! hollow without, so the panel still shows where focus will return.
//! The cursors provide the focus indicators.
//!
//! Changes to `GtkWindow:focus-widget` update all indicators through one function so they agree.
//! A pane has the keys only when it holds the focus widget and the window is active.
//! `notify::is-active` runs the same function, so switching to another application clears that state.
//! The last focused pane is remembered even while the window is inactive. The top bar's
//! `Ctrl+j` returns to it, and the `Ctrl+a` prefix zooms it.
//! Visual behavior has not been checked on screen.

use gtk4::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

/// The index of the first pane in `panes` whose element appears in `chain`, walking `chain` in
/// order (the focus widget first, then each ancestor). `None` means focus is in none of them: the
/// top bar's buttons, say, or no focus widget at all.
///
/// Generic so the decision can be unit-tested without a display. The GTK half only builds the
/// chain.
pub(crate) fn owning_pane<T: PartialEq>(chain: impl IntoIterator<Item = T>, panes: &[T]) -> Option<usize> {
    chain.into_iter().find_map(|w| panes.iter().position(|p| *p == w))
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

/// Wires the tracker to `window` and applies it once for the current focus. `panes` are the
/// widgets whose subtree counts as that pane, in the order `on_pane_focus` reports indices.
///
/// `on_pane_focus(index, has_keys)` is called when a pane's answer changes. Returns the index of
/// the pane that most recently held focus, which the top bar's `Ctrl+j` uses to go back to it.
pub(crate) fn install(
    window: &gtk4::ApplicationWindow,
    panes: Vec<gtk4::Widget>,
    on_pane_focus: impl Fn(usize, bool) + 'static,
) -> Rc<Cell<usize>> {
    let last_pane = Rc::new(Cell::new(0));
    let last: std::cell::RefCell<Vec<Option<bool>>> = std::cell::RefCell::new(vec![None; panes.len()]);
    let remembered = last_pane.clone();
    let apply = move |window: &gtk4::ApplicationWindow| {
        let chain = std::iter::successors(gtk4::prelude::GtkWindowExt::focus(window), |w| w.parent());
        let focused = owning_pane(chain, &panes);
        if let Some(i) = focused {
            remembered.set(i);
        }
        // Said on every focus change, not only on a pane's own transition, because the two states
        // that look identical from the per-pane callback are exactly the two a "I cannot type"
        // report has to tell apart (2026-09-20, after one such report this log could not explain):
        // the window went inactive -- ordinary, the keys are in another application -- or the
        // window is STILL ACTIVE and focus is sitting on a widget that is neither pane, in which
        // case the keys are going nowhere and that is a bug. `has_keys` is false either way.
        // The widget's type name is what names the culprit: the top bar after `Ctrl+k`, HINT's
        // overlay, or `None` after a control removed itself.
        println!(
            "[pane_focus] active={} focus={} owning_pane={:?}",
            window.is_active(),
            gtk4::prelude::GtkWindowExt::focus(window)
                .map(|w| w.type_().name().to_string())
                .unwrap_or_else(|| "none".to_string()),
            focused,
        );
        let changes = focus_changes(&last.borrow(), focused, window.is_active());
        for (i, now) in changes {
            last.borrow_mut()[i] = Some(now);
            on_pane_focus(i, now);
        }
    };
    apply(window);
    let apply = Rc::new(apply);
    let on_active = apply.clone();
    window.connect_notify_local(Some("focus-widget"), move |window, _| apply(window));
    window.connect_notify_local(Some("is-active"), move |window, _| on_active(window));
    last_pane
}

/// The pane holding the window's focus widget, or `None` for the top bar or no focus widget.
/// This is where a key typed now would go, rather than the last focused pane. `Ctrl+a Ctrl+a`
/// forwards its `Ctrl+a` here.
pub(crate) fn focused_pane(window: &gtk4::ApplicationWindow, panes: &[gtk4::Widget]) -> Option<usize> {
    owning_pane(
        std::iter::successors(gtk4::prelude::GtkWindowExt::focus(window), |w| w.parent()),
        panes,
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
