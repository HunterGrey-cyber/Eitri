//! Where a project's Rust files define a name, found by reading their text: the one fallback `gd` has for a name a
//! language server does not answer for. rust-analyzer's `workspace/symbol` misses short and common function names in
//! a large workspace, and holds every request for the seconds its workspace takes to load; a definition line is plain
//! enough to find without it.
//!
//! The files are the project's own listing ([`crate::path_complete::list_paths`]: git's, else a bounded walk that
//! follows no link), only `*.rs`, each checked to be a regular file before it is opened (without following a link or
//! waiting on a FIFO) and read up to a cap. The name is compared as text, never compiled into a pattern. This lists
//! and reads files and belongs on a worker thread: the reading stops 1 s after the search began, but the listing comes
//! first and is not cut short (git's may take up to 10 s), and the deadline is checked between files.

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
    let started = Instant::now();
    if name.is_empty() {
        return Vec::new();
    }
    let listing = crate::path_complete::list_paths(root);
    search_in(root, &listing, name, limits, started)
}

/// [`search`] over a listing already made, its time counted from `started`.
pub fn search_in(root: &Path, listing: &[String], name: &str, limits: Limits, started: Instant) -> Vec<Found> {
    let mut found = Vec::new();
    if name.is_empty() {
        return found;
    }
    let files = listing
        .iter()
        .filter(|entry| entry.ends_with(".rs") && !entry.ends_with('/'));
    for entry in files.take(limits.max_files) {
        if started.elapsed() >= limits.deadline || found.len() >= MAX_FOUND {
            break;
        }
        let Some(bytes) = read_capped(&root.join(entry), limits.max_bytes) else {
            continue;
        };
        let cut = bytes.len() as u64 >= limits.max_bytes;
        let mut lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
        // The last piece has no end of line: at the cap it may be any part of a longer line.
        if cut {
            lines.pop();
        }
        for (index, line) in lines.into_iter().enumerate() {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            // A definition line names the item in ASCII words, so a quick look for the name's bytes skips the rest.
            if !contains(line, name.as_bytes()) {
                continue;
            }
            let text = String::from_utf8_lossy(line);
            if let Some(at) = definition_at(&text, name) {
                let (Ok(number), Ok(col)) = (u32::try_from(index + 1), u32::try_from(text[..at].chars().count() + 1))
                else {
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
        assert!(found.is_empty(), "a comment first is no definition line");
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
    fn the_search_lists_the_project_itself() {
        let (dir, _) = project(&[("src/lib.rs", "pub struct Wanted;\n")]);
        let found = search(&dir, "Wanted", Limits::default());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "src/lib.rs");
    }
}
