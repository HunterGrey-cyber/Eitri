//! A held `j`/`k` on a plugin page, against the real WebKitGTK page: what every animation frame shows while
//! the key is held, measured, for the keyboard kit every plugin page loads (`plugin-kit/keys`).
//!
//! The page is a long plain document (`jk_frames::plain_page`) with two scripts added at document end: the
//! kit's bundle, and a stand-in for the host bridge that accepts the kit's key table as declared and hands
//! it the settings' defaults. The key is held as a person holds it: one XTEST key-down, the X server's own
//! auto-repeat at a set rate (`xset r rate`), one key-up. The report is the frame times (missed frames)
//! and the view's movement per frame (stalls, reversals, unevenness) over the hold, for `j` from the top
//! and `k` from the bottom.
//!
//! A plain `main` (`harness = false`), because GTK must own the main thread; opt-in with `-- --ignored`:
//!
//!     cargo test -p shell --test plugin_page_jk_hold -- --ignored
//!
//! It starts its own X server (`support/own_x_server.rs`) and talks to no other display. Needs `Xvfb`,
//! `xdotool` and `xset`. One GUI pass at a time per slot on the machine: take a slot first.
//!
//! - `JKP_KIT=<path>` measures another build of the kit (an older `dist/keys.js`) and checks nothing: the
//!   numbers are its report.
//! - `JKP_OUT=<dir>` writes each run's frame log there as JSON.
//! - `JKP_REPEAT=<per second>` sets the auto-repeat rate (30 when unset), after a 250ms delay.
//!
//! It prints one `JKP` line per run. Without `JKP_KIT` it fails unless, for both keys, the view moved
//! during the steady hold without reaching the end it was going to, no frame stood still or moved against
//! the key, and the per-frame move's spread (cv) is at most 0.2. A `JKP_OUT` that cannot be written fails
//! the run in either mode.
use std::cell::RefCell;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::glib;
use gtk4::prelude::*;
use serde_json::Value;
use webkit6::prelude::*;
use webkit6::{UserContentInjectedFrames, UserContentManager, UserScript, UserScriptInjectionTime, WebView};

// The display this test runs on: its own Xvfb, never an inherited one.
#[path = "support/own_x_server.rs"]
mod own_x_server;

// The frame log and its numbers, shared with the agent panel's held-key measurement.
#[path = "support/jk_frames.rs"]
mod jk_frames;

use jk_frames::{analyse, plain_page, report, start_log_js, Metrics, STOP_LOG_JS};

/// The kit plugin pages load, as `shell/build.rs` built it for this build.
const KIT_JS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../plugin-kit/keys/dist/keys.js"));

/// The host's side of the bridge, reduced to what the kit needs to install: the key table comes back as
/// declared, events never arrive. Then the kit is installed with no page rows, as a page that only reads
/// would, and the defaults a host hands over when the user set nothing.
const HOST_STUB_JS: &str = r#"
window.eitriPlugin = {
  call: async (verb, args) => verb === "keys.declare" ? { rows: args.rows } : {},
  on: () => () => {},
};
window.eitriKeys.install({
  rows: [],
  run: () => false,
  initial: { timeoutlen: 1000, hasKeys: true, prefix: "Ctrl+b", settings: {} },
}).then(() => { window.__jkpInstalled = true; }, (e) => { window.__jkpError = String(e); });
"#;

/// The kit's own per-frame record of its glide, kept beside the frame log for a reader of the JSON.
const GLIDE_TRACE_JS: &str = r#"JSON.stringify((() => {
  window.__eitriKeysScrollTrace = (f) => window.__jkh.trace.push([f.t, f.top, f.speed, f.phase]);
  return true;
})())"#;

/// How long each key is held, and how long the page is watched after the key-up.
const HOLD_MS: u64 = 1500;
const AFTER_MS: u64 = 600;

