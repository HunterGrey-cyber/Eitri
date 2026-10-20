//! The strips of tabbed and stacked containers: one row per container (tabbed) or one row per child
//! (stacked) naming the modules it holds, the drawn one marked. A strip belongs to the grid like a
//! divider handle does: it is a child of the grid from the moment it is made until the window goes
//! away, hidden (`set_child_visible(false)`) while no container needs it, and reused by the next one
//! that does. So a container switching between tabbed and split costs no widget, and nothing is
//! ever unparented while the window is up.
//!
//! An entry's text is whatever the window's label function returns for its module, set as plain
//! text: a chat's session label is the user's (or the agent's) own words, and `<b>x</b>` in it must
//! read as those five characters, not as bold.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;

use eitri_core::layout::{ConLayout, ModuleId, Strip};

/// The strip's CSS node name. `theme::gtk_css` styles it, and its entries as `constrip > label`.
pub(crate) const CSS_NAME: &str = "constrip";

/// The CSS class on the entry whose module the container draws.
pub(crate) const ACTIVE_CLASS: &str = "active";

/// One entry of a strip: its label, and the module it names now (read by its click).
struct Entry {
    label: gtk4::Label,
    module: Rc<RefCell<Option<ModuleId>>>,
}

/// A container's strip, pooled by the grid. Its entries are pooled the same way inside it: made
/// when a container first has that many children, hidden when it has fewer.
pub(super) struct ConStrip {
    pub(super) widget: gtk4::Box,
    entries: RefCell<Vec<Entry>>,
    /// Runs the grid's strip-activate hooks for a module; every entry's click calls it.
    activate: Rc<dyn Fn(&ModuleId)>,
}

impl ConStrip {
    /// A strip with no entries yet, not yet parented and not yet drawn.
    pub(super) fn new(activate: Rc<dyn Fn(&ModuleId)>) -> Self {
        let widget: gtk4::Box = glib::Object::builder()
            .property("css-name", CSS_NAME)
            .property("orientation", gtk4::Orientation::Horizontal)
            .build();
        // Equal widths for a tab row, equal heights for a stacked column: the container's children
        // are peers, and a long title must not push its neighbours off the row.
        widget.set_homogeneous(true);
        // A strip never takes the keys: they belong to the modules, and a focusable strip would be
        // one more place GTK's own focus chain could send them when a host leaves the screen.
        widget.set_focusable(false);
        widget.set_child_visible(false);
        ConStrip {
            widget,
            entries: RefCell::new(Vec::new()),
            activate,
        }
    }

    /// Draws `strip`'s entries, each named by `label`, and shows the strip. The caller allocates it.
    pub(super) fn show(&self, strip: &Strip, label: &dyn Fn(&ModuleId) -> String) {
        let orientation = match strip.layout {
            ConLayout::Stacked => gtk4::Orientation::Vertical,
            _ => gtk4::Orientation::Horizontal,
        };
        if self.widget.orientation() != orientation {
            self.widget.set_orientation(orientation);
        }
        while self.entries.borrow().len() < strip.entries.len() {
            let entry = self.new_entry();
            self.entries.borrow_mut().push(entry);
        }
        for (i, entry) in self.entries.borrow().iter().enumerate() {
            match strip.entries.get(i) {
                Some(shown) => {
                    set_entry_text(&entry.label, &label(&shown.id));
                    if shown.active {
                        entry.label.add_css_class(ACTIVE_CLASS);
                    } else {
                        entry.label.remove_css_class(ACTIVE_CLASS);
                    }
                    *entry.module.borrow_mut() = Some(shown.id.clone());
                    entry.label.set_visible(true);
                }
                None => {
                    entry.module.borrow_mut().take();
                    entry.label.set_visible(false);
                }
            }
        }
        self.widget.set_child_visible(true);
    }

    /// Takes the strip off the screen; it stays the grid's child, for the next container.
    pub(super) fn hide(&self) {
        self.widget.set_child_visible(false);
        for entry in self.entries.borrow().iter() {
            entry.module.borrow_mut().take();
        }
    }

    /// Names every entry on screen again with `label`, keeping which module each names.
    pub(super) fn relabel(&self, label: &dyn Fn(&ModuleId) -> String) {
        for entry in self.entries.borrow().iter() {
            if let Some(id) = entry.module.borrow().as_ref() {
                set_entry_text(&entry.label, &label(id));
            }
        }
    }

