//! OpenAI Codex, driven through `codex app-server` over stdio.
//!
//! - `wire`: hand-written serde types for the app-server methods Eitri uses, and nothing else. Values
//!   Codex may add later (an item type, a status, an error variant) read as `Unknown`, never as an error.
//! - `config`: the policy constants and the session config every start and resume carries.
//! - `family`: the tab's thread and the subagent threads announced from it, and which of their turns run.
//! - `translate`: Codex's words as Eitri's (the version, how a turn ended, tool calls, usage and limits).
//! - `approvals`: Codex's requests for a decision -- which become cards, which Eitri answers itself, and
//!   what each answer writes.
//! - `resume`: reading a resumed thread's history back in small pages.
//! - `tripwire`: the policy echo and the project configuration an untrusted session must not load.
//! - `locate`: which `codex` to run (the variable, the `init.lua` key, `PATH`).
//! - `scripted_model`: a Responses-API stand-in on loopback that answers from a script, for the self-check and tests.
//! - `self_check`: the per-binary check that a `codex` keeps the levers every tab relies on.
//! - `provider`: `CodexProvider`, the child, its handshake, its turns and its stream.
//!
//! What this module may send is an allowlist; `agent/tests/codex_wire_guard.rs` scans its sources for any
//! literal outside it.

pub mod approvals;
pub mod config;
pub mod family;
pub mod locate;
pub mod provider;
pub mod resume;
pub mod scripted_model;
pub mod self_check;
pub mod translate;
pub mod tripwire;
pub mod wire;

pub use locate::{decide_codex, locate_codex, LocateError};
pub use provider::{refuse_relative_codex_home, CodexLaunch, CodexProvider, HIGHEST_TESTED_CODEX, MIN_CODEX};
pub use scripted_model::{ScriptedModel, Step};
pub use self_check::{ensure_checked, BinaryKey, SelfCheckError};
pub use translate::parse_user_agent_version;
