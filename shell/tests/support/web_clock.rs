//! A host-driven JavaScript clock with idempotent steps across document replacement.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::glib;
use gtk4::prelude::*;
use webkit6::prelude::*;

thread_local! {
    static VIEWS: RefCell<Vec<(glib::WeakRef<webkit6::WebView>, Rc<Cell<u64>>, Rc<Cell<u64>>)>> = const { RefCell::new(Vec::new()) };
}

pub fn install(manager: &webkit6::UserContentManager) -> Rc<Cell<u64>> {
    assert!(manager.register_script_message_handler("eitriTestClock", None));
    let turns = Rc::new(Cell::new(0));
    let observed = turns.clone();
    manager.connect_script_message_received(Some("eitriTestClock"), move |_, value| {
        if let Ok(turn) = value.to_str().parse::<u64>() {
            observed.set(observed.get().max(turn));
        }
    });
    manager.add_script(&webkit6::UserScript::new(
        include_str!("web_clock.js"),
        webkit6::UserContentInjectedFrames::TopFrame,
        webkit6::UserScriptInjectionTime::Start,
        &[],
        &[],
    ));
    turns
}

pub fn watch(view: &webkit6::WebView, turns: Rc<Cell<u64>>) {
    let document = Rc::new(Cell::new(0));
    let committed = document.clone();
    view.connect_load_changed(move |_, event| {
        if event == webkit6::LoadEvent::Committed {
            committed.set(committed.get() + 1);
        }
    });
    VIEWS.with(|views| views.borrow_mut().push((view.downgrade(), turns, document)));
}

fn until(deadline: Instant, mut done: impl FnMut() -> bool) -> Result<(), String> {
    while !done() {
        if Instant::now() >= deadline {
            return Err("the web clock's frame was never acknowledged".into());
        }
        glib::MainContext::default().iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

fn evaluation(result: Result<webkit6::javascriptcore::Value, glib::Error>) -> Result<bool, ()> {
    result.map(|value| value.to_boolean()).map_err(|_| ())
}

fn step(view: &webkit6::WebView, turns: &Cell<u64>, document: &Cell<u64>) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let target = turns.get() + 1;
    let frames = Rc::new(Cell::new(0u64));
    let observed = frames.clone();
    let tick = view.add_tick_callback(move |_, _| {
        observed.set(observed.get() + 1);
        glib::ControlFlow::Continue
    });
    let result = (|| {
        for _ in 0..10_000 {
            if !view.is_mapped() {
                return Ok(());
            }
            let before_uri = view.uri();
            let before_document = document.get();
            let next_frame = frames.get() + 4;
            let evaluated = Rc::new(Cell::new(None));
            let heard = evaluated.clone();
            view.evaluate_javascript(
                &format!("Boolean(window.__eitriTestClock && (window.__eitriTestClock.step(33, {target}), true))"),
                None,
                None,
                None::<&gtk4::gio::Cancellable>,
                move |result| heard.set(Some(evaluation(result))),
            );
            // An old document can disappear before either callback. Retry the same token after four
            // frames or a navigation; the document clock never executes one token twice.
            until(deadline, || {
                turns.get() >= target
                    || frames.get() >= next_frame
                    || view.uri() != before_uri
                    || document.get() != before_document
                    || evaluated.get() == Some(Ok(false))
                    || !view.is_mapped()
            })?;
            if turns.get() >= target || evaluated.get() == Some(Ok(false)) {
                return Ok(());
            }
        }
        Err("the web clock exceeded its frame retries".into())
    })();
    tick.remove();
    result
}

/// Advances live views by 33 ms steps, returning errors safely to callers inside GLib callbacks.
pub fn try_pump(millis: u64) -> Result<(), String> {
    for _ in 0..millis.div_ceil(33).max(1) {
        let views: Vec<_> = VIEWS.with(|views| {
            views.borrow_mut().retain(|(view, _, _)| view.upgrade().is_some());
            views
                .borrow()
                .iter()
                .filter_map(|(view, turns, document)| Some((view.upgrade()?, turns.clone(), document.clone())))
                .collect()
        });
        for (view, turns, document) in views {
            if view.is_mapped() {
                step(&view, &turns, &document)?;
            }
        }
        glib::MainContext::default().iteration(false);
    }
    Ok(())
}

pub fn pump(millis: u64) {
    try_pump(millis).expect("the web clock advances");
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_evaluation_error_is_retried_instead_of_reporting_a_missing_clock() {
        use super::*;

        let error = glib::Error::new(gtk4::gio::IOErrorEnum::Failed, "the document was replaced");
        assert_eq!(evaluation(Err(error)), Err(()));
        let context = webkit6::javascriptcore::Context::new();
        for present in [false, true] {
            let value = webkit6::javascriptcore::Value::new_boolean(&context, present);
            assert_eq!(evaluation(Ok(value)), Ok(present));
        }
    }
}
