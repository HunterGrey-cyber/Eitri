//! A sidecar the server helper holds, reached over the tab's helper connection. The helper started it
//! detached and owns its life: dropping this transport (a window closing) ends nothing on the server.

use super::{HelperHandoff, Liveness, SidecarTransport};
use crate::remote_client::HelperClient;
use crate::runtime_thread::RuntimeThread;
use std::io;
use tonic::transport::Channel;

pub(crate) struct ThroughHelper {
    client: HelperClient,
    sidecar_id: String,
    host_key_sha256: String,
    /// The helper's version as its Hello stated it, for the build line.
    helper_version: String,
    declared_client_tools: std::collections::BTreeSet<String>,
    /// What the helper warned about at Hello (a login environment it could not capture).
    warnings: Vec<String>,
}

impl ThroughHelper {
    pub(crate) fn new(
        client: HelperClient,
        sidecar_id: String,
        host_key_sha256: String,
        hello: &eitri_server_proto::HelloResponse,
    ) -> Self {
        let warnings = hello
            .warnings
            .iter()
            .map(|warning| format!("the helper on {}: {warning}", client.host()))
            .collect();
        Self {
            client,
            sidecar_id,
            host_key_sha256,
            helper_version: hello.helper_version.clone(),
            declared_client_tools: Default::default(),
            warnings,
        }
    }
}

impl ThroughHelper {
    /// The same transport for a sidecar whose helper's Hello another window heard: its version and
    /// warnings as that window recorded them. Only a chat's move (`moving.rs`, Linux-only) makes one.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn known(
        client: HelperClient,
        sidecar_id: String,
        host_key_sha256: String,
        helper_version: String,
        warnings: Vec<String>,
    ) -> Self {
        Self {
            client,
            sidecar_id,
            host_key_sha256,
            helper_version,
            declared_client_tools: Default::default(),
            warnings,
        }
    }

    pub(crate) fn with_tools(mut self, tools: &std::collections::BTreeSet<String>) -> Self {
        self.declared_client_tools = tools.clone();
        self
    }

    /// What reaches this sidecar again, for `session_id` folded through `last_folded_sequence`.
    pub(crate) fn handoff(&self, session_id: String, last_folded_sequence: u64) -> HelperHandoff {
        HelperHandoff {
            daemon: self.client.daemon().to_owned(),
            declared_client_tools: self.declared_client_tools.clone(),
            host: self.client.host().to_string(),
            host_key_sha256: self.host_key_sha256.clone(),
            sidecar_id: self.sidecar_id.clone(),
            session_id,
            last_folded_sequence,
        }
    }
}

impl SidecarTransport for ThroughHelper {
    /// The helper connection itself: every call names this sidecar in its metadata.
    fn channel(&self, _: &RuntimeThread, _: Option<u32>) -> io::Result<Channel> {
        Ok(self.client.channel())
    }
    fn call_metadata(&self) -> Vec<(&'static str, String)> {
        vec![(eitri_server_proto::SIDECAR_METADATA, self.sidecar_id.clone())]
    }
    /// The sidecar's output goes to files on the server.
    fn stderr_tail(&self) -> Vec<String> {
        Vec::new()
    }
    /// The process is the helper's to watch; this side only sees the connection.
    fn liveness(&self) -> Liveness {
        Liveness::Unknown
    }
    /// Stops the sidecar on the server: an explicit act, never a side effect of dropping the tab. A helper that
    /// no longer knows the sidecar (it ended, or the server restarted) has nothing left to stop.
    fn remote_stop(&self) -> Option<Box<dyn FnOnce() -> io::Result<()> + Send>> {
        let client = self.client.clone();
        let id = self.sidecar_id.clone();
        Some(Box::new(move || client.stop_if_known(&id).map_err(io::Error::other)))
    }
    fn stop(&mut self) -> io::Result<()> {
        self.client.stop_if_known(&self.sidecar_id).map_err(io::Error::other)
    }
    #[cfg(target_os = "linux")]
    fn freeze_for_move(&mut self) -> Result<super::TransportHandoff, crate::providers::claude_sidecar::MoveRefusedWhy> {
        Ok(super::TransportHandoff::Helper(self.handoff(String::new(), 0)))
    }
    /// The journal's oldest sequence: the helper evicts only closed sessions' segments, so a live
    /// session's journal holds it from its first event and a watch can read from the start.
    fn replay_floor(&self) -> Option<u64> {
        Some(0)
    }
    fn build_description(&self) -> String {
        format!(
            "sidecar on {} through eitri-server {}",
            self.client.host(),
            self.helper_version
        )
    }
    fn build_warnings(&self) -> Vec<String> {
        self.warnings.clone()
    }
    /// No process on this machine.
    fn pid(&self) -> Option<u32> {
        None
    }
    fn helper_handoff(&self, session_id: String, last_folded_sequence: u64) -> Option<HelperHandoff> {
        Some(self.handoff(session_id, last_folded_sequence))
    }
    fn helper_version(&self) -> Option<String> {
        Some(self.helper_version.clone())
    }
    #[cfg(feature = "test-support")]
    fn socket_path(&self) -> std::path::PathBuf {
        std::path::PathBuf::new()
    }
    #[cfg(target_os = "linux")]
    fn handed_output(&self) -> crate::providers::claude_sidecar::spawn::HandedOutput {
        crate::providers::claude_sidecar::spawn::HandedOutput {
            stdout_partial: Vec::new(),
            stderr_partial: Vec::new(),
            stderr_tail: Vec::new(),
        }
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
