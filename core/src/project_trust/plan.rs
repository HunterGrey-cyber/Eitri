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

use super::{Discovery, EntryKind, Finding};

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
/// `project_only` names each MCP server the discovery found in the project's own configuration and not
/// as the user's own, so a session that did not load that configuration can tell when one starts
/// anyway. A name the user's own Codex configuration also gives a running server is left out only when
/// every definition the project gives it is the user's, value for value: that server starts in every
/// session, and its startup proves nothing. A project that defines the name otherwise starts a server
/// of its own under it, so the name is watched for, and a session whose user server of that name
/// starts stops too, which is the safe side. When the user's servers are not known (their file
/// could not be read, or `CODEX_HOME` is relative and so may name a directory of the project), every
/// name the project gives is kept, so the check errs toward stopping a session rather than toward
/// missing a project server.
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
        .filter(|name| !runs_as_the_users_own(d, name))
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

/// Whether the server `name` starts the same in a session that did not load the project's
/// configuration: the user's own configuration runs a server of that name, and each definition the
/// project gives it is that server's.
fn runs_as_the_users_own(d: &Discovery, name: &str) -> bool {
    let Some(own) = d.user_mcp_servers.as_ref().and_then(|user| user.get(name)) else {
        return false;
    };
    let mut project = d.project_mcp_servers.iter().filter(|(n, _)| n == name).peekable();
    project.peek().is_some() && project.all(|(_, definition)| definition == own)
}

/// What is known of a server of the editor's name in Codex's configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorServerName {
    Free,
    /// A project's definition or the user's, switched off or not.
    Defined,
    /// A file that could define it could not be read, or nothing was discovered at all.
    Unknown,
}

/// What Codex's configuration holds of the editor's name. A file of either side that could not be read leaves it
/// unknown, since a session that is answered `y` loads what discovery could not see.
pub fn eitri_server_name(d: &Discovery) -> EditorServerName {
    let name = crate::editor_tools::SERVER_NAME;
    let defined = d.project_mcp_servers.iter().any(|(defined, _)| defined == name)
        || d.user_mcp_names.as_ref().is_some_and(|names| names.contains(name));
    if defined {
        EditorServerName::Defined
    } else if d.user_mcp_names.is_none() || project_config_unknown(d) {
        EditorServerName::Unknown
    } else {
        EditorServerName::Free
    }
}

/// Whether the session must run without the editor's server: a runtime entry would merge per key with a definition
/// of that name, its `env` or `cwd` landing on the bridge and the approval mode covering a server that is not Eitri's.
/// A name that is only unknown counts.
pub fn eitri_server_taken(d: &Discovery) -> bool {
    eitri_server_name(d) != EditorServerName::Free
}

/// Whether a project `.codex/config.toml` may hold servers that are not in [`Discovery::project_mcp_servers`]:
/// the walk ran out of budget, so it may never have looked; the file, its `.codex/` or a walked directory is a
/// link or could not be read; or this parser refused the file, which Codex's own may load. A gap anywhere else
/// says nothing about servers: an `AGENTS.md` that is a link is common and defines none.
fn project_config_unknown(d: &Discovery) -> bool {
    const CONFIG: &str = ".codex/config.toml";
    let on_the_way_to_config = |path: &Path| {
        path.ends_with(CONFIG) || path.ends_with(".codex") || d.walked.iter().any(|dir| *dir == d.top.join(path))
    };
    d.over_budget.is_some()
        || d.entries.iter().any(|entry| {
            !matches!(entry.kind, EntryKind::File { .. } | EntryKind::Dir) && on_the_way_to_config(&entry.path)
        })
        || d.findings
            .iter()
            .any(|finding| matches!(finding, Finding::Unparsed { file, .. } if file.ends_with(CONFIG)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_trust::{read_user_codex_config, Fingerprint, Profile, ServerDefinition};
    use std::collections::{BTreeMap, BTreeSet};

    fn discovery(user: Option<&[&str]>, project: &[&str]) -> Discovery {
        Discovery {
            profile: Profile::Codex,
            root: PathBuf::from("/p"),
            top: PathBuf::from("/p"),
            walked: vec![PathBuf::from("/p")],
            entries: Vec::new(),
            findings: Vec::new(),
            fingerprint: Fingerprint([0; 32]),
            over_budget: None,
            user_mcp_servers: Some(BTreeMap::new()),
            user_mcp_names: user.map(|names| names.iter().map(|name| name.to_string()).collect()),
            project_mcp_servers: project
                .iter()
                .map(|name| (name.to_string(), ServerDefinition::of(&toml::Value::Boolean(true))))
                .collect(),
            instructions: BTreeMap::new(),
        }
    }

    /// The names the reader keeps for a `config.toml` holding `text`.
    fn names_in(label: &str, text: &str) -> Option<BTreeSet<String>> {
        let file = agent::state_dirs::test_workspace_dir(label).join("config.toml");
        std::fs::write(&file, text).unwrap();
        read_user_codex_config(&file).all_server_names
    }

    #[test]
    fn a_users_eitri_is_taken_even_switched_off() {
        let names = names_in(
            "taken-switched-off",
            "[mcp_servers.eitri]\nenabled = false\ncommand = \"x\"\n[mcp_servers.other]\ncommand = \"y\"\n",
        );
        assert_eq!(
            names,
            Some(BTreeSet::from(["eitri".to_string(), "other".to_string()])),
            "a runtime entry merges per key with a switched-off one too"
        );
        assert!(eitri_server_taken(&discovery(Some(&["eitri", "other"]), &[])));
    }

    #[test]
    fn a_projects_eitri_is_taken() {
        assert!(eitri_server_taken(&discovery(Some(&[]), &["other", "eitri"])));
    }

    #[test]
    fn an_unreadable_user_file_is_taken() {
        assert_eq!(names_in("taken-bad-toml", "not toml ["), None);
        assert_eq!(names_in("taken-not-a-table", "mcp_servers = 1\n"), None);
        assert!(eitri_server_taken(&discovery(None, &[])));
    }

    #[test]
    fn no_eitri_is_free() {
        let missing = agent::state_dirs::test_workspace_dir("taken-no-file").join("config.toml");
        assert_eq!(read_user_codex_config(&missing).all_server_names, Some(BTreeSet::new()));
        assert_eq!(names_in("taken-no-table", "model = \"x\"\n"), Some(BTreeSet::new()));
        assert!(!eitri_server_taken(&discovery(Some(&["other"]), &["another"])));
        assert!(!eitri_server_taken(&discovery(Some(&[]), &[])));
    }
}
