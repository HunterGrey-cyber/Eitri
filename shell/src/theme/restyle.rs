//! Making a stylesheet reload reach the widgets GTK does not restyle on its own (modules P2's GUI
//! pass, defect D1, 2026-09-24).
//!
//! **What was seen.** In a `--clean` launch, a tray chip that first appeared after startup (the
//! `editor` chip after `Ctrl+a x`) kept the fallback theme's colours: its border and text were the
//! fallback's, about 2:1 on the bar, while the `terminal` chip beside it, on screen from the start,
//! was drawn in nvim's colours. Focusing it, changing one of its classes or a window-state change
//! restyled it for good. It did not happen with the owner's rose-pine dawn, and a chip first shown
//! after a later `:colorscheme morning` was right.
//!
//! **Why, read out of GTK 4.22.5's source and reproduced in a GTK program under Xvfb.** A
//! `CssProvider` reload reaches a widget only through that widget's own `GtkStyleContext`, which is
//! connected to the display's style cascade (`gtkstylecontext.c`, `gtk_style_context_cascade_changed`
//! -> `gtk_css_node_invalidate_style_provider`), and a widget gets that context when it is first
//! realized (`gtk_widget_realize` ends in `gtk_widget_get_style_context`). The recursion that walks
//! down from a realized widget stops at every child widget, because a widget node always reports a
//! provider of its own (`gtk_css_widget_node_get_style_provider` falls back to the settings' cascade),
//! and `_gtk_css_change_for_child` drops `SOURCE` on the way down. So a widget that has never been
//! realized is reached only by `PARENT_STYLE`, which its parent passes down only when the parent's own
//! computed style changed.
//!
//! That leaves a hole: a widget whose style GTK computed before it was ever realized, under a parent
//! whose computed style the reload does not change, keeps the old stylesheet's style -- and becoming
//! visible, mapped and realized later does not recompute it. At scale 1, the pass's, creating the
//! style context invalidates nothing (`gtk_style_context_set_scale` and `_set_display` return early
//! when nothing changes). At another scale it swaps the cascade and does invalidate, so by the same
//! reading a HiDPI screen would not show this; not tried. Two shapes in this window have their style
//! computed that early:
//! - **a hidden widget before a visible sibling**: `gtk_css_node_ensure_style` computes every earlier
//!   sibling that needs it, hidden or not, before the one it was asked for. The tray's chips are made
//!   in a fixed order (editor, agent, terminal, ...) and only the terminal's shows at startup, so the
//!   editor's and the agent's were styled from the fallback on the first frame;
//! - **a `set_child_visible(false)` widget**: its CSS node stays visible and is validated with the
//!   window, though it is never mapped -- the module grid hides modules and their dividers this way.
//!
//! And the fallback theme's `chrome_fg` and `bg` are nvim's own default colours, which is exactly what
//! `--clean` sends: the top bar's and the grid's computed styles do not change on the first reload,
//! so nothing reached their hidden children. Any colorscheme that changes them (rose-pine dawn,
//! `:colorscheme morning`) did, which is why those were right.
//!
//! **The fix** is [`restyle_unrealized`], which [`super::gtk_css::ThemeCss::update`] runs after every
//! reload. The Xvfb reproduction, both shapes, red without it and green with it, is
//! `shell/tests/hidden_widget_restyle.rs` (opt-in, it needs a display).

use gtk4::prelude::*;

/// A class no rule names (`gtk_css`'s tests hold that). Adding and then removing it leaves a widget's
/// classes as they were and its style marked for recomputation: a class change is one of GTK's
/// "radical" changes (`GTK_CSS_RADICAL_CHANGE`), which always recomputes from the current providers.
pub(crate) const RESTYLE_CLASS: &str = "neovibe-restyle";

/// Marks every widget under `root` (`root` included) that has never been realized for a fresh style
/// from the current stylesheet, so that when it is next drawn it is drawn in the current theme and
/// not in whatever theme was loaded when GTK first computed its style (this module's doc).
///
/// A realized widget is left alone: its style context already had it invalidated by the reload. The
/// recomputation itself happens when GTK next needs the widget's style -- on the next frame for one
/// whose CSS node is visible, when it is shown for a hidden one -- so this costs a walk of the widget
/// tree per colorscheme change, and nothing per frame.
pub(crate) fn restyle_unrealized(root: &gtk4::Widget) {
    let mut stack = vec![root.clone()];
    while let Some(widget) = stack.pop() {
        if !widget.is_realized() {
            widget.add_css_class(RESTYLE_CLASS);
            widget.remove_css_class(RESTYLE_CLASS);
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            stack.push(c);
        }
    }
}
