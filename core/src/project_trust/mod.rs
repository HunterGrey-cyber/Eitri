//! A project's own Claude configuration: what the CLI would load from the project and local
//! setting tiers for a root, found and fingerprinted so a session can load those tiers only after
//! the user has trusted exactly that configuration.
//!
//! The CLI reads the project tier from the working directory and the local tier from the git top
//! level, and memory files from the directories in between, so discovery walks from the root up to
//! the git top level (never above `$HOME` when the root is below it) and, in each directory, looks
//! at `.claude/` (all of it), `.mcp.json`, `CLAUDE.md` and `CLAUDE.local.md`.
//!
//! The fingerprint covers all of it by content: every file's bytes, every symlink's target as
//! written (never followed) and every directory's name. Anything that could not be hashed -- a file
//! over the size limit, unreadable, a FIFO, a socket, a device, a configuration file that is a link,
//! or a walk over its budget -- leaves a gap in the fingerprint, so such a discovery is never
//! `fully_hashed` and no answer about it may outlive the start it was asked for.
//!
//! Every byte read from the disk here goes through [`nofollow`], which opens nothing through a link
//! and never blocks on a FIFO.

mod discover;
mod findings;
pub mod git_top;
pub mod nofollow;
mod store;

use std::path::{Path, PathBuf};

pub use discover::discover;
pub use findings::{mcp_findings, settings_findings};
pub use git_top::{git_top_level, GitWalk};
pub use store::{civil_date, for_root, Diff, Remember, TrustState, TrustStore};

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
    pub root: PathBuf,
    /// The git top level (never above `$HOME` when the root is below it); without one, the highest
    /// directory whose `.git` could not be checked, since git may take that one; else the root.
    pub top: PathBuf,
    /// The root, its parent, ... up to and including `top`.
    pub walked: Vec<PathBuf>,
    /// Sorted by the bytes of `path`; empty exactly when there is nothing to trust.
    pub entries: Vec<Entry>,
    pub findings: Vec<Finding>,
    pub fingerprint: Fingerprint,
    pub over_budget: Option<String>,
}

impl Discovery {
    /// Nothing went unhashed: no entry that could not be read, no configuration file that is a
    /// link, and the walk stayed within its budget. Only then can an answer about this
    /// configuration be kept beyond one start.
    pub fn fully_hashed(&self) -> bool {
        self.over_budget.is_none()
            && !self
                .entries
                .iter()
                .any(|entry| matches!(entry.kind, EntryKind::Unreadable { .. } | EntryKind::Other))
            && !self
                .findings
                .iter()
                .any(|finding| matches!(finding, Finding::Unreadable { .. }))
    }

    /// The entries that could not be checked, each with its reason.
    pub fn uncheckable(&self) -> Vec<(&Path, &str)> {
        self.findings
            .iter()
            .filter_map(|finding| match finding {
                Finding::Unreadable { path, reason } => Some((path.as_path(), reason.as_str())),
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
    },
    EnvKey {
        file: PathBuf,
        key: String,
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
    Unparsed {
        file: PathBuf,
        reason: String,
    },
    ClaudeMd {
        path: PathBuf,
    },
    Symlink {
        path: PathBuf,
        target: PathBuf,
        outside: bool,
    },
    /// Files under a `.claude/` directory that are not its settings or memory files.
    OtherFiles {
        dir: PathBuf,
        count: usize,
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
