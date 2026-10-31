//! The walk over a root's configuration, and its fingerprint.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::codex::{
    codex_findings, hooks_json_findings, project_fallback_names, project_server_definitions, rule_file_findings,
    ServerDefinition,
};
use super::findings::{frontmatter_keys, mcp_findings, settings_findings};
use super::git_top::git_top_level;
use super::nofollow::{follow_within, normalize_lexically, DirHandle, FileKind, Followed, Owner, Reached, Refusal};
use super::{Discovery, Entry, EntryKind, Finding, Fingerprint, Limits, Profile, ProfileEnv};

/// What a file found on the walk is to the agent, which decides how it is parsed and whether a link
/// in its place leaves the agent reading something the fingerprint cannot see.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Settings,
    Mcp,
    Memory,
    /// Codex's `config.toml` in a `.codex/`.
    CodexConfig,
    /// Codex's `hooks.json` in a `.codex/`.
    HooksJson,
    /// A Codex exec-policy rule file, `.codex/rules/*.rules`.
    Rules,
    /// A plugin-marketplace listing Codex reads at the top level.
    Listing,
    /// A directory in a `.codex/` that Codex lists and reads from itself (`rules/`, `skills/`): a
    /// link in its place is followed, and what it leads to is never hashed.
    CodexReadDir,
    /// Anything under a directory an agent registered by name reads whole. Eitri cannot know which
    /// of these files the agent reads or how, so every one counts as read by the agent itself: a link
    /// anywhere here leaves it reading something the fingerprint cannot see.
    Registered,
    /// Anything in a skill tree Codex reads (`.agents/skills/`, `.codex/skills/`): Codex follows a
    /// link anywhere in it and reads what it leads to, so a link here is one the fingerprint cannot
    /// see through, unless it is in `.codex/skills/` and the walk read through it.
    SkillTree,
    /// Anything in a `.claude/` that is not its settings or memory file: commands, agents, skills,
    /// the scripts its hooks run. The CLI reads all of it and follows a link anywhere in it, so a
    /// link here is hashed through to what it reaches, or leaves the CLI reading something the
    /// fingerprint cannot see.
    ClaudeOther,
    /// Anything else under a directory read whole.
    Plain,
}

impl Role {
    /// The agent reads this file itself, so a link here that the walk did not follow means it reads
    /// the link's target, whose content the fingerprint never sees.
    fn read_by_the_cli(self) -> bool {
        !matches!(self, Role::Plain)
    }

    /// A file the prompt names by itself, with its size, rather than by what it configures.
    fn named_by_itself(self) -> bool {
        matches!(
            self,
            Role::Plain | Role::CodexReadDir | Role::Registered | Role::SkillTree | Role::ClaudeOther
        )
    }
}

/// Which directory a tree read whole is, which decides what each name in it means.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tree {
    /// A project's `.claude/`.
    Claude,
    /// A project's `.codex/`.
    Codex,
    /// `.agents/skills/`: each directory directly in it is a skill.
    AgentsSkills,
    /// `.agents/plugins/`: its marketplace listings are named when it is at the top level.
    AgentsPlugins { at_top: bool },
    /// A directory an agent registered by name reads.
    Registered,
    /// A `.claude/rules/` above the git top level: every file in it is memory the CLI loads.
    ClaudeRules,
}

/// The listings Codex reads from `.agents/plugins/` at the repository's top level.
const AGENTS_PLUGIN_LISTINGS: [&str; 2] = ["marketplace.json", "api_marketplace.json"];

/// The listings Codex reads from other tools' plugin directories at the repository's top level.
const OTHER_PLUGIN_LISTINGS: [&str; 2] = [".claude-plugin", ".cursor-plugin"];

impl Tree {
    /// What a file `name` at `depth` below the tree's root, in the directory named `parent` (`None`
    /// at the root), means to the agent. `in_skills` says the directory is in `.codex/skills/`.
    fn role(self, depth: usize, parent: Option<&OsStr>, name: &OsStr, in_skills: bool) -> Role {
        match self {
            Tree::Codex if in_skills => Role::SkillTree,
            Tree::Claude => role_in(depth, name),
            Tree::Codex => match (depth, name.as_bytes()) {
                (0, b"config.toml") => Role::CodexConfig,
                (0, b"hooks.json") => Role::HooksJson,
                (0, b"rules") | (0, b"skills") => Role::CodexReadDir,
                (1, file) if parent == Some(OsStr::new("rules")) && file.ends_with(b".rules") => Role::Rules,
                _ => Role::Plain,
            },
            Tree::AgentsPlugins { at_top: true }
                if depth == 0 && AGENTS_PLUGIN_LISTINGS.iter().any(|l| l.as_bytes() == name.as_bytes()) =>
            {
                Role::Listing
            }
            Tree::AgentsSkills => Role::SkillTree,
            Tree::AgentsPlugins { .. } => Role::Plain,
            Tree::Registered => Role::Registered,
            Tree::ClaudeRules => Role::ClaudeOther,
        }
    }

    /// Whether a directory at `depth`, in the directory named `parent`, is a skill.
    fn is_skill(self, depth: usize, parent: Option<&OsStr>) -> bool {
        match self {
            Tree::AgentsSkills => depth == 0,
            Tree::Codex => depth == 1 && parent == Some(OsStr::new("skills")),
            _ => false,
        }
    }
}

/// Where Claude Code keeps whole git checkouts of its own worktrees, directly under `.claude/`. The
/// CLI loads nothing from them for a root outside them (no memory, skill, agent, command, hook or
/// server, not even after reading a file there), and a root inside one has its own
/// git top level and is discovered there; walking them would put thousands of unrelated files over
/// the budget of every repository that has some, so each start would ask again.
const WORKTREES_DIR: &[u8] = b"worktrees";
const WORKTREES_REASON: &str = "Claude Code's own worktree checkouts, not read";

/// One directory waiting to be walked in [`Scan::tree`].
struct Walked {
    dir: DirHandle,
    /// Where the agent reads it, relative to the top level; through a link this is not where it is.
    path: PathBuf,
    depth: usize,
    /// Its own name, which gives the files in it their role.
    name: Option<OsString>,
    /// Where it really is, relative to the top level, for a tree that follows links.
    real: PathBuf,
    /// Where each link on the way here was, by where it really is: a link leading back to one of
    /// these, or to a directory above one, would walk the same directories for ever.
    crossed: Vec<PathBuf>,
}

/// How a file was reached, when it was through a link.
struct Through<'a> {
    /// The link's target as written.
    target: &'a Path,
    /// The directory the file really is in, absolute: a link swapped into its place leads from there.
    dir: PathBuf,
}

