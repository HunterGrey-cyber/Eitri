//! sway's `get_config` reply as chords. The reply is the main config file only: binds in an `include`d file are
//! not in it, so they are not seen. Only the default mode's `bindsym` lines count (also those written in a
//! `bindsym [flags] { ... }` block or a `mode "default" { ... }` one, and less any `unbindsym`) -- a mode's keys apply
//! after the user entered it on purpose, and a `bar` block's `bindsym` is about the bar's buttons -- a `--release`
//! bind runs when the key goes up and passes the press on, and `bindcode` lines are left out (a key code names no
//! key here).

use serde_json::Value;

use super::chord_of_parts;
use crate::keymap::Chord;

/// The chords sway's default mode binds with Ctrl, Alt and Shift as the only modifiers, with the bind's own key
/// text.
pub fn sway_chords(get_config_json: &str) -> Result<Vec<(Chord, String)>, String> {
    let reply: Value = serde_json::from_str(get_config_json).map_err(|e| format!("sway get_config: {e}"))?;
    let text = reply
        .get("config")
        .and_then(Value::as_str)
        .ok_or_else(|| "sway get_config: the reply holds no config text".to_string())?;
    Ok(config_chords(text))
}

/// Config lines with a trailing backslash joined to the next one, as sway reads them.
fn logical_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut open = String::new();
    for raw in text.lines() {
        match raw.strip_suffix('\\') {
            Some(head) => {
                open.push_str(head);
                open.push(' ');
            }
            None => {
                open.push_str(raw);
                out.push(std::mem::take(&mut open));
            }
        }
    }
    if !open.is_empty() {
        out.push(open);
    }
    out
}

/// Variables kept, the longest value of one and the longest line after substitution: a config far beyond what a
/// person writes is read no further, so a reply cannot make the expansion grow without bound.
const VARIABLE_LIMIT: usize = 256;
const VALUE_LIMIT: usize = 4096;
const LINE_LIMIT: usize = 8192;

/// What a `{` opens, as sway reads it: every line inside is the block's head and then the line.
enum Block {
    /// `mode "default" {`: its lines are the default mode's own.
    DefaultMode,
    /// `bindsym [flags] {`: each line is a bind carrying these flags.
    Binds(String),
    /// A mode, a `bar` or anything else: not the default mode's binds.
    Other,
}

fn classify(head: &str) -> Block {
    let mut words = head.split_whitespace();
    match words.next() {
        Some("bindsym") => Block::Binds(words.collect::<Vec<_>>().join(" ")),
        Some("mode") => {
            let name = words.find(|word| !word.starts_with("--")).unwrap_or("");
            if words.next().is_none() && name.trim_matches(['"', '\'']) == "default" {
                Block::DefaultMode
            } else {
                Block::Other
            }
        }
        _ => Block::Other,
    }
}

/// A bind taken: its chord, the combo as written, and the flags it carries (sorted, `--to-code` left out, which
/// changes how the key is named and not which bind it is).
struct Bind {
    chord: Chord,
    combo: String,
    flags: Vec<String>,
}

fn config_chords(text: &str) -> Vec<(Chord, String)> {
    let mut vars: Vec<(String, String)> = Vec::new();
    let mut stack: Vec<Block> = Vec::new();
    let mut previous = String::new();
    let mut out: Vec<Bind> = Vec::new();
    for line in logical_lines(text) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let head = if line == "{" {
            Some(previous.as_str())
        } else {
            line.strip_suffix('{')
        };
        if let Some(head) = head {
            let in_use = stack.iter().all(|block| !matches!(block, Block::Other));
            stack.push(if in_use { classify(head) } else { Block::Other });
            continue;
        }
        previous = line.to_string();
        if line == "}" {
            stack.pop();
            continue;
        }
        if line.len() > LINE_LIMIT || stack.iter().any(|block| matches!(block, Block::Other)) {
            continue;
        }
        if let Some(Block::Binds(flags)) = stack.last() {
            take_bind(&format!("{flags} {line}"), &vars, &mut out);
            continue;
        }
        let mut words = line.splitn(2, char::is_whitespace);
        match (words.next(), words.next()) {
            (Some("set"), Some(rest)) => {
                let mut parts = rest.trim_start().splitn(2, char::is_whitespace);
                if let (Some(name), Some(value)) = (parts.next(), parts.next()) {
                    if name.starts_with('$') {
                        vars.retain(|(n, _)| n != name);
                        match expand(value.trim(), &vars, VALUE_LIMIT) {
                            Some(value) if vars.len() < VARIABLE_LIMIT => vars.push((name.to_string(), value)),
                            _ => {}
                        }
                    }
                }
            }
            (Some("bindsym"), Some(rest)) => take_bind(rest, &vars, &mut out),
            (Some("unbindsym"), Some(rest)) => {
                if let Some(gone) = parse_bind(rest, &vars) {
                    out.retain(|bind| !(bind.chord.folded() == gone.chord.folded() && bind.flags == gone.flags));
                }
            }
            _ => {}
        }
    }
    out.into_iter().map(|bind| (bind.chord, bind.combo)).collect()
}

