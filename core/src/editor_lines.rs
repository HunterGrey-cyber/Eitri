//! The one conversion between a file's bytes and the lines an nvim buffer holds for them.
//!
//! A file line keeps its terminator (`\n` or `\r\n`, or none for a last line without one); an nvim
//! buffer line has none, and the buffer's `'fileformat'` and `'eol'` say what is written back. Two
//! pieces of code that each strip or append a `\r` their own way will disagree on some file, and
//! a disagreement here means a hunk drawn over the wrong text or bytes written that the file never
//! had, so every conversion in either direction goes through this module. Pure: no I/O, no nvim.
//!
//! Out of scope, failing closed: a byte-order mark and `'fileencoding'` conversion. A buffer that
//! holds text converted from another encoding does not compare equal to the file's bytes, and a
//! comparison that does not match skips or refuses.

/// How a file line ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eol {
    /// `\n`.
    Lf,
    /// `\r\n`.
    CrLf,
    /// No newline: the file's last line, with nothing after it.
    Missing,
}

/// One file line as a buffer line: `text` without its terminator, any bytes kept (invalid UTF-8
/// included), and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferLine {
    pub text: Vec<u8>,
    pub eol: Eol,
}

/// nvim's `'fileformat'` of a buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFormat {
    Unix,
    Dos,
    /// `mac`, or anything else nvim might say: no line of such a buffer is compared.
    Other,
}

/// File lines (as `split_inclusive(|b| *b == b'\n')` cuts them) as buffer lines. A line ending in
/// `\r\n` loses both bytes, one ending in `\n` loses that byte, and one with no `\n` keeps all its
/// bytes, a lone trailing `\r` included. Total: any input comes back exactly from
/// [`to_file_bytes`], even a line without a newline that is not the last.
pub fn to_buffer_lines(file_lines: &[Vec<u8>]) -> Vec<BufferLine> {
    file_lines
        .iter()
        .map(|line| {
            if let Some(text) = line.strip_suffix(b"\r\n") {
                BufferLine {
                    text: text.to_vec(),
                    eol: Eol::CrLf,
                }
            } else if let Some(text) = line.strip_suffix(b"\n") {
                BufferLine {
                    text: text.to_vec(),
                    eol: Eol::Lf,
                }
            } else {
                BufferLine {
                    text: line.clone(),
                    eol: Eol::Missing,
                }
            }
        })
        .collect()
}

/// The file bytes of `lines`: each text with its own terminator. The exact inverse of
/// [`to_buffer_lines`].
pub fn to_file_bytes(lines: &[BufferLine]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(lines.iter().map(|line| line.text.len() + 2).sum());
    for line in lines {
        bytes.extend_from_slice(&line.text);
        bytes.extend_from_slice(match line.eol {
            Eol::Lf => b"\n".as_slice(),
            Eol::CrLf => b"\r\n",
            Eol::Missing => b"",
        });
    }
    bytes
}

/// nvim's `'fileformat'` value as a [`FileFormat`].
pub fn parse_file_format(s: &str) -> FileFormat {
    match s {
        "unix" => FileFormat::Unix,
        "dos" => FileFormat::Dos,
        _ => FileFormat::Other,
    }
}

/// The text nvim holds for `line` in a buffer of `ff`; `None` when such a buffer cannot hold it
/// unchanged (writing the buffer would change the line's terminator).
///
/// - `Lf` in a `Unix` buffer and `CrLf` in a `Dos` buffer: `text`;
/// - `CrLf` in a `Unix` buffer (a file of mixed endings, which nvim reads as unix): `text` + `\r`;
/// - `Missing` in a `Unix` or `Dos` buffer: `text`;
/// - `Lf` in a `Dos` buffer, and anything in an `Other` buffer: `None`.
///
/// Where the line sits is the caller's to check: a `Missing` line matches only as the buffer's
/// last line with `'eol'` off, and an `Lf`/`CrLf` line that is the buffer's last only with `'eol'`
/// on. An empty file has no lines here while nvim holds one empty line; that, too, is the caller's.
pub fn expected_in_buffer(line: &BufferLine, ff: FileFormat) -> Option<Vec<u8>> {
    match (line.eol, ff) {
        (Eol::Lf, FileFormat::Unix)
        | (Eol::CrLf, FileFormat::Dos)
        | (Eol::Missing, FileFormat::Unix | FileFormat::Dos) => Some(line.text.clone()),
        (Eol::CrLf, FileFormat::Unix) => {
            let mut text = line.text.clone();
            text.push(b'\r');
            Some(text)
        }
        (Eol::Lf, FileFormat::Dos) | (_, FileFormat::Other) => None,
    }
}

