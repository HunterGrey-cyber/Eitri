//! The bottom terminal's GTK half. The engine handles the PTY, session thread, emulator
//! and painting without GTK.
//!
//! This module holds [`TerminalPane`] and pure functions for the terminal's own behavior:
//! `Ctrl+a t`, literal `Ctrl+a Ctrl+a`, app-reserved chords and terminal colours.
//! These functions can be tested without a display.
//!
//! `ModuleId::terminal()` is placed below the editor's own column and stays hidden until
//! the first `Ctrl+a t`. A Lua `bottom` panel goes under everything without replacing it.
//! The layout and `ModuleGrid` control placement, visibility, focus after hiding and
//! `Ctrl+h/j/k/l` destinations. This module keeps the terminal-specific behavior.

mod bell;
mod gl;
mod ime;
mod input_queue;
// Whether a terminal's shell is running a job, asked when the window closes.
pub(crate) mod jobs;
pub(crate) mod keys;
mod pane;
pub(crate) mod pointer;
// The window's terminals are kept in this registry.
pub(crate) mod registry;

pub(crate) use pane::TerminalPane;

use eitri_core::layout::{Frame, Layout, LayoutError, ModuleDecl, ModuleId, ModuleKind, Placement};
use eitri_core::theme::ThemeTokens;
use eitri_terminal::{ExitInfo, TerminalColors};
use gtk4::gdk::{Key, ModifierType};
use terminal_render::RgbColor;

use crate::layout::Direction;
use std::path::{Path, PathBuf};

/// Keeps a directory read from `/proc/<pid>/cwd` only if it still is one. The kernel names a directory that has
/// been removed `"<path> (deleted)"`, which no shell can start in; every shell candidate would fail on it, and a
/// retry would read the same path again. Stats the path, so it runs off the GTK thread with the read.
pub(crate) fn usable_dir(read: std::io::Result<PathBuf>) -> std::io::Result<PathBuf> {
    let dir = read?;
    match std::fs::metadata(&dir) {
        Ok(meta) if meta.is_dir() => Ok(dir),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "the shell's directory is gone",
        )),
    }
}

pub(crate) fn start_dir(read: std::io::Result<PathBuf>, root: &Path) -> (PathBuf, Option<String>) {
    match read {
        Ok(dir) => (dir, None),
        Err(error) => (
            root.to_path_buf(),
            Some(format!(
                "Could not read the shell's directory: {error}; using {}",
                root.display()
            )),
        ),
    }
}

/// Whether an exit report belongs to the pane's current session.
pub(crate) fn exit_is_current(reported: u64, current: u64) -> bool {
    reported == current
}

/// Builds the first-launch layout with the terminal below the editor's own leaf, using
/// `Placement::BelowEditor`, then adds Lua panels in registration order. The agent and Lua
/// `side` panels keep the full window height. The terminal is hidden before the layout is
/// built into widgets, so it appears only after `Ctrl+a t`. Its hidden split collapses to
/// the editor's leaf and leaves the visible layout unchanged.
///
/// The terminal is always exactly the editor's width. In the default 1280px window, its
/// first show is 760px wide and a third of the editor column's height. A Lua `side` panel
/// reduces that width along with the editor's share. A Lua `bottom` panel uses
/// `Placement::BelowRoot` and wraps the whole tree, including the editor's nested terminal
/// and the agent. The two vertical splits compound, so the terminal's first show under a
/// `bottom` panel is 160px high rather than 240px. A `Row` split does not change height.
/// Either Lua panel can receive focus when the terminal hides if it held the keys more
/// recently than the editor or agent.
///
/// Hides the terminal through `layout::hide`, which chooses a focus destination before
/// hiding a focused module. At startup the editor or a Lua `main` panel holds the keys,
/// so hiding the terminal reads no geometry and the supplied frame is only a placeholder.
///
/// `decls` contains the window's declarations. If an open rule already declares a terminal
/// placement, that placement is used. Only the base terminal and Lua panels intended for
/// first launch reach `Layout::initial`. A chat view or numbered instance is placed later;
/// its declaration only supplies its placement. `Layout::initial` rejects placing a module twice.
pub(crate) fn initial_layout(decls: &[ModuleDecl]) -> Result<Layout, LayoutError> {
    let mut all = Vec::with_capacity(decls.len() + 1);
    if !decls.iter().any(|d| d.id == ModuleId::terminal()) {
        all.push(ModuleDecl {
            id: ModuleId::terminal(),
            placement: Placement::BelowEditor,
        });
    }
    all.extend_from_slice(decls);
    let mut layout = Layout::initial(&eitri_core::layout::first_launch_decls(&all))?;
    eitri_core::layout::hide(
        &mut layout,
        &ModuleId::terminal(),
        &Frame::new(crate::module_grid::UNALLOCATED, 1),
    )?;
    Ok(layout)
}

