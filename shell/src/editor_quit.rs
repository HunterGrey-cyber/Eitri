//! Coordinates editor quits from `prefix x` and every window close, including nvim's exit,
//! cancellation, and a declined close. Window closes ask nvim to confirm unsaved changes.
//!
//! [`EditorQuitting`] owns the decisions and their order so they can be tested with a fake
//! [`QuitHost`]. `main.rs` implements the host over the pane, layout, grid, confirmation prompt,
//! and toast. The GTK calls in that layer still need GUI verification.
//!
//! The quit path uses `:confirm qall`, never `:qa!`. If nvim stops answering or is unreachable,
//! [`QuitHost::end_nvim`] sends SIGTERM and closes stdin, then sends SIGKILL by pid after the
//! timeout. SIGTERM goes first because nvim may still be in its prompt. Closing stdin lets nvim
//! exit while keeping swap files. The editor host's shutdown sends no quit command.
//! Close and retirement actions run once, after the pane reports the process gone.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use eitri_core::layout::{KillScope, LayoutError};
use neovide_editor::CallWatch;

use crate::kill_pane::{self, Asked, EditorQuit, QuitInFlight, QuitRequest, Reveal};

/// What [`EditorQuitting::ask`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AskedNvim {
    /// nvim was sent `:confirm qall` (or had exited already); its exit, or a cancel, answers.
    Asked,
    /// nvim is being ended ([`QuitHost::end_nvim`]): its loop had not answered the quit for
    /// `kill_pane::NVIM_QUIT_PATIENCE`, or it is out of reach. Its exit closes what the quit was for.
    Ending,
}

/// The launcher exited without `exec` while nvim's own process still runs. The fork can
/// no longer send it keys; only ending the process can reach it. The requested action
/// waits for nvim's exit.
pub(crate) const UNREACHABLE_NVIM: &str = "nvim's launcher exited and nvim still runs where the editor can no \
     longer reach it \u{2014} close the window or kill-pane the editor to end it (its swap file keeps your changes)";

/// Text for a retired editor: nvim has ended and cannot restart in this window.
/// The cause is unknown: `prefix x`, `:qa`, and a crash can all reach this path.
/// A relaunch shows the editor again, so the message explains how to get it back
/// without claiming to know why it exited.
pub(crate) const RETIRED_EDITOR_TEXT: &str =
    "nvim exited and cannot restart in this window \u{2014} relaunch eitri to get the editor back";

/// What nvim's quit-cancelled letter did ([`EditorQuitting::cancelled`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cancel {
    /// It was the quit in flight: nothing is in flight now.
    Cleared,
    /// It was the window close a kill of the editor carries ([`kill_pane::Asked::lent_by`]): the
    /// close is taken back out, and nvim's exit retires the editor only.
    CloseWithdrawn,
    /// Neither: a letter from a quit already superseded, or with nothing in flight.
    Ignored,
}

/// What the window's close request does now ([`EditorQuitting::window_close`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowClose {
    /// The close waits: for the y/n, for nvim's own prompt, for a killed nvim to go -- or it was
    /// refused and said why.
    Wait,
    /// nvim is gone or never ran: the window closes (the caller saves, shuts down and proceeds).
    Proceed,
}

/// Everything the flow does to the world, one call each. `main.rs` implements it; the tests fake it.
pub(crate) trait QuitHost {
    /// nvim is up and has not exited (`NeovideEditorPane::is_running`).
    fn nvim_running(&self) -> bool;
    /// nvim ran and has exited (`NeovideEditorPane::nvim_exited`).
    fn nvim_exited(&self) -> bool;
    /// The fork reports nvim gone, and nvim's own process is still alive
    /// (`NeovideEditorPane::nvim_exit_pending`): its exit is reported once the process is gone.
    fn nvim_exit_pending(&self) -> bool;
    /// The quit request's watch (`NeovideEditorPane::watched_call`).
    fn watched_call(&self) -> Option<CallWatch>;
    /// The quit request's watch once its worker has published how it ended, waited for briefly
    /// (`NeovideEditorPane::settled_watched_call`). Read only at nvim's exit, when nvim-rs has
    /// resolved the request already: for one still outstanding it blocks the GTK thread for the
    /// whole bound.
    fn settled_watch(&self) -> Option<CallWatch>;
    /// Has nvim run `lua` as an RPC request (`NeovideEditorPane::exec_lua_watched`).
    fn send_quit(&self, lua: &str) -> bool;
    /// Ends nvim without `:qa!` (`NeovideEditorPane::end_nvim`): stdin closed and SIGTERM (SIGTERM
    /// first while the quit's request has not come back), then SIGKILL by pid on the pane's
    /// schedule. Idempotent.
    fn end_nvim(&self) -> bool;
    /// `kill_pane::reveal_for_prompt` over the layout as it stands.
    fn reveal(&self) -> Reveal;
    /// The editor is retired for this window (`Layout::is_gone`).
    fn editor_gone(&self) -> bool;
    /// Shows the editor, ending a zoom that hides it.
    fn show_editor(&self) -> Result<(), LayoutError>;
    /// Gives the editor the keys.
    fn focus_editor(&self);
    /// Hides the editor, the keys going to its neighbour.
    fn hide_editor(&self) -> Result<(), LayoutError>;
    /// Closes the editor for the rest of this window (`Reopen::Never`) and releases the pane.
    fn retire_editor(&self) -> Result<(), LayoutError>;
    /// A kill of an editor that never ran nvim: hidden in place, so it can show again.
    fn kill_editor_in_place(&self) -> Result<(), LayoutError>;
    /// Shows the chat (the decline path, when the editor was the last module on screen).
    fn show_chat(&self) -> Result<(), LayoutError>;
    /// The window close's y/n was answered `y` (`ClosePrompt::confirmed`).
    fn close_confirmed(&self) -> bool;
    /// The window close's prompt as it stands now (`eitri_core::tabs::window_close_prompt`).
    fn window_close_prompt(&self) -> Option<String>;
    /// Asks the window close's y/n.
    fn ask_to_close_window(&self, text: &str);
    /// Takes the window close's `y` back (`ClosePrompt::withdraw_confirmation`).
    fn withdraw_close_confirmation(&self);
    /// Closes the window as a `y` to its own question would (`ClosePrompt::close_window_confirmed`).
    fn close_window_confirmed(&self);
    /// Closes the window through its ordinary path, which asks what it asks.
    fn close_window(&self);
    /// A refusal: the app name's flash and the toast.
    fn refuse(&self, text: &str);
    /// The toast alone.
    fn toast(&self, text: &str);
    /// Calls `tick` once a second for as long as it returns `true`.
    fn every_second(&self, tick: Box<dyn FnMut() -> bool>);
}

/// The quit of the editor in flight, and the flow around it.
pub(crate) struct EditorQuitting {
    host: Rc<dyn QuitHost>,
    /// Set once nvim has been asked to quit (or had exited just before it could be), with that
    /// quit's own generation, and says what nvim's exit then does. Cleared by a cancel -- nvim's
    /// letter on the pane-switch socket, or the request's own answer (C1) -- and taken by the exit.
    in_flight: RefCell<QuitInFlight>,
    generation: Cell<u32>,
}

