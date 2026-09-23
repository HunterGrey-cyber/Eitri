> **Moved into neovibe on 2026-09-23** from Verdandi `crates/terminal-frame` @ `e5f9cc3` (neovibe spec
> `docs/superpowers/specs/2026-09-23-bottom-terminal-design.md`, decision 4.1(a)). Paths, revisions and pins
> below are Verdandi's as of that commit; here the crate is the workspace member `terminal-frame/`.

# terminal-frame

The stable Verdandi **presentation contract**, its **projection** from an
authoritative `alacritty_terminal::Term`, and the **wire encodings** the
transport question was settled with. Phase 3 step 1.

```text
                    ONE child -> ONE PTY -> ONE authoritative Term
                                       |
                    Projector::next  (the ONLY damage()/reset_damage() pair)
                                       |
                                 TerminalFrame
                       /                           \
               FrameAssembler                  encode_rle
        (renderer + semantic interpreter)     (a process boundary)

                    project_scrollback  <-- the read-only second view,
                                            clamped, never moves Term
```

| module | job |
|---|---|
| `frame` | the contract. No upstream type crosses it. ONE coordinate system: **absolute grid lines**. |
| `project` | `Term` -> frame. Owns damage; compensates the **six** ways damage under-reports. |
| `assemble` | frame -> screen. Makes `FrameKind`'s meaning executable; the consumer half of the contract. |
| `encode` | frame -> bytes, reversibly. Naive and RLE. |
| `viewport` | the read-only second view, with clamped scrollback indexing. |

```sh
cargo test --release                                                  # fast gate
cargo test --release --test bandwidth -- --ignored --nocapture --test-threads=1
cargo mutants --profile release -j 4 --copy-target=true --timeout 300 # exhaustive mutation
```

---

## 1. The contract

```rust
pub struct TerminalFrame {
    pub generation: u64,
    pub kind: FrameKind,                      // Full | Delta
    pub cols: u16, pub rows: u16,
    pub cursor: FrameCursor,                  // line, col, shape, visible, blinking
    pub focused: bool,
    pub modes: TerminalModes,
    pub color_overrides: Vec<ColorOverride>,  // { index: u16, color: Option<Rgb> }
    pub rows_changed: Vec<RowUpdate>,         // { line: i32, left: u16, right: u16, cells }
}
```

### ONE coordinate system: absolute grid lines

`RowUpdate::line` and `FrameCursor::line` are **absolute grid lines** — the
coordinate system of `alacritty_terminal::index::Point`. `0` is the topmost row
of the screen, `rows - 1` the bottom, and **negative values are scrollback**.

Deliberately not the viewport row. `LineDamageBounds.line` is the lone
viewport-row quantity upstream exposes, and it is converted **exactly once**,
inside `Projector::next`, where the viewport row never escapes:

```rust
// TermDamageIterator::next returns `line.line + display_offset`, where
// `line.line` is the absolute screen line. Undo it, once.
let absolute = viewport_row as i64 - display_offset as i64;
```

Chosen because absolute lines are the system of `display_iter`'s
`Indexed.point`, `RenderableCursor.point` and `SelectionRange`, and because they
are the only one of the two that can address scrollback — which the second view
needs.

### What changed from the starting shape, and why

1. **`kind: FrameKind` added.** Not a nicety. Without it a consumer cannot tell
   "a delta that touched every row" from "a full snapshot", and therefore cannot
   know whether the rows *not* listed are unchanged or blank. Those are
   different screens. `Full` is defined as *reset to an all-default
   `cols x rows` screen, then apply* — which is what lets a Full omit entirely
   default rows and trim trailing default cells. Measured: a blank 120x40 Full
   is **11 bytes**.

2. **`color_overrides` is `Vec<ColorOverride>`, not `Vec<(u16, Rgb)>`.**
   OSC 104 / 110 / 111 / 112 **reset** an override, and a `(u16, Rgb)` pair
   cannot express "index 4 is back to your default". `color: Option<Rgb>` can.
   A Full carries the set overrides wholesale; a Delta carries only the changed
   indices, `None` included.

