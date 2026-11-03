//! A shared native project picker; the controller owns its state and every action.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use eitri_core::project_chooser::{ChooserView, LineEv};
use gtk4::prelude::*;
use gtk4::{gdk, glib};

/// What a row asks for; the list shows sixteen of them at most.
const ROW_HEIGHT: i32 = 32;
const MOST_ROWS: i32 = 16;
/// The widest the name column asks to be: a long name yields to the path, which is what tells two like names apart.
const NAME_CHARS: i32 = 26;
/// The panel's distance from the top of the window, and the least it leaves below it.
const TOP_MARGIN: i32 = 48;
const BOTTOM_MARGIN: i32 = 16;
/// The line's class while the list has the keys: the theme hides the caret then.
const LIST_KEYS_CLASS: &str = "list-keys";

pub(crate) enum ChooserReport {
    Line(LineEv),
    Pick(usize),
    Dismiss,
    Folder(Option<PathBuf>),
}

pub(crate) struct ProjectChooserView {
    root: gtk4::Overlay,
    entry: gtk4::Entry,
    title: gtk4::Label,
    list: gtk4::ListBox,
    message: gtk4::Label,
    footer: gtk4::Label,
    filling: Rc<Cell<bool>>,
    /// The line has the keys; otherwise the list does and the entry takes no text.
    in_line: Rc<Cell<bool>>,
    scroll_pending: Rc<Cell<bool>>,
    panel: gtk4::Box,
    scroll: gtk4::ScrolledWindow,
    /// The names of the rows on show, so their paths start on one column.
    names: gtk4::SizeGroup,
    /// The frame tick that fits the panel again; it runs only while the chooser is up.
    tick: RefCell<Option<gtk4::TickCallbackId>>,
}

