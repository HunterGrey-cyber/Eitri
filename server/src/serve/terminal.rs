//! The daemon's `Terminal` service: opens, lists and closes terminals through `TerminalHosts`, and routes each
//! `Attach` stream to its terminal's host. The daemon reads an attach's first frame only to find the host; every frame
//! it forwards, both ways, is the bytes it received, so it builds and edits no frame of a live terminal.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use eitri_server_proto::terminal_client_frame::Frame as ClientFrame;
use eitri_server_proto::terminal_server::{Terminal, TerminalServer};
use eitri_server_proto::terminal_server_frame::Frame as ServerFrame;
use eitri_server_proto::*;
use prost::Message;
use tonic::body::Body;
use tonic::server::NamedService;
use tonic::{Request, Response, Status, Streaming};
use tower_service::Service;

use super::super::terminals::{self, AttachTarget, TerminalHosts};
use super::proxy::RawCodec;

const ATTACH_PATH: &str = "/eitri.server.v1.Terminal/Attach";

type FrameStream = Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, Status>> + Send + 'static>>;

/// The `Terminal` service as the daemon serves it: `Attach` as a raw stream, the rest through the generated server.
#[derive(Clone)]
pub struct TerminalRoutes {
    generated: TerminalServer<Unary>,
    hosts: Arc<TerminalHosts>,
}

impl TerminalRoutes {
    pub fn new(hosts: Arc<TerminalHosts>, daemon_instance: String) -> Self {
        Self {
            generated: TerminalServer::new(Unary {
                hosts: hosts.clone(),
                daemon_instance,
            }),
            hosts,
        }
    }
}

impl NamedService for TerminalRoutes {
    const NAME: &'static str = "eitri.server.v1.Terminal";
}

impl Service<http::Request<Body>> for TerminalRoutes {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        // Terminal calls are work: they keep the daemon from its idle exit, as agent calls do.
        if let Some(activity) = request.extensions().get::<Arc<std::sync::atomic::AtomicBool>>() {
            activity.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if request.uri().path() == ATTACH_PATH {
            let hosts = self.hosts.clone();
            Box::pin(async move {
                let mut grpc = tonic::server::Grpc::new(RawCodec);
                Ok(grpc.streaming(RawAttach(hosts), request).await)
            })
        } else {
            let mut generated = self.generated.clone();
            Box::pin(async move { generated.call(request).await })
        }
    }
}

/// `OpenTerminal`, `ListTerminals` and `CloseTerminal`. `Attach` never reaches this: the routes serve it raw.
pub struct Unary {
    hosts: Arc<TerminalHosts>,
    daemon_instance: String,
}

#[tonic::async_trait]
impl Terminal for Unary {
    async fn open_terminal(
        &self,
        request: Request<OpenTerminalRequest>,
    ) -> Result<Response<OpenTerminalResponse>, Status> {
        let hosts = self.hosts.clone();
        // A caller disconnecting must not cancel a start after its host has been created.
        let terminal_id = tokio::spawn(async move { hosts.start(request.into_inner()).await })
            .await
            .map_err(|error| Status::internal(error.to_string()))??;
        Ok(Response::new(OpenTerminalResponse {
            terminal_id,
            daemon_instance: self.daemon_instance.clone(),
        }))
    }

    type AttachStream = Pin<Box<dyn futures_util::Stream<Item = Result<TerminalServerFrame, Status>> + Send>>;

    async fn attach(&self, _: Request<Streaming<TerminalClientFrame>>) -> Result<Response<Self::AttachStream>, Status> {
        Err(Status::internal("attach is routed before decoding"))
    }

    async fn list_terminals(
        &self,
        _: Request<ListTerminalsRequest>,
    ) -> Result<Response<ListTerminalsResponse>, Status> {
        Ok(Response::new(self.hosts.list_terminals().await))
    }

    async fn close_terminal(
        &self,
        request: Request<CloseTerminalRequest>,
    ) -> Result<Response<CloseTerminalResponse>, Status> {
        let request = request.into_inner();
        let hosts = self.hosts.clone();
        // A caller disconnecting must not cancel a close half way.
        tokio::spawn(async move { hosts.close_by_command(&request.terminal_id, &request.command_id).await })
            .await
            .map_err(|error| Status::internal(error.to_string()))?
    }
}

