use super::super::close::NvimReply;
use super::*;
use crate::layout::ModuleId;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

/// A fake world: what the flow asks is answered from these cells, and what it does is logged.
#[derive(Default)]
struct Fake {
    running: Cell<bool>,
    exited: Cell<bool>,
    /// The fork reports nvim exited, and nvim's process lives (a launcher exited without it).
    pending: Cell<bool>,
    /// What the watch reads once its worker has published (`None`: the watch as it is).
    settled: RefCell<Option<NvimAnswer>>,
    /// Counts settled snapshots supplied at exit; requesting a quit must never collect one.
    settle_waits: Cell<u32>,
    gone: Cell<bool>,
    reveal: Cell<Option<Reveal>>,
    watch: RefCell<Option<NvimAnswer>>,
    confirmed: Cell<bool>,
    prompt: RefCell<Option<String>>,
    editor_last_visible: Cell<bool>,
    /// The window holds a module besides the editor.
    others: Cell<bool>,
    log: RefCell<Vec<String>>,
    ticks: RefCell<Vec<u32>>,
    /// How often the last record nvim left was asked to be read; kept out of `log`, which pins what the quit does.
    handoffs: Cell<u32>,
}

impl Fake {
    fn new() -> Rc<Fake> {
        let fake = Rc::new(Fake::default());
        fake.running.set(true);
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
        *self.watch.borrow_mut() = Some(NvimAnswer {
            sent_at: now,
            done,
            returned,
            last: Some(NvimReply {
                at: now,
                mode: mode.into(),
                blocking: false,
            }),
        });
    }
}

