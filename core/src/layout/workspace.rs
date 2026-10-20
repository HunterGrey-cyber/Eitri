//! A window's workspaces: named layouts, one of them current. The current one's `Layout` lives
//! outside this type (the shell keeps it in the `Rc<RefCell<Layout>>` every reader of the live layout
//! already holds), so a switch is a swap of two values and nothing that reads the live layout changes.
//! This type holds the order of the names, every other workspace's layout, and what a later build
//! wrote into the state file that this one cannot read but must write back unchanged.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::{Map, Value};

use super::module::{ModuleDecl, ModuleId};
use super::tree::Layout;

/// The workspace every window has from its first launch, and the one a file from before workspaces
/// is read into.
pub const MAIN: &str = "main";

/// The longest workspace name, in characters: a name is shown in the top bar and typed in a chooser.
pub const MAX_NAME_CHARS: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub struct Workspaces {
    order: Vec<String>,
    stored: BTreeMap<String, Layout>,
    current: String,
    /// What a later build wrote that this one does not read: top-level keys, and each workspace's own
    /// keys.
    kept: Kept,
}

/// Keys of the state file this build does not know, carried verbatim so a save by this build does
/// not drop what a later build keeps there (its floating modules, say). In [`Workspaces`] a workspace
/// with no kept keys has no entry; read from a file (`persist::V2State`), every workspace the file
/// holds has one, empty or not, which tells "the file has this workspace with no such keys" apart
/// from "the file has no such workspace".
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Kept {
    pub top: Map<String, Value>,
    pub per_workspace: BTreeMap<String, Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceError {
    /// A name that is empty, too long, or holds a control character.
    Name(String),
    /// A name given twice.
    Duplicate(String),
    /// The current workspace is not one of the names.
    NoCurrent(String),
    /// The stored layouts are not exactly the workspaces other than the current one.
    Stored(String),
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Escaped: a name refused for its control characters would otherwise carry them into
            // whatever line shows the error.
            WorkspaceError::Name(name) => write!(
                f,
                "'{}' is not a workspace name (1 to {MAX_NAME_CHARS} characters, no control characters)",
                printable(name)
            ),
            WorkspaceError::Duplicate(name) => write!(f, "workspace '{name}' is named twice"),
            WorkspaceError::NoCurrent(name) => write!(f, "the current workspace '{name}' is not one of the workspaces"),
            WorkspaceError::Stored(name) => write!(
                f,
                "workspace '{name}': the stored layouts must be every workspace but the current one"
            ),
        }
    }
}

impl std::error::Error for WorkspaceError {}

/// `text` with each control character -- a newline, an escape -- written as its Rust escape, the rest
/// as it is: for a name or key read from a file, shown on one line.
pub(crate) fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

impl Workspaces {
    /// One workspace, `main`, current: a first launch, and any window whose file could not be used.
    pub fn single() -> Workspaces {
        Workspaces {
            order: vec![MAIN.to_string()],
            stored: BTreeMap::new(),
            current: MAIN.to_string(),
            kept: Kept::default(),
        }
    }

    /// Workspaces from their parts, checked: every name valid and given once, `current` among them,
    /// and `stored` holding a layout for exactly the names other than `current`. Kept keys of a
    /// workspace not in `order` have nowhere to be written and are dropped, as is an empty entry.
    pub fn from_parts(
        order: Vec<String>,
        stored: BTreeMap<String, Layout>,
        current: String,
        mut kept: Kept,
    ) -> Result<Workspaces, WorkspaceError> {
        let mut seen = BTreeSet::new();
        for name in &order {
            Workspaces::valid_name(name)?;
            if !seen.insert(name.as_str()) {
                return Err(WorkspaceError::Duplicate(name.clone()));
            }
        }
        if !seen.contains(current.as_str()) {
            return Err(WorkspaceError::NoCurrent(current));
        }
        if let Some(name) = stored
            .keys()
            .find(|name| !seen.contains(name.as_str()) || **name == current)
        {
            return Err(WorkspaceError::Stored(name.clone()));
        }
        if let Some(name) = order
            .iter()
            .find(|name| **name != current && !stored.contains_key(*name))
        {
            return Err(WorkspaceError::Stored(name.clone()));
        }
        kept.per_workspace
            .retain(|name, map| seen.contains(name.as_str()) && !map.is_empty());
        Ok(Workspaces {
            order,
            stored,
            current,
            kept,
        })
    }

