//! Turning one nvim mapping into a tab action (spec §3.4), and one nvim `lhs` into the panel's own
//! key sequence (spec §3.5). Pure and unit-tested here; precedence against the defaults and
//! `init.lua` (`effective()`, spec §3.6) is Task 7, not this file.

use super::NvimMap;
use crate::keymap::{PanelKey, PanelSeq, TabAction};

/// `:bn[ext]` and friends (nvim: windows.txt, editing.txt `:enew`): at least the minimal prefix, at
/// most the full name, case-sensitive (`bN` is `:bNext`, previous).
const COMMANDS: &[(&str, usize, TabAction)] = &[
    ("bnext", 2, TabAction::Next),
    ("bNext", 2, TabAction::Prev),
    ("bprevious", 2, TabAction::Prev),
    ("bdelete", 2, TabAction::Close),
    ("bwipeout", 2, TabAction::Close),
    ("enew", 3, TabAction::New),
];

/// `%bd|e#`-style: a vim idiom older than any plugin (wipe every buffer, then reopen the
/// alternate) -- LazyVim's own "Delete Other Buffers" runs Snacks.bufdelete instead, a callback
/// matched by `desc` below, but a hand-written mapping often spells it this way (panel round 2
/// plan's Owner answers Q2).
fn close_others_idiom(word: &str) -> bool {
    let Some(rest) = word.strip_prefix('%') else {
        return false;
    };
    let Some((cmd, _tail)) = rest.split_once('|') else {
        return false;
    };
    matches!(cmd.trim_end_matches('!'), "bd" | "bdelete" | "bw" | "bwipeout")
}

fn command_action(word: &str) -> Option<TabAction> {
    if close_others_idiom(word) {
        return Some(TabAction::CloseOthers);
    }
    let word = word.trim_end_matches('!');
    for (full, min, action) in COMMANDS {
        if word.len() >= *min && full.starts_with(word) {
            return Some(*action);
        }
    }
    match word {
        // lazyvim: plugins/ui.lua:12-15 (bufferline); bufdelete.nvim
        "BufferLineCycleNext" => Some(TabAction::Next),
        "BufferLineCyclePrev" => Some(TabAction::Prev),
        "Bdelete" | "Bwipeout" => Some(TabAction::Close),
        // bufferline.nvim's own close-others command (panel round 2 plan's Owner answers Q2)
        "BufferLineCloseOthers" => Some(TabAction::CloseOthers),
        _ => None,
    }
}

/// The ex command an rhs runs, or `None` when it is not exactly one command.
fn command_word(rhs: &str) -> Option<&str> {
    let mut s = rhs.trim();
    for prefix in ["<Cmd>", "<cmd>", "<CMD>"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest;
            break;
        }
    }
    let s = s.trim_start_matches(':').trim();
    let s = s
        .strip_suffix("<CR>")
        .or_else(|| s.strip_suffix("<cr>"))
        .unwrap_or(s)
        .trim();
    let mut words = s.split_whitespace();
    let word = words.next()?;
    if words.next().is_some() || word.contains('<') {
        return None;
    }
    Some(word)
}

/// One nvim mapping to a tab action, or `None` when it is not one (spec §3.4). By `rhs` first, and
/// by `desc` only for a callback mapping (no `rhs`) -- an unrecognised mapping is dropped entirely,
/// never shadowing a default (spec §3.4 ruling R3).
pub fn classify(map: &NvimMap) -> Option<TabAction> {
    if let Some(rhs) = map.rhs.as_deref().filter(|r| !r.is_empty()) {
        return command_word(rhs).and_then(command_action);
    }
    let desc = map.desc.as_deref()?.trim();
    if desc.starts_with(':') {
        return command_word(desc).and_then(command_action);
    }
    // LazyVim's own words (lazyvim: config/keymaps.lua:34-49, 98, 43-45)
    match desc.to_lowercase().as_str() {
        "next buffer" => Some(TabAction::Next),
        "prev buffer" | "previous buffer" => Some(TabAction::Prev),
        "delete buffer" => Some(TabAction::Close),
        "delete other buffers" => Some(TabAction::CloseOthers),
        "new file" => Some(TabAction::New),
        _ => None,
    }
}

