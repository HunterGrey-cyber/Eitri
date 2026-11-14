//! A chat's provider as one thing that moves to another process, whatever its agent.
//!
//! Each provider moves its own way (`ClaudeSidecarProvider::freeze_for_move`, `CodexProvider::freeze_for_move`,
//! `AcpProvider::freeze_for_move`, each with its `adopt`, `commit` and way back), and a conversation holds its
//! provider only as an [`AgentProvider`]. [`ChatMover`] is the one face the window's chat code moves a provider by:
//! [`AgentProvider::mover`] gives it for a provider that can move, its freeze returns the provider's record as data
//! and its descriptors, and [`adopt`] takes the same back up in the other window by the kind the record names.
//!
//! The record crosses as the other window's input: [`adopt`] reads it with the providers' own typed records, and a
//! record that is not one of them, or names a kind this build does not move, is refused whole.

use crate::external::ExternalProvider;
use crate::jsonrpc_link::LinkHandoff;
use crate::provider::AgentProvider;
use crate::providers::acp::{AcpProvider, AcpProviderHandoff};
use crate::providers::claude_sidecar::{ClaudeProviderHandoff, ClaudeSidecarProvider, SidecarFds};
use crate::providers::codex::{CodexProvider, CodexProviderHandoff};
use serde_json::Value;
use std::io;
use std::os::fd::OwnedFd;
use std::sync::Arc;

/// Which provider a moved chat's record is for: the name the record carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovedKind {
    Claude,
    Codex,
    Acp,
}

impl MovedKind {
    pub fn wire(self) -> &'static str {
        match self {
            MovedKind::Claude => "claude",
            MovedKind::Codex => "codex",
            MovedKind::Acp => "acp",
        }
    }

    pub fn from_wire(name: &str) -> Option<MovedKind> {
        match name {
            "claude" => Some(MovedKind::Claude),
            "codex" => Some(MovedKind::Codex),
            "acp" => Some(MovedKind::Acp),
            _ => None,
        }
    }

    /// How many descriptors a provider of this kind running on this machine hands over: the agent's
    /// stdin, stdout and stderr.
    pub fn descriptors(self) -> usize {
        3
    }

    /// How many descriptors a provider of this kind hands over with `record`: [`Self::descriptors`], or
    /// none for a Claude chat whose agent runs on a server (its record names the helper's sidecar instead).
    pub fn descriptors_for(self, record: &Value) -> usize {
        match self {
            MovedKind::Claude if runs_on_a_server(record) => 0,
            _ => self.descriptors(),
        }
    }

    /// Whether `record`, a provider's record of this kind, names an agent running on a server.
    pub fn runs_on_a_server(self, record: &Value) -> bool {
        self == MovedKind::Claude && runs_on_a_server(record)
    }
}

/// A provider frozen for a move: its own record, and the descriptors that go with it. The source keeps these
/// until the move ends (they are what [`ChatMover::resume_after_chat_move`] takes back) and sends copies.
#[derive(Debug)]
pub struct FrozenProvider {
    pub kind: MovedKind,
    pub record: Value,
    pub fds: Vec<OwnedFd>,
}

impl FrozenProvider {
    /// Copies of the descriptors, close-on-exec, for the move: the originals stay for a move that does not happen.
    pub fn copies(&self) -> io::Result<Vec<OwnedFd>> {
        self.fds.iter().map(OwnedFd::try_clone).collect()
    }
}

/// A provider that can move with its chat.
pub trait ChatMover {
    /// Whether a chat with a running turn moves now, rather than once its turn ends.
    fn live_move(&self) -> bool;
    /// Stops the provider where it stands and gives its record and descriptors. Called once the conversation has
    /// stopped folding and its last pump ran. The provider is as it was when this is refused.
    fn freeze_for_chat_move(&self) -> Result<FrozenProvider, String>;
    /// The move did not happen: the provider reads on from where it stopped, nothing lost.
    fn resume_after_chat_move(&self, frozen: FrozenProvider) -> Result<(), String>;
    /// In the window that took the chat up, once the move is committed: the provider reads and answers from here.
    fn commit_chat_move(&self) -> io::Result<()>;
    /// In the window the chat left, once the other window holds it: nothing is ended, and this provider acts for
    /// the session no more.
    fn forget_chat_move(&self);
}

