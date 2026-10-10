//! The app's main menu, kept the way the panel needs it: an Edit menu for the page's text fields, and no way to
//! open a second window.
//!
//! Neovide's own main menu has no Edit menu, and without one Cmd+A/C/V/Z do nothing in the composer: the page
//! leaves those chords to AppKit, and AppKit runs them only through a menu item. Neovide builds its main menu
//! itself, after the window exists (on the window's first focus), by replacing the whole menu, so an item
//! inserted once would be dropped. [`keep_main_menu`] is therefore called repeatedly (every host tick and every
//! frame) and inserts the menu whenever the current main menu lacks it; it costs a lookup by tag and a walk of
//! the top-level items when nothing is to do.
//!
//! **The Edit items act only while the panel has the keys.** Their target is an [`EditTarget`], which enables
//! an item only then (and only when the responder that would act enables it too), and then sends the action
//! on to the first responder. While the editor has the keys every item is disabled, so the menu does not take
//! the chord and it reaches nvim: Cmd+Z there is nvim's `<D-z>`, never an undo in the composer through the
//! window's own undo manager. A chord the page leaves unhandled reaches the menu when WebKit sends it again,
//! which happens only while the page has the keys, when the items are enabled.
//!
//! **No second window.** Neovide's Window menu has New Window (Cmd+N). The window and its panel are one, and a
//! second Neovide window would get the first one's command line (the same socket for nvim) and no panel, so
//! the item is removed in the same pass.
//!
//! **Quit asks first while an agent is at work.** Neovide's Quit item sends nvim its quit key at once. It is
//! pointed at a [`QuitTarget`] instead, which asks the window (the same question as the quit chord and the
//! close button) and, when the quit may go on, sends the item's original action to its original target. So a
//! Quit chosen with the pointer, or by whatever chord the item carries, is asked about too.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenu, NSMenuItem};
use objc2_foundation::{NSObjectProtocol, NSString};

/// The tag of the Edit menu's item in the main menu, by which it is found again. Neovide tags none of its
/// items, so any value but 0 is free; this one spells "EITR".
pub const EDIT_MENU_TAG: isize = 0x4549_5452;

/// The action of Neovide's New Window item.
fn new_window_action() -> Sel {
    sel!(neovideCreateWindow:)
}

/// One item: title, action, key equivalent, and whether Shift is part of it.
type Item = (&'static str, Sel, &'static str, bool);

/// `None` is a separator.
fn items() -> [Option<Item>; 7] {
    [
        Some(("Undo", sel!(undo:), "z", false)),
        Some(("Redo", sel!(redo:), "z", true)),
        None,
        Some(("Cut", sel!(cut:), "x", false)),
        Some(("Copy", sel!(copy:), "c", false)),
        Some(("Paste", sel!(paste:), "v", false)),
        Some(("Select All", sel!(selectAll:), "a", false)),
    ]
}

/// Where the Edit menu goes: right after the application menu, as in every Mac app; first in a menu that has
/// none.
pub fn edit_menu_index(items_in_main_menu: isize) -> isize {
    items_in_main_menu.clamp(0, 1)
}

pub struct EditTargetIvars {
    panel_has_keys: Box<dyn Fn() -> bool>,
}

define_class!(
    /// The Edit items' target: enabled only while the panel has the keys, and then a relay to the responder
    /// chain.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "EitriEditMenuTarget"]
    #[ivars = EditTargetIvars]
    pub struct EditTarget;

    unsafe impl NSObjectProtocol for EditTarget {}

    impl EditTarget {
        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, item: &NSMenuItem) -> Bool {
            Bool::new(self.enabled(item))
        }

        #[unsafe(method(undo:))]
        fn undo(&self, sender: Option<&AnyObject>) {
            self.relay(sel!(undo:), sender);
        }

        #[unsafe(method(redo:))]
        fn redo(&self, sender: Option<&AnyObject>) {
            self.relay(sel!(redo:), sender);
        }

        #[unsafe(method(cut:))]
        fn cut(&self, sender: Option<&AnyObject>) {
            self.relay(sel!(cut:), sender);
        }

        #[unsafe(method(copy:))]
        fn copy(&self, sender: Option<&AnyObject>) {
            self.relay(sel!(copy:), sender);
        }

        #[unsafe(method(paste:))]
        fn paste(&self, sender: Option<&AnyObject>) {
            self.relay(sel!(paste:), sender);
        }

        #[unsafe(method(selectAll:))]
        fn select_all(&self, sender: Option<&AnyObject>) {
            self.relay(sel!(selectAll:), sender);
        }
    }
);

