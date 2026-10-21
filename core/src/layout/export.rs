//! Lua source for rebuilding the window's workspaces at first launch.

use std::collections::BTreeSet;

use super::{workspace::MAIN, Con, ConLayout, Layout, ModuleKind, Node, Workspaces};

/// The layout of every workspace as `init.lua` calls, in the grammar `eitri.layout.default` reads.
///
/// Calls up to 120 columns stay on one line. Longer calls break after the shallowest table comma
/// that fits, preferring the last such comma; continuation lines start with two spaces.
pub fn export_lua(live: &Layout, workspaces: Option<&Workspaces>) -> String {
    let mut layouts = match workspaces {
        None => vec![(MAIN, live)],
        Some(ws) => ws
            .order()
            .iter()
            .filter_map(|name| {
                let layout = if name == ws.current() {
                    Some(live)
                } else {
                    ws.stored(name)
                };
                layout.map(|layout| (name.as_str(), layout))
            })
            .collect(),
    };
    layouts.sort_by_key(|(name, _)| *name != MAIN);
    let mut omitted = BTreeSet::new();
    let layouts: Vec<_> = layouts
        .into_iter()
        .map(|(name, layout)| {
            let mut layout = layout.clone();
            for id in layout.modules() {
                if id.kind() == ModuleKind::Agent && id.number() > 1 && layout.remove(&id).is_ok() {
                    omitted.insert(id);
                }
            }
            (name, layout)
        })
        .collect();
    let mut text = "-- The window's layout as init.lua calls, from eitri.layout.export().\n".to_string();
    if let Some(first) = omitted.first() {
        let ids = omitted.iter().map(|id| id.as_str()).collect::<Vec<_>>().join(", ");
        text.push_str(&format!(
            "-- Left out: {ids} (a chat view named here is removed at startup unless a saved tab holds it;\n\
             -- eitri.layout.rule({}, {{ ... }}) says where it opens).\n",
            quote(first.as_str())
        ));
    }
    for (name, layout) in &layouts {
        if !layout.hidden().is_empty() {
            let ids = layout
                .hidden()
                .iter()
                .map(|id| {
                    let quoted = quote(id.as_str());
                    quoted[1..quoted.len() - 1].to_string()
                })
                .collect::<Vec<_>>()
                .join(", ");
            let place = if *name == MAIN {
                "the window".to_string()
            } else {
                format!("workspace {}", quote(name))
            };
            text.push_str(&format!("-- Hidden in {place}, shown at a first launch: {ids}.\n"));
        }
    }
    let calls = layouts
        .iter()
        .map(|(name, layout)| {
            let mut tree = match layout.root().children.as_slice() {
                [only] => node(&only.node, None),
                _ => container(layout.root(), None),
            };
            if !layout.floats().is_empty() {
                let floats = layout
                    .floats()
                    .iter()
                    .map(|float| {
                        format!(
                            "{{ {}, cx = {}, cy = {}, w = {}, h = {}{} }}",
                            quote(float.id.as_str()),
                            number(float.rect.cx),
                            number(float.rect.cy),
                            float.rect.w,
                            float.rect.h,
                            if float.scratch { ", scratch = true" } else { "" },
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                tree.truncate(tree.len() - 2);
                tree.push_str(&format!(", floating = {{ {floats} }} }}"));
            }
            let call = if *name == MAIN {
                format!("eitri.layout.default({tree})")
            } else {
                format!("eitri.layout.workspace({}, {tree})", quote(name))
            };
            wrap(&call)
        })
        .collect::<Vec<_>>();
    text.push_str(&calls.join("\n\n"));
    if !calls.is_empty() {
        text.push('\n');
    }
    text
}

fn number(value: f32) -> String {
    let text = value.to_string();
    if text.contains('.') {
        text
    } else {
        format!("{text}.0")
    }
}

fn quote(value: &str) -> String {
    let mut text = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => text.push_str("\\\\"),
            '"' => text.push_str("\\\""),
            '\n' => text.push_str("\\n"),
            '\r' => text.push_str("\\r"),
            '\t' => text.push_str("\\t"),
            ch if ch.is_control() => {
                let mut bytes = [0; 4];
                for byte in ch.encode_utf8(&mut bytes).as_bytes() {
                    text.push_str(&format!("\\{byte:03}"));
                }
            }
            ch => text.push(ch),
        }
    }
    text.push('"');
    text
}

fn node(value: &Node, share: Option<f32>) -> String {
    match value {
        Node::Leaf(id) => {
            let share = share.map(|s| format!(", {}", number(s))).unwrap_or_default();
            format!("{{ {}{share} }}", quote(id.as_str()))
        }
        Node::Con(con) => container(con, share),
    }
}

fn container(con: &Con, share: Option<f32>) -> String {
    let head = match con.layout {
        ConLayout::SplitH => "row",
        ConLayout::SplitV => "column",
        ConLayout::Tabbed => "tabbed",
        ConLayout::Stacked => "stacked",
    };
    let mut fields = vec![quote(head)];
    for (i, child) in con.children.iter().enumerate() {
        fields.push(node(&child.node, (i + 1 < con.children.len()).then_some(child.share)));
    }
    if con.layout.axis().is_none() && con.focus != 0 {
        fields.push(format!("active = {}", con.focus + 1));
    }
    if let Some(share) = share {
        fields.push(format!("share = {}", number(share)));
    }
    format!("{{ {} }}", fields.join(", "))
}

fn wrap(call: &str) -> String {
    let mut commas = Vec::new();
    let (mut depth, mut quoted, mut escaped) = (0usize, false, false);
    for (i, ch) in call.char_indices() {
        if quoted {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = false;
            }
        } else {
            match ch {
                '"' => quoted = true,
                '{' => depth += 1,
                '}' => depth -= 1,
                ',' if depth > 0 => commas.push((i + 1, depth)),
                _ => {}
            }
        }
    }
    let mut text = String::new();
    let mut start = 0;
    let mut width = 120;
    while call[start..].chars().count() > width {
        let end = commas
            .iter()
            .filter(|(end, _)| *end > start && call[start..*end].chars().count() <= width)
            .min_by_key(|(end, depth)| (*depth, std::cmp::Reverse(*end)))
            .map(|(end, _)| *end);
        let Some(end) = end else { break };
        text.push_str(&call[start..end]);
        text.push_str("\n  ");
        start = end;
        while call.as_bytes().get(start) == Some(&b' ') {
            start += 1;
        }
        width = 118;
    }
    text.push_str(&call[start..]);
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{arrange, hide, FloatRect, Frame, ModuleId, Size};
    use crate::lua::layout::{parse_default, parse_tree};
    use mlua::{Lua, Table, Value};

    fn layout(source: &str) -> Layout {
        let lua = Lua::new();
        let value = lua.load(format!("return {source}")).eval::<Value>().unwrap();
        let tree = parse_tree(&value).unwrap();
        let focus = tree
            .tiled
            .as_con()
            .and_then(|con| con.focused_leaf(&|_| true))
            .unwrap_or_else(|| tree.tiled.leaves()[0].clone());
        Layout::from_parts_floating(tree.tiled, tree.floats, Default::default(), focus, Default::default()).unwrap()
    }

    fn capture(source: &str) -> (Lua, Table) {
        let lua = Lua::new();
        lua.load(
            r#"
            captured = {}
            eitri = { layout = {
                default = function(tree) captured.main = tree end,
                workspace = function(name, tree) captured[name] = tree end
            } }
        "#,
        )
        .exec()
        .unwrap();
        lua.load(source).exec().unwrap();
        let captured = lua.globals().get("captured").unwrap();
        (lua, captured)
    }

    fn same(original: &Layout, value: Value) {
        let tiled = parse_default(&value).unwrap();
        let tree = parse_tree(&value).unwrap();
        let back = Layout::from_parts_floating(
            tiled,
            tree.floats,
            Default::default(),
            original.focus().clone(),
            Default::default(),
        )
        .unwrap();
        assert_eq!(back.floats(), original.floats());
        for size in [
            Size { w: 1280, h: 721 },
            Size { w: 1920, h: 1040 },
            Size { w: 800, h: 600 },
        ] {
            let frame = Frame::new(size, 1);
            assert_eq!(arrange(&back, &frame).modules, arrange(original, &frame).modules);
        }
    }

    #[test]
    fn round_trip_layouts_and_floats() {
        for source in [
            r#"{ "row", { "editor", 0.6 }, { "column", { "agent", 0.5 }, { "terminal" } } }"#,
            r#"{ "column", { "tabbed", { "editor" }, { "agent" }, active = 2 },
                { "stacked", { "terminal#2" }, { "terminal#3" }, active = 2 } }"#,
            r#"{ "row", { "editor", 0.5942142 },
                { "column", { "agent", 0.5942142 }, { "terminal" }, share = 0.2 }, { "lua:notes" } }"#,
            r#"{ "row", { "editor" }, { "agent" },
                floating = {
                    { "terminal#2", cx = 0.5942142, cy = 1.0, w = 900, h = 517 },
                    { "terminal#3", cx = 0.0, cy = 0.5, w = 900, h = 420, scratch = true }
                } }"#,
        ] {
            let live = layout(source);
            let text = export_lua(&live, None);
            let (_lua, captured) = capture(&text);
            same(&live, captured.get("main").unwrap());
            assert_eq!(text, export_lua(&live, None));
            assert!(text.ends_with('\n') && !text.ends_with("\n\n"));
        }
    }

    #[test]
    fn workspaces_use_stored_main_and_live_current_in_order() {
        let mut live = layout(r#"{ "row", { "editor", 0.6 }, { "agent" } }"#);
        let main = live.clone();
        let mut ws = Workspaces::single();
        ws.create("study", layout(r#"{ "column", { "editor" }, { "agent" } }"#))
            .unwrap();
        ws.switch(&mut live, "study", &[]).unwrap();
        live = layout(r#"{ "row", { "agent", 0.3 }, { "editor" } }"#);
        let text = export_lua(&live, Some(&ws));
        assert!(text.find("eitri.layout.default(").unwrap() < text.find("eitri.layout.workspace(").unwrap());
        let (_lua, captured) = capture(&text);
        same(&main, captured.get("main").unwrap());
        same(&live, captured.get("study").unwrap());
    }

    #[test]
    fn main_is_first_even_when_workspace_order_starts_elsewhere() {
        let main = layout(r#"{ "row", { "editor", 0.6 }, { "agent" } }"#);
        let live = layout(r#"{ "column", { "editor" }, { "agent" } }"#);
        let ws = Workspaces::from_parts(
            vec!["study".into(), "main".into(), "later".into()],
            [("main".into(), main.clone()), ("later".into(), main.clone())].into(),
            "study".into(),
            Default::default(),
        )
        .unwrap();
        let text = export_lua(&live, Some(&ws));
        assert!(text.find("eitri.layout.default(").unwrap() < text.find("eitri.layout.workspace(").unwrap());
        assert!(text.find("workspace(\"study\"").unwrap() < text.find("workspace(\"later\"").unwrap());
        let (_lua, captured) = capture(&text);
        same(&main, captured.get("main").unwrap());
        same(&live, captured.get("study").unwrap());
    }

    #[test]
    fn deterministic_across_workspaces_and_omissions() {
        let live = layout(r#"{ "row", { "editor" }, { "agent#3" }, { "agent" }, { "agent#2" } }"#);
        let mut ws = Workspaces::single();
        ws.create("study", live.clone()).unwrap();
        let text = export_lua(&live, Some(&ws));
        assert_eq!(text, export_lua(&live, Some(&ws)));
        assert!(text.contains("-- Left out: agent#2, agent#3 ("));
        assert_eq!(text.matches("-- Left out:").count(), 1);
    }

    #[test]
    fn numbered_chats_are_removed_from_a_clone_and_reported_once() {
        let mut live = layout(r#"{ "row", { "editor" }, { "agent#2" }, { "agent" } }"#);
        live.add_float(
            &ModuleId::parse("agent#3").unwrap(),
            FloatRect {
                cx: 0.5,
                cy: 0.5,
                w: 900,
                h: 517,
            },
            false,
        )
        .unwrap();
        let text = export_lua(&live, None);
        assert!(text.contains("-- Left out: agent#2, agent#3 "));
        assert!(text.contains("eitri.layout.rule(\"agent#2\", { ... })"));
        let (_lua, captured) = capture(&text);
        let tree = parse_tree(&captured.get("main").unwrap()).unwrap();
        assert!(tree.floats.is_empty());
        assert_eq!(tree.tiled.leaves(), [ModuleId::editor(), ModuleId::agent()]);
        assert_eq!(live.modules().len(), 4);
        hide(&mut live, &ModuleId::agent(), &Frame::new(Size { w: 1280, h: 720 }, 1)).unwrap();
        let (_lua, captured) = capture(&export_lua(&live, None));
        parse_default(&captured.get("main").unwrap()).unwrap();

        let only = Layout::new(
            crate::layout::Node::Leaf(ModuleId::parse("agent#2").unwrap()),
            ModuleId::parse("agent#2").unwrap(),
        )
        .unwrap();
        let text = export_lua(&only, None);
        assert!(!text.contains("-- Left out:"));
        assert!(text.contains(r#"{ "agent#2" }"#));
    }

    #[test]
    fn hidden_modules_stay_in_the_tree_and_are_reported_per_workspace() {
        let mut live = layout(r#"{ "row", { "editor" }, { "agent" }, { "terminal" } }"#);
        hide(
            &mut live,
            &ModuleId::terminal(),
            &Frame::new(Size { w: 1280, h: 720 }, 1),
        )
        .unwrap();
        let mut ws = Workspaces::single();
        ws.create("study", live.clone()).unwrap();
        let text = export_lua(&live, Some(&ws));
        assert!(text.contains("-- Hidden in the window, shown at a first launch: terminal."));
        assert!(text.contains("-- Hidden in workspace \"study\", shown at a first launch: terminal."));
        let (_lua, captured) = capture(&text);
        assert!(parse_default(&captured.get("main").unwrap())
            .unwrap()
            .leaves()
            .contains(&ModuleId::terminal()));
    }

    #[test]
    fn strings_escape_quotes_backslashes_and_controls() {
        let name = "étude \"\\";
        Workspaces::valid_name(name).unwrap();
        let mut live = layout(r#"{ "row", { "editor" }, { "agent" } }"#);
        let panel = ModuleId::lua("quote\"\\\n\r\t\0\u{7f}\u{85}9");
        live.replace_root(crate::layout::Node::split(
            crate::layout::Axis::Row,
            0.6,
            live.root_node(),
            crate::layout::Node::Leaf(panel.clone()),
        ));
        let mut ws = Workspaces::single();
        ws.create(name, live.clone()).unwrap();
        let text = export_lua(&live, Some(&ws));
        assert!(text.contains(r#"\""#) && text.contains(r#"\\"#));
        let (_lua, captured) = capture(&text);
        let tree = parse_default(&captured.get(name).unwrap()).unwrap();
        assert!(tree.leaves().contains(&panel));
    }

    #[test]
    fn long_calls_wrap_and_still_load() {
        let live = layout(
            r#"{ "row", { "editor", 0.5942142 }, { "agent", 0.2 },
            { "column", { "terminal#2", 0.5942142 }, { "terminal#3" } },
            floating = { { "lua:notes", cx = 0.5942142, cy = 0.5942142, w = 900, h = 517, scratch = true } } }"#,
        );
        let text = export_lua(&live, None);
        assert!(text.lines().all(|line| line.chars().count() <= 120), "{text}");
        assert!(text.lines().any(|line| line.starts_with("  ")));
        let (_lua, captured) = capture(&text);
        same(&live, captured.get("main").unwrap());
    }
}
