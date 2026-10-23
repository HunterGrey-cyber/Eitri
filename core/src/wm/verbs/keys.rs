//! The keys verbs: `select.number` (tmux's `display-panes`: the digits go up, then the prefix waits for one),
//! HINT's toggle from the prefix, `send-prefix` and `send-keys` (a literal key for the module that holds the keys)
//! and the terminal's copy mode, each refusal with the words the shell's arms gave.

use super::super::feedback::{self, Feedback, VerbOutcome};
use super::super::host::*;
use super::super::keys::{await_number, copy_mode_entry, literal_target, show_digits, LiteralTarget};
use super::super::window::{WinId, Window};
use crate::keymap::prefix::literal_for;
use crate::keymap::{Action, Keymap};
use crate::layout::{number_plan, ModuleId};
use crate::tabs::ViewId;

/// Runs the keys verb `action` on `w` and adds what follows to `batch`; `keycode` is the key that ran it, 0 for
/// any other origin.
pub fn run(w: &mut Window, win: WinId, action: &Action, keycode: u32, keymap: &Keymap, batch: &mut Batch) {
    match action {
        Action::SelectNumber => select_number(w, win, action, batch),
        // The key that ran it is still down, and HINT must not take its release for a key of its own.
        Action::Hint => batch.push_effect(HostOp::Hint(HintOp::Toggle { keycode })),
        Action::SendPrefix | Action::SendKeys(_) => send_literal(w, win, action, keymap, batch),
        Action::CopyMode { up } => {
            let focused = w.focus.native_module().cloned();
            match focused.filter(|module| copy_mode_entry(Some(module.kind()))) {
                Some(module) => batch.push_effect(HostOp::Terminal(TermOp::CopyMode { win, module, up: *up })),
                None => refuse(w, win, action, batch),
            }
        }
        other => debug_assert!(false, "{other:?} is not a keys verb"),
    }
}

/// The digits go up first, then the prefix waits for one, so a window that cannot number anything never
/// swallows the next key.
fn select_number(w: &mut Window, win: WinId, action: &Action, batch: &mut Batch) {
    let Ok(plan) = w.layout.try_borrow().map(|layout| number_plan(&layout)) else {
        w.warn("[wm] BUG: the layout was borrowed when select.number ran".into());
        return;
    };
    if plan.is_empty() {
        refuse(w, win, action, batch);
        return;
    }
    if w.keys.prefix.is_none() {
        w.say("[prefix] select.number: this window has no prefix to wait in yet".into());
        return;
    }
    batch.push_effect(show_digits(&plan));
    w.say(format!("[prefix] select.number: {} numbered", plan.len()));
    await_number(w, win, plan, batch);
}

/// The literal key goes to the module that holds the keys now: the editor in its own key notation, the chat view's
/// page (the base view's when the view cannot be told), the terminal with the keys whichever instance it is, and
/// nowhere when the keys are in anything else.
fn send_literal(w: &mut Window, win: WinId, action: &Action, keymap: &Keymap, batch: &mut Batch) {
    let Some(key) = literal_for(action, keymap) else {
        return;
    };
    let focused = w.focus.native_module().cloned();
    match (literal_target(focused.as_ref().map(ModuleId::kind)), focused) {
        (LiteralTarget::Editor, _) => batch.push_effect(HostOp::Editor(EditorCall::SendKeys {
            win,
            keys: key.to_vim(),
        })),
        (LiteralTarget::Panel, focused) => {
            let view = focused.as_ref().and_then(ViewId::of_module).unwrap_or(ViewId::BASE);
            batch.push_effect(HostOp::Chat(ChatOp::LiteralKey { win, view, key }));
        }
        (LiteralTarget::Terminal, Some(module)) => {
            batch.push_effect(HostOp::Terminal(TermOp::LiteralKey { win, module, key }))
        }
        (LiteralTarget::Terminal, None) | (LiteralTarget::Neither, _) => {}
    }
}

fn refuse(w: &mut Window, win: WinId, action: &Action, batch: &mut Batch) {
    match feedback::keys(action, VerbOutcome::Nothing) {
        Some(Feedback::Refuse { log, text }) => {
            w.say(log);
            batch.push_effect(HostOp::Refuse { win, text });
        }
        Some(Feedback::Log(line)) => w.say(line),
        None => {}
    }
}
