//! The bottom terminal's GTK half (spec docs/superpowers/specs/2026-09-23-bottom-terminal-design.md).
//! The engine -- PTY, session thread, emulator, paint -- is `neovibe-terminal`, which has no GTK.
//!
//! This module holds the widget ([`TerminalPane`]) and the few decisions `main.rs` makes about it,
//! each as a pure function so it is tested without a display: what `Ctrl+a t` does, where
//! `Ctrl+a Ctrl+a` goes, which chords the terminal gives up to neovibe, and its colours.
//!
//! **A module since the modules design's P1** (docs/superpowers/specs/
//! 2026-09-23-modules-and-canvas-design.md; its plan's Task 11 re-homed it). It is
//! `ModuleId::terminal()`, placed below the editor and the agent (a Lua `bottom` panel goes under
//! it instead of replacing it) and hidden in the layout until the first `Ctrl+a t`. Where it goes,
//! what shows it, who gets the keys when it hides and where `Ctrl+h/j/k/l` from it lead are the
//! layout's (`neovibe_core::layout`, `ModuleGrid`), not a slot index's: this module keeps only the
//! decisions that are the terminal's own.

mod bell;
mod gl;
mod ime;
mod input_queue;
pub(crate) mod keys;
mod pane;

pub(crate) use pane::TerminalPane;

use gtk4::gdk::{Key, ModifierType};
use neovibe_core::layout::{Frame, Layout, LayoutError, ModuleDecl, ModuleId, ModuleKind, Placement};
use neovibe_core::theme::ThemeTokens;
use neovibe_terminal::{ExitInfo, TerminalColors};
use terminal_render::RgbColor;

use crate::layout::Direction;

/// The first-launch layout with the terminal in it: `Layout::initial` over the terminal first --
/// below the editor and the agent, so a Lua `bottom` panel goes under it rather than replacing it as
/// it did on `main` -- then `lua`, in the order they were registered. The terminal is hidden before
/// anything is built from the layout, so its host is never on screen until `Ctrl+a t`, and the
/// window looks exactly as it did without it (`BELOW_ROOT_SHARE`'s split collapses: `[editor | agent]`
/// gets the whole height).
///
/// **Full width only without a Lua `side` panel** (Task 11's review, finding 4). Placed after the
/// terminal, a `side` panel wraps the whole root, `Row(Column(Row(editor | agent), terminal), side)`:
/// the side panel runs the full height and the terminal is only as wide as `[editor | agent]`
/// (857 of 1280px in the default window, where `main` spanned the full width under
/// `[editor | side slot]`). A Lua `bottom` panel, likewise, puts the terminal's own split inside the
/// upper two thirds, so its first show is 160px, not 240. Either panel can also be the module that
/// takes the keys when the terminal hides, if it had them more recently than the editor or the agent.
///
/// Hidden through `neovibe_core::layout::hide`, the only door (`Layout::hide_unfocused` is private
/// to the layout so nothing hides the module with the keys without choosing where they go). At
/// startup the keys are on the editor or a Lua `main` panel, never on the terminal, so `hide` reads
/// no geometry and the frame it is given is only a formality.
pub(crate) fn initial_layout(lua: &[ModuleDecl]) -> Result<Layout, LayoutError> {
    let mut decls = vec![ModuleDecl {
        id: ModuleId::terminal(),
        placement: Placement::BelowRoot,
    }];
    decls.extend_from_slice(lua);
    let mut layout = Layout::initial(&decls)?;
    neovibe_core::layout::hide(
        &mut layout,
        &ModuleId::terminal(),
        &Frame::new(crate::module_grid::UNALLOCATED, 1),
    )?;
    Ok(layout)
}

/// What `Ctrl+a t` does (spec §2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToggleAction {
    /// Hidden: show it and focus it. The first time, this is what starts the shell.
    ShowAndFocus,
    /// Shown, but the keys are elsewhere: focus it.
    Focus,
    /// It holds the keys: give them back to a module next to it, then hide it. The shell keeps
    /// running.
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
    /// `ModuleGrid::unzoom`.
    Unzoom,
    /// `ModuleGrid::show_module`: back where it was hidden from, at the ratio it had (the first
    /// time, `BELOW_ROOT_SHARE`'s third of the height).
    Show,
    /// `TerminalPane::start`: the first time, this starts the shell.
    Start,
    /// Focus to the terminal.
    FocusTerminal,
    /// `ModuleGrid::hide_module`. With the keys in the terminal, it first gives them to the module
    /// next to it that had them last -- the editor or the agent above it, unless a Lua `bottom` panel
    /// under it or a Lua `side` panel beside it was more recent -- and only then unmaps it. That
    /// order is `module_grid::hide_then_unmap`'s, and its own test holds it.
    ///
    /// (Task 11's review, finding 1: until then a `FocusAbove` step sat before this one. On `main`
    /// it was the real `return_to_upper_pane()` call. After the merge `main.rs` ran it as a no-op,
    /// because `hide_module` already did the focusing, so the tests pinning `FocusAbove` before
    /// `Hide` held only a list. The step was removed, and the order is tested where it runs.)
    Hide,
}

