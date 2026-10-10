//! Whether a quit the window sees first may go on, and what follows the user's answer.
//!
//! Three requests reach the window before nvim hears of them: the quit chord (the key monitor sees it before the
//! menu does), the menu's Quit item (by pointer, or by whatever chord it carries), and the window's close button.
//! While a turn runs or a message is queued, each is held and a question is put; the answer `Quit` sends the
//! request on its way, through the path it would have taken. A quit nvim decides for itself (`:qa`) asks nothing
//! here, as on Linux; nor does AppKit's own `terminate:` (the Dock's Quit, a logout, a script's `quit app`),
//! which never passes through the window.

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuitDecision {
    /// Nothing to ask about: the request goes on now.
    Proceed,
    /// Hold the request and ask; `what` says what is at work.
    Ask { what: String },
    /// A question is already open, or this is a repeat of a chord that was held: the request is dropped.
    AlreadyAsking,
}

/// What the window does after the user chose to quit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitAfter {
    /// Send the quit chord to nvim: the path the menu's Quit takes.
    SendQuitKey,
    /// Close the window again; the close requested within [`PASS_FOR`] passes without a question.
    CloseAgain,
}

/// How long the pass for a close the user already answered stays good. Closing again reaches the close request
/// at once; a pass still unused after this belongs to a close that never came, and must not let a later close
/// (another turn, perhaps) through unasked.
pub const PASS_FOR: Duration = Duration::from_secs(2);

/// What is at work, as the question says it: `1 running, 2 queued`. `None` when nothing is.
pub fn at_work(running: usize, queued: usize) -> Option<String> {
    window_close_prompt(running, queued)?;
    let mut parts = Vec::new();
    if running > 0 {
        parts.push(format!("{running} running"));
    }
    if queued > 0 {
        parts.push(format!("{queued} queued"));
    }
    Some(parts.join(", "))
}

#[derive(Debug, Default)]
pub struct QuitGate {
    asking: Option<QuitSource>,
    pass_close_until: Option<Instant>,
    /// The last press of the quit chord was held (asked about, or dropped): its repeats are dropped too.
    chord_held: bool,
}

impl QuitGate {
    pub fn request(&mut self, source: QuitSource, running: usize, queued: usize, now: Instant) -> QuitDecision {
        if source == QuitSource::CloseButton {
            if let Some(until) = self.pass_close_until.take() {
                if now < until {
                    return QuitDecision::Proceed;
                }
            }
        }
        if self.asking.is_some() {
            return QuitDecision::AlreadyAsking;
        }
        match at_work(running, queued) {
            None => QuitDecision::Proceed,
            Some(what) => {
                self.asking = Some(source);
                QuitDecision::Ask { what }
            }
        }
    }

    /// The quit chord, `repeat` when it is the key's auto-repeat. A repeat never asks a second question: while
    /// one is open, or when the press it repeats was held, it is dropped.
    pub fn request_chord(&mut self, repeat: bool, running: usize, queued: usize, now: Instant) -> QuitDecision {
        if repeat && (self.asking.is_some() || self.chord_held) {
            return QuitDecision::AlreadyAsking;
        }
        let decision = self.request(QuitSource::Key, running, queued, now);
        self.chord_held = decision != QuitDecision::Proceed;
        decision
    }

