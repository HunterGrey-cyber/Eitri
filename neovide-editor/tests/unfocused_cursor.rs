//! The editor shows no cursor while it does not have the keys -- proved by rendering real frames.
//!
//! Each case spawns a real `nvim --embed --clean` through the fork's `LiveHarness`, paints into a
//! CPU raster Skia surface (no GPU, no window, no GTK), and counts the pixels in the cursor's cell
//! that differ from nvim's own `Normal` background. The cursor sits on an empty buffer's first
//! cell, so with no cursor drawn every pixel of that cell is exactly the background.
//!
//! Ignored unless asked for, because it spawns a real `nvim` (>= 0.10 on `PATH`) and connects to
//! the session's Wayland/X11 display for the clipboard (no window is created). It is a plain `main`
//! (`harness = false`) that re-executes itself once per case, because winit allows one `EventLoop`
//! per process, on the main thread only. Run with:
//!
//!     cargo test -p neovide-editor --test unfocused_cursor -- --ignored
use std::time::{Duration, Instant};

use neovide::{
    live_harness::{LiveHarness, LiveHarnessOptions},
    units::{GridSize, PixelRect},
};
use skia_safe::{Color, Surface, surfaces};

const CANVAS: (i32, i32) = (640, 320);
const FRAME_DT: f32 = 1.0 / 60.0;

fn render(harness: &mut LiveHarness, surface: &mut Surface, region: &PixelRect<f32>) {
    harness.render_frame(surface.canvas(), Some(region), FRAME_DT);
}

