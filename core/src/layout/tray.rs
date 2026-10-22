//! The top bar's tray: chips for modules not on screen. A module whose own title is on a strip on
//! screen needs no chip; one tabbed away with no such title (an inactive tab inside a container that
//! is itself an inactive tab, or under a zoom) gets one, or nothing would show that it is there --
//! nor carry the chat's waiting cards. A chip is a top-bar item, so `Ctrl+k` then `h`/`l`/`Enter`
//! reaches it, HINT labels it, and a click works.

use super::module::ModuleId;
use super::tree::Layout;
use crate::attention::{AgentPlace, Attention};

/// Every module not on screen, in tree order, except one titled on a strip that is
/// ([`Layout::is_titled_on_strip`]). A module gone for the rest of the window has no chip because
/// nothing would bring it back, and an away one has none because the other window holds it.
pub fn tray(layout: &Layout) -> Vec<ModuleId> {
    tray_covered(layout, &[])
}

/// Tray chips plus covered modules on screen, in `modules()` order.
pub fn tray_covered(layout: &Layout, covered: &[ModuleId]) -> Vec<ModuleId> {
    layout
        .modules()
        .into_iter()
        .filter(|id| {
            (layout.is_visible(id) && covered.contains(id))
                || (!layout.is_visible(id)
                    && !layout.is_gone(id)
                    && !layout.is_away(id)
                    && !layout.is_titled_on_strip(id))
        })
        .collect()
}

/// A chip's text: the module's title, and for the agent what it is holding -- `agent ⚑2` while cards
/// wait (decision b), `agent •` once a turn finished while it was away.
pub fn chip_label(title: &str, attention: Option<Attention>) -> String {
    match attention {
        Some(Attention { pending, unread, .. }) if pending > 0 => {
            let dot = if unread { " \u{2022}" } else { "" };
            format!("{title} \u{2691}{pending}{dot}")
        }
        Some(Attention { unread: true, .. }) => format!("{title} \u{2022}"),
        _ => title.to_string(),
    }
}

/// Where the agent is (`attention::react` decides from it).
pub fn agent_place(layout: &Layout) -> AgentPlace {
    agent_place_covered(layout, &[])
}

/// A visible chat in `covered` may have cards hidden by a float.
pub fn agent_place_covered(layout: &Layout, covered: &[ModuleId]) -> AgentPlace {
    view_place(layout, &ModuleId::agent(), covered)
}

/// Where the chat view `id` is, `covered` naming the modules a float covers now.
pub fn view_place(layout: &Layout, id: &ModuleId, covered: &[ModuleId]) -> AgentPlace {
    if layout.is_visible(id) {
        if covered.contains(id) {
            AgentPlace::Covered
        } else {
            AgentPlace::OnScreen
        }
    } else if !layout.is_zoomed() && layout.is_tabbed_away(id) {
        AgentPlace::TabbedAway
    } else if layout.is_shown(id) {
        AgentPlace::ZoomedAway
    } else if layout.is_zoomed() {
        AgentPlace::HiddenUnderZoom
    } else {
        AgentPlace::Hidden
    }
}

#[cfg(test)]
mod tests {
    use super::super::geometry::{hide, Frame, Size};
    use super::super::module::{ModuleDecl, Placement};
    use super::*;

