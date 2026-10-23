//! The keys a window's capture stages hand the controller, answered at once.
//!
//! The window stage sees every key first and feeds the window's prefix (`keymap::prefix`): a key the prefix does
//! not take passes on untouched, one it takes is consumed, and the command it finishes runs here (an action
//! through the runner, a digit landing in what `select.number` numbered) or, for a split's placement and a
//! chooser's pick, goes to the shell as its verb. Every change of what the prefix waits for is the chrome's, and
//! leaving the digit wait takes the digits down. The window losing activation and any click end a wait, as they
//! always have.
//!
//! The module stage sees a key on its way into one module: `Ctrl+h/j/k/l` moves the keys between modules (a chat
//! page may keep one for itself, a page never acts on one at an edge, the terminal and an editor with no nvim
//! running claim all four), and no web page sees a chord with Super or Hyper held.
//!
//! A key that passes through either stage, and a release nobody watches, allocate nothing: the toolkit asks on
//! every key, and the answer for the ordinary key must cost nothing.

use super::chrome;
use super::focus::{arrive_tail, give_keys, land_by_key, move_focus};
use super::host::*;
use super::window::{WinId, Window};
use crate::keymap::prefix::{Outcome, Prefix, PrefixCommand, Press, Waiting, DISPLAY_PANES_TIME};
use crate::keymap::KeyName;
use crate::layout::{Direction, ModuleId, ModuleKind};
use crate::tabs::ViewId;

/// A window's keys: its prefix, built from the window's config, and the timer of the digit wait while one is
/// armed.
#[derive(Default)]
pub struct KeyState {
    pub(super) prefix: Option<Prefix>,
    pub(super) number_timer: Option<TimerToken>,
}

/// Whether a press is the prefix's own `Esc`: `Esc` while the prefix waited for a key, whatever it waited for (a
/// command, a module after a split key, a digit).
pub fn prefix_escape(escape: bool, was: Waiting) -> bool {
    escape && was != Waiting::No
}

/// `Ctrl+h/j/k/l` with Control held and nothing else but CapsLock or Hyper: `Shift`, `Alt`, `Super` or `Meta`
/// held means it is not a move, so a page's (or a terminal program's) `Ctrl+Shift+K` or `Ctrl+Alt+h` stays its
/// own. CapsLock delivers the capital, which is the same chord. Hyper alone still moves: the prefix gives such a
/// press no name, so the chord is read from the key's own name and modifiers, never from the prefix's reading.
pub fn nav_direction(press: &KeyPress) -> Option<Direction> {
    let mods = press.mods;
    if !mods.ctrl || mods.shift || mods.alt || press.super_or_meta {
        return None;
    }
    match press.key {
        Some(KeyName::Char(c)) => match c.to_ascii_lowercase() {
            'h' => Some(Direction::Left),
            'j' => Some(Direction::Down),
            'k' => Some(Direction::Up),
            'l' => Some(Direction::Right),
            _ => None,
        },
        _ => None,
    }
}

/// What a web module's `Ctrl+h/j/k/l` does once it is known whether the keys moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavOutcome {
    /// The chord ends here: it moved the keys, or it is withheld from a page that may not act on it.
    Stop,
    /// The chord goes on to the page, which may use it.
    LetThrough,
}

/// A chord that moved stops. One with nowhere to go reaches the page unless the module swallows it at its edges:
/// a plugin page must never act on `Ctrl+h/j/k/l`, so whether the chord reached it cannot depend on where the
/// module sits.
pub fn nav_outcome(moved: bool, swallow_at_edge: bool) -> NavOutcome {
    if moved || swallow_at_edge {
        NavOutcome::Stop
    } else {
        NavOutcome::LetThrough
    }
}

/// Where `send-prefix` and `send-keys` hand their literal key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiteralTarget {
    Editor,
    Panel,
    Terminal,
    Neither,
}

/// The kind of the module holding the keys, if it is one that can take a literal: by kind, so any terminal or
/// chat instance takes it, and a Lua page wherever it sits does not.
pub fn literal_target(focused: Option<ModuleKind>) -> LiteralTarget {
    match focused {
        Some(ModuleKind::Editor) => LiteralTarget::Editor,
        Some(ModuleKind::Agent) => LiteralTarget::Panel,
        Some(ModuleKind::Terminal) => LiteralTarget::Terminal,
        _ => LiteralTarget::Neither,
    }
}

/// `prefix [` and `prefix PageUp`: the terminal's copy mode asks the question [`literal_target`] answers for a
/// literal chord -- does a terminal hold the keys right now -- so it is that check, not a second rule beside it.
pub fn copy_mode_entry(focused: Option<ModuleKind>) -> bool {
    matches!(literal_target(focused), LiteralTarget::Terminal)
}

