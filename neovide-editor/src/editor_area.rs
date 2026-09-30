//! `EditorGlArea`: the pane's `GtkGLArea`, with its snapshot presenting through the editor's own
//! tiled dmabuf buffers (`dmabuf_target::Presenter`) instead of the linear colour texture
//! `GtkGLArea` exports for itself. See `dmabuf_target`'s module doc for the measurement behind it.
//!
//! It follows `gtk_gl_area_snapshot` (GTK 4.14.5 and 4.22.5 read side by side), with a different
//! colour buffer: the same pixel size (widget size times GTK's integer scale factor), `render`
//! emitted with the area's own context and this area's framebuffer bound, and the result appended
//! upside down, as GL rendered it, then flipped. The pane's `connect_render`/`connect_resize`
//! handlers run unchanged; the render handler asks [`EditorGlArea::draws_into_own_buffer`] whether
//! it can skip its Skia intermediate. Whenever the presenter is off, cannot be set up, or fails,
//! this chains up to `GtkGLArea`'s own snapshot, which is the previous behaviour in full.
//!
//! **`resize` is not emitted the way `GtkGLArea` emits it.** `GtkGLArea` emits it on the first
//! frame after every size allocation and every scale-factor change, even when the pixel size stayed
//! the same. This emits it before the first frame, whenever the device-pixel size changes, and on
//! the first own frame after a fallback frame -- so a scale change that leaves the device size
//! unchanged gets no `resize` here. That is harmless for this pane: its render arm applies the
//! scale itself (`sync_os_scale`), and its service re-syncs the grid from the framebuffer size
//! whenever it runs (the render arm asks for a run after a scale change). A host relying on
//! `resize` to learn of a scale-only change would miss it.
//!
//! One more `GtkGLArea` behaviour is not reproduced: with `auto-render` off, `GtkGLArea` re-presents
//! its last texture on a snapshot nobody asked `queue_render` for. This always renders, which is
//! `auto-render` on -- the only way the pane builds it.
//!
//! **When it falls back, and for how long** ([`Lifecycle`], per GL context, i.e. per realize):
//! off by `EITRI_EDITOR_DMABUF` or below GTK 4.16 (`dmabuf_target::decide`), or when the presenter
//! cannot be set up (no EGL, GBM, or modifier both sides take -- nothing a new size changes): until
//! the next realize. A frame that fails mid-run (an allocation, an import, an incomplete
//! framebuffer, a texture GTK refuses): that frame is redrawn through `GtkGLArea`, which keeps
//! drawing until the device size changes; then the path is set up again, at most
//! [`RETRIES_PER_CONTEXT`] times per GL context. The editor is effectively never unrealized in the
//! product (hidden modules are `set_child_visible(false)`), so without the retry one transient
//! failure would cost the saving for the whole session. Every give-up is logged.

use std::cell::{Cell, RefCell};

use gtk4::glib;
use gtk4::graphene;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;

use crate::dmabuf_target::{self, Presenter};

/// How many times a mid-run failure is retried, at a new size, per GL context.
const RETRIES_PER_CONTEXT: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Not decided for the current GL context (a new one comes with every realize).
    Untried,
    Active,
    /// `GtkGLArea` draws until the next realize: turned off, or the presenter could not be set up.
    Off,
    /// A frame failed at this device size: `GtkGLArea` draws until the size changes.
    Failed {
        size: (i32, i32),
    },
}

/// What the snapshot does with a frame of a given device size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Decide (`dmabuf_target::decide`) and set the presenter up.
    Decide,
    /// Set the presenter up again after a mid-run failure, at a new size (one retry spent).
    Retry,
    /// Draw into the presenter's buffers.
    Draw,
    /// Chain up to `GtkGLArea`'s own snapshot.
    Fallback,
}

/// Whether the area presents through its own buffers, for one GL context. GTK-free, so the retry
/// rule is unit-tested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Lifecycle {
    state: State,
    retries_left: u32,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            state: State::Untried,
            retries_left: RETRIES_PER_CONTEXT,
        }
    }
}

impl Lifecycle {
    /// Whether a frame of `size` goes to `GtkGLArea` without trying anything. Changes nothing.
    fn falls_back_at(&self, size: (i32, i32)) -> bool {
        match self.state {
            State::Off => true,
            State::Failed { size: failed_at } => failed_at == size || self.retries_left == 0,
            State::Untried | State::Active => false,
        }
    }

