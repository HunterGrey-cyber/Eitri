//! A float is one module above the tiled tree, never the editor. Its rectangle stores a centre
//! fraction and a pixel size, including its bar. The fractional centre keeps a centred float centred
//! as the grid grows, while its pixel size preserves the module's useful space.

use super::arrange::MIN_VISIBLE_LEAF_PX;
use super::geometry::{Arrangement, Frame, Rect, Size};
use super::module::ModuleId;

/// A centre as a fraction of the grid and a size in pixels, including the bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloatRect {
    pub cx: f32,
    pub cy: f32,
    pub w: i32,
    pub h: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Float {
    pub id: ModuleId,
    pub rect: FloatRect,
    pub scratch: bool,
}

/// The share of each grid axis used when no rectangle is remembered.
pub const DEFAULT_FLOAT_SHARE: f32 = 0.6;
/// The largest stored side in pixels.
pub const MAX_FLOAT_PX: i32 = 16384;

impl FloatRect {
    /// Centred, with each side rounded to the nearest pixel.
    pub fn centred(size: Size, share: f32) -> FloatRect {
        FloatRect {
            cx: 0.5,
            cy: 0.5,
            w: (size.w as f32 * share).round() as i32,
            h: (size.h as f32 * share).round() as i32,
        }
    }

    /// Fits the rectangle into the grid without changing its stored size.
    pub fn place(&self, size: Size, floor: Size) -> Rect {
        let w = self.w.max(floor.w).min(size.w);
        let h = self.h.max(floor.h).min(size.h);
        let x = ((self.cx * size.w as f32).round() as i32 - w / 2).clamp(0, size.w - w);
        let y = ((self.cy * size.h as f32).round() as i32 - h / 2).clamp(0, size.h - h);
        Rect { x, y, w, h }
    }

    /// Stores the centre and size of a rectangle already placed in the grid.
    pub fn of_rect(rect: Rect, size: Size) -> FloatRect {
        FloatRect {
            cx: (rect.x + rect.w / 2) as f32 / size.w as f32,
            cy: (rect.y + rect.h / 2) as f32 / size.h as f32,
            w: rect.w,
            h: rect.h,
        }
    }

    pub fn is_valid(&self) -> bool {
        self.cx.is_finite()
            && self.cy.is_finite()
            && (0.0..=1.0).contains(&self.cx)
            && (0.0..=1.0).contains(&self.cy)
            && (1..=MAX_FLOAT_PX).contains(&self.w)
            && (1..=MAX_FLOAT_PX).contains(&self.h)
    }

    /// Replaces non-finite centres with 0.5 and clamps all other values into their stored ranges.
    pub fn clamped(&self) -> FloatRect {
        FloatRect {
            cx: if self.cx.is_finite() {
                self.cx.clamp(0.0, 1.0)
            } else {
                0.5
            },
            cy: if self.cy.is_finite() {
                self.cy.clamp(0.0, 1.0)
            } else {
                0.5
            },
            w: self.w.clamp(1, MAX_FLOAT_PX),
            h: self.h.clamp(1, MAX_FLOAT_PX),
        }
    }
}

/// The measured module minimum, floored at the visible-leaf minimum, plus a strip row for its bar.
pub fn float_floor(id: &ModuleId, frame: &Frame) -> Size {
    let min = (frame.min)(id);
    Size {
        w: min.w.max(MIN_VISIBLE_LEAF_PX),
        h: min.h.max(MIN_VISIBLE_LEAF_PX) + frame.strip_px,
    }
}

/// The top strip row and the host below it; a short rectangle gives its whole height to the bar.
pub fn float_parts(rect: Rect, strip_px: i32) -> (Rect, Rect) {
    let bar_h = strip_px.max(0).min(rect.h);
    (
        Rect { h: bar_h, ..rect },
        Rect {
            y: rect.y + bar_h,
            h: rect.h - bar_h,
            ..rect
        },
    )
}

/// The floats above `id` overlapping it by a positive area, bottom first.
/// A module off screen has no covering floats.
pub fn covering(arrangement: &Arrangement, id: &ModuleId) -> Vec<ModuleId> {
    let (rect, above) = if let Some(i) = arrangement.floats.iter().position(|spot| &spot.id == id) {
        (arrangement.floats[i].rect, &arrangement.floats[i + 1..])
    } else if let Some(rect) = arrangement.rect_of(id) {
        (rect, arrangement.floats.as_slice())
    } else {
        return Vec::new();
    };
    above
        .iter()
        .filter(|spot| {
            rect.x.max(spot.rect.x) < (rect.x + rect.w).min(spot.rect.x + spot.rect.w)
                && rect.y.max(spot.rect.y) < (rect.y + rect.h).min(spot.rect.y + spot.rect.h)
        })
        .map(|spot| spot.id.clone())
        .collect()
}

