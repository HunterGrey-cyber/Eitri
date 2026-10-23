//! Who holds the keys in a window: the requests that give them (a module key, a strip entry, a HINT landing,
//! `Ctrl+h/j/k/l`, the top bar's way back, the `?` sheet closing), the owner the host reports and what follows
//! from it (the focus history, the sheet's way back, a float's raise), and the cursor each module draws.
//!
//! A request is a `Focus` op the host may refuse, and nothing here assumes it took: what holds the keys is what
//! the host reports ([`HostEvent::NativeFocus`]), and every cursor follows that report and whether the window is
//! active. So the cursors agree with one another by construction, and a refused grab shows as the focus the
//! window really has.
//!
//! The report is heard after the batch whose op caused it, when later ops of that batch may have changed the
//! layout. So the owner's history and the sheet's way back are judged as they stood when the grab happened: a
//! module this window asked to take the keys was on screen when it asked, so a later op of the same batch that
//! hid it does not undo that it held them. The layout's own `focus` is only ever a module the layout still
//! allows.

use super::grid::{order, would_raise};
use super::host::*;
use super::module::ModuleState;
use super::window::{WinId, Window};
use crate::keymap::keys_return::{AtClose, KeysReturn};
use crate::layout::{self, Direction, FocusHistory, LayoutError, ModuleId, ModuleKind, Nav, Zoom};
use crate::tabs::ViewId;
use std::collections::{BTreeMap, BTreeSet};

/// The top-bar item `Ctrl+k` with nothing above lands on: the host gives the keys to the first item showing from
/// here on (a tray chip, or the reload button when the tray is empty), and brings a hidden top bar out first.
pub const FIRST_TOP_BAR_ITEM: u32 = 0;

/// The keys' state of one window.
#[derive(Debug, Default)]
pub struct FocusState {
    /// The module that held the keys before the current one: `prefix ;`'s target.
    pub history: FocusHistory,
    /// Where the `?` sheet found the keys, so closing it by its own keys can give them back.
    pub keys_return: KeysReturn,
    /// What the host last reported holds the keys.
    pub native: Option<FocusOwner>,
    /// Whether the window is active, as last reported.
    pub active: bool,
    /// Each module's last "has the keys" answer, so only changes are told.
    pub has_keys: BTreeMap<ModuleId, bool>,
    /// The idle that restacks the float band after a float took the keys, while one is armed.
    pub raise_timer: Option<TimerToken>,
    /// Whether the armed restack follows a raise, which changed the layout's z-order: the chrome is derived
    /// again for that, since what the float covers changed.
    pub raised: bool,
    /// The modules this window's batches asked to take the keys, whose report has not been heard yet. Each was
    /// on screen when it was asked for; the report of one is judged by that.
    pub asked: BTreeSet<ModuleId>,
    /// A zoom the `?` sheet ended, waiting to be put back around the module given the keys back once its grab is
    /// reported: the zoom unmaps the chat the keys are leaving, so put back before the grab took it would leave a
    /// refused grab's keys in a module nobody can see.
    pub zoom_on_grab: Option<(Zoom, ModuleId)>,
}

impl FocusState {
    /// Notes that `batch` is about to ask `to` to take the keys, while the layout shows it, so its report counts
    /// even if a later op of the batch hides it. A grab of the module that already holds the keys moves nothing,
    /// so the toolkit reports nothing for it, and a request kept for it would vouch for some later report; unless
    /// an earlier grab of the same batch takes the keys away first, after which this one does report.
    pub(super) fn ask(&mut self, to: &ModuleId, batch: &Batch) {
        let moves_first = batch.ops().any(|op| matches!(op, HostOp::Focus { .. }));
        if moves_first || self.native_module() != Some(to) {
            self.asked.insert(to.clone());
        }
    }

    /// The module the host last reported holds the keys; `None` for the top bar, the rename entry or nothing.
    pub fn native_module(&self) -> Option<&ModuleId> {
        match &self.native {
            Some(FocusOwner::Module(module)) => Some(module),
            _ => None,
        }
    }
}

