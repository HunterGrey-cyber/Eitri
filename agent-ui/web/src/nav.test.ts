// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { clampStep, controlsOf, currentStop, hintTargets, nextControl, nextStop, permissionTarget, rowIndexOf, stopsIn } from "./nav";
import type { AnswerableItem } from "./nav";

afterEach(() => {
  document.body.innerHTML = "";
});

/** A panel-shaped document: two rows (the second a permission card whose reason box sits ABOVE its
 *  buttons, as the real card's does), a banner with nothing usable in it, and a status line. */
function panel(): HTMLElement {
  document.body.innerHTML = `
    <div id="root" tabindex="0">
      <div data-nav-stop="row" id="r0">prompt</div>
      <div data-nav-stop="row" id="r1">
        <input id="reason" data-nav-order="3" />
        <button id="approve" data-nav-order="1">Approve</button>
        <button id="deny" data-nav-order="2">Deny</button>
      </div>
      <div data-nav-stop="ended">no controls here</div>
      <div data-nav-stop="status"><span>working</span><button id="stop">Stop</button></div>
      <div data-nav-stop="handoff"><button disabled>Continue in a terminal…</button></div>
    </div>`;
  return document.getElementById("root")!;
}
const byId = (id: string) => document.getElementById(id)!;

describe("clampStep", () => {
  it("clamps at both ends instead of wrapping", () => {
    expect(clampStep(3, 2, 1)).toBe(2);
    expect(clampStep(3, 0, -1)).toBe(0);
    expect(clampStep(3, 1, 1)).toBe(2);
    expect(clampStep(0, 0, 1)).toBe(0);
  });
});

describe("stops", () => {
  it("keeps every row but skips a stop with no usable control", () => {
    const root = panel();
    // The "ended" banner has no control and the handoff button is disabled, so neither is a stop.
    expect(stopsIn(root).map((s) => s.getAttribute("data-nav-stop"))).toEqual(["row", "row", "status"]);
  });

  it("walks down from the last row onto the status line, and stays there", () => {
    const root = panel();
    root.focus();
    expect(nextStop(root, 1, 1)).toBe(byId("stop").parentElement);
    byId("stop").focus();
    expect(nextStop(root, 1, 1)).toBe(byId("stop").parentElement);
    // And back up to the row the cursor was on.
    expect(rowIndexOf(root, nextStop(root, 1, -1)!)).toBe(1);
  });

  it("finds the current stop from focus first, and from the cursor otherwise", () => {
    const root = panel();
    root.focus();
    expect(currentStop(root, 0)).toBe(byId("r0"));
    byId("deny").focus();
    expect(currentStop(root, 0)).toBe(byId("r1"));
    root.focus();
    expect(currentStop(root, null)).toBeNull();
  });

  it("lands on the first stop when nothing is current yet (a start screen)", () => {
    const root = panel();
    root.focus();
    expect(nextStop(root, null, 1)).toBe(byId("r0"));
    expect(nextStop(root, null, -1)).toBe(byId("r0"));
  });
});

describe("controls", () => {
  it("orders by data-nav-order, so Approve comes before the reason box that sits above it", () => {
    panel();
    expect(controlsOf(byId("r1")).map((c) => c.id)).toEqual(["approve", "deny", "reason"]);
  });

  it("walks a row's controls left and right, and h from the first returns to the row", () => {
    const root = panel();
    root.focus();
    expect(nextControl(byId("r1"), 1)).toBe(byId("approve"));
    byId("approve").focus();
    expect(nextControl(byId("r1"), 1)).toBe(byId("deny"));
    expect(nextControl(byId("r1"), -1)).toBe("stop");
    byId("reason").focus();
    expect(nextControl(byId("r1"), 1)).toBe(byId("reason"));
  });

  it("does not leave a non-row stop by h: there is no cursor there to go back to", () => {
    panel();
    const status = byId("stop").parentElement!;
    byId("stop").focus();
    expect(nextControl(status, -1)).toBe(byId("stop"));
  });

  it("treats a stop that is itself a control as its own only control", () => {
    document.body.innerHTML = `<button data-nav-stop="mode" id="m">Auto</button>`;
    expect(controlsOf(byId("m"))).toEqual([byId("m")]);
  });
});

describe("permissionTarget", () => {
  const items: AnswerableItem[] = [
    { kind: "other" },
    { kind: "tool", toolUseId: "toolu_1" },
    { kind: "permission", toolUseId: "toolu_1" },
    { kind: "tool", toolUseId: "toolu_2" },
    { kind: "other" },
    { kind: "permission", toolUseId: null },
  ];

  it("answers the card under the cursor", () => {
    expect(permissionTarget(items, 2)).toBe(2);
    expect(permissionTarget(items, 5)).toBe(5);
  });

  it("answers the card that gates the tool call under the cursor", () => {
    expect(permissionTarget(items, 1)).toBe(2);
  });

  it("answers nothing from a row that no card is about, rather than the nearest card", () => {
    expect(permissionTarget(items, 0)).toBeNull();
    // toolu_2 has no card of its own; the next card after it belongs to nobody in particular.
    expect(permissionTarget(items, 3)).toBeNull();
    expect(permissionTarget(items, 4)).toBeNull();
    expect(permissionTarget(items, 99)).toBeNull();
  });

  it("does not link a tool call whose id is empty", () => {
    expect(permissionTarget([{ kind: "tool", toolUseId: "" }, { kind: "permission", toolUseId: "" }], 0)).toBeNull();
  });
});

/* jsdom lays nothing out, so every rect is all zeros. `rect` gives one element a fake box; `hintDoc`
   builds a panel whose list viewport is y 0..100, with every element laid out explicitly. Rects are
   set per element rather than on `HTMLElement.prototype`, which would leak into every later test. */
