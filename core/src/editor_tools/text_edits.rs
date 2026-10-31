//! Pure edits against file bytes, with positions counted in the language server's units.

use crate::editor_lines::{expected_in_buffer, to_buffer_lines, FileFormat};
use sha2::{Digest, Sha256};

/// Units used by a server's zero-based character positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16,
    Utf32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    /// Zero-based (line, character) in the original text.
    pub start: (u32, u32),
    /// Exclusive end in the original text, in the same units as `start`.
    pub end: (u32, u32),
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    NotText,
    InsideCharacter { line: u32 },
    Reversed { line: u32 },
    Overlap { line: u32 },
}

/// Apply all ranges against `before` without reading or writing anything. Untouched bytes keep their line endings.
pub fn apply_text_edits(before: &[u8], edits: &[TextEdit], encoding: Encoding) -> Result<Vec<u8>, EditError> {
    let text = std::str::from_utf8(before).map_err(|_| EditError::NotText)?;
    if before.contains(&0) || edits.iter().any(|edit| edit.new_text.contains('\0')) {
        return Err(EditError::NotText);
    }

    let lines = protocol_lines(text);
    let mut ranges = Vec::with_capacity(edits.len());
    for edit in edits {
        let start = byte_offset(text, &lines, edit.start, encoding)?;
        let end = byte_offset(text, &lines, edit.end, encoding)?;
        if end < start {
            return Err(EditError::Reversed { line: edit.start.0 });
        }
        ranges.push((start, end, edit));
    }
    // Stable sorting keeps coincident insertions in their array order when applied from the back.
    ranges.sort_by_key(|&(start, _, _)| start);
    for pair in ranges.windows(2) {
        if pair[1].0 < pair[0].1 {
            return Err(EditError::Overlap {
                line: pair[1].2.start.0,
            });
        }
    }

    let mut after = before.to_vec();
    for (start, end, edit) in ranges.into_iter().rev() {
        after.splice(start..end, edit.new_text.bytes());
    }
    Ok(after)
}

/// Byte ranges of protocol lines, excluding their terminators. A final terminator leaves an empty line at EOF.
fn protocol_lines(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'\r' | b'\n' => {
                lines.push((start, at));
                if bytes[at] == b'\r' && bytes.get(at + 1) == Some(&b'\n') {
                    at += 1;
                }
                at += 1;
                start = at;
            }
            _ => at += 1,
        }
    }
    lines.push((start, bytes.len()));
    lines
}

fn byte_offset(
    text: &str,
    lines: &[(usize, usize)],
    (line, character): (u32, u32),
    encoding: Encoding,
) -> Result<usize, EditError> {
    let Some(&(start, end)) = lines.get(line as usize) else {
        return Ok(text.len());
    };
    let text = &text[start..end];
    let mut remaining = character as usize;
    match encoding {
        Encoding::Utf8 => {
            let offset = remaining.min(text.len());
            if !text.is_char_boundary(offset) {
                return Err(EditError::InsideCharacter { line });
            }
            Ok(start + offset)
        }
        Encoding::Utf16 | Encoding::Utf32 => {
            for (offset, ch) in text.char_indices() {
                if remaining == 0 {
                    return Ok(start + offset);
                }
                let width = if encoding == Encoding::Utf16 { ch.len_utf16() } else { 1 };
                if remaining < width {
                    return Err(EditError::InsideCharacter { line });
                }
                remaining -= width;
            }
            Ok(end)
        }
    }
}

/// SHA-256 hex of the buffer's lines joined by LF. A line the format cannot hold unchanged gives no digest.
pub fn buffer_digest(bytes: &[u8], format: FileFormat) -> Option<String> {
    let file_lines: Vec<_> = bytes.split_inclusive(|&b| b == b'\n').map(<[u8]>::to_vec).collect();
    let mut hash = Sha256::new();
    for (index, line) in to_buffer_lines(&file_lines).iter().enumerate() {
        let held = expected_in_buffer(line, format)?;
        if index != 0 {
            hash.update(b"\n");
        }
        hash.update(held);
    }
    Some(format!("{:x}", hash.finalize()))
}

