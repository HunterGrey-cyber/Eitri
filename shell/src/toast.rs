//! The window's one toast (modules spec §3.3, decision b): a line at the top right of the modules,
//! over them, for a few seconds, never taking a click or the keys. It says a hidden chat is holding a
//! new permission card, and it is the only thing that does in Immersive mode, where the top bar and
//! its tray are hidden. **Below the top bar while the bar is drawn** ([`toast_top`]): at the bar's
//! own height it covered the bar's right end -- the window controls, `↻`, and the `agent ⚑N` chip it
//! announces, for exactly the seconds that chip matters (the plan review's finding 4).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use neovibe_core::attention::Attention;

/// How long a toast stays up; a new one restarts it.
pub(crate) const TOAST_FOR: Duration = Duration::from_secs(4);

/// The toast's distance from the edge of what it sits over.
const TOAST_MARGIN: i32 = 8;

/// How far below the window's top the toast sits: under the top bar while the bar is drawn, so it
/// never covers the tray chip it announces; at the top when Immersive hides the bar.
pub(crate) fn toast_top(bar_shown: bool, bar_height: i32) -> i32 {
    TOAST_MARGIN + if bar_shown { bar_height.max(0) } else { 0 }
}

/// What the toast says when a card arrives for a chat that is not on screen. `way_back` is the key
/// that brings the chat back, as the effective keymap binds it (`Ctrl+b a` by default); `None` when
/// the user unbound it, and then the toast promises no key.
pub(crate) fn permission_toast_text(attention: Attention, way_back: Option<&str>) -> String {
    let cards = if attention.pending == 1 {
        "1 permission card".to_string()
    } else {
        format!("{} permission cards", attention.pending)
    };
    let mut text = format!("agent \u{2691}{} \u{2014} {cards} waiting", attention.pending);
    if let Some(way_back) = way_back {
        text.push_str(&format!(" \u{00b7} {way_back}"));
    }
    text
}

pub(crate) struct Toast {
    label: gtk4::Label,
    /// The top bar: the toast goes below it while it is drawn ([`toast_top`]).
    bar: gtk4::Widget,
    hide: RefCell<Option<glib::SourceId>>,
}

impl Toast {
    /// The toast, as an overlay child of `overlay` (the window's HINT overlay, which already wraps
    /// the whole window, top bar included), kept below `bar`. Hidden until [`Toast::show`].
    pub(crate) fn install(overlay: &gtk4::Overlay, bar: &gtk4::Widget) -> Rc<Toast> {
        let label = gtk4::Label::new(None);
        label.add_css_class("module-toast");
        label.set_halign(gtk4::Align::End);
        label.set_valign(gtk4::Align::Start);
        label.set_margin_top(TOAST_MARGIN);
        label.set_margin_end(TOAST_MARGIN);
        label.set_can_target(false);
        label.set_can_focus(false);
        label.set_visible(false);
        overlay.add_overlay(&label);
        Rc::new(Toast {
            label,
            bar: bar.clone(),
            hide: RefCell::new(None),
        })
    }

    pub(crate) fn show(self: &Rc<Self>, text: &str) {
        self.label
            .set_margin_top(toast_top(self.bar.is_visible(), self.bar.height()));
        self.label.set_label(text);
        self.label.set_visible(true);
        if let Some(source) = self.hide.borrow_mut().take() {
            source.remove();
        }
        let toast = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(TOAST_FOR, move || {
            if let Some(toast) = toast.upgrade() {
                toast.hide.borrow_mut().take();
                toast.label.set_visible(false);
            }
        });
        *self.hide.borrow_mut() = Some(source);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_toast_names_the_count_and_the_way_back_from_the_keymap() {
        let one = Attention {
            pending: 1,
            unread: false,
            ..Default::default()
        };
        assert_eq!(
            permission_toast_text(one, Some("Ctrl+b a")),
            "agent \u{2691}1 \u{2014} 1 permission card waiting \u{00b7} Ctrl+b a"
        );
        let three = Attention {
            pending: 3,
            unread: true,
            ..Default::default()
        };
        assert_eq!(
            permission_toast_text(three, Some("Ctrl+a a")),
            "agent \u{2691}3 \u{2014} 3 permission cards waiting \u{00b7} Ctrl+a a"
        );
        assert_eq!(
            permission_toast_text(one, None),
            "agent \u{2691}1 \u{2014} 1 permission card waiting",
            "no key is bound to the agent: no way back is promised"
        );
    }

    /// Below the top bar while it is drawn -- never over the `agent ⚑N` chip, `↻` or the window
    /// controls at the bar's right end -- and at the top when Immersive hides the bar.
    #[test]
    fn the_toast_sits_below_the_top_bar_and_at_the_top_without_it() {
        assert_eq!(toast_top(true, 39), 47);
        assert_eq!(toast_top(false, 39), 8);
        assert_eq!(toast_top(true, 0), 8, "a bar not yet allocated");
    }
}
