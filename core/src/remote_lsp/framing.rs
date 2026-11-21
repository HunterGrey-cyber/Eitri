//! LSP's wire framing on Neovim's stdio, and the chunking of one message on the helper's stream.
//!
//! `Content-Length` headers are bounded at [`HEADERS_MAX`] bytes and the body at the reader's own cap. A body past the
//! cap is never buffered: it is skipped as it arrives, and what is known of it (its id, its method and the document it
//! names) is scanned out of the bytes that pass, so the request it was can still be answered. A header that cannot be read
//! ends the stream ([`FrameError`]): there is no way to find where the next message starts.

use std::fmt;

use agent::remote_client::terminal_transport::wire::LSP_CHUNK_MAX;

/// The most header bytes one message may have.
pub const HEADERS_MAX: usize = 8 * 1024;

/// How much of a skipped body's `id`, `method` or document uri is kept; a longer one is not one a peer sends (a path is
/// at most 4096 bytes).
const PROBE_TOKEN_MAX: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    HeadersTooLong,
    NoContentLength,
    BadHeader(String),
    BadLength(String),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeadersTooLong => write!(f, "the headers are longer than {HEADERS_MAX} bytes"),
            Self::NoContentLength => write!(f, "the headers have no Content-Length"),
            Self::BadHeader(line) => write!(f, "a header line is not `Name: value`: {line:?}"),
            Self::BadLength(value) => write!(f, "Content-Length is not a number of bytes: {value:?}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// What is known of a message body that was skipped for its size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// The body's `id` as JSON text (`12`, `"a"`), if it had one.
    pub id: Option<String>,
    /// The body's `method`, if it had one.
    pub method: Option<String>,
    /// The body's `params.textDocument.uri`, if it had one: the document a notification was about.
    pub uri: Option<String>,
    /// The declared length of the body.
    pub length: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Message(Vec<u8>),
    Oversized(Probe),
}

enum State {
    Headers,
    Body {
        length: usize,
    },
    Skipping {
        length: usize,
        left: usize,
        scan: Box<Scan>,
    },
}

/// Reads framed messages out of bytes pushed as they arrive. Push, then call [`FrameReader::next_frame`] until it gives
/// `None`: a skipped body only drains as `next_frame` runs.
pub struct FrameReader {
    max_body: usize,
    buf: Vec<u8>,
    state: State,
}

impl FrameReader {
    pub fn new(max_body: usize) -> Self {
        Self {
            max_body,
            buf: Vec::new(),
            state: State::Headers,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next message, `None` when more bytes are needed. After an error the reader is not to be used again.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        loop {
            match &mut self.state {
                State::Headers => {
                    let Some(end) = find_blank_line(&self.buf) else {
                        if self.buf.len() > HEADERS_MAX {
                            return Err(FrameError::HeadersTooLong);
                        }
                        return Ok(None);
                    };
                    if end > HEADERS_MAX {
                        return Err(FrameError::HeadersTooLong);
                    }
                    let length = content_length(&self.buf[..end])?;
                    self.buf.drain(..end + 4);
                    self.state = if length > self.max_body {
                        State::Skipping {
                            length,
                            left: length,
                            scan: Box::default(),
                        }
                    } else {
                        State::Body { length }
                    };
                }
                State::Body { length } => {
                    if self.buf.len() < *length {
                        return Ok(None);
                    }
                    let body: Vec<u8> = self.buf.drain(..*length).collect();
                    self.state = State::Headers;
                    return Ok(Some(Frame::Message(body)));
                }
                State::Skipping { length, left, scan } => {
                    let take = (*left).min(self.buf.len());
                    scan.feed(&self.buf[..take]);
                    self.buf.drain(..take);
                    *left -= take;
                    if *left > 0 {
                        return Ok(None);
                    }
                    let probe = Probe {
                        id: scan.id.take(),
                        method: scan.method.take(),
                        uri: scan.uri.take(),
                        length: *length,
                    };
                    self.state = State::Headers;
                    return Ok(Some(Frame::Oversized(probe)));
                }
            }
        }
    }
}

fn find_blank_line(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

/// The `Content-Length` of a header block (without its closing blank line). Other headers (`Content-Type`) are read
/// for their shape and ignored; the name is case-insensitive.
fn content_length(headers: &[u8]) -> Result<usize, FrameError> {
    let text = std::str::from_utf8(headers).map_err(|_| FrameError::BadHeader("not UTF-8".into()))?;
    let mut length = None;
    for line in text.split("\r\n") {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| FrameError::BadHeader(line.chars().take(80).collect()))?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            let value = value.trim();
            let parsed = value
                .bytes()
                .all(|b| b.is_ascii_digit())
                .then(|| value.parse::<usize>().ok())
                .flatten()
                .ok_or_else(|| FrameError::BadLength(value.chars().take(40).collect()))?;
            if length.replace(parsed).is_some() {
                return Err(FrameError::BadHeader("Content-Length twice".into()));
            }
        }
    }
    length.ok_or(FrameError::NoContentLength)
}

