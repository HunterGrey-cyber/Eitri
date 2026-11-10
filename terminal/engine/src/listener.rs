//! The Term's EventListener keeps DSR, DA and colour-query replies in order for
//! [`crate::screen::Screen`] to write back once parsing returns. The listener runs inside Term's
//! parse and cannot reach the PTY or read Term itself; dropping replies would leave requests
//! such as ESC[6n unanswered.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::term::ClipboardType;
use alacritty_terminal::vte::ansi::Rgb;

/// Something the program asked for that has to be answered on the PTY, in the order asked.
pub(crate) enum Reply {
    /// A reply `Term` already formatted (DSR, DA, the kitty keyboard query, ...).
    Bytes(String),
    /// OSC 4/10/11/12 "what colour is index N": answered from the `Term`'s own override, else the
    /// palette the screen was built with.
    Color(usize, Arc<dyn Fn(Rgb) -> String + Sync + Send + 'static>),
    /// `CSI 14 t` and friends: the text area in pixels.
    TextAreaSize(Arc<dyn Fn(WindowSize) -> String + Sync + Send + 'static>),
}

/// What the host should hear about. One slot per payload, latest wins; byte totals accumulate.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HostEvents {
    /// `Some(Some(t))` a new title, `Some(None)` a reset, `None` nothing new.
    pub title: Option<Option<String>>,
    pub bell: bool,
    /// Text a program copied to the clipboard with OSC 52 (`ESC ] 52 ; c ; …`). A *load* never gets
    /// this far: `Term` refuses it (`Osc52::OnlyCopy`, see `screen::term_config`).
    pub clipboard: Option<String>,
    /// Text a program copied to the primary selection with OSC 52 (`p`, and `s`, which `Term` reads
    /// as the same thing): what a middle click pastes. Kept apart from `clipboard` so an nvim
    /// `"*y` inside the terminal does not overwrite what `Ctrl+V` pastes.
    pub primary: Option<String>,
    /// Decoded UTF-8 byte totals for every OSC 52 write, including overwritten pending copies.
    pub clipboard_bytes: Option<usize>,
    pub primary_bytes: Option<usize>,
    /// Text a HOST selection gesture (a drag release) just finished. Not an OSC 52 event --
    /// selecting text never runs through `Term`'s own event stream -- so unlike the three fields
    /// above this is never populated by [`Listener`]; `crate::screen::Screen::take_events` merges it
    /// in from its own `pending_selection`. Copies to both the clipboard and the primary selection on
    /// release, as foot and alacritty do: the clipboard for `Ctrl+Shift+V` elsewhere, the primary
    /// selection for a middle click, as in every X/Wayland terminal.
    pub selection: Option<String>,
}

impl HostEvents {
    pub(crate) fn merge(&mut self, events: Self) {
        if events.title.is_some() {
            self.title = events.title;
        }
        self.bell |= events.bell;
        for (kept, incoming) in [
            (&mut self.clipboard, events.clipboard),
            (&mut self.primary, events.primary),
            (&mut self.selection, events.selection),
        ] {
            if incoming.is_some() {
                *kept = incoming;
            }
        }
        for (total, bytes) in [
            (&mut self.clipboard_bytes, events.clipboard_bytes),
            (&mut self.primary_bytes, events.primary_bytes),
        ] {
            if let Some(bytes) = bytes {
                *total = Some(total.unwrap_or(0).saturating_add(bytes));
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct Pending {
    pub(crate) replies: Vec<Reply>,
    pub(crate) events: HostEvents,
}

/// `.0` is what the host and the program are owed. `.1` counts every title, title reset and bell
/// `Term` has reported: the only events whose dispatches change nothing a frame shows, which is how
/// [`crate::screen::Screen`] tells a stream of them from one that drew something. A counter of its
/// own, beside the mutex, because the screen reads it at every publication -- for plain output that
/// is every character.
#[derive(Clone, Default)]
pub(crate) struct Listener(pub(crate) Arc<Mutex<Pending>>, pub(crate) Arc<AtomicU64>);

impl Listener {
    /// Titles, title resets and bells reported so far.
    pub(crate) fn inert_events(&self) -> u64 {
        self.1.load(Ordering::Relaxed)
    }
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        if matches!(event, Event::Title(_) | Event::ResetTitle | Event::Bell) {
            self.1.fetch_add(1, Ordering::Relaxed);
        }
        let mut pending = self
            .0
            .lock()
            .expect("listener mutex is never poisoned: nothing panics inside it");
        match event {
            Event::PtyWrite(text) => pending.replies.push(Reply::Bytes(text)),
            Event::ColorRequest(index, format) => pending.replies.push(Reply::Color(index, format)),
            Event::TextAreaSizeRequest(format) => pending.replies.push(Reply::TextAreaSize(format)),
            Event::Title(title) => pending.events.title = Some(Some(title)),
            Event::ResetTitle => pending.events.title = Some(None),
            Event::Bell => pending.events.bell = true,
            Event::ClipboardStore(ClipboardType::Clipboard, text) => {
                pending.events.clipboard_bytes =
                    Some(pending.events.clipboard_bytes.unwrap_or(0).saturating_add(text.len()));
                pending.events.clipboard = Some(text);
            }
            Event::ClipboardStore(ClipboardType::Selection, text) => {
                pending.events.primary_bytes =
                    Some(pending.events.primary_bytes.unwrap_or(0).saturating_add(text.len()));
                pending.events.primary = Some(text);
            }
            // Refused inside `Term` by `Osc52::OnlyCopy` before it could get here. Should a later
            // alacritty_terminal send one anyway, answering nothing IS the refusal: a program in the
            // terminal reading the user's clipboard is a data leak.
            Event::ClipboardLoad(..) => {}
            Event::MouseCursorDirty
            | Event::CursorBlinkingChange
            | Event::Wakeup
            | Event::Exit
            | Event::ChildExit(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_byte_totals_include_overwritten_copies_and_empty_writes() {
        let listener = Listener::default();
        for _ in 0..10 {
            listener.send_event(Event::ClipboardStore(ClipboardType::Clipboard, "é\0secret".into()));
        }
        listener.send_event(Event::ClipboardStore(ClipboardType::Selection, String::new()));
        let pending = listener.0.lock().unwrap();
        assert_eq!(pending.events.clipboard_bytes, Some(90));
        assert_eq!(pending.events.primary_bytes, Some(0));
        assert_eq!(pending.events.clipboard.as_deref(), Some("é\0secret"));
    }

    #[test]
    fn osc52_byte_totals_survive_coalescing_across_parser_reads() {
        let mut screen = crate::Screen::new(
            crate::PtySize {
                cols: 80,
                rows: 4,
                cell_width_px: 8,
                cell_height_px: 16,
            },
            crate::TerminalColors::default(),
        );
        let mut pending = HostEvents::default();
        for _ in 0..10 {
            screen.feed(b"\x1b]52;c;aGk=\x07\x1b]52;p;w6k=\x07");
            pending.merge(screen.take_events());
        }
        assert_eq!(pending.clipboard_bytes, Some(20));
        assert_eq!(pending.primary_bytes, Some(20));
        assert_eq!(pending.clipboard.as_deref(), Some("hi"));
        assert_eq!(pending.primary.as_deref(), Some("é"));
        assert_eq!(screen.take_events(), HostEvents::default());
    }
}
