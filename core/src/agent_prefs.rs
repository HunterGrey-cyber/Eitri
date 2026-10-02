//! The permission mode an empty session tab starts in, remembered per project (session tabs spec
//! §3.6): `$XDG_STATE_HOME/eitri/agent/<16 hex>.json`, named like the layout file
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

/// `<state home>/eitri/agent`.
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
    let text = match agent::private_fs::read_private_to_string(&path) {
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

/// D3 (R07/S2): bypass is never written to disk. Refused with an `InvalidInput` error, in every
/// build: this was a `debug_assert!`, which a release build compiles out, so D3 held only because
/// every caller happens to save on `ModeCycle::Changed(Auto)` alone (the whole-branch review). Both
/// callers already log a failed save and carry on, so the refusal needs no new plumbing.
pub fn save_mode(dir: &Path, project_root: &Path, mode: SessionModeChoice) -> std::io::Result<PathBuf> {
    if mode == SessionModeChoice::Bypass {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "D3: bypass is never written to disk",
        ));
    }
    crate::layout::persist::create_state_dir(dir)?;
    let path = dir.join(file_name(project_root));
    let tmp = temporary(&path);
    let file = PrefsFile {
        version: VERSION,
        project_root: project_root.to_string_lossy().into_owned(),
        permission_mode: mode,
    };
    let text = serde_json::to_string_pretty(&file).expect("prefs always serialize");
    let written = agent::private_fs::write_private(&tmp, text.as_bytes()).and_then(|()| std::fs::rename(&tmp, &path));
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

/// The `init.lua` key: `eitri.config.set("agent.default_mode", "auto" | "bypass")`.
pub const DEFAULT_MODE_KEY: &str = "agent.default_mode";

/// The value of [`DEFAULT_MODE_KEY`] as `init.lua` left it: `None` for unset (new tabs start in
/// auto, as ever), else the mode named. Anything but the two words is an error naming the key, as
/// every other configuration key here is.
pub fn parse_default_mode(value: Option<&str>) -> Result<Option<SessionModeChoice>, String> {
    let Some(raw) = value else { return Ok(None) };
    SessionModeChoice::parse(raw.trim())
        .map(Some)
        .ok_or_else(|| format!("eitri.config.set(\"{DEFAULT_MODE_KEY}\", {raw:?}): must be \"auto\" or \"bypass\""))
}

/// The `init.lua` key: `eitri.config.set("agent.user_settings", true | false)`.
pub const USER_SETTINGS_KEY: &str = "agent.user_settings";

/// The value of [`USER_SETTINGS_KEY`] as `init.lua` left it: unset or `true` means sessions load the
/// user's own Claude Code configuration (`~/.claude`), as the CLI does in a terminal; `false` leaves
/// that tier out and keeps project and local. The store holds text, so a Lua boolean arrives as
/// `"true"` / `"false"`. Anything else is an error naming the key, as every other configuration key
/// here is.
pub fn parse_user_settings(value: Option<&str>) -> Result<bool, String> {
    match value.map(str::trim) {
        None | Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(_) => Err(format!(
            "eitri.config.set(\"{USER_SETTINGS_KEY}\", {:?}): must be true or false",
            value.unwrap_or_default()
        )),
    }
}

