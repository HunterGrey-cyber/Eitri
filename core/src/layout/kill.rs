//! `prefix x` (`module.kill`) removes the module from the screen and has `shell` end
//! what ran in it. The layout determines where it can reopen.
//!
//! `Reopen::At` returns the leaf to its first-launch place, hidden. The terminal goes
//! below the editor's column at a third of its height; a Lua panel uses its `position`.
//! The old pinned container is removed; the new one settles its length on the next show.
//!
//! `Reopen::InPlace` leaves the leaf hidden where it was. The editor and agent use this:
//! every layout must contain both, and they form the first-launch root.
//!
//! `Reopen::Never` leaves the leaf hidden and gone for this window (`Layout::is_gone`):
//! no show, placement or tray chip. After nvim quits, a second `nvim --embed` cannot start
//! in this process because `LiveHarness` creates an event loop that can be created only
//! once per process.
//! Gone is not saved. `Layout::saved_hidden` also excludes hiding caused by retirement,
//! so a module retired while shown reopens shown on relaunch. A module the user hid stays
//! hidden. Saving preserves the user's arrangement, as `kill_pane::Reveal::rehide` does
//! for the quit prompt.
//!
//! Focus moves first, as for a hide (`super::hide`). `kill` refuses the last shown module;
//! `KillScope::Window` tells `shell` to close the window instead.

use super::geometry::{hide, Frame};
use super::module::{ModuleId, Placement};
use super::tree::{place_new, Layout, LayoutError};

