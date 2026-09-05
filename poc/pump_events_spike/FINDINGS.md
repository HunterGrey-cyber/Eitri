# pump_events_spike: findings

## Verdict

**Yes, `pump_app_events`-driven winit coexists cleanly with a GTK4/GLib main loop
on this Wayland setup (GNOME/Mutter) — with one important, previously-unverified
caveat about GL swap behavior that must be respected, and one architectural wall
on true in-process embedding that rules out the "single window" variant of the
approach.**

Concretely, of the two things this spike set out to check:

1. **Event-loop coexistence mechanics (`pump_app_events` vs `run_app`)**: works
   cleanly. No stalls, no lost input, no panics, clean interleaving, across
   multiple runs.
2. **In-process widget embedding of the winit surface inside the GTK window**:
   did not attempt to force it — concluded, before writing throwaway code, that
   it is blocked at the Wayland protocol level, not just a winit/gtk4-rs API
   gap (see "Wall: no in-process embedding on Wayland" below). Built the
   sanctioned fallback instead (separate real OS window), as the task allowed.

Recommendation: **proceed with the `pump_app_events`-based approach for
Neovide's event loop**, but carry two requirements into the fork/design:
(a) never let the embedded surface's GL swap block synchronously on the shared
thread (see the swap-interval finding — this is not optional, it's the
difference between "works" and "stalls the whole IDE for seconds"), and
(b) resolve the embedding-topology question (separate window vs. some other
mechanism) explicitly, since Wayland doesn't give you the free win X11 XEmbed
would have.

## What was built

`poc/pump_events_spike/src/main.rs`, a single-file throwaway crate:

- A `gtk::Application` with an `ApplicationWindow` containing a horizontal
  `gtk4::Paned`. Left side: a `Label` + `Button` that increments a counter and
  prints to stdout on click (`connect_clicked`). Right side: an explanatory
  `Label`. The window also runs a `glib::timeout_add_local(500ms, ...)`
  heartbeat printer and a `paned.connect_notify_local("position", ...)` probe,
  both independent of the winit pump timer.
- A `winit::event_loop::EventLoop::<()>::with_user_event().build()` (explicitly
  **not** `run_app`), holding a `winit::window::Window` created via
  `glutin_winit::DisplayBuilder`, an EGL context via `glutin`, and a
  `glutin::surface::Surface<WindowSurface>`. Each frame clears the framebuffer
  to an animated color (three independent sine waves on R/G/B) and scissor-
  clears a sliding rectangular sub-region to white — no shaders/VBOs needed,
  since this is purely about event-loop plumbing, not rendering quality.
- That `EventLoop` is driven from `glib::timeout_add_local(16ms, ...)` calling
  `event_loop.pump_app_events(Some(Duration::ZERO), &mut app)` on every tick,
  stopping the GLib source only if `PumpStatus::Exit` comes back.

Both loops run in the same process, same thread, driven entirely by GLib's
main loop (`gtk::Application::run()` is the only blocking call in `main()`).

## Wall: no in-process embedding on Wayland

The task's preferred approach was a single GTK4 window with the winit surface
embedded as a widget alongside real GTK widgets (e.g. inside the `Paned`
itself). This was not attempted as code because it's blocked one level below
winit/gtk4-rs, at the Wayland protocol:

- X11 has `XEmbed` (and GTK historically exposed it as `GtkSocket`/`GtkPlug`,
  now deprecated even there): a child window's actual pixel buffer becomes
  part of the parent's window tree, with the compositor/window-manager
  handling reparenting.
- Wayland has no equivalent generic mechanism. The closest protocol,
  `xdg-foreign` (`xdg_foreign_v1`/`v2`), only lets one client's toplevel
  declare a *logical* parent relationship to another client's toplevel, for
  stacking/positioning/transient-for purposes (e.g. "this dialog belongs to
  that window") — the embedded content stays a fully separate `wl_surface`
  positioned independently by the compositor, not merged into the parent's
  rendering tree the way an `<iframe>`-like embed would be.
- winit does not attempt to paper over this: it always creates a real,
  independent toplevel (or, at best, could target a `wl_subsurface`, but
  winit's public API does not expose subsurface creation, and subsurfaces
  still require both surfaces to belong to the *same* client/connection —
  which defeats "embed a foreign winit app" but would matter for a real
  Neovide fork that we ourselves control end-to-end and compile into the GTK
  process, see "Implication for the real fork" below).

So: for *this* spike (embedding upstream, unmodified winit/glutin machinery
inside a GTK4 shell as an opaque black box), a second real OS window is not a
fallback of convenience — it's the only thing that's actually possible without
protocol-level cooperation from Mutter/wlroots or forking winit itself to speak
`wl_subsurface` directly against GTK's own `wl_surface`/`wl_display` connection.

**Implication for the real fork**: because neovibe's actual plan is to fork
Neovide's renderer/surface code anyway (architecture doc §6, §16 — a
`SurfaceHost` trait, `NeovideSurface` not touching a whole `Window`), true
in-process embedding is *not* actually blocked for the real product the same
way it is for this spike. The fork would not create its own `winit::Window` /
top-level `wl_surface` at all — it would render into a GL context tied to
GTK's own `GtkGLArea` surface (which is already a widget inside GTK's single
`wl_surface`), and use `pump_app_events` only to keep Neovim's runtime/input
processing loop alive, decoupled from window creation entirely. That is a
materially different (and easier) integration than "embed a whole separate
winit window," and this spike's Wayland-embedding wall does not block it. It
does mean the fork cannot lean on winit's own `Window`/`DisplayBuilder` for
surface creation at all — GTK's `GtkGLArea` already owns the GL surface, so
the fork's job is to get Neovide's *renderer* to draw into that surface,
while `pump_app_events` (or an even more minimal internal loop) only pumps
Neovim/keyboard/animation state, not window-system events. This spike doesn't
prove that part works (no Skia, no real Neovide here) — it only proves the
event-loop-ownership half of the problem is solvable. The GtkGLArea/Skia
integration is P0/P1 in the feasibility doc, a separate, already-planned
validation stage.