/// One message with its header, as nvim reads it.
pub fn encode_frame(body: &[u8]) -> Vec<u8> {
    let mut framed = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    framed.extend_from_slice(body);
    framed
}

/// `message` in pieces of at most [`LSP_CHUNK_MAX`] bytes, each with whether it is the last.
pub fn chunks(message: &[u8]) -> impl Iterator<Item = (&[u8], bool)> {
    let count = message.len().div_ceil(LSP_CHUNK_MAX).max(1);
    (0..count).map(move |at| {
        let from = at * LSP_CHUNK_MAX;
        (
            &message[from..(from + LSP_CHUNK_MAX).min(message.len())],
            at + 1 == count,
        )
    })
}

/// A message past the reassembler's cap.
#[derive(Debug, PartialEq, Eq)]
pub struct TooLarge;

/// Puts one message's chunks back together up to a cap.
pub struct Reassembler {
    cap: usize,
    buf: Vec<u8>,
}

impl Reassembler {
    pub fn new(cap: usize) -> Self {
        Self { cap, buf: Vec::new() }
    }

    /// The whole message once its last chunk is in.
    pub fn push(&mut self, data: &[u8], last: bool) -> Result<Option<Vec<u8>>, TooLarge> {
        if self.buf.len().saturating_add(data.len()) > self.cap {
            self.buf = Vec::new();
            return Err(TooLarge);
        }
        self.buf.extend_from_slice(data);
        Ok(last.then(|| std::mem::take(&mut self.buf)))
    }
}

/// Finds the top-level `id` and `method` of a JSON object that passes by in pieces, and its `params.textDocument.uri`,
/// keeping nothing else: which containers are open (as far as the uri's depth), the member keys on the way there,
/// string state and at most [`PROBE_TOKEN_MAX`] bytes of one value. The members of a message can come in any order, so
/// the whole body has to go by.
#[derive(Default)]
struct Scan {
    started: bool,
    done: bool,
    /// The open containers down to [`PATH_DEPTH`], `true` for an object; deeper ones are only counted in `deep`.
    open: Vec<bool>,
    deep: u64,
    /// The current member key of each object in `open` (empty for an array).
    keys: Vec<Vec<u8>>,
    /// In an object of `open`, the next string is a key.
    expect_key: bool,
    in_string: bool,
    escaped: bool,
    /// The string being read is a key of an object in `open`.
    string_is_key: bool,
    /// A number or literal is being read.
    scalar: bool,
    /// The value being read is kept: a key, or a value at one of the wanted places.
    gather: Option<Want>,
    token: Vec<u8>,
    overlong: bool,
    id: Option<String>,
    method: Option<String>,
    uri: Option<String>,
}

/// How deep the scan follows keys: `params.textDocument.uri` is a member of the third object down.
const PATH_DEPTH: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    Key,
    Id,
    Method,
    Uri,
}

impl Scan {
    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.step(byte);
        }
    }

    fn step(&mut self, byte: u8) {
        if self.done {
            return;
        }
        if self.in_string {
            self.in_string_byte(byte);
            return;
        }
        if !self.started {
            if byte == b'{' {
                self.started = true;
                self.push_container(true);
            }
            return;
        }
        if self.scalar {
            if !matches!(byte, b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n') {
                self.push_token(byte);
                return;
            }
            self.finish_value();
        }
        match byte {
            b'"' => self.begin_string(),
            b'{' | b'[' => self.push_container(byte == b'{'),
            b'}' | b']' => self.pop_container(),
            b':' => self.expect_key = false,
            b',' => self.expect_key = self.deep == 0 && self.open.last() == Some(&true),
            b' ' | b'\t' | b'\r' | b'\n' => {}
            _ => {
                self.scalar = true;
                self.begin_token(self.wanted());
                self.push_token(byte);
            }
        }
    }

    fn push_container(&mut self, object: bool) {
        if self.deep > 0 || self.open.len() >= PATH_DEPTH {
            self.deep += 1;
            return;
        }
        self.open.push(object);
        self.keys.push(Vec::new());
        self.expect_key = object;
    }

    fn pop_container(&mut self) {
        self.expect_key = false;
        if self.deep > 0 {
            self.deep -= 1;
            return;
        }
        self.open.pop();
        self.keys.pop();
        self.done = self.open.is_empty();
    }

    fn begin_token(&mut self, gather: Option<Want>) {
        self.gather = gather;
        self.token.clear();
        self.overlong = false;
    }

    fn begin_string(&mut self) {
        self.in_string = true;
        self.escaped = false;
        self.string_is_key = self.deep == 0 && self.expect_key;
        if self.string_is_key {
            self.begin_token(Some(Want::Key));
        } else {
            self.begin_token(self.wanted());
            self.push_token(b'"');
        }
    }

    fn in_string_byte(&mut self, byte: u8) {
        if self.escaped {
            self.escaped = false;
        } else if byte == b'\\' {
            self.escaped = true;
        } else if byte == b'"' {
            self.in_string = false;
            if self.string_is_key {
                let key = if self.overlong {
                    Vec::new()
                } else {
                    std::mem::take(&mut self.token)
                };
                if let Some(last) = self.keys.last_mut() {
                    *last = key;
                }
                self.gather = None;
                self.expect_key = false;
                return;
            }
            self.push_token(byte);
            self.finish_value();
            return;
        }
        self.push_token(byte);
    }

    fn push_token(&mut self, byte: u8) {
        if self.gather.is_none() {
            return;
        }
        if self.token.len() >= PROBE_TOKEN_MAX {
            self.overlong = true;
        } else {
            self.token.push(byte);
        }
    }

    /// Which kept value, if any, a value starting here is.
    fn wanted(&self) -> Option<Want> {
        if self.deep > 0 || !self.open.iter().all(|&object| object) {
            return None;
        }
        let path: Vec<&[u8]> = self.keys.iter().map(Vec::as_slice).collect();
        match path.as_slice() {
            [b"id"] => Some(Want::Id),
            [b"method"] => Some(Want::Method),
            [b"params", b"textDocument", b"uri"] => Some(Want::Uri),
            _ => None,
        }
    }

    /// The end of a scalar or string value: kept when it is one of the wanted ones and whole.
    fn finish_value(&mut self) {
        self.scalar = false;
        let gather = self.gather.take();
        if self.overlong {
            return;
        }
        let text = String::from_utf8_lossy(&self.token).into_owned();
        // A method and a uri are strings: their JSON text is decoded.
        let decoded = || serde_json::from_str::<String>(&text).ok();
        match gather {
            Some(Want::Id) => self.id = Some(text),
            Some(Want::Method) => self.method = decoded(),
            Some(Want::Uri) => self.uri = decoded(),
            _ => {}
        }
    }
}