function rect(el: HTMLElement, top: number, height: number, left = 0, width = 100) {
  el.getBoundingClientRect = () =>
    ({ top, bottom: top + height, left, right: left + width, width, height, x: left, y: top }) as DOMRect;
}

function hintDoc(): HTMLElement {
  document.body.innerHTML = `
    <div id="root" tabindex="0">
      <div class="message-list" id="list">
        <div data-nav-stop="row" id="r0">prompt</div>
        <div data-nav-stop="row" id="r1">
          <pre class="code-block" id="code"><code>ls</code></pre>
          <button id="approve" data-nav-order="1">Approve</button>
          <button id="deny" data-nav-order="2" disabled>Deny</button>
        </div>
        <div data-nav-stop="row" id="r2">below the fold</div>
      </div>
      <div data-nav-stop="status" id="status"><button id="stop">Stop</button></div>
    </div>`;
  rect(byId("root"), 0, 200);
  rect(byId("list"), 0, 100);
  rect(byId("r0"), 0, 20);
  rect(byId("r1"), 20, 60);
  rect(byId("code"), 25, 20);
  rect(byId("approve"), 50, 20);
  rect(byId("deny"), 50, 20, 50, 40);
  rect(byId("r2"), 150, 20); // entirely below the list's viewport
  rect(byId("stop"), 180, 20);
  return byId("root");
}

describe("hintTargets", () => {
  it("takes a row inside the list's viewport and leaves out one entirely below it", () => {
    const targets = hintTargets(hintDoc());
    const rows = targets.filter((t) => t.kind === "row");
    expect(rows).toEqual([
      { kind: "row", el: byId("r0"), rowIndex: 0 },
      { kind: "row", el: byId("r1"), rowIndex: 1 },
    ]);
  });

  it("judges a row by the list's viewport, not the root's, which still contains it", () => {
    // r2 (y 150..170) is inside the root (0..200) but below the list (0..100): only the list's
    // own scrollport decides what is on screen for anything inside it.
    const root = hintDoc();
    expect(hintTargets(root).some((t) => t.el === byId("r2"))).toBe(false);
    rect(byId("r2"), 90, 20); // partly inside: a sliver on screen still counts
    expect(hintTargets(root).some((t) => t.el === byId("r2"))).toBe(true);
  });

  it("puts a row's code block and then its controls right after it, in that order", () => {
    const targets = hintTargets(hintDoc());
    expect(targets.map((t) => `${t.kind}:${t.el.id}`)).toEqual([
      "row:r0",
      "row:r1",
      "code:code",
      "control:approve",
      "control:stop",
    ]);
    expect(targets[2]).toEqual({ kind: "code", el: byId("code"), rowIndex: 1 });
  });

  it("offers the status line's Stop button, which is not a row", () => {
    const targets = hintTargets(hintDoc());
    expect(targets).toContainEqual({ kind: "control", el: byId("stop") });
  });

  it("never offers a disabled button", () => {
    const targets = hintTargets(hintDoc());
    expect(targets.some((t) => t.el === byId("deny"))).toBe(false);
  });

  it("leaves out a zero-sized element, which is how a hidden one measures", () => {
    const root = hintDoc();
    rect(byId("stop"), 180, 0); // full width, no height: its box still "intersects" the root
    expect(hintTargets(root).some((t) => t.el === byId("stop"))).toBe(false);
  });

  it("works on a start screen, which has no message list: its choice buttons are controls", () => {
    document.body.innerHTML = `
      <div id="root">
        <button data-nav-stop="choice" id="c0">New session</button>
        <button data-nav-stop="mode" id="m0">Auto</button>
      </div>`;
    rect(byId("root"), 0, 200);
    rect(byId("c0"), 10, 20);
    rect(byId("m0"), 40, 20);
    expect(hintTargets(byId("root"))).toEqual([
      { kind: "control", el: byId("c0") },
      { kind: "control", el: byId("m0") },
    ]);
  });

  it("judges a start-screen choice by its own scrolling list, not by the root", () => {
    // `.session-choice` is capped at 40vh and scrolls (index.css): a choice scrolled out of it is
    // still inside the root, and must not get a label drawn over the controls below the list.
    document.body.innerHTML = `
      <div id="root">
        <div class="session-choice" id="list">
          <button data-nav-stop="choice" id="c0">New session</button>
          <button data-nav-stop="choice" id="c1">claude 1234</button>
          <button data-nav-stop="choice" id="c2">claude 5678</button>
        </div>
        <button data-nav-stop="mode" id="m0">Auto</button>
      </div>`;
    rect(byId("root"), 0, 400);
    rect(byId("list"), 0, 100);
    rect(byId("c0"), 10, 20);
    rect(byId("c1"), 50, 20);
    rect(byId("c2"), 150, 20); // below the list's viewport, inside the root's
    rect(byId("m0"), 300, 20);
    expect(hintTargets(byId("root")).map((t) => t.el.id)).toEqual(["c0", "c1", "m0"]);
  });

  it("judges an element by any ancestor that clips its overflow, not only the known lists", () => {
    document.body.innerHTML = `
      <div id="root">
        <div id="clip" style="overflow-y: auto">
          <button data-nav-stop="mode" id="in">In</button>
          <button data-nav-stop="mode" id="out">Out</button>
        </div>
      </div>`;
    rect(byId("root"), 0, 400);
    rect(byId("clip"), 0, 100);
    rect(byId("in"), 10, 20);
    rect(byId("out"), 150, 20);
    expect(hintTargets(byId("root")).map((t) => t.el.id)).toEqual(["in"]);
  });
});