/// A provider taken up from a moved chat's record: what the conversation is driven with, and for Codex and ACP
/// its external side. Nothing is read from the agent or written to it until [`ChatMover::commit_chat_move`].
pub struct Adopted {
    pub agent: Arc<dyn AgentProvider + Send + Sync>,
    pub external: Option<Arc<dyn ExternalProvider + Send + Sync>>,
}

/// Whether a Claude chat's record names a sidecar the server's helper holds.
fn runs_on_a_server(record: &Value) -> bool {
    record
        .get("transport")
        .and_then(|transport| transport.get("Helper"))
        .is_some()
}

/// Takes up the provider a moved chat's record names, with the descriptors that came with it, in a window
/// holding no connection to a server: a remote chat is refused ([`adopt_with`]).
pub fn adopt(kind: MovedKind, record: &Value, fds: Vec<OwnedFd>) -> Result<Adopted, String> {
    adopt_with(kind, record, fds, &crate::remote_client::NoHelperConnections)
}

/// [`adopt`] in a window that may hold connections to servers. A remote chat brings no descriptors and is
/// taken up over the connection `helpers` holds for its host key; a window with none refuses it.
pub fn adopt_with(
    kind: MovedKind,
    record: &Value,
    fds: Vec<OwnedFd>,
    helpers: &dyn crate::remote_client::HelperConnections,
) -> Result<Adopted, String> {
    let unreadable = |what: &str, err: serde_json::Error| format!("the moved chat's {what} is not readable: {err}");
    if kind == MovedKind::Claude && runs_on_a_server(record) {
        if !fds.is_empty() {
            return Err(format!("a moved remote chat brings no descriptors, not {}", fds.len()));
        }
        let handoff: ClaudeProviderHandoff =
            serde_json::from_value(record.clone()).map_err(|e| unreadable("sidecar record", e))?;
        let crate::providers::claude_sidecar::TransportKind::Helper(helper) = &handoff.transport else {
            return Err("the moved chat's record names no server".to_string());
        };
        let client = helpers.open_for(&helper.host, &helper.host_key_sha256).ok_or_else(|| {
            format!(
                "this window has no connection to {}; open the remote project here first",
                helper.host
            )
        })?;
        let provider = ClaudeSidecarProvider::adopt_through_helper(handoff, client)
            .map_err(|e| format!("the moved chat's agent could not be taken over: {e}"))?;
        return Ok(Adopted {
            agent: Arc::new(provider),
            external: None,
        });
    }
    let fds: [OwnedFd; 3] = fds
        .try_into()
        .map_err(|fds: Vec<OwnedFd>| format!("a moved {} chat brings 3 descriptors, not {}", kind.wire(), fds.len()))?;
    match kind {
        MovedKind::Claude => {
            let handoff: ClaudeProviderHandoff =
                serde_json::from_value(record.clone()).map_err(|e| unreadable("sidecar record", e))?;
            let provider = ClaudeSidecarProvider::adopt(handoff, SidecarFds::from_array(fds))
                .map_err(|e| format!("the moved chat's sidecar could not be taken over: {e}"))?;
            Ok(Adopted {
                agent: Arc::new(provider),
                external: None,
            })
        }
        MovedKind::Codex => {
            let (handoff, link) =
                external_parts::<CodexProviderHandoff>(record).map_err(|e| unreadable("record", e))?;
            let provider = Arc::new(
                CodexProvider::adopt(handoff, link, fds)
                    .map_err(|e| format!("the moved chat's Codex could not be taken over: {e}"))?,
            );
            Ok(Adopted {
                agent: provider.clone(),
                external: Some(provider),
            })
        }
        MovedKind::Acp => {
            let (handoff, link) = external_parts::<AcpProviderHandoff>(record).map_err(|e| unreadable("record", e))?;
            let provider = Arc::new(
                AcpProvider::adopt(handoff, link, fds)
                    .map_err(|e| format!("the moved chat's agent could not be taken over: {e}"))?,
            );
            Ok(Adopted {
                agent: provider.clone(),
                external: Some(provider),
            })
        }
    }
}

/// An external provider's record as it crosses: the provider's own state beside its link's.
fn external_record(provider: Value, link: &LinkHandoff) -> Result<Value, String> {
    let link = serde_json::to_value(link).map_err(|e| format!("the agent's connection could not be written: {e}"))?;
    Ok(serde_json::json!({ "provider": provider, "link": link }))
}

