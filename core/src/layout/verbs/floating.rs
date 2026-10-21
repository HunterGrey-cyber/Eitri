//! Floating, docking and scratchpad operations keep modules in one layer at a time.

use super::path_to;
use crate::layout::con::{Child, ConLayout, Node, MAX_CHILDREN};
use crate::layout::float::{float_floor, Float, FloatRect, DEFAULT_FLOAT_SHARE};
use crate::layout::geometry::{hide, Direction, Frame, Rect};
use crate::layout::tree::{Axis, Layout, LayoutError, MIN_RATIO, RIGHT_OF_ROOT_SHARE};
use crate::layout::ModuleId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatChange {
    Floated,
    Sank,
}

/// Raised and shown modules receive the keys from the window; only stowing moves layout focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScratchShow {
    /// Stowed, with the next owner of the keys when the module held them.
    Stowed(Option<ModuleId>),
    /// A shown scratch float, raised.
    Raised(ModuleId),
    /// A stowed float, shown centred and raised.
    Shown(ModuleId),
    /// An absent instance for the window to create before asking again.
    Missing(ModuleId),
}

/// Toggles a shown module between a float and its most recent tiled anchor.
pub fn float_toggle(layout: &mut Layout, target: &ModuleId, frame: &Frame) -> Result<FloatChange, LayoutError> {
    if *target == ModuleId::editor() {
        return Err(LayoutError::EditorNeverFloats);
    }
    layout.can_focus(target)?;
    if layout.is_floating(target) {
        sink(layout, target)?;
        layout.unzoom();
        return Ok(FloatChange::Sank);
    }
    float_at(layout, target, None, frame)?;
    Ok(FloatChange::Floated)
}

fn sink(layout: &mut Layout, id: &ModuleId) -> Result<(), LayoutError> {
    let anchor = layout.tiled_anchor();
    let root = if let Some(anchor) = anchor {
        let (parent, i) = *path_to(layout.root(), &anchor).last().expect("the anchor is tiled");
        // The anchor's parent may be a root of one child, which `root_node` would hand back as the bare
        // leaf: edit the root container itself so that parent is always there to insert into.
        let mut root_con = layout.root().clone();
        let parent = root_con.find_mut(parent).expect("the parent belongs to the root");
        if parent.children.len() >= MAX_CHILDREN {
            return Err(LayoutError::TooMany { limit: MAX_CHILDREN });
        }
        let half = parent.children[i].share / 2.0;
        parent.children[i].share = half;
        parent.children[i].pin = None;
        parent.children.insert(i + 1, Child::new(Node::Leaf(id.clone()), half));
        if half < MIN_RATIO {
            let share = 1.0 / parent.children.len() as f32;
            for child in &mut parent.children {
                child.share = share;
                if child.node.leaves().iter().any(|m| m == id || layout.is_shown(m)) {
                    child.pin = None;
                }
            }
        }
        if matches!(parent.layout, ConLayout::Tabbed | ConLayout::Stacked) {
            parent.focus = i + 1;
        }
        Node::Con(root_con)
    } else {
        Node::split(
            Axis::Row,
            RIGHT_OF_ROOT_SHARE,
            layout.root_node(),
            Node::Leaf(id.clone()),
        )
    };
    layout.take_float(id);
    layout.replace_root(root);
    layout.set_focus(id)
}

/// Translates a float inside the grid without changing its remembered size.
pub fn move_float(layout: &mut Layout, id: &ModuleId, dir: Direction, px: i32, frame: &Frame) -> bool {
    if frame.size.w <= 0 || frame.size.h <= 0 {
        return false;
    }
    let Some(float) = layout.float_of(id) else { return false };
    let stored = float.rect;
    let now = stored.place(frame.size, float_floor(id, frame));
    let mut moved = now;
    match dir {
        Direction::Left => moved.x = moved.x.saturating_sub(px),
        Direction::Right => moved.x = moved.x.saturating_add(px),
        Direction::Up => moved.y = moved.y.saturating_sub(px),
        Direction::Down => moved.y = moved.y.saturating_add(px),
    }
    moved.x = moved.x.clamp(0, frame.size.w - moved.w);
    moved.y = moved.y.clamp(0, frame.size.h - moved.h);
    if moved == now {
        return false;
    }
    let rect = FloatRect {
        w: stored.w,
        h: stored.h,
        ..FloatRect::of_rect(moved, frame.size)
    };
    layout.store_float_rect(id, rect);
    layout.unzoom();
    true
}

/// Grows right/down or shrinks left/up with a fixed centre and the measured minimum.
pub fn resize_float(layout: &mut Layout, id: &ModuleId, dir: Direction, px: i32, frame: &Frame) -> bool {
    if frame.size.w <= 0 || frame.size.h <= 0 {
        return false;
    }
    let Some(float) = layout.float_of(id) else { return false };
    let mut rect = float.rect;
    match dir {
        Direction::Left => rect.w = rect.w.saturating_sub(px),
        Direction::Right => rect.w = rect.w.saturating_add(px),
        Direction::Up => rect.h = rect.h.saturating_sub(px),
        Direction::Down => rect.h = rect.h.saturating_add(px),
    }
    let floor = float_floor(id, frame);
    rect.w = rect.w.clamp(floor.w.min(frame.size.w), frame.size.w);
    rect.h = rect.h.clamp(floor.h.min(frame.size.h), frame.size.h);
    let changed = layout.store_float_rect(id, rect.clamped());
    if changed {
        layout.unzoom();
    }
    changed
}

