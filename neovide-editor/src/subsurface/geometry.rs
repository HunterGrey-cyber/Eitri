//! Placement and buffer sizing for the editor's child surface.
//!
//! GTK maps native coordinates to surface coordinates by adding the native's surface transform.
//! A window shadow makes that transform positive, placing the content inside the shadow's extent.

/// A rectangle in the parent surface's logical coordinates: x, y, width, height.
pub(crate) type Rect = (i32, i32, i32, i32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Placement {
    pub position: (i32, i32),
    pub logical: (i32, i32),
    pub content: (i32, i32),
    pub buffer_scale: i32,
    pub viewport: Option<(i32, i32)>,
    /// Where the underlay goes: the editor's rectangle grown by [`UNDERLAY_REACH`] on each side and cut to the
    /// window. `None` until [`Placement::with_underlay`] names the window, and when nothing of it is left.
    pub underlay: Option<Rect>,
}

/// How far the underlay reaches past the editor on each side, in logical pixels.
///
/// A compositor may round or clip every surface of a window at the surface's own corners (Hyprland 0.56 rounds each
/// one by the window's `decoration:rounding`), the editor's included, and what shows in a cut-off corner is whatever
/// lies under the editor: the parent leaves a transparent hole there, so without the underlay it is the desktop. The
/// underlay is opaque and in the editor's background colour; reaching further than any such radius, its own cut
/// corners lie under the parent's opaque pixels around the hole. At the window's edge it stops at the edge, where
/// the window's own corner is rounded the same way anyway.
pub(crate) const UNDERLAY_REACH: i32 = 32;

pub(crate) const BUCKET: i32 = 128;

pub(crate) fn bucket(content: (i32, i32)) -> (i32, i32) {
    let round_up = |side: i32| side.saturating_add(BUCKET - 1) / BUCKET * BUCKET;
    (round_up(content.0), round_up(content.1))
}

pub(crate) fn too_large(buffer: (i32, i32), content: (i32, i32)) -> bool {
    let needed = bucket(content);
    i64::from(buffer.0) - i64::from(needed.0) > i64::from(BUCKET)
        || i64::from(buffer.1) - i64::from(needed.1) > i64::from(BUCKET)
}

pub(crate) fn device_len(logical: i32, scale: f64) -> i32 {
    (f64::from(logical) * scale).round() as i32
}

pub(crate) fn placement(
    origin_in_native: (f64, f64),
    surface_transform: (f64, f64),
    size: (i32, i32),
    scale: f64,
    scale_factor: i32,
    viewporter: bool,
) -> Option<Placement> {
    if size.0 < 1 || size.1 < 1 {
        return None;
    }

    let (content, buffer_scale, viewport) = if viewporter {
        if !scale.is_finite() || scale <= 0.0 {
            return None;
        }
        let content = (device_len(size.0, scale), device_len(size.1, scale));
        if content.0 < 1 || content.1 < 1 {
            return None;
        }
        (content, 1, Some(size))
    } else {
        if scale_factor < 1 {
            return None;
        }
        (
            (size.0.checked_mul(scale_factor)?, size.1.checked_mul(scale_factor)?),
            scale_factor,
            None,
        )
    };

    Some(Placement {
        position: (
            (origin_in_native.0 + surface_transform.0).round() as i32,
            (origin_in_native.1 + surface_transform.1).round() as i32,
        ),
        logical: size,
        content,
        buffer_scale,
        viewport,
        underlay: None,
    })
}

impl Placement {
    /// The editor's own part of the placement, the underlay left out: what its surface and viewport are sent.
    pub(crate) fn without_underlay(self) -> Placement {
        Placement { underlay: None, ..self }
    }

    /// The same placement with its underlay: the editor's rectangle grown by [`UNDERLAY_REACH`] and cut to the
    /// window, whose origin in the surface is the native's surface transform and whose size is the native's.
    pub(crate) fn with_underlay(mut self, window_origin: (f64, f64), window_size: (i32, i32)) -> Placement {
        let wx = window_origin.0.round() as i32;
        let wy = window_origin.1.round() as i32;
        let x0 = self.position.0.saturating_sub(UNDERLAY_REACH).max(wx);
        let y0 = self.position.1.saturating_sub(UNDERLAY_REACH).max(wy);
        let x1 = (self
            .position
            .0
            .saturating_add(self.logical.0)
            .saturating_add(UNDERLAY_REACH))
        .min(wx.saturating_add(window_size.0));
        let y1 = (self
            .position
            .1
            .saturating_add(self.logical.1)
            .saturating_add(UNDERLAY_REACH))
        .min(wy.saturating_add(window_size.1));
        self.underlay = (x1 > x0 && y1 > y0).then(|| (x0, y0, x1 - x0, y1 - y0));
        self
    }
}

/// One of the editor's four corners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Corner {
    pub(crate) const ALL: [Corner; 4] = [
        Corner::TopLeft,
        Corner::TopRight,
        Corner::BottomLeft,
        Corner::BottomRight,
    ];

    pub(crate) fn index(self) -> usize {
        self as usize
    }

    fn right(self) -> bool {
        matches!(self, Corner::TopRight | Corner::BottomRight)
    }

    fn bottom(self) -> bool {
        matches!(self, Corner::BottomLeft | Corner::BottomRight)
    }
}

