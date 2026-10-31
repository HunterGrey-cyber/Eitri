//! The bridge socket's frames and messages, between `eitri-editor-bridge` and the window serving a tab.
//!
//! The binary includes this file by path, so it uses `std` and `serde_json` and nothing else: the binary links
//! neither `eitri_core` nor anything of a window's.
//!
//! A frame is a big-endian `u32` length, then that many bytes of JSON. The length is read as a claim: over
//! [`MAX_FRAME`] is an error before a byte of the body is read or kept, so a peer can make a reader hold at most one
//! frame. JSON nesting stays under `serde_json`'s own limit, so a frame of a million `[` is a refusal and no deep
//! recursion.

use std::fmt;
use std::io::{self, ErrorKind, Read, Write};

use serde_json::{json, Map, Value};

/// The version a `hello` states; a host of another version closes the connection.
pub const PROTOCOL: u32 = 1;

/// The most a frame's JSON may come to, in bytes.
pub const MAX_FRAME: usize = 1024 * 1024;

const LENGTH_BYTES: usize = 4;

#[derive(Debug)]
pub enum FrameError {
    /// A length over [`MAX_FRAME`], read from a frame or found writing one.
    TooLong(usize),
    /// The stream ended inside a frame.
    Truncated,
    /// A body that is not one JSON value.
    NotJson(String),
    /// A frame that is JSON and no message of this wire.
    NotAMessage(String),
    /// A [`FrameBuf`] pushed to after it failed: the stream is out of step for good.
    Broken,
    Io(io::Error),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::TooLong(len) => write!(f, "a frame of {len} bytes is over the {MAX_FRAME}-byte cap"),
            FrameError::Truncated => write!(f, "the stream ended inside a frame"),
            FrameError::NotJson(why) => write!(f, "a frame is not JSON: {why}"),
            FrameError::NotAMessage(why) => write!(f, "a frame is not a message: {why}"),
            FrameError::Broken => write!(f, "the stream is out of step after an earlier bad frame"),
            FrameError::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<io::Error> for FrameError {
    fn from(error: io::Error) -> FrameError {
        FrameError::Io(error)
    }
}

/// `value` as one frame's bytes, for a writer that cannot block on [`write_frame`].
pub fn encode_frame(value: &Value) -> Result<Vec<u8>, FrameError> {
    let body = serde_json::to_vec(value).map_err(|error| FrameError::NotJson(error.to_string()))?;
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLong(body.len()));
    }
    let mut frame = Vec::with_capacity(LENGTH_BYTES + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Writes one frame at once, or nothing at all when it would pass the cap.
pub fn write_frame(writer: &mut impl Write, value: &Value) -> Result<(), FrameError> {
    writer.write_all(&encode_frame(value)?)?;
    Ok(())
}

/// Blocks for one frame. `Ok(None)` is the stream's end between frames; an end anywhere else is `Truncated`.
pub fn read_frame(reader: &mut impl Read) -> Result<Option<Value>, FrameError> {
    let mut head = [0u8; LENGTH_BYTES];
    let mut got = 0;
    while got < LENGTH_BYTES {
        match reader.read(&mut head[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(FrameError::Truncated),
            Ok(count) => got += count,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    let len = claimed_length(head)?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).map_err(|error| match error.kind() {
        ErrorKind::UnexpectedEof => FrameError::Truncated,
        _ => FrameError::Io(error),
    })?;
    parse_body(&body).map(Some)
}

fn claimed_length(head: [u8; LENGTH_BYTES]) -> Result<usize, FrameError> {
    let len = u32::from_be_bytes(head) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLong(len));
    }
    Ok(len)
}

fn parse_body(body: &[u8]) -> Result<Value, FrameError> {
    serde_json::from_slice(body).map_err(|error| FrameError::NotJson(error.to_string()))
}

/// Frames out of a non-blocking stream: what `read` gave goes in, the frames it completed come out. It keeps at most
/// one unfinished frame, so its size is bounded by [`MAX_FRAME`] and the size of one push.
///
/// An error ends it: every later push is [`FrameError::Broken`], since a stream that lied once has no next frame
/// anyone can find. The caller closes the connection.
#[derive(Default)]
pub struct FrameBuf {
    bytes: Vec<u8>,
    broken: bool,
}

impl FrameBuf {
    pub fn new() -> FrameBuf {
        FrameBuf::default()
    }

    pub fn push(&mut self, more: &[u8]) -> Result<Vec<Value>, FrameError> {
        if self.broken {
            return Err(FrameError::Broken);
        }
        self.bytes.extend_from_slice(more);
        let mut frames = Vec::new();
        let mut used = 0;
        loop {
            match self.next_frame(used) {
                Ok(Some((frame, end))) => {
                    frames.push(frame);
                    used = end;
                }
                Ok(None) => break,
                Err(error) => {
                    self.broken = true;
                    self.bytes = Vec::new();
                    return Err(error);
                }
            }
        }
        self.bytes.drain(..used);
        Ok(frames)
    }

    /// The frame starting at `start` and where it ends, if all of it is here.
    fn next_frame(&self, start: usize) -> Result<Option<(Value, usize)>, FrameError> {
        let rest = &self.bytes[start..];
        let Some(head) = rest.first_chunk::<LENGTH_BYTES>() else {
            return Ok(None);
        };
        let len = claimed_length(*head)?;
        let Some(body) = rest.get(LENGTH_BYTES..LENGTH_BYTES + len) else {
            return Ok(None);
        };
        Ok(Some((parse_body(body)?, start + LENGTH_BYTES + len)))
    }
}

/// Every message of the wire. The bridge sends `Hello`, `Call` and `Cancel`; the host sends `Welcome` and `Result`.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    Hello {
        v: u32,
        key: String,
    },
    /// `tools` are MCP tool objects, `{name, description, inputSchema, annotations?}`, served as they are.
    Welcome {
        tools: Vec<Value>,
    },
    /// `id` is the bridge's own and unique on this bridge; `meta` is the request's `params._meta` as it came.
    Call {
        id: u64,
        name: String,
        arguments: Value,
        meta: Value,
    },
    Result {
        id: u64,
        text: String,
        is_error: bool,
    },
    Cancel {
        id: u64,
    },
}

