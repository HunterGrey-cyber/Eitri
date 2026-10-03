//! The walk over a root's configuration, and its fingerprint.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::findings::{mcp_findings, settings_findings};
use super::git_top::git_top_level;
use super::nofollow::{normalize_lexically, DirHandle, FileKind, Refusal};
use super::{Discovery, Entry, EntryKind, Finding, Fingerprint, Limits};

/// What a file found on the walk is to the CLI, which decides how it is parsed and whether a link
/// in its place leaves the CLI reading something the fingerprint cannot see.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Settings,
    Mcp,
    Memory,
    /// Anything else under `.claude/`.
    Plain,
}

impl Role {
    /// The CLI reads this file itself, so a link here means it reads the link's target, whose
    /// content the fingerprint never sees.
    fn read_by_the_cli(self) -> bool {
        !matches!(self, Role::Plain)
    }
}

/// Where Claude Code keeps whole git checkouts of its own worktrees, directly under `.claude/`. The
/// CLI loads no configuration from them for a root outside them, and a root inside one has its own
/// git top level and is discovered there; walking them would put thousands of unrelated files over
/// the budget of every repository that has some, so each start would ask again.
const WORKTREES_DIR: &[u8] = b"worktrees";
const WORKTREES_REASON: &str = "Claude Code's own worktree checkouts, not read";

