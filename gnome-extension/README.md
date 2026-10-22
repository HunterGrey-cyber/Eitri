# The Eitri GNOME Shell extension (`eitri@huntergrey.cn`)

Developer notes; this file is not installed.

On GNOME an application cannot focus another application's window, so the companion panel cannot move focus to its
editor (or back) with `Ctrl+h/j/k/l` the way it does on sway, Hyprland and niri. This extension does it for the panel,
inside GNOME Shell, under rules that let no background application take focus at will. Since version 2 it also lets
an Eitri window put itself back on the monitor it was on (`MoveOwnToMonitor`), which a Wayland client cannot do on
GNOME either.

## Files

| file | shipped | what |
|---|---|---|
| `metadata.json` | yes | uuid, name, supported shell versions (45-50), the extension's `version` (2) |
| `extension.js` | yes | owns `cn.huntergrey.Eitri.Shell1` on the session bus, exports `/cn/huntergrey/Eitri/Shell1`, snapshots the shell's windows and activates what `policy.js` allows |
| `policy.js` | yes | every decision: fresh input, the partner list, who may call what, which windows a monitor move takes. No GNOME imports |
| `direction.js` | yes | which window lies left/right/above/below the focused one. No GNOME imports |
| `testing.js` | **no** | test-only methods for a headless shell in a sandbox (`Focused`, `Arrange`, `Activate`, `PressKey`, `Click`). With it loaded, any process on the bus can type into the focused window, so it is loaded only when gnome-shell runs with `EITRI_SHELL_EXTENSION_TESTING=1` |
| `test/` | no | node tests for `direction.js` and `policy.js` |

The packages, the tarball and `install.sh` list the four shipped files by name; `testing.js` and `test/` must never
appear in them.

## The interface

```
Version() -> u                        2 (absent from metadata.json, and 1, before MoveOwnToMonitor)
FocusDirection(s direction) -> b      left | right | up | down
FocusSelfIfNeighbour(s direction) -> b
SetPartner(au pids) -> b              the editor's process chain, nearest first; [] clears
ActivateOwn() -> b
ActivatePartner() -> b
MoveOwnToMonitor(s connector) -> b    version 2: the caller's own top-level windows to that monitor
```

A caller that gets `org.freedesktop.DBus.Error.UnknownMethod` for `MoveOwnToMonitor` is talking to version 1; Eitri
then leaves the window where the window manager put it.

Every method answers `false` when it does not act, never an error, and nothing returns a pid, title or geometry.

## The rules

The caller is the process on the other end of the bus connection (`GetConnectionUnixProcessID`): Eitri calls from the
panel process over its own gio connection, and a `gdbus`/`busctl` child it spawned would be a different caller.

| method | the focused window must belong to | fresh input in it | then |
|---|---|---|---|
| `FocusDirection(dir)` | the caller | yes | the neighbour in `dir` |
| `ActivatePartner()` | the caller | yes | the partner's most recently used window, any workspace; after the extension itself focused the caller's window, only if the partner is the pid whose window had focus then |
| `FocusSelfIfNeighbour(dir)` | the caller's partner | yes | the caller's window, if it is the neighbour in `dir` |
| `ActivateOwn()` | the caller's partner | yes | the caller's most recently used window |

- **`ActivateOwn` and `FocusSelfIfNeighbour`** also need a Wayland window of the caller to have held, within the last
  30 minutes, a focus the user gave it (else `caller-not-focused-lately`): the caller names its own partner, so without
  this a process the user was never in could name the window they are typing in. Only a focus gain made while the
  shell handles a key, button or touch event counts (a click, Alt+Tab, a shell shortcut); no client request produces
  such an event, so a window an application maps, raises or activates itself earns nothing, and neither does any
  activation of the extension's own, nor the focus it finds when it is enabled (also not any gain within 1 s of one of
  its activations). A focus that earned nothing renews nothing when it ends, so one grant never pays for the next. A
  panel reached only through the focus keys must be clicked at least once every 30 minutes, once after it opens, and
  after every screen lock (GNOME disables the extension then, and the record goes with it). Each focus change is
  logged at debug level as `credited` or `not credited`.