/// An attach stream, its frames kept as the bytes they arrived as.
struct RawAttach(Arc<TerminalHosts>);

impl tonic::server::StreamingService<Bytes> for RawAttach {
    type Response = Bytes;
    type ResponseStream = FrameStream;
    type Future = Pin<Box<dyn Future<Output = Result<Response<FrameStream>, Status>> + Send>>;

    fn call(&mut self, request: Request<Streaming<Bytes>>) -> Self::Future {
        let hosts = self.0.clone();
        Box::pin(attach(hosts, request.into_inner()))
    }
}

/// Reads the first frame (it must be `AttachHello`, within five seconds) to find the host, then routes the stream to
/// it: the first frame and every later one go to the host as received, and the host's frames come back as sent. When
/// either side ends, both do, so the daemon never holds a host's stream (and so its shutdown) open.
async fn attach(hosts: Arc<TerminalHosts>, mut inbound: Streaming<Bytes>) -> Result<Response<FrameStream>, Status> {
    let first = tokio::time::timeout(terminals::host::HELLO_WITHIN, inbound.message())
        .await
        .map_err(|_| Status::deadline_exceeded("the first frame must be AttachHello, within 5 seconds"))??
        .ok_or_else(|| Status::invalid_argument("the stream ended before AttachHello"))?;
    let hello = match TerminalClientFrame::decode(first.clone()) {
        Ok(TerminalClientFrame {
            frame: Some(ClientFrame::Hello(hello)),
        }) => hello,
        _ => return Err(Status::invalid_argument("the first frame must be AttachHello")),
    };
    let channel = match hosts.attach_target(&hello.terminal_id).await? {
        AttachTarget::Live(channel) => channel,
        AttachTarget::Exited(exit) => {
            // The host has gone: its exit record is all there is to say.
            let exited = TerminalServerFrame {
                frame: Some(ServerFrame::Exited(TerminalExited {
                    code: exit.code,
                    signal: exit.signal,
                    left: exit.left,
                })),
            };
            let frames: FrameStream = Box::pin(futures_util::stream::iter([Ok(Bytes::from(exited.encode_to_vec()))]));
            return Ok(Response::new(frames));
        }
    };
    let (upward, frames) = tokio::sync::mpsc::channel::<Bytes>(16);
    upward
        .send(first)
        .await
        .map_err(|_| Status::internal("the host's side closed"))?;
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready()
        .await
        .map_err(|error| Status::unavailable(error.to_string()))?;
    let response = grpc
        .streaming(
            Request::new(tokio_stream::wrappers::ReceiverStream::new(frames)),
            http::uri::PathAndQuery::from_static(ATTACH_PATH),
            RawCodec,
        )
        .await?;
    let upstream = response.into_inner();
    // The client's frames go up until either side ends; dropping this task ends the host's request side.
    let forward = tokio::spawn(async move {
        while let Ok(Some(frame)) = inbound.message().await {
            if upward.send(frame).await.is_err() {
                break;
            }
        }
    });
    let state = Some((upstream, AbortOnDrop(forward)));
    let frames: FrameStream = Box::pin(futures_util::stream::unfold(state, |state| async move {
        let (mut upstream, mut forward) = state?;
        tokio::select! {
            message = upstream.message() => match message {
                Ok(Some(frame)) => Some((Ok(frame), Some((upstream, forward)))),
                // The host ended its side: so does the client's, and the connection to the host goes with `upstream`.
                Ok(None) => None,
                Err(status) => Some((Err(status), None)),
            },
            // The client ended its side (or the host's request side went): the attachment is over, and dropping
            // `upstream` ends the host's stream at once, even when frames still wait for a client that stopped reading.
            _ = &mut forward.0 => None,
        }
    }));
    Ok(Response::new(frames))
}

/// Ends the client-to-host pump when the stream back to the client ends (or is dropped by a client gone).
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
