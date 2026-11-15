use super::*;
use std::cell::Cell;

#[derive(Default)]
struct LoadingPage {
    loads: RendererLoad,
    installed: Cell<bool>,
    pending: RefCell<Option<(String, DiagramStarted, DiagramDone)>>,
    drawing: RefCell<Option<DiagramDone>>,
    received: RefCell<Vec<String>>,
    killed: Cell<usize>,
    clock: Cell<Option<Instant>>,
}
impl LoadingPage {
    fn dispatch(&self) {
        if self.loads.is_ready() {
            if let Some((source, started, done)) = self.pending.borrow_mut().take() {
                if !started(self.clock.get().unwrap_or_else(Instant::now)) {
                    return;
                }
                self.received.borrow_mut().push(source);
                *self.drawing.borrow_mut() = Some(done);
            }
        }
    }
    fn page_ready(&self) {
        assert!(self.installed.get());
        self.loads.ready_at(self.clock.get().unwrap_or_else(Instant::now));
        self.dispatch();
    }
    fn complete(&self) {
        self.drawing.borrow_mut().take().unwrap()(DiagramOutcome::svg("<svg/>"));
    }
}
impl DiagramRenderer for LoadingPage {
    fn render(&self, source: &str, _: &str, startup_deadline: Instant, started: DiagramStarted, done: DiagramDone) {
        if !self.installed.get() {
            if let Err(error) = self
                .loads
                .build_and_install(startup_deadline, || (), |_| self.installed.set(true))
            {
                done(error);
                return;
            }
        }
        *self.pending.borrow_mut() = Some((source.into(), started, done));
        self.dispatch();
    }
    fn stop(&self) {
        self.loads.stopped();
        self.installed.set(false);
        let done = self.pending.borrow_mut().take().map(|(_, _, done)| done);
        let startup = done.is_some();
        if let Some(done) = done.or_else(|| self.drawing.borrow_mut().take()) {
            done(if startup {
                DiagramOutcome::startup()
            } else {
                DiagramOutcome::error("the renderer stopped", true)
            });
        }
        self.killed.set(self.killed.get() + 1);
    }
    fn cancel(&self) {
        self.loads.cancelled();
        self.stop();
    }
    fn drop_idle(&self) {
        self.cancel();
    }
}
fn loading_page() -> (Rc<LoadingPage>, DiagramClient) {
    let page = Rc::new(LoadingPage::default());
    let mut client = DiagramClient::default();
    client.install(Rc::new(RefCell::new(DiagramQueue::new(page.clone()))), None);
    (page, client)
}

#[test]
fn late_ready_page_receives_its_first_source_only_after_ready_without_kill_or_error() {
    let (page, mut client) = loading_page();
    let now = Instant::now();
    ask(&mut client, now, "A", "late");
    assert!(client.poll(now).is_empty());
    assert!(client.poll(now + RENDER_BUDGET / 2).is_empty());
    assert!(page.received.borrow().is_empty());
    assert_eq!(page.killed.get(), 0);
    assert_eq!(page.loads.failed_loads(), 1);
    page.page_ready();
    assert_eq!(&*page.received.borrow(), &["A"]);
    assert_eq!(page.loads.failed_loads(), 0);
    page.complete();
    let replies = client.poll(now + RENDER_BUDGET / 2);
    assert_eq!(page.killed.get(), 0);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].outcome.svg.as_deref(), Some("<svg/>"));
    assert!(replies[0].outcome.error.is_none());
}

