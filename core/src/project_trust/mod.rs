//! A project's own agent configuration: what an agent would load from the project for a root, found
//! and fingerprinted so a session can load it only after the user has trusted exactly that
//! configuration.
//!
//! Each agent reads its own files, named by its [`Profile`]. Claude's CLI reads the project tier from
//! the working directory and the local tier from the git top level, and memory files from the
//! directories in between, so discovery walks from the root up to the git top level (never above
//! `$HOME` when the root is below it) and, in each directory, looks at `.claude/` (all of it),
//! `.mcp.json`, `CLAUDE.md` and `CLAUDE.local.md`. Codex reads `.codex/`, `.agents/skills/`,
//! `.agents/plugins/`, `AGENTS.md` and its alternate names over the same walk (see [`codex`]).
//!
//! The fingerprint covers all of it by content: every file's bytes, every symlink's target as
//! written and every directory's name. A link below a walked `.claude/`, whose whole content the CLI
//! reads, in a walked `.codex/skills/`, whose whole content Codex reads, or in place of a memory file
//! in a walked directory, is also hashed through to what it reaches, when that is inside the top
//! level by a way that never leaves it. Anything that could not be hashed -- a file over the size
//! limit, unreadable, a FIFO, a socket, a device, any other configuration file that is a link (and
//! every other link below a `.claude/` or in a skill tree), or a walk over its budget -- leaves a gap
//! in the fingerprint, so such a discovery is never `fully_hashed` and no answer about it may outlive
//! the start it was asked for.
//!
//! Every byte read from the disk here goes through [`nofollow`], which opens nothing through a link
//! (a link is followed by walking its target's names one at a time) and never blocks on a FIFO.

mod codex;
mod discover;
mod findings;
pub mod git_top;
pub mod nofollow;
mod plan;
mod store;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub use codex::{
    codex_fallback_names, codex_findings, hooks_json_findings, read_user_codex_config, rule_calls, scan_rules,
    RuleScan, ServerDefinition, UserCodexConfig,
};
pub use discover::discover;
pub use findings::{mcp_findings, settings_findings};
pub use git_top::{git_top_level, GitWalk};
pub use plan::{eitri_server_name, eitri_server_taken, trust_plan, EditorServerName, NonUtf8};
pub use store::{civil_date, for_root, Diff, Remember, TrustState, TrustStore, DISCLOSURE};

/// How much discovery may look at before it stops and says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Entries under all the `.claude/` directories and beside them, together.
    pub max_entries: usize,
    /// One file's size; a larger one is not hashed.
    pub max_file_bytes: u64,
    /// All files' sizes together.
    pub max_total_bytes: u64,
    /// Directories below a `.claude/`.
    pub max_depth: usize,
    /// Directories looked at for a `.git`, the root included.
    pub max_walk_up: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_entries: 2_000,
            max_file_bytes: 4 << 20,
            max_total_bytes: 16 << 20,
            max_depth: 16,
            max_walk_up: 64,
        }
    }
}

/// Whose configuration discovery looks for: each agent reads its own files from a project, and each
/// keeps its own record, so trusting a project for one agent never trusts it for another.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Profile {
    /// `.claude/`, `.mcp.json`, `CLAUDE.md`, `CLAUDE.local.md`. Keep its fingerprint and record name
    /// stable so existing records still match.
    Claude,
    /// `.codex/`, `.agents/skills/`, `.agents/plugins/`, `AGENTS.md`, `AGENTS.override.md`, the user's
    /// alternate memory names, and the plugin-marketplace listings at the top level.
    Codex,
    /// An agent registered by name, with the project files it reads: a name ending in `/` is a
    /// directory read whole, anything else one file.
    Acp { name: String, files: Vec<String> },
}

impl Profile {
    /// The profile's name in a record's file name and in its fingerprint: `claude`, `codex`, or
    /// `acp-<name>` with every byte outside `[A-Za-z0-9._-]` written `%XX`, so no registered name can
    /// spell another profile's key or a path.
    pub fn key(&self) -> String {
        match self {
            Profile::Claude => "claude".to_string(),
            Profile::Codex => "codex".to_string(),
            Profile::Acp { name, .. } => {
                let mut key = String::from("acp-");
                for byte in name.bytes() {
                    if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
                        key.push(byte as char);
                    } else {
                        key.push_str(&format!("%{byte:02X}"));
                    }
                }
                key
            }
        }
    }

    /// How a person reads the agent: `Claude`, `Codex`, or the registered name.
    pub fn label(&self) -> String {
        match self {
            Profile::Claude => "Claude".to_string(),
            Profile::Codex => "Codex".to_string(),
            Profile::Acp { name, .. } => name.clone(),
        }
    }

    /// Whether a session of this agent starts only after a `y` for exactly what is on disk, even when
    /// nothing is found. Claude and Codex can be started without the project's own configuration, so
    /// an `n` (or nothing to ask about) starts them untrusted; a registered agent has no such switch,
    /// so for it an `n` refuses the start and an empty project is still asked about.
    pub fn requires_explicit_yes(&self) -> bool {
        matches!(self, Profile::Acp { .. })
    }

    /// The first bytes of the fingerprint. Keep Claude's header stable so existing records still
    /// match; every other profile names itself, so two profiles never hash to the same value.
    pub(crate) fn fingerprint_header(&self) -> Vec<u8> {
        match self {
            Profile::Claude => b"eitri-trust-v1\n".to_vec(),
            other => format!("eitri-trust-v1 {}\n", other.key()).into_bytes(),
        }
    }
}

