//! A small WebSocket client over a real loopback socket, for the tests that talk to the listener the way the CLI does:
//! it masks what it sends, and it can also send what the CLI never would. Included with
//! `#[path = "support/ide_client.rs"] mod ide_client;` by each test file that uses it, so each one uses only part of it.
#![allow(dead_code)]

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::time::Duration;

use serde_json::{json, Value};

/// How long a read waits before the test fails rather than hangs. Never an expected time.
pub const WAIT: Duration = Duration::from_secs(60);

const MASK: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];
const OP_TEXT: u8 = 0x1;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xa;
const FIN: u8 = 0x80;
const MASKED: u8 = 0x80;

/// A request's headers; `None` leaves a header out.
pub struct Upgrade<'a> {
    pub host: Option<String>,
    pub token: Option<&'a str>,
    pub origin: Option<&'a str>,
}

impl<'a> Upgrade<'a> {
    pub fn proper(port: u16, token: &'a str) -> Upgrade<'a> {
        Upgrade {
            host: Some(format!("127.0.0.1:{port}")),
            token: Some(token),
            origin: None,
        }
    }

    pub fn text(&self) -> String {
        let mut text = String::from("GET / HTTP/1.1\r\n");
        if let Some(host) = &self.host {
            text.push_str(&format!("Host: {host}\r\n"));
        }
        if let Some(origin) = self.origin {
            text.push_str(&format!("Origin: {origin}\r\n"));
        }
        if let Some(token) = self.token {
            text.push_str(&format!("X-Claude-Code-Ide-Authorization: {token}\r\n"));
        }
        text.push_str(
            "Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: mcp\r\n\r\n",
        );
        text
    }
}

/// The upgrade the CLI sends, header for header: the token, the `mcp` subprotocol and a user agent, no `Origin`.
pub fn cli_upgrade_text(port: u16, token: &str) -> String {
    format!(
        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: mcp\r\n\
         X-Claude-Code-Ide-Authorization: {token}\r\nUser-Agent: claude-code/2.1.311\r\n\r\n"
    )
}

pub fn dial(port: u16) -> TcpStream {
    let stream = TcpStream::connect(("127.0.0.1", port)).expect("the listener takes a connection");
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream
}

/// Everything until the peer closes. A reset counts as closed: the peer may close with bytes of ours unread.
pub fn read_to_end(stream: &mut TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return bytes,
            Ok(read) => bytes.extend_from_slice(&chunk[..read]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                panic!("the peer has not closed: {} bytes so far", bytes.len())
            }
            Err(_) => return bytes,
        }
    }
}

/// One client frame, masked with a fixed key.
pub fn client_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = frame_head(opcode, payload.len() as u64, MASKED);
    frame.extend_from_slice(&MASK);
    frame.extend(payload.iter().enumerate().map(|(at, byte)| byte ^ MASK[at % 4]));
    frame
}

/// A frame with no mask, which a client may not send.
pub fn unmasked_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = frame_head(opcode, payload.len() as u64, 0);
    frame.extend_from_slice(payload);
    frame
}

/// Only a head: a text frame announcing `length` bytes.
pub fn announcing(length: u64) -> Vec<u8> {
    let mut frame = frame_head(OP_TEXT, length, MASKED);
    frame.extend_from_slice(&MASK);
    frame
}

fn frame_head(opcode: u8, length: u64, mask_bit: u8) -> Vec<u8> {
    let mut head = vec![FIN | opcode];
    if length < 126 {
        head.push(mask_bit | length as u8);
    } else if length <= u64::from(u16::MAX) {
        head.push(mask_bit | 126);
        head.extend_from_slice(&(length as u16).to_be_bytes());
    } else {
        head.push(mask_bit | 127);
        head.extend_from_slice(&length.to_be_bytes());
    }
    head
}

/// `count` masked pings with nothing in them.
pub fn empty_pings(count: usize) -> Vec<u8> {
    client_frame(OP_PING, b"").repeat(count)
}

