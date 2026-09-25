//! tmux's key names, which is what `init.lua` writes (spec §2.3): modifiers `C-` `M-` `S-`, then one
//! character or a name. Shift is folded into a character (`H`, never `S-h`), as `shell` folds it
//! today; `S-` exists only for keys that have no character (`S-Up`). `Chord` is the other spelling a
//! key has here -- a GTK accelerator (`<Control>equal`) -- normalised so the two can be compared,
//! which is what the collision checks between the prefix, the root table and a Lua `keybinding` do.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyName {
    /// One character, Shift already applied (`H`, `%`, `"`).
    Char(char),
    Up,
    Down,
    Left,
    Right,
    /// `F1`..=`F24`.
    F(u8),
    Space,
    Enter,
    Tab,
    BSpace,
    DC,
    IC,
    Home,
    End,
    PPage,
    NPage,
}

/// (tmux name, key, human name, vim name, GTK keysym name) for every key that is not a character
/// and not a function key.
const NAMED: [(&str, KeyName, &str, &str, &str); 14] = [
    ("Up", KeyName::Up, "Up", "Up", "Up"),
    ("Down", KeyName::Down, "Down", "Down", "Down"),
    ("Left", KeyName::Left, "Left", "Left", "Left"),
    ("Right", KeyName::Right, "Right", "Right", "Right"),
    ("Space", KeyName::Space, "Space", "Space", "space"),
    ("Enter", KeyName::Enter, "Enter", "CR", "Return"),
    ("Tab", KeyName::Tab, "Tab", "Tab", "Tab"),
    ("BSpace", KeyName::BSpace, "Backspace", "BS", "BackSpace"),
    ("DC", KeyName::DC, "Delete", "Del", "Delete"),
    ("IC", KeyName::IC, "Insert", "Insert", "Insert"),
    ("Home", KeyName::Home, "Home", "Home", "Home"),
    ("End", KeyName::End, "End", "End", "End"),
    ("PPage", KeyName::PPage, "PageUp", "PageUp", "Page_Up"),
    ("NPage", KeyName::NPage, "PageDown", "PageDown", "Page_Down"),
];

/// GTK keysym names that are one character, so `<Control>equal` and `C-=` compare equal.
const KEYSYM_CHARS: [(&str, char); 20] = [
    ("equal", '='),
    ("minus", '-'),
    ("plus", '+'),
    ("space", ' '),
    ("backslash", '\\'),
    ("quotedbl", '"'),
    ("percent", '%'),
    ("comma", ','),
    ("period", '.'),
    ("slash", '/'),
    ("semicolon", ';'),
    ("bar", '|'),
    ("underscore", '_'),
    ("braceleft", '{'),
    ("braceright", '}'),
    ("question", '?'),
    ("ampersand", '&'),
    ("exclam", '!'),
    ("less", '<'),
    ("greater", '>'),
];

/// A key as the keymap names it: its modifiers and the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeySpec {
    pub ctrl: bool,
    /// tmux's `M-`: the Alt key.
    pub meta: bool,
    /// Only ever true for a key that is not a [`KeyName::Char`].
    pub shift: bool,
    pub key: KeyName,
}

/// A key name that does not parse: the text as written, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyParseError {
    pub key: String,
    pub why: &'static str,
}

impl fmt::Display for KeyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} {}", self.key, self.why)
    }
}

impl std::error::Error for KeyParseError {}

impl KeySpec {
    /// A character key. `ctrl` lowercases an ASCII capital (`C-A` is `C-a`, as a terminal sends it).
    pub fn char(c: char, ctrl: bool, meta: bool) -> KeySpec {
        KeySpec {
            ctrl,
            meta,
            shift: false,
            key: KeyName::Char(if ctrl { c.to_ascii_lowercase() } else { c }),
        }
    }

    pub fn named(key: KeyName, ctrl: bool, meta: bool, shift: bool) -> KeySpec {
        KeySpec { ctrl, meta, shift, key }
    }

    /// tmux's syntax: any of `C-` `M-` `S-`, then one printable character or a name.
    pub fn parse(text: &str) -> Result<KeySpec, KeyParseError> {
        let err = |why| KeyParseError {
            key: text.to_string(),
            why,
        };
        let (mut ctrl, mut meta, mut shift) = (false, false, false);
        let mut rest = text;
        // A prefix is stripped only while something is left after it, so `-` and `C--` both parse.
        while rest.chars().count() > 2 {
            if let Some(r) = rest.strip_prefix("C-") {
                ctrl = true;
                rest = r;
            } else if let Some(r) = rest.strip_prefix("M-") {
                meta = true;
                rest = r;
            } else if let Some(r) = rest.strip_prefix("S-") {
                shift = true;
                rest = r;
            } else {
                break;
            }
        }
        let mut chars = rest.chars();
        match (chars.next(), chars.next()) {
            (None, _) => Err(err("is empty")),
            (Some(c), None) => {
                if c.is_control() || c.is_whitespace() {
                    return Err(err("is not a printable key (the space bar is Space)"));
                }
                if shift {
                    return Err(err(
                        "uses S- on a character; write the shifted character itself (H, not S-h)",
                    ));
                }
                Ok(KeySpec::char(c, ctrl, meta))
            }
            _ => match named(rest) {
                Some(key) => Ok(KeySpec { ctrl, meta, shift, key }),
                None => Err(err(
                    "is not a tmux key name (C- M- S- then a character, or Up Down Left Right F1-F24 Space \
                     Enter Tab BSpace DC IC Home End PPage NPage)",
                )),
            },
        }
    }

