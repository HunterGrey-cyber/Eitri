//! C5 (keymap/tabs spec §4.2): `↑`/`↓` and `Ctrl+r` over this project's prompts, "per working
//! directory" as Claude Code keeps them (docs, #command-history). `$XDG_STATE_HOME/eitri/history/
//! <16 hex>.jsonl`, named like the layout file, written atomically, never inside the project. A
//! file that cannot be read is set aside as `.jsonl.unusable` and read as empty (§4.4).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::layout::persist::{file_name, state_subdir, temporary};

pub const HISTORY_LIMIT: usize = 500;

#[derive(Serialize, Deserialize)]
struct Line {
    text: String,
    at: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Loaded {
    Entries(Vec<String>),
    Missing,
    Unusable(String),
}

pub fn state_dir(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    state_subdir(xdg_state_home, home, "history")
}

pub fn path(dir: &Path, project_root: &Path) -> PathBuf {
    dir.join(file_name(project_root).replace(".json", ".jsonl"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn read_lines(file: &Path) -> Result<Option<Vec<Line>>, String> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", file.display())),
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| serde_json::from_str::<Line>(l).map_err(|e| format!("{} line {}: {e}", file.display(), i + 1)))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

pub fn load(dir: &Path, project_root: &Path) -> Loaded {
    match read_lines(&path(dir, project_root)) {
        Ok(None) => Loaded::Missing,
        Ok(Some(lines)) => Loaded::Entries(lines.into_iter().map(|l| l.text).collect()),
        Err(why) => Loaded::Unusable(why),
    }
}

/// Re-reads the file, appends `texts` (blank ones dropped, a repeat of the current last one
/// dropped), keeps the newest [`HISTORY_LIMIT`], and rewrites it atomically. Returns every entry,
/// oldest first. An unreadable file is set aside first and the append starts from empty.
pub fn append(dir: &Path, project_root: &Path, texts: &[String]) -> std::io::Result<Vec<String>> {
    crate::layout::persist::create_state_dir(dir)?;
    let file = path(dir, project_root);
    let mut lines = match read_lines(&file) {
        Ok(lines) => lines.unwrap_or_default(),
        Err(why) => {
            eprintln!("[history] {why}; setting it aside");
            set_aside(dir, project_root)?;
            Vec::new()
        }
    };
    for text in texts {
        if text.trim().is_empty() || lines.last().is_some_and(|l| &l.text == text) {
            continue;
        }
        lines.push(Line {
            text: text.clone(),
            at: now_ms(),
        });
    }
    let drop = lines.len().saturating_sub(HISTORY_LIMIT);
    lines.drain(..drop);
    let body: String = lines
        .iter()
        .map(|l| serde_json::to_string(l).expect("a history line always serializes") + "\n")
        .collect();
    let tmp = temporary(&file);
    if let Err(err) =
        agent::private_fs::write_private(&tmp, body.as_bytes()).and_then(|()| std::fs::rename(&tmp, &file))
    {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(lines.into_iter().map(|l| l.text).collect())
}

pub fn set_aside(dir: &Path, project_root: &Path) -> std::io::Result<PathBuf> {
    let file = path(dir, project_root);
    let aside = file.with_extension("jsonl.unusable");
    std::fs::rename(&file, &aside)?;
    Ok(aside)
}

/// The entries a window starts with, and what to log. Never fails: no directory, a missing file
/// and an unusable one all read as an empty history.
pub fn startup(dir: Option<&Path>, project_root: &Path) -> (Vec<String>, Vec<String>) {
    let Some(dir) = dir else {
        return (Vec::new(), Vec::new());
    };
    match load(dir, project_root) {
        Loaded::Entries(entries) => (entries, Vec::new()),
        Loaded::Missing => (Vec::new(), Vec::new()),
        Loaded::Unusable(why) => {
            let mut notes = vec![format!("[history] unusable ({why}); starting with none")];
            match set_aside(dir, project_root) {
                Ok(aside) => notes.push(format!("[history] set aside as {}", aside.display())),
                Err(e) => notes.push(format!("[history] could not set it aside: {e}")),
            }
            (Vec::new(), notes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_scratch_dir::ScratchDir;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    fn scratch(label: &str) -> ScratchDir {
        ScratchDir::new("nv-history", label)
    }
    fn root() -> PathBuf {
        PathBuf::from("/home/user/project")
    }
    fn texts(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn it_lives_beside_the_layout_and_agent_directories_as_jsonl() {
        assert_eq!(
            state_dir(Some(OsStr::new("/s")), None),
            Some(PathBuf::from("/s/eitri/history"))
        );
        let name = path(Path::new("/s/eitri/history"), &root());
        let layout_name = crate::layout::persist::file_name(&root());
        assert_eq!(
            name.file_name().unwrap().to_string_lossy(),
            layout_name.replace(".json", ".jsonl"),
            "keyed by the layout file's hash"
        );
    }

    #[test]
    fn appended_prompts_come_back_oldest_first_and_a_repeat_of_the_last_is_dropped() {
        let dir = scratch("append");
        assert_eq!(load(&dir, &root()), Loaded::Missing);
        append(&dir, &root(), &texts(&["fix the parser"])).unwrap();
        append(&dir, &root(), &texts(&["run the tests", "run the tests"])).unwrap();
        let all = append(&dir, &root(), &texts(&["多行\n中文"])).unwrap();
        assert_eq!(all, texts(&["fix the parser", "run the tests", "多行\n中文"]));
        assert_eq!(load(&dir, &root()), Loaded::Entries(all));
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "no .tmp left behind: {names:?}");
    }

    #[test]
    fn it_keeps_the_newest_five_hundred() {
        let dir = scratch("ring");
        let many: Vec<String> = (0..HISTORY_LIMIT + 7).map(|i| format!("prompt {i}")).collect();
        let kept = append(&dir, &root(), &many).unwrap();
        assert_eq!(kept.len(), HISTORY_LIMIT);
        assert_eq!(kept.first().unwrap(), "prompt 7");
        assert_eq!(kept.last().unwrap(), &format!("prompt {}", HISTORY_LIMIT + 6));
    }

    /// Two windows of one project: each append re-reads the file first, so neither erases the other.
    #[test]
    fn an_append_keeps_what_another_window_wrote_meanwhile() {
        let dir = scratch("two-windows");
        append(&dir, &root(), &texts(&["from window A"])).unwrap();
        let all = append(&dir, &root(), &texts(&["from window B"])).unwrap();
        assert_eq!(all, texts(&["from window A", "from window B"]));
    }

    #[test]
    fn empty_or_whitespace_prompts_are_not_history() {
        let dir = scratch("blank");
        let all = append(&dir, &root(), &texts(&["", "   \n", "real"])).unwrap();
        assert_eq!(all, texts(&["real"]));
    }

    #[test]
    fn an_unreadable_file_is_set_aside_and_read_as_empty() {
        let dir = scratch("unusable");
        std::fs::write(path(&dir, &root()), "{\"text\":\"ok\",\"at\":1}\nnot json\n").unwrap();
        assert!(matches!(load(&dir, &root()), Loaded::Unusable(_)));
        let (entries, notes) = startup(Some(&dir), &root());
        assert!(entries.is_empty());
        assert!(notes.iter().any(|n| n.contains("set aside")), "{notes:?}");
        assert!(dir
            .join(format!(
                "{}.unusable",
                path(&dir, &root()).file_name().unwrap().to_string_lossy()
            ))
            .exists());
        assert_eq!(load(&dir, &root()), Loaded::Missing);
        assert_eq!(
            startup(None, &root()).0,
            Vec::<String>::new(),
            "no state directory: no history, no failure"
        );
    }
}
