import {test} from 'node:test';
import assert from 'node:assert/strict';

import {
    OWN_ACTIVATION_SHADOW_MS, SETTLE_MS, decide, focusGainCredited, focusGainOrigin,
} from '../policy.js';

// An activation by the extension that puts focus on a window other than the one it named: mutter focuses the named
// window's modal dialog instead, or the client opens one as it is activated. That gain must count as the extension's
// own, bound to the window focus left, as a gain on the named window does. Windows: the editor E (pid 200, window 2) on
// the left, the caller's main window F (pid 100, window 1) on the right, F's modal dialog D (pid 100, window 3) over
// it, and another application's window G (pid 300, window 4). Times are milliseconds on the extension's monotonic
// clock.
const T0 = 80_000_000;
const E = {id: 2, pid: 200};
const F = {id: 1, pid: 100};
const D = {id: 3, pid: 100};
const G = {id: 4, pid: 300};

// The bookkeeping extension.js keeps around focus changes, over plain data: `gained` maps a window id to
// {at, byUs, pending, from}, `last` is the extension's last activation {at, from}.
function newShell() {
    return {gained: new Map(), last: null, focused: null};
}

// `activate()` in extension.js: recorded before mutter is asked, the gain on the named window still to come.
function activate(shell, window, now) {
    const from = shell.focused === null ? 0 : shell.focused.pid;
    shell.last = {at: now, from};
    shell.gained.set(window.id, {at: now, byUs: true, pending: shell.focused !== window, from});
}

// `_noteFocus()` in extension.js: focus moved to `window` at `now`.
function noteFocus(shell, window, now, {userInput = false, atEnable = false} = {}) {
    const previous = shell.gained.get(window.id);
    const origin = focusGainOrigin({
        userInput, atEnable, gainedByUs: previous?.pending === true, gainedAt64: previous?.at, gainedFrom: previous?.from,
        now64: now,
        lastOwnActivation64: shell.last?.at ?? null, lastOwnActivationFrom: shell.last?.from,
    });
    shell.gained.set(window.id, {at: now, byUs: origin.byUs, pending: false, from: origin.from});
    shell.focused = window;
    return origin;
}

function snapshot(now, keyAt) {
    const base = {
        wayland: true, minimized: false, skipTaskbar: false, onWorkspace: true, monitor: 0,
        userTime: keyAt,
    };
    return [
        {...base, ...E, type: 'normal', rect: {x: 0, y: 0, w: 900, h: 1080}, stack: 0, mru: 1},
        {...base, ...F, type: 'normal', rect: {x: 960, y: 0, w: 960, h: 1080}, stack: 1, mru: 2},
        {...base, ...D, type: 'dialog', rect: {x: 1240, y: 400, w: 400, h: 200}, stack: 2, mru: 0},
        {...base, ...G, type: 'normal', rect: {x: 0, y: 0, w: 300, h: 300}, stack: -1, mru: 3},
    ];
}

// The caller (pid 100) asks for its partner from the focused window, the user having pressed a key there at `keyAt`
// (the idle monitor agrees), the call arriving 20 ms later.
function askPartner(shell, keyAt, partnerChain = [E.pid], method = 'ActivatePartner') {
    const now = keyAt + 20;
    const record = shell.gained.get(shell.focused.id);
    return decide({
        method, direction: method === 'FocusDirection' ? 'left' : undefined, callerPid: 100,
        callerFocusedAt64: undefined, partnerChain, focusedId: shell.focused.id, focusGained64: record.at,
        focusGainedByUs: record.byUs, focusGainedFrom: record.from, now64: now, idleMs: now - keyAt, modal: false,
        windows: snapshot(now, keyAt),
    });
}

// The user is in the editor; the extension activates F (an ActivateOwn it granted, say); focus lands on D at `landAt`.
function activationLandsOnDialog(landAt) {
    const shell = newShell();
    noteFocus(shell, E, T0 - 60_000, {userInput: true});
    activate(shell, F, T0);
    noteFocus(shell, D, T0 + landAt);
    return shell;
}

test('origin: a gain on the window a pending activation named is the extension\'s, from that activation', () => {
    for (const userInput of [false, true]) {
        for (const after of [0, 5, OWN_ACTIVATION_SHADOW_MS - 1]) {
            assert.deepEqual(focusGainOrigin({userInput, atEnable: false, gainedByUs: true, gainedAt64: T0,
                gainedFrom: 200, now64: T0 + after, lastOwnActivation64: T0 + after, lastOwnActivationFrom: 300}),
            {byUs: true, from: 200}, `${after} ms after, user input ${userInput}`);
        }
    }
});

