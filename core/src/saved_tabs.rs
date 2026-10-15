//! The session tabs a project had open, kept between launches so the next launch can offer them
//! back: `$XDG_STATE_HOME/eitri/tabs/<16 hex>.json`, named, written and set aside the way the layout
//! file is (`layout::persist`): 0600 in a 0700 directory, a `.tmp` beside it and then a rename, a file
//! that cannot be used moved whole to `<name>.json.unusable` and read as nothing. Never inside the
//! project.
//!
//! **What is listed:** only tabs that have a session (an empty tab has nothing to bring back), in
//! tab-bar order, each with the rename it carries and the permission mode it was in, and the index of
//! the tab that was active. Claude tabs are in `tabs`; a tab running another agent is in
//! `external_tabs`, a separate array with the agent named, so that an older build -- which reads
//! `tabs` only and ignores what it does not know -- never resumes another agent's session as a Claude
//! one. Such a build drops the array on its next write, which loses only the offer to bring those
//! tabs back. The format's version stays 1 for that reason. Two windows on one project each write their own set and the
//! last writer wins; the session leases, not this file, are what stop two windows driving one
//! session.
//!
//! **When it is written** is [`TabMemory`]'s job, run on its own thread by [`TabWriter`]: whenever the set it is shown differs from the set
//! it last saw, and never because the window is closing -- closing closes every tab, and recording
//! "no tabs" then would erase exactly what the next launch wants.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent_backend::AgentId;
use crate::agent_bridge::SessionModeChoice;
use crate::layout::persist::{create_state_dir, file_name, state_subdir, temporary};

/// The file format's version. A file of any other version is not read.
pub const VERSION: u32 = 1;

/// One tab worth bringing back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTab {
    pub conversation_id: String,
    /// Claude's own id for the session: the key its record, its lease and its resume all use.
    pub provider_session_id: String,
    /// The tab's rename, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The mode the tab was in. `bypass` is kept here because the person who chose it should see it
    /// again -- and be asked about it, never silently given it back, when the tabs are restored.
    pub mode: SessionModeChoice,
}

/// One tab of an agent other than Claude worth bringing back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedExternalTab {
    /// Which agent, as its records name it (`codex`, `acp:<name>`).
    pub agent: AgentId,
    pub conversation_id: String,
    /// The agent's own id for the session (a Codex thread id): what its record and lease are keyed on.
    pub provider_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub mode: SessionModeChoice,
}

/// The tabs of one window, in tab-bar order: the Claude tabs, then the other agents' tabs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedTabs {
    pub tabs: Vec<SavedTab>,
    /// Index into `tabs` of the tab that was on screen, or of the first Claude tab when the tab on
    /// screen was another agent's (what an older build, which reads only `tabs`, falls back to).
    pub active: usize,
    pub external_tabs: Vec<SavedExternalTab>,
    /// Index into `external_tabs` when the tab on screen was another agent's.
    pub active_external: Option<usize>,
}

impl SavedTabs {
    /// No tab of either kind.
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty() && self.external_tabs.is_empty()
    }
}

#[derive(Serialize, Deserialize)]
struct TabsFile {
    version: u32,
    project_root: String,
    tabs: Vec<SavedTab>,
    active: usize,
    /// Absent from a file with no such tab, so the file of a window that ran only Claude is what it
    /// always was.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    external_tabs: Vec<ExternalEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_external: Option<usize>,
}

/// One entry of `external_tabs` as read: an agent this build does not know (a later build's) is kept
/// out of the restore without making the whole file unusable, which would lose the Claude tabs too.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum ExternalEntry {
    Known(SavedExternalTab),
    Unknown(serde_json::Value),
}

/// `<state home>/eitri/tabs`, by the same rule as the layout directory.
pub fn state_dir(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    state_subdir(xdg_state_home, home, "tabs")
}

/// The file's text for `saved`: pretty JSON, so a person can read and fix it.
pub fn encode(saved: &SavedTabs, project_root: &Path) -> String {
    let file = TabsFile {
        version: VERSION,
        project_root: project_root.to_string_lossy().into_owned(),
        tabs: saved.tabs.clone(),
        active: saved.active,
        external_tabs: saved.external_tabs.iter().cloned().map(ExternalEntry::Known).collect(),
        active_external: saved.active_external,
    };
    serde_json::to_string_pretty(&file).expect("saved tabs always serialize")
}

/// `text` read back as this project's saved tabs. A tab with no session id is dropped and a session
/// listed twice is kept once (its first place), because either would only ever be a hand edit gone
/// wrong; an `active` past the end becomes the first tab. `Err` says why the file cannot be used.
pub fn decode(text: &str, project_root: &Path) -> Result<SavedTabs, String> {
    let file: TabsFile = serde_json::from_str(text).map_err(|e| format!("not a saved-tabs file ({e})"))?;
    if file.version != VERSION {
        return Err(format!("version {} (this build reads {VERSION})", file.version));
    }
    let root = project_root.to_string_lossy();
    if file.project_root != root {
        return Err(format!("written for {}, not {root}", file.project_root));
    }
    let mut kept: Vec<SavedTab> = Vec::new();
    let mut active_id = file.tabs.get(file.active).map(|t| t.provider_session_id.clone());
    for tab in file.tabs {
        let usable = !tab.provider_session_id.trim().is_empty() && !tab.conversation_id.trim().is_empty();
        if usable && !kept.iter().any(|k| k.provider_session_id == tab.provider_session_id) {
            kept.push(tab);
        }
    }
    // Where the tab that was active ended up, so dropping an earlier entry cannot move it.
    let active = active_id
        .take()
        .and_then(|id| kept.iter().position(|t| t.provider_session_id == id))
        .unwrap_or(0);
    // The same rules for the other agents' tabs, a session being one agent's id of it.
    let read: Vec<Option<SavedExternalTab>> = file
        .external_tabs
        .into_iter()
        .map(|entry| match entry {
            ExternalEntry::Known(tab) => Some(tab),
            ExternalEntry::Unknown(_) => None,
        })
        .collect();
    let active_external_id = file
        .active_external
        .and_then(|i| read.get(i))
        .and_then(Option::as_ref)
        .map(|t| (t.agent.clone(), t.provider_session_id.clone()));
    let mut external: Vec<SavedExternalTab> = Vec::new();
    for tab in read.into_iter().flatten() {
        let usable = !tab.provider_session_id.trim().is_empty() && !tab.conversation_id.trim().is_empty();
        let listed = external
            .iter()
            .any(|k| k.agent == tab.agent && k.provider_session_id == tab.provider_session_id);
        if usable && !listed && tab.agent.is_external() {
            external.push(tab);
        }
    }
    let active_external = active_external_id.and_then(|(agent, id)| {
        external
            .iter()
            .position(|t| t.agent == agent && t.provider_session_id == id)
    });
    Ok(SavedTabs {
        tabs: kept,
        active,
        external_tabs: external,
        active_external,
    })
}

