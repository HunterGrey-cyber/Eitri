//! tmux's `last-pane` (`prefix ;`) and `select-pane -t :.+` (`prefix o`) over the module tree.
use super::{module::ModuleId, tree::Layout};

/// The module that held the keys before the one holding them now -- real history only. `Layout::mru`
/// starts as tree order, so its second entry can name a module nobody visited; this starts empty and
/// fills from `pane_focus`'s owner changes alone.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FocusHistory {
    current: Option<ModuleId>,
    previous: Option<ModuleId>,
}

impl FocusHistory {
    /// `owner` now holds the keys. The same module again is no move, so a repeated report never
    /// overwrites the module before it.
    pub fn note(&mut self, owner: &ModuleId) {
        if self.current.as_ref() != Some(owner) {
            self.previous = self.current.replace(owner.clone());
        }
    }

    /// The module that held the keys before the current one, if a second module ever did.
    pub fn previous(&self) -> Option<&ModuleId> {
        self.previous.as_ref()
    }
}

/// The next tile in view, then shown floats bottom to top, wrapping; `None` with fewer than two
/// shown. A zoom does not hide the others here: the caller unzooms, as tmux's `select-pane` does.
pub fn next_on_screen(layout: &Layout, from: &ModuleId) -> Option<ModuleId> {
    let shown: Vec<ModuleId> = layout
        .modules()
        .into_iter()
        .filter(|id| layout.is_in_view(id))
        .collect();
    if shown.len() < 2 {
        return None;
    }
    let at = shown.iter().position(|id| id == from).map_or(0, |i| i + 1);
    Some(shown[at % shown.len()].clone())
}

/// Walks visible terminals in tree order, wrapping in either direction.
pub fn next_terminal(layout: &Layout, from: &ModuleId, forward: bool) -> Option<ModuleId> {
    let terminals: Vec<_> = layout
        .modules()
        .into_iter()
        .filter(|id| id.kind() == super::module::ModuleKind::Terminal && layout.is_visible(id))
        .collect();
    if terminals.len() < 2 {
        return None;
    }
    let at = terminals.iter().position(|id| id == from);
    let next = match at {
        Some(at) if forward => (at + 1) % terminals.len(),
        Some(at) => (at + terminals.len() - 1) % terminals.len(),
        None if forward => 0,
        None => terminals.len() - 1,
    };
    Some(terminals[next].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_remembers_the_module_before_the_current_one_and_toggles() {
        let (e, a) = (ModuleId::editor(), ModuleId::agent());
        let mut h = FocusHistory::default();
        assert_eq!(h.previous(), None, "no history before a second module held the keys");
        h.note(&e);
        h.note(&e);
        assert_eq!(h.previous(), None, "the same module again is no move");
        h.note(&a);
        assert_eq!(h.previous(), Some(&e));
        h.note(&e);
        assert_eq!(h.previous(), Some(&a));
    }

    #[test]
    fn next_on_screen_walks_shown_modules_in_tree_order_and_wraps() {
        use super::super::geometry::{hide, Frame, Size};
        use super::super::module::{ModuleDecl, Placement};
        let (e, a, t) = (ModuleId::editor(), ModuleId::agent(), ModuleId::terminal());
        let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
        let mut layout = Layout::initial(&[ModuleDecl {
            id: t.clone(),
            placement: Placement::BelowRoot,
        }])
        .unwrap();
        assert_eq!(
            layout.leaves(),
            [e.clone(), a.clone(), t.clone()],
            "the tree order this test assumes"
        );
        assert_eq!(next_on_screen(&layout, &e), Some(a.clone()));
        assert_eq!(next_on_screen(&layout, &t), Some(e.clone()), "wraps");
        layout.toggle_zoom(&a);
        assert_eq!(
            next_on_screen(&layout, &a),
            Some(t.clone()),
            "a zoom hides nothing here"
        );
        layout.unzoom();
        hide(&mut layout, &t, &frame).unwrap();
        assert_eq!(
            next_on_screen(&layout, &a),
            Some(e.clone()),
            "a hidden module is skipped"
        );
        hide(&mut layout, &a, &frame).unwrap();
        assert_eq!(next_on_screen(&layout, &e), None, "one module on screen");
    }

    /// The keys never sit on a hidden module (`Layout::set_focus` refuses one), but asked from one
    /// anyway the walk starts at the first module on screen rather than answering nothing.
    #[test]
    fn next_on_screen_from_a_module_not_on_screen_starts_at_the_first_shown_one() {
        use super::super::geometry::{hide, Frame, Size};
        use super::super::module::{ModuleDecl, Placement};
        let (e, t) = (ModuleId::editor(), ModuleId::terminal());
        let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
        let mut layout = Layout::initial(&[ModuleDecl {
            id: t.clone(),
            placement: Placement::BelowRoot,
        }])
        .unwrap();
        hide(&mut layout, &t, &frame).unwrap();
        assert_eq!(next_on_screen(&layout, &t), Some(e));
    }

    #[test]
    fn next_terminal_walks_terminals_on_screen_in_tree_order() {
        use super::super::{ModuleDecl, Placement};
        let ids = ["terminal", "terminal#2", "terminal#3"].map(|s| ModuleId::parse(s).unwrap());
        let mut layout = Layout::initial(&ids.clone().map(|id| ModuleDecl {
            id,
            placement: Placement::BelowEditor,
        }))
        .unwrap();
        let order: Vec<_> = layout.leaves().into_iter().filter(|id| ids.contains(id)).collect();
        assert_eq!(next_terminal(&layout, &order[0], true), Some(order[1].clone()));
        assert_eq!(next_terminal(&layout, &order[0], false), Some(order[2].clone()));
        layout.hide_unfocused(&order[1]).unwrap();
        assert_eq!(next_terminal(&layout, &order[0], true), Some(order[2].clone()));
        assert_eq!(next_terminal(&layout, &order[2], true), Some(order[0].clone()));
        layout.hide_unfocused(&order[2]).unwrap();
        assert_eq!(next_terminal(&layout, &order[0], true), None);
    }

    /// A terminal on a tab its container does not draw is not on screen, so `prefix n`/`p` across
    /// terminals passes over it; it is reached through its container's tabs instead.
    #[test]
    fn next_terminal_skips_an_inactive_tab() {
        use super::super::con::{Child, Con, ConLayout, Node};
        let [t1, t2, t3] = ["terminal", "terminal#2", "terminal#3"].map(|s| ModuleId::parse(s).unwrap());
        let leaf = |id: &ModuleId, share: f32| Child::new(Node::Leaf(id.clone()), share);
        let tabs = Con::new(ConLayout::Tabbed, vec![leaf(&t1, 0.5), leaf(&t2, 0.5)]);
        let third = 1.0 / 3.0;
        let root = Con::new(
            ConLayout::SplitH,
            vec![
                Child::new(Node::Con(tabs), third),
                leaf(&ModuleId::editor(), third),
                leaf(&t3, third),
            ],
        );
        let layout = Layout::new(Node::Con(root), t1.clone()).unwrap();
        assert!(
            layout.is_tabbed_away(&t2),
            "the premise: terminal#2 is the tab not drawn"
        );
        assert_eq!(next_terminal(&layout, &t1, true), Some(t3.clone()));
        assert_eq!(next_terminal(&layout, &t3, true), Some(t1.clone()));
        assert_eq!(next_terminal(&layout, &t1, false), Some(t3.clone()));
    }
}
