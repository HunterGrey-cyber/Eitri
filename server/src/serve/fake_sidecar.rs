//! A scripted sidecar answering WatchSessionEvents on its own socket, for tests of the helper's watch.
use std::collections::VecDeque;
use std::convert::Infallible;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use claude_runtime_protocol::v1::{ErrorCode, ReplayStart, SessionEvent, WatchSessionEventsRequest};
use futures_util::stream::{BoxStream, StreamExt};
use prost::Message;
use tonic::body::Body;
use tonic::server::NamedService;
use tonic::{Code, Status};
use tower_service::Service;

use super::{proxy, replay};
use crate::run_dir::Paths;

/// How one watch call is answered.
pub(super) enum Reply {
    /// These events, then the stream ends as `End` says.
    Events(Vec<SessionEvent>, End),
    /// What a sidecar holding only these events sends for the request's cursor: the events after it, or an
    /// EVENT_GAP refusal when the event after the cursor is no longer held. The stream then stays open.
    Ring(Vec<SessionEvent>),
    Refuse(Status),
}

pub(super) enum End {
    Clean,
    Fail(Status),
    Hold,
}

pub(super) struct FakeSidecar {
    root: PathBuf,
    pub paths: Paths,
    pub info: eitri_server_proto::SidecarInfo,
    pub requests: Arc<Mutex<Vec<WatchSessionEventsRequest>>>,
    server: tokio::task::JoinHandle<()>,
}

impl FakeSidecar {
    /// Answers watch calls in order from `script`; calls beyond it get a stream that stays open and silent.
    pub async fn start(script: Vec<Reply>) -> Self {
        // Short names keep the sidecar socket inside the Unix socket path limit.
        let root = super::scratch("sc");
        let paths = Paths::new("t", &root.join("d"), &root, &root).unwrap();
        paths.prepare().unwrap();
        let id = "0123456789abcdef";
        let socket = agent::socket_path::in_dir(&paths.run, &format!("sc-{id}.sock")).unwrap();
        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let service = Script {
            replies: Arc::new(Mutex::new(script.into())),
            requests: requests.clone(),
        };
        let incoming = futures_util::stream::unfold(listener, |listener| async move {
            let accepted = listener.accept().await.map(|(stream, _)| stream);
            Some((accepted, listener))
        });
        let server = tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(service)
                .serve_with_incoming(incoming)
                .await;
        });
        Self {
            root,
            paths,
            info: eitri_server_proto::SidecarInfo {
                sidecar_id: id.into(),
                pid: std::process::id(),
                alive: true,
                handshake: true,
                ..Default::default()
            },
            requests,
            server,
        }
    }

    pub fn requests(&self) -> Vec<WatchSessionEventsRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for FakeSidecar {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[derive(Clone)]
struct Script {
    replies: Arc<Mutex<VecDeque<Reply>>>,
    requests: Arc<Mutex<Vec<WatchSessionEventsRequest>>>,
}

impl NamedService for Script {
    const NAME: &'static str = proxy::SERVICE;
}

impl Service<http::Request<Body>> for Script {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        let script = self.clone();
        Box::pin(async move {
            let handler = tower::service_fn(move |request: tonic::Request<Bytes>| {
                let script = script.clone();
                async move {
                    let request = WatchSessionEventsRequest::decode(request.into_inner())
                        .map_err(|error| Status::internal(error.to_string()))?;
                    script.requests.lock().unwrap().push(request.clone());
                    let reply = script.replies.lock().unwrap().pop_front();
                    let encode = |events: Vec<SessionEvent>| {
                        futures_util::stream::iter(
                            events
                                .into_iter()
                                .map(|event| Ok::<_, Status>(Bytes::from(event.encode_to_vec()))),
                        )
                    };
                    let stream: BoxStream<'static, Result<Bytes, Status>> = match reply {
                        None => futures_util::stream::pending().boxed(),
                        Some(Reply::Refuse(status)) => return Err(status),
                        Some(Reply::Events(events, End::Clean)) => encode(events).boxed(),
                        Some(Reply::Events(events, End::Fail(status))) => encode(events)
                            .chain(futures_util::stream::once(async move { Err(status) }))
                            .boxed(),
                        Some(Reply::Events(events, End::Hold)) => {
                            encode(events).chain(futures_util::stream::pending()).boxed()
                        }
                        Some(Reply::Ring(events)) => {
                            let after = request.after_sequence.unwrap_or(0);
                            let oldest = events.first().map_or(1, |event| event.sequence);
                            if request.start == ReplayStart::AfterSequence as i32 && after + 1 < oldest {
                                return Err(replay::refusal(Code::OutOfRange, ErrorCode::EventGap, "evicted", ""));
                            }
                            let events = events.into_iter().filter(|event| event.sequence > after).collect();
                            encode(events).chain(futures_util::stream::pending()).boxed()
                        }
                    };
                    Ok(tonic::Response::new(stream))
                }
            });
            Ok(tonic::server::Grpc::new(proxy::RawCodec)
                .server_streaming(handler, request)
                .await)
        })
    }
}
