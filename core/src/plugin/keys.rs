//! A plugin page's key table: the vim-style notation its keys are written in, the keys Eitri keeps for
//! itself (which a page may never bind, by declaration or by an `init.lua` override), and the merge of
//! what the page declared with what the user overrode in `init.lua`. The page applies the merged table
//! itself; the host only checks it, keeps it for the `?` sheet, and hands it back.

use std::collections::{HashMap, HashSet};

use crate::keymap::root;
use crate::keymap::Chord;

use super::protocol::KeyRowArg;

pub const MAX_ROWS: usize = 200;
pub const MAX_KEYS_PER_ROW: usize = 10;
pub const MAX_ACTION_CHARS: usize = 40;
pub const MAX_CONTEXT_CHARS: usize = 40;
pub const MAX_DESC_CHARS: usize = 80;
/// The longest sequence one key may be: the page's resolver waits for at most this many strokes.
pub const MAX_STROKES: usize = 10;

/// One declared row, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRow {
    pub action: String,
    pub keys: Vec<String>,
    pub context: String,
    pub desc: String,
}

/// Where a key of the effective table comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Default,
    InitLua,
}

impl KeySource {
    pub fn as_str(self) -> &'static str {
        match self {
            KeySource::Default => "default",
            KeySource::InitLua => "init.lua",
        }
    }
}

/// A declared row with the user's overrides applied; the declared metadata is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveRow {
    pub action: String,
    pub keys: Vec<(String, KeySource)>,
    pub context: String,
    pub desc: String,
}

/// One `eitri.keymap` call on a `plugin:<id>` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginKeyOp {
    Set { key: String, action: String },
    Del { key: String },
}

/// The `?` sheet's section for the plugin page that held the keys: its title, its effective keys as plain
/// text rows, and whether the page has declared at all. A page that declared but has no key left (an empty
/// table, or the user's overrides removed every key) is `declared` with no rows: the sheet must not tell
/// the user it registered nothing when it did.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PluginKeysHelp {
    pub title: String,
    pub declared: bool,
    pub rows: Vec<root::HelpRow>,
}

fn generic_actions() -> &'static [String] {
    static ACTIONS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    ACTIONS.get_or_init(|| {
        #[derive(serde::Deserialize)]
        struct GenericActions {
            actions: Vec<String>,
        }
        serde_json::from_str::<GenericActions>(include_str!("kit_generic_actions.json"))
            .expect("the bundled generic action list is valid")
            .actions
    })
}

/// Builds the sheet's section from a page's effective table (`None`: not declared yet).
pub fn plugin_help(title: &str, rows: Option<Vec<EffectiveRow>>) -> PluginKeysHelp {
    let declared = rows.is_some();
    let (generic, page): (Vec<_>, Vec<_>) = rows
        .unwrap_or_default()
        .into_iter()
        // An action the user's overrides left with no key cannot be pressed, so the sheet has nothing to say.
        .filter(|row| !row.keys.is_empty())
        .partition(|row| generic_actions().contains(&row.action));
    let mut rows: Vec<_> = page
        .into_iter()
        .map(|row| root::HelpRow {
            keys: row
                .keys
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>()
                .join(" / "),
            what: if row.context.is_empty() {
                row.desc
            } else {
                format!("{} · {}", row.context, row.desc)
            },
        })
        .collect();
    if let Some(first) = generic.first() {
        rows.push(root::HelpRow {
            keys: generic
                .iter()
                .map(|row| {
                    row.keys
                        .iter()
                        .map(|(key, _)| key.as_str())
                        .collect::<Vec<_>>()
                        .join(" / ")
                })
                .collect::<Vec<_>>()
                .join(" "),
            what: if !first.context.is_empty() && generic.iter().all(|row| row.context == first.context) {
                first.context.clone()
            } else {
                "page".into()
            },
        });
    }
    PluginKeysHelp {
        title: title.to_string(),
        declared,
        rows,
    }
}

/// `[a-z0-9_.-]`, 1 to 40 characters.
pub fn action_ok(action: &str) -> bool {
    !action.is_empty()
        && action.len() <= MAX_ACTION_CHARS
        && action
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-'))
}

/// Key names inside `<>`, case-blind as in vim: the name, the key as a chord (GTK's keysym name, as
/// `Chord::of` gives it, so a page's key compares with the prefix and the root chords), and the name the
/// page's key kit reads for it, which is the spelling handed back to the page (`None`: the kit reads the
/// key as its own character). Only keys the kit can bind are here: a page would silently lose any other,
/// so the host refuses it instead (no function keys, no PageUp/PageDown, no Insert).
const NAMED: [(&str, &str, Option<&str>); 17] = [
    ("esc", "Escape", Some("Esc")),
    ("cr", "Return", Some("CR")),
    ("enter", "Return", Some("CR")),
    ("return", "Return", Some("CR")),
    ("bs", "BackSpace", Some("BS")),
    ("tab", "Tab", Some("Tab")),
    ("space", "space", Some("Space")),
    ("del", "Delete", Some("Del")),
    ("home", "Home", Some("Home")),
    ("end", "End", Some("End")),
    ("up", "Up", Some("Up")),
    ("down", "Down", Some("Down")),
    ("left", "Left", Some("Left")),
    ("right", "Right", Some("Right")),
    ("lt", "<", Some("lt")),
    ("bslash", "\\", None),
    ("bar", "|", None),
];

/// The characters a US layout types without Shift, besides the lowercase letters and the digits. Ctrl with
/// any other character is, on that layout, Ctrl+Shift with the key that character sits on.
const UNSHIFTED: [char; 11] = ['`', '-', '=', '[', ']', '\\', ';', '\'', ',', '.', '/'];

/// The punctuation a US layout types with Shift. A browser reports Shift held for each of them, so the
/// kit reads `?` and `<S-?>` as one key, and so must the host.
const SHIFTED: &str = "~!@#$%^&*()_+{}|:\"<>?";

