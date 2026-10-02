// Test-only methods for a headless GNOME Shell in a sandbox. The packages do not ship this file, and the extension
// works without it. With it, any process on the session bus can read the focused window's pid, move windows and type
// into whatever has focus: never install it on a real session. extension.js loads it only when gnome-shell itself
// runs with EITRI_SHELL_EXTENSION_TESTING=1, which only the sandbox harness sets.
import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';

export const METHODS_XML = `
    <method name="Focused"><arg type="u" direction="out" name="pid"/></method>
    <method name="Arrange"><arg type="u" direction="in" name="pid"/><arg type="i" direction="in" name="x"/><arg type="i" direction="in" name="y"/><arg type="i" direction="in" name="width"/><arg type="i" direction="in" name="height"/><arg type="b" direction="out" name="done"/></method>
    <method name="Activate"><arg type="u" direction="in" name="pid"/><arg type="b" direction="out" name="done"/></method>
    <method name="PressKey"><arg type="u" direction="in" name="keyval"/><arg type="b" direction="out" name="done"/></method>
    <method name="Click"><arg type="i" direction="in" name="x"/><arg type="i" direction="in" name="y"/><arg type="b" direction="out" name="done"/></method>`;

function firstNormalWindowOf(pid) {
    return global.get_window_actors()
        .map(actor => actor.meta_window)
        .find(window => window.get_pid() === pid && window.get_window_type() === Meta.WindowType.NORMAL) ?? null;
}

// Adds the methods to the service object and returns a function that removes them again.
export function install(service) {
    const seat = Clutter.get_default_backend().get_default_seat();
    let keyboard = seat.create_virtual_device(Clutter.InputDeviceType.KEYBOARD_DEVICE);
    let pointer = seat.create_virtual_device(Clutter.InputDeviceType.POINTER_DEVICE);

    service.Focused = () => {
        const window = global.display.focus_window;
        const pid = window ? window.get_pid() : 0;
        return pid > 0 ? pid : 0;
    };

    service.Arrange = (pid, x, y, width, height) => {
        const window = firstNormalWindowOf(pid);
        if (!window || width <= 0 || height <= 0)
            return false;
        try {
            // Mutter 49 dropped the flags argument.
            window.unmaximize();
        } catch (e) {
            try {
                window.unmaximize(Meta.MaximizeFlags.BOTH);
            } catch (e2) {}
        }
        window.move_resize_frame(true, x, y, width, height);
        return true;
    };

    // Focuses the pid's window the way the extension does, so the activation's timestamp is recorded and never
    // counted as the window's input.
    service.Activate = pid => {
        const window = firstNormalWindowOf(pid);
        if (!window)
            return false;
        service.activate(window);
        return true;
    };

    // A key press and release delivered to whatever has keyboard focus, stamped with the current time.
    service.PressKey = keyval => {
        keyboard.notify_keyval(GLib.get_monotonic_time(), keyval, Clutter.KeyState.PRESSED);
        keyboard.notify_keyval(GLib.get_monotonic_time(), keyval, Clutter.KeyState.RELEASED);
        return true;
    };

    // A primary-button click at stage coordinates.
    service.Click = (x, y) => {
        pointer.notify_absolute_motion(GLib.get_monotonic_time(), x, y);
        pointer.notify_button(GLib.get_monotonic_time(), Clutter.BUTTON_PRIMARY, Clutter.ButtonState.PRESSED);
        pointer.notify_button(GLib.get_monotonic_time(), Clutter.BUTTON_PRIMARY, Clutter.ButtonState.RELEASED);
        return true;
    };

    return () => {
        for (const name of ['Focused', 'Arrange', 'Activate', 'PressKey', 'Click'])
            delete service[name];
        keyboard = null;
        pointer = null;
    };
}
