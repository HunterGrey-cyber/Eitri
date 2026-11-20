//! The hint panel: while the prefix waits for a key, the keys and what they do, drawn over the bottom of the
//! window. The controller's model ([`PrefixPanel`]) says what it lists; this file only draws it.
//!
//! The panel never takes part in input: it and every widget in it are neither focusable nor targetable, so a key
//! or a click goes where it would have gone without it, and an input method's composition in the editor is left
//! alone.
//!
//! Every text is the keymap's or a Lua panel's, so it is set as text, never as markup, in one function
//! ([`set_hint_text`]). Every label ellipsizes and asks for no width of its own, so the panel never raises the
//! window's minimum size (the prefix strip's lesson: a label that cannot shrink makes GTK grow the window). The
//! panel is at most 90% of the window's width and 45% of its height: groups that do not fit across are left out and
//! the footer still shows; a group taller than the height loses its last rows, and a last line says how many.
//!
//! The fit follows the window while the panel is up: a frame tick compares the overlay's size with the one the
//! panel was last fitted to and fits again when it changed, so a widened window brings back groups and rows that
//! were left out and a shrunk one narrows the panel and lowers its height cap. The tick runs only while the panel is
//! shown.
//!
//! known limit: GTK layout, so no headless test holds the placement or the refit.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use eitri_core::wm::prefix_panel::PrefixPanel;
use gtk4::prelude::*;

use crate::gtk_host::prefix_fit::{columns_that_fit, rows_that_fit, MAX_HEIGHT_PERCENT, MAX_WIDTH_PERCENT};

/// The space between two groups.
const COLUMN_GAP: i32 = 18;
/// What the panel's own padding and border take from its width, which no group can use.
const FRAME_WIDTH: i32 = 32;
/// What the panel's padding, border and the two gaps between its parts take from its height.
const FRAME_HEIGHT: i32 = 32;
/// The most characters a key column or a label asks for before it ellipsizes.
const KEYS_CHARS: i32 = 12;
const LABEL_CHARS: i32 = 22;
/// The most characters a group's title, the heading and the footer ask for.
const TITLE_CHARS: i32 = 24;
const LINE_CHARS: i32 = 60;

/// The one place a panel label's text is set: as text, never as markup.
fn set_hint_text(label: &gtk4::Label, text: &str) {
    label.set_text(text);
}

/// An empty label that ellipsizes, asks for no width of its own, shows its text as written and takes no part in
/// input, with `text` set and its style class `class`.
fn hint_label(class: &str, max_chars: i32, text: &str) -> gtk4::Label {
    let label = gtk4::Label::new(None);
    label.set_use_markup(false);
    label.set_single_line_mode(true);
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    label.set_width_chars(0);
    label.set_max_width_chars(max_chars);
    label.set_xalign(0.0);
    label.set_can_focus(false);
    label.set_can_target(false);
    label.add_css_class(class);
    set_hint_text(&label, text);
    label
}

/// The panel's widgets (each a handle to the one widget), shared with the tick that fits them again.
#[derive(Clone)]
struct Parts {
    frame: gtk4::Box,
    heading: gtk4::Label,
    scroller: gtk4::ScrolledWindow,
    columns: gtk4::Box,
    /// The groups in `columns`, in its order.
    groups: Rc<RefCell<Vec<GroupColumn>>>,
    footer: gtk4::Label,
}

/// One group's widgets, kept so that rows can be left out from the bottom.
struct GroupColumn {
    column: gtk4::Grid,
    title: gtk4::Label,
    /// Each row's key and label.
    rows: Vec<[gtk4::Label; 2]>,
    /// Says how many rows were left out; hidden while none were.
    more: gtk4::Label,
}

impl GroupColumn {
    /// Every row back, and the line about them gone.
    fn show_all_rows(&self) {
        for labels in self.rows.iter().flatten() {
            labels.set_visible(true);
        }
        self.more.set_visible(false);
    }

