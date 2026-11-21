//! Git ignore translation and the ignore decisions shared with tree listings.
use crate::remote_project::SyncRecord;
use sha2::{Digest, Sha256};

pub const NON_GIT_DEFAULTS: &[&str] = &["target/", "node_modules/", ".venv/", "__pycache__/", ".cache/"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Origin {
    ExcludesFile,
    InfoExclude,
    GitIgnore,
}

#[derive(Clone, Debug)]
pub struct IgnoreSource {
    pub dir: String,
    pub text: String,
    pub origin: Origin,
}

#[derive(Clone, Debug)]
pub struct Translated {
    pub patterns: Vec<String>,
    pub problems: Vec<String>,
    pub digest: String,
}

impl Translated {
    pub fn new(patterns: Vec<String>, problems: Vec<String>) -> Self {
        let digest = format!("{:x}", Sha256::digest(patterns.join("\0").as_bytes()));
        Self {
            patterns,
            problems,
            digest,
        }
    }

    /// A refusal or a missing handler yields no usable ignore set for tree-session creation.
    pub fn from_response(
        response: Result<agent::remote_client::SyncIgnoresResponse, String>,
    ) -> Result<Self, super::SyncError> {
        let response = response.map_err(|e| super::SyncError::Helper(format!("sync not started: {e}")))?;
        if !response.refusal.is_empty() {
            return Err(super::SyncError::Helper(format!(
                "sync not started: {}",
                response.refusal
            )));
        }
        let translated = Self::new(response.patterns, response.problems);
        if translated.digest != response.digest {
            return Err(super::SyncError::Helper(
                "sync not started: ignore digest does not match its patterns".into(),
            ));
        }
        Ok(translated)
    }
}

/// Escape a literal relative path before embedding it in an ignore pattern.
pub fn literal(path: &str) -> String {
    let mut result = String::new();
    for c in path.chars() {
        if matches!(c, '\\' | '*' | '?' | '[' | ']' | '{' | '}' | '#' | '!' | ' ') {
            result.push('\\');
        }
        result.push(c);
    }
    result
}

fn trim_line(line: &str) -> &str {
    let mut end = line.len();
    while end > 0 && line.as_bytes()[end - 1] == b' ' {
        let backslashes = line[..end - 1].bytes().rev().take_while(|b| *b == b'\\').count();
        if backslashes % 2 == 1 {
            break;
        }
        end -= 1;
    }
    &line[..end]
}

/// Anchor a source pattern under its literal directory within the sync root.
fn rebase(pattern: &str, dir: &str) -> Vec<String> {
    let directory = pattern.ends_with('/');
    let body = pattern.trim_end_matches('/');
    let anchored = body.starts_with('/') || body.contains('/');
    let body = body.trim_start_matches('/');
    let suffix = if directory { "/" } else { "" };
    if dir.is_empty() || dir == "." {
        return vec![if anchored {
            format!("/{body}{suffix}")
        } else {
            format!("{body}{suffix}")
        }];
    }
    vec![if anchored {
        format!("/{}/{body}{suffix}", literal(dir))
    } else {
        format!("/{}/**/{body}{suffix}", literal(dir))
    }]
}

pub fn translate(sources: &[IgnoreSource], tracked_ignored: &[String]) -> Translated {
    let mut ordered: Vec<_> = sources.iter().collect();
    ordered.sort_by_key(|s| {
        (
            s.origin,
            s.dir.split('/').filter(|p| !p.is_empty()).count() as isize * if s.dir.starts_with("..") { -1 } else { 1 },
        )
    });
    let mut patterns = Vec::new();
    let mut problems = Vec::new();
    let mut forced = Vec::new();
    for source in ordered {
        let label = if source.dir.is_empty() { "." } else { &source.dir };
        if source.dir.split('/').any(|part| part == "..") {
            problems.push(format!(
                "{}: ancestor ignore source needs the repository-relative root; the root is excluded",
                source.dir
            ));
            forced.push("/**".into());
            continue;
        }
        for line in source.text.lines() {
            let line = trim_line(line);
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (negative, body) = line.strip_prefix('!').map_or((false, line), |p| (true, p));
            if body.is_empty() || body == "/" {
                continue;
            }
            if body.chars().any(char::is_control) || body.contains("[:") || body.contains("[[.") || body.contains("[[=")
            {
                problems.push(format!(
                    "{}: ignore pattern cannot be represented safely; its directory is excluded",
                    label
                ));
                forced.push(if source.dir.is_empty() || source.dir.starts_with("..") {
                    "/**".into()
                } else {
                    format!("/{}/", literal(&source.dir))
                });
                continue;
            }
            let Some(body) = mutagen_body(body) else {
                problems.push(format!("{label}: invalid ignore pattern {line:?} was dropped"));
                continue;
            };
            for pattern in rebase(&body, &source.dir) {
                patterns.push(if negative { format!("!{pattern}") } else { pattern });
            }
        }
    }
    // Parent reachability is decided after all rules, including later directory exceptions.
    let matcher = Matcher::new(&patterns);
    patterns.retain(|pattern| {
        let Some(body) = pattern.strip_prefix('!') else {
            return true;
        };
        let candidate = body.trim_start_matches('/').trim_end_matches('/');
        matcher.excluded_parent(candidate).is_none()
    });
    append_includes(&mut patterns, tracked_ignored);
    let forced_matcher = Matcher::new(&forced);
    for path in tracked_ignored {
        if forced_matcher.ignored(path, false) {
            let dir = forced_matcher.excluded_parent(path).unwrap_or("the sync root");
            problems.push(format!("{path} is tracked but will not sync because {dir} is excluded"));
        }
    }
    patterns.extend(forced);
    Translated::new(patterns, problems)
}

/// Git treats braces literally, while Mutagen gives them alternation syntax.
/// Invalid Git globs match nothing and must not prevent a Mutagen session from starting.
fn mutagen_body(body: &str) -> Option<String> {
    let chars: Vec<_> = body.chars().collect();
    let mut result = String::new();
    let mut i = 0;
    while i < chars.len() {
        let end = match chars[i] {
            '\\' => {
                chars.get(i + 1)?;
                i + 2
            }
            '[' => {
                let end = character_class(&chars, i, None)?.0;
                let mut index = i;
                while index < end {
                    if chars[index] == '\\' {
                        result.extend(chars[index..index + 2].iter());
                        index += 2;
                    } else {
                        if matches!(chars[index], '{' | '}') {
                            result.push('\\');
                        }
                        result.push(chars[index]);
                        index += 1;
                    }
                }
                i = end;
                continue;
            }
            '{' | '}' => {
                result.push('\\');
                i + 1
            }
            _ => i + 1,
        };
        result.extend(chars[i..end].iter());
        i = end;
    }
    let directory = result.ends_with('/');
    if let Some(prefix) = result.trim_end_matches('/').strip_suffix("/**") {
        result = format!("{prefix}/*/**{}", if directory { "/" } else { "" });
    }
    Some(result)
}

fn append_includes(patterns: &mut Vec<String>, paths: &[String]) {
    let matcher = Matcher::new(patterns);
    let mut directories = std::collections::BTreeSet::new();
    for path in paths {
        let mut excluded = false;
        for parent in parents(path) {
            excluded |= matcher.direct(parent, true);
            if excluded {
                directories.insert((parent.matches('/').count(), parent.to_owned()));
            }
        }
    }
    // Open only concrete parents, then hide their contents again before including files.
    // Keeping every original rule prevents exceptions from exposing unrelated directories.
    for (_, directory) in directories {
        let directory = literal(&directory);
        patterns.push(format!("!/{directory}/"));
        patterns.push(format!("/{directory}/*"));
    }
    patterns.extend(paths.iter().map(|path| format!("!/{}", literal(path))));
}

/// An explicit include either takes effect or returns the directory that prevents it.
pub fn include(patterns: &[String], path: &str) -> Result<Vec<String>, String> {
    let mut result = patterns.to_vec();
    append_includes(&mut result, &[path.into()]);
    let matcher = Matcher::new(&result);
    if matcher.ignored(path, false) {
        let dir = matcher.excluded_parent(path).unwrap_or(path);
        return Err(format!("{path} will not sync because {dir} is excluded"));
    }
    Ok(result)
}

pub fn effective(server: &Translated, record: &SyncRecord) -> Vec<String> {
    let mut patterns = server.patterns.clone();
    // Tripwire exclusions are saved in excludes_added; tripwire_allowed is an allowance, not an exclusion.
    patterns.extend(record.excludes_added.iter().map(|p| format!("/{}", literal(p))));
    append_includes(&mut patterns, &record.excludes_removed);
    patterns
}

/// Start a nested repository's rules afresh without exposing an excluded ancestor.
pub fn scope_repository(patterns: &mut Vec<String>, dir: &str, nested: &[String]) -> Result<(), String> {
    if let Some(parent) = Matcher::new(patterns).excluded_parent(dir) {
        return Err(format!(
            "{dir}: nested repository will not sync because outer directory {parent} is excluded"
        ));
    }
    let dir = literal(dir);
    patterns.push(format!("!/{dir}/"));
    patterns.push(format!("!/{dir}/**"));
    for pattern in nested {
        let (negative, body) = pattern.strip_prefix('!').map_or(("", pattern.as_str()), |p| ("!", p));
        patterns.push(if let Some(body) = body.strip_prefix('/') {
            format!("{negative}/{dir}/{body}")
        } else {
            format!("{negative}/{dir}/**/{body}")
        });
    }
    Ok(())
}

fn parents(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(|(i, _)| &path[..i])
}

#[derive(Clone, Debug)]
struct Rule {
    negative: bool,
    directory: bool,
    anchored: bool,
    pattern: String,
}

impl Rule {
    fn new(pattern: &str) -> Self {
        let (negative, body) = pattern.strip_prefix('!').map_or((false, pattern), |s| (true, s));
        let directory = body.ends_with('/');
        let body = body.trim_end_matches('/');
        Self {
            negative,
            directory,
            anchored: body.contains('/'),
            pattern: body.trim_start_matches('/').into(),
        }
    }

    fn matches(&self, path: &str, is_dir: bool, icase: bool) -> bool {
        if self.directory && !is_dir {
            return false;
        }
        let path = if self.anchored {
            path
        } else {
            path.rsplit('/').next().unwrap_or(path)
        };
        glob(&self.pattern, path, icase) || self.pattern.strip_suffix("/**").is_some_and(|p| glob(p, path, icase))
    }
}

#[derive(Clone, Debug)]
pub struct Matcher {
    rules: Vec<Rule>,
}

impl Matcher {
    pub fn new(patterns: &[String]) -> Self {
        Self {
            rules: patterns.iter().map(|p| Rule::new(p)).collect(),
        }
    }

    fn direct(&self, path: &str, is_dir: bool) -> bool {
        if path.rsplit('/').next() == Some(".git") {
            return true;
        }
        let mut ignored = false;
        for rule in &self.rules {
            if rule.matches(path, is_dir, false) {
                ignored = !rule.negative;
            }
        }
        ignored
    }

    /// An icase Git operand can select a server-only spelling. A case-sensitive inclusion of
    /// one spelling cannot establish that every spelling is synchronized.
    pub(super) fn possibly_ignored_icase(&self, rel: &str) -> bool {
        parents(rel).chain(std::iter::once(rel)).any(|path| {
            self.rules
                .iter()
                .any(|rule| !rule.negative && rule.matches(path, true, true))
        })
    }

    fn excluded_parent<'a>(&self, path: &'a str) -> Option<&'a str> {
        parents(path).find(|p| self.direct(p, true))
    }

    pub fn ignored(&self, rel: &str, is_dir: bool) -> bool {
        self.excluded_parent(rel).is_some() || self.direct(rel, is_dir)
    }
}

