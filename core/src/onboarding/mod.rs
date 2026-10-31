//! The first-start screen, toolkit-free: the types its state machine, its generators and the controller
//! share. Nothing here draws or touches a window; the shell draws [`OnboardingView`] and carries out the
//! work a step asks for on worker threads. `eitri-mac` compiles this module, so every Linux-only call
//! elsewhere in it sits under `cfg(target_os = "linux")`.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::keymap::prefix::Press;
use crate::keymap::{Chord, KeyName, KeySpec, KeymapOp};

pub mod check;
pub mod detect;
pub mod display;
pub mod lua_text;
pub mod marker;
pub mod presets;
pub mod screen;
pub mod text;
pub mod trigger;
pub mod write;

/// The language of the screen's wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    ZhCn,
}

impl Lang {
    /// `LC_ALL`, then `LC_MESSAGES`, then `LANG`: the first one that is set and not empty decides, and
    /// `zh…` is Chinese; anything else, or nothing set, is English.
    pub fn from_env(env: &dyn Fn(&str) -> Option<OsString>) -> Lang {
        for name in ["LC_ALL", "LC_MESSAGES", "LANG"] {
            let Some(value) = env(name).filter(|v| !v.is_empty()) else {
                continue;
            };
            return if value.to_string_lossy().to_ascii_lowercase().starts_with("zh") {
                Lang::ZhCn
            } else {
                Lang::En
            };
        }
        Lang::En
    }
}

/// Today's date in the local zone, `YYYY-MM-DD`; the generated file's first comment carries it.
pub fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    date_of(secs)
}

fn date_of(secs: u64) -> String {
    let secs = libc::time_t::try_from(secs).unwrap_or(0);
    // SAFETY: `localtime_r` writes only into `tm`, which lives for the call; both pointers are valid and
    // unaliased.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let converted = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    if !converted {
        return "1970-01-01".to_string();
    }
    format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
}

/// What the screen offers, one row of its list each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetId {
    NvimTmux,
    NvimNoTmux,
    OtherPrefix,
    I3Style,
    Compositor,
}

/// What this binary can do, so the screen offers only that. Compositor mode does not exist on macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Build {
    pub compositor_mode: bool,
    pub nvim_prefix: bool,
}

impl Build {
    pub const THIS_BINARY: Build = Build {
        compositor_mode: cfg!(not(target_os = "macos")),
        nvim_prefix: true,
    };
}

/// Who tiles the windows, as the screen's `windows` row and `w` key settle it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tiling {
    Unasked,
    Eitri,
    Compositor,
}

/// The key the user's i3 or sway calls `$mod`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WmMod {
    Alt,
    Super,
}

/// What the user chose, and what the choice was made against; the generators build the file from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub preset: PresetId,
    /// A prefix the user chose when it differs from what the preset gives.
    pub prefix: Option<KeySpec>,
    pub wm_mod: WmMod,
    pub tiling: Tiling,
    /// One of `"Hyprland"`, `"sway"`, `"niri"`, `"i3"` (`detect::wm_name`).
    pub wm_name: Option<&'static str>,
    pub tmux_prefix: Option<KeySpec>,
    pub tmux_config_found: bool,
    pub running_prefix: KeySpec,
    pub nvim_lhs: Option<&'static str>,
    /// The canonical action name the chosen key has in the table the file starts from
    /// (`presets::start_table`).
    pub bound_as: Option<String>,
    pub press_failed: bool,
    pub date: String,
}

/// A generated file: its lines, the ops those lines make when run, and the config settings it sets.
#[derive(Debug, Clone, PartialEq)]
pub struct Generated {
    pub lines: Vec<String>,
    pub ops: Vec<KeymapOp>,
    pub settings: Vec<(String, String)>,
}

impl Generated {
    /// The lines joined, with one trailing newline.
    pub fn text(&self) -> String {
        let mut text = self.lines.join("\n");
        text.push('\n');
        text
    }
}

/// How writing `init.lua` ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    Written(PathBuf),
    Failed(String),
    NotWritten(String),
}

/// What detection found, one answer per row of the screen, arriving from a worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    Tmux(TmuxRow),
    Desktop(DesktopRow),
    Windows(Option<Tiling>),
    Neovim(NeovimRow),
    Input(InputRow),
    TrustOpen(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxRow {
    pub path: Option<String>,
    pub prefix: Option<KeySpec>,
    pub changes_keys: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopRow {
    pub wm: Option<&'static str>,
    pub live: bool,
    pub wm_mod: Option<WmMod>,
    pub note: Option<String>,
    pub binds: Vec<Chord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeovimRow {
    /// The row's lines on the first page, for the prefix this window runs with.
    pub rows: Vec<String>,
    pub nvim_lhs: Option<&'static str>,
    /// What each chord does in the editor, as a line: the review names the one the file would make the prefix.
    pub chord_lines: Vec<(KeySpec, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputRow {
    /// The input method the environment or the session bus names.
    pub im: Option<&'static str>,
    /// None is named, but the locale is one that usually has one.
    pub maybe: bool,
}

/// Why the screen was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// A first start with no `init.lua`.
    First,
    /// `:Eitri setup` or `window.setup`, on a start that would not have opened it.
    Asked,
    /// Asked for, but nothing may be written: the screen only shows.
    ShowOnly,
}

/// Why a [`OpenMode::ShowOnly`] screen cannot write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShowWhy {
    HasInit,
    RefusedDir(String),
}

/// Everything the controller hands the screen when it opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opening {
    pub generation: u64,
    pub mode: OpenMode,
    pub show_why: Option<ShowWhy>,
    /// Whether a trust question already holds the keys; read on the GTK thread when the event is built.
    pub trust_open: bool,
    pub lang: Lang,
    pub build: Build,
    pub running_prefix: KeySpec,
    /// The running table's prefix bindings as (key, canonical action name).
    pub keymap_check: Vec<(KeySpec, String)>,
    pub today: String,
    pub config_path: String,
}

/// How far a key moves the screen's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scroll {
    LineUp,
    LineDown,
    PageUp,
    PageDown,
}

/// What sets the parts of a footer apart.
pub(crate) const FOOTER_SEPARATOR: &str = " · ";

/// What the shell draws: plain text only, every field already set in the screen's language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnboardingView {
    pub title: String,
    pub step: String,
    pub rows: Vec<ViewRow>,
    pub question: Option<String>,
    pub items: Vec<ViewItem>,
    pub body: Vec<String>,
    pub status: Vec<String>,
    pub footer: String,
    pub buttons: Vec<ViewButton>,
    pub help: Option<String>,
}

