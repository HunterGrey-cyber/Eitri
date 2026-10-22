//! No host code names a plugin it serves: everything a plugin gets is a capability, a manifest table, a setting kind
//! or a load option any plugin may declare. Reads the host's sources as text -- literals kept, since a special case
//! would hide in one -- minus each file's test module, and fails on "markdown" in any letter case.

use eitri_core::source_scan::{self as scan, Source};

/// The name no host code may hold, lower case.
const NAME: &str = "markdown";

/// Where a Rust file's test module starts; it ends each of the files read.
const TEST_MODULE: &str = "#[cfg(test)]\nmod tests";

/// The `file:line: text` of each line of `text` naming [`NAME`] in any letter case; a Rust file is read only up to
/// its test module.
fn hits(path: &str, text: &str) -> Vec<String> {
    let text = if path.ends_with(".rs") {
        text.find(TEST_MODULE).map_or(text, |at| &text[..at])
    } else {
        text
    };
    text.lines()
        .enumerate()
        .filter(|(_, line)| line.to_lowercase().contains(NAME))
        .map(|(n, line)| format!("{path}:{}: {}", n + 1, line.trim()))
        .collect()
}

fn host_sources() -> Vec<Source> {
    let mut sources = scan::rust_sources(&["core/src/plugin"]);
    sources.push(scan::read_required("core/src/lua/plugin.rs"));
    sources.extend(scan::rust_sources(&["shell/src"]).into_iter().filter(|s| {
        s.path
            .rsplit('/')
            .next()
            .is_some_and(|name| name.starts_with("plugin_"))
    }));
    sources.extend(scan::sources(&["core/src/plugin"], &["lua", "js"]));
    sources
}

#[test]
fn no_host_code_names_a_plugin() {
    let sources = host_sources();
    scan::require_anchors(&sources, &["pub fn parse_manifest(", "impl PluginHost", "fn serve("]);
    for path in [
        "shell/src/plugin_page.rs",
        "shell/src/plugin_panels.rs",
        "core/src/plugin/bridge.js",
    ] {
        assert!(sources.iter().any(|s| s.path == path), "{path} is read");
    }
    let found: Vec<String> = sources.iter().flat_map(|s| hits(&s.path, &s.text)).collect();
    assert!(
        found.is_empty(),
        "host code names a plugin; give it a capability, a manifest table or a setting instead:\n{}",
        found.join("\n")
    );
}

#[test]
fn the_matcher_reads_literals_and_skips_only_the_test_module() {
    let special = "fn route(id: &str) {\n    if id == \"Markdown\" {\n        return;\n    }\n}\n";
    assert_eq!(hits("x.rs", special), ["x.rs:2: if id == \"Markdown\" {"]);
    let in_tests =
        "fn route(id: &str) {}\n\n#[cfg(test)]\nmod tests {\n    fn t() {\n        let _ = \"Markdown\";\n    }\n}\n";
    assert!(hits("x.rs", in_tests).is_empty());
    // A Lua or JavaScript file has no test module to skip.
    assert_eq!(
        hits("x.lua", "-- #[cfg(test)]\nmod tests\nlocal ft = 'MARKDOWN'\n").len(),
        1
    );
}