impl EditorQuitting {
    pub(crate) fn new(host: Rc<dyn QuitHost>) -> Rc<EditorQuitting> {
        Rc::new(EditorQuitting {
            host,
            in_flight: RefCell::new(None),
            generation: Cell::new(0),
        })
    }

    /// Asks nvim to `:confirm qall` for `quit`. Nvim prompts for unsaved buffers, and only its
    /// exit closes anything ([`EditorQuitting::nvim_exited`]). Use RPC: typed keys could be
    /// swallowed by a pending key read after `f` or inside `getchar()`. Show and focus a hidden
    /// or zoomed-away editor first, since its prompt is drawn there. A second request is refused
    /// with text describing nvim's state while the first is pending. Once nvim's loop has not
    /// answered the pane's watch for `NVIM_QUIT_PATIENCE`, a second request kills it and keeps
    /// its swap files. A toast explains this when the timeout is reached.
    pub(crate) fn ask(self: &Rc<Self>, quit: EditorQuit) -> Result<AskedNvim, String> {
        let host = &self.host;
        if host.nvim_exit_pending() {
            return Ok(self.end_pending(quit));
        }
        let reveal = host.reveal();
        if !host.nvim_running() {
            // nvim exited just now: nothing to ask, and its exit -- seen on the pane's next tick --
            // does what this request was for, with what was still in flight. An editor off
            // screen is brought on screen, so that tick runs and nothing waits for a second close.
            if reveal.show {
                if let Err(err) = host.show_editor() {
                    println!("[modules] editor: nvim exited, and the editor could not be shown ({err})");
                }
            }
            self.begin_unsent(quit, reveal.rehide);
            return Ok(AskedNvim::Asked);
        }
        let watch = host.watched_call();
        let request = kill_pane::quit_request(self.in_flight.borrow().as_ref(), watch.as_ref(), Instant::now());
        match request {
            QuitRequest::Ask => {}
            QuitRequest::Kill => {
                self.join(quit);
                println!("[modules] editor: nvim has not answered since it was asked to quit; ending it");
                host.end_nvim();
                return Ok(AskedNvim::Ending);
            }
            QuitRequest::AnswerThere(state) => {
                self.join(quit);
                let _ = host.show_editor();
                host.focus_editor();
                return Err(kill_pane::answer_there_text(state).into());
            }
        }
        if reveal.show {
            host.show_editor().map_err(|e| e.to_string())?;
        }
        host.focus_editor();
        let g = self.generation.get().wrapping_add(1);
        self.generation.set(g);
        *self.in_flight.borrow_mut() = Some(Asked {
            generation: g,
            quit,
            revealed: reveal.rehide,
            sent: true,
            lent_by: None,
        });
        if !host.send_quit(&kill_pane::editor_quit_lua(g)) {
            self.in_flight.borrow_mut().take();
            return Err("nvim could not be asked to quit \u{2014} quit it in the editor (:qa)".into());
        }
        println!(
            "[modules] editor: asked nvim to :confirm qall (quit {g}, shown for it: {}, hidden after: {})",
            reveal.show, reveal.rehide
        );
        // Once a second, for as long as this quit is the one in flight: the toast when nvim's loop
        // stops answering (at once, or after its prompt was answered and something hung), and the
        // quit cleared once its request came back answered (C1).
        let me = self.clone();
        host.every_second(Box::new(move || me.poll(g, Instant::now())));
        Ok(AskedNvim::Asked)
    }

    /// Handles a request while nvim outlives its launcher and is out of reach of the fork.
    /// Only ending the process can reach it. The request joins the quit in flight and waits for
    /// the pane to report that nvim's process is gone. Repeats start no timer or extra ending:
    /// the pane's ending is idempotent, and only the exit performs the requested action.
    fn end_pending(&self, quit: EditorQuit) -> AskedNvim {
        // Brought on screen if it is not, so the pane's tick runs and sees the exit when it comes
        // (the same reason `ask`'s exited-just-now branch shows it); hidden again after.
        let reveal = self.host.reveal();
        if reveal.show {
            if let Err(err) = self.host.show_editor() {
                println!("[modules] editor: nvim is out of reach, and the editor could not be shown ({err})");
            }
        }
        self.begin_unsent(quit, reveal.rehide);
        self.host.end_nvim();
        println!("[modules] editor: nvim is out of reach; it is being ended, and its exit does what was asked");
        AskedNvim::Ending
    }

    /// A request that sends nothing to nvim gets a fresh in-flight entry and generation, whether
    /// nvim has just exited or is out of reach. Reusing an older request's generation and `sent`
    /// state could let a delayed cancellation erase the new request or close the wrong scope.
    /// An unanswered older request is merged through `kill_pane::merge`, with a window close taking
    /// precedence over an editor kill. An answered request and its confirmation are dropped.
    ///
    /// A kill that carries a window close must still allow that close to be withdrawn.
    /// [`Asked::lent_by`] identifies the sent close even under the kill's new generation.
    /// Its cancellation letter, an answer read by a later request, or an answer read at nvim's exit
    /// removes that close regardless of how late it arrives.
    /// Nothing here waits for the watch to settle on the GTK thread: a bounded wait could freeze
    /// the window while the dialog is open and still miss an answer published after the bound.
    fn begin_unsent(&self, quit: EditorQuit, rehide: bool) {
        // The last request nvim was sent came back with its answer: its Lua returned, so nvim did
        // not quit. Every send replaces the watch, and nothing is sent once nvim is out of reach
        // or gone, so the watch is the request of the sent quit in flight, or of the close a kill
        // in flight carries.
        let answered = self.host.watched_call().is_some_and(|w| w.done && w.returned);
        let older = self.in_flight.borrow_mut().take().and_then(|mut asked| {
            if answered {
                if asked.sent {
                    return None;
                }
                if let Some(close) = asked.lent_by {
                    println!(
                        "[modules] editor: quit {close} was answered without quitting; quit {} kills the editor only",
                        asked.generation
                    );
                    asked.withdraw_lent_close();
                }
            }
            Some(asked)
        });
        // A kill joining a window close carries it, and remembers whose it is: the close's own
        // generation when it was sent, or the one an unsent quit already carried. A window close
        // of its own, sent nothing, has nothing nvim can answer, and joins as it is.
        let lent_by = match (&older, &quit) {
            (Some(older), EditorQuit::Module) if matches!(older.quit, EditorQuit::Window { .. }) => {
                if older.sent {
                    Some(older.generation)
                } else {
                    older.lent_by
                }
            }
            _ => None,
        };
        let mut in_flight = self.in_flight.borrow_mut();
        let g = self.generation.get().wrapping_add(1);
        self.generation.set(g);
        *in_flight = Some(Asked {
            generation: g,
            quit: match &older {
                Some(older) => kill_pane::merge(&older.quit, quit),
                None => quit,
            },
            revealed: older.as_ref().is_some_and(|o| o.revealed) || rehide,
            sent: false,
            lent_by,
        });
    }