    /// A group taller than `room` keeps the rows that fit above a line saying how many it left out.
    fn fit_height(&self, room: i32) {
        if self.column.measure(gtk4::Orientation::Vertical, -1).1 <= room {
            return;
        }
        let Some(first) = self.rows.first() else { return };
        let height = |label: &gtk4::Label| label.measure(gtk4::Orientation::Vertical, -1).1;
        let row = height(&first[0]).max(height(&first[1]));
        let keep = rows_that_fit(self.rows.len(), room - height(&self.title), row);
        if keep == self.rows.len() {
            return;
        }
        for labels in self.rows[keep..].iter().flatten() {
            labels.set_visible(false);
        }
        set_hint_text(&self.more, &format!("\u{2026} {} more", self.rows.len() - keep));
        self.more.set_visible(true);
    }
}

/// The panel's widgets, hidden until [`PrefixPanelView::show`].
pub(crate) struct PrefixPanelView {
    parts: Parts,
    /// The size of the overlay the panel was last fitted to.
    fitted: Rc<Cell<(i32, i32)>>,
    /// The tick that fits the panel again when the overlay's size changes, while the panel is up.
    tick: RefCell<Option<gtk4::TickCallbackId>>,
}

impl PrefixPanelView {
    pub(crate) fn new() -> Self {
        let frame = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        frame.add_css_class("prefix-panel");
        frame.set_halign(gtk4::Align::Center);
        frame.set_valign(gtk4::Align::End);
        frame.set_margin_bottom(16);
        frame.set_can_focus(false);
        frame.set_can_target(false);
        frame.set_visible(false);
        let heading = hint_label("panel-heading", LINE_CHARS, "");
        let columns = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        columns.set_can_focus(false);
        columns.set_can_target(false);
        columns.set_valign(gtk4::Align::Start);
        let scroller = gtk4::ScrolledWindow::new();
        // Never scrolled (nothing can reach it) and never drawn with a bar: `fit` leaves out what does not fit.
        scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::External);
        scroller.set_propagate_natural_height(true);
        scroller.set_propagate_natural_width(true);
        scroller.set_can_focus(false);
        scroller.set_can_target(false);
        scroller.set_child(Some(&columns));
        let footer = hint_label("panel-footer", LINE_CHARS, "");
        frame.append(&heading);
        frame.append(&scroller);
        frame.append(&footer);
        PrefixPanelView {
            parts: Parts {
                frame,
                heading,
                scroller,
                columns,
                groups: Rc::new(RefCell::new(Vec::new())),
                footer,
            },
            fitted: Rc::new(Cell::new((0, 0))),
            tick: RefCell::new(None),
        }
    }

    /// The panel's widget, for the window's overlay.
    pub(crate) fn widget(&self) -> &gtk4::Widget {
        self.parts.frame.upcast_ref()
    }

    /// Replaces what the panel shows with `panel` and shows it.
    pub(crate) fn show(&self, panel: &PrefixPanel) {
        let parts = &self.parts;
        set_hint_text(&parts.heading, &panel.heading);
        set_hint_text(&parts.footer, &panel.footer);
        while let Some(child) = parts.columns.first_child() {
            parts.columns.remove(&child);
        }
        let groups: Vec<GroupColumn> = panel.groups.iter().map(group_column).collect();
        for group in &groups {
            parts.columns.append(&group.column);
        }
        *parts.groups.borrow_mut() = groups;
        parts.frame.set_visible(true);
        parts.fit(&self.fitted);
        if self.tick.borrow().is_none() {
            let (parts, fitted) = (self.parts.clone(), self.fitted.clone());
            let id = self.parts.frame.add_tick_callback(move |_, _| {
                if parts.parent_size() != fitted.get() {
                    parts.fit(&fitted);
                }
                gtk4::glib::ControlFlow::Continue
            });
            *self.tick.borrow_mut() = Some(id);
        }
    }

    /// Takes the panel down, and the tick with it.
    pub(crate) fn hide(&self) {
        self.parts.frame.set_visible(false);
        if let Some(id) = self.tick.borrow_mut().take() {
            id.remove();
        }
    }
}

impl Parts {
    /// The size of the overlay the panel sits in, `(0, 0)` while it has none.
    fn parent_size(&self) -> (i32, i32) {
        self.frame
            .parent()
            .map_or((0, 0), |parent| (parent.width(), parent.height()))
    }

