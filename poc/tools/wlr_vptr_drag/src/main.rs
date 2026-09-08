//! Minimal `wlr-virtual-pointer-unstable-v1` client that can do what `wlrctl`
//! cannot: a real press → move → release drag sequence, addressed to one
//! specific `$WAYLAND_DISPLAY` socket. See the crate-level doc comment at the
//! bottom of this file for why this exists.
use std::env;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, Instant};

use wayland_client::{
    delegate_noop,
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_pointer, wl_registry, wl_seat},
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

struct AppState;

// The registry event we actually care about (global add/remove) is already
// captured by GlobalListContents itself; we don't need to react to it here.
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(AppState: ignore wl_seat::WlSeat);
delegate_noop!(AppState: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(AppState: ignore ZwlrVirtualPointerV1);

struct Args {
    x1: u32,
    y1: u32,
    x2: u32,
    y2: u32,
    button: u32,
    hold_ms: u64,
    steps: u32,
    screen_w: u32,
    screen_h: u32,
    /// Total press/release cycles to emit at (x1, y1). `1` (the default) is the original
    /// behavior. `2`/`3` produce a real double/triple click -- all cycles delivered over one
    /// already-open Wayland connection, which is the point: two separate invocations of this
    /// binary pay process startup + connection setup between them, which can push the gap past
    /// Neovim's own `'mousetime'` (500 ms) multi-click window and make a genuine negative result
    /// indistinguishable from a tooling artifact.
    clicks: u32,
    /// Gap between the release of one click and the press of the next, in ms.
    click_gap_ms: u64,
}

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

fn usage() -> ! {
    eprintln!(
        "usage: wlr-vptr-drag <x1> <y1> <x2> <y2> \
         [--button left|right|middle] [--hold-ms N] [--steps N] \
         [--clicks N] [--click-gap-ms N] [--screen-w W] [--screen-h H]\n\n\
         --clicks N emits N press/release cycles at (x1, y1) over one open \
         connection before the final one drags to (x2, y2) -- pass \
         --clicks 2/3 with x1==x2, y1==y2 and --steps 1 for a real \
         double/triple click. Doing it in-process matters: two separate \
         invocations pay process + connection startup between them, which \
         can exceed Neovim's own 'mousetime' multi-click window.\n\n\
         Requires $WAYLAND_DISPLAY to be set explicitly -- this tool refuses \
         to fall back to a default socket, since it exists specifically to \
         avoid ever accidentally targeting the real desktop session. Point \
         it at an isolated sandbox compositor, e.g.:\n\
         WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 sway --config /dev/null &\n\
         WAYLAND_DISPLAY=wayland-1 wlr-vptr-drag 100 100 400 300\n\n\
         --screen-w/--screen-h default to 1280x720, the wlroots headless \
         backend's default single-output resolution -- pass the real \
         values if your sandbox output differs.\n\n\
         Coordinates are absolute pixel positions and are sent via \
         motion_absolute with x_extent=--screen-w, y_extent=--screen-h -- \
         this is only pixel-accurate for a single-output layout with that \
         output at (0,0), which is what the project's headless-sway sandbox \
         always is. Multi-output real desktops would need per-output extent \
         math this tool does not implement (and should never be pointed at \
         a real desktop anyway)."
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut positional = Vec::new();
    let mut button = BTN_LEFT;
    let mut hold_ms = 150u64;
    let mut steps = 10u32;
    let mut screen_w = 1280u32;
    let mut screen_h = 720u32;
    let mut clicks = 1u32;
    let mut click_gap_ms = 40u64;

    let mut it = env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--button" => {
                let v = it.next().unwrap_or_else(|| usage());
                button = match v.as_str() {
                    "left" => BTN_LEFT,
                    "right" => BTN_RIGHT,
                    "middle" => BTN_MIDDLE,
                    _ => usage(),
                };
            }
            "--hold-ms" => hold_ms = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--steps" => steps = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--clicks" => clicks = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--click-gap-ms" => {
                click_gap_ms = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage())
            }
            "--screen-w" => screen_w = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--screen-h" => screen_h = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "-h" | "--help" => usage(),
            other => positional.push(other.to_string()),
        }
    }

    if positional.len() != 4 {
        usage();
    }
    let parse_u32 = |s: &str| s.parse::<u32>().unwrap_or_else(|_| usage());
    Args {
        x1: parse_u32(&positional[0]),
        y1: parse_u32(&positional[1]),
        x2: parse_u32(&positional[2]),
        y2: parse_u32(&positional[3]),
        button,
        hold_ms,
        steps: steps.max(1),
        screen_w,
        screen_h,
        clicks: clicks.max(1),
        click_gap_ms,
    }
}

