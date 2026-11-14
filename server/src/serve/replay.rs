use std::collections::VecDeque;
use std::sync::Arc;

use bytes::Bytes;
use claude_runtime_protocol::v1::{ErrorCode, ErrorDetail, ReplayStart, WatchSessionEventsRequest};
use eitri_server::journal::{ReadError, Record};
use prost::Message;
use tonic::{Code, Request, Response, Status};

use super::{blocking, proxy::RawCodec, Serving, SharedSession};

pub(super) const BATCH_BYTES: usize = 64 * 1024;

pub(super) fn refusal(code: Code, error: ErrorCode, message: &str, reason: &str) -> Status {
    Status::with_details(
        code,
        message,
        Bytes::from(
            ErrorDetail {
                code: error as i32,
                message: message.into(),
                reason: reason.into(),
            }
            .encode_to_vec(),
        ),
    )
}

pub(super) fn gap(message: &str) -> Status {
    refusal(Code::OutOfRange, ErrorCode::EventGap, message, "")
}

/// The refusal a laptop takes as its session being gone for good: the sidecar's own `SESSION_NOT_FOUND`.
pub(super) fn session_gone(message: &str) -> Status {
    refusal(Code::NotFound, ErrorCode::SessionNotFound, message, "")
}

pub(super) fn is_gap(status: &Status) -> bool {
    ErrorDetail::decode(status.details()).is_ok_and(|detail| detail.code == ErrorCode::EventGap as i32)
}

pub(super) async fn serve(
    serving: Arc<Serving>,
    sidecar: String,
    request: http::Request<tonic::body::Body>,
) -> http::Response<tonic::body::Body> {
    let service = tower::service_fn(move |request: Request<Bytes>| {
        let serving = serving.clone();
        let sidecar = sidecar.clone();
        async move {
            let request = WatchSessionEventsRequest::decode(request.into_inner())
                .map_err(|error| Status::invalid_argument(error.to_string()))?;
            let session = match serving.session(&sidecar, &request.session_id).await {
                Ok(session) => session,
                // A typed refusal is final; otherwise an unknown sidecar is named before an unknown session.
                Err(status) if !status.details().is_empty() => return Err(status),
                Err(status) => {
                    serving.sidecar(&sidecar)?;
                    return Err(status);
                }
            };
            open(session, request).await
        }
    });
    tonic::server::Grpc::new(RawCodec)
        .server_streaming(service, request)
        .await
}

struct Tail {
    session: SharedSession,
    changed: tokio::sync::watch::Receiver<u64>,
    cursor: u64,
    batch: VecDeque<Record>,
    finished: bool,
}

pub(super) async fn open(
    session: SharedSession,
    request: WatchSessionEventsRequest,
) -> Result<Response<impl futures_util::Stream<Item = Result<Bytes, Status>> + Send + 'static>, Status> {
    let mode = ReplayStart::try_from(request.start).map_err(|_| configuration("replay_start_unrecognized"))?;
    match mode {
        ReplayStart::Unspecified => return Err(configuration("replay_start_unrecognized")),
        ReplayStart::AfterSequence if request.after_sequence.is_none() => {
            return Err(configuration("replay_cursor_missing"))
        }
        ReplayStart::FromNow | ReplayStart::AvailableHistory if request.after_sequence.is_some() => {
            return Err(configuration("replay_cursor_unexpected"))
        }
        _ => {}
    }
    let shared = session.clone();
    let (head, cursor, batch, error, changed) = blocking(move || {
        let state = shared.lock().expect("session");
        let head = state.journal.head();
        let mut cursor = if mode == ReplayStart::FromNow {
            head
        } else {
            request.after_sequence.unwrap_or(0)
        };
        let batch = if mode == ReplayStart::AvailableHistory {
            loop {
                match state.journal.read_after(cursor, BATCH_BYTES) {
                    Err(ReadError::Gap { last, .. }) => cursor = last,
                    batch => break batch,
                }
            }
        } else if mode == ReplayStart::AfterSequence && cursor > 0 && cursor < head {
            // A cursor naming an unobserved event cannot certify a contiguous history.
            match state.journal.read_after(cursor - 1, 1) {
                Err(error) => Err(error),
                Ok(_) => state.journal.read_after(cursor, BATCH_BYTES),
            }
        } else {
            state.journal.read_after(cursor, BATCH_BYTES)
        };
        Ok((head, cursor, batch, state.error.clone(), state.changed.subscribe()))
    })
    .await?;
    if batch.as_ref().is_ok_and(|batch| batch.is_empty()) {
        if let Some(error) = error.clone() {
            return Err(error);
        }
    }
    if cursor > head {
        return Err(refusal(
            Code::InvalidArgument,
            ErrorCode::InvalidConfiguration,
            "replay cursor is ahead of the session",
            "replay_cursor_in_future",
        ));
    }
    let batch = batch.map_err(read_error)?;
    // A session that cannot continue and has nothing left to send refuses at once, which a client treats as final.
    if let (true, Some(error)) = (batch.is_empty(), error) {
        return Err(error);
    }
    let tail = Tail {
        session,
        changed,
        cursor,
        batch: batch.into(),
        finished: false,
    };
    let stream = futures_util::stream::unfold(tail, |mut tail| async move {
        loop {
            if tail.finished {
                return None;
            }
            if let Some(Record::Event { sequence, bytes }) = tail.batch.pop_front() {
                tail.cursor = sequence;
                return Some((Ok(Bytes::from(bytes)), tail));
            }
            // Register before reading, so an append between the read and wait cannot be missed.
            tail.changed.borrow_and_update();
            let session = tail.session.clone();
            let cursor = tail.cursor;
            let next = blocking(move || {
                let state = session.lock().expect("session");
                Ok((
                    state.journal.read_after(cursor, BATCH_BYTES),
                    state.closed,
                    state.error.clone(),
                ))
            })
            .await;
            match next {
                Ok((Ok(records), closed, error)) => {
                    tail.batch = records.into();
                    if !tail.batch.is_empty() {
                        continue;
                    }
                    if let Some(error) = error {
                        tail.finished = true;
                        return Some((Err(error), tail));
                    }
                    if closed {
                        return None;
                    }
                }
                Ok((Err(error), _, _)) => {
                    tail.finished = true;
                    return Some((Err(read_error(error)), tail));
                }
                Err(error) => {
                    tail.finished = true;
                    return Some((Err(error), tail));
                }
            }
            if tail.changed.changed().await.is_err() {
                return None;
            }
        }
    });
    let mut response = Response::new(stream);
    response.metadata_mut().insert(
        eitri_server_proto::JOURNAL_HEAD_METADATA,
        head.to_string().parse().expect("decimal journal head"),
    );
    Ok(response)
}

