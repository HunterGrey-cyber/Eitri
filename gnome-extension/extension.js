// Lets Eitri's panel move keyboard focus between itself and its editor's window on GNOME, where an application cannot
// focus another application's window by itself, and lets an Eitri window put itself back on the monitor it was on.
// It exports cn.huntergrey.Eitri.Shell1 on the session bus; every method answers a boolean and nothing else, so a
// caller learns nothing about other windows. Who may move focus, and when, and which windows a monitor move takes,
// are decided in policy.js.
import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

import {
    EXTENSION_VERSION, TESTING_ENV, callerFocusedAt, cleanPartnerList, decide, focusGainOrigin, isConnectorName,
    newHeldFocus, noteHeldFocus, planMoveOwnToMonitor, testingMethodsWanted,
} from './policy.js';

const BUS_NAME = 'cn.huntergrey.Eitri.Shell1';
const OBJECT_PATH = '/cn/huntergrey/Eitri/Shell1';

const METHODS_XML = `
    <method name="Version"><arg type="u" direction="out" name="version"/></method>
    <method name="FocusDirection"><arg type="s" direction="in" name="direction"/><arg type="b" direction="out" name="moved"/></method>
    <method name="FocusSelfIfNeighbour"><arg type="s" direction="in" name="direction"/><arg type="b" direction="out" name="moved"/></method>
    <method name="SetPartner"><arg type="au" direction="in" name="pids"/><arg type="b" direction="out" name="recorded"/></method>
    <method name="ActivateOwn"><arg type="b" direction="out" name="activated"/></method>
    <method name="ActivatePartner"><arg type="b" direction="out" name="activated"/></method>
    <method name="MoveOwnToMonitor"><arg type="s" direction="in" name="connector"/><arg type="b" direction="out" name="moved"/></method>`;

function interfaceXml(extraMethods) {
    return `<node><interface name="${BUS_NAME}">${METHODS_XML}${extraMethods}</interface></node>`;
}

// The focus-gain map is trimmed to live windows when it grows past this.
const FOCUS_GAINED_TRIM_AT = 64;

function debug(message) {
    console.debug(`Eitri: ${message}`);
}

function pidOf(window) {
    try {
        const pid = window.get_pid();
        return Number.isInteger(pid) && pid > 0 ? pid : 0;
    } catch (e) {
        return 0;
    }
}

function allWindows() {
    const display = global.display;
    if (typeof display.list_all_windows === 'function')
        return display.list_all_windows();
    return global.get_window_actors().map(actor => actor.meta_window);
}

// Mutter's own most-recently-used list across every workspace. It only orders a pid's windows: it can hold utility
// windows too, so which windows count is decided from each window's own type and flags.
function tabList() {
    const kind = Meta.TabList.NORMAL_ALL_MRU ?? Meta.TabList.NORMAL_ALL;
    return global.display.get_tab_list(kind, null);
}

// The monotonic clock in milliseconds, at full width. mutter's own timestamps (`get_current_time_roundtrip()`, every
// window's `user_time`) are this clock cut to 32 bits; keeping the full value lets policy.js order a focus gain and a
// key press without the 32-bit clock's wrap.
function monotonicMs() {
    return Math.floor(GLib.get_monotonic_time() / 1000);
}

// Milliseconds since the last event from a real input device (keyboard, pointer, touch), from mutter's core idle
// monitor, or undefined when it cannot be read. Clients cannot reset it: an activation request, which can move a
// window's user time, does not touch it.
function idleMs() {
    try {
        const idle = global.backend.get_core_idle_monitor().get_idletime();
        return Number.isSafeInteger(idle) ? idle : undefined;
    } catch (e) {
        return undefined;
    }
}

// The events through which a user moves focus: a click or a touch on a window, a key (Alt+Tab switches on the
// release). No Wayland or X11 client request produces them; an input device does, and so does a remote-desktop
// virtual device, which also resets the idle monitor that the fresh-input rule trusts.
const USER_INPUT_EVENTS = [
    Clutter.EventType.BUTTON_PRESS, Clutter.EventType.BUTTON_RELEASE, Clutter.EventType.KEY_PRESS,
    Clutter.EventType.KEY_RELEASE, Clutter.EventType.TOUCH_BEGIN, Clutter.EventType.TOUCH_END,
];

