//! Where a project's Rust files define a name, found by reading their text: the one fallback `gd` has for a name a
//! language server does not answer for. rust-analyzer's `workspace/symbol` misses short and common function names in
//! a large workspace, and holds every request for the seconds its workspace takes to load; a definition line is plain
//! enough to find without it.
//!
//! The files are the project's own listing ([`crate::path_complete::list_paths`]: git's, else a bounded walk that
//! follows no link; either stops at its cap, and a cut listing is silently partial), only `*.rs`, each checked to be a regular file before it is opened
//! (without following a link or waiting on a FIFO), under no directory that is a link, and read up to a cap. Only
//! code is read: comments, strings and character literals are blanked over the whole file first
//! ([`crate::rust_mask::code_only`]), so a definition line inside a block comment or a raw string is none. The name
//! is compared as text, never compiled into a pattern. This lists and reads files and belongs on a worker thread: the
//! reading stops 1 s after the search began, but the listing comes first and is not cut short by the deadline (git's
//! may take up to 10 s), and the deadline is checked between files.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

/// How much of the project one search reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most `*.rs` files read.
    pub max_files: usize,
    /// How long the search reads for, counted from its start, the listing included: a listing that took this long
    /// leaves no file read. It is checked between files, and the listing itself is not cut short.
    pub deadline: Duration,
    /// The most bytes read of one file; a line the cap cuts is not read.
    pub max_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_files: 20_000,
            deadline: Duration::from_secs(1),
            max_bytes: 1_000_000,
        }
    }
}

/// The most definitions one search returns.
pub const MAX_FOUND: usize = 50;

/// One definition line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Relative to the project root, as the listing names it.
    pub path: String,
    /// One-based.
    pub line: u32,
    /// One-based, in characters, where the name starts.
    pub col: u32,
}

/// The items whose definition line names what follows them.
const KINDS: &[&str] = &[
    "fn", "struct", "enum", "trait", "type", "const", "static", "mod", "union",
];
/// Words that may stand between the visibility and the item's kind.
const QUALIFIERS: &[&str] = &["async", "const", "unsafe", "default", "extern"];

/// The definitions of `name` in the project's Rust files.
pub fn search(root: &Path, name: &str, limits: Limits) -> Vec<Found> {
    search_within(root, name, None, limits)
}

/// [`search`], keeping with `within` only the definitions whose container has that name: the innermost `impl`
/// (its self type's last segment), `trait` or inline `mod` block around the line, else the file's own module
/// (`a` for `a.rs` and `a/mod.rs`; none for `lib.rs` and `main.rs`). So `Foo::new` keeps `new` of `impl Foo`, and of
/// `impl Trait for Foo`, and not `new` of another type.
pub fn search_within(root: &Path, name: &str, within: Option<&str>, limits: Limits) -> Vec<Found> {
    let started = Instant::now();
    if name.is_empty() {
        return Vec::new();
    }
    let listing = crate::path_complete::list_paths(root);
    scan(root, &listing, name, within, limits, started)
}

/// [`search`] over a listing already made, its time counted from `started`.
pub fn search_in(root: &Path, listing: &[String], name: &str, limits: Limits, started: Instant) -> Vec<Found> {
    scan(root, listing, name, None, limits, started)
}

