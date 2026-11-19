//! Whether a quit the window sees first may go on, what follows the user's answer, and whether the window
//! outlives an nvim that ended.
//!
//! Three requests reach the window before nvim hears of them: the quit chord (the key monitor sees it before the
//! menu does), the menu's Quit item (by pointer, or by whatever chord it carries), and the window's close button.
//! While a turn runs, a message is queued or a tab has background work left running, each is held and a question is
//! put; the answer `Quit` sends the request on its way, through the path it would have taken. AppKit's own
//! `terminate:` (the Dock's Quit, a logout, a script's `quit app`) never passes through the window and asks nothing.
//!
//! **When nvim ends, the rule is the Linux window's** (`eitri_core::wm::quit`): an nvim that said it was leaving
//! (`:qa`, or the quit this window sent) is the window's close, which asks the same question while something is at
//! work and keeps the window, its editor ended, on Cancel; one that went without saying so (a crash, a `kill`) keeps
//! the window and says how to start it again. While the editor has ended, a quit that may go on ends the app here:
//! Neovide's own Quit and close would go to the nvim that is gone.

use std::time::{Duration, Instant};

use eitri_core::tabs::window_close_prompt;

/// Where a quit request came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitSource {
    /// The quit chord, seen by the key monitor.
    Key,
    /// The menu's Quit item.
    Menu,
    /// The window's close button.
    CloseButton,
    /// nvim quit by itself (`:qa`) while something was at work.
    Editor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuitDecision {
    /// Nothing to ask about: the request goes on now.
    Proceed,
    /// Hold the request and ask; `what` says what is at work.
    Ask { what: String },
    /// A question is already open, or this is a repeat of a chord that was held: the request is dropped.
    AlreadyAsking,
    /// The editor has ended: nothing is asked, and the window ends the app itself.
    EndApp,
}

/// What the window does after the user chose to quit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitAfter {
    /// Send the quit chord to nvim: the path the menu's Quit takes.
    SendQuitKey,
    /// Close the window again; the close requested within [`PASS_FOR`] passes without a question.
    CloseAgain,
    /// The editor has ended: the window ends the app itself.
    EndApp,
}

/// What nvim said of its own end before it went (the editor watch, `editor_watch.lua`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NvimSaid {
    /// Nothing: it never reached its watch (a start that failed, a snippet that could not be loaded).
    #[default]
    Nothing,
    /// It will say when it leaves: an end with nothing more said is a crash.
    Watched,
    /// It is quitting on purpose.
    Leaving,
    /// It is quitting on purpose from a start screen's buffer (`eitri_core::wm::window::is_start_screen`): the quit may
    /// have been the second `q` of a plugin's float, which closed the float and then the dashboard behind it.
    LeavingStartScreen,
}

/// What the window says it asks when nvim quit from a start screen with nothing at work (the Linux window asks
/// `eitri_core::wm::quit::START_SCREEN_QUIT_PROMPT`).
pub const START_SCREEN_QUESTION: &str = "nvim was quit from its start screen";

/// What the window does when nvim has ended and no new nvim took its place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorEnded {
    /// The window closes with it, and the app quits.
    Close,
    /// The window stays, its editor ended, and asks before it closes; `what` says what is at work.
    Ask { what: String },
    /// The window stays, its editor ended, and asks whether to close: nvim was quit from a start screen.
    AskFromStartScreen,
    /// The window stays with its editor ended. `crashed`: nvim went without saying it was leaving.
    Keep { crashed: bool },
}

/// How long the pass for a close the user already answered stays good. Closing again reaches the close request
/// at once; a pass still unused after this belongs to a close that never came, and must not let a later close
/// (another turn, perhaps) through unasked.
pub const PASS_FOR: Duration = Duration::from_secs(2);

/// How long the user's Quit stays the answer for the end of the nvim it went to. nvim may hold the quit at its own
/// prompt for an unsaved buffer, or refuse it, and nothing here learns of either; an end later than this is a new
/// one, and asks again if anything is at work. Long enough for nvim to quit with its plugins shutting down.
pub const CONFIRMED_FOR: Duration = Duration::from_secs(10);