    /// The character, when this is a character with no modifier: what a Lua panel's `key` is.
    pub fn as_char(&self) -> Option<char> {
        match self.key {
            KeyName::Char(c) if !self.ctrl && !self.meta => Some(c),
            _ => None,
        }
    }

    /// How a person reads it: `Ctrl+b`, `Alt+Up`, `PageUp`.
    pub fn human(&self) -> String {
        let mut out = String::new();
        if self.ctrl {
            out.push_str("Ctrl+");
        }
        if self.meta {
            out.push_str("Alt+");
        }
        if self.shift {
            out.push_str("Shift+");
        }
        match self.key {
            KeyName::Char(c) => out.push(c),
            KeyName::F(n) => out.push_str(&format!("F{n}")),
            other => out.push_str(row(other).2),
        }
        out
    }

    /// nvim's key notation, for `send-keys`/`send-prefix` into the editor.
    pub fn to_vim(&self) -> String {
        let base = match self.key {
            KeyName::Char('<') => "lt".to_string(),
            KeyName::Char('\\') => "Bslash".to_string(),
            KeyName::Char('|') => "Bar".to_string(),
            KeyName::Char(c) => c.to_string(),
            KeyName::F(n) => format!("F{n}"),
            other => row(other).3.to_string(),
        };
        let plain_char = matches!(self.key, KeyName::Char(c) if !matches!(c, '<' | '\\' | '|'));
        if !self.ctrl && !self.meta && !self.shift && plain_char {
            return base;
        }
        let mut out = String::from("<");
        if self.ctrl {
            out.push_str("C-");
        }
        if self.meta {
            out.push_str("M-");
        }
        if self.shift {
            out.push_str("S-");
        }
        out.push_str(&base);
        out.push('>');
        out
    }
}

impl fmt::Display for KeySpec {
    /// tmux's own spelling, which round-trips through [`KeySpec::parse`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            f.write_str("C-")?;
        }
        if self.meta {
            f.write_str("M-")?;
        }
        if self.shift {
            f.write_str("S-")?;
        }
        match self.key {
            KeyName::Char(c) => write!(f, "{c}"),
            KeyName::F(n) => write!(f, "F{n}"),
            other => f.write_str(row(other).0),
        }
    }
}

fn named(text: &str) -> Option<KeyName> {
    if let Some(row) = NAMED.iter().find(|row| row.0 == text) {
        return Some(row.1);
    }
    let n: u8 = text.strip_prefix('F')?.parse().ok()?;
    (1..=24).contains(&n).then_some(KeyName::F(n))
}

fn row(key: KeyName) -> &'static (&'static str, KeyName, &'static str, &'static str, &'static str) {
    NAMED
        .iter()
        .find(|row| row.1 == key)
        .expect("every KeyName but Char and F has a NAMED row")
}

/// A chord as GTK or the keymap names it, normalised for comparison: a key that is one character is
/// that character (lowercase for an ASCII letter), anything else keeps its GTK keysym name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: String,
}

impl Chord {
    /// A GTK accelerator string (`<Control><Shift>f`, `F11`, `<Primary>h`).
    pub fn from_gtk(accel: &str) -> Result<Chord, String> {
        let mut chord = Chord {
            ctrl: false,
            alt: false,
            shift: false,
            key: String::new(),
        };
        let mut rest = accel;
        while let Some(end) = rest.strip_prefix('<').and_then(|r| r.find('>')) {
            match &rest[1..end + 1] {
                "Control" | "Ctrl" | "Primary" => chord.ctrl = true,
                "Alt" | "Mod1" => chord.alt = true,
                "Shift" => chord.shift = true,
                other => return Err(format!("{accel:?} holds <{other}>, a modifier neovibe does not read")),
            }
            rest = &rest[end + 2..];
        }
        let mut chars = rest.chars();
        chord.key = match (chars.next(), chars.next()) {
            (None, _) => return Err(format!("{accel:?} names no key")),
            (Some(c), None) => c.to_ascii_lowercase().to_string(),
            _ => match KEYSYM_CHARS.iter().find(|(name, _)| *name == rest) {
                Some((_, c)) => c.to_string(),
                None => rest.to_string(),
            },
        };
        Ok(chord)
    }

    /// The same key as the keymap names it.
    pub fn of(spec: &KeySpec) -> Chord {
        Chord {
            ctrl: spec.ctrl,
            alt: spec.meta,
            shift: spec.shift,
            key: match spec.key {
                KeyName::Char(c) if spec.ctrl => c.to_ascii_lowercase().to_string(),
                KeyName::Char(c) => c.to_string(),
                KeyName::F(n) => format!("F{n}"),
                other => row(other).4.to_string(),
            },
        }
    }

