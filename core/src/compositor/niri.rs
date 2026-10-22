//! niri's `niri msg --json windows`: an array with `id`, `pid` (absent for a window whose client
//! is unknown), `is_focused`, and no geometry in the scrolling layout.

use super::WindowRef;
use serde_json::Value;

pub fn parse_windows(json: &str) -> Result<Vec<WindowRef>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("niri windows: {e}"))?;
    let list = v
        .as_array()
        .ok_or_else(|| "niri windows: expected an array".to_string())?;
    let mut out = Vec::new();
    for w in list {
        let Some(pid) = w.get("pid").and_then(Value::as_u64).and_then(|p| u32::try_from(p).ok()) else {
            continue;
        };
        out.push(WindowRef {
            pid,
            id: w.get("id").and_then(Value::as_u64),
            focused: w.get("is_focused").and_then(Value::as_bool).unwrap_or(false),
            // niri does not say; nothing reads it there, since only sway is asked about edges.
            visible: true,
            rect: None,
        });
    }
    Ok(out)
}
