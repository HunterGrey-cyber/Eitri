//! The daemon's `Gate` service: the path facts of a session's gate requests, and a fresh one on request. It reads what
//! the session's watch holds and computes facts on this server's disk; it answers no request, forwards no answer and
//! builds no policy message.

use std::pin::Pin;
use std::sync::Arc;

use claude_runtime_protocol::v1::{session_event::Event, SessionEvent};
use eitri_server_proto::gate_fact_event::Event as FactEvent;
use eitri_server_proto::gate_server::Gate;
use eitri_server_proto::*;
use prost::Message;
use tonic::{Request, Response, Status};

use super::{blocking, Serving, SharedSession};
use crate::daemon::gate::{self as facts, Asked, HeldFacts};
use crate::daemon::mark_activity;

/// The `Gate` service over the daemon's sessions.
pub struct GateFacts(pub Arc<Serving>);

type FactStream = Pin<Box<dyn futures_util::Stream<Item = Result<GateFactEvent, Status>> + Send>>;

/// Where one `WatchFacts` stream is: the facts held when it began, by sequence, still to send, then a cursor over the
/// facts done after them.
struct Watching {
    facts: Arc<HeldFacts>,
    changed: tokio::sync::watch::Receiver<u64>,
    started: bool,
    backlog: std::collections::VecDeque<u64>,
    cursor: u64,
}

impl Watching {
    /// The next fact to send, or `None` when the stream ends: the session closed, or is no longer followed.
    async fn next(&mut self) -> Option<GateRequestFact> {
        loop {
            // Marked seen before the facts are read, so a change after the read wakes the wait below.
            self.changed.borrow_and_update();
            while let Some(generation) = self.backlog.pop_front() {
                // A fact answered or replaced since the stream began is skipped.
                if let Some(fact) = self.facts.done_at(generation) {
                    return Some(fact);
                }
            }
            if let Some((generation, fact)) = self.facts.next_after(self.cursor) {
                self.cursor = generation;
                return Some(fact);
            }
            if self.facts.is_closed() || self.changed.changed().await.is_err() {
                return None;
            }
        }
    }
}

fn request_event(fact: GateRequestFact) -> GateFactEvent {
    GateFactEvent {
        event: Some(FactEvent::Request(fact)),
    }
}

fn check_refused(refusal: &str) -> Response<CheckFactResponse> {
    Response::new(CheckFactResponse {
        first: None,
        fresh: None,
        refusal: refusal.to_owned(),
    })
}

/// The sequence and bytes of the request pending under `id` in `session`, if one is.
async fn pending_request(session: &SharedSession, id: &str) -> Result<Option<(u64, Vec<u8>)>, Status> {
    let (shared, id) = (session.clone(), id.to_owned());
    blocking(move || {
        let state = shared.lock().expect("session");
        Ok(state
            .pending
            .held(&id)
            .map(|(sequence, bytes)| (sequence, bytes.to_vec())))
    })
    .await
}

#[tonic::async_trait]
impl Gate for GateFacts {
    type WatchFactsStream = FactStream;

    async fn watch_facts(&self, request: Request<WatchFactsRequest>) -> Result<Response<FactStream>, Status> {
        mark_activity(&request);
        let request = request.into_inner();
        self.0.sidecar(&request.sidecar_id)?;
        let session = self.0.session(&request.sidecar_id, &request.session_id).await?;
        let facts = session.facts.clone();
        let watching = Watching {
            changed: facts.subscribe(),
            facts,
            started: false,
            backlog: Default::default(),
            cursor: 0,
        };
        let stream = futures_util::stream::unfold(watching, |mut watching| async move {
            if !watching.started {
                watching.started = true;
                let root = match watching.facts.engine_and_scene() {
                    Some((engine, scene)) => engine.root(scene).await,
                    None => GateRootFact::default(),
                };
                // The facts held now go out by sequence after the root's; any done later follow the cursor.
                let (backlog, cursor) = watching.facts.backlog();
                (watching.backlog, watching.cursor) = (backlog.into(), cursor);
                let event = GateFactEvent {
                    event: Some(FactEvent::Root(root)),
                };
                return Some((Ok(event), watching));
            }
            let fact = watching.next().await?;
            Some((Ok(request_event(fact)), watching))
        });
        Ok(Response::new(Box::pin(stream)))
    }

    async fn check_fact(&self, request: Request<CheckFactRequest>) -> Result<Response<CheckFactResponse>, Status> {
        mark_activity(&request);
        let request = request.into_inner();
        self.0.sidecar(&request.sidecar_id)?;
        let session = self.0.session(&request.sidecar_id, &request.session_id).await?;
        let Some((sequence, bytes)) = pending_request(&session, &request.permission_id).await? else {
            return Ok(check_refused(facts::REFUSAL_NOT_PENDING));
        };
        // A request's bytes may be large: read off the runtime.
        let read = blocking(move || {
            Ok(match SessionEvent::decode(bytes.as_slice()).map(|event| event.event) {
                Ok(Some(Event::PermissionRequested(asked))) => {
                    Asked::of(sequence, &asked).ok_or(facts::REFUSAL_NOT_A_GATE_REQUEST)
                }
                _ => Err(facts::REFUSAL_UNREADABLE),
            })
        })
        .await?;
        let asked = match read {
            Ok(asked) => asked,
            Err(refusal) => return Ok(check_refused(refusal)),
        };
        let (engine, scene) = match session.facts.engine_and_scene() {
            Some((engine, Ok(scene))) => (engine, scene),
            Some((_, Err(refusal))) => return Ok(check_refused(refusal)),
            None => return Ok(check_refused(facts::REFUSAL_UNKNOWN_START)),
        };
        let same = {
            let (asked, session_id, root) = (asked.clone(), session.facts.session_id().to_owned(), scene.root.clone());
            blocking(move || Ok(asked.digest(&session_id, &root)[..] == request.request_digest[..])).await?
        };
        if !same {
            return Ok(check_refused(facts::REFUSAL_DIGEST));
        }
        // The first fact is computed within one timeout of the request; waiting twice that covers a computation that
        // began just before this call.
        let (first, fresh) = tokio::join!(
            session
                .facts
                .first(&asked.permission_id, sequence, engine.timeout() * 2),
            engine.fact(&asked, session.facts.session_id(), &scene),
        );
        // The request may have been answered, or its turn ended, while the facts were computed: they are about a
        // request that is no longer pending, and nothing may rest on them.
        let still = pending_request(&session, &asked.permission_id).await?;
        if still.map(|(at, _)| at) != Some(sequence) {
            return Ok(check_refused(facts::REFUSAL_NOT_PENDING));
        }
        let refusal = match (&first, &fresh) {
            (Err(refusal), _) => refusal.clone(),
            (_, Err(refusal)) => (*refusal).to_owned(),
            _ => String::new(),
        };
        Ok(Response::new(CheckFactResponse {
            first: first.ok(),
            fresh: fresh.ok(),
            refusal,
        }))
    }
}
