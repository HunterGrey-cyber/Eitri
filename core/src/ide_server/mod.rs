//! The bottom terminal's `claude`, attached to the editor through the CLI's IDE lock file.
//!
//! Off unless `terminal.claude_ide` turns it on: a loopback WebSocket MCP server, named by a lock file under
//! `~/.claude/ide`, that plays the part an editor's extension plays for a `claude` in its integrated terminal.
//! That `claude` is no tab: it keeps its own permission prompts, and nothing here reaches the panel or its broker.
//!
//! GTK-free, and Linux only because the worker waits on eventfds and inotify. [`types`] is the vocabulary the threads
//! share; each other file holds one piece and names only what it needs of the rest.

pub mod channels;
pub mod diagnostics;
pub mod files;
pub mod frames;
pub mod handshake;
pub mod host;
pub mod keeper;
pub mod lines;
pub mod lock;
pub mod mcp;
pub mod mention;
pub mod selection;
pub mod server;
pub mod types;
pub mod view;
pub mod worker;

pub use types::*;
