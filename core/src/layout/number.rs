//! Number the tiles on screen, then floats bottom to top, using tmux's ten pane digits. Only modules
//! on screen get a digit, so a hidden or away module never takes one.

use super::module::ModuleId;
use super::tree::Layout;

pub fn number_plan(layout: &Layout) -> Vec<(char, ModuleId)> {
    "1234567890".chars().zip(layout.visible_modules()).collect()
}
