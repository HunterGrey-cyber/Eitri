//! Window chrome: top bar and window controls (minimize/maximize/close).
//! Styling lives in `theme::gtk_css`.
//!
//! There is no bottom status bar. The pane cursors indicate keyboard focus.

use std::path::Path;

use eitri_core::layout::ModuleId;
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
    /// The `Eitri` label, drawn as a solid block while the `Ctrl+a` prefix waits (`prefix`).
    pub(crate) app_name: gtk4::Label,
    /// Where the prefix strip goes, right of the app name (`prefix_strip`).
    pub(crate) strip: gtk4::Box,
    /// Where the tray goes, left of `↻` (`tray`). Its chips precede the other top-bar items;
    /// `main.rs` joins the two lists.
    pub(crate) tray: gtk4::Box,
    /// `marked: <title>`, left of the tray, shown only while a module is marked ([`mark_text`]).
    pub(crate) mark: gtk4::Label,
    /// `selected: <kind> (<titles>)`, beside the mark, shown only while a container is selected
    /// (`tray::selection_text`). Set with [`show_mark`]: the same plain-text segment.
    pub(crate) selection: gtk4::Label,
    /// The current workspace's name, left of the selection and the mark, shown only while the window
    /// has more than one workspace ([`workspace_text`], [`show_workspace`]). A top-bar item, ahead of
    /// the tray's chips: `h`/`l` and HINT reach it, and activating it opens the workspace chooser.
    pub(crate) workspace: gtk4::Button,
    /// The workspace button's text, set with [`show_workspace`] as plain text.
    pub(crate) workspace_label: gtk4::Label,
    /// Where the current workspace is renamed: hidden until a rename starts, made here so the top bar's
    /// widgets and items are fixed when it is built.
    pub(crate) rename: gtk4::Entry,
}

/// What the top bar says about the workspaces: the current one's name while there are several, and
/// nothing while there is one -- a window that never made a second workspace shows no word for it.
pub(crate) fn workspace_text(order: &[String], current: &str) -> Option<String> {
    (order.len() > 1).then(|| current.to_string())
}

/// The window's title: the project's name, then the product, then -- while the window has several
/// workspaces -- the one on screen. The window has no title bar, so the title is what a window
/// switcher, an overview and a compositor's window rules read to tell two windows apart.
pub(crate) fn window_title(project: &str, workspace: Option<&str>) -> String {
    match workspace {
        Some(name) => format!("{project} \u{2014} Eitri \u{b7} {name}"),
        None => format!("{project} \u{2014} Eitri"),
    }
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

/// Opens the rename entry holding `current`, selected, so typing replaces it.
pub(crate) fn start_rename(entry: &gtk4::Entry, current: &str) {
    entry.set_text(current);
    entry.set_visible(true);
    entry.grab_focus();
    entry.select_region(0, -1);
}

/// What the top bar says about the mark `swap.marked` swaps with: nothing without one. The mark
/// is shown only as this text -- a frame or outline on the marked module would take layout space
/// from it, or be one more kind of chrome around a pane where focus is shown by cursors alone.
pub(crate) fn mark_text(mark: Option<&ModuleId>, title: &dyn Fn(&ModuleId) -> String) -> Option<String> {
    mark.map(|id| format!("marked: {}", title(id)))
}

/// Shows `text` on the mark label, or hides it. Plain text, never markup: a Lua panel's title is
/// the user's own string.
pub(crate) fn show_mark(label: &gtk4::Label, text: Option<&str>) {
    label.set_text(text.unwrap_or(""));
    label.set_visible(text.is_some());
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

/// Builds the top bar and returns the parts other modules use (`TopBar`).
/// `Ctrl+k` reaches the bar from either pane because it is above both. `h` and `l` move
/// between the tray chips and `↻`; `main.rs` joins the two lists. Window controls cannot take
/// focus, so an accidental `Enter` after `Ctrl+k` cannot close the window.
pub(crate) fn build_top_bar(window: &ApplicationWindow, project_root: &Path) -> TopBar {
    let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    bar.add_css_class("topbar");
    bar.set_valign(gtk4::Align::Fill);

    let app_name = gtk4::Label::new(Some("Eitri"));
    app_name.add_css_class("topbar-app-name");

    let project_name = gtk4::Label::new(Some(&project_display_name(project_root)));
    project_name.add_css_class("topbar-project-name");
    project_name.set_hexpand(true);
    project_name.set_halign(gtk4::Align::Start);
    // Ellipsize the project name when the prefix strip needs room in a narrow window.
    // The strip's labels also ellipsize through `PrefixStrip::show`, so neither forces the bar
    // or window to fit the full text. Tray chips do not shrink, so many chips at a large zoom
    // can still increase the minimum width.
    project_name.set_ellipsize(gtk4::pango::EllipsizeMode::End);

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
    bar.append(&project_name);
    bar.append(&workspace);
    bar.append(&rename);
    bar.append(&selection);
    bar.append(&mark);
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
        mark,
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
    use super::project_display_name;
    use eitri_core::layout::ModuleId;
    use std::path::Path;

    #[test]
    fn the_mark_reads_as_text_naming_its_module_and_is_absent_without_one() {
        use super::mark_text;
        let title = |id: &ModuleId| id.as_str().to_string();
        assert_eq!(mark_text(None, &title), None);
        assert_eq!(
            mark_text(Some(&ModuleId::agent()), &title),
            Some("marked: agent".to_string())
        );
    }

    #[test]
    fn the_title_is_the_project_then_eitri() {
        assert_eq!(super::window_title("neovibe", None), "neovibe \u{2014} Eitri");
    }

    #[test]
    fn a_second_workspace_joins_the_title() {
        assert_eq!(
            super::window_title("neovibe", Some("docs")),
            "neovibe \u{2014} Eitri \u{b7} docs"
        );
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

    #[test]
    fn the_top_bar_names_the_workspace_only_while_there_are_several() {
        use super::workspace_text;
        let names = |list: &[&str]| list.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(
            workspace_text(&names(&["main"]), "main"),
            None,
            "one workspace: no word for it"
        );
        assert_eq!(
            workspace_text(&names(&["main", "study"]), "study"),
            Some("study".to_string())
        );
        assert_eq!(
            workspace_text(&names(&["main", "study", "review"]), "main"),
            Some("main".to_string()),
            "main is named too once there are several"
        );
    }

    /// A workspace name is the user's or a state file's text: the workspace button's label and the
    /// rename entry are given it with `set_text`, only in `show_workspace` and `start_rename` (the
    /// window's code sets no widget's text itself).
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
            ("start_rename", "entry.set_text("),
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
            let bodies: Vec<&str> = ["show_workspace", "start_rename"]
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
                    "chrome.rs calls {call} outside show_workspace and start_rename"
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

    /// `h` and `l` skip tray chips that are hidden and stop at either end.
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
