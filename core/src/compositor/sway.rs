//! sway's `swaymsg -t get_tree`: a tree of `output`/`workspace`/`con`/`floating_con` nodes. A window
//! is a leaf (no children) that has a pid. One that is not on screen (another workspace, the
//! hidden half of a tabbed container) is kept but marked not visible: nothing can be moved to it by
//! direction, yet it can still be raised by its pid, which brings its workspace or tab forward.

use super::{Rect, WindowRef};
use serde_json::Value;

pub fn parse_windows(json: &str) -> Result<Vec<WindowRef>, String> {
    let root: Value = serde_json::from_str(json).map_err(|e| format!("sway get_tree: {e}"))?;
    if !root.is_object() {
        return Err("sway get_tree: expected an object".to_string());
    }
    let mut out = Vec::new();
    collect(&root, &mut out);
    Ok(out)
}

fn children<'a>(node: &'a Value, key: &str) -> &'a [Value] {
    node.get(key).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn collect(node: &Value, out: &mut Vec<WindowRef>) {
    let nodes = children(node, "nodes");
    let floating = children(node, "floating_nodes");
    if nodes.is_empty() && floating.is_empty() {
        let kind = node.get("type").and_then(Value::as_str);
        if matches!(kind, Some("con") | Some("floating_con")) {
            if let Some(pid) = node
                .get("pid")
                .and_then(Value::as_u64)
                .and_then(|p| u32::try_from(p).ok())
            {
                out.push(WindowRef {
                    pid,
                    id: node.get("id").and_then(Value::as_u64),
                    focused: node.get("focused").and_then(Value::as_bool).unwrap_or(false),
                    visible: node.get("visible").and_then(Value::as_bool) == Some(true),
                    rect: node.get("rect").and_then(rect),
                });
            }
        }
        return;
    }
    for child in nodes.iter().chain(floating) {
        collect(child, out);
    }
}

fn rect(v: &Value) -> Option<Rect> {
    Some(Rect {
        x: v.get("x")?.as_i64()?,
        y: v.get("y")?.as_i64()?,
        w: v.get("width")?.as_i64()?,
        h: v.get("height")?.as_i64()?,
    })
}
