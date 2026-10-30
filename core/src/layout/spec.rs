//! One layout builder for the Lua and manifest readers.

use std::collections::BTreeSet;

use super::con::{unwritten_share, SHARE_SLACK};
use super::tree::MIN_RATIO;
use super::{Child, Con, ConLayout, Float, ModuleId, Node};

/// A default layout's tiled tree and its floating modules.
#[derive(Debug, Clone, PartialEq)]
pub struct LuaTree {
    pub tiled: Node,
    pub floats: Vec<Float>,
}

/// The four container heads accepted by layout declarations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitKind {
    Row,
    Column,
    Tabbed,
    Stacked,
}

impl SplitKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Row => "row",
            Self::Column => "column",
            Self::Tabbed => "tabbed",
            Self::Stacked => "stacked",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "row" => Some(Self::Row),
            "column" => Some(Self::Column),
            "tabbed" => Some(Self::Tabbed),
            "stacked" => Some(Self::Stacked),
            _ => None,
        }
    }

    fn layout(self) -> ConLayout {
        match self {
            Self::Row => ConLayout::SplitH,
            Self::Column => ConLayout::SplitV,
            Self::Tabbed => ConLayout::Tabbed,
            Self::Stacked => ConLayout::Stacked,
        }
    }
}

/// A declaration before module names and container shares are resolved.
#[derive(Debug, Clone, PartialEq)]
pub enum SpecNode {
    Module {
        name: String,
        share: Option<f64>,
    },
    Split {
        kind: SplitKind,
        children: Vec<SpecNode>,
        share: Option<f64>,
        active: Option<i64>,
    },
}

/// Builds a tree without imposing a window's inventory of modules.
pub fn build(root: &SpecNode, resolve: &dyn Fn(&str) -> Result<ModuleId, String>) -> Result<LuaTree, String> {
    let (tiled, share) = build_node(root, resolve, "the tree")?;
    if share.is_some() {
        return Err("the whole tree takes no share".into());
    }
    let mut seen = BTreeSet::new();
    for id in tiled.leaves() {
        if !seen.insert(id.clone()) {
            return Err(format!("'{id}' appears twice"));
        }
    }
    Ok(LuaTree {
        tiled,
        floats: Vec::new(),
    })
}

fn build_node(
    node: &SpecNode,
    resolve: &dyn Fn(&str) -> Result<ModuleId, String>,
    at: &str,
) -> Result<(Node, Option<f32>), String> {
    match node {
        SpecNode::Module { name, share } => {
            let id = resolve(name).map_err(|e| format!("{at}: {e}"))?;
            Ok((Node::Leaf(id), share.map(|s| s as f32)))
        }
        SpecNode::Split {
            kind,
            children,
            share,
            active,
        } => {
            let head = kind.name();
            let layout = kind.layout();
            let children = children
                .iter()
                .enumerate()
                .map(|(i, child)| build_node(child, resolve, &format!("item {} of the {head}", i + 1)))
                .collect::<Result<Vec<_>, _>>()?;
            let tabbed = layout.axis().is_none();
            if children.len() < if tabbed { 1 } else { 2 } {
                let count = if tabbed { "one" } else { "two" };
                return Err(format!("{at}: a {head} needs {count} or more items"));
            }
            if active.is_some() && !tabbed {
                return Err(format!("{at}: unknown field active"));
            }
            let active = active.unwrap_or(1);
            if active < 1 || active as usize > children.len() {
                return Err(format!(
                    "{at}: active = {active}: the {head} has {} items",
                    children.len()
                ));
            }
            let mut con = container(layout, children, head)?;
            con.focus = active as usize - 1;
            Ok((Node::Con(con), share.map(|s| s as f32)))
        }
    }
}

/// Each child keeps its share directly, so parsing never introduces another container.
///
/// Shares are accepted in the model's own range (`Con::check`'s), down to `SHARE_SLACK` below the
/// minimum: an exported layout holds shares a rounding error short of 0.05 after a drag, and the
/// text must read back. The unspecified share is computed in `f64`, so the `f32` rounding of the
/// subtraction does not push a share at the minimum below the range.
fn container(layout: ConLayout, children: Vec<(Node, Option<f32>)>, head: &str) -> Result<Con, String> {
    let lowest = MIN_RATIO - SHARE_SLACK;
    for (i, (_, share)) in children.iter().enumerate() {
        if let Some(share) = share {
            if !(lowest..=1.0).contains(share) {
                return Err(format!(
                    "item {} of the {head} gets {share:.3}; a share is 0.05 to 1",
                    i + 1
                ));
            }
        }
    }
    let given: f64 = children.iter().filter_map(|(_, s)| s.map(f64::from)).sum();
    let unset = children.iter().filter(|(_, s)| s.is_none()).count();
    if unset == 0 && (given - 1.0).abs() > 0.001 {
        return Err(format!("the {head}'s shares add up to {}, not 1", given as f32));
    }
    if unset > 0 && given >= 1.0 {
        return Err(format!(
            "the {head}'s shares add up to {}, leaving nothing for the rest",
            given as f32
        ));
    }
    let each = if unset > 0 {
        let each = unwritten_share(children.iter().filter_map(|(_, s)| *s), unset);
        // Rounding to `f32` cannot carry a value at or above an `f32` bound below it.
        if each < f64::from(lowest) {
            let i = children.iter().position(|(_, s)| s.is_none()).unwrap_or(0);
            return Err(format!(
                "item {} of the {head} gets {each:.3}; a share is 0.05 to 1",
                i + 1
            ));
        }
        each as f32
    } else {
        0.0
    };
    let shares: Vec<f32> = children.iter().map(|(_, s)| s.unwrap_or(each)).collect();
    Ok(Con::new(
        layout,
        children
            .into_iter()
            .zip(shares)
            .map(|((node, _), share)| Child::new(node, share))
            .collect(),
    ))
}
#[cfg(test)]
mod tests {
    use crate::lua::layout::parse_tree;
    use crate::plugin::manifest::parse_manifest;