/// Renders enough frames (at a simulated 60Hz) for the cursor's spring animation and any
/// focus-change repaint to settle.
fn settle(harness: &mut LiveHarness, surface: &mut Surface, region: &PixelRect<f32>) {
    for _ in 0..90 {
        render(harness, surface, region);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn pixel(surface: &mut Surface, x: i32, y: i32) -> Color {
    let pixmap = surface.peek_pixels().expect("raster surface must expose its pixels");
    pixmap.get_color((x, y))
}

/// Pixels in the cursor cell (grown by 2px each side, to catch anti-aliased bleed) that are not
/// exactly the `Normal` background, sampled from an empty cell far to the right on the same row.
fn cursor_pixels(harness: &LiveHarness, surface: &mut Surface) -> usize {
    let pos = harness.cursor_pixel_position();
    let scale = harness.grid_scale();
    let (cw, ch) = (scale.width(), scale.height());
    // Column 10 of the cursor's row: empty in a fresh buffer, and inside the canvas at every scale.
    let background = pixel(surface, (pos.x + cw * 10.5) as i32, (pos.y + ch * 0.5) as i32);
    let (x0, y0) = ((pos.x.floor() as i32 - 2).max(0), (pos.y.floor() as i32 - 2).max(0));
    let (x1, y1) = ((pos.x + cw).ceil() as i32 + 2, (pos.y + ch).ceil() as i32 + 2);
    let mut count = 0;
    for y in y0..y1 {
        for x in x0..x1 {
            if pixel(surface, x, y) != background {
                count += 1;
            }
        }
    }
    count
}

/// `(focused, unfocused)` cursor-pixel counts for one real nvim launched with `extra` after
/// `--clean`, at `scale`. `keys`, if not empty, is typed into nvim once it is ready (before `i`).
fn measure(scale: f64, insert: bool, keys: &str, extra: &[String]) -> (usize, usize) {
    let mut surface = surfaces::raster_n32_premul(CANVAS).expect("raster surface");
    let region = PixelRect::from_min_max((0.0, 0.0), (CANVAS.0 as f32, CANVAS.1 as f32));
    let mut args = vec!["--clean".to_string()];
    args.extend(extra.iter().cloned());
    let mut harness = LiveHarness::with_options(LiveHarnessOptions {
        os_scale_factor: scale,
        grid_size: Some(GridSize::new(40u32, 8u32)),
        extra_nvim_args: args,
        ..Default::default()
    })
    .expect("LiveHarness::with_options failed -- is `nvim` (>= 0.10) on $PATH?");

    let deadline = Instant::now() + Duration::from_secs(15);
    while !harness.is_ready() {
        assert!(Instant::now() < deadline && !harness.has_neovim_exited(), "nvim never became ready");
        render(&mut harness, &mut surface, &region);
        std::thread::sleep(Duration::from_millis(16));
    }
    if !keys.is_empty() {
        harness.send_text_input(keys);
    }
    if insert {
        harness.send_text_input("i");
    }
    settle(&mut harness, &mut surface, &region);
    let focused = cursor_pixels(&harness, &mut surface);

    harness.set_focused(false);
    settle(&mut harness, &mut surface, &region);
    let unfocused = cursor_pixels(&harness, &mut surface);

    assert!(harness.shutdown(), "nvim did not exit cleanly");
    (focused, unfocused)
}

fn width_cmd(width: &str) -> Vec<String> {
    vec!["--cmd".to_string(), format!("let g:neovide_cursor_unfocused_outline_width = {width}")]
}

/// What neovibe's editor pane passes: its own default, exactly as `NeovideEditorPane` builds it.
fn pane_default() -> Vec<String> {
    vec!["--cmd".to_string(), neovide_editor::HIDE_UNFOCUSED_CURSOR_CMD.to_string()]
}

const CASE_ENV: &str = "NEOVIBE_UNFOCUSED_CURSOR_CASE";

/// One launch. `insert` puts nvim in insert mode first, so the cursor is a vertical bar.
struct Case {
    name: &'static str,
    scale: f64,
    insert: bool,
    /// Keys typed once nvim is ready, AFTER `ui_attach` -- the only way this test reaches Neovide's
    /// `WatchGlobal`/`setting_changed` path, which is how a real `init.lua` (loaded at `ui_attach`)
    /// or a runtime `:let` gets a value to the renderer. A `--cmd` does not: it is read by
    /// `settings.read_initial_values`, before `ui_attach`.
    keys: &'static str,
    extra: Vec<String>,
    /// Whether the unfocused frame must have cursor pixels (`true`) or none (`false`).
    unfocused_visible: bool,
}

/// Runs one case in a child copy of this binary: winit allows one `EventLoop` per process, and
/// only on the main thread, so every `LiveHarness` needs a fresh process.
fn run_case(case: &Case) -> (usize, usize) {
    let spec = format!("{}\n{}\n{}\n{}", case.scale, case.insert, case.keys, case.extra.join("\n"));
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .env(CASE_ENV, spec)
        .output()
        .expect("re-exec of the test binary failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().find_map(|l| l.strip_prefix("RESULT ")).unwrap_or_else(|| {
        panic!("case produced no result: {}\n{}", stdout, String::from_utf8_lossy(&out.stderr))
    });
    let mut it = line.split(' ').map(|n| n.parse().unwrap());
    (it.next().unwrap(), it.next().unwrap())
}

fn cases() -> Vec<Case> {
    let mut v = Vec::new();
    // Scale 1.5 is the one that matters: a fractional cell size is where stock Neovide's width-0
    // outline leaves an anti-aliased sliver (33 pixels, measured before the fork fix).
    for scale in [1.0, 1.25, 1.5, 2.0] {
        v.push(Case {
            name: "pane default, block",
            scale,
            insert: false,
            keys: "",
            extra: pane_default(),
            unfocused_visible: false,
        });
    }
    v.push(Case {
        name: "pane default, insert-mode bar",
        scale: 1.5,
        insert: true,
        keys: "",
        extra: pane_default(),
        unfocused_visible: false,
    });
    // The user's own config runs after every `--cmd`; a later `--cmd` stands in for it here, since
    // `--clean` is what keeps the real one out.
    let mut user_override = pane_default();
    user_override.extend(width_cmd("0.125"));
    v.push(Case {
        name: "pane default + user override 0.125 (--cmd, startup read)",
        scale: 1.5,
        insert: false,
        keys: "",
        extra: user_override,
        unfocused_visible: true,
    });
    // The same override delivered AFTER `ui_attach`, through the variable watcher -- the path a
    // real `init.lua` and a runtime `:let` both take (the `--cmd` case above never reaches it).
    v.push(Case {
        name: "pane default + runtime :let 0.125 (watcher)",
        scale: 1.5,
        insert: false,
        keys: ":let g:neovide_cursor_unfocused_outline_width = 0.125<CR>",
        extra: pane_default(),
        unfocused_visible: true,
    });
    // Positive control: with nothing injected, stock Neovide's hollow block is really there, so a
    // zero above is the default's doing and not a blind probe.
    v.push(Case {
        name: "no default (stock Neovide)",
        scale: 1.5,
        insert: false,
        keys: "",
        extra: Vec::new(),
        unfocused_visible: true,
    });
    v
}

fn main() {
    if let Ok(spec) = std::env::var(CASE_ENV) {
        let mut lines = spec.split('\n');
        let scale: f64 = lines.next().unwrap().parse().unwrap();
        let insert: bool = lines.next().unwrap().parse().unwrap();
        let keys = lines.next().unwrap().to_string();
        let extra: Vec<String> = lines.filter(|l| !l.is_empty()).map(String::from).collect();
        let (f, u) = measure(scale, insert, &keys, &extra);
        println!("RESULT {f} {u}");
        return;
    }
    // Mirrors `#[ignore]` under a plain `main`: `cargo test --workspace` needs no nvim.
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("unfocused_cursor: ignored (spawns a real nvim); run with `-- --ignored`");
        return;
    }
    let mut failures = Vec::new();
    for case in cases() {
        let (focused, unfocused) = run_case(&case);
        let ok = focused > 0 && (unfocused > 0) == case.unfocused_visible;
        println!(
            "{} {} scale={}: focused={focused} unfocused={unfocused} (want {})",
            if ok { "ok  " } else { "FAIL" },
            case.name,
            case.scale,
            if case.unfocused_visible { "> 0" } else { "0" },
        );
        if !ok {
            failures.push(case.name);
        }
    }
    assert!(failures.is_empty(), "failed: {failures:?}");
}
