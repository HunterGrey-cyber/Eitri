/** The panel's HINT labels: one absolutely positioned tag per frozen target, over the panel root
 *  (the conversation, or the start screen). Positions are the targets' rects relative to the root,
 *  measured after EVERY commit of the App while labels are up (a layout effect, so before paint) --
 *  not only on `hint_show`/`hint_prefix`: events keep arriving during HINT, and measuring is what
 *  keeps a label on a target that moved. After the commit, not during render: a render runs before
 *  React has applied that render's own DOM changes, so it would measure the page one update late.
 *  A target that has LEFT the page since `hint_collect` (a streamed reply re-rendering its code
 *  block, a card answered or closed) gets no label: a detached element measures as an all-zero
 *  rect, which would draw its tag at the panel's corner as a phantom, and landing on it already
 *  does nothing (`App.tsx::landOnHint`). The other labels keep their letters, because `shell`
 *  addresses them by index. A target that is still on the page but has moved OUT of view -- the
 *  list following a streamed reply to its end scrolls it past the list's edge -- gets no label
 *  either, by the same `hintVisible` test `hint_collect` froze it with: measured where it now is,
 *  its tag would be drawn over the winbar, the status line or the composer (whole-branch review).
 *  Its letter still lands on it, and landing reveals the row. A user's own wheel scroll does not
 *  come here: it cancels the whole HINT (`shell/src/hint.rs`'s window-level scroll controller, spec
 *  §2.5). None of this has been seen on a screen. The colour pair is `hint-bg`/`hint-fg`, guarded
 *  against each other in Rust (`tokens.rs`). */
import { useLayoutEffect, useRef, useState } from "react";
import type { CSSProperties } from "react";
import { clipBox, firstShownLine, visibleBox } from "../nav";
import type { HintTarget } from "../nav";

export type ShownHint = { target: HintTarget; label: string };

/** Where each label goes, by the hint's index; `null` for a target no longer on the page or no
 *  longer in view. */
type Placement = { top: number; left: number; width?: number; height?: number; shift?: boolean } | null;

/** K07: a label's size for the clamps below until one has been drawn to measure (`index.css`'s
 *  `.hint-label`: one `--fs-xs` line plus 3px padding each side, at the default 14px font). Fix round:
 *  only a fallback -- the drawn labels are measured (`labelSize`), since `agent.font_size` 32 or a
 *  `Ctrl+=` zoom makes a label about twice this, and a clamp with 16 would let it overhang the clip box.
 *  jsdom measures nothing, so the fallback is also what its tests see. */
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
    // K07: everything is placed on the part of the target that shows, and every label's top is clamped
    // into the box that part shows in, so a target scrolled half out of the list -- or a sliver at
    // either edge -- keeps its label on screen rather than over the band above the list or past its
    // end. A label on screen matters more than one flush with its target (Vimium's rule: a hint on the
    // visible part of its target).
    const vis = visibleBox(target.el, root);
    const clip = vis === null ? null : clipBox(target.el, root);
    if (vis === null || clip === null) return null;
    const clampTop = (want: number) => Math.max(clip.top, Math.min(want, clip.bottom - label.height)) - origin.top;
    // Fix round: sideways too -- a link in a wide table scrolled sideways starts left of its box.
    const clampLeft = (want: number) => Math.max(clip.left, Math.min(want, clip.right - label.width)) - origin.left;
    if (target.kind === "code") return { top: clampTop(vis.top + 4), left: vis.right - origin.left - 4, shift: true };
    if (target.kind === "row") {
      const sign = target.el.querySelector<HTMLElement>(".row-sign");
      const cell = (sign === null ? null : visibleBox(sign, root)) ?? vis;
      return { top: clampTop(cell.top), left: cell.left - origin.left, width: cell.width, height: cell.height };
    }
    // A link (v1 picks, Task 8, R6) that wraps onto a second line has a bounding box starting at the row's
    // left edge on its FIRST line, where its label would sit over unrelated text -- perhaps another link's.
    // The first line box that is on screen is where its label belongs (`firstShownLine`: not simply the first,
    // which may have scrolled out of the list while the link's last line is still in it). jsdom reports none,
    // so the bounding box stays the fallback. Either way the label is clamped into the clip box (K07).
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
