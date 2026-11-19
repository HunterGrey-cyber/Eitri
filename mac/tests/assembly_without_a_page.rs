//! The panel's assembly with no page (the panel's inert mode): `init.lua` and its configuration reach the
//! panel, and the editor's events and the prefix's commands reach the host through `HostActions`.
//!
//! One test, because it owns the process: the panel writes its preferences and a presence lock under the
//! state home, and the account and settings-tier configuration are process-wide.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use eitri_core::keymap::prefix::PrefixCommand;
use eitri_core::keymap::{Action, TabAction};
use eitri_core::layout::{Direction, ModuleId};
use eitri_mac::assembly::{assemble, HostActions, NOT_IN_THIS_WINDOW, PANEL_NOT_SHOWN, PANEL_TOOK_NO_KEYS};
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
    Restarted,
}

/// What the host was asked to do. The first flag stands for a window too narrow to show the panel, the second
/// for AppKit refusing a view the keys, the third for an editor whose nvim ended.
#[derive(Default)]
struct Recorder(RefCell<Vec<Done>>, Cell<bool>, Cell<bool>, Cell<bool>);
impl Recorder {
    fn take(&self) -> Vec<Done> {
        std::mem::take(&mut *self.0.borrow_mut())
    }
    fn set_narrow(&self, narrow: bool) {
        self.1.set(narrow);
    }
    fn set_refusing(&self, refusing: bool) {
        self.2.set(refusing);
    }
    fn set_ended(&self, ended: bool) {
        self.3.set(ended);
    }
}
impl HostActions for Recorder {
    fn focus(&self, pane: Pane) -> bool {
        self.0.borrow_mut().push(Done::Focus(pane));
        !self.2.get()
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
    fn panel_visible(&self) -> bool {
        // Shown unless the window is narrow; the zoom's own state is the assembly's, and is what is tested.
        !self.1.get()
    }
    fn editor_ended(&self) -> bool {
        self.3.get()
    }
    fn restart_editor(&self) -> bool {
        if self.3.replace(false) {
            self.0.borrow_mut().push(Done::Restarted);
            true
        } else {
            false
        }
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

    // A window too narrow for the panel: nvim's navigator at its right edge, `prefix l`, a tab verb, the keymap
    // and the command line all leave the keys where they are and say why; the verbs that read the keys are not
    // run in a panel nobody can see.
    actions.set_narrow(true);
    assembly.handle(EditorEvent::PaneSwitch(Direction::Right));
    assert_eq!(actions.take(), vec![Done::Toast(PANEL_NOT_SHOWN.to_string())]);
    assembly.run_prefix(run(Action::Select(Direction::Right)), Pane::Editor);
    assert_eq!(actions.take(), vec![Done::Toast(PANEL_NOT_SHOWN.to_string())]);
    for action in [
        Action::Tab(TabAction::Close),
        Action::PanelKeymap,
        Action::PanelCommandLine,
    ] {
        assembly.run_prefix(run(action.clone()), Pane::Editor);
        assert_eq!(
            actions.take(),
            vec![Done::Toast(PANEL_NOT_SHOWN.to_string())],
            "{action:?}"
        );
    }
    // Hidden by the zoom as well: the zoom is undone as asked, and the keys still stay.
    assembly.run_prefix(run(Action::Zoom), Pane::Editor);
    assert_eq!(actions.take(), vec![Done::Hidden(true)]);
    assembly.handle(EditorEvent::PaneSwitch(Direction::Right));
    assert_eq!(
        actions.take(),
        vec![Done::Hidden(false), Done::Toast(PANEL_NOT_SHOWN.to_string())]
    );
    // The zoom asked to show a panel the width does not allow says so.
    assembly.run_prefix(run(Action::Zoom), Pane::Editor);
    assembly.run_prefix(run(Action::Zoom), Pane::Editor);
    assert_eq!(
        actions.take(),
        vec![
            Done::Hidden(true),
            Done::Hidden(false),
            Done::Toast(PANEL_NOT_SHOWN.to_string())
        ]
    );
    // Wide again: the keys go.
    actions.set_narrow(false);
    assembly.handle(EditorEvent::PaneSwitch(Direction::Right));
    assert_eq!(actions.take(), vec![Done::Focus(Pane::Panel)]);

    // On screen, but the move did not happen (AppKit refused the web view the keys): what was asked of the
    // keys is not done, and the window says so, as for a panel it cannot show.
    actions.set_refusing(true);
    for action in [
        Action::Select(Direction::Right),
        Action::Tab(TabAction::Close),
        Action::PanelKeymap,
        Action::PanelCommandLine,
    ] {
        assembly.run_prefix(run(action.clone()), Pane::Editor);
        assert_eq!(
            actions.take(),
            vec![Done::Focus(Pane::Panel), Done::Toast(PANEL_TOOK_NO_KEYS.to_string())],
            "{action:?}"
        );
    }
    assembly.handle(EditorEvent::PaneSwitch(Direction::Right));
    assert_eq!(
        actions.take(),
        vec![Done::Focus(Pane::Panel), Done::Toast(PANEL_TOOK_NO_KEYS.to_string())]
    );
    actions.set_refusing(false);

    // What this window has no place for is said so.
    assembly.run_prefix(run(Action::Hint), Pane::Panel);
    assert_eq!(actions.take(), vec![Done::Toast(NOT_IN_THIS_WINDOW.to_string())]);
    assembly.run_prefix(
        PrefixCommand::Place {
            cwd: eitri_core::keymap::action::SplitCwd::Start,
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

    // The editor's key gives the editor the keys while nvim runs, and starts nvim again once it has ended.
    assembly.run_prefix(run(Action::Module(ModuleId::editor())), Pane::Panel);
    assert_eq!(actions.take(), vec![Done::Focus(Pane::Editor)]);
    let defaults = eitri_core::keymap::Keymap::defaults();
    let ended = eitri_core::wm::feedback::editor_ended_text(&defaults);
    assert_eq!(ended, "nvim exited \u{2014} C-b e starts it again");
    assert_eq!(assembly.editor_ended_text(false), ended);
    assert_eq!(
        assembly.editor_ended_text(true),
        eitri_core::wm::feedback::editor_crashed_text(&defaults)
    );
    actions.set_ended(true);
    // nvim ended: the panel takes the keys (shown again from the zoom) and the notice says how to start it again.
    assembly.run_prefix(run(Action::Zoom), Pane::Editor);
    assert_eq!(
        actions.take(),
        vec![Done::Toast(ended.clone())],
        "no zoom while the editor has ended"
    );
    assembly.editor_ended(Some(true));
    assert_eq!(
        actions.take(),
        vec![
            Done::Focus(Pane::Panel),
            Done::Toast(eitri_core::wm::feedback::editor_crashed_text(&defaults))
        ]
    );
    assembly.editor_ended(Some(false));
    assert_eq!(actions.take(), vec![Done::Focus(Pane::Panel), Done::Toast(ended)]);
    assembly.editor_ended(None);
    assert_eq!(actions.take(), vec![Done::Focus(Pane::Panel)]);
    assembly.run_prefix(run(Action::Module(ModuleId::editor())), Pane::Panel);
    assert_eq!(actions.take(), vec![Done::Restarted]);
    // Running again: the zoom works as before.
    assembly.run_prefix(run(Action::Zoom), Pane::Editor);
    assembly.run_prefix(run(Action::Zoom), Pane::Editor);
    assert_eq!(actions.take(), vec![Done::Hidden(true), Done::Hidden(false)]);

    // A start that found unsaved changes in swap files says so, as the Linux window does.
    assembly.handle(EditorEvent::Recoverable {
        count: 2,
        newest: "src/a.rs".into(),
    });
    assert_eq!(
        actions.take(),
        vec![Done::Toast(eitri_core::wm::feedback::recoverable_text(2, "src/a.rs"))]
    );

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
