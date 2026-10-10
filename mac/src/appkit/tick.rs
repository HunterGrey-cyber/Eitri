//! The panel's tick on the main run loop, run only when it is due: a dispatch read source on the main queue
//! watches the panel's wake ([`PanelPump::wake_fd`]), and one one-shot `NSTimer` is armed for the moment the
//! tick is due next. What each call returns is decided by [`PanelPump`], which is host-neutral and tested on
//! every OS; this file only does what it says.
//!
//! Both run in the run loop's common modes (the main queue is serviced there, and the timer is added there), so
//! the panel keeps streaming while a menu is open or the window is being resized. An idle panel is not woken at
//! all.

use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::{Rc, Weak};
use std::time::Instant;

use block2::RcBlock;
use dispatch2::{DispatchObject, DispatchQueue, DispatchRetained, DispatchSource};
use eitri_panel::agent_panel::AgentPanelHandle;
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_foundation::{NSRunLoop, NSRunLoopCommonModes, NSTimer};

use super::page::repeat;
use crate::panel_pump::{PanelPump, Timer};

/// Runs the panel's tick for the window's life; [`PanelTicker::stop`] ends it.
pub struct PanelTicker {
    pump: PanelPump,
    timer: RefCell<Option<Retained<NSTimer>>>,
    source: RefCell<Option<DispatchRetained<DispatchSource>>>,
    /// The panel has no wake: the tick runs on its steady cadence, as it did before the wake existed.
    steady: RefCell<Option<Retained<NSTimer>>>,
    me: Weak<PanelTicker>,
}

impl PanelTicker {
    /// Starts the tick. The first one runs on the loop's next turn: whatever the window set up before is sent
    /// by it.
    pub fn start(mtm: MainThreadMarker, panel: AgentPanelHandle) -> Rc<PanelTicker> {
        let ticker = Rc::new_cyclic(|me| PanelTicker {
            pump: PanelPump::new(panel.clone()),
            timer: RefCell::new(None),
            source: RefCell::new(None),
            steady: RefCell::new(None),
            me: me.clone(),
        });
        let Some(fd) = ticker.pump.wake_fd() else {
            // No wake (the socket pair could not be made): the tick runs every `TICK`, until it says it is done.
            crate::log_line!(
                "[mac] the panel has no wake; it ticks every {:?}",
                AgentPanelHandle::TICK
            );
            *ticker.steady.borrow_mut() = Some(repeat(mtm, AgentPanelHandle::TICK, move || panel.tick()));
            return ticker;
        };
        // SAFETY: a read source on a descriptor the panel keeps open for its life (the source is cancelled at the
        // latest when the panel stops), delivering to the main queue, whose blocks run on the main thread, where
        // the handler's values belong. `_dispatch_source_type_read` is libdispatch's own constant.
        let source = unsafe {
            DispatchSource::new(
                &raw const dispatch2::_dispatch_source_type_read as *mut _,
                fd as usize,
                0,
                Some(DispatchQueue::main()),
            )
        };
        let me = Rc::downgrade(&ticker);
        let handler = RcBlock::new(move || {
            if let Some(ticker) = me.upgrade() {
                let timer = ticker.pump.on_wake(Instant::now());
                ticker.act(timer);
            }
        });
        // SAFETY: libdispatch copies the block; it runs on the main queue only (above).
        unsafe { source.set_event_handler_with_block(RcBlock::as_ptr(&handler)) };
        source.activate();
        *ticker.source.borrow_mut() = Some(source);
        let first = ticker.pump.start(Instant::now());
        ticker.act(first);
        ticker
    }

    /// Does what the pump said.
    fn act(&self, timer: Timer) {
        match timer {
            Timer::Arm(at) => self.arm(at),
            Timer::Keep => {}
            Timer::Stop => self.stop(),
        }
    }

    /// Replaces the armed timer with one for `at`.
    fn arm(&self, at: Instant) {
        if let Some(old) = self.timer.borrow_mut().take() {
            old.invalidate();
        }
        let Some(me) = self.me.upgrade() else { return };
        let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
            // Taken out before the tick runs: the tick may arm the next one.
            me.timer.borrow_mut().take();
            let timer = me.pump.on_timer(Instant::now());
            me.act(timer);
        });
        // SAFETY: a one-shot timer added to the main run loop in its common modes (an extern static Foundation
        // defines); the run loop retains it and its block until it fires or is invalidated.
        let timer = unsafe {
            // An `NSTimer` never fires before its date, so the wait needs no rounding.
            let wait = at.saturating_duration_since(Instant::now()).as_secs_f64();
            let timer = NSTimer::timerWithTimeInterval_repeats_block(wait, false, &block);
            NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
            timer
        };
        *self.timer.borrow_mut() = Some(timer);
    }

    /// Stops the tick for good: the timer, the watch on the wake and the steady cadence. Idempotent.
    pub fn stop(&self) {
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.invalidate();
        }
        if let Some(timer) = self.steady.borrow_mut().take() {
            timer.invalidate();
        }
        if let Some(source) = self.source.borrow_mut().take() {
            source.cancel();
        }
    }

    /// Whether the tick is still driven.
    pub fn is_running(&self) -> bool {
        self.source.borrow().is_some() || self.steady.borrow().is_some()
    }
}
