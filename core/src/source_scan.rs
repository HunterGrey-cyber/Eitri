//! Reading the workspace's own sources as text, for tests that hold an invariant by scanning code.
//!
//! A scanner that reads a path which no longer holds the code it guards passes on whatever is left. Every
//! reader here fails instead: a listed directory that is missing, unreadable or holds no file of the kind
//! asked for, a named file that cannot be read, and an anchor -- a definition the scan exists to cover --
//! that none of the files read contains. Paths are relative to the workspace root, so a test in any crate
//! names the same directories, and moving code between crates means editing one list, not a path per test.

use std::path::{Path, PathBuf};

/// One file a scan read: its path relative to the workspace root (`/`-separated) and its text.
#[derive(Debug, Clone)]
pub struct Source {
    pub path: String,
    pub text: String,
}

/// The directory holding the workspace's `Cargo.toml`.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("eitri-core sits one level below the workspace root")
        .to_path_buf()
}

/// Every file under each of `dirs` (relative to the workspace root), recursively, whose extension is one
/// of `extensions`, sorted by path.
///
/// # Panics
/// When `dirs` is empty, a directory is missing or unreadable, one of them holds no such file, or a file
/// cannot be read as UTF-8.
pub fn sources(dirs: &[&str], extensions: &[&str]) -> Vec<Source> {
    assert!(!dirs.is_empty(), "a scan with no directories reads nothing");
    let root = workspace_root();
    let mut out = Vec::new();
    for dir in dirs {
        let mut found = Vec::new();
        collect(&root.join(dir), extensions, &mut found);
        assert!(
            !found.is_empty(),
            "{dir} holds no {extensions:?} file: the scan's directory list is out of date"
        );
        for path in found {
            out.push(read_at(&root, &path));
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// [`sources`] over `.rs` files.
pub fn rust_sources(dirs: &[&str]) -> Vec<Source> {
    sources(dirs, &["rs"])
}

/// One file, relative to the workspace root.
///
/// # Panics
/// When it cannot be read.
pub fn read_required(path: &str) -> Source {
    let root = workspace_root();
    read_at(&root, &root.join(path))
}

/// The one file of `sources` whose code (comments and literals blanked, [`code_only`]) contains `needle`.
///
/// # Panics
/// When no file does, or more than one does.
pub fn file_with<'a>(sources: &'a [Source], needle: &str) -> &'a Source {
    let hits: Vec<&Source> = sources.iter().filter(|s| code_only(&s.text).contains(needle)).collect();
    match hits.as_slice() {
        [one] => one,
        [] => panic!("no scanned file contains `{needle}` (read: {})", read_dirs(sources)),
        many => panic!(
            "`{needle}` is in more than one file: {}",
            many.iter().map(|s| s.path.as_str()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// # Panics
/// Unless each of `anchors` appears in the code ([`code_only`]) of at least one of `sources`.
pub fn require_anchors(sources: &[Source], anchors: &[&str]) {
    let code: Vec<String> = sources.iter().map(|s| code_only(&s.text)).collect();
    let missing: Vec<&str> = anchors
        .iter()
        .copied()
        .filter(|anchor| !code.iter().any(|c| c.contains(anchor)))
        .collect();
    assert!(
        missing.is_empty(),
        "the scan never saw {missing:?}: the code it guards is not in the directories it reads ({})",
        read_dirs(sources)
    );
}

fn collect(dir: &Path, extensions: &[&str], into: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry
            .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
            .path();
        if path.is_dir() {
            collect(&path, extensions, into);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| extensions.contains(&e))
        {
            into.push(path);
        }
    }
}

fn read_at(root: &Path, path: &Path) -> Source {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let rel = path.strip_prefix(root).unwrap_or(path);
    Source {
        path: rel.to_string_lossy().replace('\\', "/"),
        text,
    }
}

fn read_dirs(sources: &[Source]) -> String {
    let mut dirs: Vec<&str> = sources
        .iter()
        .map(|s| s.path.rsplit_once('/').map_or(s.path.as_str(), |(dir, _)| dir))
        .collect();
    dirs.sort_unstable();
    dirs.dedup();
    dirs.join(", ")
}

/// `text` with every comment, string literal and character literal replaced by spaces (line
/// breaks kept), so neither a brace nor a word inside one counts.
pub fn code_only(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let blank = |c: char| if c == '\n' { '\n' } else { ' ' };
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        if c == '/' && next == Some('*') {
            let mut depth = 0;
            while i < chars.len() {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    out.extend([' ', ' ']);
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    out.extend([' ', ' ']);
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    out.push(blank(chars[i]));
                    i += 1;
                }
            }
            continue;
        }
        let starts_word = i == 0 || !ident(chars[i - 1]) || (chars[i - 1] == 'b' && (i < 2 || !ident(chars[i - 2])));
        if c == 'r' && starts_word {
            let mut j = i + 1;
            while chars.get(j) == Some(&'#') {
                j += 1;
            }
            if chars.get(j) == Some(&'"') {
                let hashes = j - i - 1;
                out.extend(std::iter::repeat_n(' ', j - i + 1));
                i = j + 1;
                while i < chars.len() {
                    if chars[i] == '"' && (1..=hashes).all(|k| chars.get(i + k) == Some(&'#')) {
                        out.extend(std::iter::repeat_n(' ', hashes + 1));
                        i += hashes + 1;
                        break;
                    }
                    out.push(blank(chars[i]));
                    i += 1;
                }
                continue;
            }
        }
        if c == '"' {
            out.push(' ');
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    out.push(' ');
                    i += 1;
                }
                if i < chars.len() {
                    out.push(blank(chars[i]));
                    i += 1;
                }
            }
            out.push(' ');
            i += 1;
            continue;
        }
        if c == '\'' {
            if next == Some('\\') {
                let mut j = i + 2;
                while j < chars.len() && chars[j] != '\'' {
                    j += 1;
                }
                out.extend(std::iter::repeat_n(' ', j + 1 - i));
                i = j + 1;
                continue;
            }
            if chars.get(i + 2) == Some(&'\'') {
                out.extend([' ', ' ', ' ']);
                i += 3;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out.into_iter().collect()
}

/// The byte positions just after every whole-word occurrence of `word`.
pub fn word_ends(code: &str, word: &str) -> Vec<usize> {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    code.match_indices(word)
        .filter(|(at, _)| !code[..*at].chars().next_back().is_some_and(ident))
        .map(|(at, w)| at + w.len())
        .filter(|end| !code[*end..].chars().next().is_some_and(ident))
        .collect()
}

/// The text from the `{` at `open` to its matching `}`, inclusive.
pub fn braced(code: &str, open: usize) -> &str {
    let mut depth = 0usize;
    for (at, c) in code[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &code[open..=open + at];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces from byte {open}");
}

/// The body of every `fn NAME`.
pub fn functions<'a>(code: &'a str, name: &str) -> Vec<&'a str> {
    word_ends(code, "fn")
        .into_iter()
        .filter_map(|after_fn| {
            let rest = &code[after_fn..];
            let trimmed = rest.trim_start();
            if !trimmed.starts_with(name)
                || trimmed[name.len()..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
            {
                return None;
            }
            let start = after_fn + (rest.len() - trimmed.len());
            let open = start + code[start..].find('{')?;
            Some(braced(code, open))
        })
        .collect()
}

/// The body of every `mod NAME`.
pub fn modules<'a>(code: &'a str, name: &str) -> Vec<&'a str> {
    word_ends(code, "mod")
        .into_iter()
        .filter_map(|after| {
            let rest = &code[after..];
            let trimmed = rest.trim_start();
            let is_name = trimmed.starts_with(name)
                && !trimmed[name.len()..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
            if !is_name {
                return None;
            }
            let start = after + (rest.len() - trimmed.len()) + name.len();
            let next = code[start..].trim_start();
            next.starts_with('{')
                .then(|| braced(code, start + (code[start..].len() - next.len())))
        })
        .collect()
}

/// `code` with every one of `bodies` (slices of `code`) blanked.
pub fn without(code: &str, bodies: &[&str]) -> String {
    let mut out = code.to_owned();
    for body in bodies {
        let start = body.as_ptr() as usize - code.as_ptr() as usize;
        out.replace_range(start..start + body.len(), &" ".repeat(body.len()));
    }
    out
}

/// Every call of `name` in `code`: the word followed (after blanks) by `(`, and not a definition
/// (`fn name`) or a method of something else (`.name(`).
pub fn calls(code: &str, name: &str) -> usize {
    word_ends(code, name)
        .into_iter()
        .filter(|end| code[*end..].trim_start().starts_with('('))
        .filter(|end| {
            let before = code[..end - name.len()].trim_end();
            !before.ends_with('.') && !before.ends_with("fn")
        })
        .count()
}

#[cfg(test)]
mod walker_tests {
    use super::*;

    #[test]
    #[should_panic(expected = "cannot read")]
    fn a_missing_directory_fails() {
        rust_sources(&["core/no-such-directory"]);
    }

    #[test]
    #[should_panic(expected = "holds no")]
    fn a_directory_without_such_files_fails() {
        rust_sources(&["docs/keymap"]);
    }

    #[test]
    #[should_panic(expected = "never saw")]
    fn a_missing_anchor_fails() {
        let read = rust_sources(&["core/src/theme"]);
        require_anchors(&read, &["fn no_such_function_anywhere("]);
    }

    #[test]
    #[should_panic(expected = "no scanned file contains")]
    fn file_with_fails_on_no_file() {
        let read = rust_sources(&["core/src/theme"]);
        file_with(&read, "fn no_such_function_anywhere(");
    }

    #[test]
    #[should_panic(expected = "more than one file")]
    fn file_with_fails_on_two_files() {
        let read = rust_sources(&["core/src"]);
        file_with(&read, "use ");
    }

    #[test]
    #[should_panic(expected = "cannot read")]
    fn a_missing_required_file_fails() {
        read_required("core/src/no_such_file.rs");
    }

    #[test]
    fn the_paths_are_relative_to_the_workspace_root() {
        let read = rust_sources(&["core/src"]);
        assert!(
            read.iter().any(|s| s.path == "core/src/lib.rs"),
            "{:?}",
            read.first().map(|s| &s.path)
        );
        let lib = file_with(&read, "pub mod source_scan");
        assert_eq!(lib.path, "core/src/lib.rs");
        require_anchors(&read, &["pub mod source_scan"]);
        assert!(read_required("Cargo.toml").text.contains("[workspace]"));
    }

    #[test]
    fn an_anchor_inside_a_comment_or_a_string_does_not_count() {
        let read = vec![Source {
            path: "x.rs".into(),
            text: "// fn hidden(\nlet s = \"fn hidden(\";".into(),
        }];
        let result = std::panic::catch_unwind(|| require_anchors(&read, &["fn hidden("]));
        assert!(result.is_err());
    }
}
