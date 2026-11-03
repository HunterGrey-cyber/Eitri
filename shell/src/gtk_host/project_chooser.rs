//! A shared native project picker; the controller owns its state and every action.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use eitri_core::project_chooser::{ChooserView, LineEv, RowKind, RowView};
use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};

/// What a row asks for; the list shows sixteen of them at most.
const ROW_HEIGHT: i32 = 32;
const MOST_ROWS: i32 = 16;
/// The name column's width, fixed, so one number aligns the column instead of every row being measured for it. The
/// count is measured in the name's own font, so every row's name keeps one weight. A row with nothing in the middle
/// column gives its name the whole row.
const NAME_CHARS: i32 = 20;
/// The panel's distance from the top of the window, and the least it leaves below it.
const TOP_MARGIN: i32 = 48;
const BOTTOM_MARGIN: i32 = 16;
/// The line's class while the list has the keys: the theme hides the caret then.
const LIST_KEYS_CLASS: &str = "list-keys";
/// A row's own box, inside the list's row node (which carries the selection).
const ROW_CLASS: &str = "project-chooser-row";
/// The panel's class while it shows a folder's subfolders instead of the projects.
const BROWSING_CLASS: &str = "browsing";
/// Every class a row's kind can put on its box: a recycled row drops the one it had.
const KIND_CLASSES: [&str; 7] = ["project", "action", "here", "folder", "file", "link", "mount"];

pub(crate) enum ChooserReport {
    Line(LineEv),
    Pick { index: usize, rows_rev: u64 },
    PickTwice { index: usize, rows_rev: u64 },
    Dismiss,
    Folder(Option<PathBuf>),
}

/// One item of the store: the rows the model drew, shared, and which of them this is. Making one copies no text.
struct RowItem {
    rows: Arc<[RowView]>,
    index: usize,
}

/// The row widgets alive now and the list items they show, so a click can tell which row it landed on.
type RowWidgets = Rc<RefCell<Vec<(glib::WeakRef<gtk4::Widget>, glib::WeakRef<gtk4::ListItem>)>>>;

