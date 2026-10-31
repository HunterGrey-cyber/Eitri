//! What the screen reads from the machine: tmux, the desktop, nvim, the input method.
//!
//! Everything here is pure: the caller gathers the text (a config file, an environment, a compositor's
//! answer) on a worker and hands it in, and what comes out is one row of the screen. A row keeps the raw
//! text it was given; the screen cuts and escapes untrusted values (`display::display_text`) when it sets
//! its labels, so approved wording is never cut.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::text::{fill, text, TextId};
use super::{Build, DesktopRow, InputRow, Lang, NeovimRow, PresetId, TmuxRow, WmMod};
use crate::compositor::{self, Wm};
use crate::keymap::root_conflicts::Im;
use crate::keymap::{Chord, KeySpec, TmuxImport};
use crate::nvim_keys::findings::{Certainty, Mode, NavOwner, NvimFindings, Owner};

/// A config file bigger than this is not read.
pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;
/// How deep `include`s are followed below the file the window manager loads itself.
pub const MAX_INCLUDE_DEPTH: usize = 4;
/// Files read at most for one config, the first included.
const MAX_CONFIG_FILES: usize = 16;
/// Text read at most for one config, over every file.
const MAX_CONFIG_TOTAL_BYTES: usize = 4 * MAX_CONFIG_BYTES;

/// The `DesktopRow` note that says the Hyprland config is Lua code and was not read. It is the id of the
/// screen's wording for it, not text to show.
pub const NOTE_HYPR_LUA: &str = "desktop.hypr_lua";

/// The tiling window manager this window runs under, as a constant name: `Wm` has no i3, so `i3` is named
/// here from `I3SOCK` when nothing else answers. GNOME is no tiling window manager; `desktop_row` names it.
pub fn wm_name(env: &dyn Fn(&str) -> Option<OsString>) -> Option<&'static str> {
    match compositor::detect(env) {
        Wm::Hyprland => Some("Hyprland"),
        Wm::Sway => Some("sway"),
        Wm::Niri => Some("niri"),
        Wm::Gnome => None,
        Wm::None => env("I3SOCK").filter(|v| !v.is_empty()).map(|_| "i3"),
    }
}

fn mod_of(word: &str) -> Option<WmMod> {
    if word.eq_ignore_ascii_case("mod1") {
        Some(WmMod::Alt)
    } else if word.eq_ignore_ascii_case("mod4") {
        Some(WmMod::Super)
    } else {
        None
    }
}

/// The key sway or i3 calls `$mod`: the first `set $mod Mod1|Mod4`; with no such line, the one modifier
/// that `bindsym Mod1+…` or `bindsym Mod4+…` rows use (both at once settle nothing).
pub fn sway_mod(config_text: &str) -> Option<WmMod> {
    let (mut alt_rows, mut super_rows) = (0usize, 0usize);
    for line in config_text.lines() {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("set") if words.next() == Some("$mod") => {
                if let Some(found) = words.next().and_then(mod_of) {
                    return Some(found);
                }
            }
            Some("bindsym") => {
                let Some(keys) = words.find(|w| !w.starts_with("--")) else {
                    continue;
                };
                match keys.split('+').next().and_then(mod_of) {
                    Some(WmMod::Alt) => alt_rows += 1,
                    Some(WmMod::Super) => super_rows += 1,
                    None => {}
                }
            }
            _ => {}
        }
    }
    match (alt_rows > 0, super_rows > 0) {
        (true, false) => Some(WmMod::Alt),
        (false, true) => Some(WmMod::Super),
        _ => None,
    }
}

/// The three files sway or i3 reads, in the order it looks (`wm` is `"sway"` or `"i3"`; any other name has
/// none). `XDG_CONFIG_HOME` counts when it is an absolute path, else `~/.config`.
pub fn wm_config_paths(wm: &str, env: &dyn Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let dotted = match wm {
        "sway" => ".sway",
        "i3" => ".i3",
        _ => return Vec::new(),
    };
    let home = env("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    let config_home = env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.as_ref().map(|h| h.join(".config")));
    let mut paths = Vec::new();
    if let Some(dir) = config_home {
        paths.push(dir.join(wm).join("config"));
    }
    if let Some(home) = home {
        paths.push(home.join(dotted).join("config"));
    }
    paths.push(PathBuf::from("/etc").join(wm).join("config"));
    paths
}

/// The absolute paths named by `include` lines. A relative one, or one with a variable, is not followed: what
/// it would resolve against is the window manager's own business.
pub fn includes(config_text: &str) -> Vec<PathBuf> {
    config_text
        .lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("include")?;
            if !rest.starts_with(char::is_whitespace) {
                return None;
            }
            let path = rest.trim();
            (path.starts_with('/') && !path.contains('$')).then(|| PathBuf::from(path))
        })
        .collect()
}

