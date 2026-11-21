//! The daemon's `Lsp` service: opens, lists and stops language servers through `lsp::Servers`, and carries each
//! `Attach` stream between a bridge and its server, and answers `ReadPath` from `lsp::read_path`.

use std::sync::Arc;

use eitri_server_proto::lsp_client_frame::Frame as ClientFrame;
use eitri_server_proto::lsp_server::{Lsp, LspServer};
use eitri_server_proto::*;
use tonic::{Request, Response, Status, Streaming};

use super::super::lsp::{read_path, Attachment, Servers};
use super::super::sync;
use crate::daemon::{mark_activity, streamed, Outbox, Streamed};

pub struct LspService {
    servers: Arc<Servers>,
}

/// The `Lsp` service as the daemon serves it.
pub fn lsp_server(servers: Arc<Servers>) -> LspServer<LspService> {
    LspServer::new(LspService { servers })
}

fn internal(error: impl std::fmt::Display) -> Status {
    Status::internal(error.to_string())
}

#[tonic::async_trait]
impl Lsp for LspService {
    async fn open_server(&self, request: Request<OpenServerRequest>) -> Result<Response<OpenServerResponse>, Status> {
        mark_activity(&request);
        let servers = self.servers.clone();
        // A caller going away must not cancel an open after its process has been started.
        let answer = tokio::spawn(async move { servers.open(request.into_inner()).await })
            .await
            .map_err(internal)??;
        Ok(Response::new(answer))
    }

    type AttachStream = Streamed<LspServerFrame>;
    async fn attach(
        &self,
        request: Request<Streaming<LspClientFrame>>,
    ) -> Result<Response<Self::AttachStream>, Status> {
        mark_activity(&request);
        let mut inbound = request.into_inner();
        let hello = match inbound.message().await? {
            Some(LspClientFrame {
                frame: Some(ClientFrame::Hello(hello)),
            }) => hello,
            _ => return Err(Status::invalid_argument("an attach starts with its hello")),
        };
        if hello.server_id.len() > 128 || hello.client_id.len() > 128 {
            return Err(Status::invalid_argument(
                "the attach hello's ids are longer than 128 bytes",
            ));
        }
        let Attachment { frames, sink } = self.servers.attach(&hello.server_id, &hello.client_id).await;
        if let Some(mut sink) = sink {
            tokio::spawn(async move {
                // The bridge's stream ending, failing, or sending a second hello ends the attach; dropping the sink
                // detaches it.
                while let Ok(Some(frame)) = inbound.message().await {
                    match frame.frame {
                        Some(ClientFrame::Chunk(chunk)) => sink.chunk(chunk).await,
                        _ => break,
                    }
                }
            });
        }
        Ok(Response::new(streamed(frames)))
    }

    async fn list_servers(
        &self,
        request: Request<ListServersRequest>,
    ) -> Result<Response<ListServersResponse>, Status> {
        mark_activity(&request);
        Ok(Response::new(self.servers.list()))
    }

    async fn stop_server(&self, request: Request<StopServerRequest>) -> Result<Response<StopServerResponse>, Status> {
        mark_activity(&request);
        let request = request.into_inner();
        super::super::sidecars::validate_command_id(&request.command_id)?;
        let servers = self.servers.clone();
        tokio::spawn(async move { servers.stop(&request.server_id).await })
            .await
            .map_err(internal)?;
        Ok(Response::new(StopServerResponse {}))
    }

    type ReadPathStream = Streamed<ReadPathChunk>;
    async fn read_path(&self, request: Request<ReadPathRequest>) -> Result<Response<Self::ReadPathStream>, Status> {
        mark_activity(&request);
        let request = request.into_inner();
        let (mut outbox, stream) = Outbox::new(sync::READ_DEADLINE);
        tokio::task::spawn_blocking(move || {
            let last = read_path::read_path(&request.path, request.limit, outbox.deadline.into_std(), &mut |chunk| {
                outbox.send(chunk)
            });
            let last = match last {
                Some(last) if !outbox.late => last,
                _ if outbox.late => ReadPathChunk {
                    refusal: format!("the read took longer than {} s", sync::READ_DEADLINE.as_secs()),
                    ..Default::default()
                },
                // Nobody reads any more.
                _ => return,
            };
            outbox.finish(last);
        });
        Ok(Response::new(stream))
    }
}
