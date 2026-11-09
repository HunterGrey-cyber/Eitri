//! The editor presented on a Wayland subsurface below the window, with the GTK path as the fallback.

mod buffers;
mod child;
mod foreign;
mod geometry;
mod log;
mod pacer;
mod setting;
mod thread;
mod types;

pub(crate) use foreign::connection_for;
pub(crate) use log::PaneLog;
pub use setting::Presentation;
pub(crate) use setting::{decide, effective, Decision, Facts};
pub use thread::{ConnectionProbe, PresenterThread, TestCommand};
pub use types::{BufferKey, DmabufFeedback, FrameData, Globals, PaneId, PresentEvent, PresentSink};

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use gtk4::{gdk, glib, graphene, prelude::*};
use wayland_client::Proxy;

use buffers::{Acquire, PoolChange, WlBufferPool};
use child::{Child, WaylandRequests};
use geometry::{os_scale, placement, Placement};
use pacer::{Action, Hole, Pacer};
use thread::ThreadState;

use crate::dmabuf_target::{DmabufPool, ModifierSource};
use crate::editor_area::{EditorGlArea, PresentationCounts};
use crate::{FrameReason, RenderFn, RenderTarget};

pub(crate) fn subsurface_target(
    placement: &Placement,
    buffer: (i32, i32),
    framebuffer: u32,
    scale: f64,
) -> RenderTarget {
    RenderTarget::Subsurface {
        framebuffer,
        buffer,
        content: placement.content,
        os_scale: os_scale(placement, scale),
    }
}

pub(crate) fn paints_cover(hole: Hole) -> bool {
    hole == Hole::Closed
}

/// The parts of an allocation of `size` that a child showing a frame of `shown` (both logical, from the allocation's
/// origin) leaves bare: a strip on the right and one along the bottom. A grown allocation outruns the child's frame
/// until a buffer of the new size exists, and the parent must cover what the child does not, or the window behind it
/// shows through.
pub(crate) fn outside_shown(size: (f32, f32), shown: (f32, f32)) -> Vec<graphene::Rect> {
    let mut rects = Vec::new();
    if size.0 > shown.0 && size.1 > 0.0 {
        rects.push(graphene::Rect::new(shown.0, 0.0, size.0 - shown.0, size.1));
    }
    let width = size.0.min(shown.0);
    if size.1 > shown.1 && width > 0.0 {
        rects.push(graphene::Rect::new(0.0, shown.1, width, size.1 - shown.1));
    }
    rects
}

/// What the editor area's own snapshot draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotPlan {
    /// The GTK path draws the frame.
    Gtk,
    /// The child shows through the open hole, so the parent draws nothing over the child's frame; only the part of the
    /// allocation that frame does not reach ([`SubsurfacePane::outside_frame`]) gets the cover colour.
    Nothing,
    /// The child is not shown yet or any more; the parent paints the cover colour.
    Cover,
}

