//! The editor's quit flow, with nvim deciding unsaved-buffer confirmation through its own RPC prompt.
//! Effects stay in the host; this value holds only the request in flight and its generation.

use super::close::{self as kill_rules, Asked, EditorQuit, NvimAnswer, QuitInFlight, QuitRequest, Reveal};
use super::host::EditorFacts;
use crate::layout::{KillScope, LayoutError};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskedNvim {
    /// The request was sent, or nvim had already exited; its exit or cancellation completes it.
    Asked,
    /// An unresponsive or unreachable nvim is being ended, with the quit waiting for its actual exit.
    Ending,
}

/// An unreachable nvim can only be ended; its swap file preserves the unsaved changes.
pub const UNREACHABLE_NVIM: &str = "nvim's launcher exited and nvim still runs where the editor can no \
     longer reach it \u{2014} close the window or kill-pane the editor to end it (its swap file keeps your changes)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cancel {
    /// The cancelled generation was in flight and is now cleared.
    Cleared,
    /// A kill keeps going after the older window close it carried was cancelled.
    CloseWithdrawn,
    /// The letter belongs to no current quit or carried close.
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowClose {
    /// A confirmation, nvim prompt or process exit is still owed, or a refusal was shown.
    Wait,
    /// nvim is gone or never ran, so shutdown may proceed.
    Proceed,
}

/// Immediate facts and effects of quitting. Sending reports failure later under its own generation.
pub trait QuitWorld {
    fn facts(&self) -> EditorFacts;
    fn reveal(&self) -> Reveal;
    fn editor_gone(&self) -> bool;
    fn close_confirmed(&self) -> bool;
    fn window_close_prompt(&self) -> Option<String>;
    fn send_quit(&mut self, lua: String, generation: u32);
    /// Ends nvim through stdin closure and signals to its own pid, preserving its swap files.
    fn end_nvim(&mut self);
    fn show_editor(&mut self) -> Result<(), LayoutError>;
    fn focus_editor(&mut self);
    fn hide_editor(&mut self) -> Result<(), LayoutError>;
    fn end_editor(&mut self) -> Result<(), LayoutError>;
    fn kill_editor_in_place(&mut self) -> Result<(), LayoutError>;
    /// Shows the module that takes the screen from an exited editor that was the last one on it.
    fn show_stand_in(&mut self) -> Result<(), LayoutError>;
    fn ask_to_close_window(&mut self, text: &str);
    fn withdraw_close_confirmation(&mut self);
    fn close_window_confirmed(&mut self);
    fn close_window(&mut self);
    fn refuse(&mut self, text: &str);
    fn toast(&mut self, text: &str);
    /// A toast that says something may be lost, up long enough to act on.
    fn warn(&mut self, text: &str);
    /// The ended editor's toast, naming the window's own key for it.
    fn editor_ended_text(&self) -> String;
    /// The toast of an editor that ended with nobody asking, naming the same key.
    fn editor_crashed_text(&self) -> String;
    /// Schedules polls of this generation until `poll` says to stop; the flow never waits on a timer.
    fn every_second(&mut self, generation: u32);
    /// nvim ended and the window stays (nobody asked, or only the editor was killed): the last record it left of
    /// what was open is to be read.
    fn read_handoff(&mut self);
}

#[derive(Default)]
pub struct QuitFlow {
    in_flight: QuitInFlight,
    generation: u32,
}

impl QuitFlow {
    /// nvim has been asked to quit and has not answered: it may end at any moment.
    pub fn is_running(&self) -> bool {
        self.in_flight.is_some()
    }

    /// A window close is in flight that nvim cannot answer: it is gone or being ended, so no prompt of its can hold the
    /// close, and its exit carries the close out.
    pub fn ends_the_window(&self) -> bool {
        self.in_flight
            .as_ref()
            .is_some_and(|asked| matches!(asked.quit, EditorQuit::Window { .. }) && (!asked.sent || asked.killed))
    }

    /// Reveals and focuses the editor before sending an RPC quit, so its prompt can be answered.
    /// An unanswered older request is joined rather than nesting another confirmation dialog.
    pub fn ask(&mut self, world: &mut dyn QuitWorld, quit: EditorQuit, now: Instant) -> Result<AskedNvim, String> {
        let facts = world.facts();
        if facts.exit_pending {
            return Ok(self.end_pending(world, quit));
        }
        let reveal = world.reveal();
        if !facts.running {
            if reveal.show {
                if let Err(err) = world.show_editor() {
                    println!("[modules] editor: nvim exited, and the editor could not be shown ({err})");
                }
            }
            self.begin_unsent(world, quit, reveal.rehide);
            return Ok(AskedNvim::Asked);
        }
        let watch = facts.watch;
        let request = kill_rules::quit_request(self.in_flight.as_ref(), watch.as_ref(), now);
        match request {
            QuitRequest::Ask => {}
            QuitRequest::Kill => {
                self.join(quit);
                if let Some(asked) = self.in_flight.as_mut() {
                    asked.killed = true;
                }
                println!("[modules] editor: nvim has not answered since it was asked to quit; ending it");
                world.end_nvim();
                return Ok(AskedNvim::Ending);
            }
            QuitRequest::AnswerThere(state) => {
                self.join(quit);
                let _ = world.show_editor();
                world.focus_editor();
                return Err(kill_rules::answer_there_text(state).into());
            }
        }
        if reveal.show {
            world.show_editor().map_err(|e| e.to_string())?;
        }
        world.focus_editor();
        let g = self.generation.wrapping_add(1);
        self.generation = g;
        self.in_flight = Some(Asked {
            generation: g,
            quit,
            revealed: reveal.rehide,
            sent: true,
            killed: false,
            lent_by: None,
        });
        world.send_quit(kill_rules::editor_quit_lua(g), g);
        println!(
            "[modules] editor: asked nvim to :confirm qall (quit {g}, shown for it: {}, hidden after: {})",
            reveal.show, reveal.rehide
        );
        world.every_second(g);
        Ok(AskedNvim::Asked)
    }

