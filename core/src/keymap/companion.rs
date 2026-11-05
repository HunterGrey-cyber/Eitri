//! What the prefix does in a companion window. The window holds one agent panel and nothing to lay
//! out, so the prefix keeps what concerns the panel (tab verbs, reload, the key help, the `:` line,
//! text size, HINT, a literal prefix key, a move to the neighbouring window, and the round over the
//! waiting agents, whose stops include this panel's tabs) and refuses every other action with a single
//! toast per window. Page keys open pages beside the attached editor.
//! The tab chords are how a tmux user drives the tabs from the keyboard.

use super::prefix::Waiting;
use super::{Action, Keymap, TabAction, TextChange};
use crate::layout::{Direction, ModuleId, ModuleKeys, ModuleKind};
use crate::wm::agents_round::RoundKey;

/// One piece of the strip the armed prefix shows in the top bar, left to right.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripPiece {
    /// Where the next module goes, after `\`/`"`: `right of editor:`.
    Heading(String),
    /// A key and what it does, `e editor`; `dimmed` for a module that is hidden or not placed.
    Run { text: String, dimmed: bool },
    /// The `·` between two runs; never before the first, after the last, or after the heading.
    Dot,
}

/// The toast for an action a companion window does not have.
pub use crate::wm::feedback::BESIDE_OFF_TEXT;

/// What a companion window does with one prefix action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompanionVerb {
    Tab(TabAction),
    Reload,
    Keymap,
    CommandLine,
    Text(TextChange),
    Hint,
    /// `send-prefix` / `send-keys`: the key goes to the panel as typed.
    Literal,
    /// A move toward the window on that side, done by the window manager.
    Select(Direction),
    /// Open or raise the page beside the attached nvim.
    Page(ModuleId),
    /// `prefix u` / `prefix B`: the round, carried out by this window's agents layer.
    Round(RoundKey),
    /// Answered with [`BESIDE_OFF_TEXT`].
    Refuse,
}

/// Sorts every `Action` into what a companion window does with it. Exhaustive on purpose, with no
/// catch-all arm: a new action does not compile until someone decides what a window without a layout
/// does with it.
pub fn classify(action: &Action) -> CompanionVerb {
    match action {
        Action::Tab(tab) => CompanionVerb::Tab(*tab),
        Action::PanelReload => CompanionVerb::Reload,
        Action::PanelKeymap => CompanionVerb::Keymap,
        Action::PanelCommandLine => CompanionVerb::CommandLine,
        // One module, so one size: both scopes are the same action here.
        Action::Text(change) | Action::TextAll(change) => CompanionVerb::Text(*change),
        Action::Hint => CompanionVerb::Hint,
        Action::SendPrefix | Action::SendKeys(_) => CompanionVerb::Literal,
        Action::Select(dir) => CompanionVerb::Select(*dir),
        Action::Module(id) if id.kind() == ModuleKind::LuaWebview => CompanionVerb::Page(id.clone()),
        Action::ProjectChoose
        | Action::FloatToggle
        | Action::FocusModeToggle
        | Action::ScratchpadShow { .. }
        | Action::ScratchpadMove
        | Action::WorkspaceStep { .. }
        | Action::WorkspaceLast
        | Action::WorkspaceChoose
        | Action::WorkspaceRename
        | Action::WorkspaceSwitch { .. }
        | Action::WorkspaceNew
        | Action::TabBreak
        | Action::WindowNew
        | Action::WindowOther
        | Action::MoveToWindow { .. }
        | Action::WorkspaceToWindow { .. }
        | Action::WorkspaceSwapWindow
        | Action::SelectLast
        | Action::SelectNext
        | Action::Split { .. }
        | Action::CopyMode { .. }
        | Action::Zoom
        | Action::Resize { .. }
        | Action::Swap(_)
        | Action::Even(_)
        | Action::Module(_)
        | Action::ModuleHide
        | Action::ModuleHideByKey
        | Action::ModuleKill
        | Action::WindowImmersive
        | Action::WindowSetup
        | Action::Move(_)
        | Action::ContainerTabbed
        | Action::ContainerStacked
        | Action::FocusParent
        | Action::FocusChild
        | Action::InstanceNew { .. }
        | Action::MoveFar(_)
        | Action::ToggleSplit
        | Action::Rotate { .. }
        | Action::Flip(_)
        | Action::MainHorizontal
        | Action::MainVertical
        | Action::Tiled
        | Action::CycleLayout { .. }
        | Action::Spread
        | Action::LayoutUndo
        | Action::MarkToggle
        | Action::MarkClear
        | Action::SwapMarked
        | Action::SelectNumber => CompanionVerb::Refuse,
        Action::AgentsNext => CompanionVerb::Round(RoundKey::Next),
        Action::AgentsBack => CompanionVerb::Round(RoundKey::Back),
    }
}

/// Whether a companion window runs `action` for a root key: everything its prefix runs, including a move toward the
/// next window (the window manager moves the keys). A root key for any other action passes on to the text it was
/// typed in.
pub fn companion_root_keep(action: &Action) -> bool {
    !matches!(classify(action), CompanionVerb::Refuse)
}

