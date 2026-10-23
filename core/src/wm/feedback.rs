//! Text feedback for the tiling, focus and keys verbs. For the tiling verbs, the caller retains effects requiring
//! values absent from [`VerbOutcome`]: successful preset cycling, mark toggling, marked swaps and zoom changes.
//! Container no-ops also flash without a toast; their [`Feedback::Log`] carries only the line,
//! so the caller must retain that flash separately.
//!
//! The success lines retained by callers are `[modules] layout -> {preset:?}` for cycling,
//! `[modules] {target}: marked={marked}` for toggling a mark, and
//! `[modules] {target}: swapped with the marked {marked}` for a marked swap. Zoom's grid helper
//! prints `[module_grid] zoomed {target}` or `[module_grid] unzoomed`. None of these successes
//! flashes or toasts. Every other successful arm is silent; resizing and an unchanged zoom
//! are silent on no change as well.

use crate::keymap::{Action, SwapTarget};
use crate::layout::{LayoutError, ModuleId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Feedback {
    Log(String),
    /// The line to print and the refusal to flash and show in the toast.
    Refuse {
        log: String,
        text: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VerbOutcome<'a> {
    Done,
    /// The verb found nothing to change or select.
    Nothing,
    Failed(&'a LayoutError),
}

/// Feedback from the tiling arms, with `Move` referring only to a tiled module.
/// Success lines that need the returned preset, mark or zoom state remain with the caller.
pub fn tiling(action: &Action, target: &ModuleId, outcome: VerbOutcome<'_>) -> Option<Feedback> {
    match (action, outcome) {
        (
            Action::MoveFar(_)
            | Action::Even(_)
            | Action::MainHorizontal
            | Action::MainVertical
            | Action::Tiled
            | Action::CycleLayout { .. }
            | Action::LayoutUndo
            | Action::MarkToggle
            | Action::SwapMarked
            | Action::Move(_)
            | Action::ContainerTabbed
            | Action::ContainerStacked,
            VerbOutcome::Failed(err),
        ) => Some(Feedback::Refuse {
            log: format!("[modules] refused: {err}"),
            text: layout_refusal_text(err),
        }),
        (_, VerbOutcome::Done | VerbOutcome::Failed(_)) => None,
        (action, VerbOutcome::Nothing) => {
            let text = match action {
                Action::MoveFar(dir) => {
                    return Some(Feedback::Log(format!(
                        "[modules] {target}: already at the far {dir:?} edge"
                    )));
                }
                Action::Swap(SwapTarget::Toward(dir)) => {
                    return Some(Feedback::Log(format!(
                        "[modules] {target}: nothing to swap with {dir:?}"
                    )));
                }
                Action::Swap(_) => {
                    return Some(Feedback::Log(format!("[modules] {target}: nothing to swap with")));
                }
                Action::Move(dir) => {
                    return Some(Feedback::Log(format!("[modules] {target}: nowhere to move {dir:?}")));
                }
                Action::MarkClear => return Some(Feedback::Log("[modules] no module is marked".into())),
                Action::ContainerTabbed | Action::ContainerStacked => {
                    return Some(Feedback::Log(format!(
                        "[modules] {}: no container around {target}",
                        action.name()
                    )));
                }
                Action::ToggleSplit => "toggle-split: no split around the module on screen".into(),
                Action::Rotate { .. } => "rotate: fewer than two modules on screen".into(),
                Action::Flip(_) => "flip: no split that way on screen".into(),
                Action::Spread => "spread: no split around the module on screen".into(),
                Action::FocusParent | Action::FocusChild => format!("{}: nothing to select", action.name()),
                _ => return None,
            };
            Some(Feedback::Refuse {
                log: format!("[modules] {text}"),
                text,
            })
        }
    }
}

/// Feedback from the focus verbs. `select.last` with no module before the current one that can take the keys,
/// and `select.next` with no other module on screen, flash and toast; every landing is a log line its verb
/// prints itself, and `select.<dir>` with nothing that way is the navigation's own line.
pub fn focus(action: &Action, outcome: VerbOutcome<'_>) -> Option<Feedback> {
    let text = match (action, outcome) {
        (Action::SelectLast, VerbOutcome::Nothing) => "no last module",
        (Action::SelectNext, VerbOutcome::Nothing) => "no other module on screen",
        _ => return None,
    };
    Some(Feedback::Refuse {
        log: format!("[prefix] {text}"),
        text: text.into(),
    })
}

/// Feedback from the keys verbs. `select.number` with nothing on screen to number and `copy-mode` with the keys
/// outside a terminal flash and toast; a literal key with no module to take it, and HINT's toggle, say nothing.
pub fn keys(action: &Action, outcome: VerbOutcome<'_>) -> Option<Feedback> {
    let text = match (action, outcome) {
        (Action::SelectNumber, VerbOutcome::Nothing) => "select.number: no module on screen to number",
        (Action::CopyMode { .. }, VerbOutcome::Nothing) => "prefix [ scrolls the terminal",
        _ => return None,
    };
    Some(Feedback::Refuse {
        log: format!("[prefix] {text}"),
        text: text.into(),
    })
}

/// A killed editor cannot return in this window, so its toast explains how to get it back.
/// Other modules keep their own error text even if they are also marked gone.
pub fn layout_refusal_text(err: &LayoutError) -> String {
    gone_editor_toast_text(err).unwrap_or_else(|| err.to_string())
}

/// What a retired editor's refusals say, here and in the shell's own toasts: nvim ended -- a `prefix x`, a
/// `:qa` or a crash, which nothing here can tell apart -- and no second nvim starts in this window. A
/// relaunch shows the editor again, so the text says that without naming a cause it does not know: a text
/// naming `:qa` or `prefix x` would be wrong after a crash. One constant, so every refusal of the killed
/// editor reads the same whichever side decided it.
pub const RETIRED_EDITOR_TEXT: &str =
    "nvim exited and cannot restart in this window \u{2014} relaunch Eitri to get the editor back";

fn gone_editor_toast_text(err: &LayoutError) -> Option<String> {
    match err {
        LayoutError::Gone(id) if *id == ModuleId::editor() => Some(RETIRED_EDITOR_TEXT.into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{focus, keys, layout_refusal_text, tiling, Feedback, VerbOutcome};
    use crate::keymap::{Action, SwapTarget};
    use crate::layout::{Axis, Direction, LayoutError, ModuleId};

    macro_rules! silent_done {
        ($($name:ident: $action:expr),* $(,)?) => {$(
            #[test]
            fn $name() {
                assert_eq!(tiling(&$action, &ModuleId::editor(), VerbOutcome::Done), None);
            }
        )*};
    }

    silent_done! {
        feedback_move_far_done: Action::MoveFar(Direction::Left),
        feedback_toggle_split_done: Action::ToggleSplit,
        feedback_rotate_up_done: Action::Rotate { down: false },
        feedback_rotate_down_done: Action::Rotate { down: true },
        feedback_flip_horizontal_done: Action::Flip(Axis::Row),
        feedback_flip_vertical_done: Action::Flip(Axis::Column),
        feedback_spread_done: Action::Spread,
        feedback_even_horizontal_done: Action::Even(Axis::Row),
        feedback_even_vertical_done: Action::Even(Axis::Column),
        feedback_main_horizontal_done: Action::MainHorizontal,
        feedback_main_vertical_done: Action::MainVertical,
        feedback_tiled_done: Action::Tiled,
        feedback_layout_undo_done: Action::LayoutUndo,
        feedback_mark_clear_done: Action::MarkClear,
        feedback_swap_toward_done: Action::Swap(SwapTarget::Toward(Direction::Left)),
        feedback_swap_prev_done: Action::Swap(SwapTarget::Prev),
        feedback_swap_next_done: Action::Swap(SwapTarget::Next),
        feedback_move_done: Action::Move(Direction::Left),
        feedback_container_tabbed_done: Action::ContainerTabbed,
        feedback_container_stacked_done: Action::ContainerStacked,
        feedback_focus_parent_done: Action::FocusParent,
        feedback_focus_child_done: Action::FocusChild,
        feedback_layout_next_done_needs_preset: Action::CycleLayout { forward: true },
        feedback_layout_previous_done_needs_preset: Action::CycleLayout { forward: false },
        feedback_mark_toggle_done_needs_mark: Action::MarkToggle,
        feedback_swap_marked_done_needs_marked_module: Action::SwapMarked,
        feedback_zoom_done_needs_zoom_state: Action::Zoom,
        feedback_resize_done: Action::Resize { dir: Direction::Left, cells: 1 },
    }

    macro_rules! failed {
        ($($name:ident: $action:expr),* $(,)?) => {$(
            #[test]
            fn $name() {
                assert_eq!(
                    tiling(&$action, &ModuleId::editor(), VerbOutcome::Failed(&LayoutError::NoScratch)),
                    Some(Feedback::Refuse {
                        log: "[modules] refused: the scratchpad is empty".into(),
                        text: "the scratchpad is empty".into(),
                    })
                );
            }
        )*};
    }

    failed! {
        feedback_move_far_failed: Action::MoveFar(Direction::Left),
        feedback_even_horizontal_failed: Action::Even(Axis::Row),
        feedback_even_vertical_failed: Action::Even(Axis::Column),
        feedback_main_horizontal_failed: Action::MainHorizontal,
        feedback_main_vertical_failed: Action::MainVertical,
        feedback_tiled_failed: Action::Tiled,
        feedback_layout_next_failed: Action::CycleLayout { forward: true },
        feedback_layout_previous_failed: Action::CycleLayout { forward: false },
        feedback_layout_undo_failed: Action::LayoutUndo,
        feedback_mark_toggle_failed: Action::MarkToggle,
        feedback_swap_marked_failed: Action::SwapMarked,
        feedback_move_failed: Action::Move(Direction::Left),
        feedback_container_tabbed_failed: Action::ContainerTabbed,
        feedback_container_stacked_failed: Action::ContainerStacked,
    }

    macro_rules! nothing {
        ($($name:ident: $action:expr => $expected:expr),* $(,)?) => {$(
            #[test]
            fn $name() {
                assert_eq!(tiling(&$action, &ModuleId::editor(), VerbOutcome::Nothing), $expected);
            }
        )*};
    }

    fn refusal(text: &str) -> Option<Feedback> {
        Some(Feedback::Refuse {
            log: format!("[modules] {text}"),
            text: text.into(),
        })
    }

    nothing! {
        feedback_move_far_nothing_only_logs: Action::MoveFar(Direction::Left) =>
            Some(Feedback::Log("[modules] editor: already at the far Left edge".into())),
        feedback_toggle_split_nothing_refuses: Action::ToggleSplit =>
            refusal("toggle-split: no split around the module on screen"),
        feedback_rotate_up_nothing_refuses: Action::Rotate { down: false } =>
            refusal("rotate: fewer than two modules on screen"),
        feedback_rotate_down_nothing_refuses: Action::Rotate { down: true } =>
            refusal("rotate: fewer than two modules on screen"),
        feedback_flip_horizontal_nothing_refuses: Action::Flip(Axis::Row) =>
            refusal("flip: no split that way on screen"),
        feedback_flip_vertical_nothing_refuses: Action::Flip(Axis::Column) =>
            refusal("flip: no split that way on screen"),
        feedback_spread_nothing_refuses: Action::Spread =>
            refusal("spread: no split around the module on screen"),
        feedback_mark_clear_nothing_only_logs: Action::MarkClear =>
            Some(Feedback::Log("[modules] no module is marked".into())),
        feedback_swap_toward_nothing_only_logs: Action::Swap(SwapTarget::Toward(Direction::Left)) =>
            Some(Feedback::Log("[modules] editor: nothing to swap with Left".into())),
        feedback_swap_prev_nothing_only_logs: Action::Swap(SwapTarget::Prev) =>
            Some(Feedback::Log("[modules] editor: nothing to swap with".into())),
        feedback_swap_next_nothing_only_logs: Action::Swap(SwapTarget::Next) =>
            Some(Feedback::Log("[modules] editor: nothing to swap with".into())),
        feedback_move_nothing_only_logs: Action::Move(Direction::Left) =>
            Some(Feedback::Log("[modules] editor: nowhere to move Left".into())),
        feedback_container_tabbed_nothing_logs_caller_flashes: Action::ContainerTabbed =>
            Some(Feedback::Log("[modules] container.tabbed: no container around editor".into())),
        feedback_container_stacked_nothing_logs_caller_flashes: Action::ContainerStacked =>
            Some(Feedback::Log("[modules] container.stacked: no container around editor".into())),
        feedback_focus_parent_nothing_refuses: Action::FocusParent =>
            refusal("focus.parent: nothing to select"),
        feedback_focus_child_nothing_refuses: Action::FocusChild =>
            refusal("focus.child: nothing to select"),
        feedback_zoom_nothing: Action::Zoom => None,
        feedback_resize_nothing: Action::Resize { dir: Direction::Left, cells: 1 } => None,
    }

    #[test]
    fn a_killed_editors_refusal_says_how_to_get_it_back() {
        let err = LayoutError::Gone(ModuleId::editor());
        let text = "nvim exited and cannot restart in this window — relaunch Eitri to get the editor back";
        assert_eq!(layout_refusal_text(&err), text);
        assert_eq!(
            tiling(
                &Action::MoveFar(Direction::Left),
                &ModuleId::editor(),
                VerbOutcome::Failed(&err)
            ),
            Some(Feedback::Refuse {
                log: "[modules] refused: 'editor' was closed and cannot be reopened in this window".into(),
                text: text.into(),
            })
        );
    }

    #[test]
    fn refuse_toasts_every_layout_error() {
        for err in [
            LayoutError::LastVisible(ModuleId::editor()),
            LayoutError::NoScratch,
            LayoutError::NothingToUndo,
            LayoutError::Gone(ModuleId::agent()),
        ] {
            assert_eq!(layout_refusal_text(&err), err.to_string());
        }
    }

    #[test]
    fn feedback_other_family_failed_is_silent() {
        assert_eq!(
            tiling(
                &Action::WindowNew,
                &ModuleId::editor(),
                VerbOutcome::Failed(&LayoutError::NoScratch)
            ),
            None
        );
    }

    #[test]
    fn select_last_with_no_module_before_refuses_with_its_text() {
        assert_eq!(
            focus(&Action::SelectLast, VerbOutcome::Nothing),
            Some(Feedback::Refuse {
                log: "[prefix] no last module".into(),
                text: "no last module".into(),
            })
        );
        assert_eq!(focus(&Action::SelectLast, VerbOutcome::Done), None);
    }

    #[test]
    fn select_next_with_one_module_on_screen_refuses_with_its_text() {
        assert_eq!(
            focus(&Action::SelectNext, VerbOutcome::Nothing),
            Some(Feedback::Refuse {
                log: "[prefix] no other module on screen".into(),
                text: "no other module on screen".into(),
            })
        );
        assert_eq!(focus(&Action::SelectNext, VerbOutcome::Done), None);
    }

    #[test]
    fn select_number_with_nothing_on_screen_refuses_with_its_text() {
        assert_eq!(
            keys(&Action::SelectNumber, VerbOutcome::Nothing),
            Some(Feedback::Refuse {
                log: "[prefix] select.number: no module on screen to number".into(),
                text: "select.number: no module on screen to number".into(),
            })
        );
        assert_eq!(keys(&Action::SelectNumber, VerbOutcome::Done), None);
    }

    #[test]
    fn copy_mode_outside_a_terminal_refuses_with_its_text() {
        for up in [false, true] {
            assert_eq!(
                keys(&Action::CopyMode { up }, VerbOutcome::Nothing),
                Some(Feedback::Refuse {
                    log: "[prefix] prefix [ scrolls the terminal".into(),
                    text: "prefix [ scrolls the terminal".into(),
                })
            );
            assert_eq!(keys(&Action::CopyMode { up }, VerbOutcome::Done), None);
        }
    }

    #[test]
    fn a_literal_key_with_no_module_to_take_it_says_nothing() {
        for action in [
            Action::SendPrefix,
            Action::SendKeys(crate::keymap::KeySpec::char('a', true, false)),
        ] {
            for outcome in [VerbOutcome::Done, VerbOutcome::Nothing] {
                assert_eq!(keys(&action, outcome), None, "{action:?}");
            }
        }
    }

    #[test]
    fn hints_toggle_has_no_feedback_of_its_own() {
        for outcome in [VerbOutcome::Done, VerbOutcome::Nothing] {
            assert_eq!(keys(&Action::Hint, outcome), None);
        }
    }

    #[test]
    fn select_toward_a_direction_has_no_feedback_of_its_own() {
        for outcome in [VerbOutcome::Done, VerbOutcome::Nothing] {
            assert_eq!(focus(&Action::Select(Direction::Left), outcome), None);
        }
    }
}