// Whether the shell is handling such an event right now. A focus change made while it does is the user's: the click
// that focuses a window, the key that ends Alt+Tab. The credit goes to whichever window mutter focuses then, which is
// usually the one the user chose; a shortcut that minimizes the focused window credits the window focus falls back to. One made from a client's request (a new window, an activation
// token), from a D-Bus call (this extension's own activations) or from a timeout is not.
function handlingUserInput() {
    try {
        const event = Clutter.get_current_event();
        return event !== null && USER_INPUT_EVENTS.includes(event.type());
    } catch (e) {
        return false;
    }
}

// mutter's index of the monitor whose connector is `connector`, or -1 when no monitor has it or it cannot be read.
function monitorIndexFor(connector) {
    try {
        const index = global.backend.get_monitor_manager().get_monitor_for_connector(connector);
        return Number.isInteger(index) ? index : -1;
    } catch (e) {
        return -1;
    }
}

// The window's type as policy.js reads it.
function kindOf(window) {
    switch (window.get_window_type()) {
    case Meta.WindowType.NORMAL: return 'normal';
    case Meta.WindowType.DIALOG: return 'dialog';
    default: return 'other';
    }
}

// The service behind the bus object. One per enable(); destroy() leaves nothing behind.
class EitriShell {
    constructor() {
        this._destroyed = false;
        this._cancellable = new Gio.Cancellable();
        this._shellPid = new Gio.Credentials().get_unix_pid();
        // caller unique name -> {pids, watchId}
        this._partners = new Map();
        // window id -> {at, byUs, pending, from}: when the window last gained focus, or this extension last activated
        // it (monotonicMs()); whether that gain counts as this extension's own (any gain the user did not make,
        // policy.js `focusGainOrigin`); whether that activation's focus notify is still to come; and, for the
        // extension's own, the pid whose window had focus when its activation was made (0 when none, when the gain
        // cannot be tied to an activation, or for a gain the user made).
        this._focusGained = new Map();
        // Which Wayland windows held a focus the user gave them, and when (policy.js, `noteHeldFocus`/
        // `callerFocusedAt`): the input to `callerHeldFocus`. Lost with the extension, as a screen lock disables it.
        this._held = newHeldFocus();
        // When this extension last activated any window (monotonicMs()) and the pid whose window had focus then, or
        // null: a focus gain soon after it is not credited and counts as that activation's, wherever it lands
        // (policy.js, `focusGainOrigin`).
        this._lastOwnActivation = null;
        this._signals = [];
        this._exported = null;
        this._ownId = 0;
        this._testingTeardown = null;

        // Whatever has focus now gained it, as far as this extension can tell, now: input from before enable() does
        // not count. Who gave it that focus cannot be told, so it earns no credit.
        this._noteFocus({atEnable: true});
        this._signals.push([global.display,
            global.display.connect('notify::focus-window', () => this._noteFocus())]);
    }

    // Exports the object and owns the name. `testing` is the test-only module when it is loaded, else null.
    start(testing) {
        if (this._destroyed)
            return;
        let extra = '';
        if (testing) {
            extra = testing.METHODS_XML;
            this._testingTeardown = testing.install(this);
        }
        this._exported = Gio.DBusExportedObject.wrapJSObject(interfaceXml(extra), this);
        this._exported.export(Gio.DBus.session, OBJECT_PATH);
        // Not replaceable: once the shell holds the name, nothing else on the bus can take it over.
        this._ownId = Gio.bus_own_name_on_connection(Gio.DBus.session, BUS_NAME, Gio.BusNameOwnerFlags.NONE,
            null, () => debug(`${BUS_NAME} is not ours (owned elsewhere, or the bus went away)`));
    }

    destroy() {
        this._destroyed = true;
        this._cancellable.cancel();
        if (this._ownId) {
            Gio.bus_unown_name(this._ownId);
            this._ownId = 0;
        }
        if (this._exported) {
            this._exported.unexport();
            this._exported = null;
        }
        for (const [object, id] of this._signals)
            object.disconnect(id);
        this._signals = [];
        for (const entry of this._partners.values())
            Gio.bus_unwatch_name(entry.watchId);
        this._partners.clear();
        this._focusGained.clear();
        this._held = newHeldFocus();
        if (this._testingTeardown) {
            this._testingTeardown();
            this._testingTeardown = null;
        }
    }