test('origin: a pending activation older than the shadow names no later gain', () => {
    const late = {atEnable: false, gainedByUs: true, gainedAt64: T0, gainedFrom: 200, now64: T0 + 60_000,
        lastOwnActivation64: T0, lastOwnActivationFrom: 200};
    assert.deepEqual(focusGainOrigin({...late, userInput: true}), {byUs: false, from: 0}, 'a click is the user\'s');
    assert.deepEqual(focusGainOrigin({...late, userInput: false}), {byUs: true, from: 0}, 'a fallback has no source');
    // Within the shadow of a later activation, that activation's source.
    assert.deepEqual(focusGainOrigin({...late, userInput: false, lastOwnActivation64: T0 + 59_900,
        lastOwnActivationFrom: 300}), {byUs: true, from: 300});
    // A pending record whose time cannot be read ties the gain to nothing and credits nothing.
    for (const gainedAt64 of [undefined, null, 0, 'x', T0 + 60_001]) {
        for (const userInput of [false, true]) {
            assert.deepEqual(focusGainOrigin({...late, userInput, gainedAt64}), {byUs: true, from: 0},
                `pending since ${gainedAt64}, user input ${userInput}`);
        }
    }
});

test('origin: a gain on another window within the shadow of an activation is the extension\'s, from it', () => {
    for (const after of [0, 1, 100, OWN_ACTIVATION_SHADOW_MS - 1]) {
        for (const userInput of [false, true]) {
            assert.deepEqual(focusGainOrigin({userInput, atEnable: false, gainedByUs: false, gainedFrom: undefined,
                now64: T0 + after, lastOwnActivation64: T0, lastOwnActivationFrom: 200}), {byUs: true, from: 200},
            `${after} ms after, user input ${userInput}`);
        }
    }
});

test('origin: a gain with no input event, outside any shadow, is the extension\'s, from no window', () => {
    for (const last of [null, T0 - OWN_ACTIVATION_SHADOW_MS, T0 - 60_000]) {
        assert.deepEqual(focusGainOrigin({userInput: false, atEnable: false, gainedByUs: false, now64: T0,
            lastOwnActivation64: last, lastOwnActivationFrom: 200}), {byUs: true, from: 0}, `last ${last}`);
    }
    assert.deepEqual(focusGainOrigin({userInput: false, atEnable: true, gainedByUs: false, now64: T0,
        lastOwnActivation64: null}), {byUs: true, from: 0}, 'the focus found at enable');
    assert.deepEqual(focusGainOrigin({userInput: true, atEnable: true, gainedByUs: false, now64: T0,
        lastOwnActivation64: null}), {byUs: true, from: 0}, 'at enable, whatever the event');
});

test('origin: only a gain the user made is the user\'s', () => {
    assert.deepEqual(focusGainOrigin({userInput: true, atEnable: false, gainedByUs: false, now64: T0,
        lastOwnActivation64: null, lastOwnActivationFrom: undefined}), {byUs: false, from: 0});
    assert.deepEqual(focusGainOrigin({userInput: true, atEnable: false, gainedByUs: false, now64: T0,
        lastOwnActivation64: T0 - OWN_ACTIVATION_SHADOW_MS, lastOwnActivationFrom: 200}), {byUs: false, from: 0});
});

test('origin: anything that cannot be trusted is the extension\'s, tied to no activation', () => {
    const base = {userInput: true, atEnable: false, gainedByUs: false, now64: T0, lastOwnActivation64: T0 - 100,
        lastOwnActivationFrom: 200};
    for (const [label, change] of [
        ['user input not a boolean', {userInput: 1}], ['at enable missing', {atEnable: undefined}],
        ['pending not a boolean', {gainedByUs: 'no'}], ['pending missing', {gainedByUs: undefined}],
    ]) {
        const origin = focusGainOrigin({...base, ...change});
        assert.equal(origin.byUs, true, label);
    }
    for (const [label, change] of [
        ['now missing', {now64: undefined}], ['now not an integer', {now64: T0 + 0.5}],
        ['activation in the future', {lastOwnActivation64: T0 + 1}],
        ['activation time not a number', {lastOwnActivation64: 'x'}],
    ])
        assert.deepEqual(focusGainOrigin({...base, ...change}), {byUs: true, from: 0}, label);
    for (const from of [0, -1, 1.5, 2 ** 32, undefined, null, '200'])
        assert.deepEqual(focusGainOrigin({...base, lastOwnActivationFrom: from}), {byUs: true, from: 0}, `from ${from}`);
    assert.deepEqual(focusGainOrigin({...base, gainedByUs: true, gainedFrom: 'x'}), {byUs: true, from: 0});
});

test('origin: a gain is the extension\'s exactly when it is not credited', () => {
    const flags = [true, false, undefined, 1];
    for (const userInput of flags) {
        for (const atEnable of flags) {
            for (const gainedByUs of flags) {
                for (const last of [null, T0 - 2_000, T0 - 999, T0, T0 + 1, undefined]) {
                    const args = {userInput, atEnable, gainedByUs, now64: T0, lastOwnActivation64: last};
                    assert.equal(focusGainOrigin({...args, gainedAt64: T0, gainedFrom: 200,
                        lastOwnActivationFrom: 200}).byUs,
                        !focusGainCredited(args), JSON.stringify(args));
                }
            }
        }
    }
});

