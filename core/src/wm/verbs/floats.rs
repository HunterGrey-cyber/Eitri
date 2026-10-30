//! Float and scratchpad verbs keep the layout writes and the order of mapping, stacking and focus together.

use super::super::feedback::{self, Feedback, VerbOutcome};
use super::super::focus::{arrive_tail, give_keys, land_by_key};
use super::super::host::{frame_with, Batch};
use super::super::window::{WinId, Window};
use super::modules::{self, Cx};
use super::tiling::{resize_px, FALLBACK_CELL};
use crate::keymap::{Action, Keymap};
use crate::layout::rules::{OpenPoint, RuleSize};
use crate::layout::{self, FloatChange, FloatRect, Layout, LayoutError, ModuleDecl, ModuleId, OpenRules, ScratchShow};

/// How far `move.<dir>` moves a float: the same step in editor cells as `M-arrow`'s resize.
pub const FLOAT_MOVE_CELLS: u16 = 5;

/// The float size `id`'s open rule gives at a new instance's point, counting a Lua panel's position.
pub fn rule_float_size(rules: &OpenRules, decls: &[ModuleDecl], id: &ModuleId) -> Option<RuleSize> {
    let position = decls.iter().find(|d| d.id == *id).map(|d| d.placement);
    match rules.resolve(id, OpenPoint::NewInstance, position).0.size {
        Some(size @ RuleSize::Float { .. }) => Some(size),
        _ => None,
    }
}

/// With no scratch float to show, name the key that sends a module there.
pub fn scratch_empty_text(keymap: &Keymap) -> String {
    format!(
        "the scratchpad is empty ({} sends a module there)",
        super::key_text(keymap, &Action::ScratchpadMove)
    )
}

enum Ran {
    Toggled(FloatChange),
    Moved(bool),
    Mode(Option<ModuleId>),
    Stowed(Option<ModuleId>),
    Show(ScratchShow),
}

/// Runs a float verb, releasing the layout before computing its host operations.
pub fn run(w: &mut Window, cx: &Cx, win: WinId, action: &Action, batch: &mut Batch) {
    let layout = w.layout.clone();
    let Ok(mut l) = layout.try_borrow_mut() else {
        w.warn(format!("[wm] BUG: the layout was borrowed when {} ran", action.name()));
        return;
    };
    let target = l.focus().clone();
    let ran = verb(&mut l, cx, win, action, &target);
    drop(l);
    match ran {
        Ok(Ran::Toggled(change)) => {
            w.say(format!("[modules] {target}: {change:?}"));
            w.changed_by_controller(win, batch);
        }
        Ok(Ran::Moved(true)) => w.changed_by_controller(win, batch),
        Ok(Ran::Moved(false) | Ran::Mode(None)) => {
            say(w, win, feedback::floats(action, &target, VerbOutcome::Nothing), batch);
        }
        Ok(Ran::Mode(Some(to))) => {
            let grabbed = land_by_key(w, win, &to, batch);
            w.say(format!("[modules] focus.mode-toggle -> {to} (grab_focus={grabbed})"));
        }
        Ok(Ran::Stowed(next)) => w.changed_with_focus(win, next.map(|id| (id, vec![])), batch),
        Ok(Ran::Show(change)) => {
            match &change {
                ScratchShow::Stowed(next) => {
                    w.changed_with_focus(win, next.clone().map(|id| (id, vec![])), batch);
                }
                ScratchShow::Shown(to) | ScratchShow::Raised(to) => {
                    // A shown plain float can be raised without changing the float band's membership.
                    if w.float_band.contains(to) {
                        w.restack(win, batch);
                    }
                    w.changed_by_controller(win, batch);
                    give_keys(w, win, to, arrive_tail(win, to, true), batch);
                }
                ScratchShow::Missing(_) => {}
            }
            say(w, win, Some(feedback::floats_show(&change)), batch);
            if let ScratchShow::Missing(id) = change {
                modules::scratch_new(w, cx, win, &id, batch);
            }
        }
        Err(LayoutError::NoScratch) if matches!(action, Action::ScratchpadShow { .. }) => {
            say(w, win, Some(feedback::floats_empty(cx.keymap)), batch);
        }
        Err(err) => say(
            w,
            win,
            feedback::floats(action, &target, VerbOutcome::Failed(&err)),
            batch,
        ),
    }
}

fn verb(l: &mut Layout, cx: &Cx, win: WinId, action: &Action, target: &ModuleId) -> Result<Ran, LayoutError> {
    frame_with(cx.reads, win, cx.reads.grid_size(win), |frame| match action {
        Action::FloatToggle => {
            let size = if !l.is_floating(target) && !l.float_memory().contains_key(target) {
                rule_float_size(&cx.setup.rules, &cx.setup.decls, target)
            } else {
                None
            };
            match size {
                Some(size) => layout::float_at(
                    l,
                    target,
                    Some(layout::rules::float_rect(Some(size), frame.size)),
                    frame,
                )
                .map(|_| Ran::Toggled(FloatChange::Floated)),
                None => layout::float_toggle(l, target, frame).map(Ran::Toggled),
            }
        }
        Action::Move(dir) => {
            let cell = cx.reads.editor_cell(win).unwrap_or(FALLBACK_CELL);
            Ok(Ran::Moved(layout::move_float(
                l,
                target,
                *dir,
                resize_px(*dir, cell, FLOAT_MOVE_CELLS),
                frame,
            )))
        }
        Action::FocusModeToggle => Ok(Ran::Mode(layout::mode_toggle_target(l))),
        Action::ScratchpadMove => layout::scratchpad_move(l, target, frame).map(Ran::Stowed),
        Action::ScratchpadShow { module } => layout::scratchpad_show(l, module.as_ref(), frame).map(Ran::Show),
        other => unreachable!("{other:?} is not a float verb"),
    })
}

/// A Lua float request changes placement without moving the keys; its caller logs a refusal without a toast.
pub fn float_at(
    w: &mut Window,
    cx: &Cx,
    win: WinId,
    id: &ModuleId,
    rect: Option<FloatRect>,
    batch: &mut Batch,
) -> Result<(), LayoutError> {
    let layout = w.layout.clone();
    let changed = match layout.try_borrow_mut() {
        Ok(mut l) => frame_with(cx.reads, win, cx.reads.grid_size(win), |frame| {
            layout::float_at(&mut l, id, rect, frame)
        })?,
        Err(_) => {
            w.warn(format!("[wm] BUG: the layout was borrowed when {id} was floated"));
            return Ok(());
        }
    };
    if changed {
        w.changed_by_controller(win, batch);
    }
    Ok(())
}

fn say(w: &mut Window, win: WinId, feedback: Option<Feedback>, batch: &mut Batch) {
    match feedback {
        Some(Feedback::Log(line)) => w.say(line),
        Some(Feedback::Refuse { log, text }) => {
            w.say(log);
            w.refuse(win, text, batch);
        }
        None => {}
    }
}

#[cfg(test)]
mod tests;