    /// `Ctrl+=`, `F11`.
    pub fn human(&self) -> String {
        let mut out = String::new();
        if self.ctrl {
            out.push_str("Ctrl+");
        }
        if self.alt {
            out.push_str("Alt+");
        }
        if self.shift {
            out.push_str("Shift+");
        }
        out.push_str(&self.key);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tmux key name the default table, the owner's snippet and the stock probe use.
    const NAMES: &[&str] = &[
        "C-b", "C-a", "%", "\"", "\\", "z", "C-Up", "C-Down", "C-Left", "C-Right", "M-Up", "M-Down", "M-Left",
        "M-Right", "Up", "Down", "Left", "Right", "{", "}", "M-1", "M-2", "x", "c", "n", "p", "l", "1", "9", ",", "&",
        "w", "i", "f", "r", "?", "C-l", "F11", "e", "a", "t", "v", "Space", "-", "|", "_", "H", "J", "K", "L", "q",
        "m", "h", "j", "k", "S-Up", "F1", "F24", "BSpace", "DC", "IC", "PPage", "NPage", "Enter", "Tab", "Home", "End",
        "C-M-x", "M-n", "C-o", "C-z",
    ];

    #[test]
    fn every_tmux_name_used_here_parses_and_prints_back_the_same() {
        for name in NAMES {
            let spec = KeySpec::parse(name).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(spec.to_string(), *name);
        }
    }

    #[test]
    fn ctrl_on_a_capital_is_the_lowercase_letter() {
        assert_eq!(KeySpec::parse("C-A").unwrap(), KeySpec::parse("C-a").unwrap());
        assert_eq!(
            KeySpec::parse("H").unwrap().key,
            KeyName::Char('H'),
            "a plain capital stays itself"
        );
    }

    #[test]
    fn a_key_that_does_not_parse_says_which_and_why() {
        for bad in ["", "S-h", "C-", "Foo", "F25", "F0", " ", "ab", "M-"] {
            let err = KeySpec::parse(bad).expect_err(bad);
            assert_eq!(err.key, bad);
            assert!(err.to_string().contains(&format!("{bad:?}")), "{err}");
        }
        assert!(KeySpec::parse("S-h").unwrap_err().why.contains("shifted character"));
    }

    #[test]
    fn human_spelling_is_what_the_overlay_and_the_toast_print() {
        for (tmux, human) in [
            ("C-b", "Ctrl+b"),
            ("M-1", "Alt+1"),
            ("C-Up", "Ctrl+Up"),
            ("S-Up", "Shift+Up"),
            ("PPage", "PageUp"),
            ("BSpace", "Backspace"),
            ("%", "%"),
            ("F11", "F11"),
            ("Space", "Space"),
        ] {
            assert_eq!(KeySpec::parse(tmux).unwrap().human(), human, "{tmux}");
        }
    }

    #[test]
    fn vim_notation_is_what_send_keys_hands_nvim() {
        for (tmux, vim) in [
            ("C-a", "<C-a>"),
            ("C-b", "<C-b>"),
            ("C-l", "<C-l>"),
            ("<", "<lt>"),
            ("\\", "<Bslash>"),
            ("|", "<Bar>"),
            ("x", "x"),
            ("Up", "<Up>"),
            ("M-1", "<M-1>"),
            ("Enter", "<CR>"),
            ("C-M-x", "<C-M-x>"),
            ("S-F11", "<S-F11>"),
        ] {
            assert_eq!(KeySpec::parse(tmux).unwrap().to_vim(), vim, "{tmux}");
        }
    }

    #[test]
    fn as_char_is_only_an_unmodified_character() {
        assert_eq!(KeySpec::parse("f").unwrap().as_char(), Some('f'));
        assert_eq!(KeySpec::parse("C-f").unwrap().as_char(), None);
        assert_eq!(KeySpec::parse("Up").unwrap().as_char(), None);
    }

    #[test]
    fn gtk_accelerators_read_as_chords_comparable_with_tmux_keys() {
        let c = Chord::from_gtk("<Control>equal").unwrap();
        assert_eq!(
            c,
            Chord {
                ctrl: true,
                alt: false,
                shift: false,
                key: "=".into()
            }
        );
        assert_eq!(
            Chord::from_gtk("<Primary>h").unwrap(),
            Chord::of(&KeySpec::parse("C-h").unwrap())
        );
        assert_eq!(
            Chord::from_gtk("<Control>H").unwrap(),
            Chord::of(&KeySpec::parse("C-h").unwrap())
        );
        assert_eq!(
            Chord::from_gtk("F11").unwrap(),
            Chord::of(&KeySpec::parse("F11").unwrap())
        );
        assert!(Chord::from_gtk("<Control><Shift>f").unwrap().shift);
        assert_eq!(Chord::from_gtk("<Control>KP_Add").unwrap().key, "KP_Add");
        assert!(Chord::from_gtk("<Control>").is_err(), "no key");
        assert!(Chord::from_gtk("<Super>x").is_err(), "a modifier neovibe does not read");
        assert_eq!(Chord::from_gtk("<Control>equal").unwrap().human(), "Ctrl+=");
    }
}
