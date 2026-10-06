//! The panel's assembly with a page that records what is sent to it: the size `agent.font_size` asks for
//! reaches the page as the theme's font size, and keeps reaching it when the editor's theme arrives and when
//! the prefix steps the text size.
//!
//! One test, because it owns the process: the panel writes its preferences and a presence lock under the
//! state home, and the account and settings-tier configuration are process-wide.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use eitri_core::keymap::prefix::PrefixCommand;
use eitri_core::keymap::{Action, TextChange};
use eitri_core::theme::payload::NvimThemePayload;
use eitri_mac::assembly::{assemble, HostActions};
use eitri_mac::editor::{Editor, EditorEvent};
use eitri_mac::keys::Pane;
use eitri_panel::panel_document::PanelDocument;
use eitri_panel::panel_page::{PageSurface, PanelHost, PanelPage};

static DOCUMENT: LazyLock<PanelDocument> = LazyLock::new(|| {
    PanelDocument::new("<!doctype html><html><head></head><body><script type=\"module\">1</script></body></html>")
});

struct NoLinks;
impl PanelHost for NoLinks {
    fn open_external(&self, _url: &str) {}
}

struct NoActions;
impl HostActions for NoActions {
    fn focus(&self, _pane: Pane) {}
    fn toast(&self, _text: &str) {}
    fn show_editor(&self) {}
    fn set_panel_hidden(&self, _hidden: bool) {}
}

/// Keeps every envelope the panel sent.
#[derive(Default)]
struct Recording(RefCell<Vec<String>>);
impl PageSurface for Recording {
    fn send(&self, envelope_json: &str) {
        self.0.borrow_mut().push(envelope_json.to_owned());
    }
    fn is_visible(&self) -> bool {
        true
    }
    fn load_document(&self, _html: &str, _base_uri: &str) {}
    fn set_background(&self, _rgb: [u8; 3]) {}
}

impl Recording {
    /// The font size of the newest theme envelope sent.
    fn theme_font_size(&self) -> Option<String> {
        self.0.borrow().iter().rev().find_map(|envelope| {
            let value: serde_json::Value = serde_json::from_str(envelope).ok()?;
            (value["kind"] == "theme").then(|| value["vars"]["--nv-font-size"].as_str().map(str::to_owned))?
        })
    }
}

#[test]
fn the_configured_font_size_reaches_the_page() {
    let tmp = tempfile::tempdir().expect("a scratch directory");
    let state_home = tmp.path().join("state");
    let config_dir = tmp.path().join("config");
    let project = tmp.path().join("project");
    for dir in [&state_home, &config_dir, &project] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::env::set_var("XDG_STATE_HOME", &state_home);
    std::env::remove_var("VERDANDI_CLAUDE_ACCOUNT");
    std::env::remove_var("EITRI_AGENT_BACKEND");
    std::fs::write(
        config_dir.join("init.lua"),
        "eitri.config.set(\"agent.font_size\", 15)\neitri.config.set(\"keymap.from_tmux\", \"off\")\n",
    )
    .unwrap();

    let surface = Rc::new(Recording::default());
    let editor = Rc::new(Editor::new(tmp.path().join("nvim-listen")).expect("an editor"));
    let assembly = assemble(
        &project,
        &config_dir,
        Some(Rc::new(PanelPage::new(surface.clone()))),
        Rc::new(NoLinks),
        &DOCUMENT,
        editor.clone(),
        Rc::new(NoActions),
    )
    .unwrap_or_else(|why| panic!("assemble: {why}"));

    assert_eq!(surface.theme_font_size().as_deref(), Some("15px"), "the start-up theme");

    // The editor's colourscheme replaces the tokens but not the size.
    let payload = NvimThemePayload::empty();
    assembly.handle(EditorEvent::Theme(payload));
    assert_eq!(
        surface.theme_font_size().as_deref(),
        Some("15px"),
        "after the editor's theme"
    );

    assembly.run_prefix(PrefixCommand::Run(Action::Text(TextChange::Larger)), Pane::Panel);
    assert_eq!(surface.theme_font_size().as_deref(), Some("16.5px"), "after a step");

    editor.shutdown();
    let mut watch = assembly.panel.shutdown();
    let deadline = Instant::now() + Duration::from_secs(10);
    while watch.step(Instant::now()) {
        assert!(Instant::now() < deadline, "the panel's shutdown never finished");
        std::thread::sleep(Duration::from_millis(20));
    }
}
