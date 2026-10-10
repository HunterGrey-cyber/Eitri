// Who may move focus, decided over plain records so that every rule can be tested outside GNOME Shell. extension.js
// takes a snapshot of the shell's windows, asks `decide`, and only then activates anything.
//
// The rule in one sentence: no method moves focus unless the user has just pressed a key or a button in the focused
// window, and that window belongs to the caller (for the calls that move focus away from it) or to the partner the
// caller named (for the calls that bring focus to the caller, which also need a Wayland window of the caller to have
// gained focus from the user's own key, click or touch within the last half hour, `callerHeldFocus`). So an
// application whose window the user has not put focus in for that long can never take focus: at most it can follow the
// user's own key within half a second. An application the user focused recently can, because the user's key in the
// partner's window is all that is asked of the user. A focus the extension itself gave (an `ActivateOwn` or
// `FocusSelfIfNeighbour` it granted) earns nothing, and neither does one the application then moves on its own
// (`focusGainCredited`), so a caller cannot spend one grant on the next and keep the credit for good.

import {isDirection, pickNeighbour} from './direction.js';

// How recent "just pressed" is, in milliseconds.
export const FRESH_INPUT_MS = 500;

// The longest focus whose input can still be judged: one period of mutter's 32-bit millisecond clock, less a second so
// a press up to FRESH_INPUT_MS old can never alias into the period before.
export const MAX_FOCUS_AGE_MS = 2 ** 32 - 1000;

// How long after the extension's own activation of a window device input still does not count for it. A key pressed
// in another window just before the extension moved focus is usually released 50-150 ms later, and that release lands
// in the newly focused window; the idle monitor cannot tell a release from a press. A human's next deliberate key in
// the new window comes later than this.
export const SETTLE_MS = 300;

// The editor's process chain is short; the cap only bounds what one caller can make the shell store.
export const MAX_PARTNER_PIDS = 64;

// How long after a Wayland window of the caller last held a focus the user gave it the caller may still bring itself
// forward on the partner's keystroke. The two calls that do that (ActivateOwn, FocusSelfIfNeighbour) name a partner
// the caller chose, so without this a process that never had focus could name the window the user is typing in and
// take their next keys; with it the caller must be an application the user has recently put focus in.
// Only a focus gain made while the shell handles the user's key, button or touch event starts a record, and never one
// the extension caused (`focusGainCredited`): a click, Alt+Tab, a shell shortcut. A window a client brings up or
// activates itself (a new window focused when it maps, a dialog, an activation request), focus falling back after a
// client closed its window, and the extension's own activations all earn nothing. A window holding such a focus holds
// no credit while it keeps it, and losing it does not renew an older record, so a grant can never pay for the next
// one, whatever the caller does with the focus it was given. Cost: a panel reached only through Ctrl+h/j/k/l and the
// `:EitriPanel` raise must be clicked (or otherwise focused by the user) at least once in this long, or both are
// refused, silently, until it is; so must a panel that has just opened. The record is also lost whenever the
// extension is disabled, which GNOME does at every screen lock, and the focus found at enable earns nothing (its cause
// cannot be told): after an unlock both are refused until the user focuses the panel again.
export const CALLER_FOCUS_MEMORY_MS = 30 * 60 * 1000;

// How long after any activation by the extension a focus gain is still taken as one it may have caused, whichever
// window it lands on. An activation can put focus somewhere other than the window it named (mutter focuses a window's
// modal dialog instead of the window, or the client opens one as it is activated), and that gain carries no mark of
// the extension: it earns no credit and counts as the extension's own (`focusGainOrigin`). A user's click in this time
// is taken the same way. A second click in the same window changes no focus, so that window counts as the extension's
// until focus leaves it and the user brings it back; when the activation started from that very window, its
// ActivatePartner then goes nowhere until they do.
export const OWN_ACTIVATION_SHADOW_MS = 1000;

export const METHODS = Object.freeze(['FocusDirection', 'FocusSelfIfNeighbour', 'ActivateOwn', 'ActivatePartner']);

// The variable in gnome-shell's own environment that lets extension.js load testing.js. Its methods let any process on
// the session bus type into the focused window, so the file being present is not enough: a developer's symlinked
// checkout, or a copy left in an install directory, would expose them in a real session. Only the sandbox harness
// starts gnome-shell with this set; a real session never does.
export const TESTING_ENV = 'EITRI_SHELL_EXTENSION_TESTING';

