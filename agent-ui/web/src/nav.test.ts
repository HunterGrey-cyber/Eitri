// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { clampStep, controlsOf, currentStop, nextControl, nextStop, permissionTarget, rowIndexOf, stopsIn } from "./nav";
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
