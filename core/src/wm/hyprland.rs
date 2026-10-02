//! Hyprland's `hyprctl clients -j`: a flat array. `focusHistoryID` 0 is the focused window.

use super::{Rect, WindowRef};
use serde_json::Value;

pub fn parse_windows(json: &str) -> Result<Vec<WindowRef>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("hyprctl clients: {e}"))?;
    let list = v
        .as_array()
        .ok_or_else(|| "hyprctl clients: expected an array".to_string())?;
    let mut out = Vec::new();
    for c in list {
        // A window that is not mapped cannot be focused. A hidden one (a group's inactive member)
        // can: focusing it brings it forward in its group.
        if c.get("mapped").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        let Some(pid) = c.get("pid").and_then(Value::as_u64).and_then(|p| u32::try_from(p).ok()) else {
            continue;
        };
        out.push(WindowRef {
            pid,
            id: None,
            focused: c.get("focusHistoryID").and_then(Value::as_i64) == Some(0),
            visible: c.get("hidden").and_then(Value::as_bool) != Some(true),
            rect: pair(c.get("at"))
                .zip(pair(c.get("size")))
                .map(|((x, y), (w, h))| Rect { x, y, w, h }),
        });
    }
    Ok(out)
}

fn pair(v: Option<&Value>) -> Option<(i64, i64)> {
    let a = v?.as_array()?;
    Some((a.first()?.as_i64()?, a.get(1)?.as_i64()?))
}