// Whether the test-only methods are wanted, given the variable's value (null when it is unset): exactly "1".
export function testingMethodsWanted(value) {
    return value === '1';
}

const TWO_32 = 2 ** 32;

// The shell's timestamps are unsigned 32-bit milliseconds, and 0 means "no time".
function isTimestamp(n) {
    return Number.isInteger(n) && n > 0 && n < TWO_32;
}

// A reading of the extension's own monotonic millisecond clock: 64 bits wide in effect, so it never wraps.
function isMonotonicMs(n) {
    return Number.isSafeInteger(n) && n > 0;
}

function isPid(n) {
    return Number.isInteger(n) && n > 0 && n < TWO_32;
}

// Whether the window's last key or button press is fresh input to it:
// - only a Wayland window counts, because an X11 client writes its own user time and could claim input it never had;
// - the press is at most FRESH_INPUT_MS old, and not in the future: an event's timestamp is its own, and a synthetic
//   one can carry any time, so both bounds are needed;
// - the press came strictly after the window last gained focus. The click that focuses a window, an activation by
//   this extension, and input from before focus left and came back all fail this;
// - and a real device event happened within FRESH_INPUT_MS, strictly after the focus gain. The user time alone is not
//   proof of input: the focused client can move its own window's user time to "now" by asking the shell to activate
//   that window (an xdg-activation request with a token it holds), which mutter honours for a window that already has
//   focus. `idleMs` comes from mutter's core idle monitor, which only real input devices reset; no client request
//   does. A key or click goes to the focused window, so a device event after the gain, together with a fresh user
//   time, is input the user gave this window;
// - when the extension itself gave the window focus (`focusGainedByUs`, anything but a plain false counting as yes),
//   the device event must also be more than SETTLE_MS after that gain, so the release of the key that led to the
//   activation does not count. Only a gain the user made (a click, Alt+Tab: `focusGainOrigin`) has no such wait; one
//   with no input event behind it, such as focus falling back after a window closed, waits too.
// What remains: any real device event while the window has focus counts as activity, so pointer motion over it, or a
// deliberate key in it after SETTLE_MS, can stand in for a press if the client moves its user time at the same moment;
// the user is then interacting with that window. A late release after the extension's own activation can too, which
// is why ActivatePartner from such a window may only send focus back where it came from (`mayLeavePulledFocusFor`).
// mutter's `user_time` is a 32-bit millisecond timestamp, the low 32 bits of the monotonic clock. The extension
// records focus gains, and reads `now64`, on that same monotonic clock at full width, so "after the focus gain" is an
// exact comparison of two 64-bit times: the input's age is taken in 32 bits (a press is at most FRESH_INPUT_MS old,
// far less than the 32-bit range), and subtracting it from `now64` puts the input on the 64-bit clock. Comparing
// 32-bit times directly would be ambiguous once a window has had focus for half or a whole period of the 32-bit clock
// (about 25 or 50 days). A focus gain later than `now64` cannot come from one monotonic clock, and is refused.
//
// The 32-bit `user_time` itself still says nothing about which period of its clock it belongs to: a window that has
// kept focus, with no new input, for a whole period would show the stamp of its last press (or of the activation that
// focused it) as a few milliseconds old again. Any input that counts came after the focus gain, so while the focus is
// younger than MAX_FOCUS_AGE_MS its real age is below one period and the 32-bit age is exact. A focus older than that
// counts nothing until the window is focused again.
export function isFreshInput({wayland, userTime, focusGained64, focusGainedByUs, now64, idleMs}) {
    if (wayland !== true)
        return false;
    if (!isTimestamp(userTime) || !isMonotonicMs(now64) || !isMonotonicMs(focusGained64))
        return false;
    if (focusGained64 > now64)
        return false;
    if (now64 - focusGained64 >= MAX_FOCUS_AGE_MS)
        return false;
    const inputAge = (now64 - userTime) >>> 0;
    if (inputAge > FRESH_INPUT_MS)
        return false;
    if (now64 - inputAge <= focusGained64)
        return false;
    if (!Number.isSafeInteger(idleMs) || idleMs < 0 || idleMs > FRESH_INPUT_MS)
        return false;
    const settle = focusGainedByUs === false ? 0 : SETTLE_MS;
    return now64 - idleMs > focusGained64 + settle;
}

