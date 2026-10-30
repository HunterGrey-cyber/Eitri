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

/// The unique function named `name`, with comments, literals and test modules blanked.
///
/// # Panics
/// When no function or more than one function matches, naming the files searched or matched.
pub fn function_in<'a>(sources: &'a [Source], name: &str) -> (&'a Source, String) {
    function_body(sources, name, false)
}

/// The unique function named `name`, retaining its original comments and literals.
///
/// # Panics
/// When no function or more than one function matches, naming the files searched or matched.
pub fn function_in_raw<'a>(sources: &'a [Source], name: &str) -> (&'a Source, String) {
    function_body(sources, name, true)
}

fn function_body<'a>(sources: &'a [Source], name: &str, raw: bool) -> (&'a Source, String) {
    let mut hits = Vec::new();
    for source in sources {
        let code = code_only(&source.text);
        let code = without(&code, &modules(&code, "tests"));
        for body in functions(&code, name) {
            let start = body.as_ptr() as usize - code.as_ptr() as usize;
            let text = if raw {
                &source.text[start..start + body.len()]
            } else {
                body
            };
            hits.push((source, text.to_owned()));
        }
    }
    assert!(
        hits.len() == 1,
        "expected one function `{name}`, found {} (read: {}; matched: {})",
        hits.len(),
        sources.iter().map(|s| s.path.as_str()).collect::<Vec<_>>().join(", "),
        hits.iter().map(|(s, _)| s.path.as_str()).collect::<Vec<_>>().join(", "),
    );
    hits.pop().unwrap()
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
/// breaks and byte offsets kept), so neither a brace nor a word inside one counts.
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
    // A blanked Unicode character still occupies its original number of bytes for raw slices.
    chars
        .into_iter()
        .zip(out)
        .fold(String::new(), |mut text, (original, masked)| {
            if masked == ' ' {
                text.extend(std::iter::repeat_n(' ', original.len_utf8()));
            } else {
                text.push(masked);
            }
            text
        })
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

/// The bodies of inherent and trait `impl` blocks whose self type is named `type_name`.
/// Pass [`code_only`] text so comments and literals cannot contribute braces or names. Generic arguments,
/// bounds and trait names do not count as the self type; qualified self types use their final path segment.
pub fn impl_blocks<'a>(code: &'a str, type_name: &str) -> Vec<&'a str> {
    word_ends(code, "impl")
        .into_iter()
        .filter_map(|after| {
            let rest = &code[after..];
            let mut angles = 0usize;
            let mut groups = 0usize;
            let mut header_end = None;
            for (at, c) in rest.char_indices() {
                match c {
                    '<' => angles += 1,
                    '>' if angles > 0 && !rest[..at].ends_with('-') => angles -= 1,
                    '(' | '[' => groups += 1,
                    ')' | ']' if groups > 0 => groups -= 1,
                    '{' if angles == 0 && groups == 0 => {
                        header_end = Some(at);
                        break;
                    }
                    _ => {}
                }
            }
            let open = after + header_end?;
            let mut header = code[after..open].trim_start();
            if header.starts_with('<') {
                let mut depth = 0;
                let end = header.char_indices().find_map(|(at, c)| {
                    match c {
                        '<' => depth += 1,
                        '>' if !header[..at].ends_with('-') => depth -= 1,
                        _ => {}
                    }
                    (depth == 0).then_some(at + c.len_utf8())
                })?;
                header = header[end..].trim_start();
            }
            // Only top-level `for` and `where` delimit the self type: a bound may mention either word.
            let mut depth = 0usize;
            let mut self_start = 0;
            let mut self_end = header.len();
            let wheres = word_ends(header, "where");
            let fors = word_ends(header, "for");
            for (at, c) in header.char_indices() {
                match c {
                    '<' | '(' | '[' => depth += 1,
                    '>' if depth > 0 && !header[..at].ends_with('-') => depth -= 1,
                    ')' | ']' if depth > 0 => depth -= 1,
                    _ => {}
                }
                if depth == 0 && wheres.contains(&(at + 5)) {
                    self_end = at;
                    break;
                }
                if depth == 0 && fors.contains(&(at + 3)) {
                    self_start = at + 3;
                }
            }
            let self_type = header[self_start..self_end].trim();
            let path_end = self_type.find('<').unwrap_or(self_type.len());
            let name = self_type[..path_end].rsplit("::").next()?.trim();
            (name == type_name).then(|| braced(code, open))
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
    fn impl_blocks_finds_only_the_named_self_type_across_headers_and_bodies() {
        let code = code_only(
            r#"
            impl Driver { fn first() { if true { nested(); } } }
            impl<T: Decide> Driver<T> where T: Other { fn second() {} }
            impl<T> HostSink for crate::wm::Driver<T> { fn third() {} }
            impl Controller { fn fourth() {} }
            impl DriverExtra { fn wrong() {} }
            impl Driver for Other { fn wrong() {} }
            impl Wrapper<Driver> { fn wrong() {} }
            impl<T: Trait<Driver>> Other<T> { fn wrong() {} }
            impl<T: Fn() -> Driver> Other<T> { fn wrong() {} }
            // impl Driver { fn wrong() {} }
            const TEXT: &str = "impl Driver { fn wrong() {} }";
            "#,
        );
        let blocks = impl_blocks(&code, "Driver");
        assert_eq!(blocks.len(), 3);
        for (body, name) in blocks.iter().zip(["first", "second", "third"]) {
            assert_eq!(functions(body, name).len(), 1);
            assert!(functions(body, "wrong").is_empty());
        }
        assert_eq!(impl_blocks(&code, "Controller").len(), 1);
        assert!(impl_blocks(&code, "Missing").is_empty());
    }

    #[test]
    fn impl_blocks_uses_braces_instead_of_line_boundaries() {
        for code in [
            "impl Driver{fn kept(){}}impl Driver{fn gone(){}}",
            "impl\nDriver\n{\nfn\nkept()\n{}\n}\nimpl\nDriver\n{fn gone(){}}",
            "impl<T> SomeTrait<T> for Driver<T> where T: Bound {fn kept(){}} impl Driver{fn gone(){}}",
            "impl<const N: usize> Driver<{ N + 1 }> {fn kept(){}} impl Driver{fn gone(){}}",
            "impl<T: Fn() -> ()> Driver<T> {fn kept(){}} impl Driver{fn gone(){}}",
        ] {
            let code = code_only(code);
            let blocks = impl_blocks(&code, "Driver");
            assert_eq!(blocks.len(), 2, "{code}");
            assert_eq!(functions(blocks[0], "kept").len(), 1);
            assert_eq!(functions(blocks[1], "gone").len(), 1);
        }
    }

    #[test]
    fn function_in_finds_one_function_across_files_and_refuses_two() {
        let mut sources = vec![
            Source {
                path: "a.rs".into(),
                text: "// é\nfn other() {}\n#[cfg(test)] mod tests { fn wanted() {} }".into(),
            },
            Source {
                path: "b.rs".into(),
                text: "// é\nfn wanted() { let text = \"héllo\"; }".into(),
            },
        ];
        let (file, body) = function_in(&sources, "wanted");
        assert_eq!(file.path, "b.rs");
        assert!(body.contains("let text ="));
        assert!(!body.contains("héllo"));
        let (raw_file, raw) = function_in_raw(&sources, "wanted");
        assert_eq!(raw_file.path, file.path);
        assert_eq!(raw, "{ let text = \"héllo\"; }");
        assert_eq!(body.len(), raw.len());
        assert!(std::panic::catch_unwind(|| function_in(&sources, "missing")).is_err());
        sources[0].text.push_str("\nfn wanted() {}");
        let failure = std::panic::catch_unwind(|| function_in(&sources, "wanted")).unwrap_err();
        let message = failure.downcast_ref::<String>().unwrap();
        assert!(message.contains("a.rs") && message.contains("b.rs"));
        sources[0].text.push_str("\nfn other() {}");
        assert!(std::panic::catch_unwind(|| function_in(&sources, "other")).is_err());
    }

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
