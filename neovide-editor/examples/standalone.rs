//! Standalone example: hosts a `neovide_editor::NeovideEditorPane` inside its own
//! `Application`/`ApplicationWindow`.
//!
//! `NeovideEditorPane` deliberately never constructs its own top-level window (see this crate's
//! `src/lib.rs` module doc for why) -- an embeddable editor surface can't assume it owns the
//! window it lives in. This example is the minimal possible host: it builds the pane, places
//! `.widget()` into an `ApplicationWindow`'s child, and wires the two lifecycle handoffs the pane
//! leaves to whatever embeds it:
//!
//! - `window.connect_close_request` -> `pane.shutdown()`, for the *host-initiated* direction (the
//!   user closes the window) -- mirrors `poc/neovide_embed_live/src/main.rs`'s own
//!   `connect_close_request` handler exactly: call the shutdown path, then unconditionally return
//!   `glib::Propagation::Proceed` regardless of whether a clean `NeovimExited` was actually
//!   observed (the reference never conditions the close on that return value -- the app is exiting
//!   either way).
//! - `pane.on_exited_unrequested(...)` -> `window.close()`, for the opposite direction: nvim
//!   exiting *on its own* (e.g. `:qa!` typed inside it) without this host ever asking. Without
//!   registering this callback, the pane has no way to tell its host "I'm done" -- the window
//!   would sit on screen forever showing a frozen last frame while still swallowing input. This is
//!   the same "dead-looking-alive window" bug `neovide_embed_live` fixed for itself by calling
//!   `window.close()` directly from its own tick callback; since this pane doesn't own a `Window`
//!   to close, it surfaces the moment as a callback instead and leaves the actual close to us.
//!
//! Build with: `cargo build --manifest-path Cargo.toml -p neovide-editor --example standalone`
//! (the repo-root workspace manifest -- there is no `poc/Cargo.toml` any more, see `poc/README.md`),
//! then run the produced `standalone` binary from a real Wayland session with `nvim` on `$PATH`.

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow};

use neovide_editor::NeovideEditorPane;

const APP_ID: &str = "cn.huntergrey.neovibe.neovide_editor_standalone";

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