/// What `Ctrl+a t` does.
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
    /// `ModuleGrid::hide_module` transfers focus before unmapping a focused terminal.
    /// It chooses the adjacent module that most recently had focus, which may be the editor,
    /// agent, a Lua `bottom` panel below it, or a Lua `side` panel beside it.
    /// `module_grid::hide_then_unmap` implements and tests that order.
    Hide,
}

impl ToggleAction {
    /// Ordered toggle actions. Unzoom first so hiding a zoomed terminal leaves visible panes
    /// and a visible focus destination. Show before focusing, since a hidden module cannot
    /// receive the keys. Move focus before hiding the terminal, since a hidden widget holding
    /// focus leaves the window with keys going nowhere. That move happens inside
    /// [`ToggleStep::Hide`], rather than as a separate step.
    pub(crate) fn steps(self) -> &'static [ToggleStep] {
        use ToggleStep::*;
        match self {
            ToggleAction::ShowAndFocus => &[Unzoom, Show, Start, FocusTerminal],
            ToggleAction::Focus => &[Unzoom, FocusTerminal],
            ToggleAction::HideAndReturn => &[Unzoom, Hide],
        }
    }
}

/// Whether the shell's end closes the terminal module,
/// as tmux closes a pane whose process exits. A real end does -- `exit`, `Ctrl+d`, a
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
/// As the last module on screen, this is not the layout's kill to make (`eitri_core::layout::kill`
/// refuses the last one, as a hide does): it closes the window instead, tmux's own last-pane rule,
/// through the ordinary close path -- `close window? N running (y/n)` when a tab runs, and `n`
/// leaves everything, this terminal and its notice included ([`OnExit`]).
pub(crate) fn closes_on_exit(exit: &ExitInfo) -> bool {
    !exit.detached && (exit.code.is_some() || exit.signal.is_some())
}

/// What a shell's own end does with the module ([`closes_on_exit`]'s `true` case), by
/// `eitri_core::layout::can_kill`'s answer for it: another module is on screen, so the layout can
/// take this one off it ([`KillScope::Module`]); or it is the last one, so ending it closes the
/// window instead and the module itself is left alone until that really happens
/// ([`KillScope::Window`]) -- `main.rs`'s `on_shell_exit` reads this rather than repeating the
/// match, and its own test pins which scope means which.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OnExit {
    CloseModule,
    CloseWindow,
}

