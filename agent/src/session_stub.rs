//! eitri-only: the legacy backend's `AgentSession` in a build without it (the `legacy-backend`
//! feature off, which is every release: spec 2026-09-27-v1-dist-design.md §10, D16).
//!
//! **Uninhabited, with the real type's public surface.** `eitri-core`'s
//! `AgentBackend::Legacy(AgentSession)` arm and its call sites in `core` and `shell` compile
//! unchanged against this, so the `cfg` stays inside `agent` (and out of
//! `core/src/agent_backend.rs`); but no value of this type can exist, so that arm can never be
//! constructed, and none of the real session, CLI spawn, wire or hook-relay code is compiled.
//! `start` is the only way in, and it fails with `LEGACY_NOT_IN_BUILD`.
//!
//! **A struct with an `Infallible` field, not the `enum AgentSession {}` D16 names**, and only
//! because the real type has a public field: `core` reads `session.projection` (three sites in
//! `agent_backend.rs`), which an enum cannot have. A struct with an uninhabited field is itself
//! uninhabited, the field is private so nothing outside this module could even try to build one,
//! and every method body is `match self.never {}` -- the same proof `match *self {}` gives for an
//! empty enum.
//!
//! **Keep the signatures in step with `session.rs`.** A method `core` or `shell` starts calling on
//! the real type fails the default build until it is added here too -- loud, not silent. The test
//! suite runs both configurations (the default, then `--features shell/legacy-backend`).

use crate::projection::{AgentDomainEvent, AgentSessionProjection};
use crate::provider::PermissionDecision;
use crate::setting_sources::ProjectTrust;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::path::Path;

// This stub and `LEGACY_BACKEND_COMPILED` (which backend selection reads) can never disagree: this
// module is compiled exactly when the feature is off, so the build fails here if the flag ever says
// otherwise.
const _: () = assert!(!crate::LEGACY_BACKEND_COMPILED);

/// See the module doc. Never constructed: `start` always fails.
pub struct AgentSession {
    /// The real type's one public field, so `core`'s reads of it compile. Never read: no
    /// `AgentSession` exists to read it from.
    pub projection: AgentSessionProjection,
    /// What makes the type uninhabited.
    never: Infallible,
    /// The real session owns an `mpsc::Receiver` (through `AgentProcess`), which makes it `Send`
    /// but not `Sync`. Mirrored so code that compiles against this stub is not relying on an auto
    /// trait the real type lacks.
    _auto_traits: PhantomData<std::sync::mpsc::Receiver<()>>,
}

impl AgentSession {
    /// Always fails: this build has no legacy backend. `io::ErrorKind::Unsupported`, carrying
    /// `LEGACY_NOT_IN_BUILD`, so the caller's "failed to start the legacy Claude backend: {e}"
    /// names the reason and the flag that brings it back.
    pub fn start(_project_dir: &Path, _disallowed_tools: &[&str], _project: ProjectTrust) -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            crate::LEGACY_NOT_IN_BUILD,
        ))
    }

    pub fn send_turn(&mut self, _text: &str) -> std::io::Result<Vec<AgentDomainEvent>> {
        match self.never {}
    }

    pub fn interrupt(&mut self) -> std::io::Result<Vec<AgentDomainEvent>> {
        match self.never {}
    }

    pub fn respond_permission(
        &mut self,
        _permission_id: &str,
        _decision: PermissionDecision,
    ) -> std::io::Result<Vec<AgentDomainEvent>> {
        match self.never {}
    }

    pub fn pump(&mut self) -> Vec<AgentDomainEvent> {
        match self.never {}
    }

    /// The real type's `pump` with each event's revision (added by main's v1 sweep, P1-A2);
    /// uninhabited here like every other method.
    pub fn pump_revised(&mut self) -> Vec<(u64, AgentDomainEvent)> {
        match self.never {}
    }

    pub fn event_log(&self) -> &[AgentDomainEvent] {
        match self.never {}
    }

    pub fn fold_locally(&mut self, _event: AgentDomainEvent) -> AgentDomainEvent {
        match self.never {}
    }

    pub fn pid(&self) -> u32 {
        match self.never {}
    }

    pub fn has_exited(&mut self) -> bool {
        match self.never {}
    }

    pub fn shutdown(&mut self) {
        match self.never {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gate itself (spec §10, D16): in a build without the feature the legacy backend cannot
    /// start, and says why in words a developer can act on.
    #[test]
    fn start_is_unsupported_and_names_the_feature_that_brings_legacy_back() {
        let error = match AgentSession::start(Path::new("."), &[], ProjectTrust::Untrusted) {
            Ok(_) => panic!("a build without the legacy backend must not start one"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        assert_eq!(error.to_string(), crate::LEGACY_NOT_IN_BUILD);
        assert!(
            crate::LEGACY_NOT_IN_BUILD.contains("--features shell/legacy-backend"),
            "the message must name the flag: {}",
            crate::LEGACY_NOT_IN_BUILD
        );
    }
}
