use std::io;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use claude_runtime_protocol::v1::{session_event::Event, ReplayStart, SessionEvent};
use eitri_server::journal::{Journal, Pending, ReadError, Record};
use prost::Message;
use tonic::{Code, Status};

use super::{blocking, proxy, replay, SharedSession};
use crate::run_dir::Paths;

/// Whether the sidecar a watch follows is still the same running process.
pub(super) type Live = Arc<dyn Fn() -> bool + Send + Sync>;

/// Reconnects after a failure that delivered nothing back off from this to `RETRY_MAX`.
const RETRY_FIRST: Duration = Duration::from_millis(100);
const RETRY_MAX: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Start {
    /// After the journal's head: every later event, or the sidecar's refusal if it no longer holds the next one.
    AfterHead,
    /// Whatever history the sidecar still holds; the journal records the sequences it lacks as a gap.
    Available,
}

/// `Ok(false)` when the event leaves Pending as it is, so no write was tried.
pub(super) fn settle(pending: &mut Pending, event: &SessionEvent, bytes: &[u8]) -> io::Result<bool> {
    let sequence = event.sequence;
    match &event.event {
        Some(Event::PermissionRequested(request)) => pending.request(sequence, bytes, &request.permission_id)?,
        Some(Event::PermissionResolved(resolved)) => pending.resolved(sequence, &resolved.permission_id)?,
        Some(Event::TurnCompleted(_) | Event::SessionClosed(_)) => pending.clear(sequence)?,
        _ => return Ok(false),
    }
    Ok(true)
}

/// Folds the journal after Pending's watermark into Pending; returns whether the session closed.
pub(super) fn recover(journal: &Journal, pending: &mut Pending) -> io::Result<bool> {
    // Every update up to the watermark is in Pending already, including an answer the journal could not keep.
    // The event at the watermark is read again only to learn whether it closed the session.
    let applied = pending.applied();
    let mut cursor = applied.saturating_sub(1);
    let mut closed = false;
    loop {
        match journal.read_after(cursor, replay::BATCH_BYTES) {
            Ok(records) if records.is_empty() => return Ok(closed),
            Ok(records) => {
                for record in records {
                    if let Record::Event { sequence, bytes } = record {
                        let event = SessionEvent::decode(bytes.as_slice()).map_err(io::Error::other)?;
                        if sequence > applied {
                            // A failed pending write keeps the change in memory; the next write persists it.
                            let _ = settle(pending, &event, &bytes);
                        }
                        closed |= matches!(event.event, Some(Event::SessionClosed(_)));
                        cursor = sequence;
                    }
                }
            }
            Err(ReadError::Gap { last, .. }) => cursor = last,
            Err(ReadError::Io(error)) => return Err(error),
        }
    }
}