impl QuitWorld for Rc<Fake> {
    fn facts(&self) -> EditorFacts {
        EditorFacts {
            running: self.running.get(),
            exited: self.exited.get(),
            exit_pending: self.pending.get(),
            starting: false,
            watch: self.watch.borrow().clone(),
        }
    }
    fn send_quit(&mut self, lua: String, _generation: u32) {
        assert!(lua.contains("confirm qall"), "{lua}");
        self.did("send_quit");
        // A new request: nvim has not answered it yet.
        *self.watch.borrow_mut() = Some(NvimAnswer {
            sent_at: Instant::now(),
            done: false,
            returned: false,
            last: None,
        });
    }
    fn end_nvim(&mut self) {
        self.did("end_nvim");
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
    fn show_editor(&mut self) -> Result<(), LayoutError> {
        self.did("show_editor");
        Ok(())
    }
    fn focus_editor(&mut self) {
        self.did("focus_editor");
    }
    fn hide_editor(&mut self) -> Result<(), LayoutError> {
        self.did("hide_editor");
        Ok(())
    }
    fn end_editor(&mut self) -> Result<(), LayoutError> {
        if self.editor_last_visible.get() {
            self.did("end_editor refused");
            return Err(LayoutError::LastVisible(ModuleId::editor()));
        }
        self.did("end_editor");
        self.gone.set(true);
        Ok(())
    }
    fn kill_editor_in_place(&mut self) -> Result<(), LayoutError> {
        self.did("kill_editor_in_place");
        Ok(())
    }
    fn editor_ended_text(&self) -> String {
        crate::wm::feedback::editor_ended_text(&crate::keymap::Keymap::defaults())
    }
    fn editor_crashed_text(&self) -> String {
        crate::wm::feedback::editor_crashed_text(&crate::keymap::Keymap::defaults())
    }
    fn warn(&mut self, text: &str) {
        self.did(format!("warn: {text}"));
    }
    fn show_stand_in(&mut self) -> Result<(), LayoutError> {
        self.did("show_stand_in");
        self.editor_last_visible.set(false);
        Ok(())
    }
    fn close_confirmed(&self) -> bool {
        self.confirmed.get()
    }
    fn holds_other_modules(&self) -> bool {
        self.others.get()
    }
    fn window_close_prompt(&self) -> Option<String> {
        self.prompt.borrow().clone()
    }
    fn ask_to_close_window(&mut self, text: &str) {
        self.did(format!("ask: {text}"));
    }
    fn withdraw_close_confirmation(&mut self) {
        self.did("withdraw");
        self.confirmed.set(false);
    }
    fn close_window_confirmed(&mut self) {
        self.did("close_window_confirmed");
    }
    fn close_window(&mut self) {
        self.did("close_window");
    }
    fn refuse(&mut self, text: &str) {
        self.did(format!("refuse: {text}"));
    }
    fn toast(&mut self, text: &str) {
        self.did(format!("toast: {text}"));
    }
    fn every_second(&mut self, generation: u32) {
        self.ticks.borrow_mut().push(generation);
    }
    fn read_handoff(&mut self) {
        self.handoffs.set(self.handoffs.get() + 1);
    }
}

struct Flow {
    flow: RefCell<QuitFlow>,
    world: Rc<Fake>,
}
impl Flow {
    fn ask(&self, quit: EditorQuit) -> Result<AskedNvim, String> {
        self.flow
            .borrow_mut()
            .ask(&mut self.world.clone(), quit, Instant::now())
    }
    fn window_close(&self) -> WindowClose {
        self.flow
            .borrow_mut()
            .window_close(&mut self.world.clone(), Instant::now())
    }
    fn kill_editor(&self, scope: KillScope, confirmed: Option<String>) -> Result<(), String> {
        self.flow
            .borrow_mut()
            .kill_editor(&mut self.world.clone(), scope, confirmed, Instant::now())
    }
    fn cancelled(&self, g: u32) -> Cancel {
        self.flow.borrow_mut().cancelled(g)
    }
    fn nvim_exited(&self) {
        let settled = self.world.settled_watch();
        self.flow
            .borrow_mut()
            .nvim_exited(&mut self.world.clone(), settled, NvimEnd::Quit);
    }
    fn nvim_exited_from_a_start_screen(&self) {
        let settled = self.world.settled_watch();
        self.flow
            .borrow_mut()
            .nvim_exited(&mut self.world.clone(), settled, NvimEnd::QuitFromStartScreen);
    }
    fn nvim_crashed(&self) {
        let settled = self.world.settled_watch();
        self.flow
            .borrow_mut()
            .nvim_exited(&mut self.world.clone(), settled, NvimEnd::Crashed);
    }
    fn nvim_unreachable(&self) {
        self.flow.borrow_mut().nvim_unreachable(&mut self.world.clone());
    }
    fn window_close_declined(&self) {
        self.flow.borrow_mut().window_close_declined(&mut self.world.clone());
    }
    fn tick(&self) {
        let ticks = std::mem::take(&mut *self.world.ticks.borrow_mut());
        for g in ticks {
            if self.flow.borrow_mut().poll(&mut self.world.clone(), g, Instant::now()) {
                self.world.ticks.borrow_mut().push(g);
            }
        }
    }
}
impl Fake {
    /// As nvim-rs settles a request: once nvim's process is gone its IO loop has ended, so a
    /// request still outstanding comes back unanswered -- unless a test says otherwise.
    fn settled_watch(&self) -> Option<NvimAnswer> {
        self.settle_waits.set(self.settle_waits.get() + 1);
        if let Some(settled) = self.settled.borrow().clone() {
            return Some(settled);
        }
        let gone = !self.running.get() && !self.pending.get();
        self.watch.borrow().clone().map(|watch| match watch {
            NvimAnswer { done: false, .. } if gone => NvimAnswer {
                done: true,
                returned: false,
                ..watch
            },
            watch => watch,
        })
    }
}
fn flow(fake: &Rc<Fake>) -> Flow {
    Flow {
        flow: RefCell::new(QuitFlow::default()),
        world: fake.clone(),
    }
}

const RUNNING: &str = "close window? 1 running (y/n)";

/// The watch of a request nvim has been silent on for as long as a second request waits.
fn silent_watch() -> Option<NvimAnswer> {
    Some(NvimAnswer {
        sent_at: Instant::now() - super::super::close::NVIM_QUIT_PATIENCE,
        done: false,
        returned: false,
        last: None,
    })
}

#[test]
fn a_close_ends_the_window_only_once_nvim_cannot_answer_it() {
    // Sent: nvim may yet ask, and be told no.
    let fake = Fake::new();
    let quitting = flow(&fake);
    quitting.window_close();
    assert!(!quitting.flow.borrow().ends_the_window());
    // Silent, and closed again: nvim is being ended.
    *fake.watch.borrow_mut() = silent_watch();
    quitting.window_close();
    assert!(fake.log().contains(&"end_nvim".to_string()));
    assert!(quitting.flow.borrow().ends_the_window());

    // Out of reach: the close is unsent from the start.
    let fake = Fake::new();
    let quitting = flow(&fake);
    fake.running.set(false);
    fake.pending.set(true);
    quitting.window_close();
    assert!(quitting.flow.borrow().ends_the_window());
}

#[test]
fn ending_the_editor_alone_never_ends_the_window() {
    let fake = Fake::new();
    let quitting = flow(&fake);
    assert_eq!(quitting.kill_editor(KillScope::Module, None), Ok(()));
    *fake.watch.borrow_mut() = silent_watch();
    assert_eq!(quitting.kill_editor(KillScope::Module, None), Ok(()));
    assert!(fake.log().contains(&"end_nvim".to_string()));
    assert!(!quitting.flow.borrow().ends_the_window());

    let fake = Fake::new();
    let quitting = flow(&fake);
    fake.running.set(false);
    fake.pending.set(true);
    assert_eq!(quitting.kill_editor(KillScope::Module, None), Ok(()));
    assert!(!quitting.flow.borrow().ends_the_window());
}

/// A confirmed close must transfer its confirmation to nvim rather than leave it on the window.
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
        quitting.flow.borrow().in_flight.as_ref().map(|a| a.quit.clone()),
        Some(EditorQuit::Window {
            confirmed: Some(RUNNING.into())
        }),
        "the y travels with the quit instead"
    );
}