impl FocusChain {
    /// The module nearest the focus destination among `modules`, walking the chain innermost first; `None` when
    /// the keys are in none of them (the top bar, or no focus at all). Named by its id, never its position.
    pub fn owner(&self, modules: &[ModuleId]) -> Option<ModuleId> {
        self.0.iter().find(|id| modules.contains(id)).cloned()
    }
}

/// Which modules' "has the keys" answer changed, as `(id, now)`, in `modules`' order. `last` holds each module's
/// previous answer (none before its first report), `focused` is the module holding the keys and `active` whether
/// the window is. Only changes are reported, so a focus move between two widgets inside one module tells nobody,
/// and every module gets one report the first time it is seen: at startup, or whenever it joined the window.
pub fn focus_change(
    last: &BTreeMap<ModuleId, bool>,
    modules: &[ModuleId],
    focused: Option<&ModuleId>,
    active: bool,
) -> Vec<(ModuleId, bool)> {
    modules
        .iter()
        .filter_map(|id| {
            let now = focused == Some(id) && active;
            (last.get(id) != Some(&now)).then(|| (id.clone(), now))
        })
        .collect()
}

/// Gives `to` the keys with one `Focus` op, its grab's tail `on_grab` carried out only if the grab takes; `false`,
/// with nothing asked, for a hidden module: a toolkit may grab an unmapped widget, and the keys would then be in
/// a module nobody can see.
fn grab(w: &mut Window, win: WinId, to: &ModuleId, on_grab: Vec<HostOp>, batch: &mut Batch) -> bool {
    match w.layout.try_borrow().map(|layout| layout.is_shown(to)) {
        Ok(false) => {
            w.say(format!("[focus] {to} is hidden; the keys stay where they are"));
            return false;
        }
        Ok(true) => {}
        // Nothing the controller runs holds the layout while it moves the keys; this line is how a shell
        // closure that does would show up.
        Err(_) => w.warn(format!(
            "[focus] BUG: the layout was borrowed for writing when {to} was given the keys"
        )),
    }
    w.focus.ask(to, batch);
    batch.push_layout(HostOp::Focus {
        win,
        to: FocusTo::Module(to.clone()),
        on_grab,
    });
    true
}

/// Gives `to` the keys by any route that chooses a module. A module on a tab its container does not draw has
/// that tab chosen first, as a layout change whose ops map it, give it the keys and only then unmap the tab it
/// replaces: a bare grab would put the keys in a host nobody can see. A hidden module gets nothing. Whether a
/// `Focus` op was asked for; the host's report says whether it took.
pub fn give_keys(w: &mut Window, win: WinId, to: &ModuleId, on_grab: Vec<HostOp>, batch: &mut Batch) -> bool {
    let tabbed_away = w.layout.try_borrow().is_ok_and(|layout| layout.is_tabbed_away(to));
    if !tabbed_away {
        return grab(w, win, to, on_grab, batch);
    }
    let layout = w.layout.clone();
    let Ok(switched) = layout
        .try_borrow_mut()
        .map(|mut layout| order::tab_switch(&mut layout, to))
    else {
        w.warn(format!(
            "[focus] BUG: the layout was borrowed when {to}'s tab was chosen"
        ));
        return false;
    };
    match switched {
        Ok(next) => {
            let next = next.unwrap_or_else(|| to.clone());
            w.changed_with_focus(win, Some((next, on_grab)), batch);
            true
        }
        Err(err) => {
            w.say(format!("[focus] {to}: its tab could not be chosen: {err}"));
            false
        }
    }
}

/// What arriving in `to` does once it has the keys: a chat view lands where its reader left it, or, with
/// `card_first`, on its oldest pending card when one waits. Nothing for any other module. A keyboard move between
/// modules never jumps to a card: the reader moving through the grid by direction or by history keeps their place,
/// and only a route that picks the chat by name (its key, its strip entry, a HINT label) goes to what waits there.
pub fn arrive_tail(win: WinId, to: &ModuleId, card_first: bool) -> Vec<HostOp> {
    ViewId::of_module(to)
        .map(|view| HostOp::Chat(ChatOp::Arrive { win, view, card_first }))
        .into_iter()
        .collect()
}