/// One key as nvim's `<>` notation names it (nvim: intro.txt), before deciding whether it is the
/// panel's leader: a bracket's lowercased name, or a bare character.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RawToken {
    Bracket(String),
    Char(char),
}

fn tokenize(text: &str) -> Result<Vec<RawToken>, String> {
    let mut tokens = Vec::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if c == '<' {
            let end = rest.find('>').ok_or_else(|| format!("{text:?}: an unclosed <"))?;
            let name = &rest[1..end];
            tokens.push(RawToken::Bracket(name.to_ascii_lowercase()));
            rest = &rest[end + 1..];
        } else {
            tokens.push(RawToken::Char(c));
            rest = &rest[c.len_utf8()..];
        }
    }
    Ok(tokens)
}

/// A token to the panel's own key, or an error naming it: only `<Space>`/`<lt>`/`<Bslash>`/`<Bar>`
/// among brackets are recognised, and everything else -- a Ctrl/Alt chord, `<Tab>`, `<CR>`, an
/// arrow, an `F` key, a mouse button -- belongs to `shell`'s own accelerators, never to a panel
/// sequence (spec §3.5, the same rule `panel::parse_seq` holds).
fn token_to_key(token: &RawToken, text: &str) -> Result<PanelKey, String> {
    Ok(match token {
        RawToken::Bracket(name) => match name.as_str() {
            "space" => PanelKey::Space,
            "lt" => PanelKey::Char('<'),
            "bslash" => PanelKey::Char('\\'),
            "bar" => PanelKey::Char('|'),
            _ => {
                return Err(format!(
                    "{text:?}: <{name}> is not a key the panel can take (Ctrl/Alt chords and \
                     special keys belong to shell)"
                ))
            }
        },
        RawToken::Char(c) if *c == ' ' => PanelKey::Space,
        RawToken::Char(c) => PanelKey::Char(*c),
    })
}

