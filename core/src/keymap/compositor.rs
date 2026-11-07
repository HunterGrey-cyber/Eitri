//! What the prefix does in `compositor` mode, where the compositor arranges the windows and Eitri keeps the
//! keys that are not about layout. Two tables, one per kind of window: window 1 (the editor and an optional
//! bottom terminal) and a module window (a chat or a page). Both are exhaustive on purpose, with no catch-all
//! arm: a new action does not compile until someone decides what each window does with it.

use super::Action;
use crate::layout::{Direction, ModuleId, ModuleKind};

/// What window 1 does with one action in `compositor` mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubVerb {
    /// The same as in `eitri` mode.
    AsToday,
    /// Raise that module's window, or spawn it when it does not exist.
    RaiseOrSpawn(ModuleId),
    /// Spawn the next free chat window.
    NextChatWindow,
    /// Raise the most recently focused chat window (spawning the base one when none exists) and run the action
    /// there.
    ToLastChat,
    /// Layout the compositor owns: the first press in a window shows the hint, later ones are logged.
    Off,
}

/// What a module window does with one action in `compositor` mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleVerb {
    /// The same as in `eitri` mode.
    AsToday,
    /// A focus move past the window's edge, which the compositor does.
    EdgeToCompositor(Direction),
    /// Asks window 1 to raise or spawn the module's window.
    AskHub,
    /// The tab leaves for a chat window of its own, or joins the base one again.
    TabBreak,
    /// Does nothing but write a log line.
    LogOnly,
    /// Layout the compositor owns: the first press in a window shows the hint, later ones are logged.
    Off,
}

/// Sorts every `Action` into what window 1 does with it.
pub fn classify_window1(action: &Action) -> HubVerb {
    match action {
        Action::Select(_)
        | Action::SelectLast
        | Action::SelectNext
        | Action::SelectNumber
        | Action::Hint
        | Action::SendPrefix
        | Action::SendKeys(_)
        | Action::CopyMode { .. }
        | Action::Text(_)
        | Action::TextAll(_)
        | Action::WindowImmersive
        | Action::WindowSetup
        | Action::ModuleHide
        | Action::ModuleHideByKey
        | Action::ModuleKill
        | Action::ProjectChoose
        | Action::PluginLoad { .. }
        | Action::AgentsNext
        | Action::AgentsBack => HubVerb::AsToday,
        Action::Module(id) => match id.kind() {
            ModuleKind::Editor | ModuleKind::Terminal | ModuleKind::Canvas => HubVerb::AsToday,
            ModuleKind::Agent | ModuleKind::LuaWebview => HubVerb::RaiseOrSpawn(id.clone()),
        },
        Action::InstanceNew { kind } => match kind {
            ModuleKind::Agent => HubVerb::NextChatWindow,
            ModuleKind::Editor | ModuleKind::Canvas | ModuleKind::LuaWebview | ModuleKind::Terminal => HubVerb::Off,
        },
        Action::Tab(_) | Action::TabBreak | Action::PanelReload | Action::PanelKeymap | Action::PanelCommandLine => {
            HubVerb::ToLastChat
        }
        Action::Split { .. }
        | Action::Move(_)
        | Action::MoveFar(_)
        | Action::ContainerTabbed
        | Action::ContainerStacked
        | Action::FocusParent
        | Action::FocusChild
        | Action::ToggleSplit
        | Action::Rotate { .. }
        | Action::Flip(_)
        | Action::MainHorizontal
        | Action::MainVertical
        | Action::Tiled
        | Action::CycleLayout { .. }
        | Action::Spread
        | Action::Even(_)
        | Action::LayoutUndo
        | Action::MarkToggle
        | Action::MarkClear
        | Action::SwapMarked
        | Action::Zoom
        | Action::Resize { .. }
        | Action::Swap(_)
        | Action::FloatToggle
        | Action::FocusModeToggle
        | Action::ScratchpadShow { .. }
        | Action::ScratchpadMove
        | Action::WorkspaceStep { .. }
        | Action::WorkspaceLast
        | Action::WorkspaceNew
        | Action::WorkspaceSwitch { .. }
        | Action::WorkspaceChoose
        | Action::WorkspaceRename
        | Action::WindowNew
        | Action::WindowOther
        | Action::MoveToWindow { .. }
        | Action::WorkspaceToWindow { .. }
        | Action::WorkspaceSwapWindow => HubVerb::Off,
    }
}

