//! Root keys that something else may take first: warnings, never failures. A root key is a chord Eitri expects to
//! receive; an input method's trigger or a desktop shortcut can swallow it before the window sees it. Three sources
//! are compared with the user's root table: the default triggers of an input method that is running (a closed table
//! of defaults read from each project's own source), GNOME's live shortcut settings (only in a GNOME session), and
//! the live binds of sway and Hyprland. Nothing here runs a command, reads a file but the notice record, or
//! touches the bus: the caller gathers the evidence off the GTK thread and hands it in.
//!
//! Every answer from outside -- a compositor's reply, a settings value -- is parsed into chords and then dropped.
//! The log line and the toast are built from the root key's own spelling, the action's name and constants of this
//! file; no text of a reply reaches them. KDE and niri have no source here and say nothing.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::compositor::Wm;
use crate::keymap::{Binding, Chord, KeySpec};

pub mod gnome;
pub mod hyprland;
pub mod sway;

pub use gnome::gnome_chords;
pub use hyprland::hyprland_chords;
pub use sway::sway_chords;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Im {
    Fcitx,
    Ibus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    InputMethod(Im),
    Gnome,
    Sway,
    Hyprland,
}

impl Source {
    /// How the toast and the log name the source: a constant, never text from an answer.
    pub fn name(self) -> &'static str {
        match self {
            Source::InputMethod(Im::Fcitx) => "fcitx5",
            Source::InputMethod(Im::Ibus) => "IBus",
            Source::Gnome => "GNOME",
            Source::Sway => "sway",
            Source::Hyprland => "Hyprland",
        }
    }
}

/// The session-bus names whose owner means an input method runs.
pub const BUS_NAMES: &[&str] = &["org.fcitx.Fcitx5", "org.freedesktop.IBus"];

/// Which input methods the window's environment or the session bus says run: `GTK_IM_MODULE`, `QT_IM_MODULE` and
/// `XMODIFIERS` naming one, or one of [`BUS_NAMES`] among `bus_names` (the names that have an owner).
pub fn im_evidence(env: &dyn Fn(&str) -> Option<OsString>, bus_names: &[&str]) -> Vec<Im> {
    let mut found = Vec::new();
    for var in ["GTK_IM_MODULE", "QT_IM_MODULE", "XMODIFIERS"] {
        let Some(value) = env(var) else { continue };
        let value = value.to_string_lossy().to_ascii_lowercase();
        if value.contains("fcitx") {
            found.push(Im::Fcitx);
        }
        if value.contains("ibus") {
            found.push(Im::Ibus);
        }
    }
    for name in bus_names {
        match *name {
            "org.fcitx.Fcitx5" => found.push(Im::Fcitx),
            "org.freedesktop.IBus" => found.push(Im::Ibus),
            _ => {}
        }
    }
    found.sort();
    found.dedup();
    found
}

/// (input method, tmux key, what it does, the upstream file the default was read from). Each default is a key the
/// project ships as its own, read from its source: fcitx5 `src/lib/fcitx/globalconfig.cpp` (`TriggerKeys`,
/// `TogglePreedit`) and `src/modules/clipboard/clipboard.h` (the clipboard addon's `TriggerKey`). IBus has no row:
/// its `general/hotkey` `trigger` key (Control+space, Alt+grave) is a legacy one no IBus source reads any more, the
/// `triggers` key that its panel and setup tool read defaults to Super+space, and its emoji pair was `Ctrl+.` and
/// `Ctrl+;` up to 1.5.26 and is `Super+.` and `Super+;` since 1.5.29.
pub const IM_DEFAULTS: &[(Im, &str, &str, &str)] = &[
    (
        Im::Fcitx,
        "C-Space",
        "fcitx5 switches input methods with it by default",
        "src/lib/fcitx/globalconfig.cpp",
    ),
    (
        Im::Fcitx,
        "C-M-p",
        "fcitx5 toggles the embedded preedit with it by default",
        "src/lib/fcitx/globalconfig.cpp",
    ),
    (
        Im::Fcitx,
        "C-;",
        "fcitx5's clipboard addon opens its history with it by default",
        "src/modules/clipboard/clipboard.h",
    ),
];

