//! The text documents Neovim has open, as the bridge keeps them to open again on a server: the text with every
//! `textDocument/didChange` applied, in the position encoding the server chose.

use std::path::PathBuf;

use serde_json::Value;

/// How the `character` of a position counts: `utf-16` unless the server's `initialize` result chose another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Encoding {
    #[default]
    Utf16,
    Utf8,
    Utf32,
}

impl Encoding {
    /// From the server's `capabilities.positionEncoding`; anything but `utf-8` and `utf-32` is `utf-16`.
    pub fn from_capability(name: Option<&str>) -> Self {
        match name {
            Some("utf-8") => Self::Utf8,
            Some("utf-32") => Self::Utf32,
            _ => Self::Utf16,
        }
    }

    /// The name the protocol gives it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Utf16 => "utf-16",
            Self::Utf8 => "utf-8",
            Self::Utf32 => "utf-32",
        }
    }

    fn width(self, c: char) -> usize {
        match self {
            Self::Utf8 => c.len_utf8(),
            Self::Utf16 => c.len_utf16(),
            Self::Utf32 => 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Document {
    pub uri: String,
    pub language_id: String,
    pub version: i64,
    pub text: String,
    /// Opening order, so a replay keeps it.
    pub seq: u64,
}

impl Document {
    /// Applies one entry of `contentChanges`: a replacement of the whole text, or of a range. Positions past the end
    /// of a line or of the text are clamped to it, as the protocol says. An entry of no known shape changes nothing.
    pub fn apply_change(&mut self, change: &Value, encoding: Encoding) {
        let Some(text) = change.get("text").and_then(Value::as_str) else {
            return;
        };
        let Some(range) = change.get("range").filter(|range| !range.is_null()) else {
            self.text = text.to_owned();
            return;
        };
        let (Some(start), Some(end)) = (position(range.get("start")), position(range.get("end"))) else {
            return;
        };
        let from = offset(&self.text, start, encoding);
        let to = offset(&self.text, end, encoding).max(from);
        self.text.replace_range(from..to, text);
    }
}

fn position(value: Option<&Value>) -> Option<(u64, u64)> {
    let value = value?;
    Some((value.get("line")?.as_u64()?, value.get("character")?.as_u64()?))
}

/// The byte offset of `(line, character)` in `text`, a line ending in `\n`, `\r\n` or `\r`.
fn offset(text: &str, (line, character): (u64, u64), encoding: Encoding) -> usize {
    let bytes = text.as_bytes();
    let mut start = 0usize;
    let mut seen = 0u64;
    while seen < line {
        match bytes[start..].iter().position(|&b| b == b'\n' || b == b'\r') {
            None => return text.len(),
            Some(at) => {
                start += at;
                start += if bytes[start] == b'\r' && bytes.get(start + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
                seen += 1;
            }
        }
    }
    let mut at = start;
    let mut counted = 0usize;
    let wanted = usize::try_from(character).unwrap_or(usize::MAX);
    for c in text[start..].chars() {
        if c == '\n' || c == '\r' || counted >= wanted {
            break;
        }
        // A character that would straddle the wanted column is not split.
        counted += encoding.width(c);
        at += c.len_utf8();
    }
    at
}

/// `text` with its `%XX` escapes decoded; `None` for a malformed escape or bytes that are not UTF-8.
pub fn percent_decode(text: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(text.len());
    let raw = text.as_bytes();
    let mut at = 0;
    while at < raw.len() {
        if raw[at] == b'%' {
            let hex = std::str::from_utf8(raw.get(at + 1..at + 3)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            bytes.push(raw[at]);
            at += 1;
        }
    }
    String::from_utf8(bytes).ok()
}

/// The path of a `file://` URI, percent-decoded; `None` for any other URI, or one with a host that is not this one.
pub fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    if !path.starts_with('/') {
        return None;
    }
    let path = percent_decode(path)?;
    (!path.contains('\0')).then(|| PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(text: &str) -> Document {
        Document {
            uri: "file:///srv/project/a.rs".into(),
            language_id: "rust".into(),
            version: 1,
            text: text.into(),
            seq: 0,
        }
    }

    #[test]
    fn an_edit_counts_a_pair_as_two_in_utf16_and_as_one_in_utf32() {
        let change =
            json!({"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 4}}, "text": "XY"});
        let mut utf16 = doc("a😀b\nz");
        utf16.apply_change(&change, Encoding::Utf16);
        assert_eq!(utf16.text, "a😀XY\nz");
        let mut utf32 = doc("a😀bb\nz");
        utf32.apply_change(&change, Encoding::Utf32);
        assert_eq!(utf32.text, "a😀bXY\nz");
    }

    #[test]
    fn positions_past_the_end_are_clamped() {
        let mut d = doc("one\r\ntwo\n");
        let tail =
            json!({"range": {"start": {"line": 1, "character": 99}, "end": {"line": 9, "character": 0}}, "text": "!"});
        d.apply_change(&tail, Encoding::Utf16);
        assert_eq!(d.text, "one\r\ntwo!");
    }

    #[test]
    fn a_file_uri_is_decoded_and_others_are_not_paths() {
        assert_eq!(
            file_uri_path("file:///srv/my%20project/a%C3%A9.rs"),
            Some(PathBuf::from("/srv/my project/aé.rs"))
        );
        assert_eq!(file_uri_path("file://other/srv"), None);
        assert_eq!(file_uri_path("eitri-remote://h/srv"), None);
        assert_eq!(file_uri_path("file:///a%00"), None);
    }
}
