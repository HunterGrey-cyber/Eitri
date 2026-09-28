//! `prefix x` (keymap action `module.kill`, tmux's `kill-pane`; owner, 2026-09-26: "prefix x应该是
//! 直接关掉pane而不是隐藏"): the module leaves the screen, as a hide does, and what ran in it is
//! ended by `shell`. The layout's half is here, and it differs from [`super::hide`] in where the
//! module is left for next time:
//!
//! - [`Reopen::At`]: its leaf goes back where a first launch puts it, hidden -- so the next show is
//!   a fresh module in its default place (the terminal below everything at a third of the height,
//!   a Lua panel by its `position`), not the place it was killed from. A pinned split it was in goes
//!   with it; the new one settles its length on its next show, as a first show does.
//! - [`Reopen::InPlace`]: its leaf stays where it was, hidden. The editor and the agent: every
//!   layout has both (`reconcile` refuses one without them), and neither has a first-launch
//!   placement of its own -- they ARE the first launch's root.
//! - [`Reopen::Never`]: hidden where it was, and marked gone for the rest of this window
//!   ([`Layout::is_gone`]): no show, no place, no tray chip. The editor after nvim quit: a second
//!   `nvim --embed` cannot be started in this process, because the Neovide fork's `LiveHarness`
//!   builds a winit `EventLoop`, which winit allows once per process (`EventLoopError::
//!   RecreationAttempt`, winit 0.30.13 `event_loop.rs:118`). Gone is never written to the state
//!   file, and neither is the hide the retirement itself made: a module retired while shown is
//!   saved shown, so a relaunch opens with it where it was ([`Layout::saved_hidden`]; the fix round
//!   after the v1-hardening Task 6/7/9 review, T6-5 -- until then it was saved hidden and a relaunch
//!   opened without the editor until `prefix e`). One the user had hidden stays hidden. The
//!   precedent is this project's own rule for nvim's quit prompt (`shell`'s `kill_pane::Reveal::
//!   rehide`): what the window saves is the user's arrangement, not what a close or a quit did to
//!   it -- and a first launch, which is what `Reopen::At` restores for every other module, shows the
//!   editor.
//!
//! The keys leave first, exactly as a hide's do (`super::hide`). The last module on screen is not the
//! layout's to kill ([`kill`] refuses it, as a hide does): it is the window's ([`KillScope::Window`],
//! 2026-09-26 later), and `shell` closes the window.

use super::geometry::{hide, Frame};
use super::module::{ModuleId, Placement};
use super::tree::{place_new, Layout, LayoutError};

/// The Lua `shell` has nvim run to quit the editor -- `prefix x` on it, and every window close
/// (v1 hardening Task 6) -- carrying `generation`, the quit's own number, bumped by `shell` before
/// each send (`kill_pane::QuitInFlight`), so a cancel can be told apart from a later, different
/// quit of the same editor.
///
/// **Sent as an RPC request (`nvim_exec_lua`), never typed.** Until the v1-hardening Task 6 review
/// it went through `nvim_input` as `<Cmd>lua …<CR>`, and typed keys are read as whatever nvim is
/// waiting for: after `f`/`t`/`r`/`m`/`q`/`Ctrl-W`, inside `getchar()` (flash.nvim, leap) or after an
/// insert-mode `Ctrl-V`, the `<Cmd>` was taken as that one character and the rest ran as Normal-mode
/// commands -- measured on nvim 0.12.5 after `f`: `u` undid the user's last edit, `a` inserted this
/// Lua into the buffer, and nvim was left in Insert mode, neither quitting nor answering. A request
/// types nothing: nvim takes it when its loop next takes one, which in those states is after the key
/// it is waiting for (`core/tests/editor_quit_with_real_nvim.rs`). Stock Neovide's own window close
/// is the same kind of request (its `ParallelCommand::Quit` runs `exit_handler.lua` through
/// `nvim_exec_lua`, `confirm qa` under `g:neovide_confirm_quit`).
///
/// `pcall(vim.cmd, 'confirm qall')` returns only when nvim did NOT exit (a genuine quit ends the
/// process before any Lua after it can run) -- cancelled, interrupted with `Ctrl+c`, or stopped by a
/// user `QuitPre`/`ExitPre` autocommand that errors, all three swallowed by the `pcall` the same way.
/// Only then is the generation written, on the pane-switch socket (`NEOVIBE_PANE_SWITCH_SOCKET`,
/// already set on the nvim child for `vim-tmux-navigator`) rather than a socket of its own -- the
/// Global Constraints forbid adding a new one -- mirroring `core/src/theme/nvim_theme.lua`'s own
/// `send()`: `pcall(vim.fn.sockconnect, 'pipe', p, {rpc = false})`, then a `pcall`-wrapped
/// `chansend` (an unprotected one would raise E5108 in the user's editor on a failed write) and
/// `chanclose`. The request's own response says the same thing (the chunk returned); `shell` reads
/// that too, so a cancel is seen even where the shim is missing.
pub fn editor_quit_lua(generation: u32) -> String {
    format!(
        "pcall(vim.cmd, 'confirm qall') \
         local p = os.getenv('NEOVIBE_PANE_SWITCH_SOCKET') \
         if p then local ok, c = pcall(vim.fn.sockconnect, 'pipe', p, {{rpc = false}}) \
         if ok and c ~= 0 then pcall(vim.fn.chansend, c, 'Q {generation}\\n') pcall(vim.fn.chanclose, c) end end"
    )
}

