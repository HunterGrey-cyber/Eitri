//! Tray ordering, visibility and attention rules.

use crate::attention::Attention;
use crate::layout::{chip_label, tray_covered, Layout, ModuleId, ModuleKind};

/// A visible chip carries its plain text and whether a card needs attention.
pub type ChipState = Option<(String, bool)>;

/// The modules the tray makes a chip for, in chip order: `modules` (the grid's hosts as added), with
/// a chip for every possible terminal number right after the base terminal's and every possible chat
/// view number right after the base chat's, in number order. Every number gets its chip when the window
/// is built, hidden until that instance exists and is off screen: the top bar's item list, which `h`/`l`
/// step through and HINT labels, is fixed when the window is built, so a chip made later could be
/// neither reached by key nor labelled.
pub fn chip_order(modules: &[ModuleId]) -> Vec<ModuleId> {
    let numbered =
        |kind| (2..=crate::layout::module::MAX_INSTANCE).filter_map(move |n| ModuleId::instance(kind, n).ok());
    let mut order = Vec::new();
    for id in modules {
        if id.kind().has_instances() && id.number() >= 2 {
            continue;
        }
        order.push(id.clone());
        if id.kind().has_instances() {
            order.extend(numbered(id.kind()));
        }
    }
    order
}

/// The mark a stowed scratch float's chip carries after its title: what tells a module waiting in the
/// scratchpad from one merely hidden.
const STOWED_MARK: &str = "\u{2301}";

/// One chip's state: `None` when its module needs no chip (`off_screen` is what does: the modules off
/// screen and a covered chat), else its text and whether it is asking for the user (the agent holding
/// a card). `stowed`: the module is a scratch float in the scratchpad, and its chip says so.
pub fn chip_state(
    id: &ModuleId,
    off_screen: &[ModuleId],
    title: &str,
    attention: Attention,
    stowed: bool,
) -> ChipState {
    if !off_screen.contains(id) {
        return None;
    }
    let title = if stowed {
        format!("{title} {STOWED_MARK}")
    } else {
        title.to_string()
    };
    if id.kind() == ModuleKind::Agent {
        Some((chip_label(&title, Some(attention)), attention.pending > 0))
    } else {
        Some((chip_label(&title, None), false))
    }
}

/// The chips whose state differs from the last refresh; updates the remembered states.
/// Leaving unchanged chips alone avoids laying out the top bar again during divider drags.
pub fn changed_chips(shown: &mut [ChipState], next: Vec<ChipState>) -> Vec<(usize, ChipState)> {
    let mut changed = Vec::new();
    for (i, (was, now)) in shown.iter_mut().zip(next).enumerate() {
        if *was != now {
            *was = now.clone();
            changed.push((i, now));
        }
    }
    changed
}

