//! The panel-facing verbs and the panel's events: `panel.reload`, `prefix ?`'s keys sheet, `prefix :`'s command
//! line, the chat's tab verbs, `tab.break` and its join back, and what the panel asks of the window -- a view shown,
//! a view left with no tab, a review's file opened in the editor, an edit in the editor ended, and a request to the
//! editor. Each is decided on the window's layout and answered with its batch: the layout's change and the keys
//! first, then the panel's call as an effect, so the page is told only once the keys are where it opens.

use super::super::close::KillScope;
use super::super::feedback::{self, Feedback, PanelOutcome, PanelWarning};
use super::super::focus::{self, arrive_tail, focus_and_arrive, give_keys};
use super::super::host::*;
use super::super::module::ModuleState;
use super::super::window::{EditorPresence, WinId, Window};
use super::modules::{self, Cx};
use crate::keymap::keys_return::Opened;
use crate::keymap::Action;
use crate::layout::{self, LayoutError, ModuleId, Reopen};
use crate::tabs::ViewId;

/// What a newly handed permission card asks the shell to do.
#[derive(Debug, Clone, PartialEq)]
pub enum AttentionChoice {
    None,
    Toast,
    Reveal(ChatEvent),
}

/// A view under a float counts as away: its cards may be hidden there. Asked again for each view, as revealing
/// the one before may have moved the layout.
pub fn attention(
    w: &Window,
    cx: &Cx,
    win: WinId,
    view: ViewId,
    policy: crate::attention::ChatOnPermission,
    mine: crate::attention::Attention,
) -> AttentionChoice {
    use crate::attention::AgentPlace;
    let Ok(layout) = w.layout.try_borrow() else {
        return AttentionChoice::Toast;
    };
    let chat = view.module();
    let covered = frame_with(cx.reads, win, cx.reads.grid_size(win), |frame| {
        !layout::covering(&layout::arrange(&layout, frame), &chat).is_empty()
    });
    let place = layout::view_place(&layout, &chat, &if covered { vec![chat.clone()] } else { vec![] });
    // The panel already judged that a card arrived in this view, so the view's attention is weighed against
    // none: react then asks only whether a card waits and where it is.
    let reaction = crate::attention::react(policy, Default::default(), mine, place);
    println!("[attention] {chat} {mine:?} ({place:?}) -> {reaction:?}");
    if reaction.reveal {
        AttentionChoice::Reveal(if place == AgentPlace::Covered {
            ChatEvent::Covered { win, view }
        } else {
            ChatEvent::Reveal { win, view }
        })
    } else if reaction.toast {
        AttentionChoice::Toast
    } else {
        AttentionChoice::None
    }
}

/// The reveal has been handled: a view still covered or behind the keys' tab gets a toast instead.
pub fn attention_revealed(w: &Window, cx: &Cx, event: &ChatEvent) -> AttentionChoice {
    let toast = match event {
        ChatEvent::Covered { win, view } => {
            // Only scratch floats without the keys are put away; a float still over the view after that gets
            // the toast the badge policy shows.
            let still = modules::answer_gate(Some(w), cx, *win, *view).is_err();
            if still {
                println!("[attention] a float still covers {}; a toast instead", view.module());
            }
            still
        }
        ChatEvent::Reveal { view, .. } => {
            // Its tab stays behind when the keys are on a sibling tab of the same container (drawing the
            // view's would hide them): the toast the badge policy shows is all there is then, as under a zoom.
            let still = w
                .layout
                .try_borrow()
                .map_or(true, |layout| layout.is_tabbed_away(&view.module()));
            if still {
                println!(
                    "[attention] {}'s tab stays behind the keys' tab; a toast instead",
                    view.module()
                );
            }
            still
        }
        _ => false,
    };
    if toast {
        AttentionChoice::Toast
    } else {
        AttentionChoice::None
    }
}