/// `path`'s text with each `include` replaced, in place, by the included file's text: followed to
/// [`MAX_INCLUDE_DEPTH`], each file read once (so a loop ends), at most [`MAX_CONFIG_BYTES`] per file and
/// [`MAX_CONFIG_FILES`] files. `read` returns a regular file's text or `None`; a file over the limit counts as
/// not read. `None` when `path` itself is not read.
pub fn read_config(path: &Path, read: &dyn Fn(&Path) -> Option<String>) -> Option<String> {
    let mut seen = HashSet::new();
    let mut total = 0usize;
    expand(path, read, 0, &mut seen, &mut total)
}

fn expand(
    path: &Path,
    read: &dyn Fn(&Path) -> Option<String>,
    depth: usize,
    seen: &mut HashSet<PathBuf>,
    total: &mut usize,
) -> Option<String> {
    if seen.len() >= MAX_CONFIG_FILES || !seen.insert(path.to_path_buf()) {
        return None;
    }
    let text = read(path).filter(|t| t.len() <= MAX_CONFIG_BYTES)?;
    *total += text.len();
    if *total > MAX_CONFIG_TOTAL_BYTES {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let followed = if depth < MAX_INCLUDE_DEPTH {
            includes(line)
        } else {
            Vec::new()
        };
        if followed.is_empty() {
            out.push_str(line);
            out.push('\n');
        }
        for next in followed {
            if let Some(inner) = expand(&next, read, depth + 1, seen, total) {
                out.push_str(&inner);
            }
        }
    }
    Some(out)
}

/// `$mainMod = SUPER|ALT` from `hyprland.conf` (the first such line; `WIN`, `LOGO`, `MOD4` and `MOD1` are
/// Hyprland's other spellings of the two).
pub fn hypr_main_mod(config_text: &str) -> Option<WmMod> {
    config_text.lines().find_map(|line| {
        let rest = line
            .trim_start()
            .strip_prefix("$mainMod")?
            .trim_start()
            .strip_prefix('=')?;
        let value = rest.split('#').next().unwrap_or("").trim();
        match value.to_ascii_uppercase().as_str() {
            "SUPER" | "WIN" | "LOGO" | "MOD4" => Some(WmMod::Super),
            "ALT" | "MOD1" => Some(WmMod::Alt),
            _ => None,
        }
    })
}

/// A path as the user would type it: under the home directory, `~/…`.
fn tilde(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// The tmux row from the import this window already ran. The files read are named `and`-joined, home as `~/`;
/// `changes_keys` is whether the import changed any key from the defaults.
pub fn tmux_row(import: Option<&TmuxImport>, home: Option<&Path>) -> TmuxRow {
    let Some(import) = import else {
        return TmuxRow {
            path: None,
            prefix: None,
            changes_keys: false,
        };
    };
    let path = (!import.files.is_empty()).then(|| {
        import
            .files
            .iter()
            .map(|f| tilde(f, home))
            .collect::<Vec<_>>()
            .join(" and ")
    });
    TmuxRow {
        path,
        prefix: import.prefix,
        changes_keys: import.changes_anything(),
    }
}

/// The desktop row. The note is [`NOTE_HYPR_LUA`] for a Hyprland whose configuration is Lua and whose live
/// binds were not read; no compositor has no name here. i3 has no `Wm`: the caller sets `wm: Some("i3")` from
/// [`wm_name`].
pub fn desktop_row(wm: Wm, live: bool, wm_mod: Option<WmMod>, binds: Vec<Chord>, lua_config: bool) -> DesktopRow {
    let name = match wm {
        Wm::Hyprland => Some("Hyprland"),
        Wm::Sway => Some("sway"),
        Wm::Niri => Some("niri"),
        Wm::Gnome => Some("GNOME"),
        Wm::None => None,
    };
    let lua_unread = wm == Wm::Hyprland && lua_config && binds.is_empty();
    DesktopRow {
        wm: name,
        live,
        wm_mod,
        note: lua_unread.then(|| NOTE_HYPR_LUA.to_string()),
        binds,
    }
}

/// `zh`, `ja` or `ko` as the language of a locale name (`zh_CN.UTF-8`, `ja`, `ko_KR`), not `kok_IN`.
fn cjk_locale(locale: &str) -> bool {
    let lower = locale.to_ascii_lowercase();
    ["zh", "ja", "ko"].iter().any(|lang| {
        lower
            .strip_prefix(lang)
            .is_some_and(|rest| !rest.starts_with(|c: char| c.is_ascii_alphabetic()))
    })
}

/// The input row: the input methods the environment or the bus shows, else a CJK locale alone says one may
/// be there. The trigger key of an input method is never read.
pub fn input_row(ims: &[Im], locale: &str) -> InputRow {
    let im = if ims.contains(&Im::Fcitx) {
        Some("fcitx5")
    } else if ims.contains(&Im::Ibus) {
        Some("ibus")
    } else {
        None
    };
    InputRow {
        im,
        maybe: im.is_none() && cjk_locale(locale),
    }
}

fn mode_word(mode: Mode, lang: Lang) -> &'static str {
    match (mode, lang) {
        (Mode::Normal, Lang::En) => "Normal",
        (Mode::Visual, Lang::En) => "Visual",
        (Mode::Insert, Lang::En) => "Insert",
        (Mode::Terminal, Lang::En) => "Terminal",
        (Mode::Normal, Lang::ZhCn) => "普通",
        (Mode::Visual, Lang::ZhCn) => "可视",
        (Mode::Insert, Lang::ZhCn) => "插入",
        (Mode::Terminal, Lang::ZhCn) => "终端",
    }
}

