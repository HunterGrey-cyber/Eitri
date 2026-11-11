//! Frame boundaries for GTK tests; time only limits a wait for a missing frame.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::prelude::*;

pub fn until(what: &str, mut done: impl FnMut() -> bool) {
    let context = gtk4::glib::MainContext::default();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        context.iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Requests and observes three complete frames on each mapped toplevel. Each request follows the previous paint,
/// so a frame already under way cannot satisfy the next boundary.
pub fn settle() {
    for widget in gtk4::Window::list_toplevels() {
        if !widget.is_mapped() {
            continue;
        }
        let Some(clock) = widget.frame_clock() else { continue };
        let paints = Rc::new(Cell::new(0));
        let observed = paints.clone();
        let handler = clock.connect_after_paint(move |_| observed.set(observed.get() + 1));
        for _ in 0..3 {
            let before = paints.get();
            widget.queue_draw();
            clock.request_phase(gtk4::gdk::FrameClockPhase::PAINT);
            until("the next paint", || paints.get() > before);
        }
        clock.disconnect(handler);
    }
    // Destruction and notifications with no mapped window still get repeated dispatch opportunities.
    for _ in 0..20 {
        gtk4::glib::MainContext::default().iteration(false);
    }
}

/// Sources attached after this marker belong to the input action the test is about to send.
pub struct TimerMark(u32);
impl TimerMark {
    pub fn now() -> Self {
        let marker =
            gtk4::glib::source::idle_source_new(None, gtk4::glib::Priority::LOW, || gtk4::glib::ControlFlow::Break);
        let id = marker.attach(Some(&gtk4::glib::MainContext::default())).as_raw();
        marker.destroy();
        Self(id)
    }

    pub fn expire(&self) {
        use gtk4::glib::translate::ToGlibPtr;
        let context = gtk4::glib::MainContext::default();
        for id in self.0..Self::now().0 {
            // SAFETY: lookups and changes are on this context's owning thread; only live timed sources are changed.
            unsafe {
                let source = gtk4::glib::ffi::g_main_context_find_source_by_id(context.to_glib_none().0, id);
                if !source.is_null() && gtk4::glib::ffi::g_source_get_ready_time(source) > 0 {
                    gtk4::glib::ffi::g_source_set_ready_time(source, 0);
                }
            }
        }
    }
}
