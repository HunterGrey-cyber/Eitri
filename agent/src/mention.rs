//! Keeps the CLI from reading a file nobody named to it.
//!
//! The CLI scans a turn's whole text for `@path` and reads what it finds, before any permission
//! check: a word that starts with `@` at the start of a line or after whitespace, wherever in the text it
//! sits. For a line a person typed that is the feature. For anything else in a turn (lines selected in an
//! editor, a plugin's words, what a command printed) it would let the text of a file or a program
//! choose which of the person's files reach the model, with no card and no word.

use std::borrow::Cow;

/// Comes before the `@`: it is not whitespace to the CLI's scan, and draws as nothing.
const WORD_JOINER: char = '\u{2060}';

/// What the CLI counts as whitespace before a mention: Unicode's, and the byte order mark its `\s` takes too.
fn opens_a_word(before: char) -> bool {
    before.is_whitespace() || before == '\u{feff}'
}

/// `text` with every `@` the CLI would read as a mention made into plain text. A reader sees the same
/// words. An `@` inside a word (`me@example.com`) is left alone, as the CLI leaves it.
pub fn inert(text: &str) -> Cow<'_, str> {
    let mut before = None;
    let mut mentions = text.char_indices().filter_map(|(at, ch)| {
        let opens = ch == '@' && before.is_none_or(opens_a_word);
        before = Some(ch);
        opens.then_some(at)
    });
    let Some(first) = mentions.next() else {
        return Cow::Borrowed(text);
    };
    let mut out = String::with_capacity(text.len() + WORD_JOINER.len_utf8());
    let mut copied = 0;
    for at in std::iter::once(first).chain(mentions) {
        out.push_str(&text[copied..at]);
        out.push(WORD_JOINER);
        copied = at;
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mention_at_the_start_of_the_text_of_a_line_or_after_a_blank_is_made_plain() {
        assert_eq!(inert("@a.txt"), "\u{2060}@a.txt");
        assert_eq!(inert("see @a.txt and\n@/etc/passwd"), "see \u{2060}@a.txt and\n\u{2060}@/etc/passwd");
        assert_eq!(inert("tab\t@a\u{3000}@b"), "tab\t\u{2060}@a\u{3000}\u{2060}@b");
        assert_eq!(inert("\u{feff}@a"), "\u{feff}\u{2060}@a");
    }

    #[test]
    fn what_the_cli_does_not_read_as_a_mention_stays_as_it_is() {
        for text in ["me@example.com", "`@a.txt`", "(@a.txt)", "x:@a.txt", "\"@a.txt\"", "\\@a.txt", "no at sign", ""] {
            assert!(matches!(inert(text), Cow::Borrowed(_)), "{text:?}");
        }
        assert_eq!(inert("@@a"), "\u{2060}@@a");
    }

    #[test]
    fn it_stops_at_once_on_what_it_already_changed() {
        let once = inert("a @b\n@c").into_owned();
        assert_eq!(inert(&once), once);
    }
}