/// Whether this project has a remembered choice of mode that the configured default must not
/// override: a readable file that says `auto` (the only thing [`save_mode`] writes, once Shift+Tab
/// has left bypass). A file from before v1 that says bypass is not a choice anyone is still making,
/// and a missing or unusable one says nothing.
pub fn remembers_a_choice(dir: Option<&Path>, project_root: &Path) -> bool {
    dir.is_some_and(|dir| load_mode(dir, project_root) == LoadedMode::Remembered(SessionModeChoice::Auto))
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
        // D3/R07 (v1): a launch never starts in bypass, even one made before v1 remembered it. The
        // file is deliberately left alone -- `save_mode` refuses bypass, and nothing here rewrites
        // it to `auto` either (the next auto that is saved does), and the note says why the owner's
        // old setting is not honoured, rather than silently starting in a different mode than
        // before.
        LoadedMode::Remembered(SessionModeChoice::Bypass) => (
            SessionModeChoice::Auto,
            vec![
                "[agent] this project remembered bypass; every launch starts in auto since v1 \
                 (R07/S2) -- Shift+Tab switches, and asks first"
                    .to_string(),
            ],
        ),
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
    use crate::test_scratch_dir::ScratchDir;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    fn scratch(label: &str) -> ScratchDir {
        ScratchDir::new("nv-agent-prefs", label)
    }

    fn root() -> PathBuf {
        PathBuf::from("/home/user/project")
    }

    #[test]
    fn it_lives_beside_the_layout_directory() {
        assert_eq!(
            state_dir(Some(OsStr::new("/s")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/s/eitri/agent"))
        );
        assert_eq!(
            state_dir(None, Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.local/state/eitri/agent"))
        );
        assert_eq!(state_dir(Some(OsStr::new("relative")), None), None);
    }

    /// **Correction (D3, R07/S2): `Bypass` is dropped from this roundtrip.** `save_mode` refuses it
    /// (`bypass_is_never_written_in_any_build`) -- `Auto` is the only mode it ever writes.
    #[test]
    fn a_saved_mode_is_read_back_and_nothing_else_is_written() {
        let dir = scratch("roundtrip");
        assert_eq!(load_mode(&dir, &root()), LoadedMode::Missing);
        let path = save_mode(&dir, &root(), SessionModeChoice::Auto).unwrap();
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            crate::layout::persist::file_name(&root())
        );
        assert_eq!(
            load_mode(&dir, &root()),
            LoadedMode::Remembered(SessionModeChoice::Auto)
        );
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "no .tmp left behind: {names:?}");
    }

    /// D3, in a release build too (it was a `debug_assert!`): nothing is written, and an `auto` already
    /// on disk is left exactly as it was.
    #[test]
    fn bypass_is_never_written_in_any_build() {
        let dir = scratch("never-bypass");
        let error = save_mode(&dir, &root(), SessionModeChoice::Bypass).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(load_mode(&dir, &root()), LoadedMode::Missing, "nothing written");
        save_mode(&dir, &root(), SessionModeChoice::Auto).unwrap();
        assert!(save_mode(&dir, &root(), SessionModeChoice::Bypass).is_err());
        assert_eq!(
            load_mode(&dir, &root()),
            LoadedMode::Remembered(SessionModeChoice::Auto),
            "the auto on disk is untouched"
        );
    }

    /// D3: a pre-v1 file remembering bypass loads as auto, with a note explaining why, and the file
    /// on disk is never touched (no rewrite, no `.unusable`).
    #[test]
    fn a_remembered_bypass_starts_in_auto_with_a_note_and_the_file_is_untouched() {
        let dir = scratch("remembered-bypass");
        // Written directly, the way `another_projects_file_or_a_broken_one_is_unusable...` writes a
        // "foreign" file: `save_mode` itself refuses to write `Bypass`.
        let path = dir.join(crate::layout::persist::file_name(&root()));
        let raw = format!(
            "{{\"version\":1,\"project_root\":{:?},\"permission_mode\":\"bypass\"}}",
            root().to_string_lossy()
        );
        std::fs::write(&path, &raw).unwrap();
        assert_eq!(
            load_mode(&dir, &root()),
            LoadedMode::Remembered(SessionModeChoice::Bypass),
            "the premise: the file really does say bypass"
        );

        let before = std::fs::read(&path).unwrap();
        let (mode, notes) = startup_mode(Some(&dir), &root());
        assert_eq!(mode, SessionModeChoice::Auto);
        assert_eq!(
            notes,
            vec![
                "[agent] this project remembered bypass; every launch starts in auto since v1 \
                 (R07/S2) -- Shift+Tab switches, and asks first"
                    .to_string()
            ]
        );
        let after = std::fs::read(&path).unwrap();
        assert_eq!(before, after, "the file is left exactly as it was");
        assert!(
            !dir.join(format!("{}.unusable", crate::layout::persist::file_name(&root())))
                .exists(),
            "a remembered bypass is not unusable -- nothing is set aside"
        );
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

    #[test]
    fn the_default_mode_key_reads_its_two_words_and_nothing_else() {
        assert_eq!(parse_default_mode(None), Ok(None));
        assert_eq!(parse_default_mode(Some("auto")), Ok(Some(SessionModeChoice::Auto)));
        assert_eq!(
            parse_default_mode(Some(" bypass ")),
            Ok(Some(SessionModeChoice::Bypass))
        );
        for bad in ["", "Bypass", "yes", "plan", "bypassPermissions"] {
            let err = parse_default_mode(Some(bad)).unwrap_err();
            assert!(
                err.contains("agent.default_mode") && err.contains(&format!("{bad:?}")),
                "{err}"
            );
        }
    }

    #[test]
    fn the_user_settings_key_reads_true_and_false_and_nothing_else() {
        assert_eq!(parse_user_settings(None), Ok(true));
        assert_eq!(parse_user_settings(Some("true")), Ok(true));
        assert_eq!(parse_user_settings(Some(" false ")), Ok(false));
        for bad in ["", "False", "TRUE", "yes", "0", "1", "off", "nil"] {
            let err = parse_user_settings(Some(bad)).unwrap_err();
            assert!(
                err.contains("agent.user_settings") && err.contains(&format!("{bad:?}")),
                "{err}"
            );
        }
    }

    /// The configured default yields to a mode Shift+Tab remembered, and only to that.
    #[test]
    fn only_an_auto_the_user_left_bypass_for_counts_as_a_remembered_choice() {
        let dir = scratch("remembers");
        assert!(!remembers_a_choice(None, &root()));
        assert!(!remembers_a_choice(Some(&dir), &root()), "nothing written yet");
        save_mode(&dir, &root(), SessionModeChoice::Auto).unwrap();
        assert!(remembers_a_choice(Some(&dir), &root()));

        let path = dir.join(crate::layout::persist::file_name(&root()));
        let raw = format!(
            "{{\"version\":1,\"project_root\":{:?},\"permission_mode\":\"bypass\"}}",
            root().to_string_lossy()
        );
        std::fs::write(&path, raw).unwrap();
        assert!(
            !remembers_a_choice(Some(&dir), &root()),
            "a pre-v1 bypass is not a choice"
        );
        std::fs::write(&path, "{ not json").unwrap();
        assert!(!remembers_a_choice(Some(&dir), &root()));
        assert!(path.exists(), "asking never moves the file");
    }
}
