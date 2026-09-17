//! Colours as nvim reports them, and the operations token derivation needs on them.

/// One opaque sRGB colour. `nvim_get_hl` reports colours as a 24-bit integer, which is what
/// [`Rgb::from_u32`] takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub(crate) const fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb { r, g, b }
    }

    pub(crate) const fn from_u32(value: u32) -> Self {
        Rgb { r: ((value >> 16) & 0xff) as u8, g: ((value >> 8) & 0xff) as u8, b: (value & 0xff) as u8 }
    }

    pub(crate) fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    /// Per-channel linear interpolation in sRGB: `t = 0.0` is `self`, `t = 1.0` is `other`.
    /// `t` is clamped, so a caller's arithmetic slip cannot produce a colour outside the segment.
    pub(crate) fn mix(self, other: Rgb, t: f64) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let lerp = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as u8;
        Rgb { r: lerp(self.r, other.r), g: lerp(self.g, other.g), b: lerp(self.b, other.b) }
    }

    /// WCAG 2.x relative luminance.
    fn relative_luminance(self) -> f64 {
        let channel = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * channel(self.r) + 0.7152 * channel(self.g) + 0.0722 * channel(self.b)
    }

    /// WCAG 2.x contrast ratio, from 1.0 to 21.0, symmetric in its arguments.
    pub(crate) fn contrast(self, other: Rgb) -> f64 {
        let (a, b) = (self.relative_luminance(), other.relative_luminance());
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }
}

/// `color` if it already reaches `min` contrast against `against`; otherwise `color` mixed toward
/// `toward` in 5% steps, stopping at the first step that passes. Returns `toward` when no step does.
///
/// Stopping at the first passing step is what keeps a colourscheme's character: a too-pale comment
/// gets just dark enough to read, not replaced by the foreground colour.
pub(crate) fn ensure_contrast(color: Rgb, against: Rgb, toward: Rgb, min: f64) -> Rgb {
    for step in 0..=20 {
        let candidate = color.mix(toward, f64::from(step) * 0.05);
        if candidate.contrast(against) >= min {
            return candidate;
        }
    }
    toward
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLACK: Rgb = Rgb::new(0, 0, 0);
    const WHITE: Rgb = Rgb::new(255, 255, 255);

    #[test]
    fn nvim_integers_round_trip_to_hex() {
        assert_eq!(Rgb::from_u32(0xfaf4ed), Rgb::new(0xfa, 0xf4, 0xed));
        assert_eq!(Rgb::from_u32(0xfaf4ed).hex(), "#faf4ed");
        assert_eq!(Rgb::from_u32(0x000000).hex(), "#000000");
    }

    #[test]
    fn mix_interpolates_per_channel_and_clamps_t() {
        assert_eq!(BLACK.mix(WHITE, 0.0), BLACK);
        assert_eq!(BLACK.mix(WHITE, 1.0), WHITE);
        assert_eq!(BLACK.mix(WHITE, 0.5), Rgb::new(128, 128, 128));
        assert_eq!(BLACK.mix(WHITE, 7.0), WHITE);
        assert_eq!(BLACK.mix(WHITE, -1.0), BLACK);
    }

    #[test]
    fn contrast_is_the_wcag_ratio() {
        assert!((BLACK.contrast(WHITE) - 21.0).abs() < 1e-9);
        assert!((WHITE.contrast(BLACK) - 21.0).abs() < 1e-9);
        assert!((WHITE.contrast(WHITE) - 1.0).abs() < 1e-9);
        // rose-pine-dawn's Comment on its Normal background: readable to its author, 2.73:1 by WCAG.
        let ratio = Rgb::from_u32(0x9893a5).contrast(Rgb::from_u32(0xfaf4ed));
        assert!((2.7..2.8).contains(&ratio), "got {ratio}");
    }

    #[test]
    fn ensure_contrast_leaves_a_passing_colour_alone() {
        assert_eq!(ensure_contrast(BLACK, WHITE, BLACK, 4.5), BLACK);
    }

    #[test]
    fn ensure_contrast_moves_a_failing_colour_toward_the_target_just_far_enough() {
        let pale = Rgb::new(0xf0, 0xf0, 0xf0);
        let fixed = ensure_contrast(pale, WHITE, BLACK, 4.5);
        assert!(fixed.contrast(WHITE) >= 4.5);
        assert_ne!(fixed, BLACK, "it must stop at the first step that passes, not jump to the end");
        assert!(fixed.r == fixed.g && fixed.g == fixed.b, "it must stay on the line toward BLACK");
    }

    #[test]
    fn ensure_contrast_returns_the_target_when_nothing_reaches_the_minimum() {
        assert_eq!(ensure_contrast(WHITE, WHITE, Rgb::new(200, 200, 200), 30.0), Rgb::new(200, 200, 200));
    }
}
