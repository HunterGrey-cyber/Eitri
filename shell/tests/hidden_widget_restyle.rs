//! A colorscheme change reaches a widget that first shows after it -- in real GTK, which is where it
//! did not (modules P2's GUI pass, defect D1, 2026-09-24).
//!
//! **What this holds.** In a `--clean` launch a tray chip first shown after startup was drawn in the
//! fallback theme's colours. The mechanism, read out of GTK 4.22.5 and written down in
//! `shell/src/theme/restyle.rs`: a widget never realized has no style context, so a `CssProvider`
//! reload reaches it only if its parent's computed style changed -- and GTK had already computed its
//! style, from the fallback, while it was hidden. `ThemeCss::update` now runs
//! `theme::restyle::restyle_unrealized` over the window after each reload. This file drives the
//! product's own `Tray` and that function (both compiled in from `src/` below) against the GTK this
//! crate links, in the two shapes the window has:
//!
//! - **tray**: the real `Tray` with its chips in their real order, only the terminal's showing, then
//!   the editor's shown after a reload that leaves the top bar's `color` alone -- as the fallback and
//!   `--clean` both give it nvim's default `#e0e2ea`;
//! - **child-visible**: a `gtk4::Separator` hidden with `set_child_visible(false)`, which is how
//!   `module_grid` hides a module and its divider.
//!
//! Each runs twice. **Without** `restyle_unrealized` the widget must come out in the OLD colour: that
//! is the positive control, and a run where it does not fails as "did not exercise the trigger" --
//! if a GTK update ever restyles these widgets on its own, this says so and the workaround can go.
//! **With** it, the NEW colour. Every run also checks that the widget beside it took the new colour
//! (the reload really happened) and that the widget read was really mapped when read.
//!
//! Needs a display, and never the real desktop: **it starts its own** (2026-09-29,
//! `support/own_x_server.rs`) -- an `Xvfb` with a 1024x768 screen, pointed to before GTK
//! initialises, with any inherited `WAYLAND_DISPLAY`/`DISPLAY` dropped --
//!
//!     cargo test -p shell --test hidden_widget_restyle -- --ignored
//!
//! A plain `main` (`harness = false`), because GTK must own the main thread. It used to refuse only a
//! missing `GDK_BACKEND=x11`, which XWayland's `DISPLAY=:0` on a desktop session satisfies. About 8s.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use neovibe_core::attention::Attention;
use neovibe_core::layout::{Axis, Layout, ModuleId, Node};

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
