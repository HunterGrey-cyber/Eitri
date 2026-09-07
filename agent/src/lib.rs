//! A pure Rust backend for Claude Code CLI sessions. Each conversation uses one long-lived,
//! full-duplex `claude --print --input-format stream-json --output-format stream-json`
//! process. User turns and control requests, including mid-turn `interrupt`, go to its stdin.
//! The stream-json output becomes provider-neutral events folded into session state for
//! `agent-ui` to consume.
//!
//! Tool permissions are gated by a generated `PreToolUse` hook relayed over a per-conversation
//! Unix socket by the `agent-hook` companion binary. The CLI's `can_use_tool` control request
//! alone does not reliably gate tool execution. This crate has no GTK or WebView dependency.

mod event;
mod process;
mod session;
mod wire;

pub mod hook_protocol;
pub mod settings;

pub use event::{AgentEvent, PermissionSource};
pub use process::{AgentProcess, PermissionMode, CONSERVATIVE_DISALLOWED_TOOLS};
pub use session::{AgentSession, AgentSessionState, PermissionRequestRecord, SessionStatus, ToolCallRecord};
pub use wire::translate_line;
