//! The linked pair's verbs: `window.new`, `window.other`, `move.to-window`, `workspace.to-window` and
//! `workspace.swap-window`. None has a default key. The move itself is [`crate::wm::moves`]'s.

use super::super::feedback;
use super::super::host::*;
use super::super::moves;
use super::super::window::{WinId, Window};
use super::modules::Cx;
use crate::keymap::action::Action;

/// A window verb from the runner.
pub fn run(w: &mut Window, cx: &Cx, win: WinId, action: &Action, batch: &mut Batch) {
    match action {
        Action::WindowNew => {
            w.say("[window] window.new: the second window is started or raised".into());
            batch.push_effect(HostOp::OpenLinkedWindow { win });
        }
        Action::WindowOther => {
            if !moves::raise_other(w, win, batch) {
                refuse(w, win, action, batch);
            }
        }
        Action::MoveToWindow { module, follow } => {
            let layout = w.layout.clone();
            let module = match module {
                Some(module) => module.clone(),
                None => match layout.try_borrow() {
                    Ok(layout) => layout.focus().clone(),
                    Err(_) => {
                        w.warn("[wm] BUG: the layout was borrowed when move.to-window ran".into());
                        return;
                    }
                },
            };
            moves::offer(w, cx, win, &module, *follow, batch);
        }
        // The modules shown by a workspace move together, so their verdicts decide when its arrangement appears.
        Action::WorkspaceToWindow { follow } => moves::workspace::offer(w, cx, win, *follow, false, batch),
        Action::WorkspaceSwapWindow => moves::workspace::offer(w, cx, win, false, true, batch),
        _ => debug_assert!(false, "{action:?} is not a window verb"),
    }
}

fn refuse(w: &mut Window, win: WinId, action: &Action, batch: &mut Batch) {
    let text = feedback::no_other_window_text(w.role());
    w.say(format!("[window] {}: {text}", action.name()));
    w.refuse(win, text.to_owned(), batch);
}

/// The host could not start or raise the other window, or could not raise it for a focus: the toast says why.
pub fn refused(w: &mut Window, win: WinId, what: &OpTag, why: &str, batch: &mut Batch) {
    w.say(format!("[window] {what:?} refused: {why}"));
    w.refuse(win, why.to_owned(), batch);
}

#[cfg(test)]
mod tests {
    use super::super::rig::*;
    use crate::keymap::Action;
    use crate::layout::{Axis, Layout, ModuleId, Node};
    use crate::wm::*;

    fn three() -> Layout {
        Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::split(
                    Axis::Column,
                    0.5,
                    Node::Leaf(ModuleId::agent()),
                    Node::Leaf(ModuleId::terminal()),
                ),
            ),
            ModuleId::terminal(),
        )
        .unwrap()
    }

    #[test]
    fn window_verbs_refuse_with_todays_text_while_no_peer_is_attached() {
        for action in [
            Action::WindowOther,
            Action::MoveToWindow {
                module: None,
                follow: false,
            },
            Action::MoveToWindow {
                module: Some(ModuleId::terminal()),
                follow: true,
            },
            Action::WorkspaceToWindow { follow: false },
            Action::WorkspaceSwapWindow,
        ] {
            let mut r = rig(three());
            let before = r.said();
            let batch = r.run(action.clone());
            r.assert_refused(&batch, &before, crate::wm::feedback::NO_SECOND_WINDOW_TEXT);
        }
    }

    #[test]
    fn window_new_asks_the_host_for_the_second_window_with_or_without_a_peer() {
        let mut r = rig(three());
        let batch = r.run(Action::WindowNew);
        assert_eq!(ops(&batch), vec![HostOp::OpenLinkedWindow { win: WIN }]);
    }

    #[test]
    fn a_refused_start_of_the_second_window_toasts_the_hosts_reason() {
        let mut r = rig(three());
        let before = r.said();
        let batch = r.controller.handle(HostEvent::Refused {
            what: OpTag::OpenLinkedWindow,
            why: "this window cannot start a second window".into(),
            surface: None,
        });
        r.assert_refused(&batch, &before, "this window cannot start a second window");
    }
}