/// Sorts every `Action` into what a module window (chat or page) does with it.
pub fn classify_module(action: &Action) -> ModuleVerb {
    match action {
        Action::Tab(_)
        | Action::PanelReload
        | Action::PanelKeymap
        | Action::PanelCommandLine
        | Action::Text(_)
        | Action::TextAll(_)
        | Action::Hint
        | Action::SendPrefix
        | Action::SendKeys(_)
        | Action::ModuleKill
        | Action::ProjectChoose
        | Action::PluginLoad { .. }
        | Action::AgentsNext
        | Action::AgentsBack => ModuleVerb::AsToday,
        Action::Select(dir) => ModuleVerb::EdgeToCompositor(*dir),
        Action::Module(id) => match id.kind() {
            ModuleKind::Editor => ModuleVerb::AsToday,
            ModuleKind::Agent | ModuleKind::Canvas | ModuleKind::LuaWebview | ModuleKind::Terminal => {
                ModuleVerb::AskHub
            }
        },
        Action::InstanceNew { kind } => match kind {
            ModuleKind::Agent => ModuleVerb::AskHub,
            ModuleKind::Editor | ModuleKind::Canvas | ModuleKind::LuaWebview | ModuleKind::Terminal => {
                ModuleVerb::LogOnly
            }
        },
        Action::TabBreak => ModuleVerb::TabBreak,
        Action::SelectLast
        | Action::SelectNext
        | Action::SelectNumber
        | Action::CopyMode { .. }
        | Action::WindowImmersive
        | Action::WindowSetup
        | Action::ModuleHide
        | Action::ModuleHideByKey => ModuleVerb::LogOnly,
        Action::Split { .. }
        | Action::Move(_)
        | Action::MoveFar(_)
        | Action::ContainerTabbed
        | Action::ContainerStacked
        | Action::FocusParent
        | Action::FocusChild
        | Action::ToggleSplit
        | Action::Rotate { .. }
        | Action::Flip(_)
        | Action::MainHorizontal
        | Action::MainVertical
        | Action::Tiled
        | Action::CycleLayout { .. }
        | Action::Spread
        | Action::Even(_)
        | Action::LayoutUndo
        | Action::MarkToggle
        | Action::MarkClear
        | Action::SwapMarked
        | Action::Zoom
        | Action::Resize { .. }
        | Action::Swap(_)
        | Action::FloatToggle
        | Action::FocusModeToggle
        | Action::ScratchpadShow { .. }
        | Action::ScratchpadMove
        | Action::WorkspaceStep { .. }
        | Action::WorkspaceLast
        | Action::WorkspaceNew
        | Action::WorkspaceSwitch { .. }
        | Action::WorkspaceChoose
        | Action::WorkspaceRename
        | Action::WindowNew
        | Action::WindowOther
        | Action::MoveToWindow { .. }
        | Action::WorkspaceToWindow { .. }
        | Action::WorkspaceSwapWindow => ModuleVerb::Off,
    }
}

