//! CLI records are read for display; display retention cannot mutate a provider's session store.
use eitri_core::source_scan::{
    braced, calls, code_only, product_sources, read_required, require_anchors, rust_sources, test_only_files,
    word_ends, Source,
};
use std::collections::BTreeSet;

const READERS: &[&str] = &[
    "agent/src/transcript.rs",       // Path construction, stability probe and title reader.
    "agent/src/history/load.rs",     // Claude display seed, with Eitri's fallback.
    "agent/src/external_writer.rs",  // Watches the transcript's modification time.
    "agent/src/conversation.rs",     // Stability check before a Claude resume.
    "core/src/transcript_titles.rs", // Background title reader.
    "core/src/agent_backend.rs",     // Resume title lookup.
    "panel/src/mentions.rs",         // Excludes the transcript directory from file mentions.
];

fn compact(code: &str) -> String {
    code.chars().filter(|c| !c.is_whitespace()).collect()
}

// Blank whole test-gated items, preserving offsets for a second pass that retains literals.
// Later production items stay in the scan, even when a test helper precedes them.
fn production(text: &str, literals: bool) -> String {
    let code = code_only(text);
    let mut out = if literals { text.to_owned() } else { code.clone() };
    let mut from = 0;
    while let Some(offset) = code[from..].find('#') {
        let start = from + offset;
        let Some(close) = code[start..].find(']') else { break };
        let attr_end = start + close + 1;
        let attr = compact(&code[start..attr_end]);
        if !(attr == "#[cfg(test)]" || attr == "#![cfg(test)]" || attr.starts_with("#[cfg(all(test,")) {
            from = start + 1;
            continue;
        }
        let rest = &code[attr_end..];
        let end = if attr.starts_with("#!") {
            code.len()
        } else {
            match (rest.find('{'), rest.find(';')) {
                (Some(b), Some(s)) if s < b => attr_end + s + 1,
                (Some(b), _) => attr_end + b + braced(&code, attr_end + b).len(),
                (_, Some(s)) => attr_end + s + 1,
                _ => code.len(),
            }
        };
        out.replace_range(start..end, &" ".repeat(end - start));
        from = end;
    }
    out
}

fn mutation(text: &str) -> bool {
    let code = code_only(text);
    if [
        "write",
        "copy",
        "hard_link",
        "set_len",
        "rename",
        "remove_file",
        "remove_dir",
        "remove_dir_all",
        "truncate",
        "ftruncate",
        "unlink",
        "unlinkat",
        "append_private",
    ]
    .iter()
    .any(|name| calls(&code, name) > 0)
    {
        return true;
    }
    let raw = compact(text);
    if ["rm", "mv", "truncate"]
        .iter()
        .any(|cmd| raw.contains(&format!("Command::new(\"{cmd}\")")))
    {
        return true;
    }
    let code = compact(&code);
    [
        "fs::write",
        "fs::rename",
        "fs::remove_file",
        "fs::remove_dir",
        "File::create(",
        "OpenOptions",
        "File::options(",
        ".append(true)",
        ".write_all(",
        ".write_fmt(",
        ".write_vectored(",
        ".write(",
        ".set_len(",
        ".truncate(",
        "libc::truncate(",
        "libc::ftruncate(",
        "libc::unlink(",
        "libc::rename(",
        "libc::open(",
        "libc::openat(",
        "std::io::Write",
        "private_fs::write",
        "create_new_file",
    ]
    .iter()
    .any(|token| code.contains(token))
}

