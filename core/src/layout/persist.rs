//! Keep a project's workspace layouts between launches.
//!
//! State files are `$XDG_STATE_HOME/eitri/layout/<hash>.v2.json`, where `<hash>` is the first
//! 16 hex characters of `sha256(canonical project root)`, matching
//! `agent::conversation_id_for_cwd`. The state root defaults to `~/.local/state`.
//! Nothing is written into the project directory.
//!
//! Version 1 builds use `<hash>.json`. A separate version 2 name lets both builds use the
//! same project without setting aside each other's state. Read version 1 for migration
//! only when no usable version 2 file exists. Never write, rename or delete version 1.
//!
//! Store `{version: 2, project_root, current, workspaces: [{name, focus, hidden, tiled}]}`.
//! Shares and pinned pixel lengths preserve the layout. Zoom, MRU order, the mark, last
//! preset, undo slot and container ids are not saved; a window opens unzoomed with fresh ids.
//!
//! Read by walking the JSON rather than deriving `Deserialize` on the model. Every id
//! passes `ModuleId::parse`, and each tree passes `reconcile` and `Layout::from_parts`,
//! so input cannot introduce an unknown module kind or skip an invariant.
//!
//! An unusable file falls back with one log line instead of preventing startup. This
//! includes corruption, a different version or project root, or no usable workspace.
//! A missing file is silent; `init.lua` errors still prevent startup. Before each write,
//! `shell` reads again and moves an unusable file to `<hash>.v2.json.unusable` with
//! [`set_aside_v2`]. If only part is unusable, load the rest and use [`copy_aside_v2`] to
//! preserve the original text as `<hash>.v2.json.partial` before overwriting it.
//! The two backup names keep complete and partial failures from replacing each other.
//!
//! Keep unknown top-level and workspace-level keys verbatim in [`Workspaces`] and write
//! them back after this build's keys, preserving later builds' data while version stays 2.
//! Drop an unknown workspace-level key that names a module already placed in this
//! window's trees, to avoid placing it twice, and copy the file aside.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::con::{Child, Con, ConLayout, Node, Pin};
use super::module::{ModuleDecl, ModuleId};
use super::reconcile::{reconcile, Reconciled};
use super::tree::{Axis, Branch, Layout, LayoutError, MAX_RATIO, MIN_RATIO};
use super::workspace::{built_in_and_registered, drop_keys_naming, printable, Kept, Workspaces};

/// The version of the file builds before workspaces write, `<hash>.json`; read here only to migrate.
pub const V1: u32 = 1;
/// The version this build reads and writes, `<hash>.v2.json`. A file of any other version is not read.
pub const V2: u32 = 2;

/// How long after the last change the file is written. In `shell` (`layout_state::LayoutSaver`) only
/// a change of the arrangement arms the timer, not a move of the keys; a change while it waits pushes
/// back when it fires, and a timer that fires early is armed again for what is left rather than
/// restarted. The close writes synchronously, and only a change still unwritten.
pub const SAVE_DEBOUNCE_MS: u64 = 500;

/// The keys of a version 2 file this build reads, at the top level and in a workspace; anything else
/// at those two levels is kept.
const TOP_KEYS: [&str; 4] = ["version", "project_root", "current", "workspaces"];
const WORKSPACE_KEYS: [&str; 4] = ["name", "focus", "hidden", "tiled"];

// ---------------------------------------------------------------------------------------------
// Version 1, read only.

