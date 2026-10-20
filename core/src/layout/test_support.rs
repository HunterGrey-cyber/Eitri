//! Shared checks for the layout's tests: a layout is asserted by where it puts each module on screen,
//! at the three window sizes the verb tests use, not by the tree's own shape.

use std::collections::BTreeMap;

use super::geometry::{arrange, settle_pins, Frame, Rect, Size};
use super::module::ModuleId;
use super::tree::Layout;

pub(crate) const FRAMES: [Size; 3] = [
    Size { w: 1280, h: 721 },
    Size { w: 1920, h: 1040 },
    Size { w: 800, h: 600 },
];

pub(crate) fn rects(layout: &Layout, size: Size) -> BTreeMap<ModuleId, Rect> {
    arrange(layout, &Frame::new(size, 1)).modules.into_iter().collect()
}

/// `got` puts every module on screen where `want` does, at every frame size.
pub(crate) fn same_rects(got: &Layout, want: &Layout) {
    for size in FRAMES {
        assert_eq!(rects(got, size), rects(want, size), "at {}x{}", size.w, size.h);
    }
}

/// [`same_rects`], then the same again with every hidden module shown and every pin settled at the
/// first size: a hidden module's place, and which lengths are pinned, show only then.
pub(crate) fn same_rects_opened(got: &Layout, want: &Layout) {
    same_rects(got, want);
    let open = |layout: &Layout| {
        let mut layout = layout.clone();
        for id in layout.hidden().clone() {
            if !layout.is_gone(&id) {
                layout.show(&id).unwrap();
            }
        }
        settle_pins(&mut layout, &Frame::new(FRAMES[0], 1));
        layout
    };
    same_rects(&open(got), &open(want));
}
