> **Moved into neovibe on 2026-09-23** from Verdandi `crates/terminal-render` @ `e5f9cc3` (neovibe spec
> `docs/superpowers/specs/2026-09-23-bottom-terminal-design.md`, decision 4.1(a)). Paths, revisions and pins
> below are Verdandi's as of that commit; here the crate is the workspace member `terminal-render/`.

# PaintOp handoff — Verdandi → Neovibe

**Verdandi revision:** `296c894` on branch `worktree-terminal-runtime-engine`
(pushed to `origin`; pin this rev, do not use a path dep into a live worktree).

**What you get:** a `Vec<PaintOp>` describing one terminal frame, fully resolved.
**What you write:** a backend that executes it. Nothing else.

```rust
for op in &list.ops { execute(canvas, op, &metrics); }
```

If you ever need to ask *"is this a wide-char spacer?"*, *"should inverse swap
these colours?"*, *"is this cell selected?"*, *"is this the cursor cell?"* or
*"what does a negative line mean?"* — **stop**. That is a contract leak and the
bug is mine. Send it back with a failing test rather than interpreting it on
your side.

---

## 1. Crates

| crate | you use it | depends on |
|---|---|---|
| `terminal-render` | **yes** — the whole contract | `terminal-frame` only |
| `terminal-frame` | only to obtain a `TerminalFrame` | `alacritty_terminal`, `bitflags` |
| `terminal-input` | yes, for keyboard — unchanged, do not rewrite | — |

`terminal-render`'s **public API contains no `terminal-frame`, `alacritty_terminal`
or `vte` type.** `RgbColor`, `CursorShape`, `UnderlineKind`, `GlyphStyle`,
`CursorText`, `PaintLayer`, `PaintOp`, `PaintList` are all its own. There is no
graphics dependency here and there never will be — a backend is yours.

`tests/backend.rs` is **not** a backend. It is a text-surface stand-in that
imports `terminal_render` and nothing else, so that a terminal type re-entering
the public API breaks compilation. Treat it as executable documentation of the
minimum a backend must do.

## 2. The pipeline

```rust
use terminal_frame::{project_window, viewport::RawViewport};
use terminal_render::{build_paint_list, palette_for, RenderInput, ViewMode};

// 1. Which absolute grid line is the top of the view?
let (top_line, mode) = match viewport.top_line(&term) {
    Some(l) => (l, ViewMode::Pinned),        // holding history
    None if viewport.is_pinned() => (0, ViewMode::AnchorExpired), // pin died
    None => (0, ViewMode::FollowBottom),     // live bottom
};

// 2. Project that window out of the authoritative Term. Read-only; never
//    mutates Term and never calls Term::scroll_display.
let frame = project_window(&term, top_line, rows);

// 3. Resolve everything into paint ops.
let palette = palette_for(&frame);
let list = build_paint_list(&RenderInput {
    frame: &frame,
    window_top_line: top_line,   // MUST be the same value passed to project_window
    mode,
    selection: &selection_spans,
    focused: pane_has_focus,     // VIEW state — see §8
    palette: &palette,
});
```

Call `viewport.observe(&term)` once per wakeup while pinned, **before** step 1.

## 3. Coordinates — one space, cells, window-relative

```text
row   0 .. list.rows     0 is the TOP row of the window.   never negative
col   0 .. list.cols     0 is the LEFT column.             never negative
cols                     columns this op spans (1, or 2 for a wide glyph)
```

Absolute grid lines and scrollback numbers **do not appear in the op stream**.
A pinned historical window and a live window are identical in shape. Pixels are
yours:

```text
x = col  * cell_width      w = cols * cell_width
y = row  * cell_height     h = cell_height
```

Verdandi has no opinion on font, cell size, baseline or DPI and carries none of
them. Use **one** metrics object for backgrounds, glyphs, cursor and overlay or
they will land on different lattices.

`list.top_line` is the absolute grid line that became row 0. It is **provenance
only** — logs, correlating a frame with `RawViewport`. Doing arithmetic with it
means reimplementing scrollback.

## 4. Before the first op

Clear the surface with `list.surface_background`. Ops need not cover every cell,
and the surface is rarely an exact multiple of the cell size.

## 5. Z-order — fixed, not your choice

`list.ops` is emitted in non-decreasing `PaintLayer` order; executing front to
back is correct by construction. `list.is_layer_ordered()` asserts it.

```text
0 CellBackground   every cell's fill; selection already resolved into it
1 Text             glyphs, with their underline and strikeout
2 Cursor           the cursor, with the covered character already re-resolved
3 Overlay          view-state notices
```

Within one `DrawText`: glyph, then underline, then strikeout.

## 6. The four ops

```rust
FillCells  { row, col, cols, color }
DrawText   { row, col, cols, text, color, style }
DrawCursor { row, col, cols, shape, color, text_under, blinking }
DrawNotice { row, col, cols, text, color, background }
```

**`DrawText.text` is one grapheme**, possibly several Unicode scalars (base +
combining marks). Shape it as a unit. **Never count scalars to decide width** —
`cols` is the width and it is the terminal's own accounting.

**`cols == 2` is a full-width glyph: one glyph, two columns.** No op is ever
emitted for the second column, so there is nothing to suppress and no spacer
concept to know. This holds for *decorated* wide glyphs too — a spacer inherits
the wide char's SGR, and suppressing it is done here.

`GlyphStyle.underline_color` is always a concrete colour (the glyph's own when
the terminal specified none) — never an `Option` to interpret. `style.bold`
means *use a bold face*; the colour is already brightened, so do not brighten it
again.

