//! The pure half of the global `f` HINT (spec: docs/superpowers/specs/2026-09-19-global-hint-design.md).
//! Label assignment, the typing state machine and the split of one label set across the window's
//! regions. No GTK and no WebView: `shell::hint` drives this with widget indices, the panel with
//! its own target indices, and neither can disagree with the other about which label is whose.

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
        Self { labels, typed: String::new() }
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
        let mut matching = self.labels.iter().enumerate().filter(|(_, l)| l.starts_with(next.as_str()));
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
    /// A GTK target before the panel (top bar items, then the editor).
    Before(usize),
    /// The panel's own target at this index.
    Panel(usize),
    /// A GTK target after the panel (the bottom plugin pane).
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
    LabelPlan { before: all, panel: panel_labels, after: after_labels }
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
        self.before.iter().chain(&self.panel).chain(&self.after).cloned().collect()
    }
}

/// Where one GTK-side HINT target lives in the window. `shell` maps each to its widget; nothing
/// here knows what a widget is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// The top bar's item at this index (of the items `shell` passed in `WindowLayout::top_visible`).
    Top(usize),
    /// The main slot, holding the editor: focused through the editor pane, which also tells the
    /// input method, never through a bare widget focus.
    Editor,
    /// The main slot, holding a Lua plugin's widget in place of the editor.
    Main,
    /// The side slot, holding a Lua plugin's widget in place of the agent panel: one target,
    /// because only the built-in panel can label its own inside.
    Side,
    /// The bottom plugin slot.
    Bottom,
}

/// What `shell` sees of the window when a HINT starts. Visibility means spec §2.2's "看得见":
/// mapped, with a non-zero size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowLayout {
    pub top_visible: Vec<bool>,
    pub main_visible: bool,
    /// The main slot holds the editor (no Lua plugin replaced it).
    pub main_is_editor: bool,
    pub side_visible: bool,
    /// The side slot holds the built-in agent panel (no Lua plugin replaced it).
    pub side_is_agent_panel: bool,
    /// `false` when there is no bottom slot at all.
    pub bottom_visible: bool,
}

/// Where a landing goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landing {
    Gtk(Slot),
    /// The panel's own target at this index (`hint_land`).
    Panel(usize),
}

/// The GTK targets in spec order around the panel's, and whether the panel is asked for its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetOrder {
    /// Top bar items, then the main slot, then a Lua side panel.
    pub before: Vec<Slot>,
    /// Only the built-in agent panel, and only when it is on screen: a panel that is hidden or
    /// replaced is never sent `hint_collect`, so a HINT never waits the timeout for it.
    pub ask_panel: bool,
    pub after: Vec<Slot>,
}

pub fn order_targets(layout: &WindowLayout) -> TargetOrder {
    let mut before: Vec<Slot> =
        layout.top_visible.iter().enumerate().filter(|(_, v)| **v).map(|(i, _)| Slot::Top(i)).collect();
    if layout.main_visible {
        before.push(if layout.main_is_editor { Slot::Editor } else { Slot::Main });
    }
    let ask_panel = layout.side_is_agent_panel && layout.side_visible;
    if !layout.side_is_agent_panel && layout.side_visible {
        before.push(Slot::Side);
    }
    let after = if layout.bottom_visible { vec![Slot::Bottom] } else { Vec::new() };
    TargetOrder { before, ask_panel, after }
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
            Region::Before(i) => self.before.get(i).copied().map(Landing::Gtk),
            Region::Panel(i) => Some(Landing::Panel(i)),
            Region::After(i) => self.after.get(i).copied().map(Landing::Gtk),
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

    fn layout() -> WindowLayout {
        WindowLayout {
            top_visible: vec![true, false, true],
            main_visible: true,
            main_is_editor: true,
            side_visible: true,
            side_is_agent_panel: true,
            bottom_visible: true,
        }
    }

    /// Spec §2.2's order: visible top bar items, the main slot, the panel, the bottom slot.
    #[test]
    fn targets_are_ordered_top_bar_then_main_then_panel_then_bottom() {
        let order = order_targets(&layout());
        assert_eq!(order.before, vec![Slot::Top(0), Slot::Top(2), Slot::Editor]);
        assert!(order.ask_panel);
        assert_eq!(order.after, vec![Slot::Bottom]);
        let plan = order.plan(2);
        assert_eq!(plan.all(), assign_labels(6));
        assert_eq!(order.landing(&plan, 1), Some(Landing::Gtk(Slot::Top(2))));
        assert_eq!(order.landing(&plan, 2), Some(Landing::Gtk(Slot::Editor)));
        assert_eq!(order.landing(&plan, 4), Some(Landing::Panel(1)));
        assert_eq!(order.landing(&plan, 5), Some(Landing::Gtk(Slot::Bottom)));
        assert_eq!(order.landing(&plan, 6), None);
    }

    /// A Lua plugin in either slot is one plain target, and one in the side slot takes the
    /// panel's place in the order: after the main slot, never before it.
    #[test]
    fn a_lua_plugin_in_a_slot_is_one_target_in_that_slots_place() {
        let order = order_targets(&WindowLayout { main_is_editor: false, side_is_agent_panel: false, ..layout() });
        assert_eq!(order.before, vec![Slot::Top(0), Slot::Top(2), Slot::Main, Slot::Side]);
        assert!(!order.ask_panel, "a replaced agent panel is never asked");
    }

    /// A hidden agent panel is not asked, so the HINT does not wait 300ms for an answer that
    /// cannot help; nothing hidden gets a label.
    #[test]
    fn a_hidden_panel_is_not_asked_and_hidden_slots_get_no_label() {
        let order = order_targets(&WindowLayout {
            top_visible: vec![false],
            main_visible: false,
            side_visible: false,
            bottom_visible: false,
            ..layout()
        });
        assert_eq!(order, TargetOrder { before: vec![], ask_panel: false, after: vec![] });
        let order = order_targets(&WindowLayout { side_visible: false, side_is_agent_panel: false, ..layout() });
        assert!(!order.before.contains(&Slot::Side));
    }

    /// A count from a panel that was never asked labels nothing in it.
    #[test]
    fn an_unasked_panels_answer_is_ignored() {
        let order = order_targets(&WindowLayout { side_is_agent_panel: false, ..layout() });
        let plan = order.plan(5);
        assert!(plan.panel.is_empty());
        assert_eq!(plan.all().len(), order.before.len() + order.after.len());
    }
}