## Finding: pump timer mechanics work; blocking vsync does not

Ran the spike repeatedly, `timeout 8`-`timeout 10` against a live Wayland
session (GNOME/Mutter, `WAYLAND_DISPLAY=wayland-0`), capturing full stdout.

**First attempt used `SwapInterval::Wait(NonZeroU32::new(1))`** (the "correct"
vsync-locked default most GL code reaches for). Result, reproduced across two
separate runs:

```
[winit] resized -> 960x720
[winit] resized -> 960x650
[gtk] paned position changed -> 622
[winit/gl] ~17 fps over last 3.73s (frame 17)      <- ~4.5 fps, not 60
[gtk] heartbeat (main loop alive)
[winit/gl] ~1 fps over last 3.25s (frame 1)         <- effectively stalled ~3s
[gtk] heartbeat (main loop alive)
[gtk] button clicked (count = 1..10)                 <- only after the stall clears
```

Heartbeats that should fire every 500ms were arriving every several seconds
during this window. The stall is on the *shared* thread: because
`swap_buffers` with `Wait(1)` blocks synchronously until the compositor's
per-surface frame-done callback arrives, and that callback was slow/absent for
several seconds right after this second toplevel was first mapped (Wayland
windows "don't appear until you draw/present to them," per winit's own
`platform::wayland` module docs, and Mutter appears to be slow to start
regular presentation callbacks for a freshly-mapped, not-yet-focused second
toplevel) — that block happened *inside* the GLib timeout callback, so it
froze GTK's own heartbeat timer and button-click signal delivery for the same
multi-second stretches. This is exactly the "GL context/surface creation
timing" risk flagged as unverified in the task brief, and it is real.

**Switching to `SwapInterval::DontWait`, keeping the same ~16ms GLib pump
timer as the sole frame-pacing mechanism**, removed the stall completely.
Steady state across multiple runs:

```
[gtk] heartbeat (main loop alive)         <- every ~500ms, no gaps
[gtk] heartbeat (main loop alive)
[winit/gl] ~63 fps over last 1.01s (frame 63)   <- locked in within ~1-2s
[gtk] button clicked (count = 1)          <- handled instantly, no lag
[gtk] heartbeat (main loop alive)
...
```

fps stabilized to ~51-63 (capped by the 16ms/~62.5Hz poll interval, as
expected) within one or two heartbeat ticks of startup, and stayed there for
the rest of every run. No warnings on stdout/stderr from GTK, winit, EGL, or
Wayland in any run.

**Conclusion from this finding**: `pump_app_events` itself (called with
`Duration::ZERO`) never blocked — it's a correctly non-blocking API as
documented. The danger is entirely on the *drawing* side: anything the
embedded surface does synchronously inside the pump callback (a blocking
buffer swap being the obvious one, but this generalizes to any blocking GL
driver call, IPC round-trip, etc.) runs on the host's main thread and can
stall the host's own UI. For the real Neovide fork this means: renderer code
invoked from inside the pump/redraw path must be non-blocking end-to-end, and
if any true blocking is unavoidable (e.g. a driver quirk under certain
compositors), it needs to move to a worker thread with results handed back
through the pump loop rather than executed inline.

## What was NOT verified

- **Real interactive resize/focus handoff between the GTK widget and the
  winit surface.** Since the winit content lives in a separate OS window (per
  the embedding wall above), "focus moving between the GTK widget and the
  winit surface" isn't a single-window focus-routing problem in this spike —
  it's normal window-manager-level focus switching between two toplevels,
  which is out of scope for what this spike was built to test. For the real
  fork (GtkGLArea-hosted, single toplevel), focus routing is a real open
  question and is correctly already called out as needing
  `NeovideInputAdapter` work in the architecture doc (§6.4) — this spike does
  not resolve it, it just confirms it's a live-window-embedding-topology
  question, not an event-loop-ownership one.