/// What `prefix !` does with the keys in `keys_view` (`None`: the keys are in no chat view).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakPlan {
    /// The base view's active tab goes out into a new chat view of its own.
    Break,
    /// That extra view's tab comes back into the base view, and the view goes.
    Join(ViewId),
    /// Like tmux's `break-pane`, it acts on the pane with the keys: anywhere but a chat view there is no tab to
    /// break.
    Refuse,
}

/// [`BreakPlan`] for the keys in `keys_view`.
pub fn break_plan(keys_view: Option<ViewId>) -> BreakPlan {
    match keys_view {
        Some(view) if view.is_base() => BreakPlan::Break,
        Some(view) => BreakPlan::Join(view),
        None => BreakPlan::Refuse,
    }
}

/// Says `feedback`: a line alone, or a line and a refusal the chrome flashes and toasts.
fn say(w: &mut Window, win: WinId, feedback: Feedback, batch: &mut Batch) {
    match feedback {
        Feedback::Log(line) => w.say(line),
        Feedback::Refuse { log, text } => {
            w.say(log);
            w.refuse(win, text, batch);
        }
    }
}

/// The module a panel verb acts from: the page that sent a tab verb, else the module the host says holds the keys.
fn keys(w: &Window) -> Option<ModuleId> {
    w.tab_view
        .map(ViewId::module)
        .or_else(|| w.focus.native_module().cloned())
}

/// The chat view a panel verb opens something in: the view with the keys, else the base view.
fn keys_view(w: &Window) -> ViewId {
    keys(w).as_ref().and_then(ViewId::of_module).unwrap_or(ViewId::BASE)
}

/// A panel verb from the runner. `r`, `?` and `:` act in the page of the chat view with the keys; from anywhere
/// else, in the base view's, which comes up with the keys for the two that read them.
pub fn run(w: &mut Window, cx: &Cx, win: WinId, action: &Action, batch: &mut Batch) {
    match action {
        Action::PanelReload => {
            let view = keys_view(w);
            batch.push_effect(HostOp::Chat(ChatOp::Reload { win, view }));
        }
        Action::PanelKeymap => keymap(w, cx, win, batch),
        Action::PanelCommandLine => command_line(w, cx, win, batch),
        Action::Tab(tab) => {
            let keys = keys(w);
            super::tabs::run(w, cx, win, *tab, keys, batch);
        }
        Action::TabBreak => tab_break(w, cx, win, action, batch),
        other => w.warn(format!("[wm] BUG: {} reached the panel verbs", other.name())),
    }
}

/// `prefix ?`: the keys sheet, in the page of the chat view with the keys or else the base view's, which is shown
/// and takes the keys. The plugin page the keys were in is read first, since once the chat has the keys it is what
/// holds them. A chat a float still covers opens nothing, and what showing it changed is put back: hidden again
/// when it was hidden, the zoom it ended restored.
fn keymap(w: &mut Window, cx: &Cx, win: WinId, batch: &mut Batch) {
    let from = keys(w);
    let plugin = from.as_ref().and_then(|module| cx.reads.plugin_keys_help(win, module));
    let view = from.as_ref().and_then(ViewId::of_module).unwrap_or(ViewId::BASE);
    let chat = view.module();
    // Kept whole, a container's zoom as well as a module's, so closing the sheet can put it back.
    let Ok((zoom_before, showed_sheet)) = w
        .layout
        .try_borrow()
        .map(|layout| (layout.zoom().cloned(), !layout.is_shown(&chat)))
    else {
        w.warn("[wm] BUG: the layout was borrowed when the keys sheet opened".into());
        return;
    };
    if let Err(err) = modules::show_on_screen(w, cx, win, &chat, batch) {
        say(w, win, feedback::panel(PanelOutcome::NotShown(&err)), batch);
        return;
    }
    if let Some(text) = modules::reveal_chat(w, cx, win, view, batch) {
        if showed_sheet {
            if let Err(err) = modules::hide(w, cx, win, &chat, batch) {
                w.warn(format!("[keys_return] could not hide {chat} again: {err}"));
            }
        }
        let unzoomed = w.layout.try_borrow().is_ok_and(|layout| layout.zoom().is_none());
        if let (Some(zoom), Some(keeps), true) = (&zoom_before, &from, unzoomed) {
            focus::put_zoom_back(w, win, zoom, keeps, batch);
        }
        say(w, win, feedback::panel(PanelOutcome::Covered(&text)), batch);
        return;
    }
    // Showing the chat ended a zoom that hid it; giving it the keys below chooses its tab if that tab is not drawn,
    // which ends any zoom left (a container's that holds the chat).
    let ended_zoom = match w.layout.try_borrow() {
        Ok(layout) if layout.zoom().is_none() || layout.is_tabbed_away(&chat) => zoom_before,
        _ => None,
    };
    let opening = focus::keys_opened(
        w,
        view,
        Opened {
            from,
            ended_zoom,
            showed_sheet,
        },
    );
    give_keys(w, win, &chat, vec![], batch);
    batch.push_effect(HostOp::Chat(ChatOp::OpenKeymap {
        win,
        view,
        plugin,
        opening,
    }));
}

