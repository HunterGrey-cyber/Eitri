import {test} from 'node:test';
import assert from 'node:assert/strict';

import {
    FRESH_INPUT_MS, MAX_PARTNER_PIDS, METHODS, cleanPartnerList, decide, isFreshInput, mayActOnPartnerInput,
    mayFocusDirection, partnerPid, MAX_FOCUS_AGE_MS, SETTLE_MS} from '../policy.js';

// Times are milliseconds. `now64` and `focusGained64` are on the extension's own 64-bit monotonic clock;
// `userTime` is mutter's 32-bit timestamp, the same clock cut to its low 32 bits.
const NOW = 55_320_000;
const DAY = 86_400_000;
const TOP = 2 ** 32;

// `idleMs` is how long ago the last real device event was. Most cases here put one at this very millisecond, so that
// they test the user time alone; the cases for the idle reading itself set it.
function fresh(overrides = {}) {
    return isFreshInput({wayland: true, userTime: NOW - 100, focusGained64: NOW - 5_000, now64: NOW, idleMs: 0,
        focusGainedByUs: false, ...overrides});
}

test('fresh input: a Wayland window pressed after it gained focus, at most 500 ms ago', () => {
    assert.equal(FRESH_INPUT_MS, 500);
    assert.equal(fresh(), true);
    assert.equal(fresh({userTime: NOW}), true, 'pressed this very millisecond');
    assert.equal(fresh({userTime: NOW - 500}), true, 'exactly 500 ms ago');
    assert.equal(fresh({userTime: NOW - 501}), false, '501 ms ago');
    assert.equal(fresh({userTime: NOW - 600}), false);
    assert.equal(fresh({userTime: NOW - 3_600_000}), false, 'an hour ago');
});

test('fresh input: a timestamp in the future is never fresh', () => {
    assert.equal(fresh({userTime: NOW + 1}), false);
    assert.equal(fresh({userTime: NOW + 5_000}), false);
    assert.equal(fresh({userTime: NOW + 2 ** 31}), false);
});

test('fresh input: only strictly after the window last gained focus', () => {
    assert.equal(fresh({userTime: NOW - 100, focusGained64: NOW - 101}), true);
    // The click that focuses a window carries the same time as, or a time just before, the focus change.
    assert.equal(fresh({userTime: NOW - 100, focusGained64: NOW - 100}), false);
    assert.equal(fresh({userTime: NOW - 100, focusGained64: NOW - 97}), false);
    // Input from before focus went away and came back.
    assert.equal(fresh({userTime: NOW - 300, focusGained64: NOW - 10}), false);
    assert.equal(fresh({userTime: NOW, focusGained64: NOW}), false, 'both this millisecond');
});

test('fresh input: a focus gain after now is never before the input, however far ahead', () => {
    assert.equal(isFreshInput({wayland: true, userTime: 99_900, focusGained64: 160_001, now64: 100_000, idleMs: 0}),
        false);
    for (const ahead of [1, 3, 60_000, 60_001, 2 ** 31, TOP - 1, TOP, TOP + 1, 30 * DAY])
        assert.equal(fresh({focusGained64: NOW + ahead}), false, `${ahead} ms ahead`);
});

test('fresh input: a window focused for weeks still counts, with no edge at 24.9 days', () => {
    // Sixty days after boot.
    const now64 = 60 * DAY + 12_345;
    const key = {wayland: true, userTime: (now64 - 100) % TOP, now64, idleMs: 0};
    for (const [label, age] of [['25 days', 25 * DAY], ['2^31 ms', 2 ** 31], ['2^31 + 1 ms', 2 ** 31 + 1],
        ['49 days', 49 * DAY], ['the longest focus that is still judged', MAX_FOCUS_AGE_MS - 1]])
        assert.equal(isFreshInput({...key, focusGained64: now64 - age}), true, label);
    // And the same windows still refuse stale input, or input from before a recent focus gain.
    assert.equal(isFreshInput({...key, userTime: (now64 - 501) % TOP, focusGained64: now64 - 25 * DAY}), false);
    assert.equal(isFreshInput({...key, focusGained64: now64 - 50}), false);
    assert.equal(isFreshInput({...key, focusGained64: now64 - 100}), false);
});