pub(crate) struct ProjectChooserView {
    root: gtk4::Overlay,
    entry: gtk4::Entry,
    title: gtk4::Label,
    /// Widgets for the rows on screen only, made and recycled by the factory.
    list: gtk4::ListView,
    store: gio::ListStore,
    selection: gtk4::SingleSelection,
    message_line: gtk4::Box,
    message: gtk4::Label,
    /// The folder a message is about, in its own label so it gives way before the words do.
    message_path: gtk4::Label,
    footer: gtk4::Label,
    filling: Rc<Cell<bool>>,
    /// The line has the keys; otherwise the list does and the entry takes no text.
    in_line: Rc<Cell<bool>>,
    rows_rev: Rc<Cell<u64>>,
    /// The revision of the rows the store holds; `None` once the chooser is hidden.
    shown_rev: Cell<Option<u64>>,
    /// The row last brought into view, and the revision it was in.
    shown_at: Cell<Option<(u64, usize)>>,
    panel: gtk4::Box,
    scroll: gtk4::ScrolledWindow,
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
        let message_line = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        message_line.add_css_class("project-chooser-message-line");
        let message = label();
        message.add_css_class("project-chooser-message");
        let message_path = capped_label();
        message_path.add_css_class("project-chooser-message-path");
        message_path.set_ellipsize(gtk4::pango::EllipsizeMode::Start);
        message_path.set_hexpand(true);
        message_line.append(&message);
        message_line.append(&message_path);
        panel.append(&message_line);
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let selection = gtk4::SingleSelection::new(Some(store.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);
        let row_widgets: RowWidgets = Rc::default();
        let list = gtk4::ListView::new(Some(selection.clone()), Some(row_factory(row_widgets.clone())));
        list.add_css_class("project-chooser-list");
        // The line keeps the keys: nothing in the list takes them, by a click or otherwise.
        list.set_can_focus(false);
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
                if let Some(ev) = line_key(key, mods) {
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
        let rows_rev = Rc::new(Cell::new(0));
        list.add_controller(row_clicks(row_widgets, rows_rev.clone(), report));
        ProjectChooserView {
            root,
            entry,
            title,
            list,
            store,
            selection,
            message_line,
            message,
            message_path,
            footer,
            filling,
            in_line,
            rows_rev,
            shown_rev: Cell::new(None),
            shown_at: Cell::new(None),
            panel,
            scroll,
            tick: RefCell::new(None),
        }
    }

    /// Overlay children do not contribute to the window's minimum, so the frame tick fits the panel to the actual
    /// allocation. It runs while the chooser is up and nowhere else.
    fn start_tick(&self) {
        if self.tick.borrow().is_some() {
            return;
        }
        let panel = self.panel.downgrade();
        let scroll = self.scroll.downgrade();
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
            // The next opening fills the store afresh; nothing of this one is kept.
            self.store.remove_all();
            self.shown_rev.set(None);
            self.shown_at.set(None);
            return;
        };
        self.filling.set(true);
        self.in_line.set(view.in_line);
        self.rows_rev.set(view.rows_rev);
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
        match view.message.as_deref().map(split_message) {
            None => self.message_line.set_visible(false),
            Some((words, path)) => {
                set_text(&self.message, words);
                // Words beside a folder keep their whole width and the folder gives way at its start; words alone are
                // held to the panel's width, as every other line of it is.
                self.message.set_max_width_chars(if path.is_some() { -1 } else { 1 });
                self.message.set_hexpand(path.is_none());
                if let Some(path) = path {
                    set_value(&self.message_path, path);
                }
                self.message_path.set_visible(path.is_some());
                self.message_line.set_visible(true);
            }
        }
        if view.browsing {
            self.panel.add_css_class(BROWSING_CLASS);
        } else {
            self.panel.remove_css_class(BROWSING_CLASS);
        }
        // New rows replace the store's items; a move of the cursor is only a selection and a scroll.
        let mut spliced = false;
        if self.shown_rev.get() != Some(view.rows_rev) {
            let items: Vec<glib::BoxedAnyObject> = (0..view.rows.len())
                .map(|index| {
                    glib::BoxedAnyObject::new(RowItem {
                        rows: view.rows.clone(),
                        index,
                    })
                })
                .collect();
            self.store.splice(0, self.store.n_items(), &items);
            self.shown_rev.set(Some(view.rows_rev));
            spliced = true;
        }
        let selected = view.selected.filter(|index| *index < view.rows.len());
        let position = selected.map_or(gtk4::INVALID_LIST_POSITION, |index| index as u32);
        if self.selection.selected() != position {
            self.selection.set_selected(position);
        }
        self.entry.set_sensitive(!view.opening);
        self.list.set_sensitive(!view.opening);
        self.root.set_visible(true);
        self.start_tick();
        // The cursor's row comes into view by the least scroll; new rows with no cursor start at the top.
        let at = selected.map(|index| (view.rows_rev, index));
        if spliced || at != self.shown_at.get() {
            match selected {
                Some(index) => self.list.scroll_to(index as u32, gtk4::ListScrollFlags::NONE, None),
                None => self.scroll.vadjustment().set_value(0.0),
            }
            self.shown_at.set(at);
        }
        self.filling.set(false);
    }

    pub(crate) fn focus(&self) {
        self.root.set_visible(true);
        self.start_tick();
        self.entry.grab_focus();
    }
}

/// The rows' factory: a row's widgets are made once and filled again each time the row shows another item.
fn row_factory(widgets: RowWidgets) -> gtk4::SignalListItemFactory {
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk4::ListItem>() else {
            return;
        };
        item.set_focusable(false);
        item.set_activatable(false);
        let row = row_widget();
        let mut widgets = widgets.borrow_mut();
        widgets.retain(|(widget, _)| widget.upgrade().is_some());
        widgets.push((row.upcast_ref::<gtk4::Widget>().downgrade(), item.downgrade()));
        item.set_child(Some(&row));
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk4::ListItem>() else {
            return;
        };
        let (Some(widget), Some(object)) = (item.child(), item.item()) else {
            return;
        };
        let Ok(object) = object.downcast::<glib::BoxedAnyObject>() else {
            return;
        };
        let item = object.borrow::<RowItem>();
        if let Some(row) = item.rows.get(item.index) {
            fill_row(&widget, row);
        }
    });
    factory
}

/// A row: its name, the middle column (a project's path, the `.` row's folder, a link's target) and its right-hand
/// words.
fn row_widget() -> gtk4::Box {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    row.add_css_class(ROW_CLASS);
    row.set_height_request(ROW_HEIGHT);
    let name = label();
    name.add_css_class("project-chooser-name");
    let shown = capped_label();
    shown.add_css_class("project-chooser-path");
    shown.set_hexpand(true);
    // The right-hand words end on one edge and take only their own width: an age or `open` leaves the path its room.
    let right = label();
    right.add_css_class("project-chooser-right");
    right.set_ellipsize(gtk4::pango::EllipsizeMode::None);
    right.set_xalign(1.0);
    row.append(&name);
    row.append(&shown);
    row.append(&right);
    row
}

