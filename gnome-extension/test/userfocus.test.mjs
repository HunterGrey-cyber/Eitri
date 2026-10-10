import {test} from 'node:test';
import assert from 'node:assert/strict';

import {
    CALLER_FOCUS_MEMORY_MS, callerFocusedAt, decide, focusGainCredited, newHeldFocus, noteHeldFocus,
} from '../policy.js';

// The 30-minute memory over a whole session, the way extension.js keeps it: every focus change goes through
// noteHeldFocus, credited as focusGainCredited says. A panel P (pid 100, window 1) on the right, a second window of the
// panel's process P2 (pid 100, window 3), its editor E (pid 200, window 2) on the left. Times are milliseconds on the
// extension's monotonic clock.
const T0 = 50_000_000;
const SEC = 1_000;
const MIN = 60 * SEC;
const P = {id: 1, pid: 100, wayland: true};
const E = {id: 2, pid: 200, wayland: true};
const P2 = {id: 3, pid: 100, wayland: true};

function windows(now) {
    const base = {
        wayland: true, minimized: false, type: 'normal', skipTaskbar: false, onWorkspace: true, monitor: 0,
        // A key in the editor 100 ms ago.
        userTime: now - 100,
    };
    return [
        {...base, id: 2, pid: 200, rect: {x: 0, y: 0, w: 960, h: 1080}, stack: 0, mru: 0},
        {...base, id: 1, pid: 100, rect: {x: 960, y: 0, w: 960, h: 1080}, stack: 1, mru: 1},
        {...base, id: 3, pid: 100, rect: {x: 1200, y: 100, w: 400, h: 300}, stack: 2, mru: 2},
    ];
}

// When the extension last activated a window, as extension.js keeps it.
let lastOwnActivation = null;
function gain(held, w, now, {userInput, gainedByUs}) {
    const credited = focusGainCredited({userInput, atEnable: false, gainedByUs, now64: now,
        lastOwnActivation64: lastOwnActivation});
    noteHeldFocus(held, w, now, credited);
}
// A click puts focus in `w`.
const userGain = (held, w, now) => gain(held, w, now, {userInput: true, gainedByUs: false});
// The extension's own activation puts focus in `w` (FocusDirection, FocusSelfIfNeighbour, ActivateOwn, ...).
function ourGain(held, w, now) {
    lastOwnActivation = now;
    gain(held, w, now, {userInput: false, gainedByUs: true});
}
// The application moves focus itself (a new window, a dialog, an activation request): no input event is being handled.
const clientGain = (held, w, now) => gain(held, w, now, {userInput: false, gainedByUs: false});

// The panel asks to come forward while the user has just pressed a key in the editor, which has focus.
function panelAsks(held, now, method = 'FocusSelfIfNeighbour') {
    const all = windows(now);
    return decide({
        method, direction: 'right', callerPid: 100,
        callerFocusedAt64: callerFocusedAt(held, 100, all, now),
        partnerChain: [200], focusedId: 2, focusGained64: now - 10 * SEC, focusGainedByUs: false, now64: now,
        idleMs: 0, modal: false, windows: all,
    });
}

for (const method of ['FocusSelfIfNeighbour', 'ActivateOwn']) {
    test(`${method}: after a grant, focus the caller moves to another of its windows earns nothing either`, () => {
        lastOwnActivation = null;
        const held = newHeldFocus();
        userGain(held, P, T0);
        ourGain(held, E, T0 + MIN);
        let now = T0 + 20 * MIN;
        assert.equal(panelAsks(held, now, method).act, true);
        ourGain(held, P, now);
        // Two seconds later, past the shadow of the activation, the panel's process brings up a window of its own.
        clientGain(held, P2, now + 2 * SEC);
        assert.equal(callerFocusedAt(held, 100, windows(now + 5 * MIN), now + 5 * MIN), T0 + MIN);
        ourGain(held, E, now + 5 * MIN);
        now = T0 + MIN + CALLER_FOCUS_MEMORY_MS + 1;
        assert.equal(panelAsks(held, now, method).reason, 'caller-not-focused-lately');
    });

    test(`${method}: a focus the extension gave does not renew the memory, so grants cannot chain`, () => {
        lastOwnActivation = null;
        const held = newHeldFocus();
        userGain(held, P, T0); // the user clicks the panel
        ourGain(held, E, T0 + MIN); // Ctrl+h
        // Within the memory of the click the panel may come forward on the editor's key...
        let now = T0 + 20 * MIN;
        assert.equal(panelAsks(held, now, method).act, true);
        ourGain(held, P, now);
        // ...and holding that focus is not holding one the user gave: it is not "now".
        assert.equal(callerFocusedAt(held, 100, windows(now + 5 * MIN), now + 5 * MIN), T0 + MIN);
        ourGain(held, E, now + 5 * MIN); // Ctrl+h again
        // The grant at +20 min and the stay in the panel bought nothing: the memory runs from the click's focus.
        now = T0 + MIN + CALLER_FOCUS_MEMORY_MS;
        assert.equal(panelAsks(held, now, method).act, true, 'exactly at the memory');
        const refused = panelAsks(held, now + 1, method);
        assert.equal(refused.act, false);
        assert.equal(refused.reason, 'caller-not-focused-lately');
    });

    test(`${method}: a window only ever focused by the extension has no credit`, () => {
        const held = newHeldFocus();
        ourGain(held, P, T0);
        assert.equal(callerFocusedAt(held, 100, windows(T0 + SEC), T0 + SEC), undefined, 'while it has focus');
        ourGain(held, E, T0 + SEC);
        assert.equal(panelAsks(held, T0 + 2 * SEC, method).reason, 'caller-not-focused-lately');
    });
}

