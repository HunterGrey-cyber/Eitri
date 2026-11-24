English | [简体中文](performance.zh-CN.md)

# Performance

Eitri's editor is Neovide's renderer in a GTK window. These figures compare it with native Neovide. On sway and Mutter, with the editor's own surface, a typed letter reaches the screen 0.02 to 0.46 ms after native Neovide's. The figures come from development builds shortly before 0.3.0 on an AMD desktop, except where a row says otherwise, and have not been repeated for 0.4.0.

## Typing in the editor

How much later a typed letter reaches the screen than it does in native Neovide, at the median.

| How the editor is drawn | Later than native Neovide |
|---|---|
| Its own surface, the default, on sway and Mutter at 60 and 165 Hz ([how the editor reaches the screen](editor-presentation.md)) | 0.02 to 0.46 ms |
| The GTK way, GTK 4.24.1, real 165 Hz period | 5.97 ms |
| The GTK way, GTK 4.22.5, real 165 Hz period | 0.30 ms |
| The GTK way, GTK 4.24.1, GTK's 60 Hz fallback clock | about 16.5 ms |

The GTK way is what X11, NVIDIA's driver and `editor.presentation = "gtk"` use, and what the editor falls back to when
the compositor refuses its own surface. The same delay shows on GTK's development branch (4.25.0, commit c36202f6,
2026-10-25).

## Scrolling

Holding `j` at a 165 Hz output, drawn the GTK way, with stock presentation feedback (GTK 4.24 then runs on its 60 Hz fallback clock):

| | Changed frames a second | 99th-percentile frame interval |
|---|---|---|
| Native Neovide | 165.5 | 6.1 ms |
| Eitri on GTK 4.24.1 | 140.6 | 12.1 ms |
| Eitri on GTK 4.22.5 | 165.4 | 6.1 ms |

With the editor's own surface, scrolling does no worse than the GTK way.

## While a reply streams

A reply streaming at the default 5 updates a second slows typing in the editor. A key's redraw takes 11.2 ms at the
median with WebKitGTK 2.54, on GTK 4.22.5 as well, against about 2.3 ms with WebKitGTK 2.52.

## The bottom terminal

The terminal draws one output frame behind what you type. On sway, its echo reaches the screen about 18 ms after the key at a 60 Hz output and 8.7 ms at 165 Hz. At 60 Hz, other terminals land at:

| Terminal | Echo after the key, median |
|---|---|
| Ghostty | 17.3 ms |
| WezTerm | 16.7 ms |
| The terminal in Zed | 18.3 ms |
| foot | 1.0 ms |
| kitty | 0.7 ms |
| alacritty | 0.7 ms |

GTK 4.24 commits the window twice for each key, and the compositor shows the second commit a refresh later. The terminal draws inside GTK's frame clock, and the editor does not. For the lowest latency, run Neovim's own `:terminal` in the editor: typing in a buffer took 1.7 ms at the median.

## Memory, idle and startup

- An idle window with one tab uses 232 MiB more than native Neovide with the panel inside the window, and 245 MiB more
  in compositor mode, where the panel is a window of its own (the default on a tiling compositor). The goal is under
  256 MiB. Measured on an AMD Radeon RX 9070 with a development build after 0.3.0. On the integrated GPU beside it the
  window used 248 to 259 MiB more.
- The first Claude tab adds about 85 MiB, its sidecar included (79 MiB of it). Each further tab adds about 63 MiB, about
  55 of it its own sidecar. The `claude` command you installed is your own program and not part of these figures: it
  used about 109 MiB per idle session, more during a turn.
- An idle window draws no frames and uses about 13 ms of CPU a minute.
- Startup takes 260 ms, against 253 ms for native Neovide.

## High refresh rates under load

On an Intel laptop at 165 Hz, with other work keeping the machine 17 to 28 % busy, the editor drew 137 to 149 frames a
second where native Neovide drew 160, and 111 at 34 % busy, with one 83.6 ms frame. These come from before
Eitri 0.2.0's typing-latency fix and have not been repeated.

## Limits

- The editor's own surface has not been tried on NVIDIA's driver or KWin. `"auto"` keeps NVIDIA on the GTK way.
- Scrolling has not been measured with real-period presentation feedback. Below GTK 4.16 the editor draws another way, and its typing latency has not been measured there or in a `:terminal` buffer.
- If typing lags during a reply, say so in the "Testing feedback" form, with your GPU and monitor refresh rate.

More in [Known issues](../known-issues.md).
