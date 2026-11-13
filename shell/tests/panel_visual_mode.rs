//! The fixture drives JavaScript time at rendered-frame boundaries. Reported JavaScript times describe that
//! controlled cadence; they are not measurements of the host's rendering speed.
//!
//! Tests BROWSE's CARET, VISUAL, and V-LINE modes against the real WebKitGTK engine linked
//! by `shell`. jsdom lacks `Selection.modify`, so these checks require a real WebView.
//! Compile with `cargo test -p shell --test panel_visual_mode --no-run`. Run with:
//!
//!     cargo test -p shell --test panel_visual_mode -- --ignored
//!
//! The harness starts its own X server through `support/own_x_server.rs`, discards inherited
//! `DISPLAY` and `WAYLAND_DISPLAY`, verifies GDK opened that display, and passes it explicitly
//! to every `xdotool` invocation. It stops the server using the captured pid. An outer
//! `xvfb-run` display is ignored. Checking `GDK_BACKEND=x11` alone would also accept XWayland
//! on the desktop, where test input and clipboard writes could affect the running session.
//! Disabling Unix socket listening also prevents a server in a separate network namespace
//! from unlinking the desktop's `/tmp/.X11-unix/X0` socket.
//!
//! No window manager runs on this server. `xdotool windowactivate` requires `_NET_ACTIVE_WINDOW`
//! and would abort chained keys. `Harness::focus` instead uses `windowfocus` and `XSetInputFocus`,
//! then checks `getwindowfocus`. Keys, text, and clicks omit a window argument so libxdo uses
//! XTEST to send trusted events to the focused window. GTK4's XInput2 path need not deliver
//! synthetic `XSendEvent` input. W0 verifies that a trusted `KeyboardEvent` reaches the page;
//! failure marks every case in that window as a harness failure.
//! A plain `main` (`harness = false`) keeps GTK on the main thread.
//!
//! The document loads with `PANEL_BASE_URI`. An opaque origin from `load_html(.., None)` has
//! no `navigator.clipboard` in WebKitGTK. The constant is duplicated because `shell` has no
//! `[lib]` target through which an integration test can import it; `panel_base_uri_matches_product`
//! checks it against the product source. Copies are read back from GDK's `Clipboard`, so a flash
//! message or wrapped `writeText` cannot stand in for an actual copy. Real XTEST keys provide
//! trusted input; script-dispatched keys may lack the activation needed by `writeText`.
//! `Harness::posted` and `posted_type` capture messages, including any unexpected
//! `permission_response`. `clear_clipboard` and `read_fresh_clipboard` use a sentinel so a failed
//! copy cannot pass by returning stale text.
//!
//! Each replay has one window at each zoom, 1.0 and 1.5. The main replay contains an English
//! prompt, a reply with a Chinese paragraph, a fenced code block and a table, a waiting `Edit`
//! card, and a `Bash` call with output exceeding the 260px fold. The card precedes the tool row
//! so selection can cross its controls. The fixtures replay has separate code-only, table-only,
//! and 300-paragraph replies. A plain `v` or `V` starts at a block's or table's first character.
//! From BROWSE, `v` starts CARET with one character selected. A second `v` starts VISUAL.
//! `V` starts V-LINE directly.
//!
//! W0 checks trusted key delivery in both windows. W1 checks that `Selection.modify` and
//! `caretRangeFromPoint` exist in the non-editable page.
//! W2 checks `vvey`, `vvwey`, `vv2ey`, and `vvwwy` on the English prompt with inclusive selection.
//! The copied word from the Chinese paragraph is logged without an exact boundary assertion.
//! Word motions `w`, `e`, and `b` follow vim's word rules; the Chinese result has not been
//! checked again since that change.
//! W3 checks that `Vjy` copies both prompt and reply without the prompt marker or button labels.
//! Entering through CARET with `vVjy` must copy the same text.
//! W4 checks exact code text: `vv$y` copies one line and `Vjy` copies two with one newline and
//! no highlight.js residue. It checks both the code-only reply and a code block reached by HINT.
//! The harness sends `hint_collect`, reads `hint_targets`, and sends `hint_land {index}`,
//! trying indexes and checking the copied text to find the block. This needs no GTK-side
//! `HintCoordinator`.
//! W5 checks `Vjjy` across the table-only reply, with tab-separated cells and newline-separated
//! rows. The exact string is printed and pinned in `W5_PINNED`. WebKitGTK 2.52.6's
//! `Selection.modify(..., "line")` cannot cross a table row boundary. `visual.ts` uses
//! `stepByLinePoint` to probe at the goal column until `caretRangeFromPoint` lands beyond the
//! starting rect, allowing `j` and `k` to cross table rows and structured permission cards.
//! W6 searches for the tool-output row and presses Enter to render the folded result. After
//! `V` and 40 `j` steps, the caret must remain within the visible tool box and list after every
//! key. The copy includes the invocation through "build line 040", excluding line 041.
//! W7 searches for the waiting `Edit` card, fills its reason box, and uses `V` and five `j`
//! steps to cross the diff and controls into the next row. The copy includes the heading,
//! filename, and both diff lines. It excludes button labels, reason text and placeholder, and
//! diff gutter signs. `stepByLinePoint` makes these steps cross the structured content at both
//! zooms. In CARET and VISUAL, `d`, `a`, and `Shift+D` must post no `permission_response`.
//! A real click in the reason box returns to BROWSE, where Enter denies. Click coordinates use
//! the page's measured CSS-to-X pixel ratio; at zoom 1.5 one CSS pixel is 1.5 widget pixels.
//! W8 checks that `v v l Esc` returns to CARET with one character selected at the moving end.
//! A second Esc clears the selection and shows BROWSE; `y` then copies the row.
//! W9 uses `G v l v e e` in a reply streamed through 40 `events` envelopes about 33ms apart.
//! The anchor node and reply DOM node must stay identical after each delta. Copying must yield
//! text, and leaving selection must preserve the reply built by all deltas.
//! W10 compares `vv3w` with `vvwww`. On the long transcript it times `9999l`, `9999j`, and
//! copying the resulting large selection from keydown to keyup in the page. Timings are printed
//! with only a 60s hang guard. Geometry-based stepping reaches the actual transcript end where
//! repeated `Selection.modify(..., "line")` steps could stop early.
//! W11 checks one-character CARET selections after `l`, `w`, `$`, `j` across a row boundary,
//! `gg`, `G`, and `3w`. The list carries `data-visual="caret"`. Esc puts `.row-current` on the
//! caret's row and returns the band to BROWSE.
//! W12 checks that `v v w e >` puts `> The quick\n\n` in the focused composer, places its caret
//! at the end, posts one `draft`, and sends nothing. A second quote over two paragraphs preserves
//! the intervening blank line and appends a quoted region. Existing `hello` text is followed by
//! `\n\n` before the quote. Extending a reply selection backward with `gg` moves `.row-current`
//! to the prompt row at the selection's start.
//! W13 checks a table row over 200px tall due to a wrapping neighbouring cell. When the pixel
//! probe exhausts its budget, `j` uses DOM position to reach the next row from a short cell.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};

use agent::{AgentDomainEvent, AgentSessionProjection, ContentKind, TurnOutcome};
use eitri_core::agent_backend::{BackendGreeting, BackendKind, ProjectionRef, CLIENT_IMPLEMENTED_PERMISSION_MODES};
use eitri_core::agent_bridge::{
    parse_inbound_message, serialize_command_result_for_js, serialize_events_for_js, serialize_focus_permission_for_js,
    serialize_hello_for_js, serialize_hint_collect_for_js, serialize_hint_land_for_js, serialize_pane_focus_for_js,
    serialize_snapshot_for_js, serialize_tabs_for_js, serialize_theme_for_js, InboundMessage, SessionModeChoice,
    SnapshotView, TabStateWire, TabView,
};
use eitri_core::theme::ThemeTokens;
use gtk4::glib;
use gtk4::prelude::*;
use serde_json::json;
use webkit6::prelude::*;
use webkit6::{UserContentManager, WebView};

// The display this test runs on: its own Xvfb, never an inherited one (shared since 2026-09-29).
#[path = "support/own_x_server.rs"]
mod own_x_server;

/// `panel/src/panel_document.rs`'s own `const PANEL_BASE_URI`, duplicated:
/// this crate has no `[lib]` target an integration test can reach a private item through, the same
/// reason `panel_stream_scroll.rs` duplicates `themed_document` rather than importing it.
/// `panel_base_uri_matches_product` (below) pins the two equal.
const PANEL_BASE_URI: &str = "https://eitri.invalid/";

/// `Harness::clear_clipboard`'s own marker: a string no real copy in this replay could ever produce.
const CLIPBOARD_SENTINEL: &str = "\u{2039}panel_visual_mode: cleared before this case\u{203a}";

/// W5's exact `toString()` of the table (pinned on first run), as
/// WebKitGTK 2.52.6 printed it at both zooms once V-LINE took whole table rows (before
/// that the last row stopped at its first cell, "3").
const W5_PINNED: Option<&str> = Some("a\tb\n1\t2\n3\t4");

/// The document `shell/src/agent_panel.rs` embeds, byte for byte.
const AGENT_UI_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../agent-ui/web/dist/index.html"));

/// `PanelDocument::themed`'s exact insertion rule (duplicated the same way
/// `panel_stream_scroll.rs` does): the theme `<style>` goes directly after the first `<head>`.
fn themed_document(vars: &[(String, String)]) -> String {
    let declarations: String = vars.iter().map(|(name, value)| format!("{name}:{value};")).collect();
    let style = format!("<style id=\"nv-theme\">:root{{{declarations}}}</style>");
    let at = AGENT_UI_HTML.find("<head>").expect("the panel document has a <head>") + "<head>".len();
    format!("{}{}{}", &AGENT_UI_HTML[..at], style, &AGENT_UI_HTML[at..])
}

/// Needs no display: reads `panel_document.rs`'s own source as text and checks the literal this file
/// duplicated is still the same one. Runs every time, `--ignored` or not.
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

/// The Chinese paragraph used by W2. Word motions follow vim's word rules,
/// but the copied text is only logged until the harness has run again.
const CHINESE_PARAGRAPH: &str = "这是一段中文，用来记录 ICU 分词的实际结果。";

const CODE_BLOCK_LANG: &str = "rust";
const CODE_LINE_1: &str = "fn add(a: i32, b: i32) -> i32 {";
const CODE_LINE_2: &str = "    a + b";
const CODE_LINE_3: &str = "}";

/// W5's table: 2x2 body under a header, cells short enough that a failure message is easy to read.
const TABLE_MARKDOWN: &str = "| a | b |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |\n";

/// W13's table: `target`'s own cell never wraps, but
/// its row-mate (250 space-separated words) wraps across many visual lines in a 900px window, making
/// the ROW itself taller than `stepByLinePoint`'s `MAX_LINE_PROBE_PX` (200px, `visual.ts`) --
/// exactly the shape that left `j` stuck on `target` unable to reach `next` below it. Measured for
/// real (`w13_tall_table_row`'s own first check): 40 words only reached 50px at zoom 1 and 101px at
/// zoom 1.5 -- comfortably short of 200px -- so 250 is not a round number, it is what cleared both
/// with margin once actually rendered.
fn tall_row_table_markdown() -> String {
    let wrapped_cell = (1..=250).map(|_| "wrap").collect::<Vec<_>>().join(" ");
    format!("| short | long |\n| --- | --- |\n| target | {wrapped_cell} |\n| next | y |\n")
}

/// Paragraphs in the fixtures' long reply (W10).
const LONG_PARAGRAPHS: usize = 300;

fn code_block_markdown() -> String {
    format!("```{CODE_BLOCK_LANG}\n{CODE_LINE_1}\n{CODE_LINE_2}\n{CODE_LINE_3}\n```")
}

