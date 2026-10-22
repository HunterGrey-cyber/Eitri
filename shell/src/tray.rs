//! The top bar's tray: a chip for every module that is not on screen -- hidden,
//! or zoomed away -- so nothing that keeps running is out of reach. A chat view's reads `agent ⚑N`
//! (`agent 2 ⚑N`) while its own tabs' cards wait, `agent •` once a turn finished unseen there. A chat
//! view on screen under a float has its chip too, for its cards may be hidden; a scratch float stowed
//! in the scratchpad reads `<title> ⌁`.
//!
//! One chip per module, made once and never reparented, in a fixed order (the editor, the agent,
//! `agent 2` to `agent 9`, the terminal, `terminal 2` to `terminal 9`, then each Lua panel as it
//! registered: [`chip_order`])
//! -- a chip does not move as the layout
//! changes; only whether it shows does. Each is a top-bar item (`topbar-item`), so `Ctrl+k` then
//! `h`/`l`/`Enter` reaches it, HINT labels it (`Slot::Top`), and a click works. Activating one is
//! `Ctrl+a <key>` for its module (`main.rs`'s `open_module`).

use std::cell::RefCell;
use std::rc::Rc;

use eitri_core::attention::Attention;
use eitri_core::layout::{chip_label, export::head_name, tray_covered, Layout, ModuleId, ModuleKind, Selection};
use gtk4::prelude::*;

/// A module's name on its chip and in the prefix strip: the built-ins by kind, a numbered instance by
/// its kind and number (`terminal 2`), a Lua panel by the `title` it registered with (its id if it is
/// somehow not in `lua`).
pub(crate) fn module_title(id: &ModuleId, lua: &[(ModuleId, String)]) -> String {
    match id.kind() {
        ModuleKind::Editor => "editor".to_string(),
        ModuleKind::Agent | ModuleKind::Terminal if id.number() >= 2 => {
            format!("{} {}", id.base(), id.number())
        }
        ModuleKind::Agent => "agent".to_string(),
        ModuleKind::Terminal => "terminal".to_string(),
        ModuleKind::Canvas => "canvas".to_string(),
        ModuleKind::LuaWebview => lua
            .iter()
            .find(|(m, _)| m == id)
            .map(|(_, title)| title.clone())
            .unwrap_or_else(|| id.as_str().to_string()),
    }
}

/// A strip entry's text: what the module's chip would read, so a chat view on an inactive tab carries
/// its own tabs' cards' mark (`agent 2 ⚑1`) where a hidden view's chip would. Only a chat view has an
/// attention; `attention` is that view's.
pub(crate) fn strip_label(id: &ModuleId, title: &str, attention: Attention) -> String {
    if id.kind() == ModuleKind::Agent {
        chip_label(title, Some(attention))
    } else {
        chip_label(title, None)
    }
}

/// What the top bar says about a selected container: `None` while the selection is the module with
/// the keys, else `selected: <kind> (<titles>)` with the container's shown modules in tree order. The
/// selection is shown only as this text, as the mark is: an outline would take room from the modules.
pub(crate) fn selection_text(layout: &Layout, title: &dyn Fn(&ModuleId) -> String) -> Option<String> {
    let Selection::Con(id) = layout.selection() else {
        return None;
    };
    let con = layout.root().find(id)?;
    let kind = head_name(con.layout);
    let titles: Vec<String> = con.leaves().iter().filter(|m| layout.is_shown(m)).map(title).collect();
    Some(format!("selected: {kind} ({})", titles.join(", ")))
}