/// A source rectangle in 1/256ths of a buffer pixel (`wl_fixed`'s resolution): x, y, width, height.
pub(crate) type FixedRect = (i32, i32, i32, i32);

/// One corner patch: a child of the editor's own surface, below it, showing one point of the editor's frame
/// stretched over a square around that corner.
///
/// A compositor that rounds every surface at its own corners (Hyprland 0.56) cuts the editor's corners off, and the
/// cut shows what lies below the editor. The patch lies below it and has the colour the editor itself has at that
/// corner: its source is the corner pixel's centre and one `wl_fixed` step inward, so a linear filter mixes in
/// nothing from past the frame's edge. The patch is rounded at its own corners too, so it reaches [`UNDERLAY_REACH`]
/// past the editor (cut to the window, as the underlay is), where the parent's opaque pixels cover its cut corners,
/// and as far into the editor (at most half the editor, so it never reaches another corner), where the editor's own
/// pixels cover its inner corner as long as the radius is at most half that reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CornerPatch {
    /// In the editor surface's coordinates (logical pixels from its top-left).
    pub rect: Rect,
    /// In the editor's buffer, whose top-left `content` pixels are the frame.
    pub source: FixedRect,
}

/// Where each corner patch goes, in [`Corner::ALL`] order, for an editor at `position` (parent surface coordinates)
/// showing a frame of `logical` size from `content` buffer pixels, with `underlay` the editor's rectangle grown and
/// cut to the window ([`Placement::with_underlay`]). A corner gets none when the window leaves no room past it: that
/// corner is the window's own, rounded the same way anyway.
pub(crate) fn corner_patches(
    position: (i32, i32),
    logical: (i32, i32),
    content: (i32, i32),
    underlay: Option<Rect>,
) -> [Option<CornerPatch>; 4] {
    let (w, h) = logical;
    let reach_x = UNDERLAY_REACH.min(w / 2);
    let reach_y = UNDERLAY_REACH.min(h / 2);
    let Some((ux, uy, uw, uh)) = underlay.filter(|_| reach_x >= 1 && reach_y >= 1 && content.0 >= 1 && content.1 >= 1)
    else {
        return [None; 4];
    };
    // The window's cut, in the editor surface's own coordinates.
    let (cx0, cy0) = (ux.saturating_sub(position.0), uy.saturating_sub(position.1));
    let (cx1, cy1) = (cx0.saturating_add(uw), cy0.saturating_add(uh));
    // From the centre of a corner pixel one `wl_fixed` step inward.
    let sample = |far: bool, side: i32| {
        if far {
            side.saturating_mul(256).saturating_sub(128 + 1)
        } else {
            128
        }
    };
    Corner::ALL.map(|corner| {
        let (x0, x1) = if corner.right() {
            (w - reach_x, w.saturating_add(UNDERLAY_REACH))
        } else {
            (-UNDERLAY_REACH, reach_x)
        };
        let (y0, y1) = if corner.bottom() {
            (h - reach_y, h.saturating_add(UNDERLAY_REACH))
        } else {
            (-UNDERLAY_REACH, reach_y)
        };
        let (x0, x1, y0, y1) = (x0.max(cx0), x1.min(cx1), y0.max(cy0), y1.min(cy1));
        // The patch must still hold the editor's corner square, and reach past the editor on at least one side.
        let holds_x = if corner.right() {
            x0 <= w - reach_x && x1 >= w
        } else {
            x0 <= 0 && x1 >= reach_x
        };
        let holds_y = if corner.bottom() {
            y0 <= h - reach_y && y1 >= h
        } else {
            y0 <= 0 && y1 >= reach_y
        };
        let past_x = if corner.right() { x1 > w } else { x0 < 0 };
        let past_y = if corner.bottom() { y1 > h } else { y0 < 0 };
        (holds_x && holds_y && (past_x || past_y)).then(|| CornerPatch {
            rect: (x0, y0, x1 - x0, y1 - y0),
            source: (
                sample(corner.right(), content.0),
                sample(corner.bottom(), content.1),
                1,
                1,
            ),
        })
    })
}

