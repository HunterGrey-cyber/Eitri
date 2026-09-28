//! Coordinates editor quits from `prefix x` and every window close, including nvim's exit,
//! cancellation, and a declined close. Window closes ask nvim to confirm unsaved changes.
//!
//! [`EditorQuitting`] owns the decisions and their order so they can be tested with a fake
//! [`QuitHost`]. `main.rs` implements the host over the pane, layout, grid, confirmation prompt,
//! and toast. The GTK calls in that layer still need GUI verification.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use neovibe_core::layout::{KillScope, LayoutError};
use neovide_editor::CallWatch;

use crate::kill_pane::{self, Asked, EditorQuit, QuitInFlight, QuitRequest, Reveal};

/// What [`EditorQuitting::ask`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AskedNvim {
    /// nvim was sent `:confirm qall` (or had exited already); its exit, or a cancel, answers.
    Asked,
    /// nvim's loop had not answered since it was asked (`kill_pane::QuitRequest::Kill`), and it was
    /// killed (or was found gone): its exit closes what the quit was for.
    Killed,
    /// It should have been killed, and no process is known to be nvim (`neovide_editor`'s
    /// `nvim_child`).
    Unkillable,
}

/// The refusal for [`AskedNvim::Unkillable`]. A window close also shows it and waits.
/// Falling back to the fork's `:qa!` could run inside a pending `:confirm qall` dialog and
/// lose both the file's changes and its swap. Nvim quitting or being ended externally still
/// closes the window.
pub(crate) const UNKILLABLE_NVIM: &str = "nvim is not responding and its process is not known, so it cannot be \
     killed from here \u{2014} the window closes once nvim quits";

