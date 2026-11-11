//! A stand-in for the OpenAI Responses API on loopback, answering from a script.
//!
//! Codex is pointed at it through a custom model provider in a private `CODEX_HOME`, so a real
//! `codex app-server` runs real tool calls, approvals, hooks and rules with no account and no traffic
//! leaving the machine. It serves one thing: `POST .../responses`, as a server-sent-event stream.
//!
//! - It listens on `127.0.0.1` only, on an ephemeral port, and lives until it is dropped.
//! - Each connection gets one std thread and carries one request (the answer says `Connection: close`).
//! - A `POST` to `.../responses` takes the first unused [`Step`] whose conditions its body satisfies;
//!   the conditions are substrings of the body's text. With no step left, or none that fits, the answer
//!   is one assistant message and a completed response, so a turn always ends.
//! - Every other request is answered `404` with no body.
//!
//! A conversation with children is scripted by what a request does or does not contain: the child's
//! requests are told apart from the parent's by the **absence of the parent's prompt**, since a child
//! that was forked with no history never sees it.

use serde_json::{json, Value};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The longest request head and the largest request body accepted.
const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 16 << 20;
/// How long a connection may sit silent before it is dropped.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// One scripted answer.
#[derive(Debug, Clone)]
pub struct Step {
    events: Vec<Value>,
    needs: Vec<String>,
    lacks: Vec<String>,
    hang: Option<Duration>,
    #[cfg(feature = "test-support")]
    hold_until_drop: bool,
}

impl Step {
    /// A step that answers with these server-sent events. `{RID}` inside an event is replaced by the
    /// response's id.
    pub fn events(events: Vec<Value>) -> Step {
        Step {
            events,
            needs: Vec::new(),
            lacks: Vec::new(),
            hang: None,
            #[cfg(feature = "test-support")]
            hold_until_drop: false,
        }
    }

    /// An assistant message, then a completed response.
    pub fn say(text: &str) -> Step {
        Step::events(vec![
            json!({"type": "response.created", "response": {"id": "{RID}"}}),
            json!({"type": "response.output_item.done", "item": {
                "type": "message", "role": "assistant", "id": "msg_{RID}",
                "content": [{"type": "output_text", "text": text}]}}),
            completed(),
        ])
    }

    /// A function call, then a completed response. `namespace` is the tool namespace the call names, if any.
    pub fn call(call_id: &str, namespace: Option<&str>, name: &str, arguments: &Value) -> Step {
        let mut item = json!({
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": arguments.to_string(),
        });
        if let (Some(namespace), Some(map)) = (namespace, item.as_object_mut()) {
            map.insert("namespace".into(), json!(namespace));
        }
        Step::events(vec![
            json!({"type": "response.created", "response": {"id": "{RID}"}}),
            json!({"type": "response.output_item.done", "item": item}),
            completed(),
        ])
    }

    /// A call of the shell tool. `escalate` asks to run outside the sandbox, which Codex turns into an
    /// approval request.
    pub fn exec(call_id: &str, command: &str, escalate: bool) -> Step {
        let mut arguments = json!({ "cmd": command });
        if escalate {
            arguments["sandbox_permissions"] = json!("require_escalated");
            arguments["justification"] = json!("scripted");
        }
        Step::call(call_id, None, "exec_command", &arguments)
    }

    /// The step applies only to a request whose body text contains `text`.
    pub fn when_body_has(mut self, text: &str) -> Step {
        self.needs.push(text.to_string());
        self
    }

    /// The step applies only to a request whose body text does not contain `text`.
    pub fn unless_body_has(mut self, text: &str) -> Step {
        self.lacks.push(text.to_string());
        self
    }

    /// After the events, the stream is held open this long (or until the model is dropped): a turn in progress.
    pub fn then_hang(mut self, how_long: Duration) -> Step {
        self.hang = Some(how_long);
        self
    }

