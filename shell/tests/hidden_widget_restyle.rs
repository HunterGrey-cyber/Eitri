//! Checks that a colorscheme reload reaches widgets first shown after the reload.
//!
//! A never-realized widget has no style context. In GTK 4.22.5, a stylesheet reload
//! reaches it only if its parent's computed style changes. GTK may already have
//! computed the hidden widget's style from the fallback theme, leaving it stale.
//! `ThemeCss::update` calls `theme::restyle::restyle_unrealized` after every reload.
//! This test uses the real `Tray` and that function, compiled from `src/`, for two shapes:
//!
//! - A tray chip shown after a reload, with only the terminal chip visible initially.
//!   The reload leaves the top bar's `color` unchanged, as both the fallback and
//!   `--clean` use nvim's default `#e0e2ea`.
//! - A `gtk4::Separator` hidden with `set_child_visible(false)`, as `module_grid`
//!   hides a module and its divider.
//!
//! Each case runs without the workaround and must retain the old colour as a positive
//! control. With the workaround it must take the new colour. The test also checks that
//! the visible sibling changed colour and that the sampled widget was mapped.
//!
//! The test starts its own Xvfb with a 1024x768 screen through `support/own_x_server.rs`.
//! It drops inherited `WAYLAND_DISPLAY` and `DISPLAY` values before GTK initialises.
//!
//!     cargo test -p shell --test hidden_widget_restyle -- --ignored
//!
//! Uses a plain `main` (`harness = false`) because GTK must own the main thread.
//! Checking only `GDK_BACKEND=x11` would also accept a desktop's XWayland display.
//! Takes about 8s.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use eitri_core::attention::Attention;
use eitri_core::layout::{Axis, Layout, ModuleId, Node};
use gtk4::prelude::*;

// The display this test runs on: its own Xvfb, never an inherited one (2026-09-29).
#[path = "support/own_x_server.rs"]
mod own_x_server;

#[allow(dead_code)]
#[path = "../src/theme/restyle.rs"]
mod restyle;

// `unused_imports`: `tray.rs`'s own `#[cfg(test)] mod tests` comes along, its `#[test]`s dropped for
// want of a harness, and its `use super::*` with them.
#[allow(dead_code, unused_imports)]
#[path = "../src/tray.rs"]
mod tray;

/// The top bar's text colour, the same before and after the reload: nvim's default `NvimLightGrey2`,
/// which both the fallback theme and `--clean` give `chrome_fg`.
const BAR: &str = "#e0e2ea";
/// A chip's colour before and after the reload. Any two colours do; these only need to differ.
const OLD: &str = "#686a70";
const NEW: &str = "#c4c6cd";

fn css(chip: &str) -> String {
    format!(
        ".topbar {{ color: {BAR}; }}\n\
         .tray-chip {{ color: {chip}; border: 1px solid {chip}; }}\n\
         .probe-divider {{ color: {chip}; }}\n"
    )
}

/// Runs the default main loop for `ms`, so frames are laid out, drawn and restyled.
fn pump(ms: u64) {
    let context = gtk4::glib::MainContext::default();
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        while context.iteration(false) {}
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The computed `color` of `widget`, as `#rrggbb`.
fn color_of(widget: &impl IsA<gtk4::Widget>) -> String {
    let c = widget.as_ref().color();
    let byte = |v: f32| (v * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(c.red()), byte(c.green()), byte(c.blue()))
}

/// A window with `child` in a `.topbar` box, painted by a provider at the stylesheet's own priority
/// (`STYLE_PROVIDER_PRIORITY_USER`, as `ThemeCss::install` adds it) loaded with the OLD colours.
fn window_with(child: &impl IsA<gtk4::Widget>) -> (gtk4::Window, gtk4::CssProvider) {
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(&css(OLD));
    let display = gtk4::gdk::Display::default().expect("a display");
    gtk4::style_context_add_provider_for_display(&display, &provider, gtk4::STYLE_PROVIDER_PRIORITY_USER);
    let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    bar.add_css_class("topbar");
    bar.append(child);
    let window = gtk4::Window::new();
    window.set_default_size(600, 80);
    window.set_child(Some(&bar));
    (window, provider)
}

fn close(window: gtk4::Window, provider: gtk4::CssProvider) {
    window.destroy();
    let display = gtk4::gdk::Display::default().expect("a display");
    gtk4::style_context_remove_provider_for_display(&display, &provider);
    pump(50);
}

/// The reload `ThemeCss::update` does, with or without its second half.
fn reload(provider: &gtk4::CssProvider, window: &gtk4::Window, with_fix: bool) {
    provider.load_from_string(&css(NEW));
    if with_fix {
        restyle::restyle_unrealized(window.upcast_ref());
    }
}

/// What a run read: the widget first shown after the reload, and the one beside it.
struct Read {
    shown: String,
    beside: String,
}

/// Three leaves, `terminal` below `editor | agent`, with `hidden` off screen.
fn layout(hidden: &[ModuleId], focus: ModuleId) -> Layout {
    let root = Node::split(
        Axis::Column,
        0.7,
        Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(ModuleId::editor()),
            Node::Leaf(ModuleId::agent()),
        ),
        Node::Leaf(ModuleId::terminal()),
    );
    Layout::from_parts(root, hidden.iter().cloned().collect::<BTreeSet<_>>(), focus).expect("a valid layout")
}

