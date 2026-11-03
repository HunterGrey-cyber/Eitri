//! What the keys do in `compositor` mode, where chats and pages are windows of the compositor: in window 1 (the
//! hub: the editor and an optional bottom terminal) and in a module window (one chat or one page). The two tables
//! that sort every action are [`crate::keymap::compositor`]'s; this module carries each verdict out as ops. A window
//! that keeps its own layout (`Tiling::Eitri`) never reaches it.
//!
//! `tab.break` is the one verb that spans three windows. The chat window asks once the tab's turn has ended, and window
//! 1 names the target ([`HostOp::HandOverTab`]); the shell then carries the hand-over between the links. Module windows
//! never link to each other, so the session goes by resume: the source lets go of it and the target takes it up. A tab
//! with no session has nothing to resume: its `tab.break` asks for the next free chat window instead, which is what
//! opens a new, empty chat window from the default keys.
//!
//! A page window beside the person's own nvim (`ModuleWindowKind::Beside`) is a module window with no window 1: it
//! asks no hub, runs nothing in a peer and starts no window of a group. Its keys and follow commands become
//! [`HostOp::Beside`] ops the host carries out by the nvim it follows, a close, a compositor focus move, or one toast.

use super::super::chrome;
use super::super::close;
use super::super::feedback;
use super::super::focus;
use super::super::host::*;
use super::super::moves;
use super::super::start::StartWindows;
use super::super::toasts::ToastKind;
use super::super::window::{ActivationToken, PeerId, Tiling, WinId, Window};
use super::modules::Cx;
use crate::agent_backend::AgentId;
use crate::keymap::action::{parse, Action, OptValue};
use crate::keymap::compositor::{classify_module, classify_window1, off_hint, HubVerb, ModuleVerb};
use crate::layout::{ModuleId, ModuleKind};
use crate::plugin::follow_feed::FollowCmd;
use crate::tabs::TabId;
use crate::window_link::names::WindowName;
use crate::window_link::{FocusTarget, HubAsk};

/// Who asked for a window to come forward or to start: a key in this window, or another window by link.
pub enum Asked {
    ByKey,
    /// A module window, with its activation token.
    ByWindow {
        token: Option<String>,
    },
}

impl Asked {
    fn token(&self) -> Option<String> {
        match self {
            Asked::ByKey => None,
            Asked::ByWindow { token } => token.clone(),
        }
    }
}

/// Decides `action` by the window's tiling: `true` when it was decided here and its ops are in `batch`, `false`
/// when it goes on to the family routing as it does in `eitri` mode.
pub fn gate(w: &mut Window, reads: &dyn HostReads, win: WinId, action: &Action, batch: &mut Batch) -> bool {
    match w.tiling {
        Tiling::Eitri => false,
        Tiling::Hub => run_hub(w, reads, win, action, batch),
        Tiling::ModuleWindow => run_module(w, reads, win, action, batch),
    }
}

/// As [`gate`], for what window 1 sent this window to run. Everything but `tab.break` runs as it is: window 1 already
/// sorted it. `tab.break` has its verb only here, where it asks window 1 back, and that ask cannot return as this.
pub fn gate_sent(w: &mut Window, reads: &dyn HostReads, win: WinId, action: &Action, batch: &mut Batch) -> bool {
    let breaks = matches!(action, Action::TabBreak) && matches!(w.tiling, Tiling::ModuleWindow);
    if breaks {
        tab_break(w, reads, win, batch);
    }
    breaks
}

/// Window 1's verdict on `action`.
pub fn run_hub(w: &mut Window, reads: &dyn HostReads, win: WinId, action: &Action, batch: &mut Batch) -> bool {
    match classify_window1(action) {
        HubVerb::AsToday => hide_of_a_window(w, action),
        HubVerb::RaiseOrSpawn(id) => {
            module_key(w, win, &id, &Asked::ByKey, batch);
            true
        }
        HubVerb::NextChatWindow => {
            next_chat_window(w, reads, win, &Asked::ByKey, batch);
            true
        }
        HubVerb::ToLastChat => {
            to_last_chat(w, win, action, &Asked::ByKey, batch);
            true
        }
        HubVerb::Off => {
            off(w, reads, action);
            true
        }
    }
}

