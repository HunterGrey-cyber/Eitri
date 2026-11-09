// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { HintLayer } from "./HintLayer";
import type { ShownHint } from "./HintLayer";

afterEach(() => { cleanup(); document.body.innerHTML = ""; vi.restoreAllMocks(); });

function rect(top: number, left = 20, width = 60, height = 20): DOMRect {
  return { top, left, width, height, right: left + width, bottom: top + height, x: left, y: top, toJSON() {} } as DOMRect;
}
function setup(rects?: DOMRect[]) {
  const root = document.createElement("div");
  root.innerHTML = '<div class="message-list"><div data-nav-stop="row" data-row-key="m-1">see a/b.rs</div></div>';
  document.body.append(root);
  const list = root.firstElementChild as HTMLElement;
  const el = list.firstElementChild as HTMLElement;
  root.getBoundingClientRect = () => rect(0, 0, 200, 200);
  list.getBoundingClientRect = () => rect(30, 10, 100, 100);
  el.getBoundingClientRect = () => rect(0, 10, 100, 200);
  let boxes = rects;
  const createRange = document.createRange.bind(document);
  vi.spyOn(document, "createRange").mockImplementation(() => {
    const range = createRange();
    if (boxes !== undefined) range.getClientRects = () => boxes as unknown as DOMRectList;
    return range;
  });
  const hints: ShownHint[] = [{ target: { kind: "path", rowKey: "m-1", start: 4, end: 10, text: "a/b.rs" }, label: "as" }];
  const view = render(<HintLayer root={root} hints={hints} typed="" />);
  return { ...view, root, el, hints, boxes: (next: DOMRect[]) => { boxes = next; } };
}

describe("path HintLayer placement", () => {
  it("labels the first range rect inside the clip and clamps its top-left", () => {
    const { container } = setup([rect(0), rect(25, 5), rect(70)]);
    const label = container.querySelector<HTMLElement>(".hint-path")!;
    expect(label.textContent).toBe("as");
    expect(label.style.top).toBe("30px");
    expect(label.style.left).toBe("10px");
  });
  it("hides a range with no shown rect even when its row shows", () => {
    const { container } = setup([rect(-40), rect(150)]);
    expect(container.querySelector(".hint-label")).toBeNull();
  });
  it("hides a range with no rects", () => {
    expect(setup([]).container.querySelector(".hint-label")).toBeNull();
  });
  it("uses the row box when jsdom has no Range layout API", () => {
    const { container } = setup();
    expect(container.querySelector<HTMLElement>(".hint-label")?.style.top).toBe("30px");
  });
  it("remeasures on commits, marks mismatches, and hides changed or missing text", () => {
    const view = setup([rect(50)]);
    view.boxes([rect(90, 100)]);
    view.rerender(<HintLayer root={view.root} hints={view.hints} typed="s" />);
    const label = view.container.querySelector<HTMLElement>(".hint-label")!;
    expect(label.style.top).toBe("90px");
    expect(label.style.left).toBe("94px");
    expect(label.classList.contains("hint-off")).toBe(true);
    view.el.replaceChildren(document.createTextNode("replacement"));
    view.rerender(<HintLayer root={view.root} hints={view.hints} typed="a" />);
    expect(view.container.querySelector(".hint-label")).toBeNull();
    view.el.remove();
    view.rerender(<HintLayer root={view.root} hints={view.hints} typed="" />);
    expect(view.container.querySelector(".hint-label")).toBeNull();
  });
  it("keeps a label after equal text replaces the row, and reports unplaced labels", () => {
    const view = setup([rect(50)]);
    const replacement = view.el.cloneNode(true) as HTMLElement;
    replacement.getBoundingClientRect = view.el.getBoundingClientRect;
    view.el.replaceWith(replacement);
    view.boxes([rect(80)]);
    const onPlaced = vi.fn();
    view.rerender(<HintLayer root={view.root} hints={view.hints} typed="" onPlaced={onPlaced} />);
    expect(view.container.querySelector<HTMLElement>(".hint-path")?.style.top).toBe("80px");
    expect(onPlaced).toHaveBeenLastCalledWith(["as"]);
    replacement.textContent = "gone";
    view.rerender(<HintLayer root={view.root} hints={view.hints} typed="" onPlaced={onPlaced} />);
    expect(view.container.querySelector(".hint-path")).toBeNull();
    expect(onPlaced).toHaveBeenLastCalledWith([]);
  });
});

it("walks a visible row once per placement pass for all its targets, and skips offscreen rows", () => {
  const view = setup([rect(50)]);
  view.el.textContent = "a/b.rs c/d.rs e/f.rs";
  const hints: ShownHint[] = ["a/b.rs", "c/d.rs", "e/f.rs"].map((text, i) => ({
    label: ["a", "s", "d"][i], target: { kind: "path", rowKey: "m-1", start: i * 7, end: i * 7 + 6, text },
  }));
  view.rerender(<HintLayer root={view.root} hints={hints} typed="" />);
  const walker = vi.spyOn(document, "createTreeWalker");
  view.rerender(<HintLayer root={view.root} hints={hints} typed="s" />);
  expect(walker).toHaveBeenCalledTimes(1);
  expect(view.container.querySelectorAll(".hint-path")).toHaveLength(3);
  walker.mockClear();
  view.el.getBoundingClientRect = () => rect(300);
  view.rerender(<HintLayer root={view.root} hints={hints} typed="d" />);
  expect(walker).not.toHaveBeenCalled();
  expect(view.container.querySelectorAll(".hint-path")).toHaveLength(0);
});
