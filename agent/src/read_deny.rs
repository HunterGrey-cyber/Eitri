//! The `Read(...)` deny rules of the CLI's settings, read so that a source the editor offers never reaches a file the
//! CLI's own `@path` would skip.
//!
//! Only this narrows. The CLI is the one that enforces a rule; nothing here lets a call through that its gate refuses,
//! and nothing consults it for a call. It answers one question for text Eitri itself puts in a turn: *might a deny rule
//! cover this file?* Where it cannot tell it says yes, so a doubt leaves a file out and never puts one in.
//!
//! What the pinned CLI does with each written form was measured on it (CLI 2.1.311, `@path` through the SDK, one message
//! naming four files and a link for each form, and read from the request the model got): a pattern with no slash names a file or a folder at any depth
//! (`*.txt` kept out `sub/deep/leaf.txt`); a folder rule (`sub`, `./sub/`, `sub/*`) covers everything under it; `./x`,
//! `/x` (from the project) and `//abs` (from the filesystem root) are anchored; `**` crosses folders; names are
//! case-sensitive; a bare `Read` covers everything; `Edit(...)` rules do not stop a read; a rule in
//! `settings.local.json` counts as one in `settings.json`; and a file reached through a link is held to the rules for
//! its own name and for where it leads. This reader takes every written form as both anchored and at any depth, so it
//! is never narrower than the CLI.
//!
//! Every tier is read whether or not the session will load it (an untrusted project's rules, a user tier switched
//! off): a rule the CLI does not apply costs the person a file left out of a list, a rule missed costs a file read.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

/// The most rules read: past it the reader is closed, since what judging that many would cost is not a thing a
/// project's own settings get to choose.
const RULES_MAX: usize = 4000;

/// The folder a settings file's relative patterns are about: the project's, for `<project>/.claude/settings.json`.
fn settings_base(origin: &Path) -> Option<PathBuf> {
    let dir = origin.parent()?;
    if dir.file_name().is_some_and(|name| name == ".claude") {
        dir.parent().map(Path::to_path_buf)
    } else {
        Some(dir.to_path_buf())
    }
}

/// One `Read` rule, as the segments of its pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    /// From the filesystem root, or from the project root.
    anchor: Anchor,
    /// The pattern's segments; `**` stands for any number of segments.
    segments: Vec<String>,
    /// How many of them are not `**`: each takes one segment of a path, so a path with fewer has no match. Counted once,
    /// here, since it is asked of every rule for every path.
    fixed: usize,
}