    /// Each entry that names a module now, with its label: what HINT labels and lands on.
    pub(super) fn entries_in_use(&self) -> Vec<(ModuleId, gtk4::Widget)> {
        self.entries
            .borrow()
            .iter()
            .filter_map(|entry| {
                let id = entry.module.borrow().clone()?;
                Some((id, entry.label.clone().upcast()))
            })
            .collect()
    }

    /// The height of one row as the entries' CSS makes it, or `None` before the strip has an entry.
    pub(super) fn row_px(&self) -> Option<i32> {
        let entries = self.entries.borrow();
        let label = &entries.first()?.label;
        // A hidden label measures 0; the first entry is hidden only on a strip no container uses.
        let (min, ..) = label.measure(gtk4::Orientation::Vertical, -1);
        (min > 0).then_some(min)
    }

    fn new_entry(&self) -> Entry {
        let label = gtk4::Label::new(None);
        // Plain text whatever the label function returns (the module doc).
        label.set_use_markup(false);
        label.set_single_line_mode(true);
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        label.set_focusable(false);
        let module: Rc<RefCell<Option<ModuleId>>> = Rc::new(RefCell::new(None));
        let click = gtk4::GestureClick::new();
        click.set_button(gtk4::gdk::BUTTON_PRIMARY);
        {
            let module = module.clone();
            let activate = self.activate.clone();
            click.connect_pressed(move |gesture, _, _, _| {
                // Claimed: the press chose a tab, and nothing under the strip acts on it as well.
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                // Cloned out: the hooks switch the tab, which re-allocates this strip.
                let id = module.borrow().clone();
                if let Some(id) = id {
                    activate(&id);
                }
            });
        }
        label.add_controller(click);
        self.widget.append(&label);
        Entry { label, module }
    }
}

/// The one way an entry's text is set: as text, never as markup (the module doc).
fn set_entry_text(label: &gtk4::Label, text: &str) {
    label.set_text(text);
}

/// How the arrangement's `strips` strips are drawn by a pool of `pool` strip widgets: how many new
/// widgets to make first, and for each strip in arrangement order, the pool slot that draws it. The
/// pool grows only when a pass needs more strips than it has ever drawn at once; slots past the
/// last strip are hidden.
pub(super) fn strip_assignments(pool: usize, strips: usize) -> (usize, Vec<usize>) {
    (strips.saturating_sub(pool), (0..strips).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_assignments_grow_the_pool_only_when_needed() {
        assert_eq!(strip_assignments(0, 0), (0, vec![]));
        assert_eq!(strip_assignments(0, 2), (2, vec![0, 1]), "the first two containers");
        assert_eq!(
            strip_assignments(2, 2),
            (0, vec![0, 1]),
            "the same two again: nothing new"
        );
        assert_eq!(
            strip_assignments(3, 1),
            (0, vec![0]),
            "fewer: the rest are hidden, none dropped"
        );
        assert_eq!(
            strip_assignments(1, 3),
            (2, vec![0, 1, 2]),
            "only the missing ones are made"
        );
        // Every slot named exists once the new ones are made, and no slot draws two strips.
        for pool in 0..5 {
            for strips in 0..5 {
                let (create, slots) = strip_assignments(pool, strips);
                assert_eq!(slots.len(), strips);
                assert!(slots.iter().all(|&s| s < pool + create));
                let mut seen = slots.clone();
                seen.dedup();
                assert_eq!(seen.len(), slots.len());
                assert_eq!(create, strips.saturating_sub(pool));
            }
        }
    }

    /// A session label is the user's text: an entry shows `<b>x</b>` as those characters. Read off
    /// this file's code (comments, literals and this test module blanked): no markup setter at all,
    /// markup never switched on, and the text set in exactly one place, `set_entry_text`, which is
    /// what every entry goes through.
    #[test]
    fn strip_entries_are_set_as_text_never_markup() {
        use eitri_core::source_scan as scan;
        let source = scan::read_required("shell/src/module_grid/strips.rs");
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
            assert!(!flat.contains(forbidden), "strips.rs calls {forbidden}");
        }
        assert!(
            flat.contains("set_use_markup(false)"),
            "markup is switched off on every entry"
        );
        assert_eq!(flat.matches(".set_text(").count(), 1, "the text is set in one place");
        let setter = scan::functions(&code, "set_entry_text");
        assert_eq!(setter.len(), 1);
        assert!(setter[0].contains(".set_text("), "{}", setter[0]);
        // `Label::new(Some(text))` would set text past the one function.
        assert!(flat.contains("Label::new(None)"), "entries are made empty");
        assert_eq!(flat.matches("Label::new(").count(), 1);
        assert!(
            scan::calls(&code, "set_entry_text") >= 2,
            "show and relabel both go through it"
        );
    }
}