/// Where a killed module is left (the module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reopen {
    At(Placement),
    InPlace,
    Never,
}

/// What a kill of a module ends (tmux: killing the last pane closes the window, and killing the
/// last window ends tmux; owner, 2026-09-26: "prefix x对neovide窗口不生效，不能触发neovibe关闭").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillScope {
    /// Another module is on screen: [`kill`] takes this one off it.
    Module,
    /// It is the last module on screen: the kill closes the window, which is the whole of this
    /// neovibe process. The layout is not changed; `shell` closes the window instead.
    Window,
}

/// Whether [`kill`] would take `id`, changing nothing: what `shell` asks before its y/n, so a kill
/// that cannot happen is refused at once rather than after the question. The last module on screen
/// is not refused: it is the window's kill ([`KillScope::Window`]), which [`kill`] itself still
/// refuses, since the layout never hides the last module.
pub fn can_kill(layout: &Layout, id: &ModuleId) -> Result<KillScope, LayoutError> {
    if !layout.contains(id) {
        return Err(LayoutError::NotInTree(id.clone()));
    }
    if layout.is_gone(id) {
        return Err(LayoutError::Gone(id.clone()));
    }
    let others_shown = layout.leaves().iter().any(|m| m != id && layout.is_shown(m));
    if layout.is_shown(id) && !others_shown {
        return Ok(KillScope::Window);
    }
    Ok(KillScope::Module)
}

