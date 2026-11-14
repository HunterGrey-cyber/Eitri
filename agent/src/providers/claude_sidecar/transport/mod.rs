//! The connection and lifetime of one sidecar, separate from its session protocol.

pub(super) mod helper;
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

/// A remote tab's agent as another window (or a later one) reaches it again: the host as typed, the
/// host key its project was recorded with, its owning helper installation, the sidecar and session ids,
/// and the last folded sequence. An absent daemon means the owner is not recorded; the other identities are
/// required: a record missing one is refused rather than guessed at. A field this build does not know (a later
/// build's) is read past, so a saved tab a later build wrote is still this build's to take up and keep. The
/// session's declared editor tools travel with it; an absent declaration means no tools.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperHandoff {
    #[serde(default)]
    pub daemon: String,
    #[serde(default)]
    pub declared_client_tools: std::collections::BTreeSet<String>,
    pub host: String,
    pub host_key_sha256: String,
    pub sidecar_id: String,
    pub session_id: String,
    pub last_folded_sequence: u64,
}

impl HelperHandoff {
    /// The stop queue keeps the owning installation beside the sidecar id in one opaque record.
    pub fn stop_key(&self) -> String {
        if self.daemon.is_empty() {
            self.sidecar_id.clone()
        } else {
            format!("{}/{}", self.daemon, self.sidecar_id)
        }
    }
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
    fn remote_stop(&self) -> Option<Box<dyn FnOnce() -> io::Result<()> + Send>> {
        None
    }
    #[cfg(target_os = "linux")]
    fn freeze_for_move(&mut self) -> Result<TransportHandoff, MoveRefusedWhy>;
    /// Oldest reachable sequence, or unknown when the peer gives no ring floor.
    #[allow(dead_code)]
    fn replay_floor(&self) -> Option<u64>;

    fn build_description(&self) -> String;
    fn build_warnings(&self) -> Vec<String>;
    /// The sidecar's pid on this machine; `None` when it runs elsewhere.
    fn pid(&self) -> Option<u32>;
    /// What reaches a sidecar the server's helper holds again, for `session_id` folded through
    /// `last_folded_sequence`; `None` for a sidecar on this machine.
    fn helper_handoff(&self, session_id: String, last_folded_sequence: u64) -> Option<HelperHandoff> {
        let _ = (session_id, last_folded_sequence);
        None
    }
    /// The server helper's version as its Hello stated it; `None` for a sidecar on this machine.
    fn helper_version(&self) -> Option<String> {
        None
    }
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

pub(super) fn stop_remote(transport: &std::sync::Mutex<Box<dyn SidecarTransport>>) -> io::Result<()> {
    let stop = super::lock(transport).remote_stop();
    stop.ok_or_else(|| io::Error::other("no remote stop transport"))?()
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
            io::ErrorKind::InvalidInput,
            "a chat whose agent runs on a server brings no descriptors; it is taken up through its helper",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc, Mutex};
    struct HeldStop {
        started: mpsc::Sender<()>,
        release: Arc<Mutex<mpsc::Receiver<()>>>,
    }
    impl SidecarTransport for HeldStop {
        fn channel(&self, _: &RuntimeThread, _: Option<u32>) -> io::Result<Channel> {
            unreachable!()
        }
        fn call_metadata(&self) -> Vec<(&'static str, String)> {
            Vec::new()
        }
        fn stderr_tail(&self) -> Vec<String> {
            Vec::new()
        }
        fn liveness(&self) -> Liveness {
            Liveness::Unknown
        }
        fn stop(&mut self) -> io::Result<()> {
            self.remote_stop().unwrap()()
        }
        fn remote_stop(&self) -> Option<Box<dyn FnOnce() -> io::Result<()> + Send>> {
            let (started, release) = (self.started.clone(), self.release.clone());
            Some(Box::new(move || {
                started.send(()).unwrap();
                release
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(30))
                    .unwrap();
                Ok(())
            }))
        }
        #[cfg(target_os = "linux")]
        fn freeze_for_move(&mut self) -> Result<TransportHandoff, MoveRefusedWhy> {
            unreachable!()
        }
        fn replay_floor(&self) -> Option<u64> {
            None
        }
        fn build_description(&self) -> String {
            String::new()
        }
        fn build_warnings(&self) -> Vec<String> {
            Vec::new()
        }
        fn pid(&self) -> Option<u32> {
            None
        }
        #[cfg(feature = "test-support")]
        fn socket_path(&self) -> std::path::PathBuf {
            std::path::PathBuf::new()
        }
        #[cfg(target_os = "linux")]
        fn handed_output(&self) -> HandedOutput {
            unreachable!()
        }
        #[cfg(target_os = "linux")]
        fn restart_output(&mut self) -> io::Result<()> {
            Ok(())
        }
        #[cfg(target_os = "linux")]
        fn start_holding(&mut self) -> io::Result<()> {
            Ok(())
        }
        #[cfg(target_os = "linux")]
        fn let_go(&mut self) {}
    }
    #[test]
    fn remote_stop_leaves_the_transport_readable_while_its_rpc_is_held() {
        let (started, entered) = mpsc::channel();
        let (release, held) = mpsc::channel();
        let transport: Arc<Mutex<Box<dyn SidecarTransport>>> = Arc::new(Mutex::new(Box::new(HeldStop {
            started,
            release: Arc::new(Mutex::new(held)),
        })));
        let worker = {
            let transport = transport.clone();
            std::thread::spawn(move || stop_remote(&transport))
        };
        entered.recv_timeout(std::time::Duration::from_secs(30)).unwrap();
        let readable = transport.try_lock().is_ok();
        release.send(()).unwrap();
        worker.join().unwrap().unwrap();
        assert!(readable, "a panel read must not wait on StopSidecar");
    }
}