    /// A second request joining the quit in flight.
    fn join(&self, quit: EditorQuit) {
        if let Some(asked) = self.in_flight.borrow_mut().as_mut() {
            asked.quit = kill_pane::merge(&asked.quit, quit);
        }
    }

    /// The once-a-second check on quit `g` ([`EditorQuitting::ask`]); `false` ends it.
    pub(crate) fn poll(&self, g: u32, now: Instant) -> bool {
        let quit = self
            .in_flight
            .borrow()
            .as_ref()
            .filter(|asked| asked.generation == g)
            .map(|asked| asked.quit.clone());
        let Some(quit) = quit.filter(|_| self.host.nvim_running()) else {
            return false;
        };
        let watch = self.host.watched_call();
        match kill_pane::nvim_state(watch.as_ref(), now) {
            kill_pane::NvimState::NotAnswering => {
                println!("[modules] editor: nvim has not answered since quit {g} was sent");
                self.host.toast(kill_pane::not_answering_text(&quit));
                false
            }
            kill_pane::NvimState::Returned => {
                self.settle(watch.as_ref());
                false
            }
            _ => true,
        }
    }

    /// Clears a sent quit unless its request is known to have ended with nvim gone. If the Lua
    /// returned, the user cancelled, so neither the quit nor its `y` may authorize a later exit,
    /// even without a cancel letter from the pane-switch shim. At exit,
    /// [`EditorQuitting::nvim_exited`] reads the settled watch. A request still outstanding after
    /// that wait is cleared too, so the close asks again rather than trusting an old `y`.
    /// A quit that sent nothing because nvim had already exited is kept. Returns the cleared quit.
    fn settle(&self, watch: Option<&CallWatch>) -> Option<Asked> {
        let mut in_flight = self.in_flight.borrow_mut();
        if !in_flight.as_ref().is_some_and(|asked| asked.sent) {
            return None;
        }
        let why = match watch {
            // Ended without nvim's answer: nvim went away, which is the quit happening.
            Some(w) if w.done && !w.returned => return None,
            Some(w) if w.done => "answered it without quitting",
            _ => "has not said how it ended",
        };
        let cleared = in_flight.take();
        if let Some(asked) = &cleared {
            println!(
                "[modules] editor: quit {}'s request {why}; nothing is in flight",
                asked.generation
            );
        }
        cleared
    }

    /// At nvim's exit, [`EditorQuitting::settle`]'s rule for the window close a kill in flight
    /// carries ([`Asked::lent_by`]), read off that close's own request: ended without nvim's answer
    /// -- nvim went away with the close's prompt up -- the close stands, and wins over the kill as
    /// it always has; answered, or still not known after the wait, it is taken back out, so an old
    /// `y` is never trusted and nvim's exit retires the editor, which is what the kill asked.
    fn settle_lent_close(&self, watch: Option<&CallWatch>) {
        let mut in_flight = self.in_flight.borrow_mut();
        let Some(asked) = in_flight.as_mut() else {
            return;
        };
        let Some(close) = asked.lent_by else {
            return;
        };
        let why = match watch {
            Some(w) if w.done && !w.returned => return,
            Some(w) if w.done => "was answered without quitting",
            _ => "has not said how it ended",
        };
        println!(
            "[modules] editor: the window close quit {} carries (quit {close}) {why}; it kills the editor only",
            asked.generation
        );
        asked.withdraw_lent_close();
    }

    /// nvim's letter on the pane-switch socket: quit `g` was cancelled. The quit in flight if it is
    /// `g`; the window close a kill in flight carries if that close is `g` ([`Asked::lent_by`]),
    /// which leaves the kill.
    pub(crate) fn cancelled(&self, g: u32) -> Cancel {
        let mut in_flight = self.in_flight.borrow_mut();
        if kill_pane::clear_if_cancelled(&mut in_flight, g).is_some() {
            return Cancel::Cleared;
        }
        match in_flight.as_mut().filter(|asked| asked.lent_by == Some(g)) {
            Some(asked) => {
                asked.withdraw_lent_close();
                Cancel::CloseWithdrawn
            }
            None => Cancel::Ignored,
        }
    }

    /// nvim exited without the host asking the pane to shut down: whatever was asked of it is
    /// answered. A window close closes the window without asking what was already asked (the editor
    /// hidden again first if the close showed it for nvim's prompt, so the arrangement saved on
    /// close is the user's); `prefix x` retires the editor (one that joined an older window close
    /// closes the window only if that close's request ended with nvim gone,
    /// [`EditorQuitting::settle_lent_close`]); an exit nobody asked for -- or one after a quit nvim
    /// already answered without quitting -- asks the window to close, which asks whatever the close
    /// asks.
    pub(crate) fn nvim_exited(&self) {
        // The request's answer may still be on its way to the watch.
        let watch = self.host.settled_watch();
        if self.settle(watch.as_ref()).is_some() {
            println!("[modules] editor: nvim exited, and the quit in flight is not what ended it");
        }
        self.settle_lent_close(watch.as_ref());
        let asked = self.in_flight.borrow_mut().take();
        match asked {
            Some(Asked {
                quit: EditorQuit::Window { confirmed },
                revealed,
                ..
            }) => {
                if revealed {
                    if let Err(err) = self.host.hide_editor() {
                        println!("[modules] editor: shown for nvim's prompt, and stays ({err})");
                    }
                }
                self.close_after(confirmed);
            }
            Some(Asked {
                quit: EditorQuit::Module,
                ..
            }) => match self.host.retire_editor() {
                Ok(()) => {}
                Err(err) => {
                    println!("[modules] editor: nvim quit and could not be closed ({err}); closing the window");
                    self.host.close_window();
                }
            },
            None => self.host.close_window(),
        }
    }

    /// The fork reports nvim exited and nvim's own process lives (`NeovideEditorPane::
    /// on_nvim_unreachable`): said once, since nothing else on screen explains an editor that takes
    /// no keys. Its exit, when it comes, is reported as any exit.
    pub(crate) fn nvim_unreachable(&self) {
        println!("[modules] editor: nvim outlived the fork's report of its exit");
        self.host.toast(UNREACHABLE_NVIM);
    }

    /// The window close once what it would ask was already asked (`confirmed`, the window close's
    /// prompt as it stood then): no second question unless what is running changed meanwhile
    /// (`kill_pane::close_is_confirmed`).
    pub(crate) fn close_after(&self, confirmed: Option<String>) {
        let now = self.host.window_close_prompt();
        if kill_pane::close_is_confirmed(confirmed.as_deref(), now.as_deref()) {
            println!("[window] closing: what the close would ask was already answered");
            self.host.close_window_confirmed();
        } else {
            println!("[window] closing; what is running changed since it was asked, so the close asks again");
            self.host.close_window();
        }
    }

