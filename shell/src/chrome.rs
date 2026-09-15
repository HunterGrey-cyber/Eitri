//! Window chrome: top bar, window controls (minimize/maximize/close), status bar, and CSS styling.

use gtk4::{
    prelude::*,
    ApplicationWindow,
};

pub(crate) fn build_top_bar(window: &ApplicationWindow) -> gtk4::Widget {
    let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    bar.add_css_class("topbar");
    bar.set_valign(gtk4::Align::Fill);

    let app_name = gtk4::Label::new(Some("neovibe"));
    app_name.add_css_class("topbar-app-name");

    let project_name = gtk4::Label::new(Some("project"));
    project_name.add_css_class("topbar-project-name");
    project_name.set_hexpand(true);
    project_name.set_halign(gtk4::Align::Start);

    // Reload the agent panel's frontend -- the same `app.reload-agent-panel` action Ctrl+Shift+R
    // fires (see `agent_panel::install_reload_action`). It lives in the window chrome rather than in
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
    reload_agent.set_tooltip_text(Some("Reload the agent panel (Ctrl+Shift+R) — the session keeps running"));
    reload_agent.set_action_name(Some("app.reload-agent-panel"));

    bar.append(&app_name);
    bar.append(&project_name);
    bar.append(&reload_agent);
    bar.append(&build_window_controls(window));

    let handle = gtk4::WindowHandle::new();
    handle.set_child(Some(&bar));
    handle.upcast()
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

    controls.append(&minimize);
    controls.append(&maximize);
    controls.append(&close);
    controls.upcast()
}

/// Bottom status bar strip, carried over from `shell_chrome::build_status_bar` unchanged.
pub(crate) fn build_status_bar() -> gtk4::Widget {
    let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    bar.add_css_class("statusbar");

    let mode = gtk4::Label::new(Some("NORMAL"));
    mode.add_css_class("statusbar-accent");

    let sep = gtk4::Label::new(Some("  \u{2014}  Ln 1, Col 1"));

    bar.append(&mode);
    bar.append(&sep);

    bar.upcast()
}

pub(crate) fn apply_css(css: &str) {
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(css);

    let display = gtk4::gdk::Display::default().expect("no default GDK display");
    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
