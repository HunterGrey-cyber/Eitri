// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { activate, assignLabels, collectTargets } from "./hints";
import { install, setCurrent } from "./install";
import { GENERIC_ROWS } from "./rows";

let cleanup: (() => void) | undefined;
let events: Map<string, (data: unknown) => void>;

function box(el: Element, x = 10, y = 10, width = 30, height = 20) {
  vi.spyOn(el, "getBoundingClientRect").mockReturnValue({
    x, y, left: x, top: y, right: x + width, bottom: y + height, width, height,
    toJSON: () => ({}),
  });
  vi.spyOn(el, "clientWidth", "get").mockReturnValue(width);
  vi.spyOn(el, "clientHeight", "get").mockReturnValue(height);
  return el;
}

function button(x = 10, y = 10) {
  const el = document.createElement("button");
  document.body.append(el);
  box(el, x, y);
  return el;
}

function key(value: string, init: KeyboardEventInit = {}) {
  const ev = new KeyboardEvent("keydown", { key: value, bubbles: true, cancelable: true, ...init });
  (document.activeElement ?? document.body).dispatchEvent(ev);
  return ev;
}

function labels() {
  return [...document.querySelectorAll<HTMLElement>(".eitri-hint")];
}

async function start(targets?: () => { el: Element; rank?: number }[]) {
  const run = vi.fn(() => true);
  cleanup = await install({ rows: [], run, targets });
  return run;
}

beforeEach(() => {
  events = new Map();
  window.eitriPlugin = {
    call: vi.fn(async () => ({ rows: GENERIC_ROWS })),
    on: (name, fn) => { events.set(name, fn); return () => { events.delete(name); }; },
  };
});

afterEach(() => {
  cleanup?.();
  cleanup = undefined;
  setCurrent(null);
  document.body.replaceChildren();
  vi.restoreAllMocks();
});

