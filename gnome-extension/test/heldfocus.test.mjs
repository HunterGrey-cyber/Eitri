import {test} from 'node:test';
import assert from 'node:assert/strict';

import {
    CALLER_FOCUS_MEMORY_MS, FOCUS_RECORDS_TRIM_AT, callerFocusedAt, callerHeldFocus, newHeldFocus, noteHeldFocus,
} from '../policy.js';

const T0 = 10_000_000;
const MIN = 60_000;

const win = (id, pid, wayland = true) => ({id, pid, wayland});
// The snapshot's side of a window: only the three fields the lookup reads.
const live = (id, pid, wayland = true) => ({id, pid, wayland});

// The verdict the policy reaches from the bookkeeping: whether the caller counts as having held focus lately.
function held(state, callerPid, windows, now) {
    return callerHeldFocus({callerFocusedAt64: callerFocusedAt(state, callerPid, windows, now), now64: now});
}

test('a window that has focus counts as now, however long ago it began', () => {
    const state = newHeldFocus();
    noteHeldFocus(state, win(1, 100), T0);
    const later = T0 + 5 * 60 * MIN;
    assert.equal(callerFocusedAt(state, 100, [live(1, 100)], later), later);
    assert.equal(held(state, 100, [live(1, 100)], later), true);
});

test('a window that lost focus counts from the moment it lost it, and only for the memory', () => {
    const state = newHeldFocus();
    noteHeldFocus(state, win(1, 100), T0);
    noteHeldFocus(state, win(2, 200), T0 + 10 * MIN);
    assert.equal(callerFocusedAt(state, 100, [live(1, 100), live(2, 200)], T0 + 20 * MIN), T0 + 10 * MIN);
    assert.equal(held(state, 100, [live(1, 100), live(2, 200)], T0 + 10 * MIN + CALLER_FOCUS_MEMORY_MS), true);
    assert.equal(held(state, 100, [live(1, 100), live(2, 200)], T0 + 10 * MIN + CALLER_FOCUS_MEMORY_MS + 1), false,
        'a record older than the memory is refused');
});

test('a recycled pid with a new window gets no credit from the old window', () => {
    const state = newHeldFocus();
    noteHeldFocus(state, win(1, 100), T0);
    noteHeldFocus(state, win(2, 200), T0 + MIN);
    // Window 1 and its process are gone; pid 100 now belongs to another application whose window 7 never had focus.
    const windows = [live(2, 200), live(7, 100)];
    assert.equal(callerFocusedAt(state, 100, windows, T0 + 2 * MIN), undefined);
    assert.equal(held(state, 100, windows, T0 + 2 * MIN), false);
});

test('a window whose pid changed since it was recorded is not the caller\'s', () => {
    const state = newHeldFocus();
    noteHeldFocus(state, win(1, 100), T0);
    noteHeldFocus(state, null, T0 + MIN);
    assert.equal(callerFocusedAt(state, 100, [live(1, 555)], T0 + 2 * MIN), undefined);
});

test('an X11 window gives no credit, and neither does one with no pid', () => {
    const state = newHeldFocus();
    noteHeldFocus(state, win(1, 100, false), T0);
    assert.equal(state.last, null);
    assert.equal(state.records.size, 0);
    noteHeldFocus(state, win(2, 0), T0 + MIN);
    assert.equal(state.records.size, 0);
    // A Wayland record of a window the snapshot reports as X11 is not used either.
    noteHeldFocus(state, win(3, 100), T0 + 2 * MIN);
    assert.equal(callerFocusedAt(state, 100, [live(3, 100, false)], T0 + 3 * MIN), undefined);
    assert.equal(held(state, 100, [live(1, 100, false)], T0 + 3 * MIN), false);
});

test('a window that no longer exists is not consulted, though its record stays', () => {
    const state = newHeldFocus();
    noteHeldFocus(state, win(1, 100), T0);
    noteHeldFocus(state, win(2, 200), T0 + MIN);
    assert.ok(state.records.has(1));
    assert.equal(callerFocusedAt(state, 100, [live(2, 200)], T0 + 2 * MIN), undefined);
});

