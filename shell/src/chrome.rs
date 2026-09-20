//! Window chrome: top bar and window controls (minimize/maximize/close).
//! Styling lives in `theme::gtk_css`.
//!
//! There is no bottom status bar. The pane cursors indicate keyboard focus.

use std::path::Path;

use gtk4::{prelude::*, ApplicationWindow};

/// The top bar's project label shows the directory's own last path component, never the whole
/// resolved path -- which would run long and, for a project under the user's home directory,
/// would publish it in every screenshot/screen-share.
///
/// `Path::file_name` already does the real work (it strips a trailing slash and any bare `.`/`..`
/// components on its own), so this exists mainly to give that behaviour a name and a fallback:
/// the root `/` has no file name at all, and a host that ever hands this a non-canonicalized or
/// otherwise degenerate path (this crate's own caller always canonicalizes first, but this
/// function does not assume that) gets the path back verbatim rather than an empty label.
pub(crate) fn project_display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// The top bar and the parts of it other modules act on.
pub(crate) struct TopBar {
    /// The whole bar (the `WindowHandle` around it). Hidden in immersive mode (`window_mode`).
    pub(crate) widget: gtk4::Widget,
    /// Its keyboard-navigable items, left to right; see `build_top_bar`'s doc.
    pub(crate) items: Vec<gtk4::Widget>,
    /// Minimize/maximize/close. Hidden whenever the window is fullscreen (`window_mode`).
    pub(crate) controls: gtk4::Widget,
    /// The `neovibe` label, drawn as a solid block while the `Ctrl+a` prefix waits (`prefix`).
    #[allow(dead_code)] // Intended for the `prefix` module; not consumed yet.
    pub(crate) app_name: gtk4::Label,
}

/// Builds the top bar and returns the parts other modules use (`TopBar`).
/// `Ctrl+k` reaches the bar from either pane because it is above both. `h` and `l` move
/// between its items, currently only `↻`. Window controls cannot take focus, so an accidental
/// `Enter` after `Ctrl+k` cannot close the window.
pub(crate) fn build_top_bar(window: &ApplicationWindow, project_root: &Path) -> TopBar {
    let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    bar.add_css_class("topbar");
    bar.set_valign(gtk4::Align::Fill);

    let app_name = gtk4::Label::new(Some("neovibe"));
    app_name.add_css_class("topbar-app-name");

    let project_name = gtk4::Label::new(Some(&project_display_name(project_root)));
    project_name.add_css_class("topbar-project-name");
    project_name.set_hexpand(true);
    project_name.set_halign(gtk4::Align::Start);

    // Reload the agent panel through the same `app.reload-agent-panel` action as Ctrl+Shift+R.
    // The button lives in the window chrome so it remains available when the panel's page hangs.
    // The shortcut enables recovery; the button makes it discoverable.
    //
    // Its appearance has not been verified. It shares `win-btn` with the window controls and sits
    // immediately before them, so it may look like part of that cluster instead of a panel control.
    let reload_agent = gtk4::Button::with_label("\u{21BB}");
    reload_agent.add_css_class("win-btn");
    reload_agent.set_valign(gtk4::Align::Center);
    reload_agent.set_tooltip_text(Some(
        "Reload the agent panel (Ctrl+Shift+R) — the session keeps running",
    ));
    reload_agent.set_action_name(Some("app.reload-agent-panel"));
    reload_agent.add_css_class("topbar-item");

    let controls = build_window_controls(window);

    bar.append(&app_name);
    bar.append(&project_name);
    bar.append(&reload_agent);
    bar.append(&controls);

    let handle = gtk4::WindowHandle::new();
    handle.set_child(Some(&bar));
    TopBar {
        widget: handle.upcast(),
        items: vec![reload_agent.upcast()],
        controls,
        app_name,
    }
}

/// Carried over from `shell_chrome::build_window_controls` unchanged.
pub(crate) fn build_window_controls(window: &ApplicationWindow) -> gtk4::Widget {
    let controls = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    controls.set_valign(gtk4::Align::Center);
    controls.set_margin_end(6);

    let minimize = gtk4::Button::with_label("—");
    minimize.add_css_class("win-btn");
    {
        let window = window.clone();
        minimize.connect_clicked(move |_| window.minimize());
    }

    let maximize = gtk4::Button::with_label("\u{25A1}");
    maximize.add_css_class("win-btn");
    {
        let window = window.clone();
        maximize.connect_clicked(move |_| {
            if window.is_maximized() {
                window.unmaximize();
            } else {
                window.maximize();
            }
        });
    }

    let close = gtk4::Button::with_label("\u{00D7}");
    close.add_css_class("win-btn");
    close.add_css_class("close");
    {
        let window = window.clone();
        close.connect_clicked(move |_| window.close());
    }

    // Mouse only. A focusable close button is one `Enter` away from closing the window from the
    // keyboard, and since `Ctrl+k` now brings keyboard focus into the top bar that is no longer a
    // theoretical path. `focusable(false)` keeps them clickable.
    for button in [&minimize, &maximize, &close] {
        button.set_focusable(false);
    }
    controls.append(&minimize);
    controls.append(&maximize);
    controls.append(&close);
    controls.upcast()
}

#[cfg(test)]
mod tests {
    use super::project_display_name;
    use std::path::Path;

    #[test]
    fn a_normal_path_shows_its_last_component() {
        assert_eq!(
            project_display_name(Path::new("/home/user/src/neovibe")),
            "neovibe"
        );
    }

    #[test]
    fn a_trailing_slash_does_not_change_the_answer() {
        assert_eq!(
            project_display_name(Path::new("/home/user/src/neovibe/")),
            "neovibe"
        );
    }

    #[test]
    fn the_root_directory_has_no_last_component_so_it_falls_back_to_itself() {
        assert_eq!(project_display_name(Path::new("/")), "/");
    }
}