/// `prefix :`, tmux's `command-prompt`: the chat comes up with the keys and its `:` line opens, which runs nothing,
/// so the letters typed after the chord land in a box instead of running as panel keys. A close of the `?` sheet
/// still on its way from the page is older than this request, so it no longer moves the keys.
fn command_line(w: &mut Window, cx: &Cx, win: WinId, batch: &mut Batch) {
    let view = keys_view(w);
    let chat = view.module();
    if let Err(err) = modules::show_on_screen(w, cx, win, &chat, batch) {
        say(w, win, feedback::panel(PanelOutcome::NotShown(&err)), batch);
        return;
    }
    if let Some(text) = modules::reveal_chat(w, cx, win, view, batch) {
        say(w, win, feedback::panel(PanelOutcome::Covered(&text)), batch);
        return;
    }
    focus::keys_superseded(w);
    give_keys(w, win, &chat, vec![], batch);
    batch.push_effect(HostOp::Chat(ChatOp::CommandLine { win, view }));
}

/// tmux's `break-pane`, on the chat view with the keys: from the base view its active tab goes out into a new view
/// where the open rules put a broken-out tab, asked first so a break that cannot happen makes no view; from an
/// extra view that view's tab comes back into the base view, which takes the keys before the view goes.
fn tab_break(w: &mut Window, cx: &Cx, win: WinId, action: &Action, batch: &mut Batch) {
    match break_plan(keys(w).as_ref().and_then(ViewId::of_module)) {
        BreakPlan::Break => match cx.reads.can_break_out(win) {
            Ok(()) => modules::tab_break(w, cx, win, batch),
            Err(text) => say(w, win, feedback::panel(PanelOutcome::NoBreak(&text)), batch),
        },
        BreakPlan::Join(view) => {
            // Asked before anything moves: a view with no tab has nothing to join, and the keys would otherwise
            // be left in the base view for a refusal.
            if !cx.reads.chat_views().contains(&view) {
                say(w, win, feedback::panel(PanelOutcome::NoTab(view)), batch);
                return;
            }
            let chat = ModuleId::agent();
            if let Err(err) = modules::show_on_screen(w, cx, win, &chat, batch) {
                say(w, win, feedback::panel(PanelOutcome::NotShown(&err)), batch);
                return;
            }
            give_keys(w, win, &chat, vec![], batch);
            batch.push_effect(HostOp::Chat(ChatOp::JoinBack { win, view }));
        }
        BreakPlan::Refuse => say(w, win, feedback::panel(PanelOutcome::NoChatView(action)), batch),
    }
}

/// The panel refused to bring `view`'s tab back: the keys stay in the base view, and its words are the refusal.
pub fn join_refused(w: &mut Window, win: WinId, why: &str, batch: &mut Batch) {
    say(w, win, feedback::panel(PanelOutcome::JoinRefused(why)), batch);
}

