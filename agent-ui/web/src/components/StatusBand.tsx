import { useLayoutEffect, useRef, useState } from "react";
import { bandLayout } from "../band";
import type { BandFacts, Seg } from "../band";

type Props = {
  facts: BandFacts;
  /** Same claim `Footer`'s old mode block made (moved here unchanged, spec §5.2's mode row): dim,
   *  never hidden, while this pane lacks the keys. */
  paneFocused: boolean;
  /** A click anywhere on the band except the `↓N` button (spec §5.4, decision 6): what `StatusRow`
   *  used to do for its whole row. Absent on the start screen, which never opened the popover from
   *  here either. */
  onOpenDetail?: () => void;
  /** The `↓N`/`↓ ⚑` button's own click (R2, unchanged rule). */
  onJump?: () => void;
};

/** `facts.pill`'s leading `"⏵⏵ "` names the mode word after it (`"auto"`/`"bypass"`), which is all
 *  `data-mode-name` needs (spec §9's glyph colouring) -- `App.tsx` only ever builds this in the
 *  short form (`modePill(mode, cycleOffered, true)`), so there is nothing else in the string to
 *  strip. `undefined` for a pill that never carries the glyph (a future caller, a test fixture that
 *  built its own `BandFacts` by hand) -- drawn as plain text with no glyph span rather than guessing. */
function pillGlyphMode(pill: string): string | undefined {
  return /^⏵⏵\s+(\S+)/u.exec(pill)?.[1];
}

/** One left-side segment's markup -- the mode block (`data-testid="mode-block"`, dim while
 *  unfocused), the pill (its `⏵⏵` split into its own `.mode-glyph` span), or a plain segment. Its
 *  own function because the band-open element it goes inside is a `<button>` or a `<div>`
 *  depending on whether a click does anything (see that element's own doc comment), and JSX cannot
 *  share children between two different host elements without factoring the children out first. */
function renderLeftSeg(seg: Seg, facts: BandFacts, paneFocused: boolean, glyphMode: string | undefined) {
  if (seg.id === "mode") {
    return (
      <span
        key="mode"
        className="band-seg band-mode"
        data-mode={facts.mode}
        data-focused={paneFocused ? "true" : "false"}
        data-testid="mode-block"
        title={paneFocused ? undefined : "This pane does not have keyboard focus (Ctrl+l to focus it)"}
      >
        {/* v1 polish F24: BROWSE/INPUT is a claim about where typing lands, so it is drawn only while
            this pane holds the keys. Without them the block stays, dim and empty, as an inactive
            window's statusline carries no mode (lualine's `inactive_sections` has no `mode`
            component; vim shows `-- INSERT --` for the current window only, `:h 'showmode'`). An
            empty tab starts in INPUT at launch with the keys in the editor, and read `INPUT`. */}
        {paneFocused ? seg.text : ""}
      </span>
    );
  }
  if (seg.id === "pill") {
    return (
      <span key="pill" className="band-seg mode-pill" data-testid="mode-pill">
        {glyphMode !== undefined && (
          <span className="mode-glyph" data-mode-name={glyphMode}>
            ⏵⏵
          </span>
        )}
        {glyphMode !== undefined ? seg.text.slice(2) : seg.text}
      </span>
    );
  }
  return (
    <span key={seg.id} className={`band-seg band-${seg.id}`}>
      {seg.text}
    </span>
  );
}

/** The bottom band (panel round 2 plan, Task 10; spec §5): replaces `StatusRow`, `Footer`, `NewPill`
 *  and `ContextLine` with one vim-statusline-style row. Purely a renderer over `bandLayout` (`../band`),
 *  which is what decides which segments fit -- this component's own job is measuring the width in
 *  pixels a mono character actually takes (a hidden `.band-measure` span, re-measured whenever its
 *  own box changes size -- a font-size or theme push included, since either changes what that span
 *  renders at) and the band's own content-box width, and turning each `Seg` into markup. */
