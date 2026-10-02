// Which window is "to the left of" (right of, above, below) the focused one. Plain records in, an id out: nothing here
// touches GNOME Shell, so the same file runs under node for the tests and under gjs inside the shell.
//
// A record is {id, rect: {x, y, w, h}, minimized, normal, onWorkspace, onMonitor, stack}. `normal` means a window the
// user switches to (a normal window or a dialog, not hidden from the taskbar; policy.js's `isUserWindow`, the same test
// that decides which windows a pid owns), `onMonitor` means on the focused window's monitor, and a larger `stack` is
// higher in the stacking order.

export const DIRECTIONS = Object.freeze(['left', 'right', 'up', 'down']);

// Frames that touch, or share a border a few pixels wide, still count as lying beyond the edge.
export const EDGE_TOLERANCE = 10;

export function isDirection(direction) {
    return typeof direction === 'string' && DIRECTIONS.includes(direction);
}

function validRect(r) {
    return r !== null && typeof r === 'object' &&
        Number.isFinite(r.x) && Number.isFinite(r.y) && Number.isFinite(r.w) && Number.isFinite(r.h) &&
        r.w > 0 && r.h > 0;
}

// Anything malformed is simply not a candidate: a window we cannot reason about is never the one focus moves to.
function eligible(c, focused) {
    return c !== null && typeof c === 'object' && c.id !== focused.id &&
        c.normal === true && c.minimized === false && c.onWorkspace === true && c.onMonitor === true &&
        validRect(c.rect);
}

function beyondEdge(c, f, direction) {
    switch (direction) {
    case 'left': return c.x + c.w <= f.x + EDGE_TOLERANCE;
    case 'right': return c.x >= f.x + f.w - EDGE_TOLERANCE;
    case 'up': return c.y + c.h <= f.y + EDGE_TOLERANCE;
    case 'down': return c.y >= f.y + f.h - EDGE_TOLERANCE;
    }
    return false;
}

// Distance from the focused frame's edge to the candidate's near edge. Frames within the tolerance overlap a little;
// they are as near as touching ones, so the overlap on the other axis decides between them.
function edgeGap(c, f, direction) {
    let gap;
    switch (direction) {
    case 'left': gap = f.x - (c.x + c.w); break;
    case 'right': gap = c.x - (f.x + f.w); break;
    case 'up': gap = f.y - (c.y + c.h); break;
    case 'down': gap = c.y - (f.y + f.h); break;
    }
    return Math.max(0, gap);
}

function crossOverlap(c, f, direction) {
    if (direction === 'left' || direction === 'right')
        return Math.min(c.y + c.h, f.y + f.h) - Math.max(c.y, f.y);
    return Math.min(c.x + c.w, f.x + f.w) - Math.max(c.x, f.x);
}

function centre(r) {
    return {x: r.x + r.w / 2, y: r.y + r.h / 2};
}

function centreBeyond(c, f, direction) {
    const m = centre(c);
    switch (direction) {
    case 'left': return m.x < f.x;
    case 'right': return m.x > f.x + f.w;
    case 'up': return m.y < f.y;
    case 'down': return m.y > f.y + f.h;
    }
    return false;
}

function stackOf(c) {
    return Number.isFinite(c.stack) ? c.stack : -Infinity;
}

// Returns the id of the window focus should move to from `focused`, or null when there is none. Focus never wraps
// round: with nothing in that direction the answer is null.
export function pickNeighbour(focused, windows, direction) {
    if (!isDirection(direction) || focused === null || typeof focused !== 'object' || !validRect(focused.rect) ||
        !Array.isArray(windows))
        return null;
    const f = focused.rect;
    const candidates = windows.filter(c => eligible(c, focused));

    let best = null;
    for (const c of candidates) {
        if (!beyondEdge(c.rect, f, direction))
            continue;
        const overlap = crossOverlap(c.rect, f, direction);
        if (overlap <= 0)
            continue;
        const score = {c, gap: edgeGap(c.rect, f, direction), overlap, stack: stackOf(c)};
        if (best === null || score.gap < best.gap ||
            (score.gap === best.gap && (score.overlap > best.overlap ||
                (score.overlap === best.overlap && score.stack > best.stack))))
            best = score;
    }
    if (best !== null)
        return best.c.id;

    // Nothing lines up with the focused window on the other axis: take the nearest window that is at least mostly on
    // that side, judged by its centre.
    const fc = centre(f);
    let fallback = null;
    for (const c of candidates) {
        if (!centreBeyond(c.rect, f, direction))
            continue;
        const m = centre(c.rect);
        const score = {c, distance: Math.hypot(m.x - fc.x, m.y - fc.y), stack: stackOf(c)};
        if (fallback === null || score.distance < fallback.distance ||
            (score.distance === fallback.distance && score.stack > fallback.stack))
            fallback = score;
    }
    return fallback === null ? null : fallback.c.id;
}