test('fresh input: a focus older than one period of the 32-bit clock counts nothing', () => {
    // The extension activated the window at 100000 (its user time is that stamp) and nothing was typed since. One whole
    // period later the stamp reads as 100 ms old: it must not count.
    const gained = 100_000;
    assert.equal(isFreshInput({wayland: true, userTime: gained, focusGained64: gained, now64: TOP + gained + 100,
        idleMs: 0}), false);
    const now64 = 60 * DAY + 12_345;
    const key = {wayland: true, userTime: (now64 - 100) % TOP, now64, idleMs: 0};
    for (const [label, age] of [['MAX_FOCUS_AGE_MS', MAX_FOCUS_AGE_MS], ['2^32 - 50 ms', TOP - 50], ['2^32 ms', TOP],
        ['2^32 + 50 ms', TOP + 50], ['2^32 + 10 s', TOP + 10_000], ['59 days', 59 * DAY]])
        assert.equal(isFreshInput({...key, focusGained64: now64 - age}), false, label);
});

test('fresh input: an X11 window never counts, nor does missing or malformed data', () => {
    assert.equal(fresh({wayland: false}), false);
    assert.equal(fresh({wayland: undefined}), false);
    assert.equal(fresh({wayland: 1}), false);
    assert.equal(fresh({focusGained64: undefined}), false, 'focus gain never recorded');
    assert.equal(fresh({focusGained64: 0}), false);
    assert.equal(fresh({focusGained64: NOW - 100.5}), false);
    assert.equal(fresh({focusGained64: -1}), false);
    assert.equal(fresh({focusGained64: String(NOW - 5_000)}), false);
    assert.equal(fresh({userTime: 0}), false, 'no user time at all');
    assert.equal(fresh({userTime: NOW - 100.5}), false);
    assert.equal(fresh({userTime: -1}), false);
    assert.equal(fresh({userTime: 2 ** 32}), false);
    assert.equal(fresh({now64: undefined}), false);
    assert.equal(fresh({now64: 0}), false);
    assert.equal(fresh({now64: 2 ** 53}), false, 'not a safe integer');
    assert.equal(fresh({userTime: '55319900'}), false);
});

test('fresh input across the wrap of the 32-bit millisecond clock', () => {
    // The 32-bit clock wrapped 50 ms ago: its value is now 50.
    const now64 = TOP + 50;
    // The press was 150 ms ago, the focus gain a second before it.
    assert.equal(isFreshInput({wayland: true, userTime: TOP - 100, focusGained64: TOP - 1_100, now64, idleMs: 0}),
        true);
    // The press after the wrap, the focus gain before it.
    assert.equal(isFreshInput({wayland: true, userTime: 20, focusGained64: TOP - 1_000, now64, idleMs: 0}), true);
    // Focus gained after the press, across the wrap.
    assert.equal(isFreshInput({wayland: true, userTime: TOP - 100, focusGained64: TOP + 10, now64, idleMs: 0}), false);
    // A press just "after" now across the wrap is in the future.
    assert.equal(isFreshInput({wayland: true, userTime: 60, focusGained64: TOP - 1_000, now64, idleMs: 0}), false);
    // Too old across the wrap.
    assert.equal(isFreshInput({wayland: true, userTime: TOP - 600, focusGained64: TOP - 5_000, now64, idleMs: 0}),
        false);
});

test('the partner list: drops 0, 1, the shell and the caller; keeps order; no repeats', () => {
    const ctx = {shellPid: 1500, callerPid: 4242};
    assert.deepEqual(cleanPartnerList([0, 1, 300, 1500, 4242, 200, 300, 100], ctx), [300, 200, 100]);
    assert.deepEqual(cleanPartnerList([], ctx), []);
    assert.deepEqual(cleanPartnerList([0, 1, 1500, 4242], ctx), []);
    assert.deepEqual(cleanPartnerList([-5, 2.5, NaN, 2 ** 32, '7', null, 7], ctx), [7]);
    assert.deepEqual(cleanPartnerList(undefined, ctx), []);
    assert.deepEqual(cleanPartnerList('1,2,3', ctx), []);
});

test('the partner list is capped at 64, keeping the first 64', () => {
    assert.equal(MAX_PARTNER_PIDS, 64);
    const pids = Array.from({length: 65}, (_, i) => 1000 + i);
    const kept = cleanPartnerList(pids, {shellPid: 2, callerPid: 3});
    assert.equal(kept.length, 64);
    assert.deepEqual(kept, pids.slice(0, 64));
    // Dropped entries do not use up the cap.
    const withJunk = [0, 1, 2, 3, ...pids];
    assert.deepEqual(cleanPartnerList(withJunk, {shellPid: 2, callerPid: 3}), pids.slice(0, 64));
});

