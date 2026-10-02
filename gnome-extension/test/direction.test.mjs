import {test} from 'node:test';
import assert from 'node:assert/strict';

import {DIRECTIONS, EDGE_TOLERANCE, isDirection, pickNeighbour} from '../direction.js';

let nextStack = 0;
function win(id, x, y, w, h, extra = {}) {
    return {
        id, rect: {x, y, w, h}, minimized: false, normal: true, onWorkspace: true, onMonitor: true,
        stack: nextStack++, ...extra,
    };
}

const F = win('F', 1000, 400, 400, 300);

test('the four directions, and nothing else', () => {
    assert.deepEqual([...DIRECTIONS], ['left', 'right', 'up', 'down']);
    for (const d of DIRECTIONS)
        assert.ok(isDirection(d));
    for (const d of ['Left', 'LEFT', '', 'north', 'left ', null, undefined, 1, {}])
        assert.equal(isDirection(d), false, String(d));
    const L = win('L', 500, 400, 400, 300);
    for (const d of ['Left', '', 'west', null, undefined, 0])
        assert.equal(pickNeighbour(F, [F, L], d), null, String(d));
});

test('side by side: each finds the other, in both directions', () => {
    const a = win('A', 0, 0, 960, 1080);
    const b = win('B', 960, 0, 960, 1080);
    assert.equal(pickNeighbour(a, [a, b], 'right'), 'B');
    assert.equal(pickNeighbour(b, [a, b], 'left'), 'A');
    assert.equal(pickNeighbour(a, [a, b], 'left'), null);
    assert.equal(pickNeighbour(b, [a, b], 'right'), null);
    assert.equal(pickNeighbour(a, [a, b], 'up'), null);
    assert.equal(pickNeighbour(a, [a, b], 'down'), null);
});

test('stacked: up and down', () => {
    const top = win('T', 0, 0, 1920, 540);
    const bottom = win('Bo', 0, 540, 1920, 540);
    assert.equal(pickNeighbour(top, [top, bottom], 'down'), 'Bo');
    assert.equal(pickNeighbour(bottom, [top, bottom], 'up'), 'T');
    assert.equal(pickNeighbour(top, [top, bottom], 'up'), null);
    assert.equal(pickNeighbour(top, [top, bottom], 'left'), null);
    assert.equal(pickNeighbour(top, [top, bottom], 'right'), null);
});

test('the nearest beyond the edge wins over one further away', () => {
    const near = win('near', 500, 400, 400, 300);
    const far = win('far', 0, 400, 400, 300);
    assert.equal(pickNeighbour(F, [F, far, near], 'left'), 'near');
});

test('diagonal: with nothing overlapping on the other axis, the nearest centre beyond the edge', () => {
    // Up-left and down-left of F, neither overlapping F's rows.
    const upLeft = win('UL', 600, 0, 300, 300);
    const downLeftFar = win('DLfar', 0, 800, 300, 200);
    assert.equal(pickNeighbour(F, [F, upLeft, downLeftFar], 'left'), 'UL');
    assert.equal(pickNeighbour(F, [F, upLeft, downLeftFar], 'up'), 'UL');
    assert.equal(pickNeighbour(F, [F, upLeft, downLeftFar], 'down'), 'DLfar');
    assert.equal(pickNeighbour(F, [F, upLeft, downLeftFar], 'right'), null);
});

test('an overlapping candidate always beats a diagonal one, even a nearer diagonal', () => {
    const diagonalNear = win('diag', 900, 100, 90, 90);
    const overlappingFar = win('row', 0, 650, 200, 300);
    assert.equal(pickNeighbour(F, [F, diagonalNear, overlappingFar], 'left'), 'row');
});

test('overlapping windows: only a centre beyond the edge counts', () => {
    // Covers F's left part, its centre left of F's left edge: reached by the fallback.
    const coverLeft = win('coverL', 700, 400, 500, 300);
    assert.equal(pickNeighbour(F, [F, coverLeft], 'left'), 'coverL');
    // Overlaps F with its centre inside F: in no direction.
    const inside = win('inside', 1100, 450, 200, 200);
    for (const d of DIRECTIONS)
        assert.equal(pickNeighbour(F, [F, inside], d), null, d);
    // Covers F entirely: same centre, so in no direction either.
    const cover = win('cover', 900, 300, 600, 500);
    for (const d of DIRECTIONS)
        assert.equal(pickNeighbour(F, [F, cover], d), null, d);
});

