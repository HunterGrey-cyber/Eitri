//! Concrete `AgentProvider` implementations: `claude_sidecar::ClaudeSidecarProvider`, and `codex`, the
//! first provider other than the Claude sidecar, reached through `agent::external`.

pub mod claude_sidecar;
pub mod codex;
