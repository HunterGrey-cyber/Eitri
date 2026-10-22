//! Open rules: where the window puts a module it opens by itself -- at a first launch, a new instance,
//! a tab broken out, a module placed into a workspace that lacks it, a module back after a kill.
//! A gesture that says where (a split key, a move, a drag) never reads them. Precedence: a rule for
//! the module's id, then its kind's (`terminal`, `agent`), then the module's own rule (a Lua panel's
//! `position`, or a plugin's declared rule), then the built-in default of the point where it opens.

use super::con::Node;
use super::float::{Float, FloatRect, DEFAULT_FLOAT_SHARE};
use super::geometry::{Direction, Size};
use super::module::{ModuleDecl, ModuleId, ModuleKind, Placement};
use super::tree::{Layout, LayoutError, MIN_RATIO};

/// What a floating rule is sized against before the window has a size: the window's default 1280x760 less its
/// 40 px top bar.
pub const NOMINAL_GRID: Size = Size { w: 1280, h: 720 };

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum At {
    /// `side`, `bottom`, `main`, `below-editor`: a first launch's own places.
    Placement(Placement),
    /// `right`, `below`, `left`, `above`: a split on that side of the module with the keys.
    Beside(Direction),
    /// `tab`: into the keys' module's tabbed or stacked container, else both into a new tabbed one.
    Tab,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RuleSize {
    /// A tiled module's share of its container, above 0 and below 1.
    Share(f32),
    /// A float's width and height as fractions of the grid, each above 0 and at most 1.
    Float { w: f32, h: f32 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpenRule {
    pub at: Option<At>,
    pub float: bool,
    pub size: Option<RuleSize>,
    /// Opens stowed in the scratchpad; implies `float`.
    pub scratchpad: bool,
    /// Takes the keys when it opens, as tmux's new pane does.
    pub focus: bool,
}

impl Default for OpenRule {
    fn default() -> Self {
        OpenRule {
            at: None,
            float: false,
            size: None,
            scratchpad: false,
            focus: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleTarget {
    Kind(ModuleKind),
    Module(ModuleId),
}

impl RuleTarget {
    /// `terminal` and `agent` are kinds: their rule is every instance's that has none of its own, the base one's
    /// included. Anything else is a module id.
    pub fn parse(text: &str) -> Result<RuleTarget, String> {
        match text {
            "terminal" => Ok(RuleTarget::Kind(ModuleKind::Terminal)),
            "agent" => Ok(RuleTarget::Kind(ModuleKind::Agent)),
            _ => ModuleId::parse(text).map(RuleTarget::Module).map_err(|e| e.to_string()),
        }
    }
}

/// Where the window is opening a module, which decides the built-in default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenPoint {
    /// A first launch, or reconcile placing a module a workspace lacks: no module holds the keys.
    Launch,
    /// `instance.new`, `eitri.layout.open`, a module the scratchpad makes.
    NewInstance,
    /// `tab.break`: a chat view for the tab broken out.
    Break,
    /// Back after a kill (`Reopen::At`).
    Reopen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleSource {
    Module,
    Kind,
    Position,
    BuiltIn,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OpenRules {
    rules: Vec<(RuleTarget, OpenRule)>,
}

impl OpenRules {
    pub fn add(&mut self, target: RuleTarget, rule: OpenRule) -> Result<(), String> {
        if self.rules.iter().any(|(t, _)| *t == target) {
            return Err("a rule for it is already set".to_string());
        }
        self.rules.push((target, rule));
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// `position` is a Lua panel's own, `None` for every other module.
    pub fn resolve(&self, id: &ModuleId, point: OpenPoint, position: Option<Placement>) -> (OpenRule, RuleSource) {
        let own = position.map(|placement| OpenRule {
            at: Some(At::Placement(placement)),
            ..OpenRule::default()
        });
        self.resolve_own(id, point, own.as_ref())
    }

    /// `own` is the module's own rule (a Lua panel's `position`, or a plugin's declared rule), counted where a Lua
    /// panel's `position` counts: after a module-id or kind rule, before the built-in default. It is returned
    /// whole, so a declared `focus` or `size` survives.
    pub fn resolve_own(&self, id: &ModuleId, point: OpenPoint, own: Option<&OpenRule>) -> (OpenRule, RuleSource) {
        if let Some((_, rule)) = self.rules.iter().find(|(t, _)| *t == RuleTarget::Module(id.clone())) {
            return (rule.clone(), RuleSource::Module);
        }
        if id.kind().has_instances() {
            if let Some((_, rule)) = self.rules.iter().find(|(t, _)| *t == RuleTarget::Kind(id.kind())) {
                return (rule.clone(), RuleSource::Kind);
            }
        }
        if let Some(rule) = own {
            return (rule.clone(), RuleSource::Position);
        }
        (builtin(id, point), RuleSource::BuiltIn)
    }

    /// The declarations a first launch, reconcile and a kill's reopen place modules by when no module holds the
    /// keys: the base terminal's when a rule names its kind, each Lua panel's (its rule's place, else its
    /// position), each instance's that has a rule of its own, and the numbered chat views' (their kind's rule,
    /// else the side). The base chat view is never placed by its declaration: a first launch puts it at the
    /// side, and a layout file where the file says. A rule that needs the keys falls back as `without_keys`
    /// says, with a note; a floating rule's declaration is its module's tiled place (a numbered instance's
    /// rule that names no place keeps its kind's), since floating needs a grid size.
    pub fn launch_decls(&self, panels: &[ModuleDecl]) -> (Vec<ModuleDecl>, Vec<String>) {
        let mut decls = Vec::new();
        let mut notes = Vec::new();
        let mut push = |id: ModuleId, rule: &OpenRule, source: RuleSource, fallback: Placement| {
            let placement = match rule.at {
                Some(at) => {
                    let (placement, fell) = without_keys(at);
                    if fell && source != RuleSource::BuiltIn {
                        // The `agent` declaration only places the numbered views (a first launch puts the base
                        // one at the side), so the note says so rather than promise a move of the base one.
                        let whom = if id == ModuleId::agent() {
                            "a numbered chat view"
                        } else {
                            "it"
                        };
                        notes.push(format!(
                            "open rule for '{id}': at '{}' needs a module with the keys, and there is none when \
                             the window places {whom} itself; it goes to the {}",
                            at_name(at),
                            at_name(At::Placement(placement))
                        ));
                    }
                    placement
                }
                None => fallback,
            };
            decls.push(ModuleDecl { id, placement });
        };
        let terminal = ModuleId::terminal();
        let (rule, source) = self.resolve(&terminal, OpenPoint::Launch, None);
        if source != RuleSource::BuiltIn {
            push(terminal, &rule, source, Placement::BelowEditor);
        }
        for panel in panels {
            let (rule, source) = self.resolve(&panel.id, OpenPoint::Launch, Some(panel.placement));
            push(panel.id.clone(), &rule, source, panel.placement);
        }
        for (target, rule) in &self.rules {
            if let RuleTarget::Module(id) = target {
                if id.kind().has_instances() && id.number() >= 2 {
                    // A rule that says nothing of where (only `float`, `size` or `focus`) keeps its kind's
                    // place: a chat view at the side, a terminal below the editor.
                    let fallback = match id.kind() {
                        ModuleKind::Agent => Placement::RightOfRoot,
                        _ => Placement::BelowEditor,
                    };
                    push(id.clone(), rule, RuleSource::Module, fallback);
                }
            }
        }
        let agent = ModuleId::agent();
        let (rule, source) = self.resolve(&agent, OpenPoint::Launch, None);
        push(agent, &rule, source, Placement::RightOfRoot);
        (decls, notes)
    }
}

/// The built-in default where nothing else decides: a first launch's or a reopen's terminal below the editor; a new
/// terminal, a new chat view and a broken-out tab right of the module with the keys.
pub fn builtin(id: &ModuleId, point: OpenPoint) -> OpenRule {
    let at = match (id.kind(), point) {
        (ModuleKind::Terminal, OpenPoint::Launch | OpenPoint::Reopen) => At::Placement(Placement::BelowEditor),
        (ModuleKind::Terminal | ModuleKind::Agent, _) => At::Beside(Direction::Right),
        _ => At::Placement(Placement::RightOfRoot),
    };
    OpenRule {
        at: Some(at),
        ..OpenRule::default()
    }
}

/// Where `at` goes when no module holds the keys, and whether that is a fallback.
pub fn without_keys(at: At) -> (Placement, bool) {
    match at {
        At::Placement(placement) => (placement, false),
        At::Beside(Direction::Right | Direction::Left) | At::Tab => (Placement::RightOfRoot, true),
        At::Beside(Direction::Down | Direction::Up) => (Placement::BelowRoot, true),
    }
}

pub fn parse_at(name: &str) -> Option<At> {
    Some(match name {
        "side" => At::Placement(Placement::RightOfRoot),
        "bottom" => At::Placement(Placement::BelowRoot),
        "main" => At::Placement(Placement::InPlaceOfEditor),
        "below-editor" => At::Placement(Placement::BelowEditor),
        "right" => At::Beside(Direction::Right),
        "below" => At::Beside(Direction::Down),
        "left" => At::Beside(Direction::Left),
        "above" => At::Beside(Direction::Up),
        "tab" => At::Tab,
        _ => return None,
    })
}

pub fn at_name(at: At) -> &'static str {
    match at {
        At::Placement(Placement::RightOfRoot) => "side",
        At::Placement(Placement::BelowRoot) => "bottom",
        At::Placement(Placement::InPlaceOfEditor) => "main",
        At::Placement(Placement::BelowEditor) => "below-editor",
        At::Beside(Direction::Right) => "right",
        At::Beside(Direction::Down) => "below",
        At::Beside(Direction::Left) => "left",
        At::Beside(Direction::Up) => "above",
        At::Tab => "tab",
    }
}

/// The declarations a first launch places itself: the base terminal's and the Lua panels', in order. The others
/// (a chat view's, a numbered instance's) only say where the module goes when something else places it.
pub fn first_launch_decls(decls: &[ModuleDecl]) -> Vec<ModuleDecl> {
    decls
        .iter()
        .filter(|d| d.id == ModuleId::terminal() || d.id.kind() == ModuleKind::LuaWebview)
        .cloned()
        .collect()
}

/// What [`open_by_rule`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct Opened {
    pub takes_keys: bool,
    /// Opened in the scratchpad, stowed.
    pub stowed: bool,
    pub notes: Vec<String>,
}

/// A rule's float, centred: its `size` as fractions of `grid`, else the default share of it.
pub fn float_rect(size: Option<RuleSize>, grid: Size) -> FloatRect {
    match size {
        Some(RuleSize::Float { w, h }) => FloatRect {
            cx: 0.5,
            cy: 0.5,
            w: ((grid.w as f32 * w).round() as i32).max(1),
            h: ((grid.h as f32 * h).round() as i32).max(1),
        },
        _ => FloatRect::centred(grid, DEFAULT_FLOAT_SHARE),
    }
}

/// Puts `id`, which `layout` does not hold, where `rule` says: beside or as a tab of `keys` (the module with the
/// keys; when it floats, the most recent tiled module in view; with none, the side or the bottom, noted), at a
/// first launch's place, or as a float sized against `grid`, stowed when `scratchpad`. A tiled `size` share is the
/// new module's share of its container. Anything it puts on screen ends a zoom. It takes the keys unless `focus`
/// is false -- except in the editor's place when the editor held them, since a hidden module cannot keep them.
pub fn open_by_rule(
    layout: &mut Layout,
    id: &ModuleId,
    rule: &OpenRule,
    keys: Option<&ModuleId>,
    grid: Size,
) -> Result<Opened, LayoutError> {
    if layout.contains(id) {
        return Err(LayoutError::Duplicate(id.clone()));
    }
    let mut notes = Vec::new();
    if rule.float || rule.scratchpad {
        layout.add_float(id, float_rect(rule.size, grid), rule.scratchpad)?;
        if rule.scratchpad {
            layout.hide_unfocused(id)?;
            layout.lower(id);
            return Ok(Opened {
                takes_keys: false,
                stowed: true,
                notes,
            });
        }
        // A float that opens on screen ends a zoom, as placing a tile does, so it is never focused while hidden.
        layout.unzoom();
        if rule.focus {
            layout.set_focus(id)?;
        }
        return Ok(Opened {
            takes_keys: rule.focus,
            stowed: false,
            notes,
        });
    }
    let anchor = keys
        .and_then(|k| {
            if layout.is_floating(k) {
                layout.tiled_anchor()
            } else {
                Some(k.clone())
            }
        })
        .filter(|k| layout.leaves().contains(k) && layout.is_shown(k));
    let mut takes_keys = rule.focus;
    let placement = match (rule.at.unwrap_or(At::Beside(Direction::Right)), anchor) {
        (At::Beside(dir), Some(anchor)) => {
            super::ops::place_at(layout, id, &anchor, dir)?;
            None
        }
        (At::Tab, Some(anchor)) => {
            super::ops::place_as_tab(layout, id, &anchor, rule.focus)?;
            None
        }
        (At::Placement(placement), _) => Some(placement),
        (at, None) => {
            let (placement, _) = without_keys(at);
            notes.push(format!(
                "'{id}' opened at the {}: its rule's '{}' needs a module with the keys, and none has them",
                at_name(At::Placement(placement)),
                at_name(at)
            ));
            Some(placement)
        }
    };
    if let Some(placement) = placement {
        let editor_had_keys = *layout.focus() == ModuleId::editor();
        let (root, took_editors_place) = super::tree::place_new(layout.root_node(), id, placement);
        layout.unzoom();
        layout.replace_root(root);
        if took_editors_place {
            // Hiding the editor moves the keys only if it held them; held elsewhere, `focus = false` keeps them.
            if editor_had_keys {
                layout.set_focus(id)?;
                takes_keys = true;
            }
            layout.hide_unfocused(&ModuleId::editor())?;
        }
    }
    if let Some(RuleSize::Share(share)) = rule.size {
        notes.extend(set_share(layout, id, share));
    }
    if takes_keys {
        layout.set_focus(id)?;
    } else if let Some(k) = keys {
        if layout.can_focus(k).is_ok() {
            layout.set_focus(k)?;
        }
    }
    Ok(Opened {
        takes_keys,
        stowed: false,
        notes,
    })
}

/// A rule's float, scratchpad or share for `id`, which a first launch has just placed tiled (sized against `grid`,
/// the window's size not being known yet). A rule with none of them changes nothing.
pub fn apply_launch_rule(
    layout: &mut Layout,
    id: &ModuleId,
    rule: &OpenRule,
    grid: Size,
) -> Result<Vec<String>, LayoutError> {
    let mut notes = Vec::new();
    if !layout.leaves().contains(id) {
        return Ok(notes);
    }
    if rule.float || rule.scratchpad {
        if *id == ModuleId::editor() {
            return Err(LayoutError::EditorNeverFloats);
        }
        // A module in the editor's place holds the keys at a first launch: the editor comes back and takes them
        // before the module floats, so the keys never stay on a module the rule hides.
        if layout.focus() == id && layout.hidden().contains(&ModuleId::editor()) {
            layout.show(&ModuleId::editor())?;
            layout.set_focus(&ModuleId::editor())?;
        }
        let root = layout
            .root_node()
            .without(id)
            .ok_or_else(|| LayoutError::LastTiled(id.clone()))?;
        layout.push_float(Float {
            id: id.clone(),
            rect: float_rect(rule.size, grid),
            scratch: rule.scratchpad,
        });
        layout.replace_root(root);
        if rule.scratchpad {
            if !layout.hidden().contains(id) {
                layout.hide_unfocused(id)?;
            }
            layout.lower(id);
        } else if rule.focus && !layout.hidden().contains(id) {
            layout.set_focus(id)?;
        }
        notes.push(format!(
            "open rule for '{id}': a first launch floats it{}",
            if rule.scratchpad {
                ", stowed in the scratchpad"
            } else {
                ""
            }
        ));
        return Ok(notes);
    }
    if let Some(RuleSize::Share(share)) = rule.size {
        notes.extend(set_share(layout, id, share));
    }
    Ok(notes)
}

/// `id`'s share of its split container set to `share`, the other children scaled to the rest in proportion, its
/// pin cleared; refused with a note when another child would fall below the smallest share. A tabbed or stacked
/// container draws no shares, so nothing changes there.
fn set_share(layout: &mut Layout, id: &ModuleId, share: f32) -> Vec<String> {
    fn apply(node: &mut Node, id: &ModuleId, share: f32) -> Option<Result<(), String>> {
        let Node::Con(c) = node else { return None };
        if let Some(i) = c
            .children
            .iter()
            .position(|ch| matches!(&ch.node, Node::Leaf(m) if m == id))
        {
            if c.layout.axis().is_none() || c.children.len() < 2 {
                return Some(Ok(()));
            }
            let rest: f32 = c
                .children
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, ch)| ch.share)
                .sum();
            let scale = (1.0 - share) / rest;
            let too_small = share < MIN_RATIO
                || c.children
                    .iter()
                    .enumerate()
                    .any(|(j, ch)| j != i && ch.share * scale < MIN_RATIO);
            if too_small {
                return Some(Err(format!(
                    "size {share} for '{id}' left as placed: a module beside it would fall below the smallest share"
                )));
            }
            for (j, child) in c.children.iter_mut().enumerate() {
                if j == i {
                    child.share = share;
                    child.pin = None;
                } else {
                    child.share *= scale;
                }
            }
            return Some(Ok(()));
        }
        c.children.iter_mut().find_map(|ch| apply(&mut ch.node, id, share))
    }
    let mut root = layout.root_node();
    match apply(&mut root, id, share) {
        Some(Ok(())) => {
            layout.replace_root(root);
            Vec::new()
        }
        Some(Err(note)) => vec![note],
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::fixture::fixture;
    use crate::layout::test_support::{rects, same_rects};

    fn id(s: &str) -> ModuleId {
        ModuleId::parse(s).unwrap()
    }

    fn at(a: At) -> OpenRule {
        OpenRule {
            at: Some(a),
            ..OpenRule::default()
        }
    }

    #[test]
    fn a_plugins_declared_rule_counts_where_position_counts() {
        let id = ModuleId::lua("markdown");
        let own = OpenRule {
            at: Some(At::Beside(Direction::Right)),
            focus: false,
            ..OpenRule::default()
        };
        let none = OpenRules::default();
        assert_eq!(
            none.resolve_own(&id, OpenPoint::NewInstance, Some(&own)),
            (own.clone(), RuleSource::Position)
        );
        assert_eq!(
            none.resolve_own(&id, OpenPoint::NewInstance, None).1,
            RuleSource::BuiltIn
        );
        let mut rules = OpenRules::default();
        let users = OpenRule {
            at: Some(At::Placement(Placement::BelowRoot)),
            size: Some(RuleSize::Share(0.3)),
            ..OpenRule::default()
        };
        rules.add(RuleTarget::Module(id.clone()), users.clone()).unwrap();
        assert_eq!(
            rules.resolve_own(&id, OpenPoint::NewInstance, Some(&own)),
            (users, RuleSource::Module)
        );
        // `resolve` with a position is `resolve_own` with that placement's rule.
        assert_eq!(
            none.resolve(&id, OpenPoint::Launch, Some(Placement::BelowRoot)),
            none.resolve_own(
                &id,
                OpenPoint::Launch,
                Some(&OpenRule {
                    at: Some(At::Placement(Placement::BelowRoot)),
                    ..OpenRule::default()
                })
            )
        );
    }

    #[test]
    fn a_module_rule_beats_its_kinds_which_beats_a_position_which_beats_the_built_in() {
        let mut rules = OpenRules::default();
        rules
            .add(
                RuleTarget::Kind(ModuleKind::Terminal),
                at(At::Placement(Placement::BelowRoot)),
            )
            .unwrap();
        rules
            .add(
                RuleTarget::Module(id("terminal#2")),
                OpenRule {
                    float: true,
                    scratchpad: true,
                    ..OpenRule::default()
                },
            )
            .unwrap();
        rules.add(RuleTarget::Module(id("lua:notes")), at(At::Tab)).unwrap();
        let (rule, source) = rules.resolve(&id("terminal#2"), OpenPoint::NewInstance, None);
        assert_eq!(source, RuleSource::Module);
        assert!(rule.scratchpad && rule.float);
        let (rule, source) = rules.resolve(&id("terminal#3"), OpenPoint::NewInstance, None);
        assert_eq!(
            (rule.at, source),
            (Some(At::Placement(Placement::BelowRoot)), RuleSource::Kind)
        );
        assert_eq!(
            rules.resolve(&ModuleId::terminal(), OpenPoint::Launch, None).1,
            RuleSource::Kind,
            "the base terminal follows its kind's rule"
        );
        let (rule, source) = rules.resolve(&id("lua:notes"), OpenPoint::Launch, Some(Placement::BelowRoot));
        assert_eq!(
            (rule.at, source),
            (Some(At::Tab), RuleSource::Module),
            "the module's rule beats a panel's position"
        );
        let (rule, source) = rules.resolve(&id("lua:todo"), OpenPoint::Launch, Some(Placement::BelowRoot));
        assert_eq!(
            (rule.at, source),
            (Some(At::Placement(Placement::BelowRoot)), RuleSource::Position)
        );
        let (rule, source) = rules.resolve(&id("agent#2"), OpenPoint::Break, None);
        assert_eq!(
            (rule.at, source),
            (Some(At::Beside(Direction::Right)), RuleSource::BuiltIn)
        );
    }

    #[test]
    fn the_built_in_default_depends_on_where_the_module_opens() {
        let rules = OpenRules::default();
        let at_of = |id: &ModuleId, point| rules.resolve(id, point, None).0.at;
        assert_eq!(
            at_of(&ModuleId::terminal(), OpenPoint::Launch),
            Some(At::Placement(Placement::BelowEditor))
        );
        assert_eq!(
            at_of(&ModuleId::terminal(), OpenPoint::Reopen),
            Some(At::Placement(Placement::BelowEditor))
        );
        assert_eq!(
            at_of(&id("terminal#2"), OpenPoint::NewInstance),
            Some(At::Beside(Direction::Right))
        );
        assert_eq!(
            at_of(&id("agent#2"), OpenPoint::NewInstance),
            Some(At::Beside(Direction::Right))
        );
        assert_eq!(
            at_of(&id("agent#2"), OpenPoint::Break),
            Some(At::Beside(Direction::Right))
        );
        assert_eq!(
            at_of(&id("agent#2"), OpenPoint::Launch),
            Some(At::Beside(Direction::Right))
        );
        let rule = rules.resolve(&ModuleId::terminal(), OpenPoint::Launch, None).0;
        assert!(rule.focus && !rule.float && !rule.scratchpad && rule.size.is_none());
    }

    #[test]
    fn two_rules_for_one_target_are_refused() {
        let mut rules = OpenRules::default();
        assert!(rules.is_empty());
        rules
            .add(RuleTarget::Kind(ModuleKind::Agent), OpenRule::default())
            .unwrap();
        assert_eq!(
            rules.add(RuleTarget::Kind(ModuleKind::Agent), OpenRule::default()),
            Err("a rule for it is already set".to_string())
        );
        rules
            .add(RuleTarget::Module(id("agent#2")), OpenRule::default())
            .unwrap();
        assert!(!rules.is_empty());
    }

    #[test]
    fn a_target_is_a_kind_or_a_module_id() {
        assert_eq!(
            RuleTarget::parse("terminal"),
            Ok(RuleTarget::Kind(ModuleKind::Terminal))
        );
        assert_eq!(RuleTarget::parse("agent"), Ok(RuleTarget::Kind(ModuleKind::Agent)));
        assert_eq!(RuleTarget::parse("agent#3"), Ok(RuleTarget::Module(id("agent#3"))));
        assert_eq!(RuleTarget::parse("lua:notes"), Ok(RuleTarget::Module(id("lua:notes"))));
        assert_eq!(RuleTarget::parse("editor"), Ok(RuleTarget::Module(ModuleId::editor())));
        assert!(RuleTarget::parse("terminal#1").is_err());
        assert!(RuleTarget::parse("chat").is_err());
    }

    #[test]
    fn with_no_module_holding_the_keys_a_side_falls_back() {
        assert_eq!(
            without_keys(At::Beside(Direction::Right)),
            (Placement::RightOfRoot, true)
        );
        assert_eq!(
            without_keys(At::Beside(Direction::Left)),
            (Placement::RightOfRoot, true)
        );
        assert_eq!(without_keys(At::Tab), (Placement::RightOfRoot, true));
        assert_eq!(without_keys(At::Beside(Direction::Down)), (Placement::BelowRoot, true));
        assert_eq!(without_keys(At::Beside(Direction::Up)), (Placement::BelowRoot, true));
        assert_eq!(
            without_keys(At::Placement(Placement::InPlaceOfEditor)),
            (Placement::InPlaceOfEditor, false)
        );
        for name in [
            "side",
            "bottom",
            "main",
            "below-editor",
            "right",
            "below",
            "left",
            "above",
            "tab",
        ] {
            assert_eq!(at_name(parse_at(name).unwrap()), name);
        }
        assert_eq!(parse_at("top"), None);
        assert_eq!(NOMINAL_GRID, Size { w: 1280, h: 720 });
    }

    #[test]
    fn the_declarations_a_first_launch_takes() {
        let mut rules = OpenRules::default();
        rules
            .add(
                RuleTarget::Kind(ModuleKind::Terminal),
                at(At::Placement(Placement::BelowRoot)),
            )
            .unwrap();
        rules
            .add(
                RuleTarget::Module(id("terminal#2")),
                OpenRule {
                    float: true,
                    scratchpad: true,
                    ..OpenRule::default()
                },
            )
            .unwrap();
        rules
            .add(RuleTarget::Kind(ModuleKind::Agent), at(At::Beside(Direction::Down)))
            .unwrap();
        rules.add(RuleTarget::Module(id("lua:notes")), at(At::Tab)).unwrap();
        let panels = [
            ModuleDecl {
                id: id("lua:notes"),
                placement: Placement::RightOfRoot,
            },
            ModuleDecl {
                id: id("lua:todo"),
                placement: Placement::BelowRoot,
            },
        ];
        let (decls, notes) = rules.launch_decls(&panels);
        assert_eq!(
            decls,
            vec![
                ModuleDecl {
                    id: ModuleId::terminal(),
                    placement: Placement::BelowRoot
                },
                ModuleDecl {
                    id: id("lua:notes"),
                    placement: Placement::RightOfRoot
                },
                ModuleDecl {
                    id: id("lua:todo"),
                    placement: Placement::BelowRoot
                },
                ModuleDecl {
                    id: id("terminal#2"),
                    placement: Placement::BelowEditor
                },
                ModuleDecl {
                    id: ModuleId::agent(),
                    placement: Placement::BelowRoot
                },
            ]
        );
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(
            notes
                .iter()
                .any(|n| n.contains("'lua:notes'") && n.contains("'tab'") && n.contains("side")),
            "{notes:?}"
        );
        assert!(
            notes
                .iter()
                .any(|n| n.contains("'agent'") && n.contains("'below'") && n.contains("bottom")),
            "{notes:?}"
        );
        let (decls, notes) = OpenRules::default().launch_decls(&panels);
        assert_eq!(
            decls,
            vec![
                panels[0].clone(),
                panels[1].clone(),
                ModuleDecl {
                    id: ModuleId::agent(),
                    placement: Placement::RightOfRoot
                },
            ],
            "no rules: the terminal keeps its own place and chat views go to the side"
        );
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn an_instance_rule_that_says_nothing_of_where_keeps_its_kinds_place() {
        let mut rules = OpenRules::default();
        rules
            .add(
                RuleTarget::Module(id("agent#2")),
                OpenRule {
                    float: true,
                    size: Some(RuleSize::Float { w: 0.5, h: 0.6 }),
                    ..OpenRule::default()
                },
            )
            .unwrap();
        rules
            .add(
                RuleTarget::Module(id("agent#3")),
                OpenRule {
                    focus: false,
                    ..OpenRule::default()
                },
            )
            .unwrap();
        rules
            .add(
                RuleTarget::Module(id("terminal#3")),
                OpenRule {
                    float: true,
                    ..OpenRule::default()
                },
            )
            .unwrap();
        let (decls, notes) = rules.launch_decls(&[]);
        let place = |module: &str| {
            decls
                .iter()
                .find(|d| d.id == id(module))
                .map(|d| d.placement)
                .unwrap_or_else(|| panic!("no declaration for {module}: {decls:?}"))
        };
        assert_eq!(place("agent#2"), Placement::RightOfRoot, "a chat view goes to the side");
        assert_eq!(place("agent#3"), Placement::RightOfRoot);
        assert_eq!(
            place("terminal#3"),
            Placement::BelowEditor,
            "a terminal below the editor"
        );
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn a_first_launch_places_only_its_own_modules() {
        let decls = [
            ModuleDecl {
                id: ModuleId::terminal(),
                placement: Placement::BelowEditor,
            },
            ModuleDecl {
                id: ModuleId::agent(),
                placement: Placement::RightOfRoot,
            },
            ModuleDecl {
                id: id("terminal#2"),
                placement: Placement::BelowEditor,
            },
            ModuleDecl {
                id: ModuleId::lua("notes"),
                placement: Placement::RightOfRoot,
            },
        ];
        let placed = first_launch_decls(&decls);
        assert_eq!(placed, vec![decls[0].clone(), decls[3].clone()]);
        let layout = Layout::initial(&placed).expect("only what a first launch places");
        assert_eq!(
            layout.leaves(),
            vec![
                ModuleId::editor(),
                ModuleId::terminal(),
                ModuleId::agent(),
                ModuleId::lua("notes")
            ]
        );
        assert_eq!(
            Layout::initial(&decls),
            Err(LayoutError::Duplicate(ModuleId::agent())),
            "Layout::initial itself stays strict"
        );
    }

    fn t2() -> ModuleId {
        id("terminal#2")
    }
    const GRID: Size = Size { w: 1000, h: 500 };

    #[test]
    fn a_rule_opens_a_module_beside_the_keys_on_any_side() {
        let size = Size { w: 1280, h: 721 };
        let mut layout = fixture("row(editor focus 0.6, agent)");
        let opened = open_by_rule(
            &mut layout,
            &t2(),
            &at(At::Beside(Direction::Right)),
            Some(&ModuleId::editor()),
            GRID,
        )
        .unwrap();
        assert_eq!(
            opened,
            Opened {
                takes_keys: true,
                stowed: false,
                notes: vec![]
            }
        );
        let mut placed = fixture("row(editor focus 0.6, agent)");
        crate::layout::place(&mut placed, &t2(), &ModuleId::editor(), crate::layout::Axis::Row).unwrap();
        same_rects(&layout, &placed);
        assert_eq!(layout.focus(), &t2());

        let mut layout = fixture("row(editor focus 0.6, agent)");
        open_by_rule(
            &mut layout,
            &t2(),
            &at(At::Beside(Direction::Up)),
            Some(&ModuleId::editor()),
            GRID,
        )
        .unwrap();
        let r = rects(&layout, size);
        assert!(r[&t2()].y < r[&ModuleId::editor()].y);
    }

    #[test]
    fn a_rule_at_a_first_launch_place_or_the_editors_place() {
        let size = Size { w: 1280, h: 721 };
        let mut layout = fixture("row(editor focus 0.6, agent)");
        open_by_rule(
            &mut layout,
            &t2(),
            &at(At::Placement(Placement::BelowRoot)),
            Some(&ModuleId::editor()),
            GRID,
        )
        .unwrap();
        assert_eq!(rects(&layout, size)[&t2()].w, 1280, "a full-width bottom row");

        let mut layout = fixture("row(editor focus 0.6, agent)");
        let opened = open_by_rule(
            &mut layout,
            &t2(),
            &OpenRule {
                at: Some(At::Placement(Placement::InPlaceOfEditor)),
                focus: false,
                ..OpenRule::default()
            },
            Some(&ModuleId::editor()),
            GRID,
        )
        .unwrap();
        assert!(layout.hidden().contains(&ModuleId::editor()));
        assert_eq!(
            layout.focus(),
            &t2(),
            "taking the editor's place takes the keys it held"
        );
        assert!(opened.takes_keys);
    }

    #[test]
    fn the_editors_place_with_focus_false_leaves_keys_held_elsewhere() {
        let mut layout = fixture("row(editor focus 0.6, agent)");
        layout.set_focus(&ModuleId::agent()).unwrap();
        let opened = open_by_rule(
            &mut layout,
            &t2(),
            &OpenRule {
                at: Some(At::Placement(Placement::InPlaceOfEditor)),
                focus: false,
                ..OpenRule::default()
            },
            Some(&ModuleId::agent()),
            GRID,
        )
        .unwrap();
        assert!(layout.hidden().contains(&ModuleId::editor()));
        assert!(!opened.takes_keys);
        assert_eq!(
            layout.focus(),
            &ModuleId::agent(),
            "the editor did not hold the keys, so nothing had to move them"
        );
    }

    #[test]
    fn an_open_that_puts_a_module_on_screen_ends_a_zoom() {
        for rule in [
            OpenRule {
                float: true,
                ..OpenRule::default()
            },
            at(At::Beside(Direction::Right)),
            at(At::Placement(Placement::BelowRoot)),
            at(At::Tab),
        ] {
            let mut layout = fixture("row(editor focus 0.6, agent)");
            layout.toggle_zoom(&ModuleId::editor());
            assert!(layout.zoomed().is_some());
            let opened = open_by_rule(&mut layout, &t2(), &rule, Some(&ModuleId::editor()), GRID).unwrap();
            assert!(opened.takes_keys, "{rule:?}");
            assert!(layout.zoomed().is_none(), "{rule:?}: the zoom ended");
            assert!(
                layout.is_visible(&t2()),
                "{rule:?}: the module with the keys is on screen"
            );
        }
        let mut layout = fixture("row(editor focus 0.6, agent)");
        layout.toggle_zoom(&ModuleId::editor());
        let rule = OpenRule {
            float: true,
            scratchpad: true,
            ..OpenRule::default()
        };
        open_by_rule(&mut layout, &t2(), &rule, Some(&ModuleId::editor()), GRID).unwrap();
        assert_eq!(
            layout.zoomed(),
            Some(&ModuleId::editor()),
            "a stowed open shows nothing, so the zoom stays"
        );
    }

    #[test]
    fn a_tab_rule_joins_the_keys_container_or_makes_one() {
        let mut layout = fixture("row(editor focus 0.6, agent)");
        open_by_rule(
            &mut layout,
            &t2(),
            &OpenRule {
                at: Some(At::Tab),
                focus: false,
                ..OpenRule::default()
            },
            Some(&ModuleId::editor()),
            GRID,
        )
        .unwrap();
        assert!(layout.is_tabbed_away(&t2()) && layout.is_visible(&ModuleId::editor()));
        assert_eq!(layout.focus(), &ModuleId::editor());
    }

    #[test]
    fn a_floating_rule_opens_a_float_sized_from_the_grid_and_a_scratchpad_rule_stows_it() {
        let mut layout = fixture("row(editor focus 0.6, agent)");
        let rule = OpenRule {
            float: true,
            size: Some(RuleSize::Float { w: 0.5, h: 0.4 }),
            ..OpenRule::default()
        };
        let opened = open_by_rule(&mut layout, &t2(), &rule, Some(&ModuleId::editor()), GRID).unwrap();
        assert_eq!(
            opened,
            Opened {
                takes_keys: true,
                stowed: false,
                notes: vec![]
            }
        );
        assert_eq!(
            layout.float_of(&t2()).unwrap().rect,
            FloatRect {
                cx: 0.5,
                cy: 0.5,
                w: 500,
                h: 200
            }
        );
        assert_eq!(layout.focus(), &t2());

        let mut layout = fixture("row(editor focus 0.6, agent)");
        let rule = OpenRule {
            float: true,
            scratchpad: true,
            ..OpenRule::default()
        };
        let opened = open_by_rule(&mut layout, &t2(), &rule, Some(&ModuleId::editor()), GRID).unwrap();
        assert_eq!(
            opened,
            Opened {
                takes_keys: false,
                stowed: true,
                notes: vec![]
            }
        );
        assert!(layout.is_stowed(&t2()));
        assert_eq!(
            layout.float_of(&t2()).unwrap().rect,
            FloatRect {
                cx: 0.5,
                cy: 0.5,
                w: 600,
                h: 300
            }
        );
        assert_eq!(layout.focus(), &ModuleId::editor());

        let mut layout = fixture("row(editor focus 0.6, agent)");
        assert_eq!(
            open_by_rule(&mut layout, &ModuleId::agent(), &OpenRule::default(), None, GRID),
            Err(LayoutError::Duplicate(ModuleId::agent())),
            "a module the layout has is never opened twice"
        );
    }

    #[test]
    fn a_rule_beside_the_keys_uses_the_tiled_anchor_when_the_keys_float() {
        let mut layout = fixture("row(editor focus 0.6, agent)");
        let t = ModuleId::terminal();
        layout
            .add_float(
                &t,
                FloatRect {
                    cx: 0.5,
                    cy: 0.5,
                    w: 400,
                    h: 300,
                },
                false,
            )
            .unwrap();
        layout.set_focus(&t).unwrap();
        for rule in [at(At::Beside(Direction::Right)), at(At::Tab)] {
            let mut layout = layout.clone();
            open_by_rule(&mut layout, &t2(), &rule, Some(&t), GRID).unwrap();
            assert!(!layout.is_floating(&t2()), "{rule:?}: never inside a float");
            assert!(layout.leaves().contains(&t2()), "{rule:?}");
            assert!(layout.is_floating(&t), "{rule:?}: the float stays a float");
            let editor_at = layout.leaves().iter().position(|m| *m == ModuleId::editor()).unwrap();
            assert_eq!(
                layout.leaves()[editor_at + 1],
                t2(),
                "{rule:?}: beside the most recent tiled module"
            );
        }
    }

    #[test]
    fn with_no_module_holding_the_keys_a_beside_rule_opens_at_the_side_with_a_note() {
        let size = Size { w: 1280, h: 721 };
        let mut layout = fixture("row(editor focus 0.6, agent)");
        let opened = open_by_rule(&mut layout, &t2(), &at(At::Beside(Direction::Left)), None, GRID).unwrap();
        assert_eq!(layout.leaves().last(), Some(&t2()));
        assert_eq!(
            rects(&layout, size)[&t2()].h,
            rects(&layout, size)[&ModuleId::agent()].h,
            "full height"
        );
        assert!(
            opened.notes.iter().any(|n| n.contains("'left'") && n.contains("side")),
            "{:?}",
            opened.notes
        );
    }

    #[test]
    fn a_share_rule_sizes_the_new_module_and_focus_false_leaves_the_keys() {
        let size = Size { w: 1280, h: 721 };
        let mut layout = fixture("row(editor focus 0.6, agent)");
        let rule = OpenRule {
            at: Some(At::Beside(Direction::Down)),
            size: Some(RuleSize::Share(0.25)),
            focus: false,
            ..OpenRule::default()
        };
        let opened = open_by_rule(&mut layout, &t2(), &rule, Some(&ModuleId::editor()), GRID).unwrap();
        assert!(!opened.takes_keys);
        assert_eq!(layout.focus(), &ModuleId::editor());
        let r = rects(&layout, size);
        let (t, e) = (r[&t2()], r[&ModuleId::editor()]);
        let got = t.h as f32 / (t.h + e.h) as f32;
        assert!((got - 0.25).abs() < 0.02, "{got}");
    }

    #[test]
    fn a_first_launch_floats_or_stows_what_a_floating_rule_names() {
        use crate::layout::geometry::Frame;
        let t = ModuleId::terminal();
        let decls = [ModuleDecl {
            id: t.clone(),
            placement: Placement::BelowEditor,
        }];
        let launch = || {
            let mut layout = Layout::initial(&decls).unwrap();
            crate::layout::hide(&mut layout, &t, &Frame::new(Size { w: 1280, h: 721 }, 1)).unwrap();
            layout
        };
        let mut layout = launch();
        let notes = apply_launch_rule(
            &mut layout,
            &t,
            &OpenRule {
                float: true,
                scratchpad: true,
                ..OpenRule::default()
            },
            NOMINAL_GRID,
        )
        .unwrap();
        assert!(layout.is_stowed(&t) && !layout.leaves().contains(&t));
        assert_eq!(
            layout.float_of(&t).unwrap().rect,
            FloatRect {
                cx: 0.5,
                cy: 0.5,
                w: 768,
                h: 432
            }
        );
        assert_eq!(notes.len(), 1, "{notes:?}");

        let mut layout = launch();
        apply_launch_rule(
            &mut layout,
            &t,
            &OpenRule {
                float: true,
                ..OpenRule::default()
            },
            NOMINAL_GRID,
        )
        .unwrap();
        assert!(layout.is_floating(&t) && layout.hidden().contains(&t) && !layout.is_stowed(&t));

        let mut layout = launch();
        let before = layout.clone();
        let notes = apply_launch_rule(
            &mut layout,
            &t,
            &at(At::Placement(Placement::BelowEditor)),
            NOMINAL_GRID,
        )
        .unwrap();
        assert!(notes.is_empty());
        same_rects(&layout, &before);

        // A Lua panel in the editor's place holds the keys at a first launch; floating or stowing it gives them back
        // to the editor, shown again, so the keys are never on a hidden module.
        let notes_panel = ModuleId::lua("notes");
        for scratchpad in [true, false] {
            let mut layout = Layout::initial(&[ModuleDecl {
                id: notes_panel.clone(),
                placement: Placement::InPlaceOfEditor,
            }])
            .unwrap();
            assert_eq!(layout.focus(), &notes_panel);
            let rule = OpenRule {
                float: true,
                scratchpad,
                focus: false,
                ..OpenRule::default()
            };
            apply_launch_rule(&mut layout, &notes_panel, &rule, NOMINAL_GRID).unwrap();
            assert!(layout.is_shown(&ModuleId::editor()), "scratchpad {scratchpad}");
            assert_eq!(layout.focus(), &ModuleId::editor(), "scratchpad {scratchpad}");
            assert!(layout.can_focus(layout.focus()).is_ok());
            assert_eq!(layout.is_stowed(&notes_panel), scratchpad);
        }
    }
}
