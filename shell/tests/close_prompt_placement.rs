//! The y/n prompt sits inside the top bar, in real GTK -- where it did not (the integration GUI pass,
//! 2026-09-26).
//!
//! **What this holds.** `ClosePrompt::ask` centres its label on the bar's height from the label's
//! own measured height (`prompt_origin`). The label is hidden until that call, and GTK's
//! `gtk_widget_measure` answers 0 for a widget that is not visible -- so the prompt was placed as if
//! it had no height: its top edge at the bar's middle, its lower half hanging over the pane below
//! (seen in Windowed and Fullscreen; Immersive places it at a fixed corner and was fine). This drives
//! the product's own `ClosePrompt` (compiled in from `src/` below) in a window shaped like the real
//! one -- a top bar of the real height with the prefix strip in it, over an overlay -- and reads
//! where the label was really allocated.
//!
//! Needs a display, and never the real desktop: **it starts its own** (2026-09-29,
//! `support/own_x_server.rs`) -- an `Xvfb` with a 1024x768 screen, pointed to before GTK
//! initialises, with any inherited `WAYLAND_DISPLAY`/`DISPLAY` dropped --
//!
//!     cargo test -p shell --test close_prompt_placement -- --ignored
//!
//! A plain `main` (`harness = false`), because GTK must own the main thread. It used to refuse only a
//! missing `GDK_BACKEND=x11`, which XWayland's `DISPLAY=:0` on a desktop session satisfies. About 1s.

use std::time::{Duration, Instant};

use gtk4::prelude::*;

// The display this test runs on: its own Xvfb, never an inherited one (2026-09-29).
#[path = "support/own_x_server.rs"]
mod own_x_server;

// `close_prompt.rs` reads `crate::toast::TOAST_MARGIN`: the real module, so Immersive's corner is
// the product's own.
#[allow(dead_code, unused_imports)]
#[path = "../src/toast.rs"]
mod toast;

// `unused_imports`: the module's own `#[cfg(test)] mod tests` comes along, its `#[test]`s dropped for
// want of a harness, and its `use super::*` with them.
#[allow(dead_code, unused_imports)]
#[path = "../src/close_prompt.rs"]
mod close_prompt;

/// The top bar's height in the product (`shell/src/main.rs`'s bar is 38px in the GUI passes).
const BAR_HEIGHT: i32 = 38;

/// The `.close-prompt` rule as `theme/gtk_css.rs` writes it, so the label measures what it will.
const CSS: &str = ".close-prompt { border: 1px solid #888888; border-radius: 4px; padding: 2px 8px; font-size: 12px; }";

fn pump(ms: u64) {
    let context = gtk4::glib::MainContext::default();
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        while context.iteration(false) {}
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// `(top, bottom)` of the prompt's label in the overlay's coordinates, after `ask`.
fn prompt_box() -> Result<(f32, f32), String> {
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(CSS);
    let display = gtk4::gdk::Display::default().ok_or("no display")?;
    gtk4::style_context_add_provider_for_display(&display, &provider, gtk4::STYLE_PROVIDER_PRIORITY_USER);

    let app = gtk4::Application::builder()
        .application_id("cn.huntergrey.neovibe.test.closeprompt")
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gtk4::gio::Cancellable>)
        .map_err(|e| e.to_string())?;
    let window = gtk4::ApplicationWindow::new(&app);
    window.set_default_size(800, 400);

    let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    bar.set_size_request(-1, BAR_HEIGHT);
    let name = gtk4::Label::new(Some("neovibe"));
    let strip = gtk4::Label::new(Some("proj"));
    bar.append(&name);
    bar.append(&strip);
    let column = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    column.append(&bar);
    let body = gtk4::Label::new(Some("pane"));
    body.set_vexpand(true);
    column.append(&body);
    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(&column));
    window.set_child(Some(&overlay));
    window.present();
    pump(300);

    let prompt = close_prompt::ClosePrompt::install(&overlay, &window, bar.upcast_ref(), strip.upcast_ref());
    prompt.ask("kill-pane editor? nvim cannot be reopened in this window (y/n)", || {});
    pump(300);

    let label = overlay
        .observe_children()
        .into_iter()
        .filter_map(|c| c.ok()?.downcast::<gtk4::Label>().ok())
        .find(|l| l.has_css_class("close-prompt"))
        .ok_or("no .close-prompt label in the overlay")?;
    if !label.is_mapped() {
        return Err("the prompt's label was never mapped".into());
    }
    let bounds = label
        .compute_bounds(&overlay)
        .ok_or("the label has no bounds in the overlay")?;
    let bar_bounds = bar.compute_bounds(&overlay).ok_or("the bar has no bounds")?;
    if (bar_bounds.height() - BAR_HEIGHT as f32).abs() > 0.5 {
        return Err(format!("the bar is {}px, not {BAR_HEIGHT}px", bar_bounds.height()));
    }
    let read = (bounds.y(), bounds.y() + bounds.height());
    window.destroy();
    gtk4::style_context_remove_provider_for_display(&display, &provider);
    pump(50);
    Ok(read)
}

fn main() {
    // Mirrors `#[ignore]` under a plain `main`: `cargo test --workspace` needs no display.
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("close_prompt_placement: ignored (drives real GTK on a display); run with `-- --ignored`");
        return;
    }
    // Its own Xvfb, GTK initialised on it and checked to be on it -- never an inherited display.
    let server = own_x_server::init_gtk("close_prompt_placement", "1024x768x24");
    match prompt_box() {
        Ok((top, bottom)) if top >= 0.0 && bottom <= BAR_HEIGHT as f32 && bottom - top > 4.0 => {
            println!("close_prompt_placement: ok (label {top}..{bottom} inside a {BAR_HEIGHT}px bar)");
        }
        Ok((top, bottom)) => {
            eprintln!(
                "close_prompt_placement: FAILED: the prompt spans {top}..{bottom}, not inside the {BAR_HEIGHT}px bar"
            );
            server.exit(1);
        }
        Err(e) => {
            eprintln!("close_prompt_placement: FAILED: {e}");
            server.exit(1);
        }
    }
}
