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
import { useLayoutEffect, useState } from "react";
import type { CSSProperties } from "react";
import { hintVisible } from "../nav";
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
    return { top: r.top - origin.top - 6, left: r.left - origin.left - 6 };
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