    /// 1 to [`MAX_NAME_CHARS`] characters, none of them a control character (a newline or an escape
    /// would break the one line the name is shown on).
    pub fn valid_name(name: &str) -> Result<(), WorkspaceError> {
        let chars = name.chars().count();
        if chars == 0 || chars > MAX_NAME_CHARS || name.chars().any(char::is_control) {
            return Err(WorkspaceError::Name(name.to_string()));
        }
        Ok(())
    }

    pub fn current(&self) -> &str {
        &self.current
    }

    pub fn order(&self) -> &[String] {
        &self.order
    }

    /// A workspace other than the current one; `None` for the current one, whose layout is the live
    /// one, and for a name that is not a workspace.
    pub fn stored(&self, name: &str) -> Option<&Layout> {
        self.stored.get(name)
    }

    pub fn kept(&self) -> &Kept {
        &self.kept
    }

    /// The kept keys of one workspace; `None` when it has none.
    pub fn kept_for(&self, name: &str) -> Option<&Map<String, Value>> {
        self.kept.per_workspace.get(name)
    }

    /// Every module id a string anywhere in the kept keys names, by `ModuleId::parse`, each once: what
    /// a later build placed where this one cannot see it, so this build must never give the same name
    /// to a module of its own.
    pub fn kept_modules(&self) -> Vec<ModuleId> {
        let mut found = BTreeSet::new();
        for value in self.kept.top.values() {
            modules_named(value, &mut found);
        }
        for map in self.kept.per_workspace.values() {
            for value in map.values() {
                modules_named(value, &mut found);
            }
        }
        found.into_iter().collect()
    }

    /// Numbered modules named by preserved data cannot be allocated again.
    pub fn reserved(&self) -> Vec<ModuleId> {
        self.kept_modules().into_iter().filter(|id| id.number() >= 2).collect()
    }

    /// Adds a hidden instance below the editor in every stored workspace.
    pub fn add_everywhere(&mut self, id: &ModuleId) {
        for layout in self.stored_mut() {
            if !layout.contains(id) {
                let root = super::tree::place_new(layout.root_node(), id, super::module::Placement::BelowEditor).0;
                layout.replace_root(root);
                layout.hide_unfocused(id).expect("another module remains shown");
            }
        }
    }

    pub fn remove_everywhere(&mut self, id: &ModuleId) {
        for layout in self.stored_mut() {
            if layout.contains(id) {
                let _ = layout.remove(id);
            }
        }
    }

    /// Every module this window places itself: the built-ins, each registered Lua panel, and every
    /// module any of its workspaces holds, `current` being the current one's layout. A kept key naming
    /// one of these would, once written back, put that module in two places.
    pub fn placed(&self, current: &Layout, lua: &[ModuleDecl]) -> BTreeSet<ModuleId> {
        let mut placed = built_in_and_registered(lua);
        for (_, layout) in self.each(current) {
            placed.extend(layout.leaves());
        }
        placed
    }

    /// Replaces the kept keys with `now`'s, the file's as it is when this window writes: the kept keys
    /// are not this window's arrangement (it can neither read nor change them), so the file's newest
    /// copy of them wins. The top-level keys are taken whole. A workspace of this window takes the
    /// file's keys for it when the file has that workspace, and keeps its own when the file does not
    /// (another window removed it, or the file's copy of it could not be read): this window writes the
    /// workspace back, and its keys with it.
    pub fn retake_kept(&mut self, now: Kept) {
        self.kept.top = now.top;
        for (name, map) in now.per_workspace {
            if !self.order.contains(&name) {
                continue;
            }
            if map.is_empty() {
                self.kept.per_workspace.remove(&name);
            } else {
                self.kept.per_workspace.insert(name, map);
            }
        }
    }

    /// Every stored layout, for a change that must reach every workspace (a module that ends, or a new
    /// instance, changes the set of modules each one holds).
    pub fn stored_mut(&mut self) -> impl Iterator<Item = &mut Layout> {
        self.stored.values_mut()
    }

    /// Every workspace in order, as `(name, layout)`, the current one's layout being `current`.
    pub fn each<'a>(&'a self, current: &'a Layout) -> Vec<(&'a str, &'a Layout)> {
        self.order
            .iter()
            .map(|name| {
                let layout = if *name == self.current {
                    current
                } else {
                    self.stored
                        .get(name)
                        .expect("every workspace but the current one is stored")
                };
                (name.as_str(), layout)
            })
            .collect()
    }
}