struct Scan<'a> {
    top: PathBuf,
    /// Links below a walked `.claude/`, in a walked `.codex/skills/` and in place of a memory file in
    /// a walked directory may be followed inside the top level. Off when the top level may hold the
    /// user's home, since a link could then lead into the account's own files, and when it is neither
    /// the root nor where a `.git` was accepted, since it may then be a shared directory the project
    /// does not own.
    follows_links: bool,
    /// The links named so far, by the path each was named at.
    named: BTreeSet<PathBuf>,
    /// Set while the walk is above the top level, where a directory such as `/tmp` may hold another
    /// user's file: such a file is a gap (that user can rewrite it after it was shown), not trust.
    above_top: bool,
    limits: &'a Limits,
    entries: Vec<Entry>,
    findings: Vec<Finding>,
    over_budget: Vec<String>,
    total_bytes: u64,
    /// The MCP servers each project `.codex/config.toml` read defines, with their definitions.
    project_servers: Vec<(String, ServerDefinition)>,
    /// The alternate names of `AGENTS.md` each project `.codex/config.toml` read gives.
    project_fallback_names: Vec<String>,
}

/// A size as a person reads it: `4 MiB`, `16 KiB`, `100 bytes`.
fn size(bytes: u64) -> String {
    if bytes >= 1 << 20 && bytes.is_multiple_of(1 << 20) {
        format!("{} MiB", bytes >> 20)
    } else if bytes >= 1 << 10 && bytes.is_multiple_of(1 << 10) {
        format!("{} KiB", bytes >> 10)
    } else {
        format!("{bytes} bytes")
    }
}

/// `path` relative to `top`, with leading `..` when `path` is not below `top` (a git directory that
/// a gitfile names somewhere else). The empty path is `.`.
fn relative(top: &Path, path: &Path) -> PathBuf {
    for (ups, ancestor) in top.ancestors().enumerate() {
        if let Ok(rest) = path.strip_prefix(ancestor) {
            let mut out = PathBuf::new();
            for _ in 0..ups {
                out.push("..");
            }
            out.push(rest);
            if out.as_os_str().is_empty() {
                out.push(".");
            }
            return out;
        }
    }
    path.to_path_buf()
}