    /// Handles window close requests from the top bar, compositor, and other routes. Ask y/n
    /// about a running tab first, then let nvim decide through `:confirm qall`. The window
    /// closes when nvim exits. Return `WindowClose::Proceed` only when nvim is gone or never ran,
    /// since the caller's shutdown runs the fork's `:qa!`.
    pub(crate) fn window_close(self: &Rc<Self>) -> WindowClose {
        let host = &self.host;
        if !host.close_confirmed() {
            if let Some(text) = host.window_close_prompt() {
                host.ask_to_close_window(&text);
                return WindowClose::Wait;
            }
        }
        // Proceed only once nvim's own process is gone, so the window cannot close while it
        // may still be writing. End a process that outlived the fork's exit report instead.
        if !host.nvim_running() && !host.nvim_exit_pending() {
            return WindowClose::Proceed;
        }
        // The `y` now travels with `EditorQuit::Window`. Nvim's exit closes the window through
        // `close_after`, which asks again if what is running has changed. Clear the host's
        // confirmation so that later close cannot skip the question.
        let confirmed = host.window_close_prompt();
        host.withdraw_close_confirmation();
        match self.ask(EditorQuit::Window { confirmed }) {
            Ok(AskedNvim::Asked) => {
                println!("[window] close: nvim decides; the window closes when it exits");
                WindowClose::Wait
            }
            Ok(AskedNvim::Ending) => {
                println!("[window] close: nvim is being ended; the window closes when it is gone");
                WindowClose::Wait
            }
            Err(refusal) => {
                println!("[window] close: {refusal}");
                host.refuse(&refusal);
                WindowClose::Wait
            }
        }
    }

    /// `prefix x` on the editor, once its y/n was answered: `scope` from `eitri_core::layout::
    /// can_kill`, `confirmed` the window close's prompt as it stood when `x` asked.
    pub(crate) fn kill_editor(self: &Rc<Self>, scope: KillScope, confirmed: Option<String>) -> Result<(), String> {
        let host = &self.host;
        match scope {
            KillScope::Window if host.nvim_running() || host.nvim_exit_pending() => {
                // nvim first, as for any kill of the editor: its exit closes the window.
                self.ask(EditorQuit::Window { confirmed })?;
                println!("[modules] editor: the last module; nvim decides, then the window closes");
            }
            KillScope::Window => self.close_after(confirmed),
            // nvim decides: its own prompt for unsaved buffers, and only its exit closes the module.
            KillScope::Module if host.nvim_running() || host.nvim_exit_pending() => {
                self.ask(EditorQuit::Module)?;
            }
            // Nvim already exited but the pane has not been released. A `:confirm qall` sent now
            // would never be answered, so retire the editor as its exit would have.
            KillScope::Module if host.nvim_exited() => host.retire_editor().map_err(|e| e.to_string())?,
            // No nvim to quit (never started, or it failed to): nothing ends, and it can show again.
            KillScope::Module => host.kill_editor_in_place().map_err(|e| e.to_string())?,
        }
        Ok(())
    }

    /// Handles a declined window close. If nvim already exited on its own, retire the editor
    /// as `prefix x` would, rather than leaving a dead editor holding the keys. The declined
    /// close concerned the running chat, so show a hidden chat first if the editor was the last
    /// visible module.
    pub(crate) fn window_close_declined(&self) {
        let host = &self.host;
        if !kill_pane::retire_on_declined_close(host.nvim_exited(), host.editor_gone()) {
            return;
        }
        let retired = match host.retire_editor() {
            Err(LayoutError::LastVisible(_)) => host.show_chat().and_then(|()| host.retire_editor()),
            other => other,
        };
        match retired {
            Ok(()) => {
                println!("[window] close declined after nvim exited; the editor is retired");
                host.toast(RETIRED_EDITOR_TEXT);
            }
            Err(err) => eprintln!("[window] close declined after nvim exited; the editor could not be retired: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::layout::ModuleId;
    use std::time::Duration;

    /// A fake world: what the flow asks is answered from these cells, and what it does is logged.
    #[derive(Default)]
    struct Fake {
        running: Cell<bool>,
        exited: Cell<bool>,
        /// The fork reports nvim exited, and nvim's process lives (a launcher exited without it).
        pending: Cell<bool>,
        /// What the watch reads once its worker has published (`None`: the watch as it is).
        settled: RefCell<Option<CallWatch>>,
        /// How often the flow waited for that (on the GTK thread, up to the pane's `SETTLE_WAIT`).
        settle_waits: Cell<u32>,
        gone: Cell<bool>,
        reveal: Cell<Option<Reveal>>,
        watch: RefCell<Option<CallWatch>>,
        send_ok: Cell<bool>,
        confirmed: Cell<bool>,
        prompt: RefCell<Option<String>>,
        editor_last_visible: Cell<bool>,
        log: RefCell<Vec<String>>,
        ticks: RefCell<Vec<Box<dyn FnMut() -> bool>>>,
    }

    impl Fake {
        fn new() -> Rc<Fake> {
            let fake = Rc::new(Fake::default());
            fake.running.set(true);
            fake.send_ok.set(true);
            fake
        }
        fn did(&self, what: impl Into<String>) {
            self.log.borrow_mut().push(what.into());
        }
        fn log(&self) -> Vec<String> {
            self.log.borrow().clone()
        }
        fn clear(&self) {
            self.log.borrow_mut().clear();
        }
        /// The request's watch as nvim answered it.
        fn answer(&self, done: bool, returned: bool, mode: &str) {
            let now = Instant::now();
            *self.watch.borrow_mut() = Some(CallWatch {
                sent_at: now,
                done,
                returned,
                last_answer: Some(neovide_editor::NvimMode {
                    at: now,
                    mode: mode.into(),
                    blocking: false,
                }),
            });
        }
        /// Runs every once-a-second check once; the ones that ended are dropped.
        fn tick(&self) {
            let mut ticks = std::mem::take(&mut *self.ticks.borrow_mut());
            ticks.retain_mut(|tick| tick());
            self.ticks.borrow_mut().extend(ticks);
        }
    }

    impl QuitHost for Fake {
        fn nvim_running(&self) -> bool {
            self.running.get()
        }
        fn nvim_exited(&self) -> bool {
            self.exited.get()
        }
        fn nvim_exit_pending(&self) -> bool {
            self.pending.get()
        }
        fn watched_call(&self) -> Option<CallWatch> {
            self.watch.borrow().clone()
        }
        /// As nvim-rs settles a request: once nvim's process is gone its IO loop has ended, so a
        /// request still outstanding comes back unanswered -- unless a test says otherwise.
        fn settled_watch(&self) -> Option<CallWatch> {
            self.settle_waits.set(self.settle_waits.get() + 1);
            if let Some(settled) = self.settled.borrow().clone() {
                return Some(settled);
            }
            let gone = !self.running.get() && !self.pending.get();
            self.watch.borrow().clone().map(|watch| match watch {
                CallWatch { done: false, .. } if gone => CallWatch {
                    done: true,
                    returned: false,
                    ..watch
                },
                watch => watch,
            })
        }
        fn send_quit(&self, lua: &str) -> bool {
            assert!(lua.contains("confirm qall"), "{lua}");
            self.did("send_quit");
            // A new request: nvim has not answered it yet.
            *self.watch.borrow_mut() = Some(CallWatch {
                sent_at: Instant::now(),
                done: false,
                returned: false,
                last_answer: None,
            });
            self.send_ok.get()
        }
        fn end_nvim(&self) -> bool {
            self.did("end_nvim");
            true
        }
        fn reveal(&self) -> Reveal {
            self.reveal.get().unwrap_or(Reveal {
                show: false,
                rehide: false,
            })
        }
        fn editor_gone(&self) -> bool {
            self.gone.get()
        }
        fn show_editor(&self) -> Result<(), LayoutError> {
            self.did("show_editor");
            Ok(())
        }
        fn focus_editor(&self) {
            self.did("focus_editor");
        }
        fn hide_editor(&self) -> Result<(), LayoutError> {
            self.did("hide_editor");
            Ok(())
        }
        fn retire_editor(&self) -> Result<(), LayoutError> {
            if self.editor_last_visible.get() {
                self.did("retire_editor refused");
                return Err(LayoutError::LastVisible(ModuleId::editor()));
            }
            self.did("retire_editor");
            self.gone.set(true);
            Ok(())
        }
        fn kill_editor_in_place(&self) -> Result<(), LayoutError> {
            self.did("kill_editor_in_place");
            Ok(())
        }
        fn show_chat(&self) -> Result<(), LayoutError> {
            self.did("show_chat");
            self.editor_last_visible.set(false);
            Ok(())
        }
        fn close_confirmed(&self) -> bool {
            self.confirmed.get()
        }
        fn window_close_prompt(&self) -> Option<String> {
            self.prompt.borrow().clone()
        }
        fn ask_to_close_window(&self, text: &str) {
            self.did(format!("ask: {text}"));
        }
        fn withdraw_close_confirmation(&self) {
            self.did("withdraw");
            self.confirmed.set(false);
        }
        fn close_window_confirmed(&self) {
            self.did("close_window_confirmed");
        }
        fn close_window(&self) {
            self.did("close_window");
        }
        fn refuse(&self, text: &str) {
            self.did(format!("refuse: {text}"));
        }
        fn toast(&self, text: &str) {
            self.did(format!("toast: {text}"));
        }
        fn every_second(&self, tick: Box<dyn FnMut() -> bool>) {
            self.ticks.borrow_mut().push(tick);
        }
    }

    fn flow(fake: &Rc<Fake>) -> Rc<EditorQuitting> {
        EditorQuitting::new(fake.clone())
    }

    const RUNNING: &str = "close window? 1 running (y/n)";

    /// A window close with nvim running hands the quit to nvim rather than proceeding to
    /// shutdown, which runs the fork's `:qa!`. Withdraw the window's `y` first, then focus
    /// the editor so the user can answer nvim's prompt.
    #[test]
    fn a_window_close_asks_nvim_and_takes_its_y_back() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        fake.confirmed.set(true);
        *fake.prompt.borrow_mut() = Some(RUNNING.into());
        assert_eq!(quitting.window_close(), WindowClose::Wait);
        assert_eq!(fake.log(), ["withdraw", "focus_editor", "send_quit"]);
        assert!(!fake.confirmed.get());
        assert_eq!(
            quitting.in_flight.borrow().as_ref().map(|a| a.quit.clone()),
            Some(EditorQuit::Window {
                confirmed: Some(RUNNING.into())
            }),
            "the y travels with the quit instead"
        );
    }

    /// Ask about a running tab before asking nvim to quit.
    #[test]
    fn a_running_tab_is_asked_about_before_nvim() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        *fake.prompt.borrow_mut() = Some(RUNNING.into());
        assert_eq!(quitting.window_close(), WindowClose::Wait);
        assert_eq!(fake.log(), [format!("ask: {RUNNING}")]);
    }

    /// Only an nvim that is gone, or never ran, lets the close proceed to the shutdown.
    #[test]
    fn the_close_proceeds_only_once_nvim_is_gone() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        fake.running.set(false);
        assert_eq!(quitting.window_close(), WindowClose::Proceed);
        assert!(fake.log().is_empty());
    }

