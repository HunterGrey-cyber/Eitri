//! Checks that nvim versions down to `neovide_editor::NVIM_FLOOR` start, paint and take keys.
//!
//! The fork normally requires nvim 0.12. The pane passes `neovide_editor::NVIM_FLOOR` to
//! `LiveHarnessOptions::minimum_nvim_version` to allow supported older versions. Without
//! that override, versions below 0.12 are refused; at 0.12 or later both configurations start.
//! On 0.12, the harness follows the server started by `:restart` and returns to its own
//! nvim after `:connect` to an address with no listener. The editor keeps taking keys in
//! both cases, and quitting its nvim leaves no processes from the run.
//! Run once for each supported nvim version, with that nvim first on `PATH`.
//!
//! The scenario runs in a child copy of this binary on its own `Xvfb`: winit and the
//! clipboard need a display even without a window. The parent connects to no display.
//! Ignored unless requested because it spawns real nvims. Run with:
//!
//!     cargo test -p neovide-editor --test nvim_floor -- --ignored
use std::time::{Duration, Instant};

use neovide::{
    live_harness::{Followed, LiveHarness, LiveHarnessOptions},
    units::{GridSize, PixelRect},
};
use skia_safe::surfaces;

#[path = "../../shell/tests/support/own_x_server.rs"]
mod own_x_server;

const CANVAS: (i32, i32) = (640, 320);
const CASE_ENV: &str = "EITRI_NVIM_FLOOR_CASE";

/// `(major, minor, patch)` of the `nvim` on `PATH`, from `nvim --version`'s first line.
fn path_nvim_version() -> (u64, u64, u64) {
    let out = std::process::Command::new("nvim")
        .arg("--version")
        .output()
        .expect("nvim must be on PATH");
    let first = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    let numbers = first
        .strip_prefix("NVIM v")
        .unwrap_or_else(|| panic!("unexpected nvim --version line: {first:?}"));
    let mut parts = numbers
        .split(|c: char| !c.is_ascii_digit())
        .take(3)
        .map(|p| p.parse::<u64>().expect("a version number"));
    (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap())
}

/// A `--cmd` this run's nvims carry in their argv, so a server `:restart` started from one (it copies
/// `v:argv`) can be found among the machine's processes.
fn marker() -> String {
    format!(
        "let g:eitri_nvim_floor_{}_{} = 1",
        std::process::id(),
        std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()
    )
}

/// Pids of live processes whose command line holds `needle`.
fn processes_with(needle: &str) -> Vec<u32> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) {
            if String::from_utf8_lossy(&cmdline).contains(needle) {
                found.push(pid);
            }
        }
    }
    found
}

/// Ends, on scope exit (a failed assertion included), every process that carries this run's marker, so the run leaves
/// nothing behind whatever it found.
struct Reaper(String);