for (const [label, landAt] of [['mutter focuses the modal dialog at once', 2],
    ['the client opens a modal dialog as it is activated', 120]]) {
    test(`${label}: input in the dialog waits for the settle`, () => {
        const shell = activationLandsOnDialog(landAt);
        assert.deepEqual(shell.gained.get(D.id), {at: T0 + landAt, byUs: true, pending: false, from: E.pid});
        const early = T0 + landAt + SETTLE_MS - 50;
        assert.equal(askPartner(shell, early).reason, 'no-fresh-input');
        assert.equal(askPartner(shell, early, [E.pid], 'FocusDirection').reason, 'no-fresh-input');
        const late = T0 + landAt + SETTLE_MS + 50;
        assert.deepEqual(askPartner(shell, late), {act: true, target: E.id});
        assert.deepEqual(askPartner(shell, late, [E.pid], 'FocusDirection'), {act: true, target: E.id});
    });

    test(`${label}: ActivatePartner from the dialog goes only back where focus came from`, () => {
        const shell = activationLandsOnDialog(landAt);
        const late = T0 + landAt + 600;
        assert.equal(askPartner(shell, late, [G.pid]).reason, 'partner-changed');
        assert.deepEqual(askPartner(shell, late, [E.pid]), {act: true, target: E.id});
    });
}

test('a dialog the client opens after the shadow, with no input behind it, may hand focus nowhere', () => {
    const landAt = OWN_ACTIVATION_SHADOW_MS + 500;
    const shell = activationLandsOnDialog(landAt);
    assert.deepEqual(shell.gained.get(D.id), {at: T0 + landAt, byUs: true, pending: false, from: 0});
    assert.equal(askPartner(shell, T0 + landAt + SETTLE_MS - 50, [E.pid], 'FocusDirection').reason,
        'no-fresh-input');
    for (const partner of [E.pid, G.pid])
        assert.equal(askPartner(shell, T0 + landAt + 600, [partner]).reason, 'partner-changed');
    // The user clicks into F, then back into the dialog: the dialog's focus is now the user's, and free.
    const click = T0 + landAt + 3_000;
    noteFocus(shell, F, click, {userInput: true});
    noteFocus(shell, D, click + 500, {userInput: true});
    assert.deepEqual(shell.gained.get(D.id), {at: click + 500, byUs: false, pending: false, from: 0});
    assert.deepEqual(askPartner(shell, click + 600, [G.pid]), {act: true, target: G.id});
});

test('focus falling back to a window after another closed is the extension\'s, from no window', () => {
    const shell = newShell();
    noteFocus(shell, F, T0, {userInput: true});
    noteFocus(shell, D, T0 + 10_000);
    noteFocus(shell, F, T0 + 20_000);
    assert.deepEqual(shell.gained.get(F.id), {at: T0 + 20_000, byUs: true, pending: false, from: 0});
    assert.equal(askPartner(shell, T0 + 20_150, [E.pid], 'FocusDirection').reason, 'no-fresh-input');
    assert.equal(askPartner(shell, T0 + 20_600, [E.pid]).reason, 'partner-changed');
    assert.deepEqual(askPartner(shell, T0 + 20_600, [E.pid], 'FocusDirection'), {act: true, target: E.id});
});

test('after the dialog an activation landed on closes, focus falling back to the named window has no source', () => {
    const shell = activationLandsOnDialog(2);
    assert.equal(shell.gained.get(F.id).pending, true, 'the named window never had its notify');
    noteFocus(shell, F, T0 + 60_000);
    assert.deepEqual(shell.gained.get(F.id), {at: T0 + 60_000, byUs: true, pending: false, from: 0});
    assert.equal(askPartner(shell, T0 + 60_600, [E.pid]).reason, 'partner-changed');
});

test('the user\'s click into the named window after the shadow is the user\'s, not the old activation\'s', () => {
    const shell = activationLandsOnDialog(2);
    noteFocus(shell, E, T0 + 30_000, {userInput: true});
    const origin = noteFocus(shell, F, T0 + 60_000, {userInput: true});
    assert.deepEqual(origin, {byUs: false, from: 0});
    assert.deepEqual(askPartner(shell, T0 + 60_100, [G.pid]), {act: true, target: G.id});
});

test('a click inside the shadow of an activation counts as the activation\'s', () => {
    const shell = newShell();
    noteFocus(shell, E, T0 - 60_000, {userInput: true});
    activate(shell, G, T0);
    noteFocus(shell, G, T0 + 3);
    noteFocus(shell, D, T0 + 400, {userInput: true});
    assert.deepEqual(shell.gained.get(D.id), {at: T0 + 400, byUs: true, pending: false, from: E.pid});
    assert.equal(askPartner(shell, T0 + 1_200, [G.pid]).reason, 'partner-changed');
});

test('a gain on the window the activation named keeps working as before', () => {
    const shell = newShell();
    noteFocus(shell, E, T0 - 60_000, {userInput: true});
    activate(shell, F, T0);
    noteFocus(shell, F, T0 + 2);
    assert.deepEqual(shell.gained.get(F.id), {at: T0 + 2, byUs: true, pending: false, from: E.pid});
    assert.equal(askPartner(shell, T0 + 200).reason, 'no-fresh-input');
    assert.deepEqual(askPartner(shell, T0 + 600), {act: true, target: E.id});
    assert.equal(askPartner(shell, T0 + 600, [G.pid]).reason, 'partner-changed');
});
