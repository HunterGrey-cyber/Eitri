//! A forced UI move is safe only in the disposable waiting room.
use eitri_core::source_scan::{code_only, modules, product_dirs_except, sources, test_only_files, without, word_ends};

fn forced_connect(text: &str) -> bool {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if text.contains("connect!") {
        return true;
    }
    // Read the whole file so storing the bang table in a variable cannot hide its later use.
    compact.contains("bang=true")
        && word_ends(&compact, "connect")
            .into_iter()
            .any(|end| !handle_method(&compact, end))
}

/// Whether the `connect` ending at `end` is a method of a handle other than `vim.cmd` (`pipe.connect`,
/// `pipe:connect`, a libuv socket), which moves no UI. A file that stores `vim.cmd` under another name counts every
/// `connect` it has, since the alias would make `alias.connect` the command.
fn handle_method(compact: &str, end: usize) -> bool {
    let before = &compact[..end - "connect".len()];
    let Some(receiver) = before.strip_suffix('.').or_else(|| before.strip_suffix(':')) else {
        return false;
    };
    let name_start = receiver
        .char_indices()
        .rev()
        .find(|(_, c)| !(c.is_alphanumeric() || *c == '_'))
        .map_or(0, |(at, c)| at + c.len_utf8());
    let name = &receiver[name_start..];
    !name.is_empty() && name != "cmd" && !compact.contains("=vim.cmd")
}

#[test]
fn connect_bang_only_in_the_waiting_room() {
    // Every product crate's `src`; the terminal crates are read whole (tests and fixtures stay out below).
    let mut dirs = product_dirs_except(&[
        "terminal/engine/src",
        "terminal/frame/src",
        "terminal/input/src",
        "terminal/render/src",
        "terminal/sync/src",
    ]);
    dirs.extend([
        "terminal",
        "plugin-kit/agents/src",
        "nvim",
        "plugins",
        "plugin-kit/keys/src",
        "agent-ui/web/src",
    ]);
    let mut files = sources(&dirs, &["rs", "lua", "ts", "tsx", "js"]);
    files.retain(|file| {
        !file.path.contains("/node_modules/")
            && !file.path.contains("/dist/")
            && !file.path.contains("/tests/")
            && !file.path.contains("/testkit/")
            && !file.path.contains(".test.")
    });
    let rust_files: Vec<_> = files
        .iter()
        .filter(|file| file.path.ends_with(".rs"))
        .cloned()
        .collect();
    let test_files = test_only_files(&rust_files);
    let mut found_room = false;
    for file in files {
        if test_files.contains(&file.path) || file.path.contains("/tests/") || file.path.contains("/testkit/") {
            continue;
        }
        let text = if file.path.ends_with(".rs") {
            let code = code_only(&file.text);
            let tests = modules(&code, "tests");
            // Keep embedded Lua visible while excluding test bodies, including after non-ASCII comments.
            let raw_tests: Vec<&str> = tests
                .iter()
                .map(|body| {
                    let at = body.as_ptr() as usize - code.as_ptr() as usize;
                    let start_chars = code[..at].chars().count();
                    let end_chars = start_chars + body.chars().count();
                    let raw_at = |n| file.text.char_indices().nth(n).map_or(file.text.len(), |(i, _)| i);
                    &file.text[raw_at(start_chars)..raw_at(end_chars)]
                })
                .collect();
            without(&file.text, &raw_tests)
        } else {
            file.text
        };
        let forced = forced_connect(&text);
        if file.path == "core/src/takeover/waiting_room.lua" {
            assert!(forced, "the room's forced return must remain covered");
            found_room = true;
        } else {
            assert!(
                !forced,
                "{} can force a UI away from a server with unsaved buffers",
                file.path
            );
        }
    }
    assert!(found_room, "the waiting room must be among the scanned sources");
}

#[test]
fn forced_connect_is_found_across_line_breaks_and_table_field_orders() {
    for code in [
        "vim.cmd('connect! /room')",
        "vim.cmd.connect {\n args = {origin},\n bang =\n true }",
        "pcall(vim.cmd.connect, { args = { origin }, bang = true })",
        "vim.api.nvim_cmd({bang = true,\ncmd = 'connect'}, {})",
        "vim.api.nvim_cmd({cmd = 'connect',\nbang = true}, {})",
        "local opts = {args = {'{'}, bang = true}; vim.cmd.connect(opts)",
        "local opts = {bang = true}; vim.cmd['connect'](opts)",
    ] {
        assert!(forced_connect(code), "a forced connection escaped: {code}");
    }
    assert!(!forced_connect("vim.cmd.connect(...)"));
    assert!(!forced_connect("{ cmd = 'tabclose', bang = true }"));
    // A libuv pipe's connect beside a `normal!` moves no UI; an alias of vim.cmd still counts.
    let pipe =
        "pcall(pipe.connect, pipe, path, cb); h:connect(path); vim.api.nvim_cmd({ cmd = 'normal', bang = true }, {})";
    assert!(!forced_connect(pipe));
    for code in [
        "local c = vim.cmd; c.connect({ args = { origin }, bang = true })",
        "pipe:connect(p); vim.cmd.connect({ args = { origin }, bang = true })",
        "pipe.connect(p); vim.api.nvim_cmd({ cmd = 'connect', bang = true }, {})",
    ] {
        assert!(forced_connect(code), "a forced connection escaped: {code}");
    }
}
