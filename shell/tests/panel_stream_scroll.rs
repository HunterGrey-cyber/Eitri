//! Tests that streaming replies stay at the bottom while following and stay put while reading,
//! using the WebKitGTK engine linked by `shell`.
//!
//! With `.row { container-type: inline-size }`, WebKitGTK 2.52.6 reset `.message-list`'s
//! `scrollTop` to a stale value on each tick of the status line's elapsed counter.
//! `MessageList`'s `onScroll` treated that drop as user input and stopped following.
//! jsdom has no layout, and Chromium does not reproduce the movement, so this test uses
//! `webkit6` with the document embedded by `agent_panel.rs` and the product's bridge serializers.
//! It calls `evaluate_javascript` once per 33ms tick, matching the pump's cadence.
//!
//! Each scenario runs on 560x740 and 519x480 CSS-px panels at zoom 1.0 and 1.5.
//! The window size is the CSS size times the zoom.
//!
//! S1 checks following with no input. A scroll event that reports a lower `scrollTop` must
//! leave the list at its true bottom: without input, a drop is either a bottom clamp or a bug.
//! The view must never spend more than 250ms over 50px short of the end, and must end at the bottom.
//! S1(c) additionally checks every following frame in S1 and S6 against `AT_BOTTOM_PX`.
//! This catches smooth scrolling that leaves a newly mounted reply below the edge for a frame.
//!
//! S2 parks the reader inside the second assistant message once it is 1.5 screens tall,
//! using a wheel event and its resulting scroll. After a 300ms settle, no scroll event,
//! frame's `scrollTop`, or text offset at the reading point may move for the rest of the turn.
//! Only plain paragraphs are appended after parking, so markdown restructuring above the
//! reader is outside this check. S3 parks in older history before the first text arrives
//! and checks the same stability, guarding against changes that move parked readers.
//!
//! S4 writes `scrollTop = scrollHeight` every 400ms to re-arm following; S1's scroll check applies.
//! S5 parks with the browser's PageUp default action on a focused control inside the list.
//! The live tool call's row uses `tabindex=-1` because the replay has no focusable row element.
//! S2's stability checks apply. Tracking only wheels, pointers, touch drags, and panel keys
//! misses this input and can pull the reader back to the bottom on the next delta.
//!
//! S6 sends eight sideways wheels with `deltaX: -40` and sub-pixel vertical jitter once
//! message 2 is half a screen tall. The list does not move, and S1's checks apply throughout.
//! A negative `deltaY` alone must not stop following when the gesture does not scroll up.
//!
//! S7 cancels a wheel up at the tail after `MessageList` has seen it, so it moves nothing.
//! A live tool call's row arrives during the gesture, followed by about 2s of silence.
//! The replay is quiet for 330ms before the call so no earlier snap event is pending.
//! The view must return to the bottom within `S7_CATCH_UP_LIMIT_MS` of the wheel,
//! when the gesture ends. Waiting for the tool result leaves the new row hidden during the pause.
//! S1's scroll check and the final distance check also apply.
//!
//! S8 sends a wheel up (`S8_WHEEL_DELTA`) just before a text delta once message 2 is
//! half a screen tall. WebKitGTK animates the scroll, but can first report no movement.
//! That event is dispatched explicitly so every run exercises it. It must not re-arm
//! following and let the delta's snap cancel the wheel. From `S8_SETTLE_MS` after the wheel
//! until the turn ends, the view must stay at least half the wheel's distance above its start.
//!
//! S9 changes layout four times after the turn ends while the view is following at the bottom.
//! The changes are a second apart and have no streaming state change to trigger a snap.
//! The window narrows to `S9_NARROW` of its width, then shortens to `S9_SHORTEN`
//! of its height. A theme envelope raises the font size through `serialize_theme_for_js`.
//! Setting `--prose-measure` to `S9_PROSE_MEASURE` on `:root` changes only the rows.
//! Each change must return the view to the end by the second sampled frame
//! (`S9_MAX_SHORT_FRAMES`) and keep it there. Snapping only on state changes misses these resizes.
//!
//! Every scenario checks that `.message-list`'s `scrollWidth` stays within `SIDEWAYS_PX`
//! of its `clientWidth`. Long tokens exercise overflow: `LIVE_COMMAND` in a live `Bash` call,
//! `LONG_PATH` in an earlier `Read`, and `LONG_URL` in earlier reply prose.
//! The single frame sampled while the list's width changes is excluded by `judge_sideways`.
//!
//! Re-snapping before paint can hide the size-container bug from S1 and S4 by correcting
//! the drop before its scroll event. S2 exposes it while the reader is parked, so keep that
//! coverage. `indexCss.test.ts` provides a guard that does not need a display.
//!
//! A park stores `scrollIndex`, the number of scroll events already dispatched.
//! Only later events can count as moving the parked reader. A previous delta's snap can
//! dispatch its event just before the park's frame callback in the same rendering update.
//! WebKitGTK's 1ms clock can give both the same timestamp, so `t >= park.t` counts an old
//! position as a new movement. Event order avoids this false failure without hiding a later yank.
//! `judge_self_test` runs without a display on every invocation, including without `--ignored`.
//! Thus `cargo test --workspace` checks the timestamp tie, a later yank, and a missing index.
//!
//! Each run must exercise its trigger. History must be three screens tall and initially
//! at the bottom, streaming must grow it by two screens, and the elapsed counter must tick
//! at least five times during streaming. S2/S3/S5 must park with at least two counter ticks
//! afterward. S5 requires focus inside the list at least 500ms before PageUp so focus itself
//! cannot explain the park. S6 requires all eight wheels while following with a screen of growth left.
//! S7 requires a cancelled wheel at the bottom, a new row that leaves the view short without
//! wheel movement, and `S7_MIN_PAUSE_MS` of silence. S8 requires its wheel while following
//! with a screen of growth left. S9 requires each change to move the end by at least `HIDDEN_PX`
//! and each resize to change the list's box. Each long token must exceed the list's width
//! when measured on one line in its rendered font.
//! Missing a trigger fails as "did not exercise the trigger". A reading point between paragraphs
//! can resolve to the row body, whose offset tracks only `scrollTop`; the scroll checks still cover it.
//!
//! `support/own_x_server.rs` starts an isolated `Xvfb` with a 1920x1200 screen and no input devices.
//! Inherited `WAYLAND_DISPLAY` and `DISPLAY` are dropped before GTK initialises, so this command
//! cannot open windows on the invoking desktop:
//!
//!     cargo test -p shell --test panel_stream_scroll -- --ignored
//!
//! A 640x480 screen clamps the 1110px-tall window and fails the trigger guard instead of testing
//! the intended size. The test uses a plain `main` (`harness = false`) because GTK needs the main thread.
//! Frame and scroll traces go to `$PANEL_STREAM_SCROLL_TRACE_DIR`, defaulting to
//! `<target tmp>/panel_stream_scroll/`. `PANEL_STREAM_SCROLL_ONLY=S1,S2` selects scenarios.
//! The configuration matrix is fixed. Each run takes about 15s, with thirty-six runs.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use agent::{AgentDomainEvent, AgentSessionProjection, ContentKind, TurnOutcome};
use eitri_core::agent_backend::{BackendGreeting, BackendKind, ProjectionRef, CLIENT_IMPLEMENTED_PERMISSION_MODES};
use eitri_core::agent_bridge::{
    parse_inbound_message, serialize_command_result_for_js, serialize_events_for_js, serialize_hello_for_js,
    serialize_pane_focus_for_js, serialize_snapshot_for_js, serialize_tabs_for_js, serialize_theme_for_js,
    InboundMessage, SessionModeChoice, SnapshotView, TabStateWire, TabView,
};
use eitri_core::theme::ThemeTokens;
use gtk4::glib;
use gtk4::prelude::*;
use serde_json::{json, Value};
use webkit6::prelude::*;
use webkit6::{UserContentInjectedFrames, UserContentManager, UserScript, UserScriptInjectionTime, WebView};

// The display this test runs on: its own Xvfb, never an inherited one (2026-09-29).
#[path = "support/own_x_server.rs"]
mod own_x_server;

/// The document `shell/src/agent_panel.rs` embeds, byte for byte: the same `include_str!` of the
/// same file, which `shell/build.rs` rebuilds before this test compiles.
const AGENT_UI_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../agent-ui/web/dist/index.html"));

/// The pump's cadence (`agent_panel.rs`'s 33ms timer), and so this replay's.
const TICK: Duration = Duration::from_millis(33);
/// How long the history sits before the turn starts: long enough for the snapshot's own smooth
/// scroll to the end to finish, so "at the bottom" is a precondition rather than a race.
const SETTLE_AFTER_SNAPSHOT_TICKS: usize = 30;
const SETTLE_AFTER_TURN_TICKS: usize = 30;
/// What a parked reader is allowed before "nothing moves" starts being checked.
const PARK_SETTLE_MS: f64 = 300.0;
/// S1(b): the longest the newest text may be out of view. Room for the new-row smooth scroll,
/// which takes 100-160ms in both engines. (The GUI-pass fix: a new row snaps too now, so S1(c)
/// holds every frame to `AT_BOTTOM_PX`; this looser bound stays as the broader net.)
const MAX_HIDDEN_STRETCH_MS: f64 = 250.0;
const HIDDEN_PX: f64 = 50.0;
/// "At the true bottom": sub-pixel rounding at a fractional zoom, nothing more.
const AT_BOTTOM_PX: f64 = 1.0;
/// Below this a `scrollTop` change is rounding, not movement (fractional zoom).
const MOVED_PX: f64 = 0.5;
const MIN_CLOCK_TICKS_WHILE_STREAMING: usize = 5;
const MIN_CLOCK_TICKS_WHILE_PARKED: usize = 2;
const RUN_TIMEOUT: Duration = Duration::from_secs(120);
/// S5: how long before the PageUp the control inside the list must already hold focus -- well past
/// `MessageList`'s 300ms steering window, which focus landing inside the list opens by itself.
const MIN_FOCUS_BEFORE_KEY_MS: f64 = 500.0;
/// S6: the wheel events in the sideways swipe (the instrument's `SWIPE_EVENTS`).
const SWIPE_EVENTS: f64 = 8.0;
/// How long the live `Bash` call runs between its start and its result: 19 ticks, about 630ms, in
/// every scenario but S7.
const TOOL_RUN_TICKS: usize = 19;
/// S7's tool call runs for 60 ticks (about 2s) instead, so that "caught up when the gesture ended"
/// and "caught up at the next state change" are a second and a half apart rather than 300ms.
const S7_TOOL_RUN_TICKS: usize = 60;
/// And S7 has 10 quiet ticks (about 330ms) between message 1's last delta and the tool call, so no
/// snap's scroll event is still on its way when the wheel comes (every other scenario: none).
const S7_QUIET_BEFORE_TOOL_TICKS: usize = 10;
/// S8: the wheel's `deltaY`, in device px; WebKitGTK scrolls it by `-S8_WHEEL_DELTA / zoom` CSS px.
const S8_WHEEL_DELTA: f64 = 120.0;
/// S8: from this long after the wheel, its animated scroll (about 200ms) must have landed and stayed.
const S8_SETTLE_MS: f64 = 400.0;
/// S7: how soon after the wheel the view must be back at its bottom. The steering window (300ms)
/// plus a new row's smooth scroll (100-160ms, measured), with room for a loaded machine -- and well
/// short of the tool result, the next state change, which comes about 2s after the wheel.
const S7_CATCH_UP_LIMIT_MS: f64 = 1000.0;
/// S7: the replay's pause after the new row must outlast the limit above by this much, or a catch-up
/// at the next state change could pass for one at the end of the gesture.
const S7_MIN_PAUSE_MS: f64 = 1500.0;
/// The live `Bash` call's command: 112 characters with spaces in it, about the length of the
/// 116-character one the sandbox GUI pass (2026-09-24) saw scroll the whole conversation sideways
/// (517/377/37px at 420/560/900px panels, identical on `main`).
const LIVE_COMMAND: &str =
    "cargo test -p agent-ui-width -- --nocapture --test-threads=1 --include-ignored and-a-few-more-args-so-it-is-long";
/// An earlier `Read` of a path with no space in it -- a real absolute path in this repository is
/// often this long. Outside the harness's own project root (`/nonexistent/panel-stream-scroll`,
/// 2026-09-29): `projectPath.ts` draws a path under the root
/// relative to it, so the token search below, which looks for this exact text, found nothing and
/// every scenario read "the long path was not found". A path outside the root is drawn as sent, and
/// this one is exactly as long as the old one.
const LONG_PATH: &str =
    "/nonexistent/outside-the-project/a/deeply/nested/directory/with/no/space/in/it/src/components/MessageList.tsx";
/// A URL in an earlier reply's prose, which `marked` turns into a link.
const LONG_URL: &str =
    "https://docs.example.com/reference/a/long/path/with/no/space/in/it/anywhere/index.html?query=abcdefghijklmnopqrstuvwxyz";
/// How far `.message-list`'s content may reach past its own width: sub-pixel rounding, nothing more.
const SIDEWAYS_PX: f64 = 1.0;
/// S9: idle ticks after each of its four changes (about 1s).
const S9_PAUSE_TICKS: usize = 30;
/// S9: the window's width and then height, as fractions of the configuration's own.
const S9_NARROW: f64 = 0.62;
const S9_SHORTEN: f64 = 0.75;
/// S9: the panel font size the theme push sets (Rust's default is 14px).
const S9_FONT_SIZE_PX: f32 = 18.0;
/// S9: the documented prose-measure knob, set narrower than any configuration's rows -- after the
/// narrowing and the font push, which is when it is applied. (30ch was not: at 18px it is wider than a
/// 347px panel's row, and moved nothing, found by the vacuity guard.)
const S9_PROSE_MEASURE: &str = "16ch";
/// S9: how many sampled frames after each change may show the view short of its end. One: the
/// instrument samples in `requestAnimationFrame`, which runs before the rendering update's layout and
/// `ResizeObserver` callbacks, so the first frame after a change reads the new layout under the old
/// `scrollTop` -- a state the page's observer corrects before that frame is painted. Counted in frames,
/// not milliseconds: relaying out a 46,000px conversation on llvmpipe spaces the frames 30-110ms
/// apart, so a clock limit would measure the machine (the first version's 250ms came within 105ms of
/// failing a correct build).
const S9_MAX_SHORT_FRAMES: usize = 1;

/// A panel size in CSS px -- what the page lays out in -- and the page zoom it is drawn at. The
/// window is `size x zoom` device px, so every zoom lays out the same CSS viewport.
#[derive(Clone, Copy, Debug)]
struct Config {
    width: i32,
    height: i32,
    zoom: f64,
}

impl Config {
    fn window_size(self) -> (i32, i32) {
        (
            (f64::from(self.width) * self.zoom).round() as i32,
            (f64::from(self.height) * self.zoom).round() as i32,
        )
    }
}

