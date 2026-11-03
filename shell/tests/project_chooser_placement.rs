//! Opt-in placement and input checks on an isolated Xvfb: run with `-- --ignored`.

use std::cell::RefCell;
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;
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

/// What the carrier does on a pick before the next event is read: the launch window redraws synchronously.
type OnPick = Rc<RefCell<Option<Box<dyn Fn(&ProjectChooserView)>>>>;

struct Stage {
    window: gtk4::Window,
    overlay: gtk4::Overlay,
    chooser: Rc<ProjectChooserView>,
    reports: Rc<RefCell<Vec<ChooserReport>>>,
    on_pick: OnPick,
}

impl Stage {
    fn new() -> Self {
        Self::sized(400)
    }

    fn sized(width: i32) -> Self {
        let reports = Rc::new(RefCell::new(Vec::new()));
        let on_pick: OnPick = Rc::new(RefCell::new(None));
        let me: Rc<RefCell<std::rc::Weak<ProjectChooserView>>> = Rc::new(RefCell::new(std::rc::Weak::new()));
        let chooser = Rc::new(ProjectChooserView::new(Rc::new({
            let reports = reports.clone();
            let on_pick = on_pick.clone();
            let me = me.clone();
            move |report| {
                let pick = matches!(report, ChooserReport::Pick { .. });
                reports.borrow_mut().push(report);
                let chooser = me.borrow().upgrade();
                if let (true, Some(chooser), Some(hook)) = (pick, chooser, on_pick.borrow().as_ref()) {
                    hook(&chooser);
                }
            }
        })));
        *me.borrow_mut() = Rc::downgrade(&chooser);
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
            on_pick,
        }
    }

    fn rows(&self) -> Vec<gtk4::Widget> {
        let mut rows: Vec<_> = self
            .widgets()
            .into_iter()
            .filter(|w| w.has_css_class("project-chooser-row") && w.is_mapped())
            .collect();
        rows.sort_by_key(|w| w.compute_bounds(&self.window).map_or(i32::MAX, |b| b.y() as i32));
        rows
    }

    fn labelled(&self, class: &str) -> gtk4::Label {
        self.widgets()
            .into_iter()
            .find(|w| w.has_css_class(class))
            .and_then(|w| w.downcast::<gtk4::Label>().ok())
            .unwrap_or_else(|| panic!("no label .{class}"))
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
    assert!(labels.iter().any(|l| l
        .text()
        .trim_start_matches(['\u{200e}', '\u{2068}'])
        .starts_with("<b>应用</b>?")));
    assert!(!entry_text(&stage).contains('\u{2068}'), "the line holds no isolate");
}

