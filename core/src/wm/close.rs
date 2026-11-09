//! Close prompt rules, kill decisions and nvim quit facts.
//!
//! `prefix x` is tmux's `kill-pane`: it ends the module, it does not hide it. Stock tmux binds `x` to
//! `confirm-before -p "kill-pane #P? (y/n)" kill-pane` (checked against a throwaway
//! `tmux -L xprobe -f /dev/null` server, tmux next-3.7), so Eitri asks first too, in the window's one
//! y/n (the chrome's [`Prompt`]), naming the module where tmux names the pane's number.
//!
//! What a kill ends, per module:
//! - **terminal**: its shell is hung up (SIGHUP, as closing a terminal does) and the module goes back
//!   where a first launch puts it, hidden; `prefix t` starts a fresh shell there.
//! - **a Lua panel**: its web process is terminated and the module goes back where its `position`
//!   puts it, hidden; its key loads the page afresh.
//! - **agent**: every base-view tab is closed as `prefix &` closes one (records stay resumable), and
//!   the module is hidden in place; `prefix a` opens the chat with the session chooser. A tab shown in
//!   another chat view keeps running there.
//! - **a chat view `agent#N`**: its one tab is closed the same way and the view leaves every
//!   workspace; its page waits idle under its id for the next view of that number.
//! - **editor**: nvim is asked to `:confirm qall`, so unsaved buffers get nvim's own prompt; only if
//!   nvim quits does the module go. It does not come back in this window (`crate::layout::kill`'s
//!   module doc: the shell starts no second nvim in it, though the pane could), and the prompt says
//!   so. Cancelled in nvim, nothing closes.
//!
//! - **the last module on screen**: tmux closes the window when its last pane is killed, and ends
//!   when its last window is, so `x` here closes Eitri ([`KillScope::Window`]) -- a kill that did
//!   nothing on the last module would leave the key dead exactly where it is most likely pressed. One
//!   question: it names what the window close would have asked about (`N running, M queued`), and
//!   `y` is taken as the answer to both. The editor still goes through `:confirm qall` first, and the
//!   window closes when nvim exits. The `y` reads the layout again: if another module came on screen
//!   meanwhile, it kills only the module it named, as any kill. A window close arriving while the
//!   question is up (nvim exiting on its own, the top bar's) takes the question's place.
//!
//! - **every window close** never discards unsaved editor changes silently: the top bar's close, the
//!   compositor's, the last module's kill, the terminal's `exit` as the last module -- each asks nvim
//!   to `:confirm qall` exactly as `prefix x` on the editor does ([`EditorQuit::Window`]), and the
//!   window closes only when nvim exits. A plain `:qa!` would discard unsaved buffers AND delete
//!   their swap files. The precedent is stock Neovide's own `g:neovide_confirm_quit`, on by default
//!   (`src/window/settings.rs` in the fork), which makes its window close run `confirm qa` -- and,
//!   like Neovide's, the quit is an RPC request (`nvim_exec_lua`), never typed keys, which a key nvim
//!   is waiting for (after `f`) would swallow (`crate::layout::kill::editor_quit_lua`). An editor
//!   that is not on screen -- hidden, or zoomed away -- is brought on screen for the prompt
//!   ([`reveal_for_prompt`]); one that was hidden is hidden again if nvim quits, so the saved
//!   arrangement stays the user's. A second request while nvim is asking is refused, as a second `x`
//!   is, with what nvim is doing ([`NvimState`]); only when nvim's own loop has not answered for
//!   [`NVIM_QUIT_PATIENCE`] does it kill nvim instead ([`quit_request`]).
//! - **a group**, in `compositor` mode: window 1's close closes every chat and page window with it, so it asks whatever
//!   they are doing and counts what they run ([`crate::tabs::group_close_prompt`]), and its teardown ends them first
//!   ([`CleanupOp::EndGroup`]). A module window is its one module, so its kill and its close are one thing: the close,
//!   which asks only while a turn runs. One that hears its group closing ([`CloseCause::GroupClosed`]) tears down
//!   without asking: the user answered in window 1.
//! - **a declined window close after nvim exited on its own**: nvim is gone and the window stays, so
//!   the editor is retired as `prefix x`'s kill of it retires it ([`Reopen::Never`]) -- never left on
//!   screen holding the keys with nothing behind it ([`retire_on_declined_close`]). It is not
//!   restarted in this window (as above); `prefix e` then says so.
//!
//! - **a person's own nvim** ([`EditorKind::Attached`](super::window::EditorKind), taken over from its terminal):
//!   nothing in the window may quit, end or restart it -- [`EditorCall::Quit`], `End` and `Restart` cannot be built
//!   without an [`OwnedEditor`], which [`owned`] gives no attached editor -- and every close lets go of it
//!   ([`EditorCall::Detach`]) so it goes back to its terminal. A close that can ask first asks the editor what it has
//!   to ask about ([`EditorCall::AskCloseFacts`], at most [`CLOSE_FACTS_WITHIN`]; no answer counts as nvim waiting
//!   for a key), then asks about the drafts and proposals that end with the window, then about a waiting nvim, then
//!   lets go, and only then asks about running turns, so the terminal never waits on that question. The window is torn
//!   down only once the editor says it let go ([`EditorEvent::Detached`], at most [`DETACHED_WITHIN`]); an answer to
//!   an earlier close's ask for the editor's facts is never taken for the current one's (its `generation`).
//!   `prefix x` on the editor asks whether to give nvim back and close, then closes. A hand-back
//!   ([`CloseCause::HandedBack`]) lets go at once and asks only about running turns; it and a signal run whatever the
//!   close is doing ([`ALWAYS_RUN`]). A `n` to the turns' question after the let-go keeps the window for its turns and
//!   ends the editor in place, since it holds no nvim any more.
//!
//! This file holds the pieces decided without a display or a toolkit: the prompt's text and keys,
//! where each kind of module is left ([`reopen`]), and each single decision of the editor's quit,
//! read from what nvim answered ([`NvimAnswer`]). [`CloseFlow`] joins native closes, the last module's
//! kill and termination signals. [`QuitFlow`] guards the editor's buffers; its callbacks become
//! intents so completing a quit never re-enters a borrowed flow. The host carries out the resulting
//! ordered operations and posts the editor's answers.

use std::time::{Duration, Instant};

pub use crate::layout::KillScope;
use crate::layout::{declared_placement, Layout, ModuleDecl, ModuleId, ModuleKind, Placement, Reopen};

use super::KeyPress;
use crate::keymap::KeyName;

#[cfg(test)]
#[path = "close/tests.rs"]
mod flow_tests;

#[cfg(test)]
#[path = "close/attached_tests.rs"]
mod attached_tests;

use super::chrome::{self, Prompt};
use super::host::*;
use super::quit::{QuitFlow, QuitWorld, WindowClose};
use super::toasts::ToastKind;
use super::verbs::modules::{self, Cx};
use super::window::{Tiling, WinId, Window};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CloseState {
    #[default]
    Idle,
    Asking,
    Moving,
    Quitting,
    TearingDown,
    Closed,
}

#[derive(Default)]
pub struct CloseFlow {
    pub state: CloseState,
    teardown: Option<TimerToken>,
    /// An attached editor's close waits for what it has to ask, at most until this timer fires.
    facts: Option<TimerToken>,
    /// The generation of the last [`EditorCall::AskCloseFacts`]; only an answer carrying it is this close's.
    facts_generation: u32,
    /// The attached editor was let go of, or its nvim ended: there is nothing left to let go of.
    let_go: bool,
    /// The editor was told to let go of the nvim and has not said it did: the window is not torn down before it
    /// does, or before [`DETACHED_WITHIN`].
    detaching: bool,
    /// The address a close names when the nvim's terminal is gone ([`CloseAsks::no_way_back`]), from the editor's
    /// last answer.
    no_way_back: Option<String>,
    /// The editor followed a `:connect`: the nvim it shows came from no terminal, and the window's own nvim it left
    /// runs on with no window at [`CloseFlow::left_own`].
    connected: bool,
    /// Where the window's own nvim a followed `:connect` left runs on, as the follow reported it. A close that ends
    /// with the reached server already gone asks about it once, and every close's teardown writes it to the log.
    left_own: Option<String>,
    /// This close's `y` to the question naming where nvim runs on: the close does not ask it again.
    left_named: bool,
    pending: Option<PendingClose>,
    confirmed: bool,
    repeated: bool,
    moving: Vec<(super::window::PeerId, crate::window_link::moves::MoveId)>,
    move_pending: Vec<(ModuleId, Option<crate::tabs::TabId>)>,
    move_wait_noted: std::collections::BTreeSet<ModuleId>,
}

pub enum PendingClose {
    Window {
        confirmed: Option<String>,
    },
    LastModule {
        module: ModuleId,
        confirmed: Option<String>,
    },
    /// A question about a person's own nvim, asked before it is let go of.
    Attached(AttachedAsk),
}

/// The questions a close asks about a person's own nvim, in the order it asks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachedAsk {
    /// `prefix x` on the editor: give nvim back to its terminal and close the window.
    GiveBack,
    /// Drafts or proposals in the editor end with the window; `blocking` is asked after it.
    Unsaved { blocking: bool },
    /// nvim is waiting for a key, or busy, or did not say in time.
    Blocking,
    /// The terminal the nvim came from is gone: once let go of, it runs on with no window.
    NoWayBack,
}

/// How long an attached editor's close waits for what it has to ask; no answer counts as nvim waiting for a key.
pub const CLOSE_FACTS_WITHIN: Duration = Duration::from_millis(200);

#[derive(Debug, Clone)]
pub enum CloseCause {
    User,
    Compositor,
    NvimExited,
    LastTerminalExit(ModuleId),
    Signal(&'static str),
    /// Window 1 said it is closing the group this module window belongs to.
    GroupClosed,
    /// A person's own nvim went back to its terminal, or must go now: the window lets go of it at once and closes,
    /// asking only about running turns.
    HandedBack(HandBackWhy),
}

/// Why a person's own nvim goes back to its terminal without the window asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandBackWhy {
    /// Another UI attached to the nvim.
    OtherUi,
    /// Enter in the waiting room took it back.
    TakeBack,
    /// The window's link to the nvim closed.
    LinkLost,
    /// Another Eitri's glue replaced this window's in the nvim.
    Replaced,
    /// The attach failed after the terminal's UI had moved.
    AttachFailed,
    /// `:connect` typed in the window's editor moved its UI to another nvim; the one it left goes back to its terminal.
    Connected,
}

impl HandBackWhy {
    fn said(self) -> &'static str {
        match self {
            HandBackWhy::OtherUi => "another UI attached to it",
            HandBackWhy::TakeBack => "the terminal took it back",
            HandBackWhy::LinkLost => super::feedback::LINK_LOST_TEXT,
            HandBackWhy::Replaced => "another Eitri's glue replaced this window's",
            HandBackWhy::AttachFailed => "the attach failed",
            HandBackWhy::Connected => "`:connect` moved the editor to another nvim",
        }
    }
}

fn is_signal(cause: &CloseCause) -> bool {
    matches!(cause, CloseCause::Signal(_))
}

fn is_hand_back(cause: &CloseCause) -> bool {
    matches!(cause, CloseCause::HandedBack(_))
}

/// The causes no refusal may stop, whatever the close is doing: a signal cannot wait for an answer, and a person's
/// own nvim goes back to them without waiting for one.
pub const ALWAYS_RUN: &[fn(&CloseCause) -> bool] = &[is_signal, is_hand_back];

/// Whether `cause` is one of [`ALWAYS_RUN`].
pub fn always_runs(cause: &CloseCause) -> bool {
    ALWAYS_RUN.iter().any(|runs| runs(cause))
}

/// A signal never waits for an answer. Ordinary closes let nvim guard unsaved buffers before an idle tears down.
pub fn request(w: &mut Window, cx: &Cx, win: WinId, cause: CloseCause, batch: &mut Batch) {
    // Not even for the moves `m` started: the teardown's goodbye ends the link, which settles them.
    if matches!(cause, CloseCause::Signal(_)) && w.close.state == CloseState::Moving {
        w.say("[window] an end signal closes without waiting for the moves".into());
        stop_moving(w);
    }
    if let CloseCause::HandedBack(why) = cause {
        if !w.editor_attached() {
            w.warn(format!(
                "[window] BUG: a hand-back ({}) for an nvim this window started; ignored",
                why.said()
            ));
            return;
        }
        hand_back(w, win, why, batch);
    }
    if ended(w) {
        return;
    }
    let beside = w.is_beside();
    // The signal's own handler has said so, through a write that never blocks; a group's close was asked in window 1.
    if matches!(cause, CloseCause::Signal(_) | CloseCause::GroupClosed) {
        // A group's window keeps its place for the group's next start; a window beside nvim is in no group.
        if matches!(cause, CloseCause::Signal(_)) && w.tiling == Tiling::ModuleWindow && !beside {
            batch.push_effect(HostOp::Cleanup(CleanupOp::KeepThisWindow));
        }
        teardown(w, win, batch);
        return;
    }
    // A window beside nvim holds no tab and no turn, and the nvim it shows is the person's own, which its close must
    // never ask to quit: it tears down from an idle, as a close nothing holds does, with no question.
    if beside {
        if w.close.teardown.is_none() {
            w.close.state = CloseState::Quitting;
            w.close.teardown = Some(w.arm_idle());
        }
        return;
    }
    // Whatever it answers; a `y` has been counted already, as the ask it answers.
    if w.tiling == Tiling::Hub && matches!(cause, CloseCause::User | CloseCause::Compositor) && !w.close.confirmed {
        batch.push_effect(HostOp::Cleanup(CleanupOp::GroupAsked));
    }
    if w.editor_attached() {
        attached_close(w, cx, win, cause, batch);
        return;
    }
    if w.close.teardown.is_some() {
        return;
    }
    if w.close.state == CloseState::Asking {
        // The last module's question is only about a kill; a close that arrives meanwhile (nvim exiting on its own,
        // the top bar's close) is the window's own and takes its place, or nothing would answer it: a `no` to the
        // kill's question would leave an exited editor on screen holding the keys.
        if !matches!(w.close.pending, Some(PendingClose::LastModule { .. })) {
            return;
        }
        w.say("[window] a window close replaces the last module's kill question".into());
        w.close.pending = None;
        chrome::set_prompt(w, None);
    }
    w.close.state = CloseState::Quitting;
    let now = w.now();
    let proceed = with_quit(w, cx, win, batch, |quit, world| quit.window_close(world, now));
    if proceed == WindowClose::Proceed && !ended(w) && w.close.teardown.is_none() {
        w.close.teardown = Some(w.arm_idle());
    }
}