impl Rule {
    fn new(anchor: Anchor, segments: Vec<String>) -> Rule {
        let fixed = segments.iter().filter(|segment| *segment != "**").count();
        Rule {
            anchor,
            segments,
            fixed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Anchor {
    /// The whole path from `/`.
    Absolute,
    /// From the project root.
    Root,
    /// Starting at any segment of the path below the project root.
    Anywhere,
}

/// The deny rules in force for one project.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadRules {
    rules: Vec<Rule>,
    /// A bare `Read` rule: nothing is readable.
    everything: bool,
    /// Set when a settings file is there and could not be read: no file is told of while it holds.
    unreadable: Option<String>,
    /// The bytes of every pattern read so far.
    pattern_bytes: usize,
}

/// The most a settings file may be: a project's own files are read before anyone has been asked about it, so what they
/// may cost has a bound that does not depend on what they are.
const FILE_MAX: u64 = 4 * 1024 * 1024;
/// The most bytes of `Read` patterns, all files together, and of one pattern.
const PATTERN_BYTES_MAX: usize = 256 * 1024;
const PATTERN_MAX: usize = 4096;
/// The most folders above the project looked at for settings.
const ANCESTORS_MAX: usize = 64;

/// The CLI's managed policy files: its Linux place and its macOS place, both read wherever this runs.
const MANAGED_DIRS: [&str; 2] = ["/etc/claude-code", "/Library/Application Support/ClaudeCode"];

/// Why a settings file was not read: the words for a person, and the kind of failure when the system gave one (matched,
/// never its text, which is in the person's language).
struct SettingsError {
    why: String,
    kind: Option<std::io::ErrorKind>,
}

impl SettingsError {
    fn io(error: &std::io::Error) -> SettingsError {
        SettingsError {
            why: error.to_string(),
            kind: Some(error.kind()),
        }
    }

    fn other(why: String) -> SettingsError {
        SettingsError { why, kind: None }
    }
}

/// Whether a file that could not be read closes the reader: one of the project's own or the account's always does, one
/// found in a folder above only when the reason is not that we may not look (the CLI could not read it either).
fn closes(strict: bool, error: &SettingsError) -> bool {
    strict || error.kind != Some(std::io::ErrorKind::PermissionDenied)
}

/// A settings file's bytes, or `None` when nothing is there. Opened without blocking, so a FIFO is not waited for, and
/// looked at on the handle: only a regular file of at most [`FILE_MAX`] bytes is read, and a link to one is followed
/// (a dotfile may be a link) while a link to `/dev/zero` is a device and refused unread.
fn read_settings_file(path: &Path) -> Result<Option<Vec<u8>>, SettingsError> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(SettingsError::io(&error)),
    };
    let meta = file.metadata().map_err(|error| SettingsError::io(&error))?;
    if !meta.is_file() {
        return Err(SettingsError::other("it is not a regular file".to_owned()));
    }
    let too_large = || SettingsError::other(format!("it is larger than {} MiB", FILE_MAX / 1024 / 1024));
    if meta.len() > FILE_MAX {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    file.take(FILE_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| SettingsError::io(&error))?;
    if bytes.len() as u64 > FILE_MAX {
        return Err(too_large());
    }
    Ok(Some(bytes))
}

/// The checkout a linked worktree belongs to: `<dir>/.git` is a file naming the worktree's own git directory, whose
/// `commondir` leads to the repository's, and the checkout is the folder that holds it.
fn main_checkout_of(dir: &Path) -> Option<PathBuf> {
    let link = read_settings_file(&dir.join(".git")).ok()??;
    let text = String::from_utf8(link).ok()?;
    let gitdir = text.lines().find_map(|line| line.strip_prefix("gitdir:"))?.trim();
    let gitdir = dir.join(gitdir);
    let common = read_settings_file(&gitdir.join("commondir")).ok()??;
    let common = gitdir.join(String::from_utf8(common).ok()?.trim());
    let common = std::fs::canonicalize(common).ok()?;
    (common.file_name()? == ".git").then(|| common.parent().map(Path::to_path_buf))?
}

impl ReadRules {
    /// The rules of every tier for a project at `root`, read from the files, each read wherever the CLI may: the
    /// user tier (`config_dir`, the account's: `settings.json`, `settings.local.json`, the server-managed
    /// `remote-settings.json`), the project's `.claude/settings.json` and `settings.local.json` in the project's
    /// folder, in every folder above it (the CLI keeps the local file at the repository's root, and a session opened in
    /// a subfolder is under it) and in the checkout a linked worktree belongs to, and the managed policy files of Linux
    /// and macOS. With no `config_dir` there is no user tier to read and the reader is closed.
    pub fn load(root: &Path, config_dir: Option<&Path>, home: Option<&Path>) -> ReadRules {
        let mut rules = ReadRules::default();
        // (file, whether a file that cannot be read closes the reader)
        let mut files: Vec<(PathBuf, bool)> = Vec::new();
        match config_dir {
            Some(dir) => {
                for name in ["settings.json", "settings.local.json", "remote-settings.json"] {
                    files.push((dir.join(name), true));
                }
            }
            None => rules.close(Path::new("the account's settings directory"), "it is not known"),
        }
        let mut dirs: Vec<PathBuf> = root.ancestors().take(ANCESTORS_MAX).map(Path::to_path_buf).collect();
        for dir in dirs.clone() {
            if let Some(main) = main_checkout_of(&dir) {
                dirs.extend(main.ancestors().take(ANCESTORS_MAX).map(Path::to_path_buf));
            }
        }
        let mut seen: Vec<PathBuf> = Vec::new();
        for dir in dirs {
            if seen.contains(&dir) {
                continue;
            }
            let own = dir == root;
            for name in ["settings.json", "settings.local.json"] {
                // The project's own files must be readable; a folder above it that denies us is one the CLI cannot read.
                files.push((dir.join(".claude").join(name), own));
            }
            seen.push(dir);
        }
        for managed in MANAGED_DIRS {
            files.push((Path::new(managed).join("managed-settings.json"), true));
            if let Ok(entries) = std::fs::read_dir(Path::new(managed).join("managed-settings.d")) {
                let mut found: Vec<PathBuf> = entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
                    .collect();
                found.sort();
                files.extend(found.into_iter().map(|path| (path, true)));
            }
        }
        for (file, strict) in files {
            match read_settings_file(&file) {
                Ok(Some(bytes)) => rules.add_settings(&bytes, root, home, &file),
                Ok(None) => {}
                Err(error) if closes(strict, &error) => rules.close(&file, &error.why),
                Err(_) => {}
            }
        }
        rules
    }

