//! The token and the checks a WebSocket upgrade passes before it is accepted.
//!
//! Pure over the bytes read so far: nothing here touches a socket, and an answer never repeats a byte of the request.

use std::io;

use sha1::{Digest, Sha1};

use super::types::Limits;

/// The header the CLI sends the lock file's token in.
const TOKEN_HEADER: &str = "x-claude-code-ide-authorization";
/// The only subprotocol spoken, and the one the CLI asks for.
const SUBPROTOCOL: &str = "mcp";
/// RFC 6455's fixed suffix for `Sec-WebSocket-Accept`.
const ACCEPT_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// What a client has to present to upgrade: 64 lowercase hex bytes, new for each listener, so a client left over from an
/// earlier Eitri is refused. `Debug` prints none of it.
pub struct Token {
    text: String,
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

impl Token {
    /// 32 bytes from `fill`, in hex. `Err` when `fill` fails: a clock or a pid would be a token a local program could
    /// guess, so there is no fallback and the caller runs no listener.
    pub fn generate(fill: impl FnOnce(&mut [u8; 32]) -> io::Result<()>) -> io::Result<Token> {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut bytes = [0u8; 32];
        fill(&mut bytes)?;
        let mut text = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            text.push(DIGITS[usize::from(byte >> 4)] as char);
            text.push(DIGITS[usize::from(byte & 15)] as char);
        }
        Ok(Token { text })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// A length difference is `false`; otherwise every byte is folded in, so where the first difference lies changes
    /// nothing a client can time. The lengths are no secret: every token has the same one.
    pub fn matches(&self, given: &[u8]) -> bool {
        let own = self.text.as_bytes();
        if own.len() != given.len() {
            return false;
        }
        let mut difference = 0u8;
        for (ours, theirs) in own.iter().zip(given) {
            difference |= std::hint::black_box(ours ^ theirs);
        }
        std::hint::black_box(difference) == 0
    }
}

/// `getrandom(2)` with flags 0, retried on `EINTR`. A short read is an error, so a token is never partly zero.
pub fn getrandom_fill(buf: &mut [u8; 32]) -> io::Result<()> {
    loop {
        // SAFETY: writes at most `buf.len()` bytes into `buf`, which lives for the call.
        let got = unsafe { libc::getrandom(buf.as_mut_ptr().cast(), buf.len(), 0) };
        if got < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if got as usize != buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "getrandom gave fewer bytes than asked",
            ));
        }
        return Ok(());
    }
}

/// What the bytes of an upgrade request come to so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// No blank line yet, and under the cap.
    NeedMore,
    /// The 101: write it, and the connection speaks WebSocket from then on.
    Upgrade { answer: Vec<u8> },
    /// An HTTP error: write it, then close. `why` says which check for a log line, a constant, so nothing of the request
    /// can reach a log through it.
    Refuse { answer: Vec<u8>, why: &'static str },
}

/// Which check a request failed. Two checks answer the same status, and this is what tells them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    Malformed,
    Host,
    Origin,
    Token,
    Upgrade,
}

impl Failure {
    fn status(self) -> &'static str {
        match self {
            Failure::Malformed | Failure::Upgrade => "400 Bad Request",
            Failure::Host | Failure::Origin => "403 Forbidden",
            Failure::Token => "401 Unauthorized",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Failure::Malformed => "not a well-formed upgrade request",
            Failure::Host => "its Host is not this listener's address",
            Failure::Origin => "it sent an Origin header",
            Failure::Token => "no token header with this listener's token",
            Failure::Upgrade => "its WebSocket upgrade headers are not the ones expected",
        }
    }

    fn verdict(self) -> Verdict {
        Verdict::Refuse {
            answer: refusal(self.status()),
            why: self.name(),
        }
    }
}

/// Judges `head`, everything read from a connection so far. Bytes after the blank line are not part of the request.
pub fn judge(head: &[u8], port: u16, token: &Token, limits: &Limits) -> Verdict {
    // Searching only the cap's worth also refuses a whole head that arrived in one read and ends past the cap.
    let within = &head[..head.len().min(limits.handshake_bytes)];
    let Some(end) = within.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
        // A head already at the cap with no blank line cannot end inside the cap, whatever comes next.
        return if head.len() >= limits.handshake_bytes {
            Failure::Malformed.verdict()
        } else {
            Verdict::NeedMore
        };
    };
    match decide(&within[..end], port, token) {
        Ok(key) => Verdict::Upgrade {
            answer: upgrade_answer(key),
        },
        Err(failure) => failure.verdict(),
    }
}