/// The bind in `rest` (a `bindsym` line after the command word) added to `out`, unless it is one that does not take
/// the press from the focused window: a `--release` bind runs when the key goes up and the press is passed on.
fn take_bind(rest: &str, vars: &[(String, String)], out: &mut Vec<Bind>) {
    if let Some(bind) = parse_bind(rest, vars) {
        if !bind.flags.iter().any(|flag| flag == "--release") {
            out.push(bind);
        }
    }
}

fn parse_bind(rest: &str, vars: &[(String, String)]) -> Option<Bind> {
    let rest = expand(rest, vars, LINE_LIMIT)?;
    let mut flags: Vec<String> = rest
        .split_whitespace()
        .take_while(|word| word.starts_with("--"))
        .filter(|word| *word != "--to-code")
        .map(str::to_string)
        .collect();
    flags.sort();
    let combo = rest.split_whitespace().find(|word| !word.starts_with("--"))?;
    Some(Bind {
        chord: combo_chord(combo)?,
        combo: combo.to_string(),
        flags,
    })
}

/// `text` with every known `$variable` replaced in one pass, the longest name first so `$mod2` is not read as `$mod`
/// and a `2`; `None` when the result would pass `limit` bytes.
fn expand(text: &str, vars: &[(String, String)], limit: usize) -> Option<String> {
    let mut ordered: Vec<&(String, String)> = vars.iter().collect();
    ordered.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        match ordered.iter().find(|(name, _)| rest.starts_with(name.as_str())) {
            Some((name, value)) => {
                out.push_str(value);
                rest = &rest[name.len()..];
            }
            None => {
                out.push('$');
                rest = &rest[1..];
            }
        }
        if out.len() > limit {
            return None;
        }
    }
    out.push_str(rest);
    (out.len() <= limit).then_some(out)
}

