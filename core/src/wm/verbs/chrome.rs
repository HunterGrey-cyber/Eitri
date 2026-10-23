//! The chrome's verbs: `window.immersive`, which toggles the window's mode as `prefix F11` always has. The mode
//! is the chrome model's, the window's own state changes through the op the toggle emits, and the toggle never
//! refuses, so it says nothing on screen.

use super::super::feedback::{self, Feedback, VerbOutcome};
use super::super::host::Batch;
use super::super::window::{WinId, Window};
use crate::keymap::Action;

/// Runs the chrome verb `action` on `w` and adds what follows to `batch`.
pub fn run(w: &mut Window, win: WinId, action: &Action, batch: &mut Batch) {
    match action {
        Action::WindowImmersive => super::super::chrome::toggle_immersive(w, win, batch),
        other => {
            debug_assert!(false, "{other:?} is not a chrome verb");
            return;
        }
    }
    match feedback::chrome(action, VerbOutcome::Done) {
        Some(Feedback::Refuse { log, text }) => {
            w.say(log);
            w.refuse(win, text, batch);
        }
        Some(Feedback::Log(line)) => w.say(line),
        None => {}
    }
}
