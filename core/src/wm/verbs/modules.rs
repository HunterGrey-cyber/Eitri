//! The module verbs: a module's key with each kind's rule, `module.hide`, the hide key's module key, `module.kill` and
//! its y/n, a new terminal
//! or chat view (`instance.new`, a split key's new instance, `eitri.layout.open`), a split key's placement of a
//! module that exists, the covered chat's answer gate and its reveal, a plugin's follow commands, a tray chip, and
//! the Lua requests that name a module. Each is decided on the window's layout and answered with the batch that
//! shows its change, the keys given in the order that never leaves them on a host leaving the screen.
//!
//! A new instance takes two batches, because a batch is computed whole and a module goes into the layout only once
//! its surface exists: the first asks for the surface (`CreateSurface`), and the driver's [`HostEvent::Created`]
//! for it brings the second, which places it, adds it hidden to every other workspace and declares it afresh. A
//! refused create adds nothing anywhere. A terminal number whose pane the window still holds idle, from a kill or a
//! retirement, is placed at once with no create, its next directory set on the pane; a chat view's number is asked
//! for again, and the host takes its idle page back with a fresh document.
//!
//! The kill's y/n is the chrome's ([`Prompt`]): while it is shown, the window stage answers it before the prefix
//! sees the key. The last module's kill is the window close, whose question is still the shell's, so it goes back
//! to the shell as its verb.
//!
//! Two arms the shell still carries out make their new instance here too: `scratchpad.show` for a named terminal or
//! chat view that does not exist yet ([`scratch_new`]), made straight into the scratchpad, and `tab.break`
//! ([`tab_break`]), whose new chat view takes the base view's active tab.

use super::super::chrome::{self, Prompt};
use super::super::close::{self, CloseState, KillScope, PromptKey};
use super::super::feedback;
use super::super::focus::{self, arrive_tail, focus_and_arrive, give_keys};
use super::super::grid::order;
use super::super::host::*;
use super::super::instances::plan_new_instance;
use super::super::module::{ModuleState, SurfaceDecl};
use super::super::moves;
use super::super::window::{
    ActivationToken, EditorPresence, EditorRun, LinkSide, PeerId, QueuedOpen, WinId, Window, WindowRole,
};
use crate::keymap::action::{Action, SplitCwd};
use crate::keymap::Keymap;
use crate::layout::rules::{without_keys, At, OpenPoint};
use crate::layout::{
    self, covering, declared_placement, Axis, Direction, KeyAction, Layout, LayoutError, ModuleDecl, ModuleId,
    ModuleKind, OpenRule, OpenRules, Opened, Placement, Reopen, ScratchShow, SplitChoice,
};
use crate::tabs::ViewId;
use std::path::PathBuf;

/// What the shell's config and plugins tell a window's module verbs: the open rules, the module declarations
/// (where a Lua panel, an open rule or a killed module goes), the plugin modules that start hidden with each one's
/// own rule, the project root a new terminal's shell starts in, and the workspaces `init.lua` defined, which a
/// workspace first made by name at runtime starts from.
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleSetup {
    pub rules: OpenRules,
    pub decls: Vec<ModuleDecl>,
    pub start_hidden: Vec<(ModuleId, Option<OpenRule>)>,
    pub project_dir: PathBuf,
    pub workspaces: super::workspaces::Definitions,
}

impl Default for ModuleSetup {
    fn default() -> Self {
        ModuleSetup {
            rules: OpenRules::default(),
            decls: Vec::new(),
            start_hidden: Vec::new(),
            project_dir: PathBuf::from("."),
            workspaces: Vec::new(),
        }
    }
}

/// What a window's module verbs wait for: the module the shown y/n would kill (or whether it asks to start the
/// editor window), the new instances whose surface was asked for, and the splits waiting for the directory of the
/// terminal they were pressed in.
#[derive(Debug, Default)]
pub struct ModuleVerbs {
    pub(in crate::wm) kill: Option<ModuleId>,
    /// The other window the shown y/n would end: asked instead of a kill while a module is frozen for a move that
    /// window stopped answering.
    pub(in crate::wm) end_peer: Option<PeerId>,
    /// The shown y/n asks whether to start the editor window, in a window 2 left without one.
    start_editor: bool,
    opens: Vec<PendingOpen>,
    cwd_reads: Vec<PendingCwd>,
    /// The next directory read's own number, which its answer carries back.
    next_cwd_read: u64,
}

impl ModuleVerbs {
    /// The y/n's question is about to change: whatever an earlier one would have done on `y` (a kill, ending the
    /// other window, starting the editor window) is forgotten, so a later `y` answers only the question shown.
    pub(in crate::wm) fn clear_question(&mut self) {
        self.kill = None;
        self.end_peer = None;
        self.start_editor = false;
    }

    /// The new instances whose surface was asked for and has not come yet; their numbers are not free.
    pub(crate) fn pending_ids(&self) -> Vec<ModuleId> {
        self.opens.iter().map(|open| open.id.clone()).collect()
    }
}

#[derive(Debug)]
struct PendingOpen {
    id: ModuleId,
    kind: SurfaceKind,
    how: OpenHow,
    /// A Lua request's own words, when one asked: its refusals are logged under them, never toasted.
    quiet: Option<String>,
}

#[derive(Debug)]
struct PendingCwd {
    read: u64,
    target: ModuleId,
    axis: Axis,
}

/// Where a new terminal or chat view goes.
#[derive(Debug, Clone, PartialEq)]
pub enum OpenHow {
    /// A split key: beside this module along this axis. The user said where, so no open rule is read.
    Beside(ModuleId, Axis),
    /// Where the open rules say for `point`, beside the module with the keys when it is placed; `explicit` is a
    /// call's own options (`eitri.layout.open(kind, opts)`), which decide in place of the rules. A chat view
    /// opened at [`OpenPoint::Break`] takes the base view's active tab rather than a new empty one.
    Rule {
        point: OpenPoint,
        explicit: Option<OpenRule>,
    },
    /// Straight into the scratchpad, shown and given the keys: a float centred at its open rule's float size, else
    /// the default share of the grid.
    Scratch,
}

/// What the module verbs read besides the window: the shell's answers, the keymap the refusals name keys from,
/// and the window's module setup.
pub struct Cx<'a> {
    pub reads: &'a dyn HostReads,
    pub keymap: &'a Keymap,
    pub setup: &'a ModuleSetup,
}

/// A module verb was decided by the controller, including the last module's close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ran {
    Done,
}

/// The module family's action from the runner, `origin` saying who asked: a chip shows and focuses and never
/// hides, and a follow command's show keeps the keys where they are.
pub fn run(w: &mut Window, cx: &Cx, win: WinId, action: &Action, origin: &Origin, batch: &mut Batch) -> Ran {
    match action {
        Action::Module(id) if *origin == Origin::Chip => chip(w, cx, win, id, batch),
        Action::Module(id) if *origin == Origin::Follow => follow(w, cx, win, id, true, batch),
        Action::Module(id) if id.kind() == ModuleKind::Terminal => terminal_key(w, cx, win, id, batch),
        Action::Module(id) => module_key(w, cx, win, id, batch),
        Action::ModuleHide => {
            debug_assert!(
                *origin != Origin::Follow,
                "a follow's hide names the module it follows: Controller::follow carries it"
            );
            if let Some(target) = target(w) {
                open_module(w, cx, win, &target, KeyAction::Hide, batch);
            }
        }
        // The hide key waits for a module key in the prefix (`hide_by_key`); on its own it hides nothing.
        Action::ModuleHideByKey => {}
        Action::ModuleKill => return kill_asked(w, cx, win, batch),
        Action::InstanceNew { kind } => {
            let how = OpenHow::Rule {
                point: OpenPoint::NewInstance,
                explicit: None,
            };
            instance_new(w, cx, win, *kind, how, None, None, batch);
        }
        // A split key waits for a module key in the prefix; on its own it places nothing.
        Action::Split { .. } => {}
        other => debug_assert!(false, "{other:?} is not a module verb"),
    }
    Ran::Done
}

/// The module with the keys in the layout: what `module.hide` and `module.kill` act on.
fn target(w: &mut Window) -> Option<ModuleId> {
    let target = w.layout.try_borrow().map(|layout| layout.focus().clone()).ok();
    if target.is_none() {
        w.warn("[wm] BUG: the layout was borrowed when a module verb read its target".into());
    }
    target
}

/// Whether the host reports `id` holds the keys now: where a key typed now would go.
fn has_keys(w: &Window, id: &ModuleId) -> bool {
    w.focus.native_module() == Some(id)
}

/// Whether the window holds a host for `id`: declared and built, whatever the layout says of it now.
fn has_host(w: &Window, id: &ModuleId) -> bool {
    matches!(w.lifecycle.state(id), Some(state) if *state != ModuleState::Declared)
}

/// Whether `id`'s host was left idle by a kill or a retirement: a terminal's pane or a chat view's page that only a
/// new instance brings back. Only a numbered instance is ever left so; the base terminal's kill leaves its pane
/// live in the layout, hidden, to start a fresh shell when it is next shown.
fn idle(w: &Window, id: &ModuleId) -> bool {
    id.number() >= 2
        && matches!(
            w.lifecycle.state(id),
            Some(ModuleState::Killed(_) | ModuleState::Retired)
        )
}

/// Whether `id` may be placed by its name, as a split key or `eitri.layout.split` places it. A numbered terminal
/// or chat view that was killed keeps its host for the next instance of its number, but its pane or page is idle:
/// placed again by name it would be a terminal with no shell or a view with no tab. Only a new instance brings
/// either back; the base chat and every other module are judged by whether the window has a host for them.
pub fn placeable_by_id(id: &ModuleId, live: bool) -> Result<(), LayoutError> {
    let pooled = match id.kind() {
        ModuleKind::Terminal => true,
        ModuleKind::Agent => id.number() >= 2,
        _ => false,
    };
    if pooled && !live {
        return Err(LayoutError::NotInTree(id.clone()));
    }
    Ok(())
}

/// The chat key's view: the chat view with the keys, so the key still hides the one you are in; else the view
/// holding the oldest waiting card, when the layout holds it; else the most recently used chat view, then the base
/// view.
pub fn chat_target(layout: &Layout, oldest: Option<ViewId>) -> ModuleId {
    if layout.focus().kind() == ModuleKind::Agent {
        return layout.focus().clone();
    }
    if let Some(view) = oldest.filter(|view| layout.contains(&view.module())) {
        return view.module();
    }
    layout::module_target(layout, &ModuleId::agent())
}

/// What revealing a covered chat does: the covering floats to stow (bottom first, each with its scratch flag) --
/// every scratch float that does not hold the keys -- and whether the chat is still covered after them. A plain
/// float stays, since a reveal never sinks one, and so does a float with the keys, where the user is working.
pub fn covered_reveal(covering: &[(ModuleId, bool)], keys: &ModuleId) -> (Vec<ModuleId>, bool) {
    let (stow, stay): (Vec<_>, Vec<_>) = covering.iter().partition(|(id, scratch)| *scratch && id != keys);
    (stow.into_iter().map(|(id, _)| id.clone()).collect(), !stay.is_empty())
}

/// The floats over `module` in the arrangement the grid would give the layout now, bottom first. A layout with no
/// float has nothing over anything, and nothing is arranged for it.
fn covering_now(layout: &Layout, reads: &dyn HostReads, win: WinId, module: &ModuleId) -> Vec<ModuleId> {
    if layout.floats().is_empty() || !layout.contains(module) {
        return Vec::new();
    }
    frame_with(reads, win, reads.grid_size(win), |frame| {
        covering(&layout::arrange(layout, frame), module)
    })
}

/// What a covered chat's refusal says, by the topmost float over it.
fn covered_refusal(w: &Window, cx: &Cx, layout: &Layout, chat: &ModuleId, top: &ModuleId) -> String {
    let scratch = layout.float_of(top).is_some_and(|float| float.scratch);
    let key = if scratch {
        super::key_text(cx.keymap, &Action::ScratchpadShow { module: None })
    } else {
        super::key_text(cx.keymap, &Action::FloatToggle)
    };
    feedback::covered_text(&chrome::title(w, chat), &chrome::title(w, top), &key, scratch)
}

/// Whether a card answer from chat view `view` may be taken: `Ok` only when nothing covers the view, as far as the
/// controller can tell. A layout being written right now, a window it does not hold, or a view this window's layout
/// does not hold refuse, so an answer it cannot judge waits on its card.
pub fn answer_gate(w: Option<&Window>, cx: &Cx, win: WinId, view: ViewId) -> Result<(), String> {
    let Some(w) = w else {
        return Err(feedback::LAYOUT_CHANGING_TEXT.into());
    };
    let Ok(layout) = w.layout.try_borrow() else {
        return Err(feedback::LAYOUT_CHANGING_TEXT.into());
    };
    let chat = view.module();
    if !layout.contains(&chat) {
        return Err(feedback::not_in_window_text(&chat));
    }
    match covering_now(&layout, cx.reads, win, &chat).last() {
        None => Ok(()),
        Some(top) => Err(covered_refusal(w, cx, &layout, &chat, top)),
    }
}

/// A covered chat view revealed before something opens in it with the keys (`prefix ?`, `prefix :`) or a card
/// shows there: the scratch floats over it that do not hold the keys are stowed, the keys staying where they are.
/// The refusal's text when a float still covers it; `None` when it can take the keys in view, a floating chat
/// included, which taking the keys raises over every other float.
pub fn reveal_chat(w: &mut Window, cx: &Cx, win: WinId, view: ViewId, batch: &mut Batch) -> Option<String> {
    let chat = view.module();
    let layout = w.layout.clone();
    let (stow, mut still) = {
        let Ok(layout) = layout.try_borrow() else {
            w.warn(format!("[wm] BUG: the layout was borrowed when {chat} was revealed"));
            return Some(feedback::LAYOUT_CHANGING_TEXT.into());
        };
        if layout.is_floating(&chat) {
            return None;
        }
        let covering: Vec<(ModuleId, bool)> = covering_now(&layout, cx.reads, win, &chat)
            .into_iter()
            .map(|id| {
                let scratch = layout.float_of(&id).is_some_and(|float| float.scratch);
                (id, scratch)
            })
            .collect();
        covered_reveal(&covering, layout.focus())
    };
    for id in stow {
        let shown = match layout.try_borrow_mut() {
            Ok(mut layout) => frame_with(cx.reads, win, cx.reads.grid_size(win), |frame| {
                layout::scratchpad_show(&mut layout, Some(&id), frame)
            }),
            Err(_) => Err(LayoutError::NotInTree(id.clone())),
        };
        match shown {
            Ok(ScratchShow::Stowed(next)) => {
                w.say(format!("[attention] {id}: stowed off {chat}"));
                w.changed_with_focus(win, next.map(|next| (next, vec![])), batch);
            }
            Ok(other) => {
                w.warn(format!(
                    "[attention] {id}: asked to stow, the scratchpad answered {other:?}"
                ));
                w.changed_by_controller(win, batch);
                still = true;
            }
            Err(err) => {
                w.warn(format!("[attention] {id}: not stowed: {err}"));
                still = true;
            }
        }
    }
    if !still {
        return None;
    }
    let layout = layout.try_borrow().ok()?;
    let top = covering_now(&layout, cx.reads, win, &chat).last().cloned()?;
    Some(covered_refusal(w, cx, &layout, &chat, &top))
}