/// A pick that names a tab bound to an extra view (a number key, a chooser pick, a resume), or a resume from an
/// extra page that opened a new base-view tab: that view is shown where it was hidden from and given the keys, and
/// arrives where its reader left it only if its grab takes.
pub fn show_view(w: &mut Window, cx: &Cx, win: WinId, view: ViewId, batch: &mut Batch) {
    let id = view.module();
    if let Err(err) = modules::show_on_screen(w, cx, win, &id, batch) {
        say(w, win, feedback::panel(PanelOutcome::NotShown(&err)), batch);
        return;
    }
    give_keys(w, win, &id, arrive_tail(win, &id, false), batch);
}

/// An extra view left with no tab -- its tab closed or joined back, or a restore that brought none into it -- leaves
/// the window, judged from an idle after the panel said so, since the panel may be in the middle of its own call
/// then. A view given a tab again by then stays; the base view never goes; nor does a view whose removal is already
/// its caller's (a kill took it out, or it is still being made), nor one whose tab is in the other window of the pair.
/// The only module on screen gets the base view (else
/// the editor) shown beside it first, since the layout never hides its last module; the keys in it go to the base
/// view before it goes.
pub fn view_empty(w: &mut Window, cx: &Cx, win: WinId, view: ViewId, batch: &mut Batch) {
    if view.is_base() || cx.reads.chat_views().contains(&view) {
        return;
    }
    let id = view.module();
    // A view whose tab went to the other window of the pair is away, not empty: its placeholder stays for the tab's
    // return.
    let removed_already = matches!(
        w.lifecycle.state(&id),
        Some(ModuleState::Killed(_) | ModuleState::Retired | ModuleState::Away)
    );
    if removed_already || w.modules.pending_ids().contains(&id) {
        return;
    }
    let layout = w.layout.clone();
    let alone = match layout.try_borrow() {
        Ok(layout) => layout::can_kill(&layout, &id) == Ok(KillScope::Window),
        Err(_) => {
            w.warn(format!("[wm] BUG: the layout was borrowed when {id} was found empty"));
            return;
        }
    };
    if alone {
        let shown = modules::show_on_screen(w, cx, win, &ModuleId::agent(), batch).or_else(|err| {
            say(
                w,
                win,
                feedback::panel(PanelOutcome::BaseNotShown { view, err: &err }),
                batch,
            );
            modules::show_on_screen(w, cx, win, &ModuleId::editor(), batch)
        });
        if let Err(err) = shown {
            say(
                w,
                win,
                feedback::panel(PanelOutcome::AloneStays { view, err: &err }),
                batch,
            );
            return;
        }
    }
    let base = ModuleId::agent();
    let holds_keys = w.focus.native_module() == Some(&id);
    // A zoom on the view keeps the base view in the layout but off screen: it ends before the base view is asked
    // to take the keys, so the base view is mapped first. A grab of an unmapped host is refused, and the removal
    // below, seeing the layout's focus already on the base view, would ask for none again.
    let unzoomed = holds_keys
        && w.layout
            .try_borrow_mut()
            .is_ok_and(|mut layout| layout.zoom_hides(&base) && layout.unzoom());
    if unzoomed {
        w.say("[module_grid] unzoomed".into());
        w.changed_by_controller(win, batch);
    }
    if holds_keys && give_keys(w, win, &base, vec![], batch) {
        // The layout learns where the keys are only from the host's report after this batch; without this, the
        // removal below would hand on keys it thinks the view still holds.
        let moved = w.layout.try_borrow_mut().map(|mut layout| layout.set_focus(&base));
        if let Ok(Err(err)) = moved {
            w.warn(format!(
                "[chat] {id}: the base view could not take the layout's focus: {err}"
            ));
        }
    }
    match drop_view(w, cx, win, view, batch) {
        Ok(true) => say(w, win, feedback::panel(PanelOutcome::Removed(view)), batch),
        Ok(false) => {}
        Err(err) => say(
            w,
            win,
            feedback::panel(PanelOutcome::NotRemoved { view, err: &err }),
            batch,
        ),
    }
}

