//! What a tab verb after the prefix does to the keys. Pure, so the table
//! the spec gives is a test rather than a reading of `main.rs`.

use eitri_core::keymap::TabAction;
use eitri_core::layout::ModuleKind;
use eitri_core::tabs::{next_prev_target, NextPrevTarget};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TabVerb {
    New,
    Step(i32),
    Last,
    Select(u16),
    Rename,
    Close,
    /// `<leader>bo`: asks for y/n confirmation in the footer for closing all tabs except the active one.
    CloseOthers,
    Choose,
    Info,
    /// Nothing to act on yet (the canvas's revisions): the app name flashes.
    Flash,
    /// Taken and dropped (one terminal's `n`/`p`).
    Nothing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TabVerbPlan {
    pub verb: TabVerb,
    /// Show the chat and focus it before creating a tab, renaming, closing, or opening
    /// its chooser or popover: these actions need keys. Switching tabs keeps focus where it is.
    pub takes_the_keys: bool,
}

pub(crate) fn plan(action: TabAction, keys_in: Option<ModuleKind>, terminals: usize) -> TabVerbPlan {
    let (verb, takes_the_keys) = match action {
        TabAction::New => (TabVerb::New, true),
        TabAction::Rename => (TabVerb::Rename, true),
        TabAction::Close => (TabVerb::Close, true),
        TabAction::Choose => (TabVerb::Choose, true),
        TabAction::Info => (TabVerb::Info, true),
        // `<leader>bo` is bound only in the panel table, with no prefix chord. It reaches this function
        // only as a `tab_verb` message from inside the panel, never through `table.rs`'s `bind()` calls.
        // The panel already has the keys, so `true` follows `Close`'s behavior without moving focus.
        TabAction::CloseOthers => (TabVerb::CloseOthers, true),
        TabAction::Last => (TabVerb::Last, false),
        TabAction::Select(n) => (TabVerb::Select(u16::from(n)), false),
        TabAction::Next | TabAction::Prev => {
            let delta = if action == TabAction::Next { 1 } else { -1 };
            let verb = match next_prev_target(keys_in, terminals) {
                NextPrevTarget::ChatTabs => TabVerb::Step(delta),
                NextPrevTarget::CanvasRevisions => TabVerb::Flash,
                NextPrevTarget::Terminals | NextPrevTarget::Swallowed => TabVerb::Nothing,
            };
            (verb, false)
        }
    };
    TabVerbPlan { verb, takes_the_keys }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::keymap::TabAction;
    use eitri_core::layout::ModuleKind;

    #[test]
    fn creating_and_the_verbs_that_need_the_keys_take_them_switching_does_not() {
        for action in [
            TabAction::New,
            TabAction::Rename,
            TabAction::Close,
            TabAction::CloseOthers,
            TabAction::Choose,
            TabAction::Info,
        ] {
            assert!(plan(action, Some(ModuleKind::Editor), 1).takes_the_keys, "{action:?}");
        }
        assert_eq!(
            plan(TabAction::CloseOthers, Some(ModuleKind::Editor), 1).verb,
            TabVerb::CloseOthers,
            "<leader>bo"
        );
        assert_eq!(
            plan(TabAction::New, Some(ModuleKind::Editor), 1).verb,
            TabVerb::New,
            "new-tab actions create a new tab"
        );
        for action in [TabAction::Next, TabAction::Prev, TabAction::Last, TabAction::Select(2)] {
            assert!(
                !plan(action, Some(ModuleKind::Editor), 1).takes_the_keys,
                "switching tabs from the editor does not take the keys: {action:?}"
            );
        }
        assert_eq!(plan(TabAction::Select(2), None, 1).verb, TabVerb::Select(2));
        assert_eq!(
            plan(TabAction::Last, Some(ModuleKind::Terminal), 1).verb,
            TabVerb::Last,
            "a verb from anywhere"
        );
    }

    #[test]
    fn n_and_p_follow_the_module_with_the_keys() {
        assert_eq!(plan(TabAction::Next, Some(ModuleKind::Agent), 1).verb, TabVerb::Step(1));
        assert_eq!(
            plan(TabAction::Prev, Some(ModuleKind::Editor), 1).verb,
            TabVerb::Step(-1)
        );
        assert_eq!(
            plan(TabAction::Next, None, 1).verb,
            TabVerb::Step(1),
            "from the top bar"
        );
        assert_eq!(
            plan(TabAction::Next, Some(ModuleKind::Canvas), 1).verb,
            TabVerb::Flash,
            "no canvas yet"
        );
        assert_eq!(
            plan(TabAction::Next, Some(ModuleKind::Terminal), 1).verb,
            TabVerb::Nothing,
            "swallowed"
        );
    }
}