/// Runs an X client against `display` -- always passed explicitly, never inherited.
fn x_tool(tool: &str, display: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(tool)
        .args(args)
        .env("DISPLAY", display)
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .map_err(|e| format!("{tool} not runnable ({e})"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "{tool} {args:?} exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn pump(ms: u64) {
    let context = glib::MainContext::default();
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        while context.iteration(false) {}
        std::thread::sleep(Duration::from_millis(1));
    }
}

struct Harness {
    window: gtk4::Window,
    webview: WebView,
    display: String,
    xid: String,
}

impl Harness {
    /// A window holding the plain page with the kit and the host stand-in added at document end.
    fn new(title: &str, display: &str, kit: &str) -> Result<Self, String> {
        let window = gtk4::Window::new();
        window.set_default_size(1000, 800);
        window.set_title(Some(title));
        let content_manager = UserContentManager::new();
        for source in [kit, HOST_STUB_JS] {
            content_manager.add_script(&UserScript::new(
                source,
                UserContentInjectedFrames::TopFrame,
                UserScriptInjectionTime::End,
                &[],
                &[],
            ));
        }
        let webview = WebView::builder().user_content_manager(&content_manager).build();
        webview.set_hexpand(true);
        webview.set_vexpand(true);
        window.set_child(Some(&webview));
        webview.load_html(&plain_page(), Some("https://plugin-page.invalid/"));
        window.present();
        webview.grab_focus();
        pump(400);
        // The title has no regex metacharacters, so it is its own pattern.
        let pattern = format!("^{title}$");
        let mut xid = None;
        for _ in 0..50 {
            if let Ok(out) = x_tool("xdotool", display, &["search", "--onlyvisible", "--name", &pattern]) {
                if let Some(id) = out.lines().map(str::trim).find(|l| !l.is_empty()) {
                    xid = Some(id.to_string());
                    break;
                }
            }
            pump(100);
        }
        let xid = xid.ok_or_else(|| format!("no mapped X window titled {title:?} on {display} after 5 s"))?;
        let harness = Harness {
            window,
            webview,
            display: display.to_string(),
            xid,
        };
        harness.focus()?;
        Ok(harness)
    }

    fn focus(&self) -> Result<(), String> {
        for _ in 0..40 {
            let _ = x_tool("xdotool", &self.display, &["windowfocus", &self.xid]);
            pump(20);
            if x_tool("xdotool", &self.display, &["getwindowfocus"])
                .map(|s| s.trim() == self.xid)
                .unwrap_or(false)
            {
                return Ok(());
            }
            pump(30);
        }
        Err(format!("X focus never reached window {} on {}", self.xid, self.display))
    }

    /// Waits for the kit's install to finish; its failure is reported as the page gave it.
    fn wait_installed(&self) -> Result<(), String> {
        let end = Instant::now() + Duration::from_secs(15);
        loop {
            let state = self.eval(
                "JSON.stringify(window.__jkpError ? 'error: ' + window.__jkpError : window.__jkpInstalled === true)",
                1000,
            );
            match state {
                Ok(Value::Bool(true)) => return Ok(()),
                Ok(Value::String(error)) => return Err(format!("the kit did not install: {error}")),
                _ => {}
            }
            if Instant::now() >= end {
                return Err("the kit's install never finished within 15 s".into());
            }
            pump(100);
        }
    }

    /// Holds `key` for `ms`: one key-down, the server's own repeats, one key-up.
    fn hold(&self, key: &str, ms: u64) -> Result<(), String> {
        x_tool("xdotool", &self.display, &["keydown", key])?;
        pump(ms);
        x_tool("xdotool", &self.display, &["keyup", key]).map(|_| ())
    }

    fn eval(&self, script: &str, timeout_ms: u64) -> Result<Value, String> {
        let outcome: Rc<RefCell<Option<Result<Value, String>>>> = Rc::new(RefCell::new(None));
        {
            let outcome = outcome.clone();
            self.webview
                .evaluate_javascript(script, None, None, None::<&gtk4::gio::Cancellable>, move |r| {
                    let parsed = r.map_err(|e| format!("evaluate_javascript failed: {e}")).and_then(|v| {
                        serde_json::from_str::<Value>(&v.to_str()).map_err(|e| format!("result JSON: {e}"))
                    });
                    outcome.borrow_mut().get_or_insert(parsed);
                });
        }
        let context = glib::MainContext::default();
        let end = Instant::now() + Duration::from_millis(timeout_ms);
        while Instant::now() < end {
            while context.iteration(false) {}
            if outcome.borrow().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let result = outcome.borrow_mut().take().unwrap_or_else(|| Err("timed out".into()));
        result
    }

    fn close(self) {
        self.window.destroy();
        pump(50);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("plugin_page_jk_hold: ignored (drives a real WebKitGTK WebView); run with `-- --ignored`");
        return;
    }
    let server = own_x_server::init_gtk("plugin_page_jk_hold", "1400x1100x24");
    for tool in ["xdotool", "xset"] {
        if Command::new(tool).arg("-version").output().is_err() && Command::new(tool).arg("version").output().is_err() {
            eprintln!("plugin_page_jk_hold: `{tool}` is not on PATH");
            server.exit(1);
        }
    }
    let code = run(server.display());
    server.exit(code);
}

fn run(display: &str) -> i32 {
    let rate = std::env::var("JKP_REPEAT").unwrap_or_else(|_| "30".into());
    if let Err(e) = x_tool("xset", display, &["r", "rate", "250", &rate]) {
        eprintln!("plugin_page_jk_hold: {e}");
        return 1;
    }
    let other = std::env::var("JKP_KIT").ok();
    let kit = match &other {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("plugin_page_jk_hold: JKP_KIT {path}: {e}");
                return 1;
            }
        },
        None => KIT_JS.to_string(),
    };
    let out = std::env::var("JKP_OUT").ok().map(PathBuf::from);
    if let Some(dir) = &out {
        // A requested log that cannot be written must fail the run, or the numbers lose their evidence.
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("plugin_page_jk_hold: JKP_OUT {}: {e}", dir.display());
            return 1;
        }
    }
    let label = if other.is_some() { "other" } else { "this" };

    let runs = match measure(display, &kit, label, out.as_ref()) {
        Ok(runs) => runs,
        Err(e) => {
            eprintln!("plugin_page_jk_hold: {e}");
            return 1;
        }
    };
    if other.is_some() {
        println!("plugin_page_jk_hold: measured {label}, nothing checked");
        return 0;
    }
    let mut failures = Vec::new();
    for (name, m) in &runs {
        // A held key glides: the view moves in every frame of the steady hold, never back, and evenly. The
        // spread bound is relative, so a slower or faster machine moves it only through its own frame times.
        // A view that never moved, or sat at the end it was going to, has no glide to judge: the still and
        // spread figures skip such frames, so a key that jumped straight to the end would otherwise pass.
        if m.speed <= 0.0 || m.step_median <= 0.0 {
            failures.push(format!("{name}: the view did not move during the steady hold"));
        }
        if m.at_end > 0 {
            failures.push(format!(
                "{name}: the view was already at the end in {} of {} frames of the steady hold",
                m.at_end, m.frames
            ));
        }
        if m.still > 0 {
            failures.push(format!(
                "{name}: the view stood still in {} of {} frames",
                m.still, m.frames
            ));
        }
        if m.reversed > 0 {
            failures.push(format!("{name}: {} frames moved against the key", m.reversed));
        }
        if m.step_cv > 0.2 {
            failures.push(format!(
                "{name}: the per-frame move's spread is {:.2} (at most 0.20)",
                m.step_cv
            ));
        }
    }
    if failures.is_empty() {
        println!("plugin_page_jk_hold: ok");
        0
    } else {
        for f in &failures {
            eprintln!("  - {f}");
        }
        1
    }
}

