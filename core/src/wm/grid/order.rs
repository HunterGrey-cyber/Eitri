//! The order a module leaves the screen in, when the keys may be on it: the module that gets them is mapped and
//! given them first, and only then is what left unmapped. Unmapping first would leave the keys on a widget
//! leaving the screen, and the toolkit's own focus chain would choose where they go.

use crate::layout::{Layout, LayoutError, ModuleId};

/// One step of a change that may move the keys, in the order the grid carries them out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Put the module's host on screen: a tab not drawn a moment ago has no mapped widget to take the keys.
    Map(ModuleId),
    /// Give the module the keys.
    Focus(ModuleId),
    /// Take off screen what the layout no longer shows.
    Unmap,
    /// Put the float band in the layout's order.
    Restack,
}

/// The steps after a change that names `next` as the module getting the keys: mapped, given them, then the
/// unmap; with no one to give them to, the unmap alone.
pub fn leave_order(next: Option<&ModuleId>) -> Vec<Step> {
    match next {
        Some(next) => vec![Step::Map(next.clone()), Step::Focus(next.clone()), Step::Unmap],
        None => vec![Step::Unmap],
    }
}

/// The layout half of choosing a tab (a strip click, a navigation key, a module key, `eitri.layout.focus`):
/// `to`'s tab drawn and given the keys in the layout; the module that gets them, for [`leave_order`].
pub fn tab_switch(layout: &mut Layout, to: &ModuleId) -> Result<Option<ModuleId>, LayoutError> {
    crate::layout::switch_tab(layout, to).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Child, Con, ConLayout, Node};

    /// `editor` beside a tabbed container of `lua:a` (drawn) and `lua:b`, with the keys on the editor.
    fn editor_beside_tabs() -> Layout {
        let leaf = |id: ModuleId| Child::new(Node::Leaf(id), 0.5);
        let tabs = Con::new(
            ConLayout::Tabbed,
            vec![leaf(ModuleId::lua("a")), leaf(ModuleId::lua("b"))],
        );
        let root = Con::new(
            ConLayout::SplitH,
            vec![leaf(ModuleId::editor()), Child::new(Node::Con(tabs), 0.5)],
        );
        let mut layout = Layout::new(Node::Con(root), ModuleId::lua("a")).unwrap();
        layout.set_focus(&ModuleId::editor()).unwrap();
        layout
    }

    #[test]
    fn a_tab_switch_maps_and_focuses_before_it_unmaps() {
        let (a, b) = (ModuleId::lua("a"), ModuleId::lua("b"));
        let mut layout = editor_beside_tabs();
        assert!(
            layout.is_visible(&a) && !layout.is_visible(&b),
            "the premise: a is the drawn tab"
        );
        let before: Vec<ModuleId> = layout.visible_modules();
        let next = tab_switch(&mut layout, &b).unwrap();
        assert_eq!(next.as_ref(), Some(&b), "the arriving tab gets the keys");
        assert_eq!(layout.focus(), &b);
        // What each step names on screen: the unmap takes what the switch stopped drawing.
        let after = layout.visible_modules();
        let gone: Vec<ModuleId> = before.into_iter().filter(|m| !after.contains(m)).collect();
        let named: Vec<String> = leave_order(next.as_ref())
            .into_iter()
            .map(|step| match step {
                Step::Map(m) => format!("map {m}"),
                Step::Focus(m) => format!("focus {m}"),
                Step::Unmap => format!(
                    "unmap {}",
                    gone.iter().map(ToString::to_string).collect::<Vec<_>>().join(",")
                ),
                Step::Restack => "restack".into(),
            })
            .collect();
        assert_eq!(
            named,
            vec![format!("map {b}"), format!("focus {b}"), format!("unmap {a}")]
        );
    }

    #[test]
    fn with_no_one_to_take_the_keys_only_the_unmap_runs() {
        assert_eq!(leave_order(None), vec![Step::Unmap]);
    }

    #[test]
    fn a_refused_tab_switch_changes_nothing() {
        let mut layout = editor_beside_tabs();
        let before = layout.clone();
        let missing = ModuleId::lua("missing");
        assert_eq!(tab_switch(&mut layout, &missing), Err(LayoutError::NotInTree(missing)));
        assert_eq!(layout, before);
    }
}