struct Scan<'a> {
    top: PathBuf,
    limits: &'a Limits,
    entries: Vec<Entry>,
    findings: Vec<Finding>,
    over_budget: Vec<String>,
    total_bytes: u64,
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
        let target = match dir.readlink(name) {
            Ok(target) => target,
            Err(refusal) => {
                self.unreadable(path, format!("a link that cannot be read: {}", refusal.describe()));
                return;
            }
        };
        let link_dir = self.top.join(&path);
        let link_dir = link_dir.parent().unwrap_or(&self.top);
        let outside = normalize_lexically(link_dir, &target).is_none_or(|resolved| !resolved.starts_with(&self.top));
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

    /// Reads, hashes and parses a regular file. Returns whether it was a regular file that was
    /// hashed.
    fn file(&mut self, dir: &DirHandle, name: &OsStr, path: PathBuf, role: Role) -> bool {
        let remaining = self.limits.max_total_bytes.saturating_sub(self.total_bytes);
        let allowed = self.limits.max_file_bytes.min(remaining);
        match dir.read_file(name, allowed) {
            Ok(bytes) => {
                self.total_bytes += bytes.len() as u64;
                let sha256: [u8; 32] = Sha256::digest(&bytes).into();
                match role {
                    Role::Settings => self.findings.extend(settings_findings(&path, &bytes)),
                    Role::Mcp => self.findings.extend(mcp_findings(&path, &bytes)),
                    Role::Memory => self.findings.push(Finding::ClaudeMd { path: path.clone() }),
                    Role::Plain => {}
                }
                self.push(
                    path,
                    EntryKind::File {
                        sha256,
                        len: bytes.len() as u64,
                    },
                );
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
                self.symlink(dir, name, path, role);
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

    /// One directory on the walk: the files beside `.claude/`, then `.claude/` itself.
    fn directory(&mut self, dir: &Path, at_home: bool) {
        let rel = relative(&self.top, dir);
        let base = if rel == Path::new(".") { PathBuf::new() } else { rel };
        let handle = match DirHandle::open_absolute(dir) {
            Ok(handle) => handle,
            Err(refusal) => {
                if self.room() {
                    let path = if base.as_os_str().is_empty() {
                        PathBuf::from(".")
                    } else {
                        base
                    };
                    self.unreadable(path, format!("the directory cannot be opened: {}", refusal.describe()));
                }
                return;
            }
        };
        for (name, role) in [
            (".mcp.json", Role::Mcp),
            ("CLAUDE.md", Role::Memory),
            ("CLAUDE.local.md", Role::Memory),
        ] {
            self.expected_file(&handle, OsStr::new(name), base.join(name), role);
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
                Ok(claude) => self.dot_claude(claude, claude_path),
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

    /// Everything under a `.claude/` directory, depth first, bounded by depth and entries.
    fn dot_claude(&mut self, claude: DirHandle, claude_path: PathBuf) {
        let mut stack = vec![(claude, claude_path, 0usize)];
        while let Some((dir, path, depth)) = stack.pop() {
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
            let mut other_files = 0usize;
            let mut below: Vec<(DirHandle, PathBuf)> = Vec::new();
            for name in names {
                let child = path.join(&name);
                let role = role_in(depth, &name);
                match dir.lstat(&name) {
                    Ok(FileKind::Dir) => {
                        if !self.room() {
                            break;
                        }
                        self.push(child.clone(), EntryKind::Dir);
                        if depth == 0 && name.as_bytes() == WORKTREES_DIR {
                            self.findings.push(Finding::NotRead {
                                path: child,
                                reason: WORKTREES_REASON.to_string(),
                            });
                            continue;
                        }
                        if depth + 1 > self.limits.max_depth {
                            self.note_over_budget(format!(
                                "deeper than {} directories under .claude/",
                                self.limits.max_depth
                            ));
                            continue;
                        }
                        match dir.open_dir(&name) {
                            Ok(sub) => below.push((sub, child)),
                            Err(refusal) => {
                                // Not listed, so it cannot count as hashed.
                                self.entries.pop();
                                self.unreadable(child, format!("cannot be opened: {}", refusal.describe()));
                            }
                        }
                    }
                    _ => {
                        if self.expected_file(&dir, &name, child, role) && role == Role::Plain {
                            other_files += 1;
                        }
                    }
                }
            }
            if other_files > 0 {
                self.findings.push(Finding::OtherFiles {
                    dir: path.clone(),
                    count: other_files,
                });
            }
            // Pushed in reverse so the first name is walked first.
            for (sub, child) in below.into_iter().rev() {
                stack.push((sub, child, depth + 1));
            }
        }
    }
}

/// What a name directly in `.claude/` (depth 0) or below it means to the CLI.
fn role_in(depth: usize, name: &OsString) -> Role {
    if depth != 0 {
        return Role::Plain;
    }
    match name.as_bytes() {
        b"settings.json" | b"settings.local.json" => Role::Settings,
        b"CLAUDE.md" => Role::Memory,
        _ => Role::Plain,
    }
}

/// The fingerprint over the top level's place relative to the root, every entry and the budget
/// note. Each field ends in a NUL, which no path, link target, hex digest or tag holds, so no two
/// different configurations feed the hash the same bytes.
fn fingerprint(root: &Path, top: &Path, entries: &[Entry], over_budget: Option<&str>) -> Fingerprint {
    let mut hash = Sha256::new();
    hash.update(b"eitri-trust-v1\n");
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

/// Discovers the configuration a session started at `root` (canonical) would load from the project
/// and local tiers: every directory from `root` up to the git top level (or, when no repository was
/// accepted, up to the highest directory whose `.git` could not be checked), never above `home`
/// when `root` is below it. No link is followed and nothing blocks.
///
/// `home` must be canonical too: it is compared with `root`'s ancestors by path, and a home spelt
/// through a link would match none of them, so the walk would neither stop there nor keep the user
/// tier's own files out of the fingerprint.
pub fn discover(root: &Path, home: Option<&Path>, limits: &Limits) -> Discovery {
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
    let mut scan = Scan {
        top: top.clone(),
        limits,
        entries: Vec::new(),
        findings: Vec::new(),
        over_budget: Vec::new(),
        total_bytes: 0,
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
    for dir in &walked {
        scan.directory(dir, home == Some(dir.as_path()));
    }
    let mut entries = scan.entries;
    entries.sort_by(|a, b| a.path.as_os_str().as_bytes().cmp(b.path.as_os_str().as_bytes()));
    let over_budget = (!scan.over_budget.is_empty()).then(|| scan.over_budget.join("; "));
    let mut findings = scan.findings;
    for reason in &scan.over_budget {
        findings.push(Finding::OverBudget { reason: reason.clone() });
    }
    let fingerprint = fingerprint(root, &top, &entries, over_budget.as_deref());
    Discovery {
        root: root.to_path_buf(),
        top,
        walked,
        entries,
        findings,
        fingerprint,
        over_budget,
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
        let a = fingerprint(root, root, &[link("p", "x\ny"), file("z")], None);
        let b = fingerprint(root, root, &[link("p", "x"), file("y\nz")], None);
        assert_ne!(a, b);
        assert_ne!(
            fingerprint(root, root, &[], None),
            fingerprint(Path::new("/r/s"), root, &[], None)
        );
        assert_ne!(
            fingerprint(root, root, &[], None),
            fingerprint(root, root, &[], Some("over"))
        );
    }
}
