use super::DiagramOutcome;
use std::{cell::Cell, rc::Rc, time::Instant};

/// A new process consumes a load attempt until its library has answered successfully.
#[derive(Clone, Default)]
pub struct RendererLoad {
    failed: Rc<Cell<u8>>,
    page_ready: Rc<Cell<bool>>,
    deadline: Rc<Cell<Option<Instant>>>,
}
impl RendererLoad {
    /// Install a fresh view with the queue's startup deadline intact.
    pub fn build_and_install<T>(
        &self,
        deadline: Instant,
        factory: impl FnOnce() -> T,
        install: impl FnOnce(&T),
    ) -> Result<T, DiagramOutcome> {
        // Retire the previous view's readiness before the new load owns the deadline.
        self.stopped();
        let view = self.build(factory)?;
        self.deadline.set(Some(deadline));
        install(&view);
        Ok(view)
    }

    pub fn build<T>(&self, factory: impl FnOnce() -> T) -> Result<T, DiagramOutcome> {
        if self.failed.get() >= 3 {
            return Err(DiagramOutcome::error("the diagram renderer could not start", false));
        }
        self.page_ready.set(false);
        self.deadline.set(Some(Instant::now() + super::RENDER_BUDGET));
        self.failed.set(self.failed.get() + 1);
        Ok(factory())
    }
    pub fn ready(&self) -> bool {
        self.ready_at(Instant::now())
    }
    pub(super) fn ready_at(&self, now: Instant) -> bool {
        if self.is_ready() {
            return true;
        }
        // A queued timeout can run after the page's message; a late message must not forgive that load.
        if self.deadline.get().is_none_or(|deadline| now >= deadline) {
            return false;
        }
        self.page_ready.set(true);
        self.deadline.set(None);
        self.failed.set(0);
        true
    }
    pub fn is_ready(&self) -> bool {
        self.page_ready.get()
    }
    pub fn stopped(&self) {
        self.page_ready.set(false);
        self.deadline.set(None);
    }
    /// Deliberately retiring an unready view refunds its provisional load charge.
    pub fn cancelled(&self) {
        if self.deadline.get().is_some() {
            self.failed.set(self.failed.get().saturating_sub(1));
        }
        self.stopped();
    }
    #[cfg(test)]
    pub(super) fn failed_loads(&self) -> u8 {
        self.failed.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_at_the_deadline_or_after_stop_cannot_release_source_or_forgive_the_load() {
        let loads = RendererLoad::default();
        loads.build(|| ()).unwrap();
        let deadline = loads.deadline.get().unwrap();
        assert!(!loads.ready_at(deadline));
        assert!(!loads.is_ready());
        loads.stopped();
        assert!(!loads.ready_at(deadline - super::super::RENDER_BUDGET));
        loads.build(|| ()).unwrap();
        loads.build(|| ()).unwrap();
        assert!(loads.build(|| ()).is_err());
    }

    #[test]
    fn failed_renderer_factories_stop_after_three_builds_and_readiness_resets_the_budget() {
        let loads = RendererLoad::default();
        let builds = std::cell::Cell::new(0);
        for _ in 0..8 {
            let result = loads.build(|| {
                builds.set(builds.get() + 1);
                Err::<(), _>("missing renderer asset")
            });
            if builds.get() < 3 {
                assert!(matches!(result, Ok(Err(_))));
            }
        }
        assert_eq!(builds.get(), 3);
        let error = loads.build(|| panic!("exhausted renderer must not build")).unwrap_err();
        assert_eq!(error.error.as_deref(), Some("the diagram renderer could not start"));
        assert!(!error.transient);

        let recovering = RendererLoad::default();
        for _ in 0..2 {
            recovering.build(|| ()).unwrap();
        }
        recovering.ready();
        for _ in 0..3 {
            recovering.build(|| ()).unwrap();
        }
        assert!(recovering.build(|| ()).is_err());
    }
}