    /// Showing an unreachable editor keeps its exit feed active; only the actual process exit completes the quit.
    fn end_pending(&mut self, world: &mut dyn QuitWorld, quit: EditorQuit) -> AskedNvim {
        let reveal = world.reveal();
        if reveal.show {
            if let Err(err) = world.show_editor() {
                println!("[modules] editor: nvim is out of reach, and the editor could not be shown ({err})");
            }
        }
        self.begin_unsent(world, quit, reveal.rehide);
        world.end_nvim();
        println!("[modules] editor: nvim is out of reach; it is being ended, and its exit does what was asked");
        AskedNvim::Ending
    }

    /// An unsent quit gets a fresh generation and carries a live older close under that close's
    /// generation. Nonblocking watch reads let a late answer withdraw the close without losing the kill.
    fn begin_unsent(&mut self, world: &mut dyn QuitWorld, quit: EditorQuit, rehide: bool) {
        let answered = world.facts().watch.is_some_and(|w| w.done && w.returned);
        let older = self.in_flight.take().and_then(|mut asked| {
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
        let in_flight = &mut self.in_flight;
        let g = self.generation.wrapping_add(1);
        self.generation = g;
        *in_flight = Some(Asked {
            generation: g,
            quit: match &older {
                Some(older) => kill_rules::merge(&older.quit, quit),
                None => quit,
            },
            revealed: older.as_ref().is_some_and(|o| o.revealed) || rehide,
            sent: false,
            killed: false,
            lent_by,
        });
    }

    fn join(&mut self, quit: EditorQuit) {
        if let Some(asked) = self.in_flight.as_mut() {
            asked.quit = kill_rules::merge(&asked.quit, quit);
        }
    }

    pub fn poll(&mut self, world: &mut dyn QuitWorld, g: u32, now: Instant) -> bool {
        let quit = self
            .in_flight
            .as_ref()
            .filter(|asked| asked.generation == g)
            .map(|asked| asked.quit.clone());
        let Some(quit) = quit.filter(|_| world.facts().running) else {
            return false;
        };
        let watch = world.facts().watch;
        match kill_rules::nvim_state(watch.as_ref(), now) {
            kill_rules::NvimState::NotAnswering => {
                println!("[modules] editor: nvim has not answered since quit {g} was sent");
                world.toast(kill_rules::not_answering_text(&quit));
                false
            }
            kill_rules::NvimState::Returned => {
                self.settle(watch.as_ref());
                false
            }
            _ => true,
        }
    }

    /// Only a request that ended without an answer proves nvim quit. An unknown or returned
    /// request cannot lend an old window confirmation to a later, unrelated exit.
    fn settle(&mut self, watch: Option<&NvimAnswer>) -> Option<Asked> {
        let in_flight = &mut self.in_flight;
        if !in_flight.as_ref().is_some_and(|asked| asked.sent) {
            return None;
        }
        let why = match watch {
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

    /// A carried close follows its own request's outcome, while the editor kill survives its cancellation.
    fn settle_lent_close(&mut self, watch: Option<&NvimAnswer>) {
        let in_flight = &mut self.in_flight;
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

    pub fn cancelled(&mut self, g: u32) -> Cancel {
        let in_flight = &mut self.in_flight;
        if kill_rules::clear_if_cancelled(in_flight, g).is_some() {
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

    /// The host supplies the settled request with the exit, so deciding what closes never waits here.
    /// A hidden editor is restored to its saved arrangement before the window's close persists it.
    /// `crashed` is nvim going without saying it was leaving, after saying it would: nobody quit it, so
    /// the window stays and says so, where a `:q` of the last buffer closes it.
    pub fn nvim_exited(&mut self, world: &mut dyn QuitWorld, settled: Option<NvimAnswer>, crashed: bool) {
        let watch = settled;
        if self.settle(watch.as_ref()).is_some() {
            println!("[modules] editor: nvim exited, and the quit in flight is not what ended it");
        }
        self.settle_lent_close(watch.as_ref());
        let asked = self.in_flight.take();
        match asked {
            Some(Asked {
                quit: EditorQuit::Window { confirmed },
                revealed,
                ..
            }) => {
                if revealed {
                    if let Err(err) = world.hide_editor() {
                        println!("[modules] editor: shown for nvim's prompt, and stays ({err})");
                    }
                }
                self.close_after(world, confirmed);
            }
            Some(Asked {
                quit: EditorQuit::Module,
                ..
            }) => match world.end_editor() {
                // Captures stop with the editor, so its last record is kept now, not at the next restart or close.
                Ok(()) => world.read_handoff(),
                Err(err) => {
                    println!("[modules] editor: nvim quit and could not be closed ({err}); closing the window");
                    world.close_window();
                }
            },
            // Nobody asked, so nothing else reads what nvim left of its open files as it ended; a `:qa`'s close then
            // only waits for the read.
            None if crashed => {
                world.read_handoff();
                if Self::end_in_place(world, "[modules] editor: nvim stopped with nobody asking") {
                    world.warn(&world.editor_crashed_text());
                }
            }
            None => {
                world.read_handoff();
                world.close_window();
            }
        }
    }

    pub fn nvim_unreachable(&mut self, world: &mut dyn QuitWorld) {
        println!("[modules] editor: nvim outlived the fork's report of its exit");
        world.toast(UNREACHABLE_NVIM);
    }

    pub fn close_after(&mut self, world: &mut dyn QuitWorld, confirmed: Option<String>) {
        let now = world.window_close_prompt();
        if kill_rules::close_is_confirmed(confirmed.as_deref(), now.as_deref()) {
            println!("[window] closing: what the close would ask was already answered");
            world.close_window_confirmed();
        } else {
            println!("[window] closing; what is running changed since it was asked, so the close asks again");
            world.close_window();
        }
    }

    /// Takes the window confirmation back before asking nvim; a later close must compare its prompt again.
    pub fn window_close(&mut self, world: &mut dyn QuitWorld, now: Instant) -> WindowClose {
        if !world.close_confirmed() {
            if let Some(text) = world.window_close_prompt() {
                world.ask_to_close_window(&text);
                return WindowClose::Wait;
            }
        }
        if !world.facts().running && !world.facts().exit_pending {
            return WindowClose::Proceed;
        }
        let confirmed = world.window_close_prompt();
        world.withdraw_close_confirmation();
        match self.ask(world, EditorQuit::Window { confirmed }, now) {
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
                world.refuse(&refusal);
                WindowClose::Wait
            }
        }
    }

    pub fn kill_editor(
        &mut self,
        world: &mut dyn QuitWorld,
        scope: KillScope,
        confirmed: Option<String>,
        now: Instant,
    ) -> Result<(), String> {
        match scope {
            KillScope::Window if world.facts().running || world.facts().exit_pending => {
                self.ask(world, EditorQuit::Window { confirmed }, now)?;
                println!("[modules] editor: the last module; nvim decides, then the window closes");
            }
            KillScope::Window => self.close_after(world, confirmed),
            KillScope::Module if world.facts().running || world.facts().exit_pending => {
                self.ask(world, EditorQuit::Module, now)?;
            }
            KillScope::Module if world.facts().exited => world.end_editor().map_err(|e| e.to_string())?,
            KillScope::Module => world.kill_editor_in_place().map_err(|e| e.to_string())?,
        }
        Ok(())
    }

    /// An exited editor cannot retain the keys after the close is declined. If it is the last
    /// visible module, show another module before ending it so the grid can transfer those keys.
    pub fn window_close_declined(&mut self, world: &mut dyn QuitWorld) {
        if !kill_rules::retire_on_declined_close(world.facts().exited, world.editor_gone()) {
            return;
        }
        if Self::end_in_place(world, "[window] close declined after nvim exited") {
            world.toast(&world.editor_ended_text());
        }
    }

    /// A restarted nvim that exited before it was ready: nothing asked it to quit and the window is not closed for
    /// it, so the editor ends in place again and says how to start it once more.
    pub fn restarted_nvim_exited(&mut self, world: &mut dyn QuitWorld) {
        if Self::end_in_place(world, "[modules] editor: the restarted nvim exited before it was ready") {
            world.toast(&world.editor_ended_text());
        }
    }

    /// An exited editor cannot keep the keys, so when it is the last module on screen another is shown first.
    fn end_in_place(world: &mut dyn QuitWorld, why: &str) -> bool {
        let ended = match world.end_editor() {
            Err(LayoutError::LastVisible(_)) => world.show_stand_in().and_then(|()| world.end_editor()),
            other => other,
        };
        match ended {
            Ok(()) => {
                println!("{why}; the editor ended in place");
                true
            }
            Err(err) => {
                eprintln!("{why}; the editor could not be ended: {err}");
                false
            }
        }
    }

    /// A failed send answers only the generation that attempted it; an older failure cannot clear a newer quit.
    pub fn quit_not_sent(&mut self, _world: &mut dyn QuitWorld, g: u32) -> Option<String> {
        kill_rules::clear_if_cancelled(&mut self.in_flight, g)?;
        Some("nvim could not be asked to quit — quit it in the editor (:qa)".into())
    }
}

#[cfg(test)]
mod tests;