impl Scan<'_> {
    fn note_over_budget(&mut self, reason: String) {
        if !self.over_budget.contains(&reason) {
            self.over_budget.push(reason);
        }
    }

    /// Room for one more entry; once full, discovery says so and records nothing more.
    fn room(&mut self) -> bool {
        if self.entries.len() < self.limits.max_entries {
            return true;
        }
        let reason = format!("more than {} entries of configuration", self.limits.max_entries);
        self.note_over_budget(reason);
        false
    }

    fn push(&mut self, path: PathBuf, kind: EntryKind) {
        if let EntryKind::Unreadable { reason } = &kind {
            self.findings.push(Finding::Unreadable {
                path: path.clone(),
                reason: reason.clone(),
            });
        }
        if let EntryKind::Other = &kind {
            self.findings.push(Finding::Unreadable {
                path: path.clone(),
                reason: "neither a file, a directory nor a link".to_string(),
            });
        }
        self.entries.push(Entry { path, kind });
    }

    fn unreadable(&mut self, path: PathBuf, reason: String) {
        self.push(path, EntryKind::Unreadable { reason });
    }

    fn symlink(&mut self, dir: &DirHandle, name: &OsStr, path: PathBuf, role: Role) {
        // Above the top level `path` starts with `..`, which is folded here so a link's target is
        // judged from the directory it is really in.
        let parent = path.parent().unwrap_or(Path::new(""));
        let link_dir = normalize_lexically(&self.top, parent).unwrap_or_else(|| self.top.clone());
        self.symlink_in(dir, name, path, role, &link_dir, false);
    }

    /// [`Scan::symlink`] for a link that is really in `link_dir`, which is not where `path` says when
    /// the agent reaches it through another link. `left` says the way it leads was walked and leaves
    /// the top level, whatever its own text says.
    fn symlink_in(&mut self, dir: &DirHandle, name: &OsStr, path: PathBuf, role: Role, link_dir: &Path, left: bool) {
        let target = match dir.readlink(name) {
            Ok(target) => target,
            Err(refusal) => {
                self.unreadable(path, format!("a link that cannot be read: {}", refusal.describe()));
                return;
            }
        };
        let outside =
            left || normalize_lexically(link_dir, &target).is_none_or(|resolved| !resolved.starts_with(&self.top));
        self.findings.push(Finding::Symlink {
            path: path.clone(),
            target: target.clone(),
            outside,
        });
        if role.read_by_the_cli() {
            self.findings.push(Finding::Unreadable {
                path: path.clone(),
                reason: format!("a link to {}", target.display()),
            });
        }
        self.push(path, EntryKind::Symlink { target, outside });
    }

    /// A link followed to something inside the top level, named once with its target as written.
    /// Returns whether this was the first time.
    fn followed_link(&mut self, path: &Path, target: &Path) -> bool {
        let first = self.named.insert(path.to_path_buf());
        if first {
            self.findings.push(Finding::Symlink {
                path: path.to_path_buf(),
                target: target.to_path_buf(),
                outside: false,
            });
        }
        first
    }

    /// The further links a followed link passed through, each named once and where it really is: their
    /// targets as written are part of what the agent reads, like the first link's.
    fn chain(&mut self, links: Vec<(PathBuf, PathBuf)>) {
        for (path, target) in links {
            if !self.room() {
                return;
            }
            if self.followed_link(&path, &target) {
                self.push(path, EntryKind::Symlink { target, outside: false });
            }
        }
    }

    /// The link `name` in `dir`, which is really at `real` relative to the top level, walked to its
    /// end, with its target as written.
    fn follow(&self, dir: &DirHandle, name: &OsStr, real: &Path) -> Option<(PathBuf, Followed)> {
        let target = dir.readlink(name).ok()?;
        let followed = follow_within(&self.top, real, &target)?;
        Some((target, followed))
    }

    /// What [`Scan::follow`] found for a link at `path`, when it is not a directory to walk: a regular
    /// file is read under the link's own path, and anything else stays a link the walk did not follow.
    fn file_behind_link(
        &mut self,
        dir: &DirHandle,
        name: &OsStr,
        path: PathBuf,
        role: Role,
        real: &Path,
        followed: Option<(PathBuf, Followed)>,
    ) {
        match followed {
            Some((
                target,
                Followed {
                    reached:
                        Reached::File {
                            dir: holder,
                            name: file,
                            at,
                        },
                    links,
                },
            )) => {
                self.followed_link(&path, &target);
                self.chain(links);
                // The links on the way may have taken the room this link was let in with.
                if !self.room() {
                    return;
                }
                let through = Through {
                    target: &target,
                    dir: self.top.join(at),
                };
                self.read_file(&holder, &file, path, role, Some(through));
            }
            other => {
                let left = matches!(
                    other,
                    Some((
                        _,
                        Followed {
                            reached: Reached::Left,
                            ..
                        }
                    ))
                );
                let link_dir = self.top.join(real);
                self.symlink_in(dir, name, path, role, &link_dir, left);
            }
        }
    }

    /// Reads, hashes and parses a regular file. Returns whether it was a regular file that was
    /// hashed.
    fn file(&mut self, dir: &DirHandle, name: &OsStr, path: PathBuf, role: Role) -> bool {
        self.read_file(dir, name, path, role, None)
    }

    /// [`Scan::file`], for the agent reading it at `path`; `link` says how it was reached when that was
    /// through a link, whose target as written is then hashed with it.
    fn read_file(&mut self, dir: &DirHandle, name: &OsStr, path: PathBuf, role: Role, link: Option<Through>) -> bool {
        let remaining = self.limits.max_total_bytes.saturating_sub(self.total_bytes);
        let allowed = self.limits.max_file_bytes.min(remaining);
        let owner = if self.above_top { Owner::MeOrRoot } else { Owner::Anyone };
        match dir.read_file_as(name, allowed, owner) {
            Ok(bytes) => {
                self.total_bytes += bytes.len() as u64;
                let sha256: [u8; 32] = Sha256::digest(&bytes).into();
                match role {
                    Role::Settings => self.findings.extend(settings_findings(&path, &bytes)),
                    Role::Mcp => self.findings.extend(mcp_findings(&path, &bytes)),
                    Role::Memory => self.findings.push(Finding::Memory { path: path.clone() }),
                    Role::CodexConfig => {
                        self.findings.extend(codex_findings(&path, &bytes));
                        self.project_servers.extend(project_server_definitions(&bytes));
                        for name in project_fallback_names(&bytes) {
                            if !self.project_fallback_names.contains(&name) {
                                self.project_fallback_names.push(name);
                            }
                        }
                    }
                    Role::HooksJson => self.findings.extend(hooks_json_findings(&path, &bytes)),
                    Role::Rules => self.findings.extend(rule_file_findings(&path, &bytes)),
                    Role::Listing => self.findings.push(Finding::PluginListing { path: path.clone() }),
                    Role::CodexReadDir | Role::Registered | Role::SkillTree | Role::ClaudeOther | Role::Plain => {}
                }
                if role.named_by_itself() {
                    // The frontmatter is read from the bytes just hashed, so it is the same content.
                    let name = path.file_name().unwrap_or(path.as_os_str());
                    self.findings.push(Finding::OtherFile {
                        dir: path.parent().map(Path::to_path_buf).unwrap_or_default(),
                        name: PathBuf::from(name),
                        len: bytes.len() as u64,
                        frontmatter: frontmatter_keys(name, &bytes),
                    });
                }
                let len = bytes.len() as u64;
                let kind = match link {
                    Some(through) => EntryKind::LinkedFile {
                        target: through.target.to_path_buf(),
                        sha256,
                        len,
                    },
                    None => EntryKind::File { sha256, len },
                };
                self.push(path, kind);
                true
            }
            Err(Refusal::TooLarge(len)) if len > self.limits.max_file_bytes => {
                let limit = size(self.limits.max_file_bytes);
                self.note_over_budget(format!("{} is over {limit}", path.display()));
                self.unreadable(path, format!("over {limit}"));
                false
            }
            Err(Refusal::TooLarge(_)) => {
                let limit = size(self.limits.max_total_bytes);
                self.note_over_budget(format!("more than {limit} of configuration"));
                self.unreadable(path, format!("over the {limit} total"));
                false
            }
            Err(Refusal::Symlink { .. }) => {
                // Swapped for a link between the look and the read.
                match link {
                    Some(through) => self.symlink_in(dir, name, path, role, &through.dir, false),
                    None => self.symlink(dir, name, path, role),
                }
                false
            }
            Err(refusal) => {
                self.unreadable(path, refusal.describe());
                false
            }
        }
    }

    /// One name looked at where a file is expected (beside `.claude/`, or its settings at `$HOME`).
    /// Returns whether it was a regular file that was hashed.
    fn expected_file(&mut self, dir: &DirHandle, name: &OsStr, path: PathBuf, role: Role) -> bool {
        match dir.lstat(name) {
            Err(Refusal::Missing) => false,
            Err(refusal) => {
                if self.room() {
                    self.unreadable(path, refusal.describe());
                }
                false
            }
            Ok(kind) => {
                if !self.room() {
                    return false;
                }
                match kind {
                    FileKind::Regular { .. } => self.file(dir, name, path, role),
                    FileKind::Symlink => {
                        self.symlink(dir, name, path, role);
                        false
                    }
                    FileKind::Unknown | FileKind::Dir => {
                        self.push(path, EntryKind::Other);
                        false
                    }
                    other => {
                        self.unreadable(path, other.describe().to_string());
                        false
                    }
                }
            }
        }
    }

    /// `dir` relative to the top level (empty for the top itself), and the directory opened. A
    /// directory that cannot be opened is an entry that cannot be checked.
    fn open_walked(&mut self, dir: &Path) -> Option<(PathBuf, DirHandle)> {
        let rel = relative(&self.top, dir);
        let base = if rel == Path::new(".") { PathBuf::new() } else { rel };
        match DirHandle::open_absolute(dir) {
            Ok(handle) => Some((base, handle)),
            Err(refusal) => {
                if self.room() {
                    let path = if base.as_os_str().is_empty() {
                        PathBuf::from(".")
                    } else {
                        base
                    };
                    self.unreadable(path, format!("the directory cannot be opened: {}", refusal.describe()));
                }
                None
            }
        }
    }

    /// Names an agent reads that discovery cannot look up as one name: each is an entry that cannot be
    /// checked, so no answer about this configuration outlives one start.
    fn refused_names(&mut self, refused: &[String], what: &str) {
        for name in refused {
            if self.room() {
                self.unreadable(
                    PathBuf::from(name),
                    format!("{what} that is not a plain file name, not read"),
                );
            }
        }
    }

    /// A memory file in a walked directory, which is `base` relative to the top level. A link here is
    /// often `CLAUDE.md -> AGENTS.md`, so it is read through like one below `.claude/`.
    fn walked_memory_file(&mut self, dir: &DirHandle, name: &OsStr, base: &Path) {
        if !self.follows_links || !matches!(dir.lstat(name), Ok(FileKind::Symlink)) {
            self.expected_file(dir, name, base.join(name), Role::Memory);
        } else if self.room() {
            let followed = self.follow(dir, name, base);
            self.file_behind_link(dir, name, base.join(name), Role::Memory, base, followed);
        }
    }

    /// One directory on the walk for Claude: the files beside `.claude/`, then `.claude/` itself.
    fn claude_directory(&mut self, dir: &Path, at_home: bool) {
        let Some((base, handle)) = self.open_walked(dir) else {
            return;
        };
        self.expected_file(&handle, OsStr::new(".mcp.json"), base.join(".mcp.json"), Role::Mcp);
        for name in ["CLAUDE.md", "CLAUDE.local.md"] {
            self.walked_memory_file(&handle, OsStr::new(name), &base);
        }
        let dot_claude = OsStr::new(".claude");
        let claude_path = base.join(".claude");
        match handle.lstat(dot_claude) {
            Err(Refusal::Missing) => {}
            Err(refusal) => {
                if self.room() {
                    self.unreadable(claude_path, refusal.describe());
                }
            }
            Ok(FileKind::Dir) => match handle.open_dir(dot_claude) {
                Ok(claude) if at_home => {
                    // The user tier lives here; only the local settings belong to the project.
                    let name = OsStr::new("settings.local.json");
                    self.expected_file(&claude, name, claude_path.join(name), Role::Settings);
                }
                Ok(claude) => self.tree(claude, claude_path, Tree::Claude),
                Err(Refusal::Symlink { .. }) => {
                    if self.room() {
                        self.symlink(&handle, dot_claude, claude_path, Role::Settings);
                    }
                }
                Err(refusal) => {
                    if self.room() {
                        self.unreadable(claude_path, format!("cannot be opened: {}", refusal.describe()));
                    }
                }
            },
            Ok(FileKind::Symlink) => {
                // The CLI would follow it to its settings.
                if self.room() {
                    self.symlink(&handle, dot_claude, claude_path, Role::Settings);
                }
            }
            Ok(FileKind::Regular { .. }) | Ok(FileKind::Unknown) => {
                if self.room() {
                    self.push(claude_path, EntryKind::Other);
                }
            }
            Ok(other) => {
                if self.room() {
                    self.unreadable(claude_path, other.describe().to_string());
                }
            }
        }
    }

    /// One directory above the git top level for Claude. The CLI walks from its working directory up
    /// to `/` for memory files and `.mcp.json`, so a trusted session loads these from every directory
    /// above the repository too (measured: `CLAUDE.md`, `CLAUDE.local.md`, `.claude/CLAUDE.md`,
    /// `.claude/rules/` and an `.mcp.json` server; not the settings files, skills, agents or commands
    /// of a `.claude/` there). At `$HOME` the `.claude/` is left out as the user tier's own; under an
    /// Eitri account the session's configuration directory is `~/.claude-<name>` instead, and whether
    /// the CLI then loads `~/.claude/CLAUDE.md` as a parent's memory is not measured (the user's own
    /// files either way). A file here owned by neither this user nor root is a gap (see `above_top`).
    fn claude_above_top(&mut self, dir: &Path, at_home: bool) {
        let Some((base, handle)) = self.open_walked(dir) else {
            return;
        };
        for (name, role) in [
            (".mcp.json", Role::Mcp),
            ("CLAUDE.md", Role::Memory),
            ("CLAUDE.local.md", Role::Memory),
        ] {
            self.expected_file(&handle, OsStr::new(name), base.join(name), role);
        }
        if at_home {
            return;
        }
        let dot_claude = OsStr::new(".claude");
        let claude_path = base.join(".claude");
        if let Some(claude) = self.pass_through(&handle, dot_claude, claude_path.clone()) {
            let memory = OsStr::new("CLAUDE.md");
            self.expected_file(&claude, memory, claude_path.join(memory), Role::Memory);
            let rules = OsStr::new("rules");
            self.whole_dir(&claude, rules, claude_path.join(rules), Tree::ClaudeRules);
        }
    }

    /// Whether a directory one below `depth` is past the walk's depth, noting it in the budget if so.
    fn too_deep(&mut self, depth: usize, tree: Tree, top_path: &Path) -> bool {
        if depth < self.limits.max_depth {
            return false;
        }
        // Keep Claude's wording stable: it is part of the record's fingerprint.
        let under = match tree {
            Tree::Claude => ".claude/".to_string(),
            _ => format!("{}/", top_path.display()),
        };
        self.note_over_budget(format!(
            "deeper than {} directories under {under}",
            self.limits.max_depth
        ));
        true
    }

    /// Everything under a directory read whole (a `.claude/`, a `.codex/`, ...), depth first,
    /// bounded by depth and entries.
    fn tree(&mut self, top_dir: DirHandle, top_path: PathBuf, tree: Tree) {
        // A main checkout's `.codex/` is reached from above the top level, where nothing is followed.
        let follows_links = self.follows_links && !top_path.starts_with("..");
        let mut stack = vec![Walked {
            dir: top_dir,
            path: top_path.clone(),
            depth: 0,
            name: None,
            real: top_path.clone(),
            crossed: Vec::new(),
        }];
        while let Some(walked) = stack.pop() {
            let Walked {
                dir,
                path,
                depth,
                name: parent,
                real,
                crossed,
            } = walked;
            let room = self.limits.max_entries.saturating_sub(self.entries.len());
            let mut names = match dir.entries(room.saturating_add(1)) {
                Ok(names) => names,
                Err(refusal) => {
                    if self.room() {
                        self.unreadable(path, format!("cannot be listed: {}", refusal.describe()));
                    }
                    continue;
                }
            };
            if names.len() > room {
                names.truncate(room);
                let reason = format!("more than {} entries of configuration", self.limits.max_entries);
                self.note_over_budget(reason);
            }
            let mut below: Vec<Walked> = Vec::new();
            let in_skills = depth > 0
                && path
                    .strip_prefix(&top_path)
                    .ok()
                    .and_then(|rest| rest.components().next())
                    .is_some_and(|first| first.as_os_str() == "skills");
            for name in names {
                let child = path.join(&name);
                let role = tree.role(depth, parent.as_deref(), &name, in_skills);
                match dir.lstat(&name) {
                    Ok(FileKind::Dir) => {
                        if !self.room() {
                            break;
                        }
                        self.push(child.clone(), EntryKind::Dir);
                        if tree == Tree::Claude && depth == 0 && name.as_bytes() == WORKTREES_DIR {
                            self.findings.push(Finding::NotRead {
                                path: child,
                                reason: WORKTREES_REASON.to_string(),
                            });
                            continue;
                        }
                        if tree.is_skill(depth, parent.as_deref()) {
                            self.findings.push(Finding::Skill {
                                path: child.clone(),
                                name: name.to_string_lossy().into_owned(),
                            });
                        }
                        if self.too_deep(depth, tree, &top_path) {
                            continue;
                        }
                        match dir.open_dir(&name) {
                            Ok(sub) => below.push(Walked {
                                dir: sub,
                                path: child,
                                depth: depth + 1,
                                real: real.join(&name),
                                name: Some(name),
                                crossed: crossed.clone(),
                            }),
                            Err(refusal) => {
                                // Not listed, so it cannot count as hashed.
                                self.entries.pop();
                                self.unreadable(child, format!("cannot be opened: {}", refusal.describe()));
                            }
                        }
                    }
                    Ok(FileKind::Symlink)
                        if follows_links
                            && match tree {
                                Tree::Claude => !(depth == 0 && name.as_bytes() == WORKTREES_DIR),
                                Tree::Codex => in_skills,
                                _ => false,
                            } =>
                    {
                        if !self.room() {
                            break;
                        }
                        match self.follow(&dir, &name, &real) {
                            // A directory holding this link, or one on the way here, would lead back.
                            Some((
                                target,
                                Followed {
                                    reached: Reached::Dir { dir: sub, at },
                                    links,
                                },
                            )) if !real.starts_with(&at) && !crossed.iter().any(|at_link| at_link.starts_with(&at)) => {
                                self.followed_link(&child, &target);
                                self.chain(links);
                                if !self.room() {
                                    break;
                                }
                                self.push(child.clone(), EntryKind::Symlink { target, outside: false });
                                if tree.is_skill(depth, parent.as_deref()) {
                                    self.findings.push(Finding::Skill {
                                        path: child.clone(),
                                        name: name.to_string_lossy().into_owned(),
                                    });
                                }
                                if self.too_deep(depth, tree, &top_path) {
                                    continue;
                                }
                                let mut crossed = crossed.clone();
                                crossed.push(real.clone());
                                below.push(Walked {
                                    dir: sub,
                                    path: child,
                                    depth: depth + 1,
                                    real: at,
                                    name: Some(name),
                                    crossed,
                                });
                            }
                            other => self.file_behind_link(&dir, &name, child, role, &real, other),
                        }
                    }
                    _ => {
                        self.expected_file(&dir, &name, child, role);
                    }
                }
            }
            // Pushed in reverse so the first name is walked first.
            stack.extend(below.into_iter().rev());
        }
    }

    /// A directory an agent reads whole, by name in `parent`: walked when it is one, a link to it
    /// reported as one the agent would follow, anything else in its place named.
    fn whole_dir(&mut self, parent: &DirHandle, name: &OsStr, path: PathBuf, tree: Tree) {
        match parent.lstat(name) {
            Err(Refusal::Missing) => {}
            Err(refusal) => {
                if self.room() {
                    self.unreadable(path, refusal.describe());
                }
            }
            // Like `.claude/` itself, the directory is not an entry of its own: an empty one is nothing
            // to trust.
            Ok(FileKind::Dir) => match parent.open_dir(name) {
                Ok(handle) => self.tree(handle, path, tree),
                Err(Refusal::Symlink { .. }) => {
                    if self.room() {
                        self.symlink(parent, name, path, Role::Settings);
                    }
                }
                Err(refusal) => {
                    if self.room() {
                        self.unreadable(path, format!("cannot be opened: {}", refusal.describe()));
                    }
                }
            },
            Ok(FileKind::Symlink) => {
                if self.room() {
                    self.symlink(parent, name, path, Role::Settings);
                }
            }
            Ok(FileKind::Regular { .. }) | Ok(FileKind::Unknown) => {
                if self.room() {
                    self.push(path, EntryKind::Other);
                }
            }
            Ok(other) => {
                if self.room() {
                    self.unreadable(path, other.describe().to_string());
                }
            }
        }
    }

    /// A directory that must be passed through to reach what an agent reads below it (`.agents/`,
    /// `.claude-plugin/`): opened without a link, and a link or anything else in its place reported
    /// as something the agent would follow or cannot read, except a regular file (or a kind `lstat`
    /// does not name), which has nothing below it to read. Nothing is recorded for it when it is a
    /// plain directory.
    fn pass_through(&mut self, parent: &DirHandle, name: &OsStr, path: PathBuf) -> Option<DirHandle> {
        match parent.lstat(name) {
            Err(Refusal::Missing) => None,
            Err(refusal) => {
                if self.room() {
                    self.unreadable(path, refusal.describe());
                }
                None
            }
            Ok(FileKind::Dir) => match parent.open_dir(name) {
                Ok(handle) => Some(handle),
                Err(Refusal::Symlink { .. }) => {
                    if self.room() {
                        self.symlink(parent, name, path, Role::Settings);
                    }
                    None
                }
                Err(refusal) => {
                    if self.room() {
                        self.unreadable(path, format!("cannot be opened: {}", refusal.describe()));
                    }
                    None
                }
            },
            Ok(FileKind::Symlink) => {
                if self.room() {
                    self.symlink(parent, name, path, Role::Settings);
                }
                None
            }
            Ok(FileKind::Regular { .. }) | Ok(FileKind::Unknown) => None,
            Ok(other) => {
                if self.room() {
                    self.unreadable(path, other.describe().to_string());
                }
                None
            }
        }
    }

    /// A `.codex/` at `dot_codex` (absolute), recorded under `path`: skipped when it is the user's own
    /// `CODEX_HOME`, else read whole.
    fn dot_codex(&mut self, parent: &DirHandle, path: PathBuf, dot_codex: &Path, env: &ProfileEnv) {
        let name = OsStr::new(".codex");
        let Some(codex_home) = env.codex_home.as_deref() else {
            self.whole_dir(parent, name, path, Tree::Codex);
            return;
        };
        if codex_home == dot_codex {
            if matches!(parent.lstat(name), Ok(FileKind::Dir)) && self.room() {
                self.push(path.clone(), EntryKind::Dir);
                self.findings.push(Finding::NotRead {
                    path,
                    reason: CODEX_HOME_REASON.to_string(),
                });
            }
            return;
        }
        // `CODEX_HOME` is canonical, so a `~/.codex` that is a link to it (a dotfile manager's) is
        // spelt differently; Codex compares the resolved paths and skips it too. The link is resolved
        // by its text alone: a target that only matches through further links stays a link the agent
        // would follow, which is the safe side.
        if matches!(parent.lstat(name), Ok(FileKind::Symlink)) {
            let resolved = parent
                .readlink(name)
                .ok()
                .and_then(|target| normalize_lexically(dot_codex.parent()?, &target));
            if resolved.as_deref() == Some(codex_home) {
                self.findings.push(Finding::NotRead {
                    path,
                    reason: CODEX_HOME_REASON.to_string(),
                });
                return;
            }
        }
        self.whole_dir(parent, name, path, Tree::Codex);
    }

    /// One directory on the walk for Codex: its memory files, `.codex/`, `.agents/skills/` and
    /// `.agents/plugins/` (not in `$HOME`, where they are the user's own), and at the top level the
    /// plugin listings of other tools.
    fn codex_directory(&mut self, dir: &Path, at_home: bool, at_top: bool, env: &ProfileEnv, names: &[OsString]) {
        let Some((base, handle)) = self.open_walked(dir) else {
            return;
        };
        for name in ["AGENTS.md", "AGENTS.override.md"] {
            self.expected_file(&handle, OsStr::new(name), base.join(name), Role::Memory);
        }
        for name in names {
            self.expected_file(&handle, name, base.join(name), Role::Memory);
        }
        self.dot_codex(&handle, base.join(".codex"), &dir.join(".codex"), env);
        if !at_home {
            let agents_path = base.join(".agents");
            if let Some(agents) = self.pass_through(&handle, OsStr::new(".agents"), agents_path.clone()) {
                self.whole_dir(
                    &agents,
                    OsStr::new("skills"),
                    agents_path.join("skills"),
                    Tree::AgentsSkills,
                );
                self.whole_dir(
                    &agents,
                    OsStr::new("plugins"),
                    agents_path.join("plugins"),
                    Tree::AgentsPlugins { at_top },
                );
            }
        }
        if at_top {
            for tool in OTHER_PLUGIN_LISTINGS {
                let tool_path = base.join(tool);
                if let Some(tool_dir) = self.pass_through(&handle, OsStr::new(tool), tool_path.clone()) {
                    let listing = OsStr::new("marketplace.json");
                    self.expected_file(&tool_dir, listing, tool_path.join(listing), Role::Listing);
                }
            }
        }
    }

    /// The memory files `names` in a walked directory that was opened before: one that cannot be opened
    /// now was named when it was first tried, or has gone since, and is not named twice.
    fn memory_files(&mut self, dir: &Path, names: &[OsString]) {
        let rel = relative(&self.top, dir);
        let base = if rel == Path::new(".") { PathBuf::new() } else { rel };
        let Ok(handle) = DirHandle::open_absolute(dir) else {
            return;
        };
        for name in names {
            self.expected_file(&handle, name, base.join(name), Role::Memory);
        }
    }

    /// The `.codex/` a linked worktree's walked directory takes its hooks from: the same place in the
    /// main checkout.
    fn main_checkout_codex(&mut self, main_dir: &Path, env: &ProfileEnv) {
        let path = relative(&self.top, &main_dir.join(".codex"));
        let handle = match DirHandle::open_absolute(main_dir) {
            Ok(handle) => handle,
            // Nothing there to read: a directory the worktree has and the main checkout lacks.
            Err(Refusal::Missing) | Err(Refusal::NotADirectory) => return,
            Err(refusal) => {
                if self.room() {
                    self.unreadable(path, format!("the directory cannot be opened: {}", refusal.describe()));
                }
                return;
            }
        };
        self.dot_codex(&handle, path, &main_dir.join(".codex"), env);
    }

    /// One directory on the walk for an agent registered by name: each of its files, and each
    /// directory it named (a name ending in `/`) read whole.
    fn named_directory(&mut self, dir: &Path, files: &[(OsString, bool)]) {
        let Some((base, handle)) = self.open_walked(dir) else {
            return;
        };
        for (name, is_dir) in files {
            if *is_dir {
                self.whole_dir(&handle, name, base.join(name), Tree::Registered);
            } else {
                self.expected_file(&handle, name, base.join(name), Role::Memory);
            }
        }
    }
}

