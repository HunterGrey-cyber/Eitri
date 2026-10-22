//! One plain GTK window per process, for asking a compositor two things a second Eitri window depends on: whether one
//! process can hand the keys to another process's window, and which monitor (in logical coordinates) each window is
//! on. GTK only; no Eitri code. Run it inside a headless sandbox compositor, never on a desktop:
//!
//! ```text
//! mw_activation_probe first     # a window titled mw-probe-1
//! mw_activation_probe second    # a window titled mw-probe-2
//! ```
//!
//! What it prints on stdout, one line each:
//! - `ready <title> <pid>` once the window is presented;
//! - `monitor <connector> <x> <y> <w> <h>` each time the window's surface enters a monitor (the monitor's
//!   logical rectangle), and `left <connector>` when it leaves one;
//! - `focus <title>` when the window becomes the active one, `unfocus <title>` when it stops being it;
//! - the answer to each stdin command below.
//!
//! What it reads on stdin, one command per line:
//! - `token`: asks for an xdg-activation token for the window that has the keyboard and prints `token <t>`
//!   (`token none` when the compositor gives none);
//! - `raise <t>`: sets `<t>` as the window's startup id and presents it; `raise` alone presents it with no token;
//!   prints `presented` either way;
//! - `move <connector>`: calls `MoveOwnToMonitor(s)` on the GNOME extension's bus name and prints `moved true`,
//!   `moved false` or `moved error <message>`;
//! - `partner <pid> [<pid> ...]`: calls `SetPartner(au)` and prints `partner <bool>` (or `partner error ...`);
//! - `activate-partner`: calls `ActivatePartner()` and prints `activated <bool>` (or `activated error ...`);
//! - `quit`, or the end of stdin: closes the window and exits.
//!
//! The extension answers by the caller's pid, which it reads from the bus connection, so the three bus calls go out
//! on this process's own session-bus connection: a helper process making the call would own no window and be told
//! `false`. They run on the stdin thread so that a slow or missing bus name never stalls the window's main loop.

use std::io::BufRead;
use std::sync::{mpsc, OnceLock};
use std::time::Duration;

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gio, glib};

const BUS_NAME: &str = "cn.huntergrey.Eitri.Shell1";
const OBJECT_PATH: &str = "/cn/huntergrey/Eitri/Shell1";
/// Long enough for a busy compositor, short enough that a missing name answers before the harness gives up.
const BUS_TIMEOUT_MS: i32 = 5_000;

/// What the stdin thread hands to the window's main loop: the commands that need the window itself.
enum WindowCmd {
    Token,
    Raise(Option<String>),
    Quit,
}

fn say(line: &str) {
    // Rust's stdout is line-buffered, so each line reaches a reading pipe as soon as it is printed.
    println!("{line}");
}

fn connector(monitor: &gtk::gdk::Monitor) -> String {
    monitor
        .connector()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".to_string())
}

fn one_line(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The process's one session-bus connection, opened on first use and held until exit. The extension keeps the list
/// `SetPartner` gives it per bus connection and forgets it when that connection closes, and GIO closes its shared
/// session connection once the last reference to it is dropped: a connection opened per call would lose the partner
/// before `ActivatePartner` asks for it.
fn session_bus() -> Result<gio::DBusConnection, String> {
    static BUS: OnceLock<gio::DBusConnection> = OnceLock::new();
    if let Some(bus) = BUS.get() {
        return Ok(bus.clone());
    }
    let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).map_err(|e| e.to_string())?;
    Ok(BUS.get_or_init(|| bus).clone())
}

fn call_extension(method: &str, params: Option<glib::Variant>) -> Result<bool, String> {
    let bus = session_bus()?;
    let reply = bus
        .call_sync(
            Some(BUS_NAME),
            OBJECT_PATH,
            BUS_NAME,
            method,
            params.as_ref(),
            Some(glib::VariantTy::new("(b)").expect("a valid reply type")),
            gio::DBusCallFlags::NONE,
            BUS_TIMEOUT_MS,
            gio::Cancellable::NONE,
        )
        .map_err(|e| e.to_string())?;
    reply
        .get::<(bool,)>()
        .map(|(b,)| b)
        .ok_or_else(|| format!("unexpected reply {reply}"))
}