    fn manifest(tree: &str) -> String {
        format!(
            r#"id = "demo"
title = "Demo"
protocol = 1
site = "site"
entry = "index.html"
[roots]
work = "${{plugin}}"
[window]
root = "${{plugin}}"
title = "Demo"
layout = {tree}
"#
        )
    }

    // The manifest also takes a bare string for a module without a share (`"editor"` for `{ module = "editor" }`);
    // Lua has no such spelling, so the pair with a bare string below reads the Lua table form on the left.
    #[test]
    fn the_same_trees_give_the_same_layout_from_lua_and_toml() {
        for (lua, toml) in [
            ("{'row', {'editor'}, {'agent'}}", r#"{ row = ["editor", "chat"] }"#),
            (
                "{'row', {'editor'}, {'agent'}}",
                r#"{ row = [{ module = "editor" }, { module = "chat" }] }"#,
            ),
            (
                "{'column', {'editor'}, {'agent'}, {'terminal'}}",
                r#"{ column = [{ module = "editor" }, { module = "chat" }, { module = "terminal" }] }"#,
            ),
            (
                "{'tabbed', {'editor'}, {'agent'}, active = 2}",
                r#"{ tabbed = [{ module = "editor" }, { module = "chat" }], active = 2 }"#,
            ),
            (
                "{'stacked', {'agent'}, {'editor'}}",
                r#"{ stacked = [{ module = "chat" }, { module = "editor" }] }"#,
            ),
            (
                "{'row', {'lua:demo', 0.55}, {'column', {'editor'}, {'terminal', 0.25}}, {'agent', 0.1}}",
                r#"{ row = [{ module = "page", share = 0.55 }, { column = [{ module = "editor" }, { module = "terminal", share = 0.25 }] }, { module = "chat", share = 0.1 }] }"#,
            ),
            (
                "{'row', {'tabbed', {'editor'}, active = 1, share = 0.3}, {'stacked', {'agent'}, {'lua:demo'}}}",
                r#"{ row = [{ tabbed = [{ module = "editor" }], active = 1, share = 0.3 }, { stacked = [{ module = "chat" }, { module = "page" }] }] }"#,
            ),
            (
                "{'stacked', {'editor'}, {'agent'}, active = 2.0}",
                r#"{ stacked = [{ module = "editor" }, { module = "chat" }], active = 2.0 }"#,
            ),
        ] {
            let lua_state = mlua::Lua::new();
            let value = lua_state.load(lua).eval().unwrap();
            let expected = parse_tree(&value).unwrap();
            let actual = parse_manifest(&manifest(toml)).unwrap().window.unwrap().layout;
            assert_eq!(actual, expected, "{toml}");
        }
    }

    #[test]
    fn the_same_bad_trees_give_the_same_refusals_from_lua_and_toml() {
        // Two heads on one node are not here: TOML cannot write a key twice, so the manifest refuses that tree
        // as a parse error, and its own test covers it.
        for (lua, toml) in [
            (
                "{'row', {'editor', 1.2}, {'agent'}}",
                r#"{ row = [{ module = "editor", share = 1.2 }, { module = "chat" }] }"#,
            ),
            (
                "{'row', {'editor'}, {'agent'}, {'editor'}}",
                r#"{ row = [{ module = "editor" }, { module = "chat" }, { module = "editor" }] }"#,
            ),
            (
                "{'row', {'editor'}, {'agent'}, active = 1}",
                r#"{ row = [{ module = "editor" }, { module = "chat" }], active = 1 }"#,
            ),
            ("{'column'}", r#"{ column = [] }"#),
            (
                "{'tabbed', {'editor'}, {'agent'}, active = 3}",
                r#"{ tabbed = [{ module = "editor" }, { module = "chat" }], active = 3 }"#,
            ),
            (
                "{'tabbed', {'editor'}, {'agent'}, active = 1.5}",
                r#"{ tabbed = [{ module = "editor" }, { module = "chat" }], active = 1.5 }"#,
            ),
            (
                "{'stacked', {'editor'}, {'agent'}, active = 3.0}",
                r#"{ stacked = [{ module = "editor" }, { module = "chat" }], active = 3.0 }"#,
            ),
        ] {
            let lua_state = mlua::Lua::new();
            let value = lua_state.load(lua).eval().unwrap();
            let expected = parse_tree(&value).unwrap_err();
            let actual = parse_manifest(&manifest(toml)).unwrap_err();
            assert_eq!(
                actual,
                format!("eitri-plugin.toml: window.layout: {expected}"),
                "{toml}"
            );
        }
    }
}
