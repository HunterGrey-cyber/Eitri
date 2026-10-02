# The Eitri GNOME Shell extension (`eitri@huntergrey.cn`)

Developer notes; this file is not installed.

On GNOME an application cannot focus another application's window, so the companion panel cannot move focus to its
editor (or back) with `Ctrl+h/j/k/l` the way it does on sway, Hyprland and niri. This extension does it for the panel,
inside GNOME Shell, under rules that let no background application take focus at will.

## Files

| file | shipped | what |
|---|---|---|
| `metadata.json` | yes | uuid, name, supported shell versions (45-50) |
| `extension.js` | yes | owns `cn.huntergrey.Eitri.Shell1` on the session bus, exports `/cn/huntergrey/Eitri/Shell1`, snapshots the shell's windows and activates what `policy.js` allows |
| `policy.js` | yes | every decision: fresh input, the partner list, who may call what. No GNOME imports |
| `direction.js` | yes | which window lies left/right/above/below the focused one. No GNOME imports |
| `testing.js` | **no** | test-only methods for a headless shell in a sandbox (`Focused`, `Arrange`, `Activate`, `PressKey`, `Click`). With it loaded, any process on the bus can type into the focused window, so it is loaded only when gnome-shell runs with `EITRI_SHELL_EXTENSION_TESTING=1` |
| `test/` | no | node tests for `direction.js` and `policy.js` |

The packages, the tarball and `install.sh` list the four shipped files by name; `testing.js` and `test/` must never
appear in them.

## The interface

```
Version() -> u                        1
FocusDirection(s direction) -> b      left | right | up | down
FocusSelfIfNeighbour(s direction) -> b
SetPartner(au pids) -> b              the editor's process chain, nearest first; [] clears
ActivateOwn() -> b
ActivatePartner() -> b
```

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
    focused window, and the idle monitor cannot tell a release from a press. A focus gain the user caused (a click,
    Alt+Tab, focus falling back) has no such wait.

  The extension records the focus gain on `notify::focus-window`, and also whenever it activates a window itself (read
  after the activation's timestamp, because an activation sets `user_time` too), with whether the gain was its own:
  the activation's own focus notify keeps it marked as the extension's, any other notify marks it as not. It keeps
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
  sender's chain, then nvim's own) still works. A focus gain the user made leaves the partner free.
  `FocusDirection` is not bound this way: it only reaches a spatial neighbour, and a late release can still pass it.
- **The partner** is per caller connection, set by `SetPartner`, and forgotten when the connection leaves the bus. The
  list is cleaned of 0, 1, the shell's pid and the caller's pid, de-duplicated and capped at 64; at check time the
  partner is the first pid in it that owns a window.
- **A pid's windows** are its normal windows and dialogs that are not hidden from the taskbar, judged from each
  window's own type and flag (a utility window, a menu or a skip-taskbar panel is never one). The same test picks the
  candidates of a direction move. Mutter's most-recently-used list only chooses which of a pid's windows is activated;
  a window not in it comes after those that are.
- Nothing acts while the shell holds a modal grab (the overview, a system dialog).

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