/// The answer that refuses a connection: `status` is a status line's code and reason. It carries nothing else, and
/// least of all anything of the request.
pub fn refusal(status: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n").into_bytes()
}

/// `base64(sha1(key + the RFC 6455 GUID))`, the value of `Sec-WebSocket-Accept`.
pub fn accept_value(key: &str) -> String {
    let mut hash = Sha1::new();
    hash.update(key.as_bytes());
    hash.update(ACCEPT_GUID.as_bytes());
    base64(&hash.finalize())
}

fn upgrade_answer(key: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\nSec-WebSocket-Protocol: {SUBPROTOCOL}\r\n\r\n",
        accept_value(key)
    )
    .into_bytes()
}

/// Every check in its order, over the head without its blank line. `Ok` holds the request's `Sec-WebSocket-Key`.
fn decide<'h>(block: &'h [u8], port: u16, token: &Token) -> Result<&'h str, Failure> {
    let headers = parse(block).ok_or(Failure::Malformed)?;
    // Exact, so a page that rebinds a name to the loopback address names the wrong host.
    if one(&headers, "host") != Some(format!("127.0.0.1:{port}").as_str()) {
        return Err(Failure::Host);
    }
    // A browser's WebSocket always sends one, and the CLI none, so its value says nothing worth reading.
    if !all(&headers, "origin").is_empty() {
        return Err(Failure::Origin);
    }
    if !one(&headers, TOKEN_HEADER).is_some_and(|given| token.matches(given.as_bytes())) {
        return Err(Failure::Token);
    }
    let upgrades = one(&headers, "upgrade").is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    let connection = in_list(&headers, "connection", |item| item.eq_ignore_ascii_case("upgrade"));
    let version = one(&headers, "sec-websocket-version") == Some("13");
    let subprotocol = in_list(&headers, "sec-websocket-protocol", |item| item == SUBPROTOCOL);
    match one(&headers, "sec-websocket-key").filter(|key| !key.is_empty()) {
        Some(key) if upgrades && connection && version && subprotocol => Ok(key),
        _ => Err(Failure::Upgrade),
    }
}

/// A header's name and its value, trimmed.
type Header<'h> = (&'h str, &'h str);

/// The header fields of a `GET <target> HTTP/1.1` head, or `None` when it is not one. Strict on purpose: a bare `\r` or
/// `\n` or any other control byte is refused, so no line can hide inside another's value. A folded line fails with the
/// rest, since its leading blank makes its name no token.
fn parse(block: &[u8]) -> Option<Vec<Header<'_>>> {
    let text = std::str::from_utf8(block).ok().filter(|text| text.is_ascii())?;
    let clean = |line: &str| !line.bytes().any(|byte| byte.is_ascii_control() && byte != b'\t');
    let mut lines = text.split("\r\n");
    let first = lines.next()?;
    let parts: Vec<&str> = first.split(' ').collect();
    if !clean(first) || !matches!(parts.as_slice(), ["GET", target, "HTTP/1.1"] if !target.is_empty()) {
        return None;
    }
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line.split_once(':')?;
        if !clean(line) || name.is_empty() || name.contains([' ', '\t']) {
            return None;
        }
        headers.push((name, value.trim()));
    }
    Some(headers)
}

/// Every value of the header `name`, which is lowercase.
fn all<'h>(headers: &[Header<'h>], name: &str) -> Vec<&'h str> {
    headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| *value)
        .collect()
}

/// The value of a header that may appear once: `None` when it is absent or there twice.
fn one<'h>(headers: &[Header<'h>], name: &str) -> Option<&'h str> {
    match all(headers, name).as_slice() {
        [value] => Some(*value),
        _ => None,
    }
}