    /// The rules of one settings file's text. A file that is not JSON closes the reader, as one that cannot be read.
    /// A pattern is read from the project's folder and, when the file lies elsewhere, from the folder it lies in.
    pub fn add_settings(&mut self, bytes: &[u8], root: &Path, home: Option<&Path>, origin: &Path) {
        let Ok(settings) = serde_json::from_slice::<Value>(bytes) else {
            return self.close(origin, "it is not JSON");
        };
        let Some(deny) = settings.get("permissions").and_then(|p| p.get("deny")) else {
            return;
        };
        let Some(list) = deny.as_array() else {
            return self.close(origin, "permissions.deny is not a list");
        };
        let base = settings_base(origin).filter(|base| base.is_absolute() && base != root);
        for entry in list {
            match entry.as_str() {
                Some(text) => self.add_rule(text, root, base.as_deref(), home),
                None => return self.close(origin, "a deny rule is not text"),
            }
        }
    }

    fn close(&mut self, origin: &Path, why: &str) {
        self.unreadable
            .get_or_insert_with(|| format!("{} could not be read ({why})", origin.display()));
    }

    /// One rule's text, `Read(<pattern>)` or `Read`. Any other tool's rule is not a limit on reading; a `Read(` that
    /// does not close is a rule this cannot parse, and reads as the widest there is.
    fn add_rule(&mut self, text: &str, root: &Path, base: Option<&Path>, home: Option<&Path>) {
        let text = text.trim();
        if text == "Read" || text == "Read(*)" || text == "Read(**)" {
            self.everything = true;
            return;
        }
        if text.starts_with("Read(") && !text.ends_with(')') {
            self.everything = true;
            return;
        }
        let Some(pattern) = text.strip_prefix("Read(").and_then(|rest| rest.strip_suffix(')')) else {
            return;
        };
        self.pattern_bytes += pattern.len();
        if pattern.len() > PATTERN_MAX || self.pattern_bytes > PATTERN_BYTES_MAX {
            self.unreadable
                .get_or_insert_with(|| "the settings hold more Read patterns than are read".to_owned());
            return;
        }
        let (mut rules, mut covers_all) = rules_of(pattern.trim(), root, home);
        if let Some(base) = base {
            // The same pattern as the file's own folder reads it.
            let (from_base, all) = rules_of(pattern.trim(), base, home);
            covers_all |= all;
            let prefix: Vec<String> = base
                .components()
                .filter(|part| !matches!(part, Component::RootDir))
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect();
            rules.extend(from_base.into_iter().filter_map(|rule| match rule.anchor {
                Anchor::Anywhere => None,
                Anchor::Absolute => Some(rule),
                Anchor::Root => Some(Rule::new(
                    Anchor::Absolute,
                    prefix.iter().cloned().chain(rule.segments).collect(),
                )),
            }));
        }
        // A rule that names the project's folder, or the filesystem's root, covers every file there is.
        self.everything |= covers_all;
        for rule in rules {
            if self.rules.len() >= RULES_MAX {
                self.unreadable
                    .get_or_insert_with(|| format!("the settings hold more than {RULES_MAX} Read rules"));
                return;
            }
            if !self.rules.contains(&rule) {
                self.rules.push(rule);
            }
        }
    }

    /// The reader of a caller that could not read the settings at all: nothing is told while it holds.
    pub fn unavailable() -> ReadRules {
        ReadRules {
            unreadable: Some("the settings were not read in time".to_owned()),
            ..ReadRules::default()
        }
    }

    /// Why nothing can be told, when a settings file could not be read.
    pub fn closed(&self) -> Option<&str> {
        self.unreadable.as_deref()
    }