/// What a profile's discovery needs from the user's own agent configuration, read beforehand by the
/// trust worker.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileEnv {
    /// The effective `CODEX_HOME`, canonical. A `.codex/` that is this directory is the user's own
    /// configuration, not the project's, and is not read.
    pub codex_home: Option<PathBuf>,
    /// The alternate names of `AGENTS.md` from the user's Codex configuration
    /// ([`codex_fallback_names`]).
    pub fallback_names: Vec<String>,
    /// The MCP servers the user's own Codex configuration starts, with their definitions
    /// ([`UserCodexConfig::mcp_servers`]); `None` when that is not known.
    pub user_mcp_servers: Option<BTreeMap<String, ServerDefinition>>,
    /// Every server name in that configuration, the switched-off ones too
    /// ([`UserCodexConfig::all_server_names`]); `None` when that is not known.
    pub user_mcp_names: Option<BTreeSet<String>>,
    /// The user's Codex `config.toml` and why it could not be read, when it is there and could not be
    /// ([`UserCodexConfig::unreadable`]): the alternate names of `AGENTS.md` it may give are unknown,
    /// so a discovery cannot have hashed every file a session would load, and no answer about it
    /// outlives one start.
    pub user_config_unreadable: Option<(PathBuf, String)>,
}

/// The sha256 of a discovery's whole configuration.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn hex(&self) -> String {
        hex(&self.0)
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// What was found for one root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    /// Whose configuration this is; the record it is checked against is that profile's.
    pub profile: Profile,
    pub root: PathBuf,
    /// The git top level (never above `$HOME` when the root is below it); without one, the highest
    /// directory whose `.git` could not be checked, since git may take that one; else the root.
    pub top: PathBuf,
    /// The root, its parent, ... up to and including `top`.
    pub walked: Vec<PathBuf>,
    /// Sorted by the bytes of `path`. Empty means nothing to trust, but nothing to trust can also hold
    /// entries: the directories discovery names and leaves unread ([`Discovery::nothing_to_trust`]).
    pub entries: Vec<Entry>,
    pub findings: Vec<Finding>,
    pub fingerprint: Fingerprint,
    pub over_budget: Option<String>,
    /// [`ProfileEnv::user_mcp_servers`] as it was read for this discovery. Not part of the
    /// fingerprint: it is the user's own configuration, not the project's.
    pub user_mcp_servers: Option<BTreeMap<String, ServerDefinition>>,
    /// [`ProfileEnv::user_mcp_names`] as it was read for this discovery. Not part of the fingerprint
    /// either.
    pub user_mcp_names: Option<BTreeSet<String>>,
    /// Each MCP server a project `.codex/config.toml` on the walk defines, by name with its
    /// definition (a name defined in two walked directories is here twice). A file that was not read
    /// or parsed adds nothing here, and the entries and findings say so. Not part of the fingerprint,
    /// which already holds the files it is read from. Empty for every other profile.
    pub project_mcp_servers: Vec<(String, ServerDefinition)>,
}

impl Discovery {
    /// Nothing went unhashed: no entry that could not be read, no configuration file that is a
    /// link nothing was read through, and the walk stayed within its budget. Only then can an answer
    /// about this configuration be kept beyond one start.
    pub fn fully_hashed(&self) -> bool {
        self.over_budget.is_none()
            && !self
                .entries
                .iter()
                .any(|entry| matches!(entry.kind, EntryKind::Unreadable { .. } | EntryKind::Other))
            && !self
                .findings
                .iter()
                .any(|finding| matches!(finding, Finding::Unreadable { .. } | Finding::Unparsed { .. }))
    }

    /// The entries that could not be checked, each with its reason.
    pub fn uncheckable(&self) -> Vec<(&Path, &str)> {
        self.findings
            .iter()
            .filter_map(|finding| match finding {
                Finding::Unreadable { path, reason } => Some((path.as_path(), reason.as_str())),
                Finding::Unparsed { file, .. } => Some((file.as_path(), "could not be parsed for this check")),
                _ => None,
            })
            .collect()
    }

