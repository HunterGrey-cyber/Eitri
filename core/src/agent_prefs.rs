//! The permission mode an empty session tab starts in, remembered per project (session tabs spec
//! §3.6): `$XDG_STATE_HOME/neovibe/agent/<16 hex>.json`, named like the layout file
//! (`layout::persist::file_name`) and written the same way -- a `.tmp` beside it, then renamed.
//! Never inside the project. A file that cannot be used is set aside as `.json.unusable` and read as
//! nothing, as the layout file is.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent_bridge::SessionModeChoice;
use crate::layout::persist::{file_name, state_subdir, temporary};

const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct PrefsFile {
    version: u32,
    project_root: String,
    permission_mode: SessionModeChoice,
}

/// `<state home>/neovibe/agent`.
pub fn state_dir(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    state_subdir(xdg_state_home, home, "agent")
}

#[derive(Debug, PartialEq, Eq)]
pub enum LoadedMode {
    Remembered(SessionModeChoice),
    Missing,
    Unusable(String),
}

pub fn load_mode(dir: &Path, project_root: &Path) -> LoadedMode {
    let path = dir.join(file_name(project_root));
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LoadedMode::Missing,
        Err(e) => return LoadedMode::Unusable(format!("{}: {e}", path.display())),
    };
    let file: PrefsFile = match serde_json::from_str(&text) {
        Ok(file) => file,
        Err(e) => return LoadedMode::Unusable(format!("{}: not an agent prefs file ({e})", path.display())),
    };
    if file.version != VERSION {
        return LoadedMode::Unusable(format!(
            "{}: version {} (this build reads {VERSION})",
            path.display(),
            file.version
        ));
    }
    let root = project_root.to_string_lossy();
    if file.project_root != root {
        return LoadedMode::Unusable(format!(
            "{}: written for {}, not {root}",
            path.display(),
            file.project_root
        ));
    }
    LoadedMode::Remembered(file.permission_mode)
}

pub fn save_mode(dir: &Path, project_root: &Path, mode: SessionModeChoice) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(file_name(project_root));
    let tmp = temporary(&path);
    let file = PrefsFile {
        version: VERSION,
        project_root: project_root.to_string_lossy().into_owned(),
        permission_mode: mode,
    };
    let text = serde_json::to_string_pretty(&file).expect("prefs always serialize");
    let written = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(err) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(path)
}

pub fn set_aside(dir: &Path, project_root: &Path) -> std::io::Result<PathBuf> {
    let path = dir.join(file_name(project_root));
    let aside = path.with_extension("json.unusable");
    std::fs::rename(&path, &aside)?;
    Ok(aside)
}

/// The mode new tabs take at launch, and what to log. `dir` is `None` when there is no state
/// directory at all. The first mode `CLIENT_IMPLEMENTED_PERMISSION_MODES` offers is the default.
pub fn startup_mode(dir: Option<&Path>, project_root: &Path) -> (SessionModeChoice, Vec<String>) {
    let default = crate::agent_backend::CLIENT_IMPLEMENTED_PERMISSION_MODES
        .first()
        .and_then(|m| SessionModeChoice::parse(m))
        .unwrap_or(SessionModeChoice::Auto);
    let Some(dir) = dir else {
        return (
            default,
            vec!["[agent] no state directory: new tabs start in the first mode".to_string()],
        );
    };
    match load_mode(dir, project_root) {
        LoadedMode::Remembered(mode) => (mode, Vec::new()),
        LoadedMode::Missing => (default, Vec::new()),
        LoadedMode::Unusable(why) => {
            let mut notes = vec![format!(
                "[agent] the remembered mode is unusable ({why}); starting in the first mode"
            )];
            match set_aside(dir, project_root) {
                Ok(aside) => notes.push(format!("[agent] set aside as {}", aside.display())),
                Err(e) => notes.push(format!("[agent] could not set the unusable file aside: {e}")),
            }
            (default, notes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_bridge::SessionModeChoice;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nv-agent-prefs-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn root() -> PathBuf {
        PathBuf::from("/home/user/project")
    }

    #[test]
    fn it_lives_beside_the_layout_directory() {
        assert_eq!(
            state_dir(Some(OsStr::new("/s")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/s/neovibe/agent"))
        );
        assert_eq!(
            state_dir(None, Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.local/state/neovibe/agent"))
        );
        assert_eq!(state_dir(Some(OsStr::new("relative")), None), None);
    }

    #[test]
    fn a_saved_mode_is_read_back_and_nothing_else_is_written() {
        let dir = scratch("roundtrip");
        assert_eq!(load_mode(&dir, &root()), LoadedMode::Missing);
        let path = save_mode(&dir, &root(), SessionModeChoice::Bypass).unwrap();
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            crate::layout::persist::file_name(&root())
        );
        assert_eq!(
            load_mode(&dir, &root()),
            LoadedMode::Remembered(SessionModeChoice::Bypass)
        );
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "no .tmp left behind: {names:?}");
    }

    #[test]
    fn another_projects_file_or_a_broken_one_is_unusable_and_set_aside_at_startup() {
        let dir = scratch("unusable");
        std::fs::write(dir.join(crate::layout::persist::file_name(&root())), "{ not json").unwrap();
        assert!(matches!(load_mode(&dir, &root()), LoadedMode::Unusable(_)));
        let (mode, notes) = startup_mode(Some(&dir), &root());
        assert_eq!(mode, SessionModeChoice::Auto, "read as nothing: the first offered mode");
        assert!(notes.iter().any(|n| n.contains("unusable")), "{notes:?}");
        assert!(dir
            .join(format!("{}.unusable", crate::layout::persist::file_name(&root())))
            .exists());
        assert_eq!(
            load_mode(&dir, &root()),
            LoadedMode::Missing,
            "set aside, not left in the way"
        );

        let foreign = "{\"version\":1,\"project_root\":\"/home/user/elsewhere\",\"permission_mode\":\"bypass\"}";
        std::fs::write(dir.join(crate::layout::persist::file_name(&root())), foreign).unwrap();
        assert!(
            matches!(load_mode(&dir, &root()), LoadedMode::Unusable(why) if why.contains("written for")),
            "a file under this project's name but written for another root is not this project's"
        );
    }

    #[test]
    fn with_no_state_directory_new_tabs_start_in_the_first_offered_mode() {
        assert_eq!(startup_mode(None, &root()).0, SessionModeChoice::Auto);
    }

    #[test]
    fn the_mode_cycles_through_what_the_client_offers_and_wraps() {
        let offered = crate::agent_backend::CLIENT_IMPLEMENTED_PERMISSION_MODES;
        assert_eq!(SessionModeChoice::Auto.cycled(offered), SessionModeChoice::Bypass);
        assert_eq!(SessionModeChoice::Bypass.cycled(offered), SessionModeChoice::Auto);
        assert_eq!(
            SessionModeChoice::Bypass.cycled(&["auto"]),
            SessionModeChoice::Auto,
            "a mode not offered goes to the first"
        );
        assert_eq!(SessionModeChoice::parse("bypass"), Some(SessionModeChoice::Bypass));
        assert_eq!(SessionModeChoice::parse("verdandi_rules"), None);
        assert_eq!(SessionModeChoice::Auto.as_str(), "auto");
    }
}
