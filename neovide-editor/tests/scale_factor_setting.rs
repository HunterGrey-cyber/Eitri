//! `g:neovide_scale_factor` really rescales an EMBEDDED editor -- against a real nvim.
//!
//! This is the test that did not exist, for the reason it matters: in standalone Neovide the
//! variable is applied by `window_wrapper.rs`, and an embedding never goes through that file. So
//! until fork commit `LiveHarness::apply_scale_factor_setting` (2026-09-22) the variable was
//! **stored and never applied** -- a `:let` inside neovibe changed nothing on screen, in either
//! pane. `shell` makes the agent panel follow this variable (`Ctrl+=` zooms both panes); that is
//! worthless if the editor itself does not move, and only `grid_scale()` can say whether it did.
//!
//! Six checks, one launch:
//!
//! 1. a value set before `ui_attach` (`--cmd`, the `init.lua` path) is applied on the first frame --
//!    the harness's `None` sentinel -- not merely reported. Asserted directly once check 2 supplies
//!    a baseline to divide by (an earlier revision only ever asserted this reading via check 2's own
//!    `assert_ratio` call, never in check 1's own right);
//! 2. a runtime `:let` rescales the renderer: the cell width shrinks by the ratio of the two
//!    scales. This is the positive control that makes 1 meaningful, since 1 alone cannot tell
//!    "applied" from "never scaled at all";
//! 3. `set_scale_factor_setting` really assigns the variable in nvim and the renderer follows;
//! 4. an unchanged variable leaves the cell size alone across many frames (no resync churn) --
//!    and, directly, so does `LiveHarness::scale_factor_resyncs()`, the diagnostic counter fork
//!    commit `bcd65c0` added specifically so this property has a signal of its own rather than
//!    relying on the cell width's silence to imply it;
//! 5. `g:neovide` reads truthy inside this embedded session. This matters because the owner's own
//!    `~/.config/nvim/init.lua` maps `<C-+>`/`<C-->`/`<C-0>` to change `g:neovide_scale_factor`
//!    only `if vim.g.neovide` -- if this embedding left that variable unset or false, those
//!    mappings would be dead code and the "both panes zoom together" design would never fire from
//!    the owner's own config, no matter what this file's first four checks prove.
//! 6. **the pixels themselves change, not just `grid_scale()`'s number.** Checks 1-5 all read
//!    `grid_scale()`/`scale_factor_setting()` -- the renderer's own *idea* of the cell size --
//!    which is exactly what would still move even if `LiveHarness::render_frame` kept calling
//!    `prepare_lines(false)` on the frame the scale changes: `grid_renderer.grid_scale` is updated
//!    by `sync_scale_factor()` regardless, it is *line pictures already recorded at the old size*
//!    that only `prepare_lines(true)` re-records. So this check puts real text ('M's) on screen,
//!    changes the scale with **no `resize_grid` call anywhere in this file** -- the same "integer
//!    grid did not change" shape as every other check here -- and measures the ink: the height, in
//!    pixels, of the first contiguous run of non-background pixels in the text's own row (see
//!    `ink_row_height`'s own doc for why "contiguous run" rather than "bounding box over a fixed
//!    band"). It is the one check in this file that the forced redraw
//!    (`LiveHarness::render_frame`'s `prepare_lines(scale_factor_changed)`) can actually fail, and
//!    was red-checked by temporarily reverting that line to `prepare_lines(false)` (see the fork's
//!    own commit message and `.superpowers/zoom/L1-report.md` for both runs' verbatim output).
//!
//! Ignored unless asked for: it spawns a real `nvim` (>= 0.10 on `PATH`) and connects to a display
//! for the clipboard (no window) -- **its own `Xvfb`** since 2026-09-29
//! (`shell/tests/support/own_x_server.rs`), started and pointed to before winit connects, with any
//! inherited `WAYLAND_DISPLAY`/`DISPLAY` dropped. Before that it took the session's display, the
//! desktop's from any agent shell. A plain `main` (`harness = false`) because winit allows one
//! `EventLoop` per process, on the main thread only. Run with, and no wrapper is needed:
//!
//!     cargo test -p neovide-editor --test scale_factor_setting -- --ignored
use std::time::{Duration, Instant};

use neovide::{
    live_harness::{LiveHarness, LiveHarnessOptions},
    units::{GridSize, PixelRect},
};
use skia_safe::{surfaces, Color, Surface};

// The display this test runs on: its own Xvfb, never an inherited one (2026-09-29). The one helper
// every display-connecting test in the workspace shares, so it lives with `shell`'s.
#[path = "../../shell/tests/support/own_x_server.rs"]
mod own_x_server;

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

