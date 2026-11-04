//! The shell carries out the controller's decisions without retaining layout writes or verb dispatch.

use eitri_core::source_scan::{self as scan, Source};

// Keep every crate-visible function, including the geometry and pointer adapters and probe-only methods,
// explicit: adding another shell entry point needs a deliberate decision about what it may do.
const GRID_FUNCTIONS: &[&str] = &[
    "add",
    "apply",
    "arrangement_for",
    "connect_changed_by_controller",
    "connect_strip_activate",
    "covered_now",
    "focus_target",
    "has_host",
    "hosts",
    "hosts_in_tree_order",
    "live_hosts",
    "map_prepared",
    "new",
    "pointer_event",
    "prepare_page",
    "probe_dividers",
    "probe_place",
    "restack",
    "restack_floats",
    "set_allocator",
    "set_editor_hole",
    "set_float_labels",
    "set_page_cover",
    "set_pointer_sink",
    "set_strip_labels",
    "strip_entries",
    "take_every_host_off_screen",
];

#[derive(Clone, Copy, Debug)]
enum Rule {
    ActionMatch,
    TryBorrow,
    BorrowGuard,
    MutableLayout,
    GridFunctions,
    Transitional,
}

fn test_module_declarations(code: &str) -> Vec<(&str, usize)> {
    scan::word_ends(code, "mod")
        .into_iter()
        .filter_map(|after| {
            let before = tokens(&code[..after - "mod".len()]);
            let mut before = before.as_slice();
            if before.last() == Some(&"pub") {
                before = &before[..before.len() - 1];
            } else if before.last() == Some(&")") {
                if let Some(open) = before.iter().rposition(|word| *word == "(") {
                    if open > 0 && before[open - 1] == "pub" {
                        before = &before[..open - 1];
                    }
                }
            }
            if !before.ends_with(&["#", "[", "cfg", "(", "test", ")", "]"]) {
                return None;
            }
            tokens(&code[after..]).first().copied().map(|name| (name, after))
        })
        .collect()
}

fn file_test_modules(sources: &[Source]) -> Vec<String> {
    let mut paths = Vec::new();
    for source in sources {
        let code = scan::code_only(&source.text);
        let path = std::path::Path::new(&source.path);
        let parent = path.parent().unwrap();
        let stem = path.file_stem().unwrap().to_str().unwrap();
        let dir = if matches!(stem, "lib" | "main" | "mod") {
            parent.to_path_buf()
        } else {
            parent.join(stem)
        };
        for (name, after) in test_module_declarations(&code) {
            if tokens(&code[after..]).get(1) == Some(&";") {
                let mut dir = dir.clone();
                // An inline module adds its name to the directory used by its file modules.
                for parent_after in scan::word_ends(&code[..after - "mod".len()], "mod") {
                    let words = tokens(&code[parent_after..]);
                    if words.get(1) == Some(&"{") {
                        let open = offset(&code, words[1]);
                        if open + scan::braced(&code, open).len() > after {
                            dir.push(words[0]);
                        }
                    }
                }
                paths.push(dir.join(name).to_string_lossy().into_owned());
            }
        }
    }
    paths
}

fn production_code(source: &Source, test_modules: &[String]) -> String {
    if source.path.rsplit('/').next() == Some("tests.rs")
        || source.path.split('/').any(|part| part == "tests")
        || test_modules
            .iter()
            .any(|path| source.path == format!("{path}.rs") || source.path.starts_with(&format!("{path}/")))
    {
        return String::new();
    }
    let code = scan::code_only(&source.text);
    let mut bodies = Vec::new();
    for (name, _) in test_module_declarations(&code) {
        bodies.extend(scan::modules(&code, name));
    }
    scan::without(&code, &bodies)
}

