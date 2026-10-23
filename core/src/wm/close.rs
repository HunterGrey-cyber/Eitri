//! Close prompt rules, kill decisions and nvim quit facts.
//!
//! `prefix x` is tmux's `kill-pane`: it ends the module, it does not hide it. Stock tmux binds `x` to
//! `confirm-before -p "kill-pane #P? (y/n)" kill-pane` (checked against a throwaway
//! `tmux -L xprobe -f /dev/null` server, tmux next-3.7), so Eitri asks first too, in the window's one
//! y/n ([`PromptState`]), naming the module where tmux names the pane's number.
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
//!   window closes when nvim exits.
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
//! - **a declined window close after nvim exited on its own**: nvim is gone and the window stays, so
//!   the editor is retired as `prefix x`'s kill of it retires it ([`Reopen::Never`]) -- never left on
//!   screen holding the keys with nothing behind it ([`retire_on_declined_close`]). It is not
//!   restarted in this window (as above); `prefix e` then says so.
//!
//! This file holds the pieces decided without a display or a toolkit: the prompt's text and keys,
//! where each kind of module is left ([`reopen`]), and each single decision of the editor's quit,
//! read from what nvim answered ([`NvimAnswer`]). The flow that strings those decisions together is
//! the shell's `editor_quit` (also display-free, tested against a fake host); the shell's
//! `kill_pane` turns the editor pane's request watch into those facts. Every other module's kill is
//! carried out by [`crate::wm::verbs::modules`]; only the editor's quit is still carried out by the
//! shell's `main.rs`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

pub use crate::layout::KillScope;
use crate::layout::{declared_placement, Layout, ModuleDecl, ModuleId, ModuleKind, Placement, Reopen};

use super::KeyPress;
use crate::keymap::KeyName;

/// How far the window's overlay labels sit from the edge they are drawn over: the toast always, and
/// this prompt at its corner when the top bar is hidden (Immersive). One value, so the two stay aligned.
pub const TOAST_MARGIN: i32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKey {
    Yes,
    No,
    Ignore,
}

/// Only declining the window close's own question runs its decline hook.
pub fn runs_decline_hook(answer: PromptKey, closes_window: bool) -> bool {
    answer == PromptKey::No && closes_window
}

