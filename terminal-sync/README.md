> **Moved into neovibe on 2026-09-23** from Verdandi `crates/terminal-sync` @ `e5f9cc3` (neovibe spec
> `docs/superpowers/specs/2026-09-23-bottom-terminal-design.md`, decision 4.1(a)). Paths, revisions and pins
> below are Verdandi's as of that commit; here the crate is the workspace member `terminal-sync/`.

# terminal-sync

A **DECSET 2026 synchronized-output publication barrier** for the verdandi terminal runtime
engine. Phase 2.5 correctness-gate item 2.

Claude Code's TUI redraws frequently. If presentation snapshots are published mid-redraw, the
semantic interpreter sees torn frames. This crate decides *when* it is safe to publish. It is a
**publication barrier, not a replacement parser**.

```text
BSU (ESC[?2026h)  -> Term keeps parsing and mutating normally
                     (no raw PTY bytes buffered outside Term; parsing is never delayed)
                  -> wakeups may occur; publication of intermediate snapshots is suppressed
ESU (ESC[?2026l)  -> publish ONE consolidated snapshot
```

## The three pieces

| Piece | File | What it is |
|---|---|---|
| `NeverBuffer` | `src/never_buffer.rs` | a `vte::ansi::Timeout` whose `pending_timeout()` is unconditionally `false` |
| `SyncSpy` | `src/spy.rs` | a fully transparent `vte::ansi::Handler` that intercepts exactly the two 2026 mode dispatches — and still forwards them |
| `SyncBarrier` / `SyncDriver` | `src/barrier.rs`, `src/driver.rs` | publication gating driven by Handler **dispatch**, never by byte arrival |

## Verified facts this design rests on

Every line reference was read, not remembered.

* **`alacritty_terminal` 0.26.0's `Term` ignores 2026 in both directions.**
  `term/mod.rs:1992` (`set_private_mode`) and `:2041` (`unset_private_mode`) both end in
  `NamedPrivateMode::SyncUpdate => ()`. There is no `TermMode` bit for sync.
* **…but it advertises the mode.** `term/mod.rs:2084` (`report_private_mode`) answers DECRQM with
  `NamedPrivateMode::SyncUpdate => ModeState::Reset`, i.e. *recognised but unset*. Programs
  therefore will use 2026. That is also why `SyncSpy` still forwards the intercepted escapes
  rather than swallowing them.
* **`vte` 0.15.0 — not `alacritty_terminal` — implements 2026, by buffering raw PTY bytes.**
  `ansi.rs:39` `SYNC_BUFFER_SIZE = 0x20_0000` (2 MiB); `ansi.rs:370` `advance_sync` extends that
  buffer; `ansi.rs:335` `stop_sync_internal` replays it at ESU. That freezes `Term` for the whole
  update — exactly what the locked model forbids.
* **The escape hatch.** `Processor<T: Timeout = StdSyncHandler>` is generic and `Timeout`
  (`ansi.rs:478`) is public. `Processor::advance` (`ansi.rs:298`) guards its buffering branch on
  one call — `if self.state.sync_state.timeout.pending_timeout()`. Returning `false` there removes
  the buffering branch from reach entirely. BSU is still dispatched (`ansi.rs:1607`), and ESU is
  parsed as an ordinary `CSI ? 2026 l` through `ansi.rs:1672`.
* **Stock `alacritty_terminal::event_loop` is unusable for us** for two independent reasons: it
  advances `Term` with no hook (`event_loop.rs:154`), and it hardcodes `Processor<StdSyncHandler>`.
  It also gates its redraw wakeup on *bytes*: `event_loop.rs:166`,
  `if state.parser.sync_bytes_count() < processed && processed > 0`.

## Decisions that had to be made, and the evidence for them

### "We keep upstream's 150 ms deadline for free" — HALF TRUE, so: not kept

`NeverBuffer::set_timeout` is still *called* by vte at BSU, but nothing ever consults it.
`clear_timeout` is reachable only from `stop_sync_internal`, which is reachable only from
`advance_sync` / `advance_sync_csi` / the explicit `stop_sync` — all behind the branch
`pending_timeout() == false` disables.

Upstream does not enforce the deadline in the parser either. It enforces it in the **application
event loop**: `alacritty_terminal/src/event_loop.rs:229-231` turns `parser.sync_timeout()` into a
poll timeout and `:246` calls `parser.stop_sync(..)` when the poll expires. Under `NeverBuffer`
there is nothing buffered to flush, so that entire mechanism is absent, not inherited.

### Unterminated BSU — we ship **no** default deadline