/// A module window's verdict on `action`.
pub fn run_module(w: &mut Window, reads: &dyn HostReads, win: WinId, action: &Action, batch: &mut Batch) -> bool {
    if w.is_beside() {
        return run_beside(w, win, action, batch);
    }
    if in_a_page_window(w) && matches!(classify_window1(action), HubVerb::ToLastChat) {
        w.say(format!(
            "[keys] {}: this window has no chat; asks window 1",
            action.name()
        ));
        let ask = HubAsk::ChatKey {
            name: action.name(),
            opts: action.wire_options(),
        };
        batch.push_effect(HostOp::HubAsk { win, ask });
        return true;
    }
    match classify_module(action) {
        ModuleVerb::AsToday => return false,
        ModuleVerb::EdgeToCompositor(direction) => {
            w.say(format!("[keys] {}: the compositor moves the focus", action.name()));
            batch.push_effect(HostOp::CompositorFocus { win, direction });
        }
        ModuleVerb::AskHub => match ask_of(action) {
            Some(ask) => {
                w.say(format!("[keys] {}: asks window 1", action.name()));
                batch.push_effect(HostOp::HubAsk { win, ask });
            }
            None => w.say(format!("[keys] {}: nothing to ask window 1", action.name())),
        },
        ModuleVerb::TabBreak => tab_break(w, reads, win, batch),
        ModuleVerb::LogOnly => w.say(format!("[keys] {}: nothing to act on in this window", action.name())),
        ModuleVerb::Off => off(w, reads, action),
    }
    true
}

/// Which of a beside window's texts it has shown: each is toasted on its first press in the window, then only logged.
#[derive(Debug, Default)]
pub struct BesideToasts {
    off: bool,
    no_chat: bool,
    tmux: bool,
}

/// A beside window's verdict on `action`: `true` when it was decided here, `false` when it goes on to the family
/// routing as in any module window (text size, `f`, the send-prefix, the kill that closes the window). Nothing it
/// decides reaches another window by a link: there is none.
pub fn run_beside(w: &mut Window, win: WinId, action: &Action, batch: &mut Batch) -> bool {
    match action {
        // `tab.break` has no tab to move and no chat window to move it to.
        Action::TabBreak | Action::InstanceNew { .. } | Action::ProjectChoose => {
            beside_off(w, action);
            return true;
        }
        Action::Module(id) => {
            beside_module_key(w, action, id, batch);
            return true;
        }
        // The keys sheet is drawn in a chat's page, which this window has none of, and no host call opens the page's
        // own sheet: every press says where the keys are listed.
        Action::PanelKeymap => {
            w.say(format!(
                "[keys] {}: the page lists its own keys on its ?",
                action.name()
            ));
            chrome::show_toast(w, feedback::BESIDE_PAGE_KEYS_TEXT.to_owned(), ToastKind::Other);
            return true;
        }
        _ => {}
    }
    if matches!(classify_window1(action), HubVerb::ToLastChat) {
        let name = action.name();
        w.say(format!("[keys] {name}: no chat in a window beside nvim"));
        if !w.beside_toasts.no_chat {
            w.beside_toasts.no_chat = true;
            chrome::show_toast(w, feedback::BESIDE_NO_CHAT_TEXT.to_owned(), ToastKind::Other);
        }
        return true;
    }
    match classify_module(action) {
        ModuleVerb::AsToday => return false,
        ModuleVerb::EdgeToCompositor(direction) => {
            w.say(format!("[keys] {}: the compositor moves the focus", action.name()));
            batch.push_effect(HostOp::CompositorFocus { win, direction });
        }
        ModuleVerb::AskHub | ModuleVerb::TabBreak | ModuleVerb::Off => beside_off(w, action),
        ModuleVerb::LogOnly => w.say(format!("[keys] {}: nothing to act on in this window", action.name())),
    }
    true
}