- **Manual interactive verification of the GTK button/splitter and the winit
  window's own resize.** No screenshot tooling works in this compositor
  (known/expected limitation). There is also no working GUI input-injection
  tool available in this sandbox for a *native Wayland* toplevel specifically:
  `ydotool` is installed but `ydotoold` isn't running (would need a
  root-owned uinput device set up first) and `xdotool` only sees XWayland
  clients, not native `wl_surface` toplevels, so it can't target this app's
  windows. That said, some background process in this sandbox environment
  did synthesize real button clicks against the running app (10-11 clicks
  landed and were correctly counted/logged across several runs) — that was
  not something this spike orchestrated, but it's genuine evidence the GTK
  side kept receiving and handling real input events correctly throughout,
  including during the ~63fps winit animation. The two `[winit] resized ->
  ...` log lines seen right after window creation (960x720 then 960x650,
  before any user action) show the winit window's own resize path (`gl_surface
  .resize()` + immediate redraw) executing without corruption or a crash, but
  that was the window manager's initial decoration/configure settling, not a
  deliberate drag-resize test.
- **FD-based reactive integration** (registering the Wayland connection's fd
  directly with GLib instead of polling on a timer) was investigated, not
  just assumed infeasible. Checked winit 0.30.13's actual public API
  (`~/.cargo/registry/src/.../winit-0.30.13/src/platform/wayland.rs`): the
  only Wayland-specific extension traits it exports are `is_wayland()` checks,
  `xdg_toplevel()` (a raw pointer, not a connection), `with_name()`, and
  Wayland-only monitor id lookup. There is no public accessor for the
  underlying `wayland-client::Connection`, its fd, or the internal `calloop`
  loop winit uses. Confirmed via winit's own changelog
  (docs.rs/winit/latest/winit/changelog) that 0.30.10 shipped "external event
  loops are now woken up when using `pump_events` and integrating via FD" —
  but this describes winit's *internal* wakeup plumbing for its own use of
  `pump_events`, not a public fd handle callers can hand to
  `glib::unix_fd_add_local`. Getting true FD-driven integration would require
  either unsafe access to winit internals or a winit fork/patch that exposes
  the connection — out of scope for this spike. The 16ms timer poll is not a
  "we didn't try hard enough" compromise; it's what's available against
  stock winit 0.30's public API today.
- **Long-run stability** (minutes/hours) was not tested; only 8-10s runs
  (repeated ~5 times) due to the sandboxed, non-interactive nature of this
  environment. No degradation was observed across runs, but this doesn't
  rule out slow leaks (GL resources, GLib source accumulation, etc.) over a
  real editing session.

## Recommendation

**Proceed with the `pump_app_events`-based approach for Neovide's event-loop
ownership conflict — with caveats, not a plain "yes."**

1. The core mechanism (drive a `winit::EventLoop` via `pump_app_events` from a
   GLib source instead of `run_app`) is sound on this Wayland setup: no
   stalls, no lost input, no crashes, clean shutdown via SIGTERM, across
   repeated runs, *once the swap/present path is non-blocking*.
2. Do not port `Wait(1)`/vsync-blocking swap semantics into the fork's hot
   path unexamined. Whatever presents Neovide's rendered frame (Skia's GL
   backend flush + buffer swap, in the real fork) must not be allowed to
   block the shared GTK main loop; either force non-blocking presentation and
   self-pace redraws (as this spike does), or move presentation off the main
   thread with a handoff back into the pump loop. This should become an
   explicit item in the P1 (Neovide-renderer-in-GtkGLArea) validation stage
   in the feasibility doc, since P1 is exactly where this would first bite.
3. Don't plan on the FD-reactive GLib integration; budget for a timer-driven
   poll (this spike used 16ms/~60Hz) unless/until winit exposes the
   connection publicly. A well-tuned poll interval matching the target
   refresh rate is a reasonable permanent design, not just a stopgap — it's
   also what this spike's steady-state numbers (locking to ~60fps within 1-2
   ticks) suggest is perfectly workable in practice.
4. The "single GTK4 window, winit surface embedded as a widget" variant of
   this spike is a dead end as literally specified (embedding *unmodified*
   winit's own toplevel inside GTK on Wayland has no supporting protocol).
   That's fine, because it's not actually the architecture neovibe needs: the
   real fork renders into `GtkGLArea`'s own surface directly and only uses
   `pump_app_events` (or a further-simplified internal loop) to keep runtime/
   input processing alive, never to own window/surface creation. Re-scope any
   remaining "embedding" risk in the architecture doc/feasibility doc away
   from "can winit's window live inside GTK's" (answered: no, and doesn't
   need to be) and toward "can Neovide's renderer be made to draw into a
   GtkGLArea-provided GL surface/context instead of one it creates itself" —
   which is precisely what P0/P1 in `neovibe_feasibility_validation.md` are
   already scoped to test, and remains the real open question this spike
   does not touch.