/// Lets go of a person's own nvim now, whatever the close is doing: the wait for what it had to ask and any question
/// about it go, since the nvim they were about is going back.
fn hand_back(w: &mut Window, win: WinId, why: HandBackWhy, batch: &mut Batch) {
    w.say(format!("[window] nvim goes back to its terminal: {}", why.said()));
    if why == HandBackWhy::LinkLost {
        chrome::show_toast(w, super::feedback::LINK_LOST_TEXT.into(), ToastKind::Warning);
    }
    // The teardown has let go of it already.
    if matches!(w.close.state, CloseState::TearingDown | CloseState::Closed) {
        return;
    }
    stop_asking_about_the_editor(w);
    let_go(w, win, batch);
}

/// The wait for an attached editor's facts and any question about the editor end; the close is left idle unless it
/// is moving modules away or tearing down.
fn stop_asking_about_the_editor(w: &mut Window) {
    if let Some(token) = w.close.facts.take() {
        w.cancel_timer(token);
        if w.close.state == CloseState::Quitting && w.close.teardown.is_none() {
            w.close.state = CloseState::Idle;
        }
    }
    if w.close.state == CloseState::Asking && matches!(w.close.pending, Some(PendingClose::Attached(_))) {
        w.close.pending = None;
        w.close.state = CloseState::Idle;
        chrome::set_prompt(w, None);
    }
}

/// The one `Detach` of a close: once let go of, the nvim is not the window's to let go of again.
fn let_go(w: &mut Window, win: WinId, batch: &mut Batch) {
    if std::mem::replace(&mut w.close.let_go, true) {
        return;
    }
    w.say("[window] letting go of nvim".into());
    w.close.detaching = true;
    // The nvim is no longer the window's from here: an editor kept on screen only ends in place.
    if matches!(
        w.editor_run,
        super::window::EditorRun::Running | super::window::EditorRun::Restarting
    ) {
        w.editor_run = super::window::EditorRun::Ending;
    }
    batch.push_effect(HostOp::Editor(EditorCall::Detach { win }));
}

/// The editor said it let go of the nvim: a teardown that waited for that runs from the next idle.
fn detach_answered(w: &mut Window) {
    if !std::mem::take(&mut w.close.detaching) || w.close.state != CloseState::Quitting {
        return;
    }
    if let Some(token) = w.close.teardown.take() {
        w.cancel_timer(token);
        w.close.teardown = Some(w.arm_idle());
    }
}

/// A `n` to the turns' question after the nvim was let go of keeps the window for its turns. Its editor holds no nvim
/// any more and cannot have one back, so it ends in place, as an editor whose nvim exited does, instead of holding
/// the keys with nothing behind it.
fn retire_let_go_editor(w: &mut Window, cx: &Cx, win: WinId, batch: &mut Batch) {
    if !w.close.let_go {
        return;
    }
    with_quit(w, cx, win, batch, |quit, world| quit.let_go_declined(world));
}

/// A close of a window whose editor shows a person's own nvim: what the editor has to ask is asked for, then the
/// questions about it, then it is let go of, and only then the question about running turns.
fn attached_close(w: &mut Window, cx: &Cx, win: WinId, cause: CloseCause, batch: &mut Batch) {
    if matches!(cause, CloseCause::NvimExited) {
        // The nvim ended: there is nothing to ask about it or to give back.
        stop_asking_about_the_editor(w);
        w.close.let_go = true;
    }
    if w.close.teardown.is_some() || w.close.facts.is_some() {
        return;
    }
    if w.close.state == CloseState::Asking {
        if !matches!(w.close.pending, Some(PendingClose::LastModule { .. })) {
            return;
        }
        w.say("[window] a window close replaces the last module's kill question".into());
        w.close.pending = None;
        w.close.state = CloseState::Idle;
        chrome::set_prompt(w, None);
    }
    if w.close.let_go {
        // The server a `:connect` reached has ended, or was let go of before: the window's own nvim it left still
        // runs with no window, and nothing else would name it before the window goes.
        if !w.close.left_named {
            if let Some(addr) = w.close.left_own.clone() {
                ask_attached(w, AttachedAsk::NoWayBack, super::feedback::no_way_back_prompt(&addr));
                return;
            }
        }
        after_let_go(w, cx, win);
        return;
    }
    w.close.state = CloseState::Quitting;
    w.close.facts = Some(w.arm_after(CLOSE_FACTS_WITHIN));
    w.close.facts_generation = w.close.facts_generation.wrapping_add(1);
    let generation = w.close.facts_generation;
    batch.push_effect(HostOp::Editor(EditorCall::AskCloseFacts { win, generation }));
}

/// The attached editor's facts with the generation of the ask they answer, or `None` when none came in time, which
/// counts as nvim waiting for a key. A late answer, or one no close is waiting for, changes nothing: an answer to an
/// earlier close's ask may have been taken before something changed.
pub(super) fn close_facts(w: &mut Window, cx: &Cx, win: WinId, answer: Option<(u32, CloseAsks)>, batch: &mut Batch) {
    if let Some((generation, _)) = answer {
        if w.close.facts.is_none() || generation != w.close.facts_generation {
            w.say(format!(
                "[window] the editor's answer to an earlier close's ask ({generation}) came late; ignored"
            ));
            return;
        }
    }
    let asks = answer.map(|(_, asks)| asks);
    let Some(token) = w.close.facts.take() else {
        return;
    };
    if asks.is_some() {
        w.cancel_timer(token);
    } else {
        w.say("[window] the editor did not say in time whether nvim waits for a key; taken as waiting".into());
    }
    let asks = asks.unwrap_or(CloseAsks {
        blocking: true,
        no_way_back: w.close.no_way_back.clone(),
        ..CloseAsks::default()
    });
    w.close.state = CloseState::Idle;
    w.close.no_way_back = asks.no_way_back.clone();
    if asks.drafts > 0 || asks.proposals > 0 {
        let text = super::feedback::unsaved_in_editor_prompt(asks.drafts, asks.proposals);
        ask_attached(
            w,
            AttachedAsk::Unsaved {
                blocking: asks.blocking,
            },
            text,
        );
    } else {
        ask_next_or_let_go(w, cx, win, asks.blocking, batch);
    }
}

/// After the drafts and proposals: whether the terminal is gone, else whether nvim waits for a key, else the let-go.
/// With no terminal to go back to, nothing would show the blank screen a waiting nvim leaves, so only the first is
/// asked.
fn ask_next_or_let_go(w: &mut Window, cx: &Cx, win: WinId, blocking: bool, batch: &mut Batch) {
    if let Some(addr) = w.close.no_way_back.clone() {
        ask_attached(w, AttachedAsk::NoWayBack, super::feedback::no_way_back_prompt(&addr));
    } else if blocking {
        ask_attached(w, AttachedAsk::Blocking, super::feedback::BLOCKING_PROMPT.into());
    } else {
        let_go(w, win, batch);
        after_let_go(w, cx, win);
    }
}

/// `prefix x` on an attached editor: whether to give nvim back to its terminal and close the window.
pub(super) fn ask_give_back(w: &mut Window, cx: &Cx, win: WinId, batch: &mut Batch) {
    if ended(w) {
        return;
    }
    superseded(w, cx, win, batch);
    if w.close.state != CloseState::Idle {
        w.say("[modules] editor: not asked; the window is closing".into());
        return;
    }
    // Nothing is left to give back: killing the editor is the window's close.
    if w.close.let_go {
        w.say("[modules] editor: its nvim was let go of already; the window closes".into());
        request(w, cx, win, CloseCause::User, batch);
        return;
    }
    // An nvim a `:connect` reached came from no terminal, and the editor cannot show another: the window closes.
    let text = if w.close.connected {
        super::feedback::CONNECTED_CLOSE_PROMPT
    } else {
        super::feedback::GIVE_BACK_PROMPT
    };
    ask_attached(w, AttachedAsk::GiveBack, text.into());
}

fn ask_attached(w: &mut Window, ask: AttachedAsk, text: String) {
    w.say(format!("[window] {text}"));
    w.modules.clear_question();
    w.close.pending = Some(PendingClose::Attached(ask));
    w.close.state = CloseState::Asking;
    chrome::set_prompt(
        w,
        Some(Prompt {
            text,
            closes_window: true,
            offers_move: false,
        }),
    );
}

/// A `y` to a question about a person's own nvim: the next question, or the close goes on.
fn attached_answered(w: &mut Window, cx: &Cx, win: WinId, ask: AttachedAsk, batch: &mut Batch) {
    match ask {
        AttachedAsk::GiveBack => request(w, cx, win, CloseCause::User, batch),
        AttachedAsk::Unsaved { blocking } => ask_next_or_let_go(w, cx, win, blocking, batch),
        AttachedAsk::Blocking | AttachedAsk::NoWayBack => {
            if ask == AttachedAsk::NoWayBack {
                w.close.left_named = true;
            }
            let_go(w, win, batch);
            after_let_go(w, cx, win);
        }
    }
}

/// The nvim has been let go of: the question about running turns, if any, then the teardown from an idle.
fn after_let_go(w: &mut Window, cx: &Cx, win: WinId) {
    if !w.close.confirmed {
        if let Some(text) = window_prompt(w, cx, win) {
            ask_window(w, cx, win, text);
            return;
        }
    }
    w.close.state = CloseState::Quitting;
    if w.close.teardown.is_none() {
        w.close.teardown = Some(if w.close.detaching {
            w.arm_after(DETACHED_WITHIN)
        } else {
            w.arm_idle()
        });
    }
}

fn ended(w: &mut Window) -> bool {
    if !matches!(
        w.close.state,
        CloseState::Moving | CloseState::TearingDown | CloseState::Closed
    ) {
        return false;
    }
    if !w.close.repeated {
        w.close.repeated = true;
        w.say("[window] the window is already closing".into());
    }
    true
}

/// The last module's one question also confirms the window consequences at this moment.
pub(super) fn ask_last_module(w: &mut Window, cx: &Cx, win: WinId, module: ModuleId, batch: &mut Batch) {
    if ended(w) {
        return;
    }
    // A module window is its one module, so killing it is closing it, with the one question that close has.
    if w.tiling == Tiling::ModuleWindow {
        request(w, cx, win, CloseCause::User, batch);
        return;
    }
    let (running, queued, background, _) = window_counts(cx);
    let text = window_prompt(w, cx, win).filter(|_| w.has_peer()).unwrap_or_else(|| {
        prompt(
            &module,
            &chrome::title(w, &module),
            KillScope::Window,
            running,
            queued,
            cx.reads.terminal_jobs(win),
            background,
        )
    });
    let confirmed = window_prompt(w, cx, win);
    w.say(format!("[modules] {module}: {text}"));
    w.modules.clear_question();
    w.close.pending = Some(PendingClose::LastModule { module, confirmed });
    w.close.state = CloseState::Asking;
    chrome::set_prompt(
        w,
        Some(Prompt {
            text,
            closes_window: true,
            offers_move: move_target(w) && !movable(cx, win).is_empty(),
        }),
    );
}

impl CloseFlow {
    /// Whether this window is on its way out. A close that nvim was asked about and has not answered is held at nvim's
    /// prompt, and one that nvim turned down or never took stays at [`CloseState::Quitting`] with the window still
    /// here, so neither counts.
    pub fn is_ending(&self, quit: &QuitFlow) -> bool {
        match self.state {
            CloseState::Idle | CloseState::Asking => false,
            CloseState::Quitting => self.teardown.is_some() || quit.ends_the_window(),
            CloseState::Moving | CloseState::TearingDown | CloseState::Closed => true,
        }
    }

    /// The pending action is taken before it runs, so a callback may ask a fresh question in the same batch.
    pub fn answer(&mut self, w: &mut Window, cx: &Cx, win: WinId, answer: PromptKey, batch: &mut Batch) {
        if answer == PromptKey::Ignore || self.state != CloseState::Asking {
            return;
        }
        let answer = if answer == PromptKey::Move && !w.chrome.prompt.as_ref().is_some_and(|p| p.offers_move) {
            PromptKey::No
        } else {
            answer
        };
        let pending = self.pending.take();
        let closes_window = w.chrome.prompt.as_ref().is_some_and(|prompt| prompt.closes_window);
        chrome::set_prompt(w, None);
        self.state = CloseState::Idle;
        // Quit callbacks must see the live close flow, including any new prompt they open.
        std::mem::swap(self, &mut w.close);
        match (answer, pending) {
            (PromptKey::Move, Some(_)) => begin_moves(w, cx, win, batch),
            (PromptKey::Yes, Some(PendingClose::Window { .. })) => {
                w.close.confirmed = true;
                request(w, cx, win, CloseCause::User, batch);
            }
            (PromptKey::Yes, Some(PendingClose::LastModule { module, confirmed })) => {
                last_module_confirmed(w, cx, win, module, confirmed, batch);
            }
            (PromptKey::Yes, Some(PendingClose::Attached(ask))) => attached_answered(w, cx, win, ask, batch),
            // A `y` given earlier in this close (the last module's) confirmed this close only.
            (_, Some(PendingClose::Attached(_))) => {
                w.close.confirmed = false;
                w.say("[window] the close was declined; nvim stays in the editor".into());
            }
            // The last module's question never was the window close's own: declining it leaves the editor as it is.
            (_, Some(PendingClose::Window { .. })) if runs_decline_hook(answer, closes_window) => {
                if w.editor_attached() {
                    retire_let_go_editor(w, cx, win, batch);
                } else {
                    with_quit(w, cx, win, batch, |quit, world| quit.window_close_declined(world));
                }
            }
            _ => {}
        }
        // A close that did not go on asks again, next time, where nvim runs on.
        if answer != PromptKey::Yes {
            w.close.left_named = false;
        }
        std::mem::swap(self, &mut w.close);
    }
}