/// The modules the tray makes a chip for, in chip order: `modules` (the grid's hosts as added), with
/// a chip for every possible terminal number right after the base terminal's and every possible chat
/// view number right after the base chat's, in number order. Every number gets its chip when the window
/// is built, hidden until that instance exists and is off screen: the top bar's item list, which `h`/`l`
/// step through and HINT labels, is fixed when the window is built, so a chip made later could be
/// neither reached by key nor labelled.
pub(crate) fn chip_order(modules: &[ModuleId]) -> Vec<ModuleId> {
    let numbered =
        |kind| (2..=eitri_core::layout::module::MAX_INSTANCE).filter_map(move |n| ModuleId::instance(kind, n).ok());
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
pub(crate) fn chip_state(
    id: &ModuleId,
    off_screen: &[ModuleId],
    title: &str,
    attention: Attention,
    stowed: bool,
) -> Option<(String, bool)> {
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

/// The chips whose state differs from what `shown` says they were last given, each with its new
/// state; `shown` becomes `next`. What [`Tray::refresh`] touches: every divider-drag motion event is
/// a layout change that refreshes the tray, and `gtk_label_set_label` re-lays the top bar out even
/// for the same text, so a chip whose state did not
/// change is left alone.
pub(crate) fn changed_chips(
    shown: &mut [Option<(String, bool)>],
    next: Vec<Option<(String, bool)>>,
) -> Vec<(usize, Option<(String, bool)>)> {
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
fn chip_states<W>(
    chips: &[(ModuleId, String, W)],
    layout: &Layout,
    attention: &dyn Fn(&ModuleId) -> Attention,
    covered: &[ModuleId],
) -> Vec<Option<(String, bool)>> {
    let off = tray_covered(layout, covered);
    chips
        .iter()
        .map(|(id, title, _)| {
            let held = if id.kind() == ModuleKind::Agent {
                attention(id)
            } else {
                Attention::default()
            };
            chip_state(id, &off, title, held, layout.is_stowed(id))
        })
        .collect()
}

pub(crate) struct Tray {
    widget: gtk4::Box,
    chips: Vec<(ModuleId, String, gtk4::Button)>,
    /// What each chip was last given ([`chip_state`]); `None` for a hidden one, which every chip
    /// starts as.
    shown: RefCell<Vec<Option<(String, bool)>>>,
    on_activate: RefCell<Option<Rc<dyn Fn(&ModuleId)>>>,
}

impl Tray {
    /// A chip for each of `modules` (id and title), all hidden until [`Tray::refresh`].
    pub(crate) fn build(modules: &[(ModuleId, String)]) -> Rc<Tray> {
        Rc::new_cyclic(|weak: &std::rc::Weak<Tray>| {
            let widget = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
            widget.add_css_class("tray");
            widget.set_valign(gtk4::Align::Center);
            let chips: Vec<(ModuleId, String, gtk4::Button)> = modules
                .iter()
                .map(|(id, title)| {
                    let chip = gtk4::Button::with_label(title);
                    chip.add_css_class("tray-chip");
                    chip.add_css_class("topbar-item");
                    chip.set_valign(gtk4::Align::Center);
                    chip.set_visible(false);
                    let weak = weak.clone();
                    let target = id.clone();
                    chip.connect_clicked(move |_| {
                        let Some(tray) = weak.upgrade() else { return };
                        // Cloned out: activating re-enters the tray through the layout's change hook.
                        let hook = tray.on_activate.borrow().clone();
                        if let Some(hook) = hook {
                            hook(&target);
                        }
                    });
                    widget.append(&chip);
                    (id.clone(), title.clone(), chip)
                })
                .collect();
            Tray {
                widget,
                shown: RefCell::new(vec![None; chips.len()]),
                chips,
                on_activate: RefCell::new(None),
            }
        })
    }

    pub(crate) fn widget(&self) -> &gtk4::Box {
        &self.widget
    }

    /// Every chip, in order: top-bar items, before the bar's own.
    pub(crate) fn items(&self) -> Vec<gtk4::Widget> {
        self.chips.iter().map(|(_, _, chip)| chip.clone().upcast()).collect()
    }

    /// What activating a chip does. `main.rs` sets it once `open_module` exists.
    pub(crate) fn on_activate(&self, hook: impl Fn(&ModuleId) + 'static) {
        *self.on_activate.borrow_mut() = Some(Rc::new(hook));
    }

    /// Shows a chip for each module off screen in `layout`, and for each of `covered` on screen under a
    /// float (the chat views, whose cards a float may hide), each chat view's chip with that view's own
    /// `attention`. Touches only the chips whose state changed ([`changed_chips`]); the borrow is
    /// released before GTK is.
    pub(crate) fn refresh(&self, layout: &Layout, attention: &dyn Fn(&ModuleId) -> Attention, covered: &[ModuleId]) {
        let next = chip_states(&self.chips, layout, attention, covered);
        let changed = changed_chips(&mut self.shown.borrow_mut(), next);
        for (i, state) in changed {
            let chip = &self.chips[i].2;
            match state {
                Some((label, asking)) => {
                    if chip.label().as_deref() != Some(label.as_str()) {
                        chip.set_label(&label);
                    }
                    if asking {
                        chip.add_css_class("attention");
                    } else {
                        chip.remove_css_class("attention");
                    }
                    chip.set_visible(true);
                }
                None => chip.set_visible(false),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_module_is_named_by_its_kind_or_its_registered_title() {
        let lua = [(ModuleId::lua("notes"), "Notes".to_string())];
        assert_eq!(module_title(&ModuleId::editor(), &lua), "editor");
        assert_eq!(module_title(&ModuleId::terminal(), &lua), "terminal");
        assert_eq!(module_title(&ModuleId::lua("notes"), &lua), "Notes");
        assert_eq!(module_title(&ModuleId::lua("gone"), &lua), "lua:gone");
    }

    #[test]
    fn an_instances_title_carries_its_number() {
        let title = |text: &str| module_title(&ModuleId::parse(text).unwrap(), &[]);
        assert_eq!(title("terminal#2"), "terminal 2");
        assert_eq!(title("terminal#9"), "terminal 9");
        assert_eq!(title("agent#3"), "agent 3");
        assert_eq!(title("terminal"), "terminal", "the base keeps its plain name");
        assert_eq!(title("agent"), "agent");
    }

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
        use eitri_core::layout::{hide, Axis, FloatRect, Frame, Node, Size};
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
        use eitri_core::layout::{Axis, FloatRect, Node, Size};
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
            (chat.clone(), "agent".to_string(), ()),
            (ModuleId::terminal(), "terminal".to_string(), ()),
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
        use eitri_core::layout::{hide, Axis, Frame, Node, Size};
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
            (ModuleId::editor(), "editor".to_string(), ()),
            (ModuleId::agent(), "agent".to_string(), ()),
            (two.clone(), "agent 2".to_string(), ()),
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

    #[test]
    fn the_selection_reads_its_kind_and_modules() {
        use eitri_core::layout::{focus_parent, Axis, Node};
        let title = |id: &ModuleId| module_title(id, &[]);
        let mut layout = Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::split(
                    Axis::Column,
                    0.5,
                    Node::Leaf(ModuleId::editor()),
                    Node::Leaf(ModuleId::terminal()),
                ),
                Node::Leaf(ModuleId::agent()),
            ),
            ModuleId::editor(),
        )
        .unwrap();
        assert_eq!(
            selection_text(&layout, &title),
            None,
            "no selection: the module with the keys"
        );
        assert!(focus_parent(&mut layout));
        assert_eq!(
            selection_text(&layout, &title),
            Some("selected: column (editor, terminal)".to_string())
        );
        assert!(focus_parent(&mut layout));
        assert_eq!(
            selection_text(&layout, &title),
            Some("selected: row (editor, terminal, agent)".to_string())
        );
        layout.clear_selection();
        assert_eq!(selection_text(&layout, &title), None);
    }

    #[test]
    fn a_strip_entry_reads_like_a_chip() {
        let one = Attention {
            pending: 1,
            ..Default::default()
        };
        assert_eq!(strip_label(&ModuleId::agent(), "agent", one), "agent \u{2691}1");
        assert_eq!(strip_label(&ModuleId::terminal(), "terminal", one), "terminal");
        assert_eq!(strip_label(&ModuleId::editor(), "editor", one), "editor");
    }

    /// The window relabels the strips on every change of attention (`refresh_strips` beside the
    /// tray's refresh); what each refresh shows is this function over the attention it is given.
    #[test]
    fn a_card_arriving_relabels_the_strip_entry() {
        let none = Attention::default();
        let one = Attention {
            pending: 1,
            arrived: 1,
            ..Default::default()
        };
        let before = strip_label(&ModuleId::agent(), "agent", none);
        let after = strip_label(&ModuleId::agent(), "agent", one);
        assert_eq!(before, "agent");
        assert_eq!(after, "agent \u{2691}1");
        assert_ne!(before, after, "the entry's text changes with the card");
        let cleared = strip_label(&ModuleId::agent(), "agent", none);
        assert_eq!(cleared, "agent", "and back when it is answered");
    }

    /// A refresh with nothing new touches no chip: a divider drag refreshes the tray on every motion
    /// event, and each `set_label` re-lays the bar out.
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
