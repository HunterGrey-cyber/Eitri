import { useLayoutEffect, useRef, useState } from "react";
import type { CSSProperties } from "react";
import { bandLayoutSteady, phoneTitle } from "../band";
import type { BandFacts, LayoutHold, Seg } from "../band";
import { initialSteady, steadyFacts } from "../steadyBand";

/** A figure's kept width: its `cells` as `ch`, which on the band's monospace font is one cell. The `↓N` button adds
 *  its own horizontal padding (4px each side, `index.css`). */
function cellsStyle(seg: Seg): CSSProperties | undefined {
  if (seg.cells === undefined) return undefined;
  return { minWidth: seg.id === "unread" ? `calc(${seg.cells}ch + 8px)` : `${seg.cells}ch` };
}

type Props = {
  facts: BandFacts;
  /** Dim, never hidden, while the pane lacks the keys. */
  paneFocused: boolean;
  /** The key that brings the keyboard into this panel (`panelEntryKey`), named in the dim mode block's title.
   *  `null` or absent when the window lists none (a companion window, or before the keymap arrives): the
   *  title then says only that the pane lacks the keys, rather than naming a key that may not be bound. */
  entryKey?: string | null;
  /** Band click outside the unread button. Absent on the start screen, where no details open. */
  onOpenDetail?: () => void;
  /** The `↓N`/`↓ ⚑` button's own click. */
  onJump?: () => void;
  onOpenTasks?: () => void;
  /** A click on the model or the effort: asks the agent for its choices, which open the `/model` or `/effort`
   *  picker. Absent where there is nothing to choose (a Codex or ACP tab): the segment stays plain text. */
  onPickModel?: () => void;
  onPickEffort?: () => void;
};

/** `facts.pill`'s leading `"⏵⏵ "` names the mode word after it (`"manual"`/`"auto"`/`"bypass"`), which is all
 *  `data-mode-name` needs (the mode glyph's colouring) -- `App.tsx` only ever builds this in the
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
function renderLeftSeg(
  seg: Seg,
  facts: BandFacts,
  paneFocused: boolean,
  glyphMode: string | undefined,
  entryKey: string | null | undefined,
) {
  if (seg.id === "mode") {
    return (
      <span
        key="mode"
        className="band-seg band-mode"
        data-mode={facts.mode}
        data-focused={paneFocused ? "true" : "false"}
        data-testid="mode-block"
        title={
          paneFocused
            ? undefined
            : `This pane does not have keyboard focus${entryKey == null ? "" : ` (${entryKey} to focus it)`}`
        }
      >
        {/* BROWSE/INPUT is a claim about where typing lands, so it is drawn only while
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
    <span key={seg.id} className={`band-seg band-${seg.id}`} title={seg.id === "phone" ? phoneTitle(facts.remoteSandbox, facts.phoneDevices) : seg.id === "cards" && facts.questions ? `${facts.cards} permissions · ${facts.questions} questions` : seg.full}>
      {seg.text}
    </span>
  );
}

/** One bottom statusline, rendered from `bandLayout`'s segments. Measure its content-box width
 *  and the mono character width to decide what fits. Re-measure the hidden `.band-measure` span
 *  when its box changes, including font or theme pushes, then turn each segment into markup. */
