//! A pure Rust backend for Claude Code CLI sessions. Spawns `claude -p`, translates its
//! stream-json output into provider-neutral events, and folds them into session state
//! for `agent-ui` to consume. This crate has no GTK or WebView dependency.

mod event;
mod process;
mod session;
mod wire;

pub use event::AgentEvent;
pub use process::{AgentProcess, SpawnMode, CONSERVATIVE_DISALLOWED_TOOLS};
pub use session::{AgentSession, AgentSessionState, SessionStatus, ToolCallRecord};
pub use wire::translate_line;
