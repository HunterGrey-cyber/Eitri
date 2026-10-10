//! The main menu kept the way the panel needs it, against a menu shaped like Neovide's, as Neovide replaces it
//! on the window's first focus:
//! - the Edit menu goes in after the application menu, once, and again into a replaced main menu;
//! - Neovide's New Window item is taken out in the same pass;
//! - beside another Edit menu (one with Paste) none is added;
//! - the Edit items are disabled while the panel does not have the keys, and do nothing if sent anyway;
//! - Neovide's Quit item asks the window first, and does what it did only when the window lets it.
//!
//! No window is made; only the application's main menu, which a process with no window never shows. AppKit
//! wants the main thread, so this file is its own `main`.

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    use std::cell::Cell;
    use std::rc::Rc;

    use eitri_mac::appkit::menu::{
        edit_menu_present, keep_main_menu, menu_settled, EditTarget, QuitTarget, EDIT_MENU_TAG,
    };
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Bool, NSObject};
    use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem};
    use objc2_foundation::NSString;

    let mtm = MainThreadMarker::new().expect("a test without the harness runs on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    let mut failed = 0;
    let mut check = |name: &str, ok: bool| {
        println!("{} {name}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            failed += 1;
        }
    };

    thread_local! {
        static QUITS: Cell<u32> = const { Cell::new(0) };
    }
    define_class!(
        // Stands in for Neovide's own Quit handler: counts the quits it is sent.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "EitriTestQuitHandler"]
        struct NeovideQuit;

        impl NeovideQuit {
            #[unsafe(method(quit:))]
            fn quit(&self, _sender: Option<&AnyObject>) {
                QUITS.with(|q| q.set(q.get() + 1));
            }
        }
    );
    let neovide_quit: Retained<NeovideQuit> = unsafe { msg_send![NeovideQuit::alloc(mtm), init] };
    let may_quit = Rc::new(Cell::new(false));
    let quit_target = {
        let may_quit = may_quit.clone();
        QuitTarget::new(mtm, move || may_quit.get())
    };

    let panel_has_keys = Rc::new(Cell::new(false));
    let target = {
        let panel_has_keys = panel_has_keys.clone();
        EditTarget::new(mtm, move || panel_has_keys.get())
    };

    // A main menu shaped like Neovide's: the application menu, Window (with New Window), Help.
    let neovides_menu = || {
        let main = NSMenu::new(mtm);
        for title in ["neovide", "Window", "Help"] {
            let item = NSMenuItem::new(mtm);
            item.setTitle(&NSString::from_str(title));
            let submenu = NSMenu::new(mtm);
            if title == "neovide" {
                let quit = NSMenuItem::new(mtm);
                quit.setTitle(&NSString::from_str("Quit neovide"));
                // SAFETY: the stand-in handler implements `quit:`, and lives to the end of the test.
                unsafe {
                    quit.setAction(Some(sel!(quit:)));
                    quit.setTarget(Some(&neovide_quit));
                }
                submenu.addItem(&quit);
            }
            if title == "Window" {
                for (name, action) in [
                    ("New Window", sel!(neovideCreateWindow:)),
                    ("Minimize", sel!(performMiniaturize:)),
                ] {
                    let child = NSMenuItem::new(mtm);
                    child.setTitle(&NSString::from_str(name));
                    // SAFETY: an action selector on a fresh item.
                    unsafe { child.setAction(Some(action)) };
                    submenu.addItem(&child);
                }
            }
            item.setSubmenu(Some(&submenu));
            main.addItem(&item);
        }
        main
    };
    let titles = || -> Vec<String> {
        let main = app.mainMenu().unwrap();
        (0..main.numberOfItems())
            .map(|i| main.itemAtIndex(i).unwrap().title().to_string())
            .collect()
    };
    let window_items = || -> Vec<String> {
        let main = app.mainMenu().unwrap();
        let window = main
            .itemWithTitle(&NSString::from_str("Window"))
            .unwrap()
            .submenu()
            .unwrap();
        (0..window.numberOfItems())
            .map(|i| window.itemAtIndex(i).unwrap().title().to_string())
            .collect()
    };

    let pass = keep_main_menu(mtm, &target, &quit_target);
    check(
        "with no main menu there is nothing to do",
        !pass.inserted_edit_menu && pass.removed_new_window == 0,
    );
    check(
        "and the host's tick keeps waiting for Neovide's menu",
        !menu_settled(0, &pass),
    );

    app.setMainMenu(Some(&neovides_menu()));
    let pass = keep_main_menu(mtm, &target, &quit_target);
    check("the Edit menu goes into the first main menu", pass.inserted_edit_menu);
    check(
        "Neovide's menu seen (its Quit retargeted) and the Edit menu in: the host's tick may stop",
        pass.retargeted_quit == 1 && menu_settled(pass.retargeted_quit, &pass),
    );
    check(
        "after the application menu",
        titles() == ["neovide", "Edit", "Window", "Help"],
    );
    check(
        "New Window is taken out, the rest of the Window menu stays",
        pass.removed_new_window == 1 && window_items() == ["Minimize"],
    );
    let pass = keep_main_menu(mtm, &target, &quit_target);
    check(
        "a second pass does nothing",
        !pass.inserted_edit_menu && pass.removed_new_window == 0 && titles().len() == 4,
    );

    // Neovide builds its menu again: a new main menu without it.
    app.setMainMenu(Some(&neovides_menu()));
    check("a replaced main menu lacks it", !edit_menu_present(mtm));
    let pass = keep_main_menu(mtm, &target, &quit_target);
    check(
        "the next pass puts it back and takes New Window out again",
        pass.inserted_edit_menu && pass.removed_new_window == 1,
    );
    check(
        "once, in the same place",
        titles() == ["neovide", "Edit", "Window", "Help"],
    );
    let edit = app
        .mainMenu()
        .unwrap()
        .itemWithTag(EDIT_MENU_TAG)
        .unwrap()
        .submenu()
        .unwrap();
    let items: Vec<_> = (0..edit.numberOfItems())
        .map(|i| edit.itemAtIndex(i).unwrap())
        .collect();
    let titled: Vec<String> = items
        .iter()
        .map(|i| i.title().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    check(
        "with the standard items",
        titled.starts_with(&["Undo", "Redo", "Cut", "Copy", "Paste", "Select All"].map(String::from)),
    );

    // The Quit item asks first.
    let app_menu = app.mainMenu().unwrap().itemAtIndex(0).unwrap().submenu().unwrap();
    let quit_item = app_menu.itemAtIndex(0).unwrap();
    check(
        "the Quit item now asks the window first",
        quit_item.action() == Some(sel!(eitriQuit:))
            && quit_item
                .target()
                .is_some_and(|t| t.class().name().to_str() == Ok("EitriQuitMenuTarget")),
    );
    let choose = || {
        // SAFETY: the item's own action and target, as AppKit sends them.
        unsafe {
            app.sendAction_to_from(
                quit_item.action().unwrap(),
                quit_item.target().as_deref(),
                Some(&quit_item),
            )
        };
    };
    choose();
    check(
        "held by the window: Neovide's quit is not sent",
        QUITS.with(|q| q.get()) == 0,
    );
    may_quit.set(true);
    choose();
    check(
        "let go by the window: Neovide's quit is sent once",
        QUITS.with(|q| q.get()) == 1,
    );
    let pass = keep_main_menu(mtm, &target, &quit_target);
    check(
        "a pass over a retargeted Quit changes nothing",
        pass.retargeted_quit == 0,
    );
    check(
        "and stays settled on the count so far",
        pass.edit_menu_present && menu_settled(1, &pass) && !menu_settled(0, &pass),
    );

    // The items' target: disabled while the panel does not have the keys.
    let ours: Vec<_> = items.iter().filter(|i| i.action().is_some()).take(6).collect();
    // SAFETY: the standard validation message, on the target the items name.
    let enabled = |item: &NSMenuItem| -> bool {
        let answer: Bool = unsafe { msg_send![&*target, validateMenuItem: item] };
        answer.as_bool()
    };
    check(
        "every Edit item names the target",
        ours.len() == 6 && ours.iter().all(|i| i.target().is_some()),
    );
    check(
        "disabled while the editor has the keys",
        ours.iter().all(|i| !enabled(i)),
    );
    // With the panel holding the keys but no window, no responder would act: still disabled.
    panel_has_keys.set(true);
    check(
        "disabled while nothing in a window would act",
        ours.iter().all(|i| !enabled(i)),
    );

    // Beside another Edit menu none is added.
    let other = neovides_menu();
    let edit_item = NSMenuItem::new(mtm);
    edit_item.setTitle(&NSString::from_str("Edit"));
    let edit_menu = NSMenu::new(mtm);
    let paste = NSMenuItem::new(mtm);
    // SAFETY: an action selector on a fresh item.
    unsafe { paste.setAction(Some(sel!(paste:))) };
    edit_menu.addItem(&paste);
    edit_item.setSubmenu(Some(&edit_menu));
    other.insertItem_atIndex(&edit_item, 1);
    app.setMainMenu(Some(&other));
    let pass = keep_main_menu(mtm, &target, &quit_target);
    check(
        "beside another Edit menu none is added",
        !pass.inserted_edit_menu && pass.other_edit_menu && !edit_menu_present(mtm),
    );

    println!("{failed} failed");
    std::process::exit(if failed == 0 { 0 } else { 1 });
}