/// Takes the extra view `view` out of the window as a kill takes one: out of the layout (its keys handed on before
/// its host unmaps), out of every other workspace, its page left idle under its id for the next view of that
/// number, and the panel told which views remain. Retired, so no key or split places it by name again. `true`
/// when there was anything to take out.
fn drop_view(w: &mut Window, cx: &Cx, win: WinId, view: ViewId, batch: &mut Batch) -> Result<bool, LayoutError> {
    let id = view.module();
    let layout = w.layout.clone();
    let next = {
        let Ok(mut l) = layout.try_borrow_mut() else {
            w.warn(format!("[wm] BUG: the layout was borrowed when {id} was removed"));
            return Ok(false);
        };
        if l.contains(&id) {
            Some(frame_with(cx.reads, win, cx.reads.grid_size(win), |frame| {
                layout::kill(&mut l, &id, Reopen::Remove, frame)
            })?)
        } else {
            None
        }
    };
    let in_layout = next.is_some();
    if let Some(next) = next {
        w.changed_with_focus(win, next.map(|next| (next, vec![])), batch);
    }
    let workspaces = w.workspaces.clone();
    let elsewhere = match (layout.try_borrow(), workspaces.try_borrow_mut()) {
        (Ok(layout), Ok(mut workspaces)) => {
            let held = workspaces.each(&layout).iter().any(|(_, layout)| layout.contains(&id));
            workspaces.remove_everywhere(&id);
            held
        }
        _ => {
            w.warn(format!("[wm] BUG: the workspaces were borrowed when {id} was removed"));
            false
        }
    };
    let hosted = w
        .lifecycle
        .state(&id)
        .is_some_and(|state| !matches!(state, ModuleState::Declared));
    if hosted {
        w.lifecycle.record_event(
            win,
            &HostEvent::SurfaceEnded {
                win,
                module: id.clone(),
                why: Ended::Retired,
            },
        );
        batch.push_effect(HostOp::DropSurface {
            win,
            module: id.clone(),
            why: DropWhy::Retired,
        });
    }
    // A restore still to come must not bring a tab back into a view that has left: its page is idle, so the tab
    // would show nowhere.
    let views = modules::remaining_views(w);
    batch.push_effect(HostOp::Chat(ChatOp::LayoutViews { win, views }));
    focus::modules_changed(w, win, batch);
    Ok(in_layout || elsewhere || hosted)
}

/// The review overlay opened a file in this window's editor: the editor is shown and given the keys, as a draft
/// being edited is.
pub fn review_open(w: &mut Window, cx: &Cx, win: WinId, batch: &mut Batch) {
    let editor = ModuleId::editor();
    match modules::show_on_screen(w, cx, win, &editor, batch) {
        Ok(()) => {
            give_keys(w, win, &editor, vec![], batch);
        }
        Err(err) => w.warn(feedback::panel_warning(PanelWarning::ReviewNotShown(&err))),
    }
}

/// An edit in the editor ended: the keys go back to the view the edit's tab is in, arriving there, or to the base
/// view when that view has gone meanwhile.
pub fn editor_done(w: &mut Window, cx: &Cx, win: WinId, view: ViewId, batch: &mut Batch) {
    let _ = cx;
    let returned = focus_and_arrive(w, win, &view.module(), batch).or_else(|err| {
        if view.is_base() {
            Err(err)
        } else {
            focus_and_arrive(w, win, &ModuleId::agent(), batch)
        }
    });
    if let Err(err) = returned {
        w.warn(feedback::panel_warning(PanelWarning::NoReturn(&err)));
    }
}