fn fill_row(widget: &gtk4::Widget, row: &RowView) {
    let labels: Vec<gtk4::Label> = std::iter::successors(widget.first_child(), |child| child.next_sibling())
        .filter_map(|child| child.downcast::<gtk4::Label>().ok())
        .collect();
    let Ok([name, shown, right]) = <[gtk4::Label; 3]>::try_from(labels) else {
        return;
    };
    for class in KIND_CLASSES {
        widget.remove_css_class(class);
    }
    widget.add_css_class(kind_class(row.kind));
    if row.dimmed {
        widget.add_css_class("dimmed");
    } else {
        widget.remove_css_class("dimmed");
    }
    set_value(&name, &row.name);
    if row.shown.is_empty() {
        // Nothing in the middle: the name has the row.
        name.set_hexpand(true);
        name.set_width_chars(-1);
        name.set_max_width_chars(1);
        shown.set_text("");
    } else {
        name.set_hexpand(false);
        name.set_width_chars(NAME_CHARS);
        name.set_max_width_chars(NAME_CHARS);
        // A path keeps its end, the folder itself; a link keeps its arrow and its target's last part.
        shown.set_ellipsize(match row.kind {
            RowKind::Link => gtk4::pango::EllipsizeMode::Middle,
            _ => gtk4::pango::EllipsizeMode::Start,
        });
        set_value(&shown, &row.shown);
    }
    shown.set_visible(!row.shown.is_empty());
    set_text(&right, &row.right);
}

fn kind_class(kind: RowKind) -> &'static str {
    match kind {
        RowKind::Project => "project",
        RowKind::Action => "action",
        RowKind::Here => "here",
        RowKind::Folder => "folder",
        RowKind::File => "file",
        RowKind::Link => "link",
        RowKind::Mount => "mount",
    }
}

/// Clicks on the rows. The gesture runs ahead of the rows' own (capture) and claims every press, so GTK neither selects
/// nor activates a row by itself: the model holds the cursor. A click reports `Pick` when it is released on the row it
/// pressed, as a button's does: a press let go elsewhere, or cancelled, picks nothing. A double-click reports
/// `PickTwice` with the row and the revision its first press saw, kept here, because that first click may already have
/// changed the rows.
fn row_clicks(widgets: RowWidgets, rows_rev: Rc<Cell<u64>>, report: Rc<dyn Fn(ChooserReport)>) -> gtk4::GestureClick {
    let click = gtk4::GestureClick::new();
    click.set_button(1);
    click.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let first: Rc<Cell<Option<(usize, u64)>>> = Rc::default();
    click.connect_pressed({
        let first = first.clone();
        let widgets = widgets.clone();
        let report = report.clone();
        move |gesture, n_press, x, y| {
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            match n_press {
                1 => first.set(
                    gesture
                        .widget()
                        .and_then(|list| row_at(&list, &widgets, x, y))
                        .map(|index| (index, rows_rev.get())),
                ),
                2 => {
                    if let Some((index, rows_rev)) = first.get() {
                        report(ChooserReport::PickTwice { index, rows_rev });
                    }
                }
                _ => {}
            }
        }
    });
    // A grab taken from the gesture ends its press without a click.
    click.connect_cancel({
        let first = first.clone();
        move |_, _| first.set(None)
    });
    // GTK still reports the release of a press that moved away (the click is stopped, `n_press` stays 1), at the
    // pointer's place: the row under it decides. Nothing has redrawn the rows between the press and this release.
    click.connect_released(move |gesture, n_press, x, y| {
        if n_press != 1 {
            return;
        }
        let Some((index, rows_rev)) = first.get() else {
            return;
        };
        if gesture.widget().and_then(|list| row_at(&list, &widgets, x, y)) == Some(index) {
            report(ChooserReport::Pick { index, rows_rev });
        }
    });
    click
}