/// Ends a zoom as a change of the layout; nothing when none is on.
fn unzoom(w: &mut Window, win: WinId, batch: &mut Batch) {
    if w.layout.try_borrow_mut().is_ok_and(|mut layout| layout.unzoom()) {
        w.say("[module_grid] unzoomed".into());
        w.changed_by_controller(win, batch);
    }
}

/// A module takes the keys by keyboard (`Ctrl+h/j/k/l`, `select.last`, `select.next`): any zoom ends first, as
/// tmux's `select-pane` does, then `to` gets the keys and, only if its grab takes, arrives where its reader left
/// it. Whether a `Focus` op was asked for.
pub fn land_by_key(w: &mut Window, win: WinId, to: &ModuleId, batch: &mut Batch) -> bool {
    unzoom(w, win, batch);
    give_keys(w, win, to, arrive_tail(win, to, false), batch)
}

/// Gives `to` the keys and arrives, on a waiting card first (a module key, a strip entry, `eitri.layout.focus`).
/// Refused before anything moves for a module the keys cannot go to, so a refused focus does not end a zoom on its
/// way to being refused; a zoom ends only when it would keep `to` off screen, so landing on the zoomed module
/// itself keeps it.
pub fn focus_and_arrive(w: &mut Window, win: WinId, to: &ModuleId, batch: &mut Batch) -> Result<(), LayoutError> {
    let layout = w.layout.clone();
    let zoom_hides = match layout.try_borrow() {
        Ok(layout) => {
            layout.can_focus(to)?;
            layout.zoom_hides(to)
        }
        Err(_) => {
            w.warn(format!("[focus] BUG: the layout was borrowed when {to} was landed on"));
            return Ok(());
        }
    };
    if zoom_hides {
        unzoom(w, win, batch);
    }
    give_keys(w, win, to, arrive_tail(win, to, true), batch);
    Ok(())
}

/// `Ctrl+h/j/k/l` from `from`, by the geometry the grid would arrange now. A module that way is landed on by key;
/// `Up` with nothing above goes to the top bar, which sits above every module; anything else with nothing there
/// is tmux's no-op at the edge of its grid, and `false` lets the key go on to the module it was pressed in.
pub fn move_focus(
    w: &mut Window,
    reads: &dyn HostReads,
    win: WinId,
    from: &ModuleId,
    direction: Direction,
    batch: &mut Batch,
) -> bool {
    let layout = w.layout.clone();
    let nav = match layout.try_borrow() {
        Ok(layout) => frame_with(reads, win, reads.grid_size(win), |frame| {
            layout::navigate(&layout, from, direction, frame)
        }),
        Err(_) => {
            w.warn(format!(
                "[pane_switch] BUG: the layout was borrowed when {from} {direction:?} ran"
            ));
            return false;
        }
    };
    match nav {
        Nav::Module(to) => {
            let asked = land_by_key(w, win, &to, batch);
            w.say(format!(
                "[pane_switch] {from} {direction:?} -> {to} (focus asked={asked})"
            ));
            true
        }
        Nav::TopBar => {
            batch.push_layout(HostOp::Focus {
                win,
                to: FocusTo::TopBarItem(FIRST_TOP_BAR_ITEM),
                on_grab: vec![],
            });
            w.say(format!("[pane_switch] {from} {direction:?} -> the top bar"));
            true
        }
        Nav::Nothing => {
            w.say(format!(
                "[pane_switch] {from} {direction:?} -> no module that way, not moving"
            ));
            false
        }
    }
}

/// The top bar gave the keys up by its own key: they go back to the module that last had them, as a bare grab.
pub fn return_to_pane(w: &mut Window, win: WinId, batch: &mut Batch) {
    let Ok(target) = w.layout.try_borrow().map(|layout| layout.focus().clone()) else {
        w.warn("[focus] BUG: the layout was borrowed when the top bar gave the keys back".into());
        return;
    };
    grab(w, win, &target, vec![], batch);
}

