//! The editor's poll on the main run loop, run only while it has something to do: a repeating `NSTimer` every
//! [`crate::editor::TICK`] while [`Editor::wait`](crate::editor::Editor::wait) says so, and otherwise one dispatch
//! read source on the main queue per [`Editor::wake_fds`](crate::editor::Editor::wake_fds) descriptor, so an idle
//! window is not woken by the editor's half. What to do after each poll is the editor's answer; this file only does
//! what it says.
//!
//! The timer runs in the run loop's common modes and the main queue is serviced there too, so a pane switch or a
//! colourscheme is still read while a menu is open or the window is being resized.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::time::Instant;

use block2::RcBlock;
use dispatch2::{DispatchObject, DispatchQueue, DispatchRetained, DispatchSource};
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_foundation::NSTimer;

use super::page::repeat;
use crate::assembly::Assembly;
use crate::editor::EditorWait;

/// Polls the editor from the attach until [`EditorTicker::stop`].
pub struct EditorTicker {
    mtm: MainThreadMarker,
    assembly: Rc<Assembly>,
    timer: RefCell<Option<Retained<NSTimer>>>,
    watches: RefCell<Vec<DispatchRetained<DispatchSource>>>,
    stopped: std::cell::Cell<bool>,
    me: Weak<EditorTicker>,
}

impl EditorTicker {
    /// Starts on the timer: the link has just been aimed at nvim and is connecting.
    pub fn start(mtm: MainThreadMarker, assembly: Rc<Assembly>) -> Rc<EditorTicker> {
        let ticker = Rc::new_cyclic(|me| EditorTicker {
            mtm,
            assembly,
            timer: RefCell::new(None),
            watches: RefCell::new(Vec::new()),
            stopped: std::cell::Cell::new(false),
            me: me.clone(),
        });
        ticker.arm();
        ticker
    }

    /// Does what the editor said after a poll.
    fn follow(&self, wait: EditorWait) {
        match wait {
            EditorWait::Tick => self.arm(),
            EditorWait::Sockets => self.watch(),
            EditorWait::Stop => self.stop(),
        }
    }

    fn arm(&self) {
        if self.stopped.get() || self.timer.borrow().is_some() {
            return;
        }
        let me = self.me.clone();
        let timer = repeat(self.mtm, crate::editor::TICK, move || {
            let Some(ticker) = me.upgrade() else { return false };
            ticker.assembly.poll_editor(Instant::now());
            match ticker.assembly.editor.wait(Instant::now()) {
                EditorWait::Tick => true,
                other => {
                    // Returning `false` invalidates this timer.
                    ticker.timer.borrow_mut().take();
                    ticker.follow(other);
                    false
                }
            }
        });
        *self.timer.borrow_mut() = Some(timer);
    }

    /// One read source per descriptor; the first to fire cancels them all, polls, and the editor's answer says
    /// what waits next. A descriptor stays readable until the poll reads it, so nothing that arrived between the
    /// poll and the watch is missed.
    fn watch(&self) {
        if self.stopped.get() || !self.watches.borrow().is_empty() {
            return;
        }
        let mut watches = Vec::new();
        for fd in self.assembly.editor.wake_fds() {
            // SAFETY: a read source on a descriptor the editor keeps open until its shutdown, which comes after the
            // window's teardown has cancelled every source here; it delivers to the main queue, whose blocks run on
            // the main thread, where the handler's values belong. `_dispatch_source_type_read` is libdispatch's own
            // constant.
            let source = unsafe {
                DispatchSource::new(
                    &raw const dispatch2::_dispatch_source_type_read as *mut _,
                    fd as usize,
                    0,
                    Some(DispatchQueue::main()),
                )
            };
            let me = self.me.clone();
            let handler = RcBlock::new(move || {
                if let Some(ticker) = me.upgrade() {
                    ticker.on_ready();
                }
            });
            // SAFETY: libdispatch copies the block; it runs on the main queue only (above).
            unsafe { source.set_event_handler_with_block(RcBlock::as_ptr(&handler)) };
            source.activate();
            watches.push(source);
        }
        *self.watches.borrow_mut() = watches;
    }

    fn on_ready(&self) {
        let watches: Vec<_> = self.watches.borrow_mut().drain(..).collect();
        if watches.is_empty() {
            // A source cancelled after this block was queued: whoever cancelled it already polled.
            return;
        }
        for source in watches {
            source.cancel();
        }
        self.assembly.poll_editor(Instant::now());
        self.follow(self.assembly.editor.wait(Instant::now()));
    }

    /// Stops polling for good: the timer and every watch. Idempotent.
    pub fn stop(&self) {
        self.stopped.set(true);
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.invalidate();
        }
        for source in self.watches.borrow_mut().drain(..) {
            source.cancel();
        }
    }

    /// The sources waiting on the editor's descriptors now: what a test reads to see them cancelled.
    pub fn watch_sources(&self) -> Vec<DispatchRetained<DispatchSource>> {
        self.watches.borrow().clone()
    }

    /// Whether the editor is polled on the timer now (`false` while it waits on its sockets, and once stopped).
    pub fn is_ticking(&self) -> bool {
        self.timer.borrow().is_some()
    }

    /// Whether the editor waits on its sockets now.
    pub fn is_watching(&self) -> bool {
        !self.watches.borrow().is_empty()
    }
}

impl Drop for EditorTicker {
    /// A source libdispatch holds armed outlives its last owner, and a handler that can no longer reach the
    /// ticker would leave a readable descriptor firing on the main queue for good.
    fn drop(&mut self) {
        self.stop();
    }
}
