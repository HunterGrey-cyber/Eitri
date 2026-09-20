//! DIAGNOSTIC-ONLY companion to `wlr-vptr-drag`, written during the P3 mouse-input
//! verification pass to test a specific hypothesis: `wlrctl pointer scroll` sends a raw
//! `wl_pointer` axis value with no `axis_source` request at all, and some GTK4/GDK Wayland
//! backends silently ignore an axis event with no declared source rather than treating it as a
//! generic/continuous scroll. This sends a real `axis_source(Wheel)` + `axis(...)` + `frame()`
//! sequence -- exactly what a real scroll-wheel notch produces -- to test that hypothesis
//! directly. Not part of the mouse-input feature itself; kept only if useful for future sandbox
//! verification work, per this project's own precedent of building small protocol-level
//! diagnostic tools when `wlrctl` proves to have a real, specific gap (see `main.rs`'s own doc
//! for the press/release gap it closed the same way).
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
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

struct AppState;

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

fn usage() -> ! {
    eprintln!(
        "usage: wlr-vptr-scroll <notches> [--axis vertical|horizontal] [--value V] \
         [--source wheel|finger|continuous]\n\n\
         Sends <notches> separate axis_source+axis+frame request sequences (one real \
         'wheel notch' each, matching a real physical wheel's own event shape), unlike \
         `wlrctl pointer scroll` which sends a bare axis+frame with no axis_source at all. \
         Requires $WAYLAND_DISPLAY set explicitly -- refuses to guess a default socket, same \
         safety rule as wlr-vptr-drag."
    );
    std::process::exit(2);
}

fn main() -> ExitCode {
    let wayland_display = match env::var("WAYLAND_DISPLAY") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("error: $WAYLAND_DISPLAY is not set. Refusing to guess a default socket.");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("wlr-vptr-scroll: targeting WAYLAND_DISPLAY={wayland_display}");

    let mut notches = None;
    let mut axis = wl_pointer::Axis::VerticalScroll;
    let mut value = 10.0f64; // libinput's own convention: ~10 units = one wheel click
    let mut source = wl_pointer::AxisSource::Wheel;

    let mut it = env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--axis" => {
                axis = match it.next().as_deref() {
                    Some("vertical") => wl_pointer::Axis::VerticalScroll,
                    Some("horizontal") => wl_pointer::Axis::HorizontalScroll,
                    _ => usage(),
                };
            }
            "--value" => value = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--source" => {
                source = match it.next().as_deref() {
                    Some("wheel") => wl_pointer::AxisSource::Wheel,
                    Some("finger") => wl_pointer::AxisSource::Finger,
                    Some("continuous") => wl_pointer::AxisSource::Continuous,
                    _ => usage(),
                };
            }
            "-h" | "--help" => usage(),
            other => {
                if notches.is_some() {
                    usage();
                }
                notches = Some(other.parse::<u32>().unwrap_or_else(|_| usage()));
            }
        }
    }
    let notches = notches.unwrap_or_else(|| usage()).max(1);

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
            eprintln!("error: compositor does not expose zwlr_virtual_pointer_manager_v1: {e}");
            return ExitCode::FAILURE;
        }
    };
    let seat = globals.bind::<wl_seat::WlSeat, _, _>(&qh, 1..=1, ()).ok();
    let pointer = manager.create_virtual_pointer(seat.as_ref(), &qh, ());

    let start = Instant::now();
    let now_ms = |start: Instant| -> u32 { start.elapsed().as_millis() as u32 };
    let flush = |queue: &mut wayland_client::EventQueue<AppState>, state: &mut AppState| {
        let _ = queue.roundtrip(state);
    };

    eprintln!("scroll: {notches} notch(es) axis={axis:?} value={value} source={source:?}");
    for _ in 0..notches {
        pointer.axis_source(source);
        pointer.axis(now_ms(start), axis, value);
        pointer.frame();
        flush(&mut queue, &mut state);
        sleep(Duration::from_millis(50));
    }

    pointer.destroy();
    manager.destroy();
    let _ = queue.roundtrip(&mut state);
    eprintln!("scroll: done");
    ExitCode::SUCCESS
}