## 7. Cursor

Emitted **only** when visible and inside the window, so its presence is the whole
decision. Draw `shape` in `color`, then `text_under` on top if `Some`.

`text_under` is the covered character already recoloured to contrast — for a
`Block` cursor, which would otherwise erase it. **You invert nothing.** Non-
obscuring shapes (`Beam`, `Underline`, `HollowBlock`) carry `None` because the
glyph beneath is still visible.

Shapes are drawing instructions, not terminal modes: `Block` fills the cell box,
`Underline` a bar along the bottom, `Beam` a bar along the left, `HollowBlock`
strokes the outline. `blinking` is advisory — a steady cursor is conforming.

Cursor position is authoritative terminal state. **Never infer it from the last
glyph drawn.**

## 8. Focus and selection

**Focus is view state**, passed on `RenderInput`, not read from the frame —
`project_window` hardcodes `focused: false`, so reading it there makes every
window permanently unfocused. Unfocused ⇒ `HollowBlock`, resolved here.

**Selection is resolved into colours.** No op carries a selection flag; selected
cells simply have swapped foreground and background, which composes correctly
with an already-inverse cell. Supply spans in **absolute grid lines**
(`SelectionSpan { line, start_col, end_col }`) — that is the only place you speak
absolute coordinates, because selection anchors to content, not to the window.

Upstream resize behaviour you must not fight: a row-only resize rotates a
selection by the line delta; a **column resize clears it** (`Term::resize` sets
`selection = None`). Do not invent persistence past that. Selection and
historical browsing stay separate concepts.

## 9. AnchorExpired

When `RawViewport::top_line()` returns `None` for a pinned view, the anchored
content is gone — evicted by output, by a narrowing resize, or destroyed by
`ESC[3J`/`RIS`. Render `ViewMode::AnchorExpired`: the frame is the **live
bottom** and a `DrawNotice` says so.

**Never silently show whatever now occupies the old position.** The notice
carries its own text, colours and extent precisely so a backend cannot
accidentally render nothing. Product meaning: *the history you pinned no longer
exists; you are now looking at live output.* Offering an explicit "back to
bottom" affordance is yours.

## 10. Threading and ownership

**Rendering progress must never drive stream consumption.** Three separate
notions of progress, never collapsed into one counter:

```text
bytes consumed from the PTY  →  Term is authoritative, advances on its own
viewport observed            →  once per wakeup while pinned
last painted                 →  UI only
```

A stalled or unfocused pane must not stop the PTY being read or the `Term` being
fed. (The SDK track hit the collapsed-counter version of this and measured 253
events piling up client-side during a 20s UI stall; on a terminal it would be
worse — backpressure reaches the child process.)

`build_paint_list` is pure and takes `&TerminalFrame`; it never touches `Term`.
Project a frame under whatever lock owns `Term`, then release it and build/paint
off that snapshot.

## 11. What must NOT be reimplemented in Neovibe

- **No second emulator.** One child, one PTY, one authoritative
  `alacritty_terminal::Term`. No xterm.js, no second VT parser, no replay into
  another emulator. Switching Raw ↔ semantic view must never require restart,
  resume or replay.
- **No offset scrolling.** Scroll position is a `RawViewport` ordinal, not a
  distance from the bottom. An offset drifts under output *and* under reflow —
  measured: a pinned row's absolute line moved −11 → −181 across 170 appends
  while its content never moved.
- **Never call `Term::scroll_display()`** for the Raw view. It mutates shared
  state and mangles damage.
- **Do not reinterpret `WIDE_CHAR_SPACER`** — it never reaches you.
- **Do not reconstruct logical lines.** You paint physical rows; reflow has
  already happened. Logical-line identity is `RawViewport`'s job.
- **No VT/SGR semantics in the backend.** Colours arrive resolved.
- **Do not rewrite keyboard encoding.** Use `terminal-input`; its `key_release`,
  stray-`ESC` fall-through and Kitty Shift+Backspace fixes are regression-covered.

## 12. Not yet provided — say so rather than building one

**There is no Rust PTY+`Term` session type in this repo yet.** `terminal-frame`
gives you projection from a `Term` you already have; `terminal-sidecar` is
TypeScript and moves raw bytes over `terminal.runtime.v1` for a *different*
consumer — it holds no `Term`.

Intended shape: **Neovibe links `terminal-frame`/`terminal-render` in-process and
Verdandi supplies the session type that owns the PTY and the authoritative
`Term`.** That crate is the next Verdandi task. Until it lands, build the backend,
metrics, pane and input plumbing against a `PaintList` you construct in tests —
that is the whole of what the backend consumes. **Do not write a PTY/`Term`
owner**; two of those is the one-emulator invariant broken.

## 13. Instrumentation to leave room for

Not to optimise now — just do not make it impossible later:

```text
prompt submitted → first PTY byte → first PaintOp carrying a visible glyph
                 → first painted glyph → stable presentation → completion
```

## 14. Golden corpus

`crates/terminal-render/tests/golden/paintops.txt` — one readable line per op
across 17 scenarios (ASCII, CJK, wide-at-wrap, decorated wide, combining marks,
indexed/truecolor, bold/dim/inverse/underline/strike, cursor over glyph and over
a wide glyph, unfocused, blinking, selection, wrapped rows, pinned history with
negative absolute lines, anchor expired). Useful as backend fixtures: it is the
exact op stream for known input.
