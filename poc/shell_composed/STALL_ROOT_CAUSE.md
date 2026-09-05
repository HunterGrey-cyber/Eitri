# Root-cause follow-up: the ~10.00s periodic stall

`P6_P7_MEASUREMENTS.md`'s "Cross-cutting" section left this open: a recurring ~250–290ms dt spike
at an almost exactly 10.00s period, confirmed to reproduce with the resize sweep on or off and
with WebKitGTK busy, blank, or **entirely absent**. This follow-up chased it to a real, causally-
confirmed root cause.

**Verdict: environmental — not a neovibe/neovide bug.** The stall lives in the host GNOME Shell
(Mutter) session on this dev workstation, not in neovide, its dependency tree, WebKitGTK, or any
`shell_composed`/`neovide_embed_live` code. No code change follows from this finding; it's recorded
here so nobody re-opens it as a neovibe defect.

## What was ruled out

- **The neovide fork's own dependencies** (task background hypothesis #1). Checked `Cargo.toml` /
  `Cargo.lock`: `tracy-client-sys` is `default-features = false`, gated behind the `profiling`
  feature (`profiling = ["dep:tracy-client-sys"]`), which nothing in this workspace enables — its
  source isn't even present in `~/.cargo/registry/src/.../tracy-client-sys-0.28.0` (never
  downloaded, confirming it was never compiled in). `mundy`'s freedesktop backend
  (`src/freedesktop/mod.rs`) is a `zbus` D-Bus **signal subscription** (event-driven), not a poll
  loop — no periodic timer in its source. `notify-debouncer-full` is wired up only by
  `settings::config::watch_config_file`, which is called from the real `neovide` binary's
  `main.rs`, never from `LiveHarness` (confirmed by reading `live_harness.rs` in full — it never
  calls it). `tokio`'s only literal 10-second constant is `runtime::blocking::pool::KEEP_ALIVE`
  (an idle-thread teardown timeout, fires once per idle thread, not periodic). None of this
  matters in the end because of the next point.
- **Any neovide/tokio/mundy/nvim/WebKit code at all.** A ~90-line standalone C program
  (`gtk4` + `libepoxy` only, no Rust, no neovide, no nvim, no WebKitGTK) that does nothing but open
  a `GtkGLArea` and continuously redraw it via `gtk_widget_add_tick_callback` reproduces the
  **identical** ~250–290ms / ~10.00s pattern. This is dispositive: the cause cannot be anywhere in
  the neovide fork, its Cargo dependency tree, or `shell_composed`'s own code, since none of that is
  present in the reproduction.
- **A generic OS-wide freeze / scheduler stall.** A dependency-free Python busy/sleep-loop process
  (no GTK, no Wayland, no GLib) ran for 130s with **zero** stalls > 50ms. `/proc/pressure/cpu`
  read `avg10=0.00` throughout. This rules out a true system-wide freeze (CPU throttle, swap
  storm, hypervisor stall) — an unrelated process on this box doesn't see it at all.
- **A generic GLib-main-loop or GTK-window-existing effect.** A bare `GMainLoop` with an 8ms
  `g_timeout_add` and no window (130s, 0 stalls) and a real `GtkApplicationWindow` that is
  presented but never continuously redraws (130s, 0 stalls) both show nothing. The distinguishing
  condition is specifically *continuously requesting frame-clock-driven redraws* — i.e. exactly
  what an animating `GtkGLArea` does, and exactly the pattern `LiveHarness::render_frame` drives
  every frame.
- **an unrelated monitoring agent** (a separate personal systemd service on this workstation).
  Its GPU-usage collector (an unrelated monitoring agent's own source) has a
  literal `const drmFullRescanInterval = 10 * time.Second` — an extremely tempting correlative
  match found early in this investigation. **Causally disproved**: `systemctl --user stop
  unrelated-monitor-agent.service` was run *mid-run* against a live reproduction; the ~10.00s/~265ms
  stalls continued completely unaffected for the rest of that run (and a fresh run) with the
  service stopped. Restarted afterward to leave the system as found. This is the textbook case the
  debugging process exists to catch — a coincidental period match that correlation alone would have
  wrongly blamed.

## What was confirmed

Direct `/proc/<gnome-shell-pid>/stat` CPU-tick sampling (50ms resolution, `utime+stime` deltas),
timestamp-correlated against the reproducing C program's own stall log (both anchored to the same
wall clock): **GNOME Shell's own process shows a genuine multi-threaded CPU burst — 40–70ms of CPU
time consumed within a single 50ms sample window (i.e. saturating more than one logical core) for
roughly 250–300ms — recurring at essentially the same instants as the client-side render stalls,**
confirmed across two independent capture runs. GNOME Shell (which embeds Mutter, the Wayland
compositor) is where the real work is happening; the client-side symptom is Mutter being too busy
to service a `wl_surface.frame` callback promptly for a client that's continuously animating.

This session had `Vitals@CoreCoding.com` and
`system-monitor@gnome-shell-extensions.gcampax.github.com` active — both run their own
sensor/process-stat polling as JS code loaded directly into GNOME Shell's process, so their cost
shows up as GNOME Shell's own CPU time, exactly like what was measured. Disabling both
(`gnome-extensions disable ...`) and re-running the reproduction: **zero stalls over 35s.**
Re-enabling both and re-running: **zero stalls over a cumulative 255s** (70s + 150s) — the original
~250–290ms/10.00s pattern did not recur in that window, though direct `/proc` sampling of GNOME
Shell during that same window still showed a related (now ~5.00s-period, smaller-magnitude)
recurring CPU-burst signature, consistent with disabling/re-enabling having reset some
uptime-accumulated internal state (e.g. a growing sensor-history buffer one of these extensions
iterates every tick) rather than removing the underlying periodic mechanism.

This was not pushed further into single-extension bisection (disabling only one of the two, or
reading their JS source for the exact interval/data structure) — the task's own scope guidance is
not to chase an environmental cause into a rabbit hole once it's well-evidenced, and the point that
matters for neovibe is already fully nailed down by the C-program reproduction above: **whatever
the exact extension-internal mechanism, it lives entirely inside the GNOME Shell/Mutter compositor
process on this workstation, is exercised by any continuously-animating GTK4/Wayland client
(neovide's `LiveHarness`-driven `GtkGLArea` included), and has nothing to do with the code in this
repository or the neovide fork.**

## Why prior phases' own frame-pacing logs didn't show this clearly

`P6_P7_MEASUREMENTS.md` already noted the likely reason: earlier phases' periodic frame-pacing log
only fires every `LOG_EVERY_N_FRAMES` (60) frames, a ~1-in-60 chance of landing on the single
affected frame per ~10s occurrence. This phase's `SHELL_COMPOSED_LOG_EVERY_N_FRAMES=1` override
(already committed, kept) is what made the pattern visible in the first place.

## Practical takeaway for neovibe

A quarter-second freeze every ~10s would indeed be noticeable — but it is a property of *this dev
machine's* GNOME Shell session (specifically, system-monitoring shell extensions active in it), not
of the embedding architecture. On a target machine without heavy always-on GNOME Shell extensions
doing their own periodic polling (or under a different compositor entirely), there is no reason to
expect this. It does **not** undermine the project's premise that Neovide's native responsiveness
survives being embedded — the render pipeline itself was never at fault; the compositor it was
presenting frames through was momentarily busy with unrelated work. No action item follows for
`shell_composed` or the neovide fork.
