use std::convert::Infallible;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::{Buf, BufMut, Bytes};
use claude_runtime_protocol::v1::CreateSessionResponse;
use http_body_util::{BodyExt, StreamBody};
use prost::Message;
use tonic::body::Body;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::server::NamedService;
use tonic::Status;
use tower_service::Service;

use super::Serving;

pub(super) const SERVICE: &str = "verdandi.claude.runtime.v1.RuntimeService";

#[derive(Clone)]
pub struct Proxy(pub Arc<Serving>);

impl NamedService for Proxy {
    const NAME: &'static str = SERVICE;
}

impl Service<http::Request<Body>> for Proxy {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        let serving = self.0.clone();
        Box::pin(async move {
            if let Some(activity) = request.extensions().get::<Arc<std::sync::atomic::AtomicBool>>() {
                activity.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            let response = tokio::spawn(route(serving, request, connect)).await;
            Ok(match response {
                Ok(Ok(response)) => response,
                Ok(Err(status)) => status.into_http(),
                Err(error) => Status::internal(error.to_string()).into_http(),
            })
        })
    }
}

async fn route<S>(
    serving: Arc<Serving>,
    request: http::Request<Body>,
    connect: impl AsyncFnOnce(&Path, u32) -> Result<S, Status>,
) -> Result<http::Response<Body>, Status>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let sidecar = request
        .headers()
        .get(eitri_server_proto::SIDECAR_METADATA)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let method = request.uri().path().rsplit('/').next().unwrap_or_default();
    if method == "WatchSessionEvents" {
        // Served from the journal, which outlives the sidecar: a client still gets what was recorded.
        return Ok(super::replay::serve(serving, sidecar, request).await);
    }
    let info = serving.sidecar(&sidecar)?;
    let create = method == "CreateSession";
    let response = forward(connect(&info.socket, info.pid), request).await?;
    if !create {
        return Ok(response.map(Body::new));
    }
    let (parts, mut body) = response.into_parts();
    let mut frames = Vec::new();
    let mut bytes = Vec::new();
    let mut overflow = false;
    let mut status_ok = parts.headers.get("grpc-status").is_none_or(|status| status == "0");
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(unavailable)?;
        if let Some(data) = frame.data_ref() {
            if !overflow && bytes.len().saturating_add(data.len()) <= 4 * 1024 * 1024 + 5 {
                bytes.extend_from_slice(data);
            } else {
                overflow = true;
                bytes.clear();
            }
        }
        if let Some(trailers) = frame.trailers_ref() {
            status_ok &= trailers.get("grpc-status").is_some_and(|status| status == "0");
        }
        frames.push(Ok::<_, Status>(frame));
    }
    match created_session(status_ok, overflow, &bytes) {
        Created::Session(id) => {
            // A failure is audited by `observe` and answers this session's watches with a retryable refusal
            // until a maintenance pass observes it; the sidecar's response goes back unchanged either way.
            let _ = serving.observe(&sidecar, &id).await;
        }
        Created::Unread => serving.unlearned(&sidecar),
        Created::Nothing => {}
    }
    Ok(http::Response::from_parts(
        parts,
        Body::new(StreamBody::new(futures_util::stream::iter(frames))),
    ))
}

/// What a CreateSession answer says about a new session.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Created {
    Session(String),
    /// The call succeeded but its answer could not be read: a session exists that the daemon does not know.
    Unread,
    /// The call failed: no session.
    Nothing,
}

/// Reads a CreateSession answer: its gRPC status, whether its data overflowed what is kept, and the data (one
/// uncompressed gRPC frame).
pub(super) fn created_session(status_ok: bool, overflow: bool, bytes: &[u8]) -> Created {
    if !status_ok {
        return Created::Nothing;
    }
    if overflow || bytes.len() < 5 || bytes[0] != 0 {
        return Created::Unread;
    }
    let length = u32::from_be_bytes(bytes[1..5].try_into().expect("frame length")) as usize;
    match CreateSessionResponse::decode(&bytes[5..]) {
        Ok(created) if bytes.len() == length + 5 && !created.session_id.is_empty() => {
            Created::Session(created.session_id)
        }
        _ => Created::Unread,
    }
}