/// W6's tool output: comfortably taller than `ToolResult`'s 260px fold at either zoom point tested.
fn tall_tool_output() -> String {
    (1..=80)
        .map(|n| format!("build line {n:03}: ok"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The main reply: the Chinese paragraph, the code block, the table, in that order.
fn reply_markdown() -> String {
    format!("{CHINESE_PARAGRAPH}\n\n{}\n\n{TABLE_MARKDOWN}", code_block_markdown())
}

fn long_markdown() -> String {
    (1..=LONG_PARAGRAPHS)
        .map(|n| format!("para {n:03}: the quick brown fox jumps over the lazy dog"))
        .collect::<Vec<_>>()
        .join("\n\n")
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

#[path = "support/web_clock.rs"]
mod web_clock;

/// Advances the page clock at acknowledged rendered frames.
fn pump(ms: u64) {
    web_clock::pump(ms);
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
    /// Every raw message the page posted, in arrival order.
    posted: Rc<RefCell<Vec<String>>>,
    /// Increments per HINT this harness starts (W4's HINT half), so each `hint_collect` is a new
    /// session the page does not confuse with an older one.
    hint_session: Cell<u64>,
}

impl Harness {
    fn new(zoom: f64, title: String, display: &str, replay: Vec<String>) -> Result<Self, String> {
        let window = gtk4::Window::new();
        window.set_default_size(900, 700);
        window.set_title(Some(&title));

        let content_manager = UserContentManager::new();
        let clock_turns = web_clock::install(&content_manager);
        let webview = WebView::builder().user_content_manager(&content_manager).build();
        web_clock::watch(&webview, clock_turns);
        webview.set_hexpand(true);
        webview.set_vexpand(true);
        webview.set_zoom_level(zoom);
        window.set_child(Some(&webview));

        let queue: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(replay));
        let started = Rc::new(Cell::new(false));
        let posted: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        content_manager.register_script_message_handler("eitriAgent", None);
        {
            let queue = queue.clone();
            let started = started.clone();
            let posted = posted.clone();
            let webview_weak = webview.downgrade();
            content_manager.connect_script_message_received(Some("eitriAgent"), move |_manager, js_value| {
                let Some(webview) = webview_weak.upgrade() else { return };
                let raw = js_value.to_str().to_string();
                posted.borrow_mut().push(raw.clone());
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
            posted,
            hint_session: Cell::new(9000),
        };
        harness.focus()?;
        // The replay is dispatched on the page's own `ready`, which a debug build under load can take
        // well past the 400ms above to post; nothing below is meaningful before the conversation
        // (and its root, which takes the keys) exists.
        let end = Instant::now() + Duration::from_secs(15);
        loop {
            let ready = harness.eval(
                "JSON.stringify(document.querySelector('.agent-ui-conversation .message-list') !== null)",
                1000,
            );
            if matches!(ready, Ok(serde_json::Value::Bool(true))) {
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
    /// `getwindowfocus` -- every key, text and click below goes to the focused window through XTEST.
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

    /// One keystroke (an xdotool keysym chord, e.g. `v`, `shift+v`, `dollar`, `Escape`) via XTEST.
    fn xdotool_key(&self, keys: &str) {
        if let Err(e) = self.focus() {
            eprintln!("[panel_visual_mode] {e}");
        }
        if let Err(e) = xdotool(&self.display, &["key", "--clearmodifiers", keys]) {
            eprintln!("[panel_visual_mode] {e}");
        }
        pump(60);
    }

    fn xdotool_keys(&self, sequence: &[&str]) {
        for keys in sequence {
            self.xdotool_key(keys);
        }
    }

    /// Literal text into whatever holds the caret (the search bar's input, W6's route to a row).
    fn xdotool_type(&self, text: &str) {
        if let Err(e) = self.focus() {
            eprintln!("[panel_visual_mode] {e}");
        }
        if let Err(e) = xdotool(&self.display, &["type", text]) {
            eprintln!("[panel_visual_mode] {e}");
        }
        pump(60);
    }

    /// A real left click at a point given in the page's own CSS px (`getBoundingClientRect`).
    ///
    /// CSS px are not X px. `set_zoom_level(1.5)` (and WebKit's own
    /// `gtk-xft-dpi` zoom, `shell/src/webkit_zoom.rs`) make one CSS px several widget px, so the
    /// first version's straight copy clicked beside the reason box at zoom 1.5. The ratio is
    /// measured, not assumed: the WebView's width in widget px over the page's `innerWidth` in CSS
    /// px. Then the WebView's origin inside the window, the window's offset inside its X surface
    /// (`surface_transform`, non-zero only with client-side decorations), and GDK's integer scale.
    fn click_css(&self, css_x: f64, css_y: f64) -> Result<(), String> {
        let inner_width = self
            .eval("JSON.stringify(window.innerWidth)", 1000)?
            .as_f64()
            .filter(|w| *w > 0.0)
            .ok_or("the page reported no innerWidth")?;
        let ratio = f64::from(self.webview.width()) / inner_width;
        let origin = self
            .webview
            .compute_point(&self.window, &gtk4::graphene::Point::new(0.0, 0.0))
            .ok_or("the WebView has no position inside its window")?;
        let (dx, dy) = self.window.surface_transform();
        let scale = f64::from(self.window.scale_factor());
        let x = ((f64::from(origin.x()) + dx + css_x * ratio) * scale).round() as i64;
        let y = ((f64::from(origin.y()) + dy + css_y * ratio) * scale).round() as i64;
        println!("[panel_visual_mode] click: css ({css_x:.1}, {css_y:.1}) x ratio {ratio:.3} -> window px ({x}, {y})");
        self.focus()?;
        xdotool(
            &self.display,
            &[
                "mousemove",
                "--window",
                &self.xid,
                "--sync",
                &x.to_string(),
                &y.to_string(),
            ],
        )?;
        xdotool(&self.display, &["click", "1"])?;
        pump(80);
        Ok(())
    }

    /// Evaluates `script` and returns its JSON-decoded result (or an error string), pumping the
    /// main loop until the callback lands or `timeout_ms` elapses.
    fn eval(&self, script: &str, timeout_ms: u64) -> Result<serde_json::Value, String> {
        let outcome: Rc<RefCell<Option<Result<serde_json::Value, String>>>> = Rc::new(RefCell::new(None));
        {
            let outcome = outcome.clone();
            self.webview
                .evaluate_javascript(script, None, None, None::<&gtk4::gio::Cancellable>, move |r| {
                    let parsed = r.map_err(|e| format!("evaluate_javascript failed: {e}")).and_then(|v| {
                        serde_json::from_str::<serde_json::Value>(&v.to_str()).map_err(|e| format!("result JSON: {e}"))
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

    /// The band's `data-mode` (`browse`/`visual`/`vline`/...), or `null` when the block is not drawn.
    fn mode(&self) -> Result<serde_json::Value, String> {
        self.eval(
            "JSON.stringify(document.querySelector('[data-testid=\"mode-block\"]')?.getAttribute('data-mode') ?? null)",
            1000,
        )
    }

    /// Records every keydown/keyup the page sees, in the window's capture phase (ahead of the panel's
    /// own handlers), with `performance.now()` and `isTrusted`. Clears the log each call.
    fn start_key_log(&self) -> Result<(), String> {
        self.eval(
            "(() => { if (!window.__nvKeyLog) { window.__nvKeyLog = []; \
             const rec = (e) => window.__nvKeyLog.push({ type: e.type, key: e.key, t: performance.now(), trusted: e.isTrusted, keyCode: e.keyCode, composing: e.isComposing, target: e.target && e.target.tagName }); \
             window.addEventListener('keydown', rec, true); window.addEventListener('keyup', rec, true); } \
             window.__nvKeyLog.length = 0; return JSON.stringify(true); })()",
            1000,
        )
        .map(|_| ())
    }

    /// Waits (pumping, up to `timeout_ms`) until the key log holds a `keyup` for `key` after its last
    /// `keydown`, and returns the milliseconds between the two -- the page's whole handling of that
    /// keydown, since a keyup is dispatched only once the keydown's handlers have returned.
    fn keydown_to_keyup_ms(&self, key: &str, timeout_ms: u64) -> Result<f64, String> {
        let script = format!(
            "(() => {{ const log = window.__nvKeyLog || []; let down = -1; \
             for (let i = log.length - 1; i >= 0; i--) {{ if (log[i].type === 'keydown' && log[i].key === {k}) {{ down = i; break; }} }} \
             if (down < 0) return JSON.stringify(null); \
             for (let i = down + 1; i < log.length; i++) {{ if (log[i].type === 'keyup' && log[i].key === {k}) return JSON.stringify(log[i].t - log[down].t); }} \
             return JSON.stringify(null); }})()",
            k = serde_json::to_string(key).unwrap_or_default()
        );
        let end = Instant::now() + Duration::from_millis(timeout_ms);
        while Instant::now() < end {
            if let Some(ms) = self.eval(&script, timeout_ms)?.as_f64() {
                return Ok(ms);
            }
            pump(20);
        }
        Err(format!("no keyup for {key:?} within {timeout_ms} ms"))
    }

    /// Writes an unmistakable sentinel to OUR display's clipboard, so a `y` that silently copies
    /// nothing reads back as nothing rather than as a previous case's success.
    fn clear_clipboard(&self) {
        if let Some(display) = gtk4::gdk::Display::default() {
            display.clipboard().set_text(CLIPBOARD_SENTINEL);
        }
        pump(30);
    }

    /// Reads the GDK clipboard's own text back: never the page's own
    /// `writeText` wrapper, never the flash.
    fn read_clipboard(&self, timeout_ms: u64) -> Option<String> {
        let display = gtk4::gdk::Display::default()?;
        let clipboard = display.clipboard();
        let outcome: Rc<RefCell<Option<Option<String>>>> = Rc::new(RefCell::new(None));
        {
            let outcome = outcome.clone();
            clipboard.read_text_async(None::<&gtk4::gio::Cancellable>, move |r| {
                let text = r.ok().flatten().map(|s| s.to_string());
                outcome.borrow_mut().get_or_insert(text);
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
        let result = outcome.borrow_mut().take().flatten();
        result
    }

    /// `read_clipboard`, but the sentinel `clear_clipboard` left behind reads as `None`.
    fn read_fresh_clipboard(&self, timeout_ms: u64) -> Option<String> {
        self.read_clipboard(timeout_ms).filter(|s| s != CLIPBOARD_SENTINEL)
    }

    fn clear_posted(&self) {
        self.posted.borrow_mut().clear();
    }

    /// The first message posted since the last `clear_posted` whose JSON `"type"` is `message_type`.
    fn posted_type(&self, message_type: &str) -> Option<serde_json::Value> {
        self.posted.borrow().iter().find_map(|raw| {
            let value: serde_json::Value = serde_json::from_str(raw).ok()?;
            if value.get("type").and_then(|t| t.as_str()) == Some(message_type) {
                Some(value)
            } else {
                None
            }
        })
    }

    /// Starts a HINT the way `shell` does and returns (session id, how many targets the page froze).
    fn hint_collect(&self) -> Result<(u64, usize), String> {
        let session = self.hint_session.get() + 1;
        self.hint_session.set(session);
        self.clear_posted();
        evaluate_js_dispatch(&self.webview, &serialize_hint_collect_for_js(session));
        let end = Instant::now() + Duration::from_millis(2000);
        while Instant::now() < end {
            pump(20);
            if let Some(reply) = self.posted_type("hint_targets") {
                let count = reply
                    .get("count")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or("hint_targets carried no count")?;
                return Ok((session, count as usize));
            }
        }
        Err("the page never answered hint_collect with hint_targets".into())
    }

    fn hint_land(&self, session: u64, index: usize) {
        evaluate_js_dispatch(&self.webview, &serialize_hint_land_for_js(session, index));
        pump(120);
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
            eprintln!("[panel_visual_mode] evaluate_javascript failed: {e}");
        }
    });
}

// ---------------------------------------------------------------------------------------------
// The two replays.
// ---------------------------------------------------------------------------------------------

fn opened_projection() -> AgentSessionProjection {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::SessionOpened {
        session_id: "visual-mode-test".into(),
        provider_session_id: "claude-visual-mode-test".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/nonexistent/panel-visual-mode".into(),
    });
    projection
}

/// One finished turn: the prompt, then `reply` as a single assistant text.
fn finished_turn(turn_id: &str, prompt: &str, reply: String) -> Vec<AgentDomainEvent> {
    vec![
        AgentDomainEvent::UserPromptSubmitted { text: prompt.into() },
        AgentDomainEvent::TurnStarted {
            turn_id: turn_id.into(),
        },
        AgentDomainEvent::ContentDelta {
            turn_id: turn_id.into(),
            kind: ContentKind::Text,
            text: reply,
        },
        AgentDomainEvent::TurnCompleted {
            turn_id: turn_id.into(),
            outcome: TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: Some("end_turn".into()),
            usage: None,
            detail: Default::default(),
        },
    ]
}

/// `hello`, the theme, the snapshot of `projection` on tab 1, and pane focus -- what Rust sends a
/// freshly loaded panel whose tab holds a live session.
fn on_ready_batch(projection: &AgentSessionProjection) -> Vec<String> {
    let tokens = ThemeTokens::fallback();
    let greeting = BackendGreeting {
        kind: BackendKind::Sidecar,
        project_dir: PathBuf::from("/nonexistent/panel-visual-mode"),
        permission_modes: CLIENT_IMPLEMENTED_PERMISSION_MODES,
        resumable: Vec::new(),
        account: None,
    };
    let snapshot = serialize_snapshot_for_js(
        eitri_core::tabs::TabId(1),
        &SnapshotView {
            backend: "sidecar",
            conversation_id: Some("conversation-visual-mode-test"),
            session_id: Some("visual-mode-test"),
            provider_session_id: Some("claude-visual-mode-test".into()),
            capabilities: agent::ProviderCapabilities {
                resume: true,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
                cli_auto_mode: false,
                steer: false,
                ..Default::default()
            },
            provider: None,
            projection: ProjectionRef::Borrowed(projection),
            hidden_pending: None,
        },
        None,
    );
    // A real launch's `TabSet`
    // always sends a `tabs` envelope before the active tab's own snapshot (`agent_bridge.rs`'s own
    // doc comment on `serialize_tabs_for_js`: "always BEFORE the active tab's snapshot"), because
    // `tabs.ts`'s `acceptsEnvelope` drops a snapshot for a tab the page does not know about yet
    // while `activeTab` is still `null`. This harness never sent one at all, so every W case failed
    // before its first keystroke ("the replay's conversation never rendered within 15 s"). One tab,
    // live, matching the fixed id every fixture builds its snapshot for.
    let tabs = vec![TabView {
        question_draft: Default::default(),
        plan_feedback_draft: Default::default(),
        plan_on_start: false,
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
        following: false,
    }];
    vec![
        serialize_hello_for_js(&greeting),
        serialize_theme_for_js(&tokens),
        serialize_tabs_for_js(eitri_core::tabs::TabId(1), &tabs, SessionModeChoice::Auto),
        snapshot,
        serialize_pane_focus_for_js(true),
    ]
}

/// The main replay .
fn replay_main() -> Vec<String> {
    let mut projection = opened_projection();
    let turn_id = "t1".to_string();
    for event in [
        AgentDomainEvent::UserPromptSubmitted {
            text: "The quick brown fox jumps over the lazy dog".into(),
        },
        AgentDomainEvent::TurnStarted {
            turn_id: turn_id.clone(),
        },
        AgentDomainEvent::ContentDelta {
            turn_id: turn_id.clone(),
            kind: ContentKind::Text,
            text: reply_markdown(),
        },
        // The card comes BEFORE the Bash call, so a row follows it -- W7's copy has to cross
        // the card's own controls (its reason box and buttons) for "none of them reached the copy"
        // to be a claim at all. As the last row, nothing could follow them into a selection.
        AgentDomainEvent::PermissionRequested {
            permission_id: "perm_edit".into(),
            tool_use_id: Some("toolu_edit".into()),
            tool_name: "Edit".into(),
            input: json!({
                "file_path": "/nonexistent/panel-visual-mode/src/lib.rs",
                "old_string": "old text here",
                "new_string": "new text here",
            }),
            provider_prompt: None,
        },
        AgentDomainEvent::ToolCallStarted {
            turn_id: turn_id.clone(),
            tool_use_id: "toolu_bash".into(),
            name: "Bash".into(),
            input: json!({ "command": "cat build.log" }),
        },
        AgentDomainEvent::ToolCallCompleted {
            turn_id: turn_id.clone(),
            tool_use_id: "toolu_bash".into(),
            content: json!(tall_tool_output()),
            is_error: false,
        },
        AgentDomainEvent::TurnCompleted {
            turn_id: turn_id.clone(),
            outcome: TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: Some("end_turn".into()),
            usage: None,
            detail: Default::default(),
        },
    ] {
        projection.apply(&event);
    }
    on_ready_batch(&projection)
}

/// The fixtures replay (a 4th turn is inserted BEFORE the long reply
/// so W10's own "9999j reaches paragraph 300" stays the transcript's real end): rows `[prompt,
/// code-only reply, prompt, table-only reply, prompt, tall-row table reply, prompt, long reply]`, so
/// `gg` then 1 / 3 / 5 / 7 `j` reach the code, the table, W13's own wrapped-cell table and the long
/// reply.
fn replay_fixtures() -> Vec<String> {
    let mut projection = opened_projection();
    for event in finished_turn("tc", "code please", code_block_markdown())
        .into_iter()
        .chain(finished_turn("tt", "table please", TABLE_MARKDOWN.to_string()))
        .chain(finished_turn("tw", "tall row table please", tall_row_table_markdown()))
        .chain(finished_turn("tl", "long please", long_markdown()))
    {
        projection.apply(&event);
    }
    on_ready_batch(&projection)
}

/// W9's own streaming reply, precomputed step by step so `w9_streaming` can deliver it as a real
/// per-delta drip, applied to a FRESH projection -- W9 wants its own turn.
struct StreamingReply {
    /// The prompt, `TurnStarted`, and the FIRST delta -- enough for a reply row to exist.
    setup: String,
    /// The remaining 39 deltas, each its own one-event `events` envelope, in delivery order.
    deltas: Vec<String>,
    /// The reply's full text once every delta has landed.
    full_text: String,
}

fn streaming_reply_steps() -> StreamingReply {
    let mut projection = opened_projection();
    let turn_id = "t-stream".to_string();
    let first_text = "word0 ".to_string();
    let setup_events = vec![
        AgentDomainEvent::UserPromptSubmitted {
            text: "one more, streamed".into(),
        },
        AgentDomainEvent::TurnStarted {
            turn_id: turn_id.clone(),
        },
        AgentDomainEvent::ContentDelta {
            turn_id: turn_id.clone(),
            kind: ContentKind::Text,
            text: first_text.clone(),
        },
    ];
    let setup_from = projection.last_revision;
    for event in &setup_events {
        projection.apply(event);
    }
    let setup = serialize_events_for_js(
        eitri_core::tabs::TabId(1),
        setup_from,
        projection.last_revision,
        &setup_events,
    );

    let mut deltas = Vec::new();
    let mut full_text = first_text;
    for n in 1..40u32 {
        let text = format!("word{n} ");
        full_text.push_str(&text);
        let event = AgentDomainEvent::ContentDelta {
            turn_id: turn_id.clone(),
            kind: ContentKind::Text,
            text,
        };
        let from = projection.last_revision;
        projection.apply(&event);
        deltas.push(serialize_events_for_js(
            eitri_core::tabs::TabId(1),
            from,
            projection.last_revision,
            std::slice::from_ref(&event),
        ));
    }
    StreamingReply {
        setup,
        deltas,
        full_text,
    }
}

/// An `events` envelope for `events`, applied to `projection` on the way so the next one continues
/// from its revision.
fn events_envelope(projection: &mut AgentSessionProjection, events: &[AgentDomainEvent]) -> String {
    let from = projection.last_revision;
    for event in events {
        projection.apply(event);
    }
    serialize_events_for_js(eitri_core::tabs::TabId(1), from, projection.last_revision, events)
}

/// A reply of `paragraphs` short paragraphs: enough rows above the case's own turn that the list scrolls.
fn filler_reply(paragraphs: usize) -> String {
    (1..=paragraphs)
        .map(|n| format!("Filler paragraph {n}, a line of ordinary prose so the conversation is taller than the view."))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The "follow" window's start: three finished turns of filler, then the case's own prompt with its turn
/// running. Returns the replay and the projection, so a case can deliver what follows.
fn replay_follow_start() -> (Vec<String>, AgentSessionProjection) {
    let mut projection = opened_projection();
    for (id, prompt) in [("f1", "first"), ("f2", "second"), ("f3", "third")] {
        for event in finished_turn(id, prompt, filler_reply(6)) {
            projection.apply(&event);
        }
    }
    for event in [
        AgentDomainEvent::UserPromptSubmitted {
            text: "read the files".into(),
        },
        AgentDomainEvent::TurnStarted { turn_id: "tr".into() },
    ] {
        projection.apply(&event);
    }
    (on_ready_batch(&projection), projection)
}

/// Three completed `Read` calls with results taller than a tool result's fold, then a short reply, then the
/// turn's end: the batches a case delivers one at a time.
fn read_turn_batches(projection: &mut AgentSessionProjection) -> Vec<String> {
    let mut batches = Vec::new();
    for n in 0..3 {
        let id = format!("toolu_read{n}");
        let result = (0..14)
            .map(|k| format!("result line {n}.{k}"))
            .collect::<Vec<_>>()
            .join("\n");
        batches.push(events_envelope(
            projection,
            &[
                AgentDomainEvent::ToolCallStarted {
                    turn_id: "tr".into(),
                    tool_use_id: id.clone(),
                    name: "Read".into(),
                    input: json!({ "file_path": format!("/nonexistent/panel-visual-mode/file{n}.txt") }),
                },
                AgentDomainEvent::ToolCallCompleted {
                    turn_id: "tr".into(),
                    tool_use_id: id,
                    content: json!(result),
                    is_error: false,
                },
            ],
        ));
    }
    batches.push(events_envelope(
        projection,
        &[AgentDomainEvent::ContentDelta {
            turn_id: "tr".into(),
            kind: ContentKind::Text,
            text: "Done reading.".into(),
        }],
    ));
    batches.push(events_envelope(
        projection,
        &[AgentDomainEvent::TurnCompleted {
            turn_id: "tr".into(),
            outcome: TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: Some("end_turn".into()),
            usage: None,
            detail: Default::default(),
        }],
    ));
    batches
}

/// How far the list's end lies below the view, in CSS px (`0` when it shows the end).
const DISTANCE_JS: &str = "(() => { const l = document.querySelector('.message-list'); return l.scrollHeight - l.scrollTop - l.clientHeight; })()";

fn distance_to_end(h: &Harness) -> Result<f64, String> {
    h.eval(&format!("JSON.stringify({DISTANCE_JS})"), 1000)?
        .as_f64()
        .ok_or_else(|| "the list reported no distance".into())
}

/// The band's own `n/m` position, for a case's message.
fn band_position(h: &Harness) -> String {
    h.eval(
        "JSON.stringify(document.querySelector('.status-band')?.textContent ?? null)",
        1000,
    )
    .map(|v| v.to_string())
    .unwrap_or_default()
}

/// Enter on a tool row while the view follows the end: the row's result opens in its capped box, the list
/// grows below the cursor, and the view stays at the end rather than leaving the new height off screen.
fn w14_expand_while_following(h: &Harness) -> Result<(), String> {
    let (_, mut projection) = replay_follow_start();
    for batch in read_turn_batches(&mut projection) {
        evaluate_js_dispatch(&h.webview, &batch);
        pump(120);
    }
    pump(600);
    let at_end = distance_to_end(h)?;
    println!(
        "[panel_visual_mode] W14: turn done, distance to the end {at_end:.1}, band {}",
        band_position(h)
    );
    if at_end > 1.0 {
        return Err(format!(
            "W14: the view did not follow the streamed turn to its end ({at_end:.1}px short)"
        ));
    }
    // The cursor rides the run of three reads, which sits above the reply. Open it,
    // walk to its last call and expand that call's result.
    h.xdotool_key("Return");
    pump(500);
    h.xdotool_keys(&["j", "j"]);
    pump(300);
    let before = distance_to_end(h)?;
    println!(
        "[panel_visual_mode] W14: on the last call, distance {before:.1}, band {}",
        band_position(h)
    );
    h.xdotool_key("Return");
    pump(700);
    let after = distance_to_end(h)?;
    println!(
        "[panel_visual_mode] W14: expanded, distance {after:.1}, band {}",
        band_position(h)
    );
    if before > 1.0 {
        return Err(format!(
            "W14: the view was already short of the end before the expand ({before:.1}px)"
        ));
    }
    if after > 1.0 {
        return Err(format!(
            "W14: Enter expanded the last call's result and the view stayed {after:.1}px short of the end"
        ));
    }
    Ok(())
}

/// The "cards" window's start: filler turns, then a running turn holding two permission cards, the first
/// far taller than the list (a 60-line command) and the second short.
fn replay_cards_start() -> (Vec<String>, AgentSessionProjection) {
    let mut projection = opened_projection();
    for (id, prompt) in [("f1", "first"), ("f2", "second")] {
        for event in finished_turn(id, prompt, filler_reply(6)) {
            projection.apply(&event);
        }
    }
    let tall_command = (0..60)
        .map(|k| format!("echo line {k} of a tall command"))
        .collect::<Vec<_>>()
        .join("\n");
    for event in [
        AgentDomainEvent::UserPromptSubmitted {
            text: "run both commands".into(),
        },
        AgentDomainEvent::TurnStarted { turn_id: "tc2".into() },
        AgentDomainEvent::ToolCallStarted {
            turn_id: "tc2".into(),
            tool_use_id: "toolu_tall".into(),
            name: "Bash".into(),
            input: json!({ "command": tall_command }),
        },
        AgentDomainEvent::PermissionRequested {
            permission_id: "perm_tall".into(),
            tool_use_id: Some("toolu_tall".into()),
            tool_name: "Bash".into(),
            input: json!({ "command": tall_command }),
            provider_prompt: None,
        },
        AgentDomainEvent::ToolCallStarted {
            turn_id: "tc2".into(),
            tool_use_id: "toolu_short".into(),
            name: "Bash".into(),
            input: json!({ "command": "echo short" }),
        },
        AgentDomainEvent::PermissionRequested {
            permission_id: "perm_short".into(),
            tool_use_id: Some("toolu_short".into()),
            tool_name: "Bash".into(),
            input: json!({ "command": "echo short" }),
            provider_prompt: None,
        },
    ] {
        projection.apply(&event);
    }
    (on_ready_batch(&projection), projection)
}

/// The tray chip's landing (`focus_permission`) on the oldest of two waiting cards, the first taller than the
/// list, while the view follows the end: the card's head is what shows, and a later arrival does not snap
/// the view back to the end and the card out of it.
fn w15_card_landing_while_following(h: &Harness) -> Result<(), String> {
    let (_, mut projection) = replay_cards_start();
    let at_end = distance_to_end(h)?;
    println!(
        "[panel_visual_mode] W15: two cards waiting, distance to the end {at_end:.1}, band {}",
        band_position(h)
    );
    if at_end > 1.0 {
        return Err(format!(
            "W15: the view was not at the end before the landing ({at_end:.1}px short)"
        ));
    }
    evaluate_js_dispatch(
        &h.webview,
        &serialize_focus_permission_for_js(eitri_core::tabs::TabId(1)),
    );
    pump(700);
    let card_head = "JSON.stringify((() => { const list = document.querySelector('.message-list').getBoundingClientRect(); \
         const card = document.querySelector('.row-current .permission-card'); \
         if (card === null) return null; const r = card.getBoundingClientRect(); \
         return { listTop: list.top, listBottom: list.bottom, cardTop: r.top, cardBottom: r.bottom, tall: card.textContent.includes('tall command') }; })())";
    let landed = h.eval(card_head, 1000)?;
    println!(
        "[panel_visual_mode] W15: after the landing {landed}, band {}",
        band_position(h)
    );
    let head_in_view = |v: &serde_json::Value| -> bool {
        let n = |k: &str| v.get(k).and_then(serde_json::Value::as_f64).unwrap_or(f64::NAN);
        v.get("tall").and_then(serde_json::Value::as_bool) == Some(true)
            && n("cardTop") >= n("listTop") - 1.0
            && n("cardTop") < n("listBottom")
    };
    if !head_in_view(&landed) {
        return Err(format!(
            "W15: the cursor is not on the first card with its head in view after the landing: {landed}"
        ));
    }
    evaluate_js_dispatch(
        &h.webview,
        &events_envelope(
            &mut projection,
            &[AgentDomainEvent::ContentDelta {
                turn_id: "tc2".into(),
                kind: ContentKind::Text,
                text: "LATE TEXT ARRIVAL while the cards wait.".into(),
            }],
        ),
    );
    pump(700);
    let later = h.eval(card_head, 1000)?;
    println!("[panel_visual_mode] W15: after a late arrival {later}");
    if !head_in_view(&later) {
        return Err(format!(
            "W15: a later arrival moved the first card's head out of view: {later}"
        ));
    }
    Ok(())
}

/// The composer's line-edge keys in the real engine, which binds neither `Ctrl+e` (nothing) nor `Ctrl+a`
/// (select all) under GTK's default key theme: on the second of two lines, with the caret in its middle,
/// `Ctrl+e` puts it at that line's end and `Ctrl+a` at that line's start, and the text is untouched.
fn w16_composer_line_edges(h: &Harness) -> Result<(), String> {
    h.xdotool_key("i");
    pump(200);
    if h.mode()?.as_str() != Some("input") {
        return Err(format!("W16: i did not enter INPUT: data-mode={}", h.mode()?));
    }
    h.xdotool_type("first line");
    h.xdotool_key("shift+Return");
    h.xdotool_type("second line");
    h.xdotool_keys(&["Left", "Left", "Left", "Left", "Left"]);
    let caret = "JSON.stringify((() => { const t = document.querySelector('textarea'); \
         return { start: t.selectionStart, end: t.selectionEnd, value: t.value }; })())";
    let at = h.eval(caret, 1000)?;
    println!("[panel_visual_mode] W16: before the keys {at}");
    h.xdotool_key("ctrl+e");
    let end = h.eval(caret, 1000)?;
    h.xdotool_key("ctrl+a");
    let start = h.eval(caret, 1000)?;
    println!("[panel_visual_mode] W16: after Ctrl+e {end}, after Ctrl+a {start}");
    let n = |v: &serde_json::Value, k: &str| v.get(k).and_then(serde_json::Value::as_u64);
    let value = "first line\nsecond line";
    if end.get("value").and_then(serde_json::Value::as_str) != Some(value)
        || start.get("value").and_then(serde_json::Value::as_str) != Some(value)
    {
        return Err(format!("W16: the text changed: {end} / {start}"));
    }
    if n(&end, "start") != Some(22) || n(&end, "end") != Some(22) {
        return Err(format!(
            "W16: Ctrl+e did not put the caret at the line's end (22): {end}"
        ));
    }
    if n(&start, "start") != Some(11) || n(&start, "end") != Some(11) {
        return Err(format!(
            "W16: Ctrl+a did not put the caret at the line's start (11): {start}"
        ));
    }
    h.xdotool_key("Escape");
    pump(200);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// W-cases. Each returns `Ok(())` on a pass, `Err(reason)` on a failure -- `main` collects both.
// ---------------------------------------------------------------------------------------------

/// Harness preflight, not a VISUAL case: a real XTEST key reaches the page as a trusted event. A
/// bare `Shift` is used because it does nothing in BROWSE.
fn w0_keys_reach_the_page(h: &Harness) -> Result<(), String> {
    h.start_key_log()?;
    h.xdotool_key("shift");
    let seen = h.eval(
        "JSON.stringify({ log: window.__nvKeyLog || [], hasFocus: document.hasFocus(), active: document.activeElement ? document.activeElement.className : null })",
        1000,
    )?;
    let trusted_shift = seen
        .get("log")
        .and_then(|l| l.as_array())
        .map(|log| {
            log.iter().any(|e| {
                e.get("type").and_then(|t| t.as_str()) == Some("keydown")
                    && e.get("key").and_then(|k| k.as_str()) == Some("Shift")
                    && e.get("trusted").and_then(serde_json::Value::as_bool) == Some(true)
            })
        })
        .unwrap_or(false);
    if !trusted_shift {
        return Err(format!(
            "a real Shift never reached the page as a trusted keydown -- a harness problem, not a VISUAL defect: {seen}"
        ));
    }
    println!("[panel_visual_mode] W0: keys reach the page ({seen})");
    Ok(())
}

fn w1_vacuity(h: &Harness) -> Result<(), String> {
    let has_modify = h.eval(
        "JSON.stringify(typeof window.getSelection().modify === 'function')",
        2000,
    )?;
    if has_modify != serde_json::Value::Bool(true) {
        return Err(format!(
            "Selection.modify is not a function in this engine: {has_modify}"
        ));
    }
    let has_caret_range = h.eval(
        "JSON.stringify(typeof document.caretRangeFromPoint === 'function')",
        2000,
    )?;
    if has_caret_range != serde_json::Value::Bool(true) {
        return Err(format!(
            "document.caretRangeFromPoint is not a function in this engine: {has_caret_range}"
        ));
    }
    Ok(())
}

fn w2_english_and_chinese_words(h: &Harness) -> Result<(), String> {
    // The prompt row is the first row; `gg` lands the cursor there.
    h.xdotool_keys(&["g", "g"]);
    h.clear_clipboard();
    h.xdotool_keys(&["v", "v", "e", "y"]);
    let first = h.read_fresh_clipboard(2000).ok_or("vvey copied nothing")?;
    if first != "The" {
        return Err(format!("vvey: expected \"The\", got {first:?}"));
    }
    h.clear_clipboard();
    h.xdotool_keys(&["g", "g", "v", "v", "w", "e", "y"]);
    let second = h.read_fresh_clipboard(2000).ok_or("vvwey copied nothing")?;
    if second != "The quick" {
        return Err(format!("vvwey: expected \"The quick\", got {second:?}"));
    }
    h.clear_clipboard();
    h.xdotool_keys(&["g", "g", "v", "v", "2", "e", "y"]);
    let third = h.read_fresh_clipboard(2000).ok_or("vv2ey copied nothing")?;
    if third != "The quick" {
        return Err(format!("vv2ey: expected \"The quick\" (same as vvwey), got {third:?}"));
    }
    // `vvwwy`: two word-forward steps land on the first character of "brown".
    // Inclusive selection includes that character in the copy.
    h.clear_clipboard();
    h.xdotool_keys(&["g", "g", "v", "v", "w", "w", "y"]);
    let fourth = h.read_fresh_clipboard(2000).ok_or("vvwwy copied nothing")?;
    if fourth != "The quick b" {
        return Err(format!("vvwwy: expected \"The quick b\", got {fourth:?}"));
    }
    // Word motions follow vim's word rules. Log the copied Chinese text without an exact
    // boundary assertion until the harness has run again.
    h.clear_clipboard();
    h.xdotool_keys(&["g", "g", "j", "v", "v", "e", "y"]);
    let chinese = h
        .read_fresh_clipboard(2000)
        .ok_or("vvey on the Chinese paragraph copied nothing")?;
    println!("[panel_visual_mode] W2: vvey on the Chinese paragraph copied {chinese:?} (recorded, R7)");
    Ok(())
}

fn w3_linewise_prompt_and_reply(h: &Harness) -> Result<(), String> {
    h.clear_clipboard();
    h.xdotool_keys(&["g", "g", "V", "j", "y"]);
    let text = h.read_fresh_clipboard(2000).ok_or("Vjy copied nothing")?;
    if !text.contains("The quick brown fox") {
        return Err(format!("Vjy: missing the prompt's own text: {text:?}"));
    }
    if !text.contains(CHINESE_PARAGRAPH) {
        return Err(format!("Vjy: missing the reply's own text: {text:?}"));
    }
    if text.contains('\u{203a}') || text.to_lowercase().contains("approve") || text.to_lowercase().contains("deny") {
        return Err(format!("Vjy: chrome leaked into the copy: {text:?}"));
    }
    // From CARET, `V` switches straight to V-LINE.
    // `v V j y` must copy the same text as the plain `V j y` above.
    h.clear_clipboard();
    h.xdotool_keys(&["g", "g", "v", "V", "j", "y"]);
    let via_caret = h.read_fresh_clipboard(2000).ok_or("vVjy copied nothing")?;
    if via_caret != text {
        return Err(format!(
            "vVjy: expected the same text as Vjy ({text:?}), got {via_caret:?}"
        ));
    }
    Ok(())
}

/// `vv$y` enters CARET then VISUAL and copies one code line; `Vjy` copies two lines from the
/// same code-block entry point. Exact comparisons reject highlight markup residue and require
/// one `\n` between the two lines. A trailing `\n` is tolerated and printed because `$` reaches
/// the line end and the inclusive selection may include its newline.
fn code_block_lines(h: &Harness, where_: &str, enter: &dyn Fn()) -> Result<(), String> {
    h.clear_clipboard();
    enter();
    h.xdotool_keys(&["v", "v", "dollar", "y"]);
    let one = h
        .read_fresh_clipboard(2000)
        .ok_or(format!("W4 ({where_}): vv$y copied nothing"))?;
    println!("[panel_visual_mode] W4 ({where_}): vv$y copied {one:?}");
    if one.trim_end_matches('\n') != CODE_LINE_1 {
        return Err(format!(
            "W4 ({where_}): vv$y should copy exactly {CODE_LINE_1:?}, got {one:?}"
        ));
    }
    h.clear_clipboard();
    enter();
    h.xdotool_keys(&["V", "j", "y"]);
    let two = h
        .read_fresh_clipboard(2000)
        .ok_or(format!("W4 ({where_}): Vjy copied nothing"))?;
    println!("[panel_visual_mode] W4 ({where_}): Vjy copied {two:?}");
    let want = format!("{CODE_LINE_1}\n{CODE_LINE_2}");
    if two.trim_end_matches('\n') != want {
        return Err(format!("W4 ({where_}): Vjy should copy exactly {want:?}, got {two:?}"));
    }
    Ok(())
}

/// On the code-only reply, `v` starts on the code block's first character without a HINT
/// landing.
fn w4_code_block_row(h: &Harness) -> Result<(), String> {
    code_block_lines(h, "code-only reply", &|| h.xdotool_keys(&["g", "g", "j"]))
}

/// After HINT lands on a code block between prose and a table, `v` starts on that block's first
/// character. The page reports only the target count, so each index is tried. Once focus
/// returns to the panel root in BROWSE, `y` identifies a code-block landing by copying exactly
/// that block.
fn w4_code_block_after_hint(h: &Harness) -> Result<(), String> {
    let block = format!("{CODE_LINE_1}\n{CODE_LINE_2}\n{CODE_LINE_3}");
    let on_root_in_browse = "JSON.stringify(document.activeElement !== null && document.activeElement.classList.contains('agent-ui-conversation') \
        && document.querySelector('[data-testid=\"mode-block\"]')?.getAttribute('data-mode') === 'browse')";
    let (_, count) = h.hint_collect()?;
    let mut found = None;
    for index in 0..count {
        let (session, _) = h.hint_collect()?;
        h.hint_land(session, index);
        if h.eval(on_root_in_browse, 1000)? != serde_json::Value::Bool(true) {
            // A control or the composer took the keys: leave it without typing anything.
            h.xdotool_key("Escape");
            continue;
        }
        h.clear_clipboard();
        h.xdotool_key("y");
        if h.read_fresh_clipboard(1000)
            .map(|t| t.trim_end_matches('\n') == block)
            .unwrap_or(false)
        {
            found = Some(index);
            break;
        }
    }
    let index = found.ok_or(format!(
        "W4 (after HINT): none of the {count} HINT targets was the code block (no landing made y copy it)"
    ))?;
    code_block_lines(h, "after a HINT landing", &|| {
        if let Ok((session, _)) = h.hint_collect() {
            h.hint_land(session, index);
        }
    })
}

/// W5 on the fixtures' table-only reply: `V` starts on the header's first cell, `jj` walks two rows
/// down. Structurally: the first line is the header's cells, tab-separated, the second the first
/// body row's, and the third the second body row's, whole. Inside a table V-LINE's hard
/// line is the ROW (`visual.ts`'s `tableRowEdge`); `paragraphboundary` alone stopped at the cell, and
/// this case used to accept a third line of just "3". The exact string is pinned in `W5_PINNED`.
fn w5_table(h: &Harness) -> Result<(), String> {
    h.clear_clipboard();
    h.xdotool_keys(&["g", "g", "j", "j", "j", "V", "j", "j", "y"]);
    let text = h
        .read_fresh_clipboard(2000)
        .ok_or("W5: Vjjy on the table copied nothing")?;
    println!("[panel_visual_mode] W5: Vjjy across the table copied {text:?} (pin this in W5_PINNED)");
    if let Some(pinned) = W5_PINNED {
        if text != pinned {
            return Err(format!("W5: expected the pinned {pinned:?}, got {text:?}"));
        }
    }
    let lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    let cells = |line: &str| line.split('\t').map(str::trim).map(str::to_string).collect::<Vec<_>>();
    if lines.len() < 3 {
        return Err(format!("W5: rows should be newline-separated, three of them: {text:?}"));
    }
    if cells(lines[0]) != ["a", "b"] || cells(lines[1]) != ["1", "2"] {
        return Err(format!(
            "W5: cells should be tab-separated, a/b then 1/2: {:?} / {:?} in {text:?}",
            cells(lines[0]),
            cells(lines[1])
        ));
    }
    if cells(lines[2]) != ["3", "4"] {
        return Err(format!(
            "W5: the third row should be whole, 3 then 4 (V-LINE takes rows): {text:?}"
        ));
    }
    Ok(())
}

/// Search reaches a folded tool-output row, so `Enter` must unfold it before selection.
/// `MessageList` draws the result only when `expanded[key] === true`; search can match text
/// that is still folded.
///
/// After each `j`, the caret must lie inside both the tool-result box and the list's visible
/// rectangles. V-LINE copies from its starting line through the line reached by 40 `j` motions,
/// excluding the next line. When the row's top is visible, `V` starts on `$ cat build.log`, so
/// the last copied line is "build line 040". Otherwise it starts on the first output line under
/// the list's top edge, and the expectation is read from there. The DOM-based test in
/// `agent-ui/web/src/App.test.tsx` uses the same keys with a hard-line `modify` stub.
fn w6_tall_tool_output(h: &Harness) -> Result<(), String> {
    search(h, "build line 001");
    let folded = h.eval(
        "JSON.stringify(document.querySelector('.tool-result-body') === null)",
        1000,
    )?;
    if folded != serde_json::Value::Bool(true) {
        return Err(format!(
            "W6: the result `/` found should still be folded before Enter (a finished result folds by default): {folded}"
        ));
    }
    h.xdotool_key("Return");
    let drawn = h.eval(
        "JSON.stringify(document.querySelector('.tool-result-body') !== null)",
        1000,
    )?;
    if drawn != serde_json::Value::Bool(true) {
        return Err(format!("W6: Enter on the tool row did not draw its result: {drawn}"));
    }
    h.xdotool_key("shift+v");
    // Where `V` started: 0 for the invocation line, N for "build line N".
    let start_probe = r#"(() => {
        const sel = window.getSelection();
        const node = sel.anchorNode;
        if (!node) return JSON.stringify({ where: "none" });
        const el = node.nodeType === 1 ? node : node.parentElement;
        if (el && el.closest(".tool-card-bash")) return JSON.stringify({ where: "invocation", line: 0 });
        const box = el ? el.closest(".tool-result-body") : null;
        if (box) {
            // The anchor's position in the box's own text, whatever node WebKit reports it in.
            const upTo = document.createRange();
            upTo.setStart(box, 0);
            upTo.setEnd(node, sel.anchorOffset);
            const at = upTo.toString().length;
            const text = box.textContent;
            const lineStart = text.lastIndexOf("\n", Math.max(0, at - 1)) + 1;
            const lineEnd = text.indexOf("\n", lineStart);
            const line = text.slice(lineStart, lineEnd === -1 ? text.length : lineEnd);
            const m = /build line (\d{3})/.exec(line);
            return JSON.stringify({ where: "output", line: m ? Number(m[1]) : null, text: line });
        }
        return JSON.stringify({ where: "elsewhere", className: el ? String(el.className) : null });
    })()"#;
    let start = h.eval(start_probe, 1000)?;
    let start_line = start
        .get("line")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| format!("W6: V did not start on the tool row's invocation or an output line: {start}"))?;
    println!("[panel_visual_mode] W6: V started on {start}");
    let caret_probe = r#"(() => {
        const sel = window.getSelection();
        const node = sel.focusNode;
        if (!node) return JSON.stringify({ ok: false, reason: "no focusNode" });
        let range;
        try {
            range = document.createRange();
            range.setStart(node, sel.focusOffset);
            range.setEnd(node, sel.focusOffset);
        } catch (e) {
            return JSON.stringify({ ok: false, reason: "range error: " + e.message });
        }
        const r = range.getBoundingClientRect();
        const el = node.nodeType === 1 ? node : node.parentElement;
        const box = el ? el.closest(".tool-result-body") : null;
        const list = el ? el.closest(".message-list") : null;
        if (!box || !list) return JSON.stringify({ ok: false, reason: "no box/list ancestor", hasBox: !!box, hasList: !!list });
        const b = box.getBoundingClientRect();
        const l = list.getBoundingClientRect();
        const insideBox = r.top >= b.top - 1 && r.bottom <= b.bottom + 1;
        const insideList = r.top >= l.top - 1 && r.bottom <= l.bottom + 1;
        return JSON.stringify({ ok: insideBox && insideList, insideBox, insideList, rTop: r.top, rBottom: r.bottom, bTop: b.top, bBottom: b.bottom });
    })()"#;
    for step in 1..=40 {
        h.xdotool_key("j");
        let result = h.eval(caret_probe, 1000)?;
        if result.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(format!("W6: caret left the visible box/list after j #{step}: {result}"));
        }
    }
    h.clear_clipboard();
    h.xdotool_key("y");
    let text = h
        .read_fresh_clipboard(2000)
        .ok_or("W6: V then 40 j then y copied nothing")?;
    // The output has 80 lines; a walk that would pass the last stops on it (the row is the last).
    let last = (start_line + 40).min(80);
    let first_wanted = if start_line == 0 {
        vec!["$ cat build.log".to_string(), "build line 001".to_string()]
    } else {
        vec![format!("build line {start_line:03}")]
    };
    for wanted in first_wanted
        .iter()
        .chain(std::iter::once(&format!("build line {last:03}")))
    {
        if !text.contains(wanted.as_str()) {
            return Err(format!(
                "W6: the copy did not hold every line the walk passed (missing {wanted:?}): {text:?}"
            ));
        }
    }
    if last < 80 {
        let past = format!("build line {:03}", last + 1);
        if text.contains(&past) {
            return Err(format!(
                "W6: V-LINE should end on the cursor's own line ({last:03}), but the copy holds {past:?}: {text:?}"
            ));
        }
    }
    Ok(())
}

/// `/` to the waiting `Edit` card: its input JSON is the only row text holding "old text here".
/// A failure's context: the band's mode and message, what has focus, and the row cursor.
fn page_state(h: &Harness) -> String {
    h.eval(
        "JSON.stringify({ mode: document.querySelector('[data-testid=\"mode-block\"]')?.getAttribute('data-mode') ?? null, \
         flash: document.querySelector('.band-message')?.textContent ?? null, \
         active: document.activeElement ? (document.activeElement.tagName + '.' + document.activeElement.className + ' ' + (document.activeElement.getAttribute('aria-label') ?? document.activeElement.getAttribute('placeholder') ?? '') + ' =' + (document.activeElement.value ?? '')) : null, \
         searchOpen: document.querySelector('.search-bar') !== null, \
         current: document.querySelector('.row-current')?.className ?? null, \
         selection: String(window.getSelection()).slice(0, 40) })",
        1000,
    )
    .map(|v| v.to_string())
    .unwrap_or_else(|e| format!("(page state unreadable: {e})"))
}

/// `/`, `text`, `Return`: search the panel and land the row cursor on the first match.
fn search(h: &Harness, text: &str) {
    let _ = h.start_key_log();
    h.xdotool_key("slash");
    h.xdotool_type(text);
    h.xdotool_key("Return");
    // The page handles keys on its own process's time: wait for the prompt to close (every typed
    // character re-runs the search) before the caller's next key, so that key never lands in it.
    let end = Instant::now() + Duration::from_millis(3000);
    loop {
        let open = h.eval("JSON.stringify(document.querySelector('.search-bar') !== null)", 1000);
        if open.as_ref().ok() != Some(&serde_json::Value::Bool(true)) {
            return;
        }
        if Instant::now() >= end {
            let log = h.eval("JSON.stringify(window.__nvKeyLog || [])", 1000);
            println!(
                "[panel_visual_mode] search {text:?}: the / prompt is still open 3 s after Return; key log: {log:?}"
            );
            return;
        }
        pump(20);
    }
}

fn to_card(h: &Harness) {
    search(h, "old text here");
}

/// What W7 types into the card's reason box, so its absence from the copy is a claim.
const W7_REASON: &str = "w7 reason box marker";

fn w7_permission_card(h: &Harness) -> Result<(), String> {
    // Text in the reason box, without moving focus (a click would be the explicit act that is under
    // test further down): React's own `value` setter trick, which its `onChange` hears.
    let typed = h.eval(
        &format!(
            "(() => {{ const box = document.querySelector('.permission-card input'); if (!box) return JSON.stringify(false); \
             const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set; \
             set.call(box, {reason}); box.dispatchEvent(new Event('input', {{ bubbles: true }})); \
             return JSON.stringify(box.value === {reason}); }})()",
            reason = serde_json::to_string(W7_REASON).unwrap_or_default()
        ),
        1000,
    )?;
    if typed != serde_json::Value::Bool(true) {
        return Err(format!("W7: could not put text in the reason box: {typed}"));
    }
    // The first version's `vjjy` never reached the card's controls
    // -- the card was the last row, and three lines from its top end in its diff -- so "no Approve,
    // Deny, Always allow" held of any copy at all. V-LINE from the card's first line, five `j` down:
    // across the diff, the reason box and the buttons, into the Bash row the replay puts after it.
    to_card(h);
    // A search that lands on a row too tall for the list (the card plus its margins is taller than the list
    // at zoom 1.5) leaves the row where it is when part of it is already in
    // view, so its heading can sit above the list's top. `V` then starts at the first character the
    // reader can see, which is the rule, not a defect: the copy below wants the heading, so the card
    // is put at the top of the view first, as a reader would with `zt`.
    h.xdotool_keys(&["z", "t"]);
    pump(400);
    let head_in_view = h.eval(
        "JSON.stringify((() => { const list = document.querySelector('.message-list').getBoundingClientRect(); \
         const card = document.querySelector('.row-current .permission-card-tool'); \
         return card !== null && card.getBoundingClientRect().top >= list.top - 1; })())",
        1000,
    )?;
    if head_in_view != serde_json::Value::Bool(true) {
        return Err(format!(
            "W7: `zt` did not bring the card's heading into view: {head_in_view}"
        ));
    }
    h.clear_clipboard();
    h.xdotool_keys(&["shift+v", "j", "j", "j", "j", "j", "y"]);
    let text = h
        .read_fresh_clipboard(2000)
        .ok_or("W7: V then 5 j then y on the card copied nothing")?;
    println!("[panel_visual_mode] W7: V 5j y from the card copied {text:?}");
    // What the card says, all of it: its heading, the file (the "command" of an Edit), the change.
    for wanted in ["Permission requested: Edit", "lib.rs", "old text here", "new text here"] {
        if !text.contains(wanted) {
            return Err(format!("W7: the copy is missing the card's own {wanted:?}: {text:?}"));
        }
    }
    if !text.contains("$ cat build.log") {
        return Err(format!(
            "W7: the walk never crossed the card's controls into the next row (R1: j over a form control or buttons) -- \
             the chrome checks below would say nothing: {text:?}"
        ));
    }
    // Every VISUAL_CHROME member is hidden with `display: none` while reading: buttons, the
    // reason box's value and placeholder, and the diff gutter (`[aria-hidden="true"]`). The
    // diff gutter and text are separate flex items, so their text appears on separate lines
    // even without hiding the gutter. Banning "+new text here" or "-old text here" would
    // therefore miss a visible gutter. Check the gutter's standalone "+" and "-" lines
    // separately below.
    for banned in ["Approve", "Deny", "Always allow", W7_REASON, "Enter denies"] {
        if text.contains(banned) {
            return Err(format!(
                "W7: the card's own chrome leaked into the copy: {banned:?} in {text:?}"
            ));
        }
    }
    // The diff gutter's `+`/`-` glyph must never appear as a line by itself -- the edit head's own
    // counts read "+1 −1" (U+2212 MINUS SIGN, not ASCII hyphen, and never alone on a line), so they
    // cannot collide with this check.
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "+" || trimmed == "-" {
            return Err(format!(
                "W7: the diff gutter leaked into the copy as its own line ({trimmed:?}) -- the shield did not hide it: {text:?}"
            ));
        }
    }
    // `a`, `d`, and `Shift+D` must post no permission answer in CARET or VISUAL. The
    // capture-phase key handler applies in both modes.
    for keys in [
        vec!["v", "d"],
        vec!["v", "a"],
        vec!["v", "shift+d"],
        vec!["v", "v", "d"],
        vec!["v", "v", "a"],
        vec!["v", "v", "shift+d"],
    ] {
        to_card(h);
        h.clear_posted();
        h.xdotool_keys(&keys);
        pump(300); // Wait beyond the 250 ms guard window so a deferred permission answer has time to fire.
        if let Some(msg) = h.posted_type("permission_response") {
            return Err(format!(
                "W7: {keys:?} on the card posted permission_response while VISUAL was on: {msg}"
            ));
        }
        let mode = h.mode()?;
        if mode.as_str() != Some("browse") {
            return Err(format!("W7: {keys:?} did not end VISUAL: data-mode={mode}"));
        }
    }
    println!("[panel_visual_mode] W7: v/vv then a/d/D posted no permission_response, in CARET and in VISUAL");

    // v, then a real click into the reason box: BROWSE at once, and the Enter
    // that follows denies as the box says. A plain `v` starts CARET, not VISUAL.
    to_card(h);
    h.xdotool_key("v");
    if h.mode()?.as_str() != Some("caret") {
        return Err(format!(
            "W7: v on the card did not enter CARET before the click: {}",
            page_state(h)
        ));
    }
    let rect = h.eval(
        "(() => { const box = document.querySelector('.permission-card input'); if (!box) return JSON.stringify(null); \
         box.scrollIntoView({ block: 'nearest' }); const r = box.getBoundingClientRect(); \
         return JSON.stringify({ x: r.left + r.width / 2, y: r.top + r.height / 2 }); })()",
        1000,
    )?;
    let (Some(x), Some(y)) = (
        rect.get("x").and_then(serde_json::Value::as_f64),
        rect.get("y").and_then(serde_json::Value::as_f64),
    ) else {
        return Err(format!("W7: could not read the reason box's rect: {rect}"));
    };
    h.click_css(x, y)?;
    let focused_box = h.eval(
        "JSON.stringify(document.activeElement === document.querySelector('.permission-card input'))",
        1000,
    )?;
    if focused_box != serde_json::Value::Bool(true) {
        return Err(format!(
            "W7: the click did not land in the reason box (activeElement is not it) -- the click's coordinates, not VISUAL: {focused_box}"
        ));
    }
    let mode = h.mode()?;
    if mode.as_str() != Some("browse") {
        return Err(format!(
            "W7: a click into the reason box did not end VISUAL: data-mode={mode}"
        ));
    }
    h.clear_posted();
    h.xdotool_key("Return");
    pump(300);
    match h.posted_type("permission_response") {
        Some(msg) if msg.get("decision").and_then(|d| d.as_str()) == Some("deny") => Ok(()),
        Some(msg) => Err(format!("W7: Enter in the reason box answered, but not deny: {msg}")),
        None => Err("W7: Enter in the reason box after the click posted no permission_response at all".into()),
    }
}

/// `v v l Escape` enters CARET, enters VISUAL, moves once, then returns to CARET with one
/// character selected at the moving end. A second `Esc` returns to BROWSE with nothing
/// selected.
fn w8_escape(h: &Harness) -> Result<(), String> {
    h.xdotool_keys(&["g", "g", "v", "v", "l", "Escape"]);
    let mode = h.mode()?;
    if mode.as_str() != Some("caret") {
        return Err(format!(
            "W8: Esc from VISUAL did not land CARET: data-mode={mode}; {}",
            page_state(h)
        ));
    }
    // CARET must return to the moving end: `v` entered on "T" and `l` moved to "h", so its
    // one-character selection must read "h". A length-only check would also pass if it returned
    // to the anchor on "T".
    let selection_text = h.eval("JSON.stringify(String(window.getSelection()))", 1000)?;
    if selection_text.as_str() != Some("h") {
        return Err(format!(
            "W8: Esc from VISUAL should leave CARET's own one-character selection on the moving end, \"h\": {selection_text}"
        ));
    }
    h.xdotool_key("Escape");
    let selection_text = h.eval("JSON.stringify(String(window.getSelection()))", 1000)?;
    if selection_text.as_str() != Some("") {
        return Err(format!(
            "W8: the second Esc left a non-empty native selection: {selection_text}"
        ));
    }
    let mode = h.mode()?;
    if mode.as_str() != Some("browse") {
        return Err(format!(
            "W8: the band did not read browse after the second Esc: data-mode={mode}"
        ));
    }
    h.clear_clipboard();
    h.xdotool_key("y");
    let row_copy = h.read_fresh_clipboard(2000).ok_or("W8: y after Esc copied nothing")?;
    if !row_copy.contains("The quick brown fox") {
        return Err(format!(
            "W8: y after Esc did not copy the (BROWSE) row it landed on: {row_copy:?}"
        ));
    }
    Ok(())
}

/// `v` alone only starts CARET, so W9 starts there and moves (`l`) before the
/// SECOND `v` switches to VISUAL for `e e` over the deltas (W9 starts
/// CARET in the streaming reply, moves, then `v e e` over the deltas).
fn w9_streaming(h: &Harness) -> Result<(), String> {
    let reply = streaming_reply_steps();
    evaluate_js_dispatch(&h.webview, &reply.setup);
    pump(150);
    h.xdotool_keys(&["G", "v"]);
    if h.mode()?.as_str() != Some("caret") {
        return Err(format!("W9: G v did not start CARET: data-mode={}", h.mode()?));
    }
    h.xdotool_keys(&["l", "v", "e", "e"]);
    let mode = h.mode()?;
    if mode.as_str() != Some("visual") {
        return Err(format!(
            "W9: v l v ee on the streaming reply did not enter VISUAL: data-mode={mode}"
        ));
    }
    // Streaming deltas must preserve both the selection's anchor node and the reply row's DOM
    // node. Select the last `.row-assistant` by array position because `:last-of-type` matches
    // by tag.
    h.eval(
        "(() => { const rows = document.querySelectorAll('.row-assistant'); window.__w9AnchorNode = window.getSelection().anchorNode; \
         window.__w9ReplyNode = rows[rows.length - 1] ?? null; return JSON.stringify(true); })()",
        1000,
    )?;
    for (i, delta) in reply.deltas.iter().enumerate() {
        evaluate_js_dispatch(&h.webview, delta);
        pump(33); // the pump's own cadence
        let identity = h.eval(
            "JSON.stringify({ sameAnchor: window.getSelection().anchorNode === window.__w9AnchorNode, sameNode: (() => { const rows = document.querySelectorAll('.row-assistant'); return (rows[rows.length - 1] ?? null) === window.__w9ReplyNode; })() })",
            500,
        )?;
        if identity.get("sameAnchor").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(format!(
                "W9: the selection's own anchor moved after delta #{i}: {identity}"
            ));
        }
        if identity.get("sameNode").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(format!(
                "W9: the reply's own DOM node was replaced (not merely extended) after delta #{i}: {identity}"
            ));
        }
    }
    h.clear_clipboard();
    h.xdotool_key("y");
    let copied = h
        .read_fresh_clipboard(2000)
        .ok_or("W9: y after the drip copied nothing")?;
    if copied.trim().is_empty() {
        return Err("W9: y after the drip copied an empty string".into());
    }
    h.xdotool_key("Escape");
    let after = h.eval(
        "(() => { const rows = document.querySelectorAll('.row-assistant'); const last = rows[rows.length - 1]; return JSON.stringify(last ? last.textContent : null); })()",
        1000,
    )?;
    let after_text = after
        .as_str()
        .ok_or_else(|| format!("W9: no reply row found after Esc: {after}"))?;
    if !after_text.contains(reply.full_text.trim()) {
        return Err(format!(
            "W9: the reply did not hold every delta once VISUAL ended: got {after_text:?}, wanted it to contain {:?}",
            reply.full_text.trim()
        ));
    }
    Ok(())
}

fn w10_counts(h: &Harness) -> Result<(), String> {
    h.xdotool_keys(&["g", "g"]);
    h.clear_clipboard();
    h.xdotool_keys(&["v", "v", "3", "w", "y"]);
    let three_w = h.read_fresh_clipboard(2000).ok_or("vv3wy copied nothing")?;
    h.clear_clipboard();
    h.xdotool_keys(&["g", "g", "v", "v", "w", "w", "w", "y"]);
    let www = h.read_fresh_clipboard(2000).ok_or("vvwwwy copied nothing")?;
    if three_w != www {
        return Err(format!("W10: 3w != www: {three_w:?} vs {www:?}"));
    }
    println!("[panel_visual_mode] W10: 3w == www ({three_w:?})");
    Ok(())
}

/// Time `9999l`, `9999j`, and `y` on each resulting large selection in the long transcript.
/// Each motion stops at the first step that does not move.
///
/// The last paragraph of `9999j` is checked by prefix because charwise `j` preserves a screen
/// column rather than selecting a whole line. Geometry in `stepByLinePoint` and
/// `caretRangeFromPoint` accumulates subpixel rounding across 300 steps. At zoom 1 and 1.5, the
/// live selection reaches paragraph 300 at different character offsets (12 and 2). Both reach
/// the transcript's end; requiring the whole final paragraph would reject valid
/// column-preserving motion.
fn w10_long_transcript_timing(h: &Harness) -> Result<(), String> {
    const HANG_GUARD_MS: u64 = 60_000;
    let last_paragraph_full = format!("para {LONG_PARAGRAPHS:03}: the quick brown fox jumps over the lazy dog");
    for (motion, keysym) in [("l", "l"), ("j", "j")] {
        h.xdotool_keys(&["g", "g", "v", "v", "9", "9", "9", "9"]);
        h.start_key_log()?;
        h.xdotool_key(keysym);
        let motion_ms = h.keydown_to_keyup_ms(motion, HANG_GUARD_MS)?;
        if h.mode()?.as_str() != Some("visual") {
            return Err(format!("W10: 9999{motion} did not leave VISUAL on"));
        }
        h.clear_clipboard();
        h.start_key_log()?;
        let copy_started = Instant::now();
        h.xdotool_key("y");
        let yank_ms = h.keydown_to_keyup_ms("y", HANG_GUARD_MS)?;
        let copied = h
            .read_fresh_clipboard(5000)
            .ok_or(format!("W10: y after 9999{motion} copied nothing"))?;
        let wall = copy_started.elapsed();
        println!(
            "[panel_visual_mode] W10: 9999{motion}: {motion_ms:.1} ms keydown->keyup; y on {} chars: {yank_ms:.1} ms keydown->keyup, {wall:?} to the clipboard (recorded)",
            copied.chars().count()
        );
        if !copied.contains("code please") {
            let head: String = copied.chars().take(80).collect();
            return Err(format!(
                "W10: y after 9999{motion} did not start at the first row: {head:?}"
            ));
        }
        if motion == "j" {
            // A prefix check alone could not tell paragraph 300 from any
            // other at zoom 1.5, where the goal column lands two characters in and every paragraph
            // starts "par". The copy must hold paragraph 299 whole, and end in a prefix of 300.
            let second_last_full = format!(
                "para {:03}: the quick brown fox jumps over the lazy dog",
                LONG_PARAGRAPHS - 1
            );
            if !copied.contains(&second_last_full) {
                return Err(format!(
                    "W10: 9999j should pass paragraph {} whole on its way to the end; the copy does not hold {second_last_full:?}",
                    LONG_PARAGRAPHS - 1
                ));
            }
            let last_segment = copied.trim_end_matches('\n').rsplit("\n\n").next().unwrap_or("");
            if last_segment.is_empty() || !last_paragraph_full.starts_with(last_segment) {
                return Err(format!(
                    "W10: 9999j should reach the transcript's end ({last_paragraph_full:?}); its last segment was {last_segment:?}"
                ));
            }
            let before_last = copied.trim_end_matches('\n').rsplit("\n\n").nth(1).unwrap_or("");
            if before_last != second_last_full {
                return Err(format!(
                    "W10: the segment before 9999j's last one should be paragraph {} whole, was {before_last:?}",
                    LONG_PARAGRAPHS - 1
                ));
            }
            println!(
                "[panel_visual_mode] W10: 9999j ended in {last_segment:?}, right after paragraph {} whole",
                LONG_PARAGRAPHS - 1
            );
        }
    }
    Ok(())
}

fn message_list_data_visual(h: &Harness) -> Result<Option<String>, String> {
    let v = h.eval(
        "JSON.stringify(document.querySelector('.message-list')?.getAttribute('data-visual') ?? null)",
        1000,
    )?;
    Ok(v.as_str().map(str::to_string))
}

/// What the live selection is, read against the list: its text, whether both ends are inside
/// `.message-list`, which conversation row (outermost `data-nav-stop="row"`, as `nav.ts` counts) the
/// anchor sits in, how many rows there are, and -- the G oracle -- the list's last selectable
/// character, found with a walk of its own over the text (chrome skipped by the same selectors
/// `VISUAL_CHROME` names), not by the product's `lastSelectableCaret`.
const CARET_INFO_JS: &str = r#"JSON.stringify((() => {
  const sel = window.getSelection();
  const list = document.querySelector('.message-list');
  const rows = [...list.querySelectorAll('[data-nav-stop="row"]')].filter((r) => {
    const outer = r.parentElement && r.parentElement.closest('[data-nav-stop]');
    return !outer || !list.contains(outer);
  });
  const rowOf = (n) => rows.findIndex((r) => r.contains(n));
  const chrome = '.row-sign, [aria-hidden="true"], button, input, textarea, [data-nav-action], .fold-marker';
  const walker = document.createTreeWalker(list, NodeFilter.SHOW_TEXT, {
    acceptNode: (n) => (!/\S/.test(n.data) || (n.parentElement && n.parentElement.closest(chrome)))
      ? NodeFilter.FILTER_SKIP : NodeFilter.FILTER_ACCEPT,
  });
  let last = null, n;
  while ((n = walker.nextNode())) last = n;
  const lastText = last ? last.data.replace(/\s+$/, '') : '';
  return {
    text: String(sel),
    anchorIn: !!sel.anchorNode && list.contains(sel.anchorNode),
    focusIn: !!sel.focusNode && list.contains(sel.focusNode),
    anchorRow: sel.anchorNode ? rowOf(sel.anchorNode) : -1,
    rows: rows.length,
    lastChar: [...lastText].pop() ?? null,
  };
})())"#;

struct CaretInfo {
    text: String,
    anchor_in: bool,
    focus_in: bool,
    anchor_row: i64,
    rows: i64,
    last_char: Option<String>,
}

fn caret_info(h: &Harness) -> Result<CaretInfo, String> {
    let v = h.eval(CARET_INFO_JS, 1000)?;
    let field = |k: &str| v.get(k).cloned().unwrap_or(serde_json::Value::Null);
    Ok(CaretInfo {
        text: field("text")
            .as_str()
            .map(str::to_string)
            .ok_or(format!("caret info: {v}"))?,
        anchor_in: field("anchorIn").as_bool().unwrap_or(false),
        focus_in: field("focusIn").as_bool().unwrap_or(false),
        anchor_row: field("anchorRow").as_i64().unwrap_or(-1),
        rows: field("rows").as_i64().unwrap_or(0),
        last_char: field("lastChar").as_str().map(str::to_string),
    })
}

/// Check the expected character as well as the one-character selection after every motion. On
/// "The quick brown fox jumps over the lazy dog", `gg v` selects "T", `l` selects "h", `w`
/// selects "q", and `$` selects the line break or final "g". `gg` returns to "T" and `3w`
/// reaches the "f" of "fox". `G` selects the list's last character with both selection ends
/// inside the list, excluding the following newline. `gg j` crosses into the reply.
/// `data-visual="caret"` holds throughout; `Esc` moves `.row-current` to the caret's row and
/// returns the band to BROWSE.
fn w11_caret_motions(h: &Harness) -> Result<(), String> {
    h.xdotool_keys(&["g", "g", "v"]);
    if h.mode()?.as_str() != Some("caret") {
        return Err(format!("W11: g g v did not start CARET: data-mode={}", h.mode()?));
    }
    let check = |step: &str, info: &CaretInfo, want: &[&str], row: Option<i64>| -> Result<(), String> {
        if info.text.chars().count() != 1 {
            return Err(format!(
                "W11: after {step}, CARET's selection should be one character, was {:?}",
                info.text
            ));
        }
        if !info.anchor_in {
            return Err(format!(
                "W11: after {step}, the caret is not inside .message-list ({:?})",
                info.text
            ));
        }
        if !want.is_empty() && !want.contains(&info.text.as_str()) {
            return Err(format!(
                "W11: after {step}, the caret should be on {want:?}, was on {:?}",
                info.text
            ));
        }
        if let Some(row) = row {
            if info.anchor_row != row {
                return Err(format!(
                    "W11: after {step}, the caret should be in row {row}, was in row {}",
                    info.anchor_row
                ));
            }
        }
        Ok(())
    };
    for (step, keys, want, row) in [
        ("v (entry)", &[][..], &["T"][..], Some(0)),
        ("l", &["l"][..], &["h"][..], Some(0)),
        ("w", &["w"][..], &["q"][..], Some(0)),
        ("$", &["dollar"][..], &["g"][..], Some(0)),
        ("gg", &["g", "g"][..], &["T"][..], Some(0)),
        ("3w", &["3", "w"][..], &["f"][..], Some(0)),
    ] {
        if !keys.is_empty() {
            h.xdotool_keys(keys);
        }
        let info = caret_info(h)?;
        check(step, &info, want, row)?;
        let visual = message_list_data_visual(h)?;
        if visual.as_deref() != Some("caret") {
            return Err(format!(
                "W11: after {step}, .message-list data-visual should be \"caret\", was {visual:?}"
            ));
        }
        println!("[panel_visual_mode] W11: after {step}: {:?}", info.text);
    }
    h.xdotool_key("shift+g");
    let info = caret_info(h)?;
    let last = info
        .last_char
        .clone()
        .ok_or("W11: the list has no selectable text to end on")?;
    check("G", &info, &[last.as_str()], Some(info.rows - 1))?;
    if !info.focus_in {
        return Err(format!(
            "W11: after G, the caret's block reaches out of .message-list: {:?}",
            info.text
        ));
    }
    println!(
        "[panel_visual_mode] W11: G is on {:?}, the list's last character, in row {} of {}",
        info.text, info.anchor_row, info.rows
    );
    // `v` then `y` after `G` copies only the list's last selectable character, with nothing outside the list.
    h.clear_clipboard();
    h.xdotool_keys(&["v", "y"]);
    let copied = h
        .read_fresh_clipboard(2000)
        .ok_or("W11: G v y copied nothing")?;
    if copied != last {
        return Err(format!(
            "W11: G v y should copy the last character {last:?}, copied {copied:?}"
        ));
    }
    // Return to the prompt's first character, then use `j` to cross into the reply.
    // `.row-current` stays fixed during the selection region; `Esc` moves it to the caret's
    // row.
    h.xdotool_keys(&["g", "g", "v", "j"]);
    let info = caret_info(h)?;
    check("gg j", &info, &[], Some(1))?;
    println!(
        "[panel_visual_mode] W11: gg j crossed into the reply row, on {:?}",
        info.text
    );
    h.xdotool_key("Escape");
    let mode = h.mode()?;
    if mode.as_str() != Some("browse") {
        return Err(format!("W11: Esc did not return to browse: data-mode={mode}"));
    }
    let still_on_reply_row = h.eval(
        "JSON.stringify(document.querySelector('.row-current')?.className.includes('row-assistant') ?? false)",
        1000,
    )?;
    if still_on_reply_row != serde_json::Value::Bool(true) {
        return Err(format!(
            "W11: Esc should keep .row-current on the caret's own row: {still_on_reply_row}"
        ));
    }
    Ok(())
}

/// The list's scroll position, the caret's rect and the list's rect, and every `nv-user-scroll`
/// announcement recorded since `install_scroll_recorder`.
const VIEW_INFO_JS: &str = r#"JSON.stringify((() => {
  const list = document.querySelector('.message-list');
  const sel = window.getSelection();
  const range = document.createRange();
  range.setStart(sel.anchorNode, sel.anchorOffset);
  range.collapse(true);
  const c = range.getClientRects()[0] || range.getBoundingClientRect();
  const l = list.getBoundingClientRect();
  return { top: list.scrollTop, caretTop: c.top, caretBottom: c.bottom, listTop: l.top, listBottom: l.bottom,
           said: window.__nvScrollSaid || [] };
})())"#;

fn view_info(h: &Harness) -> Result<serde_json::Value, String> {
    h.eval(VIEW_INFO_JS, 1000)
}

fn num(v: &serde_json::Value, k: &str) -> f64 {
    v.get(k).and_then(serde_json::Value::as_f64).unwrap_or(f64::NAN)
}

/// Each key scrolls only enough to show the caret and reports any scroll to `follow.ts`. On the
/// long reply, `j` with room below neither scrolls nor reports a scroll. `40j` leaves the
/// caret's line at the list's bottom edge and reports the movement. `gg` scrolls up and reports
/// `up`, stopping following as BROWSE's `k` does.
// `!(a > b)` on purpose: a missing field reads as NaN, and a NaN must fail the check too.
#[allow(clippy::neg_cmp_op_on_partial_ord)]
fn w11_caret_view(h: &Harness) -> Result<(), String> {
    h.xdotool_keys(&["g", "g", "5", "j", "v"]);
    if h.mode()?.as_str() != Some("caret") {
        return Err(format!(
            "W11 view: g g 5 j v did not start CARET: data-mode={}",
            h.mode()?
        ));
    }
    // Room below the caret, whatever the zoom left on screen: `10j` reaches (or nears) the list's
    // bottom edge, `5k` climbs back up inside the view.
    h.xdotool_keys(&["1", "0", "j", "5", "k"]);
    h.eval(
        "JSON.stringify((() => { window.__nvScrollSaid = []; document.querySelector('.message-list').addEventListener('nv-user-scroll', (e) => window.__nvScrollSaid.push(e.detail)); return true; })())",
        1000,
    )?;
    let start = view_info(h)?;
    let line = num(&start, "caretBottom") - num(&start, "caretTop");
    if !(line > 0.0) || num(&start, "caretBottom") + 3.0 * line > num(&start, "listBottom") {
        return Err(format!(
            "W11 view: the entry caret should sit with room below it (harness precondition): {start}"
        ));
    }
    h.xdotool_key("j");
    let after_j = view_info(h)?;
    if num(&after_j, "top") != num(&start, "top") {
        return Err(format!("W11 view: a j on a line already in view moved the list (the least that shows it): {start} -> {after_j}"));
    }
    if after_j
        .get("said")
        .and_then(|s| s.as_array())
        .is_none_or(|a| !a.is_empty())
    {
        return Err(format!(
            "W11 view: a j that moved nothing announced a scroll: {after_j}"
        ));
    }
    h.xdotool_keys(&["4", "0", "j"]);
    let after_40 = view_info(h)?;
    let (bottom, list_bottom) = (num(&after_40, "caretBottom"), num(&after_40, "listBottom"));
    if !(num(&after_40, "top") > num(&after_j, "top")) {
        return Err(format!("W11 view: 40j did not scroll the list down: {after_40}"));
    }
    if bottom > list_bottom + 1.0 || bottom < list_bottom - 2.0 * line {
        return Err(format!(
            "W11 view: after 40j the caret's line should sit at the list's bottom edge (the least scroll), not elsewhere: {after_40}"
        ));
    }
    if after_40
        .get("said")
        .and_then(|s| s.as_array())
        .is_none_or(Vec::is_empty)
    {
        return Err(format!(
            "W11 view: 40j scrolled the list but announced nothing (follow.ts never learns): {after_40}"
        ));
    }
    h.xdotool_keys(&["g", "g"]);
    let after_gg = view_info(h)?;
    if !(num(&after_gg, "top") < num(&after_40, "top")) {
        return Err(format!("W11 view: gg did not scroll the list back up: {after_gg}"));
    }
    let said_up = after_gg
        .get("said")
        .and_then(|s| s.as_array())
        .is_some_and(|a| a.iter().any(|d| d.as_str() == Some("up")));
    if !said_up {
        return Err(format!(
            "W11 view: gg scrolled the list up but never announced \"up\": {after_gg}"
        ));
    }
    h.xdotool_key("Escape");
    println!(
        "[panel_visual_mode] W11 view: j moved nothing; 40j parked the caret {:.1}px above the list's bottom; gg said up",
        list_bottom - bottom
    );
    Ok(())
}

/// The composer's own `<textarea>` value (only rendered while `mode === "input"`, `Composer.tsx`'s
/// own return JSX -- absent otherwise, so callers of this only reach it right after a quote or a
/// real `i`/`o`/`A`/`Ctrl+j`).
fn composer_value(h: &Harness) -> Result<String, String> {
    let v = h.eval(
        "JSON.stringify(document.querySelector('textarea')?.value ?? null)",
        1000,
    )?;
    v.as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("no textarea value (is mode really \"input\"?): {v}"))
}

