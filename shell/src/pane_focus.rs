//! Tracks keyboard focus and updates the three pane indicators from GTK's focus widget.
//!
//! `Ctrl+l` enters the panel in BROWSE without a textarea caret. These indicators make the
//! focused pane visible while keeping the three-mode keyboard model.
//!
//! Each change to `GtkWindow:focus-widget` updates the `pane-focused` class on the pane's frame,
//! the status bar's focus label, and the panel's mode block through a `pane_focus` envelope
//! (`AgentPanelHandle::set_pane_focused`). `theme::gtk_css` draws the class as an outline;
//! the panel dims its mode block when unfocused.
//!
//! Toggling the class here instead of using GTK's `:focus-within` keeps the outline, status label,
//! and panel on one input and one function. Separate mechanisms could disagree.
//! This requires a `notify::focus-widget` handler.
//!
//! Focus here means holding the window's focus widget, regardless of window activity.
//! After switching to another application, the indicators keep naming the pane that will receive
//! keys on return. They do not dim merely because the window is inactive.

use gtk4::prelude::*;

/// One pane the tracker knows about.
pub(crate) struct Pane {
    /// Focus inside this widget, or on it, counts as this pane having focus.
    pub(crate) content: gtk4::Widget,
    /// The widget that gets the `pane-focused` class and draws the outline. It differs from
    /// `content` for the side slot: the WebView sits inside the resize-throttle `Overlay`
    /// (`layout::install_webview_resize_throttle`) and may be sized smaller than its slot mid-drag.
    /// The outline belongs to the slot.
    pub(crate) frame: gtk4::Widget,
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

/// Wires the tracker to `window` and applies it once for the current focus.
///
/// `on_side_focus` gets `true` when the pane at `side_index` is the focused one and `false`
/// otherwise. It is called only when that answer changes, so a focus move between two widgets
/// inside the editor does not send a WebView dispatch.
pub(crate) fn install(
    window: &gtk4::ApplicationWindow,
    panes: Vec<Pane>,
    status_label: gtk4::Label,
    side_index: usize,
    on_side_focus: impl Fn(bool) + 'static,
) {
    for pane in &panes {
        pane.frame.add_css_class("pane");
    }
    let last_side: std::cell::Cell<Option<bool>> = std::cell::Cell::new(None);
    let apply = move |window: &gtk4::ApplicationWindow| {
        let chain = std::iter::successors(gtk4::prelude::GtkWindowExt::focus(window), |w| w.parent());
        let contents: Vec<gtk4::Widget> = panes.iter().map(|p| p.content.clone()).collect();
        let focused = owning_pane(chain, &contents);
        for (i, pane) in panes.iter().enumerate() {
            if Some(i) == focused {
                pane.frame.add_css_class("pane-focused");
            } else {
                pane.frame.remove_css_class("pane-focused");
            }
        }
        status_label.set_text(&status_text(focused.map(|i| panes[i].title.as_str())));
        let side = focused == Some(side_index);
        if last_side.get() != Some(side) {
            last_side.set(Some(side));
            on_side_focus(side);
        }
    };
    apply(window);
    window.connect_notify_local(Some("focus-widget"), move |window, _| apply(window));
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
    fn the_status_bar_says_nothing_is_focused_rather_than_keep_a_stale_name() {
        assert_eq!(status_text(Some("Editor")), "Editor");
        assert_eq!(status_text(None), "\u{2014}");
    }
}