* The DEC 2026 specification mandates no timeout.
* vte's `Processor` does not self-enforce one: `StdSyncHandler::pending_timeout` is
  `self.timeout.is_some()` and never checks expiry (`ansi.rs:469`).
* Upstream's 150 ms is application policy (above), not a parser rule.
* Under `NeverBuffer` an unterminated BSU does **not** freeze `Term` — it only suspends
  *publication*. That is a strictly milder failure than upstream's frozen screen.

So the policy lives where upstream puts it — in the engine's event loop — and
`SyncBarrier::abort_sync()` / `SyncDriver::abort_sync()` is the hook for it. We do not invent
timeout semantics without evidence. Tested by
`barrier::an_unterminated_bsu_suspends_publication_but_never_freezes_term`.

### No "2 MiB analogue" as a secondary bound

Under `NeverBuffer` the sync buffer is never populated, so `Processor::sync_bytes_count()` is
permanently `0` and carries **no volume signal**. It is exposed as `SyncDriver::sync_bytes_count()`
only because it is the measuring instrument for the negative control. Do not use it as a bound.

### Publication is gated on DISPATCH, never on byte arrival

An 8-byte BSU delivered one byte per read is seven reads in which **no Handler method is
dispatched at all**. The prototype gated on chunk arrival and published 7 spurious snapshots for
it. `SyncBarrier::dirty` is set only by `SyncSpy` forwarding an actual dispatch.
Tested by `barrier::an_eight_byte_bsu_delivered_byte_at_a_time_publishes_nothing`.

### `SyncDriver::feed` drives the parser one byte at a time

A publication must land at the exact byte that dispatched the ESU. `Processor::advance` consumes a
whole slice with no hook inside it, so feeding a whole read defers the publication to the end of
the read — and a read containing `ESU(n) BSU(n+1) <mutations of n+1>` would then publish a **torn**
mix of frames n and n+1 (or, with the naive end-of-read test, drop frame n entirely — the exact
prototype bug). Splitting at every byte is the only split that is exact without re-implementing
vte's escape scanner.

Measured cost (`tests/throughput.rs`, 818 800 bytes of TUI-shaped output, 400 synchronized
updates): **41.3 MiB/s release, 4.6 MiB/s debug**. Ample for a TUI; if it ever stops being ample,
an ESC-aware fast path is possible but must carry its own exactness proof.

## The defect this gate exists to close

Every `vte::ansi::Handler` method has a silent no-op **default** implementation, so a forgotten
forward compiles cleanly and silently breaks the terminal.

The previous prototype's `tests/forwarding.rs` was byte-stream driven — it pushed escape sequences
through the parser and checked the terminal reacted. That can only cover methods some escape
sequence happens to trigger. An adversarial sweep deleted each of the 71 forwards one at a time:
**26 were killed and 42 survived green**, including `scroll_up`, `scroll_down`,
`insert_blank_lines`, `delete_lines`, `erase_chars`, `delete_chars`, `insert_blank`, `clear_line`,
`backspace`, `newline`, `reverse_index`, `save_cursor_position`. The author's six hand-picked
mutants all landed inside the 26 that die.

The replacement (`tests/forwarding.rs`) is **direct, not byte-stream driven**: for every method of
the trait it calls that method on a `SyncSpy` wrapping a recording inner handler and asserts the
recorder observed exactly that call with exactly those arguments. Exhaustive by construction,
independent of which escapes vte emits. Same-typed parameters get index-dependent values, so an
argument swap is visible too. A second test asserts every method also signals the barrier.

### Why the method list is generated by a build script

`build.rs` re-derives the list from vte's own `pub trait Handler` declaration on **every build**.
A checked-in list would have the same failure mode one level up: it can silently fall behind the
trait. The build script hard-fails if the vte source cannot be found, if the trait block cannot be
found, if a line inside the trait looks like a method but does not parse, or if a parameter type is
not in its value table. Drift becomes a red build, never a quietly shrinking test.

`src/spy.rs`, by contrast, is **checked in and hand-maintained on purpose** — it is the mutation
target, and it must be able to drift so the generated test can catch the drift.

## Proofs

Run everything: `cargo test --offline`.

| Requirement | Where |
|---|---|
| Exhaustive forwarding sweep | `scripts/mutation_sweep.py` (see below) |
| One snapshot per update = final state, synthetic | `tests/barrier.rs::burst_between_bsu_and_esu_publishes_exactly_one_final_snapshot` |
| …byte-at-a-time splits | `tests/barrier.rs::same_burst_split_one_byte_per_read_still_publishes_exactly_once`, `::nondeterministic_chunking_is_equivalent_to_one_big_read` (64 seeds × 7 chunk sizes) |
| …real PTY, real child, real chunking | `tests/real_pty.rs::real_pty_one_snapshot_per_update` |
| Negative control (stock `StdSyncHandler` must fail) | `cargo test --features control-stock-sync-handler --test real_pty`, plus the always-on `tests/real_pty.rs::real_pty_control_stock_sync_handler_buffers_bytes_and_freezes_term` |