impl Message {
    pub fn into_value(self) -> Value {
        match self {
            Message::Hello { v, key } => json!({"t": "hello", "v": v, "key": key}),
            Message::Welcome { tools } => json!({"t": "welcome", "tools": tools}),
            Message::Call {
                id,
                name,
                arguments,
                meta,
            } => {
                json!({"t": "call", "id": id, "name": name, "arguments": arguments, "meta": meta})
            }
            Message::Result { id, text, is_error } => {
                json!({"t": "result", "id": id, "text": text, "is_error": is_error})
            }
            Message::Cancel { id } => json!({"t": "cancel", "id": id}),
        }
    }

    /// Reads a frame by the shape of each message: a field missing or of another type is an error, and so is an
    /// unknown `t`. Both ends of a connection are one build's, or a `hello` of another version already closed it.
    pub fn parse(frame: Value) -> Result<Message, FrameError> {
        let Value::Object(mut fields) = frame else {
            return Err(not_a_message("not an object"));
        };
        let kind = take_string(&mut fields, "t")?;
        match kind.as_str() {
            "hello" => {
                let v = take_number(&fields, "v")?;
                let v = u32::try_from(v).map_err(|_| not_a_message("v is out of range"))?;
                Ok(Message::Hello {
                    v,
                    key: take_string(&mut fields, "key")?,
                })
            }
            "welcome" => match fields.remove("tools") {
                Some(Value::Array(tools)) => Ok(Message::Welcome { tools }),
                _ => Err(not_a_message("tools is not an array")),
            },
            "call" => {
                let id = take_number(&fields, "id")?;
                let name = take_string(&mut fields, "name")?;
                let arguments = fields.remove("arguments").filter(Value::is_object);
                let arguments = arguments.ok_or_else(|| not_a_message("arguments is not an object"))?;
                let meta = fields.remove("meta").unwrap_or(Value::Null);
                Ok(Message::Call {
                    id,
                    name,
                    arguments,
                    meta,
                })
            }
            "result" => {
                let id = take_number(&fields, "id")?;
                let text = take_string(&mut fields, "text")?;
                let is_error = fields.get("is_error").and_then(Value::as_bool);
                let is_error = is_error.ok_or_else(|| not_a_message("is_error is not a boolean"))?;
                Ok(Message::Result { id, text, is_error })
            }
            "cancel" => Ok(Message::Cancel {
                id: take_number(&fields, "id")?,
            }),
            _ => Err(not_a_message("an unknown t")),
        }
    }
}

fn not_a_message(why: &str) -> FrameError {
    FrameError::NotAMessage(why.to_string())
}

fn take_string(fields: &mut Map<String, Value>, key: &str) -> Result<String, FrameError> {
    match fields.remove(key) {
        Some(Value::String(text)) => Ok(text),
        _ => Err(FrameError::NotAMessage(format!("{key} is not a string"))),
    }
}

