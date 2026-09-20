//! `g:neovide_fullscreen` reaches the host, and the host's write reaches nvim -- against a real nvim.
//!
//! `shell` follows this variable with the real window and writes it back when the window changes
//! for a reason nvim did not see (spec: docs/superpowers/specs/2026-09-19-window-modes-design.md
//! §2.4). Three paths, one launch:
//!
//! 1. a value set before `ui_attach` (`--cmd`, read by `settings.read_initial_values`) is what the
//!    getter starts at;
//! 2. a runtime `:let` reaches the getter through Neovide's variable watcher -- the path `init.lua`
//!    and a mapping take;
//! 3. `set_fullscreen_setting` really assigns the variable in nvim: nothing but that watcher moves
//!    the getter, so the getter following the write is the proof the write landed.
//!
//! Ignored unless asked for: it spawns a real `nvim` (>= 0.10 on `PATH`) and connects to the
//! session's display for the clipboard (no window). A plain `main` (`harness = false`) because
//! winit allows one `EventLoop` per process, on the main thread only. Run with:
//!
//!     cargo test -p neovide-editor --test fullscreen_setting -- --ignored
use std::time::{Duration, Instant};

use neovide::{
    live_harness::{LiveHarness, LiveHarnessOptions},
    units::{GridSize, PixelRect},
};
use skia_safe::{Surface, surfaces};

/// Readiness is decided by the renderer as it handles draw commands, so the harness is driven the
/// way a host drives it: one frame at a time, into a CPU raster surface (no GPU, no window).
struct Frames {
    surface: Surface,
    region: PixelRect<f32>,
}

impl Frames {
    fn new() -> Self {
        let surface = surfaces::raster_n32_premul((320, 160)).expect("raster surface");
        Self { surface, region: PixelRect::from_min_max((0.0, 0.0), (320.0, 160.0)) }
    }

    fn step(&mut self, harness: &mut LiveHarness) {
        harness.render_frame(self.surface.canvas(), Some(&self.region), 1.0 / 60.0);
        std::thread::sleep(Duration::from_millis(16));
    }
}

/// Pumps until `fullscreen_setting()` reads `want`, or panics naming `what` after five seconds.
fn wait_for(harness: &mut LiveHarness, frames: &mut Frames, want: bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while harness.fullscreen_setting() != want {
        assert!(Instant::now() < deadline, "{what}: fullscreen_setting() never became {want}");
        frames.step(harness);
    }
}

/// Pumps for `for_how_long` and panics if the getter moves off `want` meanwhile: the control that
/// makes a later change the doing of the step that follows, not drift.
fn holds(harness: &mut LiveHarness, frames: &mut Frames, want: bool, for_how_long: Duration) {
    let until = Instant::now() + for_how_long;
    while Instant::now() < until {
        frames.step(harness);
        assert_eq!(harness.fullscreen_setting(), want, "the getter moved on its own");
    }
}

fn main() {
    // Mirrors `#[ignore]` under a plain `main`: `cargo test --workspace` needs no nvim.
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("fullscreen_setting: ignored (spawns a real nvim); run with `-- --ignored`");
        return;
    }

    let mut harness = LiveHarness::with_options(LiveHarnessOptions {
        os_scale_factor: 1.0,
        grid_size: Some(GridSize::new(40u32, 8u32)),
        extra_nvim_args: vec![
            "--clean".to_string(),
            "--cmd".to_string(),
            "let g:neovide_fullscreen = v:true".to_string(),
        ],
        ..Default::default()
    })
    .expect("LiveHarness::with_options failed -- is `nvim` (>= 0.10) on $PATH?");

    let mut frames = Frames::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !harness.is_ready() {
        assert!(Instant::now() < deadline && !harness.has_neovim_exited(), "nvim never became ready");
        frames.step(&mut harness);
    }

    // 1. The startup value.
    wait_for(&mut harness, &mut frames, true, "--cmd before ui_attach");
    holds(&mut harness, &mut frames, true, Duration::from_millis(300));
    println!("ok: a value set before ui_attach is the getter's starting value");

    // 2. nvim -> host.
    harness.send_text_input(":let g:neovide_fullscreen = v:false<CR>");
    wait_for(&mut harness, &mut frames, false, "runtime :let");
    holds(&mut harness, &mut frames, false, Duration::from_millis(300));
    println!("ok: a runtime :let reaches the getter");

    // 3. host -> nvim.
    harness.set_fullscreen_setting(true);
    wait_for(&mut harness, &mut frames, true, "set_fullscreen_setting(true)");
    harness.set_fullscreen_setting(false);
    wait_for(&mut harness, &mut frames, false, "set_fullscreen_setting(false)");
    println!("ok: set_fullscreen_setting assigns the variable in nvim, both ways");

    assert!(harness.shutdown(), "nvim did not exit cleanly");
    println!("fullscreen_setting: all passed");
}
