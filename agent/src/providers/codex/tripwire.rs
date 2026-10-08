//! What a Codex session may not do, checked against what Codex reports.
//!
//! Two kinds of check, both of which end the tab through [`AgentDomainEvent::PolicyMismatch`]:
//!
//! - **The policy echo.** A start or resume answer names the approval policy, the reviewer and the sandbox
//!   the session actually runs under. They are compared with what Eitri sent, exactly: a different policy
//!   or reviewer, a sandbox other than the narrow workspace one, and a field that is *missing* all fail. The
//!   sandbox field is marked legacy in Codex's own schema, so a Codex that drops it must not pass for
//!   one that merely agrees.
//! - **Project configuration in an untrusted session.** An instruction file from a project directory, a
//!   tool server only the project's configuration names, and a hook from the project layer all mean Codex
//!   read something the user was never asked about. None of them has a signal before it happens, so these
//!   are looked for in what Codex says it loaded and started.
//!
//! Everything here is a pure function of what Codex said, so each rule has a test that needs no process.

use super::config::Echo;
use super::wire::{reviewer_word, ThreadEcho};
use crate::external::{DirTrust, TrustPlan};
use crate::projection::AgentDomainEvent;
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// What the tab says when Codex loaded something an untrusted session may not.
pub const UNTRUSTED_LOADED: &str = "Codex loaded project configuration Eitri did not allow";

/// Codex did something other than what Eitri asked for, in words for the tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyMismatch {
    /// What Eitri sent or allowed.
    pub requested: String,
    /// What Codex reported or did.
    pub reported: String,
}

impl PolicyMismatch {
    fn project_loaded(what: String) -> PolicyMismatch {
        PolicyMismatch {
            requested: "no project configuration in an untrusted session".to_string(),
            reported: format!("{UNTRUSTED_LOADED}: {what}"),
        }
    }

    /// The event the host fails the tab on.
    pub fn into_event(self) -> AgentDomainEvent {
        AgentDomainEvent::PolicyMismatch {
            requested: self.requested,
            reported: self.reported,
        }
    }
}

impl fmt::Display for PolicyMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "asked for {}, Codex reported {}", self.requested, self.reported)
    }
}

/// A reported value for a sentence, `absent` for a field Codex did not send.
fn shown(value: Option<&Value>) -> String {
    match value {
        Some(v) => v.to_string(),
        None => "absent".to_string(),
    }
}

/// Compares what a start or resume answer reports with what was sent.
///
/// Every field must be present and equal. The sandbox is compared as a whole, so a writable root, network
/// access, an excluded temporary directory or any other type of sandbox is a difference, and so is a field
/// Codex added that Eitri did not ask for.
pub fn check_echo(sent: &Echo, got: &ThreadEcho) -> Result<(), PolicyMismatch> {
    let want_policy = Value::String(sent.approval_policy.to_string());
    let want_reviewer = Value::String(reviewer_word(sent.approvals_reviewer).to_string());
    let want_sandbox = sent.sandbox.to_value();

    let mut asked = Vec::new();
    let mut reported = Vec::new();
    let mut compare = |name: &str, want: &Value, got: Option<&Value>| {
        if got != Some(want) {
            asked.push(format!("{name} {want}"));
            reported.push(format!("{name} {}", shown(got)));
        }
    };
    compare("approvalPolicy", &want_policy, got.approval_policy.as_ref());
    compare("approvalsReviewer", &want_reviewer, got.approvals_reviewer.as_ref());
    compare("sandbox", &want_sandbox, got.sandbox.as_ref());
    if asked.is_empty() {
        return Ok(());
    }
    Err(PolicyMismatch {
        requested: asked.join("; "),
        reported: reported.join("; "),
    })
}

/// Removes `.` and resolves `..` without touching the disk: an instruction file that is not there yet
/// must still be placed.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Fails when an instruction file Codex says it loaded sits in a directory the session was told not to
/// trust.
///
/// `walked` are those directories, each one the folder an instruction file would be read from, not its
/// ancestors' subtrees: a root at `/home/user` must not make everything below `/home` suspect. A file in
/// the effective `codex_home` is the user's own and always allowed, even where `walked` holds its parent
/// (a root at `$HOME` legitimately lists `~/.codex/AGENTS.md`).
pub fn check_instruction_sources(
    sources: &[String],
    walked: &[PathBuf],
    codex_home: &Path,
) -> Result<(), PolicyMismatch> {
    let home = lexical(codex_home);
    let walked: HashSet<PathBuf> = walked.iter().map(|d| lexical(d)).collect();
    for source in sources {
        let file = lexical(Path::new(source));
        if file.starts_with(&home) {
            continue;
        }
        if file.parent().is_some_and(|dir| walked.contains(dir)) {
            return Err(PolicyMismatch::project_loaded(format!(
                "it read the instruction file {source}"
            )));
        }
    }
    Ok(())
}

/// The directories an untrusted start was told not to read project instructions from.
pub fn dirs_not_trusted(plan: &TrustPlan) -> Vec<PathBuf> {
    plan.dirs
        .iter()
        .filter(|(_, level)| *level == DirTrust::Untrusted)
        .map(|(dir, _)| dir.clone())
        .collect()
}

/// What an untrusted session watches for after it started: tool servers and hooks that only the project's
/// own configuration provides.
#[derive(Debug, Clone, Default)]
pub struct TrustWatch {
    /// Names of tool servers found only in project configuration.
    project_only: HashSet<String>,
    /// Whether a hook from the project layer is a violation. A trusted session lets those run.
    hooks_gated: bool,
}

impl TrustWatch {
    /// A watch for an untrusted session, over these tool servers.
    pub fn new(project_only: impl IntoIterator<Item = String>) -> TrustWatch {
        TrustWatch {
            project_only: project_only.into_iter().collect(),
            hooks_gated: true,
        }
    }

    /// The watch a plan calls for: a plan that turns project skills off is an untrusted one; a trusted one
    /// has nothing to watch because the user allowed what the project configures.
    pub fn for_plan(plan: &TrustPlan) -> TrustWatch {
        if plan.skills_off {
            TrustWatch::new(plan.project_only.iter().cloned())
        } else {
            TrustWatch::default()
        }
    }

    /// A tool server reported a startup status, on any thread of the family.
    pub fn mcp_started(&self, name: &str) -> Result<(), PolicyMismatch> {
        if self.project_only.contains(name) {
            return Err(PolicyMismatch::project_loaded(format!(
                "its tool server {name:?} started, and only the project's configuration names it"
            )));
        }
        Ok(())
    }

    /// A hook started; `source` is the configuration layer Codex says it came from.
    pub fn hook_started(&self, source: &str) -> Result<(), PolicyMismatch> {
        if self.hooks_gated && source.trim().to_ascii_lowercase().starts_with("project") {
            return Err(PolicyMismatch::project_loaded(
                "a hook from the project's configuration ran".to_string(),
            ));
        }
        Ok(())
    }
}