#[test]
fn never_ready_pages_exhaust_three_loads_at_four_seconds_each() {
    let (page, mut client) = loading_page();
    let now = Instant::now();
    for attempt in 0..3 {
        let start = now + RENDER_BUDGET * attempt;
        ask(&mut client, start, "A", "never");
        assert!(client.poll(start).is_empty());
        assert!(client.poll(start + RENDER_BUDGET - Duration::from_nanos(1)).is_empty());
        page.clock.set(Some(start + RENDER_BUDGET));
        page.page_ready();
        assert!(page.received.borrow().is_empty());
        let replies = client.poll(start + RENDER_BUDGET);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].outcome.error.as_deref(), Some("the renderer stopped"));
        assert!(replies[0].outcome.startup);
        assert_eq!(page.loads.failed_loads(), (attempt + 1) as u8);
        assert_eq!(page.killed.get(), (attempt + 1) as usize);
        assert!(!page.loads.is_ready());
    }
    ask(&mut client, now + RENDER_BUDGET * 3, "A", "exhausted");
    let replies = client.poll(now + RENDER_BUDGET * 3);
    assert_eq!(replies.len(), 1);
    assert_eq!(
        replies[0].outcome.error.as_deref(),
        Some("the diagram renderer could not start")
    );
    assert!(!replies[0].outcome.transient);
    assert!(page.received.borrow().is_empty());
    assert_eq!(page.killed.get(), 3);
}

#[test]
fn stopping_a_ready_page_reports_a_renderer_stop_and_replacement_can_become_ready() {
    let (page, mut client) = loading_page();
    let now = Instant::now();
    ask(&mut client, now, "A", "stopped");
    assert!(client.poll(now).is_empty());
    page.page_ready();
    assert_eq!(&*page.received.borrow(), &["A"]);
    page.stop();
    let replies = client.poll(now);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].outcome.error.as_deref(), Some("the renderer stopped"));
    assert!(replies[0].outcome.transient);
    assert_eq!(page.killed.get(), 1);
    assert!(!page.loads.is_ready());
    assert_eq!(page.loads.failed_loads(), 0);
    ask(&mut client, now, "B", "replacement");
    assert!(client.poll(now).is_empty());
    page.page_ready();
    assert_eq!(&*page.received.borrow(), &["A", "B"]);
    assert_eq!(page.loads.failed_loads(), 0);
    page.complete();
    let replies = client.poll(now);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].outcome.svg.as_deref(), Some("<svg/>"));
    assert!(replies[0].outcome.error.is_none());
    assert_eq!(page.killed.get(), 1);
}