export function StatusBand({ facts, paneFocused, entryKey, onOpenDetail, onJump, onOpenTasks, onPickModel, onPickEffort }: Props) {
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

  // The band draws steady facts, not the live ones: while a turn streams a figure changes at most once a second
  // and keeps its width, `↓N` waits half a second before it shows, and a segment dropped for room comes back only
  // with room to spare (`steadyBand.ts`, `bandLayoutSteady`). What each render settled is kept for the next in
  // a layout effect, and one timer redraws the band when something held back is due, even if nothing else renders it.
  const memoRef = useRef(initialSteady());
  const holdRef = useRef<LayoutHold | null>(null);
  const [, setTick] = useState(0);
  const now = Date.now();
  // Another tab's band: what this one dropped for room says nothing about that one.
  if (facts.scope !== memoRef.current.scope) holdRef.current = null;
  const steady = steadyFacts(memoRef.current, facts, now);
  const laid = bandLayoutSteady(steady.facts, widthPx, charPx, { hold: holdRef.current, now, reserve: steady.reserve });
  const due = steady.due === null ? laid.due : laid.due === null ? steady.due : Math.min(steady.due, laid.due);
  useLayoutEffect(() => {
    memoRef.current = steady.memo;
    holdRef.current = laid.hold;
    if (due === null) return;
    const timer = setTimeout(() => setTick((n) => n + 1), Math.max(0, due - Date.now()));
    return () => clearTimeout(timer);
  });
  facts = steady.facts;
  const segs = laid.segs;
  const left = segs.filter((seg) => seg.side === "left");
  const right = segs.filter((seg) => seg.side === "right");
  const glyphMode = pillGlyphMode(facts.pill);
  // `bandLayout` marks a `"prompt"`/`"message"` segment
  // `wraps` instead of truncating it (its own doc comment, `../band`) exactly when the full text does
  // not fit the one-row budget -- `index.css`'s `.status-band--wrap` is what actually lets the band
  // grow upward and wrap that text onto more rows, for as long as this render still carries one; an
  // ordinary short prompt/flash never sets it, so the band stays the fixed one-editor-row height it
  // always was.
  // The count opens the background tasks list only where the agent reports its tasks (a roster in
  // `facts.backgroundTasks`); elsewhere (Codex, ACP) it stays plain text in its place inside the
  // band's details button, since that list would have nothing to show.
  const tasksButton = facts.backgroundTasks !== undefined && onOpenTasks !== undefined;
  const background = tasksButton ? left.find((seg) => seg.id === "background") : undefined;
  const detailLeft = tasksButton ? left.filter((seg) => seg.id !== "background") : left;
  const wrapping = segs.some((seg) => seg.wraps === true);

  return (
    // The band is outside j/k stops: reaching the last stop flashes how to type instead.
    // Details remain reachable by <leader>i, prefix i or a click, outside the stop list.
    <div className={wrapping ? "status-band status-band--wrap" : "status-band"} ref={bandRef}>
      {/* `aria-hidden`: not content, just this render's ruler. One character, at the band's own
          `--fs-sm`/mono font, so its rendered width IS one mono character's width. */}
      <span className="band-measure" aria-hidden="true" ref={measureRef}>
        0
      </span>
      {/* A real `<button>` only when there is something for a click to do (`onOpenDetail`) -- the
          start screen passes none, and a focusable control that activates nothing would be a dead
          stop for `hjkl`/HINT to land on (the empty tab's own HINT
          count grew by one for exactly that reason). A `<div>` otherwise, same content, same
          classes, no click handling and no place in `controlsOf`'s selector. */}
      {onOpenDetail !== undefined ? (
        <button type="button" className="band-open" onClick={onOpenDetail}>
          {detailLeft.map((seg) => renderLeftSeg(seg, facts, paneFocused, glyphMode, entryKey))}
        </button>
      ) : (
        <div className="band-open">{detailLeft.map((seg) => renderLeftSeg(seg, facts, paneFocused, glyphMode, entryKey))}</div>
      )}
      {background && <button type="button" className="band-seg band-background" onClick={onOpenTasks}>{background.text}</button>}
      <span className="band-spacer" />
      <div className="band-right">
        {right.map((seg) => {
          if (seg.id === "unread") {
            return (
              <button key="unread" type="button" className="band-seg band-unread" onClick={onJump} title="G" style={cellsStyle(seg)}>
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
          const pick = seg.id === "model" ? onPickModel : seg.id === "effort" ? onPickEffort : undefined;
          if (pick !== undefined) {
            return (
              <button key={seg.id} type="button" className={`band-seg band-${seg.id} band-pick`} onClick={pick} title={seg.id === "model" ? "Choose the model" : "Choose the effort"}>
                {seg.text}
              </button>
            );
          }
          // The segment is the short figure; its tooltip is the breakdown it has no room for
          // (each token part, the exact cost, and what the figure is), as `⚠`'s carries its warning.
          if (seg.id === "usage") {
            return (
              <span key="usage" className="band-seg band-usage" title={facts.usage?.title} style={cellsStyle(seg)}>
                {seg.text}
              </span>
            );
          }
          return (
            <span key={seg.id} className={`band-seg band-${seg.id}`} data-testid={seg.id === "showcmd" ? "showcmd" : seg.id === "link" ? "band-link" : undefined} style={cellsStyle(seg)}>
              {seg.text}
            </span>
          );
        })}
      </div>
    </div>
  );
}
