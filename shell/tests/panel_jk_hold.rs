//! Measures held `j` and `k` scrolling in the agent panel against a real WebKitGTK page.
//! The test sends one XTEST key-down, lets the X server repeat at a set rate
//! (`xset r rate`), then sends one key-up. The page logs each animation frame's time,
//! `scrollTop`, the cursor row's top and each keydown. Frame times and movement per
//! frame reveal missed frames, jumps, stalls and reversals. The same measurements
//! are taken for WebKitGTK's held-arrow scrolling on a long plain page for comparison.
//!
//! Uses a plain `main` (`harness = false`) because GTK must own the main thread.
//! Run explicitly with:
//!
//!     cargo test -p shell --test panel_jk_hold -- --ignored
//!
//! It starts its own X server through `support/own_x_server.rs` and uses no other display.
//! Needs `Xvfb`, `xdotool` and `xset`. Run one GUI test at a time on the machine.
//!
//! - `JKH_HTML=<path>` measures another page build, such as an older `dist/index.html`, without assertions.
//! - `JKH_OUT=<dir>` saves each run's frame log as JSON and a mid-hold screenshot using `import`.
//! - `JKH_REPEAT=<per second>` sets the repeat rate, defaulting to 30 after a 250ms delay.
//! - `JKH_GROUPS=<n>` sets the conversation length to `4n` rows, defaulting to 40 groups.
//! - `JKH_CSS=<rules>` adds a stylesheet to the panel page.
//! - `JKH_SAMPLER=1` logs when the page's main thread was busy for more than 30ms.
//! - `JKH_RECORD=1`, with `JKH_OUT`, records video of each held key, including the plain page.
//!
//! Prints one `JKH` line per run.
use std::cell::RefCell;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};

use agent::{AgentDomainEvent, AgentSessionProjection, ContentKind, TurnOutcome};
use eitri_core::agent_backend::{BackendGreeting, BackendKind, ProjectionRef, CLIENT_IMPLEMENTED_PERMISSION_MODES};
use eitri_core::agent_bridge::{
    parse_inbound_message, serialize_command_result_for_js, serialize_hello_for_js, serialize_pane_focus_for_js,
    serialize_snapshot_for_js, serialize_tabs_for_js, serialize_theme_for_js, InboundMessage, SessionModeChoice,
    SnapshotView, TabStateWire, TabView,
};
use eitri_core::theme::ThemeTokens;
use gtk4::glib;
use gtk4::prelude::*;
use serde_json::{json, Value};
use webkit6::prelude::*;
use webkit6::{UserContentManager, WebView};

// The display this test runs on: its own Xvfb, never an inherited one.
#[path = "support/own_x_server.rs"]
mod own_x_server;

/// `panel/src/panel_document.rs`'s own `const PANEL_BASE_URI`, duplicated as `panel_jk_scroll.rs` does.
const PANEL_BASE_URI: &str = "https://eitri.invalid/";

/// The document `shell/src/agent_panel.rs` embeds, byte for byte.
const AGENT_UI_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../agent-ui/web/dist/index.html"));

fn themed_document(html: &str, vars: &[(String, String)]) -> String {
    let declarations: String = vars.iter().map(|(name, value)| format!("{name}:{value};")).collect();
    let style = format!("<style id=\"nv-theme\">:root{{{declarations}}}</style>");
    let at = html.find("<head>").expect("the panel document has a <head>") + "<head>".len();
    format!("{}{}{}", &html[..at], style, &html[at..])
}

fn panel_base_uri_matches_product() -> Result<(), String> {
    let source = include_str!("../../panel/src/panel_document.rs");
    let needle = format!("const PANEL_BASE_URI: &str = \"{PANEL_BASE_URI}\";");
    if !source.contains(&needle) {
        return Err(format!(
            "panel_document.rs's own PANEL_BASE_URI no longer reads {needle:?} -- update the copy in this file"
        ));
    }
    Ok(())
}

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

fn regex_escape(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if ".^$*+?()[]{}|\\".contains(c) {
                vec!['\\', c]
            } else {
                vec![c]
            }
        })
        .collect()
}

