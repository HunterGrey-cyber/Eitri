//! The WebSocket frame decoder and writers, with their size bounds.
//!
//! The decoder is a state machine over the stream's bytes, so what it answers never depends on how reads cut it. A
//! frame is judged at the byte that shows its fault, and a length as soon as it is whole: nothing is read of a frame
//! that cannot be accepted. UTF-8 is left to whoever parses the message.

use std::collections::VecDeque;

const FIN: u8 = 0x80;
const RESERVED: u8 = 0x70;
const OPCODE: u8 = 0x0f;
const MASKED: u8 = 0x80;
const LENGTH: u8 = 0x7f;
/// The 7-bit length values that say the length follows in 16 or 64 bits.
const LENGTH_16: u8 = 126;
const LENGTH_64: u8 = 127;
/// The longest length the 7-bit field itself can say.
const LENGTH_7_MAX: u64 = 125;

const OP_CONTINUATION: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xa;

const PROTOCOL_ERROR: u16 = 1002;
const UNSUPPORTED_DATA: u16 = 1003;
const INVALID_PAYLOAD: u16 = 1007;
const MESSAGE_TOO_BIG: u16 = 1009;

/// The most a control frame may carry.
const CONTROL_MAX: usize = 125;
/// What is kept of a message past its cap: the CLI's encoder writes a message's `id` last.
const TAIL_BYTES: usize = 256;
const KEY_BYTES: usize = 4;
/// Two fixed bytes, a 64-bit length and the mask key.
const HEAD_MAX: usize = 2 + 8 + KEY_BYTES;

/// One thing a client's stream has completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    /// One whole message, unmasked, at most `message_bytes`.
    Text(Vec<u8>),
    /// A message past `message_bytes`, within `discard_bytes`: its last 256 bytes.
    Oversized {
        tail: Vec<u8>,
    },
    Ping(Vec<u8>),
    Pong,
    /// The client's close. A code here is one an endpoint may send, so it can go back on the wire as it is; `None`: the
    /// close carried none.
    Close(Option<u16>),
    /// The close code to send; the connection ends.
    Violation(u16),
}

/// A frame whose head is whole, while its payload arrives.
#[derive(Clone, Copy)]
struct Frame {
    opcode: u8,
    fin: bool,
    key: [u8; KEY_BYTES],
    /// The payload's position so far, modulo the key: a read can end anywhere in it.
    key_at: usize,
    left: u64,
}

enum Stage {
    Head,
    Payload(Frame),
    /// After a close or a violation: whatever follows is not read.
    Ended,
}

/// The data message a frame or more has opened.
enum Message {
    None,
    Kept(Vec<u8>),
    /// Past the cap: only the last bytes, unmasked.
    Discarding(VecDeque<u8>),
}

/// State over partial frames. Grows only with what has arrived and never past `message_bytes`; the head, one control
/// frame's payload and the tail are fixed sizes on top of that.
pub struct Decoder {
    message_bytes: u64,
    discard_bytes: u64,
    stage: Stage,
    head: [u8; HEAD_MAX],
    head_len: usize,
    message: Message,
    /// Payload the open message's frames have announced so far, the current frame's whole length included.
    message_len: u64,
    control: [u8; CONTROL_MAX],
    control_len: usize,
}

impl Decoder {
    pub fn new(message_bytes: usize, discard_bytes: usize) -> Decoder {
        Decoder {
            message_bytes: message_bytes as u64,
            discard_bytes: discard_bytes as u64,
            stage: Stage::Head,
            head: [0; HEAD_MAX],
            head_len: 0,
            message: Message::None,
            message_len: 0,
            control: [0; CONTROL_MAX],
            control_len: 0,
        }
    }

    /// Takes any split of the stream; appends what completed.
    pub fn feed(&mut self, mut bytes: &[u8], out: &mut Vec<Inbound>) {
        while let Some((&next, after_next)) = bytes.split_first() {
            match self.stage {
                Stage::Ended => return,
                Stage::Head => {
                    self.head[self.head_len] = next;
                    self.head_len += 1;
                    bytes = after_next;
                    self.advance_head(out);
                }
                Stage::Payload(frame) => {
                    let taken = frame.left.min(bytes.len() as u64) as usize;
                    let (now, rest) = bytes.split_at(taken);
                    bytes = rest;
                    self.take_payload(frame, now, out);
                }
            }
        }
    }

    fn advance_head(&mut self, out: &mut Vec<Inbound>) {
        match self.judge_head() {
            Ok(None) => {}
            Err(code) => self.end(Inbound::Violation(code), out),
            Ok(Some(frame)) => {
                self.head_len = 0;
                if frame.left == 0 {
                    self.finish_frame(frame, out);
                } else {
                    self.stage = Stage::Payload(frame);
                }
            }
        }
    }