fn configuration(reason: &str) -> Status {
    refusal(
        Code::InvalidArgument,
        ErrorCode::InvalidConfiguration,
        "invalid replay start",
        reason,
    )
}

fn read_error(error: ReadError) -> Status {
    match error {
        ReadError::Gap { first, last, why } => gap(&format!("journal gap {first}..={last}: {why:?}")),
        ReadError::Io(error) => Status::internal(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::SessionLock;
    use super::*;
    use claude_runtime_protocol::v1::{session_event::Event, SessionEvent, TextDelta};
    use eitri_server::journal::{GapWhy, Journal, JournalConfig, Pending};
    use futures_util::StreamExt;
    use std::path::PathBuf;

    struct Fixture(PathBuf, Option<SharedSession>);
    impl Fixture {
        fn new() -> Self {
            let root = super::super::scratch("replay");
            let journal = Journal::open(&root.join("journal/session"), &JournalConfig::default()).unwrap();
            let pending = Pending::open(&root.join("pending/session.bin")).unwrap();
            Self(
                root,
                Some(Arc::new(SessionLock::new(super::super::Session::new(
                    ("sidecar".into(), "session".into()),
                    journal,
                    pending,
                    false,
                )))),
            )
        }
        fn session(&self) -> &SharedSession {
            self.1.as_ref().unwrap()
        }
        fn append(&self, sequence: u64) -> Vec<u8> {
            let mut bytes = SessionEvent {
                session_id: "session".into(),
                sequence,
                event: Some(Event::TextDelta(TextDelta {
                    text: "x".repeat(1024),
                    ..Default::default()
                })),
                ..Default::default()
            }
            .encode_to_vec();
            // A field unknown to the generated type proves that replay does not re-encode events.
            bytes.extend_from_slice(&[0xa0, 0x06, 0x07]);
            let mut state = self.session().lock().unwrap();
            state.journal.append(sequence, &bytes).unwrap();
            state.changed.send_replace(sequence);
            bytes
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            drop(self.1.take());
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn request(cursor: u64) -> WatchSessionEventsRequest {
        WatchSessionEventsRequest {
            session_id: "session".into(),
            start: ReplayStart::AfterSequence as i32,
            after_sequence: Some(cursor),
        }
    }

    #[tokio::test]
    async fn an_expired_empty_journal_returns_its_terminal_status_before_a_future_cursor() {
        let f = Fixture::new();
        f.append(1);
        {
            let mut state = f.session().lock().unwrap();
            state.journal.finish_sync();
            state.journal.forget_history();
            state.error = Some(session_gone("the retained history has expired"));
        }
        let error = open(f.session().clone(), request(2)).await.err().unwrap();
        assert_eq!(error.code(), Code::NotFound);
        assert_eq!(
            ErrorDetail::decode(error.details()).unwrap().code,
            ErrorCode::SessionNotFound as i32
        );
    }

    #[tokio::test]
    async fn serving_replay_preserves_bytes_and_hands_over_after_multiple_batches() {
        let f = Fixture::new();
        let mut expected = (1..=150).map(|sequence| f.append(sequence)).collect::<Vec<_>>();
        let response = open(f.session().clone(), request(0)).await.unwrap();
        assert_eq!(
            response
                .metadata()
                .get(eitri_server_proto::JOURNAL_HEAD_METADATA)
                .unwrap(),
            "150"
        );
        let stream = response.into_inner();
        tokio::pin!(stream);
        for sequence in 151..=175 {
            expected.push(f.append(sequence));
        }
        for bytes in expected {
            assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), bytes);
        }
        {
            let mut session = f.session().lock().unwrap();
            session.closed = true;
            session.changed.send_modify(|head| *head += 1);
        }
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn serving_replay_rejects_future_invalid_and_gap_cursors_with_details() {
        let f = Fixture::new();
        f.append(1);
        f.append(5);
        for cursor in [1, 2, 3, 4] {
            let error = match open(f.session().clone(), request(cursor)).await {
                Err(error) => error,
                Ok(_) => panic!("gap accepted"),
            };
            assert_eq!(error.code(), Code::OutOfRange);
            assert!(is_gap(&error));
            assert_eq!(ErrorDetail::decode(error.details()).unwrap().reason, "");
        }
        let error = match open(f.session().clone(), request(6)).await {
            Err(error) => error,
            Ok(_) => panic!("future cursor accepted"),
        };
        assert_eq!(error.code(), Code::InvalidArgument);
        assert_eq!(
            ErrorDetail::decode(error.details()).unwrap().reason,
            "replay_cursor_in_future"
        );
        f.session()
            .lock()
            .unwrap()
            .journal
            .record_gap(6, 8, GapWhy::NotObserved)
            .unwrap();
        assert!(open(f.session().clone(), request(8)).await.is_ok());
        assert!(open(
            f.session().clone(),
            WatchSessionEventsRequest {
                session_id: "session".into(),
                start: ReplayStart::FromNow as i32,
                after_sequence: None,
            }
        )
        .await
        .is_ok());
        for (start, cursor, reason) in [
            (ReplayStart::Unspecified, None, "replay_start_unrecognized"),
            (ReplayStart::AfterSequence, None, "replay_cursor_missing"),
            (ReplayStart::FromNow, Some(0), "replay_cursor_unexpected"),
            (ReplayStart::AvailableHistory, Some(0), "replay_cursor_unexpected"),
        ] {
            let mut req = request(0);
            req.start = start as i32;
            req.after_sequence = cursor;
            let error = match open(f.session().clone(), req).await {
                Err(error) => error,
                Ok(_) => panic!("invalid start accepted"),
            };
            assert_eq!(error.code(), Code::InvalidArgument);
            assert_eq!(ErrorDetail::decode(error.details()).unwrap().reason, reason);
        }
    }

    #[tokio::test]
    async fn serving_replay_sends_prefix_before_refusing_a_later_gap() {
        let f = Fixture::new();
        let bytes = f.append(1);
        f.session()
            .lock()
            .unwrap()
            .journal
            .record_gap(2, 8, GapWhy::NotObserved)
            .unwrap();
        let stream = open(f.session().clone(), request(0)).await.unwrap().into_inner();
        tokio::pin!(stream);
        assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), bytes);
        assert!(is_gap(&stream.next().await.unwrap().unwrap_err()));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn serving_replay_of_a_session_that_cannot_continue_sends_the_rest_then_its_refusal() {
        let f = Fixture::new();
        let expected = (1..=3).map(|sequence| f.append(sequence)).collect::<Vec<_>>();
        let stream = open(f.session().clone(), request(1)).await.unwrap().into_inner();
        tokio::pin!(stream);
        assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), expected[1]);
        {
            let mut session = f.session().lock().unwrap();
            session.error = Some(session_gone("the sidecar has stopped"));
            session.changed.send_modify(|_| {});
        }
        assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), expected[2]);
        let gone = |error: Status| {
            assert_eq!(error.code(), Code::NotFound);
            let detail = ErrorDetail::decode(error.details()).unwrap();
            assert_eq!(detail.code, ErrorCode::SessionNotFound as i32);
            assert_eq!(detail.reason, "");
        };
        gone(stream.next().await.unwrap().unwrap_err());
        assert!(stream.next().await.is_none());
        // Nothing left to send: the refusal comes at once, as a client's open failure.
        match open(f.session().clone(), request(3)).await {
            Err(error) => gone(error),
            Ok(_) => panic!("a session that cannot continue was opened at its head"),
        }
    }

    #[tokio::test]
    async fn serving_replay_at_head_waits_for_live_and_available_history_skips_eviction() {
        let f = Fixture::new();
        f.session()
            .lock()
            .unwrap()
            .journal
            .record_gap(1, 10, GapWhy::Evicted)
            .unwrap();
        let expected = f.append(11);
        let req = WatchSessionEventsRequest {
            session_id: "session".into(),
            start: ReplayStart::AvailableHistory as i32,
            after_sequence: None,
        };
        let stream = open(f.session().clone(), req).await.unwrap().into_inner();
        tokio::pin!(stream);
        assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), expected);
        let stream = open(f.session().clone(), request(11)).await.unwrap().into_inner();
        tokio::pin!(stream);
        assert!(futures_util::poll!(stream.next()).is_pending());
        let expected = f.append(12);
        assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), expected);
    }
}
