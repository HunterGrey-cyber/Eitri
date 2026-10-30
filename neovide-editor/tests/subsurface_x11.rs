//! Foreign Wayland connections are unavailable on the test's own X server.
use gtk4::{gdk, prelude::*};
use neovide_editor::{NeovideEditorPane, Presentation};
use std::time::{Duration, Instant};

#[path = "../../shell/tests/support/own_x_server.rs"]
mod own_x_server;

fn x11_has_no_wayland_connection() {
    let display = gdk::Display::default().expect("GTK opened the test's X server");
    assert_eq!(display.type_().name(), "GdkX11Display");
    assert!(neovide_editor::subsurface_testing::connection_for(&display).is_none());
}

fn an_x11_pane_takes_the_gtk_path_and_says_nothing() {
    let pane = NeovideEditorPane::new(true);
    pane.set_presentation(Presentation::Auto);
    // An ended session still paints its GTK framebuffer and needs no nvim child.
    pane.shutdown();
    let window = gtk4::Window::builder().default_width(640).default_height(480).build();
    window.set_child(Some(pane.widget()));
    window.present();
    let context = gtk4::glib::MainContext::default();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        context.iteration(false);
        let counts = pane.presentation_counts();
        if counts.own_frames + counts.fallback_frames > 0 {
            break;
        }
        assert!(Instant::now() < deadline, "the X11 pane did not draw a GTK frame");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(pane.widget().is_realized());
    assert!(pane.presentation_hole().is_none());
    window.close();
}

fn main() {
    const CHILD: &str = "EITRI_SUBSURFACE_X11_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _server = own_x_server::init_gtk("subsurface_x11", "1024x768x24");
        x11_has_no_wayland_connection();
        an_x11_pane_takes_the_gtk_path_and_says_nothing();
        println!("x11_has_no_wayland_connection: pass");
        println!("an_x11_pane_takes_the_gtk_path_and_says_nothing: pass");
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .env(CHILD, "1")
        .env_remove("EITRI_EDITOR_PRESENTATION")
        .output()
        .expect("start the display-isolated case");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{}\n{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!stderr.contains("[editor] subsurface"), "{stderr}");
    print!("{}", String::from_utf8_lossy(&output.stdout));
}