/// Kills `id` in the layout: hides it (the keys go where a hide sends them, returned), then leaves
/// it as `reopen` says. Refused as a hide is: a module not in the tree, or the last one on screen.
pub fn kill(
    layout: &mut Layout,
    id: &ModuleId,
    reopen: Reopen,
    frame: &Frame,
) -> Result<Option<ModuleId>, LayoutError> {
    if layout.is_gone(id) {
        return Err(LayoutError::Gone(id.clone()));
    }
    let was_shown = !layout.hidden().contains(id);
    let next = hide(layout, id, frame)?;
    match reopen {
        Reopen::At(placement) => {
            let root = layout
                .root()
                .clone()
                .without(id)
                .expect("a hide succeeded, so another module is still in the tree");
            // A first launch hides the editor under a Lua `main` panel (`place_new`'s `true`). Not
            // here: the panel is hidden, so hiding the editor too would empty its region, and the
            // editor's visibility is the user's own by now.
            let (root, _took_editors_place) = place_new(root, id, placement);
            layout.replace_root(root);
        }
        Reopen::InPlace => {}
        Reopen::Never => layout.retire(id, was_shown),
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::super::geometry::{arrange, Size};
    use super::super::module::ModuleDecl;
    use super::super::ops::place;
    use super::super::tray::tray;
    use super::super::tree::{Axis, Branch, Node, Pin, BELOW_ROOT_SHARE};
    use super::*;

    fn frame() -> Frame<'static> {
        Frame::new(Size { w: 1280, h: 721 }, 1)
    }
    fn editor() -> ModuleId {
        ModuleId::editor()
    }
    fn agent() -> ModuleId {
        ModuleId::agent()
    }
    fn term() -> ModuleId {
        ModuleId::terminal()
    }

    /// The first launch with the terminal in it, before anything is hidden.
    fn with_terminal() -> Layout {
        Layout::initial(&[ModuleDecl {
            id: term(),
            placement: Placement::BelowRoot,
        }])
        .unwrap()
    }

    /// A terminal moved right of the editor and killed there comes back where a first launch puts
    /// it -- below everything, its pin fresh -- and the keys went to its neighbour first.
    #[test]
    fn a_killed_terminal_goes_back_below_everything_hidden_and_the_keys_move_first() {
        let mut layout = with_terminal();
        place(&mut layout, &term(), &editor(), Axis::Row).unwrap();
        assert_eq!(layout.focus(), &term());
        assert_eq!(
            kill(&mut layout, &term(), Reopen::At(Placement::BelowRoot), &frame()),
            Ok(Some(editor()))
        );
        assert_eq!(layout.focus(), &editor());
        assert!(!layout.is_shown(&term()));
        assert!(!layout.is_gone(&term()));
        assert_eq!(
            layout.root(),
            &Node::pinned(
                Axis::Column,
                BELOW_ROOT_SHARE,
                Branch::Second,
                Layout::initial(&[]).unwrap().root().clone(),
                Node::Leaf(term())
            )
        );
        assert_eq!(layout.visible_leaves(), [editor(), agent()]);
        // Shown again: a third of the height, as a first show is.
        layout.show(&term()).unwrap();
        assert_eq!(arrange(&layout, &frame()).rect_of(&term()).unwrap().h, 240);
    }

    /// The first launch with a Lua `side` panel too, terminal `BelowRoot` before it (task 6's own
    /// equivalence check, brief step 1): `[editor | agent] | side`, terminal below `[editor | agent]`
    /// only.
    fn with_terminal_and_side() -> Layout {
        Layout::initial(&[
            ModuleDecl {
                id: term(),
                placement: Placement::BelowRoot,
            },
            ModuleDecl {
                id: ModuleId::lua("side"),
                placement: Placement::RightOfRoot,
            },
        ])
        .unwrap()
    }

    /// A terminal moved next to the side panel and killed there with `Reopen::At(BelowEditorAndAgent)`
    /// comes back under `[editor | agent]` only, producing exactly the tree a first launch with the
    /// same terminal and side panel builds -- not full width below the side panel too, which
    /// `Reopen::At(BelowRoot)` would give it.
    #[test]
    fn a_killed_terminal_reopens_below_editor_and_agent_not_below_the_side_panel() {
        let mut layout = with_terminal_and_side();
        place(&mut layout, &term(), &ModuleId::lua("side"), Axis::Row).unwrap();
        assert_eq!(
            kill(
                &mut layout,
                &term(),
                Reopen::At(Placement::BelowEditorAndAgent),
                &frame()
            ),
            Ok(Some(ModuleId::lua("side")))
        );
        assert_eq!(layout.root(), with_terminal_and_side().root());
    }

    /// The pin the bottom row had settled goes with the old split: the next show settles anew.
    #[test]
    fn a_killed_bottom_row_forgets_its_pinned_height() {
        let mut layout = with_terminal();
        assert!(super::super::geometry::settle_pins(&mut layout, &frame()));
        let settled = |layout: &Layout| match layout.root() {
            Node::Split {
                pin: Some(Pin { px, .. }),
                ..
            } => *px,
            other => panic!("not the pinned split: {other:?}"),
        };
        assert_eq!(settled(&layout), Some(240));
        kill(&mut layout, &term(), Reopen::At(Placement::BelowRoot), &frame()).unwrap();
        assert_eq!(settled(&layout), None);
        assert!(!layout.is_shown(&term()));
    }

    /// The editor and the agent stay in their leaf; only the hide happens.
    #[test]
    fn in_place_is_a_hide() {
        let mut layout = Layout::initial(&[]).unwrap();
        let before = layout.root().clone();
        layout.set_focus(&agent()).unwrap();
        assert_eq!(
            kill(&mut layout, &agent(), Reopen::InPlace, &frame()),
            Ok(Some(editor()))
        );
        assert_eq!(layout.root(), &before);
        assert_eq!(layout.visible_leaves(), [editor()]);
        assert_eq!(layout.show(&agent()), Ok(true), "it can come back");
    }

    /// tmux: killing the last pane closes the window. The layout never hides the last module, so
    /// `kill` still refuses it; `can_kill` says the kill is the window's instead, and `shell` closes it.
    #[test]
    fn the_last_module_on_screen_is_the_windows_kill_and_the_layout_does_not_change() {
        let mut layout = Layout::initial(&[]).unwrap();
        assert_eq!(can_kill(&layout, &editor()), Ok(KillScope::Module));
        kill(&mut layout, &agent(), Reopen::InPlace, &frame()).unwrap();
        let before = layout.clone();
        assert_eq!(can_kill(&layout, &editor()), Ok(KillScope::Window));
        assert_eq!(
            can_kill(&layout, &agent()),
            Ok(KillScope::Module),
            "hidden, but still killable"
        );
        assert_eq!(
            can_kill(&layout, &ModuleId::lua("ghost")),
            Err(LayoutError::NotInTree(ModuleId::lua("ghost")))
        );
        assert_eq!(
            kill(&mut layout, &editor(), Reopen::Never, &frame()),
            Err(LayoutError::LastVisible(editor()))
        );
        assert_eq!(layout, before);
        assert!(!layout.is_gone(&editor()));
    }

    #[test]
    fn the_editor_quit_is_lua_carrying_its_generation_and_no_key_notation() {
        let lua = editor_quit_lua(7);
        assert!(lua.starts_with("pcall(vim.cmd, 'confirm qall')"), "{lua}");
        assert!(lua.contains(r"'Q 7\n'"), "{lua}");
        assert!(
            !lua.contains("<Cmd>") && !lua.contains("<CR>"),
            "a chunk for nvim_exec_lua, not keys: {lua}"
        );
        assert_ne!(editor_quit_lua(7), editor_quit_lua(8));
    }

    /// Gone: never shown, placed, focused or offered in the tray again in this window.
    #[test]
    fn a_module_that_can_never_come_back_is_refused_everywhere() {
        let mut layout = with_terminal();
        hide(&mut layout, &term(), &frame()).unwrap();
        kill(&mut layout, &editor(), Reopen::Never, &frame()).unwrap();
        assert!(layout.is_gone(&editor()));
        assert_eq!(layout.show(&editor()), Err(LayoutError::Gone(editor())));
        assert_eq!(can_kill(&layout, &editor()), Err(LayoutError::Gone(editor())));
        assert_eq!(
            place(&mut layout, &editor(), &agent(), Axis::Row),
            Err(LayoutError::Gone(editor()))
        );
        assert_eq!(layout.set_focus(&editor()), Err(LayoutError::Hidden(editor())));
        assert_eq!(tray(&layout), [term()], "the terminal's chip, not the editor's");
        assert_eq!(
            kill(&mut layout, &editor(), Reopen::Never, &frame()),
            Err(LayoutError::Gone(editor()))
        );
        // Still a hidden leaf -- and the state file writes it shown, as it was before (T6-5).
        assert!(layout.contains(&editor()));
        assert!(layout.hidden().contains(&editor()));
        assert!(!layout.saved_hidden().contains(&editor()));
        assert!(
            layout.saved_hidden().contains(&term()),
            "the terminal the user hid stays hidden"
        );
    }
}