/// Clears whatever the composer's box holds, through the DOM (select from the start to the end, Delete)
/// rather than a Rust-side reset -- this harness has no direct route into the tab's own draft state, and a
/// real user clearing the box is exactly what this simulates. Not `Ctrl+a`: the composer binds it to the
/// line's start, as Claude Code's input does, so it no longer selects everything.
fn clear_composer(h: &Harness) {
    h.xdotool_keys(&["ctrl+Home", "ctrl+shift+End", "Delete"]);
}

/// A quote posts exactly one `draft` containing the box's whole text and sends nothing. Count
/// all drafts because `posted_type` reads only the first.
fn one_draft_nothing_sent(h: &Harness, text: &str, which: &str) -> Result<(), String> {
    let drafts: Vec<serde_json::Value> = h
        .posted
        .borrow()
        .iter()
        .filter_map(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .filter(|m| m.get("type").and_then(|t| t.as_str()) == Some("draft"))
        .collect();
    if drafts.len() != 1 {
        return Err(format!(
            "W12: {which} should post exactly one draft, posted {}: {drafts:?}",
            drafts.len()
        ));
    }
    if drafts[0].get("text").and_then(|t| t.as_str()) != Some(text) {
        return Err(format!(
            "W12: {which}'s posted draft did not match the box ({text:?}): {}",
            drafts[0]
        ));
    }
    if h.posted_type("send").is_some() || h.posted_type("send_now").is_some() {
        return Err(format!("W12: {which} sent something -- it never does"));
    }
    Ok(())
}

/// `>` in VISUAL or V-LINE quotes the highlighted text into the tab's draft.
fn w12_quote(h: &Harness) -> Result<(), String> {
    // `v v w e >` quotes "The quick", the same inclusive character span copied by `v v w e y`.
    h.clear_posted();
    h.xdotool_keys(&["g", "g", "v", "v", "w", "e", "greater"]);
    if h.mode()?.as_str() != Some("input") {
        return Err(format!("W12: > did not enter INPUT: data-mode={}", h.mode()?));
    }
    let focused_textarea = h.eval(
        "JSON.stringify(document.activeElement === document.querySelector('textarea'))",
        1000,
    )?;
    if focused_textarea != serde_json::Value::Bool(true) {
        return Err(format!(
            "W12: > did not focus the composer's textarea: {focused_textarea}"
        ));
    }
    let first = composer_value(h)?;
    if first != "> The quick\n\n" {
        return Err(format!(
            "W12: expected the box to hold \"> The quick\\n\\n\", got {first:?}"
        ));
    }
    let caret_at_end = h.eval(
        "JSON.stringify((() => { const t = document.querySelector('textarea'); return t ? t.selectionStart === t.value.length : false; })())",
        1000,
    )?;
    if caret_at_end != serde_json::Value::Bool(true) {
        return Err(format!(
            "W12: the caret should be at the end of the quoted text: {caret_at_end}"
        ));
    }
    pump(400); // DRAFT_MIRROR_DELAY_MS (300ms) plus slack, for the debounced `draft` post.
    one_draft_nothing_sent(h, &first, "the first quote")?;
    println!(
        "[panel_visual_mode] W12: v v w e > quoted \"The quick\", posted exactly one matching draft, sent nothing"
    );

    // The second `V j >` spans the reply's Chinese paragraph and the code block's first line.
    // `toString()` inserts a blank line between the paragraph and the block. Quoting turns that
    // blank into a bare `>` line to keep one blockquote. A selection across the prompt and
    // reply joins with only one newline and would not exercise this case.
    h.xdotool_key("Escape"); // back to BROWSE
    h.clear_posted();
    h.xdotool_keys(&["g", "g", "j", "V", "j", "greater"]);
    let second = composer_value(h)?;
    pump(400);
    one_draft_nothing_sent(h, &second, "the second quote")?;
    if !second.starts_with(&first) {
        return Err(format!(
            "W12: the second quote should stay below the first ({first:?}): {second:?}"
        ));
    }
    let appended = &second[first.len()..];
    if !appended.starts_with("> ") || !appended.ends_with("\n\n") {
        return Err(format!(
            "W12: the appended quote should be its own \"> \"-prefixed block ending in a blank line: {appended:?}"
        ));
    }
    if !appended.starts_with(&format!("> {CHINESE_PARAGRAPH}\n")) {
        return Err(format!(
            "W12: the second quote should start with the reply's paragraph as its own line: {appended:?}"
        ));
    }
    // The blank line between the two paragraphs is a bare `>` line, so the quote stays
    // one blockquote rather than two, and nothing lazily continues into it.
    let Some((_, after_blank)) = appended.split_once("\n>\n") else {
        return Err(format!(
            "W12: the blank line between the paragraph and the block below it should be a bare \">\" line: {appended:?}"
        ));
    };
    if !after_blank.starts_with("> ") || after_blank.trim_end().len() <= 2 {
        return Err(format!("W12: the quote should go on past its blank line: {appended:?}"));
    }
    if appended.lines().any(|l| !l.is_empty() && !l.starts_with('>')) {
        return Err(format!("W12: every quoted line should start with \">\": {appended:?}"));
    }
    println!("[panel_visual_mode] W12: a second V j > appended below the first: {appended:?}");

    // With `hello` typed first, the quote follows `hello\n\n` without changing the existing
    // text. Clear and type while still in INPUT after the second quote: `Composer` renders its
    // `<textarea>` only when `mode === "input"`. Returning to BROWSE first would unmount it
    // before select-all, `Delete`, and typing could reach it.
    clear_composer(h);
    h.xdotool_type("hello");
    h.xdotool_key("Escape");
    h.xdotool_keys(&["g", "g", "v", "v", "l", "greater"]);
    let third = composer_value(h)?;
    if !third.starts_with("hello\n\n>") {
        return Err(format!(
            "W12: with \"hello\" typed first, the quote should follow \"hello\\n\\n\": {third:?}"
        ));
    }
    println!("[panel_visual_mode] W12: with \"hello\" typed first, the quote followed it: {third:?}");

    // `>` must move the row cursor to the selection's start row. Start BROWSE on the reply
    // (`row-assistant`), then extend VISUAL backward with `gg` to the prompt (`row-prompt`).
    // The selection starts on the opposite row from the initial BROWSE cursor. If
    // `landCursorOnRowKey` did nothing in the `vquote` arm, `.row-current` would remain on the
    // reply because nothing else moves the BROWSE cursor during the region.
    h.xdotool_key("Escape"); // back to BROWSE, from the composer left open by the check above
    h.xdotool_keys(&["g", "g", "j"]); // BROWSE: onto the reply row
    let on_reply = h.eval(
        "JSON.stringify(document.querySelector('.row-current')?.className.includes('row-assistant') ?? false)",
        1000,
    )?;
    if on_reply != serde_json::Value::Bool(true) {
        return Err(format!(
            "W12: g g j should land BROWSE on the reply row before this check even starts: {on_reply}"
        ));
    }
    h.xdotool_keys(&["v", "v", "g", "g", "greater"]);
    if h.mode()?.as_str() != Some("input") {
        return Err(format!(
            "W12: v v gg > did not enter INPUT: data-mode={}",
            h.mode()?
        ));
    }
    let landed_on_prompt = h.eval(
        "JSON.stringify(document.querySelector('.row-current')?.className.includes('row-prompt') ?? false)",
        1000,
    )?;
    if landed_on_prompt != serde_json::Value::Bool(true) {
        return Err(format!(
            "W12: > should move the row cursor to the selection's start row (the prompt row), not leave it on the reply row it started from: {landed_on_prompt}"
        ));
    }
    println!("[panel_visual_mode] W12: > moved the row cursor to the selection's start row (the prompt row), not the reply row it started from");
    Ok(())
}

/// W13: a `<tr>` made taller than
/// `MAX_LINE_PROBE_PX` (200px, `visual.ts`) by a wrapped neighbouring cell used to leave `j` stuck on
/// the caret's own short cell ("target"), unable to reach the row below ("next") -- `stepByLinePoint`
/// alone gives up within that budget. `visual.ts`'s new `stepToAdjacentTableRow` steps by DOM
/// position instead (the caret's `<tr>` to its next/previous sibling, same column), which cannot be
/// defeated by a row's real rendered height. `visual.test.ts`'s own unit tests exercise the same
/// function geometry-free (jsdom cannot measure a real row's height at all); this is the real-WebKit
/// proof the row this depends on is genuinely taller than the probe's own budget, measured directly,
/// not assumed.
fn w13_tall_table_row(h: &Harness) -> Result<(), String> {
    let row_height = h.eval(
        r#"JSON.stringify((() => {
      const list = document.querySelector('.message-list');
      const walker = document.createTreeWalker(list, NodeFilter.SHOW_TEXT);
      let node;
      while ((node = walker.nextNode())) {
        if (node.textContent.trim() === 'target') {
          const tr = node.parentElement && node.parentElement.closest('tr');
          return tr ? tr.getBoundingClientRect().height : null;
        }
      }
      return null;
    })())"#,
        1000,
    )?;
    let height = row_height
        .as_f64()
        .ok_or_else(|| format!("W13: could not measure the wrapped-cell row's own height: {row_height}"))?;
    if height <= 200.0 {
        return Err(format!(
            "W13: the wrapped-cell row must render taller than MAX_LINE_PROBE_PX (200px) for this \
             case to mean anything -- measured {height}px, the wrap did not happen as expected"
        ));
    }
    println!("[panel_visual_mode] W13: the wrapped-cell row measured {height}px tall (> 200px)");

    h.xdotool_keys(&["g", "g", "5", "j", "v", "j"]);
    let onto_target = caret_info(h)?;
    if onto_target.text != "t" {
        return Err(format!(
            "W13: v then j from the header should land on \"target\"'s own \"t\": got {:?}",
            onto_target.text
        ));
    }
    // The bug itself: from a SHORT cell inside a row a wrapped neighbour made tall, one more `j`
    // must still find the next row -- the pixel probe alone cannot, since it stops well short of
    // this row's own real (>200px) height.
    h.xdotool_key("j");
    let onto_next = caret_info(h)?;
    if onto_next.text != "n" {
        return Err(format!(
            "W13: j from \"target\" (inside the tall row) should reach \"next\"'s own \"n\", not stay stuck: got {:?}",
            onto_next.text
        ));
    }
    println!("[panel_visual_mode] W13: j crossed the tall row's own bottom, from \"target\" to \"next\"");
    h.xdotool_key("Escape");
    if h.mode()?.as_str() != Some("browse") {
        return Err(format!("W13: Escape should return to browse: data-mode={}", h.mode()?));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------

type Case = (&'static str, fn(&Harness) -> Result<(), String>);

/// Opens one window on `replay`, runs `cases` in order (W0 first, and nothing else if it fails --
/// every other failure would be the harness's, not VISUAL's), then closes it.
fn run_window(
    zoom: f64,
    which: &str,
    display: &str,
    replay: Vec<String>,
    cases: &[Case],
) -> Vec<(String, Result<(), String>)> {
    let title = format!("panel_visual_mode zoom {zoom} {which}");
    let h = match Harness::new(zoom, title, display, replay) {
        Ok(h) => h,
        Err(e) => return vec![(format!("{which} window (zoom {zoom})"), Err(format!("harness: {e}")))],
    };
    let mut results = Vec::new();
    let preflight = w0_keys_reach_the_page(&h);
    let preflight_ok = preflight.is_ok();
    results.push((format!("W0 keys reach the page, {which} (zoom {zoom})"), preflight));
    if preflight_ok {
        for (name, f) in cases {
            results.push((format!("{name} (zoom {zoom})"), f(&h)));
        }
    } else {
        for (name, _) in cases {
            results.push((format!("{name} (zoom {zoom})"), Err("not run: W0 failed".into())));
        }
    }
    h.close();
    results
}

fn run_zoom(zoom: f64, display: &str) -> Vec<(String, Result<(), String>)> {
    let main_cases: Vec<Case> = vec![
        ("W1 vacuity", w1_vacuity),
        ("W2 English and Chinese words", w2_english_and_chinese_words),
        ("W3 linewise prompt and reply", w3_linewise_prompt_and_reply),
        ("W4 code block after a HINT landing", w4_code_block_after_hint),
        ("W6 tall tool output", w6_tall_tool_output),
        ("W7 permission card", w7_permission_card),
        ("W8 Escape", w8_escape),
        ("W9 streaming", w9_streaming),
        ("W10 counts", w10_counts),
        ("W11 caret motions", w11_caret_motions),
        ("W12 quote", w12_quote),
    ];
    let fixture_cases: Vec<Case> = vec![
        ("W4 code-only reply", w4_code_block_row),
        ("W5 table", w5_table),
        ("W10 long transcript timing", w10_long_transcript_timing),
        ("W11 caret view", w11_caret_view),
        ("W13 tall table row", w13_tall_table_row),
    ];
    let follow_cases: Vec<Case> = vec![
        ("W14 expand while following", w14_expand_while_following),
        ("W16 composer line edges", w16_composer_line_edges),
    ];
    let cards_cases: Vec<Case> = vec![("W15 card landing while following", w15_card_landing_while_following)];
    // `PANEL_VISUAL_ONLY=follow,cards` runs just those windows (names as below); unset runs them all.
    let only = std::env::var("PANEL_VISUAL_ONLY").ok();
    let wanted = |name: &str| only.as_deref().is_none_or(|list| list.split(',').any(|n| n == name));
    let mut results = Vec::new();
    if wanted("main") {
        results.extend(run_window(zoom, "main", display, replay_main(), &main_cases));
    }
    if wanted("fixtures") {
        results.extend(run_window(zoom, "fixtures", display, replay_fixtures(), &fixture_cases));
    }
    if wanted("follow") {
        results.extend(run_window(
            zoom,
            "follow",
            display,
            replay_follow_start().0,
            &follow_cases,
        ));
    }
    if wanted("cards") {
        results.extend(run_window(zoom, "cards", display, replay_cards_start().0, &cards_cases));
    }
    results
}

fn main() {
    if let Err(e) = panel_base_uri_matches_product() {
        eprintln!("panel_visual_mode: {e}");
        std::process::exit(1);
    }
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("panel_visual_mode: ignored (drives a real WebKitGTK WebView); run with `-- --ignored`");
        return;
    }
    // The harness refuses a DISPLAY it did not start -- so it starts one, and nothing below
    // ever reads the inherited one. GTK's own built-in input method, never the one the desktop session
    // names: an inherited `GTK_IM_MODULE=fcitx` routed every key of this window through
    // the user's live fcitx5 daemon over the session bus -- asynchronously, so a `/` search's `Return`
    // still sat in flight when the next key arrived (W6/W7 failed intermittently, about one run in
    // three, with the search prompt still open and the following `v` typed into it), and with the
    // a user's rime switched on a letter would have been composed, not typed. The IME half is the GUI
    // pass's. `init_gtk` sets both, and refuses unless GDK opened this server's display.
    let server = own_x_server::init_gtk("panel_visual_mode", "1280x900x24");
    // After `init_gtk`, not before (since 2026-09-29): the probe then inherits this test's own
    // display and no other. Today's `xdotool version` connects to nothing, but a release that opens its
    // display before reading the subcommand would otherwise have reached the session's.
    if Command::new("xdotool").arg("version").output().is_err() {
        eprintln!("panel_visual_mode: `xdotool` is not on PATH -- install it (this test sends every key through it)");
        server.exit(1);
    }

    let code = run(server.display());
    // Every window is destroyed by now and nothing iterates the main loop after this point, so GDK
    // never reads its dropped X connection (its I/O error handler would exit with its own status).
    // Closing the GdkDisplay first was the alternative, and was not taken: WebKit may still hold
    // display-bound resources at this point, and a crash in teardown would read as a failed run.
    server.exit(code);
}

fn run(own_display: &str) -> i32 {
    let mut failed: Vec<String> = Vec::new();
    for zoom in [1.0, 1.5] {
        for (label, result) in run_zoom(zoom, own_display) {
            match result {
                Ok(()) => println!("[{label}] pass"),
                Err(e) => {
                    println!("[{label}] FAIL: {e}");
                    failed.push(format!("{label}: {e}"));
                }
            }
        }
    }

    println!();
    if failed.is_empty() {
        println!(
            "panel_visual_mode: every case passed (W5's exact string and W10's timings are printed above to record)"
        );
        0
    } else {
        println!("panel_visual_mode: {} failure(s):", failed.len());
        for f in &failed {
            println!("  - {f}");
        }
        1
    }
}
