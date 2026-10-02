import {test} from 'node:test';
import assert from 'node:assert/strict';

import {SETTLE_MS, decide} from '../policy.js';

// After the extension itself has moved focus to a window ("a pull"), the window's owner can make device input count
// for it: the user's release of the key that caused the pull lands in it, and the idle monitor cannot tell a release
// from a press, while the client moves its own user time to the same moment. ActivatePartner from that window may then
// only send focus back to where it came from, so a caller cannot name a new partner after the pull and hand focus to
// it.
//
// A panel A (pid 100, window 1) on the right, its editor B (pid 200, window 2) on the left, and an unrelated window C
// (pid 300, window 3). Times are milliseconds on the extension's monotonic clock.
const NOW = 55_320_000;

function windows() {
    const base = {
        wayland: true, userTime: NOW - 9_000, minimized: false, type: 'normal', skipTaskbar: false, onWorkspace: true,
        monitor: 0,
    };
    return [
        {...base, id: 2, pid: 200, rect: {x: 0, y: 0, w: 960, h: 1080}, stack: 0, mru: 1},
        {...base, id: 1, pid: 100, rect: {x: 960, y: 0, w: 960, h: 1080}, stack: 1, mru: 0},
        {...base, id: 3, pid: 300, rect: {x: 0, y: 0, w: 800, h: 600}, stack: 2, mru: 2, onWorkspace: false},
    ];
}

// A's window gained focus at `gained` through the extension's own activation, focus coming from B. `inputAfter` ms
// after that gain the last real device event happened (a release, or a key), and A's user time was moved to it; the
// call comes 10 ms later.
function afterPull({inputAfter, partnerChain, gained = NOW - 10_000, ...rest}) {
    const input = gained + inputAfter;
    return {
        method: 'ActivatePartner', callerPid: 100, partnerChain, focusedId: 1, focusGained64: gained,
        focusGainedByUs: true, focusGainedFrom: 200, now64: input + 10, idleMs: 10, modal: false,
        windows: windows().map(w => (w.id === 1 ? {...w, userTime: input} : w)), ...rest,
    };
}

const DELAYS = [1, 40, 80, 150, 250, 299, SETTLE_MS, SETTLE_MS + 1, 320, 350, 400, 450, 499, 500, 750, 2_000, 60_000];

test('after a pull, ActivatePartner to a partner named since is refused, at every delay of the release', () => {
    for (const inputAfter of DELAYS) {
        for (const partnerChain of [[300], [300, 200], [210, 300, 200]]) {
            const verdict = decide(afterPull({inputAfter, partnerChain}));
            assert.equal(verdict.act, false, `input ${inputAfter} ms after the pull, chain ${partnerChain}`);
            if (inputAfter > SETTLE_MS)
                assert.equal(verdict.reason, 'partner-changed', `input ${inputAfter} ms after, chain ${partnerChain}`);
        }
    }
});

test('after a pull, ActivatePartner back to the window focus came from is still allowed once settled', () => {
    for (const inputAfter of DELAYS) {
        const verdict = decide(afterPull({inputAfter, partnerChain: [200]}));
        if (inputAfter > SETTLE_MS)
            assert.deepEqual(verdict, {act: true, target: 2}, `input ${inputAfter} ms after the pull`);
        else
            assert.equal(verdict.reason, 'no-fresh-input', `input ${inputAfter} ms after the pull`);
    }
});

test('a raise from nvim: the partner is the sender\'s chain, then nvim\'s own; Ctrl+g is still accepted', () => {
    // `:EitriPanel` in nvim (pid 210) inside the terminal B (200): the panel sets the forwarding process's chain, B
    // has focus with the user's Enter in it, and ActivateOwn pulls focus to the panel.
    const raise = {
        method: 'ActivateOwn', callerPid: 100, partnerChain: [4321, 210, 205, 200], focusedId: 2,
        focusGained64: NOW - 60_000, focusGainedByUs: false, now64: NOW, idleMs: 30, modal: false,
        windows: windows().map(w => (w.id === 2 ? {...w, userTime: NOW - 30} : w)),
    };
    assert.deepEqual(decide(raise), {act: true, target: 1});
    // Once attached, the panel names nvim's own chain. Its first window owner is the same terminal, so Ctrl+g in the
    // panel, a deliberate key well after the pull, still hands focus back to the editor.
    for (const partnerChain of [[210, 205, 200], [205, 200], [200]])
        assert.deepEqual(decide(afterPull({inputAfter: 900, partnerChain})), {act: true, target: 2}, `${partnerChain}`);
});

test('a focus gain the user made keeps ActivatePartner to whatever partner the caller names', () => {
    for (const focusGainedFrom of [200, undefined, 0]) {
        for (const [partnerChain, target] of [[[300], 3], [[200], 2], [[300, 200], 3]]) {
            const call = afterPull({inputAfter: 80, partnerChain, focusGainedByUs: false, focusGainedFrom});
            assert.deepEqual(decide(call), {act: true, target}, `chain ${partnerChain}, from ${focusGainedFrom}`);
        }
    }
});

test('after a pull of unknown origin, ActivatePartner is refused whatever the partner', () => {
    for (const focusGainedFrom of [undefined, null, 0, -1, 1.5, '200', 2 ** 32]) {
        for (const partnerChain of [[200], [300]]) {
            assert.equal(decide(afterPull({inputAfter: 900, partnerChain, focusGainedFrom})).reason, 'partner-changed',
                `from ${String(focusGainedFrom)}, chain ${partnerChain}`);
        }
    }
    // A gain whose origin is unknown is taken as the extension's own, so its recorded source still binds.
    for (const focusGainedByUs of [undefined, null, 1, 'false']) {
        assert.equal(decide(afterPull({inputAfter: 900, partnerChain: [300], focusGainedByUs})).reason,
            'partner-changed', String(focusGainedByUs));
        assert.deepEqual(decide(afterPull({inputAfter: 900, partnerChain: [200], focusGainedByUs})),
            {act: true, target: 2}, String(focusGainedByUs));
    }
});

test('after a pull, a partner that owns no window is still "no-partner"', () => {
    assert.equal(decide(afterPull({inputAfter: 900, partnerChain: [555]})).reason, 'no-partner');
    assert.equal(decide(afterPull({inputAfter: 900, partnerChain: []})).reason, 'no-partner');
});

test('the origin of a pull binds ActivatePartner only: the other methods are judged as before', () => {
    // FocusDirection moves only to a spatial neighbour; the delayed release can still pass it after SETTLE_MS.
    assert.deepEqual(decide(afterPull({inputAfter: 400, partnerChain: [300], method: 'FocusDirection',
        direction: 'left'})), {act: true, target: 2});
    // B pulled from somewhere else: A's ActivateOwn on a fresh key in B is not about the origin.
    const own = {
        method: 'ActivateOwn', callerPid: 100, partnerChain: [200], focusedId: 2, focusGained64: NOW - 1_000,
        focusGainedByUs: true, focusGainedFrom: 999, now64: NOW, idleMs: 30, modal: false,
        windows: windows().map(w => (w.id === 2 ? {...w, userTime: NOW - 30} : w)),
    };
    assert.deepEqual(decide(own), {act: true, target: 1});
});