/// Text for a retired editor: nvim has ended and cannot restart in this window.
/// The cause is unknown: `prefix x`, `:qa`, and a crash can all reach this path.
/// A relaunch shows the editor again, so the message explains how to get it back
/// without claiming to know why it exited.
pub(crate) const RETIRED_EDITOR_TEXT: &str =
    "nvim exited and cannot restart in this window \u{2014} relaunch neovibe to get the editor back";

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
    /// The quit request's watch (`NeovideEditorPane::watched_call`).
    fn watched_call(&self) -> Option<CallWatch>;
    /// Has nvim run `lua` as an RPC request (`NeovideEditorPane::exec_lua_watched`).
    fn send_quit(&self, lua: &str) -> bool;
    /// `NeovideEditorPane::kill_nvim`.
    fn kill_nvim(&self) -> bool;
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
    /// The window close's prompt as it stands now (`neovibe_core::tabs::window_close_prompt`).
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
        let reveal = host.reveal();
        let g = self.generation.get().wrapping_add(1);
        if !host.nvim_running() {
            // nvim exited just now: nothing to ask, and its exit -- seen on the pane's next tick --
            // does what this request was for, joined to what was already in flight. An editor off
            // screen is brought on screen, so that tick runs and nothing waits for a second close.
            if reveal.show {
                if let Err(err) = host.show_editor() {
                    println!("[modules] editor: nvim exited, and the editor could not be shown ({err})");
                }
            }
            let mut in_flight = self.in_flight.borrow_mut();
            match in_flight.as_mut() {
                Some(asked) => {
                    asked.quit = kill_pane::merge(&asked.quit, quit);
                    asked.revealed |= reveal.rehide;
                }
                None => {
                    self.generation.set(g);
                    *in_flight = Some(Asked {
                        generation: g,
                        quit,
                        revealed: reveal.rehide,
                        sent: false,
                    });
                }
            }
            return Ok(AskedNvim::Asked);
        }
        let watch = host.watched_call();
        let request = kill_pane::quit_request(self.in_flight.borrow().as_ref(), watch.as_ref(), Instant::now());
        match request {
            QuitRequest::Ask => {}
            QuitRequest::Kill => {
                self.join(quit);
                println!("[modules] editor: nvim has not answered since it was asked to quit; killing it");
                return Ok(if host.kill_nvim() {
                    AskedNvim::Killed
                } else {
                    AskedNvim::Unkillable
                });
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
        self.generation.set(g);
        *self.in_flight.borrow_mut() = Some(Asked {
            generation: g,
            quit,
            revealed: reveal.rehide,
            sent: true,
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

    /// Clears the quit in flight when its request returns: nvim ran the Lua without quitting,
    /// so the user cancelled. Neither the quit nor its `y` may authorize a later unrequested
    /// exit, even if no cancel letter arrives from the pane-switch shim. Returns the cleared quit.
    fn settle(&self, watch: Option<&CallWatch>) -> Option<Asked> {
        let mut in_flight = self.in_flight.borrow_mut();
        let answered = watch.is_some_and(|w| w.returned);
        if !(answered && in_flight.as_ref().is_some_and(|asked| asked.sent)) {
            return None;
        }
        let cleared = in_flight.take();
        if let Some(asked) = &cleared {
            println!(
                "[modules] editor: nvim answered quit {} without quitting; nothing is in flight",
                asked.generation
            );
        }
        cleared
    }

    /// nvim's letter on the pane-switch socket: quit `g` was cancelled. `true` if it was the one in
    /// flight.
    pub(crate) fn cancelled(&self, g: u32) -> bool {
        kill_pane::clear_if_cancelled(&mut self.in_flight.borrow_mut(), g).is_some()
    }

    /// nvim exited without the host asking the pane to shut down: whatever was asked of it is
    /// answered. A window close closes the window without asking what was already asked (the editor
    /// hidden again first if the close showed it for nvim's prompt, so the arrangement saved on
    /// close is the user's); `prefix x` retires the editor; an exit nobody asked for -- or one after
    /// a quit nvim already answered without quitting -- asks the window to close, which asks
    /// whatever the close asks.
    pub(crate) fn nvim_exited(&self) {
        let watch = self.host.watched_call();
        if self.settle(watch.as_ref()).is_some() {
            println!("[modules] editor: nvim exited after it had declined the quit; nothing it said applies");
        }
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
        if !host.nvim_running() {
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
            Ok(AskedNvim::Killed) => {
                println!("[window] close: nvim was killed; the window closes when it is gone");
                WindowClose::Wait
            }
            // T6-1: never the fork's quit on a live nvim. The quit stays in flight, so nvim quitting
            // -- or being ended from outside -- still closes the window.
            Ok(AskedNvim::Unkillable) => {
                println!("[window] close: {UNKILLABLE_NVIM}");
                host.refuse(UNKILLABLE_NVIM);
                WindowClose::Wait
            }
            Err(refusal) => {
                println!("[window] close: {refusal}");
                host.refuse(&refusal);
                WindowClose::Wait
            }
        }
    }

    /// `prefix x` on the editor, once its y/n was answered: `scope` from `neovibe_core::layout::
    /// can_kill`, `confirmed` the window close's prompt as it stood when `x` asked.
    pub(crate) fn kill_editor(self: &Rc<Self>, scope: KillScope, confirmed: Option<String>) -> Result<(), String> {
        let host = &self.host;
        match scope {
            KillScope::Window if host.nvim_running() => {
                // nvim first, as for any kill of the editor: its exit closes the window.
                if self.ask(EditorQuit::Window { confirmed })? == AskedNvim::Unkillable {
                    return Err(UNKILLABLE_NVIM.into());
                }
                println!("[modules] editor: the last module; nvim decides, then the window closes");
            }
            KillScope::Window => self.close_after(confirmed),
            // nvim decides: its own prompt for unsaved buffers, and only its exit closes the module.
            KillScope::Module if host.nvim_running() => {
                if self.ask(EditorQuit::Module)? == AskedNvim::Unkillable {
                    return Err(UNKILLABLE_NVIM.into());
                }
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
    use neovibe_core::layout::ModuleId;
    use std::time::Duration;

    /// A fake world: what the flow asks is answered from these cells, and what it does is logged.
    #[derive(Default)]
    struct Fake {
        running: Cell<bool>,
        exited: Cell<bool>,
        gone: Cell<bool>,
        reveal: Cell<Option<Reveal>>,
        watch: RefCell<Option<CallWatch>>,
        send_ok: Cell<bool>,
        kill_ok: Cell<bool>,
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
            fake.kill_ok.set(true);
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
        fn watched_call(&self) -> Option<CallWatch> {
            self.watch.borrow().clone()
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
        fn kill_nvim(&self) -> bool {
            self.did("kill_nvim");
            self.kill_ok.get()
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
        assert!(!quitting.cancelled(g + 1));
        assert!(quitting.cancelled(g));
        fake.running.set(false);
        fake.clear();
        quitting.nvim_exited();
        assert_eq!(fake.log(), ["close_window"]);
    }

    /// T6-1: an nvim that stopped answering and whose process is not known is never handed the
    /// fork's quit: the close is refused, saying so, and waits -- the quit stays in flight, so nvim
    /// quitting still closes the window.
    #[test]
    fn an_unkillable_nvim_refuses_the_close_instead_of_running_qa() {
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
        fake.kill_ok.set(false);
        fake.clear();
        assert_eq!(
            quitting.window_close(),
            WindowClose::Wait,
            "never Proceed: that runs :qa!"
        );
        assert_eq!(
            fake.log(),
            ["withdraw", "kill_nvim", format!("refuse: {UNKILLABLE_NVIM}").as_str()]
        );
        fake.clear();
        fake.running.set(false);
        quitting.nvim_exited();
        assert_eq!(
            fake.log(),
            ["close_window_confirmed"],
            "its later exit closes the window"
        );
    }

    /// A killed nvim: the close waits for it to go.
    #[test]
    fn a_stuck_nvim_is_killed_by_the_second_close() {
        let fake = Fake::new();
        let quitting = flow(&fake);
        quitting.window_close();
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
        assert_eq!(quitting.window_close(), WindowClose::Wait);
        assert_eq!(fake.log(), ["withdraw", "kill_nvim"]);
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
        assert!(RETIRED_EDITOR_TEXT.contains("relaunch neovibe"));
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
