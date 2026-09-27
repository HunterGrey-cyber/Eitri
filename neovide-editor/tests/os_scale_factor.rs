//! `LiveHarness::set_os_scale_factor` really re-rasterizes an EMBEDDED editor -- against a real
//! nvim.
//!
//! This is the v1 P2 counterpart to `scale_factor_setting.rs`'s own header, for the *other* half
//! of the scale-factor product: `os_scale_factor` (the OS/display scale a host reads off
//! `Widget::scale_factor()`) rather than `g:neovide_scale_factor` (the user zoom). Before fork
//! commit `LiveHarness::set_os_scale_factor` (2026-09-27), `os_scale_factor` was read exactly
//! once, at construction, and had **no way to change afterward** -- a window dragged onto a
//! monitor of a different scale, or GTK's own `notify::scale-factor`, changed nothing on screen at
//! all (D2, `docs/superpowers/plans/2026-09-27-v1-scale.md`).
//!
//! Six checks, one launch, following `scale_factor_setting.rs`'s own pattern (`Frames`,
//! `ink_row_height`, `assert_ratio` copied rather than shared, per that file's own precedent --
//! this is a plain `main` too, and winit allows one `EventLoop` per process):
//!
//! 1. `set_os_scale_factor(2.0)` returns `true`; `os_scale_factor()` reads back `2.0`; the cell
//!    width is about 2x its scale-1 baseline after one frame;
//! 5. **folded into check 1's own transition** (same os-scale change, no separate one): the pixels
//!    themselves change, not just `grid_scale()`'s number -- the ink height of row 0's 'MMM' grows
//!    by about the same ratio, with **no `resize_grid` call anywhere in this file** (the integer
//!    grid is unchanged; only the cell size moved). This is the one check the forced redraw
//!    (`LiveHarness::render_frame`'s `os_scale_redraw_pending` folded into
//!    `scale_factor_changed`) can actually fail, and was red-checked by temporarily reverting that
//!    line to `apply_scale_factor_setting()` alone (see the fork commit's own message for both
//!    runs' verbatim output: red, ratio ~= 1.0 -- the glyph picture never re-recorded; green,
//!    ratio ~= 2.0 once the OR is restored);
//! 2. `set_os_scale_factor(2.0)` again returns `false`, and neither the cell width nor
//!    `os_scale_factor_resyncs()` moves across 30 idle frames -- the P11-class idle-cost gate,
//!    same shape as `scale_factor_setting.rs`'s own check 4;
//! 3. composes with `g:neovide_scale_factor`: at os-scale 2.0, `:let g:neovide_scale_factor = 1.5`
//!    moves the cell to about 3x the scale-1 baseline (2.0 x 1.5), and `:let ... = 1.0` returns it
//!    to about 2x -- `os_scale_factor` is left exactly as the host set it, `sync_scale_factor`
//!    multiplies the two (`apply_scale_factor_setting`'s own doc);
//! 4. `set_os_scale_factor(0.0)`, `(-1.0)` and `(f64::NAN)` all return `false` and change nothing
//!    at all -- not `os_scale_factor()`, not the cell width;
//! 6. `set_os_scale_factor(1.0)` returns `true` and the cell width returns to the original,
//!    unscaled baseline.
//!
//! Ignored unless asked for: it spawns a real `nvim` (>= 0.10 on `PATH`) and connects to the
//! session's display for the clipboard (no window). A plain `main` (`harness = false`) because
//! winit allows one `EventLoop` per process, on the main thread only. Run with:
//!
//!     cargo test -p neovide-editor --test os_scale_factor -- --ignored
use std::time::{Duration, Instant};

use neovide::{
    live_harness::{LiveHarness, LiveHarnessOptions},
    units::{GridSize, PixelRect},
};
use skia_safe::{surfaces, Color, Surface};

struct Frames {
    surface: Surface,
    region: PixelRect<f32>,
}

impl Frames {
    fn new() -> Self {
        let surface = surfaces::raster_n32_premul((320, 160)).expect("raster surface");
        Self {
            surface,
            region: PixelRect::from_min_max((0.0, 0.0), (320.0, 160.0)),
        }
    }

    fn step(&mut self, harness: &mut LiveHarness) {
        harness.render_frame(self.surface.canvas(), Some(&self.region), 1.0 / 60.0);
        std::thread::sleep(Duration::from_millis(16));
    }
}

