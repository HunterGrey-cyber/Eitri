//! The focus verbs: `select.<dir>` from the module that last held the keys, `select.last` (tmux's `last-pane`)
//! and `select.next` (`select-pane -t :.+`), each landing the way `Ctrl+h/j/k/l` does, and each refusal with the
//! words the shell's arms gave.

use super::super::feedback::{self, Feedback, VerbOutcome};
use super::super::focus::{land_by_key, move_focus};
use super::super::host::{Batch, HostOp, HostReads};
use super::super::window::{WinId, Window};
use crate::keymap::Action;
use crate::layout::{next_on_screen, ModuleId};

/// Runs the focus verb `action` on `w` and adds what follows to `batch`.
pub fn run(w: &mut Window, reads: &dyn HostReads, win: WinId, action: &Action, batch: &mut Batch) {
    let Ok(focus) = w.layout.try_borrow().map(|layout| layout.focus().clone()) else {
        w.warn(format!("[wm] BUG: the layout was borrowed when {} ran", action.name()));
        return;
    };
    let to = match action {
        Action::Select(direction) => {
            move_focus(w, reads, win, &focus, *direction, batch);
            return;
        }
        Action::SelectLast => {
            let previous = w.focus.history.previous().cloned();
            previous.filter(|id| w.layout.try_borrow().is_ok_and(|layout| layout.can_focus(id).is_ok()))
        }
        Action::SelectNext => w
            .layout
            .try_borrow()
            .ok()
            .and_then(|layout| next_on_screen(&layout, &focus)),
        other => {
            debug_assert!(false, "{other:?} is not a focus verb");
            return;
        }
    };
    match to {
        Some(to) => land(w, win, action, &to, batch),
        None => refuse(w, win, action, batch),
    }
}

fn land(w: &mut Window, win: WinId, action: &Action, to: &ModuleId, batch: &mut Batch) {
    let asked = land_by_key(w, win, to, batch);
    w.say(format!("[prefix] {} -> {to} (focus asked={asked})", action.name()));
}

fn refuse(w: &mut Window, win: WinId, action: &Action, batch: &mut Batch) {
    match feedback::focus(action, VerbOutcome::Nothing) {
        Some(Feedback::Refuse { log, text }) => {
            w.say(log);
            batch.push_effect(HostOp::Refuse { win, text });
        }
        Some(Feedback::Log(line)) => w.say(line),
        None => {}
    }
}