    /// Called after each byte of a head, so each check runs once, at the byte that settles it.
    fn judge_head(&mut self) -> Result<Option<Frame>, u16> {
        let first = self.head[0];
        let opcode = first & OPCODE;
        if self.head_len == 1 {
            self.judge_first_byte(first)?;
            return Ok(None);
        }

        let length_field = self.head[1] & LENGTH;
        if self.head_len == 2 {
            if self.head[1] & MASKED == 0 {
                return Err(PROTOCOL_ERROR);
            }
            if is_control(opcode) && usize::from(length_field) > CONTROL_MAX {
                return Err(PROTOCOL_ERROR);
            }
        }

        let key_from = 2 + match length_field {
            LENGTH_16 => 2,
            LENGTH_64 => 8,
            _ => 0,
        };
        if self.head_len < key_from {
            return Ok(None);
        }
        let left = self.announced_length(key_from);
        if self.head_len == key_from {
            if left >> 63 != 0 || !is_shortest_form(length_field, left) {
                return Err(PROTOCOL_ERROR);
            }
            if !is_control(opcode) {
                self.admit(opcode, left)?;
            }
        }
        if self.head_len < key_from + KEY_BYTES {
            return Ok(None);
        }

        let mut key = [0; KEY_BYTES];
        key.copy_from_slice(&self.head[key_from..key_from + KEY_BYTES]);
        Ok(Some(Frame {
            opcode,
            fin: first & FIN != 0,
            key,
            key_at: 0,
            left,
        }))
    }

    /// A binary frame is refused for what it is, wherever it comes.
    fn judge_first_byte(&self, first: u8) -> Result<(), u16> {
        if first & RESERVED != 0 {
            return Err(PROTOCOL_ERROR);
        }
        let message_open = !matches!(self.message, Message::None);
        match first & OPCODE {
            OP_BINARY => Err(UNSUPPORTED_DATA),
            OP_TEXT if message_open => Err(PROTOCOL_ERROR),
            OP_CONTINUATION if !message_open => Err(PROTOCOL_ERROR),
            OP_TEXT | OP_CONTINUATION => Ok(()),
            OP_CLOSE | OP_PING | OP_PONG if first & FIN != 0 => Ok(()),
            _ => Err(PROTOCOL_ERROR),
        }
    }

    /// The length bytes are all in the head here.
    fn announced_length(&self, key_from: usize) -> u64 {
        match self.head[2..key_from] {
            [] => u64::from(self.head[1] & LENGTH),
            ref bytes => bytes.iter().fold(0, |length, byte| (length << 8) | u64::from(*byte)),
        }
    }

    /// Counts a data frame into its message by its announced length. Every frame's length is known before any of its
    /// payload, so a message crosses either cap here and never in the middle of a frame.
    fn admit(&mut self, opcode: u8, length: u64) -> Result<(), u16> {
        if opcode == OP_TEXT {
            self.message = Message::Kept(Vec::new());
            self.message_len = 0;
        }
        self.message_len = self.message_len.saturating_add(length);
        if self.message_len > self.discard_bytes {
            return Err(MESSAGE_TOO_BIG);
        }
        if self.message_len > self.message_bytes {
            if let Message::Kept(kept) = &self.message {
                let mut tail = VecDeque::with_capacity(TAIL_BYTES);
                tail.extend(&kept[kept.len().saturating_sub(TAIL_BYTES)..]);
                self.message = Message::Discarding(tail);
            }
        }
        Ok(())
    }

    fn take_payload(&mut self, mut frame: Frame, now: &[u8], out: &mut Vec<Inbound>) {
        if is_control(frame.opcode) {
            let from = self.control_len;
            self.control_len += now.len();
            let room = &mut self.control[from..self.control_len];
            room.copy_from_slice(now);
            unmask(room, frame.key, frame.key_at);
        } else {
            match &mut self.message {
                Message::Kept(kept) => {
                    grow_within(kept, now.len(), self.message_bytes as usize);
                    let from = kept.len();
                    kept.extend_from_slice(now);
                    unmask(&mut kept[from..], frame.key, frame.key_at);
                }
                Message::Discarding(tail) => {
                    // What cannot reach the tail is not even unmasked.
                    let skipped = now.len().saturating_sub(TAIL_BYTES);
                    for (offset, byte) in now[skipped..].iter().enumerate() {
                        if tail.len() == TAIL_BYTES {
                            tail.pop_front();
                        }
                        tail.push_back(byte ^ frame.key[(frame.key_at + skipped + offset) % KEY_BYTES]);
                    }
                }
                // The frame's head opened the message.
                Message::None => {}
            }
        }

        frame.key_at = (frame.key_at + now.len()) % KEY_BYTES;
        frame.left -= now.len() as u64;
        if frame.left == 0 {
            self.finish_frame(frame, out);
        } else {
            self.stage = Stage::Payload(frame);
        }
    }