/// What a probe finds in a whole body that could not be parsed: its top-level `id` and `method`, and
/// `params.textDocument.uri`, as far as they can be told.
pub fn probe(body: &[u8]) -> Probe {
    let mut scan = Scan::default();
    scan.feed(body);
    Probe {
        id: scan.id,
        method: scan.method,
        uri: scan.uri,
        length: body.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_of(body: &str) -> Probe {
        let mut reader = FrameReader::new(1);
        reader.push(&encode_frame(body.as_bytes()));
        match reader.next_frame().unwrap().unwrap() {
            Frame::Oversized(probe) => probe,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_probe_finds_the_id_wherever_it_sits() {
        let probe = probe_of(r#"{"params":{"id":9,"a":[1,{"id":8}],"s":"}\"{"},"method":"m/x","id":12}"#);
        assert_eq!(
            (probe.id.as_deref(), probe.method.as_deref()),
            (Some("12"), Some("m/x"))
        );
        let probe = probe_of(r#"{"id":"a\"b","jsonrpc":"2.0","result":{"x":1}}"#);
        assert_eq!((probe.id.as_deref(), probe.method), (Some(r#""a\"b""#), None));
        let probe = probe_of(r#"{"jsonrpc":"2.0","method":"n"}"#);
        assert_eq!((probe.id, probe.method.as_deref()), (None, Some("n")));
    }

    #[test]
    fn the_probe_finds_the_document_a_notification_names() {
        let probe = probe_of(
            r#"{"method":"textDocument/didChange","params":{"contentChanges":[{"text":"uri","uri":"no"}],
            "textDocument":{"version":3,"uri":"file:\/\/\/srv\/a%20b.rs"}},"jsonrpc":"2.0"}"#,
        );
        assert_eq!(
            (probe.method.as_deref(), probe.uri.as_deref()),
            (Some("textDocument/didChange"), Some("file:///srv/a%20b.rs"))
        );
        // A uri anywhere else is not the document's.
        let probe =
            probe_of(r#"{"method":"m","params":{"uri":"file:///x","items":[{"textDocument":{"uri":"file:///y"}}]}}"#);
        assert_eq!(probe.uri, None);
        // A body that is not whole JSON still gives what passed.
        let found = super::probe(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":[[[[1]]]]");
        assert_eq!((found.id.as_deref(), found.method), (Some("3"), None));
    }

    #[test]
    fn chunks_cover_a_message_exactly_and_end_on_the_last() {
        let message = vec![7u8; LSP_CHUNK_MAX * 2 + 5];
        let parts: Vec<_> = chunks(&message).collect();
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|(data, _)| data.len() <= LSP_CHUNK_MAX));
        assert_eq!(
            parts.iter().map(|(_, last)| *last).collect::<Vec<_>>(),
            [false, false, true]
        );
        let mut back = Reassembler::new(message.len());
        let mut done = None;
        for (data, last) in parts {
            done = back.push(data, last).unwrap();
        }
        assert_eq!(done.unwrap(), message);
        assert_eq!(Reassembler::new(4).push(b"12345", true), Err(TooLarge));
    }
}