/// Why a linked worktree whose common git directory is not named `.git` cannot be checked.
const UNPLACED_COMMON_REASON: &str =
    "the git directory of this linked worktree is not named .git, so the main checkout whose .codex/ hooks Codex reads cannot be found";

const UNVERIFIED_GIT_REASON: &str =
    "a .git here could not be checked, so the main checkout whose .codex/ hooks Codex reads for a linked worktree cannot be found";

/// Why a `.codex/` that is the user's `CODEX_HOME` is named and not read.
const CODEX_HOME_REASON: &str = "the user's own Codex configuration (CODEX_HOME), not read";

/// Splits names an agent reads in each directory into the plain names discovery can look up there
/// (with whether each is a directory) and a finding for each it cannot: one with a `/` inside, an
/// absolute one, `.` or `..`, which would reach outside the directory or through another one.
fn plain_names(names: &[String], dirs_allowed: bool) -> (Vec<(OsString, bool)>, Vec<String>) {
    let mut plain: Vec<(OsString, bool)> = Vec::new();
    let mut refused = Vec::new();
    for name in names {
        let (bare, is_dir) = match name.strip_suffix('/') {
            Some(bare) if dirs_allowed => (bare, true),
            _ => (name.as_str(), false),
        };
        if bare.is_empty() || bare == "." || bare == ".." || bare.contains('/') || bare.contains('\0') {
            refused.push(name.clone());
            continue;
        }
        let entry = (OsString::from(bare), is_dir);
        if !plain.contains(&entry) {
            plain.push(entry);
        }
    }
    (plain, refused)
}