const fn config(width: i32, height: i32, zoom: f64) -> Config {
    Config { width, height, zoom }
}

const CONFIGS: [Config; 4] = [
    config(560, 740, 1.0),
    config(560, 740, 1.5),
    config(519, 480, 1.0),
    config(519, 480, 1.5),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    S1,
    S2,
    S3,
    S4,
    S5,
    S6,
    S7,
    S8,
    S9,
}

impl Scenario {
    const ALL: [Scenario; 9] = [
        Scenario::S1,
        Scenario::S2,
        Scenario::S3,
        Scenario::S4,
        Scenario::S5,
        Scenario::S6,
        Scenario::S7,
        Scenario::S8,
        Scenario::S9,
    ];

    fn name(self) -> &'static str {
        match self {
            Scenario::S1 => "S1",
            Scenario::S2 => "S2",
            Scenario::S3 => "S3",
            Scenario::S4 => "S4",
            Scenario::S5 => "S5",
            Scenario::S6 => "S6",
            Scenario::S7 => "S7",
            Scenario::S8 => "S8",
            Scenario::S9 => "S9",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The replay: real `agent` values, folded by the real projection, serialized by the real bridge.
// ---------------------------------------------------------------------------------------------

/// A deterministic generator, so every run replays byte-identical text in byte-identical chunks.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next() % (hi - lo + 1) as u64) as usize
    }
}

/// A rich reply: headings, a bullet list, a fenced Rust block and paragraphs, Chinese and English.
fn rich_reply(seed: u64, min_chars: usize) -> String {
    let mut rng = Rng(seed | 1);
    let mut out = String::from("I looked at the three places that decide this. 先说结论，再给出证据。\n\n");
    let mut section = 0;
    while out.chars().count() < min_chars {
        section += 1;
        out.push_str(&format!("## 第 {section} 部分: what the code does\n\n"));
        out.push_str(
            "The handler runs once per batch, not once per event, which matters because the batch \
             size is not fixed. 每一批事件的大小都不一样，所以不能假设一次只增长一行。\n\n",
        );
        for i in 0..5 {
            if i % 2 == 0 {
                out.push_str(&format!(
                    "- item {i}: a longer point that wraps onto a second line in a narrow panel, 这在窄面板里很常见\n"
                ));
            } else {
                out.push_str(&format!("- item {i}: a short point\n"));
            }
        }
        out.push_str("\n```rust\n");
        for i in 0..8 {
            out.push_str(&format!(
                "fn step_{section}_{i}(x: u32) -> u32 {{ x.wrapping_mul({}) + {i} }}\n",
                rng.range(2, 99)
            ));
        }
        out.push_str("```\n\n");
        out.push_str(
            "And a closing paragraph for this section, long enough to wrap a few times so that the \
             growth per delta is realistic rather than one line at a time. 这一段也足够长，会换好几行。\n\n",
        );
    }
    out
}

/// Plain paragraphs only: nothing a later delta can restructure above a reader parked inside it.
/// No paragraph starts with a character markdown would read as a list, heading, quote or fence.
fn plain_reply(seed: u64, min_chars: usize) -> String {
    let sentences = [
        "The view should stay exactly where the reader left it while more text arrives below.",
        "新的文字只会追加在下面，上面已经显示的段落不应该移动哪怕一个像素。",
        "A browser keeps a reader's place with scroll anchoring, which this engine ships turned off.",
        "所以这里靠的是不让引擎自己去改 scrollTop，而不是事后把它改回来。",
        "Each delta replaces the whole message's markup, and the layout of the unchanged text is identical.",
        "状态栏的秒数每秒变一次，这正是以前会把视图拉回固定位置的那个触发条件。",
        "Following is decided by what the user did, never by which way the scroll offset happened to move.",
        "如果读者停在回复中间，他看到的那一行必须一直停在同一个位置。",
    ];
    let mut rng = Rng(seed | 1);
    let mut out = String::new();
    while out.chars().count() < min_chars {
        let n = rng.range(3, 5);
        let mut paragraph = Vec::with_capacity(n);
        for _ in 0..n {
            paragraph.push(sentences[rng.range(0, sentences.len() - 1)]);
        }
        out.push_str(&paragraph.join(" "));
        out.push_str("\n\n");
    }
    out
}

/// Split into 6-28 character deltas, grouped 1-5 per tick -- the shapes the code key measured.
fn ticks_of(text: &str, rng: &mut Rng) -> Vec<Vec<String>> {
    let chars: Vec<char> = text.chars().collect();
    let mut deltas = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let n = rng.range(6, 28).min(chars.len() - i);
        deltas.push(chars[i..i + n].iter().collect::<String>());
        i += n;
    }
    let mut groups = Vec::new();
    let mut j = 0;
    while j < deltas.len() {
        let k = rng.range(1, 5).min(deltas.len() - j);
        groups.push(deltas[j..j + k].to_vec());
        j += k;
    }
    groups
}

fn numbered_lines(prefix: &str, n: usize) -> String {
    (1..=n).map(|i| format!("{prefix} {i:>3}: output line {i}\n")).collect()
}

enum Step {
    /// One envelope, delivered by this file's own `evaluate_js_dispatch`, which mirrors the host's `PageSurface::send`.
    Dispatch(String),
    Idle,
    /// S9: the window resized to these fractions of the configuration's own size, the way a divider
    /// drag, a zoom or `F11` resizes the WebView -- `set_default_size` on a window already shown.
    Resize {
        width: f64,
        height: f64,
    },
    /// S9: a script run in the page as it stands, outside the dispatch path -- what the GUI pass did
    /// through the inspector to set `--prose-measure`.
    Script(String),
}

struct Replay {
    /// The panel's `ready` batch, in `agent_panel.rs`'s order (hello, theme, snapshot, pane focus),
    /// then the `command_result` answering `ready` itself.
    on_ready: Vec<String>,
    steps: Vec<Step>,
}