    /// The user answered the open question. `None` when there was none, or for Cancel: nothing happens.
    pub fn answer(&mut self, quit: bool, now: Instant) -> Option<QuitAfter> {
        let source = self.asking.take()?;
        if !quit {
            return None;
        }
        Some(match source {
            QuitSource::Key | QuitSource::Menu => QuitAfter::SendQuitKey,
            QuitSource::CloseButton => self.close_again(now),
        })
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
pub fn wait_for_the_close(mut watch: eitri_panel::agent_panel::ShutdownWatch) -> Duration {
    let started = Instant::now();
    while watch.step(Instant::now()) {
        std::thread::sleep(eitri_panel::agent_panel::CLOSE_WATCH_POLL);
    }
    started.elapsed()
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
    /// held for 300 ms on the writer thread, the wait lasts that long, and the tab is in the file after it.
    #[test]
    fn the_close_waits_for_the_saved_tabs_last_write() {
        use std::sync::{mpsc, Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        let (files, project) = (dir.path().join("tabs"), dir.path().join("project"));
        std::fs::create_dir_all(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let panel = eitri_panel::agent_panel::AgentPanelHandle::for_host_test_with_saved_tabs(
            std::rc::Rc::new(NoPage),
            files.clone(),
            project.clone(),
            "s-held",
            Arc::new(move || {
                let _ = release_rx.lock().unwrap().recv_timeout(Duration::from_secs(10));
            }),
        );
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            let _ = release_tx.send(());
        });
        let took = wait_for_the_close(panel.shutdown());
        release.join().unwrap();
        assert!(
            took >= Duration::from_millis(250),
            "the wait ended before the write did: {took:?}"
        );
        assert!(took < Duration::from_secs(5), "and ended once it was done: {took:?}");
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

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn nothing_at_work_quits_at_once() {
        let now = Instant::now();
        let mut gate = QuitGate::default();
        for source in [QuitSource::Key, QuitSource::Menu, QuitSource::CloseButton] {
            assert_eq!(gate.request(source, 0, 0, now), QuitDecision::Proceed, "{source:?}");
        }
        assert_eq!(gate.request_chord(false, 0, 0, now), QuitDecision::Proceed);
        assert_eq!(gate.request_chord(true, 0, 0, now), QuitDecision::Proceed);
        assert!(!gate.is_asking());
    }

    #[test]
    fn a_running_turn_asks_and_cancel_keeps_everything() {
        let now = Instant::now();
        let mut gate = QuitGate::default();
        assert_eq!(
            gate.request_chord(false, 1, 2, now),
            QuitDecision::Ask {
                what: "1 running, 2 queued".into()
            }
        );
        assert_eq!(
            gate.request(QuitSource::CloseButton, 1, 0, now),
            QuitDecision::AlreadyAsking
        );
        assert_eq!(gate.request(QuitSource::Menu, 1, 0, now), QuitDecision::AlreadyAsking);
        assert_eq!(gate.answer(false, now), None);
        assert!(!gate.is_asking());
        // Asked again next time: a cancelled answer is not remembered.
        assert!(matches!(gate.request_chord(false, 1, 0, now), QuitDecision::Ask { .. }));
    }

    #[test]
    fn a_held_or_doubled_quit_chord_never_gets_past_the_question() {
        let now = Instant::now();
        let mut gate = QuitGate::default();
        assert!(matches!(gate.request_chord(false, 1, 0, now), QuitDecision::Ask { .. }));
        // The key's repeats, and a second press, while the question is open.
        assert_eq!(gate.request_chord(true, 1, 0, now), QuitDecision::AlreadyAsking);
        assert_eq!(gate.request_chord(false, 1, 0, now), QuitDecision::AlreadyAsking);
        // Cancelled while the key is still down: its repeats ask nothing more.
        assert_eq!(gate.answer(false, now), None);
        assert_eq!(gate.request_chord(true, 1, 0, now), QuitDecision::AlreadyAsking);
        // A fresh press asks again.
        assert!(matches!(gate.request_chord(false, 1, 0, now), QuitDecision::Ask { .. }));
    }

    #[test]
    fn quit_from_the_key_or_the_menu_sends_the_key_on() {
        let now = Instant::now();
        for source in [QuitSource::Key, QuitSource::Menu] {
            let mut gate = QuitGate::default();
            let _ = gate.request(source, 0, 1, now);
            assert_eq!(gate.answer(true, now), Some(QuitAfter::SendQuitKey));
            assert_eq!(gate.answer(true, now), None, "one answer per question");
        }
    }

    #[test]
    fn quit_from_the_close_button_closes_again_once_without_asking() {
        let start = Instant::now();
        let mut gate = QuitGate::default();
        let _ = gate.request(QuitSource::CloseButton, 1, 0, start);
        assert_eq!(gate.answer(true, start), Some(QuitAfter::CloseAgain));
        assert_eq!(
            gate.request(QuitSource::CloseButton, 1, 0, at(start, 10)),
            QuitDecision::Proceed
        );
        // Only that one close: nvim may have refused it (an unsaved buffer), and the next one asks.
        assert!(matches!(
            gate.request(QuitSource::CloseButton, 1, 0, at(start, 20)),
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
            gate.request(QuitSource::CloseButton, 1, 0, later),
            QuitDecision::Ask { .. }
        ));
    }

    #[test]
    fn the_pass_is_for_a_close_only() {
        let now = Instant::now();
        let mut gate = QuitGate::default();
        let _ = gate.close_again(now);
        assert!(matches!(gate.request_chord(false, 1, 0, now), QuitDecision::Ask { .. }));
    }

    #[test]
    fn what_is_at_work_reads_like_the_linux_question() {
        assert_eq!(at_work(0, 0), None);
        assert_eq!(at_work(2, 0).as_deref(), Some("2 running"));
        assert_eq!(at_work(0, 1).as_deref(), Some("1 queued"));
    }
}
