# P6/P7 measurement pass — raw findings

Gathers concrete evidence for feasibility doc §9 (P6: WebKitGTK coexistence) and §10 (P7: pane
resize) by running the composed `shell_composed` binary against a live Wayland session. This is
**not** the Go/No-Go verdict (that's the next phase's job) — it's the numbers that verdict should
be based on.

Environment: live Wayland/Mutter session (`WAYLAND_DISPLAY=wayland-0`), `nvim 0.12.5` on `PATH`,
18 logical CPUs, a shared dev workstation with other unrelated processes already running
(`tsserver`/`vtsls` instances, two pre-existing baseline `nvim --embed` sessions never touched by
any run below). No screen capture or synthetic pointer/keyboard input works in this sandbox (see
`neovide_embed_live/MANUAL_VERIFICATION.md` and this repo's other phases) — everything below is
from stdout instrumentation and `/proc/<pid>/stat` CPU sampling, not visual inspection.

Instrumentation added to `shell_composed` for this pass (both **kept**, see `git diff`
`0dda2d6..HEAD` on this branch for the exact patch):

- `SHELL_COMPOSED_LOG_EVERY_N_FRAMES` env var overrides the periodic frame-pacing log's cadence
  (default still 60). Set to `1` during resize-sweep runs to get a dt/fps line for *every* frame,
  interleaved in true chronological order with `[resize #N]` lines (both come off the same
  single-threaded GTK main loop), so "dt on the frame immediately after a resize" can be read
  straight off the log.
- `install_webview_stall_poll`: polls the WebView's own on-screen `#frame-count`/`#fps`/
  `#stall-indicator` DOM text every 2s via `evaluate_javascript` and logs it as
  `[webview-poll] round_trip=... result={...}` — the only way to get that number out of the
  sandboxed WebProcess without a working screenshot path.

Two more scaffolds were used **temporarily and reverted before commit** (not in the diff):
`SHELL_COMPOSED_VERIFY_BLANK_WEBVIEW=1` (loads a static blank page instead of the busy streaming
payload) and `SHELL_COMPOSED_VERIFY_NO_WEBVIEW=1` (skips creating a WebView/WebKitGTK process
entirely, a plain placeholder `Box` in its place) — both used only for the controlled comparisons
below.

Every run in this document was confirmed via `pgrep -af nvim` / `pgrep -af WebKit` before and
after: **zero orphaned processes across every run**, whether ended via the real
`connect_close_request → LiveHarness::shutdown()` path or via `SIGTERM` (the abrupt-kill path,
tested repeatedly here as that's how automated runs in this sandbox have to be ended — see
`neovide_embed_live/MANUAL_VERIFICATION.md` for why `SIGTERM`/`SIGKILL` are already known to be
safe here: nvim exits on stdin/stdout EOF regardless of how the parent dies). Baseline was always
exactly the same two pre-existing sessions (`nvim`/`nvim --embed` pairs), untouched throughout.

---

## P7: pane resize

Run: `SHELL_COMPOSED_AUTO_RESIZE_SWEEP=1 SHELL_COMPOSED_LOG_EVERY_N_FRAMES=1 ./shell_composed --clean`,
~95s, default sweep tunables (16ms interval, 6px step, bouncing 15%–85% of paned width). Default
(busy) WebView payload running throughout — not isolated from P6 in this run, see the CPU note
below for why that turned out to matter.

**Resize events / dimensions**: 3,959 `[resize #N]` events logged, sweeping fb width from 384px to
2176px (both directions, many full round trips — bounds `[192,1088]` logical, ×2 scale factor).
Grepped every `[resize #N]` line for zero/negative fb or `content_region` dimensions: **none
found**. `live_state=failed` count: **0**. `nvim_exited=true` mid-run count: **0**. No
panic/error/abort/GL-error strings anywhere in the log.

**Frame dt** (10,972 per-frame samples, excluding the first two startup frames which include the
one-time ~115ms synchronous `nvim --embed` launch cost):

| stat | value |
|---|---|
| median dt | 8.40ms (≈119 fps) |
| p90 | 13.71ms |
| p95 | 15.32ms |
| p99 | 20.86ms |
| p99.9 | 93.45ms |
| max | 447.36ms (1 occurrence) |
| frames with dt > 33ms (missed ≥1 vsync) | 26 / 10,972 (0.24%) |

Every one of those 26 elevated-dt frames was individually inspected against its immediately
preceding `[resize #N]` log line. **None correlate with sweep direction reversal or any particular
paned position** — spike-frame fb sizes vary across the whole 384–2176px range with no pattern.
Instead they cluster at an almost exactly **10.00s period** (deltas between consecutive large
spikes: 9.97, 10.18, 10.81, 10.01, 10.00, 10.03, 9.99, 10.0, 9.99, 9.99s), each ~250–450ms. See
"cross-cutting" section below — **this periodic stall is not resize-caused**; it reproduces
identically with the sweep off and even with no WebView at all. Excluding it, resize-adjacent dt is
consistently in the 4–20ms band with no systematic degradation attributable to the sweep itself.

**CPU during sweep** (fresh 17s run, `/proc/<pid>/stat` utime+stime sampled at 1Hz, correct PIDs —
i.e. the real `/usr/lib/webkitgtk-6.0/WebKitWebProcess`, not the `bwrap` supervisor wrapping it):

| process | mean CPU% | range |
|---|---|---|
| `shell_composed` (main process: GTK loop, Skia render, resize-sweep timer) | 49.1% | 32–58% |
| `WebKitWebProcess` | **107.7%** | 65–121% (i.e. over one full core, sustained) |
| `nvim --embed` child | 0.0% | 0.0% (idle, no keystrokes sent) |

This is a real, reproducible finding worth flagging explicitly: continuous resize doesn't only cost
the editor pane — every tick also resizes the WebView's allocated width, forcing WebKitGTK to
reflow/repaint its (large, ever-growing, see P6) DOM at ~60Hz, which **pegs a full CPU core**
sustained for the duration of the sweep. Compare to the no-sweep baseline's `WebKitWebProcess`
usage of ~22–31% (below) — roughly a 4x increase. The feasibility doc's Go condition explicitly
calls out watching for a "CPU spike" during resize: by this measure there is one, and it's on the
WebKitGTK side rather than the editor side. **It did not, in this environment, translate into
measurable editor-frame degradation** (the dt distribution above already excludes it as a cause) —
but a full saturated core for the duration of any drag-resize gesture is a real cost worth carrying
into the Go/No-Go discussion, particularly for less powerful target hardware than this dev
workstation (18 logical CPUs).

**Verdict inputs, not verdict**: no correctness failures (no bad dimensions, no stuck frames, no
crashes), dt stays smooth outside the one already-explained-away periodic hitch, but WebKitGTK CPU
cost during continuous resize is non-trivial and should be weighed.

---

## P6: WebKitGTK coexistence

Four comparable runs, same binary/build, `--clean` nvim, resize sweep **off** throughout (isolating
resize as a variable), editor idle (no keystrokes) in all of them:

| run | WebView | duration | notes |
|---|---|---|---|
| A | busy (default streaming/highlight payload) | 46s | `SHELL_COMPOSED_LOG_EVERY_N_FRAMES=1`, primary evidence run |
| A′ | busy (default) | ~46s (first attempt) | earlier run, less clean (see anomaly below) |
| B | **blank** (`SHELL_COMPOSED_VERIFY_BLANK_WEBVIEW=1`, temporary, reverted) | 65s | controlled comparison |
| C | **no WebView at all** (`SHELL_COMPOSED_VERIFY_NO_WEBVIEW=1`, temporary, reverted) | 57s | zero WebKitGTK processes spawned |

This is a genuine 3-way controlled comparison, not a single uncontrolled sample — the fallback
allowance for a single well-instrumented run when a controlled comparison would be too invasive
does not apply here; a real busy/blank/absent comparison was completed.

### Editor frame dt, 5-second buckets (median, excluding the already-identified periodic
non-P6 stall, dt>100ms filtered out for this table only)