/// Who an owner is, in words; the text of `Origin` and `Desc` passes `clean`.
fn owner_words(owner: &Owner, lang: Lang, clean: &dyn Fn(&str) -> String) -> String {
    match (owner, lang) {
        (Owner::NvimDefault, Lang::En) => "Neovim's default".to_string(),
        (Owner::NvimDefault, Lang::ZhCn) => "Neovim 默认映射".to_string(),
        (Owner::Eitri, Lang::En) => "Eitri's".to_string(),
        (Owner::Eitri, Lang::ZhCn) => "Eitri 的".to_string(),
        (Owner::Plugin(plugin), _) => plugin.name().to_string(),
        (Owner::Origin(text) | Owner::Desc(text), _) => clean(text),
        (Owner::LuaFunction, Lang::En) => "a Lua function".to_string(),
        (Owner::LuaFunction, Lang::ZhCn) => "Lua 函数".to_string(),
    }
}

/// [`neovim_row_with`] with the owner text as the probe gave it, for a window on the stock prefix. The screen sets
/// its labels with `neovim_row_with` and `display::display_text`.
pub fn neovim_row(findings: &NvimFindings, lang: Lang) -> NeovimRow {
    let stock = KeySpec::parse(crate::keymap::stock::STOCK_PREFIX).expect("the stock prefix parses");
    neovim_row_with(findings, lang, &|text| text.to_string(), &stock)
}

/// The Neovim row of a window on `prefix`: at most three lines, the navigator's, who else has `prefix` in the editor
/// (Eitri's own and Neovim's defaults say nothing), the leader; a line that has to go goes from the end. "Press it
/// twice" is only true of the prefix, so no other chord has a line on this row; `chord_lines` keeps them all for the
/// review, which names the prefix the file would set. `clean` is applied to every piece of text the probe supplied (a
/// mapping's origin or description, the leader) before it fills a template. `nvim_lhs` is left to the caller, which
/// knows the report's leader token.
pub fn neovim_row_with(
    findings: &NvimFindings,
    lang: Lang,
    clean: &dyn Fn(&str) -> String,
    prefix: &KeySpec,
) -> NeovimRow {
    let held = clean(&prefix.human());
    let nav_line = |id: TextId, who: &str| fill(text(lang, id), &[("who", who), ("key", &held)]);
    let mut rows = Vec::new();
    match &findings.nav {
        NavOwner::EitriFallback => rows.push(nav_line(TextId::NvimNavOk, &owner_words(&Owner::Eitri, lang, clean))),
        NavOwner::VimTmuxNavigator => rows.push(nav_line(TextId::NvimNavOk, "vim-tmux-navigator")),
        NavOwner::Navigator => rows.push(nav_line(TextId::NvimNavOk, "Navigator.nvim")),
        NavOwner::SmartSplits => rows.push(nav_line(TextId::NvimNavEdge, "smart-splits.nvim")),
        // Left alone by the nav fallback, so the map keeps its keys inside the editor.
        NavOwner::UserMapping(owner) => {
            rows.push(nav_line(TextId::NvimNavYours, &owner_words(owner, lang, clean)));
        }
        NavOwner::LazyStub => {}
    }
    let chord_lines: Vec<(KeySpec, String)> = findings
        .chords
        .iter()
        .filter(|found| !matches!(found.owner, Owner::Eitri | Owner::NvimDefault))
        .map(|found| {
            let id = match found.certainty {
                Certainty::Observed => TextId::NvimChordCertain,
                Certainty::Probably => TextId::NvimChordProbably,
            };
            let line = fill(
                text(lang, id),
                &[
                    ("key", &clean(&found.chord.human())),
                    ("mode", mode_word(found.mode, lang)),
                    ("who", &owner_words(&found.owner, lang, clean)),
                ],
            );
            (found.chord, line)
        })
        .collect();
    rows.extend(
        chord_lines
            .iter()
            .filter(|(chord, _)| chord == prefix)
            .map(|(_, line)| line.clone()),
    );
    if !findings.leader.is_empty() {
        let token = findings.leader.trim_matches(['<', '>']);
        rows.push(fill(text(lang, TextId::NvimLeader), &[("leader", &clean(token))]));
    }
    rows.truncate(3);
    NeovimRow {
        rows,
        nvim_lhs: None,
        chord_lines,
    }
}