/// Every chip's state ([`chip_state`]) for `layout`, a chat view's chip asking `attention` for its own
/// view.
pub fn chip_states(
    chips: &[(ModuleId, String)],
    layout: &Layout,
    attention: &dyn Fn(&ModuleId) -> Attention,
    covered: &[ModuleId],
) -> Vec<ChipState> {
    let off = tray_covered(layout, covered);
    chips
        .iter()
        .map(|(id, title)| {
            let held = if id.kind() == ModuleKind::Agent {
                attention(id)
            } else {
                Attention::default()
            };
            chip_state(id, &off, title, held, layout.is_stowed(id))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wm::titles::strip_label;

    #[test]
    fn instance_chips_follow_the_base_terminal_in_number_order() {
        let modules = [
            ModuleId::editor(),
            ModuleId::agent(),
            ModuleId::terminal(),
            ModuleId::parse("terminal#3").unwrap(),
            ModuleId::lua("notes"),
        ];
        let order: Vec<String> = chip_order(&modules).iter().map(ToString::to_string).collect();
        let mut expected = vec!["editor", "agent"];
        let chats: Vec<String> = (2..=9).map(|n| format!("agent#{n}")).collect();
        expected.extend(chats.iter().map(String::as_str));
        expected.push("terminal");
        let numbered: Vec<String> = (2..=9).map(|n| format!("terminal#{n}")).collect();
        expected.extend(numbered.iter().map(String::as_str));
        expected.push("lua:notes");
        assert_eq!(
            order, expected,
            "each number once, after the base, before the Lua panels"
        );
        let with_view = [
            ModuleId::agent(),
            ModuleId::parse("agent#4").unwrap(),
            ModuleId::terminal(),
        ];
        let order = chip_order(&with_view);
        assert_eq!(
            order.iter().filter(|id| id.as_str() == "agent#4").count(),
            1,
            "a view the window opened with gets its chip once, in number order: {order:?}"
        );
    }

    #[test]
    fn a_chip_shows_only_off_screen_and_only_the_agents_asks() {
        let off = [ModuleId::agent(), ModuleId::terminal()];
        let two = Attention {
            pending: 2,
            unread: false,
            ..Default::default()
        };
        assert_eq!(chip_state(&ModuleId::editor(), &off, "editor", two, false), None);
        assert_eq!(
            chip_state(&ModuleId::agent(), &off, "agent", two, false),
            Some(("agent \u{2691}2".to_string(), true))
        );
        assert_eq!(
            chip_state(&ModuleId::terminal(), &off, "terminal", two, false),
            Some(("terminal".to_string(), false)),
            "the agent's attention is the agent's"
        );
        assert_eq!(
            chip_state(&ModuleId::agent(), &off, "agent", Attention::default(), false),
            Some(("agent".to_string(), false))
        );
    }

    #[test]
    fn a_stowed_scratch_float_reads_its_mark_on_its_chip() {
        use crate::layout::{hide, Axis, FloatRect, Frame, Node, Size};
        let size = Size { w: 1280, h: 721 };
        let frame = Frame::new(size, 1);
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let mut layout = Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent()),
            ),
            ModuleId::editor(),
        )
        .unwrap();
        layout.add_float(&t2, FloatRect::centred(size, 0.6), true).unwrap();
        let chip = |layout: &Layout| {
            let off = tray_covered(layout, &[]);
            chip_state(&t2, &off, "terminal 2", Attention::default(), layout.is_stowed(&t2))
        };
        assert_eq!(chip(&layout), None, "shown: no chip");
        hide(&mut layout, &t2, &frame).unwrap();
        assert!(layout.is_stowed(&t2));
        assert_eq!(chip(&layout), Some(("terminal 2 \u{2301}".to_string(), false)));
        assert_eq!(
            chip_state(
                &ModuleId::terminal(),
                &[ModuleId::terminal()],
                "terminal",
                Attention::default(),
                false
            ),
            Some(("terminal".to_string(), false)),
            "a module merely hidden has no mark"
        );
        let two = Attention {
            pending: 2,
            ..Default::default()
        };
        assert_eq!(
            chip_state(&ModuleId::agent(), &[ModuleId::agent()], "agent", two, true),
            Some(("agent \u{2301} \u{2691}2".to_string(), true)),
            "a stowed chat keeps its cards' mark after the scratchpad's"
        );
    }

    /// A chat on screen has a chip only while a float covers it: the window passes what covers it now,
    /// and the chip goes with the float.
    #[test]
    fn a_covered_chat_has_a_chip_only_while_covered() {
        use crate::layout::{Axis, FloatRect, Node, Size};
        let size = Size { w: 1280, h: 721 };
        let mut layout = Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent()),
            ),
            ModuleId::editor(),
        )
        .unwrap();
        layout
            .add_float(&ModuleId::terminal(), FloatRect::centred(size, 0.6), false)
            .unwrap();
        let one = Attention {
            pending: 1,
            ..Default::default()
        };
        let chat = ModuleId::agent();
        let chips = [
            (chat.clone(), "agent".to_string()),
            (ModuleId::terminal(), "terminal".to_string()),
        ];
        let attention = |id: &ModuleId| if *id == chat { one } else { Attention::default() };
        let chip = |covered: &[ModuleId]| chip_states(&chips, &layout, &attention, covered)[0].clone();
        assert_eq!(chip(&[]), None, "on screen and clear: no chip");
        assert_eq!(
            chip(std::slice::from_ref(&chat)),
            Some(("agent \u{2691}1".to_string(), true)),
            "covered: the chip carries its cards"
        );
        assert_eq!(chip(&[]), None, "the float moved off: the chip goes");
        assert_eq!(
            chip_states(&chips, &layout, &attention, std::slice::from_ref(&chat))[1],
            None,
            "the float itself is on screen"
        );
    }

    /// Each chat view's chip, and its strip entry, carries the attention of its own view's tabs: a card
    /// in the base view marks `agent`, and `agent 2`'s chip stays plain while its own tab owes nothing.
    #[test]
    fn each_chat_chip_carries_its_own_views_attention() {
        use crate::layout::{hide, Axis, Frame, Node, Size};
        let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
        let two = ModuleId::parse("agent#2").unwrap();
        let mut layout = Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::split(
                    Axis::Column,
                    0.5,
                    Node::Leaf(ModuleId::agent()),
                    Node::Leaf(two.clone()),
                ),
            ),
            ModuleId::editor(),
        )
        .unwrap();
        hide(&mut layout, &ModuleId::agent(), &frame).unwrap();
        hide(&mut layout, &two, &frame).unwrap();
        let one = Attention {
            pending: 1,
            ..Default::default()
        };
        let unread = Attention {
            unread: true,
            ..Default::default()
        };
        let chips = [
            (ModuleId::editor(), "editor".to_string()),
            (ModuleId::agent(), "agent".to_string()),
            (two.clone(), "agent 2".to_string()),
        ];
        let base_card = |id: &ModuleId| {
            if *id == ModuleId::agent() {
                one
            } else {
                Attention::default()
            }
        };
        assert_eq!(
            chip_states(&chips, &layout, &base_card, &[]),
            [
                None,
                Some(("agent \u{2691}1".to_string(), true)),
                Some(("agent 2".to_string(), false))
            ],
            "the base view's card marks the base view's chip alone"
        );
        let view_card = |id: &ModuleId| if *id == two { one } else { unread };
        assert_eq!(
            chip_states(&chips, &layout, &view_card, &[]),
            [
                None,
                Some(("agent \u{2022}".to_string(), false)),
                Some(("agent 2 \u{2691}1".to_string(), true))
            ],
            "a card in view 2 asks on view 2's chip"
        );
        assert_eq!(strip_label(&two, "agent 2", one), "agent 2 \u{2691}1");
        assert_eq!(strip_label(&ModuleId::agent(), "agent", Attention::default()), "agent");
    }

    /// A refresh with nothing new touches no chip: a divider drag refreshes the tray on every motion
    /// event, and each label update re-lays the bar out.
    /// A card for the hidden agent touches the agent's chip alone.
    #[test]
    fn a_refresh_touches_only_the_chips_that_changed() {
        let chip = |label: &str, asking: bool| Some((label.to_string(), asking));
        let mut shown = vec![None, None, None];
        let terminal_off = vec![None, None, chip("terminal", false)];
        assert_eq!(
            changed_chips(&mut shown, terminal_off.clone()),
            [(2, chip("terminal", false))],
            "the first refresh shows the terminal's chip"
        );
        assert_eq!(
            changed_chips(&mut shown, terminal_off.clone()),
            [],
            "a drag: nothing new"
        );
        assert_eq!(changed_chips(&mut shown, terminal_off), [], "and again");
        let agent_asks = vec![None, chip("agent \u{2691}1", true), chip("terminal", false)];
        assert_eq!(
            changed_chips(&mut shown, agent_asks),
            [(1, chip("agent \u{2691}1", true))]
        );
        assert_eq!(
            changed_chips(&mut shown, vec![None, None, chip("terminal", false)]),
            [(1, None)],
            "the chat back on screen hides its chip alone"
        );
        assert_eq!(shown, [None, None, chip("terminal", false)]);
    }
}
