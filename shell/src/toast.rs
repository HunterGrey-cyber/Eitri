//! The window's one toast: a line at the top right of the modules,
//! over them, for a few seconds, never taking a click or the keys. It says a hidden chat is holding a
//! new permission card, and it is the only thing that does in Immersive mode, where the top bar and
//! its tray are hidden. **Below the top bar while the bar is drawn** ([`toast_top`]): at the bar's
//! own height it covered the bar's right end -- the window controls, `↻`, and the `agent ⚑N` chip it
//! announces, for exactly the seconds that chip matters.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use eitri_core::attention::Attention;
use gtk4::glib;
use gtk4::prelude::*;

/// How long a toast stays up; a new one restarts it.
pub(crate) const TOAST_FOR: Duration = Duration::from_secs(4);

/// How long a one-time notice stays up: it is a sentence read once, not a glance at a count.
pub(crate) const NOTICE_FOR: Duration = Duration::from_secs(12);

/// The toast's distance from the edge of what it sits over. Also `close_prompt`'s corner margin
/// when the top bar is hidden (Immersive) -- the two overlay labels share one Immersive fallback.
pub(crate) const TOAST_MARGIN: i32 = 8;

/// How far below the window's top the toast sits: under the top bar while the bar is drawn, so it
/// never covers the tray chip it announces; at the top when Immersive hides the bar.
pub(crate) fn toast_top(bar_shown: bool, bar_height: i32) -> i32 {
    TOAST_MARGIN + if bar_shown { bar_height.max(0) } else { 0 }
}

/// What the toast says when a card arrives for a chat view that is not on screen. `chat` is the
/// view's chip title (`agent`, `agent 2`) and `attention` the view's own, so the toast reads as that
/// view's chip does. `tab` is the view's tab holding its newest pending card, `(number, label name)`
///; `None` when no tab is known
/// (session tabs not yet wired up, or every tracked card already gone by the time this reads it),
/// and then the toast falls back to a bare count. `way_back` is the key that brings the chat back,
/// as the effective keymap binds it (`Ctrl+b a` by default); `None` when the user unbound it, and
/// then the toast promises no key.
pub(crate) fn permission_toast_text(
    chat: &str,
    attention: Attention,
    tab: Option<(u16, &str)>,
    way_back: Option<&str>,
) -> String {
    let what = match tab {
        Some((number, name)) => format!("tab {number} \"{name}\""),
        None if attention.pending == 1 => "1 permission card waiting".to_string(),
        None => format!("{} permission cards waiting", attention.pending),
    };
    let mut text = format!("{chat} \u{2691}{} \u{2014} {what}", attention.pending);
    if let Some(way_back) = way_back {
        text.push_str(&format!(" \u{00b7} {way_back}"));
    }
    text
}

/// What the toast now showing announces. A waiting permission card's toast is the only word of the
/// card in Immersive mode, so it is told apart from every other toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToastKind {
    /// A hidden chat holds a permission card ([`permission_toast_text`]).
    Attention,
    /// Anything else: a refusal's reason, a one-time notice, a restore's result.
    Other,
    /// A note that stays up for as long as what it says holds ([`Toast::set_standing`]).
    Standing,
}

/// What the toast shows once a timed line's time is up: the standing note when there is one, else nothing. A
/// timed line covers the standing note for its seconds and then gives it back.
pub(crate) fn after_timed(standing: Option<&str>) -> Option<(&str, ToastKind)> {
    standing.map(|text| (text, ToastKind::Standing))
}

/// Whether a refused key may show its reason on the toast, given what the toast shows now. Never
/// over a waiting card's toast: a stray key would otherwise erase the one line saying a card waits,
/// and the refusal still has its flash and its stdout line.
pub(crate) fn refusal_may_show(showing: Option<ToastKind>) -> bool {
    showing != Some(ToastKind::Attention)
}