/// A module key in a beside window: the agent's window and another page's window are the host's to raise or start,
/// the editor's is the window holding the nvim this one follows. A terminal or a canvas has no window here.
fn beside_module_key(w: &mut Window, action: &Action, id: &ModuleId, batch: &mut Batch) {
    let name = action.name();
    match id.kind() {
        ModuleKind::Agent => {
            w.say(format!("[keys] {name}: the agent window comes forward, or one starts"));
            batch.push_effect(HostOp::Beside(BesideOp::Agent));
        }
        ModuleKind::Editor => beside_raise_editor(w, &name, batch),
        ModuleKind::LuaWebview => match WindowName::for_module(id) {
            Some(page) if w.module_name.as_ref() == Some(&page) => {
                w.say(format!("[keys] {name}: this window shows it"));
            }
            Some(WindowName::Page(page)) => {
                w.say(format!("[keys] {name}: its page window comes forward, or one starts"));
                batch.push_effect(HostOp::Beside(BesideOp::Page(page)));
            }
            Some(WindowName::Chat(_) | WindowName::Second) | None => no_page_window(w, id),
        },
        ModuleKind::Terminal | ModuleKind::Canvas => beside_off(w, action),
    }
}

/// The window holding the nvim a beside window follows comes forward, for `what`: a key or a page's focus.
pub fn beside_raise_editor(w: &mut Window, what: &str, batch: &mut Batch) {
    w.say(format!("[keys] {what}: the editor's window comes forward"));
    batch.push_effect(HostOp::Beside(BesideOp::RaiseEditor));
}

/// A key a beside window has no use for: the first press says why on the toast, every press on the log.
fn beside_off(w: &mut Window, action: &Action) {
    w.say(format!("[keys] {}: off in a window beside nvim", action.name()));
    if !w.beside_toasts.off {
        w.beside_toasts.off = true;
        chrome::show_toast(w, feedback::BESIDE_OFF_TEXT.to_owned(), ToastKind::Other);
    }
}

/// The host could not carry out a beside op. Inside tmux the editor's window cannot be reached, which is said once
/// and then only logged, as an off key is; any other reason is a refusal, shown each time.
pub fn beside_refused(w: &mut Window, win: WinId, why: String, batch: &mut Batch) {
    w.say(format!("[window] beside: {why}"));
    if why != feedback::BESIDE_TMUX_TEXT {
        w.refuse(win, why, batch);
    } else if !w.beside_toasts.tmux {
        w.beside_toasts.tmux = true;
        chrome::show_toast(w, why, ToastKind::Other);
    }
}

/// A page window holds one page and no chat, so a chat key has nothing to act on there.
fn in_a_page_window(w: &Window) -> bool {
    matches!(w.module_name, Some(WindowName::Page(_)))
}

/// What a module window asks window 1 for `action`.
fn ask_of(action: &Action) -> Option<HubAsk> {
    match action {
        Action::Module(id) => Some(HubAsk::Module {
            id: id.as_str().to_owned(),
        }),
        Action::InstanceNew {
            kind: ModuleKind::Agent,
        } => Some(HubAsk::InstanceNew),
        _ => None,
    }
}

/// A key the compositor owns: the first press in a window says so on the toast, every press on the log.
fn off(w: &mut Window, reads: &dyn HostReads, action: &Action) {
    off_by_name(w, reads, &action.name());
}

/// As [`off`], for what is named by more than a key: a `:Eitri` verb or an `eitri.layout` call.
pub fn off_by_name(w: &mut Window, reads: &dyn HostReads, name: &str) {
    w.say(feedback::off_in_compositor_mode_log(name));
    if !w.hint_shown {
        w.hint_shown = true;
        chrome::show_toast(w, off_hint(reads.compositor_name()), ToastKind::Other);
    }
}