    fn frame() -> Frame<'static> {
        Frame::new(Size { w: 1280, h: 721 }, 1)
    }

    fn tabbed_chat() -> Layout {
        use super::super::{Child, Con, ConLayout, Node};
        Layout::new(
            Node::Con(Con::new(
                ConLayout::Tabbed,
                vec![
                    Child::new(Node::Leaf(ModuleId::editor()), 0.5),
                    Child::new(Node::Leaf(ModuleId::agent()), 0.5),
                ],
            )),
            ModuleId::editor(),
        )
        .unwrap()
    }

    #[test]
    fn a_chat_views_place_is_its_own() {
        use super::super::{Child, Con, ConLayout, Node};
        let a2 = ModuleId::parse("agent#2").unwrap();
        let mut layout = Layout::new(
            Node::Con(Con::new(
                ConLayout::SplitH,
                vec![
                    Child::new(Node::Leaf(ModuleId::editor()), 0.4),
                    Child::new(
                        Node::Con(Con::new(
                            ConLayout::Tabbed,
                            vec![
                                Child::new(Node::Leaf(ModuleId::agent()), 0.5),
                                Child::new(Node::Leaf(a2.clone()), 0.5),
                            ],
                        )),
                        0.6,
                    ),
                ],
            )),
            ModuleId::editor(),
        )
        .unwrap();
        assert_eq!(view_place(&layout, &ModuleId::agent(), &[]), AgentPlace::OnScreen);
        assert_eq!(view_place(&layout, &a2, &[]), AgentPlace::TabbedAway);
        assert_eq!(
            agent_place_covered(&layout, &[]),
            AgentPlace::OnScreen,
            "the base view's place is unchanged"
        );
        assert_eq!(
            view_place(&layout, &ModuleId::agent(), &[ModuleId::agent()]),
            AgentPlace::Covered
        );
        assert_eq!(
            view_place(&layout, &a2, &[a2.clone()]),
            AgentPlace::TabbedAway,
            "a tab not drawn is not covered"
        );
        hide(&mut layout, &a2, &frame()).unwrap();
        assert_eq!(view_place(&layout, &a2, &[]), AgentPlace::Hidden);
        assert_eq!(view_place(&layout, &ModuleId::agent(), &[]), AgentPlace::OnScreen);
    }

    #[test]
    fn a_chat_on_an_inactive_tab_is_tabbed_away_and_reveal_brings_it() {
        use crate::attention::{react, ChatOnPermission};
        let mut layout = tabbed_chat();
        let place = agent_place(&layout);
        assert_eq!(place, AgentPlace::TabbedAway);
        assert!(crate::tabs::reveal_on_switch(place));
        let after = Attention {
            arrived: 1,
            pending: 1,
            ..Default::default()
        };
        assert!(react(ChatOnPermission::Reveal, Attention::default(), after, place).reveal);
        assert!(react(ChatOnPermission::Badge, Attention::default(), after, place).toast);
        assert!(!layout.bring_forward(&ModuleId::agent()));
        layout.toggle_zoom(&ModuleId::editor());
        assert_eq!(agent_place(&layout), AgentPlace::ZoomedAway);

        use super::super::{Axis, Child, Con, ConLayout, Node};
        let mut tabs = Con::new(
            ConLayout::Tabbed,
            vec![
                Child::new(Node::Leaf(ModuleId::agent()), 0.5),
                Child::new(Node::Leaf(ModuleId::terminal()), 0.5),
            ],
        );
        tabs.focus = 1;
        let mut layout = Layout::new(
            Node::split(Axis::Row, 0.5, Node::Leaf(ModuleId::editor()), Node::Con(tabs)),
            ModuleId::editor(),
        )
        .unwrap();
        assert_eq!(agent_place(&layout), AgentPlace::TabbedAway);
        assert!(layout.bring_forward(&ModuleId::agent()));
        assert_eq!(agent_place(&layout), AgentPlace::OnScreen);
        assert_eq!(layout.focus(), &ModuleId::editor());
    }

    #[test]
    fn an_inactive_tab_has_no_chip() {
        let mut layout = tabbed_chat();
        assert!(tray(&layout).is_empty());
        hide(&mut layout, &ModuleId::agent(), &frame()).unwrap();
        assert_eq!(tray(&layout), [ModuleId::agent()]);
    }

    /// An outer tabbed container drawing the terminal, its other tab an inner tabbed container of the
    /// chat and `terminal#2` that remembers `terminal#2`: the outer strip's entry for the inner one
    /// names `terminal#2`, the inner strip is not on screen, so the chat has no title anywhere and gets
    /// a chip -- which carries its waiting cards. `terminal#2` has its entry and no chip.
    #[test]
    fn a_tab_with_no_title_on_screen_gets_a_chip() {
        use super::super::{Child, Con, ConLayout, Node};
        let second = ModuleId::parse("terminal#2").unwrap();
        let mut inner = Con::new(
            ConLayout::Tabbed,
            vec![
                Child::new(Node::Leaf(ModuleId::agent()), 0.5),
                Child::new(Node::Leaf(second.clone()), 0.5),
            ],
        );
        inner.focus = 1;
        let outer = Con::new(
            ConLayout::Tabbed,
            vec![
                Child::new(Node::Leaf(ModuleId::terminal()), 0.5),
                Child::new(Node::Con(inner), 0.5),
            ],
        );
        let mut layout = Layout::new(
            Node::split(
                super::super::Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::Con(outer),
            ),
            ModuleId::editor(),
        )
        .unwrap();
        // `Layout::new` draws the editor's own path; the outer container draws its first tab.
        assert!(layout.is_visible(&ModuleId::terminal()));
        assert!(layout.is_titled_on_strip(&second));
        assert!(!layout.is_titled_on_strip(&ModuleId::agent()));
        assert_eq!(tray(&layout), [ModuleId::agent()]);
        assert_eq!(
            chip_label(
                "agent",
                Some(Attention {
                    pending: 1,
                    ..Default::default()
                })
            ),
            "agent \u{2691}1"
        );

        // With the inner container drawn, its own strip names the chat: no chip.
        layout.set_focus(&second).unwrap();
        assert!(layout.is_titled_on_strip(&ModuleId::agent()));
        assert!(tray(&layout).is_empty());

        // A zoom leaves no strip on screen: every module off screen has its chip.
        layout.toggle_zoom(&second);
        assert_eq!(
            tray(&layout),
            [ModuleId::editor(), ModuleId::terminal(), ModuleId::agent()]
        );
    }

    #[test]
    fn the_tray_holds_every_module_not_on_screen_in_tree_order() {
        let mut layout = Layout::initial(&[ModuleDecl {
            id: ModuleId::terminal(),
            placement: Placement::BelowRoot,
        }])
        .unwrap();
        assert!(tray(&layout).is_empty());
        hide(&mut layout, &ModuleId::terminal(), &frame()).unwrap();
        assert_eq!(tray(&layout), [ModuleId::terminal()]);
        layout.toggle_zoom(&ModuleId::agent());
        assert_eq!(
            tray(&layout),
            [ModuleId::editor(), ModuleId::terminal()],
            "zoomed away is off screen too"
        );
    }

    #[test]
    fn the_agents_chip_says_what_it_holds() {
        assert_eq!(chip_label("terminal", None), "terminal");
        assert_eq!(chip_label("agent", Some(Attention::default())), "agent");
        assert_eq!(
            chip_label(
                "agent",
                Some(Attention {
                    pending: 2,
                    unread: true,
                    ..Default::default()
                })
            ),
            "agent \u{2691}2 \u{2022}"
        );
        assert_eq!(
            chip_label(
                "agent",
                Some(Attention {
                    pending: 0,
                    unread: true,
                    ..Default::default()
                })
            ),
            "agent \u{2022}"
        );
        assert_eq!(
            chip_label(
                "agent",
                Some(Attention {
                    pending: 1,
                    unread: false,
                    ..Default::default()
                })
            ),
            "agent \u{2691}1"
        );
    }

    #[test]
    fn the_agent_is_on_screen_hidden_hidden_under_a_zoom_or_zoomed_away() {
        let mut layout = Layout::initial(&[]).unwrap();
        assert_eq!(agent_place(&layout), AgentPlace::OnScreen);
        layout.toggle_zoom(&ModuleId::editor());
        assert_eq!(agent_place(&layout), AgentPlace::ZoomedAway);
        layout.unzoom();
        hide(&mut layout, &ModuleId::agent(), &frame()).unwrap();
        assert_eq!(agent_place(&layout), AgentPlace::Hidden);
        let mut three = Layout::initial(&[ModuleDecl {
            id: ModuleId::lua("side"),
            placement: Placement::RightOfRoot,
        }])
        .unwrap();
        hide(&mut three, &ModuleId::agent(), &frame()).unwrap();
        three.toggle_zoom(&ModuleId::editor());
        assert_eq!(agent_place(&three), AgentPlace::HiddenUnderZoom);
    }

    #[test]
    fn a_chat_under_a_float_is_covered_and_has_a_chip_while_it_is() {
        let mut layout = Layout::initial(&[]).unwrap();
        let agent = ModuleId::agent();
        assert_eq!(agent_place_covered(&layout, &[agent.clone()]), AgentPlace::Covered);
        assert_eq!(agent_place_covered(&layout, &[]), AgentPlace::OnScreen);
        assert_eq!(tray_covered(&layout, &[agent.clone()]), vec![agent.clone()]);
        assert!(tray_covered(&layout, &[]).is_empty());
        layout.toggle_zoom(&ModuleId::editor());
        assert_eq!(agent_place_covered(&layout, &[agent.clone()]), AgentPlace::ZoomedAway);
        assert_eq!(
            tray_covered(&layout, &[agent.clone()]),
            vec![agent],
            "once, as zoomed away"
        );
    }
}
