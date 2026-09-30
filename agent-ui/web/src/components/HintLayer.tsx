/** HINT labels are positioned relative to the panel root after each commit, before paint.
 *  Events can move targets while HINT is active; measuring during render would use stale DOM.
 *  Detached targets get no label because their zero rect would put a phantom tag at the corner.
 *  Targets outside the visible area also get no label, so tags do not cover the surrounding UI.
 *  Their letters stay assigned by index, and landing on an offscreen target reveals its row.
 *  A user wheel scroll cancels HINT through the window scroll controller.
 *  Rows with no usable sign cell, including a clipped sliver, get a small label at their visible
 *  top-left instead of one covering the row. These labels are measured and kept clear of later labels.
 *  Placement must settle because the effect runs again after `setPlacements`. `labelHeight` uses
 *  whole pixels, `SLIVER_SLACK_PX` avoids treating nearly full-height cells as slivers, and
 *  `samePlacements` ignores sub-pixel differences. `MAX_SELF_PASSES` bounds layout changes to
 *  avoid reaching React's update-depth limit and replacing the panel with its crash fallback.
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

/** The height a label would have at its own font size, read off the style `index.css` gives every
 *  `.hint-label` (`line-height: 1`, so one line is the font size, plus the vertical padding), for a label
 *  whose box was set to a sign cell's and so says nothing about its natural size. jsdom loads no stylesheet and
 *  reports its own defaults (a 16px font, no padding: 16), which is why the tests stub it for a label. */
function naturalHeight(label: HTMLElement): number {
  const style = getComputedStyle(label);
  const height = parseFloat(style.fontSize) + parseFloat(style.paddingTop) + parseFloat(style.paddingBottom);
  return Number.isFinite(height) ? height : 0;
}

/** One label's height as a whole-pixel box height, however the label was placed. An unsized
 *  label is its own box (`offsetHeight`, which rounds); one sized to a sign cell has that cell's box, so its
 *  height is what its style says -- ROUNDED the same way. This used to be the style unrounded (31.14px at
 *  `agent.font_size` 32) against the box's 31 for the same label, so a sign cell showing 31.05px was a sliver
 *  by one measure and not by the other, and the two placements alternated on every commit. */
function labelHeight(label: HTMLElement): number {
  return label.style.width === "" ? label.offsetHeight : Math.round(naturalHeight(label));
}

/** The largest label in `layer`, or `LABEL_PX` for a dimension not yet measured. The placement
 *  effect runs after every commit, using the fallback first and measured sizes next. A
 *  sign-cell-sized label contributes only `labelHeight`, since its width belongs to the cell.
 *  Fallback row labels have their own boxes and contribute both dimensions, so larger fonts
 *  cannot overflow a clamp based on 16px. */
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
 *  same thing can. */
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
    // Place labels on the visible part of each target and clamp their tops into its clip box.
    // This keeps partially clipped targets labelled on screen instead of over the surrounding UI.
    const vis = visibleBox(target.el, root);
    const clip = vis === null ? null : clipBox(target.el, root);
    if (vis === null || clip === null) return null;
    limits[i] = clip.right - label.width - origin.left;
    const clampTop = (want: number) => Math.max(clip.top, Math.min(want, clip.bottom - label.height)) - origin.top;
    // Clamp sideways too: a link in a horizontally scrolled wide table can start left of its clip box.
    const clampLeft = (want: number) => Math.max(clip.left, Math.min(want, clip.right - label.width)) - origin.left;
    if (target.kind === "code") return { top: clampTop(vis.top + 4), left: vis.right - origin.left - 4, shift: true };
    if (target.kind === "row") {
      const sign = target.el.querySelector<HTMLElement>(".row-sign");
      const cell = sign === null ? null : visibleBox(sign, root);
      // A clipped sign cell shorter than a label would squash the label into a sliver. Use the
      // fallback below instead.
      const sliver =
        cell !== null &&
        cell.height + SLIVER_SLACK_PX < label.height &&
        cell.height < (sign?.getBoundingClientRect().height ?? 0);
      if (cell !== null && !sliver) {
        return { top: clampTop(cell.top), left: cell.left - origin.left, width: cell.width, height: cell.height };
      }
      // A missing, zero-width, or offscreen sign cell cannot hold a row label. Use a small label at
      // the visible top-left, clamped into the clip box, so an opaque label does not cover the whole row.
      return { top: clampTop(vis.top), left: clampLeft(vis.left) };
    }
    // A wrapped link bounding box can start at the row edge and place its label over unrelated text.
    // Use `firstShownLine` so the label follows the first visible line, even if earlier lines scrolled out.
    // jsdom reports no line boxes, so the bounding box is the fallback. Clamp either into the clip box.
    const r = target.el.getBoundingClientRect();
    const at = target.kind === "link" ? firstShownLine(target.el, root) : r;
    return { top: clampTop(at.top - 6), left: clampLeft(at.left - 6) };
  });
  // A large fallback row label can extend from the row's top-left into nearby link or control
  // labels. Move a later overlapping label to the fallback label's right edge when it fits in the
  // clip box. Leave it in place when there is no room. Sized sign-column labels and code labels are
  // unchanged.
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
  // with the very placement it just drew.
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
