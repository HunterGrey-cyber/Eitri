//! Palette resolution and the BOLD/DIM rules.
//!
//! `FrameColor` deliberately does NOT decide what bold means -- it carries
//! `Palette(index)` or `Rgb(..)` and leaves "bold makes ANSI 0..8 bright" to the
//! renderer. This module is that renderer rule, kept in one place so the
//! §10 colour cases have a single thing to test.

use terminal_frame::frame::{CellFlags, FrameColor, Rgb as FrameRgb, PALETTE_LEN};

use crate::paint::RgbColor;

/// Palette index of `NamedColor::Foreground`.
pub const FOREGROUND: u16 = 256;
/// Palette index of `NamedColor::Background`.
pub const BACKGROUND: u16 = 257;
/// Palette index of `NamedColor::Cursor`.
pub const CURSOR: u16 = 258;
/// Palette index of `NamedColor::DimBlack`; `DimBlack..=DimWhite` is 259..=266.
pub const DIM_BLACK: u16 = 259;
/// Palette index of `NamedColor::BrightForeground`.
pub const BRIGHT_FOREGROUND: u16 = 267;
/// Palette index of `NamedColor::DimForeground`.
pub const DIM_FOREGROUND: u16 = 268;

/// The 269-entry colour table, already resolved to RGB.
///
/// Built from a default and then overlaid with the frame's `color_overrides`,
/// which is how OSC 4/10/11/12 reach the renderer.
#[derive(Debug, Clone)]
pub struct Palette {
    entries: [RgbColor; PALETTE_LEN],
}

impl Palette {
    /// The standard xterm-compatible table: 16 named, a 6x6x6 cube, a 24-step
    /// greyscale ramp, and the special slots.
    ///
    /// The exact RGB values here are a RENDERER choice and nothing semantic may
    /// depend on them (§10). They exist so the paint list is fully resolved and
    /// the Skia backend never has to know what a palette is.
    pub fn xterm_default() -> Self {
        let mut e = [RgbColor::new(0, 0, 0); PALETTE_LEN];

        const BASE16: [(u8, u8, u8); 16] = [
            (0x00, 0x00, 0x00),
            (0xcd, 0x00, 0x00),
            (0x00, 0xcd, 0x00),
            (0xcd, 0xcd, 0x00),
            (0x00, 0x00, 0xee),
            (0xcd, 0x00, 0xcd),
            (0x00, 0xcd, 0xcd),
            (0xe5, 0xe5, 0xe5),
            (0x7f, 0x7f, 0x7f),
            (0xff, 0x00, 0x00),
            (0x00, 0xff, 0x00),
            (0xff, 0xff, 0x00),
            (0x5c, 0x5c, 0xff),
            (0xff, 0x00, 0xff),
            (0x00, 0xff, 0xff),
            (0xff, 0xff, 0xff),
        ];
        for (i, (r, g, b)) in BASE16.iter().enumerate() {
            e[i] = RgbColor::new(*r, *g, *b);
        }

        // 16..232: the 6x6x6 cube, in xterm's non-linear steps.
        const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let mut i = 16usize;
        for r in STEPS {
            for g in STEPS {
                for b in STEPS {
                    e[i] = RgbColor::new(r, g, b);
                    i += 1;
                }
            }
        }
        // 232..256: greyscale ramp.
        for j in 0..24u8 {
            let v = 8 + j * 10;
            e[232 + j as usize] = RgbColor::new(v, v, v);
        }

        e[FOREGROUND as usize] = RgbColor::new(0xd8, 0xd8, 0xd8);
        e[BACKGROUND as usize] = RgbColor::new(0x18, 0x18, 0x18);
        e[CURSOR as usize] = RgbColor::new(0xd8, 0xd8, 0xd8);
        // Dim variants of the eight base colours: alacritty derives these as
        // two-thirds intensity.
        for k in 0..8usize {
            let c = e[k];
            e[DIM_BLACK as usize + k] = RgbColor::new(
                (c.r as u16 * 2 / 3) as u8,
                (c.g as u16 * 2 / 3) as u8,
                (c.b as u16 * 2 / 3) as u8,
            );
        }
        e[BRIGHT_FOREGROUND as usize] = RgbColor::new(0xff, 0xff, 0xff);
        let fg = e[FOREGROUND as usize];
        e[DIM_FOREGROUND as usize] = RgbColor::new(
            (fg.r as u16 * 2 / 3) as u8,
            (fg.g as u16 * 2 / 3) as u8,
            (fg.b as u16 * 2 / 3) as u8,
        );

        Self { entries: e }
    }

