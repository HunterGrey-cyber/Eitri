//! The top bar's tray (modules spec §3.3): a chip for every module that is not on screen -- hidden,
//! or zoomed away -- so nothing that keeps running is out of reach. The agent's reads `agent ⚑N`
//! while cards wait, `agent •` once a turn finished unseen.
//!
//! One chip per module, made once and never reparented, in a fixed order (the editor, the agent,
//! the terminal, then each Lua panel as it registered) -- a chip does not move as the layout
//! changes; only whether it shows does. Each is a top-bar item (`topbar-item`), so `Ctrl+k` then
//! `h`/`l`/`Enter` reaches it, HINT labels it (`Slot::Top`), and a click works. Activating one is
//! `Ctrl+a <key>` for its module (`main.rs`'s `open_module`).

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use neovibe_core::attention::Attention;
use neovibe_core::layout::{chip_label, tray, Layout, ModuleId, ModuleKind};

/// A module's name on its chip and in the prefix strip: the built-ins by kind, a Lua panel by the
/// `title` it registered with (its id if it is somehow not in `lua`).
pub(crate) fn module_title(id: &ModuleId, lua: &[(ModuleId, String)]) -> String {
    match id.kind() {
        ModuleKind::Editor => "editor".to_string(),
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

/// One chip's state: `None` when its module is on screen (no chip), else its text and whether it
/// is asking for the user (the agent holding a card).
pub(crate) fn chip_state(
    id: &ModuleId,
    off_screen: &[ModuleId],
    title: &str,
    attention: Attention,
) -> Option<(String, bool)> {
    if !off_screen.contains(id) {
        return None;
    }
    if id.kind() == ModuleKind::Agent {
        Some((chip_label(title, Some(attention)), attention.pending > 0))
    } else {
        Some((chip_label(title, None), false))
    }
}

/// The chips whose state differs from what `shown` says they were last given, each with its new
/// state; `shown` becomes `next`. What [`Tray::refresh`] touches: every divider-drag motion event is
/// a layout change that refreshes the tray, and `gtk_label_set_label` re-lays the top bar out even
/// for the same text (the whole-branch review's window finding 5), so a chip whose state did not
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

    /// Shows a chip for each module off screen in `layout`, with the agent's `attention`. Touches only
    /// the chips whose state changed ([`changed_chips`]); the borrow is released before GTK is.
    pub(crate) fn refresh(&self, layout: &Layout, attention: Attention) {
        let off = tray(layout);
        let next = self
            .chips
            .iter()
            .map(|(id, title, _)| chip_state(id, &off, title, attention))
            .collect();
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
    fn a_chip_shows_only_off_screen_and_only_the_agents_asks() {
        let off = [ModuleId::agent(), ModuleId::terminal()];
        let two = Attention {
            pending: 2,
            unread: false,
            ..Default::default()
        };
        assert_eq!(chip_state(&ModuleId::editor(), &off, "editor", two), None);
        assert_eq!(
            chip_state(&ModuleId::agent(), &off, "agent", two),
            Some(("agent \u{2691}2".to_string(), true))
        );
        assert_eq!(
            chip_state(&ModuleId::terminal(), &off, "terminal", two),
            Some(("terminal".to_string(), false)),
            "the agent's attention is the agent's"
        );
        assert_eq!(
            chip_state(&ModuleId::agent(), &off, "agent", Attention::default()),
            Some(("agent".to_string(), false))
        );
    }

    /// A refresh with nothing new touches no chip: a divider drag refreshes the tray on every motion
    /// event, and each `set_label` re-lays the bar out (the whole-branch review's window finding 5).
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
