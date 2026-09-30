// @vitest-environment jsdom
/**
 * K03 (kbux 2026-09-29, S07.4b): one throw while an overlay rendered used to unmount the whole panel --
 * `#root` empty, every key dead, nothing logged. Every overlay `App` draws (the chooser, the
 * `/model`/`/effort` picker, the `?` overlay, the session details popover) now sits inside its own
 * `PanelErrorBoundary` in `App.tsx`, so a throw closes just that overlay through its own leave path,
 * says so in the band, and leaves the panel drawn and the keys working.
 *
 * The overlays are wrapped here so that each can be made to throw on demand (`broken`); they draw the
 * real component otherwise, so nothing else about `App` changes under test.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import type { ComponentProps, ComponentPropsWithoutRef } from "react";
import App from "./App";
import { initialState } from "./reducer";
import type { AgentDomainEvent, AgentUiState, Hello } from "./types";

/** Which overlay's next render throws. Read at render time, so a test flips one on before it opens the overlay. */
const broken = vi.hoisted(() => ({ chooser: false, picker: false, keymap: false, detail: false }));

vi.mock("./components/Chooser", async (importOriginal) => {
  const real = await importOriginal<typeof import("./components/Chooser")>();
  const { createElement } = await import("react");
  return {
    ...real,
    Chooser: (props: ComponentProps<typeof real.Chooser>) => {
      if (broken.chooser) throw new Error("chooser exploded");
      return createElement(real.Chooser, props);
    },
  };
});
vi.mock("./components/SlashPicker", async (importOriginal) => {
  const real = await importOriginal<typeof import("./components/SlashPicker")>();
  const { createElement } = await import("react");
  return {
    ...real,
    SlashPicker: (props: ComponentProps<typeof real.SlashPicker>) => {
      if (broken.picker) throw new Error("picker exploded");
      return createElement(real.SlashPicker, props);
    },
  };
});
vi.mock("./components/KeymapOverlay", async (importOriginal) => {
  const real = await importOriginal<typeof import("./components/KeymapOverlay")>();
  const { createElement, forwardRef } = await import("react");
  const KeymapOverlay = forwardRef<HTMLDivElement, ComponentPropsWithoutRef<typeof real.KeymapOverlay>>((props, ref) => {
    if (broken.keymap) throw new Error("keymap exploded");
    return createElement(real.KeymapOverlay, { ...props, ref });
  });
  return { ...real, KeymapOverlay };
});
vi.mock("./components/DetailPopover", async (importOriginal) => {
  const real = await importOriginal<typeof import("./components/DetailPopover")>();
  const { createElement, forwardRef } = await import("react");
  const DetailPopover = forwardRef<HTMLDivElement, ComponentPropsWithoutRef<typeof real.DetailPopover>>((props, ref) => {
    if (broken.detail) throw new Error("detail exploded");
    return createElement(real.DetailPopover, { ...props, ref });
  });
  return { ...real, DetailPopover };
});

/** React reports a caught render error on `console.error` and, through jsdom, as an uncaught error on
 *  `window`, which vitest would count as an unhandled error. Both are expected here. */
const claimWindowError = (event: ErrorEvent) => event.preventDefault();
beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});
beforeEach(() => {
  vi.spyOn(console, "error").mockImplementation(() => {});
  window.addEventListener("error", claimWindowError);
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { neovibeAgent: { postMessage: () => {} } },
  };
});
afterEach(() => {
  window.removeEventListener("error", claimWindowError);
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  cleanup();
  broken.chooser = broken.picker = broken.keymap = broken.detail = false;
});

function dispatch(payload: unknown) {
  act(() => {
    window.__neovibeDispatch!(JSON.stringify(payload));
  });
}

const HELLO: Hello = {
  backend: "legacy",
  projectDir: "/home/user/project",
  permissionModes: ["auto", "bypass"],
  resumableSessions: [],
  expectedVerdandiRevision: null,
  account: null,
};
const LIVE_TAB = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
} as const;
function snapshotState(overrides: Partial<AgentUiState> = {}): AgentUiState {
  return { ...initialState(), status: { kind: "running" }, ...overrides };
}

/** The band shows a message only once it has a width, and jsdom has no layout: report one, the way
 *  `App.test.tsx`'s own `stubBandWidth` does. Call it before the render that mounts the band. */
function stubBandWidth(): (container: HTMLElement) => void {
  type Rec = { callback: ResizeObserverCallback; observed: Element[] };
  const observers: Rec[] = [];
  class FakeResizeObserver {
    private record: Rec;
    constructor(callback: ResizeObserverCallback) {
      this.record = { callback, observed: [] };
      observers.push(this.record);
    }
    observe(el: Element) {
      this.record.observed.push(el);
    }
    unobserve() {}
    disconnect() {}
  }
  vi.stubGlobal("ResizeObserver", FakeResizeObserver);
  return (container: HTMLElement) => {
    const band = container.querySelector(".status-band");
    const measure = container.querySelector(".band-measure");
    for (const o of observers) {
      for (const el of o.observed) {
        if (el === band) o.callback([{ target: el, contentRect: { width: 900 } } as unknown as ResizeObserverEntry], {} as ResizeObserver);
        if (el === measure) o.callback([{ target: el, contentRect: { width: 7.2 } } as unknown as ResizeObserverEntry], {} as ResizeObserver);
      }
    }
  };
}