/// Which live desktop sources apply to this session. GNOME's settings are read only where GNOME runs: on sway or
/// Hyprland the installed GNOME schemas hold defaults nobody runs.
pub fn desktop_sources(wm: Wm) -> &'static [Source] {
    match wm {
        Wm::Gnome => &[Source::Gnome],
        Wm::Sway => &[Source::Sway],
        Wm::Hyprland => &[Source::Hyprland],
        Wm::Niri | Wm::None => &[],
    }
}

/// The key names the keymap spells that are not one character, in the case a compositor may write them.
const KEY_NAMES: &[&str] = &[
    "Up",
    "Down",
    "Left",
    "Right",
    "space",
    "Return",
    "Tab",
    "BackSpace",
    "Delete",
    "Insert",
    "Home",
    "End",
    "Page_Up",
    "Page_Down",
];

/// The xkb names of the ASCII punctuation (and the two old aliases of `apostrophe` and `grave`), which a compositor
/// writes in full where the keymap spells the character. Matched without regard to case, as xkb does when the exact
/// name is unknown.
const PUNCTUATION_KEYSYMS: &[(&str, char)] = &[
    ("exclam", '!'),
    ("quotedbl", '"'),
    ("numbersign", '#'),
    ("dollar", '$'),
    ("percent", '%'),
    ("ampersand", '&'),
    ("apostrophe", '\''),
    ("quoteright", '\''),
    ("parenleft", '('),
    ("parenright", ')'),
    ("asterisk", '*'),
    ("plus", '+'),
    ("comma", ','),
    ("minus", '-'),
    ("period", '.'),
    ("slash", '/'),
    ("colon", ':'),
    ("semicolon", ';'),
    ("less", '<'),
    ("equal", '='),
    ("greater", '>'),
    ("question", '?'),
    ("at", '@'),
    ("bracketleft", '['),
    ("backslash", '\\'),
    ("bracketright", ']'),
    ("asciicircum", '^'),
    ("underscore", '_'),
    ("grave", '`'),
    ("quoteleft", '`'),
    ("braceleft", '{'),
    ("bar", '|'),
    ("braceright", '}'),
    ("asciitilde", '~'),
];

/// The longest key text a compositor bind is read from.
const KEY_TEXT_LIMIT: usize = 32;

/// Whether `key` can be a key name: ASCII letters, digits and `_`, or one printable ASCII character. Nothing else a
/// reply holds can enter a chord.
fn is_key_text(key: &str) -> bool {
    if key.is_empty() || key.len() > KEY_TEXT_LIMIT {
        return false;
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => c.is_ascii_graphic(),
        _ => key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
    }
}

/// A chord from modifier flags and a key as a compositor writes it (`Return`, `t`, `T`, `space`, `comma`, `f5`),
/// in the spelling the keymap's chords use; `None` for text that is not a key.
pub(crate) fn chord_of_parts(ctrl: bool, alt: bool, shift: bool, key: &str) -> Option<Chord> {
    if !is_key_text(key) {
        return None;
    }
    let single = key.chars().count() == 1;
    let mut chord = Chord::from_accel(key).ok()?;
    if !single {
        if let Some(name) = KEY_NAMES.iter().find(|name| name.eq_ignore_ascii_case(key)) {
            chord.key = (*name).to_string();
        } else if let Some(n) = key.strip_prefix(['f', 'F']).and_then(|n| n.parse::<u8>().ok()) {
            chord.key = format!("F{n}");
        } else {
            match key.to_ascii_lowercase().as_str() {
                "prior" => chord.key = "Page_Up".to_string(),
                "next" => chord.key = "Page_Down".to_string(),
                _ => {}
            }
        }
    }
    chord.ctrl = ctrl;
    chord.alt = alt;
    chord.shift = shift;
    checked(chord)
}

/// `chord` in the spelling [`Chord::of`] gives a key of the keymap (the space bar is `space` there, while an
/// accelerator's `space` reads as the character, and a punctuation key named in full -- `grave`, `bracketleft` -- is
/// the character), or `None` when its key is not key text.
pub(crate) fn checked(mut chord: Chord) -> Option<Chord> {
    if chord.key == " " {
        chord.key = "space".to_string();
    } else if let Some((_, ch)) = PUNCTUATION_KEYSYMS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(&chord.key))
    {
        chord.key = ch.to_string();
    }
    is_key_text(&chord.key).then_some(chord)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub key: KeySpec,
    /// The action's name, as `init.lua` writes it.
    pub action: String,
    pub source: Source,
    /// One sentence from this file's constants, never from an answer.
    pub what: String,
}

