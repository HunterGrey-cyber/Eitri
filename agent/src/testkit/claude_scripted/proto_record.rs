//! A second watcher of one sidecar session that writes every wire event it sees to disk.
//!
//! The provider's own watch translates events into domain events and forgets the wire form. The recorder opens
//! its own `WatchSessionEvents` for the same session, asking for every retained event, and keeps the exact
//! frames: length-delimited protobuf in `proto.bin` and one `Debug` line per event in `proto.txt`. The first
//! event it receives must carry sequence 1: anything later means the recorder attached after events were
//! already gone, and a recording that starts mid-session cannot be replayed.

use claude_runtime_protocol::v1::runtime_service_client::RuntimeServiceClient;
use claude_runtime_protocol::v1::session_event::Event;
use claude_runtime_protocol::v1::{ReplayStart, SessionEvent, WatchSessionEventsRequest};
use hyper_util::rt::TokioIo;
use prost::Message;
use std::{
    collections::VecDeque,
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};
use tokio::net::UnixStream;
use tonic::transport::{Endpoint, Uri};
use tower::service_fn;

/// How many one-line descriptions of the latest events a failure report can show.
const TAIL: usize = 20;

/// How far the recording has got: whole events written, their bytes in `proto.bin`, and the newest sequence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Position {
    pub events: usize,
    pub bytes: u64,
    pub sequence: u64,
}

#[derive(Default)]
struct State {
    position: Position,
    first_sequence: Option<u64>,
    events: Vec<SessionEvent>,
    tail: VecDeque<String>,
    /// Why the watch stopped, when it did not end because the session closed.
    error: Option<String>,
    /// The stream ended.
    ended: bool,
    /// The recording was cut where its host stopped taking events: nothing the watch sees later is written.
    cut: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

/// The recorder of one session. Dropping it ends the watch.
pub struct ProtoRecorder {
    shared: Arc<Shared>,
    runtime: Option<tokio::runtime::Runtime>,
    bin: PathBuf,
    txt: PathBuf,
}

impl ProtoRecorder {
    /// Opens the watch of `session_id` on the sidecar socket and starts writing `<dir>/<stem>.bin` and
    /// `<dir>/<stem>.txt`. Returns once the sidecar has accepted the watch.
    pub fn start(socket: &Path, session_id: &str, dir: &Path, stem: &str) -> io::Result<Self> {
        crate::private_fs::create_private_dir_all(dir, dir)?;
        let bin_path = dir.join(format!("{stem}.bin"));
        let txt_path = dir.join(format!("{stem}.txt"));
        let bin = crate::private_fs::open_private(&bin_path)?;
        let txt = crate::private_fs::open_private(&txt_path)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        let socket = socket.to_path_buf();
        let session = session_id.to_owned();
        let stream = runtime.block_on(async move {
            let channel = Endpoint::try_from("http://[::]:50051")
                .map_err(io::Error::other)?
                .connect_with_connector(service_fn(move |_: Uri| {
                    let socket = socket.clone();
                    async move {
                        let stream = UnixStream::connect(socket).await?;
                        crate::socket_path::require_own_peer(stream.peer_cred()?.uid())?;
                        Ok::<_, io::Error>(TokioIo::new(stream))
                    }
                }))
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::ConnectionRefused, e.to_string()))?;
            let mut client = RuntimeServiceClient::new(channel);
            let response = client
                .watch_session_events(WatchSessionEventsRequest {
                    session_id: session,
                    start: ReplayStart::AvailableHistory as i32,
                    after_sequence: None,
                })
                .await
                .map_err(|status| io::Error::other(format!("the recorder's watch was refused: {status}")))?;
            Ok::<_, io::Error>(response.into_inner())
        })?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        let worker = Arc::clone(&shared);
        runtime.spawn(async move { pump(stream, worker, bin, txt).await });
        Ok(Self {
            shared,
            runtime: Some(runtime),
            bin: bin_path,
            txt: txt_path,
        })
    }

    pub fn bin_path(&self) -> &Path {
        &self.bin
    }

    pub fn position(&self) -> Position {
        self.shared.state.lock().unwrap().position
    }

    /// Every event recorded so far, in order.
    pub fn events(&self) -> Vec<SessionEvent> {
        self.shared.state.lock().unwrap().events.clone()
    }

    /// One short line per newest event, oldest first.
    pub fn tail(&self) -> Vec<String> {
        self.shared.state.lock().unwrap().tail.iter().cloned().collect()
    }

    /// Why the watch stopped early, if it did.
    pub fn failure(&self) -> Option<String> {
        let state = self.shared.state.lock().unwrap();
        state
            .error
            .clone()
            .or_else(|| state.first_sequence.filter(|s| *s != 1).map(first_sequence_message))
    }

    /// How many of the CLI's session state changes the recording holds: the sidecar passes each
    /// `system`/`session_state_changed` line on as a provider notice of that subtype.
    pub fn state_changes(&self) -> usize {
        let state = self.shared.state.lock().unwrap();
        state.events.iter().filter(|e| is_state_change(e)).count()
    }

    /// Waits until the recording holds the event with `sequence` (or a later one), up to `limit`.
    pub fn wait_through(&self, sequence: u64, limit: Duration) -> Result<(), String> {
        let end = Instant::now() + limit;
        let mut state = self.shared.state.lock().unwrap();
        while state.position.sequence < sequence {
            if let Some(why) = &state.error {
                return Err(why.clone());
            }
            if state.ended {
                return Err(format!("the stream ended at sequence {}", state.position.sequence));
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!(
                    "the recording stopped at sequence {} while waiting for {sequence}",
                    state.position.sequence
                ));
            }
            state = self.shared.changed.wait_timeout(state, left).unwrap().0;
        }
        Ok(())
    }

    /// Ends the recording at `sequence`, the last event its host took: the session goes on emitting after the host
    /// has stopped reading (the CLI can report itself ready again once its last turn is over), and a recording that
    /// kept those events would replay to more than the host was handed. Events past `sequence` leave memory and both
    /// files, and the watch writes nothing more. Fails when the recording has not reached `sequence`.
    pub fn cut_at(&self, sequence: u64) -> Result<(), String> {
        let mut state = self.shared.state.lock().unwrap();
        if state.position.sequence < sequence {
            return Err(format!(
                "the recording is at sequence {} and cannot end at {sequence}",
                state.position.sequence
            ));
        }
        state.cut = true;
        keep_through(&mut state, &self.bin, &self.txt, sequence)
            .map_err(|e| format!("cannot cut the recording at sequence {sequence}: {e}"))
    }

    /// Stops the watch and checks the recording: it must have started at sequence 1 and must not have failed.
    pub fn finish(mut self) -> Result<Vec<SessionEvent>, String> {
        let outcome = match self.failure() {
            Some(why) => Err(why),
            None if self.position().events == 0 => Err("the recording holds no event".to_owned()),
            None => Ok(self.events()),
        };
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
        outcome
    }
}

