//! The policy a Codex session runs under, and the `config` object every `thread/start` and
//! `thread/resume` carries.
//!
//! **The approval policy is a constant, and the sandbox is one of two.** The tab's mode changes only the
//! reviewer (Codex's own in Auto, the user's in Bypass), so a Codex tab never runs a no-ask policy and never
//! runs unsandboxed except for a single escalation someone approved.
//!
//! **The sandbox is `workspace-write`.** Inside it Codex runs every command and every edit within the
//! project without asking, writes `/tmp` and `$TMPDIR`, and reads any file the user can read: the sandbox
//! restricts writes, network and sockets, not reads. That is stated to the user as it is. The narrow
//! `sandbox_workspace_write` table is always sent explicitly, because without it a user's or a trusted
//! project's `network_access = true` or extra writable roots would widen the sandbox.
//!
//! **At a root that is no boundary it is `read-only`.** Keeping writes inside such a folder keeps nothing
//! safe, so nothing is written without an escalation, which is `on-request` as ever.
//!
//! **The config is nested objects, never dotted keys.** The app-server splits each top-level `config`
//! key on every `.` with no quoting, so `projects./a.b/c.trust_level` would name a garbage path; the
//! directory would then have no entry, and Codex writes `trusted` for it into the user's own
//! `config.toml`. Keys inside an object are not split, so a directory name is always one key of the
//! `projects` object.

use super::wire::{trust_level_word, SandboxPolicyWire};
use crate::external::{DirTrust, Reviewer, Sandbox, TrustPlan};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// Codex asks only when a command needs to leave the sandbox, and whoever reviews decides.
pub const APPROVAL_POLICY: &str = "on-request";

/// Writes inside the project, no network, no sockets; reads are not restricted.
pub const SANDBOX_MODE: &str = "workspace-write";

/// The per-turn sandbox policy's `type`; it must name the same sandbox as [`SANDBOX_MODE`].
const SANDBOX_POLICY_TYPE: &str = "workspaceWrite";

/// No writes, no network, no sockets; reads are not restricted.
pub const READ_ONLY_SANDBOX_MODE: &str = "read-only";

/// The per-turn sandbox policy's `type`; it must name the same sandbox as [`READ_ONLY_SANDBOX_MODE`].
const READ_ONLY_POLICY_TYPE: &str = "readOnly";

/// The sandbox a `thread/start` or `thread/resume` names.
pub fn sandbox_mode(sandbox: Sandbox) -> &'static str {
    match sandbox {
        Sandbox::Project => SANDBOX_MODE,
        Sandbox::ReadOnly => READ_ONLY_SANDBOX_MODE,
    }
}

/// The sandbox policy sent with every `turn/start`: the workspace sandbox with no extra writable root, no
/// network, and Codex's own defaults for the temporary directories, or the read-only one with no network.
pub fn sandbox_policy(sandbox: Sandbox) -> SandboxPolicyWire {
    match sandbox {
        Sandbox::Project => SandboxPolicyWire::narrow(SANDBOX_POLICY_TYPE),
        Sandbox::ReadOnly => SandboxPolicyWire::read_only(READ_ONLY_POLICY_TYPE),
    }
}

/// Why a Codex session cannot be started for a plan. Each one reads as a sentence for a tab notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionConfigError {
    /// A directory whose name is not UTF-8 cannot be a JSON key exactly, and a lossy name would leave
    /// the real directory without an entry.
    NonUtf8Root(PathBuf),
    /// A relative directory, or one with `.` or `..` components, is not the name Codex looks up.
    NotCanonical(PathBuf),
    /// A plan with no directory would send no `projects` entry at all.
    NoDirectories,
}

impl fmt::Display for SessionConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionConfigError::NonUtf8Root(p) => write!(
                f,
                "Codex cannot be started here: the folder name {} is not valid UTF-8, so Eitri cannot tell Codex \
                 how far to trust it",
                p.display()
            ),
            SessionConfigError::NotCanonical(p) => {
                write!(
                    f,
                    "Codex cannot be started here: {} is not an absolute, plain folder path",
                    p.display()
                )
            }
            SessionConfigError::NoDirectories => {
                write!(
                    f,
                    "Codex cannot be started here: no folder was given to set its trust for"
                )
            }
        }
    }
}

impl std::error::Error for SessionConfigError {}

/// The `config` object a `thread/start` or `thread/resume` carries.
///
/// Its contents are private and only [`session_config`] makes one, so no caller can add a top-level key
/// (a dotted path, `hooks`, a tool server, a wider sandbox table) to what goes on the wire.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SessionConfig(Value);

impl SessionConfig {
    /// The object as JSON, for reading only.
    pub fn as_value(&self) -> &Value {
        &self.0
    }