/// The position of the row under `(x, y)` in the list's own coordinates, if a row is there. The list's own row widget
/// holds ours as its child and draws the highlight with its padding around it, so a press in that padding is on the row
/// too.
fn row_at(list: &gtk4::Widget, widgets: &RowWidgets, x: f64, y: f64) -> Option<usize> {
    let mut at = list.pick(x, y, gtk4::PickFlags::DEFAULT);
    while let Some(widget) = at {
        if &widget == list {
            return None;
        }
        let parent = widget.parent();
        let row = if parent.as_ref() == Some(list) {
            widget.first_child()
        } else {
            Some(widget)
        };
        if let Some(row) = row.filter(|row| row.has_css_class(ROW_CLASS)) {
            let item = widgets
                .borrow()
                .iter()
                .find(|(widget, _)| widget.upgrade().as_ref() == Some(&row))
                .and_then(|(_, item)| item.upgrade())?;
            let position = item.position();
            return (position != gtk4::INVALID_LIST_POSITION).then_some(position as usize);
        }
        at = parent;
    }
    None
}

/// A message about a folder carries it after a tab: the words, then the folder, each for its own label. The folder
/// came through the model's display escapes, which show a tab in a name as an escape, so the first tab is the only one.
fn split_message(message: &str) -> (&str, Option<&str>) {
    match message.split_once('\t') {
        Some((words, path)) => (words, Some(path).filter(|path| !path.is_empty())),
        None => (message, None),
    }
}

/// The character a plain printable key types: Shift counts as plain (`G`, `~`), Ctrl, Alt and Super do not. Enter, Esc,
/// Tab, Backspace and the arrows are not printable and keep their own meaning. A dead tilde (US-international and
/// similar layouts, where `~` is a dead key) is the `~` it will type.
fn list_key(key: gdk::Key, mods: gdk::ModifierType) -> Option<char> {
    if key == gdk::Key::dead_tilde {
        return plain(mods).then_some('~');
    }
    key.to_unicode().filter(|c| plain(mods) && !c.is_control())
}

/// No Ctrl, Alt or Super: Shift alone still makes a plain key.
fn plain(mods: gdk::ModifierType) -> bool {
    !mods.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK)
}

/// What a key means in both states: the four step keys, `Ctrl+o`, `Tab` and `Esc`.
fn mapped_key(key: gdk::Key, mods: gdk::ModifierType) -> Option<LineEv> {
    let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
    let plain = plain(mods);
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

/// What a key means while the line has the keys: the keys of both states, and `Ctrl+w`, which deletes back to the
/// previous `/` as readline's does. Backspace and the arrows stay the entry's own editing.
fn line_key(key: gdk::Key, mods: gdk::ModifierType) -> Option<LineEv> {
    let ctrl_only = mods.contains(gdk::ModifierType::CONTROL_MASK)
        && !mods.intersects(gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK);
    if ctrl_only && matches!(key, gdk::Key::w | gdk::Key::W) {
        return Some(LineEv::DeleteSegment);
    }
    mapped_key(key, mods)
}

/// What the list hears of a key: a command for a plain printable one, the mapped keys, Backspace as up and the side
/// arrows as out and in, and `Other` for every key left (it does nothing, and still ends a waiting `g`). `Enter` is the
/// entry's own `activate`, and a bare modifier is no key after the `g`: both are left alone (`None`).
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
    let plain = plain(mods);
    Some(match key {
        gdk::Key::BackSpace if plain => LineEv::Up,
        gdk::Key::Left | gdk::Key::KP_Left if plain => LineEv::Side(-1),
        gdk::Key::Right | gdk::Key::KP_Right if plain => LineEv::Side(1),
        _ => LineEv::Other,
    })
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

/// Every label of the chooser is laid out left to right, whatever language its text is in.
fn label() -> gtk4::Label {
    let label = gtk4::Label::new(None);
    label.set_xalign(0.0);
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    label.set_direction(gtk4::TextDirection::Ltr);
    label
}

/// A label whose width never asks for more than the panel gives it: the panel is 640 px at most, whatever the text.
fn capped_label() -> gtk4::Label {
    let label = label();
    label.set_max_width_chars(1);
    label
}

/// A control character shows as `?`: a label is one line of text.
fn printable(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { '?' } else { c }).collect()
}

fn set_text(label: &gtk4::Label, text: &str) {
    label.set_text(&printable(text));
}

/// A value (a name, a path) as a label shows it: after a left-to-right mark, so the label is laid out left to right
/// whatever the value's first letter, and inside a first-strong isolate, so a right-to-left value keeps its own order
/// without reordering the words beside it. Only labels take this; the line is what the person edits, and never does.
fn isolated(value: &str) -> String {
    format!("\u{200e}\u{2068}{value}\u{2069}")
}

fn set_value(label: &gtk4::Label, value: &str) {
    label.set_text(&isolated(&printable(value)));
}

