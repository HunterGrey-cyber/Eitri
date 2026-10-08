//! The trust plan of one session start: built from the very discovery the user's answer was bound to,
//! and handed to the agent's provider, which applies it as given.
//!
//! Every ancestor of the root up to `/` gets an explicit level. An agent that decides each directory
//! by its own entry first then never falls back to a trust the user's own configuration gave a
//! directory it might walk, and one that would otherwise record a trust for the project writes
//! nothing when an entry is present.

use std::fmt;
use std::path::{Path, PathBuf};

use agent::{DirTrust, TrustPlan};

use super::{Discovery, Finding};

/// A root whose path is not UTF-8: no agent configuration can name it exactly, and spelling it
/// lossily would name another directory, so the start is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonUtf8(pub PathBuf);

impl fmt::Display for NonUtf8 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the project path {} is not valid UTF-8, so it cannot be named to the agent",
            self.0.display()
        )
    }
}

impl std::error::Error for NonUtf8 {}

/// The plan for a start at `root` answered `answer`:
/// - untrusted: every ancestor of `root`, `root` included, up to `/` untrusted, and skills off;
/// - trusted: the directories discovery walked (those it fingerprinted) trusted, every ancestor above
///   them untrusted.
///
/// `project_only` names each MCP server the discovery found only in the project's own configuration,
/// so a session that did not load that configuration can tell when one starts anyway. A name the
/// user's own Codex configuration also gives a running server is left out: that server starts in
/// every session, and its startup proves nothing. When the user's servers are not known (their file
/// could not be read), every name the project gives is kept, so the check errs toward stopping a
/// session rather than toward missing a project server.
pub fn trust_plan(d: &Discovery, answer: DirTrust, root: &Path) -> Result<TrustPlan, NonUtf8> {
    // Every ancestor of a UTF-8 path is UTF-8.
    if root.to_str().is_none() {
        return Err(NonUtf8(root.to_path_buf()));
    }
    let dirs = root
        .ancestors()
        .map(|dir| {
            let level = match answer {
                DirTrust::Trusted if d.walked.iter().any(|walked| walked == dir) => DirTrust::Trusted,
                _ => DirTrust::Untrusted,
            };
            (dir.to_path_buf(), level)
        })
        .collect();
    let mut project_only: Vec<String> = d
        .findings
        .iter()
        .filter_map(|finding| match finding {
            Finding::McpServer { name, .. } => Some(name.clone()),
            _ => None,
        })
        .filter(|name| {
            !d.user_mcp_servers
                .as_ref()
                .is_some_and(|user| user.iter().any(|own| own == name))
        })
        .collect();
    project_only.sort();
    project_only.dedup();
    Ok(TrustPlan {
        dirs,
        skills_off: answer == DirTrust::Untrusted,
        fingerprint: d.fingerprint.hex(),
        project_only,
    })
}