/// The file bytes of buffer lines `lines` read from a buffer of `ff`: each line followed by `\n`
/// (`Unix`) or `\r\n` (`Dos`), except that with `ends_buffer` and `eol == false` (nvim's
/// `'noeol'`) the last line has none. `None` for an `Other` buffer; no lines are no bytes.
pub fn buffer_range_bytes(lines: &[Vec<u8>], ff: FileFormat, eol: bool, ends_buffer: bool) -> Option<Vec<u8>> {
    let terminator: &[u8] = match ff {
        FileFormat::Unix => b"\n",
        FileFormat::Dos => b"\r\n",
        FileFormat::Other => return None,
    };
    let mut bytes = Vec::with_capacity(lines.iter().map(|line| line.len() + 2).sum());
    for (at, line) in lines.iter().enumerate() {
        bytes.extend_from_slice(line);
        let last = at + 1 == lines.len();
        if !(last && ends_buffer && !eol) {
            bytes.extend_from_slice(terminator);
        }
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(parts: &[&[u8]]) -> Vec<Vec<u8>> {
        parts.iter().map(|part| part.to_vec()).collect()
    }

    fn split(bytes: &[u8]) -> Vec<Vec<u8>> {
        bytes.split_inclusive(|b| *b == b'\n').map(<[u8]>::to_vec).collect()
    }

    fn line(text: &[u8], eol: Eol) -> BufferLine {
        BufferLine {
            text: text.to_vec(),
            eol,
        }
    }

    #[test]
    fn lf_lines_lose_their_newline_and_come_back() {
        let buffer = to_buffer_lines(&lines(&[b"a\n", b"b\n"]));
        assert_eq!(buffer, vec![line(b"a", Eol::Lf), line(b"b", Eol::Lf)]);
        assert_eq!(to_file_bytes(&buffer), b"a\nb\n");
    }

    #[test]
    fn crlf_lines_lose_both_bytes_and_come_back() {
        let buffer = to_buffer_lines(&lines(&[b"a\r\n", b"b\r\n"]));
        assert_eq!(buffer, vec![line(b"a", Eol::CrLf), line(b"b", Eol::CrLf)]);
        assert_eq!(to_file_bytes(&buffer), b"a\r\nb\r\n");
    }

    #[test]
    fn a_missing_final_newline_is_its_own_eol() {
        let buffer = to_buffer_lines(&lines(&[b"a\n", b"b"]));
        assert_eq!(buffer, vec![line(b"a", Eol::Lf), line(b"b", Eol::Missing)]);
        assert_eq!(to_file_bytes(&buffer), b"a\nb");
        assert_eq!(to_buffer_lines(&lines(&[b"a\r"])), vec![line(b"a\r", Eol::Missing)]);
    }

    /// Every byte string of length 0 to 7 over `a`, `\r`, `\n` and an invalid UTF-8 byte.
    fn every_short_string() -> Vec<Vec<u8>> {
        const ALPHABET: [u8; 4] = [b'a', b'\r', b'\n', 0xff];
        let mut all = vec![Vec::new()];
        let mut last = vec![Vec::new()];
        for _ in 0..7 {
            let next: Vec<Vec<u8>> = last
                .iter()
                .flat_map(|prefix: &Vec<u8>| {
                    ALPHABET.iter().map(move |b| {
                        let mut s = prefix.clone();
                        s.push(*b);
                        s
                    })
                })
                .collect();
            all.extend(next.iter().cloned());
            last = next;
        }
        all
    }

    #[test]
    fn invalid_utf8_and_mixed_endings_round_trip() {
        let all = every_short_string();
        assert_eq!(all.len(), 21845);
        for x in &all {
            assert_eq!(&to_file_bytes(&to_buffer_lines(&split(x))), x, "{x:?}");
        }
        // A line without a newline that is not the last still round-trips.
        let odd = vec![line(b"a", Eol::Missing), line(b"b", Eol::Lf)];
        assert_eq!(
            to_buffer_lines(&split(&to_file_bytes(&odd))),
            vec![line(b"ab", Eol::Lf)]
        );
        assert_eq!(to_file_bytes(&to_buffer_lines(&lines(&[b"a", b"b\n"]))), b"ab\n");
    }

    #[test]
    fn expected_in_buffer_follows_the_rule() {
        use FileFormat::{Dos, Other, Unix};
        let cases: [(Eol, FileFormat, Option<&[u8]>); 9] = [
            (Eol::Lf, Unix, Some(b"t")),
            (Eol::Lf, Dos, None),
            (Eol::Lf, Other, None),
            (Eol::CrLf, Unix, Some(b"t\r")),
            (Eol::CrLf, Dos, Some(b"t")),
            (Eol::CrLf, Other, None),
            (Eol::Missing, Unix, Some(b"t")),
            (Eol::Missing, Dos, Some(b"t")),
            (Eol::Missing, Other, None),
        ];
        for (eol, ff, want) in cases {
            assert_eq!(
                expected_in_buffer(&line(b"t", eol), ff),
                want.map(<[u8]>::to_vec),
                "{eol:?} in {ff:?}"
            );
        }
        assert_eq!(parse_file_format("unix"), Unix);
        assert_eq!(parse_file_format("dos"), Dos);
        assert_eq!(parse_file_format("mac"), Other);
        assert_eq!(parse_file_format(""), Other);
    }

    #[test]
    fn buffer_range_bytes_is_the_inverse() {
        use FileFormat::{Dos, Other, Unix};
        let ab = lines(&[b"a", b"b"]);
        assert_eq!(buffer_range_bytes(&ab, Unix, true, false), Some(b"a\nb\n".to_vec()));
        assert_eq!(buffer_range_bytes(&ab, Dos, true, true), Some(b"a\r\nb\r\n".to_vec()));
        assert_eq!(
            buffer_range_bytes(&ab, Unix, false, true),
            Some(b"a\nb".to_vec()),
            "the buffer's end with 'eol' off"
        );
        assert_eq!(
            buffer_range_bytes(&ab, Unix, false, false),
            Some(b"a\nb\n".to_vec()),
            "'eol' only matters at the buffer's end"
        );
        assert_eq!(buffer_range_bytes(&ab, Other, true, true), None);
        assert_eq!(buffer_range_bytes(&[], Unix, false, true), Some(Vec::new()));

        // Every short file: what nvim would hold for it, read back, gives the file again.
        for x in every_short_string().iter().filter(|x| !x.is_empty()) {
            let file = to_buffer_lines(&split(x));
            let eol = file.last().is_some_and(|last| last.eol != Eol::Missing);
            for ff in [Unix, Dos] {
                let held: Option<Vec<Vec<u8>>> = file.iter().map(|l| expected_in_buffer(l, ff)).collect();
                if ff == Unix {
                    assert!(held.is_some(), "a unix buffer holds every line of {x:?}");
                }
                if let Some(held) = held {
                    assert_eq!(
                        buffer_range_bytes(&held, ff, eol, true).as_ref(),
                        Some(x),
                        "{x:?} as {ff:?}"
                    );
                }
            }
            assert!(file.iter().all(|l| expected_in_buffer(l, Other).is_none()));
        }
    }
}