/// A refusal of a layout verb: said on stdout, and the toast gives the layout's reason.
fn refuse_layout(w: &mut Window, win: WinId, err: &LayoutError, batch: &mut Batch) {
    w.say(format!("[modules] refused: {err}"));
    w.refuse(win, feedback::layout_refusal_text(err), batch);
}

/// A refusal in its own words: logged under the Lua request's words when one asked (no toast), else said on stdout
/// with the flash and the toast.
fn refusal(w: &mut Window, win: WinId, quiet: Option<&str>, text: String, batch: &mut Batch) {
    match quiet {
        Some(request) => w.warn(format!("[lua] layout: {request} refused: {text}")),
        None => {
            w.say(format!("[modules] refused: {text}"));
            w.refuse(win, text, batch);
        }
    }
}

/// A change the verb made to the layout, laid out with `to` given the keys in its order (mapped, given them, then
/// what left unmapped). A module still hidden or on a tab not drawn takes the route [`give_keys`] gives it after the
/// change; with nothing changed, it is only given the keys.
fn change_then_give(w: &mut Window, win: WinId, changed: bool, to: &ModuleId, tail: Vec<HostOp>, batch: &mut Batch) {
    if !changed {
        give_keys(w, win, to, tail, batch);
        return;
    }
    let in_view = w.layout.try_borrow().is_ok_and(|layout| layout.is_visible(to));
    if in_view {
        w.changed_with_focus(win, Some((to.clone(), tail)), batch);
    } else {
        w.changed_by_controller(win, batch);
        give_keys(w, win, to, tail, batch);
    }
}

/// `prefix t`'s rule, for every terminal key: the base key reaches the terminal with the keys, else the most recent
/// one, else the base; a numbered key reaches that instance only. A hidden terminal is shown and given the keys
/// (its shell starts once it is shown), a shown one is given them, and the one holding them is hidden, the keys
/// going first to the module the layout chooses. Every case ends a zoom first: hiding a zoomed terminal without
/// that hid everything.
fn terminal_key(w: &mut Window, cx: &Cx, win: WinId, id: &ModuleId, batch: &mut Batch) {
    let target = w.layout.try_borrow().ok().map(|l| layout::module_target(&l, id));
    if target.as_ref().is_some_and(|id| peer_key(w, cx, win, id, batch)) {
        return;
    }
    let layout = w.layout.clone();
    let Ok(mut l) = layout.try_borrow_mut() else {
        w.warn(format!("[wm] BUG: the layout was borrowed when {id}'s key ran"));
        return;
    };
    let id = layout::module_target(&l, id);
    if !l.contains(&id) {
        drop(l);
        let text = feedback::not_in_window_text(&id);
        w.say(format!("[prefix] {text}"));
        w.refuse(win, text, batch);
        return;
    }
    let shown = l.is_shown(&id);
    let keys = has_keys(w, &id);
    let unzoomed = l.unzoom();
    if unzoomed {
        w.say("[module_grid] unzoomed".into());
    }
    match (shown, keys) {
        (false, _) => {
            let showed = match l.show(&id) {
                Ok(showed) => showed,
                Err(err) => {
                    w.warn(format!("[terminal] not shown: {err}"));
                    false
                }
            };
            drop(l);
            change_then_give(w, win, unzoomed || showed, &id, vec![], batch);
        }
        (true, false) => {
            drop(l);
            change_then_give(w, win, unzoomed, &id, vec![], batch);
        }
        (true, true) => {
            let hidden = frame_with(cx.reads, win, cx.reads.grid_size(win), |frame| {
                order::hide_layout(&mut l, &id, frame)
            });
            drop(l);
            match hidden {
                Ok(next) => w.changed_with_focus(win, next.map(|next| (next, vec![])), batch),
                Err(err) => {
                    if unzoomed {
                        w.changed_by_controller(win, batch);
                    }
                    refuse_layout(w, win, &err, batch);
                }
            }
        }
    }
}

/// A module key for any kind but the terminal: the chat's key reaches the view with the keys, else the one holding
/// the oldest waiting card, else the most recent one; a numbered view's key addresses that view. A module the window
/// has no host for is refused; every other one gets the key's four cases ([`layout::key_action`]).
fn module_key(w: &mut Window, cx: &Cx, win: WinId, id: &ModuleId, batch: &mut Batch) {
    let Ok(id) = w.layout.try_borrow().map(|layout| {
        if *id == ModuleId::agent() {
            chat_target(&layout, cx.reads.oldest_card_view(win))
        } else {
            id.clone()
        }
    }) else {
        w.warn(format!("[wm] BUG: the layout was borrowed when {id}'s key ran"));
        return;
    };
    if peer_key(w, cx, win, &id, batch) {
        return;
    }
    if !has_host(w, &id) {
        let text = feedback::not_here_yet_text(&id);
        w.say(format!("[prefix] {text}"));
        w.refuse(win, text, batch);
        return;
    }
    if id == ModuleId::editor() && w.editor_run != EditorRun::Running {
        open_module(w, cx, win, &id, KeyAction::ShowAndFocus, batch);
        return;
    }
    let keys = has_keys(w, &id);
    let Ok(action) = w
        .layout
        .try_borrow()
        .map(|layout| layout::key_action(&layout, &id, keys))
    else {
        return;
    };
    open_module(w, cx, win, &id, action, batch);
}

/// A key naming the peer's module gives it the intent before any local host is required.
pub(in crate::wm) fn peer_key(w: &mut Window, cx: &Cx, win: WinId, id: &ModuleId, batch: &mut Batch) -> bool {
    let editor_elsewhere = *id == ModuleId::editor() && *w.presence() != EditorPresence::Owned;
    let (away, local) = match w.layout.try_borrow() {
        Ok(layout) => (layout.is_away(id), layout.contains(id)),
        Err(_) => return false,
    };
    let held = !local && w.peer.as_ref().is_some_and(|p| p.held.contains(id));
    if editor_elsewhere || away || held {
        if moves::raise_for(
            w,
            win,
            crate::window_link::FocusTarget::Module { id: id.to_string() },
            batch,
        ) {
            return true;
        }
        if editor_elsewhere {
            if *w.role() == (WindowRole::Linked { side: LinkSide::Second }) {
                ask_start_editor(w, cx, win, batch);
            } else {
                w.refuse(win, feedback::EDITOR_WINDOW_CLOSED_TEXT.into(), batch);
            }
            return true;
        }
        if away {
            refuse_layout(w, win, &LayoutError::Away(id.clone()), batch);
            return true;
        }
    }
    false
}

/// A tray chip: `prefix <key>` for its module from the top bar, which never holds the keys itself, so a chip never
/// hides its module. It meets neither the selection's gate nor the float's and never goes to a chat's waiting card.
fn chip(w: &mut Window, cx: &Cx, win: WinId, id: &ModuleId, batch: &mut Batch) {
    if peer_key(w, cx, win, id, batch) {
        return;
    }
    let Ok(action) = w
        .layout
        .try_borrow()
        .map(|layout| layout::key_action(&layout, id, false))
    else {
        w.warn(format!(
            "[wm] BUG: the layout was borrowed when {id}'s chip was clicked"
        ));
        return;
    };
    open_module(w, cx, win, id, action, batch);
}

/// Something other than a key here asked for `id`'s keys (the other window, a file opened from outside): brought on
/// screen as a tray chip brings it (shown when hidden, placed when this workspace lacks it, never hidden) and given
/// the keys. Its refusal is returned for the caller to log: the user pressed nothing here.
pub(in crate::wm) fn reveal_and_focus(
    w: &mut Window,
    cx: &Cx,
    win: WinId,
    id: &ModuleId,
    batch: &mut Batch,
) -> Result<(), LayoutError> {
    let action = match w.layout.try_borrow() {
        Ok(layout) => layout::key_action(&layout, id, false),
        Err(_) => return Ok(()),
    };
    w.say(format!("[modules] {id}: {action:?}, asked from outside this window"));
    match action {
        KeyAction::Place | KeyAction::ShowAndFocus if starts_hidden(cx, id) => {
            open_plugin_module(w, cx, win, id, Some(true), batch)
        }
        KeyAction::ShowAndFocus => {
            show_on_screen(w, cx, win, id, batch).and_then(|()| focus_and_arrive(w, win, id, batch))
        }
        KeyAction::Focus => focus_and_arrive(w, win, id, batch),
        KeyAction::Place => place_and_arrive(w, cx, win, id, Axis::Row, batch),
        // Not asked with the keys on it, so never; kept to the keys if it ever is.
        KeyAction::Hide => focus_and_arrive(w, win, id, batch),
    }
}

/// A module key's four cases for `id`; a refusal flashes the app name and the toast says why. A plugin module that
/// starts hidden is opened by its own rule rather than split beside the keys, and takes the keys as every module key
/// gives them.
fn open_module(w: &mut Window, cx: &Cx, win: WinId, id: &ModuleId, action: KeyAction, batch: &mut Batch) {
    w.say(format!("[modules] {id}: {action:?}"));
    let result = match action {
        KeyAction::Place | KeyAction::ShowAndFocus if starts_hidden(cx, id) => {
            open_plugin_module(w, cx, win, id, Some(true), batch)
        }
        KeyAction::ShowAndFocus => {
            show_on_screen(w, cx, win, id, batch).and_then(|()| focus_and_arrive(w, win, id, batch))
        }
        KeyAction::Focus if *id == ModuleId::editor() => {
            show_on_screen(w, cx, win, id, batch).and_then(|()| focus_and_arrive(w, win, id, batch))
        }
        KeyAction::Focus => focus_and_arrive(w, win, id, batch),
        KeyAction::Place => place_and_arrive(w, cx, win, id, Axis::Row, batch),
        KeyAction::Hide => hide(w, cx, win, id, batch),
    };
    if let Err(err) = result {
        refuse_layout(w, win, &err, batch);
    }
}

fn starts_hidden(cx: &Cx, id: &ModuleId) -> bool {
    cx.setup.start_hidden.iter().any(|(module, _)| module == id)
}

fn own_rule<'a>(cx: &'a Cx, id: &ModuleId) -> Option<&'a OpenRule> {
    cx.setup
        .start_hidden
        .iter()
        .find(|(module, _)| module == id)
        .and_then(|(_, rule)| rule.as_ref())
}

/// The rule a start-hidden plugin's module opens by: the open rules', the plugin's own counted where a Lua panel's
/// `position` counts, at a new instance's point, with `focus` replaced by `take_keys` when the way it was opened
/// decides who has the keys (`None`: the rule's own).
fn plugin_open_rule(cx: &Cx, id: &ModuleId, take_keys: Option<bool>) -> OpenRule {
    let (mut rule, _) = cx.setup.rules.resolve_own(id, OpenPoint::NewInstance, own_rule(cx, id));
    if let Some(take) = take_keys {
        rule.focus = take;
    }
    rule
}

/// Where a start-hidden plugin's module goes, hidden, in a workspace that lacks it once another holds it: its rule's
/// place as a tiled one, since hidden there it needs no float.
fn plugin_hidden_placement(cx: &Cx, id: &ModuleId) -> Placement {
    let rule = plugin_open_rule(cx, id, None);
    without_keys(rule.at.unwrap_or(At::Beside(Direction::Right))).0
}

/// A change that opened or placed `id`: laid out, and `id` given the keys when it takes them. A tile is given them
/// between its map and the unmaps; a float only after the change has mapped and restacked it, so the keys never
/// land on it while another float is over it.
fn opened_with_keys(w: &mut Window, win: WinId, id: &ModuleId, takes_keys: bool, tail: Vec<HostOp>, batch: &mut Batch) {
    let floats = w.layout.try_borrow().is_ok_and(|layout| layout.is_floating(id));
    if takes_keys && !floats {
        change_then_give(w, win, true, id, tail, batch);
    } else {
        w.changed_by_controller(win, batch);
        if takes_keys {
            give_keys(w, win, id, tail, batch);
        }
    }
}

/// Puts a start-hidden plugin's module on screen: shown where it was if the layout holds it; else opened by its
/// rule beside the module with the keys, and put hidden into every other workspace that lacks it, as a new instance
/// is. The keys move only when it takes them. A zoom that would hide it ends first. Its page is built once it is on
/// screen, so an open the layout refuses builds no page.
fn open_plugin_module(
    w: &mut Window,
    cx: &Cx,
    win: WinId,
    id: &ModuleId,
    take_keys: Option<bool>,
    batch: &mut Batch,
) -> Result<(), LayoutError> {
    let rule = plugin_open_rule(cx, id, take_keys);
    let layout = w.layout.clone();
    let Ok(mut l) = layout.try_borrow_mut() else {
        w.warn(format!("[wm] BUG: the layout was borrowed when {id} was opened"));
        return Ok(());
    };
    if l.contains(id) {
        let unzoomed = l.zoom_hides(id) && l.unzoom();
        let shown = l.show(id);
        drop(l);
        match shown {
            Ok(_) => w.changed_by_controller(win, batch),
            Err(err) => {
                if unzoomed {
                    w.changed_by_controller(win, batch);
                }
                return Err(err);
            }
        }
        if rule.focus {
            give_keys(w, win, id, vec![], batch);
        }
        return Ok(());
    }
    if !has_host(w, id) {
        return Err(LayoutError::NotInTree(id.clone()));
    }
    let keys = l.focus().clone();
    let opened = layout::open_by_rule(&mut l, id, &rule, Some(&keys), cx.reads.grid_size(win))?;
    drop(l);
    for note in &opened.notes {
        w.say(format!("[layout] {note}"));
    }
    let placement = plugin_hidden_placement(cx, id);
    let added = w
        .workspaces
        .try_borrow_mut()
        .map(|mut workspaces| workspaces.add_everywhere_at(id, placement))
        .is_ok();
    if !added {
        w.warn(format!(
            "[wm] BUG: the workspaces were borrowed when {id} was added to them"
        ));
    }
    opened_with_keys(w, win, id, opened.takes_keys, vec![], batch);
    w.say(format!(
        "[modules] {id}: opened by its rule{}",
        if opened.takes_keys {
            ""
        } else {
            ", the keys left where they were"
        }
    ));
    Ok(())
}

