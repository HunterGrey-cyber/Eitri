//! `prefix x` (`module.kill`) removes the module from the screen and has `shell` end
//! what ran in it. The layout determines where it can reopen.
//!
//! `Reopen::At` returns the leaf to its first-launch place, hidden. The terminal goes
//! below everything at a third of the height; a Lua panel uses its `position`.
//! The old pinned split is removed; the new one settles its length on the next show.
//!
//! `Reopen::InPlace` leaves the leaf hidden where it was. The editor and agent use this:
//! every layout must contain both, and they form the first-launch root.
//!
//! `Reopen::Never` leaves the leaf hidden and gone for this window (`Layout::is_gone`):
//! no show, placement or tray chip. After nvim quits, a second `nvim --embed` cannot start
//! in this process because `LiveHarness` creates an event loop that can be created only
//! once per process.
//! Gone is not saved; the leaf is saved as hidden and reopens hidden on relaunch.
//!
//! Focus moves first, as for a hide (`super::hide`). `kill` refuses the last shown module;
//! `KillScope::Window` tells `shell` to close the window instead.

use super::geometry::{hide, Frame};
use super::module::{ModuleId, Placement};
use super::tree::{place_new, Layout, LayoutError};

/// Lua typed into nvim to quit the editor. A single `<Cmd>lua` line keeps a cancelled
/// prompt in the previous mode. `generation` is bumped by `shell` before each send
/// (`kill_pane::QuitInFlight`) to distinguish a cancellation from a later quit.
///
/// Use exactly one `<Cmd>...<CR>` with no other `<` inside the Lua source. A second
/// command queued after `:confirm qall` in the same `send_keys` call would be read as
/// the prompt's answer. `nvim_input` also parses `<...>` key notation inside the source.
/// The key-notation literal lives here because `shell` sources contain none.
///
/// `pcall(vim.cmd, 'confirm qall')` returns only if nvim stays alive, whether the quit was
/// cancelled, interrupted with `Ctrl+c`, or stopped by an error in `QuitPre`/`ExitPre`.
/// Only then is the generation sent on the existing pane-switch socket. Connection,
/// write and close use `pcall`, as in `core/src/theme/nvim_theme.lua`, so a failed write
/// does not raise E5108 in the editor.
pub fn editor_quit_keys(generation: u32) -> String {
    format!(
        "<Cmd>lua pcall(vim.cmd, 'confirm qall') \
         local p = os.getenv('NEOVIBE_PANE_SWITCH_SOCKET') \
         if p then local ok, c = pcall(vim.fn.sockconnect, 'pipe', p, {{rpc = false}}) \
         if ok and c ~= 0 then pcall(vim.fn.chansend, c, 'Q {generation}\\n') pcall(vim.fn.chanclose, c) end end<CR>"
    )
}

/// Where a killed module is left (the module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reopen {
    At(Placement),
    InPlace,
    Never,
}

/// What killing a module ends. Killing the last shown module closes the window;
/// otherwise it removes only that module.
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
        Reopen::Never => layout.retire(id),
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
    fn the_editor_quit_is_one_cmd_line_carrying_its_generation() {
        let keys = editor_quit_keys(7);
        assert!(keys.starts_with("<Cmd>lua "), "{keys}");
        assert!(keys.ends_with("<CR>"), "{keys}");
        assert!(keys.contains("confirm qall"), "{keys}");
        assert!(keys.contains(r"'Q 7\n'"), "{keys}");
        assert_eq!(keys.matches('<').count(), 2, "only <Cmd> and <CR>: {keys}");
        assert_ne!(editor_quit_keys(7), editor_quit_keys(8));
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
        // Still a hidden leaf, which is what the state file writes.
        assert!(layout.contains(&editor()));
        assert!(layout.hidden().contains(&editor()));
    }
}
