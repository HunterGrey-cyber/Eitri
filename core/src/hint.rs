//! The pure half of the global `f` HINT (spec: docs/superpowers/specs/2026-09-19-global-hint-design.md).
//! Label assignment, the typing state machine and the split of one label set across the window's
//! regions. No GTK and no WebView: `shell::hint` drives this with module ids, the panel with its
//! own target indices, and neither can disagree with the other about which label is whose.
//!
//! **The window is a list of modules in tree order** since the modules design's P1
//! (docs/superpowers/specs/2026-09-23-modules-and-canvas-design.md): the regions used to be the
//! three fixed slots (main, side, bottom), and a layout that is data has no fixed slots.

use crate::layout::{ModuleId, ModuleKind};

/// Label letters in priority order: home row first. `f` is absent on purpose, so pressing `f`
/// twice can never land anywhere.
pub const ALPHABET: &str = "asdjklghweruio";

/// `count` labels, all the same length -- the smallest `L` with `ALPHABET.len()^L >= count` -- in
/// lexicographic order over `ALPHABET`. Equal length is what makes the set prefix-free.
pub fn assign_labels(count: usize) -> Vec<String> {
    if count == 0 {
        return Vec::new();
    }
    let letters: Vec<char> = ALPHABET.chars().collect();
    let base = letters.len();
    let mut len = 1;
    while base.pow(len as u32) < count {
        len += 1;
    }
    (0..count)
        .map(|mut n| {
            let mut out = vec![' '; len];
            for slot in out.iter_mut().rev() {
                *slot = letters[n % base];
                n /= base;
            }
            out.into_iter().collect()
        })
        .collect()
}

/// What one keystroke did to a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintStep {
    /// The typed prefix still matches more than one label.
    Narrowed { typed: String },
    /// The typed string is exactly this label.
    Landed { index: usize },
    /// Not a continuation of any label: nothing changed.
    Ignored,
}

/// The typing state of one HINT session over a fixed label set.
#[derive(Debug, Clone)]
pub struct HintSession {
    labels: Vec<String>,
    typed: String,
}

impl HintSession {
    pub fn new(labels: Vec<String>) -> Self {
        Self {
            labels,
            typed: String::new(),
        }
    }

    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    pub fn typed(&self) -> &str {
        &self.typed
    }

    pub fn key(&mut self, ch: char) -> HintStep {
        let mut next = self.typed.clone();
        next.push(ch);
        let mut matching = self
            .labels
            .iter()
            .enumerate()
            .filter(|(_, l)| l.starts_with(next.as_str()));
        let Some((first, label)) = matching.next() else {
            return HintStep::Ignored;
        };
        if label.len() == next.len() {
            // Equal-length labels: an exact match is the only match.
            return HintStep::Landed { index: first };
        }
        self.typed = next.clone();
        HintStep::Narrowed { typed: next }
    }

    pub fn backspace(&mut self) -> &str {
        self.typed.pop();
        &self.typed
    }
}

/// Where a global label index points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// A GTK target before the panel (top bar items, then every module ahead of the agent).
    Before(usize),
    /// The panel's own target at this index.
    Panel(usize),
    /// A GTK target after the panel (every module behind the agent in tree order).
    After(usize),
}

/// One label set, split in spec order: GTK targets before the panel, the panel, GTK after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelPlan {
    pub before: Vec<String>,
    pub panel: Vec<String>,
    pub after: Vec<String>,
}

pub fn plan_labels(before: usize, panel: usize, after: usize) -> LabelPlan {
    let mut all = assign_labels(before + panel + after);
    let after_labels = all.split_off(before + panel);
    let panel_labels = all.split_off(before);
    LabelPlan {
        before: all,
        panel: panel_labels,
        after: after_labels,
    }
}

impl LabelPlan {
    pub fn region_of(&self, index: usize) -> Option<Region> {
        let (b, p, a) = (self.before.len(), self.panel.len(), self.after.len());
        if index < b {
            Some(Region::Before(index))
        } else if index < b + p {
            Some(Region::Panel(index - b))
        } else if index < b + p + a {
            Some(Region::After(index - b - p))
        } else {
            None
        }
    }

    /// Every label, in the global order `HintSession` indexes.
    pub fn all(&self) -> Vec<String> {
        self.before
            .iter()
            .chain(&self.panel)
            .chain(&self.after)
            .cloned()
            .collect()
    }
}

/// Where one GTK-side HINT target lives in the window. `shell` maps each to its widget; nothing
/// here knows what a widget is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot {
    /// The top bar's item at this index (of the items `shell` passed in `WindowLayout::top_visible`).
    Top(usize),
    /// A module's host, as one target. The editor is focused through the editor pane, which also
    /// tells the input method -- `shell`'s job, by the module's kind. The agent is never one: it
    /// labels its own inside (`TargetOrder::ask_panel`).
    Module(ModuleId),
}