impl OnboardingView {
    /// The footer's words that no button says (`j/k or 1-4 choose`): the rest of the footer is the buttons, so a
    /// screen drawing both shows each part once.
    pub fn footer_hint(&self) -> String {
        let parts: Vec<&str> = self
            .footer
            .split(FOOTER_SEPARATOR)
            .filter(|part| !self.buttons.iter().any(|button| button.label == *part))
            .collect();
        parts.join(FOOTER_SEPARATOR)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewRow {
    pub label: String,
    pub text: String,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewItem {
    pub number: u32,
    pub name: String,
    pub summary: String,
    pub tag: Option<String>,
    pub selected: bool,
}

/// A key the screen answers, as a button stands for it: a character or one of the named keys the footer lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewKey {
    Char(char),
    Enter,
    Esc,
    /// Backspace.
    Back,
}

impl ViewKey {
    /// The press a click on the button stands for: the same one the key makes, so the screen has one input.
    pub fn press(self) -> (Press, Option<KeyName>) {
        let named = |key| (Press::Key(KeySpec::named(key, false, false, false)), Some(key));
        match self {
            ViewKey::Char(c) => (Press::Key(KeySpec::char(c, false, false)), Some(KeyName::Char(c))),
            ViewKey::Enter => named(KeyName::Enter),
            // Escape has no name of its own: the window's key translation leaves it out too.
            ViewKey::Esc => (Press::Escape, None),
            ViewKey::Back => named(KeyName::BSpace),
        }
    }
}

impl PartialEq<char> for ViewKey {
    fn eq(&self, other: &char) -> bool {
        *self == ViewKey::Char(*other)
    }
}

impl std::fmt::Display for ViewKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ViewKey::Char(c) => write!(f, "{c}"),
            ViewKey::Enter => f.write_str("Enter"),
            ViewKey::Esc => f.write_str("Esc"),
            ViewKey::Back => f.write_str("Backspace"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewButton {
    pub key: ViewKey,
    pub label: String,
}

impl ViewButton {
    pub fn new(key: ViewKey, label: &str) -> ViewButton {
        ViewButton {
            key,
            label: label.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| OsString::from(*v))
    }

    #[test]
    fn lc_all_beats_lang() {
        let pairs = [("LC_ALL", "zh_CN.UTF-8"), ("LANG", "en_US.UTF-8")];
        assert_eq!(Lang::from_env(&env(&pairs)), Lang::ZhCn);
        let pairs = [("LC_ALL", "en_US.UTF-8"), ("LANG", "zh_CN.UTF-8")];
        assert_eq!(Lang::from_env(&env(&pairs)), Lang::En);
    }

    #[test]
    fn an_empty_variable_is_skipped() {
        let pairs = [("LC_ALL", ""), ("LC_MESSAGES", ""), ("LANG", "zh_CN.UTF-8")];
        assert_eq!(Lang::from_env(&env(&pairs)), Lang::ZhCn);
        let pairs = [("LC_ALL", ""), ("LC_MESSAGES", "zh_TW"), ("LANG", "en_US")];
        assert_eq!(Lang::from_env(&env(&pairs)), Lang::ZhCn);
    }

    #[test]
    fn nothing_set_is_english() {
        assert_eq!(Lang::from_env(&env(&[])), Lang::En);
        assert_eq!(Lang::from_env(&env(&[("LANG", "C")])), Lang::En);
    }

    #[test]
    fn today_is_a_date() {
        let day = today();
        let parts: Vec<&str> = day.split('-').collect();
        assert_eq!(parts.len(), 3, "{day}");
        assert_eq!(parts[0].len(), 4);
        assert!(parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())), "{day}");
        assert!(date_of(0).starts_with("19"), "{}", date_of(0));
    }

    #[test]
    fn generated_text_ends_in_one_newline() {
        let generated = Generated {
            lines: vec!["a".into(), "b".into()],
            ops: Vec::new(),
            settings: Vec::new(),
        };
        assert_eq!(generated.text(), "a\nb\n");
    }

    #[test]
    fn this_binary_has_compositor_mode_except_on_macos() {
        let build = Build::THIS_BINARY;
        assert_eq!(
            build,
            Build {
                compositor_mode: cfg!(not(target_os = "macos")),
                nvim_prefix: true
            }
        );
    }
}