/// Whether a buffer whose `'endofline'` is `eol` agrees with `bytes` about the file's end, which [`buffer_digest`]
/// cannot see: `a` and `a\n` have one line each. An empty file agrees either way: it has no last line to end, and nvim
/// reads it with `'endofline'` set.
pub fn final_newline_agrees(bytes: &[u8], eol: bool) -> bool {
    bytes.is_empty() || bytes.ends_with(b"\n") == eol
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn edit(start: (u32, u32), end: (u32, u32), new_text: &str) -> TextEdit {
        TextEdit {
            start,
            end,
            new_text: new_text.into(),
        }
    }

    fn apply(before: &[u8], edits: &[TextEdit]) -> Result<Vec<u8>, EditError> {
        apply_text_edits(before, edits, Encoding::Utf8)
    }

    #[test]
    fn a_one_line_edit() {
        assert_eq!(
            apply(b"hello world", &[edit((0, 6), (0, 11), "you")]),
            Ok(b"hello you".to_vec())
        );
    }

    #[test]
    fn two_edits_on_one_line_keep_their_positions() {
        assert_eq!(
            apply(b"abcdef", &[edit((0, 1), (0, 2), "LONG"), edit((0, 4), (0, 6), "Z")]),
            Ok(b"aLONGcdZ".to_vec())
        );
    }

    #[test]
    fn an_insert_and_a_delete_at_one_position_keep_array_order() {
        assert_eq!(
            apply(
                b"abcd",
                &[
                    edit((0, 1), (0, 1), "first"),
                    edit((0, 1), (0, 1), "second"),
                    edit((0, 1), (0, 3), ""),
                ]
            ),
            Ok(b"afirstsecondd".to_vec())
        );
    }

    #[test]
    fn edits_in_any_order_give_the_same_result() {
        let edits = [
            edit((0, 1), (0, 2), "LONG"),
            edit((1, 1), (1, 3), ""),
            edit((0, 4), (0, 4), "!"),
        ];
        for order in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
            let ordered = order.map(|i| edits[i].clone());
            assert_eq!(apply(b"abcde\nfgh", &ordered), Ok(b"aLONGcd!e\nf".to_vec()));
        }
    }

    #[test]
    fn touching_ranges_are_fine() {
        assert_eq!(
            apply(b"abcdef", &[edit((0, 1), (0, 3), "X"), edit((0, 3), (0, 5), "Y")]),
            Ok(b"aXYf".to_vec())
        );
    }

    #[test]
    fn an_overlap_is_refused_naming_the_line() {
        for edits in [
            vec![edit((0, 1), (1, 2), "X"), edit((1, 1), (1, 3), "Y")],
            vec![edit((1, 0), (1, 3), "X"), edit((1, 1), (1, 1), "Y")],
            vec![edit((1, 1), (1, 3), "X"), edit((1, 1), (1, 1), "Y")],
        ] {
            assert_eq!(apply(b"abc\ndef", &edits), Err(EditError::Overlap { line: 1 }));
        }
    }

    #[test]
    fn a_reversed_range_is_refused() {
        for range in [edit((1, 2), (1, 1), ""), edit((1, 0), (0, 2), "")] {
            assert_eq!(apply(b"abc\ndef", &[range]), Err(EditError::Reversed { line: 1 }));
        }
    }

    #[test]
    fn each_encoding_counts_its_own_units() {
        for (encoding, start) in [(Encoding::Utf8, 6), (Encoding::Utf16, 3), (Encoding::Utf32, 2)] {
            assert_eq!(
                apply_text_edits("é𝒳xy".as_bytes(), &[edit((0, start), (0, start + 1), "!")], encoding),
                Ok("é𝒳!y".as_bytes().to_vec()),
                "{encoding:?}"
            );
        }
    }

    #[test]
    fn a_position_inside_a_surrogate_pair_is_refused() {
        for range in [edit((1, 2), (1, 3), ""), edit((1, 0), (1, 2), "")] {
            assert_eq!(
                apply_text_edits("a\né𝒳z".as_bytes(), &[range], Encoding::Utf16),
                Err(EditError::InsideCharacter { line: 1 })
            );
        }
    }

    #[test]
    fn a_position_inside_a_utf8_character_is_refused() {
        for character in [1, 3, 4, 5] {
            for range in [edit((1, character), (1, 6), ""), edit((1, 0), (1, character), "")] {
                assert_eq!(
                    apply("a\né𝒳z".as_bytes(), &[range]),
                    Err(EditError::InsideCharacter { line: 1 })
                );
            }
        }
    }

    #[test]
    fn a_character_past_the_end_is_the_line_end() {
        for encoding in [Encoding::Utf8, Encoding::Utf16, Encoding::Utf32] {
            assert_eq!(
                apply_text_edits("é𝒳\nz".as_bytes(), &[edit((0, u32::MAX), (0, u32::MAX), "!")], encoding),
                Ok("é𝒳!\nz".as_bytes().to_vec())
            );
        }
    }

    #[test]
    fn a_line_past_the_end_is_the_file_end() {
        for before in [b"abc".as_slice(), b"abc\n", b"abc\r\n", b"abc\r", b""] {
            for encoding in [Encoding::Utf8, Encoding::Utf16, Encoding::Utf32] {
                let mut expected = before.to_vec();
                expected.push(b'!');
                assert_eq!(
                    apply_text_edits(before, &[edit((u32::MAX, 2), (u32::MAX, 9), "!")], encoding),
                    Ok(expected)
                );
            }
        }
    }

    #[test]
    fn crlf_lines_keep_their_ends_outside_the_edit() {
        assert_eq!(
            apply(
                b"abc\r\ndef\r\n",
                &[edit((0, 1), (0, 99), "X"), edit((1, 0), (1, 1), "Y")]
            ),
            Ok(b"aX\r\nYef\r\n".to_vec())
        );
    }

    #[test]
    fn a_lone_cr_ends_a_line() {
        assert_eq!(
            apply(
                b"abc\rdef\r\nghi\njkl\r",
                &[edit((1, 1), (1, 99), "X"), edit((3, 0), (3, 1), "Y")]
            ),
            Ok(b"abc\rdX\r\nghi\nYkl\r".to_vec())
        );
        assert_eq!(apply(b"abc\r", &[edit((1, 0), (1, 0), "!")]), Ok(b"abc\r!".to_vec()));
    }

    #[test]
    fn an_edit_across_a_line_end() {
        for before in [b"abc\ndef".as_slice(), b"abc\r\ndef", b"abc\rdef"] {
            assert_eq!(apply(before, &[edit((0, 1), (1, 2), "X\nY")]), Ok(b"aX\nYf".to_vec()));
        }
    }

    #[test]
    fn a_new_final_newline_is_kept() {
        assert_eq!(apply(b"abc", &[edit((0, 3), (0, 3), "\n")]), Ok(b"abc\n".to_vec()));
        assert_eq!(apply(b"abc\n", &[edit((1, 0), (1, 0), "\n")]), Ok(b"abc\n\n".to_vec()));
    }

    #[test]
    fn invalid_utf8_is_not_text() {
        for edits in [vec![], vec![edit((0, 0), (1, 0), "valid")]] {
            assert_eq!(apply(b"a\xff", &edits), Err(EditError::NotText));
        }
    }

    #[test]
    fn a_nul_is_not_text() {
        for edits in [vec![], vec![edit((0, 0), (1, 0), "valid")]] {
            assert_eq!(apply(b"a\0b", &edits), Err(EditError::NotText));
        }
    }

    #[test]
    fn a_nul_in_new_text_is_refused() {
        assert_eq!(apply(b"abc", &[edit((0, 1), (0, 2), "x\0y")]), Err(EditError::NotText));
    }

    #[test]
    fn no_edits_give_the_same_bytes() {
        for before in [b"".as_slice(), "é𝒳".as_bytes(), b"a\r\nb\nc\rd", b"\r\n\r\n"] {
            assert_eq!(apply(before, &[]), Ok(before.to_vec()));
        }
    }

    #[test]
    fn the_digest_of_lf_crlf_mixed_and_noeol_files_matches_the_joined_lines() {
        for (bytes, format, joined) in [
            (b"a\nb\n".as_slice(), FileFormat::Unix, b"a\nb".as_slice()),
            (b"a\r\nb\r\n", FileFormat::Dos, b"a\nb"),
            (b"a\r\nb\n", FileFormat::Unix, b"a\r\nb"),
            (b"a\nb", FileFormat::Unix, b"a\nb"),
            (b"a\r\nb", FileFormat::Dos, b"a\nb"),
            (b"a\rb\r", FileFormat::Unix, b"a\rb\r"),
            (b"\n\n", FileFormat::Unix, b"\n"),
        ] {
            assert_eq!(
                buffer_digest(bytes, format),
                Some(format!("{:x}", Sha256::digest(joined)))
            );
        }
        assert_eq!(buffer_digest(b"a\n", FileFormat::Other), None);
    }

    #[test]
    fn an_lf_line_in_a_dos_buffer_has_no_digest() {
        assert_eq!(buffer_digest(b"a\nb\n", FileFormat::Dos), None);
        assert_eq!(buffer_digest(b"a\r\nb\nc", FileFormat::Dos), None);
    }

    #[test]
    fn the_digest_cannot_tell_a_final_newline_and_the_end_can() {
        for (bytes, ends) in [
            (b"a\n".as_slice(), true),
            (b"a\r\n", true),
            (b"\n", true),
            (b"a", false),
            (b"a\r", false),
        ] {
            assert!(final_newline_agrees(bytes, ends), "{bytes:?}");
            assert!(!final_newline_agrees(bytes, !ends), "{bytes:?}");
        }
        assert_eq!(
            buffer_digest(b"a\n", FileFormat::Unix),
            buffer_digest(b"a", FileFormat::Unix)
        );
    }

    #[test]
    fn an_empty_file_agrees_about_its_end_either_way() {
        assert!(final_newline_agrees(b"", true));
        assert!(final_newline_agrees(b"", false));
    }

    #[test]
    fn an_empty_file_has_the_empty_digest() {
        for format in [FileFormat::Unix, FileFormat::Dos] {
            assert_eq!(
                buffer_digest(b"", format).as_deref(),
                Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
            );
        }
    }
}
