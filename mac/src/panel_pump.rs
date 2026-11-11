//! The panel's tick as the Mac window runs it, without AppKit: the AppKit half watches [`PanelPump::wake_fd`]
//! for reading (a dispatch read source on the main queue) and keeps one one-shot timer, and does what these
//! calls return. An idle panel is not woken at all; the tick runs when an agent's events arrive, when the
//! window calls into the panel, at the typing cadence's next release, and on its steady cadence only while
//! something it acts on cannot say when it is ready (`eitri_panel::panel_wake`).

use std::cell::{Cell, RefCell};
use std::os::fd::RawFd;
use std::time::Instant;

use eitri_panel::agent_panel::AgentPanelHandle;
use eitri_panel::panel_wake::{NextTick, TickSchedule};

/// What the host does with its timer after a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timer {
    /// Cancel the armed timer, if any, and arm it for this moment.
    Arm(Instant),
    /// Leave the timer as it is (armed or not).
    Keep,
    /// The panel is done: cancel the timer and stop watching the descriptor.
    Stop,
}

pub struct PanelPump {
    handle: AgentPanelHandle,
    schedule: RefCell<TickSchedule>,
    stopped: Cell<bool>,
}

impl PanelPump {
    pub fn new(handle: AgentPanelHandle) -> PanelPump {
        PanelPump {
            handle,
            schedule: RefCell::new(TickSchedule::new()),
            stopped: Cell::new(false),
        }
    }

    /// The descriptor to watch for reading. `None`: the panel has no wake, and the host runs the tick every
    /// `AgentPanelHandle::TICK` by arming its timer for what [`on_timer`](Self::on_timer) returns.
    pub fn wake_fd(&self) -> Option<RawFd> {
        self.handle.wake_fd()
    }

    /// The first tick runs at once: whatever the window set up before its loop runs is sent by it.
    pub fn start(&self, now: Instant) -> Timer {
        match self.schedule.borrow_mut().on_wake(now) {
            Some(at) => Timer::Arm(at),
            None => Timer::Keep,
        }
    }

    /// The descriptor is readable.
    pub fn on_wake(&self, now: Instant) -> Timer {
        if self.stopped.get() {
            return Timer::Stop;
        }
        let Some(due) = self.handle.take_wake(now) else {
            return Timer::Keep;
        };
        match self.schedule.borrow_mut().on_wake(due) {
            Some(at) => Timer::Arm(at),
            None => Timer::Keep,
        }
    }

    /// The timer fired: runs the tick.
    pub fn on_timer(&self, now: Instant) -> Timer {
        if self.stopped.get() {
            return Timer::Stop;
        }
        self.schedule.borrow_mut().fired();
        let next = self.handle.run_tick(now);
        if next == NextTick::Stop {
            self.stopped.set(true);
            return Timer::Stop;
        }
        match self.schedule.borrow_mut().after_tick(next) {
            Some(at) => Timer::Arm(at),
            None => Timer::Keep,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Page {
        sent: RefCell<Vec<String>>,
    }

    impl eitri_panel::panel_page::PageSurface for Page {
        fn send(&self, envelope_json: &str) {
            self.sent.borrow_mut().push(envelope_json.to_owned());
        }
        fn is_visible(&self) -> bool {
            true
        }
        fn load_document(&self, _html: &str, _base_uri: &str) {}
        fn set_background(&self, _rgb: [u8; 3]) {}
    }

    /// An idle panel driven the way the Mac window drives it is not ticked; a call into it still is, at
    /// once, and the close stops the pump.
    #[test]
    fn an_idle_panel_is_not_ticked_and_a_call_still_is() {
        let page = std::rc::Rc::new(Page::default());
        let handle = AgentPanelHandle::for_host_test(page.clone());
        handle.page_message(
            eitri_panel::agent_panel::ViewId::BASE,
            r#"{"type":"ready","request_id":"r1"}"#,
        );
        let pump = PanelPump::new(handle.clone());
        let now = Instant::now();
        assert_eq!(pump.start(now), Timer::Arm(now));
        assert_eq!(pump.on_wake(now), Timer::Keep, "the ready wake shares the first tick");
        assert_eq!(pump.on_timer(now), Timer::Keep, "idle leaves no timer armed");
        let before = handle.ticks_run();
        let later = now + std::time::Duration::from_secs(3600);
        assert_eq!(pump.on_wake(later), Timer::Keep);
        assert_eq!(handle.ticks_run(), before);

        let sent = page.sent.borrow().len();
        handle.new_tab();
        let Timer::Arm(due) = pump.on_wake(later) else {
            panic!("the call arms a tick")
        };
        assert_eq!(pump.on_timer(due), Timer::Keep);
        assert_eq!(handle.ticks_run(), before + 1, "the call woke one tick");
        assert!(page.sent.borrow().len() > sent, "and what it changed reached the page");

        let _watch = handle.shutdown();
        assert_ne!(pump.on_wake(later), Timer::Stop, "the close's wake asks for a tick");
        assert_eq!(
            pump.on_timer(later + AgentPanelHandle::TICK),
            Timer::Stop,
            "which stops the pump"
        );
        assert_eq!(pump.on_wake(later), Timer::Stop, "stopped for good");
    }
}
