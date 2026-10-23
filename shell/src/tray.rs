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
use eitri_core::layout::{Layout, ModuleId};

pub(crate) use eitri_core::wm::titles::{module_title, selection_text, strip_label};
pub(crate) use eitri_core::wm::tray::{changed_chips, chip_order, chip_state, chip_states, ChipState};
use gtk4::prelude::*;

pub(crate) struct Tray {
    widget: gtk4::Box,
    chips: Vec<(ModuleId, String, gtk4::Button)>,
    /// What each chip was last given ([`chip_state`]); `None` for a hidden one, which every chip
    /// starts as.
    shown: RefCell<Vec<ChipState>>,
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
        let chips: Vec<_> = self
            .chips
            .iter()
            .map(|(id, title, _)| (id.clone(), title.clone()))
            .collect();
        let next = chip_states(&chips, layout, attention, covered);
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
