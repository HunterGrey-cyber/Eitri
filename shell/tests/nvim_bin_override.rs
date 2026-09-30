//! The pinned fork honours `NEOVIM_BIN` (v1-dist plan Task 6, spec §7): with the variable set in
//! this process before a `LiveHarness` is built, the pinned fork's `CmdLineSettings` (clap's own
//! `#[arg(long = "neovim-bin", env = "NEOVIM_BIN")]`, `cmd_line.rs:229` at the pinned rev `910053d`)
//! picks it up and spawns exactly that binary -- never plain `nvim` -- which is the one thing
//! `shell::main`'s own `unsafe { std::env::set_var(..) }` (documented at that call site,
//! `eitri_core::nvim_bin`'s module doc) depends on and nothing in this workspace's own tests
//! otherwise exercises: `eitri_core::nvim_bin`'s tests are pure, over an injected `version_of`,
//! and never touch the fork at all.
//!
//! Proof, not inference: a wrapper script stands in for `nvim`, appends its own argv to a marker
//! file, and `exec`s the real `nvim` so the harness still gets a working session. The marker being
//! written at all is the proof the fork actually ran *this* binary rather than resolving `"nvim"` on
//! `PATH` itself.
//!
//! `#[ignore]`d (mirrored here by hand, `harness = false` means cargo's own skip does not apply --
//! see the check in `main` below): it needs `nvim` (>= 0.10) on `PATH`, and -- like every
//! `LiveHarness`-driving test in this workspace, `neovide-editor`'s own ignored ones included -- a
//! display connection, even though it never opens a window (winit's `EventLoop` needs one for the
//! clipboard). **It starts its own** (2026-09-29, `support/own_x_server.rs`): an `Xvfb` pointed to
//! before winit connects, with any inherited `WAYLAND_DISPLAY`/`DISPLAY` dropped. Until then it
//! connected to whatever display it inherited, and this header's "never the real desktop" was only a
//! sentence -- from an agent shell, the desktop's. So, with no wrapper:
//!
//!     cargo test -p shell --test nvim_bin_override -- --ignored
//!
//! A plain `main` (`harness = false`), copied from `neovide-editor/tests/fullscreen_setting.rs`'s
//! own pump/readiness plumbing almost verbatim (that file's own header explains why: winit allows
//! one `EventLoop` per process, on the main thread only).

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use neovide::live_harness::{LiveHarness, LiveHarnessOptions};
use neovide::units::{GridSize, PixelRect};
use skia_safe::surfaces;

// The display this test runs on: its own Xvfb, never an inherited one (2026-09-29).
#[path = "support/own_x_server.rs"]
mod own_x_server;

fn main() {
    // Mirrors `#[ignore]` under a plain `main`: `cargo test --workspace` needs no nvim and writes
    // nothing to disk.
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("nvim_bin_override: ignored (spawns a real nvim, needs a display); run with `-- --ignored`");
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
    let _server = own_x_server::isolate("nvim_bin_override", "640x480x24");

    // `$TMPDIR` (`/tmp` when unset), like every other test in this workspace that needs a throwaway
    // directory -- this project's own dev-scratch convention (`~/.cache/nv-v1dist-*`) is for a human
    // running commands, not for a test's own temporary files, and nothing here is a build artifact.
    let scratch = std::env::temp_dir().join(format!("eitri-nvim-bin-override-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("creating the scratch dir must succeed");
    let marker = scratch.join("wrapper-argv.txt");
    let wrapper = scratch.join("nvim-wrapper.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\nexec nvim \"$@\"\n",
            shell_single_quote(&marker)
        ),
    )
    .expect("writing the wrapper script must succeed");
    let mut perms = std::fs::metadata(&wrapper)
        .expect("the wrapper must exist")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&wrapper, perms).expect("making the wrapper executable must succeed");

    // SAFETY: this is a fresh, single-threaded `harness = false` `main` -- nothing else in this
    // process has touched the environment yet, exactly the condition `shell::main`'s own real
    // `set_var` call (which this test stands in for) is documented to require.
    unsafe { std::env::set_var("NEOVIM_BIN", &wrapper) };

    let mut harness = LiveHarness::with_options(LiveHarnessOptions {
        os_scale_factor: 1.0,
        grid_size: Some(GridSize::new(40u32, 8u32)),
        extra_nvim_args: vec!["--clean".to_string()],
        ..Default::default()
    })
    .expect("LiveHarness::with_options failed -- is `nvim` (>= 0.10) on $PATH?");

    let mut surface = surfaces::raster_n32_premul((320, 160)).expect("raster surface");
    let region = PixelRect::from_min_max((0.0, 0.0), (320.0, 160.0));
    let deadline = Instant::now() + Duration::from_secs(15);
    while !harness.is_ready() {
        assert!(
            Instant::now() < deadline && !harness.has_neovim_exited(),
            "nvim never became ready"
        );
        harness.render_frame(surface.canvas(), Some(&region), 1.0 / 60.0);
        std::thread::sleep(Duration::from_millis(16));
    }

    assert!(harness.shutdown(), "nvim did not exit cleanly");

    let written = std::fs::read_to_string(&marker)
        .unwrap_or_else(|e| panic!("the wrapper never wrote {}: {e}", marker.display()));
    assert!(!written.trim().is_empty(), "the wrapper ran but recorded no argv");
    println!(
        "nvim_bin_override: the fork spawned {} ({} bytes of argv recorded) -- NEOVIM_BIN honoured",
        wrapper.display(),
        written.len()
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

/// Single-quotes `path` for the wrapper's own `sh` line -- a scratch directory under `$TMPDIR` is
/// never expected to contain a `'`, but the marker's path is still not hand-escaped inline.
fn shell_single_quote(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}