fn scan(
    root: &Path,
    listing: &[String],
    name: &str,
    within: Option<&str>,
    limits: Limits,
    started: Instant,
) -> Vec<Found> {
    let mut found = Vec::new();
    if name.is_empty() {
        return found;
    }
    let files = listing
        .iter()
        .filter(|entry| entry.ends_with(".rs") && !entry.ends_with('/'));
    let mut judged = HashMap::new();
    for entry in files.take(limits.max_files) {
        if started.elapsed() >= limits.deadline || found.len() >= MAX_FOUND {
            break;
        }
        if under_link(root, entry, &mut judged) {
            continue;
        }
        let Some(bytes) = read_capped(&root.join(entry), limits.max_bytes) else {
            continue;
        };
        // A definition line names the item in ASCII words, so a quick look for the name's bytes skips most files.
        if !contains(&bytes, name.as_bytes()) {
            continue;
        }
        let cut = bytes.len() as u64 >= limits.max_bytes;
        let text = String::from_utf8_lossy(&bytes);
        // The same text with comments and literals blanked: every character keeps its byte length, so a line's
        // offsets in one are its offsets in the other.
        let code = crate::rust_mask::code_only(&text);
        if code.len() != text.len() {
            continue;
        }
        let mut containers = None;
        let mut lines = Vec::new();
        let mut start = 0;
        for line in text.split('\n') {
            lines.push((start, line.strip_suffix('\r').unwrap_or(line).len()));
            start += line.len() + 1;
        }
        // The last piece has no end of line: at the cap it may be any part of a longer line.
        if cut {
            lines.pop();
        }
        for (index, (start, len)) in lines.into_iter().enumerate() {
            let line = &code[start..start + len];
            if !line.contains(name) {
                continue;
            }
            if let Some(at) = definition_at(line, name) {
                if let Some(within) = within {
                    let containers = containers.get_or_insert_with(|| Containers::of(&code, entry));
                    if containers.at(start + at) != Some(within) {
                        continue;
                    }
                }
                let (Ok(number), Ok(col)) = (
                    u32::try_from(index + 1),
                    u32::try_from(text[start..start + at].chars().count() + 1),
                ) else {
                    continue;
                };
                found.push(Found {
                    path: entry.clone(),
                    line: number,
                    col,
                });
                if found.len() >= MAX_FOUND {
                    break;
                }
            }
        }
    }
    found
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|window| window == needle)
}

/// Whether a directory between `root` and `entry` is a link, or no directory at all. A listing names paths as they
/// were (git's index in particular), and a directory since replaced by a link would take the read outside the
/// project; the file itself is judged when it is read. Each directory is judged once per search.
fn under_link(root: &Path, entry: &str, judged: &mut HashMap<String, bool>) -> bool {
    entry.match_indices('/').any(|(at, _)| {
        let dir = &entry[..at];
        if let Some(linked) = judged.get(dir) {
            return *linked;
        }
        let linked = !std::fs::symlink_metadata(root.join(dir)).is_ok_and(|meta| meta.is_dir());
        judged.insert(dir.to_owned(), linked);
        linked
    })
}

/// The blocks of one file's code that give what they hold a container name, and the file's own module name.
struct Containers {
    /// Where each block's `{` is, where its `}` is (the end of the text when it has none), and its name.
    blocks: Vec<(usize, usize, String)>,
    file: Option<String>,
}

impl Containers {
    /// `code` is the file's text with comments and literals blanked, `path` its name in the listing.
    fn of(code: &str, path: &str) -> Containers {
        let mut blocks = Vec::new();
        for keyword in ["impl", "trait", "mod"] {
            for (at, _) in code.match_indices(keyword) {
                let after = at + keyword.len();
                // A keyword inside a word is none, and `impl` after `->`, `:` or `,` is a type, not a block.
                let joined = code[..at].chars().next_back().is_some_and(is_word)
                    || code[after..].chars().next().is_some_and(is_word);
                let typed = code[..at]
                    .trim_end()
                    .chars()
                    .next_back()
                    .is_some_and(|c| ":,(<>&=+|".contains(c));
                if joined || typed {
                    continue;
                }
                let Some(open) = header_end(&code[after..]).map(|end| after + end) else {
                    continue;
                };
                let header = &code[after..open];
                let name = if keyword == "impl" {
                    self_type(header)
                } else {
                    word(header.trim_start()).0.to_owned()
                };
                if !name.is_empty() {
                    blocks.push((open, block_end(code, open), name));
                }
            }
        }
        let file = Path::new(path);
        let file = match file.file_stem().and_then(|stem| stem.to_str()) {
            Some("lib" | "main") => None,
            Some("mod") => file.parent().and_then(Path::file_name).and_then(|dir| dir.to_str()),
            stem => stem,
        };
        Containers {
            blocks,
            file: file.map(str::to_owned),
        }
    }