/// Identifiers and punctuation stay separate even when a formatter changes every line break.
fn tokens(code: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut chars = code.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        let mut end = start + c.len_utf8();
        if c.is_alphanumeric() || c == '_' {
            while let Some(&(at, next)) = chars.peek() {
                if !next.is_alphanumeric() && next != '_' {
                    break;
                }
                chars.next();
                end = at + next.len_utf8();
            }
        } else if chars
            .peek()
            .is_some_and(|&(_, next)| matches!((c, next), (':', ':') | ('=', '>')))
        {
            let (at, next) = chars.next().unwrap();
            end = at + next.len_utf8();
        }
        out.push(&code[start..end]);
    }
    out
}

fn has(words: &[&str], needle: &[&str]) -> bool {
    words.windows(needle.len()).any(|part| part == needle)
}

/// Explicit types, constructors and handle clones identify bindings even when their names change.
fn bindings_of<'a>(words: &[&'a str], ty: &str) -> Vec<&'a str> {
    let mut names = Vec::new();
    if ty == "Driver" {
        names.extend(
            words
                .iter()
                .copied()
                .filter(|word| *word == "driver" || word.ends_with("_driver")),
        );
    }
    for (i, word) in words.iter().enumerate() {
        if *word == ":" && i > 0 {
            let end = words[i + 1..]
                .iter()
                .position(|w| matches!(*w, "," | ")" | "=" | ";" | "{"))
                .map_or(words.len(), |end| i + 1 + end);
            let annotation = &words[i + 1..end];
            if annotation
                .iter()
                .enumerate()
                .any(|(j, word)| *word == ty && annotation.get(j + 1) != Some(&"::"))
            {
                names.push(words[i - 1]);
            }
        }
    }
    loop {
        let before = names.len();
        for (i, word) in words.iter().enumerate() {
            if *word != "let" {
                continue;
            }
            let tail = &words[i + 1..];
            let name_at = usize::from(tail.first() == Some(&"mut"));
            let Some(name) = tail.get(name_at) else { continue };
            if tail.get(name_at + 1) != Some(&"=") || names.contains(name) {
                continue;
            }
            let value = &tail[name_at + 2..];
            let prefix = value.iter().take_while(|w| **w == "&" || **w == "mut").count();
            let value = &value[prefix..];
            let path_end = value
                .iter()
                .position(|word| !word.chars().all(|c| c.is_alphanumeric() || c == '_') && *word != "::")
                .unwrap_or(value.len());
            let constructor = has(&value[..path_end], &[ty, "::"]);
            let alias = value.first().is_some_and(|word| names.contains(word))
                && (value.get(1) == Some(&";") || value.get(1..6) == Some(&[".", "clone", "(", ")", ";"]));
            if constructor || alias {
                names.push(*name);
            }
        }
        if names.len() == before {
            return names;
        }
    }
}

fn offset(code: &str, word: &str) -> usize {
    word.as_ptr() as usize - code.as_ptr() as usize
}

