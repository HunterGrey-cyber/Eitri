//! What the log says when a module's host is unrealized.
//!
//! A host is never unrealized while its window is up (hiding it, moving it between tiles and the
//! scratchpad only unmap it), so the line a host's unrealize writes is the grid's tripwire for a
//! reparent or rebuild by mistake. The window's own close unrealizes every host once as GTK takes
//! the window down; that is expected and its line says so quietly. GTK takes a closing window down
//! after hiding it, so a host unrealized under a window that is no longer visible is the close, and
//! one unrealized under a visible window (or under none, where nothing says it is closing) is not.

use gtk4::prelude::*;

/// Whether `widget` is being unrealized because its window is closing.
pub(crate) fn window_is_closing(widget: &gtk4::Widget) -> bool {
    widget
        .root()
        .and_then(|root| root.downcast::<gtk4::Window>().ok())
        .is_some_and(|window| !window.is_visible())
}

/// The log line for `id`'s `nth` unrealize.
pub(crate) fn line(id: &dyn std::fmt::Display, nth: u32, closing: bool) -> String {
    if closing {
        format!("[module_grid] {id} unrealized ({nth} so far) as its window closes")
    } else {
        format!("[module_grid] {id} unrealized ({nth} so far) -- must not happen before the window closes")
    }
}

#[cfg(test)]
mod tests {
    use super::line;

    #[test]
    fn the_close_s_line_is_quiet_and_any_other_names_the_fault() {
        let closing = line(&"editor", 1, true);
        assert_eq!(
            closing,
            "[module_grid] editor unrealized (1 so far) as its window closes"
        );
        assert!(!closing.contains("must not"), "{closing}");
        let fault = line(&"agent", 2, false);
        assert!(
            fault.ends_with("-- must not happen before the window closes"),
            "{fault}"
        );
        // The passes read both by this prefix.
        for l in [&closing, &fault] {
            assert!(l.starts_with("[module_grid] ") && l.contains(" unrealized ("), "{l}");
        }
    }
}
