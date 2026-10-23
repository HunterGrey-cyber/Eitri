//! The tiling verbs: what changes the tiled tree, the mark, the zoom or the selection, each run on the window's
//! layout with the frame the grid would give it now, answered with the batch that shows the change and with the
//! feedback the shell's arms gave.

use super::super::feedback::{self, Feedback, VerbOutcome};
use super::super::host::{frame_with, Batch, HostOp, HostReads};
use super::super::window::{WinId, Window};
use crate::keymap::{Action, SwapTarget};
use crate::layout::{self, Axis, ConLayout, Direction, Layout, LayoutError, ModuleId, Preset, ZoomChange};

/// A cell's size, in logical px, when the editor cannot say (not ready yet, or hidden since before nvim started).
pub const FALLBACK_CELL: (f64, f64) = (8.0, 16.0);

/// One resize step in pixels: `cells` editor cells, across for left/right, down for up/down. The sign is the
/// geometry's (`layout::resize`).
pub fn resize_px(direction: Direction, cell: (f64, f64), cells: u16) -> i32 {
    let along = match direction {
        Direction::Left | Direction::Right => cell.0,
        Direction::Up | Direction::Down => cell.1,
    };
    (f64::from(cells) * along).round() as i32
}

/// What one verb did to the layout, with what its feedback needs.
enum Ran {
    /// The layout changed; `said` is the success line some verbs print.
    Changed {
        said: Option<String>,
    },
    /// Only the selection or the mark changed, which nothing in the grid draws: the shell's change hooks
    /// redraw them, and the state file is not written for it.
    Marked {
        said: Option<String>,
    },
    /// Nothing to change.
    Nothing,
    Failed(LayoutError),
}

/// Runs the tiling verb `action` on `w`'s layout and adds what follows to `batch`: the layout's ops and the
/// change's effects when it changed, else the refusal's or the log line's feedback.
pub fn run(w: &mut Window, reads: &dyn HostReads, win: WinId, action: &Action, batch: &mut Batch) {
    let layout = w.layout.clone();
    let Ok(mut layout) = layout.try_borrow_mut() else {
        w.warn(format!("[wm] BUG: the layout was borrowed when {} ran", action.name()));
        return;
    };
    let target = layout.focus().clone();
    let ran = verb(&mut layout, reads, win, action, &target);
    drop(layout);
    let outcome = match &ran {
        Ran::Changed { .. } | Ran::Marked { .. } => VerbOutcome::Done,
        Ran::Nothing => VerbOutcome::Nothing,
        Ran::Failed(err) => VerbOutcome::Failed(err),
    };
    // A container verb with no container flashes without a toast; its table row carries the line only.
    let flash_alone = matches!(
        (action, &ran),
        (Action::ContainerTabbed | Action::ContainerStacked, Ran::Nothing)
    );
    match feedback::tiling(action, &target, outcome) {
        Some(Feedback::Log(line)) => {
            w.say(line);
            if flash_alone {
                batch.push_effect(HostOp::Refuse {
                    win,
                    text: String::new(),
                });
            }
        }
        Some(Feedback::Refuse { log, text }) => {
            w.say(log);
            batch.push_effect(HostOp::Refuse { win, text });
        }
        None => {}
    }
    match ran {
        Ran::Changed { said } => {
            if let Some(line) = said {
                w.say(line);
            }
            w.changed_by_controller(win, batch);
        }
        Ran::Marked { said } => {
            if let Some(line) = said {
                w.say(line);
            }
            batch.push_effect(HostOp::ChromeHooks { win });
        }
        Ran::Failed(_) if matches!(action, Action::MarkToggle) => {
            // The mark's text is redrawn whether or not the toggle took, as the shell's arm did.
            batch.push_effect(HostOp::ChromeHooks { win });
        }
        Ran::Nothing if matches!(action, Action::MarkClear) => batch.push_effect(HostOp::ChromeHooks { win }),
        Ran::Nothing | Ran::Failed(_) => {}
    }
}