fn a_row_click_reports_a_pick_and_the_line_keeps_the_keys(display: &str) {
    let stage = Stage::new();
    stage.show();
    assert!(stage.entry_has_keys(), "focus before the first draw reaches the line");
    let list = stage
        .widgets()
        .into_iter()
        .find(|w| w.has_css_class("project-chooser-list"))
        .unwrap();
    assert!(!list.can_focus(), "no row can take the keys from the line");
    let row = stage.rows().into_iter().next().unwrap();
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
    xdo(display, &["key", "--clearmodifiers", "Delete"]);
    xdo(display, &["key", "--clearmodifiers", "Home"]);
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
    xdo(display, &["key", "--clearmodifiers", "BackSpace"]);
    xdo(display, &["key", "--clearmodifiers", "Left"]);
    xdo(display, &["key", "--clearmodifiers", "Right"]);
    pump();
    let reports = stage.reports.borrow();
    let browse: Vec<_> = reports
        .iter()
        .filter_map(|r| match r {
            ChooserReport::Line(ev @ (LineEv::Up | LineEv::Side(_))) => Some(ev.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(browse, vec![LineEv::Up, LineEv::Side(-1), LineEv::Side(1)]);
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
    xdo(display, &["key", "--clearmodifiers", "ctrl+w"]);
    pump();
    assert!(stage
        .reports
        .borrow()
        .iter()
        .any(|r| matches!(r, ChooserReport::Line(LineEv::DeleteSegment))));
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

fn folders(n: usize) -> ChooserView {
    let mut rows = vec![RowView {
        name: ".".into(),
        shown: "~/src".into(),
        right: "this folder".into(),
        dimmed: false,
        kind: RowKind::Here,
    }];
    rows.extend((1..n).map(|i| RowView {
        name: format!("folder-{i}/"),
        shown: String::new(),
        right: if i % 3 == 0 { "git".into() } else { String::new() },
        dimmed: false,
        kind: RowKind::Folder,
    }));
    ChooserView {
        title: "Browse folders".into(),
        line: "~/src/".into(),
        placeholder: String::new(),
        rows: Arc::from(rows),
        selected: Some(0),
        message: None,
        footer: "Enter open src · l in · h up · / filter · . hidden · Esc back · q close".into(),
        opening: false,
        in_line: false,
        browsing: true,
        hidden: false,
        rows_rev: 7,
    }
}

/// Moving the cursor over a long folder keeps a bounded set of row widgets: the list view makes widgets for a window
/// of rows around what is on screen (GTK keeps about two hundred), never one per row, and a folder three times longer
/// makes no more of them.
fn a_2000_row_view_keeps_its_row_widgets_bounded() {
    let stage = Stage::sized(720);
    stage.overlay.add_overlay(stage.chooser.widget());
    stage.chooser.focus();
    let row_widgets = |stage: &Stage| {
        stage
            .widgets()
            .into_iter()
            .filter(|w| w.has_css_class("project-chooser-row"))
            .count()
    };
    let mut view = folders(2000);
    stage.chooser.show(Some(&view));
    pump();
    for i in 1..=100 {
        view.selected = Some(i);
        stage.chooser.show(Some(&view));
        while gtk4::glib::MainContext::default().iteration(false) {}
    }
    pump();
    view.selected = Some(1999);
    stage.chooser.show(Some(&view));
    pump();
    pump();
    let rows = row_widgets(&stage);
    println!("row widgets: {rows} for 2000 rows");
    assert!(rows <= 256, "{rows} row widgets for 2000 rows");
    let last = stage.rows().into_iter().last().expect("rows on screen");
    let name = last
        .first_child()
        .and_then(|w| w.downcast::<gtk4::Label>().ok())
        .unwrap();
    assert!(
        name.text().contains("folder-1999/"),
        "G shows the last row: {}",
        name.text()
    );
    let mut longer = folders(6000);
    longer.rows_rev = 8;
    longer.selected = Some(5999);
    stage.chooser.show(Some(&longer));
    pump();
    pump();
    let more = row_widgets(&stage);
    println!("row widgets: {more} for 6000 rows");
    assert!(
        more <= rows + 8,
        "{more} row widgets for 6000 rows against {rows} for 2000"
    );
    let panel = stage
        .widgets()
        .into_iter()
        .find(|w| w.has_css_class("project-chooser-panel"))
        .unwrap();
    assert!(panel.has_css_class("browsing"), "the panel says it shows a folder");
    view.browsing = false;
    stage.chooser.show(Some(&view));
    pump();
    assert!(!panel.has_css_class("browsing"));
}

/// A double-click carries the revision its first press saw: when that press changed the rows (a redraw in the same
/// turn), the second press must not act on the new rows.
fn a_double_click_reports_pick_twice_with_its_first_press_rev(display: &str) {
    let stage = Stage::sized(720);
    stage.overlay.add_overlay(stage.chooser.widget());
    stage.chooser.focus();
    let first = folders(5);
    stage.chooser.show(Some(&first));
    pump();
    let row = stage.rows().into_iter().nth(1).unwrap();
    let bounds = row.compute_bounds(&stage.window).unwrap();
    let (x, y) = ((bounds.x() + 8.0) as i32, (bounds.y() + bounds.height() / 2.0) as i32);
    let double = |stage: &Stage| {
        // Clicks closer together than the double-click time count on from the last ones: let the last ones end.
        std::thread::sleep(Duration::from_millis(800));
        pump();
        let xid = xdo(
            display,
            &["search", "--onlyvisible", "--name", "^eitri-project-placement$"],
        );
        xdo(display, &["windowfocus", &xid]);
        xdo(
            display,
            &["mousemove", "--window", &xid, &x.to_string(), &y.to_string()],
        );
        xdo(display, &["click", "--repeat", "2", "--delay", "60", "1"]);
        pump();
        let picks: Vec<(bool, usize, u64)> = stage
            .reports
            .borrow_mut()
            .drain(..)
            .filter_map(|r| match r {
                ChooserReport::Pick { index, rows_rev } => Some((false, index, rows_rev)),
                ChooserReport::PickTwice { index, rows_rev } => Some((true, index, rows_rev)),
                _ => None,
            })
            .collect();
        picks
    };
    assert_eq!(
        double(&stage),
        vec![(false, 1, 7), (true, 1, 7)],
        "a click selects, the second press acts on the same rows"
    );
    *stage.on_pick.borrow_mut() = Some(Box::new(|chooser: &ProjectChooserView| {
        let mut next = folders(3);
        next.rows_rev = 8;
        chooser.show(Some(&next));
    }));
    let picks = double(&stage);
    assert_eq!(picks.first(), Some(&(false, 1, 7)));
    assert!(
        picks.contains(&(true, 1, 7)),
        "the second press keeps the first press's revision, not the redrawn rows': {picks:?}"
    );
    assert!(!picks.iter().any(|(twice, _, rev)| *twice && *rev == 8), "{picks:?}");
}

fn picks_of(stage: &Stage) -> Vec<usize> {
    stage
        .reports
        .borrow_mut()
        .drain(..)
        .filter_map(|r| match r {
            ChooserReport::Pick { index, .. } => Some(index),
            _ => None,
        })
        .collect()
}

/// A press let go away from the row it pressed picks nothing, as a button's does; one let go on its own row picks it,
/// however far it moved inside the row and however long it was held.
fn a_press_let_go_off_its_row_picks_nothing(display: &str) {
    let stage = Stage::sized(720);
    stage.overlay.add_overlay(stage.chooser.widget());
    stage.chooser.focus();
    stage.chooser.show(Some(&folders(5)));
    pump();
    let rows = stage.rows();
    let at = |row: usize, dx: f32| {
        let bounds = rows[row].compute_bounds(&stage.window).unwrap();
        ((bounds.x() + dx) as i32, (bounds.y() + bounds.height() / 2.0) as i32)
    };
    let press = |from: (i32, i32), to: (i32, i32), hold: u64| {
        // Presses closer together than the double-click time count on from the last ones: let the last ones end.
        std::thread::sleep(Duration::from_millis(800));
        pump();
        let xid = xdo(
            display,
            &["search", "--onlyvisible", "--name", "^eitri-project-placement$"],
        );
        xdo(display, &["windowfocus", &xid]);
        let to_point = |(x, y): (i32, i32)| {
            xdo(
                display,
                &["mousemove", "--window", &xid, &x.to_string(), &y.to_string()],
            );
            pump();
        };
        to_point(from);
        xdo(display, &["mousedown", "1"]);
        pump();
        to_point(to);
        std::thread::sleep(Duration::from_millis(hold));
        pump();
        xdo(display, &["mouseup", "1"]);
        pump();
        picks_of(&stage)
    };
    assert_eq!(
        press(at(1, 8.0), at(3, 8.0), 0),
        Vec::<usize>::new(),
        "let go on another row"
    );
    assert_eq!(press(at(1, 8.0), at(1, 60.0), 0), vec![1], "moved inside its own row");
    assert_eq!(
        press(at(2, 8.0), at(2, 8.0), 700),
        vec![2],
        "held past the double-click time"
    );
}

/// A row takes a click anywhere in its highlight, the side padding of the list's own row included.
fn a_click_in_a_rows_padding_picks_it(display: &str) {
    // The theme's row padding (`.project-chooser-list > row`), which this harness does not load otherwise.
    let css = gtk4::CssProvider::new();
    css.load_from_string(".project-chooser-list > row { padding: 0 9px; }");
    let screen = gtk4::gdk::Display::default().unwrap();
    gtk4::style_context_add_provider_for_display(&screen, &css, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
    let stage = Stage::sized(720);
    stage.overlay.add_overlay(stage.chooser.widget());
    stage.chooser.focus();
    stage.chooser.show(Some(&folders(5)));
    pump();
    let row = stage.rows().into_iter().nth(2).unwrap();
    let inner = row.compute_bounds(&stage.window).unwrap();
    let outer = row.parent().unwrap().compute_bounds(&stage.window).unwrap();
    assert!(
        inner.x() - outer.x() >= 4.0,
        "the list's row pads ours: {} against {}",
        outer.x(),
        inner.x()
    );
    std::thread::sleep(Duration::from_millis(800));
    pump();
    stage.reports.borrow_mut().clear();
    click(
        display,
        &stage,
        (outer.x() + 2.0) as i32,
        (inner.y() + inner.height() / 2.0) as i32,
    );
    let picks = picks_of(&stage);
    gtk4::style_context_remove_provider_for_display(&screen, &css);
    assert_eq!(picks, vec![2], "a click in the padding picks the row");
}

/// The folder in a message is its own label and gives way at its start; the words before it are never cut.
fn the_path_label_ellipsizes_at_the_start_and_the_action_words_stay() {
    for width in [720, 400] {
        let stage = Stage::sized(width);
        stage.overlay.add_overlay(stage.chooser.widget());
        stage.chooser.focus();
        let mut stuck = folders(1);
        let deep = "/mnt/nas/".to_owned() + &"very-long-folder-name/".repeat(12);
        stuck.message = Some(format!("not answering · h goes back\t{deep}"));
        stage.chooser.show(Some(&stuck));
        pump();
        pump();
        let words = stage.labelled("project-chooser-message");
        let path = stage.labelled("project-chooser-message-path");
        assert_eq!(words.text(), "not answering · h goes back");
        assert!(!words.layout().is_ellipsized(), "{width}: the action words are whole");
        assert!(path.is_visible());
        assert_eq!(path.ellipsize(), gtk4::pango::EllipsizeMode::Start);
        assert!(path.layout().is_ellipsized(), "{width}: the folder gives way");
        assert!(
            path.text().ends_with("very-long-folder-name/\u{2069}"),
            "{}",
            path.text()
        );
        let panel = stage
            .widgets()
            .into_iter()
            .find(|w| w.has_css_class("project-chooser-panel"))
            .unwrap();
        assert!(panel.width() <= 640, "{width}: the panel is {} px", panel.width());
        assert!(path.compute_bounds(&panel).unwrap().x() > words.compute_bounds(&panel).unwrap().x());
        stuck.message = Some("Opening atlas…".into());
        stage.chooser.show(Some(&stuck));
        pump();
        assert!(!path.is_visible(), "a message with no folder is words alone");
    }
}

// Share the production copy so this measurement follows changes in either language.
mod onboarding {
    pub use eitri_core::onboarding::Lang;
}
#[allow(dead_code)]
#[path = "../../core/src/project_chooser/text.rs"]
mod chooser_text;

fn every_footer_fits_640_px_in_both_languages_with_the_longest_name() {
    use chooser_text::Enter;
    use onboarding::Lang;

    // Match the shipped panel's text metrics and padding without depending on its colour tokens. The copy comes from
    // `text.rs` itself (a path-included module, so it must keep to `core::` imports this shim provides), and the
    // numbers below restate `theme/gtk_css.rs`: they must move together.
    let css = gtk4::CssProvider::new();
    css.load_from_string(
        ".project-chooser-panel { padding: 12px; border: 1px solid; font-size: 13px; }
         .project-chooser-footer { font-size: 12px; padding: 0 9px; }",
    );
    let display = gtk4::gdk::Display::default().unwrap();
    gtk4::style_context_add_provider_for_display(&display, &css, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
    let stage = Stage::sized(720);
    stage.overlay.add_overlay(stage.chooser.widget());
    stage.chooser.focus();
    let mut failures = Vec::new();
    for lang in [Lang::En, Lang::ZhCn] {
        let copy = chooser_text::text(lang);
        let mut footers = vec![copy.footer_list.to_owned(), copy.footer.to_owned()];
        // Wide glyphs exercise the name cap in both scripts, including a name shortened in the middle.
        for name in ["W".repeat(24), "宽".repeat(24), "W".repeat(48)] {
            for (enter, in_line) in [
                (Enter::Open, false),
                (Enter::Into, false),
                (Enter::OpenFile, false),
                (Enter::Inert, false),
                (Enter::Into, true),
                (Enter::Open, true),
                (Enter::OpenFile, true),
                (Enter::Inert, true),
            ] {
                footers.push(chooser_text::footer(lang, &name, enter, in_line));
            }
        }
        for footer in footers {
            let mut view = folders(1);
            view.footer = footer.clone();
            stage.chooser.show(Some(&view));
            pump();
            let panel = stage
                .widgets()
                .into_iter()
                .find(|w| w.has_css_class("project-chooser-panel"))
                .unwrap();
            // `width()` is the content box: the 640 px request includes the 12 px padding and 1 px border on each side.
            assert_eq!(panel.width_request(), 640, "the measurement uses the full panel width");
            assert_eq!(panel.width(), 640 - 2 * 12 - 2, "the panel is laid out at its request");
            let label = stage.labelled("project-chooser-footer");
            // The rule is about the action words before the first `·`: the label ends in an ellipsis when the keys
            // after them do not fit, and that is fine as long as the action words and the ellipsis fit together.
            let action = footer.split('·').next().unwrap().trim_end();
            let action_width = label.create_pango_layout(Some(action)).pixel_size().0;
            let ellipsized = label.layout().is_ellipsized();
            let ellipsis_width = if ellipsized {
                label.create_pango_layout(Some("\u{2026}")).pixel_size().0
            } else {
                0
            };
            if action_width + ellipsis_width > label.width() {
                failures.push(format!(
                    "{footer:?}: action {action_width} px, label {} px, ellipsized {ellipsized}",
                    label.width()
                ));
            }
        }
    }
    gtk4::style_context_remove_provider_for_display(&display, &css);
    assert!(
        failures.is_empty(),
        "footers must fit and keep their action words whole:\n{}",
        failures.join("\n")
    );
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
        "every_footer_fits_640_px_in_both_languages_with_the_longest_name",
        every_footer_fits_640_px_in_both_languages_with_the_longest_name,
    );
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
    run(
        f,
        "a_2000_row_view_keeps_its_row_widgets_bounded",
        a_2000_row_view_keeps_its_row_widgets_bounded,
    );
    run(f, "a_double_click_reports_pick_twice_with_its_first_press_rev", || {
        a_double_click_reports_pick_twice_with_its_first_press_rev(d)
    });
    run(f, "a_press_let_go_off_its_row_picks_nothing", || {
        a_press_let_go_off_its_row_picks_nothing(d)
    });
    run(f, "a_click_in_a_rows_padding_picks_it", || {
        a_click_in_a_rows_padding_picks_it(d)
    });
    run(
        f,
        "the_path_label_ellipsizes_at_the_start_and_the_action_words_stay",
        the_path_label_ellipsizes_at_the_start_and_the_action_words_stay,
    );
}
