//! A revert, an undo and a recovery reach project files only through `write::ProjectDir`'s
//! directory descriptors, never by a path string: a path string is resolved by the kernel again
//! on every call, through whatever links a directory on it has become since it was checked.
//!
//! This reads `turn_review/write.rs` (and `revert.rs`, once it exists) as source text and fails on
//! anything outside `#[cfg(test)]` that takes a path: `std::fs`, `File::…`, `OpenOptions`, the
//! `Path` methods that touch the disk, and the libc calls that take a path rather than a directory
//! descriptor. A line may carry such a token only with `// containment-scan: <why this is not a
//! project path>`. Comments and string literals are blanked first, and whitespace between tokens is
//! skipped, so neither formatting nor a word in a comment changes the result.

use std::path::Path;

/// `text` with every comment, string literal and character literal replaced by spaces (line breaks
/// kept), so neither a brace nor a word inside one counts.
fn code_only(text: &str) -> String {
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
        // A raw string: `r"..."`, `r#"..."#`, also after `b`.
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
            // `'\n'`, `'\u{7b}'` or `'{'`; anything else is a lifetime.
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

/// The text from the `{` at `open` to its matching `}`, inclusive.
fn braced(code: &str, open: usize) -> &str {
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

/// `code` (already blanked) with every item under `#[cfg(test)]` blanked too: from the attribute to
/// the end of the item's braces, or to its `;` when it has none.
fn without_test_items(code: &str) -> String {
    let mut out = code.to_owned();
    let mut from = 0;
    while let Some(found) = squeezed_find(&out, from, "#[cfg(test)]") {
        let (start, end_attr) = found;
        let rest = &out[end_attr..];
        let brace = rest.find('{');
        let semi = rest.find(';');
        let end = match (brace, semi) {
            (Some(b), Some(s)) if s < b => end_attr + s + 1,
            (Some(b), _) => {
                let open = end_attr + b;
                open + braced(&out, open).len()
            }
            (None, Some(s)) => end_attr + s + 1,
            (None, None) => out.len(),
        };
        let blanked: String = out[start..end]
            .chars()
            .map(|c| if c == '\n' { '\n' } else { ' ' })
            .collect();
        out.replace_range(start..end, &blanked);
        from = start + blanked.len();
    }
    out
}

/// The next `(start, end)` of `pattern` in `code` from `from`, whitespace inside the match allowed
/// between any two characters of the pattern.
fn squeezed_find(code: &str, from: usize, pattern: &str) -> Option<(usize, usize)> {
    let wanted: Vec<char> = pattern.chars().collect();
    let mut start = from;
    while let Some(off) = code[start..].find(wanted[0]) {
        let at = start + off;
        let mut k = 0;
        for (i, c) in code[at..].char_indices() {
            if c == wanted[k] {
                k += 1;
                if k == wanted.len() {
                    return Some((at, at + i + c.len_utf8()));
                }
            } else if !(k > 0 && c.is_whitespace()) {
                break;
            }
        }
        start = at + wanted[0].len_utf8();
    }
    None
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Where in `code` a token sequence starts: `parts` in order, with whitespace allowed between them.
/// A part that starts with an identifier character must not follow one, and one that ends with an
/// identifier character must not be followed by one, so `fs` never matches inside `fsync`.
fn sequence_hits(code: &str, parts: &[&str]) -> Vec<usize> {
    let mut hits = Vec::new();
    let first = parts[0];
    for (at, _) in code.match_indices(first) {
        if first.starts_with(is_ident) && code[..at].chars().next_back().is_some_and(is_ident) {
            continue;
        }
        let mut pos = at;
        let mut ok = true;
        for (n, part) in parts.iter().enumerate() {
            if n > 0 {
                pos += code[pos..].len() - code[pos..].trim_start().len();
            }
            if !code[pos..].starts_with(part) {
                ok = false;
                break;
            }
            pos += part.len();
            if part.ends_with(is_ident) && code[pos..].chars().next().is_some_and(is_ident) {
                ok = false;
                break;
            }
        }
        if ok {
            hits.push(at);
        }
    }
    hits
}

/// The token sequences that reach the disk by a path string.
fn forbidden() -> Vec<Vec<&'static str>> {
    let mut list = vec![
        vec!["fs", "::"],
        vec!["std", "::", "fs"],
        vec!["File", "::"],
        vec!["OpenOptions"],
    ];
    for method in [
        "exists",
        "try_exists",
        "metadata",
        "symlink_metadata",
        "read_dir",
        "read_link",
        "canonicalize",
        "is_file",
        "is_dir",
        "is_symlink",
    ] {
        list.push(vec![".", method, "("]);
    }
    for call in [
        "open", "stat", "lstat", "unlink", "rename", "mkdir", "link", "symlink", "readlink", "chmod", "chown",
        "truncate",
    ] {
        list.push(vec!["libc", "::", call, "("]);
    }
    list
}

/// Every unannotated use of a path-taking call in `source`, as `line: text`.
fn violations(source: &str) -> Vec<String> {
    let code = without_test_items(&code_only(source));
    let lines: Vec<&str> = source.lines().collect();
    let mut found = Vec::new();
    for parts in forbidden() {
        for at in sequence_hits(&code, &parts) {
            let line = code[..at].matches('\n').count();
            let raw = lines.get(line).copied().unwrap_or("");
            let annotated = raw
                .split_once("// containment-scan:")
                .is_some_and(|(_, why)| !why.trim().is_empty());
            if !annotated {
                found.push(format!("{}: {} ({})", line + 1, raw.trim(), parts.concat()));
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

#[test]
fn project_files_are_reached_only_through_directory_descriptors() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/turn_review");
    let write = std::fs::read_to_string(dir.join("write.rs")).expect("write.rs exists");
    // A scan that reads the wrong file passes on anything.
    assert!(
        code_only(&write).contains("openat"),
        "write.rs no longer opens anything relative to a directory; is this scanning the right file?"
    );
    let mut problems = Vec::new();
    for name in ["write.rs", "revert.rs"] {
        // A file that went missing would pass on nothing, silently.
        let source = std::fs::read_to_string(dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        for v in violations(&source) {
            problems.push(format!("{name}:{v}"));
        }
    }
    assert!(
        problems.is_empty(),
        "path-based file access outside ProjectDir (annotate with `// containment-scan: <why>` only when \
         the path is not a project path):\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_scan_catches_what_it_is_for() {
    let caught = |src: &str| !violations(src).is_empty();
    assert!(caught(
        "fn f(root: &Path, p: &Path) { std::fs::write(root.join(p), b\"x\").unwrap(); }"
    ));
    assert!(caught("fn f() { std::fs\n    ::write(p, b) }"));
    assert!(caught("fn f() { let ok = root.join(p).exists(); }"));
    assert!(caught("fn f() { root.join(p)\n    .metadata() }"));
    assert!(caught("fn f() { unsafe { libc::unlink(c.as_ptr()) }; }"));
    assert!(caught("fn f() { unsafe { libc :: rename (a, b) }; }"));
    assert!(caught("fn f() { let f = File::open(p); }"));
    assert!(caught("fn f() { OpenOptions::new().open(p); }"));
    assert!(caught("use std::fs;"));
    // An annotation with no reason is no annotation.
    assert!(caught("fn f() { std::fs::write(p, b); } // containment-scan:   "));

    // Words in comments and strings, `*at` calls and test items are not path access.
    assert!(!caught("// std::fs::write(root.join(p), b)\nfn f() {}"));
    assert!(!caught("fn f() { let s = \"std::fs::write(p) and .exists()\"; }"));
    assert!(!caught(
        "fn f() { unsafe { libc::unlinkat(fd, c.as_ptr(), 0); libc::openat(fd, n, 0); } }"
    ));
    assert!(!caught(
        "fn f() { unsafe { libc::fsync(fd); libc::fstat(fd, &mut st); } }"
    ));
    assert!(!caught(
        "#[cfg(test)]\nmod tests { fn t() { std::fs::write(p, b).unwrap(); } }"
    ));
    assert!(!caught("#[cfg(test)]\nuse std::fs;\nfn f() {}"));
    assert!(!caught(
        "fn f() { std::fs::write(p, b); } // containment-scan: a file under the review directory"
    ));
    // But code after a test item is scanned again.
    assert!(caught(
        "#[cfg(test)]\nmod tests {}\nfn f() { std::fs::remove_file(p); }"
    ));
}