#[derive(Default)]
struct Fake {
    calls: RefCell<Vec<DiagramDone>>,
    sources: RefCell<Vec<String>>,
    stopped: Cell<usize>,
    dropped: Cell<usize>,
    clock: Cell<Option<Instant>>,
}
impl DiagramRenderer for Fake {
    fn render(&self, source: &str, _vars: &str, _: Instant, started: DiagramStarted, done: DiagramDone) {
        assert!(started(self.clock.get().unwrap()));
        self.sources.borrow_mut().push(source.into());
        self.calls.borrow_mut().push(done);
    }
    fn stop(&self) {
        self.stopped.set(self.stopped.get() + 1);
    }
    fn cancel(&self) {
        self.stop();
    }
    fn drop_idle(&self) {
        self.dropped.set(self.dropped.get() + 1);
    }
}
fn setup(now: Instant) -> (Rc<Fake>, DiagramClient, DiagramClient) {
    let renderer = Rc::new(Fake::default());
    renderer.clock.set(Some(now));
    let queue = Rc::new(RefCell::new(DiagramQueue::new(renderer.clone())));
    let mut a = DiagramClient::default();
    let mut b = DiagramClient::default();
    a.install(queue.clone(), None);
    b.install(queue, None);
    (renderer, a, b)
}
fn ask(client: &mut DiagramClient, now: Instant, source: &str, id: &str) {
    client.request(now, ViewId::BASE, id.into(), 1, source.into(), "{}".into());
}
#[test]
fn shares_one_renderer_but_windows_keep_distinct_requests_and_only_askers_poll() {
    let now = Instant::now();
    let (renderer, mut a, mut b) = setup(now);
    let generation_a = next_theme_generation();
    let generation_b = next_theme_generation();
    a.request(now, ViewId::BASE, "a".into(), generation_a, "A".into(), "{}".into());
    assert!(a.poll(now).is_empty());
    assert!(a.needs_polling());
    assert!(!b.needs_polling());
    assert_eq!(b.earliest(), Some(now + RENDER_BUDGET));
    b.request(now, ViewId::BASE, "b".into(), generation_b, "A".into(), "{}".into());
    assert!(b.needs_polling());
    assert_eq!(&*renderer.sources.borrow(), &["A"]);
    renderer.calls.borrow_mut().remove(0)(DiagramOutcome::svg("<svg/>"));
    assert_eq!(a.poll(now)[0].id, "a");
    assert!(b.poll(now).is_empty());
    assert!(!a.needs_polling());
    assert!(b.needs_polling());
    assert_eq!(&*renderer.sources.borrow(), &["A", "A"]);
    renderer.calls.borrow_mut().remove(0)(DiagramOutcome::svg("<svg/>"));
    assert_eq!(b.poll(now)[0].id, "b");
    assert!(a.poll(now).is_empty());
}
#[test]
fn kills_once_discards_late_completion_and_drops_idle() {
    let now = Instant::now();
    let (renderer, mut a, _) = setup(now);
    ask(&mut a, now, "A", "a");
    assert!(a.poll(now).is_empty());
    assert_eq!(a.earliest(), Some(now + RENDER_BUDGET));
    let result = a.poll(now + RENDER_BUDGET);
    assert_eq!(renderer.stopped.get(), 1);
    assert_eq!(result[0].outcome.error.as_deref(), Some("took longer than 4 s"));
    renderer.calls.borrow_mut().remove(0)(DiagramOutcome::svg("<svg/>"));
    assert!(a.poll(now + RENDER_BUDGET).is_empty());
    a.poll(now + RENDER_BUDGET + IDLE_DROP);
    assert_eq!(renderer.dropped.get(), 1);
    assert_eq!(a.earliest(), None);
}
#[test]
fn bounds_waiting_per_window_and_output_and_source() {
    let now = Instant::now();
    let (renderer, mut a, _) = setup(now);
    ask(&mut a, now, "A", "a");
    for i in 0..32 {
        ask(&mut a, now, &format!("B{i}"), &format!("b{i}"));
    }
    ask(&mut a, now, "overflow", "full");
    assert_eq!(
        a.poll(now)[0].outcome.error.as_deref(),
        Some("too many diagrams waiting")
    );
    renderer.calls.borrow_mut().remove(0)(DiagramOutcome::svg(&"x".repeat(MAX_SVG_BYTES + 1)));
    assert_eq!(
        a.poll(now)[0].outcome.error.as_deref(),
        Some("the drawing is too large")
    );
    ask(&mut a, now, "A\0B", "nul");
    assert_eq!(
        a.poll(now)[0].outcome.error.as_deref(),
        Some("it holds hidden characters")
    );
}
#[test]
fn class_relations_are_subject_to_the_universal_budget_and_rebuild() {
    let now = Instant::now();
    let (renderer, mut a, _) = setup(now);
    let source = format!(
        "classDiagram\n{}",
        (0..1000).map(|i| format!("A{i} --> B{i}\n")).collect::<String>()
    );
    assert!(source.len() < 20_000);
    // A single long line exercises types whose relation count is not checked by Mermaid's flowchart limit.
    let source = source.replace('\n', ";");
    ask(&mut a, now, &source, "class");
    assert!(a.poll(now).is_empty());
    assert_eq!(
        a.poll(now + RENDER_BUDGET)[0].outcome.error.as_deref(),
        Some("took longer than 4 s")
    );
    renderer.clock.set(Some(now + RENDER_BUDGET));
    ask(&mut a, now + RENDER_BUDGET, "flowchart LR;A-->B", "next");
    assert!(a.poll(now + RENDER_BUDGET).is_empty());
    assert_eq!(renderer.sources.borrow().len(), 2);
    renderer.calls.borrow_mut().pop().unwrap()(DiagramOutcome::svg("<svg/>"));
    assert!(a.poll(now + RENDER_BUDGET)[0].outcome.svg.is_some());
}

#[test]
fn completions_wake_without_borrowing_the_panel_and_reload_discards_old_ids() {
    let now = Instant::now();
    let renderer = Rc::new(Fake::default());
    renderer.clock.set(Some(now));
    let queue = Rc::new(RefCell::new(DiagramQueue::new(renderer.clone())));
    let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let raised = wakes.clone();
    let mut client = DiagramClient::default();
    client.install(
        queue,
        Some(std::sync::Arc::new(move || {
            raised.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        })),
    );
    ask(&mut client, now, "A", "old");
    client.poll(now);
    client.reset(ViewId::BASE);
    renderer.calls.borrow_mut().remove(0)(DiagramOutcome::svg("<svg/>"));
    assert!(wakes.load(std::sync::atomic::Ordering::Relaxed) > 0);
    assert!(client.poll(now).is_empty());
}