// The pids a caller names as its editor's process chain, cleaned before they are stored: no 0 or 1, never the shell
// itself, never the caller (its own window is not its partner), no repeats, order kept, at most MAX_PARTNER_PIDS.
export function cleanPartnerList(pids, {shellPid, callerPid}) {
    if (!Array.isArray(pids))
        return [];
    const out = [];
    const seen = new Set();
    for (const pid of pids) {
        if (out.length >= MAX_PARTNER_PIDS)
            break;
        if (!isPid(pid) || pid <= 1 || pid === shellPid || pid === callerPid || seen.has(pid))
            continue;
        seen.add(pid);
        out.push(pid);
    }
    return out;
}

// The partner is the first pid in the chain that owns a window right now: the caller cannot list windows, so it names
// the editor's whole ancestry and the shell picks the one that actually has the window (a terminal hosting nvim, or
// Neovide itself). With `ownsPreferred` the chain is read twice: first for a pid that passes it, and only when none
// does, for one that passes `ownsWindow`. The chain starts at the editor itself and its shell, which own no window; a
// window of another application that claims one of those pids would otherwise be reached before the terminal's own.
export function partnerPid(chain, ownsWindow, ownsPreferred = null) {
    if (!Array.isArray(chain))
        return null;
    const passes = typeof ownsPreferred === 'function' ? [ownsPreferred, ownsWindow] : [ownsWindow];
    for (const owns of passes) {
        for (const pid of chain) {
            if (isPid(pid) && owns(pid) === true)
                return pid;
        }
    }
    return null;
}

// Whether a focus gain at `now64` starts a record for `callerHeldFocus`: only when the user made it and the extension
// did not. `userInput` is whether the shell was handling a key, button or touch event from an input device when focus
// changed (no client request can make one); `atEnable` whether this is the focus the extension found when it started;
// `gainedByUs` whether a pending activation by the extension named this window; `lastOwnActivation64` when the
// extension last activated any window, null when it never has. Each flag must be a plain true or false as stated, or
// the gain is not credited; nor is one within OWN_ACTIVATION_SHADOW_MS of that activation, or one with a clock reading
// that cannot be trusted.
export function focusGainCredited({userInput, atEnable, gainedByUs, now64, lastOwnActivation64}) {
    if (userInput !== true || atEnable !== false || gainedByUs !== false || !isMonotonicMs(now64))
        return false;
    if (lastOwnActivation64 === null)
        return true;
    if (!isMonotonicMs(lastOwnActivation64) || lastOwnActivation64 > now64)
        return false;
    return now64 - lastOwnActivation64 >= OWN_ACTIVATION_SHADOW_MS;
}

// Whose a focus gain at `now64` is, for the rules that treat the extension's own gains apart (SETTLE_MS in
// `isFreshInput`, `mayLeavePulledFocusFor`): {byUs, from}. The gain is the user's ({byUs: false, from: 0}) exactly when
// it is credited (`focusGainCredited`); every other gain is the extension's own, since its cause cannot be told apart
// from one: an activation can put focus on a window other than the one it named (mutter focuses the named window's
// modal dialog instead, or the client opens one as it is activated), and that gain carries no mark of the extension.
// `from` is the pid whose window had focus when the activation was made, the only place ActivatePartner may then send
// focus: the pending activation's own (`gainedFrom`) when one named this window, else that of the extension's last
// activation (`lastOwnActivationFrom`) when the gain is within OWN_ACTIVATION_SHADOW_MS of it, else 0, which lets
// ActivatePartner go nowhere until the user focuses the window. A clock reading that cannot be trusted ties the gain to
// no activation either.
// A pending activation (`gainedByUs`, made at `gainedAt64`) names the gain only within OWN_ACTIVATION_SHADOW_MS of it:
// its notify comes during the activation or just after. One left waiting longer (focus went to the window's modal
// dialog instead) says nothing about a later gain, which is then judged like any other: a click is the user's, focus
// falling back to the window when the dialog closes is the extension's with no source. Kept for good, it would bind
// that fallback to the old activation's source, and swallow the credit of the user's next click into the window.
export function focusGainOrigin({
    userInput, atEnable, gainedByUs, gainedAt64, gainedFrom, now64, lastOwnActivation64, lastOwnActivationFrom,
}) {
    let pending = gainedByUs;
    if (gainedByUs === true) {
        if (!isMonotonicMs(gainedAt64) || !isMonotonicMs(now64) || gainedAt64 > now64)
            return {byUs: true, from: 0};
        pending = now64 - gainedAt64 < OWN_ACTIVATION_SHADOW_MS;
    }
    if (focusGainCredited({userInput, atEnable, gainedByUs: pending, now64, lastOwnActivation64}))
        return {byUs: false, from: 0};
    let from = 0;
    if (pending === true)
        from = gainedFrom;
    else if (isMonotonicMs(now64) && isMonotonicMs(lastOwnActivation64) && lastOwnActivation64 <= now64 &&
        now64 - lastOwnActivation64 < OWN_ACTIVATION_SHADOW_MS)
        from = lastOwnActivationFrom;
    return {byUs: true, from: isPid(from) ? from : 0};
}

