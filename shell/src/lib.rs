//! `shell` is primarily a binary (`src/main.rs`); this library target exists for exactly one
//! reason: so the real, live `terminal_panel` wiring (`TerminalSession` + `WatchHub` + the raw
//! and semantic panes) can be exercised from something other than the product binary itself --
//! a diagnostic harness, a real-session smoke test -- without duplicating that wiring. Every
//! earlier attempt at this duplicated `wire_live_session`'s logic in a throwaway `src/bin/*.rs`
//! diagnostic (see `terminal-pane/MANUAL_VERIFICATION.md`'s 2026-09-13 entries); duplication is
//! exactly the risk a "second implementation just for tests" the Semantic Pane Integration v0
//! brief explicitly says not to take. Keep this surface minimal: only what a harness genuinely
//! needs to drive the real bottom slot.

pub mod terminal_panel;