describe("hint targets", () => {
  it("excludes disabled, hidden, zero-size and fully clipped targets, including extra targets", () => {
    const visible = button();
    const disabled = button();
    disabled.disabled = true;
    const ariaDisabled = button();
    ariaDisabled.setAttribute("aria-disabled", "true");
    const hidden = button();
    hidden.setAttribute("aria-hidden", "true");
    const zero = button();
    box(zero, 10, 10, 0, 20);
    const clip = document.createElement("div");
    // Explicit axes avoid relying on shorthand expansion in the DOM emulator.
    clip.style.overflowX = clip.style.overflowY = "hidden";
    document.body.append(clip);
    box(clip, 0, 0, 50, 50);
    const clipped = button(80, 10);
    clip.append(clipped);
    const marked = document.createElement("span");
    marked.dataset.eitriTarget = "";
    document.body.append(marked);
    box(marked);
    expect(collectTargets(document, [{ el: clipped }, { el: disabled }]).map(({ el }) => el))
      .toEqual([visible, marked]);
  });

  it("includes every supported native, role, tabindex and added target once", () => {
    document.body.innerHTML = '<a href="#x"></a><button></button><summary></summary><input><select></select>' +
      '<textarea></textarea><div role="button"></div><div role="tab"></div><div role="option"></div>' +
      '<div role="checkbox"></div><div role="switch"></div><div role="menuitem"></div>' +
      '<div tabindex="0"></div><div tabindex="2"></div><div tabindex="-1"></div><span></span>';
    const elements = [...document.body.children];
    elements.forEach((el) => box(el));
    const result = collectTargets(document, [{ el: elements[0], rank: 0 }, { el: elements[15] }]);
    expect(result.map(({ el }) => el)).toEqual([...elements.slice(0, 14), elements[15]]);
    expect(result[0].rank).toBe(0);
  });

  it("excludes targets hidden by ancestors or disabled fieldsets", () => {
    document.body.innerHTML = '<div aria-hidden="true"><button></button></div>' +
      '<fieldset disabled><button></button></fieldset><div style="visibility:hidden"><button></button></div>';
    document.querySelectorAll("button").forEach((el) => box(el));
    expect(collectTargets(document, [])).toEqual([]);
  });

  it("includes a visible target that overrides an ancestor's visibility", () => {
    const parent = document.createElement("div");
    parent.style.visibility = "hidden";
    const el = button();
    el.style.visibility = "visible";
    parent.append(el);
    document.body.append(parent);
    expect(collectTargets(document, []).map(({ el }) => el)).toEqual([el]);
  });

  it("intersects clipping ancestors with the viewport and retains partly visible targets", () => {
    const outer = document.createElement("div");
    outer.style.overflowX = outer.style.overflowY = "auto";
    box(outer, -50, 0, 100, 100);
    document.body.append(outer);
    const partial = button(-10, 10);
    const outsideViewport = button(-40, 10);
    box(outsideViewport, -40, 10, 20, 20);
    const inner = document.createElement("div");
    inner.style.overflowX = inner.style.overflowY = "clip";
    box(inner, 30, 0, 50, 100);
    const between = button(55, 10);
    inner.append(between);
    outer.append(partial, outsideViewport, inner);
    expect(collectTargets(document, []).map(({ el }) => el)).toEqual([partial]);
  });

  it("clips only the axes whose overflow clips", () => {
    const clip = document.createElement("div");
    clip.style.overflowX = "hidden";
    clip.style.overflowY = "visible";
    box(clip, 0, 0, 50, 50);
    document.body.append(clip);
    const below = button(10, 80);
    const right = button(80, 10);
    clip.append(below, right);
    expect(collectTargets(document, []).map(({ el }) => el)).toEqual([below]);
  });

  it("clips at the client box even when a border leaves its client width zero", () => {
    const clip = document.createElement("div");
    clip.style.overflowX = clip.style.overflowY = "hidden";
    box(clip, 0, 0, 50, 50);
    vi.spyOn(clip, "clientWidth", "get").mockReturnValue(0);
    const el = button();
    clip.append(el);
    document.body.append(clip);
    expect(collectTargets(document, [])).toEqual([]);
  });

  it.each([[2, 150, true], [0.5, 75, false]] as const)(
    "intersects the viewport client box for scale %s", (scale, x, visible) => {
      const clip = document.createElement("div");
      clip.style.overflowX = clip.style.overflowY = "hidden";
      clip.style.transform = `scale(${scale})`;
      box(clip, 0, 0, 100 * scale, 100 * scale);
      vi.spyOn(clip, "clientWidth", "get").mockReturnValue(100);
      vi.spyOn(clip, "clientHeight", "get").mockReturnValue(100);
      vi.spyOn(clip, "offsetWidth", "get").mockReturnValue(100);
      vi.spyOn(clip, "offsetHeight", "get").mockReturnValue(100);
      const el = button(x, 10);
      clip.append(el);
      document.body.append(clip);
      expect(collectTargets(document, []).map(({ el }) => el)).toEqual(visible ? [el] : []);
    },
  );
});