/// Something may have come on screen since the last module's question (a chip, a Lua show): the `y` then kills only
/// what it asked about, as any other kill, and the window stays with what is now shown.
fn last_module_confirmed(
    w: &mut Window,
    cx: &Cx,
    win: WinId,
    module: ModuleId,
    confirmed: Option<String>,
    batch: &mut Batch,
) {
    let scope = w
        .layout
        .try_borrow()
        .map(|layout| crate::layout::can_kill(&layout, &module));
    match scope {
        Ok(Ok(KillScope::Window)) => {}
        Ok(Ok(KillScope::Module)) => {
            w.say(format!(
                "[modules] {module}: no longer the last module on screen; killed as any other is"
            ));
            modules::kill_confirmed(w, cx, win, &module, batch);
            return;
        }
        Ok(Err(err)) => {
            w.say(format!("[modules] {module}: not killed: {err}"));
            w.refuse(win, String::new(), batch);
            return;
        }
        Err(_) => {
            w.warn(format!(
                "[wm] BUG: the layout was borrowed when {module}'s kill was answered"
            ));
            return;
        }
    }
    // A person's own nvim is let go of as any close lets go of it, the kill's question standing for the turns'.
    if w.editor_attached() {
        let now = window_prompt(w, cx, win);
        w.close.confirmed = close_is_confirmed(confirmed.as_deref(), now.as_deref());
        request(w, cx, win, CloseCause::User, batch);
        return;
    }
    w.close.state = CloseState::Quitting;
    with_quit(w, cx, win, batch, |quit, world| {
        if module == ModuleId::editor() {
            let now = world.w.now();
            if let Err(text) = quit.kill_editor(world, KillScope::Window, confirmed, now) {
                world.w.say(format!("[modules] editor: not killed: {text}"));
                world.feedback(super::feedback::CloseOutcome::KillRefused);
            }
        } else {
            quit.close_after(world, confirmed);
        }
    });
}

/// Another question takes the window close's place on the overlay. The close is left as a `no` to it leaves it, so
/// the next close asks again instead of waiting on a question nobody can answer any more.
pub(super) fn superseded(w: &mut Window, cx: &Cx, win: WinId, batch: &mut Batch) {
    if w.close.state != CloseState::Asking {
        return;
    }
    let pending = w.close.pending.take();
    w.close.state = CloseState::Idle;
    w.close.left_named = false;
    match pending {
        Some(PendingClose::Window { .. }) if w.editor_attached() => retire_let_go_editor(w, cx, win, batch),
        Some(PendingClose::Window { .. }) => {
            with_quit(w, cx, win, batch, |quit, world| quit.window_close_declined(world));
        }
        Some(PendingClose::Attached(_)) => w.close.confirmed = false,
        Some(PendingClose::LastModule { .. }) | None => {}
    }
}

/// A peer has gone, and the window's close question may have been about it. What the close would ask now replaces it,
/// and with nothing left to ask the close goes on: a question about windows that have gone would otherwise stay up for
/// good, holding whatever is waiting for this window to close, as a logout does.
pub(super) fn peer_left(w: &mut Window, cx: &Cx, win: WinId, batch: &mut Batch) {
    if w.close.state != CloseState::Asking || !matches!(w.close.pending, Some(PendingClose::Window { .. })) {
        return;
    }
    let Some(text) = window_prompt(w, cx, win) else {
        w.say("[window] what the close asked about has gone, so it goes on".into());
        w.close.pending = None;
        w.close.state = CloseState::Idle;
        chrome::set_prompt(w, None);
        request(w, cx, win, CloseCause::User, batch);
        return;
    };
    // The window went while window 1's close stood open: a logout closes again only every few seconds, and a window
    // that took longer to go than a burst lasts is no less part of it.
    if w.tiling == Tiling::Hub {
        batch.push_effect(HostOp::Cleanup(CleanupOp::GroupAsked));
    }
    if w.chrome.prompt.as_ref().is_none_or(|prompt| prompt.text != text) {
        ask_window(w, cx, win, text);
    }
}

/// Puts the window's close question up.
fn ask_window(w: &mut Window, cx: &Cx, win: WinId, text: String) {
    w.say(format!("[window] {text}"));
    let confirmed = Some(text.clone());
    w.modules.clear_question();
    w.close.pending = Some(PendingClose::Window { confirmed });
    w.close.state = CloseState::Asking;
    chrome::set_prompt(
        w,
        Some(Prompt {
            text,
            closes_window: true,
            offers_move: move_target(w) && !movable(cx, win).is_empty(),
        }),
    );
}

pub(super) fn answer(w: &mut Window, cx: &Cx, win: WinId, key: PromptKey, batch: &mut Batch) {
    let mut flow = std::mem::take(&mut w.close);
    flow.answer(w, cx, win, key, batch);
    w.close = flow;
}

fn movable(cx: &Cx, win: WinId) -> Vec<(ModuleId, Option<crate::tabs::TabId>)> {
    let mut modules = cx.reads.running_chats(win);
    modules.extend(cx.reads.terminal_job_modules(win).into_iter().map(|id| (id, None)));
    modules.extend(cx.reads.pages_with_tasks(win).into_iter().map(|id| (id, None)));
    let mut seen = std::collections::BTreeSet::new();
    modules.retain(|item| seen.insert(item.clone()));
    modules
}

fn begin_moves(w: &mut Window, cx: &Cx, win: WinId, batch: &mut Batch) {
    w.close.state = CloseState::Moving;
    w.close.confirmed = true;
    w.close.move_pending = movable(cx, win);
    w.close.moving = super::moves::active_source_moves(w);
    for (peer, id) in w.close.moving.clone() {
        super::moves::workspace::watch(w, peer, id);
    }
    moves_progress(w, cx, win, batch);
}

/// The close no longer waits on any move; the moves themselves go on until the link's end settles them.
fn stop_moving(w: &mut Window) {
    let ids = std::mem::take(&mut w.close.moving);
    super::moves::workspace::unwatch(w, &ids);
    w.close.move_pending.clear();
    w.close.state = CloseState::Idle;
}

/// Every verdict is known before nvim's quit or the idle teardown can start.
pub(super) fn moves_progress(w: &mut Window, cx: &Cx, win: WinId, batch: &mut Batch) {
    if w.close.state != CloseState::Moving {
        return;
    }
    let pending = std::mem::take(&mut w.close.move_pending);
    for (module, tab) in pending {
        let selected = match tab {
            Some(tab) => Some(cx.reads.tab_move(win, tab)),
            None if module.kind() == crate::layout::ModuleKind::Agent => Some(cx.reads.chat_move(
                win,
                crate::tabs::ViewId::of_module(&module).unwrap_or(crate::tabs::ViewId::BASE),
                None,
            )),
            None => None,
        };
        if selected
            .as_ref()
            .is_some_and(|f| matches!(f, Ok(f) if f.running && !f.live))
        {
            if w.close.move_wait_noted.insert(module.clone()) {
                super::moves::note_turn_wait(w, &module);
            }
            w.close.move_pending.push((module, tab));
            continue;
        }
        if let Some((peer, id)) = super::moves::offer_for_close(w, cx, win, &module, tab, selected, batch) {
            super::moves::workspace::watch(w, peer, id);
            w.close.moving.push((peer, id));
        }
    }
    if !w.close.move_pending.is_empty()
        || w.close
            .moving
            .iter()
            .any(|(peer, id)| !super::moves::move_decided(w, *peer, *id))
    {
        return;
    }
    stop_moving(w);
    request(w, cx, win, CloseCause::User, batch);
}

/// A dead link decides the remaining handovers, including chats waiting for a turn's end. Only the end of a link the
/// close waits on counts: one with a move in flight, or window 2's, which the waiting hand-overs are for.
pub(super) fn moves_link_ended(
    w: &mut Window,
    cx: &Cx,
    win: WinId,
    peer: super::window::PeerId,
    was_window_2: bool,
    batch: &mut Batch,
) {
    if w.close.state != CloseState::Moving {
        return;
    }
    if !was_window_2 && !w.close.moving.iter().any(|(moving_to, _)| *moving_to == peer) {
        return;
    }
    stop_moving(w);
    request(w, cx, win, CloseCause::User, batch);
}

/// An allocation's pin lengths are part of the launch baseline even before its deferred event is heard.
pub fn save_for_close(w: &mut Window) -> bool {
    if let Some(token) = w.save_timer.take() {
        w.cancel_timer(token);
    }
    let Some(saver) = w.saver.as_mut() else {
        w.unheard_settles.clear();
        return false;
    };
    for root in w.unheard_settles.drain(..) {
        saver.settled(&root);
    }
    saver.collect(w.workspaces.try_borrow_mut().ok().as_deref_mut());
    let finished = match (w.layout.try_borrow(), w.workspaces.try_borrow()) {
        (Ok(layout), Ok(workspaces)) => saver.save_now(&layout, &workspaces),
        _ => saver.save_now_unread(),
    };
    saver.collect(w.workspaces.try_borrow_mut().ok().as_deref_mut());
    let holds = match (w.layout.try_borrow(), w.workspaces.try_borrow()) {
        (Ok(layout), Ok(workspaces)) => saver.holds(&layout, &workspaces),
        _ => false,
    };
    finished && holds
}

/// Services end in dependency order; closing the native window is the final, non-asking operation.
pub fn teardown(w: &mut Window, win: WinId, batch: &mut Batch) {
    if ended(w) {
        return;
    }
    w.close.state = CloseState::TearingDown;
    if let Some(token) = w.close.teardown.take() {
        w.cancel_timer(token);
    }
    if let Some(token) = w.close.facts.take() {
        w.cancel_timer(token);
    }
    if let Some((token, _)) = w.quit_timer.take() {
        w.cancel_timer(token);
    }
    if let Some(token) = w.release_exited.take() {
        w.cancel_timer(token);
    }
    w.close.pending = None;
    w.modules.clear_question();
    chrome::set_prompt(w, None);
    // Whatever closed the window (a signal or a hand-back cannot ask), the log says where the window's own nvim a
    // `:connect` left runs on.
    if let Some(addr) = w.close.left_own.clone() {
        w.say(format!(
            "[window] {}",
            super::feedback::no_way_back_prompt(&addr).trim_end_matches(" Close? (y/n)")
        ));
    }
    let layout_written = save_for_close(w);
    super::moves::forget_sent_at_teardown(w, win, batch);
    // Window 2 outlives window 1; a row of orphaned chat windows after the editor was quit would be clutter.
    if w.tiling == Tiling::Hub {
        batch.push_effect(HostOp::Cleanup(CleanupOp::EndGroup));
    }
    // The editor's record waits for nvim's own last one, which it leaves in the channel directory as it ends: told
    // after nvim is, before that directory goes. A person's own nvim is let go of in its place, and keeps no record
    // of the window's.
    let (editor, record) = if w.editor_attached() {
        if w.close.detaching {
            w.say("[window] closing before the editor said it let go of nvim".into());
        }
        let detach = (!std::mem::replace(&mut w.close.let_go, true)).then_some(EditorCall::Detach { win });
        (detach, None)
    } else {
        (Some(EditorCall::Shutdown { win }), super::reopen::teardown_op(w))
    };
    for op in [HostOp::Cleanup(CleanupOp::PairOnClose { layout_written })]
        .into_iter()
        .chain(editor.map(HostOp::Editor))
        .chain(record)
        .chain([
            HostOp::Terminal(TermOp::ShutdownAll { win }),
            HostOp::Cleanup(CleanupOp::PaneSwitchDir),
            HostOp::Cleanup(CleanupOp::Feeds),
            HostOp::Plugin(PluginOp::ShutdownAll { win }),
            HostOp::Chat(ChatOp::ShutdownAll { win, hold: true }),
            HostOp::Cleanup(CleanupOp::ScratchDir),
            HostOp::CloseWindow { win },
        ])
    {
        batch.push_effect(op);
    }
    w.close.state = CloseState::Closed;
}

/// The last terminal stays in the layout while its window close waits for confirmation or nvim's unsaved buffers;
/// any other terminal can be removed as soon as its shell ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnExit {
    CloseModule,
    CloseWindow,
}

pub fn on_shell_exit(scope: KillScope) -> OnExit {
    match scope {
        KillScope::Module => OnExit::CloseModule,
        KillScope::Window => OnExit::CloseWindow,
    }
}

