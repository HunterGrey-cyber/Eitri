//! Placement and buffer sizing for the editor's child surface.
//!
//! GTK maps native coordinates to surface coordinates by adding the native's surface transform.
//! A window shadow makes that transform positive, placing the content inside the shadow's extent.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Placement {
    pub position: (i32, i32),
    pub logical: (i32, i32),
    pub content: (i32, i32),
    pub buffer_scale: i32,
    pub viewport: Option<(i32, i32)>,
}

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