/// Follows one session until it closes or cannot continue.
pub(super) async fn run(paths: Paths, info: eitri_server_proto::SidecarInfo, session: SharedSession, live: Live) {
    let mut start = Start::AfterHead;
    let mut retry = RETRY_FIRST;
    let mut failing = Failing::default();
    loop {
        let mut delivered = false;
        let mut refused = false;
        let result = watch(&paths, &info, &session, start, &mut delivered, &mut failing).await;
        let shared = session.clone();
        if blocking(move || Ok(shared.lock().expect("session").closed))
            .await
            .unwrap_or(false)
        {
            break;
        }
        if delivered {
            start = Start::AfterHead;
            retry = RETRY_FIRST;
        }
        let mut end = match result {
            // Refused before anything arrived: the sidecar no longer holds the event after the head, so its
            // remaining history is taken once, at once: the refusal is an answer, not a failure, and the ring
            // keeps evicting while the watch waits. A gap that ends a stream which was delivering (a lagging
            // watch) says nothing about the next event, so the watch reopens after the head.
            Err(status) if replay::is_gap(&status) => {
                if !delivered && start == Start::AfterHead {
                    start = Start::Available;
                    refused = true;
                }
                None
            }
            Err(status) if status.code() == Code::NotFound => {
                Some(replay::session_gone("the sidecar no longer has this session"))
            }
            Err(status) if status.code() == Code::InvalidArgument => Some(replay::refusal(
                Code::FailedPrecondition,
                claude_runtime_protocol::v1::ErrorCode::InvalidConfiguration,
                status.message(),
                "session_observation_ended",
            )),
            Ok(()) | Err(_) => None,
        };
        if end.is_none() && !delivered {
            let live = live.clone();
            if !blocking(move || Ok(live())).await.unwrap_or(true) {
                end = Some(replay::session_gone("the sidecar has stopped"));
            }
        }
        if let Some(status) = end {
            let shared = session.clone();
            let _ = blocking(move || {
                let mut state = shared.lock().expect("session");
                state.error = Some(status);
                state.changed.send_modify(|_| {});
                Ok(())
            })
            .await;
            break;
        }
        if refused {
            retry = RETRY_FIRST;
        } else if !delivered {
            tokio::time::sleep(retry).await;
            retry = (retry * 2).min(RETRY_MAX);
        }
    }
    let _ = blocking(move || {
        session.lock().expect("session").journal.finish_sync();
        Ok(())
    })
    .await;
}

/// Whether the journal and the pending file are each failing to write; only the first failure of a run is audited.
#[derive(Default)]
struct Failing {
    journal: bool,
    pending: bool,
}

async fn watch(
    paths: &Paths,
    info: &eitri_server_proto::SidecarInfo,
    session: &SharedSession,
    start: Start,
    delivered: &mut bool,
    failing: &mut Failing,
) -> Result<(), Status> {
    let shared = session.clone();
    let (id, head) = blocking(move || {
        let state = shared.lock().expect("session");
        Ok((state.identity.1.clone(), state.journal.head()))
    })
    .await?;
    let mut events = proxy::within_connect_timeout(async {
        let stream = proxy::connect(paths, info).await?;
        let mut stream = Some(stream);
        let channel = tonic::transport::Endpoint::from_static("http://localhost")
            .connect_with_connector(tower::service_fn(move |_| {
                let stream = stream.take();
                async move {
                    stream
                        .map(hyper_util::rt::TokioIo::new)
                        .ok_or_else(|| io::Error::other("watch connection already used"))
                }
            }))
            .await
            .map_err(|error| Status::unavailable(error.to_string()))?;
        let mut client = tonic::client::Grpc::new(channel);
        client
            .ready()
            .await
            .map_err(|error| Status::unavailable(error.to_string()))?;
        client
            .server_streaming(
                tonic::Request::new(envelope(&id, head, start)),
                http::uri::PathAndQuery::from_static("/verdandi.claude.runtime.v1.RuntimeService/WatchSessionEvents"),
                proxy::RawCodec,
            )
            .await
            .map(tonic::Response::into_inner)
    })
    .await?;
    // From here until this watch ends, the session's state is current whenever nothing more is waiting on the stream.
    let _current = Current(session);
    loop {
        use futures_util::FutureExt;
        let next = match events.message().now_or_never() {
            Some(next) => next,
            None => {
                // Everything the sidecar had sent has been taken in; what follows comes live.
                session.current.store(true, std::sync::atomic::Ordering::Release);
                events.message().await
            }
        };
        let Some(bytes) = next? else { break };
        let event = SessionEvent::decode(bytes.clone()).map_err(|error| Status::internal(error.to_string()))?;
        if event.session_id != id || event.sequence == 0 {
            return Err(Status::internal("watch returned an invalid event identity"));
        }
        *delivered = true;
        let shared = session.clone();
        let (closed, journal_failure, pending_write) = blocking(move || {
            let mut state = shared.lock().expect("session");
            if event.sequence <= state.journal.head() {
                return Ok((state.closed, None, None));
            }
            // Events skipped before this one may have started or ended a turn.
            let gap = event.sequence > state.journal.head() + 1;
            state.turn = super::advance(state.turn, gap, &event);
            state.tasks = super::advance_tasks(state.tasks, gap, &event);
            // A journal that cannot write records the loss as a WriteFailed gap and keeps its head moving; the
            // watch goes on with the same stream, and Pending follows every event whatever the journal did.
            let mut journal_failure = state.journal.append(event.sequence, &bytes).err();
            // Pending already holds the effect of every event up to its watermark; an older one is not applied again.
            // `None` here means no Pending write was tried, which neither starts nor ends a run of failures.
            let pending_write = if event.sequence > state.pending.applied() {
                match settle(&mut state.pending, &event, &bytes) {
                    Ok(written) => written.then_some(None),
                    Err(error) => Some(Some(error.to_string())),
                }
            } else {
                None
            };
            if matches!(event.event, Some(Event::TurnCompleted(_) | Event::SessionClosed(_))) {
                journal_failure = journal_failure.or(state.journal.turn_ended().err());
            }
            if matches!(event.event, Some(Event::SessionClosed(_))) {
                state.closed = true;
            }
            let head = state.journal.head();
            state.changed.send_replace(head);
            Ok((
                state.closed,
                journal_failure.map(|error| error.to_string()),
                pending_write,
            ))
        })
        .await?;
        audit_failure(paths, &id, "journal-write", journal_failure, &mut failing.journal).await;
        if let Some(failure) = pending_write {
            audit_failure(paths, &id, "pending-write", failure, &mut failing.pending).await;
        }
        if closed {
            return Ok(());
        }
    }
    Ok(())
}