    // An activation by this extension fires this notify itself, during Main.activateWindow() or a little later, so
    // it must not undo what activate() recorded: a record still waiting for its notify makes this gain the
    // extension's own. So does every gain the user did not make (policy.js, `focusGainOrigin`): one within a second
    // of any activation of the extension's, wherever it lands (the named window's modal dialog, a dialog its client
    // opens on being activated), and one with no key, button or touch event behind it. Only a click, Alt+Tab or a
    // shell shortcut is the user's. A record left waiting because an activation never gave the window focus names
    // only a gain within that second; a later one is judged on its own.
    // The source of focus is kept from the activation's record, or taken from the activation whose second this gain
    // falls in, so the gain stays bound to the window focus left; a gain tied to no activation has none.
    _noteFocus({atEnable = false} = {}) {
        const window = global.display.focus_window;
        const at = monotonicMs();
        const previous = window ? this._focusGained.get(window.get_id()) : undefined;
        const origin = focusGainOrigin({
            userInput: handlingUserInput(), atEnable, gainedByUs: previous?.pending === true, gainedAt64: previous?.at,
            gainedFrom: previous?.from, now64: at,
            // Only "never activated" is null: a record without a time must reach policy.js as one and be refused.
            lastOwnActivation64: this._lastOwnActivation === null ? null : this._lastOwnActivation.at,
            lastOwnActivationFrom: this._lastOwnActivation?.from,
        });
        const credited = origin.byUs === false;
        this._noteHeldFocus(window, at, credited);
        if (!window)
            return;
        debug(`focus to window ${window.get_id()} of pid ${pidOf(window)}: ${credited ? 'credited' : 'not credited'}`);
        if (origin.byUs)
            debug(`focus gain of window ${window.get_id()} counts as the extension's own, from pid ${origin.from}`);
        this._recordFocusGained(window, {at, byUs: origin.byUs, pending: false, from: origin.from});
    }

    // Records the focus change for `callerHeldFocus`: kept per window id, which the shell never reuses, and not per
    // pid, so a process that gets a closed application's pid number must not inherit the time that application's
    // window held focus. Only a gain the user made (`credited`) starts a record.
    _noteHeldFocus(window, at, credited) {
        const focus = window ? {
            id: window.get_id(),
            pid: pidOf(window),
            wayland: window.get_client_type() === Meta.WindowClientType.WAYLAND,
        } : null;
        noteHeldFocus(this._held, focus, at, credited);
    }

    _recordFocusGained(window, record) {
        this._focusGained.set(window.get_id(), record);
        if (this._focusGained.size > FOCUS_GAINED_TRIM_AT) {
            const live = new Set(allWindows().map(w => w.get_id()));
            for (const id of [...this._focusGained.keys()]) {
                if (!live.has(id))
                    this._focusGained.delete(id);
            }
        }
    }

    // Activating a window sets its user time to the activation's timestamp, and on a window that already has focus
    // no focus change follows. Recording the activation as a focus gain, before activating, keeps the extension's own
    // activations from ever looking like the user's input: the clock is read for the gain only after the activation's
    // timestamp was taken, so the gain is never earlier than that timestamp, and "strictly after the gain" excludes it.
    // The record also says the gain is the extension's own, so that input in the first moments after it counts only
    // once it has settled (policy.js); on a window that already has focus no notify will follow to complete it.
    // It also names the pid whose window has focus now: from the activated window, ActivatePartner may only hand focus
    // back there (policy.js). Activating the window that already has focus names its own pid, which is never a
    // caller's partner, so ActivatePartner from it waits until the user focuses it again; only the test harness does
    // that. The activation's time is noted before it is made, since its focus notify can fire inside
    // Main.activateWindow(), so that no gain it causes, on this window or another, is credited to the user.
    activate(window) {
        const time = global.display.get_current_time_roundtrip();
        const focus = global.display.focus_window;
        const pending = focus !== window;
        const from = focus ? pidOf(focus) : 0;
        const at = monotonicMs();
        this._lastOwnActivation = {at, from};
        this._recordFocusGained(window, {at, byUs: true, pending, from});
        Main.activateWindow(window, time);
    }

