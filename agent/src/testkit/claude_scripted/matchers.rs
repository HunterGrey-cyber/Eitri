//! What a request looks like to a script: the tail of the conversation, the model, the system text and the tools the
//! request declares.
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// The CLI announces the tools it defers by name, one per line, in a mid-conversation system message: a sentence that
/// ends with the text below, the names, and a blank line.
pub const ANNOUNCEMENT_OPEN: &str = "to load tool schemas before calling them:\n";
pub const ANNOUNCEMENT_CLOSE: &str = "\n\n";
/// The tool a model calls to load deferred tools, and the input it passes: `query` is `select:<names>` and
/// `max_results` is required by the tool's schema.
pub const PRELOAD_TOOL: &str = "ToolSearch";
pub fn preload_input(names: &[String]) -> Value {
    serde_json::json!({"query": format!("select:{}", names.join(",")), "max_results": 5})
}

/// Marker the CLI puts around task notifications.
const NOTIFICATION_TAG: &str = "<task-notification>";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Loaded,
    Deferred,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LastUser {
    Prompt,
    ToolResult,
    TaskNotification,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TaskNotification {
    pub contains: Option<String>,
}

/// All present conditions must hold.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Matcher {
    /// The request path without its query string.
    pub path: Option<String>,
    /// `main`, `small` or an exact model id.
    pub model: Option<String>,
    pub first_user_contains: Option<String>,
    pub last_user: Option<LastUser>,
    pub last_user_contains: Option<String>,
    pub tool_result_for: Option<String>,
    pub task_notification: Option<TaskNotification>,
    pub system_contains: Option<String>,
    pub has_tool: Option<String>,
    pub not: Option<Box<Matcher>>,
}

/// What a matcher needs to know besides the request.
pub struct Ctx<'a> {
    pub main_model: Option<&'a str>,
    pub small_model: Option<&'a str>,
    /// Census mode: `main` is any model id.
    pub census: bool,
    /// Captured values substituted into the matcher's strings.
    pub vars: &'a BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
enum Block {
    Text(String),
    ToolResult { id: String, text: String },
    Other,
}

#[derive(Clone, Debug)]
struct Msg {
    role: String,
    blocks: Vec<Block>,
}

