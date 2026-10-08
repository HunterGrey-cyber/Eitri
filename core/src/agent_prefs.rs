//! The permission mode an empty session tab starts in, remembered per project in the state
//! directory under `agent/<16 hex>.json`, outside the project. The file is named like the layout
//! file (`layout::persist::file_name`) and written the same way: a `.tmp` beside it, then renamed.
//! A file that cannot be used is set aside as `.json.unusable` and read as nothing, as the layout
//! file is.

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

/// Refuses bypass with `InvalidInput` in every build, before writing anything. A
/// `debug_assert!` would disappear in release builds. Both callers log save failures and
/// continue.
pub fn save_mode(dir: &Path, project_root: &Path, mode: SessionModeChoice) -> std::io::Result<PathBuf> {
    if mode == SessionModeChoice::Bypass {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "bypass is never written to disk",
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

/// The `init.lua` key: `eitri.config.set("review.enabled", true | false)`.
pub const REVIEW_ENABLED_KEY: &str = "review.enabled";

/// The `init.lua` key: `eitri.config.set("review.hint", true | false)`.
pub const REVIEW_HINT_KEY: &str = "review.hint";

/// A true/false switch as `init.lua` left it: unset is `default`, else exactly `true` or `false`
/// (the store holds text, so a Lua boolean arrives as one of those words). Anything else is an
/// error naming the key.
fn parse_switch(key: &str, value: Option<&str>, default: bool) -> Result<bool, String> {
    match value.map(str::trim) {
        None => Ok(default),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(_) => Err(format!(
            "eitri.config.set(\"{key}\", {:?}): must be true or false",
            value.unwrap_or_default()
        )),
    }
}

/// [`REVIEW_ENABLED_KEY`]: whether turn review runs at all. Unset means it does.
pub fn parse_review_enabled(value: Option<&str>) -> Result<bool, String> {
    parse_switch(REVIEW_ENABLED_KEY, value, true)
}

/// [`REVIEW_HINT_KEY`]: whether a finished turn that edited files puts a hint in the status band.
/// Unset means it stays quiet; the review is still one key away.
pub fn parse_review_hint(value: Option<&str>) -> Result<bool, String> {
    parse_switch(REVIEW_HINT_KEY, value, false)
}

/// The `init.lua` key: `eitri.config.set("agent.default", "claude" | "codex")`.
pub const AGENT_DEFAULT_KEY: &str = "agent.default";

/// The `init.lua` key: `eitri.config.set("agent.codex.command", "<path or command name>")`.
pub const CODEX_COMMAND_KEY: &str = "agent.codex.command";

/// The agent a new tab runs when the user names none (the new-tab key, an empty window's first tab).
/// Unset is Claude, as ever. Anything but a name this build can start is an error naming the key,
/// like every other configuration key here; the names of agents registered from `init.lua` join the
/// accepted ones when such a registry exists.
pub fn parse_agent_default(value: Option<&str>) -> Result<crate::agent_backend::AgentId, String> {
    use crate::agent_backend::AgentId;
    match value.map(str::trim) {
        None | Some("claude") => Ok(AgentId::Claude),
        Some("codex") => Ok(AgentId::Codex),
        Some(_) => Err(format!(
            "eitri.config.set(\"{AGENT_DEFAULT_KEY}\", {:?}): must be \"claude\" or \"codex\"",
            value.unwrap_or_default()
        )),
    }
}

/// The agent a word typed at the panel's `:tabnew` names, or `None` for a word that names none.
pub fn agent_named(word: &str) -> Option<crate::agent_backend::AgentId> {
    parse_agent_default(Some(word)).ok()
}

/// [`CODEX_COMMAND_KEY`]: the `codex` to run, as an absolute path or a bare command name looked up on
/// `PATH`. Unset or empty says nothing (the binary is found on `PATH`). A relative path with a
/// directory part is refused: Codex runs from the project directory, where it would name a file
/// the project supplied. That the file exists is not checked here; a missing binary fails the Codex
/// tab's start, never the window.
pub fn parse_codex_command(value: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let path = Path::new(raw);
    let mut parts = path.components();
    let bare = matches!(
        (parts.next(), parts.next()),
        (Some(std::path::Component::Normal(_)), None)
    );
    if path.is_absolute() || bare {
        Ok(Some(raw.to_string()))
    } else {
        Err(format!(
            "eitri.config.set(\"{CODEX_COMMAND_KEY}\", {raw:?}): must be an absolute path or a bare command name"
        ))
    }
}

/// `EITRI_CODEX`, checked when the window starts: a value that is set must be an absolute path to a
/// file, else the window fails naming the variable, as a bad `init.lua` key does. Unset or empty
/// says nothing; a `codex` missing from `PATH` (or from `agent.codex.command`) fails only a Codex
/// tab's start. The rule is the one the tab's start applies (`decide_codex`), asked of the variable
/// alone.
pub fn check_codex_env(value: Option<&OsStr>, is_file: impl Fn(&Path) -> bool) -> Result<(), String> {
    use agent::providers::codex::{decide_codex, LocateError};
    match decide_codex(value, None, None, is_file) {
        Err(error @ (LocateError::EnvNotAbsolute(_) | LocateError::EnvNotAFile(_))) => Err(error.to_string()),
        _ => Ok(()),
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
        // A remembered bypass starts in auto. Leave the saved file untouched and return a
        // note explaining the mode change.
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
    fn agent_default_accepts_claude_codex_and_refuses_others_naming_the_key() {
        use crate::agent_backend::AgentId;
        assert_eq!(parse_agent_default(None), Ok(AgentId::Claude));
        assert_eq!(parse_agent_default(Some("claude")), Ok(AgentId::Claude));
        assert_eq!(parse_agent_default(Some(" codex ")), Ok(AgentId::Codex));
        for bad in ["", "Codex", "gemini", "acp:x", "claude codex"] {
            let err = parse_agent_default(Some(bad)).unwrap_err();
            assert!(err.contains("agent.default"), "{bad}: {err}");
            assert!(err.contains(&format!("{bad:?}")), "{bad}: {err}");
        }
        assert_eq!(agent_named("codex"), Some(AgentId::Codex));
        assert_eq!(agent_named("claude"), Some(AgentId::Claude));
        assert_eq!(agent_named(""), None);
        assert_eq!(agent_named("gemini"), None);
    }

    #[test]
    fn codex_command_absolute_or_bare_name() {
        assert_eq!(parse_codex_command(None), Ok(None));
        assert_eq!(parse_codex_command(Some("  ")), Ok(None));
        assert_eq!(parse_codex_command(Some("codex")), Ok(Some("codex".into())));
        assert_eq!(
            parse_codex_command(Some("/opt/codex/bin/codex")),
            Ok(Some("/opt/codex/bin/codex".into()))
        );
        for bad in ["bin/codex", "./codex", "../codex", "..", "."] {
            let err = parse_codex_command(Some(bad)).unwrap_err();
            assert!(err.contains("agent.codex.command"), "{bad}: {err}");
            assert!(err.contains(&format!("{bad:?}")), "{bad}: {err}");
        }
    }

    /// `EITRI_CODEX` set to anything but an absolute path to a file is a startup failure naming the
    /// variable; unset or empty says nothing, and whether a `codex` is on `PATH` is not asked here.
    #[test]
    fn a_bad_eitri_codex_is_a_startup_failure_naming_it() {
        let files = |p: &Path| p == Path::new("/opt/codex");
        assert_eq!(check_codex_env(None, files), Ok(()));
        assert_eq!(check_codex_env(Some(OsStr::new("")), files), Ok(()));
        assert_eq!(check_codex_env(Some(OsStr::new("/opt/codex")), files), Ok(()));
        for bad in ["codex", "bin/codex", "/opt/missing", "/opt"] {
            let err = check_codex_env(Some(OsStr::new(bad)), files).unwrap_err();
            assert!(err.contains("EITRI_CODEX"), "{bad}: {err}");
            assert!(err.contains(bad), "{bad}: {err}");
        }
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

    /// Saves and reloads `Auto`, the only mode `save_mode` writes. Bypass refusal is checked by
    /// `bypass_is_never_written_in_any_build`.
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

    /// Refusing bypass writes nothing in every build and leaves any saved `auto` value
    /// untouched.
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

    /// A file remembering bypass starts in auto with an explanatory note. The file is neither
    /// rewritten nor renamed to `.unusable`.
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

    #[test]
    fn review_switches_parse_like_user_settings() {
        assert_eq!(parse_review_enabled(None), Ok(true));
        assert_eq!(parse_review_hint(None), Ok(false));
        for (text, want) in [("true", true), ("false", false), (" false ", false)] {
            assert_eq!(parse_review_enabled(Some(text)), Ok(want), "{text:?}");
            assert_eq!(parse_review_hint(Some(text)), Ok(want), "{text:?}");
        }
        for bad in ["yes", "1", ""] {
            let err = parse_review_enabled(Some(bad)).unwrap_err();
            assert!(
                err.contains("review.enabled") && err.contains(&format!("{bad:?}")),
                "{err}"
            );
            let err = parse_review_hint(Some(bad)).unwrap_err();
            assert!(
                err.contains("review.hint") && err.contains(&format!("{bad:?}")),
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