/// Stores a dropped rectangle after fitting its size and origin inside the grid.
pub fn set_float_rect(layout: &mut Layout, id: &ModuleId, mut rect: Rect, frame: &Frame) -> bool {
    if !layout.is_floating(id) || frame.size.w <= 0 || frame.size.h <= 0 {
        return false;
    }
    let floor = float_floor(id, frame);
    rect.w = rect.w.clamp(floor.w.min(frame.size.w), frame.size.w);
    rect.h = rect.h.clamp(floor.h.min(frame.size.h), frame.size.h);
    rect.x = rect.x.clamp(0, frame.size.w - rect.w);
    rect.y = rect.y.clamp(0, frame.size.h - rect.h);
    let changed = layout.store_float_rect(id, FloatRect::of_rect(rect, frame.size));
    if changed {
        layout.unzoom();
    }
    changed
}

/// Floats a tile at the supplied or remembered rectangle, or updates an existing float.
pub fn float_at(
    layout: &mut Layout,
    id: &ModuleId,
    rect: Option<FloatRect>,
    frame: &Frame,
) -> Result<bool, LayoutError> {
    if *id == ModuleId::editor() {
        return Err(LayoutError::EditorNeverFloats);
    }
    if rect.is_some_and(|rect| !rect.is_valid()) {
        return Err(LayoutError::FloatRect(id.clone()));
    }
    layout.can_focus(id)?;
    if layout.is_floating(id) {
        let changed = rect.is_some_and(|rect| layout.store_float_rect(id, rect));
        if changed {
            layout.unzoom();
        }
        return Ok(changed);
    }
    if !layout.leaves().iter().any(|m| m != id && layout.is_in_view(m)) {
        return Err(LayoutError::LastTiled(id.clone()));
    }
    let rect = rect
        .or_else(|| layout.float_memory().get(id).copied())
        .unwrap_or_else(|| FloatRect::centred(frame.size, DEFAULT_FLOAT_SHARE));
    let root = layout.root_node().without(id).expect("another tile is in view");
    layout.unzoom();
    // The float must exist before replacing the tree so no module disappears between layers.
    layout.push_float(Float {
        id: id.clone(),
        rect,
        scratch: false,
    });
    layout.replace_root(root);
    Ok(true)
}

/// Runs `verb` on a copy of the layout and keeps the copy only when it succeeds, so a verb refused
/// half way through (after it has shown, unzoomed or floated something) leaves nothing changed.
fn all_or_nothing<T>(
    layout: &mut Layout,
    verb: impl FnOnce(&mut Layout) -> Result<T, LayoutError>,
) -> Result<T, LayoutError> {
    let mut trial = layout.clone();
    let result = verb(&mut trial)?;
    *layout = trial;
    Ok(result)
}

/// Stows a module at the bottom of the scratchpad and chooses the next owner as a hide does.
/// A refusal changes nothing.
pub fn scratchpad_move(layout: &mut Layout, target: &ModuleId, frame: &Frame) -> Result<Option<ModuleId>, LayoutError> {
    all_or_nothing(layout, |layout| stow(layout, target, frame))
}

fn stow(layout: &mut Layout, target: &ModuleId, frame: &Frame) -> Result<Option<ModuleId>, LayoutError> {
    if *target == ModuleId::editor() {
        return Err(LayoutError::EditorNeverFloats);
    }
    layout.can_focus(target)?;
    if !layout.modules().iter().any(|m| m != target && layout.is_shown(m)) {
        return Err(LayoutError::LastVisible(target.clone()));
    }
    if layout.is_zoomed() && !layout.zoom_hides(target) {
        layout.unzoom();
    }
    if !layout.is_floating(target) {
        let rect = layout
            .float_memory()
            .get(target)
            .copied()
            .unwrap_or_else(|| FloatRect::centred(frame.size, DEFAULT_FLOAT_SHARE));
        float_at(
            layout,
            target,
            Some(FloatRect {
                cx: 0.5,
                cy: 0.5,
                ..rect
            }),
            frame,
        )?;
    }
    layout.set_scratch(target, true);
    let next = hide(layout, target, frame)?;
    layout.lower(target);
    layout.unzoom();
    Ok(next)
}

fn show_scratch(layout: &mut Layout, id: &ModuleId) -> Result<ScratchShow, LayoutError> {
    layout.show(id)?;
    let rect = layout.float_of(id).expect("a scratch module floats").rect;
    layout.store_float_rect(
        id,
        FloatRect {
            cx: 0.5,
            cy: 0.5,
            ..rect
        },
    );
    layout.raise(id);
    if layout.zoom_hides(id) {
        layout.unzoom();
    }
    Ok(ScratchShow::Shown(id.clone()))
}