impl Kept {
    /// Drops every workspace-level key that names a module in `placed` ([`Workspaces::placed`]),
    /// with one note per key dropped. Each note says a copy is kept: a caller that gets any back keeps
    /// a copy of the text these keys came from before it writes without them.
    pub fn drop_placed(&mut self, placed: &BTreeSet<ModuleId>) -> Vec<String> {
        let mut notes = Vec::new();
        for (name, map) in &mut self.per_workspace {
            drop_keys_naming(name, map, placed, &mut notes);
        }
        notes
    }
}

/// The modules every window places whatever its file says: the built-ins and each registered Lua
/// panel.
pub(crate) fn built_in_and_registered(lua: &[ModuleDecl]) -> BTreeSet<ModuleId> {
    let mut placed: BTreeSet<ModuleId> = [ModuleId::editor(), ModuleId::agent(), ModuleId::terminal()].into();
    placed.extend(lua.iter().map(|decl| decl.id.clone()));
    placed
}

/// Drops each of workspace `name`'s kept keys that names a module in `placed`, anywhere in its value:
/// written back, it would put that module in two places, in this window's tree and wherever the
/// later build keeps it. One note per key, its text escaped for one log line.
pub(crate) fn drop_keys_naming(
    name: &str,
    map: &mut Map<String, Value>,
    placed: &BTreeSet<ModuleId>,
    notes: &mut Vec<String>,
) {
    map.retain(|key, value| {
        let mut named = BTreeSet::new();
        modules_named(value, &mut named);
        match named.iter().find(|id| placed.contains(id)) {
            Some(id) => {
                notes.push(printable(&format!(
                    "{key} of workspace '{name}' names '{id}', which this version places itself; a copy is kept"
                )));
                false
            }
            None => true,
        }
    });
}

