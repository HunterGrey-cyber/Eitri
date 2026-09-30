//! The one flow every quit of the editor takes -- `prefix x` on it, every window close -- and what
//! nvim's exit, a cancel and a declined window close then do (v1 hardening Task 6, ruling R3:
//! "closing the window never discards unsaved editor changes silently"; `kill_pane`'s module doc
//! has the rulings and their precedents).
//!
//! **Why a module of its own (the fix round after the Task 6/7/9 review).** The flow used to live in
//! four closures in `main.rs` (`ask_nvim_to_quit`, `on_exited_unrequested`, the window's
//! `connect_close_request`, the decline hook), where no test could reach it: the Opus review's T6-4
//! deleted six of its rules at once -- the close's route through nvim, the `y`'s withdrawal, the
//! reveal of an editor off screen, the retirement of an exited editor, the decline hook -- and every
//! test stayed green. Here every decision and its order is [`EditorQuitting`]'s, under test against a
//! fake [`QuitHost`]; `main.rs` implements the host over the pane, the layout, the grid, the window's
//! y/n and the toast, one GTK call per method, and that one-line layer is what only the GUI
//! checklist still checks.
//!
//! **Never `:qa!`** (round 4, the main session's ruling, 2026-09-27). Ending the editor is only
//! nvim's own `:confirm qall`, or -- when nvim's loop stopped answering it, or nvim is out of
//! reach -- [`QuitHost::end_nvim`]: its stdin closed (nvim exits keeping its swap files) and
//! SIGTERM -- SIGTERM first while nvim may be in its prompt (the quit follow-up) -- then SIGKILL by
//! pid after the patience (`neovide_editor`'s `nvim_child::end`). The fork's shutdown
//! sends no quit either, so a window close that proceeds can no longer destroy work; what is left
//! to get right is that nothing closes or retires twice, and nothing follows an exit while nvim's
//! own process lives (the pane reports the exit only once it is gone).

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

/// The fork reports nvim exited while nvim's own process lives: a `nvim` launcher that does not
/// `exec` exited without it (codex, round 3). The fork takes no keys for it any more, so only an
/// ending reaches it; what was asked waits for its exit.
pub(crate) const UNREACHABLE_NVIM: &str = "nvim's launcher exited and nvim still runs where the editor can no \
     longer reach it \u{2014} close the window or kill-pane the editor to end it (its swap file keeps your changes)";