/// The window stage: `press` fed to the window's prefix at the scheduler's time. A change of what the prefix
/// waits for is the chrome's, the prefix's own `Esc` puts a container selection back on the module with the
/// keys, and a finished command is handed back for the controller to carry out. A window with no prefix yet
/// (before its config) passes everything.
pub(super) fn window_press(
    w: &mut Window,
    win: WinId,
    press: &KeyPress,
    batch: &mut Batch,
) -> (Verdict, Option<PrefixCommand>) {
    let now = w.now();
    let Some(prefix) = w.keys.prefix.as_mut() else {
        return (Verdict::PassThrough, None);
    };
    let was = prefix.waiting();
    let outcome = prefix.press(press.press, press.keycode, now);
    let after = prefix.waiting();
    if was != after {
        waiting_changed(w, win, was, after, batch);
    }
    if prefix_escape(press.press == Press::Escape, was) {
        escape(w, win, batch);
    }
    match outcome {
        Outcome::Pass => (Verdict::PassThrough, None),
        Outcome::Swallow => (Verdict::Consumed, None),
        Outcome::Run(command) => {
            w.say(format!("[prefix] {command:?}"));
            (Verdict::Consumed, Some(command))
        }
    }
}

/// The module stage, for a key on its way into `module`; see the module documentation.
pub(super) fn module_press(
    w: &mut Window,
    reads: &dyn HostReads,
    win: WinId,
    module: &ModuleId,
    press: &KeyPress,
    batch: &mut Batch,
) -> Verdict {
    let kind = module.kind();
    if let Some(direction) = nav_direction(press) {
        match kind {
            ModuleKind::Agent => {
                if let Some(view) = ViewId::of_module(module).filter(|view| reads.chat_claims(*view, direction)) {
                    // The page keeps the chord for its own use (its composer's), and is told it as a key.
                    batch.push_effect(HostOp::Chat(ChatOp::NavKey { win, view, direction }));
                    return Verdict::Consumed;
                }
                let moved = move_focus(w, reads, win, module, direction, batch);
                return match nav_outcome(moved, false) {
                    NavOutcome::Stop => Verdict::Consumed,
                    NavOutcome::LetThrough => Verdict::LetThrough,
                };
            }
            ModuleKind::LuaWebview => {
                move_focus(w, reads, win, module, direction, batch);
                return Verdict::Consumed;
            }
            // All four are the terminal's to give up even with no module that way, so what its shell receives
            // never depends on the layout.
            ModuleKind::Terminal => {
                move_focus(w, reads, win, module, direction, batch);
                return Verdict::Consumed;
            }
            // With no nvim in the editor nothing inside it can resolve the chord, so it moves at the shell's level,
            // as vim-tmux-navigator's own tmux half moves a pane not running vim.
            ModuleKind::Editor if !reads.editor_running(win) => {
                move_focus(w, reads, win, module, direction, batch);
                return Verdict::Consumed;
            }
            ModuleKind::Editor | ModuleKind::Canvas => return Verdict::PassThrough,
        }
    }
    // A page cannot see Super or Hyper on its own key events, so a chord the window or the compositor owns would
    // reach it as a plain key.
    if press.super_or_hyper && matches!(kind, ModuleKind::Agent | ModuleKind::LuaWebview) {
        return Verdict::Consumed;
    }
    Verdict::PassThrough
}

/// Whether the window's prefix waits for the release of `keycode`: the prefix's own key while armed, and the key
/// that ran a command. Any other release need not be reported.
pub(super) fn watches_release(w: &Window, keycode: u32) -> bool {
    w.keys.prefix.as_ref().is_some_and(|prefix| prefix.holds(keycode))
}

/// A watched key was released.
pub(super) fn released(w: &mut Window, keycode: u32) {
    if let Some(prefix) = w.keys.prefix.as_mut() {
        prefix.release(keycode);
    }
}

/// The window stopped being active, or was clicked: a prefix waiting for a key stops waiting.
pub(super) fn cancel(w: &mut Window, win: WinId, batch: &mut Batch) {
    let Some(prefix) = w.keys.prefix.as_mut() else {
        return;
    };
    if !prefix.is_armed() {
        return;
    }
    let was = prefix.waiting();
    prefix.cancel();
    waiting_changed(w, win, was, Waiting::No, batch);
}

/// The workspace chooser is drawn: the prefix waits for its key, with no timer.
pub(super) fn await_workspace(w: &mut Window, win: WinId, batch: &mut Batch) {
    let Some(prefix) = w.keys.prefix.as_mut() else {
        w.say("[prefix] the chooser has no prefix to wait in yet".into());
        return;
    };
    let was = prefix.waiting();
    prefix.await_workspace();
    let after = prefix.waiting();
    if was != after {
        waiting_changed(w, win, was, after, batch);
    }
}