3. **`FrameColor` has two variants, not three.** `vte::ansi::Color` is
   `Named(NamedColor) | Indexed(u8) | Spec(Rgb)`. The first two collapse into
   `Palette(u16)`, because they already denote the same thing: `Colors:
   Index<NamedColor>` resolves with `self.0[index as usize]`, and `NamedColor`'s
   discriminants *are* palette indices (`Black = 0 … BrightWhite = 15`,
   `Foreground = 256 … DimForeground = 268`). Collapsing them makes the cell
   colour and the override-table key share one index space — which is exactly
   what lets one `Vec<ColorOverride>` describe every OSC 4/10/11/12 change.
   `contract.rs::the_named_palette_constants_match_upstream_discriminants` is
   the drift alarm.

4. **Cursor visibility is separate from cursor shape.**
   `RenderableCursor::new` folds "DECTCEM is off" into `CursorShape::Hidden`,
   destroying the shape the application chose. Here `visible: bool` is its own
   field, so a renderer can restore the right shape and a semantic consumer can
   say "the cursor is at (r, c) but hidden" instead of "there is no cursor".
   `blinking` comes from `Term::cursor_style().blinking` — the one field
   `RenderableCursor` cannot supply at all.

5. **`TerminalModes` is a small named subset, and the input-encoding modes are
   absent on purpose.** `alt_screen`, `line_wrap`, `insert`, `origin`,
   `mouse_reporting`. `APP_CURSOR`, `APP_KEYPAD`, `BRACKETED_PASTE` and the five
   kitty-keyboard bits are *not* here: input is encoded engine-side by
   `terminal-input`, which consumes the authoritative `TermMode` directly.
   Shipping a second copy of those bits across the boundary would create exactly
   the "two declarations of the same thing drift apart" seam this crate exists
   to avoid.

6. **`FrameCell` mirrors upstream's rare-attribute box.**
   `{ c, fg, bg, flags, extra: Option<Box<CellExtras>> }` — **24 bytes**, the
   same as `alacritty_terminal::term::cell::Cell`, asserted by
   `the_frame_cell_stays_small_enough_to_ship_a_screen_of`. `CellExtras` carries
   the combining marks (`zerowidth`) and the SGR 58 underline colour. The
   shaping input for a cell is `[cell.c]` followed by `zerowidth`, so any glyph
   cache key must include them.

7. **All 15 cell flags are carried, mapped explicitly.** Including the four
   upstream only ever *writes* inside the crate (BOLD, DIM, ITALIC, HIDDEN) and
   the four that are emulation-load-bearing (WIDE_CHAR, WIDE_CHAR_SPACER,
   LEADING_WIDE_CHAR_SPACER, WRAPLINE). The mapping is flag by flag rather than
   `from_bits_truncate(flags.bits())`, because a bit-cast would silently
   re-interpret if upstream renumbered;
   `the_flag_mapping_covers_every_upstream_bit` asserts the table is exhaustive
   over `Flags::all()`, and `the_flag_bit_positions_still_agree_with_upstream`
   is the alarm for a renumbering.

### Deliberately NOT in the contract

| left out | why |
|---|---|
| **SGR blink (5 / 6 / 25)** | Unrecoverable. `Attr::BlinkSlow`/`BlinkFast`/`CancelBlink` reach `Term::terminal_attribute` and fall into its `_ => ()` arm — no `Flags` bit, no `TermMode` bit. There is nothing to project. Pinned by `sgr_blink_is_unrecoverable_from_term`. |
| **OSC 8 hyperlinks** | A pure renderer affordance with no semantic content; carrying it would put an unbounded URI in the cell type. The terminal *does* record them, so `osc_8_hyperlinks_are_not_carried` asserts the gap is a decision, not an oversight. Escape hatch if wanted: a per-frame side table with a `u16` index in `CellExtras`, never a `String` per cell. |
| **Selection** | Client state the terminal deliberately excludes from damage. The client owns it, like focus. |
| **`Term::is_focused`** | A public field this crate **never reads**. Focus flows client -> frame via `Projector::set_focused`, not the other way. `focus_is_client_parked_state_the_crate_never_reads_from_term` asserts it. |
| **Vi-mode cursor** | An Alacritty *user* feature driven by keybindings this engine does not implement. If it is ever wired up it gets its own field; it is not silently swapped in for the real cursor. |
| **Raw `TermMode` bitflags** | See refinement 5. |