async fn forward<S>(
    socket: impl Future<Output = Result<S, Status>>,
    request: http::Request<Body>,
) -> Result<http::Response<hyper::body::Incoming>, Status>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut sender = within_connect_timeout(async {
        let stream = socket.await?;
        let (sender, connection) = hyper::client::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
            .timer(hyper_util::rt::TokioTimer::new())
            .keep_alive_interval(agent::remote_client::HELPER_KEEPALIVE_INTERVAL)
            .keep_alive_timeout(agent::remote_client::HELPER_KEEPALIVE_TIMEOUT)
            .keep_alive_while_idle(true)
            .handshake::<_, Body>(hyper_util::rt::TokioIo::new(stream))
            .await
            .map_err(unavailable)?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(sender)
    })
    .await?;
    // A healthy unary handler keeps the caller's deadline, even while it takes time to answer.
    sender.send_request(request).await.map_err(unavailable)
}

fn unavailable(error: impl std::fmt::Display) -> Status {
    Status::unavailable(error.to_string())
}

/// How long reaching a sidecar may take: its socket, peer check and HTTP/2 handshake.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Bounds connection setup to a sidecar, so a hung sidecar fails the call instead of holding it forever.
pub(super) async fn within_connect_timeout<T>(setup: impl Future<Output = Result<T, Status>>) -> Result<T, Status> {
    tokio::time::timeout(CONNECT_TIMEOUT, setup)
        .await
        .unwrap_or_else(|_| Err(Status::unavailable("the sidecar did not accept a connection in time")))
}

pub(super) async fn connect(socket: &Path, pid: u32) -> Result<tokio::net::UnixStream, Status> {
    let stream = tokio::net::UnixStream::connect(socket).await.map_err(unavailable)?;
    crate::peer::check(&stream).map_err(unavailable)?;
    if stream.peer_cred().map_err(unavailable)?.pid() != Some(pid as i32) {
        return Err(Status::unavailable("sidecar socket peer pid differs"));
    }
    Ok(stream)
}