    /// Show an off-screen editor before focusing it and sending the quit, so nvim's prompt
    /// is visible. If the editor was hidden, hide it again when nvim exits.
    #[test]
    fn an_editor_off_screen_is_shown_for_the_prompt_and_hidden_again_after() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        fake.reveal.set(Some(Reveal {
            show: true,
            rehide: true,
        }));
        assert_eq!(quitting.window_close(), WindowClose::Wait);
        assert_eq!(fake.log(), ["withdraw", "show_editor", "focus_editor", "send_quit"]);
        fake.clear();
        fake.running.set(false);
        quitting.nvim_exited();
        assert_eq!(fake.log(), ["hide_editor", "close_window_confirmed"]);
    }

    /// The window closes when nvim exits, without asking again what was already asked -- unless
    /// what is running changed meanwhile.
    #[test]
    fn nvims_exit_closes_the_window_and_asks_again_only_if_something_changed() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        *fake.prompt.borrow_mut() = Some(RUNNING.into());
        fake.confirmed.set(true);
        quitting.window_close();
        fake.clear();
        fake.running.set(false);
        quitting.nvim_exited();
        assert_eq!(fake.log(), ["close_window_confirmed"]);

        let quitting = flow(&fake);
        fake.running.set(true);
        fake.confirmed.set(true);
        quitting.window_close();
        fake.clear();
        *fake.prompt.borrow_mut() = Some("close window? 2 running (y/n)".into());
        fake.running.set(false);
        quitting.nvim_exited();
        assert_eq!(fake.log(), ["close_window"]);
    }

    /// A window close confirmed with `y` is then cancelled in nvim's prompt. The Lua returns
    /// without nvim quitting, even if the shim sends no cancel letter. A later `:qa` must ask
    /// about a still-running tab again rather than closing the window on the old `y`.
    #[test]
    fn a_cancelled_close_does_not_lend_its_y_to_a_later_exit() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        *fake.prompt.borrow_mut() = Some(RUNNING.into());
        fake.confirmed.set(true);
        quitting.window_close();
        // The user cancels in nvim: the request comes back with nvim's answer. No letter arrives.
        fake.answer(true, true, "n");
        fake.clear();
        // Later, `:qa` in the editor, the tab still running.
        fake.running.set(false);
        quitting.nvim_exited();
        assert_eq!(fake.log(), ["close_window"], "the ordinary close, which asks again");
    }

    /// C1, the once-a-second check's half: the answered request clears the quit, so nothing is in
    /// flight any more -- a later `prefix x`'s exit retires the editor, not a window close.
    #[test]
    fn the_check_clears_a_quit_nvim_answered_without_quitting() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        quitting.window_close();
        assert_eq!(fake.ticks.borrow().len(), 1);
        fake.answer(true, true, "n");
        fake.tick();
        assert!(quitting.in_flight.borrow().is_none());
        assert!(fake.ticks.borrow().is_empty(), "and the check ends");
    }

    /// The other side of C1: a request that ended because nvim went away (no answer) is nvim's quit
    /// happening, and its exit closes the window without asking again.
    #[test]
    fn an_exit_that_ends_the_request_is_the_quit() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        *fake.prompt.borrow_mut() = Some(RUNNING.into());
        fake.confirmed.set(true);
        quitting.window_close();
        fake.answer(true, false, "n");
        fake.tick();
        fake.clear();
        fake.running.set(false);
        quitting.nvim_exited();
        assert_eq!(fake.log(), ["close_window_confirmed"]);
    }

    /// The shim's letter clears the quit too; a letter for another quit changes nothing.
    #[test]
    fn a_cancel_letter_clears_only_its_own_quit() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        quitting.window_close();
        let g = quitting.in_flight.borrow().as_ref().unwrap().generation;
        assert_eq!(quitting.cancelled(g + 1), Cancel::Ignored);
        assert_eq!(quitting.cancelled(g), Cancel::Cleared);
        fake.running.set(false);
        fake.clear();
        quitting.nvim_exited();
        assert_eq!(fake.log(), ["close_window"]);
    }

    /// If nvim's loop stops answering, a second close ends it and waits for its exit.
    /// Ending closes stdin, sends SIGTERM, then SIGKILL by pid on the pane's schedule, keeping
    /// the swap file. No fork quit or shutdown runs first. Nvim's exit closes the window
    /// without asking again.
    #[test]
    fn a_second_close_ends_an_nvim_that_stopped_answering_and_waits_for_it() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        quitting.window_close();
        // Not answered since the send, for longer than the patience.
        *fake.watch.borrow_mut() = Some(CallWatch {
            sent_at: Instant::now() - kill_pane::NVIM_QUIT_PATIENCE - Duration::from_secs(1),
            done: false,
            returned: false,
            last_answer: None,
        });
        fake.tick();
        assert!(fake
            .log()
            .iter()
            .any(|l| l.starts_with("toast: nvim is not responding")));
        fake.clear();
        assert_eq!(
            quitting.window_close(),
            WindowClose::Wait,
            "never Proceed while nvim lives"
        );
        assert_eq!(fake.log(), ["withdraw", "end_nvim"]);
        fake.clear();
        fake.running.set(false);
        fake.exited.set(true);
        quitting.nvim_exited();
        assert_eq!(fake.log(), ["close_window_confirmed"], "its exit closes the window");
    }

    /// A second close while nvim's dialog is up is refused where nvim is, and the editor is given
    /// the keys to answer it.
    #[test]
    fn a_second_close_while_nvim_asks_is_answered_there() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        quitting.window_close();
        fake.answer(false, false, "r?");
        fake.clear();
        assert_eq!(quitting.window_close(), WindowClose::Wait);
        assert_eq!(
            fake.log(),
            [
                "withdraw",
                "show_editor",
                "focus_editor",
                "refuse: nvim is already asking whether to quit; answer it there"
            ]
        );
    }

    /// `prefix x` retires an editor whose nvim already exited rather than sending
    /// `:confirm qall` to nothing. An editor that never ran is hidden in place.
    #[test]
    fn prefix_x_on_an_exited_editor_retires_it() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        fake.running.set(false);
        fake.exited.set(true);
        quitting.kill_editor(KillScope::Module, None).unwrap();
        assert_eq!(fake.log(), ["retire_editor"]);

        let fake = Fake::new();
        let quitting = flow(&fake);
        fake.running.set(false);
        quitting.kill_editor(KillScope::Module, None).unwrap();
        assert_eq!(fake.log(), ["kill_editor_in_place"]);
    }

    /// `prefix x` on a running editor asks nvim; its exit retires the editor.
    #[test]
    fn prefix_x_asks_nvim_and_its_exit_retires_the_editor() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        quitting.kill_editor(KillScope::Module, None).unwrap();
        assert_eq!(fake.log(), ["focus_editor", "send_quit"]);
        fake.clear();
        fake.running.set(false);
        quitting.nvim_exited();
        assert_eq!(fake.log(), ["retire_editor"]);
    }

    /// The last module on screen: `prefix x` asks nvim, and its exit closes the window on the `y`.
    #[test]
    fn prefix_x_on_the_last_module_closes_the_window_through_nvim() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        quitting.kill_editor(KillScope::Window, None).unwrap();
        assert_eq!(fake.log(), ["focus_editor", "send_quit"]);
        fake.clear();
        fake.running.set(false);
        quitting.nvim_exited();
        assert_eq!(fake.log(), ["close_window_confirmed"]);

        let fake = Fake::new();
        let quitting = flow(&fake);
        fake.running.set(false);
        quitting.kill_editor(KillScope::Window, None).unwrap();
        assert_eq!(fake.log(), ["close_window_confirmed"]);
    }

    /// An exit nobody asked for asks the window to close.
    #[test]
    fn an_unrequested_exit_asks_the_window_to_close() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        fake.running.set(false);
        quitting.nvim_exited();
        assert_eq!(fake.log(), ["close_window"]);
    }

    /// A declined close after nvim exited retires the editor and reports that nvim exited.
    /// Show the chat first if the editor was the last visible module. Declining while nvim
    /// runs changes nothing.
    #[test]
    fn a_declined_close_after_nvim_exited_retires_the_editor() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        quitting.window_close_declined();
        assert!(fake.log().is_empty(), "a running nvim is a working editor");

        fake.running.set(false);
        fake.exited.set(true);
        fake.editor_last_visible.set(true);
        quitting.window_close_declined();
        assert_eq!(
            fake.log(),
            [
                "retire_editor refused".to_string(),
                "show_chat".to_string(),
                "retire_editor".to_string(),
                format!("toast: {RETIRED_EDITOR_TEXT}")
            ]
        );
        fake.clear();
        quitting.window_close_declined();
        assert!(fake.log().is_empty(), "already retired");
    }

    /// T6-5: the retired editor's text names no cause it cannot know, and says a relaunch brings it
    /// back (the layout saves a retired editor as it was before, not hidden).
    #[test]
    fn the_retired_editors_text_claims_no_cause() {
        assert!(!RETIRED_EDITOR_TEXT.contains(":qa"));
        assert!(!RETIRED_EDITOR_TEXT.contains("prefix x"));
        assert!(RETIRED_EDITOR_TEXT.contains("relaunch eitri"));
    }

    /// A launcher that does not `exec` can exit while nvim is still handling a window close.
    /// The fork reports an exit, but the pane reports only that nvim is out of reach until its
    /// own process is gone. Another close ends nvim and waits instead of proceeding to
    /// shutdown or sending `:qa!`. Nvim's exit performs the requested action once.
    #[test]
    fn a_close_never_proceeds_while_nvims_own_process_lives() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        *fake.prompt.borrow_mut() = Some(RUNNING.into());
        fake.confirmed.set(true);
        quitting.window_close();
        // The launcher is gone: the fork says exited, nvim's pidfd says alive.
        fake.running.set(false);
        fake.pending.set(true);
        fake.clear();
        quitting.nvim_unreachable();
        assert_eq!(fake.log(), [format!("toast: {UNREACHABLE_NVIM}")]);
        fake.clear();
        // The close asks about the running tab again (its `y` travels with the quit), then ends nvim.
        assert_eq!(quitting.window_close(), WindowClose::Wait);
        fake.confirmed.set(true);
        fake.clear();
        assert_eq!(
            quitting.window_close(),
            WindowClose::Wait,
            "never Proceed while nvim lives"
        );
        assert_eq!(fake.log(), ["withdraw", "end_nvim"]);
        assert!(fake.ticks.borrow().len() <= 1, "no timer of its own");
        // nvim's process goes; the pane reports its exit, once.
        fake.pending.set(false);
        fake.exited.set(true);
        fake.clear();
        quitting.nvim_exited();
        assert_eq!(fake.log(), ["close_window_confirmed"]);
    }

    /// If nvim finishes an approved save after its launcher exits, the pending window close
    /// still completes. The pane reports nvim's exit when the process ends, even without
    /// another request.
    #[test]
    fn a_close_asked_before_the_launcher_went_completes_when_nvim_exits() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        quitting.window_close();
        fake.running.set(false);
        fake.pending.set(true);
        quitting.nvim_unreachable();
        fake.clear();
        // Later, nvim's save is done and it quits; the pane reports that exit.
        fake.pending.set(false);
        fake.exited.set(true);
        quitting.nvim_exited();
        assert_eq!(fake.log(), ["close_window_confirmed"], "the close that was asked");
    }

    /// Repeated `prefix x` requests while nvim is out of reach end it once through the pane's
    /// idempotent ending. They register no timer and retire nothing until the pane reports
    /// the exit, which retires the editor without closing the window.
    #[test]
    fn repeated_prefix_x_while_nvim_is_out_of_reach_retires_the_editor_once() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        fake.running.set(false);
        fake.pending.set(true);
        assert_eq!(quitting.kill_editor(KillScope::Module, None), Ok(()));
        assert_eq!(quitting.kill_editor(KillScope::Module, None), Ok(()));
        assert_eq!(fake.log(), ["end_nvim", "end_nvim"]);
        assert!(fake.ticks.borrow().is_empty(), "no timer: the pane reports the exit");
        fake.clear();
        fake.pending.set(false);
        fake.exited.set(true);
        quitting.nvim_exited();
        assert_eq!(fake.log(), ["retire_editor"], "the editor only, never the window");
    }

    /// The exit of an nvim out of reach is seen by the pane's tick, which runs only while the editor
    /// is mapped: a close over a hidden editor shows it for that, and hides it again after.
    #[test]
    fn a_hidden_editor_is_shown_while_its_nvim_is_ended_and_hidden_again_after() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        fake.running.set(false);
        fake.pending.set(true);
        fake.reveal.set(Some(Reveal {
            show: true,
            rehide: true,
        }));
        assert_eq!(quitting.window_close(), WindowClose::Wait);
        assert_eq!(fake.log(), ["withdraw", "show_editor", "end_nvim"]);
        fake.clear();
        fake.pending.set(false);
        fake.exited.set(true);
        quitting.nvim_exited();
        assert_eq!(fake.log(), ["hide_editor", "close_window_confirmed"]);
    }

    /// An older quit that nvim answered without quitting must not clear a newer explicit
    /// ending, even before its cancel letter or watch update is read. Each request has its
    /// own generation: `prefix x` while nvim is unreachable retires the editor when it exits
    /// without reusing a cancelled window close's `y`.
    #[test]
    fn an_answered_old_quit_never_takes_a_newer_ending_with_it() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        *fake.prompt.borrow_mut() = Some(RUNNING.into());
        fake.confirmed.set(true);
        quitting.window_close();
        // Cancelled in nvim's prompt: answered, no letter, the check not yet run.
        fake.answer(true, true, "n");
        // Its launcher exits; then `prefix x`.
        fake.running.set(false);
        fake.pending.set(true);
        fake.clear();
        assert_eq!(quitting.kill_editor(KillScope::Module, None), Ok(()));
        assert_eq!(fake.log(), ["end_nvim"]);
        fake.clear();
        fake.pending.set(false);
        fake.exited.set(true);
        quitting.nvim_exited();
        assert_eq!(fake.log(), ["retire_editor"], "the editor, as asked -- not the window");
    }

    /// An older window close still pending in nvim's dialog joins a newer kill request when
    /// the launcher exits. The window close takes precedence, so nvim's exit closes the
    /// window using the confirmation carried by that close.
    #[test]
    fn a_live_old_window_close_still_wins_over_a_newer_kill() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        *fake.prompt.borrow_mut() = Some(RUNNING.into());
        fake.confirmed.set(true);
        quitting.window_close();
        fake.answer(false, false, "r?");
        fake.running.set(false);
        fake.pending.set(true);
        fake.clear();
        assert_eq!(quitting.kill_editor(KillScope::Module, None), Ok(()));
        fake.clear();
        fake.pending.set(false);
        fake.exited.set(true);
        quitting.nvim_exited();
        assert_eq!(fake.log(), ["close_window_confirmed"]);
    }

    /// A window close sent to nvim with a `y` for a running tab, then cancelled in nvim's prompt; its
    /// launcher exits and `prefix x` on the editor is confirmed while the watch still reads the
    /// dialog -- the worker has not published nvim's answer, and the letter is not read yet.
    /// Returns the close's generation.
    fn a_kill_over_a_close_whose_cancel_is_late(fake: &Rc<Fake>, quitting: &Rc<EditorQuitting>) -> u32 {
        *fake.prompt.borrow_mut() = Some(RUNNING.into());
        fake.confirmed.set(true);
        quitting.window_close();
        let close = quitting.in_flight.borrow().as_ref().unwrap().generation;
        fake.answer(false, false, "r?");
        // Were the kill to wait for the worker, it would still see the request outstanding: the
        // answer comes after any bound (codex's timing).
        *fake.settled.borrow_mut() = fake.watch.borrow().clone();
        fake.running.set(false);
        fake.pending.set(true);
        fake.clear();
        assert_eq!(quitting.kill_editor(KillScope::Module, None), Ok(()));
        assert_eq!(fake.log(), ["end_nvim"]);
        assert_eq!(
            fake.settle_waits.get(),
            0,
            "the kill waits for nothing on the GTK thread"
        );
        fake.clear();
        close
    }

    /// The worker publishes, late: nvim answered the close without quitting.
    fn publish_the_cancel(fake: &Fake) {
        fake.answer(true, true, "n");
        *fake.settled.borrow_mut() = None;
    }

    /// nvim's own process goes, and the pane reports its exit.
    fn nvim_goes(fake: &Fake, quitting: &EditorQuitting) -> Vec<String> {
        fake.pending.set(false);
        fake.exited.set(true);
        fake.clear();
        quitting.nvim_exited();
        fake.log()
    }

    /// A window close is cancelled in nvim's prompt, then its launcher exits and `prefix x`
    /// is confirmed before the worker publishes the answer or the cancel letter is read.
    /// The kill carries the potentially live close. A late cancel letter removes that close,
    /// so nvim's exit retires the editor without reusing the cancelled `y` to close a window
    /// that still has a running tab.
    #[test]
    fn a_close_cancelled_as_the_launcher_went_lends_no_y_to_a_kill() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        let close = a_kill_over_a_close_whose_cancel_is_late(&fake, &quitting);
        assert_eq!(quitting.cancelled(close), Cancel::CloseWithdrawn);
        assert_eq!(
            nvim_goes(&fake, &quitting),
            ["retire_editor"],
            "the editor, as asked -- not the window"
        );
    }

    /// An answer published after a kill joins a window close must still withdraw the cancelled close.
    /// Waiting on the watch with a 500 ms bound could miss it, ignore the old cancellation generation,
    /// and close the window when nvim exited. Reading the answer again at exit removes the close
    /// even without a cancellation letter from the pane-switch shim.
    #[test]
    fn a_cancel_published_after_any_bound_still_takes_the_close_back() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        a_kill_over_a_close_whose_cancel_is_late(&fake, &quitting);
        publish_the_cancel(&fake);
        assert_eq!(nvim_goes(&fake, &quitting), ["retire_editor"]);

        // With the letter as well, read after the answer: the same.
        let fake = Fake::new();
        let quitting = flow(&fake);
        let close = a_kill_over_a_close_whose_cancel_is_late(&fake, &quitting);
        publish_the_cancel(&fake);
        assert_eq!(quitting.cancelled(close), Cancel::CloseWithdrawn);
        assert_eq!(nvim_goes(&fake, &quitting), ["retire_editor"]);
    }

    /// A close carried by a kill needs the same known outcome as a standalone close. If its
    /// request is still unsettled after the wait, keep only the kill: retire the editor and
    /// leave the window open.
    #[test]
    fn a_carried_close_still_unsettled_at_the_exit_is_not_trusted() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        a_kill_over_a_close_whose_cancel_is_late(&fake, &quitting);
        assert_eq!(nvim_goes(&fake, &quitting), ["retire_editor"]);
    }

    /// A later request that sends nothing reads the carried close's request too: answered, the close
    /// is taken out there, and the exit -- even one that would let a live close stand -- retires the
    /// editor.
    #[test]
    fn a_later_request_sees_the_carried_close_was_answered() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        a_kill_over_a_close_whose_cancel_is_late(&fake, &quitting);
        publish_the_cancel(&fake);
        assert_eq!(quitting.kill_editor(KillScope::Module, None), Ok(()));
        let asked = quitting.in_flight.borrow().clone().unwrap();
        assert_eq!((asked.quit, asked.lent_by), (EditorQuit::Module, None));
        *fake.settled.borrow_mut() = Some(CallWatch {
            sent_at: Instant::now(),
            done: true,
            returned: false,
            last_answer: None,
        });
        assert_eq!(nvim_goes(&fake, &quitting), ["retire_editor"]);
    }

    /// A repeated `prefix x` while nvim is unreachable carries the pending close under the
    /// new kill's generation but preserves the close's own identity. A late cancel letter
    /// can therefore remove the close from the second kill too.
    #[test]
    fn a_repeated_kill_carries_the_close_on_and_the_letter_still_finds_it() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        let close = a_kill_over_a_close_whose_cancel_is_late(&fake, &quitting);
        assert_eq!(quitting.kill_editor(KillScope::Module, None), Ok(()));
        assert_eq!(quitting.in_flight.borrow().as_ref().unwrap().lent_by, Some(close));
        assert_eq!(quitting.cancelled(close), Cancel::CloseWithdrawn);
        *fake.settled.borrow_mut() = Some(CallWatch {
            sent_at: Instant::now(),
            done: true,
            returned: false,
            last_answer: None,
        });
        assert_eq!(nvim_goes(&fake, &quitting), ["retire_editor"]);
    }

    /// A window close asked after the kill is a close of its own, which no answer of nvim's to the
    /// older one takes back: a late letter for that older close leaves it, and nvim's exit closes
    /// the window.
    #[test]
    fn a_later_window_close_is_its_own_and_a_late_letter_leaves_it() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        let close = a_kill_over_a_close_whose_cancel_is_late(&fake, &quitting);
        *fake.prompt.borrow_mut() = None;
        assert_eq!(quitting.window_close(), WindowClose::Wait);
        assert_eq!(quitting.cancelled(close), Cancel::Ignored);
        publish_the_cancel(&fake);
        assert_eq!(nvim_goes(&fake, &quitting), ["close_window_confirmed"]);
    }

    /// A request that sends nothing never waits on the GTK thread, whether a kill joins a close
    /// whose dialog is still open, a close joins an older close, or a kill joins another kill.
    #[test]
    fn a_request_that_sends_nothing_never_waits_on_the_gtk_thread() {
        let joins = [
            (EditorQuit::Window { confirmed: None }, EditorQuit::Module),
            (
                EditorQuit::Window { confirmed: None },
                EditorQuit::Window { confirmed: None },
            ),
            (EditorQuit::Module, EditorQuit::Module),
        ];
        for (older, newer) in joins {
            let fake = Fake::new();
            let quitting = flow(&fake);
            quitting.ask(older.clone()).unwrap();
            fake.answer(false, false, "r?");
            fake.running.set(false);
            fake.pending.set(true);
            assert_eq!(quitting.ask(newer.clone()), Ok(AskedNvim::Ending));
            assert_eq!(fake.settle_waits.get(), 0, "{older:?} then {newer:?}");
        }
    }

    /// The quit's answer reaches a worker through the RPC oneshot, but nvim's exit does not
    /// wait for that worker. Cancelling nvim's prompt and immediately typing `:qa` can report
    /// the exit before the watch says the quit was answered. Read the settled watch at exit
    /// so the cancelled `y` is not reused.
    #[test]
    fn the_exit_waits_for_the_quits_answer_before_using_its_y() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        *fake.prompt.borrow_mut() = Some(RUNNING.into());
        fake.confirmed.set(true);
        quitting.window_close();
        // At the exit the worker has not published yet; once it has, the quit was answered.
        fake.answer(false, false, "n");
        let now = Instant::now();
        *fake.settled.borrow_mut() = Some(CallWatch {
            sent_at: now,
            done: true,
            returned: true,
            last_answer: None,
        });
        fake.running.set(false);
        fake.exited.set(true);
        fake.clear();
        quitting.nvim_exited();
        assert_eq!(fake.log(), ["close_window"], "the ordinary close, which asks again");
    }

    /// If a quit's outcome is still unknown after the wait, do not treat it as approved.
    /// The window close asks again rather than trusting an old `y`.
    #[test]
    fn a_quit_still_unsettled_at_the_exit_is_not_taken_as_approved() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        *fake.prompt.borrow_mut() = Some(RUNNING.into());
        fake.confirmed.set(true);
        quitting.window_close();
        fake.answer(false, false, "n");
        *fake.settled.borrow_mut() = fake.watch.borrow().clone();
        fake.running.set(false);
        fake.exited.set(true);
        fake.clear();
        quitting.nvim_exited();
        assert_eq!(fake.log(), ["close_window"]);
    }

    /// A quit nvim could not be sent: nothing is left in flight.
    #[test]
    fn a_quit_that_could_not_be_sent_leaves_nothing_in_flight() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        fake.send_ok.set(false);
        assert!(quitting.ask(EditorQuit::Module).is_err());
        assert!(quitting.in_flight.borrow().is_none());
    }
}
