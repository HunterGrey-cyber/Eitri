//! What a window's chrome shows, as values the controller keeps and the shell draws: so far the prefix's part,
//! what it waits for and the digits `select.number` put up. A change of either bumps the window's chrome
//! revision and is announced as [`HostOp::Chrome`](super::host::HostOp::Chrome); the shell reads the model back
//! through [`Controller::chrome`](super::Controller::chrome).

use crate::keymap::prefix::Waiting;
use crate::layout::ModuleId;

/// A window's chrome as the controller decides it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChromeModel {
    /// The prefix's part: the app name's armed block, the strip's pieces and the digits' wait.
    pub prefix: PrefixView,
}

/// What the prefix shows: what it waits for, and the digits the last `select.number` numbered the modules with,
/// in the order it numbered them. The plan outlives its wait: a digit is looked up in what was drawn, after the
/// wait has already ended by that very key.
#[derive(Debug, Clone, PartialEq)]
pub struct PrefixView {
    pub waiting: Waiting,
    pub plan: Vec<(char, ModuleId)>,
}

impl Default for PrefixView {
    fn default() -> Self {
        PrefixView {
            waiting: Waiting::No,
            plan: Vec::new(),
        }
    }
}

/// The digits `plan` draws, as the hint coordinator takes them: each module with its digit's value, the tenth
/// module's `0` being 0.
pub fn digits(plan: &[(char, ModuleId)]) -> Vec<(ModuleId, u8)> {
    plan.iter()
        .filter_map(|(digit, module)| digit.to_digit(10).map(|value| (module.clone(), value as u8)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digits_drawn_are_the_plans_with_the_tenth_module_at_zero() {
        let plan: Vec<(char, ModuleId)> = "1234567890"
            .chars()
            .enumerate()
            .map(|(i, digit)| (digit, ModuleId::lua(&format!("p{i}"))))
            .collect();
        let drawn = digits(&plan);
        assert_eq!(drawn.len(), 10);
        assert_eq!(drawn[0], (ModuleId::lua("p0"), 1));
        assert_eq!(drawn[8], (ModuleId::lua("p8"), 9));
        assert_eq!(drawn[9], (ModuleId::lua("p9"), 0), "the tenth module's key is 0");
    }

    #[test]
    fn a_window_starts_with_the_prefix_waiting_for_nothing_and_no_plan() {
        let model = ChromeModel::default();
        assert_eq!(model.prefix.waiting, Waiting::No);
        assert!(model.prefix.plan.is_empty());
    }
}