test('the partner is the first pid in the chain that owns a window', () => {
    const owners = new Set([30, 40]);
    const owns = pid => owners.has(pid);
    assert.equal(partnerPid([10, 20, 30, 40], owns), 30);
    assert.equal(partnerPid([40, 30], owns), 40);
    assert.equal(partnerPid([10, 20], owns), null);
    assert.equal(partnerPid([], owns), null);
    assert.equal(partnerPid(null, owns), null);
    assert.equal(partnerPid([10, 30], pid => (pid === 30 ? 'yes' : false)), null, 'only a true answer counts');
});

test('mayFocusDirection: the caller focused, with fresh input', () => {
    assert.equal(mayFocusDirection({focusedPid: 7, callerPid: 7, freshInput: true}), true);
    assert.equal(mayFocusDirection({focusedPid: 8, callerPid: 7, freshInput: true}), false);
    assert.equal(mayFocusDirection({focusedPid: 7, callerPid: 7, freshInput: false}), false);
    assert.equal(mayFocusDirection({focusedPid: 0, callerPid: 0, freshInput: true}), false);
    assert.equal(mayFocusDirection({focusedPid: undefined, callerPid: undefined, freshInput: true}), false);
});

test('mayActOnPartnerInput: the partner focused, with fresh input', () => {
    assert.equal(mayActOnPartnerInput({focusedPid: 9, partnerPid: 9, freshInput: true}), true);
    assert.equal(mayActOnPartnerInput({focusedPid: 9, partnerPid: 10, freshInput: true}), false);
    assert.equal(mayActOnPartnerInput({focusedPid: 9, partnerPid: 9, freshInput: false}), false);
    assert.equal(mayActOnPartnerInput({focusedPid: null, partnerPid: null, freshInput: true}), false);
});

// A panel A (pid 100) on the right, its editor B (pid 200, a terminal) on the left, and an unrelated window C
// (pid 300) on another workspace. Ids are window ids, as the shell would give them.
function scene(overrides = {}) {
    const base = {
        wayland: true, userTime: NOW - 5_000, minimized: false, type: 'normal', skipTaskbar: false,
        onWorkspace: true, monitor: 0,
    };
    const windows = [
        {...base, id: 2, pid: 200, rect: {x: 0, y: 0, w: 960, h: 1080}, stack: 0, mru: 1},
        {...base, id: 1, pid: 100, rect: {x: 960, y: 0, w: 960, h: 1080}, stack: 1, mru: 0},
        {...base, id: 3, pid: 300, rect: {x: 0, y: 0, w: 800, h: 600}, stack: 2, mru: 2, onWorkspace: false},
    ];
    for (const [id, patch] of Object.entries(overrides.patch ?? {}))
        Object.assign(windows.find(w => w.id === Number(id)), patch);
    return {
        method: 'FocusDirection', direction: 'left', callerPid: 100, partnerChain: [200], focusedId: 1,
        focusGained64: NOW - 10_000, focusGainedByUs: false, now64: NOW, idleMs: 0, modal: false,
        windows: [...windows, ...(overrides.extra ?? [])],
        ...overrides.call,
    };
}

// The window had a key press `age` ms ago.
function pressed(id, age = 100) {
    return {[id]: {userTime: NOW - age}};
}

test('decide: unknown methods and bad directions are refused', () => {
    assert.deepEqual([...METHODS], ['FocusDirection', 'FocusSelfIfNeighbour', 'ActivateOwn', 'ActivatePartner']);
    assert.equal(decide(scene({call: {method: 'SetPartner'}, patch: pressed(1)})).reason, 'unknown-method');
    for (const direction of ['', 'west', 'Left', undefined]) {
        assert.equal(decide(scene({call: {direction}, patch: pressed(1)})).reason, 'bad-direction');
        assert.equal(decide(scene({call: {method: 'FocusSelfIfNeighbour', direction, focusedId: 2},
            patch: pressed(2)})).reason, 'bad-direction');
    }
});

test('decide FocusDirection: from the caller\'s own focused window, after a fresh key', () => {
    assert.deepEqual(decide(scene({patch: pressed(1)})), {act: true, target: 2});
    assert.equal(decide(scene({patch: pressed(1), call: {direction: 'right'}})).reason, 'nothing-there');
    // Any window may be the target, partner or not.
    assert.deepEqual(decide(scene({patch: pressed(1), call: {partnerChain: []}})), {act: true, target: 2});
});

test('decide FocusDirection: refused unless the caller is focused', () => {
    assert.equal(decide(scene({patch: pressed(1), call: {callerPid: 999}})).reason, 'focus-not-caller');
    assert.equal(decide(scene({patch: pressed(2), call: {focusedId: 2}})).reason, 'focus-not-caller');
    assert.equal(decide(scene({patch: pressed(1), call: {focusedId: null}})).reason, 'no-focus');
    assert.equal(decide(scene({patch: pressed(1), call: {focusedId: 77}})).reason, 'no-focus');
});