fn action_match(code: &str) -> Option<usize> {
    let words = tokens(code);
    let actions = bindings_of(&words, "Action");
    for (i, word) in words.iter().enumerate() {
        if *word == "matches" && words.get(i + 1) == Some(&"!") {
            let mut depth = 0;
            let mut pattern = None;
            for (j, token) in words.iter().enumerate().skip(i + 2) {
                match *token {
                    "(" | "[" | "{" => depth += 1,
                    ")" | "]" | "}" => {
                        depth -= 1;
                        if depth == 0 {
                            if pattern.is_some_and(|start| has(&words[start..j], &["Action", "::"])) {
                                return Some(offset(code, word));
                            }
                            break;
                        }
                    }
                    "," if depth == 1 && pattern.is_none() => pattern = Some(j + 1),
                    "if" if depth == 1 && pattern.is_some() => {
                        // A guard may construct an action; only the pattern dispatches on it.
                        if has(&words[pattern.unwrap()..j], &["Action", "::"]) {
                            return Some(offset(code, word));
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
        if *word == "let" {
            let pattern = &words[i + 1..];
            if let Some(equal) = pattern.iter().position(|word| matches!(*word, "=" | ";")) {
                if pattern[equal] == "=" && has(&pattern[..equal], &["Action", "::"]) {
                    return Some(offset(code, word));
                }
            }
        }
    }
    for after in scan::word_ends(code, "match") {
        let rest = &code[after..];
        let Some(open) = rest.find('{') else { continue };
        let scrutinee = tokens(&rest[..open]);
        if scrutinee.contains(&"Action") || scrutinee.iter().any(|word| actions.contains(word)) {
            return Some(after - "match".len());
        }
        let body = tokens(scan::braced(code, after + open));
        // At the arm level, only patterns before `=>` count: Action constructors in an arm's body are valid.
        let mut depth = 0;
        let mut pattern = 1;
        let mut in_pattern = true;
        for (i, word) in body.iter().enumerate().skip(1) {
            match *word {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" if depth > 0 => {
                    depth -= 1;
                    if *word == "}" && depth == 0 && !in_pattern {
                        pattern = i + 1;
                        in_pattern = true;
                    }
                }
                "=>" if depth == 0 => {
                    if in_pattern && has(&body[pattern..i], &["Action", "::"]) {
                        return Some(after - "match".len());
                    }
                    in_pattern = false;
                }
                "," if depth == 0 => {
                    pattern = i + 1;
                    in_pattern = true;
                }
                _ => {}
            }
        }
    }
    None
}

fn binds_borrow_guard<'a>(words: &[&'a str]) -> Option<&'a str> {
    words.iter().enumerate().find_map(|(i, word)| {
        if *word != "let" {
            return None;
        }
        let tail = &words[i + 1..];
        let mut depth = 0;
        let mut equal = None;
        for (j, word) in tail.iter().enumerate() {
            match *word {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" if depth > 0 => depth -= 1,
                "=" if depth == 0 => equal = Some(j),
                ";" if depth == 0 => {
                    let equal = equal?;
                    let mut value = &tail[equal + 1..j];
                    while value.first() == Some(&"(") && value.last() == Some(&")") {
                        value = &value[1..value.len() - 1];
                    }
                    return (value.ends_with(&[".", "borrow", "(", ")"])
                        || value.ends_with(&[".", "borrow_mut", "(", ")"]))
                    .then_some(words[i]);
                }
                _ => {}
            }
        }
        None
    })
}

fn grid_functions<'a>(words: &[&'a str]) -> Vec<&'a str> {
    let mut found = Vec::new();
    for (i, word) in words.iter().enumerate() {
        if *word == "pub" {
            let mut rest = &words[i + 1..];
            if rest.first() == Some(&"(") {
                let Some(end) = rest.iter().position(|word| *word == ")") else {
                    continue;
                };
                rest = &rest[end + 1..];
            }
            let modifiers = rest
                .iter()
                .take_while(|w| matches!(**w, "async" | "unsafe" | "const" | "extern"))
                .count();
            if rest.get(modifiers) == Some(&"fn") {
                found.push(rest[modifiers + 1]);
            }
        }
    }
    found.sort_unstable();
    found
}

