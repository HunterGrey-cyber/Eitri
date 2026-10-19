import { readFileSync } from "node:fs";
import { JSDOM } from "jsdom";
import { afterEach, describe, expect, it, vi } from "vitest";

const bridge = readFileSync(new URL("../../../core/src/plugin/bridge.js", import.meta.url), "utf8");
type Example = { id: string; name: string; value: string; why?: string };
const examples = JSON.parse(
  readFileSync(new URL("../../../core/src/plugin/theme_values.json", import.meta.url), "utf8"),
) as { accepted: Example[]; refused: Example[] };
const ANCHOR = "--nv-test-anchor";
const windows: JSDOM[] = [];

function page(random: number) {
  const dom = new JSDOM("", { runScripts: "outside-only" });
  windows.push(dom);
  const posted: { id: number; verb: string }[] = [];
  const win = dom.window;
  win.Math.random = () => random;
  win.TextEncoder = TextEncoder;
  win.webkit = { messageHandlers: { eitriPlugin: {
    postMessage: (text: string) => posted.push(JSON.parse(text)),
  } } };
  win.eval(bridge);
  return {
    call: (verb: string) => win.eitriPlugin.call(verb, {}),
    reply: (id: number, result: unknown) => win.__eitriPluginDispatch(JSON.stringify({ v: 1, re: id, ok: true, result })),
    event: (event: string, data: unknown) => win.__eitriPluginDispatch(JSON.stringify({ v: 1, event, data })),
    style: () => win.document.documentElement.style as CSSStyleDeclaration,
    win,
    posted,
  };
}

afterEach(() => { windows.splice(0).forEach((dom) => dom.window.close()); });

describe("plugin bridge document replies", () => {
  it.each([0, 12])("ignores another document's answer after %i earlier calls", async (earlier) => {
    const a = page(0.25);
    const b = page(0.75);
    for (let i = 0; i < earlier; i++) {
      for (const doc of [a, b]) {
        const pending = doc.call("state.read");
        doc.reply(doc.posted[i].id, { value: i });
        await pending;
      }
    }
    const oldRead = a.call("state.read");
    const hello = b.call("host.hello");
    let settled = false;
    void hello.then(() => { settled = true; }, () => { settled = true; });
    b.reply(a.posted[earlier].id, { value: "another page's state" });
    await Promise.resolve();
    expect(settled).toBe(false);
    const answer = { host: "eitri", keys_kit: 1 };
    b.reply(b.posted[earlier].id, answer);
    await expect(hello).resolves.toEqual(answer);
    a.reply(a.posted[earlier].id, { value: "own state" });
    await expect(oldRead).resolves.toEqual({ value: "own state" });
  });

  it.each([0, 1 - Number.EPSILON])("keeps successive ids safe integers at random=%s", async (random) => {
    const doc = page(random);
    for (let i = 0; i < 3; i++) {
      const pending = doc.call("state.read");
      const id = doc.posted[i].id;
      expect(Number.isSafeInteger(id)).toBe(true);
      expect(id).toBeGreaterThan(0);
      if (i > 0) expect(id).toBe(doc.posted[i - 1].id + 1);
      doc.reply(id, { value: i });
      await expect(pending).resolves.toEqual({ value: i });
    }
  });
});