// Whether the caller held focus in a Wayland window of its own within CALLER_FOCUS_MEMORY_MS, on the extension's
// monotonic clock (`callerFocusedAt64`: the last moment such a window held a focus the user gave it, or now while it
// holds one). Anything missing, in the future or older than the memory is no.
export function callerHeldFocus({callerFocusedAt64, now64}) {
    if (!isMonotonicMs(callerFocusedAt64) || !isMonotonicMs(now64) || callerFocusedAt64 > now64)
        return false;
    return now64 - callerFocusedAt64 <= CALLER_FOCUS_MEMORY_MS;
}

// The bookkeeping behind `callerFocusedAt64`, kept here as plain data so it can be tested outside GNOME Shell.
// `held` is {records: Map<window id, {pid, at}>, last: window id | null}: per window, never per pid, so a process that
// gets a closed application's pid number inherits nothing; `last` is the window that has focus right now, if it is one
// that counts and the user gave it that focus.

// How many records may build up before the ones older than the memory are dropped. Windows that ever had focus are
// few; this only bounds what a long-running session accumulates.
export const FOCUS_RECORDS_TRIM_AT = 64;

export function newHeldFocus() {
    return {records: new Map(), last: null};
}

// Records a focus change at `now` (the extension's monotonic clock): the window whose credited focus this ends held it
// up to now, and `focus` (null when no window has it, else {id, pid, wayland}) holds it from now. Only a gain with
// `credited` exactly true (`focusGainCredited`) is recorded; any other leaves the window's earlier record, if it has
// one, as it was, and when that focus ends nothing is stamped. A window that cannot count (an X11 window, whose pid is
// client-written, or one with no pid) is not recorded. Past FOCUS_RECORDS_TRIM_AT records, those older than the
// memory go; the window that has focus now is stamped `now` and so is never among them.
export function noteHeldFocus(held, focus, now, credited) {
    if (held.last !== null)
        held.records.set(held.last, {...held.records.get(held.last), at: now});
    held.last = null;
    if (credited !== true || !focus || focus.wayland !== true || !isPid(focus.pid))
        return;
    held.records.set(focus.id, {pid: focus.pid, at: now});
    held.last = focus.id;
    if (held.records.size > FOCUS_RECORDS_TRIM_AT) {
        for (const [id, record] of [...held.records]) {
            if (now - record.at > CALLER_FOCUS_MEMORY_MS)
                held.records.delete(id);
        }
    }
}

// The latest moment at which a window held a credited focus, among the windows that exist now (`windows`, the
// snapshot's records), belong to the caller's pid both in the record and now, and are Wayland windows; `now64` while
// one still holds it. Undefined when there is none.
export function callerFocusedAt(held, callerPid, windows, now64) {
    let latest;
    for (const w of windows) {
        const record = held.records.get(w.id);
        if (!record || record.pid !== callerPid || w.pid !== callerPid || w.wayland !== true)
            continue;
        const at = w.id === held.last ? now64 : record.at;
        if (latest === undefined || at > latest)
            latest = at;
    }
    return latest;
}

// Moving focus away from the caller's own window: the caller must be focused and the user must just have pressed a
// key or button in it.
export function mayFocusDirection({focusedPid, callerPid, freshInput}) {
    return isPid(callerPid) && focusedPid === callerPid && freshInput === true;
}

// Bringing focus to the caller: the partner must be focused and the user must just have pressed a key or button in
// it.
export function mayActOnPartnerInput({focusedPid, partnerPid: partner, freshInput}) {
    return isPid(partner) && focusedPid === partner && freshInput === true;
}

