//! The window background around an editor presented below the GTK surface.

use std::cell::{Cell, RefCell};

use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, glib, graphene};

/// Disjoint positive-area rectangles covering the bounds outside the clipped hole.
pub(crate) fn hole_complement(bounds: graphene::Rect, hole: Option<graphene::Rect>) -> Vec<graphene::Rect> {
    if bounds.width() <= 0.0 || bounds.height() <= 0.0 {
        return Vec::new();
    }
    let Some(hole) = hole
        .and_then(|hole| bounds.intersection(&hole))
        .filter(|hole| hole.width() > 0.0 && hole.height() > 0.0)
    else {
        return vec![bounds];
    };
    let right = bounds.x() + bounds.width();
    let bottom = bounds.y() + bounds.height();
    let hole_right = hole.x() + hole.width();
    let hole_bottom = hole.y() + hole.height();
    [
        graphene::Rect::new(bounds.x(), bounds.y(), bounds.width(), hole.y() - bounds.y()),
        graphene::Rect::new(bounds.x(), hole_bottom, bounds.width(), bottom - hole_bottom),
        graphene::Rect::new(bounds.x(), hole.y(), hole.x() - bounds.x(), hole.height()),
        graphene::Rect::new(hole_right, hole.y(), right - hole_right, hole.height()),
    ]
    .into_iter()
    .filter(|rect| rect.width() > 0.0 && rect.height() > 0.0)
    .collect()
}

mod imp {
    use super::*;

    pub struct HoleBackdrop {
        pub(super) background: Cell<gdk::RGBA>,
        pub(super) editor: glib::WeakRef<gtk4::Widget>,
        pub(super) editor_handlers: RefCell<Vec<glib::SignalHandlerId>>,
        pub(super) open: Cell<bool>,
    }

    impl Default for HoleBackdrop {
        fn default() -> Self {
            Self {
                background: Cell::new(gdk::RGBA::BLACK),
                editor: glib::WeakRef::new(),
                editor_handlers: RefCell::new(Vec::new()),
                open: Cell::new(false),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for HoleBackdrop {
        const NAME: &'static str = "EitriHoleBackdrop";
        type Type = super::HoleBackdrop;
        type ParentType = gtk4::Widget;
    }

    impl ObjectImpl for HoleBackdrop {
        fn dispose(&self) {
            self.obj().disconnect_editor();
        }
    }

    impl WidgetImpl for HoleBackdrop {
        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            let widget = self.obj();
            let bounds = graphene::Rect::new(0.0, 0.0, widget.width() as f32, widget.height() as f32);
            let hole = self
                .open
                .get()
                .then(|| self.editor.upgrade())
                .flatten()
                .filter(|editor| editor.is_mapped())
                .and_then(|editor| editor.compute_bounds(&*widget));
            for rect in hole_complement(bounds, hole) {
                snapshot.append_color(&self.background.get(), &rect);
            }
        }
    }
}

glib::wrapper! {
    pub struct HoleBackdrop(ObjectSubclass<imp::HoleBackdrop>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl HoleBackdrop {
    pub(crate) fn new() -> Self {
        glib::Object::builder().property("can-target", false).build()
    }

    pub(crate) fn set_background(&self, rgba: gdk::RGBA) {
        self.imp().background.set(rgba);
        self.queue_draw();
    }

    pub(crate) fn set_hole_widget(&self, editor: Option<&gtk4::Widget>) {
        self.disconnect_editor();
        self.imp().editor.set(editor);
        if let Some(editor) = editor {
            let backdrop = self.downgrade();
            let mapped = editor.connect_map(move |_| {
                if let Some(backdrop) = backdrop.upgrade() {
                    backdrop.queue_draw();
                }
            });
            let backdrop = self.downgrade();
            let unmapped = editor.connect_unmap(move |_| {
                if let Some(backdrop) = backdrop.upgrade() {
                    backdrop.queue_draw();
                }
            });
            self.imp().editor_handlers.replace(vec![mapped, unmapped]);
        }
        self.queue_draw();
    }

    fn disconnect_editor(&self) {
        let handlers = self.imp().editor_handlers.take();
        if let Some(editor) = self.imp().editor.upgrade() {
            for handler in handlers {
                editor.disconnect(handler);
            }
        }
        self.imp().editor.set(None::<&gtk4::Widget>);
    }

    pub(crate) fn set_open(&self, open: bool) {
        self.imp().open.set(open);
        self.queue_draw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds() -> graphene::Rect {
        graphene::Rect::new(10.0, 20.0, 800.0, 600.0)
    }

    fn area(rect: &graphene::Rect) -> f32 {
        rect.width() * rect.height()
    }

    fn check_partition(hole: graphene::Rect, count: usize, removed: f32) {
        let bounds = bounds();
        let rects = hole_complement(bounds, Some(hole));
        assert_eq!(rects.len(), count);
        assert_eq!(rects.iter().map(area).sum::<f32>(), area(&bounds) - removed);
        for (i, rect) in rects.iter().enumerate() {
            assert!(rect.width() > 0.0 && rect.height() > 0.0);
            assert!(bounds.contains_rect(rect));
            assert!(rect.intersection(&hole).is_none_or(|r| area(&r) == 0.0));
            for other in &rects[..i] {
                assert!(rect.intersection(other).is_none_or(|r| area(&r) == 0.0));
            }
        }
    }

    #[test]
    fn no_hole_is_one_rectangle() {
        assert_eq!(hole_complement(bounds(), None), vec![bounds()]);
    }

    #[test]
    fn a_hole_inside_gives_four_disjoint_rectangles_covering_the_rest() {
        check_partition(graphene::Rect::new(110.0, 140.0, 400.0, 300.0), 4, 120_000.0);
    }

    #[test]
    fn a_hole_touching_an_edge_gives_three() {
        check_partition(graphene::Rect::new(10.0, 140.0, 400.0, 300.0), 3, 120_000.0);
    }

    #[test]
    fn a_hole_outside_the_bounds_is_ignored() {
        assert_eq!(
            hole_complement(bounds(), Some(graphene::Rect::new(900.0, 30.0, 50.0, 50.0))),
            vec![bounds()]
        );
    }

    #[test]
    fn a_hole_covering_everything_gives_none() {
        check_partition(graphene::Rect::new(-10.0, -20.0, 1000.0, 800.0), 0, 480_000.0);
    }

    #[test]
    fn a_hole_crossing_the_bounds_is_clipped() {
        check_partition(graphene::Rect::new(-20.0, 140.0, 100.0, 1000.0), 2, 70.0 * 480.0);
    }

    #[test]
    fn a_hole_only_touching_the_outside_edge_removes_nothing() {
        assert_eq!(
            hole_complement(bounds(), Some(graphene::Rect::new(810.0, 20.0, 50.0, 50.0))),
            vec![bounds()]
        );
    }

    #[test]
    fn empty_bounds_paint_nothing() {
        assert!(hole_complement(graphene::Rect::new(0.0, 0.0, 0.0, 600.0), None).is_empty());
    }
}