pub(super) fn timer(w: &mut Window, cx: &Cx, win: WinId, token: TimerToken, batch: &mut Batch) -> bool {
    if w.close.facts == Some(token) {
        close_facts(w, cx, win, None, batch);
        return true;
    }
    if w.close.teardown == Some(token) {
        w.close.teardown = None;
        teardown(w, win, batch);
        return true;
    }
    if w.release_exited == Some(token) {
        w.release_exited = None;
        batch.push_effect(HostOp::Editor(EditorCall::ReleaseExited { win }));
        return true;
    }
    if let Some((_, generation)) = w.quit_timer.filter(|(armed, _)| *armed == token) {
        w.quit_timer = None;
        let now = w.now();
        let again = with_quit(w, cx, win, batch, |quit, world| quit.poll(world, generation, now));
        if again {
            w.quit_timer = Some((w.arm_after(Duration::from_secs(1)), generation));
        }
        return true;
    }
    false
}

pub(super) fn editor_event(w: &mut Window, cx: &Cx, win: WinId, event: EditorEvent, batch: &mut Batch) {
    use super::window::{EditorRun, NvimSaid};
    match &event {
        EditorEvent::HandedBack { why, .. } => {
            request(w, cx, win, CloseCause::HandedBack(*why), batch);
            return;
        }
        EditorEvent::CloseFacts { generation, asks, .. } => {
            close_facts(w, cx, win, Some((*generation, asks.clone())), batch);
            return;
        }
        // The editor followed its nvim to another server: from now on the window holds that kind of nvim, and every
        // rule of it holds. A server a `:connect` reached is not the window's to quit, end or restart, and the
        // window's own nvim it left runs on with no window, which the close names.
        EditorEvent::Followed { kind, left, .. } => {
            if *kind == super::window::EditorKind::Attached {
                w.set_editor_kind(*kind);
                w.close.no_way_back = left.clone();
                w.close.connected = true;
                w.close.left_own = left.clone();
                w.say(match left {
                    Some(addr) => format!(
                        "[modules] editor: it follows a :connect to an nvim the window did not start; the window's own \
                         nvim runs on at {addr}"
                    ),
                    None => "[modules] editor: it follows a :connect to an nvim the window did not start".into(),
                });
            } else if w.editor_attached() {
                // Nothing makes an nvim the window did not start its own again.
                w.warn(
                    "[wm] BUG: a followed :restart was reported for an nvim this window did not start; ignored".into(),
                );
            } else {
                w.say("[modules] editor: it follows a :restart to the server its nvim started".into());
            }
            return;
        }
        // The pane holds no nvim any more: nothing can take a file, and nothing brings this one back here.
        EditorEvent::Detached { .. } if !w.editor_attached() => {
            w.warn("[wm] BUG: a detach was reported for an nvim this window started; ignored".into());
            return;
        }
        EditorEvent::Detached { .. } => {
            w.close.let_go = true;
            w.editor_ready = false;
            w.editor_lost = true;
            w.editor_run = EditorRun::Ended;
            w.say("[modules] editor: nvim was let go of".into());
            detach_answered(w);
            return;
        }
        // An exit reported after the nvim was let go of is the terminal's nvim, not this window's: it closes nothing,
        // and while the close waits for the detach's answer it is that answer.
        EditorEvent::Exited { .. } if w.editor_attached() && w.close.let_go => {
            w.editor_ready = false;
            w.editor_lost = true;
            w.editor_run = EditorRun::Ended;
            w.say("[modules] editor: an exit after nvim was let go of; nothing to close for it".into());
            detach_answered(w);
            return;
        }
        _ => {}
    }
    if ended(w) {
        return;
    }
    // A person's own nvim that ends closes the window, as plain Neovim's does; nothing reads a record of it, and no
    // quit of this window's is in flight to settle.
    if w.editor_attached() {
        match event {
            EditorEvent::Exited { .. } => {
                w.editor_ready = false;
                w.editor_lost = true;
                w.editor_run = EditorRun::Ended;
                w.say("[modules] editor: nvim ended".into());
                request(w, cx, win, CloseCause::NvimExited, batch);
                return;
            }
            EditorEvent::Unreachable { .. }
            | EditorEvent::QuitCancelled { .. }
            | EditorEvent::QuitNotSent { .. }
            | EditorEvent::Released { .. }
            | EditorEvent::ReleaseRefused { .. }
            | EditorEvent::RestartRefused { .. }
            | EditorEvent::Reopen { .. }
            | EditorEvent::Captured { .. } => {
                w.say(format!(
                    "[modules] editor: {event:?} does not apply to a person's own nvim; ignored"
                ));
                return;
            }
            _ => {}
        }
    }
    let was_ready = w.editor_ready;
    // Taken at every exit, so a restarted nvim starts from nothing said.
    let crashed = matches!(event, EditorEvent::Exited { .. }) && std::mem::take(&mut w.nvim_said) == NvimSaid::Watched;
    // The editor is ready from `Ready` until it ends; a restart in flight clears it too, where it is begun.
    if matches!(
        event,
        EditorEvent::Exited { .. } | EditorEvent::StartFailed { .. } | EditorEvent::Released { .. }
    ) {
        w.editor_ready = false;
    }
    match &event {
        EditorEvent::Ready { .. } => {
            // Nvim has now read its arguments, so a later restart opens nothing.
            w.pending_open.clear();
            if matches!(w.editor_run, EditorRun::Running | EditorRun::Restarting) {
                w.editor_run = EditorRun::Running;
                w.editor_ready = true;
                w.editor_lost = false;
                batch.push_effect(HostOp::Plugin(PluginOp::EditorBack { win }));
                modules::run_queued_opens(w, win, batch);
            }
            return;
        }
        EditorEvent::Released { .. } => {
            if w.editor_run != EditorRun::Ending {
                return;
            }
            if let Some(token) = w.release_exited.take() {
                w.cancel_timer(token);
            }
            w.editor_run = EditorRun::Ended;
            w.editor_lost = false;
            // A show made before the release asked for the restart; a hide or a workspace switch since has taken the
            // editor off screen again, and the restart would bring it back unasked.
            let shown = w
                .layout
                .try_borrow()
                .is_ok_and(|layout| layout.is_shown(&ModuleId::editor()));
            if std::mem::take(&mut w.restart_wanted) && shown {
                modules::restart_editor(w, cx, win, batch);
            }
            return;
        }
        EditorEvent::ReleaseRefused { .. } => {
            if w.editor_run != EditorRun::Ending {
                return;
            }
            if !w.release_refused_logged {
                w.say("[modules] editor: nvim's process is still alive; the pane is released once it ends".into());
                w.release_refused_logged = true;
            }
            if let Some(token) = w.release_exited.take() {
                w.cancel_timer(token);
            }
            w.release_exited = Some(w.arm_after(Duration::from_secs(1)));
            return;
        }
        EditorEvent::RestartRefused { state, .. } => {
            if w.editor_run == EditorRun::Restarting {
                w.editor_run = EditorRun::Ended;
                let text = modules::drop_queued_opens(w);
                let text = text.unwrap_or_else(|| super::feedback::restart_refused_text(state));
                w.refuse(win, text, batch);
            }
            return;
        }
        // A restart whose nvim could not be launched (gone from `PATH`, say) leaves a pane with no nvim that can be
        // restarted again, so the editor is ended rather than left restarting with nothing to wait for, and the next
        // show tries again. The pane says why over itself; the toast says how to try again. A first start's failure
        // is not a restart and leaves the editor's run as it is. Either way the pane cannot take input now, so it
        // does not keep the keys, and the files waiting for it are refused once, in one line.
        EditorEvent::StartFailed { why, .. } => {
            let dropped = modules::drop_queued_opens(w);
            if w.editor_run == EditorRun::Restarting {
                w.editor_run = EditorRun::Ended;
                w.say(format!(
                    "[modules] editor: the restarted nvim could not be launched: {why}"
                ));
                let text = dropped.unwrap_or_else(|| super::feedback::restart_failed_text(cx.keymap));
                w.refuse(win, text, batch);
            } else {
                w.editor_lost = true;
                if let Some(text) = dropped {
                    w.refuse(win, text, batch);
                }
            }
            keys_leave_a_failed_editor(w, cx, win, batch);
            return;
        }
        EditorEvent::Exited { .. } if matches!(w.editor_run, EditorRun::Ending | EditorRun::Ended) => return,
        // A restarted nvim that ends before it is ready never ran for the user, so nothing was quit: the pane ends in
        // place again, to be released and started again, where a running editor's exit would close the window.
        EditorEvent::Exited { .. } if w.editor_run == EditorRun::Restarting => {
            with_quit(w, cx, win, batch, |quit, world| quit.restarted_nvim_exited(world));
            if let Some(text) = modules::drop_queued_opens(w) {
                w.refuse(win, text, batch);
            }
            return;
        }
        // The call may still run: at a blocking prompt it waits in nvim, so this never says the file will not open.
        EditorEvent::OpenFailed { why, .. } => {
            w.say(format!("[modules] editor: an open has no answer: {why}"));
            w.refuse(win, super::feedback::open_failed_text(why), batch);
            return;
        }
        EditorEvent::Watched { .. } => {
            w.nvim_said = NvimSaid::Watched;
            return;
        }
        EditorEvent::Leaving { .. } => {
            w.nvim_said = NvimSaid::Leaving;
            super::reopen::leaving(w);
            return;
        }
        EditorEvent::Reopen { line, .. } => {
            super::reopen::line(w, win, line.clone(), batch);
            return;
        }
        EditorEvent::Captured { token, reply, .. } => {
            super::reopen::captured(w, token.clone(), reply.clone());
            return;
        }
        EditorEvent::Recoverable { count, newest, .. } => {
            w.say(format!(
                "[modules] editor: {count} file(s) with unsaved changes in swap files"
            ));
            chrome::show_toast(w, super::feedback::recoverable_text(*count, newest), ToastKind::Warning);
            return;
        }
        // Its process lives on where nothing reaches it, so no `Ready` comes; its `Exited` comes only once it is gone.
        // What is said about it is the quit flow's, below.
        EditorEvent::Unreachable { .. } => w.editor_lost = true,
        _ => {}
    }
    let never_ready = !was_ready && matches!(event, EditorEvent::Exited { .. } | EditorEvent::Unreachable { .. });
    with_quit(w, cx, win, batch, |quit, world| match event {
        EditorEvent::Exited { settled, .. } => quit.nvim_exited(world, settled, crashed),
        EditorEvent::Unreachable { .. } => quit.nvim_unreachable(world),
        // The window close's `y`, if it had one, was taken back when the close handed the quit to nvim, so a later
        // exit asks again.
        EditorEvent::QuitCancelled { generation, .. } => {
            let cancel = u32::try_from(generation).map_or(super::quit::Cancel::Ignored, |g| quit.cancelled(g));
            world.w.say(match cancel {
                super::quit::Cancel::Cleared => {
                    format!("[modules] editor: nvim's quit {generation} was cancelled; nothing is in flight")
                }
                super::quit::Cancel::CloseWithdrawn => format!(
                    "[modules] editor: nvim's quit {generation}, a window close a later kill joined, was cancelled; \
                     nvim's exit ends the editor only"
                ),
                super::quit::Cancel::Ignored => format!(
                    "[modules] editor: a quit-cancelled letter for quit {generation}, not the one in flight; ignored"
                ),
            });
        }
        EditorEvent::QuitNotSent { generation, .. } => {
            if let Some(text) = quit.quit_not_sent(world, generation) {
                world.refuse(&text);
            }
        }
        _ => {}
    });
    if never_ready {
        if let Some(text) = modules::drop_queued_opens(w) {
            w.refuse(win, text, batch);
        }
    }
}

pub(super) fn kill_editor(w: &mut Window, cx: &Cx, win: WinId, scope: KillScope, batch: &mut Batch) {
    if ended(w) {
        return;
    }
    // A person's own nvim is never quit: killing the editor gives it back, which closes the window.
    if w.editor_attached() {
        w.say("[modules] editor: nvim goes back to its terminal, and the window closes".into());
        request(w, cx, win, CloseCause::User, batch);
        return;
    }
    with_quit(w, cx, win, batch, |quit, world| {
        let now = world.w.now();
        if let Err(text) = quit.kill_editor(world, scope, None, now) {
            world.w.say(format!("[modules] editor: not killed: {text}"));
            world.feedback(super::feedback::CloseOutcome::KillRefused);
        }
    });
}

/// The window's running turns, queued messages, tabs with background work and drafts that would go with it. The base
/// view's running, queued and draft counts already cover every tab of every chat view; adding each extra view's own
/// tab would count it twice. Its background count covers the base view's own tabs only, so each extra chat view's is
/// added.
fn window_counts(cx: &Cx) -> (usize, usize, usize, usize) {
    let counts = cx.reads.chat_kill_counts(crate::tabs::ViewId::BASE);
    let background = counts.background
        + cx.reads
            .chat_views()
            .into_iter()
            .filter(|view| !view.is_base())
            .map(|view| cx.reads.chat_kill_counts(view).background)
            .sum::<usize>();
    (counts.running, counts.queued, background, counts.drafts)
}

/// Whether the window offers to move its modules away: window 2 is the only window it is linked to.
fn move_target(w: &Window) -> bool {
    w.lone_second_peer().is_some()
}

pub(super) fn window_prompt(w: &Window, cx: &Cx, win: WinId) -> Option<String> {
    let (running, queued, background, drafts) = window_counts(cx);
    match w.tiling {
        // Idle windows are kept for the group's next start, so only work the close would cut short is asked about.
        Tiling::Hub if w.has_peer() => return group_cuts_work(w, cx, win).then(|| group_prompt(w, cx, win)),
        // Its close touches nothing of the rest of the group, so what runs here is all it names.
        Tiling::ModuleWindow => {
            let jobs = cx.reads.terminal_jobs(win);
            return crate::tabs::window_close_prompt_with_drafts(running, queued, jobs, background, drafts);
        }
        Tiling::Hub | Tiling::Eitri => {}
    }
    // Only a window that offers `m` counts the pages it would move as running work.
    let offers_move = move_target(w);
    let running = running
        + if offers_move {
            cx.reads.pages_with_tasks(win).len()
        } else {
            0
        };
    let summary = w.group_summary();
    let other = w.has_peer().then(|| crate::tabs::OtherWindow {
        name: super::moves::other_name(w),
        running: summary.as_ref().map_or(0, |s| s.running as usize),
        queued: summary.as_ref().map_or(0, |s| s.queued as usize),
        stays_without_editor: matches!(w.presence(), super::window::EditorPresence::Owned),
    });
    crate::tabs::linked_window_close_prompt(
        running,
        queued,
        cx.reads.terminal_jobs(win),
        background,
        drafts,
        other,
        if offers_move { movable(cx, win).len() } else { 0 },
    )
}

