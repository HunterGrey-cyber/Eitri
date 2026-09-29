//! A web module's `WebView` sits at (0,0) of its `WebHost`, in real GTK (2026-09-29).
//!
//! **What this holds.** WebKitGTK 2.52.6 adds `gtk_widget_get_allocation(webView)`'s x and y -- the
//! `WebView`'s position in its parent -- to the caret rectangle it gives the input method, and GTK4's
//! input methods translate that rectangle from the `WebView` to the window again, so the candidate
//! window lands off by that position (`shell/src/web_host.rs`'s module doc has the source lines).
//! `WebHost` (compiled in from `src/` below) exists to make that addend (0,0) wherever the module is.
//! This places a stand-in for the `WebView` where the module grid puts the agent panel in the default
//! split, 761 logical px from the left, and reads the same `gtk_widget_get_allocation` WebKit reads:
//!
//! - **direct** (the positive control): the stand-in placed at x=761 itself reports x=761, the error
//!   the owner saw. A run where it does not fails as "did not exercise the trigger";
//! - **wrapped**: the stand-in in a `WebHost` placed at x=761 reports (0,0) and the host's whole size,
//!   even though margins and a centred alignment were set on it beforehand, while its origin in the
//!   window is still x=761 -- the input method's own translation, which is then the only one;
//! - **one child**: the host's only child is the stand-in;
//! - **focus**: the host is not focusable itself, and a grab on it lands on the stand-in, as a grab on
//!   the stand-in does (review round 1: code handed only the grid's hosts still gives the `WebView`
//!   the keys, though `ModuleGrid::add` takes a focus target so module focus never needs this).
//!
//! A `gtk4::Fixed` stands in for the grid: both allocate a child with a translation, which is where
//! `gtk_widget_get_allocation`'s x and y come from. The stand-in is a focusable `DrawingArea`, not a
//! `WebView`: the allocation read here is GTK's, whatever the widget. Not held here: that the input
//! method is then sent the right rectangle (the sandbox pass of 2026-09-29 read that off fcitx5's bus
//! with the same change), and that capture-phase key controllers on the host see the `WebView`'s keys
//! (GTK has no public way to synthesize a key event; `gtk_propagate_event_internal` is read, not run).
//!
//! Needs a display, and never the real desktop. Run it only inside a private `/tmp` and runtime dir,
//! under its own X server that listens on no socket file, with GTK told to use it rather than a
//! Wayland session the shell may have exported. Build first (outside is fine), then --
//!
//!     bwrap --dev-bind / / --tmpfs /tmp --tmpfs /run/user/$(id -u) --unsetenv WAYLAND_DISPLAY \
//!         --unsetenv DISPLAY --unsetenv XAUTHORITY --setenv TMPDIR /tmp -- sh -c '
//!       Xvfb :573 -nolisten tcp -nolisten unix -screen 0 1600x900x24 & x=$!; sleep 1
//!       DISPLAY=:573 GDK_BACKEND=x11 cargo test -p shell --test web_host_origin -- --ignored; r=$?
//!       kill $x; exit $r'
//!
//! Not `xvfb-run -a` on the host: it picks a display by lock file, so beside a desktop whose X socket
//! has no lock file (this machine's `/tmp/.X11-unix/X0` on 2026-09-29) it can take `:0` and unlink
//! that socket. `-nolisten unix` leaves only the abstract socket (the wrapper shares the network
//! namespace, so the test still reaches it), and the private `/tmp` keeps `/tmp/.X11-unix` apart.
//!
//! A plain `main` (`harness = false`), because GTK must own the main thread; it refuses to start
//! unless `GDK_BACKEND=x11`. About 1s.

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