    // The windows as policy.js sees them, plus a way back from an id to the window.
    _snapshot() {
        const workspace = global.workspace_manager.get_active_workspace();
        const mru = new Map();
        tabList().forEach((window, index) => {
            const id = window.get_id();
            if (!mru.has(id))
                mru.set(id, index);
        });
        const byId = new Map();
        const windows = global.display.sort_windows_by_stacking(allWindows()).map((window, stack) => {
            const id = window.get_id();
            byId.set(id, window);
            const r = window.get_frame_rect();
            return {
                id,
                pid: pidOf(window),
                wayland: window.get_client_type() === Meta.WindowClientType.WAYLAND,
                userTime: window.get_user_time(),
                rect: {x: r.x, y: r.y, w: r.width, h: r.height},
                minimized: window.minimized === true,
                type: kindOf(window),
                // Anything but a plain false counts as hidden, so a window whose flag cannot be read is never one.
                skipTaskbar: window.is_skip_taskbar() !== false,
                onWorkspace: window.located_on_workspace(workspace),
                monitor: window.get_monitor(),
                // Read only by MoveOwnToMonitor; anything but a plain false counts as a window with a parent.
                transient: window.get_transient_for() !== null,
                stack,
                mru: mru.has(id) ? mru.get(id) : null,
            };
        });
        const focus = global.display.focus_window;
        const focusedId = focus ? focus.get_id() : null;
        // One clock reading serves as both clocks: policy.js takes its low 32 bits to age the window's 32-bit user
        // time, so the input and the focus gain land on the same 64-bit clock. A method call is handled outside event
        // dispatch, where `get_current_time_roundtrip()` is this clock cut to 32 bits anyway; a second reading could
        // differ from it by a millisecond. The idle reading is taken right after it: read in that order, any delay
        // between the two only makes the last device event look older, never newer.
        const now64 = monotonicMs();
        const idle = idleMs();
        return {
            windows,
            byId,
            focusedId,
            now64,
            idleMs: idle,
            focusGained64: focusedId === null ? undefined : this._focusGained.get(focusedId)?.at,
            focusGainedByUs: focusedId === null ? undefined : this._focusGained.get(focusedId)?.byUs,
            focusGainedFrom: focusedId === null ? undefined : this._focusGained.get(focusedId)?.from,
        };
    }

    // The pid of the process on the other end of the caller's bus connection, as the bus daemon saw it connect. A
    // helper process the caller spawns (gdbus, busctl) has its own connection and so its own pid.
    _callerPid(invocation) {
        return new Promise(resolve => {
            const sender = invocation.get_sender();
            if (!sender || this._destroyed) {
                resolve(0);
                return;
            }
            invocation.get_connection().call('org.freedesktop.DBus', '/org/freedesktop/DBus',
                'org.freedesktop.DBus', 'GetConnectionUnixProcessID', new GLib.Variant('(s)', [sender]),
                new GLib.VariantType('(u)'), Gio.DBusCallFlags.NONE, 1000, this._cancellable,
                (connection, result) => {
                    try {
                        const [pid] = connection.call_finish(result).deepUnpack();
                        resolve(Number.isInteger(pid) && pid > 0 ? pid : 0);
                    } catch (e) {
                        resolve(0);
                    }
                });
        });
    }

    // Runs `body(callerPid, sender)` once the caller's pid is known and replies with its boolean. Every failure,
    // including the extension being disabled meanwhile, is a `false` reply, never a D-Bus error.
    _answer(method, invocation, body) {
        let replied = false;
        const reply = value => {
            if (replied)
                return;
            replied = true;
            invocation.return_value(new GLib.Variant('(b)', [value === true]));
        };
        this._callerPid(invocation).then(pid => {
            if (this._destroyed || pid === 0) {
                debug(`${method}: refused (caller unknown)`);
                reply(false);
                return;
            }
            reply(body(pid, invocation.get_sender()));
        }).catch(e => {
            debug(`${method}: refused (${e})`);
            reply(false);
        });
    }

    _act(method, direction, callerPid, sender) {
        const snapshot = this._snapshot();
        const verdict = decide({
            method,
            direction,
            callerPid,
            // Still holding focus counts as now, however long ago it began.
            callerFocusedAt64: callerFocusedAt(this._held, callerPid, snapshot.windows, snapshot.now64),
            partnerChain: this._partners.get(sender)?.pids ?? [],
            focusedId: snapshot.focusedId,
            focusGained64: snapshot.focusGained64,
            focusGainedByUs: snapshot.focusGainedByUs,
            focusGainedFrom: snapshot.focusGainedFrom,
            now64: snapshot.now64,
            idleMs: snapshot.idleMs,
            modal: Main.modalCount > 0,
            windows: snapshot.windows,
        });
        if (!verdict.act) {
            debug(`${method}${direction ? ` ${direction}` : ''} from pid ${callerPid}: refused (${verdict.reason})`);
            return false;
        }
        const window = snapshot.byId.get(verdict.target);
        if (!window)
            return false;
        this.activate(window);
        debug(`${method}${direction ? ` ${direction}` : ''} from pid ${callerPid}: done`);
        return true;
    }

    Version() {
        return EXTENSION_VERSION;
    }