/// Shows `id` where it was hidden from, the keys staying where they are (`eitri.layout.show`, and the first half of
/// a module key on a hidden module). A zoom that would keep it off screen ends first: a module shown is a module on
/// screen. A start-hidden plugin's module this workspace does not hold yet is opened by its own rule, the keys left
/// where they are.
pub fn show_on_screen(
    w: &mut Window,
    cx: &Cx,
    win: WinId,
    id: &ModuleId,
    batch: &mut Batch,
) -> Result<(), LayoutError> {
    let held = w.layout.try_borrow().map(|layout| layout.contains(id));
    let Ok(held) = held else {
        w.warn(format!("[wm] BUG: the layout was borrowed when {id} was shown"));
        return Ok(());
    };
    if !held && starts_hidden(cx, id) {
        return open_plugin_module(w, cx, win, id, Some(false), batch);
    }
    if !held {
        return Err(LayoutError::NotInTree(id.clone()));
    }
    let layout = w.layout.clone();
    let Ok(mut l) = layout.try_borrow_mut() else {
        return Ok(());
    };
    let unzoomed = l.zoom_hides(id) && l.unzoom();
    if unzoomed {
        w.say("[module_grid] unzoomed".into());
    }
    let shown = l.show(id);
    drop(l);
    match shown {
        Ok(showed) => {
            if *id == ModuleId::editor() {
                restart_editor(w, cx, win, batch);
            }
            if unzoomed || showed {
                w.changed_by_controller(win, batch);
            }
            Ok(())
        }
        Err(err) => {
            if unzoomed {
                w.changed_by_controller(win, batch);
            }
            Err(err)
        }
    }
}

/// A show restarts only a released pane owned by this window. Shows while its harness is being
/// released remember the request; shows during startup keep the one restart already in flight.
pub fn restart_editor(w: &mut Window, _cx: &Cx, win: WinId, batch: &mut Batch) {
    if *w.presence() != EditorPresence::Owned {
        return;
    }
    let shown = w
        .layout
        .try_borrow_mut()
        .map(|mut layout| layout.show(&ModuleId::editor()));
    match shown {
        Ok(Ok(true)) => w.changed_by_controller(win, batch),
        Ok(Ok(false)) => {}
        Ok(Err(err)) => {
            refuse_layout(w, win, &err, batch);
            return;
        }
        Err(_) => {
            w.warn("[wm] BUG: the layout was borrowed when the editor was restarted".into());
            return;
        }
    }
    match w.editor_run {
        EditorRun::Ended => {
            let Some(mut spawn) = w.editor_spawn.clone() else {
                w.refuse(win, "the editor's launch settings are unavailable".into(), batch);
                return;
            };
            // Until the editor has been ready once, nothing has opened the launch's targets.
            spawn.open = w.pending_open.clone();
            w.restart_wanted = false;
            w.editor_ready = false;
            w.editor_run = EditorRun::Restarting;
            batch.push_effect(HostOp::Editor(EditorCall::Restart { win, spawn }));
        }
        EditorRun::Ending => w.restart_wanted = true,
        EditorRun::Restarting | EditorRun::Running => {}
    }
}

/// How many files may wait for the editor at once; one more is refused and its client opens a new window.
const MAX_QUEUED_OPENS: usize = 16;

/// Why this window cannot take a file now. The client then opens a new window, so anything that would leave the file
/// waiting on nothing belongs here: a window on its way out, an nvim asked to quit, and an editor nothing brings back
/// (`Window::editor_lost`).
fn open_refusal(w: &Window) -> Option<String> {
    if w.close.state != CloseState::Idle {
        return Some("the window is closing".into());
    }
    if w.quit.is_running() {
        return Some("nvim is quitting".into());
    }
    if *w.presence() != EditorPresence::Owned {
        return Some("the window does not own its editor".into());
    }
    if w.editor_lost {
        return Some("the editor did not start or cannot be reached".into());
    }
    (w.queued_opens.len() >= MAX_QUEUED_OPENS).then(|| format!("{MAX_QUEUED_OPENS} files wait already"))
}

/// A file another process asks this window to open: refused, or brought on screen, the window raised with `token`,
/// the keys given to the editor and the file sent to it, in the order an open by hand would go. An editor that is
/// not ready, because it has not started, is starting or is restarted here, gets the file when it is
/// ([`run_queued_opens`]). The one answer closes the batch: the file is on its way or waiting by then.
pub fn open_file(
    w: &mut Window,
    cx: &Cx,
    win: WinId,
    id: u64,
    file: QueuedOpen,
    token: Option<String>,
    batch: &mut Batch,
) {
    let refusal = open_refusal(w);
    let accepted = match refusal {
        Some(why) => {
            w.say(format!("[modules] editor: an open was refused: {why}"));
            false
        }
        None => take_open(w, cx, win, file, token, batch),
    };
    batch.push_effect(HostOp::OpenAnswer { id, accepted });
}

fn take_open(w: &mut Window, cx: &Cx, win: WinId, file: QueuedOpen, token: Option<String>, batch: &mut Batch) -> bool {
    let editor = ModuleId::editor();
    if let Err(err) = reveal_and_focus(w, cx, win, &editor, batch) {
        w.say(format!("[modules] editor: an open was refused: {err}"));
        return false;
    }
    // A show or a place restarts an ended editor; keys moving to one still on screen do not.
    restart_editor(w, cx, win, batch);
    if w.editor_run == EditorRun::Ended {
        return false;
    }
    batch.push_layout_before_keys(HostOp::Present {
        win,
        activation: token.map(ActivationToken),
    });
    if w.editor_ready && w.editor_run == EditorRun::Running {
        batch.push_effect(open_call(win, file));
    } else {
        w.queued_opens.push_back(file);
    }
    true
}

fn open_call(win: WinId, file: QueuedOpen) -> HostOp {
    HostOp::Editor(EditorCall::Open {
        win,
        path: file.path,
        line: file.line,
        col: file.col,
    })
}

/// The editor is ready: every file waiting for it, oldest first.
pub(in crate::wm) fn run_queued_opens(w: &mut Window, win: WinId, batch: &mut Batch) {
    for file in std::mem::take(&mut w.queued_opens) {
        batch.push_effect(open_call(win, file));
    }
}

/// The editor did not come up: the files waiting for it are forgotten, and the one line to say so, if any waited.
pub(in crate::wm) fn drop_queued_opens(w: &mut Window) -> Option<String> {
    let dropped = w.queued_opens.len();
    w.queued_opens.clear();
    (dropped > 0).then(|| feedback::opens_dropped_text(dropped))
}

/// Hides `id`: when it holds the keys, the module the layout chooses is mapped and given them before `id` is
/// unmapped, since a host leaving the screen with the keys would let the toolkit's own focus chain choose where they
/// go. Refused as the layout refuses it: the last module on screen, or one not in the tree.
pub fn hide(w: &mut Window, cx: &Cx, win: WinId, id: &ModuleId, batch: &mut Batch) -> Result<(), LayoutError> {
    let layout = w.layout.clone();
    let Ok(mut l) = layout.try_borrow_mut() else {
        w.warn(format!("[wm] BUG: the layout was borrowed when {id} was hidden"));
        return Ok(());
    };
    let next = frame_with(cx.reads, win, cx.reads.grid_size(win), |frame| {
        order::hide_layout(&mut l, id, frame)
    })?;
    drop(l);
    // A show before the editor's release asked for a restart; this hide takes it back.
    if *id == ModuleId::editor() {
        w.restart_wanted = false;
    }
    w.changed_with_focus(win, next.map(|next| (next, vec![])), batch);
    Ok(())
}

/// The hide key's module key: the module `named` names ([`layout::hide_target`]) hidden wherever the keys are, the keys
/// moving only when it held them, by [`hide`]'s rule. It acts on one module, so a selected container refuses it as it
/// refuses `module.hide`; so are a module held away in the other window, one this layout does not hold, one hidden
/// already and the last one on screen, each with the flash and its reason. It never reaches the other window: a hide
/// is about this window's screen.
pub fn hide_by_key(w: &mut Window, cx: &Cx, win: WinId, named: &ModuleId, batch: &mut Batch) {
    let Ok((id, selection, away, held, shown)) = w.layout.try_borrow().map(|layout| {
        let id = layout::hide_target(&layout, named);
        let (away, held, shown) = (layout.is_away(&id), layout.contains(&id), layout.is_shown(&id));
        (id, layout.selection(), away, held, shown)
    }) else {
        w.warn(format!(
            "[wm] BUG: the layout was borrowed when the hide key's {named} ran"
        ));
        return;
    };
    w.say(format!("[modules] {id}: hidden by its key"));
    if super::selection_gate(&Action::ModuleHide, selection) == super::SelectionGate::Refuse {
        refusal(w, win, None, super::one_module_text(cx.keymap), batch);
        return;
    }
    let refused = if away {
        Some(LayoutError::Away(id.clone()))
    } else if !held {
        Some(LayoutError::NotInTree(id.clone()))
    } else {
        None
    };
    if let Some(err) = refused {
        refuse_layout(w, win, &err, batch);
        return;
    }
    if !shown {
        refusal(w, win, None, feedback::already_hidden_text(&id), batch);
        return;
    }
    if let Err(err) = hide(w, cx, win, &id, batch) {
        refuse_layout(w, win, &err, batch);
    }
}

/// `id` into a new split after the module with the keys, along `axis`, moved there if it is elsewhere, and given the
/// keys, arriving only if its grab takes (a split key and a module key, a module key for a module never placed,
/// `eitri.layout.split`). A start-hidden plugin's module placed this way is then held, hidden, by every other
/// workspace that lacks it, as one opened by its rule is.
pub fn place_and_arrive(
    w: &mut Window,
    cx: &Cx,
    win: WinId,
    id: &ModuleId,
    axis: Axis,
    batch: &mut Batch,
) -> Result<(), LayoutError> {
    placeable_by_id(id, !idle(w, id))?;
    if !has_host(w, id) {
        return Err(LayoutError::NotInTree(id.clone()));
    }
    let layout = w.layout.clone();
    let Ok(mut l) = layout.try_borrow_mut() else {
        w.warn(format!("[wm] BUG: the layout was borrowed when {id} was placed"));
        return Ok(());
    };
    let target = l.focus().clone();
    layout::place(&mut l, id, &target, axis)?;
    drop(l);
    // A split puts the editor on screen as a show does, so an ended editor starts again here too: placed with no
    // nvim, its pane would take the keys and run nothing.
    if *id == ModuleId::editor() {
        restart_editor(w, cx, win, batch);
    }
    if starts_hidden(cx, id) {
        let placement = plugin_hidden_placement(cx, id);
        if let Ok(mut workspaces) = w.workspaces.try_borrow_mut() {
            workspaces.add_everywhere_at(id, placement);
        }
    }
    change_then_give(w, win, true, id, arrive_tail(win, id, true), batch);
    Ok(())
}

/// A split key's module key, with the keys on `target` (the layout's focus): a module that exists is placed beside
/// it ([`place_and_arrive`]); the terminal's key from a terminal makes a new terminal there, its shell starting in
/// the pressed terminal's directory when the key says so; the chat's key from a chat view makes a new chat view
/// holding a new empty tab; any other kind has no new instance. The split ends a container selection.
pub fn place(w: &mut Window, cx: &Cx, win: WinId, module: &ModuleId, axis: Axis, cwd: SplitCwd, batch: &mut Batch) {
    let Ok((choice, target)) = w
        .layout
        .try_borrow()
        .map(|layout| (layout::split_choice(&layout, module), layout.focus().clone()))
    else {
        w.warn(format!(
            "[wm] BUG: the layout was borrowed when {module} was placed by a split key"
        ));
        return;
    };
    match choice {
        SplitChoice::Existing(id) => {
            if let Err(err) = place_and_arrive(w, cx, win, &id, axis, batch) {
                refuse_layout(w, win, &err, batch);
            }
        }
        SplitChoice::NewInstance(ModuleKind::Terminal)
            if cwd == SplitCwd::Pane && target.kind() == ModuleKind::Terminal =>
        {
            // The directory is read off the toolkit's thread, since a slow filesystem must never stall the
            // window; the terminal is made when it comes back, beside the target captured here.
            let read = w.modules.next_cwd_read;
            w.modules.next_cwd_read += 1;
            batch.push_effect(HostOp::Terminal(TermOp::ReadCwd {
                win,
                module: target.clone(),
                read,
            }));
            w.modules.cwd_reads.push(PendingCwd { read, target, axis });
        }
        SplitChoice::NewInstance(kind @ (ModuleKind::Terminal | ModuleKind::Agent)) => {
            instance_new(w, cx, win, kind, OpenHow::Beside(target, axis), None, None, batch);
        }
        SplitChoice::NewInstance(kind) => {
            w.say(format!("[modules] split: {}", feedback::no_new_instance_text(kind)));
            w.refuse(win, String::new(), batch);
        }
    }
    if w.clear_selection() {
        w.layout_touched();
    }
}

/// The directory a split's [`TermOp::ReadCwd`] read: the new terminal is made beside the terminal it was read for,
/// its shell starting there, or the split is refused with the host's words when that terminal ended meanwhile. The
/// answer is matched to its split by `read`, the request's own number, never by the module alone.
pub fn cwd_read(
    w: &mut Window,
    cx: &Cx,
    win: WinId,
    module: &ModuleId,
    read: u64,
    dir: Result<PathBuf, String>,
    batch: &mut Batch,
) {
    let Some(at) = w
        .modules
        .cwd_reads
        .iter()
        .position(|pending| pending.read == read && pending.target == *module)
    else {
        w.say(format!(
            "[terminal] {module}: a directory came back that no split waits for"
        ));
        return;
    };
    let pending = w.modules.cwd_reads.remove(at);
    match dir {
        Ok(dir) => instance_new(
            w,
            cx,
            win,
            ModuleKind::Terminal,
            OpenHow::Beside(pending.target, pending.axis),
            Some(dir),
            None,
            batch,
        ),
        Err(text) => refusal(w, win, None, text, batch),
    }
}