/// Every module id a string in `value` (at any depth) names.
pub(crate) fn modules_named(value: &Value, out: &mut BTreeSet<ModuleId>) {
    match value {
        Value::String(text) => {
            if let Ok(id) = ModuleId::parse(text) {
                out.insert(id);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| modules_named(item, out)),
        Value::Object(map) => map.values().for_each(|item| modules_named(item, out)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn layout() -> Layout {
        Layout::initial(&[]).unwrap()
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn workspace_names_are_1_to_32_characters_without_control_characters() {
        for good in [
            "main",
            "a",
            "study",
            "ворк",
            "x y",
            &"é".repeat(32),
            "日本語のワークスペース",
        ] {
            assert_eq!(Workspaces::valid_name(good), Ok(()), "{good:?}");
        }
        for bad in [
            "",
            &"a".repeat(33),
            &"é".repeat(33),
            "a\nb",
            "tab\there",
            "\u{1b}[31m",
            "nul\0",
        ] {
            assert_eq!(
                Workspaces::valid_name(bad),
                Err(WorkspaceError::Name(bad.to_string())),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn single_is_main_alone_and_current() {
        let one = Workspaces::single();
        assert_eq!(one.order(), ["main"]);
        assert_eq!(one.current(), MAIN);
        assert_eq!(one.stored(MAIN), None);
        assert_eq!(one.kept(), &Kept::default());
        let live = layout();
        assert_eq!(one.each(&live), vec![("main", &live)]);
    }

    #[test]
    fn from_parts_refuses_what_does_not_fit() {
        let stored = |list: &[&str]| -> BTreeMap<String, Layout> {
            list.iter().map(|name| (name.to_string(), layout())).collect()
        };
        let ok = Workspaces::from_parts(
            names(&["main", "study"]),
            stored(&["study"]),
            "main".into(),
            Kept::default(),
        )
        .unwrap();
        assert_eq!(ok.order(), ["main", "study"]);
        assert!(ok.stored("study").is_some());

        let cases: Vec<(Vec<String>, BTreeMap<String, Layout>, &str, WorkspaceError)> = vec![
            (
                names(&["main", ""]),
                stored(&[""]),
                "main",
                WorkspaceError::Name("".into()),
            ),
            (
                names(&["main", "a\u{7}"]),
                stored(&["a\u{7}"]),
                "main",
                WorkspaceError::Name("a\u{7}".into()),
            ),
            (
                names(&["main", "main"]),
                stored(&[]),
                "main",
                WorkspaceError::Duplicate("main".into()),
            ),
            (
                names(&["main"]),
                stored(&[]),
                "study",
                WorkspaceError::NoCurrent("study".into()),
            ),
            (
                names(&["main", "study"]),
                stored(&[]),
                "main",
                WorkspaceError::Stored("study".into()),
            ),
            (
                names(&["main"]),
                stored(&["main"]),
                "main",
                WorkspaceError::Stored("main".into()),
            ),
            (
                names(&["main"]),
                stored(&["other"]),
                "main",
                WorkspaceError::Stored("other".into()),
            ),
        ];
        for (order, stored, current, want) in cases {
            assert_eq!(
                Workspaces::from_parts(order.clone(), stored, current.into(), Kept::default()),
                Err(want.clone()),
                "{order:?} current {current}"
            );
        }
    }

    #[test]
    fn kept_keys_of_a_workspace_that_is_not_there_are_dropped() {
        let mut some = Map::new();
        some.insert("floating".into(), json!([]));
        let mut kept = Kept::default();
        kept.per_workspace.insert("main".into(), some.clone());
        kept.per_workspace.insert("gone".into(), some);
        kept.per_workspace.insert("study".into(), Map::new());
        let mut stored = BTreeMap::new();
        stored.insert("study".to_string(), layout());
        let ws = Workspaces::from_parts(names(&["main", "study"]), stored, "main".into(), kept).unwrap();
        assert_eq!(
            ws.kept().per_workspace.keys().collect::<Vec<_>>(),
            ["main"],
            "'gone' is not a workspace, and 'study' keeps nothing"
        );
    }

    #[test]
    fn each_lists_every_workspace_in_order_with_the_live_one_current() {
        let mut stored = BTreeMap::new();
        let mut study = layout();
        study.set_focus(&ModuleId::agent()).unwrap();
        stored.insert("a-first".to_string(), layout());
        stored.insert("study".to_string(), study.clone());
        let ws = Workspaces::from_parts(
            names(&["study", "main", "a-first"]),
            stored,
            "main".into(),
            Kept::default(),
        )
        .unwrap();
        let live = Layout::companion();
        let each = ws.each(&live);
        assert_eq!(
            each.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            ["study", "main", "a-first"]
        );
        assert_eq!(each[1].1, &live);
        assert_eq!(each[0].1, &study);
    }

    #[test]
    fn kept_modules_lists_every_string_that_parses_once() {
        let mut kept = Kept::default();
        kept.top
            .insert("future".into(), json!({"deep": [["lua:x", "not a module"]]}));
        let mut main = Map::new();
        main.insert(
            "floating".into(),
            json!([{"module": "lua:x", "w": 900}, {"module": "terminal"}]),
        );
        kept.per_workspace.insert("main".into(), main);
        let ws = Workspaces::from_parts(names(&["main"]), BTreeMap::new(), "main".into(), kept).unwrap();
        assert_eq!(ws.kept_modules(), vec![ModuleId::lua("x"), ModuleId::terminal()]);
    }

    #[test]
    fn retake_kept_takes_the_files_keys_and_keeps_a_workspace_the_file_lacks() {
        let mut stored = BTreeMap::new();
        stored.insert("study".to_string(), layout());
        stored.insert("review".to_string(), layout());
        let mut kept = Kept::default();
        kept.top.insert("old".into(), json!(1));
        let mut floating = Map::new();
        floating.insert("floating".into(), json!([]));
        kept.per_workspace.insert("study".into(), floating.clone());
        kept.per_workspace.insert("review".into(), floating.clone());
        let mut ws = Workspaces::from_parts(names(&["main", "study", "review"]), stored, "main".into(), kept).unwrap();

        let mut now = Kept::default();
        now.top.insert("future".into(), json!(2));
        let mut memory = Map::new();
        memory.insert("float_memory".into(), json!([]));
        now.per_workspace.insert("main".into(), memory.clone());
        now.per_workspace.insert("review".into(), Map::new());
        now.per_workspace.insert("elsewhere".into(), memory.clone());
        ws.retake_kept(now);
        assert_eq!(ws.kept().top.keys().collect::<Vec<_>>(), ["future"]);
        assert_eq!(ws.kept_for("main"), Some(&memory), "the file's keys for 'main'");
        assert_eq!(ws.kept_for("review"), None, "the file's 'review' keeps nothing now");
        assert_eq!(
            ws.kept_for("study"),
            Some(&floating),
            "the file has no 'study': this window's copy stays"
        );
        assert_eq!(ws.kept_for("elsewhere"), None, "not a workspace of this window");
    }

    #[test]
    fn w1_every_workspace_gains_and_loses_an_instance() {
        let id = ModuleId::parse("terminal#2").unwrap();
        let mut ws = Workspaces::from_parts(
            names(&["main", "study"]),
            [("study".into(), layout())].into(),
            "main".into(),
            Kept::default(),
        )
        .unwrap();
        ws.add_everywhere(&id);
        let study = ws.stored("study").unwrap();
        assert!(study.contains(&id));
        assert!(study.hidden().contains(&id));
        let mut expected = layout();
        expected.replace_root(
            super::super::tree::place_new(expected.root_node(), &id, super::super::Placement::BelowEditor).0,
        );
        assert_eq!(study.root(), expected.root());
        ws.add_everywhere(&id);
        let study = ws.stored_mut().next().unwrap();
        study.show(&id).unwrap();
        study.set_focus(&id).unwrap();
        ws.remove_everywhere(&id);
        let study = ws.stored("study").unwrap();
        assert!(!study.contains(&id));
        assert_eq!(study.focus(), &ModuleId::editor());
    }

    #[test]
    fn a_kept_key_naming_a_module_any_workspace_now_holds_is_dropped_with_a_note() {
        use super::super::ModuleDecl;
        let id = ModuleId::parse("terminal#2").unwrap();
        let mut study = layout();
        study.replace_root(
            super::super::tree::place_new(study.root_node(), &id, super::super::Placement::BelowEditor).0,
        );
        let ws = Workspaces::from_parts(
            names(&["main", "study"]),
            [("study".to_string(), study)].into(),
            MAIN.to_string(),
            Kept::default(),
        )
        .unwrap();
        let panel = ModuleDecl {
            id: ModuleId::lua("side"),
            placement: super::super::Placement::RightOfRoot,
        };
        let placed = ws.placed(&layout(), std::slice::from_ref(&panel));
        assert!(placed.contains(&id), "held by a workspace not on screen");
        assert!(placed.contains(&panel.id));
        assert!(placed.contains(&ModuleId::terminal()));
        assert!(!placed.contains(&ModuleId::parse("terminal#3").unwrap()));

        let mut kept = Kept::default();
        let mut main = Map::new();
        main.insert("floating".to_string(), json!([{"module": "terminal#2"}]));
        main.insert("float_memory".to_string(), json!([{"module": "terminal#3"}]));
        kept.per_workspace.insert("main".to_string(), main);
        kept.top.insert("floating".to_string(), json!(["terminal#2"]));
        let notes = kept.drop_placed(&placed);
        assert_eq!(
            notes,
            ["floating of workspace 'main' names 'terminal#2', which this version places itself; a copy is kept"]
        );
        assert_eq!(
            kept.per_workspace["main"].keys().collect::<Vec<_>>(),
            ["float_memory"],
            "a key naming only modules this window never places stays"
        );
        assert!(
            kept.top.contains_key("floating"),
            "only workspace-level keys are checked"
        );
    }

    #[test]
    fn an_instance_named_only_in_kept_data_keeps_its_number() {
        use super::super::persist::{decode_v2, encode_v2};
        let root = std::path::Path::new("/project");
        let mut file: Value = serde_json::from_str(&encode_v2(&Workspaces::single(), &layout(), root)).unwrap();
        file["workspaces"][0]["floating"] = json!([{"module": "terminal#2"}]);
        let restored = decode_v2(&file.to_string(), root, &[]).unwrap();
        let id = ModuleId::parse("terminal#2").unwrap();
        assert_eq!(restored.workspaces.reserved(), [id.clone()]);
        assert!(restored.workspaces.kept_for("main").unwrap().contains_key("floating"));
        let mut live = layout();
        live.replace_root(super::super::tree::place_new(live.root_node(), &id, super::super::Placement::BelowEditor).0);
        file["workspaces"][0]["tiled"] = serde_json::from_str::<Value>(&encode_v2(&Workspaces::single(), &live, root))
            .unwrap()["workspaces"][0]["tiled"]
            .clone();
        let restored = decode_v2(&file.to_string(), root, &[]).unwrap();
        assert!(restored.workspaces.reserved().is_empty());
        assert!(restored.workspaces.kept_for("main").is_none());
        assert!(restored.keep_copy.is_some());
    }
}