// String tokens and identifiers, without comments. Session/list protocol words do not name files.
fn store_name(text: &str) -> bool {
    let mut tokens = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek().copied()) {
            ('/', Some('/')) => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut depth = 1;
                while let Some(c) = chars.next() {
                    match (c, chars.peek().copied()) {
                        ('/', Some('*')) => {
                            chars.next();
                            depth += 1;
                        }
                        ('*', Some('/')) => {
                            chars.next();
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
            ('"', _) => {
                tokens.push('"');
                while let Some(c) = chars.next() {
                    tokens.push(c);
                    if c == '\\' {
                        if let Some(c) = chars.next() {
                            tokens.push(c);
                        }
                    } else if c == '"' {
                        break;
                    }
                }
            }
            _ => tokens.push(c),
        }
    }
    let tokens = compact(&tokens).to_lowercase();
    [
        "rollout",
        "sessions/",
        "/sessions",
        "archived_sessions",
        "session_file",
        "session_path",
        "session_dir",
        "join(\"sessions\")",
        "join(\"session\")",
        ".jsonl",
    ]
    .iter()
    .any(|token| tokens.contains(token))
}

fn scan_readers(sources: &[Source]) -> Result<BTreeSet<&str>, String> {
    let tests = test_only_files(sources);
    let mut found = BTreeSet::new();
    for source in sources.iter().filter(|s| !tests.contains(&s.path)) {
        let code = production(&source.text, false);
        if !word_ends(&code, "transcript_path").is_empty() || !word_ends(&code, "claude_projects_dir").is_empty() {
            if !READERS.contains(&source.path.as_str()) {
                return Err(format!("unreviewed CLI record reader: {}", source.path));
            }
            if mutation(&production(&source.text, true)) {
                return Err(format!("CLI record reader can mutate files: {}", source.path));
            }
            found.insert(source.path.as_str());
        }
    }
    Ok(found)
}

#[test]
fn cli_record_paths_have_only_the_allowlisted_readers_and_no_mutators() {
    let sources = product_sources();
    require_anchors(
        &sources,
        &[
            "fn transcript_path(",
            "fn claude_projects_dir(",
            "fn seed_for_resume(",
            "fn resume_external(",
        ],
    );
    let found = scan_readers(&sources).unwrap();
    assert_eq!(
        found,
        READERS.iter().copied().collect(),
        "reader allowlist must not grow stale"
    );
    // The parser receives a path rather than constructing it, but belongs to the same read-only surface.
    let parser = read_required("agent/src/history/transcript_jsonl.rs");
    assert!(!mutation(&production(&parser.text, true)));
}

#[test]
fn codex_and_acp_never_name_a_cli_session_store() {
    let sources = rust_sources(&["agent/src/providers/codex", "agent/src/providers/acp"]);
    let tests = test_only_files(&sources);
    require_anchors(
        &sources,
        &["fn take_history(", "fn effective_codex_home(", "struct History"],
    );
    for source in sources.iter().filter(|s| !tests.contains(&s.path)) {
        let raw = production(&source.text, true);
        if source.path.starts_with("agent/src/providers/acp/") {
            let code = compact(&production(&source.text, false));
            assert!(
                !code.contains("fs::") && !code.contains("File::") && !code.contains("private_fs::"),
                "ACP must obtain history through replay rather than opening session files: {}",
                source.path
            );
        }
        assert!(!store_name(&raw), "provider names a CLI session store: {}", source.path);
    }
    // This check's child owns an isolated CODEX_HOME under Eitri's private run directory.
    // It may write its own fixtures and verdict, never the user's session store.
    let check = read_required("agent/src/providers/codex/self_check.rs");
    let raw = production(&check.text, true);
    assert!(raw.contains("OsString::from(\"CODEX_HOME\")"));
    assert!(raw.contains("create_private_dir_all"));
}

#[test]
fn acp_replay_only_builds_a_display_seed() {
    let source = read_required("agent/src/providers/acp/history.rs");
    let code = production(&source.text, false);
    assert!(!mutation(&code));
    for method in [
        "send",
        "send_turn",
        "request",
        "notify",
        "write",
        "save",
        "session_prompt",
    ] {
        assert_eq!(calls(&code, method), 0, "replay gained an outbound operation: {method}");
        assert!(!compact(&code).contains(&format!(".{method}(")));
    }
    assert!(compact(&code).contains("vec![AgentDomainEvent::MoveSeeded(Box::new(seed))]"));
    assert!(!code.contains("ExternalProvider"));
    assert!(!code.contains("JsonRpcLink"));
}

#[test]
fn scan_catches_mutations_and_store_paths_after_tests_and_across_line_breaks() {
    for text in [
        "std::fs::\nwrite(p,b);",
        "File :: create(p);",
        "f. set_len(0);",
        "use std::fs::write as put; put(p,b);",
        "use std::fs::remove_file; remove_file(p);",
        "std::fs::rename(a,b);",
    ] {
        assert!(mutation(&production(text, false)), "missed {text}");
    }
    assert!(!mutation(&production(
        "#[cfg(test)] mod t { fn f(){std::fs::write(p,b);} }",
        false
    )));
    assert!(mutation(&production(
        "#[cfg(test)] mod t {} fn f(){std::fs::write(p,b);}",
        false
    )));
    assert!(mutation(&production(
        "#[cfg(all(test, unix))] mod t {} fn f(){File::create(p);}",
        false
    )));
    for text in [
        "home.join(\"sessions\")",
        "p.join(\"rollout-123.jsonl\")",
        "p.join(\"sessions/x\")",
        "let session_file = p;",
    ] {
        assert!(store_name(text), "missed {text}");
    }
    assert!(!store_name("// sessions/x\nlet method = \"session/list\";"));
    assert_eq!(
        calls(
            &production("// transcript_path(p)\nfn f(){ transcript_path \n (p); }", false),
            "transcript_path"
        ),
        1
    );
    let extra = Source {
        path: "panel/src/new_reader.rs".into(),
        text: "fn f(){agent::transcript::transcript_path(p,s);}".into(),
    };
    let mut sources = rust_sources(&["agent/src", "core/src", "panel/src"]);
    sources.push(extra);
    assert!(scan_readers(&sources).unwrap_err().contains("panel/src/new_reader.rs"));
    for operation in [
        "fs::copy(a,b);",
        "fs::hard_link(a,b);",
        "f.append(true);",
        "File::options();",
        "set_len(0);",
        "Command::new(\"rm\");",
        "Command::new(\"mv\");",
        "Command::new(\"truncate\");",
    ] {
        let text = format!("fn f() {{ transcript_path(p,s); {operation} }}");
        let source = Source {
            path: "agent/src/transcript.rs".into(),
            text,
        };
        assert!(
            scan_readers(&[source]).unwrap_err().contains("can mutate"),
            "missed {operation}"
        );
    }
}
