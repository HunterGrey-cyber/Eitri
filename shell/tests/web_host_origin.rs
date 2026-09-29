//! Checks that a web module's WebView has allocation origin (0,0) inside WebHost.
//! WebKitGTK 2.52.6 adds the WebView's parent-relative allocation offset to the caret rectangle
//! it sends to the input method. GTK then translates from the WebView to the window again.
//! WebHost makes that extra offset zero so the candidate window is not displaced.
//!
//! A focusable DrawingArea stands in for WebView in a gtk4::Fixed container.
//! The direct control is placed at x=761 and must report that allocation offset.
//! Wrapped in WebHost at x=761, it must report (0,0) and fill the host despite prior margins
//! and centred alignment. Its window-relative origin must still be x=761.
//! The host must have one child and forward focus grabs to it without being focusable itself.
//!
//! This checks GTK allocation and focus forwarding. It does not check the rectangle delivered
//! to an input method or capture-phase key delivery from a WebView.
//!
//! Run with an isolated /tmp and runtime directory under an X server with no filesystem socket.
//! Unset inherited displays and XAUTHORITY, and explicitly select the X11 backend. Build first, then:
//!
//!     bwrap --dev-bind / / --tmpfs /tmp --tmpfs /run/user/$(id -u) --unsetenv WAYLAND_DISPLAY \
//!         --unsetenv DISPLAY --unsetenv XAUTHORITY --setenv TMPDIR /tmp -- sh -c '
//!       Xvfb :573 -nolisten tcp -nolisten unix -screen 0 1600x900x24 & x=$!; sleep 1
//!       DISPLAY=:573 GDK_BACKEND=x11 cargo test -p shell --test web_host_origin -- --ignored; r=$?
//!       kill $x; exit $r'
//!
//! xvfb-run chooses a display by lock file and can unlink a desktop socket that has no lock file.
//! The isolated /tmp separates socket paths, and -nolisten unix leaves only the abstract socket.
//! The wrapper shares the network namespace so the test can reach that socket.
//! A plain main with `harness = false` keeps GTK on the main thread and requires GDK_BACKEND=x11.

use std::time::{Duration, Instant};

use gtk4::prelude::*;

// `dead_code`: the product calls more of the module than this test does.
#[allow(dead_code)]
#[path = "../src/web_host.rs"]
mod web_host;

/// Where the module grid allocated the agent panel's `WebView` in the default split of the
/// 2026-09-29 sandbox pass (1280x760 window, editor 760px, a 1px divider).
const MODULE_X: f64 = 761.0;
const MODULE_W: i32 = 519;
const MODULE_H: i32 = 600;