/// `quiet_before_tool_ticks`: idle ticks between message 1's last delta and the live tool call
/// (0, or S7's `S7_QUIET_BEFORE_TOOL_TICKS`); `tool_run_ticks`: idle ticks between that call's start
/// and its result (`TOOL_RUN_TICKS`, or S7's `S7_TOOL_RUN_TICKS`); `s9_tail`: S9's four layout
/// changes after the turn's settle. Nothing else differs between the three replays.
fn build_replay(quiet_before_tool_ticks: usize, tool_run_ticks: usize, s9_tail: bool) -> Replay {
    let turn = |t: &str| t.to_string();
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::SessionOpened {
        session_id: "sidecar-test".into(),
        provider_session_id: "claude-test".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/nonexistent/panel-stream-scroll".into(),
    });
    // Three earlier turns, folded by the real projection and shipped in the snapshot.
    for n in 0..3u64 {
        let turn_id = format!("t-history-{n}");
        for event in [
            AgentDomainEvent::UserPromptSubmitted {
                text: format!("earlier question {n}: why does the view move while a reply streams?"),
            },
            AgentDomainEvent::TurnStarted {
                turn_id: turn_id.clone(),
            },
            AgentDomainEvent::ContentDelta {
                turn_id: turn_id.clone(),
                kind: ContentKind::Text,
                text: rich_reply(100 + n, 1400),
            },
            AgentDomainEvent::ToolCallStarted {
                turn_id: turn_id.clone(),
                tool_use_id: format!("toolu_history_{n}"),
                name: "Read".into(),
                // The sideways check's long path, in the last history turn.
                input: json!({ "file_path": if n == 2 { LONG_PATH } else { "/nonexistent/panel-stream-scroll/src/main.rs" } }),
            },
            AgentDomainEvent::ToolCallCompleted {
                turn_id: turn_id.clone(),
                tool_use_id: format!("toolu_history_{n}"),
                content: json!(numbered_lines("main.rs", 48)),
                is_error: false,
            },
            AgentDomainEvent::ContentDelta {
                turn_id: turn_id.clone(),
                kind: ContentKind::Text,
                // The sideways check's long URL, in the last history turn's second reply.
                text: if n == 2 {
                    format!(
                        "The reference is {LONG_URL} and it says the same.\n\n{}",
                        rich_reply(200 + n, 900)
                    )
                } else {
                    rich_reply(200 + n, 900)
                },
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
    }

    let tokens = ThemeTokens::fallback();
    let greeting = BackendGreeting {
        kind: BackendKind::Sidecar,
        project_dir: PathBuf::from("/nonexistent/panel-stream-scroll"),
        permission_modes: CLIENT_IMPLEMENTED_PERMISSION_MODES,
        expected_verdandi_revision: Some(agent::EXPECTED_VERDANDI_REVISION),
        resumable: Vec::new(),
        account: None,
    };
    // This replay uses a single tab.
    let sole_tab = eitri_core::tabs::TabId(1);
    let snapshot = serialize_snapshot_for_js(
        sole_tab,
        &SnapshotView {
            backend: "sidecar",
            conversation_id: Some("conversation-test"),
            session_id: Some("sidecar-test"),
            provider_session_id: Some("claude-test".into()),
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
    let on_ready = vec![
        serialize_hello_for_js(&greeting),
        serialize_theme_for_js(&tokens),
        sole_tab_envelope(sole_tab),
        snapshot,
        serialize_pane_focus_for_js(true),
        // `request_id` is filled in when the page's own `ready` arrives; see `run_one`.
    ];

    let mut steps: Vec<Step> = Vec::new();
    for _ in 0..SETTLE_AFTER_SNAPSHOT_TICKS {
        steps.push(Step::Idle);
    }
    let mut batch = |events: Vec<AgentDomainEvent>, steps: &mut Vec<Step>| {
        let from = projection.last_revision;
        for event in &events {
            projection.apply(event);
        }
        steps.push(Step::Dispatch(serialize_events_for_js(
            sole_tab,
            from,
            projection.last_revision,
            &events,
        )));
    };
    let live = turn("t-live");
    batch(
        vec![
            AgentDomainEvent::UserPromptSubmitted {
                text: "why does the panel jump back while you are typing?".into(),
            },
            AgentDomainEvent::TurnStarted { turn_id: live.clone() },
        ],
        &mut steps,
    );
    for _ in 0..10 {
        batch(
            vec![AgentDomainEvent::ContentDelta {
                turn_id: live.clone(),
                kind: ContentKind::Thinking,
                text: "considering".into(),
            }],
            &mut steps,
        );
    }
    let mut rng = Rng(0x5eed_1234);
    let text_batch = |group: Vec<String>| -> Vec<AgentDomainEvent> {
        group
            .into_iter()
            .map(|text| AgentDomainEvent::ContentDelta {
                turn_id: "t-live".into(),
                kind: ContentKind::Text,
                text,
            })
            .collect()
    };
    // Message 1, rich markdown, about 3.6s of deltas.
    for group in ticks_of(&rich_reply(7, 5400), &mut rng) {
        batch(text_batch(group), &mut steps);
    }
    for _ in 0..quiet_before_tool_ticks {
        steps.push(Step::Idle);
    }
    batch(
        vec![AgentDomainEvent::ToolCallStarted {
            turn_id: live.clone(),
            tool_use_id: "toolu_live_1".into(),
            name: "Bash".into(),
            input: json!({ "command": LIVE_COMMAND }),
        }],
        &mut steps,
    );
    for _ in 0..tool_run_ticks {
        steps.push(Step::Idle);
    }
    batch(
        vec![AgentDomainEvent::ToolCallCompleted {
            turn_id: live.clone(),
            tool_use_id: "toolu_live_1".into(),
            content: json!(numbered_lines("test", 40)),
            is_error: false,
        }],
        &mut steps,
    );
    // Message 2, plain paragraphs only (S2's reader parks inside it), about 5s of deltas.
    for group in ticks_of(&plain_reply(11, 7600), &mut rng) {
        batch(text_batch(group), &mut steps);
    }
    batch(
        vec![AgentDomainEvent::TurnCompleted {
            turn_id: live.clone(),
            outcome: TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: Some("end_turn".into()),
            usage: None,
            detail: Default::default(),
        }],
        &mut steps,
    );
    for _ in 0..SETTLE_AFTER_TURN_TICKS {
        steps.push(Step::Idle);
    }
    if s9_tail {
        // S9: with the turn over and nothing streaming, four changes that move the list's end without
        // a state change behind them, each followed by a quiet second.
        let pause = |steps: &mut Vec<Step>| steps.extend((0..S9_PAUSE_TICKS).map(|_| Step::Idle));
        steps.push(Step::Resize {
            width: S9_NARROW,
            height: 1.0,
        });
        pause(&mut steps);
        steps.push(Step::Resize {
            width: S9_NARROW,
            height: S9_SHORTEN,
        });
        pause(&mut steps);
        let mut bigger = ThemeTokens::fallback();
        bigger.font_size_px = S9_FONT_SIZE_PX;
        steps.push(Step::Dispatch(serialize_theme_for_js(&bigger)));
        pause(&mut steps);
        steps.push(Step::Script(format!(
            "window.__mark('restyle'); document.documentElement.style.setProperty('--prose-measure', '{S9_PROSE_MEASURE}');"
        )));
        pause(&mut steps);
    }
    Replay { on_ready, steps }
}

/// `PanelDocument::themed`'s exact insertion rule (`panel/src/panel_document.rs`): the theme `<style>` goes directly after
/// the first `<head>`, so the first frame is already in the panel's colours.
fn themed_document(vars: &[(String, String)]) -> String {
    let declarations: String = vars.iter().map(|(name, value)| format!("{name}:{value};")).collect();
    let style = format!("<style id=\"nv-theme\">:root{{{declarations}}}</style>");
    let at = AGENT_UI_HTML.find("<head>").expect("the panel document has a <head>") + "<head>".len();
    format!("{}{}{}", &AGENT_UI_HTML[..at], style, &AGENT_UI_HTML[at..])
}

/// The host's `PageSurface::send` script, minus its guard for a page that has not installed its dispatcher.
fn evaluate_js_dispatch(webview: &WebView, json_payload: &str) {
    let script = format!(
        "window.__eitriDispatch({});",
        serde_json::to_string(json_payload).unwrap_or_default()
    );
    webview.evaluate_javascript(&script, None, None, None::<&gtk4::gio::Cancellable>, |result| {
        if let Err(e) = result {
            eprintln!("[panel_stream_scroll] evaluate_javascript failed: {e}");
        }
    });
}

// ---------------------------------------------------------------------------------------------
// The instrument: injected at document start, before the panel's own script runs.
// ---------------------------------------------------------------------------------------------

/// Records, per animation frame, the list's scroll geometry, the elapsed counter's text, the
/// document's own scroll offset, and the text under a fixed reading point 40px below the list's top
/// edge with its offset; and every `scroll` event on `.message-list`. It wraps
/// `window.__eitriDispatch` only to timestamp what arrived -- the panel receives the identical
/// string -- and performs the scenario's own reader actions. `window.__probe()` returns it all.
const INSTRUMENT: &str = r#"
(() => {
  const SCENARIO = "__SCENARIO__";
  const P = { scenario: SCENARIO, frames: [], scrolls: [], marks: [], park: null, rearms: 0, listChanges: 0,
              firstText: null, lastText: null, viewport: null, focused: null, swipe: null, gesture: null };
  const now = () => Math.round(performance.now() * 10) / 10;
  const mark = (name, extra) => P.marks.push(Object.assign({ t: now(), mark: name }, extra || {}));
  // S9's own steps mark themselves through this; a window resize marks itself below.
  window.__mark = mark;
  window.addEventListener("resize", () => mark("resize", { w: window.innerWidth, h: window.innerHeight }));
  // The sideways check's vacuity guard: how wide `text` is laid out on ONE line in the font `el` draws
  // it in. Wider than the list means that, had it not wrapped, it would have scrolled the list sideways.
  function oneLineWidth(el, text) {
    const cs = getComputedStyle(el);
    const span = document.createElement("span");
    span.style.cssText = "position:absolute;left:0;top:0;visibility:hidden;white-space:pre";
    span.style.fontFamily = cs.fontFamily;
    span.style.fontSize = cs.fontSize;
    span.style.fontWeight = cs.fontWeight;
    span.style.letterSpacing = cs.letterSpacing;
    span.textContent = text;
    document.body.appendChild(span);
    const w = span.getBoundingClientRect().width;
    span.remove();
    return Math.round(w * 10) / 10;
  }
  function token(text) {
    const l = list();
    if (!l) return null;
    const walker = document.createTreeWalker(l, NodeFilter.SHOW_TEXT);
    for (let n = walker.nextNode(); n; n = walker.nextNode()) {
      if (n.nodeValue.includes(text)) return { width: oneLineWidth(n.parentElement, text), list: l.clientWidth };
    }
    return { width: null, list: l.clientWidth };
  }
  window.__probe = () => {
    P.viewport = { innerWidth: window.innerWidth, innerHeight: window.innerHeight, dpr: window.devicePixelRatio };
    P.tokens = { command: token(__LIVE_COMMAND__), path: token(__LONG_PATH__), url: token(__LONG_URL__) };
    return JSON.stringify(P);
  };

  const list = () => document.querySelector(".message-list");
  // What a real wheel does: a `wheel` event on the element under the pointer, then the scroll it
  // causes, written here so it lands at an exact target. (Correction: this said a
  // synthetic `WheelEvent` scrolls nothing by itself. WebKitGTK 2.52.6 does perform it, animated over
  // about 200ms -- 300px at zoom 1 and 200 CSS px at zoom 1.5 for a `deltaY` of -300, measured. The
  // write straight after cancels that animation, which is why S2 still sees 0 frames off the park.)
  function park(target, why) {
    const l = list();
    l.dispatchEvent(new WheelEvent("wheel", { deltaY: -120, deltaMode: 0, bubbles: true, cancelable: true }));
    const before = l.scrollTop;
    l.scrollTop = target;
    // `scrollIndex`: every scroll event from here on was dispatched after the write. The judge counts
    // yanks from it, never by timestamp -- see `judge`.
    P.park = { t: now(), why, before, target, after: l.scrollTop, sh: l.scrollHeight, ch: l.clientHeight,
               scrollIndex: P.scrolls.length };
  }
  // S5: what the browser's own PageUp does with a control inside the list focused -- a `keydown` on
  // the focused element, which `resolveKey` has no action for, and then the scroll that is the
  // key's default action. A synthetic `KeyboardEvent` has no default action, so the scroll is
  // written, the same way `park` writes a wheel's.
  function parkByKey(target, why) {
    const l = list();
    const focused = document.activeElement;
    const inside = focused !== null && focused !== l && l.contains(focused);
    (inside ? focused : l).dispatchEvent(new KeyboardEvent("keydown", { key: "PageUp", code: "PageUp", bubbles: true, cancelable: true }));
    const before = l.scrollTop;
    l.scrollTop = target;
    P.park = { t: now(), why, by: "PageUp", focusInside: inside, before, target, after: l.scrollTop,
               sh: l.scrollHeight, ch: l.clientHeight, scrollIndex: P.scrolls.length };
  }
  // S6: a sideways touchpad swipe over the reply, carrying the sub-pixel vertical jitter a real one
  // does. It moves nothing in the list; the list must go on following.
  const SWIPE_EVENTS = 8;
  function swipeStep(l, row) {
    const n = P.swipe.n;
    (row.querySelector(".row-body") || row).dispatchEvent(new WheelEvent("wheel", {
      deltaX: -40, deltaY: n % 2 === 0 ? -0.5 : 0.25, deltaMode: 0, bubbles: true, cancelable: true }));
    P.swipe.n = n + 1;
    if (P.swipe.n === SWIPE_EVENTS) {
      P.swipe.tEnd = now();
      P.swipe.stEnd = l.scrollTop;
    }
  }

  let real;
  let thinking = 0;
  let parkHistoryPending = false;
  let armMessage2 = false;
  let focusPending = false;
  let armWheel = false;
  let rearmTimer = null;
  function dispatch(json) {
    let env = null;
    try { env = JSON.parse(json); } catch (_) {}
    if (env && env.kind === "events") {
      for (const e of env.events) {
        if (e.type === "content_delta" && e.kind === "text") {
          if (P.firstText === null) P.firstText = now();
          P.lastText = now();
          if (P.toolDone && !armMessage2 && (SCENARIO === "S2" || SCENARIO === "S5" || SCENARIO === "S6" || SCENARIO === "S8")) {
            armMessage2 = true;
          }
        } else if (e.type === "content_delta") {
          thinking += 1;
          if (SCENARIO === "S3" && thinking === 5) parkHistoryPending = true;
        } else {
          mark(e.type);
          if (e.type === "tool_call_completed") P.toolDone = true;
          if (e.type === "turn_started") {
            P.turnViewport = { innerWidth: window.innerWidth, innerHeight: window.innerHeight, dpr: window.devicePixelRatio };
          }
          if (e.type === "tool_call_started" && SCENARIO === "S5") focusPending = true;
          if (e.type === "turn_started" && SCENARIO === "S4") {
            rearmTimer = window.setInterval(() => {
              const l = list();
              l.scrollTop = l.scrollHeight;
              P.rearms += 1;
              mark("rearm");
            }, 400);
          }
          if (e.type === "turn_completed" && rearmTimer !== null) {
            window.clearInterval(rearmTimer);
            rearmTimer = null;
          }
        }
      }
    } else if (env) {
      mark(env.kind);
    }
    // S7: a wheel up that the list could take but that moves nothing -- one something swallows (as
    // the `?` keymap overlay swallows keys), or one the engine sends somewhere else, which
    // `MessageList` can only find out once the gesture is over -- at the tail, just before the live
    // tool call's row arrives. Then the replay is quiet for about 2s. WebKitGTK performs a synthetic
    // wheel's scroll, so this one is cancelled, after `MessageList`'s own
    // passive listener has seen it.
    if (SCENARIO === "S7" && P.gesture === null && env && env.kind === "events"
        && env.events.some((e) => e.type === "tool_call_started")) {
      const l = list();
      l.addEventListener("wheel", (e) => e.preventDefault(), { passive: false, once: true });
      const wheel = new WheelEvent("wheel", { deltaY: -120, deltaMode: 0, bubbles: true, cancelable: true });
      l.dispatchEvent(wheel);
      P.gesture = { t: now(), st: l.scrollTop, sh: l.scrollHeight, ch: l.clientHeight,
                    distance: l.scrollHeight - l.scrollTop - l.clientHeight, prevented: wheel.defaultPrevented };
    }
    // S8: a wheel up at the tail while the reply streams, which WebKitGTK performs (animated), and
    // then the wheel's first scroll event reporting no movement yet -- measured 0-1ms after the wheel
    // in 2 of 8 wheels at the tail, and reproduced here by dispatching it -- just before a delta.
    if (SCENARIO === "S8" && P.gesture === null && armWheel && env && env.kind === "events"
        && env.events.some((e) => e.type === "content_delta" && e.kind === "text")) {
      const l = list();
      const at = { st: l.scrollTop, sh: l.scrollHeight, ch: l.clientHeight };
      l.dispatchEvent(new WheelEvent("wheel", { deltaY: -__S8_WHEEL_DELTA__, deltaMode: 0, bubbles: true, cancelable: true }));
      l.dispatchEvent(new Event("scroll"));
      P.gesture = { t: now(), st: at.st, sh: at.sh, ch: at.ch, distance: at.sh - at.st - at.ch };
    }
    return real(json);
  }
  Object.defineProperty(window, "__eitriDispatch", {
    configurable: true,
    get() { return real === undefined ? undefined : dispatch; },
    set(fn) { real = fn; },
  });

  let lastList = null;
  function frame() {
    const f = { t: now() };
    const se = document.scrollingElement;
    f.doc = se ? se.scrollTop : null;
    const te = document.querySelector(".turn-elapsed");
    f.el = te ? te.textContent : null;
    const l = list();
    if (l) {
      if (l !== lastList) {
        if (lastList !== null) mark("list-element-changed");
        P.listChanges += 1;
        lastList = l;
        l.addEventListener("scroll", () => P.scrolls.push({ t: now(), st: l.scrollTop, sh: l.scrollHeight, ch: l.clientHeight }),
                           { passive: true });
      }
      f.st = l.scrollTop; f.sh = l.scrollHeight; f.ch = l.clientHeight;
      f.sw = l.scrollWidth; f.cw = l.clientWidth;
      const lr = l.getBoundingClientRect();
      const hit = document.elementFromPoint(lr.left + 60, lr.top + 40);
      if (hit && l.contains(hit)) {
        const block = hit.closest("p, li, pre, h1, h2, h3, h4, table, .tool-call, .row-body") || hit;
        f.rk = block.tagName + ":" + (block.textContent || "").slice(0, 48);
        f.ro = Math.round((block.getBoundingClientRect().top - lr.top) * 10) / 10;
      }
      if (parkHistoryPending && P.park === null) {
        parkHistoryPending = false;
        park(Math.max(0, l.scrollHeight - l.clientHeight - 1.5 * l.clientHeight), "history");
      }
      if (focusPending && P.focused === null) {
        // S5: a control inside the list takes focus the way Tab, `l` or a click would put it there,
        // without scrolling. The replay has no natively focusable element in a row, so the live tool
        // call's own row stands in for one (`tabindex=-1`, which React leaves alone). Taken when that
        // call starts, seconds before the key, so focus landing -- a steering signal of its own --
        // is long over when the PageUp comes.
        const tools = l.querySelectorAll(".row-tool");
        const tool = tools[tools.length - 1];
        if (tool) {
          focusPending = false;
          tool.tabIndex = -1;
          tool.focus({ preventScroll: true });
          P.focused = { t: now(), ok: document.activeElement === tool };
        }
      }
      if (armMessage2 && P.park === null) {
        const rows = l.querySelectorAll(".row");
        const last = rows[rows.length - 1];
        const prev = rows[rows.length - 2];
        if (last && prev && last.classList.contains("row-assistant") && prev.classList.contains("row-tool")) {
          const top = last.getBoundingClientRect().top - lr.top + l.scrollTop;
          const height = last.getBoundingClientRect().height;
          if (SCENARIO === "S6" && P.swipe === null && height >= 0.5 * l.clientHeight) {
            P.swipe = { t: now(), n: 0, st: l.scrollTop, sh: l.scrollHeight, ch: l.clientHeight,
                        distance: l.scrollHeight - l.scrollTop - l.clientHeight };
          }
          if (SCENARIO === "S6" && P.swipe !== null && P.swipe.n < SWIPE_EVENTS) swipeStep(l, last);
          if (SCENARIO === "S8" && height >= 0.5 * l.clientHeight) armWheel = true;
          if ((SCENARIO === "S2" || SCENARIO === "S5") && height >= 1.5 * l.clientHeight) {
            if (SCENARIO === "S2") park(top + 0.3 * l.clientHeight, "inside-message-2");
            else parkByKey(top + 0.3 * l.clientHeight, "inside-message-2");
            P.park.message2Top = top;
            P.park.message2Height = height;
          }
        }
      }
    }
    P.frames.push(f);
    requestAnimationFrame(frame);
  }
  requestAnimationFrame(frame);
})();
"#;

// ---------------------------------------------------------------------------------------------
// One run: a fresh window, a fresh WebView, the replay, the probe.
// ---------------------------------------------------------------------------------------------

fn run_one(config: Config, scenario: Scenario, replay: &Replay) -> Result<Value, String> {
    let window = gtk4::Window::new();
    let (window_width, window_height) = config.window_size();
    window.set_default_size(window_width, window_height);
    window.set_title(Some(&format!("panel_stream_scroll {} {:?}", scenario.name(), config)));

    let content_manager = UserContentManager::new();
    content_manager.add_script(&UserScript::new(
        &INSTRUMENT
            .replace("__SCENARIO__", scenario.name())
            .replace("__S8_WHEEL_DELTA__", &S8_WHEEL_DELTA.to_string())
            .replace("__LIVE_COMMAND__", &json!(LIVE_COMMAND).to_string())
            .replace("__LONG_PATH__", &json!(LONG_PATH).to_string())
            .replace("__LONG_URL__", &json!(LONG_URL).to_string()),
        UserContentInjectedFrames::TopFrame,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    ));
    // As `build_agent_panel` builds it: default settings, one `eitriAgent` handler.
    let webview = WebView::builder().user_content_manager(&content_manager).build();
    webview.set_hexpand(true);
    webview.set_vexpand(true);
    webview.set_zoom_level(config.zoom);
    window.set_child(Some(&webview));

    let queue: Rc<RefCell<VecDeque<String>>> = Rc::new(RefCell::new(VecDeque::new()));
    let started = Rc::new(Cell::new(false));
    content_manager.register_script_message_handler("eitriAgent", None);
    {
        let queue = queue.clone();
        let started = started.clone();
        let on_ready = replay.on_ready.clone();
        content_manager.connect_script_message_received(Some("eitriAgent"), move |_manager, js_value| {
            if let Some(InboundMessage::Ready { request_id }) = parse_inbound_message(&js_value.to_str()) {
                if !started.replace(true) {
                    let mut q = queue.borrow_mut();
                    q.extend(on_ready.iter().cloned());
                    q.push_back(serialize_command_result_for_js(&request_id, Ok(())));
                }
            }
        });
    }
    webview.load_html(&themed_document(&ThemeTokens::fallback().css_vars()), None);
    window.present();

    let main_loop = glib::MainLoop::new(None, false);
    let outcome: Rc<RefCell<Option<Result<Value, String>>>> = Rc::new(RefCell::new(None));
    let step_index = Rc::new(Cell::new(0usize));
    let ready_drained = Rc::new(Cell::new(false));
    // The replay's steps as plain data the timer closure owns, sizes resolved for this configuration.
    enum Action {
        Dispatch(String),
        Idle,
        Resize(i32, i32),
        Script(String),
    }
    let steps: Rc<Vec<Action>> = Rc::new(
        replay
            .steps
            .iter()
            .map(|s| match s {
                Step::Dispatch(p) => Action::Dispatch(p.clone()),
                Step::Idle => Action::Idle,
                Step::Resize { width, height } => Action::Resize(
                    (f64::from(window_width) * width).round() as i32,
                    (f64::from(window_height) * height).round() as i32,
                ),
                Step::Script(js) => Action::Script(js.clone()),
            })
            .collect(),
    );
    {
        let window = window.clone();
        let webview = webview.clone();
        let main_loop = main_loop.clone();
        let outcome = outcome.clone();
        let queue = queue.clone();
        let started = started.clone();
        glib::timeout_add_local(TICK, move || {
            if !started.get() {
                return glib::ControlFlow::Continue;
            }
            if let Some(payload) = queue.borrow_mut().pop_front() {
                evaluate_js_dispatch(&webview, &payload);
                return glib::ControlFlow::Continue;
            }
            if !ready_drained.replace(true) {
                // The first tick after the ready batch: nothing, so the batch's last envelope is
                // not immediately followed by the settle's count starting a tick early.
                return glib::ControlFlow::Continue;
            }
            let i = step_index.get();
            if i < steps.len() {
                match &steps[i] {
                    Action::Dispatch(payload) => evaluate_js_dispatch(&webview, payload),
                    Action::Idle => {}
                    Action::Resize(width, height) => window.set_default_size(*width, *height),
                    Action::Script(js) => {
                        webview.evaluate_javascript(js, None, None, None::<&gtk4::gio::Cancellable>, |result| {
                            if let Err(e) = result {
                                eprintln!("[panel_stream_scroll] a step's script failed: {e}");
                            }
                        });
                    }
                }
                step_index.set(i + 1);
                return glib::ControlFlow::Continue;
            }
            let outcome = outcome.clone();
            let main_loop = main_loop.clone();
            webview.evaluate_javascript(
                "window.__probe()",
                None,
                None,
                None::<&gtk4::gio::Cancellable>,
                move |r| {
                    let parsed = r.map_err(|e| format!("reading the probe failed: {e}")).and_then(|v| {
                        serde_json::from_str::<Value>(&v.to_str()).map_err(|e| format!("probe JSON: {e}"))
                    });
                    outcome.borrow_mut().get_or_insert(parsed);
                    main_loop.quit();
                },
            );
            glib::ControlFlow::Break
        });
    }
    {
        let outcome = outcome.clone();
        let main_loop = main_loop.clone();
        glib::timeout_add_local_once(RUN_TIMEOUT, move || {
            outcome
                .borrow_mut()
                .get_or_insert(Err(format!("run did not finish within {RUN_TIMEOUT:?}")));
            main_loop.quit();
        });
    }
    main_loop.run();
    window.destroy();
    // Let the destroyed WebView's web process go before the next run starts one.
    let ctx = glib::MainContext::default();
    for _ in 0..50 {
        while ctx.iteration(false) {}
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = outcome.borrow_mut().take();
    result.unwrap_or_else(|| Err("no outcome".into()))
}

// ---------------------------------------------------------------------------------------------
// The verdict, read off the probe.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Frame {
    t: f64,
    st: Option<f64>,
    sh: Option<f64>,
    ch: Option<f64>,
    /// The list's `scrollWidth` and `clientWidth`: the sideways check.
    sw: Option<f64>,
    cw: Option<f64>,
    el: Option<String>,
    doc: Option<f64>,
    rk: Option<String>,
    ro: Option<f64>,
}

impl Frame {
    fn distance(&self) -> Option<f64> {
        Some(self.sh? - self.st? - self.ch?)
    }
}

#[derive(Clone, Copy, Debug)]
struct ScrollEvent {
    t: f64,
    st: f64,
    sh: f64,
    ch: f64,
}

fn num(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

struct Probe {
    frames: Vec<Frame>,
    scrolls: Vec<ScrollEvent>,
    marks: Vec<(f64, String)>,
    park: Option<Value>,
    focused: Option<Value>,
    swipe: Option<Value>,
    gesture: Option<Value>,
    /// The sideways check's three long tokens, each measured on one line against the list's width.
    tokens: Option<Value>,
    rearms: u64,
    list_changes: u64,
    /// The page's size when the turn started (`viewport` is read at the end of the run).
    turn_viewport: Value,
    first_text: Option<f64>,
    last_text: Option<f64>,
    viewport: Value,
}

impl Probe {
    fn parse(v: &Value) -> Self {
        let frames = v["frames"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|f| Frame {
                        t: num(f, "t").unwrap_or(0.0),
                        st: num(f, "st"),
                        sh: num(f, "sh"),
                        ch: num(f, "ch"),
                        sw: num(f, "sw"),
                        cw: num(f, "cw"),
                        el: text(f, "el"),
                        doc: num(f, "doc"),
                        rk: text(f, "rk"),
                        ro: num(f, "ro"),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let scrolls = v["scrolls"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| {
                        Some(ScrollEvent {
                            t: num(s, "t")?,
                            st: num(s, "st")?,
                            sh: num(s, "sh")?,
                            ch: num(s, "ch")?,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let marks = v["marks"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| Some((num(m, "t")?, text(m, "mark")?)))
                    .collect()
            })
            .unwrap_or_default();
        Probe {
            frames,
            scrolls,
            marks,
            park: v.get("park").filter(|p| !p.is_null()).cloned(),
            focused: v.get("focused").filter(|p| !p.is_null()).cloned(),
            swipe: v.get("swipe").filter(|p| !p.is_null()).cloned(),
            gesture: v.get("gesture").filter(|p| !p.is_null()).cloned(),
            tokens: v.get("tokens").filter(|p| !p.is_null()).cloned(),
            rearms: v["rearms"].as_u64().unwrap_or(0),
            list_changes: v["listChanges"].as_u64().unwrap_or(0),
            first_text: num(v, "firstText"),
            last_text: num(v, "lastText"),
            viewport: v["viewport"].clone(),
            turn_viewport: v["turnViewport"].clone(),
        }
    }

    fn mark(&self, name: &str) -> Option<f64> {
        self.marks.iter().find(|(_, m)| m == name).map(|(t, _)| *t)
    }

    /// How many times the elapsed counter's text changed between two instants.
    fn clock_changes(&self, from: f64, to: f64) -> usize {
        let mut last: Option<&String> = None;
        let mut changes = 0;
        for f in self.frames.iter().filter(|f| f.t >= from && f.t <= to) {
            if let Some(el) = &f.el {
                if let Some(prev) = last {
                    if prev != el {
                        changes += 1;
                    }
                }
                last = Some(el);
            }
        }
        changes
    }
}

struct Verdict {
    lines: Vec<String>,
    failures: Vec<String>,
    vacuous: Vec<String>,
}

fn judge(config: Config, scenario: Scenario, probe: &Probe) -> Verdict {
    let mut v = Verdict {
        lines: Vec::new(),
        failures: Vec::new(),
        vacuous: Vec::new(),
    };
    // The run measured the size it was asked for. `xvfb-run`'s default screen is 640x480, which
    // silently clamps a 740px-tall window to 480 -- found the first time this ran, under `xvfb-run`;
    // the test's own Xvfb (2026-09-29, `main`) is 1920x1200.
    let want_w = f64::from(config.width);
    let want_h = f64::from(config.height);
    // As the turn found it: S9 resizes the window after the turn, before the probe is read.
    let viewport = if probe.turn_viewport.is_null() {
        &probe.viewport
    } else {
        &probe.turn_viewport
    };
    let (got_w, got_h) = (num(viewport, "innerWidth"), num(viewport, "innerHeight"));
    if got_w.is_none_or(|w| (w - want_w).abs() > 2.0) || got_h.is_none_or(|h| (h - want_h).abs() > 2.0) {
        v.vacuous.push(format!(
            "the page is {got_w:?}x{got_h:?} CSS px, not the {want_w:.0}x{want_h:.0} asked for (is the X screen big enough? `main` starts its own Xvfb at 1920x1200x24)"
        ));
    }
    let (Some(t_start), Some(t_end)) = (probe.mark("turn_started"), probe.mark("turn_completed")) else {
        v.vacuous.push("the turn's start or end was never delivered".into());
        return v;
    };
    let Some(pre) = probe
        .frames
        .iter()
        .rev()
        .find(|f| f.t < t_start && f.st.is_some())
        .cloned()
    else {
        v.vacuous.push("no frame with a message list before the turn".into());
        return v;
    };
    let (pre_st, pre_sh, pre_ch) = (pre.st.unwrap(), pre.sh.unwrap(), pre.ch.unwrap());
    let last = probe.frames.iter().rev().find(|f| f.st.is_some()).cloned().unwrap();
    v.lines.push(format!(
        "viewport {} | before the turn: scrollHeight {pre_sh} clientHeight {pre_ch} scrollTop {pre_st} | after: scrollHeight {} | list element changed {}x | max document scroll {}",
        probe.viewport,
        last.sh.unwrap(),
        probe.list_changes.saturating_sub(1),
        probe.frames.iter().filter_map(|f| f.doc).fold(0.0f64, f64::max),
    ));

    // --- vacuity guards --------------------------------------------------------------------------
    if pre_sh < 3.0 * pre_ch {
        v.vacuous
            .push(format!("history is {pre_sh}px, under 3 x clientHeight {pre_ch}"));
    }
    let pre_distance = pre_sh - pre_st - pre_ch;
    if pre_distance.abs() > AT_BOTTOM_PX {
        v.vacuous.push(format!(
            "the list was {pre_distance}px from its bottom when the turn started"
        ));
    }
    let (Some(first_text), Some(last_text)) = (probe.first_text, probe.last_text) else {
        v.vacuous.push("no text delta was delivered".into());
        return v;
    };
    let ticks = probe.clock_changes(first_text, last_text);
    v.lines.push(format!(
        "elapsed counter changed {ticks}x while text streamed ({:.0}ms of text)",
        last_text - first_text
    ));
    if ticks < MIN_CLOCK_TICKS_WHILE_STREAMING {
        v.vacuous.push(format!(
            "the elapsed counter changed only {ticks}x while text streamed (need {MIN_CLOCK_TICKS_WHILE_STREAMING})"
        ));
    }
    if last.sh.unwrap() < pre_sh + 2.0 * pre_ch {
        v.vacuous.push(format!(
            "the turn grew the list to {} from {pre_sh}, under two screens ({pre_ch} each)",
            last.sh.unwrap()
        ));
    }
    judge_sideways(probe, t_start, &mut v);

    // --- S1(a): an unrequested drop that leaves the view off the bottom ---------------------------
    let drops_off_bottom = |from: f64| -> Vec<String> {
        let mut prev = probe
            .scrolls
            .iter()
            .rev()
            .find(|s| s.t < from)
            .map(|s| s.st)
            .unwrap_or(pre_st);
        let mut out = Vec::new();
        for s in probe.scrolls.iter().filter(|s| s.t >= from) {
            let distance = s.sh - s.st - s.ch;
            if s.st < prev - MOVED_PX && distance.abs() > AT_BOTTOM_PX {
                out.push(format!(
                    "+{:.0}ms scrollTop {prev} -> {} leaving {distance:.0}px below (elapsed '{}')",
                    s.t - t_start,
                    s.st,
                    probe
                        .frames
                        .iter()
                        .rev()
                        .find(|f| f.t <= s.t)
                        .and_then(|f| f.el.clone())
                        .unwrap_or_default()
                ));
            }
            prev = s.st;
        }
        out
    };

    match scenario {
        Scenario::S1 | Scenario::S4 | Scenario::S6 | Scenario::S7 | Scenario::S9 => {
            let drops = drops_off_bottom(t_start);
            v.lines.push(format!("S1(a) drops off the bottom: {}", drops.len()));
            for d in drops.iter().take(8) {
                v.lines.push(format!("    {d}"));
            }
            if !drops.is_empty() {
                v.failures.push(format!(
                    "S1(a): {} scroll event(s) moved the view up and left it off the bottom; first: {}",
                    drops.len(),
                    drops[0]
                ));
            }
            if scenario == Scenario::S4 {
                v.lines.push(format!("rearm writes: {}", probe.rearms));
                if probe.rearms < 5 {
                    v.vacuous.push(format!("only {} re-arm writes happened", probe.rearms));
                }
            }
        }
        Scenario::S2 | Scenario::S3 | Scenario::S5 | Scenario::S8 => {}
    }
    if scenario == Scenario::S7 {
        judge_s7(probe, t_end, &mut v);
    }
    if scenario == Scenario::S8 {
        judge_s8(config, probe, t_end, &mut v);
    }
    if scenario == Scenario::S9 {
        judge_s9(probe, t_end, &mut v);
    }
    if scenario == Scenario::S6 {
        // --- S6: the swipe really happened, at the tail, with the reply still to grow ------------
        match &probe.swipe {
            None => v.vacuous.push("the sideways swipe never happened".into()),
            Some(swipe) => {
                v.lines.push(format!("swipe: {swipe}"));
                let t_swipe = num(swipe, "t").unwrap_or(f64::MAX);
                let events = num(swipe, "n").unwrap_or(0.0);
                let distance = num(swipe, "distance").unwrap_or(f64::MAX);
                let sh = num(swipe, "sh").unwrap_or(f64::MAX);
                if events < SWIPE_EVENTS {
                    v.vacuous
                        .push(format!("the swipe sent {events} wheel events, not {SWIPE_EVENTS}"));
                }
                if t_swipe > t_end || distance > HIDDEN_PX {
                    v.vacuous.push(format!(
                        "the swipe did not land on a following view: +{:.0}ms, {distance}px from the bottom",
                        t_swipe - t_start
                    ));
                }
                if last.sh.unwrap() < sh + pre_ch {
                    v.vacuous.push(format!(
                        "the reply grew only {}px after the swipe, under one screen ({pre_ch})",
                        last.sh.unwrap() - sh
                    ));
                }
            }
        }
    }
    if matches!(scenario, Scenario::S1 | Scenario::S6) {
        // --- S1(b): how long the newest text was out of view -------------------------------------
        let mut longest = 0.0f64;
        let mut longest_at = 0.0f64;
        let mut worst = 0.0f64;
        let mut run_start: Option<f64> = None;
        for f in probe.frames.iter().filter(|f| f.t >= t_start && f.t <= t_end) {
            let d = f.distance().unwrap_or(0.0);
            worst = worst.max(d);
            match (d > HIDDEN_PX, run_start) {
                (true, None) => run_start = Some(f.t),
                (false, Some(s)) => {
                    if f.t - s > longest {
                        longest = f.t - s;
                        longest_at = s - t_start;
                    }
                    run_start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = run_start {
            if t_end - s > longest {
                longest = t_end - s;
                longest_at = s - t_start;
            }
        }
        let final_distance = last.distance().unwrap();
        v.lines.push(format!(
            "S1(b) longest stretch >{HIDDEN_PX}px short: {longest:.0}ms (from +{longest_at:.0}ms), worst {worst:.0}px, final distance {final_distance:.1}px"
        ));
        if longest > MAX_HIDDEN_STRETCH_MS {
            v.failures.push(format!(
                "S1(b): the newest text was more than {HIDDEN_PX}px below the view for {longest:.0}ms (limit {MAX_HIDDEN_STRETCH_MS}ms), from +{longest_at:.0}ms"
            ));
        }
        if final_distance.abs() > AT_BOTTOM_PX {
            v.failures.push(format!(
                "S1(b): the turn ended {final_distance:.0}px short of the bottom"
            ));
        }

        // --- S1(c): no frame short of the end (the GUI pass's F-c) ------------------------------
        // Every frame after the turn started, not just the stretches S1(b) measures: with no input
        // the view is following throughout, and a view that is following is at its end whenever the
        // page is painted. A smooth scroll to a new row broke this for a frame or two per row.
        let short: Vec<&Frame> = probe
            .frames
            .iter()
            .filter(|f| f.t > t_start && f.t <= t_end)
            .filter(|f| f.distance().is_some_and(|d| d > AT_BOTTOM_PX))
            .collect();
        let worst_short = short.iter().filter_map(|f| f.distance()).fold(0.0f64, f64::max);
        v.lines.push(format!(
            "S1(c) frames short of the end: {} (worst {worst_short:.0}px)",
            short.len()
        ));
        if let Some(f) = short.first() {
            v.failures.push(format!(
                "S1(c): {} frame(s) showed a following view short of its end; first at +{:.0}ms, {:.0}px short; worst {worst_short:.0}px",
                short.len(),
                f.t - t_start,
                f.distance().unwrap_or(0.0)
            ));
        }
    }

    if matches!(scenario, Scenario::S2 | Scenario::S3 | Scenario::S5) {
        // A parked reader must keep the same scroll position.
        let Some(park) = &probe.park else {
            v.vacuous.push("the reader never parked".into());
            return v;
        };
        let t_park = num(park, "t").unwrap_or(f64::MAX);
        if scenario == Scenario::S5 {
            // The key came from a control inside the list that had held focus long enough that only
            // the key itself can explain the park being kept.
            let focused_at = probe.focused.as_ref().and_then(|f| num(f, "t"));
            let focused_ok = probe
                .focused
                .as_ref()
                .and_then(|f| f.get("ok"))
                .and_then(Value::as_bool);
            v.lines.push(format!("focused: {:?}", probe.focused));
            if focused_ok != Some(true) || park.get("focusInside").and_then(Value::as_bool) != Some(true) {
                v.vacuous.push(format!(
                    "no control inside the list held focus when the PageUp came (focused {:?}, park {park})",
                    probe.focused
                ));
            }
            if focused_at.is_none_or(|t| t_park - t < MIN_FOCUS_BEFORE_KEY_MS) {
                v.vacuous.push(format!(
                    "focus landed only {:?}ms before the key (need {MIN_FOCUS_BEFORE_KEY_MS}), so it, not the key, could explain the park",
                    focused_at.map(|t| t_park - t)
                ));
            }
        }
        let parked_at = num(park, "after").unwrap_or(f64::NAN);
        let target = num(park, "target").unwrap_or(f64::NAN);
        v.lines.push(format!("parked: {park}"));
        if (parked_at - target).abs() > 1.0 || t_park > t_end {
            v.vacuous.push(format!(
                "the park did not land: asked for {target}, got {parked_at}, at +{:.0}ms",
                t_park - t_start
            ));
        }
        let from = t_park + PARK_SETTLE_MS;
        let parked_ticks = probe.clock_changes(from, t_end);
        v.lines
            .push(format!("elapsed counter changed {parked_ticks}x while parked"));
        if parked_ticks < MIN_CLOCK_TICKS_WHILE_PARKED {
            v.vacuous.push(format!(
                "the elapsed counter changed only {parked_ticks}x while the reader was parked (need {MIN_CLOCK_TICKS_WHILE_PARKED})"
            ));
        }

        // The reader must stay where they parked from the park itself onward. The settle only
        // excuses the park's own scroll event; starting checks after it can hide a yank inside it.
        // A clock tick that moves the reader 80ms after parking must therefore still count as movement.
        //
        // Frames count strictly after the park's own frame, whose timestamp was read before
        // the park was written and whose position is therefore still the old one. Scroll events
        // count from `scrollIndex`, rather than by timestamp. A previous delta's snap can dispatch
        // its event just before the park's frame callback in the same rendering update.
        // WebKitGTK's 1ms clock can give that old position the park's timestamp, so `s.t >= t_park`
        // can falsely report a yank. Counting by order includes the first event after the park.
        let off_park: Vec<&Frame> = probe
            .frames
            .iter()
            .filter(|f| f.t > t_park && f.t <= t_end)
            .filter(|f| f.st.is_some_and(|st| (st - parked_at).abs() > MOVED_PX))
            .collect();
        let scroll_index = park
            .get("scrollIndex")
            .and_then(Value::as_u64)
            .and_then(|i| usize::try_from(i).ok());
        let Some(scroll_index) = scroll_index.filter(|&i| i <= probe.scrolls.len()) else {
            v.vacuous.push(format!(
                "the park recorded no usable scroll index ({:?} of {} events), so nothing says which scroll events came after it",
                park.get("scrollIndex"),
                probe.scrolls.len()
            ));
            return v;
        };
        let (before_park, after_park) = probe.scrolls.split_at(scroll_index);
        let ties = before_park.iter().filter(|s| s.t >= t_park).count();
        v.lines.push(format!(
            "scroll events dispatched before the park at or after its timestamp (the snap's, read pre-park; not yanks): {ties}"
        ));
        let yanks: Vec<&ScrollEvent> = after_park
            .iter()
            .filter(|s| s.t <= t_end && (s.st - parked_at).abs() > MOVED_PX)
            .collect();
        let after_settle: Vec<&ScrollEvent> = probe.scrolls.iter().filter(|s| s.t >= from && s.t <= t_end).collect();
        v.lines.push(format!(
            "while parked: {} frame(s) off {parked_at}, {} scroll event(s) off it, {} scroll event(s) after the settle",
            off_park.len(),
            yanks.len(),
            after_settle.len()
        ));
        for s in yanks.iter().take(6) {
            v.lines.push(format!(
                "    +{:.0}ms scrollTop {} (scrollHeight {}, clientHeight {}, elapsed '{}')",
                s.t - t_start,
                s.st,
                s.sh,
                s.ch,
                probe
                    .frames
                    .iter()
                    .rev()
                    .find(|f| f.t <= s.t)
                    .and_then(|f| f.el.clone())
                    .unwrap_or_default()
            ));
        }
        if let Some(f) = off_park.first() {
            v.failures.push(format!(
                "{}: the reader parked at scrollTop {parked_at} and was moved in {} frame(s); first at +{:.0}ms to {:?}",
                scenario.name(),
                off_park.len(),
                f.t - t_start,
                f.st
            ));
        }
        if let Some(s) = yanks.first() {
            v.failures.push(format!(
                "{}: {} scroll event(s) moved a parked reader; first at +{:.0}ms to scrollTop {}",
                scenario.name(),
                yanks.len(),
                s.t - t_start,
                s.st
            ));
        }
        if let Some(s) = after_settle.first() {
            v.failures.push(format!(
                "{}: {} scroll event(s) after the {PARK_SETTLE_MS}ms settle; first at +{:.0}ms, scrollTop {}",
                scenario.name(),
                after_settle.len(),
                s.t - t_start,
                s.st
            ));
        }

        // The text under the reading point, from the end of the settle: same block, same offset.
        let parked: Vec<&Frame> = probe.frames.iter().filter(|f| f.t >= from && f.t <= t_end).collect();
        if let Some(first) = parked.first() {
            let reading_moves: Vec<&&Frame> = parked
                .iter()
                .filter(|f| f.rk != first.rk || (f.ro.unwrap_or(0.0) - first.ro.unwrap_or(0.0)).abs() > MOVED_PX)
                .collect();
            v.lines.push(format!(
                "frames after the settle: {} | reading point moved in {} (reading {:?} at {:?}px)",
                parked.len(),
                reading_moves.len(),
                first.rk,
                first.ro
            ));
            if first.rk.is_none() {
                v.vacuous
                    .push("nothing was under the reading point while parked".into());
            }
            if let Some(f) = reading_moves.first() {
                v.failures.push(format!(
                    "{}: the text under the reading point moved in {} frame(s); first at +{:.0}ms: {:?} at {:?}px (was {:?} at {:?}px)",
                    scenario.name(),
                    reading_moves.len(),
                    f.t - t_start,
                    f.rk,
                    f.ro,
                    first.rk,
                    first.ro
                ));
            }
        } else {
            v.vacuous
                .push("no frame was sampled while the reader was parked".into());
        }
    }
    v
}

/// S7: a wheel up that the list could take lands at the tail and moves nothing, the live tool
/// call's row arrives during the gesture, and then the replay stays quiet for about 2s.
/// `MessageList` must scroll to that row when the gesture ends, not when the next state change
/// (the tool result) arrives. Otherwise a row that arrives just before a pause, such as a
/// permission card after which the turn waits for the user, stays below the view.
fn judge_s7(probe: &Probe, t_end: f64, v: &mut Verdict) {
    let Some(gesture) = &probe.gesture else {
        v.vacuous.push("the wheel before the tool call never happened".into());
        return;
    };
    v.lines.push(format!("gesture: {gesture}"));
    let t_gesture = num(gesture, "t").unwrap_or(f64::MAX);
    let at_gesture = num(gesture, "distance").unwrap_or(f64::MAX);
    if at_gesture.abs() > AT_BOTTOM_PX {
        v.vacuous.push(format!(
            "the wheel did not land on a view at its bottom: {at_gesture}px short"
        ));
    }
    let (Some(t_row), Some(t_result)) = (probe.mark("tool_call_started"), probe.mark("tool_call_completed")) else {
        v.vacuous
            .push("the live tool call's start or its result was never delivered".into());
        return;
    };
    if (t_gesture - t_row).abs() > 50.0 {
        v.vacuous.push(format!(
            "the wheel came {:.0}ms from the tool call's row, not with it",
            t_gesture - t_row
        ));
    }
    if t_result - t_gesture < S7_MIN_PAUSE_MS {
        v.vacuous.push(format!(
            "the replay was quiet for only {:.0}ms after the wheel (need {S7_MIN_PAUSE_MS}ms)",
            t_result - t_gesture
        ));
    }
    if gesture.get("prevented").and_then(Value::as_bool) != Some(true) {
        v.vacuous
            .push("the wheel was not cancelled, so it could have scrolled the list itself".into());
    }
    // The stop held the new row back: some frame after the wheel shows the view short of its bottom.
    // (Which frame first shows the row depends on when React commits it, so this does not insist on
    // the very first one.)
    let after: Vec<&Frame> = probe
        .frames
        .iter()
        .filter(|f| f.t > t_gesture && f.t <= t_end && f.st.is_some())
        .collect();
    let Some(first_off) = after
        .iter()
        .position(|f| f.distance().is_some_and(|d| d > AT_BOTTOM_PX))
    else {
        v.vacuous.push(
            "the view never left its bottom after the wheel, so nothing was held back and nothing had to catch up"
                .into(),
        );
        return;
    };
    let off_at = after[first_off].t;
    let off_by = after[first_off].distance().unwrap_or(0.0);
    let back = after[first_off..]
        .iter()
        .find(|f| f.distance().is_some_and(|d| d <= AT_BOTTOM_PX));
    // The wheel itself moved nothing: until the view is back at its bottom, it never went above where
    // the wheel found it. (If WebKitGTK scrolled a cancelled wheel anyway, a stop for good would be
    // the right answer, and this scenario would be testing nothing.)
    let gesture_st = num(gesture, "st").unwrap_or(f64::NAN);
    let until = back.map_or(t_end, |f| f.t);
    if let Some(f) = after
        .iter()
        .filter(|f| f.t <= until)
        .find(|f| f.st.is_some_and(|st| st < gesture_st - AT_BOTTOM_PX))
    {
        v.vacuous.push(format!(
            "the cancelled wheel moved the list anyway: scrollTop {:?} at +{:.0}ms, from {gesture_st}",
            f.st,
            f.t - t_gesture
        ));
    }
    match back {
        None => v.failures.push(format!(
            "S7: the view left its bottom {:.0}ms after the wheel ({off_by:.0}px short) and never came back",
            off_at - t_gesture
        )),
        Some(f) => {
            let took = f.t - t_gesture;
            v.lines.push(format!(
                "S7: {off_by:.0}px short from +{:.0}ms after the wheel, back at the bottom at +{took:.0}ms; the tool result came at +{:.0}ms",
                off_at - t_gesture,
                t_result - t_gesture
            ));
            if took > S7_CATCH_UP_LIMIT_MS {
                v.failures.push(format!(
                    "S7: after a wheel that moved nothing, the new row stayed below the view for {took:.0}ms (limit {S7_CATCH_UP_LIMIT_MS}ms); the tool result came at +{:.0}ms",
                    t_result - t_gesture
                ));
            }
        }
    }
    let final_distance = probe.frames.iter().rev().find_map(Frame::distance).unwrap_or(0.0);
    if final_distance.abs() > AT_BOTTOM_PX {
        v.failures
            .push(format!("S7: the turn ended {final_distance:.0}px short of the bottom"));
    }
}

/// S8 sends a wheel up at the tail while the reply streams, then a text delta.
/// WebKitGTK animates the wheel scroll but can first emit a scroll event with no movement.
/// That event is dispatched here so every run exercises it. The wheel must move the view
/// and leave following stopped for the rest of the turn. Re-arming at the bottom on the
/// first event can clear the provisional stop and let the delta's snap cancel the wheel.
fn judge_s8(config: Config, probe: &Probe, t_end: f64, v: &mut Verdict) {
    let Some(gesture) = &probe.gesture else {
        v.vacuous.push("the wheel never happened".into());
        return;
    };
    v.lines.push(format!("gesture: {gesture}"));
    let t_gesture = num(gesture, "t").unwrap_or(f64::MAX);
    let gesture_st = num(gesture, "st").unwrap_or(f64::NAN);
    let gesture_sh = num(gesture, "sh").unwrap_or(f64::MAX);
    let ch = num(gesture, "ch").unwrap_or(f64::NAN);
    let at_gesture = num(gesture, "distance").unwrap_or(f64::MAX);
    if at_gesture.abs() > AT_BOTTOM_PX || t_gesture > t_end {
        v.vacuous.push(format!(
            "the wheel did not land on a following view during the turn: {at_gesture}px short"
        ));
    }
    let last_sh = probe.frames.iter().rev().find_map(|f| f.sh).unwrap_or(0.0);
    if last_sh < gesture_sh + ch {
        v.vacuous.push(format!(
            "the reply grew only {}px after the wheel, under one screen ({ch})",
            last_sh - gesture_sh
        ));
    }
    // Where the wheel's scroll must have taken the view: `S8_WHEEL_DELTA` device px, in CSS px.
    let wheel_px = S8_WHEEL_DELTA / config.zoom;
    let must_be_at_most = gesture_st - wheel_px / 2.0;
    let settled: Vec<&Frame> = probe
        .frames
        .iter()
        .filter(|f| f.t >= t_gesture + S8_SETTLE_MS && f.t <= t_end && f.st.is_some())
        .collect();
    if settled.is_empty() {
        v.vacuous.push("no frame was sampled after the wheel's scroll".into());
        return;
    }
    let (lo, hi) = settled
        .iter()
        .filter_map(|f| f.st)
        .fold((f64::MAX, f64::MIN), |(lo, hi), st| (lo.min(st), hi.max(st)));
    v.lines.push(format!(
        "S8: the wheel found scrollTop {gesture_st}; from +{S8_SETTLE_MS}ms to the turn's end it was {lo}..{hi} (must stay at or above {wheel_px:.0}px/2 up, i.e. <= {must_be_at_most:.0})"
    ));
    if let Some(f) = settled.iter().find(|f| f.st.is_some_and(|st| st > must_be_at_most)) {
        v.failures.push(format!(
            "S8: the wheel's scroll was undone: at +{:.0}ms after the wheel the view was at scrollTop {:?} ({:?}px from the bottom), where the wheel found it at {gesture_st} and should have taken it about {wheel_px:.0}px up",
            f.t - t_gesture,
            f.st,
            f.distance()
        ));
    }
}

/// Every scenario (the GUI pass's F-b): the conversation never scrolls sideways. `.message-list`
/// scrolls on both axes, so ANY descendant whose text cannot wrap -- a `<pre>` left at the UA's
/// `white-space: pre`, a path or URL with no space in it -- makes the whole conversation, sign column
/// and all, scroll horizontally. Vacuous unless the replay's three long tokens really are wider than
/// the list when laid out on one line.
///
/// One frame per width change is not judged: the frame whose `clientWidth` differs from the frame
/// before it. The instrument samples in `requestAnimationFrame`, which runs before the rendering
/// update's layout and `ResizeObserver` callbacks -- so that frame reads a layout still carrying the
/// OLD `--list-inline-size`, which `MessageList`'s observer rewrites before the frame is painted. S9's
/// narrowing showed exactly that on the fixed build: one frame 201px wide at the new width, the next
/// one flush. A stale width that stayed would show in the frame after it, which IS judged.
fn judge_sideways(probe: &Probe, t_start: f64, v: &mut Verdict) {
    let mut previous_width: Option<f64> = None;
    let widths: Vec<(&Frame, f64)> = probe
        .frames
        .iter()
        .filter_map(|f| {
            let (sw, cw) = (f.sw?, f.cw?);
            let resized = previous_width.is_some_and(|w| (w - cw).abs() > 0.5);
            previous_width = Some(cw);
            (!resized).then_some((f, sw - cw))
        })
        .collect();
    if widths.is_empty() {
        v.vacuous.push("no frame measured the list's scrollWidth".into());
        return;
    }
    let worst = widths.iter().map(|(_, d)| *d).fold(0.0f64, f64::max);
    v.lines.push(format!(
        "sideways: the list's content reached at most {worst:.0}px past its own width; tokens {}",
        probe.tokens.clone().unwrap_or(Value::Null)
    ));
    if let Some((f, d)) = widths.iter().find(|(_, d)| *d > SIDEWAYS_PX) {
        v.failures.push(format!(
            "sideways: the conversation scrolled sideways, its content {d:.0}px wider than the list, first at +{:.0}ms from the turn's start (worst {worst:.0}px)",
            f.t - t_start
        ));
    }
    let Some(tokens) = &probe.tokens else {
        v.vacuous.push("the long tokens were never measured".into());
        return;
    };
    for name in ["command", "path", "url"] {
        let width = tokens[name]["width"].as_f64();
        let list = tokens[name]["list"].as_f64().unwrap_or(f64::MAX);
        match width {
            None => v.vacuous.push(format!("the long {name} was not found in the conversation")),
            Some(w) if w <= list => v.vacuous.push(format!(
                "the long {name} is {w}px on one line, not wider than the list's {list}px, so it could not have scrolled it"
            )),
            Some(_) => {}
        }
    }
}

/// S9 changes layout after the turn ends while the view follows at the bottom.
/// The window narrows so the reply reflows taller, then shortens the list's box. A theme
/// envelope raises the font size, and `--prose-measure` narrows only the rows. After each,
/// at most `S9_MAX_SHORT_FRAMES` sampled frames may show the view short of the end.
/// It must then stay at the end until the next change. Snapping only on state changes
/// leaves the view short while nothing streams.
fn judge_s9(probe: &Probe, t_end: f64, v: &mut Verdict) {
    // One window resize can reach the page as more than one `resize` event; those within 100ms of the
    // previous one are the same change.
    let mut triggers: Vec<(f64, &str)> = Vec::new();
    for (t, m) in probe
        .marks
        .iter()
        .filter(|(t, m)| *t > t_end && matches!(m.as_str(), "resize" | "theme" | "restyle"))
    {
        if triggers
            .last()
            .is_some_and(|(last_t, last_m)| *last_m == m && t - last_t < 100.0)
        {
            continue;
        }
        triggers.push((*t, m.as_str()));
    }
    let kinds: Vec<&str> = triggers.iter().map(|(_, m)| *m).collect();
    if kinds != ["resize", "resize", "theme", "restyle"] {
        v.vacuous.push(format!(
            "S9's four changes did not reach the page as resize, resize, theme, restyle: {kinds:?}"
        ));
        return;
    }
    let what = [
        "narrowed the window",
        "shortened the window",
        "raised the font size",
        "narrowed the prose measure",
    ];
    for (i, &(t, kind)) in triggers.iter().enumerate() {
        let until = triggers.get(i + 1).map_or(f64::MAX, |(next, _)| *next);
        // A frame at the change's own millisecond already shows it: a window resize is dispatched in
        // the rendering update's resize steps, before the frame callback; a theme or restyle is applied
        // in the task that marked it, before the next frame.
        let Some(pre) = probe.frames.iter().rev().find(|f| f.t < t && f.st.is_some()).cloned() else {
            v.vacuous
                .push(format!("S9: no frame before the change that {}", what[i]));
            continue;
        };
        let after: Vec<&Frame> = probe
            .frames
            .iter()
            .filter(|f| f.t >= t && f.t < until && f.distance().is_some())
            .collect();
        let Some(last) = after.last() else {
            v.vacuous
                .push(format!("S9: no frame after the change that {}", what[i]));
            continue;
        };
        // Had the view stayed where it was, how much further from its end the change alone left it.
        let moved = (last.sh.unwrap() - pre.sh.unwrap()) - (last.ch.unwrap() - pre.ch.unwrap());
        if moved <= HIDDEN_PX {
            v.vacuous.push(format!(
                "S9: the change that {} moved the list's end only {moved:.0}px",
                what[i]
            ));
        }
        if kind == "resize" {
            let dw = (last.cw.unwrap_or(0.0) - pre.cw.unwrap_or(0.0)).abs();
            let dh = (last.ch.unwrap() - pre.ch.unwrap()).abs();
            if dw < 20.0 && dh < 20.0 {
                v.vacuous.push(format!(
                    "S9: the window resize that {} changed the list's box by only {dw:.0}x{dh:.0}px",
                    what[i]
                ));
            }
        }
        // From the change to the first frame from which the view stays at its end until the next one.
        let settled_from = after
            .iter()
            .rposition(|f| f.distance().is_some_and(|d| d > AT_BOTTOM_PX))
            .map_or(Some(0), |p| (p + 1 < after.len()).then_some(p + 1));
        let last_distance = last.distance().unwrap_or(0.0);
        match settled_from {
            None => {
                v.lines.push(format!(
                    "S9: the panel {}: its end moved {moved:.0}px; the view never came back ({last_distance:.0}px short {:.0}ms later)",
                    what[i],
                    last.t - t
                ));
                v.failures.push(format!(
                    "S9: after the panel {} with nothing streaming, the view was left {last_distance:.0}px short of its end",
                    what[i]
                ));
            }
            Some(p) => {
                let took = after[p].t - t;
                v.lines.push(format!(
                    "S9: the panel {}: its end moved {moved:.0}px; the view was back at it in frame {} after the change ({took:.0}ms later) and stayed",
                    what[i],
                    p + 1
                ));
                if p > S9_MAX_SHORT_FRAMES {
                    v.failures.push(format!(
                        "S9: after the panel {} with nothing streaming, {p} sampled frames showed the view short of its end, over {took:.0}ms (at most {S9_MAX_SHORT_FRAMES})",
                        what[i]
                    ));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The judge's own tests: synthetic probes, no display. They run on every invocation, including the
// plain `cargo test --workspace` that skips the WebKitGTK runs.
// ---------------------------------------------------------------------------------------------

/// A parked-reader (S2) run the judge must pass, built frame by frame the way the instrument records
/// one: 560x740 CSS px, three screens of history at the bottom, a turn of text growing the list by
/// two and a half screens, and a park 300px up inside it. `before_write` are scroll events the page
/// dispatched in the park's own rendering update, before the instrument's frame callback wrote the
/// park; `after_write` are events after it, with the park's own event at +5ms appended to them.
/// `index` controls whether the park records where its events start (`scrollIndex`).
fn synthetic_parked_run(before_write: &[(f64, f64)], after_write: &[(f64, f64)], index: bool) -> Value {
    const CH: f64 = 700.0;
    const CW: f64 = 536.0;
    const T_START: f64 = 1000.0;
    const T_END: f64 = 9000.0;
    const FIRST_TEXT: f64 = 2000.0;
    const LAST_TEXT: f64 = 8000.0;
    // A frame time, as the real traces have one: the park is written from inside the frame callback
    // whose own `t` was read first, so the park and its frame share a millisecond.
    const T_PARK: f64 = 4992.0;
    // One 16px delta every 48ms while text streams.
    let sh_at = |t: f64| 2400.0 + 16.0 * ((t.clamp(FIRST_TEXT, LAST_TEXT) - FIRST_TEXT) / 48.0).floor();
    let before = sh_at(T_PARK) - CH;
    let parked_at = before - 300.0;

    let mut frames = Vec::new();
    let mut scrolls = Vec::new();
    let mut last_sh = sh_at(0.0);
    let mut t = 0.0;
    while t <= 10_000.0 {
        let sh = sh_at(t);
        let parked = t > T_PARK;
        let st = if parked { parked_at } else { sh - CH };
        if !parked && sh != last_sh {
            // Following: each delta's snap has its own scroll event, at the bottom.
            scrolls.push(json!({ "t": t - 1.0, "st": st, "sh": sh, "ch": CH }));
        }
        last_sh = sh;
        let el = (T_START..=T_END)
            .contains(&t)
            .then(|| format!("{}s", ((t - T_START) / 1000.0).floor()));
        let (rk, ro) = if parked {
            ("P:the reader's paragraph", 37.0)
        } else {
            ("P:the tail", -40.0)
        };
        frames.push(
            json!({ "t": t, "doc": 0, "el": el, "st": st, "sh": sh, "ch": CH, "sw": CW, "cw": CW, "rk": rk, "ro": ro }),
        );
        t += 16.0;
    }
    for &(t, st) in before_write {
        scrolls.push(json!({ "t": t, "st": st, "sh": sh_at(t), "ch": CH }));
    }
    let scroll_index = scrolls.len();
    for &(t, st) in after_write.iter().chain([(T_PARK + 5.0, parked_at)].iter()) {
        scrolls.push(json!({ "t": t, "st": st, "sh": sh_at(t), "ch": CH }));
    }
    let mut park = json!({
        "t": T_PARK, "why": "inside-message-2", "before": before, "target": parked_at, "after": parked_at,
        "sh": sh_at(T_PARK), "ch": CH,
    });
    if index {
        park["scrollIndex"] = json!(scroll_index);
    }
    json!({
        "scenario": "S2", "frames": frames, "scrolls": scrolls,
        "marks": [{ "t": T_START, "mark": "turn_started" }, { "t": T_END, "mark": "turn_completed" }],
        "park": park, "rearms": 0, "listChanges": 1, "firstText": FIRST_TEXT, "lastText": LAST_TEXT,
        "focused": null, "swipe": null, "tokens": synthetic_tokens(900.0, CW),
        "viewport": { "innerWidth": 560, "innerHeight": 740, "dpr": 1 },
    })
}

/// The sideways check's three measured tokens, each `width` wide on one line against a `list`-wide list.
fn synthetic_tokens(width: f64, list: f64) -> Value {
    let token = json!({ "width": width, "list": list });
    json!({ "command": token, "path": token, "url": token })
}

/// How the view behaves after each of S9's four changes in `synthetic_s9_run`.
#[derive(Clone, Copy, PartialEq)]
enum S9Shape {
    /// Back at the end one frame after each change -- what the fix does.
    Fixed,
    /// Left where it was until the end.
    Stuck,
    /// Back at the end, but 400ms after each change.
    Slow,
    /// As `Fixed`, but the last change (the restyle) moves nothing.
    RestyleMovesNothing,
}

/// A following (S9) run the judge must read per `shape`: the turn as in `synthetic_parked_run` but
/// never parked, then four changes a second apart after it -- a narrowing (the list 536 -> 330px wide,
/// its content 300px taller), a shortening (700 -> 525px tall), a font push (300px taller, the list 20px
/// shorter) and a restyle (300px taller) -- with the first resize reaching the page as two `resize`
/// events 5ms apart, which the judge must count once.
fn synthetic_s9_run(shape: S9Shape) -> Value {
    const T_START: f64 = 1000.0;
    const T_END: f64 = 9000.0;
    const FIRST_TEXT: f64 = 2000.0;
    const LAST_TEXT: f64 = 8000.0;
    const CHANGES: [f64; 4] = [10_100.0, 11_100.0, 12_100.0, 13_100.0];
    let sh_at = |t: f64| 2400.0 + 16.0 * ((t.clamp(FIRST_TEXT, LAST_TEXT) - FIRST_TEXT) / 48.0).floor();
    let back_after = if shape == S9Shape::Slow { 400.0 } else { 16.0 };

    let mut frames = Vec::new();
    let mut scrolls = Vec::new();
    let (mut sh, mut ch, mut cw) = (sh_at(0.0), 700.0, 536.0);
    // Where the view was when the latest change came, and when it came.
    let mut held: Option<(f64, f64)> = None;
    let mut applied = 0;
    let mut t = 0.0;
    while t <= 14_200.0 {
        if applied < CHANGES.len() && t > CHANGES[applied] {
            let st_before = sh - ch;
            match applied {
                0 => {
                    sh += 300.0;
                    cw = 330.0;
                }
                1 => ch -= 175.0,
                2 => {
                    sh += 300.0;
                    ch -= 20.0;
                }
                _ => {
                    if shape != S9Shape::RestyleMovesNothing {
                        sh += 300.0;
                    }
                }
            }
            // A stuck view keeps the position the FIRST change found it at.
            held = match (shape, held) {
                (S9Shape::Stuck, Some(h)) => Some((h.0, CHANGES[applied])),
                _ => Some((st_before, CHANGES[applied])),
            };
            applied += 1;
        }
        let new_sh = if t < T_END { sh_at(t) } else { sh };
        if t < T_END {
            if new_sh != sh {
                scrolls.push(json!({ "t": t - 1.0, "st": new_sh - ch, "sh": new_sh, "ch": ch }));
            }
            sh = new_sh;
        }
        let st = match held {
            Some((st, at)) if shape == S9Shape::Stuck || t <= at + back_after => st,
            _ => sh - ch,
        };
        let el = (T_START..=T_END)
            .contains(&t)
            .then(|| format!("{}s", ((t - T_START) / 1000.0).floor()));
        frames.push(json!({ "t": t, "doc": 0, "el": el, "st": st, "sh": sh, "ch": ch, "sw": cw, "cw": cw, "rk": "P:the tail", "ro": -40.0 }));
        t += 16.0;
    }
    json!({
        "scenario": "S9", "frames": frames, "scrolls": scrolls,
        "marks": [
            { "t": T_START, "mark": "turn_started" }, { "t": T_END, "mark": "turn_completed" },
            { "t": CHANGES[0], "mark": "resize" }, { "t": CHANGES[0] + 5.0, "mark": "resize" },
            { "t": CHANGES[1], "mark": "resize" }, { "t": CHANGES[2], "mark": "theme" },
            { "t": CHANGES[3], "mark": "restyle" },
        ],
        "park": null, "rearms": 0, "listChanges": 1, "firstText": FIRST_TEXT, "lastText": LAST_TEXT,
        "focused": null, "swipe": null, "tokens": synthetic_tokens(900.0, 536.0),
        "viewport": { "innerWidth": 560, "innerHeight": 740, "dpr": 1 },
    })
}

/// Runs the judge over synthetic S2 probes whose verdicts are known. Returns how many cases ran.
fn judge_self_test() -> Result<usize, String> {
    let config = config(560, 740, 1.0);
    let verdict_of = |raw: Value| judge(config, Scenario::S2, &Probe::parse(&raw));
    let park = |raw: &Value| (num(&raw["park"], "t").unwrap(), num(&raw["park"], "before").unwrap());
    let mut ran = 0;

    // 1. A still reader, with nothing unusual: passes.
    let raw = synthetic_parked_run(&[], &[], true);
    let v = verdict_of(raw);
    if !v.failures.is_empty() || !v.vacuous.is_empty() {
        return Err(format!(
            "a still reader must pass; failures {:?}, vacuous {:?}",
            v.failures, v.vacuous
        ));
    }
    ran += 1;

    // 2. The previous delta's snap dispatches its scroll event before the park's frame callback
    //    in the same rendering update, so it reads the pre-park position. WebKitGTK's 1ms clock
    //    can give it the park's timestamp. Dispatch order keeps this earlier event from being
    //    counted as movement after the park.
    let probe = synthetic_parked_run(&[], &[], true);
    let (t_park, before) = park(&probe);
    let v = verdict_of(synthetic_parked_run(&[(t_park, before)], &[], true));
    if !v.failures.is_empty() || !v.vacuous.is_empty() {
        return Err(format!(
            "an event dispatched before the park, at its millisecond, is not a yank; failures {:?}, vacuous {:?}",
            v.failures, v.vacuous
        ));
    }
    ran += 1;

    // 3. The counterpart the tie must not hide: the FIRST event after the write, at the park's own
    //    millisecond, reading the reader put back where they were. That is a yank, however early.
    let v = verdict_of(synthetic_parked_run(&[], &[(t_park, before)], true));
    if v.failures.len() != 1
        || !v.failures[0].contains("scroll event(s) moved a parked reader")
        || !v.vacuous.is_empty()
    {
        return Err(format!(
            "the first event after the park, off it, must be the one failure; failures {:?}, vacuous {:?}",
            v.failures, v.vacuous
        ));
    }
    ran += 1;

    // 4. A park that does not say where its events start is not judged at all.
    let v = verdict_of(synthetic_parked_run(&[(t_park, before)], &[], false));
    if !v.vacuous.iter().any(|m| m.contains("scroll index")) {
        return Err(format!(
            "a park with no scroll index must be vacuous; failures {:?}, vacuous {:?}",
            v.failures, v.vacuous
        ));
    }
    ran += 1;

    // 5. Sideways (the GUI pass's F-b): one frame whose content is 40px wider than the list is the
    //    one failure, on a run that is otherwise the still reader of case 1.
    let mut raw = synthetic_parked_run(&[], &[], true);
    raw["frames"][300]["sw"] = json!(576.0);
    let v = verdict_of(raw);
    if v.failures.len() != 1 || !v.failures[0].starts_with("sideways:") || !v.vacuous.is_empty() {
        return Err(format!(
            "one frame 40px wider than the list must be the one failure; failures {:?}, vacuous {:?}",
            v.failures, v.vacuous
        ));
    }
    ran += 1;

    // 5b. ...but not the one frame sampled as the list's width changes, before the page's observer
    //     has re-measured it; the same overflow in the frame after it still is.
    let narrow = |raw: &mut Value, from: usize| {
        for f in raw["frames"].as_array_mut().unwrap().iter_mut().skip(from) {
            f["cw"] = json!(330.0);
            f["sw"] = json!(330.0);
        }
    };
    let mut raw = synthetic_parked_run(&[], &[], true);
    narrow(&mut raw, 300);
    raw["frames"][300]["sw"] = json!(536.0);
    let v = verdict_of(raw);
    if !v.failures.is_empty() || !v.vacuous.is_empty() {
        return Err(format!(
            "the frame sampled as the list narrows is not judged; failures {:?}, vacuous {:?}",
            v.failures, v.vacuous
        ));
    }
    let mut raw = synthetic_parked_run(&[], &[], true);
    narrow(&mut raw, 300);
    raw["frames"][300]["sw"] = json!(536.0);
    raw["frames"][301]["sw"] = json!(536.0);
    let v = verdict_of(raw);
    if v.failures.len() != 1 || !v.failures[0].starts_with("sideways:") {
        return Err(format!(
            "an overflow still there one frame after the width changed must fail; failures {:?}, vacuous {:?}",
            v.failures, v.vacuous
        ));
    }
    ran += 1;

    // 6. ...and tokens that fit on one line prove nothing about it.
    let mut raw = synthetic_parked_run(&[], &[], true);
    raw["tokens"] = synthetic_tokens(400.0, 536.0);
    let v = verdict_of(raw);
    if v.vacuous.len() != 3 || !v.vacuous.iter().all(|m| m.contains("not wider than the list")) {
        return Err(format!(
            "three tokens narrower than the list must be three vacuity findings; failures {:?}, vacuous {:?}",
            v.failures, v.vacuous
        ));
    }
    ran += 1;

    // 7-10. S9 (the GUI pass's F-a): back at the end a frame after each change passes; left short, or
    //       back only after 400ms, is one failure per change; a change that moves nothing is vacuous.
    let s9 = |shape| judge(config, Scenario::S9, &Probe::parse(&synthetic_s9_run(shape)));
    let v = s9(S9Shape::Fixed);
    if !v.failures.is_empty() || !v.vacuous.is_empty() {
        return Err(format!(
            "S9: a view back at its end a frame after each change must pass; failures {:?}, vacuous {:?}",
            v.failures, v.vacuous
        ));
    }
    ran += 1;
    for (shape, needle) in [
        (S9Shape::Stuck, "was left"),
        (S9Shape::Slow, "sampled frames showed the view short"),
    ] {
        let v = s9(shape);
        if v.failures.len() != 4 || !v.failures.iter().all(|m| m.contains(needle)) || !v.vacuous.is_empty() {
            return Err(format!(
                "S9: each of the four changes must fail with {needle:?}; failures {:?}, vacuous {:?}",
                v.failures, v.vacuous
            ));
        }
        ran += 1;
    }
    let v = s9(S9Shape::RestyleMovesNothing);
    if v.vacuous.len() != 1 || !v.vacuous[0].contains("narrowed the prose measure moved the list's end only") {
        return Err(format!(
            "S9: a restyle that moves nothing must be the one vacuity finding; failures {:?}, vacuous {:?}",
            v.failures, v.vacuous
        ));
    }
    ran += 1;
    Ok(ran)
}

// ---------------------------------------------------------------------------------------------
// BROWSE's `Ctrl+e`/`Ctrl+y`, copying vim's own `:help CTRL-E`/
// `:help CTRL-Y`. This is a real WebKitGTK check for exactly what jsdom (`App.test.tsx`) cannot
// lay out: a real computed line height actually moves `.message-list`'s `scrollTop`, a real capped
// tool-result box (`.tool-result-body`'s own max-height + overflow-y) takes the box-first
// precedence over the list while it has room, and a real re-home moves the cursor once a scroll
// large enough carries its row off screen. It reuses this file's own bring-up (`Replay`, `Step`,
// `themed_document`, the `on_ready`/`eitriAgent` handshake) and the real `agent`/
// `eitri_core::agent_bridge` wire the scenarios above use, but has none of their follow-during-
// streaming machinery (no `INSTRUMENT`, no `Scenario`, no `judge`) -- there is nothing here for
// that judge to weigh, only a handful of direct assertions against one settled history.
// ---------------------------------------------------------------------------------------------

/// `window.__ctrlEYMark(label)`'s own tiny probe, injected instead of the S1-S9 `INSTRUMENT`: the
/// list's scroll geometry, the current row's own capped tool-result box (if any and if expanded),
/// and which row is current -- appended to `window.__ctrlEYLog` under `label` each time a step
/// calls it, so the whole run's log can be read back in one final `JSON.stringify`.
const CTRL_E_Y_PROBE: &str = r#"
(() => {
  window.__ctrlEYLog = [];
  window.__ctrlEYMark = (label) => {
    const l = document.querySelector(".message-list");
    const cur = document.querySelector(".row-current");
    const box = cur ? cur.querySelector(".tool-result-body") : null;
    window.__ctrlEYLog.push({
      label,
      st: l ? l.scrollTop : null,
      sh: l ? l.scrollHeight : null,
      ch: l ? l.clientHeight : null,
      cur: cur ? cur.textContent : null,
      boxSt: box ? box.scrollTop : null,
      boxSh: box ? box.scrollHeight : null,
      boxCh: box ? box.clientHeight : null,
    });
  };
})();
"#;

/// A history long enough to scroll (five rows: a prompt, a `Bash` tool call, four more prompts),
/// with the tool call's own result 80 lines long -- long enough to overflow the product's own
/// capped `.tool-result-body` (260px, `index.css`) once really laid out, which jsdom cannot do.
/// Mirrors `App.test.tsx`'s own `startedAppWithExpandedResult` shape (a prompt, the tool call, more
/// prompts, `j` then `Enter` to reach and expand it) rather than inventing a new one.
fn ctrl_e_y_replay() -> Replay {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::SessionOpened {
        session_id: "ctrl-e-y-test".into(),
        provider_session_id: "claude-test".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/nonexistent/ctrl-e-y".into(),
    });
    let events = [
        AgentDomainEvent::UserPromptSubmitted { text: "r0".into() },
        AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_ctrl_e_y".into(),
            name: "Bash".into(),
            input: json!({ "command": "long" }),
        },
        AgentDomainEvent::ToolCallCompleted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_ctrl_e_y".into(),
            content: json!(numbered_lines("row", 80)),
            is_error: false,
        },
        AgentDomainEvent::UserPromptSubmitted { text: "r1".into() },
        AgentDomainEvent::UserPromptSubmitted { text: "r2".into() },
        AgentDomainEvent::UserPromptSubmitted { text: "r3".into() },
        AgentDomainEvent::UserPromptSubmitted { text: "r4".into() },
    ];
    for event in &events {
        projection.apply(event);
    }

    let tokens = ThemeTokens::fallback();
    let greeting = BackendGreeting {
        kind: BackendKind::Sidecar,
        project_dir: PathBuf::from("/nonexistent/ctrl-e-y"),
        permission_modes: CLIENT_IMPLEMENTED_PERMISSION_MODES,
        expected_verdandi_revision: Some(agent::EXPECTED_VERDANDI_REVISION),
        resumable: Vec::new(),
        account: None,
    };
    let sole_tab = eitri_core::tabs::TabId(1);
    let snapshot = serialize_snapshot_for_js(
        sole_tab,
        &SnapshotView {
            backend: "sidecar",
            conversation_id: Some("conversation-ctrl-e-y"),
            session_id: Some("ctrl-e-y-test"),
            provider_session_id: Some("claude-test".into()),
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
    let on_ready = vec![
        serialize_hello_for_js(&greeting),
        serialize_theme_for_js(&tokens),
        sole_tab_envelope(sole_tab),
        snapshot,
        serialize_pane_focus_for_js(true),
    ];

    // A real `KeyboardEvent`, dispatched on the real focusable root `App.tsx` attaches its BROWSE
    // handler to -- the same kind of synthetic dispatch S5's `parkByKey` (`INSTRUMENT`, above)
    // already relies on in this exact engine, just aimed at `resolveKey`'s own chords instead of
    // the browser's default PageUp action.
    let key = |k: &str, ctrl: bool, shift: bool| -> Step {
        Step::Script(format!(
            "document.querySelector('.agent-ui-conversation').dispatchEvent(new KeyboardEvent('keydown', {{ key: {}, ctrlKey: {ctrl}, shiftKey: {shift}, bubbles: true, cancelable: true }}));",
            serde_json::to_string(k).unwrap_or_default()
        ))
    };
    let mark = |label: &str| -> Step {
        Step::Script(format!(
            "window.__ctrlEYMark({});",
            serde_json::to_string(label).unwrap_or_default()
        ))
    };

    let mut steps: Vec<Step> = Vec::new();
    for _ in 0..SETTLE_AFTER_SNAPSHOT_TICKS {
        steps.push(Step::Idle);
    }
    steps.push(key("g", false, false));
    steps.push(key("g", false, false)); // gg: top, cursor on "r0"
    steps.push(mark("top"));
    steps.push(key("j", false, false)); // down onto the tool row
    steps.push(key("Enter", false, false)); // unfold its result -- `.tool-result-body` exists now
    steps.push(mark("expanded"));
    steps.push(key("e", true, false)); // Ctrl+e: the box first, while it has room
    steps.push(mark("box-e1"));
    steps.push(key("y", true, false)); // Ctrl+y: the box back up
    steps.push(mark("box-y1"));
    steps.push(key("G", false, true)); // Shift+G: the very last row, "r4", at the very end
    steps.push(mark("bottom"));
    steps.push(key("y", true, false)); // Ctrl+y on an ordinary row: one real line up
    steps.push(mark("plain-y1"));
    steps.push(key("e", true, false)); // Ctrl+e: one real line back down
    steps.push(mark("plain-e1"));
    steps.push(key("9", false, false)); // a count -- "99 Ctrl+y", far more than one screen's worth
    steps.push(key("9", false, false));
    // Ctrl+y first sends a bare `Control` keydown with `ctrlKey: true`, then a `y` keydown.
    // Send both after the digits to check that the bare modifier preserves `countRef`.
    // Going straight to `key("y", true, false)` skips the separate `Control` event,
    // so it cannot catch the modifier clearing the count before the chord.
    steps.push(key("Control", true, false));
    steps.push(key("y", true, false));
    steps.push(mark("rehomed"));

    Replay { on_ready, steps }
}

/// One run: a fresh window, a fresh WebView, `ctrl_e_y_replay`'s own steps, the log. Structurally
/// `run_one` without a `Scenario` or its `INSTRUMENT` -- see this section's own header for why a
/// second near-duplicate function is simpler and safer here than bending that one to a check it was
/// never shaped for.
fn run_ctrl_e_y(config: Config) -> Result<Value, String> {
    let replay = ctrl_e_y_replay();
    let window = gtk4::Window::new();
    let (window_width, window_height) = config.window_size();
    window.set_default_size(window_width, window_height);
    window.set_title(Some(&format!("panel_stream_scroll ctrl_e_y {config:?}")));

    let content_manager = UserContentManager::new();
    content_manager.add_script(&UserScript::new(
        CTRL_E_Y_PROBE,
        UserContentInjectedFrames::TopFrame,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    ));
    let webview = WebView::builder().user_content_manager(&content_manager).build();
    webview.set_hexpand(true);
    webview.set_vexpand(true);
    webview.set_zoom_level(config.zoom);
    window.set_child(Some(&webview));

    let queue: Rc<RefCell<VecDeque<String>>> = Rc::new(RefCell::new(VecDeque::new()));
    let started = Rc::new(Cell::new(false));
    content_manager.register_script_message_handler("eitriAgent", None);
    {
        let queue = queue.clone();
        let started = started.clone();
        let on_ready = replay.on_ready;
        content_manager.connect_script_message_received(Some("eitriAgent"), move |_manager, js_value| {
            if let Some(InboundMessage::Ready { request_id }) = parse_inbound_message(&js_value.to_str()) {
                if !started.replace(true) {
                    let mut q = queue.borrow_mut();
                    q.extend(on_ready.iter().cloned());
                    q.push_back(serialize_command_result_for_js(&request_id, Ok(())));
                }
            }
        });
    }
    webview.load_html(&themed_document(&ThemeTokens::fallback().css_vars()), None);
    window.present();

    let main_loop = glib::MainLoop::new(None, false);
    let outcome: Rc<RefCell<Option<Result<Value, String>>>> = Rc::new(RefCell::new(None));
    let step_index = Rc::new(Cell::new(0usize));
    let ready_drained = Rc::new(Cell::new(false));
    let steps: Rc<Vec<Step>> = Rc::new(replay.steps);
    {
        let window = window.clone();
        let webview = webview.clone();
        let main_loop = main_loop.clone();
        let outcome = outcome.clone();
        let queue = queue.clone();
        let started = started.clone();
        glib::timeout_add_local(TICK, move || {
            if !started.get() {
                return glib::ControlFlow::Continue;
            }
            if let Some(payload) = queue.borrow_mut().pop_front() {
                evaluate_js_dispatch(&webview, &payload);
                return glib::ControlFlow::Continue;
            }
            if !ready_drained.replace(true) {
                return glib::ControlFlow::Continue;
            }
            let i = step_index.get();
            if i < steps.len() {
                match &steps[i] {
                    Step::Dispatch(payload) => evaluate_js_dispatch(&webview, payload),
                    Step::Idle => {}
                    Step::Resize { width, height } => window.set_default_size(
                        (f64::from(window_width) * width).round() as i32,
                        (f64::from(window_height) * height).round() as i32,
                    ),
                    Step::Script(js) => {
                        webview.evaluate_javascript(js, None, None, None::<&gtk4::gio::Cancellable>, |result| {
                            if let Err(e) = result {
                                eprintln!("[panel_stream_scroll] ctrl_e_y step failed: {e}");
                            }
                        })
                    }
                }
                step_index.set(i + 1);
                return glib::ControlFlow::Continue;
            }
            let outcome = outcome.clone();
            let main_loop = main_loop.clone();
            webview.evaluate_javascript(
                "JSON.stringify(window.__ctrlEYLog)",
                None,
                None,
                None::<&gtk4::gio::Cancellable>,
                move |r| {
                    let parsed = r
                        .map_err(|e| format!("reading the log failed: {e}"))
                        .and_then(|v| serde_json::from_str::<Value>(&v.to_str()).map_err(|e| format!("log JSON: {e}")));
                    outcome.borrow_mut().get_or_insert(parsed);
                    main_loop.quit();
                },
            );
            glib::ControlFlow::Break
        });
    }
    {
        let outcome = outcome.clone();
        let main_loop = main_loop.clone();
        glib::timeout_add_local_once(RUN_TIMEOUT, move || {
            outcome
                .borrow_mut()
                .get_or_insert(Err(format!("run did not finish within {RUN_TIMEOUT:?}")));
            main_loop.quit();
        });
    }
    main_loop.run();
    window.destroy();
    let ctx = glib::MainContext::default();
    for _ in 0..50 {
        while ctx.iteration(false) {}
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = outcome.borrow_mut().take();
    result.unwrap_or_else(|| Err("no outcome".into()))
}

/// Reads `ctrl_e_y_replay`'s marks back and checks the real WebKitGTK layout did what jsdom
/// (`App.test.tsx`) can only assert on stubbed rects.
fn check_ctrl_e_y(log: &[Value]) -> Result<(), String> {
    let by_label = |label: &str| -> Result<&Value, String> {
        log.iter()
            .find(|v| text(v, "label").as_deref() == Some(label))
            .ok_or_else(|| format!("no mark named {label:?} in the log of {} entries", log.len()))
    };
    let top = by_label("top")?;
    if num(top, "st") != Some(0.0) {
        return Err(format!("gg did not scroll the conversation to the very top: {top}"));
    }
    let expanded = by_label("expanded")?;
    let box_sh = num(expanded, "boxSh").ok_or("no .tool-result-body after Enter -- the tool row never expanded")?;
    let box_ch = num(expanded, "boxCh").ok_or("no .tool-result-body after Enter -- the tool row never expanded")?;
    if box_sh <= box_ch + 1.0 {
        return Err(format!(
            "the tool result's own box is not actually scrollable in this real layout (scrollHeight {box_sh}, clientHeight {box_ch}) -- the corpus needs more lines"
        ));
    }
    let box_e1 = by_label("box-e1")?;
    let box_st_e1 = num(box_e1, "boxSt").ok_or("no box after Ctrl+e")?;
    if box_st_e1 <= 0.0 {
        return Err(format!("Ctrl+e did not scroll the tool result's own box: {box_e1}"));
    }
    if num(box_e1, "st") != num(expanded, "st") {
        return Err(format!(
            "Ctrl+e moved the conversation itself while the box still had room to scroll: {expanded} then {box_e1}"
        ));
    }
    let box_y1 = by_label("box-y1")?;
    let box_st_y1 = num(box_y1, "boxSt").ok_or("no box after Ctrl+y")?;
    if box_st_y1 >= box_st_e1 {
        return Err(format!(
            "Ctrl+y did not scroll the tool result's own box back up: {box_e1} then {box_y1}"
        ));
    }
    let bottom = by_label("bottom")?;
    let bottom_cur = text(bottom, "cur").unwrap_or_default();
    if !bottom_cur.contains("r4") {
        return Err(format!(
            "Shift+G did not land the cursor on the last row (\"r4\"): {bottom}"
        ));
    }
    let bottom_st = num(bottom, "st").ok_or("no list after Shift+G")?;
    let plain_y1 = by_label("plain-y1")?;
    let plain_y1_st = num(plain_y1, "st").ok_or("no list after the plain Ctrl+y")?;
    if plain_y1_st >= bottom_st {
        return Err(format!(
            "Ctrl+y did not scroll the conversation by a real line on an ordinary row: {bottom_st} then {plain_y1_st}"
        ));
    }
    if !text(plain_y1, "cur").unwrap_or_default().contains("r4") {
        return Err(format!(
            "one line's worth of Ctrl+y moved the cursor off a row still on screen: {plain_y1}"
        ));
    }
    let plain_e1 = by_label("plain-e1")?;
    let plain_e1_st = num(plain_e1, "st").ok_or("no list after the plain Ctrl+e")?;
    if plain_e1_st <= plain_y1_st {
        return Err(format!(
            "Ctrl+e did not scroll back down by a real line: {plain_y1_st} then {plain_e1_st}"
        ));
    }
    let rehomed = by_label("rehomed")?;
    let rehomed_cur = text(rehomed, "cur").unwrap_or_default();
    if rehomed_cur.contains("r4") {
        return Err(format!(
            "99 Ctrl+y did not carry the last row off screen and re-home the cursor: {rehomed}"
        ));
    }
    Ok(())
}

fn trace_dir() -> PathBuf {
    std::env::var_os("PANEL_STREAM_SCROLL_TRACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("panel_stream_scroll"))
}

/// The `tabs` envelope a real launch's `TabSet` sends before the active tab's snapshot (2026-09-29).
/// `tabs.ts`'s `acceptsEnvelope` drops a snapshot for a tab the page does not know yet while
/// `activeTab` is still `null`, so without it nothing rendered and every scenario read "did not
/// exercise the trigger" (no message list), `ctrl_e_y` "gg did not scroll" -- since session tabs moved
/// the panel onto `TabSet` (2026-09-25), and unseen because the test was not run until the display
/// guard's own verification. `panel_visual_mode.rs` found and fixed the same harness bug on
/// 2026-09-28 (its `on_ready_batch`); this is that envelope. One live tab, the id every snapshot
/// here is built for.
fn sole_tab_envelope(sole_tab: eitri_core::tabs::TabId) -> String {
    let tabs = vec![TabView {
        id: sole_tab,
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
    serialize_tabs_for_js(sole_tab, &tabs, SessionModeChoice::Auto)
}

fn main() {
    // The judge's own tests need no display, so they run every time, `--ignored` or not.
    match judge_self_test() {
        Ok(n) => println!("panel_stream_scroll: the judge's {n} self-tests pass"),
        Err(e) => {
            eprintln!("panel_stream_scroll: the judge's self-test failed: {e}");
            std::process::exit(1);
        }
    }
    // Mirrors `#[ignore]` under a plain `main`: `cargo test --workspace` needs no display.
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("panel_stream_scroll: ignored (drives a real WebKitGTK WebView); run with `-- --ignored`");
        return;
    }
    // Its own Xvfb, GTK initialised on it and checked to be on it -- never an inherited display.
    let server = own_x_server::init_gtk("panel_stream_scroll", "1920x1200x24");
    let mut failed: Vec<String> = Vec::new();

    // Run this first and unconditionally: it is outside `Scenario::ALL`, so
    // `PANEL_STREAM_SCROLL_ONLY` does not filter it. One configuration covers the arithmetic
    // and box precedence, which use the same document at every size below.
    match run_ctrl_e_y(config(560, 740, 1.0)) {
        Err(e) => {
            println!("\n[ctrl_e_y] ERROR: {e}");
            failed.push(format!("ctrl_e_y: {e}"));
        }
        Ok(raw) => {
            let log: Vec<Value> = raw.as_array().cloned().unwrap_or_default();
            match check_ctrl_e_y(&log) {
                Ok(()) => println!("\n[ctrl_e_y] pass"),
                Err(e) => {
                    println!("\n[ctrl_e_y] FAIL: {e}");
                    failed.push(format!("ctrl_e_y: {e}"));
                }
            }
        }
    }

    let only: Option<Vec<String>> = std::env::var("PANEL_STREAM_SCROLL_ONLY")
        .ok()
        .map(|s| s.split(',').map(|p| p.trim().to_string()).collect());
    let scenarios: Vec<Scenario> = Scenario::ALL
        .into_iter()
        .filter(|s| only.as_ref().is_none_or(|o| o.iter().any(|n| n == s.name())))
        .collect();

    let replay = build_replay(0, TOOL_RUN_TICKS, false);
    let replay_s7 = build_replay(S7_QUIET_BEFORE_TOOL_TICKS, S7_TOOL_RUN_TICKS, false);
    let replay_s9 = build_replay(0, TOOL_RUN_TICKS, true);
    let dir = trace_dir();
    let _ = std::fs::create_dir_all(&dir);
    println!(
        "panel_stream_scroll: {} steps per turn replay ({} for S7, {} for S9), {} scenario(s) x {} configuration(s); traces in {}",
        replay.steps.len(),
        replay_s7.steps.len(),
        replay_s9.steps.len(),
        scenarios.len(),
        CONFIGS.len(),
        dir.display()
    );
    for config in CONFIGS {
        for &scenario in &scenarios {
            let label = format!(
                "{} {}x{} zoom {}",
                scenario.name(),
                config.width,
                config.height,
                config.zoom
            );
            let replay = match scenario {
                Scenario::S7 => &replay_s7,
                Scenario::S9 => &replay_s9,
                _ => &replay,
            };
            match run_one(config, scenario, replay) {
                Err(e) => {
                    println!("\n[{label}] ERROR: {e}");
                    failed.push(format!("{label}: {e}"));
                }
                Ok(raw) => {
                    let file = dir.join(format!(
                        "{}-{}x{}-z{}.json",
                        scenario.name(),
                        config.width,
                        config.height,
                        config.zoom
                    ));
                    let _ = std::fs::write(&file, raw.to_string());
                    let probe = Probe::parse(&raw);
                    let verdict = judge(config, scenario, &probe);
                    let status = if !verdict.vacuous.is_empty() {
                        "DID NOT EXERCISE THE TRIGGER"
                    } else if !verdict.failures.is_empty() {
                        "FAIL"
                    } else {
                        "pass"
                    };
                    println!("\n[{label}] {status}");
                    for line in &verdict.lines {
                        println!("  {line}");
                    }
                    for vac in &verdict.vacuous {
                        println!("  vacuous: {vac}");
                        failed.push(format!("{label}: did not exercise the trigger: {vac}"));
                    }
                    for fail in &verdict.failures {
                        println!("  FAIL: {fail}");
                        failed.push(format!("{label}: {fail}"));
                    }
                }
            }
        }
    }

    println!();
    if failed.is_empty() {
        println!("panel_stream_scroll: every scenario passed");
    } else {
        println!("panel_stream_scroll: {} failure(s):", failed.len());
        for f in &failed {
            println!("  - {f}");
        }
        server.exit(1);
    }
}
