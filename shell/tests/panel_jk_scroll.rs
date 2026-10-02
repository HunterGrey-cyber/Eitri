//! The agent panel's `j`/`k` over rows of very different heights -- a two-line reply, a tool result, a
//! reply taller than the view, a code block, a one-line reply -- driven against the real WebKitGTK `shell`
//! links, not jsdom (which has no layout, so the rules in `agent-ui/web/src/jkScroll.ts` can only be
//! checked there as arithmetic).
//!
//! What this turns the 2026-10-01 measurement (`jk_measure.rs` on `scratch/jk-measure`) into, as
//! assertions, over the window sizes and zoom levels it measured:
//! - **no single `j`/`k` moves the view by more than two thirds of it** (it was a whole view: -901 of 900
//!   when `k` entered a row taller than the view; `G`/`gg`/`Ctrl+f`/`Ctrl+b` are not asserted);
//! - **the cursor row always keeps at least two lines on screen** after a press (it was 0.39px at zoom 1.0,
//!   a dead press), and a row that fits keeps a two-line margin on both sides;
//! - **the same press sequence at zoom 1.0 and 1.5 visits the same rows in the same order**, each landing in
//!   the same place (it was nothing at one and a whole view at the other, from a sub-pixel);
//! - **`k` into a tall row lands on its end** (two thirds of the way down), **`j` on its head** (a third);
//! - **counts move that many steps inside a row**, a tool result's box is entered at the end nearest the
//!   reader and moves three of its own lines a press;
//! - **a lone press eases the view over about 150ms** without the list's scroll listener pulling the cursor
//!   off the row it is moving to, and a second press during the ease ends where two slow presses end;
//! - **a folded result's preview** is three lines and a count, and a runaway first line cannot widen the
//!   list.
//!
//! A plain `main` (`harness = false`), because GTK must own the main thread; opt-in with `-- --ignored`:
//!
//!     cargo test -p shell --test panel_jk_scroll -- --ignored
//!
//! It starts its own X server (`support/own_x_server.rs`) and talks to no other display; keys arrive as real
//! X key events (XTEST via `xdotool`), the harness copied from `panel_visual_mode.rs`. Needs `Xvfb` and
//! `xdotool`. One GUI pass at a time on the machine, like every other: take the slot first
//! (`the private review notes`). It prints one `JKS` line per run with the worst single move as a
//! fraction of the view, so a regression shows as a number as well as a failure.
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

// The display this test runs on: its own Xvfb, never an inherited one (shared since 2026-09-29).
#[path = "support/own_x_server.rs"]
mod own_x_server;

/// `shell/src/agent_panel.rs`'s own private `const PANEL_BASE_URI`, duplicated (this crate has no `[lib]`
/// target an integration test can reach a private item through); `panel_base_uri_matches_product` pins the
/// two equal.
const PANEL_BASE_URI: &str = "https://eitri.invalid/";

/// The document `shell/src/agent_panel.rs` embeds, byte for byte.
const AGENT_UI_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../agent-ui/web/dist/index.html"));

/// `agent_panel::themed_document`'s exact insertion rule (duplicated the same way `panel_stream_scroll.rs`
/// does): the theme `<style>` goes directly after the first `<head>`.
fn themed_document(vars: &[(String, String)]) -> String {
    let declarations: String = vars.iter().map(|(name, value)| format!("{name}:{value};")).collect();
    let style = format!("<style id=\"nv-theme\">:root{{{declarations}}}</style>");
    let at = AGENT_UI_HTML.find("<head>").expect("the panel document has a <head>") + "<head>".len();
    format!("{}{}{}", &AGENT_UI_HTML[..at], style, &AGENT_UI_HTML[at..])
}