// Rows are evaluated backwards with O(path length) storage. A whole '**/' component
// may stop only at a component boundary; an ordinary star never crosses a slash.
fn glob(pattern: &str, text: &str, icase: bool) -> bool {
    let equal = |a: char, b: char| a == b || icase && a.eq_ignore_ascii_case(&b);
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < p.len() {
        let start = i;
        i += match p[i] {
            '\\' if i + 1 < p.len() => 2,
            '*' => {
                let end = (i..p.len()).find(|k| p[*k] != '*').unwrap_or(p.len());
                let double = end - i >= 2 && (i == 0 || p[i - 1] == '/') && (end == p.len() || p[end] == '/');
                end - i + usize::from(double && end < p.len())
            }
            '[' => character_class(&p, i, None).map_or(1, |(end, _)| end - i),
            _ => 1,
        };
        tokens.push((start, i));
    }
    let mut next = vec![false; t.len() + 1];
    next[t.len()] = true;
    for (start, end) in tokens.into_iter().rev() {
        let mut row = vec![false; t.len() + 1];
        let double = p[start] == '*'
            && end - start >= 2
            && (start == 0 || p[start - 1] == '/')
            && (end == p.len() || p[end - 1] == '/');
        let prefix = double && p[end - 1] == '/';
        for j in (0..=t.len()).rev() {
            row[j] = match p[start] {
                '*' => {
                    (next[j] && (!prefix || j == 0 || t[j - 1] == '/'))
                        || (j < t.len() && (double || t[j] != '/') && row[j + 1])
                }
                '?' => j < t.len() && t[j] != '/' && next[j + 1],
                '[' if end - start > 1 => {
                    j < t.len() && next[j + 1] && {
                        let matches = |c| character_class(&p, start, Some(c)).unwrap().1;
                        matches(t[j])
                            || icase && (matches(t[j].to_ascii_lowercase()) || matches(t[j].to_ascii_uppercase()))
                    }
                }
                '\\' if end - start == 2 => t.get(j).is_some_and(|&c| equal(c, p[start + 1])) && next[j + 1],
                c => t.get(j).is_some_and(|&ch| equal(ch, c)) && next[j + 1],
            };
        }
        next = row;
    }
    next[0]
}