/// Whether closing a hub's group ends something under way or loses something: a turn running, a message queued,
/// background work or an unsent draft that would not come back, in any of its windows or its own; a job in its bottom
/// terminal; or a window that has not said what it holds yet, which counts as holding something. Every module window
/// comes back at the next start.
fn group_cuts_work(w: &Window, cx: &Cx, win: WinId) -> bool {
    let (running, queued, background, drafts) = window_counts(cx);
    let summary = w.group_summary().unwrap_or_default();
    running + queued + background + drafts + cx.reads.terminal_jobs(win) > 0
        || crate::window_link::summary::has_work(&summary)
        || w.peers().any(|(_, peer)| peer.summary().is_none())
}

/// Window 1's question in a hub: every linked module window and what their last summaries say runs in them.
fn group_prompt(w: &Window, cx: &Cx, win: WinId) -> String {
    let (running, queued, background, drafts) = window_counts(cx);
    let summary = w.group_summary();
    crate::tabs::group_close_prompt_with_background(
        w.peers().count(),
        running + summary.as_ref().map_or(0, |s| s.running as usize),
        queued + summary.as_ref().map_or(0, |s| s.queued as usize),
        cx.reads.terminal_jobs(win),
        background + summary.as_ref().map_or(0, |s| s.background as usize),
        drafts + summary.as_ref().map_or(0, |s| s.drafts as usize),
    )
}

/// Close callbacks are delayed until the quit value is back in the window; none re-enters a borrowed flow.
enum CloseIntent {
    Ask(String),
    Withdraw,
    Confirmed,
    Close,
}

struct WindowQuit<'a, 'b> {
    w: &'a mut Window,
    cx: &'a Cx<'b>,
    win: WinId,
    batch: &'a mut Batch,
    intents: Vec<CloseIntent>,
}

/// Where the keys go when nvim fails to start: a pane that cannot take input does not keep them. Construction is
/// asynchronous, after the window is shown and the first focus chosen, so the editor can hold the keys when the
/// failure lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeysAfterFailure {
    /// The keys were elsewhere, or nowhere: they stay.
    Stay,
    /// The editor held them: they move to the chat, shown if it was hidden.
    ToChat,
}

pub fn keys_after_failure(focused: Option<&ModuleId>) -> KeysAfterFailure {
    if focused == Some(&ModuleId::editor()) {
        KeysAfterFailure::ToChat
    } else {
        KeysAfterFailure::Stay
    }
}

/// A failed editor that held the keys gives them to the chat, shown first where it was hidden, and the chat is
/// arrived at as its module key lands; keys anywhere else stay. Only where the host last reported the keys counts:
/// a pane that never took them has nothing to give up.
fn keys_leave_a_failed_editor(w: &mut Window, cx: &Cx, win: WinId, batch: &mut Batch) {
    let focused = w.focus.native_module().cloned();
    if keys_after_failure(focused.as_ref()) == KeysAfterFailure::Stay {
        return;
    }
    let chat = ModuleId::agent();
    if let Err(err) = modules::show_on_screen(w, cx, win, &chat, batch) {
        w.say(format!(
            "[modules] editor: start failed, and the chat could not be shown ({err})"
        ));
    }
    if let Err(err) = super::focus::focus_and_arrive(w, win, &chat, batch) {
        w.say(format!(
            "[modules] editor: start failed, and the keys could not move to the chat ({err})"
        ));
    }
}

fn with_quit<T>(
    w: &mut Window,
    cx: &Cx,
    win: WinId,
    batch: &mut Batch,
    call: impl FnOnce(&mut QuitFlow, &mut WindowQuit<'_, '_>) -> T,
) -> T {
    let mut quit = std::mem::take(&mut w.quit);
    let mut world = WindowQuit {
        w,
        cx,
        win,
        batch,
        intents: Vec::new(),
    };
    let result = call(&mut quit, &mut world);
    let intents = std::mem::take(&mut world.intents);
    world.w.quit = quit;
    for intent in intents {
        match intent {
            CloseIntent::Ask(text) => ask_window(world.w, cx, win, text),
            CloseIntent::Withdraw => world.w.close.confirmed = false,
            CloseIntent::Confirmed => {
                world.w.close.confirmed = true;
                request(world.w, cx, win, CloseCause::NvimExited, world.batch);
            }
            CloseIntent::Close => request(world.w, cx, win, CloseCause::NvimExited, world.batch),
        }
    }
    result
}

impl QuitWorld for WindowQuit<'_, '_> {
    fn read_handoff(&mut self) {
        super::reopen::nvim_ended(self.w, self.win, self.batch);
    }
    fn facts(&self) -> EditorFacts {
        let mut facts = self.cx.reads.editor_facts(self.win);
        facts.starting |= self.w.editor_run == super::window::EditorRun::Restarting;
        if self.w.editor_run != super::window::EditorRun::Running {
            facts.running = false;
            facts.exit_pending = false;
        }
        facts
    }
    fn reveal(&self) -> Reveal {
        self.w
            .layout
            .try_borrow()
            .map(|layout| reveal_for_prompt(&layout, &ModuleId::editor()))
            .unwrap_or(Reveal {
                show: true,
                rehide: false,
            })
    }
    fn editor_gone(&self) -> bool {
        self.w.editor_run != super::window::EditorRun::Running
            || self
                .w
                .layout
                .try_borrow()
                .is_ok_and(|layout| layout.is_gone(&ModuleId::editor()))
    }
    fn close_confirmed(&self) -> bool {
        self.w.close.confirmed
    }
    fn window_close_prompt(&self) -> Option<String> {
        window_prompt(self.w, self.cx, self.win)
    }
    fn send_quit(&mut self, lua: String, generation: u32) {
        let Some(editor) = owned(self.w.editor_kind()) else {
            self.w
                .warn("[wm] BUG: a quit was built for a person's own nvim; nothing was sent".into());
            return;
        };
        self.batch.push_effect(HostOp::Editor(EditorCall::Quit {
            win: self.win,
            lua,
            generation,
            editor,
        }));
    }
    fn end_nvim(&mut self) {
        let Some(editor) = owned(self.w.editor_kind()) else {
            self.w
                .warn("[wm] BUG: an end was built for a person's own nvim; nothing was sent".into());
            return;
        };
        self.batch
            .push_effect(HostOp::Editor(EditorCall::End { win: self.win, editor }));
    }
    fn show_editor(&mut self) -> Result<(), crate::layout::LayoutError> {
        modules::show_on_screen(self.w, self.cx, self.win, &ModuleId::editor(), self.batch)?;
        let brought = self.w.layout.try_borrow_mut().is_ok_and(|mut layout| {
            layout.is_tabbed_away(&ModuleId::editor()) && layout.bring_forward(&ModuleId::editor())
        });
        if brought {
            self.w
                .changed_with_focus(self.win, Some((ModuleId::editor(), vec![])), self.batch);
        }
        Ok(())
    }
    fn focus_editor(&mut self) {
        super::focus::give_keys(self.w, self.win, &ModuleId::editor(), vec![], self.batch);
    }
    fn hide_editor(&mut self) -> Result<(), crate::layout::LayoutError> {
        modules::hide(self.w, self.cx, self.win, &ModuleId::editor(), self.batch)
    }
    fn end_editor(&mut self) -> Result<(), crate::layout::LayoutError> {
        use super::window::EditorRun;
        match self.w.editor_run {
            EditorRun::Running | EditorRun::Restarting => {}
            // Ended already, so there is nothing to end or release again; an editor shown meanwhile (a show before
            // the release, or after a refused restart) is still the user's to kill, and a restart that show asked
            // for is taken back with it.
            EditorRun::Ending | EditorRun::Ended => {
                self.w.restart_wanted = false;
                let shown = self
                    .w
                    .layout
                    .try_borrow()
                    .is_ok_and(|layout| layout.is_shown(&ModuleId::editor()));
                if shown {
                    self.kill_editor_layout(Reopen::InPlace)?;
                    self.w.saver_changed();
                }
                return Ok(());
            }
        }
        self.kill_editor_layout(Reopen::InPlace)?;
        if let Ok(mut workspaces) = self.w.workspaces.try_borrow_mut() {
            frame_with(self.cx.reads, self.win, self.cx.reads.grid_size(self.win), |frame| {
                workspaces.end_everywhere(&ModuleId::editor(), frame)
            });
        }
        self.w.editor_run = super::window::EditorRun::Ending;
        self.w.restart_wanted = false;
        self.w.release_refused_logged = false;
        self.batch
            .push_effect(HostOp::Plugin(PluginOp::EditorClosed { win: self.win }));
        self.w.release_exited = Some(self.w.arm_idle());
        self.w.saver_changed();
        self.w
            .say("[modules] editor: nvim quit; the editor can start again in place".into());
        Ok(())
    }
    fn kill_editor_in_place(&mut self) -> Result<(), crate::layout::LayoutError> {
        self.w.restart_wanted = false;
        self.kill_editor_layout(Reopen::InPlace)
    }
    /// The chat, or in window 1 of a group, whose chats are other windows, the bottom terminal: it is the only other
    /// module that window holds, so the window keeps a place for the keys and the key that starts nvim again.
    fn show_stand_in(&mut self) -> Result<(), crate::layout::LayoutError> {
        let stand_in = match self.w.tiling {
            super::window::Tiling::Hub => ModuleId::terminal(),
            super::window::Tiling::Eitri | super::window::Tiling::ModuleWindow => ModuleId::agent(),
        };
        // The window shows the terminal, not the user: the record keeps it as it was.
        if self.w.tiling == super::window::Tiling::Hub && self.w.terminal_before_stand_in.is_none() {
            self.w.terminal_before_stand_in = self
                .w
                .layout
                .try_borrow()
                .ok()
                .map(|layout| crate::window_link::cm_file::CmTerminal::of(&layout));
        }
        modules::show_on_screen(self.w, self.cx, self.win, &stand_in, self.batch)
    }
    fn ask_to_close_window(&mut self, text: &str) {
        self.intents.push(CloseIntent::Ask(text.into()));
    }
    fn withdraw_close_confirmation(&mut self) {
        self.intents.push(CloseIntent::Withdraw);
    }
    fn close_window_confirmed(&mut self) {
        self.intents.push(CloseIntent::Confirmed);
    }
    fn close_window(&mut self) {
        self.intents.push(CloseIntent::Close);
    }
    fn refuse(&mut self, text: &str) {
        self.feedback(super::feedback::CloseOutcome::Refused(text));
    }
    fn toast(&mut self, text: &str) {
        self.feedback(super::feedback::CloseOutcome::Toast(text));
    }
    fn warn(&mut self, text: &str) {
        chrome::show_toast(self.w, text.to_string(), ToastKind::Warning);
    }
    fn editor_ended_text(&self) -> String {
        super::feedback::editor_ended_text(self.cx.keymap)
    }
    fn editor_crashed_text(&self) -> String {
        super::feedback::editor_crashed_text(self.cx.keymap)
    }
    fn every_second(&mut self, generation: u32) {
        if let Some((token, _)) = self.w.quit_timer.take() {
            self.w.cancel_timer(token);
        }
        self.w.quit_timer = Some((self.w.arm_after(Duration::from_secs(1)), generation));
    }
}

impl WindowQuit<'_, '_> {
    fn feedback(&mut self, outcome: super::feedback::CloseOutcome<'_>) {
        let feedback = super::feedback::close(outcome);
        if feedback.flash {
            self.w.refuse(self.win, feedback.toast.unwrap_or_default(), self.batch);
        } else if let Some(text) = feedback.toast {
            chrome::show_toast(self.w, text, ToastKind::Other);
        }
    }

    fn kill_editor_layout(&mut self, reopen: Reopen) -> Result<(), crate::layout::LayoutError> {
        let next = {
            let mut layout = self
                .w
                .layout
                .try_borrow_mut()
                .map_err(|_| crate::layout::LayoutError::NotInTree(ModuleId::editor()))?;
            frame_with(self.cx.reads, self.win, self.cx.reads.grid_size(self.win), |frame| {
                crate::layout::kill(&mut layout, &ModuleId::editor(), reopen, frame)
            })?
        };
        let ended = HostEvent::SurfaceEnded {
            win: self.win,
            module: ModuleId::editor(),
            why: if reopen == Reopen::Never {
                Ended::Retired
            } else {
                Ended::Killed { reopen }
            },
        };
        self.w.lifecycle.record_event(self.win, &ended);
        self.w
            .changed_with_focus(self.win, next.map(|next| (next, vec![])), self.batch);
        super::focus::modules_changed(self.w, self.win, self.batch);
        Ok(())
    }
}

/// How far the window's overlay labels sit from the edge they are drawn over: the toast always, and
/// this prompt at its corner when the top bar is hidden (Immersive). One value, so the two stay aligned.
pub const TOAST_MARGIN: i32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKey {
    Yes,
    Move,
    No,
    Ignore,
}

/// Only declining the window close's own question runs its decline hook.
pub fn runs_decline_hook(answer: PromptKey, closes_window: bool) -> bool {
    answer == PromptKey::No && closes_window
}

