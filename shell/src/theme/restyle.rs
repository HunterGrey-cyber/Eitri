//! Restyles widgets that GTK leaves on an older stylesheet after a reload.
//!
//! In a `--clean` launch, a tray chip first shown after startup could keep the fallback
//! colours while a chip visible from startup used nvim's colours. Changing focus,
//! classes or window state could force the hidden chip to take the current colours.
//!
//! In GTK 4.22.5, a `CssProvider` reload reaches a widget through its `GtkStyleContext`,
//! which is created when the widget is first realized. Recursion from a realized
//! widget stops at child widget nodes: each has its own provider, and
//! `_gtk_css_change_for_child` drops `SOURCE`. An unrealized child is reached only by
//! `PARENT_STYLE`, which is passed down when its parent's computed style changes.
//!
//! A hidden widget whose style was already computed can therefore keep the old style
//! when a reload leaves its parent's computed style unchanged. Becoming visible and
//! realized later does not recompute it at scale 1: `gtk_style_context_set_scale` and
//! `_set_display` return early when nothing changes.
//!
//! Two widget shapes here have their styles computed before they are realized:
//! a hidden tray chip before a visible sibling, because `gtk_css_node_ensure_style`
//! computes earlier siblings, and a widget hidden with `set_child_visible(false)`,
//! whose CSS node stays visible and is validated with the window. The module grid
//! uses the latter for modules and dividers.
//!
//! The fallback bar colours and `bg` match nvim's defaults. A `--clean` reload can
//! leave the top bar's and grid's computed styles unchanged, so their hidden children
//! keep fallback styles. A colorscheme that changes those parent colours reaches them.
//!
//! [`super::gtk_css::ThemeCss::update`] calls [`restyle_unrealized`] after every reload.
//! `shell/tests/hidden_widget_restyle.rs` reproduces both shapes under Xvfb and checks
//! that the workaround changes the result. The test is opt-in and needs a display.

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