/// The host reports who holds the keys. For a module: the history and the sheet's way back hear of it if the
/// layout allowed it at the moment of the grab, the layout's `focus` follows only if the layout allows it now,
/// a float that took the keys is restacked from an idle (never inside the report). Then every module's cursor
/// follows the report and `active`, and last the chrome's text is derived again for a selection the move ended.
pub fn on_native_focus(
    w: &mut Window,
    win: WinId,
    owner: Option<FocusOwner>,
    active: bool,
    op_index: Option<u32>,
    batch: &mut Batch,
) {
    w.say(format!(
        "[pane_focus] active={active} owner={owner:?} op_index={op_index:?}"
    ));
    let before = w.focus.native_module().cloned();
    w.focus.native = owner;
    w.focus.active = active;
    let refresh = match w.focus.native_module().cloned() {
        Some(module) => owner_moved(w, &module, op_index, before.as_ref() != Some(&module)),
        None => false,
    };
    cursor_signals(w, win, batch);
    zoom_after_report(w, win, op_index, batch);
    // The keys moving to another module ended any selection; the top bar says so.
    if refresh {
        w.layout_touched();
    }
}

/// The window became active or stopped being: the cursors follow.
pub fn on_active(w: &mut Window, win: WinId, active: bool, batch: &mut Batch) {
    w.say(format!("[pane_focus] active={active}"));
    w.focus.active = active;
    cursor_signals(w, win, batch);
}

/// What a module's taking the keys changes besides the cursors; `moved` says it is not the module reported last.
/// Whether the selection's chrome is to be redrawn: the owner or the layout's `focus` changed.
fn owner_moved(w: &mut Window, owner: &ModuleId, op_index: Option<u32>, moved: bool) -> bool {
    let asked = w.focus.asked.remove(owner);
    let layout = w.layout.clone();
    let Ok(mut layout) = layout.try_borrow_mut() else {
        w.warn(format!(
            "[pane_focus] BUG: the layout was borrowed when the keys moved to {owner}"
        ));
        return false;
    };
    let now = layout.can_focus(owner);
    // A module hidden now held the keys only if they were asked for it in the batch that was applying: it was on
    // screen then. Any other hidden or unknown module the toolkit names is a widget the keys should not be in.
    let held = match &now {
        Ok(()) => true,
        Err(LayoutError::Hidden(_)) => op_index.is_some() && asked,
        Err(_) => false,
    };
    // A report heard after its batch, for a module the batch has since taken off screen (another tab drawn in
    // its place, a zoom put back around another module), is a grab the batch went past: the layout keeps the
    // focus the batch left, since taking this one would draw its tab again or end that zoom. The report of the
    // grab the batch ended on follows and settles the focus.
    let passed = op_index.is_some() && now.is_ok() && !layout.is_visible(owner);
    let raises = !passed && would_raise(&layout, owner);
    let previous = layout.focus().clone();
    let set = match now {
        Ok(()) if passed => Ok(()),
        Ok(()) => layout.set_focus(owner),
        Err(err) => Err(err),
    };
    let floating = layout.is_floating(owner);
    let keeps = layout.focus().clone();
    drop(layout);
    if held {
        w.focus.history.note(owner);
        // A return is kept only for a sheet in the base view.
        w.focus.keys_return.focus_moved(owner, &ModuleId::agent());
    }
    if passed {
        w.say(format!(
            "[pane_focus] {owner} held the keys mid-batch and is off screen now; the layout keeps {keeps}"
        ));
    }
    match set {
        Ok(()) if passed => {}
        Ok(()) if floating => {
            // A float given the keys is on top of the z-order now; its host goes to the top of the grid once this
            // report is over, from an idle, never inside it.
            w.focus.raised |= raises;
            if w.focus.raise_timer.is_none() {
                w.focus.raise_timer = Some(w.arm_idle());
            }
        }
        Ok(()) => {}
        Err(err) => w.warn(format!("[pane_focus] {err}; the layout keeps {keeps}")),
    }
    moved || previous != keeps
}

/// The raise's idle fired: the float band restacked, and after a raise the chrome and the saver hear of the new
/// z-order. `false` when `token` is not this window's raise.
pub fn raise_timer_fired(w: &mut Window, win: WinId, token: TimerToken, batch: &mut Batch) -> bool {
    if w.focus.raise_timer != Some(token) {
        return false;
    }
    w.focus.raise_timer = None;
    w.restack(win, batch);
    if std::mem::take(&mut w.focus.raised) {
        w.layout_touched();
        w.saver_changed();
    }
    true
}