/// Needs no display: reads `agent_panel.rs`'s own source as text and checks the literal this file
/// duplicated is still the same one. Runs every time, `--ignored` or not.
fn panel_base_uri_matches_product() -> Result<(), String> {
    let source = include_str!("../src/agent_panel.rs");
    let needle = format!("const PANEL_BASE_URI: &str = \"{PANEL_BASE_URI}\";");
    if !source.contains(&needle) {
        return Err(format!(
            "agent_panel.rs's own PANEL_BASE_URI no longer reads {needle:?} -- update the copy in this file"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The X server this file owns, and the only one it will talk to: `support/own_x_server.rs`.
// ---------------------------------------------------------------------------------------------

/// Runs `xdotool` against `display` -- always passed explicitly, never inherited -- and returns its
/// stdout, or its stderr as the error on a non-zero exit.
fn xdotool(display: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new("xdotool")
        .args(args)
        .env("DISPLAY", display)
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .map_err(|e| format!("xdotool not runnable ({e})"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "xdotool {args:?} exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Runs the GLib main context for about `ms`, without a nested `MainLoop::run` -- the same idiom
/// `close_prompt_placement.rs`'s own `pump` uses.
fn pump(ms: u64) {
    let context = glib::MainContext::default();
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        while context.iteration(false) {}
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// `xdotool search --name` takes a regex; the titles here are plain words, digits, spaces and dots.
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

// ---------------------------------------------------------------------------------------------
// One window, one WebView, one replay.
// ---------------------------------------------------------------------------------------------

struct Harness {
    window: gtk4::Window,
    webview: WebView,
    /// Our own Xvfb's display name (`OwnXServer::display`), handed to every `xdotool`.
    display: String,
    /// This window's X id, found once by its unique title.
    xid: String,
}

impl Harness {
    fn new(zoom: f64, title: String, display: &str, replay: Vec<String>, size: (i32, i32)) -> Result<Self, String> {
        let window = gtk4::Window::new();
        window.set_default_size(size.0, size.1);
        window.set_title(Some(&title));

        let content_manager = UserContentManager::new();
        let webview = WebView::builder().user_content_manager(&content_manager).build();
        webview.set_hexpand(true);
        webview.set_vexpand(true);
        webview.set_zoom_level(zoom);
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
                            evaluate_js_dispatch(&webview, &payload);
                        }
                    }
                }
            });
        }
        webview.load_html(
            &themed_document(&ThemeTokens::fallback().css_vars()),
            Some(PANEL_BASE_URI),
        );
        window.present();
        pump(400); // settle: the snapshot's own layout, the window's map.

        // No `search --sync`: it blocks with the main loop stopped, forever if the window never
        // maps. Poll instead, pumping between tries.
        let pattern = format!("^{}$", regex_escape(&title));
        let mut xid = None;
        for _ in 0..50 {
            if let Ok(out) = xdotool(display, &["search", "--onlyvisible", "--name", &pattern]) {
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
        // The replay is dispatched on the page's own `ready`, which a debug build under load can take
        // well past the 400ms above to post; nothing below is meaningful before the conversation exists.
        let end = Instant::now() + Duration::from_secs(15);
        loop {
            let ready = harness.eval(
                "JSON.stringify(document.querySelector('.agent-ui-conversation .message-list') !== null)",
                1000,
            );
            if matches!(ready, Ok(Value::Bool(true))) {
                break;
            }
            if Instant::now() >= end {
                return Err(format!(
                    "the replay's conversation never rendered within 15 s ({ready:?})"
                ));
            }
            pump(100);
        }
        pump(200);
        Ok(harness)
    }

    /// X input focus to this window (`XSetInputFocus`, which needs no window manager), confirmed by
    /// `getwindowfocus` -- every key goes to the focused window through XTEST.
    fn focus(&self) -> Result<(), String> {
        for _ in 0..40 {
            let _ = xdotool(&self.display, &["windowfocus", &self.xid]);
            pump(20);
            if xdotool(&self.display, &["getwindowfocus"])
                .map(|s| s.trim() == self.xid)
                .unwrap_or(false)
            {
                return Ok(());
            }
            pump(30);
        }
        Err(format!("X focus never reached window {} on {}", self.xid, self.display))
    }

    /// One keystroke (an xdotool keysym chord, e.g. `j`, `G`, `ctrl+e`, `Return`) via XTEST.
    fn key(&self, keys: &str) -> Result<(), String> {
        xdotool(&self.display, &["key", "--clearmodifiers", keys]).map(|_| ())
    }

    /// Several keystrokes in one `xdotool` run, `delay_ms` apart: a person pressing faster than the view
    /// eases.
    fn keys_with_delay(&self, delay_ms: u32, keys: &[&str]) -> Result<(), String> {
        let delay = delay_ms.to_string();
        let mut args = vec!["key", "--clearmodifiers", "--delay", delay.as_str()];
        args.extend_from_slice(keys);
        xdotool(&self.display, &args).map(|_| ())
    }

    /// Evaluates `script` and returns its JSON-decoded result (or an error string), pumping the
    /// main loop until the callback lands or `timeout_ms` elapses.
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

fn evaluate_js_dispatch(webview: &WebView, json_payload: &str) {
    let script = format!(
        "window.__eitriDispatch({});",
        serde_json::to_string(json_payload).unwrap_or_default()
    );
    webview.evaluate_javascript(&script, None, None, None::<&gtk4::gio::Cancellable>, |result| {
        if let Err(e) = result {
            eprintln!("[panel_jk_scroll] evaluate_javascript failed: {e}");
        }
    });
}

fn opened_projection() -> AgentSessionProjection {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::SessionOpened {
        session_id: "jk-scroll-test".into(),
        provider_session_id: "claude-jk-scroll-test".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/nonexistent/panel-jk-scroll".into(),
    });
    projection
}

/// `hello`, the theme, the snapshot of `projection` on tab 1, and pane focus -- what Rust sends a
/// freshly loaded panel whose tab holds a live session.
fn on_ready_batch(projection: &AgentSessionProjection) -> Vec<String> {
    let tokens = ThemeTokens::fallback();
    let greeting = BackendGreeting {
        kind: BackendKind::Sidecar,
        project_dir: PathBuf::from("/nonexistent/panel-jk-scroll"),
        permission_modes: CLIENT_IMPLEMENTED_PERMISSION_MODES,
        expected_verdandi_revision: Some(agent::EXPECTED_VERDANDI_REVISION),
        resumable: Vec::new(),
        account: None,
    };
    let snapshot = serialize_snapshot_for_js(
        eitri_core::tabs::TabId(1),
        &SnapshotView {
            backend: "sidecar",
            conversation_id: Some("conversation-jk-scroll-test"),
            session_id: Some("jk-scroll-test"),
            provider_session_id: Some("claude-jk-scroll-test".into()),
            capabilities: agent::ProviderCapabilities {
                resume: true,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
                cli_auto_mode: false,
            },
            provider: None,
            projection: ProjectionRef::Borrowed(projection),
            hidden_pending: None,
        },
        None,
    );
    // A real launch's `TabSet` always sends a `tabs` envelope before the active tab's own snapshot
    // (`serialize_tabs_for_js`'s own doc), because the page drops a snapshot for a tab it does not know yet.
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
        title: None,
    }];
    vec![
        serialize_hello_for_js(&greeting),
        serialize_theme_for_js(&tokens),
        serialize_tabs_for_js(eitri_core::tabs::TabId(1), &tabs, SessionModeChoice::Auto),
        snapshot,
        serialize_pane_focus_for_js(true),
    ]
}

// ---------------------------------------------------------------------------------------------
// The fixture: long / short / long, with tool results and a code block between.
// ---------------------------------------------------------------------------------------------

const VOCAB: [&str; 40] = [
    "build", "the", "cache", "layout", "scroll", "when", "cursor", "moves", "across", "a", "long", "reply", "and",
    "then", "lands", "on", "short", "row", "while", "reading", "text", "that", "wraps", "over", "several", "lines",
    "inside", "panel", "view", "which", "keeps", "its", "own", "position", "until", "next", "press", "arrives", "from",
    "below",
];

/// Deterministic prose: `n` words from `VOCAB`, stepping by a prime so paragraphs differ.
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

fn tool_output(lines: usize) -> String {
    (1..=lines)
        .map(|n| format!("build line {n:03}: ok -- compiled unit {n} with no warnings"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn code_block(lines: usize) -> String {
    let body = (1..=lines)
        .map(|n| format!("    let value_{n:02} = compute_step({n}, &context)?; // step {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "CODE: here is the patch.\n\n```rust\nfn run(context: &Context) -> Result<()> {{\n{body}\n    Ok(())\n}}\n```"
    )
}

/// Row indices in the fixture, in order.
const S1: usize = 0;
const T1: usize = 1;
const L1: usize = 2;
const ROWS: usize = 8;

/// Rows: S1 short (2 lines), T1 a `Bash` result of `TOOL_LINES` lines, L1 long prose, S2 short, T2 a `Bash`
/// result whose first line is far wider than the panel, C a code block, S3 one line, L2 long prose.
fn replay_fixture() -> Vec<String> {
    let mut projection = opened_projection();
    let turn = "t1".to_string();
    let boundary = || AgentDomainEvent::AssistantMessageBoundary { turn_id: "t1".into() };
    let text = |t: String| AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: ContentKind::Text,
        text: t,
    };
    let events = vec![
        AgentDomainEvent::TurnStarted { turn_id: turn.clone() },
        text(format!("S1: {}.", words(3, 24))),
        AgentDomainEvent::ToolCallStarted {
            turn_id: turn.clone(),
            tool_use_id: "toolu_bash".into(),
            name: "Bash".into(),
            input: json!({ "command": "cat build.log" }),
        },
        AgentDomainEvent::ToolCallCompleted {
            turn_id: turn.clone(),
            tool_use_id: "toolu_bash".into(),
            content: json!(tool_output(TOOL_LINES)),
            is_error: false,
        },
        text(paragraphs("L1", 22, 60)),
        boundary(),
        text(format!("S2: {}.", words(5, 24))),
        boundary(),
        AgentDomainEvent::ToolCallStarted {
            turn_id: turn.clone(),
            tool_use_id: "toolu_wide".into(),
            name: "Bash".into(),
            input: json!({ "command": "cat wide.txt" }),
        },
        AgentDomainEvent::ToolCallCompleted {
            turn_id: turn.clone(),
            tool_use_id: "toolu_wide".into(),
            content: json!(format!("{}\nsecond line\nthird line\nfourth line", "w".repeat(700))),
            is_error: false,
        },
        text(code_block(40)),
        boundary(),
        text(format!("S3: {}.", words(9, 11))),
        boundary(),
        text(paragraphs("L2", 8, 60)),
        AgentDomainEvent::TurnCompleted {
            turn_id: turn.clone(),
            outcome: TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: Some("end_turn".into()),
            usage: None,
            detail: Default::default(),
        },
    ];
    for event in &events {
        projection.apply(event);
    }
    on_ready_batch(&projection)
}

/// Lines in T1's result: enough that its box (260px) has a good deal to scroll and its preview says
/// `+57 lines`.
const TOOL_LINES: usize = 60;

// ---------------------------------------------------------------------------------------------
// Reading the page.
// ---------------------------------------------------------------------------------------------

const STATE_JS: &str = r#"JSON.stringify((() => {
  const list = document.querySelector('.message-list');
  if (!list) return null;
  const l = list.getBoundingClientRect();
  const rows = Array.from(list.querySelectorAll('[data-nav-stop="row"]')).filter(el => !(el.parentElement && el.parentElement.closest('[data-nav-stop]')));
  const cur = rows.findIndex(r => r.classList.contains('row-current'));
  const rect = (el) => { const b = el.getBoundingClientRect(); return { top: b.top - l.top, bottom: b.bottom - l.top, h: b.height }; };
  const row = rows[cur];
  const body = row ? row.querySelector('.row-body') : null;
  const box = row ? row.querySelector('.tool-result-body') : null;
  return {
    cur, n: rows.length,
    top: list.scrollTop, max: list.scrollHeight - list.clientHeight, vh: list.clientHeight,
    sw: list.scrollWidth, cw: list.clientWidth,
    line: body ? parseFloat(getComputedStyle(body).lineHeight) : null,
    rows: rows.map(rect),
    box: box ? { top: box.scrollTop, max: box.scrollHeight - box.clientHeight, line: parseFloat(getComputedStyle(box).lineHeight) } : null,
  };
})())"#;

/// Every folded result preview in the list, as the page draws it.
const PREVIEWS_JS: &str = r#"JSON.stringify(Array.from(document.querySelectorAll('.message-list .tool-result-preview')).map(p => ({
  lines: Array.from(p.querySelectorAll('.tool-result-preview-line')).map(e => ({ text: e.textContent.slice(0, 20), clipped: e.scrollWidth > e.clientWidth + 1 })),
  more: (p.querySelector('.tool-result-preview-more') || {}).textContent || null,
  height: p.getBoundingClientRect().height,
})))"#;

