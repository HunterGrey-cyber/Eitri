//! The order a module leaves the screen in, when the keys may be on it: the module that gets them is mapped and
//! given them first, and only then is what left unmapped. Unmapping first would leave the keys on a widget
//! leaving the screen, and the toolkit's own focus chain would choose where they go.

use crate::layout::{Frame, Layout, LayoutError, ModuleId};

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

/// A workspace switch gives the arriving module the keys before unmapping, then restores its float order.
pub fn switch_order(next: Option<&ModuleId>) -> Vec<Step> {
    let mut steps = leave_order(next);
    steps.push(Step::Restack);
    steps
}

/// The layout half of a hide (`module.hide`, a module key on the module with the keys, `eitri.layout.hide`, a
/// follow command): `id` hidden in the layout; the module that gets the keys when `id` held them, for
/// [`leave_order`].
pub fn hide_layout(layout: &mut Layout, id: &ModuleId, frame: &Frame) -> Result<Option<ModuleId>, LayoutError> {
    crate::layout::hide(layout, id, frame)
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
    /// The keys the default keymap answers a y/n with: `y`, as the shell translates it.
    fn yes() -> crate::wm::KeyPress {
        let spec = crate::keymap::KeySpec::parse("y").unwrap();
        crate::wm::KeyPress {
            keycode: 29,
            press: crate::keymap::prefix::Press::Key(spec),
            key: Some(spec.key),
            mods: crate::wm::Mods::default(),
            super_or_hyper: false,
            super_or_meta: false,
        }
    }

    fn focus(module: &ModuleId) -> crate::wm::HostOp {
        crate::wm::HostOp::Focus {
            win: rig::WIN,
            to: crate::wm::FocusTo::Module(module.clone()),
            on_grab: vec![],
        }
    }

    use crate::wm::verbs::rig::{self, allocate, map, moves, ops, unmap};
    use crate::wm::Decide;

    /// `module.hide` with the keys in the terminal gives them to the module above it that had them last, and only
    /// then unmaps it: unmapping first lets the toolkit's own focus chain choose where they go. Both upper modules
    /// are tried as the one that had the keys last, so a fixed choice cannot pass either.
    #[test]
    fn hiding_the_module_with_the_keys_gives_them_away_before_it_unmaps() {
        for last_above in [ModuleId::agent(), ModuleId::editor()] {
            let mut layout = crate::wm::persist::first_launch_layout(&[]).unwrap();
            layout.show(&ModuleId::terminal()).unwrap();
            layout.set_focus(&last_above).unwrap();
            layout.set_focus(&ModuleId::terminal()).unwrap();
            let mut r = rig::rig(layout);
            let batch = r.run(crate::keymap::Action::ModuleHide);
            assert_eq!(
                moves(&batch),
                vec![focus(&last_above), unmap(&ModuleId::terminal()), allocate()],
                "last above: {last_above}"
            );
            assert_eq!(r.layout.borrow().focus(), &last_above);
        }
    }

    /// `prefix x` takes the same door: the keys leave the killed terminal before it unmaps, and it is left hidden
    /// where a first launch puts it, its shell hung up.
    #[test]
    fn killing_the_module_with_the_keys_gives_them_away_before_it_unmaps() {
        let mut layout = crate::wm::persist::first_launch_layout(&[]).unwrap();
        layout.show(&ModuleId::terminal()).unwrap();
        layout.set_focus(&ModuleId::agent()).unwrap();
        layout.set_focus(&ModuleId::terminal()).unwrap();
        let mut r = rig::rig(layout);
        let asked = r.run(crate::keymap::Action::ModuleKill);
        assert!(moves(&asked).is_empty(), "the y/n asks first: {asked:?}");
        assert!(r.controller.chrome(rig::WIN).prompt.is_some());
        let (verdict, batch) = r.controller.key(rig::WIN, &crate::wm::KeyStage::Window, &yes());
        assert_eq!(verdict, crate::wm::Verdict::Consumed);
        assert_eq!(
            moves(&batch),
            vec![focus(&ModuleId::agent()), unmap(&ModuleId::terminal()), allocate()]
        );
        assert!(
            ops(&batch).contains(&crate::wm::HostOp::Terminal(crate::wm::TermOp::HangUp {
                win: rig::WIN,
                module: ModuleId::terminal(),
            }))
        );
        assert_eq!(
            r.layout.borrow().root(),
            crate::wm::persist::first_launch_layout(&[]).unwrap().root(),
            "back where a first launch puts it"
        );
        assert_eq!(r.controller.chrome(rig::WIN).prompt, None);
    }

    /// A numbered terminal killed while it holds the keys as the drawn tab of a tabbed container: the tab drawn
    /// next is mapped and given the keys before the killed one is unmapped, and the killed one leaves the layout
    /// altogether, its pane left idle for the next terminal of its number.
    #[test]
    fn killing_an_instance_gives_the_keys_away_before_it_unmaps() {
        let (two, base) = (ModuleId::parse("terminal#2").unwrap(), ModuleId::terminal());
        let leaf = |id: ModuleId| Child::new(Node::Leaf(id), 0.5);
        let tabs = Con::new(ConLayout::Tabbed, vec![leaf(two.clone()), leaf(base.clone())]);
        let root = Con::new(
            ConLayout::SplitH,
            vec![leaf(ModuleId::editor()), Child::new(Node::Con(tabs), 0.5)],
        );
        let mut layout = Layout::new(Node::Con(root), ModuleId::editor()).unwrap();
        layout.set_focus(&two).unwrap();
        assert!(
            layout.is_visible(&two) && !layout.is_visible(&base),
            "the premise: #2 is drawn"
        );
        let mut r = rig::rig(layout);
        r.run(crate::keymap::Action::ModuleKill);
        let (_, batch) = r.controller.key(rig::WIN, &crate::wm::KeyStage::Window, &yes());
        assert_eq!(moves(&batch), vec![map(&base), focus(&base), unmap(&two), allocate()]);
        assert!(ops(&batch).contains(&crate::wm::HostOp::DropSurface {
            win: rig::WIN,
            module: two.clone(),
            why: crate::wm::DropWhy::Killed,
        }));
        let layout = r.layout.borrow();
        assert_eq!(layout.focus(), &base);
        assert!(!layout.contains(&two), "an instance leaves the layout");
        assert!(layout.is_visible(&base));
    }

    /// Hiding the drawn tab while it holds the keys gives them to the tab drawn next, which was unmapped a moment
    /// ago: its host is mapped before it gets them, and the hidden tab is unmapped last. Hidden without the keys,
    /// the next tab is drawn and mapped and no keys move.
    #[test]
    fn hiding_the_drawn_tab_maps_the_next_tab_before_it_gets_the_keys() {
        let (a, b) = (ModuleId::lua("a"), ModuleId::lua("b"));
        let mut with_keys = editor_beside_tabs();
        with_keys.set_focus(&a).unwrap();
        let mut r = rig::rig(with_keys);
        let batch = r.run(crate::keymap::Action::ModuleHide);
        assert_eq!(moves(&batch), vec![map(&b), focus(&b), unmap(&a), allocate()]);
        assert_eq!(r.layout.borrow().focus(), &b);

        let mut r = rig::rig(editor_beside_tabs());
        let batch = r
            .controller
            .lua_request(rig::WIN, crate::lua::layout::LayoutRequest::Hide(a.clone()));
        assert_eq!(moves(&batch), vec![map(&b), unmap(&a), allocate()]);
        assert_eq!(r.layout.borrow().focus(), &ModuleId::editor());
    }

    /// Hiding a module that does not hold the keys moves no keys, and still unmaps it. A hide the layout refuses
    /// does neither, its batch the refusal alone: one not in the tree, or the last shown module.
    #[test]
    fn a_hide_without_the_keys_only_unmaps_and_a_refused_hide_does_nothing() {
        let mut layout = crate::wm::persist::first_launch_layout(&[]).unwrap();
        layout.show(&ModuleId::terminal()).unwrap();
        assert_eq!(layout.focus(), &ModuleId::editor(), "the premise");
        let mut r = rig::rig(layout);
        let batch = r
            .controller
            .lua_request(rig::WIN, crate::lua::layout::LayoutRequest::Hide(ModuleId::terminal()));
        assert_eq!(moves(&batch), vec![unmap(&ModuleId::terminal()), allocate()]);
        assert_eq!(r.layout.borrow().focus(), &ModuleId::editor());

        let before = r.layout.borrow().clone();
        let nowhere = ModuleId::lua("nowhere");
        let batch = r
            .controller
            .lua_request(rig::WIN, crate::lua::layout::LayoutRequest::Hide(nowhere.clone()));
        assert!(ops(&batch).is_empty(), "a Lua refusal is logged: {batch:?}");
        assert_eq!(*r.layout.borrow(), before);

        let mut alone = Layout::initial(&[]).unwrap();
        crate::layout::hide(
            &mut alone,
            &ModuleId::agent(),
            &Frame::new(super::super::UNALLOCATED, 1),
        )
        .unwrap();
        let mut r = rig::rig(alone);
        let before = r.layout.borrow().clone();
        let said = r.said();
        let batch = r.run(crate::keymap::Action::ModuleHide);
        let text = crate::wm::feedback::layout_refusal_text(&LayoutError::LastVisible(ModuleId::editor()));
        r.assert_refused(&batch, &said, &text);
        assert_eq!(*r.layout.borrow(), before);
    }
}