    /// Overlay the frame's OSC-driven overrides.
    pub fn apply_overrides(&mut self, overrides: &[terminal_frame::frame::ColorOverride]) {
        for o in overrides {
            let idx = o.index as usize;
            if idx >= PALETTE_LEN {
                continue;
            }
            match o.color {
                // `None` is a RESET to the built-in default for that slot, not a
                // transparent colour -- OSC 104/110/111 reset rather than clear.
                None => self.entries[idx] = Self::xterm_default().entries[idx],
                Some(rgb) => self.entries[idx] = from_frame_rgb(rgb),
            }
        }
    }

    pub fn get(&self, index: u16) -> RgbColor {
        self.entries.get(index as usize).copied().unwrap_or_default()
    }
}

/// Map a palette index through alacritty's BOLD brightening.
///
/// Only the eight base ANSI colours and the default foreground brighten. An
/// indexed colour from the cube or the greyscale ramp does NOT -- `Indexed(129)`
/// with BOLD stays 129. Getting that wrong is the classic "bold made my
/// 256-colour prompt change hue" bug.
fn to_bright(index: u16) -> u16 {
    match index {
        0..=7 => index + 8,
        FOREGROUND => BRIGHT_FOREGROUND,
        DIM_FOREGROUND => FOREGROUND,
        i @ DIM_BLACK..=266 => i - DIM_BLACK,
        _ => index,
    }
}

/// Map a palette index through alacritty's DIM darkening.
fn to_dim(index: u16) -> u16 {
    match index {
        0..=7 => DIM_BLACK + index,
        8..=15 => index - 8,
        FOREGROUND => DIM_FOREGROUND,
        BRIGHT_FOREGROUND => FOREGROUND,
        _ => index,
    }
}

/// The foreground and background a cell is actually painted with.
///
/// Order matters and is alacritty's: BOLD/DIM adjust the FOREGROUND first, and
/// INVERSE swaps the two afterwards. Swapping first would make `inverse + bold`
/// brighten the background instead of the text, which is the single most common
/// way to get §10's combination cases wrong.
pub fn resolve(fg: FrameColor, bg: FrameColor, flags: CellFlags, palette: &Palette) -> (RgbColor, RgbColor) {
    let mut fg_idx = fg;
    if flags.contains(CellFlags::BOLD) && !flags.contains(CellFlags::DIM) {
        if let FrameColor::Palette(i) = fg_idx {
            fg_idx = FrameColor::Palette(to_bright(i));
        }
    }
    if flags.contains(CellFlags::DIM) {
        if let FrameColor::Palette(i) = fg_idx {
            fg_idx = FrameColor::Palette(to_dim(i));
        }
    }

    let mut fg_rgb = to_rgb(fg_idx, palette);
    let mut bg_rgb = to_rgb(bg, palette);

    if flags.contains(CellFlags::INVERSE) {
        std::mem::swap(&mut fg_rgb, &mut bg_rgb);
    }
    (fg_rgb, bg_rgb)
}

/// A `terminal-frame` colour to the crate's own resolved colour.
pub fn from_frame_rgb(c: FrameRgb) -> RgbColor {
    RgbColor::new(c.r, c.g, c.b)
}

/// Resolve a frame colour -- palette index or direct 24-bit -- to RGB.
pub fn to_rgb(color: FrameColor, palette: &Palette) -> RgbColor {
    match color {
        FrameColor::Palette(i) => palette.get(i),
        FrameColor::Rgb(c) => from_frame_rgb(c),
    }
}
