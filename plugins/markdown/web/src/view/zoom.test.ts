import { describe, expect, it } from "vitest";
import {
  carry, clampView, fitScale, fitted, fittedWidth, isFitted, MAX_ZOOM, PAN_STEP, panBy, panSteps, parseViewBox,
  scaleLimits, toEdge, viewBoxOf, ZOOM_STEP, zoomBy, zoomSteps, zoomTo, type View,
} from "./zoom";

// A wide diagram (2000 x 500 picture units) in an 800 x 600 viewer: it fits at 0.4, by its width.
const PIC = { w: 2000, h: 500 };
const BOX = { w: 800, h: 600 };
const close = (a: View, b: View) => {
  expect(a.scale).toBeCloseTo(b.scale, 9);
  expect(a.x).toBeCloseTo(b.x, 9);
  expect(a.y).toBeCloseTo(b.y, 9);
};

describe("fitting", () => {
  it("fits the whole picture by its tighter side and centres the other", () => {
    expect(fitScale(PIC, BOX)).toBeCloseTo(0.4);
    // 600 px of viewer show 1500 units of a 500-unit-tall picture: it sits in the middle.
    close(fitted(PIC, BOX), { scale: 0.4, x: 0, y: -500 });
    expect(isFitted(fitted(PIC, BOX), PIC, BOX)).toBe(true);
  });

  it("fits a tall picture by its height", () => {
    const tall = { w: 300, h: 3000 };
    expect(fitScale(tall, BOX)).toBeCloseTo(0.2);
    close(fitted(tall, BOX), { scale: 0.2, x: (300 - 4000) / 2, y: 0 });
  });

  it("enlarges a small picture to fit, and fits a width", () => {
    const small = { w: 100, h: 100 };
    expect(fitScale(small, BOX)).toBeCloseTo(6);
    const w = fittedWidth({ w: 400, h: 4000 }, BOX);
    expect(w.scale).toBeCloseTo(2);
    expect(w.x).toBeCloseTo(0);
    expect(w.y).toBe(0);
  });

  it("treats a picture or a viewer without a size as scale 1, never NaN", () => {
    expect(fitScale({ w: 0, h: 10 }, BOX)).toBe(1);
    expect(fitScale(PIC, { w: 0, h: 0 })).toBe(1);
    const v = clampView({ scale: Number.NaN, x: Number.NaN, y: 1 }, PIC, BOX);
    expect(Number.isFinite(v.scale) && Number.isFinite(v.x) && Number.isFinite(v.y)).toBe(true);
  });
});

describe("clamps", () => {
  it("hold the scale between the smaller and sixteen times the larger of fitted and 100%", () => {
    expect(scaleLimits(PIC, BOX)).toEqual({ min: 0.4, max: MAX_ZOOM });
    const small = { w: 100, h: 100 };
    expect(scaleLimits(small, BOX).min).toBe(1);
    expect(scaleLimits(small, BOX).max).toBeCloseTo(6 * MAX_ZOOM);
    expect(clampView({ scale: 0.01, x: 0, y: 0 }, PIC, BOX).scale).toBeCloseTo(0.4);
    expect(clampView({ scale: 1e6, x: 0, y: 0 }, PIC, BOX).scale).toBe(MAX_ZOOM);
  });

  it("keep the view inside the picture where it is larger than the viewer", () => {
    // At 1:1 the viewer shows 800 x 600 of the 2000 x 500 picture: x runs 0..1200, y is centred.
    close(clampView({ scale: 1, x: -50, y: 40 }, PIC, BOX), { scale: 1, x: 0, y: -50 });
    close(clampView({ scale: 1, x: 5000, y: 0 }, PIC, BOX), { scale: 1, x: 1200, y: -50 });
  });

  it("go to each edge, the other axis kept", () => {
    const v = { scale: 2, x: 300, y: 100 };
    close(toEdge(v, "left", PIC, BOX), { scale: 2, x: 0, y: 100 });
    close(toEdge(v, "right", PIC, BOX), { scale: 2, x: 2000 - 400, y: 100 });
    close(toEdge(v, "top", PIC, BOX), { scale: 2, x: 300, y: 0 });
    close(toEdge(v, "bottom", PIC, BOX), { scale: 2, x: 300, y: 500 - 300 });
  });
});

