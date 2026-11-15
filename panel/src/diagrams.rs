//! Bounded, process-shared diagram work. Renderer callbacks write to a separate completion slot and wake the
//! asking panel; only its tick delivers page messages, after the panel-state borrow has ended.
use eitri_core::tab_set::ViewId;
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    rc::Rc,
    time::{Duration, Instant},
};

mod load_budget;
pub use load_budget::RendererLoad;

pub const RENDER_BUDGET: Duration = Duration::from_secs(4);
pub const IDLE_DROP: Duration = Duration::from_secs(10);
pub const MAX_SVG_BYTES: usize = 512 * 1024;
const MAX_WAITING: usize = 32;
/// Records the handoff to a ready page; an expired startup refuses the handoff.
pub type DiagramStarted = Box<dyn FnOnce(Instant) -> bool>;
pub type DiagramDone = Box<dyn FnOnce(DiagramOutcome)>;

pub trait DiagramRenderer {
    fn render(
        &self,
        source: &str,
        vars_json: &str,
        startup_deadline: Instant,
        started: DiagramStarted,
        done: DiagramDone,
    );
    fn stop(&self);
    fn cancel(&self);
    fn drop_idle(&self);
    fn available(&self) -> bool {
        true
    }
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct DiagramOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub svg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub later: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub transient: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub startup: bool,
}
impl DiagramOutcome {
    pub fn svg(svg: &str) -> Self {
        Self {
            svg: Some(svg.into()),
            ..Self::default()
        }
    }
    pub fn error(error: &str, transient: bool) -> Self {
        Self {
            error: Some(error.into()),
            transient,
            ..Self::default()
        }
    }
    pub fn startup() -> Self {
        Self {
            startup: true,
            ..Self::error("the renderer stopped", true)
        }
    }
    pub fn disabled() -> Self {
        Self {
            disabled: true,
            ..Self::default()
        }
    }
    fn bounded(self) -> Self {
        if self.disabled {
            return Self::disabled();
        }
        if self.svg.as_ref().is_some_and(|s| s.len() > MAX_SVG_BYTES) {
            return Self::error("the drawing is too large", false);
        }
        match (&self.svg, &self.error) {
            (Some(_), None) => self,
            (None, Some(_)) => Self {
                error: self.error.map(|s| s.chars().take(200).collect()),
                ..self
            },
            _ => Self::error("the renderer stopped", true),
        }
    }
    pub fn envelope(&self, id: &str) -> String {
        let mut value = serde_json::to_value(self).expect("diagram outcome serializes");
        value["kind"] = "diagram".into();
        value["id"] = id.into();
        value.to_string()
    }
}
#[derive(Clone)]
pub(crate) struct Delivery {
    pub view: ViewId,
    pub id: String,
    pub outcome: DiagramOutcome,
    epoch: u64,
}
type Sink = Rc<RefCell<Vec<Delivery>>>;
struct Asker {
    owner: u64,
    view: ViewId,
    id: String,
    epoch: u64,
    sink: Sink,
    wake: Option<agent::UiWake>,
}
impl Asker {
    fn deliver(self, outcome: DiagramOutcome) {
        self.sink.borrow_mut().push(Delivery {
            view: self.view,
            id: self.id,
            outcome,
            epoch: self.epoch,
        });
        if let Some(wake) = self.wake {
            wake();
        }
    }
}
struct Job {
    generation: u64,
    source: String,
    vars: String,
    askers: Vec<Asker>,
}
struct Flight {
    attempt: u64,
    startup_deadline: Instant,
    started: Rc<Cell<Option<Instant>>>,
    job: Job,
}
impl Flight {
    fn deadline(&self) -> Instant {
        self.started
            .get()
            .map_or(self.startup_deadline, |at| at + RENDER_BUDGET)
    }
}
struct Completion {
    attempt: u64,
    at: Instant,
    outcome: DiagramOutcome,
}