    /// The step for a frame of `size`; spends a retry when it returns [`Step::Retry`].
    fn step(&mut self, size: (i32, i32)) -> Step {
        match self.state {
            State::Untried => Step::Decide,
            State::Active => Step::Draw,
            State::Off => Step::Fallback,
            State::Failed { .. } if self.falls_back_at(size) => Step::Fallback,
            State::Failed { .. } => {
                self.retries_left -= 1;
                Step::Retry
            }
        }
    }

    fn set_up(&mut self) {
        self.state = State::Active;
    }

    fn turn_off(&mut self) {
        self.state = State::Off;
    }

    /// A frame of `size` failed; returns the retries left for this GL context.
    fn frame_failed(&mut self, size: (i32, i32)) -> u32 {
        self.state = State::Failed { size };
        self.retries_left
    }
}

/// What the pane's presentation did since it was built, across every realize. Diagnostic: the
/// real-framebuffer regression (`tests/cursor_animation.rs`) fails a run on the own-buffer path if
/// a single frame of it went through `GtkGLArea`, or the path failed even once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PresentationCounts {
    /// Frames drawn into the pane's own dmabuf buffers and handed to GTK.
    pub own_frames: u64,
    /// Frames drawn through `GtkGLArea`'s texture (the fallback, or a frame redrawn after a failure).
    pub fallback_frames: u64,
    /// Times the own-buffer path failed and fell back: a presenter that could not be set up, or a
    /// frame that failed mid-run. Turning it off (`EITRI_EDITOR_DMABUF`, GTK below 4.16) is not one.
    pub failures: u32,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct EditorGlArea {
        pub(super) presenter: RefCell<Option<Presenter>>,
        pub(super) lifecycle: Cell<Lifecycle>,
        pub(super) last_resize: Cell<(i32, i32)>,
        pub(super) drawing_into_own_buffer: Cell<bool>,
        /// The fallback hint (`dmabuf_target::fallback_hint`) is printed at most once per pane.
        pub(super) fallback_hint_done: Cell<bool>,
        /// Never reset: it spans every realize.
        pub(super) counts: Cell<PresentationCounts>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EditorGlArea {
        const NAME: &'static str = "EitriEditorGlArea";
        type Type = super::EditorGlArea;
        type ParentType = gtk4::GLArea;
    }

    impl ObjectImpl for EditorGlArea {}

    impl WidgetImpl for EditorGlArea {
        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            if self.present_own(snapshot) {
                return;
            }
            self.drawing_into_own_buffer.set(false);
            // `GtkGLArea` emits `resize` itself while it draws; the next own frame emits it again,
            // whatever size `GtkGLArea` last reported.
            self.last_resize.set((0, 0));
            self.count(|c| c.fallback_frames += 1);
            self.note_fallback();
            self.parent_snapshot(snapshot);
        }

        /// A class handler, so it runs after the pane's own `connect_unrealize` (which releases
        /// Skia) and before `GtkGLArea`'s, which destroys the context these objects live in.
        fn unrealize(&self) {
            let area = self.obj();
            area.make_current();
            let current = area.error().is_none()
                && area
                    .context()
                    .is_some_and(|c| gtk4::gdk::GLContext::current().as_ref() == Some(&c));
            if let Some(presenter) = self.presenter.borrow_mut().take() {
                presenter.release(current);
            }
            // A new GL context, a new decision and a new retry budget.
            self.lifecycle.set(Lifecycle::default());
            self.last_resize.set((0, 0));
            self.drawing_into_own_buffer.set(false);
            self.parent_unrealize();
        }
    }

    impl GLAreaImpl for EditorGlArea {}

    impl EditorGlArea {
        fn count(&self, f: impl FnOnce(&mut PresentationCounts)) {
            let mut counts = self.counts.get();
            f(&mut counts);
            self.counts.set(counts);
        }

        fn update_lifecycle<R>(&self, f: impl FnOnce(&mut Lifecycle) -> R) -> R {
            let mut lifecycle = self.lifecycle.get();
            let r = f(&mut lifecycle);
            self.lifecycle.set(lifecycle);
            r
        }

        /// Once per pane: name the likely per-frame download when the fallback runs under GSK's
        /// Vulkan renderer on GTK 4.16+. Diagnostic only.
        fn note_fallback(&self) {
            use gtk4::prelude::NativeExt;
            if self.fallback_hint_done.get() {
                return;
            }
            let Some(native) = self.obj().native() else {
                return;
            };
            let Some(renderer) = native.renderer() else {
                return;
            };
            self.fallback_hint_done.set(true);
            let name = glib::prelude::ObjectExt::type_(&renderer).name();
            if let Some(hint) = dmabuf_target::fallback_hint(gtk4::minor_version(), true, name) {
                eprintln!("[editor] {hint}");
            }
        }

        /// `true` when this frame was handled (drawn, or -- like `GtkGLArea` -- nothing to draw);
        /// `false` to chain up to `GtkGLArea`'s own snapshot.
        fn present_own(&self, snapshot: &gtk4::Snapshot) -> bool {
            let area = self.obj();
            let (logical_w, logical_h) = (area.width(), area.height());
            let scale = area.scale_factor();
            let (w, h) = (logical_w * scale, logical_h * scale);
            if self.lifecycle.get().falls_back_at((w, h)) {
                return false;
            }
            if w <= 0 || h <= 0 {
                return true;
            }
            let Some(context) = area.context() else {
                return false;
            };
            if area.error().is_some() {
                return false;
            }
            area.make_current();
            if area.error().is_some() {
                return false;
            }
            match self.update_lifecycle(|l| l.step((w, h))) {
                Step::Fallback => return false,
                Step::Draw => {}
                Step::Decide => {
                    if !self.decide() || !self.set_up(&area) {
                        return false;
                    }
                }
                Step::Retry => {
                    if !self.set_up(&area) {
                        return false;
                    }
                }
            }
            let begun = self.presenter.borrow_mut().as_mut().map(|p| p.begin_frame(w, h));
            if let Some(Err(err)) = begun {
                self.give_up((w, h), &err);
                return false;
            }
            // No borrow of the presenter is held across the two emissions: the handlers are the
            // pane's, and they only read `draws_into_own_buffer`.
            if self.last_resize.get() != (w, h) {
                self.last_resize.set((w, h));
                area.emit_by_name::<()>("resize", &[&w, &h]);
            }
            self.drawing_into_own_buffer.set(true);
            let _handled: bool = area.emit_by_name("render", &[&context]);
            let finished = self
                .presenter
                .borrow_mut()
                .as_mut()
                .map(|p| p.finish_frame(&area.display()));
            match finished {
                Some(Ok(texture)) => {
                    // Rendered by GL, so bottom row first: flip it, as `gtk_gl_area_snapshot` does.
                    snapshot.save();
                    snapshot.translate(&graphene::Point::new(0.0, logical_h as f32));
                    snapshot.scale(1.0, -1.0);
                    snapshot.append_texture(
                        &texture,
                        &graphene::Rect::new(0.0, 0.0, logical_w as f32, logical_h as f32),
                    );
                    snapshot.restore();
                    self.count(|c| c.own_frames += 1);
                    true
                }
                Some(Err(err)) => {
                    self.give_up((w, h), &err);
                    // Draw this frame again, through `GtkGLArea`'s own buffer.
                    false
                }
                None => false,
            }
        }

        /// `EITRI_EDITOR_DMABUF` and the runtime GTK version, once per GL context.
        fn decide(&self) -> bool {
            let value = std::env::var_os(dmabuf_target::ENV);
            let gtk = (gtk4::major_version(), gtk4::minor_version(), gtk4::micro_version());
            match dmabuf_target::decide(value.as_deref(), gtk) {
                Ok(note) => {
                    if let Some(note) = note {
                        eprintln!("[editor] {note}");
                    }
                    true
                }
                Err(reason) => {
                    eprintln!("[editor] own presentation buffers {reason}; using GtkGLArea's texture");
                    self.update_lifecycle(Lifecycle::turn_off);
                    false
                }
            }
        }

        /// Requires the area's context to be current.
        fn set_up(&self, area: &super::EditorGlArea) -> bool {
            match Presenter::new(&area.display()) {
                Ok(presenter) => {
                    *self.presenter.borrow_mut() = Some(presenter);
                    self.update_lifecycle(Lifecycle::set_up);
                    true
                }
                Err(err) => {
                    eprintln!("[editor] own presentation buffers unavailable ({err}); using GtkGLArea's texture");
                    self.update_lifecycle(Lifecycle::turn_off);
                    self.count(|c| c.failures += 1);
                    false
                }
            }
        }

        fn give_up(&self, (w, h): (i32, i32), err: &str) {
            let retries_left = self.update_lifecycle(|l| l.frame_failed((w, h)));
            self.count(|c| c.failures += 1);
            self.drawing_into_own_buffer.set(false);
            if let Some(presenter) = self.presenter.borrow_mut().take() {
                // Only called from the snapshot, after `make_current` succeeded.
                presenter.release(true);
            }
            let then = match retries_left {
                0 => "for the rest of this GL context (no retries left)".to_string(),
                1 => "until the size changes (1 retry left for this GL context)".to_string(),
                n => format!("until the size changes ({n} retries left for this GL context)"),
            };
            eprintln!("[editor] own presentation buffers failed at {w}x{h} ({err}); using GtkGLArea's texture {then}");
        }
    }
}

