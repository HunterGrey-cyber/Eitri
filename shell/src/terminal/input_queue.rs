//! Input that waits for a paste (bottom-terminal phase 2).
//!
//! `Ctrl+Shift+V` reads the clipboard asynchronously: on Wayland the text comes through a pipe from
//! whichever program owns the clipboard, a main-loop turn or more later. Anything typed in that time
//! -- the `"` that closes a quoted argument, the Enter that runs the line -- must reach the shell
//! after the paste, never before it: an Enter that overtook a paste would run a command before the
//! owner had finished it. So a paste takes a place in line when the chord is pressed, and every key,
//! input-method commit and `Ctrl+a` literal after it waits behind that place until the text is in.
//!
//! **A clipboard that never answers must not hold the keyboard.** The pane gives up on a paste after
//! `pane::PASTE_TIMEOUT`; what waited behind it goes on, and text that turns up after that is
//! dropped (and logged), since by then it would land in the middle of later typing.
//!
//! Pure, and tested without a display. The GTK half (`pane.rs`) starts the read and reports how it
//! ended.

use std::collections::VecDeque;

use terminal_input::NormalizedInput;

/// One paste's place in line, handed out by [`InputQueue::begin_paste`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PasteTicket(u64);

#[derive(Debug)]
enum Slot {
    Waiting(PasteTicket),
    Ready(NormalizedInput),
}

/// Everything bound for the shell, in the order the owner produced it.
#[derive(Debug, Default)]
pub(crate) struct InputQueue {
    slots: VecDeque<Slot>,
    next: u64,
}

impl InputQueue {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Input for the shell. `Some`: send it now, no paste is waiting. `None`: it waits behind one.
    pub(crate) fn submit(&mut self, input: NormalizedInput) -> Option<NormalizedInput> {
        if self.slots.is_empty() {
            return Some(input);
        }
        self.slots.push_back(Slot::Ready(input));
        None
    }

    /// `Ctrl+Shift+V` was pressed: a place in line for the text it will read.
    pub(crate) fn begin_paste(&mut self) -> PasteTicket {
        let ticket = PasteTicket(self.next);
        self.next += 1;
        self.slots.push_back(Slot::Waiting(ticket));
        ticket
    }

    /// A paste's read ended: `Some(input)` if it produced something to send, `None` if not (an
    /// empty clipboard, a read error, the timeout). Returns what is ready now, in order: this paste
    /// and what waited behind it, up to the next paste still being read. `None` when this paste was
    /// already given up on (or forgotten by [`Self::clear`]): its caller drops the text.
    pub(crate) fn finish(
        &mut self,
        ticket: PasteTicket,
        input: Option<NormalizedInput>,
    ) -> Option<Vec<NormalizedInput>> {
        let at = self
            .slots
            .iter()
            .position(|slot| matches!(slot, Slot::Waiting(waiting) if *waiting == ticket))?;
        match input {
            Some(input) => self.slots[at] = Slot::Ready(input),
            None => {
                self.slots.remove(at);
            }
        }
        let mut ready = Vec::new();
        while matches!(self.slots.front(), Some(Slot::Ready(_))) {
            if let Some(Slot::Ready(input)) = self.slots.pop_front() {
                ready.push(input);
            }
        }
        Some(ready)
    }

    /// Forgets everything waiting: it was for a shell that is gone (Enter restarts a new one).
    pub(crate) fn clear(&mut self) {
        self.slots.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(text: &str) -> NormalizedInput {
        NormalizedInput::Paste {
            text: text.to_string(),
            bracketed: false,
        }
    }

    fn pasted(text: &str) -> NormalizedInput {
        NormalizedInput::Paste {
            text: text.to_string(),
            bracketed: true,
        }
    }

    #[test]
    fn with_no_paste_waiting_input_goes_straight_through() {
        let mut queue = InputQueue::new();
        assert_eq!(queue.submit(typed("a")), Some(typed("a")));
        assert_eq!(queue.submit(typed("b")), Some(typed("b")));
    }

    /// The case this exists for: `git commit -m "`, `Ctrl+Shift+V`, `"`, Enter, typed faster than the
    /// clipboard answers. The paste goes first, then the quote, then the Enter.
    #[test]
    fn what_is_typed_after_a_paste_waits_for_it_and_follows_it() {
        let mut queue = InputQueue::new();
        let ticket = queue.begin_paste();
        assert_eq!(queue.submit(typed("\"")), None);
        assert_eq!(queue.submit(typed("\r")), None);
        assert_eq!(
            queue.finish(ticket, Some(pasted("fix the bug"))),
            Some(vec![pasted("fix the bug"), typed("\""), typed("\r")])
        );
        assert_eq!(queue.submit(typed("x")), Some(typed("x")), "nothing waits any more");
    }

    #[test]
    fn a_paste_that_produced_nothing_releases_what_waited_behind_it() {
        let mut queue = InputQueue::new();
        let ticket = queue.begin_paste();
        assert_eq!(queue.submit(typed("a")), None);
        assert_eq!(queue.finish(ticket, None), Some(vec![typed("a")]));
    }

    /// The timeout gave up on a paste and let the typing go on; the clipboard's text then turns up.
    /// It is dropped: sent now, it would land in the middle of whatever came after.
    #[test]
    fn text_that_arrives_after_its_paste_was_given_up_on_is_dropped() {
        let mut queue = InputQueue::new();
        let ticket = queue.begin_paste();
        assert_eq!(queue.submit(typed("a")), None);
        assert_eq!(queue.finish(ticket, None), Some(vec![typed("a")]), "the timeout");
        assert_eq!(queue.finish(ticket, Some(pasted("late"))), None);
        assert_eq!(queue.submit(typed("b")), Some(typed("b")));
    }

    /// Two pastes in a row keep their order whichever clipboard read ends first.
    #[test]
    fn two_pastes_keep_their_order_whichever_read_ends_first() {
        let mut queue = InputQueue::new();
        let first = queue.begin_paste();
        assert_eq!(queue.submit(typed("a")), None);
        let second = queue.begin_paste();
        assert_eq!(queue.submit(typed("b")), None);
        assert_eq!(
            queue.finish(second, Some(pasted("two"))),
            Some(vec![]),
            "the first paste is still being read"
        );
        assert_eq!(
            queue.finish(first, Some(pasted("one"))),
            Some(vec![pasted("one"), typed("a"), pasted("two"), typed("b")])
        );
    }

    #[test]
    fn clearing_forgets_everything_that_waited() {
        let mut queue = InputQueue::new();
        let ticket = queue.begin_paste();
        assert_eq!(queue.submit(typed("for the old shell")), None);
        queue.clear();
        assert_eq!(queue.submit(typed("b")), Some(typed("b")));
        assert_eq!(queue.finish(ticket, Some(pasted("late"))), None);
    }
}
