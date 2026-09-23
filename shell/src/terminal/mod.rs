//! The bottom terminal's GTK half (spec docs/superpowers/specs/2026-09-23-bottom-terminal-design.md).
//! The engine -- PTY, session thread, emulator, paint -- is `neovibe-terminal`, which has no GTK.
//!
//! This module holds the widget ([`TerminalPane`]) and the few decisions `main.rs` makes about it,
//! each as a pure function so it is tested without a display: what `Ctrl+a t` does, where
//! `Ctrl+a Ctrl+a` goes, which chords the terminal gives up to neovibe, and its colours.

mod gl;
pub(crate) mod keys;
mod pane;

pub(crate) use pane::TerminalPane;

use gtk4::gdk::{Key, ModifierType};
use neovibe_core::theme::ThemeTokens;
use neovibe_terminal::TerminalColors;
use terminal_render::RgbColor;

use crate::layout::Direction;

/// The bottom slot's index in `pane_focus`'s list: main, side, bottom.
pub(crate) const BOTTOM_PANE: usize = 2;

/// What `Ctrl+a t` does (spec §2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToggleAction {
    /// Hidden: show it and focus it. The first time, this is what starts the shell.
    ShowAndFocus,
    /// Shown, but the keys are elsewhere: focus it.
    Focus,
    /// It holds the keys: give them back to the pane above, then hide it. The shell keeps running.
    HideAndReturn,
}

pub(crate) fn toggle_action(shown: bool, has_keys: bool) -> ToggleAction {
    match (shown, has_keys) {
        (false, _) => ToggleAction::ShowAndFocus,
        (true, false) => ToggleAction::Focus,
        (true, true) => ToggleAction::HideAndReturn,
    }
}

/// One thing `main.rs` does for a [`ToggleAction`], in the order [`ToggleAction::steps`] gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToggleStep {
    /// `PaneLayout::unzoom`.
    Unzoom,
    /// `PaneLayout::set_bottom_shown(true)`.
    Show,
    /// `TerminalPane::start`: the first time, this starts the shell.
    Start,
    /// Focus to the terminal.
    FocusTerminal,
    /// Focus to the pane above that last held the keys.
    FocusAbove,
    /// `PaneLayout::set_bottom_shown(false)`.
    Hide,
}

impl ToggleAction {
    /// What to do, in order. Every action unzooms first: with the terminal zoomed, hiding it without
    /// unzooming hid the top row AND the terminal -- nothing visible, and focus sent to a hidden
    /// pane (review 2026-09-23, finding 4). And focus leaves before the terminal hides: a hidden
    /// widget holding focus leaves the window with keys going nowhere.
    pub(crate) fn steps(self) -> &'static [ToggleStep] {
        use ToggleStep::*;
        match self {
            ToggleAction::ShowAndFocus => &[Unzoom, Show, Start, FocusTerminal],
            ToggleAction::Focus => &[Unzoom, FocusTerminal],
            ToggleAction::HideAndReturn => &[Unzoom, FocusAbove, Hide],
        }
    }
}

/// Where `Ctrl+a Ctrl+a` hands its literal `Ctrl+a`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiteralTarget {
    Editor,
    Panel,
    Terminal,
    Neither,
}

/// The pane holding the keys (`pane_focus::focused_pane`), if it is one that can take a literal.
/// Before this, pane 2 was always `Neither`.
pub(crate) fn literal_target(
    focused: Option<usize>,
    editor_is_main: bool,
    side_is_agent: bool,
    bottom_is_terminal: bool,
) -> LiteralTarget {
    match focused {
        Some(0) if editor_is_main => LiteralTarget::Editor,
        Some(1) if side_is_agent => LiteralTarget::Panel,
        Some(BOTTOM_PANE) if bottom_is_terminal => LiteralTarget::Terminal,
        _ => LiteralTarget::Neither,
    }
}

/// `Ctrl+h/j/k/l` with nothing else held: the chords neovibe takes from the terminal before its own
/// key controller sees them ("neovibe的按键优先"). Claimed even with no pane in that direction, so what
/// the shell receives never depends on the layout. `Shift`, `Alt` or `Super` held means it is not
/// one of these, and the terminal gets it.
pub(crate) fn navigation(key: Key, state: ModifierType) -> Option<Direction> {
    let others = ModifierType::SHIFT_MASK | ModifierType::ALT_MASK | ModifierType::SUPER_MASK | ModifierType::META_MASK;
    if !state.contains(ModifierType::CONTROL_MASK) || state.intersects(others) {
        return None;
    }
    match key.to_lower() {
        Key::h => Some(Direction::Left),
        Key::j => Some(Direction::Down),
        Key::k => Some(Direction::Up),
        Key::l => Some(Direction::Right),
        _ => None,
    }
}

