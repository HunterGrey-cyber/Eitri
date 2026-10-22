//! The pre-float JSON reader and writer are kept independent so compatibility checks cannot drift
//! with changes to the current format's JSON walk.
//!
//! Only the JSON walk is frozen. Normalising and reconciling go through this build's `Con::normalise`
//! and `reconcile`, so a check here pins what the pre-float build reads and writes, not how it
//! repaired a tree: its reconcile replaced a container that pruning left with one child by that
//! child whatever the container's mode, where this build keeps a tabbed or stacked one. A case that
//! depends on that repair needs the real binary, not this copy.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::super::con::{Child, Con, ConLayout, Node, Pin};
use super::super::module::{ModuleDecl, ModuleId};
use super::super::reconcile::reconcile;
use super::super::tree::{Axis, Layout, LayoutError};
use super::super::workspace::{built_in_and_registered, drop_keys_naming, printable, Kept, Workspaces};
use super::Opened;

const V2: u32 = 2;
const TOP_KEYS: [&str; 4] = ["version", "project_root", "current", "workspaces"];
const WORKSPACE_KEYS: [&str; 4] = ["name", "focus", "hidden", "tiled"];

#[derive(Serialize)]
struct FileOut<'a> {
    version: u32,
    project_root: String,
    current: &'a str,
    workspaces: Vec<WorkspaceOut<'a>>,
    #[serde(flatten)]
    kept: Map<String, Value>,
}

#[derive(Serialize)]
struct WorkspaceOut<'a> {
    name: &'a str,
    focus: &'a str,
    hidden: Vec<String>,
    tiled: NodeOut<'a>,
    #[serde(flatten)]
    kept: Map<String, Value>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum NodeOut<'a> {
    Leaf {
        module: &'a str,
    },
    Con {
        layout: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        last_split: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        active: Option<usize>,
        children: Vec<ChildOut<'a>>,
    },
}

#[derive(Serialize)]
struct ChildOut<'a> {
    share: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pin: Option<PinOut>,
    node: NodeOut<'a>,
}

#[derive(Serialize)]
struct PinOut {
    px: Option<i32>,
}

fn layout_name(layout: ConLayout) -> &'static str {
    match layout {
        ConLayout::SplitH => "splith",
        ConLayout::SplitV => "splitv",
        ConLayout::Tabbed => "tabbed",
        ConLayout::Stacked => "stacked",
    }
}