/// Whether any float above `id` overlaps it on screen.
pub fn obscured(arrangement: &Arrangement, id: &ModuleId) -> bool {
    !covering(arrangement, id).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Size = Size { w: 1280, h: 721 };
    const FLOOR: Size = Size { w: 32, h: 56 };

    #[test]
    fn a_centred_float_takes_sixty_percent_of_the_grid() {
        let r = FloatRect::centred(WINDOW, DEFAULT_FLOAT_SHARE);
        assert_eq!(
            r,
            FloatRect {
                cx: 0.5,
                cy: 0.5,
                w: 768,
                h: 433
            }
        );
        assert_eq!(
            r.place(WINDOW, FLOOR),
            Rect {
                x: 256,
                y: 145,
                w: 768,
                h: 433
            }
        );
    }

    #[test]
    fn a_float_is_clamped_into_the_grid_and_keeps_its_own_size_in_the_rect() {
        let r = FloatRect {
            cx: 0.95,
            cy: 0.05,
            w: 600,
            h: 300,
        };
        assert_eq!(
            r.place(WINDOW, FLOOR),
            Rect {
                x: 680,
                y: 0,
                w: 600,
                h: 300
            }
        );
        let big = FloatRect {
            cx: 0.5,
            cy: 0.5,
            w: 900,
            h: 600,
        };
        assert_eq!(
            big.place(Size { w: 800, h: 600 }, FLOOR),
            Rect {
                x: 0,
                y: 0,
                w: 800,
                h: 600
            }
        );
        assert_eq!(
            big.place(Size { w: 1920, h: 1040 }, FLOOR),
            Rect {
                x: 510,
                y: 220,
                w: 900,
                h: 600
            }
        );
        let tiny = FloatRect {
            cx: 0.5,
            cy: 0.5,
            w: 10,
            h: 10,
        };
        assert_eq!(
            tiny.place(WINDOW, Size { w: 100, h: 80 }),
            Rect {
                x: 590,
                y: 321,
                w: 100,
                h: 80
            }
        );
    }

    #[test]
    fn of_rect_is_place_backwards() {
        let rect = Rect {
            x: 100,
            y: 50,
            w: 401,
            h: 200,
        };
        let back = FloatRect::of_rect(rect, WINDOW);
        assert_eq!((back.w, back.h), (401, 200));
        assert_eq!(back.place(WINDOW, FLOOR), rect);
    }

    #[test]
    fn a_stored_rect_is_valid_only_inside_its_ranges_and_clamps_into_them() {
        assert!(FloatRect {
            cx: 0.0,
            cy: 1.0,
            w: 1,
            h: MAX_FLOAT_PX
        }
        .is_valid());
        for bad in [
            FloatRect {
                cx: -0.1,
                cy: 0.5,
                w: 10,
                h: 10,
            },
            FloatRect {
                cx: 0.5,
                cy: 1.5,
                w: 10,
                h: 10,
            },
            FloatRect {
                cx: f32::NAN,
                cy: 0.5,
                w: 10,
                h: 10,
            },
            FloatRect {
                cx: 0.5,
                cy: 0.5,
                w: 0,
                h: 10,
            },
            FloatRect {
                cx: 0.5,
                cy: 0.5,
                w: 10,
                h: MAX_FLOAT_PX + 1,
            },
        ] {
            assert!(!bad.is_valid(), "{bad:?}");
            assert!(bad.clamped().is_valid(), "{bad:?}");
        }
        assert_eq!(
            FloatRect {
                cx: f32::NAN,
                cy: 2.0,
                w: -5,
                h: 99_999
            }
            .clamped(),
            FloatRect {
                cx: 0.5,
                cy: 1.0,
                w: 1,
                h: MAX_FLOAT_PX
            }
        );
    }

    #[test]
    fn the_bar_is_the_top_strip_row_and_the_floor_adds_it() {
        let (bar, host) = float_parts(
            Rect {
                x: 256,
                y: 145,
                w: 768,
                h: 433,
            },
            24,
        );
        assert_eq!(
            bar,
            Rect {
                x: 256,
                y: 145,
                w: 768,
                h: 24
            }
        );
        assert_eq!(
            host,
            Rect {
                x: 256,
                y: 169,
                w: 768,
                h: 409
            }
        );
        let min = |id: &ModuleId| {
            if *id == ModuleId::terminal() {
                Size { w: 200, h: 10 }
            } else {
                Size::default()
            }
        };
        let frame = Frame {
            size: WINDOW,
            handle_px: 1,
            strip_px: 24,
            min: &min,
        };
        assert_eq!(float_floor(&ModuleId::terminal(), &frame), Size { w: 200, h: 56 });
        assert_eq!(float_floor(&ModuleId::agent(), &frame), Size { w: 32, h: 56 });
    }
}

#[cfg(test)]
mod covering_tests {
    use super::*;
    use crate::layout::geometry::{arrange, hide};
    use crate::layout::module::ModuleKind;
    use crate::layout::tree::Layout;

    #[test]
    fn a_float_covers_what_it_overlaps_below_it_and_a_stowed_one_covers_nothing() {
        let t2 = ModuleId::instance(ModuleKind::Terminal, 2).unwrap();
        let t3 = ModuleId::instance(ModuleKind::Terminal, 3).unwrap();
        let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
        let mut layout = Layout::initial(&[]).unwrap();
        layout
            .add_float(
                &t2,
                FloatRect {
                    cx: 0.5,
                    cy: 0.5,
                    w: 768,
                    h: 433,
                },
                true,
            )
            .unwrap();
        let a = arrange(&layout, &frame);
        assert_eq!(covering(&a, &ModuleId::agent()), vec![t2.clone()]);
        assert!(obscured(&a, &ModuleId::editor()));
        assert!(!obscured(&a, &t2));
        layout
            .add_float(
                &t3,
                FloatRect {
                    cx: 0.2,
                    cy: 0.5,
                    w: 300,
                    h: 300,
                },
                false,
            )
            .unwrap();
        let a = arrange(&layout, &frame);
        assert_eq!(covering(&a, &t2), vec![t3.clone()]);
        assert!(covering(&a, &t3).is_empty());
        assert_eq!(
            covering(&a, &ModuleId::agent()),
            vec![t2.clone()],
            "t3 lies left of the chat"
        );
        assert_eq!(covering(&a, &ModuleId::editor()), vec![t2.clone(), t3.clone()]);
        hide(&mut layout, &t2, &frame).unwrap();
        assert!(!obscured(&arrange(&layout, &frame), &ModuleId::agent()));
    }
}