/// A running tab requires its own confirmation before nvim receives any quit request.
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

/// A prompt needs a visible editor; restoring its hidden state keeps the saved arrangement intact.
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

/// A returned request cancelled the close even when no cancellation letter arrives.
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

/// Polling a returned request removes the confirmation before another quit can use it.
#[test]
fn the_check_clears_a_quit_nvim_answered_without_quitting() {
    let fake = Fake::new();
    let quitting = flow(&fake);
    quitting.window_close();
    assert_eq!(fake.ticks.borrow().len(), 1);
    fake.answer(true, true, "n");
    quitting.tick();
    assert!(quitting.flow.borrow().in_flight.is_none());
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
    quitting.tick();
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
    let g = quitting.flow.borrow().in_flight.as_ref().unwrap().generation;
    assert_eq!(quitting.cancelled(g + 1), Cancel::Ignored);
    assert_eq!(quitting.cancelled(g), Cancel::Cleared);
    fake.running.set(false);
    fake.clear();
    quitting.nvim_exited();
    assert_eq!(fake.log(), ["close_window"]);
}

/// An unresponsive nvim is ended with its swap files retained; only its exit completes the close.
#[test]
fn a_second_close_ends_an_nvim_that_stopped_answering_and_waits_for_it() {
    let fake = Fake::new();
    let quitting = flow(&fake);
    quitting.window_close();
    // Not answered since the send, for longer than the patience.
    *fake.watch.borrow_mut() = Some(NvimAnswer {
        sent_at: Instant::now() - kill_rules::NVIM_QUIT_PATIENCE - Duration::from_secs(1),
        done: false,
        returned: false,
        last: None,
    });
    quitting.tick();
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

/// prefix x on an exited editor retires it.
#[test]
fn prefix_x_on_an_exited_editor_retires_it() {
    let fake = Fake::new();
    let quitting = flow(&fake);
    fake.running.set(false);
    fake.exited.set(true);
    quitting.kill_editor(KillScope::Module, None).unwrap();
    assert_eq!(fake.log(), ["end_editor"]);

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
    assert_eq!(fake.log(), ["end_editor"]);
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

/// a declined close after nvim exited retires the editor.
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
            "end_editor refused".to_string(),
            "show_stand_in".to_string(),
            "end_editor".to_string(),
            format!(
                "toast: {}",
                super::super::feedback::editor_ended_text(&crate::keymap::Keymap::defaults())
            )
        ]
    );
    fake.clear();
    quitting.window_close_declined();
    assert!(fake.log().is_empty(), "already retired");
}

/// The ended editor's text names no cause it cannot know, and says its key brings it
/// back (the layout saves an ended editor as it was before, not hidden).
#[test]
fn a_crash_nobody_asked_for_keeps_the_window_and_says_so() {
    let fake = Fake::new();
    let quitting = flow(&fake);
    fake.running.set(false);
    fake.exited.set(true);
    fake.editor_last_visible.set(true);
    quitting.nvim_crashed();
    assert_eq!(
        fake.log(),
        [
            "end_editor refused".to_string(),
            "show_stand_in".to_string(),
            "end_editor".to_string(),
            format!(
                "warn: {}",
                super::super::feedback::editor_crashed_text(&crate::keymap::Keymap::defaults())
            )
        ]
    );
}