/// Whether `c` changes how the text around it reads, or breaks its line, without showing as anything: a
/// control (C0, DEL, C1), a line or paragraph separator, or any character the agent panel draws as an
/// escape (format characters such as bidi marks, isolates, zero-width characters and tag characters, and
/// blank-rendering fillers), taken from that one table so the two never drift apart. The `?` sheet draws a
/// page's rows as they are written, so a row holding one could disguise itself or the rows beside it; a
/// manifest's window title is refused for the same characters.
pub(crate) fn hidden(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') || agent::providers::acp::wire::is_invisible_char(c)
}

/// One spelling for a key that has two: Shift (without Ctrl) on a character a US layout types with Shift
/// is that character alone, as in vim, so `<S-h>`, `<S-H>` and `H` compare equal, and so do `<S-?>` and
/// `?`. Shift stays a modifier on any other key (`<S-Tab>`, `<S-1>`), and with Ctrl, where it marks the
/// chord the input method owns.
fn fold_shift(mut chord: Chord) -> Chord {
    let single = {
        let mut chars = chord.key.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        }
    };
    if let (Some(c), true, false) = (single, chord.shift, chord.ctrl) {
        if c.is_ascii_alphabetic() {
            chord.key = c.to_ascii_uppercase().to_string();
            chord.shift = false;
        } else if SHIFTED.contains(c) {
            chord.shift = false;
        }
    }
    chord
}

/// Modifier spellings that mean Super or Hyper. Those chords belong to the desktop, so a page may not
/// name one at all.
const DESKTOP_MODIFIERS: [&str; 5] = ["d", "super", "h", "hyper", "cmd"];

fn bracketed(text: &str, inner: &str) -> Result<Chord, String> {
    let bad = |why: &str| format!("{text:?}: <{inner}> {why}");
    let mut chord = Chord {
        ctrl: false,
        alt: false,
        shift: false,
        key: String::new(),
    };
    let mut rest = inner;
    // A modifier is a word followed by `-` with something after it, so `<C-->` is Ctrl and `-`.
    while let Some(at) = rest.find('-').filter(|&at| at > 0 && at + 1 < rest.len()) {
        let word = rest[..at].to_ascii_lowercase();
        let held = match word.as_str() {
            "c" => &mut chord.ctrl,
            "s" => &mut chord.shift,
            // The page's key kit reads no Alt: a key holding it would be dropped there and never fire.
            "a" | "m" => return Err(bad("holds Alt, which a page's keys cannot use")),
            w if DESKTOP_MODIFIERS.contains(&w) => {
                return Err(format!(
                    "{text:?} holds a Super or Hyper chord, which belongs to the desktop"
                ))
            }
            _ => return Err(bad("has a modifier that is not C- or S-")),
        };
        if std::mem::replace(held, true) {
            return Err(bad("names a modifier twice"));
        }
        rest = &rest[at + 1..];
    }
    let mut chars = rest.chars();
    chord.key = match (chars.next(), chars.next()) {
        (Some(c), None) if hidden(c) => return Err(bad("is an invisible or direction character")),
        // The kit reads one printable ASCII character inside `<>`; anything else is written bare.
        (Some(c), None) if c.is_ascii_graphic() => c.to_string(),
        (Some(_), None) => return Err(bad("is neither a key name nor one printable ASCII character")),
        _ => {
            let lower = rest.to_ascii_lowercase();
            match NAMED.iter().find(|(name, _, _)| *name == lower) {
                Some((_, key, _)) => key.to_string(),
                None => return Err(bad("is not a key name a page can bind")),
            }
        }
    };
    let mut chars = chord.key.chars();
    if let (true, Some(c), None) = (chord.ctrl, chars.next(), chars.next()) {
        // A browser reports Ctrl with a character Shift types (a capital, `?`, `+`) as Ctrl+Shift with
        // the key it sits on, so the chord is that one whichever way the row spells it.
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || UNSHIFTED.contains(&c)) {
            chord.shift = true;
        }
        chord.key = c.to_ascii_lowercase().to_string();
    }
    Ok(fold_shift(chord))
}

/// Every stroke of `text` as a chord, or why it is not a key. The notation is vim's, narrowed to what the
/// page's key kit binds: a character is itself, and `<...>` names a key, case-blind, with `C-` or `S-`
/// modifiers (`<C-d>`, `<Esc>`, `<lt>`).
pub fn parse(text: &str) -> Result<Vec<Chord>, String> {
    let mut strokes = Vec::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if c == '<' {
            let end = rest
                .find('>')
                .ok_or_else(|| format!("{text:?}: an unclosed < (the key < itself is <lt>)"))?;
            strokes.push(bracketed(text, &rest[1..end])?);
            rest = &rest[end + 1..];
        } else {
            if hidden(c) || c.is_whitespace() {
                return Err(format!(
                    "{text:?} holds a space, a control or an invisible character; name the key, as <Space>"
                ));
            }
            strokes.push(Chord {
                ctrl: false,
                alt: false,
                shift: false,
                key: c.to_string(),
            });
            rest = &rest[c.len_utf8()..];
        }
    }
    if strokes.is_empty() {
        return Err(format!("{text:?} is empty"));
    }
    if strokes.len() > MAX_STROKES {
        return Err(format!("{text:?} is more than {MAX_STROKES} keys long"));
    }
    Ok(strokes)
}

/// Why `strokes` hold a key a page may never bind, or `None`.
fn kept(strokes: &[Chord], prefix_key: &Chord) -> Option<String> {
    let roots = root::chords();
    let prefix_key = &fold_shift(prefix_key.clone());
    strokes.iter().find_map(|stroke| {
        if stroke.ctrl && stroke.shift {
            Some(format!(
                "{}, a Ctrl+Shift chord, which belongs to the input method",
                stroke.human()
            ))
        } else if stroke == prefix_key {
            Some(format!("{}, the prefix", stroke.human()))
        } else {
            roots
                .iter()
                .find(|(chord, _)| chord == stroke)
                .map(|(_, named)| format!("{named}, which Eitri keeps"))
        }
    })
}