    /// The container of what is defined at `offset`.
    fn at(&self, offset: usize) -> Option<&str> {
        self.blocks
            .iter()
            .filter(|(open, close, _)| *open < offset && offset < *close)
            .max_by_key(|(open, ..)| *open)
            .map(|(.., name)| name.as_str())
            .or(self.file.as_deref())
    }
}

/// Where the header after `impl`, `trait` or `mod` ends at its block's `{`; `None` when a `;` ends the item first.
fn header_end(rest: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (at, c) in rest.char_indices() {
        match c {
            '<' | '(' | '[' => depth += 1,
            '>' if depth > 0 && !rest[..at].ends_with('-') => depth -= 1,
            ')' | ']' if depth > 0 => depth -= 1,
            '{' if depth == 0 => return Some(at),
            ';' | '}' if depth == 0 => return None,
            _ => {}
        }
    }
    None
}

/// Where the block opened at `open` closes, or the end of `code`.
fn block_end(code: &str, open: usize) -> usize {
    let mut depth = 0usize;
    for (at, c) in code[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return open + at;
                }
            }
            _ => {}
        }
    }
    code.len()
}

/// The last path segment of an `impl` header's self type: the last word before a `where` and after a `for`, read
/// from the header with everything inside `<>`, `()` and `[]` left out (the impl's generics, the type's arguments).
fn self_type(header: &str) -> String {
    let mut top = String::new();
    let mut depth = 0usize;
    for (at, c) in header.char_indices() {
        match c {
            '<' | '(' | '[' => depth += 1,
            '>' if depth > 0 && !header[..at].ends_with('-') => depth -= 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => top.push(c),
            _ => {}
        }
    }
    let words: Vec<&str> = top.split_whitespace().take_while(|word| *word != "where").collect();
    let after_for = words.iter().rposition(|word| *word == "for").map_or(0, |at| at + 1);
    let ty = words[after_for..].last().copied().unwrap_or("");
    let ty = ty.rsplit("::").next().unwrap_or("");
    ty.trim_start_matches(|c: char| !is_word(c)).to_owned()
}

/// A file's first `max` bytes. A link is not followed, a FIFO or device is never opened or waited on: only a regular
/// file is read, judged by its own entry before it is opened and again on the open handle.
fn read_capped(path: &Path, max: u64) -> Option<Vec<u8>> {
    read_capped_via(path, max, open_plain)
}

fn open_plain(path: &Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(path)
        .ok()
}

