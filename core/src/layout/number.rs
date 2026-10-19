//! Number the modules on screen in tree order, using tmux's ten pane digits.

use super::module::ModuleId;
use super::tree::Layout;

pub fn number_plan(layout: &Layout) -> Vec<(char, ModuleId)> {
    if let Some(zoomed) = layout.zoomed() {
        return vec![('1', zoomed.clone())];
    }
    "1234567890".chars().zip(layout.visible_leaves()).collect()
}