pub(crate) struct Toast {
    label: gtk4::Label,
    /// The top bar: the toast goes below it while it is drawn ([`toast_top`]).
    bar: gtk4::Widget,
    hide: RefCell<Option<glib::SourceId>>,
    /// What the label says while it is up; `None` once it has hidden.
    showing: Cell<Option<ToastKind>>,
    /// The note that stays up between timed lines, while what it says holds.
    standing: RefCell<Option<String>>,
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
            showing: Cell::new(None),
            standing: RefCell::new(None),
        })
    }

    pub(crate) fn show(self: &Rc<Self>, text: &str) {
        self.show_for(text, TOAST_FOR);
    }

    pub(crate) fn show_for(self: &Rc<Self>, text: &str, duration: Duration) {
        self.show_kind(text, duration, ToastKind::Other);
    }

    /// A hidden chat's waiting card ([`permission_toast_text`]): kept up against refusals for its
    /// [`TOAST_FOR`].
    pub(crate) fn show_attention(self: &Rc<Self>, text: &str) {
        self.show_kind(text, TOAST_FOR, ToastKind::Attention);
    }

    /// A refused key's reason, unless a waiting card's toast is up ([`refusal_may_show`]); `false`
    /// when it was not shown.
    pub(crate) fn show_refusal(self: &Rc<Self>, text: &str) -> bool {
        if !refusal_may_show(self.showing.get()) {
            return false;
        }
        self.show_kind(text, TOAST_FOR, ToastKind::Other);
        true
    }

    /// A note that stays up until it is set again: `Some` shows it now and after every timed line, `None` takes
    /// it down (a timed line up now still runs its time).
    pub(crate) fn set_standing(self: &Rc<Self>, text: Option<&str>) {
        *self.standing.borrow_mut() = text.map(str::to_string);
        match text {
            Some(text) => {
                if let Some(source) = self.hide.borrow_mut().take() {
                    source.remove();
                }
                self.place(text, ToastKind::Standing);
            }
            None if self.showing.get() == Some(ToastKind::Standing) => {
                self.showing.set(None);
                self.label.set_visible(false);
            }
            None => {}
        }
    }

    /// Puts `text` up, below the top bar while it is drawn.
    fn place(&self, text: &str, kind: ToastKind) {
        // A toast raised while the window is still being built (the tmux import's notice) comes
        // before the bar has a height; its natural height is the one it is about to get.
        let bar_height = match self.bar.height() {
            0 => self.bar.measure(gtk4::Orientation::Vertical, -1).1,
            allocated => allocated,
        };
        self.label.set_margin_top(toast_top(self.bar.is_visible(), bar_height));
        self.label.set_label(text);
        self.label.set_visible(true);
        self.showing.set(Some(kind));
    }

    fn show_kind(self: &Rc<Self>, text: &str, duration: Duration, kind: ToastKind) {
        self.place(text, kind);
        if let Some(source) = self.hide.borrow_mut().take() {
            source.remove();
        }
        let toast = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(duration, move || {
            if let Some(toast) = toast.upgrade() {
                toast.hide.borrow_mut().take();
                let standing = toast.standing.borrow().clone();
                match after_timed(standing.as_deref()) {
                    Some((text, kind)) => toast.place(text, kind),
                    None => {
                        toast.showing.set(None);
                        toast.label.set_visible(false);
                    }
                }
            }
        });
        *self.hide.borrow_mut() = Some(source);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The toast names the tab that holds the newest card.
    #[test]
    fn the_toast_names_the_tab_and_the_way_back_from_the_keymap() {
        let one = Attention {
            pending: 1,
            unread: false,
            ..Default::default()
        };
        assert_eq!(
            permission_toast_text("agent", one, Some((2, "docs")), Some("Ctrl+b a")),
            "agent \u{2691}1 \u{2014} tab 2 \"docs\" \u{00b7} Ctrl+b a"
        );
        let three = Attention {
            pending: 3,
            unread: true,
            ..Default::default()
        };
        assert_eq!(
            permission_toast_text("agent", three, Some((1, "new")), Some("Ctrl+a a")),
            "agent \u{2691}3 \u{2014} tab 1 \"new\" \u{00b7} Ctrl+a a"
        );
        assert_eq!(
            permission_toast_text("agent", one, None, None),
            "agent \u{2691}1 \u{2014} 1 permission card waiting",
            "no tab known and no key bound: the count, and no promise"
        );
        assert_eq!(
            permission_toast_text("agent 2", one, Some((3, "x")), Some("the agent 2 chip")),
            "agent 2 \u{2691}1 \u{2014} tab 3 \"x\" \u{00b7} the agent 2 chip",
            "an extra view's toast reads as its chip does"
        );
    }

    /// A refused key pressed while a hidden chat's card toast is up -- in Immersive mode the toast
    /// is all that says a card waits -- leaves that toast alone; any other toast, or none, gives way.
    #[test]
    fn a_refusal_never_replaces_a_waiting_cards_toast() {
        assert!(!refusal_may_show(Some(ToastKind::Attention)));
        assert!(
            refusal_may_show(Some(ToastKind::Other)),
            "a refusal replaces another refusal or a notice"
        );
        assert!(refusal_may_show(None), "nothing showing");
        assert!(
            refusal_may_show(Some(ToastKind::Standing)),
            "a refusal covers a standing note for its seconds"
        );
    }

    /// A standing note outlasts every timed line: each covers it for its time and then gives it back, and with
    /// no standing note the toast hides as it always has.
    #[test]
    fn a_standing_note_comes_back_after_each_timed_line() {
        assert_eq!(
            after_timed(Some("the editor window is closed")),
            Some(("the editor window is closed", ToastKind::Standing))
        );
        assert_eq!(after_timed(None), None);
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
