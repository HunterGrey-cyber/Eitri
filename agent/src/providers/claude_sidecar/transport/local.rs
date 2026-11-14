//! A sidecar child held by this process, including a child adopted from another window.

use super::{Liveness, SidecarTransport};
use crate::providers::claude_sidecar::{spawn::SpawnedSidecar, UNARY_RPC_TIMEOUT};
use crate::runtime_thread::RuntimeThread;
use hyper_util::rt::TokioIo;
use std::io;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

pub(crate) struct LocalChild {
    child: SpawnedSidecar,
}

impl LocalChild {
    pub(crate) fn new(child: SpawnedSidecar) -> Self {
        Self { child }
    }
    #[cfg(target_os = "linux")]
    pub(crate) fn adopt(
        handoff: &crate::providers::claude_sidecar::ClaudeProviderHandoff,
        fds: super::SidecarFds,
    ) -> io::Result<Self> {
        Ok(Self::new(SpawnedSidecar::adopted(
            handoff.socket_path.clone(),
            &handoff.child,
            fds,
            crate::providers::claude_sidecar::spawn::HandedOutput {
                stdout_partial: handoff.stdout_partial.clone(),
                stderr_partial: handoff.stderr_partial.clone(),
                stderr_tail: handoff.stderr_tail.clone(),
            },
            handoff.build_description.clone(),
            handoff.build_warnings.clone(),
        )?))
    }
}

impl SidecarTransport for LocalChild {
    fn channel(&self, runtime: &RuntimeThread, expected_pid: Option<u32>) -> io::Result<Channel> {
        let socket_path = self.child.socket_path.clone();
        runtime
            .block_on(
                async move {
                    Endpoint::try_from("http://[::]:50051")?
                        .connect_with_connector(service_fn(move |_: Uri| {
                            let socket_path = socket_path.clone();
                            async move {
                                let stream = UnixStream::connect(socket_path).await?;
                                let peer = stream.peer_cred()?;
                                crate::socket_path::require_own_peer(peer.uid())?;
                                if let Some(expected) = expected_pid {
                                    if peer.pid().map(|pid| pid as u32) != Some(expected) {
                                        return Err(io::Error::new(
                                            io::ErrorKind::PermissionDenied,
                                            "the sidecar socket is served by another process than the one handed over",
                                        ));
                                    }
                                }
                                Ok::<_, io::Error>(TokioIo::new(stream))
                            }
                        }))
                        .await
                },
                UNARY_RPC_TIMEOUT,
            )
            .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error.to_string()))?
            .map_err(|error| io::Error::new(io::ErrorKind::ConnectionRefused, error.to_string()))
    }

    fn call_metadata(&self) -> Vec<(&'static str, String)> {
        Vec::new()
    }
    fn stderr_tail(&self) -> Vec<String> {
        self.child.stderr_tail()
    }
    fn liveness(&self) -> Liveness {
        self.child.liveness()
    }
    fn stop(&mut self) -> io::Result<()> {
        self.child.stop()
    }
    fn replay_floor(&self) -> Option<u64> {
        // The local protocol states a capacity, but no oldest retained sequence.
        None
    }
    fn build_description(&self) -> String {
        self.child.build_description.clone()
    }
    fn build_warnings(&self) -> Vec<String> {
        self.child.build_warnings.clone()
    }
    fn pid(&self) -> Option<u32> {
        Some(self.child.pid())
    }
    #[cfg(feature = "test-support")]
    fn socket_path(&self) -> std::path::PathBuf {
        self.child.socket_path.clone()
    }

    #[cfg(target_os = "linux")]
    fn freeze_for_move(&mut self) -> Result<super::TransportHandoff, crate::providers::claude_sidecar::MoveRefusedWhy> {
        let taken = self.child.park_output().and_then(|()| self.child.duplicate_fds());
        match taken {
            Ok(fds) => Ok(super::TransportHandoff::Local {
                fds,
                socket_path: self.child.socket_path.clone(),
                pid: self.child.pid(),
            }),
            Err(error) => Err(crate::providers::claude_sidecar::MoveRefusedWhy::SidecarEnding(error)),
        }
    }
    #[cfg(target_os = "linux")]
    fn handed_output(&self) -> crate::providers::claude_sidecar::spawn::HandedOutput {
        self.child.handed_output()
    }
    #[cfg(target_os = "linux")]
    fn restart_output(&mut self) -> io::Result<()> {
        self.child.restart_output()
    }
    #[cfg(target_os = "linux")]
    fn start_holding(&mut self) -> io::Result<()> {
        self.child.start_holding()
    }
    #[cfg(target_os = "linux")]
    fn let_go(&mut self) {
        self.child.let_go();
    }
}

impl Drop for LocalChild {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