#[derive(Debug, Deserialize)]
struct LayoutFile {
    version: u32,
    project_root: String,
    root: NodeFile,
    hidden: Vec<String>,
    focus: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum NodeFile {
    Leaf {
        module: String,
    },
    Split {
        axis: AxisFile,
        ratio: f32,
        #[serde(default)]
        pin: Option<PinFile>,
        first: Box<NodeFile>,
        second: Box<NodeFile>,
    },
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AxisFile {
    Row,
    Column,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SideFile {
    First,
    Second,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct PinFile {
    side: SideFile,
    px: Option<i32>,
}

impl NodeFile {
    /// The node this split stands for, as a container of two children: each binary split becomes a
    /// two-child container of the same orientation with shares `ratio` and `1 - ratio`, and a pin
    /// goes to the child on its side. Nothing is flattened, so the window is pixel-identical. A ratio
    /// outside `MIN_RATIO..=MAX_RATIO` is refused here, naming it, before it becomes two shares.
    fn into_node(self) -> Result<Node, String> {
        Ok(match self {
            NodeFile::Leaf { module } => Node::Leaf(ModuleId::parse(&module).map_err(|e| e.to_string())?),
            NodeFile::Split {
                axis,
                ratio,
                pin,
                first,
                second,
            } => {
                if !(MIN_RATIO..=MAX_RATIO).contains(&ratio) {
                    return Err(LayoutError::Ratio(ratio).to_string());
                }
                let axis = match axis {
                    AxisFile::Row => Axis::Row,
                    AxisFile::Column => Axis::Column,
                };
                let (first, second) = (first.into_node()?, second.into_node()?);
                match pin {
                    None => Node::split(axis, ratio, first, second),
                    Some(PinFile { side, px }) => {
                        let side = match side {
                            SideFile::First => Branch::First,
                            SideFile::Second => Branch::Second,
                        };
                        let Node::Con(mut c) = Node::pinned(axis, ratio, side, first, second) else {
                            unreachable!("a split is a container")
                        };
                        c.children[usize::from(side == Branch::Second)].pin = Some(Pin { px });
                        Node::Con(c)
                    }
                }
            }
        })
    }
}

/// A version 1 file's text read back as a layout of this window: this project, version 1, every id a
/// module this window has (`reconcile`, whose notes come back with it). `Err` says why it cannot be
/// used. A key it does not know is ignored, as the builds that wrote it did.
pub fn decode_v1(text: &str, project_root: &Path, lua: &[ModuleDecl]) -> Result<Reconciled, String> {
    let file: LayoutFile = serde_json::from_str(text).map_err(|e| format!("not a layout file ({e})"))?;
    if file.version != V1 {
        return Err(format!("version {} (this build migrates {V1})", file.version));
    }
    let root = project_root.to_string_lossy();
    if file.project_root != root {
        return Err(format!("written for {}, not {root}", file.project_root));
    }
    let tree = file.root.into_node()?;
    let hidden = file
        .hidden
        .iter()
        .map(|id| ModuleId::parse(id).map_err(|e| e.to_string()))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let focus = ModuleId::parse(&file.focus).map_err(|e| e.to_string())?;
    reconcile(tree, &hidden, Some(&focus), lua, &[]).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------------------------
// Version 2: writing.

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

/// A version 2 file opened against this window.
#[derive(Debug, Clone, PartialEq)]
pub struct Opened {
    /// Every workspace that could be used; the current one's layout is [`Opened::current`].
    pub workspaces: Workspaces,
    pub current: Layout,
    /// One line per change made to open it: a workspace left out, a container read differently, what
    /// `reconcile` changed.
    pub notes: Vec<String>,
    /// The exact text decoded, when something of it could not be carried into this window and would
    /// be lost at the next write; `None` otherwise. The window copies it aside before it writes.
    pub keep_copy: Option<String>,
}

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
        .any(|id| id.kind() == super::module::ModuleKind::Agent && id.number() >= 2)
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
                    if id.kind() == super::module::ModuleKind::Terminal && id.number() >= 2 {
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

// ---------------------------------------------------------------------------------------------
// Files.

/// `<state home>/eitri/<sub>`: the rule `state_dir` states, for any of Eitri's state
/// directories (`layout`, and the session tabs' `agent`).
pub fn state_subdir(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>, sub: &str) -> Option<PathBuf> {
    match (xdg_state_home, home) {
        (Some(state), _) if Path::new(state).is_absolute() => Some(PathBuf::from(state).join("eitri").join(sub)),
        (_, Some(home)) if Path::new(home).is_absolute() => {
            Some(PathBuf::from(home).join(".local/state/eitri").join(sub))
        }
        _ => None,
    }
}

/// Creates `dir` -- a [`state_subdir`], `<state home>/eitri/<sub>` -- and anything missing above
/// it 0700, and tightens `dir` and its `eitri` parent to 0700 where an older build left either
/// open. Every writer of Eitri's own state in this
/// crate creates its directory through this, and writes its files 0600
/// (`agent::private_fs::write_private`).
pub fn create_state_dir(dir: &Path) -> std::io::Result<()> {
    agent::private_fs::create_private_dir_all(dir, dir.parent().unwrap_or(dir))
}

/// `<state home>/eitri/layout`, from the two variables that decide it: `XDG_STATE_HOME`, else
/// `$HOME/.local/state` (the XDG spec's own default). An `XDG_STATE_HOME` that is empty or not an
/// absolute path is ignored, as the XDG spec says to. A `HOME` that is empty or not absolute is
/// refused too, as `agent::state_dirs` refuses a relative home directory: relative, it would resolve
/// against the process's cwd, which for a window started with no project argument IS the project --
/// and nothing is written into the project. `None` when neither gives a directory.
pub fn state_dir(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    state_subdir(xdg_state_home, home, "layout")
}

/// The version 1 file's name for this canonical project root: 16 hex digits of its SHA-256, `.json`.
/// This build only reads that file; Eitri's other per-project state files are named the same way.
pub fn file_name(project_root: &Path) -> String {
    format!("{}.json", &agent::conversation_id_for_cwd(project_root)[..16])
}

/// The version 2 file's name: the same 16 hex digits, `.v2.json`.
pub fn file_name_v2(project_root: &Path) -> String {
    format!("{}.v2.json", &agent::conversation_id_for_cwd(project_root)[..16])
}

/// What [`load`] found.
#[derive(Debug)]
pub enum Loaded {
    /// The version 2 file, opened against this window.
    Restored(Opened),
    /// No usable version 2 file, and the version 1 file read into one workspace, `main`. Its notes
    /// say so, and why the version 2 file could not be used if there was one.
    Migrated(Opened),
    /// Neither file: this project's arrangement has not been changed in a window since its file was
    /// last deleted or set aside. The normal first launch, and not logged.
    Missing,
    /// Neither file can be used, and why; the caller logs it and opens the default layout.
    Unusable(String),
}

/// What [`v2_state`] found in the version 2 file, with the keys it keeps for a later build:
/// `per_workspace` holds an entry, empty or not, for every workspace the file holds that can be used.
#[derive(Debug, Clone, PartialEq)]
pub enum V2State {
    Missing,
    /// Opened whole; the text is what was read, for a writer that finds it cannot carry one of the
    /// kept keys after all (`Kept::drop_placed` against its own modules) and must copy it aside.
    Usable(String, Kept),
    /// Opened, but something of it could not be carried (`Opened::keep_copy`); the text is what was
    /// read.
    Partly(String, Kept),
    /// Cannot be opened, and why.
    Unusable(String),
}

/// The version 2 file's text; `None` when there is none.
pub fn read_v2_text(dir: &Path, project_root: &Path) -> std::io::Result<Option<String>> {
    match agent::private_fs::read_private_to_string(&dir.join(file_name_v2(project_root))) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Reads this project's layouts from `dir`: the version 2 file if it can be used, else the version
/// 1 file migrated into one workspace, else nothing. The version 1 file is only ever read.
pub fn load(dir: &Path, project_root: &Path, lua: &[ModuleDecl]) -> Loaded {
    let v2_path = dir.join(file_name_v2(project_root));
    let v2_why = match read_v2_text(dir, project_root) {
        Ok(Some(text)) => match decode_v2(&text, project_root, lua) {
            Ok(opened) => return Loaded::Restored(opened),
            Err(why) => Some(format!("{}: {why}", v2_path.display())),
        },
        Ok(None) => None,
        Err(e) => Some(format!("{}: {e}", v2_path.display())),
    };
    let v1_path = dir.join(file_name(project_root));
    let v1_why = match agent::private_fs::read_private_to_string(&v1_path) {
        Ok(text) => match decode_v1(&text, project_root, lua) {
            Ok(reconciled) => {
                let mut notes = reconciled.notes;
                notes.push("migrated from the version 1 layout file".to_string());
                if let Some(why) = v2_why {
                    notes.push(format!(
                        "{why}; it is set aside as .v2.json.unusable the first time this window saves"
                    ));
                }
                return Loaded::Migrated(Opened {
                    workspaces: Workspaces::single(),
                    current: reconciled.layout,
                    notes,
                    keep_copy: None,
                });
            }
            Err(why) => Some(format!("{}: {why}", v1_path.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(format!("{}: {e}", v1_path.display())),
    };
    match (v2_why, v1_why) {
        (None, None) => Loaded::Missing,
        (Some(why), None) | (None, Some(why)) => Loaded::Unusable(why),
        (Some(v2), Some(v1)) => Loaded::Unusable(format!("{v2}; {v1}")),
    }
}

/// The version 2 file as it is now, read only to know what a write would replace: whether it can be
/// used, whether part of it would be lost, and the keys it keeps for a later build.
pub fn v2_state(dir: &Path, project_root: &Path, lua: &[ModuleDecl]) -> V2State {
    let text = match read_v2_text(dir, project_root) {
        Ok(None) => return V2State::Missing,
        Ok(Some(text)) => text,
        Err(e) => return V2State::Unusable(e.to_string()),
    };
    match decode_v2(&text, project_root, lua) {
        Ok(opened) => {
            let mut kept = opened.workspaces.kept().clone();
            for name in opened.workspaces.order() {
                kept.per_workspace.entry(name.clone()).or_default();
            }
            match opened.keep_copy {
                Some(_) => V2State::Partly(text, kept),
                None => V2State::Usable(text, kept),
            }
        }
        Err(why) => V2State::Unusable(why),
    }
}

/// Writes `text` to `path` through this process's own `.tmp` beside it, then renames it into place,
/// so a process that dies mid-write leaves the previous file, never half of one; a failed write or
/// rename removes its `.tmp`.
fn write_whole(dir: &Path, path: &Path, text: &str) -> std::io::Result<()> {
    create_state_dir(dir)?;
    let tmp = temporary(path);
    let written = agent::private_fs::write_private(&tmp, text.as_bytes()).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(err) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

/// Writes every workspace of the window, and every kept key, to this project's version 2 file in
/// `dir`, creating it: to a `.tmp` beside it first and then renamed over it, so a process that dies
/// mid-write leaves the previous file, never half of one. (There is no `fsync`, so a power loss can
/// still leave a torn file on some filesystems; that file is then unusable, and is set aside like any
/// other.) The `.tmp` is this process's own ([`temporary`]): two windows on one project are two
/// processes (`NON_UNIQUE`), and one shared name would let one rename the other's half-written file
/// into place. Nothing of the file on disk is merged in: the last to write wins, whole. Only a
/// process killed between the write and the rename leaves a `.tmp`, and nothing sweeps those: telling
/// a dead window's from a live one's would mean asking whether its process still runs, for a small
/// file that `load` never reads.
///
/// A text nested deeper than [`MAX_NESTING`] is not written (`InvalidData`): the next launch could not
/// read it, and the file already there is a better one to open with.
pub fn save(dir: &Path, project_root: &Path, workspaces: &Workspaces, current: &Layout) -> std::io::Result<PathBuf> {
    let path = dir.join(file_name_v2(project_root));
    let text = encode_v2(workspaces, current, project_root);
    let deepest = nesting(&text);
    if deepest > MAX_NESTING {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("the layout nests {deepest} levels deep; a file deeper than {MAX_NESTING} could not be read back"),
        ));
    }
    write_whole(dir, &path, &text)?;
    Ok(path)
}

/// Where a file at `path` is written first: for a `.json` file, `<its name>.<this process's id>.tmp`
/// beside it.
pub fn temporary(path: &Path) -> PathBuf {
    path.with_extension(format!("json.{}.tmp", std::process::id()))
}

/// Moves this project's version 2 file, whole, to `<hash>.v2.json.unusable` beside it and returns
/// where it went: what the window does before it writes over a file it cannot open. A hand-edit with
/// one typo, or a file a newer build wrote, is kept for a person to read. An earlier `.unusable` is
/// replaced; a `.partial` never is.
pub fn set_aside_v2(dir: &Path, project_root: &Path) -> std::io::Result<PathBuf> {
    let path = dir.join(file_name_v2(project_root));
    let aside = dir.join(format!("{}.unusable", file_name_v2(project_root)));
    std::fs::rename(&path, &aside)?;
    Ok(aside)
}

/// Writes `text` -- a version 2 file's bytes, part of which this window cannot carry -- to
/// `<hash>.v2.json.partial` beside the file (0600, through its own `.tmp`), and returns where. An
/// earlier `.partial` is replaced; an `.unusable` never is.
pub fn copy_aside_v2(dir: &Path, project_root: &Path, text: &str) -> std::io::Result<PathBuf> {
    let aside = dir.join(format!("{}.partial", file_name_v2(project_root)));
    write_whole(dir, &aside, text)?;
    Ok(aside)
}

#[cfg(test)]
mod tests {
    use super::super::geometry::{arrange, settle_pins, Frame, Size};
    use super::super::module::Placement;
    use super::super::test_support::FRAMES;
    use super::super::v1_reference::{v1_arrange, V1Node, V1Pin};
    use super::*;
    use serde_json::json;

    fn root() -> PathBuf {
        PathBuf::from("/home/user/project")
    }

    fn terminal_decl() -> ModuleDecl {
        ModuleDecl {
            id: ModuleId::terminal(),
            placement: Placement::BelowEditor,
        }
    }

    /// A first launch's tree with the terminal shown: every built-in in it, so `reconcile` has
    /// nothing to say about it.
    fn plain() -> Layout {
        Layout::initial(&[terminal_decl()]).unwrap()
    }

    /// A window as it may be left: a Lua side panel, the terminal shown and its pin settled, the
    /// agent hidden, the keys in the terminal.
    fn left_as() -> (Layout, Vec<ModuleDecl>) {
        let lua = vec![ModuleDecl {
            id: ModuleId::lua("notes"),
            placement: Placement::RightOfRoot,
        }];
        let mut decls = vec![terminal_decl()];
        decls.extend(lua.iter().cloned());
        let mut layout = Layout::initial(&decls).unwrap();
        assert!(settle_pins(&mut layout, &Frame::new(Size { w: 1280, h: 721 }, 1)));
        layout.set_focus(&ModuleId::terminal()).unwrap();
        super::super::geometry::hide(
            &mut layout,
            &ModuleId::agent(),
            &Frame::new(Size { w: 1280, h: 721 }, 1),
        )
        .unwrap();
        (layout, lua)
    }

    /// `layout` alone, as the window's one workspace.
    fn one(layout: &Layout) -> String {
        encode_v2(&Workspaces::single(), layout, &root())
    }

    fn with(names: &[&str], current: &str, layouts: &[Layout]) -> Workspaces {
        let stored = names
            .iter()
            .zip(layouts)
            .filter(|(name, _)| **name != current)
            .map(|(name, layout)| (name.to_string(), layout.clone()))
            .collect();
        Workspaces::from_parts(
            names.iter().map(|s| s.to_string()).collect(),
            stored,
            current.to_string(),
            Kept::default(),
        )
        .unwrap()
    }

    fn json_of(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    fn a_dir(label: &str) -> PathBuf {
        agent::state_dirs::test_workspace_dir(label)
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn sorted(mut names: Vec<String>) -> Vec<String> {
        names.sort();
        names
    }

    /// The on-disk format, byte for byte: every file written depends on these names and this order,
    /// and a change to a `rename_all` or a field's place would pass every round trip while stranding
    /// them with the version unchanged.
    #[test]
    fn the_version_2_format_is_pinned() {
        let golden = r#"{
  "version": 2,
  "project_root": "/home/user/project",
  "current": "main",
  "workspaces": [
    {
      "name": "main",
      "focus": "agent",
      "hidden": [
        "terminal"
      ],
      "tiled": {
        "type": "con",
        "layout": "splitv",
        "children": [
          {
            "share": 0.5,
            "node": {
              "type": "con",
              "layout": "splith",
              "children": [
                {
                  "share": 0.25,
                  "node": {
                    "type": "leaf",
                    "module": "editor"
                  }
                },
                {
                  "share": 0.75,
                  "node": {
                    "type": "leaf",
                    "module": "agent"
                  }
                }
              ]
            }
          },
          {
            "share": 0.5,
            "pin": {
              "px": 240
            },
            "node": {
              "type": "leaf",
              "module": "terminal"
            }
          }
        ]
      }
    }
  ]
}"#;
        let Node::Con(mut tree) = Node::pinned(
            Axis::Column,
            0.5,
            Branch::Second,
            Node::split(
                Axis::Row,
                0.25,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent()),
            ),
            Node::Leaf(ModuleId::terminal()),
        ) else {
            unreachable!()
        };
        tree.children[1].pin = Some(Pin { px: Some(240) });
        let layout = Layout::from_parts(Node::Con(tree), [ModuleId::terminal()].into(), ModuleId::agent()).unwrap();
        assert_eq!(one(&layout), golden);
        let back = decode_v2(golden, &root(), &[]).unwrap();
        assert_eq!(back.current, layout);
        assert_eq!(back.workspaces, Workspaces::single());
        assert!(back.notes.is_empty(), "{:?}", back.notes);
        assert_eq!(back.keep_copy, None);
        let unsettled = golden.replace(r#""px": 240"#, r#""px": null"#);
        let back = decode_v2(&unsettled, &root(), &[]).unwrap();
        assert_eq!(
            back.current.root().children[1].pin,
            Some(Pin { px: None }),
            "a pin with no length yet is `null`"
        );
    }

    #[test]
    fn a_tabbed_container_round_trips_through_the_version_2_file() {
        for kind in [ConLayout::Tabbed, ConLayout::Stacked] {
            let mut c = Con::new(
                kind,
                vec![
                    Child::new(Node::Leaf(ModuleId::agent()), 0.5),
                    Child::new(Node::Leaf(ModuleId::terminal()), 0.5),
                ],
            );
            c.focus = 1;
            c.last_split = Axis::Column;
            let layout = Layout::new(
                Node::split(Axis::Row, 0.5, Node::Leaf(ModuleId::editor()), Node::Con(c)),
                ModuleId::editor(),
            )
            .unwrap();
            let text = one(&layout);
            let mut value = json_of(&text);
            let node = &value["workspaces"][0]["tiled"]["children"][1]["node"];
            assert_eq!(node["layout"], layout_name(kind));
            assert_eq!(node["last_split"], "column");
            assert_eq!(node["active"], 1);
            let back = decode_v2(&text, &root(), &[]).unwrap();
            assert_eq!(back.current.root(), layout.root());
            assert!(back.notes.is_empty());
            assert!(back.keep_copy.is_none());
            value["workspaces"][0]["tiled"]["children"][1]["node"]["active"] = json!(5);
            let back = decode_v2(&value.to_string(), &root(), &[]).unwrap();
            assert_eq!(back.current.root().children[1].node.as_con().unwrap().focus, 0);
            assert_eq!(back.notes, ["container active 5 is past its 2 children; reading 0"]);
            assert_eq!(
                back.keep_copy.as_deref(),
                Some(value.to_string().as_str()),
                "the file's choice of drawn tab is lost at the next write, so it is copied"
            );
            let node = value["workspaces"][0]["tiled"]["children"][1]["node"]
                .as_object_mut()
                .unwrap();
            node.remove("active");
            node.remove("last_split");
            let back = decode_v2(&value.to_string(), &root(), &[]).unwrap();
            assert_eq!(back.current.root().children[1].node.as_con().unwrap().focus, 0);
            assert_eq!(
                back.current.root().children[1].node.as_con().unwrap().last_split,
                Axis::Row
            );
            assert!(back.notes.is_empty());
        }
    }

    #[test]
    fn a_layout_comes_back_from_version_2_as_it_was_left() {
        let (main, lua) = left_as();
        let mut decls = vec![terminal_decl()];
        decls.extend(lua.iter().cloned());
        let mut study = Layout::initial(&decls).unwrap();
        assert!(settle_pins(&mut study, &Frame::new(Size { w: 1920, h: 1040 }, 1)));
        super::super::ops::even(&mut study, Axis::Row);
        study.set_focus(&ModuleId::agent()).unwrap();
        let workspaces = with(&["main", "study"], "main", &[main.clone(), study.clone()]);
        let text = encode_v2(&workspaces, &main, &root());
        assert!(text.contains("\"px\": 240"), "the pinned height is in the file: {text}");

        let back = decode_v2(&text, &root(), &lua).unwrap();
        assert!(back.notes.is_empty(), "{:?}", back.notes);
        assert_eq!(back.keep_copy, None);
        assert_eq!(back.workspaces.order(), ["main", "study"]);
        assert_eq!(back.workspaces.current(), "main");
        assert_eq!(back.current.root(), main.root());
        assert_eq!(back.current.hidden(), main.hidden());
        assert_eq!(back.current.focus(), &ModuleId::terminal());
        let back_study = back.workspaces.stored("study").unwrap();
        assert_eq!(back_study.root(), study.root());
        assert_eq!(back_study.hidden(), study.hidden());
        assert_eq!(back_study.focus(), &ModuleId::agent());
        assert_eq!(
            encode_v2(&back.workspaces, &back.current, &root()),
            text,
            "and again, byte for byte"
        );
    }

    /// An editor retired because nvim ended (`prefix x`, `:qa`, a crash) is saved as it was before --
    /// shown -- so a relaunch opens with the editor, not without it until `prefix e`. One the user had
    /// hidden stays hidden: the retirement is not an arrangement choice, the hide was.
    #[test]
    fn a_retired_editor_is_saved_as_it_was_before_nvim_ended() {
        let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
        let mut layout = plain();
        super::super::kill(&mut layout, &ModuleId::editor(), super::super::Reopen::Never, &frame).unwrap();
        assert!(layout.is_gone(&ModuleId::editor()), "the case");
        let back = decode_v2(&one(&layout), &root(), &[]).unwrap();
        assert!(
            back.current.is_shown(&ModuleId::editor()),
            "a relaunch shows the editor"
        );

        let mut layout = plain();
        super::super::geometry::hide(&mut layout, &ModuleId::editor(), &frame).unwrap();
        super::super::kill(&mut layout, &ModuleId::editor(), super::super::Reopen::Never, &frame).unwrap();
        let back = decode_v2(&one(&layout), &root(), &[]).unwrap();
        assert!(
            back.current.hidden().contains(&ModuleId::editor()),
            "one the user had hidden stays hidden"
        );
    }

    #[test]
    fn a_zoom_is_not_stored() {
        let (mut layout, lua) = left_as();
        layout.toggle_zoom(&ModuleId::terminal());
        assert!(layout.zoomed().is_some());
        let back = decode_v2(&one(&layout), &root(), &lua).unwrap();
        assert_eq!(back.current.zoomed(), None);
    }

    /// Every way a whole file can be unusable degrades to an `Err` that says why -- never a panic,
    /// never a layout that breaks an invariant.
    #[test]
    fn an_unusable_file_says_why_and_is_never_half_read() {
        let (layout, lua) = left_as();
        let good = one(&layout);
        let mut no_current = json_of(&good);
        no_current.as_object_mut().unwrap().remove("current");
        let mut bad_share = json_of(&good);
        bad_share["workspaces"][0]["tiled"]["children"][0]["share"] = json!(0.99);
        let cases: Vec<(String, &str)> = vec![
            ("{ not json".to_string(), "not a layout file"),
            ("[1, 2]".to_string(), "not a layout file"),
            (
                good.replace("\"version\": 2", "\"version\": 1"),
                "version 1 (this build reads 2)",
            ),
            (good.replace("\"version\": 2", "\"version\": 3"), "version 3"),
            (
                good.replace("/home/user/project", "/home/user/other"),
                "written for /home/user/other",
            ),
            (no_current.to_string(), "not a layout file (no `current`)"),
            (
                good.replace("\"workspaces\": [", "\"workspaces\": [], \"was\": ["),
                "no workspace can be used",
            ),
            (
                good.replace("\"module\": \"editor\"", "\"module\": \"chat\""),
                "unknown module 'chat'",
            ),
            (
                good.replace("\"module\": \"editor\"", "\"module\": \"lua:gone\""),
                "no 'editor'",
            ),
            (
                good.replace("\"focus\": \"terminal\"", "\"focus\": \"\""),
                "unknown module ''",
            ),
            (bad_share.to_string(), "add up to"),
        ];
        for (text, why) in cases {
            let err = decode_v2(&text, &root(), &lua).unwrap_err();
            assert!(err.contains(why), "expected {why:?} in {err:?}");
        }
    }

    /// A Lua panel removed from `init.lua` since the file was written is left out, and the notes say
    /// so; the rest of the window comes back. Nothing of the file is lost that this window could keep,
    /// so no copy is asked for.
    #[test]
    fn a_panel_no_longer_registered_is_left_out_and_said() {
        let (layout, _) = left_as();
        let back = decode_v2(&one(&layout), &root(), &[]).unwrap();
        assert!(!back.current.contains(&ModuleId::lua("notes")));
        assert_eq!(
            back.notes,
            ["workspace 'main': left out 'lua:notes': this window has no such module"]
        );
        assert_eq!(back.keep_copy, None);
    }

    // -- version 1 --------------------------------------------------------------------------------

    fn leaf(module: &str) -> Value {
        json!({"type": "leaf", "module": module})
    }

    fn split(axis: &str, ratio: f32, first: Value, second: Value) -> Value {
        json!({"type": "split", "axis": axis, "ratio": ratio, "first": first, "second": second})
    }

    fn pinned(axis: &str, ratio: f32, side: &str, px: Option<i32>, first: Value, second: Value) -> Value {
        json!({"type": "split", "axis": axis, "ratio": ratio, "pin": {"side": side, "px": px},
               "first": first, "second": second})
    }

    fn v1_text(tree: Value, hidden: &[&str], focus: &str) -> String {
        serde_json::to_string_pretty(&json!({
            "version": 1,
            "project_root": "/home/user/project",
            "root": tree,
            "hidden": hidden,
            "focus": focus,
        }))
        .unwrap()
    }

    /// The test's own reading of a version 1 text into the frozen binary tree, independent of
    /// `decode_v1`, so the two can be compared.
    fn v1_from_json(text: &str) -> V1Node {
        fn node(v: &Value) -> V1Node {
            match v["type"].as_str().unwrap() {
                "leaf" => V1Node::Leaf(ModuleId::parse(v["module"].as_str().unwrap()).unwrap()),
                "split" => V1Node::Split {
                    axis: match v["axis"].as_str().unwrap() {
                        "row" => Axis::Row,
                        "column" => Axis::Column,
                        other => panic!("axis {other}"),
                    },
                    ratio: v["ratio"].as_f64().unwrap() as f32,
                    pin: v.get("pin").filter(|p| !p.is_null()).map(|p| V1Pin {
                        side: match p["side"].as_str().unwrap() {
                            "first" => Branch::First,
                            "second" => Branch::Second,
                            other => panic!("side {other}"),
                        },
                        px: p["px"].as_i64().map(|px| px as i32),
                    }),
                    first: Box::new(node(&v["first"])),
                    second: Box::new(node(&v["second"])),
                },
                other => panic!("type {other}"),
            }
        }
        node(&json_of(text)["root"])
    }

    fn hidden_of(text: &str) -> BTreeSet<ModuleId> {
        json_of(text)["hidden"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| ModuleId::parse(id.as_str().unwrap()).unwrap())
            .collect()
    }

    const EDITOR_SHARE: f32 = 760.0 / 1279.0;
    const BELOW: f32 = 2.0 / 3.0;

    /// Five version 1 files as builds before workspaces left them, each with the Lua panels its
    /// window registered.
    fn v1_cases() -> Vec<(&'static str, String, Vec<ModuleDecl>)> {
        let editor_column = |px| pinned("column", BELOW, "second", px, leaf("editor"), leaf("terminal"));
        vec![
            (
                "the default window",
                v1_text(
                    split("row", EDITOR_SHARE, editor_column(None), leaf("agent")),
                    &["terminal"],
                    "editor",
                ),
                vec![],
            ),
            (
                "the chat moved left of the editor",
                v1_text(split("row", 0.4, leaf("agent"), editor_column(Some(240))), &[], "agent"),
                vec![],
            ),
            (
                "a Lua bottom panel",
                v1_text(
                    pinned(
                        "column",
                        0.75,
                        "second",
                        Some(180),
                        split("row", EDITOR_SHARE, editor_column(None), leaf("agent")),
                        leaf("lua:logs"),
                    ),
                    &["terminal"],
                    "editor",
                ),
                vec![ModuleDecl {
                    id: ModuleId::lua("logs"),
                    placement: Placement::BelowRoot,
                }],
            ),
            (
                "an evened row with the hidden terminal row kept",
                v1_text(
                    pinned(
                        "column",
                        BELOW,
                        "second",
                        Some(200),
                        split(
                            "row",
                            1.0 / 3.0,
                            leaf("editor"),
                            split("row", 0.5, leaf("agent"), leaf("lua:side")),
                        ),
                        leaf("terminal"),
                    ),
                    &["terminal"],
                    "editor",
                ),
                vec![ModuleDecl {
                    id: ModuleId::lua("side"),
                    placement: Placement::RightOfRoot,
                }],
            ),
            (
                "a retired editor, saved shown",
                v1_text(
                    split("row", EDITOR_SHARE, editor_column(Some(240)), leaf("agent")),
                    &[],
                    "terminal",
                ),
                vec![],
            ),
        ]
    }

    /// Migration converts each binary split into a two-child container, so the window opens
    /// pixel-identical to the binary tree it was: every module's rectangle and every divider, at all
    /// three window sizes, against the frozen binary geometry.
    #[test]
    fn a_version_1_file_migrates_pixel_for_pixel() {
        for (case, text, lua) in v1_cases() {
            let decoded = decode_v1(&text, &root(), &lua).unwrap_or_else(|e| panic!("{case}: {e}"));
            assert!(decoded.notes.is_empty(), "{case}: {:?}", decoded.notes);
            let old = v1_from_json(&text);
            let hidden = hidden_of(&text);
            assert_eq!(decoded.layout.hidden(), &hidden, "{case}");
            for size in FRAMES {
                let frame = Frame::new(size, 1);
                let (modules, dividers) = v1_arrange(&old, &|id| !hidden.contains(id), &frame);
                let new = arrange(&decoded.layout, &frame);
                assert_eq!(modules, new.modules, "{case} at {}x{}", size.w, size.h);
                assert_eq!(dividers.len(), new.dividers.len(), "{case} at {}x{}", size.w, size.h);
                for (a, b) in dividers.iter().zip(&new.dividers) {
                    assert_eq!(
                        (a.rect, a.axis, a.avail, a.first_px, a.min_first, a.min_second),
                        (b.rect, b.axis, b.avail, b.first_px, b.min_first, b.min_second),
                        "{case} at {}x{}",
                        size.w,
                        size.h
                    );
                }
            }
        }
    }

    /// The version 1 format as builds before workspaces wrote it is still read, field for field.
    #[test]
    fn the_version_1_format_is_still_read() {
        let golden = r#"{
  "version": 1,
  "project_root": "/home/user/project",
  "root": {
    "type": "split",
    "axis": "column",
    "ratio": 0.5,
    "pin": {
      "side": "second",
      "px": 240
    },
    "first": {
      "type": "split",
      "axis": "row",
      "ratio": 0.25,
      "first": {
        "type": "leaf",
        "module": "editor"
      },
      "second": {
        "type": "leaf",
        "module": "agent"
      }
    },
    "second": {
      "type": "leaf",
      "module": "terminal"
    }
  },
  "hidden": [
    "terminal"
  ],
  "focus": "agent"
}"#;
        let Node::Con(mut tree) = Node::pinned(
            Axis::Column,
            0.5,
            Branch::Second,
            Node::split(
                Axis::Row,
                0.25,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent()),
            ),
            Node::Leaf(ModuleId::terminal()),
        ) else {
            unreachable!()
        };
        tree.children[1].pin = Some(Pin { px: Some(240) });
        let want = Layout::from_parts(Node::Con(tree), [ModuleId::terminal()].into(), ModuleId::agent()).unwrap();
        let back = decode_v1(golden, &root(), &[]).unwrap();
        assert!(back.notes.is_empty(), "{:?}", back.notes);
        assert_eq!(back.layout, want);

        let cases: Vec<(String, &str)> = vec![
            ("{ not json".to_string(), "not a layout file"),
            (
                golden.replace("\"version\": 1", "\"version\": 2"),
                "version 2 (this build migrates 1)",
            ),
            (
                golden.replace("/home/user/project", "/home/user/other"),
                "written for /home/user/other",
            ),
            (
                golden.replace("\"module\": \"agent\"", "\"module\": \"chat\""),
                "unknown module 'chat'",
            ),
            (
                golden.replace("\"module\": \"editor\"", "\"module\": \"lua:gone\""),
                "no 'editor'",
            ),
            (golden.replace("\"ratio\": 0.25", "\"ratio\": 0.99"), "ratio 0.99"),
        ];
        for (text, why) in cases {
            let err = decode_v1(&text, &root(), &[]).unwrap_err();
            assert!(err.contains(why), "expected {why:?} in {err:?}");
        }
    }

    /// The version 1 file belongs to the builds before workspaces: opening it and saving leaves its
    /// bytes and its modification time as they were, and the only file written beside it is the
    /// version 2 one.
    #[test]
    fn the_version_1_file_is_never_written() {
        let dir = a_dir("layout-v1-untouched");
        let v1 = dir.join(file_name(&root()));
        let (_, text, _) = v1_cases().remove(4);
        std::fs::write(&v1, &text).unwrap();
        let mtime = std::fs::metadata(&v1).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));

        let Loaded::Migrated(opened) = load(&dir, &root(), &[]) else {
            panic!("the version 1 file, migrated")
        };
        save(&dir, &root(), &opened.workspaces, &opened.current).unwrap();
        let Loaded::Restored(again) = load(&dir, &root(), &[]) else {
            panic!("the version 2 file, now")
        };
        save(&dir, &root(), &again.workspaces, &again.current).unwrap();
        assert_eq!(std::fs::read_to_string(&v1).unwrap(), text);
        assert_eq!(std::fs::metadata(&v1).unwrap().modified().unwrap(), mtime);
        assert_eq!(names_in(&dir), sorted(vec![file_name(&root()), file_name_v2(&root())]));
    }

    #[test]
    fn the_load_order_is_v2_then_v1_then_nothing() {
        let dir = a_dir("layout-load-order");
        assert!(matches!(load(&dir, &root(), &[]), Loaded::Missing), "neither file");

        let (_, v1, _) = v1_cases().remove(1);
        std::fs::write(dir.join(file_name(&root())), &v1).unwrap();
        match load(&dir, &root(), &[]) {
            Loaded::Migrated(opened) => {
                assert_eq!(opened.notes, ["migrated from the version 1 layout file"]);
                assert_eq!(opened.workspaces, Workspaces::single());
                assert_eq!(
                    opened.current.visible_leaves()[0],
                    ModuleId::agent(),
                    "the chat on the left"
                );
                assert_eq!(opened.keep_copy, None);
            }
            other => panic!("{other:?}"),
        }

        save(&dir, &root(), &Workspaces::single(), &plain()).unwrap();
        match load(&dir, &root(), &[]) {
            Loaded::Restored(opened) => {
                assert_eq!(opened.current.root(), plain().root(), "the version 2 file wins");
                assert!(opened.notes.is_empty(), "{:?}", opened.notes);
            }
            other => panic!("{other:?}"),
        }

        let v2 = dir.join(file_name_v2(&root()));
        std::fs::write(&v2, "{ a typo").unwrap();
        match load(&dir, &root(), &[]) {
            Loaded::Migrated(opened) => {
                assert_eq!(opened.notes.len(), 2, "{:?}", opened.notes);
                assert_eq!(opened.notes[0], "migrated from the version 1 layout file");
                assert!(
                    opened.notes[1].starts_with(&format!("{}: not a layout file", v2.display()))
                        && opened.notes[1]
                            .ends_with("; it is set aside as .v2.json.unusable the first time this window saves"),
                    "{:?}",
                    opened.notes
                );
            }
            other => panic!("{other:?}"),
        }

        std::fs::write(dir.join(file_name(&root())), "{ another typo").unwrap();
        match load(&dir, &root(), &[]) {
            Loaded::Unusable(why) => {
                assert!(why.contains(&file_name_v2(&root())), "{why}");
                assert!(
                    why.contains(&format!("{}: not a layout file", file_name(&root()))),
                    "{why}"
                );
            }
            other => panic!("{other:?}"),
        }

        std::fs::remove_file(dir.join(file_name(&root()))).unwrap();
        match load(&dir, &root(), &[]) {
            Loaded::Unusable(why) => assert!(why.starts_with(&v2.display().to_string()), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    /// A good file in which one workspace cannot be used: the rest open, the note says which and why,
    /// and the exact text is asked to be copied before anything writes over it.
    #[test]
    fn one_unusable_workspace_is_left_out_and_a_copy_asked_for() {
        let workspaces = with(&["main", "study"], "main", &[plain(), plain()]);
        let mut value = json_of(&encode_v2(&workspaces, &plain(), &root()));
        assert_eq!(
            value["workspaces"][1]["tiled"]["children"][1]["node"]["module"],
            "agent"
        );
        value["workspaces"][1]["tiled"]["children"][1]["node"]["module"] = json!("chat");
        let text = serde_json::to_string_pretty(&value).unwrap();
        let back = decode_v2(&text, &root(), &[]).unwrap();
        assert_eq!(back.workspaces.order(), ["main"]);
        assert_eq!(back.notes, ["workspace 'study' left out: unknown module 'chat'"]);
        assert_eq!(back.keep_copy.as_deref(), Some(text.as_str()));

        value["current"] = json!("study");
        let back = decode_v2(&value.to_string(), &root(), &[]).unwrap();
        assert_eq!(back.workspaces.current(), "main", "the first remaining one is current");
        assert_eq!(
            back.notes,
            [
                "workspace 'study' left out: unknown module 'chat'",
                "workspace 'study' was current and cannot be used; 'main' is current"
            ]
        );

        value["workspaces"][0]["tiled"]["children"][1]["node"]["module"] = json!("chat");
        let err = decode_v2(&value.to_string(), &root(), &[]).unwrap_err();
        assert!(err.starts_with("no workspace can be used"), "{err}");

        // A name that is invalid, or repeats an earlier one, is left out the same way.
        let mut value = json_of(&encode_v2(&workspaces, &plain(), &root()));
        value["workspaces"][1]["name"] = json!("main");
        let back = decode_v2(&value.to_string(), &root(), &[]).unwrap();
        assert_eq!(back.workspaces.order(), ["main"]);
        assert_eq!(
            back.notes,
            ["workspace 'main' left out: an earlier workspace has that name"]
        );
        assert!(back.keep_copy.is_some());
        value["workspaces"][1]["name"] = json!("a\nb");
        let back = decode_v2(&value.to_string(), &root(), &[]).unwrap();
        assert_eq!(back.workspaces.order(), ["main"]);
        assert_eq!(
            back.notes,
            [
                "workspace 'a\\nb' left out: 'a\\nb' is not a workspace name (1 to 32 characters, no control \
                 characters)"
            ],
            "the name is escaped in the reason too, not only in the label"
        );
        assert!(back.keep_copy.is_some());

        // Whatever of the file's own text a note repeats -- a key, a layout's name -- reaches the log
        // line escaped.
        let mut value = json_of(&one(&plain()));
        value["workspaces"][0]["tiled"]["children"][0]["node"]["layout"] = json!("x\u{1b}[2Jy");
        value["workspaces"][0]["tiled"]["children"][1]["node"]["\u{1b}]0;title\u{7}"] = json!(1);
        let back = decode_v2(&value.to_string(), &root(), &[]).unwrap();
        assert!(
            back.notes.iter().all(|note| !note.chars().any(char::is_control)),
            "{:?}",
            back.notes
        );
        assert!(
            back.notes
                .contains(&"container layout 'x\\u{1b}[2Jy' read as splith".to_string()),
            "{:?}",
            back.notes
        );
    }

    #[test]
    fn an_unknown_container_layout_is_read_as_splith_and_a_copy_asked_for() {
        let mut value = json_of(&one(&plain()));
        let inner = &mut value["workspaces"][0]["tiled"]["children"][0]["node"];
        assert_eq!(inner["layout"], "splitv");
        inner["layout"] = json!("scroll");
        let text = value.to_string();
        let back = decode_v2(&text, &root(), &[]).unwrap();
        assert_eq!(back.notes, ["container layout 'scroll' read as splith"]);
        assert_eq!(back.keep_copy.as_deref(), Some(text.as_str()));
        let Node::Con(inner) = &back.current.root().children[0].node else {
            panic!("a container")
        };
        assert_eq!(inner.layout, ConLayout::SplitH);

        // A key this build does not know inside a tree has nowhere to be kept: said, and copied.
        let mut value = json_of(&one(&plain()));
        value["workspaces"][0]["tiled"]["focus"] = json!(0);
        value["workspaces"][0]["tiled"]["children"][1]["weight"] = json!(2);
        value["workspaces"][0]["tiled"]["children"][1]["node"]["title"] = json!("chat");
        let text = value.to_string();
        let back = decode_v2(&text, &root(), &[]).unwrap();
        assert_eq!(
            back.notes,
            [
                "focus inside workspace 'main' is not kept",
                "weight inside workspace 'main' is not kept",
                "title inside workspace 'main' is not kept"
            ]
        );
        assert_eq!(back.keep_copy.as_deref(), Some(text.as_str()));
        assert_eq!(back.current.root(), plain().root());
    }

    /// A hand-edited container of one child is normalised, with a note, rather than costing the
    /// workspace; nothing is lost, so no copy is asked for.
    #[test]
    fn a_one_child_container_in_a_file_is_normalised() {
        let text = json!({
            "version": 2,
            "project_root": "/home/user/project",
            "current": "main",
            "workspaces": [{
                "name": "main",
                "focus": "editor",
                "hidden": [],
                "tiled": {"type": "con", "layout": "splith", "children": [
                    {"share": 0.5, "node": {"type": "con", "layout": "splitv", "children": [
                        {"share": 1.0, "node": {"type": "leaf", "module": "editor"}}
                    ]}},
                    {"share": 0.5, "node": {"type": "con", "layout": "splitv", "children": [
                        {"share": 0.5, "node": {"type": "leaf", "module": "agent"}},
                        {"share": 0.5, "node": {"type": "leaf", "module": "terminal"}},
                        {"share": 0.0, "node": {"type": "con", "layout": "splith", "children": []}}
                    ]}}
                ]}
            }]
        })
        .to_string();
        let back = decode_v2(&text, &root(), &[]).unwrap();
        assert_eq!(back.notes, ["normalised a container in workspace 'main'"]);
        assert_eq!(back.keep_copy, None);
        let want = Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(ModuleId::editor()),
            Node::split(
                Axis::Column,
                0.5,
                Node::Leaf(ModuleId::agent()),
                Node::Leaf(ModuleId::terminal()),
            ),
        );
        assert_eq!(Node::Con(back.current.root().clone()), want);
    }

    /// Shares are checked as written, before normalising: dissolving a one-child container drops its
    /// share, and dropping an empty child rescales the rest, so a wrong sum would otherwise open as a
    /// right one, ask for no copy, and be written over.
    #[test]
    fn shares_that_do_not_add_up_fail_the_workspace_before_normalising() {
        let file = |tiled: Value| {
            json!({
                "version": 2,
                "project_root": "/home/user/project",
                "current": "main",
                "workspaces": [
                    {"name": "main", "focus": "editor", "hidden": [],
                     "tiled": json_of(&one(&plain()))["workspaces"][0]["tiled"].clone()},
                    {"name": "study", "focus": "editor", "hidden": [], "tiled": tiled}
                ]
            })
            .to_string()
        };
        let lonely = json!({"type": "con", "layout": "splith", "children": [
            {"share": 0.5, "node": {"type": "con", "layout": "splitv", "children": [
                {"share": 0.2, "node": {"type": "leaf", "module": "editor"}}
            ]}},
            {"share": 0.5, "node": {"type": "con", "layout": "splitv", "children": [
                {"share": 0.5, "node": {"type": "leaf", "module": "agent"}},
                {"share": 0.5, "node": {"type": "leaf", "module": "terminal"}}
            ]}}
        ]});
        let text = file(lonely);
        let back = decode_v2(&text, &root(), &[]).unwrap();
        assert_eq!(back.workspaces.order(), ["main"]);
        assert_eq!(
            back.notes,
            ["workspace 'study' left out: a container's shares add up to 0.2, not 1"]
        );
        assert_eq!(back.keep_copy.as_deref(), Some(text.as_str()));

        let beside_an_empty_one = json!({"type": "con", "layout": "splith", "children": [
            {"share": 0.6, "node": {"type": "leaf", "module": "editor"}},
            {"share": 0.6, "node": {"type": "con", "layout": "splitv", "children": [
                {"share": 0.5, "node": {"type": "leaf", "module": "agent"}},
                {"share": 0.5, "node": {"type": "leaf", "module": "terminal"}}
            ]}},
            {"share": 0.2, "node": {"type": "con", "layout": "splith", "children": []}}
        ]});
        let text = file(beside_an_empty_one);
        let back = decode_v2(&text, &root(), &[]).unwrap();
        assert_eq!(back.workspaces.order(), ["main"]);
        assert_eq!(back.notes.len(), 1, "{:?}", back.notes);
        assert!(
            back.notes[0].starts_with("workspace 'study' left out: a container's shares add up to 1.4"),
            "{:?}",
            back.notes
        );
        assert!(back.keep_copy.is_some());
    }

    /// `depth` containers nested around a two-level tree of the built-ins, each beside one more
    /// module, alternating rows and columns so that none merges into its parent: the shape a window
    /// with many side panels reaches as they are moved around.
    fn nested(depth: usize) -> (Layout, Vec<ModuleDecl>) {
        let lua: Vec<ModuleDecl> = (0..depth)
            .map(|i| ModuleDecl {
                id: ModuleId::lua(&format!("p{i}")),
                placement: Placement::RightOfRoot,
            })
            .collect();
        // Every built-in in the tree, so that reading it back adds none.
        let mut tree = Node::split(
            Axis::Row,
            0.5,
            Node::split(
                Axis::Column,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::terminal()),
            ),
            Node::Leaf(ModuleId::agent()),
        );
        for (i, decl) in lua.iter().enumerate() {
            let axis = if i % 2 == 0 { Axis::Column } else { Axis::Row };
            tree = Node::split(axis, 0.5, tree, Node::Leaf(decl.id.clone()));
        }
        let layout = Layout::from_parts(tree, BTreeSet::new(), ModuleId::editor()).unwrap();
        (layout, lua)
    }

    /// A tree deeper than serde_json's default limit of 128 levels -- three per container -- comes
    /// back as it was written, rather than costing the whole file.
    #[test]
    fn a_deep_tree_comes_back_from_the_file_it_was_written_to() {
        let dir = a_dir("layout-deep");
        let (layout, lua) = nested(45);
        let saved = save(&dir, &root(), &Workspaces::single(), &layout).unwrap();
        let text = std::fs::read_to_string(&saved).unwrap();
        assert!(nesting(&text) > 128, "{}", nesting(&text));
        let Loaded::Restored(back) = load(&dir, &root(), &lua) else {
            panic!("the file it wrote is read back")
        };
        assert_eq!(back.current.root(), layout.root());
        assert_eq!(back.keep_copy, None);
    }

    /// Nesting is bounded before the parse, which recurses once per level: a file at the bound is
    /// read, one past it is refused with a reason, and a hostile one does not reach the parser. The
    /// writer refuses what the reader would.
    #[test]
    fn nesting_is_bounded_alike_for_reading_and_writing() {
        // A kept top-level key nested so that the file is `levels` deep, its own object being one.
        let with_kept = |levels: usize| {
            let text = one(&plain());
            let body = text.strip_suffix("\n}").unwrap();
            format!(
                "{body},\n  \"deep\": {}{}\n}}",
                "[".repeat(levels - 1),
                "]".repeat(levels - 1)
            )
        };
        let at = with_kept(MAX_NESTING);
        assert_eq!(nesting(&at), MAX_NESTING);
        let back = decode_v2(&at, &root(), &[]).unwrap();
        assert!(back.workspaces.kept().top.contains_key("deep"));

        let past = with_kept(MAX_NESTING + 1);
        let err = decode_v2(&past, &root(), &[]).unwrap_err();
        assert!(err.contains("nested 513 levels deep"), "{err}");
        let hostile = with_kept(200_000);
        assert!(decode_v2(&hostile, &root(), &[]).is_err());
        assert_eq!(
            nesting(r#"{"a": "[[[[{{", "b": "\"[["}"#),
            1,
            "brackets inside strings do not count"
        );

        let dir = a_dir("layout-too-deep");
        // Three levels per container and four around the root's leaves: 169 containers nest 511
        // deep, 170 nest 514.
        let (layout, _) = nested(167);
        assert_eq!(nesting(&one(&layout)), 511);
        save(&dir, &root(), &Workspaces::single(), &layout).unwrap();
        let before = file_text_in(&dir);
        let (deeper, _) = nested(168);
        assert_eq!(nesting(&one(&deeper)), 514);
        let err = save(&dir, &root(), &Workspaces::single(), &deeper).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(file_text_in(&dir), before, "the file already there stays");
        assert_eq!(names_in(&dir), [file_name_v2(&root())], "and no temporary file is left");
    }

    fn file_text_in(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(file_name_v2(&root()))).unwrap()
    }

    /// What a later build keeps in a workspace and at the top level comes back unchanged, in its
    /// workspace, after this build's keys; carrying it asks for no copy.
    #[test]
    fn a_later_builds_unknown_keys_survive_a_save() {
        let mut value = json_of(&one(&plain()));
        let floating = json!([{"module": "terminal#2", "cx": 0.5, "cy": 0.45, "w": 900, "h": 420, "scratch": true}]);
        value["workspaces"][0]["floating"] = floating.clone();
        value["workspaces"][0]["float_memory"] = json!([]);
        value["future"] = json!(1);
        let back = decode_v2(&value.to_string(), &root(), &[]).unwrap();
        assert!(back.notes.is_empty(), "{:?}", back.notes);
        assert_eq!(back.keep_copy, None);

        let written = encode_v2(&back.workspaces, &back.current, &root());
        let again = json_of(&written);
        assert_eq!(again["workspaces"][0]["floating"], floating);
        assert_eq!(again["workspaces"][0]["float_memory"], json!([]));
        assert_eq!(again["future"], json!(1));
        assert!(
            written.find("\"floating\"") > written.find("\"tiled\""),
            "kept keys follow the ones this build writes: {written}"
        );
        assert!(written.find("\"future\"") > written.find("\"workspaces\""), "{written}");
        let want: Vec<ModuleId> = ["terminal#2"].iter().filter_map(|t| ModuleId::parse(t).ok()).collect();
        assert_eq!(back.workspaces.kept_modules(), want, "what parses of the kept data");
    }

    /// A kept key naming a module this window places itself -- a later build's floating terminal,
    /// while this window places the terminal tiled -- would put that module in two places when
    /// written back. It is not kept, the note says so, and a copy is asked for.
    #[test]
    fn a_kept_key_naming_a_module_this_window_places_is_copied_not_kept() {
        let mut value = json_of(&one(&Layout::initial(&[]).unwrap()));
        value["workspaces"][0]["floating"] = json!([{"module": "terminal", "cx": 0.5, "cy": 0.5, "w": 900, "h": 420}]);
        let text = value.to_string();
        let back = decode_v2(&text, &root(), &[]).unwrap();
        assert!(back
            .workspaces
            .kept_for("main")
            .is_none_or(|kept| !kept.contains_key("floating")));
        assert!(
            back.notes.contains(
                &"floating of workspace 'main' names 'terminal', which this version places itself; a copy is kept"
                    .to_string()
            ),
            "{:?}",
            back.notes
        );
        assert!(
            back.notes.contains(
                &"workspace 'main': placed 'terminal' below the editor, hidden, as a first launch does".to_string()
            ),
            "{:?}",
            back.notes
        );
        assert_eq!(back.keep_copy.as_deref(), Some(text.as_str()));
        assert!(back.current.contains(&ModuleId::terminal()) && !back.current.is_shown(&ModuleId::terminal()));
        let written = encode_v2(&back.workspaces, &back.current, &root());
        assert_eq!(written.matches("\"module\": \"terminal\"").count(), 1, "{written}");
        assert!(!written.contains("floating"), "{written}");

        // The same for a registered Lua panel the file's tree lacks.
        let lua = [ModuleDecl {
            id: ModuleId::lua("notes"),
            placement: Placement::RightOfRoot,
        }];
        let mut value = json_of(&one(&plain()));
        value["workspaces"][0]["floating"] = json!([{"module": "lua:notes", "w": 300, "h": 200}]);
        let text = value.to_string();
        let back = decode_v2(&text, &root(), &lua).unwrap();
        assert!(back
            .workspaces
            .kept_for("main")
            .is_none_or(|kept| !kept.contains_key("floating")));
        assert!(
            back.notes.contains(
                &"floating of workspace 'main' names 'lua:notes', which this version places itself; a copy is kept"
                    .to_string()
            ),
            "{:?}",
            back.notes
        );
        assert_eq!(back.keep_copy.as_deref(), Some(text.as_str()));
        let written = encode_v2(&back.workspaces, &back.current, &root());
        assert_eq!(written.matches("\"module\": \"lua:notes\"").count(), 1, "{written}");
    }

    /// The window writes what it has: a workspace it removed or renamed stays so.
    #[test]
    fn a_deleted_or_renamed_workspace_stays_so() {
        let dir = a_dir("layout-v2-renamed");
        save(
            &dir,
            &root(),
            &with(&["main", "study"], "main", &[plain(), plain()]),
            &plain(),
        )
        .unwrap();
        save(
            &dir,
            &root(),
            &with(&["main", "review"], "main", &[plain(), plain()]),
            &plain(),
        )
        .unwrap();
        let Loaded::Restored(back) = load(&dir, &root(), &[]) else {
            panic!("the file")
        };
        assert_eq!(back.workspaces.order(), ["main", "review"]);
    }

    /// Two windows on one project: each writes all of its workspaces and nothing of the file is merged
    /// in, so the later writer's file is the whole file.
    #[test]
    fn the_later_writer_wins_the_whole_file() {
        let dir = a_dir("layout-v2-later-writer");
        let a = with(&["main", "study"], "main", &[plain(), plain()]);
        let b = Workspaces::single();
        let mut b_layout = plain();
        super::super::ops::even(&mut b_layout, Axis::Row);
        save(&dir, &root(), &a, &plain()).unwrap();
        save(&dir, &root(), &b, &b_layout).unwrap();
        assert_eq!(
            read_v2_text(&dir, &root()).unwrap().unwrap(),
            encode_v2(&b, &b_layout, &root())
        );
        let Loaded::Restored(back) = load(&dir, &root(), &[]) else {
            panic!("the file")
        };
        assert_eq!(back.workspaces.order(), ["main"]);
    }

    /// A partial copy and a whole-file set-aside have names of their own, so neither ever replaces
    /// the other; each replaces only an earlier file of its own name.
    #[test]
    fn copy_aside_and_set_aside_never_overwrite_each_other() {
        use std::os::unix::fs::PermissionsExt;
        let dir = a_dir("layout-v2-asides");
        let path = dir.join(file_name_v2(&root()));
        std::fs::write(&path, "partly").unwrap();
        let partial = copy_aside_v2(&dir, &root(), "partly").unwrap();
        assert_eq!(partial, dir.join(format!("{}.partial", file_name_v2(&root()))));
        assert_eq!(std::fs::metadata(&partial).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::write(&path, "{ wholly unusable").unwrap();
        let unusable = set_aside_v2(&dir, &root()).unwrap();
        assert_eq!(unusable, dir.join(format!("{}.unusable", file_name_v2(&root()))));
        assert_eq!(std::fs::read_to_string(&partial).unwrap(), "partly");
        assert_eq!(std::fs::read_to_string(&unusable).unwrap(), "{ wholly unusable");
        assert!(!path.exists());

        copy_aside_v2(&dir, &root(), "partly, later").unwrap();
        assert_eq!(std::fs::read_to_string(&partial).unwrap(), "partly, later");
        assert_eq!(std::fs::read_to_string(&unusable).unwrap(), "{ wholly unusable");
        std::fs::write(&path, "{ unusable again").unwrap();
        set_aside_v2(&dir, &root()).unwrap();
        assert_eq!(std::fs::read_to_string(&unusable).unwrap(), "{ unusable again");
        assert_eq!(std::fs::read_to_string(&partial).unwrap(), "partly, later");
        assert_eq!(
            names_in(&dir),
            sorted(vec![
                format!("{}.partial", file_name_v2(&root())),
                format!("{}.unusable", file_name_v2(&root()))
            ]),
            "no .tmp is left behind"
        );
    }

    /// What `v2_state` reports of the file on disk, and the kept keys it carries: an entry for every
    /// workspace the file holds, empty or not.
    #[test]
    fn v2_state_reads_the_file_as_it_is() {
        let dir = a_dir("layout-v2-state");
        assert_eq!(v2_state(&dir, &root(), &[]), V2State::Missing);
        let mut value = json_of(&encode_v2(
            &with(&["main", "study"], "main", &[plain(), plain()]),
            &plain(),
            &root(),
        ));
        value["workspaces"][1]["floating"] = json!([]);
        let path = dir.join(file_name_v2(&root()));
        std::fs::write(&path, value.to_string()).unwrap();
        let V2State::Usable(text, kept) = v2_state(&dir, &root(), &[]) else {
            panic!("usable")
        };
        assert_eq!(kept.per_workspace["main"], Map::new());
        assert_eq!(kept.per_workspace["study"]["floating"], json!([]));
        assert_eq!(text, value.to_string());

        value["workspaces"][0]["tiled"]["layout"] = json!("scroll");
        std::fs::write(&path, value.to_string()).unwrap();
        assert!(matches!(v2_state(&dir, &root(), &[]), V2State::Partly(text, _) if text == value.to_string()));
        std::fs::write(&path, "{").unwrap();
        assert!(matches!(v2_state(&dir, &root(), &[]), V2State::Unusable(_)));
    }

    /// Into a state directory that does not exist yet -- a first save, where `~/.local/state/eitri`
    /// has other directories and no `layout/`.
    #[test]
    fn save_then_load_round_trips_and_leaves_no_temporary_file() {
        let dir = a_dir("layout-persist").join("eitri/layout");
        assert!(!dir.exists());
        let (layout, lua) = left_as();
        let path = save(&dir, &root(), &Workspaces::single(), &layout).unwrap();
        assert_eq!(path, dir.join(file_name_v2(&root())));
        match load(&dir, &root(), &lua) {
            Loaded::Restored(back) => assert_eq!(back.current.root(), layout.root()),
            other => panic!("{other:?}"),
        }
        assert_eq!(names_in(&dir), [file_name_v2(&root())]);
    }

    /// Every writer of Eitri's own state in this crate -- the layout, the prompt history, the
    /// permission rules, the empty tab's mode -- creates its directory 0700 and its file 0600, and
    /// tightens the `eitri` root an older build left at `0755`. Under the usual umask 022 each of
    /// these was `0755`/`0644` before, readable by every local user wherever the path down to the
    /// state home is traversable.
    #[test]
    fn every_state_writer_here_keeps_its_directory_and_file_private() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
        let state = a_dir("state-modes").join("eitri");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (layout, _) = left_as();

        let layout_file = save(&state.join("layout"), &root(), &Workspaces::single(), &layout).unwrap();
        let partial_file = copy_aside_v2(&state.join("layout"), &root(), "partly").unwrap();
        crate::prompt_history::append(&state.join("history"), &root(), &["a prompt".to_string()]).unwrap();
        let history_file = crate::prompt_history::path(&state.join("history"), &root());
        crate::permission_store::add(
            &state.join("permissions"),
            &root(),
            &agent::PrefixRule::parse("Bash(git push *)").unwrap(),
        )
        .unwrap();
        let rules_file = crate::permission_store::path(&state.join("permissions"), &root());
        let mode_file = crate::agent_prefs::save_mode(
            &state.join("agent"),
            &root(),
            crate::agent_bridge::SessionModeChoice::Auto,
        )
        .unwrap();

        assert_eq!(mode(&state), 0o700, "the eitri root an older build left open");
        for sub in ["layout", "history", "permissions", "agent"] {
            assert_eq!(mode(&state.join(sub)), 0o700, "{sub}/");
        }
        for file in [&layout_file, &partial_file, &history_file, &rules_file, &mode_file] {
            assert_eq!(mode(file), 0o600, "{}", file.display());
        }
    }

    /// The `.tmp` is this process's own: another window's half-written file beside it is neither
    /// renamed into place nor overwritten.
    #[test]
    fn the_temporary_file_is_this_processs_own() {
        let dir = a_dir("layout-persist-tmp");
        let path = dir.join(file_name_v2(&root()));
        assert_eq!(
            temporary(&path),
            dir.join(format!("{}.{}.tmp", file_name_v2(&root()), std::process::id()))
        );
        // A shared name, and another process's own (pid 1 is never a window).
        let others = [
            dir.join(format!("{}.tmp", file_name_v2(&root()))),
            dir.join(format!("{}.1.tmp", file_name_v2(&root()))),
        ];
        for other in &others {
            std::fs::write(other, "{ half of another window's").unwrap();
        }
        let (layout, lua) = left_as();
        save(&dir, &root(), &Workspaces::single(), &layout).unwrap();
        for other in &others {
            assert_eq!(std::fs::read_to_string(other).unwrap(), "{ half of another window's");
        }
        assert!(matches!(load(&dir, &root(), &lua), Loaded::Restored(_)));
        assert!(!temporary(&path).exists());
    }

    /// A rename that fails -- here because a directory sits where the file goes -- removes its
    /// `.tmp` rather than leaving one per failed save.
    #[test]
    fn a_failed_save_leaves_no_temporary_file() {
        let dir = a_dir("layout-persist-failed");
        let path = dir.join(file_name_v2(&root()));
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("in the way"), "").unwrap();
        let (layout, _) = left_as();
        assert!(save(&dir, &root(), &Workspaces::single(), &layout).is_err());
        assert_eq!(names_in(&dir), [file_name_v2(&root())], "no .tmp is left behind");
    }

    #[test]
    fn a_missing_file_is_missing_and_a_corrupt_one_is_unusable() {
        let dir = a_dir("layout-persist-missing");
        assert!(matches!(load(&dir, &root(), &[]), Loaded::Missing));
        std::fs::write(dir.join(file_name_v2(&root())), "\u{0}garbage").unwrap();
        match load(&dir, &root(), &[]) {
            Loaded::Unusable(why) => assert!(why.contains("not a layout file"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    /// A file that could not be used is moved aside whole, never overwritten: afterwards the project
    /// has no file, and the `.unusable` beside it holds every byte.
    #[test]
    fn an_unusable_file_is_set_aside_whole() {
        let dir = a_dir("layout-persist-aside");
        let text = "{ \"version\": 2, \"a typo\" ";
        std::fs::write(dir.join(file_name_v2(&root())), text).unwrap();
        assert!(matches!(load(&dir, &root(), &[]), Loaded::Unusable(_)));
        let aside = set_aside_v2(&dir, &root()).unwrap();
        assert_eq!(aside, dir.join(format!("{}.unusable", file_name_v2(&root()))));
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), text);
        assert!(matches!(load(&dir, &root(), &[]), Loaded::Missing));
    }

    #[test]
    fn the_file_is_named_by_the_roots_hash_and_lives_under_the_state_home() {
        let name = file_name(&root());
        assert_eq!(name.len(), 16 + ".json".len());
        assert!(name[..16].chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(&name[..16], &agent::conversation_id_for_cwd(&root())[..16]);
        assert_ne!(name, file_name(Path::new("/home/user/other")));
        assert_eq!(file_name_v2(&root()), format!("{}.v2.json", &name[..16]));
        assert_eq!(
            state_dir(Some(OsStr::new("/s")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/s/eitri/layout"))
        );
        assert_eq!(
            state_dir(None, Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.local/state/eitri/layout"))
        );
        assert_eq!(
            state_dir(Some(OsStr::new("")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.local/state/eitri/layout")),
            "an empty XDG_STATE_HOME is unset"
        );
        assert_eq!(
            state_dir(Some(OsStr::new("state")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.local/state/eitri/layout")),
            "a relative XDG_STATE_HOME is ignored, as the XDG spec says"
        );
        assert_eq!(state_dir(None, None), None);
        assert_eq!(state_dir(None, Some(OsStr::new(""))), None, "an empty HOME");
        assert_eq!(
            state_dir(Some(OsStr::new("state")), Some(OsStr::new("h"))),
            None,
            "a relative HOME would put the file under the cwd -- the project, launched with no argument"
        );
    }

    #[test]
    fn a_file_names_its_instances_and_every_workspace_gets_them() {
        let id = ModuleId::parse("terminal#2").unwrap();
        let mut main = plain();
        main.replace_root(super::super::tree::place_new(main.root_node(), &id, Placement::BelowEditor).0);
        let ws = with(&["main", "study"], "main", &[main.clone(), plain()]);
        let restored = decode_v2(&encode_v2(&ws, &main, &root()), &root(), &[]).unwrap();
        assert!(restored.current.contains(&id));
        assert!(restored.workspaces.stored("study").unwrap().hidden().contains(&id));
        assert!(restored
            .notes
            .iter()
            .any(|n| n.contains("placed 'terminal#2' below the editor, hidden: another workspace has it")));
        let agent = ModuleId::parse("agent#2").unwrap();
        main.replace_root(super::super::tree::place_new(main.root_node(), &agent, Placement::BelowEditor).0);
        let restored = decode_v2(&encode_v2(&Workspaces::single(), &main, &root()), &root(), &[]).unwrap();
        assert!(!restored.current.contains(&agent));
        assert!(restored
            .notes
            .iter()
            .any(|n| n.contains("left out 'agent#2': this window has no such module")));
        assert!(restored.keep_copy.is_some());
    }
}