    fn finish_frame(&mut self, frame: Frame, out: &mut Vec<Inbound>) {
        self.stage = Stage::Head;
        let control_len = std::mem::take(&mut self.control_len);
        match frame.opcode {
            OP_PING => out.push(Inbound::Ping(self.control[..control_len].to_vec())),
            OP_PONG => out.push(Inbound::Pong),
            OP_CLOSE => {
                let last = judge_close(&self.control[..control_len]);
                self.end(last, out);
            }
            _ if frame.fin => match std::mem::replace(&mut self.message, Message::None) {
                Message::Kept(text) => out.push(Inbound::Text(text)),
                Message::Discarding(tail) => out.push(Inbound::Oversized { tail: tail.into() }),
                Message::None => {}
            },
            _ => {}
        }
    }

    fn end(&mut self, last: Inbound, out: &mut Vec<Inbound>) {
        self.stage = Stage::Ended;
        self.message = Message::None;
        out.push(last);
    }

    /// The bytes that grow with what a client sends.
    #[cfg(test)]
    fn held_bytes(&self) -> usize {
        match &self.message {
            Message::None => 0,
            Message::Kept(kept) => kept.capacity(),
            Message::Discarding(tail) => tail.capacity(),
        }
    }
}

fn is_control(opcode: u8) -> bool {
    opcode & 0x8 != 0
}

/// A length must use the smallest field that holds it.
fn is_shortest_form(length_field: u8, length: u64) -> bool {
    match length_field {
        LENGTH_16 => length > LENGTH_7_MAX,
        LENGTH_64 => length > u64::from(u16::MAX),
        _ => true,
    }
}

/// The reason is read here because nothing past this point sees it.
fn judge_close(body: &[u8]) -> Inbound {
    match body {
        [] => Inbound::Close(None),
        [_] => Inbound::Violation(PROTOCOL_ERROR),
        [high, low, reason @ ..] => {
            let code = u16::from_be_bytes([*high, *low]);
            if !may_be_sent(code) {
                Inbound::Violation(PROTOCOL_ERROR)
            } else if std::str::from_utf8(reason).is_err() {
                Inbound::Violation(INVALID_PAYLOAD)
            } else {
                Inbound::Close(Some(code))
            }
        }
    }
}

/// RFC 6455's codes and IANA's later 1012 to 1014, then the registered and private ranges. 1005, 1006 and 1015 only
/// name what an endpoint saw locally and never go on the wire.
fn may_be_sent(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999)
}

/// `Vec`'s own doubling could leave a buffer of up to twice the cap around a message within it.
fn grow_within(kept: &mut Vec<u8>, more: usize, cap: usize) {
    let needed = kept.len() + more;
    if needed > kept.capacity() {
        let target = needed.max(kept.capacity() * 2).min(cap);
        kept.reserve_exact(target - kept.len());
    }
}

fn unmask(payload: &mut [u8], key: [u8; KEY_BYTES], key_at: usize) {
    for (offset, byte) in payload.iter_mut().enumerate() {
        *byte ^= key[(key_at + offset) % KEY_BYTES];
    }
}

/// A server's frames are never masked, and these are never fragmented.
fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEAD_MAX + payload.len());
    bytes.push(FIN | opcode);
    match payload.len() {
        length @ 0..=125 => bytes.push(length as u8),
        length @ 126..=65535 => {
            bytes.push(LENGTH_16);
            bytes.extend((length as u16).to_be_bytes());
        }
        length => {
            bytes.push(LENGTH_64);
            bytes.extend((length as u64).to_be_bytes());
        }
    }
    bytes.extend_from_slice(payload);
    bytes
}

pub fn text_frame(payload: &[u8]) -> Vec<u8> {
    frame(OP_TEXT, payload)
}

/// Cut at 125 bytes, past which it would not be a control frame a client can accept. A ping that came through
/// [`Decoder`] never reaches the cut.
pub fn pong_frame(payload: &[u8]) -> Vec<u8> {
    frame(OP_PONG, &payload[..payload.len().min(CONTROL_MAX)])
}