// Whether ActivatePartner may hand focus to `partnerPid` from a window this extension itself focused. Fresh input
// cannot fully vouch for such a window: device input that lands in it after the settling time, such as the late
// release of a modifier held while the user pressed the key that caused the activation, reads as a press, and the
// client can move its own user time to that moment. Were the partner free there, the caller could name any pid after
// being focused and send focus to it. So the partner must be the pid whose window had focus when the activation was
// made: the window the user was in. When the caller brought itself forward (ActivateOwn, FocusSelfIfNeighbour), that
// is exactly the partner as it was resolved then, so the panel's own flows, which name the same editor before and
// after a raise, still hand focus back. A gain the user made (a click, Alt+Tab) leaves the partner free; a gain of
// unknown origin, or with no recorded source, is taken as the extension's own and allows nothing.
export function mayLeavePulledFocusFor({focusGainedByUs, focusGainedFrom, partnerPid: partner}) {
    if (focusGainedByUs === false)
        return true;
    return isPid(focusGainedFrom) && isPid(partner) && partner === focusGainedFrom;
}

// Whether a window is one the user switches to and types into: a normal window or a dialog, not hidden from the
// taskbar. Judged from the window's own type and flags, so a utility window, a menu or a panel is never "the editor"
// or "the caller's window", whatever mutter's lists hold. Dialogs are included because the user works in them (a save
// dialog, a terminal's preferences) and moves focus to them by direction; this one test is what both a direction move
// and the partner and activation targets use, so the two never disagree about which windows a pid has.
export function isUserWindow(w) {
    return (w.type === 'normal' || w.type === 'dialog') && w.skipTaskbar === false;
}

function mruPlace(w) {
    return Number.isInteger(w.mru) && w.mru >= 0 ? w.mru : Infinity;
}

function stackPlace(w) {
    return Number.isFinite(w.stack) ? w.stack : -Infinity;
}

// Whether `w` is one of the windows that stand for `pid`. A Wayland window's pid is the connection's, which the
// compositor knows; an X11 window's is a property the client writes itself, so an Xwayland client could claim another
// process's pid. A pid's X11 windows stand for it only when it has no Wayland user window at all, which keeps an editor
// that really runs under X11 reachable without letting an X11 window impersonate a Wayland application.
function standsFor(windows, w, pid) {
    if (w.pid !== pid || !isUserWindow(w))
        return false;
    if (w.wayland === true)
        return true;
    return !windows.some(o => o.pid === pid && o.wayland === true && isUserWindow(o));
}

// The pid's user window to activate: the most recently used of those that stand for it (`standsFor`), and among
// windows the shell's most-recently-used list does not hold, the highest in the stacking order. The list only orders;
// it decides nothing about ownership.
function mostRecentWindowOf(windows, pid) {
    let best = null;
    for (const w of windows) {
        if (!standsFor(windows, w, pid))
            continue;
        if (best === null || mruPlace(w) < mruPlace(best) ||
            (mruPlace(w) === mruPlace(best) && stackPlace(w) > stackPlace(best)))
            best = w;
    }
    return best;
}

function directionRecord(w, focused) {
    return {
        id: w.id,
        rect: w.rect,
        minimized: w.minimized,
        normal: isUserWindow(w),
        onWorkspace: w.onWorkspace,
        onMonitor: Number.isInteger(w.monitor) && w.monitor === focused.monitor,
        stack: w.stack,
    };
}

const refuse = reason => ({act: false, reason});

