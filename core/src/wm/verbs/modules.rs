//! The module verbs: a module's key with each kind's rule, `module.hide`, `module.kill` and its y/n, a new terminal
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
use super::super::close::{self, KillScope, PromptKey};
use super::super::feedback;
use super::super::focus::{self, arrive_tail, focus_and_arrive, give_keys};
use super::super::grid::order;
use super::super::host::*;
use super::super::instances::plan_new_instance;
use super::super::module::{ModuleState, SurfaceDecl};
use super::super::moves;
use super::super::window::{WinId, Window};
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

/// What a window's module verbs wait for: the module the shown y/n would kill, the new instances whose surface
/// was asked for, and the splits waiting for the directory of the terminal they were pressed in.
#[derive(Debug, Default)]
pub struct ModuleVerbs {
    kill: Option<ModuleId>,
    opens: Vec<PendingOpen>,
    cwd_reads: Vec<PendingCwd>,
    /// The next directory read's own number, which its answer carries back.
    next_cwd_read: u64,
}

impl ModuleVerbs {
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

/// Whether a verb was carried out here or is the shell's: the last module's kill, which is the window close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ran {
    Done,
    Shell,
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
    if !has_host(w, &id) {
        let text = feedback::not_here_yet_text(&id);
        w.say(format!("[prefix] {text}"));
        w.refuse(win, text, batch);
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

/// A tray chip: `prefix <key>` for its module from the top bar, which never holds the keys itself, so a chip never
/// hides its module. It meets neither the selection's gate nor the float's and never goes to a chat's waiting card.
fn chip(w: &mut Window, cx: &Cx, win: WinId, id: &ModuleId, batch: &mut Batch) {
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
    w.changed_with_focus(win, next.map(|next| (next, vec![])), batch);
    Ok(())
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
                ModuleKind::Terminal | ModuleKind::Agent => plan_new_instance(kind, &here, &reserved, &[]),
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
        } else {
            None
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
/// it. [`Ran::Shell`] when it is the last module on screen, whose close is the window's.
pub fn kill_confirmed(w: &mut Window, cx: &Cx, win: WinId, id: &ModuleId, batch: &mut Batch) -> Ran {
    kill_now(w, cx, win, id, batch)
}

/// `prefix x` with the keys on a module that is not the editor: refused at once if the layout would refuse the
/// kill; the last module on screen is the window close, whose question is the shell's; any other is asked in the
/// window's y/n, naming what its kill ends.
fn kill_asked(w: &mut Window, cx: &Cx, win: WinId, batch: &mut Batch) -> Ran {
    let Some(id) = target(w) else {
        return Ran::Done;
    };
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
        return Ran::Shell;
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
    w.modules.kill = Some(id);
    chrome::set_prompt(
        w,
        Some(Prompt {
            text,
            closes_window: false,
        }),
    );
    Ran::Done
}

/// The y/n's answer: it closes either way, and `y` kills the module it asked about.
pub fn prompt_answer(w: &mut Window, cx: &Cx, win: WinId, answer: PromptKey, batch: &mut Batch) -> Ran {
    chrome::set_prompt(w, None);
    let Some(id) = w.modules.kill.take() else {
        return Ran::Done;
    };
    match answer {
        PromptKey::Yes => kill_now(w, cx, win, &id, batch),
        PromptKey::No | PromptKey::Ignore => Ran::Done,
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
        Ok(Ok(KillScope::Window)) => return Ran::Shell,
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
            debug_assert!(false, "the editor's kill is its quit, which is the shell's");
            return Ran::Shell;
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
fn remaining_views(w: &Window) -> Vec<ViewId> {
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