fn transitional(source: &Source, code: &str, words: &[&str]) -> Vec<String> {
    let mut found = Vec::new();
    for name in [
        "ShellVerb",
        "ShellCall",
        "run_shell_verb",
        "set_shell_verb_arm",
        "LayoutChangedByShell",
        "ShellRefused",
        "PromptWithdrawn",
    ] {
        if !scan::word_ends(code, name).is_empty() {
            found.push(name.to_owned());
        }
    }
    if source.path.starts_with("core/src/wm/") {
        for (ty, forbidden) in [
            (
                "Driver",
                &[
                    "lua_request",
                    "keymap_opened",
                    "keymap_superseded",
                    "persist_now",
                    "focus_now",
                    "scratch_new",
                    "tab_break",
                    "reveal_chat",
                ][..],
            ),
            (
                "Controller",
                &[
                    "persist_now",
                    "keymap_opened",
                    "keymap_superseded",
                    "scratch_new",
                    "tab_break",
                    "reveal_chat",
                    "shell_revision",
                ][..],
            ),
        ] {
            let blocks = scan::impl_blocks(code, ty);
            for name in forbidden {
                if blocks.iter().any(|body| !scan::functions(body, name).is_empty()) {
                    found.push(format!("{ty}::{name}"));
                }
            }
        }
    }
    if matches!(source.path.as_str(), "core/src/wm/lua.rs" | "core/src/wm/verbs/mod.rs")
        && !scan::functions(code, "moved").is_empty()
    {
        found.push("fn moved".into());
    }
    if source.path.starts_with("shell/src/") {
        let drivers = bindings_of(words, "Driver");
        for i in 0..words.len().saturating_sub(2) {
            if words[i] != "."
                || !matches!(words[i + 1], "lua_request" | "persist_now" | "focus_now")
                || words[i + 2] != "("
            {
                continue;
            }
            let start = words[..i]
                .iter()
                .rposition(|word| matches!(*word, ";" | "=" | "{" | "}" | ","))
                .map_or(0, |at| at + 1);
            if words[start..i].iter().any(|word| drivers.contains(word)) {
                found.push(format!("driver.{}()", words[i + 1]));
            }
        }
    }
    found
}

fn violations(rule: Rule, sources: &[Source]) -> Vec<String> {
    let mut failures = Vec::new();
    let test_modules = file_test_modules(sources);
    let test_only = scan::test_only_files(sources);
    for source in sources.iter().filter(|source| !test_only.contains(&source.path)) {
        // Keep newlines from test modules too: production_code blanks their entire bodies.
        let blanked = scan::code_only(&source.text);
        let line_at = |at| blanked[..at].bytes().filter(|b| *b == b'\n').count() + 1;
        let code = production_code(source, &test_modules);
        let words = tokens(&code);
        let failures_here = match rule {
            Rule::ActionMatch
                if matches!(
                    source.path.as_str(),
                    "shell/src/main.rs" | "shell/src/launch.rs" | "shell/src/window_services.rs"
                ) =>
            {
                action_match(&code)
                    .map(|at| format!("{}: match over Action", line_at(at)))
                    .into_iter()
                    .collect()
            }
            Rule::TryBorrow if source.path != "shell/src/gtk_host/services.rs" => ["try_borrow", "try_borrow_mut"]
                .into_iter()
                .filter(|name| has(&words, &[name, "("]))
                .map(str::to_owned)
                .collect(),
            Rule::BorrowGuard if source.path == "shell/src/gtk_host/services.rs" => binds_borrow_guard(&words)
                .map(|word| format!("{}: let binds a borrow guard", line_at(offset(&code, word))))
                .into_iter()
                .collect(),
            Rule::MutableLayout => ["Layout", "Workspaces"]
                .into_iter()
                .filter(|ty| {
                    has(&words, &["RefCell", "<", ty, ">"])
                        || has(&words, &["RefCell", "<", "eitri_core", "::", "layout", "::", ty, ">"])
                })
                .map(|ty| format!("RefCell<{ty}>"))
                .collect(),
            Rule::GridFunctions if source.path == "shell/src/module_grid.rs" => {
                let found = grid_functions(&words);
                (found != GRID_FUNCTIONS)
                    .then(|| format!("non-private functions: {found:?}; expected {GRID_FUNCTIONS:?}"))
                    .into_iter()
                    .collect()
            }
            Rule::Transitional => transitional(source, &code, &words),
            _ => Vec::new(),
        };
        failures.extend(failures_here.into_iter().map(|failure| match rule {
            Rule::ActionMatch | Rule::BorrowGuard => format!("{}:{failure}", source.path),
            _ => format!("{}: {failure}", source.path),
        }));
    }
    failures
}

