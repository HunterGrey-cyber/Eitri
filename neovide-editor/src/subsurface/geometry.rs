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