/// Pumps until `g:neovide_scale_factor` reads `want`, then one more frame so `render_frame` has
/// applied it. Copied from `scale_factor_setting.rs` -- check 3 (composing with the user zoom)
/// needs the same wait, and `set_os_scale_factor` itself is synchronous so the other checks below
/// only ever need a single `frames.step`.
fn wait_for_user_scale(harness: &mut LiveHarness, frames: &mut Frames, want: f32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while (harness.scale_factor_setting() - want).abs() > f32::EPSILON {
        assert!(
            Instant::now() < deadline,
            "{what}: scale_factor_setting() never became {want} (it is {})",
            harness.scale_factor_setting()
        );
        frames.step(harness);
    }
    frames.step(harness);
}

fn cell_width(harness: &LiveHarness) -> f32 {
    harness.grid_scale().width()
}

fn pixel(surface: &mut Surface, x: i32, y: i32) -> Color {
    let pixmap = surface.peek_pixels().expect("raster surface must expose its pixels");
    pixmap.get_color((x, y))
}

/// Whether any pixel across `0..x1` on row `y` differs from `background`.
fn row_has_ink(surface: &mut Surface, y: i32, x1: i32, background: Color) -> bool {
    (0..x1).any(|x| pixel(surface, x, y) != background)
}

/// Height, in pixels, of the first contiguous run of non-background ink across the first
/// `text_columns` columns, scanning down from y=0 -- row 0's own glyph height, whatever cell size
/// is currently in force. Copied verbatim from `scale_factor_setting.rs` (see that file's own doc
/// for why "first contiguous run" rather than a fixed band, and why the cursor sits several rows
/// below the text in this test's own launch too).
fn ink_row_height(surface: &mut Surface, scan_height: i32, cell_width: f32, text_columns: u32) -> f32 {
    let x1 = (text_columns as f32 * cell_width).ceil() as i32;
    let background = pixel(surface, x1 + (cell_width * 2.0) as i32, 1);
    let min_y = (0..scan_height)
        .find(|&y| row_has_ink(surface, y, x1, background))
        .expect("no ink found in the text row -- did the text render at all?");
    let max_y = (min_y..scan_height)
        .take_while(|&y| row_has_ink(surface, y, x1, background))
        .last()
        .unwrap();
    (max_y - min_y + 1) as f32
}