/// nvim's `lhs` (already through `keytrans`, spec §3.1) into a panel sequence, substituting nvim's
/// own effective leader (`nvim_leader`, `nvim_leader_token`'s return) for [`PanelKey::Leader`] when
/// `lhs` starts with it (spec §3.5): a `\bd` in an nvim with no `mapleader` becomes `Space b d`
/// under the panel's own leader, the same binding either way.
pub fn translate_lhs(lhs: &str, nvim_leader: &str) -> Result<PanelSeq, String> {
    let lhs_tokens = tokenize(lhs)?;
    if lhs_tokens.is_empty() {
        return Err(format!("{lhs:?}: an empty key sequence"));
    }
    let leader_tokens = tokenize(nvim_leader)?;
    // nvim's leader is exactly one key; a multi-token nvim_leader (should not happen) never matches.
    let leader_token = (leader_tokens.len() == 1).then(|| &leader_tokens[0]);
    let mut keys = Vec::with_capacity(lhs_tokens.len());
    for (i, token) in lhs_tokens.iter().enumerate() {
        if i == 0 && leader_token == Some(token) {
            keys.push(PanelKey::Leader);
        } else {
            keys.push(token_to_key(token, lhs)?);
        }
    }
    Ok(PanelSeq(keys))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(lhs: &str, rhs: Option<&str>, desc: Option<&str>) -> NvimMap {
        NvimMap {
            lhs: lhs.into(),
            rhs: rhs.map(Into::into),
            desc: desc.map(Into::into),
            callback: rhs.is_none(),
        }
    }

    #[test]
    fn rhs_commands_match_by_vims_abbreviation_rule() {
        use TabAction::*;
        for (rhs, want) in [
            ("<Cmd>bnext<CR>", Some(Next)),
            ("<cmd>bn<cr>", Some(Next)),
            (":bnext<CR>", Some(Next)),
            ("<Cmd>bprevious<CR>", Some(Prev)),
            (":bp<CR>", Some(Prev)),
            (":bN<CR>", Some(Prev)),
            ("<Cmd>bNext<CR>", Some(Prev)),
            ("<Cmd>:bd<CR>", Some(Close)),
            (":bd!<CR>", Some(Close)),
            (":bw<CR>", Some(Close)),
            ("<Cmd>enew<CR>", Some(New)),
            (":ene<CR>", Some(New)),
            ("<Cmd>BufferLineCycleNext<CR>", Some(Next)),
            ("<Cmd>BufferLineCyclePrev<CR>", Some(Prev)),
            ("<Cmd>Bdelete<CR>", Some(Close)),
            (":b<CR>", None),
            (":bnexte<CR>", None),
            (":en<CR>", None),
            ("<Cmd>e #<CR>", None),
            ("<Cmd>bnext<CR>zz", None),
            ("^", None),
        ] {
            assert_eq!(classify(&map("x", Some(rhs), None)), want, "{rhs}");
        }
    }

    #[test]
    fn callbacks_match_by_desc() {
        use TabAction::*;
        // nvim 0.12's own defaults (probed with nvim --clean, 2026-09-26)
        assert_eq!(classify(&map("[b", None, Some(":bprevious"))), Some(Prev));
        assert_eq!(classify(&map("]b", None, Some(":bnext"))), Some(Next));
        // LazyVim keymaps.lua:40-42 (Snacks.bufdelete, a callback)
        assert_eq!(classify(&map(" bd", None, Some("Delete Buffer"))), Some(Close));
        assert_eq!(classify(&map("x", None, Some("next buffer"))), Some(Next));
        assert_eq!(classify(&map("x", None, Some("Previous Buffer"))), Some(Prev));
        // panel round 2 plan's Owner answers Q2: recognised, not dropped (LazyVim keymaps.lua:43-45)
        assert_eq!(
            classify(&map("x", None, Some("Delete Other Buffers"))),
            Some(CloseOthers)
        );
        assert_eq!(classify(&map("x", None, Some("Grep (Root Dir)"))), None);
        assert_eq!(classify(&map("x", None, None)), None);
    }

    #[test]
    fn close_others_is_also_recognised_by_rhs() {
        use TabAction::CloseOthers;
        for rhs in ["<Cmd>BufferLineCloseOthers<CR>", ":%bd|e#<CR>", ":%bdelete!|e#<CR>"] {
            assert_eq!(classify(&map("x", Some(rhs), None)), Some(CloseOthers), "{rhs}");
        }
    }

    #[test]
    fn lhs_under_nvims_leader_becomes_the_panels_leader() {
        let s = translate_lhs("<Space>bd", "<Space>").unwrap();
        assert_eq!(s.0, vec![PanelKey::Leader, PanelKey::Char('b'), PanelKey::Char('d')]);
        // mapleader unset: nvim wrote `\bd` (map.txt *mapleader*); the panel reads it as <leader>bd
        assert_eq!(translate_lhs("\\bd", "\\").unwrap().0[0], PanelKey::Leader);
        assert_eq!(
            translate_lhs(",n", ",").unwrap().0,
            vec![PanelKey::Leader, PanelKey::Char('n')]
        );
        assert_eq!(translate_lhs("H", "<Space>").unwrap().0, vec![PanelKey::Char('H')]);
        assert_eq!(
            translate_lhs("<lt>b", "<Space>").unwrap().0,
            vec![PanelKey::Char('<'), PanelKey::Char('b')]
        );
        for bad in ["<C-H>x", "<Tab>", "<M-n>", "<F2>", "<LeftMouse>"] {
            assert!(translate_lhs(bad, "<Space>").is_err(), "{bad}");
        }
    }

    #[test]
    fn a_report_parses_only_at_version_one() {
        let line = br#"{"v":1,"mapleader":"<Space>","timeoutlen":300,"timeout":true,
            "maps":[{"lhs":"H","rhs":"<Cmd>bprevious<CR>","desc":"Prev Buffer","callback":false}]}"#;
        let r = super::super::parse_report(line).unwrap();
        assert_eq!(
            (r.timeoutlen, r.maps.len(), super::super::nvim_leader_token(&r)),
            (300, 1, "<Space>")
        );
        assert!(super::super::parse_report(br#"{"v":2,"timeoutlen":1,"timeout":true,"maps":[]}"#).is_none());
        assert!(super::super::parse_report(b"{not json").is_none());
        let unset =
            super::super::parse_report(br#"{"v":1,"mapleader":null,"timeoutlen":1000,"timeout":true,"maps":[]}"#)
                .unwrap();
        assert_eq!(super::super::nvim_leader_token(&unset), "\\");
    }
}
