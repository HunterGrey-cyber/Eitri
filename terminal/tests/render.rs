//! Bytes from a real child, through the session, painted by the product's `paint` onto a CPU
//! raster: the pixels, not the op list, are what is checked.

mod common;

use common::{metrics, size, spec, Harness, Raster, WAIT};
use neovibe_terminal::{SessionCommand, SessionConfig, TerminalColors};
use terminal_render::RgbColor;

const BG: RgbColor = RgbColor::new(0xfa, 0xf4, 0xed); // rose-pine dawn's base, a LIGHT background
const FG: RgbColor = RgbColor::new(0x57, 0x52, 0x79);

/// What `shell`'s `colors_from` hands over: the theme's background and foreground, and no fixed
/// cursor colour (GUI pass 2026-09-23, defect 3).
fn themed() -> TerminalColors {
    TerminalColors {
        background: BG,
        foreground: FG,
        cursor: None,
    }
}

fn start(script: &str, focused: bool) -> Harness {
    start_colored(script, focused, themed())
}

fn start_colored(script: &str, focused: bool, colors: TerminalColors) -> Harness {
    Harness::start_with(SessionConfig {
        spawn: spec("/bin/sh", &["-c", script]),
        size: size(40, 6),
        colors,
        focused,
        tap: None,
    })
}

#[test]
fn printed_text_paints_ink_in_every_cell_on_the_themes_background() {
    let mut h = start("printf 'hello-neovibe\\n'; exec sleep 5", true);
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l == "hello-neovibe"));
    let list = h.frame.clone().unwrap();
    assert_eq!(list.surface_background, BG, "the theme's background, not xterm's black");
    let m = metrics();
    let r = Raster::of(&list, &m);
    let bg = (BG.r, BG.g, BG.b);
    assert_eq!(r.at(r.width - 1, r.height - 1), bg);
    for col in 0..13 {
        assert!(r.ink(&m, 0, col, 1, bg) > 0, "no ink in cell {col} of 'hello-neovibe'");
    }
    assert!(r.ink(&m, 1, 0, 1, bg) > 0, "the cursor on row 1");
}

/// Solid when the terminal holds the keys, hollow when it does not (`shell/src/pane_focus.rs`'s rule).
#[test]
fn an_unfocused_cursor_is_hollow() {
    let mut h = start("exec sleep 5", true);
    h.wait_for(WAIT, |h| h.frame.is_some());
    let m = metrics();
    let bg = (BG.r, BG.g, BG.b);
    let solid = Raster::of(h.frame.as_ref().unwrap(), &m).ink(&m, 0, 0, 1, bg);
    // `before` must be read BEFORE sending `Focus(false)` (fix round 1, review finding 3): if the
    // session thread renders the focus change first, `before` would already be the hollow frame's
    // own render count and `wait_for` below would then time out waiting for a count that can never
    // increase again (the script prints nothing more).
    let before = h.session.renders();
    h.session.send(SessionCommand::Focus(false));
    h.wait_for(WAIT, |h| h.session.renders() > before);
    let hollow = Raster::of(h.frame.as_ref().unwrap(), &m).ink(&m, 0, 0, 1, bg);
    assert!(hollow > 0 && hollow * 2 < solid, "solid {solid}px, hollow {hollow}px");
}

/// Bold default-coloured text on a light theme: xterm's bright foreground is white, which would be
/// white on near-white. It follows the theme's foreground instead.
#[test]
fn bold_default_text_stays_readable_on_a_light_background() {
    let mut h = start(r"printf '\033[1mBOLD\033[0m'; exec sleep 5", true);
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l == "BOLD"));
    let colors = h.frame.as_ref().unwrap().ops.iter().find_map(|op| match op {
        terminal_render::PaintOp::DrawText {
            row: 0, col: 0, color, ..
        } => Some(*color),
        _ => None,
    });
    assert_eq!(colors, Some(FG));
}

/// GUI pass 2026-09-23, defect 3: the cursor was drawn in the theme's foreground whatever the cell
/// under it, so on a cell a program painted in its own light colours under a DARK theme -- nvim
/// with LazyVim's light scheme, inside -- it was a pale block on a pale cell. Painted here, pixels
/// read back: the block is the covered cell's own ink colour, and the character inside it is the
/// cell's own background, as foot draws it.
#[test]
fn a_block_cursor_on_a_cell_a_program_painted_takes_that_cells_colours_swapped() {
    const PALE: RgbColor = RgbColor::new(0xc8, 0xc8, 0xc8);
    let dark_theme = TerminalColors {
        background: RgbColor::new(0x14, 0x16, 0x1b),
        foreground: PALE,
        cursor: None,
    };
    // rose-pine dawn's own pair, painted by the program, then the cursor put back onto it.
    let script = r"printf '\033[48;2;250;244;237m\033[38;2;87;82;121mX\033[0m\033[1;1H'; exec sleep 5";
    let mut h = start_colored(script, true, dark_theme);
    let cursor_home = |h: &Harness| {
        h.frame.as_ref().is_some_and(|list| {
            matches!(
                list.cursor(),
                Some(terminal_render::PaintOp::DrawCursor { row: 0, col: 0, .. })
            )
        }) && h.text().first().is_some_and(|l| l == "X")
    };
    h.wait_for(WAIT, cursor_home);
    let m = metrics();
    let r = Raster::of(h.frame.as_ref().unwrap(), &m);
    let cell = m.cell_rect(0, 0, 1);
    // A corner of the cell, clear of the glyph: the cursor's body.
    let body = r.at(cell.left as i32 + 1, cell.top as i32 + 1);
    assert_eq!(
        body,
        (87, 82, 121),
        "the block is the cell's own foreground, not the theme's pale one"
    );
    // And the character is still there, drawn in the cell's background on top of it.
    let glyph = r.ink(&m, 0, 0, 1, body);
    assert!(glyph > 0, "the X under the block must stay readable");
}
