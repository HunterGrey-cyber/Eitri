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
 *  §2.5). A row whose sign cell has no width or has scrolled away is labelled like a link, small, at the
 *  row's visible top-left -- never sized to the row (rc.4 item 5); so does one whose sign cell only a sliver
 *  of shows, and such a label is measured like any other and never covered by a later label (its fix round).
 *  Placement has to SETTLE, because this effect re-runs after its own `setPlacements` (rc.4 review): a label's
 *  height is read one way whatever kind of label it is (`labelHeight`: a whole-pixel box height), a sign cell is a sliver only when it is
 *  clearly shorter than a label (`SLIVER_SLACK_PX`), two placements a sub-pixel apart are the same one
 *  (`samePlacements`), and a layer whose own re-placing keeps changing the layout stops after `MAX_SELF_PASSES`
 *  rather than looping until React's update-depth limit replaces the whole panel with its crash fallback.
 *  None of this has been seen on a screen. The colour pair is `hint-bg`/`hint-fg`, guarded
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

/** The height a label would have at its own font size, read off the style `index.css` gives every
 *  `.hint-label` (`line-height: 1`, so one line is the font size, plus the vertical padding), for a label
 *  whose box was set to a sign cell's and so says nothing about its natural size. jsdom loads no stylesheet and
 *  reports its own defaults (a 16px font, no padding: 16), which is why the tests stub it for a label. */
function naturalHeight(label: HTMLElement): number {
  const style = getComputedStyle(label);
  const height = parseFloat(style.fontSize) + parseFloat(style.paddingTop) + parseFloat(style.paddingBottom);
  return Number.isFinite(height) ? height : 0;
}

/** One label's height as a whole-pixel box height, however the label was placed (rc.4 review, Codex). An unsized
 *  label is its own box (`offsetHeight`, which rounds); one sized to a sign cell has that cell's box, so its
 *  height is what its style says -- ROUNDED the same way. This used to be the style unrounded (31.14px at
 *  `agent.font_size` 32) against the box's 31 for the same label, so a sign cell showing 31.05px was a sliver
 *  by one measure and not by the other, and the two placements alternated on every commit. */
function labelHeight(label: HTMLElement): number {
  return label.style.width === "" ? label.offsetHeight : Math.round(naturalHeight(label));
}

/** The largest label drawn in `layer` (a two-letter label is wider than a one-letter one), or `LABEL_PX` for
 *  a dimension nothing has been drawn to measure yet. The placement effect runs after every commit, so the
 *  first pass uses the fallback and the next one the measured size. A label sized to its sign cell has a width
 *  that says nothing about a label's own, so only its height is read (`labelHeight`); a row's FALLBACK label
 *  (no usable sign cell, `place`) has a box of its own and gives its width too -- fix round, rc.4 item 5: they
 *  used to be left out, so at `agent.font_size` 32 a label of ~31px was clamped as if it were 16 and spilled
 *  past the list. */
function labelSize(layer: HTMLElement | null): { width: number; height: number } {
  let width = 0;
  let height = 0;
  for (const label of layer?.querySelectorAll<HTMLElement>(".hint-label") ?? []) {
    if (label.style.width === "") width = Math.max(width, label.offsetWidth);
    height = Math.max(height, labelHeight(label));
  }
  return { width: width > 0 ? width : LABEL_PX, height: height > 0 ? height : LABEL_PX };
}

/** A sign cell is a sliver -- too short to carry a label squashed into it -- only when it is clearly shorter
 *  than a label, not when the two merely disagree by the sub-pixel a rounded and a fractional measure of the
 *  same thing can (rc.4 review: the boundary case that used to flip on every pass). */
const SLIVER_SLACK_PX = 1;

/** Two placements closer than this are the same one: a fractional rect differs by float noise between
 *  commits, and re-placing for it would only start another commit. Half a CSS pixel is under a device pixel
 *  at any scale this panel is drawn at 1x and above, and a label never drifts further than that from its
 *  target, since the comparison is always against what is drawn. */
const SAME_PLACEMENT_PX = 0.5;

/** The most commits in a row that this layer's own `setPlacements` may cause before it keeps what is drawn.
 *  A legitimate placement settles in two or three (the first uses the fallback size, the next the measured
 *  one, and a fallback label's own width can move a neighbour); a layout that still changes after that is
 *  one the labels themselves keep changing, and looping on it ends in React's "Maximum update depth
 *  exceeded" and the panel's crash fallback. */
const MAX_SELF_PASSES = 4;

function near(a: number | undefined, b: number | undefined): boolean {
  if (a === undefined || b === undefined) return a === b;
  return Math.abs(a - b) < SAME_PLACEMENT_PX;
}

function samePlacement(a: Placement | undefined, b: Placement | undefined): boolean {
  if (a === undefined || b === undefined || a === null || b === null) return a === b;
  return (
    (a.shift === true) === (b.shift === true) &&
    near(a.top, b.top) &&
    near(a.left, b.left) &&
    near(a.width, b.width) &&
    near(a.height, b.height)
  );
}

