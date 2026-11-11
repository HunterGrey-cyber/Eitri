//! A module host's unrealize is told apart from the window's close in real GTK (`src/module_grid/unrealize.rs`).
//!
//! **What this holds.** The grid logs every host's unrealize, loudly when it is not the window closing.
//! Which of the two a given unrealize is comes from the window GTK has already hidden: this file builds
//! the shapes a window has and reads `window_is_closing` from inside the widget's own `unrealize`
//! signal, as the grid does:
//!
//! - **close**: `Window::close()`, which a close request that is let through ends in, and
//!   **destroy**: `Window::destroy()`, which the shell's own teardown calls -- the host is unrealized
//!   once, the window already hidden, and the answer is "closing";
//! - **removed while up**: a host taken out of a window that stays on screen -- the answer is "not
//!   closing", which is the tripwire the line exists for;
//! - **hidden**, **child-visible**: a window hidden, and a host unmapped with `set_child_visible(false)`,
//!   which is how the grid hides one -- no unrealize at all, so the line is never written.
//!
//! Needs a display, and never the real desktop: it starts its own Xvfb (`support/own_x_server.rs`) --
//!
//!     cargo test -p shell --test grid_unrealize_line -- --ignored
//!
//! A plain `main` (`harness = false`), because GTK must own the main thread. About 5 s.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;

#[path = "support/own_x_server.rs"]
mod own_x_server;

#[allow(dead_code)]
#[path = "../src/module_grid/unrealize.rs"]
mod unrealize;

/// Waits for complete frames after a window change.
#[allow(dead_code)]
#[path = "support/frame_turns.rs"]
mod frame_turns;

fn pump() {
    frame_turns::settle();
}

/// What `act` on a shown window did to its one host: the grid's line for each unrealize the host
/// signalled, in order, once `act` has run and again once the window is gone. `act` says whether it took
/// the window down itself; if not, the window is destroyed after it, as the shell's teardown ends one.
fn lines_after(act: impl FnOnce(&gtk4::Window, &gtk4::Box, &gtk4::Button) -> bool) -> (Vec<String>, Vec<String>) {
    let lines = Rc::new(RefCell::new(Vec::new()));
    let window = gtk4::Window::new();
    window.set_default_size(300, 100);
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    let host = gtk4::Button::with_label("host");
    row.append(&host);
    window.set_child(Some(&row));
    {
        let lines = lines.clone();
        host.connect_unrealize(move |widget| {
            let nth = lines.borrow().len() as u32 + 1;
            lines.borrow_mut().push(unrealize::line(
                &"host",
                nth,
                unrealize::window_is_closing(widget.upcast_ref()),
            ));
        });
    }
    window.present();
    pump();
    if !host.is_mapped() {
        let said = vec!["did not exercise the trigger: the host never got on screen".to_string()];
        return (said.clone(), said);
    }
    let down = act(&window, &row, &host);
    pump();
    let during = lines.borrow().clone();
    if !down {
        window.destroy();
        pump();
    }
    let after = lines.borrow().clone();
    (during, after)
}

fn main() {
    // Mirrors `#[ignore]` under a plain `main`: `cargo test --workspace` needs no display.
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("grid_unrealize_line: ignored (drives real GTK on a display); run with `-- --ignored`");
        return;
    }
    let server = own_x_server::init_gtk("grid_unrealize_line", "800x600x24");
    let closing = "[module_grid] host unrealized (1 so far) as its window closes".to_string();
    let fault = "[module_grid] host unrealized (1 so far) -- must not happen before the window closes".to_string();
    let none: Vec<String> = Vec::new();
    // (name, what `act` left, what the window's end left)
    let cases: Vec<(&str, (Vec<String>, Vec<String>), (Vec<String>, Vec<String>))> = vec![
        (
            "close",
            lines_after(|w, _, _| {
                w.close();
                true
            }),
            (vec![closing.clone()], vec![closing.clone()]),
        ),
        (
            "destroy",
            lines_after(|w, _, _| {
                w.destroy();
                true
            }),
            (vec![closing.clone()], vec![closing.clone()]),
        ),
        // The host leaves a window that stays on screen: unrealized once with the window visible, the tripwire.
        (
            "removed while up",
            lines_after(|_, row, host| {
                row.remove(host);
                false
            }),
            (vec![fault.clone()], vec![fault]),
        ),
        // Hiding the window, or a host with `set_child_visible(false)`, unrealizes nothing; the destroy
        // that follows is the close.
        (
            "hidden",
            lines_after(|w, _, _| {
                w.set_visible(false);
                false
            }),
            (none.clone(), vec![closing.clone()]),
        ),
        (
            "child-visible",
            lines_after(|_, _, host| {
                host.set_child_visible(false);
                false
            }),
            (none, vec![closing]),
        ),
    ];
    let mut failed = Vec::new();
    for (name, got, want) in cases {
        if got == want {
            println!("grid_unrealize_line: {name}: ok ({got:?})");
        } else {
            println!("grid_unrealize_line: {name}: FAILED: wanted {want:?}, got {got:?}");
            failed.push(name);
        }
    }
    if !failed.is_empty() {
        eprintln!("grid_unrealize_line: {} failed: {}", failed.len(), failed.join("; "));
        server.exit(1);
    }
    println!("grid_unrealize_line: 5 passed");
}