fn check_shell(rule: Rule) {
    let sources = scan::rust_sources(&["shell/src"]);
    for path in [
        "shell/src/main.rs",
        "shell/src/launch.rs",
        "shell/src/window_services.rs",
        "shell/src/module_grid.rs",
        "shell/src/gtk_host/services.rs",
    ] {
        assert!(sources.iter().any(|s| s.path == path), "missing scan anchor {path}");
    }
    assert_eq!(violations(rule, &sources), Vec::<String>::new(), "{rule:?}");
}

#[test]
fn no_match_over_action_in_main_rs() {
    check_shell(Rule::ActionMatch);
}

#[test]
fn no_try_borrow_in_the_shell_outside_its_services() {
    check_shell(Rule::TryBorrow);
}

#[test]
fn the_services_bind_no_borrow_guard() {
    check_shell(Rule::BorrowGuard);
}

#[test]
fn the_shell_holds_no_mutable_layout() {
    check_shell(Rule::MutableLayout);
}

#[test]
fn the_grid_has_no_verb_forwarders() {
    check_shell(Rule::GridFunctions);
}

#[test]
fn no_transitional_item_is_left() {
    let sources = scan::rust_sources(&["core/src", "shell/src"]);
    for path in ["driver.rs", "controller.rs", "lua.rs", "verbs/mod.rs"] {
        let path = format!("core/src/wm/{path}");
        assert!(sources.iter().any(|s| s.path == path), "missing scan anchor {path}");
    }
    let test_modules = file_test_modules(&sources);
    for ty in ["Controller", "Driver"] {
        assert!(
            sources
                .iter()
                .filter(|source| source.path.starts_with("core/src/wm/"))
                .any(|source| !scan::impl_blocks(&production_code(source, &test_modules), ty).is_empty()),
            "no {ty} impl block matched under core/src/wm/"
        );
    }
    assert_eq!(violations(Rule::Transitional, &sources), Vec::<String>::new());
}

fn fixture(path: &str, text: &str) -> Source {
    Source {
        path: path.into(),
        text: text.into(),
    }
}

fn caught(rule: Rule, path: &str, text: &str) {
    let failures = violations(rule, &[fixture(path, text)]);
    assert!(!failures.is_empty(), "{rule:?} missed {text}");
    assert!(failures.iter().all(|failure| failure.starts_with(path)));
}

#[test]
fn action_match_mutants_are_caught_without_rejecting_constructors() {
    for text in [
        "fn run(a: Action) { match a { _ => () } }",
        "fn run(a: &eitri_core::keymap::Action) { match a { _ => () } }",
        "fn run() { match input { Action::Zoom => (), _ => () } }",
        "fn run(){match input{other=>{},Action :: Zoom=>()}}",
        "fn run(){match input{other=>{} Action::Zoom=>{}}}",
        "fn run() { let renamed = Action::Zoom; match renamed { _ => () } }",
        "fn run() { if let Action::Zoom = input {} }",
        "fn run() { let Action::Zoom = input else { return }; }",
    ] {
        for path in [
            "shell/src/main.rs",
            "shell/src/launch.rs",
            "shell/src/window_services.rs",
        ] {
            caught(Rule::ActionMatch, path, text);
        }
    }
    assert!(violations(
        Rule::ActionMatch,
        &[fixture("shell/src/main.rs", "match x { _ => run(Action::Zoom), }")]
    )
    .is_empty());
}

#[test]
fn a_try_borrow_in_a_new_shell_file_is_caught() {
    for text in ["fn f(){ x.try_borrow(); }", "fn f(){ x.\ntry_borrow_mut\n(); }"] {
        caught(Rule::TryBorrow, "shell/src/new_service.rs", text);
        assert!(violations(Rule::TryBorrow, &[fixture("shell/src/gtk_host/services.rs", text)]).is_empty());
    }
}