#[test]
fn the_first_completed_outcome_wins_even_when_the_tick_sees_two() {
    let now = Instant::now();
    let (renderer, mut a, _) = setup(now);
    ask(&mut a, now, "A", "a");
    a.poll(now);
    let queue = a.queue().unwrap();
    let attempt = queue.borrow().flight.as_ref().unwrap().attempt;
    queue.borrow().completed.borrow_mut().push(Completion {
        attempt,
        at: now,
        outcome: DiagramOutcome::error("the renderer stopped", true),
    });
    renderer.calls.borrow_mut().remove(0)(DiagramOutcome::svg("<svg/>"));
    let results = a.poll(now);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].outcome.error.as_deref(), Some("the renderer stopped"));
}

#[test]
fn three_deliberate_window_closes_leave_the_fourth_load_free_to_draw() {
    let page = Rc::new(LoadingPage::default());
    let queue = Rc::new(RefCell::new(DiagramQueue::new(page.clone())));
    let now = Instant::now();
    for _ in 0..3 {
        let mut client = DiagramClient::default();
        client.install(queue.clone(), None);
        ask(&mut client, now, "A", "closed");
        assert!(client.poll(now).is_empty());
        let (_, owner) = client.close().unwrap();
        queue.borrow_mut().release(owner);
    }
    let mut client = DiagramClient::default();
    client.install(queue, None);
    ask(&mut client, now, "A", "fourth");
    assert!(client.poll(now).is_empty(), "closing a window must not exhaust startup");
    page.page_ready();
    assert_eq!(&*page.received.borrow(), &["A"]);
    page.complete();
    assert!(client.poll(now)[0].outcome.svg.is_some());
}

#[test]
fn startup_expiry_reports_no_source_strike_but_ready_page_stops_do() {
    let (page, mut client) = loading_page();
    let now = Instant::now();
    ask(&mut client, now, "A", "startup");
    client.poll(now);
    let outcome = &client.poll(now + RENDER_BUDGET)[0].outcome;
    let wire: serde_json::Value = serde_json::from_str(&outcome.envelope("startup")).unwrap();
    assert_eq!(wire["startup"], true);
    assert_eq!(outcome.error.as_deref(), Some("the renderer stopped"));
    assert!(outcome.transient);
    for attempt in 1..=2 {
        let start = now + RENDER_BUDGET * attempt;
        page.clock.set(Some(start));
        ask(&mut client, start, "A", "draw-stop");
        assert!(client.poll(start).is_empty());
        page.page_ready();
        page.stop();
        let outcomes = client.poll(start);
        assert_eq!(outcomes[0].outcome.error.as_deref(), Some("the renderer stopped"));
        assert!(outcomes[0].outcome.transient);
        assert!(!outcomes[0].outcome.startup);
    }
    assert_eq!(&*page.received.borrow(), &["A", "A"]);
}

#[test]
fn a_late_ready_page_has_four_seconds_for_its_own_draw() {
    let (page, mut client) = loading_page();
    let now = Instant::now();
    ask(&mut client, now, "A", "draw");
    client.poll(now);
    let ready = now + RENDER_BUDGET / 2;
    page.clock.set(Some(ready));
    page.page_ready();
    assert!(
        client.poll(now + RENDER_BUDGET).is_empty(),
        "startup must not spend the draw budget"
    );
    assert_eq!(client.earliest(), Some(ready + RENDER_BUDGET));
    let outcomes = client.poll(ready + RENDER_BUDGET);
    assert_eq!(outcomes[0].outcome.error.as_deref(), Some("took longer than 4 s"));
    assert_eq!(
        serde_json::to_value(&outcomes[0].outcome).unwrap()["startup"],
        serde_json::Value::Null
    );
}
