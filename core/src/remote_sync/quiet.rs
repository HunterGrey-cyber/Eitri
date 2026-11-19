//! When a project is quiet enough to re-create its sync sessions: no turn may be running in it, no editor attached to
//! it holds an unsaved buffer under the root, and sync has seen no change for a while.
//!
//! Re-creating a session drops Mutagen's ancestor, so a save that lands between the listing and the new session
//! becomes a conflict. Waiting for quiet keeps that rare; the conflict is still shown when it happens.
//!
//! Editors report in two ways. The window's own nvim is asked directly ([`UNSAVED_UNDER_LUA`] through the window's
//! editor link). Every other editor (a second window, a plain nvim with `eitri.nvim`) reports through an
//! `eitri host sync watch --json` process, which writes the report into the project's sync directory as
//! `editors/<pid>.json`; [`EditorReports`] reads them. "Cannot tell" from any attached editor is not quiet.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Lists the unsaved buffers under a root and the loaded files under it with their modification times. Argument:
/// the root. Answer: `{unsaved = {<path>...}, files = {{<path>, <mtime ms>}...}}`.
pub const UNSAVED_UNDER_LUA: &str = include_str!("unsaved_under.lua");

/// What one editor holds under a project's root.
pub trait EditorState: Send + Sync {
    /// The unsaved buffers' paths under `root`; `None` when this editor cannot tell (not answering, not started).
    fn unsaved_under(&self, root: &Path) -> Option<Vec<PathBuf>>;
    /// The save generation now (the newest modification time, in milliseconds, of a file the editor has loaded under
    /// the root) and the files saved after `generation`. An editor that cannot tell answers `(generation, [])`
    /// and its `unsaved_under` says `None`.
    fn saves_since(&self, generation: u64) -> (u64, Vec<PathBuf>);
}

/// An editor's answer, as [`UNSAVED_UNDER_LUA`] returns it and as a report file holds it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorAnswer {
    #[serde(default)]
    pub unsaved: Vec<PathBuf>,
    /// `(path, modification time in ms)` for each loaded file under the root.
    #[serde(default)]
    pub files: Vec<(PathBuf, u64)>,
}

impl EditorAnswer {
    /// Reads the msgpack answer of [`UNSAVED_UNDER_LUA`]; anything else is `None` (cannot tell).
    pub fn from_value(value: &rmpv::Value) -> Option<Self> {
        let map = value.as_map()?;
        let field = |name: &str| map.iter().find(|(key, _)| key.as_str() == Some(name)).map(|(_, v)| v);
        let list = |value: Option<&rmpv::Value>| -> Option<Vec<rmpv::Value>> {
            match value {
                None => Some(vec![]),
                // An empty Lua table travels as a map.
                Some(rmpv::Value::Map(map)) if map.is_empty() => Some(vec![]),
                Some(rmpv::Value::Array(items)) => Some(items.clone()),
                Some(_) => None,
            }
        };
        let unsaved = list(field("unsaved"))?
            .iter()
            .map(|item| item.as_str().map(PathBuf::from))
            .collect::<Option<Vec<_>>>()?;
        let files = list(field("files"))?
            .iter()
            .map(|item| {
                let pair = item.as_array()?;
                let path = pair.first()?.as_str()?;
                let mtime = pair.get(1)?;
                let mtime = mtime.as_u64().or_else(|| mtime.as_f64().map(|f| f.max(0.0) as u64))?;
                Some((PathBuf::from(path), mtime))
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self { unsaved, files })
    }

    pub fn generation(&self) -> u64 {
        self.files.iter().map(|(_, mtime)| *mtime).max().unwrap_or(0)
    }

    pub fn saved_after(&self, generation: u64) -> Vec<PathBuf> {
        self.files
            .iter()
            .filter(|(_, mtime)| *mtime > generation)
            .map(|(path, _)| path.clone())
            .collect()
    }
}

/// A report file: an editor's answer, when it was written and by which process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorReport {
    pub pid: u32,
    pub written_unix_ms: u64,
    /// `None`: the editor cannot tell.
    pub answer: Option<EditorAnswer>,
}

/// A report older than this, from a process still alive, counts as "cannot tell".
pub const REPORT_STALE_AFTER: Duration = Duration::from_secs(15);

pub fn editors_dir(sync_dir: &Path) -> PathBuf {
    sync_dir.join("editors")
}