test('the newest of the caller\'s windows decides, the focused one counting as now', () => {
    const state = newHeldFocus();
    noteHeldFocus(state, win(1, 100), T0);
    noteHeldFocus(state, win(2, 100), T0 + 3 * MIN);
    noteHeldFocus(state, win(3, 200), T0 + 4 * MIN);
    const windows = [live(1, 100), live(2, 100), live(3, 200)];
    assert.equal(callerFocusedAt(state, 100, windows, T0 + 9 * MIN), T0 + 4 * MIN);
    noteHeldFocus(state, win(1, 100), T0 + 9 * MIN);
    assert.equal(callerFocusedAt(state, 100, windows, T0 + 9 * MIN + 5), T0 + 9 * MIN + 5);
});

test('losing focus to nothing ends the focused window\'s "now"', () => {
    const state = newHeldFocus();
    noteHeldFocus(state, win(1, 100), T0);
    noteHeldFocus(state, null, T0 + MIN);
    assert.equal(callerFocusedAt(state, 100, [live(1, 100)], T0 + 20 * MIN), T0 + MIN);
});

test('past the trim size only records older than the memory go, and the focused window\'s never does', () => {
    const state = newHeldFocus();
    // A window that held focus long ago, then many fresh ones, then the focused one.
    noteHeldFocus(state, win(1, 100), T0);
    noteHeldFocus(state, null, T0);
    const later = T0 + CALLER_FOCUS_MEMORY_MS + MIN;
    for (let id = 2; id <= FOCUS_RECORDS_TRIM_AT; id++) {
        noteHeldFocus(state, win(id, 1000 + id), later);
        noteHeldFocus(state, null, later);
    }
    assert.ok(state.records.has(1), 'at the limit nothing is trimmed yet');
    noteHeldFocus(state, win(500, 5000), later);
    assert.ok(!state.records.has(1), 'the old record went');
    assert.ok(state.records.has(500), 'the focused window stayed');
    assert.equal(state.last, 500);
    assert.equal(state.records.size, FOCUS_RECORDS_TRIM_AT);
    assert.equal(callerFocusedAt(state, 5000, [live(500, 5000)], later), later);

    // With every record fresh, nothing is trimmed even past the size.
    const fresh = newHeldFocus();
    for (let id = 1; id <= FOCUS_RECORDS_TRIM_AT + 10; id++)
        noteHeldFocus(fresh, win(id, id), T0);
    assert.equal(fresh.records.size, FOCUS_RECORDS_TRIM_AT + 10);
});

// What the docs say plainly: the extension notes its own activations like any other focus gain, so the credit is a
// recent-focus test, not a "the user was in it" test. A caller that is granted focus once every half hour never loses it.
test('a focus the caller was granted renews the memory, so regular grants keep the credit indefinitely', () => {
    const state = newHeldFocus();
    let now = T0;
    noteHeldFocus(state, win(1, 100), now);
    for (let round = 0; round < 10; round++) {
        now += 20 * MIN;
        noteHeldFocus(state, win(2, 200), now - 10 * MIN);
        assert.equal(held(state, 100, [live(1, 100), live(2, 200)], now), true, `round ${round}`);
        noteHeldFocus(state, win(1, 100), now);
    }
    // Left alone for longer than the memory, the credit is gone.
    noteHeldFocus(state, win(2, 200), now);
    assert.equal(held(state, 100, [live(1, 100), live(2, 200)], now + CALLER_FOCUS_MEMORY_MS + 1), false);
});

test('a new record, as after the extension was disabled, gives nobody credit until they have focus again', () => {
    const state = newHeldFocus();
    noteHeldFocus(state, win(1, 100), T0);
    const afterUnlock = newHeldFocus();
    assert.equal(held(afterUnlock, 100, [live(1, 100)], T0 + MIN), false);
    noteHeldFocus(afterUnlock, win(1, 100), T0 + 2 * MIN);
    assert.equal(held(afterUnlock, 100, [live(1, 100)], T0 + 3 * MIN), true);
});
