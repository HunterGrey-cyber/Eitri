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

fn first_sequence_message(sequence: u64) -> String {
    format!("the recording starts at sequence {sequence}, not 1: events before it were gone when the recorder attached")
}

async fn pump(mut stream: tonic::Streaming<SessionEvent>, shared: Arc<Shared>, mut bin: File, mut txt: File) {
    loop {
        let next = stream.message().await;
        let mut state = shared.state.lock().unwrap();
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
    fn an_event_without_a_body_is_described() {
        assert_eq!(describe(&SessionEvent::default()), "(empty)");
    }
}