### Who drives resize

Resize is expressed in **cells only**. `font metrics -> cell box ->
(cols, rows)` is **renderer-owned**, and from that one decision the engine must
drive **both**:

1. `Term::resize(TermSize { columns, screen_lines })`, then
2. the PTY `TIOCSWINSZ`.

Letting those diverge is a silent-corruption seam: the child lays out for one
width while the grid is another, nothing errors, and output simply wraps in the
wrong places for as long as the mismatch lasts. The frame carries `cols`/`rows`
so a consumer can **detect** a mismatch (`ApplyError::DeltaResized`) — never so
it can drive one. Only a `Full` may change the geometry.

---

## 2. The projection

### One owner of damage

`damage()` / `reset_damage()` are a **global, destructive pair**. `Projector::next`
is the only place in the engine that may call either, it calls them exactly once
per frame, and the frame is then fanned out. `damage()` takes `&mut self` and the
returned `TermDamage<'_>` extends that borrow, so the bounds are **collected
first** into a plain `Vec`, the borrow dropped, `reset_damage()` called, and only
then is the grid read.

`Projector::full` does **not** touch damage, so a newly attached consumer can be
served at any moment without stealing the delta stream.

### Damage is a hint, not a change log

Six under-reports. Five were known; **the sixth was found by the differential
test in this crate**, and the shrinker reduced it to four escapes.

| # | what is missed | compensation |
|---|---|---|
| 1 | Combining marks — `Term::input`'s zero-width branch `push_zerowidth`es onto a *previous* column and `return`s, with no damage call. One column left of the cursor, or **two** when the previous cell is a `WIDE_CHAR_SPACER`. | span policy |
| 2 | The cross-line `LEADING_WIDE_CHAR_SPACER` clear — `write_at_cursor` clears the flag on `grid[point.line - 1][last_column]`. That cell can be on a line with **no damage at all**, so no span policy reaches it. | `previous_line_last_column` |
| 3 | The spacer-half overwrite — `clear_wide()` on `column - 1`, and the spacer flag removed from `column + 1`. | span policy |
| 4 | `Term::grid_mut()` — a public `&mut Grid` with no bookkeeping whatsoever. Undetectable from here. | `Projector::force_full()` |
| 5 | In-place writes **below the fold** while `display_offset != 0` — `TermDamageIterator::new` truncates the array to `len - display_offset`. | `below_the_fold` |
| 6 | **Everything written to the right of where the cursor ended the frame.** | `SpanPolicy::FullLine` |

#### Under-report 6, the one nobody listed

`Term::input` — the function that writes every printable character — contains
**no damage call whatsoever**, and neither does `Term::put_tab` (which also
*writes*: it substitutes `'\t'` into a blank cell). A printed run is covered
only because `Term::damage()` damages the cursor point from the *previous* call
and the cursor point now, and `LineDamageBounds::expand` takes min/max so the
two endpoints span the run between them.

That argument collapses the moment the cursor ends the frame **left** of the
rightmost cell it wrote.

Wrapping usually covers itself — `wrapline` calls `damage_cursor()` before
moving, and `linefeed()` either scrolls (full damage) or moves the line with
`damage_cursor()` on both sides. But `wrapline` **skips** its pre-move
`damage_cursor()` on the `linefeed()` path, and `linefeed()` is a **no-op** when
the cursor sits on the bottom screen line while the scroll region ends above it
(`cursor.line + 1 == scroll_region.end` is false *and*
`cursor.line < bottommost_line()` is false). The cursor snaps to column 0 of the
same line and the right-hand half of it is simply absent from damage.

Four escapes reach it:

```text
ESC[4;5r   ESC[7;21H   ESC[3B   "tab<TAB>here"
```

DECSTBM homing the cursor reaches the same gap from a different direction, and
there the lost text is not even on the cursor's final line.

**The gap is as wide as the line, so no fixed widening bounds it.**
`the_undamaged_gap_is_as_wide_as_the_line_so_no_fixed_widening_bounds_it` shows
a 20-column gap surviving widenings of 1, 2, 3, 5, 8 and 13. That is why the
production span policy is `FullLine` — every line damage mentions at all is
carried whole. Measured over **290 corpora** (34 torture cases, 89 reads in all, at three
different widths + 256 seeded random streams of 600 reads each, each read 1–6
concatenated operations):

| span policy | corpora that diverge |
|---|---|
| raw damage, nothing at all | **219** |
| widen by 1 each side (the original rule) + prev + fold | **125** |
| widen by 2 left / 1 right + prev + fold | **124** |
| `FullLine`, no previous-line rule | **21** |
| `FullLine` + `previous_line_last_column` + `below_the_fold` | **0** |

Three smaller findings recorded along the way rather than papered over:

* **A right-hand widening is not load-bearing.** `write_at_cursor`'s right-hand
  reach can never exceed the cursor's own next position, which
  `damage()`'s unconditional `damage_cursor()` always covers. The symmetric
  "widen one column each side" rule is right for the wrong reason on that side.
* **`CHT` (`CSI 3 I`) and three literal tabs land the cursor identically but
  write different screens.** Three `\t` put a `'\t'` character in columns 0, 8
  and 16; `CSI 3 I` writes nothing at all.
* **The corpus shape decides what the corpus can find.** A one-operation-per-read
  corpus reports the horizontal widening as unnecessary, because publishing a
  frame between the write and the cursor move lets the cursor damage cover the
  write for free. Real PTY reads contain many operations; the corpus was changed
  to match, and only then did the combining-mark cases become reachable.

### What is NOT compensated, because upstream already does it

`TermMode::INSERT` -> Full. `Term::damage()` opens with
`if self.mode.contains(TermMode::INSERT) { self.mark_fully_damaged(); }` and the
*leaving* edge is handled in `unset_mode`. Duplicating that here would be dead
code and an equivalent mutant. It is pinned instead by
`upstream_forces_full_damage_under_insert_mode`, which goes red if upstream stops.

### Things a damage-driven consumer must not assume

* **An empty partial does not mean "nothing changed".** `damage()` ends in an
  unconditional `damage_cursor()`. Measured: eight consecutive `damage()` calls
  with *no input at all* each reported exactly one damaged line.
  `Projector::stats().empty_deltas` was 0 across every workload.
* **A fresh `Term` reports `Full` before any `reset_damage()`.**
  `TermDamageState::new` starts with `full = true`, and nothing but
  `reset_damage()` clears it. Taking a baseline with `full()` instead of
  `next()` leaves the flag standing and the next "delta" is silently a Full —
  which is exactly how the first draft of `tests/damage.rs` managed to pass
  while measuring nothing.
* **Damage also over-reports.** `move_forward` damages every column it passed
  over; `CSI 30 C` on a blank screen damages `[0, 30]` and wrote none of it.
  Over-reporting costs bandwidth, never correctness, so nothing is done about it.
* **Selection, cursor shape, cursor visibility, OSC 12 and the title are
  excluded from damage entirely** (upstream's own doc comment says so for the
  first). This projector therefore puts the cursor, the modes and the colour
  overrides into **every** frame, Full and Delta alike, and diffs the overrides
  itself.

### The second view

`project_scrollback(&term, scrollback)` reads the grid directly at an offset the
*caller* owns. It never touches damage and never moves `Term` — asserted by
`the_authoritative_term_is_not_moved_by_a_scrollback_projection`, which also
checks the next live frame is still a Delta.

