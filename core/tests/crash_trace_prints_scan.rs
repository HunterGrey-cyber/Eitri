//! A window's log and a crash record go into public issues, so no print in the product may name what a conversation
//! holds. A print that needs to speak of such a value says its length. `crash_trace::scrub` is the second pass, for
//! what no scan can see: a library's words, a child's stderr, an error that quotes.

use eitri_core::source_scan::{code_only, modules, rust_sources, without, word_ends, Source};

const DIRS: [&str; 6] = [
    "core/src",
    "agent/src",
    "panel/src",
    "shell/src",
    "neovide-editor/src",
    "terminal/engine/src",
];
const PRINTS: [&str; 4] = ["println", "eprintln", "print", "eprint"];
/// A variable of one of these names holds what the person or the agent said, or a credential.
const HOLDS_CONTENT: [&str; 20] = [
    "raw",
    "json_str",
    "trimmed",
    "prompt",
    "prompt_text",
    "input",
    "input_json",
    "content",
    "content_json",
    "transcript",
    "output",
    "stdout",
    "body",
    "secret",
    "token",
    "api_key",
    "password",
    "url",
    "uri",
    "argv",
];
/// Files that print such a value on purpose. The keyboard's lines are off until a debug variable is set, and
/// `crash_trace::scrub` keeps them out of every log; the rest are programs or checks a window never runs.
const EXEMPT: [&str; 7] = [
    "neovide-editor/src/keyboard.rs",
    // A watch's token here is a number the scheduler made.
    "shell/src/gtk_host/scheduler.rs",
    "agent/src/providers/claude_sidecar/runtime_policy_verification.rs",
    "agent/src/testkit/",
    "/bin/",
    "/tests/",
    "core/src/crash_trace/",
];

fn close_paren(code: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (at, c) in code[open..].char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + at);
                }
            }
            _ => {}
        }
    }
    None
}

/// What a format string interpolates by name: `{name}` and `{name:?}`.
fn inline_names(literal: &str) -> Vec<&str> {
    literal
        .match_indices('{')
        .filter(|(at, _)| !literal[..*at].ends_with('{'))
        .map(|(at, _)| &literal[at + 1..])
        .map(|rest| {
            &rest[..rest
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(rest.len())]
        })
        .filter(|name| !name.is_empty())
        .collect()
}

/// The arguments after the format string: `code` has its strings blanked, so a comma in one does not count.
fn arguments(code: &str) -> Vec<&str> {
    let mut depth = 0usize;
    let mut start = 0;
    let mut out = Vec::new();
    for (at, c) in code.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(code[start..at].trim());
                start = at + 1;
            }
            _ => {}
        }
    }
    out.push(code[start..].trim());
    out.remove(0);
    out
}

fn head(argument: &str) -> &str {
    let argument = argument.trim_start_matches(['&', '*']);
    &argument[..argument
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .unwrap_or(argument.len())]
}

/// Its size says nothing of it.
fn says_only_its_size(argument: &str) -> bool {
    argument.ends_with(".len()") || argument.ends_with(".count()")
}

fn violations(source: &Source) -> Vec<String> {
    let code = code_only(&source.text);
    let product = without(&code, &modules(&code, "tests"));
    let mut found = Vec::new();
    for name in PRINTS {
        for end in word_ends(&product, name) {
            let Some(open) = product[end..]
                .strip_prefix('!')
                .and_then(|_| product[end + 1..].find('('))
            else {
                continue;
            };
            let open = end + 1 + open;
            let Some(close) = close_paren(&product, open) else {
                continue;
            };
            let line = source.text[..end].matches('\n').count() + 1;
            let (literal, blanked) = (&source.text[open + 1..close], &product[open + 1..close]);
            let inline = inline_names(literal);
            let named = inline.iter().copied().chain(
                arguments(blanked)
                    .into_iter()
                    .filter(|argument| !says_only_its_size(argument))
                    .map(head),
            );
            for value in named {
                if HOLDS_CONTENT.contains(&value) {
                    found.push(format!("{}:{line}: {name}! prints {value}", source.path));
                }
            }
        }
    }
    found
}

#[test]
fn no_print_in_the_product_names_what_a_conversation_holds() {
    let sources = rust_sources(&DIRS);
    assert!(sources.len() > 100, "the scan read {} files", sources.len());
    let found: Vec<String> = sources
        .iter()
        .filter(|source| !EXEMPT.iter().any(|path| source.path.contains(path)))
        .flat_map(violations)
        .collect();
    assert!(
        found.is_empty(),
        "these reach a log a person attaches to a public issue:\n{}",
        found.join("\n")
    );
}

#[test]
fn the_scan_sees_a_print_of_each_shape() {
    let source = |text: &str| Source {
        path: "x.rs".to_string(),
        text: text.to_string(),
    };
    assert_eq!(
        violations(&source(r#"fn a(raw: &str) { eprintln!("got {raw}"); }"#)).len(),
        1
    );
    assert_eq!(
        violations(&source(r#"fn a(raw: &str) { eprintln!("got {raw:?}"); }"#)).len(),
        1
    );
    assert_eq!(
        violations(&source(
            "fn a(raw: &str) {\n  println!(\n    \"got {}\",\n    raw\n  );\n}"
        ))
        .len(),
        1
    );
    assert_eq!(
        violations(&source(r#"fn a(raw: &str) { eprintln!("got {}", raw.len()); }"#)).len(),
        0
    );
    assert_eq!(
        violations(&source(r#"fn a(x: &str) { eprintln!("the raw {{raw}} {x}"); }"#)).len(),
        0
    );
    let tests = r#"#[cfg(test)] mod tests { fn a(raw: &str) { eprintln!("{raw}"); } }"#;
    assert_eq!(violations(&source(tests)).len(), 0);
}