/// What [`load`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum Loaded {
    Saved(SavedTabs),
    /// No file: a project whose tabs have not been kept yet. The normal first launch, and not logged.
    Missing,
    /// A file that cannot be used, and why.
    Unusable(String),
}

/// Reads this project's file from `dir`.
pub fn load(dir: &Path, project_root: &Path) -> Loaded {
    let path = dir.join(file_name(project_root));
    match agent::private_fs::read_private_to_string(&path) {
        Ok(text) => match decode(&text, project_root) {
            Ok(saved) => Loaded::Saved(saved),
            Err(why) => Loaded::Unusable(format!("{}: {why}", path.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Loaded::Missing,
        Err(e) => Loaded::Unusable(format!("{}: {e}", path.display())),
    }
}

/// Writes this project's file into `dir`, creating it: to a `.tmp` of this process's own beside it and
/// then renamed over it, so a process that dies mid-write leaves the previous file, never half of one.
pub fn save(dir: &Path, project_root: &Path, saved: &SavedTabs) -> std::io::Result<PathBuf> {
    create_state_dir(dir)?;
    let path = dir.join(file_name(project_root));
    let tmp = temporary(&path);
    let written = agent::private_fs::write_private(&tmp, encode(saved, project_root).as_bytes())
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(err) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(path)
}

/// Moves this project's file, whole, to `<name>.json.unusable` beside it and returns where it went.
pub fn set_aside(dir: &Path, project_root: &Path) -> std::io::Result<PathBuf> {
    let path = dir.join(file_name(project_root));
    let aside = path.with_extension("json.unusable");
    std::fs::rename(&path, &aside)?;
    Ok(aside)
}

/// What the window learns about the saved tabs at launch, and what to log: `None` when there is
/// nothing to offer (no state directory, no file, a file that lists no tab, or one that cannot be
/// used -- which is set aside here, once, so the window's own later write does not replace it).
pub fn startup(dir: Option<&Path>, project_root: &Path) -> (Option<SavedTabs>, Vec<String>) {
    let Some(dir) = dir else { return (None, Vec::new()) };
    match load(dir, project_root) {
        Loaded::Saved(saved) if saved.is_empty() => (None, Vec::new()),
        Loaded::Saved(saved) => (Some(saved), Vec::new()),
        Loaded::Missing => (None, Vec::new()),
        Loaded::Unusable(why) => {
            let mut notes = vec![format!(
                "[tabs] the saved tabs are unusable ({why}); starting without them"
            )];
            match set_aside(dir, project_root) {
                Ok(aside) => notes.push(format!("[tabs] set aside as {}", aside.display())),
                Err(e) => notes.push(format!("[tabs] could not set the unusable file aside: {e}")),
            }
            (None, notes)
        }
    }
}

/// Keeps the saved file in step with the window's tabs.
///
/// It is shown the window's tabs as often as the window likes ([`TabMemory::observe`]) and writes only
/// when they differ from the last it saw. It starts from "no tabs", so a launch that has done nothing
/// yet writes nothing and leaves the previous window's file alone for the launch to offer.
pub struct TabMemory {
    /// `None`: no state directory, or the feature is off -- nothing is ever written.
    dir: Option<PathBuf>,
    project_root: PathBuf,
    last: SavedTabs,
}

impl TabMemory {
    pub fn new(dir: Option<PathBuf>, project_root: PathBuf) -> Self {
        TabMemory {
            dir,
            project_root,
            last: SavedTabs::default(),
        }
    }

    /// Stops all writing, for good: a window that has been told not to remember its tabs.
    pub fn disable(&mut self) {
        self.dir = None;
    }

    pub fn is_enabled(&self) -> bool {
        self.dir.is_some()
    }

    /// `now` is the window's tabs, or `None` while the window is closing and has none to show. Returns
    /// what a write did (`Some`) or that none was needed (`None`).
    ///
    /// A write that fails is reported once and not tried again until the tabs change: this is
    /// called on every tick, and a full disk must not turn into thirty failures a second.
    ///
    /// A file found unusable just before the write -- hand-edited, or from another build, since the
    /// window opened -- is set aside first rather than overwritten, as the layout file's is.
    pub fn observe(&mut self, now: Option<SavedTabs>) -> Option<Result<Written, std::io::Error>> {
        let now = now?;
        if now == self.last {
            return None;
        }
        self.write(now)
    }

    /// Writes `now` whatever was written last; `None` when nothing is ever written. For a caller that has
    /// already decided `now` is a change: [`TabWriter`]'s thread, whose sets are compared on the window's
    /// thread, and which may never have been handed the set this one replaced.
    fn write(&mut self, now: SavedTabs) -> Option<Result<Written, std::io::Error>> {
        let dir = self.dir.as_ref()?;
        self.last = now.clone();
        // A file that cannot be moved aside is left where it is: writing over it would destroy the one
        // copy of what a person may want to read, so the write is given up and said.
        let aside = match load(dir, &self.project_root) {
            Loaded::Unusable(_) => match set_aside(dir, &self.project_root) {
                Ok(aside) => Some(aside),
                Err(e) => return Some(Err(e)),
            },
            _ => None,
        };
        Some(save(dir, &self.project_root, &now).map(|path| Written { path, set_aside: aside }))
    }
}

/// A completed write.
#[derive(Debug, PartialEq, Eq)]
pub struct Written {
    pub path: PathBuf,
    /// Where an unusable file that stood in the way went, if one did.
    pub set_aside: Option<PathBuf>,
}

/// [`TabMemory`] on a thread of its own, so the window's thread never touches the disk for it.
///
/// [`observe`](Self::observe) only compares the tabs with the last set handed over and, when they differ,
/// leaves the new set for the thread, replacing one it has not taken yet: the last set wins, and since one
/// thread writes them in the order it takes them, the file always ends at the newest. What a write did is
/// logged by the thread. [`finish`](Self::finish) at the window's close hands over the last set and
/// returns a receiver that reports once the thread has written it and ended, which the close watches
/// under a backstop: a write is a temporary file renamed over the old one, so a process that leaves
/// before it ends keeps the previous file whole.
pub struct TabWriter {
    /// `None`: nothing is ever written (no state directory, or remembering is off).
    dir: Option<PathBuf>,
    project_root: PathBuf,
    /// The last set handed over, to hand over only a change.
    last: SavedTabs,
    /// Started at the first write.
    worker: Option<WriterHandle>,
    #[cfg(any(test, feature = "test-support"))]
    before_write: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

struct WriterHandle {
    shared: std::sync::Arc<WriterShared>,
    done: Option<std::sync::mpsc::Receiver<()>>,
}

#[derive(Default)]
struct WriterShared {
    queue: std::sync::Mutex<WriterQueue>,
    ready: std::sync::Condvar,
}

#[derive(Default)]
struct WriterQueue {
    next: Option<SavedTabs>,
    /// Taken by the thread and being written now.
    writing: bool,
    /// No more sets come: the thread ends once `next` is written.
    finished: bool,
    /// Remembering was turned off: what is queued is dropped and nothing more is written.
    off: bool,
}

impl TabWriter {
    pub fn new(dir: Option<PathBuf>, project_root: PathBuf) -> Self {
        TabWriter {
            dir,
            project_root,
            last: SavedTabs::default(),
            worker: None,
            #[cfg(any(test, feature = "test-support"))]
            before_write: None,
        }
    }

    /// Stops all writing, for good, a set still queued included.
    pub fn disable(&mut self) {
        self.dir = None;
        if let Some(worker) = &self.worker {
            let mut queue = worker.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.off = true;
            queue.next = None;
            worker.shared.ready.notify_all();
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.dir.is_some()
    }

    /// `now` is the window's tabs, or `None` while the window is closing and has none to show. Hands a
    /// changed set to the writer and returns at once; touches no disk.
    pub fn observe(&mut self, now: Option<SavedTabs>) {
        let Some(dir) = self.dir.clone() else { return };
        let Some(now) = now else { return };
        if now == self.last {
            return;
        }
        self.last = now.clone();
        let Some(worker) = self.worker_for(dir) else { return };
        let mut queue = worker.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.next = Some(now);
        worker.shared.ready.notify_all();
    }

    /// No more sets come: the writer writes what it was last handed and ends. The receiver reports once it
    /// has (or is gone when the thread died); `None` when no writer ever started, so nothing is owed.
    pub fn finish(&mut self) -> Option<std::sync::mpsc::Receiver<()>> {
        let worker = self.worker.as_mut()?;
        {
            let mut queue = worker.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.finished = true;
            worker.shared.ready.notify_all();
        }
        worker.done.take()
    }

    /// Runs `hook` on the writer thread before each write, for a test that holds a write under way. Set before
    /// the first hand-over: the thread takes it when it starts.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_before_write(&mut self, hook: std::sync::Arc<dyn Fn() + Send + Sync>) {
        self.before_write = Some(hook);
    }

    /// Waits until the writer has written everything handed to it, for a test to read the file.
    #[cfg(any(test, feature = "test-support"))]
    pub fn wait_written(&self, within: std::time::Duration) -> bool {
        let Some(worker) = &self.worker else { return true };
        let deadline = std::time::Instant::now() + within;
        let mut queue = worker.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        while queue.next.is_some() || queue.writing {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            queue = worker
                .shared
                .ready
                .wait_timeout(queue, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }

    fn worker_for(&mut self, dir: PathBuf) -> Option<&WriterHandle> {
        if self.worker.is_none() {
            let shared = std::sync::Arc::new(WriterShared::default());
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let memory = TabMemory::new(Some(dir), self.project_root.clone());
            let thread_shared = shared.clone();
            #[cfg(any(test, feature = "test-support"))]
            let before_write = self.before_write.clone();
            let spawned = std::thread::Builder::new()
                .name("eitri-saved-tabs".into())
                .spawn(move || {
                    #[cfg(any(test, feature = "test-support"))]
                    let hook = move || {
                        if let Some(hook) = &before_write {
                            hook();
                        }
                    };
                    #[cfg(not(any(test, feature = "test-support")))]
                    let hook = || {};
                    write_loop(&thread_shared, memory, hook);
                    let _ = done_tx.send(());
                });
            if let Err(e) = spawned {
                eprintln!("[tabs] the saved-tabs writer could not start ({e}); the open tabs are not kept");
                self.dir = None;
                return None;
            }
            self.worker = Some(WriterHandle {
                shared,
                done: Some(done_rx),
            });
        }
        self.worker.as_ref()
    }
}

/// A writer dropped without [`TabWriter::finish`] (a window's state that went away some other way) still lets
/// its thread end: it writes what it was last handed and returns. Nothing here waits for it.
impl Drop for TabWriter {
    fn drop(&mut self) {
        if let Some(worker) = &self.worker {
            let mut queue = worker.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.finished = true;
            worker.shared.ready.notify_all();
        }
    }
}

/// The writer thread: takes the newest set, writes it outside the lock, and ends once finished and empty.
fn write_loop(shared: &WriterShared, mut memory: TabMemory, before_write: impl Fn()) {
    loop {
        let next = {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if queue.off {
                    queue.writing = false;
                    shared.ready.notify_all();
                    return;
                }
                if let Some(next) = queue.next.take() {
                    queue.writing = true;
                    break next;
                }
                if queue.finished {
                    return;
                }
                queue = shared.ready.wait(queue).unwrap_or_else(|e| e.into_inner());
            }
        };
        before_write();
        // Not `observe`: the window's thread compared this set with the last one it handed over, and the set
        // in between may never have been taken here (replaced while queued), so this thread's own last set
        // can be stale. An emptied window, the set before it never taken, must still empty the file.
        match memory.write(next) {
            Some(Err(e)) => eprintln!("[tabs] could not save the open tabs: {e}"),
            Some(Ok(Written {
                set_aside: Some(aside), ..
            })) => eprintln!(
                "[tabs] an unusable saved-tabs file was set aside as {}",
                aside.display()
            ),
            Some(Ok(_)) | None => {}
        }
        let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.writing = false;
        shared.ready.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_scratch_dir::ScratchDir;

    fn root() -> PathBuf {
        PathBuf::from("/home/user/project")
    }

    fn tab(session: &str, name: Option<&str>, mode: SessionModeChoice) -> SavedTab {
        SavedTab {
            conversation_id: "0123456789abcdef0123456789abcdef".to_string(),
            provider_session_id: session.to_string(),
            name: name.map(str::to_string),
            mode,
        }
    }

    fn two() -> SavedTabs {
        SavedTabs {
            tabs: vec![
                tab("s-one", Some("api"), SessionModeChoice::Auto),
                tab("s-two", None, SessionModeChoice::Bypass),
            ],
            active: 1,
            ..SavedTabs::default()
        }
    }

    #[test]
    fn it_lives_beside_the_layout_and_agent_directories() {
        assert_eq!(
            state_dir(Some(OsStr::new("/s")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/s/eitri/tabs"))
        );
        assert_eq!(
            state_dir(None, Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.local/state/eitri/tabs"))
        );
        assert_eq!(state_dir(Some(OsStr::new("relative")), None), None);
    }

    #[test]
    fn the_file_round_trips_with_order_names_modes_and_the_active_tab() {
        let saved = two();
        let text = encode(&saved, &root());
        assert_eq!(decode(&text, &root()), Ok(saved));
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["active"], 1);
        assert_eq!(value["tabs"][0]["name"], "api");
        assert_eq!(value["tabs"][0]["mode"], "auto");
        assert_eq!(value["tabs"][1]["mode"], "bypass");
        assert!(value["tabs"][1].get("name").is_none(), "no rename, no key");
    }

    /// A tab saved in manual is written as the word `manual` and read back as manual.
    #[test]
    fn a_manual_tab_is_saved_as_manual_and_read_back() {
        let saved = SavedTabs {
            tabs: vec![tab("m", None, SessionModeChoice::Manual)],
            active: 0,
            external_tabs: vec![SavedExternalTab {
                agent: AgentId::Codex,
                conversation_id: "c".to_string(),
                provider_session_id: "x".to_string(),
                name: None,
                mode: SessionModeChoice::Manual,
            }],
            active_external: None,
        };
        let text = encode(&saved, &root());
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["tabs"][0]["mode"], "manual");
        assert_eq!(value["external_tabs"][0]["mode"], "manual");
        assert_eq!(decode(&text, &root()).unwrap(), saved);
    }

    #[test]
    fn a_wrong_version_another_projects_file_or_a_broken_one_cannot_be_decoded() {
        let text = encode(&two(), &root());
        let newer = text.replace("\"version\": 1", "\"version\": 2");
        assert!(decode(&newer, &root()).unwrap_err().contains("version 2"));
        assert!(decode(&text, Path::new("/elsewhere"))
            .unwrap_err()
            .contains("written for"));
        assert!(decode("{ not json", &root()).is_err());
        assert!(decode(
            r#"{"version":1,"project_root":"/home/user/project","tabs":[]}"#,
            &root()
        )
        .is_err());
    }

    #[test]
    fn a_hand_edit_is_made_usable_rather_than_trusted() {
        let text = r#"{"version":1,"project_root":"/home/user/project","active":2,"tabs":[
            {"conversation_id":"c","provider_session_id":"a","mode":"auto"},
            {"conversation_id":"c","provider_session_id":"","mode":"auto"},
            {"conversation_id":"c","provider_session_id":"b","mode":"auto"},
            {"conversation_id":"c","provider_session_id":"a","mode":"bypass"}]}"#;
        let saved = decode(text, &root()).unwrap();
        let ids: Vec<&str> = saved.tabs.iter().map(|t| t.provider_session_id.as_str()).collect();
        assert_eq!(
            ids,
            ["a", "b"],
            "the blank is dropped and the repeat kept once, in its first place"
        );
        assert_eq!(
            saved.tabs[0].mode,
            SessionModeChoice::Auto,
            "the first listing of a repeated session wins"
        );
        assert_eq!(
            saved.active, 1,
            "the tab that was active is still the one, whatever was dropped before it"
        );

        let past_the_end = text.replace("\"active\":2", "\"active\":9");
        assert_eq!(decode(&past_the_end, &root()).unwrap().active, 0);
    }

    #[test]
    fn a_missing_file_is_nothing_to_offer_and_says_nothing() {
        let dir = ScratchDir::new("nv-saved-tabs", "missing");
        assert_eq!(startup(Some(&dir), &root()), (None, Vec::new()));
        assert_eq!(startup(None, &root()), (None, Vec::new()));
    }

    #[test]
    fn a_file_listing_no_tab_offers_nothing() {
        let dir = ScratchDir::new("nv-saved-tabs", "empty");
        save(&dir, &root(), &SavedTabs::default()).unwrap();
        assert_eq!(startup(Some(&dir), &root()), (None, Vec::new()));
        assert!(
            dir.join(file_name(&root())).exists(),
            "it is a normal file, left where it is"
        );
    }

    #[test]
    fn a_saved_file_is_offered_as_it_was_left() {
        let dir = ScratchDir::new("nv-saved-tabs", "offered");
        save(&dir, &root(), &two()).unwrap();
        assert_eq!(startup(Some(&dir), &root()), (Some(two()), Vec::new()));
    }

    /// An unusable file is kept for a person to read and is never replaced by the window's own later
    /// write: afterwards the project has no file and the `.unusable` beside it holds every byte.
    #[test]
    fn an_unusable_file_is_set_aside_whole_at_startup() {
        let dir = ScratchDir::new("nv-saved-tabs", "unusable");
        let path = dir.join(file_name(&root()));
        std::fs::write(&path, "{ not json").unwrap();
        let (saved, notes) = startup(Some(&dir), &root());
        assert_eq!(saved, None);
        assert!(notes.iter().any(|n| n.contains("unusable")), "{notes:?}");
        assert!(!path.exists());
        let aside = dir.join(format!("{}.unusable", file_name(&root())));
        assert_eq!(std::fs::read_to_string(aside).unwrap(), "{ not json");

        // A newer build's file is set aside the same way.
        std::fs::write(
            &path,
            encode(&two(), &root()).replace("\"version\": 1", "\"version\": 7"),
        )
        .unwrap();
        let (saved, notes) = startup(Some(&dir), &root());
        assert_eq!((saved, notes.len()), (None, 2));
        assert!(!path.exists());
    }

    #[test]
    fn the_file_is_private_and_leaves_no_temporary_behind() {
        use std::os::unix::fs::PermissionsExt;
        let dir = ScratchDir::new("nv-saved-tabs", "private");
        let state = dir.join("eitri").join("tabs");
        let path = save(&state, &root(), &two()).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&state), 0o700);
        let names: Vec<_> = std::fs::read_dir(&state)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "no .tmp left behind: {names:?}");
    }

    fn memory(dir: &Path) -> TabMemory {
        TabMemory::new(Some(dir.to_path_buf()), root())
    }

    fn numbered(n: usize) -> SavedTabs {
        SavedTabs {
            tabs: (0..=n % 7)
                .map(|i| tab(&format!("s-{n}-{i}"), None, SessionModeChoice::Auto))
                .collect(),
            active: 0,
            ..SavedTabs::default()
        }
    }

    /// The window's thread never waits for the disk: with the writer held inside a write, a hundred changes
    /// are handed over before the writer is released or its hang guard expires. When it is let go the file
    /// ends at the last of them, with the modes the synchronous write had.
    #[test]
    fn the_writer_never_blocks_the_caller_and_the_last_set_wins() {
        use std::os::unix::fs::PermissionsExt;
        let calibration = (0..3)
            .map(|_| {
                let started = std::time::Instant::now();
                for n in 0..100 {
                    std::hint::black_box(numbered(n).clone() == numbered((n + 1) % 100));
                }
                started.elapsed()
            })
            .min()
            .unwrap();
        let mut min_took = std::time::Duration::MAX;
        for attempt in 0..3 {
            let dir = ScratchDir::new("nv-saved-tabs", &format!("writer{attempt}"));
            let state = dir.join("state");
            let mut writer = TabWriter::new(Some(state.clone()), root());
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let release_rx = std::sync::Mutex::new(release_rx);
            let held = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            let gave_up = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            {
                let held = held.clone();
                let gave_up = gave_up.clone();
                writer.before_write = Some(std::sync::Arc::new(move || {
                    if held.load(std::sync::atomic::Ordering::SeqCst) {
                        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = release_rx
                            .lock()
                            .unwrap()
                            .recv_timeout(std::time::Duration::from_secs(60))
                        {
                            gave_up.store(true, std::sync::atomic::Ordering::SeqCst);
                            held.store(false, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                }));
            }
            let started = std::time::Instant::now();
            for n in 0..100 {
                writer.observe(Some(numbered(n)));
            }
            let took = started.elapsed();
            min_took = min_took.min(took);
            assert!(
                !gave_up.load(std::sync::atomic::Ordering::SeqCst),
                "the hand-overs had to finish while the writer was still held"
            );
            held.store(false, std::sync::atomic::Ordering::SeqCst);
            release_tx.send(()).unwrap();
            let done = writer.finish().expect("a writer ran");
            done.recv_timeout(std::time::Duration::from_secs(60))
                .expect("the last write and the end");
            assert_eq!(load(&state, &root()), Loaded::Saved(numbered(99)));
            let path = state.join(file_name(&root()));
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(&state).unwrap().permissions().mode() & 0o777, 0o700);
        }
        // The writer is held: waiting for it or writing to disk on the caller costs milliseconds per
        // hand-over against a calibration of microseconds; a healthy hand-over is a lock and a notify.
        // The generous factor absorbs scheduler noise, and the minimum drops a preempted attempt.
        assert!(
            min_took < calibration * 500,
            "hand-overs took {min_took:?}, against a caller-work calibration of {calibration:?}"
        );
    }

    /// Each change reaches the disk in order: waiting between hand-overs, every set is what the file holds.
    #[test]
    fn the_writer_writes_each_change_in_order() {
        let dir = ScratchDir::new("nv-saved-tabs", "writer-order");
        let mut writer = TabWriter::new(Some(dir.to_path_buf()), root());
        assert!(writer.finish().is_none(), "nothing written, nothing owed");
        let mut writer = TabWriter::new(Some(dir.to_path_buf()), root());
        writer.observe(Some(SavedTabs::default()));
        assert!(writer.worker.is_none(), "no change, no writer");
        for n in 0..20 {
            writer.observe(Some(numbered(n)));
            assert!(writer.wait_written(std::time::Duration::from_secs(5)));
            assert_eq!(load(&dir, &root()), Loaded::Saved(numbered(n)));
        }
        writer.observe(None);
        let done = writer.finish().unwrap();
        done.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(
            load(&dir, &root()),
            Loaded::Saved(numbered(19)),
            "a closing window writes nothing"
        );
    }

    /// A window that adopted a session and closed it again before the writer took the first set hands over
    /// tabs, then no tabs, and only the second is ever taken. The thread writes it although it never wrote
    /// anything else: the file the launch found is emptied, not left to be offered again.
    #[test]
    fn an_emptied_set_whose_predecessor_was_never_taken_is_still_written() {
        let dir = ScratchDir::new("nv-saved-tabs", "writer-emptied");
        save(&dir, &root(), &two()).unwrap();
        let shared = WriterShared::default();
        {
            let mut queue = shared.queue.lock().unwrap();
            queue.next = Some(SavedTabs::default());
            queue.finished = true;
        }
        write_loop(&shared, memory(&dir), || {});
        assert_eq!(load(&dir, &root()), Loaded::Saved(SavedTabs::default()));

        // The same through the whole writer. Whether the thread takes the first set before it is replaced or not
        // is up to the scheduler, so this half only checks the file ends empty either way; the half above pins
        // the case that went wrong.
        let dir = ScratchDir::new("nv-saved-tabs", "writer-emptied-whole");
        save(&dir, &root(), &two()).unwrap();
        let mut writer = TabWriter::new(Some(dir.to_path_buf()), root());
        let gate = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        writer.before_write = Some({
            let gate = gate.clone();
            std::sync::Arc::new(move || {
                let (open, ready) = &*gate;
                let open = open.lock().unwrap();
                let _ = ready.wait_timeout_while(open, std::time::Duration::from_secs(10), |open| !*open);
            })
        });
        writer.observe(Some(numbered(3)));
        writer.observe(Some(SavedTabs::default()));
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        let done = writer.finish().unwrap();
        done.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert_eq!(load(&dir, &root()), Loaded::Saved(SavedTabs::default()));
    }

    /// A writer dropped without `finish` ends its thread, after the last set it was handed, and the drop
    /// itself does not wait.
    #[test]
    fn a_dropped_writer_ends_its_thread() {
        let dir = ScratchDir::new("nv-saved-tabs", "writer-drop");
        let mut writer = TabWriter::new(Some(dir.to_path_buf()), root());
        writer.observe(Some(numbered(4)));
        let done = writer.worker.as_mut().unwrap().done.take().unwrap();
        let started = std::time::Instant::now();
        drop(writer);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "the drop waited"
        );
        done.recv_timeout(std::time::Duration::from_secs(5))
            .expect("the thread ended");
        assert_eq!(load(&dir, &root()), Loaded::Saved(numbered(4)));
    }

    /// Turning remembering off drops a set still queued and writes nothing more.
    #[test]
    fn a_disabled_writer_drops_what_is_queued() {
        let dir = ScratchDir::new("nv-saved-tabs", "writer-off");
        let mut writer = TabWriter::new(Some(dir.to_path_buf()), root());
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        writer.before_write = Some(std::sync::Arc::new(move || {
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10));
        }));
        writer.observe(Some(numbered(1)));
        writer.observe(Some(numbered(2)));
        writer.disable();
        assert!(!writer.is_enabled());
        writer.observe(Some(numbered(3)));
        release_tx.send(()).unwrap();
        let done = writer.finish().unwrap();
        done.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        // The write already under way may finish; the queued one and anything later never do.
        match load(&dir, &root()) {
            Loaded::Missing => {}
            Loaded::Saved(saved) => assert_eq!(saved, numbered(1)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_window_that_has_done_nothing_writes_nothing_and_leaves_the_last_windows_file() {
        let dir = ScratchDir::new("nv-saved-tabs", "quiet");
        save(&dir, &root(), &two()).unwrap();
        let mut memory = memory(&dir);
        assert!(memory.observe(Some(SavedTabs::default())).is_none());
        assert!(memory.observe(Some(SavedTabs::default())).is_none());
        assert_eq!(load(&dir, &root()), Loaded::Saved(two()));
    }

    #[test]
    fn it_writes_each_change_once_and_only_a_change() {
        let dir = ScratchDir::new("nv-saved-tabs", "changes");
        let mut memory = memory(&dir);
        let first = SavedTabs {
            tabs: vec![tab("s-one", None, SessionModeChoice::Auto)],
            active: 0,
            ..SavedTabs::default()
        };
        assert!(matches!(memory.observe(Some(first.clone())), Some(Ok(_))));
        assert_eq!(load(&dir, &root()), Loaded::Saved(first.clone()));
        assert!(
            memory.observe(Some(first.clone())).is_none(),
            "the same tabs again writes nothing"
        );

        // Each of the things the window can change about a listed tab is a write.
        let mut next = first.clone();
        next.tabs[0].name = Some("renamed".to_string());
        assert!(matches!(memory.observe(Some(next.clone())), Some(Ok(_))));
        next.tabs[0].mode = SessionModeChoice::Bypass;
        assert!(matches!(memory.observe(Some(next.clone())), Some(Ok(_))));
        next.tabs.push(tab("s-two", None, SessionModeChoice::Auto));
        assert!(matches!(memory.observe(Some(next.clone())), Some(Ok(_))));
        next.active = 1;
        assert!(matches!(memory.observe(Some(next.clone())), Some(Ok(_))));
        next.tabs.reverse();
        assert!(matches!(memory.observe(Some(next.clone())), Some(Ok(_))), "a reorder");
        assert_eq!(load(&dir, &root()), Loaded::Saved(next));
    }

    /// Closing the last listed tab on purpose is a change like any other; the window closing is not.
    #[test]
    fn closing_the_last_tab_is_recorded_but_the_window_closing_is_not() {
        let dir = ScratchDir::new("nv-saved-tabs", "closing");
        let mut memory = memory(&dir);
        assert!(matches!(memory.observe(Some(two())), Some(Ok(_))));
        assert!(memory.observe(None).is_none(), "a closing window has no tabs to show");
        assert_eq!(
            load(&dir, &root()),
            Loaded::Saved(two()),
            "the file still says what was open"
        );
        assert!(matches!(memory.observe(Some(SavedTabs::default())), Some(Ok(_))));
        assert_eq!(load(&dir, &root()), Loaded::Saved(SavedTabs::default()));
    }

    #[test]
    fn a_failed_write_is_reported_once_per_change() {
        let dir = ScratchDir::new("nv-saved-tabs", "failing");
        // A file where the directory should be: every write under it fails.
        let blocker = dir.join("blocked");
        std::fs::write(&blocker, "").unwrap();
        let mut memory = TabMemory::new(Some(blocker.join("tabs")), root());
        assert!(matches!(memory.observe(Some(two())), Some(Err(_))));
        assert!(
            memory.observe(Some(two())).is_none(),
            "not again until something changes"
        );
    }

    #[test]
    fn a_disabled_memory_never_writes() {
        let dir = ScratchDir::new("nv-saved-tabs", "disabled");
        let mut memory = memory(&dir);
        memory.disable();
        assert!(!memory.is_enabled());
        assert!(memory.observe(Some(two())).is_none());
        assert_eq!(load(&dir, &root()), Loaded::Missing);
        assert!(TabMemory::new(None, root()).observe(Some(two())).is_none());
    }

    /// If the bad file cannot be set aside, it is left where it is: replacing it would destroy the one copy
    /// of what a person may want to read.
    #[test]
    fn a_bad_file_that_cannot_be_set_aside_is_not_overwritten() {
        let dir = ScratchDir::new("nv-saved-tabs", "aside-blocked");
        let mut memory = memory(&dir);
        let path = dir.join(file_name(&root()));
        std::fs::write(&path, "typo").unwrap();
        // The place it would go is a directory that is not empty, so the rename fails.
        let aside = dir.join(format!("{}.unusable", file_name(&root())));
        std::fs::create_dir_all(aside.join("occupied")).unwrap();
        let outcome = memory.observe(Some(two())).expect("a write was due");
        assert!(outcome.is_err(), "reported, not swallowed");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "typo",
            "the bad file is untouched"
        );
        // It is tried again when the tabs next change, not on every tick.
        assert!(memory.observe(Some(two())).is_none());
    }

    /// A file that went bad while the window was open is kept, not overwritten.
    #[test]
    fn a_file_that_went_bad_since_launch_is_set_aside_before_the_write() {
        let dir = ScratchDir::new("nv-saved-tabs", "went-bad");
        let mut memory = memory(&dir);
        std::fs::create_dir_all(&*dir).unwrap();
        std::fs::write(dir.join(file_name(&root())), "typo").unwrap();
        let written = memory.observe(Some(two())).unwrap().unwrap();
        let aside = written.set_aside.expect("the bad file was set aside");
        assert_eq!(std::fs::read_to_string(aside).unwrap(), "typo");
        assert_eq!(load(&dir, &root()), Loaded::Saved(two()));
    }

    fn codex(session: &str, mode: SessionModeChoice) -> SavedExternalTab {
        SavedExternalTab {
            agent: AgentId::Codex,
            conversation_id: "0123456789abcdef0123456789abcdef".to_string(),
            provider_session_id: session.to_string(),
            name: Some("refactor".to_string()),
            mode,
        }
    }

    #[test]
    fn external_tabs_round_trip_and_old_files_load() {
        let mut saved = two();
        saved.external_tabs = vec![
            codex("thread-a", SessionModeChoice::Bypass),
            SavedExternalTab {
                agent: AgentId::Acp("gemini".into()),
                name: None,
                ..codex("session-b", SessionModeChoice::Auto)
            },
        ];
        saved.active_external = Some(1);
        let text = encode(&saved, &root());
        assert!(text.contains("\"external_tabs\""), "{text}");
        assert!(
            text.contains("\"agent\": \"codex\"") && text.contains("\"agent\": \"acp:gemini\""),
            "{text}"
        );
        assert!(text.contains("\"version\": 1"), "the version stays 1: {text}");
        assert_eq!(decode(&text, &root()), Ok(saved));

        // A window that ran only Claude writes exactly what it always wrote.
        let only_claude = encode(&two(), &root());
        assert!(!only_claude.contains("external"), "{only_claude}");
        // And a file written before external tabs existed loads with none.
        let old = r#"{"version":1,"project_root":"/home/user/project","tabs":[
            {"conversation_id":"c","provider_session_id":"s-one","mode":"auto"}],"active":0}"#;
        let loaded = decode(old, &root()).unwrap();
        assert_eq!(loaded.tabs.len(), 1);
        assert!(loaded.external_tabs.is_empty() && loaded.active_external.is_none());

        // An entry for Claude, an unknown agent or a session listed twice is never kept as another
        // agent's tab; an agent this build does not know is left out and the rest of the file kept.
        let mut twice = two();
        twice.external_tabs = vec![
            codex("thread-a", SessionModeChoice::Auto),
            codex("thread-a", SessionModeChoice::Bypass),
        ];
        twice.active_external = Some(1);
        let kept = decode(&encode(&twice, &root()), &root()).unwrap();
        assert_eq!(kept.external_tabs, vec![codex("thread-a", SessionModeChoice::Auto)]);
        assert_eq!(
            kept.active_external,
            Some(0),
            "the active entry keeps pointing at its session"
        );
        let claude_as_external = encode(&twice, &root()).replace("\"agent\": \"codex\"", "\"agent\": \"claude\"");
        assert!(decode(&claude_as_external, &root()).unwrap().external_tabs.is_empty());
        let unknown = encode(&twice, &root()).replacen("\"agent\": \"codex\"", "\"agent\": \"openai\"", 1);
        let read = decode(&unknown, &root()).expect("the rest of the file is still usable");
        assert_eq!(read.tabs, two().tabs);
        assert_eq!(read.external_tabs, vec![codex("thread-a", SessionModeChoice::Bypass)]);
        assert_eq!(read.active_external, Some(0));

        // A file that holds only another agent's tabs is still something to offer at launch.
        let dir = ScratchDir::new("nv-saved-tabs", "external-only");
        let only_external = SavedTabs {
            external_tabs: vec![codex("thread-a", SessionModeChoice::Auto)],
            active_external: Some(0),
            ..SavedTabs::default()
        };
        save(&dir, &root(), &only_external).unwrap();
        assert_eq!(startup(Some(&dir), &root()).0, Some(only_external));
    }

    /// What an Eitri from before external tabs reads: its own `TabsFile`, copied here as it was.
    #[derive(Deserialize)]
    struct OlderTabsFile {
        version: u32,
        project_root: String,
        tabs: Vec<SavedTab>,
        active: usize,
    }

    #[test]
    fn an_older_reader_ignores_external_tabs() {
        let mut saved = two();
        saved.external_tabs = vec![codex("thread-a", SessionModeChoice::Bypass)];
        saved.active_external = Some(0);
        let text = encode(&saved, &root());
        let older: OlderTabsFile = serde_json::from_str(&text).expect("an older build reads the file");
        assert_eq!(older.version, VERSION);
        assert_eq!(older.project_root, root().to_string_lossy());
        assert_eq!(
            older.tabs,
            two().tabs,
            "only the Claude tabs, so no Codex thread resumes as Claude"
        );
        assert_eq!(
            older.active,
            two().active,
            "and a Claude tab to fall back to as the active one"
        );
    }
}

/// How long a change of the saved tabs holds the thread that observes it (the GTK thread, once per
/// change): `EITRI_MEASURE_DIR` names the directory to write in, so the storage is the one measured.
#[cfg(test)]
mod measure {
    use super::*;

    #[test]
    #[ignore]
    fn measure_a_saved_tabs_write() {
        let dir = PathBuf::from(std::env::var("EITRI_MEASURE_DIR").expect("EITRI_MEASURE_DIR"))
            .join(format!("saved-tabs-{}", std::process::id()));
        let root = Path::new("/p");
        let tab = |i: usize| SavedTab {
            conversation_id: format!("c{i}"),
            provider_session_id: format!("s{i}"),
            name: None,
            mode: SessionModeChoice::Auto,
        };
        let mut memory = TabMemory::new(Some(dir.clone()), root.to_path_buf());
        let mut times = Vec::new();
        for i in 0..200 {
            let now = SavedTabs {
                tabs: (0..4).map(|t| tab(t + i % 2)).collect(),
                active: 0,
                external_tabs: Vec::new(),
                active_external: None,
            };
            let started = std::time::Instant::now();
            let written = memory.observe(Some(now));
            times.push(started.elapsed());
            assert!(matches!(written, Some(Ok(_))));
        }
        times.sort();
        eprintln!(
            "saved-tabs write in {}: median {:?}, p99 {:?}, max {:?}",
            dir.display(),
            times[100],
            times[198],
            times[199]
        );
        // The same changes through the writer: what the window's thread spends on each.
        let mut writer = TabWriter::new(Some(dir.clone()), root.to_path_buf());
        let mut times = Vec::new();
        for i in 0..200 {
            let now = SavedTabs {
                tabs: (0..4).map(|t| tab(t + i % 2)).collect(),
                active: 0,
                external_tabs: Vec::new(),
                active_external: None,
            };
            let started = std::time::Instant::now();
            writer.observe(Some(now));
            times.push(started.elapsed());
            // Each change lands before the next, as the window's do.
            assert!(writer.wait_written(std::time::Duration::from_secs(5)));
        }
        times.sort();
        eprintln!(
            "saved-tabs hand-over to the writer in {}: median {:?}, p99 {:?}, max {:?}",
            dir.display(),
            times[100],
            times[198],
            times[199]
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
