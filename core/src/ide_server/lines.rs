//! A whole text as the lines of a view's buffer, and back.
//!
//! A proposal travels as bytes and is shown as lines, and what a person accepts has to come back as the bytes the CLI
//! sent. The conversions themselves are [`crate::editor_lines`]'s; this file adds what a whole text needs on top of
//! them: one terminator for the whole text, and the values nvim's API takes and gives.

use rmpv::Value;

use crate::editor_lines::{buffer_range_bytes, to_buffer_lines, BufferLine, Eol, FileFormat};

/// What a text's lines alone do not say, for putting it back together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineShape {
    pub crlf: bool,
    pub eol: bool,
}

/// A text's lines for a buffer, and what coming back needs. CR-LF only when every terminated line has it; otherwise a
/// `\r` stays in its line, as nvim shows a file of mixed endings. An empty text is no lines with `eol: false`; nvim
/// then holds one empty line, which [`join_lines`] gives back as nothing.
pub fn split_lines(text: &[u8]) -> (Vec<Vec<u8>>, LineShape) {
    let file_lines: Vec<Vec<u8>> = text
        .split_inclusive(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    let lines = to_buffer_lines(&file_lines);
    let terminated = || lines.iter().filter(|line| line.eol != Eol::Missing);
    let crlf = terminated().next().is_some() && terminated().all(|line| line.eol == Eol::CrLf);
    let shape = LineShape {
        crlf,
        eol: lines.last().is_some_and(|line| line.eol != Eol::Missing),
    };
    let lines = lines
        .into_iter()
        .map(|BufferLine { mut text, eol }| {
            if eol == Eol::CrLf && !crlf {
                text.push(b'\r');
            }
            text
        })
        .collect();
    (lines, shape)
}

pub fn join_lines(lines: &[Vec<u8>], shape: LineShape) -> Vec<u8> {
    let format = if shape.crlf { FileFormat::Dos } else { FileFormat::Unix };
    buffer_range_bytes(lines, format, shape.eol, true).expect("only an `Other` format has no terminator")
}

/// The lines as the byte strings nvim's API takes. Moved, not copied: a large text is cut once.
pub fn line_values(lines: Vec<Vec<u8>>) -> Vec<Value> {
    lines.into_iter().map(Value::Binary).collect()
}

/// [`join_lines`] for lines as nvim gives them. `None`: a line that is not bytes.
pub fn join_values(lines: &[Value], shape: LineShape) -> Option<Vec<u8>> {
    let lines = lines.iter().map(line_bytes).collect::<Option<Vec<_>>>()?;
    Some(join_lines(&lines, shape))
}

/// nvim sends a buffer's lines as msgpack strs, whatever bytes they hold; a value this file made is a bin.
fn line_bytes(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::String(text) => Some(text.as_bytes().to_vec()),
        Value::Binary(bytes) => Some(bytes.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A msgpack str holding `bytes`, as it comes off the wire whatever the bytes are.
    fn str_on_the_wire(bytes: &[u8]) -> Value {
        let mut frame = vec![0xa0 | bytes.len() as u8];
        frame.extend_from_slice(bytes);
        rmpv::decode::read_value(&mut frame.as_slice()).expect("a msgpack str")
    }

    fn lines(of: &[&[u8]]) -> Vec<Vec<u8>> {
        of.iter().map(|line| line.to_vec()).collect()
    }

    const TEXTS: &[&[u8]] = &[
        b"",
        b"\n",
        b"\n\n",
        b"a",
        b"a\nb",
        b"a\nb\n",
        b"\r\n",
        b"a\r\nb\r\n",
        b"a\r\nb",
        b"a\r\nb\n",
        b"a\nb\r\n",
        b"a\nb\r\nc",
        b"\r",
        b"a\r",
        b"a\rb\n",
        b"a\nb\r",
        b"a\r\nb\r",
        b"not utf-8 \xff\n",
    ];

    #[test]
    fn split_and_join_are_inverse() {
        for text in TEXTS {
            let (lines, shape) = split_lines(text);
            assert_eq!(join_lines(&lines, shape), *text, "{lines:?} {shape:?}");
        }

        let unix = |eol| LineShape { crlf: false, eol };
        let dos = |eol| LineShape { crlf: true, eol };
        assert_eq!(split_lines(b""), (lines(&[]), unix(false)), "an empty text is no lines");
        assert_eq!(split_lines(b"\n"), (lines(&[b""]), unix(true)));
        assert_eq!(
            split_lines(b"a\nb"),
            (lines(&[b"a", b"b"]), unix(false)),
            "no final newline"
        );
        assert_eq!(split_lines(b"a\r\nb\r\n"), (lines(&[b"a", b"b"]), dos(true)), "CR-LF");
        assert_eq!(
            split_lines(b"a\r\nb"),
            (lines(&[b"a", b"b"]), dos(false)),
            "CR-LF, no final newline"
        );
        assert_eq!(
            split_lines(b"a\r\nb\n"),
            (lines(&[b"a\r", b"b"]), unix(true)),
            "mixed endings keep the `\\r`"
        );
        assert_eq!(
            split_lines(b"a\r"),
            (lines(&[b"a\r"]), unix(false)),
            "a lone `\\r` ends nothing"
        );
        assert_eq!(split_lines(b"a\rb\n"), (lines(&[b"a\rb"]), unix(true)));
    }

    #[test]
    fn the_empty_line_nvim_holds_for_an_empty_text_joins_to_nothing() {
        let (_, shape) = split_lines(b"");
        assert_eq!(join_lines(&[vec![]], shape), b"");
        assert_eq!(join_values(&[Value::Binary(vec![])], shape), Some(vec![]));
    }

    #[test]
    fn join_values_agrees_with_join_lines() {
        for text in TEXTS {
            let (lines, shape) = split_lines(text);
            let values = line_values(lines.clone());
            assert_eq!(
                join_values(&values, shape),
                Some(join_lines(&lines, shape)),
                "bins of {text:?}"
            );
            let strs: Vec<Value> = lines.iter().map(|line| str_on_the_wire(line)).collect();
            assert_eq!(
                join_values(&strs, shape),
                Some(join_lines(&lines, shape)),
                "strs of {text:?}"
            );
        }
    }

    #[test]
    fn line_values_moves_the_bytes_it_is_given() {
        let line = b"a line long enough to have its own allocation".to_vec();
        let held = line.as_ptr();
        let values = line_values(vec![line]);
        assert!(matches!(&values[0], Value::Binary(bytes) if bytes.as_ptr() == held));
    }

    #[test]
    fn a_value_that_is_not_bytes_joins_to_none() {
        let shape = LineShape { crlf: false, eol: true };
        for bad in [Value::Nil, Value::from(1), Value::from(true), Value::Array(vec![])] {
            assert_eq!(
                join_values(&[Value::Binary(b"a".to_vec()), bad.clone()], shape),
                None,
                "{bad}"
            );
        }
    }
}