Measured on the real PTY (`cargo test --test real_pty -- --nocapture --test-threads=1`):

```text
under test: reads=38 publications=5 mid_update_boundaries=33 mid_update_term_advanced=28 max_sync_bytes=0
control   : reads=35 publications=5 mid_update_boundaries=30 mid_update_term_advanced=0  max_sync_bytes=60
```

Both publish 5 consolidated snapshots — publication count alone does NOT separate them. What
separates them is the locked model: under `NeverBuffer` `Term` had advanced at 28 of the 33
mid-update read boundaries and vte's raw buffer never held a byte; under the stock handler `Term`
was frozen at every one of the 30 and vte held up to 60 raw bytes. With the feature enabled,
`real_pty_one_snapshot_per_update` fails on exactly that assertion:

```text
thread 'real_pty_one_snapshot_per_update' panicked at tests/real_pty.rs:158:5:
assertion `left == right` failed: NeverBuffer must never let vte buffer raw PTY bytes (saw 60 bytes)
  left: 60
 right: 0
```
| Frame-drop: `ESU(n)` + `BSU(n+1)` in one read | `tests/barrier.rs::esu_then_bsu_in_the_same_read_still_publishes_frame_n`, `::esu_then_bsu_publication_is_not_torn_by_the_next_frames_bytes` |

### The mutation sweep

```sh
python3 scripts/mutation_sweep.py            # 71 methods x 3 operators = 213 mutants
```

The space is enumerated mechanically by brace-matching the `impl .. Handler for SyncSpy` block, so
nothing is hand-picked and nothing is mangled by a naive regex (the last method in the block
carries the impl's closing brace; the two intercepted methods are multi-line). Operators:

* `delete_method` — remove the whole `fn` item; the trait's no-op default takes over. This is the
  real defect.
* `delete_forward` — remove only the `self.inner.<name>(..)` statement.
* `delete_signal` — remove only the barrier notification (for the two intercepted methods, the
  whole `if .. SyncUpdate .. {} else {}`).

The script restores `src/spy.rs` on exit (including on failure) and exits non-zero if any mutant
survived.

**Result (2026-09-12, 213 mutants in 101 s):**

| operator | total | killed | survived | did not compile |
|---|---|---|---|---|
| `delete_method` | 71 | 71 | **0** | 0 |
| `delete_forward` | 71 | 71 | **0** | 0 |
| `delete_signal` | 71 | 71 | **0** | 0 |
| **total** | **213** | **213** | **0** | 0 |

Every mutant compiled, so the "68 of 71 mutate cleanly" caveat from the prototype does not apply
here: the brace-matching mutator handles the last method (which carries the impl's closing brace)
and the two multi-line intercepted methods correctly, and all 71 are cleanly mutable under all
three operators.

### How much a byte-stream test cannot reach

`tests/forwarding_bytestream_control.rs` is a *reasonable* byte-stream-driven forwarding test —
the shape the prototype relied on — kept solely as the control for this axis. Sweeping the same
`delete_method` space against it instead:

```sh
python3 scripts/mutation_sweep.py --ops delete_method \
  --test "cargo test --offline --test forwarding_bytestream_control"
```

**14 killed, 57 SURVIVED of 71.** Survivors include `newline`, `save_cursor_position`,
`clear_line`, `reverse_index`, `clear_screen`, `terminal_attribute`, `set_scrolling_region`,
`set_mode`/`unset_mode`, and — notably — `set_private_mode`/`unset_private_mode` themselves, even
though the corpus contains a full synchronized update. That is the whole argument for the direct
test: 57 of 71 forwards can be deleted without a byte-stream test noticing.

## Licensing

This crate **vendors no upstream source**. `alacritty_terminal` (Apache-2.0) and `vte`
(Apache-2.0 OR MIT) are ordinary crates.io dependencies. `build.rs` *reads* vte's `ansi.rs` at
build time and re-derives method names and parameter types from it; the generated file carries a
provenance header naming the exact source path, version and line range, and reproduces no vte
source text. Apache-2.0 section 4 (retain the licence, state that files were changed) is therefore
not triggered here — it *is* triggered by gate item 1, which vendors upstream alacritty's
keyboard encoder.