function samePlacements(a: Placement[], b: Placement[]): boolean {
  return a.length === b.length && a.every((p, i) => samePlacement(p, b[i]));
}

type Rect = { top: number; left: number; right: number; bottom: number };

function place(root: HTMLElement, hints: ShownHint[], label: { width: number; height: number }): Placement[] {
  const origin = root.getBoundingClientRect();
  // Each placed label's furthest right (root-relative), for the nudge below.
  const limits: Array<number | null> = [];
  const placed = hints.map(({ target }, i): Placement => {
    limits[i] = null;
    if (!target.el.isConnected) return null;
    // K07: everything is placed on the part of the target that shows, and every label's top is clamped
    // into the box that part shows in, so a target scrolled half out of the list -- or a sliver at
    // either edge -- keeps its label on screen rather than over the band above the list or past its
    // end. A label on screen matters more than one flush with its target (Vimium's rule: a hint on the
    // visible part of its target).
    const vis = visibleBox(target.el, root);
    const clip = vis === null ? null : clipBox(target.el, root);
    if (vis === null || clip === null) return null;
    limits[i] = clip.right - label.width - origin.left;
    const clampTop = (want: number) => Math.max(clip.top, Math.min(want, clip.bottom - label.height)) - origin.top;
    // Fix round: sideways too -- a link in a wide table scrolled sideways starts left of its box.
    const clampLeft = (want: number) => Math.max(clip.left, Math.min(want, clip.right - label.width)) - origin.left;
    if (target.kind === "code") return { top: clampTop(vis.top + 4), left: vis.right - origin.left - 4, shift: true };
    if (target.kind === "row") {
      const sign = target.el.querySelector<HTMLElement>(".row-sign");
      const cell = sign === null ? null : visibleBox(sign, root);
      // Fix round, rc.4 item 5: a sign cell that only a sliver of shows -- clipped, and what shows shorter
      // than a label -- would squash its label to that sliver. It takes the fallback below instead.
      const sliver =
        cell !== null &&
        cell.height + SLIVER_SLACK_PX < label.height &&
        cell.height < (sign?.getBoundingClientRect().height ?? 0);
      if (cell !== null && !sliver) {
        return { top: clampTop(cell.top), left: cell.left - origin.left, width: cell.width, height: cell.height };
      }
      // rc.4 item 5: a row with no usable sign cell -- a prose row's has no width, another's has scrolled out
      // of the list, a third has none at all -- used to be labelled on the row's OWN box (`?? vis`), an opaque
      // label the size of the row's whole visible part laid over its text. It gets a normal small label at
      // the row's visible top-left instead, clamped into the clip box like every other one (K07).
      return { top: clampTop(vis.top), left: clampLeft(vis.left) };
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
  // Fix round, rc.4 item 5 (Codex): a row's fallback label sits at the row's top-left and, once labels are
  // big (`agent.font_size` 32, a zoom), reaches past the sign column into the text, where a link or control
  // on the row's first line has its own label a few px to the right -- which, drawn later, hid the row
  // label's last letter. A later label that would overlap an earlier fallback row label moves to its right
  // edge (kept inside the clip box; with no room it stays), so neither is covered. Sized labels, which sit
  // inside their sign column, and code labels, which hang left of the block's right edge, are left alone.
  const rowLabels: Rect[] = [];
  placed.forEach((p, i) => {
    if (p === null || p.width !== undefined || p.shift === true) return;
    if (hints[i].target.kind === "row") {
      rowLabels.push({ top: p.top, left: p.left, right: p.left + label.width, bottom: p.top + label.height });
      return;
    }
    for (const r of rowLabels) {
      const hit = p.top < r.bottom && p.top + label.height > r.top && p.left < r.right && p.left + label.width > r.left;
      const room = limits[i];
      if (hit && room !== null && r.right <= room) p.left = r.right;
    }
  });
  return placed;
}

export function HintLayer({ root, hints, typed }: { root: HTMLElement | null; hints: ShownHint[]; typed: string }) {
  const [placements, setPlacements] = useState<Placement[]>([]);
  const layerRef = useRef<HTMLDivElement>(null);
  // Whether the commit this effect is about to run for was caused by its own `setPlacements` below, and how many
  // in a row were (`MAX_SELF_PASSES`).
  const ownUpdate = useRef(false);
  const selfPasses = useRef(0);
  // No dependency list on purpose: see the doc above. The equality check is what stops this effect from
  // re-rendering itself forever, and `MAX_SELF_PASSES` is what stops it if a measure it reads ever changes
  // with the very placement it just drew (rc.4 review).
  useLayoutEffect(() => {
    selfPasses.current = ownUpdate.current ? selfPasses.current + 1 : 0;
    ownUpdate.current = false;
    const next = root === null ? [] : place(root, hints, labelSize(layerRef.current));
    if (samePlacements(next, placements) || selfPasses.current >= MAX_SELF_PASSES) return;
    ownUpdate.current = true;
    setPlacements(next);
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
