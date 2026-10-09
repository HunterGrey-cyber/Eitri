//! Concrete `AgentProvider` implementations: `claude_sidecar::ClaudeSidecarProvider`, and the providers
//! other than the Claude sidecar, reached through `agent::external`: `codex` (Codex's own app-server) and
//! `acp` (any agent that speaks the Agent Client Protocol).

pub mod acp;
pub mod claude_sidecar;
pub mod codex;
