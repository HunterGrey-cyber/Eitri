/** HINT labels are positioned relative to the panel root after each commit, before paint.
 *  Events can move targets while HINT is active; measuring during render would use stale DOM.
 *  Detached targets get no label because their zero rect would put a phantom tag at the corner.
 *  Targets outside the visible area also get no label, so tags do not cover the surrounding UI.
 *  Their letters stay assigned by index, and landing on an offscreen target reveals its row.
 *  A user wheel scroll cancels HINT through the window scroll controller.
 *  The `hint-bg`/`hint-fg` colour pair is checked for contrast in Rust (`tokens.rs`). */
import { useLayoutEffect, useState } from "react";
import type { CSSProperties } from "react";
import { firstShownLine, hintVisible } from "../nav";
import type { HintTarget } from "../nav";

export type ShownHint = { target: HintTarget; label: string };

/** Where each label goes, by the hint's index; `null` for a target no longer on the page or no
 *  longer in view. */
type Placement = { top: number; left: number; width?: number; height?: number; shift?: boolean } | null;

function place(root: HTMLElement, hints: ShownHint[]): Placement[] {
  const origin = root.getBoundingClientRect();
  return hints.map(({ target }) => {
    if (!target.el.isConnected || !hintVisible(target.el, root)) return null;
    const host = target.kind === "row" ? (target.el.querySelector<HTMLElement>(".row-sign") ?? target.el) : target.el;
    const r = host.getBoundingClientRect();
    if (target.kind === "code") return { top: r.top - origin.top + 4, left: r.right - origin.left - 4, shift: true };
    if (target.kind === "row") return { top: r.top - origin.top, left: r.left - origin.left, width: r.width, height: r.height };
    // A link that wraps onto a second line has a bounding box starting at the row's
    // left edge on its FIRST line, where its label would sit over unrelated text -- perhaps another link's.
    // The first line box that is on screen is where its label belongs (`firstShownLine`: not simply the first,
    // which may have scrolled out of the list while the link's last line is still in it). jsdom reports none,
    // so the bounding box stays the fallback.
    const at = target.kind === "link" ? firstShownLine(target.el, root) : r;
    return { top: at.top - origin.top - 6, left: at.left - origin.left - 6 };
  });
}

export function HintLayer({ root, hints, typed }: { root: HTMLElement | null; hints: ShownHint[]; typed: string }) {
  const [placements, setPlacements] = useState<Placement[]>([]);
  // No dependency list on purpose: see the doc above. The equality check is what stops this effect
  // from re-rendering itself forever.
  useLayoutEffect(() => {
    const next = root === null ? [] : place(root, hints);
    if (JSON.stringify(next) !== JSON.stringify(placements)) setPlacements(next);
  });
  if (root === null || hints.length === 0) return null;
  return (
    <div className="hint-layer" aria-hidden="true">
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
