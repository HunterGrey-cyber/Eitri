//! Window chrome: top bar and window controls (minimize/maximize/close). Styling itself lives in
//! `theme::gtk_css`.
//!
//! **There is no bottom status bar** (deleted 2026-09-19, the owner's choice; UI spec §2 had already
//! called for it). Its last job was naming the focused pane, which the cursors do on their own
//! (`pane_focus`).

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
    pub(crate) app_name: gtk4::Label,
    /// Where the prefix strip goes, right of the app name (`prefix_strip`, modules P2).
    pub(crate) strip: gtk4::Box,
    /// Where the tray goes, left of `↻` (`tray`, modules P2). Its chips are top-bar items too, before
    /// `items` -- `main.rs` joins the two lists.
    pub(crate) tray: gtk4::Box,
}

/// `h`/`l` on the top bar: the visible item `step` away from `current`, stopping at either end, or
/// `None` when no item is visible. A tray chip whose module is on screen is not visible, and is
/// stepped over. From no current item, the first visible one.
pub(crate) fn step_item(visible: &[bool], current: Option<usize>, step: isize) -> Option<usize> {
    let shown: Vec<usize> = (0..visible.len()).filter(|&i| visible[i]).collect();
    let Some(current) = current.and_then(|c| shown.iter().position(|&i| i == c)) else {
        return shown.first().copied();
    };
    let next = (current as isize + step).clamp(0, shown.len() as isize - 1) as usize;
    Some(shown[next])
}

/// Builds the top bar. Returns the bar and the parts of it other modules act on (`TopBar`).
///
/// `Ctrl+k` reaches the top bar from either pane (2026-09-19): it is spatially above both, the
/// same way `Ctrl+l` reaches the panel. The items are what `h`/`l` move between once there: `↻`
/// here, and since modules P2 the tray's chips ahead of it (`main.rs` joins the two lists); the
/// project switcher the UI spec puts at the left (§2.1) joins them when it lands. **The window controls are deliberately not items and cannot take focus at all** (see
/// `build_window_controls`): a stray `Enter` after `Ctrl+k` must never be able to close the window.
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
    // The name gives way when the bar is short of room -- in a narrow window while the prefix strip
    // lists every key beside it (modules P2). The strip's own labels ellipsize too
    // (`PrefixStrip::show`); an ellipsized label's minimum is its ellipsis, so neither raises the
    // bar's minimum width, and so the window's. The tray's chips do not shrink: a zoom that puts
    // many of them on a narrow bar can still widen it (not seen on a screen either way).
    project_name.set_ellipsize(gtk4::pango::EllipsizeMode::End);

    // Reload the agent panel's frontend -- the same action `prefix r` fires (see
    // `agent_panel::install_reload_action`). It lives in the window chrome rather than in
    // the panel's own page on purpose: the page is the thing that wedges, and a control the page
    // draws would be gone exactly when it is wanted. The accelerator is what makes the recovery
    // possible at all; this button is what makes it discoverable.
    //
    // Its appearance is NOT verified. It borrows `win-btn`, the class the minimize/maximize/close
    // buttons use, and is appended immediately before `build_window_controls`, so on screen it may
    // well read as part of the window-control cluster rather than as a panel control. Left as-is
    // rather than restyled blind -- see `shell/MANUAL_VERIFICATION.md`'s 2026-09-15 section.
    let reload_agent = gtk4::Button::with_label("\u{21BB}");
    reload_agent.add_css_class("win-btn");
    reload_agent.set_valign(gtk4::Align::Center);
    reload_agent.set_tooltip_text(Some("Reload the agent panel — the session keeps running"));
    reload_agent.set_action_name(Some("app.reload-agent-panel"));
    reload_agent.add_css_class("topbar-item");

    let controls = build_window_controls(window);

    let strip = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    strip.set_valign(gtk4::Align::Center);
    let tray = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    tray.set_valign(gtk4::Align::Center);
    tray.set_margin_end(6);

    bar.append(&app_name);
    bar.append(&strip);
    bar.append(&project_name);
    bar.append(&tray);
    bar.append(&reload_agent);
    bar.append(&controls);

    let handle = gtk4::WindowHandle::new();
    handle.set_child(Some(&bar));
    TopBar {
        widget: handle.upcast(),
        items: vec![reload_agent.upcast()],
        controls,
        app_name,
        strip,
        tray,
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

    /// Modules P2: the tray's chips come and go, and `h`/`l` step over the ones that are not shown,
    /// stopping at the ends as they always did.
    #[test]
    fn h_and_l_step_over_items_that_are_not_shown() {
        use super::step_item;
        // [editor chip (hidden), terminal chip, agent chip (hidden), ↻]
        let visible = [false, true, false, true];
        assert_eq!(
            step_item(&visible, None, 1),
            Some(1),
            "Ctrl+k lands on the first shown item"
        );
        assert_eq!(step_item(&visible, Some(1), 1), Some(3));
        assert_eq!(step_item(&visible, Some(3), 1), Some(3), "stops at the end");
        assert_eq!(step_item(&visible, Some(3), -1), Some(1));
        assert_eq!(step_item(&visible, Some(1), -1), Some(1), "stops at the start");
        assert_eq!(
            step_item(&visible, Some(0), 1),
            Some(1),
            "from a hidden item: the first shown"
        );
        assert_eq!(step_item(&[false, false], None, 1), None);
    }
}
