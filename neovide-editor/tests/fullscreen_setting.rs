//! Checks `g:neovide_fullscreen` propagation between the host and a real nvim.
//!
//! `shell` follows the variable with the window and writes it back when the window changes
//! for a reason nvim did not see. One launch checks three paths:
//!
//! 1. A value set before `ui_attach` with `--cmd` is read by `settings.read_initial_values`
//!    and becomes the getter's initial value.
//! 2. A runtime `:let` reaches the getter through the variable watcher, as `init.lua` and
//!    mappings do.
//! 3. `set_fullscreen_setting` assigns the variable in nvim. Only the watcher moves the
//!    getter, so observing the new value confirms that the write arrived.
//!
//! Ignored unless requested: it spawns `nvim` (>= 0.10 on `PATH`) and starts its own `Xvfb`
//! for the clipboard without creating a window. `shell/tests/support/own_x_server.rs`
//! starts the display before winit connects and removes inherited `WAYLAND_DISPLAY` and
//! `DISPLAY`. A plain `main` (`harness = false`) keeps the single winit `EventLoop`
//! on the main thread. Run without a display wrapper:
//!
//!     cargo test -p neovide-editor --test fullscreen_setting -- --ignored
use std::time::{Duration, Instant};

use neovide::{
    live_harness::{LiveHarness, LiveHarnessOptions},
    units::{GridSize, PixelRect},
};
use skia_safe::{surfaces, Surface};

// The display this test runs on: its own Xvfb, never an inherited one (2026-09-29). The one helper
// every display-connecting test in the workspace shares, so it lives with `shell`'s.
#[path = "../../shell/tests/support/own_x_server.rs"]
mod own_x_server;

/// Readiness is decided by the renderer as it handles draw commands, so the harness is driven the
/// way a host drives it: one frame at a time, into a CPU raster surface (no GPU, no window).
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

/// Pumps until `fullscreen_setting()` reads `want`, or panics naming `what` after five seconds.
fn wait_for(harness: &mut LiveHarness, frames: &mut Frames, want: bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while harness.fullscreen_setting() != want {
        assert!(
            Instant::now() < deadline,
            "{what}: fullscreen_setting() never became {want}"
        );
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
    // As `shell`'s own `main` and `cursor_animation` do: a pipe or socket on
    // stdin whose writer stays open -- an agent tool's -- reaches the embedded nvim as a buffer and
    // blocks it at startup (`neovide_editor::stdin`), which failed this test as "nvim never became ready".
    if let Ok(Some(kind)) = neovide_editor::detach_stdin_from_nvim() {
        println!("[stdin] a {kind} on stdin would reach nvim as a buffer; stdin is /dev/null now");
    }
    // Before winit or the clipboard connects to anything: its own Xvfb, the only display left to find.
    // Declared first, so a failed assertion below drops (and stops) it last, after the harness.
    let _server = own_x_server::isolate("fullscreen_setting", "640x480x24");

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
        assert!(
            Instant::now() < deadline && !harness.has_neovim_exited(),
            "nvim never became ready"
        );
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