/// The ratio a font's cell width (or ink height) moves by is close to the scale ratio but not
/// exact -- hinting and pixel snapping round it -- so this asks for "moved, in the right
/// direction, by about the right amount", loose enough to survive a font change and tight enough
/// to catch "did not move" (ratio 1.0) and "moved the wrong way". Copied from
/// `scale_factor_setting.rs`.
fn assert_ratio(before: f32, after: f32, expected: f32, what: &str) {
    let ratio = after / before;
    assert!(
        (ratio - expected).abs() < 0.12,
        "{what}: went {before} -> {after} (ratio {ratio:.3}), expected about {expected:.3}"
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("os_scale_factor: ignored (spawns a real nvim); run with `-- --ignored`");
        return;
    }

    let mut harness = LiveHarness::with_options(LiveHarnessOptions {
        os_scale_factor: 1.0,
        grid_size: Some(GridSize::new(40u32, 8u32)),
        extra_nvim_args: vec!["--clean".to_string()],
        ..Default::default()
    })
    .expect("LiveHarness::with_options failed -- is `nvim` (>= 0.10) on $PATH?");

    let mut frames = Frames::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !harness.is_ready() {
        assert!(
            Instant::now() < deadline && !harness.has_neovim_exited(),
            "nvim never became ready"
        );
        frames.step(&mut harness);
    }

    // Put real text ('MMM') on row 0, and four more (empty) lines below it so the cursor lands on
    // row 4 -- far enough from row 0 that the cursor's own (always-fresh) pixels never bleed into
    // row 0's ink measurement. Same shape `scale_factor_setting.rs`'s own check 6 uses, and for the
    // same reason (see that file's comment on the two wrong readings it took to get here).
    const TEXT_COLUMNS: u32 = 3;
    const SCAN_HEIGHT: i32 = 80;
    harness.send_text_input("iMMM<Esc>o<Esc>o<Esc>o<Esc>o<Esc>");
    for _ in 0..10 {
        frames.step(&mut harness);
    }

    assert_eq!(
        harness.os_scale_factor(),
        1.0,
        "the harness must start at the os scale it was constructed with"
    );
    let baseline_cell_width = cell_width(&harness);
    let baseline_ink = ink_row_height(&mut frames.surface, SCAN_HEIGHT, baseline_cell_width, TEXT_COLUMNS);
    println!(
        "ok: 'MMM' on screen at os scale 1.0; cell width {baseline_cell_width}, row-0 ink height {baseline_ink}px"
    );

    // 1 (+5, folded in): set_os_scale_factor(2.0) returns true, reads back, and the cell -- and the
    // pixels themselves -- really move. No resize_grid call anywhere in this file: the integer
    // grid is untouched, only the cell size is.
    assert!(
        harness.set_os_scale_factor(2.0),
        "set_os_scale_factor(2.0) must report a real change"
    );
    assert_eq!(
        harness.os_scale_factor(),
        2.0,
        "os_scale_factor() must read back what was just set"
    );
    frames.step(&mut harness);
    let cell_at_2 = cell_width(&harness);
    assert_ratio(baseline_cell_width, cell_at_2, 2.0, "os scale 1.0 -> 2.0, cell width");
    println!("ok: set_os_scale_factor(2.0) moves the cell width ({baseline_cell_width} -> {cell_at_2})");

    let ink_at_2 = ink_row_height(&mut frames.surface, SCAN_HEIGHT, cell_at_2, TEXT_COLUMNS);
    assert_ratio(
        baseline_ink,
        ink_at_2,
        2.0,
        "os scale 1.0 -> 2.0, row-0 ink height (the forced redraw)",
    );
    println!("ok: the forced redraw re-records the glyph picture at the new cell size (ink height {baseline_ink} -> {ink_at_2})");

    // 2. Reassigning the same value is a no-op, and stays one across 30 idle frames -- the
    //    P11-class idle-cost gate, same shape as scale_factor_setting.rs's own check 4.
    assert!(
        !harness.set_os_scale_factor(2.0),
        "reassigning the value already in force must be a no-op"
    );
    let resyncs_before_idle = harness.os_scale_factor_resyncs();
    for _ in 0..30 {
        frames.step(&mut harness);
        assert_eq!(
            cell_width(&harness),
            cell_at_2,
            "the cell width drifted with no assignment"
        );
        assert_eq!(
            harness.os_scale_factor_resyncs(),
            resyncs_before_idle,
            "set_os_scale_factor resynced the renderer with nothing assigned -- the P11-class \
             idle-cost regression the change gate exists to prevent"
        );
    }
    println!("ok: an unchanged os scale leaves the editor alone (resync counter held at {resyncs_before_idle})");

    // 3. Composes with g:neovide_scale_factor: os scale stays 2.0 throughout, only the user zoom
    //    moves. os_scale_factor is left exactly as the host set it (apply_scale_factor_setting's
    //    own doc); sync_scale_factor multiplies the two.
    harness.send_text_input(":let g:neovide_scale_factor = 1.5<CR>");
    wait_for_user_scale(&mut harness, &mut frames, 1.5, "compose: user 1.5 on top of os 2.0");
    let cell_at_2x1_5 = cell_width(&harness);
    assert_ratio(baseline_cell_width, cell_at_2x1_5, 3.0, "os 2.0 x user 1.5 = 3.0");
    println!("ok: os scale 2.0 composes with user scale 1.5 (cell width {cell_at_2x1_5}, ~3x baseline)");

    harness.send_text_input(":let g:neovide_scale_factor = 1.0<CR>");
    wait_for_user_scale(&mut harness, &mut frames, 1.0, "compose: back to user 1.0");
    let cell_back_to_2 = cell_width(&harness);
    assert_ratio(baseline_cell_width, cell_back_to_2, 2.0, "back to os scale 2.0 alone");
    println!("ok: dropping the user zoom back to 1.0 returns to os scale 2.0 alone ({cell_back_to_2})");

    // 4. Non-finite / non-positive values are rejected outright: no return-true, no effect on
    //    os_scale_factor(), no effect on the cell width.
    for bad in [0.0_f64, -1.0_f64, f64::NAN] {
        assert!(
            !harness.set_os_scale_factor(bad),
            "set_os_scale_factor({bad}) must be rejected"
        );
    }
    assert_eq!(
        harness.os_scale_factor(),
        2.0,
        "a rejected value must not change os_scale_factor()"
    );
    frames.step(&mut harness);
    assert_eq!(
        cell_width(&harness),
        cell_back_to_2,
        "a rejected value must not move the cell width"
    );
    println!("ok: 0.0, -1.0 and NaN are all rejected with no effect");

    // 6. Returns cleanly to the original, unscaled baseline.
    assert!(
        harness.set_os_scale_factor(1.0),
        "set_os_scale_factor(1.0) must report a real change"
    );
    frames.step(&mut harness);
    assert_eq!(harness.os_scale_factor(), 1.0);
    let cell_final = cell_width(&harness);
    assert_ratio(
        baseline_cell_width,
        cell_final,
        1.0,
        "back to the original os scale 1.0",
    );
    println!("ok: set_os_scale_factor(1.0) returns to the original baseline ({cell_final})");

    assert!(harness.shutdown(), "nvim did not exit cleanly");
    println!("os_scale_factor: all passed");
}