/** Makes `App` render again without touching any overlay: the wrappers above consult `broken` when `App`
 *  hands them fresh props, which is how an overlay that drew fine comes to throw on a later render. */
function renderAgain() {
  dispatch({ kind: "queue", tab: 1, items: [], error: null });
}

type Layout = "conversation" | "empty";
/** Renders `App` in the given layout with the band measured, so a flash is visible. */
function start(layout: Layout) {
  const widen = stubBandWidth();
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  if (layout === "conversation") {
    dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB] });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: snapshotState() });
  } else {
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "not_started" }] });
  }
  act(() => widen(rendered.container));
  return rendered;
}

const CHOOSER = {
  kind: "chooser",
  open: [{ tab: 1, label: "1 new", marker: null, pending: 0, resumable: true }],
  records: [],
};
const DETAIL = { kind: "tab_detail", tab: 1, rows: [{ label: "account", value: "work" }] };
// Verbatim probe reply, CLI 2.1.283 under (`App.test.tsx`'s slash-picker describe).
const MODEL_REPLY =
  "Current model: `Haiku 4.5` (effort: high)\n" +
  "Usage: /model <name>. Available: sonnet, opus, haiku, fable, best, sonnet[1m], opus[1m], " +
  "fable[1m], opusplan, default, or a full model ID.";

/** A bare `/model` typed into the composer and answered, which is what opens the picker. `round`
 *  numbers the turn, so a second call carries the revisions a second turn would. */
function openModelPicker(container: HTMLElement, round = 1) {
  if (container.querySelector("textarea") === null) {
    fireEvent.keyDown(container.querySelector(".agent-ui-conversation")!, { key: "i" });
  }
  fireEvent.change(container.querySelector("textarea")!, { target: { value: "/model" } });
  fireEvent.keyDown(container.querySelector("textarea")!, { key: "Enter" });
  const events: AgentDomainEvent[] = [
    { type: "turn_started", turn_id: `t${round}` },
    { type: "turn_completed", turn_id: `t${round}`, outcome: "completed", result_text: MODEL_REPLY, stop_reason: null, usage: null },
  ];
  dispatch({ kind: "events", tab: 1, fromRevision: (round - 1) * 2, throughRevision: round * 2, events });
}

type Site = {
  name: string;
  layout: Layout;
  flag: keyof typeof broken;
  overlay: string;
  flash: string;
  /** Opens it; `round` is 1 the first time and 2 the second (the picker needs a second turn's revisions). */
  open: (container: HTMLElement, round: number) => void;
};
const SITES: Site[] = [
  { name: "the chooser over a conversation", layout: "conversation", flag: "chooser", overlay: ".chooser", flash: "the chooser failed and closed", open: () => dispatch(CHOOSER) },
  { name: "the chooser over an empty tab", layout: "empty", flag: "chooser", overlay: ".chooser", flash: "the chooser failed and closed", open: () => dispatch(CHOOSER) },
  { name: "the /model picker", layout: "conversation", flag: "picker", overlay: ".slash-picker", flash: "the picker failed and closed", open: (c, round) => openModelPicker(c, round) },
  { name: "the ? overlay over a conversation", layout: "conversation", flag: "keymap", overlay: ".keymap-overlay", flash: "the keys overlay failed and closed", open: () => dispatch({ kind: "open_keymap" }) },
  { name: "the ? overlay over an empty tab", layout: "empty", flag: "keymap", overlay: ".keymap-overlay", flash: "the keys overlay failed and closed", open: () => dispatch({ kind: "open_keymap" }) },
  { name: "the session details popover", layout: "conversation", flag: "detail", overlay: ".detail-popover", flash: "the session details failed and closed", open: () => dispatch(DETAIL) },
];