/// What a name directly in `.claude/` (depth 0) or below it means to the CLI.
fn role_in(depth: usize, name: &OsStr) -> Role {
    if depth != 0 {
        return Role::ClaudeOther;
    }
    match name.as_bytes() {
        b"settings.json" | b"settings.local.json" => Role::Settings,
        b"CLAUDE.md" => Role::Memory,
        _ => Role::ClaudeOther,
    }
}

/// Whether `link`, made for a link on another link's way, says nothing `kept` does not, because the walk
/// met the same link itself. The walked entry is the one kept: it holds what a file link reached, which
/// the record compares path by path.
fn repeats(link: &Entry, kept: &Entry) -> bool {
    link.path == kept.path
        && match (&link.kind, &kept.kind) {
            (EntryKind::Symlink { target, .. }, EntryKind::LinkedFile { target: same, .. }) => target == same,
            (EntryKind::Symlink { .. }, EntryKind::Symlink { .. }) => link.kind == kept.kind,
            _ => false,
        }
}

/// The fingerprint over the profile's header, the top level's place relative to the root, every
/// entry and the budget note. The header and the place (only `..` and `/`) each end in a newline;
/// each entry's path, tag and payload each end in a NUL, which no path, link target, hex digest or
/// tag holds; the budget note comes last. So no two different configurations feed the hash the
/// same bytes.
fn fingerprint(
    profile: &Profile,
    root: &Path,
    top: &Path,
    entries: &[Entry],
    over_budget: Option<&str>,
) -> Fingerprint {
    let mut hash = Sha256::new();
    hash.update(profile.fingerprint_header());
    let ups = root
        .strip_prefix(top)
        .map(|rest| rest.components().count())
        .unwrap_or(0);
    let up = vec![".."; ups].join("/");
    hash.update(up.as_bytes());
    hash.update(b"\n");
    for entry in entries {
        hash.update(entry.path.as_os_str().as_bytes());
        hash.update(b"\0");
        let (tag, payload): (&[u8], Vec<u8>) = match &entry.kind {
            EntryKind::File { sha256, .. } => (b"f", super::hex(sha256).into_bytes()),
            EntryKind::Symlink { target, .. } => (b"l", target.as_os_str().as_bytes().to_vec()),
            EntryKind::LinkedFile { target, sha256, .. } => {
                let mut payload = target.as_os_str().as_bytes().to_vec();
                payload.push(0);
                payload.extend_from_slice(super::hex(sha256).as_bytes());
                (b"t", payload)
            }
            EntryKind::Dir => (b"d", Vec::new()),
            EntryKind::Unreadable { .. } => (b"u", Vec::new()),
            EntryKind::Other => (b"o", Vec::new()),
        };
        hash.update(tag);
        hash.update(b"\0");
        hash.update(&payload);
        hash.update(b"\0");
    }
    if let Some(reason) = over_budget {
        hash.update(reason.as_bytes());
    }
    Fingerprint(hash.finalize().into())
}