fn character_class(p: &[char], start: usize, value: Option<char>) -> Option<(usize, bool)> {
    let mut i = start + 1;
    let invert = matches!(p.get(i), Some('!' | '^'));
    i += usize::from(invert);
    let mut matched = false;
    let first = i;
    while i < p.len() && (p[i] != ']' || i == first) {
        let mut c = p[i];
        if c == '\\' {
            i += 1;
            c = *p.get(i)?;
        }
        if p.get(i + 1) == Some(&'-') && p.get(i + 2).is_some_and(|c| *c != ']') {
            matched |= value.is_some_and(|v| c <= v && v <= p[i + 2]);
            i += 3;
        } else {
            matched |= value == Some(c);
            i += 1;
        }
    }
    (i < p.len()).then_some((i + 1, value.is_some_and(|v| v != '/') && (matched != invert)))
}

/// Rebase an ancestor file with the actual path from that file's directory to the root.
/// The names are essential: an anchored rule for a sibling must not exclude the root's files.
pub fn ancestor_source(text: &str, root_from_source: &str) -> IgnoreSource {
    let mut result = Vec::new();
    for line in text.lines() {
        let line = trim_line(line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (negative, body) = line.strip_prefix('!').map_or((false, line), |p| (true, p));
        if mutagen_body(body).is_none() {
            // Keep invalid lines visible to translation so projection cannot hide their diagnostics.
            result.push(line.to_owned());
            continue;
        }
        let directory = body.ends_with('/');
        let body = body.trim_end_matches('/');
        let neg = if negative { "!" } else { "" };
        if !body.contains('/') {
            result.push(format!("{neg}{body}{}", if directory { "/" } else { "" }));
            continue;
        }
        let components: Vec<_> = body.trim_start_matches('/').split('/').collect();
        // Keep all glob residuals: '**' can stop at more than one component boundary.
        let mut states = vec![0];
        for name in root_from_source.split('/') {
            let mut next = Vec::new();
            for state in states {
                let mut index = state;
                while components.get(index) == Some(&"**") {
                    next.push(index);
                    index += 1;
                }
                if components.get(index).is_some_and(|p| glob(p, name, false)) {
                    next.push(index + 1);
                }
                if index == components.len() {
                    next.push(index);
                }
            }
            next.sort_unstable();
            next.dedup();
            states = next;
        }
        for state in states {
            let tail = components[state..].join("/");
            if !tail.is_empty() {
                result.push(format!("{neg}/{tail}{}", if directory { "/" } else { "" }));
            }
        }
    }
    IgnoreSource {
        dir: String::new(),
        text: result.join("\n"),
        origin: Origin::GitIgnore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn src(dir: &str, text: &str) -> IgnoreSource {
        IgnoreSource {
            dir: dir.into(),
            text: text.into(),
            origin: Origin::GitIgnore,
        }
    }
    fn matched(sources: &[IgnoreSource], path: &str, directory: bool) -> bool {
        Matcher::new(&translate(sources, &[]).patterns).ignored(path, directory)
    }

    #[test]
    fn ancestor_root_negations_never_become_catch_all_includes() {
        for text in [".env\n*.pem\n/*\n!/app/\n!.gitignore", "*\n!*/\n!*.rs"] {
            let source = ancestor_source(text, "app");
            let translated = translate(&[source], &[]);
            assert!(!translated.patterns.iter().any(|p| p == "!/**"));
            assert!(Matcher::new(&translated.patterns).ignored(".env", false));
        }
        assert!(ancestor_source("/app/\n!/app/", "app").text.is_empty());
    }

    #[test]
    fn source_directory_metacharacters_are_literal() {
        for name in ["[slug]", "*", "?", "\\", "{}"] {
            let translated = translate(&[src(&format!("app/{name}"), "secret.json\n/root")], &[]);
            let matcher = Matcher::new(&translated.patterns);
            assert!(matcher.ignored(&format!("app/{name}/secret.json"), false), "{name}");
            assert!(matcher.ignored(&format!("app/{name}/root"), false), "{name}");
            assert!(!matcher.ignored("app/other/secret.json", false), "{name}");
        }
    }

    #[test]
    fn concrete_tracked_chains_preserve_source_rules_and_other_directories() {
        let translated = translate(&[src("", "/deploy/\n*.key\n!public.key")], &["deploy/README".into()]);
        assert_eq!(&translated.patterns[..3], ["/deploy/", "*.key", "!public.key"]);
        let matcher = Matcher::new(&translated.patterns);
        assert!(!matcher.ignored("deploy/README", false));
        assert!(matcher.ignored("deploy/public.key", false));
        let record: SyncRecord = serde_json::from_value(serde_json::json!({
            "engine": "mutagen", "host_key_sha256": "SHA256:example", "excludes_removed": ["deploy/README"]
        }))
        .unwrap();
        let base = translate(&[src("", "/deploy/\n*.key\n!public.key")], &[]);
        let effective_matcher = Matcher::new(&effective(&base, &record));
        assert!(!effective_matcher.ignored("deploy/README", false));
        assert!(effective_matcher.ignored("deploy/public.key", false));
        let included = Matcher::new(&include(&base.patterns, "deploy/README").unwrap());
        assert!(!included.ignored("deploy/README", false));
        assert!(included.ignored("deploy/public.key", false));
        let translated = translate(&[src("", "target/\n!/app/target/")], &["target/keep".into()]);
        assert!(!Matcher::new(&translated.patterns).ignored("app/target/main.rs", false));
        assert!(Matcher::new(&translated.patterns).ignored("else/target/keep", false));
        let translated = translate(&[src("", "target/")], &["target/doc/a".into(), "target/doc/b".into()]);
        assert!(translated.problems.is_empty());
        assert_eq!(translated.patterns.iter().filter(|p| *p == "!/target/").count(), 1);
        let matcher = Matcher::new(&translated.patterns);
        for path in ["target/doc/a", "target/doc/b"] {
            assert!(!matcher.ignored(path, false));
        }
        for path in ["target/doc/c", "target/other/a"] {
            assert!(matcher.ignored(path, false));
        }
    }

    #[test]
    fn trailing_double_star_preserves_its_parent_and_parent_file() {
        let translated = translate(&[src("", "logs/**\n!logs/.gitkeep\nxf/**")], &[]);
        assert!(!Matcher::new(&translated.patterns).ignored("logs/.gitkeep", false));
        assert!(!Matcher::new(&translated.patterns).ignored("xf", false));
        assert!(Matcher::new(&["xf/**".into()]).ignored("xf", false));
        assert!(Matcher::new(&translated.patterns).ignored("logs/sub/file", false));
        let translated = translate(&[src("", "logs/**/")], &[]);
        assert_eq!(translated.patterns, ["/logs/*/**/"]);
        let matcher = Matcher::new(&translated.patterns);
        assert!(!matcher.ignored("logs", true));
        assert!(!matcher.ignored("logs/a.txt", false));
        assert!(matcher.ignored("logs/sub", true));
        assert!(matcher.ignored("logs/sub/a", false));
        assert!(!matcher.ignored("logs/sub", false));
    }

    #[test]
    fn braces_are_literal_and_invalid_globs_are_reported_and_dropped() {
        let translated = translate(&[src("", "secret{1}\n*.{pem,key}\nunclosed[\ntrailing\\")], &[]);
        assert_eq!(translated.problems.len(), 2);
        assert_eq!(translated.patterns, [r"secret\{1\}", r"*.\{pem,key\}"]);
        assert!(translated.problems.iter().all(|p| p.starts_with(".:")));
        let matcher = Matcher::new(&translated.patterns);
        assert!(matcher.ignored("secret{1}", false));
        assert!(!matcher.ignored("secret1", false));
        assert!(matcher.ignored("a.{pem,key}", false));
        assert_eq!(literal("a{b}"), r"a\{b\}");
        let ancestor = ancestor_source("/app/unclosed[\n/bad[/secret\n/app/trailing\\", "app");
        let translated = translate(&[ancestor], &[]);
        assert!(translated.patterns.is_empty());
        assert_eq!(translated.problems.len(), 3);
    }

    #[test]
    fn unreachable_negations_are_decided_against_final_rules() {
        let translated = translate(&[src("", "*.log\ntarget/\n!target/keep.log\n!target/")], &[]);
        assert!(translated.patterns.contains(&"!/target/keep.log".into()));
        assert!(!Matcher::new(&translated.patterns).ignored("target/keep.log", false));
        assert!(Matcher::new(&translated.patterns).ignored("target/drop.log", false));
    }

    #[test]
    fn root_and_nested_gitignores_are_anchored_to_their_directory() {
        let sources = [src("", "/a\nname\ndir/\n**/c\nx/**"), src("sub", "/root\nbuild/\na/b")];
        for p in [
            "a",
            "name",
            "deep/name",
            "dir/file",
            "c",
            "deep/c",
            "sub/root",
            "sub/deep/build/file",
            "sub/a/b",
            "x/file",
        ] {
            assert!(matched(&sources, p, false), "{p}");
        }
        for p in [
            "deep/a",
            "name2",
            "cc",
            "deep/cc",
            "sub/deep/root",
            "else/build/file",
            "deep/sub/a/b",
            "dir",
        ] {
            assert!(!matched(&sources, p, false), "{p}");
        }
        assert!(!matched(&sources, "x", true));
    }

    #[test]
    fn negation_keeps_gitignore_order() {
        let sources = [src("", "*.log\n!keep.log\n/sub/keep.log")];
        assert!(!matched(&sources, "keep.log", false));
        assert!(!matched(&sources, "deep/keep.log", false));
        assert!(matched(&sources, "sub/keep.log", false));
        assert!(matched(&sources, "deep/no.log", false));
    }

    #[test]
    fn a_negation_under_an_excluded_dir_is_dropped() {
        let translated = translate(&[src("", "target/\n!target/doc\n!target/*.txt")], &[]);
        assert_eq!(translated.patterns, ["target/"]);
        assert!(Matcher::new(&translated.patterns).ignored("target/doc", false));
        let translated = translate(&[src("", "/target/*\n!/target/doc\n!/target/x")], &[]);
        assert!(!Matcher::new(&translated.patterns).ignored("target/doc/a", false));
        assert!(!Matcher::new(&translated.patterns).ignored("target/x", false));
    }

    #[test]
    fn info_exclude_and_excludes_file_come_first() {
        let mut info = src("", "!a");
        info.origin = Origin::InfoExclude;
        let mut global = src("", "a");
        global.origin = Origin::ExcludesFile;
        let translated = translate(&[src("", "a"), info, global], &[]);
        assert_eq!(translated.patterns, ["a", "!a", "a"]);
    }

    #[test]
    fn escaped_space_and_hash_survive() {
        let translated = translate(&[src("", "# comment\n\\#secret\nspace\\ name\n\\!bang")], &[]);
        assert_eq!(translated.patterns, ["\\#secret", "space\\ name", "\\!bang"]);
        for p in ["#secret", "a/#secret", "space name", "!bang"] {
            assert!(Matcher::new(&translated.patterns).ignored(p, false), "{p}");
        }
    }

    #[test]
    fn crlf_and_trailing_spaces_follow_git() {
        let sources = [src("", "a  \r\nb\\ \r\nc\\\\  \r\n")];
        for p in ["a", "b ", "c\\"] {
            assert!(matched(&sources, p, false), "{p}");
        }
        for p in ["a ", "b", "c"] {
            assert!(!matched(&sources, p, false), "{p}");
        }
    }

    #[test]
    fn tracked_ignored_files_still_sync() {
        let translated = translate(
            &[src("", "target/\n*.log"), src("sub", "build/")],
            &[
                "target/keep.log".into(),
                "sub/deep/build/a".into(),
                "other/keep.log".into(),
            ],
        );
        assert!(translated.problems.is_empty(), "{:?}", translated.problems);
        assert!(translated.patterns.contains(&"/target/*".into()));
        assert!(translated.patterns.contains(&"/sub/deep/build/*".into()));
        let matcher = Matcher::new(&translated.patterns);
        for p in ["target/keep.log", "sub/deep/build/a", "other/keep.log"] {
            assert!(!matcher.ignored(p, false), "{p}");
        }
        assert!(matcher.ignored("target/drop", false));
        let translated = translate(&[src("", "target/")], &["target/doc/a".into()]);
        assert!(translated.problems.is_empty());
        assert!(!Matcher::new(&translated.patterns).ignored("target/doc/a", false));
    }

    #[test]
    fn tracked_exception_does_not_soften_an_already_reincluded_directory() {
        let translated = translate(&[src("", "target/\n!target/\n*.log")], &["target/tracked.log".into()]);
        assert!(!Matcher::new(&translated.patterns).ignored("target/plain", false));
        assert!(!Matcher::new(&translated.patterns).ignored("target/tracked.log", false));
        assert!(Matcher::new(&translated.patterns).ignored("target/other.log", false));
    }

    #[test]
    fn root_below_toplevel_applies_ancestor_gitignores() {
        let ancestor = ancestor_source("/app/secret\n/other/keep\n*.log\n/app/a/**/hidden\n", "app");
        for p in ["secret", "deep/a.log", "a/hidden", "a/deep/hidden"] {
            assert!(matched(std::slice::from_ref(&ancestor), p, false), "{p}");
        }
        assert!(!matched(&[ancestor], "keep", false));
        assert!(!matched(&[ancestor_source("/app/", "app/sub")], "anything", false));
        assert!(!matched(&[ancestor_source("build/", "build/sub")], "anything", false));
    }

    #[test]
    fn record_additions_and_removals_apply_after_translation() {
        let record: SyncRecord = serde_json::from_value(serde_json::json!({
            "engine":"mutagen", "host_key_sha256":"SHA256:example",
            "excludes_added":["build"], "excludes_removed":["build", "secret"], "tripwire_allowed":["big"]
        }))
        .unwrap();
        let patterns = effective(&translate(&[src("", "secret")], &[]), &record);
        let matcher = Matcher::new(&patterns);
        assert!(!matcher.ignored("build/file", false));
        assert!(!matcher.ignored("secret", false));
        assert!(!matcher.ignored("big", false));
    }

    #[test]
    fn include_under_an_excluded_dir_takes_effect_or_says_why() {
        let patterns = vec!["/target/".into()];
        let included = include(&patterns, "target/x").unwrap();
        assert!(!Matcher::new(&included).ignored("target/x", false));
        let included = include(&patterns, "target/doc/a").unwrap();
        assert!(!Matcher::new(&included).ignored("target/doc/a", false));
        assert!(Matcher::new(&included).ignored("target/doc/other", false));
        assert!(include(&[], ".git").unwrap_err().contains(".git is excluded"));
        let included = include(&["*.log".into()], "literal[1].log").unwrap();
        assert!(!Matcher::new(&included).ignored("literal[1].log", false));
        assert!(Matcher::new(&included).ignored("literal1.log", false));
    }

    #[test]
    fn unrepresentable_source_patterns_exclude_their_directory_with_a_problem() {
        for source in [
            src("..", "/app/secret"),
            src("", "[[:alpha:]]*"),
            src("", "secret\tfile"),
        ] {
            let translated = translate(&[source], &[]);
            assert!(!translated.problems.is_empty());
            assert!(Matcher::new(&translated.patterns).ignored("secret", false));
        }
    }

    #[test]
    fn no_ignores_answer_no_tree_session() {
        use agent::remote_client::SyncIgnoresResponse;
        for response in [
            Err("SyncIgnores is not built yet".into()),
            Ok(SyncIgnoresResponse {
                refusal: "git failed".into(),
                ..Default::default()
            }),
        ] {
            let error = Translated::from_response(response).unwrap_err().to_string();
            assert!(error.starts_with("sync not started:"), "{error}");
        }
        let translated = translate(&[src("", "secret")], &[]);
        assert!(Translated::from_response(Ok(SyncIgnoresResponse {
            patterns: translated.patterns,
            digest: translated.digest,
            ..Default::default()
        }))
        .is_ok());
        assert!(Translated::from_response(Ok(SyncIgnoresResponse::default()))
            .unwrap_err()
            .to_string()
            .contains("digest"));
    }

    #[test]
    fn digest_changes_only_when_patterns_change() {
        let first = translate(&[src("", "a\n")], &[]);
        assert_eq!(first.digest, translate(&[src("", "# comment\na  \r\n")], &[]).digest);
        assert_ne!(first.digest, translate(&[src("", "b")], &[]).digest);
        assert_ne!(
            Translated::new(vec!["a".into(), "b".into()], vec![]).digest,
            Translated::new(vec!["ab".into()], vec![]).digest
        );
    }

    #[test]
    fn git_metadata_is_always_ignored_and_directories_are_pruned() {
        let matcher = Matcher::new(&["!**".into()]);
        for p in [".git", "sub/.git", "sub/.git/config"] {
            assert!(matcher.ignored(p, false));
            assert!(matcher.ignored(p, true));
        }
        assert!(!matcher.ignored(".github/a", false));
        let matcher = Matcher::new(&["a/".into(), "!a/b".into()]);
        assert!(matcher.ignored("a/b", false));
    }

    #[test]
    fn nested_repository_clears_outer_rules_or_reports_an_excluded_parent() {
        let mut patterns = vec!["build/".into(), "*.key".into(), "/sub/".into()];
        scope_repository(&mut patterns, "sub", &["secret".into()]).unwrap();
        let matcher = Matcher::new(&patterns);
        assert!(!matcher.ignored("sub/build/x", false));
        assert!(!matcher.ignored("sub/a.key", false));
        assert!(matcher.ignored("sub/secret", false));
        assert!(matcher.ignored("other/build/x", false));
        let mut patterns = vec!["/parent/".into()];
        let problem = scope_repository(&mut patterns, "parent/sub", &[]).unwrap_err();
        assert!(problem.contains("parent/sub") && problem.contains("outer directory parent is excluded"));
        assert_eq!(patterns, ["/parent/"]);
    }

    #[test]
    fn glob_classes_questions_and_double_stars_are_component_aware() {
        for (pattern, path, expected) in [
            ("**/a", "ba", false),
            ("**/a", "a", true),
            ("**/a", "x/a", true),
            ("a/**/b", "a/b", true),
            ("a/**/b", "a/x/b", true),
            ("a/**/b", "a/xb", false),
            ("a/*/b", "a/x/y/b", false),
            ("[a-c]?.rs", "b1.rs", true),
            ("[!a-c]", "d", true),
            ("[!a-c]", "/", false),
            ("x[[]", "x[", true),
        ] {
            assert_eq!(glob(pattern, path, false), expected, "{pattern} {path}");
        }
        assert!(glob(&"a".repeat(20000), &"a".repeat(10), false) == false);
    }

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("sync-ignore-{}-{}", std::process::id(), uuid::Uuid::new_v4()));
            agent::private_fs::create_private_dir_all(&root, &root).unwrap();
            Self(root)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(root: &Path) -> (Vec<IgnoreSource>, Vec<String>, Vec<String>) {
        let mut sources = vec![
            src("", "/a\nname\ndir/\n**/hidden\n*.log\n!keep.log\n/target/*\n!/target/doc\nspace\\ name\n\\#secret\n[ab]?.tmp\nx/**\nxf/**\nlogs/**/\nbuild/\n!logs/.gitkeep\nsecret{1}\n*.{pem,key}\ncomma,name\ndouble\"quote\nunclosed[\ntrailing\\\n/deploy/\n*.key\n!public.key\ntracked-target/\n!/app/tracked-target/\n"),
            src("sub", "/root\nbuild/\n*.bak\n!keep.bak"),
        ];
        for dir in ["[slug]", "*", "?", "\\", "{}"] {
            sources.push(src(&format!("app/{dir}"), "secret.json\n/root"));
        }
        for dir in ["module", "nested"] {
            sources.push(src(dir, ".env\nnode_modules/"));
        }
        std::fs::create_dir_all(root).unwrap();
        for source in &sources {
            std::fs::create_dir_all(root.join(&source.dir)).unwrap();
            std::fs::write(root.join(&source.dir).join(".gitignore"), &source.text).unwrap();
        }
        // The sync root is nested below a whitelist at the repository top level.
        let parent = root.parent().unwrap();
        assert_eq!(root.file_name().unwrap(), "alpha");
        assert!(git(parent, &["init", "-q"]).status.success());
        let ancestor = ".env\n*.pem\n/*\n!/alpha/\n!.gitignore\n";
        std::fs::write(parent.join(".gitignore"), ancestor).unwrap();
        sources.insert(0, ancestor_source(ancestor, "alpha"));
        let global = parent.join("global-ignore");
        std::fs::write(&global, "*.global-ignore\n").unwrap();
        let link = parent.join("global-ignore-link");
        std::os::unix::fs::symlink(&global, &link).unwrap();
        assert!(git(parent, &["config", "core.excludesFile", link.to_str().unwrap()])
            .status
            .success());
        sources.insert(
            0,
            IgnoreSource {
                dir: String::new(),
                text: "*.global-ignore\n".into(),
                origin: Origin::ExcludesFile,
            },
        );
        let paths: Vec<String> = [
            "a",
            "deep/a",
            "name",
            "deep/name",
            "name2",
            "dir/a",
            "deep/dir/a",
            "hidden",
            "deep/hidden",
            "hidden2",
            "a.log",
            "keep.log",
            "deep/a.log",
            "deep/keep.log",
            "target/doc/a",
            "target/drop/a",
            "target/file",
            "space name",
            "deep/space name",
            "#secret",
            "deep/#secret",
            "a1.tmp",
            "b2.tmp",
            "c3.tmp",
            "x/a",
            "deep/x/a",
            "sub/root",
            "sub/deep/root",
            "root",
            "sub/build/a",
            "sub/deep/build/a",
            "build/a",
            "sub/a.bak",
            "sub/keep.bak",
            "sub/deep/a.bak",
            "sub/deep/keep.bak",
            "a.bak",
            "README",
            "sub/README",
            "deep/file",
            ".env",
            "deep/.env",
            "private.pem",
            "deep/private.pem",
            "xf",
            "logs/a.txt",
            "logs/sub/a",
            "a.global-ignore",
            "deep/a.global-ignore",
            "logs/.gitkeep",
            "logs/drop",
            "logs/sub/drop",
            "secret{1}",
            "secret1",
            "a.{pem,key}",
            "a.pem",
            "comma,name",
            "deep/comma,name",
            "comma",
            "name,comma",
            "double\"quote",
            "deep/double\"quote",
            "doublequote",
            "app/[slug]/secret.json",
            "app/[slug]/root",
            "app/other/secret.json",
            "app/*/secret.json",
            "app/?/secret.json",
            "app/\\/secret.json",
            "app/{}/secret.json",
            "deploy/README",
            "deploy/keep[1]{a}",
            "deploy/keep1a",
            "deploy/public.key",
            "public.key",
            "tracked-target/keep",
            "tracked-target/doc/a",
            "tracked-target/doc/drop",
            "else/tracked-target/keep",
            "app/tracked-target/main.rs",
            "module/.env",
            "module/node_modules/a",
            "module/README",
            "module/build/x",
            "nested/.env",
            "nested/node_modules/a",
            "nested/README",
            "nested/build/x",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        for p in &paths {
            let path = root.join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "data").unwrap();
        }
        let tracked: Vec<String> = [
            "deploy/README",
            "deploy/keep[1]{a}",
            "tracked-target/keep",
            "tracked-target/doc/a",
            "a.log",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        for path in &tracked {
            assert!(git(root, &["add", "-f", "--", path]).status.success());
        }
        for dir in ["module", "nested"] {
            assert!(git(&root.join(dir), &["init", "-q"]).status.success());
            assert!(git(&root.join(dir), &["add", ".gitignore", "README", "build/x"])
                .status
                .success());
        }
        assert!(git(
            root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "160000,1111111111111111111111111111111111111111,module"
            ]
        )
        .status
        .success());
        (sources, paths, tracked)
    }

    fn git(root: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(["-c", "core.fsmonitor=false"])
            .args(args)
            .current_dir(root)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root)
            .output()
            .unwrap()
    }

    fn table_git_ignored(root: &Path, path: &str, tracked: &[String]) -> bool {
        if tracked.iter().any(|p| p == path) {
            return false;
        }
        let (cwd, path) = match path.split_once('/') {
            Some((dir @ ("module" | "nested"), rest)) => (root.join(dir), rest),
            _ => (root.to_owned(), path),
        };
        let output = git(&cwd, &["check-ignore", "--no-index", "-q", "--", path]);
        assert!(
            matches!(output.status.code(), Some(0 | 1)),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.status.success()
    }

    fn table_translation(sources: &[IgnoreSource], tracked: &[String]) -> Translated {
        let outer: Vec<_> = sources
            .iter()
            .filter(|s| s.dir != "module" && s.dir != "nested")
            .cloned()
            .collect();
        let mut translated = translate(&outer, tracked);
        for source in sources.iter().filter(|s| s.dir == "module" || s.dir == "nested") {
            let nested = translate(&[src("", &source.text)], &[]);
            scope_repository(&mut translated.patterns, &source.dir, &nested.patterns).unwrap();
        }
        Translated::new(translated.patterns, translated.problems)
    }

    #[test]
    fn translation_and_matcher_match_git() {
        let scratch = Scratch::new();
        let root = scratch.0.join("alpha");
        let (sources, paths, tracked) = fixture(&root);
        let matcher = Matcher::new(&table_translation(&sources, &tracked).patterns);
        for path in paths {
            assert_eq!(
                matcher.ignored(&path, false),
                table_git_ignored(&root, &path, &tracked),
                "{path}"
            );
        }
    }

    #[test]
    #[ignore = "requires EITRI_TEST_MUTAGEN and a private local Mutagen daemon"]
    fn translation_and_matcher_match_git_and_mutagen() {
        let binary = std::env::var_os("EITRI_TEST_MUTAGEN").expect("EITRI_TEST_MUTAGEN is required");
        assert!(Path::new(&binary).is_absolute());
        let scratch = Scratch::new();
        let alpha = scratch.0.join("alpha");
        let beta = scratch.0.join("beta");
        std::fs::create_dir(&alpha).unwrap();
        std::fs::create_dir(&beta).unwrap();
        let (sources, paths, tracked) = fixture(&alpha);
        let translated = table_translation(&sources, &tracked);
        let run = |args: &[String]| {
            Command::new(&binary)
                .args(args)
                .env("MUTAGEN_DATA_DIRECTORY", scratch.0.join("daemon"))
                .output()
                .unwrap()
        };
        struct Daemon<'a>(&'a dyn Fn(&[String]) -> std::process::Output);
        impl Drop for Daemon<'_> {
            fn drop(&mut self) {
                (self.0)(&["sync".into(), "terminate".into(), "--all".into()]);
                (self.0)(&["daemon".into(), "stop".into()]);
            }
        }
        let _daemon = Daemon(&run);
        let mut args: Vec<String> = [
            "sync",
            "create",
            "--name",
            "ignore-table",
            "--sync-mode",
            "one-way-replica",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        for pattern in std::iter::once(".git").chain(translated.patterns.iter().map(String::as_str)) {
            // Mutagen parses each ignore value as CSV, so commas and quotes need CSV escaping.
            args.extend(["--ignore".into(), format!("\"{}\"", pattern.replace('"', "\"\""))]);
        }
        args.extend([alpha.to_str().unwrap().into(), beta.to_str().unwrap().into()]);
        let created = run(&args);
        assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
        let flushed = run(&["sync".into(), "flush".into(), "ignore-table".into()]);
        assert!(flushed.status.success(), "{}", String::from_utf8_lossy(&flushed.stderr));
        let matcher = Matcher::new(&translated.patterns);
        for path in paths {
            let expected = table_git_ignored(&alpha, &path, &tracked);
            assert_eq!(matcher.ignored(&path, false), expected, "matcher {path}");
            assert_eq!(!beta.join(&path).exists(), expected, "Mutagen {path}");
        }
    }
}