/// Starts recording every scroll event on the list with its time and the cursor row at that moment.
const INSTALL_LOG_JS: &str = r#"JSON.stringify((() => {
  const list = document.querySelector('.message-list');
  if (window.__jkLog === undefined) {
    window.__jkLog = [];
    const rows = () => Array.from(list.querySelectorAll('[data-nav-stop="row"]')).filter(el => !(el.parentElement && el.parentElement.closest('[data-nav-stop]')));
    list.addEventListener('scroll', () => window.__jkLog.push({ t: performance.now(), top: list.scrollTop, cur: rows().findIndex(r => r.classList.contains('row-current')) }), { passive: true });
  }
  window.__jkLog.length = 0;
  return performance.now();
})())"#;

#[derive(Clone, Debug)]
struct RowRect {
    top: f64,
    bottom: f64,
    h: f64,
}

#[derive(Clone, Debug)]
struct BoxState {
    top: f64,
    max: f64,
    line: f64,
}

#[derive(Clone, Debug)]
struct St {
    cur: usize,
    top: f64,
    max: f64,
    vh: f64,
    sw: f64,
    cw: f64,
    line: f64,
    rows: Vec<RowRect>,
    boxed: Option<BoxState>,
}

fn num(v: &Value, key: &str) -> Result<f64, String> {
    v.get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("state has no number {key:?}: {v}"))
}