#[test]
fn an_exit_after_a_requested_close_closes_even_with_no_leaving_line() {
    let fake = Fake::new();
    let quitting = flow(&fake);
    assert_eq!(quitting.window_close(), WindowClose::Wait);
    fake.clear();
    fake.running.set(false);
    fake.exited.set(true);
    quitting.nvim_crashed();
    assert!(
        fake.log()
            .iter()
            .any(|did| did.starts_with("close_window") || did == "close_after"),
        "nvim was asked to quit: {:?}",
        fake.log()
    );
}

#[test]
fn a_quit_typed_in_nvim_still_closes_the_window() {
    let fake = Fake::new();
    let quitting = flow(&fake);
    fake.running.set(false);
    fake.exited.set(true);
    quitting.nvim_exited();
    assert_eq!(fake.log(), ["close_window".to_string()]);
}

/// The window's own other modules are what a quit from a start screen would take with it.
#[test]
fn a_quit_from_a_start_screen_asks_when_the_window_holds_more() {
    let fake = Fake::new();
    let quitting = flow(&fake);
    fake.running.set(false);
    fake.exited.set(true);
    fake.others.set(true);
    quitting.nvim_exited_from_a_start_screen();
    assert_eq!(fake.log(), [format!("ask: {START_SCREEN_QUIT_PROMPT}")]);
    assert_eq!(START_SCREEN_QUIT_PROMPT, "close Eitri? (y/n)");
    assert_eq!(
        fake.handoffs.get(),
        1,
        "the last record is read as for any unasked exit"
    );
}

#[test]
fn a_declined_start_screen_quit_ends_the_editor_in_place() {
    let fake = Fake::new();
    let quitting = flow(&fake);
    fake.running.set(false);
    fake.exited.set(true);
    fake.others.set(true);
    quitting.nvim_exited_from_a_start_screen();
    fake.clear();
    quitting.window_close_declined();
    assert_eq!(
        fake.log(),
        [
            "end_editor".to_string(),
            format!(
                "toast: {}",
                super::super::feedback::editor_ended_text(&crate::keymap::Keymap::defaults())
            )
        ]
    );
}

#[test]
fn a_quit_from_a_start_screen_with_nothing_else_closes_at_once() {
    let fake = Fake::new();
    let quitting = flow(&fake);
    fake.running.set(false);
    fake.exited.set(true);
    quitting.nvim_exited_from_a_start_screen();
    assert_eq!(fake.log(), ["close_window".to_string()]);
}

/// The close's own question about running work already asks, and `n` to it keeps the window the same way.
#[test]
fn a_quit_from_a_start_screen_with_work_running_leaves_the_asking_to_the_close() {
    let fake = Fake::new();
    let quitting = flow(&fake);
    fake.running.set(false);
    fake.exited.set(true);
    fake.others.set(true);
    *fake.prompt.borrow_mut() = Some(RUNNING.into());
    quitting.nvim_exited_from_a_start_screen();
    assert_eq!(fake.log(), ["close_window".to_string()]);
}

#[test]
fn a_crash_from_a_start_screen_is_still_a_crash() {
    let fake = Fake::new();
    let quitting = flow(&fake);
    fake.running.set(false);
    fake.exited.set(true);
    fake.others.set(true);
    quitting.nvim_crashed();
    assert!(fake.log().iter().all(|did| !did.starts_with("ask")), "{:?}", fake.log());
}

#[test]
fn the_retired_editors_text_claims_no_cause() {
    let text = super::super::feedback::editor_ended_text(&crate::keymap::Keymap::defaults());
    assert!(!text.contains(":qa"));
    assert!(!text.contains("prefix x"));
    assert!(text.contains("C-b e starts it again"));
    let crashed = super::super::feedback::editor_crashed_text(&crate::keymap::Keymap::defaults());
    assert!(crashed.starts_with("nvim stopped unexpectedly"));
    assert!(crashed.contains("C-b e starts it again"));
}