/// What `shell` sees of the window when a HINT starts. Visibility means spec §2.2's "看得见":
/// mapped, with a non-zero size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowLayout {
    pub top_visible: Vec<bool>,
    /// Every module in the layout's tree order (`Layout::leaves`), with whether it is on screen.
    pub modules: Vec<(ModuleId, bool)>,
}

/// Where a landing goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Landing {
    Gtk(Slot),
    /// The panel's own target at this index (`hint_land`).
    Panel(usize),
}

/// The GTK targets in spec order around the panel's, and whether the panel is asked for its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetOrder {
    /// Top bar items, then every visible module ahead of the agent in tree order.
    pub before: Vec<Slot>,
    /// Only the built-in agent panel, and only when it is on screen: a panel that is hidden or
    /// zoomed away is never sent `hint_collect`, so a HINT never waits the timeout for it.
    pub ask_panel: bool,
    /// Every visible module after the agent in tree order.
    pub after: Vec<Slot>,
}

/// Spec §2.2's order, generalized: the visible top bar items, then the visible modules in tree
/// order, with the agent's own labels where the agent sits. For the window as it was before the
/// modules design -- `[editor | agent]`, with or without a panel below -- that is exactly the old
/// order: top bar, main slot, panel, bottom slot.
pub fn order_targets(layout: &WindowLayout) -> TargetOrder {
    let mut before: Vec<Slot> = layout
        .top_visible
        .iter()
        .enumerate()
        .filter(|(_, v)| **v)
        .map(|(i, _)| Slot::Top(i))
        .collect();
    let mut after = Vec::new();
    let mut ask_panel = false;
    for (id, visible) in &layout.modules {
        if !visible {
            continue;
        }
        if id.kind() == ModuleKind::Agent {
            ask_panel = true;
        } else if ask_panel {
            after.push(Slot::Module(id.clone()));
        } else {
            before.push(Slot::Module(id.clone()));
        }
    }
    TargetOrder {
        before,
        ask_panel,
        after,
    }
}

impl TargetOrder {
    /// The label plan for the panel's answer. A count from a panel that was never asked (a stray
    /// or stale `hint_targets`) labels nothing in the panel.
    pub fn plan(&self, panel_answer: usize) -> LabelPlan {
        let panel = if self.ask_panel { panel_answer } else { 0 };
        plan_labels(self.before.len(), panel, self.after.len())
    }

