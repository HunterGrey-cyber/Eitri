//! Tab decisions over the panel's read-only answers, shared by hosts that call the panel.

use super::modules;
use crate::attention::AgentPlace;
use crate::keymap::TabAction;
use crate::layout::{self, ModuleId, ModuleKind};
use crate::tab_set::TabPlace;
use crate::tabs::{self, TabVerb, ViewId};
use crate::wm::feedback::{self, Feedback, VerbOutcome};
use crate::wm::focus::give_keys;
use crate::wm::host::{frame_with, Batch, ChatOp, HostOp};
use crate::wm::window::{WinId, Window};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabCall {
    New,
    Step(i32),
    Last,
    Select(u16),
    Rename(ViewId),
    Close(ViewId),
    CloseOthers,
    Choose(ViewId),
    Info(ViewId),
}

pub fn base_call(verb: &TabVerb) -> Option<TabCall> {
    view_call(verb, ViewId::BASE)
}

pub fn view_call(verb: &TabVerb, page: ViewId) -> Option<TabCall> {
    Some(match *verb {
        TabVerb::New => TabCall::New,
        TabVerb::Step(delta) => TabCall::Step(delta),
        TabVerb::Last => TabCall::Last,
        TabVerb::Select(n) => TabCall::Select(n),
        TabVerb::Rename => TabCall::Rename(page),
        TabVerb::Close => TabCall::Close(page),
        TabVerb::CloseOthers => TabCall::CloseOthers,
        TabVerb::Choose => TabCall::Choose(page),
        TabVerb::Info => TabCall::Info(page),
        TabVerb::Flash | TabVerb::Nothing | TabVerb::Sibling(_) | TabVerb::Terminal(_) => return None,
    })
}

/// What `verb` says for `outcome`; a bare flash has no line to print.
fn tell(w: &mut Window, win: WinId, verb: &TabVerb, outcome: VerbOutcome<'_>, batch: &mut Batch) {
    match feedback::tabs(verb, outcome) {
        Some(Feedback::Refuse { log, text }) => {
            if !log.is_empty() {
                w.say(log);
            }
            w.refuse(win, text, batch);
        }
        Some(Feedback::Log(line)) => w.say(line),
        None => {}
    }
}

fn base_keys(w: &mut Window, cx: &modules::Cx, win: WinId, verb: &TabVerb, batch: &mut Batch) -> bool {
    let chat = ModuleId::agent();
    match modules::show_on_screen(w, cx, win, &chat, batch) {
        Ok(()) => {
            give_keys(w, win, &chat, vec![], batch);
            true
        }
        Err(err) => {
            tell(w, win, verb, VerbOutcome::Failed(&err), batch);
            false
        }
    }
}

fn reveal_base(w: &mut Window, cx: &modules::Cx, win: WinId, batch: &mut Batch) {
    let chat = ModuleId::agent();
    let place = {
        let Ok(layout) = w.layout.try_borrow() else { return };
        let covered = frame_with(cx.reads, win, cx.reads.grid_size(win), |frame| {
            let arranged = layout::arrange(&layout, frame);
            if layout::covering(&arranged, &chat).is_empty() {
                vec![]
            } else {
                vec![chat.clone()]
            }
        });
        layout::view_place(&layout, &chat, &covered)
    };
    if !tabs::reveal_on_switch(place) {
        return;
    }
    if place == AgentPlace::Covered {
        if modules::reveal_chat(w, cx, win, ViewId::BASE, batch).is_some() {
            w.say("[tabs] a float still covers the chat".into());
        }
        return;
    }
    let layout = w.layout.clone();
    let result = layout.try_borrow_mut().ok().map(|mut layout| {
        let shown = layout.show(&chat)?;
        let away = layout.is_tabbed_away(&chat);
        let brought = away && layout.bring_forward(&chat);
        Ok::<_, layout::LayoutError>((shown || brought, !away || brought))
    });
    match result {
        Some(Ok((changed, drawn))) => {
            if changed {
                w.changed_by_controller(win, batch);
            }
            if !drawn {
                w.say("[tabs] the chat's tab stays behind the keys' tab".into());
            }
        }
        Some(Err(err)) => w.warn(format!("[tabs] could not reveal the chat: {err}")),
        None => {}
    }
}

