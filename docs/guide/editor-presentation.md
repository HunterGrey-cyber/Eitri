English | [简体中文](editor-presentation.zh-CN.md)

# How the editor reaches the screen

The editor draws Neovim with the GPU. On a Wayland desktop Eitri can hand those frames straight to the compositor on a
surface of their own, beside the window's other modules; anywhere else it draws them into the GTK window like any other
widget. The first way skips a copy and a GTK frame on every keystroke: in our measurements a typed letter reached the
screen within half a millisecond of Neovide itself, where the GTK way added 6 ms at 165 Hz and 17 ms at 60 Hz.

`editor.presentation` in your `init.lua` chooses between them:

```lua
eitri.config.set("editor.presentation", "auto")
```

| value | what it does |
|---|---|
| `"auto"` (the default) | Its own surface on Wayland with GTK 4.24 or newer, unless the graphics driver is NVIDIA's; the GTK way otherwise. |
| `"subsurface"` | Its own surface on any Wayland desktop, whatever the GTK version or driver. Use it to try a combination `"auto"` leaves out. |
| `"gtk"` | Always the GTK way. |

On X11 the editor always draws the GTK way. The setting is read when the window starts and is kept when the editor
restarts; open a new window to apply a change.

`EITRI_EDITOR_PRESENTATION`, with the same three values, overrides the setting for every window the process opens. Any
other value stops Eitri at startup with a message naming the variable.

## When it falls back

If the compositor refuses something the editor's own surface needs (the buffer format, a buffer, the graphics context),
the editor switches to the GTK way for the rest of that window's life and prints one line on stderr:

```
[editor] subsurface: <the reason>; falls back to the GTK path
```

The editor keeps working; only the latency gain is gone. If you see this line, tell us the reason it names and your
compositor and driver.

## What has been tried

The editor's own surface is tested on sway and on Mutter (GNOME), at scale 1 and 1.5, with AMD graphics. It has not
been tried on KWin, Hyprland or NVIDIA. `"auto"` keeps NVIDIA on the GTK way for that reason. If something looks wrong
on your desktop (the editor misplaced, a gap beside it, a stale frame), set `"gtk"` and report it.