impl ToggleAction {
    /// What to do, in order. Every action unzooms first: with the terminal zoomed, hiding it without
    /// unzooming hid the top row AND the terminal -- nothing visible, and focus sent to a hidden
    /// pane (review 2026-09-23, finding 4). A show comes before the focus, because a hidden module
    /// is refused the keys. The keys leave before the terminal hides too, because a hidden widget
    /// holding focus leaves the window with keys going nowhere. That half happens inside
    /// [`ToggleStep::Hide`], not as a step of its own.
    pub(crate) fn steps(self) -> &'static [ToggleStep] {
        use ToggleStep::*;
        match self {
            ToggleAction::ShowAndFocus => &[Unzoom, Show, Start, FocusTerminal],
            ToggleAction::Focus => &[Unzoom, FocusTerminal],
            ToggleAction::HideAndReturn => &[Unzoom, Hide],
        }
    }
}

/// Whether the shell's end closes the terminal module (owner, 2026-09-26: "底下终端exit应该是直接关闭
/// terminal窗口"), as tmux closes a pane whose process exits. A real end does -- `exit`, `Ctrl+d`, a
/// signal -- and `main.rs` then kills the module as `prefix x` does, without asking. What does not,
/// and keeps its notice under the last screen with Enter restarting:
/// - a child that left the terminal and still runs (`exec nohup cmd`): it is still this terminal's
///   child, and closing the module would hang it up;
/// - an end with no status (`ExitInfo::UNKNOWN`: the session thread failed, or the child could not
///   be waited for): the pane's frame is the only place that says so.
///
/// A shell that never started (an exec error) is not an end at all -- the pane shows the error in
/// its own frame (`pane::ensure_session`) and never reaches this.
///
/// As the last module on screen, this is not the layout's kill to make (`neovibe_core::layout::kill`
/// refuses the last one, as a hide does): it closes the window instead, tmux's own last-pane rule,
/// through the ordinary close path -- `close window? N running (y/n)` when a tab runs, and `n`
/// leaves everything, this terminal and its notice included ([`OnExit`], task 6, 2026-09-26).
pub(crate) fn closes_on_exit(exit: &ExitInfo) -> bool {
    !exit.detached && (exit.code.is_some() || exit.signal.is_some())
}

/// What a shell's own end does with the module ([`closes_on_exit`]'s `true` case), by
/// `neovibe_core::layout::can_kill`'s answer for it: another module is on screen, so the layout can
/// take this one off it ([`KillScope::Module`]); or it is the last one, so ending it closes the
/// window instead and the module itself is left alone until that really happens
/// ([`KillScope::Window`]) -- `main.rs`'s `on_shell_exit` reads this rather than repeating the
/// match, and its own test pins which scope means which.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OnExit {
    CloseModule,
    CloseWindow,
}