    /// Where global label `index` of `plan` lands.
    pub fn landing(&self, plan: &LabelPlan, index: usize) -> Option<Landing> {
        match plan.region_of(index)? {
            Region::Before(i) => self.before.get(i).cloned().map(Landing::Gtk),
            Region::Panel(i) => Some(Landing::Panel(i)),
            Region::After(i) => self.after.get(i).cloned().map(Landing::Gtk),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_targets_no_labels() {
        assert!(assign_labels(0).is_empty());
    }

    #[test]
    fn up_to_fourteen_targets_get_one_letter_each_in_alphabet_order() {
        assert_eq!(assign_labels(3), vec!["a", "s", "d"]);
        let fourteen = assign_labels(14);
        assert_eq!(fourteen.concat(), ALPHABET);
    }

    #[test]
    fn fifteen_targets_switch_every_label_to_two_letters() {
        let labels = assign_labels(15);
        assert_eq!(labels.len(), 15);
        assert!(labels.iter().all(|l| l.len() == 2));
        assert_eq!(&labels[..3], &["aa", "as", "ad"]);
    }

    #[test]
    fn beyond_196_uses_three_letters() {
        let labels = assign_labels(197);
        assert!(labels.iter().all(|l| l.len() == 3));
        assert_eq!(assign_labels(196).iter().map(String::len).max(), Some(2));
    }

    #[test]
    fn labels_never_contain_f_and_are_prefix_free() {
        for n in [1, 14, 15, 60, 196, 197] {
            let labels = assign_labels(n);
            assert!(labels.iter().all(|l| !l.contains('f')), "n={n}");
            for (i, a) in labels.iter().enumerate() {
                for (j, b) in labels.iter().enumerate() {
                    if i != j {
                        assert!(!b.starts_with(a.as_str()), "{a} prefixes {b} at n={n}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_session_narrows_then_lands() {
        let mut s = HintSession::new(assign_labels(20));
        assert_eq!(s.key('a'), HintStep::Narrowed { typed: "a".into() });
        assert_eq!(s.key('s'), HintStep::Landed { index: 1 });
    }

    #[test]
    fn a_non_label_key_is_ignored_and_changes_nothing() {
        let mut s = HintSession::new(assign_labels(3));
        assert_eq!(s.key('z'), HintStep::Ignored);
        assert_eq!(s.key('f'), HintStep::Ignored);
        assert_eq!(s.typed(), "");
        assert_eq!(s.key('d'), HintStep::Landed { index: 2 });
    }

    #[test]
    fn backspace_undoes_one_letter() {
        let mut s = HintSession::new(assign_labels(20));
        s.key('a');
        assert_eq!(s.backspace(), "");
        assert_eq!(s.backspace(), "");
    }

    #[test]
    fn a_plan_splits_one_label_set_across_regions_in_order() {
        let plan = plan_labels(2, 3, 1);
        assert_eq!(plan.before, vec!["a", "s"]);
        assert_eq!(plan.panel, vec!["d", "j", "k"]);
        assert_eq!(plan.after, vec!["l"]);
        assert_eq!(plan.region_of(0), Some(Region::Before(0)));
        assert_eq!(plan.region_of(4), Some(Region::Panel(2)));
        assert_eq!(plan.region_of(5), Some(Region::After(0)));
        assert_eq!(plan.region_of(6), None);
    }

    fn editor() -> Slot {
        Slot::Module(ModuleId::editor())
    }

    fn lua(id: &str) -> ModuleId {
        ModuleId::lua(id)
    }

    /// Today's window with a Lua panel below: `[editor | agent]` over `lua:bottom`.
    fn layout() -> WindowLayout {
        WindowLayout {
            top_visible: vec![true, false, true],
            modules: vec![
                (ModuleId::editor(), true),
                (ModuleId::agent(), true),
                (lua("bottom"), true),
            ],
        }
    }

    /// Spec §2.2's order: visible top bar items, the main slot, the panel, the bottom slot. The
    /// same labels and the same landings the slot-based order gave this window (`Slot::Editor`
    /// then, `Slot::Module(editor)` now; `Slot::Bottom` then, the bottom module now).
    #[test]
    fn targets_are_ordered_top_bar_then_main_then_panel_then_bottom() {
        let order = order_targets(&layout());
        assert_eq!(order.before, vec![Slot::Top(0), Slot::Top(2), editor()]);
        assert!(order.ask_panel);
        assert_eq!(order.after, vec![Slot::Module(lua("bottom"))]);
        let plan = order.plan(2);
        assert_eq!(plan.all(), assign_labels(6));
        assert_eq!(order.landing(&plan, 1), Some(Landing::Gtk(Slot::Top(2))));
        assert_eq!(order.landing(&plan, 2), Some(Landing::Gtk(editor())));
        assert_eq!(order.landing(&plan, 4), Some(Landing::Panel(1)));
        assert_eq!(order.landing(&plan, 5), Some(Landing::Gtk(Slot::Module(lua("bottom")))));
        assert_eq!(order.landing(&plan, 6), None);
    }

    /// The default window, no bottom panel: the same order as before minus the bottom.
    #[test]
    fn the_default_window_is_top_bar_then_editor_then_panel() {
        let order = order_targets(&WindowLayout {
            modules: vec![(ModuleId::editor(), true), (ModuleId::agent(), true)],
            ..layout()
        });
        assert_eq!(order.before, vec![Slot::Top(0), Slot::Top(2), editor()]);
        assert!(order.ask_panel);
        assert!(order.after.is_empty());
    }

    /// A Lua panel is one plain target in its tree position: one that took the editor's place
    /// comes before the panel, one placed right of the root after it.
    #[test]
    fn a_lua_panel_is_one_target_where_it_sits_in_the_tree() {
        let order = order_targets(&WindowLayout {
            top_visible: vec![],
            modules: vec![
                (lua("main"), true),
                (ModuleId::editor(), false),
                (ModuleId::agent(), true),
                (lua("side"), true),
            ],
        });
        assert_eq!(order.before, vec![Slot::Module(lua("main"))]);
        assert!(order.ask_panel);
        assert_eq!(order.after, vec![Slot::Module(lua("side"))]);
    }

    /// A hidden agent panel is not asked, so the HINT does not wait 300ms for an answer that
    /// cannot help; nothing hidden gets a label.
    #[test]
    fn a_hidden_panel_is_not_asked_and_hidden_modules_get_no_label() {
        let order = order_targets(&WindowLayout {
            top_visible: vec![false],
            modules: vec![
                (ModuleId::editor(), false),
                (ModuleId::agent(), false),
                (lua("bottom"), false),
            ],
        });
        assert_eq!(
            order,
            TargetOrder {
                before: vec![],
                ask_panel: false,
                after: vec![]
            }
        );
        let order = order_targets(&WindowLayout {
            modules: vec![
                (ModuleId::editor(), true),
                (ModuleId::agent(), false),
                (lua("bottom"), true),
            ],
            ..layout()
        });
        assert!(!order.ask_panel);
        assert_eq!(
            order.before,
            vec![Slot::Top(0), Slot::Top(2), editor(), Slot::Module(lua("bottom"))],
            "with the panel off screen everything visible is one run, in tree order"
        );
    }

    /// A count from a panel that was never asked labels nothing in it.
    #[test]
    fn an_unasked_panels_answer_is_ignored() {
        let order = order_targets(&WindowLayout {
            modules: vec![(ModuleId::editor(), true), (ModuleId::agent(), false)],
            ..layout()
        });
        let plan = order.plan(5);
        assert!(plan.panel.is_empty());
        assert_eq!(plan.all().len(), order.before.len() + order.after.len());
    }
}