struct Harness {
    window: gtk4::Window,
    webview: WebView,
    display: String,
    xid: String,
}

impl Harness {
    /// A window holding `document` (loaded at the panel's base URI); `replay` is dispatched on the page's
    /// `ready` (empty for a page that is not the panel).
    fn new(
        title: String,
        display: &str,
        document: String,
        replay: Vec<String>,
        size: (i32, i32),
    ) -> Result<Self, String> {
        let window = gtk4::Window::new();
        window.set_default_size(size.0, size.1);
        window.set_title(Some(&title));
        let content_manager = UserContentManager::new();
        let webview = WebView::builder().user_content_manager(&content_manager).build();
        webview.set_hexpand(true);
        webview.set_vexpand(true);
        window.set_child(Some(&webview));

        let queue: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(replay));
        let started = Rc::new(std::cell::Cell::new(false));
        content_manager.register_script_message_handler("eitriAgent", None);
        {
            let queue = queue.clone();
            let started = started.clone();
            let webview_weak = webview.downgrade();
            content_manager.connect_script_message_received(Some("eitriAgent"), move |_manager, js_value| {
                let Some(webview) = webview_weak.upgrade() else { return };
                let raw = js_value.to_str().to_string();
                if let Some(InboundMessage::Ready { request_id }) = parse_inbound_message(&raw) {
                    if !started.replace(true) {
                        let ready_ack = serialize_command_result_for_js(&request_id, Ok(()));
                        let mut batch = queue.borrow_mut();
                        batch.push(ready_ack);
                        for payload in batch.drain(..) {
                            dispatch(&webview, &payload);
                        }
                    }
                }
            });
        }
        webview.load_html(&document, Some(PANEL_BASE_URI));
        window.present();
        pump(400);
        let pattern = format!("^{}$", regex_escape(&title));
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

    fn wait_for(&self, script: &str, what: &str) -> Result<(), String> {
        let end = Instant::now() + Duration::from_secs(15);
        loop {
            if matches!(self.eval(script, 1000), Ok(Value::Bool(true))) {
                return Ok(());
            }
            if Instant::now() >= end {
                return Err(format!("{what} never happened within 15 s"));
            }
            pump(100);
        }
    }

    fn key(&self, keys: &str) -> Result<(), String> {
        x_tool("xdotool", &self.display, &["key", "--clearmodifiers", keys]).map(|_| ())
    }