/// `module.hide` with the keys on a chat or a page has nothing to hide: those windows belong to the compositor.
fn hide_of_a_window(w: &mut Window, action: &Action) -> bool {
    if !matches!(action, Action::ModuleHide | Action::ModuleHideByKey) {
        return false;
    }
    let Ok(focus) = w.layout.try_borrow().map(|layout| layout.focus().clone()) else {
        return false;
    };
    if !matches!(focus.kind(), ModuleKind::Agent | ModuleKind::LuaWebview) {
        return false;
    }
    w.say(format!(
        "[keys] {}: {focus} is a window of the compositor",
        action.name()
    ));
    true
}

/// `prefix d` and a module key in window 1: `true` when that module is a window of the compositor, which has only the
/// log to say so.
pub fn hide_key_of_a_window(w: &mut Window, module: &ModuleId) -> bool {
    if !matches!(w.tiling, Tiling::Hub) || !matches!(module.kind(), ModuleKind::Agent | ModuleKind::LuaWebview) {
        return false;
    }
    w.say(format!(
        "[keys] module.hide-by-key: {module} is a window of the compositor"
    ));
    true
}

/// A plugin's follow command, in a window where plugin pages are windows of their own; `false` where they are tiles.
/// Window 1 opens a page that has no window yet on a `Show`, leaving the keys where they are as a follow command does
/// in `eitri` mode. Once a page has its window in a group the command reaches that window, which keeps it: only the
/// compositor closes it. A window beside nvim has no window 1 to open it, so it answers every command itself.
pub fn follow(w: &mut Window, cx: &Cx, win: WinId, module: &ModuleId, cmd: FollowCmd, batch: &mut Batch) -> bool {
    match w.tiling {
        Tiling::Eitri => return false,
        Tiling::ModuleWindow if w.is_beside() => follow_beside(w, cx, win, module, cmd, batch),
        Tiling::Hub if cmd == FollowCmd::Show => follow_opens(w, win, module, batch),
        Tiling::Hub | Tiling::ModuleWindow => w.say(format!(
            "[modules] {module}: the editor's follow command leaves the window as it is, the compositor's"
        )),
    }
    true
}

/// A follow command in a window beside nvim, which is open exactly while its page is shown. `Show` brings it
/// forward; `Hide` and `Toggle` close it, asking nothing; `Auto` fires on every buffer of a followed filetype the
/// person enters, so it never raises the window or takes the keyboard from nvim.
fn follow_beside(w: &mut Window, cx: &Cx, win: WinId, module: &ModuleId, cmd: FollowCmd, batch: &mut Batch) {
    match cmd {
        FollowCmd::Show => {
            w.say(format!("[modules] {module}: shown, so this window comes forward"));
            batch.push_effect(HostOp::Beside(BesideOp::RaiseSelf));
        }
        FollowCmd::Hide | FollowCmd::Toggle => {
            w.say(format!("[modules] {module}: {cmd:?} closes this window"));
            close::request(w, cx, win, close::CloseCause::User, batch);
        }
        FollowCmd::Auto => w.say(format!(
            "[modules] {module}: a followed buffer was entered; this window stays where it is"
        )),
    }
}

fn follow_opens(w: &mut Window, win: WinId, module: &ModuleId, batch: &mut Batch) {
    let Some(name) = WindowName::for_module(module) else {
        no_page_window(w, module);
        return;
    };
    if w.peer_named(&name).is_none() && !w.pending_runs.contains_key(&name) {
        spawn(w, win, name, None, true, batch);
    }
}

/// `id` is a panel with no window of its own: its id cannot name one. One line and one toast say so.
fn no_page_window(w: &mut Window, id: &ModuleId) {
    let page = id.as_str().trim_start_matches("lua:");
    w.say(format!("[window] {page}: no page window for this id"));
    chrome::show_toast(w, feedback::no_page_window_text(page), ToastKind::Other);
}