impl St {
    fn parse(v: &Value) -> Result<St, String> {
        let rows = v["rows"]
            .as_array()
            .ok_or("state has no rows")?
            .iter()
            .map(|r| {
                Ok(RowRect {
                    top: num(r, "top")?,
                    bottom: num(r, "bottom")?,
                    h: num(r, "h")?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let boxed = if v["box"].is_null() {
            None
        } else {
            Some(BoxState {
                top: num(&v["box"], "top")?,
                max: num(&v["box"], "max")?,
                line: num(&v["box"], "line")?,
            })
        };
        let cur = v["cur"]
            .as_i64()
            .filter(|c| *c >= 0)
            .ok_or("the page has no cursor row")? as usize;
        Ok(St {
            cur,
            top: num(v, "top")?,
            max: num(v, "max")?,
            vh: num(v, "vh")?,
            sw: num(v, "sw")?,
            cw: num(v, "cw")?,
            line: num(v, "line")?,
            rows,
            boxed,
        })
    }

    /// Whether nothing the walk watches has moved between two reads.
    fn same(&self, other: &St) -> bool {
        self.cur == other.cur
            && (self.top - other.top).abs() < 0.5
            && match (&self.boxed, &other.boxed) {
                (Some(a), Some(b)) => (a.top - b.top).abs() < 0.5,
                (None, None) => true,
                _ => false,
            }
    }

    /// Pixels of the cursor row on screen.
    fn visible(&self) -> f64 {
        let r = &self.rows[self.cur];
        (r.bottom.min(self.vh) - r.top.max(0.0)).max(0.0)
    }
}

fn read_state(h: &Harness) -> Result<St, String> {
    St::parse(&h.eval(STATE_JS, 3000)?)
}

/// The state after a press has settled: the animation (150ms) has run and nothing moves for two reads.
fn settled(h: &Harness) -> Result<St, String> {
    pump(190);
    let mut previous = read_state(h)?;
    for _ in 0..24 {
        pump(50);
        let now = read_state(h)?;
        if now.same(&previous) {
            return Ok(now);
        }
        previous = now;
    }
    Ok(previous)
}

// ---------------------------------------------------------------------------------------------
// Assertions.
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Report {
    failures: Vec<String>,
}

impl Report {
    fn check(&mut self, ok: bool, what: impl FnOnce() -> String) {
        if !ok {
            let message = what();
            eprintln!("[panel_jk_scroll] FAIL {message}");
            self.failures.push(message);
        }
    }
}

/// One settled press and what it did.
struct Press {
    key: char,
    before: St,
    after: St,
    /// The scroll events the page logged during it: (ms since the press, scrollTop, cursor row).
    scrolls: Vec<(f64, f64, i64)>,
}

impl Press {
    fn moved(&self) -> f64 {
        self.after.top - self.before.top
    }
    fn row_changed(&self) -> bool {
        self.after.cur != self.before.cur
    }
}

/// Presses `key` (`j` or `k`) and reads what the page did, with its scroll events.
fn press(h: &Harness, key: char) -> Result<Press, String> {
    let before = read_state(h)?;
    let t0 = h.eval(INSTALL_LOG_JS, 2000)?.as_f64().unwrap_or(0.0);
    h.key(&key.to_string())?;
    let after = settled(h)?;
    let log = h.eval(
        &format!(
            "JSON.stringify((window.__jkLog || []).filter(e => e.t >= {t0}).map(e => [e.t - {t0}, e.top, e.cur]))"
        ),
        2000,
    )?;
    let scrolls = log
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| Some((e[0].as_f64()?, e[1].as_f64()?, e[2].as_i64()?)))
                .collect()
        })
        .unwrap_or_default();
    Ok(Press {
        key,
        before,
        after,
        scrolls,
    })
}

/// What every single press must hold, whichever way it went.
fn check_press(rep: &mut Report, run: &str, n: usize, p: &Press) {
    let st = &p.after;
    let tag = || format!("{run} {}{n}", p.key);
    // The most a press may move the view: two thirds of it.
    let limit = st.vh * 2.0 / 3.0 + 2.0;
    rep.check(p.moved().abs() <= limit, || {
        format!(
            "{}: moved the view {:+.0}px of {:.0} (limit {:.0})",
            tag(),
            p.moved(),
            st.vh,
            limit
        )
    });
    // The cursor row keeps two lines on screen, or all of it when it is shorter than two lines.
    let r = &st.rows[st.cur];
    let want = (2.0 * st.line).min(r.h) - 1.0;
    rep.check(st.visible() >= want, || {
        format!(
            "{}: row {} shows {:.1}px, wanted {:.1}",
            tag(),
            st.cur,
            st.visible(),
            want
        )
    });
    // Nothing in the list scrolls sideways.
    rep.check(st.sw <= st.cw + 1.0, || {
        format!("{}: the list scrolls sideways ({} wide in {})", tag(), st.sw, st.cw)
    });
    if !p.row_changed() {
        return;
    }
    // A row move: where did the row land?
    let margin = 2.0 * st.line;
    let tall = r.h > st.vh - 2.0 * margin;
    let at_end = st.top <= 0.5 || st.top >= st.max - 0.5;
    let moved = p.moved().abs() > 1.0;
    if tall && p.key == 'j' {
        let frac = r.top / st.vh;
        rep.check(
            if moved {
                (0.28..=0.39).contains(&frac)
            } else {
                frac <= 0.39
            },
            || {
                format!(
                    "{}: tall row {} entered with its head at {:.2} of the view (want a third)",
                    tag(),
                    st.cur,
                    frac
                )
            },
        );
    } else if tall && p.key == 'k' {
        let frac = r.bottom / st.vh;
        rep.check(
            if moved {
                (0.61..=0.72).contains(&frac)
            } else {
                frac >= 0.61
            },
            || {
                format!(
                    "{}: tall row {} entered with its end at {:.2} of the view (want two thirds)",
                    tag(),
                    st.cur,
                    frac
                )
            },
        );
    } else if !at_end {
        rep.check(r.top >= margin - 3.0 && r.bottom <= st.vh - margin + 3.0, || {
            format!(
                "{}: row {} ({:.0}..{:.0}) is outside the {:.0}px margins of a {:.0}px view",
                tag(),
                st.cur,
                r.top,
                r.bottom,
                margin,
                st.vh
            )
        });
    }
    // A move of the view that crossed rows eased: more than one scroll event, never backwards, and the
    // cursor stayed on the row it moved to the whole way (the list's scroll listener did not pull it back).
    if moved && p.moved().abs() >= 150.0 {
        let tops: Vec<f64> = p.scrolls.iter().map(|s| s.1).collect();
        rep.check(tops.len() >= 3, || {
            format!(
                "{}: a {:+.0}px move took {} scroll events (an ease-out takes several)",
                tag(),
                p.moved(),
                tops.len()
            )
        });
        let direction = p.moved().signum();
        rep.check(tops.windows(2).all(|w| (w[1] - w[0]) * direction >= -1.0), || {
            format!("{}: the ease went backwards: {tops:?}", tag())
        });
        rep.check(p.scrolls.iter().all(|s| s.2 == p.after.cur as i64), || {
            format!(
                "{}: the cursor left row {} while the view eased: {:?}",
                tag(),
                p.after.cur,
                p.scrolls
            )
        });
    }
}

// ---------------------------------------------------------------------------------------------
// One configuration.
// ---------------------------------------------------------------------------------------------

struct Config {
    name: &'static str,
    size: (i32, i32),
    zoom: f64,
    /// Whether T1's result is unfolded (its 260px box) before the walks.
    expand_tool: bool,
}

/// What a run records for the cross-zoom comparison: the rows visited, in order, by `j` and by `k`.
struct RunSummary {
    j_rows: Vec<usize>,
    k_rows: Vec<usize>,
    worst_j: f64,
    worst_k: f64,
    min_visible_lines: f64,
}

/// Walks the conversation with `key` until two presses in a row change nothing (or `cap` presses).
fn walk(h: &Harness, rep: &mut Report, run: &str, key: char, cap: usize) -> Result<Vec<Press>, String> {
    let mut presses = Vec::new();
    let mut idle = 0;
    for n in 1..=cap {
        if n % 20 == 0 {
            h.focus()?;
        }
        let p = press(h, key)?;
        check_press(rep, run, n, &p);
        let still = !p.row_changed()
            && p.moved().abs() < 0.5
            && match (&p.before.boxed, &p.after.boxed) {
                (Some(a), Some(b)) => (a.top - b.top).abs() < 0.5,
                _ => true,
            };
        presses.push(p);
        idle = if still { idle + 1 } else { 0 };
        if idle >= 2 {
            break;
        }
    }
    Ok(presses)
}

fn run_config(display: &str, rep: &mut Report, cfg: &Config, primary: bool) -> Result<RunSummary, String> {
    let h = Harness::new(
        cfg.zoom,
        format!("jk-scroll {}", cfg.name),
        display,
        replay_fixture(),
        cfg.size,
    )?;
    h.focus()?;
    h.key("g")?;
    h.key("g")?;
    pump(200);

    // The page is laid out and BROWSE holds the keys on the first row.
    let start = read_state(&h)?;
    rep.check(start.n_rows() == ROWS, || {
        format!("{}: the fixture drew {} rows, not {ROWS}", cfg.name, start.n_rows())
    });
    rep.check(start.cur == S1, || {
        format!("{}: gg left the cursor on row {}", cfg.name, start.cur)
    });

    // The folded result's preview (collapsed runs): three lines and a count; a runaway line is clipped and
    // the list does not scroll sideways.
    if !cfg.expand_tool {
        let previews = h.eval(PREVIEWS_JS, 3000)?;
        let previews = previews.as_array().cloned().unwrap_or_default();
        rep.check(previews.len() == 2, || {
            format!(
                "{}: {} previews drawn, wanted one per folded Bash",
                cfg.name,
                previews.len()
            )
        });
        if previews.len() == 2 {
            let first = &previews[0];
            rep.check(first["lines"].as_array().map(Vec::len) == Some(3), || {
                format!(
                    "{}: T1's preview shows {} lines",
                    cfg.name,
                    first["lines"].as_array().map_or(0, Vec::len)
                )
            });
            let more = format!("… +{} lines (Enter to expand, Ctrl+o for all)", TOOL_LINES - 3);
            rep.check(first["more"].as_str() == Some(more.as_str()), || {
                format!(
                    "{}: T1's preview count reads {:?}, wanted {more:?}",
                    cfg.name, first["more"]
                )
            });
            let wide = &previews[1];
            rep.check(wide["lines"][0]["clipped"].as_bool() == Some(true), || {
                format!(
                    "{}: the 700-character first line was not clipped: {}",
                    cfg.name, wide["lines"][0]
                )
            });
            rep.check(
                wide["lines"].as_array().map(Vec::len) == Some(3) && wide["more"].as_str().is_some(),
                || format!("{}: T2's preview is {wide}", cfg.name),
            );
        }
        rep.check(start.sw <= start.cw + 1.0, || {
            format!(
                "{}: a 700-character preview line widened the list ({} in {})",
                cfg.name, start.sw, start.cw
            )
        });
    }
    if cfg.expand_tool {
        h.key("j")?;
        pump(120);
        h.key("Return")?;
        pump(250);
        let s = read_state(&h)?;
        if s.boxed.is_none() {
            return Err(format!(
                "{}: the tool row has no .tool-result-body after Enter",
                cfg.name
            ));
        }
        h.key("g")?;
        h.key("g")?;
        pump(250);
    }

    // Down: every row, one press at a time.
    let down = walk(&h, rep, cfg.name, 'j', 260)?;
    rep.check(down.last().map(|p| p.after.cur) == Some(ROWS - 1), || {
        format!(
            "{}: the j walk ended on row {:?}",
            cfg.name,
            down.last().map(|p| p.after.cur)
        )
    });
    // Up: from the last row, back to the first.
    h.key("G")?;
    pump(300);
    let up = walk(&h, rep, cfg.name, 'k', 260)?;
    rep.check(up.last().map(|p| p.after.cur) == Some(S1), || {
        format!(
            "{}: the k walk ended on row {:?}",
            cfg.name,
            up.last().map(|p| p.after.cur)
        )
    });

    // A tool result's box: entered at its start going down, at its end going up, three of its own lines a
    // press.
    if cfg.expand_tool {
        let entering_down = down.iter().find(|p| p.before.cur != T1 && p.after.cur == T1);
        rep.check(
            entering_down.and_then(|p| p.after.boxed.as_ref()).map(|b| b.top) == Some(0.0),
            || {
                format!(
                    "{}: j into the tool row left its box at {:?}",
                    cfg.name,
                    entering_down.and_then(|p| p.after.boxed.clone())
                )
            },
        );
        let stepped: Vec<(f64, &BoxState)> = down
            .iter()
            .filter(|p| p.before.cur == T1 && p.after.cur == T1)
            .filter_map(|p| {
                Some((
                    p.after.boxed.as_ref()?.top - p.before.boxed.as_ref()?.top,
                    p.after.boxed.as_ref()?,
                ))
            })
            .filter(|(d, b)| *d > 0.0 && b.top < b.max - 0.5)
            .collect();
        rep.check(!stepped.is_empty(), || {
            format!("{}: no j press stepped the box", cfg.name)
        });
        for (d, b) in &stepped {
            rep.check((d - 3.0 * b.line).abs() <= 2.5, || {
                format!(
                    "{}: a box step moved {d:.1}px, three lines of {:.2} is {:.1}",
                    cfg.name,
                    b.line,
                    3.0 * b.line
                )
            });
        }
        let entering_up = up.iter().find(|p| p.before.cur == L1 && p.after.cur == T1);
        let ok = entering_up
            .and_then(|p| p.after.boxed.as_ref())
            .map(|b| (b.top - b.max).abs() <= 1.0)
            .unwrap_or(false);
        rep.check(ok, || {
            format!(
                "{}: k into the tool row left its box at {:?}",
                cfg.name,
                entering_up.and_then(|p| p.after.boxed.clone())
            )
        });
    }

    // Counts: five steps inside a tall row, not one.
    if primary {
        h.key("g")?;
        h.key("g")?;
        pump(200);
        let mut guard = 0;
        while read_state(&h)?.cur != L1 && guard < 10 {
            h.key("j")?;
            settled(&h)?;
            guard += 1;
        }
        h.key("j")?;
        settled(&h)?;
        h.key("j")?;
        let before = settled(&h)?;
        rep.check(before.cur == L1, || {
            format!("{}: the count test started on row {}", cfg.name, before.cur)
        });
        // Down nine steps, back up five (the row's own top margin line is more than four steps above where
        // nine took the view), down twelve: all inside L1, which has the travel for it.
        for (digits, key, sign) in [("9", 'j', 1.0), ("5", 'k', -1.0), ("12", 'j', 1.0)] {
            let before = read_state(&h)?;
            for d in digits.chars() {
                h.key(&d.to_string())?;
            }
            h.key(&key.to_string())?;
            let after = settled(&h)?;
            let n: f64 = digits.parse().unwrap_or(1.0);
            let step = 3.0 * after.line;
            let moved = after.top - before.top;
            rep.check(
                after.cur == L1 && (moved - sign * n * step).abs() <= n * 1.0 + 2.0,
                || {
                    format!(
                        "{}: {digits}{key} in a tall row moved {:+.1}px (wanted {:+.1}) on row {}",
                        cfg.name,
                        moved,
                        sign * n * step,
                        after.cur
                    )
                },
            );
        }

        // A press during the ease starts from that ease's target: two quick presses end where two slow ones do.
        h.key("g")?;
        h.key("g")?;
        pump(200);
        let mut guard = 0;
        while read_state(&h)?.cur != 3 && guard < 80 {
            h.key("j")?;
            settled(&h)?;
            guard += 1;
        }
        let at_s2 = read_state(&h)?;
        h.keys_with_delay(25, &["j", "j"])?;
        let fast = settled(&h)?;
        h.key("g")?;
        h.key("g")?;
        pump(200);
        let mut guard = 0;
        while read_state(&h)?.cur != 3 && guard < 80 {
            h.key("j")?;
            settled(&h)?;
            guard += 1;
        }
        let again = read_state(&h)?;
        rep.check((again.top - at_s2.top).abs() <= 1.0, || {
            format!(
                "{}: the walk to row 3 is not repeatable ({:.0} then {:.0})",
                cfg.name, at_s2.top, again.top
            )
        });
        h.key("j")?;
        settled(&h)?;
        h.key("j")?;
        let slow = settled(&h)?;
        rep.check(fast.cur == slow.cur && (fast.top - slow.top).abs() <= 1.0, || {
            format!(
                "{}: two quick presses ended on row {} at {:.0}, two slow ones on row {} at {:.0}",
                cfg.name, fast.cur, fast.top, slow.cur, slow.top
            )
        });
    }

    let worst = |presses: &[Press]| {
        presses
            .iter()
            .map(|p| p.moved().abs() / p.after.vh)
            .fold(0.0_f64, f64::max)
    };
    let min_visible = down
        .iter()
        .chain(up.iter())
        .map(|p| p.after.visible() / p.after.line)
        .fold(f64::MAX, f64::min);
    let summary = RunSummary {
        j_rows: row_visits(&down),
        k_rows: row_visits(&up),
        worst_j: worst(&down),
        worst_k: worst(&up),
        min_visible_lines: min_visible,
    };
    println!(
        "JKS {} view={:.0}px j: {} presses worst {:.2} of the view, k: {} presses worst {:.2}, fewest lines of the cursor row on screen {:.1}",
        cfg.name,
        down.first().map_or(0.0, |p| p.after.vh),
        down.len(),
        summary.worst_j,
        up.len(),
        summary.worst_k,
        summary.min_visible_lines
    );
    h.close();
    Ok(summary)
}

impl St {
    fn n_rows(&self) -> usize {
        self.rows.len()
    }
}

/// The rows the cursor visited, in order, each once.
fn row_visits(presses: &[Press]) -> Vec<usize> {
    let mut visited = Vec::new();
    for p in presses {
        if visited.last() != Some(&p.after.cur) {
            visited.push(p.after.cur);
        }
    }
    visited
}

fn main() {
    if let Err(e) = panel_base_uri_matches_product() {
        eprintln!("panel_jk_scroll: {e}");
        std::process::exit(1);
    }
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("panel_jk_scroll: ignored (drives a real WebKitGTK WebView); run with `-- --ignored`");
        return;
    }
    // This test sends real keys: it starts a display of its own and nothing below reads the inherited one.
    // GTK's own built-in input method, never the one the desktop session names; `init_gtk` sets both and
    // refuses unless GDK opened this server's display.
    let server = own_x_server::init_gtk("panel_jk_scroll", "1400x1700x24");
    if Command::new("xdotool").arg("version").output().is_err() {
        eprintln!("panel_jk_scroll: `xdotool` is not on PATH -- install it (this test sends every key through it)");
        server.exit(1);
    }
    let code = run(server.display());
    server.exit(code);
}