/// A new terminal or chat view, its number the lowest no workspace holds, no later build's kept data reserves and
/// no create in flight takes: its surface asked for, to be placed once it exists ([`created`]); placed at once when
/// the window still holds that number's host idle. A terminal's shell starts in `cwd`, else the project root.
/// Refused with today's words for a kind with no instances or a tenth instance; `quiet` (a Lua request's words)
/// logs a refusal instead of showing it.
#[allow(clippy::too_many_arguments)]
pub fn instance_new(
    w: &mut Window,
    cx: &Cx,
    win: WinId,
    kind: ModuleKind,
    how: OpenHow,
    cwd: Option<PathBuf>,
    quiet: Option<String>,
    batch: &mut Batch,
) {
    let planned = match (w.layout.try_borrow(), w.workspaces.try_borrow()) {
        (Ok(layout), Ok(workspaces)) => {
            let here: Vec<&Layout> = workspaces.each(&layout).into_iter().map(|(_, layout)| layout).collect();
            let reserved: Vec<ModuleId> = workspaces
                .reserved()
                .into_iter()
                .chain(w.modules.pending_ids())
                .chain(moves::arriving(w))
                .collect();
            Some(match kind {
                ModuleKind::Terminal | ModuleKind::Agent => plan_new_instance(
                    kind,
                    &here,
                    &reserved,
                    w.peer.as_ref().map(|p| p.held.as_slice()).unwrap_or_default(),
                ),
                other => Err(feedback::no_new_instance_text(other)),
            })
        }
        _ => None,
    };
    let Some(planned) = planned else {
        w.warn(format!(
            "[wm] BUG: the layout was borrowed when a new {kind:?} was planned"
        ));
        return;
    };
    let id = match planned {
        Ok(id) => id,
        Err(text) => {
            refusal(w, win, quiet.as_deref(), text, batch);
            return;
        }
    };
    open_instance(w, cx, win, id, how, cwd, quiet, batch);
}

/// The new instance `id`'s surface asked for, or, when the window still holds that number's host idle, `id` placed
/// at once with no create.
#[allow(clippy::too_many_arguments)]
fn open_instance(
    w: &mut Window,
    cx: &Cx,
    win: WinId,
    id: ModuleId,
    how: OpenHow,
    cwd: Option<PathBuf>,
    quiet: Option<String>,
    batch: &mut Batch,
) {
    let kind = id.kind();
    let surface = match kind {
        ModuleKind::Agent => SurfaceKind::Chat {
            view: ViewId::of_module(&id).expect("a chat view's id"),
        },
        _ => SurfaceKind::Terminal {
            cwd: cwd.unwrap_or_else(|| cx.setup.project_dir.clone()),
        },
    };
    let open = PendingOpen {
        id: id.clone(),
        kind: surface.clone(),
        how,
        quiet,
    };
    // A terminal's idle pane comes back with no create: its new directory is all it needs. A chat view's idle page
    // is asked for again, so the host takes it back with a fresh document before its first tab opens.
    if idle(w, &id) && kind == ModuleKind::Terminal {
        // The idle pane keeps the directory its last shell was given; this instance's comes with no create.
        if let SurfaceKind::Terminal { cwd } = &surface {
            batch.push_layout(HostOp::Terminal(TermOp::SetStartDir {
                win,
                module: id.clone(),
                dir: cwd.clone(),
            }));
        }
        place_new(w, cx, win, open, batch);
        return;
    }
    batch.push_layout(HostOp::CreateSurface {
        win,
        module: id,
        kind: surface,
    });
    w.modules.opens.push(open);
}

/// `scratchpad.show` for the terminal or chat view `id` that does not exist yet: made as a new instance is, under
/// that number, and put straight into the scratchpad, shown and given the keys. Refused, with nothing made, for a
/// number another workspace holds or a later build's kept data names (two modules must never share a name), for a
/// number already being made, and for any other kind. Its shell starts in the project root.
pub fn scratch_new(w: &mut Window, cx: &Cx, win: WinId, id: &ModuleId, batch: &mut Batch) {
    if !matches!(id.kind(), ModuleKind::Terminal | ModuleKind::Agent) || id.number() < 2 {
        refusal(w, win, None, feedback::not_in_window_text(id), batch);
        return;
    }
    let pending = w.modules.pending_ids().contains(id);
    let taken = match (w.layout.try_borrow(), w.workspaces.try_borrow()) {
        (Ok(layout), Ok(workspaces)) => Some(if workspaces.reserved().contains(id) {
            Some(format!("{id}: a later build's saved layout holds that number"))
        } else if workspaces.each(&layout).iter().any(|(_, layout)| layout.contains(id)) {
            Some(format!("{id}: another workspace holds it"))
        } else if pending {
            Some(format!("{id}: it is being made"))
        } else if moves::arriving(w).contains(id) {
            Some(format!("{id}: it is arriving from the other window"))
        } else if w.peer.as_ref().is_some_and(|p| p.held.contains(id)) {
            Some(format!("{id}: the other window holds it"))
        } else {
            let here: Vec<&Layout> = workspaces.each(&layout).into_iter().map(|(_, layout)| layout).collect();
            // A named scratchpad pins its candidate: the allocator must not pick a different free number.
            let reserved: Vec<ModuleId> = (2..=9)
                .filter(|n| *n != id.number())
                .map(|n| ModuleId::instance(id.kind(), n).expect("a numbered terminal or chat view"))
                .collect();
            plan_new_instance(
                id.kind(),
                &here,
                &reserved,
                w.peer.as_ref().map(|p| p.held.as_slice()).unwrap_or_default(),
            )
            .err()
        }),
        _ => None,
    };
    let Some(taken) = taken else {
        w.warn(format!(
            "[wm] BUG: the layout was borrowed when {id} was made for the scratchpad"
        ));
        return;
    };
    if let Some(text) = taken {
        refusal(w, win, None, text, batch);
        return;
    }
    open_instance(w, cx, win, id.clone(), OpenHow::Scratch, None, None, batch);
}

/// `tab.break` from the base view: a new chat view where the open rules say for a broken-out tab (by default right of
/// the base view), holding the base view's active tab.
pub fn tab_break(w: &mut Window, cx: &Cx, win: WinId, batch: &mut Batch) {
    let how = OpenHow::Rule {
        point: OpenPoint::Break,
        explicit: None,
    };
    instance_new(w, cx, win, ModuleKind::Agent, how, None, None, batch);
}

/// The surface of a new instance exists: it is placed. Whether `module` was a new instance waiting for its surface.
pub fn created(w: &mut Window, cx: &Cx, win: WinId, module: &ModuleId, batch: &mut Batch) -> bool {
    let Some(at) = w.modules.opens.iter().position(|open| open.id == *module) else {
        return false;
    };
    let open = w.modules.opens.remove(at);
    place_new(w, cx, win, open, batch);
    true
}

/// The host could not make a new instance's surface: nothing was added anywhere, and the host's reason is the
/// refusal's text.
pub fn create_refused(w: &mut Window, win: WinId, module: &ModuleId, why: &str, batch: &mut Batch) {
    let Some(at) = w.modules.opens.iter().position(|open| open.id == *module) else {
        return;
    };
    let open = w.modules.opens.remove(at);
    refusal(w, win, open.quiet.as_deref(), why.to_string(), batch);
}

/// The rule a new instance opens by: `explicit` (an `eitri.layout.open` with options) wins, else the open rules'.
pub fn open_rule(rules: &OpenRules, id: &ModuleId, point: OpenPoint, explicit: Option<&OpenRule>) -> OpenRule {
    match explicit {
        Some(rule) => rule.clone(),
        None => rules.resolve(id, point, None).0,
    }
}

/// Whether a new chat view opened `how` takes the base view's active tab (`tab.break`) rather than a new empty one.
fn breaks_out(how: &OpenHow) -> bool {
    matches!(
        how,
        OpenHow::Rule {
            point: OpenPoint::Break,
            ..
        }
    )
}

/// A new instance whose surface exists placed as `open.how` says. Placed, it is declared afresh (a terminal's shell
/// starts once it is shown), added hidden to every other workspace at its declared place, and given the keys unless
/// its open rule keeps them where they are; a chat view gets its new empty tab first, and arrives only if its grab
/// takes. A placement the layout refuses, or a chat view the panel cannot give its first tab, adds it nowhere and
/// leaves its surface idle.
fn place_new(w: &mut Window, cx: &Cx, win: WinId, open: PendingOpen, batch: &mut Batch) {
    let PendingOpen { id, kind, how, quiet } = open;
    let noun = if id.kind() == ModuleKind::Agent {
        "chat view"
    } else {
        "terminal"
    };
    let layout = w.layout.clone();
    let Ok(mut l) = layout.try_borrow_mut() else {
        w.warn(format!("[wm] BUG: the layout was borrowed when {id} was placed"));
        return;
    };
    let keys = l.focus().clone();
    // A chat view's first tab is asked for after its placement in the same batch, and nothing takes a placement back
    // once a refused tab leaves the view empty: so the panel is asked first, and a refusal places nothing.
    let first_tab = match &kind {
        SurfaceKind::Chat { view } => cx.reads.chat_first_tab(*view, breaks_out(&how)),
        _ => Ok(()),
    };
    let placed: Result<Opened, String> = first_tab.and_then(|()| match &how {
        OpenHow::Beside(beside, axis) => {
            if l.contains(beside) {
                layout::place(&mut l, &id, beside, *axis)
                    .map(|()| Opened {
                        takes_keys: true,
                        stowed: false,
                        notes: Vec::new(),
                    })
                    .map_err(|err| err.to_string())
            } else {
                Err(feedback::left_the_layout_text(beside, noun))
            }
        }
        OpenHow::Rule { point, explicit } => {
            let rule = open_rule(&cx.setup.rules, &id, *point, explicit.as_ref());
            layout::open_by_rule(&mut l, &id, &rule, Some(&keys), cx.reads.grid_size(win))
                .map_err(|err| err.to_string())
        }
        // A plain float first: the scratchpad's show below makes it a scratch float and shows it, where a float
        // already in the scratchpad and on screen would be stowed instead.
        OpenHow::Scratch => {
            let size = cx.setup.rules.resolve(&id, OpenPoint::NewInstance, None).0.size;
            let rect = layout::rules::float_rect(size, cx.reads.grid_size(win));
            l.add_float(&id, rect, false)
                .map(|()| Opened {
                    takes_keys: false,
                    stowed: false,
                    notes: Vec::new(),
                })
                .map_err(|err| err.to_string())
        }
    });
    drop(l);
    let opened = match placed {
        Ok(opened) => opened,
        Err(text) => {
            batch.push_effect(HostOp::DropSurface {
                win,
                module: id.clone(),
                why: DropWhy::Retired,
            });
            w.lifecycle.declare(
                &SurfaceDecl {
                    module: id.clone(),
                    kind,
                    deferred: false,
                },
                true,
            );
            let ended = HostEvent::SurfaceEnded {
                win,
                module: id,
                why: Ended::Retired,
            };
            w.lifecycle.record_event(win, &ended);
            refusal(w, win, quiet.as_deref(), text, batch);
            return;
        }
    };
    if let SurfaceKind::Chat { view } = &kind {
        let view = *view;
        batch.push_layout(HostOp::Chat(if breaks_out(&how) {
            ChatOp::BreakOut { win, view }
        } else {
            ChatOp::OpenTab { win, view }
        }));
    }
    w.lifecycle.declare(
        &SurfaceDecl {
            module: id.clone(),
            kind,
            deferred: false,
        },
        true,
    );
    let placement = declared_placement(&cx.setup.decls, &id, Placement::BelowEditor);
    let added = w
        .workspaces
        .try_borrow_mut()
        .map(|mut workspaces| workspaces.add_everywhere_at(&id, placement))
        .is_ok();
    if !added {
        w.warn(format!(
            "[wm] BUG: the workspaces were borrowed when {id} was added to them"
        ));
    }
    for note in &opened.notes {
        w.say(format!("[layout] {note}"));
    }
    if how == OpenHow::Scratch {
        scratch_shown(w, cx, win, &id, batch);
        focus::modules_changed(w, win, batch);
        let tag = if id.kind() == ModuleKind::Agent {
            "chat"
        } else {
            "terminal"
        };
        w.say(format!("[{tag}] {id}: opened as a float for the scratchpad"));
        return;
    }
    opened_with_keys(w, win, &id, opened.takes_keys, arrive_tail(win, &id, true), batch);
    focus::modules_changed(w, win, batch);
    let place = match &how {
        OpenHow::Beside(beside, axis) => format!("beside {beside} ({axis:?})"),
        OpenHow::Rule { point, explicit } => format!(
            "where {} says ({point:?}, the keys in {keys})",
            if explicit.is_some() {
                "the call"
            } else {
                "its open rule"
            }
        ),
        OpenHow::Scratch => "into the scratchpad".to_string(),
    };
    let place = if opened.stowed {
        format!("{place}, stowed in the scratchpad")
    } else if opened.takes_keys {
        place
    } else {
        format!("{place}, the keys left where they were")
    };
    let tag = if id.kind() == ModuleKind::Agent {
        "chat"
    } else {
        "terminal"
    };
    w.say(format!("[{tag}] {id}: opened {place}"));
}

/// The new instance `id`, a plain float now, shown by the scratchpad as `scratchpad.show` names it: a scratch float,
/// raised, mapped and restacked before it is given the keys.
fn scratch_shown(w: &mut Window, cx: &Cx, win: WinId, id: &ModuleId, batch: &mut Batch) {
    let layout = w.layout.clone();
    let shown = match layout.try_borrow_mut() {
        Ok(mut l) => frame_with(cx.reads, win, cx.reads.grid_size(win), |frame| {
            layout::scratchpad_show(&mut l, Some(id), frame)
        }),
        Err(_) => {
            w.warn(format!(
                "[wm] BUG: the layout was borrowed when {id} was shown from the scratchpad"
            ));
            return;
        }
    };
    w.changed_by_controller(win, batch);
    match shown {
        Ok(ScratchShow::Shown(to) | ScratchShow::Raised(to)) => {
            give_keys(w, win, &to, vec![], batch);
        }
        Ok(other) => {
            w.say(format!("[modules] {id}: made, and the scratchpad answered {other:?}"));
            w.refuse(win, String::new(), batch);
        }
        Err(err) => refuse_layout(w, win, &err, batch),
    }
}

/// A kill already confirmed, with no y/n: a terminal whose shell ended by itself, which closes as `prefix x` closes
/// it. The last module on screen enters the window close flow.
pub fn kill_confirmed(w: &mut Window, cx: &Cx, win: WinId, id: &ModuleId, batch: &mut Batch) -> Ran {
    kill_now(w, cx, win, id, batch)
}

