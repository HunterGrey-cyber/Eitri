//! Concrete `AgentProvider` implementations (design doc §10.1). Currently just
//! `claude_sidecar::ClaudeSidecarProvider` -- a second provider (e.g. a future Codex adapter)
//! would be a sibling module here, not a restructure of this one.

pub mod claude_sidecar;