/// The toast for the first press of an off key in a window: it names the compositor and the way back.
pub fn off_hint(compositor: &str) -> String {
    format!(
        "{compositor} arranges windows here: use its own keys. window.tiling = \"eitri\" brings Eitri's layout keys back."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_start_screen_is_window_1s_and_a_module_window_has_none() {
        assert_eq!(classify_window1(&Action::WindowSetup), HubVerb::AsToday);
        assert_eq!(classify_module(&Action::WindowSetup), ModuleVerb::LogOnly);
    }
    use crate::keymap::action::{SplitCwd, SwapTarget, TabAction, TextChange};
    use crate::keymap::KeySpec;
    use crate::layout::Axis;
    use crate::source_scan::{function_in, read_required, Source};
    use crate::wm::verbs::{family, Family};

    fn id(text: &str) -> ModuleId {
        ModuleId::parse(text).expect("a module name")
    }

    /// One action per variant (a few more where the table looks inside the payload).
    fn every_action() -> Vec<Action> {
        let key = KeySpec::parse("a").expect("a key");
        vec![
            Action::Move(Direction::Left),
            Action::ContainerTabbed,
            Action::ContainerStacked,
            Action::FocusParent,
            Action::FocusChild,
            Action::InstanceNew {
                kind: ModuleKind::Agent,
            },
            Action::InstanceNew {
                kind: ModuleKind::Terminal,
            },
            Action::InstanceNew {
                kind: ModuleKind::Editor,
            },
            Action::FloatToggle,
            Action::FocusModeToggle,
            Action::ScratchpadShow { module: None },
            Action::ScratchpadMove,
            Action::WorkspaceStep { forward: true },
            Action::WorkspaceLast,
            Action::ProjectChoose,
            Action::WorkspaceChoose,
            Action::WorkspaceRename,
            Action::WorkspaceSwitch { name: "w".to_string() },
            Action::WorkspaceNew,
            Action::TabBreak,
            Action::WindowNew,
            Action::WindowOther,
            Action::MoveToWindow {
                module: None,
                follow: false,
            },
            Action::WorkspaceToWindow { follow: false },
            Action::WorkspaceSwapWindow,
            Action::MoveFar(Direction::Right),
            Action::ToggleSplit,
            Action::Rotate { down: false },
            Action::Flip(Axis::Row),
            Action::MainHorizontal,
            Action::MainVertical,
            Action::Tiled,
            Action::CycleLayout { forward: true },
            Action::Spread,
            Action::LayoutUndo,
            Action::MarkToggle,
            Action::MarkClear,
            Action::SwapMarked,
            Action::SelectNumber,
            Action::SendPrefix,
            Action::SendKeys(key),
            Action::Split {
                axis: Axis::Row,
                cwd: SplitCwd::Start,
            },
            Action::Zoom,
            Action::Resize {
                dir: Direction::Up,
                cells: 1,
            },
            Action::Select(Direction::Down),
            Action::SelectLast,
            Action::SelectNext,
            Action::Swap(SwapTarget::Next),
            Action::Even(Axis::Column),
            Action::ModuleHide,
            Action::ModuleHideByKey,
            Action::ModuleKill,
            Action::Module(ModuleId::editor()),
            Action::Module(ModuleId::terminal()),
            Action::Module(id("terminal#2")),
            Action::Module(ModuleId::agent()),
            Action::Module(id("agent#3")),
            Action::Module(ModuleId::lua("notes")),
            Action::Tab(TabAction::New),
            Action::Hint,
            Action::PanelReload,
            Action::PanelKeymap,
            Action::PanelCommandLine,
            Action::WindowImmersive,
            Action::Text(TextChange::Larger),
            Action::CopyMode { up: false },
            Action::AgentsNext,
            Action::AgentsBack,
        ]
    }

    #[test]
    fn every_action_has_its_row_in_both_tables() {
        use HubVerb as H;
        use ModuleVerb as M;
        let key = KeySpec::parse("a").expect("a key");
        let hub = |action: Action, want: HubVerb| assert_eq!(classify_window1(&action), want, "window 1: {action:?}");
        let module = |action: Action, want: ModuleVerb| {
            assert_eq!(classify_module(&action), want, "module window: {action:?}");
        };
        hub(Action::ProjectChoose, H::AsToday);
        module(Action::ProjectChoose, M::AsToday);
        for dir in [Direction::Left, Direction::Down, Direction::Up, Direction::Right] {
            hub(Action::Select(dir), H::AsToday);
            module(Action::Select(dir), M::EdgeToCompositor(dir));
        }
        for action in [
            Action::SelectLast,
            Action::SelectNext,
            Action::SelectNumber,
            Action::CopyMode { up: false },
            Action::CopyMode { up: true },
            Action::WindowImmersive,
            Action::ModuleHide,
            Action::ModuleHideByKey,
        ] {
            hub(action.clone(), H::AsToday);
            module(action, M::LogOnly);
        }
        for action in [
            Action::Hint,
            Action::SendPrefix,
            Action::SendKeys(key),
            Action::Text(TextChange::Reset),
            Action::ModuleKill,
        ] {
            hub(action.clone(), H::AsToday);
            module(action, M::AsToday);
        }
        for action in [
            Action::Tab(TabAction::Select(2)),
            Action::PanelReload,
            Action::PanelKeymap,
            Action::PanelCommandLine,
        ] {
            hub(action.clone(), H::ToLastChat);
            module(action, M::AsToday);
        }
        hub(Action::TabBreak, H::ToLastChat);
        module(Action::TabBreak, M::TabBreak);
        // The round reads every window's rows, so it runs from window 1 and from any chat or page window.
        for action in [Action::AgentsNext, Action::AgentsBack] {
            hub(action.clone(), H::AsToday);
            module(action, M::AsToday);
        }

        hub(Action::Module(ModuleId::editor()), H::AsToday);
        hub(Action::Module(ModuleId::terminal()), H::AsToday);
        hub(Action::Module(id("terminal#2")), H::AsToday);
        for name in ["agent", "agent#4"] {
            hub(Action::Module(id(name)), H::RaiseOrSpawn(id(name)));
        }
        hub(
            Action::Module(ModuleId::lua("notes")),
            H::RaiseOrSpawn(ModuleId::lua("notes")),
        );
        module(Action::Module(ModuleId::editor()), M::AsToday);
        for other in [
            ModuleId::terminal(),
            ModuleId::agent(),
            id("agent#2"),
            ModuleId::lua("notes"),
        ] {
            module(Action::Module(other), M::AskHub);
        }

        hub(
            Action::InstanceNew {
                kind: ModuleKind::Agent,
            },
            H::NextChatWindow,
        );
        module(
            Action::InstanceNew {
                kind: ModuleKind::Agent,
            },
            M::AskHub,
        );
        for kind in [
            ModuleKind::Editor,
            ModuleKind::Canvas,
            ModuleKind::LuaWebview,
            ModuleKind::Terminal,
        ] {
            hub(Action::InstanceNew { kind }, H::Off);
            module(Action::InstanceNew { kind }, M::LogOnly);
        }

        let split = Action::Split {
            axis: Axis::Column,
            cwd: SplitCwd::Pane,
        };
        hub(split.clone(), H::Off);
        module(split, M::Off);

        // Every action of the four layout families the compositor owns is off in both windows.
        let mut off = 0;
        for action in every_action() {
            if matches!(
                family(&action),
                Family::Tiling | Family::Floats | Family::Workspaces | Family::Windows
            ) {
                hub(action.clone(), H::Off);
                module(action, M::Off);
                off += 1;
            }
        }
        assert!(
            off >= 30,
            "the list of actions covers the layout families ({off} found)"
        );
    }

    #[test]
    fn every_action_window_1_sends_to_a_chat_window_round_trips() {
        use crate::keymap::action::{parse, OptValue};
        let mut sent: Vec<Action> = every_action()
            .into_iter()
            .filter(|action| classify_window1(action) == HubVerb::ToLastChat)
            .collect();
        sent.push(Action::Tab(TabAction::Select(3)));
        assert!(sent.len() >= 5, "the list holds the chat verbs: {sent:?}");
        for action in sent {
            let opts: Vec<(String, OptValue)> = action
                .wire_options()
                .iter()
                .map(|(key, value)| (key.clone(), OptValue::from_wire(value)))
                .collect();
            let back = parse(&action.name(), &opts, &[]).expect("the name and options parse");
            assert_eq!(back.action, action, "{action:?} crosses the link by name and options");
        }
    }

    #[test]
    fn the_test_list_names_each_variant_of_the_groups_the_tables_split_on() {
        let actions = every_action();
        for family_wanted in [
            Family::Tiling,
            Family::Focus,
            Family::Keys,
            Family::Chrome,
            Family::Modules,
            Family::Project,
            Family::Workspaces,
            Family::Windows,
            Family::Floats,
            Family::Panel,
            Family::TextSize,
            Family::Agents,
        ] {
            assert!(
                actions.iter().any(|a| family(a) == family_wanted),
                "no action of {family_wanted:?} in the list"
            );
        }
    }

    /// Whether `body` (code with comments and literals blanked) has a `_ =>` arm.
    fn has_catch_all(body: &str) -> bool {
        body.match_indices("=>").any(|(at, _)| {
            let before = body[..at].trim_end();
            before.strip_suffix('_').is_some_and(|rest| {
                !rest
                    .bytes()
                    .next_back()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
            })
        })
    }

    #[test]
    fn neither_table_has_a_catch_all_arm() {
        let sources = [read_required("core/src/keymap/compositor.rs")];
        for name in ["classify_window1", "classify_module"] {
            let (_, body) = function_in(&sources, name);
            assert!(body.contains("match action"), "{name} is not the table: {body}");
            assert!(
                !has_catch_all(&body),
                "{name} must list every action, with no catch-all arm"
            );
        }

        // The scan sees a catch-all in a second file, and leaves a `_` inside a pattern alone.
        let other = [Source {
            path: "core/src/keymap/other.rs".to_string(),
            text: "fn classify_window1(a: &Action) -> u8 { match a { Action::Hint => 1, _ => 0 } }\n\
                   fn classify_module(a: &Action) -> u8 { match a { Action::Select(_) => 1, Action::Hint => 2 } }\n"
                .to_string(),
        }];
        assert!(has_catch_all(&function_in(&other, "classify_window1").1));
        assert!(!has_catch_all(&function_in(&other, "classify_module").1));
    }

    #[test]
    fn the_off_hint_names_the_compositor_and_the_way_back() {
        assert_eq!(
            off_hint("Hyprland"),
            "Hyprland arranges windows here: use its own keys. window.tiling = \"eitri\" brings Eitri's layout keys back."
        );
        assert!(off_hint("sway").starts_with("sway arranges windows here"));
    }
}
