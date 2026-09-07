//! agent: a pure-Rust backend that manages Claude Code CLI sessions -- spawns `claude -p`,
//! translates its real stream-json wire protocol into provider-neutral events, and folds them
//! into a session state a future `agent-ui` module will consume. No GTK/WebView dependency; see
//! docs/superpowers/plans/2026-09-07-agent-claude-cli-backend.md for the full design.

mod event;
mod process;
mod session;
mod wire;

pub use event::AgentEvent;
pub use process::{AgentProcess, SpawnMode, CONSERVATIVE_DISALLOWED_TOOLS};
pub use session::{AgentSession, AgentSessionState, SessionStatus, ToolCallRecord};
pub use wire::translate_line;
