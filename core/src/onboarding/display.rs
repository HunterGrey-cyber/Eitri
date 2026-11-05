//! Display-safe text: the one cut every untrusted value passes before it fills a template.

/// The longest [`display_text`], in characters.
pub const MAX_CHARS: usize = 80;

/// `s` as a label may show it: each character that [`is_hidden`] names becomes a visible `\u{...}` escape, and the
/// result is cut to [`MAX_CHARS`], never through an escape. Constant wording does not come through here, so only the
/// value is cut, not the sentence around it.
pub fn display_text(s: &str) -> String {
    display_text_cut(s, MAX_CHARS)
}

/// Escape hidden characters and fit the result within `max` characters, keeping each escape whole.
pub fn display_text_cut(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut room = max;
    let mut chars = s.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        // A value already escaped by another process must keep its escape whole on a second cut.
        if let Some(escape) = unicode_escape(&s[at..]) {
            if escape.len() > room {
                break;
            }
            room -= escape.len();
            out.push_str(escape);
            while chars.peek().is_some_and(|(next, _)| *next < at + escape.len()) {
                chars.next();
            }
            continue;
        }
        let piece = if is_hidden(c) {
            c.escape_unicode().to_string()
        } else {
            c.to_string()
        };
        let width = piece.chars().count();
        if width > room {
            break;
        }
        room -= width;
        out.push_str(&piece);
    }
    out
}

fn unicode_escape(s: &str) -> Option<&str> {
    let digits = s.strip_prefix("\\u{")?;
    let end = digits.bytes().take(7).position(|b| b == b'}')?;
    if end == 0 || !digits.as_bytes()[..end].iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    char::from_u32(u32::from_str_radix(&digits[..end], 16).ok()?)?;
    Some(&s[..end + 4])
}

/// Escape invisible characters in a name without cutting it. Layout may ellipsize the label independently.
pub fn display_name(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if is_hidden(c) {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

/// Whether `c` changes what a line looks like without looking like anything itself: the set the panel's
/// `revealHidden` escapes (control and format characters, line and paragraph separators, every space but the plain
/// one, and the characters that render blank), with newline and tab included, since a label is one line.
pub(crate) fn is_hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c as u32,
            // Format characters.
            0xad
                | 0x600..=0x605
                | 0x61c
                | 0x6dd
                | 0x70f
                | 0x890..=0x891
                | 0x8e2
                | 0x180e
                | 0x200b..=0x200f
                | 0x202a..=0x202e
                | 0x2060..=0x2064
                | 0x2066..=0x206f
                | 0xfeff
                | 0xfff9..=0xfffb
                | 0x110bd
                | 0x110cd
                | 0x13430..=0x1343f
                | 0x1bca0..=0x1bca3
                | 0x1d173..=0x1d17a
                | 0xe0001
                | 0xe0020..=0xe007f
                // Line and paragraph separators.
                | 0x2028..=0x2029
                // Spaces other than the plain one.
                | 0xa0
                | 0x1680
                | 0x2000..=0x200a
                | 0x202f
                | 0x205f
                | 0x3000
                // Characters that render blank.
                | 0x34f
                | 0x115f..=0x1160
                | 0x17b4..=0x17b5
                | 0x180b..=0x180f
                | 0x2800
                | 0x3164
                | 0xfe00..=0xfe0f
                | 0xffa0
                | 0xe0100..=0xe01ef
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_escapes_format_and_control_characters_and_does_not_cut() {
        assert_eq!(display_name("a\u{202e}\u{200b}\nb"), "a\\u{202e}\\u{200b}\\u{a}b");
        assert_eq!(display_name(&"名".repeat(200)), "名".repeat(200));
        assert_eq!(display_name("plain <b> name"), "plain <b> name");
    }

    #[test]
    fn a_bidi_override_is_shown_as_an_escape() {
        assert_eq!(display_text("\u{202e}abc"), "\\u{202e}abc");
    }

    #[test]
    fn what_would_not_show_is_escaped_the_same_way() {
        for (hidden, shown) in [
            ('\u{feff}', "\\u{feff}"),
            ('\u{200b}', "\\u{200b}"),
            ('\u{ad}', "\\u{ad}"),
            ('\t', "\\u{9}"),
            ('\n', "\\u{a}"),
            ('\u{1b}', "\\u{1b}"),
            ('\u{7f}', "\\u{7f}"),
            ('\u{85}', "\\u{85}"),
            ('\u{2066}', "\\u{2066}"),
            ('\u{2069}', "\\u{2069}"),
            ('\u{2028}', "\\u{2028}"),
            ('\u{a0}', "\\u{a0}"),
            ('\u{fe0f}', "\\u{fe0f}"),
            ('\u{e0041}', "\\u{e0041}"),
        ] {
            assert_eq!(display_text(&format!("a{hidden}b")), format!("a{shown}b"));
        }
    }

    #[test]
    fn a_long_value_is_cut_to_eighty_characters() {
        assert_eq!(display_text(&"x".repeat(200)), "x".repeat(80));
        assert_eq!(display_text(&"中".repeat(200)).chars().count(), MAX_CHARS);
    }

    #[test]
    fn the_cut_never_splits_an_escape() {
        let text = format!("{}\u{202e}tail", "x".repeat(MAX_CHARS - 3));
        assert_eq!(display_text(&text), "x".repeat(MAX_CHARS - 3));
    }

    #[test]
    fn ordinary_text_is_untouched() {
        for text in [
            "~/.tmux.conf",
            "设置 Eitri 的窗口按键",
            "<leader>W: which-key",
            "a b",
            "",
        ] {
            assert_eq!(display_text(text), text);
        }
    }
}