/// Writes this process's report (0600, through a rename).
pub fn write_report(state_home: &Path, sync_dir: &Path, report: &EditorReport) -> io::Result<()> {
    let dir = editors_dir(sync_dir);
    agent::private_fs::create_private_dir_all(&dir, &state_home.join("eitri"))?;
    let bytes = serde_json::to_vec(report).map_err(io::Error::other)?;
    let temporary = dir.join(format!(".{}-{}.tmp", report.pid, uuid::Uuid::new_v4()));
    let written = (|| {
        use std::io::Write;
        let mut file = agent::private_fs::open_private(&temporary)?;
        file.write_all(&bytes)?;
        drop(file);
        std::fs::rename(&temporary, dir.join(format!("{}.json", report.pid)))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// Removes this process's report, when it stops reporting.
pub fn remove_report(sync_dir: &Path, pid: u32) {
    let _ = std::fs::remove_file(editors_dir(sync_dir).join(format!("{pid}.json")));
}

fn alive(pid: u32) -> bool {
    pid > 0 && pid <= i32::MAX as u32 && {
        // SAFETY: signal 0 only checks that the process exists and may be signalled.
        let result = unsafe { libc::kill(pid as i32, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// The editors that report through `watch` processes, read from the sync directory. A report from a process that is
/// gone is removed; one from a live process that has not written for [`REPORT_STALE_AFTER`] is "cannot tell".
pub struct EditorReports {
    pub sync_dir: PathBuf,
    /// This process's own report, which the caller already counts through its own editor.
    pub skip_pid: Option<u32>,
    pub now_unix_ms: fn() -> u64,
}

impl EditorReports {
    pub fn new(sync_dir: PathBuf) -> Self {
        Self {
            sync_dir,
            skip_pid: Some(std::process::id()),
            now_unix_ms: super::status_file::now_unix_ms,
        }
    }

    fn reports(&self) -> Vec<Option<EditorAnswer>> {
        let Ok(entries) = std::fs::read_dir(editors_dir(&self.sync_dir)) else {
            return vec![];
        };
        let now = (self.now_unix_ms)();
        let mut answers = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(pid) = name
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .and_then(|pid| pid.parse::<u32>().ok())
            else {
                continue;
            };
            if Some(pid) == self.skip_pid {
                continue;
            }
            if !alive(pid) {
                let _ = std::fs::remove_file(entry.path());
                continue;
            }
            let report = agent::private_fs::read_private_nofollow_bytes_capped(&entry.path(), 4 * 1024 * 1024)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<EditorReport>(&bytes).ok());
            answers.push(report.and_then(|report| {
                let fresh = now.saturating_sub(report.written_unix_ms) <= REPORT_STALE_AFTER.as_millis() as u64;
                (fresh && report.pid == pid).then_some(report.answer).flatten()
            }));
        }
        answers
    }
}

impl EditorState for EditorReports {
    fn unsaved_under(&self, root: &Path) -> Option<Vec<PathBuf>> {
        let mut unsaved = Vec::new();
        for answer in self.reports() {
            unsaved.extend(answer?.unsaved.into_iter().filter(|path| path.starts_with(root)));
        }
        Some(unsaved)
    }

    fn saves_since(&self, generation: u64) -> (u64, Vec<PathBuf>) {
        let mut newest = generation;
        let mut saved = Vec::new();
        for answer in self.reports().into_iter().flatten() {
            newest = newest.max(answer.generation());
            saved.extend(answer.saved_after(generation));
        }
        (newest, saved)
    }
}

/// Several editors as one: "cannot tell" from any is "cannot tell"; unsaved buffers and saves are joined.
pub struct Editors(pub Vec<Box<dyn EditorState>>);

impl EditorState for Editors {
    fn unsaved_under(&self, root: &Path) -> Option<Vec<PathBuf>> {
        let mut all = BTreeSet::new();
        for editor in &self.0 {
            all.extend(editor.unsaved_under(root)?);
        }
        Some(all.into_iter().collect())
    }

    fn saves_since(&self, generation: u64) -> (u64, Vec<PathBuf>) {
        let mut newest = generation;
        let mut saved = BTreeSet::new();
        for editor in &self.0 {
            let (their, paths) = editor.saves_since(generation);
            newest = newest.max(their);
            saved.extend(paths);
        }
        (newest, saved.into_iter().collect())
    }
}

/// No editor attached: a `watch` run by hand, or a window whose editor is closed.
pub struct NoEditor;

impl EditorState for NoEditor {
    fn unsaved_under(&self, _root: &Path) -> Option<Vec<PathBuf>> {
        Some(vec![])
    }
    fn saves_since(&self, generation: u64) -> (u64, Vec<PathBuf>) {
        (generation, vec![])
    }
}

/// Why the project is not quiet; the text is the band's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotQuiet {
    /// A turn or a background task may be running (`unknown` counts as running).
    Turn,
    /// The server could not say what is running.
    ActivityUnknown(String),
    Unsaved(Vec<PathBuf>),
    /// An attached editor could not say what it holds.
    EditorCannotTell,
    /// Sync saw a change less than the quiet time ago.
    RecentChange,
}

impl NotQuiet {
    pub fn text(&self) -> String {
        match self {
            Self::Turn => "waiting for the agent's turn to end".into(),
            Self::ActivityUnknown(why) => format!("waiting to learn whether an agent is working ({why})"),
            Self::Unsaved(paths) => format!(
                "waiting for unsaved buffers to be saved: {}",
                paths
                    .iter()
                    .take(3)
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::EditorCannotTell => "waiting for the editor to answer".into(),
            Self::RecentChange => "waiting for files to stop changing".into(),
        }
    }
}

/// What a quiet check reads on the server: whether turns or background tasks run in the project.
pub trait ActivitySource {
    /// `Ok(true)` when something may be running (a turn, a task, or the helper's `unknown`).
    fn busy(&self) -> Result<bool, String>;
}

/// The quiet check: activity, then editors, then the time since the last change sync saw.
pub fn check(
    activity: &dyn ActivitySource,
    editor: &dyn EditorState,
    root: &Path,
    last_change: Instant,
    quiet_for: Duration,
) -> Result<(), NotQuiet> {
    match activity.busy() {
        Ok(true) => return Err(NotQuiet::Turn),
        Ok(false) => {}
        Err(why) => return Err(NotQuiet::ActivityUnknown(why)),
    }
    match editor.unsaved_under(root) {
        None => return Err(NotQuiet::EditorCannotTell),
        Some(unsaved) if !unsaved.is_empty() => return Err(NotQuiet::Unsaved(unsaved)),
        Some(_) => {}
    }
    if last_change.elapsed() < quiet_for {
        return Err(NotQuiet::RecentChange);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    pub(crate) struct FakeEditor {
        pub unsaved: Mutex<Option<Vec<PathBuf>>>,
        pub files: Mutex<Vec<(PathBuf, u64)>>,
    }

    impl FakeEditor {
        pub fn new() -> Self {
            Self {
                unsaved: Mutex::new(Some(vec![])),
                files: Mutex::new(vec![]),
            }
        }
    }

    impl EditorState for std::sync::Arc<FakeEditor> {
        fn unsaved_under(&self, root: &Path) -> Option<Vec<PathBuf>> {
            self.unsaved
                .lock()
                .unwrap()
                .clone()
                .map(|paths| paths.into_iter().filter(|p| p.starts_with(root)).collect())
        }
        fn saves_since(&self, generation: u64) -> (u64, Vec<PathBuf>) {
            let answer = EditorAnswer {
                unsaved: vec![],
                files: self.files.lock().unwrap().clone(),
            };
            (answer.generation().max(generation), answer.saved_after(generation))
        }
    }

    struct Busy(Result<bool, String>);
    impl ActivitySource for Busy {
        fn busy(&self) -> Result<bool, String> {
            self.0.clone()
        }
    }

    #[test]
    fn quiet_needs_no_turn_no_unsaved_buffer_and_no_recent_change() {
        let root = Path::new("/srv/project");
        let editor = std::sync::Arc::new(FakeEditor::new());
        let long_ago = Instant::now() - Duration::from_secs(10);
        let quiet = Duration::from_secs(2);
        assert_eq!(check(&Busy(Ok(false)), &editor, root, long_ago, quiet), Ok(()));
        assert_eq!(
            check(&Busy(Ok(true)), &editor, root, long_ago, quiet),
            Err(NotQuiet::Turn)
        );
        assert!(matches!(
            check(&Busy(Err("no answer".into())), &editor, root, long_ago, quiet),
            Err(NotQuiet::ActivityUnknown(_))
        ));
        *editor.unsaved.lock().unwrap() = Some(vec![root.join("a.rs"), "/elsewhere/b.rs".into()]);
        assert_eq!(
            check(&Busy(Ok(false)), &editor, root, long_ago, quiet),
            Err(NotQuiet::Unsaved(vec![root.join("a.rs")]))
        );
        *editor.unsaved.lock().unwrap() = None;
        assert_eq!(
            check(&Busy(Ok(false)), &editor, root, long_ago, quiet),
            Err(NotQuiet::EditorCannotTell)
        );
        *editor.unsaved.lock().unwrap() = Some(vec![]);
        assert_eq!(
            check(&Busy(Ok(false)), &editor, root, Instant::now(), quiet),
            Err(NotQuiet::RecentChange)
        );
    }

    #[test]
    fn editor_reports_from_watch_processes_count_and_go_stale() {
        let state = agent::state_dirs::test_workspace_dir("sync-editor-reports");
        let dir = state.join("eitri/remote/sync/0123456789abcdef");
        let root = Path::new("/srv/project");
        let mut reports = EditorReports::new(dir.clone());
        reports.skip_pid = None;
        // No report at all: nothing attached, quiet as far as editors go.
        assert_eq!(reports.unsaved_under(root), Some(vec![]));
        let me = std::process::id();
        let now = super::super::status_file::now_unix_ms();
        let answer = EditorAnswer {
            unsaved: vec![root.join("src/lib.rs")],
            files: vec![(root.join("src/lib.rs"), 1_000), (root.join("README.md"), 2_000)],
        };
        write_report(
            &state,
            &dir,
            &EditorReport {
                pid: me,
                written_unix_ms: now,
                answer: Some(answer.clone()),
            },
        )
        .unwrap();
        assert_eq!(reports.unsaved_under(root), Some(vec![root.join("src/lib.rs")]));
        assert_eq!(reports.saves_since(1_500), (2_000, vec![root.join("README.md")]));
        // Cannot tell, or not written for too long: not quiet.
        write_report(
            &state,
            &dir,
            &EditorReport {
                pid: me,
                written_unix_ms: now,
                answer: None,
            },
        )
        .unwrap();
        assert_eq!(reports.unsaved_under(root), None);
        write_report(
            &state,
            &dir,
            &EditorReport {
                pid: me,
                written_unix_ms: now - 60_000,
                answer: Some(EditorAnswer::default()),
            },
        )
        .unwrap();
        assert_eq!(reports.unsaved_under(root), None);
        // A report from a process that is gone is removed and does not count.
        remove_report(&dir, me);
        let gone = u32::MAX / 2;
        write_report(
            &state,
            &dir,
            &EditorReport {
                pid: gone,
                written_unix_ms: now,
                answer: None,
            },
        )
        .unwrap();
        assert_eq!(reports.unsaved_under(root), Some(vec![]));
        assert!(!editors_dir(&dir).join(format!("{gone}.json")).exists());
        // The process's own report is skipped (it counts its own editor directly).
        reports.skip_pid = Some(me);
        write_report(
            &state,
            &dir,
            &EditorReport {
                pid: me,
                written_unix_ms: now,
                answer: None,
            },
        )
        .unwrap();
        assert_eq!(reports.unsaved_under(root), Some(vec![]));
    }

    #[test]
    fn editors_join_and_cannot_tell_wins() {
        let a = std::sync::Arc::new(FakeEditor::new());
        let b = std::sync::Arc::new(FakeEditor::new());
        *a.unsaved.lock().unwrap() = Some(vec!["/r/x".into()]);
        *b.files.lock().unwrap() = vec![("/r/y".into(), 50)];
        let both = Editors(vec![Box::new(a.clone()), Box::new(b.clone())]);
        assert_eq!(both.unsaved_under(Path::new("/r")), Some(vec![PathBuf::from("/r/x")]));
        assert_eq!(both.saves_since(10), (50, vec![PathBuf::from("/r/y")]));
        *b.unsaved.lock().unwrap() = None;
        assert_eq!(both.unsaved_under(Path::new("/r")), None);
    }

    #[test]
    fn answers_parse_from_the_chunk_and_refuse_other_shapes() {
        use rmpv::Value;
        let answer = Value::Map(vec![
            (Value::from("unsaved"), Value::Array(vec![Value::from("/r/a")])),
            (
                Value::from("files"),
                Value::Array(vec![Value::Array(vec![
                    Value::from("/r/a"),
                    Value::from(1_700_000_000_123u64),
                ])]),
            ),
        ]);
        let parsed = EditorAnswer::from_value(&answer).unwrap();
        assert_eq!(parsed.unsaved, vec![PathBuf::from("/r/a")]);
        assert_eq!(parsed.generation(), 1_700_000_000_123);
        // Empty Lua tables arrive as maps.
        let empty = Value::Map(vec![
            (Value::from("unsaved"), Value::Map(vec![])),
            (Value::from("files"), Value::Map(vec![])),
        ]);
        assert_eq!(EditorAnswer::from_value(&empty), Some(EditorAnswer::default()));
        assert_eq!(EditorAnswer::from_value(&Value::from("x")), None);
        let bad = Value::Map(vec![(Value::from("unsaved"), Value::Array(vec![Value::from(3)]))]);
        assert_eq!(EditorAnswer::from_value(&bad), None);
    }
}
