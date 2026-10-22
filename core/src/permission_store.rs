//! The rules saved from a card's third button, per project: `$XDG_STATE_HOME/eitri/permissions/<16 hex>.json`.
//! Claude Code writes `.claude/settings.local.json` into the repository; Eitri writes nothing into
//! the project. Removing a rule is editing this file; `prefix i` shows its path. An unusable file is
//! set aside as `.json.unusable` and read as no rules, which fails toward more cards.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use agent::{PrefixRule, PrefixRules};
use serde::{Deserialize, Serialize};

use crate::state_paths::{file_name, state_subdir, temporary};

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

/// One line of a rules file as read: a rule, or a text the loader refused, kept verbatim so that
/// rewriting the file never deletes what the user wrote.
enum Line {
    Rule(PrefixRule),
    Refused(String),
}

/// What [`read`] found: the lines of a usable file, or why it is not one.
enum Read {
    Lines(Vec<Line>),
    Missing,
    Unusable(String),
}

pub fn load(dir: &Path, project_root: &Path) -> LoadedRules {
    match read(dir, project_root) {
        Read::Lines(lines) => LoadedRules::Rules(PrefixRules::new(
            lines
                .into_iter()
                .filter_map(|line| match line {
                    Line::Rule(rule) => Some(rule),
                    Line::Refused(_) => None,
                })
                .collect(),
        )),
        Read::Missing => LoadedRules::Missing,
        Read::Unusable(why) => LoadedRules::Unusable(why),
    }
}

fn read(dir: &Path, project_root: &Path) -> Read {
    let file = path(dir, project_root);
    let text = match agent::private_fs::read_private_to_string(&file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Read::Missing,
        Err(e) => return Read::Unusable(format!("{}: {e}", file.display())),
    };
    let parsed: RulesFile = match serde_json::from_str(&text) {
        Ok(parsed) => parsed,
        Err(e) => return Read::Unusable(format!("{}: not a rules file ({e})", file.display())),
    };
    if parsed.version != VERSION {
        return Read::Unusable(format!(
            "{}: version {} (this build reads {VERSION})",
            file.display(),
            parsed.version
        ));
    }
    let root = project_root.to_string_lossy();
    if parsed.project_root != root {
        return Read::Unusable(format!(
            "{}: written for {}, not {root}",
            file.display(),
            parsed.project_root
        ));
    }
    let lines = parsed
        .rules
        .into_iter()
        .map(|text| match PrefixRule::parse_or_refusal(&text) {
            Ok(rule) => Line::Rule(rule),
            Err(why) => {
                eprintln!(
                    "[permissions] {}: {}; not applied, and kept in the file as written: {text:?}",
                    file.display(),
                    why.describe()
                );
                Line::Refused(text)
            }
        })
        .collect();
    Read::Lines(lines)
}

