//! shell: the real neovibe product window -- custom chrome, a two-pane layout, the
//! neovide-editor crate embedded as the real editor pane, and a placeholder panel standing
//! in for the future agent-ui module. See docs/superpowers/specs/2026-09-06-shell-scaffolding-design.md.

mod agent_placeholder;
mod chrome;
mod layout;
mod theme;

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow};

use neovide_editor::NeovideEditorPane;

const APP_ID: &str = "cn.huntergrey.neovibe";

fn main() -> glib::ExitCode {
    // Same `--clean` passthrough convenience as `neovide_embed_live`/`shell_composed`: pass
    // `--clean` on this binary's own command line to launch nvim with `--clean` instead of a
    // real embedding host's actual config.
    let want_clean = std::env::args().any(|arg| arg == "--clean");

    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| build_ui(app, want_clean));
    app.run_with_args::<&str>(&[])
}

fn build_ui(app: &Application, want_clean: bool) {
    let theme = theme::Theme::dark();
    chrome::apply_css(&theme.to_css());

    let pane = NeovideEditorPane::new(want_clean);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("neovibe")
        .default_width(1280)
        .default_height(760)
        // No HeaderBar: decorated(false) suppresses GTK's own CSD titlebar entirely -- the
        // custom top bar built below (chrome::build_top_bar) is the only titlebar.
        .decorated(false)
        .build();
    window.add_css_class("shell-root");

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.add_css_class("shell-root");

    let agent_widget = agent_placeholder::build_placeholder_panel();
    let editor_widget: gtk4::Widget = pane.widget().clone().upcast();
    let (content_widget, _paned) = layout::build_content_area(&editor_widget, &agent_widget);

    root.append(&chrome::build_top_bar(&window));
    root.append(&content_widget);
    root.append(&chrome::build_status_bar());

    window.set_child(Some(&root));

    // nvim exiting on its own (e.g. `:qa!`) has no window to close by itself -- ask this
    // host's window to close, which in turn drives the connect_close_request handler below.
    // Mirrors neovide-editor's own `examples/standalone.rs` exactly.
    {
        let window = window.clone();
        pane.on_exited_unrequested(move || {
            window.close();
        });
    }

    window.present();
    // grab_focus() after present(), matching standalone.rs's own
    // `window.present(); pane.grab_focus();` ordering -- focusing a not-yet-shown widget is
    // meaningless.
    pane.grab_focus();

    // Registered last: this is `pane`'s final use in this function, so it can be moved in
    // outright rather than needing `Rc`-wrapping or an extra clone -- `NeovideEditorPane`
    // doesn't derive `Clone`, but every earlier use above only ever borrowed it via `&self`
    // methods (`.widget()`, `.on_exited_unrequested()`, `.grab_focus()`). Same
    // connect_close_request -> shutdown() -> unconditional glib::Propagation::Proceed pattern
    // as standalone.rs: the app is exiting either way, regardless of whether a clean
    // `NeovimExited` was actually observed.
    window.connect_close_request(move |_window| {
        pane.shutdown();
        glib::Propagation::Proceed
    });

    println!(
        "shell running: chrome+editor+placeholder-agent-panel. scale_factor={} clean={}",
        window.scale_factor(),
        want_clean,
    );
}
