#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent::{AgentDomainEvent, AgentSessionProjection, ContentKind};
use eitri_core::agent_backend::ProjectionRef;
use eitri_core::agent_bridge::{
    serialize_events_with_notes_and_revisions_for_js, serialize_snapshot_for_js, CallNotes, SnapshotView,
};
use eitri_core::agents::{ProjectRef, Publisher, Row, WindowRef};
use eitri_core::remote_greeting::{greeting_line, Greeting, PendingPrompt};
use eitri_core::remote_rv::{chunk_lines, frame_line, CutEntry, RvListener, RvOutbound, RV_MAX_LINE};
use eitri_core::tab_set::{GoneWhy, ObserverFrame, RowKey, WorkMark};
use eitri_core::tabs::TabId;
use serde_json::{json, Value};

use super::outbound::{Next, Outbound, Pushed, Source, PHONE_OUT_CAP_BYTES};
use super::session::{Hub, Limits};

const KEY: &str = "k-0123456789abcdef";
const INSTANCE: &str = "0123456789abcdef";

#[test]
fn phone_fixtures_wire_bytes_match() {
    for (name, lines) in window_lines() {
        let out = Outbound::new(PHONE_OUT_CAP_BYTES, |_| panic!("the queue overflowed"));
        out.hear(1);
        out.route(KEY, Some(1));
        out.heard(2, 0, &mut 0).unwrap();
        let messages: Vec<_> = lines
            .iter()
            .map(|line| {
                assert_eq!(
                    out.push(Source::Upstream(1), Some(KEY), watch_line(line)),
                    Pushed::Queued
                );
                let Next::Write { bytes, .. } = out.next() else {
                    panic!("a queued message")
                };
                read_message(&mut &bytes[..])
            })
            .collect();
        fixture(&name, &messages);
    }
}

#[test]
fn phone_fixtures_session_reassembles_and_rewrites_watch_frames() {
    for (name, lines) in window_lines() {
        fixture(&name, &through_session(lines));
    }
}

fn key() -> RowKey {
    RowKey::parse(KEY).unwrap()
}

fn mark(generation: u64) -> WorkMark {
    WorkMark {
        generation,
        unseen: None,
    }
}

fn delta(text: &str) -> AgentDomainEvent {
    AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: ContentKind::Text,
        text: text.into(),
    }
}

fn snapshot(projection: &AgentSessionProjection) -> String {
    serialize_snapshot_for_js(
        TabId(1),
        &SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: Some("session"),
            provider_session_id: None,
            capabilities: Default::default(),
            provider: None,
            projection: ProjectionRef::Borrowed(projection),
            hidden_pending: None,
        },
        None,
    )
}

fn snapshot_line(json: String, through: u64, first_turn: u64, cut: Vec<CutEntry>, generation: u64) -> String {
    frame_line(&ObserverFrame::Snapshot {
        key: key(),
        through,
        work: mark(generation),
        first_turn,
        cut,
        json: Arc::from(json),
    })
}

