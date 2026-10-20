//! Container presentation changes preserve shares and pins for the return to a split.

use crate::layout::{ConLayout, Layout, LayoutError, ModuleId, Node};

pub fn container_mode(layout: &mut Layout, target: &ModuleId, mode: ConLayout) -> Result<bool, LayoutError> {
    if mode.axis().is_some() {
        debug_assert!(false, "container mode must be tabbed or stacked");
        return Ok(false);
    }
    layout.can_focus(target)?;
    let Some(id) = layout.selected_container(target) else {
        return Ok(false);
    };
    let mut root = layout.root().clone();
    let c = root.find_mut(id).expect("the container belongs to this tree");
    if let Some(axis) = c.layout.axis() {
        c.last_split = axis;
    }
    c.layout = if c.layout == mode {
        ConLayout::of_axis(c.last_split)
    } else {
        mode
    };
    c.focus_leaf(target);
    root.normalise();
    layout.unzoom();
    layout.replace_root(Node::Con(root));
    Ok(true)
}

pub fn switch_tab(layout: &mut Layout, to: &ModuleId) -> Result<ModuleId, LayoutError> {
    layout.can_focus(to)?;
    layout.unzoom();
    // Choosing a tab is navigation: `set_focus` brings the tab forward in every ancestor without
    // taking the undo slot, which belongs to the last change of the layout itself.
    layout.set_focus(to)?;
    Ok(to.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::*;

    fn id(s: &str) -> ModuleId {
        ModuleId::parse(s).unwrap()
    }
    fn leaf(s: &str) -> Node {
        Node::Leaf(id(s))
    }
    fn con(mode: ConLayout, children: Vec<Node>) -> Node {
        let share = 1.0 / children.len() as f32;
        Node::Con(Con::new(
            mode,
            children.into_iter().map(|n| Child::new(n, share)).collect(),
        ))
    }
    fn frame() -> Frame<'static> {
        Frame::new(Size { w: 1280, h: 721 }, 1)
    }
    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }
    fn beside(mode: ConLayout) -> Layout {
        Layout::new(
            con(
                ConLayout::SplitH,
                vec![leaf("editor"), con(mode, vec![leaf("lua:a"), leaf("lua:b")])],
            ),
            id("editor"),
        )
        .unwrap()
    }

    #[test]
    fn container_equality_only_tracks_presentation_focus_for_tabs() {
        let mut a = Con::new(
            ConLayout::SplitH,
            vec![Child::new(leaf("editor"), 0.5), Child::new(leaf("agent"), 0.5)],
        );
        let mut b = a.clone();
        b.focus = 1;
        b.last_split = Axis::Column;
        assert_eq!(a, b);
        a.layout = ConLayout::Tabbed;
        b.layout = ConLayout::Tabbed;
        assert_ne!(a, b);
        b.focus = a.focus;
        assert_ne!(a, b);
        b.last_split = a.last_split;
        assert_eq!(a, b);
        b.focus = b.children.len();
        assert_eq!(b.check(true), Err(crate::layout::con::ConFault::Focus(2)));
        assert_eq!(Layout::new(Node::Con(b), id("editor")), Err(LayoutError::Unnormalised));
    }

    #[test]
    fn removing_an_earlier_tab_keeps_the_focused_child_and_container_id() {
        let mut c = Con::new(
            ConLayout::Tabbed,
            vec![
                Child::new(leaf("editor"), 1.0 / 3.0),
                Child::new(leaf("lua:a"), 1.0 / 3.0),
                Child::new(leaf("lua:b"), 1.0 / 3.0),
            ],
        );
        c.focus = 1;
        let con_id = c.id;
        let Node::Con(c) = Node::Con(c).without(&id("editor")).unwrap() else {
            panic!("tabs keep their container");
        };
        assert_eq!(c.id, con_id);
        assert_eq!(c.focused_leaf(&|_| true), Some(id("lua:a")));
        assert_eq!(c.check(true), Ok(()));
    }

    #[test]
    fn switching_focus_out_of_a_zoomed_tab_keeps_something_visible() {
        let mut l = beside(ConLayout::Tabbed);
        l.set_focus(&id("lua:a")).unwrap();
        l.toggle_zoom(&id("lua:a"));
        l.set_focus(&id("lua:b")).unwrap();
        assert_eq!(l.zoomed(), None);
        assert!(l.is_visible(&id("lua:b")));
    }

    #[test]
    fn a_tabbed_container_draws_its_strip_and_only_its_focused_child() {
        let tabs = con(ConLayout::Tabbed, vec![leaf("lua:a"), leaf("lua:b")]);
        let root = Con::new(
            ConLayout::SplitV,
            vec![Child::new(leaf("editor"), 2.0 / 3.0), Child::new(tabs, 1.0 / 3.0)],
        );
        let mut l = Layout::new(Node::Con(root), id("editor")).unwrap();
        assert!(l.bring_forward(&id("lua:b")));
        let a = arrange(&l, &frame());
        assert_eq!(a.rect_of(&id("editor")), Some(rect(0, 0, 1280, 480)));
        assert_eq!(a.strips[0].rect, rect(0, 481, 1280, 24));
        assert_eq!(
            a.strips[0].entries,
            vec![
                ConStripEntry {
                    id: id("lua:a"),
                    active: false
                },
                ConStripEntry {
                    id: id("lua:b"),
                    active: true
                }
            ]
        );
        assert_eq!(a.rect_of(&id("lua:b")), Some(rect(0, 505, 1280, 216)));
        assert_eq!(a.rect_of(&id("lua:a")), None);
        assert_eq!(a.cell_of(&id("lua:b")), Some(rect(0, 481, 1280, 240)));
    }
    #[test]
    fn a_stacked_container_draws_one_title_row_per_shown_child() {
        let mut l = Layout::new(
            con(ConLayout::Stacked, vec![leaf("lua:a"), leaf("lua:b"), leaf("lua:c")]),
            id("lua:a"),
        )
        .unwrap();
        let a = arrange(&l, &frame());
        assert_eq!(a.strips[0].rect, rect(0, 0, 1280, 72));
        assert_eq!(a.rect_of(&id("lua:a")), Some(rect(0, 72, 1280, 649)));
        hide(&mut l, &id("lua:b"), &frame()).unwrap();
        let a = arrange(&l, &frame());
        assert_eq!(a.strips[0].rect.h, 48);
        assert_eq!(
            a.strips[0]
                .entries
                .iter()
                .map(|e| (&e.id, e.active))
                .collect::<Vec<_>>(),
            vec![(&id("lua:a"), true), (&id("lua:c"), false)]
        );
    }
    #[test]
    fn an_inactive_tab_is_shown_but_not_visible_nor_in_view() {
        let mut l = beside(ConLayout::Tabbed);
        l.set_focus(&id("lua:b")).unwrap();
        assert!(l.is_shown(&id("lua:a")));
        assert!(!l.is_visible(&id("lua:a")));
        assert!(l.is_tabbed_away(&id("lua:a")));
        assert!(!l.is_in_view(&id("lua:a")));
        assert!(!l.visible_leaves().contains(&id("lua:a")));
    }
    #[test]
    fn rotate_select_next_and_swap_next_skip_an_inactive_tab() {
        let mut l = Layout::new(
            con(
                ConLayout::SplitH,
                vec![
                    leaf("editor"),
                    leaf("lua:c"),
                    con(ConLayout::Tabbed, vec![leaf("lua:a"), leaf("lua:b")]),
                ],
            ),
            id("editor"),
        )
        .unwrap();
        assert_eq!(next_on_screen(&l, &id("lua:c")), Some(id("lua:a")));
        assert_eq!(next_on_screen(&l, &id("lua:a")), Some(id("editor")));
        let original = l.clone();
        rotate(&mut l, true);
        assert_eq!(l.leaves()[3], id("lua:b"));
        l = original;
        assert_eq!(swap_adjacent(&mut l, &id("lua:c"), true), Some(id("lua:a")));
        assert_eq!(l.leaves()[3], id("lua:b"));
    }
    #[test]
    fn giving_the_keys_to_an_inactive_tab_brings_it_forward() {
        let mut l = beside(ConLayout::Tabbed);
        l.set_focus(&id("lua:b")).unwrap();
        assert!(l.is_visible(&id("lua:b")));
        assert!(!l.is_visible(&id("lua:a")));
    }
    #[test]
    fn undo_never_leaves_the_keys_on_an_inactive_tab() {
        let mut l = beside(ConLayout::Tabbed);
        l.set_focus(&id("lua:b")).unwrap();
        container_mode(&mut l, &id("lua:b"), ConLayout::Stacked).unwrap();
        l.set_focus(&id("lua:a")).unwrap();
        l.undo().unwrap();
        assert!(l.is_in_view(l.focus()));
        assert_eq!(l.focus(), &id("lua:a"));
    }
    #[test]
    fn bring_forward_never_hides_the_module_with_the_keys() {
        let mut l = beside(ConLayout::Tabbed);
        l.set_focus(&id("lua:b")).unwrap();
        let before = l.clone();
        assert!(!l.bring_forward(&id("lua:a")));
        assert_eq!(l, before);
        l.set_focus(&id("editor")).unwrap();
        assert!(l.bring_forward(&id("lua:a")));
        assert!(l.is_in_view(&id("lua:a")));
        assert_eq!(l.focus(), &id("editor"));
        assert!(!l.bring_forward(&id("lua:missing")));
        hide(&mut l, &id("lua:a"), &frame()).unwrap();
        let before = l.clone();
        assert!(!l.bring_forward(&id("lua:a")));
        assert_eq!(l, before);
    }
    #[test]
    fn container_tabbed_and_stacked_switch_the_targets_container_and_back() {
        let mut l = crate::layout::fixture::fixture("row(editor 0.6, agent)");
        let before = l.root().clone();
        container_mode(&mut l, &id("editor"), ConLayout::Tabbed).unwrap();
        assert_eq!(l.root().layout, ConLayout::Tabbed);
        assert_eq!(l.root().last_split, Axis::Row);
        assert!(l.is_tabbed_away(&id("agent")));
        container_mode(&mut l, &id("editor"), ConLayout::Tabbed).unwrap();
        assert_eq!(l.root(), &before);
        assert_eq!(l.root().id, before.id);
        container_mode(&mut l, &id("editor"), ConLayout::Stacked).unwrap();
        container_mode(&mut l, &id("editor"), ConLayout::Tabbed).unwrap();
        assert_eq!(l.root().layout, ConLayout::Tabbed);
        let mut l = crate::layout::fixture::fixture(&format!(
            "row(column(editor {b}, terminal hidden pinned) {e}, agent)",
            b = crate::layout::tree::BELOW_ROOT_SHARE,
            e = crate::layout::tree::DEFAULT_EDITOR_SHARE
        ));
        container_mode(&mut l, &id("editor"), ConLayout::Tabbed).unwrap();
        assert_eq!(l.root().layout, ConLayout::Tabbed);
        assert_eq!(
            arrange(&l, &frame()).strips[0]
                .entries
                .iter()
                .map(|e| e.id.clone())
                .collect::<Vec<_>>(),
            vec![id("editor"), id("agent")]
        );
    }
    #[test]
    fn toggle_split_flips_a_split_and_returns_a_tabbed_container_to_its_split() {
        let mut l = crate::layout::fixture::fixture("column(editor 0.6, terminal pinned)");
        assert!(toggle_split(&mut l, &id("editor")));
        assert_eq!(l.root().layout, ConLayout::SplitH);
        assert!(l.root().children.iter().all(|c| c.pin.is_none()));
        container_mode(&mut l, &id("editor"), ConLayout::Tabbed).unwrap();
        assert!(toggle_split(&mut l, &id("editor")));
        assert_eq!(l.root().layout, ConLayout::SplitH);
    }
    #[test]
    fn a_tabbed_container_with_one_child_is_kept_and_an_empty_one_goes() {
        let mut c = Con::new(
            ConLayout::SplitH,
            vec![
                Child::new(leaf("editor"), 0.5),
                Child::new(con(ConLayout::Tabbed, vec![leaf("lua:a")]), 0.5),
            ],
        );
        c.normalise();
        assert!(matches!(&c.children[1].node,Node::Con(t) if t.layout==ConLayout::Tabbed));
        assert_eq!(c.check(true), Ok(()));
        if let Node::Con(t) = &mut c.children[1].node {
            t.children.clear();
        }
        c.normalise();
        assert_eq!(c.children.len(), 1);
        assert_eq!(c.check(true), Ok(()));
    }
    #[test]
    fn navigation_steps_through_tabs_before_leaving_the_container() {
        let mut l = Layout::new(
            con(
                ConLayout::SplitH,
                vec![
                    leaf("editor"),
                    con(ConLayout::Tabbed, vec![leaf("lua:a"), leaf("lua:b"), leaf("lua:c")]),
                ],
            ),
            id("lua:a"),
        )
        .unwrap();
        assert_eq!(
            navigate(&l, &id("lua:a"), Direction::Right, &frame()),
            Nav::Module(id("lua:b"))
        );
        l.set_focus(&id("lua:b")).unwrap();
        assert_eq!(
            navigate(&l, &id("lua:b"), Direction::Right, &frame()),
            Nav::Module(id("lua:c"))
        );
        l.set_focus(&id("lua:c")).unwrap();
        assert_eq!(navigate(&l, &id("lua:c"), Direction::Right, &frame()), Nav::Nothing);
        l.set_focus(&id("lua:a")).unwrap();
        assert_eq!(
            navigate(&l, &id("lua:a"), Direction::Left, &frame()),
            Nav::Module(id("editor"))
        );
        assert_eq!(navigate(&l, &id("lua:a"), Direction::Up, &frame()), Nav::TopBar);
        let mut l = beside(ConLayout::Stacked);
        l.set_focus(&id("lua:a")).unwrap();
        assert_eq!(
            navigate(&l, &id("lua:a"), Direction::Down, &frame()),
            Nav::Module(id("lua:b"))
        );
        assert_eq!(
            navigate(&l, &id("lua:a"), Direction::Left, &frame()),
            Nav::Module(id("editor"))
        );
    }
    #[test]
    fn hiding_the_drawn_tab_with_the_keys_gives_them_to_the_next_tab() {
        let mut l = beside(ConLayout::Tabbed);
        l.set_focus(&id("lua:a")).unwrap();
        assert_eq!(hide(&mut l, &id("lua:a"), &frame()), Ok(Some(id("lua:b"))));
        assert!(l.is_visible(&id("lua:b")));
    }
    #[test]
    fn switch_tab_always_names_the_tab_as_the_keys_next_owner() {
        let mut l = beside(ConLayout::Tabbed);
        l.set_focus(&id("lua:a")).unwrap();
        assert_eq!(switch_tab(&mut l, &id("lua:b")), Ok(id("lua:b")));
        l.set_focus(&id("editor")).unwrap();
        assert_eq!(switch_tab(&mut l, &id("lua:a")), Ok(id("lua:a")));
        hide(&mut l, &id("lua:b"), &frame()).unwrap();
        assert_eq!(switch_tab(&mut l, &id("lua:b")), Err(LayoutError::Hidden(id("lua:b"))));
        assert_eq!(
            switch_tab(&mut l, &id("lua:missing")),
            Err(LayoutError::NotInTree(id("lua:missing")))
        );
    }
    #[test]
    fn switch_tab_leaves_the_undo_of_the_last_layout_change_alone() {
        let mut l = beside(ConLayout::Tabbed);
        l.set_focus(&id("lua:a")).unwrap();
        container_mode(&mut l, &id("lua:a"), ConLayout::Stacked).unwrap();
        assert_eq!(switch_tab(&mut l, &id("lua:b")), Ok(id("lua:b")));
        assert!(l.is_visible(&id("lua:b")));
        assert!(!l.is_visible(&id("lua:a")));
        l.undo().unwrap();
        assert_eq!(l.root().children[1].node.as_con().unwrap().layout, ConLayout::Tabbed);
        assert!(l.is_in_view(l.focus()));
    }
    #[test]
    fn a_container_mode_change_draws_the_child_that_holds_the_keys() {
        // `target` is not the module with the keys: the keys may not sit on an undrawn tab, so the
        // child holding them stays the drawn one.
        let mut l = beside(ConLayout::SplitV);
        l.set_focus(&id("lua:a")).unwrap();
        assert_eq!(container_mode(&mut l, &id("lua:b"), ConLayout::Tabbed), Ok(true));
        assert_eq!(l.focus(), &id("lua:a"));
        assert!(l.is_visible(&id("lua:a")));
        assert!(!l.is_visible(&id("lua:b")));
    }
    #[test]
    fn a_tab_that_holds_a_split_is_named_by_its_focused_module() {
        let mut l = Layout::new(
            con(
                ConLayout::Tabbed,
                vec![
                    con(ConLayout::SplitH, vec![leaf("editor"), leaf("agent")]),
                    leaf("lua:a"),
                ],
            ),
            id("editor"),
        )
        .unwrap();
        l.set_focus(&id("agent")).unwrap();
        let a = arrange(&l, &frame());
        assert_eq!(a.strips[0].entries[0].id, id("agent"));
        assert_eq!(a.cell_of(&id("agent")).unwrap().y, 0);
        assert_eq!(a.cell_of(&id("editor")).unwrap().y, 0);
    }
    #[test]
    fn tab_neighbour_wraps_inside_the_innermost_tabbed_container() {
        let mut l = Layout::new(
            con(
                ConLayout::Tabbed,
                vec![
                    con(ConLayout::Stacked, vec![leaf("lua:a"), leaf("lua:b")]),
                    leaf("editor"),
                ],
            ),
            id("lua:a"),
        )
        .unwrap();
        assert_eq!(l.tab_neighbour(&id("lua:a"), true), Some(id("lua:b")));
        assert_eq!(l.tab_neighbour(&id("lua:a"), false), Some(id("lua:b")));
        l.set_focus(&id("lua:b")).unwrap();
        assert_eq!(l.tab_neighbour(&id("lua:b"), true), Some(id("lua:a")));
        hide(&mut l, &id("lua:a"), &frame()).unwrap();
        assert_eq!(l.tab_neighbour(&id("lua:b"), true), None);
    }
}