impl<'a> NodeOut<'a> {
    fn of(node: &'a Node) -> NodeOut<'a> {
        match node {
            Node::Leaf(id) => NodeOut::Leaf { module: id.as_str() },
            Node::Con(c) => NodeOut::of_con(c),
        }
    }

    fn of_con(c: &'a Con) -> NodeOut<'a> {
        NodeOut::Con {
            layout: layout_name(c.layout),
            last_split: c.layout.axis().is_none().then_some(match c.last_split {
                Axis::Row => "row",
                Axis::Column => "column",
            }),
            active: c.layout.axis().is_none().then_some(c.focus),
            children: c
                .children
                .iter()
                .map(|ch| ChildOut {
                    share: ch.share,
                    pin: ch.pin.map(|pin| PinOut { px: pin.px }),
                    node: NodeOut::of(&ch.node),
                })
                .collect(),
        }
    }
}

/// `kept` without any key this build writes itself, so a kept key can never stand beside a written
/// one of the same name.
fn kept_except(kept: Option<&Map<String, Value>>, known: &[&str]) -> Map<String, Value> {
    kept.into_iter()
        .flatten()
        .filter(|(key, _)| !known.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// The file's text for every workspace of the window, `current` being the current one's layout:
/// pretty JSON, so a person can read and fix it, with what a later build wrote after the keys this
/// one writes. A retired editor is written shown (`Layout::saved_hidden`).
pub fn encode_v2(workspaces: &Workspaces, current: &Layout, project_root: &Path) -> String {
    let file = FileOut {
        version: V2,
        project_root: project_root.to_string_lossy().into_owned(),
        current: workspaces.current(),
        workspaces: workspaces
            .each(current)
            .into_iter()
            .map(|(name, layout)| WorkspaceOut {
                name,
                focus: layout.focus().as_str(),
                hidden: layout.saved_hidden().iter().map(|id| id.as_str().to_string()).collect(),
                tiled: NodeOut::of_con(layout.root()),
                kept: kept_except(workspaces.kept_for(name), &WORKSPACE_KEYS),
            })
            .collect(),
        kept: kept_except(Some(&workspaces.kept().top), &TOP_KEYS),
    };
    serde_json::to_string_pretty(&file).expect("a layout always serializes")
}

// ---------------------------------------------------------------------------------------------
// Version 2: reading.

/// One workspace of a file, as it was read.
struct ReadWorkspace {
    name: String,
    layout: Layout,
    kept: Map<String, Value>,
}

fn take_str(map: &mut Map<String, Value>, key: &str) -> Result<String, String> {
    match map.remove(key) {
        Some(Value::String(text)) => Ok(text),
        Some(_) => Err(format!("`{key}` is not a string")),
        None => Err(format!("no `{key}`")),
    }
}

/// Notes each key of `map` not in `known`: inside a tree there is nowhere to keep one, since the tree
/// is rebuilt, so its loss asks for a copy.
fn not_kept(map: &Map<String, Value>, known: &[&str], ws: &str, notes: &mut Vec<String>, copy: &mut bool) {
    for key in map.keys().filter(|key| !known.contains(&key.as_str())) {
        notes.push(format!("{key} inside workspace '{ws}' is not kept"));
        *copy = true;
    }
}

/// A tree node of workspace `ws`. An unknown container layout is read as `splith`, a tab container's
/// `active` past its children is read as 0, and an unknown key in a node, a child or a pin is not
/// kept: each is noted and sets `copy`. Anything else that cannot be read fails the workspace.
fn read_node(value: &Value, ws: &str, notes: &mut Vec<String>, copy: &mut bool) -> Result<Node, String> {
    let Value::Object(map) = value else {
        return Err("a tree node that is not an object".to_string());
    };
    match map.get("type") {
        Some(Value::String(kind)) if kind == "leaf" => {
            not_kept(map, &["type", "module"], ws, notes, copy);
            match map.get("module") {
                Some(Value::String(module)) => Ok(Node::Leaf(ModuleId::parse(module).map_err(|e| e.to_string())?)),
                _ => Err("a leaf without a `module`".to_string()),
            }
        }
        Some(Value::String(kind)) if kind == "con" => {
            let layout = match map.get("layout") {
                Some(Value::String(name)) if name == "splith" => ConLayout::SplitH,
                Some(Value::String(name)) if name == "splitv" => ConLayout::SplitV,
                Some(Value::String(name)) if name == "tabbed" => ConLayout::Tabbed,
                Some(Value::String(name)) if name == "stacked" => ConLayout::Stacked,
                Some(Value::String(name)) => {
                    notes.push(format!("container layout '{name}' read as splith"));
                    *copy = true;
                    ConLayout::SplitH
                }
                _ => return Err("a container without a `layout`".to_string()),
            };
            let known: &[&str] = if layout.axis().is_none() {
                &["type", "layout", "last_split", "active", "children"]
            } else {
                &["type", "layout", "children"]
            };
            not_kept(map, known, ws, notes, copy);
            let Some(Value::Array(items)) = map.get("children") else {
                return Err("a container without `children`".to_string());
            };
            let mut children = Vec::with_capacity(items.len());
            for item in items {
                let Value::Object(child) = item else {
                    return Err("a container's child that is not an object".to_string());
                };
                not_kept(child, &["share", "pin", "node"], ws, notes, copy);
                let Some(share) = child.get("share").and_then(Value::as_f64) else {
                    return Err("a container's child without a `share`".to_string());
                };
                let pin = match child.get("pin") {
                    None | Some(Value::Null) => None,
                    Some(Value::Object(pin)) => {
                        not_kept(pin, &["px"], ws, notes, copy);
                        let px = match pin.get("px") {
                            None | Some(Value::Null) => None,
                            Some(px) => {
                                Some(px.as_i64().and_then(|px| i32::try_from(px).ok()).ok_or_else(|| {
                                    format!("a pinned length of {px} is not a whole number of pixels")
                                })?)
                            }
                        };
                        Some(Pin { px })
                    }
                    Some(_) => return Err("a `pin` that is not an object".to_string()),
                };
                let Some(node) = child.get("node") else {
                    return Err("a container's child without a `node`".to_string());
                };
                let mut out = Child::new(read_node(node, ws, notes, copy)?, share as f32);
                out.pin = pin;
                children.push(out);
            }
            let mut c = Con::new(layout, children);
            if layout.axis().is_none() {
                c.last_split = match map.get("last_split").and_then(Value::as_str) {
                    None if !map.contains_key("last_split") => Axis::Row,
                    Some("row") => Axis::Row,
                    Some("column") => Axis::Column,
                    _ => return Err("a container's `last_split` must be row or column".to_string()),
                };
                let active = match map.get("active") {
                    None => 0,
                    Some(value) => value
                        .as_u64()
                        .ok_or_else(|| "a container's `active` must be a child index".to_string())?,
                };
                if active >= c.children.len() as u64 {
                    notes.push(format!(
                        "container active {active} is past its {} children; reading 0",
                        c.children.len()
                    ));
                    *copy = true;
                } else {
                    c.focus = active as usize;
                }
            }
            Ok(Node::Con(c))
        }
        Some(Value::String(kind)) => Err(format!("a tree node of type '{kind}'")),
        _ => Err("a tree node without a `type`".to_string()),
    }
}

/// Whether [`Con::normalise`] or `Layout::new` would change `c`'s shape: a container below the root
/// with fewer than two children, or a root whose one child is a container.
fn unnormalised(c: &Con, is_root: bool) -> bool {
    let own = if is_root {
        matches!(c.children.as_slice(), [only] if matches!(only.node, Node::Con(_)))
    } else {
        c.children.len() < 2
    };
    own || c
        .children
        .iter()
        .any(|ch| matches!(&ch.node, Node::Con(inner) if unnormalised(inner, false)))
}

/// Every container of `c` that holds a child has shares adding up to 1, as written. Checked before
/// [`Con::normalise`], which dissolves a one-child container with its share and rescales a container
/// that lost an empty child: either would turn a wrong sum into a right one, and the file would then
/// be written over as though nothing of it had been lost.
fn shares_add_up(c: &Con) -> Result<(), String> {
    if !c.children.is_empty() {
        let sum: f32 = c.children.iter().map(|ch| ch.share).sum();
        if !sum.is_finite() || (sum - 1.0).abs() > 1e-4 {
            return Err(LayoutError::ShareSum(sum).to_string());
        }
    }
    c.children.iter().try_for_each(|ch| match &ch.node {
        Node::Con(inner) => shares_add_up(inner),
        Node::Leaf(_) => Ok(()),
    })
}

/// Workspace `name`'s own fields, read into a layout of this window; what is left of `map` is its
/// kept keys. A hand-edited container of one child, or of none, is normalised with a note rather
/// than failing the workspace; shares that do not add up to 1 still fail it, since they say nothing
/// reliable about the sizes meant. The workspace's notes reach `notes` only if it can be used.
fn read_workspace(
    name: &str,
    mut map: Map<String, Value>,
    lua: &[ModuleDecl],
    notes: &mut Vec<String>,
    copy: &mut bool,
    instances: &[ModuleId],
) -> Result<(Layout, Map<String, Value>), String> {
    let mut own = Vec::new();
    let focus = ModuleId::parse(&take_str(&mut map, "focus")?).map_err(|e| e.to_string())?;
    let hidden = match map.remove("hidden") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::String(id) => ModuleId::parse(id).map_err(|e| e.to_string()),
                _ => Err("`hidden` holds something that is not a module name".to_string()),
            })
            .collect::<Result<BTreeSet<_>, _>>()?,
        Some(_) => return Err("`hidden` is not a list".to_string()),
        None => return Err("no `hidden`".to_string()),
    };
    let tiled = map.remove("tiled").ok_or("no `tiled`")?;
    let mut tree = read_node(&tiled, name, &mut own, copy)?;
    if let Node::Con(c) = &mut tree {
        shares_add_up(c)?;
        if unnormalised(c, true) {
            c.normalise();
            own.push(format!("normalised a container in workspace '{name}'"));
        }
    }
    if tree
        .leaves()
        .iter()
        .any(|id| id.kind() == super::super::module::ModuleKind::Agent && id.number() >= 2)
    {
        *copy = true;
    }
    let reconciled = reconcile(tree, &hidden, Some(&focus), lua, instances).map_err(|e| e.to_string())?;
    own.extend(
        reconciled
            .notes
            .into_iter()
            .map(|note| format!("workspace '{name}': {note}")),
    );
    notes.extend(own);
    Ok((reconciled.layout, map))
}

fn left_out(label: &str, why: &str, notes: &mut Vec<String>, copy: &mut bool) {
    notes.push(format!("workspace {label} left out: {why}"));
    *copy = true;
}

/// A version 2 file's text read back as this window's workspaces. The whole file fails -- `Err`,
/// saying why -- when it is not JSON, is another version, was written for another project root, or
/// has no workspace that can be used. Each workspace is read on its own: one whose name is invalid or
/// repeats an earlier one, or whose tree cannot be used, is left out with a note, and the rest open.
/// Whatever is left out or read differently sets [`Opened::keep_copy`]; a key carried as it is does
/// not. Every note and reason comes back with its control characters escaped: names and keys in them
/// are the file's own text, and each ends up on one log line.
pub fn decode_v2(text: &str, project_root: &Path, lua: &[ModuleDecl]) -> Result<Opened, String> {
    match decode_v2_as_written(text, project_root, lua) {
        Ok(mut opened) => {
            opened.notes = opened.notes.iter().map(|note| printable(note)).collect();
            Ok(opened)
        }
        Err(why) => Err(printable(&why)),
    }
}

/// How deeply arrays and objects may nest in a version 2 file: [`decode_v2`] refuses a deeper file
/// before parsing it, and [`save`] never writes one. Each container of a tree costs three levels (the
/// container, its `children`, the child), so this allows a tree 169 containers deep, which takes at
/// least 170 modules in one workspace, while keeping the parse, which recurses once per level, far
/// from the end of a thread's stack.
pub const MAX_NESTING: usize = 512;

/// How deeply arrays and objects nest in `text`, read as JSON: brackets inside strings do not count.
/// Malformed text gives some number, and is refused by the parse that follows.
fn nesting(text: &str) -> usize {
    let (mut depth, mut deepest) = (0usize, 0usize);
    let (mut in_string, mut escaped) = (false, false);
    // Byte by byte: none of these bytes occurs inside a multi-byte UTF-8 character.
    for byte in text.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    deepest
}

fn decode_v2_as_written(text: &str, project_root: &Path, lua: &[ModuleDecl]) -> Result<Opened, String> {
    let deepest = nesting(text);
    if deepest > MAX_NESTING {
        return Err(format!(
            "not a layout file (nested {deepest} levels deep; this build reads at most {MAX_NESTING})"
        ));
    }
    // serde_json's own limit of 128 levels is lower than a tree this build writes can reach; the
    // depth was bounded above instead.
    let mut parser = serde_json::Deserializer::from_str(text);
    parser.disable_recursion_limit();
    let value = Value::deserialize(&mut parser)
        .and_then(|value| parser.end().map(|()| value))
        .map_err(|e| format!("not a layout file ({e})"))?;
    let Value::Object(mut top) = value else {
        return Err("not a layout file (not a JSON object)".to_string());
    };
    let version = match top.remove("version") {
        Some(Value::Number(n)) => n.as_u64().ok_or_else(|| format!("not a layout file (version {n})"))?,
        Some(_) => return Err("not a layout file (`version` is not a number)".to_string()),
        None => return Err("not a layout file (no `version`)".to_string()),
    };
    if version != u64::from(V2) {
        return Err(format!("version {version} (this build reads {V2})"));
    }
    let file_root = take_str(&mut top, "project_root").map_err(|e| format!("not a layout file ({e})"))?;
    let root = project_root.to_string_lossy();
    if file_root != root {
        return Err(format!("written for {file_root}, not {root}"));
    }
    let current = take_str(&mut top, "current").map_err(|e| format!("not a layout file ({e})"))?;
    let Some(Value::Array(items)) = top.remove("workspaces") else {
        return Err("not a layout file (no list of `workspaces`)".to_string());
    };

    // Only tiled leaves create instances; names in preserved data reserve their numbers instead.
    fn terminals(node: &Value, found: &mut BTreeSet<ModuleId>) {
        match node.get("type").and_then(Value::as_str) {
            Some("leaf") => {
                if let Some(id) = node
                    .get("module")
                    .and_then(Value::as_str)
                    .and_then(|s| ModuleId::parse(s).ok())
                {
                    if id.kind() == super::super::module::ModuleKind::Terminal && id.number() >= 2 {
                        found.insert(id);
                    }
                }
            }
            Some("con") => {
                if let Some(children) = node.get("children").and_then(Value::as_array) {
                    for child in children {
                        if let Some(node) = child.get("node") {
                            terminals(node, found);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let mut instances = BTreeSet::new();
    for item in &items {
        if let Some(tiled) = item.get("tiled") {
            terminals(tiled, &mut instances);
        }
    }
    let instances: Vec<_> = instances.into_iter().collect();

    let mut notes = Vec::new();
    let mut copy = false;
    let mut seen = BTreeSet::new();
    let mut read: Vec<ReadWorkspace> = Vec::new();
    for (index, item) in items.into_iter().enumerate() {
        let number = format!("#{}", index + 1);
        let Value::Object(mut map) = item else {
            left_out(&number, "it is not an object", &mut notes, &mut copy);
            continue;
        };
        let name = match take_str(&mut map, "name") {
            Ok(name) => name,
            Err(why) => {
                left_out(&number, &why, &mut notes, &mut copy);
                continue;
            }
        };
        let label = format!("'{}'", name.escape_debug());
        if let Err(err) = Workspaces::valid_name(&name) {
            left_out(&label, &err.to_string(), &mut notes, &mut copy);
            continue;
        }
        if !seen.insert(name.clone()) {
            left_out(&label, "an earlier workspace has that name", &mut notes, &mut copy);
            continue;
        }
        match read_workspace(&name, map, lua, &mut notes, &mut copy, &instances) {
            Ok((layout, kept)) => read.push(ReadWorkspace { name, layout, kept }),
            Err(why) => left_out(&label, &why, &mut notes, &mut copy),
        }
    }
    if read.is_empty() {
        return Err(if notes.is_empty() {
            "no workspace can be used".to_string()
        } else {
            format!("no workspace can be used ({})", notes.join("; "))
        });
    }

    // A kept key that names a module this window places would have it in two places once written
    // back: in the tree this window rebuilt, and wherever the later build keeps it.
    let mut placed = built_in_and_registered(lua);
    for ws in &read {
        placed.extend(ws.layout.leaves());
    }
    let before = notes.len();
    for ws in &mut read {
        drop_keys_naming(&ws.name, &mut ws.kept, &placed, &mut notes);
    }
    copy |= notes.len() > before;

    let current = if read.iter().any(|ws| ws.name == current) {
        current
    } else {
        let first = read[0].name.clone();
        notes.push(format!(
            "workspace '{}' was current and cannot be used; '{first}' is current",
            current.escape_debug()
        ));
        first
    };
    let order: Vec<String> = read.iter().map(|ws| ws.name.clone()).collect();
    let mut kept = Kept {
        top,
        per_workspace: BTreeMap::new(),
    };
    let mut stored = BTreeMap::new();
    let mut current_layout = None;
    for ws in read {
        kept.per_workspace.insert(ws.name.clone(), ws.kept);
        if ws.name == current {
            current_layout = Some(ws.layout);
        } else {
            stored.insert(ws.name, ws.layout);
        }
    }
    let workspaces = Workspaces::from_parts(order, stored, current, kept).map_err(|e| e.to_string())?;
    Ok(Opened {
        workspaces,
        current: current_layout.expect("the current workspace is one of those read"),
        notes,
        keep_copy: copy.then(|| text.to_string()),
    })
}