/// Shared by every panel using one renderer. A callback never borrows this queue, even when a render fails inline.
pub struct DiagramQueue {
    renderer: Rc<dyn DiagramRenderer>,
    available: bool,
    waiting: VecDeque<Job>,
    flight: Option<Flight>,
    completed: Rc<RefCell<Vec<Completion>>>,
    attempt: u64,
    idle: Option<Instant>,
}
impl DiagramQueue {
    pub fn new(renderer: Rc<dyn DiagramRenderer>) -> Self {
        Self {
            available: renderer.available(),
            renderer,
            waiting: VecDeque::new(),
            flight: None,
            completed: Rc::default(),
            attempt: 0,
            idle: None,
        }
    }
    fn request(&mut self, now: Instant, generation: u64, source: String, vars: String, asker: Asker) {
        if let Err(error) = eitri_core::agent_bridge::validate_diagram_source(&source) {
            asker.deliver(DiagramOutcome::error(error, false));
            return;
        }
        if !self.available {
            asker.deliver(DiagramOutcome::startup());
            return;
        }
        let same = |job: &Job| job.generation == generation && job.source == source;
        if let Some(job) = self
            .flight
            .as_mut()
            .map(|flight| &mut flight.job)
            .filter(|job| same(job))
        {
            // Repeated ids from one document do not grow the waiter list.
            if !job
                .askers
                .iter()
                .any(|a| a.owner == asker.owner && a.epoch == asker.epoch && a.view == asker.view && a.id == asker.id)
            {
                if job.askers.iter().filter(|a| a.owner == asker.owner).count() >= MAX_WAITING {
                    asker.deliver(DiagramOutcome::error("too many diagrams waiting", true));
                } else {
                    job.askers.push(asker);
                }
            }
            return;
        }
        let waiting = self
            .waiting
            .iter()
            .flat_map(|job| &job.askers)
            .filter(|a| a.owner == asker.owner)
            .count();
        if waiting >= MAX_WAITING {
            asker.deliver(DiagramOutcome::error("too many diagrams waiting", true));
            return;
        }
        if let Some(job) = self.waiting.iter_mut().find(|job| same(job)) {
            if !job
                .askers
                .iter()
                .any(|a| a.owner == asker.owner && a.epoch == asker.epoch && a.view == asker.view && a.id == asker.id)
            {
                job.askers.push(asker);
            }
        } else {
            self.waiting.push_back(Job {
                generation,
                source,
                vars,
                askers: vec![asker],
            });
        }
        self.idle = None;
        let _ = now;
    }
    fn start(&mut self, now: Instant) {
        if self.flight.is_some() {
            return;
        }
        let Some(job) = self.waiting.pop_front() else {
            return;
        };
        self.attempt += 1;
        let attempt = self.attempt;
        let completed = self.completed.clone();
        let wakes: Vec<_> = job.askers.iter().filter_map(|a| a.wake.clone()).collect();
        let source = job.source.clone();
        let vars = job.vars.clone();
        let started = Rc::new(Cell::new(None));
        let startup_deadline = now + RENDER_BUDGET;
        self.flight = Some(Flight {
            attempt,
            startup_deadline,
            started: started.clone(),
            job,
        });
        let start_wakes = wakes.clone();
        self.renderer.render(
            &source,
            &vars,
            startup_deadline,
            Box::new(move |at| {
                if at >= startup_deadline {
                    return false;
                }
                started.set(Some(at));
                for wake in start_wakes {
                    wake();
                }
                true
            }),
            Box::new(move |outcome| {
                completed.borrow_mut().push(Completion {
                    attempt,
                    at: Instant::now(),
                    outcome,
                });
                for wake in wakes {
                    wake();
                }
            }),
        );
    }
    fn finish(&mut self, now: Instant, outcome: DiagramOutcome) {
        let Some(flight) = self.flight.take() else {
            return;
        };
        let outcome = outcome.bounded();
        for asker in flight.job.askers {
            asker.deliver(outcome.clone());
        }
        self.idle = Some(now + IDLE_DROP);
        self.start(now);
    }
    pub(crate) fn poll(&mut self, now: Instant) {
        self.start(now);
        let completed = std::mem::take(&mut *self.completed.borrow_mut());
        for done in completed {
            if let Some(flight) = &self.flight {
                if done.attempt == flight.attempt && done.at <= flight.deadline() {
                    self.finish(now, done.outcome);
                }
            }
        }
        if self.flight.as_ref().is_some_and(|flight| now >= flight.deadline()) {
            // Remove the attempt before killing: termination and call-error callbacks can arrive synchronously.
            let flight = self.flight.take().unwrap();
            self.renderer.stop();
            for asker in flight.job.askers {
                asker.deliver(if flight.started.get().is_some() {
                    DiagramOutcome::error("took longer than 4 s", true)
                } else {
                    DiagramOutcome::startup()
                });
            }
            self.idle = Some(now + IDLE_DROP);
            self.start(now);
        }
        if self.flight.is_none() && self.idle.is_some_and(|due| now >= due) {
            self.idle = None;
            self.renderer.drop_idle();
        }
    }
    pub(crate) fn release(&mut self, owner: u64) {
        for job in &mut self.waiting {
            job.askers.retain(|asker| asker.owner != owner);
        }
        self.waiting.retain(|job| !job.askers.is_empty());
        if let Some(flight) = &mut self.flight {
            flight.job.askers.retain(|asker| asker.owner != owner);
        }
        if self.flight.as_ref().is_some_and(|flight| flight.job.askers.is_empty()) {
            self.flight = None;
            self.renderer.cancel();
        }
        if self.flight.is_none() && self.waiting.is_empty() {
            self.idle = None;
            self.renderer.drop_idle();
        } else {
            self.start(Instant::now());
        }
    }
    fn earliest(&self) -> Option<Instant> {
        self.flight.as_ref().map(Flight::deadline).or(self.idle)
    }
}

