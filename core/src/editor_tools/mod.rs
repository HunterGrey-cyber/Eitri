//! The tools an agent calls on the person's editor, for every backend: what each tool is, how a call is
//! checked and judged, and how a chunk reaches an nvim. No GTK and no panel in here.
//!
//! `broker` runs the calls; `tools` holds one file pair per tool; `step` is what a tool and the broker say
//! to each other; `way` is how a call reaches an editor, directly or through the other window.

pub mod args;
#[cfg(target_os = "linux")]
pub mod bridge_host;
pub mod bridge_wire;
pub mod broker;
pub mod cards;
pub mod catalog;
pub mod follow;
pub mod gate;
pub mod memory;
pub mod paths;
pub mod proposals;
pub mod stamp;
pub mod step;
pub mod switches;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;
pub mod text_edits;
pub mod tools;
pub mod way;
pub mod words;

use std::time::Duration;

/// The tool server's name: the model sees each tool as `mcp__eitri__<tool>`.
pub const SERVER_NAME: &str = "eitri";

/// The transport's hard limit on one call, above [`CARD_LIMIT`], so a person reading a card is never cut
/// off by the transport first.
pub const CALL_TIMEOUT_MS: u32 = 900_000;

/// How long a broker card waits for an answer.
pub const CARD_LIMIT: Duration = Duration::from_secs(10 * 60);
