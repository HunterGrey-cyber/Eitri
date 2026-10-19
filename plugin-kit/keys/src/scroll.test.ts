// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { runScroll } from "./scroll";

beforeEach(() => {
  vi.spyOn(window, "scrollBy").mockImplementation(() => {});
  vi.spyOn(window, "scrollTo").mockImplementation(() => {});
});
afterEach(() => { document.body.replaceChildren(); vi.restoreAllMocks(); });

describe("generic scrolling and navigation", () => {
  it.each([["scroll.down", 60], ["scroll.up", -60],
    ["scroll.half_down", window.innerHeight / 2], ["scroll.half_up", -window.innerHeight / 2]])(
    "%s scrolls by %s", (action, top) => {
      expect(runScroll(action as string)).toBe(true);
      expect(window.scrollBy).toHaveBeenCalledWith({ top, behavior: "smooth" });
    },
  );
  it("scrolls to the top and document bottom", () => {
    Object.defineProperty(document.documentElement, "scrollHeight", { value: 2000, configurable: true });
    runScroll("scroll.top");
    expect(window.scrollTo).toHaveBeenLastCalledWith({ top: 0, behavior: "smooth" });
    runScroll("scroll.bottom");
    expect(window.scrollTo).toHaveBeenLastCalledWith({ top: 2000, behavior: "smooth" });
  });
  it.each([["history.back", "back"], ["history.forward", "forward"]] as const)("%s uses browser history", (action, method) => {
    const spy = vi.spyOn(window.history, method).mockImplementation(() => {});
    expect(runScroll(action)).toBe(true);
    expect(spy).toHaveBeenCalledTimes(1);
  });
  it("prefers rel=prev over an earlier textual match", () => {
    document.body.innerHTML = '<a href="#text">Previous</a><a href="#rel" rel="prev">Chapter</a>';
    const links = document.querySelectorAll("a");
    const text = vi.fn();
    const rel = vi.fn();
    links[0].addEventListener("click", (ev) => { ev.preventDefault(); text(); });
    links[1].addEventListener("click", (ev) => { ev.preventDefault(); rel(); });
    expect(runScroll("link.prev")).toBe(true);
    expect(rel).toHaveBeenCalledTimes(1);
    expect(text).not.toHaveBeenCalled();
  });
  it.each(["prev", "previous", "back", "older", "<", "‹", "←", "«", "上一"])(
    "follows the first previous link matching %s", (text) => {
      const first = document.createElement("a");
      first.href = "#first";
      first.textContent = text;
      const second = first.cloneNode(true);
      document.body.append(first, second);
      const clicked = vi.fn();
      first.addEventListener("click", (ev) => { ev.preventDefault(); clicked(); });
      expect(runScroll("link.prev")).toBe(true);
      expect(clicked).toHaveBeenCalledTimes(1);
    },
  );
  it("follows rel=next", () => {
    const link = document.createElement("a");
    link.href = "#next";
    link.rel = "next";
    document.body.append(link);
    const clicked = vi.fn();
    link.addEventListener("click", (ev) => { ev.preventDefault(); clicked(); });
    expect(runScroll("link.next")).toBe(true);
    expect(clicked).toHaveBeenCalledTimes(1);
  });
  it("leaves the default when navigation has no target or the action is unknown", () => {
    expect(runScroll("link.prev")).toBe(false);
    expect(runScroll("item.next")).toBe(false);
  });
});