pub(crate) fn os_scale(placement: &Placement, scale: f64) -> f64 {
    if placement.viewport.is_some() {
        scale
    } else {
        f64::from(placement.buffer_scale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_underlay_reaches_past_the_editor_and_stops_at_the_window() {
        let inside = placement((100.0, 80.0), (0.0, 0.0), (800, 600), 1.25, 2, true).unwrap();
        assert_eq!(inside.underlay, None);
        let r = UNDERLAY_REACH;
        assert_eq!(
            inside.with_underlay((0.0, 0.0), (1200, 900)).underlay,
            Some((100 - r, 80 - r, 800 + 2 * r, 600 + 2 * r))
        );
        // An editor on the window's left and bottom edges: the underlay stops there, in the window's corner.
        let edge = placement((0.0, 40.0), (0.0, 0.0), (800, 860), 1.25, 2, true).unwrap();
        assert_eq!(
            edge.with_underlay((0.0, 0.0), (1200, 900)).underlay,
            Some((0, 40 - r, 800 + r, 860 + r))
        );
        // A window shadow moves the window inside the surface; the underlay stays out of the shadow.
        let shadowed = placement((0.0, 0.0), (24.0, 18.0), (400, 300), 1.0, 1, true).unwrap();
        assert_eq!(shadowed.position, (24, 18));
        assert_eq!(
            shadowed.with_underlay((24.0, 18.0), (400, 300)).underlay,
            Some((24, 18, 400, 300))
        );
        // Nothing of it in the window: none.
        assert_eq!(shadowed.with_underlay((2000.0, 0.0), (400, 300)).underlay, None);
    }

    #[test]
    fn corner_patches_reach_past_the_editor_and_into_it_and_sample_each_corner_pixel_from_inside() {
        let r = UNDERLAY_REACH;
        // An editor inside the window on every side: four patches, each a square of 2r around its corner.
        let p = placement((100.0, 80.0), (0.0, 0.0), (800, 600), 1.25, 2, true)
            .unwrap()
            .with_underlay((0.0, 0.0), (1200, 900));
        let patches = corner_patches(p.position, p.logical, p.content, p.underlay);
        let (cw, ch) = p.content;
        assert_eq!((cw, ch), (1000, 750));
        assert_eq!(
            patches,
            [
                Some(CornerPatch {
                    rect: (-r, -r, 2 * r, 2 * r),
                    source: (128, 128, 1, 1)
                }),
                Some(CornerPatch {
                    rect: (800 - r, -r, 2 * r, 2 * r),
                    source: (cw * 256 - 129, 128, 1, 1)
                }),
                Some(CornerPatch {
                    rect: (-r, 600 - r, 2 * r, 2 * r),
                    source: (128, ch * 256 - 129, 1, 1)
                }),
                Some(CornerPatch {
                    rect: (800 - r, 600 - r, 2 * r, 2 * r),
                    source: (cw * 256 - 129, ch * 256 - 129, 1, 1)
                }),
            ]
        );
        // Every source lies inside the frame, from a pixel's centre inward.
        for patch in patches.iter().flatten() {
            let (x, y, w, h) = patch.source;
            assert!(x >= 0 && y >= 0 && x + w <= cw * 256 && y + h <= ch * 256);
            assert!(x % 256 == 128 || (x + w) % 256 == 128);
            assert!(y % 256 == 128 || (y + h) % 256 == 128);
        }
    }

    #[test]
    fn every_source_lies_inside_the_frame_at_every_scale_and_small_size() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            for w in [1, 2, 3, 5, 21, 63, 64, 65, 801] {
                for h in [1, 2, 9, 40, 601] {
                    let p = placement((300.0, 200.0), (0.0, 0.0), (w, h), scale, 2, true)
                        .unwrap()
                        .with_underlay((0.0, 0.0), (2000, 1500));
                    let (cw, ch) = p.content;
                    for patch in corner_patches(p.position, p.logical, p.content, p.underlay)
                        .iter()
                        .flatten()
                    {
                        let (x, y, sw, sh) = patch.source;
                        assert!(sw > 0 && sh > 0, "{scale} {w}x{h}");
                        assert!(
                            x >= 0 && y >= 0 && x + sw <= cw * 256 && y + sh <= ch * 256,
                            "{scale} {w}x{h} {patch:?}"
                        );
                        let (_, _, rw, rh) = patch.rect;
                        assert!(rw > 0 && rh > 0, "{scale} {w}x{h} {patch:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_corner_on_the_window_corner_gets_no_patch_and_one_on_an_edge_reaches_along_it() {
        let r = UNDERLAY_REACH;
        // The editor on the window's left and bottom edges, as a left split above nothing.
        let p = placement((0.0, 40.0), (0.0, 0.0), (800, 860), 1.0, 1, true)
            .unwrap()
            .with_underlay((0.0, 0.0), (1200, 900));
        let [tl, tr, bl, br] = corner_patches(p.position, p.logical, p.content, p.underlay);
        assert_eq!(bl, None, "the window's own corner");
        assert_eq!(
            tl.unwrap().rect,
            (0, -r, r, 2 * r),
            "cut at the window's left edge, reaching up"
        );
        assert_eq!(tr.unwrap().rect, (800 - r, -r, 2 * r, 2 * r));
        assert_eq!(
            br.unwrap().rect,
            (800 - r, 860 - r, 2 * r, r),
            "cut at the window's bottom edge"
        );
        // A window edge 5 px past the editor: the patch stops there.
        let near = p.without_underlay().with_underlay((0.0, 0.0), (805, 900));
        let [_, tr, _, br] = corner_patches(near.position, near.logical, near.content, near.underlay);
        assert_eq!(tr.unwrap().rect, (800 - r, -r, r + 5, 2 * r));
        assert_eq!(br.unwrap().rect, (800 - r, 860 - r, r + 5, r));
        // No window known: no patches.
        assert_eq!(corner_patches(p.position, p.logical, p.content, None), [None; 4]);
    }

    #[test]
    fn corner_patches_of_a_small_editor_never_reach_another_corner() {
        let p = placement((300.0, 300.0), (0.0, 0.0), (21, 9), 2.0, 2, true)
            .unwrap()
            .with_underlay((0.0, 0.0), (1200, 900));
        let patches = corner_patches(p.position, p.logical, p.content, p.underlay);
        let rects: Vec<Rect> = patches.iter().map(|patch| patch.unwrap().rect).collect();
        let r = UNDERLAY_REACH;
        assert_eq!(
            rects,
            vec![
                (-r, -r, r + 10, r + 4),
                (11, -r, 10 + r, r + 4),
                (-r, 5, r + 10, 4 + r),
                (11, 5, 10 + r, 4 + r)
            ]
        );
        // A side of one pixel leaves no half to reach into.
        let thin = placement((300.0, 300.0), (0.0, 0.0), (1, 9), 2.0, 2, true)
            .unwrap()
            .with_underlay((0.0, 0.0), (1200, 900));
        assert_eq!(
            corner_patches(thin.position, thin.logical, thin.content, thin.underlay),
            [None; 4]
        );
    }

    #[test]
    fn the_transform_is_added() {
        let p = placement((10.0, 20.0), (3.0, 4.0), (800, 600), 1.0, 1, true).unwrap();
        assert_eq!(p.position, (13, 24));
    }

    #[test]
    fn fractional_scales_round_half_away_from_zero() {
        for (s, w, want) in [
            (1.25, 801, 1001),
            (1.5, 801, 1202),
            (1.75, 801, 1402),
            (2.0, 801, 1602),
            (1.5, 1, 2),
        ] {
            assert_eq!(device_len(w, s), want, "{s} x {w}");
        }
        let p = placement((0.0, 0.0), (0.0, 0.0), (801, 601), 1.5, 2, true).unwrap();
        assert_eq!(
            (p.content, p.buffer_scale, p.viewport),
            ((1202, 902), 1, Some((801, 601)))
        );
    }

    #[test]
    fn the_integer_path_is_always_divisible() {
        for sf in 1..=4 {
            let p = placement((0.0, 0.0), (0.0, 0.0), (333, 257), 1.5, sf, false).unwrap();
            assert_eq!(p.content, (333 * sf, 257 * sf));
            assert_eq!(
                (p.content.0 % p.buffer_scale, p.content.1 % p.buffer_scale, p.viewport),
                (0, 0, None)
            );
        }
    }

    #[test]
    fn renderer_scale_follows_the_presentation_path() {
        for viewporter in [true, false] {
            let p = placement((0.0, 0.0), (0.0, 0.0), (801, 601), 1.5, 2, viewporter).unwrap();
            assert_eq!(p.logical, (801, 601));
            assert_eq!(os_scale(&p, 1.5), if viewporter { 1.5 } else { 2.0 });
        }
    }

    #[test]
    fn negative_half_pixels_round_away_from_zero() {
        assert_eq!(device_len(-1, 1.5), -2);
        let p = placement((-1.5, -2.5), (0.0, 0.0), (1, 1), 1.0, 1, true).unwrap();
        assert_eq!(p.position, (-2, -3));
    }

    #[test]
    fn buckets_round_up_to_128() {
        assert_eq!(bucket((1, 1)), (128, 128));
        assert_eq!(bucket((128, 129)), (128, 256));
        assert_eq!(bucket((1202, 902)), (1280, 1024));
    }

    #[test]
    fn a_buffer_two_buckets_larger_is_too_large() {
        assert!(!too_large((1280, 1024), (1202, 902)));
        assert!(!too_large((1408, 1024), (1202, 902)));
        assert!(too_large((1536, 1024), (1202, 902)));
        assert!(too_large((1280, 1280), (1202, 902)));
    }

    #[test]
    fn a_zero_allocation_places_nothing() {
        assert_eq!(placement((0.0, 0.0), (0.0, 0.0), (0, 600), 1.0, 1, true), None);
        assert_eq!(placement((0.0, 0.0), (0.0, 0.0), (800, -1), 1.0, 1, false), None);
    }

    #[test]
    fn positions_round_to_the_nearest_logical_pixel() {
        assert_eq!(
            placement((10.4, 20.6), (0.0, 0.0), (1, 1), 1.0, 1, true)
                .unwrap()
                .position,
            (10, 21)
        );
        assert_eq!(
            placement((-1.6, -2.4), (0.0, 0.0), (1, 1), 1.0, 1, true)
                .unwrap()
                .position,
            (-2, -2)
        );
    }
}
