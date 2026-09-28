//! Colours as nvim reports them, and the operations token derivation needs on them.

/// One opaque sRGB colour. `nvim_get_hl` reports colours as a 24-bit integer, which is what
/// [`Rgb::from_u32`] takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub(crate) const fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb { r, g, b }
    }

    pub(crate) const fn from_u32(value: u32) -> Self {
        Rgb {
            r: ((value >> 16) & 0xff) as u8,
            g: ((value >> 8) & 0xff) as u8,
            b: (value & 0xff) as u8,
        }
    }

    /// `pub`, not `pub(crate)`: `shell/src/theme/gtk_css.rs` and `agent_panel.rs` call this
    /// directly on `ThemeTokens`' public `Rgb` fields.
    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    /// Per-channel linear interpolation in sRGB: `t = 0.0` is `self`, `t = 1.0` is `other`.
    /// `t` is clamped, so a caller's arithmetic slip cannot produce a colour outside the segment.
    pub(crate) fn mix(self, other: Rgb, t: f64) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let lerp = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as u8;
        Rgb {
            r: lerp(self.r, other.r),
            g: lerp(self.g, other.g),
            b: lerp(self.b, other.b),
        }
    }

    /// WCAG 2.x relative luminance.
    fn relative_luminance(self) -> f64 {
        let channel = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(self.r) + 0.7152 * channel(self.g) + 0.0722 * channel(self.b)
    }

    /// WCAG 2.x contrast ratio, from 1.0 to 21.0, symmetric in its arguments.
    ///
    /// `pub`, not `pub(crate)`: `shell/src/theme/gtk_css.rs`'s own tests call this directly.
    pub fn contrast(self, other: Rgb) -> f64 {
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

/// [`ensure_contrast`], with a black/white pole as the guard of last resort (sw-theme-4).
///
/// `ensure_contrast` can hand back a colour that still fails `min`: a scheme's own two colours
/// cannot always carry text on a mid-luminance surface (rose-pine dawn's `IncSearch`, `#d7827e`,
/// reaches only 2.60:1 from either of `Normal`'s colours). Black or white always reaches
/// `TEXT_CONTRAST` on one side of any background, so once the first pass fails, this falls through
/// to whichever pole contrasts more against `against` and re-runs the same stepped mix toward it --
/// stopping at the first step that passes, for the same reason `ensure_contrast` does, rather than
/// jumping straight to a pure black or white that would erase the scheme's own character.
///
/// First written for `hint_fg`; sw-theme-4 found `chrome_fg`/`chrome_muted` had been left without
/// it, extracted here so every surface-guarded text token shares one implementation.
pub(crate) fn ensure_contrast_with_pole(color: Rgb, against: Rgb, toward: Rgb, min: f64) -> Rgb {
    let guarded = ensure_contrast(color, against, toward, min);
    if guarded.contrast(against) >= min {
        return guarded;
    }
    let (black, white) = (Rgb::new(0, 0, 0), Rgb::new(0xff, 0xff, 0xff));
    let pole = if against.contrast(black) >= against.contrast(white) {
        black
    } else {
        white
    };
    ensure_contrast(color, against, pole, min)
}

/// A text colour painted on two backgrounds, guarded against both at once (sw-theme-3).
///
/// Two single-background guards in a row cannot do this: the second one mixes toward its own
/// target with no regard for the first background, and when `color` sits on the far side of `a`
/// from `toward`, the mix walks it back across `a` and undoes the first guard. This takes the first
/// 5% step from `color` that reaches `min` against **both**, trying `toward` first (the scheme's
/// own direction, as [`ensure_contrast`] does) and then the black/white poles, the one reading
/// better on both first, as [`ensure_contrast_with_pole`] does. When no step reaches `min` on both
/// -- two backgrounds on opposite sides of mid-grey leave no colour that does -- it returns the
/// step whose worse contrast is the highest, never one that trades one background away.
pub(crate) fn ensure_contrast_against_both(color: Rgb, a: Rgb, b: Rgb, toward: Rgb, min: f64) -> Rgb {
    let worse = |c: Rgb| c.contrast(a).min(c.contrast(b));
    let (black, white) = (Rgb::new(0, 0, 0), Rgb::new(0xff, 0xff, 0xff));
    let (first_pole, second_pole) = if worse(black) >= worse(white) {
        (black, white)
    } else {
        (white, black)
    };
    let mut best = color;
    for target in [toward, first_pole, second_pole] {
        for step in 0..=20 {
            let candidate = color.mix(target, f64::from(step) * 0.05);
            if worse(candidate) >= min {
                return candidate;
            }
            if worse(candidate) > worse(best) {
                best = candidate;
            }
        }
    }
    best
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
        assert_ne!(
            fixed, BLACK,
            "it must stop at the first step that passes, not jump to the end"
        );
        assert!(
            fixed.r == fixed.g && fixed.g == fixed.b,
            "it must stay on the line toward BLACK"
        );
    }

    #[test]
    fn ensure_contrast_returns_the_target_when_nothing_reaches_the_minimum() {
        assert_eq!(
            ensure_contrast(WHITE, WHITE, Rgb::new(200, 200, 200), 30.0),
            Rgb::new(200, 200, 200)
        );
    }

    #[test]
    fn the_pole_variant_leaves_a_passing_colour_alone_too() {
        assert_eq!(ensure_contrast_with_pole(BLACK, WHITE, BLACK, 4.5), BLACK);
    }

    #[test]
    fn the_two_background_guard_leaves_a_colour_passing_both_alone() {
        assert_eq!(
            ensure_contrast_against_both(BLACK, WHITE, Rgb::new(0xee, 0xee, 0xee), WHITE, 4.5),
            BLACK
        );
    }

    /// The case two sequential guards get wrong: black passes a mid-grey `a` but not a darker `b`,
    /// and mixing it toward white to clear `b` walks it back across `a`.
    #[test]
    fn the_two_background_guard_never_trades_one_background_for_the_other() {
        let (a, b) = (Rgb::from_u32(0x767676), Rgb::from_u32(0x666666));
        let fixed = ensure_contrast_against_both(BLACK, a, b, WHITE, 4.5);
        assert!(fixed.contrast(a) >= 4.5 && fixed.contrast(b) >= 4.5, "{}", fixed.hex());
    }

    /// Two backgrounds with no colour reaching `min` on both (nothing reaches 7:1 on black and on
    /// white): the best compromise, not a pole that reads on one and vanishes on the other (1:1).
    #[test]
    fn the_two_background_guard_maximises_the_worse_contrast_when_nothing_passes() {
        let fixed = ensure_contrast_against_both(Rgb::from_u32(0x777777), BLACK, WHITE, BLACK, 7.0);
        let worse = fixed.contrast(BLACK).min(fixed.contrast(WHITE));
        assert!(
            worse > 4.0,
            "{} reaches only {worse:.2} on its worse background",
            fixed.hex()
        );
    }

    /// sw-theme-4's own scenario in miniature: `ensure_contrast` toward a colour that itself never
    /// reaches `min` against `against` returns that failing colour unchanged; the pole variant must
    /// fall through to black or white instead.
    #[test]
    fn the_pole_variant_falls_through_when_the_plain_target_never_reaches_the_minimum() {
        // rose-pine dawn's real IncSearch: neither of Normal's two colours (#575279, #faf4ed)
        // reaches 4.5:1 against it.
        let ischbg = Rgb::from_u32(0xd7827e);
        let plain = ensure_contrast(Rgb::from_u32(0xfaf4ed), ischbg, Rgb::from_u32(0x575279), 4.5);
        assert!(
            plain.contrast(ischbg) < 4.5,
            "the plain guard fails here, as documented"
        );

        let fixed = ensure_contrast_with_pole(Rgb::from_u32(0xfaf4ed), ischbg, Rgb::from_u32(0x575279), 4.5);
        assert!(
            fixed.contrast(ischbg) >= 4.5,
            "the pole fallback must still reach the minimum"
        );
    }
}