pub fn ping_frame(payload: &[u8]) -> Vec<u8> {
    client_frame(OP_PING, payload)
}

/// What came off the wire.
#[derive(Debug, PartialEq, Eq)]
pub enum Received {
    Text(Vec<u8>),
    Pong(Vec<u8>),
    Close(u16),
    /// The peer closed the socket.
    Gone,
}

pub struct IdeClient {
    pub stream: TcpStream,
    held: Vec<u8>,
    next_id: i64,
}

impl IdeClient {
    pub fn attach(port: u16, token: &str) -> IdeClient {
        let mut client = IdeClient::try_attach(port, &Upgrade::proper(port, token));
        let head = client.read_head();
        assert!(head.starts_with("HTTP/1.1 101 "), "not upgraded: {head}");
        client
    }

    /// Upgrades the way the CLI does and checks that the server selected `mcp`.
    pub fn attach_like_cli(port: u16, token: &str) -> IdeClient {
        let mut stream = dial(port);
        stream.write_all(cli_upgrade_text(port, token).as_bytes()).unwrap();
        let mut client = IdeClient {
            stream,
            held: Vec::new(),
            next_id: 1_000,
        };
        let head = client.read_head();
        assert!(head.starts_with("HTTP/1.1 101 "), "not upgraded: {head}");
        assert!(
            head.to_ascii_lowercase().contains("sec-websocket-protocol: mcp"),
            "mcp not selected: {head}"
        );
        client
    }

    /// Whether something can be read now, or arrives within `within`. A closed socket counts: the read then says so.
    /// Never an expected time: the caller asks again.
    pub fn waiting_within(&mut self, within: Duration) -> bool {
        if !self.held.is_empty() {
            return true;
        }
        self.stream.set_read_timeout(Some(within)).unwrap();
        let mut chunk = [0u8; 65536];
        let waiting = match self.stream.read(&mut chunk) {
            Ok(0) => true,
            Ok(read) => {
                self.held.extend_from_slice(&chunk[..read]);
                true
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => false,
            Err(_) => true,
        };
        self.stream.set_read_timeout(Some(WAIT)).unwrap();
        waiting
    }

    /// Sends the request; reading its answer is the caller's.
    pub fn try_attach(port: u16, upgrade: &Upgrade<'_>) -> IdeClient {
        let mut stream = dial(port);
        stream.write_all(upgrade.text().as_bytes()).unwrap();
        IdeClient {
            stream,
            held: Vec::new(),
            next_id: 1_000,
        }
    }

    pub fn read_head(&mut self) -> String {
        while !self.held.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            if !self.fill() {
                break;
            }
        }
        let end = self
            .held
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .map_or(self.held.len(), |at| at + 4);
        let head: Vec<u8> = self.held.drain(..end).collect();
        String::from_utf8(head).expect("a head is text")
    }