    /// With `JKH_RECORD` and an output directory, a video of this window while `key` is held for `ms`
    /// (`ffmpeg`'s x11grab), into `<dir>/<name>.mp4`; never during a measured hold, for the same reason as
    /// a screenshot.
    fn record(&self, dir: &PathBuf, name: &str, key: &str, ms: u64) -> Result<(), String> {
        let geometry = x_tool("xdotool", &self.display, &["getwindowgeometry", "--shell", &self.xid])?;
        let field = |k: &str| {
            geometry
                .lines()
                .find_map(|l| l.strip_prefix(&format!("{k}=")))
                .and_then(|v| v.trim().parse::<i32>().ok())
                .unwrap_or(0)
        };
        let (x, y, w, h) = (field("X"), field("Y"), field("WIDTH") & !1, field("HEIGHT") & !1);
        let mut child = Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-y",
                "-f",
                "x11grab",
                "-framerate",
                "60",
                "-video_size",
            ])
            .arg(format!("{w}x{h}"))
            .arg("-i")
            .arg(format!("{}+{x},{y}", self.display))
            .args([
                "-t",
                &format!("{:.1}", (ms + 1200) as f64 / 1000.0),
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(dir.join(format!("{name}.mp4")))
            .env("DISPLAY", &self.display)
            .env_remove("WAYLAND_DISPLAY")
            .spawn()
            .map_err(|e| format!("ffmpeg not runnable ({e})"))?;
        pump(400);
        self.hold(key, ms, None)?;
        pump(900);
        let _ = child.wait();
        Ok(())
    }

    /// Holds `key` for `ms`: one key-down, the server's own repeats, one key-up.
    fn hold(&self, key: &str, ms: u64, shot: Option<&PathBuf>) -> Result<(), String> {
        x_tool("xdotool", &self.display, &["keydown", key])?;
        if let Some(path) = shot {
            pump(ms / 2);
            let _ = x_tool(
                "import",
                &self.display,
                &["-window", &self.xid, path.to_str().unwrap_or("shot.png")],
            );
            pump(ms / 2);
        } else {
            pump(ms);
        }
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

fn dispatch(webview: &WebView, json_payload: &str) {
    let script = format!(
        "window.__eitriDispatch({});",
        serde_json::to_string(json_payload).unwrap_or_default()
    );
    webview.evaluate_javascript(&script, None, None, None::<&gtk4::gio::Cancellable>, |result| {
        if let Err(e) = result {
            eprintln!("[panel_jk_hold] evaluate_javascript failed: {e}");
        }
    });
}

// ---------------------------------------------------------------------------------------------
// The conversation: forty rows of very different heights.
// ---------------------------------------------------------------------------------------------

const VOCAB: [&str; 24] = [
    "build", "the", "cache", "layout", "scroll", "when", "cursor", "moves", "across", "a", "long", "reply", "and",
    "then", "lands", "on", "short", "row", "while", "reading", "text", "that", "wraps", "over",
];

fn words(seed: usize, n: usize) -> String {
    (0..n)
        .map(|i| VOCAB[(seed * 7 + i * 13 + i * i) % VOCAB.len()])
        .collect::<Vec<_>>()
        .join(" ")
}

fn paragraphs(tag: &str, count: usize, n_words: usize) -> String {
    (1..=count)
        .map(|i| format!("{tag}-P{i:02}: {}.", words(i + tag.len(), n_words)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Groups of four rows in the conversation: `JKH_GROUPS`, 10 (40 rows) when unset.
fn groups() -> usize {
    std::env::var("JKH_GROUPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10)
}

fn replay_fixture() -> Vec<String> {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::SessionOpened {
        session_id: "jk-hold-test".into(),
        provider_session_id: "claude-jk-hold-test".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/nonexistent/panel-jk-hold".into(),
    });
    let turn = "t1".to_string();
    let text = |t: String| AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: ContentKind::Text,
        text: t,
    };
    let boundary = || AgentDomainEvent::AssistantMessageBoundary { turn_id: "t1".into() };
    let mut events = vec![AgentDomainEvent::TurnStarted { turn_id: turn.clone() }];
    for i in 0..groups() {
        // A one-liner, a long reply, a tool result, a short reply: heights from ~25px to several views.
        events.push(text(format!("S{i}: {}.", words(i, 9))));
        events.push(boundary());
        events.push(text(paragraphs(&format!("L{i}"), 2 + (i * 5) % 17, 50)));
        events.push(boundary());
        let id = format!("toolu_{i}");
        events.push(AgentDomainEvent::ToolCallStarted {
            turn_id: turn.clone(),
            tool_use_id: id.clone(),
            name: "Bash".into(),
            input: json!({ "command": format!("cat build-{i}.log") }),
        });
        events.push(AgentDomainEvent::ToolCallCompleted {
            turn_id: turn.clone(),
            tool_use_id: id,
            content: json!((1..=40)
                .map(|n| format!("line {n:03}: ok"))
                .collect::<Vec<_>>()
                .join("\n")),
            is_error: false,
        });
        events.push(text(format!("M{i}: {}.", words(i + 3, 40))));
        events.push(boundary());
    }
    events.push(AgentDomainEvent::TurnCompleted {
        turn_id: turn,
        outcome: TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: Some("end_turn".into()),
        usage: None,
        detail: Default::default(),
    });
    for event in &events {
        projection.apply(event);
    }
    let tokens = ThemeTokens::fallback();
    let greeting = BackendGreeting {
        kind: BackendKind::Sidecar,
        project_dir: PathBuf::from("/nonexistent/panel-jk-hold"),
        permission_modes: CLIENT_IMPLEMENTED_PERMISSION_MODES,
        expected_verdandi_revision: Some(agent::EXPECTED_VERDANDI_REVISION),
        resumable: Vec::new(),
        account: None,
    };
    let snapshot = serialize_snapshot_for_js(
        eitri_core::tabs::TabId(1),
        &SnapshotView {
            backend: "sidecar",
            conversation_id: Some("conversation-jk-hold-test"),
            session_id: Some("jk-hold-test"),
            provider_session_id: Some("claude-jk-hold-test".into()),
            capabilities: agent::ProviderCapabilities {
                resume: true,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
                cli_auto_mode: false,
            },
            provider: None,
            projection: ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        },
        None,
    );
    let tabs = vec![TabView {
        id: eitri_core::tabs::TabId(1),
        number: 1,
        label: "1".into(),
        name: None,
        state: TabStateWire::Live,
        mode: SessionModeChoice::Auto,
        marker: None,
        pending: 0,
        resumable: false,
        failure: None,
        failed_after_start: false,
        resumes_on_r: false,
        title: None,
        agent: "claude".into(),
    }];
    vec![
        serialize_hello_for_js(&greeting),
        serialize_theme_for_js(&tokens),
        serialize_tabs_for_js(eitri_core::tabs::TabId(1), &tabs, SessionModeChoice::Auto),
        snapshot,
        serialize_pane_focus_for_js(true),
    ]
}

/// WebKitGTK's own held-arrow scroll is measured on this: a long page of the same prose, nothing else.
fn plain_page() -> String {
    let body: String = (0..120)
        .map(|i| format!("<p>{}</p>", words(i, 60)))
        .collect::<Vec<_>>()
        .join("\n");
    format!("<!doctype html><html><head><style>body{{font: 15px/1.65 sans-serif; margin: 12px; max-width: 560px}}</style></head><body>{body}</body></html>")
}

// ---------------------------------------------------------------------------------------------
// The frame log.
// ---------------------------------------------------------------------------------------------

/// Starts a log of every animation frame (time, the scroller's `scrollTop`, the cursor row's top) and every
/// keydown/keyup, on `window.__jkh`. `scroller` is the expression for the element that scrolls.
fn start_log_js(scroller: &str) -> String {
    let sampler = std::env::var("JKH_SAMPLER").is_ok();
    format!(
        r#"JSON.stringify((() => {{
  if (window.__jkh && window.__jkh.raf) cancelAnimationFrame(window.__jkh.raf);
  const el = {scroller};
  const log = {{ frames: [], keys: [], trace: [] }};
  window.__jkh = log;
  window.__eitriHoldTrace = (f) => log.trace.push([f.t, f.scrollTop, f.cursor, f.y, f.speed]);
  const tick = (t) => {{
    const cur = el.querySelector ? el.querySelector(':scope > .row-current') : null;
    log.frames.push([t, el.scrollTop, cur ? cur.offsetTop : -1, el.scrollHeight - el.clientHeight, cur ? cur.className : '']);
    log.raf = requestAnimationFrame(tick);
  }};
  log.raf = requestAnimationFrame(tick);
  // With `JKH_SAMPLER`, the page's main thread: a 4ms timer whose late runs say how long the thread was
  // busy, and when. Off by default: the timer is load of its own.
  log.gaps = [];
  if ({sampler}) {{
    let last = performance.now();
    log.sampler = setInterval(() => {{
      const now = performance.now();
      if (now - last > 30) log.gaps.push([last, now - last]);
      last = now;
    }}, 4);
  }}
  if (!window.__jkhKeys) {{
    window.__jkhKeys = true;
    document.addEventListener('keydown', (e) => window.__jkh.keys.push([e.timeStamp, e.key, e.repeat]), true);
    document.addEventListener('keyup', (e) => window.__jkh.keys.push([e.timeStamp, 'up:' + e.key, false]), true);
  }}
  return true;
}})())"#
    )
}