test('a window that opened with focus earns nothing; a click on it does', () => {
    lastOwnActivation = null;
    const held = newHeldFocus();
    clientGain(held, P, T0);
    ourGain(held, E, T0 + 5 * SEC);
    assert.equal(panelAsks(held, T0 + 10 * SEC).reason, 'caller-not-focused-lately');
    userGain(held, P, T0 + 20 * SEC);
    ourGain(held, E, T0 + 25 * SEC);
    assert.equal(panelAsks(held, T0 + 30 * SEC).act, true);
});

test('a click within a second of the extension\'s activation earns nothing; the next one does', () => {
    lastOwnActivation = null;
    const held = newHeldFocus();
    ourGain(held, E, T0);
    userGain(held, P, T0 + 500);
    ourGain(held, E, T0 + 5 * SEC);
    assert.equal(panelAsks(held, T0 + 10 * SEC).reason, 'caller-not-focused-lately');
    userGain(held, P, T0 + 20 * SEC);
    ourGain(held, E, T0 + 25 * SEC);
    assert.equal(panelAsks(held, T0 + 30 * SEC).act, true);
});

test('a click on the panel restores what the memory ran out on', () => {
    lastOwnActivation = null;
    const held = newHeldFocus();
    userGain(held, P, T0);
    ourGain(held, E, T0 + MIN);
    let now = T0 + MIN + CALLER_FOCUS_MEMORY_MS + 1;
    assert.equal(panelAsks(held, now).reason, 'caller-not-focused-lately');
    userGain(held, P, now + 2 * SEC); // the click
    ourGain(held, E, now + 3 * SEC); // Ctrl+h
    now += 4 * SEC;
    assert.equal(panelAsks(held, now).act, true);
    assert.equal(panelAsks(held, now, 'ActivateOwn').act, true);
});

test('a session driven only by keys keeps working for 30 minutes after the last click, then stops', () => {
    lastOwnActivation = null;
    const held = newHeldFocus();
    userGain(held, P, T0);
    // The user stays in the panel for 2 minutes, then goes back and forth with Ctrl+h/Ctrl+l every 30 s.
    const lastClickFocusEnds = T0 + 2 * MIN;
    ourGain(held, E, lastClickFocusEnds);
    let now = lastClickFocusEnds;
    let granted = 0;
    while (now + 40 * SEC < lastClickFocusEnds + CALLER_FOCUS_MEMORY_MS) {
        now += 30 * SEC;
        assert.equal(panelAsks(held, now).act, true, `at +${(now - T0) / MIN} min`);
        granted += 1;
        ourGain(held, P, now); // Ctrl+l lands in the panel
        ourGain(held, E, now + 10 * SEC); // Ctrl+h
    }
    assert.equal(granted, 59);
    assert.equal(panelAsks(held, lastClickFocusEnds + CALLER_FOCUS_MEMORY_MS).act, true, 'exactly at the memory');
    const refused = panelAsks(held, lastClickFocusEnds + CALLER_FOCUS_MEMORY_MS + 1);
    assert.equal(refused.reason, 'caller-not-focused-lately');
});

test('the panel focused by the user still counts as now, however long it keeps that focus', () => {
    lastOwnActivation = null;
    const held = newHeldFocus();
    userGain(held, P, T0);
    const later = T0 + 3 * CALLER_FOCUS_MEMORY_MS;
    assert.equal(callerFocusedAt(held, 100, windows(later), later), later);
});