describe("zoom", () => {
  it("around the viewer's centre keeps the centre's picture point", () => {
    const v = { scale: 1, x: 400, y: 0 };
    const centre = (w: View) => [w.x + BOX.w / 2 / w.scale, w.y + BOX.h / 2 / w.scale];
    const before = centre(clampView(v, PIC, BOX));
    const after = zoomSteps(clampView(v, PIC, BOX), 2, PIC, BOX);
    expect(after.scale).toBeCloseTo(ZOOM_STEP ** 2);
    expect(centre(after)[0]).toBeCloseTo(before[0]);
    expect(centre(after)[1]).toBeCloseTo(before[1]);
    // Out again by the same count is where it began.
    close(zoomSteps(after, -2, PIC, BOX), clampView(v, PIC, BOX));
  });

  it("at the pointer keeps the picture point under the pointer", () => {
    const v = { scale: 1, x: 300, y: 0 };
    const px = 100;
    const py = 250;
    const under = (w: View) => [w.x + px / w.scale, w.y + py / w.scale];
    const at = clampView(v, PIC, BOX);
    const z = zoomBy(at, 3, px, py, PIC, BOX);
    expect(z.scale).toBeCloseTo(3);
    expect(under(z)[0]).toBeCloseTo(under(at)[0]);
    expect(under(z)[1]).toBeCloseTo(under(at)[1]);
  });

  it("to a scale is held to the limits", () => {
    expect(zoomTo(fitted(PIC, BOX), 0.01, 0, 0, PIC, BOX).scale).toBeCloseTo(0.4);
    expect(zoomTo(fitted(PIC, BOX), 100, 0, 0, PIC, BOX).scale).toBe(MAX_ZOOM);
  });
});

describe("pan", () => {
  it("moves a fifth of the viewer per step, a count multiplying it", () => {
    const v = { scale: 1, x: 0, y: -50 };
    close(panSteps(v, 1, 0, PIC, BOX), { scale: 1, x: PAN_STEP * 800, y: -50 });
    close(panSteps(v, 3, 0, PIC, BOX), { scale: 1, x: 3 * PAN_STEP * 800, y: -50 });
    // Screen px are picture units over the scale.
    close(panBy({ scale: 2, x: 0, y: 0 }, 100, 60, PIC, BOX), { scale: 2, x: 50, y: 30 });
  });

  it("stops at the edges", () => {
    close(panSteps({ scale: 1, x: 1100, y: -50 }, 5, 0, PIC, BOX), { scale: 1, x: 1200, y: -50 });
    close(panSteps({ scale: 1, x: 100, y: -50 }, -5, 0, PIC, BOX), { scale: 1, x: 0, y: -50 });
  });
});

describe("carrying a view to a picture that changed", () => {
  it("fits again when the view was fitted", () => {
    const to = { w: 1000, h: 1000 };
    close(carry(fitted(PIC, BOX), true, PIC, to, BOX, BOX), fitted(to, BOX));
  });

  it("keeps the scale and the centre's share of the picture otherwise", () => {
    const v = { scale: 2, x: 800, y: 100 }; // centre at (1000, 250): half across, half down
    const to = { w: 3000, h: 500 };
    const c = carry(v, false, PIC, to, BOX, BOX);
    expect(c.scale).toBe(2);
    expect(c.x + BOX.w / 2 / c.scale).toBeCloseTo(1500);
    expect(c.y + BOX.h / 2 / c.scale).toBeCloseTo(250);
  });

  it("holds the carried view to the new picture's limits", () => {
    const c = carry({ scale: 1, x: 1200, y: -50 }, false, PIC, { w: 900, h: 200 }, BOX, BOX);
    expect(c.x).toBeCloseTo(100);
  });
});

describe("the SVG viewBox", () => {
  it("is the view in picture units from the diagram's own origin", () => {
    expect(viewBoxOf({ scale: 2, x: 10, y: 20 }, BOX, { x: -8, y: -8 })).toBe("2 12 400 300");
    expect(viewBoxOf({ scale: 3, x: 0, y: 0 }, { w: 100, h: 100 }, { x: 0, y: 0 })).toBe("0 0 33.333 33.333");
  });

  it("reads Mermaid's viewBox and refuses a bad one", () => {
    expect(parseViewBox("-8 -8 516.5 300")).toEqual({ x: -8, y: -8, w: 516.5, h: 300 });
    expect(parseViewBox("0,0,10,20")).toEqual({ x: 0, y: 0, w: 10, h: 20 });
    for (const bad of [null, "", "0 0 10", "0 0 0 10", "a b c d", "0 0 -1 5"]) expect(parseViewBox(bad)).toBeNull();
  });
});