/// Whether a comma-separated header, over all its appearances, has an item `wanted` accepts.
fn in_list(headers: &[Header<'_>], name: &str, wanted: impl Fn(&str) -> bool) -> bool {
    all(headers, name)
        .iter()
        .flat_map(|value| value.split(','))
        .any(|item| wanted(item.trim()))
}

/// Standard base64 with padding, for the 20 bytes of a SHA-1.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let byte = |at: usize| u32::from(chunk.get(at).copied().unwrap_or(0));
        let group = (byte(0) << 16) | (byte(1) << 8) | byte(2);
        for digit in 0..4 {
            if digit <= chunk.len() {
                text.push(ALPHABET[((group >> (18 - 6 * digit)) & 63) as usize] as char);
            } else {
                text.push('=');
            }
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_scan::{function_in, read_required};

    const PORT: u16 = 40123;
    /// RFC 6455's worked example, section 1.3.
    const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
    const ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

    fn token() -> Token {
        Token::generate(|bytes| {
            bytes.fill(0xab);
            Ok(())
        })
        .unwrap()
    }

    fn good_token() -> String {
        "ab".repeat(32)
    }

    /// A request as header fields, starting from one that upgrades.
    struct Head {
        headers: Vec<(String, String)>,
    }

    impl Head {
        fn good() -> Head {
            let mut head = Head { headers: Vec::new() };
            for (name, value) in [
                ("Host", format!("127.0.0.1:{PORT}")),
                ("Upgrade", "websocket".to_owned()),
                ("Connection", "Upgrade".to_owned()),
                ("Sec-WebSocket-Key", KEY.to_owned()),
                ("Sec-WebSocket-Version", "13".to_owned()),
                ("Sec-WebSocket-Protocol", "mcp".to_owned()),
                ("X-Claude-Code-Ide-Authorization", good_token()),
            ] {
                head = head.with(name, &value);
            }
            head
        }

        fn without(mut self, name: &str) -> Head {
            self.headers.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
            self
        }

        fn with(mut self, name: &str, value: &str) -> Head {
            self.headers.push((name.to_owned(), value.to_owned()));
            self
        }

        fn set(self, name: &str, value: &str) -> Head {
            self.without(name).with(name, value)
        }

        fn text(&self) -> String {
            let mut text = String::from("GET / HTTP/1.1\r\n");
            for (name, value) in &self.headers {
                text.push_str(&format!("{name}: {value}\r\n"));
            }
            text.push_str("\r\n");
            text
        }
    }

    fn judged(head: &[u8]) -> Verdict {
        judge(head, PORT, &token(), &Limits::default())
    }

    fn status(verdict: &Verdict) -> u16 {
        let answer = match verdict {
            Verdict::NeedMore => panic!("a whole head got NeedMore"),
            Verdict::Upgrade { answer } | Verdict::Refuse { answer, .. } => answer,
        };
        let answer = std::str::from_utf8(answer).unwrap();
        let code = answer.strip_prefix("HTTP/1.1 ").unwrap().split(' ').next().unwrap();
        let code: u16 = code.parse().unwrap();
        assert_eq!(code == 101, matches!(verdict, Verdict::Upgrade { .. }), "{answer}");
        code
    }

    fn status_of(head: &Head) -> u16 {
        status(&judged(head.text().as_bytes()))
    }

    fn failure_of(head: &Head) -> Result<(), Failure> {
        let text = head.text();
        decide(text.strip_suffix("\r\n\r\n").unwrap().as_bytes(), PORT, &token()).map(|_| ())
    }

    #[test]
    fn a_good_request_upgrades() {
        assert_eq!(status_of(&Head::good()), 101);
    }

    #[test]
    fn a_wrong_host_gets_403() {
        let bad = [
            "localhost".to_owned(),
            format!("localhost:{PORT}"),
            "127.0.0.1".to_owned(),
            format!("127.0.0.1:{}", PORT + 1),
            format!("[::1]:{PORT}"),
            format!("127.0.0.1:{PORT}.example"),
        ];
        for host in bad {
            assert_eq!(status_of(&Head::good().set("Host", &host)), 403, "{host}");
        }
        assert_eq!(status_of(&Head::good().without("Host")), 403, "absent");
        let twice = Head::good().with("Host", &format!("127.0.0.1:{PORT}"));
        assert_eq!(status_of(&twice), 403, "twice");
    }

    #[test]
    fn any_origin_gets_403() {
        for origin in ["null", "", "http://127.0.0.1", "vscode-webview://x"] {
            assert_eq!(status_of(&Head::good().with("Origin", origin)), 403, "{origin:?}");
        }
        assert_eq!(
            status_of(&Head::good().with("oRiGiN", "x")),
            403,
            "the name in another case"
        );
    }

    #[test]
    fn a_missing_token_gets_401() {
        assert_eq!(status_of(&Head::good().without("X-Claude-Code-Ide-Authorization")), 401);
    }

    #[test]
    fn a_wrong_token_gets_401() {
        let wrong = "cd".repeat(32);
        assert_eq!(
            status_of(&Head::good().set("X-Claude-Code-Ide-Authorization", &wrong)),
            401
        );
    }

    #[test]
    fn a_token_differing_in_its_last_byte_gets_401() {
        let mut close = good_token();
        close.pop();
        close.push('c');
        assert_eq!(
            status_of(&Head::good().set("X-Claude-Code-Ide-Authorization", &close)),
            401
        );
    }

    #[test]
    fn a_token_of_another_length_gets_401() {
        let whole = good_token();
        for given in [&whole[..63], &whole[..0], &format!("{whole}a")] {
            assert_eq!(
                status_of(&Head::good().set("X-Claude-Code-Ide-Authorization", given)),
                401,
                "{given}"
            );
        }
    }

    #[test]
    fn a_token_sent_twice_gets_401() {
        let twice = Head::good().with("X-Claude-Code-Ide-Authorization", &good_token());
        assert_eq!(status_of(&twice), 401);
    }

    #[test]
    fn no_mcp_subprotocol_is_refused() {
        assert_eq!(
            status_of(&Head::good().without("Sec-WebSocket-Protocol")),
            400,
            "absent"
        );
        for offered in ["", "chat", "chat, superchat", "MCP", "mcp2", "xmcp", "m c p"] {
            assert_eq!(
                status_of(&Head::good().set("Sec-WebSocket-Protocol", offered)),
                400,
                "{offered:?}"
            );
        }
    }

    #[test]
    fn the_upgrade_selects_mcp() {
        let among_others = Head::good().set("Sec-WebSocket-Protocol", "chat, mcp ,superchat");
        let on_a_second_line = Head::good()
            .set("Sec-WebSocket-Protocol", "chat")
            .with("Sec-WebSocket-Protocol", "mcp");
        for head in [Head::good(), among_others, on_a_second_line] {
            let Verdict::Upgrade { answer } = judged(head.text().as_bytes()) else {
                panic!("{}", head.text());
            };
            assert_eq!(
                String::from_utf8(answer).unwrap(),
                format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                     Sec-WebSocket-Accept: {ACCEPT}\r\nSec-WebSocket-Protocol: mcp\r\n\r\n"
                ),
            );
        }
    }

    #[test]
    fn a_bad_upgrade_gets_400() {
        let bad = [
            ("no Upgrade", Head::good().without("Upgrade")),
            ("another Upgrade", Head::good().set("Upgrade", "h2c")),
            ("Upgrade twice", Head::good().with("Upgrade", "websocket")),
            ("no Connection", Head::good().without("Connection")),
            (
                "Connection lacking upgrade",
                Head::good().set("Connection", "keep-alive"),
            ),
            (
                "Connection naming it inside a word",
                Head::good().set("Connection", "upgraded"),
            ),
            ("no key", Head::good().without("Sec-WebSocket-Key")),
            ("an empty key", Head::good().set("Sec-WebSocket-Key", "")),
            ("two keys", Head::good().with("Sec-WebSocket-Key", KEY)),
            ("no version", Head::good().without("Sec-WebSocket-Version")),
            ("another version", Head::good().set("Sec-WebSocket-Version", "8")),
            ("a version list", Head::good().set("Sec-WebSocket-Version", "13, 8")),
        ];
        for (what, head) in bad {
            assert_eq!(status_of(&head), 400, "{what}");
        }
    }

    #[test]
    fn a_head_that_is_not_a_get_request_gets_400() {
        let good = Head::good().text();
        let first_lines = [
            "POST / HTTP/1.1",
            "get / HTTP/1.1",
            "GET / HTTP/1.0",
            "GET / HTTP/2",
            "GET /  HTTP/1.1",
            "GET HTTP/1.1",
            "GET  HTTP/1.1",
            "GET / HTTP/1.1 more",
            "GET /a\u{1}b HTTP/1.1",
        ];
        for first in first_lines {
            let head = good.replacen("GET / HTTP/1.1", first, 1);
            assert_eq!(status(&judged(head.as_bytes())), 400, "{first:?}");
        }
        assert_eq!(status(&judged(b"\r\n\r\n")), 400, "nothing at all");
    }

    #[test]
    fn a_bad_header_line_gets_400() {
        let good = Head::good().text();
        let lines = [
            "no colon here",
            " folded: onto the line before",
            "\tfolded: onto the line before",
            ": no name",
            "Spaced : name",
            "X-Text: caf\u{e9}",
            "X-Bare: a\nOrigin: http://hidden.example",
            "X-Bare: a\rb",
            "X-Trailing: a\n",
            "X-Nul: a\0b",
        ];
        for line in lines {
            let head = good.replacen("\r\n\r\n", &format!("\r\n{line}\r\n\r\n"), 1);
            assert_eq!(status(&judged(head.as_bytes())), 400, "{line:?}");
        }
    }

    #[test]
    fn names_are_case_insensitive_and_values_trimmed() {
        let token = good_token();
        let head = format!(
            "GET /some/where?a=b HTTP/1.1\r\nhOsT:\t 127.0.0.1:{PORT} \t\r\nuPgRaDe: WebSocket\r\n\
             CONNECTION: keep-alive,  UPGRADE\r\nsec-websocket-key:   {KEY}\r\nsec-websocket-version:13\r\n\
             sec-websocket-protocol:  other ,  mcp \r\nx-claude-code-ide-authorization:  {token}  \r\n\r\n"
        );
        let verdict = judged(head.as_bytes());
        assert_eq!(status(&verdict), 101);
        let Verdict::Upgrade { answer } = verdict else {
            unreachable!()
        };
        assert!(
            String::from_utf8(answer).unwrap().contains(ACCEPT),
            "the key was read with its blanks"
        );
    }

    #[test]
    fn the_checks_run_in_order() {
        let every_check_fails = Head::good()
            .set("Host", "localhost")
            .with("Origin", "http://127.0.0.1")
            .without("X-Claude-Code-Ide-Authorization")
            .without("Sec-WebSocket-Protocol");
        assert_eq!(failure_of(&every_check_fails), Err(Failure::Host));
        assert_eq!(status_of(&every_check_fails), 403);

        let host_fixed = every_check_fails.set("Host", &format!("127.0.0.1:{PORT}"));
        assert_eq!(failure_of(&host_fixed), Err(Failure::Origin));
        assert_eq!(status_of(&host_fixed), 403);

        let origin_gone = host_fixed.without("Origin");
        assert_eq!(failure_of(&origin_gone), Err(Failure::Token));
        assert_eq!(status_of(&origin_gone), 401);

        let token_fixed = origin_gone.set("X-Claude-Code-Ide-Authorization", &good_token());
        assert_eq!(failure_of(&token_fixed), Err(Failure::Upgrade));
        assert_eq!(status_of(&token_fixed), 400);

        let protocol_fixed = token_fixed.set("Sec-WebSocket-Protocol", "mcp");
        assert_eq!(failure_of(&protocol_fixed), Ok(()));
        assert_eq!(status_of(&protocol_fixed), 101);
    }

    #[test]
    fn a_malformed_head_is_refused_before_any_header_is_read() {
        let head = Head::good().set("Host", "localhost").text();
        let head = head.replacen("\r\n\r\n", "\r\nno colon here\r\n\r\n", 1);
        assert_eq!(status(&judged(head.as_bytes())), 400);
    }

    #[test]
    fn a_head_over_the_cap_is_refused() {
        let limits = Limits {
            handshake_bytes: 512,
            ..Limits::default()
        };
        let token = token();
        let padded_to = |size: usize| {
            let short = Head::good().with("X-Pad", "").text();
            Head::good().with("X-Pad", &"p".repeat(size - short.len())).text()
        };

        let whole_at_the_cap = padded_to(512);
        assert_eq!(whole_at_the_cap.len(), 512);
        assert_eq!(status(&judge(whole_at_the_cap.as_bytes(), PORT, &token, &limits)), 101);

        let whole_past_the_cap = padded_to(513);
        assert_eq!(
            status(&judge(whole_past_the_cap.as_bytes(), PORT, &token, &limits)),
            400
        );

        let endless = "x".repeat(513);
        assert_eq!(status(&judge(endless.as_bytes(), PORT, &token, &limits)), 400);

        let endless_at_the_cap = "x".repeat(512);
        assert_eq!(
            status(&judge(endless_at_the_cap.as_bytes(), PORT, &token, &limits)),
            400
        );

        assert_eq!(
            judge("x".repeat(511).as_bytes(), PORT, &token, &limits),
            Verdict::NeedMore
        );
    }

    #[test]
    fn need_more_until_the_blank_line() {
        let head = Head::good().text();
        for end in 0..head.len() {
            assert_eq!(judged(&head.as_bytes()[..end]), Verdict::NeedMore, "{end}");
        }
        assert_eq!(status(&judged(head.as_bytes())), 101);
    }

    #[test]
    fn bytes_after_the_blank_line_are_no_part_of_the_request() {
        let head = Head::good().text() + "Origin: http://127.0.0.1\r\n\r\n";
        assert_eq!(status(&judged(head.as_bytes())), 101);
    }

    #[test]
    fn a_refusal_closes_with_no_body_and_repeats_nothing() {
        let head = Head::good()
            .set("Host", "marker-host")
            .with("User-Agent", "marker-agent")
            .text()
            .replacen("GET /", "GET /marker-target", 1);
        let Verdict::Refuse { answer, .. } = judged(head.as_bytes()) else {
            panic!("refused above")
        };
        assert_eq!(
            answer,
            b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
        );
    }

    #[test]
    fn each_refusal_names_its_check() {
        let why_of = |head: Head| match judged(head.text().as_bytes()) {
            Verdict::Refuse { why, .. } => why,
            other => panic!("not refused: {other:?}"),
        };
        let named = [
            (Head::good().set("Host", "marker-host"), Failure::Host),
            (Head::good().with("Origin", "http://marker"), Failure::Origin),
            (
                Head::good().set("X-Claude-Code-Ide-Authorization", "marker"),
                Failure::Token,
            ),
            (Head::good().without("Upgrade"), Failure::Upgrade),
        ];
        let mut seen = Vec::new();
        for (head, failure) in named {
            let why = why_of(head);
            assert_eq!(why, failure.name());
            assert!(!seen.contains(&why), "{why}");
            seen.push(why);
        }
        let Verdict::Refuse { why, .. } = judged(b"not a request\r\n\r\n") else {
            panic!("refused")
        };
        assert_eq!(why, Failure::Malformed.name());
    }

    #[test]
    fn each_status_has_its_reason() {
        for (head, line) in [
            (Head::good().without("Host"), "HTTP/1.1 403 Forbidden\r\n"),
            (
                Head::good().without("X-Claude-Code-Ide-Authorization"),
                "HTTP/1.1 401 Unauthorized\r\n",
            ),
            (Head::good().without("Upgrade"), "HTTP/1.1 400 Bad Request\r\n"),
        ] {
            let Verdict::Refuse { answer, .. } = judged(head.text().as_bytes()) else {
                panic!("refused above")
            };
            assert!(answer.starts_with(line.as_bytes()), "{line}");
        }
    }

    #[test]
    fn accept_matches_the_rfc_example() {
        assert_eq!(accept_value(KEY), ACCEPT);
    }

    #[test]
    fn base64_pads_each_length() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn the_token_is_64_hex_and_fresh_each_time() {
        let one = Token::generate(getrandom_fill).unwrap();
        let other = Token::generate(getrandom_fill).unwrap();
        for token in [&one, &other] {
            assert_eq!(token.as_str().len(), 64);
            assert!(token
                .as_str()
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')));
        }
        assert_ne!(one.as_str(), other.as_str());
        assert!(one.matches(one.as_str().as_bytes()));
        assert!(!other.matches(one.as_str().as_bytes()));
    }

    #[test]
    fn the_token_spells_every_byte() {
        let token = Token::generate(|bytes| {
            for (at, byte) in bytes.iter_mut().enumerate() {
                *byte = at as u8 * 8;
            }
            Ok(())
        })
        .unwrap();
        assert!(token.as_str().starts_with("00081018"), "{}", token.as_str());
        assert!(token.as_str().ends_with("f0f8"), "{}", token.as_str());
    }

    #[test]
    fn a_failing_random_source_gives_no_token() {
        let failed = Token::generate(|_| Err(io::Error::from(io::ErrorKind::Unsupported)));
        assert_eq!(failed.unwrap_err().kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn debug_prints_none_of_the_token() {
        let token = token();
        assert!(!format!("{token:?}").contains(token.as_str()));
    }

    #[test]
    fn matches_compares_by_a_fold() {
        let source = read_required("core/src/ide_server/handshake.rs");
        let (_, body) = function_in(std::slice::from_ref(&source), "matches");
        for needed in ["black_box", "|="] {
            assert!(body.contains(needed), "`matches` lost `{needed}`: {body}");
        }
        for banned in [".eq(", "starts_with", "== given"] {
            assert!(
                !body.contains(banned),
                "`matches` holds `{banned}`, which stops at a difference: {body}"
            );
        }
    }
}
