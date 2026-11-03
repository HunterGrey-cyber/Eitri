//! Opt-in placement and input checks on an isolated Xvfb: run with `-- --ignored`.

use std::cell::RefCell;
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};

use eitri_core::project_chooser::{ChooserView, LineEv, RowKind, RowView};
use gtk4::prelude::*;

#[path = "support/own_x_server.rs"]
mod own_x_server;
#[allow(dead_code, unused_imports)]
#[path = "../src/gtk_host/project_chooser.rs"]
mod project_chooser;
use project_chooser::{ChooserReport, ProjectChooserView};

fn pump() {
    let end = Instant::now() + Duration::from_millis(150);
    let context = gtk4::glib::MainContext::default();
    while Instant::now() < end {
        while context.iteration(false) {}
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn view() -> ChooserView {
    ChooserView {
        title: "Open a project".into(),
        line: String::new(),
        placeholder: "filter, or a path: ~/src/app".into(),
        rows: (0..35)
            .map(|_| RowView {
                name: "<b>应用</b>\n".repeat(80),
                shown: "/a/very/long/path".repeat(80),
                right: "open".into(),
                dimmed: false,
                kind: RowKind::Project,
            })
            .collect(),
        selected: Some(0),
        message: None,
        footer: "Enter open · ↑↓ move · Tab complete · Esc cancel".into(),
        opening: false,
        in_line: true,
        browsing: false,
        hidden: false,
        rows_rev: 1,
    }
}

struct Stage {
    window: gtk4::Window,
    overlay: gtk4::Overlay,
    chooser: ProjectChooserView,
    reports: Rc<RefCell<Vec<ChooserReport>>>,
}

impl Stage {
    fn new() -> Self {
        Self::sized(400)
    }

    fn sized(width: i32) -> Self {
        let reports = Rc::new(RefCell::new(Vec::new()));
        let chooser = ProjectChooserView::new(Rc::new({
            let reports = reports.clone();
            move |report| reports.borrow_mut().push(report)
        }));
        let window = gtk4::Window::new();
        window.set_title(Some("eitri-project-placement"));
        window.set_default_size(width, 500);
        let overlay = gtk4::Overlay::new();
        overlay.set_child(Some(&gtk4::Label::new(Some("body"))));
        window.set_child(Some(&overlay));
        window.present();
        pump();
        Self {
            window,
            overlay,
            chooser,
            reports,
        }
    }

    fn show(&self) {
        self.overlay.add_overlay(self.chooser.widget());
        self.chooser.focus();
        self.chooser.show(Some(&view()));
        pump();
    }

    fn widgets(&self) -> Vec<gtk4::Widget> {
        fn walk(widget: &gtk4::Widget, all: &mut Vec<gtk4::Widget>) {
            all.push(widget.clone());
            let mut child = widget.first_child();
            while let Some(widget) = child {
                walk(&widget, all);
                child = widget.next_sibling();
            }
        }
        let mut all = Vec::new();
        walk(self.chooser.widget(), &mut all);
        all
    }

    fn entry_has_keys(&self) -> bool {
        let entry = self.widgets().into_iter().find(|w| w.is::<gtk4::Entry>()).unwrap();
        GtkWindowExt::focus(&self.window).is_some_and(|focus| focus == entry || focus.is_ancestor(&entry))
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        self.window.destroy();
        pump();
    }
}

fn xdo(display: &str, args: &[&str]) -> String {
    let output = Command::new("xdotool")
        .env("DISPLAY", display)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "xdotool failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn focus_window(display: &str, title: &str) {
    let xid = xdo(display, &["search", "--onlyvisible", "--name", &format!("^{title}$")]);
    xdo(display, &["windowfocus", &xid]);
    pump();
}

fn click(display: &str, stage: &Stage, x: i32, y: i32) {
    let xid = xdo(
        display,
        &["search", "--onlyvisible", "--name", "^eitri-project-placement$"],
    );
    xdo(display, &["windowfocus", &xid]);
    xdo(
        display,
        &["mousemove", "--window", &xid, &x.to_string(), &y.to_string()],
    );
    xdo(display, &["click", "1"]);
    pump();
    assert!(stage.window.is_visible());
}

fn it_never_raises_the_windows_minimum() {
    let stage = Stage::new();
    let before = stage.window.measure(gtk4::Orientation::Horizontal, -1);
    stage.show();
    assert_eq!(stage.window.measure(gtk4::Orientation::Horizontal, -1), before);
    assert!(
        stage.window.width() <= 400,
        "the overlay keeps the tiled window's width"
    );
    let panel = stage
        .widgets()
        .into_iter()
        .find(|w| w.has_css_class("project-chooser-panel"))
        .unwrap();
    assert!(panel.width() <= 400);
    let labels: Vec<_> = stage
        .widgets()
        .into_iter()
        .filter_map(|w| w.downcast::<gtk4::Label>().ok())
        .collect();
    assert!(labels.iter().all(|l| !l.uses_markup()));
    assert!(labels.iter().any(|l| l.text().starts_with("<b>应用</b>?")));
}

fn a_row_click_reports_a_pick_and_the_line_keeps_the_keys(display: &str) {
    let stage = Stage::new();
    stage.show();
    assert!(stage.entry_has_keys(), "focus before the first draw reaches the line");
    let row = stage
        .widgets()
        .into_iter()
        .find_map(|w| w.downcast::<gtk4::ListBoxRow>().ok())
        .unwrap();
    assert!(!row.is_focusable());
    let bounds = row.compute_bounds(&stage.window).unwrap();
    click(
        display,
        &stage,
        (bounds.x() + 8.0) as i32,
        (bounds.y() + bounds.height() / 2.0) as i32,
    );
    assert!(stage
        .reports
        .borrow()
        .iter()
        .any(|r| matches!(r, ChooserReport::Pick { index: 0, .. })));
    assert!(stage.entry_has_keys());
    xdo(display, &["key", "--clearmodifiers", "ctrl+n"]);
    pump();
    assert!(stage
        .reports
        .borrow()
        .iter()
        .any(|r| matches!(r, ChooserReport::Line(LineEv::Step(1)))));
    stage.chooser.show(None);
    stage.chooser.focus();
    stage.chooser.show(Some(&view()));
    pump();
    assert!(
        stage.entry_has_keys(),
        "a hidden chooser takes the keys before it is filled again"
    );
}

fn a_click_outside_reports_dismiss(display: &str) {
    let stage = Stage::new();
    stage.show();
    click(display, &stage, 2, 2);
    assert!(stage
        .reports
        .borrow()
        .iter()
        .any(|r| matches!(r, ChooserReport::Dismiss)));
}

fn losing_the_window_does_not_report_the_keys_lost(display: &str) {
    let stage = Stage::new();
    stage.show();
    assert!(stage.entry_has_keys());
    let other = gtk4::Window::new();
    other.set_title(Some("eitri-project-other"));
    other.set_default_size(200, 100);
    other.present();
    pump();
    focus_window(display, "eitri-project-other");
    assert!(!stage.window.is_active());
    assert!(
        !stage
            .reports
            .borrow()
            .iter()
            .any(|r| matches!(r, ChooserReport::Line(LineEv::KeysLost))),
        "the window deactivating is not another module taking the keys"
    );
    focus_window(display, "eitri-project-placement");
    assert!(
        stage.entry_has_keys(),
        "the line still holds the keys when the window comes back"
    );
    other.destroy();
    pump();
}

fn the_keys_going_to_another_widget_while_active_report_lost() {
    let stage = Stage::new();
    let other = gtk4::Entry::new();
    stage.overlay.add_overlay(&other);
    stage.show();
    assert!(stage.window.is_active() || stage.entry_has_keys());
    other.grab_focus();
    pump();
    assert!(stage
        .reports
        .borrow()
        .iter()
        .any(|r| matches!(r, ChooserReport::Line(LineEv::KeysLost))));
}

fn shift_tab_stays_in_the_line(display: &str) {
    let stage = Stage::new();
    let other = gtk4::Entry::new();
    other.set_halign(gtk4::Align::Start);
    other.set_valign(gtk4::Align::Start);
    stage.overlay.add_overlay(&other);
    stage.show();
    focus_window(display, "eitri-project-placement");
    xdo(display, &["key", "--clearmodifiers", "shift+Tab"]);
    pump();
    assert!(stage.entry_has_keys());
    assert!(!stage
        .reports
        .borrow()
        .iter()
        .any(|r| matches!(r, ChooserReport::Line(LineEv::KeysLost))));
}

fn entry_text(stage: &Stage) -> String {
    let entry = stage
        .widgets()
        .into_iter()
        .find_map(|w| w.downcast::<gtk4::Entry>().ok())
        .unwrap();
    entry.text().to_string()
}

fn keys_of(stage: &Stage) -> Vec<char> {
    stage
        .reports
        .borrow()
        .iter()
        .filter_map(|r| match r {
            ChooserReport::Line(LineEv::Key(c)) => Some(*c),
            _ => None,
        })
        .collect()
}

/// While the list has the keys a plain printable key is reported as a command and the entry stays empty; the other
/// keys keep their meaning, and once the line has the keys the same key is text.
fn the_list_reports_plain_keys_and_the_line_takes_text(display: &str) {
    let stage = Stage::new();
    stage.overlay.add_overlay(stage.chooser.widget());
    stage.chooser.focus();
    let mut list = view();
    list.in_line = false;
    list.placeholder = "press / to filter or type a path".into();
    stage.chooser.show(Some(&list));
    pump();
    focus_window(display, "eitri-project-placement");
    for key in ["j", "k", "g", "shift+g", "slash", "q", "shift+grave"] {
        xdo(display, &["key", "--clearmodifiers", key]);
        pump();
    }
    assert_eq!(keys_of(&stage), vec!['j', 'k', 'g', 'G', '/', 'q', '~']);
    assert_eq!(entry_text(&stage), "", "the list's keys are not the entry's text");
    xdo(display, &["key", "--clearmodifiers", "BackSpace"]);
    xdo(display, &["key", "--clearmodifiers", "Left"]);
    pump();
    let others = |stage: &Stage| {
        stage
            .reports
            .borrow()
            .iter()
            .filter(|r| matches!(r, ChooserReport::Line(LineEv::Other)))
            .count()
    };
    assert_eq!(others(&stage), 2, "a key that is no command still ends a waiting g");
    xdo(display, &["key", "--clearmodifiers", "ctrl+n"]);
    xdo(display, &["key", "--clearmodifiers", "Return"]);
    xdo(display, &["key", "--clearmodifiers", "Escape"]);
    pump();
    let reports = stage.reports.borrow();
    assert!(reports
        .iter()
        .any(|r| matches!(r, ChooserReport::Line(LineEv::Step(1)))));
    assert!(reports.iter().any(|r| matches!(r, ChooserReport::Line(LineEv::Enter))));
    assert!(reports.iter().any(|r| matches!(r, ChooserReport::Line(LineEv::Escape))));
    drop(reports);
    stage.chooser.show(Some(&view()));
    pump();
    xdo(display, &["key", "--clearmodifiers", "j"]);
    pump();
    assert_eq!(entry_text(&stage), "j", "in the line a plain key is text");
    assert_eq!(keys_of(&stage).len(), 7, "and no command");
    assert_eq!(others(&stage), 2, "and no other-key report");
}

fn updates_over(stage: &Stage, millis: u64) -> u32 {
    let clock = stage.window.frame_clock().unwrap();
    let count = Rc::new(std::cell::Cell::new(0u32));
    let id = clock.connect_update({
        let count = count.clone();
        move |_| count.set(count.get() + 1)
    });
    let end = Instant::now() + Duration::from_millis(millis);
    let context = gtk4::glib::MainContext::default();
    while Instant::now() < end {
        while context.iteration(false) {}
        std::thread::sleep(Duration::from_millis(2));
    }
    clock.disconnect(id);
    count.get()
}

fn the_frame_tick_runs_only_while_the_chooser_is_up() {
    if let Some(settings) = gtk4::Settings::default() {
        settings.set_gtk_cursor_blink(false);
    }
    let stage = Stage::new();
    pump();
    assert_eq!(updates_over(&stage, 400), 0, "a window with no chooser is idle");
    stage.show();
    pump();
    assert!(
        updates_over(&stage, 400) >= 5,
        "the chooser fits itself by frames while it is up"
    );
    stage.chooser.show(None);
    pump();
    pump();
    assert_eq!(updates_over(&stage, 400), 0, "a hidden chooser wakes nothing");
    stage.chooser.focus();
    stage.chooser.show(Some(&view()));
    assert!(updates_over(&stage, 400) >= 5, "it comes back when it is shown again");
}

fn a_long_message_does_not_widen_the_panel() {
    let stage = Stage::sized(1000);
    stage.overlay.add_overlay(stage.chooser.widget());
    stage.chooser.focus();
    let mut long = view();
    long.message = Some(format!("no such folder: {}", "/very/long/segment".repeat(40)));
    stage.chooser.show(Some(&long));
    pump();
    pump();
    let panel = stage
        .widgets()
        .into_iter()
        .find(|w| w.has_css_class("project-chooser-panel"))
        .unwrap();
    assert!(stage.window.width() >= 900, "the window is wide enough to tell");
    assert!(panel.width() <= 640, "the panel is {} px", panel.width());
}

fn run(filter: &Option<String>, name: &str, check: impl FnOnce()) {
    if filter.as_ref().is_some_and(|f| !name.contains(f.as_str())) {
        return;
    }
    check();
    println!("{name}: ok");
}

fn main() {
    if !std::env::args().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("project_chooser_placement: ignored (requires its own Xvfb); run with `-- --ignored`");
        return;
    }
    // A bare argument filters by name, as the libtest harness does.
    let filter = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    let server = own_x_server::init_gtk("project_chooser_placement", "1024x768x24");
    let d = server.display();
    let f = &filter;
    run(
        f,
        "it_never_raises_the_windows_minimum",
        it_never_raises_the_windows_minimum,
    );
    run(f, "a_row_click_reports_a_pick_and_the_line_keeps_the_keys", || {
        a_row_click_reports_a_pick_and_the_line_keeps_the_keys(d)
    });
    run(f, "a_click_outside_reports_dismiss", || {
        a_click_outside_reports_dismiss(d)
    });
    run(f, "losing_the_window_does_not_report_the_keys_lost", || {
        losing_the_window_does_not_report_the_keys_lost(d)
    });
    run(
        f,
        "the_keys_going_to_another_widget_while_active_report_lost",
        the_keys_going_to_another_widget_while_active_report_lost,
    );
    run(f, "shift_tab_stays_in_the_line", || shift_tab_stays_in_the_line(d));
    run(f, "the_list_reports_plain_keys_and_the_line_takes_text", || {
        the_list_reports_plain_keys_and_the_line_takes_text(d)
    });
    run(
        f,
        "the_frame_tick_runs_only_while_the_chooser_is_up",
        the_frame_tick_runs_only_while_the_chooser_is_up,
    );
    run(
        f,
        "a_long_message_does_not_widen_the_panel",
        a_long_message_does_not_widen_the_panel,
    );
}