/// A bare modifier waits; either case of y confirms, regardless of its modifiers.
pub fn prompt_key(press: &KeyPress, is_modifier: bool, offers_move: bool) -> PromptKey {
    if is_modifier {
        PromptKey::Ignore
    } else if offers_move && matches!(press.key, Some(KeyName::Char('m' | 'M'))) {
        PromptKey::Move
    } else if matches!(press.key, Some(KeyName::Char('y' | 'Y'))) {
        PromptKey::Yes
    } else {
        PromptKey::No
    }
}

/// Where the prompt's top-left goes, in the overlay's coordinates: in the top bar, from the strip's
/// left edge and centred on the bar's height, while the bar is drawn; at the toast's corner margin
/// (top-left) when Immersive hides it.
pub fn prompt_origin(bar_shown: bool, strip_x: i32, bar_height: i32, label_height: i32) -> (i32, i32) {
    if bar_shown {
        (strip_x.max(0), ((bar_height - label_height) / 2).max(0))
    } else {
        (TOAST_MARGIN, TOAST_MARGIN)
    }
}

/// The facts known about a request sent to nvim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvimAnswer {
    /// When the request was sent.
    pub sent_at: Instant,
    /// The request came back: its Lua returned, or nvim went away.
    pub done: bool,
    /// It came back with nvim's answer, rather than nvim having gone away.
    pub returned: bool,
    /// nvim's last `nvim_get_mode` answer since the send.
    pub last: Option<NvimReply>,
}

/// What nvim last said about its mode, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvimReply {
    /// When the answer came back.
    pub at: Instant,
    /// `nvim_get_mode`'s `mode`: `r?` while a `:confirm` dialog is up.
    pub mode: String,
    /// `nvim_get_mode`'s `blocking`: nvim waits for a key before it takes a request (after `f`, at a
    /// hit-enter prompt).
    pub blocking: bool,
}

impl NvimAnswer {
    /// When nvim's loop was last known to run: its last answer, or the send.
    pub fn last_alive(&self) -> Instant {
        self.last.as_ref().map_or(self.sent_at, |answer| answer.at)
    }
}

/// The y/n `prefix x` asks. `title` is the module's name as the tray and the strip show it;
/// `running`/`queued` are the agent's tabs with a turn or a connect in flight and its queued
/// messages (the window-close prompt's two counts). For [`KillScope::Module`] they are the agent's
/// own consequences, ignored for any other module; for [`KillScope::Window`] -- the last module on
/// screen, whose kill closes Eitri -- they are the window close's, for every module, so this one
/// question stands in for that one too. `jobs` is the window close's third count, its terminals
/// running a job (`terminal::jobs`), named only for [`KillScope::Window`]: a module's own kill ends
/// at most its own terminal, whose y/n is tmux's plain one.
pub fn prompt(
    id: &ModuleId,
    title: &str,
    scope: KillScope,
    running: usize,
    queued: usize,
    jobs: usize,
    background: usize,
) -> String {
    prompt_closing_tab(id, title, scope, None, running, queued, jobs, background)
}

/// [`prompt`] for a chat view that shows one tab (`agent#N`): the y/n names the tab it closes,
/// `tab` (its label as the view's strip shows it), ahead of the counts, which are that tab's own. The
/// base view's kill closes every base-view tab and names none.
pub fn prompt_closing_tab(
    id: &ModuleId,
    title: &str,
    scope: KillScope,
    tab: Option<&str>,
    running: usize,
    queued: usize,
    jobs: usize,
    background: usize,
) -> String {
    let mut consequences = Vec::new();
    if scope == KillScope::Window {
        consequences.push("closes Eitri".to_string());
        if running > 0 {
            consequences.push(format!("{running} running"));
        }
        if queued > 0 {
            consequences.push(format!("{queued} queued"));
        }
        if background > 0 {
            consequences.push(format!("{background} with background tasks"));
        }
        match jobs {
            0 => {}
            1 => consequences.push("1 terminal job".to_string()),
            n => consequences.push(format!("{n} terminal jobs")),
        }
        return format!("kill-pane {title}? {} (y/n)", consequences.join(", "));
    }
    match id.kind() {
        ModuleKind::Agent => {
            if let Some(tab) = tab {
                consequences.push(format!("closes tab {tab}"));
            }
            if running > 0 {
                consequences.push(format!("{running} running"));
            }
            if queued > 0 {
                consequences.push(format!("{queued} queued"));
            }
            if background > 0 {
                consequences.push(format!("{background} with background tasks"));
            }
        }
        ModuleKind::Editor => {}
        ModuleKind::Terminal | ModuleKind::Canvas | ModuleKind::LuaWebview => {}
    }
    if consequences.is_empty() {
        format!("kill-pane {title}? (y/n)")
    } else {
        format!("kill-pane {title}? {} (y/n)", consequences.join(", "))
    }
}

/// Why nvim was asked to `:confirm qall`, for what its exit does: close the editor for this window
/// ([`KillScope::Module`]), or close the window ([`KillScope::Window`], and every window close),
/// carrying the window close's prompt as it stood when nvim was asked ([`close_is_confirmed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorQuit {
    Module,
    Window { confirmed: Option<String> },
}

/// A quit of the editor in flight: nvim was asked to run `editor_quit_lua(generation)`
/// (the answer records how nvim is taking it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    /// Bumped by the quit flow before each send, so a cancel's own generation can be told apart from a
    /// later, different quit of the same editor ([`clear_if_cancelled`]).
    pub generation: u32,
    /// What nvim's exit then does.
    pub quit: EditorQuit,
    /// The editor was hidden and was shown for nvim's prompt ([`Reveal::rehide`]): hidden again if
    /// nvim quits, so the arrangement the window saves on close is the user's, not the close's.
    pub revealed: bool,
    /// Its request was sent to nvim, so the pane's watch is this quit's own (every send replaces the
    /// watch, and only a quit sends one): when that watch says nvim answered it, nvim did not quit
    /// and this is cleared. `false` for a quit joined to an nvim
    /// that had exited already, which has no request of its own.
    pub sent: bool,
    /// nvim did not answer this quit and is being ended: the request stands, and no prompt can come of it.
    pub killed: bool,
    /// A kill of the editor that sent nothing (nvim out of reach, or just exited) and joined an
    /// older window close nvim was sent and had not answered: the close wins ([`merge`]), and this
    /// is that close's generation, since its request -- not this quit's, which has none -- says
    /// whether it still stands. Its cancel letter, or its request coming back answered, however
    /// late either is read, takes the close back out ([`Asked::withdraw_lent_close`]): the kill is
    /// what is left, even when the answer arrives after the exit. Only
    /// ever set on an unsent `Window` whose own request was a kill, so what it withdraws to is
    /// always [`EditorQuit::Module`].
    pub lent_by: Option<u32>,
}

impl Asked {
    /// The window close this quit carries was cancelled in nvim ([`Asked::lent_by`]): nvim's exit
    /// then does what the kill asked, retiring the editor, and never closes the window on that
    /// close's `y`.
    pub fn withdraw_lent_close(&mut self) {
        if self.lent_by.take().is_some() {
            self.quit = EditorQuit::Module;
        }
    }
}

pub type QuitInFlight = Option<Asked>;

/// Clears the quit in flight if `generation` is its own, returning it; a letter from an earlier,
/// already superseded quit leaves a newer one alone.
pub fn clear_if_cancelled(quitting: &mut QuitInFlight, generation: u32) -> Option<Asked> {
    if quitting.as_ref().is_some_and(|asked| asked.generation == generation) {
        return quitting.take();
    }
    None
}

/// What the quit flow does with the editor before nvim draws its prompt in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reveal {
    /// Not on screen (hidden, zoomed away, or on a tab its container does not draw): shown, which
    /// ends a zoom and, for a tab, chooses it (the quit flow's `QuitWorld::show_editor`).
    /// `Layout::is_visible`, not `is_shown`, which ignores a zoom and a tab -- a close with the agent
    /// zoomed left nvim's prompt waiting unseen while the editor took the keys, and the next `n` or
    /// `d` answered it blind.
    pub show: bool,
    /// Hidden (not merely zoomed away): hidden again if nvim quits. A zoom is not saved
    /// (`crate::layout::persist`), so ending one changes nothing the window keeps.
    pub rehide: bool,
}

/// What the editor needs before nvim's prompt is drawn in it. An editor on a tab not drawn is shown in
/// the layout's terms but off screen, so it is shown (its tab chosen) and, never having been hidden,
/// not hidden again.
pub fn reveal_for_prompt(layout: &Layout, editor: &ModuleId) -> Reveal {
    Reveal {
        show: !layout.is_visible(editor),
        rehide: !layout.is_shown(editor),
    }
}

/// How long nvim's own loop may go without answering before a second request takes it as not
/// answering: the fork's own `LiveHarness::shutdown` wait (`SHUTDOWN_WAIT`, 5 s). An nvim whose
/// loop runs answers the pane's watch every 250 ms, whatever it is showing.
pub const NVIM_QUIT_PATIENCE: Duration = Duration::from_secs(5);

/// Where nvim stands with the quit it was sent, read off the pane's watch on that request
/// (the last `nvim_get_mode` answer, refreshed every 250 ms while the request is outstanding).
/// Not from redraws: the dialog draws once and then waits for the user, drawing nothing, exactly
/// as a hung nvim draws nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvimState {
    /// The request came back -- its Lua returned, so nvim did not quit -- or nothing was sent.
    Returned,
    /// nvim's loop has not answered for [`NVIM_QUIT_PATIENCE`]: stuck (a Lua busy loop, a stopped
    /// process), or waiting on an external command (`system()`, `:!`, a formatter a save hook runs),
    /// during which nvim answers nothing (see [`NvimState::Busy`]). A second request then ends it
    /// ([`EditorCall::End`]: SIGTERM and stdin closed, SIGKILL by pid after the
    /// patience), and [`not_answering_text`] says so.
    NotAnswering,
    /// Its `:confirm` dialog is up (`nvim_get_mode` says `r?`, or `c` on nvim 0.11: [`is_quit_dialog`]).
    Asking,
    /// It waits for a key before it takes the request (`blocking`: after `f`, at a hit-enter prompt).
    WaitingForKey,
    /// Answering, and neither: about to ask, or busy in something that keeps its loop running (a
    /// write hook in `vim.wait()`), which `Ctrl+C` interrupts. NOT an external command: measured on
    /// nvim 0.12.5, `vim.fn.system('sleep 3')` and `:silent !sleep 3` leave
    /// `nvim_get_mode` unanswered until they end, exactly as a Lua busy loop does, so a save hook
    /// that runs one for [`NVIM_QUIT_PATIENCE`] or longer reads [`NvimState::NotAnswering`].
    Busy,
}

/// The `:confirm qall` dialog while the pane's quit is out: `nvim_get_mode` says `r?` on nvim 0.12, `c` on 0.11, and
/// `blocking` false on both (measured on 0.11.0, 0.11.2 and 0.12.6 with a UI attached). A `c` that blocks is a `:` line
/// waiting for the key after `Ctrl+V`, `Ctrl+R` or `Ctrl+K`, which is [`NvimState::WaitingForKey`].
pub fn is_quit_dialog(mode: &str, blocking: bool) -> bool {
    mode == "r?" || (mode == "c" && !blocking)
}

pub fn nvim_state(watch: Option<&NvimAnswer>, now: Instant) -> NvimState {
    let Some(watch) = watch else {
        return NvimState::Returned;
    };
    if watch.done {
        return NvimState::Returned;
    }
    if now.saturating_duration_since(watch.last_alive()) >= NVIM_QUIT_PATIENCE {
        return NvimState::NotAnswering;
    }
    match &watch.last {
        Some(answer) if is_quit_dialog(&answer.mode, answer.blocking) => NvimState::Asking,
        Some(answer) if answer.blocking => NvimState::WaitingForKey,
        _ => NvimState::Busy,
    }
}

/// What a request to quit the editor -- a window close, `prefix x` on it -- does, given the quit
/// already in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitRequest {
    /// Nothing in flight, or the one in flight came back (cancelled): nvim is asked.
    Ask,
    /// nvim has the quit already: a second `:confirm qall` would nest inside the first instead of
    /// answering it (measured), so the user deals with the one in the editor ([`answer_there_text`]).
    AnswerThere(NvimState),
    /// nvim's loop has not answered for [`NVIM_QUIT_PATIENCE`]: ended (never `:qa!`), its swap
    /// files kept.
    Kill,
}

/// `watch` records the answer to the request in flight. It
/// also says when that request came back, so a cancel is seen even with no pane-switch shim to send
/// the letter.
pub fn quit_request(asked: Option<&Asked>, watch: Option<&NvimAnswer>, now: Instant) -> QuitRequest {
    if asked.is_none() {
        return QuitRequest::Ask;
    }
    match nvim_state(watch, now) {
        NvimState::Returned => QuitRequest::Ask,
        NvimState::NotAnswering => QuitRequest::Kill,
        state => QuitRequest::AnswerThere(state),
    }
}

/// The refusal of a second quit request, by what nvim is doing with the first.
pub fn answer_there_text(state: NvimState) -> &'static str {
    match state {
        NvimState::WaitingForKey => {
            "nvim will ask once it has the key it is waiting for \u{2014} type it, or Esc, in the editor"
        }
        NvimState::Busy => "nvim is busy and asks when it is done \u{2014} Ctrl+C in the editor interrupts it",
        NvimState::Asking | NvimState::Returned | NvimState::NotAnswering => {
            "nvim is already asking whether to quit; answer it there"
        }
    }
}

/// The quit in flight once a second request joins it: a window close wins over the editor's own
/// kill, since the user asked for the window to go.
pub fn merge(current: &EditorQuit, incoming: EditorQuit) -> EditorQuit {
    match (current, incoming) {
        (_, incoming @ EditorQuit::Window { .. }) => incoming,
        (current, EditorQuit::Module) => current.clone(),
    }
}

