//! The library target exposes the live `terminal_panel` wiring (`TerminalSession`, `WatchHub`,
//! and the raw and semantic panes) to diagnostic harnesses and real-session smoke tests.
//! Reusing `wire_live_session` avoids duplicating the product binary's wiring in test binaries.
//! Keep this surface limited to what a harness needs to drive the real bottom slot.

pub mod terminal_panel;