/// Adds `rule` (read-modify-write, atomic) and returns the project's rules after it. An unusable
/// file is set aside first, so the new rule is not lost behind it. A line the loader refused is
/// written back exactly as it was, in its place: it applies to nothing, but it is the user's text,
/// and a later build, or the user fixing it by hand, may still want it.
pub fn add(dir: &Path, project_root: &Path, rule: &PrefixRule) -> std::io::Result<PrefixRules> {
    crate::state_paths::create_state_dir(dir)?;
    let lines = match read(dir, project_root) {
        Read::Lines(lines) => lines,
        Read::Missing => Vec::new(),
        Read::Unusable(why) => {
            eprintln!("[permissions] {why}; setting it aside");
            set_aside(dir, project_root)?;
            Vec::new()
        }
    };
    let mut rules = PrefixRules::default();
    let mut texts = Vec::new();
    for line in lines {
        match line {
            Line::Rule(kept) => {
                if !rules.rules().contains(&kept) {
                    texts.push(kept.to_rule_string());
                    rules = rules.with(kept);
                }
            }
            Line::Refused(text) => texts.push(text),
        }
    }
    if !rules.rules().contains(rule) {
        texts.push(rule.to_rule_string());
        rules = rules.with(rule.clone());
    }
    let file = path(dir, project_root);
    let body = RulesFile {
        version: VERSION,
        project_root: project_root.to_string_lossy().into_owned(),
        rules: texts,
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
            Some(PathBuf::from("/s/eitri/permissions"))
        );
        assert_eq!(
            path(Path::new("/d"), &root()),
            Path::new("/d").join(crate::state_paths::file_name(&root()))
        );
    }

    /// The rules file decides which calls are answered without a card, so only a regular file this
    /// user owns is read: another user's file (here, a link to one) is unusable and loads no rules,
    /// and a FIFO in its place neither blocks the launch nor yields any.
    #[test]
    fn a_rules_file_that_is_not_this_users_own_loads_no_rules() {
        let dir = scratch("foreign");
        std::fs::create_dir_all(&*dir).unwrap();
        let file = path(&dir, &root());
        if agent::private_fs::current_uid() != 0 {
            std::os::unix::fs::symlink("/etc/passwd", &file).unwrap();
            match load(&dir, &root()) {
                LoadedRules::Unusable(why) => assert!(why.contains("not a regular file of this user"), "{why}"),
                other => panic!("{other:?}"),
            }
            std::fs::remove_file(&file).unwrap();
        }
        let c_path = std::ffi::CString::new(file.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let (d, r) = (dir.to_path_buf(), root());
        std::thread::spawn(move || {
            let _ = tx.send(matches!(load(&d, &r), LoadedRules::Unusable(_)));
        });
        let unusable = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("loading the rules blocked on a FIFO");
        assert!(unusable);
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
    fn a_rule_the_card_never_offers_is_skipped_on_load() {
        let dir = scratch("wrapper");
        let root = root();
        std::fs::write(
            path(&dir, &root),
            format!(
                "{{\"version\":1,\"project_root\":{:?},\"rules\":[\"Bash(env *)\",\"Bash(timeout *)\",\"Bash(git log *)\"]}}",
                root.to_string_lossy()
            ),
        )
        .unwrap();
        let LoadedRules::Rules(rules) = load(&dir, &root) else {
            panic!("usable")
        };
        assert_eq!(
            rules.rules().iter().map(|r| r.to_rule_string()).collect::<Vec<_>>(),
            vec!["Bash(git log *)"]
        );
    }

    /// A line the loader refused is not applied, but saving another rule keeps it, verbatim and in
    /// its place: the file is the user's, and a rule they wrote must never vanish because a build
    /// would not read it. Before, the next save rewrote the file from the parsed rules alone.
    #[test]
    fn saving_a_rule_keeps_the_lines_the_loader_refused() {
        let dir = scratch("keep-refused");
        let root = root();
        std::fs::write(
            path(&dir, &root),
            format!(
                "{{\"version\":1,\"project_root\":{:?},\"rules\":[\"Bash(git st\\u00e4tus *)\",\"Bash(npm ci *)\",\"Bash(env *)\",\"Read(*)\",\"Bash(npm ci *)\"]}}",
                root.to_string_lossy()
            ),
        )
        .unwrap();
        let rules = add(&dir, &root, &rule("Bash(cargo test *)")).unwrap();
        assert_eq!(
            rules.rules().iter().map(|r| r.to_rule_string()).collect::<Vec<_>>(),
            vec!["Bash(npm ci *)", "Bash(cargo test *)"]
        );
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path(&dir, &root)).unwrap()).unwrap();
        assert_eq!(
            written["rules"],
            serde_json::json!([
                "Bash(git st\u{e4}tus *)",
                "Bash(npm ci *)",
                "Bash(env *)",
                "Read(*)",
                "Bash(cargo test *)"
            ])
        );
        // Saving one already there adds nothing and still keeps every refused line.
        add(&dir, &root, &rule("Bash(npm ci *)")).unwrap();
        let again: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path(&dir, &root)).unwrap()).unwrap();
        assert_eq!(again["rules"], written["rules"]);
        assert_eq!(load(&dir, &root), LoadedRules::Rules(rules));
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