/// The toast once nvim's loop has not answered for [`NVIM_QUIT_PATIENCE`]: what a second request
/// then does -- closes stdin and sends SIGTERM, giving nvim a chance to sync its swap file before
/// SIGKILL after 5 s. nvim may only be running a long external command.
pub fn not_answering_text(quit: &EditorQuit) -> &'static str {
    match quit {
        EditorQuit::Window { .. } => {
            "nvim is not responding to the quit \u{2014} close the window again to end it \
             (its swap file keeps your changes)"
        }
        EditorQuit::Module => {
            "nvim is not responding to the quit \u{2014} kill-pane the editor again to end it \
             (its swap file keeps your changes)"
        }
    }
}

/// Whether a declined window close ends the editor in place: only once nvim has exited, and only
/// if it is not ended already. A close declined while nvim runs leaves a working editor.
pub fn retire_on_declined_close(nvim_exited: bool, editor_gone: bool) -> bool {
    nvim_exited && !editor_gone
}

/// Whether a window close after a `y` to [`KillScope::Window`] may skip its own y/n. `confirmed` is
/// the window close's prompt (`crate::tabs::window_close_prompt_with_jobs`) as it stood when the kill
/// was asked, `now` as it stands at the close: the editor's close waits for nvim, whose own
/// `:confirm qall` may be answered much later. Nothing worth asking about now, or exactly what was
/// said yes to: no second question. Anything else is asked.
pub fn close_is_confirmed(confirmed: Option<&str>, now: Option<&str>) -> bool {
    now.is_none() || now == confirmed
}

/// Where the layout leaves a killed module (`crate::layout::Reopen`). `decls` are the window's
/// module declarations: the Lua panels as `init.lua` registered them, each at its open rule's place
/// when it has one, and the base terminal's when an open rule names it.
pub fn reopen(id: &ModuleId, decls: &[ModuleDecl]) -> Reopen {
    match id.kind() {
        // `BelowEditor`, not `BelowRoot`, unless an open rule says otherwise: a terminal killed next
        // to a Lua `side` panel comes back below the editor's own leaf only, as a first launch puts
        // it, not full width below the side panel too
        // (`crate::layout::tree::place_new`'s own doc).
        ModuleKind::Terminal if id.number() == 1 => Reopen::At(declared_placement(decls, id, Placement::BelowEditor)),
        // A numbered terminal was made by the user and has no first-launch place to go back to: it
        // leaves every workspace, and its number is free for the next new terminal.
        ModuleKind::Terminal => Reopen::Remove,
        ModuleKind::LuaWebview => decls
            .iter()
            .find(|d| d.id == *id)
            .map_or(Reopen::InPlace, |d| Reopen::At(d.placement)),
        // A numbered chat view was made by the user, as a numbered terminal was: it leaves every
        // workspace, its page waits idle under its id, and its number is free for the next new view.
        ModuleKind::Agent if id.number() >= 2 => Reopen::Remove,
        ModuleKind::Agent | ModuleKind::Canvas => Reopen::InPlace,
        ModuleKind::Editor => Reopen::InPlace,
    }
}

/// The Lua nvim is asked to run to quit the editor (`crate::layout::kill::editor_quit_lua`).
pub use crate::layout::kill::editor_quit_lua;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        keymap::{prefix::Press, KeyName},
        wm::{KeyPress, Mods},
    };

    /// The keys leave a failed editor only if it held them.
    #[test]
    fn the_keys_leave_a_failed_editor_only_if_it_held_them() {
        assert_eq!(keys_after_failure(Some(&ModuleId::editor())), KeysAfterFailure::ToChat);
        assert_eq!(keys_after_failure(Some(&ModuleId::agent())), KeysAfterFailure::Stay);
        assert_eq!(keys_after_failure(Some(&ModuleId::terminal())), KeysAfterFailure::Stay);
        assert_eq!(keys_after_failure(None), KeysAfterFailure::Stay);
    }

    fn press(key: Option<KeyName>) -> KeyPress {
        KeyPress {
            keycode: 42,
            press: Press::Other,
            key,
            mods: Mods::default(),
            super_or_hyper: false,
            super_or_meta: false,
        }
    }

    #[test]
    fn y_confirms_any_other_key_cancels_and_a_bare_modifier_waits() {
        assert_eq!(
            prompt_key(&press(Some(KeyName::Char('y'))), false, false),
            PromptKey::Yes
        );
        assert_eq!(
            prompt_key(&press(Some(KeyName::Char('Y'))), false, false),
            PromptKey::Yes
        );
        assert_eq!(
            prompt_key(&press(Some(KeyName::Char('n'))), false, false),
            PromptKey::No
        );
        assert_eq!(prompt_key(&press(None), false, false), PromptKey::No);
        assert_eq!(
            prompt_key(&press(Some(KeyName::Enter)), false, false),
            PromptKey::No,
            "Enter is not yes"
        );
        assert_eq!(prompt_key(&press(None), true, false), PromptKey::Ignore);
        for key in ['m', 'M'] {
            assert_eq!(
                prompt_key(&press(Some(KeyName::Char(key))), false, true),
                PromptKey::Move
            );
            assert_eq!(
                prompt_key(&press(Some(KeyName::Char(key))), false, false),
                PromptKey::No
            );
        }
    }

    /// Only a `No` to the window close's own question reaches its decline hook -- not a `y`,
    /// not a bare modifier, and not a `No` to `prefix x`'s kill-pane question.
    #[test]
    fn only_a_declined_window_close_runs_the_decline_hook() {
        assert!(runs_decline_hook(PromptKey::No, true));
        assert!(!runs_decline_hook(PromptKey::Yes, true));
        assert!(!runs_decline_hook(PromptKey::Ignore, true));
        assert!(!runs_decline_hook(PromptKey::No, false));
    }

    /// The prompt stays in the strip or at the corner when the bar is hidden.
    #[test]
    fn prompt_origin_sits_in_the_strip_while_the_bar_is_shown_and_at_the_toast_corner_otherwise() {
        assert_eq!(prompt_origin(true, 100, 24, 14), (100, 5));
        // A label taller than the bar: the centring math would go negative, clamped to 0.
        assert_eq!(prompt_origin(true, 100, 10, 24), (100, 0));
        assert_eq!(prompt_origin(false, 100, 24, 14), (TOAST_MARGIN, TOAST_MARGIN));
        // A strip whose `compute_point` x came back negative (not yet laid out) clamps to 0.
        assert_eq!(prompt_origin(true, -5, 24, 14), (0, 5));
    }
    #[test]
    fn ctrl_shift_y_answers_the_close_prompt() {
        let mut press = press(Some(KeyName::Char('y')));
        press.mods.ctrl = true;
        press.mods.shift = true;
        assert_eq!(press.press, Press::Other);
        assert_eq!(prompt_key(&press, false, false), PromptKey::Yes);
    }
}

#[cfg(test)]
mod kill_tests {
    use super::*;

    fn asked(generation: u32, quit: EditorQuit) -> Asked {
        Asked {
            generation,
            quit,
            revealed: false,
            sent: true,
            killed: false,
            lent_by: None,
        }
    }

    /// Every state past the question reads as ending, but `Quitting`, which also outlives a close nvim turned down.
    #[test]
    fn only_a_close_nothing_can_hold_is_ending() {
        let quit = QuitFlow::default();
        for (state, ending) in [
            (CloseState::Idle, false),
            (CloseState::Asking, false),
            (CloseState::Quitting, false),
            (CloseState::Moving, true),
            (CloseState::TearingDown, true),
            (CloseState::Closed, true),
        ] {
            let close = CloseFlow {
                state,
                ..CloseFlow::default()
            };
            assert_eq!(close.is_ending(&quit), ending, "{state:?}");
        }
    }

    /// A watch on a request sent at `sent_at`, last answered `answered` after it with `mode`.
    fn watch(sent_at: Instant, done: bool, answer: Option<(Duration, &str, bool)>) -> NvimAnswer {
        NvimAnswer {
            sent_at,
            done,
            returned: done,
            last: answer.map(|(after, mode, blocking)| NvimReply {
                at: sent_at + after,
                mode: mode.to_string(),
                blocking,
            }),
        }
    }

    #[test]
    fn a_cancelled_quit_clears_its_own_kill() {
        let mut quitting: QuitInFlight = Some(asked(3, EditorQuit::Module));
        assert_eq!(
            clear_if_cancelled(&mut quitting, 3).map(|a| a.quit),
            Some(EditorQuit::Module)
        );
        assert_eq!(quitting, None);

        let window = EditorQuit::Window {
            confirmed: Some("x".into()),
        };
        let mut quitting: QuitInFlight = Some(asked(3, window.clone()));
        assert_eq!(clear_if_cancelled(&mut quitting, 3).map(|a| a.quit), Some(window));
        assert_eq!(quitting, None);
    }

    #[test]
    fn a_late_letter_from_an_earlier_kill_leaves_the_newer_one() {
        let newer = asked(4, EditorQuit::Module);
        let mut quitting: QuitInFlight = Some(newer.clone());
        assert_eq!(clear_if_cancelled(&mut quitting, 3), None);
        assert_eq!(quitting, Some(newer));
    }

    #[test]
    fn a_letter_with_nothing_in_flight_changes_nothing() {
        let mut quitting: QuitInFlight = None;
        assert_eq!(clear_if_cancelled(&mut quitting, 1), None);
        assert_eq!(quitting, None);
    }

    #[test]
    fn a_letter_from_the_future_is_not_this_kill() {
        let older = asked(2, EditorQuit::Module);
        let mut quitting: QuitInFlight = Some(older.clone());
        assert_eq!(clear_if_cancelled(&mut quitting, 5), None);
        assert_eq!(quitting, Some(older));
    }

    /// A kill carrying an older close takes it back out to itself; a close of its own, which no
    /// request of nvim's can cancel, is never withdrawn.
    #[test]
    fn withdrawing_a_lent_close_leaves_the_kill_and_only_a_lent_one() {
        let window = EditorQuit::Window {
            confirmed: Some("x".into()),
        };
        let mut lent = Asked {
            sent: false,
            lent_by: Some(3),
            ..asked(4, window.clone())
        };
        lent.withdraw_lent_close();
        assert_eq!((lent.quit, lent.lent_by), (EditorQuit::Module, None));

        let mut own = Asked {
            sent: false,
            ..asked(5, window.clone())
        };
        own.withdraw_lent_close();
        assert_eq!(own.quit, window);
    }

    /// With nothing in flight, a quit of the editor -- a window close included -- asks nvim. A
    /// second one while nvim has it is refused (a second `:confirm qall` nests), saying what nvim is
    /// doing; once the first came back (cancelled -- with or without the shim's letter) nvim is
    /// asked again.
    #[test]
    fn a_quit_asks_nvim_and_a_second_one_is_answered_where_nvim_is() {
        let sent = Instant::now();
        let now = sent + Duration::from_secs(1);
        let in_flight = asked(1, EditorQuit::Window { confirmed: None });
        assert_eq!(quit_request(None, None, now), QuitRequest::Ask);
        let dialog = watch(sent, false, Some((Duration::from_millis(900), "r?", false)));
        assert_eq!(quit_request(None, Some(&dialog), now), QuitRequest::Ask);

        assert_eq!(
            quit_request(Some(&in_flight), Some(&dialog), now),
            QuitRequest::AnswerThere(NvimState::Asking)
        );
        let after_f = watch(sent, false, Some((Duration::from_millis(900), "n", true)));
        assert_eq!(
            quit_request(Some(&in_flight), Some(&after_f), now),
            QuitRequest::AnswerThere(NvimState::WaitingForKey),
            "held behind a key nvim waits for: it asks once it has it"
        );
        let busy = watch(sent, false, Some((Duration::from_millis(900), "n", false)));
        assert_eq!(
            quit_request(Some(&in_flight), Some(&busy), now),
            QuitRequest::AnswerThere(NvimState::Busy)
        );
        let returned = watch(sent, true, Some((Duration::from_millis(900), "n", false)));
        assert_eq!(
            quit_request(Some(&in_flight), Some(&returned), now),
            QuitRequest::Ask,
            "cancelled, its letter not read yet (or no shim to send one)"
        );
        assert_eq!(quit_request(Some(&in_flight), None, now), QuitRequest::Ask);
    }

    /// nvim 0.11 shows the same `:confirm qall` dialog in mode `c` (0.12 says `r?`); measured on 0.11.0 and
    /// 0.11.2 with a UI attached, `blocking` false on both. While the quit is out, `c` is that dialog.
    #[test]
    fn an_nvim_0_11_quit_dialog_in_mode_c_is_asking() {
        let sent = Instant::now();
        let now = sent + Duration::from_secs(1);
        let in_flight = asked(1, EditorQuit::Window { confirmed: None });
        let dialog_0_11 = watch(sent, false, Some((Duration::from_millis(900), "c", false)));
        assert_eq!(
            quit_request(Some(&in_flight), Some(&dialog_0_11), now),
            QuitRequest::AnswerThere(NvimState::Asking)
        );
        // A `:` line waiting for the key after Ctrl+V, Ctrl+R or Ctrl+K reports `c` and blocking.
        let cmdline_key = watch(sent, false, Some((Duration::from_millis(900), "c", true)));
        assert_eq!(
            quit_request(Some(&in_flight), Some(&cmdline_key), now),
            QuitRequest::AnswerThere(NvimState::WaitingForKey)
        );
    }