/// The window `id` shows comes forward, or is started when none exists; one that is starting is waited for.
pub fn raise_or_spawn(w: &mut Window, win: WinId, id: &ModuleId, asked: &Asked, batch: &mut Batch) {
    match WindowName::for_module(id) {
        Some(name) => raise_or_spawn_window(w, win, name, asked, batch),
        None => no_page_window(w, id),
    }
}

/// A module key: as [`raise_or_spawn`], but the chat key goes to a chat window waiting on the user first, as it goes to
/// the view with the oldest card among tiles. Window 1 knows only each window's count, so among several it picks the
/// one last used, else the lowest number.
fn module_key(w: &mut Window, win: WinId, id: &ModuleId, asked: &Asked, batch: &mut Batch) {
    let waiting = (*id == ModuleId::agent()).then(|| waiting_chat(w)).flatten();
    match waiting {
        Some(name) => raise_or_spawn_window(w, win, name, asked, batch),
        None => raise_or_spawn(w, win, id, asked, batch),
    }
}

/// The chat window to go to for what waits on the user, by each window's last summary.
fn waiting_chat(w: &Window) -> Option<WindowName> {
    let waits = |peer: &PeerId| {
        w.peer(*peer)
            .is_some_and(|p| matches!(p.name(), WindowName::Chat(_)) && p.summary().is_some_and(|s| s.waiting > 0))
    };
    if let Some(last) = w.last_chat.filter(waits).and_then(|peer| w.peer(peer)) {
        return Some(last.name().clone());
    }
    (1..=9u8)
        .map(WindowName::Chat)
        .find(|name| w.peer_named(name).is_some_and(|peer| waits(&peer)))
}

fn raise_or_spawn_window(w: &mut Window, win: WinId, name: WindowName, asked: &Asked, batch: &mut Batch) {
    if let Some(peer) = w.peer_named(&name) {
        let raised = match asked {
            Asked::ByKey => moves::raise_for(w, win, peer, FocusTarget::Window, batch),
            Asked::ByWindow { token } => moves::forward_raise(w, win, peer, token.clone(), batch),
        };
        if raised {
            w.say(format!("[window] {} comes forward", name.text()));
        }
    } else if w.pending_runs.contains_key(&name) {
        w.say(format!("[window] {} is starting", name.text()));
    } else {
        spawn(w, win, name, asked.token(), false, batch);
    }
}

/// Opens what a hub's start owes, in order: nothing comes forward, so the keys stay in the editor. A window that is
/// already there (an orphan this start relinked) or starting is left as it is.
pub fn open_at_start(w: &mut Window, win: WinId, windows: StartWindows, batch: &mut Batch) {
    for id in &windows.unnamed {
        no_page_window(w, id);
    }
    for name in windows.open {
        if w.peer_named(&name).is_none() && !w.pending_runs.contains_key(&name) {
            spawn(w, win, name, None, true, batch);
        }
    }
}

/// The lowest free chat window of 2 to 9 is started: one neither linked nor already starting.
pub fn next_chat_window(w: &mut Window, reads: &dyn HostReads, win: WinId, asked: &Asked, batch: &mut Batch) {
    match free_chat_window(w, reads) {
        Some(name) => spawn(w, win, name, asked.token(), false, batch),
        None => {
            w.say(format!("[window] {}", feedback::ALL_CHAT_WINDOWS_TEXT));
            w.refuse(win, feedback::ALL_CHAT_WINDOWS_TEXT.to_owned(), batch);
        }
    }
}

/// The lowest chat window of 2 to 9 neither linked nor starting, by this window's own start or a hand-over's.
fn free_chat_window(w: &Window, reads: &dyn HostReads) -> Option<WindowName> {
    (2..=9u8).map(WindowName::Chat).find(|name| {
        w.peer_named(name).is_none() && !w.pending_runs.contains_key(name) && !reads.module_window_open(name)
    })
}

