//! Presses and drags on the grid's own pointer targets -- a float's bar and grip, a divider -- as pointer events in
//! and batches out. The grid's gestures post a drag's begin, its motions and its end and decide nothing; what is
//! dragged, and from where, is the window's [`Drag`].
//!
//! A motion moves what is drawn now: the divider or the float's rectangle in the window's last arrangement
//! ([`Window::arranged`]), by the drag rules ([`dragged_first_px`], [`dragged_float_rect`]). Its batch is the
//! grid's allocation and nothing else -- no map, no unmap, no grab and no chrome of its own -- and the saver hears
//! of the change.

use super::super::host::{frame_with, Batch, HostOp, HostReads, PointerTarget};
use super::super::window::{WinId, Window};
use super::{dragged_first_px, dragged_float_rect};
use crate::layout::{Divider, ModuleId};

/// The button a float's bar and grip answer: the toolkit's primary button.
pub const PRIMARY_BUTTON: u32 = 1;

/// What a press began dragging.
#[derive(Debug, Clone, PartialEq)]
pub enum Drag {
    /// A divider: the handle pressed, and the divider it drew at the press, which says which split the drag moves.
    Divider { handle: u32, grabbed: Divider },
    /// A float, by its bar (`grip: false`), which moves it, or by its grip, which resizes it.
    Float { module: ModuleId, grip: bool },
}

/// A press on `on` in `win`. A primary press on a float's bar or grip raises the float (a restack with the band's
/// new order, when it was not on top) and gives it the keys as a strip entry does, then begins its drag. A press
/// on a divider begins that divider's drag, or none when the handle draws nothing now. Any other press ends a drag.
pub fn press(w: &mut Window, win: WinId, on: &PointerTarget, button: u32, batch: &mut Batch) {
    w.drag = None;
    match on {
        PointerTarget::FloatBar(module) | PointerTarget::FloatGrip(module) if button == PRIMARY_BUTTON => {
            raise(w, win, module, batch);
            if let Err(err) = super::super::focus::focus_and_arrive(w, win, module, batch) {
                w.say(format!("[modules] {module}: its float bar chose nothing: {err}"));
            }
            w.drag = Some(Drag::Float {
                module: module.clone(),
                grip: matches!(on, PointerTarget::FloatGrip(_)),
            });
        }
        PointerTarget::Divider(handle) => {
            let drawn = w
                .arranged
                .as_ref()
                .and_then(|arranged| arranged.dividers.get(*handle as usize).cloned());
            w.drag = drawn.map(|grabbed| Drag::Divider {
                handle: *handle,
                grabbed,
            });
        }
        _ => {}
    }
}

/// The float put on top of the z-order. The state file keeps floats as a set, so a raise is no change for the
/// saver; what each float covers and the order of their bars did change, which the restack's own bookkeeping
/// derives again.
fn raise(w: &mut Window, win: WinId, module: &ModuleId, batch: &mut Batch) {
    let layout = w.layout.clone();
    let raised = match layout.try_borrow_mut() {
        Ok(mut layout) => layout.raise(module),
        Err(_) => {
            w.warn(format!(
                "[wm] BUG: the layout was borrowed when {module}'s bar was pressed; nothing raised"
            ));
            false
        }
    };
    if raised {
        w.restack(win, batch);
    }
}

/// One motion of the drag, `(dx, dy)` from where the dragged widget is drawn now. A divider moves only while its
/// handle still draws the split it grabbed and the layout's split still lies along it; a float only while it is drawn, and once it has left the screen (hidden,
/// stowed, zoomed away or in a workspace no longer current) the drag ends and nothing is written for it.
pub fn motion(w: &mut Window, reads: &dyn HostReads, win: WinId, dx: f64, dy: f64, batch: &mut Batch) {
    let Some(drag) = w.drag.clone() else {
        return;
    };
    let arranged = w.arranged.clone();
    let layout = w.layout.clone();
    let divider = matches!(drag, Drag::Divider { .. });
    let moved = match drag {
        Drag::Divider { handle, grabbed } => {
            let Some(now) = arranged
                .as_ref()
                .and_then(|arranged| arranged.dividers.get(handle as usize))
            else {
                w.drag = None;
                return;
            };
            let Some(first_px) = dragged_first_px(&grabbed, now, dx, dy) else {
                return;
            };
            match layout.try_borrow_mut() {
                // A ratio on a ratio split, pixels on a pinned one: a dragged bottom row keeps the height it was
                // dragged to when the window grows.
                // The last arrangement is the allocation's, and a verb since may have turned the split it drew
                // (a row made a column, before the allocation it queued has run): the layout's own split must
                // still lie along the drawn divider's axis, or its geometry would move the wrong way.
                Ok(mut layout) => {
                    let same_axis = layout.root().find(now.con).and_then(|con| con.layout.axis()) == Some(now.axis);
                    same_axis && crate::layout::move_divider(&mut layout, now, first_px).is_ok()
                }
                Err(_) => {
                    w.warn("[wm] BUG: the layout was borrowed when a divider was dragged; nothing moved".into());
                    return;
                }
            }
        }
        Drag::Float { module, grip } => {
            let drawn = match layout.try_borrow() {
                Ok(layout) => !layout.is_zoomed() && layout.is_floating(&module) && layout.is_visible(&module),
                Err(_) => {
                    w.warn(format!(
                        "[wm] BUG: the layout was borrowed when {module} was dragged; nothing moved"
                    ));
                    return;
                }
            };
            // The last arrangement is the allocation's, and the layout may have taken the float away since:
            // both must still draw it.
            let now = arranged
                .as_ref()
                .and_then(|arranged| arranged.float_spot(&module))
                .map(|spot| spot.rect);
            let (true, Some(now)) = (drawn, now) else {
                w.drag = None;
                return;
            };
            let size = w.allocated.unwrap_or_else(|| reads.grid_size(win));
            let rect = dragged_float_rect(now, dx, dy, grip);
            let Ok(mut layout) = layout.try_borrow_mut() else {
                w.warn(format!(
                    "[wm] BUG: the layout was borrowed when {module} was dragged; nothing moved"
                ));
                return;
            };
            frame_with(reads, win, size, |frame| {
                crate::layout::set_float_rect(&mut layout, &module, rect, frame)
            })
        }
    };
    if moved {
        // A dragged divider is the user's height for the terminal, also one that stood in for an exited nvim.
        if divider {
            w.terminal_before_stand_in = None;
        }
        batch.push_layout(HostOp::Allocate { win });
        // What a float covers is in pixels, as after a resize: judged again, with no label derived.
        super::super::chrome::resized(w);
        w.saver_changed();
    }
}

/// The drag's end.
pub fn release(w: &mut Window) {
    w.drag = None;
}

#[cfg(test)]
mod tests;