    /// A dialog that waits for its answer draws
    /// nothing, as a hung nvim draws nothing, so "not answering" is nvim's own loop not answering the
    /// watch -- never redraws, and never timed from the send alone. A dialog left up for minutes is
    /// still the user's; an nvim that stopped answering after drawing it (Save, then a hook that
    /// hangs) is killed on the next request.
    #[test]
    fn only_an_nvim_whose_loop_stopped_answering_is_killed() {
        let sent = Instant::now();
        let in_flight = asked(1, EditorQuit::Window { confirmed: None });

        let dialog_for_minutes = watch(sent, false, Some((Duration::from_secs(300), "r?", false)));
        let now = sent + Duration::from_secs(300) + Duration::from_millis(250);
        assert_eq!(
            quit_request(Some(&in_flight), Some(&dialog_for_minutes), now),
            QuitRequest::AnswerThere(NvimState::Asking)
        );

        // Drew its dialog, answered, then went quiet: killed once the quiet lasts the patience.
        let hung_after_save = watch(sent, false, Some((Duration::from_secs(2), "r?", false)));
        let quiet_since = sent + Duration::from_secs(2);
        assert_eq!(
            quit_request(
                Some(&in_flight),
                Some(&hung_after_save),
                quiet_since + NVIM_QUIT_PATIENCE - Duration::from_millis(1)
            ),
            QuitRequest::AnswerThere(NvimState::Asking),
            "not quiet long enough"
        );
        assert_eq!(
            quit_request(
                Some(&in_flight),
                Some(&hung_after_save),
                quiet_since + NVIM_QUIT_PATIENCE
            ),
            QuitRequest::Kill
        );

        // Never answered at all: timed from the send.
        let never = watch(sent, false, None);
        assert_eq!(
            quit_request(Some(&in_flight), Some(&never), sent + NVIM_QUIT_PATIENCE),
            QuitRequest::Kill
        );
        assert_eq!(nvim_state(Some(&never), sent + Duration::from_secs(1)), NvimState::Busy);
    }

    #[test]
    fn a_refused_second_request_says_what_nvim_is_doing() {
        assert!(answer_there_text(NvimState::Asking).contains("answer it there"));
        assert!(answer_there_text(NvimState::WaitingForKey).contains("Esc"));
        assert!(answer_there_text(NvimState::Busy).contains("Ctrl+C"));
    }

    /// An editor zoomed away is shown too -- it is shown in the layout's terms
    /// (`is_shown`) yet off screen -- and, not having been hidden, is not hidden again afterwards.
    #[test]
    fn a_hidden_or_zoomed_away_editor_is_brought_on_screen_for_the_prompt() {
        use crate::layout::{hide, Frame, Size};
        let editor = ModuleId::editor();
        let mut layout = Layout::initial(&[]).unwrap();
        assert_eq!(
            reveal_for_prompt(&layout, &editor),
            Reveal {
                show: false,
                rehide: false
            }
        );

        layout.toggle_zoom(&ModuleId::agent());
        assert!(layout.is_shown(&editor) && !layout.is_visible(&editor), "the case");
        assert_eq!(
            reveal_for_prompt(&layout, &editor),
            Reveal {
                show: true,
                rehide: false
            }
        );

        layout.toggle_zoom(&ModuleId::agent());
        hide(&mut layout, &editor, &Frame::new(Size { w: 1280, h: 721 }, 1)).unwrap();
        assert_eq!(
            reveal_for_prompt(&layout, &editor),
            Reveal {
                show: true,
                rehide: true
            }
        );
    }

    #[test]
    fn an_editor_on_a_tab_not_drawn_is_brought_on_screen_for_the_prompt() {
        use crate::layout::{Child, Con, ConLayout, Node};
        let editor = ModuleId::editor();
        let layout = Layout::new(
            Node::Con(Con::new(
                ConLayout::Tabbed,
                vec![
                    Child::new(Node::Leaf(ModuleId::editor()), 0.5),
                    Child::new(Node::Leaf(ModuleId::agent()), 0.5),
                ],
            )),
            ModuleId::agent(),
        )
        .unwrap();
        assert!(layout.is_tabbed_away(&editor), "the case");
        assert_eq!(
            reveal_for_prompt(&layout, &editor),
            Reveal {
                show: true,
                rehide: false
            }
        );
    }

    #[test]
    fn a_window_close_joining_a_kill_of_the_editor_closes_the_window() {
        let window = EditorQuit::Window {
            confirmed: Some("close window? 1 running (y/n)".into()),
        };
        assert_eq!(merge(&EditorQuit::Module, window.clone()), window);
        assert_eq!(merge(&window, EditorQuit::Module), window);
        assert_eq!(merge(&EditorQuit::Module, EditorQuit::Module), EditorQuit::Module);
        assert_eq!(
            merge(&window, EditorQuit::Window { confirmed: None }),
            EditorQuit::Window { confirmed: None },
            "the newer close's prompt is what was asked last"
        );
    }

    #[test]
    fn the_not_answering_toast_says_what_the_second_request_is() {
        assert!(not_answering_text(&EditorQuit::Window { confirmed: None }).contains("close the window again"));
        assert!(not_answering_text(&EditorQuit::Module).contains("kill-pane the editor again"));
        // nvim may only be waiting on an external command; the kill loses what its swap file
        // does not have yet, and the toast says so before the second request.
        for quit in [EditorQuit::Window { confirmed: None }, EditorQuit::Module] {
            assert!(not_answering_text(&quit).contains("swap file"), "{quit:?}");
        }
    }

    /// A close declined once nvim has exited retires the editor; declined while nvim runs, or
    /// with the editor already retired, it changes nothing.
    #[test]
    fn a_declined_close_retires_the_editor_only_once_nvim_is_gone() {
        assert!(retire_on_declined_close(true, false));
        assert!(
            !retire_on_declined_close(false, false),
            "a running nvim is a working editor"
        );
        assert!(!retire_on_declined_close(true, true), "already retired");
    }

    /// tmux's own text, with the module's name where tmux puts the pane's number.
    #[test]
    fn kill_prompts_name_background_tabs_for_agent_views_and_the_window() {
        let view = ModuleId::parse("agent#2").unwrap();
        assert_eq!(
            prompt_closing_tab(&view, "agent 2", KillScope::Module, Some("3 refactor"), 0, 0, 0, 1),
            "kill-pane agent 2? closes tab 3 refactor, 1 with background tasks (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::editor(), "editor", KillScope::Window, 1, 2, 3, 4),
            "kill-pane editor? closes Eitri, 1 running, 2 queued, 4 with background tasks, 3 terminal jobs (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::terminal(), "terminal", KillScope::Module, 0, 0, 0, 1),
            "kill-pane terminal? (y/n)"
        );
    }

    #[test]
    fn the_prompt_is_tmuxs_kill_pane_prompt() {
        assert_eq!(
            prompt(&ModuleId::terminal(), "terminal", KillScope::Module, 0, 0, 0, 0),
            "kill-pane terminal? (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::lua("notes"), "Notes", KillScope::Module, 3, 3, 0, 0),
            "kill-pane Notes? (y/n)"
        );
    }

    #[test]
    fn the_agents_prompt_says_how_many_tabs_are_running_or_queued() {
        let agent = ModuleId::agent();
        assert_eq!(
            prompt(&agent, "agent", KillScope::Module, 0, 0, 0, 0),
            "kill-pane agent? (y/n)"
        );
        assert_eq!(
            prompt(&agent, "agent", KillScope::Module, 1, 0, 0, 0),
            "kill-pane agent? 1 running (y/n)"
        );
        assert_eq!(
            prompt(&agent, "agent", KillScope::Module, 2, 1, 0, 0),
            "kill-pane agent? 2 running, 1 queued (y/n)"
        );
    }

    #[test]
    fn the_editors_prompt_is_tmuxs_plain_kill_pane_prompt() {
        assert_eq!(
            prompt(&ModuleId::editor(), "editor", KillScope::Module, 5, 5, 0, 0),
            "kill-pane editor? (y/n)"
        );
    }

    /// The last module on screen: the kill closes Eitri, and the one question also carries the
    /// window close's own `N running, M queued` (it is not asked a second time), for every module.
    #[test]
    fn the_last_modules_prompt_says_it_closes_eitri_and_what_is_running() {
        assert_eq!(
            prompt(&ModuleId::editor(), "editor", KillScope::Window, 0, 0, 0, 0),
            "kill-pane editor? closes Eitri (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::editor(), "editor", KillScope::Window, 2, 1, 0, 0),
            "kill-pane editor? closes Eitri, 2 running, 1 queued (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::terminal(), "terminal", KillScope::Window, 1, 0, 0, 0),
            "kill-pane terminal? closes Eitri, 1 running (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::agent(), "agent", KillScope::Window, 0, 3, 0, 0),
            "kill-pane agent? closes Eitri, 3 queued (y/n)"
        );
    }

    /// Closing Eitri ends every terminal's job too, so the last module's kill names them as the
    /// window close does; a module's own kill does not.
    #[test]
    fn the_last_modules_kill_names_terminal_jobs() {
        assert_eq!(
            prompt(&ModuleId::editor(), "editor", KillScope::Window, 0, 0, 2, 0),
            "kill-pane editor? closes Eitri, 2 terminal jobs (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::terminal(), "terminal", KillScope::Window, 1, 0, 1, 0),
            "kill-pane terminal? closes Eitri, 1 running, 1 terminal job (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::terminal(), "terminal", KillScope::Module, 0, 0, 1, 0),
            "kill-pane terminal? (y/n)",
            "a module's own kill asks tmux's plain question"
        );
    }

    /// What `y` said yes to is the window close's prompt as it stood when asked. The close does not
    /// ask again unless that changed into something else still worth asking about.
    #[test]
    fn the_close_asks_again_only_if_what_was_running_changed() {
        assert!(close_is_confirmed(None, None));
        assert!(
            close_is_confirmed(Some("close window? 1 running (y/n)"), None),
            "it finished"
        );
        assert!(close_is_confirmed(
            Some("close window? 1 running (y/n)"),
            Some("close window? 1 running (y/n)")
        ));
        assert!(!close_is_confirmed(None, Some("close window? 1 running (y/n)")));
        assert!(!close_is_confirmed(
            Some("close window? 1 running (y/n)"),
            Some("close window? 2 running (y/n)")
        ));
    }

    #[test]
    fn a_killed_module_goes_back_to_its_first_launch_place_or_stays() {
        let decls = [ModuleDecl {
            id: ModuleId::lua("side"),
            placement: Placement::RightOfRoot,
        }];
        assert_eq!(
            reopen(&ModuleId::terminal(), &decls),
            Reopen::At(Placement::BelowEditor)
        );
        assert_eq!(
            reopen(&ModuleId::lua("side"), &decls),
            Reopen::At(Placement::RightOfRoot)
        );
        assert_eq!(reopen(&ModuleId::lua("unknown"), &decls), Reopen::InPlace);
        assert_eq!(reopen(&ModuleId::agent(), &decls), Reopen::InPlace);
        assert_eq!(reopen(&ModuleId::editor(), &decls), Reopen::InPlace);
    }

    #[test]
    fn a_killed_terminal_instance_is_removed_and_the_base_terminal_goes_below_the_editor() {
        let instance = ModuleId::parse("terminal#2").unwrap();
        assert_eq!(reopen(&instance, &[]), Reopen::Remove);
        assert_eq!(reopen(&ModuleId::terminal(), &[]), Reopen::At(Placement::BelowEditor));
    }

    #[test]
    fn a_numbered_chat_view_leaves_the_layout_when_killed() {
        for n in 2..=9 {
            let view = ModuleId::parse(&format!("agent#{n}")).unwrap();
            assert_eq!(reopen(&view, &[]), Reopen::Remove, "{view}");
        }
        assert_eq!(
            reopen(&ModuleId::agent(), &[]),
            Reopen::InPlace,
            "the base view stays, hidden"
        );
    }

    #[test]
    fn the_y_n_for_an_extra_view_names_its_tab() {
        let view = ModuleId::parse("agent#2").unwrap();
        assert_eq!(
            prompt_closing_tab(&view, "agent 2", KillScope::Module, Some("3 refactor"), 0, 0, 0, 0),
            "kill-pane agent 2? closes tab 3 refactor (y/n)"
        );
        assert_eq!(
            prompt_closing_tab(&view, "agent 2", KillScope::Module, Some("3 refactor"), 1, 2, 0, 0),
            "kill-pane agent 2? closes tab 3 refactor, 1 running, 2 queued (y/n)"
        );
        assert_eq!(
            prompt_closing_tab(&view, "agent 2", KillScope::Module, None, 0, 0, 0, 0),
            "kill-pane agent 2? (y/n)",
            "a view whose tab has gone names none"
        );
        assert_eq!(
            prompt_closing_tab(&view, "agent 2", KillScope::Window, Some("3 refactor"), 1, 0, 0, 0),
            "kill-pane agent 2? closes Eitri, 1 running (y/n)",
            "the last module's kill is the window close's question"
        );
        assert_eq!(
            prompt_closing_tab(
                &ModuleId::terminal(),
                "terminal",
                KillScope::Module,
                Some("3 refactor"),
                0,
                0,
                0,
                0
            ),
            "kill-pane terminal? (y/n)",
            "only a chat view closes a tab"
        );
    }

    /// An open rule moves the base terminal's first-launch place, and a kill brings it back there, not
    /// below the editor.
    #[test]
    fn a_killed_terminal_comes_back_where_its_rule_says() {
        let decls = [ModuleDecl {
            id: ModuleId::terminal(),
            placement: Placement::BelowRoot,
        }];
        assert_eq!(reopen(&ModuleId::terminal(), &decls), Reopen::At(Placement::BelowRoot));
        assert_eq!(
            reopen(&ModuleId::parse("terminal#2").unwrap(), &decls),
            Reopen::Remove,
            "a numbered terminal still leaves"
        );
    }
}