pub(crate) fn on_exit(scope: eitri_core::layout::KillScope) -> OnExit {
    use eitri_core::layout::KillScope;
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

/// Routes a literal by the kind of module holding the keys (`pane_focus::focused_module`).
/// The editor, agent and terminal can receive it. Lua panels and other focus targets cannot.
pub(crate) fn literal_target(focused: Option<ModuleKind>) -> LiteralTarget {
    match focused {
        Some(ModuleKind::Editor) => LiteralTarget::Editor,
        Some(ModuleKind::Agent) => LiteralTarget::Panel,
        Some(ModuleKind::Terminal) => LiteralTarget::Terminal,
        _ => LiteralTarget::Neither,
    }
}

/// `Ctrl+h/j/k/l` with no other modifiers are claimed before the terminal's key controller.
/// They are consumed even with no destination in that direction, so the shell's input
/// never depends on the layout. With `Shift`, `Alt`, `Super` or `Meta` held, these chords
/// are left to the terminal.
///
/// `pane_switch::nav_direction` supplies the same modifier and direction handling used by
/// the web modules, keeping all four navigation chords consistent.
pub(crate) fn navigation(key: Key, state: ModifierType) -> Option<Direction> {
    crate::pane_switch::nav_direction(key, state)
}

/// `prefix [`/`prefix PageUp`: the terminal's own copy mode asks exactly the
/// same question [`literal_target`] already answers for a literal chord -- does this pane hold the
/// keys right now -- so this names that check rather than adding a second rule beside it.
pub(crate) fn copy_mode_entry(focused: Option<ModuleKind>) -> bool {
    matches!(literal_target(focused), LiteralTarget::Terminal)
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
    let rgb = |c: eitri_core::theme::color::Rgb| RgbColor::new(c.r, c.g, c.b);
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
    use eitri_core::layout::{arrange, hide, navigate, Nav, Rect, ZoomChange};

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

    /// Returns the module and divider rectangles that are on screen. The editor is the first
    /// child of the terminal's hidden split, so divider paths can differ even when the visible
    /// rectangles are identical.
    fn on_screen(layout: &Layout) -> (Vec<(ModuleId, Rect)>, Vec<Rect>) {
        let a = arrange(layout, &frame());
        (a.modules, a.dividers.iter().map(|d| d.rect).collect())
    }

    /// The terminal stays hidden until the first `Ctrl+a t`. Until then, the window's module
    /// and divider rectangles match the `[editor | agent]` layout pixel for pixel.
    /// While hidden below the editor, the terminal's pinned split collapses to the editor's
    /// leaf alone.
    #[test]
    fn the_terminal_starts_hidden_below_the_editor() {
        let layout = initial_layout(&[]).unwrap();
        assert!(layout.contains(&term()));
        assert!(!layout.is_shown(&term()));
        assert_eq!(layout.visible_leaves(), [editor(), agent()]);
        assert_eq!(layout.focus(), &editor());
        assert_eq!(on_screen(&layout), on_screen(&Layout::initial(&[]).unwrap()));
    }

    /// A Lua `bottom` panel goes under the whole layout, including the terminal's place below
    /// the editor. It does not replace the terminal, which remains hidden until shown.
    #[test]
    fn a_lua_bottom_panel_goes_under_the_terminal() {
        let below = ModuleDecl {
            id: ModuleId::lua("below"),
            placement: Placement::BelowRoot,
        };
        let mut layout = initial_layout(&[below]).unwrap();
        // The terminal is now nested inside the editor's own leaf, so it comes right after it in
        // tree order -- before the agent, not after.
        assert_eq!(layout.leaves(), [editor(), term(), agent(), ModuleId::lua("below")]);
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
        // The outer `bottom` panel's own two-thirds split and the terminal's own nested one compound
        // (a `Row` split never touches height): 160px, not 240, and still only the editor's own
        // width, 760 of 1280, not full width.
        let rect = arrange(&layout, &frame()).rect_of(&term()).unwrap();
        assert_eq!((rect.w, rect.h), (760, 160));
    }

    /// Checks `Ctrl+a t`'s show, focus and hide cycle using the layout allocated by the grid.
    /// `ModuleGrid` shows through `Layout::show` and hides through `layout::hide`, applies the
    /// returned focus destination, then unmaps the terminal.
    /// The first show is a third of the editor column's height and exactly the editor's width.
    /// `Ctrl+j` reaches it from the editor; the agent has no neighbour below. From the terminal,
    /// `Ctrl+k` reaches the editor and `Ctrl+l` reaches the full-height agent column. Hiding a
    /// focused terminal returns the keys to the more recently focused geometric neighbour,
    /// using the `Up` and `Right` candidates. Zooming does not reveal a hidden terminal, and
    /// showing it again restores its previous position.
    #[test]
    fn the_terminal_hides_and_shows_as_ctrl_a_t_did_on_main() {
        let mut layout = initial_layout(&[]).unwrap();
        assert_eq!(navigate(&layout, &editor(), Direction::Down, &frame()), Nav::Nothing);

        assert_eq!(layout.show(&term()), Ok(true));
        let shown = arrange(&layout, &frame());
        // 721px of content, one 1px divider: 480 above it (the paned's 480px start, which
        // `shown_position` made two thirds of the height on `main`), 240 for the terminal -- but
        // only 760 of 1280px wide, the editor's own share, not the whole window.
        assert_eq!(
            shown.rect_of(&term()),
            Some(Rect {
                x: 0,
                y: 481,
                w: 760,
                h: 240
            })
        );

        assert_eq!(
            navigate(&layout, &editor(), Direction::Down, &frame()),
            Nav::Module(term())
        );
        assert_eq!(
            navigate(&layout, &agent(), Direction::Down, &frame()),
            Nav::Nothing,
            "the terminal sits beside the agent now, not under it"
        );
        assert_eq!(
            navigate(&layout, &term(), Direction::Up, &frame()),
            Nav::Module(editor()),
            "Ctrl+k always reaches the editor: the only module that shares the terminal's own edge above it"
        );
        assert_eq!(
            navigate(&layout, &term(), Direction::Right, &frame()),
            Nav::Module(agent()),
            "Ctrl+l reaches the agent, which runs the whole column's height beside the editor and the terminal"
        );
        for edge in [Direction::Left, Direction::Down] {
            assert_eq!(navigate(&layout, &term(), edge, &frame()), Nav::Nothing, "{edge:?}");
        }

        // `hide`'s own MRU tie-break still gives the keys to whichever of the editor or the agent
        // was more recently focused, even though only one of them (the agent) reaches the terminal
        // via a single `navigate` query -- `hide` merges every direction's neighbour, not just one.
        for (above, other) in [(agent(), editor()), (editor(), agent())] {
            layout.set_focus(&other).unwrap();
            layout.set_focus(&above).unwrap();
            layout.set_focus(&term()).unwrap();
            assert_eq!(
                hide(&mut layout, &term(), &frame()),
                Ok(Some(above.clone())),
                "hide gives the keys to whichever of the two was focused last"
            );
            layout.show(&term()).unwrap();
        }

        // The user drags the divider; `Ctrl+a t` with the keys in the terminal hides it, and the
        // keys go up first -- to the agent, which had them last above it. The terminal's own pinned
        // divider is not the root container's (that one separates the editor and the agent): it is
        // the one Column-axis divider, nested one level in.
        let divider = shown
            .dividers
            .iter()
            .find(|d| d.axis == eitri_core::layout::Axis::Column)
            .unwrap()
            .clone();
        layout.set_ratio(&divider, divider.ratio_for(400)).unwrap();
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

    /// Hiding a zoomed terminal must end the zoom, leave visible panes and transfer focus.
    /// The layout must handle this even when the caller has not run `ToggleStep::Unzoom`.
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
        use eitri_core::layout::KillScope;
        assert_eq!(on_exit(KillScope::Module), OnExit::CloseModule);
        assert_eq!(on_exit(KillScope::Window), OnExit::CloseWindow);
    }

    #[test]
    fn ctrl_a_t_shows_then_focuses_then_hides() {
        assert_eq!(toggle_action(false, false), ToggleAction::ShowAndFocus);
        assert_eq!(toggle_action(true, false), ToggleAction::Focus);
        assert_eq!(toggle_action(true, true), ToggleAction::HideAndReturn);
    }

    /// Every toggle unzooms first so hiding a zoomed terminal leaves content visible.
    /// The terminal must also be shown before it receives focus.
    /// Focus transfer before unmapping is tested where it happens:
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

    /// The ordering assertion above passes even if `Show` is missing. `at(step)` returns
    /// `Option<usize>`, and Rust orders `None < Some(_)`, so a missing step can compare before
    /// a present step. `ShowAndFocus => &[Unzoom, Start, FocusTerminal]` therefore passes it.
    /// Comparing each complete sequence with `assert_eq!` catches missing and reordered steps
    /// without relying on `Option` ordering.
    ///
    /// This checks the list `main.rs` executes. Focus transfer before unmapping happens inside
    /// `Hide` and is tested by `module_grid::hide_then_unmap`.
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
    fn copy_mode_entry_is_exactly_holding_the_keys() {
        assert!(copy_mode_entry(Some(ModuleKind::Terminal)));
        assert!(!copy_mode_entry(Some(ModuleKind::Editor)));
        assert!(!copy_mode_entry(Some(ModuleKind::Agent)));
        assert!(!copy_mode_entry(Some(ModuleKind::LuaWebview)));
        assert!(!copy_mode_entry(Some(ModuleKind::Canvas)));
        assert!(!copy_mode_entry(None), "the top bar");
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
        // the case above never reached `.to_lower()` at all.
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

#[cfg(test)]
mod start_dir_tests {
    use super::*;
    use std::io::{Error, ErrorKind};
    use std::path::{Path, PathBuf};

    #[test]
    fn the_start_directory_falls_back_to_the_project_root() {
        let root = Path::new("/project");
        assert_eq!(
            start_dir(Ok(PathBuf::from("/tmp")), root),
            (PathBuf::from("/tmp"), None)
        );
        let error = Error::from(ErrorKind::NotFound);
        let message = error.to_string();
        let (dir, notice) = start_dir(Err(error), root);
        assert_eq!(dir, root);
        assert!(notice.unwrap().contains(&message));
    }

    /// A fresh directory of this test's own under the system's scratch directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("eitri-start-dir-{}-{label}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("a scratch directory");
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_directory_that_is_gone_is_not_a_start_directory() {
        let scratch = Scratch::new("gone");
        let gone = scratch.0.join("gone");
        std::fs::create_dir(&gone).unwrap();
        std::fs::remove_dir(&gone).unwrap();
        let deleted = PathBuf::from(format!("{} (deleted)", gone.display()));
        assert!(
            usable_dir(Ok(deleted.clone())).is_err(),
            "the kernel's name for a removed cwd"
        );
        assert!(usable_dir(Ok(gone.clone())).is_err(), "a path with nothing there");
        let file = scratch.0.join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(usable_dir(Ok(file)).is_err(), "a file is not a directory");
        let named = scratch.0.join("x (deleted)");
        std::fs::create_dir(&named).unwrap();
        assert_eq!(
            usable_dir(Ok(named.clone())).unwrap(),
            named,
            "a directory really named so"
        );
        assert!(usable_dir(Err(Error::from(ErrorKind::PermissionDenied))).is_err());

        let root = Path::new("/project");
        let (dir, note) = start_dir(usable_dir(Ok(deleted)), root);
        assert_eq!(dir, root);
        assert!(note.expect("a note says why").contains("gone"));
    }
}
