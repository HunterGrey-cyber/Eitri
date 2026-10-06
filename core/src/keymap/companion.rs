//! What the prefix does in a companion window. The window holds one agent panel and nothing to lay
//! out, so the prefix keeps what concerns the panel (tab verbs, reload, the key help, the `:` line,
//! text size, HINT, a literal prefix key, and a move to the neighbouring window) and refuses every
//! other action with a toast. The tab chords are not layout, and they are how a tmux user drives the
//! tabs from the keyboard.

use super::prefix::Waiting;
use super::{Action, Keymap, TabAction, TextChange};
use crate::layout::Direction;

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
pub const REFUSED_TEXT: &str = "not in a companion window";

/// What a companion window does with one prefix action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Answered with [`REFUSED_TEXT`].
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
        Action::Text(change) => CompanionVerb::Text(*change),
        Action::Hint => CompanionVerb::Hint,
        Action::SendPrefix | Action::SendKeys(_) => CompanionVerb::Literal,
        Action::Select(dir) => CompanionVerb::Select(*dir),
        Action::SelectLast
        | Action::SelectNext
        | Action::Split(_)
        | Action::CopyMode { .. }
        | Action::Zoom
        | Action::Resize { .. }
        | Action::Swap(_)
        | Action::Even(_)
        | Action::Module(_)
        | Action::ModuleHide
        | Action::ModuleKill
        | Action::WindowImmersive => CompanionVerb::Refuse,
    }
}

/// What the armed prefix shows in a companion window: the keys that do something here, as the
/// effective keymap binds them. After a split key (the only way the prefix waits for a module key)
/// it says why nothing will happen.
pub fn strip_pieces(waiting: Waiting, keymap: &Keymap) -> Vec<StripPiece> {
    match waiting {
        Waiting::No => Vec::new(),
        Waiting::Module(_) => vec![StripPiece::Heading(REFUSED_TEXT.to_string())],
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
    use crate::layout::{Axis, ModuleId};

    /// One action of every family, so a new variant that is mapped wrongly shows up here as well as
    /// failing to compile in `classify`.
    #[test]
    fn classify_maps_every_action_family() {
        let key = KeySpec::char('x', false, false);
        let cases: Vec<(Action, CompanionVerb)> = vec![
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
            (
                Action::Tab(TabAction::Select(3)),
                CompanionVerb::Tab(TabAction::Select(3)),
            ),
            (Action::Split(Axis::Column), CompanionVerb::Refuse),
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
        assert_eq!(
            strip_pieces(Waiting::Module(Axis::Row), &keymap),
            vec![StripPiece::Heading(REFUSED_TEXT.to_string())]
        );
    }
}