pub fn close_frame(code: u16) -> Vec<u8> {
    frame(OP_CLOSE, &code.to_be_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc_count::bytes_allocated_by;
    use crate::ide_server::Limits;

    const FINAL_TEXT: u8 = 0x81;
    const FIRST_TEXT: u8 = 0x01;
    const CONTINUE: u8 = 0x00;
    const FINAL_CONTINUE: u8 = 0x80;
    const PING: u8 = 0x89;
    const PONG: u8 = 0x8a;
    const CLOSE: u8 = 0x88;
    const KEY: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];

    /// The smallest length field for `length`, with the mask bit set, and the key.
    fn head(first: u8, length: usize, key: [u8; 4]) -> Vec<u8> {
        let mut head = vec![first];
        match length {
            0..=125 => head.push(0x80 | length as u8),
            126..=65535 => {
                head.push(0x80 | 126);
                head.extend((length as u16).to_be_bytes());
            }
            _ => {
                head.push(0x80 | 127);
                head.extend((length as u64).to_be_bytes());
            }
        }
        head.extend(key);
        head
    }

    fn masked(first: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = head(first, payload.len(), KEY);
        frame.extend(payload.iter().enumerate().map(|(at, byte)| byte ^ KEY[at % 4]));
        frame
    }

    fn pattern(length: usize) -> Vec<u8> {
        (0..length).map(|at| (at % 251) as u8).collect()
    }

    fn decode_with(message_bytes: usize, discard_bytes: usize, stream: &[u8]) -> Vec<Inbound> {
        let mut out = Vec::new();
        Decoder::new(message_bytes, discard_bytes).feed(stream, &mut out);
        out
    }

    fn decode(stream: &[u8]) -> Vec<Inbound> {
        decode_with(1 << 20, 1 << 21, stream)
    }

    fn concat(frames: &[Vec<u8>]) -> Vec<u8> {
        frames.concat()
    }

    #[test]
    fn an_unmasked_frame_ends_the_connection() {
        assert_eq!(decode(&[FINAL_TEXT, 2, b'h', b'i']), [Inbound::Violation(1002)]);
        assert_eq!(decode(&[PING, 0]), [Inbound::Violation(1002)]);
        assert_eq!(
            decode(&[FINAL_TEXT, 0]),
            [Inbound::Violation(1002)],
            "an empty frame needs its mask bit too"
        );
    }

    #[test]
    fn a_message_arrives_whole_from_any_split() {
        let stream = concat(&[
            masked(FINAL_TEXT, b"hello"),
            masked(FIRST_TEXT, b"one "),
            masked(PING, b"mid"),
            masked(CONTINUE, b"two "),
            masked(FINAL_CONTINUE, b"three"),
            masked(FINAL_TEXT, b""),
            masked(PONG, b"ignored"),
            masked(CLOSE, &1000u16.to_be_bytes()),
        ]);
        let expected = vec![
            Inbound::Text(b"hello".to_vec()),
            Inbound::Ping(b"mid".to_vec()),
            Inbound::Text(b"one two three".to_vec()),
            Inbound::Text(Vec::new()),
            Inbound::Pong,
            Inbound::Close(Some(1000)),
        ];
        assert_eq!(decode(&stream), expected);

        let mut decoder = Decoder::new(1 << 20, 1 << 21);
        let mut out = Vec::new();
        for byte in &stream {
            decoder.feed(std::slice::from_ref(byte), &mut out);
        }
        assert_eq!(out, expected, "byte by byte");

        for first in 0..=stream.len() {
            for second in first..=stream.len() {
                let mut decoder = Decoder::new(1 << 20, 1 << 21);
                let mut out = Vec::new();
                decoder.feed(&stream[..first], &mut out);
                decoder.feed(&stream[first..second], &mut out);
                decoder.feed(&stream[second..], &mut out);
                assert_eq!(out, expected, "split at {first} and {second}");
            }
        }
    }

    #[test]
    fn a_violation_is_the_same_from_any_split() {
        let stream = concat(&[masked(FINAL_TEXT, b"kept"), vec![0x82, 0x80]]);
        let mut decoder = Decoder::new(1024, 2048);
        let mut out = Vec::new();
        for byte in &stream {
            decoder.feed(std::slice::from_ref(byte), &mut out);
        }
        assert_eq!(out, [Inbound::Text(b"kept".to_vec()), Inbound::Violation(1003)]);
        assert_eq!(out, decode_with(1024, 2048, &stream));
    }

    #[test]
    fn lengths_of_7_16_and_64_bits() {
        for length in [0usize, 1, 125, 126, 127, 65535, 65536, 70000] {
            let payload = pattern(length);
            assert_eq!(
                decode(&masked(FINAL_TEXT, &payload)),
                [Inbound::Text(payload)],
                "length {length}"
            );
        }
    }

    #[test]
    fn a_length_in_more_bytes_than_it_needs_is_a_violation() {
        for length in [2u16, 125] {
            let mut stream = vec![FINAL_TEXT, 0x80 | 126];
            stream.extend(length.to_be_bytes());
            assert_eq!(
                decode(&stream),
                [Inbound::Violation(1002)],
                "{length} fits the 7-bit field; judged before the key arrives"
            );
        }
        for length in [2u64, 125, 126, 65535] {
            let mut stream = vec![FINAL_TEXT, 0x80 | 127];
            stream.extend(length.to_be_bytes());
            assert_eq!(
                decode(&stream),
                [Inbound::Violation(1002)],
                "{length} fits a shorter field"
            );
        }
    }

    #[test]
    fn a_64_bit_length_with_its_top_bit_is_a_violation() {
        let mut stream = vec![FINAL_TEXT, 0x80 | 127];
        stream.extend((1u64 << 63).to_be_bytes());
        assert_eq!(
            decode(&stream),
            [Inbound::Violation(1002)],
            "judged before the key arrives"
        );

        let mut stream = vec![FINAL_TEXT, 0x80 | 127];
        stream.extend(u64::MAX.to_be_bytes());
        assert_eq!(decode(&stream), [Inbound::Violation(1002)]);
    }

    #[test]
    fn a_message_over_the_cap_keeps_only_its_last_256_bytes() {
        let payload = pattern(2000);
        assert_eq!(
            decode_with(1024, 4096, &masked(FINAL_TEXT, &payload)),
            [Inbound::Oversized {
                tail: payload[payload.len() - 256..].to_vec()
            }],
        );

        let at_the_cap = pattern(1024);
        assert_eq!(
            decode_with(1024, 4096, &masked(FINAL_TEXT, &at_the_cap)),
            [Inbound::Text(at_the_cap)],
            "exactly at the cap is an ordinary message",
        );

        let one_past = pattern(1025);
        assert_eq!(
            decode_with(1024, 4096, &masked(FINAL_TEXT, &one_past)),
            [Inbound::Oversized {
                tail: one_past[1025 - 256..].to_vec()
            }],
        );
    }

    /// The last 256 bytes can lie in bytes already kept when a later fragment takes the message over the cap.
    #[test]
    fn the_tail_reaches_back_into_what_was_kept() {
        let payload = pattern(1100);
        let stream = concat(&[
            masked(FIRST_TEXT, &payload[..1000]),
            masked(FINAL_CONTINUE, &payload[1000..]),
        ]);
        assert_eq!(
            decode_with(1024, 4096, &stream),
            [Inbound::Oversized {
                tail: payload[1100 - 256..].to_vec()
            }]
        );
    }

    #[test]
    fn a_tail_shorter_than_256_bytes_is_the_whole_message() {
        let payload = pattern(40);
        assert_eq!(
            decode_with(16, 4096, &masked(FINAL_TEXT, &payload)),
            [Inbound::Oversized { tail: payload.clone() }],
        );
    }

    /// The discard skips what falls out of the tail, so where each byte sits against the key has to survive that.
    #[test]
    fn a_discarded_tail_unmasks_from_any_chunk_size() {
        let payload = pattern(3001);
        let tail = payload[3001 - 256..].to_vec();
        let stream = masked(FINAL_TEXT, &payload);
        for chunk in [1, 2, 3, 5, 7, 255, 256, 257, 511, 1021] {
            let mut decoder = Decoder::new(1024, 4096);
            let mut out = Vec::new();
            for part in stream.chunks(chunk) {
                decoder.feed(part, &mut out);
            }
            assert_eq!(out, [Inbound::Oversized { tail: tail.clone() }], "chunks of {chunk}");
        }
    }

    #[test]
    fn the_connection_goes_on_after_an_oversized_message() {
        let stream = concat(&[masked(FINAL_TEXT, &pattern(2000)), masked(FINAL_TEXT, b"next")]);
        let found = decode_with(1024, 4096, &stream);
        assert_eq!(found.len(), 2);
        assert!(matches!(found[0], Inbound::Oversized { .. }));
        assert_eq!(found[1], Inbound::Text(b"next".to_vec()));
    }

    #[test]
    fn a_message_past_the_hard_cap_is_a_violation() {
        let at_the_cap = pattern(4096);
        assert_eq!(
            decode_with(1024, 4096, &masked(FINAL_TEXT, &at_the_cap)),
            [Inbound::Oversized {
                tail: at_the_cap[4096 - 256..].to_vec()
            }],
            "exactly at the hard cap is still answered",
        );
        assert_eq!(
            decode_with(1024, 4096, &masked(FINAL_TEXT, &pattern(4097))),
            [Inbound::Violation(1009)]
        );
        assert_eq!(
            decode_with(1024, 4096, &head(FINAL_TEXT, 1 << 40, KEY)),
            [Inbound::Violation(1009)],
            "an announced length is enough: nothing of it needs to arrive",
        );

        let across_fragments = concat(&[
            masked(FIRST_TEXT, &pattern(3000)),
            masked(CONTINUE, &pattern(1000)),
            masked(FINAL_CONTINUE, &pattern(97)),
        ]);
        assert_eq!(decode_with(1024, 4096, &across_fragments), [Inbound::Violation(1009)]);
    }

    #[test]
    fn fragments_are_one_message_under_one_cap() {
        let payload = pattern(30);
        let stream = concat(&[
            masked(FIRST_TEXT, &payload[..10]),
            masked(CONTINUE, &[]),
            masked(CONTINUE, &payload[10..20]),
            masked(FINAL_CONTINUE, &payload[20..]),
        ]);
        assert_eq!(decode_with(30, 60, &stream), [Inbound::Text(payload.clone())]);
        assert_eq!(
            decode_with(29, 60, &stream),
            [Inbound::Oversized { tail: payload }],
            "no fragment passes the cap alone: the message does",
        );
    }

    #[test]
    fn a_ping_between_fragments_is_passed_at_once() {
        let mut decoder = Decoder::new(1024, 4096);
        let mut out = Vec::new();
        decoder.feed(&concat(&[masked(FIRST_TEXT, b"one "), masked(PING, b"now")]), &mut out);
        assert_eq!(
            out,
            [Inbound::Ping(b"now".to_vec())],
            "the message is still open and the ping does not wait"
        );
        decoder.feed(&masked(FINAL_CONTINUE, b"two"), &mut out);
        assert_eq!(
            out,
            [Inbound::Ping(b"now".to_vec()), Inbound::Text(b"one two".to_vec())]
        );
    }

    #[test]
    fn a_ping_inside_an_oversized_message_is_passed_at_once() {
        let stream = concat(&[
            masked(FIRST_TEXT, &pattern(2000)),
            masked(PING, b"now"),
            masked(FINAL_CONTINUE, b"end"),
        ]);
        let found = decode_with(1024, 4096, &stream);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0], Inbound::Ping(b"now".to_vec()));
        let mut tail = pattern(2000)[2000 - 253..].to_vec();
        tail.extend(b"end");
        assert_eq!(found[1], Inbound::Oversized { tail });
    }

    #[test]
    fn a_long_control_frame_is_a_violation() {
        let longest = pattern(125);
        assert_eq!(decode(&masked(PING, &longest)), [Inbound::Ping(longest)]);
        for first in [PING, PONG, CLOSE] {
            assert_eq!(
                decode(&masked(first, &pattern(126))),
                [Inbound::Violation(1002)],
                "{first:#x}"
            );
            assert_eq!(
                decode(&masked(first, &pattern(70000))),
                [Inbound::Violation(1002)],
                "{first:#x}"
            );
        }
    }

    #[test]
    fn a_fragmented_control_frame_is_a_violation() {
        for first in [PING & 0x7f, PONG & 0x7f, CLOSE & 0x7f] {
            assert_eq!(decode(&masked(first, b"x")), [Inbound::Violation(1002)], "{first:#x}");
        }
    }

    #[test]
    fn a_reserved_bit_is_a_violation() {
        for bit in [0x10u8, 0x20, 0x40] {
            assert_eq!(
                decode(&masked(FINAL_TEXT | bit, b"x")),
                [Inbound::Violation(1002)],
                "{bit:#x}"
            );
            assert_eq!(
                decode(&masked(PING | bit, b"x")),
                [Inbound::Violation(1002)],
                "{bit:#x}"
            );
        }
    }

    #[test]
    fn a_binary_frame_is_a_violation() {
        assert_eq!(decode(&masked(0x82, b"x")), [Inbound::Violation(1003)]);
        assert_eq!(
            decode(&masked(0x02, b"x")),
            [Inbound::Violation(1003)],
            "the first of several"
        );
    }

    #[test]
    fn a_binary_frame_inside_a_message_is_refused_as_binary() {
        let stream = concat(&[masked(FIRST_TEXT, b"open"), masked(0x82, b"x")]);
        assert_eq!(decode(&stream), [Inbound::Violation(1003)]);
    }

    #[test]
    fn the_first_byte_is_judged_before_the_second() {
        assert_eq!(
            decode(&[0x82 | 0x40, 0x80]),
            [Inbound::Violation(1002)],
            "a reserved bit before the opcode"
        );
        assert_eq!(
            decode(&[0x82]),
            [Inbound::Violation(1003)],
            "the opcode needs nothing after it"
        );
        assert_eq!(decode(&[0x82, 0]), [Inbound::Violation(1003)], "before the mask bit");
    }

    #[test]
    fn an_unknown_opcode_is_a_violation() {
        for opcode in (3u8..=7).chain(0xb..=0xf) {
            assert_eq!(
                decode(&masked(0x80 | opcode, b"x")),
                [Inbound::Violation(1002)],
                "{opcode:#x}"
            );
        }
    }

    #[test]
    fn a_stray_continuation_is_a_violation() {
        assert_eq!(decode(&masked(FINAL_CONTINUE, b"x")), [Inbound::Violation(1002)]);
        assert_eq!(decode(&masked(CONTINUE, b"x")), [Inbound::Violation(1002)]);

        let after_a_whole_message = concat(&[masked(FINAL_TEXT, b"done"), masked(CONTINUE, b"x")]);
        assert_eq!(
            decode(&after_a_whole_message),
            [Inbound::Text(b"done".to_vec()), Inbound::Violation(1002)]
        );
    }

    #[test]
    fn a_new_message_inside_an_open_one_is_a_violation() {
        for first in [FIRST_TEXT, FINAL_TEXT] {
            let stream = concat(&[masked(FIRST_TEXT, b"open"), masked(first, b"x")]);
            assert_eq!(decode(&stream), [Inbound::Violation(1002)], "{first:#x}");
        }
    }

    #[test]
    fn a_close_carries_its_code() {
        assert_eq!(decode(&masked(CLOSE, b"")), [Inbound::Close(None)]);
        assert_eq!(
            decode(&masked(CLOSE, &1001u16.to_be_bytes())),
            [Inbound::Close(Some(1001))]
        );

        let mut with_a_reason = 4000u16.to_be_bytes().to_vec();
        with_a_reason.extend(b"bye");
        assert_eq!(decode(&masked(CLOSE, &with_a_reason)), [Inbound::Close(Some(4000))]);
    }

    #[test]
    fn a_close_code_no_endpoint_may_send_is_a_violation() {
        for code in [0u16, 999, 1004, 1005, 1006, 1015, 1016, 2999, 5000, u16::MAX] {
            assert_eq!(
                decode(&masked(CLOSE, &code.to_be_bytes())),
                [Inbound::Violation(1002)],
                "{code}"
            );
        }
    }

    #[test]
    fn a_close_code_an_endpoint_may_send_is_passed_on() {
        for code in [1000u16, 1003, 1007, 1014, 3000, 4999] {
            assert_eq!(
                decode(&masked(CLOSE, &code.to_be_bytes())),
                [Inbound::Close(Some(code))],
                "{code}"
            );
        }
    }

    #[test]
    fn a_close_reason_that_is_not_utf_8_is_a_violation() {
        let close = masked(CLOSE, &[0x03, 0xe8, 0xff]);
        assert_eq!(decode(&close), [Inbound::Violation(1007)]);

        let mut decoder = Decoder::new(1024, 4096);
        let mut out = Vec::new();
        for byte in &close {
            decoder.feed(std::slice::from_ref(byte), &mut out);
        }
        assert_eq!(out, [Inbound::Violation(1007)], "byte by byte");

        let mut with_a_reason = 1000u16.to_be_bytes().to_vec();
        with_a_reason.extend("再见".as_bytes());
        assert_eq!(decode(&masked(CLOSE, &with_a_reason)), [Inbound::Close(Some(1000))]);
    }

    #[test]
    fn a_one_byte_close_is_a_violation() {
        assert_eq!(decode(&masked(CLOSE, b"x")), [Inbound::Violation(1002)]);
    }

    #[test]
    fn nothing_counts_after_the_end() {
        for ending in [masked(CLOSE, b""), vec![FINAL_TEXT, 0]] {
            let mut decoder = Decoder::new(1024, 4096);
            let mut out = Vec::new();
            decoder.feed(&concat(&[ending, masked(FINAL_TEXT, b"late")]), &mut out);
            assert_eq!(out.len(), 1, "{out:?}");
            decoder.feed(&masked(FINAL_TEXT, b"later"), &mut out);
            assert_eq!(out.len(), 1, "{out:?}");
            assert_eq!(
                decoder.held_bytes(),
                0,
                "an ended connection holds nothing of its message"
            );
        }
    }

    #[test]
    fn a_close_drops_a_message_left_open() {
        let stream = concat(&[masked(FIRST_TEXT, b"open"), masked(CLOSE, b"")]);
        assert_eq!(decode(&stream), [Inbound::Close(None)]);
    }

    #[test]
    fn utf_8_is_not_judged_here() {
        assert_eq!(
            decode(&masked(FINAL_TEXT, &[0xff, 0xfe])),
            [Inbound::Text(vec![0xff, 0xfe])]
        );
    }

    /// Fragments, so that the first `message_bytes` really are kept before the rest is thrown away.
    #[test]
    fn the_decoder_holds_no_more_than_the_cap() {
        let limits = Limits::default();
        const FRAGMENT: usize = 64 * 1024;
        let fragments = limits.discard_bytes / FRAGMENT;
        assert_eq!(fragments * FRAGMENT, limits.discard_bytes);

        let key = [0u8; 4];
        let chunk = [b'x'; 4096];
        let mut decoder = Decoder::new(limits.message_bytes, limits.discard_bytes);
        let mut out = Vec::new();
        let mut allocated = 0;
        let mut allocated_past_the_cap = 0;
        for index in 0..fragments {
            let first = match index {
                0 => FIRST_TEXT,
                last if last + 1 == fragments => FINAL_CONTINUE,
                _ => CONTINUE,
            };
            let frame_head = head(first, FRAGMENT, key);
            allocated += bytes_allocated_by(|| decoder.feed(&frame_head, &mut out)).1;
            for _ in 0..FRAGMENT / chunk.len() {
                let (_, bytes) = bytes_allocated_by(|| decoder.feed(&chunk, &mut out));
                allocated += bytes;
                if index * FRAGMENT > limits.message_bytes {
                    allocated_past_the_cap += bytes;
                }
                assert!(
                    decoder.held_bytes() <= limits.message_bytes,
                    "{} held",
                    decoder.held_bytes()
                );
            }
        }

        assert_eq!(out.len(), 1);
        assert_eq!(out[0], Inbound::Oversized { tail: vec![b'x'; 256] });
        assert!(
            allocated >= limits.message_bytes,
            "the first 8 MiB were kept, and counted: {allocated}"
        );
        assert!(
            allocated < 3 * limits.message_bytes,
            "growth, not one buffer for each chunk: {allocated}"
        );
        assert!(
            allocated_past_the_cap < 4096,
            "the discard allocates nothing per chunk: {allocated_past_the_cap}"
        );
    }

    #[test]
    fn a_kept_message_never_grows_past_the_cap() {
        const CAP: usize = 100_000;
        let mut decoder = Decoder::new(CAP, CAP * 2);
        let mut out = Vec::new();
        decoder.feed(&head(FINAL_TEXT, CAP, [0; 4]), &mut out);
        let byte = [b'x'; 1];
        for _ in 0..CAP - 1 {
            decoder.feed(&byte, &mut out);
            assert!(decoder.held_bytes() <= CAP, "{} held", decoder.held_bytes());
        }
        decoder.feed(&byte, &mut out);
        assert!(
            out == [Inbound::Text(vec![b'x'; CAP])],
            "{} items, not one whole message",
            out.len()
        );
    }

    #[test]
    fn a_silent_client_pins_nothing_it_only_announced() {
        let mut decoder = Decoder::new(1 << 20, 1 << 21);
        let mut out = Vec::new();
        decoder.feed(&head(FINAL_TEXT, 1 << 20, KEY), &mut out);
        assert_eq!(decoder.held_bytes(), 0);
    }

    #[test]
    fn writers_send_final_unmasked_frames() {
        assert_eq!(text_frame(b"hi"), [FINAL_TEXT, 2, b'h', b'i']);
        assert_eq!(pong_frame(b"hi"), [PONG, 2, b'h', b'i']);
        assert_eq!(close_frame(1002), [CLOSE, 2, 0x03, 0xea]);
        assert_eq!(text_frame(b""), [FINAL_TEXT, 0]);
    }

    #[test]
    fn text_frames_use_the_length_their_payload_needs() {
        assert_eq!(text_frame(&pattern(125))[..2], [FINAL_TEXT, 125]);
        assert_eq!(text_frame(&pattern(126))[..4], [FINAL_TEXT, 126, 0, 126]);
        assert_eq!(text_frame(&pattern(65535))[..4], [FINAL_TEXT, 126, 0xff, 0xff]);
        let long = text_frame(&pattern(65536));
        assert_eq!(long[..2], [FINAL_TEXT, 127]);
        assert_eq!(long[2..10], 65536u64.to_be_bytes());
        assert_eq!(long[10..], pattern(65536));
    }

    #[test]
    fn a_pong_never_passes_the_control_frame_limit() {
        assert_eq!(pong_frame(&pattern(200))[..2], [PONG, 125]);
        assert_eq!(pong_frame(&pattern(200)).len(), 127);
    }
}