describe("plugin bridge theme", () => {
  it("applies a hello reply's theme before the page's call resolves", async () => {
    const doc = page(0.5);
    const hello = doc.call("host.hello");
    let seen = "";
    const done = hello.then(() => { seen = doc.style().getPropertyValue("--nv-bg"); });
    doc.reply(doc.posted[0].id, { host: "eitri", theme: { vars: { "--nv-bg": "#faf4ed", "--nv-fg": "#575279" } } });
    await done;
    expect(seen).toBe("#faf4ed");
    expect(doc.style().getPropertyValue("--nv-fg")).toBe("#575279");
  });

  it("resolves a hello reply that carries no theme and applies nothing", async () => {
    const doc = page(0.5);
    const hello = doc.call("host.hello");
    doc.reply(doc.posted[0].id, { host: "eitri" });
    await expect(hello).resolves.toEqual({ host: "eitri" });
    expect(doc.style().length).toBe(0);
  });

  it("applies nothing from the reply to any other call", async () => {
    const doc = page(0.5);
    const read = doc.call("state.read");
    doc.reply(doc.posted[0].id, { theme: { vars: { "--nv-bg": "#000000" } } });
    await read;
    expect(doc.style().length).toBe(0);
  });

  it("applies a theme event before the page's own listeners run", () => {
    const doc = page(0.5);
    const seen: unknown[] = [];
    doc.win.eitriPlugin.on("theme", (data: unknown) => { seen.push(doc.style().getPropertyValue("--nv-bg"), data); });
    doc.event("theme", { vars: { "--nv-bg": "#000000" } });
    expect(seen).toEqual(["#000000", { vars: { "--nv-bg": "#000000" } }]);
  });

  it("applies a theme event that arrives before any hello, with no listener", () => {
    const doc = page(0.5);
    doc.event("theme", { vars: { "--nv-bg": "#101010" } });
    expect(doc.style().getPropertyValue("--nv-bg")).toBe("#101010");
  });

  it.each(examples.accepted.map((e) => [e.id, e] as const))("applies the accepted example %s", (_id, example) => {
    const doc = page(0.5);
    doc.event("theme", { vars: { [example.name]: example.value } });
    expect(doc.style().getPropertyValue(example.name)).toBe(example.value);
  });

  it.each(examples.refused.map((e) => [e.id, e] as const))(
    "skips the refused example %s and applies the entry beside it", (_id, example) => {
      const doc = page(0.5);
      const warn = vi.spyOn(doc.win.console, "warn").mockImplementation(() => {});
      doc.event("theme", { vars: { [ANCHOR]: "#123456", [example.name]: example.value } });
      expect(doc.style().getPropertyValue(ANCHOR)).toBe("#123456");
      expect(doc.style().length).toBe(1);
      expect(warn).toHaveBeenCalledTimes(1);
    },
  );

  it("counts a family of non-BMP characters the way Rust does", () => {
    const accepted = examples.accepted.find((e) => e.id === "family-100-non-bmp")!;
    const refused = examples.refused.find((e) => e.id === "family-101-non-bmp")!;
    const first = page(0.5);
    first.event("theme", { vars: { [accepted.name]: accepted.value } });
    expect(first.style().getPropertyValue(accepted.name)).toBe(accepted.value);
    const second = page(0.5);
    vi.spyOn(second.win.console, "warn").mockImplementation(() => {});
    second.event("theme", { vars: { [refused.name]: refused.value } });
    expect(second.style().getPropertyValue(refused.name)).toBe("");
  });

  it("sets nothing outside the --nv- names", () => {
    const doc = page(0.5);
    vi.spyOn(doc.win.console, "warn").mockImplementation(() => {});
    doc.event("theme", { vars: { color: "red", background: "#000000", "--other": "#000000", "--nv-bg": "#000000" } });
    expect(doc.style().length).toBe(1);
    expect(doc.style().getPropertyValue("color")).toBe("");
  });

  it("keeps applying when a page listener throws, and still calls the next listener", () => {
    const doc = page(0.5);
    vi.spyOn(doc.win.console, "error").mockImplementation(() => {});
    const later: unknown[] = [];
    doc.win.eitriPlugin.on("theme", () => { throw new Error("page bug"); });
    doc.win.eitriPlugin.on("theme", (data: unknown) => later.push(data));
    doc.event("theme", { vars: { "--nv-bg": "#000000" } });
    doc.event("theme", { vars: { "--nv-bg": "#111111" } });
    expect(doc.style().getPropertyValue("--nv-bg")).toBe("#111111");
    expect(later).toHaveLength(2);
  });

  it("removes a name it set when a later set lacks or refuses it, and never a property the page set", () => {
    const doc = page(0.5);
    vi.spyOn(doc.win.console, "warn").mockImplementation(() => {});
    doc.style().setProperty("--nv-page-own", "4px");
    doc.style().setProperty("color", "red");
    doc.event("theme", { vars: { "--nv-bg": "#000000", "--nv-fg": "#ffffff", "--nv-muted": "#888888" } });
    doc.event("theme", { vars: { "--nv-bg": "#111111", "--nv-muted": "not a colour" } });
    expect(doc.style().getPropertyValue("--nv-bg")).toBe("#111111");
    expect(doc.style().getPropertyValue("--nv-fg")).toBe("");
    expect(doc.style().getPropertyValue("--nv-muted")).toBe("");
    expect(doc.style().getPropertyValue("--nv-page-own")).toBe("4px");
    expect(doc.style().getPropertyValue("color")).toBe("red");
  });

  it.each([[null], [[]], ["x"], [{ vars: null }], [{ vars: "x" }], [{}]])(
    "applies nothing and does not throw for the data %j", (data) => {
      const doc = page(0.5);
      vi.spyOn(doc.win.console, "warn").mockImplementation(() => {});
      expect(() => doc.event("theme", data)).not.toThrow();
      expect(doc.style().length).toBe(0);
    },
  );

  it("applies nothing to a document without a root element, and still reaches listeners", () => {
    const doc = page(0.5);
    const style = doc.style();
    Object.defineProperty(doc.win.document, "documentElement", { value: null, configurable: true });
    const seen: unknown[] = [];
    doc.win.eitriPlugin.on("theme", (data: unknown) => seen.push(data));
    expect(() => doc.event("theme", { vars: { "--nv-bg": "#000000" } })).not.toThrow();
    expect(seen).toHaveLength(1);
    expect(style.length).toBe(0);
  });

  it("creates no style element", () => {
    const doc = page(0.5);
    doc.event("theme", { vars: { "--nv-bg": "#000000" } });
    expect(doc.win.document.querySelectorAll("style")).toHaveLength(0);
  });
});