const STOP_LOG_JS: &str = r#"JSON.stringify((() => {
  const log = window.__jkh;
  cancelAnimationFrame(log.raf);
  clearInterval(log.sampler);
  return { frames: log.frames, keys: log.keys, trace: log.trace, gaps: log.gaps };
})())"#;

#[derive(Debug, Default)]
struct Metrics {
    repeats: usize,
    frames: usize,
    dt_median: f64,
    dt_p95: f64,
    dt_max: f64,
    /// Frames that took more than 1.5 frame intervals (dropped frames).
    long_frames: usize,
    step_median: f64,
    step_max: f64,
    /// Frames during the hold (after the first repeat) in which the view did not move.
    still: usize,
    /// Frames in which it moved against the key.
    reversed: usize,
    /// Moves more than twice the median move.
    jumps: usize,
    /// The spread of the per-frame move, as a share of the median (0: perfectly even).
    step_cv: f64,
    /// Velocity in px/ms over the steady part of the hold.
    speed: f64,
    cursor_rows: usize,
    after_release_px: f64,
    after_release_ms: f64,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

fn analyse(log: &Value, key: &str, down: bool) -> Result<Metrics, String> {
    let frames: Vec<(f64, f64, f64, f64)> = log["frames"]
        .as_array()
        .ok_or("no frames")?
        .iter()
        .map(|f| {
            (
                f[0].as_f64().unwrap_or(0.0),
                f[1].as_f64().unwrap_or(0.0),
                f[2].as_f64().unwrap_or(-1.0),
                f[3].as_f64().unwrap_or(f64::INFINITY),
            )
        })
        .collect();
    let keys = log["keys"].as_array().ok_or("no keys")?;
    let repeats: Vec<f64> = keys
        .iter()
        .filter(|k| k[1].as_str() == Some(key) && k[2].as_bool() == Some(true))
        .map(|k| k[0].as_f64().unwrap_or(0.0))
        .collect();
    let up = keys
        .iter()
        .find(|k| k[1].as_str() == Some(&format!("up:{key}")))
        .and_then(|k| k[0].as_f64())
        .ok_or_else(|| format!("no keyup of {key} logged"))?;
    let first_repeat = *repeats
        .first()
        .ok_or_else(|| format!("no repeat of {key} arrived: the X server's auto-repeat did not run"))?;
    // The steady part: from 300ms after the first repeat (past the glide's ramp) to the key-up.
    let hold: Vec<&(f64, f64, f64, f64)> = frames
        .iter()
        .filter(|f| f.0 >= first_repeat + 300.0 && f.0 <= up)
        .collect();
    if hold.len() < 10 {
        return Err(format!("only {} frames during the hold", hold.len()));
    }
    let mut dts: Vec<f64> = hold.windows(2).map(|w| w[1].0 - w[0].0).collect();
    let sign = if down { 1.0 } else { -1.0 };
    let steps: Vec<f64> = hold.windows(2).map(|w| sign * (w[1].1 - w[0].1)).collect();
    // A frame that starts with the view already at the end it is going to has nowhere to move.
    let at_end: Vec<bool> = hold
        .windows(2)
        .map(|w| if down { w[0].1 >= w[0].3 - 0.5 } else { w[0].1 <= 0.5 })
        .collect();
    dts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let dt_median = percentile(&dts, 0.5);
    let mut sorted_steps: Vec<f64> = steps.iter().copied().filter(|s| *s > 0.0).collect();
    sorted_steps.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let step_median = percentile(&sorted_steps, 0.5);
    let mean = steps.iter().sum::<f64>() / steps.len() as f64;
    let var = steps.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / steps.len() as f64;
    let span_t = hold.last().unwrap().0 - hold.first().unwrap().0;
    let span_px = sign * (hold.last().unwrap().1 - hold.first().unwrap().1);
    let mut cursor_rows: Vec<f64> = frames
        .iter()
        .filter(|f| f.0 >= first_repeat && f.0 <= up)
        .map(|f| f.2)
        .collect();
    cursor_rows.dedup();
    let at_up = frames.iter().filter(|f| f.0 <= up).last().map(|f| f.1).unwrap_or(0.0);
    let last = frames.last().map(|f| f.1).unwrap_or(0.0);
    let settled_at = frames
        .iter()
        .rev()
        .find(|f| (f.1 - last).abs() > 0.5)
        .map(|f| f.0 - up)
        .unwrap_or(0.0);
    Ok(Metrics {
        repeats: repeats.len(),
        frames: hold.len(),
        dt_median,
        dt_p95: percentile(&dts, 0.95),
        dt_max: dts.last().copied().unwrap_or(0.0),
        long_frames: dts.iter().filter(|d| **d > dt_median * 1.5).count(),
        step_median,
        step_max: sorted_steps.last().copied().unwrap_or(0.0),
        still: steps
            .iter()
            .zip(&at_end)
            .filter(|(s, end)| s.abs() < 0.5 && !**end)
            .count(),
        reversed: steps.iter().filter(|s| **s < -0.5).count(),
        jumps: steps.iter().filter(|s| **s > 2.0 * step_median.max(1.0)).count(),
        step_cv: if mean.abs() > 1e-9 {
            var.sqrt() / mean.abs()
        } else {
            0.0
        },
        speed: if span_t > 0.0 { span_px / span_t } else { 0.0 },
        cursor_rows: cursor_rows.len(),
        after_release_px: sign * (last - at_up),
        after_release_ms: settled_at.max(0.0),
    })
}

fn report(run: &str, m: &Metrics) {
    println!(
        "JKH {run}: repeats={} frames={} dt_median={:.1}ms dt_p95={:.1}ms dt_max={:.1}ms long_frames={} step_median={:.1}px step_max={:.1}px still={} reversed={} jumps={} step_cv={:.2} speed={:.2}px/ms cursor_rows={} after_release={:.0}px/{:.0}ms",
        m.repeats, m.frames, m.dt_median, m.dt_p95, m.dt_max, m.long_frames, m.step_median, m.step_max, m.still,
        m.reversed, m.jumps, m.step_cv, m.speed, m.cursor_rows, m.after_release_px, m.after_release_ms
    );
}

fn main() {
    if let Err(e) = panel_base_uri_matches_product() {
        eprintln!("panel_jk_hold: {e}");
        std::process::exit(1);
    }
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("panel_jk_hold: ignored (drives a real WebKitGTK WebView); run with `-- --ignored`");
        return;
    }
    let server = own_x_server::init_gtk("panel_jk_hold", "1400x1100x24");
    for tool in ["xdotool", "xset"] {
        if Command::new(tool).arg("-version").output().is_err() && Command::new(tool).arg("version").output().is_err() {
            eprintln!("panel_jk_hold: `{tool}` is not on PATH");
            server.exit(1);
        }
    }
    let code = run(server.display());
    server.exit(code);
}