test('decide FocusDirection: refused without fresh input in the caller\'s window', () => {
    assert.equal(decide(scene({patch: pressed(1, 600)})).reason, 'no-fresh-input');
    assert.equal(decide(scene()).reason, 'no-fresh-input', 'no key for five seconds');
    assert.equal(decide(scene({patch: {1: {userTime: NOW + 50}}})).reason, 'no-fresh-input', 'future');
    assert.equal(decide(scene({patch: {1: {userTime: NOW - 100, wayland: false}}})).reason, 'no-fresh-input', 'X11');
    assert.equal(decide(scene({patch: pressed(1), call: {focusGained64: NOW - 50}})).reason, 'no-fresh-input',
        'the key came before the window last gained focus');
    assert.equal(decide(scene({patch: pressed(1), call: {focusGained64: undefined}})).reason, 'no-fresh-input');
});

test('decide: nothing acts while the shell holds a modal grab, or with no known caller', () => {
    for (const method of METHODS) {
        assert.equal(decide(scene({patch: pressed(1), call: {method, modal: true}})).reason, 'shell-modal', method);
        assert.equal(decide(scene({patch: pressed(1), call: {method, modal: undefined}})).reason, 'shell-modal');
        assert.equal(decide(scene({patch: pressed(1), call: {method, callerPid: 0}})).reason, 'no-caller');
    }
});

test('decide ActivatePartner: from the caller\'s focused window, after a fresh key, to the partner', () => {
    assert.deepEqual(decide(scene({patch: pressed(1), call: {method: 'ActivatePartner'}})), {act: true, target: 2});
    // The partner's window on another workspace is still activated.
    assert.deepEqual(decide(scene({patch: pressed(1), call: {method: 'ActivatePartner', partnerChain: [300]}})),
        {act: true, target: 3});
    // The partner's most recently used window, when it has several.
    const second = {id: 4, pid: 200, wayland: true, userTime: NOW - 9_000, rect: {x: 0, y: 0, w: 10, h: 10},
        minimized: true, type: 'normal', skipTaskbar: false, onWorkspace: false, monitor: 0, stack: 3, mru: 0};
    assert.deepEqual(decide(scene({patch: {1: {userTime: NOW - 100, mru: 1}, 2: {mru: 2}},
        extra: [second], call: {method: 'ActivatePartner'}})), {act: true, target: 4});
});

test('decide ActivatePartner: refused without a partner, a focused caller, or fresh input', () => {
    assert.equal(decide(scene({patch: pressed(1), call: {method: 'ActivatePartner', partnerChain: []}})).reason,
        'no-partner');
    assert.equal(decide(scene({patch: pressed(1), call: {method: 'ActivatePartner', partnerChain: [555]}})).reason,
        'no-partner', 'a pid with no window');
    assert.equal(decide(scene({patch: pressed(2), call: {method: 'ActivatePartner', focusedId: 2}})).reason,
        'focus-not-caller');
    assert.equal(decide(scene({patch: pressed(1, 600), call: {method: 'ActivatePartner'}})).reason,
        'no-fresh-input');
    // The caller's own pid in the chain is never its partner.
    assert.equal(decide(scene({patch: pressed(1), call: {method: 'ActivatePartner', partnerChain: [100]}})).reason,
        'no-partner');
});

test('decide: the partner is the first pid in the chain that owns a window in the tab list', () => {
    // nvim (pid 210) has no window; its parent, the terminal (pid 200), does.
    assert.deepEqual(decide(scene({patch: pressed(1), call: {method: 'ActivatePartner', partnerChain: [210, 200]}})),
        {act: true, target: 2});
    // A window of another kind (a utility window, a tooltip) makes no pid a partner, even with a place in the shell's
    // most-recently-used list.
    assert.equal(decide(scene({patch: {...pressed(1), 2: {type: 'other'}},
        call: {method: 'ActivatePartner'}})).reason, 'no-partner');
    assert.equal(decide(scene({patch: {...pressed(1), 2: {skipTaskbar: true}},
        call: {method: 'ActivatePartner'}})).reason, 'no-partner');
    // With both owning windows, the first in the chain wins.
    assert.deepEqual(decide(scene({patch: pressed(1), call: {method: 'ActivatePartner', partnerChain: [300, 200]}})),
        {act: true, target: 3});
});

