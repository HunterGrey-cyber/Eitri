use crate::layout::{Con, ConId, Layout, Selection};

pub(super) fn path_to_con(root: &Con, id: ConId) -> Option<Vec<(ConId, usize)>> {
    if root.id == id {
        return Some(Vec::new());
    }
    for (i, child) in root.children.iter().enumerate() {
        if let Some(inner) = child.node.as_con() {
            if let Some(mut path) = path_to_con(inner, id) {
                path.insert(0, (root.id, i));
                return Some(path);
            }
        }
    }
    None
}

pub fn focus_parent(layout: &mut Layout) -> bool {
    let next = match layout.selection() {
        Selection::Module => layout.container_of(layout.focus()),
        Selection::Con(id) => path_to_con(layout.root(), id).and_then(|path| {
            path.into_iter().rev().find_map(|(parent, i)| {
                let c = layout.root().find(parent).unwrap();
                c.children
                    .iter()
                    .enumerate()
                    .any(|(j, ch)| j != i && ch.node.leaves().iter().any(|m| layout.is_shown(m)))
                    .then_some(parent)
            })
        }),
    };
    let Some(id) = next else {
        return false;
    };
    layout.select(Selection::Con(id));
    true
}

pub fn focus_child(layout: &mut Layout) -> bool {
    let Selection::Con(id) = layout.selection() else {
        return false;
    };
    let next = layout
        .ancestors(layout.focus())
        .into_iter()
        .skip_while(|(c, _)| c.id != id)
        .skip(1)
        .find_map(|(c, i)| {
            c.children
                .iter()
                .enumerate()
                .any(|(j, ch)| j != i && ch.node.leaves().iter().any(|m| layout.is_shown(m)))
                .then_some(c.id)
        });
    layout.select(next.map(Selection::Con).unwrap_or(Selection::Module));
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::*;

    fn sample() -> Layout {
        Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::split(
                    Axis::Column,
                    0.5,
                    Node::Leaf(ModuleId::agent()),
                    Node::Leaf(ModuleId::lua("a")),
                ),
            ),
            ModuleId::agent(),
        )
        .unwrap()
    }

    #[test]
    fn focus_parent_selects_upward_and_focus_child_narrows_back() {
        let mut l = sample();
        let inner = l.container_of(l.focus()).unwrap();
        assert_eq!(l.selection(), Selection::Module);
        assert!(focus_parent(&mut l));
        assert_eq!(l.selection(), Selection::Con(inner));
        assert!(focus_parent(&mut l));
        assert_eq!(l.selection(), Selection::Con(l.root().id));
        assert!(!focus_parent(&mut l));
        assert!(focus_child(&mut l));
        assert_eq!(l.selection(), Selection::Con(inner));
        assert!(focus_child(&mut l));
        assert_eq!(l.selection(), Selection::Module);
        assert!(!focus_child(&mut l));
        focus_parent(&mut l);
        l.set_focus(&ModuleId::editor()).unwrap();
        assert_eq!(l.selection(), Selection::Module);
    }

    #[test]
    fn a_selection_survives_a_kill_elsewhere() {
        let mut l = sample();
        focus_parent(&mut l);
        let before = l.selection();
        kill(
            &mut l,
            &ModuleId::editor(),
            Reopen::At(Placement::RightOfRoot),
            &Frame::new(Size { w: 1280, h: 721 }, 1),
        )
        .unwrap();
        assert_eq!(l.selection(), before);
    }

    #[test]
    fn a_selection_whose_container_disappears_goes_back_to_the_module() {
        let mut l = sample();
        focus_parent(&mut l);
        kill(
            &mut l,
            &ModuleId::lua("a"),
            Reopen::At(Placement::BelowRoot),
            &Frame::new(Size { w: 1280, h: 721 }, 1),
        )
        .unwrap();
        assert_eq!(l.selection(), Selection::Module);
    }

    #[test]
    fn zooming_the_selected_container_shows_it_strip_and_all() {
        let mut l = sample();
        container_mode(&mut l, &ModuleId::agent(), ConLayout::Tabbed).unwrap();
        focus_parent(&mut l);
        let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
        let before = arrange(&l, &frame);
        assert_eq!(l.toggle_zoom(&ModuleId::agent()), ZoomChange::Zoomed);
        assert!(l.is_zoomed());
        assert_eq!(l.zoomed(), None);
        assert!(l.zoom_hides(&ModuleId::editor()));
        assert!(!l.is_visible(&ModuleId::editor()));
        let a = arrange(&l, &frame);
        assert_eq!(
            a.strips[0].rect,
            Rect {
                x: 0,
                y: 0,
                w: 1280,
                h: 24
            }
        );
        assert_eq!(
            a.rect_of(&ModuleId::agent()),
            Some(Rect {
                x: 0,
                y: 24,
                w: 1280,
                h: 697
            })
        );
        assert_eq!(number_plan(&l), vec![('1', ModuleId::agent())]);
        let min = |_: &ModuleId| Size { w: 100, h: 80 };
        let measured = Frame {
            min: &min,
            ..Frame::new(Size { w: 1280, h: 721 }, 1)
        };
        assert_eq!(min_size(&l, &measured), Size { w: 100, h: 104 });
        assert_eq!(l.toggle_zoom(&ModuleId::agent()), ZoomChange::Unzoomed);
        assert_eq!(arrange(&l, &frame), before);
    }

    #[test]
    fn focus_parent_and_child_skip_containers_with_only_one_shown_child() {
        let mut l = Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::split(
                    Axis::Column,
                    0.5,
                    Node::Leaf(ModuleId::agent()),
                    Node::Leaf(ModuleId::terminal()),
                ),
            ),
            ModuleId::agent(),
        )
        .unwrap();
        l.hide_unfocused(&ModuleId::terminal()).unwrap();
        assert!(focus_parent(&mut l));
        assert_eq!(l.selection(), Selection::Con(l.root().id));
        assert!(!focus_parent(&mut l));
        assert!(focus_child(&mut l));
        assert_eq!(l.selection(), Selection::Module);
        assert!(!l.clear_selection());
        focus_parent(&mut l);
        assert!(l.clear_selection());
    }

    #[test]
    fn a_container_zoom_numbers_each_visible_leaf_and_the_root_cannot_zoom() {
        let mut l = sample();
        focus_parent(&mut l);
        let selection = l.selection();
        l.set_focus(&ModuleId::agent()).unwrap();
        assert_eq!(l.selection(), selection);
        assert_eq!(l.toggle_zoom(&ModuleId::agent()), ZoomChange::Zoomed);
        assert_eq!(
            number_plan(&l),
            vec![('1', ModuleId::agent()), ('2', ModuleId::lua("a"))]
        );
        assert!(l.is_visible(&ModuleId::lua("a")));
        assert!(!l.zoom_hides(&ModuleId::lua("a")));
        l.unzoom();
        focus_parent(&mut l);
        assert_eq!(l.selection(), Selection::Con(l.root().id));
        assert_eq!(l.toggle_zoom(&ModuleId::agent()), ZoomChange::Nothing);
        assert!(!l.is_zoomed());
    }
}