fn report(prefix: &str, result: Result<bool, String>) {
    match result {
        Ok(b) => say(&format!("{prefix} {b}")),
        Err(e) => say(&format!("{prefix} error {}", one_line(&e))),
    }
}

/// Reads stdin until it ends. Bus commands are answered here; window commands go to the main loop.
fn read_commands(to_window: mpsc::Sender<WindowCmd>) {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let mut words = line.split_whitespace();
        match words.next() {
            Some("token") => {
                let _ = to_window.send(WindowCmd::Token);
            }
            Some("raise") => {
                let _ = to_window.send(WindowCmd::Raise(words.next().map(str::to_string)));
            }
            Some("move") => match words.next() {
                Some(connector) => {
                    let params = (connector,).to_variant();
                    report("moved", call_extension("MoveOwnToMonitor", Some(params)))
                }
                None => say("moved error no connector given"),
            },
            Some("partner") => {
                let pids: Result<Vec<u32>, _> = words.map(str::parse::<u32>).collect();
                match pids {
                    Ok(pids) if !pids.is_empty() => {
                        report("partner", call_extension("SetPartner", Some((pids,).to_variant())))
                    }
                    _ => say("partner error expected one or more pids"),
                }
            }
            Some("activate-partner") => report("activated", call_extension("ActivatePartner", None)),
            Some("quit") => break,
            Some(other) => say(&format!("unknown {other}")),
            None => {}
        }
    }
    let _ = to_window.send(WindowCmd::Quit);
}

fn main() {
    let title = match std::env::args().nth(1).as_deref() {
        Some("first") => "mw-probe-1",
        Some("second") => "mw-probe-2",
        _ => {
            eprintln!("usage: mw_activation_probe first|second");
            std::process::exit(2);
        }
    };
    gtk::init().expect("GTK could not open a display");
    let main_loop = glib::MainLoop::new(None, false);

    let window = gtk::Window::builder()
        .title(title)
        .default_width(640)
        .default_height(400)
        .child(&gtk::Label::new(Some(title)))
        .build();
    window.connect_is_active_notify(move |w| {
        say(&format!("{} {title}", if w.is_active() { "focus" } else { "unfocus" }));
    });
    // The surface exists from realize on; its monitor signals carry what GDK knows of each output.
    window.connect_realize(|w| {
        let Some(surface) = w.surface() else { return };
        surface.connect_enter_monitor(|_, monitor| {
            let g = monitor.geometry();
            let name = connector(monitor);
            let (x, y, w, h) = (g.x(), g.y(), g.width(), g.height());
            say(&format!("monitor {name} {x} {y} {w} {h}"));
        });
        surface.connect_leave_monitor(|_, monitor| {
            say(&format!("left {}", connector(monitor)));
        });
    });
    {
        let main_loop = main_loop.clone();
        window.connect_close_request(move |_| {
            main_loop.quit();
            glib::Propagation::Proceed
        });
    }
    window.present();
    say(&format!("ready {title} {}", std::process::id()));

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || read_commands(tx));
    {
        let window = window.clone();
        let main_loop = main_loop.clone();
        glib::timeout_add_local(Duration::from_millis(20), move || {
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    WindowCmd::Token => {
                        // GDK asks the compositor for a token naming the surface with the keyboard and the
                        // seat's last input serial, and waits for it.
                        let context = WidgetExt::display(&window).app_launch_context();
                        match context.startup_notify_id(None::<&gio::AppInfo>, &[]) {
                            Some(t) if !t.is_empty() => say(&format!("token {t}")),
                            _ => say("token none"),
                        }
                    }
                    WindowCmd::Raise(token) => {
                        if let Some(t) = token {
                            window.set_startup_id(&t);
                        }
                        window.present();
                        say("presented");
                    }
                    WindowCmd::Quit => {
                        window.close();
                        main_loop.quit();
                        return glib::ControlFlow::Break;
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    }
    main_loop.run();
}
