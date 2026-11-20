//! The texts a sync holder has shown once, remembered so they are not shown again.
//!
//! Many of them are the server's (mode keepers' lines, ignore problems, refusals, error text that carries
//! a server's words), so what is remembered is bounded: at most [`SHOWN_ONCE`]'s count and bytes, the
//! oldest forgotten first, and each text cut at [`LINE_BYTES`] with a mark saying so. Forgetting, rather
//! than refusing a new text, keeps the laptop's own lines showing however much the server says; a flood can
//! push any text out, the laptop's own among them, so the most it causes is a forgotten line shown again.

use std::collections::{BTreeSet, VecDeque};

use agent::remote_answers::{AnswerBound, ITEM_OVERHEAD_BYTES};

/// The texts remembered between starts.
pub const SHOWN_ONCE: AnswerBound = AnswerBound::new("the sync lines remembered as shown", 512, 256 << 10);

/// The longest line shown; a longer text keeps its head and says how much was cut.
pub const LINE_BYTES: usize = 4096;

#[derive(Debug, Default)]
pub struct ShownOnce {
    texts: BTreeSet<String>,
    order: VecDeque<String>,
    bytes: usize,
}

impl ShownOnce {
    /// The line to show for `text` (cut if long), or `None` when it was shown already.
    pub fn admit(&mut self, text: String) -> Option<String> {
        let text = cut(text);
        if !self.texts.insert(text.clone()) {
            return None;
        }
        self.bytes += cost(&text);
        self.order.push_back(text.clone());
        while SHOWN_ONCE.admits(self.texts.len(), self.bytes).is_err() {
            let Some(oldest) = self.order.pop_front() else { break };
            self.texts.remove(&oldest);
            self.bytes -= cost(&oldest);
        }
        Some(text)
    }

    /// Forgets every text, so a new start shows them again.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn len(&self) -> usize {
        self.texts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.texts.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

fn cost(text: &str) -> usize {
    text.len() + ITEM_OVERHEAD_BYTES
}

fn cut(mut text: String) -> String {
    if text.len() <= LINE_BYTES {
        return text;
    }
    let mut end = LINE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let more = text.len() - end;
    text.truncate(end);
    text.push_str(&format!(" [cut: {more} more bytes]"));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_is_shown_once_until_cleared() {
        let mut shown = ShownOnce::default();
        assert_eq!(shown.admit("a".into()).as_deref(), Some("a"));
        assert_eq!(shown.admit("a".into()), None);
        shown.clear();
        assert_eq!(shown.admit("a".into()).as_deref(), Some("a"));
    }

    /// A server's flood of distinct texts never hides a later line of the laptop's own, and what is
    /// remembered stays within the bound.
    #[test]
    fn past_the_bound_the_oldest_texts_are_forgotten_and_new_ones_still_show() {
        let mut shown = ShownOnce::default();
        for n in 0..SHOWN_ONCE.entries() * 3 {
            assert!(shown.admit(format!("server note {n}")).is_some());
        }
        assert_eq!(shown.len(), SHOWN_ONCE.entries());
        let own = "sync stopped: the laptop's own line";
        assert_eq!(shown.admit(own.into()).as_deref(), Some(own));
        assert_eq!(
            shown.admit(format!("server note {}", SHOWN_ONCE.entries() * 3 - 1)),
            None
        );
        assert!(
            shown.admit("server note 0".into()).is_some(),
            "the oldest was forgotten"
        );
        for n in 0..SHOWN_ONCE.entries() {
            shown.admit(format!("{n} {}", "x".repeat(LINE_BYTES - 10)));
        }
        assert!(shown.bytes() <= SHOWN_ONCE.bytes(), "{}", shown.bytes());
    }

    #[test]
    fn a_long_text_is_cut_with_a_mark() {
        let mut shown = ShownOnce::default();
        let line = shown.admit(format!("{}é", "x".repeat(LINE_BYTES - 1))).unwrap();
        assert_eq!(line, format!("{} [cut: 2 more bytes]", "x".repeat(LINE_BYTES - 1)));
        let line = shown.admit("y".repeat(1 << 20)).unwrap();
        assert!(line.ends_with(&format!(" [cut: {} more bytes]", (1 << 20) - LINE_BYTES)));
        assert_eq!(
            shown.admit("y".repeat(1 << 20)),
            None,
            "the cut text is what is remembered"
        );
    }
}