// Decides one call. The snapshot's windows are
//   {id, pid, wayland, userTime, rect: {x, y, w, h}, minimized, type, skipTaskbar, onWorkspace, monitor, stack, mru}
// where `type` is 'normal', 'dialog' or 'other', `skipTaskbar` the window's own flag, and `mru` the window's place in
// the shell's most-recently-used list (smaller is more recent), or null when it is not in that list. A pid "owns a
// window" when one of its windows passes `isUserWindow`; `mru` only chooses between a pid's windows, and a pid's
// Wayland windows come before any of its X11 ones (`standsFor`).
// `partnerChain` is what the caller last set (already cleaned), `focusGained64` the time the focused window last
// gained focus and `now64` the current time, both on the extension's monotonic millisecond clock, `focusGainedByUs`
// whether that gain counts as the extension's own (any gain the user did not make, `focusGainOrigin`),
// `focusGainedFrom` the pid whose window had focus when the activation it is tied to was made (0 when it is tied to
// none, which lets ActivatePartner go nowhere), `idleMs` the milliseconds since the last real input device event, read together with `now64`,
// and `modal` whether the shell holds a modal grab (the overview, a system dialog, the lock screen).
// Returns {act: true, target: <window id>} or {act: false, reason}.
export function decide({
    method, direction, callerPid, callerFocusedAt64, partnerChain, focusedId, focusGained64, focusGainedByUs,
    focusGainedFrom, now64, idleMs, modal, windows,
}) {
    if (!METHODS.includes(method))
        return refuse('unknown-method');
    const directional = method === 'FocusDirection' || method === 'FocusSelfIfNeighbour';
    if (directional && !isDirection(direction))
        return refuse('bad-direction');
    if (!isPid(callerPid))
        return refuse('no-caller');
    // While the shell itself has the keyboard, the user is not typing into any window, whatever the focused window's
    // last input says.
    if (modal !== false)
        return refuse('shell-modal');
    if (!Array.isArray(windows))
        return refuse('no-focus');
    const all = windows.filter(w => w !== null && typeof w === 'object');
    const focused = focusedId === null || focusedId === undefined ? undefined : all.find(w => w.id === focusedId);
    if (!focused)
        return refuse('no-focus');

    const freshInput = isFreshInput({
        wayland: focused.wayland, userTime: focused.userTime, focusGained64, focusGainedByUs, now64, idleMs,
    });

    if (method === 'FocusDirection' || method === 'ActivatePartner') {
        if (focused.pid !== callerPid)
            return refuse('focus-not-caller');
        if (!mayFocusDirection({focusedPid: focused.pid, callerPid, freshInput}))
            return refuse('no-fresh-input');
        if (method === 'FocusDirection') {
            const id = pickNeighbour(directionRecord(focused, focused), all.map(w => directionRecord(w, focused)),
                direction);
            return id === null ? refuse('nothing-there') : {act: true, target: id};
        }
        const partner = resolvePartner(all, partnerChain, callerPid);
        if (partner === null)
            return refuse('no-partner');
        if (!mayLeavePulledFocusFor({focusGainedByUs, focusGainedFrom, partnerPid: partner}))
            return refuse('partner-changed');
        return {act: true, target: mostRecentWindowOf(all, partner).id};
    }

    // The caller chose its partner, so it must also be an application the user has been in lately.
    if (!callerHeldFocus({callerFocusedAt64, now64}))
        return refuse('caller-not-focused-lately');
    const partner = resolvePartner(all, partnerChain, callerPid);
    if (partner === null)
        return refuse('no-partner');
    if (focused.pid !== partner)
        return refuse('focus-not-partner');
    if (!mayActOnPartnerInput({focusedPid: focused.pid, partnerPid: partner, freshInput}))
        return refuse('no-fresh-input');

    if (method === 'FocusSelfIfNeighbour') {
        const id = pickNeighbour(directionRecord(focused, focused), all.map(w => directionRecord(w, focused)),
            direction);
        if (id === null)
            return refuse('nothing-there');
        // Only the caller's own window may be the one focus moves to; otherwise the caller would learn, or steer,
        // where some other window sits.
        if (!standsFor(all, all.find(w => w.id === id), callerPid))
            return refuse('neighbour-not-caller');
        return {act: true, target: id};
    }

    const own = mostRecentWindowOf(all, callerPid);
    return own === null ? refuse('no-own-window') : {act: true, target: own.id};
}

// The partner pid for a call: the first pid of the chain with a Wayland user window, else the first with any user
// window (an editor that really runs under X11). Wayland windows are judged by the compositor's pid; an X11 window's
// pid is client-written, so it may name a pid only when nothing in the whole chain has a Wayland window.
function resolvePartner(all, partnerChain, callerPid) {
    return partnerPid(
        chainWithout(partnerChain, callerPid),
        pid => mostRecentWindowOf(all, pid) !== null,
        pid => all.some(w => w.pid === pid && w.wayland === true && isUserWindow(w)),
    );
}

function chainWithout(chain, pid) {
    return Array.isArray(chain) ? chain.filter(p => p !== pid) : [];
}