fn save(out: Option<&PathBuf>, name: &str, log: &Value) -> Result<(), String> {
    if let Some(dir) = out {
        let path = dir.join(format!("{name}.json"));
        std::fs::write(&path, log.to_string()).map_err(|e| format!("JKP_OUT {}: {e}", path.display()))?;
    }
    Ok(())
}

fn measure(display: &str, kit: &str, label: &str, out: Option<&PathBuf>) -> Result<Vec<(String, Metrics)>, String> {
    let h = Harness::new(&format!("plugin page jk hold {label}"), display, kit)?;
    h.wait_installed()?;
    pump(300);
    let scroller = "document.scrollingElement";
    let mut runs = Vec::new();
    for (key, down) in [("j", true), ("k", false)] {
        // `j` starts at the top and `k` at the bottom, so neither reaches the end it is going to.
        let start = if down { "0" } else { "el.scrollHeight" };
        h.eval(
            &format!("JSON.stringify((() => {{ const el = {scroller}; el.scrollTop = {start}; return true; }})())"),
            2000,
        )?;
        pump(400);
        h.eval(&start_log_js(scroller), 2000)?;
        h.eval(GLIDE_TRACE_JS, 2000)?;
        pump(100);
        h.hold(key, HOLD_MS)?;
        pump(AFTER_MS);
        let log = h.eval(STOP_LOG_JS, 5000)?;
        let name = format!("{label}-{key}");
        save(out, &name, &log)?;
        let m = analyse(&log, key, down)?;
        report("JKP", &name, &m);
        runs.push((name, m));
    }
    h.close();
    Ok(runs)
}