test('frames that touch or share a border within the tolerance are beyond the edge', () => {
    const touching = win('touch', 600, 400, 400, 300);
    assert.equal(pickNeighbour(F, [F, touching], 'left'), 'touch');
    const sharing = win('share', 600 + EDGE_TOLERANCE, 400, 400, 300);
    assert.equal(pickNeighbour(F, [F, sharing], 'left'), 'share');
    // One pixel more is an overlap, not a neighbour beyond the edge; its centre is still left of F, so the fallback
    // takes it when nothing else is there.
    const tooMuch = win('much', 600 + EDGE_TOLERANCE + 1, 400, 400, 300);
    const beyond = win('beyond', 0, 0, 100, 100);
    assert.equal(pickNeighbour(F, [F, tooMuch, beyond], 'left'), 'much');
    const lined = win('lined', 100, 400, 100, 300);
    assert.equal(pickNeighbour(F, [F, tooMuch, lined], 'left'), 'lined');
    // Within the tolerance on the right and below too.
    const right = win('R', 1400 - EDGE_TOLERANCE, 400, 300, 300);
    assert.equal(pickNeighbour(F, [F, right], 'right'), 'R');
    const below = win('D', 1000, 700 - EDGE_TOLERANCE, 400, 200);
    assert.equal(pickNeighbour(F, [F, below], 'down'), 'D');
    const above = win('U', 1000, 0, 400, 400 + EDGE_TOLERANCE);
    assert.equal(pickNeighbour(F, [F, above], 'up'), 'U');
});

test('a touching overlap is as near as a frame that just touches; overlap on the other axis decides', () => {
    const overlapsEdge = win('o', 1000 - 400 + 5, 400, 400, 100);
    const touchesTaller = win('t', 1000 - 400, 400, 400, 300);
    assert.equal(pickNeighbour(F, [F, overlapsEdge, touchesTaller], 'left'), 't');
});

test('minimised windows are not candidates', () => {
    const min = win('min', 500, 400, 400, 300, {minimized: true});
    assert.equal(pickNeighbour(F, [F, min], 'left'), null);
    const other = win('other', 0, 400, 400, 300);
    assert.equal(pickNeighbour(F, [F, min, other], 'left'), 'other');
});

test('windows on another workspace are not candidates', () => {
    const away = win('away', 500, 400, 400, 300, {onWorkspace: false});
    assert.equal(pickNeighbour(F, [F, away], 'left'), null);
});

test('windows on another monitor are not candidates', () => {
    const away = win('away', 500, 400, 400, 300, {onMonitor: false});
    assert.equal(pickNeighbour(F, [F, away], 'left'), null);
});

test('windows that may not take focus this way are not candidates', () => {
    const notNormal = win('nn', 500, 400, 400, 300, {normal: false});
    assert.equal(pickNeighbour(F, [F, notNormal], 'left'), null);
});

test('malformed records are never picked', () => {
    const cases = [
        null, 'x', win('m1', 500, 400, 400, 300, {normal: 1}), win('m2', 500, 400, 400, 300, {minimized: undefined}),
        win('m3', 500, 400, 400, 300, {onWorkspace: undefined}), win('m4', 500, 400, 400, 300, {onMonitor: 'yes'}),
        {id: 'm5', rect: {x: 500, y: 400, w: 0, h: 300}, minimized: false, normal: true, onWorkspace: true, onMonitor: true},
        {id: 'm6', rect: {x: NaN, y: 400, w: 400, h: 300}, minimized: false, normal: true, onWorkspace: true, onMonitor: true},
        {id: 'm7', minimized: false, normal: true, onWorkspace: true, onMonitor: true},
    ];
    assert.equal(pickNeighbour(F, [F, ...cases], 'left'), null);
    assert.equal(pickNeighbour({id: 'F', rect: {x: 0, y: 0, w: -1, h: 1}}, [win('a', 0, 0, 10, 10)], 'left'), null);
    assert.equal(pickNeighbour(null, [F], 'left'), null);
    assert.equal(pickNeighbour(F, null, 'left'), null);
});

test('the focused window is never its own neighbour, and none at all gives null', () => {
    assert.equal(pickNeighbour(F, [F], 'left'), null);
    assert.equal(pickNeighbour(F, [], 'right'), null);
    // A record with the focused id elsewhere is still the focused window.
    const sameId = win('F', 0, 400, 400, 300);
    assert.equal(pickNeighbour(F, [F, sameId], 'left'), null);
});

test('ties: equal gaps go to the larger overlap, then to the higher window in the stacking order', () => {
    const smallOverlap = win('small', 500, 400, 400, 100, {stack: 10});
    const bigOverlap = win('big', 500, 500, 400, 200, {stack: 1});
    assert.equal(pickNeighbour(F, [F, smallOverlap, bigOverlap], 'left'), 'big');

    const lower = win('lower', 500, 400, 400, 300, {stack: 2});
    const higher = win('higher', 500, 400, 400, 300, {stack: 7});
    assert.equal(pickNeighbour(F, [F, lower, higher], 'left'), 'higher');
    assert.equal(pickNeighbour(F, [F, higher, lower], 'left'), 'higher');
});

test('ties in the fallback go to the higher window in the stacking order', () => {
    const a = win('a', 600, 0, 200, 200, {stack: 3});
    const b = win('b', 600, 0, 200, 200, {stack: 9});
    assert.equal(pickNeighbour(F, [F, a, b], 'up'), 'b');
    assert.equal(pickNeighbour(F, [F, b, a], 'up'), 'b');
});
