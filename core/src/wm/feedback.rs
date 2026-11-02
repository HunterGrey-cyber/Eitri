//! Text feedback for the tiling, float, focus, keys, chrome, module, window and panel verbs. For the tiling verbs,
//! the caller retains effects requiring values absent from [`VerbOutcome`]: successful preset cycling, mark
//! toggling, marked swaps and zoom changes.
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
use crate::layout::{LayoutError, ModuleId, ModuleKind};

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

/// The close paths distinguish a window refusal from the module kill's bare flash and an informational toast.
pub enum CloseOutcome<'a> {
    Refused(&'a str),
    KillRefused,
    Toast(&'a str),
}

#[derive(Debug, PartialEq, Eq)]
pub struct CloseFeedback {
    pub flash: bool,
    pub toast: Option<String>,
}

pub fn close(outcome: CloseOutcome<'_>) -> CloseFeedback {
    match outcome {
        CloseOutcome::Refused(text) => CloseFeedback {
            flash: true,
            toast: Some(text.into()),
        },
        CloseOutcome::KillRefused => CloseFeedback {
            flash: true,
            toast: None,
        },
        CloseOutcome::Toast(text) => CloseFeedback {
            flash: false,
            toast: Some(text.into()),
        },
    }
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

/// The float arms' feedback. Success lines needing the returned float change or focus destination stay
/// with the caller; the scratchpad's empty refusal also needs the keymap and is [`floats_empty`].
pub fn floats(action: &Action, target: &ModuleId, outcome: VerbOutcome<'_>) -> Option<Feedback> {
    match (action, outcome) {
        (Action::FloatToggle | Action::ScratchpadMove | Action::ScratchpadShow { .. }, VerbOutcome::Failed(err)) => {
            Some(Feedback::Refuse {
                log: format!("[modules] refused: {err}"),
                text: layout_refusal_text(err),
            })
        }
        (Action::Move(dir), VerbOutcome::Nothing) => Some(Feedback::Log(format!(
            "[modules] {target}: the float is against the {dir:?} edge"
        ))),
        (Action::FocusModeToggle, VerbOutcome::Nothing) => {
            let text = "focus.mode-toggle: nothing in view on the other layer";
            Some(Feedback::Refuse {
                log: format!("[modules] {text}"),
                text: text.into(),
            })
        }
        _ => None,
    }
}

/// Each scratchpad show reports its outcome; a missing instance is made by the module verbs next.
pub fn floats_show(change: &crate::layout::ScratchShow) -> Feedback {
    let line = match change {
        crate::layout::ScratchShow::Missing(id) => {
            format!("[modules] {id}: not made yet; the controller makes it for the scratchpad")
        }
        change => format!("[modules] scratchpad.show: {change:?}"),
    };
    Feedback::Log(line)
}

/// An empty scratchpad flashes and names the key that fills it in the toast.
pub fn floats_empty(keymap: &crate::keymap::Keymap) -> Feedback {
    let text = super::verbs::floats::scratch_empty_text(keymap);
    Feedback::Refuse {
        log: format!("[modules] refused: {text}"),
        text,
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

/// Feedback from the chrome's verbs. `window.immersive` always toggles and its line is its own, so nothing here
/// flashes or toasts.
pub fn chrome(action: &Action, outcome: VerbOutcome<'_>) -> Option<Feedback> {
    let _ = (action, outcome);
    None
}

/// What the answer gate and a covered chat's reveal say when the layout is being written as they ask, or the
/// window is not one the controller holds: the answer waits on its card, to be given again.
pub const LAYOUT_CHANGING_TEXT: &str = "the layout is changing: answer again";

/// A card answer while the project chooser covers the window; the answer waits on its card.
pub const PROJECT_CHOOSER_COVERS_TEXT: &str = "close the project chooser to answer";

/// A terminal key for a terminal this window's layout does not hold, and a card answer from a chat view it does not
/// hold.
pub fn not_in_window_text(id: &ModuleId) -> String {
    format!("{id}: not in this window")
}

/// A hide key's module key for a module that is hidden already.
pub fn already_hidden_text(id: &ModuleId) -> String {
    format!("{id} is hidden already")
}

/// A module key for a module this window has no host for (the canvas before it exists, a module of the other
/// window), and a kill of one.
pub fn not_here_yet_text(id: &ModuleId) -> String {
    format!("{id}: not in this window yet")
}

/// A split's new terminal or chat view whose neighbour left the layout before it could be placed beside it.
pub fn left_the_layout_text(beside: &ModuleId, noun: &str) -> String {
    format!("{beside} has left the layout; no {noun} was opened")
}

/// A new instance of a kind that has none: only terminals and chat views have numbered instances.
pub fn no_new_instance_text(kind: ModuleKind) -> String {
    format!("no new instance of {kind:?} can be made")
}

/// What a card answer, `prefix ?` or `prefix :` from a covered chat view says: `{chat} is under {float}: {key} from
/// {float} stows it` for a scratch float, which the scratchpad's key puts away, or `... sinks it` for another, which
/// only sinking clears. Both keys act on the float only while it holds the keys: pressed in the chat, where an
/// answer comes from, the scratchpad's key first moves the keys into a shown scratch float, and `float.toggle` would
/// float the chat itself. So the text says where to press it. `key` is how the keymap reaches that action.
pub fn covered_text(chat: &str, float: &str, key: &str, scratch: bool) -> String {
    let verb = if scratch { "stows" } else { "sinks" };
    format!("{chat} is under {float}: {key} from {float} {verb} it")
}

/// The editor is gone only from a window that never starts one (the pair's second window, whose editor is the first
/// window's), so its refusals say that rather than offering a restart nothing there can carry out; other modules keep
/// their own error text even if they are also marked gone.
pub fn layout_refusal_text(err: &LayoutError) -> String {
    gone_editor_toast_text(err).unwrap_or_else(|| err.to_string())
}

/// What the ended editor's toasts say where the cause is not known: nvim exited, and the key that starts it again.
/// It names no cause -- a text naming `:qa` or `prefix x` would be wrong after a close that was declined later.
/// A crash has its own text ([`editor_crashed_text`]). The key is the window's own (its prefix and the editor's module key), written in
/// tmux's notation (`C-b e`) as tmux's own messages name keys; with no key bound, the action's name.
pub fn editor_ended_text(keymap: &crate::keymap::Keymap) -> String {
    let key = super::verbs::key_text(keymap, &Action::Module(ModuleId::editor())).replace("Ctrl+", "C-");
    format!("nvim exited \u{2014} {key} starts it again")
}

/// nvim ended with nobody asking. A start says what can be recovered, so this points there rather than promise
/// anything survived.
pub fn editor_crashed_text(keymap: &crate::keymap::Keymap) -> String {
    let key = super::verbs::key_text(keymap, &Action::Module(ModuleId::editor())).replace("Ctrl+", "C-");
    format!("nvim stopped unexpectedly \u{2014} {key} starts it again and says what can be recovered")
}

/// What a start found in swap files, and nvim's own way to it: `:e` raises the swap prompt, whose `r` recovers
/// and `d` deletes. Nothing is recovered unless the person does that.
pub fn recoverable_text(count: u32, newest: &str) -> String {
    let files = match count {
        1 => newest.to_string(),
        count => format!("{count} files, newest {newest}"),
    };
    format!("unsaved changes kept for {files} \u{2014} :e the file, then r recovers them or d deletes them")
}

/// "1 file" or "N files": the reopen lines' counts.
fn file_count(count: u32) -> String {
    match count {
        1 => "1 file".to_string(),
        count => format!("{count} files"),
    }
}

/// Files the last session had open that are no longer there, named by the first of them the editor reported.
pub fn reopen_gone_text(count: u32, name: &str) -> String {
    let verb = if count == 1 { "is" } else { "are" };
    let more = match count.saturating_sub(1) {
        0 => String::new(),
        more => format!(" and {more} more"),
    };
    format!("{} from last time {verb} gone: {name}{more}", file_count(count))
}

/// A file the editor could not open again; the error's first line went to its stderr.
pub fn reopen_failed_text(name: &str) -> String {
    format!("could not reopen {name}")
}

/// [`reopen_gone_text`] before, or without, the first name: a name that could not travel costs only itself.
pub fn reopen_gone_count_text(count: u32) -> String {
    let verb = if count == 1 { "is" } else { "are" };
    format!("{} from last time {verb} gone", file_count(count))
}

/// [`reopen_failed_text`] before, or without, the first name.
pub fn reopen_failed_count_text(count: u32) -> String {
    format!("could not reopen {}", file_count(count))
}

/// Windows the screen had no room for: their files were listed as buffers instead.
pub fn reopen_squeezed_text(count: u32) -> String {
    let rest = if count == 1 { "it is" } else { "they are" };
    format!(
        "{} from last time did not fit on screen; {rest} in :ls",
        file_count(count)
    )
}

/// The record of the editor's files did not read back and was moved aside, so nothing was reopened.
pub fn reopen_set_aside_text() -> String {
    "the editor's last files could not be read; the file was set aside".to_string()
}

/// The last start's reopening never said it ended (a crash or a hang in it), so this start does not try again.
pub fn reopen_unfinished_text() -> String {
    "the editor's last files were not reopened: the last try did not finish; editor.restore = false turns reopening off"
        .to_string()
}

/// Requests must wait until the restarted nvim can receive them.
pub const EDITOR_NOT_RUNNING_TEXT: &str = "the editor is not running";

/// A restart's launch failed; the pane shows why over itself, so this says only that it failed and the key that
/// tries again, in the ended editor's own notation ([`editor_ended_text`]).
pub fn restart_failed_text(keymap: &crate::keymap::Keymap) -> String {
    let key = super::verbs::key_text(keymap, &Action::Module(ModuleId::editor())).replace("Ctrl+", "C-");
    format!("nvim could not start \u{2014} {key} tries again")
}

/// An editor that did not come up leaves the files that waited for it unopened, each client having been told they
/// would open.
pub fn opens_dropped_text(count: usize) -> String {
    match count {
        1 => "the editor did not start: the file was not opened".into(),
        count => format!("the editor did not start: {count} files were not opened"),
    }
}

/// An open with no answer in time, or an error. nvim's own text can run to a traceback; the toast takes its first
/// line. "Not yet", because a call waiting at a blocking prompt runs when the prompt ends.
pub fn open_failed_text(why: &str) -> String {
    match why.lines().next().filter(|line| !line.is_empty()) {
        Some(line) => format!("the file is not open yet: {line}"),
        None => "the file is not open yet".into(),
    }
}

/// The pane reports why it could not restart, so the next show can try again.
pub fn restart_refused_text(state: &str) -> String {
    format!("the editor could not start again: the pane is {state}")
}

fn gone_editor_toast_text(err: &LayoutError) -> Option<String> {
    match err {
        LayoutError::Gone(id) if *id == ModuleId::editor() => Some(NO_EDITOR_HERE_TEXT.into()),
        _ => None,
    }
}

/// Feedback from the chat's tab verbs, the same with the keys in the base view or in an extra one. A switch that
/// found no tab (a step or `last` with nowhere to go, a number no tab has), a key with nothing to do there (a step
/// over the canvas's revisions, closing the others from a view that holds one tab) and a step to a sibling or
/// terminal that is not there flash the app name alone: the `Refuse` has an empty line and an empty text, so nothing
/// is printed and the toast stays as it was. A step the lone terminal swallows says nothing. A verb that first shows
/// the base view to give it the keys, refused by the layout, says why on the toast, as every layout refusal does.
/// Every switch and every call made is silent here.
pub fn tabs(verb: &crate::tabs::TabVerb, outcome: VerbOutcome<'_>) -> Option<Feedback> {
    match outcome {
        VerbOutcome::Done => None,
        VerbOutcome::Nothing if matches!(verb, crate::tabs::TabVerb::Nothing) => None,
        VerbOutcome::Nothing => Some(Feedback::Refuse {
            log: String::new(),
            text: String::new(),
        }),
        VerbOutcome::Failed(err) => Some(Feedback::Refuse {
            log: format!("[modules] refused: {err}"),
            text: layout_refusal_text(err),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{chrome, focus, keys, layout_refusal_text, tabs, tiling, Feedback, VerbOutcome};
    use crate::keymap::{Action, SwapTarget};
    use crate::layout::{Axis, Direction, LayoutError, ModuleId};

    #[test]
    fn feedback_window_immersive_says_nothing_on_screen() {
        assert_eq!(chrome(&Action::WindowImmersive, VerbOutcome::Done), None);
        assert_eq!(chrome(&Action::WindowImmersive, VerbOutcome::Nothing), None);
    }

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
    fn the_ended_editors_text_uses_the_configured_prefix() {
        let keymap =
            crate::keymap::Keymap::apply_user(&[crate::keymap::KeymapOp::Prefix { key: "C-a".into() }], &[]).unwrap();
        assert_eq!(super::editor_ended_text(&keymap), "nvim exited — C-a e starts it again");
    }

    #[test]
    fn a_gone_editors_refusal_says_the_window_has_none() {
        let err = LayoutError::Gone(ModuleId::editor());
        let text = "this window has no editor: it is in the first window";
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
    #[test]
    fn a_terminal_key_for_a_terminal_the_window_lacks_says_so() {
        let id = crate::layout::ModuleId::parse("terminal#3").unwrap();
        assert_eq!(super::not_in_window_text(&id), "terminal#3: not in this window");
    }

    #[test]
    fn a_module_key_with_no_host_here_says_not_yet() {
        let canvas = crate::layout::ModuleId::parse("canvas").unwrap();
        assert_eq!(super::not_here_yet_text(&canvas), "canvas: not in this window yet");
    }

    #[test]
    fn a_split_whose_neighbour_left_names_it_and_what_was_not_opened() {
        let target = crate::layout::ModuleId::terminal();
        assert_eq!(
            super::left_the_layout_text(&target, "terminal"),
            "terminal has left the layout; no terminal was opened"
        );
        assert_eq!(
            super::left_the_layout_text(&target, "chat view"),
            "terminal has left the layout; no chat view was opened"
        );
    }

    #[test]
    fn a_kind_with_no_instances_has_no_new_instance() {
        assert_eq!(
            super::no_new_instance_text(crate::layout::ModuleKind::Editor),
            "no new instance of Editor can be made"
        );
    }

    #[test]
    fn a_covered_chat_names_the_float_and_the_key_that_clears_it() {
        assert_eq!(
            super::covered_text("agent", "notes", "C-b -", true),
            "agent is under notes: C-b - from notes stows it"
        );
        assert_eq!(
            super::covered_text("agent 2", "terminal", "C-b f", false),
            "agent 2 is under terminal: C-b f from terminal sinks it"
        );
    }

    #[test]
    fn an_answer_the_gate_cannot_judge_is_asked_again() {
        assert_eq!(super::LAYOUT_CHANGING_TEXT, "the layout is changing: answer again");
    }

    fn flash_alone() -> Option<Feedback> {
        Some(Feedback::Refuse {
            log: String::new(),
            text: String::new(),
        })
    }

    macro_rules! tab_nothing {
        ($($name:ident: $verb:expr => $expected:expr),* $(,)?) => {$(
            #[test]
            fn $name() {
                assert_eq!(tabs(&$verb, VerbOutcome::Nothing), $expected);
            }
        )*};
    }

    tab_nothing! {
        feedback_tab_step_with_no_other_tab_flashes_alone: crate::tabs::TabVerb::Step(1) => flash_alone(),
        feedback_tab_step_back_with_no_other_tab_flashes_alone: crate::tabs::TabVerb::Step(-1) => flash_alone(),
        feedback_tab_last_with_no_last_tab_flashes_alone: crate::tabs::TabVerb::Last => flash_alone(),
        feedback_tab_number_no_tab_has_flashes_alone: crate::tabs::TabVerb::Select(9) => flash_alone(),
        feedback_tab_key_with_nothing_to_do_there_flashes_alone: crate::tabs::TabVerb::Flash => flash_alone(),
        feedback_tab_sibling_step_with_no_sibling_flashes_alone: crate::tabs::TabVerb::Sibling(1) => flash_alone(),
        feedback_tab_terminal_step_with_no_terminal_flashes_alone: crate::tabs::TabVerb::Terminal(-1) => flash_alone(),
        feedback_tab_step_the_lone_terminal_swallows_says_nothing: crate::tabs::TabVerb::Nothing => None,
    }

    macro_rules! tab_failed {
        ($($name:ident: $verb:expr),* $(,)?) => {$(
            #[test]
            fn $name() {
                assert_eq!(
                    tabs(&$verb, VerbOutcome::Failed(&LayoutError::NoScratch)),
                    Some(Feedback::Refuse {
                        log: "[modules] refused: the scratchpad is empty".into(),
                        text: "the scratchpad is empty".into(),
                    })
                );
            }
        )*};
    }

    tab_failed! {
        feedback_tab_new_with_the_base_view_refused_says_why: crate::tabs::TabVerb::New,
        feedback_tab_rename_with_the_base_view_refused_says_why: crate::tabs::TabVerb::Rename,
        feedback_tab_close_with_the_base_view_refused_says_why: crate::tabs::TabVerb::Close,
        feedback_tab_close_others_with_the_base_view_refused_says_why: crate::tabs::TabVerb::CloseOthers,
        feedback_tab_chooser_with_the_base_view_refused_says_why: crate::tabs::TabVerb::Choose,
        feedback_tab_info_with_the_base_view_refused_says_why: crate::tabs::TabVerb::Info,
        feedback_tab_number_of_a_base_tab_with_the_base_view_refused_says_why: crate::tabs::TabVerb::Select(1),
    }

    #[test]
    fn feedback_tab_a_gone_editor_refusing_the_base_view_says_the_window_has_none() {
        assert_eq!(
            tabs(
                &crate::tabs::TabVerb::New,
                VerbOutcome::Failed(&LayoutError::Gone(ModuleId::editor()))
            ),
            Some(Feedback::Refuse {
                log: format!("[modules] refused: {}", LayoutError::Gone(ModuleId::editor())),
                text: super::NO_EDITOR_HERE_TEXT.into(),
            })
        );
    }

    #[test]
    fn feedback_tab_every_switch_and_call_made_is_silent() {
        use crate::tabs::TabVerb;
        for verb in [
            TabVerb::New,
            TabVerb::Step(1),
            TabVerb::Last,
            TabVerb::Select(1),
            TabVerb::Rename,
            TabVerb::Close,
            TabVerb::CloseOthers,
            TabVerb::Choose,
            TabVerb::Info,
            TabVerb::Sibling(1),
            TabVerb::Terminal(1),
        ] {
            assert_eq!(tabs(&verb, VerbOutcome::Done), None, "{verb:?} done says nothing");
        }
    }
}

/// A workspace verb's refusal: a step with only one workspace logs under the workspaces' own prefix, every other
/// refusal as the modules' refusals do.
pub fn workspaces(alone: bool, text: String) -> Feedback {
    let log = if alone {
        format!("[workspace] {text}")
    } else {
        format!("[modules] refused: {text}")
    };
    Feedback::Refuse { log, text }
}

// The window verbs' words.

/// An editor key in a window that neither owns the editor nor has a peer to ask.
pub const EDITOR_WINDOW_CLOSED_TEXT: &str = "the editor window is closed";

/// The editor key's question in a window 2 whose editor window is closed.
pub const START_EDITOR_PROMPT: &str = "start the editor? (y/n)";

/// What a window verb says while the half of the pair it needs is not there: no other window attached, a module
/// the other window holds asked for here, a workspace sent across, or a kind of module the other window cannot
/// take yet.
pub const NO_SECOND_WINDOW_TEXT: &str = "this window has no second window yet";
/// The same in a window 2 whose window 1 has gone: it is the editor window that is missing, and the editor key is the
/// way to start it again (running `eitri` on the project links a new one too).
pub const ORPHAN_TEXT: &str = "the editor window is closed: the editor key starts it again";

// Compositor mode's words.

/// What the next-chat-window key says when chat windows 2 to 9 are all started or starting.
pub const ALL_CHAT_WINDOWS_TEXT: &str = "all nine chat windows are open";

/// What the toast says when the module window `name` did not come up, for `why`.
pub fn module_window_failed_text(name: &crate::window_link::names::WindowName, why: &str) -> String {
    format!("window {} did not start: {why}", name.text())
}

/// What a module window says when its tab cannot be handed to another window: it holds no session yet.

/// What the toast says when `tab.break` finds the tab's turn running: the tab leaves once it ends.
pub const TAB_BREAK_WAITS_TEXT: &str = "the tab moves to its own window when this turn ends";

/// What the toast says when a second `tab.break` calls off the one waiting for its turn's end.
pub const TAB_BREAK_CANCELLED_TEXT: &str = "the tab stays here";

/// Why a hand-over is refused while another from the same window is under way.
pub const TAB_ALREADY_MOVING_TEXT: &str = "a tab is already moving";

/// Why a tab stays when a turn started between the ask and its release.
pub const A_TURN_STARTED_TEXT: &str = "a turn started";

/// What a window 1 in compositor mode says for a panel whose id no window can be named by: the id and the rule.
pub fn no_page_window_text(id: &str) -> String {
    format!("{id}: no page window for this id (letters, digits, . _ and -, at most 40, a letter or digit at both ends)")
}

/// The log line for a key the compositor owns, by the action's name.
pub fn off_in_compositor_mode_log(action: &str) -> String {
    format!("[keys] {action}: off in compositor mode")
}

/// What a window verb that needs the other window says while none is attached: in window 2, that its window 1 is
/// gone; anywhere else, that there is no second window yet.
pub fn no_other_window_text(role: &super::window::WindowRole) -> &'static str {
    use super::window::{LinkSide, WindowRole};
    if *role == (WindowRole::Linked { side: LinkSide::Second }) {
        ORPHAN_TEXT
    } else {
        NO_SECOND_WINDOW_TEXT
    }
}
/// A move toward another window of the same process: the pair is two processes, and nothing else was chosen.
pub const LOCAL_MOVE_TEXT: &str = "windows of one process were not chosen";
/// A move of the editor, which stays with the nvim it runs.
pub const EDITOR_MOVE_TEXT: &str = "the editor stays in the window that runs nvim";
/// A move of a module already on its way.
pub const MOVING_TEXT: &str = "the module is already moving to the other window";
/// A move of a module this window does not hold.
pub const NOT_HELD_TEXT: &str = "the module is not in this window";
/// The destination's answer to an offer of a page or Lua panel it holds itself: one name is held by one window.
pub const HELD_HERE_TEXT: &str = "the other window holds that module itself";
/// The destination's answer to an offer it cannot take as one: a number that is not the other window's, or one
/// already taken.
pub const NOT_AN_OFFER_TEXT: &str = "not an offer this window can take";

/// What the source says, once, when the other window has neither committed nor given back a move it was sent: the
/// module stays frozen. In window 1 the kill key is the way out, since window 1 started window 2 and can end it;
/// window 2 did not start window 1, so there the way out is window 1's own close.
pub fn not_answering_text(other: u8) -> String {
    if other == 1 {
        "window 1 is not answering; closing it gives the module back here".to_owned()
    } else {
        format!("window {other} is not answering; prefix x closes it")
    }
}

/// What a chat says when it is asked to move while its turn runs and its agent cannot keep a running turn across a
/// move: it goes once the turn ends.
pub fn moves_at_turn_end_text(other: u8) -> String {
    format!("moves to window {other} when this turn ends")
}

/// Why a chat stays when its workspace moves: its running turn cannot move live, and a workspace waits for no turn.
pub const TURN_RUNS_STAYS_TEXT: &str = "its turn is running and cannot move until it ends";

/// Why a moved chat's arrival was given up when this window's presence could not hold its running turn in time.
pub const PRESENCE_TOO_SLOW_TEXT: &str = "the moved chat's running turn could not be held here in time";

/// The kill key's question about a module frozen for a window that stopped answering: that window ends, not the
/// module, which the link's end gives back. What ends with it, as its last summary counted it, is named.
pub fn close_peer_text(other: u8, running: u32, queued: u32) -> String {
    let ends = match (running, queued) {
        (0, 0) => String::new(),
        (r, 0) => format!(" ({r} running end with it)"),
        (0, q) => format!(" ({q} queued end with it)"),
        (r, q) => format!(" ({r} running, {q} queued end with it)"),
    };
    format!("close window {other}{ends}? (y/n)")
}

#[cfg(test)]
mod window_tests {
    #[test]
    fn the_window_verbs_say_what_they_said_before_the_pair_was_wired() {
        assert_eq!(super::NO_SECOND_WINDOW_TEXT, "this window has no second window yet");
    }

    #[test]
    fn a_silent_destination_is_named_by_its_window_number() {
        assert_eq!(
            super::not_answering_text(2),
            "window 2 is not answering; prefix x closes it"
        );
        assert_eq!(
            super::not_answering_text(1),
            "window 1 is not answering; closing it gives the module back here"
        );
        assert_eq!(super::close_peer_text(2, 0, 0), "close window 2? (y/n)");
        assert_eq!(
            super::close_peer_text(2, 2, 3),
            "close window 2 (2 running, 3 queued end with it)? (y/n)"
        );
        assert_eq!(
            super::close_peer_text(2, 1, 0),
            "close window 2 (1 running end with it)? (y/n)"
        );
    }
}

#[cfg(test)]
mod float_tests {
    use super::*;
    use crate::layout::{Direction, ScratchShow};

    macro_rules! quiet {
        ($($name:ident: $action:expr),* $(,)?) => {$(
            #[test]
            fn $name() {
                assert_eq!(floats(&$action, &ModuleId::terminal(), VerbOutcome::Done), None);
            }
        )*};
    }
    quiet! {
        feedback_float_toggle_done: Action::FloatToggle,
        feedback_float_move_done: Action::Move(Direction::Left),
        feedback_focus_mode_toggle_done: Action::FocusModeToggle,
        feedback_scratchpad_move_done: Action::ScratchpadMove,
        feedback_scratchpad_show_done: Action::ScratchpadShow { module: None },
    }

    macro_rules! failed {
        ($($name:ident: $action:expr),* $(,)?) => {$(
            #[test]
            fn $name() {
                assert_eq!(floats(&$action, &ModuleId::editor(), VerbOutcome::Failed(&LayoutError::EditorNeverFloats)),
                    Some(Feedback::Refuse {
                        log: format!("[modules] refused: {}", LayoutError::EditorNeverFloats),
                        text: LayoutError::EditorNeverFloats.to_string(),
                    }));
            }
        )*};
    }
    failed! {
        feedback_float_toggle_failed: Action::FloatToggle,
        feedback_scratchpad_move_failed: Action::ScratchpadMove,
        feedback_scratchpad_show_failed: Action::ScratchpadShow { module: None },
    }

    #[test]
    fn feedback_float_move_nothing() {
        assert_eq!(
            floats(
                &Action::Move(Direction::Right),
                &ModuleId::terminal(),
                VerbOutcome::Nothing
            ),
            Some(Feedback::Log(
                "[modules] terminal: the float is against the Right edge".into()
            ))
        );
    }

    #[test]
    fn feedback_focus_mode_toggle_nothing() {
        let text = "focus.mode-toggle: nothing in view on the other layer";
        assert_eq!(
            floats(&Action::FocusModeToggle, &ModuleId::terminal(), VerbOutcome::Nothing),
            Some(Feedback::Refuse {
                log: format!("[modules] {text}"),
                text: text.into()
            })
        );
    }

    macro_rules! shown {
        ($($name:ident: $change:expr => $text:expr),* $(,)?) => {$(
            #[test]
            fn $name() {
                assert_eq!(floats_show(&$change), Feedback::Log($text.into()));
            }
        )*};
    }
    shown! {
        feedback_scratchpad_show_shown: ScratchShow::Shown(ModuleId::terminal()) =>
            "[modules] scratchpad.show: Shown(ModuleId(\"terminal\"))",
        feedback_scratchpad_show_raised: ScratchShow::Raised(ModuleId::terminal()) =>
            "[modules] scratchpad.show: Raised(ModuleId(\"terminal\"))",
        feedback_scratchpad_show_stowed: ScratchShow::Stowed(Some(ModuleId::editor())) =>
            "[modules] scratchpad.show: Stowed(Some(ModuleId(\"editor\")))",
        feedback_scratchpad_show_stowed_without_focus: ScratchShow::Stowed(None) =>
            "[modules] scratchpad.show: Stowed(None)",
        feedback_scratchpad_show_missing: ScratchShow::Missing(ModuleId::terminal()) =>
            "[modules] terminal: not made yet; the controller makes it for the scratchpad",
    }

    #[test]
    fn feedback_scratchpad_show_no_scratch() {
        let keymap = crate::keymap::Keymap::defaults();
        let text = format!(
            "the scratchpad is empty ({} sends a module there)",
            crate::wm::verbs::key_text(&keymap, &Action::ScratchpadMove)
        );
        assert_eq!(
            floats_empty(&keymap),
            Feedback::Refuse {
                log: format!("[modules] refused: {text}"),
                text
            }
        );
    }
}

// The panel verbs' and the panel events' words.

/// What a panel verb or a panel event says, as its arm or hook in the shell said it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PanelOutcome<'a> {
    /// The chat view a verb opens something in, a view the panel asked to show, or the editor a chat's request
    /// drives could not be put on screen: the layout's refusal.
    NotShown(&'a LayoutError),
    /// A float still covers the chat view the verb would open something in.
    Covered(&'a str),
    /// The base view's active tab cannot go out into a view of its own now: the panel's words.
    NoBreak(&'a str),
    /// `tab.break` from an extra view that holds no tab.
    NoTab(crate::tabs::ViewId),
    /// `tab.break` with the keys in no chat view.
    NoChatView(&'a Action),
    /// The panel refused to bring an extra view's tab back: its words.
    JoinRefused(&'a str),
    /// An extra view left with no tab is the only module on screen, and the base view could not be shown beside
    /// it, so the editor is tried.
    BaseNotShown {
        view: crate::tabs::ViewId,
        err: &'a LayoutError,
    },
    /// An extra view left with no tab is the only module on screen, and nothing could be shown beside it: it
    /// stays.
    AloneStays {
        view: crate::tabs::ViewId,
        err: &'a LayoutError,
    },
    /// An extra view left with no tab was removed from the window.
    Removed(crate::tabs::ViewId),
    /// An extra view left with no tab could not be removed.
    NotRemoved {
        view: crate::tabs::ViewId,
        err: &'a LayoutError,
    },
}

/// The panel's lines and refusals. A refusal by the layout says the layout's words on the toast; a refusal with its
/// own words says them; `tab.break` with the keys in no chat view logs under the prefix's own tag. What an empty
/// view's removal does is a stdout line alone.
pub fn panel(outcome: PanelOutcome<'_>) -> Feedback {
    let refuse = |text: String| Feedback::Refuse {
        log: format!("[modules] refused: {text}"),
        text,
    };
    match outcome {
        PanelOutcome::NotShown(err) => Feedback::Refuse {
            log: format!("[modules] refused: {err}"),
            text: layout_refusal_text(err),
        },
        PanelOutcome::Covered(text) | PanelOutcome::NoBreak(text) | PanelOutcome::JoinRefused(text) => {
            refuse(text.to_string())
        }
        PanelOutcome::NoTab(view) => refuse(format!("agent {} shows no tab", view.number())),
        PanelOutcome::NoChatView(action) => {
            let text = format!("{}: the keys are in no chat view", action.name());
            Feedback::Refuse {
                log: format!("[prefix] {text}"),
                text,
            }
        }
        PanelOutcome::BaseNotShown { view, err } => Feedback::Log(format!(
            "[chat] {}: the base view could not be shown ({err}); trying the editor",
            view.module()
        )),
        PanelOutcome::AloneStays { view, err } => Feedback::Log(format!(
            "[chat] {} holds no tab but is the only module on screen: {err}",
            view.module()
        )),
        PanelOutcome::Removed(view) => Feedback::Log(format!("[chat] {} holds no tab; removed", view.module())),
        PanelOutcome::NotRemoved { view, err } => Feedback::Log(format!(
            "[chat] {} holds no tab and could not be removed: {err}",
            view.module()
        )),
    }
}

/// What a panel event that could not be carried out says on stderr, with no flash and no toast: the page asked,
/// not the user.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PanelWarning<'a> {
    /// The editor a review opened a file in could not be shown.
    ReviewNotShown(&'a LayoutError),
    /// The keys could not go back to the chat after an edit in the editor ended.
    NoReturn(&'a LayoutError),
}

/// The stderr line of a [`PanelWarning`].
pub fn panel_warning(warning: PanelWarning<'_>) -> String {
    match warning {
        PanelWarning::ReviewNotShown(err) => format!("[review] could not show the editor: {err}"),
        PanelWarning::NoReturn(err) => format!("[scratch] could not return the keys to the chat: {err}"),
    }
}

/// What a chat's request to an editor in the other window of the pair is refused with here.
pub const NO_EDITOR_HERE_TEXT: &str = "this window has no editor: it is in the first window";
/// What a chat's request to an editor whose nvim does not run is refused with.
pub const EDITOR_NOT_READY_TEXT: &str = "the editor is not ready yet";

#[cfg(test)]
mod panel_tests {
    use super::*;
    use crate::tabs::ViewId;

    fn two() -> ViewId {
        ViewId::new(2).unwrap()
    }

    fn refusal(log: &str, text: &str) -> Feedback {
        Feedback::Refuse {
            log: log.into(),
            text: text.into(),
        }
    }

    #[test]
    fn feedback_panel_keymap_not_shown_says_the_layouts_words() {
        let err = LayoutError::NotInTree(ModuleId::agent());
        assert_eq!(
            panel(PanelOutcome::NotShown(&err)),
            refusal(&format!("[modules] refused: {err}"), &layout_refusal_text(&err))
        );
    }

    #[test]
    fn feedback_panel_keymap_covered_says_what_covers_the_chat() {
        assert_eq!(
            panel(PanelOutcome::Covered("x covers the chat")),
            refusal("[modules] refused: x covers the chat", "x covers the chat")
        );
    }

    #[test]
    fn feedback_tab_break_no_break_says_the_panels_words() {
        assert_eq!(
            panel(PanelOutcome::NoBreak("one tab left")),
            refusal("[modules] refused: one tab left", "one tab left")
        );
    }

    #[test]
    fn feedback_tab_break_no_tab_names_the_view() {
        assert_eq!(
            panel(PanelOutcome::NoTab(two())),
            refusal("[modules] refused: agent 2 shows no tab", "agent 2 shows no tab")
        );
    }

    #[test]
    fn feedback_tab_break_no_chat_view_logs_under_the_prefix() {
        assert_eq!(
            panel(PanelOutcome::NoChatView(&Action::TabBreak)),
            refusal(
                "[prefix] tab.break: the keys are in no chat view",
                "tab.break: the keys are in no chat view"
            )
        );
    }

    #[test]
    fn feedback_tab_break_join_refused_says_the_panels_words() {
        assert_eq!(
            panel(PanelOutcome::JoinRefused("the window is closing")),
            refusal("[modules] refused: the window is closing", "the window is closing")
        );
    }

    #[test]
    fn feedback_view_empty_base_not_shown_is_a_line_alone() {
        let err = LayoutError::NotInTree(ModuleId::agent());
        assert_eq!(
            panel(PanelOutcome::BaseNotShown { view: two(), err: &err }),
            Feedback::Log(format!(
                "[chat] {}: the base view could not be shown ({err}); trying the editor",
                two().module()
            ))
        );
    }

    #[test]
    fn feedback_view_empty_alone_stays_is_a_line_alone() {
        let err = LayoutError::NotInTree(ModuleId::editor());
        assert_eq!(
            panel(PanelOutcome::AloneStays { view: two(), err: &err }),
            Feedback::Log(format!(
                "[chat] {} holds no tab but is the only module on screen: {err}",
                two().module()
            ))
        );
    }

    #[test]
    fn feedback_view_empty_removed_is_a_line_alone() {
        assert_eq!(
            panel(PanelOutcome::Removed(two())),
            Feedback::Log(format!("[chat] {} holds no tab; removed", two().module()))
        );
    }

    #[test]
    fn feedback_view_empty_not_removed_is_a_line_alone() {
        let err = LayoutError::LastVisible(ModuleId::agent());
        assert_eq!(
            panel(PanelOutcome::NotRemoved { view: two(), err: &err }),
            Feedback::Log(format!(
                "[chat] {} holds no tab and could not be removed: {err}",
                two().module()
            ))
        );
    }

    #[test]
    fn feedback_review_open_not_shown_is_a_stderr_line() {
        let err = LayoutError::NotInTree(ModuleId::editor());
        assert_eq!(
            panel_warning(PanelWarning::ReviewNotShown(&err)),
            format!("[review] could not show the editor: {err}")
        );
    }

    #[test]
    fn feedback_editor_done_no_return_is_a_stderr_line() {
        let err = LayoutError::NotInTree(ModuleId::agent());
        assert_eq!(
            panel_warning(PanelWarning::NoReturn(&err)),
            format!("[scratch] could not return the keys to the chat: {err}")
        );
    }

    #[test]
    fn feedback_editor_request_refusals_say_what_the_shell_said() {
        assert_eq!(
            NO_EDITOR_HERE_TEXT,
            "this window has no editor: it is in the first window"
        );
        assert_eq!(EDITOR_NOT_READY_TEXT, "the editor is not ready yet");
    }
}
