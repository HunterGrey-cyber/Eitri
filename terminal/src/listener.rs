//! The `Term`'s `EventListener`. The frozen driver used `VoidListener`, which silently dropped every
//! answer the terminal owes a program: DSR cursor position, DA, colour queries (the spike reproduced
//! `ESC[6n` going unanswered). This one keeps them, in order, for [`crate::screen::Screen`] to write
//! back to the PTY once the parser returns -- the listener runs inside `Term`'s own parse and cannot
//! reach the PTY, or read the `Term`, itself.

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

/// What the host should hear about. Bounded by construction: one slot each, latest wins.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HostEvents {
    /// `Some(Some(t))` a new title, `Some(None)` a reset, `None` nothing new.
    pub title: Option<Option<String>>,
    pub bell: bool,
    /// Text a program copied to the clipboard with OSC 52 (`ESC ] 52 ; c ; …`). A *load* never gets
    /// this far: `Term` refuses it (`Osc52::OnlyCopy`, see `screen::term_config`).
    pub clipboard: Option<String>,
    /// Text a program copied to the primary selection with OSC 52 (`p`, and `s`, which `Term` reads
    /// as the same thing): what a middle click pastes. Kept apart from `clipboard` (bottom-terminal
    /// phase 2) so an nvim `"*y` inside the terminal does not overwrite what `Ctrl+V` pastes.
    pub primary: Option<String>,
}

#[derive(Default)]
pub(crate) struct Pending {
    pub(crate) replies: Vec<Reply>,
    pub(crate) events: HostEvents,
}

#[derive(Clone, Default)]
pub(crate) struct Listener(pub(crate) Arc<Mutex<Pending>>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
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
            Event::ClipboardStore(ClipboardType::Clipboard, text) => pending.events.clipboard = Some(text),
            Event::ClipboardStore(ClipboardType::Selection, text) => pending.events.primary = Some(text),
            // Refused inside `Term` by `Osc52::OnlyCopy` before it could get here. Should a later
            // alacritty_terminal send one anyway, answering nothing IS the refusal: a program in the
            // terminal reading the owner's clipboard is a data leak (spec, OSC 52).
            Event::ClipboardLoad(..) => {}
            Event::MouseCursorDirty
            | Event::CursorBlinkingChange
            | Event::Wakeup
            | Event::Exit
            | Event::ChildExit(_) => {}
        }
    }
}
