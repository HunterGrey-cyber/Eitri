//! D7's rules, per project (keymap/tabs spec §4.1): `$XDG_STATE_HOME/neovibe/permissions/<16 hex>.json`.
//! Claude Code writes `.claude/settings.local.json` into the repository; neovibe writes nothing into
//! the project. Removing a rule is editing this file; `prefix i` shows its path. An unusable file is
//! set aside as `.json.unusable` and read as no rules, which fails toward more cards.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use agent::{PrefixRule, PrefixRules};
use serde::{Deserialize, Serialize};

use crate::layout::persist::{file_name, state_subdir, temporary};

const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct RulesFile {
    version: u32,
    project_root: String,
    rules: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LoadedRules {
    Rules(PrefixRules),
    Missing,
    Unusable(String),
}

pub fn state_dir(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    state_subdir(xdg_state_home, home, "permissions")
}

pub fn path(dir: &Path, project_root: &Path) -> PathBuf {
    dir.join(file_name(project_root))
}

pub fn load(dir: &Path, project_root: &Path) -> LoadedRules {
    let file = path(dir, project_root);
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LoadedRules::Missing,
        Err(e) => return LoadedRules::Unusable(format!("{}: {e}", file.display())),
    };
    let parsed: RulesFile = match serde_json::from_str(&text) {
        Ok(parsed) => parsed,
        Err(e) => return LoadedRules::Unusable(format!("{}: not a rules file ({e})", file.display())),
    };
    if parsed.version != VERSION {
        return LoadedRules::Unusable(format!(
            "{}: version {} (this build reads {VERSION})",
            file.display(),
            parsed.version
        ));
    }
    let root = project_root.to_string_lossy();
    if parsed.project_root != root {
        return LoadedRules::Unusable(format!(
            "{}: written for {}, not {root}",
            file.display(),
            parsed.project_root
        ));
    }
    let mut rules = Vec::new();
    for text in parsed.rules {
        match PrefixRule::parse(&text) {
            Some(rule) => rules.push(rule),
            None => eprintln!(
                "[permissions] {}: not a Bash(<words> *) rule, skipped: {text:?}",
                file.display()
            ),
        }
    }
    LoadedRules::Rules(PrefixRules::new(rules))
}

/// Adds `rule` (read-modify-write, atomic) and returns the project's rules after it. An unusable
/// file is set aside first, so the new rule is not lost behind it.
pub fn add(dir: &Path, project_root: &Path, rule: &PrefixRule) -> std::io::Result<PrefixRules> {
    crate::layout::persist::create_state_dir(dir)?;
    let current = match load(dir, project_root) {
        LoadedRules::Rules(rules) => rules,
        LoadedRules::Missing => PrefixRules::default(),
        LoadedRules::Unusable(why) => {
            eprintln!("[permissions] {why}; setting it aside");
            set_aside(dir, project_root)?;
            PrefixRules::default()
        }
    };
    let rules = current.with(rule.clone());
    let file = path(dir, project_root);
    let body = RulesFile {
        version: VERSION,
        project_root: project_root.to_string_lossy().into_owned(),
        rules: rules.rules().iter().map(PrefixRule::to_rule_string).collect(),
    };
    let tmp = temporary(&file);
    let text = serde_json::to_string_pretty(&body).expect("a rules file always serializes");
    if let Err(err) =
        agent::private_fs::write_private(&tmp, text.as_bytes()).and_then(|()| std::fs::rename(&tmp, &file))
    {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(rules)
}

pub fn set_aside(dir: &Path, project_root: &Path) -> std::io::Result<PathBuf> {
    let file = path(dir, project_root);
    let aside = file.with_extension("json.unusable");
    std::fs::rename(&file, &aside)?;
    Ok(aside)
}

pub fn startup(dir: Option<&Path>, project_root: &Path) -> (PrefixRules, Vec<String>) {
    let Some(dir) = dir else {
        return (PrefixRules::default(), Vec::new());
    };
    match load(dir, project_root) {
        LoadedRules::Rules(rules) => (rules, Vec::new()),
        LoadedRules::Missing => (PrefixRules::default(), Vec::new()),
        LoadedRules::Unusable(why) => {
            let mut notes = vec![format!("[permissions] unusable ({why}); no rules this launch")];
            match set_aside(dir, project_root) {
                Ok(aside) => notes.push(format!("[permissions] set aside as {}", aside.display())),
                Err(e) => notes.push(format!("[permissions] could not set it aside: {e}")),
            }
            (PrefixRules::default(), notes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_scratch_dir::ScratchDir;
    use agent::PrefixRule;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    fn scratch(label: &str) -> ScratchDir {
        ScratchDir::new("nv-rules", label)
    }
    fn root() -> PathBuf {
        PathBuf::from("/home/user/project")
    }
    fn rule(text: &str) -> PrefixRule {
        PrefixRule::parse(text).unwrap()
    }

    #[test]
    fn it_lives_under_permissions_named_like_the_layout_file() {
        assert_eq!(
            state_dir(Some(OsStr::new("/s")), None),
            Some(PathBuf::from("/s/neovibe/permissions"))
        );
        assert_eq!(
            path(Path::new("/d"), &root()),
            Path::new("/d").join(crate::layout::persist::file_name(&root()))
        );
    }

    #[test]
    fn an_added_rule_is_read_back_and_not_stored_twice() {
        let dir = scratch("add");
        assert_eq!(load(&dir, &root()), LoadedRules::Missing);
        add(&dir, &root(), &rule("Bash(git push *)")).unwrap();
        let rules = add(&dir, &root(), &rule("Bash(git push *)")).unwrap();
        assert_eq!(rules.rules().len(), 1);
        assert_eq!(load(&dir, &root()), LoadedRules::Rules(rules));
        let text = std::fs::read_to_string(path(&dir, &root())).unwrap();
        assert!(
            text.contains("\"Bash(git push *)\""),
            "the file holds Claude Code's own syntax: {text}"
        );
    }

    #[test]
    fn a_line_that_is_not_a_prefix_rule_is_skipped_not_fatal() {
        let dir = scratch("skip");
        let root = root();
        std::fs::write(
            path(&dir, &root),
            format!(
                "{{\"version\":1,\"project_root\":{:?},\"rules\":[\"Bash(npm ci *)\",\"Read(*)\",\"Bash(rm)\"]}}",
                root.to_string_lossy()
            ),
        )
        .unwrap();
        let LoadedRules::Rules(rules) = load(&dir, &root) else {
            panic!("usable")
        };
        assert_eq!(
            rules.rules().iter().map(|r| r.to_rule_string()).collect::<Vec<_>>(),
            vec!["Bash(npm ci *)"]
        );
    }

    #[test]
    fn a_broken_or_foreign_file_is_set_aside_and_read_as_no_rules() {
        let dir = scratch("unusable");
        std::fs::write(path(&dir, &root()), "{ not json").unwrap();
        let (rules, notes) = startup(Some(&dir), &root());
        assert!(rules.is_empty());
        assert!(notes.iter().any(|n| n.contains("set aside")), "{notes:?}");
        assert_eq!(load(&dir, &root()), LoadedRules::Missing);
        let foreign = "{\"version\":1,\"project_root\":\"/elsewhere\",\"rules\":[\"Bash(rm *)\"]}";
        std::fs::write(path(&dir, &root()), foreign).unwrap();
        assert!(matches!(load(&dir, &root()), LoadedRules::Unusable(why) if why.contains("written for")));
        assert!(startup(None, &root()).0.is_empty());
    }
}