Run B (blank WebView) — flat the entire 65s:

```
t=[ 0- 5)s  median=6.04ms   t=[30-35)s  median=6.03ms   t=[55-60)s  median=6.03ms
t=[ 5-10)s  median=6.04ms   t=[35-40)s  median=6.04ms   t=[60-65)s  median=6.05ms
... (every bucket 6.03-6.05ms median, zero drift)
```

Run C (no WebView) — same flat pattern, 4.32–284.26ms range but median steady ~6.1ms throughout,
confirming B's flatness isn't an artifact of the blank page specifically.

Run A (busy WebView, the cleanest/longest per-frame-logged run) — **also flat**, median stays
6.04–6.13ms across all ten 5-second buckets from t=0 to t=50s, even as the WebView's own reported
fps (via `[webview-poll]`) visibly degrades over the same window: 163→165→164→...→161(t=35.7s)→
151(t=37.7s)→**137(t=39.7s)→124(t=41.7s)→140(t=43.7s)→125(t=45.7s)**. So in this run: the WebView's
own render rate genuinely drops by ~25% as its DOM grows (expected — `PAYLOAD_HTML` never clears
old messages, and re-runs its highlight regex over the full revealed text on every 16ms tick,
exactly the "large conversation" cost the feasibility doc's test-content list calls out), **but the
editor pane's own frame pacing does not measurably follow it down**.

### An anomaly that did not reproduce

An earlier busy-WebView run (A′, same binary, same payload, no sweep) *did* show a sustained
~2–3x editor-dt increase (steady ~6ms baseline jumping to a sustained 12–17ms plateau from t≈27.8s
onward, continuing to the end of that ~36s log) landing in the same window as a WebView fps drop to
51–79. Taken alone this would be a real P6 red flag — sustained, not a single spike, and
time-correlated with WebView degradation. **It did not reproduce** in the longer, better-controlled
run A described above, despite similar or greater WebView-side fps degradation (down to 124 fps)
producing no corresponding editor-side plateau. Given this is a shared, live dev workstation with
other unrelated processes, I cannot rule out transient contention from something else on the
machine as the cause of A′ rather than an inherent WebView/editor main-loop interaction — but I
also cannot rule the interaction *in*. **Flagging this explicitly as unresolved**: the next phase
(or a re-run on a quieter/dedicated machine) should specifically try to reproduce a sustained
editor-dt plateau correlated with heavy WebView activity before ruling this out.

### WebView's own stall indicator

`stall` (the JS-side rAF loop's own >50ms-single-frame detector) was read via `[webview-poll]` a
total of 39 times across all busy-WebView runs (A, A′, plus the earlier `p6_busy_with_stallpoll`
run) spanning fps readings from 163 down to 51. **It never once fired** (`"stall":""` every time) —
i.e. even when the WebView's *average* fps over a 500ms window dropped as low as 51–63fps, no
*single* rAF callback took more than 50ms. The degradation observed is a sustained per-frame cost
increase (each frame costing more, consistent with a bigger DOM to reflow/highlight), not discrete
janky stalls. `evaluate_javascript` round-trip time itself (the poll's own IPC latency, a proxy for
"is the WebProcess responsive right now") stayed in the 0.2–2.3ms range throughout every run,
including during the degraded-fps windows — the WebProcess never became unresponsive to IPC.

### CPU (no-sweep baseline, correct PIDs, run A)

| process | mean CPU% | range |
|---|---|---|
| `shell_composed` | 25.3% | 20–29% |
| `WebKitWebProcess` | 27.8% | 22–31% |
| `nvim --embed` | 0.0% | idle |

Compare to the resize-sweep run's 107.7% mean `WebKitWebProcess` CPU above — confirms the high
WebKitGTK CPU cost in P7 is specifically a resize-driven reflow cost, not baseline busy-payload
cost.

### Answering the doc's specific question

> Does WebView JS work stall the editor in lockstep when WebKitGTK blocks the GTK main loop?

Primary evidence (run A, the cleanest): **no** — editor dt stayed flat for the full run regardless
of WebView fps. Secondary evidence (run A′, unreproduced): a sustained correlated slowdown was
observed once and not since. Net: no confirmed lockstep blocking, but not a clean, fully-reproduced
"no" either — see the anomaly section. The WebView's own workload does get measurably more
expensive over time (fps 163→~125, no discrete stalls) purely on its own side, which is expected
and separate from the "does it block the shared loop" question.

---

## Cross-cutting: the ~10.00s periodic stall

Every run above — sweep on, sweep off, WebView busy, WebView blank, **and WebView entirely
absent** — shows a periodic dt spike of ~250–290ms recurring at an almost exactly 10.00s period
(measured deltas across dozens of occurrences across 4 independent runs: 9.97–10.81s, median
~10.00s). This was isolated by deliberately constructing the no-WebView run (C): with zero
WebKitGTK processes running at all, the same period/magnitude stall still appears
(t=8.77, 18.74, 28.74, 38.74, 48.74s — deltas 9.97, 10.00, 10.00, 10.00s).

**Conclusion: this is not a P6 finding and not a P7 finding.** It's some property of this sandbox
environment or of the underlying GTK4+Skia+Neovide/nvim pipeline itself, orthogonal to both of this
phase's variables. It does not appear to have been visible in prior phases' own reports because
their frame-pacing logs sample only every 60th frame (a ~1-in-60 chance of landing on the single
affected frame per occurrence) — this phase's `SHELL_COMPOSED_LOG_EVERY_N_FRAMES=1` override is
what surfaced it. `cgroup`/`/proc/pressure/cpu` showed `avg10=0.00` (no system-wide CPU contention)
when checked; no systemd timer with anywhere near a 10s period was found. Root cause not
identified — flagging for the next phase / a follow-up investigation rather than attributing it to
either WebKitGTK coexistence or pane resize, since it demonstrably occurs without either.

> **Resolved by a follow-up investigation — see `STALL_ROOT_CAUSE.md`.** Verdict: environmental,
> not a neovibe/neovide bug. A ~90-line GTK4+libepoxy C program with zero neovide/tokio/mundy/nvim/
> WebKit code reproduces the identical pattern; direct `/proc/<gnome-shell-pid>/stat` CPU sampling
> correlates it to a genuine periodic CPU burst inside the host's own GNOME Shell/Mutter process
> (traced to system-monitoring GNOME Shell extensions active in this dev session), which delays
> Wayland frame-callback delivery to any continuously-animating client. No code change follows.

---

## Summary of what was and wasn't fully tested

- **Fully tested, with numbers**: resize-sweep correctness (dimensions, no crashes), resize-sweep
  dt distribution, resize-sweep CPU (both processes, correct PIDs), P6 3-way controlled comparison
  (busy/blank/absent WebView) on editor dt, WebView's own fps/stall-indicator readings, P6 baseline
  CPU, clean-shutdown/no-orphan confirmation (every single run, ~10 runs total).
- **Partially tested / flagged as open**: the one non-reproduced sustained editor-dt-plateau
  anomaly (run A′) — genuinely uncertain whether it's a real WebView→editor interaction or
  workstation contention; needs a repeat attempt, ideally on a quieter machine or with the
  workstation's other processes controlled for.
- **Resolved since this doc was written**: the ~10.00s periodic stall's root cause, which was
  completely open here, has since been chased down and closed as environmental (host GNOME Shell/
  Mutter extensions, not neovibe) — see `STALL_ROOT_CAUSE.md`.
- **Not attempted**: real keyboard input into the editor during the P6 WebView-busy window (task
  called idle-editor an acceptable baseline; sending synthetic input was deprioritized given the
  known xdotool unreliability in this sandbox and the time budget).
