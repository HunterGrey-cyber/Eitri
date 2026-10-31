//! Hyprland's `hyprctl clients -j`: a flat array. `focusHistoryID` 0 is the focused window.

use super::{Rect, WindowRef};
use serde_json::Value;

pub fn parse_windows(json: &str) -> Result<Vec<WindowRef>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("hyprctl clients: {e}"))?;
    let list = v
        .get("clients")
        .unwrap_or(&v)
        .as_array()
        .ok_or_else(|| "hyprctl clients: expected an array".to_string())?;
    let on_screen = on_screen_workspaces(&v);
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
            app_id: c.get("class").and_then(Value::as_str).map(str::to_owned),
            title: c.get("title").and_then(Value::as_str).map(str::to_owned),
            address: c.get("address").and_then(Value::as_str).map(str::to_owned),
            workspace: c
                .get("workspace")
                .and_then(|w| w.get("name"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            output: c
                .get("monitor")
                .and_then(|monitor| {
                    monitor
                        .as_str()
                        .or_else(|| monitor.get("name").and_then(Value::as_str))
                        .or_else(|| {
                            v.get("monitors")?
                                .as_array()?
                                .iter()
                                .find(|m| m.get("id") == Some(monitor))?
                                .get("name")?
                                .as_str()
                        })
                })
                .or_else(|| c.get("monitorName").and_then(Value::as_str))
                .map(str::to_owned),
            focused: c.get("focusHistoryID").and_then(Value::as_i64) == Some(0),
            visible: c.get("hidden").and_then(Value::as_bool) != Some(true) && on_screen.shows(c),
            rect: pair(c.get("at"))
                .zip(pair(c.get("size")))
                .map(|((x, y), (w, h))| Rect { x, y, w, h }),
        });
    }
    Ok(out)
}

/// The workspaces the monitors show now, when the reply carries `hyprctl monitors -j` with them. A client list alone
/// has every workspace's windows and does not say which are shown.
struct OnScreen(Vec<i64>);

impl OnScreen {
    fn shows(&self, client: &Value) -> bool {
        let workspace = client
            .get("workspace")
            .and_then(|w| w.get("id"))
            .and_then(Value::as_i64);
        self.0.is_empty() || workspace.is_none_or(|id| self.0.contains(&id))
    }
}

fn on_screen_workspaces(reply: &Value) -> OnScreen {
    let monitors = reply.get("monitors").and_then(Value::as_array);
    OnScreen(
        monitors
            .into_iter()
            .flatten()
            .flat_map(|monitor| {
                ["activeWorkspace", "specialWorkspace"]
                    .into_iter()
                    .filter_map(|key| monitor.get(key)?.get("id")?.as_i64())
            })
            .collect(),
    )
}

fn pair(v: Option<&Value>) -> Option<(i64, i64)> {
    let a = v?.as_array()?;
    Some((a.first()?.as_i64()?, a.get(1)?.as_i64()?))
}
