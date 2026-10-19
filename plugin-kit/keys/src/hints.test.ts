// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { activate, assignLabels, collectTargets, placeLabel } from "./hints";
import { install, setCurrent } from "./install";
import { GENERIC_ROWS } from "./rows";
import { STYLE_TEXT } from "./style";

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

/** Gives every hint label, and the HUD, a size; jsdom lays nothing out. */
function sizes(label = { width: 20, height: 16 }, hudHeight = 0) {
  const original = Element.prototype.getBoundingClientRect;
  vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function (this: Element) {
    const rect = (left: number, top: number, width: number, height: number) =>
      ({ x: left, y: top, left, top, right: left + width, bottom: top + height, width, height, toJSON: () => ({}) }) as DOMRect;
    if (this.classList.contains("eitri-hint")) return rect(0, 0, label.width, label.height);
    if (this.id === "eitri-keys-hud") return rect(0, window.innerHeight - hudHeight, window.innerWidth, hudHeight);
    return original.call(this);
  });
}

function labelAt(top: number) {
  return labels().find((label) => label.style.top === `${top}px`)!;
}

function intersects(a: { left: number; top: number; right: number; bottom: number },
  b: { left: number; top: number; right: number; bottom: number }) {
  return a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
}

function labelRect(label: HTMLElement, size = { width: 20, height: 16 }) {
  const left = parseFloat(label.style.left);
  const top = parseFloat(label.style.top);
  return { left, top, right: left + size.width, bottom: top + size.height };
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

  it("excludes targets inside an inert subtree, and the page's own targets there", () => {
    document.body.innerHTML = '<section inert><button></button><a href="#x">x</a></section><button></button>';
    document.querySelectorAll("button, a").forEach((el) => box(el));
    const [inside, link, outside] = [...document.querySelectorAll("button, a")];
    expect(collectTargets(document, [{ el: inside }, { el: link }]).map(({ el }) => el)).toEqual([outside]);
  });

  it.each([true, false])("limits targets to a modal dialog when modal=%s", (modal) => {
    const outside = button();
    const dialog = document.createElement("dialog");
    dialog.open = true;
    document.body.append(dialog);
    box(dialog);
    const inside = button();
    dialog.append(inside);
    const extra = document.createElement("span");
    document.body.append(extra);
    box(extra);
    const matches = dialog.matches.bind(dialog);
    vi.spyOn(dialog, "matches").mockImplementation((selector) => selector === ":modal" ? modal : matches(selector));
    expect(collectTargets(document, [{ el: extra }]).map(({ el }) => el))
      .toEqual(modal ? [inside] : [outside, inside, extra]);
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
    expect(labels()[0].style.background).toBe("");
    expect(labels()[0].style.color).toBe("");
    expect(STYLE_TEXT).toContain(".eitri-hint.eitri-hint-danger{border:2px solid var(--nv-error, #9c2525)}");
  });

  it.each(["window scroll", "document scroll", "nested scroll", "unrelated scroll", "resize"])(
    "keeps hints on %s and moves each label with its target", async (change) => {
      const container = document.createElement("div");
      container.style.overflow = "auto";
      const other = document.createElement("div");
      document.body.append(container, other);
      const el = button();
      container.append(el);
      box(container, 0, 0, 200, 200);
      let clicks = 0;
      el.addEventListener("click", () => clicks++);
      await start();
      key("f");
      expect(labels()).toHaveLength(1);
      box(el, 50, 60);
      if (change === "window scroll") window.dispatchEvent(new Event("scroll"));
      if (change === "document scroll") document.dispatchEvent(new Event("scroll"));
      if (change === "nested scroll") container.dispatchEvent(new Event("scroll"));
      if (change === "unrelated scroll") other.dispatchEvent(new Event("scroll"));
      if (change === "resize") window.dispatchEvent(new Event("resize"));
      expect(labels()).toHaveLength(1);
      expect([labels()[0].style.left, labels()[0].style.top]).toEqual(["48px", "60px"]);
      expect(document.getElementById("eitri-keys-hud")!.dataset.mode).toBe("hint");
      key("s");
      expect(clicks).toBe(1);
    },
  );

  it("hides the label of a target scrolled out of view, and ends hint mode when none is left", async () => {
    const first = button(10, 10);
    const second = button(10, 40);
    let clicks = 0;
    first.addEventListener("click", () => clicks++);
    second.addEventListener("click", () => clicks++);
    await start();
    key("f");
    box(first, 10, -100);
    window.dispatchEvent(new Event("scroll"));
    const gone = labels().filter((label) => label.hidden);
    expect(gone).toHaveLength(1);
    expect(labels().filter((label) => !label.hidden)).toHaveLength(1);
    key(gone[0].textContent!);
    expect(clicks).toBe(0);
    expect(labels()).toHaveLength(0);
    key("f");
    expect(labels()).toHaveLength(1);
    box(second, 10, -100);
    window.dispatchEvent(new Event("scroll"));
    expect(labels()).toHaveLength(0);
    expect(document.getElementById("eitri-keys-hud")!.dataset.mode).toBe("normal");
  });

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

describe("label placement", () => {
  const view = { width: 400, height: 300 };
  const size = { width: 20, height: 16 };

  it("puts the label left of its target, centred on its first line", () => {
    expect(placeLabel({ left: 100, top: 50, right: 200, bottom: 70 }, size, view, []))
      .toEqual({ left: 78, top: 52, side: "left" });
    expect(placeLabel({ left: 100, top: 50, right: 200, bottom: 150 }, size, view, []))
      .toEqual({ left: 78, top: 58, side: "left" });
    expect(placeLabel({ left: 100, top: 50, right: 200, bottom: 60 }, size, view, []))
      .toEqual({ left: 78, top: 50, side: "left" });
  });

  it("goes above, then below, then right when the earlier sides do not fit", () => {
    expect(placeLabel({ left: 10, top: 50, right: 110, bottom: 70 }, size, view, []))
      .toEqual({ left: 10, top: 32, side: "above" });
    expect(placeLabel({ left: 10, top: 5, right: 110, bottom: 25 }, size, view, []))
      .toEqual({ left: 10, top: 27, side: "below" });
    const avoid = [{ left: 0, top: 70, right: 400, bottom: 99 }, { left: 0, top: 121, right: 400, bottom: 150 }];
    expect(placeLabel({ left: 10, top: 100, right: 110, bottom: 120 }, size, view, avoid))
      .toEqual({ left: 112, top: 102, side: "right" });
  });

  it("takes a side that touches the viewport's edge but not one that crosses it", () => {
    expect(placeLabel({ left: 22, top: 50, right: 100, bottom: 66 }, size, view, []).side).toBe("left");
    expect(placeLabel({ left: 21, top: 50, right: 100, bottom: 66 }, size, view, []).side).toBe("above");
  });

  it("falls back to the target's top-right corner when nothing outside fits", () => {
    expect(placeLabel({ left: 0, top: 0, right: 400, bottom: 300 }, size, view, []))
      .toEqual({ left: 380, top: 0, side: "inside" });
    expect(placeLabel({ left: 0, top: 0, right: 10, bottom: 300 }, size, { width: 10, height: 300 }, []))
      .toEqual({ left: 0, top: 0, side: "inside" });
  });

  it("keeps the inside fallback within a view shortened by the HUD", () => {
    const short = { width: 1024, height: 748 };
    const above = { left: 100, top: 720, right: 200, bottom: 740 };
    expect(placeLabel({ left: 100, top: 740, right: 200, bottom: 760 }, size, short, [above]))
      .toEqual({ left: 180, top: 732, side: "inside" });
    expect(placeLabel({ left: 0, top: 0, right: 1100, bottom: 748 }, size, short, []))
      .toEqual({ left: 1004, top: 0, side: "inside" });
  });

  it("never lets a label placed outside touch its own target, another target or a placed label", () => {
    const targets = Array.from({ length: 48 }, (_, i) => {
      const left = 4 + (i % 6) * 64;
      const top = 4 + Math.floor(i / 6) * 36;
      return { left, top, right: left + 40 + (i % 3) * 6, bottom: top + 14 + (i % 4) * 4 };
    });
    const placed: { left: number; top: number; right: number; bottom: number }[] = [];
    targets.forEach((target, i) => {
      const others = targets.filter((_, j) => j !== i);
      const at = placeLabel(target, size, view, [...others, ...placed]);
      const box = { left: at.left, top: at.top, right: at.left + size.width, bottom: at.top + size.height };
      if (at.side !== "inside") {
        expect(intersects(box, target)).toBe(false);
        for (const other of [...others, ...placed]) expect(intersects(box, other)).toBe(false);
      }
      placed.push(box);
    });
  });
});

describe("hint labels outside their targets", () => {
  it("places each label beside its target and colours nothing inline", async () => {
    sizes();
    const column = Array.from({ length: 6 }, (_, i) => button(100, 10 + 40 * i));
    await start();
    key("f");
    expect(labels()).toHaveLength(6);
    for (let i = 0; i < column.length; i++) {
      const label = labelAt(12 + 40 * i);
      expect(label.style.left).toBe("78px");
      expect(label.style.background).toBe("");
      expect(label.style.color).toBe("");
      expect(label.style.position).toBe("");
    }
  });

  it("covers no choice's first character in a list whose gaps are smaller than a label", async () => {
    sizes();
    const choices = Array.from({ length: 4 }, (_, i) => {
      const el = document.createElement("div");
      el.setAttribute("role", "option");
      document.body.append(el);
      return box(el, 0, 100 + 28 * i, 300, 24);
    });
    await start();
    key("f");
    expect(labels()).toHaveLength(4);
    const firstChars = choices.map((el) => {
      const r = el.getBoundingClientRect();
      return { left: r.left, top: r.top, right: r.left + 16, bottom: r.top + 16 };
    });
    for (const label of labels()) {
      for (const first of firstChars) expect(intersects(labelRect(label), first)).toBe(false);
    }
  });

  it("keeps labels out of the HUD's row", async () => {
    sizes({ width: 20, height: 16 }, 20);
    const upper = document.createElement("div");
    upper.setAttribute("role", "option");
    const lower = document.createElement("div");
    lower.setAttribute("role", "option");
    document.body.append(upper, lower);
    box(upper, 0, 692, 300, 24);
    box(lower, 0, 720, 300, 24);
    await start();
    key("f");
    const bottom = window.innerHeight - 20;
    for (const label of labels()) expect(labelRect(label).bottom).toBeLessThanOrEqual(bottom);
    expect(labels().some((label) => label.style.left === "302px" && label.style.top === "724px")).toBe(true);
  });

  it("keeps a kept target's label across two sessions while the others stay prefix-free", async () => {
    sizes();
    const column = Array.from({ length: 20 }, (_, i) => button(100, 10 + 30 * i));
    const kept = column[17];
    kept.dataset.eitriHintKeep = "";
    let clicks = 0;
    kept.addEventListener("click", () => clicks++);
    await start();
    key("f");
    const first = labelAt(12 + 30 * 17).textContent!;
    for (const character of first) key(character);
    expect(clicks).toBe(1);
    setCurrent(column[0]);
    expect(assignLabels(collectTargets(document, []), { x: 115, y: 20 }).get(kept)).not.toBe(first);
    key("f");
    expect(labelAt(12 + 30 * 17).textContent).toBe(first);
    const texts = labels().map((label) => label.textContent!);
    expect(new Set(texts).size).toBe(texts.length);
    for (const a of texts) for (const b of texts) if (a !== b) expect(b.startsWith(a)).toBe(false);
  });

  it("forgets the kept label after another target is activated", async () => {
    sizes();
    const column = Array.from({ length: 20 }, (_, i) => button(100, 10 + 30 * i));
    const kept = column[17];
    kept.dataset.eitriHintKeep = "";
    await start();
    key("f");
    for (const character of labelAt(12 + 30 * 17).textContent!) key(character);
    key("f");
    for (const character of labelAt(12).textContent!) key(character);
    setCurrent(column[0]);
    const expected = assignLabels(collectTargets(document, []), { x: 115, y: 20 }).get(kept);
    key("f");
    expect(labelAt(12 + 30 * 17).textContent).toBe(expected);
  });

  it("forgets the kept label when its target is not among the next session's targets", async () => {
    sizes();
    const column = Array.from({ length: 20 }, (_, i) => button(100, 10 + 30 * i));
    const kept = column[17];
    kept.dataset.eitriHintKeep = "";
    await start();
    key("f");
    for (const character of labelAt(12 + 30 * 17).textContent!) key(character);
    kept.remove();
    key("f");
    key("Escape");
    document.body.append(kept);
    setCurrent(column[0]);
    const expected = assignLabels(collectTargets(document, []), { x: 115, y: 20 }).get(kept);
    key("f");
    expect(labelAt(12 + 30 * 17).textContent).toBe(expected);
  });
});