test('decide FocusSelfIfNeighbour: partner focused, fresh key, the caller beside it in that direction', () => {
    const call = {method: 'FocusSelfIfNeighbour', direction: 'right', focusedId: 2};
    assert.deepEqual(decide(scene({patch: pressed(2), call})), {act: true, target: 1});
    assert.equal(decide(scene({patch: pressed(2), call: {...call, direction: 'left'}})).reason, 'nothing-there');
    assert.equal(decide(scene({patch: pressed(2), call: {...call, direction: 'up'}})).reason, 'nothing-there');
});

test('decide FocusSelfIfNeighbour: refused when another window is the neighbour', () => {
    const between = {id: 5, pid: 500, wayland: true, userTime: NOW - 9_000, rect: {x: 960, y: 0, w: 400, h: 1080},
        minimized: false, type: 'normal', skipTaskbar: false, onWorkspace: true, monitor: 0, stack: 5, mru: 3};
    const call = {method: 'FocusSelfIfNeighbour', direction: 'right', focusedId: 2};
    assert.equal(decide(scene({patch: {...pressed(2), 1: {rect: {x: 1400, y: 0, w: 520, h: 1080}}},
        extra: [between], call})).reason, 'neighbour-not-caller');
});

test('decide FocusSelfIfNeighbour: refused without a partner, a focused partner, or fresh input', () => {
    const call = {method: 'FocusSelfIfNeighbour', direction: 'right', focusedId: 2};
    assert.equal(decide(scene({patch: pressed(2), call: {...call, partnerChain: []}})).reason, 'no-partner');
    assert.equal(decide(scene({patch: pressed(1), call: {...call, focusedId: 1}})).reason, 'focus-not-partner');
    assert.equal(decide(scene({patch: pressed(2, 600), call})).reason, 'no-fresh-input');
    assert.equal(decide(scene({patch: {2: {userTime: NOW - 100, wayland: false}}, call})).reason, 'no-fresh-input',
        'an Xwayland editor');
    assert.equal(decide(scene({patch: pressed(2), call: {...call, focusGained64: NOW}})).reason, 'no-fresh-input');
    // A partner set to some other pid while the user types in B gets nothing either.
    assert.equal(decide(scene({patch: {...pressed(2), 3: {onWorkspace: true}}, call: {...call, partnerChain: [300]}}))
        .reason, 'focus-not-partner');
});

test('decide ActivateOwn: partner focused with a fresh key brings the caller\'s window forward', () => {
    const call = {method: 'ActivateOwn', focusedId: 2};
    assert.deepEqual(decide(scene({patch: pressed(2), call})), {act: true, target: 1});
    // Wherever the caller's window is: no direction applies.
    assert.deepEqual(decide(scene({patch: {...pressed(2), 1: {onWorkspace: false, minimized: true}}, call})),
        {act: true, target: 1});
});

test('decide ActivateOwn: refused without a partner, a focused partner, fresh input, or a window', () => {
    const call = {method: 'ActivateOwn', focusedId: 2};
    assert.equal(decide(scene({patch: pressed(2), call: {...call, partnerChain: []}})).reason, 'no-partner');
    assert.equal(decide(scene({patch: pressed(1), call: {...call, focusedId: 1}})).reason, 'focus-not-partner');
    assert.equal(decide(scene({patch: pressed(2, 501), call})).reason, 'no-fresh-input');
    assert.equal(decide(scene({patch: pressed(2), call: {...call, callerPid: 101}})).reason, 'no-own-window');
});

// The shell's own activation sets the activated window's user time to the activation's timestamp; the extension
// records that same timestamp as the window's focus gain.
function afterActivation(windows, id, t) {
    return windows.map(w => (w.id === id ? {...w, userTime: t} : w));
}

