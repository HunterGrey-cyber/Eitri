//! The bottom terminal's GTK half.
//! The engine -- PTY, session thread, emulator, paint -- is `eitri-terminal`, which has no GTK.
//!
//! This module holds the widget ([`TerminalPane`]) and the few decisions `main.rs` makes about it,
//! each as a pure function so it is tested without a display: what `Ctrl+a t` does, where
//! `Ctrl+a Ctrl+a` goes, which chords the terminal gives up to Eitri, and its colours.
//!
//! **A module since the modules design.** It is
//! `ModuleId::terminal()`, placed below the editor's own column (since 2026-09-28; not below the
//! editor and the agent together, as it was before -- a Lua `bottom` panel still goes under
//! everything instead of replacing anything) and
//! hidden in the layout until the first `Ctrl+a t`. Where it goes, what shows it, who gets the keys
//! when it hides and where `Ctrl+h/j/k/l` from it lead are the layout's (`eitri_core::layout`,
//! `ModuleGrid`), not a slot index's: this module keeps only the decisions that are the terminal's
//! own.

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

#[cfg(test)]
use eitri_core::layout::{Layout, LayoutError, ModuleDecl};
use eitri_core::theme::ThemeTokens;
use eitri_terminal::{ExitInfo, TerminalColors};
use terminal_render::RgbColor;

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
///
/// Uses `wm::persist::first_launch_layout`, which also supplies the first-launch layout
/// when there is no Lua default. The shell tests start from the same layout.
#[cfg(test)]
pub(crate) fn initial_layout(decls: &[ModuleDecl]) -> Result<Layout, LayoutError> {
    eitri_core::wm::persist::first_launch_layout(decls)
}

/// Whether the shell's end closes the terminal module (decided 2026-09-26: `exit` in the bottom
/// terminal closes the terminal window), as tmux closes a pane whose process exits. A real end does -- `exit`, `Ctrl+d`, a
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
/// leaves everything, this terminal and its notice included ([`OnExit`], task 6, 2026-09-26).
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
    use crate::layout::Direction;
    use crate::module_grid::UNALLOCATED;
    use eitri_core::layout::{arrange, hide, navigate, Frame, ModuleId, Nav, Placement, Rect, ZoomChange};

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

    /// The terminal stays hidden until the first `Ctrl+a t`. Its pinned split collapses to the
    /// editor's own leaf, so the visible window has the same `[editor | agent]` rectangles
    /// as a layout without the terminal.
    #[test]
    fn the_terminal_starts_hidden_below_the_editor() {
        let layout = initial_layout(&[]).unwrap();
        assert!(layout.contains(&term()));
        assert!(!layout.is_shown(&term()));
        assert_eq!(layout.visible_leaves(), [editor(), agent()]);
        assert_eq!(layout.focus(), &editor());
        assert_eq!(on_screen(&layout), on_screen(&Layout::initial(&[]).unwrap()));
    }

    /// The first launch hides the terminal against the same stand-in size the grid computes against before its
    /// first allocation, so the two never disagree about the startup layout.
    #[test]
    fn the_first_launch_frame_is_the_grids_unallocated_size() {
        assert_eq!(eitri_core::wm::persist::FIRST_LAUNCH_FRAME, UNALLOCATED);
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

    /// Hiding the terminal while it is itself the zoomed module must leave something on screen. The
    /// terminal's key ends a zoom first anyway; this is the layout holding it without that, as every
    /// other hide needs.
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