impl Drop for ProtoRecorder {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// Drops every event after `sequence` from `state` and truncates `bin` and `txt` to the events kept: each event is
/// one length-delimited frame in `bin` and one line in `txt`, written in order.
fn keep_through(state: &mut State, bin: &Path, txt: &Path, sequence: u64) -> io::Result<()> {
    let kept = state.events.iter().take_while(|e| e.sequence <= sequence).count();
    if kept == state.events.len() {
        return Ok(());
    }
    state.events.truncate(kept);
    let bytes: u64 = state
        .events
        .iter()
        .map(|e| e.encode_length_delimited_to_vec().len() as u64)
        .sum();
    std::fs::OpenOptions::new().write(true).open(bin)?.set_len(bytes)?;
    let text = std::fs::read_to_string(txt)?;
    let lines: usize = text.split_inclusive('\n').take(kept).map(str::len).sum();
    std::fs::OpenOptions::new()
        .write(true)
        .open(txt)?
        .set_len(lines as u64)?;
    state.position = Position {
        events: kept,
        bytes,
        sequence: state.events.last().map_or(0, |e| e.sequence),
    };
    Ok(())
}

fn first_sequence_message(sequence: u64) -> String {
    format!("the recording starts at sequence {sequence}, not 1: events before it were gone when the recorder attached")
}

async fn pump(mut stream: tonic::Streaming<SessionEvent>, shared: Arc<Shared>, mut bin: File, mut txt: File) {
    loop {
        let next = stream.message().await;
        let mut state = shared.state.lock().unwrap();
        if state.cut {
            return;
        }
        match next {
            Ok(Some(event)) => {
                state.first_sequence.get_or_insert(event.sequence);
                let frame = event.encode_length_delimited_to_vec();
                let line = format!("{} {}", event.sequence, describe(&event));
                let written = bin
                    .write_all(&frame)
                    .and_then(|()| bin.flush())
                    .and_then(|()| writeln!(txt, "{event:?}"))
                    .and_then(|()| txt.flush());
                if let Err(e) = written {
                    state.error = Some(format!("cannot write the recording: {e}"));
                    shared.changed.notify_all();
                    return;
                }
                state.position.events += 1;
                state.position.bytes += frame.len() as u64;
                state.position.sequence = event.sequence;
                if state.tail.len() == TAIL {
                    state.tail.pop_front();
                }
                state.tail.push_back(line);
                state.events.push(event);
            }
            Ok(None) => state.ended = true,
            Err(status) => state.error = Some(format!("the recorder's stream failed: {status}")),
        }
        let done = state.ended || state.error.is_some();
        shared.changed.notify_all();
        if done {
            return;
        }
    }
}

/// A provider notice for one of the CLI's `session_state_changed` lines.
pub fn is_state_change(event: &SessionEvent) -> bool {
    matches!(&event.event, Some(Event::ProviderNotice(notice)) if notice.subtype.as_deref() == Some("session_state_changed"))
}

/// The kind of an event, with the one field that tells similar events apart.
pub fn describe(event: &SessionEvent) -> String {
    match &event.event {
        Some(Event::SessionReady(_)) => "SessionReady".into(),
        Some(Event::TurnStarted(_)) => "TurnStarted".into(),
        Some(Event::TextDelta(_)) => "TextDelta".into(),
        Some(Event::ThinkingDelta(_)) => "ThinkingDelta".into(),
        Some(Event::ToolCallStarted(started)) => format!("ToolCallStarted({})", started.name),
        Some(Event::ToolCallCompleted(completed)) => format!("ToolCallCompleted(is_error={})", completed.is_error),
        Some(Event::PermissionRequested(requested)) => format!("PermissionRequested({})", requested.tool_name),
        Some(Event::PermissionResolved(resolved)) => format!("PermissionResolved(outcome={})", resolved.outcome),
        Some(Event::TurnCompleted(completed)) => format!("TurnCompleted(outcome={})", completed.outcome),
        Some(Event::SessionClosed(closed)) => format!("SessionClosed(reason={})", closed.reason),
        Some(Event::ProviderNotice(notice)) => format!("ProviderNotice({})", notice.kind),
        Some(other) => format!("{other:?}").chars().take(60).collect(),
        None => "(empty)".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recording_that_starts_after_sequence_one_is_reported_with_both_numbers() {
        let message = first_sequence_message(7);
        assert!(message.contains("sequence 7") && message.contains("not 1"), "{message}");
    }

    #[test]
    fn only_a_state_change_notice_counts_as_a_state_change() {
        use claude_runtime_protocol::v1::ProviderNotice;
        let notice = |subtype: Option<&str>| SessionEvent {
            event: Some(Event::ProviderNotice(ProviderNotice {
                kind: "system".into(),
                subtype: subtype.map(str::to_owned),
            })),
            ..SessionEvent::default()
        };
        assert!(is_state_change(&notice(Some("session_state_changed"))));
        assert!(!is_state_change(&notice(Some("status"))));
        assert!(!is_state_change(&notice(None)));
        assert!(!is_state_change(&SessionEvent::default()));
    }

    #[test]
    fn a_cut_keeps_the_frames_and_lines_through_its_sequence() {
        let dir = std::env::temp_dir().join(format!("proto-cut-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (bin, txt) = (dir.join("proto.bin"), dir.join("proto.txt"));
        let events: Vec<SessionEvent> = (1..=4)
            .map(|sequence| SessionEvent {
                sequence,
                session_id: format!("s\n{sequence}"),
                ..SessionEvent::default()
            })
            .collect();
        let frames = |events: &[SessionEvent]| -> Vec<u8> {
            events.iter().flat_map(|e| e.encode_length_delimited_to_vec()).collect()
        };
        let lines = |events: &[SessionEvent]| -> String { events.iter().map(|e| format!("{e:?}\n")).collect() };
        std::fs::write(&bin, frames(&events)).unwrap();
        std::fs::write(&txt, lines(&events)).unwrap();
        let mut state = State {
            events: events.clone(),
            position: Position {
                events: 4,
                bytes: frames(&events).len() as u64,
                sequence: 4,
            },
            ..State::default()
        };

        keep_through(&mut state, &bin, &txt, 2).unwrap();

        assert_eq!(std::fs::read(&bin).unwrap(), frames(&events[..2]));
        assert_eq!(std::fs::read_to_string(&txt).unwrap(), lines(&events[..2]));
        assert_eq!(state.events, events[..2]);
        assert_eq!(
            state.position,
            Position {
                events: 2,
                bytes: frames(&events[..2]).len() as u64,
                sequence: 2
            }
        );
        // Cutting at the end the recording already has changes nothing.
        keep_through(&mut state, &bin, &txt, 2).unwrap();
        assert_eq!(std::fs::read(&bin).unwrap(), frames(&events[..2]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_event_without_a_body_is_described() {
        assert_eq!(describe(&SessionEvent::default()), "(empty)");
    }
}
