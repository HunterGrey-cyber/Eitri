//! Hyprland's `hyprctl binds -j` reply as chords. The reply is a JSON array with one object per bind; the fields
//! read are `modmask` (a bit set: Shift 1, Caps 2, Ctrl 4, Alt 8, Num 16, Mod3 32, Super 64, Mod5 128), `key`,
//! `submap` (empty for the default), `submap_universal` (a string, `"true"` or `"false"`), `non_consuming`, `mouse`
//! and `keycode`, as `hyprctl binds -j` writes them
//! (field names and bit values from Hyprland's `src/ipc/s1/Commands.cpp`, `bindsRequest`, read on 2026-10-30; the
//! older `src/debug/HyprCtl.cpp` wrote the same fields).

use serde_json::Value;

use super::chord_of_parts;
use crate::keymap::Chord;

/// A set flag. `bindsRequest` writes `submap_universal` as a string (`"true"`) and the other flags as booleans; a
/// boolean in `submap_universal` and a string elsewhere are read the same.
fn flag(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(set)) => *set,
        Some(Value::String(text)) => text == "true",
        _ => false,
    }
}

const SHIFT: u64 = 1;
const CTRL: u64 = 4;
const ALT: u64 = 8;

/// The chords the default submap binds with Ctrl, Alt and Shift as the only modifiers, with the bind's key text. A
/// mouse bind, a keycode bind, a bind in another submap (unless it applies in every submap), a non-consuming bind
/// (the press goes on to the window too) or one holding Super (or any other bit) is not a candidate.
pub fn hyprland_chords(binds_json: &str) -> Result<Vec<(Chord, String)>, String> {
    let reply: Value = serde_json::from_str(binds_json).map_err(|e| format!("hyprctl binds: {e}"))?;
    let binds = reply
        .as_array()
        .ok_or_else(|| "hyprctl binds: expected an array".to_string())?;
    let mut out = Vec::new();
    for bind in binds {
        if bind.get("mouse").and_then(Value::as_bool) == Some(true)
            || (bind
                .get("submap")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
                && !flag(bind.get("submap_universal")))
            || flag(bind.get("non_consuming"))
            || bind.get("keycode").and_then(Value::as_i64).is_some_and(|code| code > 0)
        {
            continue;
        }
        let (Some(mask), Some(key)) = (
            bind.get("modmask").and_then(Value::as_u64),
            bind.get("key").and_then(Value::as_str),
        ) else {
            continue;
        };
        if mask & !(SHIFT | CTRL | ALT) != 0 {
            continue;
        }
        if let Some(chord) = chord_of_parts(mask & CTRL != 0, mask & ALT != 0, mask & SHIFT != 0, key) {
            out.push((chord, key.to_string()));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The shape `bindsRequest` writes, one object per bind (`submap_universal` a string; one object has it as a
    // boolean, which is read too).
    const REPLY: &str = r#"[
        {"locked":false,"mouse":false,"release":false,"repeat":false,"longPress":false,"non_consuming":false,
         "has_description":false,"modmask":12,"submap":"","key":"T","keycode":0,"catch_all":false,
         "description":"","dispatcher":"exec","arg":"foot"},
        {"locked":false,"mouse":false,"modmask":64,"submap":"","key":"Return","keycode":0,"dispatcher":"exec","arg":"foot"},
        {"locked":false,"mouse":false,"modmask":8,"submap":"","key":"space","keycode":0,"dispatcher":"exec","arg":"menu"},
        {"locked":false,"mouse":false,"modmask":8,"submap":"resize","key":"h","keycode":0,"dispatcher":"resizeactive","arg":"-10 0"},
        {"locked":false,"mouse":true,"modmask":8,"submap":"","key":"mouse:272","keycode":0,"dispatcher":"movewindow","arg":""},
        {"locked":false,"mouse":false,"modmask":72,"submap":"","key":"q","keycode":0,"dispatcher":"killactive","arg":""},
        {"locked":false,"mouse":false,"modmask":9,"submap":"","key":"x","keycode":0,"dispatcher":"exec","arg":"x"},
        {"locked":false,"mouse":false,"modmask":8,"submap":"","key":"","keycode":38,"dispatcher":"exec","arg":"y"},
        {"locked":false,"mouse":false,"non_consuming":true,"modmask":8,"submap":"","key":"n","keycode":0,"dispatcher":"exec","arg":"n"},
        {"locked":false,"mouse":false,"modmask":4,"submap":"resize","submap_universal":"false","key":"j","keycode":0,"dispatcher":"exec","arg":"j"},
        {"locked":false,"mouse":false,"modmask":4,"submap":"resize","submap_universal":true,"key":"k","keycode":0,"dispatcher":"exec","arg":"k"},
        {"locked":false,"mouse":false,"modmask":4,"submap":"resize","submap_universal":"true","key":"g","keycode":0,"dispatcher":"exec","arg":"z"}
    ]"#;

    #[test]
    fn ctrl_alt_and_alt_binds_are_chords_and_super_ones_are_not() {
        let chords: Vec<Chord> = hyprland_chords(REPLY).unwrap().into_iter().map(|(c, _)| c).collect();
        let expected = [
            "<Control><Alt>t",
            "<Alt>space",
            "<Alt><Shift>x",
            "<Control>k",
            "<Control>g",
        ]
        .map(|a| super::super::checked(Chord::from_accel(a).unwrap()).unwrap());
        assert_eq!(chords, expected);
    }

    #[test]
    fn a_reply_that_is_not_an_array_is_an_error() {
        assert!(hyprland_chords("{}").is_err());
        assert!(hyprland_chords("nonsense").is_err());
    }
}