/// Stows the focused scratch float, raises a shown one, or shows the oldest stowed one.
/// Naming a module toggles that scratch float or makes the module scratch and shows it; a refusal
/// changes nothing.
pub fn scratchpad_show(
    layout: &mut Layout,
    module: Option<&ModuleId>,
    frame: &Frame,
) -> Result<ScratchShow, LayoutError> {
    if let Some(id) = module {
        if *id == ModuleId::editor() {
            return Err(LayoutError::EditorNeverFloats);
        }
        if !layout.contains(id) {
            return if id.kind().has_instances() {
                Ok(ScratchShow::Missing(id.clone()))
            } else {
                Err(LayoutError::NotInTree(id.clone()))
            };
        }
        if layout.float_of(id).is_some_and(|f| f.scratch) {
            return if layout.is_shown(id) {
                scratchpad_move(layout, id, frame).map(ScratchShow::Stowed)
            } else {
                show_scratch(layout, id)
            };
        }
        return all_or_nothing(layout, |layout| {
            layout.show(id)?;
            if !layout.is_floating(id) {
                float_at(layout, id, None, frame)?;
            }
            layout.set_scratch(id, true);
            show_scratch(layout, id)
        });
    }
    let focus = layout.focus().clone();
    if layout.float_of(&focus).is_some_and(|f| f.scratch) && layout.is_shown(&focus) {
        return scratchpad_move(layout, &focus, frame).map(ScratchShow::Stowed);
    }
    if let Some(id) = layout
        .floats()
        .iter()
        .rev()
        .find(|f| f.scratch && layout.is_shown(&f.id))
        .map(|f| f.id.clone())
    {
        layout.raise(&id);
        return Ok(ScratchShow::Raised(id));
    }
    if let Some(id) = layout
        .floats()
        .iter()
        .rev()
        .find(|f| f.scratch && layout.is_stowed(&f.id))
        .map(|f| f.id.clone())
    {
        return show_scratch(layout, &id);
    }
    Err(LayoutError::NoScratch)
}

