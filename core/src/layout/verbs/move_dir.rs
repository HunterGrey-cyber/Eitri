use super::select::path_to_con;
use crate::layout::tree::MIN_RATIO;
use crate::layout::{
    Axis, Child, Con, ConLayout, Direction, Layout, LayoutError, ModuleId, Node, Selection, MAX_CHILDREN,
};

fn insert(c: &mut Con, at: usize, mut unit: Child, layout: &Layout) -> Result<(), LayoutError> {
    if c.children.len() >= MAX_CHILDREN {
        return Err(LayoutError::TooMany { limit: MAX_CHILDREN });
    }
    let budget = 1.0 - unit.share;
    for child in &mut c.children {
        child.share *= budget;
    }
    if at != c.children.len() || !matches!(c.layout, ConLayout::SplitV | ConLayout::Stacked) {
        unit.pin = None;
    }
    c.children.insert(at, unit);
    if c.children.iter().any(|ch| ch.share < MIN_RATIO) {
        let share = 1.0 / c.children.len() as f32;
        for child in &mut c.children {
            child.share = share;
            if child.node.leaves().iter().any(|m| layout.is_shown(m)) {
                child.pin = None;
            }
        }
    }
    Ok(())
}

fn take(root: &mut Con, parent: crate::layout::ConId, index: usize) -> Child {
    let c = root.find_mut(parent).unwrap();
    let unit = c.children.remove(index);
    let sum: f32 = c.children.iter().map(|ch| ch.share).sum();
    for child in &mut c.children {
        child.share /= sum;
    }
    c.focus = c.focus.min(c.children.len().saturating_sub(1));
    unit
}

