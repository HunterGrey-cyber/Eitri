//! The panel's assembly with no page (the panel's inert mode): `init.lua` and its configuration reach the
//! panel, and the editor's events and the prefix's commands reach the host through `HostActions`.
//!
//! One test, because it owns the process: the panel writes its preferences and a presence lock under the
//! state home, and the account and settings-tier configuration are process-wide.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use eitri_core::keymap::prefix::PrefixCommand;
use eitri_core::keymap::{Action, TabAction};
use eitri_core::layout::Direction;
use eitri_mac::assembly::{assemble, HostActions, NOT_IN_THIS_WINDOW};
use eitri_mac::editor::{Editor, EditorEvent};
use eitri_mac::keys::Pane;
use eitri_panel::panel_document::PanelDocument;
use eitri_panel::panel_page::PanelHost;

static DOCUMENT: LazyLock<PanelDocument> = LazyLock::new(|| {
    PanelDocument::new("<!doctype html><html><head></head><body><script type=\"module\">1</script></body></html>")
});

struct NoLinks;
impl PanelHost for NoLinks {
    fn open_external(&self, _url: &str) {}
}

#[derive(Debug, PartialEq, Eq)]
enum Done {
    Focus(Pane),
    Toast(String),
    ShowEditor,
    Hidden(bool),
}

#[derive(Default)]
struct Recorder(RefCell<Vec<Done>>);
impl Recorder {
    fn take(&self) -> Vec<Done> {
        std::mem::take(&mut *self.0.borrow_mut())
    }
}
impl HostActions for Recorder {
    fn focus(&self, pane: Pane) {
        self.0.borrow_mut().push(Done::Focus(pane));
    }
    fn toast(&self, text: &str) {
        self.0.borrow_mut().push(Done::Toast(text.to_string()));
    }
    fn show_editor(&self) {
        self.0.borrow_mut().push(Done::ShowEditor);
    }
    fn set_panel_hidden(&self, hidden: bool) {
        self.0.borrow_mut().push(Done::Hidden(hidden));
    }
}

fn run(command: Action) -> PrefixCommand {
    PrefixCommand::Run(command)
}

#[test]
fn the_configuration_and_the_events_reach_the_panel_and_the_host() {
    let tmp = tempfile::tempdir().expect("a scratch directory");
    let state_home = tmp.path().join("state");
    let config_dir = tmp.path().join("config");
    let project: PathBuf = tmp.path().join("project");
    for dir in [&state_home, &config_dir, &project] {
        std::fs::create_dir_all(dir).unwrap();
    }
    // Before anything reads them: the panel's state files, and the account and backend choices.
    std::env::set_var("XDG_STATE_HOME", &state_home);
    std::env::remove_var("VERDANDI_CLAUDE_ACCOUNT");
    std::env::remove_var("EITRI_AGENT_BACKEND");
    // Without this the developer's own tmux prefix would be imported.
    std::fs::write(
        config_dir.join("init.lua"),
        "eitri.config.set(\"agent.font_size\", 15)\neitri.config.set(\"keymap.from_tmux\", \"off\")\n",
    )
    .unwrap();

    let editor = Rc::new(Editor::new(tmp.path().join("nvim-listen")).expect("an editor"));
    let actions = Rc::new(Recorder::default());
    let assembly = assemble(
        &project,
        &config_dir,
        None,
        Rc::new(NoLinks),
        &DOCUMENT,
        editor.clone(),
        actions.clone(),
    )
    .unwrap_or_else(|why| panic!("assemble: {why}"));

    assert_eq!(assembly.config.panel_font_size, 15.0);
    assert_eq!(assembly.panel_font_size_px(), 15.0);
    assert!(actions.take().is_empty(), "building the panel asks nothing of the host");

    // nvim's navigator ran out of windows to the right: the keys go to the panel; to the left, nowhere.
    assembly.handle(EditorEvent::PaneSwitch(Direction::Right));
    assert_eq!(actions.take(), vec![Done::Focus(Pane::Panel)]);
    assembly.handle(EditorEvent::PaneSwitch(Direction::Left));
    assert_eq!(actions.take(), vec![]);

    // The zoom hides the panel and shows it again.
    assembly.run_prefix(run(Action::Zoom), Pane::Editor);
    assembly.run_prefix(run(Action::Zoom), Pane::Editor);
    assert_eq!(actions.take(), vec![Done::Hidden(true), Done::Hidden(false)]);

    // A tab verb takes the keys, which unhides a hidden panel first.
    assembly.run_prefix(run(Action::Zoom), Pane::Editor);
    actions.take();
    assembly.run_prefix(run(Action::Tab(TabAction::New)), Pane::Editor);
    assert_eq!(actions.take(), vec![Done::Hidden(false), Done::Focus(Pane::Panel)]);

    // What this window has no place for is said so.
    assembly.run_prefix(run(Action::Hint), Pane::Panel);
    assert_eq!(actions.take(), vec![Done::Toast(NOT_IN_THIS_WINDOW.to_string())]);
    assembly.run_prefix(
        PrefixCommand::Place {
            module: eitri_core::layout::ModuleId::agent(),
            axis: eitri_core::layout::Axis::Row,
        },
        Pane::Panel,
    );
    assert_eq!(actions.take(), vec![Done::Toast(NOT_IN_THIS_WINDOW.to_string())]);

    // `prefix h` from the panel moves toward the editor; from the editor there is nothing left of it.
    assembly.run_prefix(run(Action::Select(Direction::Left)), Pane::Panel);
    assert_eq!(actions.take(), vec![Done::Focus(Pane::Editor)]);
    assembly.run_prefix(run(Action::Select(Direction::Left)), Pane::Editor);
    assert_eq!(actions.take(), vec![]);

    // `prefix` then the send-prefix key from the editor hands the prefix chord to nvim; with no editor
    // attached the reason reaches the user instead of the key vanishing.
    assembly.run_prefix(run(Action::SendPrefix), Pane::Editor);
    match actions.take().as_slice() {
        [Done::Toast(why)] => assert!(why.contains("not connected"), "{why}"),
        other => panic!("expected the one toast, got {other:?}"),
    }

    // The text size steps the panel alone.
    assembly.run_prefix(run(Action::Text(eitri_core::keymap::TextChange::Larger)), Pane::Panel);
    assert!((assembly.panel_font_size_px() - 16.5).abs() < 1e-4);

    editor.shutdown();
    let mut watch = assembly.panel.shutdown();
    let deadline = Instant::now() + Duration::from_secs(10);
    while watch.step(Instant::now()) {
        assert!(Instant::now() < deadline, "the panel's shutdown never finished");
        std::thread::sleep(Duration::from_millis(20));
    }
}
