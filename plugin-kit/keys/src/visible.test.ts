// @vitest-environment jsdom
import { afterEach, expect, it, vi } from "vitest";
import { isOperable, isRendered } from "./visible";

afterEach(() => {
  document.body.replaceChildren();
  vi.restoreAllMocks();
});

/** Boxes as an engine without `checkVisibility` reports them: none under `display: none` or for a `display: contents`
 *  element itself, one otherwise (jsdom lays nothing out). */
function engineBoxes() {
  expect(typeof (Element.prototype as Partial<HTMLElement>).checkVisibility).toBe("undefined");
  vi.spyOn(Element.prototype, "getClientRects").mockImplementation(function (this: Element) {
    for (let el: Element | null = this; el; el = el.parentElement) {
      if (getComputedStyle(el).display === "none") return [] as unknown as DOMRectList;
    }
    const box = { x: 0, y: 0, left: 0, top: 0, right: 10, bottom: 10, width: 10, height: 10 } as DOMRect;
    return (getComputedStyle(this).display === "contents" ? [] : [box]) as unknown as DOMRectList;
  });
}

it("a_display_contents_item_is_operable_when_a_child_is_drawn", () => {
  document.body.innerHTML = '<a href="#1" style="display: contents"><span>1</span></a>';
  const link = document.querySelector("a")!;
  Object.defineProperty(link, "checkVisibility", { value: () => false });
  Object.defineProperty(link.firstElementChild!, "checkVisibility", { value: () => true });
  expect(isOperable(link)).toBe(true);
});

it("a_text_only_display_contents_wrapper_is_rendered_where_its_parent_is_drawn", () => {
  engineBoxes();
  document.body.innerHTML = '<p><span style="display: contents">alpha</span></p>' +
    '<div style="display: none"><span style="display: contents">beta</span></div>';
  const [shown, folded] = document.querySelectorAll("span");
  expect(isRendered(shown), "text inside a drawn paragraph is drawn").toBe(true);
  expect(isRendered(folded), "text inside a display: none group is not drawn").toBe(false);
});

it("a_display_contents_chain_deeper_than_the_search_is_judged_by_its_place", () => {
  engineBoxes();
  const open = '<span style="display: contents">'.repeat(12);
  const close = "</span>".repeat(12);
  document.body.innerHTML = `<p>${open}<b>alpha</b>${close}</p><div style="display: none">${open}gamma${close}</div>`;
  const [shown, folded] = [...document.querySelectorAll("p > span, div > span")];
  expect(isRendered(shown)).toBe(true);
  expect(isRendered(folded)).toBe(false);
});

it("a_display_contents_wrapper_with_only_undrawn_children_and_no_text_is_not_rendered", () => {
  engineBoxes();
  document.body.innerHTML = '<p><a href="#1" style="display: contents">\n  <span style="display: none">1</span>\n</a></p>';
  expect(isRendered(document.querySelector("a")!)).toBe(false);
});

it("a_disabled_attribute_on_a_link_leaves_it_operable_but_a_disabled_button_is_not", () => {
  engineBoxes();
  document.body.innerHTML = '<a href="#1" disabled>1</a><button disabled>2</button>' +
    '<fieldset disabled><button>3</button></fieldset>';
  const [link, button, inFieldset] = [...document.querySelectorAll("a, button")];
  expect(isOperable(link), "a browser does not disable a link").toBe(true);
  expect(isOperable(button)).toBe(false);
  expect(isOperable(inFieldset), "a disabled fieldset disables its controls").toBe(false);
});
