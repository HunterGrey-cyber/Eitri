//! Toast placement, permission text, protection of waiting-card announcements and the standing note.

use super::close::TOAST_MARGIN;
use crate::attention::Attention;

/// How far below the window's top the toast sits: under the top bar while the bar is drawn, so it
/// never covers the tray chip it announces; at the top when Immersive hides the bar.
pub fn toast_top(bar_shown: bool, bar_height: i32) -> i32 {
    TOAST_MARGIN + if bar_shown { bar_height.max(0) } else { 0 }
}

/// What the toast says when a card arrives for a chat view that is not on screen. `chat` is the
/// view's chip title (`agent`, `agent 2`) and `attention` the view's own, so the toast reads as that
/// view's chip does. `tab` is the view's tab holding its newest pending card, `(number, label name)`;
/// `None` when no tab is known (or every tracked card is already gone by the time this reads it),
/// and then the toast falls back to a bare count. `way_back` is the key that brings the chat back,
/// as the effective keymap binds it (`Ctrl+b a` by default); `None` when the user unbound it, and
/// then the toast promises no key.
pub fn permission_toast_text(
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
pub enum ToastKind {
    /// A hidden chat holds a permission card ([`permission_toast_text`]).
    Attention,
    /// Anything else: a refusal's reason, a one-time notice, a restore's result.
    Other,
    /// A note that stays up for as long as what it says holds, between timed lines.
    Standing,
}

/// What the toast shows once a timed line's time is up: the standing note when there is one, else nothing. A
/// timed line covers the standing note for its seconds and then gives it back.
pub fn after_timed(standing: Option<&str>) -> Option<(&str, ToastKind)> {
    standing.map(|text| (text, ToastKind::Standing))
}

/// Whether a refused key may show its reason on the toast, given what the toast shows now. Never
/// over a waiting card's toast: a stray key would otherwise erase the one line saying a card waits,
/// and the refusal still has its flash and its stdout line.
pub fn refusal_may_show(showing: Option<ToastKind>) -> bool {
    showing != Some(ToastKind::Attention)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Naming the newest card's tab tells the user where to answer it.
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