/// A protobuf payload stays opaque, including fields this build does not know.
#[derive(Default)]
pub(super) struct RawCodec;
pub(super) struct RawEncoder;
pub(super) struct RawDecoder;
impl Codec for RawCodec {
    type Encode = Bytes;
    type Decode = Bytes;
    type Encoder = RawEncoder;
    type Decoder = RawDecoder;
    fn encoder(&mut self) -> RawEncoder {
        RawEncoder
    }
    fn decoder(&mut self) -> RawDecoder {
        RawDecoder
    }
}
impl Encoder for RawEncoder {
    type Item = Bytes;
    type Error = Status;
    fn encode(&mut self, item: Bytes, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        dst.put(item);
        Ok(())
    }
}
impl Decoder for RawDecoder {
    type Item = Bytes;
    type Error = Status;
    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Bytes>, Status> {
        Ok(Some(src.copy_to_bytes(src.remaining())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn proxy_reaches_an_adopted_sidecar_in_the_ordinary_run_directory() {
        use crate::daemon::sidecars::{self, Record, Sidecars};
        use crate::run_dir::Paths;

        let root = super::super::scratch("proxy");
        let state = root.join("s".repeat(48 - root.as_os_str().len() - 1));
        let paths = Paths::new("1", &root.join("d"), &state, &root).unwrap();
        paths.prepare().unwrap();
        let ordinary = paths.metadata_run();
        assert_ne!(paths.run, ordinary);
        let id = "0123456789abcdef";
        let socket = agent::socket_path::in_dir(&ordinary, &format!("sc-{id}.sock")).unwrap();
        let pid = std::process::id();
        let record = Record {
            pid,
            start_time: sidecars::start_time(pid).unwrap(),
            socket: socket.clone(),
            sidecar_id: id.into(),
            root: "/srv/project".into(),
            account: String::new(),
            command_id: "start-sidecar".into(),
            started_unix_ms: crate::run_dir::unix_ms(),
            ready: true,
            failure: None,
            exited_unix_ms: None,
        };
        agent::private_fs::write_private(
            &ordinary.join(format!("sc-{id}.pid")),
            &serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        sidecars
            .refresh_handshakes_with(async |path, peer_pid| {
                assert_eq!((path, peer_pid), (socket.as_path(), pid));
                Ok(())
            })
            .await;
        assert!(sidecars.list().sidecars[0].handshake);

        let (stream, peer) = tokio::io::duplex(64 * 1024);
        let fake = tokio::spawn(async move {
            let service = hyper::service::service_fn(|_| async {
                let response = http::Response::builder()
                    .header("grpc-status", "0")
                    .body(Body::empty())
                    .unwrap();
                Ok::<_, Infallible>(response)
            });
            let _ = hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection(hyper_util::rt::TokioIo::new(peer), service)
                .await;
        });
        let request = http::Request::builder()
            .method(http::Method::POST)
            .uri(format!("http://localhost/{SERVICE}/Handshake"))
            .header(eitri_server_proto::SIDECAR_METADATA, id)
            .body(Body::empty())
            .unwrap();
        let response = route(Serving::new(paths, sidecars), request, async |path, peer_pid| {
            if (path, peer_pid) != (socket.as_path(), pid) {
                return Err(Status::unavailable("wrong sidecar socket or pid"));
            }
            Ok(stream)
        })
        .await;
        fake.abort();
        std::fs::remove_dir_all(root).unwrap();

        assert_eq!(response.unwrap().headers()["grpc-status"], "0");
    }

    #[tokio::test]
    async fn a_healthy_unary_handler_can_take_longer_than_connection_setup() {
        let (stream, peer) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection(
                    hyper_util::rt::TokioIo::new(peer),
                    hyper::service::service_fn(|_| async {
                        tokio::time::sleep(CONNECT_TIMEOUT + std::time::Duration::from_secs(2)).await;
                        Ok::<_, Infallible>(http::Response::new(Body::empty()))
                    }),
                )
                .await
        });
        let response = forward(async { Ok(stream) }, http::Request::new(Body::empty()))
            .await
            .unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        server.abort();
    }

    #[tokio::test]
    async fn a_peer_pause_longer_than_three_seconds_does_not_cancel_a_healthy_call() {
        let (stream, peer) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let (seen, requested) = tokio::sync::oneshot::channel();
            let seen = std::sync::Mutex::new(Some(seen));
            let ready = Arc::new(tokio::sync::Notify::new());
            let response_ready = ready.clone();
            let builder = hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new());
            let connection = builder.serve_connection(
                hyper_util::rt::TokioIo::new(peer),
                hyper::service::service_fn(move |_| {
                    seen.lock().unwrap().take().unwrap().send(()).unwrap();
                    let ready = response_ready.clone();
                    async move {
                        ready.notified().await;
                        Ok::<_, Infallible>(http::Response::new(Body::empty()))
                    }
                }),
            );
            tokio::pin!(connection);
            tokio::select! {
                _ = &mut connection => panic!("peer ended before receiving the call"),
                _ = requested => {}
            }
            // Stop polling the connection as well as the handler, as when the peer's process pauses.
            tokio::time::sleep(std::time::Duration::from_secs(6)).await;
            ready.notify_one();
            connection.await
        });
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            forward(async { Ok(stream) }, http::Request::new(Body::empty())),
        )
        .await
        .expect("healthy peer must answer")
        .unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        server.abort();
    }

    #[tokio::test]
    async fn a_silent_http2_peer_hits_the_keepalive_deadline() {
        let (stream, _silent_peer) = tokio::io::duplex(64 * 1024);
        let request = http::Request::builder()
            .uri("http://localhost/Handshake")
            .body(Body::empty())
            .unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            forward(async { Ok(stream) }, request),
        )
        .await
        .expect("keepalive must end a silent peer");
        assert_eq!(result.unwrap_err().code(), tonic::Code::Unavailable);
    }

    #[test]
    fn a_created_session_the_daemon_cannot_read_is_recorded_as_unread() {
        let framed = |message: &[u8]| {
            let mut bytes = vec![0];
            bytes.extend_from_slice(&(message.len() as u32).to_be_bytes());
            bytes.extend_from_slice(message);
            bytes
        };
        let created = CreateSessionResponse {
            session_id: "s1".into(),
            ..Default::default()
        }
        .encode_to_vec();
        assert_eq!(
            created_session(true, false, &framed(&created)),
            Created::Session("s1".into())
        );
        assert_eq!(created_session(false, false, &framed(&created)), Created::Nothing);
        assert_eq!(created_session(true, true, &[]), Created::Unread);
        assert_eq!(created_session(true, false, &[]), Created::Unread);
        assert_eq!(created_session(true, false, &framed(b"\xff\xff")), Created::Unread);
        let mut compressed = framed(&created);
        compressed[0] = 1;
        assert_eq!(created_session(true, false, &compressed), Created::Unread);
        assert_eq!(
            created_session(true, false, &framed(&[])),
            Created::Unread,
            "an answer without an id"
        );
    }
}
