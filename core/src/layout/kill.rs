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
//! Focus moves first, as for a hide (`super::hide`). Killing the last shown module
//! is refused, just as hiding it is.

use super::geometry::{hide, Frame};
use super::module::{ModuleId, Placement};
use super::tree::{place_new, Layout, LayoutError};

/// What `shell` types into nvim to kill the editor: `:confirm qall`, so unsaved buffers get nvim's
/// own prompt, through `<Cmd>` so a cancelled prompt leaves nvim in the mode it was in. Here rather
/// than in `shell`, whose sources hold no key-notation literal (its
/// `shell_src_writes_no_accelerator_literal`).
pub const EDITOR_QUIT_KEYS: &str = "<Cmd>confirm qall<CR>";

/// Where a killed module is left (the module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reopen {
    At(Placement),
    InPlace,
    Never,
}

/// Whether [`kill`] would take `id`, changing nothing: what `shell` asks before its y/n, so a kill
/// the layout will refuse is refused at once rather than after the question.
pub fn can_kill(layout: &Layout, id: &ModuleId) -> Result<(), LayoutError> {
    if !layout.contains(id) {
        return Err(LayoutError::NotInTree(id.clone()));
    }
    if layout.is_gone(id) {
        return Err(LayoutError::Gone(id.clone()));
    }
    let others_shown = layout.leaves().iter().any(|m| m != id && layout.is_shown(m));
    if layout.is_shown(id) && !others_shown {
        return Err(LayoutError::LastVisible(id.clone()));
    }
    Ok(())
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

    #[test]
    fn the_last_module_on_screen_is_refused_and_nothing_changes() {
        let mut layout = Layout::initial(&[]).unwrap();
        kill(&mut layout, &agent(), Reopen::InPlace, &frame()).unwrap();
        let before = layout.clone();
        assert_eq!(can_kill(&layout, &editor()), Err(LayoutError::LastVisible(editor())));
        assert_eq!(can_kill(&layout, &agent()), Ok(()), "hidden, but still killable");
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
    fn the_editor_is_asked_to_confirm_before_it_quits() {
        assert_eq!(EDITOR_QUIT_KEYS, "<Cmd>confirm qall<CR>");
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