    /// Holds the stream until the fixture is dropped, without a timer ending the turn.
    #[cfg(feature = "test-support")]
    pub fn then_hold(mut self) -> Step {
        self.hold_until_drop = true;
        self
    }

    fn fits(&self, body_text: &str) -> bool {
        self.needs.iter().all(|n| body_text.contains(n.as_str()))
            && !self.lacks.iter().any(|n| body_text.contains(n.as_str()))
    }
}

fn completed() -> Value {
    json!({"type": "response.completed", "response": {"id": "{RID}", "usage": {
        "input_tokens": 1, "input_tokens_details": null,
        "output_tokens": 1, "output_tokens_details": null,
        "total_tokens": 2}}})
}

#[derive(Default)]
struct Inner {
    script: Vec<Step>,
    requests: Vec<Value>,
    responses_served: u64,
}

fn lock(inner: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The running stand-in. Dropping it stops the listener.
pub struct ScriptedModel {
    /// The loopback port it listens on.
    pub port: u16,
    inner: Arc<Mutex<Inner>>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl ScriptedModel {
    /// Starts listening on `127.0.0.1` with an ephemeral port and the given script.
    pub fn start(script: Vec<Step>) -> io::Result<ScriptedModel> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let inner = Arc::new(Mutex::new(Inner {
            script,
            ..Inner::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let accept = {
            let inner = Arc::clone(&inner);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("eitri-scripted-model".into())
                .spawn(move || accept_loop(listener, inner, stop))?
        };
        Ok(ScriptedModel {
            port,
            inner,
            stop,
            accept: Some(accept),
        })
    }

    /// The JSON body of every `responses` request so far, in arrival order. A body that is not JSON is
    /// recorded as `{"unparsed": "<its first 2000 characters>"}`.
    pub fn requests(&self) -> Vec<Value> {
        lock(&self.inner).requests.clone()
    }

    /// How many steps have not been used.
    pub fn steps_left(&self) -> usize {
        lock(&self.inner).script.len()
    }

    /// The `config.toml` lines that make Codex use this model: a custom provider on this port, the
    /// catalog model `slug`, and a reasoning effort a project can be told apart by. Top-level keys only come
    /// before the provider table, so anything appended must be a table of its own.
    pub fn provider_toml(&self, slug: &str) -> String {
        format!(
            "model_provider = \"scripted\"\nmodel = \"{slug}\"\nmodel_reasoning_effort = \"low\"\n\n[model_providers.scripted]\nname = \"scripted model on loopback\"\nbase_url = \"http://127.0.0.1:{}/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = false\n",
            self.port
        )
    }
}

impl Drop for ScriptedModel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // The accept call blocks; one connection to ourselves wakes it to see the flag.
        let _ = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], self.port)), Duration::from_secs(1));
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
    }
}

fn accept_loop(listener: TcpListener, inner: Arc<Mutex<Inner>>, stop: Arc<AtomicBool>) {
    for stream in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = stream else { continue };
        let inner = Arc::clone(&inner);
        let stop = Arc::clone(&stop);
        let _ = std::thread::Builder::new()
            .name("eitri-scripted-model-conn".into())
            .spawn(move || {
                let _ = serve(stream, &inner, &stop);
            });
    }
}

struct Head {
    method: String,
    path: String,
    content_length: Option<usize>,
    chunked: bool,
}

fn read_head(reader: &mut BufReader<TcpStream>) -> io::Result<Head> {
    let mut line = String::new();
    let mut total = 0usize;
    let mut next_line = |reader: &mut BufReader<TcpStream>, line: &mut String| -> io::Result<usize> {
        line.clear();
        let n = reader.by_ref().take(MAX_HEAD as u64).read_line(line)?;
        total += n;
        if total > MAX_HEAD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "request head too long"));
        }
        Ok(n)
    };
    if next_line(reader, &mut line)? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut head = Head {
        method,
        path,
        content_length: None,
        chunked: false,
    };
    loop {
        if next_line(reader, &mut line)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let header = line.trim_end();
        if header.is_empty() {
            return Ok(head);
        }
        if let Some((name, value)) = header.split_once(':') {
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                head.content_length = value.parse().ok();
            } else if name.eq_ignore_ascii_case("transfer-encoding") && value.to_ascii_lowercase().contains("chunked") {
                head.chunked = true;
            }
        }
    }
}