fn pump(ms: u64) {
    let context = gtk4::glib::MainContext::default();
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        while context.iteration(false) {}
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// What `InputMethodFilter::platformTransformCursorRectToViewCoordinates` adds to the caret.
#[allow(deprecated)]
fn webkit_addend(widget: &impl IsA<gtk4::Widget>) -> (i32, i32, i32, i32) {
    let a = widget.allocation();
    (a.x(), a.y(), a.width(), a.height())
}

fn stand_in() -> gtk4::DrawingArea {
    let area = gtk4::DrawingArea::new();
    area.set_focusable(true);
    area.set_size_request(MODULE_W, MODULE_H);
    area
}

fn run() -> Result<Vec<String>, String> {
    let app = gtk4::Application::builder()
        .application_id("cn.huntergrey.neovibe.test.webhostorigin")
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gtk4::gio::Cancellable>)
        .map_err(|e| e.to_string())?;
    let window = gtk4::ApplicationWindow::new(&app);
    window.set_default_size(1400, 700);
    let fixed = gtk4::Fixed::new();
    window.set_child(Some(&fixed));

    // The positive control: in the grid directly, as before 2026-09-29.
    let direct = stand_in();
    fixed.put(&direct, MODULE_X, 0.0);
    // The fix: the same, in a `WebHost`, with everything that could move it inside the host set first.
    let content = stand_in();
    content.set_margin_start(12);
    content.set_margin_top(7);
    content.set_halign(gtk4::Align::Center);
    content.set_valign(gtk4::Align::End);
    let host = web_host::WebHost::new(&content);
    fixed.put(&host, MODULE_X, 0.0);
    window.present();
    pump(300);

    let mut report = Vec::new();
    let direct_addend = webkit_addend(&direct);
    if direct_addend.0 != MODULE_X as i32 {
        return Err(format!(
            "did not exercise the trigger: the stand-in placed at x={MODULE_X} directly reports {direct_addend:?}"
        ));
    }
    report.push(format!("direct: WebKit would add {direct_addend:?}"));

    let host_addend = webkit_addend(&host);
    let addend = webkit_addend(&content);
    if (addend.0, addend.1) != (0, 0) {
        return Err(format!("wrapped: WebKit would add {addend:?}, not (0,0)"));
    }
    if (addend.2, addend.3) != (host_addend.2, host_addend.3) {
        return Err(format!(
            "wrapped: the content is {addend:?}, not the host's whole size {host_addend:?}"
        ));
    }
    let origin = content
        .compute_point(&window, &gtk4::graphene::Point::new(0.0, 0.0))
        .ok_or("the content has no origin in the window")?;
    if (origin.x() as f64 - MODULE_X).abs() > 0.5 {
        return Err(format!(
            "wrapped: the content's origin in the window is {origin:?}, not x={MODULE_X}"
        ));
    }
    report.push(format!(
        "wrapped: WebKit would add {addend:?}; the content's origin in the window is ({}, {})",
        origin.x(),
        origin.y()
    ));

    let children: Vec<gtk4::Widget> = std::iter::successors(host.first_child(), |c| c.next_sibling()).collect();
    if children.len() != 1 || children[0] != content.clone().upcast::<gtk4::Widget>() {
        return Err(format!(
            "the host has {} children, not exactly its content",
            children.len()
        ));
    }
    if host.content().as_ref() != Some(content.upcast_ref::<gtk4::Widget>()) {
        return Err("WebHost::content is not the child it was given".into());
    }
    report.push("one child: the content".into());

    if host.is_focusable() {
        return Err("the host is focusable: the keys could stop on it instead of the content".into());
    }
    // From the control each time, so a grab that does nothing cannot pass as one that worked.
    if !direct.grab_focus() || !direct.is_focus() {
        return Err("the control could not take the focus, so the grabs below would prove nothing".into());
    }
    if !host.grab_focus() || !content.is_focus() {
        return Err("a grab on the host did not land on its content".into());
    }
    direct.grab_focus();
    if !content.grab_focus() || !content.is_focus() {
        return Err("a grab on the content did not take the window's focus".into());
    }
    report
        .push("focus: the host is not focusable; a grab on it lands on the content, as one on the content does".into());

    window.destroy();
    pump(50);
    Ok(report)
}

fn main() {
    // Mirrors `#[ignore]` under a plain `main`: `cargo test --workspace` needs no display.
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("web_host_origin: ignored (drives real GTK on a display); run with `-- --ignored`");
        return;
    }
    if std::env::var("GDK_BACKEND").as_deref() != Ok("x11") {
        eprintln!(
            "web_host_origin: refusing to open windows without GDK_BACKEND=x11 -- run it under its own X server:"
        );
        eprintln!(
            "    xvfb-run -a -s \"-nolisten tcp -screen 0 1600x900x24\" env GDK_BACKEND=x11 \
             cargo test -p shell --test web_host_origin -- --ignored"
        );
        std::process::exit(1);
    }
    if let Err(e) = gtk4::init() {
        eprintln!("web_host_origin: GTK could not initialise ({e}); it needs an X server of its own");
        std::process::exit(1);
    }
    match run() {
        Ok(report) => {
            for line in report {
                println!("web_host_origin: {line}");
            }
            println!("web_host_origin: ok");
        }
        Err(e) => {
            eprintln!("web_host_origin: FAILED: {e}");
            std::process::exit(1);
        }
    }
}