    /// The same config with each of the named tool servers of the user's own Codex configuration turned
    /// off, for tests that run a real Codex on a real account: its servers must not start there. Each
    /// name is one key of a nested object, so a name holding `.` still names exactly that server.
    #[cfg(feature = "test-support")]
    pub fn with_tool_servers_off(mut self, names: &[&str]) -> Self {
        let mut servers = Map::new();
        for name in names {
            servers.insert((*name).to_string(), json!({ "enabled": false }));
        }
        if let Value::Object(config) = &mut self.0 {
            config.insert(TOOL_SERVERS_KEY.to_string(), Value::Object(servers));
        }
        self
    }
}

impl SessionConfig {
    /// The same config without the lever that keeps skills out of the thread, for the self-check's own
    /// test: a check that cannot notice a missing lever proves nothing.
    #[cfg(feature = "test-support")]
    pub fn without_skills_lever(mut self) -> Self {
        if let Value::Object(config) = &mut self.0 {
            config.remove("skills");
        }
        self
    }
}

/// The config table of the user's tool servers; written only by tests.
#[cfg(feature = "test-support")]
const TOOL_SERVERS_KEY: &str = "mcp_servers";

/// The `config` object for one start or resume, built from the plan the user's trust answer was bound to.
///
/// - `projects`: an explicit entry for every directory of the plan and for every ancestor of each up to
///   `/`. A runtime entry overrides the user's own file for the same directory, so no directory Codex
///   might walk keeps a trust the user's file gave it, and with an entry present Codex never writes one
///   into that file. An ancestor the plan does not name is untrusted, and a directory named twice keeps
///   the narrower level.
/// - `skills`: `include_instructions = false` when the plan turns skills off (an untrusted start).
/// - `features`: Codex's extra-permission requests off, so a command approval can never grant more than
///   its card shows.
/// - `sandbox_workspace_write`: the narrow sandbox, always.
pub fn session_config(plan: &TrustPlan) -> Result<SessionConfig, SessionConfigError> {
    if plan.dirs.is_empty() {
        return Err(SessionConfigError::NoDirectories);
    }
    fn set(levels: &mut BTreeMap<String, DirTrust>, key: String, level: DirTrust) {
        let entry = levels.entry(key).or_insert(level);
        if level == DirTrust::Untrusted {
            *entry = DirTrust::Untrusted;
        }
    }
    let mut levels: BTreeMap<String, DirTrust> = BTreeMap::new();
    for (dir, level) in &plan.dirs {
        set(&mut levels, config_key(dir)?, *level);
        for ancestor in dir.ancestors().skip(1) {
            let key = config_key(ancestor)?;
            if !levels.contains_key(&key) && !plan.dirs.iter().any(|(d, _)| d.as_path() == ancestor) {
                set(&mut levels, key, DirTrust::Untrusted);
            }
        }
    }

    let mut projects = Map::new();
    for (dir, level) in levels {
        projects.insert(dir, json!({ "trust_level": trust_level_word(level) }));
    }
    let mut config = Map::new();
    config.insert("projects".into(), Value::Object(projects));
    if plan.skills_off {
        config.insert("skills".into(), json!({ "include_instructions": false }));
    }
    config.insert(
        "features".into(),
        json!({ "exec_permission_approvals": false, "request_permissions_tool": false }),
    );
    config.insert(
        "sandbox_workspace_write".into(),
        json!({
            "writable_roots": [],
            "network_access": false,
            "exclude_tmpdir_env_var": false,
            "exclude_slash_tmp": false,
        }),
    );
    Ok(SessionConfig(Value::Object(config)))
}

/// The exact name Codex keys a directory by: absolute, no `.` or `..`, no trailing slash, UTF-8.
fn config_key(dir: &Path) -> Result<String, SessionConfigError> {
    if !dir.is_absolute() {
        return Err(SessionConfigError::NotCanonical(dir.to_path_buf()));
    }
    let mut clean = PathBuf::new();
    for c in dir.components() {
        match c {
            Component::RootDir | Component::Normal(_) => clean.push(c.as_os_str()),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(SessionConfigError::NotCanonical(dir.to_path_buf()))
            }
        }
    }
    // `Path::components` drops a `.` in the middle of a path; the text must not have held one either.
    let text = dir
        .to_str()
        .ok_or_else(|| SessionConfigError::NonUtf8Root(dir.to_path_buf()))?;
    if text.split('/').any(|seg| seg == "." || seg == "..") {
        return Err(SessionConfigError::NotCanonical(dir.to_path_buf()));
    }
    clean
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| SessionConfigError::NonUtf8Root(dir.to_path_buf()))
}

/// What a `thread/start` or `thread/resume` answer must echo for the session Eitri asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Echo {
    pub approval_policy: &'static str,
    pub approvals_reviewer: Reviewer,
    pub sandbox: SandboxPolicyWire,
}

/// The echo expected of a session started with `reviewer` and `sandbox`: the constant policy, that reviewer,
/// and that sandbox exactly as [`sandbox_policy`] sends it.
pub fn expected_echo(reviewer: Reviewer, sandbox: Sandbox) -> Echo {
    Echo {
        approval_policy: APPROVAL_POLICY,
        approvals_reviewer: reviewer,
        sandbox: sandbox_policy(sandbox),
    }
}