impl ProjectChooserView {
    pub(crate) fn new(report: Rc<dyn Fn(ChooserReport)>) -> ProjectChooserView {
        let root = gtk4::Overlay::new();
        root.add_css_class("project-chooser");
        root.set_hexpand(true);
        root.set_vexpand(true);
        root.set_visible(false);
        let catcher = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        catcher.set_hexpand(true);
        catcher.set_vexpand(true);
        root.set_child(Some(&catcher));
        let click = gtk4::GestureClick::new();
        click.set_button(1);
        click.connect_released({
            let report = report.clone();
            move |gesture, _, _, _| {
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                report(ChooserReport::Dismiss);
            }
        });
        catcher.add_controller(click);

        let panel = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        panel.add_css_class("project-chooser-panel");
        panel.set_halign(gtk4::Align::Center);
        panel.set_valign(gtk4::Align::Start);
        panel.set_margin_top(TOP_MARGIN);
        let title = capped_label();
        title.add_css_class("project-chooser-title");
        panel.append(&title);
        let entry = gtk4::Entry::new();
        entry.add_css_class("project-chooser-line");
        panel.append(&entry);
        let message = capped_label();
        message.add_css_class("project-chooser-message");
        panel.append(&message);
        let list = gtk4::ListBox::new();
        list.add_css_class("project-chooser-list");
        list.set_focusable(false);
        list.set_activate_on_single_click(true);
        list.set_selection_mode(gtk4::SelectionMode::Single);
        let scroll = gtk4::ScrolledWindow::new();
        scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scroll.set_propagate_natural_height(true);
        scroll.set_max_content_height(MOST_ROWS * ROW_HEIGHT);
        scroll.set_child(Some(&list));
        panel.append(&scroll);
        let footer = capped_label();
        footer.add_css_class("project-chooser-footer");
        panel.append(&footer);
        root.add_overlay(&panel);
        let scroll_pending = Rc::new(Cell::new(false));
        let filling = Rc::new(Cell::new(false));
        entry.connect_changed({
            let report = report.clone();
            let filling = filling.clone();
            move |entry| {
                if !filling.get() {
                    report(ChooserReport::Line(LineEv::Changed(entry.text().to_string())));
                }
            }
        });
        entry.connect_activate({
            let report = report.clone();
            move |_| report(ChooserReport::Line(LineEv::Enter))
        });
        // The default bubble phase leaves preedit keys to the entry's input method first.
        let in_line = Rc::new(Cell::new(false));
        let keys = gtk4::EventControllerKey::new();
        keys.connect_key_pressed({
            let report = report.clone();
            move |_, key, _, mods| {
                if let Some(ev) = mapped_key(key, mods) {
                    report(ChooserReport::Line(ev));
                    return glib::Propagation::Stop;
                }
                // Shift+Tab has no meaning here; left alone it would walk the window's focus away.
                if key == gdk::Key::ISO_Left_Tab {
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }
        });
        entry.add_controller(keys);
        // While the list has the keys every key is the list's: a plain printable one is a command and never the entry's
        // text, and any other is reported too (the entry would act on Backspace and the arrows), so the controller runs
        // ahead of the entry (capture) and answers only then.
        let list_keys = gtk4::EventControllerKey::new();
        list_keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        list_keys.connect_key_pressed({
            let report = report.clone();
            let in_line = in_line.clone();
            move |_, key, _, mods| match list_ev(key, mods) {
                Some(ev) if !in_line.get() => {
                    report(ChooserReport::Line(ev));
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        entry.add_controller(list_keys);
        let focus = gtk4::EventControllerFocus::new();
        focus.connect_leave({
            let report = report.clone();
            let root = root.downgrade();
            move |controller| {
                if !root.upgrade().is_some_and(|root| root.is_visible()) {
                    return;
                }
                // A window that lost activation takes the focus out of its widgets too, and that reports itself as
                // the window deactivating; the keys are still with the line, so no module took them.
                let deactivated = controller
                    .widget()
                    .and_then(|entry| entry.root())
                    .and_then(|window| window.downcast::<gtk4::Window>().ok())
                    .is_some_and(|window| !window.is_active());
                if !deactivated {
                    report(ChooserReport::Line(LineEv::KeysLost));
                }
            }
        });
        entry.add_controller(focus);
        list.connect_row_activated(move |_, row| report(ChooserReport::Pick(row.index() as usize)));
        ProjectChooserView {
            root,
            entry,
            title,
            list,
            message,
            footer,
            filling,
            in_line,
            scroll_pending,
            panel,
            scroll,
            names: gtk4::SizeGroup::new(gtk4::SizeGroupMode::Horizontal),
            tick: RefCell::new(None),
        }
    }

    /// Overlay children do not contribute to the window's minimum, so the frame tick fits the panel to the actual
    /// allocation and brings the chosen row into view. It runs while the chooser is up and nowhere else.
    fn start_tick(&self) {
        if self.tick.borrow().is_some() {
            return;
        }
        let panel = self.panel.downgrade();
        let scroll = self.scroll.downgrade();
        let list = self.list.downgrade();
        let scroll_pending = self.scroll_pending.clone();
        let id = self.root.add_tick_callback(move |root, _| {
            if let (Some(panel), Some(scroll)) = (panel.upgrade(), scroll.upgrade()) {
                let width = (root.width() - 32).clamp(0, 640);
                if panel.width_request() != width {
                    panel.set_size_request(width, -1);
                }
                // The panel's whole box: its own height leaves out the padding and the border. One not allocated yet
                // has none, which only means the first frame offers the most.
                let outer = panel
                    .compute_bounds(root)
                    .map_or(0, |bounds| bounds.height().ceil() as i32);
                let height = list_room(root.height(), outer - scroll.height());
                if scroll.max_content_height() != height {
                    scroll.set_max_content_height(height);
                }
                if scroll_pending.get() {
                    if let Some((list, row)) = list
                        .upgrade()
                        .and_then(|list| list.selected_row().map(|row| (list, row)))
                    {
                        if row.height() > 0 {
                            if let Some(bounds) = row.compute_bounds(&list) {
                                let adjustment = scroll.vadjustment();
                                let top = f64::from(bounds.y());
                                let bottom = top + f64::from(bounds.height());
                                adjustment.set_value(scrolled_to_show(
                                    top,
                                    bottom,
                                    adjustment.value(),
                                    adjustment.page_size(),
                                ));
                                scroll_pending.set(false);
                            }
                        }
                    } else {
                        scroll_pending.set(false);
                    }
                }
            }
            glib::ControlFlow::Continue
        });
        *self.tick.borrow_mut() = Some(id);
    }

    pub(crate) fn widget(&self) -> &gtk4::Widget {
        self.root.upcast_ref()
    }

    pub(crate) fn show(&self, view: Option<&ChooserView>) {
        let Some(view) = view else {
            self.root.set_visible(false);
            if let Some(id) = self.tick.borrow_mut().take() {
                id.remove();
            }
            self.entry.set_sensitive(true);
            self.list.set_sensitive(true);
            self.in_line.set(false);
            return;
        };
        self.filling.set(true);
        self.in_line.set(view.in_line);
        self.entry.set_editable(view.in_line);
        // The list has no caret: the line is not taking text.
        if view.in_line {
            self.entry.remove_css_class(LIST_KEYS_CLASS);
        } else {
            self.entry.add_css_class(LIST_KEYS_CLASS);
        }
        if self.entry.text().as_str() != view.line {
            self.entry.set_text(&view.line);
            self.entry.set_position(-1);
        }
        self.entry.set_placeholder_text(Some(&view.placeholder));
        set_text(&self.title, &view.title);
        set_text(&self.footer, &view.footer);
        set_text(&self.message, view.message.as_deref().unwrap_or(""));
        self.message.set_visible(view.message.is_some());
        while let Some(row) = self.list.first_child() {
            self.list.remove(&row);
        }
        for row in &view.rows {
            let item = gtk4::ListBoxRow::new();
            item.add_css_class("project-chooser-row");
            item.set_focusable(false);
            if row.dimmed {
                item.add_css_class("dimmed");
            }
            let contents = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
            contents.set_height_request(ROW_HEIGHT);
            let name = label();
            name.add_css_class("project-chooser-name");
            name.set_max_width_chars(NAME_CHARS);
            self.names.add_widget(&name);
            set_text(&name, &row.name);
            let shown = label();
            shown.add_css_class("project-chooser-path");
            shown.set_hexpand(true);
            shown.set_ellipsize(gtk4::pango::EllipsizeMode::Start);
            set_text(&shown, &row.shown);
            let right = label();
            right.add_css_class("project-chooser-right");
            right.set_xalign(1.0);
            set_text(&right, &row.right);
            contents.append(&name);
            contents.append(&shown);
            contents.append(&right);
            item.set_child(Some(&contents));
            self.list.append(&item);
        }
        self.list.select_row(
            view.selected
                .and_then(|index| self.list.row_at_index(index as i32))
                .as_ref(),
        );
        self.scroll_pending.set(true);
        self.entry.set_sensitive(!view.opening);
        self.list.set_sensitive(!view.opening);
        self.root.set_visible(true);
        self.start_tick();
        self.filling.set(false);
    }

    pub(crate) fn focus(&self) {
        self.root.set_visible(true);
        self.start_tick();
        self.entry.grab_focus();
    }
}

/// The character a plain printable key types: Shift counts as plain (`G`, `~`), Ctrl, Alt and Super do not. Enter, Esc,
/// Tab, Backspace and the arrows are not printable and keep their own meaning. A dead tilde (US-international and
/// similar layouts, where `~` is a dead key) is the `~` it will type.
fn list_key(key: gdk::Key, mods: gdk::ModifierType) -> Option<char> {
    let plain =
        !mods.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK);
    if key == gdk::Key::dead_tilde {
        return plain.then_some('~');
    }
    key.to_unicode().filter(|c| plain && !c.is_control())
}

/// What a key means in both states: the four step keys, `Ctrl+o`, `Tab` and `Esc`.
fn mapped_key(key: gdk::Key, mods: gdk::ModifierType) -> Option<LineEv> {
    let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
    let plain =
        !mods.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK);
    Some(match key {
        gdk::Key::Down if plain => LineEv::Step(1),
        gdk::Key::Up if plain => LineEv::Step(-1),
        gdk::Key::n | gdk::Key::j if ctrl => LineEv::Step(1),
        gdk::Key::p | gdk::Key::k if ctrl => LineEv::Step(-1),
        gdk::Key::o if ctrl => LineEv::ChooseFolder,
        gdk::Key::Tab if plain => LineEv::Tab,
        gdk::Key::Escape => LineEv::Escape,
        _ => return None,
    })
}

/// What the list hears of a key: a command for a plain printable one, the mapped keys, and `Other` for every key left
/// (it does nothing, and still ends a waiting `g`). `Enter` is the entry's own `activate`, and a bare modifier is no
/// key after the `g`: both are left alone (`None`).
fn list_ev(key: gdk::Key, mods: gdk::ModifierType) -> Option<LineEv> {
    if let Some(c) = list_key(key, mods) {
        return Some(LineEv::Key(c));
    }
    if let Some(ev) = mapped_key(key, mods) {
        return Some(ev);
    }
    if is_modifier(key) || matches!(key, gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::ISO_Enter) {
        return None;
    }
    Some(LineEv::Other)
}

fn is_modifier(key: gdk::Key) -> bool {
    use gdk::Key;
    [
        Key::Shift_L,
        Key::Shift_R,
        Key::Control_L,
        Key::Control_R,
        Key::Alt_L,
        Key::Alt_R,
        Key::Meta_L,
        Key::Meta_R,
        Key::Super_L,
        Key::Super_R,
        Key::Hyper_L,
        Key::Hyper_R,
        Key::Caps_Lock,
        Key::Shift_Lock,
        Key::Num_Lock,
        Key::Scroll_Lock,
        Key::ISO_Level3_Shift,
        Key::ISO_Level5_Shift,
        Key::Mode_switch,
    ]
    .contains(&key)
}

/// How tall the list may be in a window `height` tall, when the rest of the panel takes `chrome`.
fn list_room(height: i32, chrome: i32) -> i32 {
    (height - TOP_MARGIN - BOTTOM_MARGIN - chrome).clamp(0, MOST_ROWS * ROW_HEIGHT)
}

/// Where a list at `value` with `page` showing goes to show the row from `top` to `bottom`. A row taller than the page
/// shows its top.
fn scrolled_to_show(top: f64, bottom: f64, value: f64, page: f64) -> f64 {
    if top < value || bottom - top > page {
        top
    } else if bottom > value + page {
        bottom - page
    } else {
        value
    }
}

fn label() -> gtk4::Label {
    let label = gtk4::Label::new(None);
    label.set_xalign(0.0);
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    label
}

/// A label whose width never asks for more than the panel gives it: the panel is 640 px at most, whatever the text.
fn capped_label() -> gtk4::Label {
    let label = label();
    label.set_max_width_chars(1);
    label
}

fn set_text(label: &gtk4::Label, text: &str) {
    let text: String = text.chars().map(|c| if c.is_control() { '?' } else { c }).collect();
    label.set_text(&text);
}

#[cfg(test)]
mod tests {
    use super::{is_modifier, list_ev, list_key, list_room, scrolled_to_show, BOTTOM_MARGIN, ROW_HEIGHT, TOP_MARGIN};
    use eitri_core::project_chooser::LineEv;
    use eitri_core::source_scan as scan;
    use gtk4::gdk::{Key, ModifierType};

    fn code() -> String {
        let source = scan::read_required("shell/src/gtk_host/project_chooser.rs");
        let sources = [source];
        let (_, constructor) = scan::function_in(&sources, "new");
        let (_, show) = scan::function_in(&sources, "show");
        let (_, mapped) = scan::function_in(&sources, "mapped_key");
        let (_, list) = scan::function_in(&sources, "list_ev");
        format!("{constructor}{show}{mapped}{list}")
            .split_whitespace()
            .collect()
    }

    #[test]
    fn the_list_takes_what_the_window_leaves_up_to_sixteen_rows() {
        assert_eq!(list_room(900, 130), 16 * ROW_HEIGHT);
        let short = 400;
        assert_eq!(list_room(short, 130), short - TOP_MARGIN - BOTTOM_MARGIN - 130);
        assert_eq!(
            list_room(100, 130),
            0,
            "a window too short for the list has no room, never less"
        );
    }

    #[test]
    fn a_row_is_brought_into_view_by_the_least_move() {
        assert_eq!(scrolled_to_show(40.0, 72.0, 32.0, 80.0), 32.0, "already in view");
        assert_eq!(scrolled_to_show(0.0, 32.0, 32.0, 80.0), 0.0, "above the page");
        assert_eq!(scrolled_to_show(64.0, 96.0, 0.0, 80.0), 16.0, "below the page");
    }

    /// The list is at its shortest on the frame before the panel has been fitted, so a page shorter than a row is only
    /// a moment: moving by the row's bottom would leave the first row cut for good.
    #[test]
    fn a_page_shorter_than_the_row_shows_its_top() {
        assert_eq!(scrolled_to_show(0.0, 32.0, 0.0, 28.0), 0.0);
        assert_eq!(scrolled_to_show(32.0, 64.0, 0.0, 28.0), 32.0);
    }

    /// A word about the typed text stays beside the text, however many rows follow it, and the panel keeps its place as
    /// the rows come and go.
    #[test]
    fn the_message_is_under_the_line_and_the_panel_stands_near_the_top() {
        let code = code();
        let at = |what: &str| code.find(what).unwrap_or_else(|| panic!("no {what}"));
        assert!(at("panel.append(&entry)") < at("panel.append(&message)"));
        assert!(at("panel.append(&message)") < at("panel.append(&scroll)"));
        assert!(code.contains("panel.set_valign(gtk4::Align::Start)"));
        assert!(code.contains("panel.set_margin_top(TOP_MARGIN)"));
    }

    #[test]
    fn labels_are_set_as_text_never_markup() {
        let source = scan::read_required("shell/src/gtk_host/project_chooser.rs");
        let raw = scan::code_only(&source.text);
        let code = scan::without(&raw, &scan::modules(&raw, "tests"));
        assert!(!code.contains("set_markup"));
        assert!(!code.contains("set_use_markup"));
        let (_, setter) = scan::function_in(&[source], "set_text");
        assert!(setter.contains(".set_text("));
    }

    #[test]
    fn the_keys_are_read_on_the_entry_in_the_bubble_phase() {
        let code = code();
        assert!(code.contains("EventControllerKey::new()"));
        assert!(code.contains("entry.add_controller(keys)"));
        // Only the list's controller runs ahead of the entry; the line's stays in the bubble phase.
        assert_eq!(code.matches("set_propagation_phase(").count(), 1);
        assert!(code.contains("list_keys.set_propagation_phase(gtk4::PropagationPhase::Capture)"));
        assert!(code.contains("Some(ev)if!in_line.get()=>"));
        assert!(code.contains("LineEv::Key(c)"));
        assert!(code.contains("entry.set_editable(view.in_line)"));
        assert!(code.contains("entry.add_css_class(LIST_KEYS_CLASS)"));
        assert_eq!(super::LIST_KEYS_CLASS, "list-keys");
        assert!(code.contains("LineEv::Other"));
        assert!(code.contains("entry.connect_activate("));
        assert!(code.contains("LineEv::Step("));
        assert!(code.contains("LineEv::Tab"));
        assert!(code.contains("LineEv::Escape"));
        assert!(code.contains("LineEv::ChooseFolder"));
        assert!(code.contains("LineEv::Enter"));
    }

    #[test]
    fn a_bare_modifier_is_no_key_after_a_waiting_g() {
        for key in [
            Key::Shift_L,
            Key::Control_R,
            Key::Alt_L,
            Key::Super_L,
            Key::Caps_Lock,
            Key::ISO_Level3_Shift,
        ] {
            assert!(is_modifier(key), "{key:?}");
        }
        for key in [Key::BackSpace, Key::Left, Key::Home, Key::F1, Key::j, Key::Delete] {
            assert!(!is_modifier(key), "{key:?}");
        }
    }

    #[test]
    fn a_dead_tilde_in_the_list_is_a_tilde() {
        let none = ModifierType::empty();
        for mods in [none, ModifierType::SHIFT_MASK] {
            assert_eq!(list_key(Key::dead_tilde, mods), Some('~'), "{mods:?}");
            assert_eq!(list_ev(Key::dead_tilde, mods), Some(LineEv::Key('~')), "{mods:?}");
        }
        for mods in [
            ModifierType::CONTROL_MASK,
            ModifierType::ALT_MASK,
            ModifierType::SUPER_MASK,
        ] {
            assert_eq!(list_key(Key::dead_tilde, mods), None, "{mods:?}");
        }
        for dead in [Key::dead_acute, Key::dead_grave, Key::dead_circumflex] {
            assert_ne!(list_ev(dead, none), Some(LineEv::Key('~')), "{dead:?} is not a tilde");
        }
    }

    #[test]
    fn every_key_in_the_list_is_a_command_or_other_but_enter_and_modifiers() {
        let none = ModifierType::empty();
        let ctrl = ModifierType::CONTROL_MASK;
        for (key, mods, expected) in [
            (Key::j, none, Some(LineEv::Key('j'))),
            (Key::G, ModifierType::SHIFT_MASK, Some(LineEv::Key('G'))),
            (Key::j, ctrl, Some(LineEv::Step(1))),
            (Key::k, ctrl, Some(LineEv::Step(-1))),
            (Key::n, ctrl, Some(LineEv::Step(1))),
            (Key::p, ctrl, Some(LineEv::Step(-1))),
            (Key::Down, none, Some(LineEv::Step(1))),
            (Key::Up, none, Some(LineEv::Step(-1))),
            (Key::o, ctrl, Some(LineEv::ChooseFolder)),
            (Key::Tab, none, Some(LineEv::Tab)),
            (Key::Escape, none, Some(LineEv::Escape)),
            (Key::BackSpace, none, Some(LineEv::Other)),
            (Key::Delete, none, Some(LineEv::Other)),
            (Key::Left, none, Some(LineEv::Other)),
            (Key::Home, none, Some(LineEv::Other)),
            (Key::F1, none, Some(LineEv::Other)),
            (Key::g, ctrl, Some(LineEv::Other)),
            (Key::ISO_Left_Tab, ModifierType::SHIFT_MASK, Some(LineEv::Other)),
            (Key::Return, none, None),
            (Key::KP_Enter, none, None),
            (Key::Shift_L, ModifierType::SHIFT_MASK, None),
            (Key::Control_L, ctrl, None),
        ] {
            assert_eq!(list_ev(key, mods), expected, "{key:?} {mods:?}");
        }
    }

    #[test]
    fn the_list_takes_plain_printable_keys_and_leaves_the_rest() {
        let none = ModifierType::empty();
        for (key, mods, expected) in [
            (Key::j, none, Some('j')),
            (Key::k, none, Some('k')),
            (Key::g, none, Some('g')),
            (Key::G, ModifierType::SHIFT_MASK, Some('G')),
            (Key::slash, none, Some('/')),
            (Key::q, none, Some('q')),
            (Key::asciitilde, ModifierType::SHIFT_MASK, Some('~')),
            (Key::x, none, Some('x')),
            (Key::space, none, Some(' ')),
            (Key::j, ModifierType::CONTROL_MASK, None),
            (Key::o, ModifierType::CONTROL_MASK, None),
            (Key::g, ModifierType::ALT_MASK, None),
            (Key::g, ModifierType::SUPER_MASK, None),
            (Key::Return, none, None),
            (Key::Escape, none, None),
            (Key::Tab, none, None),
            (Key::BackSpace, none, None),
            (Key::Down, none, None),
            (Key::Shift_L, ModifierType::SHIFT_MASK, None),
        ] {
            assert_eq!(list_key(key, mods), expected, "{key:?} {mods:?}");
        }
    }
}