pub(crate) fn on_exit(scope: neovibe_core::layout::KillScope) -> OnExit {
    use neovibe_core::layout::KillScope;
    match scope {
        KillScope::Module => OnExit::CloseModule,
        KillScope::Window => OnExit::CloseWindow,
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

/// The kind of the module holding the keys (`pane_focus::focused_module`), if it is one that can
/// take a literal. Keyed by kind since the modules design's P1 re-homed the terminal: it used to be
/// "pane 2, if the bottom slot holds the terminal" (and pane 0/1 only if the editor/agent had not
/// been replaced by a Lua panel, which a Lua panel no longer does).
pub(crate) fn literal_target(focused: Option<ModuleKind>) -> LiteralTarget {
    match focused {
        Some(ModuleKind::Editor) => LiteralTarget::Editor,
        Some(ModuleKind::Agent) => LiteralTarget::Panel,
        Some(ModuleKind::Terminal) => LiteralTarget::Terminal,
        _ => LiteralTarget::Neither,
    }
}

/// `Ctrl+h/j/k/l` with nothing else held: the chords neovibe takes from the terminal before its own
/// key controller sees them ("neovibe的按键优先"). Claimed even with no module in that direction, so
/// what the shell receives never depends on the layout. `Shift`, `Alt` or `Super` held means it is
/// not one of these, and the terminal gets it.
///
/// **One rule with the web modules' since the modules design's P1 re-homed the terminal**: this is
/// `pane_switch::nav_direction`, which every web module's `Ctrl+h/j/k/l` controller uses. On `main`
/// the agent panel's `Ctrl+j` into the terminal already used this function's exact-Ctrl rule (review
/// 2026-09-23, task-8 minor 3: the inline `key == Key::j` matched CapsLock's `Key::J` never and
/// `Ctrl+Alt+j` always); once one controller asks the geometry for all four chords, the rule is the
/// same for all four.
pub(crate) fn navigation(key: Key, state: ModifierType) -> Option<Direction> {
    crate::pane_switch::nav_direction(key, state)
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
    use crate::module_grid::UNALLOCATED;
    use neovibe_core::layout::{arrange, hide, navigate, Nav, Rect, ZoomChange};

    fn editor() -> ModuleId {
        ModuleId::editor()
    }
    fn agent() -> ModuleId {
        ModuleId::agent()
    }
    fn term() -> ModuleId {
        ModuleId::terminal()
    }
    fn frame() -> Frame<'static> {
        Frame::new(UNALLOCATED, 1)
    }

    /// The module rectangles and the divider rectangles: what is on screen. (A divider's `path`
    /// differs -- `[editor | agent]` is the first child of the terminal's hidden split now -- and a
    /// path is not a pixel.)
    fn on_screen(layout: &Layout) -> (Vec<(ModuleId, Rect)>, Vec<Rect>) {
        let a = arrange(layout, &frame());
        (a.modules, a.dividers.iter().map(|d| d.rect).collect())
    }

    /// Hidden until the first `Ctrl+a t`, and invisible until then: the window is `[editor | agent]`
    /// pixel for pixel, as it was before the terminal existed (the terminal's GUI pass measured
    /// that on `main`; P1's measured `[editor | agent]` against `25724d1`).
    #[test]
    fn the_terminal_starts_hidden_below_the_editor_and_the_agent() {
        let layout = initial_layout(&[]).unwrap();
        assert!(layout.contains(&term()));
        assert!(!layout.is_shown(&term()));
        assert_eq!(layout.visible_leaves(), [editor(), agent()]);
        assert_eq!(layout.focus(), &editor());
        assert_eq!(on_screen(&layout), on_screen(&Layout::initial(&[]).unwrap()));
    }

    /// A Lua `bottom` panel no longer replaces the terminal (on `main` it did, and the terminal
    /// then did not exist in that window): it goes under it, and the terminal is still hidden.
    #[test]
    fn a_lua_bottom_panel_goes_under_the_terminal() {
        let below = ModuleDecl {
            id: ModuleId::lua("below"),
            placement: Placement::BelowRoot,
        };
        let mut layout = initial_layout(&[below]).unwrap();
        assert_eq!(layout.leaves(), [editor(), agent(), term(), ModuleId::lua("below")]);
        assert_eq!(layout.visible_leaves(), [editor(), agent(), ModuleId::lua("below")]);
        layout.show(&term()).unwrap();
        assert_eq!(
            navigate(&layout, &term(), Direction::Down, &frame()),
            Nav::Module(ModuleId::lua("below"))
        );
        assert_eq!(
            navigate(&layout, &ModuleId::lua("below"), Direction::Up, &frame()),
            Nav::Module(term())
        );
    }

    /// `Ctrl+a t`'s show/focus/hide cycle and the keys around it, over the layout the grid
    /// allocates -- the same calls `main.rs` makes through `ModuleGrid` (`show_module` is
    /// `Layout::show`; `hide_module` is `neovibe_core::layout::hide`, then the focus it returns,
    /// then the unmap). What `main`'s `PaneLayout` did for the terminal, each point checked here:
    /// the first show a third of the height (`shown_position(None, h)`); `Ctrl+j` down to it from
    /// the editor's bottom window (`'D'`) and from the panel; `Ctrl+k` back to whichever of the two
    /// above last had the keys (`last_upper_pane`); `Ctrl+h/l/j` from it the edge; hiding it with the
    /// keys handing them up first; a zoom never showing a hidden terminal (`bottom_visible`); and a
    /// second show putting it back where it was left (`bottom_position`).
    #[test]
    fn the_terminal_hides_and_shows_as_ctrl_a_t_did_on_main() {
        let mut layout = initial_layout(&[]).unwrap();
        assert_eq!(navigate(&layout, &editor(), Direction::Down, &frame()), Nav::Nothing);

        assert_eq!(layout.show(&term()), Ok(true));
        let shown = arrange(&layout, &frame());
        // 721px of content, one 1px divider: 480 above it (the paned's 480px start, which
        // `shown_position` made two thirds of the height on `main`), 240 for the terminal.
        assert_eq!(
            shown.rect_of(&term()),
            Some(Rect {
                x: 0,
                y: 481,
                w: 1280,
                h: 240
            })
        );

        assert_eq!(
            navigate(&layout, &editor(), Direction::Down, &frame()),
            Nav::Module(term())
        );
        assert_eq!(
            navigate(&layout, &agent(), Direction::Down, &frame()),
            Nav::Module(term())
        );
        for (above, other) in [(agent(), editor()), (editor(), agent())] {
            layout.set_focus(&other).unwrap();
            layout.set_focus(&above).unwrap();
            layout.set_focus(&term()).unwrap();
            assert_eq!(
                navigate(&layout, &term(), Direction::Up, &frame()),
                Nav::Module(above.clone()),
                "Ctrl+k goes back to the one above that last had the keys"
            );
        }
        for edge in [Direction::Left, Direction::Right, Direction::Down] {
            assert_eq!(navigate(&layout, &term(), edge, &frame()), Nav::Nothing, "{edge:?}");
        }

        // The user drags the divider; `Ctrl+a t` with the keys in the terminal hides it, and the
        // keys go up first -- to the agent, which had them last above it.
        let divider = shown.dividers.iter().find(|d| d.path.is_empty()).unwrap().clone();
        layout.set_ratio(&divider.path, divider.ratio_for(400)).unwrap();
        let dragged = arrange(&layout, &frame()).rect_of(&term());
        assert_ne!(dragged, shown.rect_of(&term()), "the drag moved it");
        layout.set_focus(&agent()).unwrap();
        layout.set_focus(&term()).unwrap();
        assert_eq!(hide(&mut layout, &term(), &frame()), Ok(Some(agent())));
        assert_eq!(layout.focus(), &agent());
        assert_eq!(layout.visible_leaves(), [editor(), agent()]);
        assert_eq!(navigate(&layout, &agent(), Direction::Down, &frame()), Nav::Nothing);

        // No zoom brings it back.
        assert_eq!(layout.toggle_zoom(&editor()), ZoomChange::Zoomed);
        assert_eq!(layout.toggle_zoom(&editor()), ZoomChange::Unzoomed);
        assert!(!layout.is_shown(&term()));

        // Shown again: where it was left.
        assert_eq!(layout.show(&term()), Ok(true));
        assert_eq!(arrange(&layout, &frame()).rect_of(&term()), dragged);
    }

    /// `main`'s review M3 (`hiding_bottom_must_unzoom`): hiding the terminal while it is itself the
    /// zoomed module must leave something on screen. `ToggleStep::Unzoom` comes first anyway; this
    /// is the layout holding it without that step, as a second caller (P2's `Ctrl+a x`) will need.
    #[test]
    fn hiding_the_zoomed_terminal_ends_the_zoom_and_hands_the_keys_up() {
        let mut layout = initial_layout(&[]).unwrap();
        layout.show(&term()).unwrap();
        layout.set_focus(&term()).unwrap();
        assert_eq!(layout.toggle_zoom(&term()), ZoomChange::Zoomed);
        assert_eq!(layout.visible_leaves(), [term()]);
        assert_eq!(hide(&mut layout, &term(), &frame()), Ok(Some(editor())));
        assert_eq!(layout.zoomed(), None);
        assert_eq!(layout.visible_leaves(), [editor(), agent()]);
        assert_eq!(layout.focus(), &editor());
    }

    /// A Lua `main` panel hides the editor; the terminal still goes below the row, and `Ctrl+k`
    /// from it reaches the Lua panel (on `main`, `return_to_upper_pane`'s index 0).
    #[test]
    fn with_a_lua_main_panel_the_keys_go_up_to_it() {
        let main_panel = ModuleDecl {
            id: ModuleId::lua("m"),
            placement: Placement::InPlaceOfEditor,
        };
        let mut layout = initial_layout(&[main_panel]).unwrap();
        assert_eq!(layout.focus(), &ModuleId::lua("m"));
        layout.show(&term()).unwrap();
        layout.set_focus(&term()).unwrap();
        assert_eq!(
            navigate(&layout, &term(), Direction::Up, &frame()),
            Nav::Module(ModuleId::lua("m"))
        );
        assert_eq!(hide(&mut layout, &term(), &frame()), Ok(Some(ModuleId::lua("m"))));
    }

    #[test]
    fn a_real_end_closes_the_terminal_and_a_detach_or_an_unknown_end_does_not() {
        let exit = |code, signal, detached| ExitInfo { code, signal, detached };
        assert!(closes_on_exit(&exit(Some(0), None, false)), "exit");
        assert!(closes_on_exit(&exit(Some(130), None, false)), "exit 130");
        assert!(closes_on_exit(&exit(None, Some(9), false)), "kill -9");
        assert!(!closes_on_exit(&exit(None, None, true)), "exec nohup: still running");
        assert!(!closes_on_exit(&ExitInfo::UNKNOWN), "the session thread failed");
    }

    #[test]
    fn on_exit_reads_can_kills_scope() {
        use neovibe_core::layout::KillScope;
        assert_eq!(on_exit(KillScope::Module), OnExit::CloseModule);
        assert_eq!(on_exit(KillScope::Window), OnExit::CloseWindow);
    }

    #[test]
    fn ctrl_a_t_shows_then_focuses_then_hides() {
        assert_eq!(toggle_action(false, false), ToggleAction::ShowAndFocus);
        assert_eq!(toggle_action(true, false), ToggleAction::Focus);
        assert_eq!(toggle_action(true, true), ToggleAction::HideAndReturn);
    }

    /// `Ctrl+a z` in the terminal, then `Ctrl+a t`: without the unzoom, nothing was left visible.
    /// And the terminal is shown before it is given the keys.
    ///
    /// This test was `every_toggle_unzooms_first_and_focus_leaves_before_the_terminal_hides` until
    /// Task 11's review (finding 1). Its `FocusAbove`-before-`Hide` assertion went with that step.
    /// The keys leaving before the unmap is tested where it happens:
    /// `module_grid::tests::hiding_the_module_with_the_keys_gives_them_away_before_it_unmaps`.
    #[test]
    fn every_toggle_unzooms_first_and_shows_before_it_focuses() {
        for action in [
            ToggleAction::ShowAndFocus,
            ToggleAction::Focus,
            ToggleAction::HideAndReturn,
        ] {
            assert_eq!(action.steps().first(), Some(&ToggleStep::Unzoom), "{action:?}");
        }
        let show = ToggleAction::ShowAndFocus.steps();
        let at = |step| show.iter().position(|s| *s == step);
        assert!(
            at(ToggleStep::Show) < at(ToggleStep::FocusTerminal),
            "a hidden widget cannot take focus"
        );
    }

    /// Fix round 1, review finding 1 (the terminal's own plan): the ordering assertion above passes
    /// even with `Show` deleted outright. `at(step)` is `Option<usize>`, and Rust orders
    /// `None < Some(_)`, so a missing step's position compares less than a present one's and
    /// `at(missing) < at(present)` reads `true`. Demonstrated: with
    /// `ShowAndFocus => &[Unzoom, Start, FocusTerminal]` (no `Show`), the test above still passes.
    /// Pinning each sequence whole closes that hole: a missing or reordered step fails the
    /// `assert_eq!` directly, with no `Option` ordering involved. (The same demonstration used to
    /// delete `FocusAbove` from `HideAndReturn` as well. That step is gone since Task 11's review
    /// of modules P1, finding 1; see [`ToggleStep::Hide`].)
    ///
    /// This holds the list `main.rs` walks, and nothing more. `Hide`'s own order, the keys first and
    /// then the unmap, is `module_grid::hide_then_unmap`'s test.
    #[test]
    fn each_actions_full_step_sequence_is_pinned() {
        use ToggleStep::*;
        assert_eq!(
            ToggleAction::ShowAndFocus.steps(),
            &[Unzoom, Show, Start, FocusTerminal]
        );
        assert_eq!(ToggleAction::Focus.steps(), &[Unzoom, FocusTerminal]);
        assert_eq!(ToggleAction::HideAndReturn.steps(), &[Unzoom, Hide]);
    }

    #[test]
    fn ctrl_a_ctrl_a_reaches_the_terminal_only_when_it_holds_the_keys() {
        assert_eq!(literal_target(Some(ModuleKind::Terminal)), LiteralTarget::Terminal);
        assert_eq!(
            literal_target(Some(ModuleKind::LuaWebview)),
            LiteralTarget::Neither,
            "a Lua panel, bottom or not"
        );
        assert_eq!(literal_target(Some(ModuleKind::Editor)), LiteralTarget::Editor);
        assert_eq!(literal_target(Some(ModuleKind::Agent)), LiteralTarget::Panel);
        assert_eq!(literal_target(Some(ModuleKind::Canvas)), LiteralTarget::Neither);
        assert_eq!(literal_target(None), LiteralTarget::Neither, "the top bar");
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