`Grid: Index<Line>` goes through `Storage::compute_index`, whose two bounds
checks are `debug_assert!`. In a release build an out-of-range `Line` does not
panic: the ring arithmetic wraps into the *physical* backing store. Measured —
after 60 lines of `SECRET-n` and a `CSI 3 J` that cleared the whole scrollback,
an unclamped read of `Line(-1)` returned:

```text
unclamped read of the ERASED history returned "SECRET-55           "
```

So the clamp is applied, once, to the window's top line:

```rust
let top = Line(-(scrollback as i32)).grid_clamp(&*grid, Boundary::Grid);
```

Every other index is `top + row`, in bounds by arithmetic because
`total_lines >= screen_lines`. Clamping each row as well would be dead code and
an equivalent mutant; `every_projected_line_is_inside_the_grid` sweeps absurd
offsets (`usize::MAX` included) and asserts the derived range stays inside
`[topmost_line, bottommost_line]`, which is the claim that makes one clamp
enough.

`display_offset` is *structurally* always 0 in this engine — `Grid` raises it
only from `Grid::scroll_display`, or from `scroll_up` when it is **already**
non-zero — and 2 000 scrolling lines confirm it never drifts. The
`below_the_fold` compensation is implemented and tested anyway, because
"structurally impossible" is a claim that should cost nothing to be wrong about.

---

## 3. The bandwidth measurement

Real PTYs, real children, **120x40**, one frame per PTY read (the engine's
wakeup model), both encodings computed on every frame.

```sh
cargo test --release --test bandwidth -- --ignored --nocapture --test-threads=1
```

| workload | frames | full | partial | naive B/frame | rle B/frame | naive B/s | rle B/s | frames/s |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| (a) shell, idle typing | 40 | 1 | 39 | 1 863 | **59** | 20 375 | **647** | 10.9 |
| (a2) shell, commands, screen full | 12 | 11 | 1 | 3 544 | **691** | 12 493 | **2 435** | 3.5 |
| (b) spinner, small deltas | 401 | 1 | 400 | 3 631 | **88** | 158 255 | **3 849** | 43.6 |
| (b) nvim, page-down burst | 114 | 14 | 100 | 12 858 | **356** | 194 240 | **5 377** | 15.1 |
| (b) Claude Code TUI, idling | 9 | 1 | 8 | 9 839 | **350** | 7 084 | **252** | 0.7 |
| (c) `seq 1 20000` | 25 | 25 | 0 | 2 918 | **750** | 11 578 | **2 975** | 4.0 |

```text
(a)  3.66s   40 reads /    120 B pty   4 929 cells  full   2%  largest rle frame   152 B  sync updates   0
       delta-only naive 1 906 B/f  rle  60 B/f     full-only naive    149 B/f  rle    34 B/f
(a2) 3.40s   12 reads /    781 B pty   2 746 cells  full  92%  largest rle frame   905 B  sync updates   0
       delta-only naive 3 617 B/f  rle  61 B/f     full-only naive  3 537 B/f  rle   748 B/f
(b)  9.20s  401 reads / 25 417 B pty  96 520 cells  full   0%  largest rle frame    96 B  sync updates 400
       delta-only naive 3 640 B/f  rle  89 B/f
(b)  7.55s  114 reads / 30 515 B pty  97 450 cells  full  12%  largest rle frame 3 616 B  sync updates 170
       delta-only naive 5 281 B/f  rle 148 B/f     full-only naive 66 980 B/f  rle 1 843 B/f
(b) 12.50s    9 reads /  3 583 B pty   5 886 cells  full  11%  largest rle frame 1 807 B  sync updates   0
       delta-only naive 11 068 B/f rle 393 B/f
(c)  6.30s   25 reads / 128 894 B pty  4 648 cells  full 100%  largest rle frame   771 B  sync updates   0
```

Workload (c) is the one figure that moves between runs: `seq` produces its
128 894 bytes faster than the reader drains them, so read coalescing decides the
frame count (**25 – 76 reads** observed across runs, i.e. 4 – 12 frames/s and
3.0 – 6.8 KB/s RLE). Bytes *per frame* are stable at ~750 B, because every one
of those frames is a Full of a scrolled screen.

### The publication rate measured here is an upper bound