/// What is at work, as the window's close question counts it: turns running (a waiting question counts as one),
/// messages queued, and tabs with live background work, which quitting stops.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AtWork {
    pub running: usize,
    pub queued: usize,
    pub background: usize,
}

/// What is at work, as the question says it: `1 running, 2 queued, 1 with background tasks`. `None` when nothing is.
pub fn at_work(work: AtWork) -> Option<String> {
    window_close_prompt(work.running, work.queued, work.background)?;
    let mut parts = Vec::new();
    if work.running > 0 {
        parts.push(format!("{} running", work.running));
    }
    if work.queued > 0 {
        parts.push(format!("{} queued", work.queued));
    }
    if work.background > 0 {
        parts.push(format!("{} with background tasks", work.background));
    }
    Some(parts.join(", "))
}

#[derive(Debug, Default)]
pub struct QuitGate {
    /// The open question: where the request came from and what it asked about.
    asking: Option<(QuitSource, String)>,
    pass_close_until: Option<Instant>,
    /// The last press of the quit chord was held (asked about, or dropped): its repeats are dropped too.
    chord_held: bool,
    /// What the user chose to quit over, and until when (`CONFIRMED_FOR`): the end of the nvim the quit went to asks
    /// nothing about it again.
    confirmed: Option<(String, Instant)>,
    /// The editor has ended and the window stayed.
    ended: bool,
    /// nvim was started again: an end of the new one that said nothing (it never reached its watch) is no quit of
    /// the user's.
    restarting: bool,
    /// A quit of the window's that went on with nothing to ask, and until when (`CONFIRMED_FOR`): the end of the nvim
    /// it went to is the window's own close, which asks nothing more, a start screen's question included.
    let_through: Option<Instant>,
}

impl QuitGate {
    pub fn request(&mut self, source: QuitSource, work: AtWork, now: Instant) -> QuitDecision {
        if source == QuitSource::CloseButton {
            if let Some(until) = self.pass_close_until.take() {
                if now < until {
                    return self.go_on(now);
                }
            }
        }
        if self.asking.is_some() {
            return QuitDecision::AlreadyAsking;
        }
        match at_work(work) {
            None => self.go_on(now),
            Some(what) => {
                self.asking = Some((source, what.clone()));
                QuitDecision::Ask { what }
            }
        }
    }

    /// A request that may go on: on its own way, or, with the editor ended, to the end of the app.
    fn go_on(&mut self, now: Instant) -> QuitDecision {
        if self.ended {
            QuitDecision::EndApp
        } else {
            self.let_through = Some(now + CONFIRMED_FOR);
            QuitDecision::Proceed
        }
    }

    /// The quit chord, `repeat` when it is the key's auto-repeat. A repeat never asks a second question: while
    /// one is open, or when the press it repeats was held, it is dropped.
    pub fn request_chord(&mut self, repeat: bool, work: AtWork, now: Instant) -> QuitDecision {
        if repeat && (self.asking.is_some() || self.chord_held) {
            return QuitDecision::AlreadyAsking;
        }
        let decision = self.request(QuitSource::Key, work, now);
        self.chord_held = decision != QuitDecision::Proceed;
        decision
    }

    /// The user answered the open question. `None` when there was none, or for Cancel: nothing happens.
    pub fn answer(&mut self, quit: bool, now: Instant) -> Option<QuitAfter> {
        let (source, what) = self.asking.take()?;
        if !quit {
            return None;
        }
        if self.ended {
            return Some(QuitAfter::EndApp);
        }
        // The quit goes to nvim, whose end follows; that end must not ask about the same work again.
        self.confirmed = Some((what, now + CONFIRMED_FOR));
        Some(match source {
            QuitSource::Key | QuitSource::Menu => QuitAfter::SendQuitKey,
            QuitSource::CloseButton => self.close_again(now),
            // Asked only once the editor has ended, which is handled above.
            QuitSource::Editor => QuitAfter::EndApp,
        })
    }