fn read_body(reader: &mut BufReader<TcpStream>, head: &Head) -> io::Result<Vec<u8>> {
    if head.chunked {
        let mut body = Vec::new();
        loop {
            let mut size_line = String::new();
            reader.read_line(&mut size_line)?;
            let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0"), 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
            if size == 0 {
                // Trailers, then the blank line.
                loop {
                    let mut trailer = String::new();
                    if reader.read_line(&mut trailer)? == 0 || trailer.trim().is_empty() {
                        return Ok(body);
                    }
                }
            }
            if body.len() + size > MAX_BODY {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "request body too large"));
            }
            let start = body.len();
            body.resize(start + size, 0);
            reader.read_exact(&mut body[start..])?;
            let mut crlf = [0u8; 2];
            reader.read_exact(&mut crlf)?;
        }
    }
    let len = head.content_length.unwrap_or(0);
    if len > MAX_BODY {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "request body too large"));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    Ok(body)
}

fn sse(events: &[Value], response_id: &str) -> Vec<u8> {
    let mut out = String::new();
    for event in events {
        let text = event.to_string().replace("{RID}", response_id);
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("message");
        out.push_str(&format!("event: {kind}\ndata: {text}\n\n"));
    }
    out.into_bytes()
}

fn serve(stream: TcpStream, inner: &Mutex<Inner>, stop: &AtomicBool) -> io::Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let head = read_head(&mut reader)?;
    let body = read_body(&mut reader, &head)?;
    let is_responses = head.method == "POST" && head.path.split('?').next().is_some_and(|p| p.ends_with("/responses"));
    if !is_responses {
        writer.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
        return writer.shutdown(Shutdown::Both);
    }

    let text = String::from_utf8_lossy(&body).into_owned();
    let parsed: Value = serde_json::from_slice(&body)
        .unwrap_or_else(|_| json!({ "unparsed": text.chars().take(2000).collect::<String>() }));
    let (step, response_id) = {
        let mut inner = lock(inner);
        inner.responses_served += 1;
        let response_id = format!("resp_{}", inner.responses_served);
        inner.requests.push(parsed);
        let chosen = inner.script.iter().position(|s| s.fits(&text));
        (chosen.map(|i| inner.script.remove(i)), response_id)
    };
    #[cfg(feature = "test-support")]
    let hold_until_drop = step.as_ref().is_some_and(|step| step.hold_until_drop);
    let (events, hang) = match step {
        Some(step) => (step.events, step.hang),
        None => (Step::say("done").events, None),
    };

    writer.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
    )?;
    writer.write_all(&sse(&events, &response_id))?;
    writer.flush()?;
    #[cfg(feature = "test-support")]
    if hold_until_drop {
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
        }
        return writer.shutdown(Shutdown::Both);
    }
    if let Some(how_long) = hang {
        let end = Instant::now() + how_long;
        while Instant::now() < end && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    writer.shutdown(Shutdown::Both)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests take turns, so that the port one of them frees is not handed to another that is starting.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    fn turn() -> MutexGuard<'static, ()> {
        ONE_AT_A_TIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// One request as a plain HTTP client: returns the status line and the whole answer text.
    fn post(port: u16, path: &str, body: &str) -> (String, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connects");
        stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).expect("writes");
        let mut answer = String::new();
        stream.read_to_string(&mut answer).expect("reads to the close");
        (answer.lines().next().unwrap_or_default().to_string(), answer)
    }

    #[test]
    fn scripted_model_serves_sse_and_records_bodies() {
        let _turn = turn();
        let model = ScriptedModel::start(vec![
            Step::exec("c1", "echo hi", false)
                .when_body_has("FIRST")
                .unless_body_has("c1"),
            Step::say("second").when_body_has("c1"),
        ])
        .expect("starts");
        assert!(model.port > 0);

        let (status, answer) = post(model.port, "/v1/responses", r#"{"input":"FIRST"}"#);
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert!(answer.contains("text/event-stream"));
        assert!(answer.contains("event: response.created\ndata: "));
        // The call carries its id and the tool's name, and the response id replaced the placeholder.
        assert!(answer.contains(r#""call_id":"c1""#) && answer.contains(r#""name":"exec_command""#));
        assert!(answer.contains(r#""id":"resp_1""#) && !answer.contains("{RID}"));
        assert!(answer.contains("event: response.completed"));

        // The second request fits the second step only; the first is spent.
        let (_, answer) = post(model.port, "/v1/responses", r#"{"input":"FIRST c1"}"#);
        assert!(answer.contains("second"), "{answer}");
        // Nothing left: one assistant message, and the turn ends.
        let (_, answer) = post(model.port, "/v1/responses", r#"{"input":"third"}"#);
        assert!(answer.contains("\"text\":\"done\"") && answer.contains("response.completed"));
        assert_eq!(model.steps_left(), 0);

        // Anything but a POST to responses is a 404, and is not recorded.
        let mut stream = TcpStream::connect(("127.0.0.1", model.port)).expect("connects");
        stream
            .write_all(b"GET /v1/models HTTP/1.1\r\nHost: x\r\n\r\n")
            .expect("writes");
        let mut answer = String::new();
        stream.read_to_string(&mut answer).expect("reads");
        assert!(answer.starts_with("HTTP/1.1 404"), "{answer}");

        let seen = model.requests();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0], json!({"input": "FIRST"}));
        assert_eq!(seen[2]["input"], "third");
    }

    #[test]
    fn a_step_with_conditions_waits_for_the_request_it_describes() {
        let _turn = turn();
        let model = ScriptedModel::start(vec![Step::say("only for child")
            .when_body_has("CHILD")
            .unless_body_has("PARENT")])
        .expect("starts");
        let (_, answer) = post(model.port, "/v1/responses", r#"{"input":"PARENT CHILD"}"#);
        assert!(answer.contains("\"text\":\"done\""));
        assert_eq!(model.steps_left(), 1);
        let (_, answer) = post(model.port, "/v1/responses", r#"{"input":"CHILD"}"#);
        assert!(answer.contains("only for child"));
        assert_eq!(model.steps_left(), 0);
    }

    #[test]
    fn a_chunked_body_and_a_non_json_body_are_recorded() {
        let _turn = turn();
        let model = ScriptedModel::start(Vec::new()).expect("starts");
        let mut stream = TcpStream::connect(("127.0.0.1", model.port)).expect("connects");
        stream
            .write_all(
                b"POST /v1/responses HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n2\r\n1}\r\n0\r\n\r\n",
            )
            .expect("writes");
        let mut answer = String::new();
        stream.read_to_string(&mut answer).expect("reads");
        assert!(answer.starts_with("HTTP/1.1 200"));
        let (_, _) = post(model.port, "/v1/responses", "not json");
        let seen = model.requests();
        assert_eq!(seen[0], json!({"a": 1}));
        assert_eq!(seen[1]["unparsed"], "not json");
    }

    #[test]
    fn dropping_the_model_ends_its_threads() {
        let _turn = turn();
        let model = ScriptedModel::start(Vec::new()).expect("starts");
        let (_, answer) = post(model.port, "/v1/responses", "{}");
        assert!(answer.contains("done"));
        // The accept thread and every connection thread hold the shared state; once they are gone only this
        // test does. (Connecting to the port again would prove nothing: another test may have been handed it.)
        let shared = Arc::clone(&model.inner);
        drop(model);
        let end = Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&shared) > 1 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(Arc::strong_count(&shared), 1);
    }
}