describe("hint labels", () => {
  it("uses one character for each of fourteen targets", () => {
    const targets = Array.from({ length: 14 }, () => ({ el: button() }));
    expect([...assignLabels(targets, { x: 0, y: 0 }).values()]).toEqual([..."sadfjklewcmpgh"]);
  });

  it.each([15, 200, 210])("assigns %s unique labels with no label a prefix of another", (count) => {
    const targets = Array.from({ length: count }, () => ({ el: button() }));
    const values = [...assignLabels(targets, { x: 0, y: 0 }).values()];
    expect(values).toHaveLength(count);
    expect(new Set(values).size).toBe(count);
    for (const value of values) {
      expect(value).toMatch(/^[sadfjklewcmpgh]+$/);
      expect(values.some((other) => other !== value && other.startsWith(value))).toBe(false);
    }
    expect(values.map((value) => value.length)).toEqual(values.map((value) => value.length).sort((a, b) => a - b));
  });

  it("gives rank zero the first label even when farther away, then sorts by distance", () => {
    const near = button(0, 0);
    const middle = button(100, 100);
    const far = button(500, 500);
    const result = assignLabels([{ el: middle }, { el: near }, { el: far, rank: 0 }], { x: 0, y: 0 });
    expect([...result]).toEqual([[far, "s"], [near, "a"], [middle, "d"]]);
  });

  it("supports a supplied alphabet and no targets", () => {
    expect([...assignLabels([], { x: 0, y: 0 })]).toEqual([]);
    const targets = Array.from({ length: 3 }, () => ({ el: button() }));
    const values = [...assignLabels(targets, { x: 0, y: 0 }, "ab").values()];
    expect(values).toHaveLength(3);
    expect(values.every((value) => /^[ab]+$/.test(value))).toBe(true);
    expect(values.some((value) => values.some((other) => other !== value && other.startsWith(value)))).toBe(false);
  });

  it("focuses before clicking", () => {
    const el = button();
    let focused: Element | null = null;
    el.addEventListener("click", () => { focused = document.activeElement; });
    activate(el);
    expect(focused).toBe(el);
  });
});