    /// nvim ended with no new nvim in its place; `said` is what it said of that end, `work` what is at work now.
    ///
    /// As the Linux window decides it (`eitri_core::wm::quit::QuitFlow::nvim_exited`): an nvim that said it would
    /// say when it leaves and ended without saying so crashed, and the window stays; so does an nvim started again
    /// that ended before it was watching. Any other end is the window's close: it closes when nothing is at work, or
    /// when what is at work is what the user chose to quit over within `CONFIRMED_FOR`, and asks otherwise; a question
    /// already open
    /// decides for both. A crash keeps the window even after the user chose to quit (the Linux window closes then):
    /// nothing here learns that nvim's own prompt for an unsaved buffer was cancelled, so the choice may be old, and
    /// a quit of the user's always says it is leaving.
    pub fn nvim_ended(&mut self, said: NvimSaid, work: AtWork, now: Instant) -> EditorEnded {
        let confirmed = self
            .confirmed
            .take()
            .filter(|(_, until)| now < *until)
            .map(|(what, _)| what);
        let let_through = self.let_through.take().is_some_and(|until| now < until);
        let restarting = std::mem::take(&mut self.restarting);
        let decided = if said == NvimSaid::Watched {
            EditorEnded::Keep { crashed: true }
        } else if restarting && said == NvimSaid::Nothing {
            EditorEnded::Keep { crashed: false }
        } else if self.asking.is_some() {
            EditorEnded::Keep { crashed: false }
        } else {
            match at_work(work) {
                // As the Linux window: a quit from a start screen asks first, unless it was the window's own.
                None if said == NvimSaid::LeavingStartScreen && confirmed.is_none() && !let_through => {
                    self.asking = Some((QuitSource::Editor, START_SCREEN_QUESTION.to_owned()));
                    EditorEnded::AskFromStartScreen
                }
                None => EditorEnded::Close,
                Some(what) if confirmed.as_deref() == Some(what.as_str()) => EditorEnded::Close,
                Some(what) => {
                    self.asking = Some((QuitSource::Editor, what.clone()));
                    EditorEnded::Ask { what }
                }
            }
        };
        self.ended = decided != EditorEnded::Close;
        decided
    }

    /// A new nvim was asked for in the window whose editor ended.
    pub fn restarted(&mut self) {
        self.ended = false;
        self.restarting = true;
    }

    /// Whether the editor has ended and the window stayed.
    pub fn editor_ended(&self) -> bool {
        self.ended
    }

    /// The window closes again and that close must not ask (also when the quit chord could not reach nvim).
    pub fn close_again(&mut self, now: Instant) -> QuitAfter {
        self.pass_close_until = Some(now + PASS_FOR);
        QuitAfter::CloseAgain
    }

    pub fn is_asking(&self) -> bool {
        self.asking.is_some()
    }
}

/// The panel's close, waited for: steps `watch` every `CLOSE_WATCH_POLL` until it holds nothing (every session
/// closed, the saved tabs' last write on disk, or each one's backstop passed: 5 s for the write), and says how long
/// that took. The app's shutdown runs it last, on the main thread, after the window's own sources have stopped:
/// AppKit ends the process once a terminate's notification returns, and Neovide's loop has already ended on the
/// other way out, so this wait is what keeps the process alive for them, as the application hold does on Linux.
pub fn wait_for_the_close(watch: eitri_panel::agent_panel::ShutdownWatch) -> Duration {
    wait_for_close_with(watch, Instant::now, || {
        std::thread::sleep(eitri_panel::agent_panel::CLOSE_WATCH_POLL)
    })
}