/// Phase 1 takes two colours from the window's theme -- background and foreground -- so a light
/// colorscheme does not get a dark box under it. The sixteen ANSI colours follow nvim in phase 4.
///
/// **No fixed cursor colour** (GUI pass 2026-09-23, defect 3). This used to hand over the theme's
/// foreground as the cursor, and a fixed colour is drawn whatever the cell under it: a program that
/// paints its own colours (nvim with LazyVim's light scheme, under a dark theme) got a pale block on
/// a pale cell. With `None` the cursor takes the covered cell's colours swapped, as foot does; on
/// the theme's own default cells that is the same foreground block as before.
pub(crate) fn colors_from(tokens: &ThemeTokens) -> TerminalColors {
    let rgb = |c: neovibe_core::theme::color::Rgb| RgbColor::new(c.r, c.g, c.b);
    TerminalColors {
        background: rgb(tokens.bg),
        foreground: rgb(tokens.fg),
        cursor: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_a_t_shows_then_focuses_then_hides() {
        assert_eq!(toggle_action(false, false), ToggleAction::ShowAndFocus);
        assert_eq!(toggle_action(true, false), ToggleAction::Focus);
        assert_eq!(toggle_action(true, true), ToggleAction::HideAndReturn);
    }

    /// `Ctrl+a z` in the terminal, then `Ctrl+a t`: without the unzoom, nothing was left visible.
    #[test]
    fn every_toggle_unzooms_first_and_focus_leaves_before_the_terminal_hides() {
        for action in [
            ToggleAction::ShowAndFocus,
            ToggleAction::Focus,
            ToggleAction::HideAndReturn,
        ] {
            assert_eq!(action.steps().first(), Some(&ToggleStep::Unzoom), "{action:?}");
        }
        let hide = ToggleAction::HideAndReturn.steps();
        let at = |step| hide.iter().position(|s| *s == step);
        assert!(at(ToggleStep::FocusAbove) < at(ToggleStep::Hide), "{hide:?}");
        let show = ToggleAction::ShowAndFocus.steps();
        let at = |step| show.iter().position(|s| *s == step);
        assert!(
            at(ToggleStep::Show) < at(ToggleStep::FocusTerminal),
            "a hidden widget cannot take focus"
        );
    }

    /// Fix round 1, review finding 1: the assertions above pass even with `FocusAbove` or `Show`
    /// deleted outright, because `at(step)` is `Option<usize>` and Rust orders `None < Some(_)` --
    /// a missing step's position compares less than a present one's, so `at(missing) < at(present)`
    /// reads `true`. Demonstrated: with `HideAndReturn => &[Unzoom, Hide]` (no `FocusAbove`) and
    /// `ShowAndFocus => &[Unzoom, Start, FocusTerminal]` (no `Show`), the test above still passes.
    /// Pinning each sequence whole closes that hole -- a missing or reordered step fails the
    /// `assert_eq!` directly, with no `Option` ordering involved.
    #[test]
    fn each_actions_full_step_sequence_is_pinned() {
        use ToggleStep::*;
        assert_eq!(
            ToggleAction::ShowAndFocus.steps(),
            &[Unzoom, Show, Start, FocusTerminal]
        );
        assert_eq!(ToggleAction::Focus.steps(), &[Unzoom, FocusTerminal]);
        assert_eq!(ToggleAction::HideAndReturn.steps(), &[Unzoom, FocusAbove, Hide]);
    }

    #[test]
    fn ctrl_a_ctrl_a_reaches_the_terminal_only_when_it_holds_the_keys() {
        assert_eq!(literal_target(Some(2), true, true, true), LiteralTarget::Terminal);
        assert_eq!(
            literal_target(Some(2), true, true, false),
            LiteralTarget::Neither,
            "a Lua bottom panel"
        );
        assert_eq!(literal_target(Some(0), true, true, true), LiteralTarget::Editor);
        assert_eq!(literal_target(Some(1), true, true, true), LiteralTarget::Panel);
        assert_eq!(
            literal_target(None, true, true, true),
            LiteralTarget::Neither,
            "the top bar"
        );
    }

    #[test]
    fn exactly_ctrl_hjkl_are_taken_from_the_terminal() {
        let ctrl = ModifierType::CONTROL_MASK;
        assert_eq!(navigation(Key::h, ctrl), Some(Direction::Left));
        assert_eq!(navigation(Key::j, ctrl), Some(Direction::Down));
        assert_eq!(navigation(Key::k, ctrl), Some(Direction::Up));
        assert_eq!(navigation(Key::l, ctrl), Some(Direction::Right));
        assert_eq!(
            navigation(Key::k, ctrl | ModifierType::LOCK_MASK),
            Some(Direction::Up),
            "CapsLock is not a chord"
        );
        // GDK delivers the UPPERCASE keyval under CapsLock, not `Key::k` with `LOCK_MASK` alone --
        // the case above never reached `.to_lower()` at all (review 2026-09-23, task-8 minor 1).
        assert_eq!(
            navigation(Key::K, ctrl | ModifierType::LOCK_MASK),
            Some(Direction::Up),
            "CapsLock delivers the uppercase keyval"
        );
        assert_eq!(navigation(Key::l, ctrl | ModifierType::SHIFT_MASK), None);
        assert_eq!(navigation(Key::a, ctrl), None, "the prefix is the prefix's");
        assert_eq!(navigation(Key::k, ModifierType::empty()), None);
    }

    #[test]
    fn the_terminal_takes_the_themes_background_and_foreground() {
        let tokens = ThemeTokens::fallback();
        let colors = colors_from(&tokens);
        assert_eq!(colors.background, RgbColor::new(tokens.bg.r, tokens.bg.g, tokens.bg.b));
        assert_eq!(colors.foreground, RgbColor::new(tokens.fg.r, tokens.fg.g, tokens.fg.b));
    }

    /// GUI pass 2026-09-23, defect 3: a fixed cursor colour from the theme vanished on a cell a
    /// program painted near it. The theme gives none, so the cursor follows the cell it covers.
    #[test]
    fn the_theme_fixes_no_cursor_colour_so_the_cursor_follows_the_cell() {
        let colors = colors_from(&ThemeTokens::fallback());
        assert_eq!(colors.cursor, None);
        assert_eq!(colors.cursor_coloring(), terminal_render::CursorColoring::CellInverse);
    }
}