fn run(display: &str) -> i32 {
    let only = std::env::var("JKS_RUNS").unwrap_or_default();
    let want = |name: &str| only.is_empty() || only.split(',').any(|n| n == name);
    let configs = [
        // The window sizes and zoom the 2026-10-01 measurement used: 900px and 600px views at zoom 1.0, and
        // WebKit zoom 1.5 (a 144 dpi session), whose list is a fractional height in CSS px.
        Config {
            name: "collapsed-900",
            size: (600, 965),
            zoom: 1.0,
            expand_tool: false,
        },
        Config {
            name: "expanded-900",
            size: (600, 965),
            zoom: 1.0,
            expand_tool: true,
        },
        Config {
            name: "collapsed-600",
            size: (600, 665),
            zoom: 1.0,
            expand_tool: false,
        },
        Config {
            name: "collapsed-zoom15",
            size: (900, 1452),
            zoom: 1.5,
            expand_tool: false,
        },
    ];
    let mut rep = Report::default();
    let mut summaries: Vec<(&str, RunSummary)> = Vec::new();
    for (i, cfg) in configs.iter().enumerate() {
        if !want(cfg.name) {
            continue;
        }
        match run_config(display, &mut rep, cfg, i == 0) {
            Ok(summary) => summaries.push((cfg.name, summary)),
            Err(e) => {
                eprintln!("[panel_jk_scroll] {} FAILED: {e}", cfg.name);
                rep.failures.push(format!("{}: {e}", cfg.name));
            }
        }
    }
    // The same press sequence at zoom 1.0 and 1.5 visits the same rows in the same order.
    let find = |name: &str| summaries.iter().find(|(n, _)| *n == name).map(|(_, s)| s);
    if let (Some(a), Some(b)) = (find("collapsed-900"), find("collapsed-zoom15")) {
        rep.check(a.j_rows == b.j_rows, || {
            format!("j visits rows {:?} at zoom 1.0 but {:?} at 1.5", a.j_rows, b.j_rows)
        });
        rep.check(a.k_rows == b.k_rows, || {
            format!("k visits rows {:?} at zoom 1.0 but {:?} at 1.5", a.k_rows, b.k_rows)
        });
        rep.check(
            (a.worst_j - b.worst_j).abs() <= 0.1 && (a.worst_k - b.worst_k).abs() <= 0.1,
            || {
                format!(
                    "the worst move differs between zoom 1.0 ({:.2}/{:.2}) and 1.5 ({:.2}/{:.2})",
                    a.worst_j, a.worst_k, b.worst_j, b.worst_k
                )
            },
        );
    }
    if rep.failures.is_empty() {
        println!("panel_jk_scroll: ok ({} runs)", summaries.len());
        0
    } else {
        eprintln!("panel_jk_scroll: {} failures", rep.failures.len());
        for f in &rep.failures {
            eprintln!("  - {f}");
        }
        1
    }
}