pub(crate) fn snapshot_plan(pane: Option<&SubsurfacePane>) -> SnapshotPlan {
    match pane.filter(|pane| pane.owns_snapshot()) {
        None => SnapshotPlan::Gtk,
        Some(pane) if paints_cover(pane.hole()) => SnapshotPlan::Cover,
        Some(_) => SnapshotPlan::Nothing,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Geometry {
    placement: Placement,
    scale: f64,
}

impl Geometry {
    /// The editor's own part: what its surface is sent and what a frame is rendered for.
    fn without_underlay(self) -> Geometry {
        Geometry {
            placement: self.placement.without_underlay(),
            ..self
        }
    }
}

// Keeping the transition's effects behind this boundary lets it run without a display.
trait PaneIo {
    fn log(&self) -> Option<Rc<PaneLog>> {
        None
    }
    fn count(&self, _update: fn(&mut PresentationCounts)) {}
    fn transform(&self) -> (f64, f64) {
        (0.0, 0.0)
    }
    fn surface(&self) -> Option<gdk::Surface> {
        None
    }
    /// None while the parent/globals are unavailable; true when a new child replaced the old one.
    fn setup(&self) -> Result<Option<bool>, String>;
    fn geometry(&self) -> Option<Geometry>;
    fn render(&self, geometry: Geometry, transaction: bool) -> Result<Option<u64>, String>;
    fn move_to(&self, geometry: Geometry);
    fn queue_transaction(&self);
    fn queue_render(&self);
    fn hole_changed(&self);
    fn hide(&self);
    fn after_paint(&self);
    fn destroy(&self);
    fn unsubscribe(&self);
    fn pool_event(&self, event: &PresentEvent) -> PoolChange;
}

/// GTK owns the transitions and all drawing; the presentation thread only supplies events.
pub(crate) struct SubsurfacePane {
    io: Box<dyn PaneIo>,
    log: Option<Rc<PaneLog>>,
    pending_seq: Cell<Option<u64>>,
    pacer: RefCell<Pacer>,
    active: Cell<bool>,
    mapped: Cell<bool>,
    globals: Cell<bool>,
    transaction: Cell<bool>,
    drawing: Cell<bool>,
    switched: Cell<bool>,
    again: Cell<bool>,
    blocked: Cell<bool>,
    last: Cell<Option<Geometry>>,
    destroy_pending: Cell<bool>,
    /// A hide asked for while a draw is running; the value is whether the hole was open when it was asked.
    hide_pending: Cell<Option<bool>>,
    stale: RefCell<Option<glib::SourceId>>,
    signals: RefCell<Vec<(glib::Object, glib::SignalHandlerId)>>,
    surface_signals: RefCell<Vec<(glib::Object, glib::SignalHandlerId)>>,
    weak: Weak<Self>,
}

impl SubsurfacePane {
    fn with_io(io: Box<dyn PaneIo>) -> Rc<Self> {
        Rc::new_cyclic(|weak| Self {
            log: io.log(),
            pending_seq: Cell::new(None),
            io,
            pacer: RefCell::new(Pacer::new()),
            active: Cell::new(true),
            mapped: Cell::new(false),
            globals: Cell::new(false),
            transaction: Cell::new(false),
            drawing: Cell::new(false),
            switched: Cell::new(false),
            again: Cell::new(false),
            blocked: Cell::new(false),
            last: Cell::new(None),
            destroy_pending: Cell::new(false),
            hide_pending: Cell::new(None),
            stale: RefCell::new(None),
            signals: RefCell::new(Vec::new()),
            surface_signals: RefCell::new(Vec::new()),
            weak: weak.clone(),
        })
    }

    pub(crate) fn start(area: &EditorGlArea, thread: Rc<PresenterThread>, render: RenderFn) -> Rc<Self> {
        let id = thread.new_pane_id();
        let pane = Self::with_io(Box::new(WaylandPane {
            area: area.downgrade(),
            thread: thread.clone(),
            id,
            render,
            resources: RefCell::new(None),
            last_seq: Cell::new(0),
        }));
        area.set_subsurface(Some(pane.clone()));
        if let Some(clock) = area.frame_clock() {
            let weak = Rc::downgrade(&pane);
            let id = clock.connect_local("layout", true, move |_| {
                if let Some(pane) = weak.upgrade() {
                    pane.layout();
                }
                None
            });
            pane.signals.borrow_mut().push((clock.clone().upcast(), id));
            let weak = Rc::downgrade(&pane);
            let id = clock.connect_after_paint(move |_| {
                if let Some(pane) = weak.upgrade() {
                    pane.after_paint();
                }
            });
            pane.signals.borrow_mut().push((clock.upcast(), id));
        }
        pane.watch_surface();
        let weak = Rc::downgrade(&pane);
        let display = area.display();
        let closed_id = display.connect_closed(move |_, _| {
            if let Some(pane) = weak.upgrade() {
                pane.stop();
            }
        });
        pane.signals.borrow_mut().push((display.upcast(), closed_id));
        let sink: Rc<dyn PresentSink> = pane.clone();
        #[cfg(feature = "fault-injection")]
        {
            if log::fault_requested(log::Fault::Thread) {
                pane.event(PresentEvent::ThreadFailed(
                    "injected presentation thread failure".into(),
                ));
                return pane;
            }
            if log::fault_requested(log::Fault::Globals) {
                pane.event(PresentEvent::Unavailable("injected missing compositor globals".into()));
                return pane;
            }
        }
        thread.subscribe(id, Rc::downgrade(&sink));
        if area.is_mapped() {
            pane.map();
        }
        pane
    }

    fn watch_surface(&self) {
        if let Some(surface) = self.io.surface() {
            if self
                .surface_signals
                .borrow()
                .first()
                .is_some_and(|(object, _)| object == surface.upcast_ref::<glib::Object>())
            {
                return;
            }
            self.disconnect_surface();
            let weak = self.weak.clone();
            let id = surface.connect_scale_notify(move |_| {
                if let Some(pane) = weak.upgrade() {
                    pane.geometry_changed();
                }
            });
            self.surface_signals.borrow_mut().push((surface.clone().upcast(), id));
            if let Ok(toplevel) = surface.dynamic_cast::<gdk::Toplevel>() {
                self.set_suspended(toplevel.state().contains(gdk::ToplevelState::SUSPENDED));
                let weak = self.weak.clone();
                let id = toplevel.connect_notify_local(Some("state"), move |toplevel, _| {
                    if let Some(pane) = weak.upgrade() {
                        pane.set_suspended(toplevel.state().contains(gdk::ToplevelState::SUSPENDED));
                    }
                });
                self.surface_signals.borrow_mut().push((toplevel.upcast(), id));
            }
        }
    }

    fn disconnect_surface(&self) {
        for (object, id) in self.surface_signals.borrow_mut().drain(..) {
            object.disconnect(id);
        }
    }

    fn note(&self, event: &str, seq: u64) {
        if let Some(log) = &self.log {
            log.event(event, seq);
        }
    }

    fn hide(&self) {
        self.hide_with(self.hole() == Hole::Open);
    }

    /// Hides the child; `hole_was_open` is the hole's state when the hide was asked for, which a deferred
    /// hide can no longer read because the pacer has closed the hole by then.
    fn hide_with(&self, hole_was_open: bool) {
        self.note("null_attach", 0);
        if hole_was_open {
            self.note("hole_closed", 0);
        }
        self.pending_seq.set(None);
        self.io.hide();
    }

    fn hide_or_defer(&self) {
        if self.drawing.get() {
            let open = self.hole() == Hole::Open;
            self.hide_pending
                .set(Some(open || self.hide_pending.get().unwrap_or(false)));
        } else {
            self.hide();
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.get()
    }

    pub(crate) fn hole(&self) -> Hole {
        self.pacer.borrow().hole()
    }

    pub(crate) fn period(&self) -> Duration {
        self.pacer.borrow().period()
    }

    pub(crate) fn owns_snapshot(&self) -> bool {
        self.is_active() && self.switched.get()
    }

    pub(crate) fn uses_pacing(&self) -> bool {
        self.is_active() && (self.switched.get() || self.drawing.get())
    }

    pub(crate) fn animating(&self, on: bool) {
        self.pacer.borrow_mut().animating(on);
    }

    pub(crate) fn hole_rect(&self) -> Option<graphene::Rect> {
        if self.hole() != Hole::Open {
            return None;
        }
        let size = self.last.get()?.placement.logical;
        Some(graphene::Rect::new(0.0, 0.0, size.0 as f32, size.1 as f32))
    }

    /// What of an allocation of `size` the child's current frame leaves bare while the hole is open; empty otherwise.
    pub(crate) fn outside_frame(&self, size: (f32, f32)) -> Vec<graphene::Rect> {
        self.hole_rect()
            .map(|shown| outside_shown(size, (shown.width(), shown.height())))
            .unwrap_or_default()
    }

    pub(crate) fn map(&self) {
        if !self.is_active() {
            return;
        }
        self.mapped.set(true);
        self.watch_surface();
        self.pacer.borrow_mut().mapped();
        self.geometry_changed();
    }

    pub(crate) fn unmap(&self) {
        self.mapped.set(false);
        if !self.is_active() {
            return;
        }
        self.hide_or_defer();
        let was_open = self.hole() == Hole::Open;
        self.pacer.borrow_mut().unmapped();
        self.cancel_stale();
        self.last.set(None);
        if was_open {
            self.io.hole_changed();
        }
        self.io.queue_transaction();
    }

    pub(crate) fn geometry_changed(&self) {
        if self.is_active() {
            self.transaction.set(true);
            self.io.queue_transaction();
        }
    }

    pub(crate) fn request(&self, _reason: FrameReason) {
        if !self.is_active() {
            self.io.queue_render();
            return;
        }
        if self.drawing.get() {
            self.again.set(true);
            return;
        }
        if self.hole() == Hole::Closed {
            self.pacer.borrow_mut().want();
            self.io.queue_render();
            return;
        }
        let geometry = self.io.geometry();
        self.move_underlay_alone(geometry);
        if geometry != self.last.get() {
            self.geometry_changed();
            return;
        }
        let action = self.pacer.borrow_mut().want();
        self.act(action);
    }

    /// When `geometry` differs from the last one drawn only in the underlay's rectangle (the window's edge moved, the
    /// editor did not), moves the underlay alone and records the geometry as drawn: no transaction, no render. Returns
    /// whether it did.
    fn move_underlay_alone(&self, geometry: Option<Geometry>) -> bool {
        let (Some(now), Some(last)) = (geometry, self.last.get()) else {
            return false;
        };
        if now == last || now.without_underlay() != last.without_underlay() {
            return false;
        }
        self.io.move_to(now);
        self.last.set(Some(now));
        true
    }

    fn act(&self, action: Action) {
        match action {
            Action::Render => match self.io.geometry() {
                Some(geometry)
                    if !self.transaction.get()
                        && (self.move_underlay_alone(Some(geometry)) || Some(geometry) == self.last.get()) =>
                {
                    self.draw(geometry, false);
                }
                // An unplaceable allocation also goes through the layout handler, which closes the hole.
                _ => self.geometry_changed(),
            },
            Action::OpenTransaction => self.geometry_changed(),
            Action::None | Action::NullAttach => {}
        }
    }

    fn layout(&self) {
        if !self.is_active() || !self.mapped.get() || !self.globals.get() {
            return;
        }
        match self.io.setup() {
            Ok(Some(rebuilt)) => {
                if rebuilt && self.hole() == Hole::Open {
                    self.note("hole_closed", 0);
                    self.pending_seq.set(None);
                    self.pacer.borrow_mut().unmapped();
                    self.pacer.borrow_mut().mapped();
                    self.last.set(None);
                    self.io.hole_changed();
                }
            }
            Ok(None) => return,
            Err(reason) => {
                self.fall_back(&reason);
                return;
            }
        }
        let Some(geometry) = self.io.geometry() else {
            self.close_unplaceable();
            return;
        };
        if !self.transaction.get() && self.move_underlay_alone(Some(geometry)) {
            return;
        }
        if self.transaction.replace(false) || self.last.get() != Some(geometry) {
            self.draw(geometry, true);
        }
    }

    /// An allocation too small to place (a collapsed split, a tiny window) leaves the child nothing to show: it must
    /// not keep its old picture under a hole the shell still keeps open. The pane stays mapped, keeps any want, and
    /// reopens through the next transaction with a placeable allocation.
    fn close_unplaceable(&self) {
        self.transaction.set(false);
        if self.hole() != Hole::Open {
            return;
        }
        self.hide();
        {
            let mut pacer = self.pacer.borrow_mut();
            pacer.unmapped();
            pacer.mapped();
        }
        self.cancel_stale();
        self.last.set(None);
        self.io.hole_changed();
    }

    fn draw(&self, geometry: Geometry, transaction: bool) {
        if self.drawing.replace(true) {
            self.again.set(true);
            return;
        }
        if let Some(log) = &self.log {
            log.begin_render();
        }
        if transaction {
            if let Some(log) = &self.log {
                log.transaction(self.io.transform());
            }
        }
        match self.io.render(geometry, transaction) {
            Ok(Some(seq)) if self.is_active() && self.mapped.get() => {
                self.io.count(|c| c.child_commits += 1);
                self.pending_seq.set(Some(seq));
                if let Some(log) = &self.log {
                    log.commit(seq);
                }
                self.last.set(Some(geometry));
                self.blocked.set(false);
                let now = Instant::now();
                if self.hole() == Hole::Closed {
                    self.note("hole_open", seq);
                    self.pacer.borrow_mut().opened(seq, now);
                    self.switched.set(true);
                } else {
                    self.pacer.borrow_mut().committed(seq, now);
                }
                // A moved open hole invalidates the parent's backdrop even though its state stayed open.
                if transaction {
                    self.io.hole_changed();
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                self.blocked.set(true);
                if self.hole() == Hole::Closed {
                    self.io.count(|c| c.open_deferred += 1);
                    self.note("open_deferred", 0);
                    self.pacer.borrow_mut().open_deferred();
                } else if transaction {
                    self.io.count(|c| c.transaction_misses += 1);
                    self.note("transaction_miss", 0);
                    self.io.move_to(geometry);
                    self.io.hole_changed();
                }
            }
            Err(reason) => self.fall_back(&reason),
        }
        self.drawing.set(false);
        if let Some(hole_was_open) = self.hide_pending.take() {
            self.hide_with(hole_was_open);
        }
        if self.again.replace(false) && self.is_active() {
            let action = self.pacer.borrow_mut().want();
            self.act(action);
        }
        self.arm_stale();
    }

    fn buffer_changed(&self, change: PoolChange) {
        if !self.is_active() {
            return;
        }
        match change {
            PoolChange::Failed => self.fall_back("the compositor refused a presentation buffer"),
            PoolChange::Created | PoolChange::Freed => {
                let opening = self.pacer.borrow_mut().buffer_ready();
                if opening == Action::OpenTransaction {
                    self.act(opening);
                } else if self.blocked.replace(false) {
                    // A skipped render consumed the pacer's want; the release must restore it.
                    if self.hole() == Hole::Closed || self.io.geometry() != self.last.get() {
                        self.geometry_changed();
                    } else {
                        let action = self.pacer.borrow_mut().want();
                        self.act(action);
                    }
                }
            }
            PoolChange::None => {}
        }
    }

    pub(crate) fn fall_back(&self, reason: &str) {
        if !self.active.replace(false) {
            return;
        }
        self.io.count(|c| c.fallbacks += 1);
        self.note("fallback", 0);
        if let Some(log) = &self.log {
            log.missing();
        }
        eprintln!("[editor] subsurface: {reason}; falls back to the GTK path");
        self.hide_or_defer();
        self.pacer.borrow_mut().unmapped();
        self.cancel_stale();
        self.io.unsubscribe();
        self.destroy_pending.set(true);
        self.io.hole_changed();
        self.io.queue_render();
        self.io.queue_transaction();
    }

    fn after_paint(&self) {
        self.io.after_paint();
        if self.destroy_pending.replace(false) {
            self.io.destroy();
        }
    }

    pub(crate) fn stop(&self) {
        self.active.set(false);
        self.hide();
        self.pacer.borrow_mut().unmapped();
        self.cancel_stale();
        self.io.unsubscribe();
        self.io.hole_changed();
        self.io.destroy();
        self.destroy_pending.set(false);
        self.disconnect_surface();
        for (object, id) in self.signals.borrow_mut().drain(..) {
            object.disconnect(id);
        }
    }

    fn set_suspended(&self, suspended: bool) {
        self.pacer.borrow_mut().set_suspended(suspended);
        let action = self.stale_check(Instant::now());
        self.act(action);
        self.arm_stale();
    }

    fn stale_check(&self, now: Instant) -> Action {
        if self.pacer.borrow().stale_check_due().is_some_and(|due| now >= due) {
            self.io.count(|c| c.stale_callbacks += 1);
            self.note("stale_callback", self.pending_seq.take().unwrap_or(0));
        }
        self.pacer.borrow_mut().stale_check(now)
    }

    fn cancel_stale(&self) {
        if let Some(source) = self.stale.borrow_mut().take() {
            source.remove();
        }
    }

    fn arm_stale(&self) {
        self.cancel_stale();
        if !self.is_active() {
            return;
        }
        let due = {
            let pacer = self.pacer.borrow();
            if !pacer.callback_pending() {
                return;
            }
            pacer.stale_check_due()
        };
        let Some(due) = due else { return };
        let weak = self.weak.clone();
        *self.stale.borrow_mut() = Some(glib::timeout_add_local_once(
            due.saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1)),
            move || {
                if let Some(pane) = weak.upgrade() {
                    pane.stale.borrow_mut().take();
                    let now = Instant::now();
                    let action = pane.stale_check(now);
                    pane.act(action);
                    pane.arm_stale();
                }
            },
        ));
    }
}

impl PresentSink for SubsurfacePane {
    fn globals_ready(&self) {
        if self.is_active() {
            self.globals.set(true);
            self.geometry_changed();
        }
    }

    fn event(&self, event: PresentEvent) {
        let change = self.io.pool_event(&event);
        self.buffer_changed(change);
        if !self.is_active() {
            return;
        }
        match event {
            PresentEvent::Unavailable(reason) | PresentEvent::ThreadFailed(reason) => self.fall_back(&reason),
            PresentEvent::FrameDone { seq, at, .. } => {
                self.io.count(|c| c.child_frames += 1);
                self.note("frame", seq);
                if self.pending_seq.get() == Some(seq) {
                    self.pending_seq.set(None);
                } else {
                    self.io.count(|c| c.stale_callbacks += 1);
                    self.note("stale_callback", seq);
                }
                let action = self.pacer.borrow_mut().frame_done(seq, at);
                self.act(action);
                self.arm_stale();
            }
            PresentEvent::Presented {
                seq,
                presented_ns,
                refresh_ns,
                ..
            } => {
                if let Some(log) = &self.log {
                    log.presented(seq, presented_ns, refresh_ns);
                }
                self.pacer.borrow_mut().presented(refresh_ns);
            }
            PresentEvent::Discarded { seq, .. } => self.note("discarded", seq),
            PresentEvent::FeedbackChanged(_) => {
                // Existing buffers may no longer belong to the advertised device or modifier set.
                self.fall_back("the compositor's dmabuf feedback changed");
            }
            _ => {}
        }
    }
}

impl Drop for SubsurfacePane {
    fn drop(&mut self) {
        self.cancel_stale();
        self.io.unsubscribe();
        self.io.destroy();
        self.disconnect_surface();
        for (object, id) in self.signals.get_mut().drain(..) {
            object.disconnect(id);
        }
    }
}

struct Resources {
    child: Child<WaylandRequests<ThreadState>>,
    pool: WlBufferPool,
    viewporter: bool,
}

struct WaylandPane {
    area: glib::WeakRef<EditorGlArea>,
    thread: Rc<PresenterThread>,
    id: PaneId,
    render: RenderFn,
    resources: RefCell<Option<Resources>>,
    last_seq: Cell<u64>,
}

impl WaylandPane {
    /// After the underlay was sent a change: it stays in sync mode, so the change waits for a parent commit, and a
    /// parent frame in which nothing GTK draws changed is committed only when GDK is told something is pending
    /// (a colour change, or a window edge that moved while the editor did not, comes with no GTK change of its own).
    /// A lost underlay is said once.
    fn underlay_changed(&self, area: &EditorGlArea, child: &mut Child<WaylandRequests<ThreadState>>) {
        if child.take_underlay_sent() {
            if let Some(surface) = area.native().and_then(|native| native.surface()) {
                foreign::force_next_commit(&surface);
            }
            area.queue_draw();
        }
        if child.take_underlay_lost() {
            eprintln!(
                "[editor] subsurface: no buffer could be made for the editor's underlay; a compositor that rounds \
                 the editor's corners shows the desktop there"
            );
        }
    }

    fn current(&self) -> Result<EditorGlArea, String> {
        let area = self.area.upgrade().ok_or("the editor area is gone")?;
        area.make_current();
        if let Some(error) = area.error() {
            return Err(format!("the editor's GL context failed: {error}"));
        }
        if area
            .context()
            .is_none_or(|context| gdk::GLContext::current().as_ref() != Some(&context))
        {
            return Err("the editor's GL context is not current".into());
        }
        Ok(area)
    }

    fn flush(&self) {
        // GDK owns the connection. Only borrow a clone during this request, never across display close.
        if let Some(area) = self.area.upgrade() {
            if let Some(conn) = connection_for(&area.display()) {
                if conn.flush().is_err() {
                    self.thread.request_flush();
                }
            }
        }
    }
}

impl PaneIo for WaylandPane {
    fn log(&self) -> Option<Rc<PaneLog>> {
        self.area.upgrade()?.present_log()
    }
    fn count(&self, update: fn(&mut PresentationCounts)) {
        if let Some(area) = self.area.upgrade() {
            area.count_presentation(update);
        }
    }
    fn transform(&self) -> (f64, f64) {
        self.area
            .upgrade()
            .and_then(|area| area.native())
            .map(|native| native.surface_transform())
            .unwrap_or_default()
    }
    fn surface(&self) -> Option<gdk::Surface> {
        self.area.upgrade()?.native()?.surface()
    }
    fn setup(&self) -> Result<Option<bool>, String> {
        #[cfg(feature = "fault-injection")]
        if log::fault_requested(log::Fault::Dmabuf) {
            return Err("injected missing dmabuf support".into());
        }
        let Some(globals) = self.thread.globals() else {
            return Ok(None);
        };
        let area = self.current()?;
        let Some(surface) = area.native().and_then(|n| n.surface()) else {
            return Ok(None);
        };
        let Some(conn) = connection_for(&area.display()) else {
            return Err("the Wayland display is closed".into());
        };
        let Some(parent) = foreign::toplevel_surface(&conn, &surface) else {
            return Ok(None);
        };
        if let Some(resources) = self.resources.borrow().as_ref() {
            if resources.child.parent_is(&parent) {
                return if resources.child.is_ready() {
                    Ok(Some(false))
                } else {
                    Err("the child surface is no longer alive".into())
                };
            }
        }
        self.destroy();
        let modifiers = globals.feedback.formats.iter().map(|&(_, modifier)| modifier).collect();
        let pool = DmabufPool::new(&area.display(), ModifierSource::Compositor(modifiers))?;
        if let Err(reason) = pool.check_device(globals.feedback.main_device) {
            pool.release(true);
            return Err(reason);
        }
        let Some(chosen) = buffers::choose_format(&globals.feedback, pool.egl_modifiers(), true) else {
            pool.release(true);
            return Err("EGL and the compositor have no common tiled format and modifier".into());
        };
        let viewporter = globals.viewporter.is_some();
        let mut child = Child::new(
            WaylandRequests::new(&globals, self.thread.queue_handle()),
            &parent,
            &chosen,
            viewporter,
        );
        if !child.is_ready() {
            child.destroy();
            pool.release(true);
            return Err("the child surface could not be created".into());
        }
        if viewporter && !child.has_corners() {
            eprintln!(
                "[editor] subsurface: no corner patches under the editor; a compositor that rounds the editor's \
                 corners shows the underlay's colour there"
            );
        }
        if viewporter && !child.has_corners() && !child.has_underlay() {
            eprintln!(
                "[editor] subsurface: no underlay under the editor (no single-pixel buffers or wl_shm); a compositor \
                 that rounds the editor's corners shows the desktop there"
            );
        }
        child.continue_after(self.last_seq.get());
        *self.resources.borrow_mut() = Some(Resources {
            child,
            pool: WlBufferPool::new(pool, chosen, self.id, viewporter),
            viewporter,
        });
        self.flush();
        Ok(Some(true))
    }

    fn geometry(&self) -> Option<Geometry> {
        let area = self.area.upgrade()?;
        let native = area.native()?;
        let surface = native.surface()?;
        let widget = native.clone().dynamic_cast::<gtk4::Widget>().ok()?;
        let point = area.compute_point(&widget, &graphene::Point::zero())?;
        let resources = self.resources.borrow();
        let viewporter = resources.as_ref()?.viewporter;
        let scale = surface.scale();
        Some(Geometry {
            placement: placement(
                (f64::from(point.x()), f64::from(point.y())),
                native.surface_transform(),
                (area.width(), area.height()),
                scale,
                area.scale_factor(),
                viewporter,
            )?
            .with_underlay(native.surface_transform(), (widget.width(), widget.height())),
            scale,
        })
    }

    fn render(&self, geometry: Geometry, transaction: bool) -> Result<Option<u64>, String> {
        #[cfg(feature = "fault-injection")]
        if log::fault_requested(log::Fault::Alloc) {
            return Err("injected buffer allocation failure".into());
        }
        let area = self.current()?;
        let globals = self.thread.globals().ok_or("the compositor globals are gone")?;
        let mut resources = self.resources.borrow_mut();
        let resources = resources.as_mut().ok_or("the child surface is gone")?;
        if transaction {
            resources.child.begin_transaction();
        }
        let slot = resources
            .pool
            .acquire(&globals, &self.thread.queue_handle(), geometry.placement.content);
        if resources.pool.take_event().is_some() {
            return Err(resources
                .pool
                .failure_reason()
                .unwrap_or("a presentation buffer failed")
                .to_owned());
        }
        let Acquire::Ready(slot) = slot else {
            self.flush();
            return Ok(None);
        };
        debug_assert!(resources.pool.created_count() > 0);
        let size = resources.pool.buffer_size(slot);
        let result = (self.render)(
            area.upcast_ref(),
            subsurface_target(&geometry.placement, size, slot.framebuffer, geometry.scale),
        );
        if result != glib::Propagation::Stop {
            return Err("Skia could not render the presentation buffer".into());
        }
        self.current()?;
        let buffer = resources
            .pool
            .wl_buffer(slot)
            .cloned()
            .ok_or("the presentation buffer is gone")?;
        // The corner patches show the same frame through views of its dmabuf, a `wl_buffer` of their own each.
        let views = resources
            .child
            .has_corners()
            .then(|| resources.pool.corner_views(&globals, &self.thread.queue_handle(), slot))
            .flatten();
        resources.pool.submit(slot);
        if let Some(rgb) = area.cover_rgb() {
            resources.child.set_underlay_color(rgb);
        }
        let seq = resources
            .child
            .present(&buffer, views.as_ref(), size, &geometry.placement, self.id);
        if seq == 0 {
            return Err("the child refused the presentation buffer or geometry".into());
        }
        resources
            .pool
            .corner_views_committed(slot, resources.child.corner_views_committed());
        self.underlay_changed(&area, &mut resources.child);
        debug_assert!(resources.child.has_buffer());
        self.last_seq.set(seq);
        drop(globals);
        self.flush();
        Ok(Some(seq))
    }

    fn move_to(&self, geometry: Geometry) {
        let Some(area) = self.area.upgrade() else { return };
        if let Some(resources) = self.resources.borrow_mut().as_mut() {
            if let Some(rgb) = area.cover_rgb() {
                resources.child.set_underlay_color(rgb);
            }
            resources.child.move_to(&geometry.placement);
            self.underlay_changed(&area, &mut resources.child);
            // A corner that left the window's corner while the editor stayed shows its patch from the next frame,
            // asked for once this move is done.
            if resources.child.take_corner_wants_frame() {
                let area = area.downgrade();
                glib::idle_add_local_once(move || {
                    if let Some(pane) = area.upgrade().and_then(|area| area.subsurface()) {
                        pane.request(FrameReason::Map);
                    }
                });
            }
        }
    }

    fn queue_transaction(&self) {
        if let Some(area) = self.area.upgrade() {
            area.queue_draw();
            if let Some(clock) = area.frame_clock() {
                clock.request_phase(gdk::FrameClockPhase::LAYOUT);
            }
        }
    }

    fn queue_render(&self) {
        if let Some(area) = self.area.upgrade() {
            area.queue_render();
        }
    }

    fn hole_changed(&self) {
        if let Some(area) = self.area.upgrade() {
            area.hole_changed();
        }
    }

    fn hide(&self) {
        if let Some(resources) = self.resources.borrow_mut().as_mut() {
            resources.child.hide();
            // The underlay's null attach rides on the hide's own parent commit.
            resources.child.take_underlay_sent();
        }
        self.flush();
    }

    fn after_paint(&self) {
        if let Some(resources) = self.resources.borrow_mut().as_mut() {
            // Ends a transaction if one is open, and lets go of underlay buffers the parent's commit replaced.
            resources.child.after_paint();
        }
        self.flush();
    }

    fn destroy(&self) {
        if let Some(resources) = self.resources.borrow_mut().take() {
            let current = self.current().is_ok();
            resources.child.destroy();
            resources.pool.release(current);
            self.flush();
        }
    }

    fn unsubscribe(&self) {
        self.thread.unsubscribe(self.id);
    }

    fn pool_event(&self, event: &PresentEvent) -> PoolChange {
        #[cfg(feature = "fault-injection")]
        if log::fault_requested(log::Fault::BufferFailed) {
            if let PresentEvent::BufferCreated { key, buffer } = event {
                if buffer.is_alive() {
                    buffer.destroy();
                    self.flush();
                }
                return self
                    .resources
                    .borrow_mut()
                    .as_mut()
                    .map(|resources| resources.pool.on_event(&PresentEvent::BufferFailed { key: *key }))
                    .unwrap_or(PoolChange::None);
            }
        }
        if let Some(resources) = self.resources.borrow_mut().as_mut() {
            resources.pool.on_event(event)
        } else {
            if let PresentEvent::BufferCreated { buffer, .. } = event {
                if buffer.is_alive() {
                    buffer.destroy();
                    self.flush();
                }
            }
            PoolChange::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Recording {
        calls: RefCell<Vec<&'static str>>,
        geometry: Cell<Geometry>,
        placeable: Cell<bool>,
        ready: Cell<bool>,
        rebuilt: Cell<bool>,
        seq: Cell<u64>,
        transaction: Cell<bool>,
        buffers: Cell<bool>,
        counts: Cell<PresentationCounts>,
    }

    impl Recording {
        fn record(&self, call: &'static str) {
            self.calls.borrow_mut().push(call);
        }

        fn take(&self) -> Vec<&'static str> {
            self.calls.take()
        }
    }

    impl PaneIo for Rc<Recording> {
        fn count(&self, update: fn(&mut PresentationCounts)) {
            let mut counts = self.counts.get();
            update(&mut counts);
            self.counts.set(counts);
        }
        fn setup(&self) -> Result<Option<bool>, String> {
            Ok(Some(self.rebuilt.replace(false)))
        }
        fn geometry(&self) -> Option<Geometry> {
            self.placeable.get().then(|| self.geometry.get())
        }
        fn render(&self, _: Geometry, transaction: bool) -> Result<Option<u64>, String> {
            if !self.ready.get() {
                return Ok(None);
            }
            if transaction {
                self.record("set_sync");
                self.transaction.set(true);
            }
            self.record("render");
            self.record("attach");
            self.record("commit");
            self.seq.set(self.seq.get() + 1);
            self.buffers.set(true);
            Ok(Some(self.seq.get()))
        }
        fn move_to(&self, _: Geometry) {
            self.record("move");
        }
        fn queue_transaction(&self) {
            self.record("queue_draw");
        }
        fn queue_render(&self) {
            self.record("queue_render");
        }
        fn hole_changed(&self) {
            self.record("hole_changed");
        }
        fn hide(&self) {
            self.record("hide");
            self.transaction.set(true);
        }
        fn after_paint(&self) {
            if self.transaction.replace(false) {
                self.record("set_desync");
            }
        }
        fn destroy(&self) {
            if self.buffers.replace(false) {
                for object in [
                    "destroy_subsurface",
                    "destroy_viewport",
                    "destroy_surface",
                    "destroy_buffers",
                ] {
                    self.record(object);
                }
            }
        }
        fn unsubscribe(&self) {
            self.record("unsubscribe");
        }
        fn pool_event(&self, event: &PresentEvent) -> PoolChange {
            match event {
                PresentEvent::BufferFailed { .. } => PoolChange::Failed,
                PresentEvent::BufferReleased { .. } => PoolChange::Freed,
                _ => PoolChange::None,
            }
        }
    }

    fn with_pane(test: impl FnOnce(Rc<SubsurfacePane>, Rc<Recording>)) {
        let _lock = crate::DEFAULT_CONTEXT_TEST_LOCK.lock().unwrap();
        let context = glib::MainContext::default();
        let _guard = context.acquire().unwrap();
        let io = Rc::new(Recording {
            calls: RefCell::new(Vec::new()),
            geometry: Cell::new(Geometry {
                placement: placement((10.0, 20.0), (0.0, 0.0), (801, 601), 1.5, 2, true).unwrap(),
                scale: 1.5,
            }),
            placeable: Cell::new(true),
            ready: Cell::new(false),
            rebuilt: Cell::new(false),
            seq: Cell::new(0),
            transaction: Cell::new(false),
            buffers: Cell::new(false),
            counts: Cell::new(PresentationCounts::default()),
        });
        test(SubsurfacePane::with_io(Box::new(io.clone())), io);
    }

    fn open(pane: &SubsurfacePane, io: &Recording) {
        pane.map();
        pane.globals_ready();
        io.ready.set(true);
        pane.layout();
        assert_eq!(pane.hole(), Hole::Open);
        io.take();
        pane.after_paint();
        io.take();
    }

    #[test]
    fn thread_failed_moves_the_pane_to_the_gtk_path() {
        with_pane(|pane, io| {
            open(&pane, &io);
            pane.event(PresentEvent::ThreadFailed("reader failed".into()));
            assert!(!pane.is_active());
            assert!(pane.hole_rect().is_none());
            assert_eq!(
                io.take(),
                ["hide", "unsubscribe", "hole_changed", "queue_render", "queue_draw"]
            );
            pane.after_paint();
            assert_eq!(
                io.take(),
                [
                    "set_desync",
                    "destroy_subsurface",
                    "destroy_viewport",
                    "destroy_surface",
                    "destroy_buffers"
                ]
            );
            pane.request(FrameReason::Batch);
            assert_eq!(io.take(), ["queue_render"]);
        });
    }

    #[test]
    fn a_switch_to_the_subsurface_opens_the_hole_in_one_transaction() {
        with_pane(|pane, io| {
            pane.map();
            pane.globals_ready();
            pane.layout();
            assert_eq!(pane.hole(), Hole::Closed);
            io.ready.set(true);
            pane.buffer_changed(PoolChange::Created);
            assert_eq!(pane.hole(), Hole::Closed);
            io.take();
            pane.layout();
            assert_eq!(io.take(), ["set_sync", "render", "attach", "commit", "hole_changed"]);
            assert_eq!(pane.hole(), Hole::Open);
            assert!(pane.pacer.borrow().callback_pending());
            pane.after_paint();
            assert_eq!(io.take(), ["set_desync"]);
        });
    }

    #[test]
    fn an_opening_waits_for_a_created_buffer_not_for_a_frame_done() {
        with_pane(|pane, io| {
            pane.map();
            pane.globals_ready();
            pane.layout();
            pane.request(FrameReason::Batch);
            assert!(!pane.owns_snapshot());
            assert_eq!(pane.hole(), Hole::Closed);
            io.take();
            io.ready.set(true);
            pane.buffer_changed(PoolChange::Created);
            assert_eq!(io.take(), ["queue_draw"]);
            pane.layout();
            assert_eq!(pane.hole(), Hole::Open);
            assert!(pane.owns_snapshot());
        });
    }

    #[test]
    fn a_buffer_failed_event_falls_back_once() {
        with_pane(|pane, io| {
            open(&pane, &io);
            let key = BufferKey {
                pane: PaneId(1),
                generation: 1,
                id: 1,
                view: 0,
            };
            pane.event(PresentEvent::BufferFailed { key });
            pane.event(PresentEvent::BufferFailed { key });
            pane.event(PresentEvent::ThreadFailed("reader failed".into()));
            pane.after_paint();
            let calls = io.take();
            assert_eq!(calls.iter().filter(|&&call| call == "hide").count(), 1);
            assert_eq!(calls.iter().filter(|&&call| call == "destroy_buffers").count(), 1);
            assert!(!pane.is_active());
            assert_eq!(io.counts.get().fallbacks, 1);
        });
    }

    #[test]
    fn a_hide_deferred_through_a_draw_remembers_the_open_hole() {
        with_pane(|pane, io| {
            open(&pane, &io);
            pane.drawing.set(true);
            pane.unmap();
            assert_eq!(pane.hole(), Hole::Closed);
            assert_eq!(pane.hide_pending.get(), Some(true));
            pane.fall_back("test");
            assert_eq!(pane.hide_pending.get(), Some(true));
            assert!(!io.take().contains(&"hide"));
        });
    }

    #[test]
    fn a_late_callback_is_counted_without_consuming_the_current_callback() {
        with_pane(|pane, io| {
            open(&pane, &io);
            assert_eq!(io.counts.get().child_commits, 1);
            pane.event(PresentEvent::FrameDone {
                pane: PaneId(1),
                seq: 0,
                at: Instant::now(),
            });
            assert_eq!(io.counts.get().stale_callbacks, 1);
            assert!(pane.pacer.borrow().callback_pending());
            pane.event(PresentEvent::FrameDone {
                pane: PaneId(1),
                seq: 1,
                at: Instant::now(),
            });
            assert_eq!(io.counts.get().child_frames, 2);
            assert_eq!(io.counts.get().stale_callbacks, 1);
            assert!(!pane.pacer.borrow().callback_pending());
        });
    }

    #[test]
    fn a_transaction_without_a_buffer_counts_a_miss_without_a_commit() {
        with_pane(|pane, io| {
            pane.map();
            pane.globals_ready();
            pane.layout();
            assert_eq!(io.counts.get().open_deferred, 1);
            assert_eq!(io.counts.get().child_commits, 0);
            io.ready.set(true);
            pane.buffer_changed(PoolChange::Created);
            pane.layout();
            io.ready.set(false);
            pane.geometry_changed();
            pane.layout();
            assert_eq!(io.counts.get().transaction_misses, 1);
            assert_eq!(io.counts.get().child_commits, 1);
        });
    }

    fn rect(r: &graphene::Rect) -> (f32, f32, f32, f32) {
        (r.x(), r.y(), r.width(), r.height())
    }

    #[test]
    fn the_parent_covers_only_what_the_childs_frame_does_not_reach() {
        assert!(outside_shown((801.0, 601.0), (801.0, 601.0)).is_empty());
        assert!(outside_shown((700.0, 500.0), (801.0, 601.0)).is_empty());
        let grown: Vec<_> = outside_shown((1001.0, 701.0), (801.0, 601.0))
            .iter()
            .map(rect)
            .collect();
        assert_eq!(grown, [(801.0, 0.0, 200.0, 701.0), (0.0, 601.0, 801.0, 100.0)]);
        let wider_but_shorter: Vec<_> = outside_shown((1001.0, 500.0), (801.0, 601.0))
            .iter()
            .map(rect)
            .collect();
        assert_eq!(wider_but_shorter, [(801.0, 0.0, 200.0, 500.0)]);
        let taller_but_narrower: Vec<_> = outside_shown((700.0, 701.0), (801.0, 601.0)).iter().map(rect).collect();
        assert_eq!(taller_but_narrower, [(0.0, 601.0, 700.0, 100.0)]);
    }

    #[test]
    fn an_allocation_that_outgrows_the_buffers_keeps_the_new_strip_covered_until_a_buffer_arrives() {
        with_pane(|pane, io| {
            open(&pane, &io);
            assert!(pane.outside_frame((801.0, 601.0)).is_empty());
            let grown = Geometry {
                placement: placement((10.0, 20.0), (0.0, 0.0), (1001, 601), 1.5, 2, true).unwrap(),
                scale: 1.5,
            };
            io.geometry.set(grown);
            io.ready.set(false);
            pane.geometry_changed();
            pane.layout();
            assert_eq!(io.counts.get().transaction_misses, 1);
            assert_eq!(pane.hole(), Hole::Open);
            let bare: Vec<_> = pane.outside_frame((1001.0, 601.0)).iter().map(rect).collect();
            assert_eq!(bare, [(801.0, 0.0, 200.0, 601.0)]);
            pane.after_paint();
            io.ready.set(true);
            pane.buffer_changed(PoolChange::Created);
            pane.layout();
            assert!(pane.outside_frame((1001.0, 601.0)).is_empty());
            assert!(!pane.outside_frame((1101.0, 601.0)).is_empty());
        });
    }

    #[test]
    fn moving_an_open_hole_notifies_its_callbacks_in_the_geometry_transaction() {
        with_pane(|pane, io| {
            open(&pane, &io);
            let mut geometry = io.geometry.get();
            geometry.placement.position.0 += 31;
            io.geometry.set(geometry);
            pane.layout();
            assert_eq!(io.take(), ["set_sync", "render", "attach", "commit", "hole_changed"]);
            assert_eq!(pane.hole(), Hole::Open);
            pane.after_paint();
            assert_eq!(io.take(), ["set_desync"]);
        });
    }

    #[test]
    fn a_window_edge_that_moves_alone_moves_the_underlay_without_a_transaction_or_a_render() {
        with_pane(|pane, io| {
            let mut geometry = io.geometry.get();
            geometry.placement = geometry.placement.with_underlay((0.0, 0.0), (1600, 1000));
            io.geometry.set(geometry);
            open(&pane, &io);
            // The window's right edge comes to within 10 px of the editor: only the underlay's rectangle changes.
            let mut narrower = geometry;
            narrower.placement = geometry
                .placement
                .without_underlay()
                .with_underlay((0.0, 0.0), (10 + 801 + 10, 1000));
            assert_ne!(narrower, geometry);
            io.geometry.set(narrower);
            pane.layout();
            assert_eq!(io.take(), ["move"]);
            pane.after_paint();
            assert_eq!(io.take(), Vec::<&str>::new(), "no transaction to end");
            // The same through a frame request: the underlay moves, nothing opens a transaction.
            let mut narrower_still = narrower;
            narrower_still.placement = geometry
                .placement
                .without_underlay()
                .with_underlay((0.0, 0.0), (10 + 801 + 4, 1000));
            io.geometry.set(narrower_still);
            pane.request(FrameReason::Batch);
            let calls = io.take();
            assert_eq!(calls.first(), Some(&"move"), "{calls:?}");
            assert!(
                !calls.contains(&"set_sync") && !calls.contains(&"queue_draw"),
                "{calls:?}"
            );
            // A later layout with nothing changed sends nothing.
            pane.layout();
            assert!(!io.take().contains(&"set_sync"));
            // The editor itself moving is still a transaction.
            let mut moved = narrower_still;
            moved.placement.position.0 += 3;
            io.geometry.set(moved);
            pane.layout();
            assert_eq!(io.take(), ["set_sync", "render", "attach", "commit", "hole_changed"]);
        });
    }

    #[test]
    fn an_unplaceable_allocation_closes_the_open_hole_until_it_can_be_placed_again() {
        with_pane(|pane, io| {
            open(&pane, &io);
            pane.request(FrameReason::Batch);
            assert!(io.take().is_empty(), "a want waits for the pending callback");
            io.placeable.set(false);
            pane.event(PresentEvent::FrameDone {
                pane: PaneId(1),
                seq: 1,
                at: Instant::now(),
            });
            assert_eq!(
                io.take(),
                ["queue_draw"],
                "the released want goes to the layout handler"
            );
            pane.layout();
            assert_eq!(io.take(), ["hide", "hole_changed"]);
            assert_eq!(pane.hole(), Hole::Closed);
            assert!(pane.hole_rect().is_none());
            assert_eq!(snapshot_plan(Some(&pane)), SnapshotPlan::Cover);
            pane.after_paint();
            assert_eq!(
                io.take(),
                ["set_desync"],
                "the null attach lands with the parent's commit"
            );
            pane.request(FrameReason::Batch);
            pane.layout();
            assert_eq!(
                io.take(),
                ["queue_render"],
                "nothing reaches the child while it cannot be placed"
            );
            io.placeable.set(true);
            pane.geometry_changed();
            pane.layout();
            assert_eq!(
                io.take(),
                ["queue_draw", "set_sync", "render", "attach", "commit", "hole_changed"]
            );
            assert_eq!(pane.hole(), Hole::Open);
            assert!(pane.is_active());
        });
    }

    #[test]
    fn mapping_renders_the_kept_want_without_a_frame_done() {
        with_pane(|pane, io| {
            open(&pane, &io);
            pane.unmap();
            assert!(paints_cover(pane.hole()));
            pane.request(FrameReason::Batch);
            pane.map();
            io.take();
            pane.layout();
            assert_eq!(io.take(), ["set_sync", "render", "attach", "commit", "hole_changed"]);
            assert_eq!(pane.hole(), Hole::Open);
        });
    }

    #[test]
    fn a_release_restores_the_want_consumed_by_a_full_pool() {
        with_pane(|pane, io| {
            open(&pane, &io);
            pane.event(PresentEvent::FrameDone {
                pane: PaneId(1),
                seq: 1,
                at: Instant::now(),
            });
            io.ready.set(false);
            pane.request(FrameReason::Batch);
            assert!(pane.blocked.get());
            io.ready.set(true);
            io.take();
            pane.buffer_changed(PoolChange::Freed);
            assert_eq!(io.take(), ["render", "attach", "commit"]);
        });
    }

    #[test]
    fn replacing_the_parent_covers_the_editor_until_the_new_buffer_is_ready() {
        with_pane(|pane, io| {
            open(&pane, &io);
            io.rebuilt.set(true);
            io.ready.set(false);
            pane.layout();
            assert_eq!(pane.hole(), Hole::Closed);
            assert!(pane.owns_snapshot());
            assert!(io.take().contains(&"hole_changed"));
            io.ready.set(true);
            pane.buffer_changed(PoolChange::Created);
            pane.layout();
            assert_eq!(pane.hole(), Hole::Open);
        });
    }

    #[test]
    fn the_subsurface_target_carries_the_fractional_scale() {
        let p = geometry::placement((0.0, 0.0), (0.0, 0.0), (801, 601), 1.5, 2, true).unwrap();
        let crate::RenderTarget::Subsurface {
            buffer,
            content,
            os_scale,
            framebuffer,
        } = subsurface_target(&p, (1280, 1024), 7, 1.5)
        else {
            panic!("expected a subsurface target")
        };
        assert_eq!(os_scale, 1.5);
        assert_eq!(content, (1202, 902));
        assert_eq!(buffer, (1280, 1024));
        assert_eq!(framebuffer, 7);
    }

    #[test]
    fn the_snapshot_paints_the_cover_until_the_hole_opens() {
        assert_eq!(snapshot_plan(None), SnapshotPlan::Gtk);
        with_pane(|pane, io| {
            pane.map();
            pane.globals_ready();
            pane.layout();
            assert_eq!(
                snapshot_plan(Some(&pane)),
                SnapshotPlan::Gtk,
                "GTK draws until the first opening"
            );
            io.ready.set(true);
            pane.buffer_changed(PoolChange::Created);
            pane.layout();
            assert_eq!(snapshot_plan(Some(&pane)), SnapshotPlan::Nothing);
            pane.unmap();
            assert_eq!(snapshot_plan(Some(&pane)), SnapshotPlan::Cover);
            pane.map();
            pane.layout();
            assert_eq!(snapshot_plan(Some(&pane)), SnapshotPlan::Nothing);
            pane.fall_back("test");
            assert_eq!(
                snapshot_plan(Some(&pane)),
                SnapshotPlan::Gtk,
                "a fallback returns the frame to GTK"
            );
        });
    }

    #[test]
    fn leaving_suspension_recovers_an_overdue_want_at_once() {
        with_pane(|pane, io| {
            open(&pane, &io);
            pane.set_suspended(true);
            pane.pacer
                .borrow_mut()
                .committed(1, Instant::now() - Duration::from_secs(2));
            pane.request(FrameReason::Batch);
            assert!(pane.stale.borrow().is_none());
            assert!(io.take().is_empty());
            pane.set_suspended(false);
            assert_eq!(io.take(), ["render", "attach", "commit"]);
            assert_eq!(io.counts.get().stale_callbacks, 1);
            assert!(pane.stale.borrow().is_some());
        });
    }

    #[test]
    fn an_idle_frame_done_does_not_render_or_rearm_the_timeout() {
        with_pane(|pane, io| {
            open(&pane, &io);
            pane.event(PresentEvent::FrameDone {
                pane: PaneId(1),
                seq: 1,
                at: Instant::now(),
            });
            assert!(io.take().is_empty());
            assert!(pane.stale.borrow().is_none());
        });
    }
}