glib::wrapper! {
    pub struct EditorGlArea(ObjectSubclass<imp::EditorGlArea>)
        @extends gtk4::GLArea, gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl EditorGlArea {
    /// Whether the `render` now being emitted draws into one of this area's own tiled buffers,
    /// which Skia can draw into directly. `false` means `GtkGLArea`'s own framebuffer is bound.
    pub(crate) fn draws_into_own_buffer(&self) -> bool {
        self.imp().drawing_into_own_buffer.get()
    }

    pub(crate) fn presentation_counts(&self) -> PresentationCounts {
        self.imp().counts.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: (i32, i32) = (820, 560);
    const B: (i32, i32) = (1640, 1120);
    const C: (i32, i32) = (900, 600);

    #[test]
    fn a_new_context_decides_once_then_draws() {
        let mut l = Lifecycle::default();
        assert!(!l.falls_back_at(A));
        assert_eq!(l.step(A), Step::Decide);
        l.set_up();
        assert_eq!(l.step(A), Step::Draw);
        assert_eq!(
            l.step(B),
            Step::Draw,
            "a size change is the presenter's own business while active"
        );
    }

    #[test]
    fn turned_off_or_not_set_up_stays_off_at_every_size_and_spends_no_retry() {
        let mut l = Lifecycle::default();
        assert_eq!(l.step(A), Step::Decide);
        l.turn_off();
        for size in [A, B, C, A] {
            assert!(l.falls_back_at(size));
            assert_eq!(l.step(size), Step::Fallback);
        }
        assert_eq!(l.retries_left, RETRIES_PER_CONTEXT);
    }

    #[test]
    fn a_mid_run_failure_falls_back_at_that_size_and_retries_at_the_next_one() {
        let mut l = Lifecycle::default();
        l.step(A);
        l.set_up();
        assert_eq!(l.frame_failed(A), RETRIES_PER_CONTEXT);
        assert!(l.falls_back_at(A));
        assert_eq!(l.step(A), Step::Fallback, "the same size is not retried");
        assert_eq!(l.retries_left, RETRIES_PER_CONTEXT);
        assert!(!l.falls_back_at(B));
        assert_eq!(l.step(B), Step::Retry);
        assert_eq!(l.retries_left, RETRIES_PER_CONTEXT - 1);
        l.set_up();
        assert_eq!(
            l.step(B),
            Step::Draw,
            "one transient failure no longer costs the session"
        );
    }

    #[test]
    fn the_retry_budget_is_per_context_and_ends_in_a_fallback_for_good() {
        let mut l = Lifecycle::default();
        l.step(A);
        l.set_up();
        let sizes = [A, B, C, A];
        for (i, size) in sizes.iter().enumerate() {
            // Fails at every size, even after a successful set-up.
            assert_eq!(l.frame_failed(*size), RETRIES_PER_CONTEXT - i as u32);
            let next = sizes.get(i + 1).copied().unwrap_or(B);
            if l.retries_left > 0 {
                assert_eq!(l.step(next), Step::Retry);
                l.set_up();
            }
        }
        assert_eq!(l.retries_left, 0);
        for size in [A, B, C] {
            assert!(l.falls_back_at(size));
            assert_eq!(l.step(size), Step::Fallback);
        }
        // A re-realize is a new GL context: a new decision and a full budget.
        let l = Lifecycle::default();
        assert_eq!((l.state, l.retries_left), (State::Untried, RETRIES_PER_CONTEXT));
    }

    #[test]
    fn a_retry_whose_set_up_fails_is_off_until_the_next_realize() {
        let mut l = Lifecycle::default();
        l.step(A);
        l.set_up();
        l.frame_failed(A);
        assert_eq!(l.step(B), Step::Retry);
        l.turn_off();
        for size in [A, B, C] {
            assert_eq!(l.step(size), Step::Fallback);
        }
        assert_eq!(l.retries_left, RETRIES_PER_CONTEXT - 1);
    }
}
