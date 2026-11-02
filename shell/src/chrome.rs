//! Window chrome: top bar and window controls (minimize/maximize/close). Styling itself lives in
//! `theme::gtk_css`.
//!
//! **There is no bottom status bar** (deleted 2026-09-19; the UI design had already
//! called for it). Its last job was naming the focused pane, which the cursors do on their own
//! (the focus report).

use std::rc::Rc;

use eitri_core::wm::titles::window_title;
use eitri_core::wm::{EntryEv, HostEvent, WinId};
use gtk4::{prelude::*, ApplicationWindow};

/// The top bar and the parts of it other modules act on.
pub(crate) struct TopBar {
    /// The whole bar (the `WindowHandle` around it). Hidden in immersive mode (`window_mode`).
    pub(crate) widget: gtk4::Widget,
    /// Its keyboard-navigable items, left to right; see `build_top_bar`'s doc.
    pub(crate) items: Vec<gtk4::Widget>,
    /// Minimize/maximize/close. Hidden whenever the window is fullscreen (`window_mode`).
    pub(crate) controls: gtk4::Widget,
    /// The `Eitri` label, drawn as a solid block while the `Ctrl+a` prefix waits (`prefix`).
    pub(crate) app_name: gtk4::Label,
    /// The project name, opening the project chooser on click or keyboard activation.
    pub(crate) project: gtk4::Button,
    /// Where the prefix strip goes, right of the app name (`prefix_strip`).
    pub(crate) strip: gtk4::Box,
    /// Where the tray goes, left of `↻` (`tray`). Its chips precede the other top-bar items;
    /// `main.rs` joins the two lists.
    pub(crate) tray: gtk4::Box,
    /// `marked: <title>`, left of the tray, shown only while a module is marked (the chrome model's `mark`).
    pub(crate) mark: gtk4::Label,
    /// The linked window's counts, always drawn as plain text.
    pub(crate) other: gtk4::Label,
    /// `selected: <kind> (<titles>)`, beside the mark, shown only while a container is selected
    /// (the chrome model's `selection`). Set with [`show_mark`]: the same plain-text segment.
    pub(crate) selection: gtk4::Label,
    /// The current workspace's name, left of the selection and the mark, shown only while the window
    /// has more than one workspace (the chrome model's `workspace`, [`show_workspace`]). A top-bar item, ahead of
    /// the tray's chips: `h`/`l` and HINT reach it, and activating it opens the workspace chooser.
    pub(crate) workspace: gtk4::Button,
    /// The workspace button's text, set with [`show_workspace`] as plain text.
    pub(crate) workspace_label: gtk4::Label,
    /// Where the current workspace is renamed: hidden until a rename starts, made here so the top bar's
    /// widgets and items are fixed when it is built.
    pub(crate) rename: gtk4::Entry,
}

/// Shows `text` on the workspace button, or hides it, and gives the window holding the label the
/// matching title ([`window_title`]), so the title and the label never disagree. Plain text, never
/// markup: a workspace name is whatever the user or a state file gave.
pub(crate) fn show_workspace(button: &gtk4::Button, label: &gtk4::Label, project: &str, text: Option<&str>) {
    label.set_text(text.unwrap_or(""));
    button.set_visible(text.is_some());
    if let Some(window) = label.root().and_downcast::<gtk4::Window>() {
        window.set_title(Some(&window_title(project, text)));
    }
}

/// Shows the rename entry holding `current` as plain text, selected, so typing replaces it: what opening it and a
/// new name while it is open both draw. The keys are given to it on their own, not here.
pub(crate) fn fill_rename(entry: &gtk4::Entry, current: &str) {
    entry.set_text(current);
    entry.set_visible(true);
    entry.select_region(0, -1);
}

/// Hides the rename entry, holding nothing, so the next rename never shows what was typed before.
pub(crate) fn close_rename(entry: &gtk4::Entry) {
    entry.set_visible(false);
    entry.set_text("");
}

/// The rename entry's own events, told to the controller and nothing else: `Enter` with the text typed, `Esc`, and
/// the keys leaving it any other way (a click elsewhere, a key that moved them). What is renamed, where the keys go
/// back to and when the entry closes are the controller's to decide, so the entry never renames, hides or refills
/// itself.
pub(crate) fn wire_rename_entry(entry: &gtk4::Entry, post: Rc<dyn Fn(HostEvent)>) {
    let report: Rc<dyn Fn(EntryEv)> = Rc::new(move |ev| post(HostEvent::TopBarEntry { win: WinId::MAIN, ev }));
    {
        let report = report.clone();
        entry.connect_activate(move |entry| report(EntryEv::Enter(entry.text().to_string())));
    }
    let keys = gtk4::EventControllerKey::new();
    keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
    {
        let report = report.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key != gtk4::gdk::Key::Escape {
                return gtk4::glib::Propagation::Proceed;
            }
            report(EntryEv::Escape);
            gtk4::glib::Propagation::Stop
        });
    }
    entry.add_controller(keys);
    let left = gtk4::EventControllerFocus::new();
    left.connect_leave(move |_| report(EntryEv::LostKeys));
    entry.add_controller(left);
}

