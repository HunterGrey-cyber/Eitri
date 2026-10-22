//! A generic Agent Client Protocol (v1) client, driven over stdio.
//!
//! - `wire`: hand-written types for the methods Eitri sends and lenient readers for what an agent sends.
//!   An update kind this build does not know is `Unknown`, never an error.
//! - `translate`: an agent's words as Eitri's events (tool calls, message chunks, how a turn ended and
//!   what it used), shared by the live stream and the replay of a loaded session.
//! - `history`: the replay of `session/load` as past turns.
//! - `provider`: `AcpProvider`, the child, its handshake, its sessions, its turns and its requests.
//!
//! Nothing here is specific to one agent: the registration (`AcpAgentSpec`) names the command and the one
//! mode Eitri may set.

pub mod history;
pub mod provider;
pub mod translate;
pub mod wire;

pub use crate::external::ContextGauge;
pub use provider::{AcpAgentSpec, AcpProvider, AcpProviderHandoff};