    /// Nothing on the path from the root to the top level that the CLI would load. A directory
    /// discovery names and leaves unread is not configuration, so it alone is nothing to ask about.
    pub fn nothing_to_trust(&self) -> bool {
        self.entries.iter().all(|entry| {
            matches!(entry.kind, EntryKind::Dir)
                && self
                    .findings
                    .iter()
                    .any(|finding| matches!(finding, Finding::NotRead { path, .. } if *path == entry.path))
        })
    }
}

/// One thing found, relative to `top`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    File {
        sha256: [u8; 32],
        len: u64,
    },
    /// `target` as written; `outside` when it does not lead below `top` (judged by text alone).
    Symlink {
        target: PathBuf,
        outside: bool,
    },
    /// A link reaching a regular file inside the top level: the link's `target` as written and what
    /// the file held when it was read through the link.
    LinkedFile {
        target: PathBuf,
        sha256: [u8; 32],
        len: u64,
    },
    Dir,
    /// Could not be hashed: too large, unreadable, a FIFO, a socket, a device.
    Unreadable {
        reason: String,
    },
    /// Neither a file, a directory nor a link where one was expected.
    Other,
}

/// What the prompt shows about a discovery. Paths are relative to `top` (with leading `..` for a
/// git directory a gitfile names somewhere else).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    Hook {
        file: PathBuf,
        event: String,
        matcher: Option<String>,
        command: String,
    },
    McpServer {
        file: PathBuf,
        name: String,
        command_line: String,
        /// How the server's tools are approved, as the file sets it (Codex's
        /// `default_tools_approval_mode` and each tool's `approval_mode`); `None` when it sets none.
        approval_mode: Option<String>,
    },
    /// One `env` entry of a settings file: the CLI applies these to its own process and to every
    /// tool it runs, so the value (a proxy address, `NODE_OPTIONS`) matters as much as the name.
    Env {
        file: PathBuf,
        key: String,
        value: String,
    },
    ApiKeyHelper {
        file: PathBuf,
        command: String,
    },
    Allow {
        file: PathBuf,
        rule: String,
    },
    AdditionalDirectory {
        file: PathBuf,
        path: String,
    },
    /// Every other top-level key, as its compact JSON.
    OtherSetting {
        file: PathBuf,
        key: String,
        value: String,
    },
    /// A configuration file this module could not parse (JSON, TOML, or a rule file that is not
    /// UTF-8). The agent's parser is not ours, so a file one rejects may be accepted by the other: the
    /// file counts as unchecked, and `reason` carries why and the file's text (lossily, when it is not
    /// UTF-8).
    Unparsed {
        file: PathBuf,
        reason: String,
    },
    /// A memory or instructions file the agent puts into its prompt (`CLAUDE.md`, `AGENTS.md`, ...).
    Memory {
        path: PathBuf,
    },
    /// A Codex exec-policy rule file, with each rule call in it as written (`prefix_rule(...)`,
    /// `network_rule(...)`, `host_executable(...)`), and the file's whole text when it holds anything
    /// besides those calls, comments and whitespace: a rule reached through another name or made by
    /// a function shows in no call, so then only the text says what the file does.
    RuleFile {
        path: PathBuf,
        calls: Vec<String>,
        text: Option<String>,
    },
    /// A skill directory, by name.
    Skill {
        path: PathBuf,
        name: String,
    },
    /// A plugin-marketplace listing.
    PluginListing {
        path: PathBuf,
    },
    /// A setting Eitri replaces on every session it starts (the sandbox and approval keys), so what the
    /// file says is not what runs.
    Overridden {
        file: PathBuf,
        key: String,
        value: String,
    },
    Symlink {
        path: PathBuf,
        target: PathBuf,
        outside: bool,
    },
    /// A file under a directory read whole that is not one the prompt says more about (a settings,
    /// memory, rule or listing file): a command, an agent, a skill's file, a hook script. Named under
    /// the directory it is in, with its size and, for a Markdown file, which of the frontmatter keys
    /// that change what runs it sets.
    OtherFile {
        /// The directory the file is directly in.
        dir: PathBuf,
        /// The file's name in `dir`.
        name: PathBuf,
        len: u64,
        /// The frontmatter keys among `hooks`, `allowed-tools`, `permissionMode` and `mcpServers` it sets, in that order.
        frontmatter: Vec<String>,
    },
    /// A directory that is named and deliberately not read, and why. It is not configuration the CLI
    /// loads for this root, so it is not unchecked either: its name is in the fingerprint, its
    /// content is not.
    NotRead {
        path: PathBuf,
        reason: String,
    },
    /// Cannot be checked, and why.
    Unreadable {
        path: PathBuf,
        reason: String,
    },
    OverBudget {
        reason: String,
    },
    /// A `.git`, `HEAD`, `commondir` or gitdir path that was a link, a FIFO, unreadable, too large or
    /// not what git accepts: not followed, not read, and the walk went on above it. Not configuration.
    GitUnverified {
        path: PathBuf,
        reason: String,
        target: Option<PathBuf>,
    },
}
