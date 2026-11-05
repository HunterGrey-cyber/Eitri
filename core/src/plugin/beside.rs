//! Which local pages can run beside a person's own nvim, without an Eitri editor or layout.

use std::collections::BTreeSet;

use super::manifest::{id_ok, parse_manifest, Manifest};
use super::search::{candidates, find_plugin, SearchEnv, MANIFEST};

pub const BESIDE_CAPABILITIES: [&str; 5] = ["editor.follow", "editor.commands", "editor.open", "state", "net"];

/// Names the first capability that needs the full window, preserving manifest order.
pub fn check(manifest: &Manifest) -> Result<(), String> {
    let refused = manifest
        .capabilities
        .iter()
        .map(|capability| capability.as_str())
        .find(|name| !BESIDE_CAPABILITIES.contains(name))
        .or_else(|| manifest.window.as_ref().map(|_| "[window]"));
    match refused {
        Some(name) => Err(format!("{} needs Eitri's own window (it declares {name})", manifest.id)),
        None => Ok(()),
    }
}

/// Sorted eligible ids, using the same first-copy-wins search as a named plugin load.
pub fn list(env: &SearchEnv) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for (base, _) in candidates("", env) {
        let Ok(entries) = std::fs::read_dir(base) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Some(id) = entry.file_name().to_str().filter(|id| id_ok(id)) {
                ids.insert(id.to_owned());
            }
        }
    }
    ids.into_iter()
        .filter(|id| {
            let Ok(found) = find_plugin(id, env) else {
                return false;
            };
            let Ok(text) = std::fs::read_to_string(found.dir.join(MANIFEST)) else {
                return false;
            };
            parse_manifest(&text).is_ok_and(|manifest| manifest.id == *id && check(&manifest).is_ok())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest::{parse_manifest, Capability};
    use crate::plugin::search::{candidates, MANIFEST};
    use crate::plugin::test_support::Scratch;
    use std::path::Path;

    fn manifest() -> crate::plugin::manifest::Manifest {
        parse_manifest(
            r#"id = "notes"
title = "Notes"
protocol = 1
site = "site"
entry = "index.html"
"#,
        )
        .unwrap()
    }

    #[test]
    fn agents_is_refused_beside() {
        let mut m = manifest();
        m.capabilities = vec![Capability::Agents];
        assert_eq!(
            check(&m),
            Err("notes needs Eitri's own window (it declares agents)".into())
        );
    }

    #[test]
    fn check_admits_exactly_the_five_and_names_the_first_out() {
        assert_eq!(
            BESIDE_CAPABILITIES,
            ["editor.follow", "editor.commands", "editor.open", "state", "net"]
        );
        let mut m = manifest();
        assert_eq!(check(&m), Ok(()));
        for cap in Capability::ALL {
            m.capabilities = vec![cap];
            if BESIDE_CAPABILITIES.contains(&cap.as_str()) {
                assert_eq!(check(&m), Ok(()), "{}", cap.as_str());
            } else {
                assert_eq!(
                    check(&m),
                    Err(format!("notes needs Eitri's own window (it declares {})", cap.as_str()))
                );
            }
        }
        m.capabilities = vec![Capability::Net, Capability::EditorBuffers, Capability::Tasks];
        assert_eq!(
            check(&m),
            Err("notes needs Eitri's own window (it declares editor.buffers)".into())
        );
        m.capabilities.reverse();
        assert_eq!(
            check(&m),
            Err("notes needs Eitri's own window (it declares tasks)".into())
        );
        m = parse_manifest(
            r#"id = "notes"
title = "Notes"
protocol = 1
site = "site"
entry = "index.html"
[roots]
work = "${plugin}"
[window]
root = "${plugin}"
title = "Notes"
layout = "page"
"#,
        )
        .unwrap();
        assert_eq!(
            check(&m),
            Err("notes needs Eitri's own window (it declares [window])".into())
        );
    }

    fn write_plugin(dir: &Path, id: &str, caps: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(MANIFEST), format!(
            "id = {id:?}\ntitle = \"Notes\"\nprotocol = 1\nsite = \"site\"\nentry = \"index.html\"\ncapabilities = [{caps}]\n[roots]\nwork = \"${{plugin}}\"\n"
        )).unwrap();
    }

    #[test]
    fn list_reads_the_search_path() {
        let scratch = Scratch::new("beside-list");
        let env = SearchEnv {
            xdg_data_home: Some(scratch.0.join("user").into_os_string()),
            home: None,
            exe_dir: Some(scratch.0.join("bin")),
            source_plugins: Some(scratch.0.join("src")),
            xdg_data_dirs: Some(scratch.0.join("system").into_os_string()),
        };
        for (step, id) in ["zeta", "alpha", "middle", "omega"].into_iter().enumerate() {
            write_plugin(&candidates(id, &env)[step].0, id, "\"state\", \"net\"");
        }
        write_plugin(&candidates("zeta", &env)[3].0, "zeta", "");
        write_plugin(&candidates("blocked", &env)[0].0, "blocked", "\"editor.assist\"");
        write_plugin(&candidates("blocked", &env)[1].0, "blocked", "");
        write_plugin(&candidates("broken", &env)[0].0, "broken", "");
        std::fs::write(candidates("broken", &env)[0].0.join(MANIFEST), "not toml [").unwrap();
        write_plugin(&candidates("broken", &env)[1].0, "broken", "");
        write_plugin(&candidates("mismatch", &env)[0].0, "other", "");
        write_plugin(&candidates("Not-an-id", &env)[0].0, "other", "");
        assert_eq!(list(&env), vec!["alpha", "middle", "omega", "zeta"]);
        let fallback = SearchEnv {
            xdg_data_home: Some("relative".into()),
            home: Some(scratch.0.join("home")),
            ..env
        };
        write_plugin(&candidates("fallback", &fallback)[0].0, "fallback", "");
        assert_eq!(
            list(&fallback),
            vec!["alpha", "blocked", "broken", "fallback", "middle", "omega", "zeta"]
        );
    }
}