impl EditTarget {
    /// `panel_has_keys`: whether the panel has the keys now (the window's pane, and AppKit agreeing).
    pub fn new(mtm: MainThreadMarker, panel_has_keys: impl Fn() -> bool + 'static) -> Retained<EditTarget> {
        let this = EditTarget::alloc(mtm).set_ivars(EditTargetIvars {
            panel_has_keys: Box::new(panel_has_keys),
        });
        // SAFETY: NSObject's `init` on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    }

    /// Enabled while the panel has the keys and the responder that would act enables it too.
    fn enabled(&self, item: &NSMenuItem) -> bool {
        if !(self.ivars().panel_has_keys)() {
            return false;
        }
        let Some(action) = item.action() else { return false };
        let app = NSApplication::sharedApplication(self.mtm());
        // SAFETY: AppKit's own lookup along the key window's responder chain, then the window and the app.
        let Some(target) = (unsafe { app.targetForAction_to_from(action, None, Some(item)) }) else {
            return false;
        };
        // SAFETY: the standard validation messages, each asked only of a target that answers it.
        unsafe {
            let responds = |selector: Sel| -> bool { msg_send![&*target, respondsToSelector: selector] };
            if responds(sel!(validateMenuItem:)) {
                msg_send![&*target, validateMenuItem: item]
            } else if responds(sel!(validateUserInterfaceItem:)) {
                msg_send![&*target, validateUserInterfaceItem: item]
            } else {
                true
            }
        }
    }

    fn relay(&self, action: Sel, sender: Option<&AnyObject>) {
        if !(self.ivars().panel_has_keys)() {
            return;
        }
        let app = NSApplication::sharedApplication(self.mtm());
        // SAFETY: a standard responder action sent along the responder chain, with the item as its sender.
        unsafe { app.sendAction_to_from(action, None, sender) };
    }
}

fn edit_menu_item(mtm: MainThreadMarker, target: &EditTarget) -> Retained<NSMenuItem> {
    let title = NSString::from_str("Edit");
    let menu = NSMenu::new(mtm);
    menu.setTitle(&title);
    for item in items() {
        let Some((title, action, key, shift)) = item else {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            continue;
        };
        let it = NSMenuItem::new(mtm);
        it.setTitle(&NSString::from_str(title));
        // SAFETY: a standard responder action, which the target implements, and the target outlives the menu
        // (the host keeps it for the process's life).
        unsafe {
            it.setAction(Some(action));
            it.setTarget(Some(target));
        }
        it.setKeyEquivalent(&NSString::from_str(key));
        let mut mask = NSEventModifierFlags::Command;
        if shift {
            mask |= NSEventModifierFlags::Shift;
        }
        it.setKeyEquivalentModifierMask(mask);
        menu.addItem(&it);
    }
    let top = NSMenuItem::new(mtm);
    top.setTitle(&title);
    top.setTag(EDIT_MENU_TAG);
    top.setSubmenu(Some(&menu));
    top
}

pub struct QuitTargetIvars {
    /// Whether the quit may go on now; `false` when the window held it (and asks).
    may_quit: Box<dyn Fn() -> bool>,
    original: RefCell<Option<(Retained<AnyObject>, Sel)>>,
}

define_class!(
    /// The Quit item's target: asks the window first, then does what the item did.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "EitriQuitMenuTarget"]
    #[ivars = QuitTargetIvars]
    pub struct QuitTarget;

    unsafe impl NSObjectProtocol for QuitTarget {}

    impl QuitTarget {
        #[unsafe(method(eitriQuit:))]
        fn quit(&self, sender: Option<&AnyObject>) {
            if !(self.ivars().may_quit)() {
                return;
            }
            let original = self.ivars().original.borrow().clone();
            let Some((target, action)) = original else { return };
            let app = NSApplication::sharedApplication(self.mtm());
            // SAFETY: the action and target the item carried before, sent as the item would have.
            unsafe { app.sendAction_to_from(action, Some(&target), sender) };
        }
    }
);

impl QuitTarget {
    pub fn new(mtm: MainThreadMarker, may_quit: impl Fn() -> bool + 'static) -> Retained<QuitTarget> {
        let this = QuitTarget::alloc(mtm).set_ivars(QuitTargetIvars {
            may_quit: Box::new(may_quit),
            original: RefCell::new(None),
        });
        // SAFETY: NSObject's `init` on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    }
}

/// The action of Neovide's Quit item.
fn neovide_quit_action() -> Sel {
    sel!(quit:)
}

fn our_quit_action() -> Sel {
    sel!(eitriQuit:)
}

/// What one [`keep_main_menu`] pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MenuPass {
    pub inserted_edit_menu: bool,
    /// New Window items removed.
    pub removed_new_window: usize,
    /// Another Edit menu is there (one with Paste), so none was added beside it.
    pub other_edit_menu: bool,
    /// Quit items pointed at the window's question.
    pub retargeted_quit: usize,
}

/// One pass over the app's current main menu: Neovide's New Window items go, its Quit item asks first, and the
/// Edit menu goes in unless it, or another Edit menu, is already there. A fresh Edit menu each time: a menu
/// that was the submenu of an item in a replaced main menu may still count that item as its parent, and a menu
/// is meant to have only one.
pub fn keep_main_menu(mtm: MainThreadMarker, target: &EditTarget, quit: &QuitTarget) -> MenuPass {
    let mut pass = MenuPass::default();
    let Some(main) = NSApplication::sharedApplication(mtm).mainMenu() else {
        return pass;
    };
    let mut ours = false;
    for top in main.itemArray().iter() {
        if top.tag() == EDIT_MENU_TAG {
            ours = true;
            continue;
        }
        let Some(submenu) = top.submenu() else { continue };
        let children: Vec<Retained<NSMenuItem>> = submenu.itemArray().iter().collect();
        for child in children {
            match child.action() {
                Some(action) if action == new_window_action() => {
                    submenu.removeItem(&child);
                    pass.removed_new_window += 1;
                }
                Some(action) if action == sel!(paste:) => pass.other_edit_menu = true,
                Some(action) if action == neovide_quit_action() => {
                    if let Some(original) = child.target() {
                        *quit.ivars().original.borrow_mut() = Some((original, action));
                        // SAFETY: an action the target implements; the target outlives the menu (the host keeps it).
                        unsafe {
                            child.setTarget(Some(quit));
                            child.setAction(Some(our_quit_action()));
                        }
                        pass.retargeted_quit += 1;
                    }
                }
                _ => {}
            }
        }
    }
    if !ours && !pass.other_edit_menu {
        main.insertItem_atIndex(&edit_menu_item(mtm, target), edit_menu_index(main.numberOfItems()));
        pass.inserted_edit_menu = true;
    }
    pass
}

/// Whether the current main menu holds the Edit menu.
pub fn edit_menu_present(mtm: MainThreadMarker) -> bool {
    NSApplication::sharedApplication(mtm)
        .mainMenu()
        .is_some_and(|main| main.itemWithTag(EDIT_MENU_TAG).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_edit_menu_follows_the_application_menu() {
        assert_eq!(edit_menu_index(0), 0);
        assert_eq!(edit_menu_index(1), 1);
        assert_eq!(edit_menu_index(3), 1);
    }

    #[test]
    fn the_edit_menu_has_the_standard_items_and_keys() {
        let items: Vec<_> = items()
            .into_iter()
            .flatten()
            .map(|(t, s, k, shift)| (t, s.name().to_str().unwrap().to_owned(), k, shift))
            .collect();
        assert_eq!(
            items,
            vec![
                ("Undo", "undo:".to_owned(), "z", false),
                ("Redo", "redo:".to_owned(), "z", true),
                ("Cut", "cut:".to_owned(), "x", false),
                ("Copy", "copy:".to_owned(), "c", false),
                ("Paste", "paste:".to_owned(), "v", false),
                ("Select All", "selectAll:".to_owned(), "a", false),
            ]
        );
        assert_ne!(EDIT_MENU_TAG, 0);
    }
}