fn window_lines() -> Vec<(&'static str, Vec<String>)> {
    let theme = json!({"vars": {"--nv-bg": "#112233", "--nv-fg": "#ddeeff"}, "scheme": "dark"});
    let greeting = greeting_line(
        &key(),
        &Greeting {
            tab: json!({"id":1,"agent":"claude","mode":"manual"}),
            prompt: Some(PendingPrompt::Trust),
            root_note: Some("The project folder is no boundary".into()),
            notices: vec!["Waiting for the desk".into()],
            shell_running: true,
            theme: theme.clone(),
            send_block: None,
        },
    );
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::UserPromptSubmitted {
        text: "Say hello".into(),
    });
    projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    let mut live = vec![
        greeting,
        snapshot_line(snapshot(&projection), projection.last_revision, 0, vec![], 1),
    ];
    for text in ["Hello", " phone"] {
        let from = projection.last_revision;
        let event = delta(text);
        projection.apply(&event);
        live.push(frame_line(&ObserverFrame::Events {
            key: key(),
            from,
            through: projection.last_revision,
            work: mark(1),
            json: Arc::from(serialize_events_with_notes_and_revisions_for_js(
                TabId(1),
                from,
                projection.last_revision,
                &[event],
                &CallNotes::default(),
                Some(&[projection.last_revision]),
            )),
        }));
    }
    live.push(
        RvOutbound::SendBlock {
            v: 1,
            key: KEY.into(),
            block: None,
        }
        .line(),
    );
    live.push(RvOutbound::Theme { v: 1, theme }.line());
    live.push(frame_line(&ObserverFrame::Ended {
        key: key(),
        how: eitri_core::remote_rv::EndedHow::Lost,
    }));
    live.push(frame_line(&ObserverFrame::Gone {
        key: key(),
        why: GoneWhy::Closed,
    }));

    let mut history = AgentSessionProjection::default();
    for (prompt, reply) in [
        ("zero", "older".into()),
        ("one", "é\"\\\n".repeat(RV_MAX_LINE / 8 + 1)),
        ("two", "latest".into()),
    ] {
        history.apply(&AgentDomainEvent::UserPromptSubmitted { text: prompt.into() });
        history.apply(&delta(&reply));
        history.apply(&AgentDomainEvent::AssistantMessageBoundary { turn_id: "t1".into() });
    }
    let whole: Value = serde_json::from_str(&snapshot(&history)).unwrap();
    let mut tail = between(&whole, 3, u64::MAX);
    tail["state"]["transcript"][0]["text"] = json!("start of a large reply");
    let page = |from_turn, through_turn, lo, hi| {
        frame_line(&ObserverFrame::Page {
            key: key(),
            mark: mark(1),
            from_turn,
            through_turn,
            json: Arc::from(between(&whole, lo, hi).to_string()),
        })
    };
    let older = page(0, 0, 0, 3);
    let cut_page = page(1, 1, 3, 6);
    assert!(chunk_lines(1, &cut_page).len() > 1, "the large reply uses rv chunks");
    vec![
        ("live", live),
        (
            "history",
            vec![
                snapshot_line(
                    tail.to_string(),
                    history.last_revision,
                    1,
                    vec![CutEntry {
                        entry_id: "transcript:4".into(),
                        bytes: RV_MAX_LINE as u64 * 2,
                    }],
                    1,
                ),
                older.clone(),
                older.clone(),
                cut_page,
                snapshot_line(snapshot(&projection), projection.last_revision, 0, vec![], 2),
                older,
            ],
        ),
    ]
}

fn between(envelope: &Value, lo: u64, hi: u64) -> Value {
    let mut envelope = envelope.clone();
    for name in [
        "userPrompts",
        "transcript",
        "toolCalls",
        "thinking",
        "turnStarts",
        "notices",
        "turnEndings",
        "questions",
    ] {
        if let Some(entries) = envelope["state"].get_mut(name).and_then(Value::as_array_mut) {
            entries.retain(|entry| entry["seq"].as_u64().is_some_and(|seq| (lo..hi).contains(&seq)));
        }
    }
    envelope
}

// The byte oracle is checked separately against a full session, including its watch-only rewrite.
fn watch_line(line: &str) -> String {
    match eitri_core::remote_rv::parse_rv_outbound(line.as_bytes()).unwrap() {
        RvOutbound::SendBlock { key, .. } => RvOutbound::SendBlock {
            v: 1,
            key,
            block: Some(eitri_core::remote_rv::SendBlock::WatchOnly),
        }
        .line(),
        _ => line.to_owned(),
    }
}

fn fixture(name: &str, messages: &[String]) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../agent-ui/web/src/fixtures/phone-frames")
        .join(format!("{name}.json"));
    let bytes = format!("[\n{}\n]\n", messages.join(",\n"));
    if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|value| value == "1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    } else {
        let expected = std::fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert!(
            expected == bytes.as_bytes(),
            "{} differs from the phone's messages",
            path.display()
        );
    }
}

/// Reads one complete WebSocket text message, including continuation frames.
fn read_message(reader: &mut impl Read) -> String {
    let mut message = Vec::new();
    loop {
        let mut head = [0; 2];
        reader.read_exact(&mut head).expect("a WebSocket frame header");
        assert_eq!(head[1] & 0x80, 0, "server frames are unmasked");
        assert_eq!(head[0] & 0xf, if message.is_empty() { 1 } else { 0 });
        let len = match head[1] {
            126 => {
                let mut bytes = [0; 2];
                reader.read_exact(&mut bytes).unwrap();
                u16::from_be_bytes(bytes) as usize
            }
            127 => {
                let mut bytes = [0; 8];
                reader.read_exact(&mut bytes).unwrap();
                u64::from_be_bytes(bytes) as usize
            }
            len => len as usize,
        };
        let old = message.len();
        message.resize(old + len, 0);
        reader
            .read_exact(&mut message[old..])
            .expect("a whole WebSocket payload");
        if head[0] & 0x80 != 0 {
            break;
        }
    }
    let message = String::from_utf8(message).unwrap();
    let parsed: Value = serde_json::from_str(&message).unwrap();
    assert_ne!(parsed["kind"], "chunk", "rv chunks never reach the phone");
    message
}

