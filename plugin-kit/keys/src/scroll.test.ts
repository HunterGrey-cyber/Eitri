// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cancelGlide } from "./glide";
import { DEFAULT_SETTINGS, useSettings } from "./settings";
import { runScroll } from "./scroll";

// The glide's motion is tested in glide.test.ts; here the system asks for reduced motion, so every scroll action is
// one instant write and the routing shows plainly.
let y = 1000;
beforeEach(() => {
  y = 1000;
  Object.defineProperty(window, "scrollY", { configurable: true, get: () => y });
  Object.defineProperty(window, "matchMedia", { configurable: true, value: () => ({ matches: true }) });
  vi.spyOn(window, "scrollTo").mockImplementation(((opts: ScrollToOptions) => { y = opts.top!; }) as typeof window.scrollTo);
});
afterEach(() => {
  cancelGlide();
  useSettings(DEFAULT_SETTINGS);
  document.body.replaceChildren();
  delete (window as unknown as Record<string, unknown>).matchMedia;
  delete (document.documentElement as unknown as Record<string, unknown>).scrollHeight;
  vi.restoreAllMocks();
});

describe("generic scrolling and navigation", () => {
  it("scrolls by the configured step", () => {
    useSettings({ ...DEFAULT_SETTINGS, scrollStep: 25 });
    expect(runScroll("scroll.down")).toBe(true);
    expect(window.scrollTo).toHaveBeenCalledWith({ top: 1025, behavior: "instant" });
  });
  it.each([["scroll.down", 60], ["scroll.up", -60],
    ["scroll.half_down", window.innerHeight / 2], ["scroll.half_up", -window.innerHeight / 2]])(
    "%s scrolls by %s", (action, top) => {
      expect(runScroll(action as string)).toBe(true);
      expect(window.scrollTo).toHaveBeenCalledWith({ top: 1000 + (top as number), behavior: "instant" });
    },
  );
  it("scrolls to the top and document bottom", () => {
    Object.defineProperty(document.documentElement, "scrollHeight", { value: 2000, configurable: true });
    runScroll("scroll.top");
    expect(window.scrollTo).toHaveBeenLastCalledWith({ top: 0, behavior: "instant" });
    runScroll("scroll.bottom");
    expect(window.scrollTo).toHaveBeenLastCalledWith({ top: 2000 - window.innerHeight, behavior: "instant" });
  });
  it("hands the key event to the glide, which tells a held key's repeat from a press", () => {
    Object.defineProperty(window, "matchMedia", { configurable: true, value: () => ({ matches: false }) });
    vi.useFakeTimers({ toFake: ["requestAnimationFrame", "cancelAnimationFrame", "performance"] });
    try {
      expect(runScroll("scroll.bottom", new KeyboardEvent("keydown", { key: "G", repeat: true }))).toBe(true);
      vi.advanceTimersByTime(400);
      expect(window.scrollTo).not.toHaveBeenCalled();
      runScroll("scroll.down", new KeyboardEvent("keydown", { key: "j" }));
      vi.advanceTimersByTime(400);
      expect(y).toBe(1060);
    } finally {
      cancelGlide();
      vi.useRealTimers();
    }
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