One PTY read = one frame. That is the same publication point `terminal-sync`
produces **outside** a synchronized update; inside one it *suppresses*
publication until the ESU. The `sync updates` column counts DECSET 2026 updates
in the raw stream, so the difference is visible rather than assumed: the spinner
emits 400 updates for 401 reads (one frame per update either way), while nvim
emits 170 updates across 114 reads (more updates than reads, i.e. several
complete redraws arriving in one read — with `terminal-sync` in the loop those
would publish as separate consolidated frames).

### Full vs Partial: the earlier reading is confirmed, but it is conditional

The earlier observation — *full = 13 / partial = 7 over 20 samples on a real
shell, so Full dominates* — reproduces, but only under the condition that made
it true:

* **(a2) shell running commands with the screen already full: 11 Full of 12** (92%). This is the earlier reading.
* **(a) shell you are only typing at: 1 Full of 40** (2.5%).
* **(c) scrolling burst: 25 of 25** (100%).
* **(b) spinner redrawing in place: 1 of 401** (0.2%).
* **(b) nvim paging: 14 of 114** (12%).

The split is decided by **whether the workload scrolls**, not by how busy it is:
`Term::scroll_up_relative` calls `mark_fully_damaged`. A shell that runs commands
on a full screen scrolls on every line of output; a shell you are typing at does
not; a TUI redrawing in place does not. (The same test showed that at 120x40 a
handful of short commands never scrolls at all, because the cursor never reaches
the last row — the screen has to be filled first, which is why `command_script`
opens with `seq 1 60`.)

So *"Full dominates, therefore the full-frame rate is the real upper bound"* is
**true for shell-shaped workloads and false for TUI-shaped ones** — and the TUI
is the workload Verdandi actually cares about. The honest upper bound is not the
Full rate; it is the **worst-case full frame**, below.

### Encoding: the naive baseline and the RLE

* **Naive** — a fixed 15-byte record per cell (`char` 4, `fg` 4, `bg` 4,
  `flags` 2, `has_extra` 1), plus the extras payload when present. Asserted to
  be exactly that by `the_naive_body_really_is_a_fixed_record_per_cell`.
* **RLE** — style runs keyed on `(fg, bg, flags)`, and inside each run, uniform
  segments for stretches of >= `UNIFORM_MIN_RUN` identical characters. Extras
  ride once per row as `(offset, payload)` pairs rather than a presence bit on
  every cell.

Both are **exactly reversible**, and every frame every other corpus produces is
round-tripped through both: **2 180 frames / 951 761 cells**, plus an exhaustive
pass over all 2^15 flag combinations with a 269-entry override table and a
negative `line`. A byte count for a lossy encoder is not a measurement.

Ceilings, measured:

```text
BEST   blank 120x40 Full (no non-default rows at all)      rle      11 B
       120x40 Full, 40 rows of all-default cells emitted   rle     611 B
TYPICAL full 120x40 screen of coloured text, 4 080 cells   naive 61 331 B  rle  5 971 B  (10.3x)
WORST  120x40 Full, every cell a unique 24-bit style       naive 72 131 B  rle 74 637 B
       -> at 60 fps: 4.33 MB/s naive, 4.48 MB/s rle
```

**Two findings, recorded rather than asserted away:**

* On that pathological frame the RLE is **~3.5% larger** than the naive
  encoding. Every segment is one cell, so it pays a 2-byte segment header the
  fixed record does not, clawing back only one byte on the character. The RLE's
  advantage is a property of real screens being repetitive, not a property of
  the encoding. **A shipping transport should emit `min(naive, rle)` behind a
  one-byte tag** rather than committing to either.
* `UNIFORM_MIN_RUN` is **3, not the 4 the obvious hand-derivation gives.** The
  break-even point is not a constant: a run at the end of a row wins from about
  3 (nothing follows that must open a new segment), a run sandwiched between
  literals not until about 6. Encoding the whole corpus at every threshold:

  ```text
  threshold   1       2       3       4       5       6       8      12
  bytes     433241  430919  429272  430684  431722  433402  435919  444732
  ```

  `the_uniform_threshold_is_the_measured_minimum` asserts the constant is the
  argmin, and `every_threshold_produces_a_decodable_stream` proves the sweep is
  comparing one format and not twelve.