impl Drop for Reaper {
    fn drop(&mut self) {
        for pid in processes_with(&self.0) {
            // Only processes carrying this run's own marker.
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
    }
}

/// The processes carrying `marker` once they have had `wait` to exit on their own: polled, so one that ends late is not
/// counted as left running.
fn left_running(marker: &str, wait: Duration) -> Vec<u32> {
    let deadline = Instant::now() + wait;
    loop {
        let left = processes_with(marker);
        if left.is_empty() || Instant::now() >= deadline {
            return left;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn options(floor: Option<(u64, u64, u64)>, marker: &str) -> LiveHarnessOptions {
    LiveHarnessOptions {
        grid_size: Some(GridSize::new(40u32, 8u32)),
        extra_nvim_args: vec!["--clean".to_string(), "--cmd".to_string(), marker.to_string()],
        startup_message_capture: false,
        minimum_nvim_version: floor,
        ..Default::default()
    }
}

/// Pumps and paints until `done` holds, failing after 15 s. The pump is what follows a `:restart` or `:connect`.
fn wait(harness: &mut LiveHarness, what: &str, mut done: impl FnMut(&LiveHarness) -> bool) {
    let mut surface = surfaces::raster_n32_premul(CANVAS).expect("raster surface");
    let region = PixelRect::from_min_max((0.0, 0.0), (CANVAS.0 as f32, CANVAS.1 as f32));
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        harness.pump(Duration::ZERO);
        harness.render_frame(surface.canvas(), Some(&region), 1.0 / 60.0);
        if done(harness) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn cursor_col(harness: &LiveHarness) -> i32 {
    (harness.cursor_pixel_position().x / harness.grid_scale().width()).round() as i32
}

fn scenario() {
    let version = path_nvim_version();
    println!(
        "nvim on PATH: {version:?}; the editor's floor: {:?}",
        neovide_editor::NVIM_FLOOR
    );
    assert!(
        version >= neovide_editor::NVIM_FLOOR,
        "the matrix's nvim is below the floor"
    );

    let marker = marker();
    let _reaper = Reaper(marker.clone());
    let mut harness = LiveHarness::with_options(options(Some(neovide_editor::NVIM_FLOOR), &marker))
        .unwrap_or_else(|e| panic!("with the floor, {version:?} must start: {e:#}"));
    wait(&mut harness, "ready", |h| h.is_ready());
    harness.send_text_input("ifloor<Esc>");
    wait(&mut harness, "the cursor at col 4", |h| cursor_col(h) == 4);
    assert!(harness.shutdown(), "the shutdown did not observe nvim's exit");
    drop(harness);
    println!("with the floor: started, typed, ended");

    match LiveHarness::with_options(options(None, &marker)) {
        Ok(mut harness) => {
            assert!(version >= (0, 12, 0), "{version:?} started without the floor");
            wait(&mut harness, "ready without the floor", |h| h.is_ready());
            println!("without the floor: started ({version:?} is 0.12 or later)");
            // An 0.12 nvim's `:restart` asks its UI to attach to the new server, and the harness follows it there:
            // the session goes on, on the new server.
            harness.send_text_input(":restart<CR>");
            let mut followed = None;
            wait(&mut harness, "the harness followed :restart", |h| {
                followed = followed.take().or_else(|| h.take_followed());
                followed.is_some()
            });
            assert_eq!(followed, Some(Followed::Restart));
            assert!(!harness.has_neovim_exited(), "`:restart` ended the session");
            harness.send_text_input("ifollowed<Esc>");
            wait(&mut harness, "the followed server takes keys", |h| cursor_col(h) == 7);
            // Its quit ends the session, and with it the server: nothing of this run is left.
            harness.send_text_input(":set nomodified | qa<CR>");
            wait(&mut harness, "the followed server's exit", |h| h.has_neovim_exited());
            assert!(harness.shutdown(), "the shutdown did not observe nvim's exit");
            drop(harness);
            let left = left_running(&marker, Duration::from_secs(5));
            println!("`:restart` followed; nvims still running 5 s after its quit: {left:?}");
            assert!(left.is_empty(), "the followed `:restart` left {left:?} running");

            // `:connect` to an address nothing listens on: the editor goes back to its own nvim.
            let marker = self::marker();
            let _reaper = Reaper(marker.clone());
            let mut harness = LiveHarness::with_options(options(None, &marker)).expect("a second editor");
            wait(&mut harness, "ready for :connect", |h| h.is_ready());
            let nowhere = format!("/nonexistent/eitri-nvim-floor-{}.sock", std::process::id());
            harness.send_text_input(&format!(":connect {nowhere}<CR>"));
            let until = Instant::now() + Duration::from_secs(2);
            wait(&mut harness, "two seconds after :connect", |_| Instant::now() >= until);
            assert!(!harness.has_neovim_exited(), "`:connect` to nowhere ended the session");
            assert_eq!(
                harness.take_followed(),
                None,
                "a connect that went nowhere was followed"
            );
            harness.send_text_input("ikept<Esc>");
            wait(&mut harness, "the editor's own nvim takes keys", |h| cursor_col(h) == 3);
            harness.send_text_input(":set nomodified | qa<CR>");
            wait(&mut harness, "the editor's nvim exit", |h| h.has_neovim_exited());
            harness.shutdown();
            drop(harness);
            let left = left_running(&marker, Duration::from_secs(5));
            println!("`:connect` to nowhere kept the session; nvims still running 5 s after its quit: {left:?}");
            assert!(left.is_empty(), "`:connect` to nowhere left {left:?} running");
        }
        Err(e) => {
            let message = format!("{e:#}");
            assert!(
                version < (0, 12, 0),
                "{version:?} was refused without the floor: {message}"
            );
            assert!(
                message.contains("requires nvim version 0.12.0"),
                "another refusal: {message}"
            );
            println!("without the floor: refused, as Neovide refuses {version:?}");
        }
    }
}

fn main() {
    if std::env::var_os(CASE_ENV).is_some() {
        if let Ok(Some(kind)) = neovide_editor::detach_stdin_from_nvim() {
            println!("[stdin] a {kind} on stdin would reach nvim as a buffer; stdin is /dev/null now");
        }
        let _server = own_x_server::isolate("nvim_floor", "640x480x24");
        scenario();
        println!("RESULT pass");
        return;
    }
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("nvim_floor: ignored (spawns real nvims); run with `-- --ignored`");
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .env(CASE_ENV, "1")
        .output()
        .expect("re-exec of the test binary failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    println!("{stdout}");
    if !stdout.lines().any(|l| l == "RESULT pass") {
        panic!(
            "nvim_floor failed ({})\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