/// Whether the window that holds the editor and the panel side by side runs `action` for a key: the companion verbs
/// except HINT (it answers HINT with a notice), the zoom (which hides the panel), and a move between the editor and
/// the panel, the only two things to move between.
pub fn beside_keep(action: &Action) -> bool {
    match action {
        Action::Zoom => true,
        Action::Select(direction) => matches!(direction, Direction::Left | Direction::Right),
        action => !matches!(classify(action), CompanionVerb::Refuse | CompanionVerb::Hint),
    }
}

/// What the armed prefix shows in a companion window: the keys that do something here, as the
/// effective keymap binds them. With no pages, module waits explain why nothing will happen.
pub fn strip_pieces(waiting: Waiting, keymap: &Keymap) -> Vec<StripPiece> {
    strip_pieces_with_pages(waiting, keymap, &ModuleKeys::built_in())
}

/// The strip with the page keys recorded from init.lua, in registration order.
pub fn strip_pieces_with_pages(waiting: Waiting, keymap: &Keymap, pages: &ModuleKeys) -> Vec<StripPiece> {
    match waiting {
        Waiting::No | Waiting::Number | Waiting::Workspace { .. } => Vec::new(),
        Waiting::Module(_) => {
            let mut pieces = Vec::new();
            for (key, id) in pages
                .entries()
                .iter()
                .filter(|(_, id)| id.kind() == ModuleKind::LuaWebview)
            {
                if !pieces.is_empty() {
                    pieces.push(StripPiece::Dot);
                }
                pieces.push(StripPiece::Run {
                    text: format!("{key} {}", id.as_str().trim_start_matches("lua:")),
                    dimmed: false,
                });
            }
            if pieces.is_empty() {
                pieces.push(StripPiece::Heading(BESIDE_OFF_TEXT.to_string()));
            }
            pieces
        }
        Waiting::Hide => vec![StripPiece::Heading(BESIDE_OFF_TEXT.to_string())],
        Waiting::Command => {
            let verbs = [
                (Action::Tab(TabAction::New), "new tab"),
                (Action::Tab(TabAction::Next), "next tab"),
                (Action::Tab(TabAction::Prev), "previous tab"),
                (Action::PanelReload, "reload"),
                (Action::PanelKeymap, "keys"),
                (Action::PanelCommandLine, "command"),
                (Action::Hint, "hint"),
            ];
            let mut pieces = Vec::new();
            for (action, label) in verbs {
                let keys: Vec<String> = keymap.keys_for(&action).iter().map(|key| key.to_string()).collect();
                if keys.is_empty() {
                    continue;
                }
                if !pieces.is_empty() {
                    pieces.push(StripPiece::Dot);
                }
                pieces.push(StripPiece::Run {
                    text: format!("{} {label}", keys.join(" ")),
                    dimmed: false,
                });
            }
            pieces
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{KeySpec, SwapTarget};
    use crate::layout::Axis;

    #[test]
    fn classify_maps_a_page_module_to_page() {
        let id = ModuleId::lua("notes");
        assert_eq!(classify(&Action::Module(id.clone())), CompanionVerb::Page(id));
        assert_eq!(classify(&Action::Module(ModuleId::editor())), CompanionVerb::Refuse);
    }

    #[test]
    fn strip_after_a_split_key_lists_page_keys() {
        let keymap = Keymap::defaults();
        let pages = ModuleKeys::build(&[(ModuleId::lua("notes"), Some("g".into()))], &keymap).unwrap();
        assert_eq!(
            strip_pieces_with_pages(Waiting::Module(Axis::Row), &keymap, &pages),
            vec![StripPiece::Run {
                text: "g notes".into(),
                dimmed: false
            }]
        );
    }

    /// One action of every family, so a new variant that is mapped wrongly shows up here as well as
    /// failing to compile in `classify`.
    #[test]
    fn classify_maps_every_action_family() {
        assert!(strip_pieces(Waiting::Workspace { deleting: false }, &Keymap::defaults()).is_empty());
        let key = KeySpec::char('x', false, false);
        let cases: Vec<(Action, CompanionVerb)> = vec![
            (Action::FloatToggle, CompanionVerb::Refuse),
            (Action::FocusModeToggle, CompanionVerb::Refuse),
            (Action::ScratchpadShow { module: None }, CompanionVerb::Refuse),
            (Action::ScratchpadMove, CompanionVerb::Refuse),
            (Action::WorkspaceStep { forward: false }, CompanionVerb::Refuse),
            (Action::WorkspaceStep { forward: true }, CompanionVerb::Refuse),
            (Action::WorkspaceLast, CompanionVerb::Refuse),
            (Action::ProjectChoose, CompanionVerb::Refuse),
            (Action::WorkspaceChoose, CompanionVerb::Refuse),
            (Action::WorkspaceRename, CompanionVerb::Refuse),
            (Action::WorkspaceSwitch { name: "x".into() }, CompanionVerb::Refuse),
            (Action::WorkspaceNew, CompanionVerb::Refuse),
            (Action::TabBreak, CompanionVerb::Refuse),
            (Action::WindowNew, CompanionVerb::Refuse),
            (Action::WindowOther, CompanionVerb::Refuse),
            (
                Action::MoveToWindow {
                    module: None,
                    follow: false,
                },
                CompanionVerb::Refuse,
            ),
            (Action::WorkspaceToWindow { follow: true }, CompanionVerb::Refuse),
            (Action::WorkspaceSwapWindow, CompanionVerb::Refuse),
            (Action::SelectNumber, CompanionVerb::Refuse),
            (Action::MoveFar(Direction::Left), CompanionVerb::Refuse),
            (Action::Zoom, CompanionVerb::Refuse),
            (
                Action::Resize {
                    dir: Direction::Left,
                    cells: 5,
                },
                CompanionVerb::Refuse,
            ),
            (
                Action::Select(Direction::Right),
                CompanionVerb::Select(Direction::Right),
            ),
            (Action::SelectLast, CompanionVerb::Refuse),
            (Action::SelectNext, CompanionVerb::Refuse),
            (Action::SendPrefix, CompanionVerb::Literal),
            (Action::SendKeys(key), CompanionVerb::Literal),
            (Action::Module(ModuleId::agent()), CompanionVerb::Refuse),
            (Action::ModuleHide, CompanionVerb::Refuse),
            (Action::ModuleKill, CompanionVerb::Refuse),
            (Action::Swap(SwapTarget::Next), CompanionVerb::Refuse),
            (Action::Even(Axis::Row), CompanionVerb::Refuse),
            (
                Action::Text(TextChange::Larger),
                CompanionVerb::Text(TextChange::Larger),
            ),
            (Action::Hint, CompanionVerb::Hint),
            (Action::PanelReload, CompanionVerb::Reload),
            (Action::PanelKeymap, CompanionVerb::Keymap),
            (Action::PanelCommandLine, CompanionVerb::CommandLine),
            (Action::WindowImmersive, CompanionVerb::Refuse),
            (Action::WindowSetup, CompanionVerb::Refuse),
            (Action::AgentsNext, CompanionVerb::Round(RoundKey::Next)),
            (Action::AgentsBack, CompanionVerb::Round(RoundKey::Back)),
            (
                Action::Tab(TabAction::Select(3)),
                CompanionVerb::Tab(TabAction::Select(3)),
            ),
            (
                Action::Split {
                    axis: Axis::Column,
                    cwd: crate::keymap::action::SplitCwd::Start,
                },
                CompanionVerb::Refuse,
            ),
            (Action::Move(Direction::Left), CompanionVerb::Refuse),
            (Action::ContainerTabbed, CompanionVerb::Refuse),
            (Action::ContainerStacked, CompanionVerb::Refuse),
            (Action::FocusParent, CompanionVerb::Refuse),
            (Action::FocusChild, CompanionVerb::Refuse),
            (
                Action::InstanceNew {
                    kind: crate::layout::ModuleKind::Terminal,
                },
                CompanionVerb::Refuse,
            ),
            (Action::CopyMode { up: true }, CompanionVerb::Refuse),
        ];
        for (action, want) in cases {
            assert_eq!(classify(&action), want, "{action:?}");
        }
    }

    /// The match has to stay exhaustive: a catch-all arm would send a new layout action to whatever
    /// it names without anyone deciding. Read from the source, as the other scanners here do, with
    /// the name split so the scan does not match itself.
    #[test]
    fn classify_has_no_catch_all_arm() {
        let source = include_str!("companion.rs");
        let start = source.find("pub fn classify").expect("classify is in this file");
        let body = &source[start..];
        let end = body.find("\n}\n").expect("classify ends");
        let wildcard = ["_", " =>"].concat();
        assert!(
            !body[..end].contains(&wildcard),
            "classify must list every action, with no catch-all arm"
        );
    }

    #[test]
    fn the_strip_names_the_panel_keys_and_the_refusal_after_a_split() {
        let keymap = Keymap::defaults();
        let pieces = strip_pieces(Waiting::Command, &keymap);
        let texts: Vec<&str> = pieces
            .iter()
            .filter_map(|piece| match piece {
                StripPiece::Run { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(texts.iter().any(|t| t.ends_with("reload")), "{texts:?}");
        assert!(texts.iter().any(|t| t.ends_with("new tab")), "{texts:?}");
        assert!(
            !texts.iter().any(|t| t.contains("kill") || t.contains("zoom")),
            "{texts:?}"
        );
        assert_eq!(strip_pieces(Waiting::No, &keymap), vec![]);
        assert_eq!(strip_pieces(Waiting::Number, &keymap), vec![]);
        assert_eq!(
            strip_pieces(Waiting::Module(Axis::Row), &keymap),
            vec![StripPiece::Heading(BESIDE_OFF_TEXT.to_string())]
        );
        assert_eq!(
            strip_pieces(Waiting::Hide, &keymap),
            vec![StripPiece::Heading(BESIDE_OFF_TEXT.to_string())],
            "a hide key waits as a split key does, with no layout to hide from"
        );
        assert_eq!(classify(&Action::ModuleHideByKey), CompanionVerb::Refuse);
    }
}