/// Shows `text` on the mark label, or hides it. Plain text, never markup: a Lua panel's title is
/// the user's own string.
pub(crate) fn show_mark(label: &gtk4::Label, text: Option<&str>) {
    label.set_text(text.unwrap_or(""));
    label.set_visible(text.is_some());
}

/// Builds the top bar. Returns the bar and the parts of it other modules act on (`TopBar`).
///
/// `Ctrl+k` reaches the top bar from either pane (2026-09-19): it is spatially above both, the
/// same way `Ctrl+l` reaches the panel. The items are what `h`/`l` move between once there: `↻`
/// here, with the project, workspace and tray's chips ahead of it (`main.rs` joins the lists).
/// **The window controls are deliberately not items and cannot take focus at all** (see
/// `build_window_controls`): a stray `Enter` after `Ctrl+k` must never be able to close the window.
pub(crate) fn build_top_bar(window: &ApplicationWindow, project: &str) -> TopBar {
    let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    bar.add_css_class("topbar");
    bar.set_valign(gtk4::Align::Fill);

    let app_name = gtk4::Label::new(Some("Eitri"));
    app_name.add_css_class("topbar-app-name");

    let project_text: String = project.chars().map(|c| if c.is_control() { '?' } else { c }).collect();
    let project_name = gtk4::Label::new(Some(&project_text));
    project_name.set_halign(gtk4::Align::Start);
    // Ellipsize the project name when the prefix strip needs room in a narrow window.
    // The strip's labels also ellipsize through `PrefixStrip::show`, so neither forces the bar
    // or window to fit the full text. Tray chips do not shrink, so many chips at a large zoom
    // can still increase the minimum width.
    project_name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    let project = gtk4::Button::new();
    project.set_child(Some(&project_name));
    project.add_css_class("topbar-project-name");
    project.add_css_class("topbar-item");
    project.set_valign(gtk4::Align::Center);
    project.set_focus_on_click(false);
    let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);

    // Reload the agent panel through the same `app.reload-agent-panel` action as `prefix r`.
    // The button lives in the window chrome so it remains available when the panel's page hangs.
    // The shortcut enables recovery; the button makes it discoverable.
    //
    // Its appearance has not been verified. It shares `win-btn` with the window controls and sits
    // immediately before them, so it may look like part of that cluster instead of a panel control.
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
    let other = gtk4::Label::new(None);
    other.add_css_class("topbar-mark");
    other.set_valign(gtk4::Align::Center);
    other.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    other.set_visible(false);
    let mark = gtk4::Label::new(None);
    mark.add_css_class("topbar-mark");
    mark.set_valign(gtk4::Align::Center);
    // A long title gives way rather than widening the bar, as the project name does.
    mark.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    mark.set_visible(false);
    // Styled as the mark is: both are a word about the layout, not an item to reach.
    let selection = gtk4::Label::new(None);
    selection.add_css_class("topbar-mark");
    selection.add_css_class("topbar-selection");
    selection.set_valign(gtk4::Align::Center);
    selection.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    selection.set_visible(false);
    // A button around a label rather than a button's own label, so the name is set with `set_text`
    // and nothing else. Not focused by a click: a click opens the chooser, whose key is read by the
    // prefix wherever the keys are, and they stay in the module that had them.
    let workspace_label = gtk4::Label::new(None);
    workspace_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    let workspace = gtk4::Button::new();
    workspace.set_child(Some(&workspace_label));
    workspace.add_css_class("topbar-workspace");
    workspace.add_css_class("topbar-item");
    workspace.set_valign(gtk4::Align::Center);
    workspace.set_focus_on_click(false);
    workspace.set_tooltip_text(Some("Workspaces"));
    workspace.set_visible(false);
    let rename = gtk4::Entry::new();
    rename.add_css_class("topbar-rename");
    rename.set_valign(gtk4::Align::Center);
    rename.set_max_length(eitri_core::layout::workspace::MAX_NAME_CHARS as i32);
    rename.set_width_chars(12);
    rename.set_visible(false);

    bar.append(&app_name);
    bar.append(&strip);
    bar.append(&project);
    bar.append(&spacer);
    bar.append(&workspace);
    bar.append(&rename);
    bar.append(&selection);
    bar.append(&mark);
    bar.append(&other);
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
        project,
        strip,
        tray,
        mark,
        other,
        selection,
        workspace,
        workspace_label,
        rename,
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

    #[test]
    fn the_project_name_is_a_button_and_an_item_ahead_of_the_workspace() {
        use eitri_core::source_scan as scan;
        let compact = |text: &str| text.split_whitespace().collect::<String>();
        let chrome = scan::code_only(&scan::read_required("shell/src/chrome.rs").text);
        let chrome = scan::without(&chrome, &scan::modules(&chrome, "tests"));
        assert!(compact(&chrome).contains("pub(crate)project:gtk4::Button,"));
        let bodies = scan::functions(&chrome, "build_top_bar");
        assert_eq!(bodies.len(), 1, "the top bar is built in one place");
        let bar = compact(bodies[0]);
        assert!(bar.contains("letproject=gtk4::Button::new();"));
        assert!(bar.contains("project.set_child(Some(&project_name));"));
        assert!(bar.contains("bar.append(&project);bar.append(&spacer);bar.append(&workspace);"));
        let (_, main) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
        let main = compact(&main);
        assert!(main.contains(
            "lettop_items:Vec<gtk4::Widget>=std::iter::once(top_bar.project.clone().upcast()).chain(std::iter::once(top_bar.workspace.clone().upcast()))"
        ));
        assert!(main.contains("top_bar.project.connect_clicked("));
        assert!(main.contains("driver.run(WinId::MAIN,Action::ProjectChoose,Origin::Chip);"));
    }

    #[test]
    fn an_expanding_spacer_follows_it() {
        use eitri_core::source_scan as scan;
        let sources = [scan::read_required("shell/src/chrome.rs")];
        let (_, bar) = scan::function_in(&sources, "build_top_bar");
        let bar = bar.split_whitespace().collect::<String>();
        assert!(bar.contains("letspacer=gtk4::Box::new(gtk4::Orientation::Horizontal,0);"));
        assert!(bar.contains("spacer.set_hexpand(true);"));
        assert!(bar.contains("bar.append(&project);bar.append(&spacer);"));
        assert!(!bar.contains("project.set_hexpand(true)"));
        assert!(!bar.contains("project_name.set_hexpand(true)"));
    }

    /// The title is the label's twin: whichever function sets the workspace label sets the title in
    /// the same call, from the same text.
    #[test]
    fn the_workspace_text_and_the_title_are_set_together() {
        use eitri_core::source_scan as scan;
        let code = scan::code_only(&scan::read_required("shell/src/chrome.rs").text);
        let code = scan::without(&code, &scan::modules(&code, "tests"));
        let bodies = scan::functions(&code, "show_workspace");
        assert_eq!(bodies.len(), 1, "show_workspace is defined once");
        assert!(bodies[0].contains("label.set_text("), "the label is set: {}", bodies[0]);
        assert!(
            bodies[0].contains(".set_title(") && bodies[0].contains("window_title("),
            "the same function sets the window title from window_title: {}",
            bodies[0]
        );
    }

    /// A workspace name is the user's or a state file's text: the workspace button's label and the
    /// rename entry are given it with `set_text`, only in `show_workspace` and `fill_rename`, and the
    /// entry is emptied only in `close_rename` (the window's code sets no widget's text itself).
    #[test]
    fn a_workspace_name_is_set_as_text_never_markup() {
        use eitri_core::source_scan as scan;
        let without_tests = |path: &str| {
            let code = scan::code_only(&scan::read_required(path).text);
            scan::without(&code, &scan::modules(&code, "tests"))
        };
        let chrome = without_tests("shell/src/chrome.rs");
        for (setter, writes) in [
            ("show_workspace", "label.set_text("),
            ("fill_rename", "entry.set_text("),
            ("close_rename", "entry.set_text("),
        ] {
            let bodies = scan::functions(&chrome, setter);
            assert_eq!(bodies.len(), 1, "{setter} is defined once");
            assert!(
                bodies[0].contains(writes),
                "{setter} sets its text with set_text: {}",
                bodies[0]
            );
            assert!(
                !bodies[0].contains(".set_label("),
                "{setter}: a button's label is not set"
            );
        }
        // Any other text write would bypass the two setters above: a button's own label replaces
        // the plain label inside it, and an entry's text can be set or inserted.
        let setters = [".set_text(", ".set_label(", ".insert_text(", ".set_buffer("];
        let (_, main) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
        for setter in setters {
            assert!(
                !main.contains(setter),
                "main.rs calls {setter}: a widget's text goes through chrome's setters"
            );
        }
        let outside_setters = {
            let bodies: Vec<&str> = ["show_workspace", "fill_rename", "close_rename"]
                .iter()
                .flat_map(|name| scan::functions(&chrome, name))
                .collect();
            scan::without(&chrome, &bodies)
        };
        for widget in ["workspace_label", "workspace", "rename"] {
            for setter in setters {
                let call = format!("{widget}{setter}");
                assert!(
                    !outside_setters.contains(&call),
                    "chrome.rs calls {call} outside show_workspace, fill_rename and close_rename"
                );
            }
        }
    }

    fn check_markup(sources: &[eitri_core::source_scan::Source]) {
        use eitri_core::source_scan as scan;
        let allowlist = [
            (
                "shell/src/hint.rs",
                "on_targets",
                "sets label_markup's output, which escapes every piece",
            ),
            (
                "shell/src/hint.rs",
                "show_prefix",
                "sets label_markup's output, which escapes every piece",
            ),
            (
                "shell/src/editor_start_failure.rs",
                "show",
                "escapes its message with markup_escape_text before it is set",
            ),
        ];
        for source in sources {
            let code = scan::code_only(&source.text);
            let code = scan::without(&code, &scan::modules(&code, "tests"));
            // Every way gtk4-rs sets widget text as markup: the setters, the constructor and the builder
            // properties. A form left off this list would pass the scan unseen.
            for setter in [
                "set_markup",
                "set_markup_with_mnemonic",
                "set_use_markup",
                "set_tooltip_markup",
                "with_markup",
                "use_markup",
                "tooltip_markup",
            ] {
                for end in scan::word_ends(&code, setter) {
                    let before = code[..end - setter.len()].trim_end();
                    let after = code[end..].trim_start();
                    if !after.starts_with('(') || before.ends_with("fn") {
                        continue;
                    }
                    if setter == "set_use_markup" || setter == "use_markup" {
                        let argument = after[1..].trim_start();
                        if !argument
                            .strip_prefix("true")
                            .is_some_and(|rest| rest.trim_start().starts_with(')'))
                        {
                            continue;
                        }
                    }
                    let enclosing = scan::word_ends(&code, "fn")
                        .into_iter()
                        .filter_map(|at| {
                            let rest = code[at..].trim_start();
                            let len = rest
                                .chars()
                                .take_while(|c| c.is_alphanumeric() || *c == '_')
                                .map(char::len_utf8)
                                .sum::<usize>();
                            (len > 0).then_some(&rest[..len])
                        })
                        .flat_map(|name| scan::functions(&code, name).into_iter().map(move |body| (name, body)))
                        .filter(|(_, body)| {
                            let start = body.as_ptr() as usize - code.as_ptr() as usize;
                            start <= end && end < start + body.len()
                        })
                        .min_by_key(|(_, body)| body.len());
                    let allowed = enclosing.is_some_and(|(name, _)| {
                        allowlist
                            .iter()
                            .any(|(path, allowed_name, _reason)| source.path == *path && name == *allowed_name)
                    });
                    assert!(allowed, "{} calls {setter} outside the allowed functions", source.path);
                }
            }
        }
    }

    #[test]
    fn no_widget_text_is_set_as_markup_outside_the_allowlist() {
        check_markup(&eitri_core::source_scan::rust_sources(&["shell/src"]));
    }

    #[test]
    fn the_markup_scan_catches_a_set_markup_in_a_new_file() {
        let fixture = [eitri_core::source_scan::Source {
            path: "shell/src/new_widget.rs".into(),
            text: "fn show() { label.set_markup(&text); }".into(),
        }];
        assert!(std::panic::catch_unwind(|| check_markup(&fixture)).is_err());
    }

    #[test]
    fn the_markup_scan_catches_an_allowlisted_call_in_a_new_place() {
        let fixture = [eitri_core::source_scan::Source {
            path: "shell/src/hint.rs".into(),
            text: "fn elsewhere() { label.set_markup(&text); }".into(),
        }];
        assert!(std::panic::catch_unwind(|| check_markup(&fixture)).is_err());
    }

    #[test]
    fn the_markup_scan_catches_the_tooltip_constructor_and_builder_forms() {
        for text in [
            "fn show() { button.set_tooltip_markup(Some(&text)); }",
            "fn show() { let label = gtk::Label::with_markup(&text); }",
            "fn show() { let label = gtk::Label::builder().label(&text).use_markup(true).build(); }",
            "fn show() { let button = gtk::Button::builder().tooltip_markup(&text).build(); }",
        ] {
            let fixture = [eitri_core::source_scan::Source {
                path: "shell/src/new_widget.rs".into(),
                text: text.into(),
            }];
            assert!(
                std::panic::catch_unwind(|| check_markup(&fixture)).is_err(),
                "a markup form the scan must refuse passed: {text}"
            );
        }
    }

    #[test]
    fn the_markup_scan_passes_the_safe_form() {
        check_markup(&[eitri_core::source_scan::Source {
            path: "shell/src/new_widget.rs".into(),
            text: "fn show() { label.set_use_markup(false); let markup = text; label_markup(&markup); \
                   let other = gtk::Label::builder().use_markup(false).build(); }"
                .into(),
        }]);
    }
}