test('decide: no chain of calls moves focus on without a new key (three windows)', () => {
    // B is the partner; the user types in B; A brings itself forward.
    const c = {onWorkspace: true, rect: {x: 0, y: 600, w: 960, h: 480}};
    const s = scene({patch: {...pressed(2), 3: c}, call: {method: 'ActivateOwn', focusedId: 2}});
    assert.deepEqual(decide(s), {act: true, target: 1});
    const t = NOW + 3;
    // A is now focused; its user time is the activation's, and so is its recorded focus gain. A names C as partner.
    const after = {...s, windows: afterActivation(s.windows, 1, t), focusedId: 1, focusGained64: t,
        focusGainedByUs: true, focusGainedFrom: 200, now64: t + 10, partnerChain: [300]};
    assert.equal(decide({...after, method: 'ActivatePartner'}).reason, 'no-fresh-input');
    assert.equal(decide({...after, method: 'FocusDirection', direction: 'left'}).reason, 'no-fresh-input');
    // A moves its own user time to "now" by asking the shell to activate its already-focused window; the last real
    // input was still the key in B, before A gained focus.
    const bumped = {...after, windows: afterActivation(after.windows, 1, t + 8), idleMs: (t + 10) - (NOW - 100)};
    assert.equal(decide({...bumped, method: 'ActivatePartner'}).reason, 'no-fresh-input');
    assert.equal(decide({...bumped, method: 'FocusDirection', direction: 'left'}).reason, 'no-fresh-input');
    // The release of the key the user pressed in B lands in A 80 ms after the activation, and A moves its user time.
    const released = {...after, windows: afterActivation(after.windows, 1, t + 85), now64: t + 90, idleMs: 10};
    assert.equal(decide({...released, method: 'ActivatePartner'}).reason, 'no-fresh-input');
    // Input in A once the activation has settled (a key of the user's, or a late release A cannot be told from one):
    // focus may go back to B, where it came from, but never on to C, the partner A named after it was focused.
    const keyed = {...after, windows: afterActivation(after.windows, 1, t + 350), now64: t + 400, idleMs: 50};
    assert.equal(decide({...keyed, method: 'ActivatePartner'}).reason, 'partner-changed');
    assert.deepEqual(decide({...keyed, method: 'ActivatePartner', partnerChain: [200]}), {act: true, target: 2});
});

test('decide: input from before the last focus gain does not count', () => {
    // A key in B, then a click in A (focus to A), then A moves focus left to B at once: B's key is older than B's new
    // focus gain, so ActivateOwn is refused.
    const keyInB = NOW - 200;
    const s = scene({patch: {2: {userTime: keyInB}}, call: {method: 'ActivateOwn', focusedId: 2, focusGained64: NOW}});
    assert.equal(decide(s).reason, 'no-fresh-input');
});

test('decide: malformed snapshots are refused', () => {
    assert.equal(decide(scene({patch: pressed(1), call: {windows: null}})).reason, 'no-focus');
    assert.equal(decide(scene({patch: pressed(1), call: {windows: [null, 'x']}})).reason, 'no-focus');
    assert.equal(decide({}).reason, 'unknown-method');
});

// A window of pid `pid` that is not one the user switches to: a utility window, or one hidden from the taskbar. It
// has a place in the most-recently-used list all the same, as mutter can give it one.
function sideWindow(id, pid, mru, patch = {}) {
    return {id, pid, wayland: true, userTime: NOW - 9_000, rect: {x: 100, y: 100, w: 300, h: 200}, minimized: false,
        type: 'other', skipTaskbar: false, onWorkspace: true, monitor: 0, stack: 10 + id, mru, ...patch};
}

// Panel A (100) and editor B (200) as in `scene`, with the more recent windows pushed down the list.
const pushedDown = {1: {mru: 2}, 2: {mru: 3}, 3: {mru: 4}};

test('decide: a pid whose only window is a utility or skip-taskbar window is not the partner', () => {
    for (const side of [sideWindow(6, 250, 0), sideWindow(6, 250, 0, {type: 'normal', skipTaskbar: true})]) {
        const label = `${side.type}${side.skipTaskbar ? ', skip-taskbar' : ''}`;
        const extra = [side];
        // The chain names 250 first; it owns nothing the user would call its window, so the partner is 200.
        assert.deepEqual(decide(scene({patch: {...pushedDown, 1: {mru: 2, userTime: NOW - 100}}, extra,
            call: {method: 'ActivatePartner', partnerChain: [250, 200]}})), {act: true, target: 2}, label);
        assert.deepEqual(decide(scene({patch: {...pushedDown, 2: {mru: 3, userTime: NOW - 100}}, extra,
            call: {method: 'ActivateOwn', focusedId: 2, partnerChain: [250, 200]}})), {act: true, target: 1}, label);
        assert.deepEqual(decide(scene({patch: {...pushedDown, 2: {mru: 3, userTime: NOW - 100}}, extra,
            call: {method: 'FocusSelfIfNeighbour', direction: 'right', focusedId: 2, partnerChain: [250, 200]}})),
        {act: true, target: 1}, label);
    }
});

test('decide: the window activated is the most recent one the user switches to, never a utility window', () => {
    // The partner's and the caller's own utility windows are the most recently used of all.
    const extra = [sideWindow(6, 200, 0), sideWindow(7, 100, 1, {type: 'normal', skipTaskbar: true})];
    assert.deepEqual(decide(scene({patch: {...pushedDown, 1: {mru: 2, userTime: NOW - 100}}, extra,
        call: {method: 'ActivatePartner'}})), {act: true, target: 2});
    assert.deepEqual(decide(scene({patch: {...pushedDown, 2: {mru: 3, userTime: NOW - 100}}, extra,
        call: {method: 'ActivateOwn', focusedId: 2}})), {act: true, target: 1});
    // With only a utility window left, the caller has no window to bring forward.
    assert.equal(decide(scene({patch: {...pushedDown, 1: {type: 'other'}, 2: {mru: 3, userTime: NOW - 100}},
        call: {method: 'ActivateOwn', focusedId: 2}})).reason, 'no-own-window');
});