/// Starts the module window `name`, which then counts as starting until its peer attaches.
fn spawn(w: &mut Window, win: WinId, name: WindowName, token: Option<String>, keep_keys: bool, batch: &mut Batch) {
    w.say(format!("[window] {} is started", name.text()));
    w.pending_runs.entry(name.clone()).or_default();
    batch.push_effect(HostOp::OpenModuleWindow {
        win,
        name,
        token,
        keep_keys,
    });
}

/// A chat key pressed in window 1 or a page window: it runs in the chat window that last held the keyboard, or in
/// `c1`; with neither there, `c1` is started and the action waits for its link.
fn to_last_chat(w: &mut Window, win: WinId, action: &Action, asked: &Asked, batch: &mut Batch) {
    let last = w.last_chat.filter(|peer| w.peer(*peer).is_some());
    let target = last.or_else(|| w.peer_named(&WindowName::Chat(1)));
    match target {
        Some(peer) => {
            batch.push_effect(HostOp::PeerRun {
                win,
                peer,
                name: action.name(),
                opts: action.wire_options(),
            });
            match asked {
                Asked::ByKey => moves::raise_for(w, win, peer, FocusTarget::Window, batch),
                Asked::ByWindow { token } => moves::forward_raise(w, win, peer, token.clone(), batch),
            };
            w.say(format!("[keys] {}: runs in the last chat window", action.name()));
        }
        None => {
            let name = WindowName::Chat(1);
            let starting = w.pending_runs.contains_key(&name);
            w.pending_runs
                .entry(name.clone())
                .or_default()
                .push((action.name(), action.wire_options()));
            w.say(format!("[keys] {}: waits for the base chat window", action.name()));
            if !starting {
                spawn(w, win, name, asked.token(), false, batch);
            }
        }
    }
}

/// `tab.break` in a chat window: the active tab leaves for another chat window, or goes back to `c1` from one that
/// is not `c1`. Window 1 does the move; this asks, once the tab's turn has ended, since the session changes process.
/// A second press while it waits calls it off.
fn tab_break(w: &mut Window, reads: &dyn HostReads, win: WinId, batch: &mut Batch) {
    if w.tab_break_wait.take().is_some() {
        w.say(format!("[keys] tab.break: {}", feedback::TAB_BREAK_CANCELLED_TEXT));
        chrome::show_toast(w, feedback::TAB_BREAK_CANCELLED_TEXT.to_owned(), ToastKind::Other);
        return;
    }
    let Some(tab) = reads.active_base_tab(win) else {
        ask_for_session(w, win, None, batch);
        return;
    };
    match reads.tab_hand_over(win, tab) {
        Err(why) => {
            w.say(format!("[keys] tab.break: {why}"));
            w.refuse(win, why, batch);
        }
        Ok(facts) if facts.running => wait_for_turn(w, tab),
        Ok(facts) => ask_for_session(w, win, named(facts), batch),
    }
}

/// The agent and session that name a tab in a hand-over; a tab with no session has neither.
fn named(facts: TabHandOver) -> Option<(AgentId, String)> {
    facts.session.map(|session| (facts.agent, session))
}

/// The tab's turn runs: the break goes when it ends.
fn wait_for_turn(w: &mut Window, tab: TabId) {
    w.tab_break_wait = Some(tab);
    w.say(format!("[keys] tab.break: {}", feedback::TAB_BREAK_WAITS_TEXT));
    chrome::show_toast(w, feedback::TAB_BREAK_WAITS_TEXT.to_owned(), ToastKind::Other);
}

