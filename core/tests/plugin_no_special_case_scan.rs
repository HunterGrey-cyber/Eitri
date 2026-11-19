//! No host code names a plugin it serves: everything a plugin gets is a capability, a manifest table, a setting kind
//! or a load option any plugin may declare. Reads the host's sources as text -- literals kept, since a special case
//! would hide in one -- minus each file's test module, and fails on a plugin name in any letter case.

use eitri_core::source_scan::{self as scan, Source};

/// Names no host code may hold, lower case.
const NAMES: &[&str] = &["markdown", "snotra", "agents-strips", "agents-top", "agents-pass"];

/// The `file:line: text` of each non-test line naming a plugin in any letter case.
fn hits(path: &str, text: &str) -> Vec<String> {
    let production;
    let text = if path.ends_with(".rs") {
        production = scan::production_text(text);
        &production
    } else {
        text
    };
    text.lines()
        .enumerate()
        .filter(|(_, line)| NAMES.iter().any(|name| line.to_lowercase().contains(name)))
        .map(|(n, line)| format!("{path}:{}: {}", n + 1, line.trim()))
        .collect()
}

fn in_tests(path: &str) -> bool {
    path.split('/').any(|part| part == "tests" || part == "tests.rs")
}

fn plugin_source(path: &str) -> bool {
    let Some((_, relative)) = path.split_once("/src/") else {
        return false;
    };
    !in_tests(relative)
        && relative.split('/').any(|part| {
            matches!(part, "plugin" | "plugin.rs" | "plugins" | "plugins.rs")
                || part.starts_with("plugin_")
                || part.starts_with("plugin-")
        })
}

fn rust_host_sources(sources: Vec<Source>) -> Vec<Source> {
    let test_files = scan::test_only_files(&sources);
    sources
        .into_iter()
        .filter(|source| !test_files.contains(&source.path))
        .filter(|source| {
            plugin_source(&source.path)
                || (!in_tests(&source.path)
                    && (source.path.starts_with("core/src/wm/") || source.path.starts_with("core/src/layout/")))
        })
        .collect()
}

fn host_sources() -> Vec<Source> {
    let mut sources = rust_host_sources(scan::product_sources());
    sources.extend(scan::sources(&["core/src/plugin"], &["lua", "js"]));
    sources
}

#[test]
fn a_test_only_plugins_file_is_not_host_code() {
    let sources = [
        ("desk/src/lib.rs", "#[cfg(test)] mod plugins; mod plugin_routes;"),
        ("desk/src/plugins.rs", "const ID: &str = \"markdown\";"),
        ("desk/src/plugin_routes.rs", "const ID: &str = \"snotra\";"),
    ]
    .map(|(path, text)| Source {
        path: path.into(),
        text: text.into(),
    });
    let sources = rust_host_sources(sources.into());
    let found: Vec<_> = sources
        .iter()
        .flat_map(|source| hits(&source.path, &source.text))
        .collect();
    assert_eq!(found, ["desk/src/plugin_routes.rs:1: const ID: &str = \"snotra\";"]);
}

#[test]
fn a_plugin_file_declared_in_production_and_tests_is_scanned() {
    for newline in ["\n", "\r\n"] {
        let sources = [
            ("desk/src/lib.rs", "mod plugins;\n#[cfg(test)] #[path = \"plugins.rs\"] mod test_plugins;\n#[cfg(test,)] mod plugin_fixtures;"),
            (
                "desk/src/plugin_fixtures.rs",
                "#[path = \"plugins.rs\"] mod shared;\nmod private;",
            ),
            ("desk/src/plugin_fixtures/private.rs", "const ID: &str = \"markdown\";"),
            ("desk/src/plugins.rs", "mod routes;\nconst ID: &str = \"markdown\";"),
            ("desk/src/plugins/routes.rs", "const ID: &str = \"snotra\";"),
        ]
        .map(|(path, text)| Source {
            path: path.into(),
            text: text.replace('\n', newline),
        });
        let found: Vec<_> = rust_host_sources(sources.into())
            .iter()
            .flat_map(|source| hits(&source.path, &source.text))
            .collect();
        assert_eq!(
            found,
            [
                "desk/src/plugins.rs:2: const ID: &str = \"markdown\";",
                "desk/src/plugins/routes.rs:1: const ID: &str = \"snotra\";",
            ]
        );
    }
}