describe("an overlay that throws closes itself and leaves the panel drawn (K03)", () => {
  const panel = (layout: Layout) => (layout === "conversation" ? ".agent-ui-conversation" : ".empty-tab");

  it.each(SITES)("$name: a throw while it opens closes it, says so in the band, and the panel stays", (site) => {
    const { container } = start(site.layout);
    broken[site.flag] = true;
    site.open(container, 1);
    expect(container.querySelector(site.overlay), "the overlay is gone").toBeNull();
    expect(container.querySelector(panel(site.layout)), "the panel is still drawn").not.toBeNull();
    expect(container.querySelector(".status-band")).not.toBeNull();
    expect(container.querySelector(".band-message")?.textContent).toBe(site.flash);
  });

  it.each(SITES)("$name: it opens again, drawn fresh, once what threw is gone", (site) => {
    const { container } = start(site.layout);
    broken[site.flag] = true;
    site.open(container, 1);
    expect(container.querySelector(site.overlay)).toBeNull();
    broken[site.flag] = false;
    site.open(container, 2);
    expect(container.querySelector(site.overlay)).not.toBeNull();
  });

  it("names the failure on the console, once, so a throw is no longer silent", () => {
    const { container } = start("conversation");
    broken.chooser = true;
    dispatch(CHOOSER);
    const logged = (console.error as ReturnType<typeof vi.fn>).mock.calls.filter((call) => call[0] === "[agent-ui] chooser failed:");
    expect(logged).toHaveLength(1);
    expect(logged[0][1]).toEqual(expect.objectContaining({ message: "chooser exploded" }));
    expect(container.querySelector(".chooser")).toBeNull();
  });

  /** K03's own shape: the overlay drew fine, held the keys, and threw on a later render (a filter that
   *  matched nothing). Focus was inside it when it went, so it must be handed back or the keys die. */
  describe("an overlay that held the keys and threw on a later render hands them back", () => {
    it("the chooser", () => {
      const { container } = start("conversation");
      dispatch(CHOOSER);
      const chooser = container.querySelector<HTMLElement>(".chooser")!;
      expect(chooser.contains(document.activeElement), "the chooser holds the keys").toBe(true);
      broken.chooser = true;
      renderAgain();
      expect(container.querySelector(".chooser")).toBeNull();
      expect(container.querySelector(".band-message")?.textContent).toBe("the chooser failed and closed");
      const root = container.querySelector<HTMLElement>(".agent-ui-conversation")!;
      expect(document.activeElement, "the keys are on the panel again").toBe(root);
      fireEvent.keyDown(root, { key: "?", shiftKey: true });
      expect(container.querySelector(".keymap-overlay"), "and a key does what it does there").not.toBeNull();
    });

    it("the chooser over an empty tab", () => {
      const { container } = start("empty");
      dispatch(CHOOSER);
      expect(container.querySelector(".chooser")).not.toBeNull();
      broken.chooser = true;
      renderAgain();
      expect(container.querySelector(".chooser")).toBeNull();
      expect(container.querySelector(".band-message")?.textContent).toBe("the chooser failed and closed");
      expect(container.querySelector(".empty-tab")).not.toBeNull();
      expect(container.contains(document.activeElement), "the keys are on the empty tab again").toBe(true);
      expect(document.activeElement).not.toBe(document.body);
    });

    it("the /model picker", () => {
      const { container } = start("conversation");
      openModelPicker(container);
      const picker = container.querySelector<HTMLElement>(".slash-picker")!;
      expect(picker.contains(document.activeElement), "the picker holds the keys").toBe(true);
      broken.picker = true;
      renderAgain();
      expect(container.querySelector(".slash-picker")).toBeNull();
      expect(container.querySelector(".band-message")?.textContent).toBe("the picker failed and closed");
      expect(container.contains(document.activeElement), "the keys are on the panel again").toBe(true);
      expect(document.activeElement).not.toBe(document.body);
    });
  });
});

/** The scenario K03 was found in, end to end through `App`: the real chooser, no forced throw. */
describe("K03 itself: a chooser filter that matches nothing", () => {
  it("leaves the whole panel drawn, shows 'nothing matches', and Esc still leaves the chooser", () => {
    const { container } = start("conversation");
    dispatch(CHOOSER);
    fireEvent.keyDown(container.querySelector(".chooser")!, { key: "/" });
    const filter = container.querySelector<HTMLInputElement>(".chooser-filter")!;
    fireEvent.change(filter, { target: { value: "zzz" } });
    expect(container.querySelector(".chooser-empty")?.textContent).toBe("nothing matches");
    expect(container.querySelector(".agent-ui-conversation"), "the panel under it is still drawn").not.toBeNull();
    expect(container.querySelector(".status-band")).not.toBeNull();
    expect(container.querySelector(".band-message"), "and no failure was reported: nothing threw").toBeNull();
    fireEvent.keyDown(filter, { key: "Escape" });
    fireEvent.keyDown(container.querySelector(".chooser")!, { key: "Escape" });
    expect(container.querySelector(".chooser")).toBeNull();
    expect(container.querySelector(".agent-ui-conversation")).not.toBeNull();
  });

  it("does the same over an empty tab", () => {
    const { container } = start("empty");
    dispatch(CHOOSER);
    fireEvent.keyDown(container.querySelector(".chooser")!, { key: "/" });
    fireEvent.change(container.querySelector<HTMLInputElement>(".chooser-filter")!, { target: { value: "zzz" } });
    expect(container.querySelector(".chooser-empty")?.textContent).toBe("nothing matches");
    expect(container.querySelector(".empty-tab")).not.toBeNull();
  });
});
