import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { installFakeLayout, type FakeLayout } from "./view/fakelayout";

/* The page booted as the host loads it (`main.ts` with the host's bridge and kit 6 stubbed), for what only its own
   wiring does: the handlers it gives the bridge's events. */
type Handler = (data: unknown) => void;
const handlers = new Map<string, Handler>();
const DOC = { root: "eitri-plugin://markdown/.eitri/doc/4/", base: "eitri-plugin://markdown/.eitri/doc/4/" };
const HELLO = {
  host: "eitri", protocol: 1, version: "0.0.0", capabilities: [], timeoutlen: 1000, prefix: "<C-b>", module_key: null,
  theme: { vars: {} }, settings: {},
};
const VARS = ["--nv-cursor-blinkwait-ms", "--nv-cursor-blinkon-ms", "--nv-cursor-blinkoff-ms"];
let layout: FakeLayout;

beforeEach(() => {
  handlers.clear();
  document.head.innerHTML = "";
  document.body.innerHTML = '<div id="mdp-notice"></div><main id="mdp-root" class="mdp-root"></main>';
  const root = document.getElementById("mdp-root")!;
  layout = installFakeLayout(root, { chars: true });
  root.style.lineHeight = "20px";
  root.style.fontSize = "16px";
  (window as { requestAnimationFrame?: unknown }).requestAnimationFrame = (cb: () => void) => setTimeout(cb, 0);
  (window as unknown as { eitriPlugin: unknown }).eitriPlugin = {
    call: async (verb: string) =>
      verb === "host.hello" ? HELLO : verb === "editor.follow" ? { filetypes: ["markdown"] } : { applied: true },
    on: (event: string, fn: Handler) => { handlers.set(event, fn); return () => handlers.delete(event); },
  };
  (window as unknown as { eitriKeys: unknown }).eitriKeys = {
    version: 6, install: async () => () => {}, setCurrent: () => {}, message: () => {}, status: () => {}, keys: () => [],
    findRefresh: () => {}, findSeed: () => {}, motion: () => null,
  };
  vi.spyOn(console, "info").mockImplementation(() => {});
});
afterEach(() => {
  for (const name of VARS) document.documentElement.style.removeProperty(name);
  layout.restore();
  delete (window as { eitriPlugin?: unknown }).eitriPlugin;
  delete (window as { eitriKeys?: unknown }).eitriKeys;
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("the page's theme event", () => {
  it("starts the block's blink with the times the host has just put on the root", async () => {
    await import("./main");
    await vi.waitFor(() => expect(handlers.has("theme")).toBe(true));
    vi.useFakeTimers();
    handlers.get("focus")!({ has_keys: true });
    handlers.get("editor.view")!({ buf: 1, name: "/p/notes.md", filetype: "markdown", tick: 1, current: true,
      line_count: 3, top: 1, top_fraction: 0, bottom: 3, cursor: { line: 1, col: 0 }, cause: "user", doc: DOC });
    handlers.get("editor.text")!({ buf: 1, tick: 1, name: "/p/notes.md", text: "hello\n\nnext" });
    vi.advanceTimersByTime(100);
    const block = document.querySelector<HTMLElement>(".mdp-cursor-block")!;
    expect(block.hidden).toBe(false);
    const blinkOff = () => block.classList.contains("mdp-blink-hidden");
    vi.advanceTimersByTime(2000);
    expect(blinkOff()).toBe(false);
    // The bridge puts the theme's variables on the root before the page hears of the event.
    ["500ms", "100ms", "50ms"].forEach((v, i) => document.documentElement.style.setProperty(VARS[i], v));
    const timers = vi.spyOn(globalThis, "setTimeout");
    handlers.get("theme")!({ vars: { "--nv-bg": "#000000" } });
    // The handler itself starts the blink, before the render it schedules (which would start it too) has run.
    expect(timers.mock.calls.some(([, ms]) => ms === 500)).toBe(true);
    vi.advanceTimersByTime(499);
    expect(blinkOff()).toBe(false);
    vi.advanceTimersByTime(1);
    expect(blinkOff()).toBe(true);
  });
});