/// `Ctrl+Alt+t`, `Mod1+Shift+x`: the chord, or `None` for a combo with a modifier that is not Shift, Ctrl or Alt
/// (Super, Mod2, Mod3, Mod5, Lock, an unset variable) or with no key.
fn combo_chord(combo: &str) -> Option<Chord> {
    let (key, mods) = combo.rsplit_once('+').map_or((combo, ""), |(mods, key)| (key, mods));
    let (mut ctrl, mut alt, mut shift) = (false, false, false);
    for name in mods.split('+').filter(|m| !m.is_empty()) {
        match name.to_ascii_lowercase().as_str() {
            "shift" => shift = true,
            "ctrl" | "control" => ctrl = true,
            "alt" | "mod1" => alt = true,
            _ => return None,
        }
    }
    chord_of_parts(ctrl, alt, shift, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(config: &str) -> String {
        serde_json::json!({ "config": config }).to_string()
    }

    /// The chords as `Ctrl+Alt+t`, a space bar as `space`.
    fn human(chords: Vec<(Chord, String)>) -> Vec<String> {
        chords.into_iter().map(|(c, _)| c.human()).collect()
    }

    #[test]
    fn variables_are_substituted_and_super_binds_are_not_candidates() {
        let config = "set $mod Mod4\nbindsym Ctrl+Alt+t exec foot\nbindsym $mod+Return exec foot\n\
                      bindsym --release Mod1+x exec thing\nbindcode 36 exec foot\nbindsym Mod1+y exec bar\n";
        assert_eq!(
            human(sway_chords(&reply(config)).unwrap()),
            ["Ctrl+Alt+t", "Alt+y"],
            "a Super bind, a bindcode line and a release bind (the press goes to the window) are left out"
        );
    }

    #[test]
    fn the_binds_own_key_text_is_kept_for_the_log() {
        let chords = sway_chords(&reply("set $mod Mod1\nbindsym $mod+Shift+e exit\n")).unwrap();
        assert_eq!(chords.len(), 1);
        assert_eq!(chords[0].1, "Mod1+Shift+e");
    }

    #[test]
    fn a_mode_and_a_bar_block_are_not_the_default_mode() {
        let config = "bindsym Mod1+a exec one\nmode \"resize\" {\n    bindsym Mod1+b exec two\n    bindsym Escape mode default\n}\n\
                      bar {\n    bindsym Mod1+c exec three\n}\nbindsym Mod1+d exec four\n";
        assert_eq!(human(sway_chords(&reply(config)).unwrap()), ["Alt+a", "Alt+d"]);
    }

    #[test]
    fn comments_blank_lines_and_continuations_are_read_as_sway_reads_them() {
        let config = "# bindsym Mod1+z exec no\n\nbindsym Mod1+q \\\n    kill\nbindsym Ctrl+space exec launcher\n";
        assert_eq!(human(sway_chords(&reply(config)).unwrap()), ["Alt+q", "Ctrl+space"]);
    }

    #[test]
    fn a_variable_named_inside_another_is_replaced_whole() {
        let config = "set $mod Mod1\nset $mod2 Ctrl\nbindsym $mod2+k exec a\nbindsym $mod+k exec b\n";
        assert_eq!(human(sway_chords(&reply(config)).unwrap()), ["Ctrl+k", "Alt+k"]);
    }

    #[test]
    fn a_reply_that_is_not_a_config_is_an_error() {
        assert!(sway_chords("not json").is_err());
        assert!(sway_chords("{\"other\": 1}").is_err());
        assert!(sway_chords("[]").is_err());
    }

    #[test]
    fn a_bindsym_block_and_a_default_mode_block_hold_default_mode_binds() {
        let config = "bindsym {\n    Mod1+a exec one\n    Mod4+b exec two\n}\n\
                      bindsym --to-code {\n    Mod1+c exec three\n}\n\
                      bindsym --release {\n    Mod1+r exec up\n}\n\
                      mode \"default\" {\n    bindsym Mod1+d exec four\n}\n\
                      mode \"resize\" {\n    bindsym Mod1+e exec five\n}\n\
                      mode \"default\"\n{\n    bindsym Mod1+f exec six\n}\n\
                      bindsym Mod1+g exec seven\n";
        assert_eq!(
            human(sway_chords(&reply(config)).unwrap()),
            ["Alt+a", "Alt+c", "Alt+d", "Alt+f", "Alt+g"]
        );
    }

    #[test]
    fn an_unbindsym_takes_back_the_bind_with_the_same_flags_only() {
        let config = "bindsym Mod1+x exec foot\nbindsym Mod1+y exec foot\nbindsym --locked Mod1+z exec foot\n\
                      unbindsym Mod1+x\nunbindsym --release Mod1+y\nunbindsym Mod1+z\n";
        assert_eq!(human(sway_chords(&reply(config)).unwrap()), ["Alt+y", "Alt+z"]);
    }

    #[test]
    fn a_reply_cannot_make_a_variable_grow_without_bound() {
        let mut config = String::from("set $v00 xxxxxxxx\n");
        for n in 1..40 {
            config.push_str(&format!("set $v{n:02} $v{p:02}$v{p:02}\n", p = n - 1));
        }
        config.push_str("bindsym Mod1+q exec foot\nbindsym $v39+q exec foot\n");
        assert_eq!(human(sway_chords(&reply(&config)).unwrap()), ["Alt+q"]);
        let wide = format!(
            "set $a {}\nbindsym Mod1+w exec {}\n",
            "y".repeat(3000),
            "$a".repeat(10_000)
        );
        assert_eq!(human(sway_chords(&reply(&wide)).unwrap()), Vec::<String>::new());
    }
}