    /// Leaves out the groups that do not fit the window's width and the rows that do not fit its height, and notes in
    /// `fitted` the size it fitted to. A window not laid out yet (no width) leaves everything in.
    fn fit(&self, fitted: &Cell<(i32, i32)>) {
        let (width, height) = self.parent_size();
        fitted.set((width, height));
        if width <= 0 || height <= 0 {
            return;
        }
        let groups = self.groups.borrow();
        for group in groups.iter() {
            group.column.set_visible(true);
            group.show_all_rows();
        }
        let widths: Vec<i32> = groups
            .iter()
            .map(|group| group.column.measure(gtk4::Orientation::Horizontal, -1).1 + COLUMN_GAP)
            .collect();
        let available = width * MAX_WIDTH_PERCENT / 100 - FRAME_WIDTH;
        let fits = columns_that_fit(&widths, available);
        for group in &groups[fits..] {
            group.column.set_visible(false);
        }
        let lines = self.heading.measure(gtk4::Orientation::Vertical, -1).1
            + self.footer.measure(gtk4::Orientation::Vertical, -1).1;
        let room = (height * MAX_HEIGHT_PERCENT / 100 - lines - FRAME_HEIGHT).max(1);
        self.scroller.set_max_content_height(room);
        for group in &groups[..fits] {
            group.fit_height(room);
        }
    }
}

/// One group: its title over its rows, a key and what it does on each, and the hidden line that takes over from
/// rows left out.
fn group_column(group: &eitri_core::keymap::hints::HintGroup) -> GroupColumn {
    let column = gtk4::Grid::new();
    column.set_column_spacing(10);
    column.set_margin_end(COLUMN_GAP);
    column.set_valign(gtk4::Align::Start);
    column.set_can_focus(false);
    column.set_can_target(false);
    let title = hint_label("panel-group-title", TITLE_CHARS, group.title);
    column.attach(&title, 0, 0, 2, 1);
    let mut rows = Vec::with_capacity(group.rows.len());
    let mut line = 1;
    for hint in &group.rows {
        let key = hint_label("panel-key", KEYS_CHARS, &hint.keys);
        let label = hint_label("panel-label", LABEL_CHARS, &hint.label);
        column.attach(&key, 0, line, 1, 1);
        column.attach(&label, 1, line, 1, 1);
        rows.push([key, label]);
        line = line.saturating_add(1);
    }
    let more = hint_label("panel-label", LABEL_CHARS, "");
    more.set_visible(false);
    column.attach(&more, 0, line, 2, 1);
    GroupColumn {
        column,
        title,
        rows,
        more,
    }
}

#[cfg(test)]
mod tests {

    /// A key's description is a keymap string or a Lua panel's title: the panel shows `<b>x</b>` as those
    /// characters. Read off this file's code (comments, literals and this test module blanked): no markup
    /// setter, markup never switched on, and the text set in exactly one place, `set_hint_text`, which every
    /// label goes through.
    #[test]
    fn prefix_panel_labels_are_set_as_text_never_markup() {
        use eitri_core::source_scan as scan;
        let source = scan::read_required("shell/src/gtk_host/prefix_panel.rs");
        let code = scan::code_only(&source.text);
        let code = scan::without(&code, &scan::modules(&code, "tests"));
        let flat: String = code.split_whitespace().collect();
        for forbidden in [
            "set_markup(",
            "set_use_markup(true)",
            "set_label(",
            "with_mnemonic(",
            "set_text_with_mnemonic(",
            "set_markup_with_mnemonic(",
        ] {
            assert!(!flat.contains(forbidden), "prefix_panel.rs calls {forbidden}");
        }
        assert!(
            flat.contains("set_use_markup(false)"),
            "markup is switched off on every label"
        );
        assert_eq!(flat.matches(".set_text(").count(), 1, "the text is set in one place");
        let setter = scan::functions(&code, "set_hint_text");
        assert_eq!(setter.len(), 1);
        assert!(setter[0].contains(".set_text("), "{}", setter[0]);
        // A label made with text would set it past the one function.
        assert_eq!(flat.matches("Label::new(").count(), 1);
        assert!(flat.contains("Label::new(None)"), "labels are made empty");
        assert!(
            scan::calls(&code, "set_hint_text") >= 3,
            "the heading and the footer set by `show`, and every other label made by `hint_label`, go through it"
        );
    }
}