    // Puts the caller's own top-level windows on the monitor whose connector it names (policy.js,
    // `planMoveOwnToMonitor`). Answers whether it asked mutter to move every one of them (or found it there already):
    // false for a connector no monitor has, for a caller with no such window, and when a move threw. It does not read
    // the monitor back: a move that changes a Wayland window's size lands only once the client acks the configure,
    // after this call has answered, so a read-back would call a move that is under way a failure.
    _moveOwnToMonitor(connector, callerPid) {
        // A string that cannot be a connector reaches neither mutter nor the log.
        const monitorIndex = isConnectorName(connector) ? monitorIndexFor(connector) : -1;
        const snapshot = this._snapshot();
        const plan = planMoveOwnToMonitor({
            callerPid, connector, monitorIndex, monitorCount: global.display.get_n_monitors(),
            windows: snapshot.windows,
        });
        if (!plan.act) {
            debug(`MoveOwnToMonitor from pid ${callerPid}: refused (${plan.reason})`);
            return false;
        }
        let moved = 0;
        for (const id of plan.targets) {
            const window = snapshot.byId.get(id);
            if (!window)
                continue;
            try {
                if (window.get_monitor() !== plan.monitor)
                    window.move_to_monitor(plan.monitor);
                moved += 1;
            } catch (e) {
                debug(`MoveOwnToMonitor from pid ${callerPid}: window ${id} not moved (${e})`);
            }
        }
        debug(`MoveOwnToMonitor from pid ${callerPid}: ${moved} of ${plan.targets.length} windows on ${connector}`);
        return moved > 0 && moved === plan.targets.length;
    }

    MoveOwnToMonitorAsync([connector], invocation) {
        this._answer('MoveOwnToMonitor', invocation, pid => this._moveOwnToMonitor(connector, pid));
    }

    FocusDirectionAsync([direction], invocation) {
        this._answer('FocusDirection', invocation,
            (pid, sender) => this._act('FocusDirection', direction, pid, sender));
    }

    FocusSelfIfNeighbourAsync([direction], invocation) {
        this._answer('FocusSelfIfNeighbour', invocation,
            (pid, sender) => this._act('FocusSelfIfNeighbour', direction, pid, sender));
    }

    ActivateOwnAsync(_params, invocation) {
        this._answer('ActivateOwn', invocation, (pid, sender) => this._act('ActivateOwn', undefined, pid, sender));
    }

    ActivatePartnerAsync(_params, invocation) {
        this._answer('ActivatePartner', invocation,
            (pid, sender) => this._act('ActivatePartner', undefined, pid, sender));
    }

    SetPartnerAsync([pids], invocation) {
        this._answer('SetPartner', invocation, (callerPid, sender) => {
            this._setPartner(sender, invocation.get_connection(),
                cleanPartnerList(pids, {shellPid: this._shellPid, callerPid}));
            debug(`SetPartner from pid ${callerPid}: ${this._partners.get(sender)?.pids.length ?? 0} pids`);
            return true;
        });
    }

    // Partners are kept per bus connection, and forgotten when it closes: a unique name is never reused, so a later
    // process cannot inherit what an earlier one set.
    _setPartner(sender, connection, pids) {
        const entry = this._partners.get(sender);
        if (pids.length === 0) {
            if (entry) {
                Gio.bus_unwatch_name(entry.watchId);
                this._partners.delete(sender);
            }
            return;
        }
        if (entry) {
            entry.pids = pids;
            return;
        }
        const record = {pids, watchId: 0};
        this._partners.set(sender, record);
        record.watchId = Gio.bus_watch_name_on_connection(connection, sender, Gio.BusNameWatcherFlags.NONE, null,
            () => {
                if (this._partners.get(sender) !== record)
                    return;
                this._partners.delete(sender);
                Gio.bus_unwatch_name(record.watchId);
            });
    }
}

export default class EitriExtension extends Extension {
    enable() {
        const shell = new EitriShell();
        this._shell = shell;
        // The test-only methods live in a file the packages do not ship; without it the extension is complete. Even
        // where the file is present, it is loaded only when gnome-shell's environment says this is the sandbox
        // harness (policy.js), since its methods let any process on the bus type into the focused window.
        const testingModule = testingMethodsWanted(GLib.getenv(TESTING_ENV))
            ? import('./testing.js').then(module => module, () => null)
            : Promise.resolve(null);
        testingModule.then(testing => {
            if (this._shell === shell)
                shell.start(testing);
        }).catch(e => {
            console.error(`Eitri: could not start: ${e}`);
        });
    }

    disable() {
        this._shell?.destroy();
        this._shell = null;
    }
}