fn send_phone(stream: &mut UnixStream, value: Value) {
    let payload = value.to_string().into_bytes();
    let mut frame = vec![0x81];
    if payload.len() <= 125 {
        frame.push(0x80 | payload.len() as u8);
    } else {
        frame.push(0xfe);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    // A deterministic mask is sufficient for a test client.
    frame.extend_from_slice(&[0; 4]);
    frame.extend(payload);
    stream.write_all(&frame).unwrap();
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(
                    option_env!("CARGO_TARGET_DIR").unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../target")),
                )
            });
        let path = root.join(format!(
            "pf-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        agent::private_fs::create_private_dir_all(&path, &root).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn through_session(lines: Vec<String>) -> Vec<String> {
    use std::sync::{mpsc, Mutex};
    let scratch = Scratch::new();
    let control = scratch.0.join("run");
    agent::private_fs::create_private_dir_all(&control, &scratch.0).unwrap();
    let store = scratch.0.join("devices.json");
    agent::private_fs::write_private(&store, json!({"v":1,"devices":[{
        "id":"d1","name":"phone","token_sha256":eitri_core::remote_devices::token_hash_hex("fixture-token"),"grant":"watch"
    }]}).to_string().as_bytes()).unwrap();
    let (advance, advanced) = mpsc::sync_channel(1);
    let pending = Arc::new(Mutex::new(Some((lines.clone(), advanced))));
    let listener = RvListener::bind(&control, INSTANCE, move |stream| {
        let pending = pending.clone();
        std::thread::spawn(move || {
            stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(30))).unwrap();
            let mut writer = stream.try_clone().unwrap();
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else {
                    break;
                };
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    break;
                };
                if value["kind"] != "subscribe" {
                    continue;
                }
                let Some((lines, advanced)) = pending.lock().unwrap().take() else {
                    break;
                };
                for (at, line) in lines.iter().enumerate() {
                    for piece in chunk_lines(at as u64, line) {
                        if writeln!(writer, "{piece}").is_err() {
                            return;
                        }
                    }
                    if advanced.recv_timeout(Duration::from_secs(30)).is_err() {
                        return;
                    }
                }
            }
        });
        true
    })
    .expect("the fake window's private rv socket binds");
    let window = WindowRef {
        instance: INSTANCE.into(),
        name: "desk".into(),
        focused: true,
    };
    let (published, publication) = mpsc::channel();
    let publisher = Publisher::start_with(
        &control,
        INSTANCE,
        ProjectRef::default(),
        window.clone(),
        eitri_core::agents::publish::PublisherHooks {
            on_publish: Some(Box::new(move |file| {
                if !file.rows.is_empty() {
                    let _ = published.send(());
                }
            })),
            ..Default::default()
        },
    )
    .unwrap();
    publisher.offer_now(vec![Row {
        id: format!("{INSTANCE}.1"),
        key: KEY.into(),
        agent: "claude".into(),
        window,
        state: "idle".into(),
        ..Default::default()
    }]);
    publication.recv_timeout(Duration::from_secs(30)).unwrap();
    let hub = Hub::start(
        &control,
        &store,
        Limits {
            rv_ping: Duration::from_secs(60),
            ..Default::default()
        },
    )
    .unwrap();
    let (server, mut phone) = UnixStream::pair().unwrap();
    phone.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    phone.set_write_timeout(Some(Duration::from_secs(30))).unwrap();
    let serving = std::thread::spawn(move || hub.serve_phone(Box::new(server), "desk".into(), "phone".into()));
    send_phone(
        &mut phone,
        json!({"v":1,"seq":1,"ack":0,"kind":"hello","device":"d1","token":"fixture-token","layout":"phone","protocol":1}),
    );
    send_phone(&mut phone, json!({"v":1,"seq":2,"ack":0,"kind":"subscribe","key":KEY}));
    let mut messages = Vec::new();
    for _ in &lines {
        messages.push(read_message(&mut phone));
        advance.send(()).unwrap();
    }
    phone.shutdown(std::net::Shutdown::Both).unwrap();
    serving.join().unwrap();
    publisher.stop();
    drop(listener);
    messages
}