These numbers are also a fair proxy for protobuf: protobuf adds a field tag per
field and a length prefix per message, so it is strictly larger than this. Using
them to argue about transport is conservative in the direction that matters.

---

## 4. The transport recommendation

**Recommendation: `TerminalFrame` crosses a process boundary — gRPC over a Unix
domain socket — and the in-process path stays available as the same types with
no encode/decode step.**

### The numbers, and what they rule out

The measured RLE rates across every real workload are **0.25 – 6.8 KB/s**. The
largest single frame observed in anger is **3.6 KB** (nvim redrawing a whole
120x40 screen); the theoretical worst case is **75 KB**, and at a 60 fps cap
that is **4.5 MB/s**.

A Unix domain socket moves 1–10 GB/s. The peak measured rate is **six orders of
magnitude** below that; the absolute ceiling is still **three**. Bandwidth does
not decide this question, and any argument that says "we must link the engine
because serialisation is too expensive" is contradicted by measurement, not by
preference.

Latency does not decide it either. The busiest workload published **43.6
frames/s**, a 23 ms budget per frame. A UDS round trip is tens of microseconds
and gRPC's HTTP/2 framing adds order 0.1 ms — under **1%** of one frame's
budget, and two orders of magnitude below the ~10 ms at which a human notices
keystroke latency at all.

### What does decide it

Since cost is not the deciding factor, the decision falls to the failure modes
and the requirements the plan already has:

1. **The remote/SSH case needs a serialisable frame anyway.** Building the
   contract as a linked library first means writing the serialisation later,
   under pressure, against a type that was never designed to cross a boundary —
   and discovering then that `Arc<CellExtra>` and `Term`-internal enums do not
   serialise. The contract in this crate is already narrow, already
   `Box`-not-`Arc`, already 24 bytes per cell, *because* it was written to cross.
2. **Crash isolation runs the right way.** Verdandi owns terminal truth: the
   `Term`, the PTY, the child. A renderer or a semantic-interpreter panic must
   not take the child process with it. In-process, it does.
3. **Two consumers, not one.** The renderer and the Claude-side semantic
   interpreter both consume frames, and `reset_damage()` is global — exactly one
   process may own damage. The projector enforces that inside the engine and
   fans the *result* out; that fan-out is natural over a stream and awkward
   across a linked-library API where each consumer holds a `&Term`.
4. **`FrameAssembler` makes the boundary cheap to not cross.** An in-process
   renderer takes the same `TerminalFrame` by reference and skips
   `encode`/`decode` entirely. Nothing about the contract forces a serialisation
   that is not wanted; `Projector` and `encode` are separate modules precisely so
   the transport is a deployment choice, not a design one.

### What would change this answer

* **A sustained rate within two orders of magnitude of UDS throughput** —
  roughly **> 50 MB/s** of frames. Measured peak is 6.8 KB/s; the 60 fps
  worst-case ceiling is 4.5 MB/s. If a workload appears that sustains full
  120x40 truecolour frames at high rate, the answer is not "link the library" —
  it is *shared memory with a frame ring*, which keeps every isolation property
  above.
* **An end-to-end latency budget below ~1 ms.** There is no such budget in a
  terminal UI; 60 fps is 16 ms.
* **The semantic interpreter needing synchronous access to `Term` internals the
  frame does not carry** — search, selection, vi-motion. Those are emulation
  services, not presentation, and the right answer is additional RPCs on the
  same channel, not collapsing the process boundary.
* **A measured gRPC per-message overhead above ~2 ms.** That would be 10% of a
  frame budget and worth reaching for raw UDS framing instead. Nothing measured
  here suggests it, but it has not been measured *here* — the frames were
  measured, the RPC stack was not. That is the one number this recommendation
  rests on that this crate did not produce.

### One measured caveat for whoever builds the transport