/// A launcher exit does not prove nvim is gone, so the close waits for the pane to report its process exit.
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

/// a close asked before the launcher went completes when nvim exits.
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

/// repeated prefix x while nvim is out of reach retires the editor once.
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
    assert_eq!(fake.log(), ["end_editor"], "the editor only, never the window");
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

/// A fresh unsent generation cannot inherit the cancellation of an older answered request.
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
    assert_eq!(fake.log(), ["end_editor"], "the editor, as asked -- not the window");
}

/// a live old window close still wins over a newer kill.
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
fn a_kill_over_a_close_whose_cancel_is_late(fake: &Rc<Fake>, quitting: &Flow) -> u32 {
    *fake.prompt.borrow_mut() = Some(RUNNING.into());
    fake.confirmed.set(true);
    quitting.window_close();
    let close = quitting.flow.borrow().in_flight.as_ref().unwrap().generation;
    fake.answer(false, false, "r?");
    // Were the kill to wait for the worker, it would still see the request outstanding: the
    // answer comes after any bound the kill could wait out.
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
fn nvim_goes(fake: &Fake, quitting: &Flow) -> Vec<String> {
    fake.pending.set(false);
    fake.exited.set(true);
    fake.clear();
    quitting.nvim_exited();
    fake.log()
}

/// a close cancelled as the launcher went lends no y to a kill.
#[test]
fn a_close_cancelled_as_the_launcher_went_lends_no_y_to_a_kill() {
    let fake = Fake::new();
    let quitting = flow(&fake);
    let close = a_kill_over_a_close_whose_cancel_is_late(&fake, &quitting);
    assert_eq!(quitting.cancelled(close), Cancel::CloseWithdrawn);
    assert_eq!(
        nvim_goes(&fake, &quitting),
        ["end_editor"],
        "the editor, as asked -- not the window"
    );
}

/// The carried close retains its own generation until its late answer or letter can withdraw it.
#[test]
fn a_cancel_published_after_any_bound_still_takes_the_close_back() {
    let fake = Fake::new();
    let quitting = flow(&fake);
    a_kill_over_a_close_whose_cancel_is_late(&fake, &quitting);
    publish_the_cancel(&fake);
    assert_eq!(nvim_goes(&fake, &quitting), ["end_editor"]);

    // With the letter as well, read after the answer: the same.
    let fake = Fake::new();
    let quitting = flow(&fake);
    let close = a_kill_over_a_close_whose_cancel_is_late(&fake, &quitting);
    publish_the_cancel(&fake);
    assert_eq!(quitting.cancelled(close), Cancel::CloseWithdrawn);
    assert_eq!(nvim_goes(&fake, &quitting), ["end_editor"]);
}

/// An unknown close outcome cannot authorize closing the window; the surviving kill retires the editor.
#[test]
fn a_carried_close_still_unsettled_at_the_exit_is_not_trusted() {
    let fake = Fake::new();
    let quitting = flow(&fake);
    a_kill_over_a_close_whose_cancel_is_late(&fake, &quitting);
    assert_eq!(nvim_goes(&fake, &quitting), ["end_editor"]);
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
    let asked = quitting.flow.borrow().in_flight.clone().unwrap();
    assert_eq!((asked.quit, asked.lent_by), (EditorQuit::Module, None));
    *fake.settled.borrow_mut() = Some(NvimAnswer {
        sent_at: Instant::now(),
        done: true,
        returned: false,
        last: None,
    });
    assert_eq!(nvim_goes(&fake, &quitting), ["end_editor"]);
}

/// Repeating a kill preserves the original close generation, which its late letter still identifies.
#[test]
fn a_repeated_kill_carries_the_close_on_and_the_letter_still_finds_it() {
    let fake = Fake::new();
    let quitting = flow(&fake);
    let close = a_kill_over_a_close_whose_cancel_is_late(&fake, &quitting);
    assert_eq!(quitting.kill_editor(KillScope::Module, None), Ok(()));
    assert_eq!(quitting.flow.borrow().in_flight.as_ref().unwrap().lent_by, Some(close));
    assert_eq!(quitting.cancelled(close), Cancel::CloseWithdrawn);
    *fake.settled.borrow_mut() = Some(NvimAnswer {
        sent_at: Instant::now(),
        done: true,
        returned: false,
        last: None,
    });
    assert_eq!(nvim_goes(&fake, &quitting), ["end_editor"]);
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

/// Joining an unreachable nvim reads the watch without waiting, even while its prompt is open.
#[test]
fn a_request_that_sends_nothing_never_waits_on_the_ui_thread() {
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

/// The exit must use the settled answer supplied by the host rather than trust a stale confirmation.
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
    *fake.settled.borrow_mut() = Some(NvimAnswer {
        sent_at: now,
        done: true,
        returned: true,
        last: None,
    });
    fake.running.set(false);
    fake.exited.set(true);
    fake.clear();
    quitting.nvim_exited();
    assert_eq!(fake.log(), ["close_window"], "the ordinary close, which asks again");
}

/// a quit still unsettled at the exit is not taken as approved.
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
    assert_eq!(quitting.ask(EditorQuit::Module), Ok(AskedNvim::Asked));
    let g = quitting.flow.borrow().in_flight.as_ref().unwrap().generation;
    assert_eq!(
        quitting
            .flow
            .borrow_mut()
            .quit_not_sent(&mut fake.clone(), g)
            .as_deref(),
        Some("nvim could not be asked to quit — quit it in the editor (:qa)")
    );
    assert!(quitting.flow.borrow().in_flight.is_none());
}

#[test]
fn a_quit_that_could_not_be_sent_is_cleared_and_refused_with_todays_text() {
    let fake = Fake::new();
    let mut world = fake.clone();
    let mut flow = QuitFlow::default();
    flow.ask(&mut world, EditorQuit::Module, Instant::now()).unwrap();
    let g = flow.in_flight.as_ref().unwrap().generation;
    assert_eq!(flow.quit_not_sent(&mut world, g.wrapping_add(1)), None);
    assert!(flow.in_flight.is_some());
    assert_eq!(
        flow.quit_not_sent(&mut world, g).as_deref(),
        Some("nvim could not be asked to quit — quit it in the editor (:qa)")
    );
    assert!(flow.in_flight.is_none());
    assert_eq!(flow.quit_not_sent(&mut world, g), None);
    assert_eq!(fake.log(), ["focus_editor", "send_quit"]);
    flow.ask(&mut world, EditorQuit::Module, Instant::now()).unwrap();
    assert_eq!(flow.quit_not_sent(&mut world, g), None);
    assert!(flow.in_flight.is_some());
}

#[test]
fn quit_events_keep_their_window_and_settled_answer() {
    use super::super::{start::event_window, EditorEvent, HostEvent, WinId};

    let win = WinId(2);
    let settled = Some(NvimAnswer {
        sent_at: Instant::now(),
        done: true,
        returned: false,
        last: None,
    });
    let exited = EditorEvent::Exited {
        win,
        settled: settled.clone(),
    };
    let EditorEvent::Exited { settled: answer, .. } = &exited else {
        unreachable!()
    };
    assert_eq!(*answer, settled);
    for event in [
        exited,
        EditorEvent::Unreachable { win },
        EditorEvent::QuitNotSent { win, generation: 7 },
    ] {
        assert_eq!(event_window(&HostEvent::Editor(event)), Some(win));
    }
}

/// Every exit that leaves the window reads what nvim left at its exit here, a kill of the editor included, so that
/// record is kept while the window lives on; a window close reads it at its teardown.
#[test]
fn every_exit_but_a_window_close_reads_the_last_record() {
    let fake = Fake::new();
    let quitting = flow(&fake);
    fake.running.set(false);
    quitting.nvim_exited();
    assert_eq!(fake.handoffs.get(), 1, "a :qa");
    assert_eq!(fake.log(), ["close_window"]);

    let fake = Fake::new();
    let quitting = flow(&fake);
    fake.running.set(false);
    quitting.nvim_crashed();
    assert_eq!(fake.handoffs.get(), 1, "a crash");

    let fake = Fake::new();
    let quitting = flow(&fake);
    quitting.window_close();
    fake.running.set(false);
    quitting.nvim_exited();
    assert_eq!(fake.handoffs.get(), 0, "a window close");

    let fake = Fake::new();
    let quitting = flow(&fake);
    quitting.kill_editor(KillScope::Module, None).unwrap();
    fake.running.set(false);
    quitting.nvim_exited();
    assert_eq!(fake.handoffs.get(), 1, "a kill of the editor");
}