describe("installed hint mode", () => {
  it("enters on f and typing a full label clicks once without calling the page action", async () => {
    const el = button();
    let clicks = 0;
    el.addEventListener("click", () => clicks++);
    const run = await start();
    expect(key("f").defaultPrevented).toBe(true);
    expect(document.getElementById("eitri-keys-hud")!.dataset.mode).toBe("hint");
    expect(labels()).toHaveLength(1);
    expect(key("s").defaultPrevented).toBe(true);
    expect(clicks).toBe(1);
    expect(document.activeElement).toBe(el);
    expect(labels()).toHaveLength(0);
    expect(document.getElementById("eitri-keys-hud")!.dataset.mode).toBe("normal");
    expect(run).not.toHaveBeenCalled();
  });

  it.each([false, true])("accepts an uppercase label with shiftKey=%s", async (shiftKey) => {
    const el = button();
    let clicks = 0;
    el.addEventListener("click", () => clicks++);
    await start();
    key("f");
    expect(key("S", { shiftKey }).defaultPrevented).toBe(true);
    expect(clicks).toBe(1);
    expect(document.activeElement).toBe(el);
    expect(labels()).toHaveLength(0);
    expect(document.getElementById("eitri-keys-hud")!.dataset.mode).toBe("normal");
  });

  it.each([false, true])("narrows labels before multi-character activation with uppercase=%s", async (uppercase) => {
    const targets = Array.from({ length: 15 }, () => button());
    let clicks = 0;
    for (const target of targets) target.addEventListener("click", () => clicks++);
    await start();
    key("f");
    const multi = labels().find((label) => label.textContent!.length > 1)!.textContent!;
    key(uppercase ? multi[0].toUpperCase() : multi[0]);
    const visible = labels().filter((label) => !label.hidden);
    expect(visible.length).toBeGreaterThan(0);
    expect(visible.length).toBeLessThan(15);
    expect(visible.every((label) => label.textContent!.startsWith(multi[0]))).toBe(true);
    expect(clicks).toBe(0);
    key(uppercase ? multi[1].toUpperCase() : multi[1]);
    expect(clicks).toBe(1);
    expect(labels()).toHaveLength(0);
  });

  it.each(["x", "Escape", "j"])("exits on %s without clicking or dispatching a normal action", async (value) => {
    const el = button();
    let clicks = 0;
    el.addEventListener("click", () => clicks++);
    const scroll = vi.spyOn(window, "scrollBy").mockImplementation(() => {});
    const run = await start();
    key("f");
    expect(key(value).defaultPrevented).toBe(true);
    expect(labels()).toHaveLength(0);
    expect(document.getElementById("eitri-keys-hud")!.dataset.mode).toBe("normal");
    key("s");
    expect(clicks).toBe(0);
    expect(scroll).not.toHaveBeenCalled();
    expect(run).not.toHaveBeenCalled();
  });

  it("marks dangerous labels with the warning class", async () => {
    button().dataset.eitriDanger = "";
    button();
    await start();
    key("f");
    expect(labels()).toHaveLength(2);
    expect(labels()[0].classList.contains("eitri-hint-danger")).toBe(true);
    expect(labels()[1].classList.contains("eitri-hint-danger")).toBe(false);
  });

  it.each(["window scroll", "document scroll", "nested scroll", "resize"])(
    "cancels hints on %s without activating a stale label", async (change) => {
      const container = document.createElement("div");
      container.style.overflow = "auto";
      document.body.append(container);
      const el = button();
      container.append(el);
      box(container, 0, 0, 100, 100);
      let clicks = 0;
      el.addEventListener("click", () => clicks++);
      await start();
      key("f");
      expect(labels()).toHaveLength(1);
      if (change === "window scroll") window.dispatchEvent(new Event("scroll"));
      if (change === "document scroll") document.dispatchEvent(new Event("scroll"));
      if (change === "nested scroll") container.dispatchEvent(new Event("scroll"));
      if (change === "resize") window.dispatchEvent(new Event("resize"));
      expect(labels()).toHaveLength(0);
      expect(document.getElementById("eitri-keys-hud")!.dataset.mode).toBe("normal");
      key("s");
      expect(clicks).toBe(0);
      key("f");
      key("s");
      expect(clicks).toBe(1);
    },
  );

  it("uses the viewport centre by default and setCurrent as the origin", async () => {
    const corner = button(10, 10);
    const centre = button(window.innerWidth / 2 - 15, window.innerHeight / 2 - 10);
    let chosen: Element | undefined;
    corner.addEventListener("click", () => { chosen = corner; });
    centre.addEventListener("click", () => { chosen = centre; });
    await start();
    key("f");
    key("s");
    expect(chosen).toBe(centre);
    setCurrent(corner);
    key("f");
    key("s");
    expect(chosen).toBe(corner);
  });

  it("collects and ranks page targets on each entry", async () => {
    const standard = button();
    const extra = document.createElement("span");
    document.body.append(extra);
    box(extra, 400, 400);
    let clicks = 0;
    extra.addEventListener("click", () => clicks++);
    const targets = vi.fn(() => [{ el: extra, rank: 0 }]);
    await start(targets);
    key("f");
    key("s");
    expect(clicks).toBe(1);
    standard.remove();
    extra.remove();
    key("f");
    expect(targets).toHaveBeenCalledTimes(2);
    expect(labels()).toHaveLength(0);
    expect(document.getElementById("eitri-keys-hud")!.dataset.mode).toBe("normal");
  });

  it.each(["blur", "host", "remove", "input"])("cleans up hint mode on %s", async (reason) => {
    button();
    await start();
    key("f");
    expect(labels()).toHaveLength(1);
    if (reason === "blur") window.dispatchEvent(new Event("blur"));
    if (reason === "host") events.get("focus")!({ has_keys: false });
    if (reason === "remove") cleanup!();
    if (reason === "input") {
      const input = document.createElement("input");
      document.body.append(input);
      input.focus();
      expect(key("s").defaultPrevented).toBe(false);
    }
    expect(labels()).toHaveLength(0);
  });

  it.each([{ ctrlKey: true }, { metaKey: true }, { isComposing: true }, { keyCode: 229 }])(
    "preserves hint mode without activation for skipped events %j", async (init) => {
      const el = button();
      let clicks = 0;
      el.addEventListener("click", () => clicks++);
      await start();
      key("f");
      expect(key("s", init).defaultPrevented).toBe(false);
      expect(labels()).toHaveLength(1);
      expect(clicks).toBe(0);
      key("s");
      expect(clicks).toBe(1);
    },
  );
});