export function StatusBand({ facts, paneFocused, onOpenDetail, onJump }: Props) {
  const bandRef = useRef<HTMLDivElement>(null);
  const measureRef = useRef<HTMLSpanElement>(null);
  const [widthPx, setWidthPx] = useState(0);
  const [charPx, setCharPx] = useState(0);

  useLayoutEffect(() => {
    const band = bandRef.current;
    const measure = measureRef.current;
    if (band !== null) setWidthPx(band.clientWidth);
    if (measure !== null) setCharPx(measure.getBoundingClientRect().width);
    // jsdom has neither layout nor `ResizeObserver` -- the band falls back to `bandLayout`'s own
    // `widthPx <= 0`/`charPx <= 0` floor (mode and pill only), the same floor a real host sees for
    // one frame before its first measurement lands.
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver((entries) => {
      for (const entry of entries) {
        if (entry.target === band) setWidthPx(entry.contentRect.width);
        else if (entry.target === measure) setCharPx(entry.contentRect.width);
      }
    });
    if (band !== null) observer.observe(band);
    if (measure !== null) observer.observe(measure);
    return () => observer.disconnect();
  }, []);

  const segs = bandLayout(facts, widthPx, charPx);
  const left = segs.filter((seg) => seg.side === "left");
  const right = segs.filter((seg) => seg.side === "right");
  const glyphMode = pillGlyphMode(facts.pill);
  // Defect 1 (2026-09-27 sandbox GUI pass): `bandLayout` marks a `"prompt"`/`"message"` segment
  // `wraps` instead of truncating it (its own doc comment, `../band`) exactly when the full text does
  // not fit the one-row budget -- `index.css`'s `.status-band--wrap` is what actually lets the band
  // grow upward and wrap that text onto more rows, for as long as this render still carries one; an
  // ordinary short prompt/flash never sets it, so the band stays the fixed one-editor-row height it
  // always was.
  const wrapping = segs.some((seg) => seg.wraps === true);

  return (
    // C1c (spec §3.4): the band stops being a `j`/`k` stop -- `data-nav-stop="status-band"` is gone
    // -- because `j` reaching the last stop now flashes `i or Ctrl+j to type` there instead of
    // landing the cursor on it. Its own details stay reachable on `<leader>i`, `prefix i` and a
    // click (`onOpenDetail` below), none of which go through `nav.ts`'s stop list.
    <div className={wrapping ? "status-band status-band--wrap" : "status-band"} ref={bandRef}>
      {/* `aria-hidden`: not content, just this render's ruler. One character, at the band's own
          `--fs-sm`/mono font, so its rendered width IS one mono character's width. */}
      <span className="band-measure" aria-hidden="true" ref={measureRef}>
        0
      </span>
      {/* A real `<button>` only when there is something for a click to do (`onOpenDetail`) -- the
          start screen passes none, and a focusable control that activates nothing would be a dead
          stop for `hjkl`/HINT to land on (found while widening this task: the empty tab's own HINT
          count grew by one for exactly that reason). A `<div>` otherwise, same content, same
          classes, no click handling and no place in `controlsOf`'s selector. */}
      {onOpenDetail !== undefined ? (
        <button type="button" className="band-open" onClick={onOpenDetail}>
          {left.map((seg) => renderLeftSeg(seg, facts, paneFocused, glyphMode))}
        </button>
      ) : (
        <div className="band-open">{left.map((seg) => renderLeftSeg(seg, facts, paneFocused, glyphMode))}</div>
      )}
      <span className="band-spacer" />
      <div className="band-right">
        {right.map((seg) => {
          if (seg.id === "unread") {
            return (
              <button key="unread" type="button" className="band-seg band-unread" onClick={onJump} title="G">
                {seg.text}
              </button>
            );
          }
          if (seg.id === "warn") {
            return (
              <span key="warn" className="band-seg band-warn" title={facts.warn ?? undefined}>
                {seg.text}
              </span>
            );
          }
          // R5: the segment is the short figure; its tooltip is the breakdown it has no room for
          // (each token part, the exact cost, and what the figure is), as `⚠`'s carries its warning.
          if (seg.id === "usage") {
            return (
              <span key="usage" className="band-seg band-usage" title={facts.usage?.title}>
                {seg.text}
              </span>
            );
          }
          return (
            <span key={seg.id} className={`band-seg band-${seg.id}`} data-testid={seg.id === "showcmd" ? "showcmd" : undefined}>
              {seg.text}
            </span>
          );
        })}
      </div>
    </div>
  );
}