pub fn move_dir(layout: &mut Layout, target: &ModuleId, dir: Direction) -> Result<bool, LayoutError> {
    layout.can_focus(target)?;
    if layout.leaves().iter().filter(|m| layout.is_shown(m)).count() < 2 {
        return Err(LayoutError::LastVisible(target.clone()));
    }
    let path = match layout.selection() {
        Selection::Module => super::path_to(layout.root(), target),
        Selection::Con(id) => path_to_con(layout.root(), id).unwrap_or_default(),
    };
    let Some(&(parent, index)) = path.last() else {
        return Ok(false);
    };
    let horizontal = matches!(dir, Direction::Left | Direction::Right);
    let before = matches!(dir, Direction::Left | Direction::Up);
    let orientation = |c: &Con| matches!(c.layout, ConLayout::SplitH | ConLayout::Tabbed) == horizontal;
    let p = layout.root().find(parent).unwrap();
    let sibling = if orientation(p) {
        let candidates: Vec<_> = if before {
            (0..index).rev().collect()
        } else {
            (index + 1..p.children.len()).collect()
        };
        candidates
            .into_iter()
            .find(|&i| p.children[i].node.leaves().iter().any(|m| layout.is_shown(m)))
    } else {
        None
    };
    let mut root = layout.root().clone();
    if let Some(sibling) = sibling {
        match &p.children[sibling].node {
            Node::Leaf(_) => {
                let c = root.find_mut(parent).unwrap();
                let node = c.children[index].node.clone();
                c.children[index].node = c.children[sibling].node.clone();
                c.children[sibling].node = node;
            }
            Node::Con(destination) => {
                let dest_id = destination.id;
                let drawn = destination.drawn(&|m| layout.is_shown(m)).unwrap_or(destination.focus);
                let at = drawn + usize::from(before);
                let unit = take(&mut root, parent, index);
                insert(root.find_mut(dest_id).unwrap(), at, unit, layout)?;
                root.normalise();
            }
        }
    } else if let Some(&(ancestor, child)) = path[..path.len() - 1]
        .iter()
        .rev()
        .find(|(id, _)| orientation(layout.root().find(*id).unwrap()))
    {
        let unit = take(&mut root, parent, index);
        insert(
            root.find_mut(ancestor).unwrap(),
            child + usize::from(!before),
            unit,
            layout,
        )?;
        root.normalise();
    } else {
        if orientation(layout.root()) {
            return Ok(false);
        }
        let unit = take(&mut root, parent, index);
        root.normalise();
        let old_mode = root.layout;
        let mut rest = Con::new(old_mode, std::mem::take(&mut root.children));
        rest.last_split = root.last_split;
        rest.focus = root.focus;
        let rest_node = if rest.children.len() == 1 && rest.layout.axis().is_some() {
            rest.children.pop().unwrap().node
        } else {
            Node::Con(rest)
        };
        root.layout = if horizontal {
            ConLayout::SplitH
        } else {
            ConLayout::SplitV
        };
        root.last_split = if horizontal { Axis::Row } else { Axis::Column };
        root.children = vec![Child::new(rest_node, 1.0)];
        insert(&mut root, usize::from(!before), unit, layout)?;
    }
    let focus = layout.focus().clone();
    super::apply(layout, Node::Con(root));
    layout.set_focus(&focus)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::*;
    fn id(s: &str) -> ModuleId {
        ModuleId::lua(s)
    }
    fn leaf(s: &str) -> Node {
        Node::Leaf(id(s))
    }
    fn row(a: Node, b: Node) -> Node {
        Node::split(Axis::Row, 0.5, a, b)
    }
    fn column(a: Node, b: Node) -> Node {
        Node::split(Axis::Column, 0.5, a, b)
    }
    fn shares(l: &Layout, expected: &[f32]) {
        let actual: Vec<_> = l.root().children.iter().map(|ch| ch.share).collect();
        assert_eq!(actual.len(), expected.len());
        for (a, b) in actual.iter().zip(expected) {
            assert!((a - b).abs() < 1e-6, "{actual:?}");
        }
        l.root().check(true).unwrap();
    }
    fn rects(l: &Layout, expected: &[(&str, Rect)]) {
        let a = arrange(l, &Frame::new(Size { w: 1280, h: 721 }, 1));
        for (name, r) in expected {
            assert_eq!(a.rect_of(&id(name)), Some(*r));
        }
        assert_eq!(l.focus(), &id("a"));
    }
    fn width(l: &Layout, name: &str) -> i32 {
        arrange(l, &Frame::new(Size { w: 1280, h: 721 }, 1))
            .rect_of(&id(name))
            .unwrap()
            .w
    }
    /// Beside a leaf sibling `move_dir` exchanges the two nodes: each slot keeps its share and its fixed length, as a
    /// swap does, so the module moved into the rail's slot is drawn at the rail's width.
    #[test]
    fn moving_beside_a_leaf_leaves_a_fixed_width_with_its_slot() {
        let mut root = Con::new(
            ConLayout::SplitH,
            vec![
                Child::new(leaf("a"), 0.05),
                Child::new(leaf("b"), 0.5),
                Child::new(leaf("c"), 0.45),
            ],
        );
        root.children[0].fixed_w_px = Some(62);
        let mut l = Layout::new(Node::Con(root), id("a")).unwrap();
        assert_eq!(width(&l, "a"), 62);
        assert_eq!(move_dir(&mut l, &id("a"), Direction::Right), Ok(true));
        assert_eq!(l.leaves(), vec![id("b"), id("a"), id("c")]);
        assert_eq!(width(&l, "b"), 62);
        assert_eq!(l.root().children[0].fixed_w_px, Some(62));
        assert_eq!(l.root().children[1].fixed_w_px, None);
    }
    /// Taken out of its container, the whole child goes, its length with it: into a column, where a width does not
    /// apply, and back into a row, where it does again.
    #[test]
    fn moving_into_a_container_carries_the_fixed_width_which_applies_along_a_row_only() {
        let mut root = Con::new(
            ConLayout::SplitH,
            vec![
                Child::new(leaf("a"), 0.05),
                Child::new(column(leaf("b"), leaf("c")), 0.95),
            ],
        );
        root.children[0].fixed_w_px = Some(62);
        let mut l = Layout::new(Node::Con(root), id("a")).unwrap();
        assert_eq!(move_dir(&mut l, &id("a"), Direction::Right), Ok(true));
        fn slot_of(c: &Con, a: &Node) -> Option<(ConLayout, Option<i32>)> {
            c.children.iter().find_map(|ch| match &ch.node {
                n if n == a => Some((c.layout, ch.fixed_w_px)),
                Node::Con(inner) => slot_of(inner, a),
                _ => None,
            })
        }
        assert_eq!(
            slot_of(l.root(), &leaf("a")),
            Some((ConLayout::SplitV, Some(62))),
            "{:?}",
            l.root()
        );
        assert!(width(&l, "a") > 62, "a width in a column draws nothing");
        assert_eq!(move_dir(&mut l, &id("a"), Direction::Left), Ok(true));
        assert_eq!(width(&l, "a"), 62, "back in a row");
    }
    #[test]
    fn moving_toward_a_container_goes_into_it_beside_its_focused_child() {
        let mut inner = column(leaf("b"), leaf("c"));
        if let Node::Con(c) = &mut inner {
            c.focus = 1;
        }
        let mut l = Layout::new(row(leaf("a"), inner), id("a")).unwrap();
        assert_eq!(move_dir(&mut l, &id("a"), Direction::Right), Ok(true));
        shares(&l, &[0.25, 0.5, 0.25]);
        assert_eq!(l.leaves(), vec![id("b"), id("a"), id("c")]);
        rects(
            &l,
            &[
                (
                    "b",
                    Rect {
                        x: 0,
                        y: 0,
                        w: 1280,
                        h: 180,
                    },
                ),
                (
                    "a",
                    Rect {
                        x: 0,
                        y: 181,
                        w: 1280,
                        h: 359,
                    },
                ),
                (
                    "c",
                    Rect {
                        x: 0,
                        y: 541,
                        w: 1280,
                        h: 180,
                    },
                ),
            ],
        );
    }
    #[test]
    fn moving_across_the_orientation_leaves_for_the_nearest_ancestor_that_has_it() {
        let mut l = Layout::new(
            Node::split(Axis::Row, 0.6, column(leaf("a"), leaf("b")), leaf("c")),
            id("a"),
        )
        .unwrap();
        assert_eq!(move_dir(&mut l, &id("a"), Direction::Right), Ok(true));
        shares(&l, &[0.3, 0.5, 0.2]);
        rects(
            &l,
            &[
                (
                    "b",
                    Rect {
                        x: 0,
                        y: 0,
                        w: 384,
                        h: 721,
                    },
                ),
                (
                    "a",
                    Rect {
                        x: 385,
                        y: 0,
                        w: 639,
                        h: 721,
                    },
                ),
                (
                    "c",
                    Rect {
                        x: 1025,
                        y: 0,
                        w: 255,
                        h: 721,
                    },
                ),
            ],
        );
    }
    #[test]
    fn moving_off_the_root_reorients_it() {
        let mut l = Layout::new(row(leaf("a"), leaf("b")), id("a")).unwrap();
        let root = l.root().id;
        assert_eq!(move_dir(&mut l, &id("a"), Direction::Up), Ok(true));
        assert_eq!(l.root().id, root);
        assert_eq!(l.root().layout, ConLayout::SplitV);
        shares(&l, &[0.5, 0.5]);
        rects(
            &l,
            &[
                (
                    "a",
                    Rect {
                        x: 0,
                        y: 0,
                        w: 1280,
                        h: 360,
                    },
                ),
                (
                    "b",
                    Rect {
                        x: 0,
                        y: 361,
                        w: 1280,
                        h: 360,
                    },
                ),
            ],
        );
    }
    #[test]
    fn a_hidden_sibling_is_stepped_over() {
        let root = Node::Con(Con::new(
            ConLayout::SplitH,
            vec![
                Child::new(leaf("a"), 0.3),
                Child::new(leaf("b"), 0.4),
                Child::new(leaf("c"), 0.3),
            ],
        ));
        let mut l = Layout::from_parts(root, std::collections::BTreeSet::from([id("b")]), id("a")).unwrap();
        assert_eq!(move_dir(&mut l, &id("a"), Direction::Right), Ok(true));
        assert_eq!(l.leaves(), vec![id("c"), id("b"), id("a")]);
        shares(&l, &[0.3, 0.4, 0.3]);
        rects(
            &l,
            &[
                (
                    "c",
                    Rect {
                        x: 0,
                        y: 0,
                        w: 640,
                        h: 721,
                    },
                ),
                (
                    "a",
                    Rect {
                        x: 641,
                        y: 0,
                        w: 639,
                        h: 721,
                    },
                ),
            ],
        );
    }
    #[test]
    fn at_the_edge_of_a_root_of_that_orientation_nothing_moves() {
        let mut l = Layout::new(row(leaf("a"), leaf("b")), id("a")).unwrap();
        l.toggle_zoom(&id("a"));
        let before = l.clone();
        assert_eq!(move_dir(&mut l, &id("a"), Direction::Left), Ok(false));
        assert_eq!(l, before);
    }
    #[test]
    fn moving_into_a_tabbed_container_makes_the_module_its_drawn_tab() {
        let inner = Node::Con(Con::new(
            ConLayout::Tabbed,
            vec![Child::new(leaf("b"), 0.5), Child::new(leaf("c"), 0.5)],
        ));
        let mut l = Layout::new(row(leaf("a"), inner), id("a")).unwrap();
        assert_eq!(move_dir(&mut l, &id("a"), Direction::Right), Ok(true));
        assert_eq!(l.root().layout, ConLayout::Tabbed);
        assert_eq!(l.leaves(), vec![id("a"), id("b"), id("c")]);
        shares(&l, &[0.5, 0.25, 0.25]);
        assert_eq!(l.visible_leaves(), vec![id("a")]);
        rects(
            &l,
            &[(
                "a",
                Rect {
                    x: 0,
                    y: 24,
                    w: 1280,
                    h: 697,
                },
            )],
        );
    }
    #[test]
    fn the_selected_container_moves_as_one_unit() {
        let mut l = Layout::new(row(leaf("b"), column(leaf("a"), leaf("c"))), id("a")).unwrap();
        focus_parent(&mut l);
        let selection = l.selection();
        assert_eq!(move_dir(&mut l, &id("a"), Direction::Left), Ok(true));
        assert_eq!(l.selection(), selection);
        shares(&l, &[0.5, 0.5]);
        rects(
            &l,
            &[
                (
                    "a",
                    Rect {
                        x: 0,
                        y: 0,
                        w: 640,
                        h: 360,
                    },
                ),
                (
                    "c",
                    Rect {
                        x: 0,
                        y: 361,
                        w: 640,
                        h: 360,
                    },
                ),
                (
                    "b",
                    Rect {
                        x: 641,
                        y: 0,
                        w: 639,
                        h: 721,
                    },
                ),
            ],
        );
    }

    #[test]
    fn a_full_destination_refuses_without_taking_the_unit_out() {
        let children = (0..20).map(|i| Child::new(leaf(&format!("m{i}")), 0.05)).collect();
        let mut l = Layout::new(
            row(leaf("a"), Node::Con(Con::new(ConLayout::SplitV, children))),
            id("a"),
        )
        .unwrap();
        l.toggle_zoom(&id("a"));
        let before = l.clone();
        assert_eq!(
            move_dir(&mut l, &id("a"), Direction::Right),
            Err(LayoutError::TooMany { limit: 20 })
        );
        assert_eq!(l, before);
    }
    #[test]
    fn inserting_a_large_unit_evens_small_shares_and_clears_shown_pins() {
        let mut inner = Con::new(
            ConLayout::SplitV,
            vec![Child::new(leaf("b"), 0.05), Child::new(leaf("c"), 0.95)],
        );
        inner.children[0].pin = Some(Pin { px: Some(100) });
        let mut l = Layout::new(row(leaf("a"), Node::Con(inner)), id("a")).unwrap();
        assert_eq!(move_dir(&mut l, &id("a"), Direction::Right), Ok(true));
        shares(&l, &[1.0 / 3.0; 3]);
        assert!(l.root().children.iter().all(|ch| ch.pin.is_none()));
        rects(
            &l,
            &[
                (
                    "a",
                    Rect {
                        x: 0,
                        y: 0,
                        w: 1280,
                        h: 240,
                    },
                ),
                (
                    "b",
                    Rect {
                        x: 0,
                        y: 241,
                        w: 1280,
                        h: 240,
                    },
                ),
                (
                    "c",
                    Rect {
                        x: 0,
                        y: 482,
                        w: 1280,
                        h: 239,
                    },
                ),
            ],
        );
    }
    #[test]
    fn moving_from_the_right_inserts_after_the_focused_child() {
        let mut l = Layout::new(row(column(leaf("b"), leaf("c")), leaf("a")), id("a")).unwrap();
        assert_eq!(move_dir(&mut l, &id("a"), Direction::Left), Ok(true));
        assert_eq!(l.leaves(), vec![id("b"), id("a"), id("c")]);
        shares(&l, &[0.25, 0.5, 0.25]);
        rects(
            &l,
            &[(
                "a",
                Rect {
                    x: 0,
                    y: 181,
                    w: 1280,
                    h: 359,
                },
            )],
        );
    }
    #[test]
    fn a_pin_travels_only_to_the_bottom_slot_of_a_vertical_destination() {
        for dir in [Direction::Left, Direction::Right] {
            let mut inner = Con::new(
                ConLayout::Stacked,
                vec![Child::new(leaf("b"), 0.5), Child::new(leaf("c"), 0.5)],
            );
            inner.focus = 1;
            let children = if dir == Direction::Left {
                vec![Child::new(Node::Con(inner), 0.5), Child::new(leaf("a"), 0.5)]
            } else {
                vec![Child::new(leaf("a"), 0.5), Child::new(Node::Con(inner), 0.5)]
            };
            let mut root = Con::new(ConLayout::SplitH, children);
            let at = if dir == Direction::Left { 1 } else { 0 };
            root.children[at].pin = Some(Pin { px: Some(90) });
            let mut l = Layout::new(Node::Con(root), id("a")).unwrap();
            assert_eq!(move_dir(&mut l, &id("a"), dir), Ok(true));
            let unit = l
                .root()
                .children
                .iter()
                .find(|ch| ch.node.leaves() == vec![id("a")])
                .unwrap();
            assert_eq!(
                unit.pin,
                if dir == Direction::Left {
                    Some(Pin { px: Some(90) })
                } else {
                    None
                }
            );
        }
    }
    #[test]
    fn hidden_targets_and_the_last_shown_module_are_refused() {
        let mut l = Layout::new(row(leaf("a"), leaf("b")), id("a")).unwrap();
        l.hide_unfocused(&id("b")).unwrap();
        let before = l.clone();
        assert_eq!(
            move_dir(&mut l, &id("b"), Direction::Left),
            Err(LayoutError::Hidden(id("b")))
        );
        assert_eq!(
            move_dir(&mut l, &id("a"), Direction::Right),
            Err(LayoutError::LastVisible(id("a")))
        );
        assert_eq!(l, before);
    }
}