/// How the page's key kit writes `chord`: modifiers as `C-` then `S-`, names as the kit has them, and a
/// character with no modifier bare (but `<` as `<lt>`).
fn spelled(chord: &Chord) -> String {
    let name = NAMED
        .iter()
        .find_map(|(_, key, kit)| if *key == chord.key { *kit } else { None });
    let modifiers = match (chord.ctrl, chord.shift) {
        (true, true) => "C-S-",
        (true, false) => "C-",
        (false, true) => "S-",
        (false, false) => "",
    };
    match name {
        Some(name) => format!("<{modifiers}{name}>"),
        None if modifiers.is_empty() => chord.key.clone(),
        None => format!("<{modifiers}{}>", chord.key),
    }
}

/// `text` in the one spelling the page's key kit parses (`<c-n>` is `<C-n>`, `<enter>` is `<CR>`, `<S-g>`
/// is `G`): the kit compares keys as written, so a spelling it does not know would bind nothing.
pub fn canonical(text: &str) -> Result<String, String> {
    Ok(parse(text)?.iter().map(spelled).collect())
}

/// `key` as the kit spells it; a key that does not parse (never one the host checked) is kept as written.
fn kit_spelling(key: &str) -> String {
    canonical(key).unwrap_or_else(|_| key.to_string())
}

/// Parses and checks a key the page or `init.lua` names: an error names the key and says why.
pub fn check_key(text: &str, prefix_key: &Chord) -> Result<Vec<Chord>, String> {
    let strokes = parse(text)?;
    match kept(&strokes, prefix_key) {
        Some(why) => Err(format!("{text:?} holds {why}")),
        None => Ok(strokes),
    }
}

fn check_text(what: &str, action: &str, text: &str, max: usize) -> Result<(), String> {
    if let Some(c) = text.chars().find(|&c| hidden(c)) {
        return Err(format!(
            "{action:?}: its {what} holds U+{:04X}, a control or invisible character; a row is one line of plain text",
            c as u32
        ));
    }
    let n = text.chars().count();
    if n > max {
        return Err(format!("{action:?}: its {what} is {n} characters; at most {max}"));
    }
    Ok(())
}

/// Checks a page's declaration: the limits, unique actions, no key held by two actions, and no key Eitri keeps.
pub fn validate(rows: Vec<KeyRowArg>, prefix_key: &Chord) -> Result<Vec<KeyRow>, String> {
    if rows.len() > MAX_ROWS {
        return Err(format!("{} rows; a page may declare at most {MAX_ROWS}", rows.len()));
    }
    let mut out: Vec<KeyRow> = Vec::with_capacity(rows.len());
    let mut actions: HashSet<String> = HashSet::with_capacity(rows.len());
    // Every key of the rows checked so far, as its strokes, to the action holding it: a declaration may hold
    // thousands of keys, and each is parsed once and looked up once.
    let mut held: HashMap<Vec<Chord>, String> = HashMap::new();
    for row in rows {
        if !action_ok(&row.action) {
            return Err(format!(
                "{:?}: an action is [a-z0-9_.-], 1 to {MAX_ACTION_CHARS} characters",
                row.action
            ));
        }
        if actions.contains(&row.action) {
            return Err(format!("{:?} is declared twice", row.action));
        }
        if row.keys.is_empty() || row.keys.len() > MAX_KEYS_PER_ROW {
            return Err(format!(
                "{:?}: {} keys; a row has 1 to {MAX_KEYS_PER_ROW}",
                row.action,
                row.keys.len()
            ));
        }
        let mut strokes_of_row = Vec::with_capacity(row.keys.len());
        for key in &row.keys {
            // A key that does not parse is refused here, so two spellings of one key always meet as strokes.
            let strokes = check_key(key, prefix_key).map_err(|why| format!("{:?}: {why}", row.action))?;
            // The page's kit would run only one of the two, and the `?` sheets would still list both.
            if let Some(other) = held.get(&strokes) {
                return Err(format!(
                    "{key:?} is held by both {other:?} and {:?}; a key names one action",
                    row.action
                ));
            }
            strokes_of_row.push(strokes);
        }
        check_text("context", &row.action, &row.context, MAX_CONTEXT_CHARS)?;
        check_text("desc", &row.action, &row.desc, MAX_DESC_CHARS)?;
        // A row's keys are compared only with earlier rows', so one key written twice in a row is not refused.
        for strokes in strokes_of_row {
            held.insert(strokes, row.action.clone());
        }
        actions.insert(row.action.clone());
        out.push(KeyRow {
            action: row.action,
            keys: row.keys,
            context: row.context,
            desc: row.desc,
        });
    }
    Ok(out)
}