/// Whether a chat's request to the editor is refused, asked while the page waits for the answer: the window never
/// starts an editor (its leaf retired at the second window's start), the editor is the other window's (or there is
/// none), or its nvim is not running (ended, or started again and not yet ready), whose keys would reach nothing. The refusal's words; `None` when the request may go.
pub fn editor_request_refusal(w: &Window, cx: &Cx, win: WinId) -> Option<String> {
    let editor = ModuleId::editor();
    // An editor marked gone cannot be shown.
    match w.layout.try_borrow() {
        Ok(layout) if layout.is_gone(&editor) => return Some(LayoutError::Gone(editor).to_string()),
        Ok(_) => {}
        Err(_) => return Some(feedback::LAYOUT_CHANGING_TEXT.into()),
    }
    match w.presence() {
        EditorPresence::Owned => {}
        EditorPresence::Proxy | EditorPresence::None => return Some(feedback::NO_EDITOR_HERE_TEXT.into()),
    }
    // An ended editor is not gone: it is refused here until its new nvim runs.
    if w.editor_run != super::super::window::EditorRun::Running {
        return Some(feedback::EDITOR_NOT_RUNNING_TEXT.into());
    }
    // Running, not ready: an nvim that exited is not ready either, and keys sent to it would reach nothing.
    if !cx.reads.editor_running(win) {
        return Some(feedback::EDITOR_NOT_READY_TEXT.into());
    }
    None
}

/// A chat's request to this window's editor: checked, shown and given the keys, then typed the request's keys.
/// No keys (window 2 asking only for the editor) types nothing. A refusal is returned to the caller, whose panel
/// may live in another process.
pub fn editor_request(w: &mut Window, cx: &Cx, win: WinId, keys: String, batch: &mut Batch) -> Result<(), String> {
    if let Some(why) = editor_request_refusal(w, cx, win) {
        return Err(why);
    }
    let editor = ModuleId::editor();
    modules::show_on_screen(w, cx, win, &editor, batch).map_err(|err| feedback::layout_refusal_text(&err))?;
    give_keys(w, win, &editor, vec![], batch);
    if !keys.is_empty() {
        batch.push_effect(HostOp::Editor(EditorCall::SendKeys { win, keys }));
    }
    Ok(())
}

/// Brings the chat view `chat` back for a reveal that leaves the keys where they are: shown where it was
/// hidden from and, when it sits in a tabbed or stacked container, its tab drawn. A hidden view's
/// container may have gone on drawing a sibling, so showing it alone can leave it on a tab nobody sees;
/// both steps are taken whichever place it started from. `Ok((changed, drawn))`: `drawn` is `false`
/// when its tab stays behind the keys' own (drawing it would hide them), so the caller can say so
/// instead.
pub fn reveal_hidden(layout: &mut crate::layout::Layout, chat: &ModuleId) -> Result<(bool, bool), LayoutError> {
    let shown = layout.show(chat)?;
    let tabbed_away = layout.is_tabbed_away(chat);
    let brought = tabbed_away && layout.bring_forward(chat);
    Ok((shown || brought, !tabbed_away || brought))
}

/// A card arrived in chat view `view` while it is off screen, and the attention policy brings it back
/// ([`ChatEvent::Reveal`]): [`reveal_hidden`] on the window's layout, laid out when it changed anything. Whether its
/// tab stays behind the keys' is read by [`attention_revealed`] afterwards for its toast.
pub fn reveal(w: &mut Window, win: WinId, view: ViewId, batch: &mut Batch) {
    let chat = view.module();
    let layout = w.layout.clone();
    let revealed = match layout.try_borrow_mut() {
        Ok(mut layout) => reveal_hidden(&mut layout, &chat),
        Err(_) => {
            w.warn(format!(
                "[attention] BUG: the layout was borrowed when {chat} was revealed"
            ));
            return;
        }
    };
    match revealed {
        Ok((true, _)) => w.changed_by_controller(win, batch),
        Ok((false, _)) => {}
        Err(err) => w.warn(format!("[attention] could not reveal {chat}: {err}")),
    }
}

#[cfg(test)]
mod tests;