/// What a retired editor's refusals say (P8's toast, and the decline path's): nvim ended -- a
/// `prefix x`, a `:qa`, a crash, which this cannot tell apart -- and this window cannot start
/// another one (`eitri_core::layout::kill`'s module doc: one winit event loop per process). A
/// relaunch shows the editor again (`eitri_core::layout::Layout::saved_hidden`), so this says so
/// without naming a cause it does not know (the Opus review's T6-5: "the editor was closed (:qa or
/// prefix x)" was also shown for a crash).
pub(crate) const RETIRED_EDITOR_TEXT: &str =
    "nvim exited and cannot restart in this window \u{2014} relaunch Eitri to get the editor back";

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
    /// whole bound (the quit follow-up's fix round took it out of `begin_unsent` for that).
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

    /// Asks nvim to `:confirm qall` for `quit` -- the one route every quit of the editor takes. nvim
    /// decides: its own prompt for unsaved buffers, and only its exit closes anything
    /// ([`EditorQuitting::nvim_exited`]). The quit is an RPC request, never typed keys, which a key
    /// nvim is waiting for (after `f`, inside `getchar()`) would swallow (review finding I1). An
    /// editor that is not on screen -- hidden, or zoomed away (I2) -- is brought on screen and given
    /// the keys first, since the prompt is drawn in it. A second request while nvim has the quit is
    /// refused (`Err`, the text to show, saying what nvim is doing), except once nvim's own loop has
    /// not answered the pane's watch for `NVIM_QUIT_PATIENCE`: then the second request kills it
    /// (`kill_pane::quit_request`), its swap files kept. A toast says so when that point is reached.
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

    /// A request while nvim's process outlives the fork's report of its exit (a launcher exited
    /// without it): nvim is out of reach, so only an ending gets to it. What the request was for
    /// joins the quit in flight and waits for nvim's exit, which the pane reports once the process is
    /// gone. Repeated, it starts nothing more (the pane's ending is idempotent) -- codex's round-3
    /// finding (d): each repeat used to register its own timer, and the second one to fire closed
    /// the whole window.
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

    /// A request that sends nvim nothing (nvim exited just now, or is out of reach) gets its own
    /// entry in flight, with its own generation (round 5, codex finding 1): it used to be merged
    /// into whatever was in flight, keeping that quit's generation and its `sent` -- so an older
    /// quit nvim had answered without quitting (its cancel letter absent, the watch not yet polled)
    /// cleared it at nvim's exit, and a `prefix x` closed the whole window. What an older quit
    /// still in flight was for joins the new one (a window close wins over the editor's own kill,
    /// `kill_pane::merge`); one nvim answered is dropped, `y` and all.
    ///
    /// **A close a kill joins stays that close's to take back** (the quit follow-up, codex finding 1
    /// of round 5's review, and its fix round). A window close cancelled in nvim's prompt just before
    /// its launcher exited has an answer the watch's worker may not have published yet, and a cancel
    /// letter not read yet. A kill that joins it carries the close ([`kill_pane::merge`]) under its
    /// own generation, so it records which sent close it carries ([`Asked::lent_by`]): that close's
    /// letter ([`EditorQuitting::cancelled`]), its request read answered here by a later request, or
    /// at nvim's exit ([`EditorQuitting::nvim_exited`]) takes the close back out, however late. The
    /// follow-up first read the settled watch here instead (up to the pane's 500 ms `SETTLE_WAIT`,
    /// on the GTK thread): an answer published after that bound kept the close for good, with every
    /// later sign of the cancel ignored (codex), and the wait froze the window when nvim's dialog
    /// was still up. Nothing here waits now.
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

    /// Clears the quit in flight unless its own request is known to have ended with nvim gone. An
    /// answered request means nvim ran the Lua and did not quit -- the user cancelled -- so neither
    /// the quit nor the `y` it carries may answer a later, unrequested exit (codex finding C1:
    /// without the pane-switch shim's letter, a cancelled window close stayed in flight, and a `:qa`
    /// typed later closed the window on the old `y` while a tab was still running). A request whose
    /// ending is not known yet is not taken as the quit either (round 3, codex finding 2): at the
    /// exit, [`EditorQuitting::nvim_exited`] reads the settled watch, and one still outstanding
    /// after that wait is cleared too, so the close asks again rather than trusting the old `y`. A
    /// quit that sent nothing (joined to an nvim that had exited already) is kept. Returns what it
    /// cleared.
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
        // Round 3, finding 2: the request's answer may still be on its way to the watch.
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

    /// The window's close request (the top bar's close, the compositor's, and every route above):
    /// a running tab is worth one y/n (D11 A); then nvim decides through its own `:confirm qall` and
    /// the window closes when nvim exits. `WindowClose::Proceed` only once nvim is gone or never ran,
    /// since the caller's shutdown runs the fork's quit, which is `:qa!`.
    pub(crate) fn window_close(self: &Rc<Self>) -> WindowClose {
        let host = &self.host;
        if !host.close_confirmed() {
            if let Some(text) = host.window_close_prompt() {
                host.ask_to_close_window(&text);
                return WindowClose::Wait;
            }
        }
        // Only once nvim's own process is gone: the window never closes while an nvim we started
        // may still be writing (round 4). One that outlived the fork's report is ended instead.
        if !host.nvim_running() && !host.nvim_exit_pending() {
            return WindowClose::Proceed;
        }
        // From here the `y` travels with the quit (`EditorQuit::Window`), and nvim's exit closes the
        // window through `close_after`, which asks again if what is running changed meanwhile. Left
        // set, it would let that second close skip the question (review M1).
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
            // nvim already exited and the pane was not released yet (R1-2): nothing to ask, and a
            // `:confirm qall` sent to it would never be answered. Retired as its exit would have.
            KillScope::Module if host.nvim_exited() => host.retire_editor().map_err(|e| e.to_string())?,
            // No nvim to quit (never started, or it failed to): nothing ends, and it can show again.
            KillScope::Module => host.kill_editor_in_place().map_err(|e| e.to_string())?,
        }
        Ok(())
    }

    /// The window close's own y/n was answered with anything but `y` (R1-2): if nvim already exited
    /// -- on its own (`:q`, a crash), which is what asked to close the window -- the editor is
    /// retired as `prefix x`'s kill retires it, never left on screen holding the keys with nothing
    /// behind it. The declined close was about what runs in the chat, so a chat hidden meanwhile is
    /// shown when the editor was the last module on screen.
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

    /// R1-1 (the review's first mutation): with nvim running, a window close never proceeds to the
    /// shutdown -- which runs the fork's `:qa!` -- but hands the quit to nvim, having taken the `y`
    /// back first (M1, the second mutation), and the editor gets the keys for nvim's prompt.
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

    /// D11 A still comes first: a running tab is asked about before nvim is.
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

    /// I2 (the third mutation): an editor off screen is shown for nvim's prompt before it gets the
    /// keys and before the quit is sent, and one that was hidden is hidden again when nvim quits.
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

    /// Codex finding C1: a window close answered `y`, then cancelled in nvim's own prompt. nvim's
    /// request comes back answered (the Lua returned, so nvim did not quit); without the shim's
    /// letter nothing else says so. A `:qa` typed later must not close the window on that old `y`
    /// while the tab still runs: the close asks again.
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

    /// T6-1, as round 4 settles it: an nvim whose loop stopped answering is never handed the
    /// fork's quit (there is none any more) and never proceeds to the shutdown. The second close
    /// ends it -- stdin closed, SIGTERM, SIGKILL by pid on the pane's schedule, the swap file kept
    /// -- and waits; its exit closes the window without asking again.
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

    /// R1-2 (the fourth mutation): `prefix x` on an editor whose nvim already exited retires it
    /// instead of sending `:confirm qall` to nothing; one that never ran is hidden in place.
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

    /// R1-2 (the fifth mutation's other half): a declined close after nvim exited retires the
    /// editor and says so; the chat is shown first when the editor was the last module on screen.
    /// Declined while nvim runs, it changes nothing.
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
        assert!(RETIRED_EDITOR_TEXT.contains("relaunch Eitri"));
    }

    /// Round 4 (codex's round-3 finding (a)-(c) under the new rule): a `nvim` launcher that does not
    /// `exec` exits while nvim still runs -- in the pending `:confirm qall` of a window close, say.
    /// The fork reports an exit; the pane reports none until nvim's own process is gone, only that
    /// nvim is out of reach. A close then never proceeds to the shutdown: it ends nvim (no `:qa!`
    /// anywhere) and waits, and nvim's exit does what was asked -- once.
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

    /// Codex's round-3 finding (c): nvim finishing an approved save when its launcher exits, and
    /// nobody asks anything more. Nothing is lost track of: the pane reports nvim's exit when the
    /// process goes (`neovide_editor`'s `exit_events`), and the close that was asked completes.
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

    /// Codex's round-3 finding (d): `prefix x` repeated while nvim is out of reach ends it once (the
    /// pane's ending is idempotent), registers no timer, retires nothing yet -- and the one exit the
    /// pane reports retires the editor without closing the window.
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

    /// Round 5, codex finding 1: an old quit nvim answered without quitting -- its cancel letter
    /// absent, the watch not yet polled -- must not take a newer, explicit ending with it. Each
    /// request has its own generation: `prefix x` while nvim is out of reach retires the editor
    /// when nvim goes, never closes the whole window, and never on the cancelled `y`.
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

    /// Finding 1's other side: an older quit still live -- nvim's dialog up for a window close when
    /// its launcher exits -- joins the newer request, and a window close wins over the editor's own
    /// kill, as it always has: nvim's exit closes the window on the `y` that close carries.
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

    /// The quit follow-up, codex finding 1 of round 5's review, from its own scenario: a window
    /// close is cancelled in nvim's prompt; before the watch's worker publishes nvim's answer, and
    /// before the cancel letter is read, the launcher exits and a `prefix x` on the editor is
    /// confirmed. The kill carries the close (it may still be live), but the letter, read late,
    /// takes it back out, and nvim's exit retires the editor -- never closes the window on the
    /// cancelled `y`, with a tab still running.
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

    /// The fix round, codex's finding on the follow-up: the follow-up read the settled watch at the
    /// kill instead, and a worker publishing after its 500 ms bound left the close in the kill for
    /// good -- the late letter named an old generation and was ignored, and the answer, once
    /// published, was never read for a quit that sent nothing -- so nvim's exit closed the window.
    /// Now the answer counts however late it is published: read at the exit, it takes the close
    /// back out even with no letter at all (no pane-switch shim).
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

    /// The exit's rule for a close a kill carries is the exit's rule for a close of its own
    /// (round 3, finding 2): one whose request has still not said how it ended after the wait is
    /// not trusted. The kill is what is left, so the editor is retired and the window stays.
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

    /// `prefix x` repeated while nvim is out of reach (round 3's finding (d)) before anything says
    /// the close was cancelled: the second kill carries the close on under its own generation,
    /// still as that close's, so the late letter takes it back out of the second kill too.
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

    /// The fix round's minor finding: a request that sends nothing never waits on the GTK thread --
    /// not a kill joining a close whose dialog is still up (the follow-up blocked there for the whole
    /// 500 ms), not a close joining an older one (round 5's orphan path, the second close), and not a
    /// kill joining a kill.
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

    /// Round 3, codex finding 2: the quit's answer comes back through nvim-rs's oneshot to a worker
    /// thread, and nvim's exit is reported without waiting for that worker. A close cancelled in
    /// nvim's prompt and followed at once by `:qa` can reach the exit before the watch says the
    /// quit was answered. The exit reads the settled watch, so the cancelled `y` is not reused.
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

    /// Finding 2's other half: a quit whose ending is still unknown after the wait is not taken as
    /// approved -- the close asks again rather than trusting an old `y`.
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