/// A bare modifier waits; either case of y confirms, regardless of its modifiers.
pub fn prompt_key(press: &KeyPress, is_modifier: bool) -> PromptKey {
    if is_modifier {
        PromptKey::Ignore
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

/// The prompt state is separate from its label so callbacks can be checked without a display.
#[derive(Default)]
pub struct PromptState {
    open: Cell<bool>,
    confirmed: Cell<bool>,
    /// What `y` runs; taken when the prompt closes either way.
    on_yes: RefCell<Option<Box<dyn FnOnce()>>>,
    /// The open prompt is the window close's: `y` sets `confirmed` before it closes again.
    closes_window: Cell<bool>,
    /// Runs when the window close's own question is answered with anything but `y`: the window
    /// stays, and what the close was about to end may already be gone.
    on_window_close_declined: RefCell<Option<Rc<dyn Fn()>>>,
    /// The controller's question shows on the same overlay instead: its keys are the controller's to answer.
    model_open: Cell<bool>,
}

impl PromptState {
    /// Whether a yes has confirmed the window close.
    pub fn confirmed(&self) -> bool {
        self.confirmed.get()
    }

    /// Changes the confirmation when a close is handed to or returned from the editor.
    pub fn set_confirmed(&self, confirmed: bool) {
        self.confirmed.set(confirmed);
    }

    /// Registers the callback for a declined window close.
    pub fn connect_window_close_declined(&self, hook: impl Fn() + 'static) {
        *self.on_window_close_declined.borrow_mut() = Some(Rc::new(hook));
    }

    /// Opens the prompt, replacing one already open: `y` runs `on_yes`, and `closes_window` marks it
    /// as the window close's own question. `true` when it replaced the controller's question, which the caller
    /// must withdraw in the controller too: left pending there, a later key would answer a question no longer
    /// on screen.
    #[must_use = "a replaced controller question must be withdrawn in the controller"]
    pub fn open(&self, on_yes: Box<dyn FnOnce()>, closes_window: bool) -> bool {
        *self.on_yes.borrow_mut() = Some(on_yes);
        self.closes_window.set(closes_window);
        self.open.set(true);
        self.model_open.replace(false)
    }

    /// The controller's question shows, replacing this prompt's own if one is open: that one closes unanswered,
    /// running nothing `y` would. When it was the window close's, the window stays as a `no` would have left it,
    /// so the decline hook is returned for the caller to run once nothing is being applied.
    pub fn show_model(&self) -> Option<Rc<dyn Fn()>> {
        self.model_open.set(true);
        let was_open = self.open.replace(false);
        self.on_yes.borrow_mut().take();
        if was_open && self.closes_window.get() {
            self.on_window_close_declined.borrow().clone()
        } else {
            None
        }
    }

    /// The controller's question is answered or gone: `true` when nothing else shows, so the overlay hides.
    pub fn hide_model(&self) -> bool {
        self.model_open.set(false);
        !self.open.get()
    }

    /// Whether the controller's question is the one showing.
    pub fn model_open(&self) -> bool {
        self.model_open.get()
    }

    pub fn is_open(&self) -> bool {
        self.open.get()
    }

    /// `answer` while the prompt is open. `false`, changing nothing, when it is not open or the key
    /// is a bare modifier: the key goes on. Otherwise the prompt closes -- `hide` runs first, since
    /// what runs next may ask again -- and `y` runs what was asked (confirming a window close first),
    /// anything else cancels it and, for the window close's own question only, runs the decline hook.
    pub fn answer(&self, answer: PromptKey, hide: impl FnOnce()) -> bool {
        if !self.open.get() || answer == PromptKey::Ignore {
            return false;
        }
        self.open.set(false);
        hide();
        // Taken out before anything runs: what runs may ask again.
        let on_yes = self.on_yes.borrow_mut().take();
        match answer {
            PromptKey::Yes => {
                if self.closes_window.get() {
                    self.confirmed.set(true);
                }
                if let Some(on_yes) = on_yes {
                    on_yes();
                }
            }
            PromptKey::No | PromptKey::Ignore => {
                if runs_decline_hook(answer, self.closes_window.get()) {
                    // Cloned out first: the hook changes the layout, which may ask again.
                    let declined = self.on_window_close_declined.borrow().clone();
                    if let Some(declined) = declined {
                        declined();
                    }
                }
            }
        }
        true
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
pub fn prompt(id: &ModuleId, title: &str, scope: KillScope, running: usize, queued: usize, jobs: usize) -> String {
    prompt_closing_tab(id, title, scope, None, running, queued, jobs)
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
        }
        ModuleKind::Editor => consequences.push("nvim cannot be reopened in this window".to_string()),
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
    /// Bumped by `main.rs` before each send, so a cancel's own generation can be told apart from a
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

/// What `main.rs`'s `ask_nvim_to_quit` does with the editor before nvim draws its prompt in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reveal {
    /// Not on screen (hidden, zoomed away, or on a tab its container does not draw): shown, which
    /// ends a zoom and, for a tab, chooses it (`main.rs`'s `ShellQuitHost::show_editor`).
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
    /// (`editor_quit`'s `QuitHost::end_nvim`: SIGTERM and stdin closed, SIGKILL by pid after the
    /// patience), and [`not_answering_text`] says so.
    NotAnswering,
    /// Its `:confirm` dialog is up (`nvim_get_mode` says `r?`).
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
        Some(answer) if answer.mode == "r?" => NvimState::Asking,
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

/// Whether a declined window close retires the editor: only once nvim has exited, and only
/// if it is not retired already. A close declined while nvim runs leaves a working editor.
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
        ModuleKind::Editor => Reopen::Never,
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
        assert_eq!(prompt_key(&press(Some(KeyName::Char('y'))), false), PromptKey::Yes);
        assert_eq!(prompt_key(&press(Some(KeyName::Char('Y'))), false), PromptKey::Yes);
        assert_eq!(prompt_key(&press(Some(KeyName::Char('n'))), false), PromptKey::No);
        assert_eq!(prompt_key(&press(None), false), PromptKey::No);
        assert_eq!(
            prompt_key(&press(Some(KeyName::Enter)), false),
            PromptKey::No,
            "Enter is not yes"
        );
        assert_eq!(prompt_key(&press(None), true), PromptKey::Ignore);
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

    /// Answers hide the prompt before running a callback, which may open another prompt.
    #[test]
    fn the_prompts_answers_run_what_they_say() {
        let log: Rc<RefCell<Vec<&'static str>>> = Rc::default();
        let state = PromptState::default();
        {
            let log = log.clone();
            *state.on_window_close_declined.borrow_mut() = Some(Rc::new(move || log.borrow_mut().push("declined")));
        }
        let on_yes = |log: &Rc<RefCell<Vec<&'static str>>>| -> Box<dyn FnOnce()> {
            let log = log.clone();
            Box::new(move || log.borrow_mut().push("yes"))
        };
        let hide = |log: &Rc<RefCell<Vec<&'static str>>>| {
            let log = log.clone();
            move || log.borrow_mut().push("hide")
        };

        assert!(
            !state.answer(PromptKey::No, hide(&log)),
            "no prompt open: the key goes on"
        );
        assert!(log.borrow().is_empty());

        assert!(!state.open(on_yes(&log), true));
        assert!(!state.answer(PromptKey::Ignore, hide(&log)), "a bare modifier waits");
        assert!(state.is_open());
        assert!(state.answer(PromptKey::No, hide(&log)));
        assert_eq!(*log.borrow(), ["hide", "declined"]);
        assert!(!state.confirmed.get() && !state.is_open());

        log.borrow_mut().clear();
        assert!(!state.open(on_yes(&log), true));
        assert!(state.answer(PromptKey::Yes, hide(&log)));
        assert_eq!(*log.borrow(), ["hide", "yes"]);
        assert!(state.confirmed.get(), "the window close's y confirms it");

        log.borrow_mut().clear();
        state.confirmed.set(false);
        assert!(!state.open(on_yes(&log), false));
        assert!(state.answer(PromptKey::No, hide(&log)));
        assert_eq!(*log.borrow(), ["hide"], "prefix x's question declined runs no hook");
        assert!(!state.open(on_yes(&log), false));
        assert!(state.answer(PromptKey::Yes, hide(&log)));
        assert!(!state.confirmed.get(), "and its y confirms no window close");
    }

    /// The overlay holds one question at a time, the newest: the shell's own over the controller's says the
    /// controller's must be withdrawn, and the controller's over the shell's closes the shell's unanswered, handing
    /// back the decline hook only for a window close, since its window stays.
    #[test]
    fn the_newest_question_replaces_the_one_on_the_overlay() {
        let log: Rc<RefCell<Vec<&'static str>>> = Rc::default();
        let state = PromptState::default();
        {
            let log = log.clone();
            state.connect_window_close_declined(move || log.borrow_mut().push("declined"));
        }
        let on_yes = |log: &Rc<RefCell<Vec<&'static str>>>| -> Box<dyn FnOnce()> {
            let log = log.clone();
            Box::new(move || log.borrow_mut().push("yes"))
        };

        assert!(state.show_model().is_none(), "nothing of the shell's was open");
        assert!(state.model_open());
        assert!(
            state.open(on_yes(&log), true),
            "the window close over the controller's question withdraws it"
        );
        assert!(!state.model_open() && state.is_open());
        assert!(
            !state.hide_model(),
            "the controller's going leaves the window close showing"
        );
        assert!(state.is_open());

        let declined = state
            .show_model()
            .expect("a replaced window close stays, as a no leaves it");
        assert!(!state.is_open() && state.model_open());
        assert!(
            !state.answer(PromptKey::Yes, || {}),
            "the replaced question takes no key"
        );
        declined();
        assert_eq!(*log.borrow(), ["declined"], "its y never runs");
        assert!(state.hide_model(), "nothing else shows: the overlay hides");

        log.borrow_mut().clear();
        assert!(!state.open(on_yes(&log), false));
        assert!(
            state.show_model().is_none(),
            "a replaced kill question has no decline hook"
        );
        assert!(log.borrow().is_empty());
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
        assert_eq!(prompt_key(&press, false), PromptKey::Yes);
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
            lent_by: None,
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
    fn the_prompt_is_tmuxs_kill_pane_prompt() {
        assert_eq!(
            prompt(&ModuleId::terminal(), "terminal", KillScope::Module, 0, 0, 0),
            "kill-pane terminal? (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::lua("notes"), "Notes", KillScope::Module, 3, 3, 0),
            "kill-pane Notes? (y/n)"
        );
    }

    #[test]
    fn the_agents_prompt_says_how_many_tabs_are_running_or_queued() {
        let agent = ModuleId::agent();
        assert_eq!(
            prompt(&agent, "agent", KillScope::Module, 0, 0, 0),
            "kill-pane agent? (y/n)"
        );
        assert_eq!(
            prompt(&agent, "agent", KillScope::Module, 1, 0, 0),
            "kill-pane agent? 1 running (y/n)"
        );
        assert_eq!(
            prompt(&agent, "agent", KillScope::Module, 2, 1, 0),
            "kill-pane agent? 2 running, 1 queued (y/n)"
        );
    }

    #[test]
    fn the_editors_prompt_says_it_will_not_come_back() {
        assert_eq!(
            prompt(&ModuleId::editor(), "editor", KillScope::Module, 5, 5, 0),
            "kill-pane editor? nvim cannot be reopened in this window (y/n)"
        );
    }

    /// The last module on screen: the kill closes Eitri, and the one question also carries the
    /// window close's own `N running, M queued` (it is not asked a second time), for every module.
    #[test]
    fn the_last_modules_prompt_says_it_closes_eitri_and_what_is_running() {
        assert_eq!(
            prompt(&ModuleId::editor(), "editor", KillScope::Window, 0, 0, 0),
            "kill-pane editor? closes Eitri (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::editor(), "editor", KillScope::Window, 2, 1, 0),
            "kill-pane editor? closes Eitri, 2 running, 1 queued (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::terminal(), "terminal", KillScope::Window, 1, 0, 0),
            "kill-pane terminal? closes Eitri, 1 running (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::agent(), "agent", KillScope::Window, 0, 3, 0),
            "kill-pane agent? closes Eitri, 3 queued (y/n)"
        );
    }

    /// Closing Eitri ends every terminal's job too, so the last module's kill names them as the
    /// window close does; a module's own kill does not.
    #[test]
    fn the_last_modules_kill_names_terminal_jobs() {
        assert_eq!(
            prompt(&ModuleId::editor(), "editor", KillScope::Window, 0, 0, 2),
            "kill-pane editor? closes Eitri, 2 terminal jobs (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::terminal(), "terminal", KillScope::Window, 1, 0, 1),
            "kill-pane terminal? closes Eitri, 1 running, 1 terminal job (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::terminal(), "terminal", KillScope::Module, 0, 0, 1),
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
        assert_eq!(reopen(&ModuleId::editor(), &decls), Reopen::Never);
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
            prompt_closing_tab(&view, "agent 2", KillScope::Module, Some("3 refactor"), 0, 0, 0),
            "kill-pane agent 2? closes tab 3 refactor (y/n)"
        );
        assert_eq!(
            prompt_closing_tab(&view, "agent 2", KillScope::Module, Some("3 refactor"), 1, 2, 0),
            "kill-pane agent 2? closes tab 3 refactor, 1 running, 2 queued (y/n)"
        );
        assert_eq!(
            prompt_closing_tab(&view, "agent 2", KillScope::Module, None, 0, 0, 0),
            "kill-pane agent 2? (y/n)",
            "a view whose tab has gone names none"
        );
        assert_eq!(
            prompt_closing_tab(&view, "agent 2", KillScope::Window, Some("3 refactor"), 1, 0, 0),
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