/// The most recently used module in view on the other layer.
pub fn mode_toggle_target(layout: &Layout) -> Option<ModuleId> {
    let floating = layout.is_floating(layout.focus());
    layout
        .mru()
        .iter()
        .find(|id| {
            layout.is_floating(id) != floating
                && if floating {
                    layout.is_in_view(id)
                } else {
                    layout.is_visible(id)
                }
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::cycle::next_on_screen;
    use crate::layout::geometry::{arrange, navigate, resize, Nav, Size};
    use crate::layout::kill::{kill, Reopen};
    use crate::layout::module::{ModuleDecl, ModuleKind, Placement};
    use crate::layout::number::number_plan;
    use crate::layout::ops::{place, swap, swap_adjacent};
    use crate::layout::tree::Axis;
    use crate::layout::verbs::{move_far, swap_marked};

    fn editor() -> ModuleId {
        ModuleId::editor()
    }
    fn agent() -> ModuleId {
        ModuleId::agent()
    }
    fn term() -> ModuleId {
        ModuleId::terminal()
    }
    fn t2() -> ModuleId {
        ModuleId::instance(ModuleKind::Terminal, 2).unwrap()
    }
    fn t3() -> ModuleId {
        ModuleId::instance(ModuleKind::Terminal, 3).unwrap()
    }
    fn frame() -> Frame<'static> {
        Frame::new(Size { w: 1280, h: 721 }, 1)
    }
    fn centred() -> FloatRect {
        FloatRect {
            cx: 0.5,
            cy: 0.5,
            w: 768,
            h: 433,
        }
    }
    fn left() -> FloatRect {
        FloatRect {
            cx: 0.25,
            cy: 0.5,
            w: 400,
            h: 300,
        }
    }
    fn right() -> FloatRect {
        FloatRect {
            cx: 0.75,
            cy: 0.5,
            w: 400,
            h: 300,
        }
    }
    /// `[column(editor, terminal) | agent]`, everything shown, the keys in the editor.
    fn base() -> Layout {
        Layout::initial(&[ModuleDecl {
            id: term(),
            placement: Placement::BelowEditor,
        }])
        .unwrap()
    }
    fn ids(layout: &Layout) -> Vec<ModuleId> {
        layout.floats().iter().map(|f| f.id.clone()).collect()
    }
    fn spot(layout: &Layout, id: &ModuleId) -> Rect {
        arrange(layout, &frame()).float_spot(id).unwrap().rect
    }

    #[test]
    fn stowing_a_zoomed_module_ends_zoom_and_keeps_other_modules_shown() {
        for container in [false, true] {
            let mut layout = base();
            layout.set_focus(&term()).unwrap();
            if container {
                assert!(crate::layout::verbs::focus_parent(&mut layout));
            }
            layout.toggle_zoom(&term());
            assert!(layout.is_zoomed());
            assert!(scratchpad_move(&mut layout, &term(), &frame()).is_ok());
            assert!(!layout.is_zoomed());
            assert!(layout.is_stowed(&term()));
            assert!(layout.float_of(&term()).unwrap().scratch);
            assert!(layout.is_shown(&editor()) && layout.is_shown(&agent()));
        }
        let mut layout = base();
        hide(&mut layout, &editor(), &frame()).unwrap();
        hide(&mut layout, &agent(), &frame()).unwrap();
        assert_eq!(
            scratchpad_move(&mut layout, &term(), &frame()),
            Err(LayoutError::LastVisible(term()))
        );
    }

    #[test]
    fn empty_frames_create_valid_floats_and_leave_move_resize_and_drop_unchanged() {
        for size in [Size { w: 0, h: 0 }, Size { w: 1280, h: 0 }] {
            let empty = Frame::new(size, 1);
            let expected = FloatRect {
                cx: 0.5,
                cy: 0.5,
                w: (size.w as f32 * DEFAULT_FLOAT_SHARE).round().max(1.0) as i32,
                h: (size.h as f32 * DEFAULT_FLOAT_SHARE).round().max(1.0) as i32,
            };
            for make in 0..3 {
                let mut layout = base();
                match make {
                    0 => {
                        float_toggle(&mut layout, &term(), &empty).unwrap();
                    }
                    1 => {
                        float_at(&mut layout, &term(), None, &empty).unwrap();
                    }
                    _ => {
                        scratchpad_move(&mut layout, &term(), &empty).unwrap();
                    }
                }
                assert_eq!(layout.float_of(&term()).unwrap().rect, expected);
                assert!(layout.floats().iter().all(|f| f.rect.is_valid()));
            }
            let mut layout = base();
            float_toggle(&mut layout, &term(), &frame()).unwrap();
            let before = layout.clone();
            assert!(!move_float(&mut layout, &term(), Direction::Right, 50, &empty));
            assert!(!resize_float(&mut layout, &term(), Direction::Down, 50, &empty));
            assert!(!set_float_rect(
                &mut layout,
                &term(),
                Rect {
                    x: 50,
                    y: 50,
                    w: 400,
                    h: 300
                },
                &empty
            ));
            assert_eq!(layout, before);
            assert!(layout.floats().iter().all(|f| f.rect.is_valid()));
        }
    }

    #[test]
    fn removing_a_float_keeps_the_preset_cycle_but_removing_a_tile_restarts_it() {
        use crate::layout::verbs::{cycle_preset, preset, Preset};
        let mut layout = base();
        preset(&mut layout, Preset::MainHorizontal).unwrap();
        layout.add_float(&t2(), centred(), false).unwrap();
        layout.remove(&t2()).unwrap();
        assert_eq!(layout.last_preset(), Some(Preset::MainHorizontal));
        assert_eq!(cycle_preset(&mut layout, true), Ok(Preset::MainVertical));

        let mut layout = crate::layout::fixture::fixture("row(editor, agent, terminal#2)");
        preset(&mut layout, Preset::MainHorizontal).unwrap();
        layout.remove(&t2()).unwrap();
        assert_eq!(layout.last_preset(), None);
        assert_eq!(cycle_preset(&mut layout, true), Ok(Preset::EvenHorizontal));
    }

    #[test]
    fn float_toggle_floats_a_tiled_module_centred_and_keeps_the_keys() {
        let mut layout = base();
        layout.set_focus(&term()).unwrap();
        assert_eq!(float_toggle(&mut layout, &term(), &frame()), Ok(FloatChange::Floated));
        assert!(layout.is_floating(&term()) && !layout.leaves().contains(&term()));
        assert_eq!(layout.focus(), &term());
        let a = arrange(&layout, &frame());
        assert_eq!(
            a.rect_of(&editor()),
            Some(Rect {
                x: 0,
                y: 0,
                w: 760,
                h: 721
            })
        );
        assert_eq!(
            spot(&layout, &term()),
            Rect {
                x: 256,
                y: 145,
                w: 768,
                h: 433
            }
        );
    }

    #[test]
    fn float_toggle_on_a_float_sinks_it_beside_the_most_recent_tiled_module_and_remembers_its_rect() {
        let mut layout = base();
        layout.set_focus(&term()).unwrap();
        float_toggle(&mut layout, &term(), &frame()).unwrap();
        layout.set_focus(&agent()).unwrap();
        layout.set_focus(&term()).unwrap();
        assert_eq!(float_toggle(&mut layout, &term(), &frame()), Ok(FloatChange::Sank));
        assert!(!layout.is_floating(&term()));
        assert_eq!(layout.float_memory().get(&term()), Some(&centred()));
        assert_eq!(layout.focus(), &term());
        let a = arrange(&layout, &frame());
        // The agent's container is the root row: the terminal joins it right after the agent, halving its share.
        assert_eq!(
            a.rect_of(&editor()),
            Some(Rect {
                x: 0,
                y: 0,
                w: 760,
                h: 721
            })
        );
        assert_eq!(
            a.rect_of(&agent()),
            Some(Rect {
                x: 761,
                y: 0,
                w: 259,
                h: 721
            })
        );
        assert_eq!(
            a.rect_of(&term()),
            Some(Rect {
                x: 1021,
                y: 0,
                w: 259,
                h: 721
            })
        );
        // Floated again, it comes back at its remembered rectangle.
        float_toggle(&mut layout, &term(), &frame()).unwrap();
        assert_eq!(layout.float_of(&term()).unwrap().rect, centred());
    }

    #[test]
    fn float_toggle_refuses_the_editor_a_hidden_module_and_the_last_tiled_module() {
        let mut layout = base();
        assert_eq!(
            float_toggle(&mut layout, &editor(), &frame()),
            Err(LayoutError::EditorNeverFloats)
        );
        crate::layout::geometry::hide(&mut layout, &term(), &frame()).unwrap();
        assert_eq!(
            float_toggle(&mut layout, &term(), &frame()),
            Err(LayoutError::Hidden(term()))
        );
        let mut lone = Layout::initial(&[]).unwrap();
        kill(&mut lone, &editor(), Reopen::Never, &frame()).unwrap();
        assert_eq!(
            float_toggle(&mut lone, &agent(), &frame()),
            Err(LayoutError::LastTiled(agent()))
        );
    }

    #[test]
    fn a_far_move_docks_a_float_at_that_edge_with_phase_ones_share() {
        let mut layout = Layout::initial(&[]).unwrap();
        layout.add_float(&t2(), centred(), false).unwrap();
        layout.set_focus(&t2()).unwrap();
        assert_eq!(move_far(&mut layout, &t2(), Direction::Down, &frame()), Ok(true));
        assert!(!layout.is_floating(&t2()));
        assert_eq!(layout.float_memory().get(&t2()), Some(&centred()));
        assert_eq!(
            arrange(&layout, &frame()).rect_of(&t2()),
            Some(Rect {
                x: 0,
                y: 481,
                w: 1280,
                h: 240
            })
        );
        assert_eq!(layout.focus(), &t2());
    }

    #[test]
    fn a_float_moves_and_grows_by_pixels_inside_the_grid() {
        let mut layout = Layout::initial(&[]).unwrap();
        layout.add_float(&t2(), centred(), false).unwrap();
        assert!(move_float(&mut layout, &t2(), Direction::Right, 50, &frame()));
        assert_eq!(
            spot(&layout, &t2()),
            Rect {
                x: 306,
                y: 145,
                w: 768,
                h: 433
            }
        );
        assert!(move_float(&mut layout, &t2(), Direction::Right, 1000, &frame()));
        assert_eq!(
            spot(&layout, &t2()),
            Rect {
                x: 512,
                y: 145,
                w: 768,
                h: 433
            }
        );
        assert!(
            !move_float(&mut layout, &t2(), Direction::Right, 10, &frame()),
            "already at the edge"
        );
        assert!(resize_float(&mut layout, &t2(), Direction::Down, 40, &frame()));
        assert_eq!(layout.float_of(&t2()).unwrap().rect.h, 473);
        assert!(resize_float(&mut layout, &t2(), Direction::Left, 68, &frame()));
        assert_eq!(layout.float_of(&t2()).unwrap().rect.w, 700);
        assert!(
            !move_float(&mut layout, &agent(), Direction::Left, 10, &frame()),
            "a tiled module is no float"
        );
    }

    #[test]
    fn the_resize_keys_on_a_float_grow_and_shrink_it() {
        let mut layout = Layout::initial(&[]).unwrap();
        layout.add_float(&t2(), centred(), false).unwrap();
        layout.set_focus(&t2()).unwrap();
        assert!(resize(&mut layout, &t2(), Direction::Right, 32, &frame()));
        assert_eq!(layout.float_of(&t2()).unwrap().rect.w, 800);
        assert!(resize(&mut layout, &t2(), Direction::Up, 33, &frame()));
        assert_eq!(layout.float_of(&t2()).unwrap().rect.h, 400);
    }

    #[test]
    fn a_dragged_float_is_stored_where_it_was_dropped_clamped_into_the_grid() {
        let mut layout = Layout::initial(&[]).unwrap();
        layout.add_float(&t2(), centred(), false).unwrap();
        assert!(set_float_rect(
            &mut layout,
            &t2(),
            Rect {
                x: 100,
                y: 50,
                w: 401,
                h: 200
            },
            &frame()
        ));
        assert_eq!(
            spot(&layout, &t2()),
            Rect {
                x: 100,
                y: 50,
                w: 401,
                h: 200
            }
        );
        assert!(set_float_rect(
            &mut layout,
            &t2(),
            Rect {
                x: -40,
                y: 600,
                w: 401,
                h: 200
            },
            &frame()
        ));
        assert_eq!(
            spot(&layout, &t2()),
            Rect {
                x: 0,
                y: 521,
                w: 401,
                h: 200
            }
        );
    }

    #[test]
    fn float_at_floats_a_tiled_module_at_a_rect_or_moves_a_float_there() {
        let mut layout = base();
        assert_eq!(float_at(&mut layout, &term(), Some(left()), &frame()), Ok(true));
        assert_eq!(layout.float_of(&term()).unwrap().rect, left());
        assert_eq!(float_at(&mut layout, &term(), Some(centred()), &frame()), Ok(true));
        assert_eq!(layout.float_of(&term()).unwrap().rect, centred());
        assert_eq!(float_at(&mut layout, &term(), Some(centred()), &frame()), Ok(false));
        assert_eq!(
            float_at(&mut layout, &editor(), None, &frame()),
            Err(LayoutError::EditorNeverFloats)
        );
        let bad = FloatRect {
            cx: 3.0,
            cy: 0.5,
            w: 10,
            h: 10,
        };
        assert_eq!(
            float_at(&mut layout, &agent(), Some(bad), &frame()),
            Err(LayoutError::FloatRect(agent()))
        );
    }

    #[test]
    fn scratchpad_move_stows_a_module_as_a_scratch_float_and_the_keys_go_where_a_hide_sends_them() {
        let mut layout = base();
        layout.set_focus(&agent()).unwrap();
        layout.set_focus(&term()).unwrap();
        assert_eq!(scratchpad_move(&mut layout, &term(), &frame()), Ok(Some(agent())));
        assert!(layout.is_stowed(&term()) && layout.float_of(&term()).unwrap().scratch);
        assert_eq!(layout.float_of(&term()).unwrap().rect, centred());
        assert_eq!(layout.focus(), &agent());
        assert_eq!(
            scratchpad_move(&mut layout, &editor(), &frame()),
            Err(LayoutError::EditorNeverFloats)
        );
    }

    #[test]
    fn scratchpad_show_cycles_raise_stow_and_show_as_i3s_does() {
        let mut layout = Layout::initial(&[]).unwrap();
        layout.add_float(&t2(), centred(), true).unwrap();
        layout
            .add_float(
                &t3(),
                FloatRect {
                    cx: 0.3,
                    cy: 0.3,
                    w: 400,
                    h: 300,
                },
                true,
            )
            .unwrap();
        let f = frame();
        assert_eq!(scratchpad_show(&mut layout, None, &f), Ok(ScratchShow::Raised(t3())));
        layout.set_focus(&t3()).unwrap();
        assert_eq!(
            scratchpad_show(&mut layout, None, &f),
            Ok(ScratchShow::Stowed(Some(editor())))
        );
        assert!(layout.is_stowed(&t3()));
        assert_eq!(ids(&layout), vec![t3(), t2()], "a stowed float goes to the bottom");
        assert_eq!(scratchpad_show(&mut layout, None, &f), Ok(ScratchShow::Raised(t2())));
        layout.set_focus(&t2()).unwrap();
        assert_eq!(
            scratchpad_show(&mut layout, None, &f),
            Ok(ScratchShow::Stowed(Some(editor())))
        );
        assert_eq!(ids(&layout), vec![t2(), t3()]);
        assert_eq!(
            scratchpad_show(&mut layout, None, &f),
            Ok(ScratchShow::Shown(t3())),
            "the one stowed longest"
        );
        assert!(!layout.hidden().contains(&t3()));
        assert_eq!(
            layout.float_of(&t3()).unwrap().rect,
            FloatRect {
                cx: 0.5,
                cy: 0.5,
                w: 400,
                h: 300
            }
        );
        assert_eq!(scratchpad_show(&mut layout, None, &f), Ok(ScratchShow::Raised(t3())));
        assert_eq!(
            scratchpad_show(&mut Layout::initial(&[]).unwrap(), None, &f),
            Err(LayoutError::NoScratch)
        );
    }

    #[test]
    fn scratchpad_show_with_a_module_toggles_that_module_or_asks_for_it() {
        let mut layout = base();
        let f = frame();
        assert_eq!(
            scratchpad_show(&mut layout, Some(&t2()), &f),
            Ok(ScratchShow::Missing(t2()))
        );
        assert_eq!(
            scratchpad_show(&mut layout, Some(&term()), &f),
            Ok(ScratchShow::Shown(term()))
        );
        assert!(layout.float_of(&term()).unwrap().scratch && !layout.hidden().contains(&term()));
        assert_eq!(
            scratchpad_show(&mut layout, Some(&term()), &f),
            Ok(ScratchShow::Stowed(None))
        );
        assert!(layout.is_stowed(&term()));
        assert_eq!(
            scratchpad_show(&mut layout, Some(&term()), &f),
            Ok(ScratchShow::Shown(term()))
        );
        assert_eq!(
            scratchpad_show(&mut layout, Some(&editor()), &f),
            Err(LayoutError::EditorNeverFloats)
        );
        let x = ModuleId::lua("x");
        assert_eq!(
            scratchpad_show(&mut layout, Some(&x), &f),
            Err(LayoutError::NotInTree(x))
        );
    }

    #[test]
    fn ctrl_hjkl_from_a_float_reaches_another_float_first_then_the_tiled_module_that_way() {
        let mut layout = Layout::initial(&[]).unwrap();
        layout.add_float(&t2(), left(), false).unwrap();
        layout.add_float(&t3(), right(), false).unwrap();
        let f = frame();
        assert_eq!(navigate(&layout, &t2(), Direction::Right, &f), Nav::Module(t3()));
        assert_eq!(navigate(&layout, &t3(), Direction::Left, &f), Nav::Module(t2()));
        assert_eq!(navigate(&layout, &t3(), Direction::Right, &f), Nav::Module(agent()));
        assert_eq!(navigate(&layout, &t2(), Direction::Left, &f), Nav::Module(editor()));
        assert_eq!(
            navigate(&layout, &t2(), Direction::Down, &f),
            Nav::Module(editor()),
            "the module under it"
        );
        assert_eq!(
            navigate(&layout, &t2(), Direction::Up, &f),
            Nav::Module(editor()),
            "never the top bar"
        );
        assert_eq!(
            navigate(&layout, &editor(), Direction::Right, &f),
            Nav::Module(agent()),
            "tiles stay among tiles"
        );
    }

    #[test]
    fn select_next_and_the_numbers_walk_tiled_modules_then_floats_bottom_to_top() {
        let mut layout = Layout::initial(&[]).unwrap();
        layout.add_float(&t2(), centred(), false).unwrap();
        layout.add_float(&t3(), left(), false).unwrap();
        assert_eq!(next_on_screen(&layout, &agent()), Some(t2()));
        assert_eq!(next_on_screen(&layout, &t3()), Some(editor()));
        assert_eq!(
            number_plan(&layout),
            vec![('1', editor()), ('2', agent()), ('3', t2()), ('4', t3())]
        );
        crate::layout::geometry::hide(&mut layout, &t2(), &frame()).unwrap();
        assert_eq!(number_plan(&layout), vec![('1', editor()), ('2', agent()), ('3', t3())]);
    }

    #[test]
    fn focus_mode_toggle_jumps_to_the_most_recent_module_of_the_other_layer() {
        let mut layout = Layout::initial(&[]).unwrap();
        assert_eq!(mode_toggle_target(&layout), None);
        layout.add_float(&t2(), centred(), false).unwrap();
        layout.set_focus(&agent()).unwrap();
        assert_eq!(mode_toggle_target(&layout), Some(t2()));
        layout.set_focus(&t2()).unwrap();
        assert_eq!(mode_toggle_target(&layout), Some(agent()));
    }

    #[test]
    fn swap_marked_between_a_float_and_a_tiled_module_exchanges_them() {
        let mut layout = Layout::initial(&[]).unwrap();
        layout.add_float(&t2(), centred(), true).unwrap();
        layout.set_mark(&agent()).unwrap();
        layout.set_focus(&t2()).unwrap();
        assert_eq!(swap_marked(&mut layout, &t2()), Ok(agent()));
        assert!(layout.is_floating(&agent()) && !layout.is_floating(&t2()));
        assert_eq!(layout.float_of(&agent()).unwrap().rect, centred());
        assert!(
            layout.float_of(&agent()).unwrap().scratch,
            "the scratch flag stays with the slot"
        );
        assert_eq!(
            arrange(&layout, &frame()).rect_of(&t2()),
            Some(Rect {
                x: 761,
                y: 0,
                w: 519,
                h: 721
            })
        );
        assert_eq!(layout.focus(), &t2());
        assert_eq!(layout.mark(), Some(&agent()));
        layout.set_focus(&editor()).unwrap();
        assert_eq!(swap_marked(&mut layout, &editor()), Err(LayoutError::EditorNeverFloats));

        let mut two = Layout::initial(&[]).unwrap();
        two.add_float(&t2(), centred(), false).unwrap();
        two.add_float(&t3(), left(), false).unwrap();
        two.set_mark(&t3()).unwrap();
        two.set_focus(&t2()).unwrap();
        assert_eq!(swap_marked(&mut two, &t2()), Ok(t3()));
        assert_eq!(two.float_of(&t2()).unwrap().rect, left());
        assert_eq!(two.float_of(&t3()).unwrap().rect, centred());
    }

    #[test]
    fn a_split_key_with_the_keys_on_a_float_places_beside_the_most_recent_tiled_module_and_sinks_a_floating_one() {
        let mut layout = base();
        layout.add_float(&t2(), centred(), false).unwrap();
        layout.set_focus(&agent()).unwrap();
        layout.set_focus(&t2()).unwrap();
        place(&mut layout, &term(), &t2(), Axis::Row).unwrap();
        let a = arrange(&layout, &frame());
        assert_eq!(
            a.rect_of(&agent()),
            Some(Rect {
                x: 761,
                y: 0,
                w: 259,
                h: 721
            })
        );
        assert_eq!(
            a.rect_of(&term()),
            Some(Rect {
                x: 1021,
                y: 0,
                w: 259,
                h: 721
            })
        );
        assert_eq!(layout.focus(), &term());
        place(&mut layout, &t2(), &term(), Axis::Column).unwrap();
        assert!(!layout.is_floating(&t2()) && layout.leaves().contains(&t2()));
        assert_eq!(layout.float_memory().get(&t2()), Some(&centred()));
        assert_eq!(layout.focus(), &t2());
    }

    #[test]
    fn swap_toward_and_adjacent_refuse_a_float() {
        let mut layout = Layout::initial(&[]).unwrap();
        layout.add_float(&t2(), centred(), false).unwrap();
        layout.set_focus(&t2()).unwrap();
        assert_eq!(swap(&mut layout, &t2(), Direction::Left, &frame()), None);
        assert_eq!(swap_adjacent(&mut layout, &t2(), true), None);
        assert_eq!(layout.modules(), vec![editor(), agent(), t2()]);
    }

    /// Twenty modules in one container of `mode`, the keys on the first, and `x` floating.
    fn full(mode: crate::layout::con::ConLayout) -> Layout {
        use crate::layout::con::{Child, Con, Node};
        let children = (0..20)
            .map(|i| Child::new(Node::Leaf(ModuleId::lua(&format!("m{i}"))), 0.05))
            .collect();
        let mut l = Layout::new(Node::Con(Con::new(mode, children)), ModuleId::lua("m0")).unwrap();
        l.add_float(&ModuleId::lua("x"), centred(), false).unwrap();
        l
    }

    #[test]
    fn a_refused_place_or_sink_leaves_the_float_where_it_was() {
        use crate::layout::con::ConLayout;
        let x = ModuleId::lua("x");
        let mut l = full(ConLayout::SplitH);
        let before = l.clone();
        assert_eq!(
            place(&mut l, &x, &ModuleId::lua("m0"), Axis::Row),
            Err(LayoutError::TooMany { limit: 20 })
        );
        assert_eq!(l, before);
        for mode in [ConLayout::SplitH, ConLayout::Tabbed, ConLayout::Stacked] {
            let mut l = full(mode);
            l.set_focus(&x).unwrap();
            let before = l.clone();
            assert_eq!(
                float_toggle(&mut l, &x, &frame()),
                Err(LayoutError::TooMany { limit: 20 }),
                "{mode:?}"
            );
            assert_eq!(l, before, "{mode:?}");
        }
    }

    #[test]
    fn a_float_sinks_into_a_tabbed_container_as_a_new_tab_with_valid_shares() {
        use crate::layout::con::{Child, Con, ConLayout, Node};
        let a = ModuleId::lua("a");
        let tabs = Con::new(
            ConLayout::Tabbed,
            vec![
                Child::new(Node::Leaf(a.clone()), 0.05),
                Child::new(Node::Leaf(ModuleId::lua("b")), 0.95),
            ],
        );
        let root = Node::split(Axis::Row, 0.5, Node::Leaf(editor()), Node::Con(tabs));
        let mut l = Layout::new(root, a.clone()).unwrap();
        l.add_float(&t2(), centred(), false).unwrap();
        l.set_focus(&t2()).unwrap();
        assert_eq!(float_toggle(&mut l, &t2(), &frame()), Ok(FloatChange::Sank));
        let tabs = l.root().children[1].node.as_con().unwrap();
        assert_eq!(tabs.children.len(), 3);
        assert_eq!(tabs.children[1].node, Node::Leaf(t2()), "right after the anchor");
        assert_eq!(tabs.focus, 1, "the new tab is drawn");
        assert!(
            tabs.children.iter().all(|ch| (ch.share - 1.0 / 3.0).abs() < 1e-6),
            "evened: a half of 0.05 is too small"
        );
        assert_eq!(l.root().check(true), Ok(()));
    }

    #[test]
    fn sinking_a_float_beside_the_lone_tiled_editor_makes_a_row() {
        let mut layout = Layout::initial(&[]).unwrap();
        assert_eq!(float_toggle(&mut layout, &agent(), &frame()), Ok(FloatChange::Floated));
        assert_eq!(layout.leaves(), vec![editor()]);
        assert_eq!(float_toggle(&mut layout, &agent(), &frame()), Ok(FloatChange::Sank));
        let root = layout.root();
        assert_eq!(root.layout, ConLayout::SplitH);
        assert_eq!(
            root.children.iter().map(|ch| ch.node.clone()).collect::<Vec<_>>(),
            vec![Node::Leaf(editor()), Node::Leaf(agent())]
        );
        assert_eq!(root.check(true), Ok(()));
        assert_eq!(layout.focus(), &agent());
        let a = arrange(&layout, &frame());
        assert!(a.rect_of(&editor()).is_some() && a.rect_of(&agent()).is_some());
    }

    #[test]
    fn sinking_beside_a_lone_tile_that_is_not_the_editor_makes_a_row() {
        let mut layout = Layout::new(Node::Leaf(agent()), agent()).unwrap();
        layout.add_float(&t2(), centred(), false).unwrap();
        layout.set_focus(&t2()).unwrap();
        assert_eq!(float_toggle(&mut layout, &t2(), &frame()), Ok(FloatChange::Sank));
        let root = layout.root();
        assert_eq!(root.layout, ConLayout::SplitH);
        assert_eq!(
            root.children.iter().map(|ch| ch.node.clone()).collect::<Vec<_>>(),
            vec![Node::Leaf(agent()), Node::Leaf(t2())]
        );
        assert_eq!(root.check(true), Ok(()));
        assert_eq!(layout.focus(), &t2());
    }

    #[test]
    fn sinking_into_a_one_child_tabbed_root_keeps_the_tabs() {
        use crate::layout::con::Con;
        let tabs = Con::new(
            ConLayout::Tabbed,
            vec![
                Child::new(Node::Leaf(editor()), 0.5),
                Child::new(Node::Leaf(agent()), 0.5),
            ],
        );
        let mut layout = Layout::new(Node::Con(tabs), editor()).unwrap();
        assert_eq!(float_toggle(&mut layout, &agent(), &frame()), Ok(FloatChange::Floated));
        assert_eq!(layout.root().layout, ConLayout::Tabbed);
        assert_eq!(layout.root().children.len(), 1);
        assert_eq!(float_toggle(&mut layout, &agent(), &frame()), Ok(FloatChange::Sank));
        let root = layout.root();
        assert_eq!(root.check(true), Ok(()));
        assert_eq!(root.layout, ConLayout::Tabbed);
        assert_eq!(
            root.children.iter().map(|ch| ch.node.clone()).collect::<Vec<_>>(),
            vec![Node::Leaf(editor()), Node::Leaf(agent())]
        );
        assert_eq!(root.focus, 1, "the sunk module is the drawn tab");
        assert_eq!(layout.focus(), &agent());
    }

    #[test]
    fn a_refused_scratchpad_show_leaves_the_layout_unchanged() {
        let mut layout = Layout::initial(&[]).unwrap();
        place(&mut layout, &t2(), &agent(), Axis::Column).unwrap();
        layout.add_float(&t3(), centred(), false).unwrap();
        for id in [t2(), agent(), editor()] {
            hide(&mut layout, &id, &frame()).unwrap();
        }
        assert!(layout.is_shown(&t3()) && !layout.is_shown(&t2()));
        let before = layout.clone();
        assert_eq!(
            scratchpad_show(&mut layout, Some(&t2()), &frame()),
            Err(LayoutError::LastTiled(t2()))
        );
        assert_eq!(layout, before);
    }

    #[test]
    fn a_refused_scratchpad_move_leaves_the_layout_unchanged() {
        let mut layout = Layout::initial(&[]).unwrap();
        layout.add_float(&t3(), centred(), false).unwrap();
        hide(&mut layout, &editor(), &frame()).unwrap();
        layout.set_focus(&agent()).unwrap();
        layout.toggle_zoom(&agent());
        assert!(layout.is_zoomed());
        let before = layout.clone();
        assert_eq!(
            scratchpad_move(&mut layout, &agent(), &frame()),
            Err(LayoutError::LastTiled(agent()))
        );
        assert_eq!(layout, before);
        assert!(layout.is_zoomed());
    }
}