#[cfg(test)]
mod tests {
    use super::{
        is_modifier, isolated, line_key, list_ev, list_key, list_room, split_message, BOTTOM_MARGIN, ROW_HEIGHT,
        TOP_MARGIN,
    };
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

    /// A word about the typed text stays beside the text, however many rows follow it, and the panel keeps its place as
    /// the rows come and go.
    #[test]
    fn the_message_is_under_the_line_and_the_panel_stands_near_the_top() {
        let code = code();
        let at = |what: &str| code.find(what).unwrap_or_else(|| panic!("no {what}"));
        assert!(at("panel.append(&entry)") < at("panel.append(&message_line)"));
        assert!(at("panel.append(&message_line)") < at("panel.append(&scroll)"));
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
            (Key::BackSpace, none, Some(LineEv::Up)),
            (Key::Delete, none, Some(LineEv::Other)),
            (Key::Left, none, Some(LineEv::Side(-1))),
            (Key::Right, none, Some(LineEv::Side(1))),
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

    /// The file's code with comments, literals and tests blanked, as one string without whitespace.
    fn whole() -> String {
        let source = scan::read_required("shell/src/gtk_host/project_chooser.rs");
        let raw = scan::code_only(&source.text);
        scan::without(&raw, &scan::modules(&raw, "tests"))
    }

    fn compact(code: &str) -> String {
        code.split_whitespace().collect()
    }

    /// Only the rows on screen have widgets: a folder of two thousand names builds a page of rows, not two thousand.
    #[test]
    fn the_rows_are_a_list_view_over_a_list_store() {
        let code = compact(&whole());
        assert!(code.contains("gtk4::ListView::new("), "the rows are a ListView");
        assert!(code.contains("gio::ListStore::new::<glib::BoxedAnyObject>()"));
        assert!(code.contains("gtk4::SignalListItemFactory::new()"));
        assert!(code.contains("gtk4::SingleSelection::new("));
        assert!(!code.contains("ListBox"), "no widget per row in a ListBox");
    }

    /// A cursor move is a selection change and a scroll; only new rows replace the store's items.
    #[test]
    fn the_store_is_spliced_only_when_rows_rev_changes() {
        assert_eq!(
            compact(&whole()).matches(".splice(").count(),
            1,
            "one place replaces the rows"
        );
        let source = scan::read_required("shell/src/gtk_host/project_chooser.rs");
        let (_, show) = scan::function_in(std::slice::from_ref(&source), "show");
        let at = show
            .find("if self.shown_rev.get() != Some(view.rows_rev)")
            .expect("the splice is guarded by rows_rev");
        let open = at + show[at..].find('{').unwrap();
        let block = scan::braced(&show, open);
        assert!(
            compact(block).contains("self.store.splice(0,self.store.n_items(),&items)"),
            "{block}"
        );
        let selection = show
            .find("self.selection.set_selected(")
            .expect("the cursor is a selection");
        assert!(
            selection > open + block.len(),
            "the cursor moves without touching the store"
        );
    }

    /// A name from the disk is a value: wrapped in a first-strong isolate after a left-to-right mark, so a
    /// right-to-left name keeps its own order without reordering what stands beside it, in a label laid out left to
    /// right.
    #[test]
    fn values_are_isolated_and_laid_out_left_to_right() {
        assert_eq!(isolated("atlas/"), "\u{200e}\u{2068}atlas/\u{2069}");
        assert_eq!(isolated("שלום/"), "\u{200e}\u{2068}שלום/\u{2069}");
        let source = scan::read_required("shell/src/gtk_host/project_chooser.rs");
        let sources = [source];
        let (_, isolate) = scan::function_in_raw(&sources, "isolated");
        assert!(
            isolate.contains("\\u{2068}") && isolate.contains("\\u{2069}"),
            "{isolate}"
        );
        let (_, label) = scan::function_in(&sources, "label");
        assert!(compact(&label).contains("label.set_direction(gtk4::TextDirection::Ltr)"));
        let (_, setter) = scan::function_in(&sources, "set_value");
        assert!(compact(&setter).contains("label.set_text(&isolated("), "{setter}");
        let (_, bind) = scan::function_in(&sources, "fill_row");
        let bind = compact(&bind);
        assert!(bind.contains("set_value(&name,&row.name)"), "{bind}");
        assert!(bind.contains("set_value(&shown,&row.shown)"), "{bind}");
        let (_, show) = scan::function_in(&sources, "show");
        assert!(compact(&show).contains("set_value(&self.message_path,path)"));
    }

    /// The line is what the person edits and what the model reads back as a path: no isolate may ever reach it.
    #[test]
    fn the_entry_text_never_holds_an_isolate() {
        let code = whole();
        assert_eq!(scan::calls(&code, "isolated"), 1, "only set_value isolates");
        let source = scan::read_required("shell/src/gtk_host/project_chooser.rs");
        let (_, setter) = scan::function_in(std::slice::from_ref(&source), "set_value");
        assert_eq!(scan::calls(&setter, "isolated"), 1);
        for line in code.lines().filter(|line| line.contains("entry")) {
            assert!(!line.contains("set_value(") && !line.contains("isolated("), "{line}");
        }
        let (_, show) = scan::function_in(std::slice::from_ref(&source), "show");
        assert!(compact(&show).contains("self.entry.set_text(&view.line)"));
        let shipped = &source.text[..source.text.find("#[cfg(test)]").unwrap()];
        assert!(!shipped.contains('\u{2068}'), "no isolate written as a raw character");
        assert_eq!(
            shipped.matches("\\u{2068}").count(),
            1,
            "the isolate is written once, in isolated"
        );
    }

    /// A size group would measure every row to align one column; the name column is a fixed number of characters.
    #[test]
    fn no_size_group_measures_the_rows() {
        let code = compact(&whole());
        assert!(!code.contains("SizeGroup"));
        let source = scan::read_required("shell/src/gtk_host/project_chooser.rs");
        let (_, bind) = scan::function_in(std::slice::from_ref(&source), "fill_row");
        let bind = compact(&bind);
        assert!(bind.contains("name.set_width_chars(NAME_CHARS)"), "{bind}");
        assert!(bind.contains("name.set_max_width_chars(NAME_CHARS)"), "{bind}");
    }

    /// In the list, Backspace goes up and the side arrows go out and in; in the line they stay the entry's own editing.
    #[test]
    fn backspace_and_the_arrows_in_the_list_report_up_and_side() {
        let none = ModifierType::empty();
        assert_eq!(list_ev(Key::BackSpace, none), Some(LineEv::Up));
        assert_eq!(list_ev(Key::Left, none), Some(LineEv::Side(-1)));
        assert_eq!(list_ev(Key::KP_Left, none), Some(LineEv::Side(-1)));
        assert_eq!(list_ev(Key::Right, none), Some(LineEv::Side(1)));
        assert_eq!(list_ev(Key::KP_Right, none), Some(LineEv::Side(1)));
        assert_eq!(list_ev(Key::Left, ModifierType::CONTROL_MASK), Some(LineEv::Other));
        assert_eq!(list_ev(Key::BackSpace, ModifierType::ALT_MASK), Some(LineEv::Other));
        for key in [Key::BackSpace, Key::Left, Key::Right] {
            assert_eq!(line_key(key, none), None, "{key:?} edits the line");
        }
    }

    /// `Ctrl+w` in the line deletes back to the previous slash, as readline does; in the list it is no command.
    #[test]
    fn ctrl_w_in_the_line_reports_delete_segment() {
        let ctrl = ModifierType::CONTROL_MASK;
        assert_eq!(line_key(Key::w, ctrl), Some(LineEv::DeleteSegment));
        assert_eq!(
            line_key(Key::W, ctrl | ModifierType::SHIFT_MASK),
            Some(LineEv::DeleteSegment)
        );
        assert_eq!(line_key(Key::w, ModifierType::empty()), None, "a plain w is text");
        assert_eq!(line_key(Key::w, ctrl | ModifierType::ALT_MASK), None);
        assert_eq!(
            line_key(Key::o, ctrl),
            Some(LineEv::ChooseFolder),
            "the keys of both states stay"
        );
        assert_eq!(line_key(Key::Escape, ModifierType::empty()), Some(LineEv::Escape));
        assert_eq!(list_ev(Key::w, ctrl), Some(LineEv::Other));
        assert!(
            code().contains("line_key(key,mods)"),
            "the line's controller reads line_key"
        );
    }

    /// A message that names a folder carries it after a tab: the words in one label, the folder in another that gives
    /// way first. Any other message is words alone.
    #[test]
    fn a_message_splits_its_words_from_its_folder_at_a_tab() {
        assert_eq!(
            split_message("not answering · h goes back\t/mnt/nas"),
            ("not answering · h goes back", Some("/mnt/nas"))
        );
        assert_eq!(split_message("Opening atlas…"), ("Opening atlas…", None));
        assert_eq!(split_message("reading\t"), ("reading", None));
    }
}