/// `prefix x` asks one question about the module's consequences. For the last module it also confirms the
/// window consequences; the editor's answer still goes through nvim's unsaved-buffer prompt.
fn kill_asked(w: &mut Window, cx: &Cx, win: WinId, batch: &mut Batch) -> Ran {
    let Some(id) = target(w) else {
        return Ran::Done;
    };
    if let Some((peer, other)) = moves::silent_peer(w, &id) {
        // The module cannot be killed while the other window may hold its session; ending that window can, and the
        // link's end gives the module back here. Only window 1 started the other window and can end it: window 2
        // says how the module comes back instead of asking a question whose `y` could do nothing.
        if other == 1 {
            let text = feedback::not_answering_text(other);
            w.say(format!("[modules] {id}: not killed: {text}"));
            w.refuse(win, text, batch);
            return Ran::Done;
        }
        let (running, queued) = moves::peer_counts(w);
        let text = feedback::close_peer_text(other, running, queued);
        w.say(format!(
            "[modules] {id}: frozen for window {other}, which is not answering: {text}"
        ));
        w.modules.clear_question();
        w.modules.end_peer = Some(peer);
        chrome::set_prompt(
            w,
            Some(Prompt {
                text,
                closes_window: false,
                offers_move: false,
            }),
        );
        close::superseded(w, cx, win, batch);
        return Ran::Done;
    }
    if moves::moving(w, &id) {
        w.say(format!("[modules] {id}: not killed: {}", feedback::MOVING_TEXT));
        w.refuse(win, feedback::MOVING_TEXT.to_owned(), batch);
        return Ran::Done;
    }
    let scope = w.layout.try_borrow().map(|layout| layout::can_kill(&layout, &id));
    let scope = match scope {
        Ok(Ok(scope)) => scope,
        Ok(Err(err)) => {
            refuse_layout(w, win, &err, batch);
            return Ran::Done;
        }
        Err(_) => return Ran::Done,
    };
    if scope == KillScope::Window {
        close::ask_last_module(w, cx, win, id, batch);
        return Ran::Done;
    }
    let title = chrome::title(w, &id);
    let text = match ViewId::of_module(&id).filter(|view| !view.is_base()) {
        Some(view) => {
            let counts = cx.reads.chat_kill_counts(view);
            close::prompt_closing_tab(
                &id,
                &title,
                scope,
                counts.tab.as_deref(),
                counts.running,
                counts.queued,
                0,
                counts.background,
            )
        }
        None => {
            let counts = match id.kind() {
                ModuleKind::Agent => cx.reads.chat_kill_counts(ViewId::BASE),
                _ => ChatKillCounts::default(),
            };
            close::prompt(&id, &title, scope, counts.running, counts.queued, 0, counts.background)
        }
    };
    w.say(format!("[modules] {id}: {text}"));
    w.modules.clear_question();
    w.modules.kill = Some(id);
    chrome::set_prompt(
        w,
        Some(Prompt {
            text,
            closes_window: false,
            offers_move: false,
        }),
    );
    close::superseded(w, cx, win, batch);
    Ran::Done
}

/// The editor key in a window 2 with no window 1 to raise: the editor runs in window 1, so the question is
/// whether to start it. The window has no editor to show in the meantime. Like a kill's question, it takes the
/// window close's place on the overlay.
fn ask_start_editor(w: &mut Window, cx: &Cx, win: WinId, batch: &mut Batch) {
    w.say(format!("[modules] editor: {}", feedback::START_EDITOR_PROMPT));
    w.modules.clear_question();
    w.modules.start_editor = true;
    chrome::set_prompt(
        w,
        Some(Prompt {
            text: feedback::START_EDITOR_PROMPT.to_owned(),
            closes_window: false,
            offers_move: false,
        }),
    );
    close::superseded(w, cx, win, batch);
}

/// The other window went: a shown question about ending it names a window that is not there any more (and a new
/// window 2 could take its number), so it goes, and a `y` meant for it does nothing.
pub(in crate::wm) fn peer_gone(w: &mut Window) {
    if w.modules.end_peer.take().is_some() && w.chrome.prompt.as_ref().is_some_and(|p| !p.closes_window) {
        w.say("[modules] the other window went; its question is gone".into());
        chrome::set_prompt(w, None);
    }
}

/// The y/n's answer: it closes either way, and `y` kills the module it asked about, or starts the editor window.
pub fn prompt_answer(w: &mut Window, cx: &Cx, win: WinId, answer: PromptKey, batch: &mut Batch) -> Ran {
    if answer == PromptKey::Ignore {
        return Ran::Done;
    }
    if w.chrome.prompt.as_ref().is_some_and(|prompt| prompt.closes_window) {
        close::answer(w, cx, win, answer, batch);
        return Ran::Done;
    }
    chrome::set_prompt(w, None);
    if let Some(peer) = w.modules.end_peer.take() {
        if answer == PromptKey::Yes {
            w.say("[modules] ending the other window, which is not answering".into());
            batch.push_effect(HostOp::EndPeer { win, peer });
        }
        return Ran::Done;
    }
    if std::mem::take(&mut w.modules.start_editor) {
        if answer != PromptKey::Yes {
            return Ran::Done;
        }
        // The editor window may have linked while the question was shown: its editor is then raised instead.
        let editor = crate::window_link::FocusTarget::Module {
            id: ModuleId::editor().to_string(),
        };
        if !moves::raise_for(w, win, editor, batch) {
            w.say("[modules] editor: the editor window is started".into());
            batch.push_effect(HostOp::StartEditorWindow { win });
        }
        return Ran::Done;
    }
    let Some(id) = w.modules.kill.take() else {
        return Ran::Done;
    };
    match answer {
        PromptKey::Yes => kill_now(w, cx, win, &id, batch),
        PromptKey::No | PromptKey::Move | PromptKey::Ignore => Ran::Done,
    }
}

/// `id` killed, as its kind ends: off screen first in the hide's order (the keys to the module the layout chooses,
/// then the unmap), left where its kind goes back to, then what ran in it ended, only once the layout took the
/// kill, so a kill the layout refuses ends nothing. A terminal's shell is hung up, and a
/// numbered one leaves every workspace, its pane idle for the next terminal of that number. A Lua page's or a
/// plugin's runs stop and its page ends, to load afresh when it is next shown. A chat view's tabs close (the base
/// view's own tabs only; a tab shown in another view is that view's), and a numbered view leaves every workspace,
/// its page idle for the next view of that number. Refused if the layout would refuse it now, and a chat's kill while
/// one of its tabs cannot close (handed off to a terminal, or moving to the other window), and any module while this
/// window is moving it to the other window; the last module on screen is the window close.
fn kill_now(w: &mut Window, cx: &Cx, win: WinId, id: &ModuleId, batch: &mut Batch) -> Ran {
    if moves::moving(w, id) {
        // The other window may already be taking the session over: what runs in it is not this window's to end
        // until the move does.
        w.say(format!("[modules] {id}: not killed: {}", feedback::MOVING_TEXT));
        w.refuse(win, feedback::MOVING_TEXT.to_owned(), batch);
        return Ran::Done;
    }
    let scope = w.layout.try_borrow().map(|layout| layout::can_kill(&layout, id));
    match scope {
        Ok(Ok(KillScope::Module)) => {}
        Ok(Ok(KillScope::Window)) => {
            close::request(w, cx, win, close::CloseCause::LastTerminalExit(id.clone()), batch);
            return Ran::Done;
        }
        Ok(Err(err)) => {
            w.say(format!("[modules] {id}: not killed: {err}"));
            w.refuse(win, String::new(), batch);
            return Ran::Done;
        }
        Err(_) => {
            w.warn(format!("[wm] BUG: the layout was borrowed when {id} was killed"));
            return Ran::Done;
        }
    }
    if let Some(view) = ViewId::of_module(id) {
        if let Some(text) = cx.reads.chat_kill_counts(view).blocked {
            // Checked before anything leaves the screen: the view and every tab stay as they are.
            w.say(format!("[modules] {id}: not killed: {text}"));
            w.refuse(win, String::new(), batch);
            return Ran::Done;
        }
    }
    let reopen = close::reopen(id, &cx.setup.decls);
    let killed = match id.kind() {
        ModuleKind::Terminal => leave_killed(w, cx, win, id, reopen, batch).map(|()| {
            batch.push_effect(HostOp::Terminal(TermOp::HangUp {
                win,
                module: id.clone(),
            }));
            if id.number() >= 2 {
                if let Ok(mut workspaces) = w.workspaces.try_borrow_mut() {
                    workspaces.remove_everywhere(id);
                }
                batch.push_effect(HostOp::DropSurface {
                    win,
                    module: id.clone(),
                    why: DropWhy::Killed,
                });
            }
        }),
        ModuleKind::LuaWebview => leave_killed(w, cx, win, id, reopen, batch),
        ModuleKind::Agent if id.number() >= 2 => {
            let view = ViewId::of_module(id).expect("a chat view's id");
            let in_layout = w.layout.try_borrow().is_ok_and(|layout| layout.contains(id));
            let left = if in_layout {
                leave_killed(w, cx, win, id, Reopen::Remove, batch)
            } else {
                Ok(())
            };
            left.map(|()| {
                batch.push_effect(HostOp::Chat(ChatOp::CloseTabs { win, view }));
                if let Ok(mut workspaces) = w.workspaces.try_borrow_mut() {
                    workspaces.remove_everywhere(id);
                }
                batch.push_effect(HostOp::DropSurface {
                    win,
                    module: id.clone(),
                    why: DropWhy::Killed,
                });
                let views = remaining_views(w);
                batch.push_effect(HostOp::Chat(ChatOp::LayoutViews { win, views }));
                w.say(format!(
                    "[modules] {id}: killed, its tab closed; the view left the layout"
                ));
            })
        }
        ModuleKind::Agent => leave_killed(w, cx, win, id, reopen, batch).map(|()| {
            batch.push_effect(HostOp::Chat(ChatOp::CloseTabs {
                win,
                view: ViewId::BASE,
            }));
            w.say(format!("[modules] {id}: killed, its tabs closed"));
        }),
        ModuleKind::Editor => {
            close::kill_editor(w, cx, win, KillScope::Module, batch);
            return Ran::Done;
        }
        ModuleKind::Canvas => Err(feedback::not_here_yet_text(id)),
    };
    if let Err(text) = killed {
        w.say(format!("[modules] {id}: not killed: {text}"));
        w.refuse(win, String::new(), batch);
    }
    Ran::Done
}

/// The layout half of a kill, and the lifecycle's record of it: `id` leaves the screen in the hide's order and is
/// left where `reopen` says, and its kind's effects of the kill follow (a page's runs stopped and its page ended).
fn leave_killed(
    w: &mut Window,
    cx: &Cx,
    win: WinId,
    id: &ModuleId,
    reopen: Reopen,
    batch: &mut Batch,
) -> Result<(), String> {
    let layout = w.layout.clone();
    let next = {
        let Ok(mut l) = layout.try_borrow_mut() else {
            return Err("the layout is changing".into());
        };
        frame_with(cx.reads, win, cx.reads.grid_size(win), |frame| {
            layout::kill(&mut l, id, reopen, frame)
        })
        .map_err(|err| err.to_string())?
    };
    let ended = HostEvent::SurfaceEnded {
        win,
        module: id.clone(),
        why: Ended::Killed { reopen },
    };
    match layout.try_borrow() {
        Ok(l) => w.lifecycle.on_event(win, &ended, &l, batch),
        Err(_) => w.lifecycle.record_event(win, &ended),
    }
    w.changed_with_focus(win, next.map(|next| (next, vec![])), batch);
    focus::modules_changed(w, win, batch);
    Ok(())
}

/// Every extra chat view the window's workspaces still hold, ascending and once each.
pub(super) fn remaining_views(w: &Window) -> Vec<ViewId> {
    let (layout, workspaces) = match (w.layout.try_borrow(), w.workspaces.try_borrow()) {
        (Ok(layout), Ok(workspaces)) => (layout, workspaces),
        _ => return Vec::new(),
    };
    let mut ids: Vec<ModuleId> = workspaces
        .each(&layout)
        .into_iter()
        .flat_map(|(_, layout)| layout.modules())
        .filter(|id| id.kind() == ModuleKind::Agent && id.number() >= 2)
        .collect();
    ids.sort();
    ids.dedup();
    ids.iter().filter_map(ViewId::of_module).collect()
}

/// A plugin's follow command for its module: shown puts it on screen (opening a start-hidden one by its rule the
/// first time) with the keys where they are, and draws its tab unless that would hide the module with the keys;
/// hidden sends the keys where any hide sends them. A refusal is logged; the editor asked, not the user.
pub fn follow(w: &mut Window, cx: &Cx, win: WinId, module: &ModuleId, show: bool, batch: &mut Batch) {
    if !show {
        if let Err(err) = hide(w, cx, win, module, batch) {
            w.say(format!(
                "[modules] {module}: the editor asked to hide it, refused: {err}"
            ));
        }
        return;
    }
    if let Err(err) = show_on_screen(w, cx, win, module, batch) {
        w.say(format!(
            "[modules] {module}: the editor asked to show it, refused: {err}"
        ));
        return;
    }
    let brought = w
        .layout
        .try_borrow_mut()
        .is_ok_and(|mut layout| layout.is_tabbed_away(module) && layout.bring_forward(module));
    if brought {
        w.changed_by_controller(win, batch);
    } else if w.layout.try_borrow().is_ok_and(|layout| layout.is_tabbed_away(module)) {
        w.say(format!(
            "[modules] {module}: shown, but its tab stays behind the one with the keys"
        ));
    }
}

/// `eitri.layout.show(id)`: a start-hidden plugin's module this workspace does not hold yet opens by its rule, its
/// rule's `focus` deciding the keys; anything else is shown with the keys where they are.
pub fn lua_show(w: &mut Window, cx: &Cx, win: WinId, id: &ModuleId, batch: &mut Batch) -> Result<(), LayoutError> {
    let held = w.layout.try_borrow().is_ok_and(|layout| layout.contains(id));
    if starts_hidden(cx, id) && !held {
        return open_plugin_module(w, cx, win, id, None, batch);
    }
    show_on_screen(w, cx, win, id, batch)
}