fn external_parts<H: serde::de::DeserializeOwned>(record: &Value) -> Result<(H, LinkHandoff), serde_json::Error> {
    let field = |name: &str| record.get(name).cloned().unwrap_or(Value::Null);
    Ok((
        serde_json::from_value(field("provider"))?,
        serde_json::from_value(field("link"))?,
    ))
}

fn three(fds: Vec<OwnedFd>) -> Result<[OwnedFd; 3], String> {
    fds.try_into()
        .map_err(|fds: Vec<OwnedFd>| format!("a frozen provider holds 3 descriptors, not {}", fds.len()))
}

impl ChatMover for ClaudeSidecarProvider {
    fn live_move(&self) -> bool {
        ClaudeSidecarProvider::live_move(self)
    }
    fn freeze_for_chat_move(&self) -> Result<FrozenProvider, String> {
        let (handoff, fds) = ClaudeSidecarProvider::freeze_for_chat_move(self).map_err(|why| why.to_string())?;
        let record = match serde_json::to_value(&handoff) {
            Ok(record) => record,
            Err(err) => {
                let _ = self.unfreeze();
                return Err(format!("the sidecar's record could not be written: {err}"));
            }
        };
        Ok(FrozenProvider {
            kind: MovedKind::Claude,
            record,
            fds: fds.map_or_else(Vec::new, |fds| fds.into_array().into()),
        })
    }
    fn resume_after_chat_move(&self, frozen: FrozenProvider) -> Result<(), String> {
        // The descriptors are this process's duplicates; the sidecar's own stay with it.
        drop(frozen);
        self.unfreeze()
            .map_err(|e| format!("the sidecar's output could not be read again: {e}"))
    }
    fn commit_chat_move(&self) -> io::Result<()> {
        self.commit()
    }
    fn forget_chat_move(&self) {
        self.forget_for_move();
    }
}

impl ChatMover for CodexProvider {
    fn live_move(&self) -> bool {
        CodexProvider::live_move(self)
    }
    fn freeze_for_chat_move(&self) -> Result<FrozenProvider, String> {
        let (handoff, link, fds) = self.freeze_for_move().map_err(|why| why.to_string())?;
        let provider = serde_json::to_value(&handoff).map_err(|e| e.to_string());
        match provider.and_then(|provider| external_record(provider, &link)) {
            Ok(record) => Ok(FrozenProvider {
                kind: MovedKind::Codex,
                record,
                fds: fds.into(),
            }),
            Err(err) => {
                // Taken back at once: a record that cannot be written cannot move.
                let _ = self.resume_after_move(handoff, link, fds);
                Err(format!("Codex's record could not be written: {err}"))
            }
        }
    }
    fn resume_after_chat_move(&self, frozen: FrozenProvider) -> Result<(), String> {
        let (handoff, link) = external_parts::<CodexProviderHandoff>(&frozen.record).map_err(|e| e.to_string())?;
        self.resume_after_move(handoff, link, three(frozen.fds)?)
            .map_err(|e| format!("Codex could not be read again: {e}"))
    }
    fn commit_chat_move(&self) -> io::Result<()> {
        self.commit()
    }
    // The freeze already let go of the link without ending anything: the agent's stdio left with the record.
    fn forget_chat_move(&self) {}
}

impl ChatMover for AcpProvider {
    fn live_move(&self) -> bool {
        AcpProvider::live_move(self)
    }
    fn freeze_for_chat_move(&self) -> Result<FrozenProvider, String> {
        let (handoff, link, fds) = self.freeze_for_move().map_err(|why| why.to_string())?;
        let provider = serde_json::to_value(&handoff).map_err(|e| e.to_string());
        match provider.and_then(|provider| external_record(provider, &link)) {
            Ok(record) => Ok(FrozenProvider {
                kind: MovedKind::Acp,
                record,
                fds: fds.into(),
            }),
            Err(err) => {
                let _ = self.resume_after_move(handoff, link, fds);
                Err(format!("the agent's record could not be written: {err}"))
            }
        }
    }
    fn resume_after_chat_move(&self, frozen: FrozenProvider) -> Result<(), String> {
        let (handoff, link) = external_parts::<AcpProviderHandoff>(&frozen.record).map_err(|e| e.to_string())?;
        self.resume_after_move(handoff, link, three(frozen.fds)?)
            .map_err(|e| format!("the agent could not be read again: {e}"))
    }
    fn commit_chat_move(&self) -> io::Result<()> {
        self.commit()
    }
    fn forget_chat_move(&self) {}
}
