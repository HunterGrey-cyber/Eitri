//! Hosts a `neovide_editor::NeovideEditorPane` in an `ApplicationWindow`.
//! The embeddable pane leaves window creation and lifecycle handling to its host.
//! Place `.widget()` in the window and connect both shutdown directions:
//!
//! - `window.connect_close_request` calls `pane.shutdown()`, then returns
//!   `glib::Propagation::Proceed` even if no clean `NeovimExited` was observed.
//! - `pane.on_exited_unrequested(...)` calls `window.close()` when nvim exits on its
//!   own, such as after `:qa!`. Without it the window keeps showing a frozen frame
//!   and swallowing input after the editor has exited.
//!
//! Build with `cargo build --manifest-path Cargo.toml -p neovide-editor --example standalone`
//! using the repository root manifest. Run `standalone` in a Wayland session with
//! `nvim` on `$PATH`.

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow};

use neovide_editor::NeovideEditorPane;

const APP_ID: &str = "cn.huntergrey.eitri.neovide_editor_standalone";

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();

    app.connect_activate(|app| {
        // `false`: not `--clean` -- this example is meant to open with whatever real nvim config
        // is on this machine, same default `LiveHarnessOptions::extra_nvim_args` behavior as the
        // reference probe when no `--clean` flag is passed on its own command line.
        let pane = NeovideEditorPane::new(false);

        let window = ApplicationWindow::builder()
            .application(app)
            .title("neovide-editor standalone example")
            .default_width(1000)
            .default_height(700)
            .child(pane.widget())
            .build();

        // nvim exiting on its own (e.g. `:qa!`) has no window to close by itself -- ask this
        // host's window to close, which in turn drives the connect_close_request handler below.
        {
            let window = window.clone();
            pane.on_exited_unrequested(move || {
                window.close();
            });
        }

        window.present();
        // grab_focus() after present(), matching the reference's own
        // `window.present(); gl_area.grab_focus(); im_context.focus_in();` ordering -- focusing a
        // not-yet-shown widget is meaningless.
        pane.grab_focus();

        // Registered last: this is `pane`'s final use in this closure, so it can be moved in
        // outright rather than needing `Rc`-wrapping or an extra clone -- `NeovideEditorPane`
        // doesn't derive `Clone`, but every earlier use above only ever borrowed it via `&self`
        // methods (`.widget()`, `.on_exited_unrequested()`, `.grab_focus()`).
        window.connect_close_request(move |_window| {
            pane.shutdown();
            glib::Propagation::Proceed
        });
    });

    app.run()
}