/// `select.number`'s wait, once its digits are up: the plan they show kept for the digit, the prefix waiting for
/// one, and the wait's timer armed. `false`, with nothing changed, for a window with no prefix yet.
pub(super) fn await_number(w: &mut Window, win: WinId, plan: Vec<(char, ModuleId)>, batch: &mut Batch) -> bool {
    let now = w.now();
    let Some(prefix) = w.keys.prefix.as_mut() else {
        return false;
    };
    let was = prefix.waiting();
    prefix.await_number(now);
    if let Some(old) = w.keys.number_timer.take() {
        w.cancel_timer(old);
    }
    w.keys.number_timer = Some(w.arm_after(DISPLAY_PANES_TIME));
    w.chrome.prefix.plan = plan;
    if was == Waiting::Number {
        // The wait goes on with a new plan and its own timer.
        w.chrome_changed(win, batch);
    } else {
        waiting_changed(w, win, was, Waiting::Number, batch);
    }
    true
}

/// A timer fired: when it is this window's digit wait, the wait ends if no digit came in time. `false` when
/// `token` is not the wait's.
pub(super) fn number_timer_fired(w: &mut Window, win: WinId, token: TimerToken, batch: &mut Batch) -> bool {
    if w.keys.number_timer != Some(token) {
        return false;
    }
    w.keys.number_timer = None;
    // The timer is cancelled whenever its wait ends or another begins, so a wait it finds is the one it was armed
    // for. Asked at a time past any deadline that wait can have, so a timer the clock fires a moment early still
    // ends it.
    let past = w.now() + DISPLAY_PANES_TIME;
    let Some(prefix) = w.keys.prefix.as_mut() else {
        return true;
    };
    let was = prefix.waiting();
    if prefix.expire(past) {
        w.say("[prefix] armed=false waiting=No (no digit in time)".into());
        set_waiting(w, win, was, Waiting::No, batch);
    }
    true
}

/// A digit typed in the digit wait lands on the module it showed, looked up in what was drawn and not in the layout
/// as it is now. The zoomed module gets the keys and the zoom stays, as tmux keeps a zoom when the pane picked is
/// the zoomed one, and arrives only if its grab takes; any other lands as `Ctrl+h/j/k/l` land. A module that left
/// the screen during the wait moves nothing, and a digit no module had does nothing, as in tmux.
pub(super) fn land_digit(w: &mut Window, win: WinId, digit: char, batch: &mut Batch) {
    let to = w
        .chrome
        .prefix
        .plan
        .iter()
        .find(|(d, _)| *d == digit)
        .map(|(_, id)| id.clone());
    let Some(to) = to else {
        w.say(format!("[prefix] number {digit}: no module has it"));
        return;
    };
    let facts = w
        .layout
        .try_borrow()
        .map(|layout| (layout.can_focus(&to).is_ok(), layout.zoomed() == Some(&to)));
    let Ok((there, zoomed_here)) = facts else {
        w.warn(format!(
            "[prefix] BUG: the layout was borrowed when number {digit} landed"
        ));
        return;
    };
    if !there {
        w.say(format!(
            "[prefix] number {digit} -> {to}: it left the screen during the wait; the keys stay"
        ));
        return;
    }
    let asked = if zoomed_here {
        give_keys(w, win, &to, arrive_tail(win, &to, false), batch)
    } else {
        land_by_key(w, win, &to, batch)
    };
    w.say(format!("[prefix] number {digit} -> {to} (focus asked={asked})"));
}

/// The digits `plan` shows, as the hint op that draws them.
pub(super) fn show_digits(plan: &[(char, ModuleId)]) -> HostOp {
    HostOp::Hint(HintOp::ShowDigits(chrome::digits(plan)))
}

/// What the prefix waits for changed, printed both ways: an arm that never disarms looks exactly like a window
/// refusing input.
fn waiting_changed(w: &mut Window, win: WinId, was: Waiting, now: Waiting, batch: &mut Batch) {
    w.say(format!("[prefix] armed={} waiting={now:?}", now != Waiting::No));
    set_waiting(w, win, was, now, batch);
}

/// The chrome follows what the prefix waits for; leaving the digit wait takes the digits down, however it ended.
fn set_waiting(w: &mut Window, win: WinId, was: Waiting, now: Waiting, batch: &mut Batch) {
    if was == Waiting::Number && now != Waiting::Number {
        batch.push_effect(HostOp::Hint(HintOp::Hide));
        if let Some(timer) = w.keys.number_timer.take() {
            w.cancel_timer(timer);
        }
    }
    w.chrome.prefix.waiting = now;
    w.chrome_changed(win, batch);
}

/// `prefix Esc`: a container selection goes back to the module with the keys. A bare `Esc` is the module's own
/// key and never gets here.
fn escape(w: &mut Window, win: WinId, batch: &mut Batch) {
    if !w.clear_selection() {
        return;
    }
    w.say("[modules] the selection is back on the module with the keys".into());
    let hooks = HostOp::ChromeHooks { win };
    if !batch.ops().any(|op| *op == hooks) {
        batch.push_effect(hooks);
    }
}

#[cfg(test)]
mod tests;