#[test]
fn a_new_plugin_module_under_desk_is_scanned() {
    let path = "desk/src/plugin/mod.rs";
    assert!(plugin_source(path));
    assert_eq!(hits(path, "fn route() { if id == \"Markdown\" {} } ").len(), 1);
    assert!(plugin_source("desk/src/plugin_routes/dispatch.rs"));
    assert!(!plugin_source("desk/src/plugin/tests/routes.rs"));
    assert!(!plugin_source("desk/src/non_plugin/routes.rs"));
}

#[test]
fn plugins_style_names_count_as_plugin_sources() {
    for path in [
        "desk/src/plugins.rs",
        "desk/src/plugins/routes.rs",
        "desk/src/plugin-page.rs",
        "desk/src/plugin-pages/routes.rs",
    ] {
        assert!(plugin_source(path), "{path}");
        for newline in ["\n", "\r\n"] {
            let source = format!("#[cfg(test)] mod checks {{ const ID: &str = \"markdown\"; }}{newline}fn route() {{ inspect(\"snotra\"); }}");
            assert_eq!(
                hits(path, &source),
                [format!("{path}:2: fn route() {{ inspect(\"snotra\"); }}")]
            );
        }
    }
    for path in [
        "desk/src/non_plugin/routes.rs",
        "desk/src/tests/plugins.rs",
        "desk/src/plugins/tests.rs",
    ] {
        assert!(!plugin_source(path), "{path}");
    }
}

#[test]
fn no_host_code_names_a_plugin() {
    let sources = host_sources();
    scan::require_anchors(&sources, &["pub fn parse_manifest(", "impl PluginHost", "fn serve("]);
    for path in [
        "shell/src/plugin_page.rs",
        "shell/src/plugin_panels.rs",
        "core/src/lua/plugin.rs",
        "core/src/plugin/bridge.js",
        "core/src/wm/start.rs",
        "core/src/wm/text_size.rs",
        "core/src/layout/persist.rs",
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
    // A byte offset into `code_only`'s output is one into the text: multi-byte characters in a comment ahead of the
    // test module keep the cut at the module.
    let wide = format!(
        "// {}\nconst A: &str = \"markdown\";\n#[cfg(test)]\nmod tests {{\n    \"markdown\";\n}}\n",
        "\u{2014}".repeat(20)
    );
    assert_eq!(hits("x.rs", &wide), ["x.rs:2: const A: &str = \"markdown\";"]);
    // A Lua or JavaScript file has no test module to skip.
    assert_eq!(
        hits("x.lua", "-- #[cfg(test)]\nmod tests\nlocal ft = 'MARKDOWN'\n").len(),
        1
    );
}

#[test]
fn both_names_are_caught_and_test_module_formatting_does_not_matter() {
    assert_eq!(hits("x.rs", "const ID: &str = \"SNOTRA\";").len(), 1);
    assert!(hits(
        "x.rs",
        "fn host() {} #[cfg(test)] mod tests { const ID: &str = \"snotra\"; }"
    )
    .is_empty());
    assert!(!host_sources()
        .iter()
        .any(|source| source.path.ends_with("/tests.rs") || source.path.contains("/tests/")));
}

#[test]
fn face_ids_are_refused_but_the_shared_capability_is_allowed() {
    for id in ["AGENTS-STRIPS", "agents-top", "Agents-Pass"] {
        assert_eq!(hits("x.rs", &format!("const ID: &str = {id:?};")).len(), 1);
    }
    assert!(hits("x.rs", r#"const CAPABILITY: &str = "agents";"#).is_empty());
}
