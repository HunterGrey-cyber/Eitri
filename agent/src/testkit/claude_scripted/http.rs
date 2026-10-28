//! Bounded HTTP/1.1 framing. Each connection serves one request, even with keep-alive input.
use super::{RequestRecord, Shared};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    net::TcpStream,
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};

const HEAD_LIMIT: usize = 64 * 1024;
const BODY_LIMIT: usize = 32 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Request {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

pub(super) struct RequestError {
    pub request: Request,
    pub reason: String,
}

fn line(stream: &mut impl Read, budget: &mut usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        if *budget == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "HTTP head exceeds limit"));
        }
        *budget -= 1;
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n") {
            bytes.truncate(bytes.len() - 2);
            return Ok(bytes);
        }
    }
}

fn bad(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(super) fn read_request(stream: &mut impl Read) -> Result<Request, RequestError> {
    let mut request = Request::default();
    let result = (|| -> io::Result<()> {
        let mut budget = HEAD_LIMIT;
        let first = line(stream, &mut budget)?;
        let first = std::str::from_utf8(&first).map_err(|_| bad("invalid request line"))?;
        let parts: Vec<_> = first.split_whitespace().collect();
        if parts.len() != 3 || parts[2] != "HTTP/1.1" {
            return Err(bad("expected HTTP/1.1 request"));
        }
        request.method = parts[0].into();
        request.path = parts[1].into();
        loop {
            let bytes = line(stream, &mut budget)?;
            if bytes.is_empty() {
                break;
            }
            let header = std::str::from_utf8(&bytes).map_err(|_| bad("invalid header encoding"))?;
            let (name, value) = header.split_once(':').ok_or_else(|| bad("invalid header"))?;
            let name = name.to_ascii_lowercase();
            if !valid_header(&name, value) || request.headers.insert(name, value.trim().into()).is_some() {
                return Err(bad("invalid or duplicate header"));
            }
        }
        let length = request.headers.get("content-length");
        if let Some(encoding) = request.headers.get("transfer-encoding") {
            if length.is_some() || !encoding.eq_ignore_ascii_case("chunked") {
                return Err(bad("unsupported transfer encoding"));
            }
            loop {
                let bytes = line(stream, &mut budget)?;
                let size = std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(|s| usize::from_str_radix(s.split(';').next()?, 16).ok())
                    .ok_or_else(|| bad("invalid chunk size"))?;
                if size == 0 {
                    while !line(stream, &mut budget)?.is_empty() {}
                    break;
                }
                if size > BODY_LIMIT - request.body.len() {
                    return Err(bad("HTTP body exceeds limit"));
                }
                let old = request.body.len();
                stream.take(size as u64).read_to_end(&mut request.body)?;
                if request.body.len() != old + size {
                    return Err(bad("incomplete chunk body"));
                }
                let mut ending = [0; 2];
                stream.read_exact(&mut ending)?;
                if ending != *b"\r\n" {
                    return Err(bad("invalid chunk ending"));
                }
            }
        } else {
            let size = length.map_or(Ok(0), |s| s.parse::<usize>().map_err(|_| bad("invalid content length")))?;
            if size > BODY_LIMIT {
                return Err(bad("HTTP body exceeds limit"));
            }
            stream.take(size as u64).read_to_end(&mut request.body)?;
            if request.body.len() != size {
                return Err(bad("incomplete request body"));
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => Ok(request),
        Err(e) => Err(RequestError {
            request,
            reason: e.to_string(),
        }),
    }
}

pub(super) fn valid_header(key: &str, value: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        && value.bytes().all(|b| b == b'\t' || (b >= 32 && b != 127))
}

pub(super) fn stream_headers(
    stream: &mut TcpStream,
    index: usize,
    headers: &BTreeMap<String, String>,
) -> io::Result<()> {
    write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nrequest-id: req_scripted_{index}\r\nconnection: close\r\n")?;
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write!(stream, "\r\n")
}

pub(super) fn json_response(
    stream: &mut TcpStream,
    index: usize,
    status: u16,
    body: &Value,
    headers: &BTreeMap<String, String>,
) -> io::Result<()> {
    let body = body.to_string();
    write!(stream, "HTTP/1.1 {status} Response\r\ncontent-type: application/json\r\nrequest-id: req_scripted_{index}\r\nconnection: close\r\ncontent-length: {}\r\n", body.len())?;
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write!(stream, "\r\n{body}")
}

pub(super) fn hold(stream: &mut TcpStream, shared: &Shared, name: &str, record: &mut RequestRecord) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    let result = (|| {
        let mut ping_at = Instant::now() + Duration::from_secs(2);
        loop {
            if shared.stop.load(Ordering::Acquire) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "server stopped"));
            }
            if shared.state.lock().unwrap().released.contains(name) {
                return Ok(());
            }
            let mut byte = [0];
            match stream.peek(&mut byte) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "client closed hold")),
                Err(e) if e.kind() != io::ErrorKind::WouldBlock => return Err(e),
                _ => {}
            }
            if Instant::now() >= ping_at {
                stream.write_all(b"event: ping\ndata: {\"type\":\"ping\"}\n\n")?;
                record
                    .response_events
                    .push(serde_json::json!({"event":"ping","data":{"type":"ping"}}));
                shared
                    .state
                    .lock()
                    .unwrap()
                    .records
                    .insert(record.arrival_index, record.clone());
                ping_at = Instant::now() + Duration::from_secs(2);
            }
            thread::sleep(Duration::from_millis(20));
        }
    })();
    stream.set_nonblocking(false)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_chunked_body_and_query() {
        let mut input =
            &b"POST /v1/messages?beta=1 HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n"
                [..];
        let request = read_request(&mut input).unwrap_or_else(|e| panic!("{}", e.reason));
        assert_eq!(request.body, b"abcde");
        assert_eq!(request.path, "/v1/messages?beta=1");
    }
    #[test]
    fn bounds_heads_and_bodies_and_rejects_ambiguous_framing() {
        for raw in [
            format!("POST / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(HEAD_LIMIT)),
            format!("POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n", BODY_LIMIT + 1),
            "POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n".into(),
            "POST / HTTP/1.1\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n".into(),
        ] {
            assert!(read_request(&mut raw.as_bytes()).is_err());
        }
    }
}