pub(crate) struct DiagramClient {
    queue: Option<Rc<RefCell<DiagramQueue>>>,
    sink: Sink,
    wake: Option<agent::UiWake>,
    owner: u64,
    epochs: std::collections::BTreeMap<ViewId, u64>,
    pub enabled: bool,
}
impl Default for DiagramClient {
    fn default() -> Self {
        Self {
            queue: None,
            sink: Rc::default(),
            wake: None,
            owner: 0,
            epochs: Default::default(),
            enabled: true,
        }
    }
}
impl DiagramClient {
    pub fn install(&mut self, queue: Rc<RefCell<DiagramQueue>>, wake: Option<agent::UiWake>) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        self.owner = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.queue = Some(queue);
        self.wake = wake;
    }
    pub fn reset(&mut self, view: ViewId) {
        *self.epochs.entry(view).or_default() += 1;
    }
    pub fn available(&self) -> bool {
        self.enabled && self.queue.as_ref().is_some_and(|q| q.borrow().available)
    }
    pub fn request(&mut self, now: Instant, view: ViewId, id: String, generation: u64, source: String, vars: String) {
        let Some(queue) = &self.queue else {
            return;
        };
        queue.borrow_mut().request(
            now,
            generation,
            source,
            vars,
            Asker {
                owner: self.owner,
                view,
                id,
                epoch: *self.epochs.get(&view).unwrap_or(&0),
                sink: self.sink.clone(),
                wake: self.wake.clone(),
            },
        );
    }
    pub fn close(&mut self) -> Option<(Rc<RefCell<DiagramQueue>>, u64)> {
        self.sink.borrow_mut().clear();
        self.queue.take().map(|queue| (queue, self.owner))
    }
    pub fn queue(&self) -> Option<Rc<RefCell<DiagramQueue>>> {
        self.queue.clone()
    }
    pub fn drain(&mut self) -> Vec<Delivery> {
        std::mem::take(&mut *self.sink.borrow_mut())
            .into_iter()
            .filter(|d| d.epoch == *self.epochs.get(&d.view).unwrap_or(&0))
            .collect()
    }
    #[cfg(test)]
    pub fn poll(&mut self, now: Instant) -> Vec<Delivery> {
        if let Some(queue) = &self.queue {
            queue.borrow_mut().poll(now);
        }
        std::mem::take(&mut *self.sink.borrow_mut())
            .into_iter()
            .filter(|d| d.epoch == *self.epochs.get(&d.view).unwrap_or(&0))
            .collect()
    }
    pub fn earliest(&self) -> Option<Instant> {
        self.queue.as_ref().and_then(|q| q.borrow().earliest())
    }
    pub fn needs_polling(&self) -> bool {
        self.queue.as_ref().is_some_and(|q| {
            let queue = q.borrow();
            let asks = |job: &Job| job.askers.iter().any(|asker| asker.owner == self.owner);
            queue.flight.as_ref().is_some_and(|flight| asks(&flight.job)) || queue.waiting.iter().any(asks)
        })
    }
}

#[cfg(test)]
mod tests;

/// Each window owns its theme generation; distinct windows never deduplicate their requests.
pub(crate) fn next_theme_generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
