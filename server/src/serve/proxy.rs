use std::convert::Infallible;
use std::future::Future;
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
use crate::run_dir::Paths;

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
            let response = tokio::spawn(route(serving, request)).await;
            Ok(match response {
                Ok(Ok(response)) => response,
                Ok(Err(status)) => status.into_http(),
                Err(error) => Status::internal(error.to_string()).into_http(),
            })
        })
    }
}

async fn route(serving: Arc<Serving>, request: http::Request<Body>) -> Result<http::Response<Body>, Status> {
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
    let response = forward(connect(&serving.paths, &info), request).await?;
    if !create {
        return Ok(response.map(Body::new));
    }
    let (parts, mut body) = response.into_parts();
    let mut frames = Vec::new();
    let mut bytes = Vec::new();
    let mut success = parts.headers.get("grpc-status").is_none_or(|status| status == "0");
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(unavailable)?;
        if let Some(data) = frame.data_ref() {
            if success && bytes.len().saturating_add(data.len()) <= 4 * 1024 * 1024 + 5 {
                bytes.extend_from_slice(data);
            } else {
                success = false;
                bytes.clear();
            }
        }
        if let Some(trailers) = frame.trailers_ref() {
            success &= trailers.get("grpc-status").is_some_and(|status| status == "0");
        }
        frames.push(Ok::<_, Status>(frame));
    }
    if success && bytes.len() >= 5 && bytes[0] == 0 {
        let length = u32::from_be_bytes(bytes[1..5].try_into().expect("frame length")) as usize;
        if bytes.len() == length + 5 {
            if let Ok(created) = CreateSessionResponse::decode(&bytes[5..]) {
                // A failure is audited by `observe` and answers this session's watches with a retryable refusal
                // until a maintenance pass observes it; the sidecar's response goes back unchanged either way.
                let _ = serving.observe(&sidecar, &created.session_id).await;
            }
        }
    }
    Ok(http::Response::from_parts(
        parts,
        Body::new(StreamBody::new(futures_util::stream::iter(frames))),
    ))
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

pub(super) async fn connect(
    paths: &Paths,
    info: &eitri_server_proto::SidecarInfo,
) -> Result<tokio::net::UnixStream, Status> {
    let socket =
        agent::socket_path::in_dir(&paths.run, &format!("sc-{}.sock", info.sidecar_id)).map_err(unavailable)?;
    let stream = tokio::net::UnixStream::connect(socket).await.map_err(unavailable)?;
    crate::peer::check(&stream).map_err(unavailable)?;
    if stream.peer_cred().map_err(unavailable)?.pid() != Some(info.pid as i32) {
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
}