/// Two spellings of the same key: compared stroke by stroke where both parse, else as written.
fn same_key(a: &str, b: &str) -> bool {
    match (parse(a), parse(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The declared rows with `overrides` applied in order, and a log line for each override that names an
/// action the page did not declare (or deletes a key no row holds), which is skipped. Every key comes back
/// in the kit's spelling ([`canonical`]), since the page binds exactly what this table holds.
pub fn effective(rows: &[KeyRow], overrides: &[PluginKeyOp]) -> (Vec<EffectiveRow>, Vec<String>) {
    let mut out: Vec<EffectiveRow> = rows
        .iter()
        .map(|r| EffectiveRow {
            action: r.action.clone(),
            keys: r.keys.iter().map(|k| (kit_spelling(k), KeySource::Default)).collect(),
            context: r.context.clone(),
            desc: r.desc.clone(),
        })
        .collect();
    let mut log = Vec::new();
    for op in overrides {
        match op {
            PluginKeyOp::Set { key, action } => {
                let Some(at) = out.iter().position(|r| r.action == *action) else {
                    log.push(format!(
                        "init.lua sets {key:?} to {action:?}, which the page does not declare; ignored"
                    ));
                    continue;
                };
                for row in &mut out {
                    row.keys.retain(|(k, _)| !same_key(k, key));
                }
                out[at].keys.push((kit_spelling(key), KeySource::InitLua));
            }
            PluginKeyOp::Del { key } => {
                let mut found = false;
                for row in &mut out {
                    let before = row.keys.len();
                    row.keys.retain(|(k, _)| !same_key(k, key));
                    found |= row.keys.len() != before;
                }
                if !found {
                    log.push(format!(
                        "init.lua deletes {key:?}, which no action of the page holds; ignored"
                    ));
                }
            }
        }
    }
    (out, log)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::KeySpec;

    fn stock_prefix() -> Chord {
        Chord::of(&KeySpec::parse("C-b").unwrap())
    }

    fn arg(action: &str, keys: &[&str]) -> KeyRowArg {
        KeyRowArg {
            action: action.into(),
            keys: keys.iter().map(|k| k.to_string()).collect(),
            context: "items".into(),
            desc: "does a thing".into(),
        }
    }

    fn row(action: &str, keys: &[&str]) -> KeyRow {
        KeyRow {
            action: action.into(),
            keys: keys.iter().map(|k| k.to_string()).collect(),
            context: "items".into(),
            desc: "does a thing".into(),
        }
    }

    fn keys_of(rows: &[EffectiveRow], action: &str) -> Vec<(String, KeySource)> {
        rows.iter().find(|r| r.action == action).unwrap().keys.clone()
    }

    /// A host chord as the kit's `parseNotation` gives it: `KeyboardEvent.key`'s name, and Shift held for
    /// any character a US layout types with Shift.
    fn kit_stroke(chord: &Chord) -> serde_json::Value {
        let key = match chord.key.as_str() {
            "Return" => "Enter",
            "BackSpace" => "Backspace",
            "space" => " ",
            "Up" => "ArrowUp",
            "Down" => "ArrowDown",
            "Left" => "ArrowLeft",
            "Right" => "ArrowRight",
            other => other,
        };
        let mut chars = key.chars();
        let shifted = match (chars.next(), chars.next()) {
            (Some(c), None) => c.is_ascii_uppercase() || SHIFTED.contains(c),
            _ => false,
        };
        serde_json::json!({ "key": key, "ctrl": chord.ctrl, "shift": chord.shift || shifted })
    }

    #[test]
    fn the_notation_list_shared_with_the_kit_holds_here() {
        let list: serde_json::Value = serde_json::from_str(include_str!("keys_notation.json")).unwrap();
        let keys = list["keys"].as_array().unwrap();
        for entry in keys {
            let notation = entry["notation"].as_str().unwrap();
            let want = entry["canonical"].as_str().unwrap();
            assert_eq!(canonical(notation).as_deref(), Ok(want), "{notation:?}");
            assert_eq!(canonical(want).as_deref(), Ok(want), "{want:?} is its own spelling");
            assert_eq!(parse(notation), parse(want), "{notation:?} and {want:?} are one key");
            let strokes: Vec<serde_json::Value> = parse(want).unwrap().iter().map(kit_stroke).collect();
            assert_eq!(serde_json::Value::from(strokes), entry["strokes"], "{want:?}");
        }
        for text in list["not_keys"].as_array().unwrap() {
            let text = text.as_str().unwrap();
            assert!(parse(text).is_err(), "{text:?}: {:?}", parse(text));
        }
        // Every name the kit reads is pinned by the list.
        for (_, _, kit) in NAMED {
            let Some(kit) = kit else { continue };
            let spelled = format!("<{kit}>");
            assert!(
                keys.iter().any(|e| e["canonical"].as_str().unwrap().contains(&spelled)),
                "{spelled}"
            );
        }
        assert_eq!(parse("gg").unwrap().len(), 2);
        assert_eq!(parse("<C-d>").unwrap()[0].key, "d");
        assert!(parse("<C-d>").unwrap()[0].ctrl);
    }

    #[test]
    fn keys_the_kit_cannot_bind_are_refused_naming_the_key() {
        let prefix = stock_prefix();
        for key in [
            "<M-n>",
            "<A-x>",
            "<m-x>",
            "<C-M-x>",
            "<F2>",
            "<F12>",
            "<PageDown>",
            "<PageUp>",
            "<Insert>",
            "<C-C-d>",
            "<é>",
            "<C-é>",
        ] {
            let err = check_key(key, &prefix).expect_err(key);
            assert!(err.contains(&format!("{key:?}")), "{key}: {err}");
        }
        assert!(check_key("<M-n>", &prefix).unwrap_err().contains("Alt"));
        // A character outside ASCII is still a key when written bare.
        assert!(check_key("é", &prefix).is_ok());
    }

    #[test]
    fn bad_notation_is_refused_naming_the_key() {
        for key in ["", "<Foo>", "<C-", "a b", "<X-a>", "jjjjjjjjjjj"] {
            let err = parse(key).expect_err(key);
            assert!(err.contains(&format!("{key:?}")), "{key}: {err}");
        }
    }

    #[test]
    fn the_keys_eitri_keeps_are_refused_naming_the_key() {
        let prefix = stock_prefix();
        for key in [
            "<C-h>", "<C-j>", "<C-k>", "<C-l>", "<F11>", "<C-=>", "<C-->", "<C-0>", "<C-b>", "<C-S-a>",
        ] {
            let err = check_key(key, &prefix).expect_err(key);
            assert!(err.contains(&format!("{key:?}")), "{key}: {err}");
        }
        for key in ["<D-j>", "<Super-j>", "<Hyper-j>", "<C-A>", "g<C-h>"] {
            let err = check_key(key, &prefix).expect_err(key);
            assert!(err.contains(&format!("{key:?}")), "{key}: {err}");
        }
        assert!(check_key("<C-S-a>", &prefix).unwrap_err().contains("Ctrl+Shift"));
        assert!(check_key("<C-b>", &prefix).unwrap_err().contains("prefix"));
        assert!(check_key("<C-h>", &prefix)
            .unwrap_err()
            .contains("move to the module left"));
        for key in ["<C-d>", "<C-n>", "j", "]q", "<Esc>", "<C-u>"] {
            assert!(check_key(key, &prefix).is_ok(), "{key}");
        }
        // On a US layout these characters are typed with Shift, so Ctrl with one is a Ctrl+Shift chord.
        for key in [
            "<C-?>", "<C-+>", "<C-_>", "<C-!>", "<C-{>", "<C-:>", "<C-\">", "<C-~>", "<C-lt>", "<C-bar>", "<S-C-?>",
        ] {
            let err = check_key(key, &prefix).expect_err(key);
            assert!(err.contains(&format!("{key:?}")), "{key}: {err}");
            assert!(err.contains("Ctrl+Shift"), "{key}: {err}");
        }
        for key in [
            "<C-[>",
            "<C-]>",
            "<C-/>",
            "<C-;>",
            "<C-'>",
            "<C-,>",
            "<C-.>",
            "<C-`>",
            "<C-\\>",
            "<C-bslash>",
            "<C-1>",
            "<C-9>",
        ] {
            assert!(check_key(key, &prefix).is_ok(), "{key}: {:?}", check_key(key, &prefix));
        }
        // A prefix moved in init.lua is what is refused, and the stock one is free again.
        let moved = Chord::of(&KeySpec::parse("C-a").unwrap());
        assert!(check_key("<C-a>", &moved).is_err());
        assert!(check_key("<C-b>", &moved).is_ok());
    }

    #[test]
    fn shift_on_a_letter_is_the_capital_letter() {
        assert_eq!(parse("<S-h>").unwrap(), parse("H").unwrap());
        assert_eq!(parse("<S-H>").unwrap(), parse("H").unwrap());
        assert_ne!(parse("<S-h>").unwrap(), parse("h").unwrap());
        assert!(!parse("<S-h>").unwrap()[0].shift);
        // Shift on a key that is not a letter stays a modifier.
        assert!(parse("<S-Tab>").unwrap()[0].shift);
    }

    #[test]
    fn shift_on_a_character_typed_with_shift_is_that_character() {
        for c in "~!@#$%^&*()_+{}|:\"?".chars() {
            assert_eq!(parse(&format!("<S-{c}>")), parse(&c.to_string()), "{c}");
        }
        assert_eq!(parse("<S-?>").unwrap(), parse("?").unwrap());
        assert_eq!(parse("<S-lt>").unwrap(), parse("<lt>").unwrap());
        assert_eq!(parse("<S-bar>").unwrap(), parse("|").unwrap());
        // A character typed without Shift keeps Shift as a modifier, as the kit does.
        assert_ne!(parse("<S-1>").unwrap(), parse("1").unwrap());
        assert_ne!(parse("<S-;>").unwrap(), parse(";").unwrap());
    }

    #[test]
    fn an_override_spelling_shift_on_punctuation_finds_the_declared_key() {
        let rows = [row("sheet.open", &["?"]), row("x.y", &["xy"])];
        let (eff, log) = effective(&rows, &[PluginKeyOp::Del { key: "<S-?>".into() }]);
        assert!(log.is_empty(), "{log:?}");
        assert!(keys_of(&eff, "sheet.open").is_empty());
        let set = PluginKeyOp::Set {
            key: "<S-?>".into(),
            action: "x.y".into(),
        };
        let (eff, log) = effective(&rows, &[set]);
        assert!(log.is_empty(), "{log:?}");
        assert!(keys_of(&eff, "sheet.open").is_empty(), "one chord, one action");
        assert_eq!(
            keys_of(&eff, "x.y"),
            vec![("xy".into(), KeySource::Default), ("?".into(), KeySource::InitLua)]
        );
    }

    #[test]
    fn every_key_comes_back_in_the_kits_spelling() {
        let rows = [
            row("scroll.down", &["j", "<C-n>"]),
            row("item.next", &["]q"]),
            row("item.open", &["o"]),
            row("item.back", &["<s-tab>", "<bslash>"]),
        ];
        let ops = [
            PluginKeyOp::Set {
                key: "<c-n>".into(),
                action: "item.next".into(),
            },
            PluginKeyOp::Set {
                key: "<enter>".into(),
                action: "item.open".into(),
            },
            PluginKeyOp::Set {
                key: "<esc>".into(),
                action: "item.back".into(),
            },
        ];
        let (eff, log) = effective(&rows, &ops);
        assert!(log.is_empty(), "{log:?}");
        assert_eq!(keys_of(&eff, "scroll.down"), vec![("j".into(), KeySource::Default)]);
        assert_eq!(
            keys_of(&eff, "item.next"),
            vec![("]q".into(), KeySource::Default), ("<C-n>".into(), KeySource::InitLua)]
        );
        assert_eq!(
            keys_of(&eff, "item.open"),
            vec![("o".into(), KeySource::Default), ("<CR>".into(), KeySource::InitLua)]
        );
        assert_eq!(
            keys_of(&eff, "item.back"),
            vec![
                ("<S-Tab>".into(), KeySource::Default),
                ("\\".into(), KeySource::Default),
                ("<Esc>".into(), KeySource::InitLua)
            ]
        );
    }

    #[test]
    fn an_override_spelling_a_capital_with_shift_finds_the_declared_capital() {
        let rows = [row("item.last", &["H", "G"]), row("item.first", &["gg"])];
        let (eff, log) = effective(&rows, &[PluginKeyOp::Del { key: "<S-h>".into() }]);
        assert!(log.is_empty(), "{log:?}");
        assert_eq!(keys_of(&eff, "item.last"), vec![("G".into(), KeySource::Default)]);
        let set = PluginKeyOp::Set {
            key: "<S-g>".into(),
            action: "item.first".into(),
        };
        let (eff, log) = effective(&rows, &[set]);
        assert!(log.is_empty(), "{log:?}");
        assert_eq!(keys_of(&eff, "item.last"), vec![("H".into(), KeySource::Default)]);
        assert_eq!(
            keys_of(&eff, "item.first"),
            vec![("gg".into(), KeySource::Default), ("G".into(), KeySource::InitLua)]
        );
    }

    #[test]
    fn invisible_or_direction_characters_are_refused_in_the_text_and_the_keys() {
        let prefix = stock_prefix();
        for bad in [
            "a\u{202E}b",
            "a\nb",
            "\u{7}",
            "a\u{7F}",
            "\u{2066}x\u{2069}",
            "a\u{200F}",
            "a\u{2028}b",
            "\u{3164}\u{3164}",
            "a\u{E0041}b",
            "a\u{E0001}",
            "\u{180E}",
            "a\u{FFF9}b",
            "a\u{34F}",
            "\u{2800}",
            "a\u{FE0F}",
            "\u{115F}",
        ] {
            let mut desc = arg("x", &["j"]);
            desc.desc = bad.into();
            let err = validate(vec![desc], &prefix).expect_err(bad);
            assert!(err.contains("desc"), "{bad:?}: {err}");
            let mut context = arg("x", &["j"]);
            context.context = bad.into();
            let err = validate(vec![context], &prefix).expect_err(bad);
            assert!(err.contains("context"), "{bad:?}: {err}");
        }
        for key in [
            "\u{202E}",
            "g\u{200B}",
            "<\u{202E}>",
            "<C-\u{2066}>",
            "\u{3164}",
            "g\u{E0041}",
            "<\u{E0020}>",
        ] {
            let err = validate(vec![arg("x", &[key])], &prefix).expect_err(key);
            assert!(err.contains(&format!("{key:?}")), "{key:?}: {err}");
        }
        let mut fine = arg("x", &["j"]);
        fine.desc = "下一题 (next) -- 看答案".into();
        fine.context = "题目 · items".into();
        assert!(validate(vec![fine], &prefix).is_ok());
    }

    #[test]
    fn a_declaration_within_the_limits_is_kept_as_written() {
        let rows = validate(
            vec![arg("item.next", &["]q", "<C-n>"]), arg("scroll.down", &["j"])],
            &stock_prefix(),
        )
        .unwrap();
        assert_eq!(
            rows,
            vec![row("item.next", &["]q", "<C-n>"]), row("scroll.down", &["j"])]
        );
        assert!(validate(vec![], &stock_prefix()).unwrap().is_empty());
    }

    #[test]
    fn a_declaration_over_the_limits_is_refused() {
        let prefix = stock_prefix();
        // Each row its own key (`aa`, `ab`, ...), since one key on two actions is refused.
        let own_key = |i: u8| format!("{}{}", (b'a' + i / 26) as char, (b'a' + i % 26) as char);
        let many: Vec<KeyRowArg> = (0..201).map(|i| arg(&format!("a{i}"), &[&own_key(i)])).collect();
        assert!(validate(many, &prefix).unwrap_err().contains("200"));
        let fine: Vec<KeyRowArg> = (0..200).map(|i| arg(&format!("a{i}"), &[&own_key(i)])).collect();
        assert!(validate(fine, &prefix).is_ok());
        let eleven = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k"];
        assert!(validate(vec![arg("x", &eleven)], &prefix).unwrap_err().contains("10"));
        assert!(validate(vec![arg("x", &eleven[..10])], &prefix).is_ok());
        assert!(validate(vec![arg("x", &[])], &prefix).is_err(), "a row needs a key");
        let long_action = "a".repeat(41);
        assert!(validate(vec![arg(&long_action, &["j"])], &prefix).is_err());
        assert!(validate(vec![arg(&"a".repeat(40), &["j"])], &prefix).is_ok());
        for bad in ["Item.next", "item next", "", "item/next", "ítem"] {
            assert!(validate(vec![arg(bad, &["j"])], &prefix).is_err(), "{bad:?}");
        }
        let mut long_desc = arg("x", &["j"]);
        long_desc.desc = "字".repeat(81);
        assert!(validate(vec![long_desc], &prefix).unwrap_err().contains("80"));
        let mut desc_80 = arg("x", &["j"]);
        desc_80.desc = "字".repeat(80);
        assert!(
            validate(vec![desc_80], &prefix).is_ok(),
            "the limit counts characters, not bytes"
        );
        let mut long_context = arg("x", &["j"]);
        long_context.context = "c".repeat(41);
        assert!(validate(vec![long_context], &prefix).unwrap_err().contains("40"));
    }

    #[test]
    fn a_repeated_action_is_refused_naming_it() {
        let err = validate(
            vec![arg("item.next", &["]q"]), arg("item.next", &["n"])],
            &stock_prefix(),
        )
        .unwrap_err();
        assert!(err.contains("\"item.next\""), "{err}");
    }

    #[test]
    fn a_key_held_by_two_actions_is_refused_naming_both() {
        for (first, second) in [("f", "f"), ("<C-n>", "<c-n>"), ("G", "<S-g>")] {
            let err = validate(
                vec![arg("hint.start", &["x", first]), arg("item.find", &[second])],
                &stock_prefix(),
            )
            .expect_err(second);
            assert!(err.contains(&format!("{second:?}")), "{err}");
            assert!(err.contains("\"hint.start\"") && err.contains("\"item.find\""), "{err}");
        }
        // A key that only starts a longer one is not the same key: the kit waits `timeoutlen` between them.
        assert!(validate(
            vec![arg("item.next", &["g"]), arg("scroll.top", &["gg"])],
            &stock_prefix()
        )
        .is_ok());
    }

    #[test]
    fn a_key_spelled_two_ways_in_two_rows_is_still_refused() {
        let prefix = stock_prefix();
        for (first, second) in [("g<Enter>", "g<CR>"), ("<lt>x", "<LT>x"), ("<Esc>G", "<esc><S-g>")] {
            assert!(
                validate(vec![arg("scroll.top", &[second])], &prefix).is_ok(),
                "{second}"
            );
            let err = validate(
                vec![
                    arg("hint.start", &[first]),
                    arg("item.next", &["n"]),
                    arg("scroll.top", &["j", second]),
                ],
                &prefix,
            )
            .expect_err(second);
            assert!(err.contains(&format!("{second:?} is held by both")), "{err}");
            assert!(
                err.contains("\"hint.start\"") && err.contains("\"scroll.top\""),
                "{err}"
            );
        }
    }

    #[test]
    fn validation_grows_linearly_with_the_declaration() {
        let prefix = stock_prefix();
        // Key `i` is `i` written in base ten with the letters `a` to `j`, one letter per stroke, so every key
        // is distinct and as long as a key may be.
        let key = |i: usize| -> String {
            (0..MAX_STROKES as u32)
                .map(|d| (b'a' + (i / 10usize.pow(d) % 10) as u8) as char)
                .collect()
        };
        let declaration = |n: usize| -> Vec<KeyRowArg> {
            (0..n)
                .map(|r| KeyRowArg {
                    action: format!("a{r}"),
                    keys: (0..MAX_KEYS_PER_ROW).map(|k| key(r * MAX_KEYS_PER_ROW + k)).collect(),
                    context: "items".into(),
                    desc: "does a thing".into(),
                })
                .collect()
        };
        let best_of_three = |n: usize| {
            let rows = declaration(n);
            (0..3)
                .map(|_| {
                    let rows = rows.clone();
                    let start = std::time::Instant::now();
                    assert_eq!(validate(rows, &prefix).expect("a valid declaration").len(), n);
                    start.elapsed()
                })
                .min()
                .unwrap()
        };
        let (small, large) = (best_of_three(20), best_of_three(200));
        // Ten times the rows: about ten times the time when each key is checked once, about a hundred when
        // each key is compared with every earlier row's keys.
        assert!(large < small * 30, "20 rows took {small:?}, 200 rows took {large:?}");
    }

    #[test]
    fn a_reserved_or_bad_key_in_a_row_refuses_the_declaration_naming_it() {
        let prefix = stock_prefix();
        for key in ["<C-h>", "<F11>", "<C-=>", "<C-b>", "<C-S-a>", "<Foo>"] {
            let err = validate(vec![arg("item.next", &["]q", key])], &prefix).expect_err(key);
            assert!(err.contains(&format!("{key:?}")), "{key}: {err}");
        }
    }

    #[test]
    fn with_no_overrides_every_key_is_a_default() {
        let rows = [row("item.next", &["]q"]), row("scroll.down", &["j", "<C-e>"])];
        let (eff, log) = effective(&rows, &[]);
        assert!(log.is_empty());
        assert_eq!(
            keys_of(&eff, "scroll.down"),
            vec![("j".into(), KeySource::Default), ("<C-e>".into(), KeySource::Default)]
        );
        assert_eq!(eff[0].context, "items");
        assert_eq!(eff[0].desc, "does a thing");
    }

    #[test]
    fn a_set_moves_the_key_to_its_action_and_marks_it_init_lua() {
        let rows = [row("item.next", &["]q"]), row("scroll.down", &["j", "<C-n>"])];
        let set = PluginKeyOp::Set {
            key: "<C-n>".into(),
            action: "item.next".into(),
        };
        let (eff, log) = effective(&rows, &[set]);
        assert!(log.is_empty(), "{log:?}");
        assert_eq!(
            keys_of(&eff, "item.next"),
            vec![("]q".into(), KeySource::Default), ("<C-n>".into(), KeySource::InitLua)]
        );
        assert_eq!(keys_of(&eff, "scroll.down"), vec![("j".into(), KeySource::Default)]);
        // The same key spelled another way is still the same key.
        let set = PluginKeyOp::Set {
            key: "<c-n>".into(),
            action: "item.next".into(),
        };
        let (eff, _) = effective(&[row("item.next", &["]q"]), row("scroll.down", &["<C-n>"])], &[set]);
        assert!(keys_of(&eff, "scroll.down").is_empty());
    }

    #[test]
    fn a_del_removes_the_key() {
        let rows = [row("item.next", &["]q", "n"]), row("item.prev", &["[q"])];
        let (eff, log) = effective(&rows, &[PluginKeyOp::Del { key: "]q".into() }]);
        assert!(log.is_empty(), "{log:?}");
        assert_eq!(keys_of(&eff, "item.next"), vec![("n".into(), KeySource::Default)]);
        assert_eq!(keys_of(&eff, "item.prev"), vec![("[q".into(), KeySource::Default)]);
    }

    #[test]
    fn an_override_naming_an_undeclared_action_is_logged_and_not_applied() {
        let rows = [row("item.next", &["]q"])];
        let set = PluginKeyOp::Set {
            key: "]q".into(),
            action: "item.nope".into(),
        };
        let (eff, log) = effective(&rows, &[set]);
        assert_eq!(keys_of(&eff, "item.next"), vec![("]q".into(), KeySource::Default)]);
        assert_eq!(log.len(), 1);
        assert!(log[0].contains("item.nope"), "{}", log[0]);
    }

    #[test]
    fn overrides_apply_in_order() {
        let rows = [row("a", &["x"]), row("b", &["y"])];
        let ops = [
            PluginKeyOp::Set {
                key: "z".into(),
                action: "a".into(),
            },
            PluginKeyOp::Set {
                key: "z".into(),
                action: "b".into(),
            },
            PluginKeyOp::Del { key: "x".into() },
        ];
        let (eff, _) = effective(&rows, &ops);
        assert!(keys_of(&eff, "a").is_empty());
        assert_eq!(
            keys_of(&eff, "b"),
            vec![("y".into(), KeySource::Default), ("z".into(), KeySource::InitLua)]
        );
    }

    fn effective_row(action: &str, keys: &[(&str, KeySource)], context: &str, desc: &str) -> EffectiveRow {
        EffectiveRow {
            action: action.into(),
            keys: keys.iter().map(|(k, src)| (k.to_string(), *src)).collect(),
            context: context.into(),
            desc: desc.into(),
        }
    }

    #[test]
    fn the_sheet_section_joins_a_rows_keys_and_names_what_it_does() {
        let help = plugin_help(
            "Snotra",
            Some(vec![
                effective_row(
                    "item.next",
                    &[("]q", KeySource::Default), ("<C-n>", KeySource::InitLua)],
                    "题目",
                    "下一题",
                ),
                effective_row("sheet.open", &[("?", KeySource::Default)], "", "按键帮助"),
            ]),
        );
        assert_eq!(help.title, "Snotra");
        assert!(help.declared);
        assert_eq!(
            help.rows,
            vec![
                root::HelpRow {
                    keys: "]q / <C-n>".into(),
                    what: "题目 · 下一题".into(),
                },
                root::HelpRow {
                    keys: "?".into(),
                    what: "page".into(),
                },
            ]
        );
    }

    #[test]
    fn page_rows_come_first_and_the_generic_rows_fold_into_one_last_row() {
        let help = plugin_help(
            "Plugin",
            Some(vec![
                effective_row(
                    "scroll.down",
                    &[("j", KeySource::Default), ("<C-e>", KeySource::InitLua)],
                    "page",
                    "down",
                ),
                effective_row("scroll.up", &[("k", KeySource::Default)], "page", "up"),
                effective_row("hint.start", &[("f", KeySource::Default)], "page", "hint"),
                effective_row("item.next", &[("]q", KeySource::Default)], "item", "next"),
                effective_row("item.prev", &[("[q", KeySource::Default)], "", "previous"),
            ]),
        );
        assert_eq!(
            help.rows,
            vec![
                root::HelpRow {
                    keys: "]q".into(),
                    what: "item · next".into()
                },
                root::HelpRow {
                    keys: "[q".into(),
                    what: "previous".into()
                },
                root::HelpRow {
                    keys: "j / <C-e> k f".into(),
                    what: "page".into()
                },
            ]
        );
    }

    #[test]
    fn the_fold_is_labelled_with_the_shared_context_else_page() {
        for (down, up, expected) in [
            ("页面", "页面", "页面"),
            ("页面", "navigation", "page"),
            ("", "", "page"),
            ("页面", "", "page"),
            ("", "页面", "page"),
        ] {
            let help = plugin_help(
                "Plugin",
                Some(vec![
                    effective_row("scroll.down", &[("j", KeySource::Default)], down, "down"),
                    effective_row("scroll.up", &[("k", KeySource::Default)], up, "up"),
                ]),
            );
            assert_eq!(
                help.rows,
                vec![root::HelpRow {
                    keys: "j k".into(),
                    what: expected.into()
                }]
            );
        }
    }

    #[test]
    fn a_page_row_with_a_generic_action_is_folded() {
        let help = plugin_help(
            "Plugin",
            Some(vec![effective_row(
                "scroll.down",
                &[("x", KeySource::Default)],
                "custom",
                "page-defined action",
            )]),
        );
        assert_eq!(
            help.rows,
            vec![root::HelpRow {
                keys: "x".into(),
                what: "custom".into()
            }]
        );
    }

    #[test]
    fn rows_the_overrides_emptied_are_dropped_before_folding() {
        let help = plugin_help(
            "Plugin",
            Some(vec![
                effective_row("scroll.down", &[], "different context", "removed"),
                effective_row("scroll.up", &[("k", KeySource::Default)], "页面", "up"),
            ]),
        );
        assert_eq!(
            help.rows,
            vec![root::HelpRow {
                keys: "k".into(),
                what: "页面".into()
            }]
        );
        let empty = plugin_help(
            "Plugin",
            Some(vec![
                effective_row("scroll.down", &[], "page", "removed"),
                effective_row("scroll.up", &[], "page", "removed"),
            ]),
        );
        assert!(empty.declared);
        assert!(empty.rows.is_empty());
    }

    #[test]
    fn the_generic_action_list_parses_and_holds_18_unique_actions() {
        let actions = generic_actions();
        assert_eq!(actions.len(), 18);
        assert_eq!(actions.iter().collect::<std::collections::HashSet<_>>().len(), 18);
    }

    #[test]
    fn a_page_that_has_not_declared_gives_an_empty_section() {
        let help = plugin_help("Snotra", None);
        assert_eq!(help.title, "Snotra");
        assert!(!help.declared);
        assert!(help.rows.is_empty());
    }

    #[test]
    fn a_page_that_declared_but_kept_no_key_is_still_declared() {
        let all_removed = plugin_help("Snotra", Some(vec![effective_row("a", &[], "", "gone")]));
        assert!(all_removed.declared);
        assert!(all_removed.rows.is_empty());
        let empty_table = plugin_help("Snotra", Some(Vec::new()));
        assert!(empty_table.declared);
        assert!(empty_table.rows.is_empty());
    }

    #[test]
    fn a_row_whose_keys_were_all_removed_is_left_out_of_the_sheet() {
        let help = plugin_help(
            "Snotra",
            Some(vec![
                effective_row("a", &[], "", "gone"),
                effective_row("b", &[("y", KeySource::Default)], "", "kept"),
            ]),
        );
        assert_eq!(help.rows.len(), 1);
        assert_eq!(help.rows[0].what, "kept");
    }
}
