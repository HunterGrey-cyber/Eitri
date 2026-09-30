/** HINT labels are positioned relative to the panel root after each commit, before paint.
 *  Events can move targets while HINT is active; measuring during render would use stale DOM.
 *  Detached targets get no label because their zero rect would put a phantom tag at the corner.
 *  Targets outside the visible area also get no label, so tags do not cover the surrounding UI.
 *  Their letters stay assigned by index, and landing on an offscreen target reveals its row.
 *  A user wheel scroll cancels HINT through the window scroll controller.
 *  The `hint-bg`/`hint-fg` colour pair is checked for contrast in Rust (`tokens.rs`). */
import { useLayoutEffect, useRef, useState } from "react";
import type { CSSProperties } from "react";
import { clipBox, firstShownLine, visibleBox } from "../nav";
import type { HintTarget } from "../nav";

export type ShownHint = { target: HintTarget; label: string };

/** Where each label goes, by the hint's index; `null` for a target no longer on the page or no
 *  longer in view. */
type Placement = { top: number; left: number; width?: number; height?: number; shift?: boolean } | null;

/** Fallback label size for the clamps until a label can be measured: one `--fs-xs` line plus 3px
 *  padding on each side at the default 14px font. Use measured `labelSize` after rendering
 *  because a larger `agent.font_size` or zoom can double the size and make a 16px clamp
 *  overflow. jsdom uses the fallback because it measures no layout. */
export const LABEL_PX = 16;

/** The largest label drawn in `layer` (a two-letter label is wider than a one-letter one), rows' aside, or
 *  `LABEL_PX` for a dimension nothing has been drawn to measure yet. The placement effect runs after
 *  every commit, so the first pass uses the fallback and the next one the measured size. */
function labelSize(layer: HTMLElement | null): { width: number; height: number } {
  let width = 0;
  let height = 0;
  // A row's label is sized to its sign cell (`place`), so it says nothing about a label's own size.
  for (const label of layer?.querySelectorAll<HTMLElement>(".hint-label:not(.hint-row)") ?? []) {
    width = Math.max(width, label.offsetWidth);
    height = Math.max(height, label.offsetHeight);
  }
  return { width: width > 0 ? width : LABEL_PX, height: height > 0 ? height : LABEL_PX };
}

function place(root: HTMLElement, hints: ShownHint[], label: { width: number; height: number }): Placement[] {
  const origin = root.getBoundingClientRect();
  return hints.map(({ target }) => {
    if (!target.el.isConnected) return null;
    // Place labels on the visible part of each target and clamp their tops into its clip box.
    // This keeps partially clipped targets labelled on screen instead of over the surrounding UI.
    const vis = visibleBox(target.el, root);
    const clip = vis === null ? null : clipBox(target.el, root);
    if (vis === null || clip === null) return null;
    const clampTop = (want: number) => Math.max(clip.top, Math.min(want, clip.bottom - label.height)) - origin.top;
    // Clamp sideways too: a link in a horizontally scrolled wide table can start left of its clip box.
    const clampLeft = (want: number) => Math.max(clip.left, Math.min(want, clip.right - label.width)) - origin.left;
    if (target.kind === "code") return { top: clampTop(vis.top + 4), left: vis.right - origin.left - 4, shift: true };
    if (target.kind === "row") {
      const sign = target.el.querySelector<HTMLElement>(".row-sign");
      const cell = (sign === null ? null : visibleBox(sign, root)) ?? vis;
      return { top: clampTop(cell.top), left: cell.left - origin.left, width: cell.width, height: cell.height };
    }
    // A wrapped link bounding box can start at the row edge and place its label over unrelated text.
    // Use `firstShownLine` so the label follows the first visible line, even if earlier lines scrolled out.
    // jsdom reports no line boxes, so the bounding box is the fallback. Clamp either into the clip box.
    const r = target.el.getBoundingClientRect();
    const at = target.kind === "link" ? firstShownLine(target.el, root) : r;
    return { top: clampTop(at.top - 6), left: clampLeft(at.left - 6) };
  });
}

export function HintLayer({ root, hints, typed }: { root: HTMLElement | null; hints: ShownHint[]; typed: string }) {
  const [placements, setPlacements] = useState<Placement[]>([]);
  const layerRef = useRef<HTMLDivElement>(null);
  // No dependency list on purpose: see the doc above. The equality check is what stops this effect
  // from re-rendering itself forever.
  useLayoutEffect(() => {
    const next = root === null ? [] : place(root, hints, labelSize(layerRef.current));
    if (JSON.stringify(next) !== JSON.stringify(placements)) setPlacements(next);
  });
  if (root === null || hints.length === 0) return null;
  return (
    <div className="hint-layer" aria-hidden="true" ref={layerRef}>
      {hints.map(({ target, label }, i) => {
        const p = placements[i];
        // `undefined`: not measured yet (the first commit of a new list; the layout effect fills it
        // in before paint). `null`: the target has left the page.
        if (p === undefined || p === null) return null;
        const matches = label.startsWith(typed);
        const style: CSSProperties = { top: p.top, left: p.left, width: p.width, height: p.height };
        if (p.shift) style.transform = "translateX(-100%)";
        return (
          <span key={i} className={`hint-label hint-${target.kind}${matches ? "" : " hint-off"}`} style={style}>
            {matches ? (
              <>
                <span className="hint-typed">{typed}</span>
                {label.slice(typed.length)}
              </>
            ) : (
              label
            )}
          </span>
        );
      })}
    </div>
  );
}