test('decide: a dialog is a window of its pid, for the partner as for a direction', () => {
    const dialog = {id: 6, pid: 250, wayland: true, userTime: NOW - 9_000, rect: {x: 0, y: 0, w: 960, h: 1080},
        minimized: false, type: 'dialog', skipTaskbar: false, onWorkspace: true, monitor: 0, stack: 9,
        mru: 0};
    // The editor's process (250) shows only a dialog; it is the partner, and its dialog the window activated.
    assert.deepEqual(decide(scene({patch: {...pushedDown, 1: {mru: 2, userTime: NOW - 100}}, extra: [dialog],
        call: {method: 'ActivatePartner', partnerChain: [250, 200]}})), {act: true, target: 6});
    // A key in that dialog lets the caller bring itself forward, or take focus as the dialog's right neighbour.
    const focusedDialog = {method: 'ActivateOwn', focusedId: 6, partnerChain: [250, 200]};
    assert.deepEqual(decide(scene({patch: pushedDown, extra: [{...dialog, userTime: NOW - 100}],
        call: focusedDialog})), {act: true, target: 1});
    assert.deepEqual(decide(scene({patch: pushedDown, extra: [{...dialog, userTime: NOW - 100}],
        call: {...focusedDialog, method: 'FocusSelfIfNeighbour', direction: 'right'}})), {act: true, target: 1});
});

test('decide: a window outside the most-recently-used list still belongs to its pid, after those in it', () => {
    assert.deepEqual(decide(scene({patch: {...pressed(1), 2: {mru: null}}, call: {method: 'ActivatePartner'}})),
        {act: true, target: 2});
    // Of two, the one in the list wins; of two outside it, the higher in the stacking order.
    const other = {id: 8, pid: 200, wayland: true, userTime: NOW - 9_000, rect: {x: 0, y: 0, w: 10, h: 10},
        minimized: false, type: 'normal', skipTaskbar: false, onWorkspace: true, monitor: 0, stack: 20,
        mru: null};
    assert.deepEqual(decide(scene({patch: pressed(1), extra: [other], call: {method: 'ActivatePartner'}})),
        {act: true, target: 2});
    assert.deepEqual(decide(scene({patch: {...pressed(1), 2: {mru: null}}, extra: [other],
        call: {method: 'ActivatePartner'}})), {act: true, target: 8});
});


test('decide: a focus gain recorded after now refuses, one from weeks ago does not, one past a clock period does', () => {
    const call = {method: 'ActivateOwn', focusedId: 2};
    assert.equal(decide(scene({patch: pressed(2), call: {...call, focusGained64: NOW + 60_001}})).reason,
        'no-fresh-input');
    const now64 = 50 * DAY + 777;
    const longFocus = {...call, now64, focusGained64: now64 - 49 * DAY};
    assert.deepEqual(decide(scene({patch: {2: {userTime: (now64 - 100) % TOP}}, call: longFocus})),
        {act: true, target: 1});
    // Past one period of the 32-bit clock nothing counts until the window is focused again.
    const tooLong = {...call, now64, focusGained64: now64 - 50 * DAY + 1};
    assert.equal(decide(scene({patch: {2: {userTime: (now64 - 100) % TOP}}, call: tooLong})).reason, 'no-fresh-input');
});

test('fresh input: the user time alone is not enough; a real device event after the focus gain is needed', () => {
    // The focused client moved its own user time to "now" through an activation request, with no input since before
    // it gained focus.
    assert.equal(fresh({userTime: NOW - 10, idleMs: 6_000}), false, 'user time moved, last real input before the gain');
    // A token from a real key, used 700 ms later.
    assert.equal(fresh({userTime: NOW - 10, idleMs: 700}), false, 'banked token');
    // A real key: the user time and the idle reading are both fresh, and after the gain.
    assert.equal(fresh({userTime: NOW - 100, idleMs: 100}), true, 'a real key');
    assert.equal(fresh({userTime: NOW - 100, idleMs: 40}), true, 'a real key, then its release');
    assert.equal(fresh({idleMs: 500}), true, 'exactly 500 ms');
    assert.equal(fresh({idleMs: 501}), false, '501 ms');
    // The last real input must be strictly after the focus gain.
    assert.equal(fresh({focusGained64: NOW - 300, userTime: NOW - 100, idleMs: 299}), true);
    assert.equal(fresh({focusGained64: NOW - 300, userTime: NOW - 100, idleMs: 300}), false, 'exactly at the gain');
    assert.equal(fresh({focusGained64: NOW - 300, userTime: NOW - 100, idleMs: 301}), false, 'before the gain');
});

