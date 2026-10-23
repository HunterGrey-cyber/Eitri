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
//!
//! What each chip says and whether it shows are the window's chrome model's (`eitri_core::wm::chrome`); the
//! tray only draws a chip as the model has it ([`Tray::set_chip`]).

use std::cell::RefCell;
use std::rc::Rc;

use eitri_core::layout::ModuleId;
use eitri_core::wm::chrome::Chip;
use gtk4::prelude::*;

/// What activating a chip runs, with its module.
type ActivateHook = Rc<dyn Fn(&ModuleId)>;

pub(crate) struct Tray {
    widget: gtk4::Box,
    chips: Vec<(ModuleId, gtk4::Button)>,
    on_activate: RefCell<Option<ActivateHook>>,
}

impl Tray {
    /// A chip for each of `modules` (id and title), all hidden until [`Tray::set_chip`] shows one.
    pub(crate) fn build(modules: &[(ModuleId, String)]) -> Rc<Tray> {
        Rc::new_cyclic(|weak: &std::rc::Weak<Tray>| {
            let widget = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
            widget.add_css_class("tray");
            widget.set_valign(gtk4::Align::Center);
            let chips: Vec<(ModuleId, gtk4::Button)> = modules
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
                    (id.clone(), chip)
                })
                .collect();
            Tray {
                widget,
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
        self.chips.iter().map(|(_, chip)| chip.clone().upcast()).collect()
    }

    /// What activating a chip does. `main.rs` sets it once `open_module` exists.
    pub(crate) fn on_activate(&self, hook: impl Fn(&ModuleId) + 'static) {
        *self.on_activate.borrow_mut() = Some(Rc::new(hook));
    }

    /// Draws `chip` as the model has it: its text (as plain text), its attention mark and whether it shows. A
    /// module the tray has no chip for draws nothing.
    pub(crate) fn set_chip(&self, chip: &Chip) {
        let Some((_, button)) = self.chips.iter().find(|(id, _)| *id == chip.module) else {
            return;
        };
        if chip.shown {
            if button.label().as_deref() != Some(chip.label.as_str()) {
                button.set_label(&chip.label);
            }
            if chip.attention {
                button.add_css_class("attention");
            } else {
                button.remove_css_class("attention");
            }
        }
        button.set_visible(chip.shown);
    }
}