    /// Whether a rule might cover `path`, which is absolute and under `root`. A path that is not is denied.
    pub fn denies(&self, root: &Path, path: &Path) -> bool {
        if self.everything || self.unreadable.is_some() {
            return true;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            return true;
        };
        let below: Vec<String> = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect();
        let absolute: Vec<String> = path
            .components()
            .filter(|part| !matches!(part, Component::RootDir))
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect();
        self.rules.iter().any(|rule| {
            let (haystack, starts_anywhere) = match rule.anchor {
                Anchor::Absolute => (&absolute, false),
                Anchor::Root => (&below, false),
                Anchor::Anywhere => (&below, true),
            };
            if rule.fixed > haystack.len() {
                return false;
            }
            // A rule that covers a folder covers what is under it: it holds for any leading part of the path.
            (1..=haystack.len()).any(|len| {
                let prefix = &haystack[..len];
                if starts_anywhere {
                    (0..prefix.len()).any(|from| matches_segments(&rule.segments, &prefix[from..]))
                } else {
                    matches_segments(&rule.segments, prefix)
                }
            })
        })
    }

    /// Whether `denies` holds for the file as named and for where it leads.
    pub fn denies_either(&self, root: &Path, named: &Path, resolved: &Path) -> bool {
        self.denies(root, named) || self.denies(root, resolved)
    }
}

/// The forms one written pattern may mean.
fn rules_of(pattern: &str, root: &Path, home: Option<&Path>) -> (Vec<Rule>, bool) {
    let pattern = pattern.strip_prefix('!').unwrap_or(pattern);
    let mut out = Vec::new();
    let mut covers_all = false;
    let mut push = |anchor, text: &str| {
        let segments: Vec<String> = text
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .map(str::to_owned)
            .collect();
        if segments.is_empty() {
            covers_all = true;
            return;
        }
        out.push(Rule::new(anchor, segments));
    };
    if let Some(rest) = pattern.strip_prefix("//") {
        push(Anchor::Absolute, rest);
    } else if let Some(rest) = pattern.strip_prefix("~/") {
        match home {
            Some(home) => push(Anchor::Absolute, &format!("{}/{rest}", home.display())),
            // Not knowing where `~` is, the rest of the pattern is read as a name at any depth.
            None => push(Anchor::Anywhere, rest),
        }
    } else if let Some(rest) = pattern.strip_prefix('/') {
        // From the project in the CLI's own words; a path from the filesystem root is read too.
        push(Anchor::Root, rest);
        push(Anchor::Absolute, rest);
    } else if pattern.starts_with("../") {
        let joined = lexical(&root.join(pattern));
        push(Anchor::Absolute, &joined.to_string_lossy());
    } else {
        push(Anchor::Root, pattern);
        push(Anchor::Anywhere, pattern);
    }
    (out, covers_all)
}

/// `path` with `.` and `..` taken out by their text.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
thread_local! { static MATCH_CELLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

/// Whether the pattern's segments match the path's segments from the first to the last. Worked out over every place in
/// the pattern and in the path at once, so a pattern with many `**` costs its length times the path's and no more:
/// a project's settings are not trusted to keep it small.
fn matches_segments(pattern: &[String], path: &[String]) -> bool {
    // Each segment that is not `**` takes exactly one of the path's: a pattern with more of them than the path has
    // matches nothing, and is not worked out.
    if pattern.iter().filter(|segment| *segment != "**").count() > path.len() {
        return false;
    }
    let (rows, columns) = (pattern.len() + 1, path.len() + 1);
    // `reach[p][n]`: the first `p` segments of the pattern match the first `n` of the path.
    #[cfg(test)]
    MATCH_CELLS.with(|count| count.set(count.get() + rows * columns));
    let mut reach = vec![vec![false; columns]; rows];
    reach[0][0] = true;
    for p in 0..pattern.len() {
        let wild = pattern[p] == "**";
        for n in 0..columns {
            if !reach[p][n] {
                continue;
            }
            if wild {
                // Any number of segments, none included.
                for later in n..columns {
                    reach[p + 1][later] = true;
                }
            } else if n < path.len() && matches_word(&pattern[p], &path[n]) {
                reach[p + 1][n + 1] = true;
            }
        }
    }
    reach[pattern.len()][path.len()]
}

/// One segment against one name: `*` any run of characters, `?` any one. A character class or an alternative
/// (`[a-z]`, `{a,b}`) reads as `*`, which can only match more than it says.
fn matches_word(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = widened(pattern).chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || (pattern[p] != '*' && pattern[p] == name[n])) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            mark = n;
            p += 1;
        } else if let Some(at) = star {
            p = at + 1;
            mark += 1;
            n = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// The pattern with each `[...]` or `{...}` group made a `*`, and a backslash dropped.
fn widened(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut depth_close: Option<char> = None;
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match (depth_close, c) {
            (Some(close), c) if c == close => {
                depth_close = None;
            }
            (Some(_), _) => {}
            (None, '[') => {
                depth_close = Some(']');
                out.push('*');
            }
            (None, '{') => {
                depth_close = Some('}');
                out.push('*');
            }
            (None, '\\') => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            (None, c) => out.push(c),
        }
    }
    // A group never closed matched to the end of the pattern: it was a `*` there.
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/work/project";

    fn rules(deny: &[&str]) -> ReadRules {
        let list: Vec<String> = deny.iter().map(|rule| format!("{rule:?}")).collect();
        let settings = format!(r#"{{"permissions":{{"deny":[{}]}}}}"#, list.join(","));
        let mut rules = ReadRules::default();
        rules.add_settings(
            settings.as_bytes(),
            Path::new(ROOT),
            Some(Path::new("/home/me")),
            Path::new("settings.json"),
        );
        rules
    }

    /// The files the measured cases read, as kept out (`true`) or let through.
    fn kept_out(deny: &[&str]) -> [bool; 4] {
        let rules = rules(deny);
        ["small.txt", "secret.txt", "sub/inner.txt", "sub/deep/leaf.txt"]
            .map(|name| rules.denies(Path::new(ROOT), &Path::new(ROOT).join(name)))
    }

    /// Each row is one of the forms measured on the pinned CLI and what it read there:
    /// small, secret, inner, leaf. `true` is a file the CLI kept out.
    #[test]
    fn every_written_form_keeps_out_what_the_pinned_cli_keeps_out() {
        let none = [false, false, false, false];
        assert_eq!(kept_out(&[]), none);
        // `deny_plain`, `deny_dot`, `deny_root_slash`, `deny_abs`: the one file.
        for rule in [
            "Read(secret.txt)",
            "Read(./secret.txt)",
            "Read(/secret.txt)",
            "Read(//work/project/secret.txt)",
        ] {
            assert_eq!(kept_out(&[rule]), [false, true, false, false], "{rule}");
        }
        // `deny_star`: a bare glob names files at every depth.
        assert_eq!(kept_out(&["Read(*.txt)"]), [true, true, true, true]);
        // `deny_dstar_name`, `deny_dstar_dir`.
        assert_eq!(kept_out(&["Read(**/inner.txt)"]), [false, false, true, false]);
        assert_eq!(kept_out(&["Read(**/deep/**)"]), [false, false, false, true]);
        // `deny_bare`.
        assert_eq!(kept_out(&["Read"]), [true, true, true, true]);
        // `deny_dir_plain`, `deny_dir_slash`, `deny_dir_star`: a folder covers what is under it.
        for rule in ["Read(sub)", "Read(./sub/)", "Read(sub/*)"] {
            assert_eq!(kept_out(&[rule]), [false, false, true, true], "{rule}");
        }
        // `deny_question`, `deny_case`, `deny_edit`.
        assert_eq!(kept_out(&["Read(secret.tx?)"]), [false, true, false, false]);
        assert_eq!(kept_out(&["Read(./SECRET.TXT)"]), none);
        assert_eq!(kept_out(&["Edit(./secret.txt)"]), none);
    }

    #[test]
    fn a_rule_the_cli_may_read_wider_is_read_wider_here() {
        // A name with a slash in it, written without a leading mark, also holds below another folder.
        assert_eq!(
            rules(&["Read(deep/leaf.txt)"]).denies(Path::new(ROOT), Path::new("/work/project/sub/deep/leaf.txt")),
            true
        );
        // A class or an alternative reads as a star.
        let classes = rules(&["Read(secret.[a-z]xt)", "Read(*.{log,tmp})"]);
        assert!(classes.denies(Path::new(ROOT), Path::new("/work/project/secret.txt")));
        assert!(classes.denies(Path::new(ROOT), Path::new("/work/project/a/b.tmp")));
    }

    #[test]
    fn a_file_is_held_to_its_name_and_to_where_it_leads() {
        let rules = rules(&["Read(./secret.txt)"]);
        let root = Path::new(ROOT);
        let link = root.join("link.txt");
        let target = root.join("secret.txt");
        assert!(rules.denies_either(root, &link, &target));
        assert!(!rules.denies_either(root, &link, &root.join("other.txt")));
        let by_name = self::rules(&["Read(./link.txt)"]);
        assert!(by_name.denies_either(root, &link, &target));
    }

    #[test]
    fn a_home_rule_and_a_parent_rule_are_anchored_where_they_say() {
        let home = rules(&["Read(~/notes/secret.txt)"]);
        assert!(home.rules.contains(&Rule::new(
            Anchor::Absolute,
            ["home", "me", "notes", "secret.txt"].map(String::from).to_vec()
        )));
        let parent = rules(&["Read(../project/secret.txt)"]);
        assert!(parent.denies(Path::new(ROOT), Path::new("/work/project/secret.txt")));
        let tilde = rules(&["Read(~/notes/*.md)"]);
        assert!(!tilde.denies(Path::new(ROOT), Path::new("/work/project/a.md")));
    }

    #[test]
    fn a_home_rule_with_no_home_known_is_read_as_a_name_at_any_depth() {
        let mut rules = ReadRules::default();
        rules.add_settings(
            br#"{"permissions":{"deny":["Read(~/notes/secret.txt)"]}}"#,
            Path::new(ROOT),
            None,
            Path::new("s"),
        );
        assert!(rules.denies(Path::new(ROOT), Path::new("/work/project/a/notes/secret.txt")));
        assert!(!rules.denies(Path::new(ROOT), Path::new("/work/project/a/secret.txt")));
    }

    #[test]
    fn a_pattern_made_of_many_double_stars_costs_no_more_than_its_length() {
        let pattern = format!("Read({}never.txt)", "**/".repeat(40));
        let rules = rules(&[&pattern]);
        let deep = format!("/work/project/{}x.txt", "d/".repeat(60));
        MATCH_CELLS.with(|count| count.set(0));
        assert!(!rules.denies(Path::new(ROOT), Path::new(&deep)));
        let pattern: Vec<String> = (0..40).map(|_| "**".into()).chain(["never.txt".into()]).collect();
        let path: Vec<String> = (0..60).map(|_| "d".into()).chain(["x.txt".into()]).collect();
        MATCH_CELLS.with(|count| count.set(0));
        assert!(!matches_segments(&pattern, &path));
        assert_eq!(
            MATCH_CELLS.with(|count| count.get()),
            (pattern.len() + 1) * (path.len() + 1)
        );
        assert!(rules.denies(
            Path::new(ROOT),
            Path::new(&format!("/work/project/{}never.txt", "d/".repeat(60)))
        ));
    }

    #[test]
    fn more_rules_than_the_bound_close_the_reader() {
        let many: Vec<String> = (0..RULES_MAX + 5).map(|n| format!("Read(f{n}.txt)")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        let rules = rules(&many);
        assert!(rules.closed().is_some_and(|why| why.contains("more than 4000")));
        assert!(rules.denies(Path::new(ROOT), Path::new("/work/project/other.txt")));
    }

    #[test]
    fn a_rule_that_names_the_project_or_the_root_covers_everything() {
        for rule in ["Read(./)", "Read(.)", "Read(//)", "Read(/)", "Read(./.)", "Read(**/)"] {
            assert!(
                rules(&[rule]).denies(Path::new(ROOT), Path::new("/work/project/a/b.txt")),
                "{rule}"
            );
        }
    }

    #[test]
    fn a_path_outside_the_project_is_denied() {
        assert!(ReadRules::default().denies(Path::new(ROOT), Path::new("/etc/passwd")));
    }

    #[test]
    fn a_settings_file_that_is_not_json_closes_the_reader() {
        let mut rules = ReadRules::default();
        rules.add_settings(
            b"{ not json",
            Path::new(ROOT),
            None,
            Path::new("/p/.claude/settings.json"),
        );
        assert!(rules.denies(Path::new(ROOT), Path::new("/work/project/a.txt")));
        assert!(rules
            .closed()
            .is_some_and(|why| why.contains("/p/.claude/settings.json")));
        let mut wrong = ReadRules::default();
        wrong.add_settings(
            br#"{"permissions":{"deny":"Read"}}"#,
            Path::new(ROOT),
            None,
            Path::new("s"),
        );
        assert!(wrong.closed().is_some());
    }

    #[test]
    fn a_settings_file_with_no_deny_list_denies_nothing() {
        let mut rules = ReadRules::default();
        rules.add_settings(
            br#"{"permissions":{"allow":["Read"]}}"#,
            Path::new(ROOT),
            None,
            Path::new("s"),
        );
        rules.add_settings(br#"{"hooks":{}}"#, Path::new(ROOT), None, Path::new("s"));
        assert_eq!(rules, ReadRules::default());
    }

    #[test]
    fn loading_reads_the_project_tiers_and_the_user_tier() {
        let dir = crate::state_dirs::test_workspace_dir("read-deny-load");
        let root = dir.join("project");
        let config = dir.join("config");
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            root.join(".claude/settings.local.json"),
            r#"{"permissions":{"deny":["Read(./a.env)"]}}"#,
        )
        .unwrap();
        std::fs::write(
            config.join("settings.json"),
            r#"{"permissions":{"deny":["Read(./b.env)"]}}"#,
        )
        .unwrap();
        let rules = ReadRules::load(&root, Some(&config), None);
        assert!(rules.denies(&root, &root.join("a.env")));
        assert!(rules.denies(&root, &root.join("b.env")));
        assert!(!rules.denies(&root, &root.join("c.env")));
    }

    fn settings_file(dir: &Path, name: &str, deny: &str) {
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(
            dir.join(".claude").join(name),
            format!(r#"{{"permissions":{{"deny":["{deny}"]}}}}"#),
        )
        .unwrap();
    }

    #[test]
    fn a_worktree_reads_the_local_file_of_the_checkout_it_belongs_to() {
        let dir = crate::state_dirs::test_workspace_dir("read-deny-worktree");
        let (main, wt, config) = (dir.join("main"), dir.join("wt"), dir.join("config"));
        for folder in [&wt, &config] {
            std::fs::create_dir_all(folder).unwrap();
        }
        let git_dir = main.join(".git").join("worktrees").join("wt");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("commondir"), "../..\n").unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", git_dir.display())).unwrap();
        settings_file(&main, "settings.local.json", "Read(./secret.txt)");
        let rules = ReadRules::load(&wt, Some(&config), None);
        assert!(rules.closed().is_none(), "{:?}", rules.closed());
        assert!(
            rules.denies(&wt, &wt.join("secret.txt")),
            "the main checkout's local rule covers the worktree's file"
        );
        assert!(!rules.denies(&wt, &wt.join("other.txt")));
    }

    #[test]
    fn a_session_in_a_subfolder_reads_the_files_of_the_folders_above_it() {
        let dir = crate::state_dirs::test_workspace_dir("read-deny-subfolder");
        let (sub, config) = (dir.join("repo").join("sub"), dir.join("config"));
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        settings_file(&dir.join("repo"), "settings.local.json", "Read(**/inner.txt)");
        settings_file(&dir.join("repo"), "settings.json", "Read(./sub/secret.txt)");
        let rules = ReadRules::load(&sub, Some(&config), None);
        assert!(rules.denies(&sub, &sub.join("inner.txt")));
        assert!(
            rules.denies(&sub, &sub.join("secret.txt")),
            "a pattern is also read from the folder its file is in"
        );
        assert!(!rules.denies(&sub, &sub.join("small.txt")));
    }

    #[test]
    fn the_server_managed_file_and_the_user_tiers_are_read_and_no_account_directory_closes_the_reader() {
        let dir = crate::state_dirs::test_workspace_dir("read-deny-tiers");
        let (root, config) = (dir.join("project"), dir.join("config"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("remote-settings.json"),
            r#"{"permissions":{"deny":["Read(./org.env)"]}}"#,
        )
        .unwrap();
        let rules = ReadRules::load(&root, Some(&config), None);
        assert!(rules.denies(&root, &root.join("org.env")));
        let none = ReadRules::load(&root, None, None);
        assert!(
            none.closed().is_some_and(|why| why.contains("not known")),
            "{:?}",
            none.closed()
        );
        assert!(none.denies(&root, &root.join("a.txt")));
    }

    #[test]
    fn a_settings_file_that_is_a_fifo_a_device_or_too_large_closes_the_reader_and_is_not_waited_for() {
        use std::os::unix::ffi::OsStrExt;
        let dir = crate::state_dirs::test_workspace_dir("read-deny-hostile");
        let config = dir.join("config");
        std::fs::create_dir_all(&config).unwrap();
        for (label, make) in [("fifo", 0), ("device", 1), ("huge", 2)] {
            let root = dir.join(label);
            std::fs::create_dir_all(root.join(".claude")).unwrap();
            let file = root.join(".claude").join("settings.json");
            match make {
                0 => {
                    let name = std::ffi::CString::new(file.as_os_str().as_bytes()).unwrap();
                    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                }
                1 => std::os::unix::fs::symlink("/dev/zero", &file).unwrap(),
                _ => std::fs::write(&file, vec![b' '; (FILE_MAX + 1) as usize]).unwrap(),
            }
            let config = config.clone();
            let root_in = root.clone();
            let rules = crate::test_tmp::completed(move || ReadRules::load(&root_in, Some(&config), None));
            let why = rules.closed().unwrap_or_else(|| panic!("{label}: not closed"));
            assert!(why.contains("settings.json"), "{label}: {why}");
            let reason = if make == 2 {
                "larger than 4 MiB"
            } else {
                "not a regular file"
            };
            assert!(why.contains(reason), "{label}: {why}");
            assert!(rules.denies(&root, &root.join("a.txt")), "{label}");
        }
    }

    #[test]
    fn a_link_to_a_regular_settings_file_is_followed() {
        let dir = crate::state_dirs::test_workspace_dir("read-deny-dotlink");
        let (root, config) = (dir.join("project"), dir.join("config"));
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            dir.join("shared.json"),
            r#"{"permissions":{"deny":["Read(./linked.env)"]}}"#,
        )
        .unwrap();
        std::os::unix::fs::symlink(dir.join("shared.json"), root.join(".claude/settings.json")).unwrap();
        let rules = ReadRules::load(&root, Some(&config), None);
        assert!(rules.closed().is_none());
        assert!(rules.denies(&root, &root.join("linked.env")));
    }

    #[test]
    fn a_rule_that_does_not_close_reads_as_everything_and_patterns_are_bounded_in_bytes() {
        assert!(rules(&["Read(foo"]).denies(Path::new(ROOT), Path::new("/work/project/a.txt")));
        let long = format!("Read({})", "a".repeat(PATTERN_MAX + 1));
        assert!(rules(&[&long]).closed().is_some());
        let each = "b".repeat(900);
        let many: Vec<String> = (0..400).map(|n| format!("Read({each}{n})")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(rules(&many)
            .closed()
            .is_some_and(|why| why.contains("more Read patterns")));
    }

    #[test]
    fn a_folder_above_that_we_may_not_look_into_is_skipped_by_the_kind_of_the_error_and_not_by_its_words() {
        let words_of_another_language = |kind| SettingsError {
            why: "没有权限".to_owned(),
            kind: Some(kind),
        };
        assert!(!closes(
            false,
            &words_of_another_language(std::io::ErrorKind::PermissionDenied)
        ));
        assert!(closes(
            true,
            &words_of_another_language(std::io::ErrorKind::PermissionDenied)
        ));
        assert!(closes(
            false,
            &words_of_another_language(std::io::ErrorKind::InvalidData)
        ));
        assert!(closes(
            false,
            &SettingsError::other("it is not a regular file".to_owned())
        ));
        // And really, in a folder above the project that denies us.
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::state_dirs::test_workspace_dir("read-deny-eacces");
        let (root, config) = (dir.join("above").join("project"), dir.join("config"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        let hidden = dir.join("above").join(".claude");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(hidden.join("settings.json"), "{}").unwrap();
        std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o000)).unwrap();
        let rules = ReadRules::load(&root, Some(&config), None);
        std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o700)).unwrap();
        let denied_to_us = std::fs::read(hidden.join("settings.json")).is_err();
        // (A test run as root can look anyway, and then there is nothing to skip.)
        assert!(rules.closed().is_none() || !denied_to_us, "{:?}", rules.closed());
    }

    #[test]
    fn a_pattern_with_more_segments_than_the_path_is_not_worked_out() {
        // Distinct, so that none is dropped as a repeat.
        let many: Vec<String> = (0..62)
            .map(|n| format!("Read(q{n}/{}x.txt)", "a/".repeat(2039)))
            .collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        let rules = rules(&many);
        assert!(rules.closed().is_none());
        MATCH_CELLS.with(|count| count.set(0));
        for n in 0..50 {
            assert!(!rules.denies(Path::new(ROOT), Path::new(&format!("/work/project/d/e/f{n}/g/h/i.txt"))));
        }
        assert!(!matches_segments(
            &["a".to_owned(), "b".to_owned(), "c".to_owned()],
            &["a".to_owned(), "b".to_owned()]
        ));
        assert_eq!(
            MATCH_CELLS.with(|count| count.get()),
            0,
            "impossible patterns allocate no match table"
        );
        assert!(matches_segments(
            &["**".to_owned(), "x".to_owned()],
            &["a".to_owned(), "x".to_owned()]
        ));
        assert!(!matches_segments(
            &["a".to_owned(), "b".to_owned(), "c".to_owned()],
            &["a".to_owned(), "b".to_owned()]
        ));
    }
}