fn run(display: &str) -> i32 {
    let rate = std::env::var("JKH_REPEAT").unwrap_or_else(|_| "30".into());
    if let Err(e) = x_tool("xset", display, &["r", "rate", "250", &rate]) {
        eprintln!("panel_jk_hold: {e}");
        return 1;
    }
    let other = std::env::var("JKH_HTML").ok();
    let html = match &other {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("panel_jk_hold: JKH_HTML {path}: {e}");
                return 1;
            }
        },
        None => AGENT_UI_HTML.to_string(),
    };
    let out = std::env::var("JKH_OUT").ok().map(PathBuf::from);
    if let Some(dir) = &out {
        let _ = std::fs::create_dir_all(dir);
    }
    let label = if other.is_some() { "other" } else { "this" };
    let mut failures = Vec::new();

    match measure_panel(display, &html, label, out.as_ref()) {
        Ok(runs) => {
            if other.is_none() {
                for (name, m) in &runs {
                    // A held key glides: no jump, no reversal, the view moving in all but a few frames.
                    if m.jumps > m.frames / 20 {
                        failures.push(format!("{name}: {} jumps over {} frames", m.jumps, m.frames));
                    }
                    if m.reversed > 0 {
                        failures.push(format!("{name}: {} frames moved against the key", m.reversed));
                    }
                    if m.still > m.frames / 10 {
                        failures.push(format!(
                            "{name}: the view stood still in {} of {} frames",
                            m.still, m.frames
                        ));
                    }
                    if m.cursor_rows < 3 {
                        failures.push(format!("{name}: the cursor passed only {} rows", m.cursor_rows));
                    }
                }
            }
        }
        Err(e) => failures.push(format!("panel: {e}")),
    }
    match measure_plain(display, out.as_ref()) {
        Ok(m) => report("webkit-arrow", &m),
        Err(e) => eprintln!("[panel_jk_hold] the plain page's arrow scroll could not be measured: {e}"),
    }
    if failures.is_empty() {
        println!("panel_jk_hold: ok");
        0
    } else {
        for f in &failures {
            eprintln!("  - {f}");
        }
        1
    }
}