/// Lua run by `shell` for `prefix x` on the editor and for window close. `generation`
/// is bumped before each send (`kill_pane::QuitInFlight`) so a cancellation can be
/// distinguished from a later quit of the same editor.
///
/// Send this as an RPC request (`nvim_exec_lua`). Typed `<Cmd>` input can be consumed
/// as a pending character after `f`/`t`/`r`/`m`/`q`/`Ctrl-W`, inside `getchar()`, or after
/// an insert-mode `Ctrl-V`. The remaining Lua can then execute as Normal-mode commands
/// and alter the buffer. A request types nothing and runs when nvim next handles it,
/// which in these states is after the awaited key (`core/tests/editor_quit_with_real_nvim.rs`).
///
/// `pcall(vim.cmd, 'confirm qall')` returns only if nvim stays alive, whether the quit was
/// cancelled, interrupted with `Ctrl+c`, or stopped by an error in `QuitPre`/`ExitPre`.
/// Only then is the generation sent on the existing pane-switch socket. Connection,
/// write and close use `pcall`, as in `core/src/theme/nvim_theme.lua`, so a failed write
/// does not raise E5108 in the editor. The request's response also reports that the chunk
/// returned; `shell` reads it so cancellation is seen even without the socket shim.
pub fn editor_quit_lua(generation: u32) -> String {
    format!(
        "pcall(vim.cmd, 'confirm qall') \
         local p = os.getenv('EITRI_PANE_SWITCH_SOCKET') \
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
    Remove,
}

/// What killing a module ends. Killing the last shown module closes the window;
/// otherwise it removes only that module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillScope {
    /// Another module is on screen: [`kill`] takes this one off it.
    Module,
    /// It is the last module on screen: the kill closes the window, which is the whole of this
    /// Eitri process. The layout is not changed; `shell` closes the window instead.
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
    let others_shown = layout.modules().iter().any(|m| m != id && layout.is_shown(m));
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
    if reopen == Reopen::Remove && id.number() == 1 {
        return Err(LayoutError::NotAnInstance(id.clone()));
    }
    if layout.is_gone(id) {
        return Err(LayoutError::Gone(id.clone()));
    }
    let was_shown = !layout.hidden().contains(id);
    let next = hide(layout, id, frame)?;
    match reopen {
        Reopen::At(placement) => {
            layout.take_float(id);
            let root = layout
                .root_node()
                .without(id)
                .expect("a hide succeeded, so another module is still in the tree");
            // A first launch hides the editor under a Lua `main` panel (`place_new`'s `true`). Not
            // here: the panel is hidden, so hiding the editor too would empty its region, and the
            // editor's visibility is the user's own by now.
            let (root, _took_editors_place) = place_new(root, id, placement);
            layout.replace_root(root);
        }
        Reopen::InPlace => {}
        Reopen::Remove => layout.remove(id)?,
        Reopen::Never => layout.retire(id, was_shown),
    }
    layout.clear_last_preset();
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::super::geometry::{arrange, Size};
    use super::super::module::ModuleDecl;
    use super::super::ops::place;
    use super::super::test_support::same_rects;
    use super::super::tray::tray;
    use super::super::tree::Axis;
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

    /// The initial layout with the terminal below the editor's own leaf,
    /// before anything is hidden.
    fn with_terminal() -> Layout {
        Layout::initial(&[ModuleDecl {
            id: term(),
            placement: Placement::BelowEditor,
        }])
        .unwrap()
    }

    /// A terminal moved right of the editor and killed there comes back where a first launch puts
    /// it -- below the editor's own leaf, its pin fresh, the agent untouched -- and the keys went to
    /// its neighbour first.
    #[test]
    fn a_killed_terminal_goes_back_below_the_editor_hidden_and_the_keys_move_first() {
        let mut layout = with_terminal();
        place(&mut layout, &term(), &editor(), Axis::Row).unwrap();
        assert_eq!(layout.focus(), &term());
        assert_eq!(
            kill(&mut layout, &term(), Reopen::At(Placement::BelowEditor), &frame()),
            Ok(Some(editor()))
        );
        assert_eq!(layout.focus(), &editor());
        assert!(!layout.is_shown(&term()));
        assert!(!layout.is_gone(&term()));
        let mut expected = with_terminal();
        let half = super::super::tree::DEFAULT_EDITOR_SHARE / 2.0;
        expected.root_mut().children[0].share = half / (1.0 - half);
        expected.root_mut().children[1].share = 1.0 - half / (1.0 - half);
        expected.hide_unfocused(&term()).unwrap();
        same_rects(&layout, &expected);
        assert_eq!(layout.visible_leaves(), [editor(), agent()]);
        // Reopening takes a third of the editor's height in the column left after removing the sibling.
        layout.show(&term()).unwrap();
        let rect = arrange(&layout, &frame()).rect_of(&term()).unwrap();
        assert_eq!((rect.w, rect.h), (541, 240));
    }

    /// The initial layout with a Lua `side` panel: `[[editor / term] | agent] | side`.
    /// The terminal goes below the editor's own leaf only.
    fn with_terminal_and_side() -> Layout {
        Layout::initial(&[
            ModuleDecl {
                id: term(),
                placement: Placement::BelowEditor,
            },
            ModuleDecl {
                id: ModuleId::lua("side"),
                placement: Placement::RightOfRoot,
            },
        ])
        .unwrap()
    }

    /// Removing a terminal beside the side panel rescales that row, then reopens the terminal
    /// under the editor alone. The side panel keeps the remaining part of its halved slot.
    #[test]
    fn a_killed_terminal_reopens_below_the_editor_not_the_agent_or_the_side_panel() {
        let mut layout = with_terminal_and_side();
        place(&mut layout, &term(), &ModuleId::lua("side"), Axis::Row).unwrap();
        assert_eq!(
            kill(&mut layout, &term(), Reopen::At(Placement::BelowEditor), &frame()),
            Ok(Some(ModuleId::lua("side")))
        );
        let mut expected = with_terminal_and_side();
        let kept = 0.67 / (0.67 + 0.33 / 2.0);
        expected.root_mut().children[0].share = kept;
        expected.root_mut().children[1].share = 1.0 - kept;
        expected.hide_unfocused(&term()).unwrap();
        same_rects(&layout, &expected);
    }

    /// The pin the bottom row had settled goes with the old container: the next show settles anew.
    #[test]
    fn a_killed_bottom_row_forgets_its_pinned_height() {
        let height_at = |layout: &Layout, h: i32| {
            arrange(layout, &Frame::new(Size { w: 1280, h }, 1))
                .rect_of(&term())
                .unwrap()
                .h
        };
        let mut layout = with_terminal();
        assert!(super::super::geometry::settle_pins(&mut layout, &frame()));
        assert_eq!((height_at(&layout, 721), height_at(&layout, 1041)), (240, 240));
        kill(&mut layout, &term(), Reopen::At(Placement::BelowEditor), &frame()).unwrap();
        assert!(!layout.is_shown(&term()));
        // Shown again it divides by its share, a third of the editor column at any height, as a
        // first launch's unsettled row does.
        layout.show(&term()).unwrap();
        assert_eq!((height_at(&layout, 721), height_at(&layout, 1041)), (240, 347));
        same_rects(&layout, &with_terminal());
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

    #[test]
    fn removing_an_instance_takes_its_leaf_its_mru_entry_and_the_mark() {
        let id = ModuleId::parse("terminal#2").unwrap();
        let mut layout = with_terminal();
        place(&mut layout, &id, &term(), Axis::Row).unwrap();
        layout.set_mark(&id).unwrap();
        let next = kill(&mut layout, &id, Reopen::Remove, &frame()).unwrap().unwrap();
        assert!(layout.is_shown(&next));
        assert!(!layout.contains(&id));
        assert!(!layout.mru().contains(&id));
        assert_eq!(layout.mark(), None);
        assert!(arrange(&layout, &frame()).rect_of(&id).is_none());
        assert_eq!(
            kill(&mut layout, &editor(), Reopen::Remove, &frame()),
            Err(LayoutError::NotAnInstance(editor()))
        );
    }
    #[test]
    fn direct_removal_ends_its_zoom_and_shows_the_editor_if_every_other_leaf_is_hidden() {
        let id = ModuleId::parse("terminal#2").unwrap();
        let mut layout = with_terminal();
        place(&mut layout, &id, &term(), Axis::Row).unwrap();
        layout.toggle_zoom(&id);
        layout.remove(&id).unwrap();
        assert_eq!(layout.zoomed(), None);
        assert_eq!(layout.focus(), &editor());
        assert!(layout.mru().iter().all(|m| layout.contains(m)));
        let mut layout = with_terminal();
        place(&mut layout, &id, &term(), Axis::Row).unwrap();
        for other in [editor(), agent(), term()] {
            hide(&mut layout, &other, &frame()).unwrap();
        }
        layout.remove(&id).unwrap();
        assert_eq!(layout.visible_leaves(), [editor()]);
        assert_eq!(layout.focus(), &editor());
    }

    #[test]
    fn a_base_removal_is_refused_without_changing_the_layout() {
        let mut layout = with_terminal();
        let before = layout.clone();
        let error = LayoutError::NotAnInstance(editor());
        assert_eq!(
            error.to_string(),
            "'editor' is not an instance; it cannot leave the layout"
        );
        assert_eq!(layout.remove(&editor()), Err(error.clone()));
        assert_eq!(kill(&mut layout, &editor(), Reopen::Remove, &frame()), Err(error));
        assert_eq!(layout, before);
    }
}