    /// Shrinks what the kernel takes for this client, for a test that needs its peer's writes to back up.
    pub fn set_small_receive_buffer(&self) {
        let bytes: libc::c_int = 4096;
        // SAFETY: reads `size_of::<c_int>()` bytes of `bytes`, which lives for the call.
        let answer = unsafe {
            libc::setsockopt(
                self.stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&bytes as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(answer, 0, "SO_RCVBUF refused");
    }

    pub fn send_raw(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).unwrap();
    }

    pub fn send_text(&mut self, text: &[u8]) {
        self.send_raw(&client_frame(OP_TEXT, text));
    }

    pub fn send_json(&mut self, message: &Value) {
        self.send_text(message.to_string().as_bytes());
    }

    pub fn request(&mut self, id: i64, method: &str, params: Value) {
        self.send_json(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        self.send_json(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    pub fn call_tool(&mut self, id: i64, name: &str, arguments: Value) {
        self.request(id, "tools/call", json!({"name": name, "arguments": arguments}));
    }

    pub fn open_diff(&mut self, id: i64, contents: &str) {
        self.call_tool(
            id,
            "openDiff",
            json!({"old_file_path": "/p/a", "new_file_path": "/p/a", "new_file_contents": contents, "tab_name": "t"}),
        );
    }

    pub fn close(&mut self, code: u16) {
        self.send_raw(&client_frame(OP_CLOSE, &code.to_be_bytes()));
    }

    pub fn ping(&mut self, payload: &[u8]) {
        self.send_raw(&client_frame(OP_PING, payload));
    }

    pub fn pong(&mut self) {
        self.send_raw(&client_frame(OP_PONG, b""));
    }

    /// `true`: more bytes came. A reset is the peer gone, as for [`read_to_end`].
    fn fill(&mut self) -> bool {
        let mut chunk = [0u8; 65536];
        match self.stream.read(&mut chunk) {
            Ok(0) => false,
            Ok(read) => {
                self.held.extend_from_slice(&chunk[..read]);
                true
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                panic!("nothing came in time, {} bytes held", self.held.len())
            }
            Err(_) => false,
        }
    }

    fn need(&mut self, count: usize) -> bool {
        while self.held.len() < count {
            if !self.fill() {
                return false;
            }
        }
        true
    }

    /// Reads raw until the stream so far ends with `tail`, keeping none of it: for a test that lets millions of small
    /// frames pile up and cares only about the last.
    pub fn read_until_ends_with(&mut self, tail: &[u8]) {
        while !self.held.ends_with(tail) {
            let keep = self.held.len().saturating_sub(tail.len());
            self.held.drain(..keep);
            assert!(self.fill(), "the stream ended first");
        }
        self.held.clear();
    }

    pub fn read_frame(&mut self) -> Received {
        if !self.need(2) {
            return Received::Gone;
        }
        let (byte0, byte1) = (self.held[0], self.held[1]);
        assert_eq!(byte1 & MASKED, 0, "a server frame is never masked");
        assert_ne!(byte0 & FIN, 0, "this server does not fragment");
        let (length, head) = match byte1 & 0x7f {
            126 => {
                if !self.need(4) {
                    return Received::Gone;
                }
                (usize::from(u16::from_be_bytes([self.held[2], self.held[3]])), 4)
            }
            127 => {
                if !self.need(10) {
                    return Received::Gone;
                }
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&self.held[2..10]);
                (u64::from_be_bytes(bytes) as usize, 10)
            }
            short => (usize::from(short), 2),
        };
        if !self.need(head + length) {
            return Received::Gone;
        }
        let payload = self.held[head..head + length].to_vec();
        self.held.drain(..head + length);
        match byte0 & 0x0f {
            OP_TEXT => Received::Text(payload),
            OP_PONG => Received::Pong(payload),
            OP_CLOSE => Received::Close(u16::from_be_bytes([payload[0], payload[1]])),
            other => panic!("an opcode this server never sends: {other}"),
        }
    }

    /// The next message, whatever frames of no interest sit before it.
    pub fn read_message(&mut self) -> Value {
        loop {
            match self.read_frame() {
                Received::Text(text) => return serde_json::from_slice(&text).expect("the server writes JSON"),
                Received::Pong(_) => {}
                other => panic!("no message: {other:?}"),
            }
        }
    }

    /// One round trip: once this returns, the server has dealt with everything sent before it. Fails on any message
    /// that arrives ahead of the answer.
    pub fn round_trip(&mut self) {
        self.next_id += 1;
        let id = self.next_id;
        self.request(id, "tools/list", json!({}));
        let answer = self.read_message();
        assert_eq!(answer["id"], id, "a message ahead of the answer: {answer}");
    }

    /// The close frame with this code, then the end of the socket.
    pub fn expect_closed_with(&mut self, code: u16) {
        assert_eq!(self.read_frame(), Received::Close(code));
        assert_eq!(self.read_frame(), Received::Gone);
    }
}