/// A request body read for matching.
#[derive(Debug)]
pub struct View {
    path: String,
    model: String,
    system: String,
    messages: Vec<Msg>,
    tail_start: usize,
    declared: BTreeMap<String, Availability>,
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(text_of).collect::<Vec<_>>().join("\n"),
        Value::Object(map) => match map.get("text") {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

fn blocks_of(content: &Value) -> Vec<Block> {
    match content {
        Value::String(s) => vec![Block::Text(s.clone())],
        Value::Array(items) => items
            .iter()
            .map(|b| match b["type"].as_str() {
                Some("text") => Block::Text(b["text"].as_str().unwrap_or("").to_owned()),
                Some("tool_result") => Block::ToolResult {
                    id: b["tool_use_id"].as_str().unwrap_or("").to_owned(),
                    text: text_of(&b["content"]),
                },
                _ => Block::Other,
            })
            .collect(),
        _ => Vec::new(),
    }
}

impl View {
    /// `path` is the request target; a query string is dropped.
    pub fn new(path: &str, body: &Value) -> Self {
        let messages: Vec<Msg> = body["messages"]
            .as_array()
            .map(|all| {
                all.iter()
                    .map(|m| Msg {
                        role: m["role"].as_str().unwrap_or("").to_owned(),
                        blocks: blocks_of(&m["content"]),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let tail_start = messages
            .iter()
            .rposition(|m| m.role == "assistant")
            .map_or(0, |i| i + 1);
        let mut view = Self {
            path: path.split('?').next().unwrap_or("").to_owned(),
            model: body["model"].as_str().unwrap_or("").to_owned(),
            system: text_of(&body["system"]),
            messages,
            tail_start,
            declared: BTreeMap::new(),
        };
        view.declared = view.declared_tools(body);
        view
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn system_text(&self) -> String {
        let mut text = self.system.clone();
        for m in self.tail().iter().filter(|m| m.role == "system") {
            text.push('\n');
            text.push_str(&m.text());
        }
        text
    }

    fn tail(&self) -> &[Msg] {
        &self.messages[self.tail_start..]
    }

    /// Every text in the tail, of any role, tool results included.
    pub fn tail_text(&self) -> String {
        self.tail().iter().map(Msg::text).collect::<Vec<_>>().join("\n")
    }

    /// Text of the tail's user messages only: a prompt that asks for a compaction or a label sits there.
    pub fn tail_user_text(&self) -> String {
        self.tail()
            .iter()
            .filter(|m| m.role == "user")
            .map(Msg::text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Text of every message before the tail: what an exchange of several requests about one call shares when the
    /// requests differ in their closing prompt.
    pub fn context_key(&self) -> String {
        self.messages[..self.tail_start]
            .iter()
            .map(Msg::text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn first_user_text(&self) -> String {
        self.messages
            .iter()
            .find(|m| m.role == "user")
            .map(Msg::text)
            .unwrap_or_default()
    }

    pub fn tool_result_ids(&self) -> Vec<&str> {
        self.tail()
            .iter()
            .flat_map(|m| &m.blocks)
            .filter_map(|b| match b {
                Block::ToolResult { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect()
    }

    pub fn tool_result_text(&self, id: &str) -> Option<String> {
        self.tail().iter().flat_map(|m| &m.blocks).find_map(|b| match b {
            Block::ToolResult { id: i, text } if i == id => Some(text.clone()),
            _ => None,
        })
    }

    /// Kind of the tail's last conversational block. A text that is only a system reminder does not count unless
    /// it carries a task notification.
    pub fn last_user_kind(&self) -> Option<LastUser> {
        for block in self.tail().iter().flat_map(|m| &m.blocks).rev() {
            match block {
                Block::ToolResult { .. } => return Some(LastUser::ToolResult),
                Block::Text(t) if t.contains(NOTIFICATION_TAG) => return Some(LastUser::TaskNotification),
                Block::Text(t) if t.trim_start().starts_with("<system-reminder>") => continue,
                Block::Text(_) => return Some(LastUser::Prompt),
                Block::Other => continue,
            }
        }
        None
    }

    pub fn declared(&self) -> &BTreeMap<String, Availability> {
        &self.declared
    }

    /// The tools a request offers: a `tools[]` entry with a schema is loaded (the CLI lists a tool a `ToolSearch`
    /// result referenced there with `defer_loading` set, and the model can call it), one without a schema is
    /// deferred, and a name only in the announcement of deferred tools is deferred. A name offered both ways is
    /// loaded.
    fn declared_tools(&self, body: &Value) -> BTreeMap<String, Availability> {
        let mut out = BTreeMap::new();
        let mut put = |name: &str, how: Availability| {
            let slot = out.entry(name.to_owned()).or_insert(how);
            if how == Availability::Loaded {
                *slot = how;
            }
        };
        for tool in body["tools"].as_array().into_iter().flatten() {
            let Some(name) = tool["name"].as_str() else { continue };
            let deferred = tool.get("input_schema").is_none();
            put(
                name,
                if deferred {
                    Availability::Deferred
                } else {
                    Availability::Loaded
                },
            );
        }
        let mut texts = vec![self.system.clone()];
        texts.extend(self.messages.iter().flat_map(|m| &m.blocks).filter_map(|b| match b {
            Block::Text(t) => Some(t.clone()),
            _ => None,
        }));
        for text in texts {
            let mut rest = text.as_str();
            while let Some(start) = rest.find(ANNOUNCEMENT_OPEN) {
                let after = &rest[start + ANNOUNCEMENT_OPEN.len()..];
                let end = after.find(ANNOUNCEMENT_CLOSE).unwrap_or(after.len());
                for line in after[..end].lines() {
                    let name = line.trim().trim_start_matches("- ").trim();
                    if !name.is_empty() {
                        put(name, Availability::Deferred);
                    }
                }
                rest = &after[end..];
            }
        }
        out
    }
}

impl Msg {
    fn text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.as_str()),
                Block::ToolResult { text, .. } => Some(text.as_str()),
                Block::Other => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Replace `{name}` with a captured value; a name without a value stays as written.
pub fn subst_vars(text: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let tail = &rest[open + 1..];
        match tail.find('}') {
            Some(close) if vars.contains_key(&tail[..close]) => {
                out.push_str(&vars[&tail[..close]]);
                rest = &tail[close + 1..];
            }
            _ => {
                out.push('{');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

impl Matcher {
    pub fn matches(&self, view: &View, ctx: &Ctx) -> bool {
        let sub = |s: &str| subst_vars(s, ctx.vars);
        if let Some(path) = &self.path {
            if view.path != sub(path) {
                return false;
            }
        }
        if let Some(model) = &self.model {
            let ok = match model.as_str() {
                "main" => {
                    ctx.census
                        || (ctx.small_model != Some(view.model()) && ctx.main_model.is_none_or(|m| m == view.model()))
                }
                "small" => ctx.small_model == Some(view.model()),
                exact => exact == view.model(),
            };
            if !ok {
                return false;
            }
        }
        if let Some(text) = &self.first_user_contains {
            if !view.first_user_text().contains(&sub(text)) {
                return false;
            }
        }
        if let Some(kind) = self.last_user {
            if view.last_user_kind() != Some(kind) {
                return false;
            }
        }
        if let Some(text) = &self.last_user_contains {
            if !view.tail_text().contains(&sub(text)) {
                return false;
            }
        }
        if let Some(id) = &self.tool_result_for {
            let id = sub(id);
            if !view.tool_result_ids().contains(&id.as_str()) {
                return false;
            }
        }
        if let Some(n) = &self.task_notification {
            let tail = view.tail_text();
            if !tail.contains(NOTIFICATION_TAG) {
                return false;
            }
            if let Some(text) = &n.contains {
                if !tail.contains(&sub(text)) {
                    return false;
                }
            }
        }
        if let Some(text) = &self.system_contains {
            if !view.system_text().contains(&sub(text)) {
                return false;
            }
        }
        if let Some(tool) = &self.has_tool {
            if !view.declared.contains_key(&sub(tool)) {
                return false;
            }
        }
        if let Some(inner) = &self.not {
            if inner.matches(view, ctx) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vars() -> BTreeMap<String, String> {
        BTreeMap::new()
    }
    fn ctx(vars: &BTreeMap<String, String>) -> Ctx<'_> {
        Ctx {
            main_model: Some("m-main"),
            small_model: Some("m-small"),
            census: false,
            vars,
        }
    }
    fn matcher(v: Value) -> Matcher {
        serde_json::from_value(v).unwrap()
    }
    fn body() -> Value {
        json!({
            "model": "m-main",
            "system": [{"type": "text", "text": "You are a helper. Be brief."}],
            "tools": [
                {"name": "Bash", "input_schema": {"type": "object"}},
                {"name": "Monitor", "defer_loading": true, "input_schema": {"type": "object"}},
            ],
            "messages": [
                {"role": "user", "content": "C01-START do the thing"},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_old", "name": "Bash", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_old", "content": "old output"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
                {"role": "system", "content": "mid system note"},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_bg1", "content": [{"type": "text", "text": "Command running in background with ID: bx1."}]},
                    {"type": "text", "text": "<system-reminder>\nremember\n</system-reminder>"},
                ]},
            ]
        })
    }

    #[test]
    fn every_matcher_is_true_and_false_over_a_mixed_role_tail() {
        let v = View::new("/v1/messages?beta=true", &body());
        let vars = vars();
        let c = ctx(&vars);
        let cases = [
            (
                json!({"path": "/v1/messages"}),
                json!({"path": "/v1/messages/count_tokens"}),
            ),
            (json!({"model": "main"}), json!({"model": "small"})),
            (json!({"model": "m-main"}), json!({"model": "m-other"})),
            (
                json!({"first_user_contains": "C01-START"}),
                json!({"first_user_contains": "old output"}),
            ),
            (json!({"last_user": "tool_result"}), json!({"last_user": "prompt"})),
            (
                json!({"last_user_contains": "mid system note"}),
                json!({"last_user_contains": "old output"}),
            ),
            (
                json!({"tool_result_for": "toolu_bg1"}),
                json!({"tool_result_for": "toolu_old"}),
            ),
            (
                json!({"system_contains": "Be brief"}),
                json!({"system_contains": "C01-START"}),
            ),
            (
                json!({"system_contains": "mid system note"}),
                json!({"system_contains": "nothing"}),
            ),
            (json!({"has_tool": "Bash"}), json!({"has_tool": "Missing"})),
            (json!({"has_tool": "Monitor"}), json!({"has_tool": "AskUserQuestion"})),
        ];
        for (yes, no) in cases {
            assert!(matcher(yes.clone()).matches(&v, &c), "{yes} should match");
            assert!(!matcher(no.clone()).matches(&v, &c), "{no} should not match");
        }
        assert!(!matcher(json!({"task_notification": {}})).matches(&v, &c));
    }

    #[test]
    fn task_notifications_match_in_any_role_of_the_tail() {
        let mut b = body();
        b["messages"] = json!([
            {"role": "user", "content": "start"},
            {"role": "assistant", "content": "STARTED"},
            {"role": "system", "content": "<task-notification>done bx1</task-notification>"},
        ]);
        let v = View::new("/v1/messages", &b);
        let mut vars = vars();
        vars.insert("bg1".into(), "bx1".into());
        let c = ctx(&vars);
        assert!(matcher(json!({"task_notification": {}})).matches(&v, &c));
        assert!(matcher(json!({"task_notification": {"contains": "{bg1}"}})).matches(&v, &c));
        assert!(!matcher(json!({"task_notification": {"contains": "bx2"}})).matches(&v, &c));
        assert_eq!(v.last_user_kind(), Some(LastUser::TaskNotification));
        assert!(matcher(json!({"last_user": "task_notification"})).matches(&v, &c));

        let mut b2 = body();
        b2["messages"] = json!([
            {"role": "user", "content": "start"},
            {"role": "assistant", "content": "STARTED"},
            {"role": "user", "content": [{"type": "text", "text": "<system-reminder>\n<task-notification>x</task-notification>\n</system-reminder>"}]},
        ]);
        assert_eq!(
            View::new("/v1/messages", &b2).last_user_kind(),
            Some(LastUser::TaskNotification)
        );
    }

    #[test]
    fn not_nests_and_all_conditions_must_hold() {
        let v = View::new("/v1/messages", &body());
        let vars = vars();
        let c = ctx(&vars);
        assert!(matcher(json!({"not": {"has_tool": "Missing"}})).matches(&v, &c));
        assert!(!matcher(json!({"not": {"has_tool": "Bash"}})).matches(&v, &c));
        assert!(matcher(json!({"not": {"not": {"has_tool": "Bash"}}})).matches(&v, &c));
        assert!(
            matcher(json!({"has_tool": "Bash", "last_user": "tool_result", "not": {"model": "small"}})).matches(&v, &c)
        );
        assert!(!matcher(json!({"has_tool": "Bash", "last_user": "prompt"})).matches(&v, &c));
        assert!(matcher(json!({})).matches(&v, &c));
    }

    #[test]
    fn main_model_is_any_id_in_census_mode_only() {
        let b = json!({"model": "m-unknown", "messages": [{"role": "user", "content": "hi"}]});
        let v = View::new("/v1/messages", &b);
        let vars = vars();
        let mut c = ctx(&vars);
        let m = matcher(json!({"model": "main"}));
        assert!(!m.matches(&v, &c));
        c.census = true;
        assert!(m.matches(&v, &c));
        c.census = false;
        c.main_model = None;
        assert!(m.matches(&v, &c));
        c.small_model = Some("m-unknown");
        assert!(!m.matches(&v, &c));
    }

    #[test]
    fn unknown_matcher_fields_are_refused() {
        assert!(serde_json::from_value::<Matcher>(json!({"bogus": 1})).is_err());
    }

    #[test]
    fn the_declared_set_reads_each_of_the_three_forms() {
        let b = json!({
            "model": "m",
            "system": "Reminder. Use ToolSearch with query \"select:<name>[,<name>...]\" to load tool schemas before calling them:\nTaskStop\n- CronCreate\n\nMore text.",
            "tools": [
                {"name": "Bash", "input_schema": {"type": "object"}},
                {"name": "Monitor", "defer_loading": true, "input_schema": {"type": "object"}},
                {"name": "ListAgents"},
            ],
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "Their schemas are NOT loaded. Use ToolSearch with query \"select:<name>\" to load tool schemas before calling them:\nSendMessage\nBash"}]}],
        });
        let d = View::new("/v1/messages", &b).declared().clone();
        let expected = [
            ("Bash", Availability::Loaded),
            ("Monitor", Availability::Loaded),
            ("ListAgents", Availability::Deferred),
            ("TaskStop", Availability::Deferred),
            ("CronCreate", Availability::Deferred),
            ("SendMessage", Availability::Deferred),
        ];
        assert_eq!(d.len(), expected.len(), "{d:?}");
        for (name, how) in expected {
            assert_eq!(d.get(name), Some(&how), "{name}");
        }
    }

    #[test]
    fn variables_substitute_only_when_captured() {
        let mut vars = vars();
        vars.insert("a".into(), "1".into());
        assert_eq!(subst_vars("x {a} {b} {{a}} {", &vars), "x 1 {b} {1} {");
    }
}