pub fn run(w: &mut Window, cx: &modules::Cx, win: WinId, tab: TabAction, keys: Option<ModuleId>, batch: &mut Batch) {
    let view_plan = keys
        .as_ref()
        .and_then(ViewId::of_module)
        .and_then(|view| tabs::view_plan(tab, view));
    let (verb, page, takes_keys) = if let Some(plan) = view_plan {
        (plan.verb, plan.page, plan.takes_the_keys)
    } else {
        let Ok(layout) = w.layout.try_borrow() else { return };
        let terminals = layout
            .modules()
            .iter()
            .filter(|id| id.kind() == ModuleKind::Terminal && layout.is_visible(id))
            .count();
        let in_tabs = keys
            .as_ref()
            .is_some_and(|id| id.kind() == ModuleKind::Terminal && layout.tab_neighbour(id, true).is_some());
        let plan = tabs::plan(tab, keys.as_ref().map(ModuleId::kind), terminals, in_tabs);
        (plan.verb, ViewId::BASE, plan.takes_the_keys)
    };
    match verb {
        TabVerb::Sibling(delta) | TabVerb::Terminal(delta) => {
            let to = w.layout.try_borrow().ok().and_then(|layout| {
                keys.as_ref().and_then(|from| {
                    if matches!(verb, TabVerb::Sibling(_)) {
                        layout.tab_neighbour(from, delta > 0)
                    } else {
                        layout::next_terminal(&layout, from, delta > 0)
                    }
                })
            });
            match to {
                Some(to) => {
                    let grabbed = give_keys(w, win, &to, vec![], batch);
                    w.say(format!("[tabs] -> {to} (grab_focus={grabbed})"));
                }
                // The plan picks these steps only where the layout has somewhere to go, so this is the layout
                // read again finding nothing.
                None => tell(w, win, &verb, VerbOutcome::Nothing, batch),
            }
            return;
        }
        _ => {}
    }
    let place = if let TabVerb::Select(n) = verb {
        cx.reads.tab_place(win, n)
    } else {
        None
    };
    let switched = match verb {
        TabVerb::Step(delta) => cx.reads.tab_can_step(win, delta),
        TabVerb::Last => cx.reads.tab_has_last(win),
        TabVerb::Select(_) => place.is_some(),
        TabVerb::Flash | TabVerb::Nothing => false,
        _ => true,
    };
    if !switched {
        tell(w, win, &verb, VerbOutcome::Nothing, batch);
        return;
    }
    if takes_keys && !matches!(verb, TabVerb::Select(_)) && !base_keys(w, cx, win, &verb, batch) {
        return;
    }
    if view_plan.is_some() && matches!(place, Some(TabPlace::Base(_))) {
        // The tab is selected whether or not the base view could be shown; a refusal has said why.
        base_keys(w, cx, win, &verb, batch);
    }
    if let Some(call) = view_call(&verb, page) {
        batch.push_effect(HostOp::Chat(ChatOp::Tab { win, call }));
    }
    if matches!(place, Some(TabPlace::View(..))) {
        return;
    }
    // A base-view tab switched to from elsewhere shows the base view where it was. An extra view's rename, close,
    // chooser and detail open on that view's own page, so they leave the base view as it is.
    let reveals = match view_plan {
        None => !takes_keys,
        Some(_) => matches!(verb, TabVerb::Step(_) | TabVerb::Last),
    };
    if reveals {
        reveal_base(w, cx, win, batch);
    }
}

#[cfg(test)]
mod tests;