fn take_number(fields: &Map<String, Value>, key: &str) -> Result<u64, FrameError> {
    let number = fields.get(key).and_then(Value::as_u64);
    number.ok_or_else(|| FrameError::NotAMessage(format!("{key} is not a number")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame_of(body: &[u8]) -> Vec<u8> {
        let mut frame = (body.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(body);
        frame
    }

    /// Serves `bytes`, then fails the test if anything reads further.
    struct NoReadPast<'a> {
        bytes: &'a [u8],
    }

    impl Read for NoReadPast<'_> {
        fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
            assert!(
                !self.bytes.is_empty(),
                "read past the bytes that were meant to be enough"
            );
            let count = into.len().min(self.bytes.len());
            into[..count].copy_from_slice(&self.bytes[..count]);
            self.bytes = &self.bytes[count..];
            Ok(count)
        }
    }

    #[test]
    fn a_frame_over_the_cap_is_an_error_before_it_is_read() {
        let head = ((MAX_FRAME + 1) as u32).to_be_bytes();
        let error = read_frame(&mut NoReadPast { bytes: &head }).expect_err("over the cap");
        assert!(
            matches!(error, FrameError::TooLong(len) if len == MAX_FRAME + 1),
            "{error}"
        );

        let mut buffer = FrameBuf::new();
        let error = buffer.push(&head).expect_err("a length alone is enough to refuse");
        assert!(matches!(error, FrameError::TooLong(_)), "{error}");
    }

    #[test]
    fn the_largest_length_a_header_can_state_is_refused_without_an_allocation() {
        let error = read_frame(&mut NoReadPast {
            bytes: &u32::MAX.to_be_bytes(),
        })
        .expect_err("over the cap");
        assert!(matches!(error, FrameError::TooLong(_)), "{error}");
    }

    #[test]
    fn a_frame_of_exactly_the_cap_passes_both_readers() {
        let value = Value::String("x".repeat(MAX_FRAME - 2));
        let frame = encode_frame(&value).expect("at the cap");
        assert_eq!(frame.len(), LENGTH_BYTES + MAX_FRAME);
        assert_eq!(
            read_frame(&mut Cursor::new(&frame)).expect("reads").as_ref(),
            Some(&value)
        );
        assert_eq!(FrameBuf::new().push(&frame).expect("reads"), vec![value]);
    }

    #[test]
    fn writing_over_the_cap_writes_nothing() {
        let mut out = Vec::new();
        let value = Value::String("x".repeat(MAX_FRAME));
        assert!(matches!(write_frame(&mut out, &value), Err(FrameError::TooLong(_))));
        assert!(out.is_empty());
    }

    #[test]
    fn a_split_frame_assembles() {
        let value = json!({"t": "cancel", "id": 9});
        let frame = encode_frame(&value).expect("encodes");
        let mut buffer = FrameBuf::new();
        let (last, firsts) = frame.split_last().expect("not empty");
        for byte in firsts {
            assert_eq!(buffer.push(std::slice::from_ref(byte)).expect("a part").len(), 0);
        }
        assert_eq!(buffer.push(std::slice::from_ref(last)).expect("the end"), vec![value]);
    }

    #[test]
    fn frames_in_one_push_come_out_in_order_and_a_part_of_the_next_stays() {
        let first = json!({"t": "cancel", "id": 1});
        let second = json!({"t": "cancel", "id": 2});
        let mut bytes = encode_frame(&first).expect("encodes");
        let tail = encode_frame(&second).expect("encodes");
        bytes.extend_from_slice(&tail);
        let (rest, whole) = (&bytes[bytes.len() - 3..], &bytes[..bytes.len() - 3]);
        let mut buffer = FrameBuf::new();
        assert_eq!(buffer.push(whole).expect("reads"), vec![first]);
        assert_eq!(buffer.push(rest).expect("reads"), vec![second]);
    }

    #[test]
    fn a_short_read_is_not_a_clean_end() {
        assert!(read_frame(&mut Cursor::new(Vec::new())).expect("an end").is_none());
        let inside_the_length = [0u8, 0];
        let error = read_frame(&mut Cursor::new(inside_the_length)).expect_err("cut in the length");
        assert!(matches!(error, FrameError::Truncated), "{error}");
        let frame = encode_frame(&json!({"t": "cancel", "id": 1})).expect("encodes");
        let error = read_frame(&mut Cursor::new(&frame[..frame.len() - 1])).expect_err("cut in the body");
        assert!(matches!(error, FrameError::Truncated), "{error}");
    }

    #[test]
    fn a_read_interrupted_by_a_signal_goes_on() {
        struct Interrupting<'a> {
            first: bool,
            rest: Cursor<&'a [u8]>,
        }
        impl Read for Interrupting<'_> {
            fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
                if std::mem::take(&mut self.first) {
                    return Err(ErrorKind::Interrupted.into());
                }
                self.rest.read(into)
            }
        }
        let frame = encode_frame(&json!(1)).expect("encodes");
        let mut reader = Interrupting {
            first: true,
            rest: Cursor::new(&frame),
        };
        assert_eq!(read_frame(&mut reader).expect("reads"), Some(json!(1)));
    }

    #[test]
    fn a_body_that_is_not_one_json_value_is_an_error() {
        for body in [&b""[..], b"{", b"1 2", b"\xff"] {
            let frame = frame_of(body);
            let error = read_frame(&mut Cursor::new(&frame)).expect_err("not JSON");
            assert!(matches!(error, FrameError::NotJson(_)), "{body:?}: {error}");
            let error = FrameBuf::new().push(&frame).expect_err("not JSON");
            assert!(matches!(error, FrameError::NotJson(_)), "{body:?}: {error}");
        }
    }

    #[test]
    fn nesting_as_deep_as_a_frame_can_hold_is_an_error_and_no_stack_overflow() {
        let frame = frame_of(&vec![b'['; MAX_FRAME]);
        let error = read_frame(&mut Cursor::new(&frame)).expect_err("too deep");
        assert!(matches!(error, FrameError::NotJson(_)), "{error}");
    }

    #[test]
    fn a_failed_buffer_stays_failed_and_keeps_nothing() {
        let mut buffer = FrameBuf::new();
        let mut bytes = frame_of(b"{");
        bytes.extend_from_slice(&encode_frame(&json!(1)).expect("encodes"));
        assert!(matches!(buffer.push(&bytes), Err(FrameError::NotJson(_))));
        assert_eq!(buffer.bytes.capacity(), 0);
        let good = encode_frame(&json!(1)).expect("encodes");
        assert!(matches!(buffer.push(&good), Err(FrameError::Broken)));
        assert_eq!(buffer.bytes.capacity(), 0);
    }

    #[test]
    fn every_message_keeps_its_shape_through_a_frame() {
        let messages = [
            Message::Hello {
                v: PROTOCOL,
                key: "k".into(),
            },
            Message::Welcome {
                tools: vec![json!({"name": "open_file"})],
            },
            Message::Call {
                id: u64::MAX,
                name: "open_file".into(),
                arguments: json!({"path": "a"}),
                meta: json!({"callId": "c"}),
            },
            Message::Call {
                id: 1,
                name: "x".into(),
                arguments: json!({}),
                meta: Value::Null,
            },
            Message::Result {
                id: 1,
                text: "line\n\"quoted\"".into(),
                is_error: true,
            },
            Message::Cancel { id: 1 },
        ];
        for message in messages {
            let frame = encode_frame(&message.clone().into_value()).expect("encodes");
            let value = read_frame(&mut Cursor::new(&frame)).expect("reads").expect("a frame");
            assert_eq!(Message::parse(value).expect("parses"), message);
        }
    }

    #[test]
    fn the_message_shapes_are_exact() {
        assert_eq!(
            Message::Hello { v: 1, key: "k".into() }.into_value(),
            json!({"t": "hello", "v": 1, "key": "k"})
        );
        assert_eq!(
            Message::Call {
                id: 2,
                name: "n".into(),
                arguments: json!({}),
                meta: Value::Null
            }
            .into_value(),
            json!({"t": "call", "id": 2, "name": "n", "arguments": {}, "meta": null})
        );
        assert_eq!(
            Message::Result {
                id: 2,
                text: "t".into(),
                is_error: false
            }
            .into_value(),
            json!({"t": "result", "id": 2, "text": "t", "is_error": false})
        );
    }

    #[test]
    fn a_frame_out_of_shape_is_no_message() {
        for frame in [
            json!(1),
            json!({}),
            json!({"t": "unknown"}),
            json!({"t": 1}),
            json!({"t": "hello", "key": "k"}),
            json!({"t": "hello", "v": 4294967296u64, "key": "k"}),
            json!({"t": "hello", "v": 1}),
            json!({"t": "welcome", "tools": {}}),
            json!({"t": "call", "id": -1, "name": "n", "arguments": {}}),
            json!({"t": "call", "id": 1, "name": 2, "arguments": {}}),
            json!({"t": "call", "id": 1, "name": "n", "arguments": []}),
            json!({"t": "call", "id": 1, "name": "n"}),
            json!({"t": "result", "id": 1, "text": "x"}),
            json!({"t": "result", "id": 1, "is_error": true}),
            json!({"t": "cancel", "id": "1"}),
        ] {
            assert!(
                matches!(Message::parse(frame.clone()), Err(FrameError::NotAMessage(_))),
                "{frame}"
            );
        }
    }
}
