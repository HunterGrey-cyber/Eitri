//! The browser fixture's clock runs without a display or a browser.

#[allow(dead_code)]
#[path = "support/web_clock.rs"]
mod web_clock;

mod node_cache {
    include!("../node_cache.rs");
}

#[test]
fn clock_turns_keep_timer_budgets_cancellation_and_frame_order() {
    let mut node = node_cache::command("node", std::path::Path::new(env!("OUT_DIR")));
    let program = r#"
const vm = require('node:vm');
const assert = require('node:assert/strict');
const nativeFrames = [], acknowledgements = [];
const context = vm.createContext({
  performance: { now: () => 0 }, Event: class Event {},
  requestAnimationFrame: fn => nativeFrames.push(fn),
  webkit: { messageHandlers: { eitriTestClock: { postMessage: value => acknowledgements.push(value) } } }
});
context.window = context;
vm.runInContext(CLOCK, context);
vm.runInContext(`
  globalThis.heard = [];
  const cancelled = setTimeout(() => heard.push('cancelled'), 1);
  clearTimeout(cancelled);
  setTimeout(() => heard.push('timer'), 34);
  globalThis.repeats = 0;
  globalThis.interval = setInterval(() => repeats++, 10);
  requestAnimationFrame(() => {
    heard.push('frame');
    cancelAnimationFrame(other);
    requestAnimationFrame(() => heard.push('next-frame'));
  });
  const other = requestAnimationFrame(() => heard.push('cancelled-frame'));
`, context);
function step(delta) {
  vm.runInContext(`__eitriTestClock.step(${delta})`, context);
  assert.equal(nativeFrames.length, 1);
  nativeFrames.shift()();
  assert.equal(nativeFrames.length, 1);
  nativeFrames.shift()();
}
assert.equal(context.repeats, 0);
step(33);
assert.deepEqual(Array.from(context.heard), ['frame']);
assert.equal(context.repeats, 3);
assert.equal(vm.runInContext('performance.now()', context), 33);
step(1);
assert.deepEqual(Array.from(context.heard), ['frame', 'timer', 'next-frame']);
vm.runInContext('clearInterval(interval)', context);
step(100);
assert.equal(context.repeats, 3);
assert.equal(acknowledgements.length, 3);
// Retries during a pending frame and after its acknowledgement never advance the clock twice.
vm.runInContext('__eitriTestClock.step(33, 4); __eitriTestClock.step(33, 4)', context);
assert.equal(nativeFrames.length, 1);
nativeFrames.shift()();
vm.runInContext('__eitriTestClock.step(33, 4)', context);
assert.equal(nativeFrames.length, 1);
nativeFrames.shift()();
const completedNow = vm.runInContext('performance.now()', context);
vm.runInContext('__eitriTestClock.step(33, 4)', context);
assert.equal(nativeFrames.length, 0);
assert.equal(vm.runInContext('performance.now()', context), completedNow);
assert.equal(acknowledgements.at(-1), '4');
// A navigation can discard the old document's frame. The same token runs in the new document.
vm.runInContext('__eitriTestClock.step(33, 5)', context);
nativeFrames.length = 0;
const next = vm.createContext({
  performance: { now: () => 0 }, Event: class Event {},
  requestAnimationFrame: fn => nativeFrames.push(fn),
  webkit: { messageHandlers: { eitriTestClock: { postMessage: value => acknowledgements.push(value) } } }
});
next.window = next;
vm.runInContext(CLOCK, next);
vm.runInContext('__eitriTestClock.step(33, 5)', next);
nativeFrames.shift()();
nativeFrames.shift()();
assert.equal(acknowledgements.at(-1), '5');
assert.equal(vm.runInContext('performance.now()', next), 33);
"#;
    let program = format!(
        "const CLOCK = {};\n{program}",
        serde_json::to_string(include_str!("support/web_clock.js")).unwrap()
    );
    let output = node
        .args(["-e", &program])
        .output()
        .expect("node runs the clock fixture");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[path = "support/source_scope.rs"]
mod source_scope;

#[test]
fn a_replay_that_never_gets_ready_leaves_no_source_after_timeout() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let context = glib::MainContext::new();
    let _owner = context.acquire().unwrap();
    let dispatched = Arc::new(AtomicUsize::new(0));
    let counted = dispatched.clone();
    let replay = glib::idle_source_new(None, glib::Priority::DEFAULT_IDLE, move || {
        counted.fetch_add(1, Ordering::SeqCst);
        glib::ControlFlow::Continue
    });
    let id = replay.attach(Some(&context));
    let sources = source_scope::SourceScope::new(context.clone(), vec![id]);
    context.iteration(false);
    assert_eq!(
        dispatched.load(Ordering::SeqCst),
        1,
        "the waiting replay was dispatched"
    );
    // The run exits on its deadline without ever receiving Ready.
    drop(sources);
    for _ in 0..20 {
        context.iteration(false);
    }
    assert_eq!(
        dispatched.load(Ordering::SeqCst),
        1,
        "cleanup must not dispatch the waiting replay again"
    );
    assert!(!context.pending(), "cleanup has no always-ready source left");
}
