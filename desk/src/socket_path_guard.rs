//! The twin of `eitri-core`'s and `agent`'s guards that every socket path is built through
//! `agent::socket_path::in_dir`, over this crate's sources (theirs read only their own `src/`).
//!
//! `in_dir` owns the length cap that keeps a path inside macOS's 103 bytes; a socket path built any other
//! way escapes it and fails on a Mac, not on the machine the tests run on.

use eitri_core::source_scan::{code_only, require_anchors, rust_sources, without_line_comments, word_ends};

/// `(file, a substring of the line)`: each one a `.sock` mention that is never a bound or connected path.
/// None yet: the crate holds no socket code.
const NEVER_BOUND: &[(&str, &str)] = &[];

#[test]
fn every_sock_path_in_this_crate_is_built_through_agent_socket_path() {
    let read = rust_sources(&["desk/src"]);
    require_anchors(&read, &["fn unchecked_socket_offsets("]);
    let mut offenders = Vec::new();
    for source in &read {
        // The guard quotes every line it excuses.
        if source.path == "desk/src/socket_path_guard.rs" {
            continue;
        }
        for offset in unchecked_socket_offsets(&source.text) {
            let start = source.text[..offset].rfind('\n').map_or(0, |at| at + 1);
            let code = source.text[start..].lines().next().unwrap_or_default().trim_start();
            if NEVER_BOUND.iter().any(|(f, s)| *f == source.path && code.contains(s)) {
                continue;
            }
            let line = source.text[..start].bytes().filter(|byte| *byte == b'\n').count() + 1;
            offenders.push(format!("{}:{line}: {code}", source.path));
        }
    }
    assert!(
        offenders.is_empty(),
        "socket paths built outside agent::socket_path escape its length check:\n{}",
        offenders.join("\n")
    );
}

/// Every entry on [`NEVER_BOUND`] still matches a real line, so the allowlist cannot become a place a
/// construction site hides behind a substring nothing writes any more.
#[test]
fn no_allowlist_entry_is_stale() {
    let read = rust_sources(&["desk/src"]);
    for (file, needle) in NEVER_BOUND {
        let source = read
            .iter()
            .find(|s| s.path == *file)
            .unwrap_or_else(|| panic!("{file} is not read"));
        assert!(
            source.text.lines().any(|l| l.contains(needle)),
            "{file} no longer contains {needle:?}"
        );
    }
}

/// Socket-name offsets outside the argument lists of `socket_path::in_dir` calls.
fn unchecked_socket_offsets(text: &str) -> Vec<usize> {
    let text = without_line_comments(text);
    let code = code_only(&text);
    let mut arguments = Vec::new();
    for end in word_ends(&code, "socket_path") {
        let tail = &code[end..];
        let Some(tail) = tail.trim_start().strip_prefix("::") else {
            continue;
        };
        let Some(tail) = tail.trim_start().strip_prefix("in_dir").map(str::trim_start) else {
            continue;
        };
        let Some(body) = tail.strip_prefix('(') else {
            continue;
        };
        let start = code.len() - body.len();
        let mut depth = 1;
        for (offset, byte) in body.bytes().enumerate() {
            match byte {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => continue,
            }
            if depth == 0 {
                arguments.push(start..start + offset);
                break;
            }
        }
    }
    text.match_indices(".sock")
        .filter(|(offset, suffix)| {
            !text[offset + suffix.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                && !arguments.iter().any(|range| range.contains(offset))
        })
        .map(|(offset, _)| offset)
        .collect()
}

/// The matcher sees a socket name and does not see a bare identifier, and sees the call that routes one.
#[test]
fn the_matcher_reads_what_it_should() {
    assert_eq!(unchecked_socket_offsets("let p = dir.join(\"x.sock\");").len(), 1);
    assert!(unchecked_socket_offsets("let p = self.socket_path;").is_empty());
    assert!(unchecked_socket_offsets("let p = agent::socket_path::in_dir(\n &dir,\n \"x.sock\",\n);").is_empty());
    assert_eq!(
        unchecked_socket_offsets("let a = 1;\nlet p = dir.join(\"x.sock\");").len(),
        1
    );
    assert!(unchecked_socket_offsets("// x.sock").is_empty());
}

#[test]
fn trailing_comments_do_not_hide_or_add_socket_paths() {
    let checked = r#"let p = socket_path::in_dir(dir, "ok.sock")?; // previously old.sock"#;
    assert!(unchecked_socket_offsets(checked).is_empty());
    for line in [
        r#"let p = dir.join("é\"//real.sock"); // old.sock"#,
        r##"let p = dir.join(r#"prefix
//real.sock"#); // old.sock"##,
    ] {
        assert_eq!(unchecked_socket_offsets(line), [line.find(".sock").unwrap()]);
    }
}

#[test]
fn a_checked_call_does_not_exempt_a_fallback_on_the_same_line() {
    let text = r#"let p = socket_path::in_dir(make_dir(), "a).sock").unwrap_or_else(|_| dir.join("fallback.sock"));"#;
    assert_eq!(unchecked_socket_offsets(text), [text.find(".sock\"));").unwrap()]);
    let split = "socket_path ::\n in_dir\n(dir, \"a.sock\")\n.or_else(|_| dir.join(\"fallback.sock\"))";
    assert_eq!(unchecked_socket_offsets(split), [split.rfind(".sock").unwrap()]);
}