fn same_chord(root: &KeySpec, other: &Chord) -> bool {
    Chord::of(root).folded() == other.folded()
}

/// Every root key that an input method running here, or a live desktop bind, takes first. One conflict for each
/// root key and source, in the root table's order.
pub fn conflicts(root: &[Binding], ims: &[Im], desktop: &[(Source, Vec<(Chord, String)>)]) -> Vec<Conflict> {
    let mut out = Vec::new();
    for binding in root {
        let action = binding.action.name();
        let mut push = |source: Source, what: String| {
            out.push(Conflict {
                key: binding.key,
                action: action.clone(),
                source,
                what,
            });
        };
        for im in ims {
            let row = IM_DEFAULTS.iter().find(|(row_im, tmux_key, _, _)| {
                row_im == im
                    && KeySpec::parse(tmux_key)
                        .is_ok_and(|spec| Chord::of(&spec).folded() == Chord::of(&binding.key).folded())
            });
            if let Some((_, _, what, _)) = row {
                push(Source::InputMethod(*im), (*what).to_string());
            }
        }
        for (source, chords) in desktop {
            if chords.iter().any(|(chord, _)| same_chord(&binding.key, chord)) {
                push(*source, format!("{} binds it", source.name()));
            }
        }
    }
    out
}

/// The line logged on every launch for one conflict.
pub fn log_line(c: &Conflict) -> String {
    let tail = match c.source {
        Source::InputMethod(_) => "if it does here, Eitri never sees it",
        _ => "Eitri never sees it",
    };
    format!("[keys] root key {} ({}): {}; {tail}", c.key.human(), c.action, c.what)
}

/// The most conflicts the toast names; the rest are counted.
const TOAST_NAMES: usize = 4;

/// The toast: the chords and the names of the sources only, the log has the rest.
pub fn toast_text(cs: &[Conflict]) -> String {
    let mut names: Vec<String> = cs
        .iter()
        .map(|c| format!("{} ({})", c.key.human(), c.source.name()))
        .collect();
    names.sort();
    names.dedup();
    let more = names.len().saturating_sub(TOAST_NAMES);
    names.truncate(TOAST_NAMES);
    let mut text = format!("Root keys another program may take first: {}", names.join(", "));
    if more > 0 {
        text.push_str(&format!(" and {more} more"));
    }
    text.push_str(". Eitri never sees a key that program handles; the log lists them.");
    text
}

/// SHA-256, hex, over the set of conflicts as (root key, source), sorted: the same set in another order is no news,
/// and neither is a changed action.
pub fn fingerprint(cs: &[Conflict]) -> String {
    let mut lines: Vec<String> = cs.iter().map(|c| format!("{}\t{}\n", c.key, c.source.name())).collect();
    lines.sort();
    lines.dedup();
    Sha256::digest(lines.concat().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The file the last announced set's fingerprint is kept in, by the same rule as every other state file.
pub fn state_file(xdg_state_home: Option<&std::ffi::OsStr>, home: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    crate::state_paths::state_subdir(xdg_state_home, home, "root-key-conflicts")
}

/// Whether this set has not been announced yet: no file, an unreadable one, or another fingerprint in it. The file
/// is the tmux import's notice record (a private 0600 file, read as a regular file only, written by rename).
pub fn needs_notice(file: &Path, fingerprint: &str) -> bool {
    crate::keymap::tmux::notice::needs_notice(file, fingerprint)
}

/// Records that this set was announced.
pub fn remember(file: &Path, fingerprint: &str) -> std::io::Result<()> {
    crate::keymap::tmux::notice::remember(file, fingerprint)
}

/// What to do about this launch's conflicts: the toast text when the set differs from the last one announced
/// (recorded in `file` when there is one), and the error of recording it. A set that went empty after one was
/// announced is recorded without a toast, so its return is news again; with no record at all and nothing found,
/// nothing is written.
pub fn announce(file: Option<&Path>, cs: &[Conflict]) -> (Option<String>, Option<std::io::Error>) {
    let print = fingerprint(cs);
    let due = file.is_none_or(|f| needs_notice(f, &print) && (!cs.is_empty() || f.exists()));
    let toast = (due && !cs.is_empty()).then(|| toast_text(cs));
    let error = match file {
        Some(f) if due => remember(f, &print).err(),
        _ => None,
    };
    (toast, error)
}

#[cfg(test)]
mod tests;