At **120 columns and `SpanPolicy::FullLine`**, a delta's cost is dominated by
whole rows: the shell-typing workload carries 1 906 naive bytes per delta for a
single echoed keystroke, because the cursor's line is carried whole. The RLE
collapses that to 60 bytes, so it does not matter in practice — but it does mean
the **naive encoding is not a viable wire format at this geometry**, and a
transport that hands `TerminalFrame` to a generic serialiser without run-length
coding will spend 20 KB/s to echo typing.

---

## Licensing

This crate **vendors no upstream source**. `alacritty_terminal` (Apache-2.0) and
`vte` (Apache-2.0 OR MIT) are ordinary crates.io dependencies, and `vte` is
reached only through `alacritty_terminal::vte` — the exact version it was
compiled against — so a two-vte-versions-in-one-binary mistake is structurally
impossible. Apache-2.0 section 4 is not triggered: nothing here reproduces
upstream source text. (It *is* triggered by `terminal-input`, which vendors
upstream's keyboard encoder.)

## Tests

| file | what it proves |
|---|---|
| `tests/contract.rs` | 33 tests. Drift alarms against upstream (palette length, all 15 flag bits, `Cell` size, `NamedColor` discriminants), the known gaps, and the public surface the differential cannot reach. |
| `tests/damage.rs` | 16 tests. Each of the six under-reports: raw damage shown WRONG, compensated damage shown RIGHT. |
| `tests/delta_equals_full.rs` | **The differential.** 6 tests, 290 corpora; after every frame the delta-assembled screen must equal the terminal itself, cell for cell, cursor, modes and palette included. Plus the negative control, with exact divergence counts per policy, and a test that pins the corpus size so it cannot silently shrink. |
| `tests/encode.rs` | 11 tests. Exact round-trip of 2 180 frames / 951 761 cells through both encodings, an exhaustive pass over all 2^15 flag combinations, truncation and trailing-byte rejection, the threshold sweep, and the best/typical/worst ceilings. |
| `tests/viewport.rs` | 6 tests. The unclamped read returning erased scrollback, and the one-clamp-is-enough argument swept over absurd offsets. |
| `tests/bandwidth.rs` | The measurement: 7 `#[ignore]`d workloads plus TWO ungated sentinels. |
| `tests/common/reference.rs` | An **independently written** projection from `alacritty_terminal`'s own types. The truth every correctness test compares against. |

### Why the oracle had to be rewritten

The first version of this suite compared a delta-driven `FrameAssembler` against
a Full-driven `FrameAssembler`. That is a real differential for the *damage*
question, and it found under-report 6 — but both sides read through the same
accessors and the same `project_cell`, so a mutation of either mutated both
sides identically and the comparison still passed. The first mutation sweep
found **thirteen such survivors in `assemble.rs` alone**:
`FrameAssembler::cursor` replaced with `Default::default()`, `palette` replaced
with an empty slice, `||` swapped for `&&` in the bounds check of `cell` — every
one of them a silent corruption of what a consumer would actually see, and every
one green.

`tests/common/reference.rs` is the replacement: the cell, cursor, modes and
palette a frame *should* carry, derived from `Term` in upstream's vocabulary,
sharing no code with the crate under test.

### On the ungated sentinels

`tests/bandwidth.rs` is entirely `#[ignore]`d, so a plain `cargo test` would be
green while the whole measurement silently did not run — and the README above
quotes numbers from it. Two tests are therefore **not** ignored:

* `the_bandwidth_gate_is_runnable_here` — asserts `bash`, `sh`, `nvim`, `seq`
  and `claude` are all present. Missing programs are a red test, not a skip.
* `the_meter_actually_meters` — runs a trivial child on a real PTY in under a
  second and asserts the meter read bytes, produced frames, and encoded both
  ways. A missing binary cannot catch the meter silently measuring nothing.

Every measuring test also asserts a floor on what it observed (frames, PTY
bytes, cells touched), so "0 frames, 0 bytes, 0 B/s" can never be reported as a
wonderfully cheap workload.
