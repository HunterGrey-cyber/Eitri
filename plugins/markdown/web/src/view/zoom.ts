/* The diagram viewer's geometry, free of the DOM: what part of a picture (a diagram's SVG user space, or an image's
   pixels) the viewer shows, and how keys and the pointer move it. A view is the scale (screen px per picture unit)
   and the picture point at the viewer's top-left corner. Every function returns a view held to `clampView`. */

export interface Size { w: number; h: number }
export interface View { scale: number; x: number; y: number }

/** One `h`/`j`/`k`/`l` step: a fifth of the viewer's width or height, as nsxiv pans. */
export const PAN_STEP = 0.2;
/** One `+`/`-` step: three steps double the size. */
export const ZOOM_STEP = 1.25;
/** How far in the viewer zooms: sixteen times the larger of the fitted size and the picture's own. */
export const MAX_ZOOM = 16;

const usable = (s: Size) => s.w > 0 && s.h > 0 && Number.isFinite(s.w) && Number.isFinite(s.h);

/** The scale that shows the whole picture (zathura's best fit, nsxiv's `W`); 1 for a picture or viewer with no size. */
export function fitScale(pic: Size, box: Size): number {
  return usable(pic) && usable(box) ? Math.min(box.w / pic.w, box.h / pic.h) : 1;
}

/** The scale that fills the viewer's width (zathura's `s`). */
export function fitWidthScale(pic: Size, box: Size): number {
  return usable(pic) && usable(box) ? box.w / pic.w : 1;
}

/** The scales a view may take: down to the fitted size or the picture's own, whichever is smaller (nothing is gained
 *  by a picture smaller than both), up to `MAX_ZOOM` times the larger of them. */
export function scaleLimits(pic: Size, box: Size): { min: number; max: number } {
  const fit = fitScale(pic, box);
  return { min: Math.min(fit, 1), max: Math.max(fit, 1) * MAX_ZOOM };
}

/* One axis: a picture narrower than the viewer is centred; a wider one cannot be moved past its edges. */
function clampAxis(at: number, pic: number, visible: number): number {
  if (pic <= visible) return (pic - visible) / 2;
  return Math.min(Math.max(at, 0), pic - visible);
}

/** `v` with its scale inside `scaleLimits` and its position kept inside the picture. */
export function clampView(v: View, pic: Size, box: Size): View {
  const { min, max } = scaleLimits(pic, box);
  const scale = Number.isFinite(v.scale) && v.scale > 0 ? Math.min(Math.max(v.scale, min), max) : fitScale(pic, box);
  return {
    scale,
    x: clampAxis(Number.isFinite(v.x) ? v.x : 0, pic.w, box.w / scale),
    y: clampAxis(Number.isFinite(v.y) ? v.y : 0, pic.h, box.h / scale),
  };
}

/** The whole picture, centred. */
export function fitted(pic: Size, box: Size): View {
  return clampView({ scale: fitScale(pic, box), x: 0, y: 0 }, pic, box);
}

/** The whole picture when it is larger than the viewer, else at 100%, centred: nsxiv's default scale mode
 *  ("down"), how an image opens. */
export function fittedDown(pic: Size, box: Size): View {
  return clampView({ scale: Math.min(fitScale(pic, box), 1), x: 0, y: 0 }, pic, box);
}

/** The picture filling the viewer's width, its top at the viewer's top. */
export function fittedWidth(pic: Size, box: Size): View {
  return clampView({ scale: fitWidthScale(pic, box), x: 0, y: 0 }, pic, box);
}

/** `v` at `scale`, the picture point under the viewer point (`px`, `py`) staying under it: the pointer for
 *  Ctrl+wheel and a double-click, the viewer's centre for keys. */
export function zoomTo(v: View, scale: number, px: number, py: number, pic: Size, box: Size): View {
  const { min, max } = scaleLimits(pic, box);
  const next = Math.min(Math.max(scale, min), max);
  const cx = v.x + px / v.scale;
  const cy = v.y + py / v.scale;
  return clampView({ scale: next, x: cx - px / next, y: cy - py / next }, pic, box);
}