/// The modules whose cursors are told: every module of the layout that has a host here.
fn tracked(w: &Window) -> Vec<ModuleId> {
    let Ok(layout) = w.layout.try_borrow() else {
        return w.focus.has_keys.keys().cloned().collect();
    };
    layout
        .modules()
        .into_iter()
        .filter(|id| {
            matches!(
                w.lifecycle.state(id),
                Some(ModuleState::Hosted | ModuleState::Shown | ModuleState::Hidden)
            )
        })
        .collect()
}

/// The window's modules changed without a focus change: a module the host gave the keys before it was declared
/// (a new terminal or chat view takes them while it is placed, and its declaration comes after), or one back from
/// a kill or from the other window, hears its answer now, since the report that named it found it untracked and
/// none may follow.
pub fn modules_changed(w: &mut Window, win: WinId, batch: &mut Batch) {
    cursor_signals(w, win, batch);
}

/// Tells each module whose "has the keys" answer changed, from the reported owner and the window's activity. A
/// module no longer tracked loses its last answer, so its next instance gets a first report of its own.
fn cursor_signals(w: &mut Window, win: WinId, batch: &mut Batch) {
    let modules = tracked(w);
    w.focus.has_keys.retain(|id, _| modules.contains(id));
    let changes = focus_change(&w.focus.has_keys, &modules, w.focus.native_module(), w.focus.active);
    for (id, has_keys) in changes {
        w.focus.has_keys.insert(id.clone(), has_keys);
        w.say(format!("[pane_focus] {id} has_keys={has_keys}"));
        cursor_signal(win, &id, has_keys, &modules, batch);
    }
}

/// One module's change, as its kind hears it.
fn cursor_signal(win: WinId, id: &ModuleId, has_keys: bool, modules: &[ModuleId], batch: &mut Batch) {
    match id.kind() {
        ModuleKind::Editor => {
            batch.push_effect(HostOp::Editor(EditorCall::SetFocused { win, focused: has_keys }));
            // The typing window only counts while the editor holds the keys.
            batch.push_effect(HostOp::Chat(ChatOp::EditorHasKeys { win, has_keys }));
        }
        // Each chat view's page is told for itself, so exactly one is told it has the keys.
        ModuleKind::Agent => {
            if let Some(view) = ViewId::of_module(id) {
                batch.push_effect(HostOp::Chat(ChatOp::PaneFocused {
                    win,
                    view,
                    focused: has_keys,
                }));
            }
        }
        // One solid cursor among the terminals: the one taking the keys draws it, and every other live terminal
        // is told it has not got them.
        ModuleKind::Terminal => {
            batch.push_effect(HostOp::Terminal(TermOp::SetFocused {
                win,
                module: id.clone(),
                focused: has_keys,
            }));
            if has_keys {
                for other in modules.iter().filter(|m| m.kind() == ModuleKind::Terminal && *m != id) {
                    batch.push_effect(HostOp::Terminal(TermOp::SetFocused {
                        win,
                        module: other.clone(),
                        focused: false,
                    }));
                }
            }
        }
        // A page hears this as its `focus` event; a Lua page that is no plugin's ignores it.
        ModuleKind::LuaWebview => batch.push_effect(HostOp::Plugin(PluginOp::HasKeys {
            win,
            module: id.clone(),
            has_keys,
        })),
        ModuleKind::Canvas => {}
    }
}