#[test]
fn services_guard_mutants_are_caught_but_values_are_allowed() {
    for text in [
        "let g = x.borrow();",
        "let\nmut g = x . borrow_mut ( ) ;",
        "let (a, b) = x.borrow();",
        "let g = (x.borrow());",
    ] {
        caught(Rule::BorrowGuard, "shell/src/gtk_host/services.rs", text);
    }
    for text in [
        "let value = x.borrow().clone();",
        "let value = x.borrow_mut().take();",
        "let value = { let g = x.borrow(); g.len() };",
    ] {
        let failures = violations(Rule::BorrowGuard, &[fixture("shell/src/gtk_host/services.rs", text)]);
        if text.contains("let g") {
            assert!(!failures.is_empty(), "a nested guard is still held across statements");
        } else {
            assert!(failures.is_empty(), "{text}: {failures:?}");
        }
    }
}

#[test]
fn mutable_layout_fields_are_caught_in_a_new_file() {
    for ty in [
        "Layout",
        "Workspaces",
        "eitri_core::layout::Layout",
        "eitri_core :: layout :: Workspaces",
    ] {
        caught(
            Rule::MutableLayout,
            "shell/src/new_layout.rs",
            &format!("struct Bad {{ state: RefCell < {ty} > }}"),
        );
    }
    assert!(violations(
        Rule::MutableLayout,
        &[fixture(
            "shell/src/new_layout.rs",
            "struct Good { layout: LayoutView, workspaces: WorkspacesView }"
        )]
    )
    .is_empty());
}

#[test]
fn a_new_grid_forwarder_or_a_deleted_adapter_is_caught() {
    let base = GRID_FUNCTIONS
        .iter()
        .map(|name| format!("pub(crate) fn {name}() {{}} "))
        .collect::<String>();
    assert!(violations(Rule::GridFunctions, &[fixture("shell/src/module_grid.rs", &base)]).is_empty());
    caught(
        Rule::GridFunctions,
        "shell/src/module_grid.rs",
        &format!("{base} pub(crate)\nfn navigate() {{}}"),
    );
    caught(
        Rule::GridFunctions,
        "shell/src/module_grid.rs",
        &base.replace("pub(crate) fn add() {}", ""),
    );
}

#[test]
fn transitional_mutants_are_caught_by_item_and_receiver() {
    for name in [
        "ShellVerb",
        "ShellCall",
        "run_shell_verb",
        "set_shell_verb_arm",
        "LayoutChangedByShell",
        "ShellRefused",
        "PromptWithdrawn",
    ] {
        caught(Rule::Transitional, "shell/src/new.rs", &format!("use old::{name};"));
    }
    for (file, ty, names) in [
        (
            "driver.rs",
            "Driver",
            vec![
                "lua_request",
                "keymap_opened",
                "keymap_superseded",
                "persist_now",
                "focus_now",
                "scratch_new",
                "tab_break",
                "reveal_chat",
            ],
        ),
        (
            "controller.rs",
            "Controller",
            vec![
                "persist_now",
                "keymap_opened",
                "keymap_superseded",
                "scratch_new",
                "tab_break",
                "reveal_chat",
                "shell_revision",
            ],
        ),
    ] {
        for name in names {
            caught(
                Rule::Transitional,
                &format!("core/src/wm/{file}"),
                &format!("impl<T> {ty}<T> {{ fn\n{name}() {{}} }}"),
            );
        }
    }
    for path in ["core/src/wm/lua.rs", "core/src/wm/verbs/mod.rs"] {
        caught(Rule::Transitional, path, "fn\nmoved() {} ");
    }
    for name in ["lua_request", "persist_now", "focus_now"] {
        caught(Rule::Transitional, "shell/src/new.rs", &format!("driver . {name} ();"));
        caught(
            Rule::Transitional,
            "shell/src/new.rs",
            &format!("fn f(renamed: &Driver<Controller>) {{ let alias = renamed.clone(); alias.{name}(); }}"),
        );
        caught(
            Rule::Transitional,
            "shell/src/new.rs",
            &format!("let alias = driver.clone(); alias.{name}();"),
        );
    }
    for (path, text) in [
        (
            "core/src/wm/controller.rs",
            "impl Controller { fn lua_request() {} } fn scratch_new() {} fn tab_break() {} fn reveal_chat() {}",
        ),
        (
            "core/src/wm/driver.rs",
            "impl Driver<Controller> { fn kill_confirmed() {} } fn persist_now() {} impl Other { fn focus_now() {} }",
        ),
        (
            "core/src/wm/verbs/modules.rs",
            "fn scratch_new() {} fn tab_break() {} fn reveal_chat() {}",
        ),
        ("core/src/wm/verbs/chat.rs", "fn tab_break() {}"),
        ("core/src/wm/focus.rs", "fn keymap_opened() {}"),
        ("core/src/wm/lua.rs", "fn lua_request_action() {}"),
        ("shell/src/new.rs", "controller.lua_request();"),
    ] {
        assert!(
            violations(Rule::Transitional, &[fixture(path, text)]).is_empty(),
            "{path}: {text}"
        );
    }
}