/** `v` zoomed by `factor` around the viewer point (`px`, `py`). */
export function zoomBy(v: View, factor: number, px: number, py: number, pic: Size, box: Size): View {
  return zoomTo(v, v.scale * factor, px, py, pic, box);
}

/** `v` zoomed `steps` `ZOOM_STEP`s (negative: out) around the viewer's centre. */
export function zoomSteps(v: View, steps: number, pic: Size, box: Size): View {
  return zoomBy(v, ZOOM_STEP ** steps, box.w / 2, box.h / 2, pic, box);
}

/** `v` moved by (`dx`, `dy`) screen px: positive is right and down, toward the picture's right and bottom edges. */
export function panBy(v: View, dx: number, dy: number, pic: Size, box: Size): View {
  return clampView({ scale: v.scale, x: v.x + dx / v.scale, y: v.y + dy / v.scale }, pic, box);
}

/** `v` moved `steps` `PAN_STEP`s across (`sx`) and down (`sy`). */
export function panSteps(v: View, sx: number, sy: number, pic: Size, box: Size): View {
  return panBy(v, sx * PAN_STEP * box.w, sy * PAN_STEP * box.h, pic, box);
}

export type Edge = "left" | "right" | "top" | "bottom";

/** `v` at one of the picture's edges (`0`, `$`, `gg`, `G`), the other axis where it was. */
export function toEdge(v: View, edge: Edge, pic: Size, box: Size): View {
  const far = Number.MAX_SAFE_INTEGER;
  switch (edge) {
    case "left": return clampView({ ...v, x: -far }, pic, box);
    case "right": return clampView({ ...v, x: far }, pic, box);
    case "top": return clampView({ ...v, y: -far }, pic, box);
    case "bottom": return clampView({ ...v, y: far }, pic, box);
  }
}

/** Whether `v` is the fitted view (within a part in a thousand of its scale). */
export function isFitted(v: View, pic: Size, box: Size): boolean {
  return Math.abs(v.scale / fitScale(pic, box) - 1) < 1e-3;
}

/** The view carried over to a picture that changed size (the diagram's source was edited) or a viewer that did:
 *  a fitted view fits again; otherwise the scale stays and the point at the viewer's centre stays at the same share
 *  of the picture's width and height. */
export function carry(v: View, wasFit: boolean, from: Size, to: Size, fromBox: Size, toBox: Size): View {
  if (wasFit) return fitted(to, toBox);
  if (!usable(from) || !usable(to)) return clampView(v, to, toBox);
  const fx = (v.x + fromBox.w / v.scale / 2) / from.w;
  const fy = (v.y + fromBox.h / v.scale / 2) / from.h;
  const cx = fx * to.w;
  const cy = fy * to.h;
  return clampView({ scale: v.scale, x: cx - toBox.w / v.scale / 2, y: cy - toBox.h / v.scale / 2 }, to, toBox);
}

/** The SVG `viewBox` showing `v` in a viewer of size `box`, for a picture whose own `viewBox` starts at `origin`. */
export function viewBoxOf(v: View, box: Size, origin: { x: number; y: number }): string {
  const r = (n: number) => String(Math.round(n * 1000) / 1000);
  return `${r(origin.x + v.x)} ${r(origin.y + v.y)} ${r(box.w / v.scale)} ${r(box.h / v.scale)}`;
}

/** An SVG's own `viewBox` (Mermaid writes one on every diagram), or null when it has none that is usable. */
export function parseViewBox(text: string | null): { x: number; y: number; w: number; h: number } | null {
  if (!text) return null;
  const n = text.trim().split(/[\s,]+/).map(Number);
  if (n.length !== 4 || n.some((x) => !Number.isFinite(x)) || n[2] <= 0 || n[3] <= 0) return null;
  return { x: n[0], y: n[1], w: n[2], h: n[3] };
}