/// The `?` sheet of opening `opening` was closed (`forced` when something other than its own keys or a click
/// outside it closed it). When the latest opening's sheet closed by its own keys and the keys are still in the
/// chat, they go back to the module that held them: the chat hidden again if the opening showed it (the keys to
/// the module the layout chooses first, then the unmap), the keys given back, and only then the zoom the opening
/// ended put back, since giving the keys to a module on a tab not drawn ends any zoom. A close of an earlier
/// opening changes nothing.
pub fn keymap_closed(
    w: &mut Window,
    reads: &dyn HostReads,
    win: WinId,
    view: ViewId,
    opening: u64,
    forced: bool,
    batch: &mut Batch,
) {
    // Only an opening in the base view keeps a return: a chat view holding the keys opens the sheet in its own
    // page, where the keys already are.
    let sheet = ModuleId::agent();
    let focused = w.focus.native_module().cloned();
    let layout = w.layout.clone();
    let ret = match layout.try_borrow() {
        Ok(layout) => {
            let shown = |id: &ModuleId| layout.is_shown(id);
            let zoom_hides = |id: &ModuleId| layout.zoom_hides(id);
            let at = AtClose {
                focused: focused.as_ref(),
                zoom: layout.zoom(),
                zoom_hides: &zoom_hides,
                sheet_shown: layout.is_shown(&sheet),
                shown: &shown,
            };
            w.focus.keys_return.closed(opening, !forced, at, &sheet)
        }
        Err(_) => {
            w.warn(format!(
                "[keys_return] BUG: the layout was borrowed when {view:?}'s sheet closed"
            ));
            return;
        }
    };
    let Some(ret) = ret else { return };
    let mut handed = None;
    if ret.hide_sheet {
        let hidden = match layout.try_borrow_mut() {
            Ok(mut layout) => frame_with(reads, win, reads.grid_size(win), |frame| {
                layout::hide(&mut layout, &sheet, frame)
            }),
            Err(_) => {
                w.warn(format!(
                    "[keys_return] BUG: the layout was borrowed when {sheet} was hidden again"
                ));
                return;
            }
        };
        match hidden {
            Ok(next) => {
                w.changed_with_focus(win, next.clone().map(|next| (next, vec![])), batch);
                handed = next;
            }
            Err(err) => w.warn(format!("[keys_return] could not hide {sheet} again: {err}")),
        }
    }
    // The hide may have handed the keys to that very module already.
    if handed.as_ref() != Some(&ret.focus) && !give_keys(w, win, &ret.focus, vec![], batch) {
        w.say(format!("[keys_return] {} could not take the keys back", ret.focus));
        return;
    }
    let Some(zoom) = ret.zoom else { return };
    // A grab of the module already holding the keys is never reported (it is not asked), so nothing is waited for.
    if w.focus.asked.contains(&ret.focus) {
        w.focus.zoom_on_grab = Some((zoom, ret.focus));
    } else {
        put_zoom_back(w, win, &zoom, &ret.focus, batch);
    }
}

/// A report of where the keys are, as it bears on a zoom waiting for a grab: the module it waits for took the
/// keys, so the zoom goes back around it; or, heard outside any batch, the keys went somewhere else, so it never
/// will. A report heard while a batch applied names a grab the batch passed on its way and leaves the wait alone.
fn zoom_after_report(w: &mut Window, win: WinId, op_index: Option<u32>, batch: &mut Batch) {
    let Some((zoom, keeps)) = w.focus.zoom_on_grab.take() else {
        return;
    };
    if w.focus.native_module() == Some(&keeps) {
        put_zoom_back(w, win, &zoom, &keeps, batch);
    } else if op_index.is_some() {
        w.focus.zoom_on_grab = Some((zoom, keeps));
    } else {
        w.say(format!(
            "[keys_return] the keys went elsewhere; the zoom {zoom:?} stays off"
        ));
    }
}

/// The host refused to give `module` the keys: a zoom waiting for that grab stays off, since the keys are still in
/// the module it would hide.
pub(super) fn grab_refused(w: &mut Window, module: &ModuleId) {
    w.focus.asked.remove(module);
    if w.focus.zoom_on_grab.as_ref().is_some_and(|(_, keeps)| keeps == module) {
        if let Some((zoom, _)) = w.focus.zoom_on_grab.take() {
            w.say(format!(
                "[keys_return] {module} could not take the keys back; the zoom {zoom:?} stays off"
            ));
        }
    }
}

fn put_zoom_back(w: &mut Window, win: WinId, zoom: &Zoom, keeps: &ModuleId, batch: &mut Batch) {
    let restored = w
        .layout
        .clone()
        .try_borrow_mut()
        .is_ok_and(|mut layout| layout.restore_zoom(zoom, keeps));
    if restored {
        w.say(format!("[module_grid] zoom {zoom:?} put back around {keeps}"));
        w.changed_by_controller(win, batch);
    } else {
        w.say(format!(
            "[keys_return] the zoom {zoom:?} could not be put back around {keeps}"
        ));
    }
}

#[cfg(test)]
mod tests;
