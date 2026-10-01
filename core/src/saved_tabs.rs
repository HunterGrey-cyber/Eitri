//! The session tabs a project had open, kept between launches so the next launch can offer them
//! back: `$XDG_STATE_HOME/eitri/tabs/<16 hex>.json`, named, written and set aside the way the layout
//! file is (`layout::persist`): 0600 in a 0700 directory, a `.tmp` beside it and then a rename, a file
//! that cannot be used moved whole to `<name>.json.unusable` and read as nothing. Never inside the
//! project.
//!
//! **What is listed:** only tabs that have a Claude session (an empty tab has nothing to bring
//! back), in tab-bar order, each with the rename it carries and the permission mode it was in, and
//! the index of the tab that was active. Two windows on one project each write their own set and the
//! last writer wins; the session leases, not this file, are what stop two windows driving one
//! session.
//!
//! **When it is written** is [`TabMemory`]'s job: whenever the set it is shown differs from the set
//! it last saw, and never because the window is closing -- closing closes every tab, and recording
//! "no tabs" then would erase exactly what the next launch wants.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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

/// The tabs of one window, in tab-bar order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedTabs {
    pub tabs: Vec<SavedTab>,
    /// Index into `tabs` of the tab that was on screen.
    pub active: usize,
}

#[derive(Serialize, Deserialize)]
struct TabsFile {
    version: u32,
    project_root: String,
    tabs: Vec<SavedTab>,
    active: usize,
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
    Ok(SavedTabs { tabs: kept, active })
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
    match std::fs::read_to_string(&path) {
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
        Loaded::Saved(saved) if saved.tabs.is_empty() => (None, Vec::new()),
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
        let dir = self.dir.as_ref()?;
        let now = now?;
        if now == self.last {
            return None;
        }
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
}