/// Discovers the configuration a session of `profile`'s agent started at `root` (canonical) would load
/// from the project: every directory from `root` up to the git top level (or, when no repository was
/// accepted, up to the highest directory whose `.git` could not be checked), never above `home`
/// when `root` is below it. For Claude, every directory above that top level up to `/` is looked at
/// too, for the memory files and `.mcp.json` the CLI loads from there. Nothing blocks, and no link is
/// followed but one below a walked `.claude/`, in a walked `.codex/skills/` or in place of a memory
/// file in a walked directory, by [`follow_within`] and only inside the top level.
///
/// `home` must be canonical too: it is compared with `root`'s ancestors by path, and a home spelt
/// through a link would match none of them, so the walk would neither stop there nor keep the user
/// tier's own files out of the fingerprint. So must `env.codex_home`, for the same reason.
pub fn discover(root: &Path, home: Option<&Path>, limits: &Limits, profile: &Profile, env: &ProfileEnv) -> Discovery {
    let clamp = home.filter(|home| root.starts_with(home));
    let walk = git_top_level(root, clamp, limits.max_walk_up);
    // With no repository accepted, a `.git` the walk could not check may still be the one git takes,
    // and the CLI would read the local tier beside it: walk up to the highest such directory, since
    // reading a directory too many only adds to the question, while stopping at the root would
    // leave that tier out of it.
    let top = walk
        .top
        .clone()
        .or_else(|| walk.unverified_top.clone())
        .unwrap_or_else(|| root.to_path_buf());
    let mut walked = Vec::new();
    for dir in root.ancestors() {
        walked.push(dir.to_path_buf());
        if dir == top {
            break;
        }
    }
    let vouched_for = walk.top.is_some() || top == root;
    let mut scan = Scan {
        top: top.clone(),
        follows_links: vouched_for && home.is_some_and(|home| !home.starts_with(&top)),
        named: BTreeSet::new(),
        above_top: false,
        limits,
        entries: Vec::new(),
        findings: Vec::new(),
        over_budget: Vec::new(),
        total_bytes: 0,
        project_servers: Vec::new(),
        project_fallback_names: Vec::new(),
    };
    for finding in walk.findings {
        if let Finding::GitUnverified { path, reason, target } = finding {
            scan.findings.push(Finding::GitUnverified {
                path: relative(&top, &path),
                reason,
                target,
            });
        }
    }
    if let Some(reason) = walk.over_budget {
        scan.note_over_budget(reason);
    }
    match profile {
        Profile::Claude => {
            for dir in &walked {
                scan.claude_directory(dir, home == Some(dir.as_path()));
            }
            let above: Vec<&Path> = top.ancestors().skip(1).collect();
            if above.len() > limits.max_walk_up {
                scan.note_over_budget(format!(
                    "more than {} directories above the top level",
                    limits.max_walk_up
                ));
            }
            scan.above_top = true;
            for dir in above.into_iter().take(limits.max_walk_up) {
                scan.claude_above_top(dir, home == Some(dir));
            }
        }
        Profile::Codex => {
            let (names, refused) = plain_names(&env.fallback_names, false);
            let names: Vec<OsString> = names
                .into_iter()
                .map(|(name, _)| name)
                .filter(|name| name != "AGENTS.md" && name != "AGENTS.override.md")
                .collect();
            scan.refused_names(&refused, "an alternate name for AGENTS.md");
            // The user's own file may name alternates this discovery then never looked for. Not an
            // entry: the file is the user's, not the project's, and the fingerprint is the project's.
            if let Some((file, why)) = &env.user_config_unreadable {
                scan.findings.push(Finding::Unreadable {
                    path: file.clone(),
                    reason: format!(
                        "your Codex configuration, which may name other files Codex reads as AGENTS.md, cannot be read ({why})"
                    ),
                });
            }
            // A linked worktree whose main checkout this walk cannot place: Codex reads hooks from a
            // `.codex/` there that nothing here can show or hash, so no answer outlives one start.
            if let Some(common) = &walk.unplaced_common {
                if scan.room() {
                    scan.unreadable(relative(&top, common), UNPLACED_COMMON_REASON.to_string());
                }
            }
            let unverified: std::collections::BTreeSet<PathBuf> = scan
                .findings
                .iter()
                .filter_map(|finding| match finding {
                    Finding::GitUnverified { path, .. } => Some(path.clone()),
                    _ => None,
                })
                .collect();
            for path in unverified {
                if scan.room() {
                    scan.unreadable(path, UNVERIFIED_GIT_REASON.to_string());
                }
            }
            // A linked worktree's main checkout, when it is somewhere else than the worktree.
            let main_checkout = walk.main_checkout.as_deref().filter(|main| *main != top.as_path());
            for dir in &walked {
                scan.codex_directory(dir, home == Some(dir.as_path()), *dir == top, env, &names);
                if let Some(main) = main_checkout {
                    if let Ok(rest) = dir.strip_prefix(&top) {
                        scan.main_checkout_codex(&main.join(rest), env);
                    }
                }
            }
            // A project's own configuration can name alternates too, and a trusted session reads them
            // in every directory it reads `AGENTS.md` in. They are known only once each `.codex/` on
            // the walk has been read, so they are looked up in a second pass.
            let (project_names, project_refused) = plain_names(&scan.project_fallback_names, false);
            let project_names: Vec<OsString> = project_names
                .into_iter()
                .map(|(name, _)| name)
                .filter(|name| name != "AGENTS.md" && name != "AGENTS.override.md" && !names.contains(name))
                .collect();
            let project_refused: Vec<String> = project_refused
                .into_iter()
                .filter(|name| !refused.contains(name))
                .collect();
            scan.refused_names(&project_refused, "an alternate name for AGENTS.md");
            if !project_names.is_empty() {
                for dir in &walked {
                    scan.memory_files(dir, &project_names);
                }
            }
        }
        Profile::Acp { files, .. } => {
            let (names, refused) = plain_names(files, true);
            scan.refused_names(&refused, "a project file name that is not one name");
            for dir in &walked {
                scan.named_directory(dir, &names);
            }
        }
    }
    let mut entries = scan.entries;
    // For one path a file reached through a link comes first, so `repeats` meets the two in that order.
    let not_linked_file = |entry: &Entry| !matches!(entry.kind, EntryKind::LinkedFile { .. });
    entries.sort_by(|a, b| {
        let by_path = a.path.as_os_str().as_bytes().cmp(b.path.as_os_str().as_bytes());
        by_path.then(not_linked_file(a).cmp(&not_linked_file(b)))
    });
    entries.dedup_by(|later, earlier| repeats(later, earlier));
    let over_budget = (!scan.over_budget.is_empty()).then(|| scan.over_budget.join("; "));
    let mut findings = scan.findings;
    for reason in &scan.over_budget {
        findings.push(Finding::OverBudget { reason: reason.clone() });
    }
    let fingerprint = fingerprint(profile, root, &top, &entries, over_budget.as_deref());
    Discovery {
        profile: profile.clone(),
        root: root.to_path_buf(),
        top,
        walked,
        entries,
        findings,
        fingerprint,
        over_budget,
        user_mcp_servers: env.user_mcp_servers.clone(),
        project_mcp_servers: scan.project_servers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_go_up_with_dotdot() {
        let top = Path::new("/a/b");
        assert_eq!(relative(top, Path::new("/a/b/c/.git")), PathBuf::from("c/.git"));
        assert_eq!(relative(top, Path::new("/a/b")), PathBuf::from("."));
        assert_eq!(relative(top, Path::new("/a/.git")), PathBuf::from("../.git"));
        assert_eq!(relative(top, Path::new("/.git")), PathBuf::from("../../.git"));
    }

    #[test]
    fn sizes_read_as_a_person_says_them() {
        assert_eq!(size(4 << 20), "4 MiB");
        assert_eq!(size(4096), "4 KiB");
        assert_eq!(size(100), "100 bytes");
    }

    fn scan<'a>(top: &Path, limits: &'a Limits) -> Scan<'a> {
        Scan {
            top: top.to_path_buf(),
            follows_links: true,
            named: BTreeSet::new(),
            above_top: false,
            limits,
            entries: Vec::new(),
            findings: Vec::new(),
            over_budget: Vec::new(),
            total_bytes: 0,
            project_servers: Vec::new(),
            project_fallback_names: Vec::new(),
        }
    }

    /// The file a link reached is swapped between the look and the read: for a link out of the root,
    /// for a pipe. Neither is read, and either leaves the discovery with a gap.
    #[test]
    fn a_target_swapped_between_the_check_and_the_read_is_a_gap() {
        for swap in ["link", "pipe"] {
            let (_s, base) = super::super::nofollow::tests::canonical_scratch("swap");
            let top = base.join("top");
            std::fs::create_dir_all(top.join(".claude/commands")).unwrap();
            std::fs::write(top.join("x.md"), b"inside\n").unwrap();
            std::fs::write(base.join("secret"), b"outside\n").unwrap();
            let target = Path::new("../../x.md");
            let followed = follow_within(&top, Path::new(".claude/commands"), target).unwrap();
            let Reached::File { dir, name, at } = followed.reached else {
                panic!("the file was reached");
            };
            std::fs::remove_file(top.join("x.md")).unwrap();
            match swap {
                "link" => std::os::unix::fs::symlink(base.join("secret"), top.join("x.md")).unwrap(),
                _ => super::super::nofollow::tests::mkfifo(&top.join("x.md")),
            }
            let limits = Limits::default();
            let mut scan = scan(&top, &limits);
            let path = PathBuf::from(".claude/commands/x.md");
            scan.followed_link(&path, target);
            let through = Through {
                target,
                dir: top.join(at),
            };
            assert!(!scan.read_file(&dir, &name, path.clone(), Role::ClaudeOther, Some(through)));
            // Named as leading out from where it really is, though `path` reads as two levels deeper.
            if swap == "link" {
                assert!(scan.findings.contains(&Finding::Symlink {
                    path: path.clone(),
                    target: base.join("secret"),
                    outside: true,
                }));
            }
            assert!(
                scan.findings
                    .iter()
                    .any(|f| matches!(f, Finding::Unreadable { path: at, .. } if *at == path)),
                "{swap}: {:?}",
                scan.findings
            );
            assert!(
                !scan
                    .entries
                    .iter()
                    .any(|entry| matches!(entry.kind, EntryKind::File { .. } | EntryKind::LinkedFile { .. })),
                "{swap}: {:?}",
                scan.entries
            );
        }
    }

    /// A directory the walk has open stays the directory it opened: moved away and replaced by a link
    /// out of the root, it is still listed through the handle and nothing outside is reached.
    #[test]
    fn a_directory_swapped_after_it_was_reached_is_read_through_its_handle() {
        let (_s, base) = super::super::nofollow::tests::canonical_scratch("swap-dir");
        let top = base.join("top");
        std::fs::create_dir_all(top.join(".claude")).unwrap();
        std::fs::create_dir(top.join("real")).unwrap();
        std::fs::write(top.join("real/f.md"), b"inside\n").unwrap();
        std::fs::create_dir(base.join("outside")).unwrap();
        std::fs::write(base.join("outside/secret.md"), b"outside\n").unwrap();
        let followed = follow_within(&top, Path::new(".claude"), Path::new("../real")).unwrap();
        let Reached::Dir { dir, .. } = followed.reached else {
            panic!("the directory was reached");
        };
        std::fs::rename(top.join("real"), top.join("moved")).unwrap();
        std::os::unix::fs::symlink(base.join("outside"), top.join("real")).unwrap();
        assert_eq!(dir.entries(10).unwrap(), vec![OsString::from("f.md")]);
    }

    #[test]
    fn the_fingerprint_separates_fields() {
        let link = |path: &str, target: &str| Entry {
            path: PathBuf::from(path),
            kind: EntryKind::Symlink {
                target: PathBuf::from(target),
                outside: false,
            },
        };
        let file = |path: &str| Entry {
            path: PathBuf::from(path),
            kind: EntryKind::Dir,
        };
        let root = Path::new("/r");
        let claude = &Profile::Claude;
        let a = fingerprint(claude, root, root, &[link("p", "x\ny"), file("z")], None);
        let b = fingerprint(claude, root, root, &[link("p", "x"), file("y\nz")], None);
        assert_ne!(a, b);
        assert_ne!(
            fingerprint(claude, root, root, &[], None),
            fingerprint(claude, Path::new("/r/s"), root, &[], None)
        );
        assert_ne!(
            fingerprint(claude, root, root, &[], None),
            fingerprint(claude, root, root, &[], Some("over"))
        );
        assert_ne!(
            fingerprint(claude, root, root, &[], None),
            fingerprint(&Profile::Codex, root, root, &[], None)
        );
        let linked = |target: &str, sha256: [u8; 32]| Entry {
            path: PathBuf::from("p"),
            kind: EntryKind::LinkedFile {
                target: PathBuf::from(target),
                sha256,
                len: 1,
            },
        };
        let print = |entry: Entry| fingerprint(claude, root, root, &[entry], None);
        let hash = crate::project_trust::hex(&[7; 32]);
        assert_ne!(print(linked("a", [7; 32])), print(linked("b", [7; 32])));
        assert_ne!(print(linked("a", [7; 32])), print(linked("a", [8; 32])));
        assert_ne!(print(linked("a", [7; 32])), print(link("p", &format!("a\0{hash}"))));
        let file = Entry {
            path: PathBuf::from("p"),
            kind: EntryKind::File {
                sha256: [7; 32],
                len: 1,
            },
        };
        assert_ne!(print(linked("a", [7; 32])), print(file));
    }
}