fn main() -> ExitCode {
    // Hard safety rule: never fall back to a default/ambient socket. The
    // caller must explicitly point this at an isolated sandbox compositor.
    // See docs/neovibe_feasibility_status.md's P3 section and
    // poc/docs/synthetic-input-findings.md for why this is non-negotiable
    // in this project -- ambient uinput-based injection (ydotool) already
    // caused a real near-miss incident against the live desktop session.
    let wayland_display = match env::var("WAYLAND_DISPLAY") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!(
                "error: $WAYLAND_DISPLAY is not set. Refusing to guess a \
                 default socket -- point this explicitly at an isolated \
                 sandbox compositor, never the real desktop session."
            );
            return ExitCode::FAILURE;
        }
    };
    eprintln!("wlr-vptr-drag: targeting WAYLAND_DISPLAY={wayland_display}");

    let args = parse_args();

    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to connect to {wayland_display}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let (globals, mut queue) = match registry_queue_init::<AppState>(&conn) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: registry init failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let qh = queue.handle();
    let mut state = AppState;

    let manager = match globals.bind::<ZwlrVirtualPointerManagerV1, _, _>(&qh, 1..=2, ()) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "error: compositor at {wayland_display} does not expose \
                 zwlr_virtual_pointer_manager_v1: {e}"
            );
            return ExitCode::FAILURE;
        }
    };

    // A seat is optional per-protocol ("a suggestion to the compositor");
    // bind one opportunistically if present but don't fail without it.
    let seat = globals.bind::<wl_seat::WlSeat, _, _>(&qh, 1..=1, ()).ok();

    let pointer = manager.create_virtual_pointer(seat.as_ref(), &qh, ());

    let start = Instant::now();
    let now_ms = |start: Instant| -> u32 { start.elapsed().as_millis() as u32 };

    let flush_and_wait = |queue: &mut wayland_client::EventQueue<AppState>, state: &mut AppState, ms: u64| {
        let _ = queue.roundtrip(state);
        if ms > 0 {
            sleep(Duration::from_millis(ms));
        }
    };

    eprintln!(
        "drag: ({}, {}) -> ({}, {}) button=0x{:x} steps={} hold_ms={} clicks={} screen={}x{}",
        args.x1, args.y1, args.x2, args.y2, args.button, args.steps, args.hold_ms, args.clicks, args.screen_w, args.screen_h
    );

    // 1. Move to the start position and settle there before pressing --
    //    mirrors how a real drag begins (cursor already at the target,
    //    button goes down, *then* movement starts).
    pointer.motion_absolute(now_ms(start), args.x1, args.y1, args.screen_w, args.screen_h);
    pointer.frame();
    flush_and_wait(&mut queue, &mut state, 30);

    // 1b. Any leading clicks of a multi-click gesture -- all but the last, which is the
    //     press/drag/release below. Emitted over this same open connection so the gap between
    //     them is only `--click-gap-ms`, not that plus process startup.
    for _ in 1..args.clicks {
        pointer.button(now_ms(start), args.button, wl_pointer::ButtonState::Pressed);
        pointer.frame();
        flush_and_wait(&mut queue, &mut state, args.click_gap_ms.min(20).max(5));
        pointer.button(now_ms(start), args.button, wl_pointer::ButtonState::Released);
        pointer.frame();
        flush_and_wait(&mut queue, &mut state, args.click_gap_ms);
    }

    // 2. Press.
    pointer.button(now_ms(start), args.button, wl_pointer::ButtonState::Pressed);
    pointer.frame();
    flush_and_wait(&mut queue, &mut state, 30);

    // 3. Step motion toward the end position. Real drag-threshold detection
    //    in GTK/GDK (and elsewhere) keys off seeing multiple intermediate
    //    motion events, not a single teleport -- so this steps, it doesn't
    //    jump.
    for i in 1..=args.steps {
        let t = i as f64 / args.steps as f64;
        let x = (args.x1 as f64 + (args.x2 as f64 - args.x1 as f64) * t).round() as u32;
        let y = (args.y1 as f64 + (args.y2 as f64 - args.y1 as f64) * t).round() as u32;
        pointer.motion_absolute(now_ms(start), x, y, args.screen_w, args.screen_h);
        pointer.frame();
        flush_and_wait(&mut queue, &mut state, args.hold_ms / args.steps as u64 + 1);
    }

    // 4. Hold briefly at the end position before releasing (some UIs sample
    //    a "drop" only once motion has been still for a moment).
    flush_and_wait(&mut queue, &mut state, args.hold_ms);

    // 5. Release.
    pointer.button(now_ms(start), args.button, wl_pointer::ButtonState::Released);
    pointer.frame();
    flush_and_wait(&mut queue, &mut state, 30);

    pointer.destroy();
    manager.destroy();
    let _ = queue.roundtrip(&mut state);

    eprintln!("drag: done");
    ExitCode::SUCCESS
}

// Why this tool exists: `wlrctl pointer` (the project's existing sandboxed
// mouse-injection tool, wrapping this same protocol) only exposes an atomic
// `click` request -- there is no way to compose press, move, and release as
// separate steps, so real click-and-drag (text-selection drag, dragging a
// GtkPaned splitter) could not be tested at all. This binary implements just
// enough of the protocol directly (via wayland-client + the
// wayland-protocols-wlr crate's generated bindings, no hand-rolled framing)
// to close that gap. It also implements absolute positioning, which `wlrctl`
// lacks entirely (it only has relative motion).