fn tray_chip(with_fix: bool) -> Result<Read, String> {
    let chips = [
        (ModuleId::editor(), "editor".to_string()),
        (ModuleId::agent(), "agent".to_string()),
        (ModuleId::terminal(), "terminal".to_string()),
    ];
    let tray = tray::Tray::build(&chips);
    let items = tray.items();
    let (editor_chip, terminal_chip) = (items[0].clone(), items[2].clone());
    let (window, provider) = window_with(tray.widget());
    // The first launch: the terminal hidden, so its chip alone shows.
    tray.refresh(
        &layout(&[ModuleId::terminal()], ModuleId::editor()),
        Attention::default(),
        &[],
    );
    window.present();
    pump(600);
    if !terminal_chip.is_mapped() || editor_chip.is_realized() {
        return Err(
            "did not exercise the trigger: the terminal's chip should be on screen and the editor's never realized"
                .into(),
        );
    }
    if color_of(&terminal_chip) != OLD {
        return Err(format!(
            "did not exercise the trigger: the terminal's chip read {} before the reload",
            color_of(&terminal_chip)
        ));
    }
    reload(&provider, &window, with_fix);
    pump(300);
    // `Ctrl+a x` in the editor: its chip shows.
    tray.refresh(
        &layout(&[ModuleId::terminal(), ModuleId::editor()], ModuleId::agent()),
        Attention::default(),
        &[],
    );
    pump(900);
    if !editor_chip.is_mapped() {
        return Err("did not exercise the trigger: the editor's chip never got on screen".into());
    }
    let read = Read {
        shown: color_of(&editor_chip),
        beside: color_of(&terminal_chip),
    };
    close(window, provider);
    Ok(read)
}

fn child_visible(with_fix: bool) -> Result<Read, String> {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    let divider = gtk4::Separator::new(gtk4::Orientation::Vertical);
    divider.add_css_class("probe-divider");
    divider.set_child_visible(false);
    let beside = gtk4::Button::with_label("beside");
    beside.add_css_class("tray-chip");
    row.append(&divider);
    row.append(&beside);
    let (window, provider) = window_with(&row);
    window.present();
    pump(600);
    if divider.is_realized() || !beside.is_mapped() || color_of(&beside) != OLD {
        return Err(
            "did not exercise the trigger: the divider should be unrealized, the button on screen in OLD".into(),
        );
    }
    reload(&provider, &window, with_fix);
    pump(300);
    divider.set_child_visible(true);
    row.queue_resize();
    pump(900);
    if !divider.is_mapped() {
        return Err("did not exercise the trigger: the divider never got on screen".into());
    }
    let read = Read {
        shown: color_of(&divider),
        beside: color_of(&beside),
    };
    close(window, provider);
    Ok(read)
}

fn main() {
    // Mirrors `#[ignore]` under a plain `main`: `cargo test --workspace` needs no display.
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("hidden_widget_restyle: ignored (drives real GTK on a display); run with `-- --ignored`");
        return;
    }
    // Its own Xvfb, GTK initialised on it and checked to be on it -- never an inherited display.
    let server = own_x_server::init_gtk("hidden_widget_restyle", "1024x768x24");

    type Scenario = fn(bool) -> Result<Read, String>;
    let scenarios: [(&str, Scenario); 2] = [("tray", tray_chip), ("child-visible", child_visible)];
    let mut failed = Vec::new();
    for (name, run) in scenarios {
        for with_fix in [false, true] {
            let label = format!("{name}, {}", if with_fix { "restyled" } else { "control" });
            let verdict = match run(with_fix) {
                Err(e) => Err(e),
                Ok(read) if read.beside != NEW => Err(format!(
                    "did not exercise the trigger: the widget beside it read {} after the reload",
                    read.beside
                )),
                Ok(read) if !with_fix && read.shown != OLD => Err(format!(
                    "did not exercise the trigger: without restyle_unrealized the widget read {} (not the \
                     stale {OLD}) -- GTK may now restyle it on its own; see src/theme/restyle.rs",
                    read.shown
                )),
                Ok(read) if with_fix && read.shown != NEW => Err(format!(
                    "the widget kept {} after restyle_unrealized; expected {NEW}",
                    read.shown
                )),
                Ok(read) => Ok(read.shown),
            };
            match verdict {
                Ok(shown) => println!("hidden_widget_restyle: {label}: ok ({shown})"),
                Err(e) => {
                    println!("hidden_widget_restyle: {label}: FAILED: {e}");
                    failed.push(label);
                }
            }
        }
    }
    if !failed.is_empty() {
        eprintln!("hidden_widget_restyle: {} failed: {}", failed.len(), failed.join("; "));
        server.exit(1);
    }
    println!("hidden_widget_restyle: 4 passed");
}