/// Running work changed in this window: a `tab.break` that waited goes once its own tab's turn is over, whatever
/// the window's other tabs are doing. A tab that can no longer go says why and the wait ends.
pub fn turn_may_have_ended(w: &mut Window, reads: &dyn HostReads, win: WinId, batch: &mut Batch) {
    let Some(tab) = w.tab_break_wait else {
        return;
    };
    match reads.tab_hand_over(win, tab) {
        Ok(facts) if facts.running => {}
        Ok(facts) => {
            w.tab_break_wait = None;
            ask_for_session(w, win, named(facts), batch);
        }
        Err(why) => {
            w.tab_break_wait = None;
            w.say(format!("[keys] tab.break: {why}"));
            chrome::show_toast(w, why, ToastKind::Other);
        }
    }
}

/// Asks window 1 to hand over the tab holding `session` of `agent`, as session ids are each agent's own. A tab with
/// none has nothing to resume elsewhere and stays: the next free chat window opens with a tab of its own instead.
fn ask_for_session(w: &mut Window, win: WinId, session: Option<(AgentId, String)>, batch: &mut Batch) {
    let Some((agent, session)) = session else {
        w.say("[keys] tab.break: an empty tab; asks window 1 for a new chat window".into());
        batch.push_effect(HostOp::HubAsk {
            win,
            ask: HubAsk::InstanceNew,
        });
        return;
    };
    let back = w.module_name != Some(WindowName::Chat(1));
    w.say("[keys] tab.break: asks window 1".into());
    batch.push_effect(HostOp::HubAsk {
        win,
        ask: HubAsk::TabBreak { agent, session, back },
    });
}

/// A tab this window offered stays here, and `why` says so.
pub fn tab_kept(w: &mut Window, why: String) {
    w.say(format!("[window] the tab stays here: {why}"));
    chrome::show_toast(w, why, ToastKind::Other);
}

/// A tab left this window: it is open in its new window, or its last was closed here. A chat window other than `c1`
/// that holds nothing now closes, as an emptied extra chat view goes; `c1` keeps its empty view, as the base view is
/// never away.
pub fn tab_left(w: &mut Window, cx: &Cx, win: WinId, batch: &mut Batch) {
    if !matches!(w.module_name, Some(WindowName::Chat(n)) if n != 1) {
        return;
    }
    let holds_nothing = cx.reads.base_tabs(win).iter().all(|tab| {
        cx.reads
            .tab_hand_over(win, *tab)
            .is_ok_and(|facts| facts.session.is_none() && !facts.running)
    });
    if holds_nothing {
        w.say("[window] its last tab has left; this chat window closes".into());
        close::request(w, cx, win, close::CloseCause::User, batch);
    }
}

/// Window 1 names the window the tab of `agent`'s `session` goes to and hands it over: the next free chat window, or
/// `c1` for `back`. Not marked as starting: the hand-over starts it, and a refused second hand-over must leave no mark.
fn hand_over_tab(
    w: &mut Window,
    reads: &dyn HostReads,
    win: WinId,
    from: PeerId,
    (agent, session): (AgentId, String),
    back: bool,
    token: Option<String>,
    batch: &mut Batch,
) {
    let to = if back {
        Some(WindowName::Chat(1))
    } else {
        free_chat_window(w, reads)
    };
    let Some(to) = to else {
        w.say(format!("[window] {}", feedback::ALL_CHAT_WINDOWS_TEXT));
        batch.push_effect(HostOp::KeepTab {
            win,
            peer: from,
            session,
            why: feedback::ALL_CHAT_WINDOWS_TEXT.to_owned(),
        });
        return;
    };
    w.say(format!("[window] a tab moves to {}", to.text()));
    batch.push_effect(HostOp::HandOverTab {
        win,
        from,
        agent,
        session,
        to,
        token,
    });
}