fn is_tiling_compositor(row: &DesktopRow) -> bool {
    row.live && matches!(row.wm, Some("Hyprland" | "sway" | "niri"))
}

/// The preset the screen starts on: 1 when a tmux configuration exists, else 2; 5 over those only in a build
/// with compositor mode, under a live tiling compositor.
pub fn preselect(row: &TmuxRow, desktop: Option<&DesktopRow>, build: Build) -> PresetId {
    if build.compositor_mode && desktop.is_some_and(is_tiling_compositor) {
        PresetId::Compositor
    } else if row.path.is_some() {
        PresetId::NvimTmux
    } else {
        PresetId::NvimNoTmux
    }
}

/// Whether `prefix` is a key the compositor's live binds already take.
pub fn prefix_is_a_compositor_bind(prefix: &KeySpec, desktop: &DesktopRow) -> bool {
    let wanted = Chord::of(prefix).folded();
    desktop.binds.iter().any(|bind| bind.folded() == wanted)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::keymap::root_conflicts::chord_of_parts;
    use crate::nvim_keys::findings::{ChordOwner, KnownPlugin};

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| OsString::from(*v))
    }

    fn key(text: &str) -> KeySpec {
        KeySpec::parse(text).unwrap()
    }

    #[test]
    fn sway_mod_reads_the_set_line() {
        assert_eq!(sway_mod("set $mod Mod4\n"), Some(WmMod::Super));
        assert_eq!(sway_mod("set $mod Mod1\n"), Some(WmMod::Alt));
        assert_eq!(sway_mod("\tset\t$mod\tMod1\n"), Some(WmMod::Alt));
        assert_eq!(sway_mod("  set $mod Mod4  \n"), Some(WmMod::Super));
    }

    #[test]
    fn sway_mod_ignores_comments_and_other_variables() {
        assert_eq!(sway_mod("# set $mod Mod1\n"), None);
        assert_eq!(sway_mod("# set $mod Mod1\nset $mod Mod4\n"), Some(WmMod::Super));
        assert_eq!(sway_mod("set $term foot\n"), None);
        assert_eq!(sway_mod("set $mod Mod3\n"), None);
    }

    #[test]
    fn sway_mod_without_a_set_line_reads_the_bindsym_rows() {
        assert_eq!(sway_mod("bindsym Mod1+Return exec foot\n"), Some(WmMod::Alt));
        assert_eq!(sway_mod("bindsym --release Mod4+Shift+q kill\n"), Some(WmMod::Super));
        assert_eq!(sway_mod("bindsym Mod1+a nop\nbindsym Mod4+b nop\n"), None);
        assert_eq!(sway_mod("bindsym $mod+Return exec foot\n"), None);
        assert_eq!(sway_mod(""), None);
    }

    #[test]
    fn the_set_line_beats_the_bindsym_rows() {
        assert_eq!(sway_mod("bindsym Mod1+a nop\nset $mod Mod4\n"), Some(WmMod::Super));
    }

    #[test]
    fn config_paths_for_sway_and_i3() {
        let e = env(&[("HOME", "/home/u"), ("XDG_CONFIG_HOME", "/xdg")]);
        assert_eq!(
            wm_config_paths("sway", &e),
            [
                PathBuf::from("/xdg/sway/config"),
                PathBuf::from("/home/u/.sway/config"),
                PathBuf::from("/etc/sway/config")
            ]
        );
        let e = env(&[("HOME", "/home/u")]);
        assert_eq!(
            wm_config_paths("i3", &e),
            [
                PathBuf::from("/home/u/.config/i3/config"),
                PathBuf::from("/home/u/.i3/config"),
                PathBuf::from("/etc/i3/config")
            ]
        );
        assert!(wm_config_paths("niri", &e).is_empty());
    }

    #[test]
    fn a_relative_config_home_is_not_used() {
        let e = env(&[("HOME", "/home/u"), ("XDG_CONFIG_HOME", "cfg")]);
        assert_eq!(
            wm_config_paths("sway", &e)[0],
            PathBuf::from("/home/u/.config/sway/config")
        );
        let nothing = env(&[]);
        assert_eq!(wm_config_paths("sway", &nothing), [PathBuf::from("/etc/sway/config")]);
    }

    #[test]
    fn wm_names_are_constants() {
        assert_eq!(wm_name(&env(&[("SWAYSOCK", "/run/s")])), Some("sway"));
        assert_eq!(wm_name(&env(&[("HYPRLAND_INSTANCE_SIGNATURE", "x")])), Some("Hyprland"));
        assert_eq!(wm_name(&env(&[("NIRI_SOCKET", "/run/n")])), Some("niri"));
        assert_eq!(wm_name(&env(&[("I3SOCK", "/run/i3")])), Some("i3"));
        assert_eq!(wm_name(&env(&[("I3SOCK", "")])), None);
        assert_eq!(
            wm_name(&env(&[("SWAYSOCK", "/run/s"), ("I3SOCK", "/run/i3")])),
            Some("sway")
        );
        assert_eq!(wm_name(&env(&[])), None);
    }

    #[test]
    fn includes_keeps_absolute_paths_only() {
        let text = "include /etc/sway/config.d/*\n  include\t/home/u/extra\ninclude relative.conf\ninclude $HOME/x\nincludes /nope\nset a b\n";
        assert_eq!(
            includes(text),
            [PathBuf::from("/etc/sway/config.d/*"), PathBuf::from("/home/u/extra")]
        );
    }

    #[test]
    fn relative_includes_are_not_followed() {
        let files: HashMap<&str, String> = HashMap::from([
            ("/c", "include rel\nset $mod Mod1\n".to_string()),
            ("rel", "set $mod Mod4\n".to_string()),
        ]);
        let read = |p: &Path| files.get(p.to_str().unwrap()).cloned();
        let text = read_config(Path::new("/c"), &read).unwrap();
        assert_eq!(sway_mod(&text), Some(WmMod::Alt));
        assert!(!text.contains("Mod4"));
    }

    #[test]
    fn an_include_replaces_its_line_in_place() {
        let files: HashMap<&str, String> =
            HashMap::from([("/c", "a\ninclude /d\nz\n".to_string()), ("/d", "b\n".to_string())]);
        let read = |p: &Path| files.get(p.to_str().unwrap()).cloned();
        assert_eq!(read_config(Path::new("/c"), &read).unwrap(), "a\nb\nz\n");
    }

    #[test]
    fn an_include_loop_ends() {
        let files: HashMap<&str, String> = HashMap::from([
            ("/a", "set $mod Mod4\ninclude /b\n".to_string()),
            ("/b", "include /a\ninclude /b\nx\n".to_string()),
        ]);
        let reads = std::cell::Cell::new(0);
        let read = |p: &Path| {
            reads.set(reads.get() + 1);
            files.get(p.to_str().unwrap()).cloned()
        };
        let text = read_config(Path::new("/a"), &read).unwrap();
        assert_eq!(text, "set $mod Mod4\nx\n");
        assert_eq!(reads.get(), 2);
    }

    #[test]
    fn includes_stop_at_depth_four() {
        let files: HashMap<String, String> = (0..7)
            .map(|n| (format!("/f{n}"), format!("line{n}\ninclude /f{}\n", n + 1)))
            .collect();
        let read = |p: &Path| files.get(p.to_str().unwrap()).cloned();
        let text = read_config(Path::new("/f0"), &read).unwrap();
        assert!(text.contains("line4"), "{text}");
        assert!(!text.contains("line5"), "{text}");
    }

    #[test]
    fn a_file_over_a_mebibyte_is_not_read() {
        let files: HashMap<&str, String> = HashMap::from([
            ("/big", "x".repeat(MAX_CONFIG_BYTES + 1)),
            ("/exact", "y".repeat(MAX_CONFIG_BYTES)),
            ("/c", "include /big\nset $mod Mod1\n".to_string()),
        ]);
        let read = |p: &Path| files.get(p.to_str().unwrap()).cloned();
        assert_eq!(read_config(Path::new("/big"), &read), None);
        assert_eq!(
            read_config(Path::new("/exact"), &read).map(|t| t.len()),
            Some(MAX_CONFIG_BYTES + 1)
        );
        assert_eq!(read_config(Path::new("/c"), &read).as_deref(), Some("set $mod Mod1\n"));
        assert_eq!(read_config(Path::new("/missing"), &read), None);
    }

    #[test]
    fn a_config_that_includes_many_files_is_bounded() {
        let body: String = (0..40).map(|n| format!("include /f{n}\n")).collect();
        let reads = std::cell::Cell::new(0);
        let read = |_: &Path| {
            reads.set(reads.get() + 1);
            Some(body.clone())
        };
        assert!(read_config(Path::new("/root"), &read).is_some());
        assert!(reads.get() <= MAX_CONFIG_FILES, "{}", reads.get());
    }

    #[test]
    fn hypr_main_mod_reads_the_variable() {
        assert_eq!(hypr_main_mod("$mainMod = SUPER\n"), Some(WmMod::Super));
        assert_eq!(hypr_main_mod("$mainMod=ALT\n"), Some(WmMod::Alt));
        assert_eq!(hypr_main_mod("  $mainMod = super # the logo key\n"), Some(WmMod::Super));
        assert_eq!(hypr_main_mod("$mainMod = MOD1\n"), Some(WmMod::Alt));
    }

    #[test]
    fn hypr_main_mod_skips_what_it_does_not_know() {
        assert_eq!(hypr_main_mod("# $mainMod = ALT\n"), None);
        assert_eq!(hypr_main_mod("$mainMod = SUPER_SHIFT\n"), None);
        assert_eq!(hypr_main_mod("$mainModX = ALT\n"), None);
        assert_eq!(hypr_main_mod("$terminal = foot\n"), None);
        assert_eq!(hypr_main_mod("$mainMod = ALT\n$mainMod = SUPER\n"), Some(WmMod::Alt));
    }

    #[test]
    fn tmux_row_with_an_import() {
        let import = TmuxImport {
            files: vec![PathBuf::from("/home/u/.config/tmux/tmux.conf")],
            prefix: Some(key("C-a")),
            ..TmuxImport::default()
        };
        let row = tmux_row(Some(&import), Some(Path::new("/home/u")));
        assert_eq!(row.path.as_deref(), Some("~/.config/tmux/tmux.conf"));
        assert_eq!(row.prefix, Some(key("C-a")));
        assert!(row.changes_keys);
    }

    #[test]
    fn tmux_row_without_an_import() {
        let row = tmux_row(None, Some(Path::new("/home/u")));
        assert_eq!(
            row,
            TmuxRow {
                path: None,
                prefix: None,
                changes_keys: false
            }
        );
        let empty = TmuxImport::default();
        let row = tmux_row(Some(&empty), None);
        assert_eq!(row.path, None);
        assert!(!row.changes_keys);
    }

    #[test]
    fn tmux_row_that_changes_nothing() {
        let import = TmuxImport {
            files: vec![PathBuf::from("/etc/tmux.conf"), PathBuf::from("/home/u/.tmux.conf")],
            prefix: None,
            ..TmuxImport::default()
        };
        let row = tmux_row(Some(&import), Some(Path::new("/home/u")));
        assert_eq!(row.path.as_deref(), Some("/etc/tmux.conf and ~/.tmux.conf"));
        assert!(!row.changes_keys);
        let removes = TmuxImport {
            removed: vec![key("C-b")],
            ..import
        };
        assert!(tmux_row(Some(&removes), None).changes_keys);
    }

    #[test]
    fn input_row_names_the_running_method() {
        assert_eq!(input_row(&[Im::Fcitx], "en_US.UTF-8").im, Some("fcitx5"));
        assert_eq!(input_row(&[Im::Ibus], "").im, Some("ibus"));
        assert_eq!(input_row(&[Im::Fcitx, Im::Ibus], "").im, Some("fcitx5"));
    }

    #[test]
    fn input_row_reads_a_locale_alone() {
        for locale in ["zh_CN.UTF-8", "ja_JP", "ko"] {
            let row = input_row(&[], locale);
            assert_eq!((row.im, row.maybe), (None, true), "{locale}");
        }
        assert_eq!(input_row(&[], "kok_IN").im, None);
        assert_eq!(input_row(&[], "en_US.UTF-8").im, None);
        assert_eq!(input_row(&[], "").im, None);
        assert_eq!(input_row(&[Im::Ibus], "zh_CN").im, Some("ibus"));
    }

    fn findings() -> NvimFindings {
        NvimFindings {
            leader: "<Space>".to_string(),
            chords: Vec::new(),
            nav: NavOwner::VimTmuxNavigator,
            pending: Vec::new(),
            distro: None,
        }
    }

    fn owner(chord: &str, mode: Mode, owner: Owner, certainty: Certainty) -> ChordOwner {
        ChordOwner {
            chord: key(chord),
            mode,
            owner,
            certainty,
        }
    }

    #[test]
    fn neovim_row_words_a_probable_engine() {
        let mut f = findings();
        f.chords.push(owner(
            "C-b",
            Mode::Insert,
            Owner::Plugin(KnownPlugin::BlinkCmp),
            Certainty::Probably,
        ));
        let row = neovim_row(&f, Lang::En);
        assert_eq!(
            row.rows,
            [
                "Ctrl+h/j/k/l: vim-tmux-navigator (works as is)",
                "Ctrl+b in Insert: probably blink.cmp (press Ctrl+b twice there)",
                "leader: Space",
            ]
        );
        assert!(row.rows.iter().all(|r| r.chars().count() <= 80));
        assert_eq!(row.nvim_lhs, None);
    }

    #[test]
    fn neovim_row_words_a_certain_owner() {
        let mut f = findings();
        f.nav = NavOwner::SmartSplits;
        let prefix = key("C-Space");
        f.chords.push(owner(
            "C-Space",
            Mode::Insert,
            Owner::Desc("complete".into()),
            Certainty::Observed,
        ));
        let row = neovim_row_with(&f, Lang::En, &|text| text.to_string(), &prefix);
        assert_eq!(
            row.rows[1],
            "Ctrl+Space in Insert: complete (press Ctrl+Space twice there)"
        );
        assert!(row.rows[0].contains("smart-splits.nvim") && row.rows[0].contains("at_edge"));
        assert!(row.rows[1].chars().count() <= 80);
    }

    #[test]
    fn neovim_row_in_chinese() {
        let mut f = findings();
        f.chords.push(owner(
            "C-b",
            Mode::Insert,
            Owner::Plugin(KnownPlugin::BlinkCmp),
            Certainty::Probably,
        ));
        let row = neovim_row(&f, Lang::ZhCn);
        assert_eq!(row.rows[0], "Ctrl+h/j/k/l：vim-tmux-navigator（可直接用）");
        assert_eq!(row.rows[1], "插入模式 Ctrl+b：可能是 blink.cmp（在那里按两次 Ctrl+b）");
        assert_eq!(row.rows[2], "leader：Space");
    }

    #[test]
    fn neovim_row_has_at_most_three_lines_and_drops_the_leader_first() {
        let mut f = findings();
        for mode in [Mode::Insert, Mode::Terminal, Mode::Normal] {
            f.chords
                .push(owner("C-b", mode, Owner::LuaFunction, Certainty::Observed));
        }
        f.chords
            .push(owner("C-Space", Mode::Insert, Owner::LuaFunction, Certainty::Observed));
        let row = neovim_row(&f, Lang::En);
        assert_eq!(row.rows.len(), 3);
        assert!(row.rows[1].starts_with("Ctrl+b in Insert"));
        assert!(row.rows[2].starts_with("Ctrl+b in Terminal"));
        assert!(!row.rows.iter().any(|r| r.starts_with("leader")));
    }

    #[test]
    fn neovim_row_says_nothing_for_eitri_defaults_and_other_keys() {
        let mut f = findings();
        f.chords
            .push(owner("C-b", Mode::Normal, Owner::NvimDefault, Certainty::Observed));
        f.chords
            .push(owner("C-Space", Mode::Insert, Owner::Eitri, Certainty::Observed));
        f.chords
            .push(owner("C-a", Mode::Insert, Owner::LuaFunction, Certainty::Observed));
        f.leader = String::new();
        let row = neovim_row(&f, Lang::En);
        assert_eq!(row.rows, ["Ctrl+h/j/k/l: vim-tmux-navigator (works as is)"]);
    }

    #[test]
    fn neovim_row_nav_wording_per_owner() {
        let mut f = findings();
        f.leader = String::new();
        f.nav = NavOwner::EitriFallback;
        assert_eq!(neovim_row(&f, Lang::En).rows, ["Ctrl+h/j/k/l: Eitri's (works as is)"]);
        f.nav = NavOwner::Navigator;
        assert_eq!(
            neovim_row(&f, Lang::En).rows,
            ["Ctrl+h/j/k/l: Navigator.nvim (works as is)"],
            "the tmux shim answers its select-pane, and it has no edge hook"
        );
        f.nav = NavOwner::UserMapping(Owner::Desc("my moves".into()));
        assert_eq!(
            neovim_row(&f, Lang::En).rows,
            ["Ctrl+h/j/k/l: yours (my moves); Eitri's moves: Ctrl+b arrows"],
            "the nav fallback leaves it alone, so it does not cross into a module"
        );
        let row = neovim_row_with(&f, Lang::ZhCn, &|text| text.to_string(), &key("C-a"));
        assert_eq!(
            row.rows,
            ["Ctrl+h/j/k/l：你的映射（my moves）；Eitri 移动：Ctrl+a 方向键"]
        );
        f.nav = NavOwner::LazyStub;
        assert!(neovim_row(&f, Lang::En).rows.is_empty());
    }

    #[test]
    fn neovim_row_says_twice_only_of_the_prefix_the_window_runs_with() {
        let blink = Owner::Plugin(KnownPlugin::BlinkCmp);
        let mut f = findings();
        for chord in ["C-b", "C-Space", "C-a"] {
            f.chords
                .push(owner(chord, Mode::Insert, blink.clone(), Certainty::Probably));
        }
        let row = neovim_row_with(&f, Lang::En, &|text| text.to_string(), &key("C-a"));
        assert_eq!(
            row.rows,
            [
                "Ctrl+h/j/k/l: vim-tmux-navigator (works as is)",
                "Ctrl+a in Insert: probably blink.cmp (press Ctrl+a twice there)",
                "leader: Space",
            ],
            "the other chords neither mislead nor push out the leader"
        );
        assert_eq!(row.chord_lines.len(), 3, "the review can still name them");
        assert_eq!(row.chord_lines[1].0, key("C-Space"));
    }

    #[test]
    fn neovim_row_with_cleans_what_the_probe_supplied() {
        let mut f = findings();
        f.nav = NavOwner::UserMapping(Owner::Origin("plugin\u{1b}[31m".into()));
        f.chords.push(owner(
            "C-b",
            Mode::Insert,
            Owner::Desc("a{key}b".into()),
            Certainty::Observed,
        ));
        let clean = |s: &str| s.replace('\u{1b}', "^[");
        let row = neovim_row_with(&f, Lang::En, &clean, &key("C-b"));
        assert_eq!(
            row.rows[0],
            "Ctrl+h/j/k/l: yours (plugin^[[31m); Eitri's moves: Ctrl+b arrows"
        );
        assert!(row.rows[1].starts_with("Ctrl+b in Insert: a{key}b "), "{}", row.rows[1]);
    }

    #[test]
    fn no_row_with_a_control_character_survives() {
        // The parsers keep the probe's and the filesystem's text as it came: `display_text` is applied by the
        // screen (or through `neovim_row_with`), so a control character is still here and nothing below may
        // be relied on to have removed it.
        let mut f = findings();
        f.nav = NavOwner::UserMapping(Owner::Desc("evil\u{1b}[2J".into()));
        assert!(neovim_row(&f, Lang::En).rows[0].contains('\u{1b}'));
        let import = TmuxImport {
            files: vec![PathBuf::from("/x/\u{7}bell")],
            ..TmuxImport::default()
        };
        assert!(tmux_row(Some(&import), None).path.is_some_and(|p| p.contains('\u{7}')));
    }

    fn desktop(wm: Wm, live: bool) -> DesktopRow {
        desktop_row(wm, live, None, Vec::new(), false)
    }

    #[test]
    fn desktop_row_names_the_compositor() {
        let row = desktop_row(Wm::Sway, true, Some(WmMod::Super), vec![Chord::of(&key("M-x"))], false);
        assert_eq!(row.wm, Some("sway"));
        assert!(row.live);
        assert_eq!(row.wm_mod, Some(WmMod::Super));
        assert_eq!(row.note, None);
        assert_eq!(row.binds.len(), 1);
        assert_eq!(desktop(Wm::Gnome, false).wm, Some("GNOME"));
        assert_eq!(desktop(Wm::None, false).wm, None);
        assert_eq!(desktop(Wm::Niri, true).wm, Some("niri"));
    }

    #[test]
    fn a_hyprland_lua_config_with_no_binds_is_noted() {
        let row = desktop_row(Wm::Hyprland, false, None, Vec::new(), true);
        assert_eq!(row.note.as_deref(), Some(NOTE_HYPR_LUA));
        let read = desktop_row(Wm::Hyprland, true, None, vec![Chord::of(&key("M-x"))], true);
        assert_eq!(read.note, None);
        assert_eq!(desktop_row(Wm::Sway, true, None, Vec::new(), true).note, None);
    }

    fn tmux(path: Option<&str>) -> TmuxRow {
        TmuxRow {
            path: path.map(String::from),
            prefix: None,
            changes_keys: false,
        }
    }

    const WITH_COMPOSITOR: Build = Build {
        compositor_mode: true,
        nvim_prefix: true,
    };

    #[test]
    fn preselect_follows_the_tmux_row() {
        let b = Build::THIS_BINARY;
        assert_eq!(preselect(&tmux(Some("~/.tmux.conf")), None, b), PresetId::NvimTmux);
        assert_eq!(preselect(&tmux(None), None, b), PresetId::NvimNoTmux);
    }

    #[test]
    fn preselect_takes_the_compositor_only_with_compositor_mode_and_a_live_tiler() {
        let live = desktop(Wm::Sway, true);
        assert_eq!(
            preselect(&tmux(Some("p")), Some(&live), WITH_COMPOSITOR),
            PresetId::Compositor
        );
        assert_eq!(
            preselect(&tmux(None), Some(&live), WITH_COMPOSITOR),
            PresetId::Compositor
        );
        assert_eq!(
            preselect(
                &tmux(Some("p")),
                Some(&live),
                Build {
                    compositor_mode: false,
                    ..WITH_COMPOSITOR
                }
            ),
            PresetId::NvimTmux
        );
        let down = desktop(Wm::Sway, false);
        assert_eq!(
            preselect(&tmux(None), Some(&down), WITH_COMPOSITOR),
            PresetId::NvimNoTmux
        );
        let gnome = desktop(Wm::Gnome, true);
        assert_eq!(
            preselect(&tmux(None), Some(&gnome), WITH_COMPOSITOR),
            PresetId::NvimNoTmux
        );
        let mut i3 = desktop(Wm::None, true);
        i3.wm = Some("i3");
        assert_eq!(
            preselect(&tmux(Some("p")), Some(&i3), WITH_COMPOSITOR),
            PresetId::NvimTmux
        );
    }

    #[test]
    fn a_prefix_on_a_compositor_bind_is_found() {
        let mut row = desktop(Wm::Hyprland, true);
        row.binds = vec![
            chord_of_parts(true, false, false, "space").unwrap(),
            chord_of_parts(false, true, false, "x").unwrap(),
            chord_of_parts(true, true, false, "p").unwrap(),
        ];
        assert!(prefix_is_a_compositor_bind(&key("C-Space"), &row));
        assert!(prefix_is_a_compositor_bind(&key("M-x"), &row));
        assert!(prefix_is_a_compositor_bind(&key("C-M-p"), &row));
        assert!(!prefix_is_a_compositor_bind(&key("C-b"), &row));
        assert!(!prefix_is_a_compositor_bind(&key("M-y"), &row));
    }
}
