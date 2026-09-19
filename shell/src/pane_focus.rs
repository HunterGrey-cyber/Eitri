//! Which pane has keyboard focus, and the three places that say so.
//!
//! Before this existed nothing in the window said which pane was focused. The composer's caret
//! used to do it by accident: `Ctrl+l` put focus in the panel's textarea, and a blinking caret
//! appeared there. The panel-as-document rework (`9dd39f2`) lands focus in BROWSE instead, with no
//! caret, so that signal went away. The owner hit this on an installed build: "切换到右边之后没有提示
//! ... 我没法确定我在哪个pane". The three-mode keyboard model is kept. This module puts a signal back.
//!
//! **One source, three indicators.** GTK's own focus widget (`GtkWindow:focus-widget`) is the only
//! input. Every change to it recomputes which pane holds it and updates all three at once:
//!
//! 1. the `pane-focused` CSS class on that pane's frame, which `theme::gtk_css` draws as a line along the pane's bottom edge;
//! 2. the status bar's focus label (`chrome::build_status_bar`);
//! 3. the agent panel's mode block, via a `pane_focus` envelope
//!    (`AgentPanelHandle::set_pane_focused`), which dims the block when the panel is not focused.
//!
//! A class toggled from here, not GTK's `:focus-within`. `:focus-within` would have been one CSS
//! rule, but then the focus line would be decided by GTK's state flags while the status label and
//! the panel were decided by this code. Two mechanisms can disagree, and on a screen nobody could
//! tell which one was wrong. With one input and one function, all three agree by construction.
//! The cost is a `notify::focus-widget` handler.
//!
//! **What "focused" means here:** the pane holding the window's focus widget. The focus line and the
//! status bar label do not track whether the window itself is active: after alt-tabbing away they
//! still name the pane that will get the keys on return, and they do not dim.
//!
//! **The panel's mode block does, since 2026-09-19 (later).** Its own doc (`StatusLine.tsx`) says a
//! bright BROWSE is a claim that keys typed NOW go there, and that claim is false while another
//! window is active -- a review finding, since this paragraph used to say nothing dims at all and
//! the two docs contradicted each other. So `on_side_focus` gets `true` only when the side pane
//! holds the focus widget AND the window is active, and `notify::is-active` re-runs the same
//! function. **Not looked at on a screen**: whether `is-active` notifies promptly on this
//! compositor after an alt-tab was not observed.

use gtk4::prelude::*;

/// One pane the tracker knows about.
pub(crate) struct Pane {
    /// Focus inside this widget, or on it, counts as this pane having focus.
    pub(crate) content: gtk4::Widget,
    /// The widget that gets the `pane-focused` class and draws the focus line. It differs from
    /// `content` for the side slot: the WebView sits inside the resize-throttle `Overlay`
    /// (`layout::install_webview_resize_throttle`) and may be sized smaller than its slot mid-drag.
    /// The focus line belongs to the slot.
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
/// `on_side_focus` gets `true` when the pane at `side_index` is the focused one and the window is
/// active, and `false` otherwise. It is called only when that answer changes, so a focus move between two widgets
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
        let side = focused == Some(side_index) && window.is_active();
        if last_side.get() != Some(side) {
            last_side.set(Some(side));
            on_side_focus(side);
        }
    };
    apply(window);
    let apply = std::rc::Rc::new(apply);
    let on_active = apply.clone();
    window.connect_notify_local(Some("focus-widget"), move |window, _| apply(window));
    window.connect_notify_local(Some("is-active"), move |window, _| on_active(window));
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