/// The verb itself on the layout, with the frame the grid would arrange in now where the geometry needs one.
fn verb(layout: &mut Layout, reads: &dyn HostReads, win: WinId, action: &Action, target: &ModuleId) -> Ran {
    let size = reads.grid_size(win);
    let changed = |changed: bool| {
        if changed {
            Ran::Changed { said: None }
        } else {
            Ran::Nothing
        }
    };
    let result = |result: Result<bool, LayoutError>| match result {
        Ok(moved) => changed(moved),
        Err(err) => Ran::Failed(err),
    };
    let preset = |layout: &mut Layout, preset: Preset| match layout::preset(layout, preset) {
        Ok(()) => Ran::Changed { said: None },
        Err(err) => Ran::Failed(err),
    };
    match action {
        Action::MoveFar(dir) => result(frame_with(reads, win, size, |frame| {
            layout::move_far(layout, target, *dir, frame)
        })),
        Action::ToggleSplit => changed(layout::toggle_split(layout, target)),
        Action::Rotate { down } => changed(layout::rotate(layout, *down)),
        Action::Flip(axis) => changed(layout::flip(layout, *axis)),
        Action::Spread => changed(layout::spread(layout, target)),
        // `even-*` are presets too, so `layout.next` goes on from them.
        Action::Even(Axis::Row) => preset(layout, Preset::EvenHorizontal),
        Action::Even(Axis::Column) => preset(layout, Preset::EvenVertical),
        Action::MainHorizontal => preset(layout, Preset::MainHorizontal),
        Action::MainVertical => preset(layout, Preset::MainVertical),
        Action::Tiled => preset(layout, Preset::Tiled),
        Action::CycleLayout { forward } => match layout::cycle_preset(layout, *forward) {
            Ok(preset) => Ran::Changed {
                said: Some(format!("[modules] layout -> {preset:?}")),
            },
            Err(err) => Ran::Failed(err),
        },
        Action::LayoutUndo => match layout.undo() {
            Ok(()) => Ran::Changed { said: None },
            Err(err) => Ran::Failed(err),
        },
        Action::MarkToggle => match layout.toggle_mark(target) {
            Ok(marked) => Ran::Marked {
                said: Some(format!("[modules] {target}: marked={marked}")),
            },
            Err(err) => Ran::Failed(err),
        },
        Action::MarkClear => {
            if layout.clear_mark() {
                Ran::Marked { said: None }
            } else {
                Ran::Nothing
            }
        }
        Action::SwapMarked => match layout::swap_marked(layout, target) {
            Ok(marked) => Ran::Changed {
                said: Some(format!("[modules] {target}: swapped with the marked {marked}")),
            },
            Err(err) => Ran::Failed(err),
        },
        Action::Zoom => match layout.toggle_zoom(target) {
            ZoomChange::Zoomed => Ran::Changed {
                said: Some(format!("[module_grid] zoomed {target}")),
            },
            ZoomChange::Unzoomed => Ran::Changed {
                said: Some("[module_grid] unzoomed".into()),
            },
            ZoomChange::Nothing => Ran::Nothing,
        },
        Action::Resize { dir, cells } => {
            let cell = reads.editor_cell(win).unwrap_or(FALLBACK_CELL);
            let px = resize_px(*dir, cell, *cells);
            changed(frame_with(reads, win, size, |frame| {
                layout::resize(layout, target, *dir, px, frame)
            }))
        }
        Action::Swap(SwapTarget::Toward(dir)) => {
            changed(frame_with(reads, win, size, |frame| layout::swap(layout, target, *dir, frame)).is_some())
        }
        Action::Swap(which) => changed(layout::swap_adjacent(layout, target, *which == SwapTarget::Next).is_some()),
        Action::Move(dir) => result(layout::move_dir(layout, target, *dir)),
        Action::ContainerTabbed => result(layout::container_mode(layout, target, ConLayout::Tabbed)),
        Action::ContainerStacked => result(layout::container_mode(layout, target, ConLayout::Stacked)),
        // Only the selection changes, which nothing in the grid draws.
        Action::FocusParent => marked(layout::focus_parent(layout)),
        Action::FocusChild => marked(layout::focus_child(layout)),
        other => {
            debug_assert!(false, "{other:?} is not a tiling verb");
            Ran::Nothing
        }
    }
}

fn marked(changed: bool) -> Ran {
    if changed {
        Ran::Marked { said: None }
    } else {
        Ran::Nothing
    }
}

#[cfg(test)]
mod tests;