fn save(out: Option<&PathBuf>, name: &str, log: &Value) {
    if let Some(dir) = out {
        let _ = std::fs::write(dir.join(format!("{name}.json")), log.to_string());
    }
}

fn measure_panel(
    display: &str,
    html: &str,
    label: &str,
    out: Option<&PathBuf>,
) -> Result<Vec<(String, Metrics)>, String> {
    let mut document = themed_document(html, &ThemeTokens::fallback().css_vars());
    // An experiment's extra rules, after everything else in the head.
    if let Ok(css) = std::env::var("JKH_CSS") {
        let at = document.find("</head>").ok_or("the panel document has no </head>")?;
        document.insert_str(at, &format!("<style>{css}</style>"));
    }
    let h = Harness::new(
        format!("panel jk hold {label}"),
        display,
        document,
        replay_fixture(),
        (600, 965),
    )?;
    h.wait_for(
        &format!(
            "JSON.stringify(document.querySelectorAll('.message-list > [data-nav-stop=\"row\"]').length >= {})",
            4 * groups()
        ),
        "the conversation's rows",
    )?;
    pump(500);
    h.key("g")?;
    h.key("g")?;
    pump(400);
    let list = "document.querySelector('.message-list')";
    let mut runs = Vec::new();
    for (key, down) in [("j", true), ("k", false)] {
        h.eval(&start_log_js(list), 2000)?;
        pump(100);
        // No screenshot in a measured hold: `import` holds this process, which is WebKit's UI process,
        // and no frame is presented meanwhile.
        h.hold(key, 3000, None)?;
        pump(700);
        let log = h.eval(STOP_LOG_JS, 5000)?;
        let name = format!("{label}-{key}");
        save(out, &name, &log);
        let m = analyse(&log, key, down)?;
        report(&name, &m);
        runs.push((name, m));
    }
    // Screenshots mid-hold, and with `JKH_RECORD` a video of each key, from holds of their own that are
    // not measured.
    if let Some(dir) = out {
        if std::env::var("JKH_RECORD").is_ok() {
            h.key("g")?;
            h.key("g")?;
            pump(400);
            h.record(dir, &format!("{label}-j"), "j", 3000)?;
            h.record(dir, &format!("{label}-k"), "k", 3000)?;
        }
        for key in ["j", "k"] {
            let shot = dir.join(format!("{label}-{key}-mid-hold.png"));
            h.hold(key, 1600, Some(&shot))?;
            pump(600);
            let _ = x_tool(
                "import",
                display,
                &[
                    "-window",
                    &h.xid,
                    dir.join(format!("{label}-{key}-after-release.png"))
                        .to_str()
                        .unwrap_or("after.png"),
                ],
            );
        }
    }
    h.close();
    Ok(runs)
}

fn measure_plain(display: &str, out: Option<&PathBuf>) -> Result<Metrics, String> {
    let h = Harness::new("plain page arrow".into(), display, plain_page(), Vec::new(), (600, 965))?;
    pump(500);
    h.eval(&start_log_js("document.scrollingElement"), 2000)?;
    pump(100);
    h.hold("Down", 3000, None)?;
    pump(700);
    let log = h.eval(STOP_LOG_JS, 5000)?;
    save(out, "webkit-arrow", &log);
    if let (Some(dir), true) = (out, std::env::var("JKH_RECORD").is_ok()) {
        h.key("Home")?;
        pump(500);
        h.record(dir, "webkit-arrow", "Down", 3000)?;
    }
    h.close();
    analyse(&log, "ArrowDown", true)
}