test('fresh input: a missing or malformed idle reading is not fresh', () => {
    for (const idleMs of [undefined, null, -1, 1.5, '100', NaN, Infinity, 2 ** 53, true])
        assert.equal(fresh({idleMs}), false, String(idleMs));
});

test('decide: every method that moves focus needs a real device event after the focus gain', () => {
    // The scene's focus gain was 10 000 ms ago: the last real input 1 ms before it.
    const stale = 10_001;
    const cases = [
        {method: 'FocusDirection', direction: 'left', focusedId: 1, key: 1, target: 2},
        {method: 'ActivatePartner', focusedId: 1, key: 1, target: 2},
        {method: 'FocusSelfIfNeighbour', direction: 'right', focusedId: 2, key: 2, target: 1},
        {method: 'ActivateOwn', focusedId: 2, key: 2, target: 1},
    ];
    for (const {key, target, ...call} of cases) {
        assert.deepEqual(decide(scene({patch: pressed(key), call: {...call, idleMs: 100}})), {act: true, target},
            call.method);
        for (const idleMs of [stale, 501, undefined, -1])
            assert.equal(decide(scene({patch: pressed(key), call: {...call, idleMs}})).reason, 'no-fresh-input',
                `${call.method}, idle ${idleMs}`);
    }
});

// A focus gain the extension caused itself: device input in the first SETTLE_MS after it may be the release of the
// key that was pressed in the window focus came from.
function settled(lastInputAfterGain, overrides = {}) {
    const gained = NOW - 400;
    return fresh({focusGained64: gained, focusGainedByUs: true, userTime: NOW - 10,
        idleMs: NOW - (gained + lastInputAfterGain), ...overrides});
}

test('fresh input: after the extension\'s own activation, device input counts only once it has settled', () => {
    assert.equal(SETTLE_MS, 300);
    // The release shape: activated at G, the release of a key from the previous window at G + 80, user time moved.
    assert.equal(settled(80), false, 'a release 80 ms after our activation');
    assert.equal(settled(300), false, 'exactly SETTLE_MS after');
    assert.equal(settled(301), true, 'SETTLE_MS + 1 after');
    assert.equal(settled(1), false);
    // The user caused the focus gain (a click, Alt+Tab): input 80 ms after it counts.
    assert.equal(settled(80, {focusGainedByUs: false}), true, 'a gain the user caused');
    assert.equal(settled(0, {focusGainedByUs: false}), false, 'still strictly after the gain');
});

test('fresh input: an unknown origin of the focus gain is taken as the extension\'s own', () => {
    for (const focusGainedByUs of [undefined, null, 0, 1, 'false', 'true', true]) {
        assert.equal(settled(80, {focusGainedByUs}), false, String(focusGainedByUs));
        assert.equal(settled(301, {focusGainedByUs}), true, String(focusGainedByUs));
    }
});

test('decide: every method waits for the extension\'s own activation to settle', () => {
    const gained = NOW - 400;
    const cases = [
        {method: 'FocusDirection', direction: 'left', focusedId: 1, key: 1, target: 2},
        {method: 'ActivatePartner', focusedId: 1, key: 1, target: 2},
        {method: 'FocusSelfIfNeighbour', direction: 'right', focusedId: 2, key: 2, target: 1},
        {method: 'ActivateOwn', focusedId: 2, key: 2, target: 1},
    ];
    for (const {key, target, ...call} of cases) {
        // Focus came to A from B, its partner (the other methods are called with B focused, and ignore it).
        const byUs = {...call, focusGained64: gained, focusGainedByUs: true, focusGainedFrom: 200};
        assert.equal(decide(scene({patch: pressed(key, 10), call: {...byUs, idleMs: NOW - (gained + 80)}})).reason,
            'no-fresh-input', `${call.method}, release 80 ms after`);
        assert.deepEqual(decide(scene({patch: pressed(key, 10), call: {...byUs, idleMs: NOW - (gained + 301)}})),
            {act: true, target}, `${call.method}, input 301 ms after`);
        assert.deepEqual(decide(scene({patch: pressed(key, 10), call: {...byUs, focusGainedByUs: false,
            idleMs: NOW - (gained + 80)}})), {act: true, target}, `${call.method}, a gain the user caused`);
    }
});