/// The module window `peer` asked window 1 for something. `Some(action)` is a key action window 1 runs itself (a
/// hidden terminal asked for by a chat window), after which it presents its own window with the asker's token.
pub fn hub_ask(
    w: &mut Window,
    reads: &dyn HostReads,
    win: WinId,
    peer: PeerId,
    ask: HubAsk,
    token: Option<String>,
    batch: &mut Batch,
) -> Option<Action> {
    let asked = Asked::ByWindow { token };
    match ask {
        HubAsk::Module { id } => {
            let Ok(id) = ModuleId::parse(&id) else {
                w.warn(format!("[window] a module window asked for {id:?}, which is no module"));
                return None;
            };
            match id.kind() {
                ModuleKind::Agent | ModuleKind::LuaWebview => {
                    module_key(w, win, &id, &asked, batch);
                    None
                }
                // The key was pressed in another window, so window 1 has nothing to toggle: which module held its keys
                // when it lost the focus says nothing about this press. A terminal on screen only takes them.
                ModuleKind::Terminal if w.layout.try_borrow().is_ok_and(|layout| layout.is_shown(&id)) => {
                    focus::give_keys(w, win, &id, vec![], batch);
                    batch.push_effect(HostOp::Present {
                        win,
                        activation: asked.token().map(ActivationToken),
                    });
                    None
                }
                ModuleKind::Terminal => Some(Action::Module(id)),
                ModuleKind::Editor | ModuleKind::Canvas => {
                    w.say(format!("[window] {id}: a module window does not ask for it"));
                    None
                }
            }
        }
        HubAsk::InstanceNew => {
            next_chat_window(w, reads, win, &asked, batch);
            None
        }
        HubAsk::TabBreak { agent, session, back } => {
            hand_over_tab(w, reads, win, peer, (agent, session), back, asked.token(), batch);
            None
        }
        HubAsk::ChatKey { name, opts } => {
            match chat_key(&name, &opts) {
                Some(action) => to_last_chat(w, win, &action, &asked, batch),
                None => w.say(format!(
                    "[window] a page window asked to run {name:?}, which is no chat key"
                )),
            }
            None
        }
    }
}

/// The chat key a page window named, read as a key's binding is. No other action is taken from a window's ask.
fn chat_key(name: &str, opts: &[(String, serde_json::Value)]) -> Option<Action> {
    let opts: Vec<(String, OptValue)> = opts
        .iter()
        .map(|(key, value)| (key.clone(), OptValue::from_wire(value)))
        .collect();
    let action = parse(name, &opts, &[]).ok()?.action;
    matches!(classify_window1(&action), HubVerb::ToLastChat).then_some(action)
}

/// The module window `peer` took the keyboard: a chat window is the one a chat key in window 1 goes to.
pub fn window_focused(w: &mut Window, peer: PeerId) {
    if w.peer(peer).is_some_and(|p| matches!(p.name(), WindowName::Chat(_))) {
        w.last_chat = Some(peer);
    }
}

/// Window 1 starts the actions that waited for the module window `name`, now linked as `peer`.
pub fn linked(w: &mut Window, win: WinId, name: &WindowName, peer: PeerId, batch: &mut Batch) {
    for (action, opts) in w.pending_runs.remove(name).unwrap_or_default() {
        w.say(format!("[keys] {action}: runs in {}, which has linked", name.text()));
        batch.push_effect(HostOp::PeerRun {
            win,
            peer,
            name: action,
            opts,
        });
    }
}

/// The module windows that were starting did not come up: what waited for them is dropped, with a line each.
pub fn start_failed(w: &mut Window, only: Option<&WindowName>, why: &str) -> bool {
    let names: Vec<WindowName> = w
        .pending_runs
        .keys()
        .filter(|name| only.is_none_or(|only| only == *name) && w.peer_named(name).is_none())
        .cloned()
        .collect();
    for name in &names {
        let dropped = w.pending_runs.remove(name).unwrap_or_default().len();
        w.say(format!(
            "[window] {} did not start ({why}); {dropped} waiting action(s) dropped",
            name.text()
        ));
    }
    !names.is_empty()
}

#[cfg(test)]
mod tests;