/// [`read_capped`] with the open given, so a test can see whether it was reached.
fn read_capped_via(path: &Path, max: u64, open: impl FnOnce(&Path) -> Option<std::fs::File>) -> Option<Vec<u8>> {
    if !std::fs::symlink_metadata(path).ok()?.file_type().is_file() {
        return None;
    }
    let file = open(path)?;
    if !file.metadata().ok()?.file_type().is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(max).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The word at the start of `text` and what follows it.
fn word(text: &str) -> (&str, &str) {
    let end = text.find(|c: char| !is_word(c)).unwrap_or(text.len());
    text.split_at(end)
}

/// `text` past at least one whitespace character, or `None` when it does not start with one.
fn spaced(text: &str) -> Option<&str> {
    let rest = text.trim_start();
    (rest.len() < text.len()).then_some(rest)
}

/// Where `name` starts in `text` when `text` holds `name` right there, ending where a word ends.
fn names(text: &str, name: &str) -> bool {
    text.strip_prefix(name)
        .is_some_and(|after| !after.chars().next().is_some_and(is_word))
}

/// The byte offset of `name` in `line` when the line defines it: an item (`fn`, `struct`, `enum`, `trait`, `type`,
/// `const`, `static`, `mod`, `union`) at the line's start, after an optional `pub`/`pub(...)` and any of `async`,
/// `const`, `unsafe`, `default`, `extern "abi"`; or `macro_rules! name` anywhere on it. The name ends where a word
/// ends; it is compared as text, whatever characters it holds.
pub fn definition_at(line: &str, name: &str) -> Option<usize> {
    if name.is_empty() {
        return None;
    }
    item_at(line, name).or_else(|| macro_at(line, name))
}

fn item_at(line: &str, name: &str) -> Option<usize> {
    let mut rest = line.trim_start();
    if let Some(after) = rest.strip_prefix("pub") {
        let after = match after.strip_prefix('(') {
            Some(inside) => &inside[inside.find(')')? + 1..],
            None => after,
        };
        rest = spaced(after)?;
    }
    // A kind or a qualifier, then whitespace; at most a few, which a real line never passes.
    for _ in 0..6 {
        let (head, after) = word(rest);
        if head.is_empty() {
            return None;
        }
        let after = spaced(after);
        if KINDS.contains(&head) {
            if let Some(next) = after {
                if names(next, name) {
                    return Some(line.len() - next.len());
                }
            }
        }
        if !QUALIFIERS.contains(&head) {
            return None;
        }
        let mut next = after?;
        // `extern "C" fn`: the ABI's string, when there is one.
        if head == "extern" {
            if let Some(quoted) = next.strip_prefix('"') {
                next = spaced(&quoted[quoted.find('"')? + 1..])?;
            }
        }
        rest = next;
    }
    None
}

fn macro_at(line: &str, name: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(at) = line[from..].find("macro_rules!") {
        let after = &line[from + at + "macro_rules!".len()..];
        if let Some(next) = spaced(after) {
            if names(next, name) {
                return Some(line.len() - next.len());
            }
        }
        from += at + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_scratch_dir::ScratchDir;

    #[test]
    fn every_item_kind_defines_its_name() {
        for kind in KINDS {
            let line = format!("{kind} wanted_name");
            assert_eq!(definition_at(&line, "wanted_name"), Some(kind.len() + 1), "{line}");
        }
        for line in [
            "    pub fn wanted(&self) {}",
            "pub(crate) fn wanted() {}",
            "pub(in crate::a) struct wanted;",
            "pub(super) const wanted: u8 = 1;",
            "\tasync fn wanted() {}",
            "pub const fn wanted() -> u8 { 1 }",
            "pub unsafe trait wanted {}",
            "pub(crate) async unsafe fn wanted() {}",
            "extern \"C\" fn wanted() {}",
            "pub type wanted<T> = Vec<T>;",
            "mod wanted;",
            "static wanted: u8 = 0;",
            "union wanted { a: u8 }",
            "enum wanted {",
        ] {
            assert!(definition_at(line, "wanted").is_some(), "{line}");
        }
    }

    #[test]
    fn what_only_uses_or_mentions_the_name_defines_nothing() {
        for line in [
            "let wanted = 1;",
            "wanted();",
            "fn wanted_more() {}",
            "fn other(wanted: u8) {}",
            "use crate::wanted;",
            "impl wanted {}",
            "pubfn wanted() {}",
            "pub(crate fn wanted() {}",
            "fnwanted()",
            "// a call of wanted",
            "struct Wanted;",
        ] {
            assert_eq!(definition_at(line, "wanted"), None, "{line}");
        }
    }

    #[test]
    fn macro_rules_defines_its_name_anywhere_on_the_line() {
        assert_eq!(definition_at("macro_rules! chunk {", "chunk"), Some(13));
        assert_eq!(
            definition_at("#[macro_export] macro_rules!  chunk {", "chunk"),
            Some(30)
        );
        assert_eq!(definition_at("macro_rules! chunk_loading {", "chunk"), None);
        assert_eq!(definition_at("macro_rules!chunk {", "chunk"), None);
    }

    #[test]
    fn a_name_is_compared_as_text_never_as_a_pattern() {
        assert_eq!(definition_at("fn a.b() {}", "a.b"), Some(3));
        assert_eq!(definition_at("fn axb() {}", "a.b"), None, "a dot is a dot");
        assert_eq!(definition_at("fn a$b() {}", "a$b"), Some(3));
        assert_eq!(definition_at("fn ab() {}", "a.*"), None);
        assert_eq!(definition_at("fn a() {}", "(a|b)"), None);
        assert_eq!(definition_at("fn a() {}", ""), None);
    }

    fn project(files: &[(&str, &str)]) -> (ScratchDir, Vec<String>) {
        let dir = ScratchDir::new("symbol-text", "p");
        let mut listing = Vec::new();
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            listing.push((*name).to_owned());
        }
        (dir, listing)
    }

    #[test]
    fn the_search_finds_definitions_in_rust_files_only() {
        let (dir, listing) = project(&[
            ("src/a.rs", "use x;\n\npub(crate) fn effective() {}\n"),
            ("src/b.rs", "struct Other;\r\nmacro_rules! effective {\r\n"),
            ("notes.md", "fn effective\n"),
            ("src/c.txt", "fn effective\n"),
        ]);
        let found = search_in(&dir, &listing, "effective", Limits::default(), Instant::now());
        assert_eq!(
            found,
            [
                Found {
                    path: "src/a.rs".into(),
                    line: 3,
                    col: 15
                },
                Found {
                    path: "src/b.rs".into(),
                    line: 2,
                    col: 14
                },
            ]
        );
    }

    #[test]
    fn a_column_counts_characters() {
        let (dir, listing) = project(&[("a.rs", "/* é */ fn wanted() {}\n")]);
        let found = search_in(&dir, &listing, "wanted", Limits::default(), Instant::now());
        assert_eq!(
            found[0].col, 12,
            "a closed comment before the item is blank, and é is one character"
        );
        let (dir, listing) = project(&[("a.rs", "\u{3000}fn wanted() {}\n")]);
        let found = search_in(&dir, &listing, "wanted", Limits::default(), Instant::now());
        assert_eq!(found[0].col, 5, "an ideographic space is whitespace and one character");
    }

    #[test]
    fn the_file_cap_and_the_size_cap_hold() {
        let (dir, listing) = project(&[
            ("a.rs", "fn wanted() {}\n"),
            ("b.rs", "fn wanted() {}\n"),
            ("c.rs", "fn wanted() {}\n"),
        ]);
        let two = Limits {
            max_files: 2,
            ..Limits::default()
        };
        let found = search_in(&dir, &listing, "wanted", two, Instant::now());
        assert_eq!(
            found.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            ["a.rs", "b.rs"]
        );

        let filler = "// filler\n".repeat(10);
        let (dir, listing) = project(&[("big.rs", &format!("fn wanted() {{}}\n{filler}fn wanted() {{}}\n"))]);
        let small = Limits {
            max_bytes: 40,
            ..Limits::default()
        };
        let found = search_in(&dir, &listing, "wanted", small, Instant::now());
        assert_eq!(found.len(), 1, "only what the cap read is searched: {found:?}");
        // A line the cap cuts is not read, so `fn wanted_more` cut to `fn wanted` is no match.
        let (dir, listing) = project(&[("cut.rs", "fn wanted_more() {}\n")]);
        let cut = Limits {
            max_bytes: 9,
            ..Limits::default()
        };
        assert!(search_in(&dir, &listing, "wanted", cut, Instant::now()).is_empty());
    }

    #[test]
    fn a_spent_deadline_reads_nothing() {
        let (dir, listing) = project(&[("a.rs", "fn wanted() {}\n")]);
        let none = Limits {
            deadline: Duration::ZERO,
            ..Limits::default()
        };
        assert!(search_in(&dir, &listing, "wanted", none, Instant::now()).is_empty());
    }

    #[test]
    fn a_link_a_fifo_and_a_directory_are_not_read() {
        let (dir, mut listing) = project(&[("real.rs", "fn wanted() {}\n")]);
        let outside = ScratchDir::new("symbol-text", "outside");
        std::fs::write(outside.join("o.rs"), "fn wanted() {}\n").unwrap();
        std::os::unix::fs::symlink(outside.join("o.rs"), dir.join("link.rs")).unwrap();
        let fifo = std::ffi::CString::new(dir.join("fifo.rs").as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        std::fs::create_dir(dir.join("dir.rs")).unwrap();
        listing.extend(["link.rs".into(), "fifo.rs".into(), "dir.rs".into(), "dir.rs/".into()]);
        let found = search_in(&dir, &listing, "wanted", Limits::default(), Instant::now());
        assert_eq!(found.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["real.rs"]);
    }

    #[test]
    fn what_is_not_a_regular_file_is_not_opened() {
        let (dir, _) = project(&[("real.rs", "fn wanted() {}\n")]);
        let fifo = std::ffi::CString::new(dir.join("fifo.rs").as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        std::os::unix::fs::symlink(dir.join("real.rs"), dir.join("link.rs")).unwrap();
        for name in ["fifo.rs", "link.rs", "missing.rs"] {
            let opened = std::cell::Cell::new(false);
            let read = read_capped_via(&dir.join(name), 1000, |path| {
                opened.set(true);
                open_plain(path)
            });
            assert!(read.is_none() && !opened.get(), "{name}: opened {}", opened.get());
        }
        let opened = std::cell::Cell::new(false);
        let read = read_capped_via(&dir.join("real.rs"), 1000, |path| {
            opened.set(true);
            open_plain(path)
        });
        assert_eq!(read.as_deref(), Some(&b"fn wanted() {}\n"[..]));
        assert!(opened.get());
    }

    #[test]
    fn comments_strings_and_raw_strings_define_nothing() {
        let (dir, listing) = project(&[(
            "a.rs",
            "/*\nfn wanted() {}\n*/\nconst S: &str = r#\"\nfn wanted() {}\n\"#;\n// macro_rules! wanted {\n\
             let x = 1; // macro_rules! wanted {\nlet c = '\"'; fn real() {}\n\"\nstruct wanted;\n\"\nfn wanted() {}\n",
        )]);
        let found = search_in(&dir, &listing, "wanted", Limits::default(), Instant::now());
        assert_eq!(found.iter().map(|f| f.line).collect::<Vec<_>>(), [13]);
    }

    #[test]
    fn a_file_under_a_linked_directory_is_not_read() {
        let (dir, mut listing) = project(&[("src/real.rs", "fn wanted() {}\n")]);
        let outside = ScratchDir::new("symbol-text", "outside");
        std::fs::create_dir(outside.join("src")).unwrap();
        std::fs::write(outside.join("src/o.rs"), "fn wanted() {}\n").unwrap();
        std::os::unix::fs::symlink(outside.join("src"), dir.join("linked")).unwrap();
        listing.push("linked/o.rs".into());
        let found = search_in(&dir, &listing, "wanted", Limits::default(), Instant::now());
        assert_eq!(
            found.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            ["src/real.rs"]
        );
    }

    #[test]
    fn a_container_keeps_only_its_own_definitions() {
        let (dir, listing) = project(&[
            (
                "src/lib.rs",
                "impl Other {\n    fn wired() {}\n}\n\
                 impl<T: Into<u8>> fmt::Display for crate::x::Foo<T> where T: Fn() -> u8 {\n    fn wired() {}\n}\n\
                 fn f() -> impl Fn() { || () }\nfn wired() {}\ntrait Foo {\n    fn wired();\n}\n",
            ),
            ("src/foo.rs", "pub fn wired() {}\nmod Foo {\n    fn wired() {}\n}\n"),
            ("src/bar/mod.rs", "fn wired() {}\n"),
        ]);
        let lines = |within: &str| {
            scan(&dir, &listing, "wired", Some(within), Limits::default(), Instant::now())
                .into_iter()
                .map(|f| (f.path, f.line))
                .collect::<Vec<_>>()
        };
        let at = |path: &str, line| (path.to_owned(), line);
        assert_eq!(
            lines("Foo"),
            [at("src/lib.rs", 5), at("src/lib.rs", 10), at("src/foo.rs", 3)]
        );
        assert_eq!(lines("Other"), [at("src/lib.rs", 2)]);
        assert_eq!(lines("foo"), [at("src/foo.rs", 1)]);
        assert_eq!(lines("bar"), [at("src/bar/mod.rs", 1)]);
        assert!(lines("Missing").is_empty());
        assert!(lines("lib").is_empty(), "lib.rs names the crate, not a module");
    }

    #[test]
    fn the_search_lists_the_project_itself() {
        let (dir, _) = project(&[("src/lib.rs", "pub struct Wanted;\n")]);
        let found = search(&dir, "Wanted", Limits::default());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "src/lib.rs");
    }
}