/// `eitri.layout.open(kind, opts)`: a new terminal, or a new chat view holding a new empty tab, as `instance.new`
/// makes one, where the call's options say when it gave them, else where the open rules do; its refusals are logged
/// under `request`, never toasted.
pub fn lua_open(
    w: &mut Window,
    cx: &Cx,
    win: WinId,
    kind: ModuleKind,
    explicit: Option<OpenRule>,
    request: String,
    batch: &mut Batch,
) {
    let how = OpenHow::Rule {
        point: OpenPoint::NewInstance,
        explicit,
    };
    instance_new(w, cx, win, kind, how, None, Some(request), batch);
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod restart_tests {
    use super::*;
    use crate::keymap::{prefix::Press, KeySpec};
    use crate::lua::layout::LayoutRequest;
    use crate::wm::fake::{FakeHost, FakeScheduler, Logged};
    use crate::wm::start::StartStep;
    use crate::wm::verbs::rig::{rig, WIN};
    use crate::wm::{Driver, EditorPresence};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    struct Rig {
        driver: Rc<Driver>,
        host: Rc<FakeHost>,
        scheduler: Rc<FakeScheduler>,
        layout: Rc<RefCell<Layout>>,
        spawn: EditorSpawn,
    }

    impl Rig {
        fn new() -> Self {
            Self::with(None, false)
        }
        /// With `setup` as the window's module setup, and with the default keymap when `keys` is set.
        fn with(setup: Option<ModuleSetup>, keys: bool) -> Self {
            Self::with_open(setup, keys, vec![])
        }
        /// As [`Rig::with`], the launch naming `open` for nvim to open.
        fn with_open(setup: Option<ModuleSetup>, keys: bool, open: Vec<String>) -> Self {
            let mut r = rig(Layout::initial(&[]).unwrap());
            if let Some(setup) = setup {
                r.controller.set_module_setup(WIN, setup);
            }
            if keys {
                let keymap = Keymap::defaults();
                let module_keys = crate::layout::ModuleKeys::build(&[], &keymap).unwrap();
                r.controller.set_keymap(WIN, Rc::new(keymap), Rc::new(module_keys));
            }
            let dir = std::path::Path::new("/private");
            let pane = agent::socket_path::in_dir(dir, "pane.sock").unwrap();
            let context = agent::socket_path::in_dir(dir, "context.sock").unwrap();
            let spawn = EditorSpawn {
                env: vec![
                    ("EITRI_PANE_SWITCH_SOCKET".into(), pane.to_str().unwrap().into()),
                    ("EITRI_EDITOR_SOCKET".into(), context.to_str().unwrap().into()),
                ],
                args: vec!["--cmd".into(), "let g:feed = 1".into(), "file.rs".into()],
                cwd: Some("/project".into()),
                clean: true,
                background: Some((0x12, 0x34, 0x56)),
                open,
            };
            r.controller.start(WIN, StartStep::Feeds { spawn: spawn.clone() });
            r.controller.set_titles(
                WIN,
                crate::wm::chrome::ChromeTitles {
                    tray: vec![ModuleId::editor(), ModuleId::agent()],
                    ..Default::default()
                },
            );
            for d in r.layout.borrow().modules().iter().map(crate::wm::verbs::rig::decl) {
                r.host.with_surface(WIN, d.module, d.kind, false);
            }
            let driver = Driver::new(r.controller, r.host.clone(), r.scheduler.clone());
            let sink: Rc<dyn HostSink> = driver.clone();
            r.host.attach(Rc::downgrade(&sink));
            r.scheduler.attach(Rc::downgrade(&sink));
            for op in r.first.ops() {
                r.host.apply(op);
            }
            Self {
                driver,
                host: r.host,
                scheduler: r.scheduler,
                layout: r.layout,
                spawn,
            }
        }
        fn ops(&self) -> Vec<HostOp> {
            self.host
                .log()
                .into_iter()
                .filter_map(|l| match l {
                    Logged::Op(op, _) if !matches!(op, HostOp::Chrome { .. }) => Some(op),
                    _ => None,
                })
                .collect()
        }
        fn event(&self, event: EditorEvent) {
            self.driver.post(HostEvent::Editor(event));
        }
        fn key(&self) {
            self.driver
                .run(WIN, Action::Module(ModuleId::editor()), Origin::Key { keycode: 7 });
        }
        fn answer(&self, key: &str) {
            let spec = KeySpec::parse(key).unwrap();
            self.driver.key(
                WIN,
                &KeyStage::Window,
                KeyPress {
                    keycode: 42,
                    press: Press::Key(spec),
                    key: Some(spec.key),
                    mods: Mods::default(),
                    super_or_hyper: false,
                    super_or_meta: false,
                },
            );
        }
        fn press(&self, key: &str, keycode: u32) {
            let spec = KeySpec::parse(key).unwrap();
            self.driver.key(
                WIN,
                &KeyStage::Window,
                KeyPress {
                    keycode,
                    press: Press::Key(spec),
                    key: Some(spec.key),
                    mods: Mods {
                        ctrl: spec.ctrl,
                        alt: spec.meta,
                        shift: spec.shift,
                    },
                    super_or_hyper: false,
                    super_or_meta: false,
                },
            );
            self.host.post(HostEvent::KeyReleased { win: WIN, keycode });
        }
        fn kill(&self) {
            self.host.set_editor_running(true);
            self.driver.run(WIN, Action::ModuleKill, Origin::Key { keycode: 7 });
            self.answer("y");
            self.host.set_editor_facts(EditorFacts {
                exited: true,
                ..Default::default()
            });
            self.event(EditorEvent::Exited {
                win: WIN,
                settled: Some(crate::wm::close::NvimAnswer {
                    sent_at: self.scheduler.now(),
                    done: true,
                    returned: false,
                    last: None,
                }),
            });
            self.scheduler.advance(Duration::ZERO);
        }
        fn release(&self) {
            self.event(EditorEvent::Released { win: WIN });
        }
        fn restarts(&self) -> Vec<EditorSpawn> {
            self.ops()
                .into_iter()
                .filter_map(|op| match op {
                    HostOp::Editor(EditorCall::Restart { spawn, .. }) => Some(spawn),
                    _ => None,
                })
                .collect()
        }
        fn ended() -> Self {
            Self::ended_with(false)
        }
        fn ended_with(keys: bool) -> Self {
            let r = Self::with(None, keys);
            r.kill();
            r.release();
            r
        }
    }

    /// What the fake host logged for the opens: the answers by id, the files sent to the editor in order.
    impl Rig {
        fn open(&self, id: u64, name: &str) {
            self.open_with(id, name, None);
        }
        fn open_with(&self, id: u64, name: &str, token: Option<&str>) {
            self.driver.post(HostEvent::OpenFile {
                win: WIN,
                id,
                path: format!("/project/{name}").into(),
                line: None,
                col: None,
                token: token.map(str::to_string),
            });
        }
        fn answers(&self) -> Vec<(u64, bool)> {
            self.ops()
                .into_iter()
                .filter_map(|op| match op {
                    HostOp::OpenAnswer { id, accepted } => Some((id, accepted)),
                    _ => None,
                })
                .collect()
        }
        fn opened(&self) -> Vec<std::path::PathBuf> {
            self.ops()
                .into_iter()
                .filter_map(|op| match op {
                    HostOp::Editor(EditorCall::Open { path, .. }) => Some(path),
                    _ => None,
                })
                .collect()
        }
        fn ready(&self) {
            self.host.set_editor_running(true);
            self.event(EditorEvent::Ready { win: WIN });
        }
        /// The editor taken off screen by the hide verb, the keys going to the module the layout chooses.
        fn hide_editor(&self) {
            self.driver
                .run(WIN, Action::Module(ModuleId::editor()), Origin::Key { keycode: 7 });
            self.driver.run(WIN, Action::ModuleHide, Origin::Key { keycode: 7 });
            assert!(!self.layout.borrow().is_shown(&ModuleId::editor()));
        }
        fn flashes(&self) -> u64 {
            self.driver.query(|c| c.chrome(WIN).top_bar.flash)
        }
        fn toast(&self) -> Option<String> {
            self.driver.query(|c| c.chrome(WIN).toast.map(|toast| toast.text))
        }
    }

    fn project(name: &str) -> std::path::PathBuf {
        format!("/project/{name}").into()
    }

    #[test]
    fn an_open_shows_ends_zoom_presents_gives_keys_and_answers() {
        let r = Rig::new();
        r.ready();
        r.driver
            .run(WIN, Action::Module(ModuleId::agent()), Origin::Key { keycode: 7 });
        r.driver.run(WIN, Action::Zoom, Origin::Key { keycode: 7 });
        assert!(r.layout.borrow().zoom_hides(&ModuleId::editor()));
        let at = r.ops().len();
        r.driver.post(HostEvent::OpenFile {
            win: WIN,
            id: 7,
            path: project("a.rs"),
            line: Some(42),
            col: Some(3),
            token: Some("token".into()),
        });
        let ops = r.ops()[at..].to_vec();
        let at_op = |what: &str, found: &dyn Fn(&HostOp) -> bool| {
            ops.iter()
                .position(found)
                .unwrap_or_else(|| panic!("no {what} in {ops:#?}"))
        };
        let map = at_op(
            "map",
            &|op| matches!(op, HostOp::Map { module, .. } if *module == ModuleId::editor()),
        );
        let present = at_op("present", &|op| {
            *op == HostOp::Present {
                win: WIN,
                activation: Some(crate::wm::ActivationToken("token".into())),
            }
        });
        let focus = at_op(
            "focus",
            &|op| matches!(op, HostOp::Focus { to: FocusTo::Module(to), .. } if *to == ModuleId::editor()),
        );
        let open = at_op("open", &|op| {
            *op == HostOp::Editor(EditorCall::Open {
                win: WIN,
                path: project("a.rs"),
                line: Some(42),
                col: Some(3),
            })
        });
        let answer = at_op("answer", &|op| *op == HostOp::OpenAnswer { id: 7, accepted: true });
        assert!(
            map < present && present < focus && focus < open && open < answer,
            "{ops:#?}"
        );
        assert_eq!(r.layout.borrow().zoom(), None, "the zoom over the editor ended");
        assert_eq!(r.layout.borrow().focus(), &ModuleId::editor());
        assert!(r.layout.borrow().is_shown(&ModuleId::editor()));
    }

    #[test]
    fn an_open_with_the_editor_on_screen_and_the_keys_elsewhere_moves_only_the_keys() {
        let r = Rig::new();
        r.ready();
        r.driver
            .run(WIN, Action::Module(ModuleId::agent()), Origin::Key { keycode: 7 });
        let at = r.ops().len();
        r.open(1, "a.rs");
        let ops = r.ops()[at..].to_vec();
        assert!(!ops.iter().any(|op| matches!(op, HostOp::Map { .. })), "{ops:#?}");
        let present = ops.iter().position(|op| matches!(op, HostOp::Present { .. })).unwrap();
        let focus = ops.iter().position(|op| matches!(op, HostOp::Focus { .. })).unwrap();
        assert!(present < focus, "{ops:#?}");
        assert_eq!(r.layout.borrow().focus(), &ModuleId::editor());
        assert_eq!(r.opened(), [project("a.rs")]);
        assert_eq!(r.answers(), [(1, true)]);
    }

    #[test]
    fn an_open_with_the_keys_on_the_editor_presents_and_opens() {
        let r = Rig::new();
        r.ready();
        r.driver
            .run(WIN, Action::Module(ModuleId::editor()), Origin::Key { keycode: 7 });
        let at = r.ops().len();
        r.open(1, "a.rs");
        let ops = r.ops()[at..].to_vec();
        assert!(ops.iter().any(|op| matches!(op, HostOp::Present { .. })), "{ops:#?}");
        assert!(r.layout.borrow().is_shown(&ModuleId::editor()), "an open never hides");
        assert_eq!(r.opened(), [project("a.rs")]);
    }

    #[test]
    fn an_open_while_closing_answers_busy() {
        let r = Rig::new();
        r.ready();
        r.host.set_chat_kill_counts(
            ViewId::BASE,
            ChatKillCounts {
                running: 1,
                ..Default::default()
            },
        );
        r.driver.close_requested(WIN, CloseBy::User);
        let at = r.ops().len();
        r.open(1, "a.rs");
        assert_eq!(r.answers(), [(1, false)]);
        assert_eq!(
            r.ops()[at..],
            [HostOp::OpenAnswer { id: 1, accepted: false }],
            "a window on its way out is not raised"
        );
    }

    #[test]
    fn an_open_during_the_quit_flow_answers_busy() {
        let r = Rig::new();
        r.ready();
        r.driver.run(WIN, Action::ModuleKill, Origin::Key { keycode: 7 });
        r.answer("y");
        assert!(r
            .ops()
            .iter()
            .any(|op| matches!(op, HostOp::Editor(EditorCall::Quit { .. }))));
        let at = r.ops().len();
        r.open(1, "a.rs");
        assert_eq!(r.ops()[at..], [HostOp::OpenAnswer { id: 1, accepted: false }]);
    }

    #[test]
    fn an_open_before_ready_is_accepted_and_runs_on_ready() {
        let r = Rig::new();
        r.open(1, "a.rs");
        assert_eq!(r.answers(), [(1, true)]);
        assert!(r.opened().is_empty(), "an nvim that is not ready takes no call");
        r.ready();
        assert_eq!(r.opened(), [project("a.rs")]);
        r.ready();
        assert_eq!(r.opened().len(), 1, "the queue runs once");
    }

    #[test]
    fn an_open_into_a_hidden_never_started_editor_shows_it_and_runs_on_ready() {
        let r = Rig::new();
        r.hide_editor();
        r.open(1, "a.rs");
        assert!(
            r.layout.borrow().is_shown(&ModuleId::editor()),
            "only a shown editor starts"
        );
        assert_eq!(r.layout.borrow().focus(), &ModuleId::editor());
        assert_eq!(r.answers(), [(1, true)]);
        assert!(r.opened().is_empty());
        assert!(
            r.restarts().is_empty(),
            "an editor never started has nothing to restart"
        );
        r.ready();
        assert_eq!(r.opened(), [project("a.rs")]);
    }

    #[test]
    fn an_open_after_the_editor_ended_restarts_it_and_runs_on_ready() {
        let r = Rig::ended();
        r.open(1, "a.rs");
        assert_eq!(r.restarts(), [r.spawn.clone()]);
        assert_eq!(r.answers(), [(1, true)]);
        assert!(r.opened().is_empty());
        r.ready();
        assert_eq!(r.opened(), [project("a.rs")]);
    }

    #[test]
    fn an_open_while_the_ended_editor_is_being_released_restarts_it_after_the_release() {
        let r = Rig::new();
        r.kill();
        r.open(1, "a.rs");
        assert_eq!(r.answers(), [(1, true)]);
        assert!(r.restarts().is_empty(), "the old harness is still being let go");
        r.release();
        assert_eq!(r.restarts().len(), 1);
        r.ready();
        assert_eq!(r.opened(), [project("a.rs")]);
    }

    #[test]
    fn two_queued_opens_run_in_order() {
        let r = Rig::new();
        r.open(1, "a.rs");
        r.open(2, "b.rs");
        let at = r.ops().len();
        r.ready();
        let opened: Vec<_> = r.ops()[at..]
            .iter()
            .filter_map(|op| match op {
                HostOp::Editor(EditorCall::Open { path, .. }) => Some(path.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(opened, [project("a.rs"), project("b.rs")]);
    }

    #[test]
    fn the_seventeenth_queued_open_answers_busy() {
        let r = Rig::new();
        for id in 0..17 {
            r.open(id, &format!("{id}.rs"));
        }
        let answers = r.answers();
        assert_eq!(answers.len(), 17);
        assert!(answers[..16].iter().all(|(_, accepted)| *accepted));
        assert_eq!(answers[16], (16, false));
        r.ready();
        assert_eq!(r.opened().len(), 16);
    }

    #[test]
    fn a_start_that_fails_drops_the_queue_with_a_refusal() {
        let r = Rig::new();
        r.open(1, "a.rs");
        r.open(2, "b.rs");
        let before = r.flashes();
        r.event(EditorEvent::StartFailed {
            win: WIN,
            why: "nvim was not found".into(),
        });
        assert_eq!(r.flashes(), before + 1, "one refusal for the whole queue");
        assert_eq!(r.toast().unwrap(), "the editor did not start: 2 files were not opened");
        r.ready();
        assert!(r.opened().is_empty(), "a dropped open does not come back");
    }

    #[test]
    fn a_restart_that_fails_drops_the_queue_and_keeps_the_way_to_try_again() {
        let r = Rig::ended();
        r.open(1, "a.rs");
        let before = r.flashes();
        r.event(EditorEvent::StartFailed {
            win: WIN,
            why: "nvim was not found".into(),
        });
        assert_eq!(
            r.flashes(),
            before + 1,
            "one refusal, for the restart and the queue together"
        );
        assert_eq!(r.toast().unwrap(), "the editor did not start: the file was not opened");
        r.key();
        assert_eq!(r.restarts().len(), 2, "the next show tries again");
        r.ready();
        assert!(r.opened().is_empty());
    }

    #[test]
    fn a_refused_restart_drops_the_queue_with_a_refusal() {
        let r = Rig::ended();
        r.open(1, "a.rs");
        let before = r.flashes();
        r.event(EditorEvent::RestartRefused {
            win: WIN,
            state: "Ready".into(),
        });
        assert_eq!(r.flashes(), before + 1);
        assert_eq!(r.toast().unwrap(), "the editor did not start: the file was not opened");
        r.key();
        r.ready();
        assert!(r.opened().is_empty());
    }

    #[test]
    fn a_restarted_nvim_that_exits_before_ready_drops_the_queue_with_a_refusal() {
        let r = Rig::ended();
        r.open(1, "a.rs");
        r.event(EditorEvent::Exited {
            win: WIN,
            settled: None,
        });
        assert_eq!(r.toast().unwrap(), "the editor did not start: the file was not opened");
        r.release();
        r.key();
        r.ready();
        assert!(r.opened().is_empty());
    }

    #[test]
    fn an_exit_of_a_ready_editor_with_nothing_queued_refuses_nothing() {
        let r = Rig::new();
        r.ready();
        r.open(1, "a.rs");
        r.kill();
        assert!(r.toast().is_none_or(|text| !text.contains("not opened")));
    }

    #[test]
    fn an_open_after_a_failed_first_start_answers_busy() {
        let r = Rig::new();
        r.event(EditorEvent::StartFailed {
            win: WIN,
            why: "nvim was not found".into(),
        });
        let at = r.ops().len();
        r.open(1, "a.rs");
        assert_eq!(
            r.ops()[at..],
            [HostOp::OpenAnswer { id: 1, accepted: false }],
            "no restart follows a first start's failure, so a queued file would wait for good"
        );
    }

    #[test]
    fn an_open_after_a_ready_nvim_went_out_of_reach_answers_busy() {
        let r = Rig::new();
        r.ready();
        r.event(EditorEvent::Unreachable { win: WIN });
        let at = r.ops().len();
        r.open(1, "a.rs");
        assert_eq!(
            r.ops()[at..],
            [HostOp::OpenAnswer { id: 1, accepted: false }],
            "that nvim takes no call, so an accepted file would never open"
        );
    }

    #[test]
    fn an_open_after_a_starting_nvim_went_out_of_reach_answers_busy_and_does_not_wait() {
        let r = Rig::new();
        r.event(EditorEvent::Unreachable { win: WIN });
        let at = r.ops().len();
        r.open(1, "a.rs");
        assert_eq!(
            r.ops()[at..],
            [HostOp::OpenAnswer { id: 1, accepted: false }],
            "no ready follows, so a queued file would wait for good"
        );
    }

    #[test]
    fn an_editor_out_of_reach_takes_files_again_once_it_was_ended() {
        let r = Rig::new();
        r.ready();
        r.event(EditorEvent::Unreachable { win: WIN });
        r.kill();
        r.release();
        r.open(1, "a.rs");
        assert_eq!(r.answers(), [(1, true)]);
        assert_eq!(r.restarts().len(), 1);
        r.ready();
        assert_eq!(r.opened(), [project("a.rs")]);
    }

    #[test]
    fn a_failed_open_says_the_file_is_not_open_yet() {
        let r = Rig::new();
        r.ready();
        r.event(EditorEvent::OpenFailed {
            win: WIN,
            why: "nvim did not answer\nstack traceback:".into(),
        });
        assert_eq!(r.toast().unwrap(), "the file is not open yet: nvim did not answer");
    }

    #[test]
    fn an_open_for_a_window_this_controller_does_not_hold_answers_busy() {
        let r = Rig::new();
        r.driver.post(HostEvent::OpenFile {
            win: WinId(9),
            id: 3,
            path: project("a.rs"),
            line: None,
            col: None,
            token: None,
        });
        assert_eq!(r.answers(), [(3, false)]);
    }

    #[test]
    fn prefix_e_restarts_nvim_after_prefix_x_retired_it() {
        let r = Rig::ended();
        assert!(!r.layout.borrow().is_shown(&ModuleId::editor()));
        assert!(!r.layout.borrow().is_gone(&ModuleId::editor()));
        let ops = r.ops();
        let focus = ops
            .iter()
            .position(|op| matches!(op, HostOp::Focus { to: FocusTo::Module(m), .. } if *m == ModuleId::agent()))
            .unwrap();
        let unmap = ops
            .iter()
            .position(|op| matches!(op, HostOp::Unmap { module, .. } if *module == ModuleId::editor()))
            .unwrap();
        assert!(focus < unmap);
        assert!(matches!(
            ops.last(),
            Some(HostOp::Editor(EditorCall::ReleaseExited { .. }))
        ));
        let at = r.ops().len();
        r.key();
        assert_eq!(
            &r.ops()[at..],
            &[
                HostOp::Map {
                    win: WIN,
                    module: ModuleId::editor()
                },
                HostOp::Allocate { win: WIN },
                HostOp::Focus {
                    win: WIN,
                    to: FocusTo::Module(ModuleId::editor()),
                    on_grab: vec![]
                },
                HostOp::Editor(EditorCall::Restart {
                    win: WIN,
                    spawn: r.spawn.clone()
                }),
                HostOp::Editor(EditorCall::SetFocused {
                    win: WIN,
                    focused: true
                }),
                HostOp::Chat(ChatOp::EditorHasKeys {
                    win: WIN,
                    has_keys: true
                }),
                HostOp::Chat(ChatOp::PaneFocused {
                    win: WIN,
                    view: ViewId::BASE,
                    focused: false
                }),
            ]
        );
        assert_eq!(r.restarts(), [r.spawn.clone()]);
        assert!(r.layout.borrow().is_shown(&ModuleId::editor()));
        assert_eq!(r.layout.borrow().focus(), &ModuleId::editor());
    }

    #[test]
    fn prefix_e_restarts_nvim_after_a_declined_close() {
        let r = Rig::new();
        r.host.set_chat_kill_counts(
            ViewId::BASE,
            ChatKillCounts {
                running: 1,
                ..Default::default()
            },
        );
        r.host.set_editor_facts(EditorFacts {
            exited: true,
            ..Default::default()
        });
        r.event(EditorEvent::Exited {
            win: WIN,
            settled: None,
        });
        r.answer("n");
        assert_eq!(
            r.driver.query(|c| c.chrome(WIN).toast.unwrap().text),
            "nvim exited — C-b e starts it again"
        );
        r.scheduler.advance(Duration::ZERO);
        r.release();
        r.key();
        assert_eq!(r.restarts(), [r.spawn.clone()]);
    }

    #[test]
    fn a_plain_q_with_nothing_to_ask_still_closes_the_window() {
        let r = Rig::new();
        r.host.set_editor_facts(EditorFacts {
            exited: true,
            ..Default::default()
        });
        r.event(EditorEvent::Exited {
            win: WIN,
            settled: None,
        });
        r.scheduler.advance(Duration::ZERO);
        assert!(matches!(r.ops().last(), Some(HostOp::CloseWindow { .. })));
        assert!(r.restarts().is_empty());
    }

    #[test]
    fn a_second_prefix_e_while_the_editor_restarts_starts_nothing_more() {
        let r = Rig::ended();
        r.key();
        r.key();
        assert_eq!(r.restarts(), [r.spawn.clone()]);
        assert_eq!(r.layout.borrow().focus(), &ModuleId::editor());
        assert!(r.layout.borrow().is_shown(&ModuleId::editor()));
    }

    #[test]
    fn a_prefix_e_before_the_release_restarts_once_the_pane_is_released() {
        let r = Rig::new();
        r.kill();
        r.key();
        r.key();
        assert!(r.restarts().is_empty());
        r.release();
        assert_eq!(r.restarts(), [r.spawn.clone()]);
        r.release();
        r.key();
        assert_eq!(r.restarts(), [r.spawn.clone()]);
        assert_eq!(r.layout.borrow().focus(), &ModuleId::editor());
    }

    #[test]
    fn a_close_while_the_editor_restarts_shuts_it_down_and_waits_for_nothing() {
        let r = Rig::ended();
        r.key();
        r.host.set_editor_running(true);
        let at = r.ops().len();
        r.driver.close_requested(WIN, CloseBy::User);
        r.scheduler.advance(Duration::ZERO);
        let ops = r.ops();
        let ops = &ops[at..];
        assert!(ops.contains(&HostOp::Editor(EditorCall::Shutdown { win: WIN })));
        assert!(!ops
            .iter()
            .any(|op| matches!(op, HostOp::Editor(EditorCall::Quit { .. }))));
        assert!(matches!(ops.last(), Some(HostOp::CloseWindow { .. })));
        r.release();
        r.event(EditorEvent::Ready { win: WIN });
        r.key();
        assert_eq!(r.restarts().len(), 1);
    }

    #[test]
    fn an_editor_request_is_refused_while_the_editor_is_not_running() {
        let r = Rig::new();
        r.kill();
        for stage in 0..3 {
            assert_eq!(
                r.driver.query(|c| c.editor_request_refusal(WIN)),
                Some(feedback::EDITOR_NOT_RUNNING_TEXT.into())
            );
            if stage == 0 {
                r.release();
            } else if stage == 1 {
                r.key();
            }
        }
        r.host.set_editor_running(true);
        r.event(EditorEvent::Ready { win: WIN });
        assert_eq!(r.driver.query(|c| c.editor_request_refusal(WIN)), None);
    }

    #[test]
    fn the_follow_snippet_is_asked_for_again_when_the_restarted_editor_is_ready() {
        let r = Rig::ended();
        r.key();
        let at = r.ops().len();
        r.event(EditorEvent::Ready { win: WIN });
        assert_eq!(&r.ops()[at..], &[HostOp::Plugin(PluginOp::EditorBack { win: WIN })]);
    }

    #[test]
    fn a_refused_restart_leaves_the_editor_ended_and_says_why() {
        let r = Rig::ended();
        r.key();
        let at = r.ops().len();
        r.event(EditorEvent::RestartRefused {
            win: WIN,
            state: "Ready".into(),
        });
        assert_eq!(r.ops().len(), at, "only chrome changes on a refusal");
        assert_eq!(
            r.driver.query(|c| c.chrome(WIN).toast.unwrap().text),
            "the editor could not start again: the pane is Ready"
        );
        r.key();
        assert_eq!(r.restarts().len(), 2);
    }

    fn targets() -> Vec<String> {
        vec!["+3".into(), "/project/a b.rs".into()]
    }

    fn without_targets(spawn: &EditorSpawn) -> EditorSpawn {
        EditorSpawn {
            open: vec![],
            ..spawn.clone()
        }
    }

    #[test]
    fn a_restart_before_ready_keeps_the_targets() {
        let r = Rig::with_open(None, false, targets());
        assert_eq!(r.driver.query(|c| c.open_state(WIN).unwrap().0), targets());
        r.kill();
        r.release();
        r.key();
        assert_eq!(
            r.restarts(),
            [r.spawn.clone()],
            "nothing opened them, so the restart carries them"
        );
        assert_eq!(r.driver.query(|c| c.editor_spawn_open(WIN)), Some(vec![]));
        assert_eq!(r.driver.query(|c| c.open_state(WIN).unwrap().0), targets());
    }

    #[test]
    fn a_failed_restart_keeps_the_targets() {
        let r = Rig::with_open(None, false, targets());
        r.kill();
        r.release();
        r.key();
        r.event(EditorEvent::StartFailed {
            win: WIN,
            why: "nvim was not found".into(),
        });
        r.key();
        assert_eq!(r.restarts(), [r.spawn.clone(), r.spawn.clone()]);
        assert_eq!(r.driver.query(|c| c.open_state(WIN).unwrap().0), targets());
    }

    #[test]
    fn ready_clears_the_targets_and_a_later_restart_has_none() {
        let r = Rig::with_open(None, false, targets());
        assert!(!r.driver.query(|c| c.open_state(WIN).unwrap().1));
        r.event(EditorEvent::Ready { win: WIN });
        assert_eq!(r.driver.query(|c| c.open_state(WIN).unwrap()), (vec![], true, false));
        r.kill();
        assert!(
            !r.driver.query(|c| c.open_state(WIN).unwrap().1),
            "an editor that exited is not ready"
        );
        r.release();
        r.key();
        assert_eq!(r.restarts(), [without_targets(&r.spawn)]);
        assert!(
            !r.driver.query(|c| c.open_state(WIN).unwrap().1),
            "a restart begun is not ready"
        );
        r.event(EditorEvent::Ready { win: WIN });
        assert!(r.driver.query(|c| c.open_state(WIN).unwrap().1));
    }

    #[test]
    fn a_restart_that_is_ready_again_opens_nothing_even_after_a_second_restart() {
        let r = Rig::with_open(None, false, targets());
        r.kill();
        r.release();
        r.key();
        r.event(EditorEvent::Ready { win: WIN });
        r.kill();
        r.release();
        r.key();
        assert_eq!(r.restarts(), [r.spawn.clone(), without_targets(&r.spawn)]);
    }

    #[test]
    fn a_restart_whose_launch_failed_leaves_the_editor_ended_and_the_next_show_tries_again() {
        let r = Rig::ended();
        r.key();
        let at = r.ops().len();
        r.event(EditorEvent::StartFailed {
            win: WIN,
            why: "nvim was not found".into(),
        });
        let failed = r.ops()[at..].to_vec();
        assert!(
            !failed
                .iter()
                .any(|op| matches!(op, HostOp::Editor(call) if !matches!(call, EditorCall::SetFocused { .. }))),
            "the editor is asked nothing more when the launch fails, but told it lost the keys: {failed:#?}"
        );
        assert!(
            failed
                .iter()
                .any(|op| matches!(op, HostOp::Focus { to: FocusTo::Module(to), .. } if *to == ModuleId::agent())),
            "the restarted editor held the keys, and a pane with no nvim does not keep them: {failed:#?}"
        );
        assert_eq!(
            r.driver.query(|c| c.chrome(WIN).toast.unwrap().text),
            "nvim could not start \u{2014} C-b e tries again"
        );
        assert_eq!(
            r.driver.query(|c| c.editor_request_refusal(WIN)),
            Some(feedback::EDITOR_NOT_RUNNING_TEXT.into()),
            "a pane with no nvim takes no requests"
        );
        r.key();
        assert_eq!(r.restarts().len(), 2, "the next show restarts the failed pane");
        r.host.set_editor_running(true);
        r.event(EditorEvent::Ready { win: WIN });
        assert_eq!(r.driver.query(|c| c.editor_request_refusal(WIN)), None);
    }

    #[test]
    fn a_first_starts_failure_changes_nothing_in_the_controller() {
        let r = Rig::new();
        let at = r.ops().len();
        r.event(EditorEvent::StartFailed {
            win: WIN,
            why: "nvim was not found".into(),
        });
        assert_eq!(r.ops().len(), at);
        assert!(
            r.driver.query(|c| c.chrome(WIN).toast.is_none()),
            "a first start's failure toasts nothing"
        );
        r.key();
        assert!(
            r.restarts().is_empty(),
            "an editor that was never ended is not restarted"
        );
    }

    #[test]
    fn a_refused_release_is_retried_and_logged_once() {
        let r = Rig::new();
        r.kill();
        let releases = || {
            r.ops()
                .iter()
                .filter(|op| matches!(op, HostOp::Editor(EditorCall::ReleaseExited { .. })))
                .count()
        };
        for n in 1..=2 {
            r.event(EditorEvent::ReleaseRefused { win: WIN });
            r.scheduler.advance(Duration::from_millis(999));
            assert_eq!(releases(), n);
            r.scheduler.advance(Duration::from_millis(1));
            assert_eq!(releases(), n + 1);
        }
        assert_eq!(
            r.driver
                .query(|c| c.notes(WIN))
                .iter()
                .filter(|s| s.contains("nvim's process is still alive"))
                .count(),
            1
        );
        r.release();
        r.scheduler.advance(Duration::from_secs(2));
        assert_eq!(releases(), 3);
    }

    #[test]
    fn every_show_of_an_ended_editor_restarts_it() {
        for case in 0..5 {
            let r = Rig::ended();
            match case {
                0 => r.driver.run(WIN, Action::Module(ModuleId::editor()), Origin::Chip),
                1 => crate::wm::fake::lua(&r.driver, WIN, LayoutRequest::Show(ModuleId::editor())),
                2 => crate::wm::fake::lua(&r.driver, WIN, LayoutRequest::Focus(ModuleId::editor())),
                3 => r.driver.follow(WIN, &ModuleId::editor(), true),
                _ => r.driver.post(HostEvent::Chat(ChatEvent::ReviewOpen { win: WIN })),
            }
            assert_eq!(r.restarts(), [r.spawn.clone()], "show {case}");
            assert!(r.layout.borrow().is_shown(&ModuleId::editor()));
        }
    }

    #[test]
    fn switching_to_a_workspace_that_showed_the_ended_editor_keeps_it_hidden() {
        let r = Rig::new();
        crate::wm::fake::lua(&r.driver, WIN, LayoutRequest::Hide(ModuleId::agent()));
        r.driver.run(WIN, Action::WorkspaceNew, Origin::Lua);
        crate::wm::fake::lua(&r.driver, WIN, LayoutRequest::Show(ModuleId::agent()));
        r.kill();
        r.release();
        crate::wm::fake::lua(&r.driver, WIN, LayoutRequest::Switch("main".into()));
        assert!(!r.layout.borrow().is_shown(&ModuleId::editor()));
        assert!(!r.layout.borrow().is_gone(&ModuleId::editor()));
        assert!(!r.layout.borrow().saved_hidden().contains(&ModuleId::editor()));
        assert!(r.layout.borrow().is_shown(&ModuleId::agent()));
        assert!(r.restarts().is_empty());
    }

    #[test]
    fn the_ended_editor_keeps_its_tray_chip() {
        let r = Rig::ended();
        assert!(crate::layout::tray(&r.layout.borrow()).contains(&ModuleId::editor()));
        assert!(r.driver.query(|c| c
            .chrome(WIN)
            .tray
            .iter()
            .any(|chip| chip.module == ModuleId::editor() && chip.shown)));
        r.driver.run(WIN, Action::Module(ModuleId::editor()), Origin::Chip);
        assert_eq!(r.restarts().len(), 1);
    }

    #[test]
    fn an_editor_in_the_other_window_is_never_restarted_here() {
        let mut r = rig(Layout::initial(&[]).unwrap());
        let w = r.controller.window_spec(WIN).unwrap().clone();
        // A second window owns no nvim and must leave the first window's session alone.
        let parts = crate::wm::WindowParts {
            layout: r.layout.clone(),
            workspaces: Rc::new(RefCell::new(crate::layout::Workspaces::single())),
            saver: None,
        };
        r.controller.open_window(
            crate::wm::WinId(2),
            crate::wm::WindowSpec {
                presence: EditorPresence::Proxy,
                ..w
            },
            parts,
        );
        // Each window's editor as an ended one with a spawn to restart it with, so only the window's presence
        // decides whether its show restarts it.
        let spawn = EditorSpawn {
            env: vec![],
            args: vec![],
            cwd: None,
            clean: false,
            background: None,
            open: vec![],
        };
        for win in [WIN, crate::wm::WinId(2)] {
            let w = r.controller.window_mut(win).unwrap();
            w.editor_run = EditorRun::Ended;
            w.editor_spawn = Some(spawn.clone());
        }
        let restarts = |batch: &Batch| {
            batch
                .ops()
                .filter(|op| matches!(op, HostOp::Editor(EditorCall::Restart { .. })))
                .count()
        };
        let batch = r.controller.follow(crate::wm::WinId(2), &ModuleId::editor(), true);
        assert_eq!(restarts(&batch), 0, "the proxy window starts no nvim");
        assert_eq!(
            r.controller.window_mut(crate::wm::WinId(2)).unwrap().editor_run,
            EditorRun::Ended
        );
        let batch = r.controller.follow(WIN, &ModuleId::editor(), true);
        assert_eq!(
            restarts(&batch),
            1,
            "the owning window's show restarts its ended editor"
        );
    }

    #[test]
    fn a_restarted_nvim_that_exits_before_it_is_ready_ends_in_place_again() {
        let r = Rig::ended();
        r.key();
        r.host.set_chat_kill_counts(
            ViewId::BASE,
            ChatKillCounts {
                running: 1,
                ..Default::default()
            },
        );
        let at = r.ops().len();
        r.event(EditorEvent::Exited {
            win: WIN,
            settled: None,
        });
        r.scheduler.advance(Duration::ZERO);
        let ops = r.ops();
        let ops = &ops[at..];
        assert!(!ops.iter().any(|op| matches!(op, HostOp::CloseWindow { .. })));
        assert!(ops.contains(&HostOp::Editor(EditorCall::ReleaseExited { win: WIN })));
        assert!(
            r.driver.query(|c| c.chrome(WIN).prompt.is_none()),
            "nothing asks to close the window"
        );
        assert_eq!(
            r.driver.query(|c| c.chrome(WIN).toast.unwrap().text),
            "nvim exited — C-b e starts it again"
        );
        assert!(!r.layout.borrow().is_shown(&ModuleId::editor()));
        assert!(!r.layout.borrow().is_gone(&ModuleId::editor()));
        r.release();
        r.key();
        assert_eq!(r.restarts(), [r.spawn.clone(), r.spawn.clone()]);
        assert!(r.layout.borrow().is_shown(&ModuleId::editor()));
    }

    #[test]
    fn prefix_x_on_an_editor_shown_before_its_release_hides_it_and_takes_back_its_restart() {
        let r = Rig::new();
        r.kill();
        r.key();
        assert!(r.layout.borrow().is_shown(&ModuleId::editor()));
        r.driver.run(WIN, Action::ModuleKill, Origin::Key { keycode: 7 });
        r.answer("y");
        assert!(!r.layout.borrow().is_shown(&ModuleId::editor()));
        assert!(!r.layout.borrow().saved_hidden().contains(&ModuleId::editor()));
        r.release();
        assert!(r.restarts().is_empty(), "the show's restart went with the kill");
        r.key();
        assert_eq!(r.restarts(), [r.spawn.clone()]);
    }

    #[test]
    fn prefix_x_on_an_editor_whose_restart_was_refused_hides_it() {
        let r = Rig::ended();
        r.key();
        r.event(EditorEvent::RestartRefused {
            win: WIN,
            state: "Ready".into(),
        });
        assert!(r.layout.borrow().is_shown(&ModuleId::editor()));
        r.driver.run(WIN, Action::ModuleKill, Origin::Key { keycode: 7 });
        r.answer("y");
        assert!(!r.layout.borrow().is_shown(&ModuleId::editor()));
        r.key();
        assert_eq!(r.restarts().len(), 2);
    }

    #[test]
    fn a_hide_after_a_show_before_the_release_keeps_the_editor_hidden_once_released() {
        let r = Rig::new();
        r.kill();
        r.key();
        assert!(r.layout.borrow().is_shown(&ModuleId::editor()));
        crate::wm::fake::lua(&r.driver, WIN, LayoutRequest::Hide(ModuleId::editor()));
        r.release();
        assert!(r.restarts().is_empty(), "the hide took back the show's restart");
        assert!(!r.layout.borrow().is_shown(&ModuleId::editor()));
        r.key();
        assert_eq!(r.restarts(), [r.spawn.clone()]);
    }

    #[test]
    fn a_workspace_switch_after_a_show_before_the_release_restarts_nothing() {
        let r = Rig::new();
        r.driver.run(WIN, Action::WorkspaceNew, Origin::Lua);
        crate::wm::fake::lua(&r.driver, WIN, LayoutRequest::Switch("main".into()));
        r.kill();
        r.key();
        crate::wm::fake::lua(&r.driver, WIN, LayoutRequest::Switch("2".into()));
        assert!(!r.layout.borrow().is_shown(&ModuleId::editor()));
        r.release();
        assert!(r.restarts().is_empty());
        assert!(!r.layout.borrow().is_shown(&ModuleId::editor()));
    }

    #[test]
    fn a_split_that_places_the_ended_editor_restarts_it() {
        for lua in [false, true] {
            let r = Rig::ended_with(true);
            if lua {
                crate::wm::fake::lua(
                    &r.driver,
                    WIN,
                    LayoutRequest::Split(ModuleId::editor(), crate::layout::Axis::Row),
                );
            } else {
                let split = Keymap::defaults()
                    .bindings()
                    .iter()
                    .find(|b| {
                        matches!(
                            b.action,
                            Action::Split {
                                axis: crate::layout::Axis::Row,
                                ..
                            }
                        )
                    })
                    .expect("a default split.right key")
                    .clone();
                r.press("C-b", 50);
                r.press(&split.key.to_string(), 51);
                r.press("e", 52);
            }
            assert_eq!(r.restarts(), [r.spawn.clone()], "lua {lua}");
            assert!(r.layout.borrow().is_shown(&ModuleId::editor()));
        }
    }

    #[test]
    fn a_split_that_places_the_editor_before_its_release_restarts_it_once_released() {
        let r = Rig::new();
        r.kill();
        crate::wm::fake::lua(
            &r.driver,
            WIN,
            LayoutRequest::Split(ModuleId::editor(), crate::layout::Axis::Row),
        );
        assert!(r.restarts().is_empty());
        r.release();
        assert_eq!(r.restarts(), [r.spawn.clone()]);
    }

    #[test]
    fn a_workspace_first_made_after_the_editor_ended_keeps_it_hidden() {
        let template = crate::lua::layout::LuaTree {
            tiled: crate::layout::Node::split(
                crate::layout::Axis::Column,
                0.3,
                crate::layout::Node::Leaf(ModuleId::agent()),
                crate::layout::Node::Leaf(ModuleId::editor()),
            ),
            floats: Vec::new(),
        };
        let r = Rig::with(
            Some(ModuleSetup {
                workspaces: vec![("study".into(), Ok(template))],
                ..Default::default()
            }),
            false,
        );
        r.kill();
        r.release();
        crate::wm::fake::lua(&r.driver, WIN, LayoutRequest::Switch("study".into()));
        assert!(r.layout.borrow().contains(&ModuleId::editor()));
        assert!(!r.layout.borrow().is_shown(&ModuleId::editor()));
        assert!(!r.layout.borrow().is_gone(&ModuleId::editor()));
        assert!(r.layout.borrow().is_shown(&ModuleId::agent()));
        assert!(r.restarts().is_empty());
        r.key();
        assert_eq!(r.restarts(), [r.spawn.clone()]);
        assert!(r.layout.borrow().is_shown(&ModuleId::editor()));
    }
}
