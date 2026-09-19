//! Tracks keyboard focus and updates the pane indicators from GTK's focus widget.
//!
//! `Ctrl+l` enters the panel in BROWSE without a textarea caret. Cursor indicators make the
//! focused pane visible while keeping the three-mode keyboard model.
//!
//! The editor forwards focus through `NeovideEditorPane::set_focused`, which also sends nvim
//! `FocusGained`/`FocusLost`. Its cursor becomes hollow when unfocused. The panel receives a
//! `pane_focus` envelope through `AgentPanelHandle::set_pane_focused` and draws its current
//! row's sign cell solid with focus and hollow without. The hollow cursor marks the return position.
//!
//! Changes to `GtkWindow:focus-widget` update all indicators through one function so they agree.
//! A pane has the keys only when it holds the focus widget and the window is active.
//! `notify::is-active` runs the same function, so switching to another application clears that state.
//! The status bar also names the focused pane. Its label ignores window activity and keeps
//! naming the pane that will receive keys on return.
//! Visual behavior has not been checked on screen.

use gtk4::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

/// One pane the tracker knows about.
pub(crate) struct Pane {
    /// Focus inside this widget, or on it, counts as this pane having focus.
    pub(crate) content: gtk4::Widget,
    /// What the status bar shows. It is the panel registry's own title, so a Lua plugin that
    /// replaces a slot is named by its own title and not by the built-in one.
    pub(crate) title: String,
}

/// The index of the first pane in `panes` whose element appears in `chain`, walking `chain` in
/// order (the focus widget first, then each ancestor). `None` means focus is in none of them: the
/// top bar's buttons, say, or no focus widget at all.
///
/// Generic so the decision can be unit-tested without a display. The GTK half only builds the
/// chain.
pub(crate) fn owning_pane<T: PartialEq>(chain: impl IntoIterator<Item = T>, panes: &[T]) -> Option<usize> {
    chain.into_iter().find_map(|w| panes.iter().position(|p| *p == w))
}

/// What the status bar says for `focused`. With no pane focused it says so ("—"). It does not
/// keep the last pane's name up, which would be the same plausible-looking lie as the hardcoded
/// `NORMAL` it replaced.
pub(crate) fn status_text(focused: Option<&str>) -> String {
    focused.unwrap_or("\u{2014}").to_string()
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

/// Wires the tracker to `window` and applies it once for the current focus.
///
/// `on_pane_focus(index, has_keys)` is called when a pane's answer changes. Returns the index of
/// the pane that most recently held focus, which the top bar's `Ctrl+j` uses to go back to it.
pub(crate) fn install(
    window: &gtk4::ApplicationWindow,
    panes: Vec<Pane>,
    status_label: gtk4::Label,
    on_pane_focus: impl Fn(usize, bool) + 'static,
) -> Rc<Cell<usize>> {
    let last_pane = Rc::new(Cell::new(0));
    let last: std::cell::RefCell<Vec<Option<bool>>> = std::cell::RefCell::new(vec![None; panes.len()]);
    let remembered = last_pane.clone();
    let apply = move |window: &gtk4::ApplicationWindow| {
        let chain = std::iter::successors(gtk4::prelude::GtkWindowExt::focus(window), |w| w.parent());
        let contents: Vec<gtk4::Widget> = panes.iter().map(|p| p.content.clone()).collect();
        let focused = owning_pane(chain, &contents);
        if let Some(i) = focused {
            remembered.set(i);
        }
        status_label.set_text(&status_text(focused.map(|i| panes[i].title.as_str())));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_focus_widget_itself_can_be_the_pane() {
        assert_eq!(owning_pane(["editor", "box", "window"], &["editor", "agent"]), Some(0));
    }

    #[test]
    fn a_widget_inside_a_pane_counts_as_that_pane() {
        assert_eq!(owning_pane(["textarea-ish", "agent", "overlay", "window"], &["editor", "agent"]), Some(1));
    }

    #[test]
    fn focus_outside_every_pane_is_none() {
        assert_eq!(owning_pane(["reload-button", "topbar", "window"], &["editor", "agent"]), None);
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
        assert_eq!(focus_changes(&[Some(true), Some(false)], Some(1), true), vec![(0, false), (1, true)]);
    }

    #[test]
    fn an_inactive_window_gives_no_pane_the_keys() {
        // Alt-tab away: the editor's cursor goes hollow, as a real Neovide window's does.
        assert_eq!(focus_changes(&[Some(true), Some(false)], Some(0), false), vec![(0, false)]);
        // Focus on the top bar: neither pane has the keys.
        assert_eq!(focus_changes(&[Some(false), Some(true)], None, true), vec![(1, false)]);
    }

    #[test]
    fn the_status_bar_says_nothing_is_focused_rather_than_keep_a_stale_name() {
        assert_eq!(status_text(Some("Editor")), "Editor");
        assert_eq!(status_text(None), "\u{2014}");
    }
}
