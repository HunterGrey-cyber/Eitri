import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';

import {EXTENSION_VERSION, isConnectorName, planMoveOwnToMonitor} from '../policy.js';

// MoveOwnToMonitor puts the caller's own top-level windows on the monitor whose connector it names. It moves nothing
// of anyone else's, and its only answer is whether it moved something.
//
// The caller is pid 100 with one main window (1) and a dialog of it (2); pid 200 has a window (3) on the same
// monitor; pid 300 is an X11 client that claims pid 100 (4). Two monitors: 0 and 1.
function windows() {
    const base = {
        wayland: true, userTime: 1_000, minimized: false, type: 'normal', skipTaskbar: false, onWorkspace: true,
        monitor: 0, transient: false, rect: {x: 0, y: 0, w: 800, h: 600},
    };
    return [
        {...base, id: 1, pid: 100, stack: 0, mru: 0},
        {...base, id: 2, pid: 100, type: 'dialog', transient: true, stack: 1, mru: 1},
        {...base, id: 3, pid: 200, stack: 2, mru: 2},
        {...base, id: 4, pid: 100, wayland: false, stack: 3, mru: 3},
    ];
}

function plan(overrides = {}) {
    return planMoveOwnToMonitor({
        callerPid: 100, connector: 'DP-2', monitorIndex: 1, monitorCount: 2, windows: windows(), ...overrides,
    });
}

test('moves_only_the_callers_own_windows', () => {
    const verdict = plan();
    assert.equal(verdict.act, true);
    assert.equal(verdict.monitor, 1);
    // The dialog follows its parent, another pid's window is never named, and an X11 window does not stand for a
    // pid that has a Wayland window.
    assert.deepEqual(verdict.targets, [1]);

    // Another caller with a window of its own moves only that window.
    assert.deepEqual(plan({callerPid: 200}).targets, [3]);

    // A pid with only X11 windows is reached through them, as every other method reaches it.
    const x11Only = windows().filter(w => w.id !== 1 && w.id !== 2);
    assert.deepEqual(plan({windows: x11Only}).targets, [4]);

    // Two main windows of the caller both go.
    const two = [...windows(), {...windows()[0], id: 5, stack: 4, mru: 4}];
    assert.deepEqual(plan({windows: two}).targets, [1, 5]);

    // A window already on that monitor is still the caller's and still counts as there.
    const there = windows().map(w => (w.id === 1 ? {...w, monitor: 1} : w));
    assert.deepEqual(plan({windows: there}).targets, [1]);
});

test('an_unknown_connector_moves_nothing_and_answers_false', () => {
    // mutter answers -1 for a connector no monitor has.
    assert.deepEqual(plan({monitorIndex: -1}), {act: false, reason: 'no-such-monitor'});
    assert.deepEqual(plan({monitorIndex: 2}), {act: false, reason: 'no-such-monitor'});
    assert.deepEqual(plan({monitorIndex: undefined}), {act: false, reason: 'no-such-monitor'});
    assert.deepEqual(plan({monitorIndex: 0.5}), {act: false, reason: 'no-such-monitor'});
    assert.deepEqual(plan({monitorCount: 0, monitorIndex: 0}), {act: false, reason: 'no-such-monitor'});
    // A name that no connector could have is refused before mutter is asked.
    for (const connector of ['', ' ', 'DP 2', 'DP-2\n', 'x'.repeat(65), 'é', '../DP-1', null, undefined, 2, ['DP-2']])
        assert.deepEqual(plan({connector}), {act: false, reason: 'bad-connector'}, JSON.stringify(connector));
});

test('a_caller_with_no_window_answers_false', () => {
    assert.deepEqual(plan({callerPid: 400}), {act: false, reason: 'no-own-window'});
    // A dialog alone, a window hidden from the taskbar, a utility window: none is a top-level window to move.
    const odd = [
        {...windows()[1], id: 6, pid: 400},
        {...windows()[0], id: 7, pid: 400, skipTaskbar: true},
        {...windows()[0], id: 8, pid: 400, type: 'other'},
        {...windows()[0], id: 9, pid: 400, transient: undefined},
    ];
    assert.deepEqual(plan({callerPid: 400, windows: odd}), {act: false, reason: 'no-own-window'});
    assert.deepEqual(plan({windows: []}), {act: false, reason: 'no-own-window'});
    assert.deepEqual(plan({windows: null}), {act: false, reason: 'no-own-window'});
    for (const callerPid of [0, -1, undefined, 2 ** 32])
        assert.deepEqual(plan({callerPid}), {act: false, reason: 'no-caller'}, String(callerPid));
});

test('connector names are what mutter and the kernel give', () => {
    for (const name of ['DP-1', 'HDMI-A-2', 'eDP-1', 'Virtual-1', 'Meta-0', 'DVI-I-1', 'x'.repeat(64)])
        assert.equal(isConnectorName(name), true, name);
});

test('the version Version answers is the one metadata.json states', () => {
    assert.equal(EXTENSION_VERSION, 2);
    const metadata = JSON.parse(readFileSync(new URL('../metadata.json', import.meta.url), 'utf8'));
    assert.equal(metadata.version, EXTENSION_VERSION);
    const src = readFileSync(new URL('../extension.js', import.meta.url), 'utf8');
    assert.ok(/<method name="MoveOwnToMonitor"><arg type="s" direction="in" name="connector"\/><arg type="b" direction="out" name="moved"\/><\/method>/
        .test(src), 'the interface declares MoveOwnToMonitor(s) -> b');
    assert.ok(/\bVersion\(\)\s*\{\s*return EXTENSION_VERSION;/.test(src), 'Version answers the shared constant');
});