#[test]
fn comments_literals_and_test_modules_cannot_trigger_the_scans() {
    let text = r##"
        // match action { Action::Zoom => () }; x.try_borrow(); let g = x.borrow();
        const TEXT: &str = r#"RefCell<Layout> ShellVerb pub(crate) fn navigate() {}"#;
        #[cfg(test)] mod tests {
            fn bad(a: Action) { match a { Action::Zoom => () }; x.try_borrow(); }
            struct Bad { state: RefCell<Workspaces> }
            let g = x.borrow_mut();
            fn moved() {} fn run_shell_verb() {}
        }
        #[cfg(test)] mod differently_named {
            fn bad() { x.try_borrow(); let g = x.borrow_mut(); }
            struct Bad { state: RefCell<Layout> }
            fn moved() {} fn run_shell_verb() {}
        }
    "##;
    for (rule, path) in [
        (Rule::ActionMatch, "shell/src/main.rs"),
        (Rule::TryBorrow, "shell/src/other.rs"),
        (Rule::BorrowGuard, "shell/src/gtk_host/services.rs"),
        (Rule::MutableLayout, "shell/src/other.rs"),
        (Rule::Transitional, "core/src/wm/lua.rs"),
    ] {
        assert!(violations(rule, &[fixture(path, text)]).is_empty(), "{rule:?}");
    }
}

#[test]
fn transitional_impls_in_any_controller_file_are_caught() {
    caught(
        Rule::Transitional,
        "core/src/wm/start.rs",
        "impl Controller { fn reveal_chat() {} }",
    );
    caught(
        Rule::Transitional,
        "core/src/wm/extra/adapter.rs",
        "impl<T> HostSink for Driver<T> { fn focus_now() {} }",
    );
}

#[test]
fn file_test_modules_are_excluded_but_production_modules_are_scanned() {
    let bad = "fn run_shell_verb() {}";
    for path in ["core/src/wm/tests.rs", "core/src/wm/tests/helper.rs"] {
        assert!(
            violations(Rule::Transitional, &[fixture(path, bad)]).is_empty(),
            "{path}"
        );
    }
    for (parent, child, descendant) in [
        (
            "core/src/wm/mod.rs",
            "core/src/wm/checks.rs",
            "core/src/wm/checks/helper.rs",
        ),
        (
            "core/src/wm/start.rs",
            "core/src/wm/start/checks/mod.rs",
            "core/src/wm/start/checks/helper.rs",
        ),
    ] {
        let sources = [
            fixture(
                parent,
                "#[cfg
(test)]
mod
checks;",
            ),
            fixture(child, bad),
            fixture(descendant, bad),
        ];
        assert!(violations(Rule::Transitional, &sources).is_empty(), "{parent}");
        let sources = [fixture(parent, "mod checks;"), fixture(child, bad)];
        assert_eq!(violations(Rule::Transitional, &sources).len(), 1);
    }
    let sources = [
        fixture(
            "shell/src/main.rs",
            "#[cfg(test)]\n#[path = \"main_tests.rs\"]\nmod tests;",
        ),
        fixture("shell/src/main_tests.rs", bad),
    ];
    assert_eq!(
        scan::test_only_files(&sources),
        ["shell/src/main_tests.rs".to_owned()].into()
    );
    assert!(violations(Rule::Transitional, &sources).is_empty());
    assert!(scan::test_only_files(&scan::rust_sources(&["shell/src"])).contains("shell/src/main_tests.rs"));
    let sources = [
        fixture("core/src/wm/mod.rs", "#[cfg(test)] pub(crate) mod checks;"),
        fixture("core/src/wm/checks.rs", bad),
    ];
    assert!(violations(Rule::Transitional, &sources).is_empty());
    assert!(violations(
        Rule::Transitional,
        &[fixture(
            "core/src/wm/mod.rs",
            "#[cfg(test)] pub(super) mod checks { fn run_shell_verb() {} }"
        )]
    )
    .is_empty());
    caught(
        Rule::Transitional,
        "core/src/wm/start.rs",
        "mod tests { fn run_shell_verb() {} }",
    );
    caught(Rule::Transitional, "core/src/wm/start.rs", bad);
}