/// Pumps until the variable reads `want`, then one more frame so `render_frame` has applied it.
fn wait_for_setting(harness: &mut LiveHarness, frames: &mut Frames, want: f32, what: &str) {
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

/// Height, in pixels, of the *first contiguous run* of non-background ink across the first
/// `text_columns` columns, scanning down from y=0 -- row 0's own glyph height, whatever cell size
/// is currently in force. Proof that the glyph *picture* was actually re-recorded at the new cell
/// size, which `grid_scale()` moving does not by itself prove (see check 6's own doc above).
///
/// This does not stop at a fixed row boundary -- an earlier revision scanned `0..cell_height` and
/// was wrong: a cursor sitting just one row below the text (which, unlike a line's own picture, is
/// redrawn fresh every frame at its live position and size, so it grows correctly regardless of
/// this bug) ended up flush against a freshly-large row 0 with no background gap between them, and
/// scanning all the way to the *new*, larger `cell_height` reached into it -- so "ink somewhere in
/// that band" let the cursor masquerade as row 0 growing even when row 0's own picture had not
/// been touched. Scanning for the first contiguous run instead, stopping at the first background
/// row once ink has started, measures only row 0's own glyphs regardless of where anything below
/// it sits or why -- `main` also keeps the cursor several rows further down for good measure.
/// `scan_height` just needs to be generously past row 0's own height and short of wherever the
/// cursor ends up, at every scale this test uses; the background sample point (one column past the
/// text, row 1) makes no assumption about the colorscheme.
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

/// The ratio a font's cell width moves by is close to the scale ratio but not exact -- hinting and
/// pixel snapping round it -- so this asks for "moved, in the right direction, by about the right
/// amount", with bounds loose enough to survive a font change and tight enough to catch "did not
/// move" (ratio 1.0) and "moved the wrong way".
fn assert_ratio(before: f32, after: f32, expected: f32, what: &str) {
    let ratio = after / before;
    assert!(
        (ratio - expected).abs() < 0.12,
        "{what}: cell width went {before} -> {after} (ratio {ratio:.3}), expected about {expected:.3}"
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("scale_factor_setting: ignored (spawns a real nvim); run with `-- --ignored`");
        return;
    }
    // As `shell`'s own `main` and `cursor_animation` do (fix round 3, 2026-09-29): a pipe or socket on
    // stdin whose writer stays open -- an agent tool's -- reaches the embedded nvim as a buffer and
    // blocks it at startup (`neovide_editor::stdin`), which failed this test as "nvim never became ready".
    if let Ok(Some(kind)) = neovide_editor::detach_stdin_from_nvim() {
        println!("[stdin] a {kind} on stdin would reach nvim as a buffer; stdin is /dev/null now");
    }
    // Before winit or the clipboard connects to anything: its own Xvfb, the only display left to find.
    // Declared first, so a failed assertion below drops (and stops) it last, after the harness.
    let _server = own_x_server::isolate("scale_factor_setting", "640x480x24");

    let mut harness = LiveHarness::with_options(LiveHarnessOptions {
        os_scale_factor: 1.0,
        grid_size: Some(GridSize::new(40u32, 8u32)),
        extra_nvim_args: vec![
            "--clean".to_string(),
            "--cmd".to_string(),
            "let g:neovide_scale_factor = 1.5".to_string(),
        ],
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

    // 1. The startup value is APPLIED, not just reported.
    wait_for_setting(&mut harness, &mut frames, 1.5, "--cmd before ui_attach");
    let at_1_5 = cell_width(&harness);
    println!("ok: startup value 1.5 read; cell width {at_1_5}");

    // 2. A runtime :let rescales the renderer -- the control that gives 1 its meaning.
    harness.send_text_input(":let g:neovide_scale_factor = 1.0<CR>");
    wait_for_setting(&mut harness, &mut frames, 1.0, "runtime :let");
    let at_1_0 = cell_width(&harness);
    assert_ratio(at_1_5, at_1_0, 1.0 / 1.5, "1.5 -> 1.0 via :let");
    println!("ok: a runtime :let rescales the editor ({at_1_5} -> {at_1_0})");

    // Closes the loop on check 1: at the time it ran, all it had was a single reading with nothing
    // to compare it to -- `at_1_5` was captured but never itself the subject of an assertion until
    // check 2 gave us a real un-zoomed baseline (`at_1_0`) to divide it by. This is the same fact
    // check 2's own `assert_ratio` above already establishes (dividing the same two numbers the
    // other way round), stated explicitly in check 1's own direction so a reader -- or a future
    // edit -- does not have to do that inversion by hand to see that check 1's "1.5" was real.
    assert_ratio(
        at_1_0,
        at_1_5,
        1.5,
        "check 1's startup value, confirmed once check 2 gave us a baseline",
    );
    println!("ok: check 1's startup value (1.5) holds up against check 2's baseline");

    // 3. host -> nvim -> renderer.
    harness.set_scale_factor_setting(2.0);
    wait_for_setting(&mut harness, &mut frames, 2.0, "set_scale_factor_setting(2.0)");
    let at_2_0 = cell_width(&harness);
    assert_ratio(at_1_0, at_2_0, 2.0, "1.0 -> 2.0 via set_scale_factor_setting");
    println!("ok: set_scale_factor_setting assigns the variable and the editor follows ({at_1_0} -> {at_2_0})");

    // 4. Nothing moves while nothing changes -- and, since fork commit `bcd65c0` added it
    //    (`scale_factor_resyncs()`), the diagnostic counter this check is really about doesn't
    //    move either: `cell_width` staying put is a symptom, the counter staying put is the direct
    //    proof that `apply_scale_factor_setting`'s change gate, not just its absence of visible
    //    drift, is still there. Construction and the five checks above may already have bumped the
    //    counter some number of times before this loop starts (known limit: this test does not
    //    assert what that starting count is, only that it stops moving here) -- what matters is
    //    that it is IDENTICAL at the end of 30 idle frames to what it was at the start.
    let resyncs_before_idle = harness.scale_factor_resyncs();
    for _ in 0..30 {
        frames.step(&mut harness);
        assert_eq!(
            cell_width(&harness),
            at_2_0,
            "the cell width drifted with no assignment"
        );
        assert_eq!(
            harness.scale_factor_resyncs(),
            resyncs_before_idle,
            "apply_scale_factor_setting resynced the renderer with nothing assigned -- the P11-class \
             idle-cost regression the change gate exists to prevent"
        );
    }
    println!("ok: an unchanged variable leaves the editor alone (resync counter held at {resyncs_before_idle})");

    // 5. g:neovide is truthy in this embedded session -- the owner's own zoom mappings in
    //    init.lua live under `if vim.g.neovide`, so this is what makes them reachable at all.
    //    Waits on the SCALE VALUE (2.0, what set_scale_factor_setting(2.0) above just assigned),
    //    not on a cell width -- an earlier revision of this probe compared scale_factor_setting()
    //    (around 1-2) against at_2_0 (a cell width, around 22) and so its wait loop never actually
    //    waited: the two were never within f32::EPSILON of each other, so it read stale state.
    harness.send_text_input(":let g:neovide_scale_factor = (exists('g:neovide') && g:neovide) ? 1.25 : 0.75<CR>");
    wait_for_setting(&mut harness, &mut frames, 1.25, "g:neovide truthiness probe");
    println!(
        "ok: g:neovide is truthy inside the embedded session (scale_factor_setting() = {})",
        harness.scale_factor_setting()
    );

    // 6. The forced redraw actually changes pixels, not just grid_scale()'s number. Put known
    //    text ('MMM') on row 0, then add four real (but empty) buffer lines below it so the cursor
    //    lands on row 4 -- far enough that, once it is off this test's undersized-for-the-zoomed-in
    //    320x160 canvas at both scales this check uses, row 0's own ink is the only thing left to
    //    measure. This needed two corrections along the way (see the fork commit's own report and
    //    `.superpowers/zoom/L1-report.md` for both wrong readings and why): a single blank line
    //    below (`o<Esc>` once) put the cursor -- which, unlike a line's own picture, is redrawn
    //    fresh every frame at its live position and size -- flush against a freshly-large row 0
    //    with no background gap to tell the two apart; filling the grid's other 7 rows (`o<Esc>`
    //    seven times) instead made nvim scroll row 0 off the top entirely, since the single-grid
    //    embedding reserves one of those 8 rows for the command line.
    const TEXT_COLUMNS: u32 = 3;
    // Comfortably past row 0's own ink at either scale this check uses, and still well short of
    // row 4 (the cursor's row) at either scale too -- see the comment above.
    const SCAN_HEIGHT: i32 = 80;
    harness.send_text_input("iMMM<Esc>o<Esc>o<Esc>o<Esc>o<Esc>");
    for _ in 0..10 {
        frames.step(&mut harness);
    }
    let scale_before = harness.scale_factor_setting();
    let cell_width_before = harness.grid_scale().width();
    let ink_before = ink_row_height(&mut frames.surface, SCAN_HEIGHT, cell_width_before, TEXT_COLUMNS);
    println!("ok: 'MMM' on screen at scale {scale_before}; row-0 ink height {ink_before}px");

    let scale_after = 2.0;
    harness.set_scale_factor_setting(scale_after);
    wait_for_setting(&mut harness, &mut frames, scale_after, "ink-measurement scale change");
    let cell_width_after = harness.grid_scale().width();
    let ink_after = ink_row_height(&mut frames.surface, SCAN_HEIGHT, cell_width_after, TEXT_COLUMNS);
    assert_ratio(
        ink_before,
        ink_after,
        scale_after / scale_before,
        "row-0 ink height, scale change with no resize_grid",
    );
    println!(
        "ok: the forced redraw re-records the glyph picture at the new cell size (ink height {ink_before} -> {ink_after})"
    );

    assert!(harness.shutdown(), "nvim did not exit cleanly");
    println!("scale_factor_setting: all passed");
}