fn wait_for_close_with(
    mut watch: eitri_panel::agent_panel::ShutdownWatch,
    mut now: impl FnMut() -> Instant,
    mut pause: impl FnMut(),
) -> Duration {
    let started = now();
    while watch.step(now()) {
        pause();
    }
    now().saturating_duration_since(started)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoPage;
    impl eitri_panel::panel_page::PageSurface for NoPage {
        fn send(&self, _envelope_json: &str) {}
        fn is_visible(&self) -> bool {
            true
        }
        fn load_document(&self, _html: &str, _base_uri: &str) {}
        fn set_background(&self, _rgb: [u8; 3]) {}
    }

    /// The close hands the saved tabs to their writer and the app's shutdown waits until the write is on disk:
    /// the writer waits until the close has been observed holding, and the tab is in the file after it.
    #[test]
    fn the_close_waits_for_the_saved_tabs_last_write() {
        use std::sync::{mpsc, Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        let (files, project) = (dir.path().join("tabs"), dir.path().join("project"));
        std::fs::create_dir_all(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let (entered, writing) = mpsc::channel();
        let entered = Mutex::new(entered);
        let panel = eitri_panel::agent_panel::AgentPanelHandle::for_host_test_with_saved_tabs(
            std::rc::Rc::new(NoPage),
            files.clone(),
            project.clone(),
            "s-held",
            Arc::new(move || {
                entered.lock().unwrap().send(()).unwrap();
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(60))
                    .unwrap();
            }),
        );
        let clock = std::cell::Cell::new(Instant::now());
        let watch = panel.shutdown();
        writing
            .recv_timeout(Duration::from_secs(60))
            .expect("the write started");
        let guard = Instant::now() + Duration::from_secs(60);
        let mut waits = 0;
        wait_for_close_with(
            watch,
            || clock.get(),
            || {
                waits += 1;
                if waits <= 3 {
                    clock.set(clock.get() + eitri_panel::agent_panel::CLOSE_WATCH_POLL);
                }
                if waits == 3 {
                    release_tx.send(()).unwrap();
                }
                assert!(
                    Instant::now() < guard,
                    "the close never finished after releasing the write"
                );
                std::thread::yield_now();
            },
        );
        assert!(waits >= 3, "the close must keep holding until the write is released");
        let saved = match eitri_core::saved_tabs::load(&files, &project) {
            eitri_core::saved_tabs::Loaded::Saved(saved) => saved,
            other => panic!("{other:?}"),
        };
        let ids: Vec<_> = saved.tabs.iter().map(|t| t.provider_session_id.as_str()).collect();
        assert_eq!(ids, ["s-held"]);
    }

    /// How many times `flat` (code with its whitespace removed) calls the panel's tick and takes its wake.
    fn tick_and_wake_calls(flat: &str) -> (usize, usize) {
        // The name and its parenthesis, whatever comes before: a method call and a path call both count.
        (flat.matches("run_tick(").count(), flat.matches("take_wake(").count())
    }

    /// A call spelled as a path (`Handle::run_tick(&h, now)`) is a call too.
    #[test]
    fn the_scan_counts_a_call_spelled_as_a_path() {
        assert_eq!(tick_and_wake_calls("h.run_tick(now);h.take_wake(now);"), (1, 1));
        assert_eq!(
            tick_and_wake_calls("AgentPanelHandle::run_tick(&h,now);AgentPanelHandle::take_wake(&h,now);"),
            (1, 1)
        );
    }

    /// The app's every way out waits for the panel's close, and the panel's tick is the pump's: started at the
    /// attach, stopped by the teardown, and no fixed timer ticks the panel. Read from the AppKit half's source,
    /// which builds only on macOS.
    #[test]
    fn the_app_waits_for_the_close_and_ticks_the_panel_through_its_pump() {
        use eitri_core::source_scan as scan;
        let sources = scan::rust_sources(&["mac/src/appkit"]);
        let flat = |code: &str| code.split_whitespace().collect::<String>();
        let app = scan::code_only(&scan::file_with(&sources, "pub fn finish(").text);
        let finish = scan::functions(&app, "finish");
        assert_eq!(finish.len(), 1);
        assert!(
            flat(finish[0]).contains("crate::quit::wait_for_the_close(assembly.panel.shutdown())"),
            "{}",
            finish[0]
        );
        let window = scan::code_only(&scan::file_with(&sources, "pub fn attach(").text);
        let attach = scan::functions(&window, "attach");
        assert!(flat(attach[0]).contains("PanelTicker::start(self.mtm,assembly.panel.clone())"));
        let teardown = scan::functions(&window, "teardown");
        assert!(flat(teardown[0]).contains("tick.stop()"));
        // Every way into the panel's tick, the wake's take included (a second taker would swallow raises the pump
        // waits for), outside test modules, across the whole crate: only the pump runs and takes, and only the
        // ticker's fallback for a panel without a wake calls `tick()`.
        let crate_sources = scan::rust_sources(&["mac/src"]);
        let mut pump_calls = (0, 0);
        for source in &crate_sources {
            let code = scan::code_only(&source.text);
            let code = flat(&scan::without(&code, &scan::modules(&code, "tests")));
            let tick_calls = code.matches(".tick()").count();
            let allowed = usize::from(code.contains("pubstructPanelTicker"));
            assert!(
                tick_calls <= allowed,
                "{:?} ticks the panel outside its pump",
                source.path
            );
            let (runs, takes) = tick_and_wake_calls(&code);
            if code.contains("pubstructPanelPump") {
                pump_calls = (runs, takes);
            } else {
                assert_eq!(
                    (runs, takes),
                    (0, 0),
                    "{:?} runs the panel's tick or takes its wake outside the pump",
                    source.path
                );
            }
        }
        assert_eq!(
            pump_calls,
            (1, 1),
            "the pump runs the tick and takes the wake, once each"
        );
    }

    fn w(running: usize, queued: usize) -> AtWork {
        AtWork {
            running,
            queued,
            background: 0,
        }
    }

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn nothing_at_work_quits_at_once() {
        let now = Instant::now();
        let mut gate = QuitGate::default();
        for source in [QuitSource::Key, QuitSource::Menu, QuitSource::CloseButton] {
            assert_eq!(gate.request(source, w(0, 0), now), QuitDecision::Proceed, "{source:?}");
        }
        assert_eq!(gate.request_chord(false, w(0, 0), now), QuitDecision::Proceed);
        assert_eq!(gate.request_chord(true, w(0, 0), now), QuitDecision::Proceed);
        assert!(!gate.is_asking());
    }

    #[test]
    fn a_running_turn_asks_and_cancel_keeps_everything() {
        let now = Instant::now();
        let mut gate = QuitGate::default();
        assert_eq!(
            gate.request_chord(false, w(1, 2), now),
            QuitDecision::Ask {
                what: "1 running, 2 queued".into()
            }
        );
        assert_eq!(
            gate.request(QuitSource::CloseButton, w(1, 0), now),
            QuitDecision::AlreadyAsking
        );
        assert_eq!(
            gate.request(QuitSource::Menu, w(1, 0), now),
            QuitDecision::AlreadyAsking
        );
        assert_eq!(gate.answer(false, now), None);
        assert!(!gate.is_asking());
        // Asked again next time: a cancelled answer is not remembered.
        assert!(matches!(
            gate.request_chord(false, w(1, 0), now),
            QuitDecision::Ask { .. }
        ));
    }

    #[test]
    fn a_held_or_doubled_quit_chord_never_gets_past_the_question() {
        let now = Instant::now();
        let mut gate = QuitGate::default();
        assert!(matches!(
            gate.request_chord(false, w(1, 0), now),
            QuitDecision::Ask { .. }
        ));
        // The key's repeats, and a second press, while the question is open.
        assert_eq!(gate.request_chord(true, w(1, 0), now), QuitDecision::AlreadyAsking);
        assert_eq!(gate.request_chord(false, w(1, 0), now), QuitDecision::AlreadyAsking);
        // Cancelled while the key is still down: its repeats ask nothing more.
        assert_eq!(gate.answer(false, now), None);
        assert_eq!(gate.request_chord(true, w(1, 0), now), QuitDecision::AlreadyAsking);
        // A fresh press asks again.
        assert!(matches!(
            gate.request_chord(false, w(1, 0), now),
            QuitDecision::Ask { .. }
        ));
    }

    #[test]
    fn quit_from_the_key_or_the_menu_sends_the_key_on() {
        let now = Instant::now();
        for source in [QuitSource::Key, QuitSource::Menu] {
            let mut gate = QuitGate::default();
            let _ = gate.request(source, w(0, 1), now);
            assert_eq!(gate.answer(true, now), Some(QuitAfter::SendQuitKey));
            assert_eq!(gate.answer(true, now), None, "one answer per question");
        }
    }

    #[test]
    fn quit_from_the_close_button_closes_again_once_without_asking() {
        let start = Instant::now();
        let mut gate = QuitGate::default();
        let _ = gate.request(QuitSource::CloseButton, w(1, 0), start);
        assert_eq!(gate.answer(true, start), Some(QuitAfter::CloseAgain));
        assert_eq!(
            gate.request(QuitSource::CloseButton, w(1, 0), at(start, 10)),
            QuitDecision::Proceed
        );
        // Only that one close: nvim may have refused it (an unsaved buffer), and the next one asks.
        assert!(matches!(
            gate.request(QuitSource::CloseButton, w(1, 0), at(start, 20)),
            QuitDecision::Ask { .. }
        ));
    }

    #[test]
    fn a_pass_for_a_close_that_never_came_expires() {
        let start = Instant::now();
        let mut gate = QuitGate::default();
        let _ = gate.close_again(start);
        let later = start + PASS_FOR + Duration::from_millis(1);
        assert!(matches!(
            gate.request(QuitSource::CloseButton, w(1, 0), later),
            QuitDecision::Ask { .. }
        ));
    }

    #[test]
    fn the_pass_is_for_a_close_only() {
        let now = Instant::now();
        let mut gate = QuitGate::default();
        let _ = gate.close_again(now);
        assert!(matches!(
            gate.request_chord(false, w(1, 0), now),
            QuitDecision::Ask { .. }
        ));
    }

    #[test]
    fn what_is_at_work_reads_like_the_linux_question() {
        assert_eq!(at_work(w(0, 0)), None);
        assert_eq!(at_work(w(2, 0)).as_deref(), Some("2 running"));
        assert_eq!(at_work(w(0, 1)).as_deref(), Some("1 queued"));
        let background = AtWork {
            background: 1,
            ..AtWork::default()
        };
        assert_eq!(at_work(background).as_deref(), Some("1 with background tasks"));
    }

    /// A turn that ended with a shell, monitor or workflow still running leaves nothing running or queued; quitting
    /// would still stop that work, so the quit asks, as the Linux window's close does.
    #[test]
    fn background_work_left_by_a_finished_turn_asks_before_quitting() {
        let now = Instant::now();
        let work = AtWork {
            background: 2,
            ..AtWork::default()
        };
        for source in [QuitSource::Key, QuitSource::Menu, QuitSource::CloseButton] {
            let mut gate = QuitGate::default();
            assert_eq!(
                gate.request(source, work, now),
                QuitDecision::Ask {
                    what: "2 with background tasks".into()
                },
                "{source:?}"
            );
        }
        assert_eq!(
            tabs_window_prompt(work),
            Some("close window? 2 with background tasks (y/n)".to_string())
        );
    }

    /// `:qa` with nothing at work closes the window, as on Linux: the chat is on disk and nothing runs.
    #[test]
    fn nvims_own_quit_with_nothing_at_work_closes_the_window() {
        for said in [NvimSaid::Leaving, NvimSaid::Nothing] {
            let mut gate = QuitGate::default();
            assert_eq!(
                gate.nvim_ended(said, w(0, 0), Instant::now()),
                EditorEnded::Close,
                "{said:?}"
            );
            assert!(!gate.editor_ended());
        }
    }

    /// `:qa` while a turn runs asks the window's question; Cancel keeps the window with its editor ended, and from
    /// then on a quit that may go on ends the app (Neovide's own would go to the nvim that is gone).
    #[test]
    fn nvims_own_quit_while_at_work_asks_and_cancel_keeps_the_window() {
        let now = Instant::now();
        let mut gate = QuitGate::default();
        assert_eq!(
            gate.nvim_ended(NvimSaid::Leaving, w(1, 0), Instant::now()),
            EditorEnded::Ask {
                what: "1 running".into()
            }
        );
        assert!(gate.editor_ended() && gate.is_asking());
        assert_eq!(
            gate.request(QuitSource::Menu, w(1, 0), now),
            QuitDecision::AlreadyAsking
        );
        assert_eq!(gate.answer(false, now), None);
        assert!(gate.editor_ended());
        assert!(matches!(
            gate.request(QuitSource::CloseButton, w(1, 0), now),
            QuitDecision::Ask { .. }
        ));
        assert_eq!(gate.answer(true, now), Some(QuitAfter::EndApp));
        assert_eq!(gate.request(QuitSource::Menu, w(0, 0), now), QuitDecision::EndApp);
        assert_eq!(gate.request_chord(false, w(0, 0), now), QuitDecision::EndApp);
    }

    #[test]
    fn quit_to_the_question_nvims_end_put_ends_the_app() {
        let now = Instant::now();
        let mut gate = QuitGate::default();
        let _ = gate.nvim_ended(NvimSaid::Leaving, w(0, 1), Instant::now());
        assert_eq!(gate.answer(true, now), Some(QuitAfter::EndApp));
    }

    /// nvim went without saying it was leaving: the window stays and says so, whatever is at work, and even when the
    /// user had chosen to quit.
    #[test]
    fn a_crash_keeps_the_window() {
        let now = Instant::now();
        for work in [w(0, 0), w(2, 1)] {
            let mut gate = QuitGate::default();
            assert_eq!(
                gate.nvim_ended(NvimSaid::Watched, work, Instant::now()),
                EditorEnded::Keep { crashed: true }
            );
            assert!(gate.editor_ended() && !gate.is_asking());
        }
        let mut gate = QuitGate::default();
        let _ = gate.request(QuitSource::Menu, w(1, 0), now);
        assert_eq!(gate.answer(true, now), Some(QuitAfter::SendQuitKey));
        assert_eq!(
            gate.nvim_ended(NvimSaid::Watched, w(1, 0), Instant::now()),
            EditorEnded::Keep { crashed: true }
        );
    }

    /// The user chose Quit over what was at work; the nvim that quit for it asks nothing about the same work again,
    /// and asks about more.
    #[test]
    fn a_quit_already_answered_closes_unless_more_is_at_work() {
        let now = Instant::now();
        for source in [QuitSource::Key, QuitSource::Menu, QuitSource::CloseButton] {
            let mut gate = QuitGate::default();
            let _ = gate.request(source, w(1, 0), now);
            assert!(gate.answer(true, now).is_some());
            assert_eq!(
                gate.nvim_ended(NvimSaid::Leaving, w(1, 0), Instant::now()),
                EditorEnded::Close,
                "{source:?}"
            );
        }
        let mut gate = QuitGate::default();
        let _ = gate.request(QuitSource::Key, w(1, 0), now);
        let _ = gate.answer(true, now);
        assert_eq!(
            gate.nvim_ended(NvimSaid::Leaving, w(1, 1), Instant::now()),
            EditorEnded::Ask {
                what: "1 running, 1 queued".into()
            }
        );
        // The answer is used once: the next nvim's end asks again.
        let mut gate = QuitGate::default();
        let _ = gate.request(QuitSource::Key, w(1, 0), now);
        let _ = gate.answer(true, now);
        let _ = gate.nvim_ended(NvimSaid::Leaving, w(1, 0), Instant::now());
        gate.restarted();
        assert!(matches!(
            gate.nvim_ended(NvimSaid::Leaving, w(1, 0), Instant::now()),
            EditorEnded::Ask { .. }
        ));
    }

    /// A Quit answered long before nvim ends (nvim's own prompt held or refused it, and a later `:qa` ended it) is no
    /// answer for that end: it asks again.
    #[test]
    fn an_old_answer_does_not_close_a_later_end_unasked() {
        let start = Instant::now();
        let mut gate = QuitGate::default();
        let _ = gate.request(QuitSource::Key, w(1, 0), start);
        assert_eq!(gate.answer(true, start), Some(QuitAfter::SendQuitKey));
        assert!(matches!(
            gate.nvim_ended(NvimSaid::Leaving, w(1, 0), start + CONFIRMED_FOR),
            EditorEnded::Ask { .. }
        ));
        let mut gate = QuitGate::default();
        let _ = gate.request(QuitSource::Key, w(1, 0), start);
        let _ = gate.answer(true, start);
        assert_eq!(
            gate.nvim_ended(
                NvimSaid::Leaving,
                w(1, 0),
                start + CONFIRMED_FOR - Duration::from_millis(1)
            ),
            EditorEnded::Close
        );
    }

    /// A quit from a start screen with nothing at work asks first, as the Linux window does; Cancel keeps the window and
    /// Quit ends the app. One typed in a file closes the window, and so does a start screen's when the quit was the
    /// window's own.
    #[test]
    fn a_quit_from_a_start_screen_asks_first_unless_the_window_sent_it() {
        let now = Instant::now();
        let mut gate = QuitGate::default();
        assert_eq!(
            gate.nvim_ended(NvimSaid::LeavingStartScreen, w(0, 0), now),
            EditorEnded::AskFromStartScreen
        );
        assert!(gate.is_asking() && gate.editor_ended());
        assert_eq!(gate.answer(false, now), None);
        assert!(gate.editor_ended());
        let mut gate = QuitGate::default();
        let _ = gate.nvim_ended(NvimSaid::LeavingStartScreen, w(0, 0), now);
        assert_eq!(gate.answer(true, now), Some(QuitAfter::EndApp));

        assert_eq!(
            QuitGate::default().nvim_ended(NvimSaid::Leaving, w(0, 0), now),
            EditorEnded::Close
        );

        for source in [QuitSource::Key, QuitSource::Menu, QuitSource::CloseButton] {
            let mut gate = QuitGate::default();
            assert_eq!(gate.request(source, w(0, 0), now), QuitDecision::Proceed);
            assert_eq!(
                gate.nvim_ended(NvimSaid::LeavingStartScreen, w(0, 0), now),
                EditorEnded::Close,
                "{source:?}"
            );
        }
        // A quit let through long ago is no answer for a later start screen's quit.
        let mut gate = QuitGate::default();
        let _ = gate.request(QuitSource::Key, w(0, 0), now);
        assert_eq!(
            gate.nvim_ended(NvimSaid::LeavingStartScreen, w(0, 0), now + CONFIRMED_FOR),
            EditorEnded::AskFromStartScreen
        );
        // With work at work the work's own question is asked.
        assert!(matches!(
            QuitGate::default().nvim_ended(NvimSaid::LeavingStartScreen, w(1, 0), now),
            EditorEnded::Ask { .. }
        ));
    }

    /// A question open when nvim ends (the quit chord's, say) decides for both: the window stays until it is answered.
    #[test]
    fn an_open_question_keeps_the_window_when_nvim_ends() {
        let now = Instant::now();
        let mut gate = QuitGate::default();
        let _ = gate.request(QuitSource::Key, w(1, 0), now);
        assert_eq!(
            gate.nvim_ended(NvimSaid::Leaving, w(1, 0), Instant::now()),
            EditorEnded::Keep { crashed: false }
        );
        assert_eq!(gate.answer(true, now), Some(QuitAfter::EndApp));
    }

    /// nvim started again: a quit goes its own way once more; an end before the new nvim watches is no quit of the
    /// user's and keeps the window.
    #[test]
    fn a_restart_hands_quits_back_to_nvim_and_an_early_end_keeps_the_window() {
        let now = Instant::now();
        let mut gate = QuitGate::default();
        let _ = gate.nvim_ended(NvimSaid::Watched, w(0, 0), Instant::now());
        gate.restarted();
        assert!(!gate.editor_ended());
        assert_eq!(gate.request(QuitSource::Menu, w(0, 0), now), QuitDecision::Proceed);
        assert_eq!(
            gate.nvim_ended(NvimSaid::Nothing, w(0, 0), Instant::now()),
            EditorEnded::Keep { crashed: false }
        );
        gate.restarted();
        assert_eq!(
            gate.nvim_ended(NvimSaid::Leaving, w(0, 0), Instant::now()),
            EditorEnded::Close
        );
    }

    /// While the editor has ended the close button's pass ends the app too.
    #[test]
    fn the_close_buttons_pass_ends_the_app_once_the_editor_ended() {
        let now = Instant::now();
        let mut gate = QuitGate::default();
        let _ = gate.nvim_ended(NvimSaid::Watched, w(0, 0), Instant::now());
        let _ = gate.close_again(now);
        assert_eq!(
            gate.request(QuitSource::CloseButton, w(1, 0), now),
            QuitDecision::EndApp
        );
    }

    fn tabs_window_prompt(work: AtWork) -> Option<String> {
        window_close_prompt(work.running, work.queued, work.background)
    }
}