#[test]
fn unicode_comments_and_literals_preserve_violation_lines() {
    for (rule, path, violation) in [
        (Rule::ActionMatch, "shell/src/main.rs", "matches!(input, Action::Zoom);"),
        (
            Rule::BorrowGuard,
            "shell/src/gtk_host/services.rs",
            "let guard = x.borrow();",
        ),
    ] {
        let text = format!(
            "// § note\n/* 多\n行 */\nconst TEXT: &str = \"é\n文\";\n#[cfg(test)] mod tests {{\nfn ignored() {{}}\n}}\n{violation}"
        );
        assert_eq!(
            violations(rule, &[fixture(path, &text)]),
            vec![format!(
                "{path}:9: {}",
                match rule {
                    Rule::ActionMatch => "match over Action",
                    Rule::BorrowGuard => "let binds a borrow guard",
                    _ => unreachable!(),
                }
            )]
        );
    }
}

#[test]
fn matches_action_patterns_are_caught_and_report_their_line() {
    for text in [
        "fn run() {

 matches!(input, Action :: Zoom); }",
        "fn run() {

 matches!
(input, Some(Action
::Zoom) | Action::Tab(_)); }",
        "fn run() {

 matches!(f(a, b), Action::Zoom if ready()); }",
    ] {
        let failures = violations(Rule::ActionMatch, &[fixture("shell/src/main.rs", text)]);
        assert!(
            failures.iter().any(|s| s.starts_with("shell/src/main.rs:3:")),
            "{failures:?}"
        );
    }
    for text in [
        "matches!(Action::Zoom, _)",
        "matches!(input, _ if allowed(Action::Zoom))",
    ] {
        assert!(violations(Rule::ActionMatch, &[fixture("shell/src/main.rs", text)]).is_empty());
    }
}

#[test]
fn borrow_guard_failures_report_the_binding_line() {
    let failures = violations(
        Rule::BorrowGuard,
        &[fixture(
            "shell/src/gtk_host/services.rs",
            "fn run() {
 let value = x.borrow().clone();
 let guard = x
.borrow();
}",
        )],
    );
    assert!(
        failures
            .iter()
            .any(|s| s.starts_with("shell/src/gtk_host/services.rs:3:")),
        "{failures:?}"
    );
}

#[test]
fn grid_public_functions_of_every_visibility_are_caught() {
    let base = GRID_FUNCTIONS
        .iter()
        .map(|name| format!("pub(crate) fn {name}() {{}} "))
        .collect::<String>();
    for visibility in ["pub", "pub(super)", "pub ( in crate )", "pub unsafe extern \"C\""] {
        caught(
            Rule::GridFunctions,
            "shell/src/module_grid.rs",
            &format!(
                "{base} {visibility}
fn navigate() {{}}"
            ),
        );
    }
    assert!(violations(
        Rule::GridFunctions,
        &[fixture(
            "shell/src/module_grid.rs",
            &format!("{base} fn private() {{}}")
        )]
    )
    .is_empty());
}
