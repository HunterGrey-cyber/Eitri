//! The connection and lifetime of one sidecar, separate from its session protocol.

pub(super) mod local;

use crate::runtime_thread::RuntimeThread;
use serde::{Deserialize, Serialize};
use std::io;
use tonic::transport::Channel;

#[cfg(target_os = "linux")]
use super::{
    moving::MoveRefusedWhy,
    spawn::{HandedOutput, SidecarFds},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    Exited { code: Option<i32> },
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperHandoff {
    pub host: String,
    pub host_key_sha256: String,
    pub sidecar_id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransportKind {
    #[default]
    Local,
    Helper(HelperHandoff),
}

#[cfg(target_os = "linux")]
pub(crate) enum TransportHandoff {
    Local {
        fds: SidecarFds,
        socket_path: std::path::PathBuf,
        pid: u32,
    },
    // Reserved for a transport that keeps the sidecar on a helper.
    #[allow(dead_code)]
    Helper(HelperHandoff),
}

pub(crate) trait SidecarTransport: Send {
    /// A connected HTTP/2 channel to this sidecar's RuntimeService.
    fn channel(&self, runtime: &RuntimeThread, expected_pid: Option<u32>) -> io::Result<Channel>;
    /// Metadata sent on every RuntimeService call, including handshake and rewatch.
    fn call_metadata(&self) -> Vec<(&'static str, String)>;
    fn stderr_tail(&self) -> Vec<String>;
    // Available to transports whose connection and process can fail independently.
    #[allow(dead_code)]
    fn liveness(&self) -> Liveness;
    fn stop(&mut self) -> io::Result<()>;
    #[cfg(target_os = "linux")]
    fn freeze_for_move(&mut self) -> Result<TransportHandoff, MoveRefusedWhy>;
    /// Oldest reachable sequence, or unknown when the peer gives no ring floor.
    #[allow(dead_code)]
    fn replay_floor(&self) -> Option<u64>;

    fn build_description(&self) -> String;
    fn build_warnings(&self) -> Vec<String>;
    fn pid(&self) -> u32;
    #[cfg(feature = "test-support")]
    fn socket_path(&self) -> std::path::PathBuf;
    // A move keeps the local output snapshots in its existing wire fields.
    #[cfg(target_os = "linux")]
    fn handed_output(&self) -> HandedOutput;
    #[cfg(target_os = "linux")]
    fn restart_output(&mut self) -> io::Result<()>;
    #[cfg(target_os = "linux")]
    fn start_holding(&mut self) -> io::Result<()>;
    #[cfg(target_os = "linux")]
    fn let_go(&mut self);
}

/// Validated once when connecting, then cloned with the generated client for every RPC.
#[derive(Clone)]
pub(super) struct CallMetadata(tonic::metadata::MetadataMap);

impl CallMetadata {
    pub(super) fn new(entries: Vec<(&'static str, String)>) -> io::Result<Self> {
        let mut metadata = tonic::metadata::MetadataMap::new();
        for (key, value) in entries {
            let key: tonic::metadata::MetadataKey<tonic::metadata::Ascii> = key
                .parse()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let value = value
                .parse()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            metadata.insert(key, value);
        }
        Ok(Self(metadata))
    }
}

impl tonic::service::Interceptor for CallMetadata {
    fn call(&mut self, mut request: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        for entry in self.0.iter() {
            if let tonic::metadata::KeyAndValueRef::Ascii(key, value) = entry {
                request.metadata_mut().insert(key.clone(), value.clone());
            }
        }
        Ok(request)
    }
}

pub(super) type RuntimeClient = claude_runtime_protocol::v1::runtime_service_client::RuntimeServiceClient<
    tonic::service::interceptor::InterceptedService<Channel, CallMetadata>,
>;

#[cfg(target_os = "linux")]
pub(super) fn adopt(handoff: &super::ClaudeProviderHandoff, fds: SidecarFds) -> io::Result<Box<dyn SidecarTransport>> {
    match &handoff.transport {
        TransportKind::Local => Ok(Box::new(local::LocalChild::adopt(handoff, fds)?)),
        TransportKind::Helper(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a helper handoff is not supported yet",
        )),
    }
}