/// Marks the session not current when the watch that made it current ends, however it ends.
struct Current<'a>(&'a SharedSession);

impl Drop for Current<'_> {
    fn drop(&mut self) {
        self.0.current.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Audits a write failure under `kind` when it is the first of a run of failures.
async fn audit_failure(paths: &Paths, id: &str, kind: &'static str, failure: Option<String>, failing: &mut bool) {
    match failure {
        Some(error) if !*failing => {
            *failing = true;
            let state = paths.state.clone();
            let fields = format!("session={id} error={error}");
            let _ = blocking(move || crate::audit::record(&state, kind, &fields)).await;
        }
        Some(_) => {}
        None => *failing = false,
    }
}

fn envelope(id: &str, head: u64, start: Start) -> Bytes {
    // Only the observation envelope is authored here. Session and permission requests are opaque in the proxy.
    let mut envelope = Vec::new();
    prost::encoding::string::encode(1, &id.to_owned(), &mut envelope);
    let mode = match start {
        Start::AfterHead => ReplayStart::AfterSequence,
        Start::Available => ReplayStart::AvailableHistory,
    } as i32;
    prost::encoding::int32::encode(3, &mode, &mut envelope);
    if start == Start::AfterHead {
        prost::encoding::uint64::encode(4, &head, &mut envelope);
    }
    Bytes::from(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_runtime_protocol::v1::{PermissionRequested, PermissionResolved, SessionClosed, TurnCompleted};

    #[test]
    fn serving_recovery_reconciles_pending_after_a_crash_and_reused_permission_ids() {
        let root = super::super::scratch("recovery");
        let mut journal = Journal::open(&root.join("journal/session"), &JournalConfig::default()).unwrap();
        let mut pending = Pending::open(&root.join("pending/session.bin")).unwrap();
        for sequence in 1..=4 {
            let event = SessionEvent {
                session_id: "session".into(),
                sequence,
                event: Some(if sequence % 2 == 1 {
                    Event::PermissionRequested(PermissionRequested {
                        permission_id: "reused".into(),
                        ..Default::default()
                    })
                } else {
                    Event::PermissionResolved(PermissionResolved {
                        permission_id: "reused".into(),
                        ..Default::default()
                    })
                }),
                ..Default::default()
            };
            let bytes = event.encode_to_vec();
            journal.append(sequence, &bytes).unwrap();
            if sequence < 4 {
                settle(&mut pending, &event, &bytes).unwrap();
            }
        }
        journal.turn_ended().unwrap();
        drop(journal);
        drop(pending);
        let mut journal = Journal::open(&root.join("journal/session"), &JournalConfig::default()).unwrap();
        let mut pending = Pending::open(&root.join("pending/session.bin")).unwrap();
        assert_eq!(pending.list()[0].0, 3);
        assert!(!recover(&journal, &mut pending).unwrap());
        assert_eq!(journal.head(), 4);
        assert!(pending.list().is_empty());
        let request = SessionEvent {
            session_id: "session".into(),
            sequence: 5,
            event: Some(Event::PermissionRequested(PermissionRequested {
                permission_id: "closing".into(),
                ..Default::default()
            })),
            ..Default::default()
        };
        journal.append(5, &request.encode_to_vec()).unwrap();
        settle(&mut pending, &request, &request.encode_to_vec()).unwrap();
        let close = SessionEvent {
            session_id: "session".into(),
            sequence: 6,
            event: Some(Event::SessionClosed(SessionClosed::default())),
            ..Default::default()
        };
        journal.append(6, &close.encode_to_vec()).unwrap();
        journal.turn_ended().unwrap();
        assert!(recover(&journal, &mut pending).unwrap());
        assert!(pending.list().is_empty());
        assert_eq!(journal.head(), 6);
        drop(journal);
        drop(pending);
        // The close is Pending's watermark now, and is still found to have closed the session.
        let journal = Journal::open(&root.join("journal/session"), &JournalConfig::default()).unwrap();
        let mut pending = Pending::open(&root.join("pending/session.bin")).unwrap();
        assert_eq!(pending.applied(), 6);
        assert!(recover(&journal, &mut pending).unwrap());
        drop(journal);
        drop(pending);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn serving_recovery_never_brings_back_a_request_answered_in_a_journal_gap() {
        let root = super::super::scratch("watermark");
        let mut journal = Journal::open(&root.join("journal/session"), &JournalConfig::default()).unwrap();
        let mut pending = Pending::open(&root.join("pending/session.bin")).unwrap();
        for sequence in 1..10 {
            journal.append(sequence, &text(sequence).encode_to_vec()).unwrap();
        }
        let request = asked(10, "p");
        journal.append(10, &request.encode_to_vec()).unwrap();
        settle(&mut pending, &request, &request.encode_to_vec()).unwrap();
        // The answer reaches Pending, while the journal could only record that it lost event 11.
        let answer = event(
            11,
            Event::PermissionResolved(PermissionResolved {
                permission_id: "p".into(),
                ..Default::default()
            }),
        );
        journal.record_gap(11, 11, GapWhy::WriteFailed).unwrap();
        settle(&mut pending, &answer, &answer.encode_to_vec()).unwrap();
        journal.turn_ended().unwrap();
        drop(journal);
        drop(pending);
        let journal = Journal::open(&root.join("journal/session"), &JournalConfig::default()).unwrap();
        let mut pending = Pending::open(&root.join("pending/session.bin")).unwrap();
        assert!(!recover(&journal, &mut pending).unwrap());
        assert!(pending.list().is_empty(), "an answered request came back");
        drop(journal);
        drop(pending);
        std::fs::remove_dir_all(root).unwrap();
    }

    use super::super::fake_sidecar::{End, FakeSidecar, Reply};
    use super::super::Session;
    use super::super::SessionLock;
    use claude_runtime_protocol::v1::{ErrorCode, ErrorDetail, TextDelta};
    use eitri_server::journal::{Disk, FileSystem, GapWhy, JournalConfig, SystemClock};
    use std::sync::atomic::{AtomicBool, Ordering};

    fn event(sequence: u64, event: Event) -> SessionEvent {
        SessionEvent {
            session_id: "session".into(),
            sequence,
            event: Some(event),
            ..Default::default()
        }
    }
    fn text(sequence: u64) -> SessionEvent {
        event(sequence, Event::TextDelta(TextDelta::default()))
    }
    fn asked(sequence: u64, id: &str) -> SessionEvent {
        event(
            sequence,
            Event::PermissionRequested(PermissionRequested {
                permission_id: id.into(),
                ..Default::default()
            }),
        )
    }
    fn gap() -> Status {
        replay::refusal(Code::OutOfRange, ErrorCode::EventGap, "lagging", "")
    }

    fn observed(sidecar: &FakeSidecar, fs: Arc<dyn FileSystem>) -> SharedSession {
        observed_with(sidecar, fs, Arc::new(Disk))
    }

    fn observed_with(sidecar: &FakeSidecar, fs: Arc<dyn FileSystem>, pending_fs: Arc<dyn FileSystem>) -> SharedSession {
        let state = &sidecar.paths.state;
        let journal = Journal::open_with(
            &state.join("journal/session"),
            &JournalConfig::default(),
            fs,
            Arc::new(SystemClock::default()),
        )
        .unwrap();
        let pending = Pending::open_with(&state.join("pending/session.bin"), pending_fs).unwrap();
        Arc::new(SessionLock::new(Session::new(
            ("sidecar".into(), "session".into()),
            journal,
            pending,
            false,
        )))
    }

    fn follow(sidecar: &FakeSidecar, session: &SharedSession, live: Live) -> tokio::task::JoinHandle<()> {
        tokio::spawn(run(sidecar.paths.clone(), sidecar.info.clone(), session.clone(), live))
    }

    fn running() -> Live {
        Arc::new(|| true)
    }

    fn head(session: &SharedSession) -> u64 {
        session.lock().unwrap().journal.head()
    }

    fn pending_sequences(session: &SharedSession) -> Vec<u64> {
        let state = session.lock().unwrap();
        state.pending.list().into_iter().map(|(sequence, _)| sequence).collect()
    }

    /// Waits for `done`; the deadline only guards against a hang.
    async fn until(mut done: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !done() {
            assert!(tokio::time::Instant::now() < deadline, "hang guard");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn serving_a_live_gap_reopens_after_the_head_and_keeps_what_the_sidecar_still_holds() {
        let sidecar = FakeSidecar::start(vec![
            Reply::Events(vec![text(1), text(2), text(3)], End::Fail(gap())),
            Reply::Ring(vec![text(1), text(2), text(3), asked(4, "p")]),
        ])
        .await;
        let session = observed(&sidecar, Arc::new(Disk));
        let task = follow(&sidecar, &session, running());
        until(|| sidecar.requests().len() >= 2 && head(&session) >= 4).await;
        let second = &sidecar.requests()[1];
        assert_eq!(second.start, ReplayStart::AfterSequence as i32);
        assert_eq!(second.after_sequence, Some(3));
        until(|| !pending_sequences(&session).is_empty()).await;
        assert_eq!(pending_sequences(&session), vec![4]);
        let recorded = session.lock().unwrap().journal.read_after(3, usize::MAX).unwrap();
        assert!(matches!(recorded.as_slice(), [Record::Event { sequence: 4, .. }]));
        task.abort();
    }

    #[tokio::test]
    async fn serving_a_refused_cursor_records_only_the_missing_range_then_reopens_after_the_head() {
        let held = || vec![text(6), text(7), text(8)];
        let sidecar = FakeSidecar::start(vec![
            Reply::Events(vec![text(1), text(2)], End::Clean),
            Reply::Ring(held()),
            Reply::Events(held(), End::Clean),
            Reply::Ring(vec![text(6), text(7), text(8), text(9)]),
        ])
        .await;
        let session = observed(&sidecar, Arc::new(Disk));
        let task = follow(&sidecar, &session, running());
        until(|| sidecar.requests().len() >= 4 && head(&session) >= 9).await;
        let requests = sidecar.requests();
        assert_eq!(
            (requests[1].start, requests[1].after_sequence),
            (ReplayStart::AfterSequence as i32, Some(2))
        );
        assert_eq!(
            (requests[2].start, requests[2].after_sequence),
            (ReplayStart::AvailableHistory as i32, None)
        );
        // Once the sidecar delivers again, a reconnect asks after the head rather than for all its history.
        assert_eq!(
            (requests[3].start, requests[3].after_sequence),
            (ReplayStart::AfterSequence as i32, Some(8))
        );
        match session.lock().unwrap().journal.read_after(2, usize::MAX) {
            Err(ReadError::Gap {
                first: 3,
                last: 5,
                why: GapWhy::NotObserved,
            }) => {}
            other => panic!("expected one NotObserved gap 3..=5, got {other:?}"),
        }
        task.abort();
    }

    struct Full(AtomicBool);
    impl FileSystem for Full {
        fn append(&self, path: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
            if self.0.load(Ordering::SeqCst) {
                return Err(io::Error::from_raw_os_error(libc::ENOSPC));
            }
            Disk.append(path, bytes)
        }
    }

    #[tokio::test]
    async fn serving_a_journal_that_cannot_write_keeps_the_stream_and_pending() {
        let sidecar = FakeSidecar::start(vec![Reply::Events(vec![text(1), asked(2, "p"), text(3)], End::Hold)]).await;
        let session = observed(&sidecar, Arc::new(Full(AtomicBool::new(true))));
        let task = follow(&sidecar, &session, running());
        until(|| head(&session) >= 3 || sidecar.requests().len() > 1).await;
        assert_eq!(sidecar.requests().len(), 1, "a journal error must not reopen the watch");
        assert_eq!(pending_sequences(&session), vec![2]);
        assert!(matches!(
            session.lock().unwrap().journal.read_after(0, usize::MAX),
            Err(ReadError::Gap {
                first: 1,
                why: GapWhy::WriteFailed,
                ..
            })
        ));
        let audit = std::fs::read_to_string(sidecar.paths.state.join("audit.log")).unwrap();
        assert_eq!(audit.matches(" journal-write ").count(), 1, "{audit}");
        task.abort();
    }

    struct PendingFull;
    impl FileSystem for PendingFull {
        fn replace(&self, _: &std::path::Path, _: &[u8]) -> io::Result<()> {
            Err(io::Error::from_raw_os_error(libc::ENOSPC))
        }
    }

    #[tokio::test]
    async fn serving_a_pending_file_that_cannot_write_is_audited_as_its_own_kind() {
        let sidecar = FakeSidecar::start(vec![Reply::Events(
            vec![text(1), asked(2, "p"), asked(3, "q")],
            End::Hold,
        )])
        .await;
        let session = observed_with(&sidecar, Arc::new(Disk), Arc::new(PendingFull));
        let task = follow(&sidecar, &session, running());
        let log = sidecar.paths.state.join("audit.log");
        until(|| head(&session) >= 3 && log.exists()).await;
        let audit = std::fs::read_to_string(&log).unwrap();
        assert_eq!(audit.matches(" pending-write ").count(), 1, "{audit}");
        assert_eq!(audit.matches(" journal-write ").count(), 0, "{audit}");
        assert_eq!(pending_sequences(&session), vec![2, 3], "memory still follows");
        task.abort();
    }

    #[tokio::test]
    async fn serving_events_that_write_no_pending_file_do_not_end_a_run_of_pending_failures() {
        let sidecar = FakeSidecar::start(vec![Reply::Events(
            vec![text(1), asked(2, "p"), text(3), asked(4, "q"), text(5)],
            End::Hold,
        )])
        .await;
        let session = observed_with(&sidecar, Arc::new(Disk), Arc::new(PendingFull));
        let task = follow(&sidecar, &session, running());
        // Each event's audit is written before the next event is applied, so at head 5 the fourth one's is done.
        until(|| head(&session) >= 5).await;
        let audit = std::fs::read_to_string(sidecar.paths.state.join("audit.log")).unwrap();
        assert_eq!(audit.matches(" pending-write ").count(), 1, "{audit}");
        assert_eq!(pending_sequences(&session), vec![2, 4]);
        task.abort();
    }

    #[tokio::test]
    async fn serving_a_live_event_below_pendings_watermark_is_not_applied_again() {
        let sidecar = FakeSidecar::start(vec![Reply::Events(
            vec![asked(3, "stale"), asked(6, "fresh")],
            End::Hold,
        )])
        .await;
        let session = observed(&sidecar, Arc::new(Disk));
        {
            // Pending is ahead of the journal: a turn ended at sequence 5 and settled every request before it.
            let mut state = session.lock().unwrap();
            state.pending.clear(5).unwrap();
        }
        let task = follow(&sidecar, &session, running());
        until(|| head(&session) >= 6).await;
        assert_eq!(pending_sequences(&session), vec![6]);
        task.abort();
    }

    #[tokio::test]
    async fn serving_a_refused_cursor_reconnects_at_once_whatever_the_back_off_has_grown_to() {
        let held = || vec![text(6), text(7)];
        let mut script: Vec<Reply> = (0..4)
            .map(|_| Reply::Refuse(Status::unavailable("restarting")))
            .collect();
        script.push(Reply::Ring(held()));
        script.push(Reply::Events(held(), End::Hold));
        let sidecar = FakeSidecar::start(script).await;
        let session = observed(&sidecar, Arc::new(Disk));
        let task = follow(&sidecar, &session, running());
        // Request n (counting from 0) arrives after the back-off that followed request n - 1.
        let mut arrived = Vec::new();
        while arrived.len() < 6 {
            until(|| sidecar.requests().len() > arrived.len()).await;
            arrived.push(std::time::Instant::now());
        }
        // The refused request is the fifth; the back-off before it had reached 800 ms. Waiting the next back-off
        // (twice as long) before the sixth would take longer still; reconnecting at once takes almost no time.
        let backed_off = arrived[4] - arrived[3];
        let after_refusal = arrived[5] - arrived[4];
        assert!(
            after_refusal * 4 < backed_off,
            "{after_refusal:?} after the refusal, {backed_off:?} before"
        );
        assert_eq!(sidecar.requests()[5].start, ReplayStart::AvailableHistory as i32);
        task.abort();
    }

    #[tokio::test]
    async fn serving_an_invalid_session_ends_observation_without_claiming_the_agent_stopped() {
        let sidecar = FakeSidecar::start(vec![Reply::Refuse(Status::invalid_argument("invalid session"))]).await;
        let session = observed(&sidecar, Arc::new(Disk));
        let task = follow(&sidecar, &session, running());
        until(|| session.lock().unwrap().error.is_some()).await;
        task.await.unwrap();
        let error = session.lock().unwrap().error.clone().unwrap();
        assert_eq!(error.code(), Code::FailedPrecondition);
        let detail = ErrorDetail::decode(error.details()).unwrap();
        assert_eq!(detail.reason, "session_observation_ended");
        assert_ne!(detail.code, ErrorCode::SessionNotFound as i32);
    }

    #[tokio::test]
    async fn serving_a_sidecar_that_stops_ends_the_session_and_retries_back_off_while_it_runs() {
        let sidecar = FakeSidecar::start(
            (0..64)
                .map(|_| Reply::Refuse(Status::unavailable("restarting")))
                .collect(),
        )
        .await;
        let session = observed(&sidecar, Arc::new(Disk));
        let alive = Arc::new(AtomicBool::new(true));
        let live: Live = {
            let alive = alive.clone();
            Arc::new(move || alive.load(Ordering::SeqCst))
        };
        let started = std::time::Instant::now();
        let task = follow(&sidecar, &session, live);
        tokio::time::sleep(Duration::from_millis(1600)).await;
        let (attempts, elapsed) = (sidecar.requests().len(), started.elapsed());
        // One attempt per 100 ms would make at least twice this many.
        assert!(attempts >= 2, "{attempts} attempts");
        assert!(
            attempts as u128 * 200 < elapsed.as_millis(),
            "{attempts} attempts in {elapsed:?}"
        );
        assert!(session.lock().unwrap().error.is_none());
        alive.store(false, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .expect("the watch must end once its sidecar has stopped")
            .unwrap();
        let error = session.lock().unwrap().error.clone().expect("a terminal status");
        assert_eq!(error.code(), Code::NotFound);
        assert_eq!(
            ErrorDetail::decode(error.details()).unwrap().code,
            ErrorCode::SessionNotFound as i32
        );
    }

    #[test]
    fn serving_watch_envelopes_preserve_an_explicit_zero_and_recovered_head() {
        use claude_runtime_protocol::v1::WatchSessionEventsRequest;
        for (head, available) in [(0, false), (42, false), (42, true)] {
            let start = if available { Start::Available } else { Start::AfterHead };
            let request = WatchSessionEventsRequest::decode(envelope("session", head, start)).unwrap();
            assert_eq!(request.session_id, "session");
            assert_eq!(
                request.start,
                if available {
                    ReplayStart::AvailableHistory
                } else {
                    ReplayStart::AfterSequence
                } as i32
            );
            assert_eq!(request.after_sequence, if available { None } else { Some(head) });
        }
    }

    #[test]
    fn serving_pending_retains_original_bytes_and_clears_only_on_observed_settlement() {
        let root = super::super::scratch("pending");
        let path = root.join("session.bin");
        let mut pending = Pending::open(&path).unwrap();
        let mut sequence = 0;
        let mut observe = |pending: &mut Pending, event| {
            sequence += 1;
            let event = SessionEvent {
                session_id: "session".into(),
                sequence,
                event: Some(event),
                ..Default::default()
            };
            let mut bytes = event.encode_to_vec();
            bytes.extend_from_slice(&[0xa0, 0x06, 7]);
            settle(pending, &event, &bytes).unwrap();
            bytes
        };
        let bytes = observe(
            &mut pending,
            Event::PermissionRequested(PermissionRequested {
                permission_id: "p".into(),
                ..Default::default()
            }),
        );
        assert_eq!(pending.list(), vec![(1, bytes.clone())]);
        drop(pending);
        let mut pending = Pending::open(&path).unwrap();
        assert_eq!(pending.list(), vec![(1, bytes)]);
        observe(
            &mut pending,
            Event::PermissionResolved(PermissionResolved {
                permission_id: "p".into(),
                ..Default::default()
            }),
        );
        assert!(pending.list().is_empty());
        observe(
            &mut pending,
            Event::PermissionRequested(PermissionRequested {
                permission_id: "turn".into(),
                ..Default::default()
            }),
        );
        observe(&mut pending, Event::TurnCompleted(TurnCompleted::default()));
        assert!(pending.list().is_empty());
        observe(
            &mut pending,
            Event::PermissionRequested(PermissionRequested {
                permission_id: "close".into(),
                ..Default::default()
            }),
        );
        observe(&mut pending, Event::SessionClosed(SessionClosed::default()));
        assert!(pending.list().is_empty());
        assert!(Pending::open(&path).unwrap().list().is_empty());
        drop(pending);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_session_is_current_only_while_its_watch_is_connected_and_caught_up() {
        use std::sync::atomic::Ordering::Acquire;
        let sidecar = FakeSidecar::start(vec![Reply::Events(vec![text(1), text(2)], End::Hold)]).await;
        let session = observed(&sidecar, Arc::new(Disk));
        assert!(
            !session.current.load(Acquire),
            "nothing is known before the watch connects"
        );
        let watch = follow(&sidecar, &session, running());
        until(|| head(&session) == 2 && session.current.load(Acquire)).await;
        watch.abort();
        let _ = watch.await;
        assert!(
            !session.current.load(Acquire),
            "a watch that ended leaves the session not current"
        );
    }
}