- **Fresh input** to a window needs two things, both within the last 500 ms and both strictly after the window last
  gained focus, and a third after the extension's own activations:
  - its `user_time`, which mutter sets to the timestamp of every key and button press into the window, is at most
    500 ms old and not in the future. It is a Wayland window: X11 windows never count, because an X11 client writes
    its own user time;
  - a real input device event, read from mutter's core idle monitor (`get_idletime()`). The user time alone is not
    proof: the focused client can move its own window's user time to "now" without any input, by asking the shell to
    activate its already-focused window (an xdg-activation request, e.g. GTK's `present()`), and mutter honours it.
    No client can reset the idle monitor; only real devices do. A key or click goes to the focused window, so a
    device event after the focus gain together with a fresh user time is input the user gave that window.

  - when the extension itself gave the window focus (its own activation for any method, or `testing.js`'s
    `Activate`), that device event must also be more than 300 ms after the activation. A key pressed in another
    window just before the extension moved focus is usually released 50-150 ms later, the release lands in the newly
    focused window, and the idle monitor cannot tell a release from a press. Only a focus gain the user made (a click,
    Alt+Tab: one the shell makes while it handles a key, button or touch event) has no such wait. Every other gain
    counts as the extension's own (`focusGainOrigin`): one within a second of any activation of the extension's,
    wherever it lands (mutter focuses the named window's modal dialog instead, or the client opens one as it is
    activated), and one with no input event behind it (focus falling back after a window closed, a client raising its
    own window, the focus found at enable).

  The extension records the focus gain on `notify::focus-window`, and also whenever it activates a window itself (read
  after the activation's timestamp, because an activation sets `user_time` too), with whether the gain counts as its
  own: the activation's own focus notify (within a second of it) keeps it marked as the extension's, and any other
  notify is marked by the rule above. It keeps
  focus gains on the monotonic clock at full width, of which mutter's timestamps are the low 32 bits, and places the
  press on that clock by its age; a focus held for a whole period of the 32-bit clock (about 49.7 days) counts nothing
  until the window is focused again. What remains open: any device event while the window has focus counts as
  activity, so pointer motion over it, or a deliberate key in it more than 300 ms after the extension focused it, can
  stand in for a press if the client moves its user time at that moment; the user is then interacting with that
  window. The shell's `captured-event` never sees input that goes to a client window, so it cannot tell a press from
  other activity.
- **After the extension's own activation**, `ActivatePartner` from that window may only send focus back to the pid
  whose window had focus when the activation was made (otherwise `partner-changed`). Input there after the 300 ms
  settle can still be the user's late release of a modifier, which the idle monitor cannot tell from a press, while the
  client moves its own user time to the same moment; without this, the caller could name a new partner after being
  focused and hand focus to any window. When the caller brought itself forward (`ActivateOwn`,
  `FocusSelfIfNeighbour`), that pid is exactly its partner then, so naming the same editor again after a raise (the
  sender's chain, then nvim's own) still works. A focus gain the user made leaves the partner free. A gain that counts
  as the extension's without a pending activation of its own takes the source of the activation whose second it falls
  in, so a modal dialog that took the focus meant for its parent is bound the same way; a gain tied to no activation
  (no input event, more than a second after any) lets `ActivatePartner` go nowhere until the user focuses the window.
  So the panel's way back to its editor (a draft, `gf`, an opened review file) does nothing from a panel that has just
  opened, after one of its own windows closed, or after a screen lock, until the user focuses it: from another window
  (a click on the panel while it already has focus changes no focus, so it frees nothing).
  `FocusDirection` is not bound this way: it only reaches a spatial neighbour, and a late release can still pass it.
- **The partner** is per caller connection, set by `SetPartner`, and forgotten when the connection leaves the bus. The
  list is cleaned of 0, 1, the shell's pid and the caller's pid, de-duplicated and capped at 64; at check time the
  partner is the first pid in it that owns a window.
- **A pid's windows** are its normal windows and dialogs that are not hidden from the taskbar, judged from each
  window's own type and flag (a utility window, a menu or a skip-taskbar panel is never one). The same test picks the
  candidates of a direction move. Mutter's most-recently-used list only chooses which of a pid's windows is activated;
  a window not in it comes after those that are.
- Nothing acts while the shell holds a modal grab (the overview, a system dialog). `MoveOwnToMonitor` is not a focus
  move and is not held back by one.
- **`MoveOwnToMonitor(connector)`** moves only windows whose pid is the caller's: its normal windows that have no
  parent window and are not hidden from the taskbar (dialogs follow their parent; X11 windows stand for the pid only
  when it has no Wayland window, as for every other method). `connector` is a connector name as `GdkMonitor` gives it
  (`DP-2`, `HDMI-A-1`): letters, digits, `-`, `_`, `.`, at most 64; anything else is refused before mutter is asked.
  The monitor is the one mutter's monitor manager finds for that connector (`get_monitor_for_connector`), and each
  window goes there with `move_to_monitor`. It answers `true` when every such window was already there or was handed to
  `move_to_monitor`, `false` for an unknown connector, a caller with no such window, or a move that threw. `true` does
  not say the window has arrived: a move that resizes a Wayland window completes when the client acks the configure,
  after the answer. No fresh input is asked
  for: a Wayland client can already ask for its own window on a given output (fullscreen on an output), and the
  boolean says nothing about any other window.

## Tests

```sh
node --test gnome-extension/test        # direction.js and policy.js
gjs -m gnome-extension/direction.js     # loads under gjs too
gjs -m gnome-extension/policy.js
for f in gnome-extension/*.js; do node --check "$f"; done
```

`test/package.json` names `all.mjs` as the directory's entry point, because this node resolves a directory argument to
`--test` as a module.

The extension itself runs only inside GNOME Shell. Exercise it in the headless GNOME Shell of the GUI sandbox, with
`testing.js` copied next to it and gnome-shell started with `EITRI_SHELL_EXTENSION_TESTING=1` in its own environment
(only the sandbox harness sets it; without it the test-only methods are never loaded, even when `testing.js` is
present); never enable it in a real session to try it out.
